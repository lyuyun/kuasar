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

//! QEMU VMM backend for `vmm-engine`.
//!
//! Wraps `vmm_sandboxer::qemu::QemuVM` to implement the `Vmm` trait.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use vmm_sandboxer::{
    device::Transport,
    qemu::{
        config::QemuVMConfig,
        devices::{
            block::{VirtioBlockDevice, VIRTIO_BLK_DRIVER},
            vfio::VfioDevice,
            vhost_user::{VhostNetDevice, VhostUserType},
            virtio_net::VirtioNetDevice,
        },
        QemuVM,
    },
    vm::{HypervisorCommonConfig, Recoverable, VM},
};
use vmm_vm_trait::{
    open_tap_fds, DiskConfig, ExitInfo, Hooks, HotPlugDevice, HotPlugResult, Pids, SandboxCtx,
    VcpuThreads, Vmm, VmmCapabilities, VmmNetworkConfig,
};

// ── Config ────────────────────────────────────────────────────────────────────

/// Simple QEMU VMM configuration for the engine layer.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct QemuVmmConfig {
    pub binary: String,
    pub vcpus: u32,
    pub memory_mb: u64,
    pub kernel_path: String,
    pub image_path: String,
    #[serde(default = "default_entropy_source")]
    pub entropy_source: String,
    /// Use vsock for guest RPC channel; false falls back to virtio-serial socket.
    #[serde(default = "default_use_vsock")]
    pub use_vsock: bool,
}

fn default_entropy_source() -> String {
    "/dev/urandom".to_string()
}

fn default_use_vsock() -> bool {
    true
}

// ── QemuVmm ──────────────────────────────────────────────────────────────────

/// QEMU VMM instance implementing the `Vmm` trait.
///
/// Wraps `QemuVM` from `vmm-sandboxer` to reuse existing QEMU backend logic.
#[derive(Serialize)]
pub struct QemuVmm {
    pub id: String,
    pub base_dir: String,
    /// Reflects the actual vsock CID allocated by the kernel (set during boot).
    pub vsock_cid: u32,
    inner: QemuVM,
    #[serde(skip)]
    #[allow(dead_code)]
    exit_tx: Arc<tokio::sync::watch::Sender<Option<ExitInfo>>>,
    #[serde(skip)]
    exit_rx: tokio::sync::watch::Receiver<Option<ExitInfo>>,
}

impl QemuVmm {
    fn from_parts(id: String, base_dir: String, vsock_cid: u32, inner: QemuVM) -> Self {
        let (tx, rx) = tokio::sync::watch::channel(None);
        Self {
            id,
            base_dir,
            vsock_cid,
            inner,
            exit_tx: Arc::new(tx),
            exit_rx: rx,
        }
    }
}

impl Default for QemuVmm {
    fn default() -> Self {
        Self::from_parts(String::new(), String::new(), 0, QemuVM::default())
    }
}

impl<'de> Deserialize<'de> for QemuVmm {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Helper {
            id: String,
            base_dir: String,
            vsock_cid: u32,
            inner: QemuVM,
        }
        let h = Helper::deserialize(d)?;
        let (tx, rx) = tokio::sync::watch::channel(None);
        Ok(QemuVmm {
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
impl Vmm for QemuVmm {
    type Config = QemuVmmConfig;

    async fn create(
        id: &str,
        base_dir: &str,
        config: &QemuVmmConfig,
        vsock_cid: u32,
    ) -> anyhow::Result<Self> {
        use uuid::Uuid;
        use vmm_common::SHARED_DIR_SUFFIX;
        use vmm_sandboxer::{
            qemu::{
                config::QmpSocket,
                devices::{
                    char::{CharDevice, VIRT_CONSOLE_DRIVER, VIRT_SERIAL_PORT_DRIVER},
                    create_bridges,
                    scsi::ScsiController,
                    serial::SerialBridge,
                    vhost_user::{VhostCharDevice, VhostUserType},
                    virtio_9p::Virtio9PDevice,
                    virtio_rng::VirtioRngDevice,
                    vsock::{find_context_id, VSockDevice},
                },
            },
            vm::{BlockDriver, ShareFsType},
        };

        let vm_config = QemuVMConfig {
            qemu_path: config.binary.clone(),
            entropy_source: config.entropy_source.clone(),
            use_vsock: config.use_vsock,
            common: HypervisorCommonConfig {
                vcpus: config.vcpus,
                memory_in_mb: config.memory_mb as u32,
                kernel_path: config.kernel_path.clone(),
                image_path: config.image_path.clone(),
                ..Default::default()
            },
            // Always use block device for image (nvdimm unimplemented)
            disable_nvdimm: true,
            ..QemuVMConfig::default()
        };

        let mut inner = QemuVM::new(id, "", base_dir);

        // Build low-level QemuConfig from vm_config
        inner.config = vm_config
            .to_qemu_config()
            .await
            .map_err(|e| anyhow::anyhow!("to_qemu_config: {}", e))?;
        inner.config.uuid = Uuid::new_v4().to_string();
        inner.config.name = format!("sandbox-{}", id);
        inner.config.pid_file = format!("{}/sandbox-{}.pid", base_dir, id);
        inner.block_driver = vm_config.block_device_driver.clone();

        // QMP socket
        inner.config.qmp_socket = Some(QmpSocket {
            param_key: if vm_config.machine_type == "microvm-pci" {
                "microvm-qmp".to_string()
            } else {
                "qmp".to_string()
            },
            r#type: "unix".to_string(),
            name: format!("/run/{}-qmp.sock", id),
            server: true,
            no_wait: true,
        });

        // PCI bridges
        if vm_config.default_bridges > 0 {
            for b in create_bridges(vm_config.default_bridges, &vm_config.machine_type) {
                inner.attach_device(b);
            }
        }

        // SCSI controller (if block driver is VirtioScsi)
        if let BlockDriver::VirtioScsi = vm_config.block_device_driver {
            inner.attach_device(ScsiController::new("scsi0", Transport::Pci));
        }

        // Serial + console
        inner.attach_device(SerialBridge::new("serial0", Transport::Pci));
        let console_socket = inner.console_socket.clone();
        inner.attach_device(CharDevice::new_socket(
            "console0",
            "charconsole0",
            &console_socket,
            VIRT_CONSOLE_DRIVER,
            None,
        ));

        // RNG
        if !vm_config.entropy_source.is_empty() {
            inner.attach_device(VirtioRngDevice::new(
                "rng0",
                &vm_config.entropy_source,
                Transport::Pci,
            ));
        }

        // Vsock or serial port as the RPC channel to the guest agent
        if vm_config.use_vsock {
            let (fd, cid) = find_context_id()
                .await
                .map_err(|e| anyhow::anyhow!("find_context_id: {}", e))?;
            let fd_index = inner.append_fd(fd);
            inner.attach_device(VSockDevice::new(cid, Some(fd_index as i32), Transport::Pci));
            inner.agent_socket = format!("vsock://{}:1024", cid);
        } else {
            let socket = format!("{}/agent.sock", base_dir);
            inner.attach_device(CharDevice::new_socket(
                "channel0",
                "charch0",
                &socket,
                VIRT_SERIAL_PORT_DRIVER,
                Some("agent.channel.0".to_string()),
            ));
            inner.agent_socket = socket;
        }

        // Share FS
        let share_fs_path = format!("{}/{}", base_dir, SHARED_DIR_SUFFIX);
        tokio::fs::create_dir_all(&share_fs_path).await?;
        match vm_config.share_fs {
            ShareFsType::Virtio9P => {
                let multidevs = if vm_config.virtio_9p_multidevs.is_empty() {
                    None
                } else {
                    Some(vm_config.virtio_9p_multidevs.clone())
                };
                inner.attach_device(Virtio9PDevice::new(
                    "extra-9p-kuasar",
                    &share_fs_path,
                    "kuasar",
                    vm_config.virtio_9p_direct_io,
                    multidevs,
                    Transport::Pci,
                ));
            }
            ShareFsType::VirtioFS => match vm_config.virtiofsd.clone() {
                Some(mut virtiofs) => {
                    virtiofs.socket_path = format!("{}/virtiofs.sock", base_dir);
                    virtiofs.shared_dir = format!("{}/{}", base_dir, SHARED_DIR_SUFFIX);
                    inner.virtiofsd_config = Some(virtiofs.clone());
                    if !virtiofs.socket_path.is_empty() {
                        inner.attach_device(VhostCharDevice::new(
                            "extra-fs-kuasar",
                            VhostUserType::VhostUserChar("vhost-user-fs-pci".to_string()),
                            &virtiofs.socket_path,
                            "",
                        ));
                    }
                }
                None => anyhow::bail!("virtiofs requires virtiofsd config"),
            },
        }

        // Image block device
        if !vm_config.common.image_path.is_empty() {
            if vm_config.disable_nvdimm {
                let mut image_device = VirtioBlockDevice::new(
                    &Transport::Pci.to_driver(VIRTIO_BLK_DRIVER),
                    "image1",
                    Some(vm_config.common.image_path.to_string()),
                    true,
                );
                image_device.format = Some("raw".to_string());
                image_device.aio = Some("threads".to_string());
                image_device.r#if = Some("none".to_string());
                inner.attach_device(image_device);
            } else {
                anyhow::bail!("nvdimm not implemented");
            }
        }

        let actual_cid = parse_vsock_cid(&inner.agent_socket).unwrap_or(vsock_cid);
        let (tx, rx) = tokio::sync::watch::channel(None);
        Ok(QemuVmm {
            id: id.to_string(),
            base_dir: base_dir.to_string(),
            vsock_cid: actual_cid,
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
            .map_err(|e| anyhow::anyhow!("qemu start: {}", e))?;

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
            .map_err(|e| anyhow::anyhow!("qemu stop: {}", e))
    }

    fn subscribe_exit(&self) -> tokio::sync::watch::Receiver<Option<ExitInfo>> {
        self.exit_rx.clone()
    }

    async fn recover(&mut self) -> anyhow::Result<()> {
        self.inner
            .recover()
            .await
            .map_err(|e| anyhow::anyhow!("qemu recover: {}", e))?;

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
        let device = VirtioBlockDevice::new(
            &Transport::Pci.to_driver(VIRTIO_BLK_DRIVER),
            &disk.id,
            Some(disk.path),
            disk.read_only,
        );
        self.inner.attach_device(device);
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
                let device = VirtioNetDevice::new(
                    &tap_device,
                    Some(tap_device.clone()),
                    &mac,
                    Transport::Pci,
                    fd_ints,
                    vec![],
                );
                self.inner.attach_device(device);
            }
            VmmNetworkConfig::VhostUser {
                id,
                socket_path,
                mac,
            } => {
                let device = VhostNetDevice::new(
                    &id,
                    VhostUserType::VhostUserNet("virtio-net-pci".to_string()),
                    &socket_path,
                    &mac,
                );
                self.inner.attach_device(device);
            }
            VmmNetworkConfig::Physical { id, bdf } => {
                let device = VfioDevice::new(&id, &bdf);
                self.inner.attach_device(device);
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
            other => anyhow::bail!("qemu: unsupported hot-plug device {:?}", other),
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
        Ok(self.inner.agent_socket.clone())
    }

    fn capabilities(&self) -> VmmCapabilities {
        VmmCapabilities {
            hot_plug_disk: true,
            virtiofs: true,
            virtio_serial: true,
            ..Default::default()
        }
    }

    fn from_legacy_vm(vm_json: serde_json::Value, id: &str, base_dir: &str) -> anyhow::Result<Self>
    where
        Self: Sized,
    {
        // Parse vsock CID from "vsock://CID:1024"; fall back to 0 if absent.
        let vsock_cid = vm_json["agent_socket"]
            .as_str()
            .and_then(parse_vsock_cid)
            .unwrap_or(0);
        let inner: QemuVM = serde_json::from_value(vm_json)
            .map_err(|e| anyhow::anyhow!("deserialize QemuVM: {}", e))?;
        Ok(Self::from_parts(
            id.to_string(),
            base_dir.to_string(),
            vsock_cid,
            inner,
        ))
    }
}

/// Parse the vsock CID from an `agent_socket` string of the form `"vsock://CID:port"`.
fn parse_vsock_cid(agent_socket: &str) -> Option<u32> {
    let rest = agent_socket.strip_prefix("vsock://")?;
    let cid_str = rest.split(':').next()?;
    cid_str.parse().ok()
}

// ── QemuHooks ─────────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct QemuHooks;

#[async_trait]
impl Hooks<QemuVmm> for QemuHooks {
    async fn post_start(&self, ctx: &mut SandboxCtx<'_, QemuVmm>) -> anyhow::Result<()> {
        ctx.data.task_address = ctx.vmm.task_address();
        Ok(())
    }

    async fn pre_stop(&self, _ctx: &mut SandboxCtx<'_, QemuVmm>) -> anyhow::Result<()> {
        Ok(())
    }

    async fn post_stop(&self, _ctx: &mut SandboxCtx<'_, QemuVmm>) -> anyhow::Result<()> {
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
    fn qemu_vmm_implements_vmm() {
        _assert_vmm::<QemuVmm>();
    }

    #[test]
    fn qemu_hooks_implements_hooks() {
        _assert_hooks::<QemuVmm, QemuHooks>();
    }

    #[tokio::test]
    async fn qemu_hooks_post_start_sets_task_address() {
        let cfg = QemuVmmConfig::default();
        let mut vmm = QemuVmm::create("qemu-1", "/tmp", &cfg, 7).await.unwrap();
        let mut data = SandboxData::default();
        let hooks = QemuHooks;
        let mut ctx = SandboxCtx {
            vmm: &mut vmm,
            data: &mut data,
            base_dir: "/tmp",
        };
        hooks.post_start(&mut ctx).await.unwrap();
        // Default config has use_vsock = false (Rust bool default), so the agent
        // socket is a Unix path. task_address() wraps it with "ttrpc+".
        assert!(
            ctx.data.task_address.starts_with("ttrpc+"),
            "expected ttrpc+ prefix, got: {}",
            ctx.data.task_address
        );
    }
}
