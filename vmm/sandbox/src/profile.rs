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

/// Pod annotation key that specifies the erofs VM disk image path for an Appliance-mode sandbox.
///
/// **Workaround**: set this to the absolute local path of the pre-staged erofs image.
/// Future versions will resolve this automatically from the containerd snapshot store
/// via `ContainerData.rootfs` when `CreateContainer` is called.
///
/// ```yaml
/// annotations:
///   io.kuasar.appliance.image: /var/lib/kuasar/appliance/myapp.erofs
/// ```
pub const ANNOTATION_APPLIANCE_IMAGE: &str = "io.kuasar.appliance.image";

/// Pod annotation key that specifies the application executable path for an Appliance-mode sandbox.
///
/// Delivered to the guest via the `BOOTSTRAP` vsock message (not via kernel cmdline),
/// so paths with spaces are supported.  Falls back to `appliance_app` in the
/// hypervisor TOML config when absent.
///
/// ```yaml
/// annotations:
///   io.kuasar.appliance.app: /usr/bin/myapp
/// ```
pub const ANNOTATION_APPLIANCE_APP: &str = "io.kuasar.appliance.app";

/// Pod annotation key for additional command-line arguments of the workload.
///
/// Value must be a JSON array of strings.  Arguments may contain spaces and
/// special characters because they are delivered via BOOTSTRAP, not cmdline.
///
/// ```yaml
/// annotations:
///   io.kuasar.appliance.args: '["--port","8080","--name","my server"]'
/// ```
pub const ANNOTATION_APPLIANCE_ARGS: &str = "io.kuasar.appliance.args";

/// Annotation key prefix for workload environment variables.
///
/// Any annotation whose key starts with this prefix is passed as an environment
/// variable to the workload.  The key suffix becomes the variable name; the
/// annotation value becomes the variable value.  Values may contain any characters
/// (spaces, `=`, quotes, …) because they travel over vsock BOOTSTRAP, not cmdline.
///
/// ```yaml
/// annotations:
///   io.kuasar.appliance.env.MY_VAR: "value with spaces and = signs"
///   io.kuasar.appliance.env.SECRET_KEY: "s3cr3t!"
/// ```
pub const ANNOTATION_APPLIANCE_ENV_PREFIX: &str = "io.kuasar.appliance.env.";

/// Pod annotation key for the heartbeat interval (milliseconds).
///
/// Overrides the guest default of 10 000 ms.  Set to `"0"` to disable heartbeats.
pub const ANNOTATION_APPLIANCE_HEARTBEAT_MS: &str = "io.kuasar.appliance.heartbeat_ms";

/// Pod annotation key for the readiness probe specification.
///
/// Forwarded verbatim to `ReadinessCheck::from_cmdline` inside the guest.
pub const ANNOTATION_APPLIANCE_READY_CHECK: &str = "io.kuasar.appliance.ready_check";

/// Pod annotation key for the readiness probe timeout (milliseconds).
pub const ANNOTATION_APPLIANCE_READY_TIMEOUT_MS: &str = "io.kuasar.appliance.ready_timeout_ms";

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
    ///
    /// `default_app` is the cluster-wide fallback executable (from the
    /// `appliance_app` TOML field in `[hypervisor]`).  It is used when the
    /// pod annotation `io.kuasar.appliance.app` is absent.  Pass `""` on the
    /// recovery path (reconnect is a no-op for Appliance mode).
    pub fn create_runtime(&self, sandbox_id: &str, default_app: &str) -> Box<dyn GuestRuntime> {
        self.runtime_kind().create_runtime(sandbox_id, default_app)
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
