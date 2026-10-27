use std::io;
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use crate::cloud::Account;
use crate::frames::{StreamKey, StreamKind};
use crate::model::CameraId;
use crate::plan::{Tools, TunnelBackend, Unavailable};
use crate::procs::{ChildKey, ChildPhase, Supervisor};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExitReason {
    SpawnFailed(io::ErrorKind),
    /// RTSP 401: the DVR's local account rejected the credential. Terminal, never retried automatically,
    /// so repeated attempts cannot lock the account.
    AuthRejected,
    Unreachable,
    TimedOut,
    Stalled,
    StreamEnded,
    /// The tunnel helper reported a `t2u_port_status` other than 1 (see docs/protocol/libt2u.md codes).
    TunnelLost(i32),
    Crashed(i32),
    Other(String),
}

impl ExitReason {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::AuthRejected)
    }
}

/// Classifies a child exit from its status and the tail of its stderr log.
pub fn classify_exit(status: Option<ExitStatus>, stderr_tail: &str) -> ExitReason {
    let has = |needle: &str| stderr_tail.contains(needle);
    if has("401 Unauthorized") {
        return ExitReason::AuthRejected;
    }
    if has("Connection refused") {
        return ExitReason::Unreachable;
    }
    if has("timed out") {
        return ExitReason::TimedOut;
    }
    #[cfg(unix)]
    if let Some(signal) = status.and_then(|s| std::os::unix::process::ExitStatusExt::signal(&s)) {
        return ExitReason::Crashed(signal);
    }
    if status.is_some_and(|s| s.success()) {
        return ExitReason::StreamEnded;
    }
    let last = stderr_tail.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    ExitReason::Other(last.to_owned())
}

/// What one tile tells the user. Derived every frame, never stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TileStatus {
    SignedOut,
    LoadingAccount,
    AccountFailed,
    CameraMissing,
    TunnelUnavailable(Unavailable),
    FfmpegMissing,
    TunnelStarting,
    /// `retry_in` is `None` when the failure is terminal.
    TunnelDown { reason: ExitReason, retry_in: Option<Duration> },
    StreamStarting,
    StreamDown { reason: ExitReason, retry_in: Option<Duration> },
    Live,
}

pub fn tile_status(cam: &CameraId, kind: StreamKind, account: &Account, tools: &Tools, procs: &Supervisor, now: Instant) -> TileStatus {
    let devices = match account {
        Account::SignedOut => return TileStatus::SignedOut,
        Account::Working(_) => return TileStatus::LoadingAccount,
        Account::Failed(_) => return TileStatus::AccountFailed,
        Account::Ready { devices } => devices,
    };
    let known = devices.get(&cam.dvr).is_some_and(|dvr| dvr.channels.iter().any(|(ch, _)| *ch == cam.channel));
    if !known {
        return TileStatus::CameraMissing;
    }
    if let TunnelBackend::Unavailable(why) = &tools.tunnel {
        return TileStatus::TunnelUnavailable(why.clone());
    }
    if tools.ffmpeg.is_none() {
        return TileStatus::FfmpegMissing;
    }
    let retry_in = |at: &Option<Instant>| at.map(|at| at.saturating_duration_since(now));
    match procs.phase(&ChildKey::Tunnel(cam.dvr.clone())) {
        None | Some(ChildPhase::Starting { .. }) => return TileStatus::TunnelStarting,
        Some(ChildPhase::Down { reason, retry_at, .. }) => {
            return TileStatus::TunnelDown { reason: reason.clone(), retry_in: retry_in(retry_at) };
        }
        Some(ChildPhase::Up { .. }) => {}
    }
    match procs.phase(&ChildKey::Decoder(StreamKey { camera: cam.clone(), kind })) {
        None | Some(ChildPhase::Starting { .. }) => TileStatus::StreamStarting,
        Some(ChildPhase::Down { reason, retry_at, .. }) => {
            TileStatus::StreamDown { reason: reason.clone(), retry_in: retry_in(retry_at) }
        }
        Some(ChildPhase::Up { .. }) => TileStatus::Live,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    fn exited(code: i32) -> Option<ExitStatus> {
        Some(ExitStatus::from_raw(code << 8))
    }

    #[test]
    fn rtsp_401_is_auth_rejected_and_terminal() {
        // Verbatim ffmpeg 9.0.2 stderr against the DVR with a wrong password (URL redacted).
        let tail = "[in#0 @ 0xc3f018000] method OPTIONS failed: 401 (Unauthorized)\n\
                    [in#0 @ 0xc3ec18000] Error opening input: Server returned 401 Unauthorized (authorization failed)\n\
                    Error opening input file rtsp://***@127.0.0.1:35955/cam/realmonitor?channel=1&subtype=1.\n\
                    Error opening input files: Server returned 401 Unauthorized (authorization failed)\n";
        let reason = classify_exit(exited(1), tail);
        assert_eq!(reason, ExitReason::AuthRejected);
        assert!(reason.is_terminal());
    }

    #[test]
    fn refused_and_timeout_are_retryable() {
        let refused = classify_exit(exited(1), "tcp://127.0.0.1:2345: Connection refused\n");
        assert_eq!(refused, ExitReason::Unreachable);
        assert!(!refused.is_terminal());
        assert_eq!(classify_exit(exited(1), "rtsp://x: Connection timed out\n"), ExitReason::TimedOut);
    }

    #[test]
    fn signal_is_crash() {
        assert_eq!(classify_exit(Some(ExitStatus::from_raw(11)), ""), ExitReason::Crashed(11));
    }

    #[test]
    fn unknown_failure_keeps_last_stderr_line() {
        let reason = classify_exit(exited(1), "first\nInvalid data found when processing input\n\n");
        assert_eq!(reason, ExitReason::Other("Invalid data found when processing input".into()));
    }

    mod tiles {
        use super::super::*;
        use crate::cloud::{Dvr, DvrCreds, Secret};
        use crate::model::{ChannelIndex, DvrSerial};
        use eframe::egui;
        use std::collections::BTreeMap;

        fn cam(channel: u8) -> CameraId {
            CameraId { dvr: DvrSerial::new("ABC0000000001"), channel: ChannelIndex::new(channel) }
        }

        fn ready() -> Account {
            let dvr = Dvr {
                serial: DvrSerial::new("ABC0000000001"),
                name: "casa".into(),
                creds: DvrCreds { user: "u".into(), pass: Secret::new("p") },
                channels: vec![(ChannelIndex::new(0), "Canal1".into())],
            };
            Account::Ready { devices: BTreeMap::from([(dvr.serial.clone(), dvr)]) }
        }

        fn tools(tunnel: TunnelBackend) -> Tools {
            Tools { ffmpeg: Some("/bin/ffmpeg".into()), tunnel, logs: "/tmp".into() }
        }

        fn status(account: &Account, tools: &Tools) -> TileStatus {
            let procs = Supervisor::new("/tmp".into(), egui::Context::default());
            tile_status(&cam(0), StreamKind::Sub, account, tools, &procs, Instant::now())
        }

        #[test]
        fn account_state_comes_first() {
            let t = tools(TunnelBackend::Unavailable(Unavailable::NoBackend));
            assert_eq!(status(&Account::SignedOut, &t), TileStatus::SignedOut);
            assert_eq!(status(&Account::Working(crate::cloud::Stage::Refreshing), &t), TileStatus::LoadingAccount);
        }

        #[test]
        fn saved_camera_absent_from_the_account_is_missing() {
            let t = tools(TunnelBackend::Helper { program: "/x".into(), libt2u: "/y".into() });
            let procs = Supervisor::new("/tmp".into(), egui::Context::default());
            let got = tile_status(&cam(5), StreamKind::Sub, &ready(), &t, &procs, Instant::now());
            assert_eq!(got, TileStatus::CameraMissing);
        }

        #[test]
        fn no_tunnel_backend_is_reported_per_tile() {
            let t = tools(TunnelBackend::Unavailable(Unavailable::NoBackend));
            assert_eq!(status(&ready(), &t), TileStatus::TunnelUnavailable(Unavailable::NoBackend));
        }

        #[test]
        fn signed_in_with_tools_waits_for_the_tunnel() {
            let t = tools(TunnelBackend::Helper { program: "/x".into(), libt2u: "/y".into() });
            assert_eq!(status(&ready(), &t), TileStatus::TunnelStarting);
        }
    }
}
