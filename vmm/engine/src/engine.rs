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
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use containerd_sandbox::data::SandboxData;
use containerd_sandbox::signal::ExitSignal;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::sync::{Mutex, RwLock};
use vmm_common::cgroup::{SandboxCgroup, DEFAULT_CGROUP_PARENT_PATH};
use vmm_common::{
    ETC_HOSTS, ETC_RESOLV, HOSTNAME_FILENAME, HOSTS_FILENAME, RESOLV_FILENAME, SHARED_DIR_SUFFIX,
};
use vmm_guest_runtime::{GuestReadiness, NetworkInterface, Route, SandboxSetupRequest};
use vmm_vm_trait::{ExitInfo, Hooks, Vmm, VmmNetworkConfig};

use crate::config::EngineConfig;
use crate::error::{Error, Result};
use crate::instance::{NetworkState, SandboxInstance, SandboxSummary};
use crate::state::{SandboxState, StateEvent};
use crate::{CreateSandboxRequest, StartResult};

type SandboxMap<V> = Arc<RwLock<HashMap<String, Arc<Mutex<SandboxInstance<V>>>>>>;

/// The sandbox engine. Manages sandbox lifecycle, VMM boot/stop, and recovery.
///
/// - `V` — VMM backend (e.g. CloudHypervisor, Qemu, StratoVirt)
/// - `R` — Guest readiness / ttrpc runtime (e.g. VmmTaskRuntime)
/// - `H` — Lifecycle hooks (e.g. CloudHypervisorHooks)
pub struct SandboxEngine<V: Vmm, R: GuestReadiness, H: Hooks<V>> {
    /// Cloned into each `V::create()` call; replaces `VmmFactory`.
    vmm_config: V::Config,
    runtime: R,
    hooks: H,
    pub(crate) config: EngineConfig,
    pub(crate) sandboxes: SandboxMap<V>,
    /// Unique vsock CID allocator. Starts at 3 (0 = hypervisor, 1 = loopback, 2 = host).
    next_vsock_cid: AtomicU32,
}

impl<V: Vmm, R: GuestReadiness, H: Hooks<V>> SandboxEngine<V, R, H> {
    pub fn new(vmm_config: V::Config, runtime: R, hooks: H, config: EngineConfig) -> Self {
        Self {
            vmm_config,
            runtime,
            hooks,
            config,
            sandboxes: Arc::new(RwLock::new(HashMap::new())),
            next_vsock_cid: AtomicU32::new(3),
        }
    }

    /// Expose the runtime so `K8sAdapter` can forward Task API calls directly.
    pub fn runtime(&self) -> &R {
        &self.runtime
    }

    // ── Public sandbox operations ─────────────────────────────────────────────

    pub async fn create_sandbox(&self, id: &str, req: CreateSandboxRequest) -> Result<()>
    where
        V: Serialize + DeserializeOwned + Default,
    {
        // Idempotency guard
        if self.sandboxes.read().await.contains_key(id) {
            return Err(Error::AlreadyExists(id.to_string()));
        }

        let base_dir = format!("{}/{}", self.config.work_dir, id);
        tokio::fs::create_dir_all(&base_dir)
            .await
            .map_err(|e| Error::Other(anyhow::anyhow!("create base_dir: {}", e)))?;

        // Write hostname, /etc/hosts, resolv.conf into the virtiofs-shared directory.
        setup_sandbox_files(&base_dir, &req.sandbox_data).await?;

        // Create host-side cgroup (cgroup-v1 only; no-op on cgroup-v2).
        let cgroup_parent = if req.cgroup_parent.is_empty() {
            DEFAULT_CGROUP_PARENT_PATH.to_string()
        } else {
            req.cgroup_parent.clone()
        };
        let cgroup =
            SandboxCgroup::create_sandbox_cgroups(&cgroup_parent, id).map_err(Error::Other)?;
        cgroup
            .update_res_for_sandbox_cgroups(&req.sandbox_data)
            .map_err(Error::Other)?;

        // Allocate a unique vsock CID for this sandbox.
        let vsock_cid = self.next_vsock_cid.fetch_add(1, Ordering::Relaxed);

        // Construct the VMM instance (no process started; just allocates struct).
        let mut vmm = V::create(id, &base_dir, &self.vmm_config, vsock_cid)
            .await
            .map_err(Error::Other)?;

        if let Some(disk) = req.rootfs_disk {
            vmm.add_disk(disk).map_err(Error::Other)?;
        }

        let mut instance = SandboxInstance {
            id: id.to_string(),
            vmm,
            state: SandboxState::Creating,
            base_dir: base_dir.clone(),
            data: req.sandbox_data,
            netns: req.netns,
            network: None,
            storages: vec![],
            containers: HashMap::new(),
            id_generator: 0,
            vsock_port_next: 1025,
            cgroup,
            exit_signal: Arc::new(ExitSignal::default()),
        };

        // post_create hook
        {
            let mut ctx = instance.make_ctx();
            self.hooks
                .post_create(&mut ctx)
                .await
                .map_err(Error::Other)?;
        }

        instance.dump().await?;
        self.sandboxes
            .write()
            .await
            .insert(id.to_string(), Arc::new(Mutex::new(instance)));
        Ok(())
    }

    pub async fn start_sandbox(&self, id: &str) -> Result<StartResult>
    where
        V: Serialize + DeserializeOwned + Default,
    {
        let instance_mutex = self.get_sandbox(id).await?;
        let t0 = Instant::now();

        let mut instance = instance_mutex.lock().await;

        // Guard: must be in Creating state
        if instance.state != SandboxState::Creating {
            return Err(Error::InvalidState(format!(
                "sandbox {} must be in Creating state to start, got {:?}",
                id, instance.state
            )));
        }

        // 1. pre_start hook
        {
            let mut ctx = instance.make_ctx();
            if let Err(e) = self.hooks.pre_start(&mut ctx).await {
                instance.state = SandboxState::Stopped;
                instance.dump().await.ok();
                return Err(Error::Other(e));
            }
        }

        // vcpu count for network queue sizing
        let vcpu = vcpu_count_from_resources(&instance.data);

        // 2. Prepare network: enter pod netns, discover interfaces + routes, attach taps.
        if !instance.netns.is_empty() {
            let discovered =
                match vmm_common::network::discover_network(&instance.netns, &instance.id, vcpu)
                    .await
                {
                    Ok(n) => n,
                    Err(e) => {
                        instance.state = SandboxState::Stopped;
                        instance.dump().await.ok();
                        return Err(Error::Other(e));
                    }
                };
            let netns = instance.netns.clone();
            for iface in &discovered.interfaces {
                let net_config = build_network_config(iface, vcpu, &netns);
                instance.vmm.add_network(net_config).map_err(Error::Other)?;
            }
            let tap_names: Vec<String> = discovered
                .interfaces
                .iter()
                .filter(|i| matches!(i.link_type, vmm_common::network::LinkType::Tap))
                .map(|i| i.name.clone())
                .collect();
            let physical_nics: Vec<vmm_common::network::PhysicalNicState> = discovered
                .interfaces
                .iter()
                .filter(|i| matches!(i.link_type, vmm_common::network::LinkType::Physical(..)))
                .map(|i| vmm_common::network::PhysicalNicState {
                    bdf: i.pci_address.clone(),
                    original_driver: i.pci_driver.clone(),
                })
                .collect();
            instance.network = Some(NetworkState {
                interfaces: discovered
                    .interfaces
                    .into_iter()
                    .map(|i| NetworkInterface {
                        name: i.name,
                        mac: i.mac,
                        ip_addresses: i.ip_addresses,
                        mtu: i.mtu,
                    })
                    .collect(),
                routes: discovered
                    .routes
                    .into_iter()
                    .map(|r| Route {
                        dest: r.dest,
                        gateway: r.gateway,
                        device: r.device,
                    })
                    .collect(),
                tap_names,
                physical_nics,
            });
        }

        // 3. Boot VMM
        let t_boot = Instant::now();
        if let Err(e) = instance.vmm.boot().await {
            instance.state = SandboxState::Stopped;
            instance.dump().await.ok();
            return Err(Error::Other(e));
        }
        let vmm_start_ms = t_boot.elapsed().as_millis() as u64;

        let vsock = instance.vmm.vsock_path().map_err(Error::Other)?;
        let pids = instance.vmm.pids();
        let setup_req = SandboxSetupRequest {
            interfaces: instance
                .network
                .as_ref()
                .map(|n| n.interfaces.clone())
                .unwrap_or_default(),
            routes: instance
                .network
                .as_ref()
                .map(|n| n.routes.clone())
                .unwrap_or_default(),
            sandbox_data: instance.data.clone(),
        };
        let exit_signal = instance.exit_signal.clone();
        drop(instance); // release lock during wait_ready

        // 4. Wait for guest readiness
        let wait_result = tokio::time::timeout(
            Duration::from_millis(self.config.ready_timeout_ms),
            self.runtime.wait_ready(id, &vsock),
        )
        .await;

        let _ready = match wait_result {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                let mut inst = instance_mutex.lock().await;
                inst.vmm.stop(true).await.ok();
                inst.state = SandboxState::Stopped;
                inst.dump().await.ok();
                return Err(Error::Other(e));
            }
            Err(_) => {
                let mut inst = instance_mutex.lock().await;
                inst.vmm.stop(true).await.ok();
                inst.state = SandboxState::Stopped;
                inst.dump().await.ok();
                return Err(Error::Timeout("ready_timeout".into()));
            }
        };

        // 5. Send network + pod config to guest
        if let Err(e) = self.runtime.setup_sandbox(id, &setup_req).await {
            tracing::warn!(sandbox_id = %id, err = %e, "setup_sandbox failed");
            let mut inst = instance_mutex.lock().await;
            inst.vmm.stop(true).await.ok();
            inst.state = SandboxState::Stopped;
            inst.dump().await.ok();
            return Err(Error::Other(e));
        }

        // 6. State transition, post_start hook, persist
        {
            let mut inst = instance_mutex.lock().await;
            if inst.state != SandboxState::Creating {
                return Err(Error::InvalidState(format!(
                    "sandbox {} state changed to {:?} during boot",
                    id, inst.state
                )));
            }
            inst.state = SandboxState::Running;

            {
                let mut ctx = inst.make_ctx();
                self.hooks.post_start(&mut ctx).await.ok();
            }

            // Add VMM process to host cgroup (cgroup-v1 only)
            if !cgroups_rs::hierarchies::is_cgroup2_unified_mode() {
                let vcpu_threads = inst
                    .vmm
                    .vcpus()
                    .await
                    .ok()
                    .map(|vt| vmm_common::cgroup::VcpuThreads { vcpus: vt.vcpus });
                inst.cgroup
                    .add_process_into_sandbox_cgroups(pids.vmm_pid.unwrap_or(0), vcpu_threads)
                    .ok();
                for pid in &pids.affiliated_pids {
                    inst.cgroup
                        .add_process_into_sandbox_cgroups(*pid, None)
                        .ok();
                }
            }
            inst.dump().await?;
        }

        // 7. Monitor VMM exit (fires exit_signal on unexpected exit → Stopped)
        let exit_rx = instance_mutex.lock().await.vmm.subscribe_exit();
        self.monitor_vmm_exit(id, instance_mutex.clone(), exit_rx);

        // 8. Forward OOM/exit events from vmm-task to containerd
        self.runtime.forward_events(id, exit_signal).await;

        Ok(StartResult {
            ready_ms: t0.elapsed().as_millis() as u64,
            vmm_start_ms,
        })
    }

    pub async fn stop_sandbox(&self, id: &str, force: bool) -> Result<()>
    where
        V: Serialize + DeserializeOwned + Default,
    {
        let instance_mutex = self.get_sandbox(id).await?;
        let mut instance = instance_mutex.lock().await;

        // Idempotent: containerd may call StopSandbox multiple times.
        if instance.state == SandboxState::Stopped {
            return Ok(());
        }

        let new_state = instance.state.transition(StateEvent::Stop)?;

        // Force-remove all containers before stopping the VM
        let container_ids: Vec<String> = instance.containers.keys().cloned().collect();
        for cid in container_ids {
            if force {
                if let Some(c) = instance.containers.remove(&cid) {
                    for dev_id in c.io_devices {
                        instance.vmm.hot_detach(&dev_id).await.ok();
                    }
                }
            }
            // graceful: caller should have sent signals before calling stop
        }

        // pre_stop hook (only on graceful stop)
        if !force {
            let mut ctx = instance.make_ctx();
            self.hooks.pre_stop(&mut ctx).await.ok();
        }

        instance.vmm.stop(force).await.map_err(Error::Other)?;

        // Signal background tasks spawned by forward_events to exit.
        // monitor_vmm_exit only fires this on unexpected exit; for graceful
        // stop we must fire it here so the clock-sync and event-forward tasks
        // are cancelled via their tokio::select! branches.
        instance.exit_signal.signal();

        // post_stop hook
        {
            let mut ctx = instance.make_ctx();
            self.hooks.post_stop(&mut ctx).await.ok();
        }

        // Destroy network (take() prevents double-destroy on recovery)
        if let Some(net) = instance.network.take() {
            vmm_common::network::destroy_tap_devices(&net.tap_names).await;
            vmm_common::network::restore_physical_devices(&net.physical_nics).await;
        }

        instance.state = new_state;
        instance.dump().await?;
        drop(instance);
        self.runtime.cleanup_sandbox(id).await;
        Ok(())
    }

    pub async fn delete_sandbox(&self, id: &str, force: bool) -> Result<()>
    where
        V: Serialize + DeserializeOwned + Default,
    {
        let instance_mutex = self.get_sandbox(id).await?;
        {
            let mut instance = instance_mutex.lock().await;
            let event = if force {
                StateEvent::ForceDelete
            } else {
                StateEvent::Delete
            };
            let new_state = instance.state.transition(event)?;

            if force {
                instance.vmm.stop(true).await.ok();
            }
            instance.state = new_state;
            if !cgroups_rs::hierarchies::is_cgroup2_unified_mode() {
                instance.cgroup.remove_sandbox_cgroups().ok();
            }
            containerd_sandbox::utils::cleanup_mounts(&instance.base_dir)
                .await
                .ok();
            tokio::fs::remove_dir_all(&instance.base_dir).await.ok();
        }
        self.sandboxes.write().await.remove(id);
        self.runtime.cleanup_sandbox(id).await;
        Ok(())
    }

    pub async fn get_sandbox(&self, id: &str) -> Result<Arc<Mutex<SandboxInstance<V>>>> {
        self.sandboxes
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| Error::NotFound(id.to_string()))
    }

    /// Synchronous variant used by non-async contexts (e.g. `Sandbox::status()`).
    /// Uses `try_read()` to avoid blocking an async executor thread.
    pub fn get_sandbox_sync(&self, id: &str) -> Result<Arc<Mutex<SandboxInstance<V>>>> {
        self.sandboxes
            .try_read()
            .map_err(|_| Error::Other(anyhow::anyhow!("sandboxes lock contended")))?
            .get(id)
            .cloned()
            .ok_or_else(|| Error::NotFound(id.to_string()))
    }

    pub async fn list_sandboxes(&self) -> Vec<SandboxSummary> {
        // Collect Arc references first, then release the RwLock before locking each
        // sandbox mutex. Holding the RwLock across blocking_lock() risks blocking
        // the entire read lock while a sandbox operation holds the inner mutex.
        let instances: Vec<_> = self.sandboxes.read().await.values().cloned().collect();
        let mut summaries = Vec::with_capacity(instances.len());
        for m in instances {
            let g = m.lock().await;
            summaries.push(SandboxSummary {
                id: g.id.clone(),
                state: g.state.clone(),
            });
        }
        summaries
    }

    /// Re-attach engine state from the work directory after process restart.
    pub async fn recover(&self, work_dir: &str)
    where
        V: Serialize + DeserializeOwned + Default,
    {
        let mut dir = match tokio::fs::read_dir(work_dir).await {
            Ok(d) => d,
            Err(e) => {
                tracing::error!("recovery: cannot read {}: {}", work_dir, e);
                return;
            }
        };
        while let Some(entry) = dir.next_entry().await.unwrap_or(None) {
            if !entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let path = entry.path();
            match SandboxInstance::<V>::load(&path).await {
                Ok(mut inst) => {
                    if inst.state == SandboxState::Running {
                        if let Err(e) = inst.vmm.recover().await {
                            tracing::warn!("recovery: vmm reconnect failed for {}: {}", inst.id, e);
                            inst.state = SandboxState::Stopped;
                        } else {
                            let vsock = inst.vmm.vsock_path().unwrap_or_default();
                            let exit_signal = inst.exit_signal.clone();
                            let wait_result = tokio::time::timeout(
                                Duration::from_millis(self.config.ready_timeout_ms),
                                self.runtime.wait_ready(&inst.id, &vsock),
                            )
                            .await;
                            let ready = match wait_result {
                                Ok(Ok(_)) => true,
                                Ok(Err(e)) => {
                                    tracing::warn!(
                                        "recovery: wait_ready failed for {}: {}",
                                        inst.id,
                                        e
                                    );
                                    false
                                }
                                Err(_) => {
                                    tracing::warn!("recovery: wait_ready timeout for {}", inst.id);
                                    false
                                }
                            };
                            if !ready {
                                inst.vmm.stop(true).await.ok();
                                inst.state = SandboxState::Stopped;
                                // Release any ttrpc client that wait_ready may have cached.
                                self.runtime.cleanup_sandbox(&inst.id).await;
                                // fall through to insert as Stopped
                            } else {
                                self.runtime.forward_events(&inst.id, exit_signal).await;
                                inst.cgroup = SandboxCgroup::create_sandbox_cgroups(
                                    &inst.cgroup.cgroup_parent_path,
                                    &inst.id,
                                )
                                .unwrap_or_default();
                                let exit_rx = inst.vmm.subscribe_exit();
                                let id = inst.id.clone();
                                let inst_arc = Arc::new(Mutex::new(inst));
                                self.monitor_vmm_exit(&id, inst_arc.clone(), exit_rx);
                                self.sandboxes.write().await.insert(id, inst_arc);
                                continue;
                            }
                        }
                    }
                    self.sandboxes
                        .write()
                        .await
                        .insert(inst.id.clone(), Arc::new(Mutex::new(inst)));
                }
                Err(e) => tracing::warn!("recovery: skip {:?}: {}", path, e),
            }
        }
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    /// Spawn a background task that waits on the VMM exit receiver.
    /// When the VMM exits unexpectedly (state still Running), transitions the sandbox
    /// to Stopped, fires exit_signal, and removes it from the map.
    ///
    /// Graceful exits (the VMM process exits after `stop_sandbox` already set the state
    /// to Stopped) are ignored here — the sandbox stays in the map so that the subsequent
    /// `delete_sandbox` call can find and clean it up normally.
    fn monitor_vmm_exit(
        &self,
        id: &str,
        instance_mutex: Arc<Mutex<SandboxInstance<V>>>,
        mut exit_rx: tokio::sync::watch::Receiver<Option<ExitInfo>>,
    ) {
        let id = id.to_string();
        let sandboxes = self.sandboxes.clone();
        tokio::spawn(async move {
            loop {
                if exit_rx.changed().await.is_err() {
                    break;
                }
                if exit_rx.borrow().is_some() {
                    break;
                }
            }
            let mut inst = instance_mutex.lock().await;
            if inst.state == SandboxState::Running {
                tracing::warn!("sandbox {} VMM exited unexpectedly; marking Stopped", id);
                inst.state = SandboxState::Stopped;
                inst.exit_signal.signal();
                // Release the mutex before acquiring the write lock to avoid
                // potential lock-order deadlocks.
                drop(inst);
                sandboxes.write().await.remove(&id);
            }
            // Graceful exit: state is already Stopped (set by stop_sandbox).
            // Leave the sandbox in the map; delete_sandbox will remove it.
        });
    }
}

// ── Free functions ────────────────────────────────────────────────────────────

/// Write hostname, /etc/hosts, and resolv.conf into the virtiofs shared directory.
async fn setup_sandbox_files(base_dir: &str, data: &SandboxData) -> Result<()> {
    let shared = format!("{}/{}", base_dir, SHARED_DIR_SUFFIX);
    tokio::fs::create_dir_all(&shared)
        .await
        .map_err(|e| Error::Other(anyhow::anyhow!("create shared dir: {}", e)))?;

    // hostname
    let mut host = get_hostname(data);
    if host.is_empty() {
        host = hostname::get()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
    }
    host.push('\n');
    write_str_to_file(format!("{}/{}", shared, HOSTNAME_FILENAME), &host).await?;

    // /etc/hosts
    tokio::fs::copy(ETC_HOSTS, format!("{}/{}", shared, HOSTS_FILENAME))
        .await
        .map_err(|e| Error::Other(anyhow::anyhow!("copy hosts: {}", e)))?;

    // resolv.conf
    match get_dns_config(data) {
        Some(dns) if !dns.servers.is_empty() || !dns.searches.is_empty() => {
            let content = format_resolv_conf(dns);
            write_str_to_file(format!("{}/{}", shared, RESOLV_FILENAME), &content).await?;
        }
        _ => {
            tokio::fs::copy(ETC_RESOLV, format!("{}/{}", shared, RESOLV_FILENAME))
                .await
                .map_err(|e| Error::Other(anyhow::anyhow!("copy resolv.conf: {}", e)))?;
        }
    }
    Ok(())
}

async fn write_str_to_file(path: String, content: &str) -> Result<()> {
    tokio::fs::write(&path, content)
        .await
        .map_err(|e| Error::Other(anyhow::anyhow!("write {}: {}", path, e)))
}

fn get_hostname(data: &SandboxData) -> String {
    data.config
        .as_ref()
        .map(|c| c.hostname.clone())
        .unwrap_or_default()
}

fn get_dns_config(data: &SandboxData) -> Option<&containerd_sandbox::cri::api::v1::DnsConfig> {
    data.config.as_ref().and_then(|c| c.dns_config.as_ref())
}

fn format_resolv_conf(dns: &containerd_sandbox::cri::api::v1::DnsConfig) -> String {
    let mut s = String::new();
    if !dns.searches.is_empty() {
        s.push_str(&format!("search {}\n", dns.searches.join(" ")));
    }
    if !dns.servers.is_empty() {
        s.push_str(&format!(
            "nameserver {}\n",
            dns.servers.join("\nnameserver ")
        ));
    }
    if !dns.options.is_empty() {
        s.push_str(&format!("options {}\n", dns.options.join(" ")));
    }
    s
}

fn get_resources(
    data: &SandboxData,
) -> Option<&containerd_sandbox::cri::api::v1::LinuxContainerResources> {
    data.config
        .as_ref()
        .and_then(|c| c.linux.as_ref())
        .and_then(|l| l.resources.as_ref())
}

/// Derive the vCPU count from pod resource limits.
fn vcpu_count_from_resources(data: &SandboxData) -> u32 {
    if let Some(res) = get_resources(data) {
        if res.cpu_period > 0 && res.cpu_quota > 0 {
            return (res.cpu_quota as f64 / res.cpu_period as f64).ceil() as u32;
        }
    }
    1
}

/// Build a `VmmNetworkConfig` from a `DiscoveredInterface` based on its `link_type`.
///
/// - [`LinkType::Tap`] → `VmmNetworkConfig::Tap`
/// - [`LinkType::Physical`] → `VmmNetworkConfig::Physical`
/// - [`LinkType::VhostUser`] → `VmmNetworkConfig::VhostUser`
fn build_network_config(
    iface: &vmm_common::network::DiscoveredInterface,
    queue: u32,
    netns: &str,
) -> VmmNetworkConfig {
    match &iface.link_type {
        vmm_common::network::LinkType::Tap => VmmNetworkConfig::Tap {
            tap_device: iface.name.clone(),
            mac: iface.mac.clone(),
            queue,
            netns: netns.to_string(),
        },
        vmm_common::network::LinkType::Physical(bdf, _) => VmmNetworkConfig::Physical {
            id: format!("intf-{}", iface.veth_index),
            bdf: bdf.clone(),
        },
        vmm_common::network::LinkType::VhostUser(socket) => VmmNetworkConfig::VhostUser {
            id: format!("intf-{}", iface.veth_index),
            socket_path: socket.clone(),
            mac: iface.mac.clone(),
        },
        _ => VmmNetworkConfig::Tap {
            tap_device: iface.name.clone(),
            mac: iface.mac.clone(),
            queue,
            netns: netns.to_string(),
        },
    }
}
