use crate::capture::monitors::MonitorTarget;
use crate::codec::CodecCapabilities;
use crate::config::{default_hostname, AppConfig};
use crate::net::relay_client::RelayCommand;
use crate::net::{local_lan_ip, RelayStatus};
use crate::service::install::{launch_elevated_service_cmd, query_service_state, ServiceState};
use crate::vdd::detect_vdd_status;
use eframe::egui::{self, Color32, RichText};
use protocol::auth::{DeviceBundle, RelayNodeConfig, RelayNodeRole};
use protocol::relay::OnlineDevice;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

pub enum HomeAction {
    None,
    ConnectRelay(OnlineDevice),
    ConnectDirect(String),
    GoToSettings,
    ConfigChanged,
}

pub struct HomeState {
    pub direct_addr_input: String,
    pub import_token_input: String,
    pub show_token_import: bool,
    pub status_banner: Option<String>,
}

impl Default for HomeState {
    fn default() -> Self {
        Self {
            direct_addr_input: "192.168.1.100:19444".into(),
            import_token_input: String::new(),
            show_token_import: false,
            status_banner: None,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn render_home(
    ui: &mut egui::Ui,
    state: &mut HomeState,
    cfg_path: &Path,
    cfg: &Arc<Mutex<AppConfig>>,
    relay_status: &Arc<Mutex<RelayStatus>>,
    relay_cmd_tx: &mpsc::UnboundedSender<RelayCommand>,
    caps: &CodecCapabilities,
    monitors: &[MonitorTarget],
) -> HomeAction {
    let mut action = HomeAction::None;

    let mut cfg_guard = cfg.lock().unwrap();
    let st = relay_status.lock().unwrap().clone();
    let svc_state = query_service_state();
    let vdd_info = detect_vdd_status();

    if let Some(banner) = state.status_banner.clone() {
        egui::Frame::group(ui.style())
            .fill(Color32::from_rgb(24, 48, 44))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(Color32::from_rgb(130, 240, 200), &banner);
                    if ui.button("关闭").clicked() {
                        state.status_banner = None;
                    }
                });
            });
        ui.add_space(6.0);
    }

    // 1. 本机信息卡
    egui::Frame::group(ui.style())
        .fill(Color32::from_rgb(22, 26, 35))
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading(RichText::new("本机状态与编码能力").color(Color32::WHITE));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .checkbox(&mut cfg_guard.allow_incoming, "允许被远程控制")
                        .changed()
                    {
                        action = HomeAction::ConfigChanged;
                    }
                });
            });
            ui.separator();

            if let Some(bundle) = &cfg_guard.bundle {
                let id_str = bundle.identity.cert.device_id.to_string();
                let short_id = id_str[..8].to_string();
                let dev_name = bundle.identity.cert.device_name.clone();
                let direct_port = cfg_guard.direct_listen_port;
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(format!("设备名称: {dev_name}"))
                            .strong()
                            .size(15.0),
                    );
                    ui.label(
                        RichText::new(format!("ID: {short_id}..."))
                            .monospace()
                            .color(Color32::LIGHT_GRAY),
                    );
                    if ui.small_button("复制完整 ID").clicked() {
                        ui.ctx().copy_text(id_str.clone());
                        state.status_banner = Some("已复制本机设备 ID".into());
                    }
                    ui.label(
                        RichText::new(format!(
                            "局域网直连: {}:{}",
                            local_lan_ip(),
                            direct_port
                        ))
                        .monospace()
                        .color(Color32::from_rgb(130, 200, 255)),
                    );
                });
            } else {
                ui.colored_label(
                    Color32::from_rgb(255, 200, 100),
                    "尚未初始化个人根密钥或导入 .fdesk 设备配置包。",
                );
                ui.horizontal(|ui| {
                    if ui.button("🚀 一键初始化个人根密钥（主设备）").clicked() {
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
                        cfg_guard.init_new_personal_root(default_hostname(), default_relays);
                        action = HomeAction::ConfigChanged;
                        state.status_banner = Some(
                            "已生成个人根密钥并激活本机身份！请前往「设置与密钥」填入你的云服务器 IP。".into(),
                        );
                    }
                    if ui.button("📂 导入 .fdesk 配置文件").clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("FlashDesk 配置包", &["fdesk", "json"])
                            .pick_file()
                        {
                            match std::fs::read_to_string(&path)
                                .map_err(anyhow::Error::from)
                                .and_then(|s| Ok(DeviceBundle::from_json_str(&s)?))
                            {
                                Ok(bundle) => {
                                    cfg_guard.import_bundle(bundle);
                                    action = HomeAction::ConfigChanged;
                                    state.status_banner =
                                        Some("成功导入设备配置包并已自动连接中转节点！".into());
                                }
                                Err(e) => {
                                    state.status_banner = Some(format!("导入失败: {e}"));
                                }
                            }
                        }
                    }
                    if ui.button("📋 粘贴配置 Token").clicked() {
                        state.show_token_import = !state.show_token_import;
                    }
                });

                if state.show_token_import {
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut state.import_token_input);
                        if ui.button("确认导入").clicked() {
                            match DeviceBundle::from_b64_token(&state.import_token_input) {
                                Ok(bundle) => {
                                    cfg_guard.import_bundle(bundle);
                                    state.import_token_input.clear();
                                    state.show_token_import = false;
                                    action = HomeAction::ConfigChanged;
                                    state.status_banner = Some("Token 导入成功！".into());
                                }
                                Err(e) => {
                                    state.status_banner = Some(format!("Token 解析失败: {e}"));
                                }
                            }
                        }
                    });
                }
            }

            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.label("编码后端:");
                for b in &caps.encode_backends {
                    ui.label(
                        RichText::new(format!(" [{b}] "))
                            .background_color(Color32::from_rgb(28, 60, 52))
                            .color(Color32::from_rgb(130, 245, 200)),
                    );
                }
                ui.label(" | 显示器:");
                for m in monitors {
                    ui.label(
                        RichText::new(format!(
                            " #{}: {}x{} ({}dpi) ",
                            m.info.index + 1,
                            m.info.width,
                            m.info.height,
                            m.info.dpi
                        ))
                        .background_color(Color32::from_rgb(35, 42, 56))
                        .color(Color32::LIGHT_GRAY),
                    );
                }
            });

            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                let agent_ok = crate::service::health::agent_healthy();
                let (svc_color, mode_text) = match svc_state {
                    ServiceState::Running if agent_ok => (
                        Color32::from_rgb(90, 235, 160),
                        "系统服务运行中（支持锁屏/UAC）".to_string(),
                    ),
                    ServiceState::Running => (
                        Color32::from_rgb(255, 150, 90),
                        "服务已运行但桌面代理未启动：桌面端已接管被控（锁屏/UAC 不可用）".to_string(),
                    ),
                    other => (Color32::from_rgb(245, 200, 110), other.label().to_string()),
                };
                ui.colored_label(svc_color, format!("🛡 运行模式: {mode_text}"));
                if svc_state != ServiceState::Running {
                    if ui.small_button("一键安装系统服务 (支持锁屏/UAC)").clicked() {
                        let _ = cfg_guard.save(cfg_path);
                        let args = format!("--install-service --config \"{}\"", cfg_path.display());
                        match launch_elevated_service_cmd(&args) {
                            Ok(()) => {
                                state.status_banner =
                                    Some("已请求 UAC 安装并启动 FlashDeskService 系统服务。".into());
                            }
                            Err(e) => state.status_banner = Some(format!("服务安装失败: {e}")),
                        }
                    }
                } else {
                    if !agent_ok && ui.small_button("重启系统服务").clicked() {
                        match launch_elevated_service_cmd("--restart-service") {
                            Ok(()) => {
                                state.status_banner = Some(
                                    "已请求 UAC 重启 FlashDeskService；若代理仍无法启动，请查看 C:\\ProgramData\\FlashDesk\\flashdesk-service.log".into(),
                                );
                            }
                            Err(e) => state.status_banner = Some(format!("重启失败: {e}")),
                        }
                    }
                    if ui.small_button("卸载系统服务").clicked() {
                        match launch_elevated_service_cmd("--uninstall-service") {
                            Ok(()) => {
                                state.status_banner = Some("已请求卸载 FlashDeskService 系统服务。".into());
                            }
                            Err(e) => state.status_banner = Some(format!("卸载失败: {e}")),
                        }
                    }
                }
                ui.label(
                    RichText::new(format!(" | 虚拟屏: {}", vdd_info.details))
                        .color(Color32::LIGHT_GRAY),
                );
            });
        });

    ui.add_space(8.0);

    // 2. 中转节点切换与状态栏
    egui::Frame::group(ui.style())
        .fill(Color32::from_rgb(22, 26, 35))
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (dot_color, status_text) = if st.connected {
                    let rtt = st
                        .rtt_ms
                        .map(|ms| format!(" ({ms} ms)"))
                        .unwrap_or_default();
                    (
                        Color32::from_rgb(60, 220, 140),
                        format!("● 已连接中转: {} [{}]{}", st.relay_name, st.relay_addr, rtt),
                    )
                } else {
                    let err = st
                        .error
                        .as_deref()
                        .unwrap_or("未连接（可在设置中添加/修改服务器地址）");
                    (
                        Color32::from_rgb(240, 100, 100),
                        format!("● 未连接中转: {err}"),
                    )
                };
                ui.colored_label(dot_color, RichText::new(status_text).strong());

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⚙ 管理节点与密钥").clicked() {
                        action = HomeAction::GoToSettings;
                    }
                    if ui.button("🔄 刷新设备").clicked() {
                        let _ = relay_cmd_tx.send(RelayCommand::RefreshDevices);
                    }
                });
            });

            let relay_nodes = cfg_guard
                .bundle
                .as_ref()
                .map(|b| b.relay_nodes.clone())
                .unwrap_or_default();
            if !relay_nodes.is_empty() {
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label("快速切换主控中转节点:");
                    let active_id = cfg_guard.selected_relay_id.clone().unwrap_or_default();
                    for node in &relay_nodes {
                        let selected = node.id == active_id;
                        let label = format!("{} ({})", node.name, node.role.label());
                        if ui.selectable_label(selected, label).clicked() && !selected {
                            cfg_guard.selected_relay_id = Some(node.id.clone());
                            action = HomeAction::ConfigChanged;
                        }
                    }
                });
            }
        });

    ui.add_space(8.0);

    // 3. 我的在线设备列表
    egui::Frame::group(ui.style())
        .fill(Color32::from_rgb(22, 26, 35))
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.heading(RichText::new("我的在线设备（Ed25519 白名单免密直连）").color(Color32::WHITE));
            ui.separator();

            let self_id = cfg_guard
                .bundle
                .as_ref()
                .map(|b| b.identity.cert.device_id);

            if st.devices.is_empty() {
                ui.label(
                    RichText::new("当前没有检测到其他在线设备。请将同一根密钥签发的 .fdesk 配置包导入另一台电脑。")
                        .color(Color32::GRAY),
                );
            } else {
                for dev in &st.devices {
                    let is_self = self_id == Some(dev.device_id);
                    egui::Frame::NONE
                        .fill(Color32::from_rgb(30, 35, 47))
                        .corner_radius(egui::CornerRadius::same(6))
                        .inner_margin(egui::Margin::same(10))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.colored_label(Color32::from_rgb(80, 225, 140), "●");
                                ui.label(RichText::new(&dev.device_name).strong().size(15.0));
                                if is_self {
                                    ui.label(
                                        RichText::new(" (本机)")
                                            .color(Color32::from_rgb(130, 200, 255)),
                                    );
                                }
                                let codecs_str = if dev.encode_codecs.is_empty() {
                                    "未知编码".into()
                                } else {
                                    dev.encode_codecs
                                        .iter()
                                        .map(|c| c.label())
                                        .collect::<Vec<_>>()
                                        .join("/")
                                };
                                ui.label(
                                    RichText::new(format!(
                                        "{} | {} 屏 | {}",
                                        dev.os, dev.monitor_count, codecs_str
                                    ))
                                    .color(Color32::LIGHT_GRAY),
                                );

                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if !is_self && ui.button("🖥 连接控制").clicked() {
                                            action = HomeAction::ConnectRelay(dev.clone());
                                        }
                                    },
                                );
                            });
                        });
                    ui.add_space(4.0);
                }
            }
        });

    ui.add_space(8.0);

    // 4. 局域网 / IP 直连
    egui::Frame::group(ui.style())
        .fill(Color32::from_rgb(22, 26, 35))
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("局域网 / IP 直连（绕过中转服，同享端到端加密）:").strong());
                ui.add(
                    egui::TextEdit::singleline(&mut state.direct_addr_input)
                        .desired_width(210.0)
                        .hint_text("192.168.1.100:19444"),
                );
                let has_bundle = cfg_guard.bundle.is_some();
                if ui
                    .add_enabled(has_bundle, egui::Button::new("⚡ 直连控制"))
                    .clicked()
                {
                    action = HomeAction::ConnectDirect(state.direct_addr_input.trim().to_string());
                }
            });
        });

    action
}
