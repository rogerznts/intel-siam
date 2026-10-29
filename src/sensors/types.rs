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
pub const MAX_PEOPLE: usize = 10; // FACES["max_people"]
pub const MAX_FACES_PER_PERSON: usize = 50; // FACES["max_faces_per_person"]
pub const READING_DEFAULT_INTERVAL_S: u32 = 30; // READING_SENSOR_DEFAULTS["interval_s"]
pub const READING_DEFAULT_THRESHOLD: f32 = 0.70; // READING_SENSOR_DEFAULTS["threshold"]
pub const READING_DEFAULT_DEBOUNCE: u8 = 1; // READING_SENSOR_DEFAULTS["debounce"]
pub const MAX_DECIMALS: u8 = 4; // READING_LIMITS["decimals"]
pub const DIGITS_LIMITS: (u8, u8) = (1, 12); // READING_LIMITS["digits"]
pub const DEFAULT_DIGITS: u8 = 6; // READING_DEFAULTS["digits"]
pub const MAX_UNIT_CHARS: usize = 16; // api/common.py: `unit: str = Field(max_length=16)`
pub const MAX_AMOUNT: f64 = 1e9; // READING_LIMITS["max_step"], READING_LIMITS["max_rate"]
pub const RATE_WINDOW_LIMITS_MIN: (u32, u32) = (1, 1440); // READING_LIMITS["rate_window_min"]
pub const DEFAULT_RATE_WINDOW_MIN: u32 = 15; // READING_DEFAULTS["rate_window_min"]
/// The most digits a value keeps; a longer read is no number.
pub const MAX_NUMBER_DIGITS: usize = 18;
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

/// Someone named by their face, known to every object sensor that recognises faces. Chosen by the
/// app when the user names them, so naming is idempotent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PersonId(pub(in crate::sensors) u64);

impl PersonId {
    pub fn random() -> Self {
        Self(SensorId::random().0)
    }
}

/// A taught face (its crop and the person it shows).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FaceId(pub(in crate::sensors) i64);

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
                faces: FaceSettings::default(),
            }),
        }
    }

    /// Origin `READING_SENSOR_DEFAULTS`, with the reading's own settings.
    pub fn new_reading(name: Name, reading: ReadingSettings) -> Self {
        Self {
            name,
            region: Region::WHOLE,
            interval: Interval(READING_DEFAULT_INTERVAL_S),
            triggers: Triggers { regular: true, change_detection: false },
            paused: false,
            kind: KindSettings::Reading(reading),
        }
    }
}

/// Origin `READING_DEFAULTS` with `READING_SENSOR_DEFAULTS`: a counter read as it is shown.
impl Default for ReadingSettings {
    fn default() -> Self {
        Self {
            mode: ReadingMode::Counter {
                limit: ChangeLimit::Auto { max_rate: Amount(0.0) },
                rate_window: RateWindow(DEFAULT_RATE_WINDOW_MIN),
            },
            display: ReadingDisplay::Auto,
            decimals: Decimals(0),
            unit: UnitLabel(String::new()),
            threshold: Unit(READING_DEFAULT_THRESHOLD),
            debounce: Debounce(READING_DEFAULT_DEBOUNCE),
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
    Reading(ReadingSettings),
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
    /// Absent in sensors saved before faces existed: off.
    #[serde(default)]
    pub faces: FaceSettings,
}

/// Recognising people by their face (origin `FACE_DEFAULTS`). Off until the user turns it on; the
/// limits are the origin's constants and the chosen face model's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaceSettings {
    pub enabled: bool,
    /// Put faces nobody was taught for in the review queue ("Quem é?").
    pub ask_unknown: bool,
}

impl ObjectsSettings {
    /// Own labels in use: those whose class is still selected (origin `active_custom`).
    pub fn active_labels(&self) -> impl Iterator<Item = &OwnLabel> {
        self.labels.iter().filter(|l| self.classes.contains(l.parent))
    }

    /// Turned on, and the sensor looks for people (origin `faces_on`).
    pub fn faces_on(&self) -> bool {
        self.faces.enabled && self.classes.contains(ObjectClass::PERSON)
    }

    /// What the sensor reports: its classes, its own labels in use, then, while it recognises
    /// faces, every person and the unknown person (origin `object_keys` with `with_people`).
    pub fn keys(&self, people: &[PersonId]) -> Vec<ObjectKey> {
        let labels = self.active_labels().map(|l| ObjectKey::Own(l.id));
        let faces: Vec<ObjectKey> = if self.faces_on() {
            people.iter().map(|p| ObjectKey::Person(*p)).chain([ObjectKey::Stranger]).collect()
        } else {
            Vec::new()
        };
        self.classes.iter().map(ObjectKey::Class).chain(labels).chain(faces).collect()
    }

    pub fn key_name<'a>(&'a self, key: ObjectKey, people: &'a [PersonView]) -> &'a str {
        match key {
            ObjectKey::Class(class) => class.name(),
            ObjectKey::Own(id) => self.labels.get(id).map_or("?", |l| l.name.as_str()),
            ObjectKey::Person(id) => person_name(people, id),
            ObjectKey::Stranger => "Pessoa desconhecida",
        }
    }
}

pub fn person_name(people: &[PersonView], id: PersonId) -> &str {
    people.iter().find(|p| p.id == id).map_or("?", |p| p.name.as_str())
}

/// How a reading sensor turns what it reads into a value (origin `READING_DEFAULTS` with
/// `READING_SENSOR_DEFAULTS`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadingSettings {
    pub mode: ReadingMode,
    pub display: ReadingDisplay,
    /// Digits after the decimal point; the reader's own dots and commas are ignored. Not used by
    /// a time left, which is whole minutes.
    pub decimals: Decimals,
    pub unit: UnitLabel,
    /// The reader's minimum confidence for a read to count.
    pub threshold: Unit,
    /// Equal readings in a row before a new value is published.
    pub debounce: Debounce,
}

/// What the number is (origin `READING_MODES`).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum ReadingMode {
    /// Only goes up: an energy, water or gas meter. `rate_window`: how far back the flow is
    /// measured.
    Counter { limit: ChangeLimit, rate_window: RateWindow },
    /// Any number; it may change by `max_step` at most either way (0: no limit).
    Value { max_step: Amount },
    /// "h:mm" or minutes on an appliance, as minutes.
    TimeLeft { max_step: Amount },
}

/// How fast a counter may go up (origin `READING_CHANGE_LIMITS`, `change_limit.limit`).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum ChangeLimit {
    /// Learned from the counter's own readings; until then `max_rate` per hour (0: the standard
    /// rate).
    Auto { max_rate: Amount },
    /// `max_rate` per hour in the counter's unit (0: no limit).
    Fixed { max_rate: Amount },
    /// `max_step` for every reading, however long since the last one (0: no limit).
    Step { max_step: Amount },
    Off,
}

/// How the reader treats the region (origin `READING_DISPLAYS`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReadingDisplay {
    /// Read as it is, and without the faint unlit segments when unsure.
    Auto,
    /// Light digits on dark; unlit segments always removed.
    Led,
    /// Dark digits on light; unlit segments always removed.
    Lcd,
    /// A mechanical counter with `digits` rolling wheels; another number of digits is a misread.
    Counter { digits: Digits, reader: CounterReader },
}

/// What reads a mechanical counter (origin `COUNTER_READERS`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CounterReader {
    /// Each wheel's position, so a wheel half way between two digits is read as such.
    Wheels(WheelModel),
    /// The wheels read as text by the number reader.
    Ocr,
}

/// The wheel readers of `readers.json`; each counter names its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum WheelModel {
    #[serde(rename = "wheels-v1")]
    V1,
    #[default]
    #[serde(rename = "wheels-v2")]
    V2,
}

impl WheelModel {
    pub const ALL: [Self; 2] = [Self::V1, Self::V2];

    /// The origin's id, also the model's id in the registry.
    pub const fn id(self) -> &'static str {
        match self {
            Self::V1 => "wheels-v1",
            Self::V2 => "wheels-v2",
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::V1 => "Leitor de rodas v1",
            Self::V2 => "Leitor de rodas v2",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::V1 => "Lê quanto cada roda girou, então uma roda entre dois dígitos sai certa. 2 MB.",
            Self::V2 => "Recomendado. Lê cada roda sozinha e com as vizinhas, então uma região um pouco fora ainda sai certa; desconfia quando alguma roda está confusa. 6 MB.",
        }
    }
}

/// The text recogniser every reading sensor that does not read wheels uses (origin `readers.json`
/// readers; one global choice, as the origin's `reader` setting).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum NumberReader {
    #[default]
    #[serde(rename = "ppocrv6-tiny")]
    Tiny,
    #[serde(rename = "ppocrv6-small")]
    Small,
}

impl NumberReader {
    pub const ALL: [Self; 2] = [Self::Tiny, Self::Small];

    /// The origin's id, also the model's id in the registry.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Tiny => "ppocrv6-tiny",
            Self::Small => "ppocrv6-small",
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Tiny => "PP-OCRv6 tiny (rápido)",
            Self::Small => "PP-OCRv6 small",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Tiny => "Recomendado. Lê números de LCD, LED e impressos em poucos milissegundos. 4 MB.",
            Self::Small => "Modelo maior; pode ajudar com fontes incomuns. 21 MB.",
        }
    }
}

impl ReadingSettings {
    pub fn is_counter(&self) -> bool {
        matches!(self.mode, ReadingMode::Counter { .. })
    }

    /// The decimals a value has: none for a time left.
    pub fn decimals(&self) -> u8 {
        match self.mode {
            ReadingMode::TimeLeft { .. } => 0,
            _ => self.decimals.0,
        }
    }

    /// The wheels of a mechanical counter.
    pub fn digits(&self) -> Option<u8> {
        match self.display {
            ReadingDisplay::Counter { digits, .. } => Some(digits.0),
            _ => None,
        }
    }

    /// The number a read stands for, or `None` (origin `readers.parse`). Only digits count: the
    /// configured decimals place the point, as a stray dot is a common misread. A time left reads
    /// "h:mm" or plain minutes.
    pub fn parse(&self, text: &str) -> Option<Number> {
        let digits = |s: &str| -> String { s.chars().filter(char::is_ascii_digit).collect() };
        if let ReadingMode::TimeLeft { .. } = self.mode {
            if let Some((hours, minutes)) = text.rsplit_once(':') {
                let minutes = digits(minutes);
                if minutes.is_empty() || minutes.len() > 2 {
                    return None;
                }
                let minutes = whole(&minutes)?;
                let hours = digits(hours);
                let hours = if hours.is_empty() { 0 } else { whole(&hours)? };
                return (minutes <= 59).then_some(Number { scaled: hours.checked_mul(60)?.checked_add(minutes)?, decimals: 0 });
            }
            return whole(&digits(text)).map(|minutes| Number { scaled: minutes, decimals: 0 });
        }
        whole(&digits(text)).map(|scaled| Number { scaled, decimals: self.decimals.0 })
    }

    /// The value someone typed as the right one, or `None` (origin `readers.right_value`): with a
    /// point or comma as written ("629.558", "629,558"); digits only as the meter shows them, with
    /// the decimals placed like the reader does ("0629558" → 629.558). A time left: minutes or
    /// "h:mm".
    pub fn typed(&self, text: &str) -> Option<Number> {
        let text = text.trim();
        let all_digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        if let ReadingMode::TimeLeft { .. } = self.mode {
            let fits = match text.split_once(':') {
                Some((hours, minutes)) => all_digits(hours) && all_digits(minutes) && minutes.len() <= 2,
                None => all_digits(text),
            };
            return if fits { self.parse(text) } else { None };
        }
        if all_digits(text) {
            return self.parse(text);
        }
        let value: f64 = text.replace(',', ".").parse().ok()?;
        Number::from_value(value, self.decimals.0)
    }

    /// A counter's flow: its unit and the factor from "per hour" (origin `READING_RATE_UNITS`;
    /// other units get "<unit>/h").
    pub fn rate_unit(&self) -> (String, f64) {
        match self.unit.as_str() {
            "kWh" => ("kW".into(), 1.0),
            "Wh" => ("W".into(), 1.0),
            "m³" => ("m³/h".into(), 1.0),
            "L" => ("L/min".into(), 1.0 / 60.0),
            unit => (format!("{unit}/h"), 1.0),
        }
    }
}

/// Digits, ignoring leading zeros, as a whole number that fits `MAX_NUMBER_DIGITS`.
fn whole(digits: &str) -> Option<i64> {
    let significant = digits.trim_start_matches('0');
    if digits.is_empty() || significant.len() > MAX_NUMBER_DIGITS {
        return None;
    }
    if significant.is_empty() { Some(0) } else { significant.parse().ok() }
}

/// A value read or typed, exactly as shown: `scaled` / 10^`decimals`. Equal values compare
/// equal, so publishing and debouncing never meet a rounding error.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Number {
    pub(in crate::sensors) scaled: i64,
    pub(in crate::sensors) decimals: u8,
}

impl Number {
    pub fn get(self) -> f64 {
        self.scaled as f64 / 10f64.powi(i32::from(self.decimals))
    }

    /// `value` rounded to `decimals`; `None` when it is not finite or too long.
    pub(in crate::sensors) fn from_value(value: f64, decimals: u8) -> Option<Self> {
        let scaled = (value * 10f64.powi(i32::from(decimals))).round();
        (scaled.is_finite() && scaled.abs() < 1e18).then_some(Self { scaled: scaled as i64, decimals })
    }
}

impl fmt::Display for Number {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.scaled < 0 { "-" } else { "" };
        let magnitude = self.scaled.unsigned_abs();
        if self.decimals == 0 {
            return write!(f, "{sign}{magnitude}");
        }
        let unit = 10u64.pow(u32::from(self.decimals));
        write!(f, "{sign}{}.{:0width$}", magnitude / unit, magnitude % unit, width = usize::from(self.decimals))
    }
}

/// Digits after the decimal point, `0..=MAX_DECIMALS`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct Decimals(u8);

impl TryFrom<u8> for Decimals {
    type Error = InvalidInput;
    fn try_from(decimals: u8) -> Result<Self, InvalidInput> {
        if decimals <= MAX_DECIMALS { Ok(Self(decimals)) } else { Err(InvalidInput("casas decimais fora de 0 a 4")) }
    }
}

impl From<Decimals> for u8 {
    fn from(decimals: Decimals) -> u8 {
        decimals.0
    }
}

/// The wheels of a mechanical counter, within `DIGITS_LIMITS`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct Digits(u8);

impl TryFrom<u8> for Digits {
    type Error = InvalidInput;
    fn try_from(digits: u8) -> Result<Self, InvalidInput> {
        let (lo, hi) = DIGITS_LIMITS;
        if (lo..=hi).contains(&digits) { Ok(Self(digits)) } else { Err(InvalidInput("dígitos fora de 1 a 12")) }
    }
}

impl Digits {
    pub const DEFAULT: Self = Self(DEFAULT_DIGITS);
}

impl From<Digits> for u8 {
    fn from(digits: Digits) -> u8 {
        digits.0
    }
}

/// What the value is measured in ("m³", "kWh", "min"), trimmed, up to `MAX_UNIT_CHARS`; may be empty.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct UnitLabel(String);

impl TryFrom<String> for UnitLabel {
    type Error = InvalidInput;
    fn try_from(unit: String) -> Result<Self, InvalidInput> {
        let trimmed = unit.trim();
        if trimmed.chars().count() > MAX_UNIT_CHARS {
            return Err(InvalidInput("a unidade tem no máximo 16 caracteres"));
        }
        Ok(Self(trimmed.to_owned()))
    }
}

impl From<UnitLabel> for String {
    fn from(unit: UnitLabel) -> String {
        unit.0
    }
}

impl UnitLabel {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A step or a rate in the value's unit, `0.0..=MAX_AMOUNT`; 0 is no limit.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
pub struct Amount(f64);

impl TryFrom<f64> for Amount {
    type Error = InvalidInput;
    fn try_from(amount: f64) -> Result<Self, InvalidInput> {
        if (0.0..=MAX_AMOUNT).contains(&amount) { Ok(Self(amount)) } else { Err(InvalidInput("valor fora de 0 a 1e9")) }
    }
}

impl Amount {
    pub const ZERO: Self = Self(0.0);
}

impl From<Amount> for f64 {
    fn from(amount: Amount) -> f64 {
        amount.0
    }
}

/// Minutes a counter's flow is measured over, within `RATE_WINDOW_LIMITS_MIN`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct RateWindow(u32);

impl TryFrom<u32> for RateWindow {
    type Error = InvalidInput;
    fn try_from(minutes: u32) -> Result<Self, InvalidInput> {
        let (lo, hi) = RATE_WINDOW_LIMITS_MIN;
        if (lo..=hi).contains(&minutes) { Ok(Self(minutes)) } else { Err(InvalidInput("janela da vazão fora de 1 a 1440 min")) }
    }
}

impl RateWindow {
    pub const DEFAULT: Self = Self(DEFAULT_RATE_WINDOW_MIN);
}

impl From<RateWindow> for u32 {
    fn from(window: RateWindow) -> u32 {
        window.0
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

/// What an object sensor reports on: a class, an own label, a person, or the unknown person.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ObjectKey {
    Class(ObjectClass),
    Own(LabelId),
    Person(PersonId),
    /// A clearly seen face that matched nobody (origin `FACE_UNKNOWN_KEY`).
    Stranger,
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

    /// Everyone named, as the vision process last said; empty before its first `System`.
    pub fn people(&self) -> &[PersonView] {
        self.system.as_ref().map_or(&[], |s| &s.people)
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
    /// A reading sensor's last accepted value.
    Number(Number),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum KindLive {
    States(StatesLive),
    Objects(ObjectsLive),
    Reading(ReadingLive),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ReadingLive {
    /// The last check: what was read and what came of it. `None` before the first one.
    pub last: Option<ReadRecord>,
    /// Share of the readings of the last 24 hours that were accepted, in percent.
    pub accepted: Option<f32>,
    /// A counter's flow in its `rate_unit`, over about its rate window.
    pub rate: Option<f64>,
    /// How much a counter may go up now; `None`: no limit.
    pub limit: Option<f64>,
    /// What "auto" learned from the counter's busy days, once it has enough of them.
    pub learned: Option<LearnedLimit>,
    /// Days with readings the limit learns from.
    pub days: u32,
    /// The readings of the last day, week and month.
    pub quality: Vec<QualityPeriod>,
    pub review_pending: u32,
}

/// What a counter does on a busy day (origin `change_limit.Learned`): the largest rise over a few
/// minutes, as a rate per hour, and the rise of a day.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LearnedLimit {
    pub per_hour: f64,
    pub per_day: f64,
    pub days: u32,
}

/// One reading: the text the reader saw, how sure it was, the number it stands for and what was
/// decided about it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadRecord {
    pub text: String,
    pub score: f32,
    pub value: Option<Number>,
    pub outcome: ReadOutcome,
    /// Times the same rejected reading was read again while it waited in review.
    #[serde(default)]
    pub repeats: u32,
}

/// What was decided about a reading (origin `reading_verdict`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReadOutcome {
    Accepted,
    /// One step of the last digit below the value: the last wheel turning. The value stays, and it
    /// is no rejection.
    Settling,
    Rejected(ReadProblem),
}

/// Why a reading was rejected (origin `READING_PROBLEMS`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ReadProblem {
    NothingRead,
    Unsure,
    /// A mechanical counter read with another number of digits than it has wheels.
    WrongDigitCount,
    /// A counter below its last value.
    WentDown,
    /// Over the counter's limit, or a value off by more than its step.
    ChangedTooMuch,
    /// The source's picture has another size than the region was drawn on.
    CameraChanged,
}

/// The readings of the last `days` days, today included (origin Quality tab).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct QualityPeriod {
    pub days: u32,
    pub reads: u32,
    pub accepted: u32,
    pub rejected: Vec<(ReadProblem, u32)>,
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
    /// A person box of a sensor that recognises faces: what its face said. Absent in rows
    /// recorded before faces existed.
    #[serde(default)]
    pub face: Option<FaceLook>,
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
                ObjectKey::Person(id) => self.face.and_then(|f| f.person()) == Some(id),
                ObjectKey::Stranger => self.face.is_some_and(|f| f.stranger()),
            }
    }

    /// Whether the box may be the own label or person `key`, but not clearly (origin `unsure_for`).
    pub fn unsure_for(&self, key: ObjectKey) -> bool {
        match key {
            ObjectKey::Own(id) => matches!(self.verdict, Verdict::Unsure { label, .. } if label == id),
            ObjectKey::Person(id) => {
                !self.filtered() && self.face.is_some_and(|f| f.state == FaceState::Ask(id))
            }
            ObjectKey::Class(_) | ObjectKey::Stranger => false,
        }
    }
}

/// Checks in a row a clearly seen face must match nobody before its box counts as the unknown
/// person (origin `FACES["unknown_after"]`).
pub const UNKNOWN_AFTER: u8 = 2;

/// What looking for a face in a person box said, and what the box carries to the next check
/// (origin: a detection's `face`).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FaceLook {
    /// The face found in the box, normalised to the frame.
    pub marks: Option<FaceMarks>,
    /// The face's width in the frame's pixels (origin `face_px`); 0 without a face.
    pub width_px: f32,
    /// Why the face cannot be used; `None`: usable.
    pub problem: Option<FaceProblem>,
    pub state: FaceState,
    /// How close the face is to `candidate`, the closest person.
    pub similarity: f32,
    pub candidate: Option<PersonId>,
    /// Checks in a row whose face matched nobody.
    pub unknown: u8,
    /// The box had a name in an earlier check: it never becomes the unknown person.
    pub named: bool,
    /// Checks since the face was last looked for; 0: looked for in this very frame.
    pub age: u8,
}

impl FaceLook {
    pub fn person(&self) -> Option<PersonId> {
        match self.state {
            FaceState::Match { person, .. } => Some(person),
            _ => None,
        }
    }

    /// Counts as the unknown person: matched nobody `UNKNOWN_AFTER` checks in a row and never
    /// had a name.
    pub fn stranger(&self) -> bool {
        self.person().is_none() && !self.named && self.unknown >= UNKNOWN_AFTER
    }

    pub fn usable(&self) -> bool {
        self.marks.is_some() && self.problem.is_none()
    }
}

/// A face's box and its five points (eyes, nose tip, mouth corners, as seen in the picture),
/// normalised to the frame, and how sure the face detector was.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FaceMarks {
    pub area: [f32; 4],
    pub points: [[f32; 2]; 5],
    pub score: f32,
}

/// Why a face cannot be used (origin `faces.problem`): never someone unknown, only no face.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FaceProblem {
    /// No face in the box.
    Missing,
    /// The frame's edge cuts it.
    Cut,
    Small,
    /// The face detector was not sure it is a face.
    Unsure,
    Turned,
    Blurry,
}

/// Who a person box's face is (origin: a face's `state`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FaceState {
    /// Nothing decided: no usable face, or nobody taught yet.
    Open,
    /// It is `person`. `sure`: at the model's sure similarity, so the next checks keep the name
    /// without looking again. `kept`: no usable face now; the box kept the name it had.
    Match { person: PersonId, sure: bool, kept: bool },
    /// Looks like `person`; a second check must agree first.
    Pending(PersonId),
    /// Maybe `person`, not sure: counted for nobody, and asked about.
    Ask(PersonId),
    /// Small or turned and like nobody: that may only be the angle.
    Unclear,
    /// Clearly seen and like nobody taught.
    Unknown,
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

/// "É <pessoa>?", or with `person` `None` "Quem é?", about the face of one person box (origin
/// `_record_question` about a face). The answer teaches that face.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FaceQuestion {
    pub person: Option<PersonId>,
    pub area: [f32; 4],
    pub face: FaceMarks,
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
    /// The sensor does not recognise faces.
    FacesOff,
    /// A face can be named only when it can be used: a back or a jacket never becomes someone.
    NoFace(FaceProblem),
    /// `MAX_PEOPLE` people are known already.
    PeopleFull,
    /// The person has `MAX_FACES_PER_PERSON` faces already.
    FacesFull,
    /// Another person has that name, ignoring case.
    NameTaken,
    Store(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SystemStatus {
    pub retention: Retention,
    pub usage: StorageUsage,
    pub models: Vec<ModelStatus>,
    /// The detector every object sensor uses.
    pub detector: Detector,
    /// The face model every sensor that recognises faces uses.
    pub recogniser: Recogniser,
    /// The text recogniser every reading sensor that does not read wheels uses.
    pub number_reader: NumberReader,
    /// Everyone named, oldest first, with where they are seen.
    pub people: Vec<PersonView>,
    /// The taught faces are being embedded; meanwhile checks go on without naming anyone.
    pub faces_indexing: bool,
    pub notice: Option<Notice>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PersonView {
    pub id: PersonId,
    pub name: Name,
    pub faces: u32,
    /// When and by which sensor they were last seen in this vision process.
    pub last_seen: Option<Sighting>,
    /// The sensors on which they are present now.
    pub here: Vec<SensorId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sighting {
    pub at: SystemTime,
    pub sensor: SensorId,
}

/// The face model that tells people apart (origin `faces.json` recognisers; one global choice, as
/// the origin's `face_model` setting). YuNet always finds the faces.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Recogniser {
    #[default]
    #[serde(rename = "sface-2021dec")]
    Sface,
    #[serde(rename = "auraface-v1")]
    AuraFace,
}

impl Recogniser {
    pub const ALL: [Self; 2] = [Self::Sface, Self::AuraFace];

    /// The origin's id, also the model's id in the registry.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Sface => "sface-2021dec",
            Self::AuraFace => "auraface-v1",
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Sface => "SFace (rápido)",
            Self::AuraFace => "AuraFace v1 (preciso)",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Sface => "Pequeno e rápido; feito para o YuNet. Recomendado. 39 MB.",
            Self::AuraFace => "Distingue melhor rostos com pouca luz e de lado, mas precisa de uns 600 MB de memória e é lento em CPUs pequenas. 261 MB.",
        }
    }
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
/// last received; the answer is the `FocusView` the vision process pushes. `Tab::People` is about
/// everyone, not one sensor: its `sensor` is ignored.
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
    /// Everyone named and their taught faces, shared by every sensor.
    People,
}

/// The vision process's answer to the current `Interest`, re-sent whole when anything in it
/// changes. Image bytes travel separately, before the first view that references them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FocusView {
    /// `None` for the people, who belong to no sensor.
    pub sensor: Option<SensorId>,
    pub preview: Option<Preview>,
    pub detail: Detail,
    pub opened: Option<OpenedItem>,
    /// A reading sensor on the live tab: its region as the reader saw it at the last check.
    pub seen: Option<ImageId>,
}

impl FocusView {
    /// Every image this view references. Both sides derive the retained set from this, so they
    /// never disagree on which images exist.
    pub fn images(&self) -> BTreeSet<ImageId> {
        let thumbs: Vec<ImageId> = match &self.detail {
            Detail::Live => Vec::new(),
            Detail::Samples { rows, .. } => rows.iter().map(|s| s.thumb).collect(),
            Detail::Review(rows) | Detail::History { rows, .. } => rows.iter().filter_map(|r| r.thumb).collect(),
            Detail::People(faces) => faces.iter().map(|f| f.thumb).collect(),
        };
        let preview = self.preview.as_ref().map(|p| p.image);
        let opened = self.opened.as_ref().map(|o| o.image);
        thumbs.into_iter().chain(preview).chain(opened).chain(self.seen).collect()
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
    /// Every taught face, newest first; the people themselves are in `SystemStatus`.
    People(Vec<FaceItem>),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FaceItem {
    pub id: FaceId,
    pub person: PersonId,
    pub thumb: ImageId,
    pub at: SystemTime,
    /// The sensor it was taught on; `None` once that sensor is deleted (the face stays).
    pub sensor: Option<SensorId>,
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
    /// A reading sensor's reading at that check.
    pub read: Option<ReadRecord>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum ReviewReason {
    LowConfidence,
    Flip,
    SpotCheck,
    /// An object sensor asks whether a box is one of its own labels.
    Ask(Question),
    /// An object sensor that recognises faces asks who a face is.
    Face(FaceQuestion),
    /// A reading sensor rejected the reading: was it read right, and what did the display show?
    Rejected(ReadProblem),
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
    SetRecogniser(Recogniser),
    /// Teaches the face of a person box as `person` (a new one is made with it). Refused unless
    /// the sensor recognises faces and the face can be used. From a review item, it also answers
    /// it.
    NameFace {
        id: SensorId,
        from: FaceFrom,
        person: PersonPick,
    },
    RenamePerson {
        person: PersonId,
        name: Name,
    },
    /// Deletes the person with every face taught for them.
    ForgetPerson(PersonId),
    /// Moves a taught face to another person; `None` deletes it.
    RelabelFace {
        face: FaceId,
        person: Option<PersonId>,
    },
    SetNumberReader(NumberReader),
    /// Answers a rejected reading. When it is newer than the last accepted value, the right value
    /// becomes the last accepted one, and later readings are checked against it.
    AnswerReading {
        id: SensorId,
        item: HistoryId,
        answer: ReadingAnswer,
    },
}

/// Where a face to name is: a person box of a held, checked frame, or a review item's question.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum FaceFrom {
    Frame { frame: FrameId, area: NormBox },
    Review(HistoryId),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PersonPick {
    Known(PersonId),
    /// Made with the first face; naming again with the same id adds to them.
    New { id: PersonId, name: Name },
}

/// What a rejected reading really was (origin review answers `read_ok` and `misread`).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum ReadingAnswer {
    /// It was read right. Only a reading that is a value (unsure, went down, changed too much)
    /// can become the last value.
    ReadRight,
    /// The display showed this.
    Showed(Number),
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
            read: None,
        };
        let view = FocusView {
            sensor: Some(SensorId(1)),
            preview: Some(Preview {
                frame: FrameId(3),
                image: ImageId(10),
                geometry: Geometry { width: 704, height: 480 },
            }),
            detail: Detail::History { rows: vec![item(1, Some(11)), item(2, None)], older: true },
            opened: Some(OpenedItem { id: HistoryId(1), image: ImageId(12), found: Vec::new() }),
            seen: Some(ImageId(13)),
        };
        assert_eq!(view.images(), [10, 11, 12, 13].map(ImageId).into());
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
            _ => unreachable!(),
        };
        let ours = OwnLabel { id: LabelId(7), name: Name("Nosso carro".into()), parent: class("car") };
        let rex = OwnLabel { id: LabelId(8), name: Name("Rex".into()), parent: class("dog") };
        settings.labels = vec![ours, rex].try_into().unwrap();
        assert_eq!(settings.keys(&[PersonId(3)]), [ObjectKey::Class(class("car")), ObjectKey::Own(LabelId(7))], "Rex's dog is not chosen");
        assert_eq!(settings.key_name(ObjectKey::Own(LabelId(7)), &[]), "Nosso carro");
        assert_eq!((f32::from(settings.threshold), u8::from(settings.debounce)), (0.60, 1), "origin defaults");
    }

    #[test]
    fn people_are_keys_only_while_faces_are_recognised_on_people() {
        let class = |key: &str| ObjectClass::by_key(key).unwrap();
        let objects = |classes: Vec<ObjectClass>, enabled: bool| match Settings::new_objects(Name("Porta".into()), classes.try_into().unwrap()).kind {
            KindSettings::Objects(objects) => ObjectsSettings { faces: FaceSettings { enabled, ask_unknown: false }, ..objects },
            _ => unreachable!(),
        };
        let people = [PersonId(1), PersonId(2)];
        assert_eq!(objects(vec![class("person")], false).keys(&people), [ObjectKey::Class(ObjectClass::PERSON)], "off by default");
        assert_eq!(objects(vec![class("car")], true).keys(&people), [ObjectKey::Class(class("car"))], "no people to look at");
        assert_eq!(
            objects(vec![class("person")], true).keys(&people),
            [ObjectKey::Class(ObjectClass::PERSON), ObjectKey::Person(PersonId(1)), ObjectKey::Person(PersonId(2)), ObjectKey::Stranger]
        );
        let old: ObjectsSettings = serde_json::from_str(
            r#"{"classes":["person"],"threshold":0.6,"debounce":1,"min_size":0.0,"clear_after":30,"use_taught":true,"labels":[]}"#,
        )
        .unwrap();
        assert_eq!(old.faces, FaceSettings::default(), "a sensor saved before faces existed has them off");
    }

    fn reading(mode: ReadingMode, decimals: u8) -> ReadingSettings {
        ReadingSettings { mode, decimals: Decimals(decimals), ..ReadingSettings::default() }
    }

    fn counter(decimals: u8) -> ReadingSettings {
        reading(ReadingMode::Counter { limit: ChangeLimit::Off, rate_window: RateWindow(15) }, decimals)
    }

    fn value(decimals: u8) -> ReadingSettings {
        reading(ReadingMode::Value { max_step: Amount(0.0) }, decimals)
    }

    fn timer() -> ReadingSettings {
        reading(ReadingMode::TimeLeft { max_step: Amount(0.0) }, 2)
    }

    fn shown(n: Option<Number>) -> Option<String> {
        n.map(|n| n.to_string())
    }

    #[test]
    fn a_read_is_its_digits_with_the_configured_decimals() {
        assert_eq!(shown(counter(0).parse("74590")), Some("74590".into()));
        assert_eq!(shown(counter(1).parse("0123456.7")), Some("123456.7".into()));
        assert_eq!(shown(value(3).parse("1.6.89")), Some("1.689".into()), "a stray dot is ignored: the decimals decide");
        assert_eq!(shown(value(3).parse("1,799")), Some("1.799".into()));
        assert_eq!(shown(timer().parse("2:38")), Some("158".into()), "minutes, whatever the decimals");
        assert_eq!(shown(timer().parse("0:07")), Some("7".into()));
        assert_eq!(shown(timer().parse("45")), Some("45".into()));
        assert_eq!(timer().parse("2:75"), None, "not a time");
        assert_eq!(counter(0).parse(""), None);
        assert_eq!(counter(0).parse(".:"), None);
        assert_eq!(counter(0).parse(&"9".repeat(19)), None, "longer than a value keeps");
        assert_eq!(shown(counter(0).parse(&format!("{}1", "0".repeat(30)))), Some("1".into()), "leading zeros are no digits to keep");
    }

    #[test]
    fn a_value_shows_its_decimals_and_a_time_left_whole_minutes() {
        assert_eq!(Number::from_value(1.5, 3).unwrap().to_string(), "1.500");
        assert_eq!(timer().parse("158").unwrap().to_string(), "158");
        assert_eq!(Number { scaled: -5, decimals: 3 }.to_string(), "-0.005");
        assert_eq!(Number::from_value(f64::INFINITY, 0), None);
    }

    #[test]
    fn a_typed_value_is_taken_as_written_or_as_the_meter_shows_it() {
        let counter = counter(3);
        assert_eq!(shown(counter.typed("0629558")), Some("629.558".into()));
        assert_eq!(shown(counter.typed("629558")), Some("629.558".into()));
        assert_eq!(shown(counter.typed(" 629.558 ")), Some("629.558".into()));
        assert_eq!(shown(counter.typed("629,558")), Some("629.558".into()));
        assert_eq!(shown(value(0).typed("12")), Some("12".into()));
        assert_eq!(counter.typed("abc"), None);
        assert_eq!(counter.typed("1.2.3"), None);
        assert_eq!(shown(timer().typed("1:25")), Some("85".into()));
        assert_eq!(shown(timer().typed("85")), Some("85".into()));
        assert_eq!(timer().typed("1:75"), None);
        assert_eq!(timer().typed("1.5"), None);
    }

    #[test]
    fn reading_settings_are_valid_or_refused_from_the_wire() {
        let settings = Settings::new_reading(Name("Luz".into()), ReadingSettings::default());
        assert_eq!(serde_json::from_str::<Settings>(&json(&settings)).unwrap(), settings);
        assert!(serde_json::from_str::<Decimals>("5").is_err());
        assert!(serde_json::from_str::<Digits>("0").is_err());
        assert!(serde_json::from_str::<Amount>("-1.0").is_err());
        assert!(UnitLabel::try_from("m".repeat(17)).is_err());
        assert_eq!(counter(0).rate_unit(), ("/h".into(), 1.0));
        let litres = ReadingSettings { unit: UnitLabel("L".into()), ..counter(0) };
        assert_eq!(litres.rate_unit(), ("L/min".into(), 1.0 / 60.0));
    }
}
