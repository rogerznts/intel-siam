//! Teaching object sensors, pure (origin `teach.py`). A taught box is a sample: the crop of a box
//! with what it really is. Every check compares the boxes of the taught classes with the taught
//! ones and gives a box the label of the closest taught box when it is clearly that one. Without a
//! clear match the detector's answer stands, so a sensor behaves as before until it is taught.
//!
//! Boxes the detector missed (drawn by the user) are found again among the boxes it was unsure
//! about: those are compared too, and count only when very close to a taught box.

use std::collections::{BTreeMap, BTreeSet};

use crate::sensors::types::{BoxLabel, ClassSet, Found, LabelId, MatchHow, ObjectClass, SampleId, TaughtMatch, Verdict};

/// Origin `TEACH`, without per-sensor overrides. Measured there on real photos: the same object in
/// other light or framing scores 0.87-0.97, other objects of the same kind mostly below 0.7.
pub struct TeachLimits {
    pub match_similarity: f32,
    /// The closest label must beat the next one by this much, else the detector decides.
    pub margin: f32,
    /// An object that stays where it was keeps its answer while still this close to it.
    pub keep_similarity: f32,
    pub keep_iou: f32,
    /// Between this and `match_similarity` a box of an own label's class is unsure, and asked about.
    pub ask_similarity: f32,
    /// A box the detector was unsure about counts only when this close to a taught one.
    pub rescue_similarity: f32,
    /// The weakest boxes looked at again, once a box the detector missed was taught.
    pub rescue_floor: f32,
    pub max_checked: usize,
    pub max_rescue: usize,
    pub rescue_overlap_iou: f32,
    /// Share of a box's size added on each side of what is compared.
    pub crop_margin: f32,
}

pub const TEACH: TeachLimits = TeachLimits {
    match_similarity: 0.88,  // TEACH["match_similarity"]
    margin: 0.03,            // TEACH["margin"]
    keep_similarity: 0.8,    // TEACH["keep_similarity"]
    keep_iou: 0.6,           // TEACH["keep_iou"]
    ask_similarity: 0.75,    // TEACH["ask_similarity"]
    rescue_similarity: 0.9,  // TEACH["rescue_similarity"]
    rescue_floor: 0.25,      // TEACH["rescue_floor"]
    max_checked: 6,          // TEACH["max_checked"]
    max_rescue: 4,           // TEACH["max_rescue"]
    rescue_overlap_iou: 0.5, // TEACH["rescue_overlap_iou"]
    crop_margin: 0.1,        // TEACH["crop_margin"]
};

/// One taught box with its embedding (L2-normalised).
pub struct TaughtRow {
    pub sample: SampleId,
    pub label: BoxLabel,
    /// The detector's class for it; `None` for a box drawn around something it missed.
    pub detected: Option<ObjectClass>,
    pub vector: Vec<f32>,
}

/// The taught boxes of one sensor, ready to compare new boxes with (origin `TaughtIndex`).
pub struct TaughtIndex {
    rows: Vec<TaughtRow>,
    /// Classes whose boxes are compared.
    keys: BTreeSet<ObjectClass>,
    /// A missed box was taught: unsure boxes are compared too.
    pub rescue: bool,
    /// Own label → the class it is a kind of, for the labels in use.
    parents: BTreeMap<LabelId, ObjectClass>,
}

/// The index of the taught boxes whose label still means something, or `None` when none does (an
/// own label not in use is ignored, origin `usable`).
pub fn build(rows: Vec<TaughtRow>, parents: BTreeMap<LabelId, ObjectClass>) -> Option<TaughtIndex> {
    let rows: Vec<TaughtRow> =
        rows.into_iter().filter(|r| !matches!(r.label, BoxLabel::Own(id) if !parents.contains_key(&id))).collect();
    if rows.is_empty() {
        return None;
    }
    let keys = rows.iter().flat_map(|r| [r.detected, resolve(r.label, &parents).0]).flatten().collect();
    let rescue = rows.iter().any(|r| r.detected.is_none());
    Some(TaughtIndex { rows, keys, rescue, parents })
}

/// The class and own label a taught label stands for; `(None, None)` for `NotThis`.
fn resolve(label: BoxLabel, parents: &BTreeMap<LabelId, ObjectClass>) -> (Option<ObjectClass>, Option<LabelId>) {
    match label {
        BoxLabel::NotThis => (None, None),
        BoxLabel::Class(class) => (Some(class), None),
        BoxLabel::Own(id) => (parents.get(&id).copied(), Some(id)),
    }
}

/// The taught label this box clearly is: at least `min_similarity`, and ahead of the next label
/// by `TEACH.margin` (origin `nearest`).
pub fn nearest(index: &TaughtIndex, vector: &[f32], min_similarity: f32) -> Option<TaughtMatch> {
    let mut best: BTreeMap<BoxLabel, (f32, SampleId)> = BTreeMap::new();
    for row in &index.rows {
        let similarity: f32 = row.vector.iter().zip(vector).map(|(a, b)| a * b).sum();
        if best.get(&row.label).is_none_or(|(s, _)| similarity > *s) {
            best.insert(row.label, (similarity, row.sample));
        }
    }
    let mut ranked: Vec<(BoxLabel, (f32, SampleId))> = best.into_iter().collect();
    ranked.sort_by(|a, b| b.1.0.total_cmp(&a.1.0));
    let (label, (similarity, sample)) = *ranked.first()?;
    if similarity < min_similarity || ranked.get(1).is_some_and(|(_, (next, _))| similarity - next < TEACH.margin) {
        return None;
    }
    Some(TaughtMatch { sample, label, similarity })
}

/// Intersection over union of two `[x1, y1, x2, y2]` boxes.
pub fn box_iou(a: [f32; 4], b: [f32; 4]) -> f32 {
    let inter = (a[2].min(b[2]) - a[0].max(b[0])).max(0.0) * (a[3].min(b[3]) - a[1].max(b[1])).max(0.0);
    let union = (a[2] - a[0]) * (a[3] - a[1]) + (b[2] - b[0]) * (b[3] - b[1]) - inter;
    if union > 0.0 { inter / union } else { 0.0 }
}

/// What is compared of a box: the box with `TEACH.crop_margin` of its surroundings (origin `crop`).
pub fn crop_area(area: [f32; 4]) -> [f32; 4] {
    let [x1, y1, x2, y2] = area;
    let (mx, my) = ((x2 - x1) * TEACH.crop_margin, (y2 - y1) * TEACH.crop_margin);
    [(x1 - mx).max(0.0), (y1 - my).max(0.0), (x2 + mx).min(1.0), (y2 + my).min(1.0)]
}

/// Which counted boxes to compare (indices into `found`, most certain first) and which unsure boxes
/// to look at again (origin `plan`).
pub fn plan(found: &[Found], candidates: &[Found], index: &TaughtIndex) -> (Vec<usize>, Vec<Found>) {
    let mut checked: Vec<usize> = (0..found.len()).filter(|i| index.keys.contains(&found[*i].class)).collect();
    checked.sort_by(|a, b| found[*b].score.total_cmp(&found[*a].score));
    checked.truncate(TEACH.max_checked);
    let mut rescue = Vec::new();
    if index.rescue {
        let mut sorted: Vec<&Found> = candidates.iter().collect();
        sorted.sort_by(|a, b| b.score.total_cmp(&a.score));
        for cand in sorted {
            if found.iter().any(|f| box_iou(cand.area, f.area) >= TEACH.rescue_overlap_iou) {
                continue;
            }
            rescue.push(*cand);
            if rescue.len() >= TEACH.max_rescue {
                break;
            }
        }
    }
    (checked, rescue)
}

/// The boxes of a check after comparing them with the taught ones (origin `apply`). `checked`
/// pairs with `checked_vectors`, `rescue` with `rescue_vectors`; `previous` is the last check's
/// boxes, so an object that stayed where it was keeps its answer at `keep_similarity`.
pub fn apply(
    found: &[Found],
    checked: &[(usize, Vec<f32>)],
    rescue: &[(Found, Vec<f32>)],
    index: &TaughtIndex,
    classes: &ClassSet,
    previous: &[Found],
) -> Vec<Found> {
    let mut result = found.to_vec();
    for (i, vector) in checked {
        let det = &mut result[*i];
        let (found_match, how) = match nearest(index, vector, TEACH.match_similarity) {
            Some(m) => (Some(m), MatchHow::Clear),
            None => (kept(index, vector, det.area, previous), MatchHow::Kept),
        };
        let Some(by) = found_match else {
            if let Some(verdict) = unsure(index, vector, det.class) {
                det.verdict = verdict;
            }
            continue;
        };
        let (class, label) = resolve(by.label, &index.parents);
        if class == Some(det.class) && label.is_none() {
            continue;
        }
        det.verdict = match class.filter(|c| classes.contains(*c)) {
            None => Verdict::Filtered(by),
            Some(class) => {
                let was = (class != det.class).then_some(det.class);
                det.class = class;
                Verdict::Taught { by, label, was, how }
            }
        };
    }
    for (cand, vector) in rescue {
        let Some(by) = nearest(index, vector, TEACH.rescue_similarity) else { continue };
        let (class, label) = resolve(by.label, &index.parents);
        let Some(class) = class.filter(|c| classes.contains(*c)) else { continue };
        if result.iter().any(|f| !f.filtered() && box_iou(cand.area, f.area) >= TEACH.rescue_overlap_iou) {
            continue;
        }
        let was = (class != cand.class).then_some(cand.class);
        result.push(Found { class, verdict: Verdict::Taught { by, label, was, how: MatchHow::Rescued }, ..*cand });
    }
    result
}

/// The answer a box got in the last check, when it is the same object at the same place and still
/// close to that taught label (origin `_kept`).
fn kept(index: &TaughtIndex, vector: &[f32], area: [f32; 4], previous: &[Found]) -> Option<TaughtMatch> {
    let before: Vec<BoxLabel> = previous
        .iter()
        .filter(|f| box_iou(f.area, area) >= TEACH.keep_iou)
        .filter_map(|f| match f.verdict {
            Verdict::Filtered(by) => Some(by.label),
            Verdict::Taught { by, how, .. } if how != MatchHow::Rescued => Some(by.label),
            _ => None,
        })
        .collect();
    if before.is_empty() {
        return None;
    }
    nearest(index, vector, TEACH.keep_similarity).filter(|m| before.contains(&m.label))
}

/// The own label of the box's class it is close to, but not clearly (origin `_unsure`).
fn unsure(index: &TaughtIndex, vector: &[f32], class: ObjectClass) -> Option<Verdict> {
    let near = nearest(index, vector, TEACH.ask_similarity)?;
    match resolve(near.label, &index.parents) {
        (Some(parent), Some(label)) if parent == class => Some(Verdict::Unsure { label, similarity: near.similarity }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class(key: &str) -> ObjectClass {
        ObjectClass::by_key(key).unwrap()
    }

    fn unit(values: [f32; 4]) -> Vec<f32> {
        let norm = values.iter().map(|v| v * v).sum::<f32>().sqrt();
        values.iter().map(|v| v / norm).collect()
    }

    /// A unit vector with exactly `similarity` to `target`; `side` is orthogonal to it.
    fn toward(target: &[f32], side: &[f32], similarity: f32) -> Vec<f32> {
        let rest = (1.0 - similarity * similarity).sqrt();
        target.iter().zip(side).map(|(t, s)| similarity * t + rest * s).collect()
    }

    const OUR_CAR: LabelId = LabelId(7);

    fn statue() -> Vec<f32> {
        unit([1.0, 0.0, 0.0, 0.0])
    }
    fn person() -> Vec<f32> {
        unit([0.0, 1.0, 0.0, 0.0])
    }
    fn our_car() -> Vec<f32> {
        unit([0.0, 0.0, 1.0, 0.0])
    }
    fn other_car() -> Vec<f32> {
        unit([0.0, 0.0, 0.6, 0.8])
    }
    fn side() -> Vec<f32> {
        unit([0.0, 0.0, 0.0, 1.0])
    }

    fn index(entries: Vec<(BoxLabel, Option<&str>, Vec<f32>)>) -> TaughtIndex {
        let rows = entries
            .into_iter()
            .enumerate()
            .map(|(i, (label, detected, vector))| TaughtRow { sample: SampleId(i as i64 + 1), label, detected: detected.map(class), vector })
            .collect();
        build(rows, [(OUR_CAR, class("car"))].into()).unwrap()
    }

    fn det(key: &str, score: f32, area: [f32; 4]) -> Found {
        Found { class: class(key), score, area, verdict: Verdict::Detector, face: None }
    }

    fn at(key: &str, score: f32) -> Found {
        det(key, score, [0.1, 0.1, 0.3, 0.5])
    }

    fn classes(keys: &[&str]) -> ClassSet {
        keys.iter().map(|k| class(k)).collect::<Vec<_>>().try_into().unwrap()
    }

    #[test]
    fn nearest_needs_a_clear_close_match() {
        let idx = index(vec![(BoxLabel::NotThis, Some("person"), statue()), (BoxLabel::Class(class("person")), Some("person"), person())]);
        let m = nearest(&idx, &statue(), TEACH.match_similarity).unwrap();
        assert_eq!((m.label, m.sample), (BoxLabel::NotThis, SampleId(1)));
        assert!(nearest(&idx, &unit([1.0, 1.0, 1.0, 1.0]), TEACH.match_similarity).is_none(), "far from everything taught");
        let both = index(vec![(BoxLabel::NotThis, Some("person"), statue()), (BoxLabel::Class(class("person")), Some("person"), statue())]);
        assert!(nearest(&both, &statue(), TEACH.match_similarity).is_none(), "equally close to two labels");
    }

    #[test]
    fn apply_filters_relabels_and_gives_own_labels() {
        let cat_like = unit([0.5, 0.5, 0.5, 0.5]);
        let idx = index(vec![
            (BoxLabel::NotThis, Some("person"), statue()),
            (BoxLabel::Class(class("person")), Some("person"), person()),
            (BoxLabel::Own(OUR_CAR), Some("car"), our_car()),
            (BoxLabel::Class(class("cat")), Some("dog"), cat_like.clone()),
        ]);
        let found = [at("person", 0.9), at("person", 0.8), at("car", 0.9), at("car", 0.7), at("dog", 0.9)];
        let vectors = [statue(), person(), our_car(), other_car(), cat_like];
        let (checked, rescue) = plan(&found, &[], &idx);
        assert_eq!(checked.iter().copied().collect::<BTreeSet<_>>(), (0..5).collect(), "every taught class is compared");
        assert!(rescue.is_empty());
        let checked: Vec<(usize, Vec<f32>)> = checked.into_iter().map(|i| (i, vectors[i].clone())).collect();
        let result = apply(&found, &checked, &[], &idx, &classes(&["person", "car", "cat"]), &[]);
        assert!(matches!(result[0].verdict, Verdict::Filtered(by) if by.label == BoxLabel::NotThis), "the statue");
        assert_eq!(result[1].verdict, Verdict::Detector, "taught as right: unchanged");
        assert_eq!((result[2].class, result[2].own_label()), (class("car"), Some(OUR_CAR)));
        assert_eq!(result[3].verdict, Verdict::Detector, "not close enough to the taught car");
        assert!(matches!(result[4].verdict, Verdict::Taught { was: Some(was), .. } if was == class("dog")));
        assert_eq!(result[4].class, class("cat"));
        assert_eq!(found[0].verdict, Verdict::Detector, "the input is not changed");
    }

    #[test]
    fn an_object_that_stays_keeps_its_taught_answer() {
        let idx = index(vec![(BoxLabel::Own(OUR_CAR), Some("car"), our_car())]);
        let (here, elsewhere) = ([0.1, 0.1, 0.3, 0.5], [0.6, 0.5, 0.9, 0.9]);
        let check = |similarity: f32, area: [f32; 4], previous: &[Found]| {
            let found = [det("car", 0.9, area)];
            apply(&found, &[(0, toward(&our_car(), &side(), similarity))], &[], &idx, &classes(&["car"]), previous)[0]
        };
        let first = check(0.90, here, &[]);
        assert!(matches!(first.verdict, Verdict::Taught { how: MatchHow::Clear, label: Some(OUR_CAR), .. }));
        assert_eq!(check(0.84, here, &[]).own_label(), None, "a new object needs match_similarity");
        let stayed = check(0.84, here, &[first]);
        assert!(matches!(stayed.verdict, Verdict::Taught { how: MatchHow::Kept, label: Some(OUR_CAR), .. }));
        assert_eq!(check(0.84, [0.11, 0.1, 0.31, 0.5], &[stayed]).own_label(), Some(OUR_CAR), "and moved a bit");
        assert_eq!(check(0.84, elsewhere, &[first]).own_label(), None, "another place: another object");
        assert_eq!(check(0.70, here, &[first]).own_label(), None, "clearly something else now");
    }

    #[test]
    fn a_box_that_may_be_an_own_label_is_asked_about() {
        let idx = index(vec![(BoxLabel::Own(OUR_CAR), Some("car"), our_car())]);
        let check = |similarity: f32, key: &str| {
            let found = [at(key, 0.9)];
            apply(&found, &[(0, toward(&our_car(), &side(), similarity))], &[], &idx, &classes(&["car", "dog"]), &[])[0]
        };
        assert_eq!(check(0.90, "car").own_label(), Some(OUR_CAR), "clear: no question");
        let asked = check(0.80, "car");
        assert!(matches!(asked.verdict, Verdict::Unsure { label: OUR_CAR, similarity } if (similarity - 0.8).abs() < 1e-5));
        let key = crate::sensors::types::ObjectKey::Own(OUR_CAR);
        assert!(asked.unsure_for(key) && !asked.counts_for(key));
        assert_eq!(check(0.70, "car").verdict, Verdict::Detector, "clearly not ours: no question either");
        assert_eq!(check(0.80, "dog").verdict, Verdict::Detector, "a dog is never our car");
    }

    #[test]
    fn a_class_the_sensor_no_longer_has_is_filtered() {
        let idx = index(vec![(BoxLabel::Class(class("cat")), Some("dog"), statue())]);
        let result = apply(&[at("dog", 0.9)], &[(0, statue())], &[], &idx, &classes(&["dog"]), &[]);
        assert!(result[0].filtered());
    }

    #[test]
    fn unsure_boxes_are_rescued_only_when_a_missed_box_was_taught() {
        let found = [det("car", 0.9, [0.6, 0.1, 0.9, 0.5])];
        let candidates = [at("person", 0.3), det("truck", 0.4, [0.6, 0.1, 0.9, 0.5])];
        let without = index(vec![(BoxLabel::Class(class("person")), Some("person"), person())]);
        assert_eq!(plan(&found, &candidates, &without), (vec![], vec![]));
        let missed = index(vec![(BoxLabel::Class(class("person")), None, person())]);
        let (checked, rescue) = plan(&found, &candidates, &missed);
        assert_eq!((checked, rescue.clone()), (vec![], vec![candidates[0]]), "the truck is the counted car");
        let both = classes(&["person", "car"]);
        let result = apply(&found, &[], &[(rescue[0], person())], &missed, &both, &[]);
        assert!(matches!(result[1].verdict, Verdict::Taught { how: MatchHow::Rescued, .. }));
        assert_eq!(result[1].class, class("person"));
        let near = unit([0.52, 1.0, 0.0, 0.0]);
        let similarity: f32 = near.iter().zip(person()).map(|(a, b)| a * b).sum();
        assert!((TEACH.match_similarity..TEACH.rescue_similarity).contains(&similarity));
        assert_eq!(apply(&found, &[], &[(rescue[0], near)], &missed, &both, &[]).len(), 1, "stricter for an unsure box");
    }

    #[test]
    fn plan_compares_only_taught_classes_and_bounds_the_work() {
        let idx = index(vec![(BoxLabel::NotThis, Some("person"), statue())]);
        let found: Vec<Found> =
            [at("dog", 0.9)].into_iter().chain((0..TEACH.max_checked + 3).map(|i| at("person", 0.5 + i as f32 / 100.0))).collect();
        let (checked, _) = plan(&found, &[], &idx);
        assert_eq!(checked.len(), TEACH.max_checked);
        assert!(!checked.contains(&0));
        assert_eq!(checked[0], found.len() - 1, "most certain first");
    }

    #[test]
    fn an_own_label_not_in_use_is_ignored() {
        let rex = LabelId(9);
        let rows = vec![TaughtRow { sample: SampleId(1), label: BoxLabel::Own(rex), detected: Some(class("dog")), vector: statue() }];
        assert!(build(rows, [(OUR_CAR, class("car"))].into()).is_none());
    }

    #[test]
    fn the_compared_crop_has_a_margin_inside_the_frame() {
        let [x1, y1, x2, y2] = crop_area([0.0, 0.5, 0.5, 1.0]);
        assert_eq!([x1, x2], [0.0, 0.55]);
        assert!((y1 - 0.45).abs() < 1e-6 && y2 == 1.0);
    }
}
