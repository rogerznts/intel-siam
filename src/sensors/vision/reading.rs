//! What a reading means (origin `readers.py` interpretation and `engine/logic.reading_verdict`):
//! whether it is accepted, settling or rejected, and the diagnostics of a reading sensor. Pure:
//! times are seconds since the epoch, passed in.

use std::collections::VecDeque;

use crate::sensors::types::{Number, ReadOutcome, ReadProblem, ReadingMode, ReadingSettings, Unit};

/// The "accepted" share covers the readings of the last day (`READING["accepted_window_s"]`).
pub const ACCEPTED_WINDOW_S: f64 = 86_400.0;
/// No flow until two accepted readings are at least this far apart (`READING["rate_min_span_s"]`).
pub const RATE_MIN_SPAN_S: f64 = 30.0;
/// The days the quality is summed over, today included (`READING["quality_periods_days"]`).
pub const QUALITY_PERIODS_DAYS: [u32; 3] = [1, 7, 30];

pub fn digit_count(text: &str) -> usize {
    text.chars().filter(char::is_ascii_digit).count()
}

/// A mechanical counter always shows all its wheels: any other number of digits is a misread.
pub fn wrong_digit_count(text: &str, settings: &ReadingSettings) -> bool {
    settings.digits().is_some_and(|digits| digit_count(text) != usize::from(digits))
}

/// Why a new value cannot be right next to the last accepted one, or `None`. A counter may go up
/// by `limit` at most (`None`: no limit); any other value may change by its `max_step` either way
/// (0: no limit).
pub fn implausible(value: f64, last: Option<f64>, mode: &ReadingMode, limit: Option<f64>) -> Option<ReadProblem> {
    let last = last?;
    let step = match mode {
        ReadingMode::Counter { .. } if value < last => return Some(ReadProblem::WentDown),
        ReadingMode::Counter { .. } => limit.unwrap_or(0.0),
        ReadingMode::Value { max_step } | ReadingMode::TimeLeft { max_step } => f64::from(*max_step),
    };
    (step > 0.0 && (value - last).abs() > step).then_some(ReadProblem::ChangedTooMuch)
}

/// Whether a counter read exactly one step of its last digit below its value: the last wheel
/// turning. A wheel between two digits is read as the one or the other; once the higher one was
/// published, every right reading until the counter gets there is one step lower. Such a reading
/// keeps the value without counting as rejected.
pub fn settling(value: f64, last: f64, settings: &ReadingSettings) -> bool {
    let step = 10f64.powi(-i32::from(settings.decimals()));
    settings.is_counter() && ((last - value) - step).abs() < step / 1000.0
}

/// Whether a reading is accepted, settling or rejected and why: nothing read, another number of
/// digits than the counter has wheels, too unsure, or implausible next to the last published
/// value (`limit`: how much a counter may go up).
pub fn verdict(
    text: &str,
    score: f32,
    value: Option<Number>,
    last: Option<Number>,
    threshold: Unit,
    settings: &ReadingSettings,
    limit: Option<f64>,
) -> ReadOutcome {
    let Some(value) = value else { return ReadOutcome::Rejected(ReadProblem::NothingRead) };
    if wrong_digit_count(text, settings) {
        return ReadOutcome::Rejected(ReadProblem::WrongDigitCount);
    }
    if score < f32::from(threshold) {
        return ReadOutcome::Rejected(ReadProblem::Unsure);
    }
    let last = last.map(Number::get);
    match implausible(value.get(), last, &settings.mode, limit) {
        Some(ReadProblem::WentDown) if last.is_some_and(|last| settling(value.get(), last, settings)) => ReadOutcome::Settling,
        Some(problem) => ReadOutcome::Rejected(problem),
        None => ReadOutcome::Accepted,
    }
}

/// The share of accepted readings among `reads` (time, accepted) of the last `window_s`, in
/// percent; older pairs are dropped.
pub fn accepted_share(reads: &mut VecDeque<(f64, bool)>, now: f64, window_s: f64) -> Option<f32> {
    while reads.front().is_some_and(|(at, _)| *at < now - window_s) {
        reads.pop_front();
    }
    if reads.is_empty() {
        return None;
    }
    let accepted = reads.iter().filter(|(_, ok)| *ok).count();
    Some((1000.0 * accepted as f64 / reads.len() as f64).round() as f32 / 10.0)
}

/// How fast a counter goes up, per hour, over about the last window. `samples` (time, accepted
/// value) is trimmed to the samples inside the window plus the last one before it, which anchors
/// the start: with readings far apart that gives the average since the previous reading.
pub fn counter_rate(samples: &mut VecDeque<(f64, f64)>, now: f64, window_s: f64, min_span_s: f64) -> Option<f64> {
    while samples.len() > 1 && samples[1].0 <= now - window_s {
        samples.pop_front();
    }
    let (&(t0, v0), &(t1, v1)) = (samples.front()?, samples.back()?);
    if samples.len() < 2 || t1 - t0 < min_span_s {
        return None;
    }
    Some(((v1 - v0) * 3600.0 / (t1 - t0)).max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sensors::types::{Amount, ChangeLimit, CounterReader, Decimals, Digits, RateWindow, ReadingDisplay, WheelModel};

    fn base() -> ReadingSettings {
        ReadingSettings::default()
    }

    fn counter(decimals: u8) -> ReadingSettings {
        let mode = ReadingMode::Counter { limit: ChangeLimit::Off, rate_window: RateWindow::try_from(15).unwrap() };
        ReadingSettings { mode, decimals: Decimals::try_from(decimals).unwrap(), ..base() }
    }

    fn value(max_step: f64, decimals: u8) -> ReadingSettings {
        let mode = ReadingMode::Value { max_step: Amount::try_from(max_step).unwrap() };
        ReadingSettings { mode, decimals: Decimals::try_from(decimals).unwrap(), ..base() }
    }

    fn wheels(digits: u8) -> ReadingSettings {
        let display = ReadingDisplay::Counter { digits: Digits::try_from(digits).unwrap(), reader: CounterReader::Wheels(WheelModel::V2) };
        ReadingSettings { display, ..counter(0) }
    }

    #[test]
    fn implausible_readings() {
        let counter = counter(0).mode;
        assert_eq!(implausible(100.0, None, &counter, Some(10.0)), None, "nothing to compare with yet");
        assert_eq!(implausible(99.0, Some(100.0), &counter, Some(10.0)), Some(ReadProblem::WentDown));
        assert_eq!(implausible(111.0, Some(100.0), &counter, Some(10.0)), Some(ReadProblem::ChangedTooMuch));
        assert_eq!(implausible(105.0, Some(100.0), &counter, Some(10.0)), None);
        assert_eq!(implausible(1000.0, Some(100.0), &counter, None), None, "no limit");
        assert_eq!(implausible(1.2, Some(1.9), &value(0.0, 1).mode, None), None, "values may go down; no step set");
        assert_eq!(implausible(1.2, Some(1.9), &value(0.5, 1).mode, None), Some(ReadProblem::ChangedTooMuch));
    }

    #[test]
    fn counter_digit_count() {
        assert!(!wrong_digit_count("0089932", &wheels(7)));
        assert!(wrong_digit_count("089932", &wheels(7)), "a wheel was missed");
        assert!(wrong_digit_count("00899321", &wheels(7)), "a divider read as a digit");
        assert!(!wrong_digit_count("12", &counter(0)), "other displays may show any number of digits");
    }

    #[test]
    fn settling_last_wheel() {
        let water = counter(3);
        assert!(settling(629.078, 629.079, &water), "one step of the last digit below: the last wheel turning");
        assert!(!settling(629.077, 629.079, &water), "two steps: a real went down");
        assert!(!settling(629.079, 629.079, &water) && !settling(629.08, 629.079, &water));
        assert!(settling(12344.0, 12345.0, &counter(0)));
        assert!(!settling(5.5, 5.6, &value(0.0, 1)));
    }

    #[test]
    fn verdict_rejects_and_settles() {
        let water = counter(3);
        let unit = |v: f32| Unit::try_from(v).unwrap();
        let n = |text: &str| water.parse(text);
        let last = n("0629079");
        let check = |text: &str, score: f32| verdict(text, score, n(text), last, unit(0.7), &water, Some(0.01));
        assert_eq!(check("0629080", 0.9), ReadOutcome::Accepted);
        assert_eq!(check("0629078", 0.9), ReadOutcome::Settling, "one step below is the last wheel turning");
        assert_eq!(check("0629070", 0.9), ReadOutcome::Rejected(ReadProblem::WentDown));
        assert_eq!(check("0629200", 0.9), ReadOutcome::Rejected(ReadProblem::ChangedTooMuch), "over the limit");
        assert_eq!(check("0629080", 0.5), ReadOutcome::Rejected(ReadProblem::Unsure));
        assert_eq!(check(".", 0.9), ReadOutcome::Rejected(ReadProblem::NothingRead));
        let seven = wheels(7);
        let outcome = verdict("089932", 0.99, seven.parse("089932"), None, unit(0.7), &seven, None);
        assert_eq!(outcome, ReadOutcome::Rejected(ReadProblem::WrongDigitCount));
    }

    #[test]
    fn accepted_share_and_rate() {
        let mut reads = VecDeque::from([(0.0, true), (100.0, false), (200.0, true), (300.0, true)]);
        assert_eq!(accepted_share(&mut reads, 300.0, 1000.0), Some(75.0));
        assert_eq!(accepted_share(&mut reads, 1150.0, 1000.0), Some(100.0));
        assert_eq!(reads.len(), 2, "the old ones dropped");
        assert_eq!(accepted_share(&mut VecDeque::new(), 0.0, 10.0), None);

        let mut samples = VecDeque::from([(0.0, 629.0), (600.0, 629.549), (900.0, 629.551), (1500.0, 629.552)]);
        let rate = counter_rate(&mut samples, 1500.0, 900.0, 30.0).unwrap();
        assert!((rate - 0.003 * 3600.0 / 900.0).abs() < 1e-9, "the window keeps one older sample as the start");
        assert_eq!(samples[0], (600.0, 629.549));
        let far = counter_rate(&mut VecDeque::from([(0.0, 10.0), (7200.0, 12.0)]), 7200.0, 900.0, 30.0).unwrap();
        assert!((far - 1.0).abs() < 1e-9, "readings far apart: the average since the previous one");
        assert_eq!(counter_rate(&mut VecDeque::from([(0.0, 10.0), (10.0, 10.5)]), 10.0, 900.0, 30.0), None, "too close together");
        assert_eq!(counter_rate(&mut VecDeque::from([(0.0, 10.0)]), 0.0, 900.0, 30.0), None);
    }
}
