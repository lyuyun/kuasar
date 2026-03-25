# Appliance Sandbox Mode for AI Agent Workloads

<!-- toc -->
- [Release Signoff Checklist](#release-signoff-checklist)
- [Summary](#summary)
- [Motivation](#motivation)
  - [Goals](#goals)
  - [Non-Goals](#non-goals)
- [Background](#background)
  - [Current Kuasar Architecture](#current-kuasar-architecture)
  - [Limitations for Agent Sandbox Scenarios](#limitations-for-agent-sandbox-scenarios)
  - [Industry Trends](#industry-trends)
- [Proposal](#proposal)
  - [User Stories](#user-stories)
    - [UC1: AI Agent Sandbox](#uc1-ai-agent-sandbox)
    - [UC2: Serverless Function Execution](#uc2-serverless-function-execution)
    - [UC3: Standard K8s Pod Isolation (Existing)](#uc3-standard-k8s-pod-isolation-existing)
    - [UC4: Sandbox Pause/Resume and Cross-Node Migration](#uc4-sandbox-pauseresume-and-cross-node-migration)
  - [Risks and Mitigations](#risks-and-mitigations)
    - [Snapshot restore is not always faster than cold boot](#snapshot-restore-is-not-always-faster-than-cold-boot)
    - [Cross-VMM snapshot incompatibility](#cross-vmm-snapshot-incompatibility)
    - [CH v50.0 restore behavior is non-obvious](#ch-v500-restore-behavior-is-non-obvious)
  - [Overview](#overview)
  - [Architecture: Three-Layer Engine Design](#architecture-three-layer-engine-design)
  - [Vmm Trait: Multi-VMM Abstraction](#vmm-trait-multi-vmm-abstraction)
  - [GuestReadiness and ContainerRuntime Traits](#guestreadiness-and-containerruntime-traits)
  - [Appliance Readiness Protocol](#appliance-readiness-protocol)
  - [Sandbox State Machine](#sandbox-state-machine)
  - [API Adapters: K8s and Direct](#api-adapters-k8s-and-direct)
  - [kuasar-builder: Two-Phase Template Build Pipeline](#kuasar-builder-two-phase-template-build-pipeline)
  - [Admission Controller](#admission-controller)
- [Design Details](#design-details)
  - [Vmm Trait Definition](#vmm-trait-definition)
  - [GuestReadiness Trait Definition](#guestreadiness-trait-definition)
  - [ContainerRuntime Trait Definition](#containerruntime-trait-definition)
  - [SandboxEngine Core](#sandboxengine-core)
  - [Template Discovery](#template-discovery)
  - [Runtime Snapshot Lifecycle](#runtime-snapshot-lifecycle)
  - [Boot Mode Selection: Benchmark-Informed Heuristics](#boot-mode-selection-benchmark-informed-heuristics)
  - [Artifact-to-Start-Mode Mapping](#artifact-to-start-mode-mapping)
  - [RootfsProvider Trait: Pluggable Disk Backend](#rootfsprovider-trait-pluggable-disk-backend)
  - [Snapshot Template Version Validation](#snapshot-template-version-validation)
  - [K8s Adapter: Sandbox and Task API Mapping](#k8s-adapter-sandbox-and-task-api-mapping)
  - [Direct Adapter: Native Sandbox API](#direct-adapter-native-sandbox-api)
  - [Direct Adapter Security Model](#direct-adapter-security-model)
  - [Appliance Protocol Specification](#appliance-protocol-specification)
  - [Configuration](#configuration)
  - [Observability and Events](#observability-and-events)
  - [Compatibility](#compatibility)
    - [Backward Compatibility](#backward-compatibility)
    - [K8s and containerd Compatibility](#k8s-and-containerd-compatibility)
    - [VMM Compatibility Matrix](#vmm-compatibility-matrix)
    - [CH v50.0 Implementation Constraints](#ch-v500-implementation-constraints)
  - [Test Plan](#test-plan)
    - [Prerequisite testing updates](#prerequisite-testing-updates)
    - [Unit tests](#unit-tests)
    - [Integration tests](#integration-tests)
    - [e2e tests](#e2e-tests)
  - [Graduation Criteria](#graduation-criteria)
    - [Alpha](#alpha)
    - [Beta](#beta)
    - [GA](#ga)
  - [Implementation Stories](#implementation-stories)
    - [Epic 1: Core Architecture Refactoring](#epic-1-core-architecture-refactoring)
    - [Epic 2: Appliance Mode Direct Path](#epic-2-appliance-mode-direct-path)
    - [Epic 3: Template Snapshot Pipeline](#epic-3-template-snapshot-pipeline)
    - [Epic 4: Runtime Snapshot Lifecycle](#epic-4-runtime-snapshot-lifecycle)
    - [Epic 5: Firecracker VMM Support](#epic-5-firecracker-vmm-support)
    - [Epic 6: Production Readiness](#epic-6-production-readiness)
  - [Upgrade / Downgrade Strategy](#upgrade--downgrade-strategy)
  - [Version Skew Strategy](#version-skew-strategy)
- [Production Readiness Review Questionnaire](#production-readiness-review-questionnaire)
  - [Feature Enablement and Rollback](#feature-enablement-and-rollback)
  - [Rollout, Upgrade and Rollback Planning](#rollout-upgrade-and-rollback-planning)
  - [Monitoring Requirements](#monitoring-requirements)
  - [Dependencies](#dependencies)
  - [Scalability](#scalability)
  - [Troubleshooting](#troubleshooting)
- [Implementation History](#implementation-history)
- [Drawbacks](#drawbacks)
- [Alternatives](#alternatives)
  - [Alternative 1: Appliance as a configuration flag within existing architecture](#alternative-1-appliance-as-a-configuration-flag-within-existing-architecture)
  - [Alternative 2: Separate binary for appliance mode](#alternative-2-separate-binary-for-appliance-mode)
  - [Alternative 3: Dynamic per-sandbox mode selection at runtime](#alternative-3-dynamic-per-sandbox-mode-selection-at-runtime)
  - [Alternative 4: Build lazy image loading directly into Kuasar](#alternative-4-build-lazy-image-loading-directly-into-kuasar)
  - [Alternative 5: Place runtime snapshot chunk upload in the external content system](#alternative-5-place-runtime-snapshot-chunk-upload-in-the-external-content-system)
- [References](#references)
- [Infrastructure Needed (Optional)](#infrastructure-needed-optional)
<!-- /toc -->

## Release Signoff Checklist

Items marked with (R) are required *prior to targeting to a milestone / release*.

- [ ] (R) Proposal issue created and linked to this document
- [ ] (R) Design details are appropriately documented
- [ ] (R) Test plan is in place, giving consideration to unit, integration, and e2e coverage
- [ ] (R) Graduation criteria is in place
- [ ] (R) Compatibility impact has been assessed (standard mode preserved, existing tests pass)
- [ ] "Implementation History" section is up-to-date for milestone
- [ ] Supporting documentation — additional design documents, benchmark results, and related PRs/issues

---

## Summary

This proposal introduces an **Appliance Sandbox Mode** for Kuasar, targeting AI Agent and serverless workloads that require sub-second microVM startup. In appliance mode, a single microVM runs a single application process — there is no guest agent (`vmm-task`), no container abstraction, and no `exec`/`attach` capability. The VM itself **is** the application.

To support this alongside Kuasar's existing standard mode, we propose a **three-layer engine architecture** with pluggable VMM backends (Cloud Hypervisor, Firecracker), pluggable guest runtime strategies (standard `vmm-task` vs. appliance `READY` protocol), and pluggable API adapters (K8s/containerd vs. direct gRPC). The runtime mode and VMM backend are selected at process startup, with zero runtime branching.

Additionally, a **kuasar-builder** subproject is introduced to provide a two-phase `OCI image → fast-boot template` build pipeline, producing both **Image Products** (for cold boot) and **Snapshot Products** (for snapshot-based restore). Benchmark data from CH v50.0 shows that cold boot can be 3× faster than snapshot restore for lightweight workloads, so the design supports both modes as first-class start paths with caller-driven mode selection.

The engine also provides a **runtime snapshot lifecycle** (`pause_sandbox` / `resume_sandbox` / `snapshot_sandbox`) that enables user-initiated pause/resume, BMS resource rebalancing, and node drain scenarios. Runtime snapshots are a first-class citizen alongside template snapshots — they can be chunk-ified and uploaded to an external content delivery system for cross-node restore, with content-addressed dedup providing 20–40× storage reduction for homogeneous workloads.

---

## Motivation

### Goals

1. **Sub-second sandbox startup** for AI Agent and serverless workloads (P95 < 1s from request to application ready).
2. **Appliance mode** where one microVM = one application, with no guest agent overhead.
3. **Multi-VMM support** enabling users to choose between Cloud Hypervisor and Firecracker based on their tradeoff preferences (feature richness vs. minimal latency).
4. **Preserve standard mode** for full K8s/containerd compatibility, ensuring Kuasar can serve both audiences from a single codebase.
5. **Snapshot-based fast restore** with a built-in template build pipeline (kuasar-builder).
6. **Runtime snapshot lifecycle** for pause/resume, BMS migration, and node drain — runtime snapshots are first-class citizens that can be chunk-ified for cross-node restore.
7. **Clean architecture** that separates API adaptation, core engine logic, and VMM-specific code into distinct layers.

### Non-Goals

1. **Live migration** (zero-downtime, memory pre-copy) of running VMs across nodes (future work). Note: stop-and-copy migration via runtime snapshot (pause → snapshot → chunk upload → restore on target) **is** in scope — it requires brief downtime but is architecturally simpler than live migration.
2. **Built-in image lazy loading** — content delivery (chunked images, multi-tier caching) is intentionally out of scope for Kuasar itself. It can be provided by external systems through a pluggable disk path interface.
3. **Multi-container pods in appliance mode** — appliance mode is explicitly single-application. Multi-container pods remain supported in standard mode.
4. **Cross-VMM snapshot compatibility** — CH snapshots cannot be loaded by Firecracker and vice versa. Templates are tagged with their target VMM.

---

## Background

### Current Kuasar Architecture

Kuasar today implements the containerd `Sandboxer` trait, managing VM-based sandboxes with a guest agent (`vmm-task`) that communicates via ttrpc over vsock:

```
containerd → shimv2 → KuasarSandboxer → CloudHypervisorVM
                                              │
                                        [vsock/ttrpc]
                                              │
                                         vmm-task (guest)
                                          ├── check()
                                          ├── setup_sandbox()
                                          ├── create_container()
                                          ├── exec_process()
                                          └── ...
```

Key characteristics:
- Guest runs `vmm-task` as PID1, providing a container runtime inside the VM.
- Host communicates with guest via ttrpc (`SandboxServiceClient`).
- Supports full OCI container lifecycle: create, start, exec, attach, kill, wait, stats.
- VMM is always Cloud Hypervisor, always cold-booted.

### Limitations for Agent Sandbox Scenarios

For AI Agent / serverless workloads, the current architecture introduces unnecessary overhead:

| Overhead | Impact | Appliance Mode Savings |
|----------|--------|----------------------|
| `vmm-task` guest agent startup | ~50-100ms | Eliminated entirely |
| ttrpc connection establishment | ~10-20ms | Eliminated entirely |
| `setup_sandbox()` RPC | ~5-10ms | Eliminated entirely |
| `create_container()` (namespace/cgroup) | ~10-30ms | Eliminated entirely |
| `start_process()` (fork+exec) | ~5-10ms | Eliminated entirely |
| `virtiofsd` process | ~10-20ms + ongoing overhead | Eliminated entirely |
| **Total removable overhead** | **~90-190ms** | **Eliminated** |

Additionally:
- **Snapshot restore** is not supported in the current start path (always cold boot).
- **Only Cloud Hypervisor** is supported; Firecracker's lighter-weight restore is unavailable.
- **exec/attach** capabilities add complexity but are never used in agent scenarios.

### Industry Trends

The "one VM = one application" pattern is gaining adoption across the industry:

- **AWS Firecracker** (Lambda/Fargate): Each microVM runs a single function or task. No guest-side container runtime.
- **Modal.com**: Purpose-built microVM sandbox for AI workloads. Snapshot-based restore with lazy content loading.
- **Fly.io**: Firecracker-based application VMs. Each VM is a single application.
- **Unikernel movement** (MirageOS, OSv, UniKraft): Compiles application + OS into a single bootable image — the most extreme form of the appliance model.

The term "appliance" originates from the VMware era's "Virtual Appliance" concept — a pre-configured VM image that does one thing and is managed as a black box — and has been refined through the unikernel and microVM communities into the "one VM = one application" pattern seen in modern serverless platforms.

---

## Proposal

### User Stories

#### UC1: AI Agent Sandbox

An AI agent orchestration platform needs to spin up isolated execution environments for AI agents. Each agent runs a pre-built application image. Requirements:

- Cold start < 1s (from API call to agent ready)
- Strong isolation (hardware virtualization)
- No need for `kubectl exec` or multi-container support
- High concurrency (hundreds of sandboxes per node)

#### UC2: Serverless Function Execution

A FaaS platform uses microVMs for function isolation. Each function invocation gets its own VM, restored from a pre-warmed snapshot:

- Restore + resume + ready < 200ms
- Single entry process per VM
- Short-lived (seconds to minutes)

#### UC3: Standard K8s Pod Isolation (Existing)

The existing use case where Kuasar provides VM-based pod isolation for Kubernetes. Full container lifecycle support via `vmm-task`:

- `exec`/`attach`/multi-container support required
- Integration with containerd via shimv2
- This continues to work unchanged

#### UC4: Sandbox Pause/Resume and Cross-Node Migration

An AI agent platform needs to pause idle sandboxes to free resources and resume them on demand (possibly on a different node):

- User pauses agent → runtime snapshot created → memory chunk-ified and uploaded
- User resumes agent → chunks fetched → snapshot restored → agent resumes from exact state
- BMS rebalancing: platform moves sandboxes between nodes via runtime snapshot
- Node retirement: idle sandboxes snapshotted and drained before decommission
- Content-addressed dedup keeps storage cost manageable (20–40× reduction for homogeneous workloads)

### Risks and Mitigations

#### Snapshot restore is not always faster than cold boot

CH v50.0 reads the entire `memory-ranges` file synchronously during restore, regardless of guest working set size. For lightweight workloads (< ~1 GB active memory), cold boot is faster than snapshot restore.

**Mitigation**: The design does not mandate a default start mode. The caller specifies `StartMode` (Boot / RestoreEager / Auto). When `Auto` is used for `FirstLaunch`, Kuasar attempts restore only when a compatible template exists, and falls back to cold boot otherwise. Documentation and the `boot_mode_selection_strategy.md` design guide instruct operators on when each mode is optimal. Future CH upstream improvements (mmap-based demand-paged restore) will close this gap.

#### Cross-VMM snapshot incompatibility

CH snapshots cannot be loaded by Firecracker and vice versa. A template built for one VMM cannot be used on a node running the other.

**Mitigation**: `snapshot.meta.json` records both `vmm_type` and `vmm_version`. The `TemplateManager::validate_compat()` check fails fast on mismatch and degrades gracefully to cold boot with an observable `sandbox.start.degraded` event. Schedulers can use the version domain tag for cache-aware placement.

#### CH v50.0 restore behavior is non-obvious

Passing `--kernel` alongside `--restore` on the CH command line causes CH to silently perform a cold boot, ignoring the snapshot. This was the root cause of misleading benchmark results (~26 ms "restore" that was actually a cold boot).

**Mitigation**: The `CloudHypervisorVmm::restore()` implementation enforces a strict rule: only `--restore source_url=` and `--api-socket` are passed; `--kernel`, `--memory`, `--cpus`, `--disk` are forbidden. This is documented in [CH v50.0 Implementation Constraints](#ch-v500-implementation-constraints) and enforced in code. Integration tests verify that restore mode does not silently fall back to boot.

### Overview

We propose refactoring Kuasar's internal architecture into three layers, introducing appliance mode as a first-class runtime option alongside the existing standard mode:

```
┌─────────────────────────────────────────────────────────┐
│  Adapter Layer (pluggable, select one at startup)        │
│                                                         │
│  ┌───────────────────┐     ┌─────────────────────────┐  │
│  │ K8s Adapter       │     │ Direct Adapter          │  │
│  │ (shimv2+Task API) │     │ (native gRPC)           │  │
│  └─────────┬─────────┘     └───────────┬─────────────┘  │
│            └────────────┬──────────────┘                 │
│                         ▼                                │
├─────────────────────────────────────────────────────────┤
│  Engine Layer                                            │
│                                                         │
│  SandboxEngine<V: Vmm, R: GuestReadiness>               │
│  ├── Sandbox lifecycle: create/start/stop/delete        │
│  ├── Guest operations: delegated to GuestReadiness      │
│  ├── Admission Controller                               │
│  └── Template Manager                                   │
│                                                         │
├─────────────────────────────────────────────────────────┤
│  Infrastructure Layer (pluggable, select one at startup) │
│                                                         │
│  Vmm trait              GuestReadiness trait             │
│  ┌────────────────┐     ┌────────────────────┐          │
│  │CloudHypervisor │ OR  │ VmmTaskRuntime     │ OR       │
│  ├────────────────┤     │ (ttrpc → vmm-task) │          │
│  │Firecracker     │     ├────────────────────┤          │
│  └────────────────┘     │ ApplianceRuntime   │          │
│                         │ (vsock JSON Lines) │          │
│                         └────────────────────┘          │
└─────────────────────────────────────────────────────────┘
```

**Process startup determines the combination.** Four valid configurations:

| VMM | Runtime Mode | Adapter | Primary Use Case |
|-----|-------------|---------|------------------|
| Cloud Hypervisor | Standard | K8s Adapter | K8s pod isolation (existing) |
| Cloud Hypervisor | Appliance | Direct Adapter | AI Agent sandbox (rich VMM features) |
| Firecracker | Standard | K8s Adapter | K8s pod isolation (lightweight) |
| Firecracker | Appliance | Direct Adapter | Serverless / FaaS (minimal latency) |

### Architecture: Three-Layer Engine Design

**Infrastructure Layer (VMM + GuestReadiness)** provides hardware and guest-communication abstraction:
- `Vmm` trait abstracts VM lifecycle operations (create, boot, restore, resume, pause, snapshot, stop, device management).
- `GuestReadiness` trait abstracts guest-side readiness and signaling. `ContainerRuntime` extends it with full container operations for standard mode.

**Engine Layer** implements sandbox lifecycle logic independent of any specific VMM or runtime mode:
- Admission control (concurrency limits, budget enforcement).
- Template management (registration, selection, garbage collection).
- Start mode negotiation (auto/restore/boot with degradation).

**Adapter Layer** translates external protocols to engine calls:
- K8s Adapter: implements `Sandboxer` trait + Task API, mapping to engine operations.
- Direct Adapter: exposes native gRPC API with full engine capabilities.

### Vmm Trait: Multi-VMM Abstraction

The `Vmm` trait provides a unified interface for different VMMs:

```rust
trait Vmm: Send + Sync {
    async fn create(&mut self, config: VmConfig) -> Result<()>;
    async fn boot(&mut self) -> Result<()>;
    async fn restore(&mut self, snapshot: &SnapshotRef) -> Result<()>;
    async fn resume(&mut self) -> Result<()>;
    async fn pause(&mut self) -> Result<()>;
    async fn snapshot(&mut self, dest: &SnapshotDest) -> Result<SnapshotInfo>;
    async fn stop(&mut self, force: bool) -> Result<()>;
    async fn wait_exit(&self) -> Result<ExitInfo>;
    fn add_disk(&mut self, disk: DiskConfig) -> Result<()>;  // call before boot/restore
    fn add_network(&mut self, net: NetworkConfig) -> Result<()>;  // call before boot/restore
    fn vsock_path(&self) -> Result<String>;
    fn capabilities(&self) -> VmmCapabilities;
}
```

Each VMM implements this trait with its specific API:
- **Cloud Hypervisor**: CLI-based restore (`--restore source_url=`), REST API for resume/pause/snapshot, supports hot-plug and resize.
- **Firecracker**: API-based snapshot load (`PUT /snapshot/load`), PATCH-based resume, supports diff snapshots, no hot-plug.

The engine queries `VmmCapabilities` to handle differences gracefully (e.g., skipping hot-plug on Firecracker).

### GuestReadiness and ContainerRuntime Traits

**Appliance mode** and **standard mode** diverge in what guest-side operations the host needs to perform. Rather than a single `GuestRuntime` trait with no-op implementations for appliance mode, we define two traits with a clean inheritance relationship:

**`GuestReadiness`** (implemented by both `ApplianceRuntime` and `VmmTaskRuntime`):

```rust
trait GuestReadiness: Send + Sync {
    /// Wait for guest to become ready.
    /// Appliance: wait for READY JSON message on vsock.
    /// Standard: ttrpc check() + setup_sandbox().
    async fn wait_ready(&self, sandbox_id: &str) -> Result<ReadyResult>;

    /// Send a signal to a process (or SHUTDOWN in appliance mode).
    async fn kill_process(&self, sandbox_id: &str, container_id: &str,
                          pid: u32, signal: u32) -> Result<()>;

    /// Wait for a process to exit (or VM exit in appliance mode).
    async fn wait_process(&self, sandbox_id: &str, container_id: &str,
                          pid: u32) -> Result<ExitStatus>;

    /// Get container/VM resource stats (host-side cgroups in appliance mode).
    async fn container_stats(&self, sandbox_id: &str, container_id: &str)
        -> Result<ContainerStats>;

    /// Notify the guest to quiesce and prepare for snapshot (kuasar-builder Phase 2).
    /// Default no-op — appliance applications quiesce on VMM pause without explicit signaling.
    async fn prepare_snapshot(&self, sandbox_id: &str) -> Result<()> { Ok(()) }
}
```

**`ContainerRuntime`** (implemented only by `VmmTaskRuntime`, standard mode only):

```rust
trait ContainerRuntime: GuestReadiness {
    /// Create a container inside the VM via ttrpc create_container().
    async fn create_container(&self, sandbox_id: &str, spec: ContainerSpec)
        -> Result<ContainerInfo>;

    /// Start the main process via ttrpc start_process() — real fork+exec.
    async fn start_process(&self, sandbox_id: &str, container_id: &str)
        -> Result<ProcessInfo>;

    /// Execute an additional process via ttrpc exec_process() — real setns+fork+exec.
    async fn exec_process(&self, sandbox_id: &str, container_id: &str,
                          spec: ExecSpec) -> Result<ProcessInfo>;
}
```

`SandboxEngine<V: Vmm, R: GuestReadiness>` is parameterized by `GuestReadiness` — sufficient for all engine operations. The K8s Adapter adds a `R: ContainerRuntime` bound at the adapter level, enforcing at compile time that container operations are only available in standard mode.

Two implementations:
- **VmmTaskRuntime** implements `ContainerRuntime` (which includes `GuestReadiness`). All methods perform real guest-side operations via ttrpc over vsock.
- **ApplianceRuntime** implements only `GuestReadiness`. `kill_process()` sends `SHUTDOWN` message. `prepare_snapshot()` uses the default no-op.

### Appliance Readiness Protocol

In appliance mode, guest-host communication uses a minimal JSON Lines protocol over vsock:

**Guest → Host:**

| Message | Semantics | Required Fields |
|---------|-----------|----------------|
| `READY` | Application is ready to serve | `sandbox_id` |
| `HEARTBEAT` | Liveness signal | `sandbox_id` |
| `METRICS` | Optional telemetry | `sandbox_id`, metric fields |
| `FATAL` | Unrecoverable error, host should reclaim VM | `sandbox_id`, `reason` |

**Host → Guest:**

| Message | Semantics | Required Fields |
|---------|-----------|----------------|
| `SHUTDOWN` | Graceful termination request | `deadline_ms` |
| `CONFIG` | One-time configuration injection (sent before READY wait) | `version`, `sandbox_id`, config payload |
| `PING` | Connectivity probe | — |

**CONFIG message schema** (sent by host immediately after vsock connection, before waiting for READY):
```json
{
  "type": "CONFIG",
  "version": 1,
  "sandbox_id": "sb-123",
  "env": {"AGENT_ID": "abc", "REGION": "us-east-1"},
  "hostname": "sb-123",
  "dns": ["8.8.8.8", "8.8.4.4"],
  "search_domains": ["internal.example.com"]
}
```
Fields: `version` (schema version, currently 1), `env` (environment variables injected before app reads them), `hostname`, `dns`, `search_domains`. CONFIG is optional — if the host has no configuration to inject, it skips CONFIG and waits directly for READY.

**HEARTBEAT host-side behavior**: After receiving `READY`, the host starts a heartbeat timer. If `heartbeat_timeout_ms > 0` and no `HEARTBEAT` is received within the window, the host takes the configured `heartbeat_action`:
- `"kill"`: Sends `SHUTDOWN`, then kills the VMM process if it does not exit within `shutdown_timeout_ms`. Emits `sandbox.liveness.timeout` event.
- `"warn"`: Emits `sandbox.liveness.timeout` event only (no kill). Useful for debugging.

Example READY message:
```json
{"type":"READY","sandbox_id":"sb-123","app_version":"1.2.3"}
```

### Sandbox State Machine

Every sandbox instance progresses through the following states:

```
       create_sandbox()
            ↓
       ┌──────────┐
       │ Creating  │
       └─────┬────┘
             │ start_sandbox() succeeds
             ▼
       ┌──────────┐   pause_sandbox()    ┌──────────┐
       │ Running  │ ──────────────────►  │  Paused  │
       │          │ ◄──────────────────  │          │
       └─────┬────┘   resume_sandbox()   └─────┬────┘
             │                                 │
             │ stop_sandbox() /                │ stop_sandbox()
             │ FATAL event /                   │
             │ VMM process exit                │
             ▼                                 ▼
       ┌──────────┐                     ┌──────────┐
       │ Stopped  │ ◄───────────────────┘          │
       └─────┬────┘                                │
             │ delete_sandbox()
             ▼
       ┌──────────┐
       │ Deleted  │
       └──────────┘

  Creating ──► Stopped  (start_sandbox() fails: READY timeout, VMM error)
  Any state ──► Deleted  (delete_sandbox(force=true))
```

**State transition table**:

| From | Event | To | Notes |
|------|-------|----|-------|
| `Creating` | `start_sandbox()` succeeds | `Running` | READY received within timeout |
| `Creating` | `start_sandbox()` fails | `Stopped` | READY timeout, VMM error, admission rejected |
| `Running` | `pause_sandbox()` | `Paused` | VM is frozen; snapshot is optional |
| `Paused` | `resume_sandbox()` | `Running` | Resume from memory or from snapshot |
| `Running` | `stop_sandbox()` / `FATAL` / VMM exit | `Stopped` | |
| `Paused` | `stop_sandbox()` | `Stopped` | |
| `Stopped` | `delete_sandbox()` | `Deleted` | Resources released |
| Any | `delete_sandbox(force=true)` | `Deleted` | Force-kill VMM if needed |

### API Adapters: K8s and Direct

**K8s Adapter** provides full containerd compatibility:
- Implements `Sandboxer` trait: `create`/`start`/`stop`/`shutdown`/`sandbox` methods map to engine sandbox operations.
- Implements Task API (standard mode only): delegates to engine → `VmmTaskRuntime` → ttrpc → `vmm-task`. All Task API operations perform real guest-side work.
- Publishes containerd events (`TaskStart`, `TaskExit`, etc.).
- Requires `R: ContainerRuntime` bound — enforced at compile time. Appliance mode is not supported; use Direct Adapter instead.

**Direct Adapter** provides a native, minimal gRPC API with no Task/Container concepts. Designed for platforms that manage sandbox lifecycle directly (e.g., AI agent orchestrators, custom FaaS platforms). Exposes `CreateSandboxRequest` with an explicit `NetworkConfig` field for network device configuration. See [Direct Adapter Security Model](#direct-adapter-security-model) for socket access control.

### kuasar-builder: Two-Phase Template Build Pipeline

A new subproject providing an `OCI image → fast-boot template` pipeline. The pipeline has **two independent phases**, each producing artifacts that are useful on their own:

```
Phase 1 (Image Build):
  OCI image ──► flatten layers ──► rootfs.ext4 + image-ref.txt
                                    └──► (optional) blockmap + manifest + chunkstore

Phase 2 (Snapshot Build — optional, consumes Phase 1 output):
  rootfs.ext4 ──► boot VM ──► wait READY ──► pause + snapshot
                                               ├── CH snapshot bundle (config.json, state.json)
                                               ├── base-disk/ (qcow2 base chunked into blockmap/chunkstore)
                                               ├── memory-disk/ (memory file chunked, shared chunkstore)
                                               ├── overlay.qcow2
                                               └── snapshot.meta.json
```

**Phase 1: Image Build** produces the **Image Product**:
1. **Rootfs flattening**: OCI layers → rootfs directory (controlled mtime/uid/gid/sort order for determinism).
2. **Ext4 image**: rootfs directory → `rootfs.ext4` raw block image + `image-ref.txt`.
3. **(Optional) Chunking**: `rootfs.ext4` → content-addressed `blockmap/manifest/chunkstore` for on-demand disk loading via block-agent/artifactd. Enabled with `--emit-chunks`.

Phase 1 output alone is sufficient for **cold boot mode** — Kuasar can boot a VM directly from `rootfs.ext4` without any snapshot.

**Phase 2: Snapshot Build** produces the **Snapshot Product**:
1. **Base disk chunking**: Convert `rootfs.ext4` → qcow2 base, then chunk the qcow2 into `base-disk/`.
2. **VM boot**: Start VM using the selected VMM (CH or FC) with the chunked base disk or local rootfs.ext4.
3. **Wait READY**: Wait for the application to initialize and report readiness via the appliance protocol.
4. **Quiesce + Snapshot**: Pause the VM, call `GuestReadiness::prepare_snapshot()`, create the VMM-specific snapshot bundle, chunk the memory file into `memory-disk/`, and write `snapshot.meta.json` with version bindings.

The `snapshot.meta.json` produced by Phase 2 binds `vmm_type`, `vmm_version`, kernel/initrd digest, and agent version — these are validated by Kuasar before attempting restore (see [Snapshot Template Version Validation](#snapshot-template-version-validation)).

### Admission Controller

A node-level admission controller governs concurrent sandbox operations:

- **In-flight start limit**: Maximum concurrent sandbox starts per node.
- **Budget enforcement**: Per-sandbox `restore_timeout_ms`, `ready_timeout_ms`.
- **Degradation policy**: When limits are exceeded, reject or queue new starts with backpressure.

---

## Design Details

### Vmm Trait Definition

```rust
/// VMM lifecycle abstraction — completely independent of runtime mode.
trait Vmm: Send + Sync {
    /// Create a VM instance (allocate resources, do not start).
    async fn create(&mut self, config: VmConfig) -> Result<()>;

    /// Cold boot the VM.
    async fn boot(&mut self) -> Result<()>;

    /// Restore from a snapshot (VM is left in paused state).
    async fn restore(&mut self, snapshot: &SnapshotRef) -> Result<()>;

    /// Resume a paused VM.
    async fn resume(&mut self) -> Result<()>;

    /// Pause the VM.
    async fn pause(&mut self) -> Result<()>;

    /// Create a snapshot of the current VM state.
    /// VM must be in paused state. The snapshot is written to `dest`.
    /// CH: PUT /api/v1/vm.snapshot { "destination_url": "file://<dest>" }
    /// FC: PUT /snapshot/create { "snapshot_path": "<dest>", "mem_file_path": "..." }
    async fn snapshot(&mut self, dest: &SnapshotDest) -> Result<SnapshotInfo>;

    /// Stop the VM. If force=true, SIGKILL immediately.
    async fn stop(&mut self, force: bool) -> Result<()>;

    /// Wait for the VM process to exit, return exit info.
    async fn wait_exit(&self) -> Result<ExitInfo>;

    /// Add a block device. Must be called before boot() or restore().
    fn add_disk(&mut self, disk: DiskConfig) -> Result<()>;

    /// Add a network device. Must be called before boot() or restore().
    fn add_network(&mut self, net: NetworkConfig) -> Result<()>;

    /// Return the vsock path for host-guest communication.
    fn vsock_path(&self) -> Result<String>;

    /// Query VMM capabilities for graceful degradation.
    fn capabilities(&self) -> VmmCapabilities;
}

struct VmmCapabilities {
    hot_plug_disk: bool,
    hot_plug_net: bool,
    hot_plug_cpu: bool,
    hot_plug_mem: bool,
    pmem_dax: bool,
    diff_snapshot: bool,
    resize: bool,
}
```

VMM capability matrix:

| Capability | Cloud Hypervisor | Firecracker |
|-----------|-----------------|-------------|
| `hot_plug_disk` | ✅ | ❌ |
| `hot_plug_net` | ✅ | ❌ |
| `hot_plug_cpu` | ✅ | ❌ |
| `hot_plug_mem` | ✅ | ❌ |
| `pmem_dax` | ✅ | ❌ |
| `diff_snapshot` | ❌ | ✅ |
| `runtime_snapshot` | ✅ (`PUT /vm.snapshot`) | ✅ (`PUT /snapshot/create`) |
| `resize` | ✅ | ❌ |
| Typical restore latency | ~1.4–1.8s (4 GB VM, see [CH v50.0 Constraints](#ch-v500-implementation-constraints)) | ~5-25ms |

### GuestReadiness Trait Definition

```rust
/// Minimal guest-side operation abstraction for all runtime modes.
/// Both ApplianceRuntime and VmmTaskRuntime implement this trait.
trait GuestReadiness: Send + Sync {
    /// Wait for guest to become ready.
    /// Standard: ttrpc check() + setup_sandbox().
    /// Appliance: wait for READY JSON message on vsock.
    async fn wait_ready(&self, sandbox_id: &str) -> Result<ReadyResult>;

    /// Send a signal to a process.
    /// Standard: ttrpc signal_process().
    /// Appliance: send SHUTDOWN message via vsock (signal is mapped to deadline_ms).
    async fn kill_process(&self, sandbox_id: &str, container_id: &str,
                          pid: u32, signal: u32) -> Result<()>;

    /// Wait for a process to exit.
    /// Standard: ttrpc wait_process().
    /// Appliance: wait for VM process exit.
    async fn wait_process(&self, sandbox_id: &str, container_id: &str,
                          pid: u32) -> Result<ExitStatus>;

    /// Get container resource stats.
    /// Standard: ttrpc get_stats() — read guest cgroups.
    /// Appliance: read host-side cgroups of the VMM process.
    async fn container_stats(&self, sandbox_id: &str, container_id: &str)
        -> Result<ContainerStats>;

    /// Notify the guest to quiesce before snapshot (kuasar-builder Phase 2).
    /// Default no-op — appliance VMs quiesce on VMM pause without explicit signaling.
    async fn prepare_snapshot(&self, sandbox_id: &str) -> Result<()> {
        Ok(())
    }
}
```

### ContainerRuntime Trait Definition

```rust
/// Extended guest communication for standard mode (vmm-task ttrpc).
/// ONLY VmmTaskRuntime implements this trait.
/// ApplianceRuntime does NOT implement ContainerRuntime — only GuestReadiness.
/// The K8s Adapter requires R: ContainerRuntime; this is enforced at compile time.
trait ContainerRuntime: GuestReadiness {
    /// Create a container inside the VM.
    /// Standard: ttrpc create_container() — real namespace/cgroup/mount.
    async fn create_container(&self, sandbox_id: &str, spec: ContainerSpec)
        -> Result<ContainerInfo>;

    /// Start the main process inside a container.
    /// Standard: ttrpc start_process() — real fork+exec.
    async fn start_process(&self, sandbox_id: &str, container_id: &str)
        -> Result<ProcessInfo>;

    /// Execute an additional process inside a container.
    /// Standard: ttrpc exec_process() — real setns+fork+exec.
    async fn exec_process(&self, sandbox_id: &str, container_id: &str,
                          spec: ExecSpec) -> Result<ProcessInfo>;
}
```

**Implementation assignment**:
- `VmmTaskRuntime`: implements `ContainerRuntime` (superset — also satisfies `GuestReadiness`).
- `ApplianceRuntime`: implements `GuestReadiness` only. Does not provide `create_container` / `start_process` / `exec_process`.

### SandboxEngine Core

```rust
/// Core engine parameterized by VMM backend and guest readiness strategy.
/// All four mode combinations use this same struct.
struct SandboxEngine<V: Vmm, R: GuestReadiness> {
    vmm_factory: VmmFactory<V>,
    runtime: R,
    admission: AdmissionController,
    templates: TemplateManager,
    sandboxes: HashMap<String, SandboxInstance<V>>,
}

impl<V: Vmm, R: GuestReadiness> SandboxEngine<V, R> {
    /// Start a sandbox: restore or boot, then wait for readiness.
    /// lifecycle_phase informs Auto mode resolution and observability tagging.
    async fn start_sandbox(
        &mut self,
        id: &str,
        mode: StartMode,
        phase: LifecyclePhase,
    ) -> Result<StartResult> {
        // 1. Admission check
        self.admission.check(id)?;
        let sandbox = self.sandboxes.get_mut(id)?;
        let t0 = Instant::now();

        // 2. Resolve effective start mode
        let effective_mode = match mode {
            StartMode::Auto => match phase {
                LifecyclePhase::FirstLaunch => {
                    // Try template restore if compatible; degrade to boot if absent/incompatible
                    match self.templates.get_snapshot(id) {
                        Ok(snap) => match self.templates.validate_compat(&snap, &sandbox.vmm) {
                            Ok(()) => StartMode::RestoreEager,
                            Err(e) => {
                                self.emit_event("sandbox.start.degraded", &[
                                    ("reason", "template_compat_failed"),
                                    ("error", &e.to_string()),
                                    ("lifecycle_phase", "FirstLaunch"),
                                ]);
                                StartMode::Boot
                            }
                        },
                        Err(_) => StartMode::Boot,
                    }
                }
                // For non-FirstLaunch phases, Auto resolves to RestoreEager using
                // the runtime snapshot. If none exists, return an error — these phases
                // require state continuity (use ResumeSandbox, not StartSandbox).
                _ => {
                    return Err(Error::InvalidRequest(format!(
                        "StartMode::Auto with {:?} requires a runtime snapshot; use ResumeSandbox",
                        phase
                    )));
                }
            },
            other => other,
        };

        // 3. VMM start (via Vmm trait — VMM-agnostic)
        match effective_mode {
            StartMode::RestoreEager => {
                let snap = self.templates.get_snapshot(id)?;
                sandbox.vmm.restore(&snap).await?;
                sandbox.vmm.resume().await?;
            }
            StartMode::Boot => {
                sandbox.vmm.boot().await?;
            }
            _ => unreachable!("Auto resolved above"),
        }

        // 4. Wait for readiness (via GuestReadiness trait — mode-agnostic)
        let ready = self.runtime.wait_ready(id).await?;

        Ok(StartResult {
            ready_ms: t0.elapsed().as_millis() as u64,
            mode_used: effective_mode,
            ready_at: ready.timestamp,
        })
    }

    // --- Runtime Snapshot Lifecycle (see dedicated section below) ---

    /// Pause a running sandbox, optionally creating a runtime snapshot.
    async fn pause_sandbox(&mut self, id: &str, opts: PauseOptions)
        -> Result<PauseResult>;

    /// Resume a paused sandbox, or restore from a runtime snapshot.
    async fn resume_sandbox(&mut self, id: &str, opts: ResumeOptions)
        -> Result<ResumeResult>;

    /// Explicitly create a runtime snapshot without pausing for external use.
    async fn snapshot_sandbox(&mut self, id: &str, dest: SnapshotDest)
        -> Result<SnapshotResult>;
}
```

**Key design points**:
- `start_sandbox` takes `lifecycle_phase` — it informs `Auto` mode resolution and is tagged on all emitted events for observability.
- `Auto` with `FirstLaunch`: attempts template restore with graceful degradation to cold boot.
- `Auto` with other phases: returns an error directing the caller to use `ResumeSandbox` (which always uses a runtime snapshot).
- Process startup wiring (four combinations, zero runtime branching):

```rust
fn main() {
    let config = load_config();

    match (config.vmm_type, config.runtime_mode) {
        (Vmm::CloudHypervisor, Mode::Standard) => {
            let engine = SandboxEngine::new(
                VmmFactory::<CloudHypervisorVmm>::new(config.ch),
                VmmTaskRuntime::new(config.ttrpc),   // impl ContainerRuntime
            );
            K8sAdapter::new(engine).serve();  // K8sAdapter<V, R: ContainerRuntime>
        }
        (Vmm::CloudHypervisor, Mode::Appliance) => {
            let engine = SandboxEngine::new(
                VmmFactory::<CloudHypervisorVmm>::new(config.ch),
                ApplianceRuntime::new(config.vsock), // impl GuestReadiness only
            );
            DirectAdapter::new(engine).serve();  // DirectAdapter<V, R: GuestReadiness>
        }
        (Vmm::Firecracker, Mode::Standard) => {
            let engine = SandboxEngine::new(
                VmmFactory::<FirecrackerVmm>::new(config.fc),
                VmmTaskRuntime::new(config.ttrpc),
            );
            K8sAdapter::new(engine).serve();
        }
        (Vmm::Firecracker, Mode::Appliance) => {
            let engine = SandboxEngine::new(
                VmmFactory::<FirecrackerVmm>::new(config.fc),
                ApplianceRuntime::new(config.vsock),
            );
            DirectAdapter::new(engine).serve();
        }
    }
}
```

### Template Discovery

Template artifacts are discovered from the configured `template_dir`. The directory layout is:

```
{template_dir}/
  {image_ref_hash}/
    {vmm_type}/
      {vmm_version}/
        snapshot.meta.json
        config.json
        state.json
        memory-ranges
        overlay.qcow2
        base-disk/      (optional, chunked base disk)
        memory-disk/    (optional, chunked memory)
```

Where:
- `image_ref_hash` = SHA-256 of the normalized OCI image reference (e.g., `sha256:abc...` of `docker.io/myorg/myapp:v1.2.3`)
- `vmm_type` = `cloud-hypervisor` or `firecracker`
- `vmm_version` = VMM binary version string (e.g., `v50.0`)

**Discovery lifecycle**:
1. On startup, `TemplateManager` scans `template_dir` and indexes all valid `snapshot.meta.json` files into an in-memory map keyed by `(image_ref_hash, vmm_type, vmm_version)`.
2. An inotify-based watcher (via the `notify` crate) monitors `template_dir` for new entries. When a new `snapshot.meta.json` appears, the engine atomically updates the template index without restart.
3. Template association at start time: When a `CreateSandboxRequest` includes an `image_ref`, the engine computes `SHA-256(normalize(image_ref))` and looks up the corresponding template. If a matching template exists and passes `validate_compat()`, it is used for `StartMode::Auto` or `StartMode::RestoreEager`.

**Template GC**: Templates older than a configurable `template_ttl_hours` (default: 168 = 7 days) are eligible for garbage collection on the next startup scan. Templates actively in use by running sandboxes are never GC'd.

### Runtime Snapshot Lifecycle

Runtime snapshots enable pause/resume, cross-node migration, and node drain. Unlike template snapshots (which are created at build time from a clean post-`READY` state), runtime snapshots capture arbitrary in-flight state of a running VM.

**Snapshot type taxonomy**:

| Type | Scope | Created by | Typical Use |
|---|---|---|---|
| **Template Snapshot** | Image-level, shared | kuasar-builder (build time) | First launch of new instances |
| **Runtime Snapshot** | Instance-specific | `pause_sandbox` / `snapshot_sandbox` (run time) | Pause/resume, migration, drain |

**Key distinction**: A VM originally started via `ColdBoot` (no template baseline) produces a runtime snapshot containing **full guest memory** — there is no template to diff against. Content-addressed dedup (zero pages, kernel text, shared libraries) is the primary storage optimization.

**Type definitions**:

```rust
struct PauseOptions {
    create_snapshot: bool,         // Create a persistent runtime snapshot
    snapshot_chunk_enabled: bool,  // Chunk-ify and upload to external content store
    drain_timeout_ms: u64,         // Wait for in-flight requests before force-pause
}

struct PauseResult {
    paused_at: Timestamp,
    snapshot_ref: Option<String>,       // e.g. "snapshot://rt-sb-123-1709308800"
    snapshot_type: String,              // "runtime"
    chunk_upload_status: Option<String>, // "in_progress" | "completed" | "failed" | "skipped"
    chunks_total: u64,
    chunks_uploaded: u64,
    chunks_deduped: u64,
}

struct ResumeOptions {
    snapshot_ref: Option<String>,       // None = resume from in-memory paused state
    lifecycle_phase: LifecyclePhase,
}

enum LifecyclePhase {
    FirstLaunch,
    UserPauseResume,
    BMSMigration,
    NodeDrain,
}

struct ResumeResult {
    ready_ms: u64,
    mode_used: StartMode,
    snapshot_type: String,              // "template" | "runtime"
    lifecycle_phase: LifecyclePhase,
}

struct SnapshotDest {
    output_dir: String,
    chunk_enabled: bool,
}

struct SnapshotResult {
    snapshot_ref: String,
    meta_path: String,
    chunk_upload_status: Option<String>,
}
```

**`pause_sandbox` flow** (chunk upload is decoupled — does not block VM frozen state):

```rust
async fn pause_sandbox(&mut self, id: &str, opts: PauseOptions) -> Result<PauseResult> {
    let sandbox = self.sandboxes.get_mut(id)?;

    // 1. Drain: best-effort wait for in-flight requests.
    // On timeout, pause proceeds unconditionally; drain_timed_out=true is set in the response
    // so callers can observe that in-flight requests may have been torn.
    let drain_timed_out = if opts.drain_timeout_ms > 0 {
        tokio::time::timeout(
            Duration::from_millis(opts.drain_timeout_ms),
            sandbox.drain_requests(),
        ).await.is_err() // true = timed out
    } else {
        false
    };

    // 2. Pause VM (via Vmm trait — VMM-agnostic)
    sandbox.vmm.pause().await?;

    let mut result = PauseResult { paused_at: Timestamp::now(), ..Default::default() };

    // 3. Optionally create runtime snapshot (VM stays paused during snapshot write)
    if opts.create_snapshot {
        let dest = SnapshotDest { /* ... */ };
        let snap_info = sandbox.vmm.snapshot(&dest).await?;
        result.snapshot_ref = Some(snap_info.snapshot_ref.clone());
        result.snapshot_type = "runtime".into();

        // 4. Chunk upload is launched as a background task — VM is NOT kept frozen.
        // PauseSandboxResponse returns immediately with chunk_upload_status="in_progress".
        // The caller polls GetSnapshotUploadStatus(snapshot_ref) for completion.
        if opts.snapshot_chunk_enabled {
            let uploader = self.chunk_uploader.clone();
            let snapshot_ref = snap_info.snapshot_ref.clone();
            tokio::spawn(async move {
                uploader.chunk_and_upload(snapshot_ref).await;
            });
            result.chunk_upload_status = Some("in_progress".into());
        }
    }

    Ok(result)
}
```

**Design rationale — background chunk upload**: The chunk-ify and HTTP upload (potentially hundreds of megabytes) must not hold the VM in a frozen paused state. The snapshot file is fully written before the background task starts, so the VM can be stopped (or re-used if the snapshot is complete) while upload continues asynchronously. The `GetSnapshotUploadStatus` RPC on the Direct Adapter allows the caller to poll for completion before initiating cross-node restore.

**Known limitation — no crash recovery for in-progress uploads**: If the Kuasar process restarts while a chunk upload is in progress, the upload task is lost. The snapshot file remains intact on local disk and is usable for same-node resume. The caller must re-trigger `PauseSandbox` (or call `SnapshotSandbox`) to restart the upload. A future enhancement may persist upload state to disk (e.g., `snapshot.upload.json`) to enable automatic resume on restart.

**Chunk upload integration**: The `chunk_and_upload` helper calls an external content delivery system (e.g., artifactd) via HTTP:
1. Slice `memory-ranges` file into fixed-size chunks (512 KiB)
2. `POST /v1/chunks/exists` — batch check which chunks already exist (dedup)
3. `PUT /v1/chunks/{chunk_id}` — upload only new chunks (idempotent, content-addressed)
4. Write `memory.blockmap.json` + `runtime_snapshot.meta.json`

This is an **optional, degradation-safe** integration. If the external content store is unavailable, the snapshot file remains intact on local disk and can be used for same-node resume.

### Boot Mode Selection: Benchmark-Informed Heuristics

Snapshot restore is **not always faster** than cold boot. Benchmarks on CH v50.0 (`/bench/cold_boot_vs_snapshot_restore.md`) reveal a critical crossover:

| Workload | Cold Boot | Snapshot Restore | Faster Mode |
|----------|-----------|-----------------|-------------|
| nginx-slim (260 MB working set, 4 GB allocated) | **499 ms** | 1,520 ms | Cold boot (3× faster) |
| nginx-fat (3.5 GB working set, 4 GB allocated) | 2,045 ms | **1,831 ms** | Snapshot restore (marginal) |

**Root cause**: CH v50.0's `fill_saved_regions()` reads the **entire** `memory-ranges` file synchronously (~1.4s warm, ~1.8s cold for 4 GB), regardless of how much guest memory is actually used. For lightweight workloads where application startup is faster than this memory read, cold boot wins.

**Two-dimensional decision**: Boot mode selection is `f(workload_type, lifecycle_phase)`, not just `f(workload_type)`. The lifecycle phase determines snapshot type and decision rules:

| Lifecycle Phase | Snapshot Type | Decision Rule |
|----------------|---------------|---------------|
| `FirstLaunch` | Template snapshot (if available) | Use crossover heuristic below |
| `UserPauseResume` | Runtime snapshot (required) | Always `SnapshotRestore` — must restore in-flight state |
| `BMSMigration` | Runtime snapshot (cross-node) | Always `SnapshotRestore` — must restore in-flight state |
| `NodeDrain` | Runtime snapshot (cross-node) | Always `SnapshotRestore` — must restore in-flight state |

For `FirstLaunch`, the caller applies the crossover heuristic:

```
if snapshot_available
   AND snapshot_template_compatible
   AND estimated_app_startup_cost > estimated_snapshot_restore_cost:
    request StartMode::RestoreEager
else:
    request StartMode::Boot
```

Where `estimated_snapshot_restore_cost` ≈ 1.4–1.8s for a 4 GB VM on current CH v50.0 (dominated by synchronous memory read). This crossover point drops dramatically with future CH optimizations (mmap-based lazy restore would reduce slim restore to ~50 ms).

For `UserPauseResume` / `BMSMigration` / `NodeDrain`, the heuristic does not apply — the VM must resume from its runtime snapshot to preserve application state. If the runtime snapshot is unavailable, the error `RUNTIME_SNAPSHOT_NOT_FOUND` is returned.

**Kuasar's role**: Kuasar itself does **not** auto-select the optimal mode. The caller specifies `StartMode` and `LifecyclePhase` in the request. When `StartMode::Auto` is used for `FirstLaunch`, Kuasar attempts restore if a compatible template exists and falls back to boot otherwise.

**API contract**: `StartSandbox` is **only valid** with `LifecyclePhase::FirstLaunch`. For `UserPauseResume`, `BMSMigration`, and `NodeDrain`, the caller must use `ResumeSandbox` — these phases require restoring in-flight application state from a runtime snapshot, which is semantically distinct from starting a sandbox for the first time. Calling `StartSandbox` with a non-`FirstLaunch` phase returns `INVALID_ARGUMENT`.


### Artifact-to-Start-Mode Mapping

Each `StartMode` requires specific artifacts from kuasar-builder. The following table maps start modes to their required inputs and disk setup:

| Start Mode | Required Artifacts | Disk Setup | Memory Setup |
|---|---|---|---|
| `Boot` | `rootfs.ext4` + kernel + initrd (Image Product Phase 1) | `rootfs.ext4` passed as virtio-blk disk | Standard `--memory size=NM` |
| `Boot` + lazy disk | `blockmap/manifest/chunkstore` + kernel + initrd (Image Product Phase 1 with `--emit-chunks`) | block-agent FUSE mount → virtio-blk | Standard `--memory size=NM` |
| `RestoreEager` | CH snapshot bundle + `overlay.qcow2` (Snapshot Product Phase 2) | Disk config from snapshot's `config.json`; only overlay needs host-side preparation | Memory from `memory-ranges` file (CH reads it synchronously) |
| `RestoreEager` + lazy disk | ⚠️ **Deferred** — requires CH upstream fix. Memory-zone file-backed restore is broken in CH v50.0 (see [CH v50.0 Implementation Constraints](#ch-v500-implementation-constraints), Constraint 3). This row describes the target design once CH upstream patches `memory-zone` restore. | block-agent mounts base via FUSE; overlay.qcow2 layered on top | Memory via block-agent FUSE mount of `memory-disk/` chunks (memory-zone file-backed) |
| `Auto` | Both Image Product and Snapshot Product available | Engine tries restore path first; falls back to boot path on template incompatibility or absence | Per resolved mode |

**Key implementation detail for restore mode**: In `RestoreEager`, the VMM's `restore()` implementation must **not** pass `--kernel`, `--memory`, `--cpus`, or `--disk` CLI flags — CH reads all configuration from the snapshot's `config.json`. Only `--restore source_url=file://<path>` and `--api-socket` should be passed. See [CH v50.0 Implementation Constraints](#ch-v500-implementation-constraints).

### RootfsProvider Trait: Pluggable Disk Backend

The `RootfsProvider` trait decouples Kuasar from any specific disk image format or content delivery mechanism. Two implementations are provided:

```rust
/// Provides a disk image file for VM boot or restore.
/// The returned path is passed to Vmm::add_disk().
trait RootfsProvider: Send + Sync {
    /// Prepare a disk image. For boot mode, this creates/mounts the rootfs.
    /// For restore mode, this prepares the base disk (if lazy loading is used).
    /// Returns the file path to be used as the virtio-blk backing file.
    async fn prepare(&self, req: RootfsPrepareRequest) -> Result<DiskPath>;

    /// Release resources (unmount FUSE, stop block-agent) after VM exit.
    async fn release(&self, sandbox_id: &str) -> Result<()>;
}

struct RootfsPrepareRequest {
    sandbox_id: String,
    image_ref: String,
    cache_domain: String,
    /// If set, prepare for restore mode (base disk + overlay).
    /// If None, prepare for boot mode (raw rootfs).
    snapshot_ref: Option<String>,
}

/// Returns either a direct file path or a FUSE mount path.
enum DiskPath {
    /// Direct file path (e.g., /path/to/rootfs.ext4).
    Direct(PathBuf),
    /// FUSE mount path — caller must keep the provider alive until release().
    FuseMount { path: PathBuf, overlay: Option<PathBuf> },
}
```

**LocalRootfsProvider** (simple, for development and lightweight deployments):
- `prepare()`: Returns the path to `rootfs.ext4` directly. No lazy loading.
- `release()`: No-op.

**BlockAgentRootfsProvider** (production, for lazy disk loading):
- `prepare()`: Starts an `artifactd` instance (if not already running), starts a `block-agent` process with the blockmap, mounts the FUSE filesystem, and returns the FUSE file path.
- `release()`: Unmounts FUSE, stops block-agent process, cleans up cache directory.

### Snapshot Template Version Validation

Before attempting snapshot restore, the engine validates the `snapshot.meta.json` against the current node environment. This prevents restore failures from version mismatches and ensures predictable degradation.

**`snapshot.meta.json` structure**:

```json
{
  "vmm_type": "cloud-hypervisor",
  "vmm_version": "v50.0",
  "guest": {
    "kernel": "sha256:abc...",
    "initrd": "sha256:def...",
    "agent_version": "1.2.3"
  },
  "compat": {
    "requires": ["virtio-blk", "vsock"]
  }
}
```

**Validated fields**:

| Field | Validation | On Mismatch |
|-------|-----------|-------------|
| `vmm_type` | Must match the configured VMM type (e.g., `cloud-hypervisor`) | Degrade to `Boot` |
| `vmm_version` | Must match the installed VMM binary version | Degrade to `Boot` |
| `guest.kernel` | Digest must match the configured kernel | Degrade to `Boot` |
| `guest.initrd` | Digest must match the configured initrd | Degrade to `Boot` |
| `guest.agent_version` | Must match the appliance SDK version bundled with the runtime | Degrade to `Boot` |
| `compat.requires` | All required devices must be available | Degrade to `Boot` |

```rust
impl TemplateManager {
    fn validate_compat(&self, snap: &SnapshotRef, vmm: &dyn Vmm) -> Result<()> {
        let meta = self.load_meta(snap)?;

        // VMM type must match (CH snapshot cannot load on FC and vice versa)
        if meta.vmm_type != self.node_vmm_type {
            return Err(Error::CompatMismatch(format!(
                "vmm_type: snapshot={} node={}", meta.vmm_type, self.node_vmm_type
            )));
        }

        // VMM version must match exactly (no cross-version snapshot compat guarantee)
        if meta.vmm_version != self.node_vmm_version {
            return Err(Error::CompatMismatch(format!(
                "vmm_version: snapshot={} node={}",
                meta.vmm_version, self.node_vmm_version
            )));
        }

        // Kernel and initrd digests must match
        if meta.guest.kernel != self.node_kernel_digest {
            return Err(Error::CompatMismatch("kernel digest mismatch".into()));
        }
        if meta.guest.initrd != self.node_initrd_digest {
            return Err(Error::CompatMismatch("initrd digest mismatch".into()));
        }

        // Agent version must match the bundled appliance SDK
        if !meta.guest.agent_version.is_empty()
            && meta.guest.agent_version != self.node_agent_version
        {
            return Err(Error::CompatMismatch(format!(
                "agent_version: snapshot={} node={}",
                meta.guest.agent_version, self.node_agent_version
            )));
        }

        // Required device capabilities.
        // Unknown requirements are REJECTED by default — a future capability requirement
        // that this node cannot satisfy must not silently pass.
        let caps = vmm.capabilities();
        for req in &meta.compat.requires {
            match req.as_str() {
                "virtio-blk" | "vsock" => {} // always available on supported VMMs
                "vfio" if caps.vfio => {}
                "vfio" => {
                    return Err(Error::CompatMismatch("vfio required but unavailable".into()));
                }
                other => {
                    return Err(Error::CompatMismatch(format!(
                        "unknown capability requirement: {other}"
                    )));
                }
            }
        }

        Ok(())
    }
}
```

**Version domain strategy**: During rolling upgrades, new VMM versions produce new snapshot templates in a new version domain. Old templates remain valid until the old VMM version is fully decommissioned. The scheduler should prefer nodes whose version domain matches the template's `vmm_version` (cache-aware scheduling). Mismatched nodes degrade to cold boot with an observable `sandbox.start.degraded` event.

### K8s Adapter: Sandbox and Task API Mapping

```rust
// Sandboxer trait — maps to engine sandbox operations
impl<V: Vmm, R: ContainerRuntime> Sandboxer for K8sAdapter<V, R> {
    async fn create(&self, id: &str, info: SandboxOption) -> Result<()> {
        let config = self.parse_sandbox_config(info)?;
        self.engine.create_sandbox(id, config).await
    }
    async fn start(&self, id: &str) -> Result<()> {
        self.engine.start_sandbox(id, StartMode::Auto, LifecyclePhase::FirstLaunch).await?;
        self.publish_event(TaskStart { pid: 1, container_id: id }).await;
        Ok(())
    }
    async fn stop(&self, id: &str, force: bool) -> Result<()> {
        let deadline = if force { 0 } else { self.graceful_timeout };
        self.engine.stop_sandbox(id, deadline).await
    }
}

// Task API — delegates to engine guest operations
// R: ContainerRuntime ensures exec_process is available (compile-time check)
impl<V: Vmm, R: ContainerRuntime> TaskService for K8sAdapter<V, R> {
    async fn create(&self, req: CreateTaskRequest) -> Result<CreateTaskResponse> {
        let info = self.engine.create_container(&req.id, req.into()).await?;
        Ok(CreateTaskResponse { pid: info.pid })
    }
    async fn exec(&self, req: ExecProcessRequest) -> Result<()> {
        self.engine.exec_process(&req.sandbox_id, &req.container_id,
                                  req.into()).await?;
        Ok(())
    }
    async fn kill(&self, req: KillRequest) -> Result<()> {
        self.engine.kill_process(&req.sandbox_id, &req.container_id,
                                 req.pid, req.signal).await
    }
    async fn wait(&self, req: WaitRequest) -> Result<WaitResponse> {
        let exit = self.engine.wait_process(&req.sandbox_id,
                       &req.container_id, req.pid).await?;
        Ok(WaitResponse { exit_status: exit.code, exited_at: exit.timestamp })
    }
}
```

The K8s Adapter is **exclusively** for standard mode (`R: ContainerRuntime`). Appliance mode always uses the Direct Adapter — the containerd Task API has no semantic mapping to appliance workloads (no exec, no attach, no multi-container). Attempting to instantiate `K8sAdapter` with `ApplianceRuntime` (which only implements `GuestReadiness`, not `ContainerRuntime`) is a compile-time error by design.

### Direct Adapter: Native Sandbox API

```protobuf
service SandboxService {
    rpc CreateSandbox(CreateSandboxRequest) returns (Sandbox);
    rpc StartSandbox(StartSandboxRequest) returns (StartSandboxResponse);
    rpc StopSandbox(StopSandboxRequest) returns (google.protobuf.Empty);
    rpc DeleteSandbox(DeleteSandboxRequest) returns (google.protobuf.Empty);
    rpc GetSandbox(GetSandboxRequest) returns (Sandbox);
    rpc ListSandboxes(ListSandboxesRequest) returns (ListSandboxesResponse);
    rpc WatchSandbox(WatchSandboxRequest) returns (stream SandboxEvent);

    // Runtime Snapshot Lifecycle (appliance mode)
    rpc PauseSandbox(PauseSandboxRequest) returns (PauseSandboxResponse);
    rpc ResumeSandbox(ResumeSandboxRequest) returns (ResumeSandboxResponse);
    rpc SnapshotSandbox(SnapshotSandboxRequest) returns (SnapshotSandboxResponse);

    // Async chunk upload status (returned immediately in PauseSandboxResponse)
    rpc GetSnapshotUploadStatus(GetSnapshotUploadStatusRequest)
        returns (SnapshotUploadStatusResponse);
}

// The caller is responsible for creating and configuring the tap interface on the host
// before calling CreateSandbox. Kuasar passes tap_device directly to the VMM as a
// virtio-net backend and does not create or destroy the tap. In K8s deployments, CNI
// creates the tap; in Direct Adapter deployments, the orchestrator (AI agent platform,
// FaaS controller) must pre-create the tap (e.g., via `ip tuntap add`).
message NetworkConfig {
    string tap_device = 1;      // host tap interface name (e.g., "tap0"); must already exist
    string ip_cidr = 2;         // guest IP with prefix (e.g., "10.0.0.2/24")
    string gateway = 3;         // gateway IP
    string mac = 4;             // MAC address (optional, auto-generated if empty)
    repeated string dns = 5;    // DNS resolver addresses
}

message CreateSandboxRequest {
    string sandbox_id = 1;
    string image_ref = 2;
    uint32 vcpus = 3;
    uint64 memory_mb = 4;
    NetworkConfig network = 5;
    map<string, string> labels = 6;
    optional string template_id = 7;
}

// StartSandbox is ONLY valid with LifecyclePhase::FIRST_LAUNCH.
// For UserPauseResume / BMSMigration / NodeDrain use ResumeSandbox instead.
// Passing a non-FIRST_LAUNCH phase returns INVALID_ARGUMENT.
message StartSandboxRequest {
    string sandbox_id = 1;
    StartMode start_mode = 2;        // AUTO, RESTORE_EAGER, BOOT
    // template_id selects a specific template for RestoreEager / Auto modes.
    // The engine resolves the snapshot path from the TemplateStore.
    // For runtime-snapshot resume (cross-node migration) use ResumeSandbox.snapshot_ref instead.
    optional string template_id = 3;
    uint64 ready_timeout_ms = 4;
    // Must be FIRST_LAUNCH; any other value returns INVALID_ARGUMENT.
    LifecyclePhase lifecycle_phase = 5;
}

message StartSandboxResponse {
    string sandbox_id = 1;
    uint64 ready_ms = 2;          // Total time from StartSandbox RPC to READY
    uint64 vmm_start_ms = 3;      // VMM process start duration (boot path only; 0 for restore)
    uint64 restore_ms = 4;        // VMM restore duration (restore path only; 0 for boot)
    uint64 resume_ms = 5;         // VMM resume duration (restore path only; 0 for boot)
    StartMode mode_used = 6;      // Actual mode used (may degrade from AUTO → BOOT)
    string snapshot_type = 7;     // "template" | "" (only set when mode_used=RESTORE_EAGER)
}

enum LifecyclePhase {
    FIRST_LAUNCH = 0;
    USER_PAUSE_RESUME = 1;
    BMS_MIGRATION = 2;
    NODE_DRAIN = 3;
}

message PauseSandboxRequest {
    string sandbox_id = 1;
    PauseMode pause_mode = 2;              // SNAPSHOT or SUSPEND
    bool snapshot_chunk_enabled = 3;       // chunk-ify runtime snapshot and upload
    uint64 drain_timeout_ms = 4;           // best-effort request drain before pause
    LifecyclePhase lifecycle_phase = 5;
}

enum PauseMode {
    SNAPSHOT = 0;   // Pause + create runtime snapshot (persistent, supports cross-node resume)
    SUSPEND = 1;    // Pause only (in-memory, same-node resume only)
}

message PauseSandboxResponse {
    string sandbox_id = 1;
    optional string snapshot_ref = 2;
    // "in_progress" | "completed" | "partial" | "failed" | "skipped"
    // "in_progress" when snapshot_chunk_enabled=true (upload is background)
    string chunk_upload_status = 3;
    uint64 chunks_total = 4;
    uint64 chunks_uploaded = 5;
    uint64 chunks_deduped = 6;
    // true if drain_timeout_ms elapsed before in-flight requests completed.
    // The VM was paused anyway; the caller may observe torn in-flight requests.
    bool drain_timed_out = 7;
}

message GetSnapshotUploadStatusRequest {
    string snapshot_ref = 1;
}

message SnapshotUploadStatusResponse {
    string snapshot_ref = 1;
    string status = 2;          // "in_progress" | "completed" | "failed"
    uint64 chunks_total = 3;
    uint64 chunks_uploaded = 4;
    uint64 chunks_deduped = 5;
    optional string error = 6;
}

message ResumeSandboxRequest {
    string sandbox_id = 1;
    optional string snapshot_ref = 2;       // None = resume from in-memory paused state
    LifecyclePhase lifecycle_phase = 3;
    uint64 restore_timeout_ms = 4;
    uint64 ready_timeout_ms = 5;
}

// How the resume was executed.
enum ResumeStrategy {
    IN_MEMORY = 0;       // resumed from in-memory Paused state; no snapshot I/O
    FROM_SNAPSHOT = 1;   // restored from snapshot_ref before resume
}

message ResumeSandboxResponse {
    string sandbox_id = 1;
    uint64 ready_ms = 2;
    ResumeStrategy resume_strategy = 3;  // how the resume was executed
    LifecyclePhase lifecycle_phase = 4;
}

message SnapshotSandboxRequest {
    string sandbox_id = 1;
    string output_dir = 2;
    bool chunk_enabled = 3;
}

// SnapshotSandbox internally performs: vmm.pause → vmm.snapshot → vmm.resume.
// The VM is briefly frozen during the snapshot write; from the caller's perspective
// the sandbox remains Running before and after the RPC.
// pause_duration_ms measures the frozen window (typically < 200 ms for local NVMe).
message SnapshotSandboxResponse {
    string snapshot_ref = 1;
    string meta_path = 2;
    optional string chunk_upload_status = 3;
    uint64 pause_duration_ms = 4;   // time VM was frozen (pause → snapshot complete → resume)
}
```

The Direct Adapter exposes no Task/Container concepts and provides richer response data (timing breakdowns, degradation status) that the K8s Adapter cannot surface through the containerd event model.

**K8s Adapter does NOT expose Pause/Resume/Snapshot**: The containerd `Sandboxer` trait has no `Pause`/`Resume`/`Snapshot` methods. Runtime snapshot lifecycle is only available through the Direct Adapter. This is by design — K8s workloads use pod eviction and rescheduling for lifecycle management, not VM-level snapshot/restore.

### Direct Adapter Security Model

The Direct Adapter gRPC service is bound to a Unix domain socket. Since it provides full control over sandbox lifecycle (including snapshot and migration operations), access must be restricted to authorized callers.

**Socket permissions**:
- Default socket path: `/run/kuasar/engine.sock`
- Default permissions: `0660`, owned by `root:kuasar`
- Only processes running under the `kuasar` group (the calling orchestrator — AI agent platform, FaaS controller) can connect

**Authentication**:
- Unix socket peer credentials (`SO_PEERCRED`) provide the caller's UID/GID, which the server validates against the configured `allow_gids` list.
- mTLS is out of scope for this proposal but can be added as a future enhancement for multi-tenant or remote direct access.

**Sandbox vsock isolation**:
- Each sandbox's vsock connection is keyed by `sandbox_id` (via the vsock CID assigned at VM creation).
- The host rejects vsock connections from CIDs not associated with a known `sandbox_id`.
- Sandboxes cannot reach each other via vsock — the host only routes vsock traffic between a sandbox and its assigned engine handler.

**Audit logging**:
- All Direct Adapter RPC calls are logged with caller PID, UID, sandbox_id, and operation. These logs are distinct from sandbox lifecycle events and are written to the audit log path configured in `[adapter] audit_log`.

### Appliance Protocol Specification

**Transport**: vsock (Linux) or hvsock (Hyper-V). Guest listens on a configurable port (default: 8192).

**Encoding**: JSON Lines (one JSON object per line). Chosen for simplicity and debuggability; can be upgraded to a binary protocol later without changing semantics.

**Message sequence** (nominal path):

```
Host                              Guest
 │                                  │
 │──── connect vsock port 8192 ────►│
 │                                  │
 │──── CONFIG (optional) ──────────►│  inject env/hostname/dns before app starts
 │                                  │
 │                                  │◄── READY ───── application is ready to serve
 │                                  │
 │                                  │◄── HEARTBEAT ── periodic liveness (if configured)
 │                                  │
 │◄─── PING / PONG ────────────────►│  connectivity probes
 │                                  │
 │──── SHUTDOWN ───────────────────►│  graceful shutdown request
 │                                  │
 │                              [exit]
```

**Guest readiness contract**:
- PID1 (or an init script) sets up the application environment and starts the application.
- Once the application can serve requests, PID1 sends `{"type":"READY","sandbox_id":"..."}` on the vsock connection.
- On receiving `{"type":"SHUTDOWN","deadline_ms":30000}`, PID1 must initiate graceful shutdown within the specified deadline.
- If PID1 encounters an unrecoverable error, it sends `{"type":"FATAL","sandbox_id":"...","reason":"..."}`.
- If `heartbeat_timeout_ms > 0` (configured on the host), PID1 must send periodic `HEARTBEAT` messages to maintain liveness.

**Host behavior**:
- After VMM resume, the host connects to the vsock port.
- If `CONFIG` injection is configured, the host sends the CONFIG message first (before waiting for READY).
- Host waits for `READY`. If not received within `ready_timeout_ms`, the host kills the VMM process and reports `READY_TIMEOUT`.
- After READY, the host starts the heartbeat timer (if `heartbeat_timeout_ms > 0`).
- If no HEARTBEAT is received within `heartbeat_timeout_ms`, the host takes `heartbeat_action` (`"kill"` or `"warn"`).

**Message schemas**:

```json
// Guest → Host: READY
{"type":"READY","sandbox_id":"sb-123","app_version":"1.2.3"}

// Guest → Host: HEARTBEAT
{"type":"HEARTBEAT","sandbox_id":"sb-123","timestamp_ms":1709308800000}

// Guest → Host: FATAL
{"type":"FATAL","sandbox_id":"sb-123","reason":"OOM: process killed"}

// Host → Guest: CONFIG
{"type":"CONFIG","version":1,"sandbox_id":"sb-123",
 "env":{"AGENT_ID":"abc"},"hostname":"sb-123",
 "dns":["8.8.8.8"],"search_domains":["internal.example.com"]}

// Host → Guest: SHUTDOWN
{"type":"SHUTDOWN","deadline_ms":30000}

// Guest → Host: METRICS (periodic resource snapshot; optional)
{"type":"METRICS","sandbox_id":"sb-123","timestamp_ms":1709308800000,
 "cpu_usage_ms":1234,"memory_rss_bytes":104857600}

// Host → Guest: PING
{"type":"PING"}
// Guest → Host: PONG
{"type":"PONG","sandbox_id":"sb-123"}
```

### Configuration

```toml
# kuasar.toml

[engine]
runtime_mode = "appliance"    # "appliance" | "standard"
vmm_type = "cloud-hypervisor" # "cloud-hypervisor" | "firecracker"

[adapter]
type = "direct"               # "direct" | "k8s"
listen = "unix:///run/kuasar/engine.sock"
socket_mode = "0660"
socket_group = "kuasar"
audit_log = "/var/log/kuasar/audit.log"

[appliance]
ready_timeout_ms = 5000
vsock_port = 8192             # port >= 1024 required; use >= 8192 to avoid privileged range
shutdown_timeout_ms = 30000
heartbeat_timeout_ms = 15000  # 0 = heartbeat monitoring disabled
heartbeat_action = "kill"     # "kill" | "warn"

[admission]
max_concurrent_starts = 10
restore_timeout_ms = 3000

[templates]
template_dir = "/var/lib/kuasar/templates"
template_ttl_hours = 168      # 7 days; 0 = never expire

[cloud-hypervisor]
binary = "/usr/bin/cloud-hypervisor"
api_socket_timeout_ms = 5000      # timeout for CH REST API calls (boot/restore/pause/snapshot)
# serial_output_dir must be writable on the restore host (CH v50.0 Constraint 4).
# On restore, CH reopens the serial path recorded in config.json. Kuasar rewrites
# the path to serial_output_dir/{sandbox_id}.log before invoking --restore.
serial_output_dir = "/var/log/kuasar/ch"

[firecracker]
binary = "/usr/bin/firecracker"
api_socket_timeout_ms = 3000
```

### Observability and Events

The engine emits structured events at each lifecycle stage:

**Start lifecycle (`sandbox.start.*`)**:

| Event | Stage | Description |
|-------|-------|-------------|
| `sandbox.start.requested` | Entry | Start request received by engine |
| `sandbox.start.admission_passed` | Admission | Admission check passed |
| `sandbox.start.restore_begin` | VMM | Snapshot restore initiated |
| `sandbox.start.restore_end` | VMM | Snapshot restore completed |
| `sandbox.start.resume` | VMM | VMM resume completed |
| `sandbox.start.ready` | Guest | Guest reported READY |
| `sandbox.start.ready_timeout` | Guest | READY timeout (failure path) |
| `sandbox.start.degraded` | Engine | Start mode degraded (e.g., restore → boot) |
| `sandbox.start.rejected` | Admission | Start rejected (e.g., `NODE_RETIRED`) |

**Stop lifecycle (`sandbox.stop.*`)**:

| Event | Stage | Description |
|-------|-------|-------------|
| `sandbox.stop.requested` | Entry | Stop request received |
| `sandbox.stop.shutdown_sent` | Guest | SHUTDOWN message sent to guest |
| `sandbox.stop.completed` | Exit | VM process exited |

**Pause lifecycle (`sandbox.pause.*`)**:

| Event | Stage | Description |
|-------|-------|-------------|
| `sandbox.pause.requested` | Entry | Pause request received (tag: `lifecycle_phase`) |
| `sandbox.pause.drain_begin` | Drain | Request drain started |
| `sandbox.pause.drain_end` | Drain | Request drain completed or timed out |
| `sandbox.pause.vm_pause_begin` | VMM | VMM pause initiated |
| `sandbox.pause.vm_pause_end` | VMM | VMM pause completed |
| `sandbox.pause.completed` | Exit | Pause operation completed |

**Liveness (`sandbox.liveness.*`)**:

| Event | Stage | Description |
|-------|-------|-------------|
| `sandbox.liveness.timeout` | Heartbeat | No HEARTBEAT received within `heartbeat_timeout_ms` |

**Runtime snapshot (`sandbox.snapshot.runtime.*`)**:

| Event | Stage | Description |
|-------|-------|-------------|
| `sandbox.snapshot.runtime.begin` | VMM | Snapshot creation started |
| `sandbox.snapshot.runtime.end` | VMM | Snapshot creation completed |
| `sandbox.snapshot.runtime.chunk_begin` | Chunk | Background chunk-ify started |
| `sandbox.snapshot.runtime.chunk_end` | Chunk | Chunk-ify completed |
| `sandbox.snapshot.runtime.chunk_failed` | Chunk | Chunk-ify failed (snapshot file intact) |
| `sandbox.snapshot.runtime.upload_begin` | Chunk | Chunk upload started |
| `sandbox.snapshot.runtime.upload_end` | Chunk | Chunk upload completed |
| `sandbox.snapshot.runtime.upload_failed` | Chunk | Chunk upload failed |

**Resume lifecycle (`sandbox.resume.*`)**:

| Event | Stage | Description |
|-------|-------|-------------|
| `sandbox.resume.requested` | Entry | Resume request received (tag: `lifecycle_phase`, `snapshot_type`) |
| `sandbox.resume.restore_begin` | VMM | Runtime snapshot restore started |
| `sandbox.resume.restore_end` | VMM | Runtime snapshot restore completed |
| `sandbox.resume.vm_resume` | VMM | VMM resume completed |
| `sandbox.resume.ready` | Guest | Guest reported READY after resume |
| `sandbox.resume.ready_timeout` | Guest | READY timeout after resume |
| `sandbox.resume.restore_failed` | VMM | Runtime snapshot restore failed |

Each event includes: `sandbox_id`, `timestamp`, `duration_ms` (where applicable), `vmm_type`, `runtime_mode`, `lifecycle_phase`.

### Compatibility

#### Backward Compatibility

- **Standard mode (K8s Adapter + VmmTaskRuntime)** preserves the exact same behavior as current Kuasar.
- The existing `Sandboxer` trait, ttrpc client, and Task API code paths are unchanged — they are merely relocated into the K8s Adapter and VmmTaskRuntime modules.
- Existing Cloud Hypervisor sandbox configurations continue to work.
- No breaking changes to the public API surface.

#### K8s and containerd Compatibility

In standard mode with K8s Adapter:
- Full shimv2 compatibility: all existing containerd/CRI-O integrations work unchanged.
- Full Task API: `Create`/`Start`/`Exec`/`Kill`/`Wait`/`Stats`/`Delete` all function as before.
- containerd events (`TaskCreate`, `TaskStart`, `TaskExit`, `TaskOOM`) are published normally.

**Appliance mode is not supported with K8s Adapter.** `K8sAdapter<V, R>` requires `R: ContainerRuntime`, which `ApplianceRuntime` does not implement — this is enforced at compile time. Appliance workloads (no exec, no attach, no multi-container) must use the Direct Adapter; the containerd Task API has no semantic mapping to the appliance model.

#### VMM Compatibility Matrix

| Operation | Cloud Hypervisor | Firecracker |
|-----------|-----------------|-------------|
| Cold boot | ✅ | ✅ |
| Snapshot restore | ✅ (`--restore`) | ✅ (`PUT /snapshot/load`) |
| Resume | ✅ (`PUT /vm.resume`) | ✅ (`PATCH /vm`) |
| Pause + Snapshot | ✅ | ✅ (`PUT /snapshot/create`) |
| Vsock | ✅ | ✅ |
| virtio-blk | ✅ | ✅ |
| Hot-plug disk | ✅ | ❌ (configure at creation) |
| VM resize (cpu/mem) | ✅ | ❌ |
| PMEM/DAX | ✅ | ❌ |
| Diff snapshot | ❌ | ✅ |

The engine gracefully handles capability differences via `VmmCapabilities`. Features unavailable on a given VMM are either skipped (with a logged warning) or trigger a documented degradation path.

#### CH v50.0 Implementation Constraints

The following CH v50.0 behaviors have been identified through benchmarking and directly affect the `CloudHypervisorVmm` implementation:

##### Constraint 1: `--kernel` flag silently disables `--restore`

When both `--kernel` and `--restore` are provided on the CH command line, CH prioritizes `--kernel` and performs a fresh boot, **silently ignoring** `--restore`:

```
// CH v50.0 internal dispatch:
if payload_present {        // ← --kernel triggers this
    VmCreate + VmBoot       // fresh boot, --restore IGNORED
} else if restore_params {
    VmRestore               // only reachable without --kernel
}
```

**Mandatory implementation rule**: `CloudHypervisorVmm::restore()` must **only** pass `--restore source_url=file://<path>` and `--api-socket <path>`. It must **not** pass `--kernel`, `--memory`, `--cpus`, `--disk`, or any other VM configuration flags. All configuration is read from the snapshot's `config.json`.

This was the root cause of invalid benchmark results that reported ~26 ms "restore" times — they were actually measuring cold boot.

##### Constraint 2: Synchronous `fill_saved_regions()` bottleneck

CH v50.0 reads the **entire** `memory-ranges` file via sequential `read()` calls during restore, regardless of guest memory utilization:

```
Restore path:
  vm_restore() → MemoryManager::new_from_snapshot()
    → fill_saved_regions(memory_ranges_file, ranges)
      → for each range: read_volatile_from(file, guest_memory)  // full 4GB sequential read
```

Measured performance for a 4 GB VM:
- **Warm page cache**: ~1.39s (2.9 GB/s, memory bandwidth limited)
- **Cold page cache**: ~1.83s (adds ~0.44s disk I/O at ~9.3 GB/s NVMe)

**Future mitigation**: An `mmap(MAP_PRIVATE)` approach for the `memory-ranges` file would enable demand-paged restore (~50 ms for slim workloads). This requires a CH upstream patch.

##### Constraint 3: Memory-zone file-backed restore is broken

CH v50.0 does not correctly restore VMs created with `--memory-zone file=<path>`:
- Snapshot records zone configuration correctly in `config.json` and `state.json`.
- The `memory-ranges` file is empty (the file **is** the memory).
- But restore creates 512 MB default memory, never opens the zone file.
- VM runs with uninitialized memory (broken state).

**Mandatory workaround**: Use standard `--memory size=NM,shared=on` for snapshot creation. Do not use `--memory-zone file=` until CH fixes the restore path.

##### Constraint 4: Snapshot serial path must be writable

The snapshot's `config.json` records the serial output file path used during snapshot creation. On restore, CH opens this path for writing. The restore host must ensure the directory exists and is writable, or rewrite the path in `config.json` before restore.

### Test Plan

[X] I/we understand the owners of the involved components may require updates to existing tests to make this code solid enough prior to committing the changes necessary to implement this enhancement.

##### Prerequisite testing updates

All existing unit tests for `vmm/sandbox` must continue to pass after the core architecture refactoring. The refactoring relocates code without changing behavior — these tests act as a behavioral regression guard.

##### Unit tests

**`vmm/engine` crate** (core lifecycle and template management; no `vmm/template-store` separate crate — template logic lives in `engine/src/template.rs` and `engine/src/discovery.rs`):
- `SandboxEngine` lifecycle: create → Running → stop → Stopped → delete; invalid state transitions return `InvalidState` error
- `SandboxEngine::start_sandbox` with `LifecyclePhase::FirstLaunch` × `{Boot, RestoreEager, Auto}` combinations; passing non-`FirstLaunch` phase returns `INVALID_ARGUMENT`
- `SandboxEngine::start_sandbox` with `StartMode::Auto`: compatible template → RestoreEager; no template → Boot; mismatched version → Boot + `sandbox.start.degraded` event
- `SandboxEngine` pause/snapshot/resume dispatch: `PauseMode::SUSPEND` and `PauseMode::SNAPSHOT` paths; `drain_timed_out=true` when drain timer expires
- `SandboxEngine::snapshot_sandbox`: pause → snapshot → resume sequence; `pause_duration_ms` is non-zero in response
- `TemplateManager::validate_compat`: one test per validated field (vmm_type, vmm_version, kernel, initrd, agent_version, compat.requires); all-match returns `Ok(())`; unknown `compat.requires` entry returns `CompatMismatch` (not silently ignored)
- `TemplateManager` disk layout: write `snapshot.meta.json` → read back fields match; missing or malformed meta returns parse error
- `TemplateManager` template discovery (inotify): atomic rename of `snapshot.meta.json` triggers registration; directory with no meta file is not registered
- `AdmissionController`: exceeding `max_concurrent_starts` returns `AdmissionRejected`; budget exhaustion returns `Timeout`

**`vmm/vm-trait` crate**:
- `Vmm` trait mock: `boot/restore/pause/snapshot/resume/stop` contract verification; `capabilities()` returns correct `VmmCapabilities` flags

**`vmm/guest-runtime` crate**:
- `GuestReadiness` trait mock: `wait_ready/kill_process/wait_process/container_stats/prepare_snapshot` contract; `prepare_snapshot` default impl returns `Ok(())`
- `ContainerRuntime: GuestReadiness` supertrait mock: `create_container/start_process/exec_process` contract; compile-time check that `K8sAdapter` rejects `ApplianceRuntime` (build test / type-level assertion)

**`vmm/runtime-appliance` crate**:
- Protocol codec: all seven message types (READY, HEARTBEAT, METRICS, FATAL, CONFIG, SHUTDOWN, PING/PONG) round-trip encode/decode; malformed JSON returns parse error
- `ApplianceRuntime::wait_ready`: READY received within timeout → Ok; timeout → `ReadyTimeout` error; FATAL message → error propagated
- `ApplianceRuntime`: CONFIG message sent before READY wait when config fields are non-empty
- `ApplianceRuntime`: HEARTBEAT timer fires after READY; `heartbeat_action=kill` kills VM; `sandbox.liveness.timeout` event emitted
- `ApplianceRuntime::prepare_snapshot`: in-memory stream tests (Cursor/Vec); trait method with bad/unreachable vsock address returns error

**`vmm/adapter-direct` crate**:
- gRPC service handlers: all `SandboxService` RPCs with mock engine — `CreateSandbox`, `StartSandbox`, `StopSandbox`, `DeleteSandbox`, `GetSandbox`, `ListSandboxes`, `WatchSandbox`
- `PauseSandbox` / `ResumeSandbox` / `SnapshotSandbox` handlers: verify `drain_timed_out`, `ResumeStrategy`, and `pause_duration_ms` fields are populated correctly
- `GetSnapshotUploadStatus`: status progresses from `in_progress` to `completed`; restarted engine returns no data for in-progress upload (documented limitation)
- `TemplateService` handlers: `CreateTemplate`, `ListTemplates`, `GetTemplate`, `DeleteTemplate` via mock engine
- Direct Adapter security: connection from unauthorized GID rejected with `PermissionDenied`; authorized GID succeeds; audit log receives one entry per RPC

##### Integration tests

All integration tests that start a VM require `/dev/kvm` and `vhost_vsock` module.

- **Appliance mode boot** (CH): Start a minimal appliance VM → wait READY → verify `sandbox.start.ready` event with `runtime_mode=appliance` tag → stop with SHUTDOWN → verify `sandbox.stop.completed` event and clean VMM exit; confirm `vmm_start_ms` is non-zero in `StartSandboxResponse`
- **Snapshot restore** (CH): Build a template with `kuasar-builder --mode synthetic` → call `StartSandbox` with `StartMode::RestoreEager` → verify `mode_used=RESTORE_EAGER` and `restore_ms` non-zero in response; verify `vmm_start_ms=0`
- **Auto degradation — no template**: Configure engine with empty template dir → `StartMode::Auto` → verify `mode_used=BOOT`; `sandbox.start.degraded` event NOT emitted
- **Auto degradation — version mismatch**: Inject template with wrong `vmm_version` → `StartMode::Auto` → verify `validate_compat` fails → `mode_used=BOOT` + `sandbox.start.degraded` event with `reason=template_compat_failed`
- **Auto degradation — unknown capability**: Inject template with `compat.requires: ["sgx"]` on non-SGX node → verify `validate_compat` returns `CompatMismatch`; engine degrades to Boot
- **Pause/Resume — SUSPEND** (same-node): Start sandbox → `PauseSandbox(SUSPEND)` → state is `Paused`; `ResumeSandbox` (no snapshot_ref) → `resume_strategy=IN_MEMORY` in response; sandbox state returns to `Running`
- **Pause/Snapshot/Resume** (same-node): Start sandbox → `PauseSandbox(SNAPSHOT)` → `snapshot_ref` present; `drain_timed_out=false` → poll `GetSnapshotUploadStatus` until `completed` → `ResumeSandbox(snapshot_ref)` → `resume_strategy=FROM_SNAPSHOT`; verify application state continuity
- **SnapshotSandbox on running VM**: Call `SnapshotSandbox` on Running sandbox → sandbox state remains `Running` before and after RPC → `pause_duration_ms` is non-zero; `snapshot_ref` usable for subsequent `ResumeSandbox`
- **Drain timeout**: Start sandbox with active simulated load → `PauseSandbox(drain_timeout_ms=1)` → verify `drain_timed_out=true` in response; VM still pauses successfully
- **Heartbeat liveness**: Start appliance VM → stop guest heartbeat → verify `sandbox.liveness.timeout` event fires within `heartbeat_timeout_ms + 500ms`; `heartbeat_action=kill` terminates VMM process
- **StartSandbox non-FirstLaunch rejected**: Call `StartSandbox` with `lifecycle_phase=USER_PAUSE_RESUME` → returns `INVALID_ARGUMENT`
- **Standard mode compatibility**: All existing K8s Adapter + VmmTaskRuntime test scenarios pass unchanged after code relocation

##### e2e tests

- **AI agent scenario** (UC1): Create 10 concurrent appliance sandboxes via Direct Adapter → verify all report READY within P95 < 1s → verify no cross-sandbox vsock access (isolation)
- **Template build pipeline**: `kuasar-builder build` (OCI → rootfs.ext4) → `kuasar-builder snapshot --mode synthetic` → verify `snapshot.meta.json` contains all required fields (vmm_type, vmm_version, kernel, initrd, agent_version) with correct values; `snapshot.meta.json` written last (atomic rename)
- **Cross-node migration** (UC4): Pause sandbox with `snapshot_chunk_enabled=true` → poll `GetSnapshotUploadStatus` until `completed` → simulate node drain → restore sandbox on second node from chunk store via `ResumeSandbox(snapshot_ref)` → verify application state preserved

### Graduation Criteria

#### Alpha

- Three-layer engine design in place. All existing tests pass with refactored code. No behavior change in standard mode.
- Appliance mode works end-to-end with Cloud Hypervisor. `ApplianceRuntime` (GuestReadiness over vsock JSON Lines) implemented. `DirectAdapter` with basic CRUD RPCs. CONFIG injection and HEARTBEAT monitoring implemented.
- Sandbox state machine (Creating/Running/Paused/Stopped/Deleted) enforced in engine.
- Unit tests for all new traits and engine: SandboxEngine lifecycle, GuestReadiness/ContainerRuntime contracts, appliance protocol round-trip.
- Configuration parsing for all four mode combinations.

#### Beta

- Snapshot restore path works end-to-end. `TemplateManager` with full version validation (`vmm_type`, `vmm_version`, kernel digest, initrd digest, `agent_version`). Template Discovery with inotify-based live refresh.
- `kuasar-builder` Phase 1 (OCI → rootfs.ext4) and Phase 2 (rootfs.ext4 → snapshot bundle) pipelines working.
- Boot mode selection is caller-driven (`StartMode` + `LifecyclePhase`). `StartMode::Auto` with graceful degradation.
- Runtime snapshot lifecycle (pause/resume/snapshot) works end-to-end. Background chunk upload pipeline with `GetSnapshotUploadStatus` polling. Both `PauseMode::SNAPSHOT` and `PauseMode::SUSPEND` work.
- Integration tests for appliance mode, snapshot restore, degradation paths, and heartbeat liveness.
- All sandbox lifecycle events emitted. Prometheus metrics exported.
- `AdmissionController` with concurrency and budget limits.
- Performance benchmarks established and tracked in CI.

#### GA

- Firecracker VMM support. `FirecrackerVmm` implementing `Vmm` trait. All four mode × VMM combinations tested end-to-end.
- Cross-node migration simulation e2e test passing (pause → chunk upload → restore on second node).
- Production-ready: full Direct Adapter security model (socket permissions, audit log, peer credential validation). Template GC with configurable TTL. `kuasar-builder` production-hardened (vm mode, block-agent integration).
- `RootfsProvider` (LocalRootfsProvider + BlockAgentRootfsProvider) fully implemented and tested.
- e2e tests passing for all primary use cases (UC1-UC4).
- Documentation complete: operator guide, appliance protocol specification, kuasar-builder usage, Direct Adapter security configuration.
- No known P0/P1 bugs. Performance benchmarks meet targets: appliance cold boot P95 < 1s, snapshot restore P95 per lifecycle phase.

### Upgrade / Downgrade Strategy

**Upgrade**: The refactored architecture is strictly additive for standard mode. Existing deployments running in standard mode (K8s Adapter + Cloud Hypervisor) upgrade transparently — the same configuration files work unchanged, and the same binary entry point routes to the same code path. Appliance mode is opt-in via `runtime_mode = "appliance"` in configuration.

**Downgrade**: Rolling back to the previous Kuasar version is safe for standard-mode deployments. Sandboxes that were created and running under the new version must be stopped before downgrade (VM lifecycle is always bounded to the sandboxer process lifetime). Appliance-mode deployments that relied on the Direct Adapter gRPC API will lose access to that API on downgrade; any in-flight sandboxes will be lost (standard VM lifecycle behavior, same as any sandboxer restart). Template snapshots (produced by kuasar-builder) are stored externally and are unaffected by a sandboxer downgrade.

### Version Skew Strategy

Kuasar is a node-level daemon — there is no distributed coordination between multiple Kuasar instances. Each node runs exactly one Kuasar process, so intra-cluster version skew between Kuasar instances on different nodes does not introduce protocol-level compatibility issues.

The relevant version skew concerns are:
- **Kuasar ↔ VMM binary**: The `Vmm` implementation is versioned against a specific VMM release. Snapshot templates record `vmm_type` and `vmm_version` in `snapshot.meta.json`; mismatched VMM versions on the restore node degrade gracefully to cold boot (see [Snapshot Template Version Validation](#snapshot-template-version-validation)).
- **Kuasar ↔ containerd (K8s Adapter)**: The shimv2 and Task API interfaces are stable. Changes to containerd's sandbox API follow the containerd versioning policy and are handled via the K8s Adapter.
- **kuasar-builder ↔ Kuasar**: The `snapshot.meta.json` format is versioned. Unknown top-level JSON fields (outside `compat.requires`) are ignored for forward compatibility. However, unknown entries in `compat.requires` are **rejected** by `validate_compat()` — a template built with a capability requirement unknown to the current engine safely degrades to cold boot rather than silently restoring on an incompatible node.

### Implementation Stories

The following epics and stories decompose the full design into independently deliverable units. Each story has a clear acceptance criterion derived from the design above.

---

#### Epic 1: Core Architecture Refactoring

Goal: establish the three-layer skeleton so that all subsequent epics build on a clean foundation. No behavior change to existing standard-mode users.

| Story | Description | Acceptance Criterion |
|-------|-------------|----------------------|
| 1.1 | Extract `Vmm` trait + `CloudHypervisorVmm` implementation into `vm-trait` crate | `CloudHypervisorVmm` compiles against trait; existing cloud-hypervisor tests pass |
| 1.2 | Define `GuestReadiness` trait and `ContainerRuntime: GuestReadiness` supertrait in `guest-runtime` crate | Both traits compile; doc-test coverage for method contracts |
| 1.3 | Implement `SandboxEngine<V: Vmm, R: GuestReadiness>` with `create/start/stop/delete` lifecycle in `engine` crate | Unit tests: create → Running → stop → Stopped state transitions pass |
| 1.4 | Implement `K8sAdapter<V, R: ContainerRuntime>` wrapping `SandboxEngine`; wire shimv2 + Task API | Existing containerd integration smoke tests pass unchanged |
| 1.5 | Implement `VmmTaskRuntime` (`impl ContainerRuntime`) extracted from existing ttrpc client code | All existing ttrpc-based tests pass via the new trait |
| 1.6 | Process startup wiring: config-driven dispatch to one of four mode combinations with zero runtime branching | Unit test: each of the four combinations constructs without panic; config parse tests |

---

#### Epic 2: Appliance Mode — Direct Path

Goal: a caller can create, start, and stop a microVM in appliance mode via the Direct Adapter gRPC API, with CONFIG injection and HEARTBEAT monitoring working end-to-end.

| Story | Description | Acceptance Criterion |
|-------|-------------|----------------------|
| 2.1 | Define appliance protocol message types (READY, HEARTBEAT, METRICS, FATAL, SHUTDOWN, CONFIG, PING/PONG) and JSON Lines codec in `runtime-appliance/src/protocol.rs` | All message types round-trip encode/decode; schema validation rejects malformed messages |
| 2.2 | Implement `ApplianceRuntime` (`impl GuestReadiness`): `wait_ready` waits for READY on vsock, `kill_process` sends SHUTDOWN | Unit tests with in-memory vsock mock: READY received → `wait_ready` returns; SHUTDOWN sent on `kill_process` |
| 2.3 | Implement CONFIG injection in `ApplianceRuntime`: send CONFIG message before waiting for READY when config fields are non-empty | Unit test: CONFIG message observed on vsock before READY wait begins |
| 2.4 | Implement HEARTBEAT monitoring in `ApplianceRuntime`: start timer after READY, take `heartbeat_action` on timeout | Unit test: `heartbeat_action=kill` triggers VM kill after `heartbeat_timeout_ms`; `sandbox.liveness.timeout` event emitted |
| 2.5 | Implement `DirectAdapter` gRPC server with `CreateSandbox`, `StartSandbox`, `StopSandbox`, `DeleteSandbox`, `GetSandbox`, `ListSandboxes`, `WatchSandbox` RPCs | Integration test: full create → start → stop → delete cycle via gRPC client |
| 2.6 | Implement `NetworkConfig` handling in `CreateSandbox`: configure tap device, IP, MAC on `CloudHypervisorVmm` before boot | Integration test: sandbox boots with assigned IP; vsock connects on correct CID |
| 2.7 | Implement Sandbox State Machine enforcement in `SandboxEngine`: invalid transitions return typed errors | Unit tests: all invalid transitions (e.g., start a Running sandbox, pause a Stopped sandbox) return `InvalidState` error |
| 2.8 | Implement `AdmissionController`: `max_concurrent_starts`, `restore_timeout_ms`, `ready_timeout_ms` limits | Unit test: exceeding `max_concurrent_starts` returns `AdmissionRejected`; budget exhaustion returns `Timeout` |

---

#### Epic 3: Template Snapshot Pipeline

Goal: a caller can build a snapshot template with `kuasar-builder`, store it on disk, have Kuasar discover it automatically, and start a sandbox from it via `StartMode::RestoreEager` or `StartMode::Auto`.

| Story | Description | Acceptance Criterion |
|-------|-------------|----------------------|
| 3.1 | Define `snapshot.meta.json` schema (`vmm_type`, `vmm_version`, `guest.kernel`, `guest.initrd`, `guest.agent_version`, `compat.requires`) and implement read/write in `engine/src/template.rs` (`TemplateManager`). No separate `template-store` crate — template persistence is part of the engine crate. | Unit tests: write meta → read back → fields match; malformed meta returns parse error |
| 3.2 | Implement `TemplateManager::validate_compat`: check all six fields, return typed `CompatMismatch` error on any mismatch | Unit tests: one test per field mismatch; all-match returns `Ok(())` |
| 3.3 | Implement Template Discovery: `TemplateManager` scans `template_dir` on startup and registers inotify watcher for live refresh. **Convention**: `kuasar-builder` writes `snapshot.meta.json` **last**, via atomic rename from a temp file (e.g., `snapshot.meta.json.tmp`). The inotify handler triggers only on `snapshot.meta.json` `IN_MOVED_TO` events, preventing partial-write races where only some files have been written. | Integration test: drop a valid `snapshot.meta.json` into `template_dir` while engine is running → template becomes queryable within 1s; partially-written template directory (no meta.json) is not registered |
| 3.4 | Implement `kuasar-builder` Phase 1 (OCI → rootfs.ext4): layer flattening, ext4 image creation, optional chunking | CLI test: `kuasar-builder build --image <ref> --output <dir>` produces `rootfs.ext4` + `image-ref.txt` |
| 3.5 | Implement `kuasar-builder` Phase 2 synthetic mode (rootfs.ext4 → snapshot bundle without real VM): placeholder files + valid `snapshot.meta.json` | CLI test: `kuasar-builder snapshot --mode synthetic` produces directory with `snapshot.meta.json` whose fields match CLI args |
| 3.6 | Implement `kuasar-builder` Phase 2 vm mode: boot real VM, wait READY, pause, snapshot via VMM API | Integration test (requires `/dev/kvm`): produced snapshot restores successfully on the same node |
| 3.7 | Implement `StartMode::RestoreEager` path in `SandboxEngine`: `validate_compat` → `vmm.restore` → `vmm.resume` → `wait_ready` | Integration test: restore sandbox from synthetic template → `mode_used=RestoreEager` in response |
| 3.8 | Implement `StartMode::Auto` with `LifecyclePhase::FirstLaunch`: attempt restore if compatible template present, degrade to Boot otherwise; emit `sandbox.start.degraded` on mismatch | Unit tests: compatible template → `RestoreEager`; missing template → `Boot`; mismatched version → `Boot` + event emitted |

---

#### Epic 4: Runtime Snapshot Lifecycle

Goal: a running sandbox can be paused (with or without snapshot), resumed (from memory or from snapshot), and cross-node migration is enabled via background chunk upload.

| Story | Description | Acceptance Criterion |
|-------|-------------|----------------------|
| 4.1 | Implement `PauseSandbox` with `PauseMode::SUSPEND`: drain requests, call `vmm.pause`, update state to `Paused` | Integration test: pause → state is `Paused`; `GetSandbox` returns `Paused` status |
| 4.2 | Implement `PauseSandbox` with `PauseMode::SNAPSHOT`: after pause, call `vmm.snapshot`, write `runtime_snapshot.meta.json` | Integration test: pause+snapshot → `snapshot_ref` present in response; meta file exists on disk |
| 4.3 | Implement `ResumeSandbox` from in-memory paused state: `vmm.resume` → `wait_ready` → state `Running` | Integration test: pause → resume → `ready_ms` recorded; `resume_strategy=IN_MEMORY` in response |
| 4.4 | Implement `ResumeSandbox` from snapshot_ref: `vmm.restore(snapshot_ref)` → `vmm.resume` → `wait_ready` | Integration test: pause+snapshot → delete in-memory state → resume from `snapshot_ref` → application state preserved |
| 4.5 | Implement background chunk upload in `pause_sandbox`: spawn tokio task, return `chunk_upload_status=in_progress` immediately | Unit test: `PauseSandboxResponse` returns before upload completes; upload task runs asynchronously |
| 4.6 | Implement `GetSnapshotUploadStatus` RPC: poll background upload task status | Integration test: poll until `status=completed`; verify `chunks_deduped` > 0 for repeated uploads of identical memory |
| 4.7 | Implement `SnapshotSandbox` RPC: internally performs `vmm.pause → vmm.snapshot → vmm.resume`; sandbox state is `Running` before and after the RPC; `pause_duration_ms` records the freeze window | Integration test: call `SnapshotSandbox` on running VM → `snapshot_ref` returned; VM stays `Running`; `pause_duration_ms` is non-zero |
| 4.8 | Emit all runtime snapshot and resume lifecycle events with correct tags | Unit test: verify event sequence for each lifecycle path (pause/resume/migrate) |

---

#### Epic 5: Firecracker VMM Support

Goal: all four mode × VMM combinations work. Firecracker's sub-25ms restore latency is exploited for FaaS workloads.

| Story | Description | Acceptance Criterion |
|-------|-------------|----------------------|
| 5.1 | Implement `FirecrackerVmm` (`impl Vmm`): `create`, `boot`, `stop`, `wait_exit` via Firecracker API socket | Unit test with FC mock: boot → running; stop → exited |
| 5.2 | Implement FC snapshot restore: `vmm.restore` → `PUT /snapshot/load`, `vmm.resume` → `PATCH /vm state=Resumed` | Integration test (requires `/dev/kvm`): restore from FC snapshot → `mode_used=RestoreEager`; latency < 100ms for 512MB VM |
| 5.3 | Implement FC diff snapshot: `snapshot()` with `snapshot_type=Diff` when `capabilities().diff_snapshot=true` | Integration test: base snapshot → start → diff snapshot → restore from diff; verify only changed pages in diff |
| 5.4 | Implement process startup for FC + Standard and FC + Appliance combinations | E2e tests: FC+Appliance starts a FaaS workload in < 200ms (cold boot); FC+Standard passes existing K8s task API tests |
| 5.5 | Validate all four mode × VMM combinations in CI with matrix tests | CI matrix: 4 rows all green; `validate_compat` correctly rejects CH snapshots on FC nodes and vice versa |

---

#### Epic 6: Production Readiness

Goal: the system is safe to operate in a production environment with proper security, observability, resource management, and documentation.

| Story | Description | Acceptance Criterion |
|-------|-------------|----------------------|
| 6.1 | Implement Direct Adapter socket security: `0660` permissions, `kuasar` group ownership, SO_PEERCRED UID/GID validation against `allow_gids` | Test: connection from unauthorized GID is rejected with `PermissionDenied`; authorized GID succeeds |
| 6.2 | Implement audit log: all Direct Adapter RPCs logged with caller PID, UID, `sandbox_id`, operation, result | Test: perform 5 RPCs → audit log contains 5 entries with correct fields |
| 6.3 | Implement Prometheus metrics export: all SLI metrics (`start_duration_ms`, `degraded_total`, `ready_timeout_total`, `pause_duration_ms`, `chunk_upload_bytes`, `dedup_ratio`, `liveness_timeout_total`) | Test: scrape `/metrics` after lifecycle operations → all counters/histograms present and non-zero |
| 6.4 | Implement `LocalRootfsProvider` and `BlockAgentRootfsProvider` (`RootfsProvider` trait) | Unit test: `LocalRootfsProvider.prepare()` returns direct path; integration test: `BlockAgentRootfsProvider` mounts FUSE and returns mount path |
| 6.5 | Implement Template GC: scan `template_dir` on startup, delete entries older than `template_ttl_hours`, skip templates in-use by running sandboxes | Unit test: expired template deleted; in-use template not deleted |
| 6.6 | E2e test UC1 (AI Agent): 10 concurrent appliance sandboxes start → all READY within P95 < 1s → strong vsock isolation verified | E2e test passes in CI |
| 6.7 | E2e test UC4 (Cross-node migration): pause → upload → restore on second node → application state preserved | E2e test passes in two-node CI environment |
| 6.8 | Documentation: operator guide, appliance protocol specification, kuasar-builder CLI reference, Direct Adapter security configuration | Docs reviewed and merged |

---

## Production Readiness Review Questionnaire

### Feature Enablement and Rollback

**How can this feature be enabled / disabled?**

Appliance mode is selected at process startup via the `kuasar.toml` configuration file:
```toml
[engine]
runtime_mode = "appliance"  # change to "standard" to disable
```

No runtime toggle is needed or supported — the mode is fixed at process startup. To switch modes, update the configuration and restart the Kuasar process.

**Does enabling the feature change any default behavior?**

No. The default configuration (`runtime_mode = "standard"`) preserves the existing behavior exactly. Appliance mode must be explicitly opted into.

**Can the feature be disabled once enabled?**

Yes. Update `runtime_mode = "standard"` in `kuasar.toml` and restart. Any sandboxes running in appliance mode will be terminated on process exit (standard VM lifecycle). On restart in standard mode, the engine operates identically to the pre-appliance Kuasar.

**Are there any tests for feature enablement/disablement?**

The process startup wiring is covered by unit tests for each of the four mode combinations. The configuration parser has unit tests for both `standard` and `appliance` values.

### Rollout, Upgrade and Rollback Planning

**How can a rollout or rollback fail? Can it impact already running workloads?**

A rollout failure (e.g., misconfigured `kuasar.toml`) will prevent the Kuasar process from starting. Already-running VMs are unaffected — they continue to run until the VMM process exits naturally. The K8s node will appear NotReady from the containerd/kubelet perspective until Kuasar is restarted with a valid configuration.

A rollback (restart with previous binary) requires stopping all active sandboxes first, as VM state is not persisted across Kuasar process restarts in the current design. For appliance-mode deployments, runtime snapshots can be used to preserve sandbox state before a planned rollback.

**What specific metrics should inform a rollback?**

- `sandbox.start.ready` event rate drops below baseline (sandboxes failing to start).
- `sandbox.start.ready_timeout` event rate increases.
- `sandbox.start.degraded` event rate is unexpectedly high (indicates systematic snapshot incompatibility).
- Kuasar process crash rate (monitor via systemd/supervisord).

**Is the rollout accompanied by any deprecations and/or removals of features, APIs, fields?**

No. The core architecture refactoring is purely internal. No public API changes in standard mode. The Direct Adapter gRPC API is new (additive).

### Monitoring Requirements

**How can an operator determine if the feature is in use?**

- Presence of `runtime_mode = "appliance"` in `kuasar.toml`.
- `sandbox.start.*` events with `runtime_mode=appliance` tag.
- Active connections to the Direct Adapter gRPC socket (`/run/kuasar/engine.sock` by default).

**How can someone using this feature know that it is working?**

- Events: `sandbox.start.ready` with `runtime_mode=appliance` confirms appliance sandboxes are starting correctly.
- gRPC response: `StartSandboxResponse.ready_ms` provides measured time-to-ready for each sandbox.
- Metrics: `kuasar_sandbox_start_duration_ms` histogram (by `mode_used`, `vmm_type`, `runtime_mode`).

**What are the reasonable SLOs for this enhancement?**

- Appliance cold boot P95 < 1,000 ms (from `StartSandbox` RPC to `sandbox.start.ready` event).
- Appliance snapshot restore P95 < 200 ms (for FC) / < 2,000 ms (for CH v50.0, pending upstream fix).
- Sandbox stop P95 < graceful_timeout + 500 ms.
- `sandbox.start.degraded` rate < 1% in steady state (indicates snapshot/template pipeline health).

**What are the SLIs an operator can use to determine the health of the service?**

- Metrics:
  - `kuasar_sandbox_start_duration_ms` (histogram, labels: `mode_used`, `vmm_type`, `runtime_mode`)
  - `kuasar_sandbox_start_degraded_total` (counter, labels: `reason`, `vmm_type`)
  - `kuasar_sandbox_start_ready_timeout_total` (counter)
  - `kuasar_sandbox_pause_duration_ms` (histogram)
  - `kuasar_snapshot_chunk_upload_bytes_total` (counter)
  - `kuasar_snapshot_chunk_dedup_ratio` (gauge)
  - `kuasar_sandbox_liveness_timeout_total` (counter, labels: `action`)

### Dependencies

**Does this feature depend on any specific services running on the node?**

- **Cloud Hypervisor binary** (`/usr/bin/cloud-hypervisor` or configured path) — required for CH VMM mode.
- **Firecracker binary** (`/usr/bin/firecracker` or configured path) — required for FC VMM mode.
- **`/dev/kvm`** — required for hardware-accelerated VM execution.
- **vsock kernel module** (`vhost_vsock`) — required for host-guest communication.
- **block-agent / artifactd** (optional) — required only when `BlockAgentRootfsProvider` is configured for lazy disk loading or chunk upload. If unavailable, the engine falls back to local disk mode.

### Scalability

**Will enabling this feature result in any new external API calls?**

In appliance mode with chunk upload enabled: HTTP calls to the external content delivery system (artifactd) during `pause_sandbox` with `snapshot_chunk_enabled=true`. These are optional and degradation-safe — if the content store is unreachable, the pause still succeeds (snapshot file stored locally, upload status set to `failed`).

**Will enabling this feature result in increasing resource usage?**

- **CPU**: VMM process (CH or FC) per sandbox, same as current standard mode. No additional host-side processes in the core path.
- **Memory**: VMM process memory footprint per sandbox (dominated by VM memory allocation), same as standard mode.
- **Disk**: Template snapshots stored in `template_dir` (configurable). Runtime snapshots stored in sandbox work directory. Both are managed by the operator.
- **Network**: No new persistent network connections. Chunk upload (optional) uses HTTP to the content store during pause operations.

**Will enabling this feature result in non-negligible increase of resource usage in any components?**

No. The three-layer architecture adds a thin abstraction over existing VMM calls. The `GuestReadiness` dispatch in appliance mode eliminates the ttrpc client and guest agent, reducing per-sandbox overhead compared to standard mode.

### Troubleshooting

**How does this feature react if a dependency (VMM binary, /dev/kvm) is unavailable?**

- Missing VMM binary: Kuasar fails to start the VM process; `sandbox.start.*` fails with an OS-level error. The sandbox remains in `Creating` state and is eventually garbage-collected by the timeout.
- `/dev/kvm` unavailable: The VMM process starts but VM creation fails with a KVM open error. Same handling as above.
- vsock module not loaded: The host-side vsock connection fails; `sandbox.start.ready_timeout` event is emitted after `ready_timeout_ms`.

**What are other known failure modes?**

- `sandbox.start.ready_timeout`: Application in the VM did not send `READY` within the timeout. Check application startup logs via the VMM serial console.
- `sandbox.start.degraded` with `reason=template_compat_failed`: The snapshot template is incompatible with the current node environment (vmm_type/vmm_version/kernel/agent_version mismatch). Rebuild the template with `kuasar-builder` targeting the current node's VMM version.
- `READY_TIMEOUT` after snapshot restore: The restored application is taking too long to become ready. Increase `ready_timeout_ms` or investigate application startup performance.
- `sandbox.liveness.timeout`: Application stopped sending heartbeats. Check VMM serial console for application errors.
- Chunk upload failure (`sandbox.snapshot.runtime.upload_failed`): The external content store is unreachable. The snapshot file is preserved on local disk; cross-node migration will not be possible until the content store is restored.

**What steps should be taken if SLOs are not being met?**

1. Check `kuasar_sandbox_start_degraded_total` — elevated `template_compat_failed` indicates stale templates; rebuild with kuasar-builder.
2. Check `kuasar_sandbox_start_ready_timeout_total` — if elevated, check VMM serial console for application startup errors.
3. Check `kuasar_sandbox_start_duration_ms` histogram — if P95 is high for `mode_used=RestoreEager`, consider switching to `Boot` mode for lightweight workloads (see [Boot Mode Selection](#boot-mode-selection-benchmark-informed-heuristics)).
4. Check host resource pressure: CPU steal, disk I/O saturation (especially for snapshot restore), memory pressure.

---

## Implementation History


---

## Drawbacks

- **Architectural complexity increase**: The three-layer design introduces more indirection than the current flat architecture. This is the intentional cost of supporting four mode combinations from a single codebase; the alternative (separate binaries) would duplicate VM lifecycle code.
- **Appliance mode is not a drop-in replacement for standard mode**: Workloads that require `exec`/`attach`/multi-container support must use standard mode. This is by design — the appliance model is explicitly for single-application VMs.
- **CH v50.0 snapshot restore performance**: For lightweight workloads, snapshot restore is slower than cold boot on current CH. Operators must understand the crossover point and configure `StartMode` appropriately. This is a temporary limitation pending CH upstream improvements.
- **kuasar-builder is a new dependency**: AI agent platforms that want snapshot-based fast start must integrate the kuasar-builder pipeline. This is additional operational complexity.

---

## Alternatives

### Alternative 1: Appliance as a configuration flag within existing architecture

Add `if appliance_mode { ... }` checks throughout the existing code without architectural refactoring.

**Rejected**: Scatters mode-specific logic across the codebase, making each mode harder to test independently and increasing the risk of regressions. The `GuestReadiness`/`ContainerRuntime` trait split provides clean separation at the type level.

### Alternative 2: Separate binary for appliance mode

Build a completely separate `kuasar-appliance` binary that doesn't share code with the standard Kuasar.

**Rejected**: Leads to code duplication in VM management, device configuration, and lifecycle logic. The three-layer architecture achieves code sharing at the Infrastructure Layer (VMM) and Engine Layer while allowing mode-specific behavior in the GuestReadiness implementation and Adapter Layer.

### Alternative 3: Dynamic per-sandbox mode selection at runtime

Allow each sandbox to independently choose standard or appliance mode during its lifecycle.

**Rejected**: Adds runtime complexity (dynamic dispatch, mixed-mode state management) for a use case that doesn't exist in practice — a Kuasar node serves one type of workload. Process-level mode selection is simpler and more predictable.

### Alternative 4: Build lazy image loading directly into Kuasar

Include chunked image loading, multi-tier caching, and FUSE block agent directly in Kuasar.

**Rejected**: Content delivery evolves independently of VM lifecycle management. Following the containerd/Nydus precedent, these concerns are better served by a separate project that Kuasar can optionally integrate with through the pluggable `RootfsProvider` interface.

### Alternative 5: Place runtime snapshot chunk upload in the external content system

Instead of Kuasar performing the chunk-ify + upload, have the external content system (artifactd/Miracle) watch for snapshot files and chunk-ify them externally.

**Rejected**: The chunk-ify trigger is inherently tied to the `pause_sandbox` lifecycle — only the engine knows *when* a VM is paused and *which* files constitute the runtime snapshot. If the content system had to detect and chunk-ify snapshots, it would need to understand VM lifecycle, violating the separation of concerns. Instead, Kuasar performs the chunking and HTTP uploads as a background task; the content system handles storage, dedup, distribution, and cross-node availability. The upload is optional and degradation-safe — if artifactd is unavailable, the snapshot file remains intact on local disk.

---

## References

1. **Kuasar Project**: https://github.com/kuasar-io/kuasar
2. **Cloud Hypervisor**: https://github.com/cloud-hypervisor/cloud-hypervisor
3. **Firecracker**: https://github.com/firecracker-microvm/firecracker
4. **On-demand Container Loading in AWS Lambda** (USENIX ATC '23): Marc Brooker et al. — content-addressed lazy loading for serverless containers.
5. **Modal.com Fast Container Loading**: https://modal.com/blog/serverless-containers — FUSE-based lazy loading for AI workloads.
6. **Nydus Image Service** (CNCF): https://nydus.dev — lazy container image loading, precedent for separating content delivery from container runtime.
7. **Kata Containers Architecture**: https://github.com/kata-containers/kata-containers/tree/main/docs/design/architecture — guest agent (`kata-agent`) + ttrpc model that appliance mode simplifies.
8. **TrENV** (SOSP '24): Repurposable sandbox pool and memory templates for fast VM startup.

---

## Infrastructure Needed (Optional)

```
kuasar/
├── vmm/
│   │
│   │   # ── Adapter Layer ──────────────────────────────────────────
│   │
│   ├── adapter-k8s/               # EXTRACTED from sandbox/src/sandbox.rs
│   │   └── src/
│   │       ├── sandboxer.rs       # impl Sandboxer for K8sAdapter<V, R: ContainerRuntime>
│   │       ├── task.rs            # impl TaskService — delegates to engine guest ops
│   │       └── events.rs          # containerd event publishing (TaskStart/Exit/OOM)
│   │
│   ├── adapter-direct/            # NEW
│   │   └── src/
│   │       ├── server.rs          # gRPC service impl + SO_PEERCRED security
│   │       ├── audit.rs           # per-RPC audit log writer
│   │       └── proto/
│   │           └── sandbox.proto  # SandboxService (CRUD + Pause/Resume/Snapshot)
│   │
│   │   # ── Engine Layer ───────────────────────────────────────────
│   │
│   ├── engine/                    # NEW (absorbs core logic of sandbox/)
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── sandbox.rs         # SandboxEngine<V: Vmm, R: GuestReadiness>
│   │       ├── state.rs           # SandboxState machine (Creating/Running/Paused/…)
│   │       ├── lifecycle.rs       # pause_sandbox / resume_sandbox / snapshot_sandbox
│   │       ├── admission.rs       # AdmissionController (concurrency + budget limits)
│   │       ├── template.rs        # TemplateManager: validate_compat, CRUD
│   │       ├── discovery.rs       # inotify-based template directory watcher
│   │       ├── boot_mode.rs       # StartMode × LifecyclePhase resolution helpers
│   │       └── config.rs          # ApplianceConfig, EngineConfig
│   │
│   │   # ── Infrastructure Layer — VMM backends ────────────────────
│   │
│   ├── vm-trait/                  # NEW: Vmm trait + VmmCapabilities
│   │   └── src/lib.rs             # create/boot/restore/resume/pause/snapshot/stop/…
│   │
│   ├── cloud-hypervisor/          # EXTRACTED from sandbox/src/cloud-hypervisor/
│   │   └── src/
│   │       ├── vmm.rs             # impl Vmm for CloudHypervisorVmm
│   │       ├── api.rs             # CH REST API client (boot/restore/pause/snapshot)
│   │       └── devices/           # virtio-blk, vsock, net, pmem, vfio device builders
│   │
│   ├── firecracker/               # NEW
│   │   └── src/
│   │       ├── vmm.rs             # impl Vmm for FirecrackerVmm
│   │       └── api.rs             # FC API socket client (boot/snapshot/restore)
│   │
│   ├── qemu/                      # EXTRACTED from sandbox/src/qemu/ (standard mode only)
│   │   └── src/
│   │       ├── vmm.rs             # impl Vmm for QemuVmm
│   │       ├── qmp_client.rs
│   │       └── devices/
│   │
│   ├── stratovirt/                # EXTRACTED from sandbox/src/stratovirt/ (standard mode only)
│   │   └── src/
│   │       ├── vmm.rs             # impl Vmm for StratoVirtVmm
│   │       ├── qmp_client.rs
│   │       └── devices/
│   │
│   │   # ── Infrastructure Layer — GuestReadiness implementations ──
│   │
│   ├── guest-runtime/             # NEW: GuestReadiness + ContainerRuntime traits
│   │   └── src/lib.rs             # GuestReadiness (all modes), ContainerRuntime (standard only)
│   │
│   ├── runtime-vmm-task/          # EXTRACTED from sandbox/src/client.rs
│   │   └── src/lib.rs             # impl ContainerRuntime for VmmTaskRuntime (ttrpc over vsock)
│   │
│   ├── runtime-appliance/         # NEW
│   │   └── src/
│   │       ├── lib.rs             # impl GuestReadiness for ApplianceRuntime (vsock JSON Lines)
│   │       └── protocol.rs        # codec: READY/HEARTBEAT/CONFIG/SHUTDOWN/PING/FATAL
│   │
│   │   # ── Infrastructure Layer — supporting host infrastructure ──
│   │
│   ├── rootfs-provider/           # NEW: pluggable disk image backend
│   │   └── src/
│   │       ├── lib.rs             # trait RootfsProvider
│   │       ├── local.rs           # LocalRootfsProvider (direct rootfs.ext4)
│   │       └── block_agent.rs     # BlockAgentRootfsProvider (FUSE + artifactd)
│   │
│   ├── snapshot-pipeline/         # NEW: host-side runtime snapshot chunk pipeline
│   │   └── src/
│   │       ├── lib.rs             # background upload orchestrator
│   │       ├── chunker.rs         # memory-ranges → 512 KiB content-addressed chunks
│   │       ├── uploader.rs        # BatchExists + PutChunk HTTP calls to artifactd
│   │       ├── blockmap.rs        # memory.blockmap.json generation
│   │       └── metadata.rs        # runtime_snapshot.meta.json read/write
│   │
│   │   # ── Unchanged ─────────────────────────────────────────────
│   │
│   ├── common/                    # RETAINED AS-IS (vmm-common shared utilities)
│   ├── task/                      # RETAINED AS-IS (vmm-task: guest-side agent binary)
│   ├── sandbox/                   # SUPERSEDED: logic migrated to engine/ + layer crates above;
│   │   │                          #   retained temporarily as migration shim, then removed
│   │   └── derive/                # RETAINED: sandbox-derive proc-macro (used by engine/)
│   └── service/                   # SUPERSEDED: replaced by cmd/kuasar-engine/ systemd unit
│
├── kuasar-builder/                # NEW: OCI image → fast-boot template pipeline (Rust binary)
│   └── src/
│       ├── main.rs                # CLI: build / snapshot --mode synthetic|vm
│       ├── oci/                   # OCI image pull + layer flattening
│       ├── rootfs/                # rootfs.ext4 creation + optional chunking
│       ├── snapshot/              # Phase 2: boot VM, wait READY, pause, snapshot
│       └── template/              # snapshot.meta.json write + TemplateStore integration
│
└── cmd/
    └── kuasar-engine/             # NEW: unified entry point (replaces per-VMM bin/ trio)
        └── main.rs                # reads config → selects (Vmm, GuestReadiness, Adapter)
```

**Dependency graph** (arrows show `depends on`):

```
cmd/kuasar-engine
  ├──► adapter-k8s   ──► engine ──► vm-trait
  │                          └──► guest-runtime
  ├──► adapter-direct ─► engine
  │
  ├──► cloud-hypervisor   (impl vm-trait::Vmm)
  ├──► firecracker         (impl vm-trait::Vmm)
  ├──► qemu                (impl vm-trait::Vmm)
  ├──► stratovirt           (impl vm-trait::Vmm)
  │
  ├──► runtime-vmm-task    (impl guest-runtime::ContainerRuntime)
  ├──► runtime-appliance   (impl guest-runtime::GuestReadiness)
  │
  ├──► rootfs-provider
  ├──► snapshot-pipeline
  └──► common
```

**Workspace `Cargo.toml` additions**:

```toml
members = [
    # existing — unchanged
    "vmm/task",
    "vmm/sandbox",
    "vmm/common",
    # Infrastructure Layer — VMM trait + backends
    "vmm/vm-trait",
    "vmm/cloud-hypervisor",
    "vmm/firecracker",
    "vmm/qemu",
    "vmm/stratovirt",
    # Infrastructure Layer — GuestReadiness trait + implementations
    "vmm/guest-runtime",
    "vmm/runtime-vmm-task",
    "vmm/runtime-appliance",
    # Infrastructure Layer — supporting host infrastructure
    "vmm/rootfs-provider",
    "vmm/snapshot-pipeline",
    # Engine Layer
    "vmm/engine",
    # Adapter Layer
    "vmm/adapter-k8s",
    "vmm/adapter-direct",
    # Build tooling + entry point
    "kuasar-builder",
    "cmd/kuasar-engine",
]
```
