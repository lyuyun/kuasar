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

//! Unit tests for `vmm-adapter-k8s` (Story 1.4).

use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use containerd_sandbox::data::{ContainerData, SandboxData};
use containerd_sandbox::signal::ExitSignal;
use containerd_sandbox::{Container, ContainerOption, Sandbox, SandboxOption, Sandboxer};
use serde::de::Deserializer;
use serde::{Deserialize, Serialize};
use vmm_engine::config::EngineConfig;
use vmm_engine::SandboxEngine;
use vmm_guest_runtime::{
    ContainerInfo, ContainerRuntime, ContainerSpec, ContainerStats, ExecSpec, ExitStatus,
    GuestReadiness, ProcessInfo, ReadyResult, SandboxSetupRequest,
};
use vmm_vm_trait::{
    DiskConfig, ExitInfo, HotPlugDevice, HotPlugResult, NoopHooks, Pids, VcpuThreads, Vmm,
    VmmCapabilities, VmmNetworkConfig,
};

use crate::task::{CreateTaskRequest, ExecProcessRequest, TaskService};
use crate::{K8sAdapter, K8sAdapterConfig};

// ── MockVmm (same as in vmm-engine tests) ────────────────────────────────────

#[derive(Default)]
struct CallLog {
    boots: u32,
    hot_attach_calls: Vec<String>,
    ping_calls: u32,
}

#[derive(Clone, Serialize, Deserialize, Default)]
struct MockVmmConfig;

#[derive(Serialize)]
struct MockVmm {
    id: String,
    base_dir: String,
    vsock_cid: u32,
    #[serde(skip)]
    log: Arc<StdMutex<CallLog>>,
    #[allow(dead_code)]
    #[serde(skip)]
    exit_tx: Arc<tokio::sync::watch::Sender<Option<ExitInfo>>>,
    #[serde(skip)]
    exit_rx: tokio::sync::watch::Receiver<Option<ExitInfo>>,
}

impl<'de> Deserialize<'de> for MockVmm {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Helper {
            id: String,
            base_dir: String,
            vsock_cid: u32,
        }
        let h = Helper::deserialize(d)?;
        let (tx, rx) = tokio::sync::watch::channel(None);
        Ok(MockVmm {
            id: h.id,
            base_dir: h.base_dir,
            vsock_cid: h.vsock_cid,
            log: Default::default(),
            exit_tx: Arc::new(tx),
            exit_rx: rx,
        })
    }
}

#[async_trait]
impl Vmm for MockVmm {
    type Config = MockVmmConfig;

    async fn create(
        id: &str,
        base_dir: &str,
        _config: &MockVmmConfig,
        vsock_cid: u32,
    ) -> anyhow::Result<Self> {
        let (tx, rx) = tokio::sync::watch::channel(None);
        Ok(Self {
            id: id.to_string(),
            base_dir: base_dir.to_string(),
            vsock_cid,
            log: Default::default(),
            exit_tx: Arc::new(tx),
            exit_rx: rx,
        })
    }

    async fn boot(&mut self) -> anyhow::Result<()> {
        self.log.lock().unwrap().boots += 1;
        Ok(())
    }

    async fn stop(&mut self, _force: bool) -> anyhow::Result<()> {
        Ok(())
    }

    fn subscribe_exit(&self) -> tokio::sync::watch::Receiver<Option<ExitInfo>> {
        self.exit_rx.clone()
    }

    async fn recover(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn add_disk(&mut self, _disk: DiskConfig) -> anyhow::Result<()> {
        Ok(())
    }

    fn add_network(&mut self, _config: VmmNetworkConfig) -> anyhow::Result<()> {
        Ok(())
    }

    async fn hot_attach(&mut self, device: HotPlugDevice) -> anyhow::Result<HotPlugResult> {
        let kind = match &device {
            HotPlugDevice::CharDevice { id, .. } => format!("char:{}", id),
            HotPlugDevice::VirtioBlock { id, .. } => format!("blk:{}", id),
            HotPlugDevice::VsockMuxIO { id, .. } => format!("vsock:{}", id),
            HotPlugDevice::VirtioFs { id, .. } => format!("fs:{}", id),
        };
        self.log.lock().unwrap().hot_attach_calls.push(kind);
        Ok(HotPlugResult {
            device_id: "mock-device".to_string(),
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
            vmm_pid: Some(1),
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

// ── MockRuntime (implements both GuestReadiness + ContainerRuntime) ───────────

#[derive(Default)]
struct RuntimeLog {
    create_container_calls: Vec<String>,
    exec_process_calls: Vec<String>,
    kill_process_calls: Vec<(String, u32)>,
    wait_process_calls: Vec<String>,
    stats_calls: Vec<String>,
}

struct MockCombinedRuntime {
    log: Arc<StdMutex<RuntimeLog>>,
}

impl MockCombinedRuntime {
    fn new() -> (Self, Arc<StdMutex<RuntimeLog>>) {
        let log = Arc::new(StdMutex::new(RuntimeLog::default()));
        (Self { log: log.clone() }, log)
    }
}

#[async_trait]
impl GuestReadiness for MockCombinedRuntime {
    async fn wait_ready(&self, _sandbox_id: &str, _vsock: &str) -> anyhow::Result<ReadyResult> {
        Ok(ReadyResult {
            sandbox_id: "mock".to_string(),
            timestamp_ms: 0,
        })
    }

    async fn setup_sandbox(
        &self,
        _sandbox_id: &str,
        _req: &SandboxSetupRequest,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn forward_events(&self, _sandbox_id: &str, _exit_signal: Arc<ExitSignal>) {}
}

#[async_trait]
impl ContainerRuntime for MockCombinedRuntime {
    async fn create_container(
        &self,
        sandbox_id: &str,
        spec: ContainerSpec,
    ) -> anyhow::Result<ContainerInfo> {
        self.log
            .lock()
            .unwrap()
            .create_container_calls
            .push(format!("{}:{}", sandbox_id, spec.id));
        Ok(ContainerInfo { pid: 42 })
    }

    async fn start_process(
        &self,
        _sandbox_id: &str,
        _container_id: &str,
    ) -> anyhow::Result<ProcessInfo> {
        Ok(ProcessInfo { pid: 43 })
    }

    async fn exec_process(
        &self,
        sandbox_id: &str,
        container_id: &str,
        spec: ExecSpec,
    ) -> anyhow::Result<ProcessInfo> {
        self.log
            .lock()
            .unwrap()
            .exec_process_calls
            .push(format!("{}:{}:{}", sandbox_id, container_id, spec.exec_id));
        Ok(ProcessInfo { pid: 0 })
    }

    async fn kill_process(
        &self,
        sandbox_id: &str,
        _container_id: &str,
        pid: u32,
        _signal: u32,
    ) -> anyhow::Result<()> {
        self.log
            .lock()
            .unwrap()
            .kill_process_calls
            .push((sandbox_id.to_string(), pid));
        Ok(())
    }

    async fn wait_process(
        &self,
        sandbox_id: &str,
        container_id: &str,
        _pid: u32,
    ) -> anyhow::Result<ExitStatus> {
        self.log
            .lock()
            .unwrap()
            .wait_process_calls
            .push(format!("{}:{}", sandbox_id, container_id));
        Ok(ExitStatus {
            exit_code: 0,
            exited_at_ms: 0,
        })
    }

    async fn container_stats(
        &self,
        sandbox_id: &str,
        container_id: &str,
    ) -> anyhow::Result<ContainerStats> {
        self.log
            .lock()
            .unwrap()
            .stats_calls
            .push(format!("{}:{}", sandbox_id, container_id));
        Ok(ContainerStats {
            cpu_usage_ns: 100,
            memory_rss_bytes: 200,
            pids_current: 3,
        })
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn make_adapter(
    tmp: &str,
    runtime: MockCombinedRuntime,
) -> K8sAdapter<MockVmm, MockCombinedRuntime, NoopHooks<MockVmm>> {
    let engine = SandboxEngine::new(
        MockVmmConfig,
        runtime,
        NoopHooks::default(),
        EngineConfig {
            work_dir: tmp.to_string(),
            ready_timeout_ms: 2_000,
        },
    );
    K8sAdapter::new(engine, K8sAdapterConfig::default())
}

/// Helper: call `Sandboxer::create` without ambiguity with `TaskService::create`.
async fn create_sandbox(
    adapter: &K8sAdapter<MockVmm, MockCombinedRuntime, NoopHooks<MockVmm>>,
    id: &str,
    opt: SandboxOption,
) {
    Sandboxer::create(adapter, id, opt).await.unwrap();
}

/// Helper: call `Sandboxer::start` without ambiguity with `TaskService::start`.
async fn start_sandbox(
    adapter: &K8sAdapter<MockVmm, MockCombinedRuntime, NoopHooks<MockVmm>>,
    id: &str,
) {
    Sandboxer::start(adapter, id).await.unwrap();
}

fn sandbox_option(data: SandboxData) -> SandboxOption {
    SandboxOption {
        base_dir: String::new(),
        sandbox: data,
    }
}

fn basic_data() -> SandboxData {
    SandboxData::default()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn sandboxer_create_delegates_to_engine() {
    let tmp = tempfile::tempdir().unwrap();
    let (runtime, _log) = MockCombinedRuntime::new();
    let adapter = make_adapter(tmp.path().to_str().unwrap(), runtime);

    create_sandbox(&adapter, "sb1", sandbox_option(basic_data())).await;

    // Sandbox should be in engine
    let inst = adapter.engine.get_sandbox("sb1").await.unwrap();
    assert!(!inst.lock().await.id.is_empty());
}

#[tokio::test]
async fn sandboxer_start_calls_engine_start() {
    let tmp = tempfile::tempdir().unwrap();
    let (runtime, _log) = MockCombinedRuntime::new();
    let adapter = make_adapter(tmp.path().to_str().unwrap(), runtime);

    create_sandbox(&adapter, "sb-start", sandbox_option(basic_data())).await;
    start_sandbox(&adapter, "sb-start").await;

    let inst = adapter.engine.get_sandbox("sb-start").await.unwrap();
    use vmm_engine::state::SandboxState;
    assert_eq!(inst.lock().await.state, SandboxState::Running);
}

#[tokio::test]
async fn sandboxer_update_persists_data() {
    let tmp = tempfile::tempdir().unwrap();
    let (runtime, _log) = MockCombinedRuntime::new();
    let adapter = make_adapter(tmp.path().to_str().unwrap(), runtime);

    create_sandbox(&adapter, "sb-update", sandbox_option(basic_data())).await;

    let mut new_data = basic_data();
    new_data.task_address = "new-address".to_string();
    adapter.update("sb-update", new_data.clone()).await.unwrap();

    let inst = adapter.engine.get_sandbox("sb-update").await.unwrap();
    assert_eq!(inst.lock().await.data.task_address, "new-address");
}

#[tokio::test]
async fn sandboxer_sandbox_cache_populated() {
    let tmp = tempfile::tempdir().unwrap();
    let (runtime, _log) = MockCombinedRuntime::new();
    let adapter = make_adapter(tmp.path().to_str().unwrap(), runtime);

    create_sandbox(&adapter, "sb-cache", sandbox_option(basic_data())).await;
    start_sandbox(&adapter, "sb-cache").await;

    // Inject a container directly into engine state
    {
        let inst_arc = adapter.engine.get_sandbox("sb-cache").await.unwrap();
        let mut inst = inst_arc.lock().await;
        inst.containers.insert(
            "ctr1".to_string(),
            vmm_engine::instance::ContainerState {
                id: "ctr1".to_string(),
                data: ContainerData::default(),
                io_devices: vec![],
                processes: vec![],
            },
        );
    }

    let view_arc = adapter.sandbox("sb-cache").await.unwrap();
    let view = view_arc.lock().await;
    // Cache should contain ctr1 from snapshot
    assert!(view.containers.contains_key("ctr1"));
}

#[tokio::test]
async fn sandbox_ping_delegates_to_vmm() {
    let tmp = tempfile::tempdir().unwrap();
    let (runtime, _log) = MockCombinedRuntime::new();
    let adapter = make_adapter(tmp.path().to_str().unwrap(), runtime);

    create_sandbox(&adapter, "sb-ping", sandbox_option(basic_data())).await;
    start_sandbox(&adapter, "sb-ping").await;

    let view_arc = adapter.sandbox("sb-ping").await.unwrap();
    view_arc.lock().await.ping().await.unwrap();
    // MockVmm.ping() increments ping_calls; just verify no panic
}

#[tokio::test]
async fn sandbox_container_returns_cached() {
    let tmp = tempfile::tempdir().unwrap();
    let (runtime, _log) = MockCombinedRuntime::new();
    let adapter = make_adapter(tmp.path().to_str().unwrap(), runtime);

    create_sandbox(&adapter, "sb-ctr", sandbox_option(basic_data())).await;
    start_sandbox(&adapter, "sb-ctr").await;

    // Pre-populate local cache via append_container
    let view_arc = adapter.sandbox("sb-ctr").await.unwrap();
    let mut view = view_arc.lock().await;
    let opt = ContainerOption::new(ContainerData {
        id: "c1".to_string(),
        ..Default::default()
    });
    view.append_container("c1", opt).await.unwrap();

    // container() reads from local cache without locking engine
    let c = view.container("c1").await.unwrap();
    assert_eq!(c.get_data().unwrap().id, "c1");
}

#[tokio::test]
async fn sandbox_append_container_updates_local_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let (runtime, _log) = MockCombinedRuntime::new();
    let adapter = make_adapter(tmp.path().to_str().unwrap(), runtime);

    create_sandbox(&adapter, "sb-append", sandbox_option(basic_data())).await;
    start_sandbox(&adapter, "sb-append").await;

    let view_arc = adapter.sandbox("sb-append").await.unwrap();
    let mut view = view_arc.lock().await;
    let opt = ContainerOption::new(ContainerData {
        id: "c2".to_string(),
        ..Default::default()
    });
    view.append_container("c2", opt).await.unwrap();

    assert!(
        view.containers.contains_key("c2"),
        "local cache should contain c2"
    );
}

#[tokio::test]
async fn sandbox_append_container_io_pipes_char_device() {
    let tmp = tempfile::tempdir().unwrap();
    let (runtime, _log) = MockCombinedRuntime::new();
    let adapter = make_adapter(tmp.path().to_str().unwrap(), runtime);

    create_sandbox(&adapter, "sb-io", sandbox_option(basic_data())).await;
    start_sandbox(&adapter, "sb-io").await;

    // Container with stdio pipes (no :// → local paths → CharDevice hot-attach)
    let mut data = ContainerData {
        id: "c-io".to_string(),
        ..Default::default()
    };
    data.io = Some(containerd_sandbox::data::Io {
        stdin: "/tmp/stdin.pipe".to_string(),
        stdout: "/tmp/stdout.pipe".to_string(),
        stderr: "/tmp/stderr.pipe".to_string(),
        terminal: false,
    });

    let view_arc = adapter.sandbox("sb-io").await.unwrap();
    let mut view = view_arc.lock().await;
    view.append_container("c-io", ContainerOption::new(data))
        .await
        .unwrap();

    // Verify hot_attach was called (3 pipes = 3 CharDevice hot-attaches)
    let inst_arc = adapter.engine.get_sandbox("sb-io").await.unwrap();
    let inst = inst_arc.lock().await;
    let c = inst.containers.get("c-io").unwrap();
    assert_eq!(c.io_devices.len(), 3, "3 CharDevice hot-attaches expected");
}

#[tokio::test]
async fn sandbox_append_container_block_storage() {
    let tmp = tempfile::tempdir().unwrap();
    let (runtime, _log) = MockCombinedRuntime::new();
    let adapter = make_adapter(tmp.path().to_str().unwrap(), runtime);

    create_sandbox(&adapter, "sb-blk", sandbox_option(basic_data())).await;
    start_sandbox(&adapter, "sb-blk").await;

    // Container with a block device rootfs
    let mut data = ContainerData {
        id: "c-blk".to_string(),
        ..Default::default()
    };
    data.rootfs = vec![containerd_sandbox::spec::Mount {
        r#type: "bind".to_string(),
        source: "/dev/vda".to_string(),
        destination: "/".to_string(),
        options: vec![],
    }];

    let view_arc = adapter.sandbox("sb-blk").await.unwrap();
    let mut view = view_arc.lock().await;
    // Inject a stub: any /dev/* path counts as a block device (no real fs needed).
    view.block_check =
        std::sync::Arc::new(|path: String| Box::pin(async move { Ok(path.starts_with("/dev/")) }));
    view.append_container("c-blk", ContainerOption::new(data))
        .await
        .unwrap();

    let inst_arc = adapter.engine.get_sandbox("sb-blk").await.unwrap();
    let inst = inst_arc.lock().await;
    assert!(
        inst.storages
            .iter()
            .any(|s| s.kind == vmm_engine::StorageMountKind::Block),
        "block storage mount expected"
    );
}

#[tokio::test]
async fn sandbox_remove_container_clears_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let (runtime, _log) = MockCombinedRuntime::new();
    let adapter = make_adapter(tmp.path().to_str().unwrap(), runtime);

    create_sandbox(&adapter, "sb-rm", sandbox_option(basic_data())).await;
    start_sandbox(&adapter, "sb-rm").await;

    let view_arc = adapter.sandbox("sb-rm").await.unwrap();
    {
        let mut view = view_arc.lock().await;
        let opt = ContainerOption::new(ContainerData {
            id: "c3".to_string(),
            ..Default::default()
        });
        view.append_container("c3", opt).await.unwrap();
        assert!(view.containers.contains_key("c3"));

        view.remove_container("c3").await.unwrap();
        assert!(
            !view.containers.contains_key("c3"),
            "cache should be cleared after remove"
        );
    }
}

#[tokio::test]
async fn sandbox_exit_signal_returns_instance_signal() {
    let tmp = tempfile::tempdir().unwrap();
    let (runtime, _log) = MockCombinedRuntime::new();
    let adapter = make_adapter(tmp.path().to_str().unwrap(), runtime);

    create_sandbox(&adapter, "sb-sig", sandbox_option(basic_data())).await;
    start_sandbox(&adapter, "sb-sig").await;

    let view_arc = adapter.sandbox("sb-sig").await.unwrap();
    let view = view_arc.lock().await;
    let _sig = view.exit_signal().await.unwrap(); // should not panic
}

// ── TaskService tests ─────────────────────────────────────────────────────────

#[tokio::test]
async fn task_service_create_delegates_to_runtime() {
    let tmp = tempfile::tempdir().unwrap();
    let (runtime, log) = MockCombinedRuntime::new();
    let adapter = make_adapter(tmp.path().to_str().unwrap(), runtime);

    create_sandbox(&adapter, "sb-task", sandbox_option(basic_data())).await;
    start_sandbox(&adapter, "sb-task").await;

    let req = CreateTaskRequest {
        id: "sb-task".to_string(),
        bundle: "/bundle".to_string(),
        rootfs: vec![],
        stdin: String::new(),
        stdout: String::new(),
        stderr: String::new(),
        terminal: false,
        spec: vec![],
    };
    let resp = TaskService::create(&adapter, req).await.unwrap();
    assert_eq!(resp.pid, 42);

    let calls = log.lock().unwrap().create_container_calls.clone();
    assert!(calls.iter().any(|c| c.starts_with("sb-task")));
}

#[tokio::test]
async fn task_service_exec_delegates_to_runtime() {
    let tmp = tempfile::tempdir().unwrap();
    let (runtime, log) = MockCombinedRuntime::new();
    let adapter = make_adapter(tmp.path().to_str().unwrap(), runtime);

    create_sandbox(&adapter, "sb-exec", sandbox_option(basic_data())).await;
    start_sandbox(&adapter, "sb-exec").await;

    let req = ExecProcessRequest {
        id: "sb-exec".to_string(),
        exec_id: "exec1".to_string(),
        stdin: String::new(),
        stdout: String::new(),
        stderr: String::new(),
        terminal: false,
        spec: vec![],
    };
    adapter.exec(req).await.unwrap();

    let calls = log.lock().unwrap().exec_process_calls.clone();
    assert!(calls.iter().any(|c| c.contains("exec1")));
}
