use anyhow::{Context, Result};
use protocol::auth::{DeviceBundle, RelayNodeConfig, RelayNodeRole, RootKeypair};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub bundle: Option<DeviceBundle>,
    pub local_root_secret_b64: Option<String>,
    #[serde(default)]
    pub revoked_device_ids: Vec<Uuid>,
    pub selected_relay_id: Option<String>,
    #[serde(default)]
    pub preferred_file_relay_id: Option<String>,
    #[serde(default = "default_true")]
    pub allow_incoming: bool,
    #[serde(default = "default_true")]
    pub auto_vdd_on_headless: bool,
    #[serde(default = "default_direct_port")]
    pub direct_listen_port: u16,
    #[serde(default = "default_fps")]
    pub default_fps: u32,
    /// Bitrate *ceiling* for the adaptive bitrate controller.
    #[serde(default = "default_bitrate_kbps")]
    pub default_bitrate_kbps: u32,
    /// Default stream resolution cap by height: 0 = native, 1080, 720.
    #[serde(default)]
    pub stream_max_height: u32,
}

fn default_true() -> bool {
    true
}
fn default_direct_port() -> u16 {
    19444
}
fn default_fps() -> u32 {
    60
}
fn default_bitrate_kbps() -> u32 {
    // Ceiling only: the host's ABR settles below the link capacity on its own (a 5 Mbps relay
    // ends up around 4 Mbps). Before ABR this had to be 2500 to avoid 500 ms queueing (Pitfall #12).
    4000
}

/// Maps a "max height" preset to the orientation-agnostic `(max_long, max_short)` cap.
pub fn res_cap_for_height(max_height: u32) -> (u32, u32) {
    match max_height {
        0 => (0, 0),
        h => ((h * 16).div_ceil(9), h),
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            bundle: None,
            local_root_secret_b64: None,
            revoked_device_ids: Vec::new(),
            selected_relay_id: None,
            preferred_file_relay_id: None,
            allow_incoming: true,
            auto_vdd_on_headless: true,
            direct_listen_port: default_direct_port(),
            default_fps: default_fps(),
            default_bitrate_kbps: default_bitrate_kbps(),
            stream_max_height: 0,
        }
    }
}

impl AppConfig {
    /// Returns the preferred config path (portable next to exe if writable, else AppData).
    pub fn resolve_path() -> PathBuf {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let portable = dir.join("flashdesk.config.json");
                if portable.exists() {
                    return portable;
                }
                let probe = dir.join(".flashdesk_write_probe");
                if std::fs::write(&probe, b"").is_ok() {
                    let _ = std::fs::remove_file(&probe);
                    return portable;
                }
            }
        }
        let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
        base.join("FlashDesk").join("config.json")
    }

    pub fn load_or_default(path: &Path) -> Self {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(cfg) = serde_json::from_str::<Self>(&text) {
                return cfg;
            }
        }
        Self::default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)
            .with_context(|| format!("failed to write config {}", path.display()))?;
        Ok(())
    }

    /// Initializes a brand-new Personal Root CA and signs an identity bundle for this PC.
    pub fn init_new_personal_root(
        &mut self,
        device_name: impl Into<String>,
        initial_relays: Vec<RelayNodeConfig>,
    ) {
        let root = RootKeypair::generate();
        let identity = root.sign_device_cert(Uuid::new_v4(), device_name, 3650 * 86400);
        let first_id = initial_relays.first().map(|r| r.id.clone());
        self.local_root_secret_b64 = Some(root.to_b64());
        self.bundle = Some(DeviceBundle {
            root_pubkey: root.pubkey_bytes(),
            identity,
            relay_nodes: initial_relays,
        });
        self.selected_relay_id = first_id;
    }

    /// Issues a signed `.fdesk` DeviceBundle for another PC using this PC's Root CA secret.
    pub fn issue_device_bundle(
        &self,
        new_device_name: impl Into<String>,
        validity_days: u64,
    ) -> Result<DeviceBundle> {
        let secret_b64 = self
            .local_root_secret_b64
            .as_deref()
            .context("当前设备没有个人根私钥，请在初始化根密钥的电脑上签发")?;
        let root = RootKeypair::from_b64(secret_b64)?;
        let relays = self
            .bundle
            .as_ref()
            .map(|b| b.relay_nodes.clone())
            .unwrap_or_default();
        let identity = root.sign_device_cert(
            Uuid::new_v4(),
            new_device_name,
            validity_days.saturating_mul(86400),
        );
        Ok(DeviceBundle {
            root_pubkey: root.pubkey_bytes(),
            identity,
            relay_nodes: relays,
        })
    }

    pub fn import_bundle(&mut self, bundle: DeviceBundle) {
        if self.selected_relay_id.is_none() {
            self.selected_relay_id = bundle.relay_nodes.first().map(|r| r.id.clone());
        }
        self.bundle = Some(bundle);
    }

    pub fn active_relay(&self) -> Option<&RelayNodeConfig> {
        let bundle = self.bundle.as_ref()?;
        if let Some(id) = &self.selected_relay_id {
            if let Some(node) = bundle.relay_nodes.iter().find(|n| &n.id == id) {
                return Some(node);
            }
        }
        bundle
            .relay_nodes
            .iter()
            .find(|n| n.role == RelayNodeRole::PrimaryControl)
            .or_else(|| bundle.relay_nodes.first())
    }

    pub fn preferred_file_relay(&self) -> Option<&RelayNodeConfig> {
        let bundle = self.bundle.as_ref()?;
        if let Some(id) = &self.preferred_file_relay_id {
            if let Some(node) = bundle.relay_nodes.iter().find(|n| &n.id == id) {
                return Some(node);
            }
        }
        bundle
            .relay_nodes
            .iter()
            .find(|n| n.role == RelayNodeRole::FilePreferred)
            .or_else(|| self.active_relay())
    }

    /// Checks whether a running SYSTEM service loaded with `svc` shares this GUI config's
    /// incoming switch, device identity / root CA, and active relay address.
    pub fn service_config_matches(&self, svc: &AppConfig) -> bool {
        if self.allow_incoming != svc.allow_incoming {
            return false;
        }
        let (Some(gb), Some(sb)) = (&self.bundle, &svc.bundle) else {
            return false;
        };
        if gb.root_pubkey != sb.root_pubkey
            || gb.identity.cert.device_id != sb.identity.cert.device_id
        {
            return false;
        }
        match (self.active_relay(), svc.active_relay()) {
            (Some(gr), Some(sr)) => gr.addr.trim() == sr.addr.trim(),
            _ => false,
        }
    }

    /// Reads `C:\ProgramData\FlashDesk\config.json` and checks whether it matches this config.
    pub fn service_config_matches_disk(&self) -> bool {
        let Ok(text) = std::fs::read_to_string(crate::service::install::SHARED_CONFIG_PATH) else {
            return false;
        };
        let Ok(svc) = serde_json::from_str::<AppConfig>(&text) else {
            return false;
        };
        self.service_config_matches(&svc)
    }
}

pub fn default_hostname() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "Windows-PC".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.init_new_personal_root(
            "TestPC",
            vec![RelayNodeConfig {
                id: "cn-5m".into(),
                name: "国内 5M".into(),
                addr: "203.0.113.10:9442".into(),
                role: RelayNodeRole::PrimaryControl,
            }],
        );
        cfg
    }

    #[test]
    fn service_config_matches_checks_identity_relay_and_incoming() {
        let base = sample_config();
        assert!(base.service_config_matches(&base));

        // Different relay port -> out of sync
        let mut diff_port = base.clone();
        diff_port.bundle.as_mut().unwrap().relay_nodes[0].addr = "203.0.113.10:19443".into();
        assert!(!base.service_config_matches(&diff_port));

        // Different device_id -> out of sync
        let mut diff_dev = base.clone();
        diff_dev.bundle.as_mut().unwrap().identity.cert.device_id = Uuid::new_v4();
        assert!(!base.service_config_matches(&diff_dev));

        // Different root_pubkey -> out of sync
        let mut diff_root = base.clone();
        diff_root.bundle.as_mut().unwrap().root_pubkey[0] ^= 0xFF;
        assert!(!base.service_config_matches(&diff_root));

        // Different allow_incoming -> out of sync
        let mut diff_incoming = base.clone();
        diff_incoming.allow_incoming = false;
        assert!(!base.service_config_matches(&diff_incoming));

        // Missing bundle -> out of sync
        let no_bundle = AppConfig::default();
        assert!(!base.service_config_matches(&no_bundle));
        assert!(!no_bundle.service_config_matches(&base));
    }
}
