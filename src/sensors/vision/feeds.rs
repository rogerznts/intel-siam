//! Frames for sensors: one feed per (source, stream), shared by every sensor on it. A DVR stream or
//! an RTSP camera is an ffmpeg decoder run by the same `procs::Supervisor` the app uses, sampling
//! `SENSOR_FPS`; a snapshot URL is fetched when a frame is needed. `frame` hides the difference.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use egui::ColorImage;

use super::logic::FeedFact;
use super::pixels;
use crate::frames::{StreamKey, StreamKind};
use crate::plan::{DECODER_STALL, Rate, ffmpeg_args};
use crate::procs::{ChildIo, ChildKey, ChildPhase, ChildSpec, Supervisor, UrlKey, Waker, redact_userinfo};
use crate::sensors::types::{Geometry, SecretUrl};

/// Sensors pay the H.264 decode, not the scale, the RGBA conversion and the pipe at full rate.
pub const SENSOR_FPS: u32 = 2;
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(15); // RUNTIME["http_timeout_s"]
const SNAPSHOT_RETRY: Duration = Duration::from_secs(5);
const MAX_SNAPSHOT_BYTES: u64 = 16 << 20;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FeedId {
    Dvr(StreamKey),
    /// The key is a hash, so neither the id nor the decoder's log name carries the URL.
    Url(UrlKey, StreamKind),
}

impl FeedId {
    fn child(&self) -> ChildKey {
        match self {
            Self::Dvr(key) => ChildKey::Decoder(key.clone()),
            Self::Url(url, kind) => ChildKey::UrlDecoder(*url, *kind),
        }
    }

    fn kind(&self) -> StreamKind {
        match self {
            Self::Dvr(key) => key.kind,
            Self::Url(_, kind) => *kind,
        }
    }
}

pub enum FeedSource<'a> {
    /// A stream ffmpeg decodes: a tunnel's local URL or an `rtsp://` camera.
    Rtsp(&'a str),
    /// An `http(s)` URL that answers one image per request.
    Snapshot(&'a SecretUrl),
}

#[derive(Clone)]
pub struct Frame {
    pub image: Arc<ColorImage>,
    pub at: Instant,
    /// The source's own size, before the decoder's fixed scale; `None` until ffmpeg reports it.
    pub geometry: Option<Geometry>,
    /// Increases with every frame of every feed.
    pub seq: u64,
}

struct Snapshot {
    url: SecretUrl,
    fetching: bool,
    failure: Option<(String, Instant)>,
}

struct Fetched {
    id: FeedId,
    result: Result<ColorImage, String>,
}

pub struct Feeds {
    procs: Supervisor,
    ffmpeg: PathBuf,
    decoders: BTreeMap<FeedId, ChildSpec>,
    snapshots: BTreeMap<FeedId, Snapshot>,
    latest: BTreeMap<FeedId, Frame>,
    fetched_tx: Sender<Fetched>,
    fetched_rx: Receiver<Fetched>,
    wake: Waker,
    seq: u64,
}

impl Feeds {
    pub fn new(ffmpeg: PathBuf, logs: PathBuf, wake: Waker) -> Self {
        let (fetched_tx, fetched_rx) = mpsc::channel();
        Self {
            procs: Supervisor::new(logs, Arc::clone(&wake)),
            ffmpeg,
            decoders: BTreeMap::new(),
            snapshots: BTreeMap::new(),
            latest: BTreeMap::new(),
            fetched_tx,
            fetched_rx,
            wake,
            seq: 0,
        }
    }
    #[cfg(test)]
    pub(super) fn seed_frame(&mut self, id: FeedId, frame: Frame) {
        self.latest.insert(id, frame);
    }

    /// Converges to `want`: decoders through `Supervisor::reconcile`, so a changed URL restarts
    /// exactly its decoder. A snapshot feed fetches once when it appears, so its status is known.
    pub fn reconcile<'a>(&mut self, want: impl IntoIterator<Item = (FeedId, FeedSource<'a>)>, now: Instant) {
        let mut decoders = BTreeMap::new();
        let mut kept = BTreeSet::new();
        for (id, source) in want {
            match source {
                FeedSource::Rtsp(url) => {
                    let kind = id.kind();
                    let spec = ChildSpec {
                        program: self.ffmpeg.clone(),
                        args: ffmpeg_args(url, kind, Rate::Sampled(SENSOR_FPS)),
                        io: ChildIo::Frames(kind),
                        stall_after: DECODER_STALL,
                    };
                    decoders.insert(id.clone(), spec);
                }
                FeedSource::Snapshot(url) => {
                    self.snapshots.entry(id.clone()).or_insert_with(|| Snapshot { url: url.clone(), fetching: false, failure: None });
                }
            }
            kept.insert(id);
        }
        self.snapshots.retain(|id, _| kept.contains(id));
        self.latest.retain(|id, _| kept.contains(id));
        self.decoders = decoders;
        self.procs.reconcile(self.specs(), now);
        let unknown: Vec<FeedId> =
            self.snapshots.iter().filter(|(id, s)| s.failure.is_none() && !self.latest.contains_key(*id)).map(|(id, _)| id.clone()).collect();
        for id in unknown {
            self.fetch(&id, now);
        }
    }

    /// Restarts decoders past their backoff and takes every new frame. Returns how soon to poll
    /// again while something is starting, stopping or waiting to retry.
    pub fn poll(&mut self, now: Instant) -> Option<Duration> {
        let mut again = self.procs.poll(now);
        self.procs.reconcile(self.specs(), now);
        for id in self.decoders.keys() {
            let child = id.child();
            if let Some(image) = self.procs.take_frame(&child) {
                let geometry = self.procs.input_geometry(&child).map(|[width, height]| Geometry { width, height });
                self.seq += 1;
                self.latest.insert(id.clone(), Frame { image, at: now, geometry, seq: self.seq });
            }
        }
        while let Ok(Fetched { id, result }) = self.fetched_rx.try_recv() {
            let Some(snapshot) = self.snapshots.get_mut(&id) else { continue };
            snapshot.fetching = false;
            match result {
                Ok(image) => {
                    snapshot.failure = None;
                    let [width, height] = image.size.map(|n| n as u32);
                    self.seq += 1;
                    let frame = Frame { image: Arc::new(image), at: now, geometry: Some(Geometry { width, height }), seq: self.seq };
                    self.latest.insert(id, frame);
                }
                Err(reason) => snapshot.failure = Some((reason, now + SNAPSHOT_RETRY)),
            }
        }
        let mut due = Vec::new();
        for (id, snapshot) in &self.snapshots {
            if snapshot.fetching {
                continue;
            }
            if let Some((_, retry_at)) = &snapshot.failure {
                if *retry_at <= now {
                    due.push(id.clone());
                } else {
                    let wait = retry_at.duration_since(now);
                    again = Some(again.map_or(wait, |a| a.min(wait)));
                }
            }
        }
        for id in due {
            self.fetch(&id, now);
        }
        again
    }

    pub fn latest(&self, id: &FeedId) -> Option<&Frame> {
        self.latest.get(id)
    }

    /// The newest frame if it is at most `max_age` old. Otherwise a snapshot feed starts a fetch,
    /// and the engine is woken when it lands.
    pub fn frame(&mut self, id: &FeedId, now: Instant, max_age: Duration) -> Option<Frame> {
        if let Some(frame) = self.latest.get(id).filter(|f| now.duration_since(f.at) < max_age) {
            return Some(frame.clone());
        }
        self.fetch(id, now);
        None
    }

    pub fn phase(&self, id: &FeedId, now: Instant) -> FeedFact {
        if let Some(snapshot) = self.snapshots.get(id) {
            return match &snapshot.failure {
                Some((reason, retry_at)) => {
                    FeedFact::Down { reason: reason.clone(), retry_in: Some(retry_at.saturating_duration_since(now)) }
                }
                None if self.latest.contains_key(id) => FeedFact::Up,
                None => FeedFact::Waiting,
            };
        }
        match self.procs.phase(&id.child()) {
            None | Some(ChildPhase::Starting { .. }) => FeedFact::Waiting,
            Some(ChildPhase::Up { .. }) => FeedFact::Up,
            Some(ChildPhase::Down { reason, retry_at, .. }) => {
                FeedFact::Down { reason: reason.label(), retry_in: retry_at.map(|at| at.saturating_duration_since(now)) }
            }
        }
    }

    fn specs(&self) -> BTreeMap<ChildKey, ChildSpec> {
        self.decoders.iter().map(|(id, spec)| (id.child(), spec.clone())).collect()
    }

    fn fetch(&mut self, id: &FeedId, now: Instant) {
        let Some(snapshot) = self.snapshots.get_mut(id) else { return };
        if snapshot.fetching || snapshot.failure.as_ref().is_some_and(|(_, retry_at)| now < *retry_at) {
            return;
        }
        snapshot.fetching = true;
        let (url, id, tx, wake) = (snapshot.url.clone(), id.clone(), self.fetched_tx.clone(), Arc::clone(&self.wake));
        thread::spawn(move || {
            let result = fetch_snapshot(&url).map_err(|e| redact_userinfo(&e));
            if tx.send(Fetched { id, result }).is_ok() {
                wake();
            }
        });
    }
}

fn fetch_snapshot(url: &SecretUrl) -> Result<ColorImage, String> {
    let agent = ureq::Agent::config_builder().timeout_global(Some(SNAPSHOT_TIMEOUT)).build().new_agent();
    let raw = url.expose();
    let authority_start = raw.find("://").expect("SecretUrl has a scheme") + 3;
    let authority_end = raw[authority_start..].find(['/', '?', '#']).map_or(raw.len(), |end| authority_start + end);
    let request = if let Some(at) = raw[authority_start..authority_end].rfind('@') {
        use base64::Engine as _;
        let at = authority_start + at;
        let (user, pass) = raw[authority_start..at].split_once(':').unwrap_or((&raw[authority_start..at], ""));
        let mut credentials: Vec<u8> = percent_encoding::percent_decode_str(user).collect();
        credentials.push(b':');
        credentials.extend(percent_encoding::percent_decode_str(pass));
        let clean = format!("{}{}", &raw[..authority_start], &raw[at + 1..]);
        let auth = base64::engine::general_purpose::STANDARD.encode(credentials);
        agent.get(&clean).header("Authorization", format!("Basic {auth}"))
    } else {
        agent.get(raw)
    };
    let bytes = request
        .call()
        .and_then(|mut response| response.body_mut().with_config().limit(MAX_SNAPSHOT_BYTES).read_to_vec())
        .map_err(|e| e.to_string())?;
    pixels::decode(&bytes).map_err(|e| format!("imagem inválida: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use tiny_http::{Response, Server};

    fn jpeg() -> Vec<u8> {
        pixels::encode_jpeg(&pixels::Rgb::from(&ColorImage::filled([2, 2], egui::Color32::BLACK)), 90).unwrap()
    }

    #[test]
    fn regression_snapshot_auth_decodes_userinfo_before_sending() {
        let server = Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_ip().unwrap();
        let serving = thread::spawn(move || {
            let request = server.recv_timeout(Duration::from_secs(3)).unwrap().expect("snapshot request");
            let auth = request.headers().iter().find(|h| h.field.equiv("Authorization")).unwrap().value.as_str();
            assert_eq!(auth, format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(b"admin:p@ss:%")));
            assert_eq!(request.url(), "/snapshot");
            request.respond(Response::from_data(jpeg())).unwrap();
        });
        let url = SecretUrl::try_from(format!("http://ad%6Din:p%40ss%3A%25@{addr}/snapshot")).unwrap();
        assert_eq!(fetch_snapshot(&url).unwrap().size, [2, 2]);
        serving.join().unwrap();
    }

    #[test]
    fn regression_poll_retries_failed_snapshot_without_frame_demand() {
        let server = Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_ip().unwrap();
        let serving = thread::spawn(move || {
            for response in [Response::from_data(Vec::new()).with_status_code(503), Response::from_data(jpeg())] {
                server.recv_timeout(Duration::from_secs(3)).unwrap().expect("snapshot fetch or retry").respond(response).unwrap();
            }
        });
        let (tx, wake) = mpsc::channel();
        let mut feeds = Feeds::new("/nonexistent/ffmpeg".into(), std::env::temp_dir(), Arc::new(move || { let _ = tx.send(()); }));
        let url = SecretUrl::try_from(format!("http://{addr}/snapshot")).unwrap();
        let id = FeedId::Url(UrlKey::of(url.expose()), StreamKind::Sub);
        let now = Instant::now();
        feeds.reconcile([(id.clone(), FeedSource::Snapshot(&url))], now);
        wake.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(feeds.poll(now), Some(SNAPSHOT_RETRY));
        assert!(matches!(feeds.phase(&id, now), FeedFact::Down { .. }));
        let due = now + SNAPSHOT_RETRY;
        feeds.poll(due);
        feeds.poll(due);
        wake.recv_timeout(Duration::from_secs(3)).expect("poll starts a retry when due");
        feeds.poll(due);
        assert!(matches!(feeds.phase(&id, due), FeedFact::Up));
        assert_eq!(feeds.latest(&id).unwrap().image.size, [2, 2]);
        serving.join().unwrap();
    }
}
