//! What turns a region into text (origin `readers.py`): the pictures the number reader and the
//! wheel reader are shown, greedy CTC decoding limited to the characters a reading may have, and
//! the wheel decoder. Pure; the networks run in `infer`. Python's `round` rounds half to even, as
//! `round_ties_even` here.

use super::pixels::{self, Rgb};
use crate::sensors::types::{NumberReader, WheelModel};

/// The characters a reading may have, with their class in PP-OCR's output (`READING["chars"]` and
/// the registry's `chars`). Class 0 is the CTC blank.
pub const OCR_CHARS: [(char, usize); 14] = [
    ('0', 33), ('1', 34), ('2', 35), ('3', 36), ('4', 37), ('5', 38), ('6', 39), ('7', 40), ('8', 41), ('9', 42),
    ('.', 14), (',', 12), (':', 16), ('-', 13),
];
/// The height PP-OCR reads at; the width follows the picture, up to `OCR_MAX_WIDTH` (`MAX_WIDTH`).
pub const OCR_HEIGHT: usize = 48;
const OCR_MAX_WIDTH: usize = 1600;
/// "auto" display: below this confidence the segments are tried too (`segments_fallback_below`).
pub const SEGMENTS_FALLBACK_BELOW: f32 = 0.6;
/// "counter" display read as text: the share of each cell's width that is read (`counter_cell_share`).
const COUNTER_CELL_SHARE: f64 = 0.7;
/// Several row bands are read and the most confident with the right number of digits is kept
/// (`counter_band_tops`, `counter_band_bottoms`, `counter_band_min_height`).
const COUNTER_BAND_TOPS: [f64; 4] = [0.0, 0.08, 0.16, 0.24];
const COUNTER_BAND_BOTTOMS: [f64; 6] = [1.0, 0.92, 0.84, 0.76, 0.68, 0.6];
const COUNTER_BAND_MIN_HEIGHT: f64 = 0.5;
/// Wheel reader: bins a wheel at rest may sit off its digit, how far all wheels may appear shifted
/// together and what each bin of shift costs (`wheel_slack_bins`, `wheel_max_shift_bins`,
/// `wheel_shift_penalty`); "weakest" counts the probability within this many bins
/// (`wheel_weakest_bins`).
const WHEEL_SLACK_BINS: isize = 1;
const WHEEL_MAX_SHIFT_BINS: isize = 2;
const WHEEL_SHIFT_PENALTY: f64 = 0.3;
const WHEEL_WEAKEST_BINS: isize = 4;

/// The number of classes the reader's output has (registry `classes`).
pub const fn ocr_classes(reader: NumberReader) -> usize {
    match reader {
        NumberReader::Tiny => 6906,
        NumberReader::Small => 18_710,
    }
}

/// A wheel reader of the registry (`wheel_readers`).
pub struct WheelSpec {
    /// Width and height of a cell as the model sees it.
    pub cell: [usize; 2],
    /// Positions around a wheel: bin `b` is position `b / (bins / 10)`.
    pub bins: usize,
    /// A model with a second input: every cell again with this share of a cell on each side, at
    /// this width.
    pub context: Option<(f64, usize)>,
    pub confidence: Confidence,
}

/// How sure a wheel reading is: the geometric mean of the wheels' decoded probabilities, or the
/// weakest wheel's probability near where the value puts it (one unclear wheel is not hidden).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confidence {
    Mean,
    Weakest,
}

pub const fn wheel_spec(model: WheelModel) -> WheelSpec {
    match model {
        WheelModel::V1 => WheelSpec { cell: [32, 48], bins: 100, context: None, confidence: Confidence::Mean },
        WheelModel::V2 => WheelSpec { cell: [32, 48], bins: 100, context: Some((0.5, 64)), confidence: Confidence::Weakest },
    }
}

/// One 8-bit channel, as PIL's "L".
#[derive(Clone, Debug, PartialEq)]
pub struct Grey {
    pub size: [usize; 2],
    pub pixels: Vec<u8>,
}

impl Grey {
    /// `[x0, y0, x1, y1)`, black outside the picture (PIL's `crop`).
    fn crop(&self, [x0, y0, x1, y1]: [isize; 4]) -> Grey {
        let [w, h] = self.size;
        let size = [(x1 - x0).max(0) as usize, (y1 - y0).max(0) as usize];
        let mut pixels = vec![0u8; size[0] * size[1]];
        for y in 0..size[1] {
            let sy = y0 + y as isize;
            if !(0..h as isize).contains(&sy) {
                continue;
            }
            for x in 0..size[0] {
                let sx = x0 + x as isize;
                if (0..w as isize).contains(&sx) {
                    pixels[y * size[0] + x] = self.pixels[sy as usize * w + sx as usize];
                }
            }
        }
        Grey { size, pixels }
    }

    fn resized(&self, size: [usize; 2]) -> Grey {
        Grey { size, pixels: pixels::resize(&self.pixels, 1, self.size, size) }
    }

    pub fn to_rgb(&self) -> Rgb {
        Rgb { size: self.size, pixels: self.pixels.iter().flat_map(|v| [*v; 3]).collect() }
    }
}

fn round(v: f64) -> isize {
    v.round_ties_even() as isize
}

/// Grey from each pixel's darkest colour channel: coloured digits turn as dark as black ones (the
/// red decimal wheels of a water meter are only mid-grey in a plain grey picture).
fn darkest_channel(img: &Rgb) -> Grey {
    Grey { size: img.size, pixels: img.pixels.as_chunks::<3>().0.iter().map(|p| p[0].min(p[1]).min(p[2])).collect() }
}

/// Stretched so that 1 % of the pixels at each end are clipped (PIL's `autocontrast(cutoff=1)`).
fn autocontrast(img: &Grey) -> Grey {
    let mut hist = [0u64; 256];
    img.pixels.iter().for_each(|v| hist[usize::from(*v)] += 1);
    let n: u64 = hist.iter().sum();
    for order in [false, true] {
        let mut cut = n / 100;
        for i in 0..256 {
            let level = if order { 255 - i } else { i };
            if cut > hist[level] {
                cut -= hist[level];
                hist[level] = 0;
            } else {
                hist[level] -= cut;
                cut = 0;
            }
            if cut == 0 {
                break;
            }
        }
    }
    let lo = hist.iter().position(|c| *c > 0).unwrap_or(255);
    let hi = hist.iter().rposition(|c| *c > 0).unwrap_or(0);
    if hi <= lo {
        return img.clone();
    }
    let scale = 255.0 / (hi - lo) as f64;
    let offset = -(lo as f64) * scale;
    let lut: Vec<u8> = (0..256).map(|v| ((v as f64 * scale + offset) as i64).clamp(0, 255) as u8).collect();
    Grey { size: img.size, pixels: img.pixels.iter().map(|v| lut[usize::from(*v)]).collect() }
}

/// The region as it is, in grey at full contrast (origin `plain`).
pub fn plain(img: &Rgb) -> Grey {
    autocontrast(&Grey { size: img.size, pixels: pixels::grey(img) })
}

/// Coloured digits as dark as black ones, at full contrast (origin `darkest`).
pub fn darkest(img: &Rgb) -> Grey {
    autocontrast(&darkest_channel(img))
}

/// The level that best splits `values` into two groups; `None` with only one level.
fn otsu(values: impl Iterator<Item = u8>) -> Option<u8> {
    let mut hist = [0f64; 256];
    values.for_each(|v| hist[usize::from(v)] += 1.0);
    let total = hist.iter().sum::<f64>().max(1.0);
    let (mut omega, mut mu) = ([0f64; 256], [0f64; 256]);
    let (mut w, mut m) = (0.0, 0.0);
    for i in 0..256 {
        w += hist[i] / total;
        m += hist[i] / total * i as f64;
        (omega[i], mu[i]) = (w, m);
    }
    let mut best: Option<(u8, f64)> = None;
    for i in 0..255 {
        let between = (mu[255] * omega[i] - mu[i]).powi(2) / (omega[i] * (1.0 - omega[i]));
        if between.is_finite() && best.is_none_or(|(_, b)| between > b) {
            best = Some((i as u8, between));
        }
    }
    best.map(|(level, _)| level)
}

/// Light digits on a dark panel: the border (background) is darker than the picture on average.
pub fn is_light_on_dark(img: &Rgb) -> bool {
    let grey = pixels::grey(img);
    let [w, h] = img.size;
    let edge = (w.min(h) / 10).max(1);
    let mut border: Vec<u8> = Vec::new();
    for y in (0..edge).chain(h - edge..h) {
        border.extend_from_slice(&grey[y * w..(y + 1) * w]);
    }
    for x in (0..edge).chain(w - edge..w) {
        border.extend((0..h).map(|y| grey[y * w + x]));
    }
    border.sort_unstable();
    let mid = border.len() / 2;
    let median = if border.len().is_multiple_of(2) { (f64::from(border[mid - 1]) + f64::from(border[mid])) / 2.0 } else { f64::from(border[mid]) };
    let mean = grey.iter().map(|v| f64::from(*v)).sum::<f64>() / grey.len() as f64;
    median < mean
}

/// Only the digits' own segments, as dark text on white (origin `segments`). Two thresholds: the
/// first separates the background, the second the lit segments from unlit ones that still show
/// faintly, so a "3" never reads as "8". A little blur joins the dots of dot-matrix digits.
pub fn segments(img: &Rgb, light_digits: bool) -> Grey {
    let strength: Vec<u8> = img
        .pixels
        .as_chunks::<3>()
        .0
        .iter()
        .map(|p| if light_digits { p[0].max(p[1]).max(p[2]) } else { 255 - p[0].min(p[1]).min(p[2]) })
        .collect();
    let Some(first) = otsu(strength.iter().copied()) else { return plain(img) };
    let strong = strength.iter().copied().filter(|v| *v > first);
    let threshold = if strong.clone().count() > 20 { otsu(strong).unwrap_or(first) } else { first };
    let mask = Grey { size: img.size, pixels: strength.iter().map(|v| if *v > threshold { 255 } else { 0 }).collect() };
    let blurred = gaussian_blur(&mask, (img.size[1] as f32 / 200.0).max(1.0));
    Grey { size: img.size, pixels: blurred.pixels.iter().map(|v| 255 - v).collect() }
}

/// PIL's `GaussianBlur`: three box blurs each way with an extended box of the radius that gives
/// the same variance.
fn gaussian_blur(img: &Grey, radius: f32) -> Grey {
    let sigma2 = radius * radius / 3.0;
    let l = (((12.0 * sigma2 + 1.0).sqrt() - 1.0) / 2.0).floor();
    let box_radius = l + (2.0 * l + 1.0) * (l * (l + 1.0) - 3.0 * sigma2) / (6.0 * (sigma2 - (l + 1.0) * (l + 1.0)));
    let [w, h] = img.size;
    let mut out = img.pixels.clone();
    for _ in 0..3 {
        for row in out.chunks_exact_mut(w) {
            let line = box_blur(row, box_radius);
            row.copy_from_slice(&line);
        }
    }
    for _ in 0..3 {
        for x in 0..w {
            let column: Vec<u8> = (0..h).map(|y| out[y * w + x]).collect();
            for (y, v) in box_blur(&column, box_radius).into_iter().enumerate() {
                out[y * w + x] = v;
            }
        }
    }
    Grey { size: img.size, pixels: out }
}

/// One line of PIL's extended box blur, in its fixed point: the edge pixel repeats.
fn box_blur(line: &[u8], float_radius: f32) -> Vec<u8> {
    let radius = float_radius as isize;
    let ww = (16_777_216.0 / (float_radius * 2.0 + 1.0)) as u64;
    let fw = (16_777_216 - (radius as u64 * 2 + 1) * ww) / 2;
    let last = line.len() as isize - 1;
    let at = |i: isize| u64::from(line[i.clamp(0, last) as usize]);
    (0..line.len() as isize)
        .map(|x| {
            let acc: u64 = (x - radius..=x + radius).map(at).sum();
            let bulk = acc * ww + (at(x - radius - 1) + at(x + radius + 1)) * fw;
            ((bulk + (1 << 23)) >> 24) as u8
        })
        .collect()
}

/// The wheels of a mechanical counter side by side without the dividers between them: the region
/// is split into `digits` equal cells and the middle of each is kept (a divider reads as a "1").
pub fn counter_line(img: &Rgb, digits: usize) -> Rgb {
    let digits = digits.max(1);
    let [w, h] = img.size;
    let cell = w as f64 / digits as f64;
    let keep = round(cell * COUNTER_CELL_SHARE).max(1) as usize;
    let mut line = Rgb { size: [keep * digits, h], pixels: vec![0; keep * digits * h * 3] };
    for i in 0..digits {
        let left = round(cell * i as f64 + (cell - keep as f64) / 2.0);
        for y in 0..h {
            for x in 0..keep {
                let sx = left + x as isize;
                if (0..w as isize).contains(&sx) {
                    let (src, dst) = ((y * w + sx as usize) * 3, (y * keep * digits + i * keep + x) * 3);
                    line.pixels[dst..dst + 3].copy_from_slice(&img.pixels[src..src + 3]);
                }
            }
        }
    }
    line
}

/// The row bands of a counter's line that are read, the whole height first.
pub fn counter_bands(line: &Grey) -> Vec<Grey> {
    let [w, h] = line.size;
    let mut bands = Vec::new();
    for top in COUNTER_BAND_TOPS {
        for bottom in COUNTER_BAND_BOTTOMS {
            if bottom - top < COUNTER_BAND_MIN_HEIGHT {
                continue;
            }
            bands.push(line.crop([0, round(top * h as f64), w as isize, round(bottom * h as f64).max(1)]));
        }
    }
    bands
}

/// PP-OCR's input: `OCR_HEIGHT` high, as wide as the picture's aspect gives, scaled to `-1..=1`.
/// The picture is grey, so the channel order does not matter.
pub fn ocr_input(img: &Grey) -> (Vec<f32>, [usize; 4]) {
    let [w, h] = img.size;
    let width = (OCR_HEIGHT * w).div_ceil(h.max(1)).clamp(OCR_HEIGHT / 3, OCR_MAX_WIDTH).max(OCR_HEIGHT / 3);
    let small = img.resized([width, OCR_HEIGHT]);
    let plane: Vec<f32> = small.pixels.iter().map(|v| (f32::from(*v) / 255.0 - 0.5) / 0.5).collect();
    (plane.repeat(3), [1, 3, OCR_HEIGHT, width])
}

/// Greedy CTC decoding over the reading's characters only: the best of them per time step, repeats
/// merged, blanks dropped. `probs` is time steps × `classes`. The score is the mean probability of
/// the characters read, 0 when none.
pub fn ctc_decode(probs: &[f32], classes: usize) -> (String, f32) {
    let columns: Vec<usize> = std::iter::once(0).chain(OCR_CHARS.iter().map(|(_, class)| *class)).collect();
    let (mut text, mut scores, mut previous) = (String::new(), Vec::new(), usize::MAX);
    for step in probs.chunks_exact(classes) {
        let (best, p) = columns.iter().enumerate().fold((0, f32::NEG_INFINITY), |(bi, bp), (i, c)| {
            if step[*c] > bp { (i, step[*c]) } else { (bi, bp) }
        });
        if best != previous && best != 0 {
            text.push(OCR_CHARS[best - 1].0);
            scores.push(p);
        }
        previous = best;
    }
    let score = if scores.is_empty() { 0.0 } else { scores.iter().sum::<f32>() / scores.len() as f32 };
    (text, score)
}

/// The region split into `digits` equal cells, each as the wheel model sees it: grey from the
/// darkest channel, scaled to `cell` and stretched to full contrast, `0.0..=1.0`, cells × height
/// × width (origin `wheel_cells`).
pub fn wheel_cells(region: &Rgb, digits: usize, cell: [usize; 2]) -> Vec<f32> {
    let grey = darkest_channel(region);
    let digits = digits.max(1);
    let width = grey.size[0] as f64 / digits as f64;
    (0..digits)
        .flat_map(|i| {
            let a = round(i as f64 * width);
            let b = (a + 1).max(round((i + 1) as f64 * width));
            scaled_cell(&grey.crop([a, 0, b, grey.size[1] as isize]), cell)
        })
        .collect()
}

/// The region `[x0, x1)` of `rows` (the frame's rows of the region, whole width) split into
/// `digits` cells, each with `context` of a cell on both sides from the picture around it, the
/// picture's edge repeated beyond it (origin `wheel_context_cells`).
pub fn wheel_context_cells(rows: &Rgb, [x0, x1]: [usize; 2], digits: usize, cell: [usize; 2], context: f64) -> Vec<f32> {
    let grey = darkest_channel(rows);
    let [w, h] = grey.size;
    let digits = digits.max(1);
    let width = (x1 as f64 - x0 as f64) / digits as f64;
    let pad = (width * context).ceil() as usize + 2;
    let wide = Grey {
        size: [w + 2 * pad, h],
        pixels: (0..h).flat_map(|y| (0..w + 2 * pad).map(move |x| (y, x))).map(|(y, x)| grey.pixels[y * w + x.saturating_sub(pad).min(w - 1)]).collect(),
    };
    (0..digits)
        .flat_map(|i| {
            let a = round(x0 as f64 + (i as f64 - context) * width + pad as f64);
            let b = (a + 1).max(round(x0 as f64 + (i as f64 + 1.0 + context) * width + pad as f64));
            scaled_cell(&wide.crop([a, 0, b, h as isize]), cell)
        })
        .collect()
}

fn scaled_cell(part: &Grey, cell: [usize; 2]) -> Vec<f32> {
    autocontrast(&part.resized(cell)).pixels.iter().map(|v| f32::from(*v) / 255.0).collect()
}

/// The cells side by side as the model sees them, one wheel per field, twice the size: a region
/// that cuts the wheels wrong shows (origin `cells_image`).
pub fn cells_image(cells: &[f32], cell: [usize; 2]) -> Rgb {
    let [w, h] = cell;
    let count = cells.len() / (w * h);
    let gap = 2;
    let width = (count * (w + gap)).saturating_sub(gap).max(1);
    let mut line = vec![255u8; width * h];
    for (i, values) in cells.chunks_exact(w * h).enumerate() {
        for (j, v) in values.iter().enumerate() {
            line[(j / w) * width + i * (w + gap) + j % w] = (v * 255.0) as u8;
        }
    }
    let big = Grey { size: [width * 2, h * 2], pixels: (0..h * 2).flat_map(|y| (0..width * 2).map(move |x| (y, x))).map(|(y, x)| line[(y / 2) * width + x / 2]).collect() };
    big.to_rgb()
}

/// Each wheel's position probabilities from the model's logits (wheels × bins).
pub fn wheel_probabilities(logits: &[f32], bins: usize) -> Vec<Vec<f64>> {
    logits
        .chunks_exact(bins)
        .map(|row| {
            let top = row.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            let e: Vec<f64> = row.iter().map(|v| (f64::from(*v) - f64::from(top)).exp()).collect();
            let total: f64 = e.iter().sum();
            e.into_iter().map(|v| v / total).collect()
        })
        .collect()
}

/// Log probabilities for decoding: a wheel at rest may sit a little off its digit.
pub fn with_slack(probs: &[Vec<f64>]) -> Vec<Vec<f64>> {
    probs
        .iter()
        .map(|row| {
            let n = row.len() as isize;
            (0..n).map(|b| (-WHEEL_SLACK_BINS..=WHEEL_SLACK_BINS).map(|s| row[(b - s).rem_euclid(n) as usize]).sum::<f64>().max(1e-12).ln()).collect()
        })
        .collect()
}

/// The most likely counter value for per-wheel position log probabilities (wheels × bins), its
/// log probability and the bin every wheel stands at, left to right. The last wheel turns freely;
/// every other wheel stands on its digit and turns only while the wheel to its right goes from 9
/// to 0, by as much as that wheel is past 9. A dynamic programme from right to left finds the
/// value whose wheel positions are most likely together. The value is in steps of the last wheel,
/// with its fraction (0089949.8: the last wheel at 9.8).
pub fn wheel_decode(logp: &[Vec<f64>]) -> (f64, f64, Vec<usize>) {
    let bins = logp[0].len();
    let tenth = bins / 10;
    let argmax = |v: &[f64]| v.iter().enumerate().fold(0, |best, (i, x)| if *x > v[best] { i } else { best });
    let mut best = logp[logp.len() - 1].clone();
    let mut back: Vec<Vec<usize>> = Vec::new();
    for k in (0..logp.len() - 1).rev() {
        let rest = argmax(&best[..=9 * tenth]);
        let lead: Vec<f64> = std::iter::once(best[rest]).chain(best[9 * tenth + 1..].iter().copied()).collect();
        let lead_bin: Vec<usize> = std::iter::once(rest).chain(9 * tenth + 1..bins).collect();
        best = (0..bins).map(|b| logp[k][b] + lead[b % tenth]).collect();
        back.push((0..bins).map(|b| lead_bin[b % tenth]).collect());
    }
    let mut b = argmax(&best);
    let score = best[b];
    let mut at = vec![b];
    for arg in back.iter().rev() {
        b = arg[b];
        at.push(b);
    }
    let value = at[..at.len() - 1].iter().fold(0.0, |v, a| v * 10.0 + (a / tenth) as f64);
    (value * 10.0 + at[at.len() - 1] as f64 / tenth as f64, score, at)
}

/// A counter's `digits` wheels from their position probabilities: the text (all digits, the last
/// wheel rounded) and the confidence. A region drawn a little above or below the digits shifts
/// every wheel alike, so a few common shifts are tried, each costing a little.
pub fn wheel_reading(probs: &[Vec<f64>], confidence: Confidence, digits: usize) -> (String, f32) {
    let bins = probs[0].len();
    let logp = with_slack(probs);
    let mut best: Option<(f64, f64, Vec<usize>, isize)> = None;
    for shift in -WHEEL_MAX_SHIFT_BINS..=WHEEL_MAX_SHIFT_BINS {
        let rolled: Vec<Vec<f64>> = logp.iter().map(|row| (0..bins).map(|b| row[(b as isize + shift).rem_euclid(bins as isize) as usize]).collect()).collect();
        let (value, score, at) = wheel_decode(&rolled);
        let score = score - WHEEL_SHIFT_PENALTY * shift.abs() as f64;
        if best.as_ref().is_none_or(|b| score > b.1) {
            best = Some((value, score, at, shift));
        }
    }
    let (value, score, at, shift) = best.expect("at least one shift");
    let number = (value + 0.5).floor() as u64 % 10u64.pow(digits as u32);
    let bin = |b: usize, n: isize| (b as isize + shift + n).rem_euclid(bins as isize) as usize;
    let sure = match confidence {
        Confidence::Mean => (score / at.len().max(1) as f64).exp(),
        Confidence::Weakest => at
            .iter()
            .enumerate()
            .map(|(k, b)| (-WHEEL_WEAKEST_BINS..=WHEEL_WEAKEST_BINS).map(|n| probs[k][bin(*b, n)]).sum::<f64>())
            .fold(f64::INFINITY, f64::min),
    };
    (format!("{number:0digits$}"), sure as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Where every wheel stands for the number on the wheels, most significant first: the last
    /// wheel turns continuously, every other only while the wheel to its right passes 9 (origin
    /// `tests/displays.counter_positions`).
    fn counter_positions(value: f64, digits: usize) -> Vec<f64> {
        let mut lower = 0.0f64;
        let mut out: Vec<f64> = (0..digits as i32)
            .map(|k| {
                let p = if k == 0 { value.rem_euclid(10.0) } else { (value / 10f64.powi(k)).floor().rem_euclid(10.0) + (lower - 9.0).max(0.0) };
                lower = p;
                p.rem_euclid(10.0)
            })
            .collect();
        out.reverse();
        out
    }

    fn wheel_logp(value: f64, digits: usize) -> Vec<Vec<f64>> {
        counter_positions(value, digits)
            .into_iter()
            .map(|p| {
                let mut row = vec![-30.0; 100];
                row[((p * 10.0).round() as usize) % 100] = 0.0;
                row
            })
            .collect()
    }

    #[test]
    fn counter_positions_follow_the_wheel_rule() {
        assert_eq!(counter_positions(89932.0, 7), [0.0, 0.0, 8.0, 9.0, 9.0, 3.0, 2.0]);
        let turning = counter_positions(89999.5, 7);
        for (got, want) in turning.iter().zip([0.0, 0.0, 8.5, 9.5, 9.5, 9.5, 9.5]) {
            assert!((got - want).abs() < 1e-9, "{turning:?}");
        }
    }

    #[test]
    fn wheel_value_follows_the_wheels() {
        for value in [89932.0, 89939.5, 89949.8, 90099.9, 1_234_567.0, 9_999_999.9, 0.0, 0.3] {
            let (found, score, _) = wheel_decode(&wheel_logp(value, 7));
            assert!((found - value).abs() < 1e-6 && score.abs() < 1e-9, "{value}: {found}");
        }
    }

    #[test]
    fn wheel_value_prefers_a_consistent_reading() {
        let mut logp = wheel_logp(89949.8, 7);
        logp[5] = vec![1e-6f64.ln(); 100];
        logp[5][48] = 0.4f64.ln();
        logp[5][50] = 0.6f64.ln();
        let (found, _, _) = wheel_decode(&logp);
        assert!((found - 89949.8).abs() < 1e-6, "the last wheel at 9.8 makes the one before it 4.8, not 5: {found}");
    }

    #[test]
    fn the_weakest_wheel_is_the_confidence_of_a_wheel_reader_that_says_so() {
        let mut probs = vec![vec![1e-6; 100]; 4];
        for (k, b) in [10, 20, 30].into_iter().enumerate() {
            probs[k][b] = 1.0;
        }
        probs[3][40] = 0.5;
        probs[3][75] = 0.5;
        for row in &mut probs {
            let total: f64 = row.iter().sum();
            row.iter_mut().for_each(|v| *v /= total);
        }
        let (text, weakest) = wheel_reading(&probs, Confidence::Weakest, 4);
        assert_eq!(text, "1234");
        assert!((weakest - 0.5).abs() < 0.01, "one unclear wheel among clear ones: {weakest}");
        let (_, mean) = wheel_reading(&probs, Confidence::Mean, 4);
        assert!(mean > 0.8, "the mean hides it: {mean}");
    }

    #[test]
    fn ctc_decode_merges_repeats_and_drops_blanks() {
        let classes = 50;
        let steps = [0, 34, 34, 0, 34, 35, 35, 0];
        let mut probs = vec![0.05f32; steps.len() * classes];
        for (i, c) in steps.iter().enumerate() {
            probs[i * classes + c] = 0.9;
        }
        let (text, score) = ctc_decode(&probs, classes);
        assert_eq!(text, "112");
        assert!((score - 0.9).abs() < 1e-6);
        let mut letter = vec![0.0f32; classes];
        letter[20] = 0.99;
        letter[33] = 0.01;
        assert_eq!(ctc_decode(&letter, classes).0, "0", "a letter is never read: the best of the reading's characters is");
    }

    #[test]
    fn counter_line_keeps_the_middle_of_every_cell() {
        let mut image = Rgb { size: [140, 30], pixels: vec![0; 140 * 30 * 3] };
        for i in 0..7 {
            for y in 0..30 {
                for x in i * 20 + 3..i * 20 + 17 {
                    image.pixels[(y * 140 + x) * 3..(y * 140 + x) * 3 + 3].copy_from_slice(&[255; 3]);
                }
            }
        }
        let line = counter_line(&image, 7);
        assert_eq!(line.size, [7 * 14, 30]);
        assert!(line.pixels.iter().all(|v| *v == 255), "no divider left");
    }

    #[test]
    fn darkest_turns_coloured_digits_as_dark_as_black_ones() {
        let image = Rgb { size: [3, 1], pixels: vec![200, 30, 35, 25, 25, 25, 235, 235, 230] };
        let dark = darkest(&image).pixels;
        assert!(dark[0].abs_diff(dark[1]) < 20 && dark[2] > 200);
        let grey = plain(&image).pixels;
        assert!(grey[0] - grey[1] > 40, "in a plain grey picture red stays much lighter than black");
    }

    #[test]
    fn segments_handle_flat_and_large_pictures() {
        let flat = Rgb { size: [40, 20], pixels: [255, 0, 0].repeat(40 * 20) };
        assert_eq!(segments(&flat, true).size, [40, 20], "one level: no threshold exists");
        let (w, h) = (600, 400);
        let mut panel = Rgb { size: [w, h], pixels: [20, 20, 20].repeat(w * h) };
        for y in 100..300 {
            for x in (100..500).filter(|x| (x / 50) % 2 == 0) {
                panel.pixels[(y * w + x) * 3..(y * w + x) * 3 + 3].copy_from_slice(&[255, 40, 40]);
            }
        }
        let mask = segments(&panel, true);
        let dark = mask.pixels.iter().filter(|v| **v < 128).count() as f64 / mask.pixels.len() as f64;
        assert!(0.05 < dark && dark < 0.6, "the lit segments, not the whole panel: {dark}");
    }

    #[test]
    fn autocontrast_stretches_and_leaves_a_flat_picture() {
        let ramp = Grey { size: [100, 1], pixels: (50..150).collect() };
        let out = autocontrast(&ramp).pixels;
        assert_eq!((out[0], out[99]), (0, 255));
        let flat = Grey { size: [4, 1], pixels: vec![7; 4] };
        assert_eq!(autocontrast(&flat), flat);
    }
}
