use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, TextureHandle, TextureOptions, pos2, vec2};

use crate::frames::StreamKind;
use crate::model::CameraId;
use crate::procs::{ChildKey, ChildPhase};
use crate::sensors::*;
use crate::ui::{Actions, Scene, toggle};

/// Origin `OBJECT_BOX_COLOR`.
const BOX_COLOR: Color32 = Color32::from_rgb(0x7e, 0xe2, 0xb8);
const OWN_LABEL_COLOR: Color32 = Color32::from_rgb(0x6c, 0xc4, 0xff);
const PERSON_COLOR: Color32 = Color32::from_rgb(0xff, 0xb3, 0x6c);
const UNSURE_COLOR: Color32 = Color32::YELLOW;
const FILTERED_COLOR: Color32 = Color32::from_gray(140);

/// One of a row of framed choices; `true` when it was picked now.
fn choice<T: PartialEq>(ui: &mut egui::Ui, current: &mut T, value: T, text: &str) -> bool {
    let on = *current == value;
    let picked = toggle(ui, on, text).clicked() && !on;
    if picked { *current = value; }
    picked
}

#[derive(Default)]
pub struct SensorsPanel {
    pub open: bool,
    selected: Option<SensorId>,
    tab: Tab,
    before: Option<HistoryId>,
    samples_before: Option<SampleId>,
    sample_pages: Vec<Option<SampleId>>,
    opened: Option<HistoryId>,
    create: Option<NewSensor>,
    region: Option<Region>,
    polygon: Vec<NormPoint>,
    polygon_mode: bool,
    /// Object sensors: dragging on the preview draws a box to teach instead of the region.
    teaching_boxes: bool,
    drag_start: Option<NormPoint>,
    picked: Option<Picked>,
    new_label: String,
    delete_confirm: bool,
    images: HashMap<ImageId, TextureHandle>,
    error: Option<String>,
    /// The people, shared by every sensor, shown instead of a sensor.
    people_open: bool,
    /// A name being typed for a new person.
    new_person: String,
    /// Names being edited, by person.
    renames: HashMap<PersonId, String>,
    forget_confirm: Option<PersonId>,
    /// A reading sensor's settings being edited, saved with one `Configure`.
    reading_draft: Option<(SensorId, ReadingSettings)>,
    /// The webhook and the image copy being edited, saved with one `Configure`.
    notify_draft: Option<NotifyDraft>,
    /// What the display really showed, typed per rejected reading.
    corrections: HashMap<HistoryId, String>,
}

/// The notify settings being edited. The URL and the folder are typed as text and only enter
/// `notify` once they parse, so a half-typed path never overwrites a working one.
struct NotifyDraft {
    id: SensorId,
    notify: Notify,
    webhook_on: bool,
    export_on: bool,
    url: String,
    folder: String,
}

/// A box chosen on the preview, to teach what it is.
#[derive(Clone, Copy)]
struct Picked {
    frame: FrameId,
    area: NormBox,
    /// The detector's class for a tapped box; `None` for a drawn one.
    detected: Option<ObjectClass>,
    /// What its face said, for a person box of a sensor that recognises faces.
    face: Option<FaceLook>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NewKind {
    States,
    Objects,
    Reading,
}

struct NewSensor {
    name: String,
    url: String,
    camera: Option<CameraId>,
    use_url: bool,
    stream: StreamKind,
    kind: NewKind,
    states: Vec<String>,
    classes: Vec<ObjectClass>,
    reading: ReadingSettings,
}

impl Default for NewSensor {
    fn default() -> Self {
        Self { name: String::new(), url: String::new(), camera: None, use_url: true, stream: StreamKind::Sub, kind: NewKind::States,
            states: vec!["Aberto".into(), "Fechado".into()], classes: ObjectClass::by_key("person").into_iter().collect(),
            reading: ReadingSettings::default() }
    }
}

impl SensorsPanel {
    pub fn interest(&self) -> Interest {
        if !self.open { Interest::default() }
        else if self.people_open { Interest { tab: Tab::People, ..Interest::default() } }
        else { Interest { sensor: self.selected, tab: self.tab, history_before: self.before, samples_before: self.samples_before, open: self.opened } }
    }

    fn select(&mut self, id: SensorId) {
        self.selected = Some(id);
        self.people_open = false;
        self.tab = Tab::Live;
        self.before = None;
        self.samples_before = None;
        self.sample_pages.clear();
        self.opened = None;
        self.region = None;
        self.polygon.clear();
        self.picked = None;
        self.teaching_boxes = false;
        self.delete_confirm = false;
        self.error = None;
        self.reading_draft = None;
        self.notify_draft = None;
        self.corrections.clear();
    }
}

pub fn value_label(entry: &SensorEntry, people: &[PersonView]) -> String {
    match &entry.status {
        SensorStatus::Live => match &entry.settings.kind {
            KindSettings::States(_) => state_label(entry, entry.value.as_ref()),
            KindSettings::Objects(objects) => presence_label(objects, people, entry.value.as_ref()),
            KindSettings::Reading(reading) => number_label(reading, entry.value.as_ref()),
        },
        SensorStatus::Paused => "pausado".into(),
        SensorStatus::GeometryChanged { .. } => "mudou a resolução".into(),
        SensorStatus::WaitingSource => "aguardando fonte".into(),
        SensorStatus::SourceDown { reason, .. } => format!("sem imagem ({reason})"),
        SensorStatus::WaitingModel { failed: Some(error), .. } => format!("falha no modelo ({error})"),
        SensorStatus::WaitingModel { progress, .. } => progress.map_or_else(|| "baixando modelo…".into(), |p| format!("baixando modelo {:.0}%", p * 100.0)),
        SensorStatus::Failed(error) => format!("falha ({error})"),
        SensorStatus::Untrained => "sem amostras de dois estados".into(),
    }
}

fn state_label(entry: &SensorEntry, value: Option<&SensorValue>) -> String {
    match (&entry.settings.kind, value) {
        (KindSettings::States(states), Some(SensorValue::State(key))) => states.states.iter().find(|s| s.key == *key).map(|s| s.name.as_str()).unwrap_or("?").into(),
        _ => "?".into(),
    }
}

/// "Carro 2 · Pessoa 1 · Tom 1": each object, own label and person present with its count.
fn presence_label(objects: &ObjectsSettings, people: &[PersonView], value: Option<&SensorValue>) -> String {
    match value {
        Some(SensorValue::Objects(present)) if present.is_empty() => "nenhum objeto".into(),
        Some(SensorValue::Objects(present)) => present.iter().map(|(key, count)| format!("{} {count}", objects.key_name(*key, people))).collect::<Vec<_>>().join(" · "),
        _ => "?".into(),
    }
}

/// "629.558 m³": the value with its unit.
fn number_label(reading: &ReadingSettings, value: Option<&SensorValue>) -> String {
    match value {
        Some(SensorValue::Number(number)) => with_unit(number, reading.unit.as_str()),
        _ => "?".into(),
    }
}

fn with_unit(value: impl std::fmt::Display, unit: &str) -> String {
    if unit.is_empty() { value.to_string() } else { format!("{value} {unit}") }
}

fn box_label_name(objects: &ObjectsSettings, label: BoxLabel) -> String {
    match label {
        BoxLabel::NotThis => "Não é isso".into(),
        BoxLabel::Class(class) => class.name().into(),
        BoxLabel::Own(id) => objects.key_name(ObjectKey::Own(id), &[]).into(),
    }
}

pub fn draw(ui: &mut egui::Ui, scene: &Scene, panel: &mut SensorsPanel, actions: &mut Actions) {
    let held = scene.sensors.focus.as_ref().map(FocusView::images).unwrap_or_default();
    panel.images.retain(|id, _| held.contains(id));
    for id in held {
        if let std::collections::hash_map::Entry::Vacant(slot) = panel.images.entry(id)
            && let Some(image) = scene.sensors.image(id)
        {
            slot.insert(ui.ctx().load_texture(format!("sensor-{id:?}"), image.clone(), TextureOptions::LINEAR));
        }
    }
    ui.heading("Sensores");
    let ready = scene.sensors.link == LinkState::Ready;
    if !ready {
        let message = if scene.tools.vision.is_none() { "intel-siam-vision não encontrado ao lado do app. Execute cargo build --release.".into() }
        else if let LinkState::Incompatible { theirs } = scene.sensors.link { format!("Versão de visão incompatível ({theirs}). Reinstale os dois binários e reabra o app.") }
        else if let Some(ChildPhase::Down { reason, .. }) = scene.procs.phase(&ChildKey::Vision) { format!("Visão indisponível: {}. Dados desatualizados.", reason.label()) }
        else { "Conectando ao processo de visão… Dados desatualizados.".into() };
        ui.colored_label(Color32::YELLOW, message);
    }
    egui::ScrollArea::vertical().show(ui, |ui| {
        for entry in scene.sensors.sensors.values() {
            let text = format!("{}: {}", entry.settings.name.as_str(), value_label(entry, scene.sensors.people()));
            if toggle(ui, panel.selected == Some(entry.id) && !panel.people_open, text).clicked() { panel.select(entry.id); }
        }
        if toggle(ui, panel.people_open, format!("Pessoas ({})", scene.sensors.people().len())).clicked() {
            panel.people_open = !panel.people_open;
            panel.forget_confirm = None;
        }
        ui.add_enabled_ui(ready, |ui| {
            if ui.button("Novo sensor").clicked() { panel.create = Some(NewSensor::default()); }
            if let Some(mut form) = panel.create.take() {
                let close = new_sensor(ui, scene, panel, &mut form, actions);
                if !close { panel.create = Some(form); }
            }
            if panel.people_open {
                people(ui, scene, panel, actions);
            } else if let Some(entry) = panel.selected.and_then(|id| scene.sensors.sensors.get(&id)) {
                selected(ui, scene, panel, entry, actions);
            }
            ui.separator();
            settings(ui, scene.sensors, actions);
        });
        if let Some(error) = &panel.error { ui.colored_label(Color32::LIGHT_RED, error); }
    });
}

/// The "novo sensor" form; `true` when it closes.
fn new_sensor(ui: &mut egui::Ui, scene: &Scene, panel: &mut SensorsPanel, form: &mut NewSensor, actions: &mut Actions) -> bool {
    let mut close = false;
    ui.group(|ui| {
        ui.horizontal(|ui| {
            ui.label("Tipo");
            ui.radio_value(&mut form.kind, NewKind::States, "Estado");
            ui.radio_value(&mut form.kind, NewKind::Objects, "Objetos");
            if ui.radio_value(&mut form.kind, NewKind::Reading, "Leitura").changed() {
                form.stream = StreamKind::Main;
            }
        });
        ui.label("Nome");
        ui.text_edit_singleline(&mut form.name);
        ui.horizontal(|ui| {
            ui.radio_value(&mut form.use_url, true, "URL avulsa");
            ui.radio_value(&mut form.use_url, false, "Câmera");
        });
        if form.use_url {
            ui.add(egui::TextEdit::singleline(&mut form.url).hint_text("http://… ou rtsp://…").password(true));
        } else {
            egui::ComboBox::from_id_salt("sensor-camera").selected_text(form.camera.as_ref().map(|c| camera_label(scene, c)).unwrap_or_else(|| "Escolha a câmera".into())).show_ui(ui, |ui| {
                if let Some(devices) = scene.account.devices() {
                    for dvr in devices.values() {
                        for (channel, name) in &dvr.channels {
                            let camera = CameraId { dvr: dvr.serial.clone(), channel: *channel };
                            ui.selectable_value(&mut form.camera, Some(camera), format!("{} / {name}", dvr.name));
                        }
                    }
                }
            });
        }
        ui.horizontal(|ui| {
            ui.label("Stream");
            ui.radio_value(&mut form.stream, StreamKind::Sub, "Sub");
            ui.radio_value(&mut form.stream, StreamKind::Main, "Main");
        });
        match form.kind {
            NewKind::Objects => {
                ui.label(format!("Objetos ({} de {OBJECT_MAX_CLASSES})", form.classes.len()));
                class_picker(ui, "new-sensor-classes", &mut form.classes);
            }
            NewKind::States => {
                for (index, state) in form.states.iter_mut().enumerate() {
                    ui.horizontal(|ui| { ui.label(format!("Estado {}", index + 1)); ui.text_edit_singleline(state); });
                }
                ui.horizontal(|ui| {
                    if ui.add_enabled(form.states.len() < MAX_STATES, egui::Button::new("+ Estado")).clicked() { form.states.push(String::new()); }
                    if ui.add_enabled(form.states.len() > 1, egui::Button::new("− Estado")).clicked() { form.states.pop(); }
                });
            }
            NewKind::Reading => {
                ui.weak("Depois de criar, desenhe na aba Ao vivo uma caixa justa em volta do número.");
                ui.push_id("new-sensor-reading", |ui| reading_editor(ui, &mut form.reading));
            }
        }
        ui.horizontal(|ui| {
            if ui.button("Criar").clicked() {
                let result = (|| -> Result<_, InvalidInput> {
                    let source = if form.use_url { SourceSpec::Url(form.url.clone().try_into()?) }
                        else { SourceSpec::Dvr(form.camera.clone().ok_or(InvalidInput("escolha uma câmera"))?) };
                    let name: Name = form.name.clone().try_into()?;
                    let settings = match form.kind {
                        NewKind::Objects => Settings::new_objects(name, form.classes.clone().try_into()?),
                        NewKind::Reading => Settings::new_reading(name, form.reading.clone()),
                        NewKind::States => {
                            let states = form.states.iter().enumerate().map(|(i, name)| Ok(StateDef { key: ((i + 1) as u8).try_into()?, name: name.clone().try_into()? })).collect::<Result<Vec<_>, InvalidInput>>()?.try_into()?;
                            Settings::new_states(name, states)
                        }
                    };
                    Ok((source, settings))
                })();
                match result {
                    Ok((source, settings)) => {
                        let id = SensorId::random();
                        actions.sensors.push(UserCommand::Create { id, source, stream: form.stream, settings });
                        panel.select(id);
                        close = true;
                    }
                    Err(error) => panel.error = Some(error.to_string()),
                }
            }
            if ui.button("Cancelar").clicked() { close = true; }
        });
    });
    close
}

/// The popular classes first, then every class by group; at most `OBJECT_MAX_CLASSES`.
fn class_picker(ui: &mut egui::Ui, id: &str, classes: &mut Vec<ObjectClass>) {
    let mut toggle = |ui: &mut egui::Ui, class: ObjectClass| {
        let mut on = classes.contains(&class);
        let allowed = if on { classes.len() > 1 } else { classes.len() < OBJECT_MAX_CLASSES };
        if ui.add_enabled(allowed, egui::Checkbox::new(&mut on, class.name())).changed() {
            if on { classes.push(class); } else { classes.retain(|c| *c != class); }
        }
    };
    ui.horizontal_wrapped(|ui| {
        for class in POPULAR.iter().filter_map(|key| ObjectClass::by_key(key)) { toggle(ui, class); }
    });
    egui::CollapsingHeader::new("Todos os objetos").id_salt(id).show(ui, |ui| {
        let mut groups: Vec<&str> = ObjectClass::all().map(ObjectClass::group).collect();
        groups.dedup();
        for group in groups {
            ui.weak(group);
            ui.horizontal_wrapped(|ui| {
                for class in ObjectClass::all().filter(|c| c.group() == group) { toggle(ui, class); }
            });
        }
    });
}

fn selected(ui: &mut egui::Ui, scene: &Scene, panel: &mut SensorsPanel, entry: &SensorEntry, actions: &mut Actions) {
    ui.separator();
    let people = scene.sensors.people();
    ui.strong(format!("{}: {}", entry.settings.name.as_str(), value_label(entry, people)));
    ui.weak(match &entry.source { SourceView::Dvr(c) => camera_label(scene, c), SourceView::Url { display, .. } => display.clone() });
    if let SensorStatus::GeometryChanged { expected, seen } = entry.status {
        ui.colored_label(Color32::YELLOW, format!("{}×{} → {}×{}. Salve a região para aceitar.", expected.width, expected.height, seen.width, seen.height));
    }
    if let Some(notice) = &entry.notice { ui.colored_label(Color32::YELLOW, notice_label(&notice.kind)); }
    ui.horizontal(|ui| {
        if ui.button("Checar agora").clicked() { actions.sensors.push(UserCommand::CheckNow(entry.id)); }
        if ui.button(if entry.settings.paused { "Retomar" } else { "Pausar" }).clicked() {
            let mut settings = entry.settings.clone(); settings.paused = !settings.paused;
            actions.sensors.push(UserCommand::Configure { id: entry.id, settings });
        }
        if ui.button("Apagar").clicked() { panel.delete_confirm = true; }
    });
    if panel.delete_confirm {
        ui.label("Apagar o sensor, as amostras e o histórico?");
        ui.horizontal(|ui| {
            if ui.button("Confirmar exclusão").clicked() { actions.sensors.push(UserCommand::Delete(entry.id)); panel.selected = None; }
            if ui.button("Cancelar exclusão").clicked() { panel.delete_confirm = false; }
        });
    }
    match &entry.settings.kind {
        KindSettings::Objects(objects) => objects_settings(ui, entry, objects, scene.sensors.system.as_ref(), actions),
        KindSettings::Reading(reading) => reading_settings(ui, panel, entry, reading, actions),
        KindSettings::States(_) => {}
    }
    notify_settings(ui, panel, entry, actions);
    let tabs: &[(Tab, &str)] = match entry.settings.kind {
        KindSettings::States(_) => &[(Tab::Live, "Ao vivo"), (Tab::Samples, "Amostras"), (Tab::Review, "Revisão"), (Tab::History, "Histórico")],
        KindSettings::Objects(_) => &[(Tab::Live, "Ao vivo"), (Tab::Samples, "Ensinados"), (Tab::Review, "Revisão"), (Tab::History, "Histórico")],
        KindSettings::Reading(_) => &[(Tab::Live, "Ao vivo"), (Tab::Review, "Revisão"), (Tab::History, "Histórico")],
    };
    ui.horizontal(|ui| {
        for (tab, label) in tabs {
            if choice(ui, &mut panel.tab, *tab, label) { panel.opened = None; panel.before = None; panel.samples_before = None; panel.sample_pages.clear(); }
        }
    });
    let Some(focus) = scene.sensors.focus.as_ref().filter(|f| f.sensor == Some(entry.id)) else { ui.spinner(); return };
    match (&focus.detail, panel.tab) {
        (Detail::Live, Tab::Live) => live(ui, panel, entry, people, focus, actions),
        (Detail::Samples { rows: samples, before, older }, Tab::Samples) if *before == panel.samples_before => {
            if samples.is_empty() { ui.label("Nenhuma amostra. Ensine na aba Ao vivo."); }
            for sample in samples {
                ui.push_id(sample.id, |ui| {
                    ui.horizontal(|ui| {
                        image(ui, &panel.images, sample.thumb, 100.0);
                        ui.vertical(|ui| {
                            ui.weak(timestamp(sample.at));
                            let mut label = sample.label;
                            let shown = match (&entry.settings.kind, label) {
                                (KindSettings::Objects(objects), Some(SampleLabel::Box(label))) => box_label_name(objects, label),
                                (_, Some(SampleLabel::State(state))) => state_label(entry, Some(&SensorValue::State(state))),
                                _ => "sem rótulo".into(),
                            };
                            egui::ComboBox::from_id_salt("label").selected_text(shown).show_ui(ui, |ui| {
                                for (choice, name) in label_choices(&entry.settings.kind) { ui.selectable_value(&mut label, Some(choice), name); }
                            });
                            if label != sample.label { actions.sensors.push(UserCommand::Relabel { id: entry.id, sample: sample.id, label }); }
                            if ui.button("Excluir amostra").clicked() { actions.sensors.push(UserCommand::Relabel { id: entry.id, sample: sample.id, label: None }); }
                        });
                    });
                });
            }
            ui.horizontal(|ui| {
                if ui.add_enabled(!panel.sample_pages.is_empty(), egui::Button::new("Amostras mais recentes")).clicked() {
                    panel.samples_before = panel.sample_pages.pop().flatten();
                }
                if ui.add_enabled(older.is_some(), egui::Button::new("Amostras mais antigas")).clicked() {
                    panel.sample_pages.push(panel.samples_before);
                    panel.samples_before = *older;
                }
            });
        }
        (Detail::Review(rows), Tab::Review) | (Detail::History { rows, .. }, Tab::History) => {
            if rows.is_empty() { ui.label("Nenhum registro."); }
            for row in rows {
                ui.push_id(row.id, |ui| {
                    ui.separator();
                    ui.horizontal(|ui| {
                        if let Some(id) = row.thumb
                            && let Some(texture) = panel.images.get(&id)
                            && ui.add(egui::Image::new(texture).fit_to_exact_size(vec2(100.0, 56.25)).sense(Sense::click())).clicked()
                        { panel.opened = Some(row.id); }
                        ui.vertical(|ui| {
                            ui.label(history_label(entry, people, row));
                            ui.weak(timestamp(row.at));
                            if let Some(reason) = row.review { ui.label(review_label(entry, people, reason)); }
                        });
                    });
                    if panel.tab == Tab::Review {
                        match &entry.settings.kind {
                            KindSettings::Reading(reading) => reading_answers(ui, entry.id, reading, row, &mut panel.corrections, &mut panel.error, actions),
                            _ => review_answers(ui, entry, row, people, &mut panel.new_person, &mut panel.error, actions),
                        }
                    }
                });
            }
            if let Detail::History { older, .. } = &focus.detail {
                ui.horizontal(|ui| {
                    if ui.button("Mais recentes").clicked() { panel.before = None; }
                    if ui.add_enabled(*older, egui::Button::new("Mais antigos")).clicked() { panel.before = rows.last().map(|r| r.id); }
                });
            }
        }
        _ => { ui.spinner(); }
    }
    if let Some(opened) = &focus.opened {
        if let Some(texture) = panel.images.get(&opened.image) {
            let shown = ui.add(egui::Image::new(texture).max_width(ui.available_width()));
            if let KindSettings::Objects(objects) = &entry.settings.kind { paint_found(ui, shown.rect, &opened.found, objects, people, None); }
        }
        if ui.button("Fechar imagem").clicked() { panel.opened = None; }
    }
}

/// What a sample can be labelled as: a state, or what a taught box is.
fn label_choices(kind: &KindSettings) -> Vec<(SampleLabel, String)> {
    match kind {
        KindSettings::States(states) => states.states.iter().map(|s| (SampleLabel::State(s.key), s.name.as_str().to_owned())).collect(),
        KindSettings::Objects(objects) => box_choices(objects, true).into_iter().map(|label| (SampleLabel::Box(label), box_label_name(objects, label))).collect(),
        KindSettings::Reading(_) => Vec::new(),
    }
}

/// "Not this" (only for a box the detector found), the sensor's objects, then its own labels.
fn box_choices(objects: &ObjectsSettings, found_by_detector: bool) -> Vec<BoxLabel> {
    let not_this = found_by_detector.then_some(BoxLabel::NotThis);
    not_this.into_iter().chain(objects.classes.iter().map(BoxLabel::Class)).chain(objects.active_labels().map(|l| BoxLabel::Own(l.id))).collect()
}

fn history_label(entry: &SensorEntry, people: &[PersonView], row: &HistoryItem) -> String {
    let objects = match &entry.settings.kind {
        KindSettings::Objects(objects) => objects,
        KindSettings::Reading(reading) => return reading_history_label(reading, row),
        KindSettings::States(_) => return format!("{} · {:.0}%", state_label(entry, Some(&row.value)), row.confidence * 100.0),
    };
    let events: Vec<String> = row.events.iter().map(|event| match event {
        ObjectEvent::On(key) => format!("{} chegou", objects.key_name(*key, people)),
        ObjectEvent::Off(key) => format!("{} saiu", objects.key_name(*key, people)),
        ObjectEvent::Filtered(class) => format!("{} filtrado por caixa ensinada", class.name()),
    }).collect();
    let present = presence_label(objects, people, Some(&row.value));
    if events.is_empty() { present } else { format!("{} · agora: {present}", events.join(", ")) }
}

fn review_label(entry: &SensorEntry, people: &[PersonView], reason: ReviewReason) -> String {
    match (reason, &entry.settings.kind) {
        (ReviewReason::LowConfidence, _) => "Baixa confiança".into(),
        (ReviewReason::Flip, _) => "Mudanças frequentes".into(),
        (ReviewReason::SpotCheck, _) => "Conferência aleatória".into(),
        (ReviewReason::Ask(question), KindSettings::Objects(objects)) => {
            format!("É {}? ({:.0}% parecido)", objects.key_name(ObjectKey::Own(question.label), people), question.similarity * 100.0)
        }
        (ReviewReason::Face(FaceQuestion { person: Some(person), similarity, .. }), _) => {
            format!("É {}? ({:.0}% parecido)", person_name(people, person), similarity * 100.0)
        }
        (ReviewReason::Face(FaceQuestion { person: None, .. }), _) => "Quem é? Rosto de ninguém nomeado".into(),
        (ReviewReason::Rejected(problem), _) => format!("Rejeitada: {}", problem_label(problem)),
        (ReviewReason::Ask(_), KindSettings::States(_) | KindSettings::Reading(_)) => "Pergunta".into(),
    }
}

fn review_answers(ui: &mut egui::Ui, entry: &SensorEntry, row: &HistoryItem, people: &[PersonView], new_person: &mut String, error: &mut Option<String>, actions: &mut Actions) {
    let resolve = |label: Option<SampleLabel>| UserCommand::Resolve { id: entry.id, item: row.id, label };
    if let Some(ReviewReason::Face(question)) = row.review {
        let name = |person: PersonPick| UserCommand::NameFace { id: entry.id, from: FaceFrom::Review(row.id), person };
        if let Some(person) = question.person && ui.button("Sim").clicked() {
            actions.sensors.push(name(PersonPick::Known(person)));
        }
        ui.label(if question.person.is_some() { "Não, é:" } else { "É:" });
        if let Some(pick) = person_picker(ui, people, question.person, new_person, error) {
            actions.sensors.push(name(pick));
        }
        if ui.button("Descartar").clicked() { actions.sensors.push(resolve(None)); }
        return;
    }
    ui.horizontal_wrapped(|ui| {
        match (&entry.settings.kind, row.review) {
            (KindSettings::Objects(objects), Some(ReviewReason::Ask(question))) => {
                if ui.button("Sim").clicked() { actions.sensors.push(resolve(Some(SampleLabel::Box(BoxLabel::Own(question.label))))); }
                if objects.classes.contains(question.detected) && ui.button(format!("Não, é outro {}", question.detected.name().to_lowercase())).clicked() {
                    actions.sensors.push(resolve(Some(SampleLabel::Box(BoxLabel::Class(question.detected)))));
                }
            }
            (KindSettings::States(states), _) => {
                for state in states.states.iter() {
                    if ui.button(format!("É {}", state.name.as_str())).clicked() { actions.sensors.push(resolve(Some(SampleLabel::State(state.key)))); }
                }
            }
            (KindSettings::Objects(_) | KindSettings::Reading(_), _) => {}
        }
        if ui.button("Descartar").clicked() { actions.sensors.push(resolve(None)); }
    });
}

/// A rejected reading: it was read right, the display showed another number, or dismiss it.
fn reading_answers(ui: &mut egui::Ui, id: SensorId, reading: &ReadingSettings, row: &HistoryItem, corrections: &mut HashMap<HistoryId, String>, error: &mut Option<String>, actions: &mut Actions) {
    let answer = |answer: ReadingAnswer| UserCommand::AnswerReading { id, item: row.id, answer };
    ui.horizontal_wrapped(|ui| {
        if let Some(value) = row.read.as_ref().and_then(|r| r.value)
            && ui.button(format!("Leitura certa ({})", with_unit(value, reading.unit.as_str()))).clicked()
        {
            actions.sensors.push(answer(ReadingAnswer::ReadRight));
        }
        let typed = corrections.entry(row.id).or_default();
        ui.add(egui::TextEdit::singleline(typed).hint_text("O visor mostrava…").desired_width(120.0));
        let typed = typed.clone();
        if ui.button("Corrigir").clicked() {
            match reading.typed(&typed) {
                Some(value) => {
                    actions.sensors.push(answer(ReadingAnswer::Showed(value)));
                    corrections.remove(&row.id);
                }
                None => *error = Some("Digite o número que o visor mostrava, ex.: 629.558 ou 0629558; tempo como 1:25 ou 85.".into()),
            }
        }
        if ui.button("Descartar").clicked() { actions.sensors.push(UserCommand::Resolve { id, item: row.id, label: None }); }
    });
}

/// "629.558 m³ · leu «0629551» (93%) · rejeitada: o contador desceu".
fn reading_history_label(reading: &ReadingSettings, row: &HistoryItem) -> String {
    let published = number_label(reading, Some(&row.value));
    let Some(read) = &row.read else { return published };
    let text = if read.text.is_empty() { "—" } else { read.text.as_str() };
    let again = if read.repeats > 0 { format!(" · lida de novo {} vezes", read.repeats) } else { String::new() };
    format!("{published} · leu «{text}» ({:.0}%) · {}{again}", read.score * 100.0, outcome_label(read.outcome))
}

fn outcome_label(outcome: ReadOutcome) -> String {
    match outcome {
        ReadOutcome::Accepted => "aceita".into(),
        ReadOutcome::Settling => "assentando (a última roda está girando; o valor fica)".into(),
        ReadOutcome::Rejected(problem) => format!("rejeitada: {}", problem_label(problem)),
    }
}

fn problem_label(problem: ReadProblem) -> &'static str {
    match problem {
        ReadProblem::NothingRead => "nada lido",
        ReadProblem::Unsure => "leitura insegura",
        ReadProblem::WrongDigitCount => "número de dígitos diferente das rodas",
        ReadProblem::WentDown => "o contador desceu",
        ReadProblem::ChangedTooMuch => "mudou mais que o limite",
        ReadProblem::CameraChanged => "a imagem da câmera mudou de tamanho",
    }
}

/// A value without trailing zeros, at most four decimals.
fn amount(value: f64) -> String {
    let text = format!("{value:.4}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// The value with its unit and flow, what the reader saw, the last reading and how the readings go.
fn reading_live(ui: &mut egui::Ui, images: &HashMap<ImageId, TextureHandle>, entry: &SensorEntry, reading: &ReadingSettings, live: &ReadingLive, focus: &FocusView) {
    let unit = reading.unit.as_str();
    ui.label(egui::RichText::new(number_label(reading, entry.value.as_ref())).size(24.0).strong());
    if let Some(rate) = live.rate {
        ui.label(format!("Vazão: {} {}", amount(rate), reading.rate_unit().0));
    }
    if let Some(seen) = focus.seen {
        ui.weak("O que o leitor viu:");
        image(ui, images, seen, ui.available_width().min(480.0));
    }
    match &live.last {
        Some(last) => {
            let text = if last.text.is_empty() { "—" } else { last.text.as_str() };
            ui.label(format!("Última leitura: «{text}» · {:.0}% · {}", last.score * 100.0, outcome_label(last.outcome)));
        }
        None => { ui.weak("Ainda sem leitura neste processo."); }
    }
    if let Some(share) = live.accepted { ui.weak(format!("Aceitas nas últimas 24 h: {share:.1}%")); }
    if let ReadingMode::Counter { limit, .. } = reading.mode {
        let allowed = live.limit.map_or_else(|| "sem limite de subida".into(), |l| format!("pode subir até {} agora", with_unit(amount(l), unit)));
        ui.weak(format!("Limite: {allowed}"));
        match (limit, live.learned) {
            (ChangeLimit::Auto { .. }, Some(learned)) => {
                ui.weak(format!("Aprendido em {} dias: até {} por hora e {} por dia", learned.days, with_unit(amount(learned.per_hour), unit), with_unit(amount(learned.per_day), unit)));
            }
            (ChangeLimit::Auto { .. }, None) => { ui.weak(format!("Aprendendo o limite: {} de 7 dias com leituras", live.days)); }
            _ => {}
        }
    }
    if live.review_pending > 0 { ui.weak(format!("{} leituras rejeitadas na revisão", live.review_pending)); }
    egui::CollapsingHeader::new("Qualidade").id_salt(("quality", entry.id)).show(ui, |ui| {
        for period in &live.quality {
            let name = match period.days { 1 => "Hoje".to_owned(), days => format!("{days} dias") };
            let share = if period.reads > 0 { format!(" · {:.0}% aceitas", 100.0 * f64::from(period.accepted) / f64::from(period.reads)) } else { String::new() };
            let rejected: Vec<String> = period.rejected.iter().map(|(problem, count)| format!("{} {count}", problem_label(*problem))).collect();
            let rejected = if rejected.is_empty() { String::new() } else { format!(" · rejeitadas: {}", rejected.join(", ")) };
            ui.label(format!("{name}: {} leituras{share}{rejected}", period.reads));
        }
    });
}

/// A reading sensor's settings, edited as a draft and saved with one `Configure`.
fn reading_settings(ui: &mut egui::Ui, panel: &mut SensorsPanel, entry: &SensorEntry, reading: &ReadingSettings, actions: &mut Actions) {
    egui::CollapsingHeader::new("Leitura: modo, visor, unidade e limite").id_salt(("reading", entry.id)).show(ui, |ui| {
        if panel.reading_draft.as_ref().is_none_or(|(id, _)| *id != entry.id) {
            panel.reading_draft = Some((entry.id, reading.clone()));
        }
        let (_, draft) = panel.reading_draft.as_mut().expect("set above");
        ui.push_id(("reading-editor", entry.id), |ui| reading_editor(ui, draft));
        let changed = draft != reading;
        ui.horizontal(|ui| {
            if ui.add_enabled(changed, egui::Button::new("Salvar leitura")).clicked() {
                let mut settings = entry.settings.clone();
                settings.kind = KindSettings::Reading(draft.clone());
                actions.sensors.push(UserCommand::Configure { id: entry.id, settings });
            }
            if ui.add_enabled(changed, egui::Button::new("Desfazer")).clicked() { *draft = reading.clone(); }
        });
    });
}

/// The webhook, the image copy and the one confidence threshold both wait for.
fn notify_settings(ui: &mut egui::Ui, panel: &mut SensorsPanel, entry: &SensorEntry, actions: &mut Actions) {
    egui::CollapsingHeader::new("Notificações: webhook, imagem e limiar").id_salt(("notify", entry.id)).show(ui, |ui| {
        if panel.notify_draft.as_ref().is_none_or(|d| d.id != entry.id) {
            let notify = entry.settings.notify.clone();
            let folder = notify.image.export.as_ref().map(|f| f.as_str().to_owned()).unwrap_or_default();
            let webhook_on = notify.webhook.is_some();
            let export_on = notify.image.export.is_some();
            panel.notify_draft = Some(NotifyDraft { id: entry.id, notify, webhook_on, export_on, url: String::new(), folder });
        }
        let draft = panel.notify_draft.as_mut().expect("set above");
        notify_editor(ui, draft);
        let changed = draft.notify != entry.settings.notify;
        ui.horizontal(|ui| {
            if ui.add_enabled(changed, egui::Button::new("Salvar notificações")).clicked() {
                let mut settings = entry.settings.clone();
                settings.notify = draft.notify.clone();
                actions.sensors.push(UserCommand::Configure { id: entry.id, settings });
            }
            if ui.add_enabled(changed, egui::Button::new("Desfazer")).clicked() {
                let notify = entry.settings.notify.clone();
                draft.folder = notify.image.export.as_ref().map(|f| f.as_str().to_owned()).unwrap_or_default();
                draft.webhook_on = notify.webhook.is_some();
                draft.export_on = notify.image.export.is_some();
                draft.notify = notify;
                draft.url.clear();
            }
        });
    });
}

fn notify_editor(ui: &mut egui::Ui, draft: &mut NotifyDraft) {
    let mut percent = f32::from(draft.notify.threshold) * 100.0;
    if ui.add(egui::DragValue::new(&mut percent).range(0.0..=100.0).speed(1.0).max_decimals(0).suffix("%")).changed()
        && let Ok(unit) = Unit::try_from((percent / 100.0).clamp(0.0, 1.0))
    {
        draft.notify.threshold = unit;
    }
    ui.weak("O aviso e a imagem só saem a partir desta confiança.");

    if ui.checkbox(&mut draft.webhook_on, "Avisar um webhook a cada detecção").changed() && !draft.webhook_on {
        draft.notify.webhook = None;
        draft.url.clear();
    }
    if draft.webhook_on {
        ui.add(egui::TextEdit::singleline(&mut draft.url).hint_text(if draft.notify.webhook.is_some() { "URL atual mantida — digite para trocar" } else { "https://..." }));
        let typed = draft.url.trim().to_owned();
        let parsed = (typed.starts_with("http://") || typed.starts_with("https://"))
            .then(|| SecretUrl::try_from(typed.clone()))
            .and_then(Result::ok);
        match parsed {
            Some(url) => {
                let payload = draft.notify.webhook.as_ref().map_or(Payload::None, |w| w.payload);
                draft.notify.webhook = Some(Webhook { url, payload });
            }
            None if !typed.is_empty() => {
                ui.weak("Só um endereço http:// ou https:// vale aqui.");
            }
            None => {}
        }
        if let Some(webhook) = &mut draft.notify.webhook {
            egui::ComboBox::from_id_salt("payload").selected_text(payload_name(webhook.payload)).show_ui(ui, |ui| {
                for choice in [Payload::None, Payload::Multipart, Payload::Base64] {
                    ui.selectable_value(&mut webhook.payload, choice, payload_name(choice));
                }
            });
        }
    }

    if ui.checkbox(&mut draft.export_on, "Salvar uma cópia da imagem numa pasta").changed() && !draft.export_on {
        draft.notify.image.export = None;
        draft.folder.clear();
    }
    if draft.export_on {
        ui.add(egui::TextEdit::singleline(&mut draft.folder).hint_text("Pasta, por exemplo /Users/voce/Imagens/portao"));
        if ui.button("Escolher pasta…").clicked() {
            let start = draft.folder.trim();
            let mut dialog = rfd::FileDialog::new();
            if !start.is_empty() {
                dialog = dialog.set_directory(start);
            }
            if let Some(picked) = dialog.pick_folder() {
                draft.folder = picked.to_string_lossy().into_owned();
            }
        }
        if let Ok(folder) = ExportFolder::try_from(draft.folder.trim().to_owned()) {
            draft.notify.image.export = Some(folder);
        }
    }

    let mut quality = u8::from(draft.notify.image.quality);
    if ui.add(egui::Slider::new(&mut quality, QUALITY_LIMITS.0..=QUALITY_LIMITS.1).text("Qualidade do JPEG no histórico")).changed()
        && let Ok(q) = Quality::try_from(quality)
    {
        draft.notify.image.quality = q;
    }
}

fn payload_name(payload: Payload) -> &'static str {
    match payload {
        Payload::None => "Só os dados (JSON)",
        Payload::Multipart => "Dados e imagem (multipart)",
        Payload::Base64 => "Dados e imagem em base64",
    }
}

fn mode_name(mode: &ReadingMode) -> &'static str {
    match mode {
        ReadingMode::Counter { .. } => "Contador (só sobe: água, gás, energia)",
        ReadingMode::Value { .. } => "Valor (qualquer número)",
        ReadingMode::TimeLeft { .. } => "Tempo restante (h:mm ou minutos)",
    }
}

fn display_name(display: &ReadingDisplay) -> &'static str {
    match display {
        ReadingDisplay::Auto => "Automático",
        ReadingDisplay::Led => "LED (dígitos claros no escuro)",
        ReadingDisplay::Lcd => "LCD (dígitos escuros no claro)",
        ReadingDisplay::Counter { .. } => "Contador de rodas",
    }
}

fn limit_name(limit: &ChangeLimit) -> &'static str {
    match limit {
        ChangeLimit::Auto { .. } => "Automático (aprende com as leituras)",
        ChangeLimit::Fixed { .. } => "Taxa fixa por hora",
        ChangeLimit::Step { .. } => "Passo fixo por leitura",
        ChangeLimit::Off => "Sem limite",
    }
}

fn amount_field(ui: &mut egui::Ui, value: &mut Amount, suffix: &str) {
    let mut v = f64::from(*value);
    if ui.add(egui::DragValue::new(&mut v).range(0.0..=MAX_AMOUNT).speed(0.01).max_decimals(4).suffix(suffix)).changed()
        && let Ok(amount) = Amount::try_from(v)
    {
        *value = amount;
    }
}

/// Mode, display, decimals, unit, confidence, confirmations and, for a counter, how fast it may
/// go up (origin `READING_DEFAULTS`, `READING_LIMITS`).
fn reading_editor(ui: &mut egui::Ui, reading: &mut ReadingSettings) {
    let standard = 40.0 * 10f64.powi(-i32::from(u8::from(reading.decimals)));
    egui::ComboBox::from_id_salt("mode").selected_text(mode_name(&reading.mode)).show_ui(ui, |ui| {
        let choices = [
            ReadingMode::Counter { limit: ChangeLimit::Auto { max_rate: Amount::ZERO }, rate_window: RateWindow::DEFAULT },
            ReadingMode::Value { max_step: Amount::ZERO },
            ReadingMode::TimeLeft { max_step: Amount::ZERO },
        ];
        for choice in choices {
            let current = std::mem::discriminant(&reading.mode) == std::mem::discriminant(&choice);
            if ui.selectable_label(current, mode_name(&choice)).clicked() && !current {
                if matches!(choice, ReadingMode::TimeLeft { .. }) && reading.unit.as_str().is_empty() {
                    reading.unit = UnitLabel::try_from("min".to_owned()).expect("a short unit");
                }
                reading.mode = choice;
            }
        }
    });
    egui::ComboBox::from_id_salt("display").selected_text(display_name(&reading.display)).show_ui(ui, |ui| {
        let choices = [ReadingDisplay::Auto, ReadingDisplay::Led, ReadingDisplay::Lcd, ReadingDisplay::Counter { digits: Digits::DEFAULT, reader: CounterReader::Wheels(WheelModel::default()) }];
        for choice in choices {
            let current = std::mem::discriminant(&reading.display) == std::mem::discriminant(&choice);
            if ui.selectable_label(current, display_name(&choice)).clicked() && !current { reading.display = choice; }
        }
    });
    if let ReadingDisplay::Counter { digits, reader } = &mut reading.display {
        ui.horizontal(|ui| {
            let mut n = u8::from(*digits);
            if ui.add(egui::DragValue::new(&mut n).range(DIGITS_LIMITS.0..=DIGITS_LIMITS.1).suffix(" rodas")).changed()
                && let Ok(d) = Digits::try_from(n)
            {
                *digits = d;
            }
            let wheels = matches!(reader, CounterReader::Wheels(_));
            if ui.radio(wheels, "Leitor de rodas").clicked() && !wheels { *reader = CounterReader::Wheels(WheelModel::default()); }
            if ui.radio(!wheels, "Texto (OCR)").clicked() { *reader = CounterReader::Ocr; }
        });
        if let CounterReader::Wheels(model) = reader {
            egui::ComboBox::from_id_salt("wheels").selected_text(model.name()).show_ui(ui, |ui| {
                for choice in WheelModel::ALL { ui.selectable_value(model, choice, choice.name()).on_hover_text(choice.description()); }
            });
        }
    }
    ui.horizontal(|ui| {
        if !matches!(reading.mode, ReadingMode::TimeLeft { .. }) {
            let mut decimals = u8::from(reading.decimals);
            if ui.add(egui::DragValue::new(&mut decimals).range(0..=MAX_DECIMALS).suffix(" casas decimais")).changed()
                && let Ok(d) = Decimals::try_from(decimals)
            {
                reading.decimals = d;
            }
        }
        let mut unit = reading.unit.as_str().to_owned();
        ui.label("Unidade");
        if ui.add(egui::TextEdit::singleline(&mut unit).desired_width(60.0)).changed()
            && let Ok(u) = UnitLabel::try_from(unit)
        {
            reading.unit = u;
        }
        for suggestion in ["m³", "L", "kWh", "Wh", "min"] {
            if ui.small_button(suggestion).clicked() { reading.unit = UnitLabel::try_from(suggestion.to_owned()).expect("a short unit"); }
        }
    });
    ui.horizontal(|ui| {
        let mut threshold = f32::from(reading.threshold);
        if ui.add(egui::Slider::new(&mut threshold, 0.0..=1.0).text("confiança mínima")).changed()
            && let Ok(t) = Unit::try_from(threshold)
        {
            reading.threshold = t;
        }
        let mut debounce = u8::from(reading.debounce);
        if ui.add(egui::DragValue::new(&mut debounce).range(DEBOUNCE_LIMITS.0..=DEBOUNCE_LIMITS.1).suffix(" leituras iguais")).changed()
            && let Ok(d) = Debounce::try_from(debounce)
        {
            reading.debounce = d;
        }
    });
    match &mut reading.mode {
        ReadingMode::Counter { limit, rate_window } => {
            egui::ComboBox::from_id_salt("limit").selected_text(limit_name(limit)).show_ui(ui, |ui| {
                let fill = |v: f64| Amount::try_from(v).unwrap_or(Amount::ZERO);
                let choices = [
                    ChangeLimit::Auto { max_rate: Amount::ZERO },
                    ChangeLimit::Fixed { max_rate: fill(standard * 60.0) },
                    ChangeLimit::Step { max_step: fill(standard) },
                    ChangeLimit::Off,
                ];
                for choice in choices {
                    let current = std::mem::discriminant(&*limit) == std::mem::discriminant(&choice);
                    if ui.selectable_label(current, limit_name(&choice)).clicked() && !current { *limit = choice; }
                }
            });
            ui.horizontal(|ui| match limit {
                ChangeLimit::Auto { max_rate } => { ui.label("Até aprender, no máximo"); amount_field(ui, max_rate, " por hora (0 = padrão)"); }
                ChangeLimit::Fixed { max_rate } => { ui.label("No máximo"); amount_field(ui, max_rate, " por hora (0 = sem limite)"); }
                ChangeLimit::Step { max_step } => { ui.label("No máximo"); amount_field(ui, max_step, " por leitura (0 = sem limite)"); }
                ChangeLimit::Off => { ui.weak("Qualquer subida é aceita; descer continua rejeitado."); }
            });
            ui.horizontal(|ui| {
                let mut minutes = u32::from(*rate_window);
                ui.label("Vazão medida em");
                if ui.add(egui::DragValue::new(&mut minutes).range(RATE_WINDOW_LIMITS_MIN.0..=RATE_WINDOW_LIMITS_MIN.1).suffix(" min")).changed()
                    && let Ok(w) = RateWindow::try_from(minutes)
                {
                    *rate_window = w;
                }
            });
        }
        ReadingMode::Value { max_step } | ReadingMode::TimeLeft { max_step } => {
            ui.horizontal(|ui| { ui.label("Maior mudança entre leituras"); amount_field(ui, max_step, " (0 = sem limite)"); });
        }
    }
}

/// One button per person (but `except`) and a field to name a new one; the person chosen.
fn person_picker(ui: &mut egui::Ui, people: &[PersonView], except: Option<PersonId>, new_person: &mut String, error: &mut Option<String>) -> Option<PersonPick> {
    let mut pick = None;
    ui.horizontal_wrapped(|ui| {
        for person in people.iter().filter(|p| Some(p.id) != except) {
            if ui.button(person.name.as_str()).clicked() { pick = Some(PersonPick::Known(person.id)); }
        }
    });
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(new_person).hint_text("Nova pessoa, ex.: Tom").desired_width(160.0));
        if ui.add_enabled(people.len() < MAX_PEOPLE, egui::Button::new("Criar pessoa")).clicked() {
            match Name::try_from(new_person.clone()) {
                Ok(name) => {
                    pick = Some(PersonPick::New { id: PersonId::random(), name });
                    new_person.clear();
                }
                Err(e) => *error = Some(e.to_string()),
            }
        }
    });
    pick
}

/// The classes, own labels, comparison and faces of an object sensor, applied with `Configure`.
fn objects_settings(ui: &mut egui::Ui, entry: &SensorEntry, objects: &ObjectsSettings, system: Option<&SystemStatus>, actions: &mut Actions) {
    egui::CollapsingHeader::new("Objetos, rótulos próprios e rostos").id_salt(("objects", entry.id)).show(ui, |ui| {
        let mut classes: Vec<ObjectClass> = objects.classes.iter().collect();
        class_picker(ui, "sensor-classes", &mut classes);
        let mut changed = objects.clone();
        if let Ok(set) = ClassSet::try_from(classes) {
            changed.classes = set;
        }
        ui.checkbox(&mut changed.use_taught, "Comparar com as caixas ensinadas");
        for label in objects.labels.iter() {
            ui.horizontal(|ui| {
                let unused = if objects.classes.contains(label.parent) { "" } else { " (objeto não escolhido)" };
                ui.label(format!("{} · um tipo de {}{unused}", label.name.as_str(), label.parent.name().to_lowercase()));
                if ui.small_button("Remover").clicked() { changed.labels = changed.labels.without(label.id); }
            });
        }
        ui.add_enabled_ui(objects.classes.contains(ObjectClass::PERSON), |ui| {
            ui.checkbox(&mut changed.faces.enabled, "Reconhecer pessoas pelo rosto")
                .on_disabled_hover_text("Escolha o objeto Pessoa para reconhecer rostos.");
            ui.add_enabled(changed.faces.enabled, egui::Checkbox::new(&mut changed.faces.ask_unknown, "Perguntar quem é quando o rosto é de ninguém nomeado"));
        });
        if objects.faces_on() {
            let indexing = if system.is_some_and(|s| s.faces_indexing) { " Preparando os rostos ensinados…" } else { "" };
            ui.weak(format!("Os rostos ficam só neste computador. Enquanto os modelos de rosto baixam, o sensor segue contando objetos.{indexing}"));
        }
        if changed != *objects {
            let mut settings = entry.settings.clone();
            settings.kind = KindSettings::Objects(changed);
            actions.sensors.push(UserCommand::Configure { id: entry.id, settings });
        }
    });
}

fn live(ui: &mut egui::Ui, panel: &mut SensorsPanel, entry: &SensorEntry, people: &[PersonView], focus: &FocusView, actions: &mut Actions) {
    let objects = match (&entry.settings.kind, &entry.live) { (KindSettings::Objects(o), KindLive::Objects(l)) => Some((o, l)), _ => None };
    if let (KindSettings::Reading(reading), KindLive::Reading(state)) = (&entry.settings.kind, &entry.live) {
        reading_live(ui, &panel.images, entry, reading, state, focus);
        ui.weak("Desenhe um retângulo justo em volta do número e salve a região.");
    }
    ui.horizontal(|ui| {
        if objects.is_some() {
            choice(ui, &mut panel.teaching_boxes, false, "Região");
            choice(ui, &mut panel.teaching_boxes, true, "Ensinar caixas");
            ui.separator();
        }
        ui.add_enabled_ui(!panel.teaching_boxes, |ui| {
            if choice(ui, &mut panel.polygon_mode, false, "Retângulo") { panel.polygon.clear(); }
            choice(ui, &mut panel.polygon_mode, true, "Polígono");
            if ui.button("Limpar").clicked() { panel.region = Some(Region::WHOLE); panel.polygon.clear(); }
            if ui.add_enabled(panel.polygon.len() >= 3, egui::Button::new("Fechar")).clicked() {
                match Region::polygon(panel.polygon.clone()) {
                    Ok(region) => { panel.region = Some(region); panel.polygon.clear(); }
                    Err(e) => panel.error = Some(e.to_string()),
                }
            }
        });
    });
    let Some(preview) = &focus.preview else {
        ui.label("Aguardando prévia da fonte…");
        save_region(ui, panel, entry, actions);
        return;
    };
    if let Some(texture) = panel.images.get(&preview.image) {
        let width = ui.available_width();
        let size = texture.size_vec2() * (width / texture.size_vec2().x);
        let (rect, response) = ui.allocate_exact_size(size, Sense::click_and_drag());
        ui.painter().image(texture.id(), rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        let normal = |p: Pos2| NormPoint::new(((p.x - rect.min.x) / rect.width()).clamp(0.0, 1.0), ((p.y - rect.min.y) / rect.height()).clamp(0.0, 1.0)).expect("clamped point");
        // The boxes belong to the analysed frame: drawn only while that frame is the one shown.
        let found = objects.filter(|(_, l)| l.frame == Some(preview.frame)).map_or(&[][..], |(_, l)| &l.found[..]);
        if panel.teaching_boxes {
            if response.clicked() && let Some(p) = response.interact_pointer_pos().map(normal) {
                let hit = found.iter().filter(|f| f.area[0] <= p.x() && p.x() <= f.area[2] && f.area[1] <= p.y() && p.y() <= f.area[3])
                    .min_by(|a, b| area(a.area).total_cmp(&area(b.area)));
                panel.picked = hit.and_then(|f| {
                    let detected = match f.verdict { Verdict::Taught { was: Some(was), .. } => was, _ => f.class };
                    NormBox::try_from(f.area).ok().map(|area| Picked { frame: preview.frame, area, detected: Some(detected), face: f.face })
                });
            }
            if response.drag_started() { panel.drag_start = response.interact_pointer_pos().map(normal); }
            if let (Some(start), Some(end)) = (panel.drag_start, response.interact_pointer_pos().map(normal)) {
                if response.drag_stopped() {
                    panel.picked = NormBox::between(start, end).ok().map(|area| Picked { frame: preview.frame, area, detected: None, face: None });
                    panel.drag_start = None;
                } else if response.dragged() {
                    ui.painter().rect_stroke(screen(rect, [start.x(), start.y(), end.x(), end.y()]), 0.0, Stroke::new(2.0, Color32::WHITE), StrokeKind::Middle);
                }
            }
        } else if panel.polygon_mode {
            if response.clicked() && panel.polygon.len() < MAX_POLYGON_POINTS
                && let Some(p) = response.interact_pointer_pos() { panel.polygon.push(normal(p)); }
        } else {
            if response.drag_started() { panel.drag_start = response.interact_pointer_pos().map(normal); }
            if (response.dragged() || response.drag_stopped())
                && let (Some(start), Some(end)) = (panel.drag_start, response.interact_pointer_pos())
                && let Ok(region) = Region::rect(start, normal(end)) { panel.region = Some(region); }
            if response.drag_stopped() { panel.drag_start = None; }
        }
        paint_region(ui, rect, panel.region.as_ref().unwrap_or(&entry.settings.region));
        let points: Vec<Pos2> = panel.polygon.iter().map(|p| rect.min + vec2(p.x() * rect.width(), p.y() * rect.height())).collect();
        for p in &points { ui.painter().circle_filled(*p, 3.0, Color32::YELLOW); }
        for pair in points.windows(2) { ui.painter().line_segment([pair[0], pair[1]], Stroke::new(2.0, Color32::YELLOW)); }
        if let Some((settings, _)) = objects {
            let picked = panel.picked.filter(|p| p.frame == preview.frame).map(|p| p.area.area());
            paint_found(ui, rect, found, settings, people, picked);
        }
    }
    match objects {
        Some((settings, live)) => objects_live(ui, panel, entry, settings, people, live, actions),
        None => states_live(ui, entry, preview, actions),
    }
    save_region(ui, panel, entry, actions);
}

fn save_region(ui: &mut egui::Ui, panel: &mut SensorsPanel, entry: &SensorEntry, actions: &mut Actions) {
    if ui.button("Salvar região").clicked() {
        let mut settings = entry.settings.clone();
        if let Some(region) = panel.region.take() { settings.region = region; }
        actions.sensors.push(UserCommand::Configure { id: entry.id, settings });
    }
}

fn states_live(ui: &mut egui::Ui, entry: &SensorEntry, preview: &Preview, actions: &mut Actions) {
    let (KindSettings::States(states), KindLive::States(live)) = (&entry.settings.kind, &entry.live) else { return };
    ui.label("Ensinar o quadro visto (teclas 1–9)");
    let keys = [egui::Key::Num1, egui::Key::Num2, egui::Key::Num3, egui::Key::Num4, egui::Key::Num5, egui::Key::Num6, egui::Key::Num7, egui::Key::Num8, egui::Key::Num9];
    ui.horizontal_wrapped(|ui| {
        for state in states.states.iter() {
            let key = u8::from(state.key);
            let label = format!("{key} · {} ({})", state.name.as_str(), live.samples.get(&state.key).copied().unwrap_or(0));
            if ui.button(label).clicked() || (!ui.ctx().text_edit_focused() && ui.input(|i| i.key_pressed(keys[key as usize - 1]))) {
                actions.sensors.push(UserCommand::Teach { id: entry.id, frame: preview.frame, state: state.key });
            }
        }
    });
    if live.training { ui.label("Treinando…"); }
}

/// Each object with its presence and count, then what the picked box is, and whose face it shows.
fn objects_live(ui: &mut egui::Ui, panel: &mut SensorsPanel, entry: &SensorEntry, objects: &ObjectsSettings, people: &[PersonView], live: &ObjectsLive, actions: &mut Actions) {
    for track in &live.tracks {
        let state = if track.on { format!("presente ({})", track.count) } else { "ausente".into() };
        let score = if track.score > 0.0 { format!(" · {:.0}%", track.score * 100.0) } else { String::new() };
        ui.label(format!("{}: {state}{score}", objects.key_name(track.key, people)));
    }
    let comparing = if live.indexing { " · preparando comparação…" } else { "" };
    ui.weak(format!("{} caixas ensinadas{comparing}", live.taught));
    if !panel.teaching_boxes {
        let faces = if objects.faces_on() { " Toque numa pessoa para dar nome ao rosto." } else { "" };
        ui.weak(format!("Em Ensinar caixas, toque numa caixa para corrigi-la ou arraste em volta de algo que não foi achado.{faces}"));
        return;
    }
    let Some(picked) = panel.picked else {
        ui.label("Toque numa caixa ou arraste para desenhar uma.");
        return;
    };
    if objects.faces_on() && let Some(look) = picked.face {
        if look.age == 0 && look.usable() {
            ui.label("O rosto desta pessoa é:");
            if let Some(person) = person_picker(ui, people, None, &mut panel.new_person, &mut panel.error) {
                actions.sensors.push(UserCommand::NameFace { id: entry.id, from: FaceFrom::Frame { frame: picked.frame, area: picked.area }, person });
                panel.picked = None;
            }
        } else {
            ui.weak(look.problem.map_or("O rosto não foi procurado neste quadro.", face_problem_label));
        }
        ui.separator();
    }
    let teach = |label: BoxLabel| UserCommand::TeachBox { id: entry.id, frame: picked.frame, area: picked.area, detected: picked.detected, label };
    ui.label(match picked.detected { Some(class) => format!("Caixa achada como {}. Ela é:", class.name().to_lowercase()), None => "Caixa desenhada. Ela é:".into() });
    ui.horizontal_wrapped(|ui| {
        for label in box_choices(objects, picked.detected.is_some()) {
            if ui.button(box_label_name(objects, label)).clicked() { actions.sensors.push(teach(label)); panel.picked = None; }
        }
    });
    let parent = picked.detected.filter(|c| objects.classes.contains(*c)).or_else(|| objects.classes.iter().next());
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut panel.new_label).hint_text("Novo rótulo, ex.: Nosso carro").desired_width(180.0));
        let Some(parent) = parent else { return };
        if ui.button(format!("Criar ({})", parent.name().to_lowercase())).clicked() {
            let label = Name::try_from(panel.new_label.clone()).map(|name| OwnLabel { id: LabelId::random(), name, parent });
            match label.and_then(|label| Ok((label.id, objects.labels.with(label)?))) {
                Ok((id, labels)) => {
                    let mut settings = entry.settings.clone();
                    settings.kind = KindSettings::Objects(ObjectsSettings { labels, ..objects.clone() });
                    actions.sensors.push(UserCommand::Configure { id: entry.id, settings });
                    actions.sensors.push(teach(BoxLabel::Own(id)));
                    panel.new_label.clear();
                    panel.picked = None;
                }
                Err(error) => panel.error = Some(error.to_string()),
            }
        }
    });
}

fn area(a: [f32; 4]) -> f32 {
    (a[2] - a[0]) * (a[3] - a[1])
}

fn screen(rect: Rect, a: [f32; 4]) -> Rect {
    Rect::from_two_pos(rect.min + vec2(a[0] * rect.width(), a[1] * rect.height()), rect.min + vec2(a[2] * rect.width(), a[3] * rect.height()))
}

/// The boxes of a check over the frame they were found in, each with what it counts as, and the
/// faces found in the person boxes.
fn paint_found(ui: &egui::Ui, rect: Rect, found: &[Found], objects: &ObjectsSettings, people: &[PersonView], picked: Option<[f32; 4]>) {
    for f in found {
        let (color, name) = match (f.verdict, f.face) {
            (Verdict::Filtered(_), _) => (FILTERED_COLOR, format!("{} (filtrado)", f.class.name())),
            (Verdict::Unsure { label, .. }, _) => (UNSURE_COLOR, format!("{}?", objects.key_name(ObjectKey::Own(label), people))),
            (Verdict::Taught { label: Some(label), .. }, _) => (OWN_LABEL_COLOR, objects.key_name(ObjectKey::Own(label), people).to_owned()),
            (_, Some(look)) if look.person().is_some() => (PERSON_COLOR, look.person().map_or("?", |p| person_name(people, p)).to_owned()),
            (_, Some(FaceLook { state: FaceState::Ask(person), .. })) => (UNSURE_COLOR, format!("{}?", person_name(people, person))),
            (_, Some(look)) if look.stranger() => (PERSON_COLOR, objects.key_name(ObjectKey::Stranger, people).to_owned()),
            (Verdict::Taught { .. } | Verdict::Detector, _) => (BOX_COLOR, f.class.name().to_owned()),
        };
        let shown = screen(rect, f.area);
        let width = if picked == Some(f.area) { 4.0 } else { 2.0 };
        ui.painter().rect_stroke(shown, 0.0, Stroke::new(width, if width > 2.0 { Color32::WHITE } else { color }), StrokeKind::Middle);
        if let Some(marks) = f.face.and_then(|l| l.marks) {
            let usable = f.face.is_some_and(|l| l.usable());
            ui.painter().rect_stroke(screen(rect, marks.area), 0.0, Stroke::new(1.0, if usable { PERSON_COLOR } else { FILTERED_COLOR }), StrokeKind::Middle);
        }
        let text = format!("{name} {:.0}%", f.score * 100.0);
        let galley = ui.painter().layout_no_wrap(text, FontId::proportional(12.0), Color32::BLACK);
        let tag = Align2::LEFT_BOTTOM.anchor_size(shown.left_top(), galley.size() + vec2(6.0, 2.0));
        ui.painter().rect_filled(tag, 2.0, color);
        ui.painter().galley(tag.min + vec2(3.0, 1.0), galley, Color32::BLACK);
    }
    if let Some(a) = picked.filter(|a| !found.iter().any(|f| f.area == *a)) {
        ui.painter().rect_stroke(screen(rect, a), 0.0, Stroke::new(3.0, Color32::WHITE), StrokeKind::Middle);
    }
}

fn face_problem_label(problem: FaceProblem) -> &'static str {
    match problem {
        FaceProblem::Missing => "Nenhum rosto nesta caixa.",
        FaceProblem::Cut => "O rosto está cortado pela borda da imagem.",
        FaceProblem::Small => "O rosto está pequeno demais.",
        FaceProblem::Unsure => "Não dá para ter certeza de que é um rosto.",
        FaceProblem::Turned => "O rosto está virado de lado.",
        FaceProblem::Blurry => "O rosto está desfocado.",
    }
}

/// Everyone named, shared by every sensor: rename, where and when each was seen, their taught
/// faces (moved to another person or deleted), and forgetting them.
fn people(ui: &mut egui::Ui, scene: &Scene, panel: &mut SensorsPanel, actions: &mut Actions) {
    ui.separator();
    ui.strong("Pessoas");
    let people = scene.sensors.people();
    if people.is_empty() {
        ui.label("Ninguém foi nomeado. Ligue o reconhecimento de rostos num sensor de Objetos e, em Ensinar caixas, toque numa pessoa.");
        return;
    }
    let faces = match scene.sensors.focus.as_ref().filter(|f| f.sensor.is_none()).map(|f| &f.detail) {
        Some(Detail::People(faces)) => &faces[..],
        _ => &[],
    };
    let sensor_name = |id: SensorId| scene.sensors.sensors.get(&id).map_or("sensor apagado", |e| e.settings.name.as_str());
    for person in people {
        ui.push_id(person.id, |ui| {
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    let name = panel.renames.entry(person.id).or_insert_with(|| person.name.as_str().to_owned());
                    ui.add(egui::TextEdit::singleline(name).desired_width(160.0));
                    if name.as_str() != person.name.as_str() && ui.button("Renomear").clicked() {
                        match Name::try_from(name.clone()) {
                            Ok(name) => actions.sensors.push(UserCommand::RenamePerson { person: person.id, name }),
                            Err(e) => panel.error = Some(e.to_string()),
                        }
                        panel.renames.remove(&person.id);
                    }
                });
                let here: Vec<&str> = person.here.iter().map(|id| sensor_name(*id)).collect();
                let seen = if !here.is_empty() {
                    format!("Vista agora em {}", here.join(", "))
                } else if let Some(last) = person.last_seen {
                    format!("Vista por último em {} ({})", sensor_name(last.sensor), timestamp(last.at))
                } else {
                    "Ainda não vista desde que o app abriu".into()
                };
                ui.weak(format!("{} rostos ensinados · {seen}", person.faces));
                ui.horizontal_wrapped(|ui| {
                    for face in faces.iter().filter(|f| f.person == person.id) {
                        ui.push_id(face.id, |ui| {
                            ui.vertical(|ui| {
                                image(ui, &panel.images, face.thumb, 64.0);
                                ui.weak(face.sensor.map_or("sensor apagado", sensor_name));
                                egui::ComboBox::from_id_salt("move").selected_text("Mover").width(64.0).show_ui(ui, |ui| {
                                    for other in people.iter().filter(|p| p.id != person.id) {
                                        if ui.selectable_label(false, other.name.as_str()).clicked() {
                                            actions.sensors.push(UserCommand::RelabelFace { face: face.id, person: Some(other.id) });
                                        }
                                    }
                                });
                                if ui.small_button("Excluir").clicked() { actions.sensors.push(UserCommand::RelabelFace { face: face.id, person: None }); }
                            });
                        });
                    }
                });
                if panel.forget_confirm == Some(person.id) {
                    ui.label(format!("Apagar {} e os {} rostos ensinados?", person.name.as_str(), person.faces));
                    ui.horizontal(|ui| {
                        if ui.button("Confirmar").clicked() { actions.sensors.push(UserCommand::ForgetPerson(person.id)); panel.forget_confirm = None; }
                        if ui.button("Cancelar").clicked() { panel.forget_confirm = None; }
                    });
                } else if ui.button("Apagar pessoa").clicked() {
                    panel.forget_confirm = Some(person.id);
                }
            });
        });
    }
}

fn paint_region(ui: &egui::Ui, rect: Rect, region: &Region) {
    let point = |p: &NormPoint| rect.min + vec2(p.x() * rect.width(), p.y() * rect.height());
    let mut points = match region.shape() {
        RegionShape::Polygon(points) => points.iter().map(point).collect::<Vec<_>>(),
        _ => { let (a, b) = region.bounds(); let a = point(&a); let b = point(&b); vec![a, pos2(b.x, a.y), b, pos2(a.x, b.y)] }
    };
    if let Some(first) = points.first().copied() { points.push(first); }
    ui.painter().add(egui::Shape::line(points, Stroke::new(2.0, Color32::LIGHT_GREEN)));
}

fn image(ui: &mut egui::Ui, images: &HashMap<ImageId, TextureHandle>, id: ImageId, width: f32) {
    if let Some(texture) = images.get(&id) { ui.add(egui::Image::new(texture).max_width(width)); }
}

fn settings(ui: &mut egui::Ui, view: &SensorsView, actions: &mut Actions) {
    ui.collapsing("Configurações e retenção", |ui| {
        if let Some(system) = &view.system {
            let mut days = system.retention.max_age.as_secs() / 86400;
            let mut mb = system.retention.max_bytes / (1024 * 1024);
            let mut changed = false;
            ui.horizontal(|ui| { changed |= ui.add(egui::DragValue::new(&mut days).range(1..=3650).suffix(" dias")).changed(); changed |= ui.add(egui::DragValue::new(&mut mb).range(1..=1048576).suffix(" MB")).changed(); });
            if changed { actions.sensors.push(UserCommand::SetRetention(Retention { max_age: Duration::from_secs(days * 86400), max_bytes: mb * 1024 * 1024 })); }
            ui.weak(format!("Histórico: {} MB · Amostras: {} MB (não expiram)", system.usage.history_bytes / 1048576, system.usage.sample_bytes / 1048576));
            let mut detector = system.detector;
            egui::ComboBox::from_label("Detector de objetos").selected_text(detector.name()).show_ui(ui, |ui| {
                for choice in Detector::ALL { ui.selectable_value(&mut detector, choice, choice.name()).on_hover_text(choice.description()); }
            });
            if detector != system.detector { actions.sensors.push(UserCommand::SetDetector(detector)); }
            let mut recogniser = system.recogniser;
            egui::ComboBox::from_label("Reconhecedor de rostos").selected_text(recogniser.name()).show_ui(ui, |ui| {
                for choice in Recogniser::ALL { ui.selectable_value(&mut recogniser, choice, choice.name()).on_hover_text(choice.description()); }
            });
            if recogniser != system.recogniser { actions.sensors.push(UserCommand::SetRecogniser(recogniser)); }
            let mut number_reader = system.number_reader;
            egui::ComboBox::from_label("Leitor de números").selected_text(number_reader.name()).show_ui(ui, |ui| {
                for choice in NumberReader::ALL { ui.selectable_value(&mut number_reader, choice, choice.name()).on_hover_text(choice.description()); }
            });
            if number_reader != system.number_reader { actions.sensors.push(UserCommand::SetNumberReader(number_reader)); }
            for model in &system.models {
                let state = match &model.state {
                    ModelState::NotNeeded => "não baixado".into(),
                    ModelState::Downloading { done, total } => format!("baixando {:.0}%", *done as f32 * 100.0 / (*total).max(1) as f32),
                    ModelState::Ready => "pronto".into(),
                    ModelState::Failed(error) => format!("falhou ({error})"),
                };
                ui.weak(format!("{}: {state}", model.name));
            }
            if let Some(notice) = &system.notice { ui.colored_label(Color32::YELLOW, notice_label(&notice.kind)); }
        }
    });
}

fn notice_label(notice: &NoticeKind) -> String {
    match notice {
        NoticeKind::FrameExpired => "O quadro expirou. Ensine novamente sobre a prévia atual.".into(),
        NoticeKind::KindMismatch => "Não é possível trocar o tipo do sensor.".into(),
        NoticeKind::UnknownItem => "Item não encontrado.".into(),
        NoticeKind::OutsideRegion => "A base da caixa está fora da região, então ela nunca contaria.".into(),
        NoticeKind::TaughtFull => format!("O sensor já tem {MAX_TAUGHT} caixas ensinadas."),
        NoticeKind::FacesOff => "Ligue o reconhecimento de rostos neste sensor primeiro.".into(),
        NoticeKind::NoFace(problem) => format!("Esse rosto não pode ser ensinado. {}", face_problem_label(*problem)),
        NoticeKind::PeopleFull => format!("Já existem {MAX_PEOPLE} pessoas."),
        NoticeKind::FacesFull => format!("Essa pessoa já tem {MAX_FACES_PER_PERSON} rostos ensinados."),
        NoticeKind::NameTaken => "Já existe uma pessoa com esse nome.".into(),
        NoticeKind::Webhook(error) => format!("Falha ao avisar o webhook: {error}"),
        NoticeKind::Export(error) => format!("Falha ao salvar a imagem: {error}"),
        NoticeKind::Store(error) => format!("Falha ao gravar: {error}"),
    }
}

fn camera_label(scene: &Scene, camera: &CameraId) -> String {
    scene.account.devices().and_then(|d| d.get(&camera.dvr)).map(|dvr| {
        let name = dvr.channels.iter().find(|(ch, _)| *ch == camera.channel).map(|(_, name)| name.as_str()).unwrap_or("Câmera");
        format!("{} / {name}", dvr.name)
    }).unwrap_or_else(|| format!("{} / Canal {}", camera.dvr.as_str(), camera.channel.rtsp_channel()))
}

fn timestamp(at: SystemTime) -> String {
    let seconds = at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as libc::time_t;
    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: both pointers are valid for this call; the result is read only on success.
    unsafe {
        if libc::localtime_r(&seconds, local.as_mut_ptr()).is_null() { return String::new(); }
        let t = local.assume_init();
        format!("{:02}/{:02} {:02}:{:02}:{:02}", t.tm_mday, t.tm_mon + 1, t.tm_hour, t.tm_min, t.tm_sec)
    }
}
