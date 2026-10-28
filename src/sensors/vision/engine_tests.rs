use super::*;
use std::path::PathBuf;
use std::sync::mpsc;
use crate::frames::StreamKind;
use crate::model::{CameraId, ChannelIndex, DvrSerial};
use crate::sensors::types::{Name, StateDef};

struct Fixture { dir: PathBuf }
impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("sensor-regression-{}-{}", std::process::id(), getrandom::u64().unwrap()));
        Self { dir }
    }
    fn store(&self) -> Store { Store::open(&self.dir).unwrap() }
    fn engine(&self) -> (Engine, Receiver<Out>, Receiver<Job>, Receiver<()>) {
        let feeds = Feeds::new("/nonexistent/ffmpeg".into(), self.dir.join("logs"), Arc::new(|| {}));
        let (infer, jobs, fetch) = super::super::infer::test_channels();
        let (out, rx) = mpsc::channel();
        (Engine::new(self.store(), feeds, infer, out).unwrap(), rx, jobs, fetch)
    }
}
impl Drop for Fixture { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.dir); } }
fn record() -> SensorRecord {
    let states = [1u8, 2].map(|k| StateDef { key: k.try_into().unwrap(), name: Name::try_from(format!("State {k}")).unwrap() });
    SensorRecord {
        id: SensorId(1), source: SourceSpec::Dvr(CameraId { dvr: DvrSerial::new("CAM"), channel: ChannelIndex::new(0) }),
        stream: StreamKind::Sub, settings: crate::sensors::types::Settings::new_states("Sensor".to_owned().try_into().unwrap(), states.to_vec().try_into().unwrap()), geometry: None,
    }
}

#[test]
fn regression_restart_resumes_training_from_persisted_samples() {
    let f = Fixture::new();
    let mut store = f.store();
    store.create(&record()).unwrap();
    store.add_sample(SensorId(1), b"first", 1.try_into().unwrap()).unwrap();
    store.add_sample(SensorId(1), b"second", 2.try_into().unwrap()).unwrap();
    drop(store);
    let (mut engine, _, jobs, fetch) = f.engine();
    let now = Instant::now();
    engine.tick(now);
    assert!(fetch.try_recv().is_ok(), "restart must request the backbone without another teach");
    engine.handle(Input::Model(ModelEvent::Ready));
    engine.tick(now + RETRAIN_DELAY);
    let Job::Train { rows, .. } = jobs.try_recv().expect("pending training resumes") else { panic!("expected training") };
    assert_eq!(rows.len(), 2);
}

#[test]
fn regression_stalled_preview_never_returns_an_expired_deadline() {
    let f = Fixture::new();
    f.store().create(&record()).unwrap();
    let (mut engine, _, _, _) = f.engine();
    engine.interest.sensor = Some(SensorId(1));
    let now = Instant::now();
    let feed = engine.sensors[&SensorId(1)].feed();
    engine.feeds_dirty = false;
    engine.feeds.seed_frame(feed, super::super::feeds::Frame {
        image: Arc::new(ColorImage::filled([2, 2], egui::Color32::BLACK)), at: now, geometry: None, seq: 1,
    });
    engine.step_preview(now);
    let later = now + Duration::from_secs(3);
    let deadline = engine.tick(later);
    assert!(deadline > later, "stale preview must wait instead of spinning");
}

fn seed_many_samples(f: &Fixture, count: i64) {
    f.store().create(&record()).unwrap();
    let image = pixels::encode_jpeg(&ColorImage::filled([2, 2], egui::Color32::BLACK), 90).unwrap();
    let dir = f.dir.join("img/s/0000000000000001");
    fs::create_dir_all(&dir).unwrap();
    let mut conn = rusqlite::Connection::open(f.dir.join("sensors.db")).unwrap();
    let tx = conn.transaction().unwrap();
    for id in 1..=count {
        let sha = format!("{id:064x}");
        fs::write(dir.join(format!("{sha}.jpg")), &image).unwrap();
        tx.execute("INSERT INTO sample (id, sensor, sha256, label, bytes, created_at) VALUES (?, 1, ?, 1, ?, 0)",
            rusqlite::params![id, sha, image.len() as i64]).unwrap();
    }
    tx.commit().unwrap();
}

#[test]
fn regression_samples_fit_the_wire_even_with_a_large_library() {
    let f = Fixture::new();
    seed_many_samples(&f, 12_000);
    let (mut engine, _, _, _) = f.engine();
    engine.interest = Interest { sensor: Some(SensorId(1)), tab: Tab::Samples, ..Interest::default() };
    let mut seen = BTreeSet::new();
    loop {
        let mut images = Vec::new();
        let view = engine.focus_view(&BTreeSet::new(), &mut images).unwrap();
        assert_eq!(view.images(), images.iter().map(|(id, _)| *id).collect());
        let mut bytes = Vec::new();
        crate::sensors::wire::write_frame(&mut bytes, &Report::Focus(Some(view)), &[]).unwrap();
        let (Report::Focus(Some(view)), _) = crate::sensors::wire::read_frame::<Report>(&mut bytes.as_slice()).unwrap().unwrap() else { panic!("expected focus") };
        let Detail::Samples { rows, before, older } = view.detail else { panic!("expected samples") };
        assert_eq!(before, engine.interest.samples_before);
        assert!(rows.len() <= 100, "the page stays bounded regardless of library size");
        for row in rows { assert!(seen.insert(row.id), "no duplicate across pages"); }
        if older.is_none() { break; }
        engine.interest.samples_before = older;
    }
    assert_eq!(seen.len(), 12_000, "pagination must preserve access to the whole library");
}

#[test]
fn regression_restart_rebuilds_old_head_after_labels_region_or_deletion_change() {
    for change in ["labels", "region", "delete"] {
        let f = Fixture::new();
        let mut store = f.store();
        let mut rec = record();
        store.create(&rec).unwrap();
        let a = store.add_sample(rec.id, b"first", 1.try_into().unwrap()).unwrap();
        store.add_sample(rec.id, b"second", 2.try_into().unwrap()).unwrap();
        let head = super::super::infer::fit(&[vec![1.0, 0.0], vec![0.0, 1.0]], &[1.try_into().unwrap(), 2.try_into().unwrap()]).unwrap();
        store.put_training(rec.id, BACKBONE.id, rec.settings.region.hash(), Some(&head), &[]).unwrap();
        match change {
            "labels" => { store.relabel(rec.id, a, Some(2.try_into().unwrap())).unwrap(); }
            "region" => {
                rec.settings.region = crate::sensors::types::Region::rect(
                    crate::sensors::types::NormPoint::new(0.1, 0.1).unwrap(),
                    crate::sensors::types::NormPoint::new(0.9, 0.9).unwrap()).unwrap();
                store.update(&rec).unwrap();
            }
            "delete" => {
                for row in store.samples(rec.id, None, 100).unwrap().0 {
                    store.relabel(rec.id, row.id, None).unwrap();
                }
            }
            _ => unreachable!(),
        }
        drop(store);
        let (mut engine, _, jobs, fetch) = f.engine();
        let now = Instant::now();
        engine.tick(now);
        assert!(fetch.try_recv().is_ok());
        engine.handle(Input::Model(ModelEvent::Ready));
        engine.tick(now + RETRAIN_DELAY);
        let Job::Train { rows, region, .. } = jobs.try_recv().expect("rebuild the head") else { panic!("expected training") };
        assert_eq!(region, rec.settings.region);
        match change {
            "labels" => assert!(rows.iter().all(|r| r.label == 2.try_into().unwrap())),
            "delete" => assert!(rows.is_empty()),
            "region" => assert_eq!(rows.len(), 2),
            _ => unreachable!(),
        }
        let head = super::super::infer::fit(
            &rows.iter().map(|r| vec![r.sample.0 as f32, 1.0]).collect::<Vec<_>>(),
            &rows.iter().map(|r| r.label).collect::<Vec<_>>());
        engine.trained(rec.id, region.hash(), Ok(Trained { head, fresh: Vec::new() }), now);
        if change != "region" { assert!(engine.sensors[&rec.id].head().is_none(), "obsolete head is removed"); }
    }
}

#[test]
fn regression_unreadable_sample_page_keeps_a_cursor_to_older_samples() {
    let f = Fixture::new();
    seed_many_samples(&f, 205);
    for id in 106..=205 {
        fs::remove_file(f.dir.join(format!("img/s/0000000000000001/{id:064x}.jpg"))).unwrap();
    }
    let (mut engine, _, _, _) = f.engine();
    engine.interest = Interest { sensor: Some(SensorId(1)), tab: Tab::Samples, ..Interest::default() };
    let view = engine.focus_view(&BTreeSet::new(), &mut Vec::new()).unwrap();
    let Detail::Samples { rows, older, .. } = view.detail else { panic!("expected samples") };
    assert!(rows.is_empty());
    assert!(older.is_some(), "an unreadable page must not hide later pages");
    engine.interest.samples_before = older;
    let view = engine.focus_view(&BTreeSet::new(), &mut Vec::new()).unwrap();
    let Detail::Samples { rows, .. } = view.detail else { panic!("expected samples") };
    assert_eq!(rows.len(), 100);
}
