use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use protocol::auth::{decode_pubkey_b64, DeviceBundle, RelayNodeConfig, RelayNodeRole, RootKeypair};
use relay_server::config::RelayConfig;
use relay_server::server::run_server;
use relay_server::state::RelayState;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

#[derive(Parser)]
#[command(name = "flashdesk-relay", about = "FlashDesk self-hosted multi-channel relay server")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the relay server
    Serve {
        /// Path to relay.toml config file
        #[arg(long, default_value = "relay.toml")]
        config: PathBuf,
        /// Optional override listen address (skips config file if root-pubkey is also provided)
        #[arg(long)]
        listen: Option<String>,
        /// Optional override base64 root pubkey
        #[arg(long)]
        root_pubkey: Option<String>,
    },
    /// Generate a new personal Ed25519 Root CA private key
    InitRoot {
        #[arg(long, default_value = "root.key")]
        out: PathBuf,
    },
    /// Issue a signed .fdesk device bundle using a Root CA private key
    IssueBundle {
        #[arg(long, default_value = "root.key")]
        root: PathBuf,
        #[arg(long)]
        name: String,
        /// Relay entries formatted as id|name|addr|role (role: control|file|backup)
        #[arg(long = "relay")]
        relays: Vec<String>,
        #[arg(long, default_value_t = 3650)]
        validity_days: u64,
        #[arg(long)]
        out: PathBuf,
    },
}

fn parse_relay_spec(spec: &str) -> Result<RelayNodeConfig> {
    let parts: Vec<&str> = spec.split('|').collect();
    if parts.len() != 4 {
        anyhow::bail!("relay spec must be id|name|addr|role, got: {spec}");
    }
    let role = match parts[3].trim().to_ascii_lowercase().as_str() {
        "control" | "primary" | "primarycontrol" => RelayNodeRole::PrimaryControl,
        "file" | "filepreferred" => RelayNodeRole::FilePreferred,
        "backup" => RelayNodeRole::Backup,
        other => anyhow::bail!("unknown relay role '{other}', expected control|file|backup"),
    };
    Ok(RelayNodeConfig {
        id: parts[0].trim().to_string(),
        name: parts[1].trim().to_string(),
        addr: parts[2].trim().to_string(),
        role,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Commands::Serve {
            config,
            listen,
            root_pubkey,
        } => {
            let (listen_addr, root_pub, revoked, timeout_secs) =
                if let (Some(addr), Some(pk_b64)) = (listen.clone(), root_pubkey.clone()) {
                    (addr, decode_pubkey_b64(&pk_b64)?, Vec::new(), 15)
                } else {
                    let cfg = RelayConfig::load_from_file(&config)?;
                    let addr = listen.unwrap_or(cfg.listen_addr);
                    let pk_b64 = root_pubkey.unwrap_or(cfg.root_pubkey_b64);
                    let pk = decode_pubkey_b64(&pk_b64)?;
                    (addr, pk, cfg.revoked_device_ids, cfg.pending_timeout_secs)
                };

            let listener = TcpListener::bind(&listen_addr)
                .await
                .with_context(|| format!("failed to bind {listen_addr}"))?;
            let state = Arc::new(RelayState::new(
                root_pub,
                revoked,
                Duration::from_secs(timeout_secs),
            ));
            run_server(listener, state).await?;
        }
        Commands::InitRoot { out } => {
            let kp = RootKeypair::generate();
            std::fs::write(&out, kp.to_b64())
                .with_context(|| format!("failed to write root key to {}", out.display()))?;
            println!("Root private key written to: {}", out.display());
            println!("root_pubkey_b64 = \"{}\"", kp.pubkey_b64());
        }
        Commands::IssueBundle {
            root,
            name,
            relays,
            validity_days,
            out,
        } => {
            let secret_b64 = std::fs::read_to_string(&root)
                .with_context(|| format!("failed to read root key {}", root.display()))?;
            let kp = RootKeypair::from_b64(&secret_b64)?;
            let relay_nodes = relays
                .iter()
                .map(|s| parse_relay_spec(s))
                .collect::<Result<Vec<_>>>()?;
            let identity = kp.sign_device_cert(
                Uuid::new_v4(),
                name,
                validity_days.saturating_mul(86400),
            );
            let bundle = DeviceBundle {
                root_pubkey: kp.pubkey_bytes(),
                identity,
                relay_nodes,
            };
            std::fs::write(&out, bundle.to_json_pretty()?)
                .with_context(|| format!("failed to write bundle to {}", out.display()))?;
            println!("Issued device bundle: {}", out.display());
            println!("Device ID: {}", bundle.identity.cert.device_id);
        }
    }

    Ok(())
}
