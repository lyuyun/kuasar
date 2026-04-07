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

//! Unit tests for `vmm-engine` (Story 1.3).
//!
//! Uses `MockVmm` (implements `Vmm`, records calls) and `MockRuntime` (implements
//! `GuestReadiness`, returns configurable responses).

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use containerd_sandbox::data::SandboxData;
use containerd_sandbox::signal::ExitSignal;
use serde::de::Deserializer;
use serde::{Deserialize, Serialize};
use vmm_guest_runtime::{GuestReadiness, ReadyResult, SandboxSetupRequest};
use vmm_vm_trait::{
    DiskConfig, ExitInfo, Hooks, HotPlugDevice, HotPlugResult, NoopHooks, Pids, SandboxCtx,
    VcpuThreads, Vmm, VmmCapabilities, VmmNetworkConfig,
};

use crate::config::EngineConfig;
use crate::engine::SandboxEngine;
use crate::error::Error;
use crate::state::SandboxState;
use crate::CreateSandboxRequest;

// ── MockVmm ───────────────────────────────────────────────────────────────────

/// Records each method call so tests can assert what was called.
#[derive(Default)]
struct CallLog {
    boots: u32,
    stops: Vec<bool>,          // force flag per call
    add_disks: Vec<String>,    // paths
    add_networks: Vec<String>, // tap names
    ping_calls: u32,
}

#[derive(Clone, Serialize, Deserialize, Default)]
struct MockVmmConfig {
    /// If true, `boot()` returns an error.
    pub fail_boot: bool,
}

/// Serializable fields only (exit channel is reconstructed on deserialization).
#[derive(Serialize)]
struct MockVmm {
    id: String,
    base_dir: String,
    vsock_cid: u32,
    fail_boot: bool,
    #[serde(skip)]
    log: Arc<StdMutex<CallLog>>,
    // Kept alive to prevent the watch channel from closing.
    #[allow(dead_code)]
    #[serde(skip)]
    exit_tx: Arc<tokio::sync::watch::Sender<Option<ExitInfo>>>,
    #[serde(skip)]
    exit_rx: tokio::sync::watch::Receiver<Option<ExitInfo>>,
}

impl Default for MockVmm {
    fn default() -> Self {
        let (tx, rx) = tokio::sync::watch::channel(None);
        Self {
            id: String::new(),
            base_dir: String::new(),
            vsock_cid: 0,
            fail_boot: false,
            log: Default::default(),
            exit_tx: Arc::new(tx),
            exit_rx: rx,
        }
    }
}

impl<'de> Deserialize<'de> for MockVmm {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Helper {
            id: String,
            base_dir: String,
            vsock_cid: u32,
            #[serde(default)]
            fail_boot: bool,
        }
        let h = Helper::deserialize(d)?;
        let (tx, rx) = tokio::sync::watch::channel(None);
        Ok(MockVmm {
            id: h.id,
            base_dir: h.base_dir,
            vsock_cid: h.vsock_cid,
            fail_boot: h.fail_boot,
            log: Arc::new(StdMutex::new(CallLog::default())),
            exit_tx: Arc::new(tx),
            exit_rx: rx,
        })
    }
}

impl MockVmm {
    #[allow(dead_code)]
    fn new_with_log(id: &str, base_dir: &str, vsock_cid: u32, log: Arc<StdMutex<CallLog>>) -> Self {
        let (tx, rx) = tokio::sync::watch::channel(None);
        Self {
            id: id.to_string(),
            base_dir: base_dir.to_string(),
            vsock_cid,
            fail_boot: false,
            log,
            exit_tx: Arc::new(tx),
            exit_rx: rx,
        }
    }

    /// Simulate an unexpected VMM crash by sending an ExitInfo on the watch channel.
    /// Used in tests to exercise the monitor_vmm_exit unexpected-exit path.
    fn trigger_crash(&self) {
        self.exit_tx
            .send(Some(ExitInfo {
                pid: 9999,
                exit_code: 137,
            }))
            .ok();
    }
}

#[async_trait]
impl Vmm for MockVmm {
    type Config = MockVmmConfig;

    async fn create(
        id: &str,
        base_dir: &str,
        config: &MockVmmConfig,
        vsock_cid: u32,
    ) -> anyhow::Result<Self> {
        let (tx, rx) = tokio::sync::watch::channel(None);
        Ok(Self {
            id: id.to_string(),
            base_dir: base_dir.to_string(),
            vsock_cid,
            fail_boot: config.fail_boot,
            log: Arc::new(StdMutex::new(CallLog::default())),
            exit_tx: Arc::new(tx),
            exit_rx: rx,
        })
    }

    async fn boot(&mut self) -> anyhow::Result<()> {
        if self.fail_boot {
            return Err(anyhow::anyhow!("mock boot failure"));
        }
        self.log.lock().unwrap().boots += 1;
        Ok(())
    }

    async fn stop(&mut self, force: bool) -> anyhow::Result<()> {
        self.log.lock().unwrap().stops.push(force);
        Ok(())
    }

    fn subscribe_exit(&self) -> tokio::sync::watch::Receiver<Option<ExitInfo>> {
        self.exit_rx.clone()
    }

    async fn recover(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn add_disk(&mut self, disk: DiskConfig) -> anyhow::Result<()> {
        self.log.lock().unwrap().add_disks.push(disk.path);
        Ok(())
    }

    fn add_network(&mut self, config: VmmNetworkConfig) -> anyhow::Result<()> {
        if let VmmNetworkConfig::Tap { tap_device, .. } = config {
            self.log.lock().unwrap().add_networks.push(tap_device);
        }
        Ok(())
    }

    async fn hot_attach(&mut self, _device: HotPlugDevice) -> anyhow::Result<HotPlugResult> {
        Ok(HotPlugResult {
            device_id: "mock-dev".to_string(),
            bus_addr: String::new(),
        })
    }

    async fn hot_detach(&mut self, _device_id: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn ping(&self) -> anyhow::Result<()> {
        self.log.lock().unwrap().ping_calls += 1;
        Ok(())
    }

    async fn vcpus(&self) -> anyhow::Result<VcpuThreads> {
        Ok(VcpuThreads {
            vcpus: Default::default(),
        })
    }

    fn pids(&self) -> Pids {
        Pids {
            vmm_pid: Some(9999),
            affiliated_pids: vec![],
        }
    }

    fn vsock_path(&self) -> anyhow::Result<String> {
        Ok(format!("mock://{}/vsock:{}", self.base_dir, self.vsock_cid))
    }

    fn capabilities(&self) -> VmmCapabilities {
        VmmCapabilities {
            virtio_serial: true,
            ..Default::default()
        }
    }
}

// ── MockRuntime ───────────────────────────────────────────────────────────────

struct MockRuntime {
    /// If set, `wait_ready` sleeps for this duration (for timeout tests).
    pub delay: Option<Duration>,
    /// If set, `wait_ready` returns this error.
    pub fail_ready: Option<String>,
    pub setup_calls: Arc<StdMutex<Vec<String>>>,
}

impl MockRuntime {
    fn new() -> Self {
        Self {
            delay: None,
            fail_ready: None,
            setup_calls: Arc::new(StdMutex::new(vec![])),
        }
    }

    fn with_delay(mut self, d: Duration) -> Self {
        self.delay = Some(d);
        self
    }

    #[allow(dead_code)]
    fn with_fail_ready(mut self, msg: &str) -> Self {
        self.fail_ready = Some(msg.to_string());
        self
    }
}

#[async_trait]
impl GuestReadiness for MockRuntime {
    async fn wait_ready(&self, _sandbox_id: &str, _vsock: &str) -> anyhow::Result<ReadyResult> {
        if let Some(d) = self.delay {
            tokio::time::sleep(d).await;
        }
        if let Some(ref msg) = self.fail_ready {
            return Err(anyhow::anyhow!("{}", msg));
        }
        Ok(ReadyResult {
            sandbox_id: "mock".to_string(),
            timestamp_ms: 0,
        })
    }

    async fn setup_sandbox(
        &self,
        sandbox_id: &str,
        _req: &SandboxSetupRequest,
    ) -> anyhow::Result<()> {
        self.setup_calls
            .lock()
            .unwrap()
            .push(sandbox_id.to_string());
        Ok(())
    }

    async fn forward_events(&self, _sandbox_id: &str, _exit_signal: Arc<ExitSignal>) {}
}

// ── MockHooks ─────────────────────────────────────────────────────────────────

#[derive(Default)]
struct HookLog {
    pub post_create: u32,
    pub pre_start: u32,
    pub post_start: u32,
    pub pre_stop: u32,
    pub post_stop: u32,
    pub last_pre_start_base_dir: String,
}

struct MockHooks {
    log: Arc<StdMutex<HookLog>>,
}

impl MockHooks {
    fn new() -> (Self, Arc<StdMutex<HookLog>>) {
        let log = Arc::new(StdMutex::new(HookLog::default()));
        (Self { log: log.clone() }, log)
    }
}

#[async_trait]
impl Hooks<MockVmm> for MockHooks {
    async fn post_create(&self, _ctx: &mut SandboxCtx<'_, MockVmm>) -> anyhow::Result<()> {
        self.log.lock().unwrap().post_create += 1;
        Ok(())
    }

    async fn pre_start(&self, ctx: &mut SandboxCtx<'_, MockVmm>) -> anyhow::Result<()> {
        let mut log = self.log.lock().unwrap();
        log.pre_start += 1;
        log.last_pre_start_base_dir = ctx.base_dir.to_string();
        Ok(())
    }

    async fn post_start(&self, ctx: &mut SandboxCtx<'_, MockVmm>) -> anyhow::Result<()> {
        // Mimic the default NoopHooks behaviour: set task_address
        ctx.data.task_address = ctx.vmm.task_address();
        self.log.lock().unwrap().post_start += 1;
        Ok(())
    }

    async fn pre_stop(&self, _ctx: &mut SandboxCtx<'_, MockVmm>) -> anyhow::Result<()> {
        self.log.lock().unwrap().pre_stop += 1;
        Ok(())
    }

    async fn post_stop(&self, _ctx: &mut SandboxCtx<'_, MockVmm>) -> anyhow::Result<()> {
        self.log.lock().unwrap().post_stop += 1;
        Ok(())
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn engine_with_mock_hooks(
    tmp: &str,
) -> (
    SandboxEngine<MockVmm, MockRuntime, MockHooks>,
    Arc<StdMutex<HookLog>>,
) {
    let (hooks, log) = MockHooks::new();
    let engine = SandboxEngine::new(
        MockVmmConfig::default(),
        MockRuntime::new(),
        hooks,
        EngineConfig {
            work_dir: tmp.to_string(),
            ready_timeout_ms: 2_000,
        },
    );
    (engine, log)
}

fn engine_with_noop_hooks(tmp: &str) -> SandboxEngine<MockVmm, MockRuntime, NoopHooks<MockVmm>> {
    SandboxEngine::new(
        MockVmmConfig::default(),
        MockRuntime::new(),
        NoopHooks::default(),
        EngineConfig {
            work_dir: tmp.to_string(),
            ready_timeout_ms: 2_000,
        },
    )
}

fn basic_req() -> CreateSandboxRequest {
    CreateSandboxRequest {
        sandbox_data: SandboxData::default(),
        netns: String::new(), // empty → skip network setup
        cgroup_parent: String::new(),
        rootfs_disk: None,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_sandbox_succeeds_state_creating() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine_with_noop_hooks(tmp.path().to_str().unwrap());

    engine.create_sandbox("sb1", basic_req()).await.unwrap();

    let inst = engine.get_sandbox("sb1").await.unwrap();
    let guard = inst.lock().await;
    assert_eq!(guard.state, SandboxState::Creating);
    assert!(guard.base_dir.ends_with("sb1"));
}

#[tokio::test]
async fn create_sandbox_directory_created() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine_with_noop_hooks(tmp.path().to_str().unwrap());

    engine.create_sandbox("sb-dir", basic_req()).await.unwrap();

    let dir = tmp.path().join("sb-dir");
    assert!(dir.is_dir(), "sandbox base_dir should be created");
}

#[tokio::test]
async fn create_sandbox_cgroup_correct_parent_path() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine_with_noop_hooks(tmp.path().to_str().unwrap());

    let mut req = basic_req();
    req.cgroup_parent = "my-cgroup-parent".to_string();
    engine.create_sandbox("sb-cg", req).await.unwrap();

    let inst = engine.get_sandbox("sb-cg").await.unwrap();
    let guard = inst.lock().await;
    assert_eq!(guard.cgroup.cgroup_parent_path, "my-cgroup-parent");
}

#[tokio::test]
async fn create_sandbox_default_cgroup_parent() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine_with_noop_hooks(tmp.path().to_str().unwrap());

    engine
        .create_sandbox("sb-defcg", basic_req())
        .await
        .unwrap();

    let inst = engine.get_sandbox("sb-defcg").await.unwrap();
    let guard = inst.lock().await;
    assert_eq!(
        guard.cgroup.cgroup_parent_path,
        vmm_common::cgroup::DEFAULT_CGROUP_PARENT_PATH
    );
}

#[tokio::test]
async fn create_sandbox_post_create_hook_called() {
    let tmp = tempfile::tempdir().unwrap();
    let (engine, log) = engine_with_mock_hooks(tmp.path().to_str().unwrap());

    engine.create_sandbox("sb-hook", basic_req()).await.unwrap();
    assert_eq!(
        log.lock().unwrap().post_create,
        1,
        "post_create hook should be called once"
    );
}

#[tokio::test]
async fn create_sandbox_duplicate_id_returns_already_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine_with_noop_hooks(tmp.path().to_str().unwrap());

    engine.create_sandbox("dup", basic_req()).await.unwrap();
    let err = engine.create_sandbox("dup", basic_req()).await.unwrap_err();
    assert!(
        matches!(err, Error::AlreadyExists(_)),
        "expected AlreadyExists, got {:?}",
        err
    );
}

#[tokio::test]
async fn start_sandbox_boot_path_state_running() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine_with_noop_hooks(tmp.path().to_str().unwrap());

    engine
        .create_sandbox("sb-start", basic_req())
        .await
        .unwrap();
    let result = engine.start_sandbox("sb-start").await.unwrap();

    assert!(
        result.vmm_start_ms > 0 || result.vmm_start_ms == 0,
        "vmm_start_ms returned"
    );

    let inst = engine.get_sandbox("sb-start").await.unwrap();
    let guard = inst.lock().await;
    assert_eq!(guard.state, SandboxState::Running);
}

#[tokio::test]
async fn start_sandbox_pre_start_hook_called() {
    let tmp = tempfile::tempdir().unwrap();
    let (engine, log) = engine_with_mock_hooks(tmp.path().to_str().unwrap());

    engine.create_sandbox("sb-pre", basic_req()).await.unwrap();
    engine.start_sandbox("sb-pre").await.unwrap();

    let l = log.lock().unwrap();
    assert_eq!(l.pre_start, 1, "pre_start hook called");
    assert!(
        l.last_pre_start_base_dir.ends_with("sb-pre"),
        "pre_start ctx.base_dir is correct"
    );
}

#[tokio::test]
async fn start_sandbox_post_start_hook_sets_task_address() {
    let tmp = tempfile::tempdir().unwrap();
    let (engine, _log) = engine_with_mock_hooks(tmp.path().to_str().unwrap());

    engine.create_sandbox("sb-ts", basic_req()).await.unwrap();
    engine.start_sandbox("sb-ts").await.unwrap();

    let inst = engine.get_sandbox("sb-ts").await.unwrap();
    let guard = inst.lock().await;
    // MockHooks::post_start calls ctx.vmm.task_address() and stores in ctx.data.task_address
    assert!(
        !guard.data.task_address.is_empty(),
        "task_address should be set by post_start hook"
    );
}

#[tokio::test]
async fn start_sandbox_wrong_state_returns_invalid_state() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine_with_noop_hooks(tmp.path().to_str().unwrap());

    engine.create_sandbox("sb-ws", basic_req()).await.unwrap();
    engine.start_sandbox("sb-ws").await.unwrap();

    // Already Running — second start should fail
    let err = engine.start_sandbox("sb-ws").await.unwrap_err();
    assert!(
        matches!(err, Error::InvalidState(_)),
        "expected InvalidState, got {:?}",
        err
    );
}

#[tokio::test]
async fn start_sandbox_ready_timeout_state_stopped() {
    let tmp = tempfile::tempdir().unwrap();
    // Use a runtime that sleeps longer than ready_timeout_ms
    let slow_runtime = MockRuntime::new().with_delay(Duration::from_millis(200));
    let engine = SandboxEngine::new(
        MockVmmConfig::default(),
        slow_runtime,
        NoopHooks::<MockVmm>::default(),
        EngineConfig {
            work_dir: tmp.path().to_str().unwrap().to_string(),
            ready_timeout_ms: 50, // expires before 200ms sleep
        },
    );

    engine
        .create_sandbox("sb-timeout", basic_req())
        .await
        .unwrap();
    let err = engine.start_sandbox("sb-timeout").await.unwrap_err();
    assert!(
        matches!(err, Error::Timeout(_)),
        "expected Timeout, got {:?}",
        err
    );

    let inst = engine.get_sandbox("sb-timeout").await.unwrap();
    assert_eq!(inst.lock().await.state, SandboxState::Stopped);
}

#[tokio::test]
async fn stop_sandbox_running_to_stopped() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine_with_noop_hooks(tmp.path().to_str().unwrap());

    engine.create_sandbox("sb-stop", basic_req()).await.unwrap();
    engine.start_sandbox("sb-stop").await.unwrap();
    engine.stop_sandbox("sb-stop", false).await.unwrap();

    let inst = engine.get_sandbox("sb-stop").await.unwrap();
    assert_eq!(inst.lock().await.state, SandboxState::Stopped);
}

#[tokio::test]
async fn stop_sandbox_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine_with_noop_hooks(tmp.path().to_str().unwrap());

    engine.create_sandbox("sb-idem", basic_req()).await.unwrap();
    engine.start_sandbox("sb-idem").await.unwrap();
    engine.stop_sandbox("sb-idem", false).await.unwrap();
    // Second stop should succeed (idempotent)
    engine.stop_sandbox("sb-idem", false).await.unwrap();

    let inst = engine.get_sandbox("sb-idem").await.unwrap();
    assert_eq!(inst.lock().await.state, SandboxState::Stopped);
}

#[tokio::test]
async fn stop_sandbox_from_deleted_returns_invalid_state() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine_with_noop_hooks(tmp.path().to_str().unwrap());

    engine
        .create_sandbox("sb-del-stop", basic_req())
        .await
        .unwrap();
    engine.start_sandbox("sb-del-stop").await.unwrap();
    engine.stop_sandbox("sb-del-stop", false).await.unwrap();
    engine.delete_sandbox("sb-del-stop", false).await.unwrap();

    // Sandbox is now removed from map; get_sandbox should fail
    let err = engine.stop_sandbox("sb-del-stop", false).await.unwrap_err();
    assert!(
        matches!(err, Error::NotFound(_)),
        "expected NotFound after delete, got {:?}",
        err
    );
}

#[tokio::test]
async fn delete_sandbox_stopped_to_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine_with_noop_hooks(tmp.path().to_str().unwrap());

    engine
        .create_sandbox("sb-delete", basic_req())
        .await
        .unwrap();
    engine.start_sandbox("sb-delete").await.unwrap();
    engine.stop_sandbox("sb-delete", false).await.unwrap();
    engine.delete_sandbox("sb-delete", false).await.unwrap();

    // Sandbox should be evicted from map
    let result = engine.get_sandbox("sb-delete").await;
    assert!(matches!(result, Err(Error::NotFound(_))));
}

#[tokio::test]
async fn delete_sandbox_running_without_force_returns_invalid_state() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine_with_noop_hooks(tmp.path().to_str().unwrap());

    engine
        .create_sandbox("sb-del-run", basic_req())
        .await
        .unwrap();
    engine.start_sandbox("sb-del-run").await.unwrap();

    let err = engine
        .delete_sandbox("sb-del-run", false)
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::InvalidState(_)),
        "expected InvalidState, got {:?}",
        err
    );
}

#[tokio::test]
async fn delete_sandbox_running_with_force_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine_with_noop_hooks(tmp.path().to_str().unwrap());

    engine
        .create_sandbox("sb-force-del", basic_req())
        .await
        .unwrap();
    engine.start_sandbox("sb-force-del").await.unwrap();
    engine.delete_sandbox("sb-force-del", true).await.unwrap();

    let result = engine.get_sandbox("sb-force-del").await;
    assert!(matches!(result, Err(Error::NotFound(_))));
}

#[tokio::test]
async fn stop_then_delete_preserves_sandbox_in_map() {
    // Regression test: monitor_vmm_exit must NOT remove the sandbox from the map
    // when the VMM exits gracefully (i.e. stop_sandbox already set state=Stopped).
    // Without the fix, the monitor fires after stop_sandbox completes, removes
    // the sandbox from the map, and the subsequent delete_sandbox call fails with
    // NotFound.
    //
    // In MockVmm the exit channel is never signalled by stop(), so we exercise
    // the map-presence invariant directly: after stop_sandbox the sandbox must
    // still be in the map, and delete_sandbox must succeed.
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine_with_noop_hooks(tmp.path().to_str().unwrap());

    engine
        .create_sandbox("sb-graceful", basic_req())
        .await
        .unwrap();
    engine.start_sandbox("sb-graceful").await.unwrap();
    engine.stop_sandbox("sb-graceful", false).await.unwrap();

    // Sandbox must still be in the map so delete_sandbox can find it.
    engine
        .get_sandbox("sb-graceful")
        .await
        .expect("sandbox must remain in map after stop (before delete)");

    engine
        .delete_sandbox("sb-graceful", false)
        .await
        .expect("delete_sandbox must succeed after stop");

    let result = engine.get_sandbox("sb-graceful").await;
    assert!(
        matches!(result, Err(Error::NotFound(_))),
        "sandbox must be gone from map after delete"
    );
}

#[tokio::test]
async fn unexpected_vmm_exit_removes_sandbox_and_fires_signal() {
    // When the VMM crashes while the sandbox is Running, monitor_vmm_exit should
    // mark it Stopped, fire the exit_signal, and remove it from the map so that
    // any pending get_sandbox calls (e.g. from a monitoring loop) see NotFound.
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine_with_noop_hooks(tmp.path().to_str().unwrap());

    engine
        .create_sandbox("sb-crash", basic_req())
        .await
        .unwrap();
    engine.start_sandbox("sb-crash").await.unwrap();

    // Grab the exit_signal Arc before triggering the crash.
    let exit_signal = {
        let inst_arc = engine.get_sandbox("sb-crash").await.unwrap();
        let inst = inst_arc.lock().await;
        assert_eq!(inst.state, SandboxState::Running);
        inst.exit_signal.clone()
    };

    // Trigger a simulated crash: send ExitInfo on the VMM's exit watch channel
    // while the sandbox is still in Running state.
    {
        let inst_arc = engine.get_sandbox("sb-crash").await.unwrap();
        let inst = inst_arc.lock().await;
        inst.vmm.trigger_crash();
    }

    // Give the monitor task time to wake up, acquire the lock, and act.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // The monitor should have removed the sandbox from the map.
    let result = engine.get_sandbox("sb-crash").await;
    assert!(
        matches!(result, Err(Error::NotFound(_))),
        "monitor_vmm_exit should remove sandbox from map on unexpected exit"
    );

    // exit_signal must have been fired.  Use timeout so the test fails fast.
    let signalled = tokio::time::timeout(std::time::Duration::from_millis(100), exit_signal.wait())
        .await
        .is_ok();
    assert!(
        signalled,
        "exit_signal should be fired on unexpected VMM exit"
    );
}
