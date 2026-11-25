use crate::capture::monitors::MonitorTarget;
use crate::codec::CodecCapabilities;
use crate::config::{default_hostname, AppConfig};
use crate::net::relay_client::RelayCommand;
use crate::net::RelayStatus;
use crate::service::install::{launch_elevated_service_cmd, ServiceState};
use crate::ui::sysinfo::SysStatus;
use crate::ui::theme::{self, c, Kind, Toast, Tone};
use eframe::egui::{self, Align, Color32, Key, Layout, RichText};
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
    pub toast: Option<Toast>,
}

impl Default for HomeState {
    fn default() -> Self {
        Self {
            direct_addr_input: "192.168.1.100:19444".into(),
            import_token_input: String::new(),
            show_token_import: false,
            toast: None,
        }
    }
}

/// Relay list written into a freshly created personal root (edit the addresses in Settings).
pub(crate) fn default_relays() -> Vec<RelayNodeConfig> {
    vec![RelayNodeConfig {
        id: "cn-5m".into(),
        name: "国内 5M (低延迟控屏)".into(),
        addr: "127.0.0.1:9442".into(),
        role: RelayNodeRole::PrimaryControl,
    }]
}

/// Status dot colour + one-line description of how this machine runs.
pub(crate) fn service_summary(sys: &SysStatus) -> (Color32, &'static str) {
    match sys.service {
        ServiceState::Running if sys.agent_ok && sys.service_config_synced => {
            (c::OK, "系统服务运行中 · 支持锁屏 / 登录界面 / UAC")
        }
        ServiceState::Running if sys.agent_ok => {
            (c::WARN, "服务配置未同步 · 桌面端已自动接管被控（锁屏 / UAC 需同步）")
        }
        ServiceState::Running => (c::WARN, "服务已运行，但桌面代理未启动（锁屏 / UAC 不可用）"),
        ServiceState::Stopped => (c::WARN, "系统服务已安装，但未运行"),
        ServiceState::NotInstalled => (c::TEXT_3, "便携模式 · 未安装系统服务"),
        ServiceState::Unknown => (c::TEXT_3, "服务状态未知"),
    }
}

pub(crate) fn install_service(cfg_path: &Path, cfg: &AppConfig) -> Result<(), String> {
    let _ = cfg.save(cfg_path);
    let args = format!("--install-service --config \"{}\"", cfg_path.display());
    launch_elevated_service_cmd(&args).map_err(|e| e.to_string())
}

pub(crate) fn sync_service_config(cfg_path: &Path, cfg: &AppConfig) -> Result<(), String> {
    let _ = cfg.save(cfg_path);
    if crate::service::install::sync_config_file_to_program_data(cfg_path).is_ok() {
        return Ok(());
    }
    let args = format!("--sync-service-config --config \"{}\"", cfg_path.display());
    launch_elevated_service_cmd(&args).map_err(|e| e.to_string())
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
    sys: &SysStatus,
) -> HomeAction {
    let mut action = HomeAction::None;
    let mut conf = cfg.lock().unwrap();
    let st = relay_status.lock().unwrap().clone();

    if conf.bundle.is_some() {
        this_device_card(ui, state, cfg_path, &mut conf, caps, monitors, sys, &mut action);
        theme::gap(ui);
        devices_card(ui, state, &conf, &st, relay_cmd_tx, &mut action);
        theme::gap(ui);
        relay_card(ui, &mut conf, &st, &mut action);
    } else {
        onboarding_card(ui, state, &mut conf, &mut action);
        theme::gap(ui);
        this_device_card(ui, state, cfg_path, &mut conf, caps, monitors, sys, &mut action);
    }

    theme::toast_area(ui.ctx(), &mut state.toast);
    action
}

fn toast(ui: &egui::Ui, state: &mut HomeState, tone: Tone, text: impl Into<String>) {
    state.toast = Some(Toast::new(ui.ctx(), tone, text));
}

/// First-run card: create a personal root key or import an identity issued elsewhere.
fn onboarding_card(
    ui: &mut egui::Ui,
    state: &mut HomeState,
    conf: &mut AppConfig,
    action: &mut HomeAction,
) {
    theme::card(ui, |ui| {
        ui.label(theme::h1("开始使用 FlashDesk"));
        ui.add(
            egui::Label::new(theme::muted(
                "每台电脑都需要由你的个人根密钥签发的身份：第一台电脑在这里生成根密钥，\
                 其它电脑导入它签发的 .fdesk 配置包或导入码。",
            ))
            .wrap(),
        );
        ui.add_space(6.0);
        // Wraps onto a second line on narrow windows.
        ui.horizontal_wrapped(|ui| {
            if theme::button(ui, "生成个人根密钥（主设备）", Kind::Primary).clicked() {
                conf.init_new_personal_root(default_hostname(), default_relays());
                *action = HomeAction::ConfigChanged;
                toast(
                    ui,
                    state,
                    Tone::Ok,
                    "已生成个人根密钥并激活本机身份。请到「设置」填入你的中转服务器地址。",
                );
            }
            if theme::button(ui, "导入 .fdesk 配置包", Kind::Secondary).clicked() {
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
                            *action = HomeAction::ConfigChanged;
                            toast(ui, state, Tone::Ok, "已导入设备配置包，正在连接中转节点…");
                        }
                        Err(e) => toast(ui, state, Tone::Danger, format!("导入失败：{e}")),
                    }
                }
            }
            let label = if state.show_token_import { "收起导入码" } else { "粘贴导入码" };
            if theme::button(ui, label, Kind::Ghost).clicked() {
                state.show_token_import = !state.show_token_import;
            }
        });
        if state.show_token_import {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                let w = (ui.available_width() - 80.0).clamp(140.0, 520.0);
                theme::text_input(ui, &mut state.import_token_input, w, "粘贴 Base64 导入码");
                if theme::button(ui, "导入", Kind::Secondary).clicked() {
                    let token = state.import_token_input.trim().to_string();
                    match DeviceBundle::from_b64_token(&token) {
                        _ if token.is_empty() => toast(ui, state, Tone::Warn, "请先粘贴导入码"),
                        Ok(bundle) => {
                            conf.import_bundle(bundle);
                            state.import_token_input.clear();
                            state.show_token_import = false;
                            *action = HomeAction::ConfigChanged;
                            toast(ui, state, Tone::Ok, "导入码已生效");
                        }
                        Err(e) => toast(ui, state, Tone::Danger, format!("导入码无效：{e}")),
                    }
                }
            });
        }
    });
}

/// "This machine": identity, incoming-control switch, run mode, virtual display, encoders and
/// monitors.
#[allow(clippy::too_many_arguments)]
fn this_device_card(
    ui: &mut egui::Ui,
    state: &mut HomeState,
    cfg_path: &Path,
    conf: &mut AppConfig,
    caps: &CodecCapabilities,
    monitors: &[MonitorTarget],
    sys: &SysStatus,
    action: &mut HomeAction,
) {
    theme::card(ui, |ui| {
        if let Some(bundle) = &conf.bundle {
            let id_str = bundle.identity.cert.device_id.to_string();
            let dev_name = bundle.identity.cert.device_name.clone();
            let lan = format!("{}:{}", sys.lan_ip, conf.direct_listen_port);
            ui.horizontal(|ui| {
                theme::avatar(ui, "💻", false);
                ui.add_space(4.0);
                // The switch goes first (right-aligned) so the identity text gets whatever
                // width is left, instead of running underneath it on narrow windows.
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if theme::toggle(ui, &mut conf.allow_incoming).changed() {
                        *action = HomeAction::ConfigChanged;
                    }
                    let hint = if conf.allow_incoming { "允许被远程控制" } else { "已禁止被控" };
                    ui.label(RichText::new(hint).color(c::TEXT_2));
                    ui.add_space(8.0);
                    ui.with_layout(Layout::top_down(Align::Min), |ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.horizontal(|ui| {
                            let name_w = ui.available_width() - 56.0;
                            theme::label_max_width(ui, theme::bold(&dev_name).size(19.0), name_w);
                            theme::chip(ui, "本机", c::ACCENT_HOVER, c::ACCENT_SOFT);
                        });
                        // ID and LAN address share a line only when both fit.
                        if ui.available_width() >= 470.0 {
                            ui.horizontal(|ui| {
                                id_line(ui, state, &id_str);
                                ui.add_space(8.0);
                                lan_line(ui, state, &lan);
                            });
                        } else {
                            ui.horizontal(|ui| id_line(ui, state, &id_str));
                            ui.horizontal(|ui| lan_line(ui, state, &lan));
                        }
                    });
                });
            });
        } else {
            theme::card_header(ui, "本机", Some("激活身份后即可被你的其它电脑远程控制"), |_| {});
        }

        theme::divider(ui);
        ui.spacing_mut().item_spacing.y = 4.0;

        let (dot, mode_text) = service_summary(sys);
        theme::form_row_w(ui, "运行模式", 96.0, |ui| {
            // Button first (right-aligned) so a long status wraps instead of sliding under it.
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if sys.service != ServiceState::Running {
                    if theme::small_button(ui, "安装系统服务", Kind::Secondary)
                        .on_hover_text("需要管理员权限 (UAC)。安装后可远控锁屏、登录界面和 UAC 弹窗")
                        .clicked()
                    {
                        match install_service(cfg_path, conf) {
                            Ok(()) => toast(ui, state, Tone::Info, "已请求安装并启动 FlashDeskService（请在 UAC 弹窗中确认）"),
                            Err(e) => toast(ui, state, Tone::Danger, format!("服务安装失败：{e}")),
                        }
                    }
                } else if !sys.agent_ok {
                    if theme::small_button(ui, "重启服务", Kind::Secondary).clicked() {
                        match launch_elevated_service_cmd("--restart-service") {
                            Ok(()) => toast(
                                ui,
                                state,
                                Tone::Info,
                                "已请求重启 FlashDeskService；若仍无法启动，请查看 C:\\ProgramData\\FlashDesk\\flashdesk-service.log",
                            ),
                            Err(e) => toast(ui, state, Tone::Danger, format!("重启失败：{e}")),
                        }
                    }
                } else if !sys.service_config_synced
                    && theme::small_button(ui, "同步到服务", Kind::Primary)
                        .on_hover_text("桌面端已自动接管普通被控；同步后系统服务将支持锁屏与 UAC（3 秒内生效）")
                        .clicked()
                {
                    match sync_service_config(cfg_path, conf) {
                        Ok(()) => toast(ui, state, Tone::Ok, "已同步最新配置到系统服务（3 秒内生效）"),
                        Err(e) => toast(ui, state, Tone::Danger, format!("同步失败：{e}")),
                    }
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    theme::status_dot(ui, dot);
                    ui.add(egui::Label::new(RichText::new(mode_text).color(c::TEXT)).wrap());
                });
            });
        });
        theme::form_row_w(ui, "虚拟显示器", 96.0, |ui| {
            ui.add(egui::Label::new(RichText::new(&sys.vdd_details).color(c::TEXT)).wrap());
        });
        theme::form_row_w(ui, "编码 / 显示器", 96.0, |ui| {
            // Several encoders / monitors wrap onto more lines instead of widening the card.
            ui.horizontal_wrapped(|ui| {
                if caps.encode_backends.is_empty() {
                    ui.label(theme::faint("未检测到可用编码器"));
                }
                for b in &caps.encode_backends {
                    theme::chip(ui, b, c::ACCENT_HOVER, c::ACCENT_SOFT);
                }
                if monitors.is_empty() {
                    ui.label(theme::faint("未检测到显示器"));
                }
                for m in monitors {
                    theme::tag(
                        ui,
                        format!(
                            "#{}  {}×{} · {} dpi",
                            m.info.index + 1,
                            m.info.width,
                            m.info.height,
                            m.info.dpi
                        ),
                    );
                }
            });
        });
    });
}

fn id_line(ui: &mut egui::Ui, state: &mut HomeState, id_str: &str) {
    ui.spacing_mut().item_spacing.x = 6.0;
    ui.label(theme::faint("设备 ID"));
    ui.label(theme::mono(format!("{}…", &id_str[..8])))
        .on_hover_text(id_str);
    if theme::small_button(ui, "复制", Kind::Ghost).clicked() {
        ui.ctx().copy_text(id_str.to_string());
        toast(ui, state, Tone::Ok, "已复制本机设备 ID");
    }
}

fn lan_line(ui: &mut egui::Ui, state: &mut HomeState, lan: &str) {
    ui.spacing_mut().item_spacing.x = 6.0;
    ui.label(theme::faint("局域网直连"));
    ui.label(theme::mono(lan));
    if theme::small_button(ui, "复制", Kind::Ghost).clicked() {
        ui.ctx().copy_text(lan.to_string());
        toast(ui, state, Tone::Ok, "已复制局域网直连地址");
    }
}

/// Online devices signed by the same root, plus LAN / IP direct connect.
fn devices_card(
    ui: &mut egui::Ui,
    state: &mut HomeState,
    conf: &AppConfig,
    st: &RelayStatus,
    relay_cmd_tx: &mpsc::UnboundedSender<RelayCommand>,
    action: &mut HomeAction,
) {
    let self_id = conf.bundle.as_ref().map(|b| b.identity.cert.device_id);
    let others: Vec<&OnlineDevice> = st
        .devices
        .iter()
        .filter(|d| Some(d.device_id) != self_id)
        .collect();

    theme::card(ui, |ui| {
        theme::card_header(
            ui,
            "我的在线设备",
            Some("同一根密钥签发的电脑会自动出现在这里，无需密码"),
            |ui| {
                if theme::small_button(ui, "刷新", Kind::Ghost).clicked() {
                    let _ = relay_cmd_tx.send(RelayCommand::RefreshDevices);
                }
            },
        );

        if !st.connected {
            theme::empty_state(
                ui,
                "🔌",
                "尚未连接中转服务器",
                "连接成功后，这里会列出你的在线设备；也可以用下方的局域网直连。",
            );
        } else if others.is_empty() {
            theme::empty_state(
                ui,
                "🖥",
                "暂无其它在线设备",
                "在另一台电脑导入同一根密钥签发的 .fdesk 配置包后，它会出现在这里。",
            );
        } else {
            for dev in others {
                theme::inset(ui, |ui| {
                    ui.horizontal(|ui| {
                        theme::avatar(ui, "🖥", true);
                        ui.add_space(4.0);
                        // Button first so long names / details are truncated, not overlapped.
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if theme::button(ui, "连接", Kind::Primary).clicked() {
                                *action = HomeAction::ConnectRelay(dev.clone());
                            }
                            ui.add_space(8.0);
                            ui.with_layout(Layout::top_down(Align::Min), |ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                ui.add(
                                    egui::Label::new(theme::bold(&dev.device_name).size(15.0))
                                        .truncate(),
                                );
                                let codecs = if dev.encode_codecs.is_empty() {
                                    "编码未知".to_string()
                                } else {
                                    dev.encode_codecs
                                        .iter()
                                        .map(|c| c.label())
                                        .collect::<Vec<_>>()
                                        .join(" / ")
                                };
                                ui.add(
                                    egui::Label::new(theme::faint(format!(
                                        "{} · {} 个显示器 · {}",
                                        dev.os, dev.monitor_count, codecs
                                    )))
                                    .truncate(),
                                );
                            });
                        });
                    });
                });
            }
        }

        theme::divider(ui);
        ui.horizontal(|ui| {
            ui.label(RichText::new("局域网 / IP 直连").color(c::TEXT_2));
            ui.add_space(4.0);
            // Narrower input on small windows; the note after the button is truncated last.
            let input_w = (ui.available_width() - 72.0).clamp(120.0, 220.0);
            let resp = theme::text_input(
                ui,
                &mut state.direct_addr_input,
                input_w,
                "192.168.1.100:19444",
            );
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            if theme::button(ui, "直连", Kind::Secondary).clicked() || enter {
                let addr = state.direct_addr_input.trim().to_string();
                if addr.is_empty() {
                    toast(ui, state, Tone::Warn, "请先输入对方的 IP:端口");
                } else {
                    *action = HomeAction::ConnectDirect(addr);
                }
            }
            theme::hint(ui, "不经过中转，同样端到端加密");
        });
    });
}

/// Relay line picker: which self-hosted relay carries the control session.
fn relay_card(
    ui: &mut egui::Ui,
    conf: &mut AppConfig,
    st: &RelayStatus,
    action: &mut HomeAction,
) {
    theme::card(ui, |ui| {
        theme::card_header(
            ui,
            "中转线路",
            Some("控屏走所选线路；大文件传输可在设置中指定其它节点"),
            |ui| {
                if theme::small_button(ui, "管理节点", Kind::Ghost).clicked() {
                    *action = HomeAction::GoToSettings;
                }
            },
        );

        // Connected: the selected card says so. Otherwise surface the error / progress here.
        if !st.connected {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                theme::status_dot(ui, if st.error.is_some() { c::DANGER } else { c::WARN });
                let text = st
                    .error
                    .clone()
                    .unwrap_or_else(|| "正在连接中转服务器…".into());
                ui.add(egui::Label::new(RichText::new(text).color(c::TEXT_2)).wrap());
            });
            ui.add_space(2.0);
        }

        let nodes = conf
            .bundle
            .as_ref()
            .map(|b| b.relay_nodes.clone())
            .unwrap_or_default();
        if nodes.is_empty() {
            ui.label(theme::muted("还没有中转节点，请到「设置 → 中转节点」添加你的服务器。"));
            return;
        }
        let active_id = conf.selected_relay_id.clone().unwrap_or_default();
        let gap = 10.0;
        let avail = ui.available_width();
        let cols = (((avail + gap) / (260.0 + gap)).floor() as usize)
            .clamp(1, 3)
            .min(nodes.len());
        let width = (avail - gap * (cols as f32 - 1.0)) / cols as f32;
        for row in nodes.chunks(cols) {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for node in row {
                    let selected = node.id == active_id;
                    let sub = if selected && st.connected {
                        let rtt = st.rtt_ms.map(|ms| format!(" · {ms} ms")).unwrap_or_default();
                        format!("已连接 · {}{rtt}", node.addr)
                    } else {
                        format!("{} · {}", node.role.label(), node.addr)
                    };
                    if theme::option_card(ui, selected, &node.name, &sub, width).clicked()
                        && !selected
                    {
                        conf.selected_relay_id = Some(node.id.clone());
                        *action = HomeAction::ConfigChanged;
                    }
                }
            });
        }
    });
}
