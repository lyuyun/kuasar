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

//! ApplianceRuntime: host-side vsock server for the guest handshake + event loop.

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use containerd_sandbox::{data::SandboxData, error::Result, signal::ExitSignal};
use serde::Serialize;
use tokio::sync::Mutex;

use super::GuestRuntime;
use crate::{
    network::Network,
    profile::{
        ANNOTATION_APPLIANCE_APP, ANNOTATION_APPLIANCE_ARGS, ANNOTATION_APPLIANCE_ENV_PREFIX,
        ANNOTATION_APPLIANCE_HEARTBEAT_MS, ANNOTATION_APPLIANCE_READY_CHECK,
        ANNOTATION_APPLIANCE_READY_TIMEOUT_MS,
    },
};

/// How long [`ApplianceRuntime::wait_ready`] waits for the guest to connect and
/// send the READY message before returning `Err`.
#[cfg(test)]
const APPLIANCE_READY_TIMEOUT_SECS: u64 = 1;
#[cfg(not(test))]
const APPLIANCE_READY_TIMEOUT_SECS: u64 = 30;

// ── Bootstrap payload (host → guest) ─────────────────────────────────────────

/// Full sandbox configuration delivered to the guest over vsock before the
/// workload is started.  Serialised as a JSON Line and sent immediately after
/// the guest connects; replaces per-parameter kernel cmdline injection.
#[derive(Debug, Serialize)]
pub struct BootstrapPayload {
    /// Always `"BOOTSTRAP"` — lets `kuasar-init` identify the message type.
    #[serde(rename = "type")]
    pub message_type: &'static str,
    /// Sandbox ID; the guest includes this in its `READY` message.
    pub sandbox_id: String,
    /// Absolute path to the workload executable inside the guest rootfs.
    pub app: String,
    /// Command-line arguments.  May contain spaces or any other characters.
    pub args: Vec<String>,
    /// Environment variables.  Values may contain any characters.
    pub env: HashMap<String, String>,
    /// Hostname to set in the guest (`sethostname`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// DNS nameserver addresses to write to `/etc/resolv.conf`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dns_servers: Vec<String>,
    /// DNS search domains.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub search_domains: Vec<String>,
    /// Heartbeat interval ms.  `None` → guest uses its default (10 s).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heartbeat_interval_ms: Option<u64>,
    /// Readiness probe specification.  `None` → no probe.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ready_check: Option<String>,
    /// Readiness probe timeout ms.  `None` → no timeout.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ready_timeout_ms: Option<u64>,
}

impl BootstrapPayload {
    /// Build a BOOTSTRAP payload from the sandbox metadata.
    ///
    /// Resolution order for `app`:
    /// 1. Pod annotation `io.kuasar.appliance.app`
    /// 2. `default_app` (from the `appliance_app` TOML field)
    fn from_sandbox_data(sandbox_id: &str, data: &SandboxData, default_app: &str) -> Self {
        let config = data.config.as_ref();

        let app = config
            .and_then(|c| c.annotations.get(ANNOTATION_APPLIANCE_APP))
            .map(String::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(default_app)
            .to_string();

        let args: Vec<String> = config
            .and_then(|c| c.annotations.get(ANNOTATION_APPLIANCE_ARGS))
            .and_then(|v| serde_json::from_str::<Vec<String>>(v).ok())
            .unwrap_or_default();

        let env: HashMap<String, String> = config
            .map(|c| {
                c.annotations
                    .iter()
                    .filter_map(|(k, v)| {
                        k.strip_prefix(ANNOTATION_APPLIANCE_ENV_PREFIX)
                            .map(|name| (name.to_string(), v.clone()))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let hostname = config
            .map(|c| c.hostname.as_str())
            .filter(|h| !h.is_empty())
            .map(str::to_string);

        let (dns_servers, search_domains) = config
            .and_then(|c| c.dns_config.as_ref())
            .map(|d| (d.servers.clone(), d.searches.clone()))
            .unwrap_or_default();

        let heartbeat_interval_ms = config
            .and_then(|c| c.annotations.get(ANNOTATION_APPLIANCE_HEARTBEAT_MS))
            .and_then(|v| v.parse::<u64>().ok());

        let ready_check = config
            .and_then(|c| c.annotations.get(ANNOTATION_APPLIANCE_READY_CHECK))
            .filter(|v| !v.is_empty())
            .map(String::clone);

        let ready_timeout_ms = config
            .and_then(|c| c.annotations.get(ANNOTATION_APPLIANCE_READY_TIMEOUT_MS))
            .and_then(|v| v.parse::<u64>().ok());

        Self {
            message_type: "BOOTSTRAP",
            sandbox_id: sandbox_id.to_string(),
            app,
            args,
            env,
            hostname,
            dns_servers,
            search_domains,
            heartbeat_interval_ms,
            ready_check,
            ready_timeout_ms,
        }
    }

    fn to_json_line(&self) -> Result<Vec<u8>> {
        use anyhow::anyhow;
        use containerd_sandbox::error::Error;
        let mut bytes = serde_json::to_vec(self)
            .map_err(|e| Error::Other(anyhow!("serialize BOOTSTRAP: {}", e)))?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

// ── Runtime state ─────────────────────────────────────────────────────────────

/// Live connection state kept after the initial READY handshake.
struct RuntimeInner {
    reader: Option<tokio::io::BufReader<tokio::io::ReadHalf<tokio::net::UnixStream>>>,
    writer: Option<tokio::io::WriteHalf<tokio::net::UnixStream>>,
}

/// Host-side vsock server that sends `BOOTSTRAP`, waits for `READY`, then keeps
/// the connection alive for HEARTBEAT/FATAL monitoring and SHUTDOWN delivery.
///
/// ## Connection lifecycle
///
/// 1. [`wait_ready`] binds a Unix socket at `address`, accepts the guest
///    connection, writes the `BOOTSTRAP` message, reads and validates the
///    `READY` message, then **retains** both halves of the split stream in `inner`.
/// 2. [`start_forward_events`] takes ownership of the read half and spawns a
///    background task that logs `HEARTBEAT` messages and calls
///    `exit_signal.signal()` on `FATAL` or EOF.
/// 3. [`shutdown`] writes a `{"type":"SHUTDOWN","deadline_ms":N}` line over the
///    write half so `kuasar-init` can gracefully stop the workload.
///
/// [`wait_ready`]: ApplianceRuntime::wait_ready
/// [`start_forward_events`]: ApplianceRuntime::start_forward_events
/// [`shutdown`]: ApplianceRuntime::shutdown
pub struct ApplianceRuntime {
    /// The sandbox ID expected in the guest's `READY` message.
    pub(crate) sandbox_id: String,
    /// Cluster-wide default app executable (from TOML `appliance_app`).
    default_app: String,
    inner: Arc<Mutex<RuntimeInner>>,
}

impl ApplianceRuntime {
    pub fn new(sandbox_id: impl Into<String>, default_app: impl Into<String>) -> Self {
        Self {
            sandbox_id: sandbox_id.into(),
            default_app: default_app.into(),
            inner: Arc::new(Mutex::new(RuntimeInner {
                reader: None,
                writer: None,
            })),
        }
    }
}

#[async_trait]
impl GuestRuntime for ApplianceRuntime {
    async fn wait_ready(
        &mut self,
        address: &str,
        _network: Option<&Network>,
        data: &SandboxData,
    ) -> Result<()> {
        use anyhow::anyhow;
        use containerd_sandbox::error::Error;
        use tokio::{
            io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
            net::UnixListener,
        };

        let socket_path = address.strip_prefix("unix://").ok_or_else(|| {
            Error::Other(anyhow!(
                "ApplianceRuntime: unsupported address scheme: {}",
                address
            ))
        })?;

        let _ = std::fs::remove_file(socket_path);

        let listener = UnixListener::bind(socket_path)
            .map_err(|e| Error::Other(anyhow!("ApplianceRuntime: bind {}: {}", socket_path, e)))?;

        let (stream, _) = tokio::time::timeout(
            std::time::Duration::from_secs(APPLIANCE_READY_TIMEOUT_SECS),
            listener.accept(),
        )
        .await
        .map_err(|_| {
            Error::Other(anyhow!(
                "ApplianceRuntime: timed out waiting for guest connection on {}",
                socket_path
            ))
        })?
        .map_err(|e| Error::Other(anyhow!("ApplianceRuntime: accept: {}", e)))?;

        // Split before read/write so both halves can be retained for later use.
        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut reader = BufReader::new(read_half);

        // ── Send BOOTSTRAP ────────────────────────────────────────────────────
        let bootstrap =
            BootstrapPayload::from_sandbox_data(&self.sandbox_id, data, &self.default_app);
        let bootstrap_line = bootstrap.to_json_line()?;
        write_half
            .write_all(&bootstrap_line)
            .await
            .map_err(|e| Error::Other(anyhow!("ApplianceRuntime: send BOOTSTRAP: {}", e)))?;

        // ── Read READY ────────────────────────────────────────────────────────
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .map_err(|e| Error::Other(anyhow!("ApplianceRuntime: read from guest: {}", e)))?;

        let msg: serde_json::Value = serde_json::from_str(line.trim_end()).map_err(|e| {
            Error::Other(anyhow!("ApplianceRuntime: invalid JSON from guest: {}", e))
        })?;

        match msg["type"].as_str().unwrap_or("") {
            "READY" => {
                let got_id = msg["sandbox_id"].as_str().unwrap_or("");
                if got_id != self.sandbox_id {
                    return Err(Error::Other(anyhow!(
                        "ApplianceRuntime: sandbox_id mismatch: expected {:?}, got {:?}",
                        self.sandbox_id,
                        got_id
                    )));
                }
                let mut inner = self.inner.lock().await;
                inner.reader = Some(reader);
                inner.writer = Some(write_half);
                Ok(())
            }
            "FATAL" => {
                let reason = msg["reason"].as_str().unwrap_or("(no reason)");
                Err(Error::Other(anyhow!(
                    "ApplianceRuntime: guest sent FATAL: {}",
                    reason
                )))
            }
            other => Err(Error::Other(anyhow!(
                "ApplianceRuntime: expected READY, got {:?}",
                other
            ))),
        }
    }

    async fn reconnect(&mut self, _address: &str) -> Result<()> {
        // Reconnect is a no-op: after a host-side process restart the VM is
        // still running; liveness is confirmed by the PID signal-0 check in
        // `CloudHypervisorVM::recover()`.  No re-handshake is needed.
        Ok(())
    }

    fn start_sync_clock(&self, _id: &str, _exit_signal: Arc<ExitSignal>) {
        // No-op: the guest VM maintains its own time via the hardware RTC.
    }

    /// Spawn a background task that reads HEARTBEAT / FATAL messages from the
    /// persistent vsock stream and signals `exit_signal` on FATAL or EOF.
    fn start_forward_events(&self, exit_signal: Arc<ExitSignal>) {
        use tokio::io::AsyncBufReadExt;

        let inner = Arc::clone(&self.inner);
        let sandbox_id = self.sandbox_id.clone();

        tokio::spawn(async move {
            let reader = {
                let mut guard = inner.lock().await;
                guard.reader.take()
            };

            let Some(mut reader) = reader else {
                log::error!(
                    "ApplianceRuntime({}): start_forward_events called with no persistent \
                     stream — guest FATAL/EOF will not be observed (host recovery path)",
                    sandbox_id
                );
                return;
            };

            let mut line = String::new();
            loop {
                line.clear();
                tokio::select! {
                    result = reader.read_line(&mut line) => {
                        match result {
                            Ok(0) => {
                                log::warn!(
                                    "ApplianceRuntime({}): vsock connection closed by guest",
                                    sandbox_id
                                );
                                exit_signal.signal();
                                return;
                            }
                            Err(e) => {
                                log::error!(
                                    "ApplianceRuntime({}): vsock read error: {}",
                                    sandbox_id, e
                                );
                                exit_signal.signal();
                                return;
                            }
                            Ok(_) => {}
                        }

                        let trimmed = line.trim_end();
                        match serde_json::from_str::<serde_json::Value>(trimmed) {
                            Ok(msg) => match msg["type"].as_str().unwrap_or("") {
                                "HEARTBEAT" => {
                                    log::debug!("ApplianceRuntime({}): HEARTBEAT", sandbox_id);
                                }
                                "FATAL" => {
                                    let reason =
                                        msg["reason"].as_str().unwrap_or("(no reason)");
                                    log::error!(
                                        "ApplianceRuntime({}): guest FATAL: {}",
                                        sandbox_id, reason
                                    );
                                    exit_signal.signal();
                                    return;
                                }
                                other => {
                                    log::warn!(
                                        "ApplianceRuntime({}): unexpected message type {:?}",
                                        sandbox_id, other
                                    );
                                }
                            },
                            Err(e) => {
                                log::warn!(
                                    "ApplianceRuntime({}): invalid JSON from guest: {}",
                                    sandbox_id, e
                                );
                            }
                        }
                    }
                    _ = exit_signal.wait() => {
                        return;
                    }
                }
            }
        });
    }

    /// Write a `SHUTDOWN` command over the persistent vsock stream.
    async fn shutdown(&mut self, deadline_ms: u64) -> Result<()> {
        use anyhow::anyhow;
        use containerd_sandbox::error::Error;
        use tokio::io::AsyncWriteExt;

        let mut inner = self.inner.lock().await;
        if let Some(ref mut writer) = inner.writer {
            let msg = serde_json::json!({
                "type": "SHUTDOWN",
                "deadline_ms": deadline_ms,
            });
            let line = format!("{}\n", msg);
            writer
                .write_all(line.as_bytes())
                .await
                .map_err(|e| Error::Other(anyhow!("ApplianceRuntime: shutdown write: {}", e)))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::Arc,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use containerd_sandbox::{data::SandboxData, signal::ExitSignal};

    use super::*;
    use crate::guest_runtime::RuntimeKind;

    fn unique_socket_path() -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("/tmp/kuasar-appliance-test-{}.sock", nanos)
    }

    // ── RuntimeKind::Appliance serde ──────────────────────────────────────────

    #[test]
    fn test_appliance_runtime_kind_serializes_to_appliance() {
        let json = serde_json::to_string(&RuntimeKind::Appliance).unwrap();
        assert_eq!(json, r#""appliance""#);
    }

    #[test]
    fn test_appliance_runtime_kind_deserializes_from_appliance() {
        let kind: RuntimeKind = serde_json::from_str(r#""appliance""#).unwrap();
        assert!(matches!(kind, RuntimeKind::Appliance));
    }

    #[test]
    fn test_default_runtime_kind_is_still_vmm_task() {
        assert!(matches!(RuntimeKind::default(), RuntimeKind::VmmTask));
    }

    // ── BootstrapPayload serialization ────────────────────────────────────────

    #[test]
    fn test_bootstrap_payload_serializes_to_json_line() {
        let mut env = HashMap::new();
        env.insert("MY_VAR".to_string(), "hello world".to_string());
        let payload = BootstrapPayload {
            message_type: "BOOTSTRAP",
            sandbox_id: "sb-001".to_string(),
            app: "/usr/bin/myapp".to_string(),
            args: vec!["--flag".to_string(), "arg with spaces".to_string()],
            env,
            hostname: Some("my-host".to_string()),
            dns_servers: vec!["8.8.8.8".to_string()],
            search_domains: vec!["example.com".to_string()],
            heartbeat_interval_ms: Some(5000),
            ready_check: None,
            ready_timeout_ms: None,
        };
        let line = payload.to_json_line().unwrap();
        let s = std::str::from_utf8(&line).unwrap();
        assert!(s.ends_with('\n'));
        let v: serde_json::Value = serde_json::from_str(s.trim_end()).unwrap();
        assert_eq!(v["type"], "BOOTSTRAP");
        assert_eq!(v["sandbox_id"], "sb-001");
        assert_eq!(v["app"], "/usr/bin/myapp");
        assert_eq!(v["args"][1], "arg with spaces");
        assert_eq!(v["env"]["MY_VAR"], "hello world");
    }

    #[test]
    fn test_bootstrap_payload_from_sandbox_data_uses_annotation() {
        use containerd_sandbox::cri::api::v1::PodSandboxConfig;
        let mut annotations = std::collections::HashMap::new();
        annotations.insert(
            ANNOTATION_APPLIANCE_APP.to_string(),
            "/usr/bin/annotated-app".to_string(),
        );
        annotations.insert(
            ANNOTATION_APPLIANCE_ARGS.to_string(),
            r#"["--port","8080","arg with spaces"]"#.to_string(),
        );
        annotations.insert(
            format!("{}MY_VAR", ANNOTATION_APPLIANCE_ENV_PREFIX),
            "value with spaces".to_string(),
        );
        annotations.insert(
            ANNOTATION_APPLIANCE_HEARTBEAT_MS.to_string(),
            "5000".to_string(),
        );
        let config = PodSandboxConfig {
            annotations,
            hostname: "my-vm".to_string(),
            ..Default::default()
        };
        let data = SandboxData {
            id: "sb-001".to_string(),
            config: Some(config),
            ..Default::default()
        };
        let payload = BootstrapPayload::from_sandbox_data("sb-001", &data, "/usr/bin/default");
        assert_eq!(payload.app, "/usr/bin/annotated-app");
        assert_eq!(payload.args, ["--port", "8080", "arg with spaces"]);
        assert_eq!(
            payload.env.get("MY_VAR").map(String::as_str),
            Some("value with spaces")
        );
        assert_eq!(payload.hostname.as_deref(), Some("my-vm"));
        assert_eq!(payload.heartbeat_interval_ms, Some(5000));
    }

    #[test]
    fn test_bootstrap_payload_from_sandbox_data_falls_back_to_default_app() {
        let data = SandboxData {
            id: "sb-002".to_string(),
            ..Default::default()
        };
        let payload = BootstrapPayload::from_sandbox_data("sb-002", &data, "/usr/bin/default-app");
        assert_eq!(payload.app, "/usr/bin/default-app");
        assert!(payload.args.is_empty());
        assert!(payload.env.is_empty());
    }

    // ── ApplianceRuntime construction ─────────────────────────────────────────

    #[test]
    fn test_appliance_runtime_new_stores_sandbox_id_and_default_app() {
        let rt = ApplianceRuntime::new("sb-001", "/usr/bin/default");
        assert_eq!(rt.sandbox_id, "sb-001");
        assert_eq!(rt.default_app, "/usr/bin/default");
    }

    #[test]
    fn test_appliance_start_sync_clock_is_noop() {
        let rt = ApplianceRuntime::new("sb-001", "");
        let exit = Arc::new(ExitSignal::default());
        rt.start_sync_clock("sb-001", exit);
    }

    #[tokio::test]
    async fn test_appliance_start_forward_events_warns_when_no_stream() {
        let rt = ApplianceRuntime::new("sb-001", "");
        let exit = Arc::new(ExitSignal::default());
        rt.start_forward_events(exit);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // ── wait_ready protocol ───────────────────────────────────────────────────

    /// Simulate the guest side: connect, read the BOOTSTRAP line, then send
    /// the provided message.
    async fn guest_exchange(socket_path: String, reply: String) {
        use tokio::{
            io::{AsyncBufReadExt, AsyncWriteExt},
            net::UnixStream,
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stream =
            tokio::time::timeout(Duration::from_secs(5), UnixStream::connect(&socket_path))
                .await
                .expect("guest_exchange: connect timed out")
                .expect("guest_exchange: connect failed");

        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut buf_reader = tokio::io::BufReader::new(read_half);

        // Read and discard the BOOTSTRAP message the host sends first.
        let mut bootstrap_line = String::new();
        buf_reader
            .read_line(&mut bootstrap_line)
            .await
            .expect("guest_exchange: read BOOTSTRAP failed");
        assert!(
            bootstrap_line.contains("sandbox_id") || bootstrap_line.contains("app"),
            "host must send a BOOTSTRAP JSON line, got: {:?}",
            bootstrap_line
        );

        // Send the reply (READY / FATAL / etc.).
        let line = format!("{}\n", reply);
        write_half
            .write_all(line.as_bytes())
            .await
            .expect("guest_exchange: write reply failed");
    }

    /// Happy path: guest reads BOOTSTRAP then sends READY → `wait_ready` returns Ok.
    #[tokio::test]
    async fn test_wait_ready_succeeds_on_valid_ready_message() {
        let socket_path = unique_socket_path();
        let sandbox_id = "sb-appliance-001";

        let path_clone = socket_path.clone();
        let id_clone = sandbox_id.to_string();
        tokio::spawn(async move {
            let msg = format!(
                r#"{{"type":"READY","sandbox_id":"{}","version":"1","init":"kuasar-init"}}"#,
                id_clone
            );
            guest_exchange(path_clone, msg).await;
        });

        let mut runtime = ApplianceRuntime::new(sandbox_id, "/bin/app");
        let data = SandboxData {
            id: sandbox_id.to_string(),
            ..SandboxData::default()
        };
        let address = format!("unix://{}", socket_path);
        runtime
            .wait_ready(&address, None, &data)
            .await
            .expect("wait_ready should succeed");

        let inner = runtime.inner.lock().await;
        assert!(inner.reader.is_some(), "reader must be stored after READY");
        assert!(inner.writer.is_some(), "writer must be stored after READY");

        let _ = std::fs::remove_file(&socket_path);
    }

    /// BOOTSTRAP contains args with spaces and env vars — verify the JSON line.
    #[tokio::test]
    async fn test_wait_ready_bootstrap_contains_args_and_env() {
        use tokio::net::UnixStream;

        let socket_path = unique_socket_path();
        let sandbox_id = "sb-appliance-bootstrap";

        // Guest task: connect, capture the BOOTSTRAP line, send READY.
        let path_clone = socket_path.clone();
        let (tx, rx) = tokio::sync::oneshot::channel::<String>();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
            tokio::time::sleep(Duration::from_millis(50)).await;
            let stream = UnixStream::connect(&path_clone).await.unwrap();
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut reader = tokio::io::BufReader::new(read_half);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let _ = tx.send(line.clone());

            let ready = format!(
                "{}\n",
                r#"{"type":"READY","sandbox_id":"sb-appliance-bootstrap","version":"1","init":"kuasar-init"}"#
            );
            write_half.write_all(ready.as_bytes()).await.unwrap();
        });

        // Build SandboxData with annotations for args and env.
        use containerd_sandbox::cri::api::v1::PodSandboxConfig;
        let mut annotations = std::collections::HashMap::new();
        annotations.insert(
            ANNOTATION_APPLIANCE_APP.to_string(),
            "/usr/bin/myapp".to_string(),
        );
        annotations.insert(
            ANNOTATION_APPLIANCE_ARGS.to_string(),
            r#"["--port","8080","arg with spaces"]"#.to_string(),
        );
        annotations.insert(
            format!("{}MY_SECRET", ANNOTATION_APPLIANCE_ENV_PREFIX),
            "value with = and spaces".to_string(),
        );
        let config = PodSandboxConfig {
            annotations,
            ..Default::default()
        };
        let data = SandboxData {
            id: sandbox_id.to_string(),
            config: Some(config),
            ..Default::default()
        };

        let mut runtime = ApplianceRuntime::new(sandbox_id, "");
        let address = format!("unix://{}", socket_path);
        runtime.wait_ready(&address, None, &data).await.unwrap();

        let bootstrap_line = tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .unwrap()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(bootstrap_line.trim_end()).unwrap();
        assert_eq!(v["sandbox_id"], "sb-appliance-bootstrap");
        assert_eq!(v["app"], "/usr/bin/myapp");
        assert_eq!(v["args"][2], "arg with spaces");
        assert_eq!(v["env"]["MY_SECRET"], "value with = and spaces");

        let _ = std::fs::remove_file(&socket_path);
    }

    /// Mismatched sandbox_id in READY → must fail.
    #[tokio::test]
    async fn test_wait_ready_fails_on_mismatched_sandbox_id() {
        let socket_path = unique_socket_path();

        let path_clone = socket_path.clone();
        tokio::spawn(async move {
            let msg =
                r#"{"type":"READY","sandbox_id":"wrong-id","version":"1","init":"kuasar-init"}"#
                    .to_string();
            guest_exchange(path_clone, msg).await;
        });

        let mut runtime = ApplianceRuntime::new("sb-expected", "");
        let data = SandboxData {
            id: "sb-expected".to_string(),
            ..SandboxData::default()
        };
        let result = runtime
            .wait_ready(&format!("unix://{}", socket_path), None, &data)
            .await;
        assert!(result.is_err(), "must fail on sandbox_id mismatch");

        let _ = std::fs::remove_file(&socket_path);
    }

    /// Guest sends HEARTBEAT instead of READY → must fail.
    #[tokio::test]
    async fn test_wait_ready_fails_on_wrong_message_type() {
        let socket_path = unique_socket_path();
        let sandbox_id = "sb-appliance-002";

        let path_clone = socket_path.clone();
        let id_clone = sandbox_id.to_string();
        tokio::spawn(async move {
            let msg = format!(r#"{{"type":"HEARTBEAT","sandbox_id":"{}"}}"#, id_clone);
            guest_exchange(path_clone, msg).await;
        });

        let mut runtime = ApplianceRuntime::new(sandbox_id, "");
        let data = SandboxData {
            id: sandbox_id.to_string(),
            ..SandboxData::default()
        };
        let result = runtime
            .wait_ready(&format!("unix://{}", socket_path), None, &data)
            .await;
        assert!(result.is_err(), "must fail when first message is not READY");

        let _ = std::fs::remove_file(&socket_path);
    }

    /// Guest sends FATAL before READY → must return Err with the reason.
    #[tokio::test]
    async fn test_wait_ready_fails_on_fatal_message_and_propagates_reason() {
        let socket_path = unique_socket_path();
        let sandbox_id = "sb-appliance-003";

        let path_clone = socket_path.clone();
        let id_clone = sandbox_id.to_string();
        tokio::spawn(async move {
            let msg = format!(
                r#"{{"type":"FATAL","sandbox_id":"{}","exit_code":-1,"reason":"init.d hook failed: 10-env.sh (exit 1)"}}"#,
                id_clone
            );
            guest_exchange(path_clone, msg).await;
        });

        let mut runtime = ApplianceRuntime::new(sandbox_id, "");
        let data = SandboxData {
            id: sandbox_id.to_string(),
            ..SandboxData::default()
        };
        let result = runtime
            .wait_ready(&format!("unix://{}", socket_path), None, &data)
            .await;
        assert!(result.is_err(), "must return Err when guest sends FATAL");
        let err_str = format!("{}", result.unwrap_err());
        assert!(
            err_str.contains("FATAL") || err_str.contains("init.d hook failed"),
            "error should include the FATAL reason; got: {}",
            err_str
        );

        let _ = std::fs::remove_file(&socket_path);
    }

    /// Malformed JSON from guest → must return Err without panicking.
    #[tokio::test]
    async fn test_wait_ready_fails_on_invalid_json() {
        let socket_path = unique_socket_path();
        let sandbox_id = "sb-appliance-004";

        let path_clone = socket_path.clone();
        tokio::spawn(async move {
            guest_exchange(path_clone, "not-valid-json{{{{".to_string()).await;
        });

        let mut runtime = ApplianceRuntime::new(sandbox_id, "");
        let data = SandboxData {
            id: sandbox_id.to_string(),
            ..SandboxData::default()
        };
        let result = runtime
            .wait_ready(&format!("unix://{}", socket_path), None, &data)
            .await;
        assert!(result.is_err(), "must return Err on malformed JSON");

        let _ = std::fs::remove_file(&socket_path);
    }

    /// No guest connects within the timeout → must return Err.
    #[tokio::test]
    async fn test_wait_ready_times_out_when_no_guest_connects() {
        let socket_path = unique_socket_path();
        let sandbox_id = "sb-appliance-005";

        let mut runtime = ApplianceRuntime::new(sandbox_id, "");
        let data = SandboxData {
            id: sandbox_id.to_string(),
            ..SandboxData::default()
        };
        let address = format!("unix://{}", socket_path);

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            runtime.wait_ready(&address, None, &data),
        )
        .await;
        match result {
            Ok(Ok(())) => panic!("wait_ready must not return Ok when no guest connects"),
            Ok(Err(_)) => {}
            Err(_) => panic!("wait_ready must not block forever"),
        }

        let _ = std::fs::remove_file(&socket_path);
    }

    // ── start_forward_events ──────────────────────────────────────────────────

    #[tokio::test]
    async fn test_start_forward_events_signals_on_fatal() {
        use tokio::{io::AsyncWriteExt, net::UnixStream};

        let socket_path = unique_socket_path();
        let sandbox_id = "sb-appliance-fwd-001";

        let path_clone = socket_path.clone();
        let id_clone = sandbox_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let mut stream = UnixStream::connect(&path_clone).await.unwrap();

            // Read BOOTSTRAP first.
            let mut reader = tokio::io::BufReader::new(&mut stream);
            let mut bootstrap_line = String::new();
            tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut bootstrap_line)
                .await
                .unwrap();

            let ready = format!(
                "{}\n",
                r#"{"type":"READY","sandbox_id":"sb-appliance-fwd-001","version":"1","init":"kuasar-init"}"#
            );
            stream.write_all(ready.as_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            let fatal = format!(
                "{}\n",
                serde_json::json!({"type":"FATAL","sandbox_id":id_clone,"reason":"oom","exit_code":-1})
            );
            stream.write_all(fatal.as_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        let mut runtime = ApplianceRuntime::new(sandbox_id, "/bin/app");
        let data = SandboxData {
            id: sandbox_id.to_string(),
            ..SandboxData::default()
        };
        let address = format!("unix://{}", socket_path);
        runtime.wait_ready(&address, None, &data).await.unwrap();

        let exit = Arc::new(ExitSignal::default());
        runtime.start_forward_events(Arc::clone(&exit));

        tokio::time::timeout(Duration::from_secs(2), exit.wait())
            .await
            .expect("exit_signal must fire when guest sends FATAL");

        let _ = std::fs::remove_file(&socket_path);
    }

    #[tokio::test]
    async fn test_start_forward_events_signals_on_eof() {
        use tokio::{io::AsyncWriteExt, net::UnixStream};

        let socket_path = unique_socket_path();
        let sandbox_id = "sb-appliance-fwd-002";

        let path_clone = socket_path.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let mut stream = UnixStream::connect(&path_clone).await.unwrap();

            // Read BOOTSTRAP first.
            let mut reader = tokio::io::BufReader::new(&mut stream);
            let mut bootstrap_line = String::new();
            tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut bootstrap_line)
                .await
                .unwrap();

            let ready = format!(
                "{}\n",
                r#"{"type":"READY","sandbox_id":"sb-appliance-fwd-002","version":"1","init":"kuasar-init"}"#
            );
            stream.write_all(ready.as_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            // Drop stream → EOF.
        });

        let mut runtime = ApplianceRuntime::new(sandbox_id, "/bin/app");
        let data = SandboxData {
            id: sandbox_id.to_string(),
            ..SandboxData::default()
        };
        runtime
            .wait_ready(&format!("unix://{}", socket_path), None, &data)
            .await
            .unwrap();

        let exit = Arc::new(ExitSignal::default());
        runtime.start_forward_events(Arc::clone(&exit));

        tokio::time::timeout(Duration::from_secs(2), exit.wait())
            .await
            .expect("exit_signal must fire on EOF");

        let _ = std::fs::remove_file(&socket_path);
    }

    // ── shutdown ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_shutdown_writes_shutdown_message() {
        use tokio::{io::AsyncWriteExt, net::UnixStream};

        let socket_path = unique_socket_path();
        let sandbox_id = "sb-appliance-shutdown-001";

        let path_clone = socket_path.clone();
        let (tx, rx) = tokio::sync::oneshot::channel::<String>();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
            tokio::time::sleep(Duration::from_millis(50)).await;
            let stream = UnixStream::connect(&path_clone).await.unwrap();
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut reader = tokio::io::BufReader::new(read_half);

            // Read BOOTSTRAP.
            let mut bootstrap_line = String::new();
            reader.read_line(&mut bootstrap_line).await.unwrap();

            let ready = format!(
                "{}\n",
                r#"{"type":"READY","sandbox_id":"sb-appliance-shutdown-001","version":"1","init":"kuasar-init"}"#
            );
            write_half.write_all(ready.as_bytes()).await.unwrap();

            // Now read the SHUTDOWN the host sends back.
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let _ = tx.send(line);
        });

        let mut runtime = ApplianceRuntime::new(sandbox_id, "/bin/app");
        let data = SandboxData {
            id: sandbox_id.to_string(),
            ..SandboxData::default()
        };
        runtime
            .wait_ready(&format!("unix://{}", socket_path), None, &data)
            .await
            .unwrap();
        runtime.shutdown(5000).await.unwrap();

        let received = tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .unwrap()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(received.trim_end()).unwrap();
        assert_eq!(v["type"], "SHUTDOWN");
        assert_eq!(v["deadline_ms"], 5000);

        let _ = std::fs::remove_file(&socket_path);
    }

    #[tokio::test]
    async fn test_shutdown_is_noop_without_prior_wait_ready() {
        let mut runtime = ApplianceRuntime::new("sb-001", "");
        runtime.shutdown(5000).await.expect("must be Ok");
    }

    // ── reconnect ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_reconnect_returns_ok_immediately_without_any_socket_activity() {
        let mut runtime = ApplianceRuntime::new("sb-001", "");
        let result = tokio::time::timeout(
            Duration::from_millis(200),
            runtime.reconnect("unix:///tmp/kuasar-nonexistent.sock"),
        )
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => panic!("reconnect must return Ok(()); got Err: {}", e),
            Err(_) => panic!("reconnect must return immediately"),
        }
    }
}
