//! The engine: the single owner of every sensor's settings, runtime state and storage. It waits for
//! the next input or deadline, handles it, then runs `tick`. Decisions come from `logic` and
//! `teach`, pixel work from `pixels`, persistence from `store`, frames from `feeds`, vectors, heads
//! and boxes from `infer`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant, SystemTime};

use egui::ColorImage;

use super::Input;
use super::feeds::{FeedId, FeedSource, Feeds};
use super::infer::{BACKBONE, Compare, DetectRequest, Done, Indexed, Infer, Job, Model, ModelEvent, Trained};
use super::logic::{
    self, CheckKind, Debouncer, FeedFact, GeometryVerdict, ObjectTrack, REVIEW, Schedule, Signature, StatusFacts,
};
use super::pixels::{self, Rgb};
use super::store::{Counts, NewHistory, SensorRecord, Store, StoreError, StoredHead, StoredHistory, TAUGHT_CROP, TaughtBox};
use super::teach::{self, TaughtIndex};
use crate::frames::StreamKey;
use crate::plan::RtspUrl;
use crate::procs::UrlKey;
use crate::sensors::types::{
    BoxLabel, Detail, Detector, FocusView, Found, FrameId, Geometry, HistoryId, HistoryItem, ImageId, Interest, KindLive,
    KindSettings, LabelId, MAX_TAUGHT, ModelState, ModelStatus, NormBox, Notice, NoticeKind, ObjectClass, ObjectEvent,
    ObjectKey, ObjectsLive, ObjectsSettings, OpenedItem, Preview, Question, ReviewReason, SampleItem, SampleLabel,
    SensorEntry, SensorId, SensorStatus, SensorValue, Settings, SourceSpec, SourceView, StateKey, StatesLive,
    StatesSettings, SystemStatus, Tab, TrackView, UserCommand, Verdict,
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
const SAMPLE_PAGE: usize = 100;
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
    /// Models never asked for are `NotNeeded`.
    models: BTreeMap<Model, ModelState>,
    model_retry: BTreeMap<Model, Instant>,
    detector: Detector,
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
    /// The feed frame it came from; `None` for an object sensor's analysed frame.
    seq: Option<u64>,
}

/// A sensor's record (written through: store first, then this copy, by the same method) and its
/// runtime state, which lives in memory only: a restart begins with no value and checks at once.
struct Sensor {
    rec: SensorRecord,
    schedule: Schedule,
    /// The region's signature at the last full check: what a probe compares with.
    baseline: Option<Signature>,
    /// Frames shown in the preview, so teaching labels the frame the user saw.
    recent: VecDeque<(FrameId, Arc<ColorImage>)>,
    /// The frame being checked. One check in flight at most; a due check meanwhile waits.
    in_flight: Option<Arc<ColorImage>>,
    /// A head to fit (state sensor) or an index to build (object sensor).
    retrain_at: Option<Instant>,
    training: bool,
    /// The source's size when it differs from the recorded one.
    geometry_seen: Option<Geometry>,
    last_check: Option<SystemTime>,
    failure: Option<String>,
    notice: Option<Notice>,
    counts: Counts,
    sent: Option<SensorEntry>,
    run: Run,
}

/// The runtime state of the sensor's kind; always the kind of `rec.settings.kind`.
enum Run {
    States(StatesRun),
    Objects(ObjectsRun),
}

#[derive(Default)]
struct StatesRun {
    debouncer: Debouncer<SensorValue>,
    head: Option<StoredHead>,
    probabilities: Vec<(StateKey, f32)>,
    /// When the published value changed, within the flip window.
    changes: VecDeque<Instant>,
    last_flag: Option<Instant>,
}

#[derive(Default)]
struct ObjectsRun {
    tracks: BTreeMap<ObjectKey, ObjectTrack>,
    /// What the taught boxes compare with; `None` when nothing was taught or teaching is off.
    index: Option<Arc<TaughtIndex>>,
    /// The last checked frame and the boxes found in it.
    analysed: Option<(FrameId, Arc<ColorImage>)>,
    found: Vec<Found>,
    /// When each own label was last asked about.
    asked: BTreeMap<LabelId, Instant>,
    filtered: BTreeSet<ObjectClass>,
    filtered_logged: BTreeMap<ObjectClass, Instant>,
}

/// A sensor's settings together with its runtime state of the same kind.
enum Kind<'a> {
    States(&'a StatesSettings, &'a mut StatesRun),
    Objects(&'a ObjectsSettings, &'a mut ObjectsRun),
}

impl Sensor {
    fn new(rec: SensorRecord) -> Self {
        let run = match rec.settings.kind {
            KindSettings::States(_) => Run::States(StatesRun::default()),
            KindSettings::Objects(_) => Run::Objects(ObjectsRun::default()),
        };
        Self {
            rec,
            schedule: Schedule::default(),
            baseline: None,
            recent: VecDeque::new(),
            in_flight: None,
            retrain_at: None,
            training: false,
            geometry_seen: None,
            last_check: None,
            failure: None,
            notice: None,
            counts: Counts::default(),
            sent: None,
            run,
        }
    }

    fn kind(&mut self) -> Kind<'_> {
        match (&self.rec.settings.kind, &mut self.run) {
            (KindSettings::States(settings), Run::States(run)) => Kind::States(settings, run),
            (KindSettings::Objects(settings), Run::Objects(run)) => Kind::Objects(settings, run),
            _ => unreachable!("Configure never changes the kind"),
        }
    }

    /// The head, if it was trained for this backbone and the current region.
    fn head(&self) -> Option<&StoredHead> {
        let Run::States(run) = &self.run else { return None };
        run.head.as_ref().filter(|h| h.backbone == BACKBONE.id && h.region == self.rec.settings.region.hash())
    }

    fn feed(&self) -> FeedId {
        match &self.rec.source {
            SourceSpec::Dvr(camera) => FeedId::Dvr(StreamKey { camera: camera.clone(), kind: self.rec.stream }),
            SourceSpec::Url(url) => FeedId::Url(UrlKey::of(url.expose()), self.rec.stream),
        }
    }

    /// Keeps a frame the user may teach from, the newest `FRAME_RING`.
    fn hold(&mut self, frame: FrameId, image: Arc<ColorImage>) {
        self.recent.push_back((frame, image));
        if self.recent.len() > FRAME_RING {
            self.recent.pop_front();
        }
    }

    fn held(&self, frame: FrameId) -> Option<Arc<ColorImage>> {
        self.recent.iter().find(|(f, _)| *f == frame).map(|(_, image)| image.clone())
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
            if let Some(Run::States(run)) = sensors.get_mut(&id).map(|s| &mut s.run) {
                run.head = Some(head);
            }
        }
        for (id, s) in &mut sensors {
            s.counts = store.counts(*id)?;
            // Rebuild from durable samples if the previous process exited before training finished,
            // and build the index of taught boxes, which lives in memory only.
            if !s.counts.states.is_empty() || s.head().is_some() || s.counts.taught > 0 {
                s.retrain_at = Some(Instant::now());
            }
        }
        let now = Instant::now();
        let engine = Self {
            detector: store.detector()?,
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
            models: BTreeMap::new(),
            model_retry: BTreeMap::new(),
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
            Input::Infer(Done::Indexed { sensor, outcome }) => self.indexed(sensor, outcome),
            Input::Infer(Done::Detected { sensor, found }) => self.detected(sensor, found, now),
            Input::Model(ModelEvent { model, state }) => {
                if matches!(state, ModelState::Failed(_)) {
                    self.model_retry.insert(model, now + MODEL_RETRY);
                }
                self.models.insert(model, state);
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
            UserCommand::Configure { id, settings } => self.configure(id, settings, now),
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
                let refusal = match s.kind() {
                    Kind::States(states, _) if states.states.iter().any(|d| d.key == state) => None,
                    Kind::States(..) => Some(NoticeKind::UnknownItem),
                    Kind::Objects(..) => Some(NoticeKind::KindMismatch),
                };
                if let Some(refusal) = refusal {
                    s.note(refusal);
                    return;
                }
                let Some(image) = s.held(frame) else {
                    s.note(NoticeKind::FrameExpired);
                    return;
                };
                let added = jpeg(&Rgb::from(&*image), SAMPLE_QUALITY).and_then(|jpeg| self.store.add_sample(id, &jpeg, state));
                self.learned(id, added.map(|_| true), now);
            }
            UserCommand::TeachBox { id, frame, area, detected, label } => self.teach_box(id, frame, area, detected, label, now),
            UserCommand::Resolve { id, item, label } => {
                let resolved = self.resolve(id, item, label);
                self.learned(id, resolved, now);
            }
            UserCommand::Relabel { id, sample, label } => {
                let Some(s) = self.sensors.get_mut(&id) else { return };
                let fits = match (label, s.kind()) {
                    (None, _) => true,
                    (Some(SampleLabel::State(state)), Kind::States(states, _)) => states.states.iter().any(|d| d.key == state),
                    (Some(SampleLabel::Box(label)), Kind::Objects(objects, _)) => box_label_fits(objects, label, None),
                    _ => false,
                };
                let relabelled = if fits { self.store.relabel(id, sample, label) } else { Ok(false) };
                self.learned(id, relabelled, now);
            }
            UserCommand::SetRetention(retention) => {
                if let Err(e) = self.store.set_retention(retention) {
                    self.note(NoticeKind::Store(e.to_string()));
                }
                self.next_retention = now;
                self.system_dirty = true;
            }
            UserCommand::SetDetector(detector) => {
                match self.store.set_detector(detector) {
                    Ok(()) => self.detector = detector,
                    Err(e) => self.note(NoticeKind::Store(e.to_string())),
                }
                self.system_dirty = true;
            }
        }
    }

    fn configure(&mut self, id: SensorId, settings: Settings, now: Instant) {
        let Some(s) = self.sensors.get_mut(&id) else { return };
        if std::mem::discriminant(&s.rec.settings.kind) != std::mem::discriminant(&settings.kind) {
            s.note(NoticeKind::KindMismatch);
            return;
        }
        let forgotten: Vec<LabelId> = match (&s.rec.settings.kind, &settings.kind) {
            (KindSettings::Objects(old), KindSettings::Objects(new)) => {
                old.labels.iter().map(|l| l.id).filter(|id| new.labels.get(*id).is_none()).collect()
            }
            _ => Vec::new(),
        };
        let mut rec = s.rec.clone();
        let retrain = rec.settings.region != settings.region || rec.settings.kind != settings.kind;
        if rec.settings.region != settings.region {
            s.baseline = None;
        }
        rec.settings = settings;
        rec.geometry = s.geometry_seen.or(rec.geometry);
        let saved = self.store.update(&rec).and_then(|()| self.store.forget_labels(id, &forgotten)).and_then(|()| self.store.counts(id));
        match saved {
            Ok(counts) => {
                s.rec = rec;
                s.counts = counts;
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

    /// One box of a held frame taught as `label` (origin `teach_box`): its crop becomes a sample.
    fn teach_box(&mut self, id: SensorId, frame: FrameId, area: NormBox, detected: Option<ObjectClass>, label: BoxLabel, now: Instant) {
        let Some(s) = self.sensors.get_mut(&id) else { return };
        let taught = s.counts.taught as usize;
        let region = s.rec.settings.region.clone();
        let refusal = match s.kind() {
            Kind::States(..) => Some(NoticeKind::KindMismatch),
            Kind::Objects(objects, _) if !box_label_fits(objects, label, Some(detected)) => Some(NoticeKind::UnknownItem),
            Kind::Objects(..) if taught >= MAX_TAUGHT => Some(NoticeKind::TaughtFull),
            Kind::Objects(..) => {
                let [x1, _, x2, y2] = area.area();
                (!logic::in_region((x1 + x2) / 2.0, y2, &region)).then_some(NoticeKind::OutsideRegion)
            }
        };
        if let Some(refusal) = refusal {
            s.note(refusal);
            return;
        }
        let Some(image) = s.held(frame) else {
            s.note(NoticeKind::FrameExpired);
            return;
        };
        let added = self.add_taught(id, &image, TaughtBox { area: area.area(), detected }, label);
        self.learned(id, added.map(|_| true), now);
    }

    fn add_taught(&mut self, id: SensorId, frame: &ColorImage, taught: TaughtBox, label: BoxLabel) -> Result<(), StoreError> {
        let crop = pixels::crop_box(frame, teach::crop_area(taught.area));
        let jpeg = jpeg(&crop, SAMPLE_QUALITY)?;
        self.store.add_taught(id, &jpeg, label, taught).map(|_| ())
    }

    /// A state sensor's answer turns the frame into a sample; an object sensor's teaches the box it
    /// asked about. `false` when the item or the answer does not belong to the sensor.
    fn resolve(&mut self, id: SensorId, item: HistoryId, label: Option<SampleLabel>) -> Result<bool, StoreError> {
        let Some(s) = self.sensors.get_mut(&id) else { return Ok(false) };
        let Some(row) = self.store.history_item(id, item)? else { return Ok(false) };
        match (label, s.kind()) {
            (None, _) => {}
            (Some(SampleLabel::State(state)), Kind::States(states, _)) if states.states.iter().any(|d| d.key == state) => {
                let jpeg = fs::read(&row.image)?;
                self.store.add_sample(id, &jpeg, state)?;
            }
            (Some(SampleLabel::Box(label)), Kind::Objects(objects, _)) => {
                let Some(ReviewReason::Ask(question)) = row.review else { return Ok(false) };
                if !box_label_fits(objects, label, Some(Some(question.detected))) {
                    return Ok(false);
                }
                let frame = pixels::decode(&fs::read(&row.image)?).map_err(|e| StoreError::from(std::io::Error::other(e)))?;
                self.add_taught(id, &frame, TaughtBox { area: question.area, detected: Some(question.detected) }, label)?;
            }
            _ => return Ok(false),
        }
        self.store.mark_resolved(item)?;
        Ok(true)
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
            Ok(counts) => s.counts = counts,
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
        let probabilities = match (s.head(), &s.rec.settings.kind) {
            (Some(head), KindSettings::States(states)) => head.head.predict(&vector, &states.states),
            _ => Vec::new(),
        };
        let Kind::States(states, run) = s.kind() else { return };
        let (value, top) = logic::state_verdict(&probabilities, states.threshold);
        let changed = run.debouncer.update(value.clone(), states.debounce);
        if changed {
            run.changes.push_back(now);
        }
        let confidence = top.map_or(0.0, |(_, p)| p);
        let review = if probabilities.is_empty() {
            None
        } else {
            let recent = logic::recent_changes(&REVIEW, &mut run.changes, now);
            let since = run.last_flag.map(|at| now.duration_since(at));
            let reason = logic::review_reason(&REVIEW, confidence, states.threshold, recent, since, roll());
            if reason.is_some() {
                run.last_flag = Some(now);
            }
            reason
        };
        run.probabilities = probabilities;
        s.last_check = Some(SystemTime::now());
        if !changed && review.is_none() {
            return;
        }
        let row = NewHistory { at: SystemTime::now(), value: &value, confidence, changed, review, jpeg: &[], events: &[], found: &[] };
        self.record(id, &frame, row);
    }

    /// One history row with the checked frame; a new view when the panel shows this sensor.
    fn record(&mut self, id: SensorId, frame: &ColorImage, row: NewHistory<'_>) {
        let review = row.review.is_some();
        let recorded = jpeg(&Rgb::from(frame), HISTORY_QUALITY).and_then(|jpeg| self.store.record(id, &NewHistory { jpeg: &jpeg, ..row }));
        let Some(s) = self.sensors.get_mut(&id) else { return };
        match recorded {
            Ok(_) => {
                s.counts.pending += u32::from(review);
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
        if let Run::States(run) = &mut s.run {
            run.head = head.map(|head| StoredHead { backbone: BACKBONE.id.to_owned(), region, head });
        }
        // The value follows the new head at once (origin: "the training checks again when done").
        s.schedule.force = true;
    }

    /// The taught boxes are ready to compare with; the next check uses them at once (origin
    /// `taught_changed`).
    fn indexed(&mut self, id: SensorId, outcome: Result<Indexed, String>) {
        let Some(s) = self.sensors.get_mut(&id) else { return };
        s.training = false;
        let Indexed { index, fresh } = match outcome {
            Ok(indexed) => indexed,
            Err(error) => {
                s.failure = Some(error);
                return;
            }
        };
        if let Err(e) = self.store.put_embeddings(BACKBONE.id, TAUGHT_CROP, &fresh) {
            s.note(NoticeKind::Store(e.to_string()));
        }
        s.failure = None;
        if let Run::Objects(run) = &mut s.run {
            run.index = index.map(Arc::new);
        }
        s.schedule.force = true;
    }

    /// One check of an object sensor came back (origin `_run_objects` from the tracks on): the
    /// tracks move, what changed goes to the history, unsure own labels are asked about.
    fn detected(&mut self, id: SensorId, found: Result<Vec<Found>, String>, now: Instant) {
        let frame_id = FrameId(self.next_id);
        self.next_id += 1;
        let Some(s) = self.sensors.get_mut(&id) else { return };
        let Some(frame) = s.in_flight.take() else { return };
        let found = match found {
            Ok(found) => found,
            Err(error) => {
                s.failure = Some(error);
                return;
            }
        };
        s.failure = None;
        s.last_check = Some(SystemTime::now());
        let Kind::Objects(objects, run) = s.kind() else { return };
        let keys = objects.keys();
        let changed = logic::update_tracks(&mut run.tracks, &found, &keys, objects.debounce, objects.clear_after.get(), now);
        let (filtered, newly) = logic::filtered_classes(&found, &run.filtered, &run.filtered_logged, now);
        run.filtered = filtered;
        let mut events: Vec<ObjectEvent> =
            changed.iter().map(|k| if run.tracks[k].on { ObjectEvent::On(*k) } else { ObjectEvent::Off(*k) }).collect();
        let mut confidence = changed.iter().map(|k| run.tracks[k].score).fold(0.0, f32::max);
        for class in newly {
            run.filtered_logged.insert(class, now);
            events.push(ObjectEvent::Filtered(class));
            confidence = found.iter().filter(|f| f.filtered() && f.class == class).map(|f| f.score).fold(confidence, f32::max);
        }
        let asks: Vec<Question> = logic::questions(&REVIEW, &found, &run.asked, now)
            .into_iter()
            .filter_map(|(label, f)| match f.verdict {
                Verdict::Unsure { similarity, .. } => {
                    Some(Question { label, detected: f.class, area: f.area, similarity })
                }
                _ => None,
            })
            .collect();
        let value = logic::present(&run.tracks, &keys);
        run.analysed = Some((frame_id, frame.clone()));
        run.found = found.clone();
        let recheck = logic::recheck_at(&run.tracks, now);
        s.schedule.recheck_at = recheck;
        self.focus_dirty |= self.interest.sensor == Some(id) && self.interest.tab == Tab::Live;
        if !events.is_empty() {
            let row = NewHistory { at: SystemTime::now(), value: &value, confidence, changed: true, review: None, jpeg: &[], events: &events, found: &found };
            self.record(id, &frame, row);
        }
        for question in asks {
            self.ask(id, &frame, &value, &found, question, now);
        }
    }

    /// "Is this <label>?" in the review queue, unless a question about that label already waits
    /// there (origin `_record_question`).
    fn ask(&mut self, id: SensorId, frame: &ColorImage, value: &SensorValue, found: &[Found], question: Question, now: Instant) {
        let waiting = match self.store.reviews(id, REVIEW_PAGE) {
            Ok(rows) => rows.iter().any(|r| matches!(r.review, Some(ReviewReason::Ask(q)) if q.label == question.label)),
            Err(e) => {
                self.note(NoticeKind::Store(e.to_string()));
                return;
            }
        };
        if waiting {
            return;
        }
        if let Some(Run::Objects(run)) = self.sensors.get_mut(&id).map(|s| &mut s.run) {
            run.asked.insert(question.label, now);
        }
        let confidence = found.iter().filter(|f| f.area == question.area).map(|f| f.score).fold(0.0, f32::max);
        let review = Some(ReviewReason::Ask(question));
        let row = NewHistory { at: SystemTime::now(), value, confidence, changed: false, review, jpeg: &[], events: &[], found };
        self.record(id, frame, row);
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
        self.ensure_models(now);
        for at in self.model_retry.values() {
            soon(*at);
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

    /// The backbone is needed once a state sensor has samples to learn or a head to check with, or
    /// an object sensor has taught boxes to compare; the detector once an object sensor exists.
    /// Each is fetched once and retried after a failure. A sensor nobody taught downloads nothing.
    fn ensure_models(&mut self, now: Instant) {
        let mut needed = BTreeSet::new();
        for s in self.sensors.values() {
            match &s.rec.settings.kind {
                KindSettings::States(_) if s.retrain_at.is_some() || s.head().is_some() => {
                    needed.insert(Model::Backbone);
                }
                KindSettings::States(_) => {}
                KindSettings::Objects(objects) => {
                    needed.insert(Model::Detector(self.detector));
                    if objects.use_taught && s.counts.taught > 0 {
                        needed.insert(Model::Backbone);
                    }
                }
            }
        }
        for model in needed {
            let due = match self.models.get(&model) {
                None | Some(ModelState::NotNeeded) => true,
                Some(ModelState::Failed(_)) => self.model_retry.get(&model).is_some_and(|at| *at <= now),
                Some(ModelState::Downloading { .. } | ModelState::Ready) => false,
            };
            if due {
                self.model_retry.remove(&model);
                self.models.insert(model, ModelState::Downloading { done: 0, total: model.spec().size });
                self.system_dirty = true;
                self.infer.ensure(model);
            }
        }
    }

    fn model(&self, model: Model) -> ModelState {
        self.models.get(&model).cloned().unwrap_or(ModelState::NotNeeded)
    }

    /// One sensor's geometry, retrain and check. Returns when it wants to run again.
    fn step(&mut self, id: SensorId, now: Instant) -> Option<Instant> {
        let backbone = self.model(Model::Backbone) == ModelState::Ready;
        let detector = self.model(Model::Detector(self.detector)) == ModelState::Ready;
        let which = self.detector;
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
            } else if backbone && !s.training {
                s.retrain_at = None;
                let region = s.rec.settings.region.clone();
                let job = match s.kind() {
                    Kind::States(states, _) => {
                        let labels: BTreeSet<StateKey> = states.states.iter().map(|d| d.key).collect();
                        self.store.training_set(id, &labels, BACKBONE.id, region.hash()).map(|rows| Job::Train { sensor: id, region, rows })
                    }
                    Kind::Objects(objects, _) => {
                        let parents = objects.active_labels().map(|l| (l.id, l.parent)).collect();
                        self.store.taught_set(id, BACKBONE.id).map(|rows| Job::Index { sensor: id, rows, parents })
                    }
                };
                match job {
                    Ok(job) => {
                        s.training = true;
                        self.infer.submit(job);
                    }
                    Err(e) => s.note(NoticeKind::Store(e.to_string())),
                }
            }
        }
        let ready = match &s.rec.settings.kind {
            KindSettings::States(_) => backbone && s.head().is_some(),
            KindSettings::Objects(_) => detector,
        };
        if s.in_flight.is_some() || s.geometry_seen.is_some() || !ready {
            return next;
        }
        let settings = &s.rec.settings;
        let (at, kind) = logic::next_check(&s.schedule, settings.triggers, settings.interval, settings.paused, now)?;
        if at > now {
            return Some(next.map_or(at, |n| n.min(at)));
        }
        // No fresh frame: the feed wakes the engine when one arrives.
        let frame = self.feeds.frame(&feed, now, FRESH)?;
        let region = s.rec.settings.region.clone();
        if kind == CheckKind::Probe {
            s.schedule.last_probe = Some(now);
            let signature = pixels::signature(&frame.image, &region);
            match &s.baseline {
                Some(baseline) if !logic::changed(baseline, &signature) => return next,
                Some(_) => logic::start_burst(&mut s.schedule, now),
                None => {}
            }
        }
        s.schedule.last_run = Some(now);
        s.schedule.force = false;
        s.schedule.recheck_at = None;
        s.baseline = Some(pixels::signature(&frame.image, &region));
        let job = match s.kind() {
            Kind::States(..) => Job::Embed { sensor: id, crop: pixels::crop(&frame.image, &region) },
            Kind::Objects(objects, run) => {
                let taught = run.index.clone().filter(|_| objects.use_taught && backbone);
                Job::Detect {
                    sensor: id,
                    frame: frame.image.clone(),
                    request: DetectRequest {
                        detector: which,
                        region,
                        classes: objects.classes.clone(),
                        threshold: objects.threshold,
                        min_size: objects.min_size,
                        taught: taught.map(|index| Compare { index, previous: run.found.clone() }),
                    },
                }
            }
        };
        self.infer.submit(job);
        s.in_flight = Some(frame.image);
        // A paused sensor only had a feed for "check now"; the frame is taken, so the feed can go.
        self.feeds_dirty |= s.rec.settings.paused;
        next
    }

    /// A new preview frame for the focused sensor's live tab: an object sensor's last checked frame,
    /// so its boxes are drawn on the frame they were found in; otherwise the feed's newest frame.
    fn step_preview(&mut self, now: Instant) -> Option<Instant> {
        let focused = self.interest.sensor.filter(|_| self.interest.tab == Tab::Live);
        let Some(s) = focused.and_then(|id| self.sensors.get_mut(&id)) else {
            if self.preview.take().is_some() {
                self.focus_dirty = true;
            }
            return None;
        };
        let shown = self.preview.as_ref().filter(|p| p.sensor == s.rec.id);
        if let Run::Objects(ObjectsRun { analysed: Some((frame, image)), .. }) = &s.run {
            let (frame, image) = (*frame, image.clone());
            if shown.is_none_or(|p| p.preview.frame != frame) {
                let [width, height] = image.size.map(|n| n as u32);
                let geometry = s.rec.geometry.unwrap_or(Geometry { width, height });
                s.hold(frame, image.clone());
                let preview = Preview { frame, image: ImageId(self.next_id), geometry };
                self.next_id += 1;
                self.preview = Some(Shown { sensor: s.rec.id, preview, image: pixels::fit_rgba(&image, MAX_WIRE_EDGE as usize), seq: None });
                self.focus_dirty = true;
            }
            return None;
        }
        let feed = s.feed();
        let frame = self.feeds.frame(&feed, now, PREVIEW_EVERY).or_else(|| self.feeds.latest(&feed).cloned())?;
        let next = (frame.at + PREVIEW_EVERY > now).then_some(frame.at + PREVIEW_EVERY);
        if shown.is_some_and(|p| p.seq == Some(frame.seq)) {
            return next;
        }
        let image = pixels::fit_rgba(&frame.image, MAX_WIRE_EDGE as usize);
        let [width, height] = frame.image.size.map(|n| n as u32);
        let preview = Preview {
            frame: FrameId(self.next_id),
            image: ImageId(self.next_id),
            geometry: frame.geometry.unwrap_or(Geometry { width, height }),
        };
        self.next_id += 1;
        s.hold(preview.frame, frame.image);
        self.preview = Some(Shown { sensor: s.rec.id, preview, image, seq: Some(frame.seq) });
        self.focus_dirty = true;
        next
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
        let (model, trained) = match &s.rec.settings.kind {
            KindSettings::States(_) => (Model::Backbone, s.head().is_some()),
            KindSettings::Objects(_) => (Model::Detector(self.detector), true),
        };
        let status = logic::sensor_status(StatusFacts {
            paused: s.rec.settings.paused,
            geometry: s.rec.geometry.zip(s.geometry_seen),
            feed,
            model: self.model(model),
            failure: s.failure.clone(),
            trained,
        });
        let entry = entry(s, status);
        let s = self.sensors.get_mut(&id).expect("looked up above");
        if s.sent.as_ref() != Some(&entry) {
            s.sent = Some(entry.clone());
            self.emit(Report::Sensor(Box::new(entry)), Vec::new());
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
        let models = [Model::Backbone, Model::Detector(self.detector)]
            .map(|model| ModelStatus { name: model.spec().name.to_owned(), state: self.model(model) })
            .into();
        SystemStatus { retention, usage, models, detector: self.detector, notice: self.notice.clone() }
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
                events: row.events,
            })
        };
        let detail = match self.interest.tab {
            Tab::Live => Ok(Detail::Live),
            Tab::Samples => self.store.samples(id, self.interest.samples_before, SAMPLE_PAGE).map(|(rows, older)| {
                Detail::Samples {
                    rows: rows.into_iter()
                        .filter_map(|row| {
                            let thumb = ImageId(SAMPLE_THUMB | row.id.0 as u64);
                            load(thumb, &row.image, THUMB_EDGE).then_some(SampleItem { id: row.id, label: row.label, thumb, at: row.at })
                        })
                        .collect(),
                    before: self.interest.samples_before,
                    older,
                }
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
        let opened = self.interest.open.map(|item| self.store.history_image(id, item).map(|found| (item, found)));
        let opened = match opened.transpose() {
            Ok(Some((item, Some((path, found))))) => {
                let image = ImageId(HISTORY_FRAME | item.0 as u64);
                load(image, &path, MAX_WIRE_EDGE as usize).then_some(OpenedItem { id: item, image, found })
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

/// Whether an object sensor can be taught `label` (origin `teach_box`'s checks): one of its
/// objects, one of its own labels in use, or "not this" for a box the detector found.
/// `detected` is `None` when the box is not known yet (relabelling a stored one).
fn box_label_fits(objects: &ObjectsSettings, label: BoxLabel, detected: Option<Option<ObjectClass>>) -> bool {
    match label {
        BoxLabel::NotThis => detected.is_none_or(|d| d.is_some()),
        BoxLabel::Class(class) => objects.classes.contains(class),
        BoxLabel::Own(id) => objects.active_labels().any(|l| l.id == id),
    }
}

fn jpeg(img: &Rgb, quality: u8) -> Result<Vec<u8>, StoreError> {
    pixels::encode_jpeg(img, quality).map_err(|e| StoreError::from(std::io::Error::other(e)))
}

fn entry(s: &Sensor, status: SensorStatus) -> SensorEntry {
    let source = match &s.rec.source {
        SourceSpec::Dvr(camera) => SourceView::Dvr(camera.clone()),
        SourceSpec::Url(url) => SourceView::Url { display: url.display(), snapshot: url.is_snapshot() },
    };
    let (value, live) = match (&s.rec.settings.kind, &s.run) {
        (KindSettings::Objects(objects), Run::Objects(run)) => {
            let keys = objects.keys();
            let tracks = keys
                .iter()
                .map(|key| {
                    let track = run.tracks.get(key).cloned().unwrap_or_default();
                    TrackView { key: *key, on: track.on, count: track.count, score: track.score }
                })
                .collect();
            let live = ObjectsLive {
                frame: run.analysed.as_ref().map(|(frame, _)| *frame),
                found: run.found.clone(),
                tracks,
                taught: s.counts.taught,
                indexing: s.training,
                review_pending: s.counts.pending,
            };
            (run.analysed.as_ref().map(|_| logic::present(&run.tracks, &keys)), KindLive::Objects(live))
        }
        (_, Run::States(run)) => {
            let live = StatesLive {
                probabilities: run.probabilities.clone(),
                training: s.training,
                samples: s.counts.states.clone(),
                review_pending: s.counts.pending,
            };
            (run.debouncer.published().cloned(), KindLive::States(live))
        }
        (KindSettings::States(_), Run::Objects(_)) => unreachable!("Configure never changes the kind"),
    };
    SensorEntry {
        id: s.rec.id,
        source,
        stream: s.rec.stream,
        settings: s.rec.settings.clone(),
        status,
        value,
        live,
        last_check: s.last_check,
        notice: s.notice.clone(),
    }
}

/// Uniform in `0.0..1.0`, for the review spot check.
fn roll() -> f32 {
    (getrandom::u32().unwrap_or(0) >> 8) as f32 / (1u32 << 24) as f32
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
