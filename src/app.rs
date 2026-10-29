use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::cloud::AccountHandle;
use crate::model::{LAYOUT_KEY, Layout, SavedLayout, ViewMode, reduce};
use crate::plan::{self, Tools};
use crate::procs::{Supervisor, discover_tools};
use crate::ui::{self, AccountAction, Scene, UiState};

pub const APP_ID: &str = "intel-siam";

/// Owns all state and is the only writer of the layout.
pub struct ViewerApp {
    layout: Layout,
    view: ViewMode,
    account: AccountHandle,
    tools: Tools,
    procs: Supervisor,
    sensors: crate::sensors::Sensors,
    ui: UiState,
}

impl ViewerApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let storage = eframe::storage_dir(APP_ID).unwrap_or_else(|| PathBuf::from(".intel-siam"));
        let tools = discover_tools(&storage);
        let layout = cc
            .storage
            .and_then(|s| eframe::get_value::<SavedLayout>(s, LAYOUT_KEY))
            .map(Layout::from_saved)
            .unwrap_or_default();
        cc.egui_ctx.all_styles_mut(ui::apply_theme);
        let ctx = cc.egui_ctx.clone();
        let wake: crate::procs::Waker = Arc::new({
            let ctx = ctx.clone();
            move || ctx.request_repaint()
        });
        Self {
            ui: UiState { show_picker: layout.tiles().is_empty(), ..UiState::default() },
            layout,
            view: ViewMode::Grid,
            account: AccountHandle::start(storage.join("secrets.json"), ctx.clone()),
            procs: Supervisor::new(tools.logs.clone(), Arc::clone(&wake)),
            sensors: crate::sensors::Sensors::new(wake),
            tools,
        }
    }
}

impl eframe::App for ViewerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let now = Instant::now();
        self.account.poll();
        if let Some(after) = self.procs.poll(now) {
            ui.ctx().request_repaint_after(after);
        }
        self.sensors.poll(&mut self.procs);
        let scene = Scene {
            layout: &self.layout,
            view: &self.view,
            account: &self.account,
            tools: &self.tools,
            procs: &self.procs,
            sensors: self.sensors.view(),
            now,
        };
        let actions = ui::draw(ui, &scene, &mut self.ui);
        for intent in actions.intents {
            reduce(&mut self.layout, &mut self.view, intent);
        }
        for command in actions.sensors { self.sensors.queue(command); }
        match actions.account {
            Some(AccountAction::SignIn) => self.account.sign_in(),
            Some(AccountAction::Retry) => self.account.retry(),
            None => {}
        }
        let ports = self.procs.tunnel_ports();
        let desired = plan::desired_children(
            &self.layout, &self.view, self.account.devices(), &ports, &self.tools,
            &self.sensors.view().wanted_feeds,
        );
        self.procs.reconcile(desired, now);
        self.sensors.sync(self.account.devices(), &ports, self.ui.sensors.interest());
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, LAYOUT_KEY, &SavedLayout::from(&self.layout));
    }

    fn on_exit(&mut self) {
        self.procs.shutdown(Duration::from_secs(1));
    }
}
