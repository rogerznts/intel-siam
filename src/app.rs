use std::path::PathBuf;
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
        let ctx = cc.egui_ctx.clone();
        Self {
            ui: UiState { show_picker: layout.tiles().is_empty(), ..UiState::default() },
            layout,
            view: ViewMode::Grid,
            account: AccountHandle::start(storage.join("secrets.json"), ctx.clone()),
            procs: Supervisor::new(tools.logs.clone(), ctx),
            tools,
        }
    }
}

impl eframe::App for ViewerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let now = Instant::now();
        self.account.poll();
        self.procs.poll(now);
        let scene = Scene {
            layout: &self.layout,
            view: &self.view,
            account: &self.account,
            tools: &self.tools,
            procs: &self.procs,
            now,
        };
        let actions = ui::draw(ui, &scene, &mut self.ui);
        for intent in actions.intents {
            reduce(&mut self.layout, &mut self.view, intent);
        }
        match actions.account {
            Some(AccountAction::SignIn) => self.account.sign_in(),
            Some(AccountAction::Retry) => self.account.retry(),
            None => {}
        }
        let desired =
            plan::desired_children(&self.layout, &self.view, self.account.devices(), &self.procs.tunnel_ports(), &self.tools);
        self.procs.reconcile(desired, now);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, LAYOUT_KEY, &SavedLayout::from(&self.layout));
    }

    fn on_exit(&mut self) {
        self.procs.shutdown(Duration::from_secs(1));
    }
}
