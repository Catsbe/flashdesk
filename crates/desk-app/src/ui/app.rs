use crate::capture::monitors::MonitorTarget;
use crate::codec::CodecCapabilities;
use crate::config::{res_cap_for_height, AppConfig};
use crate::net::direct::spawn_direct_listener;
use crate::net::relay_client::{spawn_relay_manager, RelayCommand};
use crate::net::viewer_session::{
    start_direct_viewer_session, start_relay_viewer_session, ReconnectTarget, ViewerCommand,
    ViewerSessionHandle,
};
use crate::net::RelayStatus;
use crate::ui::home::{render_home, HomeAction, HomeState};
use crate::ui::remote_view::{render_remote_view, RemoteViewAction, RemoteViewState};
use crate::ui::screen;
use crate::ui::settings::{render_settings, SettingsState};
use crate::ui::sysinfo::{spawn_status_poller, SharedSysStatus};
use crate::ui::theme::{self, c};
use crate::ui::tray::{self, TrayHandle};
use eframe::egui::{
    self, Align, CornerRadius, Frame, Layout, Margin, RichText, Sense, Stroke, StrokeKind,
};
use protocol::auth::RelayNodeConfig;
use protocol::relay::OnlineDevice;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

#[derive(PartialEq, Eq)]
enum ActiveTab {
    Home,
    Settings,
}

enum ConnectTarget {
    Relay {
        node: RelayNodeConfig,
        device_id: uuid::Uuid,
        device_name: String,
    },
    Direct(String),
}

pub struct FlashDeskApp {
    cfg_path: PathBuf,
    cfg: Arc<Mutex<AppConfig>>,
    caps: CodecCapabilities,
    monitors: Vec<MonitorTarget>,
    relay_status: Arc<Mutex<RelayStatus>>,
    relay_cmd_tx: mpsc::UnboundedSender<RelayCommand>,
    sys: SharedSysStatus,
    active_tab: ActiveTab,
    home_state: HomeState,
    settings_state: SettingsState,
    active_session: Option<(ViewerSessionHandle, RemoteViewState)>,
    /// The window has been checked against the monitor's work area (see `ui::screen`).
    window_fitted: bool,
    tray: TrayHandle,
    main_hwnd_resolved: bool,
}

impl FlashDeskApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        cfg_path: PathBuf,
        cfg: AppConfig,
        caps: CodecCapabilities,
        monitors: Vec<MonitorTarget>,
    ) -> Self {
        theme::install(&cc.egui_ctx);

        let cfg_arc = Arc::new(Mutex::new(cfg.clone()));
        let relay_status = Arc::new(Mutex::new(RelayStatus::default()));
        let egui_ctx = cc.egui_ctx.clone();
        let repaint: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            egui_ctx.request_repaint();
        });
        let sys = spawn_status_poller(Arc::clone(&repaint));

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

        let main_hwnd = tray::find_main_window_hwnd();
        let tray = TrayHandle::spawn(main_hwnd, cc.egui_ctx.clone());

        Self {
            cfg_path,
            cfg: cfg_arc,
            caps,
            monitors,
            relay_status,
            relay_cmd_tx,
            sys,
            active_tab: ActiveTab::Home,
            home_state: HomeState::default(),
            settings_state: SettingsState::default(),
            active_session: None,
            window_fitted: false,
            tray,
            main_hwnd_resolved: main_hwnd != 0,
        }
    }

    /// Shrinks / moves the window once at startup so it fits the work area of its monitor
    /// (eframe only clamps to the whole monitor, ignoring the taskbar and title bar).
    fn fit_window_to_screen(&mut self, ctx: &egui::Context) {
        if self.window_fitted {
            return;
        }
        let (outer, inner) = ctx.input(|i| (i.viewport().outer_rect, i.viewport().inner_rect));
        let (Some(outer), Some(inner)) = (outer, inner) else {
            return; // not reported yet: try again next frame
        };
        self.window_fitted = true;
        let Some(work) = screen::work_area_points(outer.center(), ctx.pixels_per_point()) else {
            return;
        };
        let fit = screen::fit_window(outer, inner, work, screen::MIN_INNER);
        ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(fit.min_inner_size));
        if let Some(size) = fit.inner_size {
            tracing::info!(
                "window {:?} does not fit work area {:?}: resizing to {:?}",
                inner.size(),
                work,
                size
            );
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
        }
        if let Some(pos) = fit.outer_pos {
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
        }
    }

    fn save_and_reload_relay(&self) {
        let snap = self.cfg.lock().unwrap().clone();
        if let Err(e) = snap.save(&self.cfg_path) {
            tracing::error!("failed to save config: {e}");
        }
        let _ = self.relay_cmd_tx.send(RelayCommand::ReloadConfig(snap));
    }

    fn start_session(&mut self, ctx: &egui::Context, target: ConnectTarget) {
        let snap = self.cfg.lock().unwrap().clone();
        let Some(bundle) = snap.bundle.clone() else {
            return;
        };
        let egui_ctx = ctx.clone();
        let repaint = Arc::new(move || egui_ctx.request_repaint());
        let res_cap = res_cap_for_height(snap.stream_max_height);
        let (handle, file_relay_id) = match target {
            ConnectTarget::Relay {
                node,
                device_id,
                device_name,
            } => {
                let pref_file = snap.preferred_file_relay().cloned();
                let pref_file_id = pref_file.as_ref().map(|r| r.id.clone());
                (
                    start_relay_viewer_session(
                        bundle,
                        snap.revoked_device_ids.clone(),
                        node,
                        pref_file,
                        device_id,
                        device_name,
                        self.caps.clone(),
                        snap.default_fps,
                        snap.default_bitrate_kbps,
                        res_cap,
                        repaint,
                    ),
                    pref_file_id,
                )
            }
            ConnectTarget::Direct(addr) => (
                start_direct_viewer_session(
                    bundle,
                    snap.revoked_device_ids.clone(),
                    addr,
                    self.caps.clone(),
                    snap.default_fps,
                    snap.default_bitrate_kbps,
                    res_cap,
                    repaint,
                ),
                None,
            ),
        };
        self.active_session = Some((
            handle,
            RemoteViewState::new(
                snap.default_fps,
                snap.default_bitrate_kbps,
                snap.stream_max_height,
                file_relay_id,
            ),
        ));
    }

    fn connect_relay_device(&mut self, ctx: &egui::Context, dev: OnlineDevice) {
        let node = self.cfg.lock().unwrap().active_relay().cloned();
        if let Some(node) = node {
            self.start_session(
                ctx,
                ConnectTarget::Relay {
                    node,
                    device_id: dev.device_id,
                    device_name: dev.device_name,
                },
            );
        }
    }

    fn top_bar(&mut self, ctx: &egui::Context) {
        let st = self.relay_status.lock().unwrap().clone();
        let has_identity = self.cfg.lock().unwrap().bundle.is_some();
        egui::TopBottomPanel::top("top_nav")
            .frame(
                Frame::new()
                    .fill(c::SURFACE)
                    .stroke(Stroke::new(1.0_f32, c::BORDER))
                    .inner_margin(Margin::symmetric(20, 8)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("⚡").size(20.0).color(c::ACCENT));
                    ui.label(theme::bold("FlashDesk").size(18.0));
                    ui.add_space(20.0);
                    if theme::nav_tab(ui, "我的设备", self.active_tab == ActiveTab::Home).clicked() {
                        self.active_tab = ActiveTab::Home;
                    }
                    if theme::nav_tab(ui, "设置", self.active_tab == ActiveTab::Settings).clicked()
                    {
                        self.active_tab = ActiveTab::Settings;
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        relay_badge(ui, &st, has_identity);
                    });
                });
            });
    }
}

/// Compact relay connection indicator for the top bar.
fn relay_badge(ui: &mut egui::Ui, st: &RelayStatus, has_identity: bool) {
    let (dot, text) = if !has_identity {
        (c::TEXT_3, "尚未初始化身份".to_string())
    } else if st.connected {
        let rtt = st.rtt_ms.map(|ms| format!(" · {ms} ms")).unwrap_or_default();
        (c::OK, format!("{}{rtt}", st.relay_name))
    } else if st.error.is_some() {
        (c::DANGER, "中转未连接".to_string())
    } else {
        (c::WARN, "正在连接中转…".to_string())
    };
    // Painted at an exact size: it sits in the top bar's right-to-left layout, where child
    // layouts would either reverse (dot after text) or stretch across the whole bar. Long relay
    // names are truncated so the badge never pushes into the tabs on narrow windows.
    let max_text_w = (ui.available_width() - 44.0).clamp(40.0, 260.0);
    let galley = egui::WidgetText::from(RichText::new(text.clone()).size(13.0).color(c::TEXT_2))
        .into_galley(
            ui,
            Some(egui::TextWrapMode::Truncate),
            max_text_w,
            egui::TextStyle::Body,
        );
    let truncated = galley.elided;
    let size = egui::vec2(12.0 + 10.0 + 8.0 + galley.size().x + 14.0, 28.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();
    painter.rect(
        rect,
        CornerRadius::same(255),
        c::SURFACE_2,
        Stroke::new(1.0_f32, c::BORDER),
        StrokeKind::Inside,
    );
    let dot_pos = egui::pos2(rect.left() + 17.0, rect.center().y);
    painter.circle_filled(dot_pos, 7.0, dot.gamma_multiply(0.18));
    painter.circle_filled(dot_pos, 4.5, dot);
    let text_pos = egui::pos2(rect.left() + 30.0, rect.center().y - galley.size().y / 2.0);
    painter.galley(text_pos, galley, c::TEXT_2);
    if let Some(err) = st.error.as_deref().filter(|_| !st.connected) {
        resp.on_hover_text(err);
    } else if truncated {
        resp.on_hover_text(text);
    }
}

/// Centred content column with a max width and comfortable side margins, instead of
/// stretching every row from window edge to window edge.
fn content_column(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let avail = ui.available_width();
    let width = (avail - 56.0).clamp(300.0, theme::CONTENT_MAX_WIDTH).min(avail);
    let side = ((avail - width) / 2.0).max(0.0);
    ui.add_space(22.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.add_space(side);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
            ui.set_width(width);
            add(ui);
        });
    });
    ui.add_space(36.0);
}

impl eframe::App for FlashDeskApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.main_hwnd_resolved {
            let h = tray::find_main_window_hwnd();
            if h != 0 {
                self.tray.set_main_hwnd(h);
                self.main_hwnd_resolved = true;
            }
        }
        if self.tray.quit_requested() {
            if let Some((session, _)) = self.active_session.take() {
                session.send(ViewerCommand::Disconnect);
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if ctx.input(|i| i.viewport().close_requested()) && self.tray.is_active() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if let Some((_, view_state)) = &mut self.active_session {
                view_state.exit_fullscreen(ctx);
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            return;
        }

        self.fit_window_to_screen(ctx);

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
                        self.cfg.lock().unwrap().selected_relay_id = Some(new_node.id.clone());
                        self.save_and_reload_relay();
                        self.start_session(
                            ctx,
                            ConnectTarget::Relay {
                                node: new_node,
                                device_id: target_device_id,
                                device_name: target_device_name,
                            },
                        );
                    }
                }
                RemoteViewAction::None => {}
            }
            return;
        }

        self.top_bar(ctx);

        let sys = self.sys.lock().unwrap().clone();
        let mut home_action = HomeAction::None;
        let mut settings_changed = false;
        egui::CentralPanel::default()
            .frame(Frame::new().fill(c::BG))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        content_column(ui, |ui| match self.active_tab {
                            ActiveTab::Home => {
                                home_action = render_home(
                                    ui,
                                    &mut self.home_state,
                                    &self.cfg_path,
                                    &self.cfg,
                                    &self.relay_status,
                                    &self.relay_cmd_tx,
                                    &self.caps,
                                    &self.monitors,
                                    &sys,
                                );
                            }
                            ActiveTab::Settings => {
                                settings_changed = render_settings(
                                    ui,
                                    &mut self.settings_state,
                                    &self.cfg_path,
                                    &self.cfg,
                                    &sys,
                                );
                            }
                        });
                    });
            });

        if settings_changed {
            self.save_and_reload_relay();
        }
        match home_action {
            HomeAction::ConfigChanged => self.save_and_reload_relay(),
            HomeAction::GoToSettings => self.active_tab = ActiveTab::Settings,
            HomeAction::ConnectRelay(dev) => self.connect_relay_device(ctx, dev),
            HomeAction::ConnectDirect(addr) => self.start_session(ctx, ConnectTarget::Direct(addr)),
            HomeAction::None => {}
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some((session, _)) = self.active_session.take() {
            session.send(ViewerCommand::Disconnect);
        }
        self.tray.shutdown();
    }
}
