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

use std::sync::Arc;

use async_trait::async_trait;
use containerd_sandbox::{data::SandboxData, error::Result, signal::ExitSignal};
use tokio::sync::Mutex;

use super::GuestRuntime;
use crate::network::Network;

/// How long [`ApplianceRuntime::wait_ready`] waits for the guest to connect and
/// send the READY message before returning `Err`.
///
/// The test value (1 s) must be shorter than the `tokio::time::timeout(2 s, …)`
/// wrapper used by the timeout test; the production value allows the VM to
/// finish booting.
#[cfg(test)]
const APPLIANCE_READY_TIMEOUT_SECS: u64 = 1;
#[cfg(not(test))]
const APPLIANCE_READY_TIMEOUT_SECS: u64 = 30;

/// Live connection state kept after the initial READY handshake.
struct RuntimeInner {
    reader: Option<tokio::io::BufReader<tokio::io::ReadHalf<tokio::net::UnixStream>>>,
    writer: Option<tokio::io::WriteHalf<tokio::net::UnixStream>>,
}

/// Host-side vsock server that waits for the guest to send a READY message and
/// then keeps the connection alive for HEARTBEAT/FATAL monitoring and SHUTDOWN
/// delivery.
///
/// ## Connection lifecycle
///
/// 1. [`wait_ready`] binds a Unix socket at `address`, accepts the guest
///    connection, reads and validates the `READY` message, then **retains** both
///    halves of the split stream in `inner`.
/// 2. [`start_forward_events`] takes ownership of the read half and spawns a
///    background task that logs `HEARTBEAT` messages and calls
///    `exit_signal.signal()` on `FATAL` or EOF.
/// 3. [`shutdown`] writes a `{"type":"SHUTDOWN","deadline_ms":N}` line over the
///    write half so `kuasar-init` can gracefully stop the workload.
///
/// ## Heartbeat opt-out
///
/// Function workloads can set `KUASAR_HEARTBEAT_INTERVAL_MS=0` in the kernel
/// cmdline to disable periodic heartbeats.  The persistent connection is still
/// maintained; `FATAL` and `SHUTDOWN` continue to work.
///
/// [`wait_ready`]: ApplianceRuntime::wait_ready
/// [`start_forward_events`]: ApplianceRuntime::start_forward_events
/// [`shutdown`]: ApplianceRuntime::shutdown
pub struct ApplianceRuntime {
    /// The sandbox ID expected in the guest's `READY` message.
    pub(crate) sandbox_id: String,
    inner: Arc<Mutex<RuntimeInner>>,
}

impl ApplianceRuntime {
    pub fn new(sandbox_id: impl Into<String>) -> Self {
        Self {
            sandbox_id: sandbox_id.into(),
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
        _data: &SandboxData,
    ) -> Result<()> {
        use anyhow::anyhow;
        use containerd_sandbox::error::Error;
        use tokio::{
            io::{AsyncBufReadExt, BufReader},
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
                "ApplianceRuntime: timed out waiting for guest READY on {}",
                socket_path
            ))
        })?
        .map_err(|e| Error::Other(anyhow!("ApplianceRuntime: accept: {}", e)))?;

        // Split before reading so the write half can be retained for SHUTDOWN.
        let (read_half, write_half) = tokio::io::split(stream);
        let mut reader = BufReader::new(read_half);

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
                "ApplianceRuntime: expected READY as first message, got {:?}",
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
    ///
    /// Must be called after [`wait_ready`] has stored the read half.  If called
    /// without a prior successful `wait_ready` this logs a warning and returns
    /// without spawning a task.
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
                // This path is hit on the recovery route: reconnect() is a no-op
                // for ApplianceRuntime so no reader is established after a host
                // process restart.  Log at error level so operators are not
                // silently unaware that guest events are not monitored.
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
    ///
    /// If `wait_ready` has not yet been called (writer is `None`) the call is
    /// silently ignored; the sandbox will still be torn down by the hypervisor.
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

    /// Adding the `Appliance` variant must not change the default.
    #[test]
    fn test_default_runtime_kind_is_still_vmm_task() {
        assert!(matches!(RuntimeKind::default(), RuntimeKind::VmmTask));
    }

    // ── ApplianceRuntime construction & no-op methods ─────────────────────────

    #[test]
    fn test_appliance_runtime_new_stores_sandbox_id() {
        let rt = ApplianceRuntime::new("sb-001");
        assert_eq!(rt.sandbox_id, "sb-001");
    }

    /// `start_sync_clock` is a pure no-op: must not panic, spawn tasks, or
    /// interact with any I/O resource.
    #[test]
    fn test_appliance_start_sync_clock_is_noop() {
        let rt = ApplianceRuntime::new("sb-001");
        let exit = Arc::new(ExitSignal::default());
        rt.start_sync_clock("sb-001", exit); // must not panic
    }

    /// `start_forward_events` with no prior `wait_ready` logs a warning and
    /// returns without panicking.
    #[tokio::test]
    async fn test_appliance_start_forward_events_warns_when_no_stream() {
        let rt = ApplianceRuntime::new("sb-001");
        let exit = Arc::new(ExitSignal::default());
        rt.start_forward_events(exit);
        // Give the spawned task a tick to run and exit gracefully.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // ── wait_ready protocol ───────────────────────────────────────────────────

    async fn guest_connect_and_send(socket_path: String, message: String) {
        use tokio::{io::AsyncWriteExt, net::UnixStream};
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut stream =
            tokio::time::timeout(Duration::from_secs(5), UnixStream::connect(&socket_path))
                .await
                .expect("guest_connect_and_send: connect timed out")
                .expect("guest_connect_and_send: connect failed");
        let line = format!("{}\n", message);
        stream
            .write_all(line.as_bytes())
            .await
            .expect("guest_connect_and_send: write failed");
    }

    /// Happy path: guest sends a well-formed READY message; `wait_ready` returns
    /// `Ok(())` and stores both stream halves for later use.
    #[tokio::test]
    async fn test_wait_ready_succeeds_on_valid_ready_message() {
        let socket_path = unique_socket_path();
        let sandbox_id = "sb-appliance-001";

        let path_clone = socket_path.clone();
        let id_clone = sandbox_id.to_string();
        tokio::spawn(async move {
            let msg = format!(r#"{{"type":"READY","sandbox_id":"{}"}}"#, id_clone);
            guest_connect_and_send(path_clone, msg).await;
        });

        let mut runtime = ApplianceRuntime::new(sandbox_id);
        let data = SandboxData {
            id: sandbox_id.to_string(),
            ..SandboxData::default()
        };
        let address = format!("unix://{}", socket_path);
        runtime
            .wait_ready(&address, None, &data)
            .await
            .expect("wait_ready should succeed when guest sends a valid READY message");

        // Stream halves must be stored.
        let inner = runtime.inner.lock().await;
        assert!(
            inner.reader.is_some(),
            "reader half must be stored after READY"
        );
        assert!(
            inner.writer.is_some(),
            "writer half must be stored after READY"
        );

        let _ = std::fs::remove_file(&socket_path);
    }

    /// The guest sends a READY message with a mismatched `sandbox_id`; must fail.
    #[tokio::test]
    async fn test_wait_ready_fails_on_mismatched_sandbox_id() {
        let socket_path = unique_socket_path();

        let path_clone = socket_path.clone();
        tokio::spawn(async move {
            let msg = r#"{"type":"READY","sandbox_id":"wrong-id"}"#.to_string();
            guest_connect_and_send(path_clone, msg).await;
        });

        let mut runtime = ApplianceRuntime::new("sb-expected");
        let data = SandboxData {
            id: "sb-expected".to_string(),
            ..SandboxData::default()
        };
        let address = format!("unix://{}", socket_path);
        let result = runtime.wait_ready(&address, None, &data).await;
        assert!(
            result.is_err(),
            "wait_ready must return Err when the sandbox_id in READY does not match"
        );

        let _ = std::fs::remove_file(&socket_path);
    }

    /// The guest sends HEARTBEAT instead of READY; must fail.
    #[tokio::test]
    async fn test_wait_ready_fails_on_wrong_message_type() {
        let socket_path = unique_socket_path();
        let sandbox_id = "sb-appliance-002";

        let path_clone = socket_path.clone();
        let id_clone = sandbox_id.to_string();
        tokio::spawn(async move {
            let msg = format!(r#"{{"type":"HEARTBEAT","sandbox_id":"{}"}}"#, id_clone);
            guest_connect_and_send(path_clone, msg).await;
        });

        let mut runtime = ApplianceRuntime::new(sandbox_id);
        let data = SandboxData {
            id: sandbox_id.to_string(),
            ..SandboxData::default()
        };
        let address = format!("unix://{}", socket_path);
        let result = runtime.wait_ready(&address, None, &data).await;
        assert!(
            result.is_err(),
            "wait_ready must return Err when the first message type is not READY"
        );

        let _ = std::fs::remove_file(&socket_path);
    }

    /// The guest sends FATAL before READY; must return `Err` with the reason.
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
            guest_connect_and_send(path_clone, msg).await;
        });

        let mut runtime = ApplianceRuntime::new(sandbox_id);
        let data = SandboxData {
            id: sandbox_id.to_string(),
            ..SandboxData::default()
        };
        let address = format!("unix://{}", socket_path);
        let result = runtime.wait_ready(&address, None, &data).await;
        assert!(
            result.is_err(),
            "wait_ready must return Err when the guest sends FATAL"
        );
        let err_str = format!("{}", result.unwrap_err());
        assert!(
            err_str.contains("FATAL") || err_str.contains("init.d hook failed"),
            "error should include the FATAL reason; got: {}",
            err_str
        );

        let _ = std::fs::remove_file(&socket_path);
    }

    /// Malformed JSON must return `Err` rather than panicking.
    #[tokio::test]
    async fn test_wait_ready_fails_on_invalid_json() {
        let socket_path = unique_socket_path();
        let sandbox_id = "sb-appliance-004";

        let path_clone = socket_path.clone();
        tokio::spawn(async move {
            guest_connect_and_send(path_clone, "not-valid-json{{{{".to_string()).await;
        });

        let mut runtime = ApplianceRuntime::new(sandbox_id);
        let data = SandboxData {
            id: sandbox_id.to_string(),
            ..SandboxData::default()
        };
        let address = format!("unix://{}", socket_path);
        let result = runtime.wait_ready(&address, None, &data).await;
        assert!(
            result.is_err(),
            "wait_ready must return Err when the guest sends malformed JSON"
        );

        let _ = std::fs::remove_file(&socket_path);
    }

    /// No guest connects within the timeout window; must return `Err`.
    #[tokio::test]
    async fn test_wait_ready_times_out_when_no_guest_connects() {
        let socket_path = unique_socket_path();
        let sandbox_id = "sb-appliance-005";

        let mut runtime = ApplianceRuntime::new(sandbox_id);
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
            Err(_) => panic!("wait_ready must not block forever: did not return within 2 s"),
        }

        let _ = std::fs::remove_file(&socket_path);
    }

    // ── start_forward_events ──────────────────────────────────────────────────

    /// After a successful `wait_ready`, `start_forward_events` must take the
    /// reader half and forward a FATAL message to `exit_signal`.
    #[tokio::test]
    async fn test_start_forward_events_signals_on_fatal() {
        use tokio::{io::AsyncWriteExt, net::UnixStream};

        let socket_path = unique_socket_path();
        let sandbox_id = "sb-appliance-fwd-001";

        // Simulate guest: send READY then FATAL.
        let path_clone = socket_path.clone();
        let id_clone = sandbox_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let mut stream = UnixStream::connect(&path_clone).await.unwrap();
            let ready = format!(
                "{}\n",
                r#"{"type":"READY","sandbox_id":"sb-appliance-fwd-001"}"#
            );
            stream.write_all(ready.as_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            let fatal = format!(
                "{}\n",
                serde_json::json!({"type":"FATAL","sandbox_id":id_clone,"reason":"oom","exit_code":-1})
            );
            stream.write_all(fatal.as_bytes()).await.unwrap();
            // Keep stream open so EOF doesn't race the FATAL read.
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        let mut runtime = ApplianceRuntime::new(sandbox_id);
        let data = SandboxData {
            id: sandbox_id.to_string(),
            ..SandboxData::default()
        };
        let address = format!("unix://{}", socket_path);
        runtime.wait_ready(&address, None, &data).await.unwrap();

        let exit = Arc::new(ExitSignal::default());
        runtime.start_forward_events(Arc::clone(&exit));

        // ExitSignal should fire when FATAL is received.
        tokio::time::timeout(Duration::from_secs(2), exit.wait())
            .await
            .expect("exit_signal must fire when guest sends FATAL");

        let _ = std::fs::remove_file(&socket_path);
    }

    /// After a successful `wait_ready`, `start_forward_events` must signal on EOF.
    #[tokio::test]
    async fn test_start_forward_events_signals_on_eof() {
        use tokio::{io::AsyncWriteExt, net::UnixStream};

        let socket_path = unique_socket_path();
        let sandbox_id = "sb-appliance-fwd-002";

        let path_clone = socket_path.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let mut stream = UnixStream::connect(&path_clone).await.unwrap();
            let ready = format!(
                "{}\n",
                r#"{"type":"READY","sandbox_id":"sb-appliance-fwd-002"}"#
            );
            stream.write_all(ready.as_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            // Drop stream → EOF on host reader.
        });

        let mut runtime = ApplianceRuntime::new(sandbox_id);
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
            .expect("exit_signal must fire on EOF");

        let _ = std::fs::remove_file(&socket_path);
    }

    // ── shutdown ──────────────────────────────────────────────────────────────

    /// After `wait_ready`, `shutdown` must write a SHUTDOWN JSON line that the
    /// guest can parse.
    #[tokio::test]
    async fn test_shutdown_writes_shutdown_message() {
        use tokio::{io::AsyncWriteExt, net::UnixStream};

        let socket_path = unique_socket_path();
        let sandbox_id = "sb-appliance-shutdown-001";

        // Simulate guest: send READY then read the SHUTDOWN the host sends back.
        let path_clone = socket_path.clone();
        let (tx, rx) = tokio::sync::oneshot::channel::<String>();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let mut stream = UnixStream::connect(&path_clone).await.unwrap();
            let ready = format!(
                "{}\n",
                r#"{"type":"READY","sandbox_id":"sb-appliance-shutdown-001"}"#
            );
            stream.write_all(ready.as_bytes()).await.unwrap();

            use tokio::io::AsyncBufReadExt;
            let mut reader = tokio::io::BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let _ = tx.send(line);
        });

        let mut runtime = ApplianceRuntime::new(sandbox_id);
        let data = SandboxData {
            id: sandbox_id.to_string(),
            ..SandboxData::default()
        };
        let address = format!("unix://{}", socket_path);
        runtime.wait_ready(&address, None, &data).await.unwrap();

        runtime.shutdown(5000).await.unwrap();

        let received = tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .expect("guest task must receive SHUTDOWN within 2 s")
            .unwrap();

        let v: serde_json::Value = serde_json::from_str(received.trim_end()).unwrap();
        assert_eq!(v["type"], "SHUTDOWN");
        assert_eq!(v["deadline_ms"], 5000);

        let _ = std::fs::remove_file(&socket_path);
    }

    /// `shutdown` on a runtime that has no persistent stream (no prior
    /// `wait_ready`) must return `Ok(())` silently.
    #[tokio::test]
    async fn test_shutdown_is_noop_without_prior_wait_ready() {
        let mut runtime = ApplianceRuntime::new("sb-001");
        runtime
            .shutdown(5000)
            .await
            .expect("shutdown without stream must be Ok");
    }

    // ── reconnect ─────────────────────────────────────────────────────────────

    /// `reconnect` must return `Ok(())` immediately without any socket activity.
    #[tokio::test]
    async fn test_reconnect_returns_ok_immediately_without_any_socket_activity() {
        let mut runtime = ApplianceRuntime::new("sb-001");
        let address = "unix:///tmp/kuasar-nonexistent-reconnect-check.sock";

        let result =
            tokio::time::timeout(Duration::from_millis(200), runtime.reconnect(address)).await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => panic!(
                "ApplianceRuntime::reconnect must return Ok(()); got Err: {}",
                e
            ),
            Err(_) => panic!(
                "ApplianceRuntime::reconnect must return immediately; timed out after 200 ms"
            ),
        }
    }
}
