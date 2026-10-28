//! Pure decisions of the engine, ported from the origin (`engine/logic.py`, `engine/state.py`,
//! `engine/history.py`). No I/O, no clock (time comes in as arguments), no randomness (the roll
//! comes in). Constants are copied from `settings.py`, with the origin's name beside each.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::sensors::types::{
    Debounce, Geometry, HistoryId, Interval, ModelState, Retention, ReviewReason, SensorStatus, SensorValue, StateKey,
    Triggers, Unit,
};

pub const BURST_INTERVAL: Duration = Duration::from_secs(2); // TRIGGER_DEFAULTS["burst_interval_s"]
pub const BURST_DURATION: Duration = Duration::from_secs(30); // TRIGGER_DEFAULTS["burst_duration_s"]
pub const CHANGE_INTERVAL: Duration = Duration::from_secs(2); // TRIGGER_DEFAULTS["change_interval_s"]
pub const CHANGE_THRESHOLD: f32 = 0.04; // TRIGGER_DEFAULTS["change_threshold"]
pub const SIGNATURE_SIZE: usize = 48; // CHANGE_SIGNATURE_SIZE
pub const NEUTRAL_FILL: [u8; 3] = [124, 116, 104]; // NEUTRAL_FILL

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
    let mut full_at = if s.burst_until.is_some_and(|until| now < until) {
        Some(last + BURST_INTERVAL)
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
}
