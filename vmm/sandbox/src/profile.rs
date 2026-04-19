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

//! Sandbox profile: encapsulates mode-specific behaviour shared across all hypervisors.
//!
//! A [`SandboxProfile`] is read from the `[sandbox]` section of the TOML config
//! (field `profile`) and injected into both the [`VMFactory`] and [`Hooks`] for
//! each hypervisor.  Adding a new mode requires:
//!
//! 1. A new variant here (with its four behaviour methods).
//! 2. A new [`GuestRuntime`] implementation if the handshake protocol differs.
//!
//! No per-hypervisor factory or hooks files are needed.
//!
//! [`VMFactory`]: crate::vm::VMFactory
//! [`Hooks`]: crate::vm::Hooks
//! [`GuestRuntime`]: crate::guest_runtime::GuestRuntime

use serde::{Deserialize, Serialize};

use crate::guest_runtime::{GuestRuntime, RuntimeKind};

/// Selects the guest coordination protocol and device topology for a sandbox.
///
/// Persisted in the `[sandbox]` section of the hypervisor TOML config under
/// the key `profile`.  Defaults to [`Standard`][SandboxProfile::Standard] when
/// the field is absent, preserving backwards compatibility.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxProfile {
    /// Standard mode: `vmm-task` runs inside the VM and exposes a ttrpc
    /// endpoint over vsock; the host connects as a client.  Requires
    /// virtiofsd for shared filesystem access.
    #[default]
    Standard,

    /// Appliance mode: the guest application connects *back* to the host
    /// over vsock and sends a JSON READY message.  No `vmm-task`, no
    /// virtiofsd — the guest manages its own filesystem.
    Appliance,
}

impl SandboxProfile {
    /// The [`RuntimeKind`] tag to persist in `sandbox.json` so that the
    /// correct [`GuestRuntime`] can be reconstructed during recovery.
    pub fn runtime_kind(&self) -> RuntimeKind {
        match self {
            Self::Standard => RuntimeKind::VmmTask,
            Self::Appliance => RuntimeKind::Appliance,
        }
    }

    /// Construct the host-side [`GuestRuntime`] for this profile.
    pub fn create_runtime(&self, sandbox_id: &str) -> Box<dyn GuestRuntime> {
        self.runtime_kind().create_runtime(sandbox_id)
    }

    /// Whether the hypervisor should start a virtiofsd / virtiofs-daemon
    /// before booting the VM.
    pub fn needs_virtiofsd(&self) -> bool {
        match self {
            Self::Standard => true,
            Self::Appliance => false,
        }
    }

    /// The `task_address` value to store in [`SandboxData`] after the VM
    /// starts, or `None` if this profile does not use the containerd ttrpc
    /// task API.
    ///
    /// [`SandboxData`]: containerd_sandbox::data::SandboxData
    pub fn task_address(&self, agent_socket: &str) -> Option<String> {
        match self {
            Self::Standard => Some(format!("ttrpc+{}", agent_socket)),
            Self::Appliance => None,
        }
    }
}
