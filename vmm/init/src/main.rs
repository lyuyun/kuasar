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
//! 2. Parse `/proc/cmdline` for `KUASAR_SANDBOX_ID` (pre-BOOTSTRAP logging only).
//! 2.5. Set up CoW overlay (tmpfs upper + ro lower) and `pivot_root`.
//! 3. Run init.d hooks in `/etc/kuasar/init.d/` (lexicographic order).
//! 4. Connect to host via vsock CID 2 port 1024.
//! 5. Receive `BOOTSTRAP` from host (app, args, env, hostname, DNS, …).
//! 6. Apply hostname / DNS from `BOOTSTRAP`.
//! 7. Fork+exec the application using `BOOTSTRAP.app`, `.args`, `.env`.
//! 8. Wait for readiness (probe from `BOOTSTRAP.ready_check`).
//! 9. Send `READY` to host.
//! 10. Enter the event loop:
//!    - Send `HEARTBEAT` every `heartbeat_interval_ms` (default 10 s).
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

use protocol::{encode, BootstrapPayload, GuestMessage, HostMessage, VSOCK_HOST_CID, VSOCK_PORT};
use readiness::ReadinessCheck;

const DEFAULT_HEARTBEAT_MS: u64 = 10_000;
/// Timeout for reading the BOOTSTRAP message after vsock is connected.
const BOOTSTRAP_TIMEOUT_SECS: u64 = 30;

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
}

// ── Application process management ───────────────────────────────────────────

/// Fork+exec the application and return its child handle.
async fn spawn_app(
    exe: &str,
    args: &[String],
    env: &HashMap<String, String>,
) -> Result<tokio::process::Child> {
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    for (k, v) in env {
        cmd.env(k, v);
    }

    let child = cmd.spawn().with_context(|| format!("spawn app: {}", exe))?;
    info!(
        "kuasar-init: spawned {:?} args={:?} pid={:?}",
        exe,
        args,
        child.id()
    );
    Ok(child)
}

/// Reap all zombie children without blocking.
///
/// Pass `skip_pid` with the PID of the managed app so that Tokio's own
/// `child.wait()` can consume its exit status; only grandchildren and other
/// non-Tokio-managed processes are reaped here.
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

/// Read the mandatory `BOOTSTRAP` message that the host sends right after
/// the vsock connection is established.
async fn read_bootstrap(
    reader: &mut BufReader<tokio::io::ReadHalf<VsockStream>>,
) -> Result<BootstrapPayload> {
    let mut line = String::new();
    tokio::time::timeout(
        Duration::from_secs(BOOTSTRAP_TIMEOUT_SECS),
        reader.read_line(&mut line),
    )
    .await
    .map_err(|_| {
        anyhow!(
            "timed out waiting for BOOTSTRAP from host after {}s",
            BOOTSTRAP_TIMEOUT_SECS
        )
    })?
    .context("read BOOTSTRAP line")?;

    match protocol::decode(line.trim_end()).context("decode BOOTSTRAP")? {
        HostMessage::Bootstrap(payload) => Ok(payload),
        other => Err(anyhow!(
            "expected BOOTSTRAP as first host message, got {:?}",
            other
        )),
    }
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
        write_resolv_conf(&cfg.dns_servers, &cfg.search_domains)?;
    }
    Ok(())
}

/// Apply hostname and DNS from the BOOTSTRAP payload.
fn apply_bootstrap_network(bootstrap: &BootstrapPayload) -> Result<()> {
    if let Some(hostname) = &bootstrap.hostname {
        nix::unistd::sethostname(hostname).context("sethostname")?;
        info!("kuasar-init: hostname set to {:?}", hostname);
    }
    if !bootstrap.dns_servers.is_empty() {
        write_resolv_conf(&bootstrap.dns_servers, &bootstrap.search_domains)?;
    }
    Ok(())
}

fn write_resolv_conf(servers: &[String], searches: &[String]) -> Result<()> {
    let mut content = String::new();
    for s in searches {
        content.push_str(&format!("search {}\n", s));
    }
    for ns in servers {
        content.push_str(&format!("nameserver {}\n", ns));
    }
    let _ = std::fs::create_dir_all("/etc");
    std::fs::write("/etc/resolv.conf", &content).context("write /etc/resolv.conf")?;
    info!(
        "kuasar-init: wrote /etc/resolv.conf ({} nameservers)",
        servers.len()
    );
    Ok(())
}

// ── Power off ─────────────────────────────────────────────────────────────────

fn power_off() -> ! {
    info!("kuasar-init: syncing filesystems and powering off");
    nix::unistd::sync();
    let _ = reboot(RebootMode::RB_POWER_OFF);
    std::process::exit(0)
}

// ── Current timestamp ms ─────────────────────────────────────────────────────

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
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
    // Only KUASAR_SANDBOX_ID is read from cmdline; it is used only for log
    // messages emitted before the BOOTSTRAP message arrives over vsock.  All
    // sandbox configuration (app, args, env, hostname, DNS, …) comes via BOOTSTRAP.
    let cmdline = Cmdline::read().context("read cmdline")?;
    let cmdline_sandbox_id = cmdline
        .get("KUASAR_SANDBOX_ID")
        .unwrap_or("<unknown>")
        .to_string();
    info!("kuasar-init: booting sandbox {}", cmdline_sandbox_id);

    // ── 2.5. CoW overlay ─────────────────────────────────────────────────────
    info!("kuasar-init: setting up CoW overlay");
    mount::setup_cow_overlay().context("setup CoW overlay")?;
    info!("kuasar-init: CoW overlay active");

    // ── 3. Run init.d hooks ───────────────────────────────────────────────────
    let hooks_result = tokio::task::spawn_blocking(|| hooks::run_hooks(hooks::HOOKS_DIR))
        .await
        .context("spawn_blocking for hooks")?;
    if let Err(e) = hooks_result {
        // No vsock yet; best-effort: try to connect and send FATAL.
        let _ = send_fatal_and_poweroff(&cmdline_sandbox_id, &e.to_string(), -1).await;
        return Err(e);
    }

    // ── 4. Connect vsock ──────────────────────────────────────────────────────
    let stream = connect_vsock().await.context("connect vsock")?;
    let (reader_half, mut writer_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader_half);

    // ── 5. Receive BOOTSTRAP ──────────────────────────────────────────────────
    let bootstrap = read_bootstrap(&mut reader)
        .await
        .context("read BOOTSTRAP")?;
    let sandbox_id = if !bootstrap.sandbox_id.is_empty() {
        bootstrap.sandbox_id.clone()
    } else {
        cmdline_sandbox_id
    };
    info!(
        "kuasar-init: BOOTSTRAP received for sandbox {} app={:?} args={:?}",
        sandbox_id, bootstrap.app, bootstrap.args
    );

    // ── 6. Apply hostname / DNS ───────────────────────────────────────────────
    if let Err(e) = apply_bootstrap_network(&bootstrap) {
        warn!("kuasar-init: bootstrap network apply failed: {}", e);
    }

    // ── 7. Spawn application ──────────────────────────────────────────────────
    if bootstrap.app.is_empty() {
        let reason = "BOOTSTRAP.app is empty — no workload to run";
        error!("kuasar-init: {}", reason);
        let _ = send_fatal_and_poweroff(&sandbox_id, reason, -1).await;
        return Err(anyhow!("{}", reason));
    }
    let mut child = spawn_app(&bootstrap.app, &bootstrap.args, &bootstrap.env)
        .await
        .context("spawn application")?;

    // ── 8. Readiness detection ────────────────────────────────────────────────
    let check =
        ReadinessCheck::from_cmdline(bootstrap.ready_check.as_deref(), bootstrap.ready_timeout_ms)
            .context("parse readiness check")?;

    if let Err(e) = check.wait().await {
        let reason = format!("readiness check failed: {}", e);
        let _ = send_fatal_and_poweroff(&sandbox_id, &reason, -1).await;
        return Err(anyhow!("{}", reason));
    }

    // ── 9. Send READY ─────────────────────────────────────────────────────────
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

    // ── 10. Event loop ────────────────────────────────────────────────────────
    let heartbeat_ms = bootstrap
        .heartbeat_interval_ms
        .unwrap_or(DEFAULT_HEARTBEAT_MS);
    let hb_duration = if heartbeat_ms == 0 {
        Duration::from_secs(365 * 24 * 3600)
    } else {
        Duration::from_millis(heartbeat_ms)
    };
    let mut hb_interval = interval(hb_duration);
    hb_interval.tick().await; // consume the first immediate tick

    let mut line_buf = String::new();
    let mut config_env: HashMap<String, String> = HashMap::new();

    loop {
        reap_zombies(child.id());

        tokio::select! {
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
                    Ok(HostMessage::Ping) => {}
                    Ok(HostMessage::Bootstrap(_)) => {
                        warn!("kuasar-init: unexpected BOOTSTRAP in event loop — ignored");
                    }
                    Err(e) => {
                        warn!("kuasar-init: unknown host message {:?}: {}", trimmed, e);
                    }
                }
            }

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
        let c = Cmdline::parse("root=/dev/vda ro KUASAR_SANDBOX_ID=sb-001").unwrap();
        assert_eq!(c.get("KUASAR_SANDBOX_ID"), Some("sb-001"));
        assert_eq!(c.get("root"), Some("/dev/vda"));
        assert_eq!(c.get("MISSING"), None);
    }

    #[test]
    fn test_cmdline_has_sandbox_id() {
        let c = Cmdline::parse("console=hvc0 root=/dev/pmem0 ro KUASAR_SANDBOX_ID=sb-1").unwrap();
        assert_eq!(c.get("KUASAR_SANDBOX_ID"), Some("sb-1"));
    }

    #[test]
    fn test_cmdline_ignores_flags_without_equals() {
        let c = Cmdline::parse("ro quiet KUASAR_SANDBOX_ID=sb-002").unwrap();
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
        assert!(s.ends_with('\n'));

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

    /// BOOTSTRAP can carry args with spaces and env vars with arbitrary characters.
    #[test]
    fn test_decode_bootstrap_with_spaces_and_special_chars() {
        let line = r#"{"type":"BOOTSTRAP","sandbox_id":"sb-1","app":"/usr/bin/my app","args":["--flag","value with spaces","--opt=a=b"],"env":{"MY_VAR":"hello world","PATH":"/usr/bin:/bin"},"hostname":"my-host","dns_servers":["8.8.8.8"],"search_domains":["example.com"]}"#;
        match protocol::decode(line).unwrap() {
            HostMessage::Bootstrap(b) => {
                assert_eq!(b.sandbox_id, "sb-1");
                assert_eq!(b.app, "/usr/bin/my app");
                assert_eq!(b.args, ["--flag", "value with spaces", "--opt=a=b"]);
                assert_eq!(b.env.get("MY_VAR").map(String::as_str), Some("hello world"));
                assert_eq!(b.env.get("PATH").map(String::as_str), Some("/usr/bin:/bin"));
                assert_eq!(b.hostname.as_deref(), Some("my-host"));
                assert_eq!(b.dns_servers, ["8.8.8.8"]);
                assert_eq!(b.search_domains, ["example.com"]);
            }
            _ => panic!("expected Bootstrap"),
        }
    }

    /// Minimal BOOTSTRAP (only sandbox_id + app) uses sane defaults.
    #[test]
    fn test_decode_bootstrap_minimal() {
        let line = r#"{"type":"BOOTSTRAP","sandbox_id":"sb-2","app":"/bin/sh"}"#;
        match protocol::decode(line).unwrap() {
            HostMessage::Bootstrap(b) => {
                assert_eq!(b.sandbox_id, "sb-2");
                assert_eq!(b.app, "/bin/sh");
                assert!(b.args.is_empty());
                assert!(b.env.is_empty());
                assert!(b.hostname.is_none());
                assert!(b.dns_servers.is_empty());
                assert!(b.heartbeat_interval_ms.is_none());
                assert!(b.ready_check.is_none());
            }
            _ => panic!("expected Bootstrap"),
        }
    }

    /// BOOTSTRAP with optional timing/readiness fields.
    #[test]
    fn test_decode_bootstrap_with_optional_fields() {
        let line = r#"{"type":"BOOTSTRAP","sandbox_id":"sb-3","app":"/bin/app","heartbeat_interval_ms":5000,"ready_check":"tcp://localhost:8080","ready_timeout_ms":30000}"#;
        match protocol::decode(line).unwrap() {
            HostMessage::Bootstrap(b) => {
                assert_eq!(b.heartbeat_interval_ms, Some(5000));
                assert_eq!(b.ready_check.as_deref(), Some("tcp://localhost:8080"));
                assert_eq!(b.ready_timeout_ms, Some(30000));
            }
            _ => panic!("expected Bootstrap"),
        }
    }

    // ── Appliance vs Standard cmdline structure ───────────────────────────────

    #[test]
    fn test_appliance_ext4_cmdline() {
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
        assert_eq!(c.get("task.sharefs_type"), None);
    }

    #[test]
    fn test_standard_cmdline_uses_partition_root() {
        let c = Cmdline::parse(
            "console=hvc0 root=/dev/pmem0p1 ro rootfstype=ext4 task.sharefs_type=virtiofs",
        )
        .unwrap();
        assert_eq!(c.get("root"), Some("/dev/pmem0p1"));
        assert_eq!(c.get("task.sharefs_type"), Some("virtiofs"));
    }
}
