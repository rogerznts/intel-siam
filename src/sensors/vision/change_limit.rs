//! How much a counter may go up since its last accepted reading (origin `change_limit.py`).
//!
//! The limit grows with the time the counter was not seen: the app down, the camera away, or the
//! readings unsure. Time in which the counter was read with confidence but went over the limit does
//! not count, so a wheel misread over and over never waits its way through. Such readings are never
//! accepted by themselves, however many agree: they wait for an answer in review.
//!
//! "auto" learns the limit from the counter's own accepted readings: the largest rise over a few
//! minutes per day and the rise per day (the stored days' `peak` and `total`), only over stretches
//! in which it was read all the time; until then a rate (set, or the standard one) applies. Pure:
//! times are seconds since the epoch, passed in. The engine keeps a `Watch` per counter and stores it.

use std::collections::VecDeque;

use crate::sensors::types::{ChangeLimit, LearnedLimit};

pub struct CounterLimit {
    /// "auto": the largest rise over this window per day, and the day's total rise, of the last
    /// `learn_days` days; the second largest of them, so one misread that slipped through does
    /// not count. Learned after `min_days` days with readings.
    pub window_s: f64,
    pub learn_days: u32,
    pub min_days: usize,
    /// "auto": the learned rate and day times this.
    pub safety: f64,
    /// "auto" until it learned, and what the panel fills in for "fixed" and "step": this many steps
    /// of the counter's last digit a minute.
    pub standard_steps_per_min: f64,
    /// Always allowed: this many steps of the last digit (a wheel half way).
    pub margin_steps: f64,
    /// A gap between two readings longer than `gap_checks` intervals (at least `gap_min_s`) is time
    /// the counter was not seen.
    pub gap_checks: f64,
    pub gap_min_s: f64,
    /// How often the learned limit is worked out again.
    pub relearn_s: f64,
}

pub const COUNTER_LIMIT: CounterLimit = CounterLimit {
    window_s: 600.0,               // COUNTER_LIMIT["window_s"]
    learn_days: 14,                // COUNTER_LIMIT["learn_days"]
    min_days: 7,                   // COUNTER_LIMIT["min_days"]
    safety: 2.0,                   // COUNTER_LIMIT["safety"]
    standard_steps_per_min: 40.0,  // COUNTER_LIMIT["standard_steps_per_min"]
    margin_steps: 2.0,             // COUNTER_LIMIT["margin_steps"]
    gap_checks: 3.0,               // COUNTER_LIMIT["gap_checks"]
    gap_min_s: 300.0,              // COUNTER_LIMIT["gap_min_s"]
    relearn_s: 3600.0,             // COUNTER_LIMIT["relearn_s"]
};

/// A counter's limit between readings. The first four fields are stored with the published value.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Watch {
    /// The last reading, of any kind.
    pub seen_at: Option<f64>,
    /// The last accepted reading.
    pub accepted_at: Option<f64>,
    /// Time since then that counts towards the limit.
    pub budget_s: f64,
    /// Read with confidence over the limit since then: its time no longer counts.
    pub held: bool,
    /// Learning: accepted readings over the last window (time, value), and the newest one, which
    /// counts once the next one says it was no misread (see `observe`).
    pub window: VecDeque<(f64, f64)>,
    pub sample: Option<(f64, f64)>,
    pub learned: Option<LearnedLimit>,
    /// Days with readings so far.
    pub days: u32,
    pub learned_at: Option<f64>,
    pub learned_day: String,
}

/// A pause between two readings longer than this is time the counter was not seen.
pub fn gap(interval_s: f64) -> f64 {
    (COUNTER_LIMIT.gap_checks * interval_s).max(COUNTER_LIMIT.gap_min_s)
}

fn step(decimals: u8) -> f64 {
    10f64.powi(-i32::from(decimals))
}

pub fn margin(decimals: u8) -> f64 {
    COUNTER_LIMIT.margin_steps * step(decimals)
}

/// `standard_steps_per_min` steps of the last digit a minute, per hour.
pub fn standard_rate(decimals: u8) -> f64 {
    COUNTER_LIMIT.standard_steps_per_min * step(decimals) * 60.0
}

/// The largest rise allowed after `seconds` of counting time, or `None` (no limit).
pub fn limit(mode: ChangeLimit, decimals: u8, learned: Option<&LearnedLimit>, seconds: f64) -> Option<f64> {
    let hours = seconds.max(0.0) / 3600.0;
    match (mode, learned) {
        (ChangeLimit::Step { max_step }, _) => Some(f64::from(max_step)).filter(|s| *s > 0.0),
        (ChangeLimit::Fixed { max_rate }, _) => {
            let rate = f64::from(max_rate);
            (rate > 0.0).then(|| margin(decimals) + rate * hours)
        }
        (ChangeLimit::Auto { .. }, Some(learned)) => {
            let days = (seconds / 86_400.0).ceil().max(1.0);
            let rise = (learned.per_hour * hours).min(learned.per_day * days);
            Some(margin(decimals) + COUNTER_LIMIT.safety * rise)
        }
        (ChangeLimit::Auto { max_rate }, None) => {
            let rate = Some(f64::from(max_rate)).filter(|r| *r > 0.0).unwrap_or_else(|| standard_rate(decimals));
            Some(margin(decimals) + rate * hours)
        }
        (ChangeLimit::Off, _) => None,
    }
}

/// The limit learned from (largest rise over the window, total rise) of each day with readings.
pub fn learn(days: &[(f64, f64)]) -> Option<LearnedLimit> {
    if days.len() < COUNTER_LIMIT.min_days {
        return None;
    }
    let sorted = |mut v: Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v
    };
    let peaks = sorted(days.iter().map(|d| d.0).collect());
    let totals = sorted(days.iter().map(|d| d.1).collect());
    let second = |v: &[f64]| v[v.len() - 2];
    let per_hour = second(&peaks) / (COUNTER_LIMIT.window_s / 3600.0);
    (per_hour > 0.0).then(|| LearnedLimit {
        per_hour,
        per_day: second(&totals).max(second(&peaks)),
        days: days.len() as u32,
    })
}

/// Before a reading is checked: the time since the last one counts towards the limit. Once the
/// counter was read with confidence over the limit, only the part of a pause longer than a normal
/// one counts (the app was down).
pub fn advance(watch: &mut Watch, now: f64, interval_s: f64) {
    if let Some(seen) = watch.seen_at {
        let passed = (now - seen).max(0.0);
        watch.budget_s += if watch.held { (passed - gap(interval_s)).max(0.0) } else { passed };
    }
    watch.seen_at = Some(now);
}

/// A reading confirmed the published value (or made it): the limit starts again.
pub fn accept(watch: &mut Watch, now: f64) {
    watch.accepted_at = Some(now);
    watch.budget_s = 0.0;
    watch.held = false;
}

/// A reading read with confidence but over the limit: from now on, normal pauses no longer count
/// towards the limit, so it does not grow until the counter was away.
pub fn hold(watch: &mut Watch) {
    watch.held = true;
}

/// An accepted reading, for learning. Returns what the one before it adds to the day: the rise
/// over the window up to it and the rise since the one before. A reading counts with the lower of
/// its value and the next one's, so one read too high adds nothing. A pause longer than a normal
/// one (down, unsure, rejected) starts again: a rise over a pause says nothing about how fast the
/// counter goes up.
pub fn observe(watch: &mut Watch, now: f64, value: f64, interval_s: f64) -> Option<(f64, f64)> {
    let before = watch.sample.replace((now, value));
    let Some((at, earlier)) = before.filter(|(at, _)| now - at <= gap(interval_s)) else {
        watch.window.clear();
        return None;
    };
    let earlier = earlier.min(value);
    let Some(&(_, last)) = watch.window.back() else {
        watch.window.push_back((at, earlier));
        return Some((0.0, 0.0));
    };
    let cut = at - COUNTER_LIMIT.window_s;
    let mut base = watch.window[0].1;
    for (t, v) in &watch.window {
        if *t > cut {
            break;
        }
        base = *v;
    }
    let rise = earlier - last;
    watch.window.push_back((at, earlier.max(last)));
    while watch.window.len() >= 2 && watch.window[1].0 <= cut {
        watch.window.pop_front();
    }
    Some(((earlier - base).max(0.0), rise.max(0.0)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sensors::types::Amount;

    const WATER: u8 = 3;
    const MARGIN: f64 = 2.0 * 0.001;
    const BUSY: LearnedLimit = LearnedLimit { per_hour: 0.55, per_day: 0.8, days: 9 };

    fn amount(v: f64) -> Amount {
        v.try_into().unwrap()
    }

    fn close(got: Option<f64>, want: f64) {
        let got = got.expect("a limit");
        assert!((got - want).abs() < 1e-9, "{got} != {want}");
    }

    fn auto(rate: f64) -> ChangeLimit {
        ChangeLimit::Auto { max_rate: amount(rate) }
    }

    #[test]
    fn limit_grows_with_time() {
        let fixed = ChangeLimit::Fixed { max_rate: amount(2.4) };
        close(limit(fixed, WATER, None, 60.0), MARGIN + 0.04);
        close(limit(fixed, WATER, None, 35.0 * 60.0), MARGIN + 1.4);
        assert_eq!(limit(ChangeLimit::Fixed { max_rate: amount(0.0) }, WATER, None, 60.0), None);
        assert_eq!(limit(ChangeLimit::Off, WATER, Some(&BUSY), 60.0), None);
        close(limit(auto(0.0), WATER, None, 60.0), MARGIN + 0.04);
        close(limit(auto(1.2), WATER, None, 60.0), MARGIN + 0.02);
        assert!((standard_rate(1) - 240.0).abs() < 1e-9);
        let step = ChangeLimit::Step { max_step: amount(0.04) };
        assert_eq!(limit(step, WATER, Some(&BUSY), 60.0), Some(0.04));
        assert_eq!(limit(step, WATER, Some(&BUSY), 86_400.0), Some(0.04), "however long the counter was away");
        assert_eq!(limit(ChangeLimit::Step { max_step: amount(0.0) }, WATER, Some(&BUSY), 60.0), None);
        close(limit(auto(0.0), WATER, Some(&BUSY), 60.0), MARGIN + COUNTER_LIMIT.safety * 0.55 / 60.0);
        close(
            limit(auto(0.0), WATER, Some(&BUSY), 3.0 * 86_400.0),
            MARGIN + COUNTER_LIMIT.safety * 3.0 * 0.8,
        );
    }

    #[test]
    fn learn_needs_days_and_ignores_the_busiest_one() {
        let week = vec![(0.05, 0.5); COUNTER_LIMIT.min_days - 1];
        assert_eq!(learn(&week), None);
        let mut days = week.clone();
        days.extend([(0.09, 0.7), (4.0, 9.0)]);
        let learned = learn(&days).expect("enough days");
        assert_eq!(learned.days as usize, COUNTER_LIMIT.min_days + 1);
        assert!((learned.per_hour - 0.09 * 3600.0 / COUNTER_LIMIT.window_s).abs() < 1e-9, "one day with a misread does not count");
        assert!((learned.per_day - 0.7).abs() < 1e-9);
        assert_eq!(learn(&[(0.0, 0.0); 10]), None, "nothing used: nothing to learn");
    }

    #[test]
    fn only_unseen_time_counts() {
        let mut watch = Watch { seen_at: Some(0.0), accepted_at: Some(0.0), ..Watch::default() };
        advance(&mut watch, 60.0, 60.0);
        assert_eq!(watch.budget_s, 60.0);
        watch.held = true;
        for minute in 2..30 {
            advance(&mut watch, f64::from(minute) * 60.0, 60.0);
        }
        assert_eq!(watch.budget_s, 60.0, "read with confidence over the limit: the next minutes add nothing");
        advance(&mut watch, 29.0 * 60.0 + 3600.0, 60.0);
        assert!((watch.budget_s - (60.0 + 3600.0 - gap(60.0))).abs() < 1e-9, "an hour down counts beyond a normal pause");
    }

    #[test]
    fn learns_the_rise_over_the_window() {
        let mut watch = Watch::default();
        assert_eq!(observe(&mut watch, 0.0, 100.0, 60.0), None, "the first one: only the start");
        let mut rise = None;
        for minute in 1..=20 {
            rise = observe(&mut watch, f64::from(minute) * 60.0, 100.0 + 0.01 * f64::from(minute), 60.0);
            let (_, day) = rise.unwrap();
            assert!((day - if minute > 1 { 0.01 } else { 0.0 }).abs() < 1e-9);
        }
        assert!((rise.unwrap().0 - 0.01 * COUNTER_LIMIT.window_s / 60.0).abs() < 1e-9);
        let (peak, day) = observe(&mut watch, 21.0 * 60.0, 101.2, 60.0).unwrap();
        assert!((peak - 0.1).abs() < 1e-9 && (day - 0.01).abs() < 1e-9, "a reading too high counts with the next one's value");
        let (peak, day) = observe(&mut watch, 22.0 * 60.0, 100.22, 60.0).unwrap();
        assert!((peak - 0.11).abs() < 1e-9 && (day - 0.02).abs() < 1e-9);
        assert_eq!(observe(&mut watch, 3600.0, 101.0, 60.0), None, "the rise over a pause is not learned");
        assert_eq!(observe(&mut watch, 3660.0, 101.0, 60.0), Some((0.0, 0.0)));
    }

    #[test]
    fn accept_starts_the_limit_again() {
        let mut watch = Watch { accepted_at: Some(0.0), budget_s: 900.0, held: true, ..Watch::default() };
        accept(&mut watch, 1000.0);
        assert_eq!((watch.accepted_at, watch.budget_s, watch.held), (Some(1000.0), 0.0, false));
    }

    #[test]
    fn held_by_readings_over_the_limit() {
        let mut watch = Watch { seen_at: Some(0.0), accepted_at: Some(0.0), learned: Some(BUSY), ..Watch::default() };
        hold(&mut watch);
        for minute in 1..=300 {
            advance(&mut watch, f64::from(minute) * 60.0, 60.0);
        }
        assert!(watch.held && watch.budget_s == 0.0, "however long readings over the limit go on, only time away counts");
    }

    #[test]
    fn a_learning_counter_keeps_out_issue_40s_misread() {
        let allowed = limit(auto(0.0), WATER, None, 1920.0).unwrap();
        assert!(allowed < 637.799 - 635.709, "a wheel read two digits off 32 minutes after the last accepted reading");
    }
}
