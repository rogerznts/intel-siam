use super::*;
use std::path::PathBuf;
use std::sync::mpsc;
use crate::frames::StreamKind;
use crate::model::{CameraId, ChannelIndex, DvrSerial};
use crate::sensors::types::{Name, NormPoint, OwnLabel, Region, StateDef};

struct Fixture { dir: PathBuf }
impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("sensor-regression-{}-{}", std::process::id(), getrandom::u64().unwrap()));
        Self { dir }
    }
    fn store(&self) -> Store { Store::open(&self.dir).unwrap() }
    fn engine(&self) -> (Engine, Receiver<Out>, Receiver<Job>, Receiver<Model>) {
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
    engine.handle(Input::Model(ModelEvent { model: Model::Backbone, state: ModelState::Ready }));
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
    let image = pixels::encode_jpeg(&Rgb::from(&ColorImage::filled([2, 2], egui::Color32::BLACK)), 90).unwrap();
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
            "labels" => { store.relabel(rec.id, a, Some(SampleLabel::State(2.try_into().unwrap()))).unwrap(); }
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
        engine.handle(Input::Model(ModelEvent { model: Model::Backbone, state: ModelState::Ready }));
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

const CARS: SensorId = SensorId(2);

fn car() -> ObjectClass {
    ObjectClass::by_key("car").unwrap()
}

fn objects_record() -> SensorRecord {
    SensorRecord {
        id: CARS, source: SourceSpec::Dvr(CameraId { dvr: DvrSerial::new("CAM"), channel: ChannelIndex::new(1) }),
        stream: StreamKind::Sub, settings: Settings::new_objects("Garagem".to_owned().try_into().unwrap(), vec![car()].try_into().unwrap()), geometry: None,
    }
}

/// A frame on the sensor's feed. A DVR feed without a URL is not reconciled into a decoder, so
/// the engine is told its feeds are current.
fn seed(engine: &mut Engine, now: Instant, seq: u64) {
    let feed = engine.sensors[&CARS].feed();
    let image = ColorImage::new([64, 48], (0..64 * 48).map(|i| egui::Color32::from_gray((i % 251) as u8)).collect());
    engine.feeds.seed_frame(feed, super::super::feeds::Frame { image: Arc::new(image), at: now, geometry: None, seq });
    engine.feeds_dirty = false;
}

fn box_at(area: [f32; 4], verdict: Verdict) -> Found {
    Found { class: car(), score: 0.9, area, verdict }
}

/// One check of the object sensor, forced: returns its request.
fn checked(engine: &mut Engine, jobs: &Receiver<Job>, now: Instant) -> DetectRequest {
    engine.sensors.get_mut(&CARS).unwrap().schedule.force = true;
    engine.tick(now);
    match jobs.try_recv().expect("a check of the object sensor") {
        Job::Detect { request, .. } => request,
        _ => panic!("expected a detection"),
    }
}

fn sent_value(engine: &Engine) -> Option<SensorValue> {
    engine.sensors[&CARS].sent.as_ref().and_then(|e| e.value.clone())
}

#[test]
fn an_object_sensor_needs_only_the_detector_and_counts_what_it_finds() {
    let f = Fixture::new();
    f.store().create(&objects_record()).unwrap();
    let (mut engine, _, jobs, fetch) = f.engine();
    let now = Instant::now();
    seed(&mut engine, now, 1);
    engine.tick(now);
    assert_eq!(fetch.try_iter().collect::<Vec<_>>(), [Model::Detector(Detector::DfineS)], "no backbone without taught boxes");
    assert!(jobs.try_recv().is_err(), "no check before the detector is ready");
    engine.handle(Input::Model(ModelEvent { model: Model::Detector(Detector::DfineS), state: ModelState::Ready }));
    let request = checked(&mut engine, &jobs, now);
    assert!(request.taught.is_none());
    let two = vec![box_at([0.1, 0.5, 0.3, 0.9], Verdict::Detector), box_at([0.5, 0.5, 0.7, 0.9], Verdict::Detector)];
    engine.handle(Input::Infer(Done::Detected { sensor: CARS, found: Ok(two) }));
    engine.tick(now);
    let cars = ObjectKey::Class(car());
    assert_eq!(sent_value(&engine), Some(SensorValue::Objects(vec![(cars, 2)])));
    let (rows, _) = engine.store.history(CARS, None, 10).unwrap();
    assert_eq!(rows.iter().map(|r| r.events.clone()).collect::<Vec<_>>(), [vec![ObjectEvent::On(cars)]]);
    checked(&mut engine, &jobs, now + Duration::from_secs(1));
    engine.handle(Input::Infer(Done::Detected { sensor: CARS, found: Ok(Vec::new()) }));
    assert!(engine.sensors[&CARS].schedule.recheck_at.is_some(), "missing once: looked for again soon");
    engine.tick(now + Duration::from_secs(1));
    assert_eq!(sent_value(&engine), Some(SensorValue::Objects(vec![(cars, 2)])), "one missed check keeps it");
}

#[test]
fn regression_a_failed_model_nobody_needs_stops_waking_the_engine() {
    let f = Fixture::new();
    f.store().create(&objects_record()).unwrap();
    let (mut engine, _, _, fetch) = f.engine();
    let now = Instant::now();
    seed(&mut engine, now, 1);
    engine.tick(now);
    assert_eq!(fetch.try_iter().collect::<Vec<_>>(), [Model::Detector(Detector::DfineS)]);
    engine.handle(Input::Model(ModelEvent { model: Model::Detector(Detector::DfineS), state: ModelState::Failed("offline".into()) }));
    engine.user(UserCommand::Delete(CARS), now);
    let later = now + MODEL_RETRY * 2;
    assert!(engine.tick(later) > later, "no deadline left in the past");
    assert_eq!(engine.model(Model::Detector(Detector::DfineS)), ModelState::NotNeeded);
    let rec = objects_record();
    engine.user(UserCommand::Create { id: rec.id, source: rec.source, stream: rec.stream, settings: rec.settings }, later);
    engine.tick(later);
    assert_eq!(fetch.try_iter().count(), 1, "needed again, fetched again");
}

#[test]
fn a_taught_own_label_is_indexed_compared_and_asked_about() {
    let f = Fixture::new();
    f.store().create(&objects_record()).unwrap();
    let (mut engine, _, jobs, fetch) = f.engine();
    let now = Instant::now();
    seed(&mut engine, now, 1);
    engine.handle(Input::Model(ModelEvent { model: Model::Detector(Detector::DfineS), state: ModelState::Ready }));
    engine.interest = Interest { sensor: Some(CARS), ..Interest::default() };
    checked(&mut engine, &jobs, now);
    engine.handle(Input::Infer(Done::Detected { sensor: CARS, found: Ok(vec![box_at([0.1, 0.5, 0.3, 0.9], Verdict::Detector)]) }));
    engine.tick(now);
    let frame = engine.preview.as_ref().map(|p| p.preview.frame).expect("the checked frame is the preview");
    let ours = LabelId(77);
    let mut settings = objects_record().settings;
    let KindSettings::Objects(objects) = &mut settings.kind else { unreachable!() };
    objects.labels = vec![OwnLabel { id: ours, name: "Nosso carro".to_owned().try_into().unwrap(), parent: car() }].try_into().unwrap();
    let area: NormBox = [0.1, 0.5, 0.3, 0.9].try_into().unwrap();
    let higher: NormBox = [0.1, 0.1, 0.3, 0.3].try_into().unwrap();
    let drawn = UserCommand::TeachBox { id: CARS, frame, area, detected: None, label: BoxLabel::NotThis };
    engine.user(drawn, now);
    assert_eq!(engine.sensors[&CARS].notice.as_ref().map(|n| n.kind.clone()), Some(NoticeKind::UnknownItem), "a drawn box is something");
    engine.user(UserCommand::TeachBox { id: CARS, frame, area, detected: Some(car()), label: BoxLabel::Own(ours) }, now);
    assert_eq!(engine.store.counts(CARS).unwrap().taught, 0, "the label does not exist yet");
    engine.user(UserCommand::Configure { id: CARS, settings: settings.clone() }, now);
    let region = Region::rect(NormPoint::new(0.0, 0.0).unwrap(), NormPoint::new(1.0, 0.5).unwrap()).unwrap();
    engine.user(UserCommand::TeachBox { id: CARS, frame, area: higher, detected: Some(car()), label: BoxLabel::Own(ours) }, now);
    engine.user(UserCommand::TeachBox { id: CARS, frame, area, detected: Some(car()), label: BoxLabel::Own(ours) }, now);
    assert_eq!(engine.store.counts(CARS).unwrap().taught, 2);
    engine.user(UserCommand::Configure { id: CARS, settings: Settings { region, ..settings.clone() } }, now);
    engine.user(UserCommand::TeachBox { id: CARS, frame, area, detected: Some(car()), label: BoxLabel::Own(ours) }, now);
    assert_eq!(engine.sensors[&CARS].notice.as_ref().map(|n| n.kind.clone()), Some(NoticeKind::OutsideRegion), "its bottom is outside");
    engine.tick(now + RETRAIN_DELAY);
    assert!(fetch.try_iter().any(|m| m == Model::Backbone), "taught boxes need the backbone");
    engine.handle(Input::Model(ModelEvent { model: Model::Backbone, state: ModelState::Ready }));
    engine.tick(now + RETRAIN_DELAY);
    let Some(Job::Index { rows, parents, .. }) = jobs.try_iter().find(|j| matches!(j, Job::Index { .. })) else { panic!("expected an index") };
    assert_eq!((rows.len(), parents), (2, vec![(ours, car())]));
    let vector = vec![1.0, 0.0];
    let index = teach::build(rows.into_iter().map(|r| teach::TaughtRow { sample: r.sample, label: r.label, detected: r.detected, vector: vector.clone() }).collect(), [(ours, car())].into());
    engine.handle(Input::Infer(Done::Indexed { sensor: CARS, outcome: Ok(Indexed { index, fresh: Vec::new() }) }));
    seed(&mut engine, now + RETRAIN_DELAY, 2);
    engine.tick(now + RETRAIN_DELAY);
    let Some(Job::Detect { request, .. }) = jobs.try_iter().find(|j| matches!(j, Job::Detect { .. })) else { panic!("the index forces a check") };
    assert!(request.taught.is_some(), "the check compares with the taught boxes");
    let unsure = box_at([0.1, 0.1, 0.3, 0.45], Verdict::Unsure { label: ours, similarity: 0.8 });
    engine.handle(Input::Infer(Done::Detected { sensor: CARS, found: Ok(vec![unsure]) }));
    let review = engine.store.reviews(CARS, 10).unwrap();
    assert!(matches!(review.as_slice(), [row] if matches!(row.review, Some(ReviewReason::Ask(q)) if q.label == ours)), "is this our car?");
    let item = review[0].id;
    engine.user(UserCommand::Resolve { id: CARS, item, label: Some(SampleLabel::Box(BoxLabel::Own(ours))) }, now);
    let counts = engine.store.counts(CARS).unwrap();
    assert_eq!((counts.taught, counts.pending), (3, 0), "the answer is taught");
    engine.user(UserCommand::Configure { id: CARS, settings: Settings { kind: objects_record().settings.kind, ..settings } }, now);
    assert_eq!(engine.store.counts(CARS).unwrap().taught, 0, "removing the label forgets its boxes");
}
