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

//! StratoVirt VMM backend for `vmm-engine`.
//!
//! Wraps `vmm_sandboxer::stratovirt::StratoVirtVM` to implement the `Vmm` trait.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use vmm_sandboxer::{
    device::{BlockDeviceInfo, DeviceInfo},
    stratovirt::{
        config::{StratoVirtVMConfig, VirtiofsdConfig},
        devices::{
            block::{VirtioBlockDevice, VIRTIO_BLK_DRIVER},
            virtio_net::VirtioNetDevice,
            DEFAULT_PCIE_BUS,
        },
        StratoVirtVM,
    },
    vm::{HypervisorCommonConfig, Recoverable, VM},
};
use vmm_vm_trait::{
    open_tap_fds, DiskConfig, ExitInfo, Hooks, HotPlugDevice, HotPlugResult, Pids, SandboxCtx,
    VcpuThreads, Vmm, VmmCapabilities, VmmNetworkConfig,
};

// ── Config ────────────────────────────────────────────────────────────────────

/// Simple StratoVirt VMM configuration for the engine layer.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StratoVirtVmmConfig {
    pub binary: String,
    pub vcpus: u32,
    pub memory_mb: u64,
    pub kernel_path: String,
    pub image_path: String,
    #[serde(default = "default_virtiofsd_path")]
    pub virtiofsd_path: String,
    #[serde(default = "default_machine_type")]
    pub machine_type: String,
    #[serde(default = "default_block_driver")]
    pub block_device_driver: String,
}

fn default_virtiofsd_path() -> String {
    "/usr/bin/virtiofsd".to_string()
}

fn default_machine_type() -> String {
    "virt".to_string()
}

fn default_block_driver() -> String {
    "virtio-blk".to_string()
}

// ── StratoVirtVmm ─────────────────────────────────────────────────────────────

/// StratoVirt VMM instance implementing the `Vmm` trait.
///
/// Wraps `StratoVirtVM` from `vmm-sandboxer` to reuse existing StratoVirt backend logic.
#[derive(Serialize)]
pub struct StratoVirtVmm {
    pub id: String,
    pub base_dir: String,
    /// Reflects the actual vsock CID allocated by the kernel (set during boot).
    pub vsock_cid: u32,
    inner: StratoVirtVM,
    #[serde(skip)]
    #[allow(dead_code)]
    exit_tx: Arc<tokio::sync::watch::Sender<Option<ExitInfo>>>,
    #[serde(skip)]
    exit_rx: tokio::sync::watch::Receiver<Option<ExitInfo>>,
}

impl StratoVirtVmm {
    fn from_parts(id: String, base_dir: String, vsock_cid: u32, inner: StratoVirtVM) -> Self {
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

impl Default for StratoVirtVmm {
    fn default() -> Self {
        Self::from_parts(String::new(), String::new(), 0, StratoVirtVM::default())
    }
}

impl<'de> Deserialize<'de> for StratoVirtVmm {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Helper {
            id: String,
            base_dir: String,
            vsock_cid: u32,
            inner: StratoVirtVM,
        }
        let h = Helper::deserialize(d)?;
        let (tx, rx) = tokio::sync::watch::channel(None);
        Ok(StratoVirtVmm {
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
impl Vmm for StratoVirtVmm {
    type Config = StratoVirtVmmConfig;

    async fn create(
        id: &str,
        base_dir: &str,
        config: &StratoVirtVmmConfig,
        vsock_cid: u32,
    ) -> anyhow::Result<Self> {
        use uuid::Uuid;
        use vmm_common::SHARED_DIR_SUFFIX;
        use vmm_sandboxer::{
            stratovirt::{
                config::QmpSocket,
                devices::{
                    char::CharDevice,
                    console::VirtConsole,
                    create_pcie_root_bus,
                    rng::VirtioRngDevice,
                    serial::SerialDevice,
                    vhost_user_fs::{VhostUserFs, DEFAULT_MOUNT_TAG_NAME},
                    vsock::{find_context_id, VSockDevice},
                    DEFAULT_CONSOLE_CHARDEV_ID, DEFAULT_CONSOLE_DEVICE_ID, DEFAULT_PCIE_BUS,
                    DEFAULT_RNG_DEVICE_ID, DEFAULT_SERIAL_DEVICE_ID, PCIE_ROOTPORT_CAPACITY,
                },
            },
            vm::BlockDriver,
        };

        let vm_config = StratoVirtVMConfig {
            path: config.binary.clone(),
            machine_type: config.machine_type.clone(),
            block_device_driver: config.block_device_driver.clone(),
            common: HypervisorCommonConfig {
                vcpus: config.vcpus,
                memory_in_mb: config.memory_mb as u32,
                kernel_path: config.kernel_path.clone(),
                image_path: config.image_path.clone(),
                ..Default::default()
            },
            virtiofsd_conf: VirtiofsdConfig {
                path: config.virtiofsd_path.clone(),
            },
        };

        let mut inner = StratoVirtVM::new(id, "", base_dir);

        // Build low-level StratoVirtConfig from vm_config
        inner.config = vm_config
            .to_stratovirt_config()
            .await
            .map_err(|e| anyhow::anyhow!("to_stratovirt_config: {}", e))?;
        inner.config.uuid = Uuid::new_v4().to_string();
        inner.config.name = format!("sandbox-{}", id);
        inner.config.pid_file = format!("{}/sandbox-{}.pid", base_dir, id);
        inner.block_driver = BlockDriver::from(&config.block_device_driver);
        if vm_config.common.debug {
            inner.config.log_file = Some(format!("{}/sandbox-{}.log", base_dir, id));
        }

        // QMP socket
        inner.config.qmp_socket = Some(QmpSocket {
            param_key: "qmp".to_string(),
            r#type: "unix".to_string(),
            name: format!("/run/{}-qmp.sock", id),
            server: true,
            no_wait: true,
        });

        let machine_type = inner.config.machine.r#type.clone();
        let machine_array: Vec<_> = machine_type.split(',').collect();
        if machine_array[0] != "microvm" {
            inner.pcie_root_bus = create_pcie_root_bus();
        }

        let transport = inner.config.machine.transport();

        // RNG
        inner
            .attach_to_bus(VirtioRngDevice::new(
                DEFAULT_RNG_DEVICE_ID,
                "/dev/urandom",
                transport.clone(),
                DEFAULT_PCIE_BUS,
            ))
            .map_err(|e| anyhow::anyhow!("attach rng: {}", e))?;

        // Serial + console
        inner
            .attach_to_bus(SerialDevice::new(
                DEFAULT_SERIAL_DEVICE_ID,
                transport.clone(),
                DEFAULT_PCIE_BUS,
            ))
            .map_err(|e| anyhow::anyhow!("attach serial: {}", e))?;
        let console_socket = inner.console_socket.clone();
        inner.attach_device(CharDevice::new(
            "socket",
            DEFAULT_CONSOLE_CHARDEV_ID,
            &console_socket,
        ));
        inner.attach_device(VirtConsole::new(
            DEFAULT_CONSOLE_DEVICE_ID,
            DEFAULT_CONSOLE_CHARDEV_ID,
        ));

        // Image block device
        if inner.config.kernel.image.is_some() {
            let mut image_device = VirtioBlockDevice::new(
                &transport.clone().to_driver(VIRTIO_BLK_DRIVER),
                "rootfs",
                "blk-0",
                inner.config.kernel.image.clone(),
                Some(true),
            );
            image_device.bus = Some(DEFAULT_PCIE_BUS.to_string());
            inner
                .attach_to_bus(image_device)
                .map_err(|e| anyhow::anyhow!("attach image disk: {}", e))?;
        }

        // Vsock — allocate a real CID from the kernel
        let (fd, cid) = find_context_id()
            .await
            .map_err(|e| anyhow::anyhow!("find_context_id: {}", e))?;
        let fd_index = inner.append_fd(fd);
        inner
            .attach_to_bus(VSockDevice::new(
                cid,
                transport.clone(),
                DEFAULT_PCIE_BUS,
                fd_index as i32,
            ))
            .map_err(|e| anyhow::anyhow!("attach vsock: {}", e))?;
        inner.agent_socket = format!("vsock://{}:1024", cid);

        // Share FS (virtiofs only for stratovirt)
        let share_fs_path = format!("{}/{}", base_dir, SHARED_DIR_SUFFIX);
        tokio::fs::create_dir_all(&share_fs_path).await?;
        let virtiofs_sock = format!("{}/virtiofs.sock", base_dir);
        let chardev_id = format!("virtio-fs-{}", id);
        inner.attach_device(CharDevice::new("socket", &chardev_id, &virtiofs_sock));
        inner
            .attach_to_bus(VhostUserFs::new(
                &format!("vhost-user-fs-{}", id),
                transport.clone(),
                &chardev_id,
                DEFAULT_MOUNT_TAG_NAME,
                DEFAULT_PCIE_BUS,
            ))
            .map_err(|e| anyhow::anyhow!("attach virtiofs: {}", e))?;

        // Virtiofs daemon
        inner.create_vitiofs_daemon(&config.virtiofsd_path, base_dir, &share_fs_path);

        // PCIe root ports for hot-plug
        if machine_array[0] != "microvm" {
            inner
                .create_pcie_root_ports(PCIE_ROOTPORT_CAPACITY)
                .map_err(|e| anyhow::anyhow!("create_pcie_root_ports: {}", e))?;
        }

        let actual_cid = parse_vsock_cid(&inner.agent_socket).unwrap_or(vsock_cid);
        let (tx, rx) = tokio::sync::watch::channel(None);
        Ok(StratoVirtVmm {
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
            .map_err(|e| anyhow::anyhow!("stratovirt start: {}", e))?;

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
            .map_err(|e| anyhow::anyhow!("stratovirt stop: {}", e))
    }

    fn subscribe_exit(&self) -> tokio::sync::watch::Receiver<Option<ExitInfo>> {
        self.exit_rx.clone()
    }

    async fn recover(&mut self) -> anyhow::Result<()> {
        self.inner
            .recover()
            .await
            .map_err(|e| anyhow::anyhow!("stratovirt recover: {}", e))?;

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
        let transport = self.inner.config.machine.transport();
        let driver = transport.to_driver(VIRTIO_BLK_DRIVER);
        let mut device = VirtioBlockDevice::new(
            &driver,
            &disk.id,
            &disk.id,
            Some(disk.path),
            Some(disk.read_only),
        );
        device.bus = Some(DEFAULT_PCIE_BUS.to_string());
        self.inner
            .attach_to_bus(device)
            .map_err(|e| anyhow::anyhow!("add_disk: {}", e))
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
                let transport = self.inner.config.machine.transport();
                let device = VirtioNetDevice::new()
                    .id(&tap_device)
                    .name(&tap_device)
                    .mac_address(&mac)
                    .transport(transport)
                    .fds(fd_ints)
                    .vhost(false)
                    .vhostfds(vec![])
                    .bus(Some(DEFAULT_PCIE_BUS.to_string()))
                    .build();
                self.inner
                    .attach_to_bus(device)
                    .map_err(|e| anyhow::anyhow!("add_network tap: {}", e))?;
            }
            VmmNetworkConfig::VhostUser { .. } => {
                anyhow::bail!("stratovirt: vhost-user network device not supported");
            }
            VmmNetworkConfig::Physical { .. } => {
                anyhow::bail!("stratovirt: physical NIC (VFIO) passthrough not supported");
            }
        }
        Ok(())
    }

    async fn hot_attach(&mut self, device: HotPlugDevice) -> anyhow::Result<HotPlugResult> {
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
            other => anyhow::bail!("stratovirt: unsupported hot-plug device {:?}", other),
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
        Ok(format!("vsock://{}:1024", self.vsock_cid))
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
        let inner: StratoVirtVM = serde_json::from_value(vm_json)
            .map_err(|e| anyhow::anyhow!("deserialize StratoVirtVM: {}", e))?;
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

// ── StratoVirtHooks ───────────────────────────────────────────────────────────

#[derive(Default)]
pub struct StratoVirtHooks;

#[async_trait]
impl Hooks<StratoVirtVmm> for StratoVirtHooks {
    async fn post_start(&self, ctx: &mut SandboxCtx<'_, StratoVirtVmm>) -> anyhow::Result<()> {
        ctx.data.task_address = ctx.vmm.task_address();
        Ok(())
    }

    async fn pre_stop(&self, _ctx: &mut SandboxCtx<'_, StratoVirtVmm>) -> anyhow::Result<()> {
        Ok(())
    }

    async fn post_stop(&self, _ctx: &mut SandboxCtx<'_, StratoVirtVmm>) -> anyhow::Result<()> {
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
    fn stratovirt_vmm_implements_vmm() {
        _assert_vmm::<StratoVirtVmm>();
    }

    #[test]
    fn stratovirt_hooks_implements_hooks() {
        _assert_hooks::<StratoVirtVmm, StratoVirtHooks>();
    }

    #[tokio::test]
    async fn stratovirt_hooks_post_start_sets_task_address() {
        let cfg = StratoVirtVmmConfig {
            image_path: "/tmp/test.img".to_string(),
            ..Default::default()
        };
        let mut vmm = StratoVirtVmm::create("sv-1", "/tmp", &cfg, 8)
            .await
            .unwrap();
        let mut data = SandboxData::default();
        let hooks = StratoVirtHooks;
        let mut ctx = SandboxCtx {
            vmm: &mut vmm,
            data: &mut data,
            base_dir: "/tmp",
        };
        hooks.post_start(&mut ctx).await.unwrap();
        assert!(ctx.data.task_address.contains("vsock"));
    }
}
