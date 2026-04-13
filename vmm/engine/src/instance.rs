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

use std::collections::HashMap;
use std::sync::Arc;

pub use containerd_sandbox::data::SandboxData;
pub use containerd_sandbox::signal::ExitSignal;
use containerd_sandbox::SandboxStatus;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
pub use vmm_common::cgroup::SandboxCgroup;
use vmm_guest_runtime::{NetworkInterface, Route};
use vmm_vm_trait::Vmm;

use vmm_vm_trait::SandboxCtx;

use crate::error::{Error, Result};

// ── Re-exports for K8sAdapter ─────────────────────────────────────────────────
pub use containerd_sandbox::data::{ContainerData, ProcessData};

// ── Network state ─────────────────────────────────────────────────────────────

/// Discovered network state for this sandbox.
/// Populated by `prepare_network()` and used to configure the guest via `SetupSandbox`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkState {
    pub interfaces: Vec<NetworkInterface>,
    pub routes: Vec<Route>,
    /// Names of tap devices created during network setup (for cleanup on stop).
    #[serde(default)]
    pub tap_names: Vec<String>,
    /// Physical NICs bound to VFIO during setup (for driver restore on stop).
    #[serde(default)]
    pub physical_nics: Vec<vmm_common::network::PhysicalNicState>,
}

// ── Storage types ─────────────────────────────────────────────────────────────

/// A storage share mounted into the VM (e.g. a bind-mount backed by the shared virtiofs,
/// or a hot-plugged block device).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageMount {
    pub id: String,
    /// All containers that reference this mount. The mount is physically unmounted
    /// (and the device hot-detached) only when this Vec is empty.
    pub ref_containers: Vec<String>,
    /// Original host source path (bind mount source or block device path).
    /// Used for dedup lookup in `attach_container_storages`.
    pub host_path: String,
    /// Bind-mount destination inside the shared virtiofs directory, if applicable.
    /// `Some(host_dest)` for VirtioFs mounts; `None` for block devices.
    /// This is the path that must be unmounted in `deference_container_storages`.
    pub mount_dest: Option<String>,
    /// Path inside the guest where vmm-task mounts this storage.
    pub guest_path: String,
    pub kind: StorageMountKind,
    /// If backed by a hot-plugged block device, holds its device_id for hot-detach.
    pub device_id: Option<String>,
    /// PCI bus address returned by the VMM when hot-plugging the block device.
    /// Populated for `StorageMountKind::Block`; vmm-task uses this to locate
    /// the guest kernel device node via its PCI address.
    #[serde(default)]
    pub bus_addr: Option<String>,
    /// Filesystem type of the block device, probed via `blkid` at attach time.
    /// Populated for `StorageMountKind::Block` and forwarded to vmm-task so it
    /// can mount the device with the correct filesystem type.
    #[serde(default)]
    pub fstype: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StorageMountKind {
    VirtioFs,
    Virtio9P,
    Block,
}

// ── Container state ───────────────────────────────────────────────────────────

/// Per-container tracking: metadata + which host IO devices were hot-plugged.
/// `io_devices` holds device_ids of hot-plugged CharDevice (IO pipes) and VirtioBlock
/// devices; these are hot-detached when the container is removed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerState {
    pub id: String,
    pub data: ContainerData,
    pub io_devices: Vec<String>,
    pub processes: Vec<ProcessState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessState {
    pub id: String,
    pub io_devices: Vec<String>,
    pub data: ProcessData,
}

// ── SandboxInstance ───────────────────────────────────────────────────────────

/// Per-sandbox state held in the engine.
#[derive(Serialize, Deserialize)]
#[serde(bound = "V: Serialize + for<'de2> Deserialize<'de2> + Default")]
pub struct SandboxInstance<V: Vmm> {
    pub id: String,
    /// VMM backend state. Missing in old state files written before this field
    /// was added; defaults to `V::default()` in that case, causing `recover()`
    /// to fail gracefully and mark the sandbox as Stopped.
    #[serde(default)]
    pub vmm: V,
    pub status: SandboxStatus,
    pub base_dir: String,

    /// Metadata forwarded from containerd (used by K8sAdapter for event publishing).
    pub data: SandboxData,

    /// Network namespace path (set from CreateSandboxRequest).
    pub netns: String,

    /// Discovered network state, populated during start_sandbox before boot.
    #[serde(default)]
    pub network: Option<NetworkState>,

    /// Storage mounts currently attached to this sandbox.
    pub storages: Vec<StorageMount>,

    /// Containers and their hot-plugged device IDs.
    pub containers: HashMap<String, ContainerState>,

    /// Sequential counter for unique device/storage ID generation.
    pub id_generator: u32,
    /// Separate counter for vsock port allocation (starts at 1025, above ttrpc port 1024).
    pub vsock_port_next: u32,

    /// Host-side cgroup set (not serialised; reconstructed on recovery).
    #[serde(skip, default)]
    pub cgroup: SandboxCgroup,

    /// Exit signal — fired when the VMM process exits unexpectedly.
    #[serde(skip, default)]
    pub exit_signal: Arc<ExitSignal>,
}

impl<V: Vmm> SandboxInstance<V> {
    /// Create a `SandboxCtx` view into this instance.
    /// The compiler can split borrows within a method body but not in an ad-hoc struct
    /// literal at the call site when accessed through a `MutexGuard`.
    pub fn make_ctx(&mut self) -> SandboxCtx<'_, V> {
        SandboxCtx {
            vmm: &mut self.vmm,
            data: &mut self.data,
            base_dir: &self.base_dir,
        }
    }
}

impl<V: Vmm + Serialize + DeserializeOwned + Default> SandboxInstance<V> {
    /// Persist sandbox state to `{base_dir}/sandbox.json`.
    pub async fn dump(&self) -> Result<()> {
        let path = format!("{}/sandbox.json", self.base_dir);
        let content = serde_json::to_string(self)
            .map_err(|e| Error::Other(anyhow::anyhow!("serialize sandbox: {}", e)))?;
        tokio::fs::write(&path, content)
            .await
            .map_err(|e| Error::Other(anyhow::anyhow!("write sandbox state: {}", e)))?;
        Ok(())
    }

    /// Load sandbox state from `{dir}/sandbox.json`.
    ///
    /// Tries the current `SandboxInstance` format first.  If that fails and the
    /// JSON has a `"vm"` key but no `"vmm"` key (legacy `KuasarSandbox` format),
    /// the migration path in `crate::legacy` is attempted instead.
    pub async fn load(dir: &std::path::Path) -> Result<Self> {
        let file = dir.join("sandbox.json");
        let content = tokio::fs::read_to_string(&file)
            .await
            .map_err(|e| Error::Other(anyhow::anyhow!("read sandbox state {:?}: {}", file, e)))?;

        // Fast path: current format.
        if let Ok(inst) = serde_json::from_str::<Self>(&content) {
            return Ok(inst);
        }

        // Detect legacy KuasarSandbox format (has "vm" but not "vmm").
        let raw: serde_json::Value = serde_json::from_str(&content)
            .map_err(|e| Error::Other(anyhow::anyhow!("deserialize sandbox: {}", e)))?;
        if raw.get("vm").is_some() && raw.get("vmm").is_none() {
            tracing::info!(
                "sandbox state {:?} is in legacy KuasarSandbox format; migrating",
                file
            );
            return crate::legacy::migrate::<V>(&raw)
                .map_err(|e| Error::Other(anyhow::anyhow!("legacy migrate: {}", e)));
        }

        // Re-run standard deserialization to surface the real error.
        serde_json::from_str::<Self>(&content)
            .map_err(|e| Error::Other(anyhow::anyhow!("deserialize sandbox: {}", e)))
    }
}

/// Compact summary returned by `SandboxEngine::list_sandboxes`.
pub struct SandboxSummary {
    pub id: String,
    pub status: SandboxStatus,
}
