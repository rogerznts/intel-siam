use std::collections::HashMap;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, FontId, Rect, Sense, Stroke, StrokeKind, TextureHandle, TextureOptions, pos2, vec2};

use crate::cloud::{Account, AccountHandle, Stage};
use crate::frames::{StreamKey, StreamKind};
use crate::layout;
use crate::model::{CameraId, Intent, Layout, MAX_TILES, ViewMode};
use crate::plan::{Tools, TunnelBackend, Unavailable};
use crate::procs::{ChildKey, Supervisor};
use crate::status::{ExitReason, TileStatus, tile_status};

/// Greens sampled from the top and bottom of the app icon's tiles.
const BRAND_GREEN: Color32 = Color32::from_rgb(26, 250, 127);
const BRAND_GREEN_DIM: Color32 = Color32::from_rgb(2, 212, 101);

pub enum AccountAction {
    SignIn,
    Retry,
}

#[derive(Default)]
pub struct Actions {
    pub intents: Vec<Intent>,
    pub account: Option<AccountAction>,
    pub sensors: Vec<crate::sensors::UserCommand>,
}

/// Read-only view of everything the UI shows.
pub struct Scene<'a> {
    pub layout: &'a Layout,
    pub view: &'a ViewMode,
    pub account: &'a AccountHandle,
    pub tools: &'a Tools,
    pub procs: &'a Supervisor,
    pub now: Instant,
    pub sensors: &'a crate::sensors::SensorsView,
}

/// UI-owned state that survives frames but is never part of the domain.
#[derive(Default)]
pub struct UiState {
    pub textures: HashMap<StreamKey, TextureHandle>,
    pub show_picker: bool,
    pub sensors: crate::sensors_ui::SensorsPanel,
}

pub fn draw(ui: &mut egui::Ui, scene: &Scene, state: &mut UiState) -> Actions {
    let mut actions = Actions::default();
    upload_frames(ui.ctx(), scene, state);

    egui::Panel::top("toolbar").show(ui, |ui| {
        ui.horizontal(|ui| {
            let fill = if state.show_picker { BRAND_GREEN } else { BRAND_GREEN_DIM };
            let label = egui::RichText::new("Câmeras").color(Color32::BLACK).strong();
            if ui.add(egui::Button::new(label).fill(fill)).clicked() {
                state.show_picker = !state.show_picker;
            }
            if ui.selectable_label(state.sensors.open, "Sensores").clicked() {
                state.sensors.open = !state.sensors.open;
                if state.sensors.open { state.show_picker = false; }
            }
            ui.separator();
            ui.label(account_summary(scene.account.state()));
            if matches!(scene.account.state(), Account::SignedOut) && ui.button("Entrar").clicked() {
                actions.account = Some(AccountAction::SignIn);
            }
        });
    });
    if state.show_picker {
        egui::Panel::right("picker").resizable(false).default_size(240.0).show(ui, |ui| {
            picker(ui, scene, &mut actions);
        });
    }
    if state.sensors.open {
        egui::Panel::right("sensors").resizable(true).default_size(450.0).show(ui, |ui| {
            crate::sensors_ui::draw(ui, scene, &mut state.sensors, &mut actions);
        });
    }
    egui::CentralPanel::no_frame().frame(egui::Frame::NONE.fill(Color32::from_gray(12))).show(ui, |ui| {
        let area = ui.available_rect_before_wrap();
        match scene.view {
            ViewMode::Solo(cam) => solo(ui, area, cam, scene, state, &mut actions),
            ViewMode::Grid if scene.layout.tiles().is_empty() => {
                ui.painter().text(
                    area.center(),
                    Align2::CENTER_CENTER,
                    "Nenhuma câmera na grade. Escolha câmeras no painel Câmeras.",
                    FontId::proportional(16.0),
                    Color32::GRAY,
                );
            }
            ViewMode::Grid => grid(ui, area, scene, state, &mut actions),
        }
    });
    actions
}

fn upload_frames(ctx: &egui::Context, scene: &Scene, state: &mut UiState) {
    let mut wanted: Vec<StreamKey> =
        scene.layout.tiles().iter().map(|cam| StreamKey { camera: cam.clone(), kind: StreamKind::Sub }).collect();
    if let ViewMode::Solo(cam) = scene.view {
        wanted.push(StreamKey { camera: cam.clone(), kind: StreamKind::Main });
    }
    state.textures.retain(|key, _| wanted.contains(key));
    for key in wanted {
        let Some(image) = scene.procs.take_frame(&ChildKey::Decoder(key.clone())) else { continue };
        match state.textures.get_mut(&key) {
            Some(texture) => texture.set(image, TextureOptions::LINEAR),
            None => {
                let name = format!("{key:?}");
                state.textures.insert(key, ctx.load_texture(name, image, TextureOptions::LINEAR));
            }
        }
    }
}

fn grid(ui: &mut egui::Ui, area: Rect, scene: &Scene, state: &UiState, actions: &mut Actions) {
    let tiles = scene.layout.tiles();
    for (index, (rect, cam)) in layout::tile_rects(area, tiles.len()).into_iter().zip(tiles).enumerate() {
        let response = ui.interact(rect, egui::Id::unique(("tile", cam)), Sense::click_and_drag());
        response.dnd_set_drag_payload(index);
        let key = StreamKey { camera: cam.clone(), kind: StreamKind::Sub };
        let status = tile_status(cam, StreamKind::Sub, scene.account.state(), scene.tools, scene.procs, scene.now);
        let dragging_this = response.dragged();
        paint_tile(ui, rect.shrink(1.0), state.textures.get(&key), &status, &camera_name(scene, cam), dragging_this);
        paint_sensor_badges(ui, rect, cam, scene);

        if response.dnd_hover_payload::<usize>().is_some_and(|from| *from != index) {
            ui.painter().rect_stroke(rect.shrink(2.0), 4.0, Stroke::new(3.0, Color32::from_rgb(80, 160, 255)), StrokeKind::Inside);
        }
        if let Some(from) = response.dnd_release_payload::<usize>()
            && *from != index
        {
            actions.intents.push(Intent::Swap(*from, index));
        }
        if response.double_clicked() {
            actions.intents.push(Intent::ToggleSolo(cam.clone()));
        }
    }
}

fn solo(ui: &mut egui::Ui, area: Rect, cam: &CameraId, scene: &Scene, state: &UiState, actions: &mut Actions) {
    let rect = layout::tile_rects(area, 1)[0];
    let response = ui.interact(area, egui::Id::unique(("solo", cam)), Sense::click());
    let main = StreamKey { camera: cam.clone(), kind: StreamKind::Main };
    let sub = StreamKey { camera: cam.clone(), kind: StreamKind::Sub };
    // The Sub texture, upscaled, covers the gap until the first Main frame arrives.
    let (texture, kind) = match state.textures.get(&main) {
        Some(t) => (Some(t), StreamKind::Main),
        None => (state.textures.get(&sub), StreamKind::Sub),
    };
    let status = tile_status(cam, kind, scene.account.state(), scene.tools, scene.procs, scene.now);
    paint_tile(ui, rect, texture, &status, &camera_name(scene, cam), false);
    paint_sensor_badges(ui, rect, cam, scene);
    if response.double_clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        actions.intents.push(Intent::ToggleSolo(cam.clone()));
    }
}
fn paint_sensor_badges(ui: &egui::Ui, rect: Rect, cam: &CameraId, scene: &Scene) {
    let painter = ui.painter_at(rect);
    let ready = scene.sensors.link == crate::sensors::LinkState::Ready;
    let color = if ready { Color32::LIGHT_GREEN } else { Color32::GRAY };
    let mut top = rect.top() + 30.0;
    for entry in scene.sensors.on_camera(cam) {
        let text = format!("{}: {}", entry.settings.name.as_str(), crate::sensors_ui::value_label(entry, scene.sensors.people()));
        let galley = painter.layout(text, FontId::proportional(13.0), color, (rect.width() - 20.0).max(1.0));
        let badge = Rect::from_min_size(pos2(rect.right() - galley.size().x - 16.0, top), galley.size() + vec2(10.0, 4.0));
        painter.rect_filled(badge, 3.0, Color32::from_black_alpha(190));
        painter.galley(badge.min + vec2(5.0, 2.0), galley, color);
        top = badge.bottom() + 4.0;
    }
}


fn paint_tile(ui: &egui::Ui, rect: Rect, texture: Option<&TextureHandle>, status: &TileStatus, name: &str, dimmed: bool) {
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, Color32::from_gray(28));
    if let Some(texture) = texture {
        let tint = if dimmed { Color32::from_gray(110) } else { Color32::WHITE };
        painter.image(texture.id(), rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), tint);
    }
    let caption = FontId::proportional(13.0);
    let label = painter.layout_no_wrap(name.to_owned(), caption.clone(), Color32::WHITE);
    let label_rect = Rect::from_min_size(rect.left_top() + vec2(6.0, 6.0), label.size() + vec2(10.0, 4.0));
    painter.rect_filled(label_rect, 3.0, Color32::from_black_alpha(150));
    painter.galley(label_rect.min + vec2(5.0, 2.0), label, Color32::WHITE);

    if let Some(message) = status_message(status) {
        let font = FontId::proportional(15.0);
        let galley = painter.layout(message, font, Color32::WHITE, rect.width() - 40.0);
        let bubble = Rect::from_center_size(rect.center(), galley.size() + vec2(24.0, 14.0));
        painter.rect_filled(bubble, 6.0, Color32::from_black_alpha(180));
        painter.galley(bubble.min + vec2(12.0, 7.0), galley, Color32::WHITE);
    }
}

fn camera_name(scene: &Scene, cam: &CameraId) -> String {
    scene
        .account
        .devices()
        .and_then(|d| d.get(&cam.dvr))
        .and_then(|dvr| dvr.channels.iter().find(|(ch, _)| *ch == cam.channel))
        .map(|(_, name)| name.clone())
        .unwrap_or_else(|| format!("Canal {}", cam.channel.rtsp_channel()))
}

fn retry_suffix(retry_in: &Option<Duration>) -> String {
    match retry_in {
        Some(d) => format!("\nnova tentativa em {}s", d.as_secs_f32().ceil() as u64),
        None => String::new(),
    }
}

fn status_message(status: &TileStatus) -> Option<String> {
    Some(match status {
        TileStatus::Live => return None,
        TileStatus::SignedOut => "Entre na conta Intelbras para ver o vídeo".into(),
        TileStatus::LoadingAccount => "Carregando conta…".into(),
        TileStatus::AccountFailed => "Falha ao acessar a nuvem".into(),
        TileStatus::CameraMissing => "Câmera não encontrada na conta".into(),
        TileStatus::TunnelUnavailable(why) => unavailable_label(why),
        TileStatus::FfmpegMissing => "ffmpeg não encontrado".into(),
        TileStatus::TunnelStarting => "Abrindo túnel…".into(),
        TileStatus::TunnelDown { reason, retry_in } => format!("Túnel caiu: {}{}", reason.label(), retry_suffix(retry_in)),
        TileStatus::StreamStarting => "Conectando…".into(),
        TileStatus::StreamDown { reason: ExitReason::AuthRejected, .. } => {
            "O DVR recusou a senha (401).\nNão será tentado de novo nesta sessão.".into()
        }
        TileStatus::StreamDown { reason, retry_in } => format!("Sem vídeo: {}{}", reason.label(), retry_suffix(retry_in)),
    })
}

fn unavailable_label(why: &Unavailable) -> String {
    match why {
        Unavailable::NoBackend => "Túnel P2P indisponível neste sistema".into(),
        Unavailable::LibMissing(_) => "libt2u.so não encontrada (defina INTEL_SIAM_LIBT2U)".into(),
        Unavailable::MissingEnv(name) => format!("Defina a variável de ambiente {name}"),
        Unavailable::InstallFailed(_) => "Falha ao instalar o helper do túnel".into(),
    }
}

fn account_summary(account: &Account) -> String {
    match account {
        Account::SignedOut => "Desconectado".into(),
        Account::Working(Stage::Refreshing) => "Renovando login…".into(),
        Account::Working(Stage::AwaitingBrowser) => "Aguardando login no navegador…".into(),
        Account::Working(Stage::LoadingDevices) => "Carregando dispositivos…".into(),
        Account::Ready { devices } => format!("Conectado · {} DVR(s)", devices.len()),
        Account::Failed(e) => format!("Erro: {e}"),
    }
}

fn picker(ui: &mut egui::Ui, scene: &Scene, actions: &mut Actions) {
    ui.add_space(6.0);
    ui.heading("Câmeras");
    ui.add_space(4.0);
    match scene.account.state() {
        Account::SignedOut => {
            ui.label("Entre com a conta Intelbras para listar os DVRs.");
            if ui.button("Entrar").clicked() {
                actions.account = Some(AccountAction::SignIn);
            }
        }
        Account::Working(_) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(account_summary(scene.account.state()));
            });
        }
        Account::Failed(e) => {
            ui.colored_label(Color32::from_rgb(255, 140, 120), e.to_string());
            ui.horizontal(|ui| {
                if ui.button("Tentar de novo").clicked() {
                    actions.account = Some(AccountAction::Retry);
                }
                if ui.button("Entrar").clicked() {
                    actions.account = Some(AccountAction::SignIn);
                }
            });
        }
        Account::Ready { devices } => {
            let full = scene.layout.is_full();
            ui.label(format!("{}/{MAX_TILES} na grade", scene.layout.tiles().len()));
            for dvr in devices.values() {
                ui.separator();
                ui.strong(&dvr.name).on_hover_text(dvr.serial.as_str());
                for (channel, name) in &dvr.channels {
                    let cam = CameraId { dvr: dvr.serial.clone(), channel: *channel };
                    let mut checked = scene.layout.contains(&cam);
                    let enabled = checked || !full;
                    if ui.add_enabled(enabled, egui::Checkbox::new(&mut checked, name)).changed() {
                        actions.intents.push(if checked { Intent::AddCamera(cam) } else { Intent::RemoveCamera(cam) });
                    }
                }
            }
        }
    }
    if let TunnelBackend::Unavailable(why) = &scene.tools.tunnel {
        ui.separator();
        ui.colored_label(Color32::from_rgb(255, 190, 90), unavailable_label(why));
    }
    if scene.tools.ffmpeg.is_none() {
        ui.colored_label(Color32::from_rgb(255, 190, 90), "ffmpeg não encontrado (defina INTEL_SIAM_FFMPEG)");
    }
    ui.separator();
    ui.weak("Arraste um tile para trocar de posição. Duplo clique abre em tela cheia.");
}
