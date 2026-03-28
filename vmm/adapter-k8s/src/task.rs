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

//! Task API delegation: K8sAdapter → ContainerRuntime (vmm-guest-runtime).
//!
//! `TaskService` is a local trait that mirrors the conceptual containerd Task
//! API surface. Each method maps 1:1 to a `ContainerRuntime` call.  The actual
//! wire-protocol (ttrpc/gRPC serialisation) is handled by the process startup
//! wiring in Story 1.6; this layer only concerns itself with the Rust types.

use async_trait::async_trait;
use vmm_guest_runtime::{
    ContainerIo, ContainerRuntime, ContainerSpec, ExecSpec, ExitStatus, Mount,
};

use crate::K8sAdapter;
use vmm_guest_runtime::GuestReadiness;
use vmm_vm_trait::{Hooks, Vmm};

// ── Request / response types ──────────────────────────────────────────────────

pub struct CreateTaskRequest {
    pub id: String,
    pub bundle: String,
    pub rootfs: Vec<TaskMount>,
    pub stdin: String,
    pub stdout: String,
    pub stderr: String,
    pub terminal: bool,
    pub spec: Vec<u8>,
}

pub struct CreateTaskResponse {
    pub pid: u32,
}

pub struct StartRequest {
    pub id: String,
    pub exec_id: String,
}

pub struct StartResponse {
    pub pid: u32,
}

pub struct ExecProcessRequest {
    pub id: String,
    pub exec_id: String,
    pub stdin: String,
    pub stdout: String,
    pub stderr: String,
    pub terminal: bool,
    pub spec: Vec<u8>,
}

pub struct KillRequest {
    pub id: String,
    pub exec_id: String,
    pub pid: u32,
    pub signal: u32,
}

pub struct WaitRequest {
    pub id: String,
    pub exec_id: String,
    pub pid: u32,
}

pub struct WaitResponse {
    pub exit_status: u32,
    pub exited_at: u64,
}

pub struct StatsRequest {
    pub id: String,
    pub exec_id: String,
}

pub struct StatsResponse {
    pub cpu_usage_ns: u64,
    pub memory_rss_bytes: u64,
    pub pids_current: u64,
}

pub struct TaskMount {
    pub kind: String,
    pub source: String,
    pub target: String,
    pub options: Vec<String>,
}

// ── TaskService trait ─────────────────────────────────────────────────────────

/// Task API surface delegated to `ContainerRuntime` (vmm-task ttrpc).
///
/// Implemented by `K8sAdapter<V, R, H>` where `R: GuestReadiness + ContainerRuntime`.
#[async_trait]
pub trait TaskService: Send + Sync {
    async fn create(&self, req: CreateTaskRequest) -> anyhow::Result<CreateTaskResponse>;

    async fn start(&self, req: StartRequest) -> anyhow::Result<StartResponse>;

    async fn exec(&self, req: ExecProcessRequest) -> anyhow::Result<()>;

    async fn kill(&self, req: KillRequest) -> anyhow::Result<()>;

    async fn wait(&self, req: WaitRequest) -> anyhow::Result<WaitResponse>;

    async fn stats(&self, req: StatsRequest) -> anyhow::Result<StatsResponse>;
}

// ── impl TaskService for K8sAdapter ──────────────────────────────────────────

#[async_trait]
impl<V, R, H> TaskService for K8sAdapter<V, R, H>
where
    V: Vmm + 'static,
    R: GuestReadiness + ContainerRuntime + 'static,
    H: Hooks<V> + 'static,
{
    async fn create(&self, req: CreateTaskRequest) -> anyhow::Result<CreateTaskResponse> {
        let spec = ContainerSpec {
            id: req.id.clone(),
            bundle: req.bundle.clone(),
            rootfs: req.rootfs.into_iter().map(task_mount_to_mount).collect(),
            io: ContainerIo {
                stdin: req.stdin,
                stdout: req.stdout,
                stderr: req.stderr,
                terminal: req.terminal,
            },
            spec_json: req.spec,
        };
        let info = self
            .engine
            .runtime()
            .create_container(&req.id, spec)
            .await?;
        Ok(CreateTaskResponse { pid: info.pid })
    }

    async fn start(&self, req: StartRequest) -> anyhow::Result<StartResponse> {
        let info = self
            .engine
            .runtime()
            .start_process(&req.id, &req.exec_id)
            .await?;
        Ok(StartResponse { pid: info.pid })
    }

    async fn exec(&self, req: ExecProcessRequest) -> anyhow::Result<()> {
        let spec = ExecSpec {
            exec_id: req.exec_id.clone(),
            spec_json: req.spec,
            io: ContainerIo {
                stdin: req.stdin,
                stdout: req.stdout,
                stderr: req.stderr,
                terminal: req.terminal,
            },
        };
        self.engine
            .runtime()
            .exec_process(&req.id, &req.exec_id, spec)
            .await?;
        Ok(())
    }

    async fn kill(&self, req: KillRequest) -> anyhow::Result<()> {
        self.engine
            .runtime()
            .kill_process(&req.id, &req.exec_id, req.pid, req.signal)
            .await
    }

    async fn wait(&self, req: WaitRequest) -> anyhow::Result<WaitResponse> {
        let exit: ExitStatus = self
            .engine
            .runtime()
            .wait_process(&req.id, &req.exec_id, req.pid)
            .await?;
        Ok(WaitResponse {
            exit_status: exit.exit_code as u32,
            exited_at: exit.exited_at_ms,
        })
    }

    async fn stats(&self, req: StatsRequest) -> anyhow::Result<StatsResponse> {
        let s = self
            .engine
            .runtime()
            .container_stats(&req.id, &req.exec_id)
            .await?;
        Ok(StatsResponse {
            cpu_usage_ns: s.cpu_usage_ns,
            memory_rss_bytes: s.memory_rss_bytes,
            pids_current: s.pids_current,
        })
    }
}

fn task_mount_to_mount(m: TaskMount) -> Mount {
    Mount {
        kind: m.kind,
        source: m.source,
        target: m.target,
        options: m.options,
    }
}
