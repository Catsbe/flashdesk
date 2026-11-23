use crate::net::viewer_session::{ReconnectTarget, ViewerCommand, ViewerSessionHandle};
use crate::transfer::{list_local_dir, TransferDirection, TransferState};
use crate::vdd::{MTT_VDD_DOWNLOAD_URL, PARSEC_VDD_DOWNLOAD_URL};
use eframe::egui::{
    self, pos2, vec2, Align2, Color32, ColorImage, CornerRadius, Event, Key, Modifiers,
    PointerButton, Rect, RichText, Sense, TextureHandle, TextureOptions, ViewportCommand,
};
use protocol::auth::RelayNodeConfig;
use protocol::session::{InputEvent, MouseBtn, VddRequest};
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
}

pub fn render_remote_view(
    ctx: &egui::Context,
    state: &mut RemoteViewState,
    session: &ViewerSessionHandle,
) -> RemoteViewAction {
    let mut action = RemoteViewAction::None;

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
        .frame(egui::Frame::NONE.fill(Color32::BLACK))
        .show(ctx, |ui| {
            let avail = ui.available_rect_before_wrap();

            if let Some(tex) = &state.texture {
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
                let video_rect = Rect::from_center_size(avail.center(), disp_size);
                let resp = ui.allocate_rect(video_rect, Sense::click_and_drag());
                ui.painter().image(
                    tex.id(),
                    video_rect,
                    Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                    Color32::WHITE,
                );

                // Only forward keyboard/mouse when floating modal windows aren't blocking pointer
                if disconnect_reason.is_none() {
                    if resp.clicked() || resp.drag_started() {
                        resp.request_focus();
                    }
                    let pointer_in_video = resp.hovered() || resp.dragged();
                    if pointer_in_video && !state.show_file_panel && !state.show_vdd_window {
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
                    if !state.show_file_panel && !state.show_vdd_window {
                        forward_inputs(ctx, state, session, video_rect, pointer_in_video);
                    }
                }
            } else {
                ui.centered_and_justified(|ui| {
                    if hud.connecting {
                        ui.spinner();
                        ui.label(
                            RichText::new(format!(
                                "正在建立端到端加密通道 ({})...",
                                session.via_label
                            ))
                            .size(16.0)
                            .color(Color32::LIGHT_GRAY),
                        );
                    } else {
                        ui.label(
                            RichText::new("等待远程画面首帧...").color(Color32::LIGHT_GRAY),
                        );
                    }
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
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(Color32::from_rgba_unmultiplied(15, 55, 50, 230))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(egui::Margin::same(24))
                    .show(ui, |ui| {
                        ui.heading(
                            RichText::new(format!(
                                "📥 松开鼠标以发送 {hovered_count} 个文件到远程主机"
                            ))
                            .color(Color32::from_rgb(130, 250, 200)),
                        );
                        ui.label(
                            RichText::new(format!("传输通道: {via} (SHA-256 校验 + 断点续传)"))
                                .color(Color32::WHITE),
                        );
                    });
            });
    }
    if !dropped_files.is_empty() {
        let remote_dir = session.file_client.remote_dir.lock().unwrap().clone();
        let mut count = 0usize;
        for df in dropped_files {
            if let Some(path) = df.path {
                if path.is_file() {
                    session
                        .file_client
                        .send(crate::transfer::client_manager::FileCommand::UploadFile {
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
                    .send(crate::transfer::client_manager::FileCommand::ListRemoteDir(
                        remote_dir,
                    ));
                state.file_panel_initialized = true;
            }
            state.drop_notice = Some(format!("已加入 {count} 个上传任务"));
        }
    }

    // Top collapsible floating toolbar
    egui::Area::new(egui::Id::new("remote_toolbar"))
        .anchor(Align2::CENTER_TOP, vec2(0.0, 8.0))
        .show(ctx, |ui| {
            egui::Frame::window(ui.style())
                .fill(Color32::from_rgba_unmultiplied(18, 22, 30, 236))
                .inner_margin(egui::Margin::symmetric(10, 6))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let toggle_txt = if state.toolbar_collapsed { "▼ 展开" } else { "▲ 收起" };
                        if ui.small_button(toggle_txt).clicked() {
                            state.toolbar_collapsed = !state.toolbar_collapsed;
                        }
                        ui.label(
                            RichText::new(&session.peer_name)
                                .strong()
                                .color(Color32::WHITE),
                        );

                        if !state.toolbar_collapsed {
                            ui.separator();

                            // Monitor selector
                            let mon_label = if active_mon == u32::MAX {
                                "全部显示器全景".into()
                            } else {
                                monitors
                                    .iter()
                                    .find(|m| m.index == active_mon)
                                    .map(|m| {
                                        format!("屏{} ({}x{})", m.index + 1, m.width, m.height)
                                    })
                                    .unwrap_or_else(|| format!("屏{}", active_mon + 1))
                            };
                            egui::ComboBox::from_id_salt("remote_mon_sel")
                                .selected_text(mon_label)
                                .width(135.0)
                                .show_ui(ui, |ui| {
                                    for m in &monitors {
                                        let txt = format!(
                                            "显示器 {} ({}x{}, {}dpi)",
                                            m.index + 1,
                                            m.width,
                                            m.height,
                                            m.dpi
                                        );
                                        if ui
                                            .selectable_value(&mut active_mon, m.index, txt)
                                            .changed()
                                        {
                                            session.send(ViewerCommand::SetMonitor(active_mon));
                                        }
                                    }
                                    if monitors.len() > 1
                                        && ui
                                            .selectable_value(
                                                &mut active_mon,
                                                u32::MAX,
                                                "全部显示器全景",
                                            )
                                            .changed()
                                    {
                                        session.send(ViewerCommand::SetMonitor(u32::MAX));
                                    }
                                });

                            // Live relay node hot-switch
                            if matches!(session.reconnect_target, ReconnectTarget::Relay { .. })
                                && session.available_relays.len() > 1
                            {
                                let cur_id = session.current_ctrl_relay_id.clone().unwrap_or_default();
                                let cur_name = session
                                    .available_relays
                                    .iter()
                                    .find(|r| r.id == cur_id)
                                    .map(|r| r.name.clone())
                                    .unwrap_or_else(|| "切换节点".into());
                                egui::ComboBox::from_id_salt("ctrl_relay_switch")
                                    .selected_text(cur_name)
                                    .width(120.0)
                                    .show_ui(ui, |ui| {
                                        for r in &session.available_relays {
                                            let selected = r.id == cur_id;
                                            if ui.selectable_label(selected, &r.name).clicked()
                                                && !selected
                                            {
                                                action = RemoteViewAction::SwitchControlRelay(
                                                    r.clone(),
                                                );
                                            }
                                        }
                                    });
                            }

                            ui.separator();
                            ui.label(RichText::new("码率上限").color(Color32::GRAY))
                                .on_hover_text("自适应码率不会超过此值；网络变差时自动下调，延迟保持在 100ms 量级");
                            let br_resp = ui.add(
                                egui::Slider::new(&mut state.bitrate_mbps, 1.0..=50.0)
                                    .suffix("M")
                                    .fixed_decimals(1),
                            );
                            if br_resp.drag_stopped() || br_resp.lost_focus() {
                                session.send(ViewerCommand::SetBitrate(
                                    (state.bitrate_mbps * 1000.0).round() as u32,
                                ));
                            }

                            let res_label = match state.res_cap_h {
                                1080 => "1080p",
                                720 => "720p",
                                _ => "原始分辨率",
                            };
                            egui::ComboBox::from_id_salt("remote_res_cap")
                                .selected_text(res_label)
                                .width(92.0)
                                .show_ui(ui, |ui| {
                                    for (h, txt) in [(0u32, "原始分辨率"), (1080, "1080p"), (720, "720p")] {
                                        if ui.selectable_value(&mut state.res_cap_h, h, txt).changed() {
                                            let (l, s) = crate::config::res_cap_for_height(h);
                                            session.send(ViewerCommand::SetResolutionCap(l, s));
                                        }
                                    }
                                })
                                .response
                                .on_hover_text("2K/4K 屏走小带宽中转时选 1080p：每帧数据量减半，更流畅");

                            if ui.selectable_value(&mut state.target_fps, 30, "30").changed() {
                                session.send(ViewerCommand::SetFps(30));
                            }
                            if ui.selectable_value(&mut state.target_fps, 60, "60fps").changed() {
                                session.send(ViewerCommand::SetFps(60));
                            }

                            ui.separator();
                            if ui
                                .selectable_label(state.show_file_panel, "📁 文件")
                                .clicked()
                            {
                                state.show_file_panel = !state.show_file_panel;
                                if state.show_file_panel && !state.file_panel_initialized {
                                    state.refresh_local_dir();
                                    session.file_client.send(
                                        crate::transfer::client_manager::FileCommand::ListRemoteDir(
                                            String::new(),
                                        ),
                                    );
                                    state.file_panel_initialized = true;
                                }
                            }
                            if ui
                                .checkbox(&mut state.clipboard_enabled, "📋 剪贴板")
                                .changed()
                            {
                                session.send(ViewerCommand::SetClipboardEnabled(
                                    state.clipboard_enabled,
                                ));
                            }
                            if ui
                                .selectable_label(state.show_vdd_window, "🖥 虚拟屏")
                                .clicked()
                            {
                                state.show_vdd_window = !state.show_vdd_window;
                                if state.show_vdd_window {
                                    session.send(ViewerCommand::VddAction(VddRequest::Query));
                                }
                            }

                            let fit_txt = match state.scale_mode {
                                ScaleMode::Fit => "自适应",
                                ScaleMode::Original => "1:1",
                            };
                            if ui.small_button(fit_txt).clicked() {
                                state.scale_mode = match state.scale_mode {
                                    ScaleMode::Fit => ScaleMode::Original,
                                    ScaleMode::Original => ScaleMode::Fit,
                                };
                            }

                            if ui
                                .small_button(if state.fullscreen { "退出全屏" } else { "全屏" })
                                .clicked()
                            {
                                state.fullscreen = !state.fullscreen;
                                ctx.send_viewport_cmd(ViewportCommand::Fullscreen(state.fullscreen));
                            }

                            if ui.small_button("Ctrl+Alt+Del").clicked() {
                                session.send(ViewerCommand::Input(InputEvent::SendSas));
                            }
                        }

                        if ui
                            .button(RichText::new("断开").color(Color32::from_rgb(255, 115, 115)))
                            .clicked()
                        {
                            if state.fullscreen {
                                ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
                            }
                            session.send(ViewerCommand::Disconnect);
                            action = RemoteViewAction::Disconnect;
                        }
                    });
                });
        });

    // Top-right semi-transparent HUD
    egui::Area::new(egui::Id::new("remote_hud"))
        .anchor(Align2::RIGHT_TOP, vec2(-12.0, 56.0))
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::NONE
                .fill(Color32::from_rgba_unmultiplied(12, 16, 22, 195))
                .corner_radius(CornerRadius::same(6))
                .inner_margin(egui::Margin::same(8))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(format!(
                            "FPS: {:.1} | 延迟: {} ms | 码率: {:.2} Mbps",
                            hud.fps, hud.rtt_ms, hud.bitrate_mbps
                        ))
                        .monospace()
                        .size(12.0)
                        .color(Color32::from_rgb(130, 245, 195)),
                    );
                    if let Some(host) = hud.host.as_ref().filter(|h| h.flow_control) {
                        // "画面" = host hand-off -> decoded here (includes all queueing on the path).
                        ui.label(
                            RichText::new(format!(
                                "画面: {} ms (峰值 {}) | 自适应码率: {:.1}/{:.1} Mbps | 解码: {:.1}ms{}",
                                host.avg_delay_ms,
                                host.max_delay_ms,
                                host.sent_kbps as f32 / 1000.0,
                                host.target_kbps as f32 / 1000.0,
                                hud.decode_ms,
                                if host.paused_ticks > 0 {
                                    format!(" | 限流 {} 帧", host.paused_ticks)
                                } else {
                                    String::new()
                                }
                            ))
                            .monospace()
                            .size(11.0)
                            .color(Color32::from_rgb(160, 210, 255)),
                        );
                    }
                    let codec_disp = if hud.codec_label.is_empty() {
                        "协商中...".into()
                    } else {
                        hud.codec_label.clone()
                    };
                    let res_disp = if hud.remote_res.is_empty() {
                        "-".into()
                    } else {
                        hud.remote_res.clone()
                    };
                    ui.label(
                        RichText::new(format!(
                            "{} | {} | 编码: {:.1}ms | {}",
                            res_disp,
                            codec_disp,
                            hud.encode_us as f32 / 1000.0,
                            hud.relay_name
                        ))
                        .monospace()
                        .size(11.0)
                        .color(Color32::LIGHT_GRAY),
                    );
                    if let Some(notice) = &hud.status_notice {
                        ui.colored_label(Color32::from_rgb(255, 210, 120), notice);
                    }
                });
        });

    // M2 Dual-Pane File Transfer Manager Window
    if state.show_file_panel {
        render_file_manager_window(ctx, state, session);
    }

    // M3 Remote Virtual Display Manager Window
    if state.show_vdd_window {
        render_remote_vdd_window(ctx, state, session);
    }

    if let Some(reason) = disconnect_reason {
        egui::Window::new("会话已断开")
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(reason);
                if ui.button("返回设备列表").clicked() {
                    if state.fullscreen {
                        ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
                    }
                    action = RemoteViewAction::Disconnect;
                }
            });
    }

    action
}

fn render_file_manager_window(
    ctx: &egui::Context,
    state: &mut RemoteViewState,
    session: &ViewerSessionHandle,
) {
    use crate::transfer::client_manager::FileCommand;

    let remote_dir = session.file_client.remote_dir.lock().unwrap().clone();
    if remote_dir != state.last_synced_remote_dir {
        state.remote_dir_input = remote_dir.clone();
        state.last_synced_remote_dir = remote_dir.clone();
    }

    let mut open = state.show_file_panel;
    egui::Window::new("📁 高速文件传输（支持拖拽、海外 1G 分流、断点续传、SHA-256 校验）")
        .open(&mut open)
        .default_size([840.0, 520.0])
        .resizable(true)
        .show(ctx, |ui| {
            // Relay channel selector bar
            let via = session.file_client.active_via_label.lock().unwrap().clone();
            ui.horizontal(|ui| {
                ui.label(RichText::new("当前文件传输通道:").strong());
                ui.colored_label(Color32::from_rgb(110, 240, 200), &via);

                if matches!(session.reconnect_target, ReconnectTarget::Relay { .. })
                    && !session.available_relays.is_empty()
                {
                    ui.separator();
                    ui.label("切换文件节点:");
                    let cur_label = state
                        .selected_file_relay_id
                        .as_ref()
                        .and_then(|id| session.available_relays.iter().find(|r| &r.id == id))
                        .map(|r| r.name.clone())
                        .unwrap_or_else(|| "默认控屏节点".into());
                    egui::ComboBox::from_id_salt("file_relay_pick")
                        .selected_text(cur_label)
                        .width(190.0)
                        .show_ui(ui, |ui| {
                            for node in &session.available_relays {
                                let label = format!("{} ({})", node.name, node.role.label());
                                let is_sel =
                                    state.selected_file_relay_id.as_deref() == Some(&node.id);
                                if ui.selectable_label(is_sel, label).clicked() && !is_sel {
                                    state.selected_file_relay_id = Some(node.id.clone());
                                    session
                                        .file_client
                                        .send(FileCommand::SetRelay(Some(node.clone())));
                                }
                            }
                        });
                }

                if let Some(note) = state.drop_notice.clone() {
                    ui.separator();
                    ui.colored_label(Color32::from_rgb(140, 235, 160), note);
                }
            });
            ui.separator();

            // Dual pane browser
            let avail_w = ui.available_width();
            let col_w = (avail_w - 16.0) * 0.5;
            ui.columns(2, |cols| {
                // Left: Local PC
                cols[0].push_id("local_pane", |ui| {
                    ui.set_min_width(col_w);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("💻 本地电脑").strong());
                        if ui.small_button("根目录").clicked() {
                            state.local_dir.clear();
                            state.refresh_local_dir();
                        }
                        if ui.small_button("上级").clicked() {
                            if let Some(parent) = Path::new(&state.local_dir).parent() {
                                state.local_dir = parent.to_string_lossy().into_owned();
                            } else {
                                state.local_dir.clear();
                            }
                            state.refresh_local_dir();
                        }
                        if ui.small_button("刷新").clicked() {
                            state.refresh_local_dir();
                        }
                    });
                    ui.horizontal(|ui| {
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut state.local_dir)
                                .desired_width(ui.available_width() - 50.0)
                                .hint_text("输入本地路径，留空显示盘符"),
                        );
                        if ui.small_button("前往").clicked()
                            || (resp.lost_focus()
                                && ui.input(|i| i.key_pressed(Key::Enter)))
                        {
                            state.refresh_local_dir();
                        }
                    });
                    if let Some(err) = &state.local_error {
                        ui.colored_label(Color32::from_rgb(255, 120, 120), err);
                    }
                    let entries = state.local_entries.clone();
                    let cur_remote_dir = session.file_client.remote_dir.lock().unwrap().clone();
                    egui::ScrollArea::vertical()
                        .max_height(210.0)
                        .show(ui, |ui| {
                            for entry in entries {
                                ui.horizontal(|ui| {
                                    let icon = if entry.is_dir { "📁" } else { "📄" };
                                    if entry.is_dir {
                                        if ui.selectable_label(false, format!("{icon} {}", entry.name)).clicked() {
                                            state.local_dir = entry.path;
                                            state.refresh_local_dir();
                                        }
                                    } else {
                                        ui.label(format!("{icon} {}", entry.name));
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                if ui.small_button("上传 ➡").clicked() {
                                                    session.file_client.send(
                                                        FileCommand::UploadFile {
                                                            local_path: PathBuf::from(&entry.path),
                                                            remote_dir: cur_remote_dir.clone(),
                                                        },
                                                    );
                                                }
                                                ui.label(
                                                    RichText::new(format_bytes(entry.size))
                                                        .small()
                                                        .color(Color32::GRAY),
                                                );
                                            },
                                        );
                                    }
                                });
                            }
                        });
                });

                // Right: Remote Host
                cols[1].push_id("remote_pane", |ui| {
                    ui.set_min_width(col_w);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("🖥 远程主机").strong());
                        if ui.small_button("根目录").clicked() {
                            session
                                .file_client
                                .send(FileCommand::ListRemoteDir(String::new()));
                        }
                        if ui.small_button("上级").clicked() {
                            let cur = state.remote_dir_input.trim();
                            let next = Path::new(cur)
                                .parent()
                                .map(|p| p.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            session.file_client.send(FileCommand::ListRemoteDir(next));
                        }
                        if ui.small_button("刷新").clicked() {
                            session.file_client.send(FileCommand::ListRemoteDir(
                                state.remote_dir_input.clone(),
                            ));
                        }
                    });
                    ui.horizontal(|ui| {
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut state.remote_dir_input)
                                .desired_width(ui.available_width() - 50.0)
                                .hint_text("远程路径，留空显示盘符"),
                        );
                        if ui.small_button("前往").clicked()
                            || (resp.lost_focus()
                                && ui.input(|i| i.key_pressed(Key::Enter)))
                        {
                            session.file_client.send(FileCommand::ListRemoteDir(
                                state.remote_dir_input.clone(),
                            ));
                        }
                    });

                    if session
                        .file_client
                        .remote_loading
                        .load(std::sync::atomic::Ordering::Relaxed)
                    {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("正在读取远程目录...");
                        });
                    }
                    if let Some(err) = session.file_client.remote_error.lock().unwrap().clone() {
                        ui.colored_label(Color32::from_rgb(255, 120, 120), err);
                    }

                    let r_entries = session.file_client.remote_entries.lock().unwrap().clone();
                    let local_target_dir = if state.local_dir.trim().is_empty() {
                        crate::transfer::default_receive_dir()
                    } else {
                        PathBuf::from(&state.local_dir)
                    };
                    egui::ScrollArea::vertical()
                        .max_height(210.0)
                        .show(ui, |ui| {
                            for entry in r_entries {
                                ui.horizontal(|ui| {
                                    let icon = if entry.is_dir { "📁" } else { "📄" };
                                    if entry.is_dir {
                                        if ui
                                            .selectable_label(false, format!("{icon} {}", entry.name))
                                            .clicked()
                                        {
                                            session.file_client.send(FileCommand::ListRemoteDir(
                                                entry.path,
                                            ));
                                        }
                                    } else {
                                        ui.label(format!("{icon} {}", entry.name));
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                if ui.small_button("⬅ 下载").clicked() {
                                                    session.file_client.send(
                                                        FileCommand::DownloadFile {
                                                            remote_path: entry.path.clone(),
                                                            local_dir: local_target_dir.clone(),
                                                        },
                                                    );
                                                }
                                                ui.label(
                                                    RichText::new(format_bytes(entry.size))
                                                        .small()
                                                        .color(Color32::GRAY),
                                                );
                                            },
                                        );
                                    }
                                });
                            }
                        });
                });
            });

            ui.separator();
            ui.label(RichText::new("传输队列（支持把本地文件直接拖进远控屏幕窗口上传）").strong());
            let records = session.file_client.transfers.lock().unwrap().clone();
            if records.is_empty() {
                ui.label(RichText::new("暂无传输任务。").color(Color32::GRAY));
            } else {
                egui::ScrollArea::vertical()
                    .max_height(150.0)
                    .show(ui, |ui| {
                        for rec in records {
                            let dir_tag = match rec.direction {
                                TransferDirection::Upload => "⬆ 上传",
                                TransferDirection::Download => "⬇ 下载",
                            };
                            let progress = if rec.total_bytes > 0 {
                                (rec.transferred_bytes as f32 / rec.total_bytes as f32)
                                    .clamp(0.0, 1.0)
                            } else if rec.state == TransferState::Done {
                                1.0
                            } else {
                                0.0
                            };

                            ui.horizontal(|ui| {
                                ui.label(RichText::new(format!("{dir_tag} {}", rec.file_name)).strong())
                                    .on_hover_text(format!("本地: {}\n远程: {}", rec.local_path, rec.remote_path));
                                ui.label(
                                    RichText::new(format!(
                                        "{}/{}",
                                        format_bytes(rec.transferred_bytes),
                                        format_bytes(rec.total_bytes)
                                    ))
                                    .small()
                                    .monospace(),
                                );
                                ui.label(
                                    RichText::new(format!("[{}]", rec.via_label))
                                        .small()
                                        .color(Color32::from_rgb(130, 200, 255)),
                                );
                                if rec.resumed {
                                    ui.label(
                                        RichText::new("[断点续传]")
                                            .small()
                                            .color(Color32::from_rgb(245, 210, 110)),
                                    );
                                }
                                if rec.sha256_verified {
                                    ui.label(
                                        RichText::new("[SHA-256 ✓]")
                                            .small()
                                            .color(Color32::from_rgb(110, 240, 170)),
                                    );
                                }

                                match &rec.state {
                                    TransferState::Active => {
                                        ui.label(
                                            RichText::new(format!("{:.2} Mbps", rec.speed_mbps))
                                                .small()
                                                .color(Color32::from_rgb(120, 245, 200)),
                                        );
                                        if ui.small_button("取消").clicked() {
                                            session
                                                .file_client
                                                .send(FileCommand::Cancel(rec.transfer_id));
                                        }
                                    }
                                    TransferState::Queued => {
                                        ui.label("排队中...");
                                    }
                                    TransferState::Done => {
                                        ui.colored_label(
                                            Color32::from_rgb(110, 240, 170),
                                            "已完成",
                                        );
                                    }
                                    TransferState::Cancelled => {
                                        ui.colored_label(
                                            Color32::from_rgb(235, 200, 110),
                                            "已暂停（可续传）",
                                        );
                                    }
                                    TransferState::Failed(err) => {
                                        ui.colored_label(
                                            Color32::from_rgb(255, 115, 115),
                                            format!("失败: {err}"),
                                        );
                                    }
                                }
                            });
                            ui.add(egui::ProgressBar::new(progress).desired_height(6.0));
                        }
                    });
            }
        });
    state.show_file_panel = open;
}

fn render_remote_vdd_window(
    ctx: &egui::Context,
    state: &mut RemoteViewState,
    session: &ViewerSessionHandle,
) {
    let mut open = state.show_vdd_window;
    let info = session.vdd_info.lock().unwrap().clone();

    egui::Window::new("🖥 远程无显示器虚拟屏控制")
        .open(&mut open)
        .default_width(420.0)
        .resizable(false)
        .show(ctx, |ui| {
            if let Some(v) = &info {
                ui.label(format!("驱动状态: {}", v.details));
                ui.label(format!("活动虚拟屏数量: {}", v.active_virtual_displays));
            } else {
                ui.label("正在查询远程主机虚拟屏状态...");
            }
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("➕ 创建 1080P 60Hz 虚拟屏").clicked() {
                    session.send(ViewerCommand::VddAction(VddRequest::AddDisplay {
                        width: 1920,
                        height: 1080,
                        refresh_hz: 60,
                    }));
                }
                if ui.button("➕ 创建 2K 60Hz 虚拟屏").clicked() {
                    session.send(ViewerCommand::VddAction(VddRequest::AddDisplay {
                        width: 2560,
                        height: 1440,
                        refresh_hz: 60,
                    }));
                }
                if ui.button("🗑 移除虚拟屏").clicked() {
                    session.send(ViewerCommand::VddAction(VddRequest::RemoveDisplay));
                }
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.small_button("🔄 刷新状态").clicked() {
                    session.send(ViewerCommand::VddAction(VddRequest::Query));
                }
                if ui.small_button("下载 Parsec VDD").clicked() {
                    crate::vdd::open_url(PARSEC_VDD_DOWNLOAD_URL);
                }
                if ui.small_button("下载 Virtual-Display-Driver").clicked() {
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
