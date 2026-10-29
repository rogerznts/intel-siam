//! The sensor database and its image files. The engine owns the one `Store`, so it is the only
//! writer of `<data>/sensors.db` and `<data>/img/`.
//!
//! ```text
//! <data>/sensors.db                     0600, WAL; `PRAGMA user_version` is the schema version
//! <data>/img/s/<sensor>/<sha256>.jpg     sample: the whole frame as taught (a state sensor) or the
//!                                       crop of a taught box (an object sensor), never pruned
//! <data>/img/h/<sensor>/<id>.jpg         history: the whole frame as checked, pruned by retention
//! ```
//!
//! A file is written before its row is committed, so a crash leaves at most an orphan file, which
//! `open` sweeps; never a row without its file. Kind-specific settings are JSON in a column, so a
//! new kind adds variants, not tables.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::infer::Head;
use super::logic::{RowCost, retention_plan};
use crate::frames::StreamKind;
use crate::sensors::types::{
    BoxLabel, Detector, Found, Geometry, HistoryId, LabelId, ObjectClass, ObjectEvent, Retention, ReviewReason,
    SampleId, SampleLabel, SensorId, SensorValue, Settings, SourceSpec, StateKey, StorageUsage,
};

/// Each entry upgrades the schema by one version; `user_version` counts the entries applied.
const MIGRATIONS: &[&str] = &[
    "
    CREATE TABLE sensor (
        id INTEGER PRIMARY KEY,
        source TEXT NOT NULL,
        stream TEXT NOT NULL,
        settings TEXT NOT NULL,
        geometry TEXT,
        created_at INTEGER NOT NULL
    ) STRICT;
    CREATE TABLE sample (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        sensor INTEGER NOT NULL REFERENCES sensor(id) ON DELETE CASCADE,
        sha256 TEXT NOT NULL,
        label INTEGER,
        bytes INTEGER NOT NULL,
        created_at INTEGER NOT NULL,
        UNIQUE (sensor, sha256)
    ) STRICT;
    CREATE TABLE embedding (
        sample INTEGER NOT NULL REFERENCES sample(id) ON DELETE CASCADE,
        backbone TEXT NOT NULL,
        region INTEGER NOT NULL,
        vector BLOB NOT NULL,
        PRIMARY KEY (sample, backbone, region)
    ) STRICT;
    CREATE TABLE head (
        sensor INTEGER PRIMARY KEY REFERENCES sensor(id) ON DELETE CASCADE,
        backbone TEXT NOT NULL,
        region INTEGER NOT NULL,
        head TEXT NOT NULL
    ) STRICT;
    CREATE TABLE history (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        sensor INTEGER NOT NULL REFERENCES sensor(id) ON DELETE CASCADE,
        at INTEGER NOT NULL,
        value TEXT NOT NULL,
        confidence REAL NOT NULL,
        changed INTEGER NOT NULL,
        review TEXT,
        resolved INTEGER NOT NULL DEFAULT 0,
        bytes INTEGER NOT NULL
    ) STRICT;
    CREATE INDEX history_by_sensor ON history (sensor, id);
    CREATE TABLE setting (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;
",
    // Object sensors: a taught box is a sample with `answer` (a `BoxLabel`) and `taught` (a
    // `TaughtBox`) instead of `label`; a history row keeps what changed and the boxes found.
    "
    ALTER TABLE sample ADD COLUMN answer TEXT;
    ALTER TABLE sample ADD COLUMN taught TEXT;
    ALTER TABLE history ADD COLUMN events TEXT;
    ALTER TABLE history ADD COLUMN found TEXT;
",
];

/// The `embedding.region` of a taught box: its crop is stored, so no region applies (origin
/// `teach.EMBEDDING_KEY`).
pub const TAUGHT_CROP: u64 = 0;

#[derive(Debug)]
pub struct StoreError(String);

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self(format!("banco: {e}"))
    }
}

impl From<io::Error> for StoreError {
    fn from(e: io::Error) -> Self {
        Self(format!("arquivo: {e}"))
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        Self(format!("dado inválido no banco: {e}"))
    }
}

pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// A sensor as persisted. A URL source keeps its password here (0600 file).
#[derive(Clone, Debug)]
pub struct SensorRecord {
    pub id: SensorId,
    pub source: SourceSpec,
    pub stream: StreamKind,
    pub settings: Settings,
    /// The source's size when the sensor was last saved; `None` until the first frame.
    pub geometry: Option<Geometry>,
}

/// A head with what it is valid for.
pub struct StoredHead {
    pub backbone: String,
    pub region: u64,
    pub head: Head,
}

/// A labelled sample for a retrain: its cached embedding for (backbone, region), or the image to
/// embed.
pub struct TrainRow {
    pub sample: SampleId,
    pub label: StateKey,
    pub cached: Option<Vec<f32>>,
    pub image: PathBuf,
}

/// Where a taught box was on its frame, and what the detector had called it (`None`: drawn by
/// the user around something it missed).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaughtBox {
    pub area: [f32; 4],
    pub detected: Option<ObjectClass>,
}

/// A taught box for an index: its cached embedding, or the crop to embed.
pub struct TaughtSample {
    pub sample: SampleId,
    pub label: BoxLabel,
    pub detected: Option<ObjectClass>,
    pub cached: Option<Vec<f32>>,
    pub image: PathBuf,
}

pub struct NewHistory<'a> {
    pub at: SystemTime,
    pub value: &'a SensorValue,
    pub confidence: f32,
    pub changed: bool,
    pub review: Option<ReviewReason>,
    pub jpeg: &'a [u8],
    pub events: &'a [ObjectEvent],
    pub found: &'a [Found],
}

pub struct StoredSample {
    pub id: SampleId,
    pub label: Option<SampleLabel>,
    pub at: SystemTime,
    pub image: PathBuf,
}

pub struct StoredHistory {
    pub id: HistoryId,
    pub at: SystemTime,
    pub value: SensorValue,
    pub confidence: f32,
    pub review: Option<ReviewReason>,
    pub events: Vec<ObjectEvent>,
    pub image: PathBuf,
}

/// What a sensor has learned and what waits for the user.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Counts {
    /// State samples per state.
    pub states: BTreeMap<StateKey, u32>,
    pub taught: u32,
    pub pending: u32,
}

pub struct Store {
    conn: Connection,
    dir: PathBuf,
}

impl Store {
    /// Creates or migrates the database, then sweeps files no row references.
    pub fn open(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir)?;
        let path = dir.join("sensors.db");
        restrict(&path)?;
        let mut conn = Connection::open(&path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        let version = usize::try_from(version).unwrap_or(usize::MAX);
        if version > MIGRATIONS.len() {
            return Err(StoreError(format!("banco da versão {version}, mais nova que este programa")));
        }
        for (i, migration) in MIGRATIONS.iter().enumerate().skip(version) {
            let tx = conn.transaction()?;
            tx.execute_batch(migration)?;
            tx.pragma_update(None, "user_version", i as i64 + 1)?;
            tx.commit()?;
        }
        let store = Self { conn, dir: dir.to_owned() };
        store.sweep()?;
        Ok(store)
    }

    pub fn sensors(&self) -> Result<Vec<SensorRecord>> {
        let mut stmt = self.conn.prepare("SELECT id, source, stream, settings, geometry FROM sensor ORDER BY id")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, Option<String>>(4)?))
        })?;
        rows.map(|row| {
            let (id, source, stream, settings, geometry) = row?;
            Ok(SensorRecord {
                id: sensor_id(id),
                source: serde_json::from_str(&source)?,
                stream: serde_json::from_str(&stream)?,
                settings: serde_json::from_str(&settings)?,
                geometry: geometry.as_deref().map(serde_json::from_str).transpose()?,
            })
        })
        .collect()
    }

    /// Replaying a `Create` changes nothing.
    pub fn create(&mut self, rec: &SensorRecord) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO sensor (id, source, stream, settings, geometry, created_at) VALUES (?, ?, ?, ?, ?, ?)",
            params![
                key(rec.id),
                json(&rec.source)?,
                json(&rec.stream)?,
                json(&rec.settings)?,
                rec.geometry.as_ref().map(json).transpose()?,
                millis(SystemTime::now())
            ],
        )?;
        Ok(())
    }

    /// Writes the settings and geometry; source and stream never change.
    pub fn update(&mut self, rec: &SensorRecord) -> Result<()> {
        self.conn.execute(
            "UPDATE sensor SET settings = ?, geometry = ? WHERE id = ?",
            params![json(&rec.settings)?, rec.geometry.as_ref().map(json).transpose()?, key(rec.id)],
        )?;
        Ok(())
    }

    /// The rows (by cascade), then the files.
    pub fn delete(&mut self, id: SensorId) -> Result<()> {
        self.conn.execute("DELETE FROM sensor WHERE id = ?", [key(id)])?;
        for kind in ["s", "h"] {
            remove_dir(&self.dir.join("img").join(kind).join(folder(id)))?;
        }
        Ok(())
    }

    /// The sample is the image's SHA-256, so teaching the same frame again relabels it.
    pub fn add_sample(&mut self, id: SensorId, jpeg: &[u8], label: StateKey) -> Result<SampleId> {
        let sha = self.put_sample_file(id, jpeg)?;
        let sample = self.conn.query_row(
            "INSERT INTO sample (sensor, sha256, label, bytes, created_at) VALUES (?, ?, ?, ?, ?)
             ON CONFLICT (sensor, sha256) DO UPDATE SET label = excluded.label RETURNING id",
            params![key(id), sha, u8::from(label), jpeg.len() as i64, millis(SystemTime::now())],
            |r| r.get(0),
        )?;
        Ok(SampleId(sample))
    }

    /// A taught box of an object sensor: `jpeg` is its crop. The same crop again relabels it.
    pub fn add_taught(&mut self, id: SensorId, jpeg: &[u8], label: BoxLabel, taught: TaughtBox) -> Result<SampleId> {
        let sha = self.put_sample_file(id, jpeg)?;
        let sample = self.conn.query_row(
            "INSERT INTO sample (sensor, sha256, answer, taught, bytes, created_at) VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT (sensor, sha256) DO UPDATE SET answer = excluded.answer RETURNING id",
            params![key(id), sha, json(&label)?, json(&taught)?, jpeg.len() as i64, millis(SystemTime::now())],
            |r| r.get(0),
        )?;
        Ok(SampleId(sample))
    }

    fn put_sample_file(&self, id: SensorId, jpeg: &[u8]) -> Result<String> {
        let sha = hex(&Sha256::digest(jpeg));
        let path = self.sample_path(id, &sha);
        if !path.is_file() {
            write_atomic(&path, jpeg)?;
        }
        Ok(sha)
    }

    /// `None` deletes the sample. `false` when the sensor has no such sample of that kind.
    pub fn relabel(&mut self, id: SensorId, sample: SampleId, label: Option<SampleLabel>) -> Result<bool> {
        let changed = match label {
            None => {
                let sha: Option<String> = self
                    .conn
                    .query_row("DELETE FROM sample WHERE id = ? AND sensor = ? RETURNING sha256", params![sample.0, key(id)], |r| {
                        r.get(0)
                    })
                    .optional()?;
                let Some(sha) = sha else { return Ok(false) };
                remove_file(&self.sample_path(id, &sha))?;
                1
            }
            Some(SampleLabel::State(state)) => self.conn.execute(
                "UPDATE sample SET label = ? WHERE id = ? AND sensor = ? AND taught IS NULL",
                params![u8::from(state), sample.0, key(id)],
            )?,
            Some(SampleLabel::Box(label)) => self.conn.execute(
                "UPDATE sample SET answer = ? WHERE id = ? AND sensor = ? AND taught IS NOT NULL",
                params![json(&label)?, sample.0, key(id)],
            )?,
        };
        Ok(changed == 1)
    }

    /// Deletes the boxes taught as these own labels, with their files.
    pub fn forget_labels(&mut self, id: SensorId, labels: &[LabelId]) -> Result<()> {
        for label in labels {
            let mut stmt = self.conn.prepare("DELETE FROM sample WHERE sensor = ? AND answer = ? RETURNING sha256")?;
            let shas: Vec<String> = stmt.query_map(params![key(id), json(&BoxLabel::Own(*label))?], |r| r.get(0))?.collect::<Result<_, _>>()?;
            for sha in shas {
                remove_file(&self.sample_path(id, &sha))?;
            }
        }
        Ok(())
    }

    pub fn counts(&self, id: SensorId) -> Result<Counts> {
        let mut stmt =
            self.conn.prepare("SELECT label, COUNT(*) FROM sample WHERE sensor = ? AND label IS NOT NULL GROUP BY label")?;
        let mut states = BTreeMap::new();
        for row in stmt.query_map([key(id)], |r| Ok((r.get::<_, u8>(0)?, r.get::<_, u32>(1)?)))? {
            let (label, count) = row?;
            states.insert(state_key(label)?, count);
        }
        let taught = self.conn.query_row("SELECT COUNT(*) FROM sample WHERE sensor = ? AND answer IS NOT NULL", [key(id)], |r| r.get(0))?;
        let pending = self.conn.query_row(
            "SELECT COUNT(*) FROM history WHERE sensor = ? AND review IS NOT NULL AND resolved = 0",
            [key(id)],
            |r| r.get(0),
        )?;
        Ok(Counts { states, taught, pending })
    }

    /// The labelled samples whose label is one of `labels`.
    pub fn training_set(&self, id: SensorId, labels: &BTreeSet<StateKey>, backbone: &str, region: u64) -> Result<Vec<TrainRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.label, s.sha256, e.vector FROM sample s
             LEFT JOIN embedding e ON e.sample = s.id AND e.backbone = ? AND e.region = ?
             WHERE s.sensor = ? AND s.label IS NOT NULL ORDER BY s.id",
        )?;
        let rows = stmt.query_map(params![backbone, region as i64, key(id)], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, u8>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<Vec<u8>>>(3)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (sample, label, sha, vector) = row?;
            let label = state_key(label)?;
            if labels.contains(&label) {
                out.push(TrainRow { sample: SampleId(sample), label, cached: vector.map(floats), image: self.sample_path(id, &sha) });
            }
        }
        Ok(out)
    }

    /// Every taught box of an object sensor, oldest first.
    pub fn taught_set(&self, id: SensorId, backbone: &str) -> Result<Vec<TaughtSample>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.answer, s.taught, s.sha256, e.vector FROM sample s
             LEFT JOIN embedding e ON e.sample = s.id AND e.backbone = ? AND e.region = ?
             WHERE s.sensor = ? AND s.answer IS NOT NULL ORDER BY s.id",
        )?;
        let rows = stmt.query_map(params![backbone, TAUGHT_CROP as i64, key(id)], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, Option<Vec<u8>>>(4)?))
        })?;
        rows.map(|row| {
            let (sample, answer, taught, sha, vector) = row?;
            let taught: TaughtBox = serde_json::from_str(&taught)?;
            Ok(TaughtSample {
                sample: SampleId(sample),
                label: serde_json::from_str(&answer)?,
                detected: taught.detected,
                cached: vector.map(floats),
                image: self.sample_path(id, &sha),
            })
        })
        .collect()
    }

    /// Caches embeddings computed for (backbone, region).
    pub fn put_embeddings(&mut self, backbone: &str, region: u64, fresh: &[(SampleId, Vec<f32>)]) -> Result<()> {
        let tx = self.conn.transaction()?;
        insert_embeddings(&tx, backbone, region, fresh)?;
        tx.commit()?;
        Ok(())
    }

    /// Caches the new embeddings and keeps the head, or forgets it when there is none.
    pub fn put_training(
        &mut self,
        id: SensorId,
        backbone: &str,
        region: u64,
        head: Option<&Head>,
        fresh: &[(SampleId, Vec<f32>)],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        insert_embeddings(&tx, backbone, region, fresh)?;
        match head {
            Some(head) => tx.execute(
                "INSERT OR REPLACE INTO head (sensor, backbone, region, head) VALUES (?, ?, ?, ?)",
                params![key(id), backbone, region as i64, json(head)?],
            )?,
            None => tx.execute("DELETE FROM head WHERE sensor = ?", [key(id)])?,
        };
        tx.commit()?;
        Ok(())
    }

    pub fn heads(&self) -> Result<Vec<(SensorId, StoredHead)>> {
        let mut stmt = self.conn.prepare("SELECT sensor, backbone, region, head FROM head")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?))
        })?;
        rows.map(|row| {
            let (id, backbone, region, head) = row?;
            Ok((sensor_id(id), StoredHead { backbone, region: region as u64, head: serde_json::from_str(&head)? }))
        })
        .collect()
    }

    /// The frame is on disk before its row exists. The engine is the only writer, so the next id is
    /// known before the insert; ids are never reused, so an image id the app holds stays its own.
    pub fn record(&mut self, id: SensorId, h: &NewHistory<'_>) -> Result<HistoryId> {
        let item = HistoryId(self.conn.query_row(
            "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'history'), 0) + 1",
            [],
            |r| r.get(0),
        )?);
        write_atomic(&self.history_path(id, item), h.jpeg)?;
        let list = |empty: bool, value: String| (!empty).then_some(value);
        self.conn.execute(
            "INSERT INTO history (id, sensor, at, value, confidence, changed, review, bytes, events, found)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                item.0,
                key(id),
                millis(h.at),
                json(h.value)?,
                f64::from(h.confidence),
                h.changed,
                h.review.as_ref().map(json).transpose()?,
                h.jpeg.len() as i64,
                list(h.events.is_empty(), json(h.events)?),
                list(h.found.is_empty(), json(h.found)?),
            ],
        )?;
        Ok(item)
    }

    pub fn samples(&self, id: SensorId, before: Option<SampleId>, limit: usize) -> Result<(Vec<StoredSample>, Option<SampleId>)> {
        let mut stmt = self.conn.prepare(
            "SELECT id, label, answer, created_at, sha256 FROM sample WHERE sensor = ? AND id < ? ORDER BY id DESC LIMIT ?",
        )?;
        let rows = stmt.query_map(params![key(id), before.map_or(i64::MAX, |s| s.0), limit as i64 + 1], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Option<u8>>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, i64>(3)?, r.get::<_, String>(4)?))
        })?;
        let mut rows = rows
            .map(|row| {
                let (sample, label, answer, at, sha) = row?;
                let label = match (label, answer) {
                    (Some(state), _) => Some(SampleLabel::State(state_key(state)?)),
                    (None, Some(answer)) => Some(SampleLabel::Box(serde_json::from_str(&answer)?)),
                    (None, None) => None,
                };
                Ok(StoredSample { id: SampleId(sample), label, at: time(at), image: self.sample_path(id, &sha) })
            })
            .collect::<Result<Vec<_>>>()?;
        let older = (rows.len() > limit).then(|| rows[limit - 1].id);
        rows.truncate(limit);
        Ok((rows, older))
    }

    /// Items waiting for review, newest first.
    pub fn reviews(&self, id: SensorId, limit: usize) -> Result<Vec<StoredHistory>> {
        self.history_rows(
            id,
            "WHERE sensor = ?1 AND review IS NOT NULL AND resolved = 0 ORDER BY id DESC LIMIT ?2",
            params![key(id), limit as i64],
        )
    }

    /// A page of history, newest first, strictly older than `before`; and whether older rows exist.
    pub fn history(&self, id: SensorId, before: Option<HistoryId>, limit: usize) -> Result<(Vec<StoredHistory>, bool)> {
        let before = before.map_or(i64::MAX, |h| h.0);
        let mut rows =
            self.history_rows(id, "WHERE sensor = ?1 AND id < ?2 ORDER BY id DESC LIMIT ?3", params![key(id), before, limit as i64 + 1])?;
        let older = rows.len() > limit;
        rows.truncate(limit);
        Ok((rows, older))
    }

    /// One history item of this sensor.
    pub fn history_item(&self, id: SensorId, item: HistoryId) -> Result<Option<StoredHistory>> {
        Ok(self.history_rows(id, "WHERE sensor = ?1 AND id = ?2", params![key(id), item.0])?.pop())
    }

    /// The frame recorded for a history item of this sensor, and the boxes found in it.
    pub fn history_image(&self, id: SensorId, item: HistoryId) -> Result<Option<(PathBuf, Vec<Found>)>> {
        let found: Option<Option<String>> = self
            .conn
            .query_row("SELECT found FROM history WHERE id = ? AND sensor = ?", params![item.0, key(id)], |r| r.get(0))
            .optional()?;
        let Some(found) = found else { return Ok(None) };
        let found = found.as_deref().map(serde_json::from_str).transpose()?.unwrap_or_default();
        Ok(Some((self.history_path(id, item), found)))
    }

    /// Takes a review item out of the queue.
    pub fn mark_resolved(&mut self, item: HistoryId) -> Result<()> {
        self.conn.execute("UPDATE history SET resolved = 1 WHERE id = ?", [item.0])?;
        Ok(())
    }

    pub fn retention(&self) -> Result<Retention> {
        self.setting("retention").map(Option::unwrap_or_default)
    }

    pub fn set_retention(&mut self, retention: Retention) -> Result<()> {
        self.set_setting("retention", &retention)
    }

    pub fn detector(&self) -> Result<Detector> {
        self.setting("detector").map(Option::unwrap_or_default)
    }

    pub fn set_detector(&mut self, detector: Detector) -> Result<()> {
        self.set_setting("detector", &detector)
    }

    fn setting<T: for<'de> Deserialize<'de>>(&self, name: &str) -> Result<Option<T>> {
        let value: Option<String> =
            self.conn.query_row("SELECT value FROM setting WHERE key = ?", [name], |r| r.get(0)).optional()?;
        Ok(value.as_deref().map(serde_json::from_str).transpose()?)
    }

    fn set_setting<T: Serialize>(&mut self, name: &str, value: &T) -> Result<()> {
        self.conn.execute("INSERT OR REPLACE INTO setting (key, value) VALUES (?, ?)", params![name, json(value)?])?;
        Ok(())
    }

    /// Deletes the history rows and frames `logic::retention_plan` picks. Samples are never pruned.
    pub fn enforce_retention(&mut self, now: SystemTime) -> Result<usize> {
        let policy = self.retention()?;
        let mut stmt = self.conn.prepare(
            "SELECT id, sensor, at, bytes, review IS NOT NULL AND resolved = 0 FROM history ORDER BY at, id",
        )?;
        let mut owners = BTreeMap::new();
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, bool>(4)?))
        })?;
        let mut costs = Vec::new();
        for row in rows {
            let (item, sensor, at, bytes, pending_review) = row?;
            owners.insert(item, sensor_id(sensor));
            let age = now.duration_since(time(at)).unwrap_or(Duration::ZERO);
            costs.push(RowCost { id: HistoryId(item), age, bytes: bytes as u64, pending_review });
        }
        drop(stmt);
        let doomed = retention_plan(&costs, policy);
        let tx = self.conn.transaction()?;
        for item in &doomed {
            tx.execute("DELETE FROM history WHERE id = ?", [item.0])?;
        }
        tx.commit()?;
        for item in &doomed {
            remove_file(&self.history_path(owners[&item.0], *item))?;
        }
        Ok(doomed.len())
    }

    pub fn usage(&self) -> Result<StorageUsage> {
        let sum = |table: &str| -> Result<u64> {
            let bytes: i64 = self.conn.query_row(&format!("SELECT COALESCE(SUM(bytes), 0) FROM {table}"), [], |r| r.get(0))?;
            Ok(bytes as u64)
        };
        Ok(StorageUsage { history_bytes: sum("history")?, sample_bytes: sum("sample")? })
    }

    fn history_rows(&self, id: SensorId, filter: &str, args: impl rusqlite::Params) -> Result<Vec<StoredHistory>> {
        let mut stmt =
            self.conn.prepare(&format!("SELECT id, at, value, confidence, review, events FROM history {filter}"))?;
        let rows = stmt.query_map(args, |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, f64>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?;
        rows.map(|row| {
            let (item, at, value, confidence, review, events) = row?;
            Ok(StoredHistory {
                id: HistoryId(item),
                at: time(at),
                value: serde_json::from_str(&value)?,
                confidence: confidence as f32,
                review: review.as_deref().map(serde_json::from_str).transpose()?,
                events: events.as_deref().map(serde_json::from_str).transpose()?.unwrap_or_default(),
                image: self.history_path(id, HistoryId(item)),
            })
        })
        .collect()
    }

    fn sample_path(&self, id: SensorId, sha: &str) -> PathBuf {
        self.dir.join("img").join("s").join(folder(id)).join(format!("{sha}.jpg"))
    }

    fn history_path(&self, id: SensorId, item: HistoryId) -> PathBuf {
        self.dir.join("img").join("h").join(folder(id)).join(format!("{}.jpg", item.0))
    }

    /// Removes files no row references: left by a crash between a file and its row, or by a
    /// sensor deleted while its files could not be removed.
    fn sweep(&self) -> Result<()> {
        let sensors: BTreeSet<String> =
            self.sensors()?.iter().map(|s| folder(s.id)).collect();
        let mut samples = BTreeSet::new();
        let mut stmt = self.conn.prepare("SELECT sensor, sha256 FROM sample")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
            let (sensor, sha) = row?;
            samples.insert(self.sample_path(sensor_id(sensor), &sha));
        }
        let mut history = BTreeSet::new();
        let mut stmt = self.conn.prepare("SELECT sensor, id FROM history")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))? {
            let (sensor, item) = row?;
            history.insert(self.history_path(sensor_id(sensor), HistoryId(item)));
        }
        for (kind, kept) in [("s", &samples), ("h", &history)] {
            for dir in entries(&self.dir.join("img").join(kind))? {
                let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                if !sensors.contains(name) {
                    remove_dir(&dir)?;
                    continue;
                }
                for file in entries(&dir)? {
                    if !kept.contains(&file) {
                        remove_file(&file)?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// The sample may have been deleted while it was being embedded: its vector is then dropped.
fn insert_embeddings(tx: &rusqlite::Transaction<'_>, backbone: &str, region: u64, fresh: &[(SampleId, Vec<f32>)]) -> Result<()> {
    let mut insert = tx.prepare("INSERT OR REPLACE INTO embedding (sample, backbone, region, vector) VALUES (?, ?, ?, ?)")?;
    for (sample, vector) in fresh {
        let bytes: Vec<u8> = vector.iter().flat_map(|v| v.to_le_bytes()).collect();
        if let Err(e) = insert.execute(params![sample.0, backbone, region as i64, bytes])
            && !matches!(e.sqlite_error_code(), Some(rusqlite::ErrorCode::ConstraintViolation))
        {
            return Err(e.into());
        }
    }
    Ok(())
}

fn floats(bytes: Vec<u8>) -> Vec<f32> {
    bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect()
}

/// The database file exists with mode 0600 before SQLite opens it; SQLite gives its WAL and
/// shared-memory files the same mode.
fn restrict(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        fs::OpenOptions::new().create(true).append(true).mode(0o600).open(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    fs::OpenOptions::new().create(true).append(true).open(path)?;
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    fs::create_dir_all(path.parent().expect("image paths have a folder"))?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

fn entries(dir: &Path) -> io::Result<Vec<PathBuf>> {
    match fs::read_dir(dir) {
        Ok(read) => read.map(|e| e.map(|e| e.path())).collect(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

fn remove_file(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn remove_dir(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn folder(id: SensorId) -> String {
    format!("{:016x}", id.0)
}

/// SQLite integers are signed; the id's bits are kept as they are.
fn key(id: SensorId) -> i64 {
    id.0 as i64
}

fn sensor_id(key: i64) -> SensorId {
    SensorId(key as u64)
}

fn state_key(label: u8) -> Result<StateKey> {
    StateKey::try_from(label).map_err(|e| StoreError(format!("dado inválido no banco: {e}")))
}

fn json<T: Serialize + ?Sized>(value: &T) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}

fn millis(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

fn time(ms: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64)
}

pub(super) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sensors::types::{Name, ObjectKey, StateDef};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("intel-siam-store-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn label(k: u8) -> StateKey {
        k.try_into().unwrap()
    }

    fn record(id: u64) -> SensorRecord {
        let def = |k: u8, name: &str| StateDef { key: label(k), name: Name::try_from(name.to_owned()).unwrap() };
        let states = vec![def(1, "Aberto"), def(2, "Fechado")].try_into().unwrap();
        SensorRecord {
            id: SensorId(id),
            source: SourceSpec::Url("http://user:pw@cam.local/snap.jpg".to_owned().try_into().unwrap()),
            stream: StreamKind::Sub,
            settings: Settings::new_states("Portão".to_owned().try_into().unwrap(), states),
            geometry: None,
        }
    }

    fn history<'a>(at: SystemTime, value: &'a SensorValue, review: Option<ReviewReason>, jpeg: &'static [u8]) -> NewHistory<'a> {
        NewHistory { at, value, confidence: 0.5, changed: true, review, jpeg, events: &[], found: &[] }
    }

    #[test]
    fn database_is_private_and_survives_reopening() {
        let dir = temp_dir("reopen");
        let mut store = Store::open(&dir).unwrap();
        store.create(&record(u64::MAX - 1)).unwrap();
        store.create(&record(u64::MAX - 1)).unwrap();
        drop(store);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(dir.join("sensors.db")).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let store = Store::open(&dir).unwrap();
        let sensors = store.sensors().unwrap();
        assert_eq!(sensors.len(), 1);
        assert_eq!(sensors[0].id, SensorId(u64::MAX - 1), "ids above i64::MAX round-trip");
    }

    #[test]
    fn samples_are_content_addressed_and_deleted_with_their_file() {
        let dir = temp_dir("samples");
        let mut store = Store::open(&dir).unwrap();
        let id = SensorId(7);
        store.create(&record(7)).unwrap();
        let a = store.add_sample(id, b"frame a", label(1)).unwrap();
        assert_eq!(store.add_sample(id, b"frame a", label(2)).unwrap(), a, "same image, same sample");
        store.add_sample(id, b"frame b", label(2)).unwrap();
        assert_eq!(store.counts(id).unwrap().states, [(label(2), 2)].into());
        let path = store.samples(id, None, 100).unwrap().0.into_iter().find(|s| s.id == a).unwrap().image;
        assert!(path.is_file());
        assert!(store.relabel(id, a, None).unwrap());
        assert!(!path.exists());
        assert!(!store.relabel(SensorId(8), a, Some(SampleLabel::State(label(1)))).unwrap(), "another sensor's sample");
    }

    #[test]
    fn taught_boxes_keep_their_answer_and_go_with_their_own_label() {
        let dir = temp_dir("taught");
        let mut store = Store::open(&dir).unwrap();
        let id = SensorId(9);
        store.create(&record(9)).unwrap();
        let car = ObjectClass::by_key("car").unwrap();
        let ours = LabelId(u64::MAX - 3);
        let taught = TaughtBox { area: [0.1, 0.2, 0.3, 0.4], detected: Some(car) };
        let a = store.add_taught(id, b"crop a", BoxLabel::Own(ours), taught).unwrap();
        let b = store.add_taught(id, b"crop b", BoxLabel::NotThis, TaughtBox { detected: None, ..taught }).unwrap();
        assert_eq!(store.counts(id).unwrap().taught, 2);
        let set = store.taught_set(id, "backbone").unwrap();
        assert_eq!(set.iter().map(|t| (t.sample, t.label, t.detected)).collect::<Vec<_>>(), [(a, BoxLabel::Own(ours), Some(car)), (b, BoxLabel::NotThis, None)]);
        store.put_embeddings("backbone", TAUGHT_CROP, &[(a, vec![0.5, -1.0])]).unwrap();
        assert_eq!(store.taught_set(id, "backbone").unwrap()[0].cached, Some(vec![0.5, -1.0]));
        assert!(!store.relabel(id, a, Some(SampleLabel::State(label(1)))).unwrap(), "a box has no state");
        assert!(store.relabel(id, b, Some(SampleLabel::Box(BoxLabel::Class(car)))).unwrap());
        let shown: Vec<Option<SampleLabel>> = store.samples(id, None, 10).unwrap().0.into_iter().map(|s| s.label).collect();
        assert_eq!(shown, [Some(SampleLabel::Box(BoxLabel::Class(car))), Some(SampleLabel::Box(BoxLabel::Own(ours)))]);
        let file = store.taught_set(id, "backbone").unwrap()[0].image.clone();
        store.forget_labels(id, &[ours]).unwrap();
        assert_eq!(store.taught_set(id, "backbone").unwrap().len(), 1);
        assert!(!file.exists());
    }

    #[test]
    fn history_pages_reviews_and_resolving() {
        let dir = temp_dir("history");
        let mut store = Store::open(&dir).unwrap();
        let id = SensorId(3);
        store.create(&record(3)).unwrap();
        let value = SensorValue::State(label(1));
        let items: Vec<HistoryId> = (0..5)
            .map(|i| {
                let review = (i == 1).then_some(ReviewReason::LowConfidence);
                store.record(id, &history(SystemTime::now(), &value, review, b"jpeg")).unwrap()
            })
            .collect();
        let (page, older) = store.history(id, None, 3).unwrap();
        assert_eq!(page.iter().map(|h| h.id).collect::<Vec<_>>(), [items[4], items[3], items[2]]);
        assert!(older);
        let (rest, older) = store.history(id, Some(items[2]), 3).unwrap();
        assert_eq!(rest.len(), 2);
        assert!(!older);
        assert_eq!(store.reviews(id, 10).unwrap()[0].id, items[1]);
        store.mark_resolved(items[1]).unwrap();
        assert!(store.reviews(id, 10).unwrap().is_empty());
        assert_eq!(store.counts(id).unwrap().pending, 0);
    }

    #[test]
    fn object_history_keeps_events_and_boxes() {
        let dir = temp_dir("objects");
        let mut store = Store::open(&dir).unwrap();
        let id = SensorId(5);
        store.create(&record(5)).unwrap();
        let car = ObjectClass::by_key("car").unwrap();
        let value = SensorValue::Objects(vec![(ObjectKey::Class(car), 2)]);
        let found = [Found { class: car, score: 0.9, area: [0.1, 0.1, 0.5, 0.5], verdict: crate::sensors::types::Verdict::Detector }];
        let events = [ObjectEvent::On(ObjectKey::Class(car))];
        let row = NewHistory { events: &events, found: &found, ..history(SystemTime::now(), &value, None, b"jpeg") };
        let item = store.record(id, &row).unwrap();
        let stored = store.history_item(id, item).unwrap().unwrap();
        assert_eq!((stored.value, stored.events), (value, events.to_vec()));
        assert_eq!(store.history_image(id, item).unwrap().unwrap().1, found);
        assert_eq!(store.history_image(SensorId(6), item).unwrap(), None, "another sensor's item");
    }

    #[test]
    fn the_detector_choice_persists() {
        let dir = temp_dir("detector");
        let mut store = Store::open(&dir).unwrap();
        assert_eq!(store.detector().unwrap(), Detector::DfineS);
        store.set_detector(Detector::DfineN).unwrap();
        drop(store);
        assert_eq!(Store::open(&dir).unwrap().detector().unwrap(), Detector::DfineN);
    }

    #[test]
    fn retention_prunes_rows_and_files_and_sweep_removes_orphans() {
        let dir = temp_dir("retention");
        let mut store = Store::open(&dir).unwrap();
        let id = SensorId(4);
        store.create(&record(4)).unwrap();
        let value = SensorValue::Unknown;
        let old = SystemTime::now() - Duration::from_secs(8 * 24 * 3600);
        let item = store.record(id, &history(old, &value, None, b"old")).unwrap();
        let file = store.history_path(id, item);
        assert_eq!(store.enforce_retention(SystemTime::now()).unwrap(), 1);
        assert!(!file.exists());
        let orphan = store.history_path(id, HistoryId(99));
        write_atomic(&orphan, b"x").unwrap();
        let stray = dir.join("img/s/00000000000000ff/x.jpg");
        write_atomic(&stray, b"x").unwrap();
        drop(store);
        Store::open(&dir).unwrap();
        assert!(!orphan.exists());
        assert!(!stray.exists());
    }
}
