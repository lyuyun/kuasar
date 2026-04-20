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

//! kuasar-init — lightweight PID 1 init for Appliance Mode microVMs.
//!
//! # Startup sequence
//!
//! 1. Mount `/proc`, `/sys`, `/dev`, `/dev/pts`, `/run`.
//! 2. Parse `/proc/cmdline` for `KUASAR_SANDBOX_ID`, `KUASAR_APP`, etc.
//! 2.5. Set up CoW overlay (tmpfs upper + ro lower) and `pivot_root`.
//! 3. Run init.d hooks in `/etc/kuasar/init.d/` (lexicographic order).
//! 4. Fork+exec the application (`KUASAR_APP`).
//! 5. Wait for readiness (mode from `KUASAR_READY`).
//! 6. Connect to host via vsock CID 2 port 1024 and send `READY`.
//! 7. Enter the event loop:
//!    - Send `HEARTBEAT` every `KUASAR_HEARTBEAT_INTERVAL_MS` (default 10 s).
//!    - Handle incoming `SHUTDOWN` / `CONFIG` / `PING` messages.
//!    - Reap zombie children (SIGCHLD).
//!    - On app exit: send `FATAL` and power off.

use std::{
    collections::HashMap,
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};
use log::{error, info, warn};
use nix::{
    sys::{
        reboot::{reboot, RebootMode},
        signal::{kill, Signal},
        wait::{waitpid, WaitPidFlag, WaitStatus},
    },
    unistd::Pid,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    time::{interval, sleep},
};
use tokio_vsock::VsockStream;

mod hooks;
mod mount;
mod protocol;
mod readiness;

use protocol::{encode, GuestMessage, HostMessage, VSOCK_HOST_CID, VSOCK_PORT};
use readiness::ReadinessCheck;

const DEFAULT_HEARTBEAT_MS: u64 = 10_000;

// ── Kernel cmdline parsing ────────────────────────────────────────────────────

/// Key=value parameters extracted from `/proc/cmdline`.
struct Cmdline {
    params: HashMap<String, String>,
}

impl Cmdline {
    fn read() -> Result<Self> {
        let raw = std::fs::read_to_string("/proc/cmdline").context("read /proc/cmdline")?;
        Self::parse(&raw)
    }

    fn parse(raw: &str) -> Result<Self> {
        let mut params = HashMap::new();
        for token in raw.split_whitespace() {
            if let Some((k, v)) = token.split_once('=') {
                params.insert(k.to_string(), v.to_string());
            }
        }
        Ok(Self { params })
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.params.get(key).map(String::as_str)
    }

    fn get_u64(&self, key: &str) -> Option<u64> {
        self.params.get(key)?.parse().ok()
    }
}

// ── Application process management ───────────────────────────────────────────

/// Fork+exec the application and return its PID.
async fn spawn_app(
    app_cmdline: &str,
    env: &HashMap<String, String>,
) -> Result<tokio::process::Child> {
    // Kernel cmdline has no quoting mechanism, so arguments containing spaces
    // cannot be expressed via KUASAR_APP.  Use indexed params (KUASAR_APP_ARG_0
    // etc.) to lift this restriction in a future iteration.
    let mut parts = app_cmdline.split_whitespace();
    let exe = parts.next().ok_or_else(|| anyhow!("KUASAR_APP is empty"))?;
    let args: Vec<&str> = parts.collect();

    let mut cmd = Command::new(exe);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    for (k, v) in env {
        cmd.env(k, v);
    }

    let child = cmd.spawn().with_context(|| format!("spawn app: {}", exe))?;
    info!(
        "kuasar-init: spawned app {:?} pid={:?}",
        app_cmdline,
        child.id()
    );
    Ok(child)
}

/// Reap all zombie children without blocking.
///
/// Pass `skip_pid` with the PID of the `tokio::process::Child` app so that
/// Tokio's own `child.wait()` can consume its exit status; only grandchildren
/// and other non-Tokio-managed processes are reaped here.
fn reap_zombies(skip_pid: Option<u32>) {
    loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(pid, code)) => {
                if skip_pid.map_or(false, |sp| sp as i32 == pid.as_raw()) {
                    break;
                }
                info!("kuasar-init: reaped child pid={} exit={}", pid, code);
            }
            Ok(WaitStatus::Signaled(pid, sig, _)) => {
                if skip_pid.map_or(false, |sp| sp as i32 == pid.as_raw()) {
                    break;
                }
                info!("kuasar-init: reaped child pid={} signal={}", pid, sig);
            }
            Ok(WaitStatus::StillAlive) | Err(_) => break,
            _ => break,
        }
    }
}

// ── vsock connection ──────────────────────────────────────────────────────────

/// Connect to the host vsock server with retries.
///
/// Cloud Hypervisor proxies AF_VSOCK connections from the guest (CID 2, port
/// 1024) to a Unix socket on the host that ApplianceRuntime is listening on.
async fn connect_vsock() -> Result<VsockStream> {
    const MAX_RETRIES: u32 = 60;
    const RETRY_DELAY_MS: u64 = 500;

    for attempt in 1..=MAX_RETRIES {
        match VsockStream::connect(VSOCK_HOST_CID, VSOCK_PORT).await {
            Ok(stream) => {
                info!(
                    "kuasar-init: vsock connected to host CID {} port {}",
                    VSOCK_HOST_CID, VSOCK_PORT
                );
                return Ok(stream);
            }
            Err(e) => {
                if attempt == MAX_RETRIES {
                    return Err(anyhow!(
                        "vsock connect failed after {} retries: {}",
                        MAX_RETRIES,
                        e
                    ));
                }
                warn!(
                    "kuasar-init: vsock connect attempt {}/{}: {}",
                    attempt, MAX_RETRIES, e
                );
                sleep(Duration::from_millis(RETRY_DELAY_MS)).await;
            }
        }
    }
    unreachable!()
}

// ── CONFIG message handler ────────────────────────────────────────────────────

fn apply_config(
    cfg: &protocol::ConfigPayload,
    env_store: &mut HashMap<String, String>,
) -> Result<()> {
    if let Some(hostname) = &cfg.hostname {
        nix::unistd::sethostname(hostname).context("sethostname")?;
        info!("kuasar-init: hostname set to {:?}", hostname);
    }

    // Store env vars in a caller-owned map rather than calling std::env::set_var,
    // which is unsound in a multi-threaded runtime (deprecated in Rust 1.81).
    for (k, v) in &cfg.env {
        env_store.insert(k.clone(), v.clone());
    }

    if !cfg.dns_servers.is_empty() {
        let mut content = String::new();
        for s in &cfg.search_domains {
            content.push_str(&format!("search {}\n", s));
        }
        for ns in &cfg.dns_servers {
            content.push_str(&format!("nameserver {}\n", ns));
        }
        let _ = std::fs::create_dir_all("/etc");
        std::fs::write("/etc/resolv.conf", &content).context("write /etc/resolv.conf")?;
        info!(
            "kuasar-init: wrote /etc/resolv.conf ({} nameservers)",
            cfg.dns_servers.len()
        );
    }
    Ok(())
}

// ── Power off ─────────────────────────────────────────────────────────────────

fn power_off() -> ! {
    info!("kuasar-init: syncing filesystems and powering off");
    nix::unistd::sync();
    let _ = reboot(RebootMode::RB_POWER_OFF);
    // reboot() only returns on error; the kernel will halt the VM regardless.
    std::process::exit(0)
}

// ── Current timestamp ms ─────────────────────────────────────────────────────

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ── Main event loop ───────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    // Simple stderr logger; no external logging daemon is available at boot.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stderr)
        .init();

    if let Err(e) = run().await {
        error!("kuasar-init: fatal error: {:#}", e);
        power_off();
    }
}

async fn run() -> Result<()> {
    // ── 1. Basic filesystem setup ─────────────────────────────────────────────
    mount::mount_basic_filesystems().context("mount basic filesystems")?;

    // ── 2. Parse kernel cmdline ───────────────────────────────────────────────
    let cmdline = Cmdline::read().context("read cmdline")?;

    let sandbox_id = cmdline
        .get("KUASAR_SANDBOX_ID")
        .ok_or_else(|| anyhow!("KUASAR_SANDBOX_ID not set in kernel cmdline"))?
        .to_string();
    let app_cmdline = cmdline
        .get("KUASAR_APP")
        .ok_or_else(|| anyhow!("KUASAR_APP not set in kernel cmdline"))?
        .to_string();
    let heartbeat_ms = cmdline
        .get_u64("KUASAR_HEARTBEAT_INTERVAL_MS")
        .unwrap_or(DEFAULT_HEARTBEAT_MS);
    let ready_str = cmdline.get("KUASAR_READY").map(str::to_string);
    let ready_timeout_ms = cmdline.get_u64("KUASAR_READY_TIMEOUT_MS");

    // Hostname and DNS are injected by the sandboxer as kernel cmdline params so
    // they are available before the vsock handshake.  Apply them now, before the
    // app starts, so the workload sees the correct network identity from the start.
    let hostname = cmdline.get("KUASAR_HOSTNAME").map(str::to_string);
    let dns_servers: Vec<String> = cmdline
        .get("KUASAR_DNS_SERVERS")
        .map(|s| s.split(',').map(str::to_string).collect())
        .unwrap_or_default();
    let dns_searches: Vec<String> = cmdline
        .get("KUASAR_DNS_SEARCHES")
        .map(|s| s.split(',').map(str::to_string).collect())
        .unwrap_or_default();

    info!(
        "kuasar-init: sandbox_id={} app={:?}",
        sandbox_id, app_cmdline
    );

    // ── 2.5. CoW overlay ─────────────────────────────────────────────────────
    // kuasar-init is the init process for Appliance mode VMs only.  The root
    // filesystem (ext4 or erofs) is always mounted read-only by the kernel.
    // Layer a tmpfs upper via overlayfs and pivot_root into the merged view so
    // the workload can write freely.  Must run before hooks so hook scripts
    // also see a writable filesystem.
    info!("kuasar-init: setting up CoW overlay");
    mount::setup_cow_overlay().context("setup CoW overlay")?;
    info!("kuasar-init: CoW overlay active");

    // ── 3. Run init.d hooks ───────────────────────────────────────────────────
    // spawn_blocking so synchronous Command::status() calls inside run_hooks
    // don't starve Tokio worker threads for the duration of each hook script.
    let hooks_result = tokio::task::spawn_blocking(|| hooks::run_hooks(hooks::HOOKS_DIR))
        .await
        .context("spawn_blocking for hooks")?;
    if let Err(e) = hooks_result {
        let _ = send_fatal_and_poweroff(&sandbox_id, &e.to_string(), -1).await;
        return Err(e);
    }

    // ── 3.5. Apply static hostname / DNS from kernel cmdline ──────────────────
    // Only runs when at least one of the params is present; silently skipped for
    // images that manage their own network config.
    if hostname.is_some() || !dns_servers.is_empty() {
        let cfg = protocol::ConfigPayload {
            hostname,
            dns_servers,
            search_domains: dns_searches,
            env: Default::default(),
        };
        let mut _sink = HashMap::new();
        if let Err(e) = apply_config(&cfg, &mut _sink) {
            warn!("kuasar-init: static hostname/DNS apply failed: {}", e);
        }
    }

    // ── 4. Spawn application ──────────────────────────────────────────────────
    let extra_env: HashMap<String, String> = HashMap::new();
    let mut child = spawn_app(&app_cmdline, &extra_env)
        .await
        .context("spawn application")?;

    // ── 5. Readiness detection ────────────────────────────────────────────────
    let check = ReadinessCheck::from_cmdline(ready_str.as_deref(), ready_timeout_ms)
        .context("parse readiness check")?;

    if let Err(e) = check.wait().await {
        let reason = format!("readiness check failed: {}", e);
        let _ = send_fatal_and_poweroff(&sandbox_id, &reason, -1).await;
        return Err(anyhow!("{}", reason));
    }

    // ── 6. Connect vsock + send READY ─────────────────────────────────────────
    let stream = connect_vsock().await.context("connect vsock")?;
    let (reader_half, mut writer_half) = tokio::io::split(stream);

    let ready_msg = GuestMessage::Ready {
        sandbox_id: sandbox_id.clone(),
        version: "1".into(),
        init: "kuasar-init".into(),
    };
    writer_half
        .write_all(&encode(&ready_msg)?)
        .await
        .context("send READY")?;
    info!("kuasar-init: sent READY for sandbox {}", sandbox_id);

    // ── 7. Event loop ─────────────────────────────────────────────────────────
    // heartbeat_ms=0 disables heartbeats (function workloads); use a very long
    // duration so the interval branch effectively never fires.
    let hb_duration = if heartbeat_ms == 0 {
        Duration::from_secs(365 * 24 * 3600)
    } else {
        Duration::from_millis(heartbeat_ms)
    };
    let mut hb_interval = interval(hb_duration);
    hb_interval.tick().await; // consume the first immediate tick

    let mut reader = BufReader::new(reader_half);
    let mut line_buf = String::new();

    // Accumulates env vars received via CONFIG messages.  Passed explicitly to
    // child processes rather than via std::env::set_var (unsound in async context).
    let mut config_env: HashMap<String, String> = HashMap::new();

    loop {
        // Reap grandchildren and hook leftovers on every iteration; skip the
        // managed app child so Tokio's child.wait() can consume its exit status.
        reap_zombies(child.id());

        tokio::select! {
            // Periodic heartbeat (skipped when heartbeat_ms=0)
            _ = hb_interval.tick() => {
                if heartbeat_ms != 0 {
                    let hb = GuestMessage::Heartbeat {
                        sandbox_id: sandbox_id.clone(),
                        timestamp_ms: now_ms(),
                    };
                    if let Err(e) = writer_half.write_all(&encode(&hb)?).await {
                        warn!("kuasar-init: heartbeat write error: {}", e);
                    }
                }
            }

            // Incoming host message
            result = reader.read_line(&mut line_buf) => {
                let n = result.context("read host message")?;
                if n == 0 {
                    warn!("kuasar-init: host closed vsock connection");
                    power_off();
                }
                let trimmed = line_buf.trim_end().to_string();
                line_buf.clear();

                match protocol::decode(&trimmed) {
                    Ok(HostMessage::Shutdown { deadline_ms }) => {
                        info!("kuasar-init: received SHUTDOWN (deadline {}ms)", deadline_ms);
                        handle_shutdown(&mut child, deadline_ms).await;
                        power_off();
                    }
                    Ok(HostMessage::Config(cfg)) => {
                        if let Err(e) = apply_config(&cfg, &mut config_env) {
                            warn!("kuasar-init: CONFIG apply error: {}", e);
                        }
                    }
                    Ok(HostMessage::Ping) => {
                        // No response required.
                    }
                    Err(e) => {
                        warn!("kuasar-init: unknown host message {:?}: {}", trimmed, e);
                    }
                }
            }

            // Application exited unexpectedly
            status = child.wait() => {
                let exit_code = match status {
                    Ok(s) => s.code().unwrap_or(-1),
                    Err(_) => -1,
                };
                let reason = format!("application exited unexpectedly with code {}", exit_code);
                error!("kuasar-init: {}", reason);
                let fatal = GuestMessage::Fatal {
                    sandbox_id: sandbox_id.clone(),
                    reason,
                    exit_code,
                };
                let _ = writer_half.write_all(&encode(&fatal)?).await;
                power_off();
            }
        }
    }
}

/// Send FATAL via vsock and power off.
async fn send_fatal_and_poweroff(sandbox_id: &str, reason: &str, exit_code: i32) -> Result<()> {
    if let Ok(stream) = connect_vsock().await {
        let (_, mut w) = tokio::io::split(stream);
        let msg = GuestMessage::Fatal {
            sandbox_id: sandbox_id.to_string(),
            reason: reason.to_string(),
            exit_code,
        };
        let _ = w.write_all(&encode(&msg)?).await;
    }
    power_off();
}

/// Handle a SHUTDOWN message: SIGTERM the app, wait up to `deadline_ms`, then SIGKILL.
async fn handle_shutdown(child: &mut tokio::process::Child, deadline_ms: u64) {
    if let Some(pid) = child.id() {
        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
    }
    let deadline = Duration::from_millis(deadline_ms);
    match tokio::time::timeout(deadline, child.wait()).await {
        Ok(Ok(status)) => {
            info!("kuasar-init: app exited after SIGTERM: {:?}", status.code());
        }
        _ => {
            warn!(
                "kuasar-init: app did not exit within {}ms, sending SIGKILL",
                deadline_ms
            );
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn test_cmdline_parse_basic() {
        let c = Cmdline::parse("root=/dev/vda ro KUASAR_SANDBOX_ID=sb-001 KUASAR_APP=/usr/bin/app")
            .unwrap();
        assert_eq!(c.get("KUASAR_SANDBOX_ID"), Some("sb-001"));
        assert_eq!(c.get("KUASAR_APP"), Some("/usr/bin/app"));
        assert_eq!(c.get("root"), Some("/dev/vda"));
        assert_eq!(c.get("MISSING"), None);
    }

    #[test]
    fn test_cmdline_parse_u64() {
        let c = Cmdline::parse("KUASAR_HEARTBEAT_INTERVAL_MS=5000 KUASAR_READY_TIMEOUT_MS=30000")
            .unwrap();
        assert_eq!(c.get_u64("KUASAR_HEARTBEAT_INTERVAL_MS"), Some(5000));
        assert_eq!(c.get_u64("KUASAR_READY_TIMEOUT_MS"), Some(30000));
        assert_eq!(c.get_u64("MISSING"), None);
    }

    #[test]
    fn test_cmdline_ignores_flags_without_equals() {
        let c = Cmdline::parse("ro quiet KUASAR_SANDBOX_ID=sb-002").unwrap();
        // "ro" and "quiet" have no `=` so they are not in the map.
        assert_eq!(c.get("ro"), None);
        assert_eq!(c.get("KUASAR_SANDBOX_ID"), Some("sb-002"));
    }

    #[test]
    fn test_ready_message_json() {
        let msg = GuestMessage::Ready {
            sandbox_id: "sb-001".into(),
            version: "1".into(),
            init: "kuasar-init".into(),
        };
        let bytes = encode(&msg).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.ends_with('\n'), "JSON Line must end with newline");

        let v: Value = serde_json::from_str(s.trim_end()).unwrap();
        assert_eq!(v["type"], "READY");
        assert_eq!(v["sandbox_id"], "sb-001");
        assert_eq!(v["version"], "1");
        assert_eq!(v["init"], "kuasar-init");
    }

    #[test]
    fn test_heartbeat_message_json() {
        let msg = GuestMessage::Heartbeat {
            sandbox_id: "sb-001".into(),
            timestamp_ms: 1_700_000_000_000,
        };
        let bytes = encode(&msg).unwrap();
        let v: Value =
            serde_json::from_str(std::str::from_utf8(&bytes).unwrap().trim_end()).unwrap();
        assert_eq!(v["type"], "HEARTBEAT");
        assert_eq!(v["sandbox_id"], "sb-001");
    }

    #[test]
    fn test_fatal_message_json() {
        let msg = GuestMessage::Fatal {
            sandbox_id: "sb-001".into(),
            reason: "init.d hook failed: 10-env.sh (exit 1)".into(),
            exit_code: -1,
        };
        let bytes = encode(&msg).unwrap();
        let v: Value =
            serde_json::from_str(std::str::from_utf8(&bytes).unwrap().trim_end()).unwrap();
        assert_eq!(v["type"], "FATAL");
        assert!(v["reason"].as_str().unwrap().contains("init.d hook failed"));
    }

    #[test]
    fn test_decode_shutdown() {
        let line = r#"{"type":"SHUTDOWN","deadline_ms":30000}"#;
        match protocol::decode(line).unwrap() {
            HostMessage::Shutdown { deadline_ms } => assert_eq!(deadline_ms, 30000),
            _ => panic!("expected Shutdown"),
        }
    }

    #[test]
    fn test_decode_ping() {
        let line = r#"{"type":"PING"}"#;
        assert!(matches!(protocol::decode(line).unwrap(), HostMessage::Ping));
    }

    #[test]
    fn test_decode_config() {
        let line = r#"{"type":"CONFIG","hostname":"my-vm","dns_servers":["8.8.8.8"],"search_domains":[],"env":{}}"#;
        match protocol::decode(line).unwrap() {
            HostMessage::Config(cfg) => {
                assert_eq!(cfg.hostname.as_deref(), Some("my-vm"));
                assert_eq!(cfg.dns_servers, ["8.8.8.8"]);
            }
            _ => panic!("expected Config"),
        }
    }

    #[test]
    fn test_decode_unknown_message_fails() {
        let line = r#"{"type":"UNKNOWN_FUTURE_MSG","data":"x"}"#;
        assert!(protocol::decode(line).is_err());
    }

    // ── Appliance vs Standard cmdline structure ───────────────────────────────

    #[test]
    fn test_appliance_ext4_cmdline_uses_partition() {
        // Appliance ext4 uses pmem0p1 (same partitioned format as Standard mode)
        // so build_image.sh can be reused.
        let c = Cmdline::parse(
            "console=hvc0 root=/dev/pmem0p1 ro rootfstype=ext4 KUASAR_SANDBOX_ID=sb-1",
        )
        .unwrap();
        assert_eq!(c.get("root"), Some("/dev/pmem0p1"));
        assert_eq!(c.get("rootfstype"), Some("ext4"));
        assert_eq!(c.get("task.sharefs_type"), None);
    }

    #[test]
    fn test_appliance_erofs_cmdline() {
        let c = Cmdline::parse(
            "console=hvc0 root=/dev/pmem0 ro rootfstype=erofs KUASAR_SANDBOX_ID=sb-2",
        )
        .unwrap();
        assert_eq!(c.get("rootfstype"), Some("erofs"));
        // No task params in appliance cmdline.
        assert_eq!(c.get("task.sharefs_type"), None);
    }

    #[test]
    fn test_standard_cmdline_uses_partition_root() {
        // Standard mode uses pmem0p1 (partitioned device) with virtiofs.
        let c = Cmdline::parse(
            "console=hvc0 root=/dev/pmem0p1 ro rootfstype=ext4 task.sharefs_type=virtiofs",
        )
        .unwrap();
        assert_eq!(c.get("root"), Some("/dev/pmem0p1"));
        assert_eq!(c.get("task.sharefs_type"), Some("virtiofs"));
    }
}
