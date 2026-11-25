use crate::config::{default_hostname, AppConfig};
use crate::net::relay_client::RelayCommand;
use crate::net::RelayStatus;
use crate::service::install::{launch_elevated_service_cmd, ServiceState};
use crate::ui::home::{default_relays, install_service, service_summary, sync_service_config};
use crate::ui::sysinfo::SysStatus;
use crate::ui::theme::{self, c, Kind, Toast, Tone};
use crate::vdd::{handle_vdd_request, open_url, MTT_VDD_DOWNLOAD_URL, PARSEC_VDD_DOWNLOAD_URL};
use eframe::egui::{self, pos2, vec2, Align, Color32, CornerRadius, Layout, Rect, RichText, Sense};
use protocol::auth::{DeviceBundle, RelayNodeConfig, RelayNodeRole};
use protocol::session::VddRequest;
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use uuid::Uuid;

pub struct SettingsState {
    pub new_node_id: String,
    pub new_node_name: String,
    pub new_node_addr: String,
    pub new_node_role: RelayNodeRole,
    pub issue_device_name: String,
    pub issued_label: String,
    pub issued_bundle: Option<DeviceBundle>,
    pub issued_compact_token: String,
    pub issued_qr_modules: Option<(usize, Vec<bool>)>,
    pub local_name_input: String,
    pub local_name_synced_with: String,
    pub revoke_uuid_input: String,
    pub import_token_input: String,
    pub toast: Option<Toast>,
}

impl Default for SettingsState {
    fn default() -> Self {
        Self {
            new_node_id: String::new(),
            new_node_name: String::new(),
            new_node_addr: String::new(),
            new_node_role: RelayNodeRole::PrimaryControl,
            issue_device_name: String::new(),
            issued_label: String::new(),
            issued_bundle: None,
            issued_compact_token: String::new(),
            issued_qr_modules: None,
            local_name_input: String::new(),
            local_name_synced_with: String::new(),
            revoke_uuid_input: String::new(),
            import_token_input: String::new(),
            toast: None,
        }
    }
}

fn toast(ui: &egui::Ui, state: &mut SettingsState, tone: Tone, text: impl Into<String>) {
    state.toast = Some(Toast::new(ui.ctx(), tone, text));
}

/// Returns true when the config changed and must be saved / pushed to the relay manager.
/// Text fields only report a change once editing finishes (focus lost / Enter) and sliders once
/// the drag ends, so typing an address no longer rewrites config.json and reconnects the relay
/// on every keystroke.
pub fn render_settings(
    ui: &mut egui::Ui,
    state: &mut SettingsState,
    cfg_path: &Path,
    cfg: &Arc<Mutex<AppConfig>>,
    relay_status: &Arc<Mutex<RelayStatus>>,
    relay_cmd_tx: &mpsc::UnboundedSender<RelayCommand>,
    sys: &SysStatus,
) -> bool {
    let mut changed = false;
    let mut conf = cfg.lock().unwrap();
    let st = relay_status.lock().unwrap().clone();

    identity_card(ui, state, cfg_path, &mut conf, &mut changed);
    theme::gap(ui);
    relays_card(ui, state, &mut conf, &mut changed);
    theme::gap(ui);
    stream_card(ui, &mut conf, &mut changed);
    theme::gap(ui);
    unattended_card(ui, state, cfg_path, &mut conf, sys, &mut changed);
    theme::gap(ui);
    revoke_card(ui, state, cfg_path, &mut conf, &st, relay_cmd_tx, &mut changed);

    theme::toast_area(ui.ctx(), &mut state.toast);
    changed
}

fn build_qr_matrix(token: &str) -> Option<(usize, Vec<bool>)> {
    let code = qrcode::QrCode::new(token.as_bytes()).ok()?;
    let width = code.width();
    let modules = code
        .to_colors()
        .into_iter()
        .map(|col| col == qrcode::Color::Dark)
        .collect();
    Some((width, modules))
}

fn paint_qr_code(ui: &mut egui::Ui, size_pt: f32, width: usize, modules: &[bool]) {
    let (rect, _) = ui.allocate_exact_size(vec2(size_pt, size_pt), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(10), Color32::WHITE);
    if width == 0 || modules.len() < width * width {
        return;
    }
    let quiet = 2.0_f32;
    let cell = (size_pt / (width as f32 + quiet * 2.0)).max(1.0);
    let total = cell * (width as f32);
    let origin = pos2(
        rect.center().x - total / 2.0,
        rect.center().y - total / 2.0,
    );
    let dark = Color32::from_rgb(16, 20, 26);
    for y in 0..width {
        for x in 0..width {
            if modules[y * width + x] {
                let r = Rect::from_min_size(
                    pos2(origin.x + x as f32 * cell, origin.y + y as f32 * cell),
                    vec2(cell + 0.25, cell + 0.25),
                );
                painter.rect_filled(r, 0, dark);
            }
        }
    }
}

fn identity_card(
    ui: &mut egui::Ui,
    state: &mut SettingsState,
    cfg_path: &Path,
    conf: &mut AppConfig,
    changed: &mut bool,
) {
    theme::card(ui, |ui| {
        theme::card_header(
            ui,
            "身份与设备绑定",
            Some("个人根密钥与本机配置固定存放在 ~/.flashdesk/，换用新版 exe 不会丢失"),
            |_| {},
        );

        let dir_display = cfg_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| cfg_path.display().to_string());
        ui.horizontal_wrapped(|ui| {
            ui.label(theme::faint("密钥存储目录"));
            ui.label(theme::mono(&dir_display));
        });

        if conf.bundle.is_some() {
            let cur_name = conf.display_device_name();
            if state.local_name_synced_with != cur_name {
                state.local_name_input = cur_name.clone();
                state.local_name_synced_with = cur_name;
            }
            theme::form_row(ui, "本机显示名称", |ui| {
                let w = (ui.available_width() - 10.0).clamp(140.0, 240.0);
                let resp = theme::text_input(ui, &mut state.local_name_input, w, "如 家里电脑");
                if theme::committed(&resp) {
                    let trimmed = state.local_name_input.trim().to_string();
                    if !trimmed.is_empty() && trimmed != conf.display_device_name() {
                        conf.local_device_name = Some(trimmed.clone());
                        state.local_name_synced_with = trimmed;
                        *changed = true;
                        toast(ui, state, Tone::Ok, "已更新本机显示名称");
                    }
                }
                theme::hint(ui, "修改后按回车，其它设备列表中立即更新");
            });
        }

        if let Some(kp) = conf.load_root_keypair(cfg_path) {
            let pub_b64 = kp.pubkey_b64();
            ui.add_space(2.0);
            ui.label(theme::subheading("中转服务器根公钥（仅需在服务器配置一次）"));
            if theme::code_box(ui, &pub_b64) {
                toast(ui, state, Tone::Ok, "已复制根公钥");
            }
            ui.horizontal_wrapped(|ui| {
                if theme::small_button(ui, "复制完整 relay.toml", Kind::Secondary).clicked() {
                    let revoked = conf
                        .revoked_device_ids
                        .iter()
                        .map(|u| format!("\"{u}\""))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let toml_text = format!(
                        "listen_addr = \"0.0.0.0:9442\"\nroot_pubkey_b64 = \"{pub_b64}\"\nrevoked_device_ids = [{revoked}]\npending_timeout_secs = 15\n"
                    );
                    ui.ctx().copy_text(toml_text);
                    toast(ui, state, Tone::Ok, "已复制完整 relay.toml，粘贴到云服务器即可");
                }
                if theme::small_button(ui, "复制一键部署命令", Kind::Secondary).clicked() {
                    let port = conf
                        .active_relay()
                        .and_then(|r| r.addr.rsplit(':').next())
                        .filter(|p| p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty())
                        .unwrap_or("9442");
                    let cmd = format!(
                        "sudo bash deploy-relay.sh --port {port} --root-pubkey \"{pub_b64}\""
                    );
                    ui.ctx().copy_text(cmd);
                    toast(ui, state, Tone::Ok, "已复制服务器一键部署命令");
                }
                theme::hint(ui, "新增电脑 / 手机或吊销设备都无需再修改服务器配置");
            });

            theme::divider(ui);
            ui.label(theme::subheading("添加新电脑 / 手机（一次签发，无需改动服务器）"));
            ui.horizontal_wrapped(|ui| {
                let name_w = (ui.available_width() - 220.0).clamp(130.0, 230.0);
                theme::text_input(
                    ui,
                    &mut state.issue_device_name,
                    name_w,
                    "备注名称（可选，如 手机 / 家里电脑）",
                );
                if theme::button(ui, "生成绑定二维码与密钥", Kind::Primary).clicked() {
                    match conf.issue_device_bundle(cfg_path, &state.issue_device_name, 3650) {
                        Ok((bundle, record)) => match bundle.to_compact_token() {
                            Ok(token) => {
                                ui.ctx().copy_text(token.clone());
                                state.issued_qr_modules = build_qr_matrix(&token);
                                state.issued_label = record.label;
                                state.issued_bundle = Some(bundle);
                                state.issued_compact_token = token;
                                state.issue_device_name.clear();
                                *changed = true;
                                toast(ui, state, Tone::Ok, "已生成绑定凭证并复制 fdesk:// 短码");
                            }
                            Err(e) => toast(ui, state, Tone::Danger, format!("编码短码失败：{e}")),
                        },
                        Err(e) => toast(ui, state, Tone::Danger, format!("签发失败：{e}")),
                    }
                }
            });

            if !state.issued_compact_token.is_empty() {
                let token = state.issued_compact_token.clone();
                let label = state.issued_label.clone();
                let qr = state.issued_qr_modules.clone();
                let bundle_opt = state.issued_bundle.clone();
                theme::inset(ui, |ui| {
                    ui.label(theme::subheading(format!("已生成凭证：{label}")));
                    ui.horizontal_wrapped(|ui| {
                        if let Some((w, modules)) = qr {
                            paint_qr_code(ui, 168.0, w, &modules);
                        }
                        ui.vertical(|ui| {
                            ui.add(
                                egui::Label::new(theme::muted(
                                    "手机端可直接扫描左侧二维码；新电脑可直接复制短码，或保存为 .fdesk 文件后拖进新电脑的 FlashDesk 窗口（已包含中转服务器地址）。",
                                ))
                                .wrap(),
                            );
                            ui.add_space(4.0);
                            ui.horizontal_wrapped(|ui| {
                                if theme::button(ui, "复制 fdesk:// 短码", Kind::Primary).clicked()
                                {
                                    ui.ctx().copy_text(token.clone());
                                    toast(ui, state, Tone::Ok, "已复制绑定短码");
                                }
                                if let Some(bundle) = bundle_opt {
                                    if theme::button(ui, "保存 .fdesk 文件", Kind::Secondary)
                                        .clicked()
                                    {
                                        if let Some(path) = rfd::FileDialog::new()
                                            .set_file_name(format!("{label}.fdesk"))
                                            .save_file()
                                        {
                                            match bundle
                                                .to_json_pretty()
                                                .map_err(anyhow::Error::from)
                                                .and_then(|j| Ok(std::fs::write(&path, j)?))
                                            {
                                                Ok(()) => toast(
                                                    ui,
                                                    state,
                                                    Tone::Ok,
                                                    format!("已保存：{}", path.display()),
                                                ),
                                                Err(e) => toast(
                                                    ui,
                                                    state,
                                                    Tone::Danger,
                                                    format!("保存失败：{e}"),
                                                ),
                                            }
                                        }
                                    }
                                }
                                if theme::button(ui, "收起", Kind::Ghost).clicked() {
                                    state.issued_compact_token.clear();
                                    state.issued_bundle = None;
                                    state.issued_qr_modules = None;
                                }
                            });
                        });
                    });
                    ui.add_space(4.0);
                    egui::ScrollArea::vertical()
                        .id_salt("issued_token_preview")
                        .max_height(84.0)
                        .show(ui, |ui| {
                            if theme::code_box(ui, &token) {
                                toast(ui, state, Tone::Ok, "已复制绑定短码");
                            }
                        });
                });
            }
        } else {
            ui.add(
                egui::Label::new(theme::muted(
                    "当前目录未检测到匹配的个人根私钥 (root.key)：可以作为主设备新建根密钥，或导入主电脑签发的 .fdesk 配置包。",
                ))
                .wrap(),
            );
            ui.horizontal_wrapped(|ui| {
                if theme::button(ui, "生成个人根密钥（主电脑）", Kind::Primary).clicked() {
                    match conf.init_new_personal_root(cfg_path, default_hostname(), default_relays())
                    {
                        Ok(()) => {
                            *changed = true;
                            toast(ui, state, Tone::Ok, "个人根密钥已创建并保存至 root.key");
                        }
                        Err(e) => toast(ui, state, Tone::Danger, format!("创建失败：{e}")),
                    }
                }
                if theme::button(ui, "导入 .fdesk 文件", Kind::Secondary).clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("FlashDesk 配置包", &["fdesk", "json"])
                        .pick_file()
                    {
                        match std::fs::read_to_string(&path)
                            .map_err(anyhow::Error::from)
                            .and_then(|s| Ok(DeviceBundle::from_any_str(&s)?))
                        {
                            Ok(bundle) => {
                                conf.import_bundle(bundle);
                                *changed = true;
                                toast(ui, state, Tone::Ok, "导入成功");
                            }
                            Err(e) => toast(ui, state, Tone::Danger, format!("导入失败：{e}")),
                        }
                    }
                }
            });
        }

        theme::divider(ui);
        ui.label(theme::subheading("粘贴绑定短码替换本机身份"));
        ui.horizontal(|ui| {
            let w = (ui.available_width() - 80.0).clamp(140.0, 460.0);
            theme::text_input(
                ui,
                &mut state.import_token_input,
                w,
                "粘贴 fdesk://v2/... 短码或旧版导入码",
            );
            if theme::button(ui, "应用", Kind::Secondary).clicked() {
                let token = state.import_token_input.trim().to_string();
                match DeviceBundle::from_any_str(&token) {
                    _ if token.is_empty() => toast(ui, state, Tone::Warn, "请先粘贴绑定短码"),
                    Ok(b) => {
                        conf.import_bundle(b);
                        state.import_token_input.clear();
                        *changed = true;
                        toast(ui, state, Tone::Ok, "已应用新的设备配置包");
                    }
                    Err(e) => toast(ui, state, Tone::Danger, format!("导入码无效：{e}")),
                }
            }
        });
    });
}

/// Relay table column widths: fixed ID / role / action columns, the name and address inputs
/// share whatever the card has left, so the table never runs past the card on small windows.
const NODE_ID_W: f32 = 84.0;
const NODE_ROLE_W: f32 = 104.0;
const NODE_ACTION_W: f32 = 54.0;
const NODE_GRID_GAP: f32 = 10.0;

fn role_combo(ui: &mut egui::Ui, id: &str, role: &mut RelayNodeRole) -> bool {
    let mut changed = false;
    egui::ComboBox::from_id_salt(id)
        .selected_text(role.label())
        .width(NODE_ROLE_W)
        .show_ui(ui, |ui| {
            for r in [
                RelayNodeRole::PrimaryControl,
                RelayNodeRole::FilePreferred,
                RelayNodeRole::Backup,
            ] {
                changed |= ui.selectable_value(role, r, r.label()).changed();
            }
        });
    changed
}

fn relays_card(
    ui: &mut egui::Ui,
    state: &mut SettingsState,
    conf: &mut AppConfig,
    changed: &mut bool,
) {
    theme::card(ui, |ui| {
        theme::card_header(
            ui,
            "中转节点",
            Some("你的自建中转服务器；修改名称或地址后按 Enter / 点击别处即保存"),
            |_| {},
        );
        let Some(bundle) = conf.bundle.as_mut() else {
            ui.label(theme::muted("请先在上方生成个人根密钥或导入配置包，然后再管理中转节点。"));
            return;
        };

        let mut remove_idx = None;
        let fixed = NODE_ID_W + NODE_ROLE_W + NODE_ACTION_W + 4.0 * NODE_GRID_GAP;
        let field_w = ((ui.available_width() - fixed) / 2.0).clamp(96.0, 220.0);
        egui::Grid::new("relay_nodes_grid")
            .num_columns(5)
            .spacing([NODE_GRID_GAP, 8.0])
            .show(ui, |ui| {
                for h in ["ID", "名称", "地址 (IP:端口)", "用途", ""] {
                    ui.label(theme::faint(h));
                }
                ui.end_row();

                for (idx, node) in bundle.relay_nodes.iter_mut().enumerate() {
                    theme::label_max_width(ui, theme::mono(&node.id), NODE_ID_W);
                    let r1 = theme::text_input(ui, &mut node.name, field_w, "名称");
                    let r2 = theme::text_input(ui, &mut node.addr, field_w, "1.2.3.4:9442");
                    if theme::committed(&r1) || theme::committed(&r2) {
                        *changed = true;
                    }
                    if role_combo(ui, &format!("relay_role_{idx}"), &mut node.role) {
                        *changed = true;
                    }
                    if theme::small_button(ui, "删除", Kind::Ghost).clicked() {
                        remove_idx = Some(idx);
                    }
                    ui.end_row();
                }

                theme::text_input(ui, &mut state.new_node_id, NODE_ID_W, "新节点 ID");
                theme::text_input(ui, &mut state.new_node_name, field_w, "名称，如 香港 1G");
                theme::text_input(ui, &mut state.new_node_addr, field_w, "1.2.3.4:9442");
                role_combo(ui, "new_node_role", &mut state.new_node_role);
                if theme::small_button(ui, "添加", Kind::Secondary).clicked() {
                    let id = state.new_node_id.trim().to_string();
                    let name = state.new_node_name.trim().to_string();
                    let addr = state.new_node_addr.trim().to_string();
                    if id.is_empty() || addr.is_empty() {
                        toast(ui, state, Tone::Warn, "请填写节点 ID 和地址");
                    } else if bundle.relay_nodes.iter().any(|n| n.id == id) {
                        toast(ui, state, Tone::Warn, format!("节点 ID「{id}」已存在"));
                    } else {
                        bundle.relay_nodes.push(RelayNodeConfig {
                            name: if name.is_empty() { id.clone() } else { name },
                            id,
                            addr,
                            role: state.new_node_role,
                        });
                        state.new_node_id.clear();
                        state.new_node_name.clear();
                        state.new_node_addr.clear();
                        *changed = true;
                        toast(ui, state, Tone::Ok, "已添加中转节点");
                    }
                }
                ui.end_row();
            });
        if let Some(idx) = remove_idx {
            bundle.relay_nodes.remove(idx);
            *changed = true;
        }
    });
}

fn stream_card(ui: &mut egui::Ui, conf: &mut AppConfig, changed: &mut bool) {
    theme::card(ui, |ui| {
        theme::card_header(
            ui,
            "画面与传输",
            Some("新建远控会话时的默认值，会话中也可以在顶部工具栏随时调整"),
            |_| {},
        );

        theme::form_row(ui, "目标帧率", |ui| {
            if theme::segmented(ui, &mut conf.default_fps, &[(30, "30 FPS"), (60, "60 FPS")]) {
                *changed = true;
            }
        });
        theme::form_row(ui, "码率上限", |ui| {
            let mut mbps = conf.default_bitrate_kbps as f32 / 1000.0;
            let resp = ui.add(
                egui::Slider::new(&mut mbps, 1.0..=50.0)
                    .step_by(0.5)
                    .fixed_decimals(1)
                    .suffix(" Mbps"),
            );
            if resp.changed() {
                conf.default_bitrate_kbps = (mbps * 1000.0).round() as u32;
            }
            if resp.drag_stopped() || resp.lost_focus() || (resp.changed() && !resp.dragged()) {
                *changed = true;
            }
            theme::hint(ui, "自适应码率的上限，网络变差时自动下调");
        });
        theme::form_row(ui, "分辨率上限", |ui| {
            if theme::segmented(
                ui,
                &mut conf.stream_max_height,
                &[(0, "原始"), (1080, "1080p"), (720, "720p")],
            ) {
                *changed = true;
            }
            theme::hint(ui, "2K / 4K 屏走 5M 小带宽中转建议 1080p");
        });

        let relay_list = conf
            .bundle
            .as_ref()
            .map(|b| b.relay_nodes.clone())
            .unwrap_or_default();
        if !relay_list.is_empty() {
            theme::form_row(ui, "文件传输节点", |ui| {
                const AUTO: &str = "自动（优先「大文件高速」节点）";
                let cur_name = conf
                    .preferred_file_relay_id
                    .as_ref()
                    .and_then(|id| relay_list.iter().find(|r| &r.id == id))
                    .map(|r| r.name.clone())
                    .unwrap_or_else(|| AUTO.into());
                egui::ComboBox::from_id_salt("pref_file_relay_cfg")
                    .selected_text(cur_name)
                    .width(280.0)
                    .show_ui(ui, |ui| {
                        if ui
                            .selectable_label(conf.preferred_file_relay_id.is_none(), AUTO)
                            .clicked()
                        {
                            conf.preferred_file_relay_id = None;
                            *changed = true;
                        }
                        for r in &relay_list {
                            let sel = conf.preferred_file_relay_id.as_deref() == Some(&r.id);
                            if ui
                                .selectable_label(sel, format!("{}（{}）", r.name, r.role.label()))
                                .clicked()
                            {
                                conf.preferred_file_relay_id = Some(r.id.clone());
                                *changed = true;
                            }
                        }
                    });
            });
        }

        theme::form_row(ui, "直连监听端口", |ui| {
            let resp = ui.add(egui::DragValue::new(&mut conf.direct_listen_port).range(1024..=65535));
            if resp.drag_stopped() || resp.lost_focus() {
                *changed = true;
            }
            theme::hint(ui, "局域网直连使用的 TCP 端口，修改后重启 FlashDesk 生效");
        });
    });
}

fn unattended_card(
    ui: &mut egui::Ui,
    state: &mut SettingsState,
    cfg_path: &Path,
    conf: &mut AppConfig,
    sys: &SysStatus,
    changed: &mut bool,
) {
    theme::card(ui, |ui| {
        theme::card_header(
            ui,
            "无人值守与虚拟显示器",
            Some("系统服务可远控锁屏、登录界面和 UAC；虚拟屏让没有显示器的电脑也能出画面"),
            |_| {},
        );

        let (dot, text) = service_summary(sys);
        theme::form_row(ui, "系统服务", |ui| {
            theme::status_dot(ui, dot);
            ui.add(egui::Label::new(RichText::new(text).color(c::TEXT)).wrap())
                .on_hover_text(sys.service.label());
        });
        theme::form_row(ui, "", |ui| {
            if sys.service != ServiceState::Running {
                if theme::small_button(ui, "安装并启动服务", Kind::Primary)
                    .on_hover_text("需要管理员权限 (UAC)")
                    .clicked()
                {
                    match install_service(cfg_path, conf) {
                        Ok(()) => toast(ui, state, Tone::Info, "已请求安装并启动 FlashDeskService（请在 UAC 弹窗中确认）"),
                        Err(e) => toast(ui, state, Tone::Danger, format!("安装失败：{e}")),
                    }
                }
            } else {
                let sync_kind = if sys.service_config_synced {
                    Kind::Secondary
                } else {
                    Kind::Primary
                };
                if theme::small_button(ui, "同步配置到服务", sync_kind)
                    .on_hover_text("把当前配置写入 C:\\ProgramData\\FlashDesk\\config.json，3 秒内生效")
                    .clicked()
                {
                    match sync_service_config(cfg_path, conf) {
                        Ok(()) => toast(ui, state, Tone::Ok, "已同步最新配置到系统服务（3 秒内生效）"),
                        Err(e) => toast(ui, state, Tone::Danger, format!("同步失败：{e}")),
                    }
                }
                if theme::small_button(ui, "停止并卸载", Kind::Danger).clicked() {
                    match launch_elevated_service_cmd("--uninstall-service") {
                        Ok(()) => toast(ui, state, Tone::Info, "已请求停止并卸载系统服务"),
                        Err(e) => toast(ui, state, Tone::Danger, format!("卸载失败：{e}")),
                    }
                }
            }
        });

        theme::divider(ui);
        theme::form_row(ui, "虚拟显示器", |ui| {
            ui.add(egui::Label::new(RichText::new(&sys.vdd_details).color(c::TEXT)).wrap());
        });
        theme::form_row(ui, "", |ui| {
            // Wraps onto a second line on narrow windows.
            ui.horizontal_wrapped(|ui| {
                let mut vdd = |ui: &mut egui::Ui, label: &str, req: VddRequest| {
                    if theme::small_button(ui, label, Kind::Secondary).clicked() {
                        match handle_vdd_request(&req) {
                            Ok(info) => toast(ui, state, Tone::Ok, format!("虚拟屏状态：{}", info.details)),
                            Err(e) => toast(
                                ui,
                                state,
                                Tone::Danger,
                                format!("操作失败（需先安装虚拟显示驱动）：{e}"),
                            ),
                        }
                    }
                };
                vdd(
                    ui,
                    "启用 1080P 60Hz",
                    VddRequest::AddDisplay {
                        width: 1920,
                        height: 1080,
                        refresh_hz: 60,
                    },
                );
                vdd(
                    ui,
                    "启用 2K 60Hz",
                    VddRequest::AddDisplay {
                        width: 2560,
                        height: 1440,
                        refresh_hz: 60,
                    },
                );
                vdd(ui, "移除虚拟屏", VddRequest::RemoveDisplay);
            });
        });
        theme::form_row(ui, "无显示器时", |ui| {
            if theme::toggle(ui, &mut conf.auto_vdd_on_headless).changed() {
                *changed = true;
            }
            ui.add(egui::Label::new(RichText::new("被控端没有实体显示器时自动创建虚拟屏").color(c::TEXT_2)).wrap());
        });
        theme::form_row(ui, "驱动下载", |ui| {
            if theme::small_button(ui, "Parsec VDD ↗", Kind::Ghost).clicked() {
                open_url(PARSEC_VDD_DOWNLOAD_URL);
            }
            if theme::small_button(ui, "Virtual-Display-Driver ↗", Kind::Ghost).clicked() {
                open_url(MTT_VDD_DOWNLOAD_URL);
            }
        });
    });
}

fn revoke_card(
    ui: &mut egui::Ui,
    state: &mut SettingsState,
    cfg_path: &Path,
    conf: &mut AppConfig,
    st: &RelayStatus,
    relay_cmd_tx: &mpsc::UnboundedSender<RelayCommand>,
    changed: &mut bool,
) {
    let has_root = conf.has_root_key(cfg_path);
    theme::card(ui, |ui| {
        theme::card_header(
            ui,
            "已签发设备与远程吊销",
            Some("主电脑签发过的设备会记录在这里；点「吊销」后由根私钥签名并实时踢除，无需再修改服务器"),
            |_| {},
        );

        if st.connected {
            let sync_note = if let Some((count, _)) = st.last_revocation_sync {
                format!("已向当前中转同步吊销名单（{count} 台）")
            } else {
                "当前中转已连接，吊销变更将实时同步".into()
            };
            ui.label(theme::faint(sync_note));
        }

        let mut toggle_revoke: Option<Uuid> = None;
        let mut ledger_ids: HashSet<Uuid> = HashSet::new();

        for rec in conf.issued_devices.iter().rev() {
            ledger_ids.insert(rec.device_id);
            let online = st.devices.iter().find(|d| d.device_id == rec.device_id);
            let is_revoked = conf.revoked_device_ids.contains(&rec.device_id);
            let id_str = rec.device_id.to_string();
            let title = match online {
                Some(d) if d.device_name != rec.label => {
                    format!("{}（备注：{}）", d.device_name, rec.label)
                }
                Some(d) => d.device_name.clone(),
                None => rec.label.clone(),
            };

            theme::inset(ui, |ui| {
                ui.horizontal(|ui| {
                    theme::status_dot(
                        ui,
                        if is_revoked {
                            c::DANGER
                        } else if online.is_some() {
                            c::OK
                        } else {
                            c::TEXT_3
                        },
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if is_revoked {
                            if theme::small_button(ui, "解除吊销", Kind::Secondary).clicked() {
                                toggle_revoke = Some(rec.device_id);
                            }
                        } else if theme::small_button(ui, "吊销", Kind::Danger).clicked() {
                            toggle_revoke = Some(rec.device_id);
                        }
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            ui.add(egui::Label::new(theme::bold(&title).size(13.5)).truncate());
                            ui.label(theme::mono(format!("{}…", &id_str[..8])))
                                .on_hover_text(&id_str);
                            if is_revoked {
                                theme::chip(ui, "已吊销", c::DANGER, Color32::from_rgb(56, 24, 28));
                            } else if online.is_some() {
                                theme::chip(ui, "在线", c::OK, c::ACCENT_SOFT);
                            }
                        });
                    });
                });
            });
        }

        for u in &conf.revoked_device_ids {
            if !ledger_ids.contains(u) {
                let id_str = u.to_string();
                theme::inset(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if theme::small_button(ui, "解除吊销", Kind::Ghost).clicked() {
                                toggle_revoke = Some(*u);
                            }
                            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                                ui.add(egui::Label::new(theme::mono(&id_str)).truncate());
                            });
                        });
                    });
                });
            }
        }

        if conf.issued_devices.is_empty() && conf.revoked_device_ids.is_empty() {
            ui.label(theme::faint("暂无已签发的设备记录；在上方为新设备生成凭证后会自动显示在这里。"));
        }

        if let Some(uid) = toggle_revoke {
            if let Some(pos) = conf.revoked_device_ids.iter().position(|id| *id == uid) {
                conf.revoked_device_ids.remove(pos);
            } else {
                conf.revoked_device_ids.push(uid);
            }
            *changed = true;
            if has_root {
                match conf.sign_current_revocations(cfg_path) {
                    Ok(cmd) => {
                        let _ = relay_cmd_tx.send(RelayCommand::SyncRevocations(cmd));
                        toast(
                            ui,
                            state,
                            Tone::Ok,
                            if st.connected {
                                "已签名并推送吊销变更到中转服务器"
                            } else {
                                "已签名保存吊销名单，连接中转后将自动同步"
                            },
                        );
                    }
                    Err(e) => toast(ui, state, Tone::Danger, format!("签名吊销名单失败：{e}")),
                }
            } else {
                toast(ui, state, Tone::Warn, "本地已更新（无根私钥，无法向中转远程同步）");
            }
        }

        theme::divider(ui);
        ui.horizontal(|ui| {
            let uuid_w = (ui.available_width() - 130.0).clamp(130.0, 340.0);
            theme::text_input(ui, &mut state.revoke_uuid_input, uuid_w, "手动输入设备 UUID");
            if theme::button(ui, "加入吊销名单", Kind::Danger).clicked() {
                match Uuid::parse_str(state.revoke_uuid_input.trim()) {
                    Ok(uid) => {
                        if !conf.revoked_device_ids.contains(&uid) {
                            conf.revoked_device_ids.push(uid);
                            *changed = true;
                        }
                        state.revoke_uuid_input.clear();
                        if has_root {
                            match conf.sign_current_revocations(cfg_path) {
                                Ok(cmd) => {
                                    let _ = relay_cmd_tx.send(RelayCommand::SyncRevocations(cmd));
                                    toast(ui, state, Tone::Ok, "已加入吊销名单并同步到中转服务器");
                                }
                                Err(e) => {
                                    toast(ui, state, Tone::Danger, format!("同步吊销失败：{e}"))
                                }
                            }
                        } else {
                            toast(ui, state, Tone::Ok, "已加入本地吊销名单");
                        }
                    }
                    Err(_) => toast(ui, state, Tone::Warn, "UUID 格式不正确"),
                }
            }
        });
    });
}
