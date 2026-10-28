use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufReader, BufWriter};
use std::sync::{Arc, mpsc};
use std::thread;

use egui::ColorImage;

use super::types::*;
use super::wire::{self, Command, Desired, Report, PROTOCOL};
use crate::cloud::Dvr;
use crate::model::DvrSerial;
use crate::plan::rtsp_url;
use crate::procs::{ChildKey, LocalPort, Pipes, Supervisor, Waker};
use crate::status::ExitReason;

pub struct Sensors {
    view: SensorsView,
    link: Option<Link>,
    outbox: Vec<UserCommand>,
    wake: Waker,
}

struct Link {
    inbound: mpsc::Receiver<Inbound>,
    commands: mpsc::Sender<Command>,
    sent: Option<Desired>,
    snapshot: Option<BTreeSet<SensorId>>,
}

enum Inbound {
    Report(Box<Report>),
    Hello(u32),
    Image(ImageId, Arc<ColorImage>),
    Closed,
}

impl Sensors {
    pub fn new(wake: Waker) -> Self {
        Self { view: SensorsView::default(), link: None, outbox: Vec::new(), wake }
    }

    pub fn view(&self) -> &SensorsView { &self.view }

    pub fn queue(&mut self, command: UserCommand) {
        if self.view.link == LinkState::Ready { self.outbox.push(command); }
    }

    pub fn poll(&mut self, procs: &mut Supervisor) {
        if let Some(pipes) = procs.take_pipes(&ChildKey::Vision) {
            self.adopt(pipes);
        }
        while let Some(input) = self.link.as_ref().and_then(|l| l.inbound.try_recv().ok()) {
            match input {
                Inbound::Hello(protocol) => {
                    if protocol != PROTOCOL {
                        self.view.link = LinkState::Incompatible { theirs: protocol };
                        procs.fail(&ChildKey::Vision, ExitReason::Incompatible);
                        self.link = None;
                        self.outbox.clear();
                        break;
                    }
                    self.view.link = LinkState::Ready;
                }
                Inbound::Closed => {
                    self.view.link = LinkState::Waiting;
                    self.link = None;
                    self.outbox.clear();
                    break;
                }
                _ if self.view.link != LinkState::Ready => {
                    procs.fail(&ChildKey::Vision, ExitReason::Other("protocolo de visão inválido".into()));
                    self.link = None;
                    break;
                }
                Inbound::Image(id, image) => { self.view.images.insert(id, image); }
                Inbound::Report(report) => match *report {
                    Report::Sensor(entry) => {
                        if let Some(snapshot) = self.link.as_mut().and_then(|l| l.snapshot.as_mut()) {
                            snapshot.insert(entry.id);
                        }
                        self.view.sensors.insert(entry.id, entry);
                    }
                    Report::Removed(id) => { self.view.sensors.remove(&id); }
                    Report::Wants(wants) => self.view.wanted_feeds = wants,
                    Report::System(system) => self.view.system = Some(system),
                    Report::Focus(focus) => {
                        if let Some(snapshot) = self.link.as_mut().and_then(|l| l.snapshot.take()) {
                            self.view.sensors.retain(|id, _| snapshot.contains(id));
                        }
                        let held = focus.as_ref().map(FocusView::images).unwrap_or_default();
                        self.view.images.retain(|id, _| held.contains(id));
                        self.view.focus = focus;
                    }
                    Report::Hello { .. } | Report::Image { .. } => unreachable!(),
                },
            }
        }
    }

    pub fn sync(&mut self, devices: Option<&BTreeMap<DvrSerial, Dvr>>, ports: &BTreeMap<DvrSerial, LocalPort>, interest: Interest) {
        if self.view.link != LinkState::Ready {
            self.outbox.clear();
            return;
        }
        let Some(link) = &mut self.link else { return };
        for command in self.outbox.drain(..) {
            let _ = link.commands.send(Command::User(command));
        }
        let feeds = self.view.wanted_feeds.iter().filter_map(|key| {
            let dvr = devices?.get(&key.camera.dvr)?;
            let port = ports.get(&key.camera.dvr)?;
            Some((key.clone(), rtsp_url(*port, &dvr.creds, &key.camera, key.kind)))
        }).collect();
        let desired = Desired { feeds, interest };
        if link.sent.as_ref() != Some(&desired) && link.commands.send(Command::Desired(desired.clone())).is_ok() {
            link.sent = Some(desired);
        }
    }

    fn adopt(&mut self, pipes: Pipes) {
        self.view.link = LinkState::Waiting;
        self.view.focus = None;
        self.view.images.clear();
        self.outbox.clear();
        let (tx, inbound) = mpsc::channel();
        let (commands, rx) = mpsc::channel();
        let wake = Arc::clone(&self.wake);
        let closed = tx.clone();
        let writer_wake = Arc::clone(&wake);
        thread::spawn(move || {
            let mut stdin = BufWriter::new(pipes.stdin);
            for cmd in rx {
                if wire::write_frame(&mut stdin, &cmd, &[]).is_err() { break; }
            }
            let _ = closed.send(Inbound::Closed);
            writer_wake();
        });
        thread::spawn(move || {
            let mut stdout = BufReader::new(pipes.stdout);
            while let Ok(Some((report, payload))) = wire::read_frame::<Report>(&mut stdout) {
                pipes.progress.bump();
                let input = match report {
                    Report::Image { id, size } => Inbound::Image(id, Arc::new(ColorImage::from_rgba_premultiplied(size.map(|n| n as usize), &payload))),
                    Report::Hello { protocol } => Inbound::Hello(protocol),
                    other => Inbound::Report(Box::new(other)),
                };
                if tx.send(input).is_err() { return; }
                wake();
            }
            let _ = tx.send(Inbound::Closed);
            wake();
        });
        self.link = Some(Link { inbound, commands, sent: None, snapshot: Some(BTreeSet::new()) });
    }
}
