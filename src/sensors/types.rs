//! Domain types shared by the app and the vision process. Invariants live in the constructors, and
//! serde goes through them (`try_from`), so a value read from the wire is valid or rejected.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use egui::ColorImage;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use super::coco::{Detector, ObjectClass};
use crate::frames::{StreamKey, StreamKind};
use crate::model::CameraId;

pub const DEFAULT_INTERVAL_S: u32 = 10; // SENSOR_DEFAULTS["interval_s"]
pub const INTERVAL_LIMITS_S: (u32, u32) = (1, 3600); // SENSOR_LIMITS["interval_s"]
pub const DEFAULT_THRESHOLD: f32 = 0.70; // SENSOR_DEFAULTS["threshold"]
pub const DEFAULT_DEBOUNCE: u8 = 2; // SENSOR_DEFAULTS["debounce"]
pub const DEBOUNCE_LIMITS: (u8, u8) = (1, 20); // SENSOR_LIMITS["debounce"]
pub const MAX_STATES: usize = 9; // MAX_STATES
pub const MAX_POLYGON_POINTS: usize = 32; // ROI_MAX_POINTS
pub const MIN_REGION_SIDE: f32 = 0.01; // imaging.normalise_roi: `w < 0.01 or h < 0.01`
pub const MAX_NAME_CHARS: usize = 64; // api/common.py: `name: str = Field(min_length=1, max_length=64)`
pub const OBJECT_DEFAULT_THRESHOLD: f32 = 0.60; // OBJECT_SENSOR_DEFAULTS["threshold"]
pub const OBJECT_DEFAULT_DEBOUNCE: u8 = 1; // OBJECT_SENSOR_DEFAULTS["debounce"]
pub const OBJECT_MAX_CLASSES: usize = 20; // OBJECT_MAX_CLASSES
pub const MIN_SIZE_LIMIT: f32 = 0.5; // OBJECT_LIMITS["min_size"]
pub const DEFAULT_CLEAR_AFTER_S: u32 = 30; // OBJECT_DEFAULTS["clear_after_s"]
pub const CLEAR_AFTER_LIMIT_S: u32 = 3600; // OBJECT_LIMITS["clear_after_s"]
pub const MAX_OWN_LABELS: usize = 10; // TEACH["max_labels"]
pub const MIN_BOX_SIDE: f32 = 0.01; // TEACH["min_box"]
pub const MAX_TAUGHT: usize = 500; // TEACH["max_examples"]
const MAX_URL_BYTES: usize = 2048;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidInput(pub &'static str);

impl fmt::Display for InvalidInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// Chosen by the app at creation, so `Create` is idempotent and the panel can select the new
/// sensor before the vision process answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SensorId(pub(in crate::sensors) u64);

impl SensorId {
    pub fn random() -> Self {
        loop {
            let id = getrandom::u64().expect("OS random source");
            if id != 0 {
                return Self(id);
            }
        }
    }
}

/// An own label. Chosen by the app when the user names it, so creating it is idempotent and a
/// taught box can refer to it in the same frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LabelId(pub(in crate::sensors) u64);

impl LabelId {
    pub fn random() -> Self {
        Self(SensorId::random().0)
    }
}

/// A frame the vision process still holds. Valid for the life of one vision process only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FrameId(pub(in crate::sensors) u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ImageId(pub(in crate::sensors) u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SampleId(pub(in crate::sensors) i64);

/// A history row. Review items are history rows with a pending review reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct HistoryId(pub(in crate::sensors) i64);

/// Where a sensor takes its images from. Fixed at `Create`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceSpec {
    /// The app owns the tunnel and supplies the local RTSP URL at runtime.
    Dvr(CameraId),
    Url(SecretUrl),
}

/// What the UI is shown about a source; never contains userinfo.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceView {
    Dvr(CameraId),
    Url { display: String, snapshot: bool },
}

/// An `rtsp://`, `http://` or `https://` URL typed by the user, possibly with a password.
/// `http(s)` means a snapshot: one image per fetch.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SecretUrl(String);

impl TryFrom<String> for SecretUrl {
    type Error = InvalidInput;
    fn try_from(url: String) -> Result<Self, InvalidInput> {
        if url.len() > MAX_URL_BYTES || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(InvalidInput("URL inválida"));
        }
        let rest = ["rtsp://", "http://", "https://"]
            .iter()
            .find_map(|scheme| url.strip_prefix(scheme))
            .ok_or(InvalidInput("a URL precisa começar com rtsp://, http:// ou https://"))?;
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        let host = authority.rsplit('@').next().unwrap_or_default();
        if host.is_empty() || host.starts_with(':') {
            return Err(InvalidInput("a URL não tem endereço"));
        }
        Ok(Self(url))
    }
}

impl From<SecretUrl> for String {
    fn from(url: SecretUrl) -> String {
        url.0
    }
}

impl SecretUrl {
    pub(in crate::sensors) fn expose(&self) -> &str {
        &self.0
    }

    /// `http(s)`: one image per fetch, not a stream.
    pub(in crate::sensors) fn is_snapshot(&self) -> bool {
        !self.0.starts_with("rtsp://")
    }

    /// The URL without its userinfo, for the UI.
    pub(in crate::sensors) fn display(&self) -> String {
        crate::procs::redact_userinfo(&self.0)
    }
}

impl fmt::Debug for SecretUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretUrl(***)")
    }
}

/// Pixel size of the source as it arrives, before the fixed `StreamKind::size()` scale.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Geometry {
    pub width: u32,
    pub height: u32,
}

/// A point in normalised frame coordinates, both axes in `0.0..=1.0`, so a region survives the
/// Sub/Main scale and the downscaled preview it is drawn on.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "[f32; 2]", into = "[f32; 2]")]
pub struct NormPoint {
    x: f32,
    y: f32,
}

impl NormPoint {
    pub fn new(x: f32, y: f32) -> Result<Self, InvalidInput> {
        let unit = 0.0..=1.0;
        if unit.contains(&x) && unit.contains(&y) {
            Ok(Self { x, y })
        } else {
            Err(InvalidInput("ponto fora do quadro"))
        }
    }

    pub fn x(self) -> f32 {
        self.x
    }

    pub fn y(self) -> f32 {
        self.y
    }
}

impl TryFrom<[f32; 2]> for NormPoint {
    type Error = InvalidInput;
    fn try_from([x, y]: [f32; 2]) -> Result<Self, InvalidInput> {
        Self::new(x, y)
    }
}

impl From<NormPoint> for [f32; 2] {
    fn from(p: NormPoint) -> Self {
        [p.x, p.y]
    }
}

/// The part of the frame a sensor observes, in canonical form: a rectangle covering the frame is
/// `Whole`, and a polygon that is an axis-aligned rectangle is a `Rect` (`imaging.normalise_roi`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RegionShape", into = "RegionShape")]
pub struct Region(RegionShape);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum RegionShape {
    Whole,
    Rect {
        min: NormPoint,
        max: NormPoint,
    },
    /// Not required to be convex. Outside it, inside its bounding box, is painted neutral.
    Polygon(Vec<NormPoint>),
}

impl Region {
    pub const WHOLE: Self = Self(RegionShape::Whole);

    pub fn rect(a: NormPoint, b: NormPoint) -> Result<Self, InvalidInput> {
        let min = NormPoint { x: a.x.min(b.x), y: a.y.min(b.y) };
        let max = NormPoint { x: a.x.max(b.x), y: a.y.max(b.y) };
        if max.x - min.x < MIN_REGION_SIDE || max.y - min.y < MIN_REGION_SIDE {
            return Err(InvalidInput("região pequena demais"));
        }
        if min.x == 0.0 && min.y == 0.0 && max.x >= 0.999 && max.y >= 0.999 {
            return Ok(Self::WHOLE);
        }
        Ok(Self(RegionShape::Rect { min, max }))
    }

    pub fn polygon(points: Vec<NormPoint>) -> Result<Self, InvalidInput> {
        if !(3..=MAX_POLYGON_POINTS).contains(&points.len()) {
            return Err(InvalidInput("o polígono precisa de 3 a 32 pontos"));
        }
        let (min, max) = bounds(&points);
        if max.x - min.x < MIN_REGION_SIDE || max.y - min.y < MIN_REGION_SIDE {
            return Err(InvalidInput("região pequena demais"));
        }
        let twice_area: f32 =
            points.iter().zip(points.iter().cycle().skip(1)).map(|(p, q)| p.x * q.y - q.x * p.y).sum();
        if twice_area == 0.0 {
            return Err(InvalidInput("o polígono não tem área"));
        }
        let corners = [(min.x, min.y), (max.x, min.y), (max.x, max.y), (min.x, max.y)];
        let is_rect = points.len() == 4 && corners.iter().all(|c| points.iter().any(|p| (p.x, p.y) == *c));
        if is_rect {
            return Self::rect(min, max);
        }
        Ok(Self(RegionShape::Polygon(points)))
    }

    pub fn shape(&self) -> &RegionShape {
        &self.0
    }

    /// Stable across processes and builds. Sample embeddings are cached per (sample, backbone,
    /// region hash), so editing the region invalidates them by construction.
    pub fn hash(&self) -> u64 {
        let (tag, points) = match &self.0 {
            RegionShape::Whole => (0u8, Vec::new()),
            RegionShape::Rect { min, max } => (1, vec![*min, *max]),
            RegionShape::Polygon(points) => (2, points.clone()),
        };
        let mut h = Sha256::new();
        h.update([tag]);
        for p in points {
            h.update(p.x.to_le_bytes());
            h.update(p.y.to_le_bytes());
        }
        let digest = h.finalize();
        u64::from_le_bytes(digest[..8].try_into().expect("8 bytes"))
    }

    /// The bounding box, as `(min, max)`.
    pub fn bounds(&self) -> (NormPoint, NormPoint) {
        match &self.0 {
            RegionShape::Whole => (NormPoint { x: 0.0, y: 0.0 }, NormPoint { x: 1.0, y: 1.0 }),
            RegionShape::Rect { min, max } => (*min, *max),
            RegionShape::Polygon(points) => bounds(points),
        }
    }
}

fn bounds(points: &[NormPoint]) -> (NormPoint, NormPoint) {
    let mut min = NormPoint { x: 1.0, y: 1.0 };
    let mut max = NormPoint { x: 0.0, y: 0.0 };
    for p in points {
        min = NormPoint { x: min.x.min(p.x), y: min.y.min(p.y) };
        max = NormPoint { x: max.x.max(p.x), y: max.y.max(p.y) };
    }
    (min, max)
}

impl TryFrom<RegionShape> for Region {
    type Error = InvalidInput;
    fn try_from(shape: RegionShape) -> Result<Self, InvalidInput> {
        match shape {
            RegionShape::Whole => Ok(Self::WHOLE),
            RegionShape::Rect { min, max } => Self::rect(min, max),
            RegionShape::Polygon(points) => Self::polygon(points),
        }
    }
}

impl From<Region> for RegionShape {
    fn from(region: Region) -> Self {
        region.0
    }
}

/// Everything the user can change after creation. Source, stream and kind are fixed at `Create`;
/// the kind is the discriminant of `kind`, which `Configure` may not change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub name: Name,
    pub region: Region,
    pub interval: Interval,
    pub triggers: Triggers,
    pub paused: bool,
    pub kind: KindSettings,
}

impl Settings {
    /// The "novo sensor" form's starting point (origin `SENSOR_DEFAULTS`, `TRIGGER_DEFAULTS`).
    pub fn new_states(name: Name, states: StateSet) -> Self {
        Self {
            name,
            region: Region::WHOLE,
            interval: Interval(DEFAULT_INTERVAL_S),
            triggers: Triggers { regular: true, change_detection: false },
            paused: false,
            kind: KindSettings::States(StatesSettings {
                states,
                threshold: Unit(DEFAULT_THRESHOLD),
                debounce: Debounce(DEFAULT_DEBOUNCE),
            }),
        }
    }

    /// Origin `OBJECT_SENSOR_DEFAULTS` and `OBJECT_DEFAULTS`.
    pub fn new_objects(name: Name, classes: ClassSet) -> Self {
        Self {
            name,
            region: Region::WHOLE,
            interval: Interval(DEFAULT_INTERVAL_S),
            triggers: Triggers { regular: true, change_detection: false },
            paused: false,
            kind: KindSettings::Objects(ObjectsSettings {
                classes,
                threshold: Unit(OBJECT_DEFAULT_THRESHOLD),
                debounce: Debounce(OBJECT_DEFAULT_DEBOUNCE),
                min_size: MinSize(0.0),
                clear_after: ClearAfter(DEFAULT_CLEAR_AFTER_S),
                use_taught: true,
                labels: OwnLabels(Vec::new()),
            }),
        }
    }
}

/// Trimmed, 1 to `MAX_NAME_CHARS` characters.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Name(String);

impl TryFrom<String> for Name {
    type Error = InvalidInput;
    fn try_from(name: String) -> Result<Self, InvalidInput> {
        let trimmed = name.trim();
        if trimmed.is_empty() || trimmed.chars().count() > MAX_NAME_CHARS {
            return Err(InvalidInput("o nome precisa ter de 1 a 64 caracteres"));
        }
        Ok(Self(trimmed.to_owned()))
    }
}

impl From<Name> for String {
    fn from(name: Name) -> String {
        name.0
    }
}

impl Name {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Seconds between regular checks, within `INTERVAL_LIMITS_S`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct Interval(u32);

impl TryFrom<u32> for Interval {
    type Error = InvalidInput;
    fn try_from(seconds: u32) -> Result<Self, InvalidInput> {
        let (lo, hi) = INTERVAL_LIMITS_S;
        if (lo..=hi).contains(&seconds) { Ok(Self(seconds)) } else { Err(InvalidInput("intervalo fora de 1 s a 1 h")) }
    }
}

impl From<Interval> for u32 {
    fn from(interval: Interval) -> u32 {
        interval.0
    }
}

impl Interval {
    pub fn get(self) -> Duration {
        Duration::from_secs(self.0.into())
    }
}

/// When to check besides "check now"; the burst and probe paces are constants.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Triggers {
    /// Off: only change detection and "check now" cause checks, plus one at start.
    pub regular: bool,
    pub change_detection: bool,
}

/// One variant per sensor kind.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum KindSettings {
    States(StatesSettings),
    Objects(ObjectsSettings),
}

/// Origin `OBJECT_DEFAULTS` with `OBJECT_SENSOR_DEFAULTS`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ObjectsSettings {
    pub classes: ClassSet,
    /// The detector's minimum confidence for a box to count.
    pub threshold: Unit,
    /// Checks in a row with an object present before it switches on.
    pub debounce: Debounce,
    pub min_size: MinSize,
    pub clear_after: ClearAfter,
    /// Off: the detector alone decides; taught boxes are kept but not compared.
    pub use_taught: bool,
    pub labels: OwnLabels,
}

impl ObjectsSettings {
    /// Own labels in use: those whose class is still selected (origin `active_custom`).
    pub fn active_labels(&self) -> impl Iterator<Item = &OwnLabel> {
        self.labels.iter().filter(|l| self.classes.contains(l.parent))
    }

    /// What the sensor reports: its classes, then its own labels in use (origin `object_keys`).
    pub fn keys(&self) -> Vec<ObjectKey> {
        let labels = self.active_labels().map(|l| ObjectKey::Own(l.id));
        self.classes.iter().map(ObjectKey::Class).chain(labels).collect()
    }

    pub fn key_name(&self, key: ObjectKey) -> &str {
        match key {
            ObjectKey::Class(class) => class.name(),
            ObjectKey::Own(id) => self.labels.get(id).map_or("?", |l| l.name.as_str()),
        }
    }
}

/// 1 to `OBJECT_MAX_CLASSES` distinct classes, in the order chosen.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<ObjectClass>", into = "Vec<ObjectClass>")]
pub struct ClassSet(Vec<ObjectClass>);

impl TryFrom<Vec<ObjectClass>> for ClassSet {
    type Error = InvalidInput;
    fn try_from(classes: Vec<ObjectClass>) -> Result<Self, InvalidInput> {
        if !(1..=OBJECT_MAX_CLASSES).contains(&classes.len()) {
            return Err(InvalidInput("escolha de 1 a 20 objetos"));
        }
        if classes.iter().collect::<BTreeSet<_>>().len() != classes.len() {
            return Err(InvalidInput("objetos repetidos"));
        }
        Ok(Self(classes))
    }
}

impl From<ClassSet> for Vec<ObjectClass> {
    fn from(set: ClassSet) -> Self {
        set.0
    }
}

impl ClassSet {
    pub fn iter(&self) -> impl Iterator<Item = ObjectClass> + '_ {
        self.0.iter().copied()
    }

    pub fn contains(&self, class: ObjectClass) -> bool {
        self.0.contains(&class)
    }
}

/// The smallest box that counts, as a share of the region's area, in `0.0..=MIN_SIZE_LIMIT`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "f32", into = "f32")]
pub struct MinSize(f32);

impl TryFrom<f32> for MinSize {
    type Error = InvalidInput;
    fn try_from(share: f32) -> Result<Self, InvalidInput> {
        if (0.0..=MIN_SIZE_LIMIT).contains(&share) { Ok(Self(share)) } else { Err(InvalidInput("tamanho mínimo fora de 0 a 50%")) }
    }
}

impl From<MinSize> for f32 {
    fn from(size: MinSize) -> f32 {
        size.0
    }
}

/// How long an object stays present after it was last seen, `0..=CLEAR_AFTER_LIMIT_S` seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct ClearAfter(u32);

impl TryFrom<u32> for ClearAfter {
    type Error = InvalidInput;
    fn try_from(seconds: u32) -> Result<Self, InvalidInput> {
        if seconds <= CLEAR_AFTER_LIMIT_S { Ok(Self(seconds)) } else { Err(InvalidInput("tempo para sumir fora de 0 s a 1 h")) }
    }
}

impl From<ClearAfter> for u32 {
    fn from(clear: ClearAfter) -> u32 {
        clear.0
    }
}

impl ClearAfter {
    pub fn get(self) -> Duration {
        Duration::from_secs(self.0.into())
    }
}

/// A name the user gives one specific object, as a kind of one class ("Nosso carro", a car).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnLabel {
    pub id: LabelId,
    pub name: Name,
    pub parent: ObjectClass,
}

/// At most `MAX_OWN_LABELS`, with distinct ids and distinct names ignoring case. A label whose
/// class is not selected stays, unused, until its class is chosen again.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<OwnLabel>", into = "Vec<OwnLabel>")]
pub struct OwnLabels(Vec<OwnLabel>);

impl TryFrom<Vec<OwnLabel>> for OwnLabels {
    type Error = InvalidInput;
    fn try_from(labels: Vec<OwnLabel>) -> Result<Self, InvalidInput> {
        if labels.len() > MAX_OWN_LABELS {
            return Err(InvalidInput("no máximo 10 rótulos próprios por sensor"));
        }
        let ids: BTreeSet<LabelId> = labels.iter().map(|l| l.id).collect();
        let names: BTreeSet<String> = labels.iter().map(|l| l.name.0.to_lowercase()).collect();
        if ids.len() != labels.len() || names.len() != labels.len() {
            return Err(InvalidInput("já existe um rótulo com esse nome"));
        }
        Ok(Self(labels))
    }
}

impl From<OwnLabels> for Vec<OwnLabel> {
    fn from(labels: OwnLabels) -> Self {
        labels.0
    }
}

impl OwnLabels {
    pub fn iter(&self) -> impl Iterator<Item = &OwnLabel> {
        self.0.iter()
    }

    pub fn get(&self, id: LabelId) -> Option<&OwnLabel> {
        self.0.iter().find(|l| l.id == id)
    }

    /// A copy with `label` added, checked like any set.
    pub fn with(&self, label: OwnLabel) -> Result<Self, InvalidInput> {
        self.0.iter().cloned().chain([label]).collect::<Vec<_>>().try_into()
    }

    pub fn without(&self, id: LabelId) -> Self {
        Self(self.0.iter().filter(|l| l.id != id).cloned().collect())
    }
}

/// What an object sensor reports on: a class, or an own label.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ObjectKey {
    Class(ObjectClass),
    Own(LabelId),
}

/// What a taught box really is (origin: the taught label).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum BoxLabel {
    /// Not what the detector said (origin `NONE_LABEL`): boxes like it are not counted.
    NotThis,
    Class(ObjectClass),
    Own(LabelId),
}

/// A box drawn or tapped by the user, normalised to the frame, each side at least `MIN_BOX_SIDE`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "[f32; 4]", into = "[f32; 4]")]
pub struct NormBox([f32; 4]);

impl TryFrom<[f32; 4]> for NormBox {
    type Error = InvalidInput;
    /// `[x1, y1, x2, y2]`, clamped to the frame (origin `TeachIn._valid_box`).
    fn try_from(area: [f32; 4]) -> Result<Self, InvalidInput> {
        if area.iter().any(|v| v.is_nan()) {
            return Err(InvalidInput("caixa inválida"));
        }
        let [x1, y1, x2, y2] = area.map(|v| v.clamp(0.0, 1.0));
        if x2 - x1 < MIN_BOX_SIDE || y2 - y1 < MIN_BOX_SIDE {
            return Err(InvalidInput("caixa pequena demais"));
        }
        Ok(Self([x1, y1, x2, y2]))
    }
}

impl From<NormBox> for [f32; 4] {
    fn from(area: NormBox) -> Self {
        area.0
    }
}

impl NormBox {
    /// The box between two corners, in any order.
    pub fn between(a: NormPoint, b: NormPoint) -> Result<Self, InvalidInput> {
        [a.x.min(b.x), a.y.min(b.y), a.x.max(b.x), a.y.max(b.y)].try_into()
    }

    pub fn area(self) -> [f32; 4] {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatesSettings {
    pub states: StateSet,
    /// Below this top probability the value is `Unknown`.
    pub threshold: Unit,
    /// Equal results in a row before the published value changes.
    pub debounce: Debounce,
}

/// Stable key of a state, `1..=MAX_STATES`, the keyboard shortcut in the teach view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct StateKey(u8);

impl TryFrom<u8> for StateKey {
    type Error = InvalidInput;
    fn try_from(key: u8) -> Result<Self, InvalidInput> {
        if (1..=MAX_STATES as u8).contains(&key) { Ok(Self(key)) } else { Err(InvalidInput("estado fora de 1 a 9")) }
    }
}

impl From<StateKey> for u8 {
    fn from(key: StateKey) -> u8 {
        key.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateDef {
    pub key: StateKey,
    pub name: Name,
}

/// 1 to `MAX_STATES` states with unique keys and unique names, ignoring case. Fewer than two
/// states with samples is the `Untrained` status, not an invalid set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<StateDef>", into = "Vec<StateDef>")]
pub struct StateSet(Vec<StateDef>);

impl TryFrom<Vec<StateDef>> for StateSet {
    type Error = InvalidInput;
    fn try_from(states: Vec<StateDef>) -> Result<Self, InvalidInput> {
        if !(1..=MAX_STATES).contains(&states.len()) {
            return Err(InvalidInput("o sensor precisa de 1 a 9 estados"));
        }
        let keys: BTreeSet<StateKey> = states.iter().map(|s| s.key).collect();
        let names: BTreeSet<String> = states.iter().map(|s| s.name.0.to_lowercase()).collect();
        if keys.len() != states.len() || names.len() != states.len() {
            return Err(InvalidInput("estados repetidos"));
        }
        Ok(Self(states))
    }
}

impl From<StateSet> for Vec<StateDef> {
    fn from(set: StateSet) -> Self {
        set.0
    }
}

impl StateSet {
    pub fn iter(&self) -> impl Iterator<Item = &StateDef> {
        self.0.iter()
    }
}

/// A probability or threshold in `0.0..=1.0`.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "f32", into = "f32")]
pub struct Unit(f32);

impl TryFrom<f32> for Unit {
    type Error = InvalidInput;
    fn try_from(value: f32) -> Result<Self, InvalidInput> {
        if (0.0..=1.0).contains(&value) { Ok(Self(value)) } else { Err(InvalidInput("valor fora de 0 a 1")) }
    }
}

impl From<Unit> for f32 {
    fn from(unit: Unit) -> f32 {
        unit.0
    }
}

/// Equal results in a row needed to publish, within `DEBOUNCE_LIMITS`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct Debounce(u8);

impl TryFrom<u8> for Debounce {
    type Error = InvalidInput;
    fn try_from(count: u8) -> Result<Self, InvalidInput> {
        let (lo, hi) = DEBOUNCE_LIMITS;
        if (lo..=hi).contains(&count) { Ok(Self(count)) } else { Err(InvalidInput("confirmações fora de 1 a 20")) }
    }
}

impl From<Debounce> for u8 {
    fn from(debounce: Debounce) -> u8 {
        debounce.0
    }
}

/// Everything the UI draws about sensors: a replica of the vision process's state, replaced by
/// reports and never written by the UI.
#[derive(Default)]
pub struct SensorsView {
    pub link: LinkState,
    pub sensors: BTreeMap<SensorId, SensorEntry>,
    pub system: Option<SystemStatus>,
    pub focus: Option<FocusView>,
    /// DVR streams the vision process needs URLs for; the app opens their tunnels even when the
    /// camera is not in the grid.
    pub wanted_feeds: BTreeSet<StreamKey>,
    pub(super) images: BTreeMap<ImageId, Arc<ColorImage>>,
}

impl SensorsView {
    pub fn on_camera<'a>(&'a self, cam: &'a CameraId) -> impl Iterator<Item = &'a SensorEntry> + 'a {
        self.sensors.values().filter(move |e| matches!(&e.source, SourceView::Dvr(c) if c == cam))
    }

    pub fn image(&self, id: ImageId) -> Option<&Arc<ColorImage>> {
        self.images.get(&id)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum LinkState {
    /// No child, or no `Hello` from it yet. Controls are disabled.
    #[default]
    Waiting,
    Ready,
    /// The child speaks another protocol version. Terminal for this app session.
    Incompatible {
        theirs: u32,
    },
}

/// One sensor as the vision process sees it now, sent whole on every change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SensorEntry {
    pub id: SensorId,
    pub source: SourceView,
    pub stream: StreamKind,
    pub settings: Settings,
    pub status: SensorStatus,
    /// Debounced, published value. `None` until the first check of this process.
    pub value: Option<SensorValue>,
    pub live: KindLive,
    pub last_check: Option<SystemTime>,
    pub notice: Option<Notice>,
}

/// Derived at publish time (`logic::sensor_status`), never stored. Precedence is the declaration
/// order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SensorStatus {
    Paused,
    /// Checks stop until the user saves the sensor again, which accepts the geometry seen.
    GeometryChanged {
        expected: Geometry,
        seen: Geometry,
    },
    /// No URL from the app yet, or the decoder is starting.
    WaitingSource,
    /// `retry_in` `None` is terminal (401).
    SourceDown {
        reason: String,
        retry_in: Option<Duration>,
    },
    WaitingModel {
        progress: Option<f32>,
        failed: Option<String>,
    },
    Failed(String),
    /// Fewer than two states have samples.
    Untrained,
    Live,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SensorValue {
    State(StateKey),
    /// Below the threshold (origin `UNKNOWN_STATE`).
    Unknown,
    /// The objects present, each with its published count; what is absent is off.
    Objects(Vec<(ObjectKey, u32)>),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum KindLive {
    States(StatesLive),
    Objects(ObjectsLive),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StatesLive {
    /// Head output of the last check, one per state.
    pub probabilities: Vec<(StateKey, f32)>,
    pub training: bool,
    pub samples: BTreeMap<StateKey, u32>,
    pub review_pending: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ObjectsLive {
    /// The frame `found` belongs to. The preview shows it, so the boxes are drawn on their frame.
    pub frame: Option<FrameId>,
    pub found: Vec<Found>,
    /// One per key the sensor reports, in `ObjectsSettings::keys` order.
    pub tracks: Vec<TrackView>,
    /// Taught boxes, and whether they are being compared yet.
    pub taught: u32,
    pub indexing: bool,
    pub review_pending: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrackView {
    pub key: ObjectKey,
    pub on: bool,
    pub count: u32,
    /// Best confidence in the last check.
    pub score: f32,
}

/// One box of a check, normalised to the whole frame (origin: a detection dict).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Found {
    /// The detector's class, or the class a taught box changed it to.
    pub class: ObjectClass,
    pub score: f32,
    /// `[x1, y1, x2, y2]`.
    pub area: [f32; 4],
    pub verdict: Verdict,
}

impl Found {
    pub fn own_label(&self) -> Option<LabelId> {
        match self.verdict {
            Verdict::Taught { label, .. } => label,
            _ => None,
        }
    }

    pub fn filtered(&self) -> bool {
        matches!(self.verdict, Verdict::Filtered(_))
    }

    /// Whether the box counts for `key`; filtered boxes count for nothing (origin `counts_for`).
    pub fn counts_for(&self, key: ObjectKey) -> bool {
        !self.filtered()
            && match key {
                ObjectKey::Class(class) => self.class == class,
                ObjectKey::Own(id) => self.own_label() == Some(id),
            }
    }

    /// Whether the box may be the own label `key`, but not clearly (origin `unsure_for`).
    pub fn unsure_for(&self, key: ObjectKey) -> bool {
        matches!((self.verdict, key), (Verdict::Unsure { label, .. }, ObjectKey::Own(id)) if label == id)
    }
}

/// What comparing a box with the taught boxes said (origin: `match`, `filtered`, `label`, `was`,
/// `kept`, `rescued`, `ask`).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Verdict {
    /// Not compared, or no clear answer: the detector's class stands.
    Detector,
    /// May be this own label, but not clearly: counted for neither, and asked about.
    Unsure { label: LabelId, similarity: f32 },
    /// Clearly a taught box that is not one of the sensor's objects: not counted.
    Filtered(TaughtMatch),
    /// Clearly a taught box: counted as `class`, and as `label` when it is an own label.
    Taught { by: TaughtMatch, label: Option<LabelId>, was: Option<ObjectClass>, how: MatchHow },
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaughtMatch {
    pub sample: SampleId,
    pub label: BoxLabel,
    pub similarity: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchHow {
    /// At least `match_similarity`.
    Clear,
    /// The same object at the same place as in the last check, still close to its answer.
    Kept,
    /// A box the detector was unsure about, very close to a taught box it had missed.
    Rescued,
}

/// What changed at a check of an object sensor, kept with its history row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObjectEvent {
    On(ObjectKey),
    Off(ObjectKey),
    /// Boxes of this class started being filtered away by taught boxes.
    Filtered(ObjectClass),
}

/// "Is this <label>?" about one box, in the review queue (origin `_record_question`).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub label: LabelId,
    pub detected: ObjectClass,
    pub area: [f32; 4],
    pub similarity: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Notice {
    pub at: SystemTime,
    pub kind: NoticeKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum NoticeKind {
    FrameExpired,
    KindMismatch,
    UnknownItem,
    /// A taught box whose bottom centre is outside the region would never count.
    OutsideRegion,
    /// The sensor has `MAX_TAUGHT` taught boxes already.
    TaughtFull,
    Store(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SystemStatus {
    pub retention: Retention,
    pub usage: StorageUsage,
    pub models: Vec<ModelStatus>,
    /// The detector every object sensor uses.
    pub detector: Detector,
    pub notice: Option<Notice>,
}

/// History pruning (origin `STORAGE_DEFAULTS`): by age, then by size. Samples are never pruned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retention {
    pub max_age: Duration,
    pub max_bytes: u64,
}

impl Default for Retention {
    fn default() -> Self {
        Self { max_age: Duration::from_secs(7 * 24 * 3600), max_bytes: 2 * 1024 * 1024 * 1024 }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageUsage {
    pub history_bytes: u64,
    pub sample_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelStatus {
    pub name: String,
    pub state: ModelState,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ModelState {
    NotNeeded,
    Downloading { done: u64, total: u64 },
    Ready,
    Failed(String),
}

/// What the panel looks at. The app sends it when it differs from what the current vision child
/// last received; the answer is the `FocusView` the vision process pushes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interest {
    pub sensor: Option<SensorId>,
    pub tab: Tab,
    /// History page: rows strictly older than this; `None` is the newest page.
    pub history_before: Option<HistoryId>,
    /// Sample page: rows strictly older than this; `None` is the newest page.
    pub samples_before: Option<SampleId>,
    /// One item opened full size.
    pub open: Option<HistoryId>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tab {
    /// Live preview: region editor and teaching.
    #[default]
    Live,
    Samples,
    Review,
    History,
}

/// The vision process's answer to the current `Interest`, re-sent whole when anything in it
/// changes. Image bytes travel separately, before the first view that references them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FocusView {
    pub sensor: SensorId,
    pub preview: Option<Preview>,
    pub detail: Detail,
    pub opened: Option<OpenedItem>,
}

impl FocusView {
    /// Every image this view references. Both sides derive the retained set from this, so they
    /// never disagree on which images exist.
    pub fn images(&self) -> BTreeSet<ImageId> {
        let thumbs: Vec<ImageId> = match &self.detail {
            Detail::Live => Vec::new(),
            Detail::Samples { rows, .. } => rows.iter().map(|s| s.thumb).collect(),
            Detail::Review(rows) | Detail::History { rows, .. } => rows.iter().filter_map(|r| r.thumb).collect(),
        };
        let preview = self.preview.as_ref().map(|p| p.image);
        let opened = self.opened.as_ref().map(|o| o.image);
        thumbs.into_iter().chain(preview).chain(opened).collect()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Preview {
    /// Teach this exact frame.
    pub frame: FrameId,
    /// Downscaled whole frame; the region is drawn over it in normalised coordinates.
    pub image: ImageId,
    pub geometry: Geometry,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Detail {
    Live,
    Samples { rows: Vec<SampleItem>, before: Option<SampleId>, older: Option<SampleId> },
    Review(Vec<HistoryItem>),
    History { rows: Vec<HistoryItem>, older: bool },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SampleItem {
    pub id: SampleId,
    /// `None`: a sample nobody labelled yet.
    pub label: Option<SampleLabel>,
    pub thumb: ImageId,
    pub at: SystemTime,
}

/// A state sample's state, or what a taught box of an object sensor is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SampleLabel {
    State(StateKey),
    Box(BoxLabel),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryItem {
    pub id: HistoryId,
    pub at: SystemTime,
    pub value: SensorValue,
    pub confidence: f32,
    pub review: Option<ReviewReason>,
    pub thumb: Option<ImageId>,
    /// What changed at that check of an object sensor; empty for a state sensor.
    pub events: Vec<ObjectEvent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum ReviewReason {
    LowConfidence,
    Flip,
    SpotCheck,
    /// An object sensor asks whether a box is one of its own labels.
    Ask(Question),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OpenedItem {
    pub id: HistoryId,
    /// Whole frame as recorded; the region may have moved since.
    pub image: ImageId,
    /// The boxes an object sensor found in that frame.
    pub found: Vec<Found>,
}

/// Everything the UI can ask. Each variant is idempotent: sending it twice has the effect of
/// sending it once.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum UserCommand {
    Create {
        id: SensorId,
        source: SourceSpec,
        stream: StreamKind,
        settings: Settings,
    },
    /// Replaces the settings; refused (`KindMismatch`) when `settings.kind` is another kind.
    Configure {
        id: SensorId,
        settings: Settings,
    },
    Delete(SensorId),
    /// One full check now, paused or not.
    CheckNow(SensorId),
    /// Teaching the same frame again relabels it. State sensors only.
    Teach {
        id: SensorId,
        frame: FrameId,
        state: StateKey,
    },
    /// Object sensors only: what one box of a held frame really is. A new own label is added with
    /// `Configure` first. `detected` is the detector's class for a tapped box, `None` for a box
    /// the user drew around something it missed.
    TeachBox {
        id: SensorId,
        frame: FrameId,
        area: NormBox,
        detected: Option<ObjectClass>,
        label: BoxLabel,
    },
    /// Answers a review item. A state sensor's `Some(State)` turns the frame into a sample; an
    /// object sensor's `Some(Box)` teaches the asked box as that label. `None` dismisses it.
    Resolve {
        id: SensorId,
        item: HistoryId,
        label: Option<SampleLabel>,
    },
    /// `None` deletes the sample.
    Relabel {
        id: SensorId,
        sample: SampleId,
        label: Option<SampleLabel>,
    },
    SetRetention(Retention),
    SetDetector(Detector),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(x: f32, y: f32) -> NormPoint {
        NormPoint::new(x, y).unwrap()
    }

    fn json<T: Serialize>(value: &T) -> String {
        serde_json::to_string(value).unwrap()
    }

    #[test]
    fn rect_is_normalised_and_full_frame_is_whole() {
        let r = Region::rect(p(0.8, 0.9), p(0.2, 0.1)).unwrap();
        assert_eq!(r.shape(), &RegionShape::Rect { min: p(0.2, 0.1), max: p(0.8, 0.9) });
        assert_eq!(Region::rect(p(0.0, 0.0), p(1.0, 0.9995)).unwrap(), Region::WHOLE);
        assert!(Region::rect(p(0.2, 0.2), p(0.205, 0.8)).is_err());
    }

    #[test]
    fn polygon_invariants() {
        let triangle = vec![p(0.1, 0.1), p(0.9, 0.1), p(0.1, 0.9)];
        assert!(matches!(Region::polygon(triangle.clone()).unwrap().shape(), RegionShape::Polygon(_)));
        assert!(Region::polygon(triangle[..2].to_vec()).is_err());
        assert!(Region::polygon(vec![p(0.1, 0.1), p(0.5, 0.5), p(0.9, 0.9)]).is_err(), "collinear");
        assert!(Region::polygon(vec![p(0.5, 0.5); MAX_POLYGON_POINTS + 1]).is_err());
        // A polygon that is just the rectangle is stored as a rectangle.
        let square = vec![p(0.2, 0.2), p(0.6, 0.2), p(0.6, 0.7), p(0.2, 0.7)];
        assert_eq!(Region::polygon(square).unwrap(), Region::rect(p(0.2, 0.2), p(0.6, 0.7)).unwrap());
    }

    #[test]
    fn region_hash_tells_shapes_with_the_same_box_apart() {
        let a = Region::polygon(vec![p(0.1, 0.1), p(0.9, 0.1), p(0.1, 0.9)]).unwrap();
        let b = Region::polygon(vec![p(0.9, 0.9), p(0.9, 0.1), p(0.1, 0.9)]).unwrap();
        let rect = Region::rect(p(0.1, 0.1), p(0.9, 0.9)).unwrap();
        assert_ne!(a.hash(), b.hash());
        assert_ne!(a.hash(), rect.hash());
        assert_eq!(a.hash(), a.clone().hash());
    }

    #[test]
    fn deserialising_validates() {
        let ok: Region = serde_json::from_str(r#"{"Rect":{"min":[0.0,0.0],"max":[1.0,1.0]}}"#).unwrap();
        assert_eq!(ok, Region::WHOLE);
        assert!(serde_json::from_str::<Region>(r#"{"Polygon":[[0.1,0.1],[0.2,0.2]]}"#).is_err());
        assert!(serde_json::from_str::<NormPoint>("[1.5,0.2]").is_err());
        assert!(serde_json::from_str::<StateKey>("0").is_err());
        assert!(serde_json::from_str::<StateKey>("10").is_err());
        assert!(serde_json::from_str::<Interval>("0").is_err());
        assert!(serde_json::from_str::<Interval>("3601").is_err());
        assert!(serde_json::from_str::<Debounce>("21").is_err());
        assert!(serde_json::from_str::<Unit>("1.01").is_err());
        assert!(serde_json::from_str::<Name>(r#""   ""#).is_err());
        assert_eq!(serde_json::from_str::<Name>(r#"" Portão ""#).unwrap().as_str(), "Portão");
        assert!(serde_json::from_str::<SecretUrl>(r#""ftp://cam/x""#).is_err());
        assert!(serde_json::from_str::<SecretUrl>(r#""rtsp://user:pw@/x""#).is_err());
    }

    #[test]
    fn state_set_rejects_repeats() {
        let def = |key: u8, name: &str| StateDef { key: StateKey(key), name: Name(name.into()) };
        assert!(StateSet::try_from(vec![def(1, "Aberto"), def(2, "Fechado")]).is_ok());
        assert!(StateSet::try_from(vec![def(1, "Aberto"), def(1, "Fechado")]).is_err());
        assert!(StateSet::try_from(vec![def(1, "Aberto"), def(2, "aberto")]).is_err());
        assert!(StateSet::try_from(vec![]).is_err());
    }

    #[test]
    fn apostrophe_credentials_never_reach_the_source_view() {
        let url = SecretUrl::try_from("rtsp://admin:pa'ss@camera.local/live".to_owned()).unwrap();
        assert_eq!(url.display(), "rtsp://***@camera.local/live");
    }
    #[test]
    fn secret_url_is_redacted_everywhere_debug_reaches() {
        let url = SecretUrl::try_from("rtsp://admin:hunter2@10.0.0.5:554/stream".to_owned()).unwrap();
        let cmd = UserCommand::Create {
            id: SensorId(7),
            source: SourceSpec::Url(url.clone()),
            stream: StreamKind::Sub,
            settings: Settings::new_states(
                Name("Portão".into()),
                StateSet(vec![StateDef { key: StateKey(1), name: Name("Aberto".into()) }]),
            ),
        };
        assert!(!format!("{url:?} {cmd:?}").contains("hunter2"));
        assert!(json(&cmd).contains("hunter2"), "the wire still carries it");
    }

    #[test]
    fn focus_images_are_every_reference() {
        let item = |id: i64, thumb: Option<u64>| HistoryItem {
            id: HistoryId(id),
            at: SystemTime::UNIX_EPOCH,
            value: SensorValue::Unknown,
            confidence: 0.5,
            review: None,
            thumb: thumb.map(ImageId),
            events: Vec::new(),
        };
        let view = FocusView {
            sensor: SensorId(1),
            preview: Some(Preview {
                frame: FrameId(3),
                image: ImageId(10),
                geometry: Geometry { width: 704, height: 480 },
            }),
            detail: Detail::History { rows: vec![item(1, Some(11)), item(2, None)], older: true },
            opened: Some(OpenedItem { id: HistoryId(1), image: ImageId(12), found: Vec::new() }),
        };
        assert_eq!(view.images(), [10, 11, 12].map(ImageId).into());
    }

    #[test]
    fn object_settings_invariants() {
        let class = |key: &str| ObjectClass::by_key(key).unwrap();
        assert!(ClassSet::try_from(vec![]).is_err(), "at least one object");
        assert!(ClassSet::try_from(vec![class("car"), class("car")]).is_err());
        assert!(ClassSet::try_from(ObjectClass::all().take(OBJECT_MAX_CLASSES + 1).collect::<Vec<_>>()).is_err());
        assert!(serde_json::from_str::<ClassSet>(r#"["car","unicorn"]"#).is_err());
        assert!(serde_json::from_str::<MinSize>("0.51").is_err());
        assert!(serde_json::from_str::<ClearAfter>("3601").is_err());
        let label = |id: u64, name: &str, parent: &str| OwnLabel { id: LabelId(id), name: Name(name.into()), parent: class(parent) };
        assert!(OwnLabels::try_from(vec![label(1, "Nosso carro", "car"), label(2, "nosso CARRO", "truck")]).is_err());
        assert!(OwnLabels::try_from((0..=MAX_OWN_LABELS as u64).map(|i| label(i, &format!("L{i}"), "car")).collect::<Vec<_>>()).is_err());
        assert!(NormBox::try_from([0.2, 0.2, 0.205, 0.5]).is_err(), "narrower than 1% of the frame");
        assert_eq!(NormBox::between(p(0.9, 1.0), p(0.1, 0.3)).unwrap().area(), [0.1, 0.3, 0.9, 1.0]);
    }

    #[test]
    fn object_keys_are_the_classes_then_the_labels_in_use() {
        let class = |key: &str| ObjectClass::by_key(key).unwrap();
        let mut settings = match Settings::new_objects(Name("Garagem".into()), vec![class("car")].try_into().unwrap()).kind {
            KindSettings::Objects(objects) => objects,
            KindSettings::States(_) => unreachable!(),
        };
        let ours = OwnLabel { id: LabelId(7), name: Name("Nosso carro".into()), parent: class("car") };
        let rex = OwnLabel { id: LabelId(8), name: Name("Rex".into()), parent: class("dog") };
        settings.labels = vec![ours, rex].try_into().unwrap();
        assert_eq!(settings.keys(), [ObjectKey::Class(class("car")), ObjectKey::Own(LabelId(7))], "Rex's dog is not chosen");
        assert_eq!(settings.key_name(ObjectKey::Own(LabelId(7))), "Nosso carro");
        assert_eq!((f32::from(settings.threshold), u8::from(settings.debounce)), (0.60, 1), "origin defaults");
    }
}
