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

//! ApplianceRuntime: host-side vsock server waiting for a READY message from the guest.

use std::sync::Arc;

use async_trait::async_trait;
use containerd_sandbox::{data::SandboxData, error::Result, signal::ExitSignal};

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

/// Host-side vsock server that waits for the guest to send a READY message.
///
/// The in-guest `kuasar-init` (or the application itself) is the **client**:
/// it connects to this host-side server and sends a single JSON-Lines message:
///
/// ```json
/// {"type":"READY","sandbox_id":"<id>"}
/// ```
///
/// This runtime binds a Unix/vsock socket at `address`, accepts one
/// connection, reads the READY line, validates `sandbox_id`, and returns
/// `Ok(())`.  The connection direction is the reverse of `VmmTaskRuntime`,
/// where the host is the client and `vmm-task` is the server.
///
/// After the initial handshake `kuasar-init` keeps the vsock connection open
/// and sends periodic `HEARTBEAT` messages and, on failure, `FATAL` messages.
/// Processing those post-READY messages is the responsibility of a separate
/// global vsock listener component and is outside the scope of this trait.
///
/// `start_sync_clock` and `start_forward_events` are **no-ops**: there is no
/// clock-sync RPC and no vmm-task event stream.
pub struct ApplianceRuntime {
    /// The sandbox ID expected in the guest's `READY` message (`data.id`).
    pub(crate) sandbox_id: String,
}

impl ApplianceRuntime {
    /// Create a new runtime that will accept a `READY` message
    /// whose `sandbox_id` field equals `sandbox_id`.
    pub fn new(sandbox_id: impl Into<String>) -> Self {
        Self {
            sandbox_id: sandbox_id.into(),
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

        // Only Unix-domain sockets are supported for now (tests and local CI).
        // Real vsock (AF_VSOCK) support will be added in a follow-up.
        let socket_path = address.strip_prefix("unix://").ok_or_else(|| {
            Error::Other(anyhow!(
                "ApplianceRuntime: unsupported address scheme: {}",
                address
            ))
        })?;

        // Remove stale socket file so bind succeeds on restart.
        let _ = std::fs::remove_file(socket_path);

        let listener = UnixListener::bind(socket_path)
            .map_err(|e| Error::Other(anyhow!("ApplianceRuntime: bind {}: {}", socket_path, e)))?;

        // Wait for the guest to connect within the timeout window.
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

        // Read exactly one newline-terminated line.
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .map_err(|e| Error::Other(anyhow!("ApplianceRuntime: read from guest: {}", e)))?;

        // Parse and validate the JSON message.
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
        // Reconnect is a no-op here.
        // After a host-side process restart the VM is still running; its
        // liveness is confirmed by the PID signal-0 check in
        // `CloudHypervisorVM::recover()`.  No re-handshake is needed.
        Ok(())
    }

    fn start_sync_clock(&self, _id: &str, _exit_signal: Arc<ExitSignal>) {
        // No-op: the guest VM maintains its own time via the hardware RTC.
    }

    fn start_forward_events(&self, _exit_signal: Arc<ExitSignal>) {
        // No-op: post-READY health signals (HEARTBEAT, FATAL) from kuasar-init
        // are handled by the global vsock listener, a separate component.
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
            .subsec_nanos();
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

    /// `start_forward_events` is a pure no-op: must not panic, spawn tasks, or
    /// interact with any I/O resource.
    #[test]
    fn test_appliance_start_forward_events_is_noop() {
        let rt = ApplianceRuntime::new("sb-001");
        let exit = Arc::new(ExitSignal::default());
        rt.start_forward_events(exit); // must not panic
    }

    // ── wait_ready protocol ───────────────────────────────────────────────────
    //
    // The "guest simulator" helper below connects to a Unix socket and writes
    // a single JSON-Lines message (terminated with `\n`), mimicking what
    // `kuasar-init` does over vsock.  It sleeps 50 ms first so the runtime has
    // time to call `bind()` before the connection arrives.

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

    /// Happy path: guest sends a well-formed READY message with the correct
    /// `sandbox_id`; `wait_ready` must return `Ok(())`.
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

        let _ = std::fs::remove_file(&socket_path);
    }

    /// The guest sends a READY message but with a `sandbox_id` that does not
    /// match the one the runtime was constructed with; `wait_ready` must return
    /// `Err` to prevent cross-sandbox routing bugs.
    #[tokio::test]
    async fn test_wait_ready_fails_on_mismatched_sandbox_id() {
        let socket_path = unique_socket_path();

        let path_clone = socket_path.clone();
        tokio::spawn(async move {
            // The runtime expects "sb-expected", but the guest announces "wrong-id".
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

    /// The guest sends a `HEARTBEAT` message instead of `READY`; `wait_ready`
    /// must return `Err` because the very first message on a new vsock connection
    /// must be `READY`.
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

    /// The guest sends a `FATAL` message (application failed to start during
    /// `init.d` hook processing); `wait_ready` must return `Err`, and the error
    /// message must carry the `reason` field so callers can diagnose the failure
    /// without having to read VM serial logs.
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
            "error should include the FATAL reason for observability; got: {}",
            err_str
        );

        let _ = std::fs::remove_file(&socket_path);
    }

    /// The guest sends malformed JSON; `wait_ready` must return `Err` rather
    /// than panicking or hanging indefinitely.
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

    /// No guest connects within the timeout window; `wait_ready` must return
    /// `Err` rather than blocking forever.  The call must complete within 2 s
    /// (the implementation's own internal timeout should be shorter than this).
    #[tokio::test]
    async fn test_wait_ready_times_out_when_no_guest_connects() {
        let socket_path = unique_socket_path();
        let sandbox_id = "sb-appliance-005";

        // No guest simulator — nobody will connect to the socket.
        let mut runtime = ApplianceRuntime::new(sandbox_id);
        let data = SandboxData {
            id: sandbox_id.to_string(),
            ..SandboxData::default()
        };
        let address = format!("unix://{}", socket_path);

        // The outer 2-second timeout distinguishes "runtime timed out correctly"
        // from "runtime hung indefinitely".
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            runtime.wait_ready(&address, None, &data),
        )
        .await;
        match result {
            Ok(Ok(())) => panic!("wait_ready must not return Ok when no guest connects"),
            Ok(Err(_)) => {} // Expected: runtime's own timeout fired.
            Err(_) => panic!("wait_ready must not block forever: did not return within 2 s"),
        }

        let _ = std::fs::remove_file(&socket_path);
    }

    // ── reconnect ─────────────────────────────────────────────────────────────

    /// `reconnect` must return `Ok(())` immediately without any socket activity.
    ///
    /// Rationale: after a host-side process restart the guest VM is still
    /// running.  Its liveness is confirmed separately by
    /// `CloudHypervisorVM::recover()` (PID signal-0 check).  Re-running the
    /// READY handshake would require kuasar-init to detect the disconnect and
    /// re-send READY, which is not guaranteed.  `reconnect` is therefore a
    /// no-op for this runtime.
    #[tokio::test]
    async fn test_reconnect_returns_ok_immediately_without_any_socket_activity() {
        let mut runtime = ApplianceRuntime::new("sb-001");
        // Non-existent socket path: if reconnect tries to connect it will fail.
        let address = "unix:///tmp/kuasar-nonexistent-reconnect-check.sock";

        let result =
            tokio::time::timeout(Duration::from_millis(200), runtime.reconnect(address)).await;
        match result {
            Ok(Ok(())) => {} // Expected.
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
