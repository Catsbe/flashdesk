use crate::config::{default_hostname, AppConfig};
use crate::service::install::{launch_elevated_service_cmd, query_service_state, ServiceState};
use crate::vdd::{
    detect_vdd_status, handle_vdd_request, open_url, MTT_VDD_DOWNLOAD_URL, PARSEC_VDD_DOWNLOAD_URL,
};
use eframe::egui::{self, Color32, RichText};
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
    pub notice: Option<String>,
}

impl Default for SettingsState {
    fn default() -> Self {
        Self {
            new_node_id: "node-custom".into(),
            new_node_name: "新中转节点".into(),
            new_node_addr: "1.2.3.4:19443".into(),
            new_node_role: RelayNodeRole::PrimaryControl,
            issue_device_name: "办公电脑".into(),
            issued_token_preview: String::new(),
            revoke_uuid_input: String::new(),
            import_token_input: String::new(),
            notice: None,
        }
    }
}

pub fn render_settings(
    ui: &mut egui::Ui,
    state: &mut SettingsState,
    cfg_path: &Path,
    cfg: &Arc<Mutex<AppConfig>>,
) -> bool {
    let mut changed = false;
    let mut c = cfg.lock().unwrap();
    let svc_state = query_service_state();
    let vdd_info = detect_vdd_status();

    if let Some(msg) = state.notice.clone() {
        egui::Frame::group(ui.style())
            .fill(Color32::from_rgb(24, 48, 44))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(Color32::from_rgb(130, 240, 200), &msg);
                    if ui.button("关闭").clicked() {
                        state.notice = None;
                    }
                });
            });
        ui.add_space(6.0);
    }

    // 1. 个人根密钥与身份配置包
    egui::Frame::group(ui.style())
        .fill(Color32::from_rgb(22, 26, 35))
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.heading("个人根密钥与设备授权（Ed25519 Root CA）");
            ui.separator();

            if let Some(secret_b64) = &c.local_root_secret_b64 {
                if let Ok(kp) = RootKeypair::from_b64(secret_b64) {
                    let pub_b64 = kp.pubkey_b64();
                    ui.horizontal_wrapped(|ui| {
                        ui.label("服务端根公钥:");
                        ui.label(
                            RichText::new(&pub_b64)
                                .monospace()
                                .color(Color32::from_rgb(130, 220, 255)),
                        );
                        if ui.button("复制公钥").clicked() {
                            ui.ctx().copy_text(pub_b64.clone());
                            state.notice = Some("已复制根公钥，可粘贴到云服务器的 relay.toml".into());
                        }
                        if ui.button("📋 复制完整 relay.toml").clicked() {
                            let revoked_toml = c
                                .revoked_device_ids
                                .iter()
                                .map(|u| format!("\"{u}\""))
                                .collect::<Vec<_>>()
                                .join(", ");
                            let toml_text = format!(
                                "listen_addr = \"0.0.0.0:19443\"\nroot_pubkey_b64 = \"{pub_b64}\"\nrevoked_device_ids = [{revoked_toml}]\npending_timeout_secs = 15\n"
                            );
                            ui.ctx().copy_text(toml_text);
                            state.notice = Some("已复制完整 relay.toml 配置到剪贴板！".into());
                        }
                    });
                }

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("为新电脑签发配置包 — 设备名:");
                    ui.add(
                        egui::TextEdit::singleline(&mut state.issue_device_name)
                            .desired_width(160.0),
                    );
                    if ui.button("💾 签发并保存 .fdesk 文件").clicked() {
                        let name = state.issue_device_name.trim();
                        if !name.is_empty() {
                            match c.issue_device_bundle(name, 3650) {
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
                                            Ok(()) => {
                                                state.notice = Some(format!(
                                                    "已签发新设备包: {}",
                                                    path.display()
                                                ));
                                            }
                                            Err(e) => {
                                                state.notice = Some(format!("保存失败: {e}"));
                                            }
                                        }
                                    }
                                }
                                Err(e) => state.notice = Some(format!("签发失败: {e}")),
                            }
                        }
                    }
                    if ui.button("📋 生成 Base64 导入码").clicked() {
                        let name = state.issue_device_name.trim();
                        if !name.is_empty() {
                            match c
                                .issue_device_bundle(name, 3650)
                                .and_then(|b| Ok(b.to_b64_token()?))
                            {
                                Ok(token) => {
                                    state.issued_token_preview = token.clone();
                                    ui.ctx().copy_text(token);
                                    state.notice =
                                        Some("已生成配置 Token 并复制到剪贴板！".into());
                                }
                                Err(e) => state.notice = Some(format!("生成失败: {e}")),
                            }
                        }
                    }
                });

                if !state.issued_token_preview.is_empty() {
                    ui.add(
                        egui::TextEdit::multiline(&mut state.issued_token_preview)
                            .desired_rows(2)
                            .desired_width(f32::INFINITY),
                    );
                }
            } else {
                ui.label(
                    RichText::new(
                        "当前电脑未保存个人根私钥（可通过初始化新建根密钥，或导入其他主设备签发的 .fdesk）。",
                    )
                    .color(Color32::LIGHT_GRAY),
                );
                ui.horizontal(|ui| {
                    if ui.button("🚀 初始化个人根密钥").clicked() {
                        let default_relays = vec![
                            RelayNodeConfig {
                                id: "cn-5m".into(),
                                name: "国内 5M (低延迟控屏)".into(),
                                addr: "127.0.0.1:19443".into(),
                                role: RelayNodeRole::PrimaryControl,
                            },
                            RelayNodeConfig {
                                id: "global-1g".into(),
                                name: "海外 1G (高速大文件)".into(),
                                addr: "127.0.0.1:19443".into(),
                                role: RelayNodeRole::FilePreferred,
                            },
                        ];
                        c.init_new_personal_root(default_hostname(), default_relays);
                        changed = true;
                        state.notice = Some("个人根密钥已创建！".into());
                    }
                    if ui.button("📂 导入 .fdesk 文件").clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("FlashDesk 配置包", &["fdesk", "json"])
                            .pick_file()
                        {
                            if let Ok(s) = std::fs::read_to_string(&path) {
                                if let Ok(bundle) = DeviceBundle::from_json_str(&s) {
                                    c.import_bundle(bundle);
                                    changed = true;
                                    state.notice = Some("导入成功！".into());
                                }
                            }
                        }
                    }
                });
            }

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("导入 Token 覆盖本机配置:");
                ui.text_edit_singleline(&mut state.import_token_input);
                if ui.button("应用 Token").clicked() {
                    match DeviceBundle::from_b64_token(&state.import_token_input) {
                        Ok(b) => {
                            c.import_bundle(b);
                            state.import_token_input.clear();
                            changed = true;
                            state.notice = Some("已应用新设备配置包！".into());
                        }
                        Err(e) => state.notice = Some(format!("解析失败: {e}")),
                    }
                }
            });
        });

    ui.add_space(8.0);

    // 2. 中转服务器节点管理
    egui::Frame::group(ui.style())
        .fill(Color32::from_rgb(22, 26, 35))
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.heading("自建中转节点列表（如国内 5M / 海外 1G）");
            ui.separator();

            if let Some(bundle) = &mut c.bundle {
                let mut remove_idx = None;
                for (idx, node) in bundle.relay_nodes.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&node.id).monospace().color(Color32::GRAY));
                        if ui
                            .add(egui::TextEdit::singleline(&mut node.name).desired_width(160.0))
                            .changed()
                        {
                            changed = true;
                        }
                        if ui
                            .add(egui::TextEdit::singleline(&mut node.addr).desired_width(170.0))
                            .changed()
                        {
                            changed = true;
                        }
                        ui.label(node.role.label());
                        if ui.button("删除").clicked() {
                            remove_idx = Some(idx);
                        }
                    });
                }
                if let Some(idx) = remove_idx {
                    bundle.relay_nodes.remove(idx);
                    changed = true;
                }

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("新增节点:");
                    ui.add(
                        egui::TextEdit::singleline(&mut state.new_node_id)
                            .desired_width(80.0)
                            .hint_text("id"),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut state.new_node_name)
                            .desired_width(140.0)
                            .hint_text("名称"),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut state.new_node_addr)
                            .desired_width(150.0)
                            .hint_text("IP:19443"),
                    );
                    egui::ComboBox::from_id_salt("new_node_role")
                        .selected_text(state.new_node_role.label())
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut state.new_node_role,
                                RelayNodeRole::PrimaryControl,
                                RelayNodeRole::PrimaryControl.label(),
                            );
                            ui.selectable_value(
                                &mut state.new_node_role,
                                RelayNodeRole::FilePreferred,
                                RelayNodeRole::FilePreferred.label(),
                            );
                            ui.selectable_value(
                                &mut state.new_node_role,
                                RelayNodeRole::Backup,
                                RelayNodeRole::Backup.label(),
                            );
                        });
                    if ui.button("➕ 添加").clicked() {
                        bundle.relay_nodes.push(RelayNodeConfig {
                            id: state.new_node_id.trim().to_string(),
                            name: state.new_node_name.trim().to_string(),
                            addr: state.new_node_addr.trim().to_string(),
                            role: state.new_node_role,
                        });
                        changed = true;
                    }
                });
            } else {
                ui.label("请先在上方初始化个人根密钥或导入配置包，然后再管理中转节点。");
            }
        });

    ui.add_space(8.0);

    // 3. Windows 无人值守系统服务与无显示器虚拟屏
    egui::Frame::group(ui.style())
        .fill(Color32::from_rgb(22, 26, 35))
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.heading("无人值守系统服务 & 无显示器虚拟屏（M3）");
            ui.separator();

            ui.horizontal_wrapped(|ui| {
                ui.label(format!("系统服务状态: {}", svc_state.label()));
                if svc_state != ServiceState::Running {
                    if ui.button("🛡 安装并启动后台服务 (UAC)").clicked() {
                        let _ = c.save(cfg_path);
                        let args = format!("--install-service --config \"{}\"", cfg_path.display());
                        match launch_elevated_service_cmd(&args) {
                            Ok(()) => {
                                state.notice = Some("已触发 UAC 安装并启动 FlashDeskService。".into());
                            }
                            Err(e) => state.notice = Some(format!("安装失败: {e}")),
                        }
                    }
                } else {
                    if ui.button("🔄 同步最新配置到系统服务").clicked() {
                        let _ = c.save(cfg_path);
                        let args = format!("--sync-service-config --config \"{}\"", cfg_path.display());
                        match launch_elevated_service_cmd(&args) {
                            Ok(()) => {
                                state.notice = Some("已将最新配置同步到 C:\\ProgramData\\FlashDesk\\config.json（3秒内热生效）。".into());
                            }
                            Err(e) => state.notice = Some(format!("同步失败: {e}")),
                        }
                    }
                    if ui.button("🛑 停止并卸载服务").clicked() {
                        match launch_elevated_service_cmd("--uninstall-service") {
                            Ok(()) => state.notice = Some("已请求停止并卸载后台服务。".into()),
                            Err(e) => state.notice = Some(format!("卸载失败: {e}")),
                        }
                    }
                }
            });

            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("虚拟屏驱动检测: {}", vdd_info.details));
                if ui.button("➕ 本机启用 1080P 60Hz 虚拟屏").clicked() {
                    match handle_vdd_request(&VddRequest::AddDisplay {
                        width: 1920,
                        height: 1080,
                        refresh_hz: 60,
                    }) {
                        Ok(info) => {
                            state.notice = Some(format!("已启用虚拟屏: {}", info.details));
                        }
                        Err(e) => {
                            state.notice = Some(format!(
                                "启用虚拟屏失败（需安装 Parsec VDD 驱动）: {e}"
                            ));
                        }
                    }
                }
                if ui.button("➕ 本机启用 2K 60Hz 虚拟屏").clicked() {
                    match handle_vdd_request(&VddRequest::AddDisplay {
                        width: 2560,
                        height: 1440,
                        refresh_hz: 60,
                    }) {
                        Ok(info) => state.notice = Some(format!("已启用虚拟屏: {}", info.details)),
                        Err(e) => state.notice = Some(format!("启用失败: {e}")),
                    }
                }
                if ui.button("🗑 移除本机虚拟屏").clicked() {
                    match handle_vdd_request(&VddRequest::RemoveDisplay) {
                        Ok(info) => state.notice = Some(format!("已更新虚拟屏状态: {}", info.details)),
                        Err(e) => state.notice = Some(format!("移除失败: {e}")),
                    }
                }
            });

            ui.horizontal_wrapped(|ui| {
                if ui
                    .checkbox(&mut c.auto_vdd_on_headless, "被控端无实体显示器时自动启用虚拟屏")
                    .changed()
                {
                    changed = true;
                }
                if ui.small_button("🌐 下载 Parsec VDD 签名驱动").clicked() {
                    open_url(PARSEC_VDD_DOWNLOAD_URL);
                }
                if ui.small_button("🌐 下载 Virtual-Display-Driver").clicked() {
                    open_url(MTT_VDD_DOWNLOAD_URL);
                }
            });
        });

    ui.add_space(8.0);

    // 4. 默认画面参数与设备吊销名单
    egui::Frame::group(ui.style())
        .fill(Color32::from_rgb(22, 26, 35))
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.heading("默认推流、文件分流与安全设置");
            ui.separator();

            ui.horizontal(|ui| {
                ui.label("默认目标帧率:");
                if ui.selectable_value(&mut c.default_fps, 30, "30 FPS").changed() {
                    changed = true;
                }
                if ui.selectable_value(&mut c.default_fps, 60, "60 FPS").changed() {
                    changed = true;
                }
                ui.separator();
                ui.label("码率上限 (kbps):")
                    .on_hover_text("自适应码率的上限；网络变差时会自动下调");
                if ui
                    .add(egui::Slider::new(&mut c.default_bitrate_kbps, 1000..=50_000))
                    .changed()
                {
                    changed = true;
                }
            });
            ui.horizontal(|ui| {
                ui.label("默认分辨率上限:");
                for (h, txt) in [(0u32, "原始"), (1080, "1080p"), (720, "720p")] {
                    if ui.selectable_value(&mut c.stream_max_height, h, txt).changed() {
                        changed = true;
                    }
                }
                ui.label(
                    RichText::new("（2K/4K 屏走 5M 小带宽中转建议 1080p）").color(Color32::GRAY),
                );
            });

            let relay_list = c
                .bundle
                .as_ref()
                .map(|b| b.relay_nodes.clone())
                .unwrap_or_default();
            if !relay_list.is_empty() {
                ui.horizontal(|ui| {
                    ui.label("文件传输优先节点:");
                    let cur_name = c
                        .preferred_file_relay_id
                        .as_ref()
                        .and_then(|id| relay_list.iter().find(|r| &r.id == id))
                        .map(|r| r.name.clone())
                        .unwrap_or_else(|| "自动（优先 FilePreferred 如海外 1G）".into());
                    egui::ComboBox::from_id_salt("pref_file_relay_cfg")
                        .selected_text(cur_name)
                        .width(260.0)
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_label(
                                    c.preferred_file_relay_id.is_none(),
                                    "自动（优先 FilePreferred 如海外 1G）",
                                )
                                .clicked()
                            {
                                c.preferred_file_relay_id = None;
                                changed = true;
                            }
                            for r in &relay_list {
                                let sel = c.preferred_file_relay_id.as_deref() == Some(&r.id);
                                if ui
                                    .selectable_label(sel, format!("{} ({})", r.name, r.role.label()))
                                    .clicked()
                                {
                                    c.preferred_file_relay_id = Some(r.id.clone());
                                    changed = true;
                                }
                            }
                        });
                });
            }

            ui.horizontal(|ui| {
                ui.label("直连监听端口:");
                if ui
                    .add(egui::DragValue::new(&mut c.direct_listen_port).range(1024..=65535))
                    .changed()
                {
                    changed = true;
                }
                ui.label("（修改端口后重启生效）");
            });

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("吊销设备 UUID:");
                ui.add(
                    egui::TextEdit::singleline(&mut state.revoke_uuid_input)
                        .desired_width(240.0),
                );
                if ui.button("加入吊销名单").clicked() {
                    if let Ok(uid) = Uuid::parse_str(state.revoke_uuid_input.trim()) {
                        if !c.revoked_device_ids.contains(&uid) {
                            c.revoked_device_ids.push(uid);
                            changed = true;
                        }
                        state.revoke_uuid_input.clear();
                    } else {
                        state.notice = Some("UUID 格式不正确".into());
                    }
                }
            });
            if !c.revoked_device_ids.is_empty() {
                let mut remove_u = None;
                for (i, u) in c.revoked_device_ids.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(u.to_string()).monospace());
                        if ui.small_button("解除").clicked() {
                            remove_u = Some(i);
                        }
                    });
                }
                if let Some(i) = remove_u {
                    c.revoked_device_ids.remove(i);
                    changed = true;
                }
            }
        });

    changed
}
