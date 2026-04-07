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

//! Cloud Hypervisor VMM backend for `vmm-engine`.
//!
//! Wraps `vmm_sandboxer::cloud_hypervisor::CloudHypervisorVM` to implement
//! the `Vmm` trait defined in `vmm-vm-trait`.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use vmm_sandboxer::{
    cloud_hypervisor::{
        config::{CloudHypervisorVMConfig, VirtiofsdConfig},
        devices::{
            block::Disk, console::Console, fs::Fs, pmem::Pmem, rng::Rng,
            virtio_net::VirtioNetDevice, vsock::Vsock,
        },
        CloudHypervisorVM,
    },
    vm::{Recoverable, VM},
};
use vmm_vm_trait::{
    open_tap_fds, DiskConfig, ExitInfo, Hooks, HotPlugDevice, HotPlugResult, Pids, SandboxCtx,
    VcpuThreads, Vmm, VmmCapabilities, VmmNetworkConfig,
};

// ── Config ────────────────────────────────────────────────────────────────────

/// Configuration for the Cloud Hypervisor VMM backend.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CloudHypervisorVmmConfig {
    /// Path to the cloud-hypervisor binary.
    pub binary: String,
    /// Number of vCPUs.
    pub vcpus: u32,
    /// Memory in MiB.
    pub memory_mb: u64,
    /// Path to the guest kernel image.
    pub kernel_path: String,
    /// Path to the guest root image (pmem).
    pub image_path: String,
    /// Entropy source device (default: /dev/urandom).
    #[serde(default = "default_entropy_source")]
    pub entropy_source: String,
    /// Path to the virtiofsd binary.
    #[serde(default = "default_virtiofsd_path")]
    pub virtiofsd_path: String,
}

fn default_entropy_source() -> String {
    "/dev/urandom".to_string()
}

fn default_virtiofsd_path() -> String {
    "/usr/local/bin/virtiofsd".to_string()
}

// ── CloudHypervisorVmm ────────────────────────────────────────────────────────

/// Cloud Hypervisor VMM instance implementing the `Vmm` trait.
///
/// Wraps `CloudHypervisorVM` from `vmm-sandboxer` to reuse existing CH backend
/// logic (process spawning, API client, virtiofsd management, hot-plug).
#[derive(Serialize)]
pub struct CloudHypervisorVmm {
    pub id: String,
    pub base_dir: String,
    pub vsock_cid: u32,
    /// The wrapped inner VM (carries serialisable state for recovery).
    inner: CloudHypervisorVM,
    #[serde(skip)]
    #[allow(dead_code)]
    exit_tx: Arc<tokio::sync::watch::Sender<Option<ExitInfo>>>,
    #[serde(skip)]
    exit_rx: tokio::sync::watch::Receiver<Option<ExitInfo>>,
}

impl CloudHypervisorVmm {
    /// Build a `CloudHypervisorVmm` from an already-deserialized `CloudHypervisorVM`.
    /// Used by both `Default` and `from_legacy_vm`.
    fn from_inner(id: String, base_dir: String, inner: CloudHypervisorVM) -> Self {
        let (tx, rx) = tokio::sync::watch::channel(None);
        Self {
            id,
            base_dir,
            vsock_cid: 0,
            inner,
            exit_tx: Arc::new(tx),
            exit_rx: rx,
        }
    }
}

impl Default for CloudHypervisorVmm {
    fn default() -> Self {
        Self::from_inner(String::new(), String::new(), CloudHypervisorVM::default())
    }
}

impl<'de> Deserialize<'de> for CloudHypervisorVmm {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Helper {
            id: String,
            base_dir: String,
            vsock_cid: u32,
            inner: CloudHypervisorVM,
        }
        let h = Helper::deserialize(d)?;
        let (tx, rx) = tokio::sync::watch::channel(None);
        Ok(CloudHypervisorVmm {
            id: h.id,
            base_dir: h.base_dir,
            vsock_cid: h.vsock_cid,
            inner: h.inner,
            exit_tx: Arc::new(tx),
            exit_rx: rx,
        })
    }
}

#[async_trait]
impl Vmm for CloudHypervisorVmm {
    type Config = CloudHypervisorVmmConfig;

    async fn create(
        id: &str,
        base_dir: &str,
        config: &CloudHypervisorVmmConfig,
        vsock_cid: u32,
    ) -> anyhow::Result<Self> {
        use vmm_sandboxer::vm::HypervisorCommonConfig;

        let vm_config = CloudHypervisorVMConfig {
            path: config.binary.clone(),
            common: HypervisorCommonConfig {
                vcpus: config.vcpus,
                memory_in_mb: config.memory_mb as u32,
                kernel_path: config.kernel_path.clone(),
                image_path: config.image_path.clone(),
                ..Default::default()
            },
            entropy_source: config.entropy_source.clone(),
            virtiofsd: VirtiofsdConfig {
                path: config.virtiofsd_path.clone(),
                ..Default::default()
            },
            ..Default::default()
        };

        let mut inner = CloudHypervisorVM::new(id, "", base_dir, &vm_config);

        // Add rootfs pmem device
        if !config.image_path.is_empty() {
            inner.add_device(Pmem::new("rootfs", &config.image_path, true));
        }
        // Add entropy device
        if !config.entropy_source.is_empty() {
            inner.add_device(Rng::new("rng", &config.entropy_source));
        }
        // Add vsock (hvsock via unix socket file)
        let guest_socket_path = format!("{}/task.vsock", base_dir);
        inner.add_device(Vsock::new(3, &guest_socket_path, "vsock"));
        // Add console
        let console_path = format!("/tmp/{}-task.log", id);
        inner.add_device(Console::new(&console_path, "console"));
        // Add virtiofs share
        let virtiofs_sock = format!("{}/virtiofs.sock", base_dir);
        if !virtiofs_sock.is_empty() {
            inner.add_device(Fs::new("fs", &virtiofs_sock, "kuasar"));
        }

        let (tx, rx) = tokio::sync::watch::channel(None);
        Ok(CloudHypervisorVmm {
            id: id.to_string(),
            base_dir: base_dir.to_string(),
            vsock_cid,
            inner,
            exit_tx: Arc::new(tx),
            exit_rx: rx,
        })
    }

    async fn boot(&mut self) -> anyhow::Result<()> {
        let pid = self
            .inner
            .start()
            .await
            .map_err(|e| anyhow::anyhow!("cloud-hypervisor start: {}", e))?;

        // Bridge old (u32, i128) exit channel to new Option<ExitInfo> channel
        if let Some(old_rx) = self.inner.wait_channel().await {
            let new_tx = Arc::clone(&self.exit_tx);
            tokio::spawn(async move {
                let mut rx = old_rx;
                loop {
                    if rx.changed().await.is_err() {
                        break;
                    }
                    let (code, ts) = *rx.borrow();
                    if ts != 0 {
                        new_tx
                            .send(Some(ExitInfo {
                                pid,
                                exit_code: code as i32,
                            }))
                            .ok();
                        break;
                    }
                }
            });
        }
        Ok(())
    }

    async fn stop(&mut self, force: bool) -> anyhow::Result<()> {
        self.inner
            .stop(force)
            .await
            .map_err(|e| anyhow::anyhow!("cloud-hypervisor stop: {}", e))
    }

    fn subscribe_exit(&self) -> tokio::sync::watch::Receiver<Option<ExitInfo>> {
        self.exit_rx.clone()
    }

    async fn recover(&mut self) -> anyhow::Result<()> {
        self.inner
            .recover()
            .await
            .map_err(|e| anyhow::anyhow!("cloud-hypervisor recover: {}", e))?;

        if let Some(old_rx) = self.inner.wait_channel().await {
            let new_tx = Arc::clone(&self.exit_tx);
            let pid = self.inner.pids().vmm_pid.unwrap_or(0);
            tokio::spawn(async move {
                let mut rx = old_rx;
                loop {
                    if rx.changed().await.is_err() {
                        break;
                    }
                    let (code, ts) = *rx.borrow();
                    if ts != 0 {
                        new_tx
                            .send(Some(ExitInfo {
                                pid,
                                exit_code: code as i32,
                            }))
                            .ok();
                        break;
                    }
                }
            });
        }
        Ok(())
    }

    fn add_disk(&mut self, disk: DiskConfig) -> anyhow::Result<()> {
        self.inner
            .add_device(Disk::new(&disk.id, &disk.path, disk.read_only, true));
        Ok(())
    }

    fn add_network(&mut self, net: VmmNetworkConfig) -> anyhow::Result<()> {
        match net {
            VmmNetworkConfig::Tap {
                tap_device,
                mac,
                queue,
                netns,
            } => {
                if !netns.is_empty() {
                    self.inner.set_netns(&netns);
                }
                let fds = open_tap_fds(&tap_device, queue)?;
                let mut fd_ints = Vec::with_capacity(fds.len());
                for fd in fds {
                    let index = self.inner.append_fd(fd);
                    fd_ints.push(index as i32);
                }
                self.inner.add_device(VirtioNetDevice::new(
                    &tap_device,
                    Some(tap_device.clone()),
                    &mac,
                    fd_ints,
                ));
            }
            VmmNetworkConfig::Physical { id, bdf } => {
                use vmm_sandboxer::cloud_hypervisor::devices::vfio::VfioDevice;
                self.inner.add_device(VfioDevice::new(&id, &bdf));
            }
            VmmNetworkConfig::VhostUser { .. } => {
                anyhow::bail!("cloud-hypervisor: vhost-user network device not yet supported");
            }
        }
        Ok(())
    }

    async fn hot_attach(&mut self, device: HotPlugDevice) -> anyhow::Result<HotPlugResult> {
        use vmm_sandboxer::device::{BlockDeviceInfo, CharBackendType, CharDeviceInfo, DeviceInfo};
        let (device_id, device_info) = match device {
            HotPlugDevice::VirtioBlock {
                id,
                path,
                read_only,
            } => {
                let di = DeviceInfo::Block(BlockDeviceInfo {
                    id: id.clone(),
                    path,
                    read_only,
                });
                (id, di)
            }
            HotPlugDevice::CharDevice {
                id,
                chardev_id,
                name,
                path,
            } => {
                let di = DeviceInfo::Char(CharDeviceInfo {
                    id: id.clone(),
                    chardev_id,
                    name,
                    backend: CharBackendType::Pipe(path),
                });
                (id, di)
            }
            other => anyhow::bail!("cloud-hypervisor: unsupported hot-plug device {:?}", other),
        };
        let (_bus_type, bus_addr) = self
            .inner
            .hot_attach(device_info)
            .await
            .map_err(|e| anyhow::anyhow!("hot_attach: {}", e))?;
        Ok(HotPlugResult {
            device_id,
            bus_addr,
        })
    }

    async fn hot_detach(&mut self, id: &str) -> anyhow::Result<()> {
        self.inner
            .hot_detach(id)
            .await
            .map_err(|e| anyhow::anyhow!("hot_detach: {}", e))
    }

    async fn ping(&self) -> anyhow::Result<()> {
        self.inner
            .ping()
            .await
            .map_err(|e| anyhow::anyhow!("ping: {}", e))
    }

    async fn vcpus(&self) -> anyhow::Result<VcpuThreads> {
        let old = self
            .inner
            .vcpus()
            .await
            .map_err(|e| anyhow::anyhow!("vcpus: {}", e))?;
        Ok(VcpuThreads { vcpus: old.vcpus })
    }

    fn pids(&self) -> Pids {
        let old = self.inner.pids();
        Pids {
            vmm_pid: old.vmm_pid,
            affiliated_pids: old.affiliated_pids,
        }
    }

    fn vsock_path(&self) -> anyhow::Result<String> {
        Ok(format!("hvsock://{}/task.vsock:1024", self.base_dir))
    }

    fn capabilities(&self) -> VmmCapabilities {
        VmmCapabilities {
            hot_plug_disk: true,
            virtiofs: true,
            virtio_serial: true,
            ..Default::default()
        }
    }

    fn from_legacy_vm(
        vm_json: serde_json::Value,
        id: &str,
        base_dir: &str,
    ) -> anyhow::Result<Self>
    where
        Self: Sized,
    {
        let inner: CloudHypervisorVM = serde_json::from_value(vm_json)
            .map_err(|e| anyhow::anyhow!("deserialize CloudHypervisorVM: {}", e))?;
        Ok(Self::from_inner(id.to_string(), base_dir.to_string(), inner))
    }
}

// ── CloudHypervisorHooks ──────────────────────────────────────────────────────

/// Lifecycle hooks for the Cloud Hypervisor backend.
#[derive(Default)]
pub struct CloudHypervisorHooks;

#[async_trait]
impl Hooks<CloudHypervisorVmm> for CloudHypervisorHooks {
    async fn post_start(&self, ctx: &mut SandboxCtx<'_, CloudHypervisorVmm>) -> anyhow::Result<()> {
        ctx.data.task_address = ctx.vmm.task_address();
        Ok(())
    }

    async fn pre_stop(&self, _ctx: &mut SandboxCtx<'_, CloudHypervisorVmm>) -> anyhow::Result<()> {
        Ok(())
    }

    async fn post_stop(&self, ctx: &mut SandboxCtx<'_, CloudHypervisorVmm>) -> anyhow::Result<()> {
        for filename in &["api.sock", "task.vsock"] {
            let path = format!("{}/{}", ctx.base_dir, filename);
            tokio::fs::remove_file(&path).await.ok();
        }
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use containerd_sandbox::data::SandboxData;

    fn _assert_vmm<T: Vmm>() {}
    fn _assert_hooks<V: Vmm, H: Hooks<V>>() {}

    #[test]
    fn cloud_hypervisor_vmm_implements_vmm() {
        _assert_vmm::<CloudHypervisorVmm>();
    }

    #[test]
    fn cloud_hypervisor_hooks_implements_hooks() {
        _assert_hooks::<CloudHypervisorVmm, CloudHypervisorHooks>();
    }

    #[tokio::test]
    async fn cloud_hypervisor_vmm_create() {
        let cfg = CloudHypervisorVmmConfig::default();
        let vmm = CloudHypervisorVmm::create("test-vm", "/tmp/vmm-test", &cfg, 3).await;
        assert!(vmm.is_ok());
        let vmm = vmm.unwrap();
        assert_eq!(vmm.id, "test-vm");
    }

    #[tokio::test]
    async fn cloud_hypervisor_hooks_post_start_sets_task_address() {
        let cfg = CloudHypervisorVmmConfig::default();
        let mut vmm = CloudHypervisorVmm::create("ch-1", "/tmp/ch-test", &cfg, 5)
            .await
            .unwrap();
        let mut data = SandboxData::default();
        let hooks = CloudHypervisorHooks;
        let mut ctx = SandboxCtx {
            vmm: &mut vmm,
            data: &mut data,
            base_dir: "/tmp/ch-test",
        };
        hooks.post_start(&mut ctx).await.unwrap();
        assert!(ctx.data.task_address.contains("task.vsock"));
    }

    #[tokio::test]
    async fn cloud_hypervisor_hooks_post_stop_removes_sockets() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_str().unwrap();
        let api_sock = format!("{}/api.sock", dir);
        let task_vsock = format!("{}/task.vsock", dir);
        tokio::fs::write(&api_sock, b"").await.unwrap();
        tokio::fs::write(&task_vsock, b"").await.unwrap();

        let cfg = CloudHypervisorVmmConfig::default();
        let mut vmm = CloudHypervisorVmm::create("ch-2", dir, &cfg, 6)
            .await
            .unwrap();
        let mut data = SandboxData::default();
        let hooks = CloudHypervisorHooks;
        let mut ctx = SandboxCtx {
            vmm: &mut vmm,
            data: &mut data,
            base_dir: dir,
        };
        hooks.post_stop(&mut ctx).await.unwrap();

        assert!(
            !std::path::Path::new(&api_sock).exists(),
            "api.sock should be removed"
        );
        assert!(
            !std::path::Path::new(&task_vsock).exists(),
            "task.vsock should be removed"
        );
    }
}
