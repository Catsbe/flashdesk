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
use std::path::{Path, PathBuf};
use windows::Win32::UI::Input::KeyboardAndMouse::{MapVirtualKeyW, MAPVK_VK_TO_VSC};

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
    prev_mods: Modifiers,

    // M2 + M3 interactive overlays
    clipboard_enabled: bool,
    show_file_panel: bool,
    file_panel_initialized: bool,
    show_vdd_window: bool,
    local_dir: String,
    local_entries: Vec<FileEntry>,
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
            show_stats: true,
            bitrate_mbps: (initial_bitrate_kbps as f32 / 1000.0).clamp(1.0, 50.0),
            target_fps: initial_fps,
            res_cap_h: initial_res_cap_h,
            last_norm_pos: None,
            prev_mods: Modifiers::NONE,
            clipboard_enabled: true,
            show_file_panel: false,
            file_panel_initialized: false,
            show_vdd_window: false,
            local_dir: String::new(),
            local_entries: Vec::new(),
            local_error: None,
            remote_dir_input: String::new(),
            last_synced_remote_dir: String::new(),
            selected_file_relay_id: initial_file_relay_id,
            drop_notice: None,
        }
    }

    fn refresh_local_dir(&mut self) {
        match list_local_dir(&self.local_dir) {
            Ok(list) => {
                self.local_entries = list;
                self.local_error = None;
            }
            Err(e) => {
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
}

pub fn render_remote_view(
    ctx: &egui::Context,
    state: &mut RemoteViewState,
    session: &ViewerSessionHandle,
) -> RemoteViewAction {
    let mut action = RemoteViewAction::None;
    let mut go_back = false;

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
                present_video(ctx, ui, state, session, tex_id, video_rect, &resp, alive);
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
                        present_video(ctx, ui, state, session, tex_id, video_rect, &resp, alive);
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
                            150.0,
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

    // Live stats (top right, below the toolbar). On small windows only the headline numbers,
    // so the panel doesn't cover a big part of the remote screen.
    if state.show_stats {
        let screen = ctx.screen_rect();
        let compact = screen.width() < 800.0 || screen.height() < 500.0;
        egui::Area::new(egui::Id::new("remote_hud"))
            .anchor(Align2::RIGHT_TOP, vec2(-12.0, 62.0))
            .interactable(false)
            .show(ctx, |ui| hud_panel(ui, &hud, compact));
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
    if !overlays_open {
        forward_inputs(ctx, state, session, video_rect, pointer_in_video);
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

fn tools_menu(ui: &mut egui::Ui, state: &mut RemoteViewState, session: &ViewerSessionHandle) {
    egui::Grid::new("tools_grid")
        .num_columns(2)
        .spacing([14.0, 10.0])
        .show(ui, |ui| {
            ui.label(menu_label("剪贴板同步"));
            if theme::toggle(ui, &mut state.clipboard_enabled).changed() {
                session.send(ViewerCommand::SetClipboardEnabled(state.clipboard_enabled));
            }
            ui.end_row();

            ui.label(menu_label("性能统计"));
            theme::toggle(ui, &mut state.show_stats);
            ui.end_row();

            ui.label(menu_label("画面缩放"));
            theme::segmented(
                ui,
                &mut state.scale_mode,
                &[(ScaleMode::Fit, "适应窗口"), (ScaleMode::Original, "1:1")],
            );
            ui.end_row();
        });
    theme::divider(ui);
    if theme::button(ui, "远程虚拟显示器…", Kind::Ghost).clicked() {
        state.show_vdd_window = true;
        session.send(ViewerCommand::VddAction(VddRequest::Query));
        ui.memory_mut(|mem| mem.close_popup());
    }
    if theme::button(ui, "发送 Ctrl+Alt+Del", Kind::Ghost).clicked() {
        session.send(ViewerCommand::Input(InputEvent::SendSas));
        ui.memory_mut(|mem| mem.close_popup());
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

/// One file / folder row: click a folder to open it, the button transfers a file.
fn entry_row(ui: &mut egui::Ui, entry: &FileEntry, transfer_label: &str) -> RowAction {
    let mut act = RowAction::None;
    let row_rect = Rect::from_min_size(ui.cursor().min, vec2(ui.available_width(), 28.0));
    // Registered before the row's own widgets so the transfer button stays clickable on top.
    let row = ui.interact(row_rect, ui.id().with(("entry_row", &entry.path)), Sense::click());
    if row.contains_pointer() {
        ui.painter()
            .rect_filled(row_rect, CornerRadius::same(6), c::SURFACE_2);
    }
    if entry.is_dir {
        if row.clicked() {
            act = RowAction::Open;
        }
        row.on_hover_cursor(CursorIcon::PointingHand);
    }
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
                paint_entry_icon(ui, entry.is_dir);
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

/// Small painted folder / document icon (emoji glyphs render tiny and skewed at this size).
fn paint_entry_icon(ui: &mut egui::Ui, is_dir: bool) {
    let (rect, _) = ui.allocate_exact_size(vec2(18.0, 16.0), Sense::hover());
    let p = ui.painter();
    let (l, t) = (rect.left() + 1.0, rect.top() + 1.0);
    if is_dir {
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
    } else {
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
                    if let Some(parent) = Path::new(&state.local_dir).parent() {
                        state.local_dir = parent.to_string_lossy().into_owned();
                    } else {
                        state.local_dir.clear();
                    }
                    state.refresh_local_dir();
                }
                if theme::small_button(ui, "盘符", Kind::Ghost).clicked() {
                    state.local_dir.clear();
                    state.refresh_local_dir();
                }
            });
        });
        ui.horizontal(|ui| {
            let w = (ui.available_width() - 62.0).max(80.0);
            let resp = theme::text_input(ui, &mut state.local_dir, w, "本地路径，留空显示盘符");
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            if theme::small_button(ui, "前往", Kind::Secondary).clicked() || enter {
                state.refresh_local_dir();
            }
        });
        if let Some(err) = &state.local_error {
            ui.label(RichText::new(err).size(12.5).color(c::DANGER));
        }
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
                for entry in entries {
                    match entry_row(ui, &entry, "上传 →") {
                        RowAction::Open => {
                            state.local_dir = entry.path.clone();
                            state.refresh_local_dir();
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
        ui.horizontal(|ui| {
            ui.label(theme::bold("远程电脑").size(14.0));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if theme::small_button(ui, "刷新", Kind::Ghost).clicked() {
                    session
                        .file_client
                        .send(FileCommand::ListRemoteDir(state.remote_dir_input.clone()));
                }
                if theme::small_button(ui, "上级", Kind::Ghost).clicked() {
                    let next = Path::new(state.remote_dir_input.trim())
                        .parent()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    session.file_client.send(FileCommand::ListRemoteDir(next));
                }
                if theme::small_button(ui, "盘符", Kind::Ghost).clicked() {
                    session
                        .file_client
                        .send(FileCommand::ListRemoteDir(String::new()));
                }
            });
        });
        ui.horizontal(|ui| {
            let w = (ui.available_width() - 62.0).max(80.0);
            let resp =
                theme::text_input(ui, &mut state.remote_dir_input, w, "远程路径，留空显示盘符");
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            if theme::small_button(ui, "前往", Kind::Secondary).clicked() || enter {
                session
                    .file_client
                    .send(FileCommand::ListRemoteDir(state.remote_dir_input.clone()));
            }
        });

        if session
            .file_client
            .remote_loading
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(14.0).color(c::ACCENT));
                ui.label(theme::faint("正在读取远程目录…"));
            });
        }
        if let Some(err) = session.file_client.remote_error.lock().unwrap().clone() {
            ui.label(RichText::new(err).size(12.5).color(c::DANGER));
        }

        let r_entries = session.file_client.remote_entries.lock().unwrap().clone();
        let local_target_dir = if state.local_dir.trim().is_empty() {
            crate::transfer::default_receive_dir()
        } else {
            PathBuf::from(&state.local_dir)
        };
        egui::ScrollArea::vertical()
            .id_salt("remote_entries")
            .max_height(list_h)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                if r_entries.is_empty() {
                    ui.label(theme::faint("（空）"));
                }
                for entry in r_entries {
                    match entry_row(ui, &entry, "← 下载") {
                        RowAction::Open => {
                            session
                                .file_client
                                .send(FileCommand::ListRemoteDir(entry.path.clone()));
                        }
                        RowAction::Transfer => {
                            session.file_client.send(FileCommand::DownloadFile {
                                remote_path: entry.path.clone(),
                                local_dir: local_target_dir.clone(),
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
) {
    let mut events_to_send = Vec::new();
    let mut cur_mods = Modifiers::NONE;

    ctx.input(|i| {
        cur_mods = i.modifiers;

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

        for ev in &i.events {
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
                Event::Key { key, pressed, .. } => {
                    if *key == Key::F11 {
                        continue;
                    }
                    if let Some((vk, ext)) = map_egui_key(*key) {
                        let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) as u16 };
                        events_to_send.push(InputEvent::Key {
                            vk,
                            scan_code: scan,
                            extended: ext,
                            pressed: *pressed,
                        });
                    }
                }
                _ => {}
            }
        }
    });

    let mod_pairs = [
        (state.prev_mods.ctrl, cur_mods.ctrl, 0xA2u16, false),
        (state.prev_mods.shift, cur_mods.shift, 0xA0u16, false),
        (state.prev_mods.alt, cur_mods.alt, 0xA4u16, false),
        (
            state.prev_mods.mac_cmd || state.prev_mods.command && !cur_mods.ctrl,
            cur_mods.mac_cmd,
            0x5Bu16,
            true,
        ),
    ];
    for (was, now, vk, ext) in mod_pairs {
        if was != now {
            let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) as u16 };
            events_to_send.push(InputEvent::Key {
                vk,
                scan_code: scan,
                extended: ext,
                pressed: now,
            });
        }
    }
    state.prev_mods = cur_mods;

    for ev in events_to_send {
        session.send(ViewerCommand::Input(ev));
    }
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

fn map_egui_key(key: Key) -> Option<(u16, bool)> {
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
        Key::Num1 => (0x31, false),
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
        Key::Minus => (0xBD, false),
        Key::Equals => (0xBB, false),
        Key::OpenBracket => (0xDB, false),
        Key::CloseBracket => (0xDD, false),
        Key::Backslash => (0xDC, false),
        Key::Semicolon => (0xBA, false),
        Key::Quote => (0xDE, false),
        Key::Backtick => (0xC0, false),
        Key::Comma => (0xBC, false),
        Key::Period => (0xBE, false),
        Key::Slash => (0xBF, false),
        _ => return None,
    };
    Some(pair)
}
