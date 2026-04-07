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

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// Block device configuration passed before boot/restore.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskConfig {
    pub id: String,
    pub path: String,
    pub read_only: bool,
}

/// Network device configuration passed to `Vmm::add_network` before boot.
///
/// Mirrors the device types handled by the sandbox's `attach_to` method:
/// veth-based tap, vhost-user, and physical NIC passthrough via VFIO.
#[derive(Debug, Clone)]
pub enum VmmNetworkConfig {
    /// Virtio-net tap device created from a pod veth endpoint (or a pre-existing tap).
    ///
    /// `tap_device` is used as both the device id and the host tap interface name.
    Tap {
        tap_device: String,
        mac: String,
        queue: u32,
        netns: String,
    },
    /// VhostUser network device (e.g. SR-IOV virtio-net via socket).
    VhostUser {
        /// Device id, e.g. `"intf-3"`.
        id: String,
        /// Unix socket path of the vhost-user backend.
        socket_path: String,
        mac: String,
    },
    /// Physical NIC passed through to the guest via VFIO.
    Physical {
        /// Device id, e.g. `"intf-5"`.
        id: String,
        /// PCI BDF address, e.g. `"0000:00:1f.0"`.
        bdf: String,
    },
}

/// VMM process exit information.
#[derive(Debug, Clone)]
pub struct ExitInfo {
    pub pid: u32,
    pub exit_code: i32,
}

/// Per-VMM feature flags.
///
/// The engine and adapter query these before issuing operations that not all backends
/// support. For example, before hot-attaching container IO devices the adapter checks
/// `virtio_serial` and falls back to `VsockMuxIO` if false (Firecracker model).
#[derive(Debug, Clone, Default)]
pub struct VmmCapabilities {
    // Hot-plug device support
    pub hot_plug_disk: bool, // VirtioBlock hot-attach/detach (CH, QEMU, StratoVirt)
    pub hot_plug_net: bool,  // virtio-net hot-attach/detach
    // Reserved for future use — no Vmm::add_vcpu() method defined in Epic 1.
    pub hot_plug_cpu: bool,
    // Reserved for future use — no Vmm::resize_memory() method defined in Epic 1.
    pub hot_plug_mem: bool,
    // Reserved for future use — no Vmm trait method or HotPlugDevice variant exists yet.
    // Add HotPlugDevice::Pmem + Vmm::add_pmem() when implementing pmem support.
    pub pmem_dax: bool,
    // Reserved for future use — no Vmm trait method exists yet.
    // Add HotPlugDevice::Vfio when implementing VFIO passthrough.
    pub vfio: bool,
    // Reserved for future use — no Vmm::resize() method defined in Epic 1.
    pub resize: bool,

    // Filesystem sharing model
    /// Backend supports virtiofs (virtiofsd sidecar). True: CH, QEMU, StratoVirt.
    /// False: Firecracker (uses drive images or 9p; virtiofsd not supported).
    pub virtiofs: bool,

    // Container IO model
    /// Backend supports virtio-serial CharDevice for container stdin/stdout/stderr.
    /// True: CH, QEMU, StratoVirt.
    /// False: Firecracker — uses vsock port multiplexing instead (see VsockMuxIO).
    pub virtio_serial: bool,

    // Lifecycle capabilities
    /// Backend supports snapshot + restore (pause-to-disk / fast-resume).
    /// True: Firecracker. False: CH, QEMU, StratoVirt (not yet / partial).
    pub snapshot_restore: bool,
}

/// A device that can be hot-plugged into a running VM, or a logical IO channel
/// that the VMM exposes to the guest for container stdio.
///
/// Not all variants are supported by every backend — check `VmmCapabilities` first.
#[derive(Debug, Clone)]
pub enum HotPlugDevice {
    /// Virtio-block device backed by a host file or block device.
    /// Supported by: CH, QEMU, StratoVirt. Check `hot_plug_disk`.
    VirtioBlock {
        id: String,
        path: String,
        read_only: bool,
    },

    /// Virtiofs share backed by a running virtiofsd instance.
    /// Supported by: CH, QEMU, StratoVirt. Check `virtiofs`.
    VirtioFs {
        id: String,
        tag: String,
        socket: String,
    },

    /// Virtio-serial char device backed by a named pipe.
    /// Used for container stdin/stdout/stderr on backends that support virtio-serial.
    /// `chardev_id` is the backend identifier; `name` is the port name seen in the guest.
    /// Supported by: CH, QEMU, StratoVirt. Check `virtio_serial`.
    CharDevice {
        id: String,
        chardev_id: String,
        name: String,
        path: String,
    },

    /// Vsock-multiplexed IO channel — Firecracker's container IO model.
    /// Instead of a per-pipe CharDevice, a single vsock stream multiplexes
    /// stdin/stdout/stderr using `port` as the vsock port number.
    /// The guest-side agent identifies the container by `container_id`.
    /// Supported by: Firecracker. Check `!virtio_serial`.
    VsockMuxIO {
        id: String,
        container_id: String,
        port: u32,
    },
}

/// Result of a successful hot-plug operation.
#[derive(Debug, Clone)]
pub struct HotPlugResult {
    pub device_id: String,
    pub bus_addr: String, // PCI/MMIO address assigned by the VMM
}

/// vCPU thread IDs, used for placing vcpu threads in the right cgroup.
#[derive(Debug)]
pub struct VcpuThreads {
    pub vcpus: HashMap<i64, i64>, // vcpu_index → tid
}

/// PIDs associated with this VMM instance.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Pids {
    pub vmm_pid: Option<u32>,
    pub affiliated_pids: Vec<u32>, // e.g. virtiofsd processes
}

/// VMM lifecycle abstraction — independent of runtime mode.
///
/// Each backend provides:
///   - An associated `Config` type (backend-specific TOML config struct).
///   - A static `create()` constructor — the single construction point, replacing
///     the closure-based `VmmFactory`. `SandboxEngine` holds `V::Config` and calls
///     `V::create(id, base_dir, config, vsock_cid)` to produce each new pre-boot instance.
///
/// Cross-cutting lifecycle customisation (resource config, task_address, graceful
/// stop, etc.) belongs in `Hooks<V>`, NOT in this trait. The `Vmm` trait only
/// contains VMM process management and device management.
///
/// Lifecycle sequence (engine drives, hooks customise):
///   V::create(id, base_dir, &config, vsock_cid) → construct pre-boot instance  [Vmm::create]
///   hooks.post_create(&mut ctx)                 → optional post-create setup   [Hooks]
///   vmm.add_disk / vmm.add_network              → attach static devices        [Vmm]
///   hooks.pre_start(&mut ctx)                   → apply pod spec to VMM config [Hooks]
///   vmm.boot()                                  → start VMM process            [Vmm]
///   hooks.post_start(&mut ctx)                  → set task_address, etc.       [Hooks]
///   [ sandbox Running ]
///   hooks.pre_stop(&mut ctx)                    → graceful pre-stop            [Hooks]
///   vmm.stop(force)                             → stop VMM process             [Vmm]
///   hooks.post_stop(&mut ctx)                   → cleanup                      [Hooks]
#[async_trait]
pub trait Vmm: Send + Sync + 'static {
    /// Backend-specific configuration type, loaded from TOML at startup.
    /// `SandboxEngine` holds one instance of this; it is cloned into each `create` call.
    type Config: Clone + Send + Sync + serde::de::DeserializeOwned;

    /// Construct a pre-boot VMM instance for the given sandbox.
    /// Derives all paths from `base_dir` (api_socket, vsock_path, etc.),
    /// stores a clone of `config`, and returns without starting any process.
    ///
    /// `vsock_cid` is a unique guest CID allocated by the engine from its
    /// `next_vsock_cid` counter (range 3..=u32::MAX; 0/1/2 are system-reserved).
    /// Backends that use numeric CIDs (QEMU, StratoVirt, Firecracker) store it;
    /// backends with file-based vsock (Cloud Hypervisor) may ignore it (`_vsock_cid`).
    async fn create(
        id: &str,
        base_dir: &str,
        config: &Self::Config,
        vsock_cid: u32,
    ) -> Result<Self>
    where
        Self: Sized;

    /// Cold-boot the VM.
    async fn boot(&mut self) -> Result<()>;

    /// Stop the VM. force=true sends SIGKILL; false sends SIGTERM then waits graceful_ms.
    async fn stop(&mut self, force: bool) -> Result<()>;

    /// Subscribe to VMM process exit events.
    /// Returns a watch receiver that yields `Some(ExitInfo)` once when the process exits.
    /// The receiver can be held outside the sandbox Mutex, enabling lock-free monitoring
    /// in a background task (avoiding the deadlock that `&mut self` would cause).
    fn subscribe_exit(&self) -> tokio::sync::watch::Receiver<Option<ExitInfo>>;

    /// Reconnect to the VMM API socket after process restart.
    /// Re-creates the API client and watch channel from the already-running process.
    /// Only called during recovery for sandboxes found in Running state.
    async fn recover(&mut self) -> Result<()>;

    /// Attach a block device. Must be called before boot().
    fn add_disk(&mut self, disk: DiskConfig) -> Result<()>;

    /// Attach a network device. Must be called before boot().
    fn add_network(&mut self, net: VmmNetworkConfig) -> Result<()>;

    /// Hot-plug a device into a running VM.
    async fn hot_attach(&mut self, device: HotPlugDevice) -> Result<HotPlugResult>;

    /// Hot-detach a previously hot-plugged device by its device_id.
    async fn hot_detach(&mut self, id: &str) -> Result<()>;

    /// Health-check the VMM — returns Ok if the VM process is alive and responsive.
    async fn ping(&self) -> Result<()>;

    /// Return the vCPU thread IDs. Used for placing vcpu threads in the cpu cgroup.
    async fn vcpus(&self) -> Result<VcpuThreads>;

    /// Return all PIDs associated with this VMM (vmm process + affiliated, e.g. virtiofsd).
    fn pids(&self) -> Pids;

    /// Return the host-side vsock/hvsock path for host-guest communication.
    fn vsock_path(&self) -> Result<String>;

    /// Return the task_address string to be stored in SandboxData after boot.
    /// Containerd uses this to locate the Task API service for this sandbox.
    /// Default: "ttrpc+<vsock_path>". Override for backends with a different
    /// addressing scheme (e.g. Firecracker's numeric vsock CID).
    fn task_address(&self) -> String {
        self.vsock_path()
            .map(|p| format!("ttrpc+{}", p))
            .unwrap_or_default()
    }

    /// Query VMM capabilities. The engine and adapter check these before calling
    /// operations that not all backends support (hot-plug, virtiofs, virtio-serial).
    fn capabilities(&self) -> VmmCapabilities;

    /// Reconstruct a Vmm instance from a legacy `KuasarSandbox` "vm" field JSON.
    ///
    /// Called during recovery when the engine finds a `sandbox.json` written by the
    /// old `vmm-sandboxer` (KuasarSandboxer) in the same working directory.
    /// `id` and `base_dir` are from the legacy JSON's top-level fields.
    ///
    /// The default returns `Err` (migration not supported). Backends override this
    /// to enable transparent in-place migration from the old sandboxer.
    fn from_legacy_vm(
        _vm_json: JsonValue,
        _id: &str,
        _base_dir: &str,
    ) -> anyhow::Result<Self>
    where
        Self: Sized,
    {
        Err(anyhow::anyhow!(
            "legacy KuasarSandbox migration not supported for this VMM backend"
        ))
    }
}
