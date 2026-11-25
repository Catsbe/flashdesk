use anyhow::{Context, Result};
use protocol::auth::{DeviceBundle, RelayNodeConfig, RelayNodeRole, RevocationCommand, RootKeypair};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IssuedDeviceRecord {
    pub device_id: Uuid,
    pub label: String,
    pub issued_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub bundle: Option<DeviceBundle>,
    /// Legacy inline root secret; newly initialized/saved configs keep it in `root.key`.
    #[serde(default, skip_serializing)]
    pub local_root_secret_b64: Option<String>,
    #[serde(default)]
    pub local_device_name: Option<String>,
    #[serde(default)]
    pub issued_devices: Vec<IssuedDeviceRecord>,
    #[serde(default)]
    pub revoked_device_ids: Vec<Uuid>,
    #[serde(default)]
    pub last_revocation_cmd: Option<RevocationCommand>,
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
            local_device_name: None,
            issued_devices: Vec::new(),
            revoked_device_ids: Vec::new(),
            last_revocation_cmd: None,
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
    /// Fixed per-user configuration directory (`%USERPROFILE%\.flashdesk`).
    pub fn config_dir() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".flashdesk")
    }

    /// Returns the fixed per-user config path (`%USERPROFILE%\.flashdesk\config.json`).
    pub fn resolve_path() -> PathBuf {
        Self::config_dir().join("config.json")
    }

    pub fn root_key_path_for(cfg_path: &Path) -> PathBuf {
        cfg_path.with_file_name("root.key")
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

    pub fn save_root_keypair(cfg_path: &Path, root: &RootKeypair) -> Result<()> {
        let key_path = Self::root_key_path_for(cfg_path);
        if let Some(parent) = key_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(&key_path, format!("{}\n", root.to_b64()))
            .with_context(|| format!("failed to write root key {}", key_path.display()))?;
        Ok(())
    }

    pub fn load_root_keypair(&self, cfg_path: &Path) -> Option<RootKeypair> {
        let key_path = Self::root_key_path_for(cfg_path);
        let kp = std::fs::read_to_string(&key_path)
            .ok()
            .and_then(|s| RootKeypair::from_b64(&s).ok())
            .or_else(|| {
                self.local_root_secret_b64
                    .as_deref()
                    .and_then(|s| RootKeypair::from_b64(s).ok())
            })?;
        if let Some(bundle) = &self.bundle {
            if kp.pubkey_bytes() != bundle.root_pubkey {
                return None;
            }
        }
        Some(kp)
    }

    pub fn has_root_key(&self, cfg_path: &Path) -> bool {
        self.load_root_keypair(cfg_path).is_some()
    }

    pub fn display_device_name(&self) -> String {
        self.local_device_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                self.bundle
                    .as_ref()
                    .map(|b| b.identity.cert.device_name.clone())
            })
            .unwrap_or_else(default_hostname)
    }

    /// Initializes a brand-new Personal Root CA (`root.key`) and signs an identity bundle for this PC.
    pub fn init_new_personal_root(
        &mut self,
        cfg_path: &Path,
        device_name: impl Into<String>,
        initial_relays: Vec<RelayNodeConfig>,
    ) -> Result<()> {
        let root = RootKeypair::generate();
        Self::save_root_keypair(cfg_path, &root)?;
        let dev_name = device_name.into();
        let identity = root.sign_device_cert(Uuid::new_v4(), &dev_name, 3650 * 86400);
        let first_id = initial_relays.first().map(|r| r.id.clone());
        self.local_root_secret_b64 = None;
        self.local_device_name = Some(dev_name);
        self.issued_devices.clear();
        self.revoked_device_ids.clear();
        self.last_revocation_cmd = None;
        self.bundle = Some(DeviceBundle {
            root_pubkey: root.pubkey_bytes(),
            identity,
            relay_nodes: initial_relays,
        });
        self.selected_relay_id = first_id;
        self.preferred_file_relay_id = None;
        Ok(())
    }

    /// Issues a signed DeviceBundle for another PC/mobile using `root.key` and records it in
    /// `issued_devices`. Blank labels receive an automatic short name; the peer also reports its
    /// actual hostname upon connecting.
    pub fn issue_device_bundle(
        &mut self,
        cfg_path: &Path,
        label: &str,
        validity_days: u64,
    ) -> Result<(DeviceBundle, IssuedDeviceRecord)> {
        let root = self
            .load_root_keypair(cfg_path)
            .context("当前设备没有个人根私钥，请在初始化根密钥的主电脑上签发")?;
        let relays = self
            .bundle
            .as_ref()
            .map(|b| b.relay_nodes.clone())
            .unwrap_or_default();
        let device_id = Uuid::new_v4();
        let trimmed = label.trim();
        let effective_label = if trimmed.is_empty() {
            format!("设备-{}", &device_id.simple().to_string()[..4])
        } else {
            trimmed.to_string()
        };
        let identity = root.sign_device_cert(
            device_id,
            &effective_label,
            validity_days.saturating_mul(86400),
        );
        let record = IssuedDeviceRecord {
            device_id,
            label: effective_label,
            issued_at: identity.cert.issued_at,
        };
        self.issued_devices.push(record.clone());
        let bundle = DeviceBundle {
            root_pubkey: root.pubkey_bytes(),
            identity,
            relay_nodes: relays,
        };
        Ok((bundle, record))
    }

    pub fn import_bundle(&mut self, bundle: DeviceBundle) {
        self.local_root_secret_b64 = None;
        let has_selected = self
            .selected_relay_id
            .as_ref()
            .is_some_and(|id| bundle.relay_nodes.iter().any(|r| &r.id == id));
        if !has_selected {
            self.selected_relay_id = bundle.relay_nodes.first().map(|r| r.id.clone());
        }
        if let Some(pref_id) = &self.preferred_file_relay_id {
            if !bundle.relay_nodes.iter().any(|r| &r.id == pref_id) {
                self.preferred_file_relay_id = None;
            }
        }
        self.local_device_name = Some(default_hostname());
        self.bundle = Some(bundle);
    }

    pub fn sign_current_revocations(&mut self, cfg_path: &Path) -> Result<RevocationCommand> {
        let root = self
            .load_root_keypair(cfg_path)
            .context("当前设备没有个人根私钥，无法签名远程吊销列表")?;
        let cmd = root.sign_revocation_list(self.revoked_device_ids.clone());
        self.last_revocation_cmd = Some(cmd.clone());
        Ok(cmd)
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

    fn temp_cfg_path(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("flashdesk-cfg-test-{tag}-{}", Uuid::new_v4()));
        (dir.join("config.json"), dir)
    }

    fn sample_config(cfg_path: &Path) -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.init_new_personal_root(
            cfg_path,
            "TestPC",
            vec![RelayNodeConfig {
                id: "cn-5m".into(),
                name: "国内 5M".into(),
                addr: "203.0.113.10:9442".into(),
                role: RelayNodeRole::PrimaryControl,
            }],
        )
        .unwrap();
        cfg
    }

    #[test]
    fn service_config_matches_checks_identity_relay_and_incoming() {
        let (cfg_path, dir) = temp_cfg_path("svc-match");
        let base = sample_config(&cfg_path);
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

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn root_key_file_separation_mismatch_guard_and_ledger() {
        let (cfg_path, dir) = temp_cfg_path("root-sep");
        let mut cfg = sample_config(&cfg_path);
        assert!(AppConfig::root_key_path_for(&cfg_path).exists());
        assert!(cfg.has_root_key(&cfg_path));

        // Saving config.json does NOT embed local_root_secret_b64 in config.json.
        cfg.save(&cfg_path).unwrap();
        let saved_json = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(!saved_json.contains("local_root_secret_b64"));

        // Blank label gets an automatic short label and records into issued_devices.
        let (bundle, rec) = cfg.issue_device_bundle(&cfg_path, "   ", 3650).unwrap();
        assert!(rec.label.starts_with("设备-"));
        assert_eq!(cfg.issued_devices.len(), 1);
        assert_eq!(cfg.issued_devices[0].device_id, bundle.identity.cert.device_id);

        // Revocation signing works via root.key.
        cfg.revoked_device_ids.push(rec.device_id);
        let rev_cmd = cfg.sign_current_revocations(&cfg_path).unwrap();
        assert!(rev_cmd
            .verify(
                &cfg.bundle.as_ref().unwrap().root_pubkey,
                protocol::unix_timestamp_secs()
            )
            .is_ok());

        // Importing a bundle from another root resets stale relay selections & rejects mismatched root.key.
        let other_root = RootKeypair::generate();
        let other_ident = other_root.sign_device_cert(Uuid::new_v4(), "Other", 3600);
        let foreign_bundle = DeviceBundle {
            root_pubkey: other_root.pubkey_bytes(),
            identity: other_ident,
            relay_nodes: vec![RelayNodeConfig {
                id: "new-node".into(),
                name: "New".into(),
                addr: "1.2.3.4:9442".into(),
                role: RelayNodeRole::PrimaryControl,
            }],
        };
        let mut child_cfg = AppConfig::default();
        child_cfg.selected_relay_id = Some("old-stale-id".into());
        child_cfg.preferred_file_relay_id = Some("old-file-id".into());
        child_cfg.import_bundle(foreign_bundle);
        assert_eq!(child_cfg.selected_relay_id.as_deref(), Some("new-node"));
        assert_eq!(child_cfg.preferred_file_relay_id, None);

        // Mismatched root.key in the same folder must NOT load for foreign_bundle.
        assert!(!child_cfg.has_root_key(&cfg_path));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
