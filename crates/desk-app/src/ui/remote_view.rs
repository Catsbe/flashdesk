use crate::net::viewer_session::{HudStats, ReconnectTarget, ViewerCommand, ViewerSessionHandle};
use crate::transfer::client_manager::FileCommand;
use crate::transfer::{list_local_dir, TransferDirection, TransferState};
use crate::ui::theme::{self, c, Kind};
use crate::vdd::{MTT_VDD_DOWNLOAD_URL, PARSEC_VDD_DOWNLOAD_URL};
use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, ColorImage, CornerRadius, CursorIcon, Event, Frame,
    Key, Layout, Margin, Modifiers, PointerButton, Rect, RichText, Sense, Shadow, Stroke,
    TextureHandle, TextureId, TextureOptions, Vec2, ViewportCommand,
};
use protocol::auth::RelayNodeConfig;
use protocol::session::{InputEvent, MonitorInfo, MouseBtn, VddRequest};
use protocol::transfer::FileEntry;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyState, MapVirtualKeyW, MAPVK_VK_TO_VSC,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetForegroundWindow, GetWindowThreadProcessId, SetWindowsHookExW,
    UnhookWindowsHookEx, HHOOK, MSG, PM_REMOVE, WH_GETMESSAGE, WM_KEYDOWN, WM_KEYUP,
    WM_SYSKEYDOWN, WM_SYSKEYUP,
};

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum ScaleMode {
    Fit,
    Original,
}

pub enum RemoteViewAction {
    None,
    Disconnect,
    SwitchControlRelay(RelayNodeConfig),
}

pub struct RemoteViewState {
    texture: Option<TextureHandle>,
    tex_dims: (u32, u32),
    scale_mode: ScaleMode,
    fullscreen: bool,
    toolbar_collapsed: bool,
    show_stats: bool,
    bitrate_mbps: f32,
    target_fps: u32,
    /// Resolution cap by height (0 = native, 1080, 720).
    res_cap_h: u32,
    last_norm_pos: Option<(f32, f32)>,
    keys: KeyForwarder,
    special_key_hook: Option<SpecialKeyHookGuard>,

    // M2 + M3 interactive overlays
    clipboard_enabled: bool,
    show_file_panel: bool,
    file_panel_initialized: bool,
    show_vdd_window: bool,
    local_dir: String,
    local_dir_input: String,
    local_entries: Vec<FileEntry>,
    local_roots: Vec<FileEntry>,
    local_error: Option<String>,
    remote_dir_input: String,
    last_synced_remote_dir: String,
    selected_file_relay_id: Option<String>,
    drop_notice: Option<String>,
}

impl RemoteViewState {
    pub fn new(
        initial_fps: u32,
        initial_bitrate_kbps: u32,
        initial_res_cap_h: u32,
        initial_file_relay_id: Option<String>,
    ) -> Self {
        Self {
            texture: None,
            tex_dims: (0, 0),
            scale_mode: ScaleMode::Fit,
            fullscreen: false,
            toolbar_collapsed: false,
            show_stats: false,
            bitrate_mbps: (initial_bitrate_kbps as f32 / 1000.0).clamp(1.0, 50.0),
            target_fps: initial_fps,
            res_cap_h: initial_res_cap_h,
            last_norm_pos: None,
            keys: KeyForwarder::default(),
            special_key_hook: None,
            clipboard_enabled: true,
            show_file_panel: false,
            file_panel_initialized: false,
            show_vdd_window: false,
            local_dir: String::new(),
            local_dir_input: String::new(),
            local_entries: Vec::new(),
            local_roots: Vec::new(),
            local_error: None,
            remote_dir_input: String::new(),
            last_synced_remote_dir: String::new(),
            selected_file_relay_id: initial_file_relay_id,
            drop_notice: None,
        }
    }

    fn refresh_local_dir(&mut self) {
        let cur = self.local_dir.clone();
        self.navigate_local_dir(cur);
    }

    fn navigate_local_dir(&mut self, target: String) {
        match list_local_dir(&target) {
            Ok(list) => {
                if target.trim().is_empty() {
                    self.local_roots = list.clone();
                }
                self.local_dir = target.clone();
                self.local_dir_input = target;
                self.local_entries = list;
                self.local_error = None;
            }
            Err(e) => {
                self.local_dir_input = target;
                self.local_error = Some(e.to_string());
            }
        }
    }

    fn toggle_file_panel(&mut self, session: &ViewerSessionHandle) {
        self.show_file_panel = !self.show_file_panel;
        if self.show_file_panel && !self.file_panel_initialized {
            self.refresh_local_dir();
            session
                .file_client
                .send(FileCommand::ListRemoteDir(String::new()));
            self.file_panel_initialized = true;
        }
    }

    pub fn exit_fullscreen(&mut self, ctx: &egui::Context) {
        if self.fullscreen {
            self.fullscreen = false;
            ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
        }
    }
}

pub fn render_remote_view(
    ctx: &egui::Context,
    state: &mut RemoteViewState,
    session: &ViewerSessionHandle,
) -> RemoteViewAction {
    let mut action = RemoteViewAction::None;
    let mut go_back = false;

    if state.special_key_hook.is_none() {
        state.special_key_hook = SpecialKeyHookGuard::install_for_current_thread();
    }
    // Always drain once per frame so keys typed while disconnected, waiting, overlaid, or
    // unfocused are discarded immediately instead of replaying once video resumes.
    let hook_specials = take_captured_special_keys();
    let hook_lock_msgs = take_captured_lock_key_msgs();

    if let Some(frame) = session.latest_frame.lock().unwrap().take() {
        // Already converted to egui's pixel format on the decode thread: just upload.
        let image: ColorImage = frame.image;
        match &mut state.texture {
            Some(tex) if state.tex_dims == (frame.width, frame.height) => {
                tex.set(image, TextureOptions::LINEAR);
            }
            _ => {
                state.texture = Some(ctx.load_texture(
                    "remote_desktop_frame",
                    image,
                    TextureOptions::LINEAR,
                ));
                state.tex_dims = (frame.width, frame.height);
            }
        }
    }

    if ctx.input(|i| i.key_pressed(Key::F11)) {
        state.fullscreen = !state.fullscreen;
        ctx.send_viewport_cmd(ViewportCommand::Fullscreen(state.fullscreen));
    }

    let hud = session.hud.lock().unwrap().clone();
    let disconnect_reason = session.disconnected_reason.lock().unwrap().clone();
    let monitors = session.remote_monitors.lock().unwrap().clone();
    let mut active_mon = *session.active_monitor.lock().unwrap();

    // Central video panel
    egui::CentralPanel::default()
        .frame(Frame::NONE.fill(Color32::BLACK))
        .show(ctx, |ui| {
            let avail = ui.available_rect_before_wrap();
            let Some(tex_id) = state.texture.as_ref().map(|t| t.id()) else {
                waiting_placeholder(ui, avail, &hud, session);
                return;
            };
            let (fw, fh) = (state.tex_dims.0.max(1) as f32, state.tex_dims.1.max(1) as f32);
            let disp_size = match state.scale_mode {
                ScaleMode::Fit => {
                    let scale = (avail.width() / fw).min(avail.height() / fh);
                    vec2(fw * scale, fh * scale)
                }
                ScaleMode::Original => {
                    let ppp = ui.ctx().pixels_per_point();
                    vec2(fw / ppp, fh / ppp)
                }
            };
            let alive = disconnect_reason.is_none();

            if disp_size.x <= avail.width() + 0.5 && disp_size.y <= avail.height() + 0.5 {
                let video_rect = Rect::from_center_size(avail.center(), disp_size);
                let resp = ui.allocate_rect(video_rect, Sense::click_and_drag());
                present_video(
                    ctx,
                    ui,
                    state,
                    session,
                    tex_id,
                    video_rect,
                    &resp,
                    alive,
                    &hook_specials,
                    &hook_lock_msgs,
                );
            } else {
                // 1:1 on a screen smaller than the remote desktop: pan with the scroll bars.
                // The mouse wheel keeps going to the remote PC, so it must not also scroll here.
                let overlays_open = state.show_file_panel || state.show_vdd_window;
                if alive && !overlays_open && ui.rect_contains_pointer(avail) {
                    ctx.input_mut(|i| i.smooth_scroll_delta = Vec2::ZERO);
                }
                // Always-visible bars (the app's default floating bars only show on hover), so
                // it is obvious the picture can be panned.
                ui.style_mut().spacing.scroll = egui::style::ScrollStyle::solid();
                egui::ScrollArea::both()
                    .id_salt("remote_video_pan")
                    .auto_shrink([false, false])
                    .drag_to_scroll(false)
                    .show(ui, |ui| {
                        // Still centred along an axis that fits.
                        let pad = ((ui.available_size() - disp_size) / 2.0).max(Vec2::ZERO);
                        let (outer, _) =
                            ui.allocate_exact_size(disp_size + pad * 2.0, Sense::hover());
                        let video_rect = Rect::from_min_size(outer.min + pad, disp_size);
                        let resp = ui.interact(
                            video_rect,
                            ui.id().with("remote_video"),
                            Sense::click_and_drag(),
                        );
                        present_video(
                            ctx,
                            ui,
                            state,
                            session,
                            tex_id,
                            video_rect,
                            &resp,
                            alive,
                            &hook_specials,
                            &hook_lock_msgs,
                        );
                    });
            }
        });

    // Drag-and-drop file upload detection
    let (hovered_count, dropped_files) = ctx.input(|i| {
        (
            i.raw.hovered_files.len(),
            i.raw.dropped_files.clone(),
        )
    });
    if hovered_count > 0 {
        let via = session.file_client.active_via_label.lock().unwrap().clone();
        egui::Area::new(egui::Id::new("drag_drop_overlay"))
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .order(egui::Order::Foreground)
            .interactable(false)
            .show(ctx, |ui| {
                Frame::new()
                    .fill(Color32::from_rgba_unmultiplied(16, 44, 41, 242))
                    .stroke(Stroke::new(1.5_f32, c::ACCENT))
                    .corner_radius(CornerRadius::same(16))
                    .inner_margin(Margin::symmetric(32, 24))
                    .shadow(overlay_shadow())
                    .show(ui, |ui| {
                        ui.set_width((ctx.screen_rect().width() - 120.0).clamp(200.0, 440.0));
                        ui.vertical_centered(|ui| {
                            ui.label(theme::title(format!(
                                "松开鼠标，发送 {hovered_count} 个文件到远程电脑"
                            )));
                            ui.label(theme::muted(format!(
                                "通道：{via} · SHA-256 校验 · 断点续传"
                            )));
                        });
                    });
            });
    }
    if !dropped_files.is_empty() {
        let remote_dir = session.file_client.remote_dir.lock().unwrap().clone();
        let mut count = 0usize;
        for df in dropped_files {
            if let Some(path) = df.path {
                if path.is_file() {
                    session.file_client.send(FileCommand::UploadFile {
                        local_path: path,
                        remote_dir: remote_dir.clone(),
                    });
                    count += 1;
                }
            }
        }
        if count > 0 {
            state.show_file_panel = true;
            if !state.file_panel_initialized {
                state.refresh_local_dir();
                session
                    .file_client
                    .send(FileCommand::ListRemoteDir(remote_dir));
                state.file_panel_initialized = true;
            }
            state.drop_notice = Some(format!("已加入 {count} 个上传任务"));
        }
    }

    // Floating toolbar (top centre). Rarely used settings live in two popovers so the bar stays
    // narrow enough for small windows.
    egui::Area::new(egui::Id::new("remote_toolbar"))
        .anchor(Align2::CENTER_TOP, vec2(0.0, 10.0))
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            Frame::new()
                .fill(Color32::from_rgba_unmultiplied(21, 25, 33, 244))
                .stroke(Stroke::new(1.0_f32, c::BORDER_STRONG))
                .corner_radius(CornerRadius::same(12))
                .inner_margin(Margin::symmetric(8, 6))
                .shadow(overlay_shadow())
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        let collapsed = state.toolbar_collapsed;
                        if theme::chevron_button(ui, !collapsed)
                            .on_hover_text(if collapsed { "展开工具栏" } else { "收起工具栏" })
                            .clicked()
                        {
                            state.toolbar_collapsed = !collapsed;
                        }
                        ui.add_space(4.0);
                        let dot = if disconnect_reason.is_some() {
                            c::DANGER
                        } else if state.texture.is_none() {
                            c::WARN
                        } else {
                            c::OK
                        };
                        theme::status_dot(ui, dot);
                        ui.add_space(2.0);
                        // Long device names are truncated so the bar fits small windows.
                        theme::label_max_width(
                            ui,
                            theme::bold(&session.peer_name).size(14.0),
                            130.0,
                        )
                        .on_hover_text(&session.via_label);
                        ui.add_space(4.0);

                        if !collapsed {
                            toolbar_sep(ui);
                            if monitors.len() > 1 {
                                let label = monitor_label(&monitors, active_mon);
                                toolbar_popup(ui, &label, "tb_monitor", 250.0, |ui| {
                                    for m in &monitors {
                                        let txt = format!(
                                            "显示器 {} · {}×{} · {} dpi",
                                            m.index + 1,
                                            m.width,
                                            m.height,
                                            m.dpi
                                        );
                                        if ui.selectable_label(active_mon == m.index, txt).clicked()
                                            && active_mon != m.index
                                        {
                                            active_mon = m.index;
                                            session.send(ViewerCommand::SetMonitor(m.index));
                                            ui.memory_mut(|mem| mem.close_popup());
                                        }
                                    }
                                    if ui
                                        .selectable_label(active_mon == u32::MAX, "全部显示器（全景）")
                                        .clicked()
                                        && active_mon != u32::MAX
                                    {
                                        active_mon = u32::MAX;
                                        session.send(ViewerCommand::SetMonitor(u32::MAX));
                                        ui.memory_mut(|mem| mem.close_popup());
                                    }
                                });
                            }
                            let multi_relay = matches!(
                                session.reconnect_target,
                                ReconnectTarget::Relay { .. }
                            ) && session.available_relays.len() > 1;
                            let quality_w = if multi_relay { 470.0 } else { 370.0 };
                            toolbar_popup(ui, "画质", "tb_quality", quality_w, |ui| {
                                quality_menu(ui, state, session, multi_relay, &mut action);
                            });
                            toolbar_popup(ui, "工具", "tb_tools", 250.0, |ui| {
                                tools_menu(ui, state, session);
                            });
                            if theme::pill(ui, "文件传输", state.show_file_panel, false).clicked() {
                                state.toggle_file_panel(session);
                            }
                            if theme::pill(ui, "详情", state.show_stats, true)
                                .on_hover_text("显示或隐藏帧率、延迟、码率与编解码详情")
                                .clicked()
                            {
                                state.show_stats = !state.show_stats;
                            }
                            toolbar_sep(ui);
                            let fs_label = if state.fullscreen { "退出全屏" } else { "全屏" };
                            if theme::pill(ui, fs_label, false, false)
                                .on_hover_text("F11")
                                .clicked()
                            {
                                state.fullscreen = !state.fullscreen;
                                ctx.send_viewport_cmd(ViewportCommand::Fullscreen(state.fullscreen));
                            }
                            ui.add_space(2.0);
                        }

                        if theme::small_button(ui, "断开", Kind::Danger).clicked() {
                            session.send(ViewerCommand::Disconnect);
                            go_back = true;
                        }
                    });
                });
        });

    // Live stats (centered directly under the top toolbar when expanded; hidden by default).
    if state.show_stats && !state.toolbar_collapsed {
        let screen = ctx.screen_rect();
        let compact = screen.width() < 560.0 || screen.height() < 420.0;
        egui::Area::new(egui::Id::new("remote_hud"))
            .anchor(Align2::CENTER_TOP, vec2(0.0, 56.0))
            .order(egui::Order::Foreground)
            .interactable(false)
            .show(ctx, |ui| hud_panel(ui, &hud, compact));
    } else if let Some(notice) = &hud.status_notice {
        let y_offset = if state.toolbar_collapsed { 52.0 } else { 56.0 };
        egui::Area::new(egui::Id::new("remote_hud_notice"))
            .anchor(Align2::CENTER_TOP, vec2(0.0, y_offset))
            .order(egui::Order::Foreground)
            .interactable(false)
            .show(ctx, |ui| {
                Frame::new()
                    .fill(Color32::from_rgba_unmultiplied(13, 16, 22, 220))
                    .stroke(Stroke::new(1.0_f32, c::WARN.gamma_multiply(0.5)))
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(Margin::symmetric(12, 6))
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(RichText::new(notice).size(12.5).color(c::WARN))
                                .extend(),
                        );
                    });
            });
    }

    // M2 Dual-Pane File Transfer Manager Window
    if state.show_file_panel {
        render_file_manager_window(ctx, state, session);
    }

    // M3 Remote Virtual Display Manager Window
    if state.show_vdd_window {
        render_remote_vdd_window(ctx, state, session);
    }

    if let Some(reason) = &disconnect_reason {
        egui::Modal::new(egui::Id::new("session_closed_modal")).show(ctx, |ui| {
            ui.set_width((ctx.screen_rect().width() - 96.0).clamp(220.0, 400.0));
            ui.label(theme::title("会话已断开"));
            ui.add_space(2.0);
            ui.add(egui::Label::new(theme::muted(reason.as_str())).wrap());
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if theme::button(ui, "返回设备列表", Kind::Primary).clicked() {
                        go_back = true;
                    }
                });
            });
        });
    }

    if go_back {
        if state.fullscreen {
            ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
        }
        action = RemoteViewAction::Disconnect;
    }

    action
}

/// Paints the remote frame into `video_rect` and, while the session is alive and no overlay
/// window is open, forwards keyboard / mouse input aimed at it.
#[allow(clippy::too_many_arguments)]
fn present_video(
    ctx: &egui::Context,
    ui: &mut egui::Ui,
    state: &mut RemoteViewState,
    session: &ViewerSessionHandle,
    tex_id: TextureId,
    video_rect: Rect,
    resp: &egui::Response,
    alive: bool,
    special_keys: &[RawSpecialKey],
    lock_key_msgs: &[(u16, bool)],
) {
    ui.painter().image(
        tex_id,
        video_rect,
        Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
        Color32::WHITE,
    );

    // Only forward keyboard/mouse when floating modal windows aren't blocking pointer
    if !alive {
        return;
    }
    if resp.clicked() || resp.drag_started() {
        resp.request_focus();
    }
    let pointer_in_video = resp.hovered() || resp.dragged();
    let overlays_open = state.show_file_panel || state.show_vdd_window;
    if pointer_in_video && !overlays_open {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                resp.id,
                egui::EventFilter {
                    tab: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: true,
                },
            );
        });
    }
    if overlays_open {
        // Forwarding pauses: release the clipboard-shortcut keys, special keys, and modifiers the
        // host still holds, or they would stay stuck there and a stale record would swallow the
        // next shortcut press.
        let mut lifted = Vec::new();
        state.keys.release_all(&mut lifted);
        for ev in lifted {
            session.send(ViewerCommand::Input(ev));
        }
    } else {
        forward_inputs(
            ctx,
            state,
            session,
            video_rect,
            pointer_in_video,
            special_keys,
            lock_key_msgs,
        );
    }
}

fn overlay_shadow() -> Shadow {
    Shadow {
        offset: [0, 6],
        blur: 22,
        spread: 0,
        color: Color32::from_black_alpha(130),
    }
}

/// Shown until the first decoded frame arrives.
fn waiting_placeholder(
    ui: &mut egui::Ui,
    avail: Rect,
    hud: &HudStats,
    session: &ViewerSessionHandle,
) {
    let rect = Rect::from_center_size(avail.center(), vec2(avail.width().min(520.0), 140.0));
    ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
        ui.vertical_centered(|ui| {
            ui.add(egui::Spinner::new().size(30.0).color(c::ACCENT));
            ui.add_space(12.0);
            let (heading, sub) = if hud.connecting {
                (
                    format!("正在连接 {}", session.peer_name),
                    format!("正在建立端到端加密通道 · {}", session.via_label),
                )
            } else {
                (
                    "已连接，等待远程画面…".to_string(),
                    format!("{} · {}", session.peer_name, session.via_label),
                )
            };
            ui.label(theme::title(heading));
            ui.label(theme::muted(sub));
            if let Some(notice) = &hud.status_notice {
                ui.label(RichText::new(notice).size(12.5).color(c::WARN));
            }
        });
    });
}

fn toolbar_sep(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(9.0, 18.0), Sense::hover());
    ui.painter()
        .vline(rect.center().x, rect.y_range(), Stroke::new(1.0_f32, c::BORDER_STRONG));
}

/// Toolbar button that opens a popover below itself.
fn toolbar_popup(
    ui: &mut egui::Ui,
    label: &str,
    id: &str,
    width: f32,
    add: impl FnOnce(&mut egui::Ui),
) {
    let popup_id = ui.make_persistent_id(id);
    let open = ui.memory(|m| m.is_popup_open(popup_id));
    let resp = theme::pill(ui, label, open, true);
    if resp.clicked() {
        ui.memory_mut(|m| m.toggle_popup(popup_id));
    }
    egui::popup_below_widget(
        ui,
        popup_id,
        &resp,
        egui::PopupCloseBehavior::CloseOnClickOutside,
        |ui| {
            ui.set_width(width.min(ui.ctx().screen_rect().width() - 32.0).max(160.0));
            ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
            add(ui);
        },
    );
}

fn monitor_label(monitors: &[MonitorInfo], active: u32) -> String {
    if active == u32::MAX {
        return "全部显示器".into();
    }
    monitors
        .iter()
        .find(|m| m.index == active)
        .map(|m| format!("显示器 {}", m.index + 1))
        .unwrap_or_else(|| format!("显示器 {}", active + 1))
}

fn menu_label(text: &str) -> RichText {
    RichText::new(text).size(13.0).color(c::TEXT_2)
}

fn quality_menu(
    ui: &mut egui::Ui,
    state: &mut RemoteViewState,
    session: &ViewerSessionHandle,
    multi_relay: bool,
    action: &mut RemoteViewAction,
) {
    egui::Grid::new("quality_grid")
        .num_columns(2)
        .spacing([14.0, 12.0])
        .show(ui, |ui| {
            ui.label(menu_label("帧率"));
            let mut fps = state.target_fps;
            if theme::segmented(ui, &mut fps, &[(30, "30 FPS"), (60, "60 FPS")]) {
                state.target_fps = fps;
                session.send(ViewerCommand::SetFps(fps));
            }
            ui.end_row();

            ui.label(menu_label("码率上限"));
            let resp = ui.add(
                egui::Slider::new(&mut state.bitrate_mbps, 1.0..=50.0)
                    .step_by(0.5)
                    .fixed_decimals(1)
                    .suffix(" Mbps"),
            );
            if resp.drag_stopped() || resp.lost_focus() || (resp.changed() && !resp.dragged()) {
                session.send(ViewerCommand::SetBitrate(
                    (state.bitrate_mbps * 1000.0).round() as u32,
                ));
            }
            ui.end_row();

            ui.label(menu_label("分辨率上限"));
            let mut h = state.res_cap_h;
            if theme::segmented(ui, &mut h, &[(0, "原始"), (1080, "1080p"), (720, "720p")]) {
                state.res_cap_h = h;
                let (l, s) = crate::config::res_cap_for_height(h);
                session.send(ViewerCommand::SetResolutionCap(l, s));
            }
            ui.end_row();

            if multi_relay {
                ui.label(menu_label("控屏线路"));
                let cur_id = session.current_ctrl_relay_id.clone().unwrap_or_default();
                let mut idx = session
                    .available_relays
                    .iter()
                    .position(|r| r.id == cur_id)
                    .unwrap_or(usize::MAX);
                let opts: Vec<(usize, &str)> = session
                    .available_relays
                    .iter()
                    .enumerate()
                    .map(|(i, r)| (i, r.name.as_str()))
                    .collect();
                if theme::segmented(ui, &mut idx, &opts) {
                    if let Some(r) = session.available_relays.get(idx) {
                        *action = RemoteViewAction::SwitchControlRelay(r.clone());
                        ui.memory_mut(|mem| mem.close_popup());
                    }
                }
                ui.end_row();
            }
        });
    // Popovers lay out "justified", which would stretch the gaps in this wrapped note.
    ui.vertical(|ui| {
        ui.add(
            egui::Label::new(theme::faint(
                "自适应码率不会超过上限，网络变差时自动下调；2K/4K 屏走小带宽中转建议选 1080p。",
            ))
            .wrap(),
        );
    });
}

const REMOTE_SYSTEM_COMBOS: &[(&str, &str, &[(u16, bool)])] = &[
    ("Win", "向远程电脑发送开始菜单键 (Win)", &[(VK_LWIN, true)]),
    (
        "Win+D",
        "向远程电脑发送显示桌面 (Win+D)",
        &[(VK_LWIN, true), (VK_D, false)],
    ),
    (
        "Win+E",
        "向远程电脑打开文件资源管理器 (Win+E)",
        &[(VK_LWIN, true), (VK_E, false)],
    ),
    (
        "Win+R",
        "向远程电脑打开运行框 (Win+R)",
        &[(VK_LWIN, true), (VK_R, false)],
    ),
    (
        "Alt+Tab",
        "向远程电脑切换窗口 (Alt+Tab)",
        &[(VK_LMENU, false), (VK_TAB, false)],
    ),
];

fn remote_combo_events(keys: &[(u16, bool)]) -> Vec<InputEvent> {
    let mut events = Vec::with_capacity(keys.len() * 2);
    for &(vk, ext) in keys {
        events.push(key_event(vk, ext, true));
    }
    for &(vk, ext) in keys.iter().rev() {
        events.push(key_event(vk, ext, false));
    }
    events
}

fn send_remote_combo(
    state: &mut RemoteViewState,
    session: &ViewerSessionHandle,
    keys: &[(u16, bool)],
) {
    let mut events = Vec::with_capacity(keys.len() * 2 + 4);
    state.keys.combo_events(keys, &mut events);
    for ev in events {
        session.send(ViewerCommand::Input(ev));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolsMenuAction {
    None,
    OpenVdd,
    SendSas,
    SendCombo(&'static [(u16, bool)]),
}

fn render_remote_system_combos(
    ui: &mut egui::Ui,
    mut on_pick: impl FnMut(&'static [(u16, bool)]),
) {
    ui.vertical(|ui| {
        ui.label(theme::faint("发送系统快捷键"));
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
            for &(label, tip, keys) in REMOTE_SYSTEM_COMBOS {
                if theme::small_button(ui, label, Kind::Secondary)
                    .on_hover_text(tip)
                    .clicked()
                {
                    on_pick(keys);
                    ui.memory_mut(|mem| mem.close_popup());
                }
            }
        });
    });
}

fn render_tools_menu_body(
    ui: &mut egui::Ui,
    clipboard_enabled: &mut bool,
    scale_mode: &mut ScaleMode,
) -> (bool, ToolsMenuAction) {
    let mut clipboard_changed = false;
    let mut menu_action = ToolsMenuAction::None;
    egui::Grid::new("tools_grid")
        .num_columns(2)
        .spacing([14.0, 10.0])
        .show(ui, |ui| {
            ui.label(menu_label("剪贴板同步"));
            if theme::toggle(ui, clipboard_enabled).changed() {
                clipboard_changed = true;
            }
            ui.end_row();

            ui.label(menu_label("画面缩放"));
            theme::segmented(
                ui,
                scale_mode,
                &[(ScaleMode::Fit, "适应窗口"), (ScaleMode::Original, "1:1")],
            );
            ui.end_row();
        });
    theme::divider(ui);
    if theme::button(ui, "远程虚拟显示器…", Kind::Ghost).clicked() {
        menu_action = ToolsMenuAction::OpenVdd;
        ui.memory_mut(|mem| mem.close_popup());
    }
    if theme::button(ui, "发送 Ctrl+Alt+Del", Kind::Ghost).clicked() {
        menu_action = ToolsMenuAction::SendSas;
        ui.memory_mut(|mem| mem.close_popup());
    }
    theme::divider(ui);
    render_remote_system_combos(ui, |keys| {
        menu_action = ToolsMenuAction::SendCombo(keys);
    });
    (clipboard_changed, menu_action)
}

fn tools_menu(ui: &mut egui::Ui, state: &mut RemoteViewState, session: &ViewerSessionHandle) {
    let (clipboard_changed, menu_action) =
        render_tools_menu_body(ui, &mut state.clipboard_enabled, &mut state.scale_mode);
    if clipboard_changed {
        session.send(ViewerCommand::SetClipboardEnabled(state.clipboard_enabled));
    }
    match menu_action {
        ToolsMenuAction::None => {}
        ToolsMenuAction::OpenVdd => {
            state.show_vdd_window = true;
            session.send(ViewerCommand::VddAction(VddRequest::Query));
        }
        ToolsMenuAction::SendSas => {
            session.send(ViewerCommand::Input(InputEvent::SendSas));
        }
        ToolsMenuAction::SendCombo(keys) => {
            send_remote_combo(state, session, keys);
        }
    }
}

/// `compact`: small window, show only the headline numbers (and any warning).
fn hud_panel(ui: &mut egui::Ui, hud: &HudStats, compact: bool) {
    Frame::new()
        .fill(Color32::from_rgba_unmultiplied(13, 16, 22, 220))
        .stroke(Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(255, 255, 255, 22)))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing = vec2(5.0, 3.0);
            ui.horizontal(|ui| {
                metric(ui, &format!("{:.0}", hud.fps), "fps");
                ui.add_space(8.0);
                metric(ui, &hud.rtt_ms.to_string(), "ms 延迟");
                ui.add_space(8.0);
                metric(ui, &format!("{:.1}", hud.bitrate_mbps), "Mbps");
            });
            if !compact {
                hud_details(ui, hud);
            }
            if let Some(notice) = &hud.status_notice {
                let label = egui::Label::new(RichText::new(notice).size(12.0).color(c::WARN));
                if compact {
                    ui.set_max_width(300.0);
                    ui.add(label.wrap());
                } else {
                    ui.add(label.extend());
                }
            }
        });
}

fn hud_details(ui: &mut egui::Ui, hud: &HudStats) {
    if let Some(host) = hud.host.as_ref().filter(|h| h.flow_control) {
        // "画面" = host hand-off -> decoded here (includes all queueing on the path).
        let paused = if host.paused_ticks > 0 {
            format!(" · 限流 {} 帧", host.paused_ticks)
        } else {
            String::new()
        };
        ui.add(hud_text(format!(
            "画面 {} ms（峰值 {}）· 码率 {:.1}/{:.1} M · 解码 {:.1} ms{paused}",
            host.avg_delay_ms,
            host.max_delay_ms,
            host.sent_kbps as f32 / 1000.0,
            host.target_kbps as f32 / 1000.0,
            hud.decode_ms,
        )));
    }
    let codec = if hud.codec_label.is_empty() {
        "协商中…"
    } else {
        hud.codec_label.as_str()
    };
    let res = if hud.remote_res.is_empty() {
        "-"
    } else {
        hud.remote_res.as_str()
    };
    ui.add(hud_text(format!(
        "{res} · {codec} · 编码 {:.1} ms · {}",
        hud.encode_us as f32 / 1000.0,
        hud.relay_name
    )));
}

fn metric(ui: &mut egui::Ui, value: &str, unit: &str) {
    ui.label(RichText::new(value).monospace().size(17.0).color(c::TEXT));
    ui.label(RichText::new(unit).size(11.5).color(c::TEXT_3));
}

fn hud_text(text: String) -> egui::Label {
    // `extend`: labels in an auto-sized overlay would otherwise wrap at last frame's width.
    egui::Label::new(RichText::new(text).size(11.5).color(c::TEXT_2)).extend()
}

enum RowAction {
    None,
    Open,
    Transfer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryIcon {
    Computer,
    Desktop,
    Documents,
    Downloads,
    Drive,
    Folder,
    File,
}

fn is_drive_root(path: &str) -> bool {
    let b = path.trim().as_bytes();
    b.len() == 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

fn normalize_path_for_match(path: &str) -> String {
    let mut s = path.trim().replace('/', "\\").to_lowercase();
    if s.is_empty() {
        return s;
    }
    let b = s.as_bytes();
    if b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        s.push('\\');
        return s;
    }
    while s.len() > 3 && s.ends_with('\\') {
        s.pop();
    }
    s
}

fn paths_equal_ci(a: &str, b: &str) -> bool {
    let na = normalize_path_for_match(a);
    let nb = normalize_path_for_match(b);
    !na.is_empty() && na == nb
}

fn is_root_shortcut_name(name: &str) -> bool {
    let n = name.trim();
    n == "桌面"
        || n.starts_with("桌面 ")
        || n == "文档"
        || n.starts_with("文档 ")
        || n == "下载"
        || n.starts_with("下载 ")
}

fn is_quick_folder_entry(entry: &FileEntry) -> bool {
    is_root_shortcut_name(&entry.name) || !is_drive_root(&entry.path)
}

fn classify_entry_icon(entry: &FileEntry, is_root_view: bool) -> EntryIcon {
    if !entry.is_dir {
        return EntryIcon::File;
    }
    if is_root_view {
        let name = entry.name.trim();
        if name == "桌面" || name.starts_with("桌面 ") {
            return EntryIcon::Desktop;
        }
        if name == "文档" || name.starts_with("文档 ") {
            return EntryIcon::Documents;
        }
        if name == "下载" || name.starts_with("下载 ") {
            return EntryIcon::Downloads;
        }
    }
    if is_drive_root(&entry.path) {
        return EntryIcon::Drive;
    }
    EntryIcon::Folder
}

/// One file / folder row: click a folder to open it, the button transfers a file.
fn entry_row(
    ui: &mut egui::Ui,
    idx: usize,
    entry: &FileEntry,
    transfer_label: &str,
    is_root_view: bool,
) -> RowAction {
    let mut act = RowAction::None;
    let row_rect = Rect::from_min_size(ui.cursor().min, vec2(ui.available_width(), 28.0));
    // Registered before the row's own widgets so the transfer button stays clickable on top.
    let mut row = ui.interact(
        row_rect,
        ui.id().with(("entry_row", idx, &entry.name, &entry.path)),
        Sense::click(),
    );
    if row.contains_pointer() {
        ui.painter()
            .rect_filled(row_rect, CornerRadius::same(6), c::SURFACE_2);
    }
    if entry.is_dir {
        if row.clicked() {
            act = RowAction::Open;
        }
        row = row.on_hover_cursor(CursorIcon::PointingHand);
        if is_root_view && is_root_shortcut_name(&entry.name) {
            row.on_hover_text(&entry.path);
        }
    }
    let icon = classify_entry_icon(entry, is_root_view);
    ui.horizontal(|ui| {
        ui.set_height(28.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add_space(2.0);
            if !entry.is_dir {
                if theme::small_button(ui, transfer_label, Kind::Ghost).clicked() {
                    act = RowAction::Transfer;
                }
                ui.label(theme::faint(format_bytes(entry.size)));
            }
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                ui.add_space(6.0);
                paint_entry_icon(ui, icon);
                ui.add_space(4.0);
                let color = if entry.is_dir { c::TEXT } else { c::TEXT_2 };
                ui.add(
                    egui::Label::new(RichText::new(&entry.name).color(color))
                        .truncate()
                        .selectable(false),
                );
            });
        });
    });
    act
}

/// Small painted vector icon (emoji glyphs render tiny and skewed at this size).
fn paint_entry_icon(ui: &mut egui::Ui, icon: EntryIcon) {
    let (rect, _) = ui.allocate_exact_size(vec2(18.0, 16.0), Sense::hover());
    let p = ui.painter();
    let (l, t) = (rect.left() + 1.0, rect.top() + 1.0);
    match icon {
        EntryIcon::Computer => {
            let frame_c = Color32::from_rgb(96, 178, 245);
            let screen_c = Color32::from_rgb(44, 118, 188);
            p.rect_filled(
                Rect::from_min_max(pos2(l + 1.5, t + 1.5), pos2(l + 14.5, t + 10.5)),
                CornerRadius::same(1),
                screen_c,
            );
            p.rect_stroke(
                Rect::from_min_max(pos2(l + 1.0, t + 1.0), pos2(l + 15.0, t + 11.0)),
                CornerRadius::same(2),
                Stroke::new(1.3_f32, frame_c),
                egui::StrokeKind::Inside,
            );
            p.line_segment(
                [pos2(l + 8.0, t + 11.0), pos2(l + 8.0, t + 13.5)],
                Stroke::new(1.5_f32, frame_c),
            );
            p.line_segment(
                [pos2(l + 4.5, t + 13.8), pos2(l + 11.5, t + 13.8)],
                Stroke::new(1.5_f32, frame_c),
            );
        }
        EntryIcon::Desktop => {
            let accent = Color32::from_rgb(76, 172, 248);
            let screen = Color32::from_rgb(38, 98, 162);
            p.rect_filled(
                Rect::from_min_max(pos2(l + 1.5, t + 1.5), pos2(l + 14.5, t + 10.5)),
                CornerRadius::same(1),
                screen,
            );
            p.rect_filled(
                Rect::from_min_max(pos2(l + 2.0, t + 8.5), pos2(l + 14.0, t + 10.3)),
                CornerRadius::ZERO,
                accent.gamma_multiply(0.7),
            );
            p.rect_stroke(
                Rect::from_min_max(pos2(l + 1.0, t + 1.0), pos2(l + 15.0, t + 11.0)),
                CornerRadius::same(2),
                Stroke::new(1.3_f32, accent),
                egui::StrokeKind::Inside,
            );
            p.line_segment(
                [pos2(l + 5.0, t + 13.8), pos2(l + 11.0, t + 13.8)],
                Stroke::new(1.5_f32, accent),
            );
        }
        EntryIcon::Documents => {
            let edge = Color32::from_rgb(176, 196, 222);
            let line = Color32::from_rgb(122, 148, 180);
            p.rect_stroke(
                Rect::from_min_max(pos2(l + 2.0, t + 0.5), pos2(l + 14.0, t + 14.0)),
                CornerRadius::same(2),
                Stroke::new(1.3_f32, edge),
                egui::StrokeKind::Inside,
            );
            p.line_segment(
                [pos2(l + 10.0, t + 0.8), pos2(l + 13.6, t + 4.4)],
                Stroke::new(1.1_f32, edge),
            );
            for i in 0..3 {
                let y = t + 5.2 + i as f32 * 2.8;
                p.line_segment([pos2(l + 4.8, y), pos2(l + 11.2, y)], Stroke::new(1.2_f32, line));
            }
        }
        EntryIcon::Downloads => {
            let green = Color32::from_rgb(66, 206, 146);
            p.line_segment(
                [pos2(l + 8.0, t + 1.5), pos2(l + 8.0, t + 10.2)],
                Stroke::new(2.0_f32, green),
            );
            p.line(
                vec![
                    pos2(l + 4.2, t + 6.8),
                    pos2(l + 8.0, t + 10.6),
                    pos2(l + 11.8, t + 6.8),
                ],
                Stroke::new(1.8_f32, green),
            );
            p.line_segment(
                [pos2(l + 2.5, t + 13.5), pos2(l + 13.5, t + 13.5)],
                Stroke::new(1.6_f32, green),
            );
        }
        EntryIcon::Drive => {
            let body = Color32::from_rgb(152, 166, 186);
            p.rect_filled(
                Rect::from_min_max(pos2(l + 1.0, t + 4.0), pos2(l + 15.0, t + 12.5)),
                CornerRadius::same(2),
                Color32::from_rgb(56, 65, 80),
            );
            p.rect_stroke(
                Rect::from_min_max(pos2(l + 1.0, t + 4.0), pos2(l + 15.0, t + 12.5)),
                CornerRadius::same(2),
                Stroke::new(1.3_f32, body),
                egui::StrokeKind::Inside,
            );
            p.line_segment(
                [pos2(l + 2.5, t + 9.2), pos2(l + 13.5, t + 9.2)],
                Stroke::new(1.0_f32, Color32::from_rgb(96, 108, 126)),
            );
            p.circle_filled(pos2(l + 12.2, t + 10.9), 1.3, c::ACCENT);
        }
        EntryIcon::Folder => {
            let fill = Color32::from_rgb(226, 182, 92);
            p.rect_filled(
                Rect::from_min_max(pos2(l, t + 1.0), pos2(l + 7.0, t + 5.0)),
                CornerRadius::same(2),
                fill,
            );
            p.rect_filled(
                Rect::from_min_max(pos2(l, t + 3.0), pos2(l + 16.0, t + 14.0)),
                CornerRadius::same(2),
                fill,
            );
            p.line_segment(
                [pos2(l + 1.0, t + 5.5), pos2(l + 15.0, t + 5.5)],
                Stroke::new(1.0_f32, Color32::from_rgb(190, 146, 64)),
            );
        }
        EntryIcon::File => {
            let stroke = Stroke::new(1.2_f32, c::TEXT_3);
            p.rect_stroke(
                Rect::from_min_max(pos2(l + 2.0, t), pos2(l + 14.0, t + 14.0)),
                CornerRadius::same(2),
                stroke,
                egui::StrokeKind::Inside,
            );
            for i in 0..3 {
                let y = t + 4.5 + i as f32 * 3.0;
                p.line_segment([pos2(l + 5.0, y), pos2(l + 11.0, y)], Stroke::new(1.0_f32, c::TEXT_3));
            }
        }
    }
}

fn shortcut_chevron_button(ui: &mut egui::Ui, open: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(28.0, 32.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let hovered = resp.hovered();
        let fill = if open {
            c::SURFACE_3
        } else if hovered {
            c::SURFACE_3
        } else {
            c::SURFACE_2
        };
        let stroke_c = if open {
            c::ACCENT
        } else if hovered {
            Color32::from_rgb(74, 86, 106)
        } else {
            c::BORDER_STRONG
        };
        ui.painter().rect(
            rect,
            CornerRadius::same(theme::RADIUS),
            fill,
            Stroke::new(1.0_f32, stroke_c),
            egui::StrokeKind::Inside,
        );
        let ctr = rect.center();
        let dir = if open { -1.0 } else { 1.0 };
        let fg = if open || hovered { c::TEXT } else { c::TEXT_2 };
        ui.painter().line(
            vec![
                pos2(ctr.x - 4.0, ctr.y - 2.0 * dir),
                pos2(ctr.x, ctr.y + 2.0 * dir),
                pos2(ctr.x + 4.0, ctr.y - 2.0 * dir),
            ],
            Stroke::new(1.6_f32, fg),
        );
    }
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

fn shortcut_menu_item(
    ui: &mut egui::Ui,
    id: egui::Id,
    icon: EntryIcon,
    label: &str,
    hover_path: Option<&str>,
    active: bool,
) -> bool {
    let row_rect = Rect::from_min_size(ui.cursor().min, vec2(ui.available_width(), 28.0));
    let mut row = ui.interact(row_rect, id, Sense::click());
    if let Some(tip) = hover_path {
        row = row.on_hover_text(tip);
    }
    let fill = if active {
        Some(c::ACCENT_SOFT)
    } else if row.contains_pointer() {
        Some(c::SURFACE_2)
    } else {
        None
    };
    if let Some(bg) = fill {
        ui.painter().rect_filled(row_rect, CornerRadius::same(6), bg);
    }
    let clicked = row.on_hover_cursor(CursorIcon::PointingHand).clicked();
    ui.horizontal(|ui| {
        ui.set_height(28.0);
        ui.add_space(6.0);
        paint_entry_icon(ui, icon);
        ui.add_space(2.0);
        let text_color = if active { c::ACCENT_HOVER } else { c::TEXT };
        ui.add(
            egui::Label::new(RichText::new(label).size(13.0).color(text_color))
                .truncate()
                .selectable(false),
        );
    });
    clicked
}

/// Address bar with Sunlogin-style quick-location dropdown (此电脑 / 桌面 / 文档 / 下载 / 盘符).
fn path_bar_with_shortcuts(
    ui: &mut egui::Ui,
    id_salt: &str,
    path_input: &mut String,
    current_dir: &str,
    roots: &[FileEntry],
    loading_roots: bool,
) -> Option<String> {
    let mut navigate_to = None;
    let popup_id = ui.make_persistent_id((id_salt, "shortcuts_popup"));
    let popup_open = ui.memory(|m| m.is_popup_open(popup_id));

    let (anchor_resp, btn_resp, enter_or_go) = ui
        .horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let input_w = (ui.available_width() - 96.0).max(60.0);
            let input_resp =
                theme::text_input(ui, path_input, input_w, "此电脑（输入路径或点右侧▾）");
            let btn_resp = shortcut_chevron_button(ui, popup_open)
                .on_hover_text("快捷位置（此电脑 / 桌面 / 文档 / 下载 / 盘符）");
            let enter = input_resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            let go = theme::small_button(ui, "前往", Kind::Secondary).clicked();
            if (input_resp.clicked() || input_resp.gained_focus()) && popup_open {
                ui.memory_mut(|m| m.close_popup());
            }
            (input_resp.union(btn_resp.clone()), btn_resp, enter || go)
        })
        .inner;

    if btn_resp.clicked() {
        ui.memory_mut(|m| m.toggle_popup(popup_id));
    }
    if enter_or_go {
        if popup_open {
            ui.memory_mut(|m| m.close_popup());
        }
        navigate_to = Some(path_input.clone());
    }

    let popup_w = anchor_resp.rect.width().clamp(210.0, 320.0);
    let space_below = (ui.ctx().screen_rect().bottom() - anchor_resp.rect.bottom() - 20.0).max(80.0);
    let menu_max_h = space_below.min(320.0);
    let mut picked: Option<String> = None;
    egui::popup_below_widget(
        ui,
        popup_id,
        &anchor_resp,
        egui::PopupCloseBehavior::CloseOnClickOutside,
        |ui| {
            ui.set_width(popup_w);
            ui.vertical(|ui| {
                egui::ScrollArea::vertical()
                    .id_salt((id_salt, "shortcut_scroll"))
                    .max_height(menu_max_h)
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        if shortcut_menu_item(
                            ui,
                            ui.id().with(("computer", 0usize)),
                            EntryIcon::Computer,
                            "此电脑",
                            None,
                            current_dir.trim().is_empty(),
                        ) {
                            picked = Some(String::new());
                        }

                        let quick_folders: Vec<&FileEntry> =
                            roots.iter().filter(|e| is_quick_folder_entry(e)).collect();
                        let drives: Vec<&FileEntry> =
                            roots.iter().filter(|e| !is_quick_folder_entry(e)).collect();

                        if !quick_folders.is_empty() {
                            theme::divider(ui);
                            for (idx, entry) in quick_folders.into_iter().enumerate() {
                                let icon = classify_entry_icon(entry, true);
                                let active = paths_equal_ci(current_dir, &entry.path);
                                if shortcut_menu_item(
                                    ui,
                                    ui.id().with(("quick", idx, &entry.name, &entry.path)),
                                    icon,
                                    &entry.name,
                                    Some(entry.path.as_str()),
                                    active,
                                ) {
                                    picked = Some(entry.path.clone());
                                }
                            }
                        }

                        if !drives.is_empty() {
                            theme::divider(ui);
                            for (idx, entry) in drives.into_iter().enumerate() {
                                let active = paths_equal_ci(current_dir, &entry.path);
                                if shortcut_menu_item(
                                    ui,
                                    ui.id().with(("drive", idx, &entry.name, &entry.path)),
                                    EntryIcon::Drive,
                                    &entry.name,
                                    Some(entry.path.as_str()),
                                    active,
                                ) {
                                    picked = Some(entry.path.clone());
                                }
                            }
                        }

                        if roots.is_empty() {
                            theme::divider(ui);
                            let note = if loading_roots {
                                "正在读取快捷位置…"
                            } else {
                                "点击「此电脑」刷新快捷位置"
                            };
                            ui.add_space(2.0);
                            ui.label(theme::faint(note));
                        }
                    });
            });
        },
    );

    if let Some(target) = picked {
        *path_input = target.clone();
        ui.memory_mut(|m| m.close_popup());
        navigate_to = Some(target);
    }

    navigate_to
}

fn pane_frame() -> Frame {
    Frame::new()
        .fill(c::FIELD)
        .stroke(Stroke::new(1.0_f32, c::BORDER))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(10))
}

fn local_pane(
    ui: &mut egui::Ui,
    state: &mut RemoteViewState,
    session: &ViewerSessionHandle,
    list_h: f32,
) {
    pane_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(theme::bold("本地电脑").size(14.0));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if theme::small_button(ui, "刷新", Kind::Ghost).clicked() {
                    state.refresh_local_dir();
                }
                if theme::small_button(ui, "上级", Kind::Ghost).clicked() {
                    let next = Path::new(state.local_dir.trim())
                        .parent()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    state.navigate_local_dir(next);
                }
                if theme::small_button(ui, "此电脑", Kind::Ghost).clicked() {
                    state.navigate_local_dir(String::new());
                }
            });
        });
        let cur_local_dir = state.local_dir.clone();
        let local_roots = state.local_roots.clone();
        if let Some(target) = path_bar_with_shortcuts(
            ui,
            "local_path_bar",
            &mut state.local_dir_input,
            &cur_local_dir,
            &local_roots,
            false,
        ) {
            state.navigate_local_dir(target);
        }
        if let Some(err) = &state.local_error {
            ui.label(RichText::new(err).size(12.5).color(c::DANGER));
        }
        let is_root = state.local_dir.trim().is_empty();
        let entries = state.local_entries.clone();
        let cur_remote_dir = session.file_client.remote_dir.lock().unwrap().clone();
        egui::ScrollArea::vertical()
            .id_salt("local_entries")
            .max_height(list_h)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                if entries.is_empty() {
                    ui.label(theme::faint("（空）"));
                }
                for (idx, entry) in entries.iter().enumerate() {
                    match entry_row(ui, idx, entry, "上传 →", is_root) {
                        RowAction::Open => {
                            state.navigate_local_dir(entry.path.clone());
                        }
                        RowAction::Transfer => {
                            session.file_client.send(FileCommand::UploadFile {
                                local_path: PathBuf::from(&entry.path),
                                remote_dir: cur_remote_dir.clone(),
                            });
                        }
                        RowAction::None => {}
                    }
                }
            });
    });
}

fn remote_pane(
    ui: &mut egui::Ui,
    state: &mut RemoteViewState,
    session: &ViewerSessionHandle,
    list_h: f32,
) {
    pane_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        let cur_remote_dir = session.file_client.remote_dir.lock().unwrap().clone();
        ui.horizontal(|ui| {
            ui.label(theme::bold("远程电脑").size(14.0));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if theme::small_button(ui, "刷新", Kind::Ghost).clicked() {
                    state.remote_dir_input = cur_remote_dir.clone();
                    session
                        .file_client
                        .send(FileCommand::ListRemoteDir(cur_remote_dir.clone()));
                }
                if theme::small_button(ui, "上级", Kind::Ghost).clicked() {
                    let next = Path::new(cur_remote_dir.trim())
                        .parent()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    session.file_client.send(FileCommand::ListRemoteDir(next));
                }
                if theme::small_button(ui, "此电脑", Kind::Ghost).clicked() {
                    if cur_remote_dir.trim().is_empty() {
                        state.remote_dir_input.clear();
                    }
                    session
                        .file_client
                        .send(FileCommand::ListRemoteDir(String::new()));
                }
            });
        });
        let remote_roots = session.file_client.remote_roots.lock().unwrap().clone();
        let remote_loading = session
            .file_client
            .remote_loading
            .load(std::sync::atomic::Ordering::Relaxed);
        if let Some(target) = path_bar_with_shortcuts(
            ui,
            "remote_path_bar",
            &mut state.remote_dir_input,
            &cur_remote_dir,
            &remote_roots,
            remote_loading,
        ) {
            session
                .file_client
                .send(FileCommand::ListRemoteDir(target));
        }

        if remote_loading {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(14.0).color(c::ACCENT));
                ui.label(theme::faint("正在读取远程目录…"));
            });
        }
        if let Some(err) = session.file_client.remote_error.lock().unwrap().clone() {
            ui.label(RichText::new(err).size(12.5).color(c::DANGER));
        }

        let is_root = cur_remote_dir.trim().is_empty();
        let r_entries = session.file_client.remote_entries.lock().unwrap().clone();
        egui::ScrollArea::vertical()
            .id_salt("remote_entries")
            .max_height(list_h)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                if r_entries.is_empty() {
                    ui.label(theme::faint("（空）"));
                }
                for (idx, entry) in r_entries.iter().enumerate() {
                    match entry_row(ui, idx, entry, "← 下载", is_root) {
                        RowAction::Open => {
                            session
                                .file_client
                                .send(FileCommand::ListRemoteDir(entry.path.clone()));
                        }
                        RowAction::Transfer => {
                            let local_target_dir = if state.local_dir.trim().is_empty() {
                                crate::transfer::default_receive_dir()
                            } else {
                                PathBuf::from(&state.local_dir)
                            };
                            session.file_client.send(FileCommand::DownloadFile {
                                remote_path: entry.path.clone(),
                                local_dir: local_target_dir,
                            });
                        }
                        RowAction::None => {}
                    }
                }
            });
    });
}

fn render_file_manager_window(
    ctx: &egui::Context,
    state: &mut RemoteViewState,
    session: &ViewerSessionHandle,
) {
    let remote_dir = session.file_client.remote_dir.lock().unwrap().clone();
    if remote_dir != state.last_synced_remote_dir {
        state.remote_dir_input = remote_dir.clone();
        state.last_synced_remote_dir = remote_dir.clone();
    }

    // Keep the whole window inside the viewport and below the floating toolbar, so its title
    // bar and close button stay reachable on small screens.
    let screen = ctx.screen_rect();
    let area = Rect::from_min_max(
        pos2(screen.left() + 8.0, screen.top() + 56.0),
        pos2(screen.right() - 8.0, screen.bottom() - 8.0),
    );
    // Window sizes are for the content: leave room for the title bar and frame margins.
    let max_content = (area.size() - vec2(36.0, 80.0)).max(vec2(240.0, 160.0));
    let records = session.file_client.transfers.lock().unwrap().clone();

    let mut open = state.show_file_panel;
    egui::Window::new("文件传输")
        .id(egui::Id::new("file_transfer_window"))
        .constrain_to(area)
        .pivot(Align2::CENTER_CENTER)
        .default_pos(area.center())
        .open(&mut open)
        .default_size(vec2(880.0, 560.0).min(max_content))
        .min_size(vec2(480.0, 220.0).min(max_content))
        .max_size(max_content)
        .resizable(true)
        .collapsible(false)
        .show(ctx, |ui| {
            // The file lists take the height that is left; on tiny screens the whole window
            // content scrolls instead of being cut off.
            let avail_h = ui.available_height();
            let queue_h = if records.is_empty() {
                0.0
            } else {
                (avail_h * 0.3).clamp(60.0, 170.0)
            };
            let list_h = (avail_h - 210.0 - queue_h).clamp(80.0, 520.0);
            egui::ScrollArea::vertical()
                .id_salt("file_window_scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    file_channel_bar(ui, state, session);
                    ui.add_space(4.0);

                    ui.columns(2, |cols| {
                        cols[0].push_id("local_pane", |ui| local_pane(ui, state, session, list_h));
                        cols[1]
                            .push_id("remote_pane", |ui| remote_pane(ui, state, session, list_h));
                    });

                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label(theme::subheading("传输队列"));
                        theme::hint(ui, "也可以把本地文件直接拖进远控画面上传");
                    });
                    if records.is_empty() {
                        ui.label(theme::faint("暂无传输任务"));
                    } else {
                        transfer_queue(ui, session, records, queue_h);
                    }
                });
        });
    state.show_file_panel = open;
}

/// Transfer channel + file relay picker (+ drop notice). The picker moves to a second line when
/// the window is narrow.
fn file_channel_bar(ui: &mut egui::Ui, state: &mut RemoteViewState, session: &ViewerSessionHandle) {
    let via = session.file_client.active_via_label.lock().unwrap().clone();
    let can_pick = matches!(session.reconnect_target, ReconnectTarget::Relay { .. })
        && !session.available_relays.is_empty();
    let one_line = ui.available_width() >= 660.0;
    ui.horizontal(|ui| {
        ui.label(menu_label("传输通道"));
        theme::chip(ui, &via, c::ACCENT_HOVER, c::ACCENT_SOFT);
        if one_line {
            if can_pick {
                ui.add_space(10.0);
                file_relay_picker(ui, state, session);
            }
            drop_notice_chip(ui, state);
        }
    });
    if !one_line && (can_pick || state.drop_notice.is_some()) {
        ui.horizontal(|ui| {
            if can_pick {
                file_relay_picker(ui, state, session);
            }
            drop_notice_chip(ui, state);
        });
    }
}

fn file_relay_picker(
    ui: &mut egui::Ui,
    state: &mut RemoteViewState,
    session: &ViewerSessionHandle,
) {
    ui.label(menu_label("文件节点"));
    let cur_label = state
        .selected_file_relay_id
        .as_ref()
        .and_then(|id| session.available_relays.iter().find(|r| &r.id == id))
        .map(|r| r.name.clone())
        .unwrap_or_else(|| "默认（控屏节点）".into());
    egui::ComboBox::from_id_salt("file_relay_pick")
        .selected_text(cur_label)
        .width(200.0)
        .show_ui(ui, |ui| {
            for node in &session.available_relays {
                let label = format!("{}（{}）", node.name, node.role.label());
                let is_sel = state.selected_file_relay_id.as_deref() == Some(&node.id);
                if ui.selectable_label(is_sel, label).clicked() && !is_sel {
                    state.selected_file_relay_id = Some(node.id.clone());
                    session
                        .file_client
                        .send(FileCommand::SetRelay(Some(node.clone())));
                }
            }
        });
}

fn drop_notice_chip(ui: &mut egui::Ui, state: &RemoteViewState) {
    if let Some(note) = state.drop_notice.clone() {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            theme::chip(ui, note, c::OK, Color32::from_rgb(18, 44, 36));
        });
    }
}

/// Two lines per transfer (name + status, then size / channel / badges) so a row never gets
/// wider than a small window.
fn transfer_queue(
    ui: &mut egui::Ui,
    session: &ViewerSessionHandle,
    records: Vec<crate::transfer::TransferRecord>,
    max_height: f32,
) {
    egui::ScrollArea::vertical()
        .id_salt("transfer_queue")
        .max_height(max_height)
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            for rec in records {
                let progress = if rec.total_bytes > 0 {
                    (rec.transferred_bytes as f32 / rec.total_bytes as f32).clamp(0.0, 1.0)
                } else if rec.state == TransferState::Done {
                    1.0
                } else {
                    0.0
                };
                Frame::new()
                    .fill(c::SURFACE_2)
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            let dir_tag = match rec.direction {
                                TransferDirection::Upload => "上传",
                                TransferDirection::Download => "下载",
                            };
                            theme::tag(ui, dir_tag);
                            // Status first (right-aligned), so a long file name is truncated.
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                transfer_status(ui, session, &rec);
                                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                                    ui.add(
                                        egui::Label::new(theme::bold(&rec.file_name).size(13.5))
                                            .truncate(),
                                    )
                                    .on_hover_text(format!(
                                        "本地：{}\n远程：{}",
                                        rec.local_path, rec.remote_path
                                    ));
                                });
                            });
                        });
                        ui.horizontal_wrapped(|ui| {
                            ui.label(theme::faint(format!(
                                "{} / {}",
                                format_bytes(rec.transferred_bytes),
                                format_bytes(rec.total_bytes)
                            )));
                            ui.label(theme::faint(&rec.via_label));
                            if rec.resumed {
                                theme::chip(ui, "断点续传", c::WARN, Color32::from_rgb(52, 40, 18));
                            }
                            if rec.sha256_verified {
                                theme::chip(
                                    ui,
                                    "SHA-256 已校验",
                                    c::OK,
                                    Color32::from_rgb(18, 44, 36),
                                );
                            }
                        });
                        let bar = match rec.state {
                            TransferState::Done => c::OK,
                            TransferState::Failed(_) => c::DANGER,
                            TransferState::Cancelled => c::WARN,
                            _ => c::ACCENT,
                        };
                        ui.add(
                            egui::ProgressBar::new(progress)
                                .desired_height(5.0)
                                .fill(bar),
                        );
                    });
            }
        });
}

fn transfer_status(
    ui: &mut egui::Ui,
    session: &ViewerSessionHandle,
    rec: &crate::transfer::TransferRecord,
) {
    match &rec.state {
        TransferState::Active => {
            if theme::small_button(ui, "取消", Kind::Ghost).clicked() {
                session
                    .file_client
                    .send(FileCommand::Cancel(rec.transfer_id));
            }
            ui.label(
                RichText::new(format!("{:.2} Mbps", rec.speed_mbps))
                    .size(12.5)
                    .color(c::ACCENT_HOVER),
            );
        }
        TransferState::Queued => {
            ui.label(theme::faint("排队中…"));
        }
        TransferState::Done => {
            ui.label(RichText::new("已完成").size(12.5).color(c::OK));
        }
        TransferState::Cancelled => {
            ui.label(RichText::new("已暂停（可续传）").size(12.5).color(c::WARN));
        }
        TransferState::Failed(err) => {
            theme::label_max_width(
                ui,
                RichText::new(format!("失败：{err}"))
                    .size(12.5)
                    .color(c::DANGER),
                220.0,
            );
        }
    }
}

fn render_remote_vdd_window(
    ctx: &egui::Context,
    state: &mut RemoteViewState,
    session: &ViewerSessionHandle,
) {
    let mut open = state.show_vdd_window;
    let info = session.vdd_info.lock().unwrap().clone();

    // Same idea as the file window: stay inside the viewport, below the toolbar.
    let screen = ctx.screen_rect();
    let area = Rect::from_min_max(
        pos2(screen.left() + 8.0, screen.top() + 56.0),
        pos2(screen.right() - 8.0, screen.bottom() - 8.0),
    );
    egui::Window::new("远程虚拟显示器")
        .id(egui::Id::new("remote_vdd_window"))
        .constrain_to(area)
        .pivot(Align2::CENTER_CENTER)
        .default_pos(area.center())
        .open(&mut open)
        .resizable(false)
        .collapsible(false)
        .show(ctx, |ui| {
            ui.set_width((area.width() - 40.0).clamp(240.0, 430.0));
            match &info {
                Some(v) => {
                    theme::form_row_w(ui, "驱动状态", 84.0, |ui| {
                        ui.add(egui::Label::new(RichText::new(&v.details).color(c::TEXT)).wrap());
                    });
                    theme::form_row_w(ui, "活动虚拟屏", 84.0, |ui| {
                        ui.label(RichText::new(v.active_virtual_displays.to_string()).color(c::TEXT));
                    });
                }
                None => {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(14.0).color(c::ACCENT));
                        ui.label(theme::muted("正在查询远程电脑的虚拟屏状态…"));
                    });
                }
            }
            theme::divider(ui);
            ui.horizontal_wrapped(|ui| {
                if theme::small_button(ui, "创建 1080P 60Hz", Kind::Secondary).clicked() {
                    session.send(ViewerCommand::VddAction(VddRequest::AddDisplay {
                        width: 1920,
                        height: 1080,
                        refresh_hz: 60,
                    }));
                }
                if theme::small_button(ui, "创建 2K 60Hz", Kind::Secondary).clicked() {
                    session.send(ViewerCommand::VddAction(VddRequest::AddDisplay {
                        width: 2560,
                        height: 1440,
                        refresh_hz: 60,
                    }));
                }
                if theme::small_button(ui, "移除虚拟屏", Kind::Danger).clicked() {
                    session.send(ViewerCommand::VddAction(VddRequest::RemoveDisplay));
                }
            });
            ui.horizontal_wrapped(|ui| {
                if theme::small_button(ui, "刷新状态", Kind::Ghost).clicked() {
                    session.send(ViewerCommand::VddAction(VddRequest::Query));
                }
                ui.label(theme::faint("驱动下载"));
                if theme::small_button(ui, "Parsec VDD ↗", Kind::Ghost).clicked() {
                    crate::vdd::open_url(PARSEC_VDD_DOWNLOAD_URL);
                }
                if theme::small_button(ui, "Virtual-Display-Driver ↗", Kind::Ghost).clicked() {
                    crate::vdd::open_url(MTT_VDD_DOWNLOAD_URL);
                }
            });
        });

    state.show_vdd_window = open;
}

fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

fn forward_inputs(
    ctx: &egui::Context,
    state: &mut RemoteViewState,
    session: &ViewerSessionHandle,
    video_rect: Rect,
    pointer_in_video: bool,
    special_keys: &[RawSpecialKey],
    lock_key_msgs: &[(u16, bool)],
) {
    let mut events_to_send = Vec::new();
    let (kbd_locks, gks_locks, kbd_ok) = read_viewer_lock_states();
    let fg_owned = current_thread_owns_foreground();
    let mut egui_focused = false;
    let mut window_focused = false;

    ctx.input(|i| {
        if pointer_in_video {
            if let Some(pos) = i.pointer.hover_pos() {
                if video_rect.contains(pos) && video_rect.width() > 1.0 && video_rect.height() > 1.0
                {
                    let nx = ((pos.x - video_rect.min.x) / video_rect.width()).clamp(0.0, 1.0);
                    let ny = ((pos.y - video_rect.min.y) / video_rect.height()).clamp(0.0, 1.0);
                    let moved = match state.last_norm_pos {
                        Some((px, py)) => (nx - px).abs() > 0.0004 || (ny - py).abs() > 0.0004,
                        None => true,
                    };
                    if moved {
                        state.last_norm_pos = Some((nx, ny));
                        events_to_send.push(InputEvent::MouseMove {
                            norm_x: nx,
                            norm_y: ny,
                        });
                    }
                }
            }

            if i.raw_scroll_delta.y.abs() > 0.1 || i.raw_scroll_delta.x.abs() > 0.1 {
                events_to_send.push(InputEvent::MouseWheel {
                    delta_x: (i.raw_scroll_delta.x * 3.0).round() as i16,
                    delta_y: (i.raw_scroll_delta.y * 3.0).round() as i16,
                });
            }
        }

        // Win32 key state of this (UI) thread: in step with the key messages egui has processed.
        let key_down = |vk: u16| unsafe { GetKeyState(i32::from(vk)) } < 0;
        // ...and also physically held right now (the thread state can be stale after a focus
        // change), for silent clipboard paste presses inferred without any key event.
        let held_now =
            |vk: u16| key_down(vk) && unsafe { GetAsyncKeyState(i32::from(vk)) } < 0;
        let lock_toggled = |vk: u16| match vk {
            VK_CAPITAL => kbd_locks.0,
            VK_NUMLOCK => kbd_locks.1,
            VK_SCROLL => kbd_locks.2,
            _ => (unsafe { GetKeyState(i32::from(vk)) } & 1) != 0,
        };
        egui_focused = i.focused;
        let has_blur_ev = i
            .events
            .iter()
            .any(|e| matches!(e, Event::WindowFocused(false)));
        window_focused = resolve_window_focused(egui_focused, fg_owned, has_blur_ev);
        // Sync lock-key states BEFORE forwarding any key presses from this frame.
        state
            .keys
            .sync_locks(window_focused, &lock_toggled, &mut events_to_send);
        for (idx, ev) in i.events.iter().enumerate() {
            match ev {
                Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    ..
                } => {
                    if !*pressed || (pointer_in_video && video_rect.contains(*pos)) {
                        if video_rect.width() > 1.0 && video_rect.height() > 1.0 {
                            let nx =
                                ((pos.x - video_rect.min.x) / video_rect.width()).clamp(0.0, 1.0);
                            let ny =
                                ((pos.y - video_rect.min.y) / video_rect.height()).clamp(0.0, 1.0);
                            events_to_send.push(InputEvent::MouseMove {
                                norm_x: nx,
                                norm_y: ny,
                            });
                        }
                        events_to_send.push(InputEvent::MouseButton {
                            button: map_pointer_btn(*button),
                            pressed: *pressed,
                        });
                    }
                }
                _ => state.keys.on_event(
                    ev,
                    &i.events[idx + 1..],
                    i.modifiers,
                    &key_down,
                    &lock_toggled,
                    &mut events_to_send,
                ),
            }
        }
        state.keys.end_frame(
            i.modifiers,
            window_focused,
            &held_now,
            special_keys,
            &mut events_to_send,
        );
    });

    let mut emitted_sync = false;
    for ev in &events_to_send {
        if let InputEvent::SyncLockKeys {
            caps_lock,
            num_lock,
            scroll_lock,
        } = ev
        {
            emitted_sync = true;
            tracing::info!(
                "viewer sync_locks: sent=({},{},{}) kbd=({},{},{}) gks=({},{},{}) kbd_ok={} egui_focused={} fg_owned={} focused={} lock_msgs={:?}",
                caps_lock,
                num_lock,
                scroll_lock,
                kbd_locks.0,
                kbd_locks.1,
                kbd_locks.2,
                gks_locks.0,
                gks_locks.1,
                gks_locks.2,
                kbd_ok,
                egui_focused,
                fg_owned,
                window_focused,
                lock_key_msgs
            );
        }
    }
    if !emitted_sync && !lock_key_msgs.is_empty() {
        tracing::info!(
            "viewer lock_key_msg without sync: lock_msgs={:?} kbd=({},{},{}) gks=({},{},{}) kbd_ok={} egui_focused={} fg_owned={} focused={} synced_locks={:?}",
            lock_key_msgs,
            kbd_locks.0,
            kbd_locks.1,
            kbd_locks.2,
            gks_locks.0,
            gks_locks.1,
            gks_locks.2,
            kbd_ok,
            egui_focused,
            fg_owned,
            window_focused,
            state.keys.synced_locks
        );
    }

    for ev in events_to_send {
        session.send(ViewerCommand::Input(ev));
    }
}

const VK_CANCEL: u16 = 0x03;
const VK_C: u16 = 0x43;
const VK_D: u16 = 0x44;
const VK_E: u16 = 0x45;
const VK_R: u16 = 0x52;
const VK_V: u16 = 0x56;
const VK_X: u16 = 0x58;
const VK_TAB: u16 = 0x09;
const VK_PAUSE: u16 = 0x13;
const VK_CAPITAL: u16 = 0x14;
const VK_SNAPSHOT: u16 = 0x2C;
const VK_INSERT: u16 = 0x2D;
const VK_DELETE: u16 = 0x2E;
const VK_LWIN: u16 = 0x5B;
const VK_APPS: u16 = 0x5D;
const VK_MULTIPLY: u16 = 0x6A;
const VK_NUMLOCK: u16 = 0x90;
const VK_SCROLL: u16 = 0x91;
const VK_LSHIFT: u16 = 0xA0;
const VK_LCONTROL: u16 = 0xA2;
const VK_LMENU: u16 = 0xA4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RawSpecialKey {
    vk: u16,
    scan_code: u16,
    extended: bool,
    pressed: bool,
}

fn is_hooked_special_vk(vk: u16) -> bool {
    matches!(
        vk,
        VK_SNAPSHOT | VK_PAUSE | VK_CANCEL | VK_APPS | VK_MULTIPLY
    )
}

fn special_key_extended(vk: u16) -> bool {
    matches!(vk, VK_SNAPSHOT | VK_CANCEL | VK_APPS)
}

fn resolve_special_scan_code(vk: u16, msg_scan: u16) -> u16 {
    match vk {
        // PrintScreen must use extended 0x37 (MapVirtualKeyW(0x2C) returns 0x54 SysRq).
        VK_SNAPSHOT => 0x37,
        // Pause must use canonical 0x45 (MapVirtualKeyW(0x13) returns 0).
        VK_PAUSE => 0x45,
        VK_CANCEL => {
            if msg_scan != 0 {
                msg_scan
            } else {
                0x46
            }
        }
        VK_APPS => {
            if msg_scan != 0 {
                msg_scan
            } else {
                0x5D
            }
        }
        VK_MULTIPLY => {
            if msg_scan != 0 {
                msg_scan
            } else {
                0x37
            }
        }
        _ => msg_scan,
    }
}

fn scan_code_for_vk(vk: u16) -> u16 {
    match vk {
        VK_SNAPSHOT => 0x37,
        VK_PAUSE => 0x45,
        VK_CANCEL => 0x46,
        _ => unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC) as u16 },
    }
}

fn resolve_window_focused(egui_focused: bool, fg_owned: bool, has_blur_ev: bool) -> bool {
    egui_focused || (fg_owned && !has_blur_ev)
}

fn current_thread_owns_foreground() -> bool {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() {
            return false;
        }
        GetWindowThreadProcessId(fg, None) == GetCurrentThreadId()
    }
}

fn read_viewer_lock_states() -> ((bool, bool, bool), (bool, bool, bool), bool) {
    crate::input::win_input::read_lock_toggles_raw()
}

thread_local! {
    static SPECIAL_KEY_QUEUE: RefCell<Vec<RawSpecialKey>> = const { RefCell::new(Vec::new()) };
    static LOCK_KEY_MSG_QUEUE: RefCell<Vec<(u16, bool)>> = const { RefCell::new(Vec::new()) };
}

unsafe extern "system" fn special_key_hook_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code >= 0 && (wparam.0 as u32) == PM_REMOVE.0 && lparam.0 != 0 {
        let msg = &*(lparam.0 as *const MSG);
        if matches!(
            msg.message,
            WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP
        ) {
            let vk = (msg.wParam.0 & 0xFFFF) as u16;
            let pressed = matches!(msg.message, WM_KEYDOWN | WM_SYSKEYDOWN);
            if is_hooked_special_vk(vk) {
                let msg_scan = (((msg.lParam.0 as usize) >> 16) & 0xFF) as u16;
                let item = RawSpecialKey {
                    vk,
                    scan_code: resolve_special_scan_code(vk, msg_scan),
                    extended: special_key_extended(vk),
                    pressed,
                };
                SPECIAL_KEY_QUEUE.with(|cell| {
                    if let Ok(mut q) = cell.try_borrow_mut() {
                        if q.len() < 256 {
                            q.push(item);
                        }
                    }
                });
            } else if matches!(vk, VK_CAPITAL | VK_NUMLOCK | VK_SCROLL) {
                // Capture lock-key messages solely for diagnostics; do not forward them as Key.
                LOCK_KEY_MSG_QUEUE.with(|cell| {
                    if let Ok(mut q) = cell.try_borrow_mut() {
                        if q.len() < 64 {
                            q.push((vk, pressed));
                        }
                    }
                });
            }
        }
    }
    CallNextHookEx(HHOOK::default(), code, wparam, lparam)
}

struct SpecialKeyHookGuard {
    hhook: isize,
    thread_id: u32,
}

impl SpecialKeyHookGuard {
    fn install_for_current_thread() -> Option<Self> {
        let tid = unsafe { GetCurrentThreadId() };
        let hhook = unsafe {
            SetWindowsHookExW(
                WH_GETMESSAGE,
                Some(special_key_hook_proc),
                HINSTANCE::default(),
                tid,
            )
        }
        .ok()?;
        Some(Self {
            hhook: hhook.0 as isize,
            thread_id: tid,
        })
    }
}

impl Drop for SpecialKeyHookGuard {
    fn drop(&mut self) {
        if self.hhook != 0 {
            unsafe {
                let _ = UnhookWindowsHookEx(HHOOK(self.hhook as *mut _));
            }
        }
        if unsafe { GetCurrentThreadId() } == self.thread_id {
            SPECIAL_KEY_QUEUE.with(|cell| {
                if let Ok(mut q) = cell.try_borrow_mut() {
                    q.clear();
                }
            });
            LOCK_KEY_MSG_QUEUE.with(|cell| {
                if let Ok(mut q) = cell.try_borrow_mut() {
                    q.clear();
                }
            });
        }
    }
}

fn take_captured_special_keys() -> Vec<RawSpecialKey> {
    SPECIAL_KEY_QUEUE.with(|cell| match cell.try_borrow_mut() {
        Ok(mut q) => std::mem::take(&mut *q),
        Err(_) => Vec::new(),
    })
}

fn take_captured_lock_key_msgs() -> Vec<(u16, bool)> {
    LOCK_KEY_MSG_QUEUE.with(|cell| match cell.try_borrow_mut() {
        Ok(mut q) => std::mem::take(&mut *q),
        Err(_) => Vec::new(),
    })
}

/// Turns the viewer's egui keyboard events into remote key events.
///
/// egui-winit never reports the key-*down* of a clipboard shortcut as `Event::Key`: for
/// Ctrl+C / Ctrl+X / Ctrl+V (and Ctrl+Insert, Shift+Insert, Shift+Delete) it emits
/// `Event::Copy` / `Cut` / `Paste` instead, and for a paste nothing at all when the *local*
/// clipboard holds no text (an image, files). Only the key-up arrives normally. Forwarding only
/// `Event::Key` sent the host `Ctrl↓ C↑ Ctrl↑`, so copy / cut / paste never happened on the remote
/// side (pitfall #21). Those key-downs are rebuilt here.
///
/// Modifiers are synced from each key event's own modifier state, so a modifier pressed in the
/// same frame as the key still reaches the host first. Lock keys (Caps/Num/Scroll Lock) are
/// synced from Win32 key state, and keys absent from `egui::Key` (PrintScreen, Pause/Ctrl+Break,
/// Apps, Numpad `*`) are captured by the UI-thread `WH_GETMESSAGE` hook.
#[derive(Default)]
struct KeyForwarder {
    /// Modifier state the host has been sent.
    sent_mods: Modifiers,
    /// Clipboard-shortcut keys (C, X, V, Insert, Delete) the host currently holds down.
    shortcut_down: Vec<u16>,
    /// Special keys absent from `egui::Key` currently held down on the host.
    special_down: Vec<(u16, u16, bool)>,
    /// Last lock-key toggle states `(caps, num, scroll)` synced to the host while focused.
    synced_locks: Option<(bool, bool, bool)>,
}

impl KeyForwarder {
    fn sync_locks(
        &mut self,
        window_focused: bool,
        lock_toggled: &dyn Fn(u16) -> bool,
        out: &mut Vec<InputEvent>,
    ) {
        if !window_focused {
            self.synced_locks = None;
            return;
        }
        let cur = (
            lock_toggled(VK_CAPITAL),
            lock_toggled(VK_NUMLOCK),
            lock_toggled(VK_SCROLL),
        );
        if self.synced_locks != Some(cur) {
            self.synced_locks = Some(cur);
            out.push(InputEvent::SyncLockKeys {
                caps_lock: cur.0,
                num_lock: cur.1,
                scroll_lock: cur.2,
            });
        }
    }

    /// Handles one keyboard-related event. `rest` holds the events after `ev` in this frame;
    /// `key_down(vk)` reads the Win32 key state of the UI thread.
    fn on_event(
        &mut self,
        ev: &Event,
        rest: &[Event],
        frame_mods: Modifiers,
        key_down: &dyn Fn(u16) -> bool,
        lock_toggled: &dyn Fn(u16) -> bool,
        out: &mut Vec<InputEvent>,
    ) {
        match ev {
            Event::Key {
                key,
                physical_key,
                pressed,
                modifiers,
                ..
            } => {
                // F11 toggles local fullscreen.
                if *key == Key::F11 {
                    return;
                }
                let Some((vk, ext)) = map_egui_key(*key, *physical_key) else {
                    return;
                };
                self.sync_modifiers(*modifiers, out);
                if is_shortcut_key(vk) {
                    if !*pressed
                        && !self.shortcut_down.contains(&vk)
                        && egui_swallows_press(vk, *modifiers)
                    {
                        // Its key-down produced no event at all (a paste with no text on the
                        // local clipboard, pressed and released between two frames): replay it.
                        out.push(key_event(vk, ext, true));
                    }
                    self.set_shortcut_down(vk, *pressed);
                }
                out.push(key_event(vk, ext, *pressed));
            }
            Event::Copy | Event::Cut | Event::Paste(_) => {
                let (letter, other) = match ev {
                    Event::Copy => (VK_C, VK_INSERT),
                    Event::Cut => (VK_X, VK_DELETE),
                    _ => (VK_V, VK_INSERT),
                };
                let candidates = [letter, other];
                // A fresh press is down right now; a held shortcut auto-repeats; a quick tap has
                // already been released later in this frame.
                let fresh = candidates
                    .into_iter()
                    .find(|vk| key_down(*vk) && !self.shortcut_down.contains(vk));
                let repeat = candidates
                    .into_iter()
                    .find(|vk| self.shortcut_down.contains(vk));
                let released_later = rest.iter().find_map(|e| match e {
                    Event::Key {
                        key,
                        physical_key,
                        pressed: false,
                        ..
                    } => map_egui_key(*key, *physical_key)
                        .map(|(vk, _)| vk)
                        .filter(|vk| candidates.contains(vk)),
                    _ => None,
                });
                let vk = fresh.or(repeat).or(released_later).unwrap_or(letter);

                // egui-winit only emits these while the shortcut's modifier is held; it may have
                // been released again within this frame.
                let mut mods = frame_mods;
                if vk == VK_DELETE || (vk == VK_INSERT && matches!(ev, Event::Paste(_))) {
                    mods.shift = true;
                } else {
                    mods.ctrl = true;
                    mods.command = true;
                }
                self.sync_modifiers(mods, out);
                let ext = shortcut_key_extended(vk);
                out.push(key_event(vk, ext, true));
                if fresh.is_some() || repeat.is_some() || released_later.is_some() {
                    self.set_shortcut_down(vk, true);
                } else {
                    // No key-up will follow (e.g. a dedicated Copy key): send a complete tap.
                    out.push(key_event(vk, ext, false));
                }
            }
            // winit already reports synthetic key-ups for held keys on focus loss; this catches
            // anything it missed so nothing stays down on the host.
            Event::WindowFocused(false) => {
                self.release_shortcut_keys(out);
                self.synced_locks = None;
            }
            Event::WindowFocused(true) => {
                self.sync_locks(true, lock_toggled, out);
            }
            _ => {}
        }
    }

    /// Runs after all events of a frame. `held_now(vk)`: the key is down right now.
    fn end_frame(
        &mut self,
        frame_mods: Modifiers,
        window_focused: bool,
        held_now: &dyn Fn(u16) -> bool,
        special_keys: &[RawSpecialKey],
        out: &mut Vec<InputEvent>,
    ) {
        self.sync_modifiers(frame_mods, out);
        if !window_focused {
            return;
        }
        // Ctrl+V / Shift+Insert with no text on the local clipboard produce no event at all: pick
        // the press up from the key state so the host pastes on key-down, as a real keyboard would.
        for vk in [VK_V, VK_INSERT] {
            let silent_paste = match vk {
                VK_V => frame_mods.command,
                _ => frame_mods.shift && !frame_mods.ctrl,
            };
            if silent_paste && !self.shortcut_down.contains(&vk) && held_now(vk) {
                self.set_shortcut_down(vk, true);
                out.push(key_event(vk, shortcut_key_extended(vk), true));
            }
        }
        self.process_special_keys(special_keys, out);
    }

    fn process_special_keys(&mut self, special_keys: &[RawSpecialKey], out: &mut Vec<InputEvent>) {
        for &sk in special_keys {
            if !is_hooked_special_vk(sk.vk) {
                continue;
            }
            let was_down = self.special_down.iter().any(|&(k, _, _)| k == sk.vk);
            if sk.vk == VK_SNAPSHOT {
                if sk.pressed {
                    if !was_down {
                        self.special_down.push((sk.vk, sk.scan_code, sk.extended));
                        out.push(key_event_with_scan(sk.vk, sk.scan_code, sk.extended, true));
                    }
                } else if was_down {
                    self.special_down.retain(|&(k, _, _)| k != sk.vk);
                    out.push(key_event_with_scan(sk.vk, sk.scan_code, sk.extended, false));
                } else {
                    // Windows omits WM_KEYDOWN for PrintScreen and emits only WM_KEYUP.
                    out.push(key_event_with_scan(sk.vk, sk.scan_code, sk.extended, true));
                    out.push(key_event_with_scan(sk.vk, sk.scan_code, sk.extended, false));
                }
                continue;
            }
            if sk.pressed {
                if !was_down {
                    self.special_down.push((sk.vk, sk.scan_code, sk.extended));
                    out.push(key_event_with_scan(sk.vk, sk.scan_code, sk.extended, true));
                } else if sk.vk == VK_MULTIPLY {
                    // Allow auto-repeat while Numpad `*` is held without duplicating release state.
                    out.push(key_event_with_scan(sk.vk, sk.scan_code, sk.extended, true));
                }
            } else if was_down {
                self.special_down.retain(|&(k, _, _)| k != sk.vk);
                out.push(key_event_with_scan(sk.vk, sk.scan_code, sk.extended, false));
            }
        }
    }

    fn combo_events(&mut self, keys: &[(u16, bool)], out: &mut Vec<InputEvent>) {
        self.sync_modifiers(Modifiers::NONE, out);
        out.extend(remote_combo_events(keys));
    }

    /// Releases everything the host holds (forwarding is about to pause).
    fn release_all(&mut self, out: &mut Vec<InputEvent>) {
        self.release_shortcut_keys(out);
        self.sync_modifiers(Modifiers::NONE, out);
        self.synced_locks = None;
    }

    fn release_shortcut_keys(&mut self, out: &mut Vec<InputEvent>) {
        for vk in std::mem::take(&mut self.shortcut_down) {
            out.push(key_event(vk, shortcut_key_extended(vk), false));
        }
        for (vk, scan_code, extended) in std::mem::take(&mut self.special_down) {
            out.push(key_event_with_scan(vk, scan_code, extended, false));
        }
    }

    fn sync_modifiers(&mut self, target: Modifiers, out: &mut Vec<InputEvent>) {
        let sent = self.sent_mods;
        let pairs = [
            (sent.ctrl, target.ctrl, VK_LCONTROL, false),
            (sent.shift, target.shift, VK_LSHIFT, false),
            (sent.alt, target.alt, VK_LMENU, false),
            // macOS Command key (always false on Windows, where `command` mirrors Ctrl).
            (sent.mac_cmd, target.mac_cmd, VK_LWIN, true),
        ];
        for (was, now, vk, ext) in pairs {
            if was != now {
                out.push(key_event(vk, ext, now));
            }
        }
        self.sent_mods = target;
    }

    fn set_shortcut_down(&mut self, vk: u16, down: bool) {
        self.shortcut_down.retain(|k| *k != vk);
        if down {
            self.shortcut_down.push(vk);
        }
    }
}

fn is_shortcut_key(vk: u16) -> bool {
    matches!(vk, VK_C | VK_X | VK_V | VK_INSERT | VK_DELETE)
}

fn shortcut_key_extended(vk: u16) -> bool {
    matches!(vk, VK_INSERT | VK_DELETE)
}

/// Mirrors egui-winit's `is_cut_command` / `is_copy_command` / `is_paste_command` on Windows:
/// whether a press of `vk` with `mods` is turned into Copy / Cut / Paste instead of a key event.
fn egui_swallows_press(vk: u16, mods: Modifiers) -> bool {
    match vk {
        VK_C | VK_X | VK_V => mods.command,
        VK_INSERT => mods.ctrl || mods.shift,
        VK_DELETE => mods.shift,
        _ => false,
    }
}

fn key_event_with_scan(vk: u16, scan_code: u16, extended: bool, pressed: bool) -> InputEvent {
    InputEvent::Key {
        vk,
        scan_code,
        extended,
        pressed,
    }
}

fn key_event(vk: u16, extended: bool, pressed: bool) -> InputEvent {
    key_event_with_scan(vk, scan_code_for_vk(vk), extended, pressed)
}

fn map_pointer_btn(btn: PointerButton) -> MouseBtn {
    match btn {
        PointerButton::Primary => MouseBtn::Left,
        PointerButton::Secondary => MouseBtn::Right,
        PointerButton::Middle => MouseBtn::Middle,
        PointerButton::Extra1 => MouseBtn::X1,
        PointerButton::Extra2 => MouseBtn::X2,
    }
}

fn map_egui_key(key: Key, physical_key: Option<Key>) -> Option<(u16, bool)> {
    if let Some(mapped) = map_key_variant(key, physical_key) {
        return Some(mapped);
    }
    let pk = physical_key?;
    if pk == Key::F11 {
        return None;
    }
    let mapped = map_key_variant(pk, Some(pk))?;
    if matches!(key, Key::Copy | Key::Cut | Key::Paste) && !is_shortcut_key(mapped.0) {
        return None;
    }
    Some(mapped)
}

fn map_key_variant(key: Key, physical_key: Option<Key>) -> Option<(u16, bool)> {
    let pair = match key {
        Key::A => (0x41, false),
        Key::B => (0x42, false),
        Key::C => (0x43, false),
        Key::D => (0x44, false),
        Key::E => (0x45, false),
        Key::F => (0x46, false),
        Key::G => (0x47, false),
        Key::H => (0x48, false),
        Key::I => (0x49, false),
        Key::J => (0x4A, false),
        Key::K => (0x4B, false),
        Key::L => (0x4C, false),
        Key::M => (0x4D, false),
        Key::N => (0x4E, false),
        Key::O => (0x4F, false),
        Key::P => (0x50, false),
        Key::Q => (0x51, false),
        Key::R => (0x52, false),
        Key::S => (0x53, false),
        Key::T => (0x54, false),
        Key::U => (0x55, false),
        Key::V => (0x56, false),
        Key::W => (0x57, false),
        Key::X => (0x58, false),
        Key::Y => (0x59, false),
        Key::Z => (0x5A, false),
        Key::Num0 => (0x30, false),
        Key::Num1 | Key::Exclamationmark => (0x31, false),
        Key::Num2 => (0x32, false),
        Key::Num3 => (0x33, false),
        Key::Num4 => (0x34, false),
        Key::Num5 => (0x35, false),
        Key::Num6 => (0x36, false),
        Key::Num7 => (0x37, false),
        Key::Num8 => (0x38, false),
        Key::Num9 => (0x39, false),
        Key::Enter => (0x0D, false),
        Key::Tab => (0x09, false),
        Key::Backspace => (0x08, false),
        Key::Escape => (0x1B, false),
        Key::Space => (0x20, false),
        Key::ArrowUp => (0x26, true),
        Key::ArrowDown => (0x28, true),
        Key::ArrowLeft => (0x25, true),
        Key::ArrowRight => (0x27, true),
        Key::Insert => (0x2D, true),
        Key::Delete => (0x2E, true),
        Key::Home => (0x24, true),
        Key::End => (0x23, true),
        Key::PageUp => (0x21, true),
        Key::PageDown => (0x22, true),
        Key::F1 => (0x70, false),
        Key::F2 => (0x71, false),
        Key::F3 => (0x72, false),
        Key::F4 => (0x73, false),
        Key::F5 => (0x74, false),
        Key::F6 => (0x75, false),
        Key::F7 => (0x76, false),
        Key::F8 => (0x77, false),
        Key::F9 => (0x78, false),
        Key::F10 => (0x79, false),
        Key::F12 => (0x7B, false),
        Key::F13 => (0x7C, false),
        Key::F14 => (0x7D, false),
        Key::F15 => (0x7E, false),
        Key::F16 => (0x7F, false),
        Key::F17 => (0x80, false),
        Key::F18 => (0x81, false),
        Key::F19 => (0x82, false),
        Key::F20 => (0x83, false),
        Key::F21 => (0x84, false),
        Key::F22 => (0x85, false),
        Key::F23 => (0x86, false),
        Key::F24 => (0x87, false),
        Key::Minus => (0xBD, false),
        Key::Equals => (0xBB, false),
        Key::Plus => {
            if physical_key == Some(Key::Plus) {
                (0x6B, false)
            } else {
                (0xBB, false)
            }
        }
        Key::OpenBracket | Key::OpenCurlyBracket => (0xDB, false),
        Key::CloseBracket | Key::CloseCurlyBracket => (0xDD, false),
        Key::Backslash | Key::Pipe => (0xDC, false),
        Key::Semicolon | Key::Colon => (0xBA, false),
        Key::Quote => (0xDE, false),
        Key::Backtick => (0xC0, false),
        Key::Comma => (0xBC, false),
        Key::Period => (0xBE, false),
        Key::Slash | Key::Questionmark => (0xBF, false),
        _ => return None,
    };
    Some(pair)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Modifier state as egui-winit reports it on Windows (`command` mirrors Ctrl).
    const NONE: Modifiers = Modifiers::NONE;
    const CTRL: Modifiers = Modifiers {
        alt: false,
        ctrl: true,
        shift: false,
        mac_cmd: false,
        command: true,
    };
    const SHIFT: Modifiers = Modifiers::SHIFT;

    fn key_phys(key: Key, physical_key: Option<Key>, pressed: bool, modifiers: Modifiers) -> Event {
        Event::Key {
            key,
            physical_key,
            pressed,
            repeat: false,
            modifiers,
        }
    }

    fn key(key: Key, pressed: bool, modifiers: Modifiers) -> Event {
        key_phys(key, None, pressed, modifiers)
    }

    fn frame_raw_special(
        fwd: &mut KeyForwarder,
        events: &[Event],
        mods: Modifiers,
        focused: bool,
        down: &[u16],
        locks: (bool, bool, bool),
        special_keys: &[RawSpecialKey],
    ) -> Vec<InputEvent> {
        let key_down = |vk: u16| down.contains(&vk);
        let lock_toggled = |vk: u16| match vk {
            VK_CAPITAL => locks.0,
            VK_NUMLOCK => locks.1,
            VK_SCROLL => locks.2,
            _ => false,
        };
        let mut out = Vec::new();
        fwd.sync_locks(focused, &lock_toggled, &mut out);
        for (idx, ev) in events.iter().enumerate() {
            fwd.on_event(
                ev,
                &events[idx + 1..],
                mods,
                &key_down,
                &lock_toggled,
                &mut out,
            );
        }
        fwd.end_frame(mods, focused, &key_down, special_keys, &mut out);
        out
    }

    fn frame_raw(
        fwd: &mut KeyForwarder,
        events: &[Event],
        mods: Modifiers,
        focused: bool,
        down: &[u16],
        locks: (bool, bool, bool),
    ) -> Vec<InputEvent> {
        frame_raw_special(fwd, events, mods, focused, down, locks, &[])
    }

    /// Feeds one egui frame through the forwarder; returns `(vk, pressed)` pairs sent to the host.
    fn frame(
        fwd: &mut KeyForwarder,
        events: &[Event],
        mods: Modifiers,
        down: &[u16],
    ) -> Vec<(u16, bool)> {
        fwd.synced_locks.get_or_insert((false, false, false));
        let out = frame_raw(fwd, events, mods, true, down, (false, false, false));
        pairs(out)
    }

    fn pairs(out: Vec<InputEvent>) -> Vec<(u16, bool)> {
        out.into_iter()
            .map(|e| match e {
                InputEvent::Key { vk, pressed, .. } => (vk, pressed),
                other => panic!("unexpected {other:?}"),
            })
            .collect()
    }

    #[test]
    fn shift_insert_without_local_text_is_picked_up_from_key_state() {
        let mut f = KeyForwarder::default();
        assert_eq!(
            frame(&mut f, &[], SHIFT, &[VK_INSERT]),
            [(VK_LSHIFT, true), (VK_INSERT, true)]
        );
    }

    #[test]
    fn copy_without_any_key_evidence_sends_a_complete_tap() {
        let mut f = KeyForwarder::default();
        assert_eq!(
            frame(&mut f, &[Event::Copy], CTRL, &[]),
            [(VK_LCONTROL, true), (VK_C, true), (VK_C, false)]
        );
        assert!(f.shortcut_down.is_empty(), "nothing may stay held");
    }

    #[test]
    fn focus_loss_and_paused_forwarding_release_held_keys() {
        let mut f = KeyForwarder::default();
        frame(&mut f, &[Event::Copy], CTRL, &[VK_C]);
        assert_eq!(
            frame(&mut f, &[Event::WindowFocused(false)], NONE, &[]),
            [(VK_C, false), (VK_LCONTROL, false)]
        );
        frame(&mut f, &[Event::Paste("t".into())], CTRL, &[VK_V]);
        let mut out = Vec::new();
        f.release_all(&mut out);
        assert_eq!(pairs(out), [(VK_V, false), (VK_LCONTROL, false)]);
        // No stale record left: the next Ctrl+V without local text is still picked up.
        assert_eq!(
            frame(&mut f, &[], CTRL, &[VK_V]),
            [(VK_LCONTROL, true), (VK_V, true)]
        );
    }

    #[test]
    fn ctrl_c_press_reaches_host() {
        let mut f = KeyForwarder::default();
        assert_eq!(frame(&mut f, &[], CTRL, &[]), [(VK_LCONTROL, true)]);
        // egui-winit swallows the C key-down and emits Copy instead.
        assert_eq!(frame(&mut f, &[Event::Copy], CTRL, &[VK_C]), [(VK_C, true)]);
        assert_eq!(
            frame(&mut f, &[key(Key::C, false, CTRL)], CTRL, &[]),
            [(VK_C, false)]
        );
        assert_eq!(frame(&mut f, &[], NONE, &[]), [(VK_LCONTROL, false)]);
    }

    #[test]
    fn ctrl_x_and_ctrl_v_with_text_reach_host() {
        let mut f = KeyForwarder::default();
        assert_eq!(
            frame(&mut f, &[Event::Cut], CTRL, &[VK_X]),
            [(VK_LCONTROL, true), (VK_X, true)]
        );
        assert_eq!(
            frame(&mut f, &[key(Key::X, false, CTRL)], CTRL, &[]),
            [(VK_X, false)]
        );
        assert_eq!(
            frame(&mut f, &[Event::Paste("local text".into())], CTRL, &[VK_V]),
            [(VK_V, true)]
        );
        assert_eq!(
            frame(&mut f, &[key(Key::V, false, CTRL)], CTRL, &[]),
            [(VK_V, false)]
        );
    }

    #[test]
    fn ctrl_v_without_local_text_is_picked_up_from_key_state() {
        // An image on the local clipboard: egui-winit emits no event for the V key-down at all.
        let mut f = KeyForwarder::default();
        assert_eq!(
            frame(&mut f, &[], CTRL, &[VK_V]),
            [(VK_LCONTROL, true), (VK_V, true)]
        );
        // Still held: no duplicate press.
        assert_eq!(frame(&mut f, &[], CTRL, &[VK_V]), []);
        assert_eq!(
            frame(&mut f, &[key(Key::V, false, CTRL)], CTRL, &[]),
            [(VK_V, false)]
        );
    }

    #[test]
    fn silent_paste_tapped_between_frames_is_replayed_at_key_up() {
        let mut f = KeyForwarder::default();
        assert_eq!(
            frame(&mut f, &[key(Key::V, false, CTRL)], CTRL, &[]),
            [(VK_LCONTROL, true), (VK_V, true), (VK_V, false)]
        );
    }

    #[test]
    fn whole_shortcut_within_one_frame_keeps_ctrl_around_the_press() {
        // Ctrl↓ C↓ Ctrl↑ C↑ all processed before the frame ran.
        let mut f = KeyForwarder::default();
        assert_eq!(
            frame(&mut f, &[Event::Copy, key(Key::C, false, NONE)], NONE, &[]),
            [
                (VK_LCONTROL, true),
                (VK_C, true),
                (VK_LCONTROL, false),
                (VK_C, false)
            ]
        );
    }

    #[test]
    fn insert_and_delete_shortcuts_send_the_real_key() {
        let mut f = KeyForwarder::default();
        assert_eq!(
            frame(&mut f, &[Event::Copy], CTRL, &[VK_INSERT]),
            [(VK_LCONTROL, true), (VK_INSERT, true)]
        );
        assert_eq!(
            frame(&mut f, &[key(Key::Insert, false, CTRL)], NONE, &[]),
            [(VK_INSERT, false), (VK_LCONTROL, false)]
        );
        assert_eq!(
            frame(&mut f, &[Event::Paste("t".into())], SHIFT, &[VK_INSERT]),
            [(VK_LSHIFT, true), (VK_INSERT, true)]
        );
        assert_eq!(
            frame(&mut f, &[key(Key::Insert, false, SHIFT)], SHIFT, &[]),
            [(VK_INSERT, false)]
        );
        assert_eq!(
            frame(&mut f, &[Event::Cut], SHIFT, &[VK_DELETE]),
            [(VK_DELETE, true)]
        );
        let mut out = Vec::new();
        f.on_event(&Event::Cut, &[], SHIFT, &|_| false, &|_| false, &mut out);
        let InputEvent::Key { extended, .. } = out[0] else {
            panic!("expected a key event");
        };
        assert!(extended, "Insert/Delete are extended keys");
    }

    #[test]
    fn held_shortcut_auto_repeats() {
        let mut f = KeyForwarder::default();
        frame(&mut f, &[Event::Copy], CTRL, &[VK_C]);
        assert_eq!(frame(&mut f, &[Event::Copy], CTRL, &[VK_C]), [(VK_C, true)]);
    }

    #[test]
    fn plain_typing_is_forwarded_once() {
        let mut f = KeyForwarder::default();
        assert_eq!(
            frame(
                &mut f,
                &[key(Key::C, true, NONE), key(Key::C, false, NONE)],
                NONE,
                &[VK_C]
            ),
            [(VK_C, true), (VK_C, false)]
        );
        // Plain V held down: egui reported it, so the key-state poll must not press it again.
        assert_eq!(
            frame(&mut f, &[key(Key::V, true, NONE)], CTRL, &[VK_V]),
            [(VK_V, true), (VK_LCONTROL, true)]
        );
    }

    #[test]
    fn modifier_pressed_in_the_same_frame_goes_first() {
        let mut f = KeyForwarder::default();
        assert_eq!(
            frame(&mut f, &[key(Key::A, true, CTRL)], CTRL, &[]),
            [(VK_LCONTROL, true), (0x41, true)]
        );
    }

    #[test]
    fn releasing_ctrl_sends_no_windows_key() {
        let mut f = KeyForwarder::default();
        frame(&mut f, &[], CTRL, &[]);
        assert_eq!(frame(&mut f, &[], NONE, &[]), [(VK_LCONTROL, false)]);
    }

    #[test]
    fn unfocused_window_does_not_poll_key_state() {
        let mut f = KeyForwarder::default();
        let mut out = Vec::new();
        f.end_frame(CTRL, false, &|vk| vk == VK_V, &[], &mut out);
        assert_eq!(out.len(), 1, "only the Ctrl sync, no V press: {out:?}");
    }

    #[test]
    fn shifted_punctuation_numpad_plus_and_f13_to_f24_are_forwarded() {
        let mut f = KeyForwarder::default();
        // Shift+1 (!), even if Shift is released before 1, sends matching VK 0x31 down and up.
        assert_eq!(
            frame(
                &mut f,
                &[key_phys(Key::Exclamationmark, Some(Key::Num1), true, SHIFT)],
                SHIFT,
                &[]
            ),
            [(VK_LSHIFT, true), (0x31, true)]
        );
        assert_eq!(
            frame(
                &mut f,
                &[key_phys(Key::Num1, Some(Key::Num1), false, NONE)],
                NONE,
                &[]
            ),
            [(VK_LSHIFT, false), (0x31, false)]
        );

        let shifted_syms = [
            (Key::Colon, Key::Semicolon, 0xBA),
            (Key::Pipe, Key::Backslash, 0xDC),
            (Key::Questionmark, Key::Slash, 0xBF),
            (Key::OpenCurlyBracket, Key::OpenBracket, 0xDB),
            (Key::CloseCurlyBracket, Key::CloseBracket, 0xDD),
            (Key::Plus, Key::Equals, 0xBB),
        ];
        for (logical, phys, expected_vk) in shifted_syms {
            assert_eq!(
                frame(
                    &mut f,
                    &[
                        key_phys(logical, Some(phys), true, SHIFT),
                        key_phys(logical, Some(phys), false, SHIFT),
                    ],
                    SHIFT,
                    &[]
                ),
                [(VK_LSHIFT, true), (expected_vk, true), (expected_vk, false)]
            );
            assert_eq!(frame(&mut f, &[], NONE, &[]), [(VK_LSHIFT, false)]);
        }

        // Numpad '+' reports physical_key = Some(Key::Plus) and maps to VK_ADD (0x6B).
        assert_eq!(
            frame(
                &mut f,
                &[
                    key_phys(Key::Plus, Some(Key::Plus), true, NONE),
                    key_phys(Key::Plus, Some(Key::Plus), false, NONE),
                ],
                NONE,
                &[]
            ),
            [(0x6B, true), (0x6B, false)]
        );

        // F13..F24 map cleanly to VK_F13..VK_F24 (0x7C..=0x87), while F11 stays local.
        let fkeys = [
            (Key::F13, 0x7C),
            (Key::F14, 0x7D),
            (Key::F15, 0x7E),
            (Key::F16, 0x7F),
            (Key::F17, 0x80),
            (Key::F18, 0x81),
            (Key::F19, 0x82),
            (Key::F20, 0x83),
            (Key::F21, 0x84),
            (Key::F22, 0x85),
            (Key::F23, 0x86),
            (Key::F24, 0x87),
        ];
        for (k, expected_vk) in fkeys {
            assert_eq!(
                frame(&mut f, &[key(k, true, NONE)], NONE, &[]),
                [(expected_vk, true)]
            );
        }
        assert_eq!(frame(&mut f, &[key(Key::F11, true, NONE)], NONE, &[]), []);
    }

    #[test]
    fn lock_keys_sync_before_typing_and_resync_after_focus_loss() {
        let mut f = KeyForwarder::default();
        // First focused frame with Caps Lock ON and Num Lock ON syncs locks BEFORE the key press.
        let out = frame_raw(
            &mut f,
            &[key(Key::A, true, NONE)],
            NONE,
            true,
            &[],
            (true, true, false),
        );
        assert_eq!(
            out[0],
            InputEvent::SyncLockKeys {
                caps_lock: true,
                num_lock: true,
                scroll_lock: false,
            }
        );
        assert!(matches!(
            out[1],
            InputEvent::Key {
                vk: 0x41,
                pressed: true,
                ..
            }
        ));

        // Same lock state next frame emits no redundant SyncLockKeys.
        let out2 = frame_raw(&mut f, &[], NONE, true, &[], (true, true, false));
        assert!(out2.is_empty(), "expected no events, got {out2:?}");

        // Toggling Caps Lock OFF syncs immediately.
        let out3 = frame_raw(
            &mut f,
            &[key(Key::B, true, NONE)],
            NONE,
            true,
            &[],
            (false, true, false),
        );
        assert_eq!(
            out3[0],
            InputEvent::SyncLockKeys {
                caps_lock: false,
                num_lock: true,
                scroll_lock: false,
            }
        );

        // Focus loss clears cached lock state; refocusing re-syncs even if toggled outside window.
        let _ = frame_raw(
            &mut f,
            &[Event::WindowFocused(false)],
            NONE,
            false,
            &[],
            (false, true, false),
        );
        let out4 = frame_raw(
            &mut f,
            &[Event::WindowFocused(true)],
            NONE,
            true,
            &[],
            (true, false, true),
        );
        assert_eq!(
            out4,
            vec![InputEvent::SyncLockKeys {
                caps_lock: true,
                num_lock: false,
                scroll_lock: true,
            }]
        );
    }

    fn key_tuples(out: Vec<InputEvent>) -> Vec<(u16, u16, bool, bool)> {
        out.into_iter()
            .map(|e| match e {
                InputEvent::Key {
                    vk,
                    scan_code,
                    extended,
                    pressed,
                } => (vk, scan_code, extended, pressed),
                other => panic!("unexpected {other:?}"),
            })
            .collect()
    }

    #[test]
    fn special_keys_forward_snapshot_tap_pause_cancel_and_release_on_blur() {
        let mut f = KeyForwarder::default();
        f.synced_locks = Some((false, false, false));

        // 1. Windows PrintScreen omits WM_KEYDOWN and only fires WM_KEYUP: forward a full tap
        //    with extended scan code 0x37 (never 0x54 SysRq).
        let snap_up_only = RawSpecialKey {
            vk: VK_SNAPSHOT,
            scan_code: resolve_special_scan_code(VK_SNAPSHOT, 0x54),
            extended: special_key_extended(VK_SNAPSHOT),
            pressed: false,
        };
        assert_eq!(
            key_tuples(frame_raw_special(
                &mut f,
                &[],
                NONE,
                true,
                &[],
                (false, false, false),
                &[snap_up_only],
            )),
            [
                (VK_SNAPSHOT, 0x37, true, true),
                (VK_SNAPSHOT, 0x37, true, false),
            ]
        );

        // 2. If a driver DOES emit WM_KEYDOWN + WM_KEYUP for VK_SNAPSHOT, do not double-press.
        let snap_down = RawSpecialKey {
            pressed: true,
            ..snap_up_only
        };
        assert_eq!(
            key_tuples(frame_raw_special(
                &mut f,
                &[],
                NONE,
                true,
                &[],
                (false, false, false),
                &[snap_down, snap_up_only],
            )),
            [
                (VK_SNAPSHOT, 0x37, true, true),
                (VK_SNAPSHOT, 0x37, true, false),
            ]
        );

        // 3. Rapid Pause make+break pulse in a single frame sends scan_code 0x45 cleanly.
        let pause_down = RawSpecialKey {
            vk: VK_PAUSE,
            scan_code: resolve_special_scan_code(VK_PAUSE, 0),
            extended: special_key_extended(VK_PAUSE),
            pressed: true,
        };
        let pause_up = RawSpecialKey {
            pressed: false,
            ..pause_down
        };
        assert_eq!(
            key_tuples(frame_raw_special(
                &mut f,
                &[],
                NONE,
                true,
                &[],
                (false, false, false),
                &[pause_down, pause_up],
            )),
            [(VK_PAUSE, 0x45, false, true), (VK_PAUSE, 0x45, false, false)]
        );

        // 4. Ctrl+Pause/Break emits VK_CANCEL (0x03, scan 0x46, extended=true) after syncing Ctrl.
        let cancel_down = RawSpecialKey {
            vk: VK_CANCEL,
            scan_code: resolve_special_scan_code(VK_CANCEL, 0),
            extended: special_key_extended(VK_CANCEL),
            pressed: true,
        };
        let cancel_up = RawSpecialKey {
            pressed: false,
            ..cancel_down
        };
        assert_eq!(
            pairs(frame_raw_special(
                &mut f,
                &[],
                CTRL,
                true,
                &[],
                (false, false, false),
                &[cancel_down, cancel_up],
            )),
            [(VK_LCONTROL, true), (VK_CANCEL, true), (VK_CANCEL, false)]
        );
        // Release Ctrl.
        assert_eq!(
            pairs(frame_raw_special(
                &mut f,
                &[],
                NONE,
                true,
                &[],
                (false, false, false),
                &[],
            )),
            [(VK_LCONTROL, false)]
        );

        // 5. Orphaned key-up (without tracked press) for VK_APPS is ignored; unfocused drops keys.
        let apps_up = RawSpecialKey {
            vk: VK_APPS,
            scan_code: resolve_special_scan_code(VK_APPS, 0),
            extended: true,
            pressed: false,
        };
        let apps_down = RawSpecialKey {
            pressed: true,
            ..apps_up
        };
        let mult_down = RawSpecialKey {
            vk: VK_MULTIPLY,
            scan_code: resolve_special_scan_code(VK_MULTIPLY, 0),
            extended: false,
            pressed: true,
        };
        assert_eq!(
            pairs(frame_raw_special(
                &mut f,
                &[],
                NONE,
                true,
                &[],
                (false, false, false),
                &[apps_up],
            )),
            []
        );
        assert_eq!(
            pairs(frame_raw_special(
                &mut f,
                &[],
                NONE,
                false,
                &[],
                (false, false, false),
                &[apps_down, mult_down],
            )),
            []
        );

        // 6. Held VK_APPS & VK_MULTIPLY are released cleanly on focus loss (preserving scan codes).
        f.synced_locks = Some((false, false, false));
        assert_eq!(
            key_tuples(frame_raw_special(
                &mut f,
                &[],
                NONE,
                true,
                &[],
                (false, false, false),
                &[apps_down, mult_down],
            )),
            [(VK_APPS, 0x5D, true, true), (VK_MULTIPLY, 0x37, false, true)]
        );
        assert_eq!(
            key_tuples(frame_raw_special(
                &mut f,
                &[Event::WindowFocused(false)],
                NONE,
                false,
                &[],
                (false, false, false),
                &[],
            )),
            [
                (VK_APPS, 0x5D, true, false),
                (VK_MULTIPLY, 0x37, false, false),
            ]
        );
    }

    #[test]
    fn win32_wh_getmessage_hook_captures_special_keys_without_pm_noremove_dupes() {
        use windows::core::w;
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, PeekMessageW, PostMessageW, HWND_MESSAGE,
            PM_NOREMOVE,
        };

        struct TestHwnd(HWND);
        impl Drop for TestHwnd {
            fn drop(&mut self) {
                if !self.0 .0.is_null() {
                    unsafe {
                        let _ = DestroyWindow(self.0);
                    }
                }
            }
        }

        fn lparam_key(scan: u16, extended: bool, up: bool) -> LPARAM {
            let mut bits = 1usize | (usize::from(scan) << 16);
            if extended {
                bits |= 1 << 24;
            }
            if up {
                bits |= (1 << 30) | (1 << 31);
            }
            LPARAM(bits as isize)
        }

        let captured = std::thread::spawn(|| {
            let _ = take_captured_special_keys();
            let hwnd = unsafe {
                CreateWindowExW(
                    Default::default(),
                    w!("STATIC"),
                    w!("flashdesk_hook_test"),
                    Default::default(),
                    0,
                    0,
                    0,
                    0,
                    HWND_MESSAGE,
                    None,
                    None,
                    None,
                )
            }
            .expect("CreateWindowExW(HWND_MESSAGE) failed");
            let _win_guard = TestHwnd(hwnd);

            let guard = SpecialKeyHookGuard::install_for_current_thread()
                .expect("SetWindowsHookExW(WH_GETMESSAGE) failed");

            unsafe {
                // Normal key 'A' should be ignored by the hook.
                PostMessageW(hwnd, WM_KEYDOWN, WPARAM(0x41), lparam_key(0x1E, false, false))
                    .unwrap();
                // Windows PrintScreen: WM_KEYUP only with SysRq scan 0x54 -> normalized to 0x37.
                PostMessageW(
                    hwnd,
                    WM_KEYUP,
                    WPARAM(usize::from(VK_SNAPSHOT)),
                    lparam_key(0x54, true, true),
                )
                .unwrap();
                // Pause rapid pulse with zero scan code -> normalized to 0x45.
                PostMessageW(
                    hwnd,
                    WM_KEYDOWN,
                    WPARAM(usize::from(VK_PAUSE)),
                    lparam_key(0, false, false),
                )
                .unwrap();
                PostMessageW(
                    hwnd,
                    WM_KEYUP,
                    WPARAM(usize::from(VK_PAUSE)),
                    lparam_key(0, false, true),
                )
                .unwrap();
                // Ctrl+Break -> VK_CANCEL.
                PostMessageW(
                    hwnd,
                    WM_KEYDOWN,
                    WPARAM(usize::from(VK_CANCEL)),
                    lparam_key(0x46, true, false),
                )
                .unwrap();
                PostMessageW(
                    hwnd,
                    WM_KEYUP,
                    WPARAM(usize::from(VK_CANCEL)),
                    lparam_key(0x46, true, true),
                )
                .unwrap();
                // Apps & NumpadMultiply.
                PostMessageW(
                    hwnd,
                    WM_KEYDOWN,
                    WPARAM(usize::from(VK_APPS)),
                    lparam_key(0x5D, true, false),
                )
                .unwrap();
                PostMessageW(
                    hwnd,
                    WM_KEYUP,
                    WPARAM(usize::from(VK_MULTIPLY)),
                    lparam_key(0x37, false, true),
                )
                .unwrap();

                // PM_NOREMOVE must NOT enqueue duplicates.
                let mut msg = MSG::default();
                for _ in 0..3 {
                    let _ = PeekMessageW(&mut msg, hwnd, 0, 0, PM_NOREMOVE);
                }
                assert!(take_captured_special_keys().is_empty());

                while PeekMessageW(&mut msg, hwnd, 0, 0, PM_REMOVE).as_bool() {}
            }
            let first_batch = take_captured_special_keys();

            // Unhook removes hook cleanly: subsequent messages produce zero captured events.
            drop(guard);
            unsafe {
                PostMessageW(
                    hwnd,
                    WM_KEYUP,
                    WPARAM(usize::from(VK_SNAPSHOT)),
                    lparam_key(0x37, true, true),
                )
                .unwrap();
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, hwnd, 0, 0, PM_REMOVE).as_bool() {}
            }
            let after_unhook = take_captured_special_keys();
            (first_batch, after_unhook)
        })
        .join()
        .expect("hook test thread panicked");

        assert!(captured.1.is_empty(), "hook kept firing after Drop");
        assert_eq!(
            captured.0,
            vec![
                RawSpecialKey {
                    vk: VK_SNAPSHOT,
                    scan_code: 0x37,
                    extended: true,
                    pressed: false,
                },
                RawSpecialKey {
                    vk: VK_PAUSE,
                    scan_code: 0x45,
                    extended: false,
                    pressed: true,
                },
                RawSpecialKey {
                    vk: VK_PAUSE,
                    scan_code: 0x45,
                    extended: false,
                    pressed: false,
                },
                RawSpecialKey {
                    vk: VK_CANCEL,
                    scan_code: 0x46,
                    extended: true,
                    pressed: true,
                },
                RawSpecialKey {
                    vk: VK_CANCEL,
                    scan_code: 0x46,
                    extended: true,
                    pressed: false,
                },
                RawSpecialKey {
                    vk: VK_APPS,
                    scan_code: 0x5D,
                    extended: true,
                    pressed: true,
                },
                RawSpecialKey {
                    vk: VK_MULTIPLY,
                    scan_code: 0x37,
                    extended: false,
                    pressed: false,
                },
            ]
        );
    }

    #[test]
    fn remote_combo_events_press_forward_and_release_reverse() {
        let evs = remote_combo_events(&[(VK_LWIN, true), (VK_D, false)]);
        assert_eq!(
            key_tuples(evs),
            vec![
                (VK_LWIN, scan_code_for_vk(VK_LWIN), true, true),
                (VK_D, scan_code_for_vk(VK_D), false, true),
                (VK_D, scan_code_for_vk(VK_D), false, false),
                (VK_LWIN, scan_code_for_vk(VK_LWIN), true, false),
            ]
        );
    }

    #[test]
    fn remote_combo_releases_held_modifiers_before_sending_combo() {
        let mut f = KeyForwarder::default();
        f.synced_locks = Some((false, false, false));
        let _ = frame(&mut f, &[], CTRL, &[]);
        assert!(f.sent_mods.ctrl);

        let mut combo = Vec::new();
        f.combo_events(&[(VK_LWIN, true), (VK_D, false)], &mut combo);
        assert_eq!(
            pairs(combo),
            vec![
                (VK_LCONTROL, false),
                (VK_LWIN, true),
                (VK_D, true),
                (VK_D, false),
                (VK_LWIN, false),
            ]
        );
        assert_eq!(f.sent_mods, NONE);
    }

    #[test]
    fn copy_cut_paste_with_non_shortcut_physical_key_returns_none() {
        assert_eq!(map_egui_key(Key::Copy, Some(Key::F13)), None);
        assert_eq!(map_egui_key(Key::Cut, Some(Key::A)), None);
        assert_eq!(map_egui_key(Key::Paste, Some(Key::B)), None);
        // Physical keys matching actual clipboard shortcut keys still map cleanly.
        assert_eq!(map_egui_key(Key::Copy, Some(Key::C)), Some((VK_C, false)));
        assert_eq!(
            map_egui_key(Key::Paste, Some(Key::Insert)),
            Some((VK_INSERT, true))
        );
    }

    #[test]
    fn tools_menu_renders_within_250px_popover() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(640.0, 440.0))),
            ..Default::default()
        };
        let mut clipboard_enabled = true;
        let mut scale_mode = ScaleMode::Fit;
        let _ = ctx.run(raw, |ctx| {
            egui::Area::new(egui::Id::new("test_tools_popover"))
                .fixed_pos(pos2(10.0, 10.0))
                .show(ctx, |ui| {
                    ui.set_width(250.0);
                    let _ = render_tools_menu_body(ui, &mut clipboard_enabled, &mut scale_mode);
                    assert!(
                        ui.min_rect().width() <= 252.0,
                        "tools popover overflowed 250px: {}",
                        ui.min_rect().width()
                    );
                });
        });
    }

    fn test_entry(name: &str, path: &str, is_dir: bool) -> FileEntry {
        FileEntry {
            name: name.into(),
            path: path.into(),
            is_dir,
            size: 0,
            modified_unix: 0,
        }
    }

    #[test]
    fn icon_classification_distinguishes_root_shortcuts_from_normal_folders() {
        let desktop = test_entry("桌面", r"C:\Users\alice\Desktop", true);
        let docs = test_entry("文档", r"C:\Users\alice\Documents", true);
        let dl = test_entry("下载", r"C:\Users\alice\Downloads", true);
        let redirected_dl = test_entry("下载", r"D:\", true);
        let drive = test_entry("娱乐 (D:)", r"D:\", true);
        let file = test_entry("notes.txt", r"D:\notes.txt", false);

        assert_eq!(classify_entry_icon(&desktop, true), EntryIcon::Desktop);
        assert_eq!(classify_entry_icon(&docs, true), EntryIcon::Documents);
        assert_eq!(classify_entry_icon(&dl, true), EntryIcon::Downloads);
        // Even when redirected to a drive root, the root shortcut keeps its shortcut icon & group.
        assert_eq!(
            classify_entry_icon(&redirected_dl, true),
            EntryIcon::Downloads
        );
        assert!(is_quick_folder_entry(&redirected_dl));
        assert!(!is_quick_folder_entry(&drive));
        assert_eq!(classify_entry_icon(&drive, true), EntryIcon::Drive);
        assert_eq!(classify_entry_icon(&file, true), EntryIcon::File);

        // Inside a normal subdirectory, a folder literally named "桌面" stays a normal folder.
        let sub_desktop = test_entry("桌面", r"D:\backup\桌面", true);
        assert_eq!(classify_entry_icon(&sub_desktop, false), EntryIcon::Folder);
        assert_eq!(classify_entry_icon(&drive, false), EntryIcon::Drive);
    }

    #[test]
    fn local_dir_draft_input_does_not_corrupt_active_dir() {
        let mut st = RemoteViewState::new(60, 4000, 0, None);
        st.refresh_local_dir();
        assert!(st.local_dir.is_empty());
        assert!(!st.local_roots.is_empty());

        let desktop = st
            .local_roots
            .iter()
            .find(|e| e.name == "桌面")
            .expect("desktop root missing")
            .path
            .clone();
        st.navigate_local_dir(desktop.clone());
        assert_eq!(st.local_dir, desktop);
        assert_eq!(st.local_dir_input, desktop);
        assert!(st.local_error.is_none());

        // Typing an uncommitted draft leaves active local_dir intact.
        st.local_dir_input = String::new();
        assert_eq!(st.local_dir, desktop);

        // Navigating to an invalid path preserves the active directory and entries.
        let prev_entries = st.local_entries.clone();
        let bad = r"Z:\__definitely_missing_flashdesk_dir__".to_string();
        st.navigate_local_dir(bad.clone());
        assert_eq!(st.local_dir, desktop);
        assert_eq!(st.local_dir_input, bad);
        assert_eq!(st.local_entries, prev_entries);
        assert!(st.local_error.is_some());
    }

    #[test]
    fn paths_equal_ci_matches_windows_paths() {
        assert!(paths_equal_ci(r"C:\Users\Alice\Desktop", r"c:/users/alice/desktop\"));
        assert!(paths_equal_ci(r"D:", r"d:\"));
        assert!(!paths_equal_ci("", r"C:\"));
        assert!(!paths_equal_ci(r"C:\Users\Alice", r"D:\Users\Alice"));
    }

    #[test]
    fn shortcut_dropdown_renders_cleanly_at_desktop_and_min_window() {
        // Include a redirected "下载" pointing to D:\ alongside "娱乐 (D:)" to verify zero ID collision.
        let roots = vec![
            test_entry("桌面", r"C:\Users\alice\Desktop", true),
            test_entry("文档", r"C:\Users\alice\Documents", true),
            test_entry("下载", r"D:\", true),
            test_entry("本地磁盘 (C:)", r"C:\", true),
            test_entry("娱乐 (D:)", r"D:\", true),
            test_entry("软件 (E:)", r"E:\", true),
            test_entry("文件 (F:)", r"F:\", true),
        ];

        for (w, h) in [(960.0_f32, 640.0_f32), (640.0_f32, 440.0_f32)] {
            let ctx = egui::Context::default();
            theme::install(&ctx);
            let mut path_input = r"D:\".to_string();
            let cur_dir = path_input.clone();

            let mut captured_pid = None;
            let mut anchor_bottom = 0.0_f32;
            let mut last_navigated = None;

            for pass in 0..3 {
                let mut events = Vec::new();
                if pass == 2 {
                    let pid = captured_pid.expect("popup id missing");
                    let popup_rect = ctx
                        .memory(|m| m.area_rect(pid))
                        .expect("popup area_rect missing after render");
                    assert!(
                        popup_rect.bottom() <= h + 1.0,
                        "popup exceeded screen bottom at {w}x{h}: {} > {h}",
                        popup_rect.bottom()
                    );
                    assert!(
                        popup_rect.top() >= anchor_bottom - 2.0,
                        "popup pushed up over toggle button at {w}x{h}: {} < {anchor_bottom}",
                        popup_rect.top()
                    );
                    let click_pos = pos2(popup_rect.left() + 40.0, popup_rect.top() + 22.0);
                    events.push(Event::PointerMoved(click_pos));
                    events.push(Event::PointerButton {
                        pos: click_pos,
                        button: PointerButton::Primary,
                        pressed: true,
                        modifiers: Modifiers::NONE,
                    });
                    events.push(Event::PointerButton {
                        pos: click_pos,
                        button: PointerButton::Primary,
                        pressed: false,
                        modifiers: Modifiers::NONE,
                    });
                }

                let raw = egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(w, h))),
                    events,
                    ..Default::default()
                };
                let _ = ctx.run(raw, |ctx| {
                    egui::Window::new("文件传输")
                        .default_size(vec2(w - 24.0, h - 72.0))
                        .show(ctx, |ui| {
                            ui.columns(2, |cols| {
                                for (idx, col) in cols.iter_mut().enumerate() {
                                    let avail_w = col.available_width();
                                    let resp = col.push_id(idx, |ui| {
                                        let id_salt = if idx == 0 {
                                            "local_path_bar"
                                        } else {
                                            "remote_path_bar"
                                        };
                                        let pid =
                                            ui.make_persistent_id((id_salt, "shortcuts_popup"));
                                        if pass == 0 && idx == 0 {
                                            captured_pid = Some(pid);
                                            ui.memory_mut(|m| m.open_popup(pid));
                                        }
                                        let nav = path_bar_with_shortcuts(
                                            ui,
                                            id_salt,
                                            &mut path_input,
                                            &cur_dir,
                                            &roots,
                                            false,
                                        );
                                        if idx == 0 && nav.is_some() {
                                            last_navigated = nav;
                                        }
                                    });
                                    if idx == 0 {
                                        anchor_bottom = resp.response.rect.bottom();
                                    }
                                    assert!(
                                        resp.response.rect.width() <= avail_w + 1.0,
                                        "column {idx} overflowed at {w}x{h}: {} > {}",
                                        resp.response.rect.width(),
                                        avail_w
                                    );
                                }
                            });
                        });
                });
            }
            assert_eq!(
                last_navigated,
                Some(String::new()),
                "clicking first dropdown row ('此电脑') should navigate to root at {w}x{h}"
            );
            assert!(
                !ctx.memory(|m| m.is_popup_open(captured_pid.unwrap())),
                "popup should close after picking an item at {w}x{h}"
            );
        }
    }

    #[test]
    fn resolve_window_focused_recovers_stale_egui_focus_but_respects_blur() {
        assert!(resolve_window_focused(false, true, false));
        assert!(!resolve_window_focused(false, true, true));
        assert!(!resolve_window_focused(false, false, false));
        assert!(resolve_window_focused(true, false, false));
    }

    #[test]
    fn viewer_lock_state_read_succeeds_without_panicking() {
        let (eff, gks, kbd_ok) = read_viewer_lock_states();
        assert!(kbd_ok, "expected GetKeyboardState to succeed on desktop thread");
        assert_eq!(
            eff,
            gks,
            "viewer GetKeyboardState must match GetKeyState on a fresh thread (catches windows-0.58 as_ptr readonly UB under opt-level=3)"
        );
    }
}
