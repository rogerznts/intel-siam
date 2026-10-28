//! The engine: the single owner of every sensor's settings, runtime state and storage. It waits for
//! the next input or deadline, handles it, then runs `tick`. Decisions come from `logic`, pixel work
//! from `pixels`, persistence from `store`, frames from `feeds`, vectors and heads from `infer`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant, SystemTime};

use egui::ColorImage;

use super::Input;
use super::feeds::{FeedId, FeedSource, Feeds};
use super::infer::{BACKBONE, Done, Infer, Job, ModelEvent, Trained};
use super::logic::{
    self, CheckKind, Debouncer, FeedFact, GeometryVerdict, REVIEW, Schedule, Signature, StatusFacts,
};
use super::pixels;
use super::store::{NewHistory, SensorRecord, Store, StoreError, StoredHead, StoredHistory};
use crate::frames::StreamKey;
use crate::plan::RtspUrl;
use crate::procs::UrlKey;
use crate::sensors::types::{
    Detail, FocusView, FrameId, Geometry, HistoryItem, ImageId, Interest, KindLive, KindSettings, ModelState,
    ModelStatus, Notice, NoticeKind, OpenedItem, Preview, SampleItem, SensorEntry, SensorId, SensorStatus, SensorValue,
    SourceSpec, SourceView, StateKey, StatesLive, SystemStatus, Tab, UserCommand,
};
use crate::sensors::wire::{Command, Desired, HEARTBEAT, MAX_WIRE_EDGE, PROTOCOL, Report};

const FRAME_RING: usize = 5; // RUNTIME["frame_cache_size"]
const RETRAIN_DELAY: Duration = Duration::from_secs(1); // RUNTIME["retrain_delay_s"]
const RETENTION_EVERY: Duration = Duration::from_secs(3600);
/// A frame older than this is not checked: the decoder may be stalling.
const FRESH: Duration = Duration::from_secs(5);
const PREVIEW_EVERY: Duration = Duration::from_millis(500);
const MODEL_RETRY: Duration = Duration::from_secs(60);
const HISTORY_PAGE: usize = 50;
const REVIEW_PAGE: usize = 100;
const THUMB_EDGE: usize = 160;
const SAMPLE_QUALITY: u8 = 90;
const HISTORY_QUALITY: u8 = 80;

/// Stored images get ids from their row, tagged by kind in the top bits, so the same image keeps
/// its id across views and is sent once. Previews count up from 1 and never reach the tags.
const SAMPLE_THUMB: u64 = 1 << 62;
const HISTORY_THUMB: u64 = 2 << 62;
const HISTORY_FRAME: u64 = 3 << 62;

/// One frame for the stdout writer, the only writer of the protocol stream.
pub struct Out {
    pub report: Report,
    pub payload: Vec<u8>,
}

pub struct Engine {
    store: Store,
    feeds: Feeds,
    infer: Infer,
    out: Sender<Out>,
    sensors: BTreeMap<SensorId, Sensor>,
    /// From the app's last `Desired`. Memory only, never logged.
    dvr_urls: BTreeMap<StreamKey, RtspUrl>,
    interest: Interest,
    /// The DVR streams sensors need; sent as `Wants` when it changes.
    wants: BTreeSet<StreamKey>,
    sent_wants: Option<BTreeSet<StreamKey>>,
    /// `None` until the first `Focus`; then exactly what the app holds.
    sent_focus: Option<Option<FocusView>>,
    model: ModelState,
    model_retry_at: Option<Instant>,
    notice: Option<Notice>,
    preview: Option<Shown>,
    next_id: u64,
    feeds_dirty: bool,
    focus_dirty: bool,
    system_dirty: bool,
    next_heartbeat: Instant,
    next_retention: Instant,
}

/// The frame last shown in the live preview.
struct Shown {
    sensor: SensorId,
    preview: Preview,
    image: ColorImage,
    seq: u64,
}

/// A sensor's record (written through: store first, then this copy, by the same method) and its
/// runtime state, which lives in memory only: a restart begins with no value and checks at once.
struct Sensor {
    rec: SensorRecord,
    schedule: Schedule,
    debouncer: Debouncer<SensorValue>,
    /// The region's signature at the last full check: what a probe compares with.
    baseline: Option<Signature>,
    /// Preview frames, so `Teach` labels the frame the user saw.
    recent: VecDeque<(FrameId, Arc<ColorImage>)>,
    /// The frame being embedded. One check in flight at most; a due check meanwhile waits.
    in_flight: Option<Arc<ColorImage>>,
    retrain_at: Option<Instant>,
    training: bool,
    head: Option<StoredHead>,
    probabilities: Vec<(StateKey, f32)>,
    /// When the published value changed, within the flip window.
    changes: VecDeque<Instant>,
    last_flag: Option<Instant>,
    /// The source's size when it differs from the recorded one.
    geometry_seen: Option<Geometry>,
    last_check: Option<SystemTime>,
    failure: Option<String>,
    notice: Option<Notice>,
    samples: BTreeMap<StateKey, u32>,
    review_pending: u32,
    sent: Option<SensorEntry>,
}

impl Sensor {
    fn new(rec: SensorRecord) -> Self {
        Self {
            rec,
            schedule: Schedule::default(),
            debouncer: Debouncer::default(),
            baseline: None,
            recent: VecDeque::new(),
            in_flight: None,
            retrain_at: None,
            training: false,
            head: None,
            probabilities: Vec::new(),
            changes: VecDeque::new(),
            last_flag: None,
            geometry_seen: None,
            last_check: None,
            failure: None,
            notice: None,
            samples: BTreeMap::new(),
            review_pending: 0,
            sent: None,
        }
    }

    /// The head, if it was trained for this backbone and the current region.
    fn head(&self) -> Option<&StoredHead> {
        self.head.as_ref().filter(|h| h.backbone == BACKBONE.id && h.region == self.rec.settings.region.hash())
    }

    fn feed(&self) -> FeedId {
        match &self.rec.source {
            SourceSpec::Dvr(camera) => FeedId::Dvr(StreamKey { camera: camera.clone(), kind: self.rec.stream }),
            SourceSpec::Url(url) => FeedId::Url(UrlKey::of(url.expose()), self.rec.stream),
        }
    }

    fn note(&mut self, kind: NoticeKind) {
        self.notice = Some(Notice { at: SystemTime::now(), kind });
    }
}

impl Engine {
    /// Loads every sensor and sends `Hello`; the first `tick` sends the rest of the snapshot.
    pub fn new(store: Store, feeds: Feeds, infer: Infer, out: Sender<Out>) -> Result<Self, StoreError> {
        let mut sensors: BTreeMap<SensorId, Sensor> =
            store.sensors()?.into_iter().map(|rec| (rec.id, Sensor::new(rec))).collect();
        for (id, head) in store.heads()? {
            if let Some(s) = sensors.get_mut(&id) {
                s.head = Some(head);
            }
        }
        for (id, s) in &mut sensors {
            (s.samples, s.review_pending) = store.counts(*id)?;
        }
        let now = Instant::now();
        let engine = Self {
            store,
            feeds,
            infer,
            out,
            sensors,
            dvr_urls: BTreeMap::new(),
            interest: Interest::default(),
            wants: BTreeSet::new(),
            sent_wants: None,
            sent_focus: None,
            model: ModelState::NotNeeded,
            model_retry_at: None,
            notice: None,
            preview: None,
            next_id: 1,
            feeds_dirty: true,
            focus_dirty: true,
            system_dirty: true,
            next_heartbeat: now,
            next_retention: now,
        };
        engine.emit(Report::Hello { protocol: PROTOCOL }, Vec::new());
        Ok(engine)
    }

    /// Until stdin closes.
    pub fn run(mut self, rx: Receiver<Input>) {
        loop {
            let deadline = self.tick(Instant::now());
            let input = match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(input) => input,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return,
            };
            if !self.handle(input) {
                return;
            }
            while let Ok(input) = rx.try_recv() {
                if !self.handle(input) {
                    return;
                }
            }
        }
    }

    /// `false` once the app is gone.
    fn handle(&mut self, input: Input) -> bool {
        let now = Instant::now();
        match input {
            Input::Closed => return false,
            Input::Wake => {}
            Input::Command(Command::Desired(desired)) => self.desire(desired),
            Input::Command(Command::User(cmd)) => self.user(cmd, now),
            Input::Infer(Done::Embedded { sensor, vector }) => self.embedded(sensor, vector, now),
            Input::Infer(Done::Trained { sensor, region, outcome }) => self.trained(sensor, region, outcome, now),
            Input::Model(event) => {
                self.model = match event {
                    ModelEvent::Progress { done, total } => ModelState::Downloading { done, total },
                    ModelEvent::Ready => ModelState::Ready,
                    ModelEvent::Failed(error) => {
                        self.model_retry_at = Some(now + MODEL_RETRY);
                        ModelState::Failed(error)
                    }
                };
                self.system_dirty = true;
            }
        }
        true
    }

    /// Nothing the app leaves out is deleted: only `Delete` removes a sensor.
    fn desire(&mut self, desired: Desired) {
        if desired.feeds != self.dvr_urls {
            self.dvr_urls = desired.feeds;
            self.feeds_dirty = true;
        }
        if desired.interest != self.interest {
            if desired.interest.sensor != self.interest.sensor {
                self.preview = None;
            }
            self.interest = desired.interest;
            self.feeds_dirty = true;
            self.focus_dirty = true;
        }
    }

    fn user(&mut self, cmd: UserCommand, now: Instant) {
        match cmd {
            UserCommand::Create { id, source, stream, settings } => {
                if self.sensors.contains_key(&id) {
                    return;
                }
                let rec = SensorRecord { id, source, stream, settings, geometry: None };
                match self.store.create(&rec) {
                    Ok(()) => {
                        self.sensors.insert(id, Sensor::new(rec));
                        self.feeds_dirty = true;
                    }
                    Err(e) => self.note(NoticeKind::Store(e.to_string())),
                }
            }
            UserCommand::Configure { id, settings } => {
                let Some(s) = self.sensors.get_mut(&id) else { return };
                if std::mem::discriminant(&s.rec.settings.kind) != std::mem::discriminant(&settings.kind) {
                    s.note(NoticeKind::KindMismatch);
                    return;
                }
                let mut rec = s.rec.clone();
                let retrain = rec.settings.region != settings.region || rec.settings.kind != settings.kind;
                if rec.settings.region != settings.region {
                    s.baseline = None;
                }
                rec.settings = settings;
                rec.geometry = s.geometry_seen.or(rec.geometry);
                match self.store.update(&rec) {
                    Ok(()) => {
                        s.rec = rec;
                        s.geometry_seen = None;
                        if retrain {
                            s.retrain_at = Some(now);
                        }
                        self.feeds_dirty = true;
                        self.focus_dirty = true;
                    }
                    Err(e) => s.note(NoticeKind::Store(e.to_string())),
                }
            }
            UserCommand::Delete(id) => {
                if !self.sensors.contains_key(&id) {
                    return;
                }
                match self.store.delete(id) {
                    Ok(()) => {
                        self.sensors.remove(&id);
                        self.emit(Report::Removed(id), Vec::new());
                        self.feeds_dirty = true;
                        self.focus_dirty = true;
                        self.system_dirty = true;
                    }
                    Err(e) => self.note(NoticeKind::Store(e.to_string())),
                }
            }
            UserCommand::CheckNow(id) => {
                if let Some(s) = self.sensors.get_mut(&id) {
                    s.schedule.force = true;
                    self.feeds_dirty = true;
                }
            }
            UserCommand::Teach { id, frame, state } => {
                let Some(s) = self.sensors.get_mut(&id) else { return };
                let KindSettings::States(states) = &s.rec.settings.kind;
                if !states.states.iter().any(|d| d.key == state) {
                    s.note(NoticeKind::UnknownItem);
                    return;
                }
                let Some((_, image)) = s.recent.iter().find(|(f, _)| *f == frame) else {
                    s.note(NoticeKind::FrameExpired);
                    return;
                };
                let added = pixels::encode_jpeg(image, SAMPLE_QUALITY)
                    .map_err(|e| StoreError::from(std::io::Error::other(e)))
                    .and_then(|jpeg| self.store.add_sample(id, &jpeg, state));
                self.learned(id, added.map(|_| true), now);
            }
            UserCommand::Resolve { id, item, state } => {
                let resolved = self.store.resolve(id, item, state);
                self.learned(id, resolved, now);
            }
            UserCommand::Relabel { id, sample, state } => {
                let relabelled = self.store.relabel(id, sample, state);
                self.learned(id, relabelled, now);
            }
            UserCommand::SetRetention(retention) => {
                if let Err(e) = self.store.set_retention(retention) {
                    self.note(NoticeKind::Store(e.to_string()));
                }
                self.next_retention = now;
                self.system_dirty = true;
            }
        }
    }

    /// After the samples or the review queue changed: fresh counts, a retrain within
    /// `RETRAIN_DELAY` (rapid clicks become one), and a new view.
    fn learned(&mut self, id: SensorId, outcome: Result<bool, StoreError>, now: Instant) {
        let counts = self.store.counts(id);
        let Some(s) = self.sensors.get_mut(&id) else { return };
        match outcome {
            Ok(true) => s.retrain_at = Some(now + RETRAIN_DELAY),
            Ok(false) => s.note(NoticeKind::UnknownItem),
            Err(e) => s.note(NoticeKind::Store(e.to_string())),
        }
        match counts {
            Ok(counts) => (s.samples, s.review_pending) = counts,
            Err(e) => s.note(NoticeKind::Store(e.to_string())),
        }
        self.focus_dirty = true;
        self.system_dirty = true;
    }

    fn embedded(&mut self, id: SensorId, vector: Result<Vec<f32>, String>, now: Instant) {
        let Some(s) = self.sensors.get_mut(&id) else { return };
        let Some(frame) = s.in_flight.take() else { return };
        let vector = match vector {
            Ok(vector) => vector,
            Err(error) => {
                s.failure = Some(error);
                return;
            }
        };
        s.failure = None;
        let KindSettings::States(states) = &s.rec.settings.kind;
        let probabilities = s.head().map(|h| h.head.predict(&vector, &states.states)).unwrap_or_default();
        let (value, top) = logic::state_verdict(&probabilities, states.threshold);
        let changed = s.debouncer.update(value.clone(), states.debounce);
        if changed {
            s.changes.push_back(now);
        }
        let confidence = top.map_or(0.0, |(_, p)| p);
        let review = if probabilities.is_empty() {
            None
        } else {
            let recent = logic::recent_changes(&REVIEW, &mut s.changes, now);
            let since = s.last_flag.map(|at| now.duration_since(at));
            let reason = logic::review_reason(&REVIEW, confidence, states.threshold, recent, since, roll());
            if reason.is_some() {
                s.last_flag = Some(now);
            }
            reason
        };
        s.probabilities = probabilities;
        s.last_check = Some(SystemTime::now());
        if !changed && review.is_none() {
            return;
        }
        let at = SystemTime::now();
        let recorded = pixels::encode_jpeg(&frame, HISTORY_QUALITY)
            .map_err(|e| StoreError::from(std::io::Error::other(e)))
            .and_then(|jpeg| {
                let row = NewHistory { at, value: &value, confidence, changed, review, jpeg: &jpeg };
                self.store.record(id, &row)
            });
        match recorded {
            Ok(_) => {
                s.review_pending += u32::from(review.is_some());
                self.focus_dirty |= self.interest.sensor == Some(id);
                self.system_dirty = true;
            }
            Err(e) => s.note(NoticeKind::Store(e.to_string())),
        }
    }

    fn trained(&mut self, id: SensorId, region: u64, outcome: Result<Trained, String>, now: Instant) {
        let Some(s) = self.sensors.get_mut(&id) else { return };
        s.training = false;
        let Trained { head, fresh } = match outcome {
            Ok(trained) => trained,
            Err(error) => {
                s.failure = Some(error);
                return;
            }
        };
        if let Err(e) = self.store.put_training(id, BACKBONE.id, region, head.as_ref(), &fresh) {
            s.note(NoticeKind::Store(e.to_string()));
        }
        if region != s.rec.settings.region.hash() {
            s.retrain_at = Some(now);
            return;
        }
        s.failure = None;
        s.head = head.map(|head| StoredHead { backbone: BACKBONE.id.to_owned(), region, head });
        // The value follows the new head at once (origin: "the training checks again when done").
        s.schedule.force = true;
    }

    /// Everything time-driven, then whatever changed goes out. Returns the next deadline.
    fn tick(&mut self, now: Instant) -> Instant {
        let mut next = now + RETENTION_EVERY;
        let mut soon = |at: Instant| next = next.min(at);
        if self.feeds_dirty {
            self.feeds_dirty = false;
            self.reconcile_feeds(now);
        }
        if let Some(wait) = self.feeds.poll(now) {
            soon(now + wait);
        }
        self.ensure_model(now);
        if let Some(at) = self.model_retry_at {
            soon(at);
        }
        let ids: Vec<SensorId> = self.sensors.keys().copied().collect();
        for id in &ids {
            if let Some(at) = self.step(*id, now) {
                soon(at);
            }
        }
        if let Some(at) = self.step_preview(now) {
            soon(at);
        }
        if now >= self.next_retention {
            self.next_retention = now + RETENTION_EVERY;
            if let Err(e) = self.store.enforce_retention(SystemTime::now()) {
                self.note(NoticeKind::Store(e.to_string()));
            }
            self.focus_dirty = true;
            self.system_dirty = true;
        }
        soon(self.next_retention);
        for id in &ids {
            self.publish(*id, now);
        }
        if self.sent_wants.as_ref() != Some(&self.wants) {
            self.sent_wants = Some(self.wants.clone());
            self.emit(Report::Wants(self.wants.clone()), Vec::new());
        }
        if self.system_dirty || now >= self.next_heartbeat {
            self.system_dirty = false;
            self.next_heartbeat = now + HEARTBEAT;
            let system = self.system();
            self.emit(Report::System(system), Vec::new());
        }
        soon(self.next_heartbeat);
        if self.focus_dirty {
            self.focus_dirty = false;
            self.push_focus();
        }
        next
    }

    /// The model is needed once a sensor has samples to learn or a head to check with: fetched
    /// once, retried after a failure. A sensor nobody taught yet downloads nothing.
    fn ensure_model(&mut self, now: Instant) {
        let due = match self.model {
            ModelState::NotNeeded => self.sensors.values().any(|s| s.retrain_at.is_some() || s.head.is_some()),
            ModelState::Failed(_) => self.model_retry_at.is_some_and(|at| at <= now),
            ModelState::Downloading { .. } | ModelState::Ready => false,
        };
        if due {
            self.model_retry_at = None;
            self.model = ModelState::Downloading { done: 0, total: BACKBONE.size };
            self.system_dirty = true;
            self.infer.ensure();
        }
    }

    /// One sensor's geometry, retrain and check. Returns when it wants to run again.
    fn step(&mut self, id: SensorId, now: Instant) -> Option<Instant> {
        let ready = self.model == ModelState::Ready;
        let s = self.sensors.get_mut(&id)?;
        let feed = s.feed();
        if let Some(seen) = self.feeds.latest(&feed).and_then(|f| f.geometry) {
            match logic::geometry_verdict(s.rec.geometry, seen) {
                GeometryVerdict::Adopt => {
                    s.rec.geometry = Some(seen);
                    if let Err(e) = self.store.update(&s.rec) {
                        s.note(NoticeKind::Store(e.to_string()));
                    }
                }
                GeometryVerdict::Same => s.geometry_seen = None,
                GeometryVerdict::Changed => s.geometry_seen = Some(seen),
            }
        }
        let mut next = None;
        if let Some(at) = s.retrain_at {
            if at > now {
                next = Some(at);
            } else if ready && !s.training {
                s.retrain_at = None;
                let KindSettings::States(states) = &s.rec.settings.kind;
                let labels: BTreeSet<StateKey> = states.states.iter().map(|d| d.key).collect();
                let region = s.rec.settings.region.clone();
                match self.store.training_set(id, &labels, BACKBONE.id, region.hash()) {
                    Ok(rows) => {
                        s.training = true;
                        self.infer.submit(Job::Train { sensor: id, region, rows });
                    }
                    Err(e) => s.note(NoticeKind::Store(e.to_string())),
                }
            }
        }
        if s.in_flight.is_some() || s.geometry_seen.is_some() || !ready || s.head().is_none() {
            return next;
        }
        let settings = &s.rec.settings;
        let (at, kind) = logic::next_check(&s.schedule, settings.triggers, settings.interval, settings.paused, now)?;
        if at > now {
            return Some(next.map_or(at, |n| n.min(at)));
        }
        // No fresh frame: the feed wakes the engine when one arrives.
        let frame = self.feeds.frame(&feed, now, FRESH)?;
        let region = &s.rec.settings.region;
        if kind == CheckKind::Probe {
            s.schedule.last_probe = Some(now);
            let signature = pixels::signature(&frame.image, region);
            match &s.baseline {
                Some(baseline) if !logic::changed(baseline, &signature) => return next,
                Some(_) => logic::start_burst(&mut s.schedule, now),
                None => {}
            }
        }
        s.schedule.last_run = Some(now);
        s.schedule.force = false;
        s.schedule.recheck_at = None;
        s.baseline = Some(pixels::signature(&frame.image, region));
        self.infer.submit(Job::Embed { sensor: id, crop: pixels::crop(&frame.image, region) });
        s.in_flight = Some(frame.image);
        // A paused sensor only had a feed for "check now"; the frame is taken, so the feed can go.
        self.feeds_dirty |= s.rec.settings.paused;
        next
    }

    /// A new preview frame for the focused sensor's live tab, whenever its feed has one.
    fn step_preview(&mut self, now: Instant) -> Option<Instant> {
        let focused = self.interest.sensor.filter(|_| self.interest.tab == Tab::Live);
        let Some(s) = focused.and_then(|id| self.sensors.get_mut(&id)) else {
            if self.preview.take().is_some() {
                self.focus_dirty = true;
            }
            return None;
        };
        let feed = s.feed();
        let frame = self.feeds.frame(&feed, now, PREVIEW_EVERY).or_else(|| self.feeds.latest(&feed).cloned())?;
        let next = frame.at + PREVIEW_EVERY;
        if self.preview.as_ref().is_some_and(|p| p.sensor == s.rec.id && p.seq == frame.seq) {
            return Some(next);
        }
        let image = pixels::fit_rgba(&frame.image, MAX_WIRE_EDGE as usize);
        let [width, height] = frame.image.size.map(|n| n as u32);
        let preview = Preview {
            frame: FrameId(self.next_id),
            image: ImageId(self.next_id),
            geometry: frame.geometry.unwrap_or(Geometry { width, height }),
        };
        self.next_id += 1;
        s.recent.push_back((preview.frame, frame.image));
        if s.recent.len() > FRAME_RING {
            s.recent.pop_front();
        }
        self.preview = Some(Shown { sensor: s.rec.id, preview, image, seq: frame.seq });
        self.focus_dirty = true;
        Some(next)
    }

    /// Feeds for sensors that are active, focused or asked to check now; `Wants` for their DVR
    /// streams, with or without a URL yet.
    fn reconcile_feeds(&mut self, now: Instant) {
        let mut want = Vec::new();
        let mut wants = BTreeSet::new();
        for s in self.sensors.values() {
            let needed = !s.rec.settings.paused || s.schedule.force || self.interest.sensor == Some(s.rec.id);
            if !needed {
                continue;
            }
            let feed = s.feed();
            match (&s.rec.source, &feed) {
                (SourceSpec::Dvr(_), FeedId::Dvr(key)) => {
                    wants.insert(key.clone());
                    if let Some(url) = self.dvr_urls.get(key) {
                        want.push((feed, FeedSource::Rtsp(url.expose())));
                    }
                }
                (SourceSpec::Url(url), _) if url.is_snapshot() => want.push((feed, FeedSource::Snapshot(url))),
                (SourceSpec::Url(url), _) => want.push((feed, FeedSource::Rtsp(url.expose()))),
                (SourceSpec::Dvr(_), FeedId::Url(..)) => unreachable!("a DVR source has a DVR feed"),
            }
        }
        self.feeds.reconcile(want, now);
        self.wants = wants;
    }

    /// Sends the sensor's entry when it differs from the last one sent.
    fn publish(&mut self, id: SensorId, now: Instant) {
        let Some(s) = self.sensors.get(&id) else { return };
        let feed = s.feed();
        let feed = match &feed {
            FeedId::Dvr(key) if !self.dvr_urls.contains_key(key) => FeedFact::Waiting,
            _ => self.feeds.phase(&feed, now),
        };
        let status = logic::sensor_status(StatusFacts {
            paused: s.rec.settings.paused,
            geometry: s.rec.geometry.zip(s.geometry_seen),
            feed,
            model: self.model.clone(),
            failure: s.failure.clone(),
            trained: s.head().is_some(),
        });
        let entry = entry(s, status);
        let s = self.sensors.get_mut(&id).expect("looked up above");
        if s.sent.as_ref() != Some(&entry) {
            s.sent = Some(entry.clone());
            self.emit(Report::Sensor(entry), Vec::new());
        }
    }

    fn system(&mut self) -> SystemStatus {
        let (retention, usage) = match self.store.retention().and_then(|r| Ok((r, self.store.usage()?))) {
            Ok(both) => both,
            Err(e) => {
                self.note(NoticeKind::Store(e.to_string()));
                Default::default()
            }
        };
        SystemStatus {
            retention,
            usage,
            models: vec![ModelStatus { name: BACKBONE.name.to_owned(), state: self.model.clone() }],
            notice: self.notice.clone(),
        }
    }

    /// The answer to the current `Interest`. Images the app does not hold yet go first, then the
    /// view, and only when it changed.
    fn push_focus(&mut self) {
        let mut images: Vec<(ImageId, ColorImage)> = Vec::new();
        let held = self.sent_focus.as_ref().and_then(|f| f.as_ref()).map(FocusView::images).unwrap_or_default();
        let view = self.focus_view(&held, &mut images);
        if self.sent_focus.as_ref() == Some(&view) {
            return;
        }
        for (id, image) in images {
            let size = image.size.map(|n| n as u32);
            self.emit(Report::Image { id, size }, image.as_raw().to_vec());
        }
        self.sent_focus = Some(view.clone());
        self.emit(Report::Focus(view), Vec::new());
    }

    /// Items whose image cannot be read are left out, so every image the view names was sent.
    fn focus_view(&mut self, held: &BTreeSet<ImageId>, images: &mut Vec<(ImageId, ColorImage)>) -> Option<FocusView> {
        let id = self.interest.sensor.filter(|id| self.sensors.contains_key(id))?;
        let preview = match &self.preview {
            Some(shown) if shown.sensor == id && self.interest.tab == Tab::Live => {
                if !held.contains(&shown.preview.image) {
                    images.push((shown.preview.image, shown.image.clone()));
                }
                Some(shown.preview.clone())
            }
            _ => None,
        };
        let mut load = |image: ImageId, path: &Path, edge: usize| -> bool {
            if held.contains(&image) {
                return true;
            }
            match fs::read(path).map_err(|e| e.to_string()).and_then(|b| pixels::decode(&b).map_err(|e| e.to_string())) {
                Ok(frame) => {
                    images.push((image, pixels::fit_rgba(&frame, edge)));
                    true
                }
                Err(e) => {
                    eprintln!("image {} unreadable: {e}", path.display());
                    false
                }
            }
        };
        let history_item = |row: StoredHistory, load: &mut dyn FnMut(ImageId, &Path, usize) -> bool| {
            let thumb = ImageId(HISTORY_THUMB | row.id.0 as u64);
            load(thumb, &row.image, THUMB_EDGE).then_some(HistoryItem {
                id: row.id,
                at: row.at,
                value: row.value,
                confidence: row.confidence,
                review: row.review,
                thumb: Some(thumb),
            })
        };
        let detail = match self.interest.tab {
            Tab::Live => Ok(Detail::Live),
            Tab::Samples => self.store.samples(id).map(|rows| {
                Detail::Samples(
                    rows.into_iter()
                        .filter_map(|row| {
                            let thumb = ImageId(SAMPLE_THUMB | row.id.0 as u64);
                            load(thumb, &row.image, THUMB_EDGE).then_some(SampleItem { id: row.id, label: row.label, thumb, at: row.at })
                        })
                        .collect(),
                )
            }),
            Tab::Review => self
                .store
                .reviews(id, REVIEW_PAGE)
                .map(|rows| Detail::Review(rows.into_iter().filter_map(|row| history_item(row, &mut load)).collect())),
            Tab::History => self.store.history(id, self.interest.history_before, HISTORY_PAGE).map(|(rows, older)| Detail::History {
                rows: rows.into_iter().filter_map(|row| history_item(row, &mut load)).collect(),
                older,
            }),
        };
        let opened = self.interest.open.map(|item| self.store.history_image(id, item).map(|path| (item, path)));
        let opened = match opened.transpose() {
            Ok(Some((item, Some(path)))) => {
                let image = ImageId(HISTORY_FRAME | item.0 as u64);
                load(image, &path, MAX_WIRE_EDGE as usize).then_some(OpenedItem { id: item, image })
            }
            Ok(_) => None,
            Err(e) => {
                self.note(NoticeKind::Store(e.to_string()));
                None
            }
        };
        let detail = detail.unwrap_or_else(|e| {
            self.note(NoticeKind::Store(e.to_string()));
            Detail::Live
        });
        Some(FocusView { sensor: id, preview, detail, opened })
    }

    fn note(&mut self, kind: NoticeKind) {
        self.notice = Some(Notice { at: SystemTime::now(), kind });
        self.system_dirty = true;
    }

    fn emit(&self, report: Report, payload: Vec<u8>) {
        let _ = self.out.send(Out { report, payload });
    }
}

fn entry(s: &Sensor, status: SensorStatus) -> SensorEntry {
    let source = match &s.rec.source {
        SourceSpec::Dvr(camera) => SourceView::Dvr(camera.clone()),
        SourceSpec::Url(url) => SourceView::Url { display: url.display(), snapshot: url.is_snapshot() },
    };
    SensorEntry {
        id: s.rec.id,
        source,
        stream: s.rec.stream,
        settings: s.rec.settings.clone(),
        status,
        value: s.debouncer.published().cloned(),
        live: KindLive::States(StatesLive {
            probabilities: s.probabilities.clone(),
            training: s.training,
            samples: s.samples.clone(),
            review_pending: s.review_pending,
        }),
        last_check: s.last_check,
        notice: s.notice.clone(),
    }
}

/// Uniform in `0.0..1.0`, for the review spot check.
fn roll() -> f32 {
    (getrandom::u32().unwrap_or(0) >> 8) as f32 / (1u32 << 24) as f32
}
