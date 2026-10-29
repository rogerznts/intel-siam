//! Faces in person boxes, pure (origin `faces.py`, and the naming in `engine/faces.py`): where a
//! face is looked for, YuNet's outputs as faces, the quality gate, the alignment on five points,
//! and who a usable face is. A face that fails the gate is no face, never someone unknown. `infer`
//! runs the networks; the engine keeps each box's look from one check to the next.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::pixels::{self, Rgb};
use super::teach::{TEACH, box_iou};
use crate::sensors::Output;
use crate::sensors::types::{
    FaceLook, FaceMarks, FaceProblem, FaceState, Found, ObjectClass, PersonId, Recogniser, Verdict,
};

/// Origin `FACES`, without per-sensor overrides.
pub struct FaceRules {
    /// Person boxes looked at per check, most certain first: one detector run.
    pub max_faces: usize,
    /// A box that keeps a sure name is looked at again only every this many checks.
    pub recheck_every: u8,
    /// The upper part of a person box where the face is: its height and a margin on every side,
    /// in box widths (a standing person is about three widths tall).
    pub region_height: f32,
    pub region_margin: f32,
    /// A face the face detector finds at all.
    pub detect_score: f32,
    pub nms_iou: f32,
    /// The quality gate: the face detector's score, how far the face is turned (`turn`), the
    /// sharpness of the aligned face, and the eyes' share of the face's width under which it is a
    /// profile.
    pub min_score: f32,
    pub max_turn: f32,
    pub min_sharpness: f32,
    pub turned_eye_share: f32,
    /// A face that matched nobody counts towards the unknown person only when it is this many
    /// times the smallest face, turned at most this far, and found with at least this score.
    pub unknown_size: f32,
    pub unknown_max_turn: f32,
    pub unknown_min_score: f32,
    /// A person's score: the mean similarity to their this many closest taught faces.
    pub top_k: usize,
    /// A match must beat the next person by this much.
    pub margin: f32,
    /// What is stored of a taught face: its box and this share of its size around it.
    pub crop_margin: f32,
    /// Questions about one person per review cooldown, at least `ask_gap` apart.
    pub ask_burst: usize,
    pub ask_gap: Duration,
    /// Looking for a face while someone nobody is named for is there: a check every
    /// `burst_interval`, at most `burst_max`, over after `burst_idle_checks` checks without a usable
    /// face or the person coming `burst_progress` times closer, and ready again once nobody was
    /// seen `burst_absent_checks` checks in a row.
    pub burst_interval: Duration,
    pub burst_max: Duration,
    pub burst_idle_checks: u32,
    pub burst_absent_checks: u32,
    pub burst_progress: f32,
}

pub const FACES: FaceRules = FaceRules {
    max_faces: 4,                                // FACES["max_faces"]
    recheck_every: 3,                            // FACES["recheck_every"]
    region_height: 1.3,                          // FACES["region_height"]
    region_margin: 0.1,                          // FACES["region_margin"]
    detect_score: 0.6,                           // FACES["detect_score"]
    nms_iou: 0.3,                                // FACES["nms_iou"]
    min_score: 0.75,                             // FACES["min_score"]
    max_turn: 0.55,                              // FACES["max_turn"]
    min_sharpness: 25.0,                         // FACES["min_sharpness"]
    turned_eye_share: 0.3,                       // FACES["turned_eye_share"]
    unknown_size: 1.5,                           // FACES["unknown_size"]
    unknown_max_turn: 0.35,                      // FACES["unknown_max_turn"]
    unknown_min_score: 0.85,                     // FACES["unknown_min_score"]
    top_k: 3,                                    // FACES["top_k"]
    margin: 0.05,                                // FACES["margin"]
    crop_margin: 0.5,                            // FACES["crop_margin"]
    ask_burst: 5,                                // FACES["ask_burst"]
    ask_gap: Duration::from_secs(3),             // FACES["ask_gap_s"]
    burst_interval: Duration::from_secs(1),      // FACES["burst_interval_s"]
    burst_max: Duration::from_secs(30),          // FACES["burst_max_s"]
    burst_idle_checks: 6,                        // FACES["burst_idle_checks"]
    burst_absent_checks: 2,                      // FACES["burst_absent_checks"]
    burst_progress: 1.1,                         // FACES["burst_progress"]
};

/// The smallest usable face: its width in the frame, in pixels (origin `FACE_DEFAULTS`).
pub const MIN_FACE_PX: f32 = 30.0;
/// How a face becomes an embedding besides the model; part of the cache key, so a change here
/// embeds the taught faces again (origin `EMBEDDING_VERSION`: 2, the light is evened out).
pub const EMBEDDING_VERSION: u32 = 2;
/// Side of an aligned face, the recognisers' input.
pub const ALIGNED: usize = 112;
/// Where the five points sit in a 112 square aligned face (the ArcFace template).
const TEMPLATE: [[f32; 2]; 5] =
    [[38.2946, 51.6963], [73.5318, 51.5014], [56.0252, 71.7366], [41.5493, 92.3655], [70.7299, 92.2041]];

/// The key taught faces' embeddings are cached under: the recogniser and `EMBEDDING_VERSION`.
pub fn embedding_key(recogniser: Recogniser) -> String {
    format!("{}/{EMBEDDING_VERSION}", recogniser.id())
}

/// A recogniser's cosine similarities, which mean nothing for another model (origin `faces.json`):
/// a match needs `matched` and a margin, a name is published at once from `sure` (else once a
/// second check agrees), from `ask` the sensor may ask, and a box that stays keeps its name down
/// to `keep`.
pub struct Similarities {
    pub matched: f32,
    pub sure: f32,
    pub ask: f32,
    pub keep: f32,
}

pub const fn similarities(recogniser: Recogniser) -> Similarities {
    match recogniser {
        Recogniser::Sface => Similarities { matched: 0.5, sure: 0.6, ask: 0.32, keep: 0.4 },
        Recogniser::AuraFace => Similarities { matched: 0.4, sure: 0.5, ask: 0.28, keep: 0.3 },
    }
}

/// A face in a picture, in that picture's pixels. The stored form of a taught face, in its crop.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Face {
    pub area: [f32; 4],
    pub points: [[f32; 2]; 5],
    pub score: f32,
}

impl Face {
    pub fn width(&self) -> f32 {
        self.area[2] - self.area[0]
    }

    pub fn eye_distance(&self) -> f32 {
        let [[ax, ay], [bx, by], ..] = self.points;
        (bx - ax).hypot(by - ay)
    }

    /// The same face in another picture, whose pixels are `x * scale + dx` (and `y` alike).
    pub fn moved(&self, dx: f32, dy: f32, scale: f32) -> Self {
        let [x1, y1, x2, y2] = self.area;
        Self {
            area: [x1 * scale + dx, y1 * scale + dy, x2 * scale + dx, y2 * scale + dy],
            points: self.points.map(|[x, y]| [x * scale + dx, y * scale + dy]),
            score: self.score,
        }
    }

    pub fn marks(&self, [w, h]: [usize; 2]) -> FaceMarks {
        let (w, h) = (w as f32, h as f32);
        let [x1, y1, x2, y2] = self.area;
        FaceMarks { area: [x1 / w, y1 / h, x2 / w, y2 / h], points: self.points.map(|[x, y]| [x / w, y / h]), score: self.score }
    }

    pub fn from_marks(marks: &FaceMarks, [w, h]: [usize; 2]) -> Self {
        let (w, h) = (w as f32, h as f32);
        let [x1, y1, x2, y2] = marks.area;
        Self { area: [x1 * w, y1 * h, x2 * w, y2 * h], points: marks.points.map(|[x, y]| [x * w, y * h]), score: marks.score }
    }
}

/// Where a face is looked for in a person box (normalised): its upper part, in frame pixels
/// `[x1, y1, x2, y2]`. A box of only head and shoulders is about as tall as wide: taken whole.
pub fn head_region(area: [f32; 4], [width, height]: [usize; 2]) -> [usize; 4] {
    let [x1, y1, x2, y2] = [area[0] * width as f32, area[1] * height as f32, area[2] * width as f32, area[3] * height as f32];
    let (bw, bh) = (x2 - x1, y2 - y1);
    let margin = bw * FACES.region_margin;
    let bottom = y1 + bh.min(bw * FACES.region_height) + margin;
    [
        (x1 - margin).max(0.0) as usize,
        (y1 - margin).max(0.0) as usize,
        ((x2 + margin).ceil() as usize).min(width),
        (bottom.ceil() as usize).min(height),
    ]
}

/// Faces from YuNet's outputs (`cls`, `obj`, `bbox`, `kps` for strides 8, 16 and 32, in that
/// order), in the input's pixels; as OpenCV's `FaceDetectorYN`: the score is √(cls × obj), boxes
/// and points are offsets from each cell of the stride's grid.
pub fn decode_yunet(outputs: &[Output], size: usize, min_score: f32) -> Result<Vec<Face>, String> {
    if outputs.len() != 12 {
        return Err(format!("o detector de rostos devolveu {} saídas, e não 12", outputs.len()));
    }
    let mut faces = Vec::new();
    for (i, stride) in [8usize, 16, 32].into_iter().enumerate() {
        let cols = size / stride;
        let cells = cols * cols;
        let (cls, obj, bbox, kps) = (&outputs[i].values, &outputs[i + 3].values, &outputs[i + 6].values, &outputs[i + 9].values);
        if cls.len() != cells || obj.len() != cells || bbox.len() != cells * 4 || kps.len() != cells * 10 {
            return Err(format!("saída do detector de rostos com forma inesperada no passo {stride}"));
        }
        for cell in 0..cells {
            let score = (cls[cell].clamp(0.0, 1.0) * obj[cell].clamp(0.0, 1.0)).sqrt();
            if score < min_score {
                continue;
            }
            let (r, c) = ((cell / cols) as f32, (cell % cols) as f32);
            let s = stride as f32;
            let b = &bbox[cell * 4..cell * 4 + 4];
            let (cx, cy) = ((c + b[0]) * s, (r + b[1]) * s);
            let (w, h) = (b[2].exp() * s, b[3].exp() * s);
            let k = &kps[cell * 10..cell * 10 + 10];
            let points = std::array::from_fn(|p| [(k[p * 2] + c) * s, (k[p * 2 + 1] + r) * s]);
            faces.push(Face { area: [cx - w / 2.0, cy - h / 2.0, cx + w / 2.0, cy + h / 2.0], points, score });
        }
    }
    faces.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut kept: Vec<Face> = Vec::new();
    for face in faces {
        if kept.iter().all(|k| box_iou(k.area, face.area) <= FACES.nms_iou) {
            kept.push(face);
        }
    }
    Ok(kept)
}

/// The rotation, scale and shift `[[a, -b, tx], [b, a, ty]]` that best maps `src` onto `dst` in
/// the least-squares sense, without reflection (origin `similarity_transform`, Umeyama).
pub fn similarity_transform(src: &[[f32; 2]; 5], dst: &[[f32; 2]; 5]) -> [[f32; 3]; 2] {
    let mean = |p: &[[f32; 2]; 5]| p.iter().fold([0.0f64; 2], |m, q| [m[0] + f64::from(q[0]) / 5.0, m[1] + f64::from(q[1]) / 5.0]);
    let (ms, md) = (mean(src), mean(dst));
    let (mut re, mut im, mut den) = (0.0, 0.0, 0.0);
    for (s, d) in src.iter().zip(dst) {
        let (sx, sy) = (f64::from(s[0]) - ms[0], f64::from(s[1]) - ms[1]);
        let (dx, dy) = (f64::from(d[0]) - md[0], f64::from(d[1]) - md[1]);
        re += sx * dx + sy * dy;
        im += sx * dy - sy * dx;
        den += sx * sx + sy * sy;
    }
    let (a, b) = if den > 0.0 { (re / den, im / den) } else { (1.0, 0.0) };
    let tx = md[0] - (a * ms[0] - b * ms[1]);
    let ty = md[1] - (b * ms[0] + a * ms[1]);
    [[a as f32, -b as f32, tx as f32], [b as f32, a as f32, ty as f32]]
}

/// The face turned and scaled so that its five points sit on the template (origin `align`).
pub fn align(image: &egui::ColorImage, face: &Face) -> Rgb {
    let template = TEMPLATE.map(|[x, y]| [x * ALIGNED as f32 / 112.0, y * ALIGNED as f32 / 112.0]);
    pixels::warp(image, similarity_transform(&face.points, &template), ALIGNED)
}

/// Variance of the Laplacian of an aligned face in grey: low is blurry.
pub fn sharpness(aligned: &Rgb) -> f32 {
    let g = pixels::grey(aligned);
    let [w, h] = aligned.size;
    let at = |x: usize, y: usize| f64::from(g[y * w + x]);
    let lap: Vec<f64> = (1..h - 1)
        .flat_map(|y| (1..w - 1).map(move |x| (x, y)))
        .map(|(x, y)| 4.0 * at(x, y) - at(x, y - 1) - at(x, y + 1) - at(x - 1, y) - at(x + 1, y))
        .collect();
    let mean = lap.iter().sum::<f64>() / lap.len() as f64;
    (lap.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / lap.len() as f64) as f32
}

/// How far the face is turned sideways: the nose's offset from between the eyes along the line
/// of the eyes, in eye distances (0 straight at the camera, about 0.35 at 30°, 0.6 at 45°).
pub fn turn(face: &Face) -> f32 {
    let [[ax, ay], [bx, by], [nx, ny], ..] = face.points;
    let dist = (bx - ax).hypot(by - ay);
    if dist <= 0.0 {
        return 1.0;
    }
    let (ux, uy) = ((bx - ax) / dist, (by - ay) / dist);
    let (mx, my) = ((ax + bx) / 2.0, (ay + by) / 2.0);
    ((nx - mx) * ux + (ny - my) * uy).abs() / dist
}

/// Whether the frame's edge cuts the face: the face detector then guesses where the eyes are, and
/// the face looks like nobody.
pub fn cut_off(face: &Face, [w, h]: [usize; 2]) -> bool {
    let (w, h) = (w as f32, h as f32);
    let [x1, y1, x2, y2] = face.area;
    let inside = face.points.iter().all(|[x, y]| (0.0..w).contains(x) && (0.0..h).contains(y));
    !inside || x1 <= 0.0 || y1 <= 0.0 || x2 >= w - 1.0 || y2 >= h - 1.0
}

/// Why a face in a frame of size `frame` cannot be used, or `None` (origin `problem`).
pub fn problem(face: &Face, aligned: &Rgb, min_face_px: f32, frame: [usize; 2]) -> Option<FaceProblem> {
    if cut_off(face, frame) {
        Some(FaceProblem::Cut)
    } else if face.width() < min_face_px {
        Some(FaceProblem::Small)
    } else if face.score < FACES.min_score {
        Some(FaceProblem::Unsure)
    } else if turn(face) > FACES.max_turn || face.eye_distance() < face.width() * FACES.turned_eye_share {
        Some(FaceProblem::Turned)
    } else if sharpness(aligned) < FACES.min_sharpness {
        Some(FaceProblem::Blurry)
    } else {
        None
    }
}

/// Seen well enough that matching nobody makes the face a stranger: big enough, nearly straight
/// at the camera, and surely a face (origin `clearly_seen`).
pub fn clearly_seen(face: &Face, min_face_px: f32) -> bool {
    face.width() >= min_face_px * FACES.unknown_size && turn(face) <= FACES.unknown_max_turn && face.score >= FACES.unknown_min_score
}

/// The face with its brightness spread over the full range, its colours kept (origin `even_light`:
/// `ImageOps.equalize` of the Y of YCbCr). A face against a bright sky is nearly black otherwise.
pub fn even_light(face: &Rgb) -> Rgb {
    let ycc: Vec<[f32; 3]> = face
        .pixels
        .as_chunks::<3>()
        .0
        .iter()
        .map(|&[r, g, b]| {
            let (r, g, b) = (f32::from(r), f32::from(g), f32::from(b));
            let y = (0.299 * r + 0.587 * g + 0.114 * b).round().clamp(0.0, 255.0);
            [y, -0.168736 * r - 0.331264 * g + 0.5 * b + 128.0, 0.5 * r - 0.418688 * g - 0.081312 * b + 128.0]
        })
        .collect();
    let mut histogram = [0usize; 256];
    ycc.iter().for_each(|[y, ..]| histogram[*y as usize] += 1);
    let last = histogram.iter().rev().find(|n| **n > 0).copied().unwrap_or(0);
    let used = histogram.iter().filter(|n| **n > 0).count();
    let step = (ycc.len() - last) / 255;
    let lut: [f32; 256] = if used <= 1 || step == 0 {
        std::array::from_fn(|i| i as f32)
    } else {
        let mut n = step / 2;
        std::array::from_fn(|i| {
            let v = (n / step).min(255) as f32;
            n += histogram[i];
            v
        })
    };
    let pixels = ycc
        .iter()
        .flat_map(|&[y, cb, cr]| {
            let y = lut[y as usize];
            [y + 1.402 * (cr - 128.0), y - 0.344136 * (cb - 128.0) - 0.714136 * (cr - 128.0), y + 1.772 * (cb - 128.0)]
                .map(|v| v.round().clamp(0.0, 255.0) as u8)
        })
        .collect();
    Rgb { size: face.size, pixels }
}

/// Where a taught face is cut from its frame: the face with `FACES.crop_margin` around it, not
/// aligned, so another model can align it its own way later (origin `face_crop`).
pub fn crop_area(face: &Face, [width, height]: [usize; 2]) -> [usize; 4] {
    let [x1, y1, x2, y2] = face.area;
    let m = (x2 - x1).max(y2 - y1) * FACES.crop_margin;
    [
        (x1 - m).max(0.0) as usize,
        (y1 - m).max(0.0) as usize,
        ((x2 + m).ceil() as usize).min(width),
        ((y2 + m).ceil() as usize).min(height),
    ]
}

/// The people's taught faces, shared by every sensor, embedded by one recogniser.
pub struct FaceIndex {
    pub recogniser: Recogniser,
    rows: Vec<(PersonId, Vec<f32>)>,
}

impl FaceIndex {
    pub fn new(recogniser: Recogniser, rows: Vec<(PersonId, Vec<f32>)>) -> Self {
        Self { recogniser, rows }
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// Who a face is: the closest person (`None`: nobody taught), their score, and the margin over
/// the next person (1 when there is only one).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Identity {
    pub person: Option<PersonId>,
    pub similarity: f32,
    pub margin: f32,
}

impl Identity {
    pub const NOBODY: Self = Self { person: None, similarity: 0.0, margin: 0.0 };
}

/// Each taught person's score: the mean of their `FACES.top_k` closest faces, so one face taught
/// under the wrong name does not match everything like it (origin `scores`, `identify`).
pub fn identify(index: &FaceIndex, vector: &[f32]) -> Identity {
    let mut per: BTreeMap<PersonId, Vec<f32>> = BTreeMap::new();
    for (person, taught) in &index.rows {
        per.entry(*person).or_default().push(taught.iter().zip(vector).map(|(a, b)| a * b).sum());
    }
    let mut ranked: Vec<(PersonId, f32)> = per
        .into_iter()
        .map(|(person, mut values)| {
            values.sort_by(|a, b| b.total_cmp(a));
            let top = &values[..values.len().min(FACES.top_k)];
            (person, top.iter().sum::<f32>() / top.len() as f32)
        })
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    let Some(&(person, score)) = ranked.first() else { return Identity::NOBODY };
    let margin = ranked.get(1).map_or(1.0, |next| score - next.1);
    let round = |v: f32| (v * 1000.0).round() / 1000.0;
    Identity { person: Some(person), similarity: round(score), margin: round(margin) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Match,
    Ask,
    Unknown,
}

/// What a usable face is (origin `decide`). `kept`: the name this box had in the last check; it
/// keeps it while the face stays close enough.
pub fn decide(identity: Identity, sims: &Similarities, kept: Option<PersonId>) -> Decision {
    let Some(person) = identity.person else { return Decision::Unknown };
    let kept_close = kept == Some(person) && identity.similarity >= sims.keep;
    if kept_close || (identity.similarity >= sims.matched && identity.margin >= FACES.margin) {
        Decision::Match
    } else if identity.similarity >= sims.ask {
        Decision::Ask
    } else {
        Decision::Unknown
    }
}

/// The last check's person box that is this one: it overlaps by `TEACH.keep_iou` (origin
/// `same_box`).
pub fn same_box(previous: &[Found], area: [f32; 4]) -> Option<&Found> {
    previous
        .iter()
        .filter(|f| f.class == ObjectClass::PERSON || matches!(f.verdict, Verdict::Taught { was: Some(ObjectClass::PERSON), .. }))
        .max_by(|a, b| box_iou(a.area, area).total_cmp(&box_iou(b.area, area)))
        .filter(|f| box_iou(f.area, area) >= TEACH.keep_iou)
}

/// What a box looked like last check, one check older.
fn carried(before: FaceLook) -> FaceLook {
    FaceLook { age: before.age.saturating_add(1), ..before }
}

/// Which person boxes to look for a face in now, at most `FACES.max_faces`: new boxes first,
/// then those looked at longest ago, the most certain first, so in a crowd everyone gets a turn.
/// A box with a sure name skips `FACES.recheck_every` checks; a box not looked at keeps its look
/// (origin `apply_faces`, before the face detector runs).
pub fn plan_search(found: &mut [Found], previous: &[Found]) -> Vec<usize> {
    let mut waiting: Vec<(usize, Option<FaceLook>)> = Vec::new();
    for (i, f) in found.iter_mut().enumerate() {
        if f.class != ObjectClass::PERSON || f.filtered() {
            continue;
        }
        let before = same_box(previous, f.area).and_then(|b| b.face);
        match before {
            Some(b) if matches!(b.state, FaceState::Match { sure: true, .. }) && b.age + 1 < FACES.recheck_every => {
                f.face = Some(carried(b));
            }
            _ => waiting.push((i, before)),
        }
    }
    waiting.sort_by(|(a, before_a), (b, before_b)| {
        let age = |look: &Option<FaceLook>| look.map_or(0, |l| l.age);
        (before_a.is_some(), std::cmp::Reverse(age(before_a)))
            .cmp(&(before_b.is_some(), std::cmp::Reverse(age(before_b))))
            .then(found[*b].score.total_cmp(&found[*a].score))
    });
    let mut searched = Vec::new();
    for (i, before) in waiting {
        if searched.len() < FACES.max_faces {
            searched.push(i);
        } else if let Some(before) = before {
            found[i].face = Some(carried(before));
        }
    }
    searched
}

/// A box just looked at: where its face is (in the frame's pixels), and why it cannot be used.
pub fn looked(face: Option<&Face>, problem: Option<FaceProblem>, frame: [usize; 2]) -> FaceLook {
    FaceLook {
        marks: face.map(|f| f.marks(frame)),
        width_px: face.map_or(0.0, Face::width),
        problem,
        state: FaceState::Open,
        similarity: 0.0,
        candidate: None,
        unknown: 0,
        named: false,
        age: 0,
    }
}

/// Who the usable face of a person box is (origin `_name_face`). `clear`: seen well enough that
/// matching nobody counts towards the unknown person; `taught`: someone was taught at all, else
/// there is no unknown person yet.
pub fn name_face(look: &mut FaceLook, before: Option<&FaceLook>, identity: Identity, sims: &Similarities, clear: bool, taught: bool) {
    let named_before = before.and_then(FaceLook::person);
    let decision = decide(identity, sims, named_before);
    look.named = before.is_some_and(|b| b.named) || named_before.is_some();
    look.similarity = identity.similarity;
    look.candidate = identity.person;
    look.age = 0;
    let unknown_before = before.map_or(0, |b| b.unknown);
    match (decision, identity.person) {
        (Decision::Match, Some(person)) => {
            let agreed = named_before == Some(person) || before.and_then(|b| b.candidate) == Some(person);
            let sure = identity.similarity >= sims.sure;
            if sure || agreed {
                look.state = FaceState::Match { person, sure, kept: false };
                look.named = true;
            } else {
                look.state = FaceState::Pending(person);
            }
        }
        (Decision::Ask, Some(person)) => look.state = FaceState::Ask(person),
        _ if !clear => {
            look.state = FaceState::Unclear;
            look.unknown = unknown_before;
        }
        _ => {
            look.state = FaceState::Unknown;
            if taught {
                look.unknown = unknown_before.saturating_add(1);
            }
        }
    }
}

/// A box without a usable face this check keeps the name it had, or how long it matched nobody
/// (origin `_keep_name`).
pub fn keep_name(look: &mut FaceLook, before: Option<&FaceLook>) {
    let Some(before) = before else { return };
    if let Some(person) = before.person() {
        look.state = FaceState::Match { person, sure: false, kept: true };
        look.named = true;
        look.similarity = before.similarity;
    } else if before.unknown > 0 {
        look.unknown = before.unknown;
        look.named = before.named;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sensors::types::{SampleId, TaughtMatch, BoxLabel, MatchHow, UNKNOWN_AFTER};
    use egui::{Color32, ColorImage};

    const FRAME: [usize; 2] = [640, 480];

    fn face_at(points: [[f32; 2]; 5], score: f32) -> Face {
        let xs = points.map(|p| p[0]);
        let ys = points.map(|p| p[1]);
        let min = |v: [f32; 5]| v.into_iter().fold(f32::MAX, f32::min);
        let max = |v: [f32; 5]| v.into_iter().fold(f32::MIN, f32::max);
        Face { area: [min(xs) - 10.0, min(ys) - 20.0, max(xs) + 10.0, max(ys) + 10.0], points, score }
    }

    /// Deterministic noise around grey, like PIL's `effect_noise` with that deviation.
    fn noise(side: usize, spread: f32, seed: u32) -> Rgb {
        let mut state = seed;
        let mut next = || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 8) as f32 / (1u32 << 24) as f32
        };
        let pixels = (0..side * side)
            .flat_map(|_| {
                let v = (128.0 + (next() - 0.5) * 3.4 * spread).round().clamp(0.0, 255.0) as u8;
                [v, v, v]
            })
            .collect();
        Rgb { size: [side, side], pixels }
    }

    #[test]
    fn align_puts_the_five_points_on_the_template() {
        let (sin, cos) = 20f32.to_radians().sin_cos();
        let points = TEMPLATE.map(|[x, y]| [(x * cos - y * sin) * 2.5 + 200.0, (x * sin + y * cos) * 2.5 + 150.0]);
        let m = similarity_transform(&points, &TEMPLATE);
        for (p, t) in points.iter().zip(TEMPLATE) {
            let mapped = [m[0][0] * p[0] + m[0][1] * p[1] + m[0][2], m[1][0] * p[0] + m[1][1] * p[1] + m[1][2]];
            assert!((mapped[0] - t[0]).abs() < 1e-3 && (mapped[1] - t[1]).abs() < 1e-3, "{mapped:?} vs {t:?}");
        }
        let mut image = ColorImage::filled([500, 500], Color32::BLACK);
        for [x, y] in points {
            let (x, y) = (x.round() as usize, y.round() as usize);
            for yy in y - 5..=y + 5 {
                for xx in x - 5..=x + 5 {
                    image.pixels[yy * 500 + xx] = Color32::WHITE;
                }
            }
        }
        let aligned = align(&image, &face_at(points, 0.9));
        let grey = pixels::grey(&aligned);
        for [x, y] in TEMPLATE {
            assert!(grey[y.round() as usize * ALIGNED + x.round() as usize] > 128, "a dot lands on its template point");
        }
    }

    #[test]
    fn even_light_spreads_a_dark_face_over_the_full_range() {
        let mut dark = noise(112, 20.0, 7);
        for rgb in dark.pixels.as_chunks_mut::<3>().0 {
            let v = 20 + rgb[1] / 8;
            *rgb = [v + 10, v, v];
        }
        let even = even_light(&dark);
        let grey = pixels::grey(&even);
        let (low, high) = (grey.iter().min().unwrap(), grey.iter().max().unwrap());
        assert!(even.size == dark.size && *low < 40 && *high > 215, "{low}..{high}");
        let mean = |c: usize| even.pixels.as_chunks::<3>().0.iter().map(|p| f32::from(p[c])).sum::<f32>();
        assert!(mean(0) > mean(1) && mean(0) > mean(2), "a red face stays red");
    }

    #[test]
    fn turn_eye_distance_and_quality_gate() {
        let straight = face_at([[40.0, 50.0], [80.0, 50.0], [60.0, 70.0], [45.0, 90.0], [75.0, 90.0]], 0.9);
        let half = face_at([[40.0, 50.0], [80.0, 50.0], [75.0, 70.0], [45.0, 90.0], [75.0, 90.0]], 0.9);
        let sideways = face_at([[40.0, 50.0], [80.0, 50.0], [85.0, 70.0], [45.0, 90.0], [75.0, 90.0]], 0.9);
        assert_eq!((straight.eye_distance(), straight.width()), (40.0, 60.0));
        assert!(turn(&straight) == 0.0 && turn(&sideways) > FACES.max_turn);
        let sharp = noise(112, 60.0, 3);
        assert_eq!(problem(&straight, &sharp, 30.0, FRAME), None);
        assert_eq!(problem(&straight, &sharp, 61.0, FRAME), Some(FaceProblem::Small), "the face's width, not its eyes");
        assert_eq!(problem(&half, &sharp, 30.0, FRAME), None);
        assert_eq!(problem(&sideways, &sharp, 30.0, FRAME), Some(FaceProblem::Turned));
        let profile = Face { area: [10.0, 10.0, 70.0, 80.0], points: [[40.0, 40.0], [44.0, 40.0], [30.0, 55.0], [38.0, 65.0], [42.0, 65.0]], score: 0.9 };
        assert_eq!(problem(&profile, &sharp, 30.0, FRAME), Some(FaceProblem::Turned), "eyes close together for the face");
        assert_eq!(problem(&profile, &sharp, 61.0, FRAME), Some(FaceProblem::Small));
        assert_eq!(problem(&Face { score: 0.5, ..straight }, &sharp, 30.0, FRAME), Some(FaceProblem::Unsure));
        let flat = Rgb { size: [112, 112], pixels: vec![90; 112 * 112 * 3] };
        assert_eq!(problem(&straight, &flat, 30.0, FRAME), Some(FaceProblem::Blurry));
        assert!(clearly_seen(&straight, 30.0) && !clearly_seen(&straight, 45.0));
        assert!(!clearly_seen(&half, 30.0), "turned a little: may be named, never a stranger");
        assert!(!clearly_seen(&Face { score: 0.8, ..straight }, 30.0));
        assert_eq!(problem(&straight, &sharp, 30.0, [200, 85]), Some(FaceProblem::Cut));
        let top = Face { area: [143.0, 0.0, 258.0, 74.0], points: [[174.0, -4.0], [225.0, 0.0], [198.0, 17.0], [175.0, 35.0], [214.0, 38.0]], score: 0.79 };
        assert!(cut_off(&top, [480, 640]) && !cut_off(&straight, FRAME));
    }

    #[test]
    fn head_region_is_the_top_of_a_standing_person_and_all_of_a_close_up() {
        let standing = head_region([0.4, 0.1, 0.5, 0.9], [1000, 1000]);
        assert!(standing[1] < 100 && (standing[3] as f32) < 100.0 + 1.3 * 100.0 + 20.0, "{standing:?}");
        let close_up = head_region([0.2, 0.2, 0.6, 0.6], [1000, 1000]);
        assert!(close_up[3] >= 600, "{close_up:?}");
    }

    fn unit(v: [f32; 3]) -> Vec<f32> {
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter().map(|x| x / norm).collect()
    }

    const KARI: PersonId = PersonId(1);
    const EVA: PersonId = PersonId(2);
    const SIMS: Similarities = similarities(Recogniser::Sface);

    fn index(entries: &[(PersonId, [f32; 3])]) -> FaceIndex {
        FaceIndex::new(Recogniser::Sface, entries.iter().map(|(p, v)| (*p, unit(*v))).collect())
    }

    #[test]
    fn a_person_is_the_mean_of_their_closest_faces() {
        let (kari, eva) = ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
        let idx = index(&[(KARI, kari), (KARI, kari), (KARI, kari), (EVA, eva), (EVA, eva), (EVA, kari)]);
        let who = identify(&idx, &unit(kari));
        assert_eq!((who.person, who.similarity), (Some(KARI), 1.0));
        assert!((who.margin - (1.0 - 1.0 / 3.0)).abs() < 1e-3, "Eva's score is her mean, not the face taught by mistake");
        assert_eq!(decide(who, &SIMS, None), Decision::Match);
        assert_eq!(decide(identify(&idx, &unit([0.0, 0.0, 1.0])), &SIMS, None), Decision::Unknown);
        assert_eq!(decide(Identity::NOBODY, &SIMS, None), Decision::Unknown);
        let between = Identity { person: Some(KARI), similarity: (SIMS.ask + SIMS.matched) / 2.0, margin: 0.3 };
        assert_eq!(decide(between, &SIMS, None), Decision::Ask);
        let two = Identity { person: Some(KARI), similarity: 0.9, margin: 0.01 };
        assert_eq!(decide(two, &SIMS, None), Decision::Ask, "close to two people at once");
        let staying = Identity { person: Some(KARI), similarity: SIMS.keep + 0.01, margin: 0.3 };
        assert_eq!(decide(staying, &SIMS, Some(KARI)), Decision::Match, "a person who stays keeps the name");
        assert_ne!(decide(staying, &SIMS, None), Decision::Match);
    }

    fn person_box(face: Option<FaceLook>) -> Found {
        Found { class: ObjectClass::PERSON, score: 0.9, area: [0.1, 0.1, 0.5, 0.9], verdict: Verdict::Detector, face }
    }

    #[test]
    fn only_a_clearly_seen_face_that_matches_nobody_is_a_stranger() {
        let idx = index(&[(KARI, [1.0, 0.0, 0.0])]);
        let who = identify(&idx, &unit([0.0, 0.0, 1.0]));
        let check = |clear: bool, before: Option<FaceLook>| {
            let mut look = looked(None, None, FRAME);
            name_face(&mut look, before.as_ref(), who, &SIMS, clear, true);
            person_box(Some(look))
        };
        let first = check(false, None);
        assert_eq!(first.face.unwrap().state, FaceState::Unclear);
        assert!(!first.counts_for(crate::sensors::types::ObjectKey::Stranger), "turned or small may be the angle");
        assert!(!check(false, first.face).face.unwrap().stranger());
        let twice = check(true, check(true, None).face);
        assert!(twice.face.unwrap().stranger() && twice.face.unwrap().unknown >= UNKNOWN_AFTER, "seen clearly twice");
        assert!(check(false, twice.face).face.unwrap().stranger(), "and stays one while turned away");
        let mut first = looked(None, None, FRAME);
        name_face(&mut first, None, Identity::NOBODY, &SIMS, true, false);
        let mut second = looked(None, None, FRAME);
        name_face(&mut second, Some(&first), Identity::NOBODY, &SIMS, true, false);
        assert_eq!((second.state, second.unknown), (FaceState::Unknown, 0), "nobody taught: there is no unknown person yet");
    }

    #[test]
    fn a_name_needs_a_sure_match_or_two_checks_that_agree_and_stays_without_a_face() {
        let named = |similarity: f32| Identity { person: Some(KARI), similarity, margin: 0.5 };
        let mut first = looked(None, None, FRAME);
        name_face(&mut first, None, named(0.55), &SIMS, true, true);
        assert_eq!(first.state, FaceState::Pending(KARI), "a match below sure waits for a second check");
        let mut second = looked(None, None, FRAME);
        name_face(&mut second, Some(&first), named(0.55), &SIMS, true, true);
        assert_eq!(second.state, FaceState::Match { person: KARI, sure: false, kept: false });
        let mut turned_away = looked(None, Some(FaceProblem::Turned), FRAME);
        keep_name(&mut turned_away, Some(&second));
        assert_eq!(turned_away.person(), Some(KARI), "no usable face now: the box keeps its name");
        let mut sure = looked(None, None, FRAME);
        name_face(&mut sure, None, named(0.7), &SIMS, true, true);
        assert_eq!(sure.state, FaceState::Match { person: KARI, sure: true, kept: false });
        let mut maybe = looked(None, None, FRAME);
        name_face(&mut maybe, None, named(0.4), &SIMS, true, true);
        assert_eq!(maybe.state, FaceState::Ask(KARI));
        assert!(person_box(Some(maybe)).unsure_for(crate::sensors::types::ObjectKey::Person(KARI)));
    }

    #[test]
    fn a_sure_name_is_carried_and_at_most_four_boxes_are_looked_at() {
        let sure = FaceLook { state: FaceState::Match { person: KARI, sure: true, kept: false }, named: true, ..looked(None, None, FRAME) };
        let previous = [person_box(Some(sure))];
        let mut found = [person_box(None)];
        assert!(plan_search(&mut found, &previous).is_empty(), "a sure name skips the next checks");
        assert_eq!(found[0].face.unwrap().age, 1);
        let previous = [person_box(Some(FaceLook { age: FACES.recheck_every - 1, ..sure }))];
        let mut found = [person_box(None)];
        assert_eq!(plan_search(&mut found, &previous), [0], "then it is looked at again");
        let boxes: Vec<Found> = (0..6)
            .map(|i| Found { area: [i as f32 * 0.15, 0.1, i as f32 * 0.15 + 0.1, 0.9], score: 0.5 + i as f32 * 0.05, ..person_box(None) })
            .collect();
        let mut crowd = boxes.clone();
        let searched = plan_search(&mut crowd, &[]);
        assert_eq!(searched, [5, 4, 3, 2], "most certain first, four per check");
        let filtered = Found { verdict: Verdict::Filtered(TaughtMatch { sample: SampleId(1), label: BoxLabel::NotThis, similarity: 0.9 }), ..person_box(None) };
        assert!(plan_search(&mut [filtered], &[]).is_empty(), "a box taught as not a person has no face");
        let taught_person = Found { verdict: Verdict::Taught { by: TaughtMatch { sample: SampleId(1), label: BoxLabel::Class(ObjectClass::PERSON), similarity: 0.9 }, label: None, was: Some(ObjectClass::PERSON), how: MatchHow::Clear }, ..person_box(Some(sure)) };
        assert!(same_box(&[taught_person], [0.1, 0.1, 0.5, 0.9]).is_some());
    }

    #[test]
    fn yunet_outputs_become_faces_in_input_pixels() {
        let size = 64;
        let mut outputs = Vec::new();
        for kind in 0..4 {
            for stride in [8usize, 16, 32] {
                let cells = (size / stride) * (size / stride);
                let per = [1, 1, 4, 10][kind];
                outputs.push(Output { shape: vec![1, cells, per], values: vec![0.0; cells * per] });
            }
        }
        let (cell, cols, stride) = (9usize, 8usize, 8.0f32);
        outputs[0].values[cell] = 0.9;
        outputs[3].values[cell] = 0.9;
        outputs[6].values[cell * 4..cell * 4 + 4].copy_from_slice(&[0.5, 0.5, 2f32.ln(), 3f32.ln()]);
        outputs[9].values[cell * 10..cell * 10 + 10].copy_from_slice(&[0.0, 0.0, 1.0, 0.0, 0.5, 0.5, 0.0, 1.0, 1.0, 1.0]);
        let faces = decode_yunet(&outputs, size, 0.6).unwrap();
        assert_eq!(faces.len(), 1);
        let (r, c) = ((cell / cols) as f32, (cell % cols) as f32);
        let (cx, cy) = ((c + 0.5) * stride, (r + 0.5) * stride);
        assert_eq!(faces[0].area, [cx - 8.0, cy - 12.0, cx + 8.0, cy + 12.0]);
        assert_eq!(faces[0].points[1], [(c + 1.0) * stride, r * stride]);
        assert!((faces[0].score - 0.9).abs() < 1e-6);
        assert!(decode_yunet(&outputs[..11], size, 0.6).is_err());
    }
}
