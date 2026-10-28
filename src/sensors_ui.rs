use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use egui::{self, Color32, Pos2, Rect, Sense, Stroke, TextureHandle, TextureOptions, pos2, vec2};

use crate::frames::StreamKind;
use crate::model::CameraId;
use crate::procs::{ChildKey, ChildPhase};
use crate::sensors::*;
use crate::ui::{Actions, Scene};

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
    drag_start: Option<NormPoint>,
    delete_confirm: bool,
    images: HashMap<ImageId, TextureHandle>,
    error: Option<String>,
}

struct NewSensor {
    name: String,
    url: String,
    camera: Option<CameraId>,
    use_url: bool,
    stream: StreamKind,
    states: Vec<String>,
}

impl Default for NewSensor {
    fn default() -> Self {
        Self { name: String::new(), url: String::new(), camera: None, use_url: true, stream: StreamKind::Sub,
            states: vec!["Aberto".into(), "Fechado".into()] }
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
        self.delete_confirm = false;
        self.error = None;
    }
}

pub fn value_label(entry: &SensorEntry) -> String {
    match &entry.status {
        SensorStatus::Live => state_label(entry, entry.value.as_ref()),
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
    let KindSettings::States(states) = &entry.settings.kind;
    match value {
        Some(SensorValue::State(key)) => states.states.iter().find(|s| s.key == *key).map(|s| s.name.as_str()).unwrap_or("?").into(),
        _ => "?".into(),
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
            if ui.button("Novo sensor de Estado").clicked() { panel.create = Some(NewSensor::default()); }
            if let Some(mut form) = panel.create.take() {
                let mut close = false;
                ui.group(|ui| {
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
                    for (index, state) in form.states.iter_mut().enumerate() {
                        ui.horizontal(|ui| { ui.label(format!("Estado {}", index + 1)); ui.text_edit_singleline(state); });
                    }
                    ui.horizontal(|ui| {
                        if ui.add_enabled(form.states.len() < MAX_STATES, egui::Button::new("+ Estado")).clicked() { form.states.push(String::new()); }
                        if ui.add_enabled(form.states.len() > 1, egui::Button::new("− Estado")).clicked() { form.states.pop(); }
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Criar").clicked() {
                            let result = (|| -> Result<_, InvalidInput> {
                                let source = if form.use_url { SourceSpec::Url(form.url.clone().try_into()?) }
                                    else { SourceSpec::Dvr(form.camera.clone().ok_or(InvalidInput("escolha uma câmera"))?) };
                                let states = form.states.iter().enumerate().map(|(i, name)| Ok(StateDef { key: ((i + 1) as u8).try_into()?, name: name.clone().try_into()? })).collect::<Result<Vec<_>, InvalidInput>>()?.try_into()?;
                                Ok((source, Settings::new_states(form.name.clone().try_into()?, states)))
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
                if !close { panel.create = Some(form); }
            }
            if let Some(entry) = panel.selected.and_then(|id| scene.sensors.sensors.get(&id)) {
                selected(ui, scene, panel, entry, actions);
            }
            ui.separator();
            retention(ui, scene.sensors, actions);
        });
        if let Some(error) = &panel.error { ui.colored_label(Color32::LIGHT_RED, error); }
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
    ui.horizontal(|ui| {
        for (tab, label) in [(Tab::Live, "Ao vivo"), (Tab::Samples, "Amostras"), (Tab::Review, "Revisão"), (Tab::History, "Histórico")] {
            if ui.selectable_value(&mut panel.tab, tab, label).changed() { panel.opened = None; panel.before = None; panel.samples_before = None; panel.sample_pages.clear(); }
        }
    });
    let Some(focus) = scene.sensors.focus.as_ref().filter(|f| f.sensor == entry.id) else { ui.spinner(); return };
    match (&focus.detail, panel.tab) {
        (Detail::Live, Tab::Live) => live(ui, panel, entry, focus, actions),
        (Detail::Samples { rows: samples, before, older }, Tab::Samples) if *before == panel.samples_before => {
            if samples.is_empty() { ui.label("Nenhuma amostra. Ensine na aba Ao vivo."); }
            let KindSettings::States(states) = &entry.settings.kind;
            for sample in samples {
                ui.push_id(sample.id, |ui| {
                    ui.horizontal(|ui| {
                        image(ui, &panel.images, sample.thumb, 100.0);
                        ui.vertical(|ui| {
                            ui.weak(timestamp(sample.at));
                            let mut label = sample.label;
                            egui::ComboBox::from_id_salt("label").selected_text(state_label(entry, label.map(SensorValue::State).as_ref())).show_ui(ui, |ui| {
                                for state in states.states.iter() { ui.selectable_value(&mut label, Some(state.key), state.name.as_str()); }
                            });
                            if label != sample.label { actions.sensors.push(UserCommand::Relabel { id: entry.id, sample: sample.id, state: label }); }
                            if ui.button("Excluir amostra").clicked() { actions.sensors.push(UserCommand::Relabel { id: entry.id, sample: sample.id, state: None }); }
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
            let KindSettings::States(states) = &entry.settings.kind;
            for row in rows {
                ui.push_id(row.id, |ui| {
                    ui.separator();
                    ui.horizontal(|ui| {
                        if let Some(id) = row.thumb
                            && let Some(texture) = panel.images.get(&id)
                            && ui.add(egui::Image::new(texture).fit_to_exact_size(vec2(100.0, 56.25)).sense(Sense::click())).clicked()
                        { panel.opened = Some(row.id); }
                        ui.vertical(|ui| {
                            ui.label(format!("{} · {:.0}%", state_label(entry, Some(&row.value)), row.confidence * 100.0));
                            ui.weak(timestamp(row.at));
                            if let Some(reason) = row.review { ui.label(match reason { ReviewReason::LowConfidence => "Baixa confiança", ReviewReason::Flip => "Mudanças frequentes", ReviewReason::SpotCheck => "Conferência aleatória" }); }
                        });
                    });
                    if panel.tab == Tab::Review {
                        ui.horizontal_wrapped(|ui| {
                            for state in states.states.iter() {
                                if ui.button(format!("É {}", state.name.as_str())).clicked() { actions.sensors.push(UserCommand::Resolve { id: entry.id, item: row.id, state: Some(state.key) }); }
                            }
                            if ui.button("Descartar").clicked() { actions.sensors.push(UserCommand::Resolve { id: entry.id, item: row.id, state: None }); }
                        });
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
        image(ui, &panel.images, opened.image, ui.available_width());
        if ui.button("Fechar imagem").clicked() { panel.opened = None; }
    }
}

fn live(ui: &mut egui::Ui, panel: &mut SensorsPanel, entry: &SensorEntry, focus: &FocusView, actions: &mut Actions) {
    ui.horizontal(|ui| {
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
    if let Some(preview) = &focus.preview {
        if let Some(texture) = panel.images.get(&preview.image) {
            let width = ui.available_width();
            let size = texture.size_vec2() * (width / texture.size_vec2().x);
            let (rect, response) = ui.allocate_exact_size(size, Sense::click_and_drag());
            ui.painter().image(texture.id(), rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
            let normal = |p: Pos2| NormPoint::new(((p.x - rect.min.x) / rect.width()).clamp(0.0, 1.0), ((p.y - rect.min.y) / rect.height()).clamp(0.0, 1.0)).expect("clamped point");
            if panel.polygon_mode {
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
        }
        let KindSettings::States(states) = &entry.settings.kind;
        let KindLive::States(live) = &entry.live;
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
    } else { ui.label("Aguardando prévia da fonte…"); }
    if ui.button("Salvar região").clicked() {
        let mut settings = entry.settings.clone();
        if let Some(region) = panel.region.take() { settings.region = region; }
        actions.sensors.push(UserCommand::Configure { id: entry.id, settings });
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

fn retention(ui: &mut egui::Ui, view: &SensorsView, actions: &mut Actions) {
    ui.collapsing("Configurações e retenção", |ui| {
        if let Some(system) = &view.system {
            let mut days = system.retention.max_age.as_secs() / 86400;
            let mut mb = system.retention.max_bytes / (1024 * 1024);
            let mut changed = false;
            ui.horizontal(|ui| { changed |= ui.add(egui::DragValue::new(&mut days).range(1..=3650).suffix(" dias")).changed(); changed |= ui.add(egui::DragValue::new(&mut mb).range(1..=1048576).suffix(" MB")).changed(); });
            if changed { actions.sensors.push(UserCommand::SetRetention(Retention { max_age: Duration::from_secs(days * 86400), max_bytes: mb * 1024 * 1024 })); }
            ui.weak(format!("Histórico: {} MB · Amostras: {} MB (não expiram)", system.usage.history_bytes / 1048576, system.usage.sample_bytes / 1048576));
            if let Some(notice) = &system.notice { ui.colored_label(Color32::YELLOW, notice_label(&notice.kind)); }
        }
    });
}

fn notice_label(notice: &NoticeKind) -> String {
    match notice {
        NoticeKind::FrameExpired => "O quadro expirou. Ensine novamente sobre a prévia atual.".into(),
        NoticeKind::KindMismatch => "Não é possível trocar o tipo do sensor.".into(),
        NoticeKind::UnknownItem => "Item não encontrado.".into(),
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
