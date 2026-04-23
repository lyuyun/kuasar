/*
Copyright 2022 The Kuasar Authors.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

//! `kuasar-ctl snapshot` — manage Kuasar application snapshots.
//!
//! Reads the same on-disk layout that vmm-sandboxer's `SnapshotStore` writes:
//!
//! ```text
//! <root>/
//!   <snapshot-id>/
//!     memory.bin        – VM memory image
//!     snapshot.json     – CHv device state
//!     metadata.json     – SnapshotMetadata (JSON)
//!     checksums.json    – SHA-256 hashes
//!   keys/
//!     <snapshot-key>    – file whose content is the snapshot id
//! ```

use std::{
    ffi::OsStr,
    io::{self, BufRead},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use anyhow::{anyhow, Context, Result};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ttrpc::context::with_timeout;
use vmm_common::api::snapshot_mgmt_ttrpc::SnapshotMgmtServiceClient;

pub const DEFAULT_STORE_ROOT: &str = "/var/lib/kuasar/snapshots";
pub const DEFAULT_MGMT_SOCK: &str = "/run/kuasar-vmm/snapshot-mgmt.sock";

const MEMORY_FILE: &str = "memory.bin";
const STATE_FILE: &str = "snapshot.json";
const METADATA_FILE: &str = "metadata.json";
const CHECKSUM_FILE: &str = "checksums.json";

// ──────────────────────────────────────────────
// On-disk types (mirrors vmm-sandboxer layout)
// ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotMetadata {
    id: SnapshotId,
    key: SnapshotKey,
    hypervisor: String,
    created_at: SystemTime,
    memory_size_bytes: u64,
    state_size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SnapshotId(String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SnapshotKey(String);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotChecksums {
    memory_sha256: String,
    state_sha256: String,
}

// ──────────────────────────────────────────────
// CLI definition
// ──────────────────────────────────────────────

#[derive(Debug, Subcommand)]
pub enum SnapshotCommands {
    /// Create a snapshot of a currently-running sandbox via ttrpc.
    Create {
        /// ID of the running sandbox to snapshot.
        sandbox_id: String,
        /// Human-readable key to assign to the snapshot in the store.
        key: String,
        /// Snapshot management ttrpc socket (unix path).
        #[arg(long, default_value = DEFAULT_MGMT_SOCK, env = "KUASAR_SNAPSHOT_MGMT_SOCK")]
        sock: String,
        /// RPC timeout in seconds.
        #[arg(long, default_value_t = 120)]
        timeout: u64,
    },

    /// List all snapshots in the store.
    List {
        /// Root directory of the snapshot store.
        #[arg(long, default_value = DEFAULT_STORE_ROOT, env = "KUASAR_SNAPSHOT_ROOT")]
        store: String,
        /// Show verbose metadata (full ID, state size).
        #[arg(short, long)]
        verbose: bool,
    },

    /// Show metadata for a single snapshot (by ID or key).
    Inspect {
        /// Snapshot ID (SHA-256 hex) or human-assigned key.
        target: String,
        /// Root directory of the snapshot store.
        #[arg(long, default_value = DEFAULT_STORE_ROOT, env = "KUASAR_SNAPSHOT_ROOT")]
        store: String,
    },

    /// Re-run SHA-256 integrity check for a snapshot (by ID or key).
    Verify {
        /// Snapshot ID (SHA-256 hex) or human-assigned key.
        target: String,
        /// Root directory of the snapshot store.
        #[arg(long, default_value = DEFAULT_STORE_ROOT, env = "KUASAR_SNAPSHOT_ROOT")]
        store: String,
    },

    /// Delete a snapshot from the store (by ID or key).
    Delete {
        /// Snapshot ID (SHA-256 hex) or human-assigned key.
        target: String,
        /// Root directory of the snapshot store.
        #[arg(long, default_value = DEFAULT_STORE_ROOT, env = "KUASAR_SNAPSHOT_ROOT")]
        store: String,
        /// Skip confirmation prompt.
        #[arg(short, long)]
        yes: bool,
    },

    /// Remove orphan snapshot directories not referenced by any key.
    Gc {
        /// Root directory of the snapshot store.
        #[arg(long, default_value = DEFAULT_STORE_ROOT, env = "KUASAR_SNAPSHOT_ROOT")]
        store: String,
        /// Dry-run: show what would be removed without actually deleting.
        #[arg(long)]
        dry_run: bool,
    },

    /// Show warm pool ready-slot counts scraped from the sandboxer's metrics endpoint.
    WarmPool {
        /// Metrics endpoint address (host:port).
        #[arg(long, default_value = "127.0.0.1:9090", env = "KUASAR_METRICS_ADDR")]
        addr: String,
    },
}

// ──────────────────────────────────────────────
// Entry point
// ──────────────────────────────────────────────

pub async fn run(cmd: SnapshotCommands) -> Result<i32> {
    match cmd {
        SnapshotCommands::Create {
            sandbox_id,
            key,
            sock,
            timeout,
        } => cmd_create(&sandbox_id, &key, &sock, timeout).await,
        SnapshotCommands::List { store, verbose } => cmd_list(&store, verbose).await,
        SnapshotCommands::Inspect { target, store } => cmd_inspect(&store, &target).await,
        SnapshotCommands::Verify { target, store } => cmd_verify(&store, &target).await,
        SnapshotCommands::Delete { target, store, yes } => {
            cmd_delete(&store, &target, yes).await
        }
        SnapshotCommands::Gc { store, dry_run } => cmd_gc(&store, dry_run).await,
        SnapshotCommands::WarmPool { addr } => cmd_warm_pool(&addr).await,
    }
}

// ──────────────────────────────────────────────
// Command implementations
// ──────────────────────────────────────────────

async fn cmd_create(sandbox_id: &str, key: &str, sock: &str, timeout_secs: u64) -> Result<i32> {
    let fd = connect_unix(sock)
        .await
        .with_context(|| format!("connect to snapshot mgmt socket {}", sock))?;

    let client = SnapshotMgmtServiceClient::new(ttrpc::r#async::Client::new(fd));
    let timeout_ns = Duration::from_secs(timeout_secs).as_nanos() as i64;

    let mut req = vmm_common::api::snapshot_mgmt::CreateSnapshotRequest::new();
    req.sandbox_id = sandbox_id.to_string();
    req.key = key.to_string();

    println!(
        "Creating snapshot of sandbox '{}' with key '{}'...",
        sandbox_id, key
    );

    let resp = client
        .create_snapshot(with_timeout(timeout_ns), &req)
        .await
        .with_context(|| "create_snapshot RPC failed")?;

    println!("Snapshot created:");
    println!("  ID     : {}", resp.snapshot_id);
    println!("  Key    : {}", resp.key);
    println!("  Memory : {}", format_bytes(resp.memory_size_bytes));
    println!("  State  : {}", format_bytes(resp.state_size_bytes));
    Ok(0)
}

async fn connect_unix(path: &str) -> Result<std::os::fd::RawFd> {
    use std::{os::fd::IntoRawFd, os::unix::net::UnixStream};

    let path = path.to_string();
    tokio::task::spawn_blocking(move || {
        let stream = UnixStream::connect(&path)
            .with_context(|| format!("UnixStream::connect({})", path))?;
        Ok(stream.into_raw_fd())
    })
    .await
    .map_err(|e| anyhow!("spawn_blocking: {}", e))?
}

async fn cmd_list(root: &str, verbose: bool) -> Result<i32> {
    let snapshots = list_all(root).await?;

    if snapshots.is_empty() {
        println!("No snapshots found in {}", root);
        return Ok(0);
    }

    println!(
        "{:<22} {:<14} {:<12} {:<18} {:<12}",
        "KEY", "ID (prefix)", "HYPERVISOR", "CREATED", "MEMORY"
    );
    println!("{}", "─".repeat(82));

    for meta in &snapshots {
        let age = format_age(meta.created_at);
        let id_prefix = &meta.id.0[..meta.id.0.len().min(12)];
        println!(
            "{:<22} {:<14} {:<12} {:<18} {:<12}",
            truncate(&meta.key.0, 22),
            id_prefix,
            meta.hypervisor,
            age,
            format_bytes(meta.memory_size_bytes),
        );
        if verbose {
            println!("    full-id : {}", meta.id.0);
            println!("    state   : {}", format_bytes(meta.state_size_bytes));
        }
    }

    println!("\n{} snapshot(s)", snapshots.len());
    Ok(0)
}

async fn cmd_inspect(root: &str, target: &str) -> Result<i32> {
    let meta = resolve(root, target).await?;
    print_metadata(root, &meta);
    Ok(0)
}

async fn cmd_verify(root: &str, target: &str) -> Result<i32> {
    let meta = resolve(root, target).await?;
    let snap_dir = Path::new(root).join(&meta.id.0);

    let checksum_path = snap_dir.join(CHECKSUM_FILE);
    let checksum_bytes = tokio::fs::read(&checksum_path)
        .await
        .with_context(|| format!("read {}", checksum_path.display()))?;
    let expected: SnapshotChecksums = serde_json::from_slice(&checksum_bytes)?;

    verify_file(snap_dir.join(MEMORY_FILE), &expected.memory_sha256, "memory.bin").await?;
    verify_file(snap_dir.join(STATE_FILE), &expected.state_sha256, "snapshot.json").await?;

    println!("OK  {} ({})", &meta.id.0[..12], meta.key.0);
    Ok(0)
}

async fn cmd_delete(root: &str, target: &str, yes: bool) -> Result<i32> {
    let meta = resolve(root, target).await?;

    if !yes {
        let total = meta.memory_size_bytes + meta.state_size_bytes;
        print!(
            "Delete snapshot {} (key='{}', {})? [y/N] ",
            &meta.id.0[..12],
            meta.key.0,
            format_bytes(total),
        );
        use std::io::Write;
        io::stdout().flush().ok();
        let stdin = io::stdin();
        let answer = stdin
            .lock()
            .lines()
            .next()
            .and_then(|l| l.ok())
            .unwrap_or_default();
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("Aborted.");
            return Ok(0);
        }
    }

    let snap_dir = Path::new(root).join(&meta.id.0);
    tokio::fs::remove_dir_all(&snap_dir)
        .await
        .with_context(|| format!("remove {}", snap_dir.display()))?;

    // Remove any key files that pointed to this ID.
    let keys_dir = Path::new(root).join("keys");
    if let Ok(mut entries) = tokio::fs::read_dir(&keys_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Ok(id_str) = tokio::fs::read_to_string(entry.path()).await {
                if id_str.trim() == meta.id.0 {
                    let _ = tokio::fs::remove_file(entry.path()).await;
                }
            }
        }
    }

    println!("Deleted {}", meta.id.0);
    Ok(0)
}

async fn cmd_gc(root: &str, dry_run: bool) -> Result<i32> {
    // Collect all IDs that are referenced by at least one key.
    let mut referenced = std::collections::HashSet::new();
    let keys_dir = Path::new(root).join("keys");
    if let Ok(mut entries) = tokio::fs::read_dir(&keys_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Ok(id) = tokio::fs::read_to_string(entry.path()).await {
                referenced.insert(id.trim().to_string());
            }
        }
    }

    let mut removed = 0usize;
    let mut entries = match tokio::fs::read_dir(root).await {
        Ok(e) => e,
        Err(e) => return Err(anyhow!("open store root {}: {}", root, e)),
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("")
            .to_string();

        if name == "keys" || !path.is_dir() {
            continue;
        }

        // Only consider 64-char hex directories (SHA-256 IDs).
        if name.len() != 64 || !name.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }

        if !referenced.contains(&name) {
            if dry_run {
                println!("would remove orphan {}", name);
            } else {
                tokio::fs::remove_dir_all(&path)
                    .await
                    .with_context(|| format!("remove {}", path.display()))?;
                println!("removed orphan {}", name);
            }
            removed += 1;
        }
    }

    match (removed, dry_run) {
        (0, _) => println!("gc: nothing to remove"),
        (n, true) => println!("gc: would remove {} orphan(s) (dry-run)", n),
        (n, false) => println!("gc: removed {} orphan(s)", n),
    }
    Ok(0)
}

async fn cmd_warm_pool(addr: &str) -> Result<i32> {
    let text = scrape_metrics(addr).await.with_context(|| {
        format!(
            "failed to scrape metrics from http://{}/metrics — is the sandboxer running?",
            addr
        )
    })?;

    // Parse lines of the form:
    //   kuasar_warm_pool_size{key="mykey"} 2
    let prefix = "kuasar_warm_pool_size{";
    let mut rows: Vec<(String, f64)> = Vec::new();
    for line in text.lines() {
        if line.starts_with('#') || !line.starts_with(prefix) {
            continue;
        }
        if let Some((labels_and_rest, value_str)) = line.rsplit_once('}') {
            let value: f64 = value_str.trim().parse().unwrap_or(0.0);
            let key = labels_and_rest
                .trim_start_matches(prefix)
                .split(',')
                .find_map(|part| {
                    let p = part.trim().trim_matches('"');
                    if p.starts_with("key=") {
                        Some(p.trim_start_matches("key=").trim_matches('"').to_string())
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| labels_and_rest.to_string());
            rows.push((key, value));
        }
    }

    if rows.is_empty() {
        println!("warm pool: no data (sandboxer may not have snapshot keys configured)");
        return Ok(0);
    }

    println!("{:<40} {:>6}", "SNAPSHOT KEY", "READY");
    println!("{}", "─".repeat(48));
    for (key, count) in &rows {
        println!("{:<40} {:>6}", truncate(key, 40), *count as usize);
    }
    println!("\nsource: http://{}/metrics", addr);
    Ok(0)
}

/// Minimal HTTP/1.1 GET of `http://<addr>/metrics` via raw TCP (no extra deps).
async fn scrape_metrics(addr: &str) -> Result<String> {
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .with_context(|| format!("connect to {}", addr))?;

    let request = format!(
        "GET /metrics HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        addr
    );
    stream.write_all(request.as_bytes()).await?;

    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;

    let text = String::from_utf8_lossy(&response);

    // Strip HTTP headers (everything before the first blank line).
    if let Some(body_start) = text.find("\r\n\r\n") {
        Ok(text[body_start + 4..].to_string())
    } else {
        Ok(text.to_string())
    }
}

// ──────────────────────────────────────────────
// Store reading helpers
// ──────────────────────────────────────────────

async fn list_all(root: &str) -> Result<Vec<SnapshotMetadata>> {
    let mut result = Vec::new();
    let mut entries = match tokio::fs::read_dir(root).await {
        Ok(e) => e,
        // Empty store is not an error.
        Err(_) => return Ok(result),
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("")
            .to_string();

        if name == "keys" || !path.is_dir() {
            continue;
        }

        let meta_path = path.join(METADATA_FILE);
        if let Ok(bytes) = tokio::fs::read(&meta_path).await {
            if let Ok(meta) = serde_json::from_slice::<SnapshotMetadata>(&bytes) {
                result.push(meta);
            }
        }
    }

    // Sort by creation time, newest first.
    result.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(result)
}

/// Resolve a target string (key or ID) to SnapshotMetadata.
async fn resolve(root: &str, target: &str) -> Result<SnapshotMetadata> {
    // Try as a key first.
    let key_path = PathBuf::from(root).join("keys").join(target);
    if let Ok(id_str) = tokio::fs::read_to_string(&key_path).await {
        let id = id_str.trim().to_string();
        let meta_path = PathBuf::from(root).join(&id).join(METADATA_FILE);
        let bytes = tokio::fs::read(&meta_path)
            .await
            .with_context(|| format!("read metadata for id {}", id))?;
        return Ok(serde_json::from_slice(&bytes)?);
    }

    // Try as a snapshot ID (full or prefix match).
    let snapshots = list_all(root).await?;
    let matches: Vec<_> = snapshots
        .into_iter()
        .filter(|m| m.id.0.starts_with(target))
        .collect();

    match matches.len() {
        0 => Err(anyhow!(
            "'{}' does not match any snapshot key or ID",
            target
        )),
        1 => Ok(matches.into_iter().next().unwrap()),
        n => Err(anyhow!(
            "ambiguous prefix '{}' matches {} snapshots — use more characters",
            target,
            n
        )),
    }
}

async fn verify_file(path: PathBuf, expected_hex: &str, label: &str) -> Result<()> {
    let data = tokio::fs::read(&path)
        .await
        .with_context(|| format!("read {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected_hex {
        return Err(anyhow!(
            "integrity check failed for {}: expected {}, got {}",
            label,
            &expected_hex[..16],
            &actual[..16],
        ));
    }
    Ok(())
}

// ──────────────────────────────────────────────
// Display helpers
// ──────────────────────────────────────────────

fn print_metadata(root: &str, meta: &SnapshotMetadata) {
    let total = meta.memory_size_bytes + meta.state_size_bytes;
    println!("ID         : {}", meta.id.0);
    println!("Key        : {}", meta.key.0);
    println!("Hypervisor : {}", meta.hypervisor);
    println!("Created    : {}", format_age(meta.created_at));
    println!("Memory     : {}", format_bytes(meta.memory_size_bytes));
    println!("State      : {}", format_bytes(meta.state_size_bytes));
    println!("Total      : {}", format_bytes(total));
    println!(
        "Path       : {}/{}/",
        root.trim_end_matches('/'),
        meta.id.0
    );
}

fn format_bytes(bytes: u64) -> String {
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.0} MB", bytes as f64 / MB as f64)
    } else {
        format!("{} KB", bytes / 1024)
    }
}

fn format_age(t: SystemTime) -> String {
    match SystemTime::now().duration_since(t) {
        Ok(d) => format!("{} ago", fmt_dur(d)),
        Err(_) => "just now".to_string(),
    }
}

fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{}s", s)
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86400)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}
