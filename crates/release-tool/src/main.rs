use anyhow::{Context, Result};
use base_common::setup_payload::{build_self_extracting_setup, SetupInputBinary};
use base_common::{unix_now, BASE_VERSION};
use clap::{Parser, Subcommand};
use protocol::auth::{DeviceBundle, DeviceIdentity, RootKeypair};
use protocol::framing::{read_frame, write_frame};
use protocol::patch::{compress_full, create_delta_patch};
use protocol::relay::{
    ConnectionPurpose, RelayAuthRequest, RelayAuthResponse, RelayHello,
};
use protocol::update::{
    sha256_bytes, DeltaPatchInfo, FullPackageInfo, PublishFileSpec, SignedUpdateManifest,
    UpdateClientMsg, UpdateManifest, UpdateServerMsg,
};
use semver::Version;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::TcpStream;

const UPLOAD_CHUNK_SIZE: usize = 256 * 1024;
const MAX_HISTORY_DELTAS: usize = 5;

#[derive(Parser)]
#[command(
    name = "flashdesk-release",
    about = "FlashDesk release packager, delta generator, signer, publisher & setup builder"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Compress core exe, compute FLDP delta patches against history, sign manifest with root.key
    Pack {
        #[arg(long)]
        core: PathBuf,
        #[arg(long)]
        version: String,
        #[arg(long, default_value = BASE_VERSION)]
        min_base: String,
        #[arg(long, default_value = "")]
        notes: String,
        #[arg(long)]
        root_key: Option<PathBuf>,
        #[arg(long, default_value = "releases/history")]
        history_dir: PathBuf,
        #[arg(long)]
        out_dir: Option<PathBuf>,
    },
    /// Pack, sign, and push release directly to relay server(s) via Root-verified UpdateChannel
    Publish {
        #[arg(long)]
        core: PathBuf,
        #[arg(long)]
        version: String,
        #[arg(long, default_value = BASE_VERSION)]
        min_base: String,
        #[arg(long, default_value = "")]
        notes: String,
        #[arg(long)]
        root_key: Option<PathBuf>,
        #[arg(long, default_value = "releases/history")]
        history_dir: PathBuf,
        #[arg(long)]
        out_dir: Option<PathBuf>,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long = "relay")]
        relays: Vec<String>,
        #[arg(long)]
        pack_only: bool,
    },
    /// Bundle base binaries + core exe into single-file graphical FlashDesk-Setup.exe
    BuildSetup {
        #[arg(long)]
        stub: PathBuf,
        #[arg(long)]
        svc: PathBuf,
        #[arg(long)]
        launcher: PathBuf,
        #[arg(long)]
        uninstall: PathBuf,
        #[arg(long)]
        core: PathBuf,
        #[arg(long)]
        version: String,
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Deserialize)]
struct PartialAppConfig {
    bundle: Option<DeviceBundle>,
}

fn default_flashdesk_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".flashdesk")
}

fn resolve_root_key(opt: Option<PathBuf>) -> PathBuf {
    opt.unwrap_or_else(|| default_flashdesk_dir().join("root.key"))
}

fn resolve_config(opt: Option<PathBuf>) -> PathBuf {
    opt.unwrap_or_else(|| default_flashdesk_dir().join("config.json"))
}

pub struct PackResult {
    pub signed: SignedUpdateManifest,
    pub out_dir: PathBuf,
    pub artifact_paths: Vec<(PublishFileSpec, PathBuf)>,
    pub raw_core_bytes: usize,
    pub compressed_full_bytes: usize,
}

fn parse_history_filename(name: &str) -> Option<(Version, String)> {
    let stripped = name
        .strip_prefix("flashdesk-core-")?
        .strip_suffix(".exe")?;
    let ver = Version::parse(stripped).ok()?;
    Some((ver, stripped.to_string()))
}

pub fn pack_release(
    core_path: &Path,
    version: &str,
    min_base: &str,
    notes: &str,
    root_key_path: &Path,
    history_dir: &Path,
    out_dir: &Path,
) -> Result<PackResult> {
    let target_ver = Version::parse(version.trim())
        .with_context(|| format!("invalid target semver '{version}'"))?;
    Version::parse(min_base.trim())
        .with_context(|| format!("invalid min-base semver '{min_base}'"))?;

    let root_b64 = std::fs::read_to_string(root_key_path)
        .with_context(|| format!("failed to read root key {}", root_key_path.display()))?;
    let root = RootKeypair::from_b64(&root_b64)?;

    let new_bytes = std::fs::read(core_path)
        .with_context(|| format!("failed to read core executable {}", core_path.display()))?;
    let target_size = new_bytes.len() as u64;
    let target_sha256 = sha256_bytes(&new_bytes);

    std::fs::create_dir_all(out_dir)?;
    std::fs::create_dir_all(history_dir)?;

    let full_z = compress_full(&new_bytes)?;
    let full_name = format!("flashdesk-core-{version}.exe.deflate");
    let full_out = out_dir.join(&full_name);
    std::fs::write(&full_out, &full_z)?;
    let full_sha = sha256_bytes(&full_z);

    let mut artifact_paths = vec![(
        PublishFileSpec {
            filename: full_name.clone(),
            size: full_z.len() as u64,
            sha256: full_sha.clone(),
        },
        full_out,
    )];

    // Collect historical versions sorted newest-first.
    let mut history_bins: Vec<(Version, String, PathBuf)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(history_dir) {
        for entry in rd.flatten() {
            let fname = entry.file_name().to_string_lossy().into_owned();
            if let Some((ver, ver_str)) = parse_history_filename(&fname) {
                if ver != target_ver && entry.path().is_file() {
                    history_bins.push((ver, ver_str, entry.path()));
                }
            }
        }
    }
    history_bins.sort_by(|a, b| b.0.cmp(&a.0));
    history_bins.truncate(MAX_HISTORY_DELTAS);

    let mut deltas = Vec::new();
    let mut seen_old_shas = HashSet::new();
    for (_old_ver, old_ver_str, old_path) in history_bins {
        let old_bytes = std::fs::read(&old_path)?;
        let old_sha = sha256_bytes(&old_bytes);
        if old_sha == target_sha256 || !seen_old_shas.insert(old_sha.clone()) {
            continue;
        }
        let patch = create_delta_patch(&old_bytes, &new_bytes)?;
        // Skip delta if it is >= 85% of the compressed full binary.
        if patch.len().saturating_mul(100) >= full_z.len().saturating_mul(85) {
            println!(
                "  - skip delta from v{old_ver_str} ({} B >= 85% of full {} B)",
                patch.len(),
                full_z.len()
            );
            continue;
        }
        let delta_name = format!("flashdesk-{old_ver_str}-to-{version}.fldp");
        let delta_out = out_dir.join(&delta_name);
        std::fs::write(&delta_out, &patch)?;
        let delta_sha = sha256_bytes(&patch);
        println!(
            "  - delta v{old_ver_str} -> v{version}: {} B ({:.1}% of full compressed)",
            patch.len(),
            (patch.len() as f64 * 100.0) / (full_z.len().max(1) as f64)
        );
        deltas.push(DeltaPatchInfo {
            from_version: old_ver_str,
            from_sha256: old_sha,
            filename: delta_name.clone(),
            size: patch.len() as u64,
            sha256: delta_sha.clone(),
        });
        artifact_paths.push((
            PublishFileSpec {
                filename: delta_name,
                size: patch.len() as u64,
                sha256: delta_sha,
            },
            delta_out,
        ));
    }

    let manifest = UpdateManifest {
        format_version: 1,
        version: version.to_string(),
        min_base_version: min_base.to_string(),
        released_at: unix_now(),
        notes: notes.to_string(),
        full: FullPackageInfo {
            filename: full_name,
            size: full_z.len() as u64,
            sha256: full_sha,
            target_size,
            target_sha256,
        },
        deltas,
    };
    let signed = root.sign_update_manifest(manifest);
    std::fs::write(
        out_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&signed)?,
    )?;

    // Archive current core exe into history_dir for future delta generation.
    let archive_path = history_dir.join(format!("flashdesk-core-{version}.exe"));
    std::fs::write(&archive_path, &new_bytes)?;

    Ok(PackResult {
        signed,
        out_dir: out_dir.to_path_buf(),
        artifact_paths,
        raw_core_bytes: new_bytes.len(),
        compressed_full_bytes: full_z.len(),
    })
}

async fn connect_and_auth_relay(
    addr: &str,
    identity: &DeviceIdentity,
    purpose: ConnectionPurpose,
) -> Result<TcpStream> {
    let mut stream = tokio::time::timeout(Duration::from_secs(8), TcpStream::connect(addr))
        .await
        .map_err(|_| anyhow::anyhow!("connect timeout to {addr}"))??;
    let _ = stream.set_nodelay(true);
    let raw_hello = read_frame(&mut stream).await?;
    let hello: RelayHello = bincode::deserialize(&raw_hello)?;
    let req = RelayAuthRequest {
        cert: identity.cert.clone(),
        challenge_sig: identity.sign_challenge(&hello.server_nonce, &purpose.canonical_bytes()),
        purpose,
    };
    write_frame(&mut stream, &bincode::serialize(&req)?).await?;
    let resp: RelayAuthResponse = bincode::deserialize(&read_frame(&mut stream).await?)?;
    match resp {
        RelayAuthResponse::Ok => Ok(stream),
        RelayAuthResponse::Rejected(reason) => anyhow::bail!("relay rejected auth: {reason}"),
    }
}

async fn exchange_update_msg(
    stream: &mut TcpStream,
    msg: &UpdateClientMsg,
) -> Result<UpdateServerMsg> {
    write_frame(stream, &bincode::serialize(msg)?).await?;
    let raw = tokio::time::timeout(Duration::from_secs(30), read_frame(stream))
        .await
        .map_err(|_| anyhow::anyhow!("relay update response timeout"))??;
    Ok(bincode::deserialize(&raw)?)
}

async fn push_to_relay(
    addr: &str,
    identity: &DeviceIdentity,
    pack: &PackResult,
) -> Result<()> {
    let mut stream =
        connect_and_auth_relay(addr, identity, ConnectionPurpose::UpdateChannel).await?;
    let specs: Vec<PublishFileSpec> = pack.artifact_paths.iter().map(|(s, _)| s.clone()).collect();
    match exchange_update_msg(
        &mut stream,
        &UpdateClientMsg::BeginPublish {
            manifest: pack.signed.clone(),
            files: specs,
        },
    )
    .await?
    {
        UpdateServerMsg::PublishReady => {}
        UpdateServerMsg::Error(e) => anyhow::bail!("BeginPublish rejected by {addr}: {e}"),
        other => anyhow::bail!("unexpected BeginPublish response from {addr}: {other:?}"),
    }

    for (spec, path) in &pack.artifact_paths {
        let bytes = std::fs::read(path)?;
        let mut offset = 0usize;
        while offset < bytes.len() {
            let end = (offset + UPLOAD_CHUNK_SIZE).min(bytes.len());
            let eof = end == bytes.len();
            match exchange_update_msg(
                &mut stream,
                &UpdateClientMsg::UploadChunk {
                    filename: spec.filename.clone(),
                    offset: offset as u64,
                    data: bytes[offset..end].to_vec(),
                    eof,
                },
            )
            .await?
            {
                UpdateServerMsg::UploadAck { .. } => {}
                UpdateServerMsg::Error(e) => anyhow::bail!("UploadChunk failed on {addr}: {e}"),
                other => anyhow::bail!("unexpected UploadAck from {addr}: {other:?}"),
            }
            offset = end;
        }
    }

    match exchange_update_msg(&mut stream, &UpdateClientMsg::CommitPublish).await? {
        UpdateServerMsg::PublishCommitted { version, files } => {
            println!("  -> {addr}: committed v{version} ({files} artifacts)");
            Ok(())
        }
        UpdateServerMsg::Error(e) => anyhow::bail!("CommitPublish failed on {addr}: {e}"),
        other => anyhow::bail!("unexpected CommitPublish response from {addr}: {other:?}"),
    }
}

pub fn build_setup_binary(
    stub: &Path,
    svc: &Path,
    launcher: &Path,
    uninstall: &Path,
    core: &Path,
    version: &str,
    out: &Path,
) -> Result<usize> {
    Version::parse(version.trim())
        .with_context(|| format!("invalid setup core version '{version}'"))?;
    let stub_bytes = std::fs::read(stub).with_context(|| format!("read {}", stub.display()))?;
    let svc_bytes = std::fs::read(svc).with_context(|| format!("read {}", svc.display()))?;
    let launcher_bytes =
        std::fs::read(launcher).with_context(|| format!("read {}", launcher.display()))?;
    let uninstall_bytes =
        std::fs::read(uninstall).with_context(|| format!("read {}", uninstall.display()))?;
    let core_bytes = std::fs::read(core).with_context(|| format!("read {}", core.display()))?;

    let packed = build_self_extracting_setup(
        &stub_bytes,
        version.trim(),
        BASE_VERSION,
        &[
            SetupInputBinary {
                role: "svc",
                bytes: &svc_bytes,
            },
            SetupInputBinary {
                role: "launcher",
                bytes: &launcher_bytes,
            },
            SetupInputBinary {
                role: "uninstall",
                bytes: &uninstall_bytes,
            },
            SetupInputBinary {
                role: "core",
                bytes: &core_bytes,
            },
        ],
    )?;

    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(out, &packed)?;
    Ok(packed.len())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Pack {
            core,
            version,
            min_base,
            notes,
            root_key,
            history_dir,
            out_dir,
        } => {
            let rk = resolve_root_key(root_key);
            let out = out_dir.unwrap_or_else(|| PathBuf::from("releases").join(&version));
            let res = pack_release(
                &core,
                &version,
                &min_base,
                &notes,
                &rk,
                &history_dir,
                &out,
            )?;
            println!(
                "Packed v{version}: raw={} B, full_compressed={} B, deltas={}, out={}",
                res.raw_core_bytes,
                res.compressed_full_bytes,
                res.signed.manifest.deltas.len(),
                res.out_dir.display()
            );
        }
        Commands::Publish {
            core,
            version,
            min_base,
            notes,
            root_key,
            history_dir,
            out_dir,
            config,
            relays,
            pack_only,
        } => {
            let rk = resolve_root_key(root_key);
            let out = out_dir.unwrap_or_else(|| PathBuf::from("releases").join(&version));
            let res = pack_release(
                &core,
                &version,
                &min_base,
                &notes,
                &rk,
                &history_dir,
                &out,
            )?;
            println!(
                "Packed v{version}: raw={} B, full_compressed={} B, deltas={}",
                res.raw_core_bytes,
                res.compressed_full_bytes,
                res.signed.manifest.deltas.len()
            );
            if pack_only {
                return Ok(());
            }

            let cfg_path = resolve_config(config);
            let cfg_text = std::fs::read_to_string(&cfg_path)
                .with_context(|| format!("failed to read config {}", cfg_path.display()))?;
            let cfg: PartialAppConfig = serde_json::from_str(&cfg_text)?;
            let bundle = cfg
                .bundle
                .context("config.json has no device bundle; cannot authenticate to relay")?;

            let mut targets = Vec::new();
            let mut seen = HashSet::new();
            if relays.is_empty() {
                for n in &bundle.relay_nodes {
                    if seen.insert(n.addr.trim().to_string()) {
                        targets.push(n.addr.trim().to_string());
                    }
                }
            } else {
                for r in relays {
                    if seen.insert(r.trim().to_string()) {
                        targets.push(r.trim().to_string());
                    }
                }
            }
            anyhow::ensure!(!targets.is_empty(), "no target relay addresses specified");

            println!("Publishing v{version} to {} relay node(s)...", targets.len());
            for addr in targets {
                push_to_relay(&addr, &bundle.identity, &res).await?;
            }
        }
        Commands::BuildSetup {
            stub,
            svc,
            launcher,
            uninstall,
            core,
            version,
            out,
        } => {
            let bytes =
                build_setup_binary(&stub, &svc, &launcher, &uninstall, &core, &version, &out)?;
            println!(
                "Built single-file installer {} ({:.2} MiB)",
                out.display(),
                bytes as f64 / (1024.0 * 1024.0)
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base_common::setup_payload::read_self_extracting_setup;
    use uuid::Uuid;

    #[test]
    fn pack_generates_signed_manifest_and_smaller_delta_from_history() {
        let dir = std::env::temp_dir().join(format!("flashdesk-rel-test-{}", Uuid::new_v4()));
        let hist = dir.join("history");
        let out = dir.join("out");
        std::fs::create_dir_all(&hist).unwrap();

        let root = RootKeypair::generate();
        let rk_path = dir.join("root.key");
        std::fs::write(&rk_path, root.to_b64()).unwrap();

        let mut old_bin = Vec::with_capacity(512 * 1024);
        let mut s = 0x9876_5432_10FE_DCBAu64;
        for _ in 0..(512 * 1024) {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
            old_bin.push((s >> 56) as u8);
        }
        std::fs::write(hist.join("flashdesk-core-0.1.0.exe"), &old_bin).unwrap();

        let mut new_bin = old_bin;
        new_bin[10_000..10_256].fill(0x55);
        let new_core = dir.join("flashdesk-0.2.0.exe");
        std::fs::write(&new_core, &new_bin).unwrap();

        let pack = pack_release(
            &new_core,
            "0.2.0",
            "1.0.0",
            "test notes",
            &rk_path,
            &hist,
            &out,
        )
        .unwrap();

        pack.signed
            .verify(&root.pubkey_bytes(), protocol::unix_timestamp_secs())
            .unwrap();
        assert_eq!(pack.signed.manifest.deltas.len(), 1);
        assert!(pack.signed.manifest.deltas[0].size < pack.signed.manifest.full.size);
        assert!(hist.join("flashdesk-core-0.2.0.exe").is_file());
        assert!(out.join("manifest.json").is_file());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_setup_packs_and_verifies_via_reader() {
        let dir = std::env::temp_dir().join(format!("flashdesk-setup-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let stub = dir.join("stub.exe");
        let svc = dir.join("svc.exe");
        let launcher = dir.join("launcher.exe");
        let uninstall = dir.join("uninstall.exe");
        let core = dir.join("core.exe");
        let out = dir.join("FlashDesk-Setup.exe");

        std::fs::write(&stub, b"MZ-STUB").unwrap();
        std::fs::write(&svc, b"SVC").unwrap();
        std::fs::write(&launcher, b"LAUNCHER").unwrap();
        std::fs::write(&uninstall, b"UNINSTALL").unwrap();
        std::fs::write(&core, b"CORE").unwrap();

        let total = build_setup_binary(
            &stub,
            &svc,
            &launcher,
            &uninstall,
            &core,
            "0.1.0",
            &out,
        )
        .unwrap();
        assert!(total > 7);

        let extracted = read_self_extracting_setup(&out).unwrap();
        assert_eq!(extracted.manifest.core_version, "0.1.0");
        assert_eq!(extracted.binaries.len(), 4);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
