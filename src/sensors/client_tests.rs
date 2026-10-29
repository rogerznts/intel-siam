use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::*;
use crate::frames::{StreamKey, StreamKind};
use crate::cloud::{Dvr, DvrCreds, Secret};
use crate::procs::LocalPort;
use crate::model::{CameraId, ChannelIndex, DvrSerial};
use crate::procs::{ChildIo, ChildKey, ChildPhase, ChildSpec, Supervisor};
use crate::status::ExitReason;

fn drive(client: &mut Sensors, supervisor: &mut Supervisor, mut done: impl FnMut(&Sensors) -> bool) {
    let start = Instant::now();
    loop {
        supervisor.poll(Instant::now());
        client.poll(supervisor);
        if done(client) { return; }
        assert!(start.elapsed() < Duration::from_secs(15), "client state did not converge");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn restart_keeps_wants_and_resends_interest_to_the_real_process() {
    let binary = super::vision::process_tests::vision_binary();
    let dir = std::env::temp_dir().join(format!("intel-siam-client-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut supervisor = Supervisor::new(dir.join("logs"), Arc::new(|| {}));
    let mut client = Sensors::new(Arc::new(|| {}));
    let desired = BTreeMap::from([(ChildKey::Vision, vision_child(&binary, &dir.join("data"), std::path::Path::new("/nonexistent/ffmpeg"), &dir.join("logs")))]);
    supervisor.reconcile(desired.clone(), Instant::now());
    drive(&mut client, &mut supervisor, |s| s.view().link == LinkState::Ready);
    let id = SensorId::random();
    let camera = CameraId { dvr: DvrSerial::new("CLIENTTEST"), channel: ChannelIndex::new(0) };
    let key = StreamKey { camera: camera.clone(), kind: StreamKind::Sub };
    let devices = BTreeMap::from([(camera.dvr.clone(), Dvr {
        serial: camera.dvr.clone(), name: "Teste".into(),
        creds: DvrCreds { user: "admin".into(), pass: Secret::new("test-password") },
        channels: vec![(camera.channel, "Portão".into())],
    })]);
    let ports = BTreeMap::from([(camera.dvr.clone(), LocalPort::new(12345))]);
    let states = ["Aberto", "Fechado"].into_iter().enumerate().map(|(i, name)| StateDef { key: ((i + 1) as u8).try_into().unwrap(), name: name.to_owned().try_into().unwrap() }).collect::<Vec<_>>().try_into().unwrap();
    client.queue(UserCommand::Create { id, source: SourceSpec::Dvr(camera), stream: StreamKind::Sub, settings: Settings::new_states("Portão".to_owned().try_into().unwrap(), states) });
    let interest = Interest { sensor: Some(id), tab: Tab::Samples, ..Interest::default() };
    client.sync(None, &BTreeMap::new(), interest.clone());
    drive(&mut client, &mut supervisor, |s| s.view().wanted_feeds.contains(&key) && s.view().focus.as_ref().is_some_and(|f| f.sensor == Some(id)));
    client.sync(Some(&devices), &ports, interest.clone());
    drive(&mut client, &mut supervisor, |s| matches!(s.view().sensors[&id].status, SensorStatus::SourceDown { .. }));
    supervisor.reconcile(BTreeMap::new(), Instant::now());
    drive(&mut client, &mut supervisor, |s| s.view().link == LinkState::Waiting);
    assert_eq!(client.view().wanted_feeds, BTreeSet::from([key.clone()]));
    assert!(client.view().sensors.contains_key(&id), "last replica stays visible while stale");
    supervisor.reconcile(desired, Instant::now());
    drive(&mut client, &mut supervisor, |s| s.view().link == LinkState::Ready);
    client.sync(Some(&devices), &ports, interest);
    drive(&mut client, &mut supervisor, |s| s.view().focus.as_ref().is_some_and(|f| f.sensor == Some(id) && matches!(f.detail, Detail::Samples { .. })));
    assert!(client.view().wanted_feeds.contains(&key));
    drive(&mut client, &mut supervisor, |s| matches!(s.view().sensors[&id].status, SensorStatus::SourceDown { .. }));
    supervisor.shutdown(Duration::ZERO);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_divergent_hello_is_terminal_and_never_receives_commands() {
    let dir = std::env::temp_dir().join(format!("intel-siam-incompatible-{}", std::process::id()));
    let mut supervisor = Supervisor::new(dir, Arc::new(|| {}));
    let mut client = Sensors::new(Arc::new(|| {}));
    let hello = serde_json::to_vec(&super::wire::Report::Hello { protocol: 999 }).unwrap();
    let mut frame = (hello.len() as u32).to_le_bytes().to_vec();
    frame.extend(hello);
    frame.extend(0u32.to_le_bytes());
    let bytes = frame.iter().map(|byte| format!("\\{:03o}", byte)).collect::<String>();
    let spec = ChildSpec { program: "/bin/sh".into(), args: vec!["-c".into(), format!("printf '{bytes}'; exec cat >/dev/null").into()], io: ChildIo::Duplex, stall_after: Duration::from_secs(30) };
    let desired = BTreeMap::from([(ChildKey::Vision, spec)]);
    supervisor.reconcile(desired.clone(), Instant::now());
    drive(&mut client, &mut supervisor, |s| matches!(s.view().link, LinkState::Incompatible { theirs: 999 }));
    let start = Instant::now();
    while !matches!(supervisor.phase(&ChildKey::Vision), Some(ChildPhase::Down { .. })) {
        supervisor.poll(Instant::now());
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(10));
    }
    supervisor.reconcile(desired, Instant::now() + Duration::from_secs(3600));
    assert!(matches!(supervisor.phase(&ChildKey::Vision), Some(ChildPhase::Down { reason: ExitReason::Incompatible, retry_at: None, .. })));
}
