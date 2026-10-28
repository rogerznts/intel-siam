use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, OnceLock, mpsc};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use eframe::egui::ColorImage;
use sha2::{Digest, Sha256};

use crate::frames::{FrameSlot, StreamKey, StreamKind, parse_input_geometry, pump_frames};
use crate::model::DvrSerial;
use crate::plan::{Tools, TunnelBackend, Unavailable};
use crate::status::{ExitReason, classify_exit};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub enum ChildKey {
    Tunnel(DvrSerial),
    Decoder(StreamKey),
    Vision,
    /// A decoder for a camera typed in by URL, outside any DVR.
    UrlDecoder(UrlKey, StreamKind),
}

impl ChildKey {
    fn log_name(&self) -> String {
        match self {
            Self::Tunnel(serial) => format!("tunnel-{}.log", serial.as_str()),
            Self::Vision => "vision.log".into(),
            Self::Decoder(k) => format!(
                "decoder-{}-{}-{}.log",
                k.camera.dvr.as_str(),
                k.camera.channel.rtsp_channel(),
                k.kind.name()
            ),
            Self::UrlDecoder(url, kind) => format!("decoder-url-{:016x}-{}.log", url.0, kind.name()),
        }
    }
}

/// Names a URL without carrying it, so a key, a log name or a `Debug` line never holds the password.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct UrlKey(u64);

impl UrlKey {
    pub fn of(url: &str) -> Self {
        let digest = Sha256::digest(url.as_bytes());
        Self(u64::from_le_bytes(digest[..8].try_into().expect("8 bytes")))
    }
}

/// Everything needed to (re)spawn a child. Equality drives restarts: a new tunnel port changes decoder
/// argv, so `reconcile` replaces exactly the affected decoders.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub io: ChildIo,
    pub stall_after: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildIo {
    TunnelLines,
    Frames(StreamKind),
    Duplex,
}

pub struct Pipes {
    pub pid: u32,
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    pub progress: Arc<Progress>,
}

#[derive(Default)]
pub struct Progress(AtomicU64);

impl Progress {
    pub fn bump(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct LocalPort(u16);

impl LocalPort {
    #[cfg(test)]
    pub const fn new(port: u16) -> Self {
        Self(port)
    }

    pub const fn get(self) -> u16 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TunnelPath {
    Relay,
    P2p,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChildPhase {
    /// Spawned, no progress yet.
    Starting { since: Instant },
    /// Tunnel: READY seen. Decoder: at least one frame.
    Up { since: Instant, last_progress: Instant },
    /// `retry_at` is `None` for terminal reasons, which are never retried automatically.
    Down { reason: ExitReason, attempt: u32, retry_at: Option<Instant> },
}

const BACKOFF_CAP: Duration = Duration::from_secs(30);
const HEALTHY_AFTER: Duration = Duration::from_secs(10);
const STDERR_TAIL: u64 = 4096;

fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(1u64 << attempt.saturating_sub(1).min(5)).min(BACKOFF_CAP)
}

enum TunnelLine {
    Ready(LocalPort, TunnelPath),
    Down(i32),
}

fn parse_tunnel_line(line: &str) -> Option<TunnelLine> {
    let mut words = line.split_whitespace();
    match (words.next()?, words.next()?, words.next()) {
        ("READY", port, Some(path)) => Some(TunnelLine::Ready(
            LocalPort(port.parse().ok()?),
            match path {
                "RELAY" => TunnelPath::Relay,
                "P2P" => TunnelPath::P2p,
                _ => return None,
            },
        )),
        ("DOWN", status, None) => Some(TunnelLine::Down(status.parse().ok()?)),
        _ => None,
    }
}

struct Event {
    key: ChildKey,
    pid: u32,
    line: TunnelLine,
}

struct Running {
    spec: ChildSpec,
    child: Child,
    /// Held open for the helper's lifetime; dropping it is how a tunnel is asked to shut down.
    stdin: Option<ChildStdin>,
    pipes: Option<Pipes>,
    progress: Option<Arc<Progress>>,
    phase: ChildPhase,
    attempt: u32,
    slot: Option<Arc<FrameSlot>>,
    seen_frames: u64,
    tunnel: Option<(LocalPort, TunnelPath)>,
    exit_reason: Option<ExitReason>,
    log: PathBuf,
    /// Copies stderr into `log`; joined before the log is read so the tail is complete.
    logger: Option<JoinHandle<()>>,
    /// The decoder's input size, as ffmpeg reports it on stderr.
    geometry: Arc<OnceLock<[u32; 2]>>,
}

/// A child that was asked to stop: stdin closed, killed at `deadline` if still alive.
struct Stopping {
    child: Child,
    deadline: Instant,
}

/// Called whenever supervisor state may have changed off the polling thread, so the owner polls again.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

pub struct Supervisor {
    running: BTreeMap<ChildKey, Running>,
    down: BTreeMap<ChildKey, ChildPhase>,
    stopping: Vec<Stopping>,
    events_tx: mpsc::Sender<Event>,
    events_rx: mpsc::Receiver<Event>,
    logs: PathBuf,
    wake: Waker,
}

const STOP_GRACE: Duration = Duration::from_secs(1);
const BUSY_POLL: Duration = Duration::from_millis(250);

impl Supervisor {
    pub fn new(logs: PathBuf, wake: Waker) -> Self {
        let (events_tx, events_rx) = mpsc::channel();
        Self { running: BTreeMap::new(), down: BTreeMap::new(), stopping: Vec::new(), events_tx, events_rx, logs, wake }
    }

    /// Converges the running set to `desired`: stops keys that are no longer desired or whose spec
    /// changed, and starts desired keys that are absent and outside their backoff. Idempotent.
    pub fn reconcile(&mut self, desired: BTreeMap<ChildKey, ChildSpec>, now: Instant) {
        let stale: Vec<ChildKey> = self
            .running
            .iter()
            .filter(|(key, r)| desired.get(*key) != Some(&r.spec))
            .map(|(key, _)| key.clone())
            .collect();
        for key in stale {
            let r = self.running.remove(&key).expect("stale key is running");
            self.stop(r, now);
        }
        // Terminal failures stay recorded for the whole session, so a 401 is never retried behind the
        // user's back, even if the camera is removed and added again.
        self.down.retain(|key, phase| {
            desired.contains_key(key) || matches!(phase, ChildPhase::Down { retry_at: None, .. })
        });
        for (key, spec) in desired {
            if self.running.contains_key(&key) {
                continue;
            }
            let attempt = match self.down.get(&key) {
                Some(ChildPhase::Down { retry_at: Some(at), attempt, .. }) if *at <= now => *attempt,
                Some(_) => continue,
                None => 0,
            };
            self.down.remove(&key);
            self.spawn(key, spec, attempt, now);
        }
    }

    fn spawn(&mut self, key: ChildKey, spec: ChildSpec, attempt: u32, now: Instant) {
        let log = self.logs.join(key.log_name());
        let geometry = Arc::new(OnceLock::new());
        match self.try_spawn(&key, &spec, &log, Arc::clone(&geometry)) {
            Ok((mut child, logger)) => {
                let stdout = child.stdout.take().expect("stdout is piped");
                let mut stdin = child.stdin.take();
                let mut pipes = None;
                let mut progress = None;
                let slot = match spec.io {
                    ChildIo::Duplex => {
                        let counter = Arc::new(Progress::default());
                        pipes = Some(Pipes {
                            pid: child.id(),
                            stdin: stdin.take().expect("duplex stdin"),
                            stdout,
                            progress: Arc::clone(&counter),
                        });
                        progress = Some(counter);
                        None
                    }
                    ChildIo::Frames(kind) => {
                        let slot = Arc::new(FrameSlot::default());
                        let (s, wake) = (Arc::clone(&slot), Arc::clone(&self.wake));
                        thread::spawn(move || {
                            let _ = pump_frames(stdout, kind, &s, &*wake);
                            wake();
                        });
                        Some(slot)
                    }
                    ChildIo::TunnelLines => {
                        let (tx, wake, key, pid) = (self.events_tx.clone(), Arc::clone(&self.wake), key.clone(), child.id());
                        thread::spawn(move || {
                            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                                if let Some(line) = parse_tunnel_line(&line) {
                                    if tx.send(Event { key: key.clone(), pid, line }).is_err() {
                                        break;
                                    }
                                    wake();
                                }
                            }
                            wake();
                        });
                        None
                    }
                };
                self.running.insert(
                    key,
                    Running {
                        spec,
                        child,
                        stdin,
                        pipes,
                        progress,
                        phase: ChildPhase::Starting { since: now },
                        attempt,
                        slot,
                        seen_frames: 0,
                        tunnel: None,
                        exit_reason: None,
                        log,
                        logger: Some(logger),
                        geometry,
                    },
                );
            }
            Err(e) => {
                let attempt = attempt + 1;
                let retry_at = Some(now + backoff(attempt));
                self.down.insert(key, ChildPhase::Down { reason: ExitReason::SpawnFailed(e.kind()), attempt, retry_at });
            }
        }
    }

    fn try_spawn(
        &self,
        _key: &ChildKey,
        spec: &ChildSpec,
        log: &Path,
        geometry: Arc<OnceLock<[u32; 2]>>,
    ) -> io::Result<(Child, JoinHandle<()>)> {
        fs::create_dir_all(&self.logs)?;
        let mut log_file = fs::File::create(log)?;
        let stdin = if matches!(spec.io, ChildIo::TunnelLines | ChildIo::Duplex) {
            Stdio::piped()
        } else {
            Stdio::null()
        };
        let mut child =
            Command::new(&spec.program).args(&spec.args).stdin(stdin).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
        let stderr = child.stderr.take().expect("stderr is piped");
        // ffmpeg echoes the input URL, DVR password included; it must never reach the log file.
        let logger = thread::spawn(move || {
            for line in BufReader::new(stderr).split(b'\n').map_while(Result::ok) {
                let line = String::from_utf8_lossy(&line);
                // ffmpeg describes the input before the output, so the first size seen is the input's.
                if geometry.get().is_none()
                    && let Some(size) = parse_input_geometry(&line)
                {
                    let _ = geometry.set(size);
                }
                let _ = writeln!(log_file, "{}", redact_userinfo(&line));
            }
        });
        Ok((child, logger))
    }

    fn stop(&mut self, mut r: Running, now: Instant) {
        match r.stdin.take() {
            Some(stdin) => drop(stdin),
            None => {
                let _ = r.child.kill();
            }
        }
        self.stopping.push(Stopping { child: r.child, deadline: now + STOP_GRACE });
    }

    /// Reaps exits, applies tunnel lines and frame progress, and kills children that stalled. Returns how
    /// soon to poll again while a child is starting, stopping or waiting out a backoff.
    pub fn poll(&mut self, now: Instant) -> Option<Duration> {
        while let Ok(ev) = self.events_rx.try_recv() {
            let Some(r) = self.running.get_mut(&ev.key).filter(|r| r.child.id() == ev.pid) else { continue };
            match ev.line {
                TunnelLine::Ready(port, path) => {
                    r.tunnel = Some((port, path));
                    r.phase = ChildPhase::Up { since: now, last_progress: now };
                }
                TunnelLine::Down(status) => {
                    r.tunnel = None;
                    r.exit_reason = Some(ExitReason::TunnelLost(status));
                }
            }
        }

        let mut exited = Vec::new();
        for (key, r) in &mut self.running {
            let progress = r.slot.as_ref().map(|slot| slot.frames())
                .or_else(|| r.progress.as_ref().map(|p| p.0.load(Ordering::Relaxed)));
            if let Some(frames) = progress {
                if frames > r.seen_frames {
                    r.seen_frames = frames;
                    r.phase = match r.phase {
                        ChildPhase::Up { since, .. } => ChildPhase::Up { since, last_progress: now },
                        _ => ChildPhase::Up { since: now, last_progress: now },
                    };
                }
            } else if let ChildPhase::Up { since, .. } = r.phase {
                // A live tunnel has no output; the helper itself reports loss with DOWN.
                r.phase = ChildPhase::Up { since, last_progress: now };
            }
            let quiet_since = match r.phase {
                ChildPhase::Starting { since } => since,
                ChildPhase::Up { last_progress, .. } => last_progress,
                ChildPhase::Down { .. } => unreachable!("running child is never Down"),
            };
            match r.child.try_wait() {
                Ok(Some(status)) => exited.push((key.clone(), Some(status))),
                Ok(None) if now.duration_since(quiet_since) > r.spec.stall_after => {
                    let _ = r.child.kill();
                    let _ = r.child.wait();
                    r.exit_reason.get_or_insert(ExitReason::Stalled);
                    exited.push((key.clone(), None));
                }
                Ok(None) => {}
                Err(_) => exited.push((key.clone(), None)),
            }
        }
        for (key, status) in exited {
            let mut r = self.running.remove(&key).expect("exited key is running");
            if let Some(logger) = r.logger.take() {
                let _ = logger.join();
            }
            let reason = r.exit_reason.take().unwrap_or_else(|| classify_exit(status, &read_tail(&r.log)));
            let healthy = matches!(r.phase, ChildPhase::Up { since, .. } if now.duration_since(since) >= HEALTHY_AFTER);
            let attempt = if healthy { 1 } else { r.attempt + 1 };
            let retry_at = (!reason.is_terminal()).then(|| now + backoff(attempt));
            self.down.insert(key, ChildPhase::Down { reason, attempt, retry_at });
        }

        self.stopping.retain_mut(|s| match s.child.try_wait() {
            Ok(None) if now < s.deadline => true,
            Ok(None) => {
                let _ = s.child.kill();
                let _ = s.child.wait();
                false
            }
            _ => false,
        });

        let busy = !self.stopping.is_empty()
            || self.running.values().any(|r| !matches!(r.phase, ChildPhase::Up { .. }))
            || self.down.values().any(|p| matches!(p, ChildPhase::Down { retry_at: Some(_), .. }));
        let stall = self.running.values().filter_map(|r| match r.phase {
            ChildPhase::Up { last_progress, .. } if r.progress.is_some() => {
                Some((last_progress + r.spec.stall_after).saturating_duration_since(now) + Duration::from_millis(1))
            }
            _ => None,
        }).min();
        busy.then_some(BUSY_POLL).into_iter().chain(stall).min()
    }

    pub fn phase(&self, key: &ChildKey) -> Option<&ChildPhase> {
        self.running.get(key).map(|r| &r.phase).or_else(|| self.down.get(key))
    }

    pub fn take_pipes(&mut self, key: &ChildKey) -> Option<Pipes> {
        self.running.get_mut(key)?.pipes.take()
    }

    pub fn fail(&mut self, key: &ChildKey, reason: ExitReason) {
        if let Some(r) = self.running.get_mut(key) {
            r.exit_reason = Some(reason);
            let _ = r.child.kill();
        }
    }

    pub fn tunnel_ports(&self) -> BTreeMap<DvrSerial, LocalPort> {
        self.running
            .iter()
            .filter_map(|(key, r)| match (key, r.tunnel) {
                (ChildKey::Tunnel(serial), Some((port, _))) => Some((serial.clone(), port)),
                _ => None,
            })
            .collect()
    }

    pub fn take_frame(&self, key: &ChildKey) -> Option<Arc<ColorImage>> {
        self.running.get(key)?.slot.as_ref()?.take()
    }

    /// The running decoder's input size, once ffmpeg has reported it.
    pub fn input_geometry(&self, key: &ChildKey) -> Option<[u32; 2]> {
        self.running.get(key)?.geometry.get().copied()
    }

    /// Decoders are killed; tunnels get their stdin closed so the helper can remove its port, and are
    /// killed if still alive after `grace`.
    pub fn shutdown(&mut self, grace: Duration) {
        let now = Instant::now();
        for (_, r) in std::mem::take(&mut self.running) {
            self.stop(r, now);
        }
        let deadline = now + grace;
        for s in &mut self.stopping {
            while Instant::now() < deadline && matches!(s.child.try_wait(), Ok(None)) {
                thread::sleep(Duration::from_millis(20));
            }
            let _ = s.child.kill();
            let _ = s.child.wait();
        }
        self.stopping.clear();
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.shutdown(Duration::ZERO);
    }
}

/// Replaces the userinfo of every `scheme://user:pass@host` in `line` with `***`.
pub(crate) fn redact_userinfo(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let authority_end = tail.find(|c: char| matches!(c, '/' | '?' | '#' | '"') || c.is_whitespace()).unwrap_or(tail.len());
        match tail[..authority_end].rfind('@') {
            Some(at) => {
                out.push_str("***");
                rest = &tail[at..];
            }
            None => rest = tail,
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod redaction_regression_tests {
    #[test]
    fn credentials_with_apostrophes_are_hidden_in_stderr() {
        let line = "[rtsp] Failed opening 'rtsp://admin:pa'ss@camera.local/live': 401";
        assert_eq!(super::redact_userinfo(line), "[rtsp] Failed opening 'rtsp://***@camera.local/live': 401");
    }
}

fn read_tail(path: &Path) -> String {
    let Ok(mut file) = fs::File::open(path) else { return String::new() };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = file.seek(SeekFrom::Start(len.saturating_sub(STDERR_TAIL)));
    let mut buf = Vec::new();
    let _ = file.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

#[cfg(target_os = "macos")]
const HELPER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/t2u_tunnel"));

/// Writes the embedded helper to `<bin>/t2u_tunnel-<hash>`. The name is content-addressed and the file
/// appears only by rename after chmod, so an existing file is always complete and current.
#[cfg(target_os = "macos")]
fn install_helper(bin: &Path) -> io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let path = bin.join(concat!("t2u_tunnel-", env!("T2U_HELPER_HASH")));
    if path.is_file() {
        return Ok(path);
    }
    fs::create_dir_all(bin)?;
    let tmp = bin.join(format!(".t2u_tunnel.{}.tmp", std::process::id()));
    let mut file = fs::File::create(&tmp)?;
    file.write_all(HELPER)?;
    file.set_permissions(fs::Permissions::from_mode(0o755))?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, &path)?;
    Ok(path)
}

#[cfg(target_os = "macos")]
fn discover_tunnel(storage: &Path) -> TunnelBackend {
    let libt2u = std::env::var_os("INTEL_SIAM_LIBT2U")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/references/simplay-1.4.1/libt2u.so")));
    if !libt2u.is_file() {
        return TunnelBackend::Unavailable(Unavailable::LibMissing(libt2u));
    }
    // The helper inherits our environment and reads both values there.
    if let Some(name) = ["INTEL_SIAM_T2U_KEY", "INTEL_SIAM_T2U_PASSWORD"].into_iter().find(|n| std::env::var_os(n).is_none()) {
        return TunnelBackend::Unavailable(Unavailable::MissingEnv(name));
    }
    match install_helper(&storage.join("bin")) {
        Ok(program) => TunnelBackend::Helper { program, libt2u },
        Err(e) => TunnelBackend::Unavailable(Unavailable::InstallFailed(e.to_string())),
    }
}

#[cfg(not(target_os = "macos"))]
fn discover_tunnel(_storage: &Path) -> TunnelBackend {
    TunnelBackend::Unavailable(Unavailable::NoBackend)
}

fn discover_ffmpeg() -> Option<PathBuf> {
    let from_env = std::env::var_os("INTEL_SIAM_FFMPEG").map(PathBuf::from);
    let from_path = std::env::var_os("PATH")
        .and_then(|paths| std::env::split_paths(&paths).map(|dir| dir.join("ffmpeg")).find(|p| p.is_file()));
    [from_env, from_path, Some(PathBuf::from("/opt/homebrew/bin/ffmpeg"))].into_iter().flatten().find(|p| p.is_file())
}

pub fn discover_tools(storage: &Path) -> Tools {
    let vision = std::env::current_exe().ok().and_then(|exe| {
        let dir = exe.parent()?;
        [Some(dir.join("intel-siam-vision")), dir.parent().filter(|_| dir.ends_with("deps")).map(|p| p.join("intel-siam-vision"))]
            .into_iter().flatten().find(|p| p.is_file())
    });
    Tools {
        ffmpeg: discover_ffmpeg(), tunnel: discover_tunnel(storage), logs: storage.join("logs"),
        vision, sensors_data: storage.join("sensors"),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::model::{CameraId, ChannelIndex};

    fn sh(script: &str, io: ChildIo) -> ChildSpec {
        ChildSpec { program: "/bin/sh".into(), args: vec!["-c".into(), script.into()], io, stall_after: Duration::from_secs(60) }
    }

    fn decoder_key() -> ChildKey {
        ChildKey::Decoder(StreamKey {
            camera: CameraId { dvr: DvrSerial::new("T"), channel: ChannelIndex::new(0) },
            kind: StreamKind::Sub,
        })
    }

    fn tunnel_key() -> ChildKey {
        ChildKey::Tunnel(DvrSerial::new("T"))
    }

    fn supervisor() -> (Supervisor, PathBuf) {
        let dir = std::env::temp_dir().join(format!("intel-siam-procs-{}-{:?}", std::process::id(), thread::current().id()));
        let _ = fs::remove_dir_all(&dir);
        (Supervisor::new(dir.clone(), Arc::new(|| {})), dir)
    }

    /// Polls until `done` or a 5 s wall-clock budget, returning the `now` that was last polled.
    fn poll_until(sup: &mut Supervisor, mut done: impl FnMut(&Supervisor) -> bool) -> Instant {
        let start = Instant::now();
        loop {
            let now = Instant::now();
            sup.poll(now);
            if done(sup) {
                return now;
            }
            assert!(start.elapsed() < Duration::from_secs(5), "timed out waiting for supervisor state");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn is_down(sup: &Supervisor, key: &ChildKey) -> bool {
        matches!(sup.phase(key), Some(ChildPhase::Down { .. }))
    }

    #[test]
    fn reconcile_twice_starts_one_child() {
        let (mut sup, dir) = supervisor();
        fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("starts");
        let spec = sh(&format!("echo x >> '{}'; sleep 30", marker.display()), ChildIo::Frames(StreamKind::Sub));
        let desired = BTreeMap::from([(decoder_key(), spec)]);
        let now = Instant::now();
        sup.reconcile(desired.clone(), now);
        sup.reconcile(desired, now);
        thread::sleep(Duration::from_millis(300));
        assert_eq!(fs::read_to_string(&marker).unwrap().lines().count(), 1);
    }

    #[test]
    fn crash_backs_off_then_retries_with_growing_delay() {
        let (mut sup, _dir) = supervisor();
        let desired = BTreeMap::from([(decoder_key(), sh("exit 3", ChildIo::Frames(StreamKind::Sub)))]);
        sup.reconcile(desired.clone(), Instant::now());
        let t1 = poll_until(&mut sup, |s| is_down(s, &decoder_key()));
        let Some(ChildPhase::Down { attempt: 1, retry_at: Some(at), .. }) = sup.phase(&decoder_key()).cloned() else {
            panic!("first failure is attempt 1 with a retry: {:?}", sup.phase(&decoder_key()));
        };
        assert_eq!(at - t1, Duration::from_secs(1));

        sup.reconcile(desired.clone(), t1);
        assert!(is_down(&sup, &decoder_key()), "no respawn inside the backoff window");

        sup.reconcile(desired, at);
        assert!(matches!(sup.phase(&decoder_key()), Some(ChildPhase::Starting { .. })));
        let t2 = poll_until(&mut sup, |s| is_down(s, &decoder_key()));
        let Some(ChildPhase::Down { attempt: 2, retry_at: Some(at2), .. }) = sup.phase(&decoder_key()).cloned() else {
            panic!("second failure is attempt 2");
        };
        assert_eq!(at2 - t2, Duration::from_secs(2));
    }

    #[test]
    fn rtsp_401_is_never_retried() {
        let (mut sup, _dir) = supervisor();
        let script = "echo 'method DESCRIBE failed: 401 Unauthorized' >&2; exit 1";
        let desired = BTreeMap::from([(decoder_key(), sh(script, ChildIo::Frames(StreamKind::Sub)))]);
        sup.reconcile(desired.clone(), Instant::now());
        poll_until(&mut sup, |s| is_down(s, &decoder_key()));
        assert!(matches!(
            sup.phase(&decoder_key()),
            Some(ChildPhase::Down { reason: ExitReason::AuthRejected, retry_at: None, .. })
        ));
        sup.reconcile(desired.clone(), Instant::now() + Duration::from_secs(3600));
        sup.reconcile(BTreeMap::new(), Instant::now());
        sup.reconcile(desired, Instant::now() + Duration::from_secs(3600));
        assert!(is_down(&sup, &decoder_key()), "removing and re-adding the camera does not retry a 401");
    }

    #[test]
    fn tunnel_ready_line_publishes_port_and_down_line_reports_status() {
        let (mut sup, _dir) = supervisor();
        let script = "echo 'noise'; echo 'READY 14239 RELAY'; read _; echo 'DOWN -4'; exit 1";
        let desired = BTreeMap::from([(tunnel_key(), sh(script, ChildIo::TunnelLines))]);
        sup.reconcile(desired.clone(), Instant::now());
        poll_until(&mut sup, |s| !s.tunnel_ports().is_empty());
        assert_eq!(sup.tunnel_ports()[&DvrSerial::new("T")], LocalPort::new(14239));

        let r = sup.running.get_mut(&tunnel_key()).unwrap();
        writeln!(r.stdin.as_mut().unwrap(), "go").unwrap();
        poll_until(&mut sup, |s| is_down(s, &tunnel_key()));
        assert!(sup.tunnel_ports().is_empty());
        assert!(matches!(
            sup.phase(&tunnel_key()),
            Some(ChildPhase::Down { reason: ExitReason::TunnelLost(-4), retry_at: Some(_), .. })
        ));
    }

    #[test]
    fn closing_stdin_stops_a_tunnel_within_grace() {
        let (mut sup, _dir) = supervisor();
        let desired = BTreeMap::from([(tunnel_key(), sh("echo 'READY 1 P2P'; cat >/dev/null", ChildIo::TunnelLines))]);
        sup.reconcile(desired, Instant::now());
        poll_until(&mut sup, |s| !s.tunnel_ports().is_empty());
        let started = Instant::now();
        sup.shutdown(Duration::from_secs(5));
        assert!(started.elapsed() < Duration::from_secs(2), "helper exited on stdin EOF, not by kill");
    }

    #[test]
    fn a_child_without_progress_is_killed_as_stalled() {
        let (mut sup, _dir) = supervisor();
        let mut spec = sh("sleep 30", ChildIo::Frames(StreamKind::Sub));
        spec.stall_after = Duration::from_millis(1);
        let start = Instant::now();
        sup.reconcile(BTreeMap::from([(decoder_key(), spec)]), start);
        sup.poll(start + Duration::from_secs(1));
        assert!(matches!(sup.phase(&decoder_key()), Some(ChildPhase::Down { reason: ExitReason::Stalled, .. })));
    }

    #[test]
    fn child_stderr_reaches_the_log_without_the_password() {
        let (mut sup, dir) = supervisor();
        let script = "echo \"Input #0, rtsp, from 'rtsp://admin:s3cr%40t@127.0.0.1:5704/cam'\" >&2; exit 1";
        sup.reconcile(BTreeMap::from([(decoder_key(), sh(script, ChildIo::Frames(StreamKind::Sub)))]), Instant::now());
        poll_until(&mut sup, |s| is_down(s, &decoder_key()));
        let log = fs::read_to_string(dir.join(decoder_key().log_name())).unwrap();
        assert_eq!(log, "Input #0, rtsp, from 'rtsp://***@127.0.0.1:5704/cam'\n");
    }

    #[test]
    fn backoff_doubles_up_to_thirty_seconds() {
        let secs: Vec<u64> = (1..=7).map(|a| backoff(a).as_secs()).collect();
        assert_eq!(secs, [1, 2, 4, 8, 16, 30, 30]);
    }
}
