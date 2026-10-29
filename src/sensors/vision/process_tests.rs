//! The `intel-siam-vision` binary end to end: a real process in a temporary directory, spoken to
//! through the codec over its real stdin and stdout. It needs no network: an untaught sensor never
//! downloads a model, and a DVR source has no feed until the app sends its URL.

use std::fs;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command as Process, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use crate::frames::{StreamKey, StreamKind};
use crate::model::{CameraId, ChannelIndex, DvrSerial};
use crate::sensors::types::*;
use crate::sensors::wire::{Command, Desired, PROTOCOL, Report, read_frame, write_frame};

const PATIENCE: Duration = Duration::from_secs(10);

/// Builds the binary with the same cargo, so the test always runs the current code.
pub(in crate::sensors) fn vision_binary() -> PathBuf {
    let mut cargo = Process::new(env!("CARGO"));
    cargo.args(["build", "--quiet", "--bin", "intel-siam-vision", "--message-format=json"]);
    cargo.args(["--manifest-path", concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")]);
    if !cfg!(debug_assertions) {
        cargo.arg("--release");
    }
    // Cargo gives this test the package's own variables (`OUT_DIR`, `CARGO_PKG_*`, …). Build scripts
    // that watch them would see a change and rebuild half the tree, here and in the next `cargo test`.
    for (name, _) in std::env::vars_os() {
        let name = name.to_string_lossy();
        let given = ["OUT_DIR", "CARGO_PKG_", "CARGO_MANIFEST_", "CARGO_CRATE_", "CARGO_PRIMARY_", "CARGO_TARGET_TMPDIR"];
        if given.iter().any(|prefix| name.starts_with(prefix)) {
            cargo.env_remove(name.as_ref());
        }
    }
    let out = cargo.stderr(Stdio::inherit()).output().expect("run cargo");
    assert!(out.status.success(), "cargo build --bin intel-siam-vision failed");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|msg| msg["reason"] == "compiler-artifact" && msg["target"]["name"] == "intel-siam-vision")
        .and_then(|msg| msg["executable"].as_str().map(PathBuf::from))
        .expect("cargo reports the vision executable")
}

struct Vision {
    child: Child,
    stdin: Option<BufWriter<ChildStdin>>,
    reports: Receiver<Report>,
}

impl Vision {
    fn start(binary: &Path, dir: &Path) -> Self {
        let mut child = Process::new(binary)
            .arg("--data")
            .arg(dir.join("data"))
            .arg("--logs")
            .arg(dir.join("logs"))
            .args(["--ffmpeg", "/nonexistent/ffmpeg"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the vision binary");
        let mut stdout = BufReader::new(child.stdout.take().expect("piped"));
        let (tx, reports) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(Some((report, _))) = read_frame::<Report>(&mut stdout) {
                if tx.send(report).is_err() {
                    return;
                }
            }
        });
        let stdin = Some(BufWriter::new(child.stdin.take().expect("piped")));
        Self { child, stdin, reports }
    }

    fn send(&mut self, cmd: Command) {
        write_frame(self.stdin.as_mut().expect("open"), &cmd, &[]).expect("write a command");
    }

    fn next(&self, what: &str) -> Report {
        self.reports.recv_timeout(PATIENCE).unwrap_or_else(|_| panic!("no report while waiting for {what}"))
    }

    /// Every report up to and including the first that matches.
    fn until(&self, what: &str, done: impl Fn(&Report) -> bool) -> Vec<Report> {
        let mut seen = Vec::new();
        loop {
            let report = self.next(what);
            let stop = done(&report);
            seen.push(report);
            if stop {
                return seen;
            }
        }
    }

    /// Closing stdin is how the app stops it; the process exits cleanly.
    fn stop(mut self) {
        self.stdin = None;
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().expect("wait") {
                assert!(status.success(), "the vision process exits cleanly: {status}");
                return;
            }
            assert!(start.elapsed() < PATIENCE, "the vision process exits when stdin closes");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

fn camera() -> CameraId {
    CameraId { dvr: DvrSerial::new("ABC0000000001"), channel: ChannelIndex::new(2) }
}

fn settings() -> Settings {
    let def = |key: u8, name: &str| StateDef { key: key.try_into().unwrap(), name: name.to_owned().try_into().unwrap() };
    let states = vec![def(1, "Aberto"), def(2, "Fechado")].try_into().unwrap();
    Settings::new_states("Portão".to_owned().try_into().unwrap(), states)
}

fn entry(reports: &[Report], id: SensorId) -> Option<&SensorEntry> {
    reports.iter().rev().find_map(|r| match r {
        Report::Sensor(e) if e.id == id => Some(&**e),
        _ => None,
    })
}

#[test]
fn a_sensor_created_over_the_wire_survives_a_restart() {
    let binary = vision_binary();
    let dir = std::env::temp_dir().join(format!("intel-siam-vision-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let id = SensorId(0xC0FFEE);
    let stream = StreamKey { camera: camera(), kind: StreamKind::Sub };
    let create =
        UserCommand::Create { id, source: SourceSpec::Dvr(camera()), stream: StreamKind::Sub, settings: settings() };

    let mut vision = Vision::start(&binary, &dir);
    assert_eq!(vision.next("hello"), Report::Hello { protocol: PROTOCOL });
    let snapshot = vision.until("the first focus", |r| matches!(r, Report::Focus(_)));
    assert!(snapshot.iter().any(|r| matches!(r, Report::Wants(w) if w.is_empty())));
    assert!(snapshot.iter().any(|r| matches!(r, Report::System(s) if s.models[0].state == ModelState::NotNeeded)));
    assert!(entry(&snapshot, id).is_none());
    assert_eq!(snapshot.last(), Some(&Report::Focus(None)));

    vision.send(Command::User(create.clone()));
    vision.send(Command::User(create));
    let created = vision.until("its stream wanted", |r| matches!(r, Report::Wants(w) if w.contains(&stream)));
    let sensor = entry(&created, id).expect("the new sensor in the replica");
    assert_eq!(sensor.settings, settings());
    assert_eq!(sensor.status, SensorStatus::WaitingSource, "no URL for its DVR stream yet");
    assert_eq!(sensor.source, SourceView::Dvr(camera()));

    let interest = Interest { sensor: Some(id), tab: Tab::Samples, ..Interest::default() };
    vision.send(Command::Desired(Desired { feeds: Default::default(), interest }));
    let focused = vision.until("the focus on it", |r| matches!(r, Report::Focus(Some(_))));
    let Some(Report::Focus(Some(view))) = focused.last() else { unreachable!() };
    assert_eq!((view.sensor, &view.detail), (Some(id), &Detail::Samples { rows: Vec::new(), before: None, older: None }));
    vision.stop();

    let vision = Vision::start(&binary, &dir);
    assert_eq!(vision.next("hello after the restart"), Report::Hello { protocol: PROTOCOL });
    let snapshot = vision.until("the first focus after the restart", |r| matches!(r, Report::Focus(_)));
    let sensor = entry(&snapshot, id).expect("the sensor comes back from SQLite");
    assert_eq!(sensor.settings, settings());
    assert_eq!(snapshot.iter().filter(|r| matches!(r, Report::Sensor(_))).count(), 1, "created once");
    assert!(snapshot.iter().any(|r| matches!(r, Report::Wants(w) if w.contains(&stream))));
    vision.stop();

    let mut store = super::store::Store::open(&dir.join("data")).unwrap();
    let jpeg = super::pixels::encode_jpeg(&super::pixels::Rgb::from(&egui::ColorImage::filled([2, 2], egui::Color32::BLACK)), 90).unwrap();
    for index in 0u32..205 {
        let mut bytes = jpeg.clone();
        bytes.extend(index.to_le_bytes());
        store.add_sample(id, &bytes, 1.try_into().unwrap()).unwrap();
    }
    drop(store);
    rusqlite::Connection::open(dir.join("data/sensors.db")).unwrap().execute("UPDATE sample SET label = NULL", []).unwrap();
    let mut vision = Vision::start(&binary, &dir);
    vision.until("startup before pagination", |r| matches!(r, Report::Focus(_)));
    let mut before = None;
    let mut seen = std::collections::BTreeSet::new();
    let mut pages = Vec::new();
    loop {
        vision.send(Command::Desired(Desired { feeds: Default::default(), interest: Interest {
            sensor: Some(id), tab: Tab::Samples, samples_before: before, ..Interest::default()
        } }));
        let reports = vision.until("sample page", |r| matches!(r, Report::Focus(Some(v)) if matches!(v.detail, Detail::Samples { .. })));
        let Report::Focus(Some(view)) = reports.last().unwrap() else { unreachable!() };
        let Detail::Samples { rows, before: received, older } = &view.detail else { unreachable!() };
        assert_eq!(*received, before);
        assert!(view.images().iter().all(|id| reports.iter().any(|r| matches!(r, Report::Image { id: sent, .. } if sent == id))));
        for row in rows { assert!(seen.insert(row.id)); }
        pages.push(rows.len());
        before = *older;
        if before.is_none() { break; }
    }
    assert_eq!(pages, [100, 100, 5]);
    assert_eq!(seen.len(), 205);
    vision.send(Command::Desired(Desired { feeds: Default::default(), interest: Interest {
        sensor: Some(id), tab: Tab::Samples, ..Interest::default()
    } }));
    let returned = vision.until("newest page again", |r| matches!(r, Report::Focus(Some(v)) if matches!(v.detail, Detail::Samples { before: None, .. })));
    let Report::Focus(Some(view)) = returned.last().unwrap() else { unreachable!() };
    let Detail::Samples { rows, .. } = &view.detail else { unreachable!() };
    assert_eq!(rows.len(), 100);
    vision.stop();
    let _ = fs::remove_dir_all(&dir);
}
