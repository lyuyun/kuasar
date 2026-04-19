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

//! Guest runtime abstraction layer.
//!
//! `GuestRuntime` decouples the host-side sandbox engine from the specific communication
//! protocol used to coordinate with the in-guest agent:
//!
//! - `VmmTaskRuntime`: connects to `vmm-task` via ttrpc over vsock, polls for readiness
//!   with `check()`, then calls `setup_sandbox()` to configure the guest network.

pub mod vmm_task;

use std::sync::Arc;

use async_trait::async_trait;
use containerd_sandbox::{data::SandboxData, error::Result, signal::ExitSignal};
use serde::{Deserialize, Serialize};
pub use vmm_task::VmmTaskRuntime;

use crate::network::Network;

/// Identifies the guest runtime variant persisted in `sandbox.json` so that the
/// correct [`GuestRuntime`] implementation can be reconstructed during recovery.
///
/// Adding a new variant here is the only change required to make recovery
/// runtime-aware; no ad-hoc hard-coding in `sandbox.rs` is needed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeKind {
    /// communicates with `vmm-task` via ttrpc over vsock.
    #[default]
    VmmTask,
}

impl RuntimeKind {
    /// Construct the [`GuestRuntime`] for this kind.
    ///
    /// Single source of truth for the `RuntimeKind → GuestRuntime` mapping, used
    /// by both the initial create path (via `SandboxProfile::create_runtime`) and
    /// the recovery path.
    pub fn create_runtime(&self, _sandbox_id: &str) -> Box<dyn GuestRuntime> {
        match self {
            Self::VmmTask => Box::new(VmmTaskRuntime::new()),
        }
    }
}

/// Abstraction over the host–guest coordination protocol.
///
/// Implementations are responsible for:
/// 1. Waiting until the guest agent reports ready.
/// 2. Providing periodic clock synchronisation with the guest.
/// 3. Forwarding in-guest events to containerd.
///
/// The trait is object-safe so that `KuasarSandbox` can hold a `Box<dyn GuestRuntime>`
/// without propagating a generic parameter through the entire type hierarchy.
#[async_trait]
pub trait GuestRuntime: Send + Sync {
    /// Block until the guest reports ready.
    ///
    /// Called on the **initial boot** path only. In addition to establishing the
    /// communication channel this also sends the guest its network configuration
    /// via `setup_sandbox()`.
    ///
    /// The `address` parameter is the value returned by `Vmm::vsock_path()`; implementations
    /// prepend the protocol-specific scheme themselves (e.g. `hvsock://…:1024`).
    async fn wait_ready(
        &mut self,
        address: &str,
        network: Option<&Network>,
        data: &SandboxData,
    ) -> Result<()>;

    /// Reconnect to an already-running guest agent after a host-side process restart.
    ///
    /// Unlike [`wait_ready`], this does **not** call `setup_sandbox`: the guest is already
    /// running with its network fully configured. Only the communication channel needs to
    /// be re-established and the agent's liveness confirmed.
    ///
    /// Called on the **recovery** path only.
    async fn reconnect(&mut self, address: &str) -> Result<()>;

    /// Start a background task that periodically synchronises the guest clock with the host.
    ///
    /// Returns immediately; the sync task runs until `exit_signal` fires.
    fn start_sync_clock(&self, id: &str, exit_signal: Arc<ExitSignal>);

    /// Start a background task that subscribes to vmm-task events and forwards them to
    /// containerd via the publisher socket.
    ///
    /// Returns immediately; the forwarding task runs until `exit_signal` fires.
    fn start_forward_events(&self, exit_signal: Arc<ExitSignal>);
}

#[cfg(test)]
mod tests {
    use super::RuntimeKind;

    #[test]
    fn test_runtime_kind_default_is_vmm_task() {
        assert!(matches!(RuntimeKind::default(), RuntimeKind::VmmTask));
    }

    #[test]
    fn test_runtime_kind_serializes_to_snake_case_json() {
        let json = serde_json::to_string(&RuntimeKind::VmmTask).unwrap();
        assert_eq!(json, r#""vmm_task""#);
    }

    #[test]
    fn test_runtime_kind_deserializes_from_json() {
        let kind: RuntimeKind = serde_json::from_str(r#""vmm_task""#).unwrap();
        assert!(matches!(kind, RuntimeKind::VmmTask));
    }

    /// Verifies backwards compatibility: a `sandbox.json` written before
    /// `runtime_kind` was added (field absent) must deserialise to
    /// `RuntimeKind::VmmTask` via `#[serde(default)]`.
    #[test]
    fn test_runtime_kind_defaults_when_field_absent_in_json() {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            #[serde(default)]
            runtime_kind: RuntimeKind,
        }
        let w: Wrapper = serde_json::from_str("{}").unwrap();
        assert!(matches!(w.runtime_kind, RuntimeKind::VmmTask));
    }
}
