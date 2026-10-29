use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, TextureHandle, TextureOptions, pos2, vec2};

use crate::frames::StreamKind;
use crate::model::CameraId;
use crate::procs::{ChildKey, ChildPhase};
use crate::sensors::*;
use crate::ui::{Actions, Scene};

/// Origin `OBJECT_BOX_COLOR`.
const BOX_COLOR: Color32 = Color32::from_rgb(0x7e, 0xe2, 0xb8);
const OWN_LABEL_COLOR: Color32 = Color32::from_rgb(0x6c, 0xc4, 0xff);
const UNSURE_COLOR: Color32 = Color32::YELLOW;
const FILTERED_COLOR: Color32 = Color32::from_gray(140);

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
}

/// A box chosen on the preview, to teach what it is.
#[derive(Clone, Copy)]
struct Picked {
    frame: FrameId,
    area: NormBox,
    /// The detector's class for a tapped box; `None` for a drawn one.
    detected: Option<ObjectClass>,
}

struct NewSensor {
    name: String,
    url: String,
    camera: Option<CameraId>,
    use_url: bool,
    stream: StreamKind,
    objects: bool,
    states: Vec<String>,
    classes: Vec<ObjectClass>,
}

impl Default for NewSensor {
    fn default() -> Self {
        Self { name: String::new(), url: String::new(), camera: None, use_url: true, stream: StreamKind::Sub, objects: false,
            states: vec!["Aberto".into(), "Fechado".into()], classes: ObjectClass::by_key("person").into_iter().collect() }
    }
}

impl SensorsPanel {
    pub fn interest(&self) -> Interest {
        if self.open { Interest { sensor: self.selected, tab: self.tab, history_before: self.before, samples_before: self.samples_before, open: self.opened } }
        else { Interest::default() }
    }

    fn select(&mut self, id: SensorId) {
        self.selected = Some(id);
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
    }
}

pub fn value_label(entry: &SensorEntry) -> String {
    match &entry.status {
        SensorStatus::Live => match &entry.settings.kind {
            KindSettings::States(_) => state_label(entry, entry.value.as_ref()),
            KindSettings::Objects(objects) => presence_label(objects, entry.value.as_ref()),
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

/// "Carro 2 · Pessoa 1": each object present with its count.
fn presence_label(objects: &ObjectsSettings, value: Option<&SensorValue>) -> String {
    match value {
        Some(SensorValue::Objects(present)) if present.is_empty() => "nenhum objeto".into(),
        Some(SensorValue::Objects(present)) => present.iter().map(|(key, count)| format!("{} {count}", objects.key_name(*key))).collect::<Vec<_>>().join(" · "),
        _ => "?".into(),
    }
}

fn box_label_name(objects: &ObjectsSettings, label: BoxLabel) -> String {
    match label {
        BoxLabel::NotThis => "Não é isso".into(),
        BoxLabel::Class(class) => class.name().into(),
        BoxLabel::Own(id) => objects.key_name(ObjectKey::Own(id)).into(),
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
            let text = format!("{}: {}", entry.settings.name.as_str(), value_label(entry));
            if ui.selectable_label(panel.selected == Some(entry.id), text).clicked() { panel.select(entry.id); }
        }
        ui.add_enabled_ui(ready, |ui| {
            if ui.button("Novo sensor").clicked() { panel.create = Some(NewSensor::default()); }
            if let Some(mut form) = panel.create.take() {
                let close = new_sensor(ui, scene, panel, &mut form, actions);
                if !close { panel.create = Some(form); }
            }
            if let Some(entry) = panel.selected.and_then(|id| scene.sensors.sensors.get(&id)) {
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
            ui.radio_value(&mut form.objects, false, "Estado");
            ui.radio_value(&mut form.objects, true, "Objetos");
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
        if form.objects {
            ui.label(format!("Objetos ({} de {OBJECT_MAX_CLASSES})", form.classes.len()));
            class_picker(ui, "new-sensor-classes", &mut form.classes);
        } else {
            for (index, state) in form.states.iter_mut().enumerate() {
                ui.horizontal(|ui| { ui.label(format!("Estado {}", index + 1)); ui.text_edit_singleline(state); });
            }
            ui.horizontal(|ui| {
                if ui.add_enabled(form.states.len() < MAX_STATES, egui::Button::new("+ Estado")).clicked() { form.states.push(String::new()); }
                if ui.add_enabled(form.states.len() > 1, egui::Button::new("− Estado")).clicked() { form.states.pop(); }
            });
        }
        ui.horizontal(|ui| {
            if ui.button("Criar").clicked() {
                let result = (|| -> Result<_, InvalidInput> {
                    let source = if form.use_url { SourceSpec::Url(form.url.clone().try_into()?) }
                        else { SourceSpec::Dvr(form.camera.clone().ok_or(InvalidInput("escolha uma câmera"))?) };
                    let name: Name = form.name.clone().try_into()?;
                    let settings = if form.objects { Settings::new_objects(name, form.classes.clone().try_into()?) } else {
                        let states = form.states.iter().enumerate().map(|(i, name)| Ok(StateDef { key: ((i + 1) as u8).try_into()?, name: name.clone().try_into()? })).collect::<Result<Vec<_>, InvalidInput>>()?.try_into()?;
                        Settings::new_states(name, states)
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
    ui.strong(format!("{}: {}", entry.settings.name.as_str(), value_label(entry)));
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
    if let KindSettings::Objects(objects) = &entry.settings.kind { objects_settings(ui, entry, objects, actions); }
    let samples = if matches!(entry.settings.kind, KindSettings::Objects(_)) { "Ensinados" } else { "Amostras" };
    ui.horizontal(|ui| {
        for (tab, label) in [(Tab::Live, "Ao vivo"), (Tab::Samples, samples), (Tab::Review, "Revisão"), (Tab::History, "Histórico")] {
            if ui.selectable_value(&mut panel.tab, tab, label).changed() { panel.opened = None; panel.before = None; panel.samples_before = None; panel.sample_pages.clear(); }
        }
    });
    let Some(focus) = scene.sensors.focus.as_ref().filter(|f| f.sensor == entry.id) else { ui.spinner(); return };
    match (&focus.detail, panel.tab) {
        (Detail::Live, Tab::Live) => live(ui, panel, entry, focus, actions),
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
                            ui.label(history_label(entry, row));
                            ui.weak(timestamp(row.at));
                            if let Some(reason) = row.review { ui.label(review_label(entry, reason)); }
                        });
                    });
                    if panel.tab == Tab::Review { review_answers(ui, entry, row, actions); }
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
            if let KindSettings::Objects(objects) = &entry.settings.kind { paint_found(ui, shown.rect, &opened.found, objects, None); }
        }
        if ui.button("Fechar imagem").clicked() { panel.opened = None; }
    }
}

/// What a sample can be labelled as: a state, or what a taught box is.
fn label_choices(kind: &KindSettings) -> Vec<(SampleLabel, String)> {
    match kind {
        KindSettings::States(states) => states.states.iter().map(|s| (SampleLabel::State(s.key), s.name.as_str().to_owned())).collect(),
        KindSettings::Objects(objects) => box_choices(objects, true).into_iter().map(|label| (SampleLabel::Box(label), box_label_name(objects, label))).collect(),
    }
}

/// "Not this" (only for a box the detector found), the sensor's objects, then its own labels.
fn box_choices(objects: &ObjectsSettings, found_by_detector: bool) -> Vec<BoxLabel> {
    let not_this = found_by_detector.then_some(BoxLabel::NotThis);
    not_this.into_iter().chain(objects.classes.iter().map(BoxLabel::Class)).chain(objects.active_labels().map(|l| BoxLabel::Own(l.id))).collect()
}

fn history_label(entry: &SensorEntry, row: &HistoryItem) -> String {
    let KindSettings::Objects(objects) = &entry.settings.kind else {
        return format!("{} · {:.0}%", state_label(entry, Some(&row.value)), row.confidence * 100.0);
    };
    let events: Vec<String> = row.events.iter().map(|event| match event {
        ObjectEvent::On(key) => format!("{} chegou", objects.key_name(*key)),
        ObjectEvent::Off(key) => format!("{} saiu", objects.key_name(*key)),
        ObjectEvent::Filtered(class) => format!("{} filtrado por caixa ensinada", class.name()),
    }).collect();
    let present = presence_label(objects, Some(&row.value));
    if events.is_empty() { present } else { format!("{} · agora: {present}", events.join(", ")) }
}

fn review_label(entry: &SensorEntry, reason: ReviewReason) -> String {
    match (reason, &entry.settings.kind) {
        (ReviewReason::LowConfidence, _) => "Baixa confiança".into(),
        (ReviewReason::Flip, _) => "Mudanças frequentes".into(),
        (ReviewReason::SpotCheck, _) => "Conferência aleatória".into(),
        (ReviewReason::Ask(question), KindSettings::Objects(objects)) => {
            format!("É {}? ({:.0}% parecido)", objects.key_name(ObjectKey::Own(question.label)), question.similarity * 100.0)
        }
        (ReviewReason::Ask(_), KindSettings::States(_)) => "Pergunta".into(),
    }
}

fn review_answers(ui: &mut egui::Ui, entry: &SensorEntry, row: &HistoryItem, actions: &mut Actions) {
    let resolve = |label: Option<SampleLabel>| UserCommand::Resolve { id: entry.id, item: row.id, label };
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
            (KindSettings::Objects(_), _) => {}
        }
        if ui.button("Descartar").clicked() { actions.sensors.push(resolve(None)); }
    });
}

/// The classes, own labels and comparison of an object sensor, applied with `Configure`.
fn objects_settings(ui: &mut egui::Ui, entry: &SensorEntry, objects: &ObjectsSettings, actions: &mut Actions) {
    egui::CollapsingHeader::new("Objetos e rótulos próprios").id_salt(("objects", entry.id)).show(ui, |ui| {
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
        if changed != *objects {
            let mut settings = entry.settings.clone();
            settings.kind = KindSettings::Objects(changed);
            actions.sensors.push(UserCommand::Configure { id: entry.id, settings });
        }
    });
}

fn live(ui: &mut egui::Ui, panel: &mut SensorsPanel, entry: &SensorEntry, focus: &FocusView, actions: &mut Actions) {
    let objects = match (&entry.settings.kind, &entry.live) { (KindSettings::Objects(o), KindLive::Objects(l)) => Some((o, l)), _ => None };
    ui.horizontal(|ui| {
        if objects.is_some() {
            ui.selectable_value(&mut panel.teaching_boxes, false, "Região");
            ui.selectable_value(&mut panel.teaching_boxes, true, "Ensinar caixas");
            ui.separator();
        }
        ui.add_enabled_ui(!panel.teaching_boxes, |ui| {
            if ui.selectable_value(&mut panel.polygon_mode, false, "Retângulo").changed() { panel.polygon.clear(); }
            ui.selectable_value(&mut panel.polygon_mode, true, "Polígono");
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
                    NormBox::try_from(f.area).ok().map(|area| Picked { frame: preview.frame, area, detected: Some(detected) })
                });
            }
            if response.drag_started() { panel.drag_start = response.interact_pointer_pos().map(normal); }
            if let (Some(start), Some(end)) = (panel.drag_start, response.interact_pointer_pos().map(normal)) {
                if response.drag_stopped() {
                    panel.picked = NormBox::between(start, end).ok().map(|area| Picked { frame: preview.frame, area, detected: None });
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
            paint_found(ui, rect, found, settings, picked);
        }
    }
    match objects {
        Some((settings, live)) => objects_live(ui, panel, entry, settings, live, actions),
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

/// Each object with its presence and count, then what the picked box is.
fn objects_live(ui: &mut egui::Ui, panel: &mut SensorsPanel, entry: &SensorEntry, objects: &ObjectsSettings, live: &ObjectsLive, actions: &mut Actions) {
    for track in &live.tracks {
        let state = if track.on { format!("presente ({})", track.count) } else { "ausente".into() };
        let score = if track.score > 0.0 { format!(" · {:.0}%", track.score * 100.0) } else { String::new() };
        ui.label(format!("{}: {state}{score}", objects.key_name(track.key)));
    }
    let comparing = if live.indexing { " · preparando comparação…" } else { "" };
    ui.weak(format!("{} caixas ensinadas{comparing}", live.taught));
    if !panel.teaching_boxes {
        ui.weak("Em Ensinar caixas, toque numa caixa para corrigi-la ou arraste em volta de algo que não foi achado.");
        return;
    }
    let Some(picked) = panel.picked else {
        ui.label("Toque numa caixa ou arraste para desenhar uma.");
        return;
    };
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

/// The boxes of a check over the frame they were found in, each with what it counts as.
fn paint_found(ui: &egui::Ui, rect: Rect, found: &[Found], objects: &ObjectsSettings, picked: Option<[f32; 4]>) {
    for f in found {
        let (color, name) = match f.verdict {
            Verdict::Filtered(_) => (FILTERED_COLOR, format!("{} (filtrado)", f.class.name())),
            Verdict::Unsure { label, .. } => (UNSURE_COLOR, format!("{}?", objects.key_name(ObjectKey::Own(label)))),
            Verdict::Taught { label: Some(label), .. } => (OWN_LABEL_COLOR, objects.key_name(ObjectKey::Own(label)).to_owned()),
            Verdict::Taught { .. } | Verdict::Detector => (BOX_COLOR, f.class.name().to_owned()),
        };
        let shown = screen(rect, f.area);
        let width = if picked == Some(f.area) { 4.0 } else { 2.0 };
        ui.painter().rect_stroke(shown, 0.0, Stroke::new(width, if width > 2.0 { Color32::WHITE } else { color }), StrokeKind::Middle);
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
