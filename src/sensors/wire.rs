//! The app ↔ vision protocol, over the vision child's stdin and stdout. One codec both ways:
//!
//! ```text
//! u32 LE header_len | header: JSON | u32 LE payload_len | payload
//! ```
//!
//! Only `Report::Image` has a payload (RGBA8, `w * h * 4` bytes). Limits are checked on read; a
//! violation is an error, the link drops and the supervisor restarts the child.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Write};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::types::{FocusView, ImageId, Interest, SensorEntry, SensorId, SystemStatus, UserCommand};
use crate::frames::StreamKey;
use crate::plan::RtspUrl;

pub(super) const PROTOCOL: u32 = 1;
/// No image on the wire is larger than this on its longer side.
pub(super) const MAX_WIRE_EDGE: u32 = 960;
/// The vision process sends `System` at least this often, so a silent child counts as stalled.
pub(super) const HEARTBEAT: Duration = Duration::from_secs(5);
const MAX_HEADER: u32 = 1 << 20;

/// App → vision.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub(super) enum Command {
    Desired(Desired),
    User(UserCommand),
}

/// The app's whole desired state for the vision process.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct Desired {
    /// Local RTSP URL for each wanted DVR stream whose tunnel is up.
    #[serde(with = "secret_urls")]
    pub feeds: BTreeMap<StreamKey, RtspUrl>,
    pub interest: Interest,
}

/// Vision → app. `Hello` comes first; nothing secret is sent down before it.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub(super) enum Report {
    Hello {
        protocol: u32,
    },
    /// Upsert: the whole truth for that sensor.
    Sensor(SensorEntry),
    Removed(SensorId),
    Wants(BTreeSet<StreamKey>),
    /// Precedes the first `Focus` that references `id`.
    Image {
        id: ImageId,
        size: [u32; 2],
    },
    Focus(Option<FocusView>),
    System(SystemStatus),
}

pub(super) trait Message: Serialize + DeserializeOwned {
    /// Payload bytes this header announces, or `None` when the header breaks a wire limit.
    fn payload_len(&self) -> Option<usize>;
}

impl Message for Command {
    fn payload_len(&self) -> Option<usize> {
        Some(0)
    }
}

impl Message for Report {
    fn payload_len(&self) -> Option<usize> {
        match self {
            Self::Image { size: [w, h], .. } => {
                let edges = 1..=MAX_WIRE_EDGE;
                (edges.contains(w) && edges.contains(h)).then(|| *w as usize * *h as usize * 4)
            }
            _ => Some(0),
        }
    }
}

pub(super) fn write_frame<M: Message>(w: &mut impl Write, msg: &M, payload: &[u8]) -> io::Result<()> {
    let header = serde_json::to_vec(msg)?;
    for part in [&header[..], payload] {
        let len =
            u32::try_from(part.len()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
        w.write_all(&len.to_le_bytes())?;
        w.write_all(part)?;
    }
    w.flush()
}

/// The message and its payload. `Ok(None)` on EOF between frames; EOF inside a frame is an error.
pub(super) fn read_frame<M: Message>(r: &mut impl Read) -> io::Result<Option<(M, Vec<u8>)>> {
    let mut len = [0; 4];
    if !read_first(r, &mut len)? {
        return Ok(None);
    }
    let header_len = u32::from_le_bytes(len);
    if header_len > MAX_HEADER {
        return Err(violation("header over the limit"));
    }
    let mut header = vec![0; header_len as usize];
    r.read_exact(&mut header)?;
    let msg: M = serde_json::from_slice(&header).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let expected = msg.payload_len().ok_or_else(|| violation("image over the wire limit"))?;
    r.read_exact(&mut len)?;
    if u32::from_le_bytes(len) as usize != expected {
        return Err(violation("payload length disagrees with the header"));
    }
    let mut payload = vec![0; expected];
    r.read_exact(&mut payload)?;
    Ok(Some((msg, payload)))
}

/// Fills `buf`; `false` when the stream ends before its first byte.
fn read_first(r: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

fn violation(what: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what)
}

/// `RtspUrl` has no serde impl, so logging it as JSON is never one derive away. This is the one
/// place it is serialised, and only onto the child's stdin.
mod secret_urls {
    use std::collections::BTreeMap;

    use serde::de::Error;
    use serde::{Deserialize, Deserializer, Serializer};

    use crate::frames::StreamKey;
    use crate::plan::RtspUrl;

    pub fn serialize<S: Serializer>(feeds: &BTreeMap<StreamKey, RtspUrl>, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(feeds.iter().map(|(key, url)| (key, url.expose())))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<StreamKey, RtspUrl>, D::Error> {
        Vec::<(StreamKey, String)>::deserialize(d)?
            .into_iter()
            .map(|(key, url)| {
                RtspUrl::from_wire(url)
                    .map(|url| (key, url))
                    .ok_or_else(|| D::Error::custom("feed is not a local tunnel URL"))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::cloud::{DvrCreds, Secret};
    use crate::frames::StreamKind;
    use crate::model::{CameraId, ChannelIndex, DvrSerial};
    use crate::plan::rtsp_url;
    use crate::procs::LocalPort;
    use crate::sensors::types::*;

    fn camera() -> CameraId {
        CameraId { dvr: DvrSerial::new("ABC123"), channel: ChannelIndex::new(0) }
    }

    fn states() -> Settings {
        let def =
            |key: u8, name: &str| StateDef { key: key.try_into().unwrap(), name: name.to_owned().try_into().unwrap() };
        let set = vec![def(1, "Aberto"), def(2, "Fechado")].try_into().unwrap();
        Settings::new_states("Portão".to_owned().try_into().unwrap(), set)
    }

    fn entry() -> SensorEntry {
        let key: StateKey = 2.try_into().unwrap();
        SensorEntry {
            id: SensorId(9),
            source: SourceView::Dvr(camera()),
            stream: StreamKind::Sub,
            settings: states(),
            status: SensorStatus::SourceDown { reason: "401".into(), retry_in: Some(Duration::from_millis(1500)) },
            value: Some(SensorValue::State(key)),
            live: KindLive::States(StatesLive {
                probabilities: vec![(1.try_into().unwrap(), 0.125), (key, 0.875)],
                training: false,
                samples: [(key, 14)].into(),
                review_pending: 1,
            }),
            last_check: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000)),
            notice: None,
        }
    }

    fn encode<M: Message>(frames: &[(M, Vec<u8>)]) -> Vec<u8> {
        let mut buf = Vec::new();
        for (msg, payload) in frames {
            write_frame(&mut buf, msg, payload).unwrap();
        }
        buf
    }

    fn decode_all<M: Message>(bytes: &[u8]) -> io::Result<Vec<(M, Vec<u8>)>> {
        let mut r = Cursor::new(bytes);
        std::iter::from_fn(|| read_frame(&mut r).transpose()).collect()
    }

    fn feed() -> Desired {
        let creds = DvrCreds { user: "admin".into(), pass: Secret::new("hunter2") };
        let port = LocalPort::new(40123);
        let key = StreamKey { camera: camera(), kind: StreamKind::Sub };
        let url = rtsp_url(port, &creds, &key.camera, key.kind);
        Desired { feeds: [(key, url)].into(), interest: Interest { sensor: Some(SensorId(9)), ..Interest::default() } }
    }

    #[test]
    fn reports_round_trip_in_order_until_a_clean_eof() {
        let pixels: Vec<u8> = (0..2 * 3 * 4).map(|i| i as u8).collect();
        let sent = vec![
            (Report::Hello { protocol: PROTOCOL }, vec![]),
            (Report::Sensor(entry()), vec![]),
            (Report::Image { id: ImageId(4), size: [2, 3] }, pixels.clone()),
            (Report::Wants([StreamKey { camera: camera(), kind: StreamKind::Main }].into()), vec![]),
        ];
        let got = decode_all::<Report>(&encode(&sent)).unwrap();
        assert_eq!(got, sent);
    }

    #[test]
    fn commands_round_trip_with_the_secret_only_on_the_wire() {
        let desired = feed();
        let create = UserCommand::Create {
            id: SensorId(9),
            source: SourceSpec::Url("rtsp://u:pw@10.0.0.2/x".to_owned().try_into().unwrap()),
            stream: StreamKind::Main,
            settings: states(),
        };
        let sent = vec![(Command::Desired(desired.clone()), vec![]), (Command::User(create), vec![])];
        let bytes = encode(&sent);
        assert!(bytes.windows(7).any(|w| w == b"hunter2"));
        assert!(!format!("{desired:?}").contains("hunter2"));
        let got: Vec<Command> = decode_all(&bytes).unwrap().into_iter().map(|(msg, _)| msg).collect();
        assert_eq!(got, sent.into_iter().map(|(c, _)| c).collect::<Vec<_>>());
    }

    #[test]
    fn eof_inside_a_frame_is_an_error() {
        let bytes = encode(&[(Report::Hello { protocol: PROTOCOL }, vec![])]);
        for cut in [1, 5, bytes.len() - 1] {
            let err = decode_all::<Report>(&bytes[..cut]).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof, "cut at {cut}");
        }
    }

    #[test]
    fn limits_are_enforced_on_read() {
        let mut oversized = (MAX_HEADER + 1).to_le_bytes().to_vec();
        oversized.extend([b' '; 8]);
        let edge = MAX_WIRE_EDGE + 1;
        let too_wide = encode(&[(Report::Image { id: ImageId(1), size: [edge, 1] }, vec![0; edge as usize * 4])]);
        let short_payload = encode(&[(Report::Image { id: ImageId(1), size: [2, 2] }, vec![0; 15])]);
        let unexpected_payload = encode(&[(Report::Hello { protocol: PROTOCOL }, vec![0])]);
        for bytes in [oversized, too_wide, short_payload, unexpected_payload] {
            assert_eq!(decode_all::<Report>(&bytes).unwrap_err().kind(), io::ErrorKind::InvalidData);
        }
        assert!(
            decode_all::<Report>(&encode(&[(
                Report::Image { id: ImageId(1), size: [960, 960] },
                vec![0; 960 * 960 * 4]
            )]))
            .is_ok()
        );
    }

    #[test]
    fn invalid_domain_values_are_rejected_at_the_boundary() {
        let bad_key = br#"{"User":{"Teach":{"id":1,"frame":2,"state":10}}}"#;
        let mut bytes = (bad_key.len() as u32).to_le_bytes().to_vec();
        bytes.extend(bad_key);
        bytes.extend(0u32.to_le_bytes());
        assert_eq!(decode_all::<Command>(&bytes).unwrap_err().kind(), io::ErrorKind::InvalidData);

        let mut remote = serde_json::to_value(Command::Desired(feed())).unwrap();
        remote["Desired"]["feeds"][0][1] = "rtsp://admin:pw@192.168.1.10:554/x".into();
        assert!(serde_json::from_value::<Command>(remote).is_err());
    }
}
