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
use crate::service::install::ServiceState;
use crate::ui::home::{render_home, HomeAction, HomeState};
use crate::ui::remote_view::{render_remote_view, RemoteViewAction, RemoteViewState};
use crate::ui::screen;
use crate::ui::settings::{render_settings, SettingsState};
use crate::ui::sysinfo::{spawn_status_poller, SharedSysStatus, SysStatus};
use crate::ui::theme::{self, c};
use crate::ui::tray::{self, TrayHandle};
use crate::update::{CheckOutcome, StagedArtifact, CORE_VERSION};
use base_common::{is_newer_version, InstallLayout, UpdatePhase, UpdateStatusSnapshot};
use eframe::egui::{
    self, Align, CornerRadius, Frame, Layout, Margin, RichText, Sense, Stroke, StrokeKind,
};
use protocol::auth::RelayNodeConfig;
use protocol::relay::OnlineDevice;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

enum GuiCheckEvent {
    Progress(UpdateStatusSnapshot),
    Finished(anyhow::Result<CheckOutcome>),
}

#[derive(Default)]
struct GuiUpdateState {
    checking: bool,
    manual_snap: Option<UpdateStatusSnapshot>,
    staged_for_gui: Option<StagedArtifact>,
    check_rx: Option<std::sync::mpsc::Receiver<GuiCheckEvent>>,
}

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
    gui_update: GuiUpdateState,
    remote_check_rx: mpsc::UnboundedReceiver<()>,
    restarting_for_update: bool,
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
        let sys = spawn_status_poller(Arc::clone(&cfg_arc), Arc::clone(&repaint));

        let active_sessions = Arc::new(AtomicU32::new(0));
        let (remote_check_tx, remote_check_rx) = mpsc::unbounded_channel();
        let relay_cmd_tx = spawn_relay_manager(
            cfg,
            caps.clone(),
            monitors.len() as u32,
            false,
            Arc::clone(&relay_status),
            repaint,
            Arc::clone(&active_sessions),
            Some(remote_check_tx),
        );
        // Always run the LAN direct listener: it self-defers to a healthy SYSTEM agent and
        // re-binds automatically when the agent dies, so LAN direct never silently disappears.
        spawn_direct_listener(Arc::clone(&cfg_arc), caps.clone(), false, active_sessions);

        let main_hwnd = tray::find_main_window_hwnd();
        let tray = TrayHandle::spawn(main_hwnd, cc.egui_ctx.clone());

        // Resume an already-installed service that was stopped when the user exited from the tray,
        // using non-elevated service control so startup never triggers a UAC prompt.
        let _ = std::thread::Builder::new()
            .name("flashdesk-svc-autostart".into())
            .spawn(|| {
                if crate::service::install::query_service_state() == ServiceState::Stopped {
                    let _ = crate::service::install::try_start_service_direct();
                }
            });

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
            gui_update: GuiUpdateState::default(),
            remote_check_rx,
            restarting_for_update: false,
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
        // Best-effort silent sync to ProgramData when writable; if UAC blocks it, the GUI
        // keeps hosting incoming sessions on its own until the user clicks "同步到服务".
        if std::path::Path::new(crate::service::install::SHARED_CONFIG_PATH).exists()
            && !snap.service_config_matches_disk()
        {
            let _ = crate::service::install::sync_config_file_to_program_data(&self.cfg_path);
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

    fn push_toast(&mut self, ctx: &egui::Context, tone: theme::Tone, msg: impl Into<String>) {
        let t = theme::Toast::new(ctx, tone, msg);
        self.home_state.toast = Some(t.clone());
        self.settings_state.toast = Some(t);
    }

    fn effective_update_snapshot<'a>(&'a self, sys: &'a SysStatus) -> Option<&'a UpdateStatusSnapshot> {
        match (&self.gui_update.manual_snap, &sys.update_status) {
            (Some(a), Some(b)) => Some(if a.updated_at >= b.updated_at { a } else { b }),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }

    fn poll_gui_update_events(&mut self, ctx: &egui::Context) {
        let mut finished = None;
        if let Some(rx) = &self.gui_update.check_rx {
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    GuiCheckEvent::Progress(snap) => {
                        self.gui_update.manual_snap = Some(snap);
                    }
                    GuiCheckEvent::Finished(res) => {
                        finished = Some(res);
                    }
                }
            }
        }
        if let Some(res) = finished {
            self.gui_update.checking = false;
            self.gui_update.check_rx = None;
            match res {
                Ok(out) => {
                    let msg = out.snapshot.message.clone();
                    let tone = match out.snapshot.state {
                        UpdatePhase::Error | UpdatePhase::NeedsBaseUpgrade { .. } => {
                            theme::Tone::Warn
                        }
                        UpdatePhase::StagedPendingIdle | UpdatePhase::SwitchedRestartGui => {
                            theme::Tone::Ok
                        }
                        _ => theme::Tone::Info,
                    };
                    self.gui_update.manual_snap = Some(out.snapshot);
                    if let Some(art) = out.staged_for_gui {
                        self.gui_update.staged_for_gui = Some(art);
                    }
                    if let Some(m) = msg {
                        self.push_toast(ctx, tone, m);
                    }
                }
                Err(e) => {
                    self.push_toast(ctx, theme::Tone::Danger, format!("检查更新失败：{e:#}"));
                }
            }
        }
    }

    fn trigger_check_update(&mut self, ctx: &egui::Context, sys: &SysStatus) {
        if sys.service == ServiceState::Running && sys.agent_ok {
            if crate::update::write_ipc_check_now().is_ok() {
                self.push_toast(ctx, theme::Tone::Info, "已通知后台服务立即检查更新…");
                return;
            }
        }
        if self.gui_update.checking {
            return;
        }
        let cfg_snap = self.cfg.lock().unwrap().clone();
        if cfg_snap.bundle.is_none() {
            self.push_toast(ctx, theme::Tone::Warn, "请先绑定设备身份后再检查更新");
            return;
        }
        self.gui_update.checking = true;
        let layout = sys.install_layout.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.gui_update.check_rx = Some(rx);
        let egui_ctx = ctx.clone();

        tokio::spawn(async move {
            let progress_tx = tx.clone();
            let progress_ctx = egui_ctx.clone();
            let res = crate::update::check_and_stage_update(&cfg_snap, &layout, false, move |snap| {
                let _ = progress_tx.send(GuiCheckEvent::Progress(snap.clone()));
                progress_ctx.request_repaint();
            })
            .await;
            let _ = tx.send(GuiCheckEvent::Finished(res));
            egui_ctx.request_repaint();
        });
    }

    fn restart_to_new_version(&mut self, ctx: &egui::Context, install_dir: &Path) {
        if let Some(art) = self.gui_update.staged_for_gui.clone() {
            if let Err(e) = crate::update::apply_staged_for_gui(install_dir, &art) {
                self.push_toast(ctx, theme::Tone::Danger, format!("应用更新失败：{e:#}"));
                return;
            }
            self.gui_update.staged_for_gui = None;
        }
        match crate::update::restart_via_launcher(install_dir) {
            Ok(()) => {
                self.restarting_for_update = true;
                if let Some((session, _)) = self.active_session.take() {
                    session.send(ViewerCommand::Disconnect);
                }
                self.tray.shutdown();
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Err(e) => {
                self.push_toast(ctx, theme::Tone::Danger, format!("重启失败：{e:#}"));
            }
        }
    }

    fn render_update_banner(&mut self, ctx: &egui::Context, sys: &SysStatus) {
        let managed_dir = match &sys.install_layout {
            InstallLayout::ManagedCore { install_dir, .. } => Some(install_dir.clone()),
            _ => None,
        };

        let ready_version = if managed_dir.is_some() {
            let from_state = sys
                .install_state
                .as_ref()
                .filter(|st| {
                    st.candidate.is_none() && is_newer_version(&st.active_version, CORE_VERSION)
                })
                .map(|st| st.active_version.clone());
            from_state.or_else(|| {
                self.gui_update
                    .staged_for_gui
                    .as_ref()
                    .map(|a| a.version.clone())
            })
        } else {
            None
        };

        let ready_notes = self
            .gui_update
            .staged_for_gui
            .as_ref()
            .map(|a| a.notes.clone())
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                sys.update_status
                    .as_ref()
                    .and_then(|u| u.notes.clone())
                    .filter(|s| !s.trim().is_empty())
            });

        let eff = self.effective_update_snapshot(sys).cloned();

        enum BannerKind {
            RestartReady(String, PathBuf, Option<String>),
            Warn(String),
            Progress(String),
        }

        let banner = if let (Some(ver), Some(dir)) = (ready_version, managed_dir) {
            Some(BannerKind::RestartReady(ver, dir, ready_notes))
        } else if let Some(snap) = eff {
            match &snap.state {
                UpdatePhase::NeedsBaseUpgrade { min_base_version } => Some(BannerKind::Warn(format!(
                    "发现新版本需要基座 >= v{min_base_version}，请运行新版 FlashDesk-Setup.exe 覆盖安装"
                ))),
                UpdatePhase::RolledBack { bad_version } => Some(BannerKind::Warn(format!(
                    "新版本 v{bad_version} 启动异常，基座已自动回滚至稳定版本"
                ))),
                UpdatePhase::Downloading => {
                    let pct = snap.progress_pct.unwrap_or(0);
                    Some(BannerKind::Progress(format!(
                        "正在下载更新 v{} ({pct}%)…",
                        snap.available_version.as_deref().unwrap_or("")
                    )))
                }
                _ => None,
            }
        } else {
            None
        };

        let Some(kind) = banner else {
            return;
        };

        let (fill, stroke) = match &kind {
            BannerKind::RestartReady(..) => (c::ACCENT_SOFT, c::ACCENT),
            BannerKind::Warn(_) => (egui::Color32::from_rgb(56, 42, 20), c::WARN),
            BannerKind::Progress(_) => (c::SURFACE_2, c::BORDER_STRONG),
        };

        egui::TopBottomPanel::top("update_banner")
            .frame(
                Frame::new()
                    .fill(fill)
                    .stroke(Stroke::new(1.0_f32, stroke))
                    .inner_margin(Margin::symmetric(20, 7)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let (text, hover) = match &kind {
                            BannerKind::RestartReady(ver, dir, notes) => {
                                if theme::small_button(ui, "立即重启更新", theme::Kind::Primary)
                                    .clicked()
                                {
                                    self.restart_to_new_version(ctx, dir);
                                }
                                (
                                    format!("新版本 v{ver} 已就绪，重启后立即生效"),
                                    notes.clone(),
                                )
                            }
                            BannerKind::Warn(msg) | BannerKind::Progress(msg) => {
                                (msg.clone(), None)
                            }
                        };
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            let resp =
                                ui.add(egui::Label::new(RichText::new(text).color(c::TEXT)).wrap());
                            if let Some(n) = hover {
                                resp.on_hover_text(format!("更新说明：{n}"));
                            }
                        });
                    });
                });
            });
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

        let sys = self.sys.lock().unwrap().clone();
        self.poll_gui_update_events(ctx);
        while self.remote_check_rx.try_recv().is_ok() {
            if !sys.agent_ok {
                self.trigger_check_update(ctx, &sys);
            }
        }

        self.top_bar(ctx);
        self.render_update_banner(ctx, &sys);

        // Support dragging a `.fdesk` / `.json` bundle directly onto the main window to bind.
        let (hovered_files, dropped_files) = ctx.input(|i| {
            (
                i.raw.hovered_files.clone(),
                i.raw.dropped_files.clone(),
            )
        });
        if hovered_files.iter().any(|f| {
            f.path.as_ref().is_some_and(|p| {
                matches!(
                    p.extension().and_then(|e| e.to_str()).map(|s| s.to_ascii_lowercase()).as_deref(),
                    Some("fdesk" | "json")
                )
            })
        }) {
            egui::Area::new(egui::Id::new("fdesk_drop_overlay"))
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .order(egui::Order::Foreground)
                .interactable(false)
                .show(ctx, |ui| {
                    Frame::new()
                        .fill(egui::Color32::from_rgba_unmultiplied(16, 44, 41, 242))
                        .stroke(Stroke::new(1.5_f32, c::ACCENT))
                        .corner_radius(CornerRadius::same(16))
                        .inner_margin(Margin::symmetric(32, 24))
                        .show(ui, |ui| {
                            ui.vertical_centered(|ui| {
                                ui.label(theme::title("松开鼠标，立即绑定此设备配置包"));
                                ui.label(theme::muted("配置将自动保存至 ~/.flashdesk/config.json"));
                            });
                        });
                });
        }
        if let Some(path) = dropped_files.into_iter().find_map(|f| f.path) {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s.to_ascii_lowercase());
            if matches!(ext.as_deref(), Some("fdesk" | "json")) {
                let res = std::fs::read_to_string(&path)
                    .map_err(anyhow::Error::from)
                    .and_then(|s| Ok(protocol::auth::DeviceBundle::from_any_str(&s)?));
                match res {
                    Ok(bundle) => {
                        self.cfg.lock().unwrap().import_bundle(bundle);
                        self.save_and_reload_relay();
                        let t = theme::Toast::new(
                            ctx,
                            theme::Tone::Ok,
                            "已通过拖拽绑定新配置包，正在连接中转…",
                        );
                        self.home_state.toast = Some(t.clone());
                        self.settings_state.toast = Some(t);
                    }
                    Err(e) => {
                        let t = theme::Toast::new(ctx, theme::Tone::Danger, format!("导入失败：{e}"));
                        self.home_state.toast = Some(t.clone());
                        self.settings_state.toast = Some(t);
                    }
                }
            }
        }

        let mut home_action = HomeAction::None;
        let mut settings_changed = false;
        let mut settings_check_update = false;
        let eff_snap = self.effective_update_snapshot(&sys).cloned();
        let update_checking = self.gui_update.checking;
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
                                    update_checking,
                                    eff_snap.as_ref(),
                                );
                            }
                            ActiveTab::Settings => {
                                let out = render_settings(
                                    ui,
                                    &mut self.settings_state,
                                    &self.cfg_path,
                                    &self.cfg,
                                    &self.relay_status,
                                    &self.relay_cmd_tx,
                                    &sys,
                                    update_checking,
                                    eff_snap.as_ref(),
                                );
                                settings_changed = out.config_changed;
                                settings_check_update = out.check_update;
                            }
                        });
                    });
            });

        if settings_changed {
            self.save_and_reload_relay();
        }
        if settings_check_update {
            self.trigger_check_update(ctx, &sys);
        }
        match home_action {
            HomeAction::ConfigChanged => self.save_and_reload_relay(),
            HomeAction::GoToSettings => self.active_tab = ActiveTab::Settings,
            HomeAction::ConnectRelay(dev) => self.connect_relay_device(ctx, dev),
            HomeAction::ConnectDirect(addr) => self.start_session(ctx, ConnectTarget::Direct(addr)),
            HomeAction::CheckUpdate => self.trigger_check_update(ctx, &sys),
            HomeAction::TriggerPeerUpdate(dev_id, dev_name) => {
                let _ = self
                    .relay_cmd_tx
                    .send(RelayCommand::RequestPeerUpdateCheck(dev_id));
                self.push_toast(
                    ctx,
                    theme::Tone::Info,
                    format!("已通知 {dev_name} 立即检查更新"),
                );
            }
            HomeAction::None => {}
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some((session, _)) = self.active_session.take() {
            session.send(ViewerCommand::Disconnect);
        }
        self.tray.shutdown();
        if !self.restarting_for_update {
            crate::service::install::stop_service_for_app_exit();
        }
    }
}
