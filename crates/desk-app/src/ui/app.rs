use crate::capture::monitors::MonitorTarget;
use crate::codec::CodecCapabilities;
use crate::config::AppConfig;
use crate::net::direct::spawn_direct_listener;
use crate::net::relay_client::{spawn_relay_manager, RelayCommand};
use crate::net::viewer_session::{
    start_direct_viewer_session, start_relay_viewer_session, ReconnectTarget, ViewerCommand,
    ViewerSessionHandle,
};
use crate::net::RelayStatus;
use crate::ui::fonts::setup_fonts_and_style;
use crate::ui::home::{render_home, HomeAction, HomeState};
use crate::ui::remote_view::{render_remote_view, RemoteViewAction, RemoteViewState};
use crate::ui::settings::{render_settings, SettingsState};
use eframe::egui::{self, Color32, RichText};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

#[derive(PartialEq, Eq)]
enum ActiveTab {
    Home,
    Settings,
}

pub struct FlashDeskApp {
    cfg_path: PathBuf,
    cfg: Arc<Mutex<AppConfig>>,
    caps: CodecCapabilities,
    monitors: Vec<MonitorTarget>,
    relay_status: Arc<Mutex<RelayStatus>>,
    relay_cmd_tx: mpsc::UnboundedSender<RelayCommand>,
    active_tab: ActiveTab,
    home_state: HomeState,
    settings_state: SettingsState,
    active_session: Option<(ViewerSessionHandle, RemoteViewState)>,
}

impl FlashDeskApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        cfg_path: PathBuf,
        cfg: AppConfig,
        caps: CodecCapabilities,
        monitors: Vec<MonitorTarget>,
    ) -> Self {
        setup_fonts_and_style(&cc.egui_ctx);

        let cfg_arc = Arc::new(Mutex::new(cfg.clone()));
        let relay_status = Arc::new(Mutex::new(RelayStatus::default()));
        let egui_ctx = cc.egui_ctx.clone();
        let repaint: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            egui_ctx.request_repaint();
        });

        let relay_cmd_tx = spawn_relay_manager(
            cfg,
            caps.clone(),
            monitors.len() as u32,
            false,
            Arc::clone(&relay_status),
            repaint,
        );
        // Always run the LAN direct listener: it self-defers to a healthy SYSTEM agent and
        // re-binds automatically when the agent dies, so LAN direct never silently disappears.
        spawn_direct_listener(Arc::clone(&cfg_arc), caps.clone(), false);

        Self {
            cfg_path,
            cfg: cfg_arc,
            caps,
            monitors,
            relay_status,
            relay_cmd_tx,
            active_tab: ActiveTab::Home,
            home_state: HomeState::default(),
            settings_state: SettingsState::default(),
            active_session: None,
        }
    }

    fn save_and_reload_relay(&self) {
        let snap = self.cfg.lock().unwrap().clone();
        if let Err(e) = snap.save(&self.cfg_path) {
            tracing::error!("failed to save config: {e}");
        }
        let _ = self.relay_cmd_tx.send(RelayCommand::ReloadConfig(snap));
    }
}

impl eframe::App for FlashDeskApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some((session, view_state)) = &mut self.active_session {
            match render_remote_view(ctx, view_state, session) {
                RemoteViewAction::Disconnect => {
                    self.active_session = None;
                }
                RemoteViewAction::SwitchControlRelay(new_node) => {
                    if let ReconnectTarget::Relay {
                        target_device_id,
                        target_device_name,
                    } = session.reconnect_target.clone()
                    {
                        session.send(ViewerCommand::Disconnect);
                        let mut cfg_guard = self.cfg.lock().unwrap();
                        cfg_guard.selected_relay_id = Some(new_node.id.clone());
                        let snap = cfg_guard.clone();
                        drop(cfg_guard);
                        self.save_and_reload_relay();

                        if let Some(bundle) = snap.bundle.clone() {
                            let pref_file = snap.preferred_file_relay().cloned();
                            let pref_file_id = pref_file.as_ref().map(|r| r.id.clone());
                            let egui_ctx = ctx.clone();
                            let repaint = Arc::new(move || egui_ctx.request_repaint());
                            let handle = start_relay_viewer_session(
                                bundle,
                                snap.revoked_device_ids,
                                new_node,
                                pref_file,
                                target_device_id,
                                target_device_name,
                                self.caps.clone(),
                                snap.default_fps,
                                snap.default_bitrate_kbps,
                                repaint,
                            );
                            self.active_session = Some((
                                handle,
                                RemoteViewState::new(
                                    snap.default_fps,
                                    snap.default_bitrate_kbps,
                                    pref_file_id,
                                ),
                            ));
                        }
                    }
                }
                RemoteViewAction::None => {}
            }
            return;
        }

        egui::TopBottomPanel::top("top_nav")
            .frame(
                egui::Frame::NONE
                    .fill(Color32::from_rgb(13, 16, 22))
                    .inner_margin(egui::Margin::symmetric(14, 10)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("⚡ FlashDesk")
                            .strong()
                            .size(19.0)
                            .color(Color32::from_rgb(80, 235, 190)),
                    );
                    ui.label(
                        RichText::new("自建中转 · 不限速 60fps 远控")
                            .color(Color32::GRAY),
                    );
                    ui.add_space(16.0);
                    if ui
                        .selectable_label(self.active_tab == ActiveTab::Home, "🖥 我的设备")
                        .clicked()
                    {
                        self.active_tab = ActiveTab::Home;
                    }
                    if ui
                        .selectable_label(self.active_tab == ActiveTab::Settings, "⚙ 设置与密钥")
                        .clicked()
                    {
                        self.active_tab = ActiveTab::Settings;
                    }
                });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| match self.active_tab {
                ActiveTab::Home => {
                    let act = render_home(
                        ui,
                        &mut self.home_state,
                        &self.cfg_path,
                        &self.cfg,
                        &self.relay_status,
                        &self.relay_cmd_tx,
                        &self.caps,
                        &self.monitors,
                    );
                    match act {
                        HomeAction::ConfigChanged => {
                            self.save_and_reload_relay();
                        }
                        HomeAction::GoToSettings => {
                            self.active_tab = ActiveTab::Settings;
                        }
                        HomeAction::ConnectRelay(dev) => {
                            let snap = self.cfg.lock().unwrap().clone();
                            if let (Some(bundle), Some(relay)) =
                                (snap.bundle.clone(), snap.active_relay().cloned())
                            {
                                let pref_file = snap.preferred_file_relay().cloned();
                                let pref_file_id = pref_file.as_ref().map(|r| r.id.clone());
                                let egui_ctx = ctx.clone();
                                let repaint = Arc::new(move || egui_ctx.request_repaint());
                                let handle = start_relay_viewer_session(
                                    bundle,
                                    snap.revoked_device_ids,
                                    relay,
                                    pref_file,
                                    dev.device_id,
                                    dev.device_name,
                                    self.caps.clone(),
                                    snap.default_fps,
                                    snap.default_bitrate_kbps,
                                    repaint,
                                );
                                self.active_session = Some((
                                    handle,
                                    RemoteViewState::new(
                                        snap.default_fps,
                                        snap.default_bitrate_kbps,
                                        pref_file_id,
                                    ),
                                ));
                            }
                        }
                        HomeAction::ConnectDirect(addr) => {
                            let snap = self.cfg.lock().unwrap().clone();
                            if let Some(bundle) = snap.bundle.clone() {
                                let egui_ctx = ctx.clone();
                                let repaint = Arc::new(move || egui_ctx.request_repaint());
                                let handle = start_direct_viewer_session(
                                    bundle,
                                    snap.revoked_device_ids,
                                    addr,
                                    self.caps.clone(),
                                    snap.default_fps,
                                    snap.default_bitrate_kbps,
                                    repaint,
                                );
                                self.active_session = Some((
                                    handle,
                                    RemoteViewState::new(
                                        snap.default_fps,
                                        snap.default_bitrate_kbps,
                                        None,
                                    ),
                                ));
                            }
                        }
                        HomeAction::None => {}
                    }
                }
                ActiveTab::Settings => {
                    if render_settings(ui, &mut self.settings_state, &self.cfg_path, &self.cfg) {
                        self.save_and_reload_relay();
                    }
                }
            });
        });
    }
}
