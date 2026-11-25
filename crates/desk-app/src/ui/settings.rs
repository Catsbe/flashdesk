use crate::config::{default_hostname, AppConfig};
use crate::service::install::{launch_elevated_service_cmd, ServiceState};
use crate::ui::home::{default_relays, install_service, service_summary, sync_service_config};
use crate::ui::sysinfo::SysStatus;
use crate::ui::theme::{self, c, Kind, Toast, Tone};
use crate::vdd::{handle_vdd_request, open_url, MTT_VDD_DOWNLOAD_URL, PARSEC_VDD_DOWNLOAD_URL};
use eframe::egui::{self, Align, Layout, RichText};
use protocol::auth::{DeviceBundle, RelayNodeConfig, RelayNodeRole, RootKeypair};
use protocol::session::VddRequest;
use std::path::Path;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

pub struct SettingsState {
    pub new_node_id: String,
    pub new_node_name: String,
    pub new_node_addr: String,
    pub new_node_role: RelayNodeRole,
    pub issue_device_name: String,
    pub issued_token_preview: String,
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
            issue_device_name: "办公电脑".into(),
            issued_token_preview: String::new(),
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
    sys: &SysStatus,
) -> bool {
    let mut changed = false;
    let mut conf = cfg.lock().unwrap();

    identity_card(ui, state, &mut conf, &mut changed);
    theme::gap(ui);
    relays_card(ui, state, &mut conf, &mut changed);
    theme::gap(ui);
    stream_card(ui, &mut conf, &mut changed);
    theme::gap(ui);
    unattended_card(ui, state, cfg_path, &mut conf, sys, &mut changed);
    theme::gap(ui);
    revoke_card(ui, state, &mut conf, &mut changed);

    theme::toast_area(ui.ctx(), &mut state.toast);
    changed
}

fn identity_card(
    ui: &mut egui::Ui,
    state: &mut SettingsState,
    conf: &mut AppConfig,
    changed: &mut bool,
) {
    theme::card(ui, |ui| {
        theme::card_header(
            ui,
            "身份与设备授权",
            Some("个人根密钥 (Ed25519) 为你的每台电脑签发身份，中转服务器只认这把公钥"),
            |_| {},
        );

        let root_pub = conf
            .local_root_secret_b64
            .as_ref()
            .and_then(|s| RootKeypair::from_b64(s).ok())
            .map(|kp| kp.pubkey_b64());

        if let Some(pub_b64) = root_pub {
            ui.label(theme::subheading("中转服务器根公钥"));
            if theme::code_box(ui, &pub_b64) {
                toast(ui, state, Tone::Ok, "已复制根公钥");
            }
            ui.horizontal(|ui| {
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
                theme::hint(ui, "包含根公钥与吊销名单");
            });

            theme::divider(ui);
            ui.label(theme::subheading("为新电脑签发配置包"));
            ui.horizontal(|ui| {
                let name_w = (ui.available_width() - 270.0).clamp(110.0, 200.0);
                theme::text_input(ui, &mut state.issue_device_name, name_w, "设备名称，如 办公电脑");
                if theme::button(ui, "保存为 .fdesk 文件", Kind::Primary).clicked() {
                    let name = state.issue_device_name.trim().to_string();
                    if name.is_empty() {
                        toast(ui, state, Tone::Warn, "请先填写设备名称");
                    } else {
                        match conf.issue_device_bundle(&name, 3650) {
                            Ok(bundle) => {
                                if let Some(path) = rfd::FileDialog::new()
                                    .set_file_name(format!("{name}.fdesk"))
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
                                            format!("已签发：{}", path.display()),
                                        ),
                                        Err(e) => {
                                            toast(ui, state, Tone::Danger, format!("保存失败：{e}"))
                                        }
                                    }
                                }
                            }
                            Err(e) => toast(ui, state, Tone::Danger, format!("签发失败：{e}")),
                        }
                    }
                }
                if theme::button(ui, "生成导入码", Kind::Secondary).clicked() {
                    let name = state.issue_device_name.trim().to_string();
                    if name.is_empty() {
                        toast(ui, state, Tone::Warn, "请先填写设备名称");
                    } else {
                        match conf
                            .issue_device_bundle(&name, 3650)
                            .and_then(|b| Ok(b.to_b64_token()?))
                        {
                            Ok(token) => {
                                ui.ctx().copy_text(token.clone());
                                state.issued_token_preview = token;
                                toast(ui, state, Tone::Ok, "导入码已生成并复制到剪贴板");
                            }
                            Err(e) => toast(ui, state, Tone::Danger, format!("生成失败：{e}")),
                        }
                    }
                }
            });
            if !state.issued_token_preview.is_empty() {
                let token = state.issued_token_preview.clone();
                egui::ScrollArea::vertical()
                    .id_salt("issued_token_preview")
                    .max_height(92.0)
                    .show(ui, |ui| {
                        if theme::code_box(ui, &token) {
                            toast(ui, state, Tone::Ok, "已复制导入码");
                        }
                    });
            }
        } else {
            ui.add(
                egui::Label::new(theme::muted(
                    "这台电脑没有保存个人根私钥：可以新建根密钥作为主设备，或导入主设备签发的 .fdesk 配置包。",
                ))
                .wrap(),
            );
            ui.horizontal(|ui| {
                if theme::button(ui, "生成个人根密钥", Kind::Primary).clicked() {
                    conf.init_new_personal_root(default_hostname(), default_relays());
                    *changed = true;
                    toast(ui, state, Tone::Ok, "个人根密钥已创建");
                }
                if theme::button(ui, "导入 .fdesk 文件", Kind::Secondary).clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("FlashDesk 配置包", &["fdesk", "json"])
                        .pick_file()
                    {
                        match std::fs::read_to_string(&path)
                            .map_err(anyhow::Error::from)
                            .and_then(|s| Ok(DeviceBundle::from_json_str(&s)?))
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
        ui.label(theme::subheading("用导入码替换本机身份"));
        ui.horizontal(|ui| {
            let w = (ui.available_width() - 80.0).clamp(200.0, 460.0);
            theme::text_input(ui, &mut state.import_token_input, w, "粘贴其它主设备生成的 Base64 导入码");
            if theme::button(ui, "应用", Kind::Secondary).clicked() {
                let token = state.import_token_input.trim().to_string();
                match DeviceBundle::from_b64_token(&token) {
                    _ if token.is_empty() => toast(ui, state, Tone::Warn, "请先粘贴导入码"),
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
    conf: &mut AppConfig,
    changed: &mut bool,
) {
    theme::card(ui, |ui| {
        theme::card_header(
            ui,
            "设备吊销",
            Some("被吊销的设备无法再连接你的电脑；同步到中转服务器的 relay.toml 后全局生效"),
            |_| {},
        );
        ui.horizontal(|ui| {
            let uuid_w = (ui.available_width() - 130.0).clamp(160.0, 340.0);
            theme::text_input(ui, &mut state.revoke_uuid_input, uuid_w, "要吊销的设备 UUID");
            if theme::button(ui, "加入吊销名单", Kind::Danger).clicked() {
                match Uuid::parse_str(state.revoke_uuid_input.trim()) {
                    Ok(uid) => {
                        if !conf.revoked_device_ids.contains(&uid) {
                            conf.revoked_device_ids.push(uid);
                            *changed = true;
                        }
                        state.revoke_uuid_input.clear();
                        toast(ui, state, Tone::Ok, "已加入吊销名单");
                    }
                    Err(_) => toast(ui, state, Tone::Warn, "UUID 格式不正确"),
                }
            }
        });
        if conf.revoked_device_ids.is_empty() {
            ui.label(theme::faint("吊销名单为空"));
        } else {
            let mut remove = None;
            for (i, u) in conf.revoked_device_ids.iter().enumerate() {
                theme::inset(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(theme::mono(u.to_string()));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if theme::small_button(ui, "解除", Kind::Ghost).clicked() {
                                remove = Some(i);
                            }
                        });
                    });
                });
            }
            if let Some(i) = remove {
                conf.revoked_device_ids.remove(i);
                *changed = true;
            }
        }
    });
}
