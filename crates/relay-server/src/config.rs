use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayConfig {
    pub listen_addr: String,
    pub root_pubkey_b64: String,
    #[serde(default)]
    pub revoked_device_ids: Vec<Uuid>,
    #[serde(default = "default_pending_timeout")]
    pub pending_timeout_secs: u64,
}

fn default_pending_timeout() -> u64 {
    15
}

impl RelayConfig {
    pub fn load_from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read relay config: {}", path.display()))?;
        let cfg: Self = toml::from_str(&text)
            .with_context(|| format!("failed to parse TOML relay config: {}", path.display()))?;
        Ok(cfg)
    }
}
