//! Pure decisions of the engine, ported from the origin (`engine/logic.py`, `engine/state.py`,
//! `engine/history.py`). No I/O, no clock (time comes in as arguments), no randomness (the roll
//! comes in). Constants are copied from `settings.py`, with the origin's name beside each.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use super::faces::FACES;
use crate::sensors::types::{
    ClassSet, Debounce, FaceState, Found, Geometry, HistoryId, Interval, LabelId, MinSize, ModelState, ObjectClass,
    ObjectKey, PersonId, Region, RegionShape, Retention, ReviewReason, SensorStatus, SensorValue, StateKey, Triggers,
    Unit, Verdict,
};

pub const BURST_INTERVAL: Duration = Duration::from_secs(2); // TRIGGER_DEFAULTS["burst_interval_s"]
pub const BURST_DURATION: Duration = Duration::from_secs(30); // TRIGGER_DEFAULTS["burst_duration_s"]
pub const CHANGE_INTERVAL: Duration = Duration::from_secs(2); // TRIGGER_DEFAULTS["change_interval_s"]
pub const CHANGE_THRESHOLD: f32 = 0.04; // TRIGGER_DEFAULTS["change_threshold"]
pub const SIGNATURE_SIZE: usize = 48; // CHANGE_SIGNATURE_SIZE
pub const NEUTRAL_FILL: [u8; 3] = [124, 116, 104]; // NEUTRAL_FILL
pub const CONTEXT_MARGIN: f32 = 0.25; // DETECTION["context_margin"]
pub const CLEAR_MISSES: u32 = 2; // DETECTION["clear_misses"]
pub const RECHECK: Duration = Duration::from_secs(5); // DETECTION["recheck_s"]
pub const FILTERED_RECORD_COOLDOWN: Duration = Duration::from_secs(600); // TEACH["filtered_record_cooldown_s"]

/// Review queue rules. Global for now: no per-sensor overrides.
#[derive(Clone, Copy, Debug)]
pub struct ReviewRules {
    pub enabled: bool,
    /// Flag frames below this confidence, never lower than the sensor's threshold.
    pub below: f32,
    /// At most one flagged frame per sensor per cooldown.
    pub cooldown: Duration,
    /// This many published changes within `flip_window` is flip-flopping.
    pub flip_limit: usize,
    pub flip_window: Duration,
    /// Share of confident frames sent for a random spot check.
    pub spot_rate: f32,
}

pub const REVIEW: ReviewRules = ReviewRules {
    enabled: true,                         // REVIEW_DEFAULTS["enabled"]
    below: 0.85,                           // REVIEW_DEFAULTS["below"]
    cooldown: Duration::from_secs(300),    // REVIEW_DEFAULTS["cooldown_s"]
    flip_limit: 3,                         // REVIEW_DEFAULTS["flip_limit"]
    flip_window: Duration::from_secs(600), // REVIEW_DEFAULTS["flip_window_s"]
    spot_rate: 0.0,                        // REVIEW_DEFAULTS["spot_rate"]
};

/// Per-sensor timing memory (origin `LiveState`).
#[derive(Clone, Debug, Default)]
pub struct Schedule {
    pub last_run: Option<Instant>,
    pub last_probe: Option<Instant>,
    pub burst_until: Option<Instant>,
    /// Look again then, before the regular check (origin `recheck_at`).
    pub recheck_at: Option<Instant>,
    /// "Check now": one full check at the next tick, even when paused.
    pub force: bool,
    pub face_burst: FaceBurst,
}

/// Checks every `FACES.burst_interval` while someone nobody is named for is there (origin
/// `FaceBurst`, steered by `steer_face_burst`).
#[derive(Clone, Debug, Default)]
pub struct FaceBurst {
    /// Running until then.
    pub until: Option<Instant>,
    /// Checks in a row without a usable face or the person coming closer.
    idle: u32,
    /// Checks in a row without a person.
    absent: u32,
    /// The tallest person box (share of the frame) and the widest face (pixels) of this burst.
    best_height: f32,
    best_face: f32,
    /// This visit had its burst: none again until the person has gone.
    spent: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BurstChange {
    Start,
    Stop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckKind {
    /// Embed and classify.
    Full,
    /// Only compare the region's signature with the last one.
    Probe,
}

/// When the sensor should wake next, and for what (origin `next_check_at`). `None`: nothing is
/// scheduled until something wakes the sensor.
pub fn next_check(
    s: &Schedule,
    triggers: Triggers,
    interval: Interval,
    paused: bool,
    now: Instant,
) -> Option<(Instant, CheckKind)> {
    if s.force {
        return Some((now, CheckKind::Full));
    }
    if paused {
        return None;
    }
    let Some(last) = s.last_run else {
        return Some((now, CheckKind::Full));
    };
    let paces = [
        s.burst_until.filter(|until| now < *until).map(|_| BURST_INTERVAL),
        s.face_burst.until.filter(|until| now < *until).map(|_| FACES.burst_interval),
    ];
    let mut full_at = if let Some(pace) = paces.into_iter().flatten().min() {
        Some(last + pace)
    } else if triggers.regular {
        // Counted from the last check, whatever caused it: triggers postpone it.
        Some(last + interval.get())
    } else {
        None
    };
    if let Some(recheck) = s.recheck_at
        && full_at.is_none_or(|at| recheck < at)
    {
        full_at = Some(recheck);
    }
    if triggers.change_detection {
        let probe_at = s.last_probe.map_or(last, |probe| probe.max(last)) + CHANGE_INTERVAL;
        if full_at.is_none_or(|at| probe_at < at) {
            return Some((probe_at, CheckKind::Probe));
        }
    }
    full_at.map(|at| (at, CheckKind::Full))
}

/// A change extends the burst; the caller then runs a full check.
pub fn start_burst(s: &mut Schedule, now: Instant) {
    s.burst_until = Some(now + BURST_DURATION);
}

/// After a check of a sensor that recognises faces: start or end looking for a face more often
/// while a person is there (origin `steer_face_burst`). Someone walking up to a door shows a usable
/// face for a few seconds only, often between two regular checks; someone sitting on the steps
/// gets one burst, not one an hour.
pub fn steer_face_burst(burst: &mut FaceBurst, found: &[Found], now: Instant) -> Option<BurstChange> {
    let people: Vec<&Found> = found.iter().filter(|f| f.class == ObjectClass::PERSON && !f.filtered()).collect();
    if people.is_empty() {
        burst.absent += 1;
        if burst.absent >= FACES.burst_absent_checks {
            burst.spent = false;
            return end_face_burst(burst, false);
        }
        return None;
    }
    burst.absent = 0;
    let unnamed: Vec<&Found> = people.iter().copied().filter(|f| f.face.is_none_or(|l| l.person().is_none())).collect();
    let height = people.iter().map(|f| f.area[3] - f.area[1]).fold(0.0, f32::max);
    let width = people.iter().map(|f| f.face.map_or(0.0, |l| l.width_px)).fold(0.0, f32::max);
    let Some(until) = burst.until else {
        if burst.spent || unnamed.is_empty() {
            return None;
        }
        *burst = FaceBurst { until: Some(now + FACES.burst_max), idle: 0, absent: 0, best_height: height, best_face: width, spent: false };
        return Some(BurstChange::Start);
    };
    if unnamed.is_empty() || now >= until {
        return end_face_burst(burst, true);
    }
    let usable = unnamed.iter().any(|f| f.face.is_some_and(|l| l.usable()));
    let closer = height > burst.best_height * FACES.burst_progress || width > burst.best_face * FACES.burst_progress;
    burst.idle = if usable || closer { 0 } else { burst.idle + 1 };
    burst.best_height = burst.best_height.max(height);
    burst.best_face = burst.best_face.max(width);
    if burst.idle >= FACES.burst_idle_checks {
        return end_face_burst(burst, true);
    }
    None
}

fn end_face_burst(burst: &mut FaceBurst, spent: bool) -> Option<BurstChange> {
    let running = burst.until.take().is_some();
    burst.idle = 0;
    burst.spent |= spent;
    running.then_some(BurstChange::Stop)
}

/// A grey thumbnail of the region, values in `0.0..=1.0` (`imaging.region_signature`).
#[derive(Clone, Debug, PartialEq)]
pub struct Signature(pub(super) Box<[f32; SIGNATURE_SIZE * SIGNATURE_SIZE]>);

/// Whether the region changed enough between two probes to start a burst.
pub fn changed(previous: &Signature, current: &Signature) -> bool {
    change_score(previous, current) >= CHANGE_THRESHOLD
}

/// Mean absolute difference: 0 identical, 1 inverted (`imaging.change_score`).
fn change_score(previous: &Signature, current: &Signature) -> f32 {
    let total: f32 = previous.0.iter().zip(current.0.iter()).map(|(a, b)| (a - b).abs()).sum();
    total / previous.0.len() as f32
}

/// The value one check yields, and the top state with its probability (`Checks._run_states`).
/// No probabilities (no head) is `Unknown` with no top.
pub fn state_verdict(probabilities: &[(StateKey, f32)], threshold: Unit) -> (SensorValue, Option<(StateKey, f32)>) {
    let top = probabilities.iter().copied().fold(None, |best, (key, p)| match best {
        Some((_, top)) if top >= p => best,
        _ => Some((key, p)),
    });
    let value = match top {
        Some((key, p)) if p >= f32::from(threshold) => SensorValue::State(key),
        _ => SensorValue::Unknown,
    };
    (value, top)
}

/// Changes the published value only after `required` equal results in a row; the first result is
/// published at once (origin `Debouncer`).
#[derive(Clone, Debug)]
pub struct Debouncer<V> {
    published: Option<V>,
    candidate: Option<V>,
    count: u8,
}

impl<V> Default for Debouncer<V> {
    fn default() -> Self {
        Self { published: None, candidate: None, count: 0 }
    }
}

impl<V: Clone + PartialEq> Debouncer<V> {
    /// Whether the published value changed.
    pub fn update(&mut self, value: V, required: Debounce) -> bool {
        if self.candidate.as_ref() == Some(&value) {
            self.count = self.count.saturating_add(1);
        } else {
            self.candidate = Some(value.clone());
            self.count = 1;
        }
        let differs = self.published.as_ref() != Some(&value);
        if self.published.is_none() || (self.count >= u8::from(required) && differs) {
            self.published = Some(value);
            return differs;
        }
        false
    }

    pub fn published(&self) -> Option<&V> {
        self.published.as_ref()
    }

    /// A value confirmed elsewhere (restored, or answered in review) is the published one, with
    /// nothing pending.
    pub fn anchor(&mut self, value: V) {
        self.published = Some(value);
        self.candidate = None;
        self.count = 0;
    }
}

/// Whether a checked frame goes to the review queue, and why (origin `review_reason`).
/// `since_flag`: time since this sensor's last flagged frame, `None` if never. `roll` is uniform
/// in `0.0..1.0`.
pub fn review_reason(
    rules: &ReviewRules,
    confidence: f32,
    threshold: Unit,
    recent_changes: usize,
    since_flag: Option<Duration>,
    roll: f32,
) -> Option<ReviewReason> {
    if !rules.enabled || since_flag.is_some_and(|since| since < rules.cooldown) {
        return None;
    }
    if confidence < rules.below.max(threshold.into()) {
        Some(ReviewReason::LowConfidence)
    } else if recent_changes >= rules.flip_limit {
        Some(ReviewReason::Flip)
    } else if roll < rules.spot_rate {
        Some(ReviewReason::SpotCheck)
    } else {
        None
    }
}

/// Forgets published changes older than the flip window and counts the rest.
pub fn recent_changes(rules: &ReviewRules, changes: &mut VecDeque<Instant>, now: Instant) -> usize {
    while changes.front().is_some_and(|at| now.duration_since(*at) > rules.flip_window) {
        changes.pop_front();
    }
    changes.len()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeometryVerdict {
    /// Nothing recorded yet: record what was seen.
    Adopt,
    Same,
    /// Checks stop until the user saves the sensor again.
    Changed,
}

pub fn geometry_verdict(recorded: Option<Geometry>, seen: Geometry) -> GeometryVerdict {
    match recorded {
        None => GeometryVerdict::Adopt,
        Some(recorded) if recorded == seen => GeometryVerdict::Same,
        Some(_) => GeometryVerdict::Changed,
    }
}

/// What `sensor_status` derives from, gathered by the engine at publish time.
pub struct StatusFacts {
    pub paused: bool,
    /// `(expected, seen)` when the geometry changed.
    pub geometry: Option<(Geometry, Geometry)>,
    pub feed: FeedFact,
    pub model: ModelState,
    pub failure: Option<String>,
    pub trained: bool,
}

pub enum FeedFact {
    /// No URL yet, or the decoder is starting.
    Waiting,
    Up,
    Down {
        reason: String,
        retry_in: Option<Duration>,
    },
}

/// Precedence is the declaration order of `SensorStatus`.
pub fn sensor_status(f: StatusFacts) -> SensorStatus {
    if f.paused {
        return SensorStatus::Paused;
    }
    if let Some((expected, seen)) = f.geometry {
        return SensorStatus::GeometryChanged { expected, seen };
    }
    match f.feed {
        FeedFact::Waiting => return SensorStatus::WaitingSource,
        FeedFact::Down { reason, retry_in } => return SensorStatus::SourceDown { reason, retry_in },
        FeedFact::Up => {}
    }
    match f.model {
        ModelState::Downloading { done, total } => {
            let progress = (total > 0).then(|| done as f32 / total as f32);
            return SensorStatus::WaitingModel { progress, failed: None };
        }
        ModelState::Failed(error) => return SensorStatus::WaitingModel { progress: None, failed: Some(error) },
        ModelState::NotNeeded | ModelState::Ready => {}
    }
    match (f.failure, f.trained) {
        (Some(error), _) => SensorStatus::Failed(error),
        (None, false) => SensorStatus::Untrained,
        (None, true) => SensorStatus::Live,
    }
}

/// One history row as retention sees it.
pub struct RowCost {
    pub id: HistoryId,
    pub age: Duration,
    pub bytes: u64,
    /// Flagged for review and not answered yet.
    pub pending_review: bool,
}

/// Rows to delete (origin `cleanup_history`): every row older than `max_age` (twice that for a row
/// waiting for review), then the oldest until the rest fit `max_bytes`, rows waiting for review
/// only when the others are not enough. Samples are not history rows: never pruned here.
pub fn retention_plan(rows_oldest_first: &[RowCost], policy: Retention) -> Vec<HistoryId> {
    let expired = |r: &&RowCost| r.age > if r.pending_review { policy.max_age * 2 } else { policy.max_age };
    let (mut doomed, kept): (Vec<&RowCost>, Vec<&RowCost>) = rows_oldest_first.iter().partition(expired);
    let mut used: u64 = kept.iter().map(|r| r.bytes).sum();
    for waiting in [false, true] {
        for row in kept.iter().filter(|r| r.pending_review == waiting) {
            if used <= policy.max_bytes {
                break;
            }
            used -= row.bytes;
            doomed.push(row);
        }
    }
    doomed.into_iter().map(|r| r.id).collect()
}

/// The region's bounding box grown by `margin` × its size on each side, clamped to the frame, as
/// `[x1, y1, x2, y2]` (`imaging.region_box`).
pub fn region_box(region: &Region, margin: f32) -> [f32; 4] {
    let (min, max) = region.bounds();
    let (dx, dy) = ((max.x() - min.x()) * margin, (max.y() - min.y()) * margin);
    [(min.x() - dx).max(0.0), (min.y() - dy).max(0.0), (max.x() + dx).min(1.0), (max.y() + dy).min(1.0)]
}

/// Whether a normalised point lies inside the region; ray casting for a polygon
/// (`imaging.in_region`).
pub fn in_region(x: f32, y: f32, region: &Region) -> bool {
    match region.shape() {
        RegionShape::Whole => true,
        RegionShape::Rect { min, max } => (min.x()..=max.x()).contains(&x) && (min.y()..=max.y()).contains(&y),
        RegionShape::Polygon(points) => {
            let mut inside = false;
            for (a, b) in points.iter().zip(points.iter().cycle().skip(points.len() - 1)) {
                let (xi, yi, xj, yj) = (a.x(), a.y(), b.x(), b.y());
                if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
                    inside = !inside;
                }
            }
            inside
        }
    }
}

/// One box as the detector gives it, normalised to the image it was shown (origin `Detection`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Detection {
    pub class: ObjectClass,
    pub score: f32,
    pub area: [f32; 4],
}

/// What decides which boxes count, from the sensor's settings.
pub struct ObjectFilter<'a> {
    pub region: &'a Region,
    pub classes: &'a ClassSet,
    pub min_size: MinSize,
}

/// Maps boxes from the `analysed` part of the frame to the whole frame and keeps those that count:
/// the bottom centre (where a person or car stands) inside the region, at least `min_size` of the
/// region's area, and of a chosen class unless `all_classes` (origin `filter_detections`).
pub fn filter_detections(found: &[Detection], analysed: [f32; 4], filter: &ObjectFilter<'_>, all_classes: bool) -> Vec<Found> {
    let [x1, y1, x2, y2] = analysed;
    let (aw, ah) = (x2 - x1, y2 - y1);
    let [rx1, ry1, rx2, ry2] = region_box(filter.region, 0.0);
    let least = f32::from(filter.min_size) * (rx2 - rx1) * (ry2 - ry1);
    found
        .iter()
        .filter(|d| all_classes || filter.classes.contains(d.class))
        .filter_map(|d| {
            let [bx1, by1, bx2, by2] = d.area;
            let area = [x1 + bx1 * aw, y1 + by1 * ah, x1 + bx2 * aw, y1 + by2 * ah];
            let big_enough = (area[2] - area[0]) * (area[3] - area[1]) >= least;
            (big_enough && in_region((area[0] + area[2]) / 2.0, area[3], filter.region))
                .then_some(Found { class: d.class, score: d.score, area, verdict: Verdict::Detector, face: None })
        })
        .collect()
}

/// The published state of one class or own label of an object sensor (origin `ObjectTrack`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ObjectTrack {
    pub on: bool,
    /// Kept while on, until the object has cleared.
    pub count: u32,
    /// Best confidence in the last check.
    pub score: f32,
    /// Checks in a row with it present.
    streak: u32,
    /// Checks in a row without it.
    misses: u32,
    last_seen: Option<Instant>,
}

/// Advances every key after one check; returns the keys that switched on or off (origin
/// `update_tracks`). On after `required` checks in a row with it present; off once it was missing
/// `CLEAR_MISSES` checks in a row and for `clear_after`. A box that may be an own label counts
/// neither way for that label. Tracks of keys no longer reported are dropped.
pub fn update_tracks(
    tracks: &mut BTreeMap<ObjectKey, ObjectTrack>,
    found: &[Found],
    keys: &[ObjectKey],
    required: Debounce,
    clear_after: Duration,
    now: Instant,
) -> Vec<ObjectKey> {
    let mut changed = Vec::new();
    for key in keys {
        let track = tracks.entry(*key).or_default();
        let present: Vec<&Found> = found.iter().filter(|f| f.counts_for(*key)).collect();
        track.score = present.iter().map(|f| f.score).fold(0.0, f32::max);
        if !present.is_empty() {
            track.streak += 1;
            track.misses = 0;
            track.last_seen = Some(now);
            if track.on || track.streak >= u32::from(u8::from(required)) {
                if !track.on {
                    changed.push(*key);
                }
                track.on = true;
                track.count = present.len() as u32;
            }
        } else if found.iter().any(|f| f.unsure_for(*key)) {
            track.streak = 0;
        } else {
            track.streak = 0;
            track.misses += 1;
            let long_gone = track.last_seen.is_none_or(|seen| now.duration_since(seen) >= clear_after);
            if track.on && long_gone && track.misses >= CLEAR_MISSES {
                track.on = false;
                track.count = 0;
                changed.push(*key);
            }
        }
    }
    tracks.retain(|key, _| keys.contains(key));
    changed
}

/// Something that was there is missing: look again soon instead of at the next interval.
pub fn recheck_at(tracks: &BTreeMap<ObjectKey, ObjectTrack>, now: Instant) -> Option<Instant> {
    tracks.values().any(|t| t.on && (1..CLEAR_MISSES).contains(&t.misses)).then_some(now + RECHECK)
}

/// The published value: what is on, with its count.
pub fn present(tracks: &BTreeMap<ObjectKey, ObjectTrack>, keys: &[ObjectKey]) -> SensorValue {
    SensorValue::Objects(keys.iter().filter_map(|k| tracks.get(k).filter(|t| t.on).map(|t| (*k, t.count))).collect())
}

/// The own labels to ask about now, each with its most similar unsure box (origin
/// `_ask_unsure`): one question per label per review cooldown, none while review is off.
pub fn questions<'a>(
    rules: &ReviewRules,
    found: &'a [Found],
    asked: &BTreeMap<LabelId, Instant>,
    now: Instant,
) -> Vec<(LabelId, &'a Found)> {
    if !rules.enabled {
        return Vec::new();
    }
    let mut best: BTreeMap<LabelId, (f32, &Found)> = BTreeMap::new();
    for f in found {
        if let Verdict::Unsure { label, similarity } = f.verdict
            && best.get(&label).is_none_or(|(s, _)| similarity > *s)
        {
            best.insert(label, (similarity, f));
        }
    }
    best.into_iter()
        .filter(|(label, _)| asked.get(label).is_none_or(|at| now.duration_since(*at) >= rules.cooldown))
        .map(|(label, (_, f))| (label, f))
        .collect()
}

/// The faces to ask about now, each the most similar box of its person (origin `_ask_unsure` about
/// faces): "É <pessoa>?" for a face that may be them and, with `ask_unknown`, "Quem é?" (`None`)
/// for a usable face like nobody. A person moves, so each question shows another face to teach:
/// up to `FACES.ask_burst` per person per review cooldown, at least `FACES.ask_gap` apart. Only
/// faces looked at in this check are asked about. `asked` holds when each was asked.
pub fn face_questions<'a>(
    rules: &ReviewRules,
    found: &'a [Found],
    asked: &BTreeMap<Option<PersonId>, Vec<Instant>>,
    ask_unknown: bool,
    now: Instant,
) -> Vec<(Option<PersonId>, &'a Found)> {
    if !rules.enabled {
        return Vec::new();
    }
    let mut best: BTreeMap<Option<PersonId>, (f32, &Found)> = BTreeMap::new();
    for f in found.iter().filter(|f| !f.filtered()) {
        let Some(look) = f.face.filter(|l| l.age == 0 && l.usable()) else { continue };
        let about = match look.state {
            FaceState::Ask(person) => Some(person),
            FaceState::Unknown if ask_unknown && look.unknown > 0 => None,
            _ => continue,
        };
        if best.get(&about).is_none_or(|(s, _)| look.similarity > *s) {
            best.insert(about, (look.similarity, f));
        }
    }
    best.into_iter()
        .filter(|(about, _)| {
            let times: Vec<&Instant> = asked.get(about).into_iter().flatten().filter(|at| now.duration_since(**at) < rules.cooldown).collect();
            times.len() < FACES.ask_burst && times.last().is_none_or(|at| now.duration_since(**at) >= FACES.ask_gap)
        })
        .map(|(about, (_, f))| (about, f))
        .collect()
}

/// The classes filtered away in this check, and those of them to keep in the history: a class
/// that starts being filtered, unless it was kept within `FILTERED_RECORD_COOLDOWN` (origin
/// `_record_filtered`).
pub fn filtered_classes(
    found: &[Found],
    before: &BTreeSet<ObjectClass>,
    logged: &BTreeMap<ObjectClass, Instant>,
    now: Instant,
) -> (BTreeSet<ObjectClass>, Vec<ObjectClass>) {
    let now_filtered: BTreeSet<ObjectClass> = found.iter().filter(|f| f.filtered()).map(|f| f.class).collect();
    let record = now_filtered
        .difference(before)
        .filter(|c| logged.get(c).is_none_or(|at| now.duration_since(*at) >= FILTERED_RECORD_COOLDOWN))
        .copied()
        .collect();
    (now_filtered, record)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(k: u8) -> StateKey {
        k.try_into().unwrap()
    }

    fn unit(v: f32) -> Unit {
        v.try_into().unwrap()
    }

    fn debounce(n: u8) -> Debounce {
        n.try_into().unwrap()
    }

    const REGULAR: Triggers = Triggers { regular: true, change_detection: false };
    const TRIGGERS_ONLY: Triggers = Triggers { regular: false, change_detection: false };

    struct Clock(Instant);

    impl Clock {
        fn at(&self, secs: u64) -> Instant {
            self.0 + Duration::from_secs(secs)
        }
    }

    fn schedule(clock: &Clock, last_run: u64) -> Schedule {
        Schedule { last_run: Some(clock.at(last_run)), ..Schedule::default() }
    }

    #[test]
    fn schedule_uses_interval_burst_and_probe() {
        let t = Clock(Instant::now());
        let minute: Interval = 60.try_into().unwrap();
        let next = |s: &Schedule, triggers, now| next_check(s, triggers, minute, false, t.at(now));
        assert_eq!(next(&Schedule::default(), REGULAR, 100), Some((t.at(100), CheckKind::Full)), "never run: now");
        let mut s = schedule(&t, 100);
        assert_eq!(next(&s, REGULAR, 101), Some((t.at(160), CheckKind::Full)));
        s.burst_until = Some(t.at(130));
        assert_eq!(next(&s, REGULAR, 101), Some((t.at(102), CheckKind::Full)));
        assert_eq!(next(&s, REGULAR, 131), Some((t.at(160), CheckKind::Full)), "burst over");
        let probing = Triggers { regular: true, change_detection: true };
        assert_eq!(next(&schedule(&t, 100), probing, 101), Some((t.at(102), CheckKind::Probe)));
        let probed = Schedule { last_probe: Some(t.at(110)), ..schedule(&t, 100) };
        assert_eq!(next(&probed, probing, 111), Some((t.at(112), CheckKind::Probe)));
    }

    fn person(height: f32, face: Option<crate::sensors::types::FaceLook>) -> Found {
        Found { class: ObjectClass::PERSON, score: 0.9, area: [0.4, 0.9 - height, 0.5, 0.9], verdict: Verdict::Detector, face }
    }

    fn look(problem: Option<crate::sensors::types::FaceProblem>, state: FaceState) -> crate::sensors::types::FaceLook {
        let marks = problem.ne(&Some(crate::sensors::types::FaceProblem::Missing)).then_some(crate::sensors::types::FaceMarks {
            area: [0.45, 0.2, 0.5, 0.25],
            points: [[0.46, 0.21]; 5],
            score: 0.9,
        });
        crate::sensors::types::FaceLook { marks, width_px: if marks.is_some() { 60.0 } else { 0.0 }, problem, state, similarity: 0.4, candidate: None, unknown: 0, named: false, age: 0 }
    }

    #[test]
    fn a_face_burst_follows_a_person_walking_up_to_the_door() {
        use crate::sensors::types::FaceProblem::{Missing, Small, Turned};
        let t = Clock(Instant::now());
        let mut s = schedule(&t, 100);
        let minute: Interval = 60.try_into().unwrap();
        assert_eq!(steer_face_burst(&mut s.face_burst, &[], t.at(100)), None, "nobody there");
        let far = [person(0.1, Some(look(Some(Missing), FaceState::Open)))];
        assert_eq!(steer_face_burst(&mut s.face_burst, &far, t.at(100)), Some(BurstChange::Start), "far away, no face yet");
        assert_eq!(next_check(&s, REGULAR, minute, false, t.at(100)), Some((t.at(101), CheckKind::Full)), "a check every second");
        for n in 1..10 {
            let coming = [person(0.1 * 1.15f32.powi(n), Some(look(Some(Small), FaceState::Open)))];
            assert_eq!(steer_face_burst(&mut s.face_burst, &coming, t.at(100 + n as u64)), None, "no face yet, but growing");
        }
        let kari = PersonId(1);
        for n in 10..20 {
            let unsure = [person(0.5, Some(look(None, FaceState::Ask(kari))))];
            assert_eq!(steer_face_burst(&mut s.face_burst, &unsure, t.at(100 + n)), None, "a face it is not sure of");
        }
        let named = [person(0.5, Some(look(None, FaceState::Match { person: kari, sure: true, kept: false })))];
        assert_eq!(steer_face_burst(&mut s.face_burst, &named, t.at(120)), Some(BurstChange::Stop), "named: done");
        s.last_run = Some(t.at(120));
        assert_eq!(next_check(&s, REGULAR, minute, false, t.at(120)), Some((t.at(180), CheckKind::Full)), "the regular interval again");
        let turned = [person(0.5, Some(look(Some(Turned), FaceState::Open)))];
        assert_eq!(steer_face_burst(&mut s.face_burst, &turned, t.at(130)), None, "still there: no second burst");
        for n in 0..FACES.burst_absent_checks {
            assert_eq!(steer_face_burst(&mut s.face_burst, &[], t.at(140 + u64::from(n))), None);
        }
        assert_eq!(steer_face_burst(&mut s.face_burst, &[person(0.2, None)], t.at(150)), Some(BurstChange::Start), "back: a burst of its own");
    }

    #[test]
    fn a_face_burst_ends_without_progress_and_after_its_time() {
        use crate::sensors::types::FaceProblem::Missing;
        let t = Clock(Instant::now());
        let back = [person(0.4, Some(look(Some(Missing), FaceState::Open)))];
        let mut burst = FaceBurst::default();
        assert_eq!(steer_face_burst(&mut burst, &back, t.at(0)), Some(BurstChange::Start));
        let stops: Vec<_> = (1..=FACES.burst_idle_checks).map(|n| steer_face_burst(&mut burst, &back, t.at(u64::from(n)))).collect();
        assert_eq!(stops.last(), Some(&Some(BurstChange::Stop)), "standing with their back to the camera");
        assert!(stops[..stops.len() - 1].iter().all(Option::is_none));
        let unsure = [person(0.4, Some(look(None, FaceState::Unknown)))];
        let mut burst = FaceBurst::default();
        assert_eq!(steer_face_burst(&mut burst, &unsure, t.at(0)), Some(BurstChange::Start));
        let max = FACES.burst_max.as_secs();
        assert_eq!(steer_face_burst(&mut burst, &unsure, t.at(max - 1)), None, "a usable face keeps it going");
        assert_eq!(steer_face_burst(&mut burst, &unsure, t.at(max)), Some(BurstChange::Stop), "but not beyond its time");
        let by = crate::sensors::types::TaughtMatch { sample: crate::sensors::types::SampleId(1), label: crate::sensors::types::BoxLabel::NotThis, similarity: 0.95 };
        let statue = [Found { verdict: Verdict::Filtered(by), ..person(0.4, None) }];
        assert_eq!(steer_face_burst(&mut FaceBurst::default(), &statue, t.at(0)), None, "taught not to be a person");
    }

    #[test]
    fn face_questions_come_in_a_burst_per_person() {
        let t = Clock(Instant::now());
        let kari = PersonId(1);
        let maybe = person(0.5, Some(look(None, FaceState::Ask(kari))));
        let stranger = person(0.5, Some(crate::sensors::types::FaceLook { unknown: 1, ..look(None, FaceState::Unknown) }));
        let found = [maybe, stranger];
        let mut asked: BTreeMap<Option<PersonId>, Vec<Instant>> = BTreeMap::new();
        let about: Vec<Option<PersonId>> = face_questions(&REVIEW, &found, &asked, false, t.at(0)).into_iter().map(|q| q.0).collect();
        assert_eq!(about, [Some(kari)], "nobody is asked about a stranger unless the sensor says so");
        assert_eq!(face_questions(&REVIEW, &found, &asked, true, t.at(0)).len(), 2);
        let untaught = person(0.5, Some(look(None, FaceState::Unknown)));
        assert!(face_questions(&REVIEW, &[untaught], &asked, true, t.at(0)).is_empty(), "with nobody taught there is no unknown to ask about");
        asked.insert(Some(kari), vec![t.at(0)]);
        assert!(face_questions(&REVIEW, &found[..1], &asked, false, t.at(1)).is_empty(), "the next one waits ask_gap");
        assert_eq!(face_questions(&REVIEW, &found[..1], &asked, false, t.at(3)).len(), 1);
        asked.insert(Some(kari), (0..FACES.ask_burst as u64).map(|n| t.at(n * 3)).collect());
        assert!(face_questions(&REVIEW, &found[..1], &asked, false, t.at(30)).is_empty(), "a burst per review cooldown");
        assert_eq!(face_questions(&REVIEW, &found[..1], &asked, false, t.at(400)).len(), 1);
        let carried = person(0.5, Some(crate::sensors::types::FaceLook { age: 1, ..look(None, FaceState::Ask(kari)) }));
        assert!(face_questions(&REVIEW, &[carried], &BTreeMap::new(), false, t.at(0)).is_empty(), "only a face looked at now");
    }

    #[test]
    fn schedule_without_regular_check() {
        let t = Clock(Instant::now());
        let minute: Interval = 60.try_into().unwrap();
        let next = |s: &Schedule, triggers, now| next_check(s, triggers, minute, false, t.at(now));
        let mut s = schedule(&t, 100);
        assert_eq!(next(&s, TRIGGERS_ONLY, 101), None, "nothing until a trigger");
        s.burst_until = Some(t.at(130));
        assert_eq!(next(&s, TRIGGERS_ONLY, 101), Some((t.at(102), CheckKind::Full)), "a burst still runs");
        let probing = Triggers { regular: false, change_detection: true };
        assert_eq!(next(&schedule(&t, 100), probing, 101), Some((t.at(102), CheckKind::Probe)));
        assert_eq!(next(&schedule(&t, 500), REGULAR, 501), Some((t.at(560), CheckKind::Full)));
    }

    #[test]
    fn recheck_comes_before_the_regular_check() {
        let t = Clock(Instant::now());
        let minute: Interval = 60.try_into().unwrap();
        let s = Schedule { recheck_at: Some(t.at(105)), ..schedule(&t, 100) };
        for triggers in [REGULAR, TRIGGERS_ONLY] {
            assert_eq!(next_check(&s, triggers, minute, false, t.at(101)), Some((t.at(105), CheckKind::Full)));
        }
    }

    #[test]
    fn pause_stops_everything_but_check_now() {
        let t = Clock(Instant::now());
        let minute: Interval = 60.try_into().unwrap();
        let mut s = Schedule { burst_until: Some(t.at(130)), ..schedule(&t, 100) };
        assert_eq!(next_check(&s, REGULAR, minute, true, t.at(101)), None);
        s.force = true;
        assert_eq!(next_check(&s, REGULAR, minute, true, t.at(101)), Some((t.at(101), CheckKind::Full)));
    }

    #[test]
    fn a_change_starts_a_burst_from_now() {
        let t = Clock(Instant::now());
        let mut s = schedule(&t, 100);
        start_burst(&mut s, t.at(140));
        assert_eq!(s.burst_until, Some(t.at(140) + BURST_DURATION));
    }

    fn signature(fill: impl Fn(usize) -> f32) -> Signature {
        Signature(Box::new(std::array::from_fn(fill)))
    }

    #[test]
    fn change_needs_four_percent_mean_difference() {
        let black = signature(|_| 0.0);
        let half = signature(|i| if i < SIGNATURE_SIZE * SIGNATURE_SIZE / 2 { 1.0 } else { 0.0 });
        assert_eq!(change_score(&black, &black), 0.0);
        assert_eq!(change_score(&black, &half), 0.5);
        assert!(!changed(&black, &signature(|_| 0.039)));
        assert!(changed(&black, &signature(|_| 0.041)));
    }

    #[test]
    fn verdict_below_threshold_is_unknown() {
        let probs = [(key(1), 0.2), (key(2), 0.8)];
        assert_eq!(state_verdict(&probs, unit(0.7)), (SensorValue::State(key(2)), Some((key(2), 0.8))));
        assert_eq!(state_verdict(&probs, unit(0.8)).0, SensorValue::State(key(2)), "the threshold itself passes");
        assert_eq!(state_verdict(&probs, unit(0.9)), (SensorValue::Unknown, Some((key(2), 0.8))));
        assert_eq!(state_verdict(&[], unit(0.0)), (SensorValue::Unknown, None));
    }

    #[test]
    fn debouncer_publishes_first_value_immediately_then_waits() {
        let mut d = Debouncer::default();
        assert!(d.update("closed", debounce(2)));
        assert_eq!(d.published(), Some(&"closed"));
        assert!(!d.update("open", debounce(2)), "one result is not enough");
        assert_eq!(d.published(), Some(&"closed"));
        assert!(d.update("open", debounce(2)));
        assert_eq!(d.published(), Some(&"open"));
        assert!(!d.update("open", debounce(2)), "no change, no publish");
    }

    #[test]
    fn debouncer_resets_on_flicker() {
        let mut d = Debouncer::default();
        for value in ["closed", "open", "closed", "open"] {
            d.update(value, debounce(3));
        }
        assert_eq!(d.published(), Some(&"closed"));
    }

    #[test]
    fn review_reason_priorities() {
        let rules = ReviewRules { spot_rate: 0.05, ..REVIEW };
        let long_ago = Some(Duration::from_secs(10_000));
        let t = unit(0.7);
        assert_eq!(review_reason(&rules, 0.5, t, 0, long_ago, 0.5), Some(ReviewReason::LowConfidence));
        assert_eq!(review_reason(&rules, 0.99, t, REVIEW.flip_limit, long_ago, 0.5), Some(ReviewReason::Flip));
        assert_eq!(review_reason(&rules, 0.99, t, 0, long_ago, 0.0), Some(ReviewReason::SpotCheck));
        assert_eq!(review_reason(&rules, 0.99, t, 0, long_ago, 0.9), None);
        assert_eq!(review_reason(&rules, 0.1, t, 0, Some(Duration::from_secs(1)), 0.0), None, "cooldown");
        assert_eq!(review_reason(&rules, 0.1, t, 0, None, 0.5), Some(ReviewReason::LowConfidence), "never flagged");
        assert_eq!(review_reason(&REVIEW, 0.99, t, 0, None, 0.0), None, "no spot checks by default");
    }

    #[test]
    fn review_limit_never_drops_below_the_threshold() {
        let long_ago = Some(Duration::from_secs(10_000));
        let t = unit(0.7);
        assert_eq!(review_reason(&ReviewRules { below: 0.8, ..REVIEW }, 0.83, t, 0, long_ago, 0.5), None);
        let low = ReviewRules { below: 0.3, ..REVIEW };
        assert_eq!(review_reason(&low, 0.65, t, 0, long_ago, 0.5), Some(ReviewReason::LowConfidence));
        let off = ReviewRules { enabled: false, ..REVIEW };
        assert_eq!(review_reason(&off, 0.1, t, 0, long_ago, 0.0), None);
    }

    #[test]
    fn recent_changes_counts_the_flip_window() {
        let t = Clock(Instant::now());
        let mut changes: VecDeque<Instant> = [0, 100, 700].map(|s| t.at(s)).into();
        assert_eq!(recent_changes(&REVIEW, &mut changes, t.at(700)), 2, "0 s is 700 s ago, over the 600 s window");
        assert_eq!(recent_changes(&REVIEW, &mut changes, t.at(1300)), 1, "a change exactly 600 s ago still counts");
    }

    #[test]
    fn geometry_is_adopted_once_then_compared() {
        let sub = Geometry { width: 704, height: 480 };
        let main = Geometry { width: 1920, height: 1080 };
        assert_eq!(geometry_verdict(None, sub), GeometryVerdict::Adopt);
        assert_eq!(geometry_verdict(Some(sub), sub), GeometryVerdict::Same);
        assert_eq!(geometry_verdict(Some(sub), main), GeometryVerdict::Changed);
    }

    fn facts() -> StatusFacts {
        StatusFacts {
            paused: false,
            geometry: None,
            feed: FeedFact::Up,
            model: ModelState::Ready,
            failure: None,
            trained: true,
        }
    }

    #[test]
    fn status_precedence() {
        let sub = Geometry { width: 704, height: 480 };
        let main = Geometry { width: 1920, height: 1080 };
        let down = || FeedFact::Down { reason: "401".into(), retry_in: None };
        assert_eq!(sensor_status(facts()), SensorStatus::Live);
        assert_eq!(sensor_status(StatusFacts { paused: true, feed: down(), ..facts() }), SensorStatus::Paused);
        assert_eq!(
            sensor_status(StatusFacts { geometry: Some((sub, main)), feed: down(), ..facts() }),
            SensorStatus::GeometryChanged { expected: sub, seen: main }
        );
        assert_eq!(
            sensor_status(StatusFacts { feed: FeedFact::Waiting, trained: false, ..facts() }),
            SensorStatus::WaitingSource
        );
        assert_eq!(
            sensor_status(StatusFacts { feed: down(), model: ModelState::Failed("x".into()), ..facts() }),
            SensorStatus::SourceDown { reason: "401".into(), retry_in: None }
        );
        assert_eq!(
            sensor_status(StatusFacts {
                model: ModelState::Downloading { done: 1, total: 4 },
                trained: false,
                ..facts()
            }),
            SensorStatus::WaitingModel { progress: Some(0.25), failed: None }
        );
        assert_eq!(
            sensor_status(StatusFacts { failure: Some("ort".into()), trained: false, ..facts() }),
            SensorStatus::Failed("ort".into())
        );
        assert_eq!(sensor_status(StatusFacts { trained: false, ..facts() }), SensorStatus::Untrained);
    }

    fn row(id: i64, age_days: u64, bytes: u64, pending_review: bool) -> RowCost {
        RowCost { id: HistoryId(id), age: Duration::from_secs(age_days * 86_400), bytes, pending_review }
    }

    #[test]
    fn retention_defaults_are_seven_days_and_two_gigabytes() {
        let policy = Retention::default();
        assert_eq!(policy.max_age, Duration::from_secs(7 * 86_400));
        assert_eq!(policy.max_bytes, 2 << 30);
    }

    #[test]
    fn retention_drops_old_rows_but_keeps_pending_reviews_twice_as_long() {
        let rows = [row(1, 15, 1, true), row(2, 10, 1, true), row(3, 8, 1, false), row(4, 1, 1, false)];
        assert_eq!(retention_plan(&rows, Retention::default()), [1, 3].map(HistoryId));
    }

    #[test]
    fn retention_trims_by_size_oldest_first_and_pending_reviews_last() {
        let policy = Retention { max_age: Duration::from_secs(7 * 86_400), max_bytes: 12 };
        let rows = [row(1, 3, 4, true), row(2, 2, 4, false), row(3, 1, 4, false), row(4, 0, 4, true)];
        assert_eq!(retention_plan(&rows, policy), [2].map(HistoryId), "16 bytes, 12 after the oldest reviewed");
        assert_eq!(retention_plan(&rows, Retention { max_bytes: 10, ..policy }), [2, 3].map(HistoryId));
        assert_eq!(retention_plan(&rows, Retention { max_bytes: 5, ..policy }), [2, 3, 1].map(HistoryId));
        assert!(retention_plan(&rows, Retention { max_bytes: 16, ..policy }).is_empty());
    }

    fn class(name: &str) -> ObjectClass {
        ObjectClass::by_key(name).unwrap()
    }

    fn point(x: f32, y: f32) -> crate::sensors::types::NormPoint {
        crate::sensors::types::NormPoint::new(x, y).unwrap()
    }

    fn classes(keys: &[&str]) -> ClassSet {
        keys.iter().map(|k| class(k)).collect::<Vec<_>>().try_into().unwrap()
    }

    fn detection(key: &str, area: [f32; 4]) -> Detection {
        Detection { class: class(key), score: 0.9, area }
    }

    fn close(got: [f32; 4], want: [f32; 4]) -> bool {
        got.iter().zip(want).all(|(g, w)| (g - w).abs() < 1e-5)
    }

    #[test]
    fn filter_maps_boxes_from_the_analysed_crop_to_the_frame() {
        let region = Region::rect(point(0.5, 0.5), point(1.0, 1.0)).unwrap();
        let analysed = region_box(&region, CONTEXT_MARGIN);
        assert!(close(analysed, [0.375, 0.375, 1.0, 1.0]), "grown by 0.125 on each side, clamped: {analysed:?}");
        let person = classes(&["person"]);
        let filter = ObjectFilter { region: &region, classes: &person, min_size: 0.0.try_into().unwrap() };
        let found = filter_detections(&[detection("person", [0.2, 0.2, 0.6, 1.0])], analysed, &filter, false);
        assert_eq!(found.len(), 1);
        assert!(close(found[0].area, [0.5, 0.5, 0.75, 1.0]), "{:?}", found[0].area);
    }

    #[test]
    fn filter_ignores_objects_standing_in_the_margin() {
        let region = Region::rect(point(0.4, 0.0), point(0.6, 1.0)).unwrap();
        let analysed = region_box(&region, CONTEXT_MARGIN);
        let dog = classes(&["dog"]);
        let filter = ObjectFilter { region: &region, classes: &dog, min_size: 0.0.try_into().unwrap() };
        let beside = detection("dog", [0.8, 0.5, 1.0, 0.9]);
        assert!(filter_detections(&[beside], analysed, &filter, false).is_empty(), "feet at x = 0.62: in the margin only");
    }

    #[test]
    fn filter_uses_the_bottom_centre_the_class_and_the_size() {
        let triangle = Region::polygon(vec![point(0.0, 0.0), point(0.0, 1.0), point(1.0, 1.0)]).unwrap();
        let inside = detection("person", [0.05, 0.3, 0.25, 0.9]);
        let head_in_feet_out = detection("person", [0.6, 0.65, 0.8, 0.7]);
        let tiny = detection("person", [0.1, 0.94, 0.12, 0.96]);
        let dog = detection("dog", [0.05, 0.3, 0.25, 0.9]);
        let person = classes(&["person"]);
        let filter = ObjectFilter { region: &triangle, classes: &person, min_size: 0.01.try_into().unwrap() };
        let whole = [0.0, 0.0, 1.0, 1.0];
        let kept = filter_detections(&[inside, head_in_feet_out, tiny, dog], whole, &filter, false);
        assert_eq!(kept.iter().map(|f| f.area[0]).collect::<Vec<_>>(), [0.05]);
        assert_eq!(filter_detections(&[inside, dog], whole, &filter, true).len(), 2, "every class when rescuing");
    }

    #[test]
    fn in_region_of_a_polygon() {
        let region = Region::polygon(vec![point(0.1, 0.1), point(0.9, 0.1), point(0.5, 0.9)]).unwrap();
        assert!(in_region(0.5, 0.3, &region));
        assert!(!in_region(0.15, 0.8, &region));
        assert!(in_region(0.99, 0.99, &Region::WHOLE));
    }

    fn seen(n: usize, key: &str) -> Vec<Found> {
        vec![Found { class: class(key), score: 0.9, area: [0.0, 0.0, 0.1, 0.1], verdict: Verdict::Detector, face: None }; n]
    }

    fn keys(names: &[&str]) -> Vec<ObjectKey> {
        names.iter().map(|k| ObjectKey::Class(class(k))).collect()
    }

    #[test]
    fn tracks_switch_on_after_required_checks_and_hold_until_cleared() {
        let t = Clock(Instant::now());
        let mut tracks = BTreeMap::new();
        let both = keys(&["person", "car"]);
        let person = ObjectKey::Class(class("person"));
        let mut update = |found: Vec<Found>, at: u64| update_tracks(&mut tracks, &found, &both, debounce(2), Duration::from_secs(30), t.at(at));
        assert!(update(seen(2, "person"), 0).is_empty());
        assert_eq!(update(seen(2, "person"), 5), [person]);
        assert!(update(vec![], 20).is_empty(), "gone for less than clear_after");
        update(seen(1, "person"), 25);
        assert!(update(vec![], 50).is_empty());
        assert_eq!(update(vec![], 56), [person], "gone long enough and missed twice");
        assert_eq!((tracks[&person].on, tracks[&person].count), (false, 0));
    }

    #[test]
    fn the_count_is_published_while_on() {
        let t = Clock(Instant::now());
        let mut tracks = BTreeMap::new();
        let person = keys(&["person"]);
        update_tracks(&mut tracks, &seen(2, "person"), &person, debounce(1), Duration::from_secs(30), t.at(0));
        assert_eq!(present(&tracks, &person), SensorValue::Objects(vec![(person[0], 2)]));
        update_tracks(&mut tracks, &seen(3, "person"), &person, debounce(1), Duration::from_secs(30), t.at(1));
        assert_eq!(present(&tracks, &person), SensorValue::Objects(vec![(person[0], 3)]));
        update_tracks(&mut tracks, &[], &person, debounce(1), Duration::from_secs(30), t.at(2));
        assert_eq!(present(&tracks, &person), SensorValue::Objects(vec![(person[0], 3)]), "the last count holds until it clears");
    }

    #[test]
    fn one_missed_check_does_not_switch_an_object_off_and_is_looked_at_again_soon() {
        let t = Clock(Instant::now());
        let mut tracks = BTreeMap::new();
        let person = keys(&["person"]);
        update_tracks(&mut tracks, &seen(1, "person"), &person, debounce(1), Duration::from_secs(30), t.at(0));
        assert_eq!(recheck_at(&tracks, t.at(0)), None);
        assert!(update_tracks(&mut tracks, &[], &person, debounce(1), Duration::from_secs(30), t.at(60)).is_empty(), "long gone, but missed once");
        assert!(tracks[&person[0]].on);
        assert_eq!(recheck_at(&tracks, t.at(60)), Some(t.at(60) + RECHECK));
        assert_eq!(update_tracks(&mut tracks, &[], &person, debounce(1), Duration::from_secs(30), t.at(65)), person, "confirmed");
        assert_eq!(recheck_at(&tracks, t.at(65)), None, "off: nothing to look for");
    }

    #[test]
    fn an_unsure_box_keeps_an_own_label_as_it_was() {
        let t = Clock(Instant::now());
        let ours = LabelId(7);
        let keys = [ObjectKey::Class(class("car")), ObjectKey::Own(ours)];
        let car = seen(1, "car")[0];
        let by = crate::sensors::types::TaughtMatch { sample: crate::sensors::types::SampleId(1), label: crate::sensors::types::BoxLabel::Own(ours), similarity: 0.95 };
        let mine = [Found { verdict: Verdict::Taught { by, label: Some(ours), was: None, how: crate::sensors::types::MatchHow::Clear }, ..car }];
        let unsure = [Found { verdict: Verdict::Unsure { label: ours, similarity: 0.8 }, ..car }];
        let mut tracks = BTreeMap::new();
        update_tracks(&mut tracks, &mine, &keys, debounce(1), Duration::ZERO, t.at(0));
        for at in [10, 20, 30] {
            assert!(update_tracks(&mut tracks, &unsure, &keys, debounce(1), Duration::ZERO, t.at(at)).is_empty());
        }
        assert!(tracks[&keys[1]].on && tracks[&keys[1]].misses == 0);
        let mut fresh = BTreeMap::new();
        update_tracks(&mut fresh, &unsure, &keys, debounce(1), Duration::ZERO, t.at(0));
        assert!(!fresh[&keys[1]].on && fresh[&keys[0]].on, "a car, but not yet ours");
    }

    #[test]
    fn tracks_skip_filtered_boxes_count_own_labels_and_forget_deselected_keys() {
        let t = Clock(Instant::now());
        let ours = LabelId(7);
        let car = seen(1, "car")[0];
        let by = crate::sensors::types::TaughtMatch { sample: crate::sensors::types::SampleId(1), label: crate::sensors::types::BoxLabel::NotThis, similarity: 0.95 };
        let found = [
            Found { verdict: Verdict::Filtered(by), ..seen(1, "person")[0] },
            Found { verdict: Verdict::Taught { by, label: Some(ours), was: None, how: crate::sensors::types::MatchHow::Clear }, ..car },
            car,
        ];
        let all = [ObjectKey::Class(class("person")), ObjectKey::Class(class("car")), ObjectKey::Own(ours)];
        let mut tracks = BTreeMap::new();
        update_tracks(&mut tracks, &found, &all, debounce(1), Duration::ZERO, t.at(1));
        assert!(!tracks[&all[0]].on);
        assert_eq!((tracks[&all[1]].count, tracks[&all[2]].count), (2, 1));
        update_tracks(&mut tracks, &[], &all[1..2], debounce(1), Duration::ZERO, t.at(2));
        assert_eq!(tracks.keys().copied().collect::<Vec<_>>(), [all[1]]);
    }

    #[test]
    fn questions_come_once_per_label_per_cooldown() {
        let t = Clock(Instant::now());
        let ours = LabelId(7);
        let car = seen(1, "car")[0];
        let found = [Found { verdict: Verdict::Unsure { label: ours, similarity: 0.78 }, ..car }, Found { verdict: Verdict::Unsure { label: ours, similarity: 0.8 }, score: 0.5, ..car }];
        let asked = questions(&REVIEW, &found, &BTreeMap::new(), t.at(0));
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].1.score, 0.5, "the most similar box is asked about");
        let before = BTreeMap::from([(ours, t.at(0))]);
        assert!(questions(&REVIEW, &found, &before, t.at(299)).is_empty(), "within the review cooldown");
        assert_eq!(questions(&REVIEW, &found, &before, t.at(300)).len(), 1);
        assert!(questions(&ReviewRules { enabled: false, ..REVIEW }, &found, &BTreeMap::new(), t.at(0)).is_empty());
    }

    #[test]
    fn a_class_filtered_again_soon_is_not_kept_twice() {
        let t = Clock(Instant::now());
        let by = crate::sensors::types::TaughtMatch { sample: crate::sensors::types::SampleId(1), label: crate::sensors::types::BoxLabel::NotThis, similarity: 0.95 };
        let statue = [Found { verdict: Verdict::Filtered(by), ..seen(1, "person")[0] }];
        let person = class("person");
        let (now, record) = filtered_classes(&statue, &BTreeSet::new(), &BTreeMap::new(), t.at(0));
        assert_eq!((now.clone(), record), (BTreeSet::from([person]), vec![person]));
        let (_, again) = filtered_classes(&statue, &now, &BTreeMap::from([(person, t.at(0))]), t.at(10));
        assert!(again.is_empty(), "still filtered: not a new start");
        let logged = BTreeMap::from([(person, t.at(0))]);
        assert!(filtered_classes(&statue, &BTreeSet::new(), &logged, t.at(599)).1.is_empty(), "within the cooldown");
        assert_eq!(filtered_classes(&statue, &BTreeSet::new(), &logged, t.at(600)).1, [person]);
    }
}
