use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use crate::cloud::{Dvr, DvrCreds, percent_encode};
use crate::frames::{StreamKey, StreamKind};
use crate::model::{CameraId, DvrSerial, Layout, ViewMode};
use crate::procs::{ChildIo, ChildKey, ChildSpec, LocalPort};

const RTSP_PORT: u16 = 554;
const TUNNEL_STALL: Duration = Duration::from_secs(30);
pub const DECODER_STALL: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TunnelBackend {
    Helper { program: PathBuf, libt2u: PathBuf },
    Unavailable(Unavailable),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unavailable {
    /// No libt2u build exists for this OS.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    NoBackend,
    LibMissing(PathBuf),
    MissingEnv(&'static str),
    InstallFailed(String),
}

#[derive(Clone, Debug)]
pub struct Tools {
    pub ffmpeg: Option<PathBuf>,
    pub tunnel: TunnelBackend,
    pub logs: PathBuf,
}

/// Contains the DVR password; `Debug` redacts it.
#[derive(Clone, PartialEq, Eq)]
pub struct RtspUrl(String);

impl fmt::Debug for RtspUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RtspUrl(***)")
    }
}

impl RtspUrl {
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }

    /// The URL as the vision process receives it from the app: only the local tunnel's form.
    pub(crate) fn from_wire(url: String) -> Option<Self> {
        let rest = url.strip_prefix("rtsp://")?;
        let authority = rest.split('/').next()?;
        let port = authority.rsplit('@').next()?.strip_prefix("127.0.0.1:")?;
        port.parse::<u16>().ok().filter(|p| *p != 0)?;
        (rest.len() > authority.len()).then_some(Self(url))
    }
}

pub fn rtsp_url(port: LocalPort, creds: &DvrCreds, cam: &CameraId, kind: StreamKind) -> RtspUrl {
    RtspUrl(format!(
        "rtsp://{}:{}@127.0.0.1:{}/cam/realmonitor?channel={}&subtype={}",
        percent_encode(&creds.user),
        percent_encode(creds.pass.expose()),
        port.get(),
        cam.channel.rtsp_channel(),
        kind.subtype(),
    ))
}

/// How many frames a decoder delivers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rate {
    /// Every frame the source sends: the grid.
    Native,
    /// This many per second: sensors pay the decode, not the scale, the conversion and the pipe.
    Sampled(u32),
}

/// `input` is a tunnel's `RtspUrl` or a camera URL typed in by the user.
pub fn ffmpeg_args(input: &str, kind: StreamKind, rate: Rate) -> Vec<OsString> {
    let [w, h] = kind.size();
    // A plain stretch, not scale+pad: the 704x480 substream is anamorphic 16:9 with no SAR flag, so
    // keeping its pixel aspect would squeeze the picture into 4:3.
    let vf = match rate {
        Rate::Native => format!("scale={w}:{h},format=rgba"),
        Rate::Sampled(fps) => format!("fps={fps},scale={w}:{h},format=rgba"),
    };
    [
        "-nostdin", "-hide_banner", "-nostats", "-loglevel", "info",
        // The tunnel forwards one TCP port, so RTP has to be interleaved on the RTSP connection.
        "-rtsp_transport", "tcp", "-timeout", "5000000",
        "-fflags", "nobuffer", "-flags", "low_delay",
        "-i", input,
        "-an", "-vf", &vf, "-fps_mode", "passthrough",
        "-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1",
    ]
    .into_iter()
    .map(OsString::from)
    .collect()
}

fn tunnel_spec(backend: &TunnelBackend, serial: &DvrSerial, tools: &Tools) -> Option<ChildSpec> {
    let TunnelBackend::Helper { program, libt2u } = backend else { return None };
    Some(ChildSpec {
        program: program.clone(),
        args: vec![
            libt2u.clone().into(),
            serial.as_str().into(),
            RTSP_PORT.to_string().into(),
            tools.logs.clone().into(),
        ],
        io: ChildIo::TunnelLines,
        stall_after: TUNNEL_STALL,
    })
}

/// The full set of children the current state wants. A tunnel for every DVR in the layout; decoders only
/// once that DVR's tunnel port is known. Every tile keeps its Sub stream during solo, so returning to the
/// grid is instant, and the solo camera adds Main.
pub fn desired_children(
    layout: &Layout,
    view: &ViewMode,
    devices: Option<&BTreeMap<DvrSerial, Dvr>>,
    ports: &BTreeMap<DvrSerial, LocalPort>,
    tools: &Tools,
) -> BTreeMap<ChildKey, ChildSpec> {
    let mut out = BTreeMap::new();
    let Some(devices) = devices else { return out };
    let solo = match view {
        ViewMode::Solo(cam) => Some(cam),
        ViewMode::Grid => None,
    };
    for cam in layout.tiles() {
        let Some(dvr) = devices.get(&cam.dvr) else { continue };
        if let Some(spec) = tunnel_spec(&tools.tunnel, &dvr.serial, tools) {
            out.insert(ChildKey::Tunnel(dvr.serial.clone()), spec);
        }
        let (Some(ffmpeg), Some(&port)) = (&tools.ffmpeg, ports.get(&cam.dvr)) else { continue };
        let kinds: &[StreamKind] = if solo == Some(cam) { &[StreamKind::Sub, StreamKind::Main] } else { &[StreamKind::Sub] };
        for &kind in kinds {
            let url = rtsp_url(port, &dvr.creds, cam, kind);
            out.insert(
                ChildKey::Decoder(StreamKey { camera: cam.clone(), kind }),
                ChildSpec {
                    program: ffmpeg.clone(),
                    args: ffmpeg_args(url.expose(), kind, Rate::Native),
                    io: ChildIo::Frames(kind),
                    stall_after: DECODER_STALL,
                },
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::Secret;
    use crate::model::{ChannelIndex, Intent, SavedLayout, reduce};

    fn serial() -> DvrSerial {
        DvrSerial::new("ABC0000000001")
    }

    fn cam(channel: u8) -> CameraId {
        CameraId { dvr: serial(), channel: ChannelIndex::new(channel) }
    }

    fn devices() -> BTreeMap<DvrSerial, Dvr> {
        let dvr = Dvr {
            serial: serial(),
            name: "casa".into(),
            creds: DvrCreds { user: "admin".into(), pass: Secret::new("p@ss:w/rd") },
            channels: (0..8).map(|i| (ChannelIndex::new(i), format!("Canal{}", i + 1))).collect(),
        };
        BTreeMap::from([(serial(), dvr)])
    }

    fn tools() -> Tools {
        Tools {
            ffmpeg: Some("/opt/homebrew/bin/ffmpeg".into()),
            tunnel: TunnelBackend::Helper { program: "/x/t2u_tunnel".into(), libt2u: "/x/libt2u.so".into() },
            logs: "/x/logs".into(),
        }
    }

    fn layout() -> Layout {
        Layout::from_saved(SavedLayout { version: 1, tiles: vec![cam(0), cam(3)] })
    }

    fn decoders(set: &BTreeMap<ChildKey, ChildSpec>) -> Vec<(u16, StreamKind)> {
        set.keys()
            .filter_map(|k| match k {
                ChildKey::Decoder(s) => Some((s.camera.channel.rtsp_channel(), s.kind)),
                ChildKey::Tunnel(_) | ChildKey::UrlDecoder(..) => None,
            })
            .collect()
    }

    #[test]
    fn signed_out_wants_nothing() {
        assert!(desired_children(&layout(), &ViewMode::Grid, None, &BTreeMap::new(), &tools()).is_empty());
    }

    #[test]
    fn tunnel_first_then_one_sub_decoder_per_tile() {
        let devs = devices();
        let before = desired_children(&layout(), &ViewMode::Grid, Some(&devs), &BTreeMap::new(), &tools());
        assert_eq!(before.keys().collect::<Vec<_>>(), [&ChildKey::Tunnel(serial())]);

        let ports = BTreeMap::from([(serial(), LocalPort::new(14239))]);
        let after = desired_children(&layout(), &ViewMode::Grid, Some(&devs), &ports, &tools());
        assert_eq!(decoders(&after), [(1, StreamKind::Sub), (4, StreamKind::Sub)]);
    }

    #[test]
    fn solo_adds_main_and_keeps_every_sub() {
        let devs = devices();
        let ports = BTreeMap::from([(serial(), LocalPort::new(14239))]);
        let mut view = ViewMode::Grid;
        let mut layout = layout();
        reduce(&mut layout, &mut view, Intent::ToggleSolo(cam(3)));
        let set = desired_children(&layout, &view, Some(&devs), &ports, &tools());
        assert_eq!(decoders(&set), [(1, StreamKind::Sub), (4, StreamKind::Sub), (4, StreamKind::Main)]);
    }

    #[test]
    fn new_tunnel_port_changes_decoder_specs() {
        let devs = devices();
        let a = desired_children(&layout(), &ViewMode::Grid, Some(&devs), &BTreeMap::from([(serial(), LocalPort::new(2000))]), &tools());
        let b = desired_children(&layout(), &ViewMode::Grid, Some(&devs), &BTreeMap::from([(serial(), LocalPort::new(2001))]), &tools());
        let key = ChildKey::Decoder(StreamKey { camera: cam(0), kind: StreamKind::Sub });
        assert_ne!(a[&key], b[&key]);
        assert_eq!(a[&ChildKey::Tunnel(serial())], b[&ChildKey::Tunnel(serial())]);
    }

    #[test]
    fn rtsp_url_is_one_based_escaped_and_redacted_in_debug() {
        let devs = devices();
        let url = rtsp_url(LocalPort::new(14239), &devs[&serial()].creds, &cam(0), StreamKind::Sub);
        assert_eq!(url.0, "rtsp://admin:p%40ss%3Aw%2Frd@127.0.0.1:14239/cam/realmonitor?channel=1&subtype=1");
        assert!(!format!("{url:?}").contains("p%40ss"));
    }
}
