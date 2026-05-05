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

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::anyhow;
use async_trait::async_trait;
use containerd_sandbox::{
    error::{Error, Result},
    SandboxOption,
};
use serde::{Deserialize, Serialize};
use tokio::sync::watch::Receiver;

use crate::{
    device::{BusType, DeviceInfo},
    sandbox::KuasarSandbox,
};

const VIRTIO_FS: &str = "virtio-fs";
const VIRTIO_9P: &str = "virtio-9p";
pub const SHAREFS_VIRTIO_BLK: &str = "virtio-blk";

#[async_trait]
pub trait VMFactory {
    type VM: VM + Sync + Send;
    type Config: Sync + Send;
    fn new(config: Self::Config) -> Self;
    async fn create_vm(&self, id: &str, s: &SandboxOption) -> Result<Self::VM>;

    // Optional accessors used by the template pool to build TemplateKey and PooledTemplate.
    // Implementations that support templating should override these.
    fn image_path(&self) -> &str {
        ""
    }
    fn kernel_path(&self) -> &str {
        ""
    }
    fn vcpus(&self) -> u32 {
        1
    }
    fn memory_mb(&self) -> u32 {
        1024
    }
}

#[async_trait]
pub trait Hooks<V: VM + Sync + Send> {
    async fn post_create(&self, _sandbox: &mut KuasarSandbox<V>) -> Result<()> {
        Ok(())
    }
    async fn pre_start(&self, _sandbox: &mut KuasarSandbox<V>) -> Result<()> {
        Ok(())
    }
    async fn post_start(&self, _sandbox: &mut KuasarSandbox<V>) -> Result<()> {
        Ok(())
    }
    async fn pre_stop(&self, _sandbox: &mut KuasarSandbox<V>) -> Result<()> {
        Ok(())
    }
    async fn post_stop(&self, _sandbox: &mut KuasarSandbox<V>) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
pub trait VM: Serialize + Sync + Send {
    async fn start(&mut self) -> Result<u32>;
    async fn stop(&mut self, force: bool) -> Result<()>;
    async fn attach(&mut self, device_info: DeviceInfo) -> Result<()>;
    async fn hot_attach(&mut self, device_info: DeviceInfo) -> Result<(BusType, String)>;
    async fn hot_detach(&mut self, id: &str) -> Result<()>;
    async fn ping(&self) -> Result<()>;
    fn socket_address(&self) -> String;
    async fn wait_channel(&self) -> Option<Receiver<(u32, i128)>>;
    async fn vcpus(&self) -> Result<VcpuThreads>;
    fn pids(&self) -> Pids;
    fn sharefs_type(&self) -> &str {
        "virtiofs"
    }
}

#[macro_export]
macro_rules! impl_recoverable {
    ($ty:ty) => {
        #[async_trait]
        impl $crate::vm::Recoverable for $ty {
            async fn recover(&mut self) -> Result<()> {
                self.client = Some(self.create_client().await?);
                let pid = self.pid()?;
                let (tx, rx) = channel((0u32, 0i128));
                tokio::spawn(async move {
                    let wait_result = wait_pid(pid as i32).await;
                    tx.send(wait_result).unwrap_or_default();
                });
                self.wait_chan = Some(rx);
                Ok(())
            }
        }
    };
}

#[async_trait]
pub trait Recoverable {
    async fn recover(&mut self) -> Result<()>;
}

/// Sandbox-specific paths that must be updated when restoring a snapshot to a new sandbox.
/// Only per-sandbox sockets are patched; pmem/rootfs is shared read-only across sandboxes.
pub struct SnapshotPathOverrides {
    pub task_vsock: String,
    pub console_path: String,
}

/// One ext4 block device to capture during a full-checkpoint snapshot.
/// Passed by the caller (which has access to sandbox storages) to `Snapshottable::snapshot`.
pub struct DiskSnapshot {
    /// Storage ID, used to derive the `.img` filename on the host.
    pub storage_id: String,
    /// CH device ID matching the `id` field in `config.json`'s `disks` array.
    pub device_id: String,
    /// Absolute path to the `.img` backing file on the host.
    pub img_path: String,
}

/// Describes a disk image stored inside a snapshot directory, used both in
/// `SnapshotMeta` (what was captured) and `RestoreSource` (what to restore).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DiskImageEntry {
    pub storage_id: String,
    pub device_id: String,
    /// Path relative to the snapshot directory, e.g. `"disks/storage3.img"`.
    pub filename: String,
}

pub struct RestoreSource {
    pub snapshot_dir: PathBuf,
    pub work_dir: PathBuf,
    pub overrides: SnapshotPathOverrides,
    /// Skip the setup_sandbox RPC on restore — the guest was already fully initialized when
    /// the snapshot was taken (e.g. snapshot from a running sandbox, not a bare boot image).
    pub ns_preinitialized: bool,
    /// Disk images to restore into the new sandbox directory.
    /// Empty = template mode (disks stripped from config.json).
    /// Non-empty = full-checkpoint mode (disk files copied and paths remapped).
    pub disk_images: Vec<DiskImageEntry>,
}

#[derive(Debug, Default)]
pub struct SnapshotMeta {
    pub snapshot_dir: PathBuf,
    pub original_task_vsock: String,
    pub original_console_path: String,
    /// Disk images captured in this snapshot.
    /// Empty for bare-VM template snapshots; non-empty for full-checkpoint snapshots.
    pub disk_images: Vec<DiskImageEntry>,
}

#[async_trait]
pub trait Snapshottable {
    /// Capture VM state to `dest_dir`.  `disks` lists host-side ext4 images to copy
    /// while the VM is paused; pass an empty slice for bare-VM (template) snapshots.
    async fn snapshot(&mut self, dest_dir: &Path, disks: &[DiskSnapshot]) -> Result<SnapshotMeta> {
        let _ = (dest_dir, disks);
        Err(anyhow!("snapshot not supported for this VM type").into())
    }
    async fn restore(&mut self, src: &RestoreSource) -> Result<()> {
        let _ = src;
        Err(anyhow!("restore not supported for this VM type").into())
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct HypervisorCommonConfig {
    #[serde(default)]
    pub debug: bool,
    pub vcpus: u32,
    pub memory_in_mb: u32,
    #[serde(default)]
    pub kernel_path: String,
    #[serde(default)]
    pub image_path: String,
    #[serde(default)]
    pub initrd_path: String,
    #[serde(default)]
    pub kernel_params: String,
    #[serde(default)]
    pub firmware: String,
    #[serde(default)]
    pub enable_mem_prealloc: bool,
}

impl Default for HypervisorCommonConfig {
    fn default() -> Self {
        Self {
            debug: false,
            vcpus: 1,
            memory_in_mb: 1024,
            kernel_path: "/var/lib/kuasar/vmlinux.bin".to_string(),
            image_path: "".to_string(),
            initrd_path: "".to_string(),
            kernel_params: "".to_string(),
            firmware: "".to_string(),
            enable_mem_prealloc: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[allow(clippy::enum_variant_names)]
pub enum BlockDriver {
    VirtioBlk,
    VirtioScsi,
    VirtioMmio,
}

impl Default for BlockDriver {
    fn default() -> Self {
        Self::VirtioBlk
    }
}

impl BlockDriver {
    pub fn from(s: &str) -> Self {
        match s {
            "virtio-blk" => Self::VirtioBlk,
            "virtio-scsi" => Self::VirtioScsi,
            "virtio-mmio" => Self::VirtioMmio,
            _ => Self::VirtioBlk,
        }
    }

    pub fn to_driver_string(&self) -> String {
        match self {
            BlockDriver::VirtioBlk => "blk".to_string(),
            BlockDriver::VirtioMmio => "mmioblk".to_string(),
            BlockDriver::VirtioScsi => "scsi".to_string(),
        }
    }

    pub fn to_bus_type(&self) -> BusType {
        match self {
            BlockDriver::VirtioBlk => BusType::PCI,
            BlockDriver::VirtioMmio => BusType::NULL,
            BlockDriver::VirtioScsi => BusType::SCSI,
        }
    }

    pub fn from_bus_type(bus_type: &BusType) -> Self {
        match bus_type {
            BusType::PCI => Self::VirtioBlk,
            BusType::SCSI => Self::VirtioScsi,
            BusType::MMIO => Self::VirtioMmio,
            _ => Self::VirtioBlk,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub enum ShareFsType {
    Virtio9P,
    VirtioFS,
    VirtioBlk,
}

impl ShareFsType {
    pub fn as_str(&self) -> &str {
        match self {
            ShareFsType::VirtioFS => "virtiofs",
            ShareFsType::Virtio9P => "9p",
            ShareFsType::VirtioBlk => SHAREFS_VIRTIO_BLK,
        }
    }
}

impl FromStr for ShareFsType {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            VIRTIO_FS => Ok(ShareFsType::VirtioFS),
            VIRTIO_9P => Ok(ShareFsType::Virtio9P),
            SHAREFS_VIRTIO_BLK => Ok(ShareFsType::VirtioBlk),
            _ => Err(Error::InvalidArgument(s.to_string())),
        }
    }
}

#[derive(Debug)]
pub struct VcpuThreads {
    pub vcpus: HashMap<i64, i64>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Pids {
    pub vmm_pid: Option<u32>,
    pub affiliated_pids: Vec<u32>,
}
