//! Machine status shown on the home / settings pages: SYSTEM service state, desktop agent
//! health, virtual display driver, LAN address, and install/update state.
//!
//! These queries hit the service control manager, SetupAPI, the network stack and the
//! filesystem. They used to run on *every* UI repaint (dozens of times per second while the
//! mouse moved); now a background thread refreshes a snapshot every 2 s and the UI only reads it.

use crate::config::AppConfig;
use crate::net::local_lan_ip;
use crate::service::health::agent_healthy;
use crate::service::install::{query_service_state, ServiceState};
use crate::vdd::detect_vdd_status;
use base_common::{
    read_json_opt, InstallLayout, InstallState, UpdateStatusSnapshot, STATE_FILE_NAME,
    UPDATE_STATUS_PATH,
};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, PartialEq)]
pub struct SysStatus {
    pub service: ServiceState,
    pub agent_ok: bool,
    pub service_config_synced: bool,
    pub vdd_details: String,
    pub lan_ip: String,
    pub install_layout: InstallLayout,
    pub install_state: Option<InstallState>,
    pub update_status: Option<UpdateStatusSnapshot>,
}

pub type SharedSysStatus = Arc<Mutex<SysStatus>>;

fn probe(cfg: &Arc<Mutex<AppConfig>>) -> SysStatus {
    let service = query_service_state();
    let cfg_snap = cfg.lock().unwrap().clone();
    let service_config_synced =
        service == ServiceState::Running && cfg_snap.service_config_matches_disk();
    let install_layout = crate::update::current_install_layout();
    let install_state = install_layout
        .install_dir()
        .and_then(|d| read_json_opt::<InstallState>(&d.join(STATE_FILE_NAME)));
    let update_status = read_json_opt::<UpdateStatusSnapshot>(Path::new(UPDATE_STATUS_PATH));
    SysStatus {
        service,
        agent_ok: agent_healthy(),
        service_config_synced,
        vdd_details: detect_vdd_status().details,
        lan_ip: local_lan_ip(),
        install_layout,
        install_state,
        update_status,
    }
}

/// Takes one snapshot now, then refreshes it every 2 s on a background thread and asks for a
/// repaint whenever something changed.
pub fn spawn_status_poller(
    cfg: Arc<Mutex<AppConfig>>,
    repaint: Arc<dyn Fn() + Send + Sync>,
) -> SharedSysStatus {
    let shared = Arc::new(Mutex::new(probe(&cfg)));
    let poll = Arc::clone(&shared);
    let _ = std::thread::Builder::new()
        .name("flashdesk-ui-status".into())
        .spawn(move || loop {
            std::thread::sleep(Duration::from_secs(2));
            let next = probe(&cfg);
            let changed = {
                let mut cur = poll.lock().unwrap();
                let changed = *cur != next;
                *cur = next;
                changed
            };
            if changed {
                repaint();
            }
        });
    shared
}
