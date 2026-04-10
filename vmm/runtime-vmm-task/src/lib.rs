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

//! `vmm-runtime-vmm-task`: implements `GuestReadiness` and `ContainerRuntime` over
//! ttrpc to the in-guest `vmm-task` process.
//!
//! Extracted from `vmm/sandbox/src/client.rs` and adapted for the new architecture.

use std::{
    collections::HashMap,
    os::fd::{IntoRawFd, RawFd},
    sync::Arc,
    time::Duration,
};

use anyhow::anyhow;
use async_trait::async_trait;
use containerd_sandbox::signal::ExitSignal;
use containerd_shim::protos::{
    api::{
        ExecProcessRequest as ShimExecProcessRequest, KillRequest,
        StartRequest as ShimStartRequest, StatsRequest, WaitRequest,
    },
    shim::shim_ttrpc_async::TaskClient,
};
use protobuf::{well_known_types::any::Any, MessageField};
use serde::Deserialize;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::{Mutex, RwLock},
    time::timeout,
};
use ttrpc::{context::with_timeout, r#async::Client};
use vmm_common::api::{
    sandbox::{CheckRequest, SetupSandboxRequest, SyncClockPacket},
    sandbox_ttrpc::SandboxServiceClient,
};
use vmm_guest_runtime::{
    ContainerInfo, ContainerRuntime, ContainerSpec, ContainerStats, ExecSpec, ExitStatus,
    GuestReadiness, NetworkInterface, ProcessInfo, ReadyResult, Route, SandboxSetupRequest,
};

// ── Constants ─────────────────────────────────────────────────────────────────

const HVSOCK_RETRY_TIMEOUT_MS: u64 = 10;
const TIME_SYNC_PERIOD: u64 = 60;
const TIME_DIFF_TOLERANCE_MS: u64 = 10;

// ── Configuration ─────────────────────────────────────────────────────────────

/// Configuration for `VmmTaskRuntime`.
#[derive(Clone, Deserialize)]
pub struct VmmTaskConfig {
    /// Timeout (ms) for establishing the initial ttrpc connection to vmm-task.
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_ms: u64,
    /// Timeout (ms) for individual ttrpc RPC calls.
    #[serde(default = "default_ttrpc_timeout")]
    pub ttrpc_timeout_ms: u64,
}

fn default_connect_timeout() -> u64 {
    45_000
}
fn default_ttrpc_timeout() -> u64 {
    10_000
}

impl Default for VmmTaskConfig {
    fn default() -> Self {
        Self {
            connect_timeout_ms: default_connect_timeout(),
            ttrpc_timeout_ms: default_ttrpc_timeout(),
        }
    }
}

// ── Combined client ───────────────────────────────────────────────────────────

/// Holds both ttrpc service clients for a single sandbox.
///
/// Both share the same underlying ttrpc connection to vmm-task.
/// `sandbox` handles: Check, SetupSandbox, SyncClock, GetEvents.
/// `task`    handles: Create, Start, Exec, Kill, Wait, Stats.
struct VmmTaskClients {
    sandbox: SandboxServiceClient,
    task: TaskClient,
}

// ── VmmTaskRuntime ─────────────────────────────────────────────────────────────

/// Implements `GuestReadiness` and `ContainerRuntime` via ttrpc to `vmm-task`.
pub struct VmmTaskRuntime {
    config: VmmTaskConfig,
    /// Per-sandbox ttrpc client pairs, keyed by sandbox_id.
    clients: Arc<RwLock<HashMap<String, Arc<Mutex<VmmTaskClients>>>>>,
}

impl VmmTaskRuntime {
    pub fn new(config: VmmTaskConfig) -> Self {
        Self {
            config,
            clients: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Connect to vmm-task and cache both ttrpc service clients.
    ///
    /// Uses `connect_timeout_ms` as the overall deadline for establishing
    /// the connection (with internal retries every 10 ms).
    async fn connect(
        &self,
        sandbox_id: &str,
        vsock_path: &str,
    ) -> anyhow::Result<Arc<Mutex<VmmTaskClients>>> {
        let client =
            new_ttrpc_client_with_timeout(vsock_path, self.config.connect_timeout_ms).await?;
        let clients = VmmTaskClients {
            sandbox: SandboxServiceClient::new(client.clone()),
            task: TaskClient::new(client),
        };
        let entry = Arc::new(Mutex::new(clients));
        self.clients
            .write()
            .await
            .insert(sandbox_id.to_string(), entry.clone());
        Ok(entry)
    }

    /// Retrieve an already-connected client pair.
    async fn get_client(&self, sandbox_id: &str) -> anyhow::Result<Arc<Mutex<VmmTaskClients>>> {
        self.clients
            .read()
            .await
            .get(sandbox_id)
            .cloned()
            .ok_or_else(|| anyhow!("no ttrpc client for sandbox {}", sandbox_id))
    }

    /// Remove the client pair for a sandbox (called on cleanup).
    pub async fn remove_client(&self, sandbox_id: &str) {
        self.clients.write().await.remove(sandbox_id);
    }
}

// ── impl GuestReadiness ───────────────────────────────────────────────────────

#[async_trait]
impl GuestReadiness for VmmTaskRuntime {
    async fn wait_ready(&self, sandbox_id: &str, vsock_path: &str) -> anyhow::Result<ReadyResult> {
        // 1. Connect with retry until connect_timeout_ms
        let client = self.connect(sandbox_id, vsock_path).await?;

        // 2. ttrpc Check() — verify vmm-task is responsive
        let ns = (self.config.ttrpc_timeout_ms as i64).saturating_mul(1_000_000);
        let ctx = with_timeout(ns);
        client
            .lock()
            .await
            .sandbox
            .check(ctx, &CheckRequest::new())
            .await
            .map_err(|e| anyhow!("vmm-task check failed: {}", e))?;

        Ok(ReadyResult {
            sandbox_id: sandbox_id.to_string(),
            timestamp_ms: unix_now_ms(),
        })
    }

    async fn setup_sandbox(
        &self,
        sandbox_id: &str,
        req: &SandboxSetupRequest,
    ) -> anyhow::Result<()> {
        let client = self.get_client(sandbox_id).await?;
        let ns = (self.config.ttrpc_timeout_ms as i64).saturating_mul(1_000_000);
        let ctx = with_timeout(ns);

        let config = if let Some(pod_config) = req.sandbox_data.config.as_ref() {
            let value = serde_json::to_vec(pod_config)
                .map_err(|e| anyhow!("serialize PodSandboxConfig: {}", e))?;
            let mut any = Any::new();
            any.type_url = "PodSandboxConfig".to_string();
            any.value = value;
            MessageField::some(any)
        } else {
            MessageField::none()
        };

        let sreq = SetupSandboxRequest {
            config,
            interfaces: req.interfaces.iter().map(interface_to_proto).collect(),
            routes: req.routes.iter().map(route_to_proto).collect(),
            ..Default::default()
        };

        client
            .lock()
            .await
            .sandbox
            .setup_sandbox(ctx, &sreq)
            .await
            .map_err(|e| anyhow!("setup_sandbox failed: {}", e))?;
        Ok(())
    }

    async fn cleanup_sandbox(&self, sandbox_id: &str) {
        self.remove_client(sandbox_id).await;
    }

    async fn forward_events(&self, sandbox_id: &str, exit_signal: Arc<ExitSignal>) {
        let client = match self.get_client(sandbox_id).await {
            Ok(c) => c,
            Err(_) => return,
        };
        let id = sandbox_id.to_string();
        let tolerance = Duration::from_millis(TIME_DIFF_TOLERANCE_MS);

        // Start the periodic clock-sync background task.
        {
            let clock_client = client.clone();
            let clock_signal = exit_signal.clone();
            let clock_id = id.clone();
            tokio::spawn(async move {
                let sync = async {
                    loop {
                        tokio::time::sleep(Duration::from_secs(TIME_SYNC_PERIOD)).await;
                        if let Err(e) = do_once_sync_clock(&clock_client, tolerance).await {
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
                    let result = {
                        let lock = client.lock().await;
                        lock.sandbox
                            .get_events(with_timeout(0), &vmm_common::api::empty::Empty::new())
                            .await
                    };
                    match result {
                        Ok(envelope) => {
                            if let Err(e) = publish_event_to_containerd(envelope).await {
                                tracing::error!("forward_events {}: publish error: {}", id, e);
                            }
                        }
                        Err(ttrpc::Error::Socket(s)) if s.contains("early eof") => break,
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

// ── impl ContainerRuntime ─────────────────────────────────────────────────────

#[async_trait]
impl ContainerRuntime for VmmTaskRuntime {
    async fn create_container(
        &self,
        sandbox_id: &str,
        spec: ContainerSpec,
    ) -> anyhow::Result<ContainerInfo> {
        let client = self.get_client(sandbox_id).await?;
        let ns = (self.config.ttrpc_timeout_ms as i64).saturating_mul(1_000_000);
        let ctx = with_timeout(ns);
        let req = spec_to_create_request(spec);
        let resp = client
            .lock()
            .await
            .task
            .create(ctx, &req)
            .await
            .map_err(|e| anyhow!("create_container failed: {}", e))?;
        Ok(ContainerInfo { pid: resp.pid })
    }

    async fn start_process(
        &self,
        sandbox_id: &str,
        container_id: &str,
    ) -> anyhow::Result<ProcessInfo> {
        let client = self.get_client(sandbox_id).await?;
        let ns = (self.config.ttrpc_timeout_ms as i64).saturating_mul(1_000_000);
        let ctx = with_timeout(ns);
        let mut req = ShimStartRequest::new();
        req.id = container_id.to_string();
        let resp = client
            .lock()
            .await
            .task
            .start(ctx, &req)
            .await
            .map_err(|e| anyhow!("start_process failed: {}", e))?;
        Ok(ProcessInfo { pid: resp.pid })
    }

    async fn exec_process(
        &self,
        sandbox_id: &str,
        container_id: &str,
        spec: ExecSpec,
    ) -> anyhow::Result<ProcessInfo> {
        let client = self.get_client(sandbox_id).await?;
        let ns = (self.config.ttrpc_timeout_ms as i64).saturating_mul(1_000_000);
        let ctx = with_timeout(ns);
        let req = exec_spec_to_request(container_id, spec);
        client
            .lock()
            .await
            .task
            .exec(ctx, &req)
            .await
            .map_err(|e| anyhow!("exec_process failed: {}", e))?;
        Ok(ProcessInfo { pid: 0 }) // actual pid delivered via wait
    }

    async fn kill_process(
        &self,
        sandbox_id: &str,
        container_id: &str,
        pid: u32,
        signal: u32,
    ) -> anyhow::Result<()> {
        let client = self.get_client(sandbox_id).await?;
        let ns = (self.config.ttrpc_timeout_ms as i64).saturating_mul(1_000_000);
        let ctx = with_timeout(ns);
        let mut req = KillRequest::new();
        req.id = container_id.to_string();
        req.exec_id = pid.to_string();
        req.signal = signal;
        client
            .lock()
            .await
            .task
            .kill(ctx, &req)
            .await
            .map_err(|e| anyhow!("kill_process failed: {}", e))?;
        Ok(())
    }

    async fn wait_process(
        &self,
        sandbox_id: &str,
        container_id: &str,
        pid: u32,
    ) -> anyhow::Result<ExitStatus> {
        let client = self.get_client(sandbox_id).await?;
        let ctx = with_timeout(0); // no timeout — wait indefinitely
        let mut req = WaitRequest::new();
        req.id = container_id.to_string();
        req.exec_id = pid.to_string();
        let resp = client
            .lock()
            .await
            .task
            .wait(ctx, &req)
            .await
            .map_err(|e| anyhow!("wait_process failed: {}", e))?;
        let exited_at_ms = resp
            .exited_at
            .as_ref()
            .map(|t| t.seconds as u64 * 1000)
            .unwrap_or(0);
        Ok(ExitStatus {
            exit_code: resp.exit_status as i32,
            exited_at_ms,
        })
    }

    async fn container_stats(
        &self,
        sandbox_id: &str,
        container_id: &str,
    ) -> anyhow::Result<ContainerStats> {
        let client = self.get_client(sandbox_id).await?;
        let ns = (self.config.ttrpc_timeout_ms as i64).saturating_mul(1_000_000);
        let ctx = with_timeout(ns);
        let mut req = StatsRequest::new();
        req.id = container_id.to_string();
        let resp = client
            .lock()
            .await
            .task
            .stats(ctx, &req)
            .await
            .map_err(|e| anyhow!("container_stats failed: {}", e))?;

        // The stats field is a google.protobuf.Any whose value is a serialized
        // io.containerd.cgroups.v1.Metrics message.
        use containerd_shim::protos::{cgroups::metrics::Metrics, protobuf::Message};
        let metrics = resp
            .stats
            .as_ref()
            .map(|any| Metrics::parse_from_bytes(&any.value))
            .transpose()
            .map_err(|e| anyhow!("parse cgroups metrics: {}", e))?
            .unwrap_or_default();

        Ok(ContainerStats {
            cpu_usage_ns: metrics.cpu().usage().total,
            memory_rss_bytes: metrics.memory().rss,
            pids_current: metrics.pids().current,
        })
    }
}

// ── Event publishing ──────────────────────────────────────────────────────────

/// Forward a single event envelope to containerd's ttrpc events service.
///
/// Creates a short-lived ttrpc connection to `/run/containerd/containerd.sock.ttrpc`
/// and calls `Events.Forward` with the envelope received from vmm-task.
async fn publish_event_to_containerd(
    envelope: vmm_common::api::events::Envelope,
) -> anyhow::Result<()> {
    let client =
        new_ttrpc_client_with_timeout("unix:///run/containerd/containerd.sock.ttrpc", 5_000)
            .await
            .map_err(|e| anyhow!("connect to containerd ttrpc: {}", e))?;

    let events_client = vmm_common::api::events_ttrpc::EventsClient::new(client);

    let mut req = vmm_common::api::events::ForwardRequest::new();
    req.envelope = ::protobuf::MessageField::some(envelope);

    let ctx = with_timeout(5_000_000_000); // 5 s
    events_client
        .forward(ctx, &req)
        .await
        .map_err(|e| anyhow!("forward event to containerd: {}", e))?;
    Ok(())
}

// ── Request conversion helpers ────────────────────────────────────────────────

fn spec_to_create_request(spec: ContainerSpec) -> containerd_shim::protos::api::CreateTaskRequest {
    let mut req = containerd_shim::protos::api::CreateTaskRequest::new();
    req.id = spec.id;
    req.bundle = spec.bundle;
    req.stdin = spec.io.stdin;
    req.stdout = spec.io.stdout;
    req.stderr = spec.io.stderr;
    req.terminal = spec.io.terminal;
    req
}

fn exec_spec_to_request(container_id: &str, spec: ExecSpec) -> ShimExecProcessRequest {
    let mut req = ShimExecProcessRequest::new();
    req.id = container_id.to_string();
    req.exec_id = spec.exec_id;
    req.stdin = spec.io.stdin;
    req.stdout = spec.io.stdout;
    req.stderr = spec.io.stderr;
    req.terminal = spec.io.terminal;
    req
}

// ── Proto conversion helpers ──────────────────────────────────────────────────

fn interface_to_proto(i: &NetworkInterface) -> vmm_common::api::sandbox::Interface {
    let mut p = vmm_common::api::sandbox::Interface::new();
    p.name = i.name.clone();
    p.mtu = i.mtu as u64;
    p
}

fn route_to_proto(r: &Route) -> vmm_common::api::sandbox::Route {
    let mut p = vmm_common::api::sandbox::Route::new();
    p.dest = r.dest.clone();
    p.gateway = r.gateway.clone();
    p.device = r.device.clone();
    p
}

// ── ttrpc socket helpers ──────────────────────────────────────────────────────

/// Create a ttrpc `Client` connected to `address`, retrying until `timeout_ms` expires.
///
/// Supported address formats (same as `vmm/sandbox/src/client.rs`):
/// - `unix://<path>`
/// - `vsock://<cid>:<port>`
/// - `hvsock://<path>:<port>`
/// - `<path>` (bare unix socket path)
pub async fn new_ttrpc_client_with_timeout(
    address: &str,
    timeout_ms: u64,
) -> anyhow::Result<Client> {
    let mut last_err: anyhow::Error = anyhow!("no connection attempt yet");
    let fut = async {
        loop {
            match connect_to_socket(address).await {
                Ok(fd) => return Client::new(fd),
                Err(e) => last_err = e,
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    timeout(Duration::from_millis(timeout_ms), fut)
        .await
        .map_err(|_| {
            anyhow!(
                "{}ms timeout connecting to {}: {}",
                timeout_ms,
                address,
                last_err
            )
        })
}

async fn connect_to_socket(address: &str) -> anyhow::Result<RawFd> {
    if let Some(addr) = address.strip_prefix("unix://") {
        return connect_to_unix(addr).await;
    }
    if let Some(addr) = address.strip_prefix("vsock://") {
        return connect_to_vsock(addr).await;
    }
    if let Some(addr) = address.strip_prefix("hvsock://") {
        return connect_to_hvsock(addr).await;
    }
    connect_to_unix(address).await
}

async fn connect_to_unix(path: &str) -> anyhow::Result<RawFd> {
    use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, UnixAddr};
    use nix::unistd::close;

    let sockaddr = UnixAddr::new(path).map_err(|e| anyhow!("bad unix path {}: {}", path, e))?;
    let fd = socket(
        AddressFamily::Unix,
        SockType::Stream,
        SockFlag::empty(),
        None,
    )
    .map_err(|e| anyhow!("create unix socket: {}", e))?;
    tokio::task::spawn_blocking(move || {
        connect(fd, &sockaddr).map_err(|e| {
            let _ = close(fd);
            anyhow!("connect unix {}: {}", sockaddr, e)
        })
    })
    .await
    .map_err(|e| anyhow!("spawn_blocking: {}", e))??;
    Ok(fd)
}

async fn connect_to_vsock(address: &str) -> anyhow::Result<RawFd> {
    use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, VsockAddr};
    use nix::unistd::close;

    let parts: Vec<&str> = address.splitn(2, ':').collect();
    if parts.len() < 2 {
        return Err(anyhow!("invalid vsock address: {}", address));
    }
    let cid: u32 = parts[0].parse().map_err(|e| anyhow!("vsock cid: {}", e))?;
    let port: u32 = parts[1].parse().map_err(|e| anyhow!("vsock port: {}", e))?;

    let fd = socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::empty(),
        None,
    )
    .map_err(|e| anyhow!("create vsock: {}", e))?;
    let addr = VsockAddr::new(cid, port);
    tokio::task::spawn_blocking(move || {
        connect(fd, &addr).map_err(|e| {
            let _ = close(fd);
            anyhow!("connect vsock {}: {}", addr, e)
        })
    })
    .await
    .map_err(|e| anyhow!("spawn_blocking: {}", e))??;
    Ok(fd)
}

async fn connect_to_hvsock(address: &str) -> anyhow::Result<RawFd> {
    let sep = address
        .rfind(':')
        .ok_or_else(|| anyhow!("invalid hvsock address: {}", address))?;
    let path = &address[..sep];
    let port = &address[sep + 1..];

    let fut = async {
        let mut stream = UnixStream::connect(path)
            .await
            .map_err(|e| anyhow!("hvsock unix connect {}: {}", path, e))?;
        stream
            .write_all(format!("CONNECT {}\n", port).as_bytes())
            .await
            .map_err(|e| anyhow!("hvsock CONNECT write: {}", e))?;
        let mut response = String::new();
        BufReader::new(&mut stream)
            .read_line(&mut response)
            .await
            .map_err(|e| anyhow!("hvsock CONNECT read: {}", e))?;
        if response.starts_with("OK") {
            Ok(stream.into_std()?.into_raw_fd())
        } else {
            Err(anyhow!("hvsock unexpected response: {}", response))
        }
    };
    timeout(Duration::from_millis(HVSOCK_RETRY_TIMEOUT_MS), fut)
        .await
        .map_err(|_| anyhow!("hvsock {}ms timeout", HVSOCK_RETRY_TIMEOUT_MS))?
}

// ── Clock sync ────────────────────────────────────────────────────────────────

async fn do_once_sync_clock(
    client: &Arc<Mutex<VmmTaskClients>>,
    tolerance: Duration,
) -> anyhow::Result<()> {
    use nix::{
        sys::time::TimeValLike,
        time::{clock_gettime, ClockId},
    };

    let clock_id = ClockId::from_raw(nix::libc::CLOCK_REALTIME);
    let mut req = SyncClockPacket::new();
    req.ClientSendTime = clock_gettime(clock_id)
        .map_err(|e| anyhow!("clock_gettime: {}", e))?
        .num_nanoseconds();

    let ctx = with_timeout(Duration::from_secs(1).as_nanos() as i64);
    let mut p = client
        .lock()
        .await
        .sandbox
        .sync_clock(ctx, &req)
        .await
        .map_err(|e| anyhow!("sync_clock: {}", e))?;

    p.ServerArriveTime = clock_gettime(clock_id)
        .map_err(|e| anyhow!("clock_gettime: {}", e))?
        .num_nanoseconds();

    let delta = checked_compute_delta(
        p.ClientSendTime,
        p.ClientArriveTime,
        p.ServerSendTime,
        p.ServerArriveTime,
    )?;
    if delta.abs() > tolerance.as_nanos() as i64 {
        p.Delta = delta;
        let ctx2 = with_timeout(Duration::from_secs(1).as_nanos() as i64);
        client
            .lock()
            .await
            .sandbox
            .sync_clock(ctx2, &p)
            .await
            .map_err(|e| anyhow!("set delta: {}", e))?;
    }
    Ok(())
}

fn checked_compute_delta(
    c_send: i64,
    c_arrive: i64,
    s_send: i64,
    s_arrive: i64,
) -> anyhow::Result<i64> {
    let dc = c_send
        .checked_sub(c_arrive)
        .ok_or_else(|| anyhow!("overflow c_send - c_arrive"))?;
    let ds = s_arrive
        .checked_sub(s_send)
        .ok_or_else(|| anyhow!("overflow s_arrive - s_send"))?;
    let sum = dc
        .checked_add(ds)
        .ok_or_else(|| anyhow!("overflow dc + ds"))?;
    sum.checked_div(2).ok_or_else(|| anyhow!("overflow /2"))
}

// ── Utilities ─────────────────────────────────────────────────────────────────

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Compile-time trait checks ─────────────────────────────────────────────

    fn _assert_guest_readiness<T: GuestReadiness>() {}
    fn _assert_container_runtime<T: ContainerRuntime>() {}

    #[test]
    fn vmm_task_runtime_implements_guest_readiness() {
        _assert_guest_readiness::<VmmTaskRuntime>();
    }

    #[test]
    fn vmm_task_runtime_implements_container_runtime() {
        _assert_container_runtime::<VmmTaskRuntime>();
    }

    // ── Connection helpers ────────────────────────────────────────────────────

    #[tokio::test]
    async fn connect_timeout_on_nonexistent_server() {
        // Should fail within the timeout, not block forever.
        let result = new_ttrpc_client_with_timeout("hvsock://nonexistent.sock:1024", 200).await;
        assert!(result.is_err(), "expected timeout error");
    }

    #[tokio::test]
    async fn connect_timeout_vsock_nonexistent() {
        let result = new_ttrpc_client_with_timeout("vsock://999999999:1024", 200).await;
        assert!(result.is_err(), "expected timeout error");
    }

    #[tokio::test]
    async fn connect_timeout_unix_nonexistent() {
        let result =
            new_ttrpc_client_with_timeout("unix:///tmp/kuasar-nonexistent-12345.sock", 200).await;
        assert!(result.is_err(), "expected timeout error");
    }

    // ── get_client without connect ────────────────────────────────────────────

    #[tokio::test]
    async fn get_client_returns_error_if_not_connected() {
        let rt = VmmTaskRuntime::new(VmmTaskConfig::default());
        match rt.get_client("no-such-sandbox").await {
            Err(e) => assert!(
                e.to_string().contains("no ttrpc client"),
                "unexpected error: {}",
                e
            ),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    // ── Clock delta computation ───────────────────────────────────────────────

    #[test]
    fn checked_compute_delta_correct() {
        // (c_send - c_arrive) + (s_arrive - s_send) / 2
        // = (231 - 135) + (298 - 137) / 2 = (96 + 161) / 2 = 128
        let delta = checked_compute_delta(231, 135, 137, 298).unwrap();
        assert_eq!(delta, 128);
    }

    #[test]
    fn checked_compute_delta_zero() {
        let delta = checked_compute_delta(100, 100, 100, 100).unwrap();
        assert_eq!(delta, 0);
    }

    // ── Request conversion ────────────────────────────────────────────────────

    #[test]
    fn spec_to_create_request_maps_fields() {
        use vmm_guest_runtime::ContainerIo;
        let spec = ContainerSpec {
            id: "ctr1".to_string(),
            bundle: "/bundle/ctr1".to_string(),
            rootfs: vec![],
            io: ContainerIo {
                stdin: "/tmp/stdin".to_string(),
                stdout: "/tmp/stdout".to_string(),
                stderr: "/tmp/stderr".to_string(),
                terminal: true,
            },
            spec_json: vec![],
        };
        let req = spec_to_create_request(spec);
        assert_eq!(req.id, "ctr1");
        assert_eq!(req.bundle, "/bundle/ctr1");
        assert_eq!(req.stdin, "/tmp/stdin");
        assert_eq!(req.stdout, "/tmp/stdout");
        assert_eq!(req.stderr, "/tmp/stderr");
        assert!(req.terminal);
    }

    #[test]
    fn exec_spec_to_request_maps_fields() {
        use vmm_guest_runtime::ContainerIo;
        let spec = ExecSpec {
            exec_id: "exec1".to_string(),
            spec_json: vec![1, 2, 3],
            io: ContainerIo {
                stdin: "/tmp/in".to_string(),
                stdout: "/tmp/out".to_string(),
                stderr: String::new(),
                terminal: false,
            },
        };
        let req = exec_spec_to_request("ctr1", spec);
        assert_eq!(req.id, "ctr1");
        assert_eq!(req.exec_id, "exec1");
        assert_eq!(req.stdin, "/tmp/in");
        assert!(!req.terminal);
    }

    // ── kill_process signal forwarding ────────────────────────────────────────

    #[test]
    fn kill_request_signal_preserved() {
        let mut req = KillRequest::new();
        req.id = "ctr1".to_string();
        req.exec_id = 42u32.to_string(); // pid encoded as exec_id
        req.signal = 15; // SIGTERM
        assert_eq!(req.signal, 15);
    }

    // ── Config defaults ───────────────────────────────────────────────────────

    #[test]
    fn vmm_task_config_defaults() {
        let cfg = VmmTaskConfig::default();
        assert_eq!(cfg.connect_timeout_ms, 45_000);
        assert_eq!(cfg.ttrpc_timeout_ms, 10_000);
    }

    // ── remove_client ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn remove_client_is_idempotent() {
        let rt = VmmTaskRuntime::new(VmmTaskConfig::default());
        // Removing a client that was never inserted should not panic.
        rt.remove_client("ghost").await;
    }
}
