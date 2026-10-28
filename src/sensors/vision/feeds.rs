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
        for (_, retry_at) in self.snapshots.values().filter_map(|s| s.failure.as_ref()) {
            let wait = retry_at.saturating_duration_since(now);
            if !wait.is_zero() {
                again = Some(again.map_or(wait, |a| a.min(wait)));
            }
        }
        again
    }

    pub fn latest(&self, id: &FeedId) -> Option<&Frame> {
        self.latest.get(id)
    }

    /// The newest frame if it is at most `max_age` old. Otherwise a snapshot feed starts a fetch,
    /// and the engine is woken when it lands.
    pub fn frame(&mut self, id: &FeedId, now: Instant, max_age: Duration) -> Option<Frame> {
        if let Some(frame) = self.latest.get(id).filter(|f| now.duration_since(f.at) <= max_age) {
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
    let bytes = agent
        .get(url.expose())
        .call()
        .and_then(|mut response| response.body_mut().with_config().limit(MAX_SNAPSHOT_BYTES).read_to_vec())
        .map_err(|e| e.to_string())?;
    pixels::decode(&bytes).map_err(|e| format!("imagem inválida: {e}"))
}
