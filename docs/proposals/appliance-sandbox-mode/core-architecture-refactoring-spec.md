# Epic 1: Core Architecture Refactoring — Implementation Spec

## Scope

Establish the three-layer skeleton (Infrastructure → Engine → Adapter) with no behavior change to existing standard-mode users. All existing tests in `vmm/sandbox` must continue to pass after this epic.

**Stories**: 1.1 vm-trait · 1.2 guest-runtime · 1.3 engine · 1.4 adapter-k8s · 1.5 runtime-vmm-task · 1.6 startup wiring

---

## New Crate Layout

```
vmm/
├── vm-trait/               # Story 1.1 — Vmm trait + Hooks<V> trait + SandboxCtx<V>
│   └── src/
│       ├── lib.rs
│       ├── vmm.rs          # Vmm trait + VmmCapabilities + HotPlugDevice
│       └── hooks.rs        # Hooks<V> trait + SandboxCtx<V> + NoopHooks<V>
├── guest-runtime/          # Story 1.2 — GuestReadiness + ContainerRuntime traits
│   └── src/lib.rs
├── engine/                 # Story 1.3 — SandboxEngine + state machine
│   └── src/
│       ├── lib.rs
│       ├── engine.rs       # SandboxEngine<V, R, H>
│       ├── instance.rs     # SandboxInstance<V>
│       ├── state.rs        # SandboxState enum + transitions
│       └── config.rs       # EngineConfig
├── adapter-k8s/            # Story 1.4 — K8s Adapter (shimv2 + Task API)
│   └── src/
│       ├── lib.rs
│       ├── sandboxer.rs    # impl Sandboxer for K8sAdapter<V, R, H>
│       └── task.rs         # impl TaskService for K8sAdapter<V, R, H>
├── runtime-vmm-task/       # Story 1.5 — VmmTaskRuntime (ttrpc → vmm-task)
│   └── src/lib.rs
├── cloud-hypervisor/       # NEW: CloudHypervisorVmm impl Vmm
│   └── src/
│       ├── lib.rs
│       ├── vmm.rs          # impl Vmm for CloudHypervisorVmm
│       ├── hooks.rs        # CloudHypervisorHooks impl Hooks<CloudHypervisorVmm>
│       └── config.rs       # CloudHypervisorVmmConfig (= Vmm::Config)
├── qemu/                   # NEW: QemuVmm impl Vmm
│   └── src/
│       ├── lib.rs
│       ├── vmm.rs          # impl Vmm for QemuVmm
│       ├── hooks.rs        # QemuHooks impl Hooks<QemuVmm>
│       └── config.rs       # QemuVmmConfig (= Vmm::Config)
├── stratovirt/             # NEW: StratoVirtVmm impl Vmm
│   └── src/
│       ├── lib.rs
│       ├── vmm.rs          # impl Vmm for StratoVirtVmm
│       ├── hooks.rs        # StratoVirtHooks impl Hooks<StratoVirtVmm>
│       └── config.rs       # StratoVirtVmmConfig (= Vmm::Config)
├── sandbox/                # RETAINED AS-IS (migration shim; existing tests remain)
│   └── ...
└── common/                 # RETAINED AS-IS
```

```
cmd/
└── vmm-engine/             # Story 1.6 — unified VMM sandboxer entry point
    └── src/
        └── main.rs
```

> **Naming rationale**: `vmm-engine` reflects that this binary is the engine layer for VMM-based
> sandboxers. `SandboxEngine<V: Vmm, …>` is inherently VMM-specific — its abstractions (`boot`,
> `add_disk`, `vsock_path`, vmm-task ttrpc) have no meaning for runc or wasm sandboxers.
> A future runc/wasm refactoring would produce separate binaries (`runc-engine`, `wasm-engine`)
> with their own engine designs, not reuse this one.

### Cargo.toml additions (workspace)

```toml
members = [
    # existing — unchanged
    "vmm/task",
    "vmm/sandbox",
    "vmm/common",
    # Epic 1 — new crates
    "vmm/vm-trait",
    "vmm/guest-runtime",
    "vmm/engine",
    "vmm/adapter-k8s",
    "vmm/runtime-vmm-task",
    # VMM backend crates (one per hypervisor)
    "vmm/cloud-hypervisor",
    "vmm/qemu",
    "vmm/stratovirt",
    # Unified VMM engine binary
    "cmd/vmm-engine",
    # existing others — unchanged
    "runc",
    "quark",
    "wasm",
    "shim",
]
```

---

## Crate Dependency Graph

All arrows point in the direction of **depends on**. There are no cycles.

```
Layer 0 — shared foundation (existing, retained)
  vmm-common

Layer 1 — trait definitions
  vmm-vm-trait          → vmm-common

Layer 2 — implementations
  vmm-cloud-hypervisor  → vmm-vm-trait, vmm-common
  vmm-qemu              → vmm-vm-trait, vmm-common
  vmm-stratovirt        → vmm-vm-trait, vmm-common
  vmm-guest-runtime     → vmm-common

Layer 3 — engine
  vmm-engine            → vmm-vm-trait, vmm-guest-runtime, vmm-common, containerd-sandbox

Layer 4 — adapters & runtime bridge
  vmm-adapter-k8s       → vmm-engine, vmm-vm-trait, vmm-guest-runtime, vmm-common,
                          containerd-sandbox, containerd-shim
  vmm-runtime-vmm-task  → vmm-guest-runtime, vmm-common

Layer 5 — binary
  cmd/vmm-engine        → vmm-engine, vmm-adapter-k8s, vmm-runtime-vmm-task,
                          vmm-cloud-hypervisor, vmm-qemu, vmm-stratovirt,
                          vmm-vm-trait, vmm-common
```

The key constraint enforced by this layering:

- **Backend crates depend only on Layer 0–1**: `vmm-cloud-hypervisor`, `vmm-qemu`,
  `vmm-stratovirt` each depend on `vmm-vm-trait` (for `Vmm`, `Hooks<V>`, `SandboxCtx<V>`)
  and `vmm-common`. They do **not** depend on `vmm-engine` — this is why `Hooks<V>` and
  `SandboxCtx<V>` are defined in `vmm-vm-trait` rather than `vmm-engine`.
- **`vmm-engine` depends on no concrete backend**: it is fully generic over `V: Vmm`,
  `R: GuestReadiness`, `H: Hooks<V>`. Concrete types are only resolved at Layer 5.
- **`cmd/vmm-engine` is the sole wiring point**: it is the only crate that imports all
  backend crates simultaneously and monomorphises `SandboxEngine<V, R, H>`.

---

## Story 1.1 — `vm-trait` crate

**Crate name**: `vmm-vm-trait`
**Path**: `vmm/vm-trait/src/lib.rs`
**Dependencies**: `async-trait`, `serde`, `anyhow`, `tokio`, `vmm-common`

> `vmm-common` is required for `SandboxData` (used by `SandboxCtx<V>` in `hooks.rs`).
> All other types (`DiskConfig`, `VmmNetworkConfig`, `HotPlugDevice`, `VmmCapabilities`,
> `ExitInfo`, `VcpuThreads`, `Pids`) are defined locally in this crate.

### Supporting types

```rust
/// Block device configuration passed before boot/restore.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskConfig {
    pub id: String,
    pub path: String,
    pub read_only: bool,
}

/// Network device configuration passed to the VMM before boot.
/// Only the tap device name and queue count are VMM-level concerns; IP/routes are
/// guest-level and are passed separately via SetupSandboxRequest after boot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmmNetworkConfig {
    pub tap_device: String,   // pre-existing host tap interface
    pub mac: String,          // empty = auto-generate
    pub queue: u32,           // virtio-net queue count (= vcpu count)
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
    pub hot_plug_disk: bool,    // VirtioBlock hot-attach/detach (CH, QEMU, StratoVirt)
    pub hot_plug_net: bool,     // virtio-net hot-attach/detach
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
    VirtioBlock { id: String, path: String, read_only: bool },

    /// Virtiofs share backed by a running virtiofsd instance.
    /// Supported by: CH, QEMU, StratoVirt. Check `virtiofs`.
    VirtioFs { id: String, tag: String, socket: String },

    /// Virtio-serial char device backed by a named pipe.
    /// Used for container stdin/stdout/stderr on backends that support virtio-serial.
    /// `chardev_id` is the backend identifier; `name` is the port name seen in the guest.
    /// Supported by: CH, QEMU, StratoVirt. Check `virtio_serial`.
    CharDevice { id: String, chardev_id: String, name: String, path: String },

    /// Vsock-multiplexed IO channel — Firecracker's container IO model.
    /// Instead of a per-pipe CharDevice, a single vsock stream multiplexes
    /// stdin/stdout/stderr using `port` as the vsock port number.
    /// The guest-side agent identifies the container by `container_id`.
    /// Supported by: Firecracker. Check `!virtio_serial`.
    VsockMuxIO { id: String, container_id: String, port: u32 },
}

/// Result of a successful hot-plug operation.
#[derive(Debug, Clone)]
pub struct HotPlugResult {
    pub device_id: String,
    pub bus_addr: String,   // PCI/MMIO address assigned by the VMM
}

/// vCPU thread IDs, used for placing vcpu threads in the right cgroup.
#[derive(Debug)]
pub struct VcpuThreads {
    pub vcpus: HashMap<i64, i64>,   // vcpu_index → tid
}

/// PIDs associated with this VMM instance.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Pids {
    pub vmm_pid: Option<u32>,
    pub affiliated_pids: Vec<u32>,  // e.g. virtiofsd processes
}
```

### `Vmm` trait

```rust
/// VMM lifecycle abstraction — independent of runtime mode.
///
/// Each backend provides:
///   - An associated `Config` type (backend-specific TOML config struct).
///   - A static `create()` constructor — the single construction point, replacing
///     the closure-based `VmmFactory`. `SandboxEngine` holds `V::Config` and calls
///     `V::create(id, base_dir, config)` to produce each new pre-boot instance.
///
/// Cross-cutting lifecycle customisation (resource config, task_address, graceful
/// stop, etc.) belongs in `Hooks<V>`, NOT in this trait. The `Vmm` trait only
/// contains VMM process management and device management.
///
/// Lifecycle sequence (engine drives, hooks customise):
///   V::create(id, base_dir, &config)   → construct pre-boot instance  [Vmm::create]
///   hooks.post_create(&mut ctx)        → optional post-create setup   [Hooks]
///   vmm.add_disk / vmm.add_network     → attach static devices        [Vmm]
///   hooks.pre_start(&mut ctx)          → apply pod spec to VMM config [Hooks]
///   vmm.boot()                         → start VMM process            [Vmm]
///   hooks.post_start(&mut ctx)         → set task_address, etc.       [Hooks]
///   [ sandbox Running ]
///   hooks.pre_stop(&mut ctx)           → graceful pre-stop            [Hooks]
///   vmm.stop(force)                    → stop VMM process             [Vmm]
///   hooks.post_stop(&mut ctx)          → cleanup                      [Hooks]
#[async_trait]
pub trait Vmm: Send + Sync + 'static {
    /// Backend-specific configuration type, loaded from TOML at startup.
    /// `SandboxEngine` holds one instance of this; it is cloned into each `create` call.
    type Config: Clone + Send + Sync + serde::de::DeserializeOwned;

    /// Construct a pre-boot VMM instance for the given sandbox.
    /// Derives all paths from `base_dir` (api_socket, vsock_path, etc.),
    /// stores a clone of `config`, and returns without starting any process.
    /// This is the single construction point; `SandboxEngine` calls it instead of
    /// a factory closure, keeping the config type visible in the type system.
    ///
    /// `vsock_cid` is a unique guest CID allocated by the engine from its
    /// `next_vsock_cid` counter (range 3..=u32::MAX; 0/1/2 are system-reserved).
    /// Backends that use numeric CIDs (QEMU, StratoVirt, Firecracker) store it;
    /// backends with file-based vsock (Cloud Hypervisor) may ignore it (`_vsock_cid`).
    async fn create(id: &str, base_dir: &str, config: &Self::Config, vsock_cid: u32) -> Result<Self>
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
    /// Mirrors old `VM::wait_channel() -> Option<Receiver<(u32, i128)>>`.
    fn subscribe_exit(&self) -> tokio::sync::watch::Receiver<Option<ExitInfo>>;

    /// Reconnect to the VMM API socket after process restart.
    /// Re-creates the API client and wait channel from the already-running process.
    /// Only called during recovery for sandboxes found in Running state.
    /// Mirrors old `Recoverable` trait + `impl_recoverable!` macro.
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
    /// Called by Hooks::post_start to set inst.data.task_address.
    fn task_address(&self) -> String {
        self.vsock_path()
            .map(|p| format!("ttrpc+{}", p))
            .unwrap_or_default()
    }

    /// Query VMM capabilities. The engine and adapter check these before calling
    /// operations that not all backends support (hot-plug, virtiofs, virtio-serial).
    fn capabilities(&self) -> VmmCapabilities;
}
```

### Construction via `Vmm::create` — no separate factory

`VmmFactory<V>` is **removed**. The associated type `V::Config` replaces the closure-based
config erasure:

- `SandboxEngine` holds `vmm_config: V::Config` (instead of a factory closure)
- `create_sandbox` calls `V::create(id, &base_dir, &self.vmm_config).await` directly
- No `Arc<dyn Fn>` allocation, no indirection, config type visible in type signatures
- Monomorphised at compile time — zero runtime cost

```rust
// engine/src/engine.rs — create_sandbox (construction site)
let vsock_cid = self.next_vsock_cid.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
let vmm = V::create(id, &base_dir, &self.vmm_config, vsock_cid).await?;
```

Each backend implements `Vmm::create` in its own crate. Example for Cloud Hypervisor:

```rust
// vmm/cloud-hypervisor/src/vmm.rs
impl Vmm for CloudHypervisorVmm {
    type Config = CloudHypervisorVmmConfig;

    async fn create(id: &str, base_dir: &str, config: &Self::Config, _vsock_cid: u32) -> Result<Self> {
        // Cloud Hypervisor uses a file-based hvsock — numeric CID not needed.
        Ok(Self {
            id: id.to_string(),
            base_dir: base_dir.to_string(),
            config: config.clone(),
            api_socket: format!("{}/api.sock", base_dir),
            process: None,
            vsock_path: format!("hvsock://{}/task.vsock", base_dir),
        })
    }
    // ... other methods
}
```

### `CloudHypervisorVmm` (new struct, `vmm/cloud-hypervisor`)

**Crate name**: `vmm-cloud-hypervisor`
**Path**: `vmm/cloud-hypervisor/src/`
**Dependencies**: `vmm-vm-trait`, `vmm-common`, `async-trait`, `anyhow`, `tokio`, `serde`

**Relationship to existing code**: `CloudHypervisorVM` in `vmm/sandbox/src/cloud_hypervisor/` continues to exist unchanged (for existing test compatibility). `CloudHypervisorVmm` is a new struct in the new `vmm/cloud-hypervisor/` crate. It reuses the CH API client (`api_client`) but presents the new `Vmm` interface.

```rust
// vmm/cloud-hypervisor/src/vmm.rs

pub struct CloudHypervisorVmm {
    id: String,
    base_dir: String,
    config: CloudHypervisorVmmConfig,
    api_socket: String,         // path to CH API socket
    process: Option<Child>,     // VMM process handle
    vsock_path: String,         // hvsock path for guest comms
}

// CloudHypervisorVmmConfig mirrors the relevant fields from existing
// CloudHypervisorVMConfig in vmm/sandbox/src/cloud_hypervisor/config.rs
#[derive(Clone, Deserialize)]
pub struct CloudHypervisorVmmConfig {
    pub binary: String,
    pub api_socket_timeout_ms: u64,
    pub serial_output_dir: String,
    // forwarded from [cloud-hypervisor] section in kuasar.toml
    #[serde(flatten)]
    pub common: HypervisorCommonConfig,  // re-exported from vmm-common
}
```

**Method mapping** (`Vmm` trait → CH implementation):

| `Vmm` method | CH implementation |
|---|---|
| `create(id, base_dir, config)` | Set `api_socket = "{base_dir}/api.sock"`, `vsock_path = "hvsock://{base_dir}/task.vsock"`. No I/O. |
| `boot()` | Spawn CH process with `--kernel --disk --memory ...` flags |
| `stop(force)` | `PUT /api/v1/vm.power-button` or SIGKILL |
| `subscribe_exit()` | Return a `watch::Receiver<Option<ExitInfo>>` backed by the child process; a background task calls `child.wait()` and sends the result |
| `recover()` | Re-open the CH API socket; re-create the watch channel from the running process PID |
| `add_disk(d)` | Append `--disk path=<d.path>` to pending CLI arg list |
| `add_network(n)` | Append `--net tap=<n.tap_device>,mac=<n.mac>,num_queues=<n.queue*2>` |
| `hot_attach(VirtioBlock)` | `PUT /api/v1/vm.add-disk`; returns PCI address |
| `hot_attach(VirtioFs)` | `PUT /api/v1/vm.add-fs`; returns PCI address |
| `hot_attach(CharDevice)` | `PUT /api/v1/vm.add-device` with virtio-serial + chardev backend |
| `hot_attach(VsockMuxIO)` | **Not supported** — adapter checks `capabilities().virtio_serial` first |
| `hot_detach(id)` | `PUT /api/v1/vm.remove-device {"id": id}` |
| `ping()` | `GET /api/v1/vm.info` — returns Ok if CH responds |
| `vcpus()` | Read `/proc/<pid>/task/*/status` for VCPU threads |
| `pids()` | `vmm_pid = child.id()`, `affiliated_pids = [virtiofsd_pid, ...]` |
| `vsock_path()` | Return `"hvsock://<base_dir>/task.vsock"` |
| `task_address()` | Default impl: `"ttrpc+hvsock://<base_dir>/task.vsock"` |
| `capabilities()` | `{ hot_plug_disk: true, hot_plug_net: true, virtiofs: true, virtio_serial: true, snapshot_restore: false, … }` |

Resource limits (vCPU / memory), graceful stop, and post-stop cleanup are handled by
`CloudHypervisorHooks` (see `Hooks<V>` section), not by `CloudHypervisorVmm` itself.

### Multi-VMM Backend Design

All three currently-implemented backends (Cloud Hypervisor, QEMU, StratoVirt) receive new
`vmm/*` crates in Epic 1. Firecracker is documented for design validation only — no
implementation required.

#### `QemuVmm` (`vmm/qemu/`)

**Crate name**: `vmm-qemu`
**Path**: `vmm/qemu/src/`
**Dependencies**: `vmm-vm-trait`, `vmm-common`, `async-trait`, `anyhow`, `tokio`, `serde`

Mirrors `QemuVM` in `vmm/sandbox/src/qemu/`. Uses QMP (QEMU Machine Protocol) over a unix
socket for hot-plug and management.

```rust
pub struct QemuVmm {
    id: String,
    base_dir: String,
    config: QemuVmmConfig,
    qmp_socket: String,         // path to QMP socket
    process: Option<Child>,
    vsock_cid: u32,             // guest CID for vsock communication
}

impl Vmm for QemuVmm {
    type Config = QemuVmmConfig;

    async fn create(id: &str, base_dir: &str, config: &Self::Config, vsock_cid: u32) -> Result<Self> {
        // vsock_cid is allocated by SandboxEngine::next_vsock_cid (range 3..=u32::MAX).
        Ok(Self {
            id: id.to_string(),
            base_dir: base_dir.to_string(),
            config: config.clone(),
            qmp_socket: format!("{}/qmp.sock", base_dir),
            process: None,
            vsock_cid,
        })
    }

    fn vsock_path(&self) -> Result<String> {
        Ok(format!("vsock://{}:1024", self.vsock_cid))
    }
    // boot: spawn qemu-system-x86_64 with -qmp unix:<qmp_socket>
    // hot_attach: QMP device_add / drive_add commands
    // hot_detach: QMP device_del
    // ...
}
```

#### `StratoVirtVmm` (`vmm/stratovirt/`)

**Crate name**: `vmm-stratovirt`
**Path**: `vmm/stratovirt/src/`
**Dependencies**: `vmm-vm-trait`, `vmm-common`, `async-trait`, `anyhow`, `tokio`, `serde`

Mirrors `StratoVirtVM` in `vmm/sandbox/src/stratovirt/`. Uses a StratoVirt-specific API
(similar to QMP) over a unix socket.

```rust
pub struct StratoVirtVmm {
    id: String,
    base_dir: String,
    config: StratoVirtVmmConfig,
    api_socket: String,
    process: Option<Child>,
    vsock_cid: u32,
}

impl Vmm for StratoVirtVmm {
    type Config = StratoVirtVmmConfig;

    async fn create(id: &str, base_dir: &str, config: &Self::Config, vsock_cid: u32) -> Result<Self> {
        // vsock_cid is allocated by SandboxEngine::next_vsock_cid (range 3..=u32::MAX).
        Ok(Self {
            id: id.to_string(),
            base_dir: base_dir.to_string(),
            config: config.clone(),
            api_socket: format!("{}/stratovirt.sock", base_dir),
            process: None,
            vsock_cid,
        })
    }

    fn vsock_path(&self) -> Result<String> {
        Ok(format!("vsock://{}:1024", self.vsock_cid))
    }
    // boot: spawn stratovirt with appropriate flags
    // hot_attach / hot_detach: StratoVirt API calls
    // ...
}
```

#### Capability matrix

| Capability flag | CloudHypervisor | QEMU | StratoVirt | Firecracker (planned) |
|---|---|---|---|---|
| `hot_plug_disk` | ✓ | ✓ | ✓ | ✗ (drive config at boot only) |
| `hot_plug_net` | ✓ | ✓ | ✓ | ✗ |
| `hot_plug_cpu` | ✓ | ✓ | ✗ | ✗ |
| `hot_plug_mem` | ✓ | ✓ | ✗ | ✗ |
| `virtiofs` | ✓ | ✓ | ✓ | ✗ (no virtiofsd support) |
| `virtio_serial` | ✓ | ✓ | ✓ | ✗ |
| `snapshot_restore` | ✗ | partial | ✗ | ✓ |

#### How the engine/adapter uses capabilities

```rust
// adapter-k8s/src/sandboxer.rs — attach_io_pipes
// Check at runtime which IO model the backend supports.
if inst.vmm.capabilities().virtio_serial {
    // CH / QEMU / StratoVirt path: virtio-serial CharDevice per pipe
    attach_io_pipes_char(&mut inst, id, io, io_devices, data).await?;
} else {
    // Firecracker path: single vsock port multiplexes all stdio
    attach_io_vsock_mux(&mut inst, id, io, io_devices, data).await?;
}
```

The `attach_io_vsock_mux` path calls `hot_attach(VsockMuxIO { port })` and relies on the guest-side agent (vmm-task) to route the multiplexed streams to the correct container. This guest protocol is outside the scope of Epic 1.

#### Firecracker-specific notes (future)

- **`Vmm::create`**: `FirecrackerVmm::create(id, base_dir, config, vsock_cid)` stores the CID allocated by `SandboxEngine::next_vsock_cid` and derives `api_socket`.
- **`vsock_path()`**: returns `"vsock://<cid>:1024"` — numeric CID, not a file path.
- **`task_address()`**: override returns `"ttrpc+vsock://<cid>:1024"`.
- **`add_disk(d)`**: appends to the Firecracker boot payload drive list (no hot-plug after boot).
- **`hot_attach` / `hot_detach`**: return `Err(NotSupported)` for VirtioBlock/VirtioFs/CharDevice; accept `VsockMuxIO` by allocating a new vsock port.
- **`FirecrackerHooks::pre_start`**: reads vCPU count + memory from `ctx.data`, writes into the boot payload via `ctx.vmm.set_machine_config(vcpu, mem)` (Firecracker-specific method, not on `Vmm` trait).
- **`FirecrackerHooks::pre_stop`**: optionally saves snapshot to `{ctx.base_dir}/snapshot`.
- **`FirecrackerHooks::post_stop`**: removes `api.sock` via `ctx.base_dir`.

#### QEMU / StratoVirt

Both use QMP (or a similar protocol) over a unix socket for management.
`vsock_path()` returns `"vsock://<cid>:1024"` (CID-based, not hvsock file path).
`task_address()` default impl therefore returns `"ttrpc+vsock://<cid>:1024"` — no override needed.
Both implementations are in `vmm/qemu/` and `vmm/stratovirt/` respectively (Epic 1 scope).

### `Hooks<V>` trait — backend lifecycle customisation

**Path**: `vmm/vm-trait/src/hooks.rs`

`Hooks<V>` lives in `vmm/vm-trait` alongside the `Vmm` trait, so each backend crate
(`vmm/cloud-hypervisor`, `vmm/qemu`, `vmm/stratovirt`) can implement its own hooks without
depending on `vmm/engine`. See the **Crate Dependency Graph** section for the full layering.

Hook methods receive `SandboxCtx<V>` — a lightweight view defined in `vmm/vm-trait` that
exposes only the VMM instance, sandbox data, and base directory. The engine constructs
`SandboxCtx` from `SandboxInstance<V>` at each call site; the hook never sees the full
engine-level instance type.

```rust
// vm-trait/src/hooks.rs

/// Lightweight context passed to every hook call.
/// Defined here (not in vmm/engine) so backend crates can implement Hooks<V>
/// without depending on the engine crate.
pub struct SandboxCtx<'a, V: Vmm> {
    pub vmm: &'a mut V,
    pub data: &'a mut SandboxData,
    pub base_dir: &'a str,
}

#[async_trait]
pub trait Hooks<V: Vmm>: Send + Sync + 'static {
    /// Called in create_sandbox after SandboxInstance is constructed.
    /// Can do backend-specific setup that requires the sandbox directory to exist.
    /// Default: no-op.
    async fn post_create(&self, _ctx: &mut SandboxCtx<'_, V>) -> Result<()> { Ok(()) }

    /// Called in start_sandbox before add_disk / add_network / boot.
    /// Typical use: read pod resource limits from ctx.data, apply to ctx.vmm
    /// via backend-specific methods (not on Vmm trait).
    /// Default: no-op.
    async fn pre_start(&self, _ctx: &mut SandboxCtx<'_, V>) -> Result<()> { Ok(()) }

    /// Called in start_sandbox after wait_ready() succeeds and before state → Running.
    /// Typical use: set ctx.data.task_address, publish metrics.
    /// Default: sets task_address = ctx.vmm.task_address().
    async fn post_start(&self, ctx: &mut SandboxCtx<'_, V>) -> Result<()> {
        ctx.data.task_address = ctx.vmm.task_address();
        Ok(())
    }

    /// Called in stop_sandbox before vmm.stop(), only when force=false.
    /// Typical use: graceful shutdown notification, save snapshot (Firecracker).
    /// Default: no-op.
    async fn pre_stop(&self, _ctx: &mut SandboxCtx<'_, V>) -> Result<()> { Ok(()) }

    /// Called in stop_sandbox after vmm.stop() completes.
    /// Typical use: clean up backend-specific resources (sockets, host devices).
    /// Default: no-op.
    async fn post_stop(&self, _ctx: &mut SandboxCtx<'_, V>) -> Result<()> { Ok(()) }
}

/// No-op implementation for use in tests and as a default when no customisation is needed.
pub struct NoopHooks<V>(std::marker::PhantomData<V>);

impl<V: Vmm> Default for NoopHooks<V> {
    fn default() -> Self { Self(std::marker::PhantomData) }
}

#[async_trait]
impl<V: Vmm> Hooks<V> for NoopHooks<V> {}  // all default no-ops
```

#### Per-backend hooks

Each backend crate owns its hook implementation. The engine crate (`vmm/engine`) and the
binary (`cmd/vmm-engine`) have no knowledge of hook logic — they only hold `H: Hooks<V>` as
a type parameter and call the trait methods.

```rust
// cloud-hypervisor/src/hooks.rs

pub struct CloudHypervisorHooks;

#[async_trait]
impl Hooks<CloudHypervisorVmm> for CloudHypervisorHooks {
    async fn pre_start(&self, ctx: &mut SandboxCtx<'_, CloudHypervisorVmm>) -> Result<()> {
        // Mirror of CloudHypervisorHooks::pre_start / process_config in vmm/sandbox.
        if let Some(res) = get_resources(ctx.data) {
            let vcpu = compute_vcpu(res);
            let memory_mb = res.memory_limit_in_bytes / (1024 * 1024);
            ctx.vmm.set_cpus(vcpu);           // CloudHypervisorVmm-specific setter
            ctx.vmm.set_memory_mb(memory_mb); // CloudHypervisorVmm-specific setter
        }
        Ok(())
    }

    async fn post_start(&self, ctx: &mut SandboxCtx<'_, CloudHypervisorVmm>) -> Result<()> {
        ctx.data.task_address = ctx.vmm.task_address();
        Ok(())
    }

    async fn post_stop(&self, ctx: &mut SandboxCtx<'_, CloudHypervisorVmm>) -> Result<()> {
        tokio::fs::remove_file(format!("{}/api.sock", ctx.base_dir)).await.ok();
        tokio::fs::remove_file(format!("{}/task.vsock", ctx.base_dir)).await.ok();
        Ok(())
    }
}
```

```rust
// qemu/src/hooks.rs

pub struct QemuHooks;

#[async_trait]
impl Hooks<QemuVmm> for QemuHooks {
    async fn pre_start(&self, ctx: &mut SandboxCtx<'_, QemuVmm>) -> Result<()> {
        // Mirror of QemuHooks::pre_start / process_config in vmm/sandbox.
        if let Some(res) = get_resources(ctx.data) {
            let vcpu = compute_vcpu(res);
            let memory_mb = res.memory_limit_in_bytes / (1024 * 1024);
            ctx.vmm.set_smp(vcpu);            // QemuVmm-specific
            ctx.vmm.set_memory_mb(memory_mb); // QemuVmm-specific
        }
        Ok(())
    }

    async fn post_start(&self, ctx: &mut SandboxCtx<'_, QemuVmm>) -> Result<()> {
        ctx.data.task_address = ctx.vmm.task_address();
        Ok(())
    }
    // post_stop: default no-op (QEMU cleans up its own socket files)
}
```

```rust
// stratovirt/src/hooks.rs

pub struct StratoVirtHooks;

#[async_trait]
impl Hooks<StratoVirtVmm> for StratoVirtHooks {
    // pre_start: StratoVirt pre_start is currently TODO in old code; no-op here.

    async fn post_start(&self, ctx: &mut SandboxCtx<'_, StratoVirtVmm>) -> Result<()> {
        ctx.data.task_address = ctx.vmm.task_address();
        Ok(())
    }
    // post_stop: default no-op
}
```

---

## Story 1.2 — `guest-runtime` crate

**Crate name**: `vmm-guest-runtime`
**Path**: `vmm/guest-runtime/src/lib.rs`
**Dependencies**: `async-trait`, `anyhow`, `tokio`, `vmm-common`

### Supporting types

```rust
/// Result of a successful wait_ready call.
pub struct ReadyResult {
    pub sandbox_id: String,
    pub timestamp_ms: u64,
}

/// Exit status of a process or VM.
pub struct ExitStatus {
    pub exit_code: i32,
    pub exited_at_ms: u64,
}

/// Resource statistics for a sandbox or container.
pub struct ContainerStats {
    pub cpu_usage_ns: u64,
    pub memory_rss_bytes: u64,
    pub pids_current: u64,
}

/// OCI container specification (passed to vmm-task).
/// Wraps the raw bytes to avoid coupling guest-runtime to OCI crate.
pub struct ContainerSpec {
    pub id: String,
    pub bundle: String,
    pub rootfs: Vec<Mount>,
    pub io: ContainerIo,
    // OCI spec JSON bytes forwarded verbatim to vmm-task
    pub spec_json: Vec<u8>,
}

pub struct ContainerIo {
    pub stdin: String,
    pub stdout: String,
    pub stderr: String,
    pub terminal: bool,
}

pub struct ContainerInfo {
    pub pid: u32,
}

pub struct ExecSpec {
    pub exec_id: String,
    pub spec_json: Vec<u8>,
    pub io: ContainerIo,
}

pub struct ProcessInfo {
    pub pid: u32,
}

pub struct Mount {
    pub kind: String,
    pub source: String,
    pub target: String,
    pub options: Vec<String>,
}
```

### `GuestReadiness` trait

```rust
/// Guest readiness abstraction used by SandboxEngine.
/// Contains only the three methods the engine itself calls: wait_ready, setup_sandbox,
/// forward_events. Process-management operations (kill_process, wait_process,
/// container_stats) belong in ContainerRuntime — they are only called by K8sAdapter.
///
/// Implementation: VmmTaskRuntime (vmm/runtime-vmm-task): ttrpc over vsock to vmm-task.
#[async_trait]
pub trait GuestReadiness: Send + Sync + 'static {
    /// Wait for the guest to become ready to serve.
    /// Calls ttrpc Check(), then SetupSandbox(); returns when vmm-task is responsive.
    /// Returns Err if timeout expires; the sandbox transitions to Stopped.
    async fn wait_ready(
        &self,
        sandbox_id: &str,
        vsock_path: &str,
    ) -> Result<ReadyResult>;

    /// Send network interface + route configuration to the guest via SetupSandboxRequest,
    /// and push PodSandboxConfig for DNS/hostname resolution inside the VM.
    /// Called once after wait_ready() succeeds, before the sandbox is marked Running.
    async fn setup_sandbox(&self, sandbox_id: &str, req: &SandboxSetupRequest) -> Result<()>;

    /// Spawn a background task that polls vmm-task for OOM/exit events and forwards
    /// them to containerd. Runs until the exit_signal fires.
    /// Called once after the sandbox transitions to Running.
    async fn forward_events(&self, sandbox_id: &str, exit_signal: Arc<ExitSignal>);
}
```

### Supporting types for `GuestReadiness`

```rust
// guest-runtime/src/lib.rs

/// Discovered network interface configuration (from the pod netns).
#[derive(Debug, Clone)]
pub struct NetworkInterface {
    pub name: String,
    pub mac: String,
    pub ip_addresses: Vec<IpNet>,   // CIDR notation
    pub mtu: u32,
}

/// IP route entry discovered from the pod netns.
#[derive(Debug, Clone)]
pub struct Route {
    pub dest: String,       // CIDR or "default"
    pub gateway: String,
    pub device: String,
}

/// Request sent to vmm-task via SetupSandbox ttrpc call after the VM boots.
/// Contains everything the guest needs to configure networking and identity.
#[derive(Debug)]
pub struct SandboxSetupRequest {
    pub interfaces: Vec<NetworkInterface>,
    pub routes: Vec<Route>,
    pub sandbox_data: SandboxData,  // provides hostname, DNS config, pod annotations
}
```

### `ContainerRuntime` trait

```rust
/// Container lifecycle and process-management operations via vmm-task ttrpc.
/// VmmTaskRuntime implements this trait.
///
/// `ContainerRuntime` is intentionally independent from `GuestReadiness` — they can be
/// implemented by different types or combined via a where bound at the call site.
/// `SandboxEngine` only requires `R: GuestReadiness` (readiness + network setup).
/// `K8sAdapter` additionally requires `R: ContainerRuntime` for Task API delegation,
/// expressed as a combined bound: `R: GuestReadiness + ContainerRuntime`.
#[async_trait]
pub trait ContainerRuntime {
    /// Create a container inside the VM via ttrpc create_container().
    async fn create_container(
        &self,
        sandbox_id: &str,
        spec: ContainerSpec,
    ) -> Result<ContainerInfo>;

    /// Start the main process of a container via ttrpc start_process().
    async fn start_process(
        &self,
        sandbox_id: &str,
        container_id: &str,
    ) -> Result<ProcessInfo>;

    /// Execute an additional process inside a container via ttrpc exec_process().
    async fn exec_process(
        &self,
        sandbox_id: &str,
        container_id: &str,
        spec: ExecSpec,
    ) -> Result<ProcessInfo>;

    /// Signal a process to stop via ttrpc signal_process().
    async fn kill_process(
        &self,
        sandbox_id: &str,
        container_id: &str,
        pid: u32,
        signal: u32,
    ) -> Result<()>;

    /// Wait for a process to exit and return its status via ttrpc wait_process().
    async fn wait_process(
        &self,
        sandbox_id: &str,
        container_id: &str,
        pid: u32,
    ) -> Result<ExitStatus>;

    /// Read container/sandbox resource statistics via ttrpc get_stats().
    async fn container_stats(
        &self,
        sandbox_id: &str,
        container_id: &str,
    ) -> Result<ContainerStats>;
}
```

---

## Story 1.3 — `engine` crate

**Crate name**: `vmm-engine`
**Path**: `vmm/engine/src/`
**Dependencies**: `vmm-vm-trait`, `vmm-guest-runtime`, `vmm-common`, `containerd-sandbox`, `async-trait`, `tokio`, `serde`, `anyhow`, `tracing`

> `containerd-sandbox` is required directly for `SandboxData`, `ExitSignal`,
> `ContainerData`, and `ProcessData` re-exported from `engine/src/instance.rs`.

### `SandboxState` enum

```rust
// engine/src/state.rs

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxState {
    Creating,
    Running,
    // Paused — removed from Epic 1; no backend supports VM pause/resume yet.
    // Re-introduce together with Vmm::pause() / Vmm::resume() in a future epic.
    Stopped,
    Deleted,
}

impl SandboxState {
    /// Returns Err(InvalidState) if the transition is not allowed.
    pub fn transition(&self, event: StateEvent) -> Result<SandboxState> {
        match (self, event) {
            (SandboxState::Creating, StateEvent::StartSucceeded) => Ok(SandboxState::Running),
            (SandboxState::Creating, StateEvent::StartFailed)    => Ok(SandboxState::Stopped),
            (SandboxState::Running,  StateEvent::Stop)           => Ok(SandboxState::Stopped),
            (SandboxState::Stopped,  StateEvent::Delete)         => Ok(SandboxState::Deleted),
            // Force-delete from any state
            (_,                      StateEvent::ForceDelete)    => Ok(SandboxState::Deleted),
            (from, event) => Err(Error::InvalidState(format!(
                "cannot {:?} a sandbox in state {:?}", event, from
            ))),
        }
    }
}

#[derive(Debug, Clone)]
pub enum StateEvent {
    StartSucceeded,
    StartFailed,
    Stop,
    Delete,
    ForceDelete,
    // Pause / Resume — reserved for future epic; requires Vmm::pause() / Vmm::resume().
}
```

### `SandboxInstance<V>`

```rust
// engine/src/instance.rs

/// Containerd sandbox metadata forwarded verbatim from the Sandbox API.
/// The engine stores it opaquely; K8sAdapter uses it for event publishing.
// Re-exported from containerd_sandbox::data::SandboxData
pub use containerd_sandbox::data::SandboxData;

/// Notifies waiters when the VMM process exits unexpectedly.
/// Uses the ExitSignal from containerd_sandbox::signal.
pub use containerd_sandbox::signal::ExitSignal;

/// Host-side cgroup set for a sandbox: sandbox cgroup, vcpu cgroup, pod-overhead cgroup.
/// Only created on cgroup-v1 systems; on cgroup-v2 this is a no-op wrapper.
/// Wraps SandboxCgroup from vmm/sandbox/src/cgroup.rs, re-exported via vmm-common.
pub use vmm_common::cgroup::SandboxCgroup;

/// Discovered network state for this sandbox.
/// Populated by prepare_network() and used to configure the guest via SetupSandbox.
#[derive(Serialize, Deserialize)]
pub struct NetworkState {
    pub interfaces: Vec<NetworkInterface>,
    pub routes: Vec<Route>,
}

/// A storage share mounted into the VM (e.g. a bind-mount backed by the shared virtiofs, or a
/// hot-plugged block device).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageMount {
    pub id: String,
    /// All containers that reference this mount. Mirrors old Storage::ref_containers.
    /// The mount is physically unmounted (and the device hot-detached) only when this
    /// Vec is empty, supporting multiple containers sharing the same hostPath/configmap.
    pub ref_containers: Vec<String>,
    /// Original host source path (bind mount source or block device path).
    /// Used for dedup lookup in attach_container_storages.
    pub host_path: String,
    /// Bind-mount destination inside the shared virtiofs directory, if applicable.
    /// Set to Some(host_dest) for VirtioFs mounts; None for block devices.
    /// This is the path that must be unmounted in deference_container_storages.
    pub mount_dest: Option<String>,
    pub guest_path: String,     // path inside the guest where vmm-task mounts this
    pub kind: StorageMountKind,
    /// If this storage was backed by a hot-plugged block device, this holds its device_id
    /// so it can be hot-detached when the last referencing container is removed.
    pub device_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StorageMountKind { VirtioFs, Virtio9P, Block }

/// Per-container tracking: metadata + which host IO devices were hot-plugged.
/// io_devices holds device_ids of hot-plugged CharDevice (IO pipes) and VirtioBlock devices;
/// these are hot-detached when the container is removed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerState {
    pub id: String,
    pub data: containerd_sandbox::data::ContainerData,
    pub io_devices: Vec<String>,        // device_ids for hot-plugged IO/block devices
    pub processes: Vec<ProcessState>,   // exec processes
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessState {
    pub id: String,
    pub io_devices: Vec<String>,
    pub data: containerd_sandbox::data::ProcessData,
}

/// Per-sandbox state held in the engine.
pub struct SandboxInstance<V: Vmm> {
    pub id: String,
    pub vmm: V,
    pub state: SandboxState,
    pub base_dir: String,

    // Metadata forwarded from containerd (used by K8sAdapter for event publishing).
    pub data: SandboxData,

    // Network namespace path (set from CreateSandboxRequest).
    pub netns: String,

    // Discovered network state, populated during start_sandbox before boot.
    #[serde(default)]
    pub network: Option<NetworkState>,

    // Storage mounts currently attached to this sandbox.
    pub storages: Vec<StorageMount>,

    // Containers and their hot-plugged device IDs.
    pub containers: HashMap<String, ContainerState>,

    // Sequential counter for unique device/storage ID generation (virtioserial, blk, storage…).
    pub id_generator: u32,
    // Separate counter for vsock port allocation (starts at 1025, above ttrpc port 1024).
    // Kept independent from id_generator to avoid non-contiguous port numbers and overflow.
    pub vsock_port_next: u32,

    // Host-side cgroup set for this sandbox (not serialised; reconstructed on recovery).
    #[serde(skip, default)]
    pub cgroup: SandboxCgroup,

    // Exit signal — fired when the VMM process exits unexpectedly.
    #[serde(skip, default)]
    pub exit_signal: Arc<ExitSignal>,
}
```

### `SandboxEngine<V, R, H>`

```rust
// engine/src/engine.rs

pub struct SandboxEngine<V: Vmm, R: GuestReadiness, H: Hooks<V>> {
    vmm_config: V::Config,  // cloned into each V::create() call; replaces VmmFactory
    runtime: R,
    hooks: H,
    config: EngineConfig,
    sandboxes: Arc<RwLock<HashMap<String, Arc<Mutex<SandboxInstance<V>>>>>>,
    /// Unique vsock CID allocator. Starts at 3 (0 = hypervisor, 1 = loopback, 2 = host).
    /// Incremented atomically per create_sandbox call; passed to Vmm::create as vsock_cid.
    next_vsock_cid: std::sync::atomic::AtomicU32,
}

impl<V: Vmm, R: GuestReadiness, H: Hooks<V>> SandboxEngine<V, R, H> {
    pub fn new(vmm_config: V::Config, runtime: R, hooks: H, config: EngineConfig) -> Self {
        Self { vmm_config, runtime, hooks, config,
               sandboxes: Arc::new(RwLock::new(HashMap::new())),
               next_vsock_cid: std::sync::atomic::AtomicU32::new(3) }
    }

    /// Expose the runtime so K8sAdapter can forward Task API calls directly.
    /// K8sAdapter holds Arc<SandboxEngine<V,R,H>>, so this borrow is always valid.
    pub fn runtime(&self) -> &R { &self.runtime }
}
```

#### `create_sandbox`

```rust
pub async fn create_sandbox(
    &self,
    id: &str,
    req: CreateSandboxRequest,
) -> Result<()> {
    // Idempotency guard
    if self.sandboxes.read().await.contains_key(id) {
        return Err(Error::AlreadyExists(id.to_string()));
    }

    let base_dir = format!("{}/{}", self.config.work_dir, id);
    tokio::fs::create_dir_all(&base_dir).await?;

    // Write hostname, /etc/hosts, resolv.conf into the virtiofs-shared directory.
    // Must happen before VM starts so virtiofsd can serve these files.
    setup_sandbox_files(&base_dir, &req.sandbox_data).await?;

    // Create host-side cgroup (cgroup-v1 only; no-op on cgroup-v2).
    let cgroup_parent = if req.cgroup_parent.is_empty() {
        DEFAULT_CGROUP_PARENT_PATH.to_string()
    } else {
        req.cgroup_parent.clone()
    };
    let cgroup = SandboxCgroup::create_sandbox_cgroups(&cgroup_parent, id)?;
    cgroup.update_res_for_sandbox_cgroups(&req.sandbox_data)?;

    // Allocate a unique vsock CID for this sandbox.
    // Range 3..=u32::MAX; 0/1/2 are system-reserved. Relaxed ordering is safe
    // here because the CID only needs to be unique within this process.
    let vsock_cid = self.next_vsock_cid.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // Construct the VMM instance via the associated Vmm::create constructor.
    // No process is started; this only allocates the struct and derives paths.
    let vmm = V::create(id, &base_dir, &self.vmm_config, vsock_cid).await?;

    // Attach root disk before boot.
    let mut vmm = vmm;
    if let Some(disk) = req.rootfs_disk {
        vmm.add_disk(disk)?;
    }

    let mut instance = SandboxInstance {
        id: id.to_string(),
        vmm,
        state: SandboxState::Creating,
        base_dir,
        data: req.sandbox_data,
        netns: req.netns,
        network: None,
        storages: vec![],
        containers: HashMap::new(),
        id_generator: 0,
        vsock_port_next: 1025,   // 1024 is reserved for ttrpc
        cgroup,
        exit_signal: Arc::new(ExitSignal::default()),
    };

    // Allow the hook to do any backend-specific post-create setup.
    let mut ctx = SandboxCtx { vmm: &mut instance.vmm, data: &mut instance.data, base_dir: &instance.base_dir };
    self.hooks.post_create(&mut ctx).await?;

    // Persist state to disk (enables recovery after restart).
    instance.dump().await?;

    self.sandboxes.write().await
        .insert(id.to_string(), Arc::new(Mutex::new(instance)));
    Ok(())
}
```

#### `setup_sandbox_files`

```rust
/// Write hostname, /etc/hosts, and resolv.conf into the virtiofs shared directory.
/// The directory is shared read-only into the VM; vmm-task bind-mounts these files
/// over the container's /etc equivalents on container create.
async fn setup_sandbox_files(base_dir: &str, data: &SandboxData) -> Result<()> {
    let shared = format!("{}/{}", base_dir, SHARED_DIR_SUFFIX);
    tokio::fs::create_dir_all(&shared).await?;

    // hostname: from SandboxData or fallback to host hostname
    let hostname = get_hostname(data).unwrap_or_else(||
        hostname::get().map(|s| s.to_string_lossy().into()).unwrap_or_default());
    write_str_to_file(format!("{}/{}", shared, HOSTNAME_FILENAME), &(hostname + "\n")).await?;

    // hosts: copy /etc/hosts from host
    tokio::fs::copy(ETC_HOSTS, format!("{}/{}", shared, HOSTS_FILENAME)).await?;

    // resolv.conf: from DNS config in SandboxData, or copy host /etc/resolv.conf
    match get_dns_config(data) {
        Some(dns) if !dns.servers.is_empty() || !dns.searches.is_empty() =>
            write_str_to_file(format!("{}/{}", shared, RESOLV_FILENAME),
                              &format_resolv_conf(&dns)).await?,
        _ => { tokio::fs::copy(ETC_RESOLV, format!("{}/{}", shared, RESOLV_FILENAME)).await?; }
    }
    Ok(())
}
```

#### `vcpu_count_from_resources`

```rust
/// Derive the vCPU count from pod resource limits.
/// Computes ceil(cpu_quota / cpu_period); falls back to 1 if limits are absent or zero.
fn vcpu_count_from_resources(data: &SandboxData) -> u32 {
    if let Some(res) = get_resources(data) {
        if res.cpu_period > 0 && res.cpu_quota > 0 {
            return (res.cpu_quota as f64 / res.cpu_period as f64).ceil() as u32;
        }
    }
    1
}
```

#### `deference_container_storages`

```rust
/// Remove `container_id` from the ref_containers of each StorageMount.
/// Only unmounts and hot-detaches when ref_containers becomes empty —
/// supports multiple containers sharing the same hostPath/configmap volume.
/// Mirrors the ref-counted `deference_container_storages` in KuasarSandbox (vmm/sandbox).
async fn deference_container_storages(
    inst: &mut SandboxInstance<impl Vmm>,
    container_id: &str,
) -> Result<()> {
    let mut remaining = vec![];
    for mut sm in inst.storages.drain(..) {
        sm.ref_containers.retain(|c| c != container_id);
        if !sm.ref_containers.is_empty() {
            // Other containers still reference this mount — keep it
            remaining.push(sm);
            continue;
        }
        // Last reference removed — unmount the bind-mount destination (not the source),
        // then hot-detach any associated block device.
        if let Some(ref dest) = sm.mount_dest {
            vmm_common::mount::unmount(dest, MNT_DETACH).ok();
            if tokio::fs::metadata(dest).await
                .map(|m| m.is_dir()).unwrap_or(false) {
                tokio::fs::remove_dir(dest).await.ok();
            } else {
                tokio::fs::remove_file(dest).await.ok();
            }
        }
        if let Some(dev_id) = sm.device_id {
            inst.vmm.hot_detach(&dev_id).await.ok();
        }
    }
    inst.storages = remaining;
    Ok(())
}
```

#### `start_sandbox`

```rust
/// Start a sandbox. The sandbox must be in Creating state.
pub async fn start_sandbox(&self, id: &str) -> Result<StartResult> {
    let instance_mutex = self.get_sandbox(id).await?;
    let t0 = Instant::now();

    let mut instance = instance_mutex.lock().await;

    // Guard: must be in Creating state
    if instance.state != SandboxState::Creating {
        return Err(Error::InvalidState(format!(
            "sandbox {} must be in Creating state to start, got {:?}", id, instance.state
        )));
    }

    // 1. pre_start hook: backend applies resource limits, annotations, etc. to VMM config.
    //    Hook receives SandboxCtx with vmm (concrete type) and data (pod spec).
    {
        let mut ctx = SandboxCtx { vmm: &mut instance.vmm, data: &mut instance.data, base_dir: &instance.base_dir };
        self.hooks.pre_start(&mut ctx).await.map_err(|e| {
            instance.state = SandboxState::Stopped; e
        })?;
    }

    // vcpu_count derived here for network queue sizing (independent of hook).
    let vcpu = vcpu_count_from_resources(&instance.data);

    // 2. Prepare network: enter pod netns, discover interfaces + routes,
    //    attach each tap device to the VMM before boot.
    if !instance.netns.is_empty() {
        let network = prepare_network(&instance.netns, &instance.id, vcpu).await
            .map_err(|e| { instance.state = SandboxState::Stopped; e })?;
        for iface in &network.interfaces {
            instance.vmm.add_network(VmmNetworkConfig {
                tap_device: iface.name.clone(),
                mac: iface.mac.clone(),
                queue: vcpu,
            })?;
        }
        instance.network = Some(network);
    }

    // 3. Boot VMM
    let t = Instant::now();
    instance.vmm.boot().await.map_err(|e| {
        instance.state = SandboxState::Stopped;
        e
    })?;
    let vmm_start_ms = t.elapsed().as_millis() as u64;

    let vsock = instance.vmm.vsock_path()?;
    let pids = instance.vmm.pids();
    let setup_req = SandboxSetupRequest {
        interfaces: instance.network.as_ref()
            .map(|n| n.interfaces.clone()).unwrap_or_default(),
        routes: instance.network.as_ref()
            .map(|n| n.routes.clone()).unwrap_or_default(),
        sandbox_data: instance.data.clone(),
    };
    let exit_signal = instance.exit_signal.clone();
    drop(instance); // release lock during potentially-long wait_ready

    // 4. Wait for guest readiness (connect + ttrpc Check)
    let wait_result = tokio::time::timeout(
        Duration::from_millis(self.config.ready_timeout_ms),
        self.runtime.wait_ready(id, &vsock),
    ).await;

    let _ready = match wait_result {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            let mut inst = instance_mutex.lock().await;
            inst.state = SandboxState::Stopped;
            inst.dump().await.ok();
            return Err(e);
        }
        Err(_) => {
            let mut inst = instance_mutex.lock().await;
            inst.state = SandboxState::Stopped;
            inst.dump().await.ok();
            return Err(Error::Timeout("ready_timeout".into()));
        }
    };

    // 5. Send network + pod config to guest via SetupSandbox
    self.runtime.setup_sandbox(id, &setup_req).await.map_err(|e| {
        // non-fatal: log and continue; guest may still function
        tracing::warn!(sandbox_id=%id, err=%e, "setup_sandbox failed");
        e
    })?;

    // 6. Apply state transition, run post_start hook, persist
    {
        let mut inst = instance_mutex.lock().await;
        // Re-validate state: a concurrent stop_sandbox/delete_sandbox may have raced us
        // while the lock was released during wait_ready. Abort if state is no longer Creating.
        if inst.state != SandboxState::Creating {
            return Err(Error::InvalidState(format!(
                "sandbox {} state changed to {:?} during boot", id, inst.state
            )));
        }
        inst.state = SandboxState::Running;

        // post_start hook: set task_address, publish metrics, etc.
        // Default impl sets ctx.data.task_address = ctx.vmm.task_address().
        let mut ctx = SandboxCtx { vmm: &mut inst.vmm, data: &mut inst.data, base_dir: &inst.base_dir };
        self.hooks.post_start(&mut ctx).await.ok();

        // Add VMM process + vcpu threads + affiliated pids to host cgroup
        if !cgroups_rs::hierarchies::is_cgroup2_unified_mode() {
            if let Ok(vcpu_threads) = inst.vmm.vcpus().await {
                inst.cgroup.add_process_into_sandbox_cgroups(
                    pids.vmm_pid.unwrap_or(0), Some(vcpu_threads)).ok();
            }
            for pid in &pids.affiliated_pids {
                inst.cgroup.add_process_into_sandbox_cgroups(*pid, None).ok();
            }
        }
        inst.dump().await?;
    }

    // 7. Monitor VMM exit (fires exit_signal on unexpected exit → Stopped)
    //    subscribe_exit() returns a watch receiver held outside the Mutex — no lock held
    //    during the wait, so other operations can proceed concurrently.
    let exit_rx = instance_mutex.lock().await.vmm.subscribe_exit();
    self.monitor_vmm_exit(id, instance_mutex.clone(), exit_rx);

    // 8. Forward OOM/exit events from vmm-task to containerd, and start clock sync.
    //    Both run until exit_signal fires. Replaces post_start hook's sync_clock() call.
    self.runtime.forward_events(id, exit_signal).await;

    Ok(StartResult {
        ready_ms: t0.elapsed().as_millis() as u64,
        vmm_start_ms,
    })
}
```

#### `prepare_network`

```rust
/// Enter the pod's network namespace and discover interfaces + routes.
/// Returns a NetworkState whose interfaces map 1:1 to tap devices in the netns.
/// Also derives the VmmNetworkConfig (tap name + MAC + queue count) for each interface,
/// to be passed to vmm.add_network() before boot.
async fn prepare_network(netns: &str, sandbox_id: &str, queue: u32) -> Result<NetworkState> {
    // Delegates to vmm_common::network::discover_network_from_netns()
    // which opens the netns, enumerates interfaces via netlink, and returns
    // (Vec<NetworkInterface>, Vec<Route>).
    let (interfaces, routes) =
        vmm_common::network::discover_from_netns(netns, sandbox_id, queue).await?;
    Ok(NetworkState { interfaces, routes })
}
```

#### `monitor_vmm_exit`

```rust
/// Spawn a background task that waits on the VMM exit receiver.
/// When the VMM exits unexpectedly, transitions the sandbox to Stopped and fires exit_signal.
/// The receiver is obtained via Vmm::subscribe_exit() BEFORE releasing the lock, so it is
/// held outside the Mutex — the task never needs to acquire the sandbox lock to wait.
fn monitor_vmm_exit(
    &self,
    id: &str,
    instance_mutex: Arc<Mutex<SandboxInstance<V>>>,
    mut exit_rx: tokio::sync::watch::Receiver<Option<ExitInfo>>,
) {
    let id = id.to_string();
    let sandboxes = self.sandboxes.clone();
    tokio::spawn(async move {
        // Wait until the exit channel delivers Some(ExitInfo)
        loop {
            if exit_rx.changed().await.is_err() { break; }
            if exit_rx.borrow().is_some() { break; }
        }
        // Acquire lock only after exit is detected — no contention with ongoing operations
        let mut inst = instance_mutex.lock().await;
        if inst.state == SandboxState::Running {
            tracing::warn!("sandbox {} VMM exited unexpectedly; marking Stopped", id);
            inst.state = SandboxState::Stopped;
            inst.exit_signal.signal();
        }
        // Remove from map so subsequent create_sandbox with same id can succeed
        sandboxes.write().await.remove(&id);
    });
}
```

#### `stop_sandbox`

```rust
pub async fn stop_sandbox(&self, id: &str, force: bool) -> Result<()> {
    let instance_mutex = self.get_sandbox(id).await?;
    let mut instance = instance_mutex.lock().await;

    // Idempotent: containerd may call StopSandbox multiple times (e.g. on retry).
    // Mirrors KuasarSandbox::stop() which returns Ok(()) when already Stopped.
    if instance.state == SandboxState::Stopped {
        return Ok(());
    }

    // Validate state: Deleted is still an error (no resurrection after delete)
    let new_state = instance.state.transition(StateEvent::Stop)?;

    // Stop or forcibly remove all containers before stopping the VM
    let container_ids: Vec<String> = instance.containers.keys().cloned().collect();
    for cid in container_ids {
        if force {
            // best-effort hot-detach of IO devices
            if let Some(c) = instance.containers.remove(&cid) {
                for dev_id in c.io_devices {
                    instance.vmm.hot_detach(&dev_id).await.ok();
                }
            }
        } else {
            // graceful: signal container process (callers should have done this already)
        }
    }

    // pre_stop hook: graceful shutdown notification, snapshot save, etc.
    // Only called when !force to preserve force-kill semantics.
    if !force {
        let mut ctx = SandboxCtx { vmm: &mut instance.vmm, data: &mut instance.data, base_dir: &instance.base_dir };
        self.hooks.pre_stop(&mut ctx).await.ok();
    }

    instance.vmm.stop(force).await?;

    // post_stop hook: remove sockets, release host devices, etc.
    {
        let mut ctx = SandboxCtx { vmm: &mut instance.vmm, data: &mut instance.data, base_dir: &instance.base_dir };
        self.hooks.post_stop(&mut ctx).await.ok();
    }

    // Destroy network (only once — take() prevents double-destroy on recovery)
    if let Some(mut net) = instance.network.take() {
        vmm_common::network::destroy_network(&mut net).await;
    }

    instance.state = new_state;
    instance.dump().await?;
    Ok(())
}
```

#### `delete_sandbox`

```rust
pub async fn delete_sandbox(&self, id: &str, force: bool) -> Result<()> {
    let instance_mutex = self.get_sandbox(id).await?;
    {
        let mut instance = instance_mutex.lock().await;
        let event = if force { StateEvent::ForceDelete } else { StateEvent::Delete };
        instance.state.transition(event)?;

        // Force-stop if force=true and VMM is still running
        if force {
            instance.vmm.stop(true).await.ok();
        }
        // Remove host-side cgroups
        if !cgroups_rs::hierarchies::is_cgroup2_unified_mode() {
            instance.cgroup.remove_sandbox_cgroups().ok();
        }
        // Unmount any remaining virtiofs/storage mounts
        containerd_sandbox::utils::cleanup_mounts(&instance.base_dir).await.ok();
        tokio::fs::remove_dir_all(&instance.base_dir).await.ok();
    }
    self.sandboxes.write().await.remove(id);
    Ok(())
}
```

#### `get_sandbox` / `list_sandboxes`

```rust
pub async fn get_sandbox(&self, id: &str)
    -> Result<Arc<Mutex<SandboxInstance<V>>>> {
    self.sandboxes.read().await
        .get(id)
        .cloned()
        .ok_or_else(|| Error::NotFound(id.to_string()))
}

pub async fn list_sandboxes(&self) -> Vec<SandboxSummary> {
    self.sandboxes.read().await
        .values()
        .map(|m| {
            let g = m.blocking_lock();
            SandboxSummary { id: g.id.clone(), state: g.state.clone() }
        })
        .collect()
}
```

#### Recovery

```rust
/// Re-attach engine state from the work directory after process restart.
/// Reads persisted SandboxInstance JSON for each subdirectory.
pub async fn recover(&self, work_dir: &str) {
    let mut dir = match tokio::fs::read_dir(work_dir).await {
        Ok(d) => d,
        Err(e) => { tracing::error!("recovery: cannot read {}: {}", work_dir, e); return; }
    };
    while let Some(entry) = dir.next_entry().await.unwrap_or(None) {
        if !entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        match SandboxInstance::<V>::load(&path).await {
            Ok(mut inst) => {
                // Re-attach VMM process monitoring for Running sandboxes
                if inst.state == SandboxState::Running {
                    if let Err(e) = inst.vmm.recover().await {
                        tracing::warn!("recovery: vmm reconnect failed for {}: {}", inst.id, e);
                        inst.state = SandboxState::Stopped;
                    } else {
                        // Re-connect ttrpc client, re-start clock sync, re-start event forwarding
                        let vsock = inst.vmm.vsock_path().unwrap_or_default();
                        let exit_signal = inst.exit_signal.clone();
                        if let Err(e) = self.runtime.wait_ready(&inst.id, &vsock).await {
                            tracing::warn!("recovery: wait_ready failed for {}: {}", inst.id, e);
                            // VMM process is alive but guest is unreachable — force-stop to
                            // release resources. Sandbox will be inserted below as Stopped.
                            inst.vmm.stop(true).await.ok();
                            inst.state = SandboxState::Stopped;
                            // fall through: no monitor spawned, no continue
                        } else {
                            self.runtime.forward_events(&inst.id, exit_signal).await;
                            // Recover cgroup handles (skeletons already exist on disk).
                            // Only done on full recovery success — no point if we're Stopped.
                            inst.cgroup = SandboxCgroup::create_sandbox_cgroups(
                                &inst.cgroup.cgroup_parent_path, &inst.id)
                                .unwrap_or_default();
                            // Start exit monitor ONLY after wait_ready succeeds:
                            // if we started it earlier and wait_ready then failed, we'd have
                            // a monitor task watching a sandbox that is already Stopped.
                            let exit_rx = inst.vmm.subscribe_exit();
                            let inst_arc = Arc::new(Mutex::new(inst.clone()));
                            self.monitor_vmm_exit(&inst.id, inst_arc.clone(), exit_rx);
                            self.sandboxes.write().await.insert(inst.id.clone(), inst_arc);
                            continue;
                        }
                    }
                }
                self.sandboxes.write().await
                    .insert(inst.id.clone(), Arc::new(Mutex::new(inst)));
            }
            Err(e) => tracing::warn!("recovery: skip {:?}: {}", path, e),
        }
    }
}
```

### Key types for engine public API

```rust
// engine/src/lib.rs — public API surface

pub struct CreateSandboxRequest {
    pub sandbox_id: String,
    pub sandbox_data: SandboxData,
    pub netns: String,
    pub cgroup_parent: String,      // "" → use DEFAULT_CGROUP_PARENT_PATH
    pub rootfs_disk: Option<DiskConfig>,
}

pub struct StartResult {
    pub ready_ms: u64,
    pub vmm_start_ms: u64,
}
```

### Configuration types

```rust
// engine/src/config.rs

#[derive(Deserialize)]
pub struct EngineConfig {
    pub work_dir: String,
    pub ready_timeout_ms: u64,
}
```

---

## Story 1.4 — `adapter-k8s` crate

**Crate name**: `vmm-adapter-k8s`
**Path**: `vmm/adapter-k8s/src/`
**Dependencies**: `vmm-engine`, `vmm-vm-trait`, `vmm-guest-runtime`, `containerd-sandbox`, `containerd-shim`, `vmm-common`, `async-trait`, `tokio`

> `vmm-vm-trait` is required directly for the `H: Hooks<V>` bound on `K8sAdapter<V, R, H>`.

### `K8sAdapter<V, R, H>`

```rust
// adapter-k8s/src/lib.rs

/// K8s Adapter: wraps SandboxEngine and implements containerd shimv2 + Task API.
pub struct K8sAdapter<V: Vmm, R: ContainerRuntime, H: Hooks<V>> {
    engine: Arc<SandboxEngine<V, R, H>>,
    graceful_stop_timeout_ms: u64,
    publisher: Option<RemotePublisher>,
}

impl<V: Vmm, R: ContainerRuntime, H: Hooks<V>> K8sAdapter<V, R, H> {
    pub fn new(engine: SandboxEngine<V, R, H>, config: K8sAdapterConfig) -> Self {
        Self {
            engine: Arc::new(engine),
            graceful_stop_timeout_ms: config.graceful_stop_timeout_ms,
            publisher: None,
        }
    }

    /// Start serving the containerd Sandbox API on the given socket.
    pub async fn serve(self, listen: &str, dir: &str) -> Result<()> {
        containerd_sandbox::run("kuasar-vmm-sandboxer", listen, dir, self).await
    }
}
```

### `impl Sandboxer for K8sAdapter<V, R, H>`

```rust
// adapter-k8s/src/sandboxer.rs

#[async_trait]
impl<V, R, H> Sandboxer for K8sAdapter<V, R, H>
where
    V: Vmm + Serialize + DeserializeOwned + 'static,
    R: GuestReadiness + ContainerRuntime + 'static,
    H: Hooks<V> + 'static,
{
    type Sandbox = K8sSandboxView<V, R, H>;

    async fn create(&self, id: &str, s: SandboxOption) -> Result<()> {
        let req = self.parse_create_request(id, s)?;
        self.engine.create_sandbox(id, req).await
    }

    async fn start(&self, id: &str) -> Result<()> {
        let result = self.engine.start_sandbox(id).await?;
        // Publish containerd event
        self.publish_event(TaskStart { pid: 1, container_id: id.to_string() }).await;
        tracing::info!(sandbox_id=%id, ready_ms=%result.ready_ms, "sandbox started");
        Ok(())
    }

    async fn update(&self, id: &str, data: SandboxData) -> Result<()> {
        let inst = self.engine.get_sandbox(id).await?;
        let mut inst = inst.lock().await;
        inst.data = data;
        inst.dump().await
    }

    async fn stop(&self, id: &str, force: bool) -> Result<()> {
        self.engine.stop_sandbox(id, force).await
    }

    async fn delete(&self, id: &str) -> Result<()> {
        self.engine.delete_sandbox(id, false).await
    }

    async fn sandbox(&self, id: &str) -> Result<Arc<Mutex<Self::Sandbox>>> {
        let inst_mutex = self.engine.get_sandbox(id).await?;
        let inst = inst_mutex.lock().await;
        // Snapshot current containers into the view's local cache
        let containers = inst.containers.iter()
            .map(|(cid, cs)| (cid.clone(), K8sContainer { data: cs.data.clone() }))
            .collect();
        let view = K8sSandboxView {
            engine: self.engine.clone(),
            id: id.to_string(),
            containers,
        };
        Ok(Arc::new(Mutex::new(view)))
    }
}
```

### `impl TaskService for K8sAdapter<V, R, H>`

The Task API is served by the same process. Calls are delegated to `R: ContainerRuntime` (i.e., `VmmTaskRuntime`) via the engine.

```rust
// adapter-k8s/src/task.rs

#[async_trait]
impl<V, R, H> TaskService for K8sAdapter<V, R, H>
where
    V: Vmm + 'static,
    R: GuestReadiness + ContainerRuntime + 'static,
    H: Hooks<V> + 'static,
{
    async fn create(&self, req: CreateTaskRequest) -> Result<CreateTaskResponse> {
        let info = self.engine.runtime()
            .create_container(&req.id, req.into()).await?;
        Ok(CreateTaskResponse { pid: info.pid })
    }

    async fn start(&self, req: StartRequest) -> Result<StartResponse> {
        let info = self.engine.runtime()
            .start_process(&req.id, &req.exec_id).await?;
        Ok(StartResponse { pid: info.pid })
    }

    async fn exec(&self, req: ExecProcessRequest) -> Result<()> {
        self.engine.runtime()
            .exec_process(&req.id, &req.exec_id, req.into())
            .await
    }

    async fn kill(&self, req: KillRequest) -> Result<()> {
        self.engine.runtime()
            .kill_process(&req.id, &req.exec_id, req.pid, req.signal)
            .await
    }

    async fn wait(&self, req: WaitRequest) -> Result<WaitResponse> {
        let exit = self.engine.runtime()
            .wait_process(&req.id, &req.exec_id, req.pid).await?;
        Ok(WaitResponse { exit_status: exit.exit_code as u32, exited_at: exit.exited_at_ms })
    }

    async fn stats(&self, req: StatsRequest) -> Result<StatsResponse> {
        let stats = self.engine.runtime()
            .container_stats(&req.id, &req.exec_id).await?;
        Ok(StatsResponse { stats: stats.into() })
    }
}
```

### `K8sContainer`

```rust
// adapter-k8s/src/sandboxer.rs

/// View of a single container, returned by Sandbox::container().
/// Implements the containerd Container trait by delegating to engine state.
#[derive(Clone)]
pub struct K8sContainer {
    pub data: ContainerData,
}

impl Container for K8sContainer {
    fn get_data(&self) -> Result<ContainerData> {
        Ok(self.data.clone())
    }
}
```

### `K8sSandboxView`

```rust
/// Projection of a single sandbox for the Sandbox trait.
/// Holds a local container cache so that `container()` can return &Self::Container.
/// The cache is always in sync: `append_container` inserts, `remove_container` removes.
///
/// The view itself is behind Arc<Mutex<>> (from Sandboxer::sandbox()), so &self
/// is already the lock holder — the cache borrow is safe for the lifetime of the call.
pub struct K8sSandboxView<V: Vmm, R: ContainerRuntime, H: Hooks<V>> {
    engine: Arc<SandboxEngine<V, R, H>>,
    id: String,
    /// Local mirror of ContainerState → K8sContainer, kept in sync with engine state.
    containers: HashMap<String, K8sContainer>,
}
```

`Sandboxer::sandbox()` populates the cache on construction:

```rust
async fn sandbox(&self, id: &str) -> Result<Arc<Mutex<Self::Sandbox>>> {
    let inst_mutex = self.engine.get_sandbox(id).await?;
    let inst = inst_mutex.lock().await;
    // Snapshot current containers into the view's local cache
    let containers = inst.containers.iter()
        .map(|(cid, cs)| (cid.clone(), K8sContainer { data: cs.data.clone() }))
        .collect();
    let view = K8sSandboxView {
        engine: self.engine.clone(),
        id: id.to_string(),
        containers,
    };
    Ok(Arc::new(Mutex::new(view)))
}
```

### `impl Sandbox for K8sSandboxView<V, R, H>`

`K8sSandboxView` proxies lifecycle operations through the engine's `SandboxInstance`.
Container operations keep the local `containers` cache in sync so that `container()` can
return `&Self::Container` without holding the engine lock.

```rust
#[async_trait]
impl<V, R, H> Sandbox for K8sSandboxView<V, R, H>
where
    V: Vmm + Serialize + DeserializeOwned + 'static,
    R: GuestReadiness + ContainerRuntime + 'static,
    H: Hooks<V> + 'static,
{
    type Container = K8sContainer;

    fn status(&self) -> Result<SandboxStatus> {
        // `Sandbox::status()` is a sync method in the containerd trait — async is not possible.
        // `blocking_lock()` is acceptable here: containerd calls status() infrequently
        // (health-check cadence), the critical section is trivial (field read), and the
        // tokio thread pool absorbs the brief block. If this becomes a bottleneck, the
        // containerd Sandbox trait would need to be changed to async first.
        let inst = self.engine.get_sandbox_blocking(&self.id)?;
        Ok(inst.state.into())
    }

    async fn ping(&self) -> Result<()> {
        let inst = self.engine.get_sandbox(&self.id).await?;
        inst.lock().await.vmm.ping().await
    }

    /// Returns from the local cache — no engine lock required.
    async fn container(&self, id: &str) -> Result<&Self::Container> {
        self.containers.get(id)
            .ok_or_else(|| Error::NotFound(id.to_string()))
    }

    /// Mirrors KuasarSandbox::append_container handler chain.
    ///
    /// **Host-side responsibilities (this function):**
    ///   1. Create bundle directory under the shared virtiofs path.
    ///   2. Process rootfs / bind-mount / block-device storage mounts
    ///      (`attach_container_storages`).
    ///   3. Hot-attach IO pipes (stdin/stdout/stderr) as CharDevice or VsockMuxIO
    ///      (`attach_io_pipes`).
    ///   4. Persist updated SandboxInstance and update the local container cache.
    ///
    /// **Guest-side responsibilities (vmm-task, via ContainerRuntime::create_container):**
    ///   - OCI spec `has_shared_pid_namespace` adjustment (pid namespace sharing).
    ///   - UTS / IPC namespace configuration.
    ///   - Overlay filesystem setup for the container rootfs.
    ///   - Ephemeral storage (tmpfs / shm) mount setup.
    ///   - Final OCI spec delivery to the container runtime inside the VM.
    ///
    /// Epic 1 host-side scope: steps 1–4 only. Guest-side steps are unchanged from the
    /// existing vmm-task implementation and require no spec changes here.
    async fn append_container(&mut self, id: &str, options: ContainerOption) -> Result<()> {
        let inst_mutex = self.engine.get_sandbox(&self.id).await?;
        let mut inst = inst_mutex.lock().await;

        // 1. Create bundle dir in virtiofs shared directory
        let bundle = format!("{}/{}/{}", inst.base_dir, SHARED_DIR_SUFFIX, id);
        tokio::fs::create_dir_all(&bundle).await?;

        let mut data = options.container.clone();
        data.bundle = bundle;

        let mut io_devices: Vec<String> = vec![];

        // 2. Process storage mounts (rootfs overlay/bind + extra mounts)
        //    Block devices are hot-plugged; bind mounts are bind-mounted into the shared dir.
        //    Each produces a StorageMount entry stored in inst.storages.
        attach_container_storages(&mut inst, id, &mut data).await?;

        // 3. Hot-attach IO pipes as CharDevice (stdin / stdout / stderr named pipes)
        //    Each pipe host path becomes a virtio-serial port in the guest; the port name
        //    (chardev_id) is written back into data.io so vmm-task can find it.
        if let Some(io) = &data.io.clone() {
            attach_io_pipes(&mut inst, id, io, &mut io_devices, &mut data).await?;
        }

        let container = ContainerState {
            id: id.to_string(),
            data: data.clone(),
            io_devices,
            processes: vec![],
        };
        inst.containers.insert(id.to_string(), container);
        inst.dump().await?;

        // Update local cache
        self.containers.insert(id.to_string(), K8sContainer { data });
        Ok(())
    }

    async fn update_container(&mut self, id: &str, options: ContainerOption) -> Result<()> {
        let inst_mutex = self.engine.get_sandbox(&self.id).await?;
        let mut inst = inst_mutex.lock().await;
        if let Some(c) = inst.containers.get_mut(id) {
            c.data = options.container.clone();
        }
        inst.dump().await?;
        // Update local cache
        if let Some(c) = self.containers.get_mut(id) {
            c.data = options.container.clone();
        }
        Ok(())
    }

    async fn remove_container(&mut self, id: &str) -> Result<()> {
        let inst_mutex = self.engine.get_sandbox(&self.id).await?;
        let mut inst = inst_mutex.lock().await;

        // Unmount and hot-detach all StorageMounts for this container
        deference_container_storages(&mut inst, id).await?;

        // Remove container bundle directory from the shared virtiofs path
        let bundle = format!("{}/{}/{}", inst.base_dir, SHARED_DIR_SUFFIX, id);
        tokio::fs::remove_dir_all(&bundle).await.ok();

        // Hot-detach IO CharDevices
        if let Some(c) = inst.containers.remove(id) {
            for dev_id in c.io_devices {
                inst.vmm.hot_detach(&dev_id).await.ok();
            }
        }
        inst.dump().await?;

        // Update local cache
        self.containers.remove(id);
        Ok(())
    }

    async fn exit_signal(&self) -> Result<Arc<ExitSignal>> {
        let inst = self.engine.get_sandbox(&self.id).await?;
        Ok(inst.lock().await.exit_signal.clone())
    }

    fn get_data(&self) -> Result<SandboxData> {
        let inst = self.engine.get_sandbox_blocking(&self.id)?;
        Ok(inst.data.clone())
    }
}
```

### Storage and IO helpers

These are free functions in `adapter-k8s/src/sandboxer.rs`.

```rust
/// Attach storage mounts for a container:
/// - Overlay / bind mounts: bind-mount the host source into the shared virtiofs directory,
///   record a StorageMount so the guest side knows where to find them.
/// - Block devices: hot-plug as VirtioBlock, record device_id in StorageMount.
/// Mirrors KuasarSandbox's StorageHandler + handle_block_device / handle_bind_mount logic.
async fn attach_container_storages(
    inst: &mut SandboxInstance<impl Vmm>,
    container_id: &str,
    data: &mut ContainerData,
) -> Result<()> {
    let mounts = data.spec.as_ref()
        .map(|s| s.mounts.clone()).unwrap_or_default();
    let rootfs = data.rootfs.clone();

    for m in mounts.iter().chain(rootfs.iter()) {
        if is_block_device(&m.source).await? {
            // Dedup: if this block device is already hot-plugged for another container,
            // just add this container to its ref_containers instead of hot-plugging again.
            if let Some(existing) = inst.storages.iter_mut()
                .find(|s| s.host_path == m.source && s.kind == StorageMountKind::Block)
            {
                if !existing.ref_containers.contains(&container_id.to_string()) {
                    existing.ref_containers.push(container_id.to_string());
                }
                continue;
            }
            // First user of this block device: hot-plug and record.
            inst.id_generator += 1;
            let dev_id = format!("blk{}", inst.id_generator);
            let result = inst.vmm.hot_attach(HotPlugDevice::VirtioBlock {
                id: dev_id.clone(),
                path: m.source.clone(),
                read_only: m.options.contains(&"ro".to_string()),
            }).await?;
            let guest_path = format!("{}{}", KUASAR_GUEST_SHARE_DIR, dev_id);
            inst.storages.push(StorageMount {
                id: dev_id.clone(),
                ref_containers: vec![container_id.to_string()],
                host_path: m.source.clone(),  // original source path, used for dedup lookup
                mount_dest: None,             // block devices use hot_detach, not umount
                guest_path,
                kind: StorageMountKind::Block,
                device_id: Some(result.device_id),
            });
        } else if is_bind_mount(m) {
            // Dedup: if this host source path is already bind-mounted into the shared dir
            // for another container, reuse that mount instead of creating a second one.
            if let Some(existing) = inst.storages.iter_mut()
                .find(|s| s.host_path == m.source && s.kind == StorageMountKind::VirtioFs)
            {
                if !existing.ref_containers.contains(&container_id.to_string()) {
                    existing.ref_containers.push(container_id.to_string());
                }
                continue;
            }
            // First user of this source path: bind-mount into shared dir and record.
            inst.id_generator += 1;
            let storage_id = format!("storage{}", inst.id_generator);
            let host_dest = format!("{}/{}/{}", inst.base_dir, SHARED_DIR_SUFFIX, storage_id);
            bind_mount_into_shared(&m.source, &host_dest).await?;
            let guest_path = format!("{}{}", KUASAR_GUEST_SHARE_DIR, storage_id);
            inst.storages.push(StorageMount {
                id: storage_id,
                ref_containers: vec![container_id.to_string()],
                host_path: m.source.clone(),  // original source path, used for dedup lookup
                mount_dest: Some(host_dest),  // bind-mount destination to be unmounted later
                guest_path,
                kind: StorageMountKind::VirtioFs,
                device_id: None,
            });
        }
        // tmpfs, shm, overlay handled by vmm-task; skip on host side
    }
    Ok(())
}

/// Attach container stdio using the IO model appropriate for this backend.
/// Checks `capabilities().virtio_serial` to choose between:
///   - virtio-serial CharDevice (CH / QEMU / StratoVirt)
///   - vsock port multiplexing (Firecracker)
async fn attach_io_pipes(
    inst: &mut SandboxInstance<impl Vmm>,
    container_id: &str,
    io: &containerd_sandbox::data::Io,
    io_devices: &mut Vec<String>,
    data: &mut ContainerData,
) -> Result<()> {
    if inst.vmm.capabilities().virtio_serial {
        attach_io_pipes_char(inst, container_id, io, io_devices, data).await
    } else {
        attach_io_vsock_mux(inst, container_id, io, io_devices, data).await
    }
}

/// virtio-serial path: hot-attach one CharDevice per non-empty stdio pipe.
/// The guest-side port name (chardev_id) is written back into ContainerData.io
/// so vmm-task can open the correct virtio-serial port.
/// Mirrors KuasarSandbox::hot_attach_pipe + IoHandler logic.
async fn attach_io_pipes_char(
    inst: &mut SandboxInstance<impl Vmm>,
    _container_id: &str,
    io: &containerd_sandbox::data::Io,
    io_devices: &mut Vec<String>,
    data: &mut ContainerData,
) -> Result<()> {
    let mut new_io = io.clone();

    if !io.stdin.is_empty() && !io.stdin.contains("://") {
        let (dev_id, chardev_id) = hot_attach_pipe(inst, &io.stdin).await?;
        io_devices.push(dev_id);
        new_io.stdin = chardev_id;
    }
    if !io.stdout.is_empty() && !io.stdout.contains("://") {
        let (dev_id, chardev_id) = hot_attach_pipe(inst, &io.stdout).await?;
        io_devices.push(dev_id);
        new_io.stdout = chardev_id;
    }
    if !io.stderr.is_empty() && !io.stderr.contains("://") {
        let (dev_id, chardev_id) = hot_attach_pipe(inst, &io.stderr).await?;
        io_devices.push(dev_id);
        new_io.stderr = chardev_id;
    }

    data.io = Some(new_io);
    Ok(())
}

/// vsock-mux path (Firecracker): allocate one vsock port for this container's stdio.
/// The port number is written back into ContainerData.io (reusing the stdin field
/// as a "vsock://:<port>" URI) so vmm-task can multiplex the streams.
/// Full guest-protocol spec is deferred to Firecracker integration (future epic).
async fn attach_io_vsock_mux(
    inst: &mut SandboxInstance<impl Vmm>,
    container_id: &str,
    _io: &containerd_sandbox::data::Io,
    io_devices: &mut Vec<String>,
    data: &mut ContainerData,
) -> Result<()> {
    let port = inst.vsock_port_next;
    inst.vsock_port_next += 1;
    let dev_id = format!("vsockmux{}", port);
    inst.vmm.hot_attach(HotPlugDevice::VsockMuxIO {
        id: dev_id.clone(),
        container_id: container_id.to_string(),
        port,
    }).await?;
    io_devices.push(dev_id);
    // Encode the allocated port as a URI; vmm-task reads this to route container IO.
    let vsock_uri = format!("vsock://:{}", port);
    data.io = Some(containerd_sandbox::data::Io {
        stdin:    vsock_uri.clone(),
        stdout:   vsock_uri.clone(),
        stderr:   vsock_uri.clone(),
        terminal: data.io.as_ref().map(|i| i.terminal).unwrap_or(false),
    });
    Ok(())
}

/// Hot-attach one named pipe as a virtio-serial CharDevice.
/// Returns (device_id, chardev_id) — device_id is stored for later hot_detach;
/// chardev_id is the guest-visible port name passed to vmm-task.
async fn hot_attach_pipe(
    inst: &mut SandboxInstance<impl Vmm>,
    path: &str,
) -> Result<(String, String)> {
    inst.id_generator += 1;
    let n = inst.id_generator;
    let device_id  = format!("virtioserial{}", n);
    let chardev_id = format!("chardev{}", n);
    inst.vmm.hot_attach(HotPlugDevice::CharDevice {
        id: device_id.clone(),
        chardev_id: chardev_id.clone(),
        name: chardev_id.clone(),
        path: path.to_string(),
    }).await?;
    Ok((device_id, chardev_id))
}
```

---

## Story 1.5 — `runtime-vmm-task` crate

**Crate name**: `vmm-runtime-vmm-task`
**Path**: `vmm/runtime-vmm-task/src/lib.rs`
**Source**: Extracted from `vmm/sandbox/src/client.rs`
**Dependencies**: `vmm-guest-runtime`, `vmm-common`, `ttrpc`, `async-trait`, `tokio`, `anyhow`

### `VmmTaskRuntime`

```rust
pub struct VmmTaskRuntime {
    config: VmmTaskConfig,
    // Per-sandbox ttrpc clients, keyed by sandbox_id.
    // Arc<Mutex<>> because wait_ready creates the client and subsequent calls reuse it.
    clients: Arc<RwLock<HashMap<String, Arc<Mutex<SandboxServiceClient>>>>>,
}

#[derive(Clone, Deserialize)]
pub struct VmmTaskConfig {
    pub connect_timeout_ms: u64,   // default: 45_000
    pub ttrpc_timeout_ms: u64,     // default: 10_000
}

impl VmmTaskRuntime {
    pub fn new(config: VmmTaskConfig) -> Self {
        Self { config, clients: Arc::new(RwLock::new(HashMap::new())) }
    }

    /// Connect to vmm-task via the vsock address and cache the ttrpc client.
    async fn connect(&self, sandbox_id: &str, vsock_path: &str)
        -> Result<Arc<Mutex<SandboxServiceClient>>> {
        // vsock_path: "hvsock://<path>:1024" (CH) or "vsock://<cid>:1024" (FC)
        let client = new_ttrpc_client_with_timeout(vsock_path, self.config.connect_timeout_ms).await?;
        let entry = Arc::new(Mutex::new(SandboxServiceClient::new(client)));
        self.clients.write().await.insert(sandbox_id.to_string(), entry.clone());
        Ok(entry)
    }

    async fn get_client(&self, sandbox_id: &str)
        -> Result<Arc<Mutex<SandboxServiceClient>>> {
        self.clients.read().await
            .get(sandbox_id)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("no ttrpc client for {}", sandbox_id)))
    }
}
```

### `impl GuestReadiness for VmmTaskRuntime`

```rust
#[async_trait]
impl GuestReadiness for VmmTaskRuntime {
    async fn wait_ready(&self, sandbox_id: &str, vsock_path: &str) -> Result<ReadyResult> {
        // 1. Connect with retry until timeout
        let client = self.connect(sandbox_id, vsock_path).await?;

        // 2. ttrpc Check() — verify vmm-task is responsive
        let ctx = with_timeout(self.config.ttrpc_timeout_ms as i64 * 1_000_000);
        client.lock().await
            .check(ctx, &CheckRequest { service: "vmm-task".into(), ..Default::default() })
            .await?;

        // Clock sync is NOT started here — it requires the exit_signal which is only
        // available after start_sandbox completes. It is started in forward_events().

        Ok(ReadyResult {
            sandbox_id: sandbox_id.to_string(),
            timestamp_ms: unix_now_ms(),
        })
    }

    async fn setup_sandbox(&self, sandbox_id: &str, req: &SandboxSetupRequest) -> Result<()> {
        let client = self.get_client(sandbox_id).await?;
        let ctx = with_timeout(self.config.ttrpc_timeout_ms as i64 * 1_000_000);

        let mut sreq = SetupSandboxRequest::default();

        // Serialise PodSandboxConfig from SandboxData
        if let Some(config) = &req.sandbox_data.config {
            let mut any = vmm_common::api::types::Any::default();
            any.type_url = "PodSandboxConfig".into();
            any.value = serde_json::to_vec(config)?;
            sreq.config = MessageField::some(any);
        }

        // Network interfaces and routes
        sreq.interfaces = req.interfaces.iter().map(|i| i.into()).collect();
        sreq.routes     = req.routes.iter().map(|r| r.into()).collect();

        client.lock().await.setup_sandbox(ctx, &sreq).await?;
        Ok(())
    }

    async fn forward_events(&self, sandbox_id: &str, exit_signal: Arc<ExitSignal>) {
        let client = match self.get_client(sandbox_id).await {
            Ok(c) => c,
            Err(_) => return,
        };
        let id = sandbox_id.to_string();

        // Start the periodic clock-sync background task now that we have the exit_signal.
        // Mirrors client_sync_clock() in vmm/sandbox/src/client.rs.
        {
            let clock_client = client.clone();
            let clock_signal = exit_signal.clone();
            let clock_id = id.clone();
            tokio::spawn(async move {
                let sync = async {
                    loop {
                        tokio::time::sleep(Duration::from_secs(TIME_SYNC_PERIOD)).await;
                        if let Err(e) = do_once_sync_clock(&clock_client).await {
                            tracing::debug!("sync_clock {}: {:?}", clock_id, e);
                        }
                    }
                };
                tokio::select! {
                    _ = sync => {},
                    _ = clock_signal.wait() => {},
                }
            });
        }

        // Forward OOM / container-exit events from vmm-task to containerd.
        tokio::spawn(async move {
            let fut = async {
                loop {
                    match client.lock().await
                        .get_events(with_timeout(0), &Empty::default()).await
                    {
                        Ok(envelope) => {
                            if let Err(e) = publish_event(convert_envelope(envelope)).await {
                                tracing::error!("forward_events {}: publish error: {}", id, e);
                            }
                        }
                        Err(ttrpc::error::Error::Socket(s)) if s.contains("early eof") => break,
                        Err(e) => {
                            tracing::error!("forward_events {}: get_events error: {}", id, e);
                            break;
                        }
                    }
                }
            };
            tokio::select! {
                _ = fut => {},
                _ = exit_signal.wait() => {},
            }
        });
    }

}
```

### `impl ContainerRuntime for VmmTaskRuntime`

```rust
#[async_trait]
impl ContainerRuntime for VmmTaskRuntime {
    async fn create_container(&self, sandbox_id: &str, spec: ContainerSpec)
        -> Result<ContainerInfo> {
        let client = self.get_client(sandbox_id).await?;
        let ctx = with_timeout(self.config.ttrpc_timeout_ms as i64 * 1_000_000);
        let resp = client.lock().await
            .create(ctx, &spec.into_ttrpc_request())
            .await?;
        Ok(ContainerInfo { pid: resp.pid })
    }

    async fn start_process(&self, sandbox_id: &str, container_id: &str)
        -> Result<ProcessInfo> {
        let client = self.get_client(sandbox_id).await?;
        let ctx = with_timeout(self.config.ttrpc_timeout_ms as i64 * 1_000_000);
        let resp = client.lock().await
            .start(ctx, &StartRequest { id: container_id.into(), ..Default::default() })
            .await?;
        Ok(ProcessInfo { pid: resp.pid })
    }

    async fn exec_process(&self, sandbox_id: &str, container_id: &str,
                          spec: ExecSpec) -> Result<ProcessInfo> {
        let client = self.get_client(sandbox_id).await?;
        let ctx = with_timeout(self.config.ttrpc_timeout_ms as i64 * 1_000_000);
        client.lock().await
            .exec(ctx, &spec.into_ttrpc_request(container_id))
            .await?;
        Ok(ProcessInfo { pid: 0 }) // pid comes via Wait
    }

    async fn kill_process(&self, sandbox_id: &str, container_id: &str,
                          pid: u32, signal: u32) -> Result<()> {
        let client = self.get_client(sandbox_id).await?;
        let ctx = with_timeout(self.config.ttrpc_timeout_ms as i64 * 1_000_000);
        client.lock().await
            .kill(ctx, &KillRequest { id: container_id.into(), pid, signal, ..Default::default() })
            .await?;
        Ok(())
    }

    async fn wait_process(&self, sandbox_id: &str, container_id: &str,
                          pid: u32) -> Result<ExitStatus> {
        let client = self.get_client(sandbox_id).await?;
        let ctx = with_timeout(0); // no timeout — wait indefinitely
        let resp = client.lock().await
            .wait(ctx, &WaitRequest { id: container_id.into(), exec_id: pid.to_string(), ..Default::default() })
            .await?;
        Ok(ExitStatus { exit_code: resp.exit_status as i32, exited_at_ms: resp.exited_at.map(|t| t.seconds as u64 * 1000).unwrap_or(0) })
    }

    async fn container_stats(&self, sandbox_id: &str, container_id: &str)
        -> Result<ContainerStats> {
        let client = self.get_client(sandbox_id).await?;
        let ctx = with_timeout(self.config.ttrpc_timeout_ms as i64 * 1_000_000);
        let resp = client.lock().await
            .stats(ctx, &StatsRequest { id: container_id.into(), ..Default::default() })
            .await?;
        Ok(resp.stats.into())
    }
}
```

---

## Story 1.6 — Process Startup Wiring

**Path**: `cmd/vmm-engine/main.rs`
**Binary name**: `vmm-engine` (replaces the three per-VMM binaries for the new path)
**Dependencies**: `vmm-engine`, `vmm-vm-trait`, `vmm-guest-runtime`, `vmm-adapter-k8s`, `vmm-runtime-vmm-task`, `vmm-cloud-hypervisor`, `vmm-qemu`, `vmm-stratovirt`, `vmm-common`, `clap`, `tokio`, `serde`

The VMM type and runtime mode are resolved at startup; the `match` is monomorphised away with no runtime branching:

```rust
// cmd/vmm-engine/main.rs

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let config: TopLevelConfig = Config::load(&args.config).await.unwrap();

    trace::setup_tracing(&config.engine.log_level, "vmm-engine").unwrap();
    utils::start_watchdog();
    sd_notify::notify(&[sd_notify::NotifyState::Ready]).ok();

    match (config.engine.vmm_type.as_str(), config.engine.runtime_mode.as_str()) {
        ("cloud-hypervisor", "standard") => {
            let engine = SandboxEngine::<CloudHypervisorVmm, _, _>::new(
                config.cloud_hypervisor.clone(),        // V::Config
                VmmTaskRuntime::new(config.vmm_task.clone()),
                CloudHypervisorHooks,                   // H: Hooks<CloudHypervisorVmm>
                config.engine.clone().into(),
            );
            let mut adapter = K8sAdapter::new(engine, config.adapter.k8s.clone());
            adapter.recover(&args.dir).await;
            adapter.serve(&args.listen, &args.dir).await.unwrap();
        }
        ("qemu", "standard") => {
            let engine = SandboxEngine::<QemuVmm, _, _>::new(
                config.qemu.clone(),                    // V::Config
                VmmTaskRuntime::new(config.vmm_task.clone()),
                QemuHooks,                              // H: Hooks<QemuVmm>
                config.engine.clone().into(),
            );
            let mut adapter = K8sAdapter::new(engine, config.adapter.k8s.clone());
            adapter.recover(&args.dir).await;
            adapter.serve(&args.listen, &args.dir).await.unwrap();
        }
        ("stratovirt", "standard") => {
            let engine = SandboxEngine::<StratoVirtVmm, _, _>::new(
                config.stratovirt.clone(),              // V::Config
                VmmTaskRuntime::new(config.vmm_task.clone()),
                StratoVirtHooks,                        // H: Hooks<StratoVirtVmm>
                config.engine.clone().into(),
            );
            let mut adapter = K8sAdapter::new(engine, config.adapter.k8s.clone());
            adapter.recover(&args.dir).await;
            adapter.serve(&args.listen, &args.dir).await.unwrap();
        }
        (vmm, mode) => {
            eprintln!("unsupported vmm={vmm} mode={mode}");
            std::process::exit(1);
        }
    }
}
```

### CLI arguments (`Args`)

```rust
// cmd/vmm-engine/main.rs
#[derive(Parser)]
struct Args {
    /// Path to the TOML config file
    #[arg(long, default_value = "/etc/kuasar/config.toml")]
    config: String,
    /// Working directory for sandbox state files (persisted across restarts)
    #[arg(long, default_value = "/run/kuasar/vmm")]
    dir: String,
    /// Unix socket path for the containerd Sandbox API
    #[arg(long, default_value = "/run/kuasar/vmm.sock")]
    listen: String,
}
```

### Top-level configuration

```rust
// Flattened TOML structure
#[derive(Deserialize)]
pub struct TopLevelConfig {
    pub engine: EngineSection,
    #[serde(rename = "cloud-hypervisor", default)]
    pub cloud_hypervisor: CloudHypervisorVmmConfig,
    #[serde(rename = "qemu", default)]
    pub qemu: QemuVmmConfig,
    #[serde(rename = "stratovirt", default)]
    pub stratovirt: StratoVirtVmmConfig,
    #[serde(rename = "vmm-task", default)]
    pub vmm_task: VmmTaskConfig,
    pub adapter: AdapterSection,
}

#[derive(Deserialize)]
pub struct EngineSection {
    pub runtime_mode: String,    // "standard"
    pub vmm_type: String,        // "cloud-hypervisor"
    pub work_dir: String,
    pub log_level: String,
    pub ready_timeout_ms: u64,
    pub enable_tracing: bool,
}

#[derive(Deserialize)]
pub struct AdapterSection {
    pub k8s: K8sAdapterConfig,
}

#[derive(Deserialize, Default)]
pub struct K8sAdapterConfig {
    pub graceful_stop_timeout_ms: u64,  // default: 30_000
}
```

### `EngineSection → EngineConfig` conversion

`EngineSection` lives in `cmd/vmm-engine` (binary config); `EngineConfig` lives in `vmm-engine` (library).
The `.into()` call in `main()` is backed by this `From` impl:

```rust
// cmd/vmm-engine/main.rs
impl From<EngineSection> for EngineConfig {
    fn from(s: EngineSection) -> Self {
        EngineConfig {
            work_dir: s.work_dir,
            ready_timeout_ms: s.ready_timeout_ms,
        }
    }
}
```

`vmm_type`, `runtime_mode`, `log_level`, and `enable_tracing` are consumed by `main()` before
the engine is constructed and are not forwarded to `EngineConfig`.

### State recovery (`recover`)

`K8sAdapter::recover` re-hydrates in-flight sandboxes from the state files written by
`SandboxInstance::dump()` during a previous run. It is called once at startup before `serve`.

```rust
// adapter-k8s/src/sandboxer.rs
impl<V, R, H> K8sAdapter<V, R, H>
where
    V: Vmm + DeserializeOwned + 'static,
    R: ContainerRuntime + 'static,
    H: Hooks<V> + 'static,
{
    /// Scan `dir` for `*.json` state files, deserialise each into `SandboxInstance<V>`,
    /// and re-insert into the engine's sandbox map.
    /// Sandboxes found in Running state have their VMM process pinged;
    /// unreachable ones are transitioned to Stopped.
    pub async fn recover(&mut self, dir: &str) {
        self.engine.recover(dir).await;
    }
}
```

The corresponding `SandboxEngine::recover` reads `{work_dir}/{id}.json`, deserialises the
`SandboxInstance<V>`, pings the VMM, and re-registers the exit-signal watcher.
Detailed recovery logic is out of scope for Epic 1 skeleton — a `todo!()` stub is sufficient
to satisfy the compiler; full implementation is Epic 2.

---

## Migration Strategy

### What stays in `vmm/sandbox`

The existing `vmm/sandbox` crate is **not deleted or modified** in Epic 1. Its three binary entry points (`src/bin/cloud_hypervisor/main.rs`, etc.) continue to build and function unchanged. This ensures:

1. Existing tests in `vmm/sandbox/src/` pass without modification.
2. No production deployment break during development of the new architecture.

The `vmm/sandbox` crate will be **superseded** once the new path reaches feature parity and existing users migrate to `vmm-engine`. At that point, it becomes a thin migration shim (Epic 6) and is eventually removed.

### Naming disambiguation

| Old (vmm/sandbox) | New (new crates) | Notes |
|---|---|---|
| `VM` trait | `Vmm` trait | Different method signatures; adds `type Config` + static `create` |
| `VMFactory` trait | Removed — replaced by `Vmm::create` + `V::Config` | `SandboxEngine` holds `V::Config`; no factory closure, no `Arc<dyn Fn>` |
| `DeviceInfo::Char(...)` (only) | `HotPlugDevice::CharDevice` **or** `VsockMuxIO` | Chosen at runtime by checking `capabilities().virtio_serial` |
| `KuasarSandboxer<F, H>` | `K8sAdapter<V, R, H>` + `SandboxEngine<V, R, H>` | Split into adapter + engine; three type params preserved; `H` is `Hooks<V>` |
| `KuasarSandbox<V>` | `SandboxInstance<V>` | Held by engine, not adapter |
| `KuasarContainer` | `ContainerState` (engine) + `K8sContainer` (adapter) | State vs API view separated |
| `KuasarProcess` | `ProcessState` | Held inside `ContainerState` |
| `Storage` (ref-counted) | `StorageMount` (`ref_containers: Vec<String>`) | Ref-counting preserved; mount is only unmounted when last referencing container is removed |
| `DeviceInfo::Char(...)` | `HotPlugDevice::CharDevice { ... }` | IO pipes for container stdio |
| `DeviceInfo::Block(...)` | `HotPlugDevice::VirtioBlock { ... }` | Block storage |
| `Hooks<V: VM>` | `Hooks<V: Vmm>` (vm-trait/src/hooks.rs) | Same 5-point design; methods now receive `SandboxCtx<V>` instead of `&mut KuasarSandbox<V>`; trait lives in vm-trait so backends don't depend on engine |
| `CloudHypervisorHooks` (vmm/sandbox) | `CloudHypervisorHooks` (vmm/cloud-hypervisor/src/hooks.rs) | Moved into the backend crate; same pre_start/post_start/post_stop logic |
| `CloudHypervisorVM` | `CloudHypervisorVmm` | New struct, new crate, implements `Vmm` |
| `SandboxServiceClient` (ttrpc) | `VmmTaskRuntime` | Wraps client, implements `ContainerRuntime` |
| `client_sync_clock()` | Inside `forward_events()` | Shares exit_signal; started after sandbox Running |
| `sandbox.hot_attach_pipe()` | `hot_attach_pipe()` free fn (adapter-k8s) | Same logic, different location |

### Test isolation

- All tests under `vmm/sandbox/src/` run against the old `VM`/`KuasarSandboxer` types — no changes needed.
- New tests live in the new crates (`vmm/engine/src/`, `vmm/adapter-k8s/src/`, etc.).
- The acceptance criterion for Epic 1 is: `cargo test -p vmm-sandboxer --lib` continues to pass unchanged.

---

## Test Requirements per Story

### 1.1 — vm-trait

#### `Vmm` trait + capabilities

- `VmmCapabilities` default: all bool fields `false`; explicit construction returns correct values.
- `CloudHypervisorVmm` implements `Vmm` — compile-time check; no `create` method on trait.
- `V::create(id, base_dir, config)` — static constructor; `MockVmm::create` returns pre-boot instance without I/O.
- `hot_attach(VirtioBlock)` → device_id returned; `hot_detach(device_id)` — verified against CH mock API.
- `hot_attach(VirtioFs)` → device_id returned; `hot_detach(device_id)` — verified against CH mock API.
- `hot_attach(CharDevice)` → device_id + chardev_id returned (when `virtio_serial = true`).
- `hot_attach(VsockMuxIO)` → device_id returned (when `virtio_serial = false`); adapter picks variant via `capabilities()`.
- `task_address()` — CH returns `"ttrpc+hvsock://…"`; Firecracker would return `"ttrpc+vsock://…"`.
- `ping()` returns `Ok` when CH API socket is reachable; returns `Err` when process is dead.
- `vcpus()` returns non-empty map for a running VM.
- `pids()` returns at least `vmm_pid`.

#### `SandboxCtx<V>` + `Hooks<V>` trait (vm-trait/src/hooks.rs)

All tests in this section use `MockVmm` (minimal `Vmm` impl, records `task_address()` calls) and `MockSandboxData`.

| Test | Assertion |
|---|---|
| `SandboxCtx` construction | Given `vmm`, `data`, `base_dir`, all three fields accessible via `ctx.vmm`, `ctx.data`, `ctx.base_dir` |
| `SandboxCtx` mutability | Write to `ctx.data.task_address`; read back correct value; write to `ctx.vmm` state field; read back |
| `NoopHooks` compile-time | `NoopHooks<MockVmm>: Hooks<MockVmm>` — compile-time check |
| `NoopHooks::post_create` | Returns `Ok(())`; `MockVmm` records no calls |
| `NoopHooks::pre_start` | Returns `Ok(())`; `MockVmm` records no calls |
| `NoopHooks::pre_stop` | Returns `Ok(())`; `MockVmm` records no calls |
| `NoopHooks::post_stop` | Returns `Ok(())`; `MockVmm` records no calls |
| `Hooks` default `post_start` | `NoopHooks::post_start(ctx)` calls `ctx.vmm.task_address()` and writes result into `ctx.data.task_address` |
| `NoopHooks::default()` | Constructs without arguments; `PhantomData` carries `MockVmm` type |

#### Per-backend hooks (vmm/cloud-hypervisor, vmm/qemu, vmm/stratovirt)

Each backend crate tests its `Hooks` impl in isolation using a real backend VMM instance (or a
minimal stub with the backend-specific setters). No engine or adapter dependency required.

| Test | Crate | Assertion |
|---|---|---|
| `CloudHypervisorHooks::pre_start` applies resources | `vmm/cloud-hypervisor` | Given `SandboxData` with CPU/memory limits, `set_cpus` and `set_memory_mb` called on `CloudHypervisorVmm` with correct values |
| `CloudHypervisorHooks::pre_start` no resources | `vmm/cloud-hypervisor` | `SandboxData` with no resource annotations; `set_cpus` / `set_memory_mb` not called |
| `CloudHypervisorHooks::post_start` sets task_address | `vmm/cloud-hypervisor` | `ctx.data.task_address` matches `ctx.vmm.task_address()` after call |
| `CloudHypervisorHooks::post_stop` removes sockets | `vmm/cloud-hypervisor` | Creates `api.sock` and `task.vsock` temp files under `base_dir`; after `post_stop`, both absent |
| `QemuHooks::pre_start` applies resources | `vmm/qemu` | `set_smp` and `set_memory_mb` called with correct values |
| `QemuHooks::post_start` sets task_address | `vmm/qemu` | `ctx.data.task_address` matches `ctx.vmm.task_address()` |
| `StratoVirtHooks::post_start` sets task_address | `vmm/stratovirt` | `ctx.data.task_address` matches `ctx.vmm.task_address()` |
| Backend hooks compile-time | each crate | `CloudHypervisorHooks: Hooks<CloudHypervisorVmm>`, `QemuHooks: Hooks<QemuVmm>`, `StratoVirtHooks: Hooks<StratoVirtVmm>` |

### 1.2 — guest-runtime

- `ContainerRuntime` and `GuestReadiness` are independent traits — compile-time check that `VmmTaskRuntime` implements both separately.
- `kill_process`, `wait_process`, `container_stats` are in `ContainerRuntime` (called only by `K8sAdapter`); `GuestReadiness` contains only `wait_ready`, `setup_sandbox`, `forward_events` (called by `SandboxEngine`).
- `setup_sandbox()` sends interfaces + routes in `SetupSandboxRequest` — verified with in-process mock ttrpc server.
- `forward_events()` relays get_events response to containerd publisher — verified with mock.

### 1.3 — engine

All tests use `MockVmm` (implements `Vmm`, records calls) and `MockRuntime` (implements `GuestReadiness`, returns configurable responses).

| Test | Assertion |
|---|---|
| `create_sandbox` succeeds | `SandboxInstance` in `Creating` state; directory created; sandbox files written |
| `create_sandbox` cgroup | `SandboxCgroup::create_sandbox_cgroups` called with correct parent path |
| `start_sandbox` network | `MockVmm::add_network()` called with discovered tap device; `setup_sandbox()` receives interfaces + routes |
| `create_sandbox` hook | `MockHooks::post_create` called after `SandboxInstance` constructed; receives `SandboxCtx` |
| `start_sandbox` pre_start hook | `MockHooks::pre_start` called before `boot()`; receives `SandboxCtx<MockVmm>` with correct `vmm`, `data`, `base_dir` |
| `start_sandbox` boot path | `MockVmm::boot()` called after `pre_start` hook; state → `Running`; `vmm_start_ms > 0` |
| `start_sandbox` post_start hook | `MockHooks::post_start` called after `wait_ready`; default impl sets `ctx.data.task_address` |
| `start_sandbox` cgroup add | `add_process_into_sandbox_cgroups` called with vmm_pid |
| `start_sandbox` wrong state | sandbox not in `Creating`; returns `InvalidState` |
| `start_sandbox` ready timeout | `wait_ready` blocks beyond timeout; state → `Stopped`; returns `Timeout` |
| `stop_sandbox` Running → Stopped | `MockVmm::stop()` called; `network.take()` called |
| `stop_sandbox` from Deleted | Returns `InvalidState` |
| `delete_sandbox` Stopped → Deleted | Directory removed; cgroup removed; sandbox evicted from map |
| `delete_sandbox` Running (no force) | Returns `InvalidState` |
| `delete_sandbox` Running (force=true) | `MockVmm::stop(true)` called; state → `Deleted` |
| `create_sandbox` duplicate id | Returns `AlreadyExists` |

### 1.4 — adapter-k8s

| Test | Assertion |
|---|---|
| `Sandboxer::create` → engine `create_sandbox` | Engine receives correct `CreateSandboxRequest` with cgroup_parent |
| `Sandboxer::start` → engine `start_sandbox` | Engine called; containerd `TaskStart` event published |
| `Sandboxer::update` | `SandboxInstance.data` updated and persisted |
| `Sandboxer::sandbox` cache | `K8sSandboxView.containers` populated from engine state on construction |
| `Sandbox::ping` → `Vmm::ping` | Mock VMM ping called |
| `Sandbox::container` returns cached | Returns `K8sContainer` from local cache without locking engine |
| `Sandbox::append_container` IO pipes | `hot_attach(CharDevice)` called per non-empty stdio path; device_ids in `ContainerState.io_devices` |
| `Sandbox::append_container` block storage | `hot_attach(VirtioBlock)` called; `StorageMount` with `device_id` added to `inst.storages` |
| `Sandbox::append_container` local cache | After append, `K8sSandboxView.containers` contains the new container |
| `Sandbox::remove_container` | `deference_container_storages` called; `hot_detach` for each io_device; local cache cleared |
| `Sandbox::exit_signal` | Returns the instance's `ExitSignal` |
| `TaskService::create` → `ContainerRuntime::create_container` | Mock runtime receives `ContainerSpec` |
| `TaskService::exec` → `ContainerRuntime::exec_process` | Delegated correctly |

### 1.5 — runtime-vmm-task

| Test | Assertion |
|---|---|
| `wait_ready` connects and calls Check | Verified with in-process mock ttrpc server; SetupSandbox and clock-sync NOT called here |
| `setup_sandbox` sends interfaces + routes | `SetupSandboxRequest` contains correct interface/route data |
| `forward_events` starts clock sync | `do_once_sync_clock` called after `TIME_SYNC_PERIOD`; stopped when exit_signal fires |
| `forward_events` relays envelope | Spawns background task; event appears in publisher; stopped when exit_signal fires |
| `kill_process` maps SIGTERM to ttrpc signal (ContainerRuntime) | Signal value forwarded unchanged |
| `create_container` forwards spec bytes | Correct protobuf request sent |
| `VmmTaskRuntime` implements `ContainerRuntime` | Compile-time check |
| `VmmTaskRuntime` implements `GuestReadiness` | Compile-time check |

### 1.6 — startup wiring

| Test | Assertion |
|---|---|
| Config parse: `runtime_mode=standard, vmm_type=cloud-hypervisor` | `TopLevelConfig` parses without error; `cloud_hypervisor` section populated |
| Config parse: `runtime_mode=standard, vmm_type=qemu` | `TopLevelConfig` parses without error; `qemu` section populated |
| Config parse: `runtime_mode=standard, vmm_type=stratovirt` | `TopLevelConfig` parses without error; `stratovirt` section populated |
| `K8sAdapter::new(engine, config)` constructs | Does not panic |
| `cargo test -p vmm-sandboxer --lib` | All existing tests pass unchanged |

---

## Epic 1 — Definition of Done

Epic 1 is complete when **all** of the following are true:

### Compilation

- [ ] `cargo build --release -p vmm-vm-trait` succeeds
- [ ] `cargo build --release -p vmm-guest-runtime` succeeds
- [ ] `cargo build --release -p vmm-engine` succeeds
- [ ] `cargo build --release -p vmm-adapter-k8s` succeeds
- [ ] `cargo build --release -p vmm-runtime-vmm-task` succeeds
- [ ] `cargo build --release -p vmm-cloud-hypervisor` succeeds
- [ ] `cargo build --release -p vmm-qemu` succeeds
- [ ] `cargo build --release -p vmm-stratovirt` succeeds
- [ ] `cargo build --release --bin vmm-engine` succeeds (all three VMM arms compile)
- [ ] `cargo build --release --bin cloud_hypervisor` (existing) still succeeds
- [ ] `cargo clippy --all-features -- -D warnings` passes on all new crates

### Tests

- [ ] `cargo test -p vmm-sandboxer --lib` — all existing tests pass unchanged
- [ ] `cargo test -p vmm-vm-trait` — all Story 1.1 tests pass (Vmm, SandboxCtx, Hooks, NoopHooks)
- [ ] `cargo test -p vmm-cloud-hypervisor` — CloudHypervisorHooks tests pass
- [ ] `cargo test -p vmm-qemu` — QemuHooks tests pass
- [ ] `cargo test -p vmm-stratovirt` — StratoVirtHooks tests pass
- [ ] `cargo test -p vmm-engine` — all Story 1.3 engine tests pass (MockVmm + MockRuntime)
- [ ] `cargo test -p vmm-adapter-k8s` — all Story 1.4 adapter tests pass
- [ ] `cargo test -p vmm-runtime-vmm-task` — all Story 1.5 runtime tests pass

### Behaviour

- [ ] No change to existing `vmm-sandboxer` behaviour — existing integration tests pass
- [ ] `vmm-engine` binary starts, loads config, and logs "ready" without error
- [ ] `vmm-engine` with `vmm_type=cloud-hypervisor` correctly monomorphises `SandboxEngine<CloudHypervisorVmm, VmmTaskRuntime, CloudHypervisorHooks>`
- [ ] `vmm-engine` with `vmm_type=qemu` and `vmm_type=stratovirt` compile and start equivalently

### Code quality

- [ ] No `todo!()` or `unimplemented!()` in Story 1.1–1.5 code paths (recovery stub in 1.6 is exempt)
- [ ] All new crates have `#![deny(unused_imports)]` and pass `cargo +nightly fmt --check`
- [ ] Dependency graph matches the **Crate Dependency Graph** section — verified by `cargo tree`
