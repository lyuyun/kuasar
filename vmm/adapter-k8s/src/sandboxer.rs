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
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use containerd_sandbox::data::{ContainerData, SandboxData};
use containerd_sandbox::error::Result as SandboxResult;
use containerd_sandbox::signal::ExitSignal;
use containerd_sandbox::{
    Container, ContainerOption, Sandbox, SandboxOption, SandboxStatus, Sandboxer,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::sync::Mutex;
use vmm_common::SHARED_DIR_SUFFIX;
use vmm_engine::instance::{ContainerState, StorageMount, StorageMountKind};
use vmm_engine::state::SandboxState;
use vmm_engine::{CreateSandboxRequest, SandboxInstance};
use vmm_guest_runtime::{ContainerRuntime, GuestReadiness};
use vmm_vm_trait::{Hooks, HotPlugDevice, Vmm};

use crate::K8sAdapter;

/// Signature for the block-device check: given a path, returns whether it is a
/// block device.  Abstracted so that unit tests can inject a stub without
/// hitting the real filesystem.
pub type BlockCheckFn =
    dyn Fn(String) -> Pin<Box<dyn Future<Output = anyhow::Result<bool>> + Send>> + Send + Sync;

// ── K8sContainer ─────────────────────────────────────────────────────────────

/// View of a single container, returned by `Sandbox::container()`.
#[derive(Clone)]
pub struct K8sContainer {
    pub data: ContainerData,
}

impl Container for K8sContainer {
    fn get_data(&self) -> SandboxResult<ContainerData> {
        Ok(self.data.clone())
    }
}

// ── K8sSandboxView ────────────────────────────────────────────────────────────

/// Projection of a single sandbox for the `Sandbox` trait.
/// Holds a local container cache so that `container()` can return `&Self::Container`.
pub struct K8sSandboxView<V: Vmm, R: GuestReadiness + ContainerRuntime, H: Hooks<V>> {
    pub(crate) engine: Arc<vmm_engine::SandboxEngine<V, R, H>>,
    pub(crate) id: String,
    /// Local mirror of `ContainerState` → `K8sContainer`, in sync with engine state.
    pub(crate) containers: HashMap<String, K8sContainer>,
    /// Block-device check, injectable for testing.
    pub(crate) block_check: Arc<BlockCheckFn>,
}

#[async_trait]
impl<V, R, H> Sandbox for K8sSandboxView<V, R, H>
where
    V: Vmm + Serialize + DeserializeOwned + Default + 'static,
    R: GuestReadiness + ContainerRuntime + 'static,
    H: Hooks<V> + 'static,
{
    type Container = K8sContainer;

    fn status(&self) -> SandboxResult<SandboxStatus> {
        let inst_arc = self
            .engine
            .get_sandbox_sync(&self.id)
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let inst = inst_arc.blocking_lock();
        Ok(sandbox_state_to_status(&inst.state))
    }

    async fn ping(&self) -> SandboxResult<()> {
        let inst_arc = self
            .engine
            .get_sandbox(&self.id)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let guard = inst_arc.lock().await;
        guard
            .vmm
            .ping()
            .await
            .map_err(|e| anyhow::anyhow!("{}", e).into())
    }

    async fn container(&self, id: &str) -> SandboxResult<&Self::Container> {
        self.containers.get(id).ok_or_else(|| {
            containerd_sandbox::error::Error::Other(anyhow::anyhow!("container not found: {}", id))
        })
    }

    /// Mirrors `KuasarSandbox::append_container`.
    ///
    /// **Host-side responsibilities:**
    /// 1. Create bundle directory in the shared virtiofs path.
    /// 2. Process rootfs/bind/block storage mounts (`attach_container_storages`).
    /// 3. Hot-attach IO pipes as `CharDevice` or vsock port (`attach_io_pipes`).
    /// 4. Persist updated `SandboxInstance` and update the local container cache.
    async fn append_container(&mut self, id: &str, options: ContainerOption) -> SandboxResult<()> {
        let inst_mutex = self
            .engine
            .get_sandbox(&self.id)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let mut inst = inst_mutex.lock().await;

        // 1. Create bundle dir in virtiofs shared directory
        let bundle = format!("{}/{}/{}", inst.base_dir, SHARED_DIR_SUFFIX, id);
        tokio::fs::create_dir_all(&bundle)
            .await
            .map_err(|e| anyhow::anyhow!("create bundle dir: {}", e))?;

        let mut data = options.container.clone();
        data.bundle = bundle;

        let mut io_devices: Vec<String> = vec![];

        // 2. Process storage mounts
        attach_container_storages(&mut inst, id, &mut data, &*self.block_check)
            .await
            .map_err(|e| anyhow::anyhow!("attach storages: {}", e))?;

        // 3. Hot-attach IO pipes
        if let Some(io) = &data.io.clone() {
            attach_io_pipes(&mut inst, id, io, &mut io_devices, &mut data)
                .await
                .map_err(|e| anyhow::anyhow!("attach io pipes: {}", e))?;
        }

        let container = ContainerState {
            id: id.to_string(),
            data: data.clone(),
            io_devices,
            processes: vec![],
        };
        inst.containers.insert(id.to_string(), container);
        inst.dump()
            .await
            .map_err(|e| anyhow::anyhow!("dump: {}", e))?;

        self.containers
            .insert(id.to_string(), K8sContainer { data });
        Ok(())
    }

    async fn update_container(&mut self, id: &str, options: ContainerOption) -> SandboxResult<()> {
        let inst_mutex = self
            .engine
            .get_sandbox(&self.id)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let mut inst = inst_mutex.lock().await;
        if let Some(c) = inst.containers.get_mut(id) {
            c.data = options.container.clone();
        }
        inst.dump()
            .await
            .map_err(|e| anyhow::anyhow!("dump: {}", e))?;
        if let Some(c) = self.containers.get_mut(id) {
            c.data = options.container.clone();
        }
        Ok(())
    }

    async fn remove_container(&mut self, id: &str) -> SandboxResult<()> {
        let inst_mutex = self
            .engine
            .get_sandbox(&self.id)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let mut inst = inst_mutex.lock().await;

        deference_container_storages(&mut inst, id)
            .await
            .map_err(|e| anyhow::anyhow!("deference storages: {}", e))?;

        let bundle = format!("{}/{}/{}", inst.base_dir, SHARED_DIR_SUFFIX, id);
        tokio::fs::remove_dir_all(&bundle).await.ok();

        if let Some(c) = inst.containers.remove(id) {
            for dev_id in c.io_devices {
                inst.vmm.hot_detach(&dev_id).await.ok();
            }
        }
        inst.dump()
            .await
            .map_err(|e| anyhow::anyhow!("dump: {}", e))?;

        self.containers.remove(id);
        Ok(())
    }

    async fn exit_signal(&self) -> SandboxResult<Arc<ExitSignal>> {
        let inst_arc = self
            .engine
            .get_sandbox(&self.id)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let sig = inst_arc.lock().await.exit_signal.clone();
        Ok(sig)
    }

    fn get_data(&self) -> SandboxResult<SandboxData> {
        let inst_arc = self
            .engine
            .get_sandbox_sync(&self.id)
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let data = inst_arc.blocking_lock().data.clone();
        Ok(data)
    }
}

// ── impl Sandboxer for K8sAdapter ─────────────────────────────────────────────

#[async_trait]
impl<V, R, H> Sandboxer for K8sAdapter<V, R, H>
where
    V: Vmm + Serialize + DeserializeOwned + Default + 'static + Send + Sync,
    R: GuestReadiness + ContainerRuntime + 'static,
    H: Hooks<V> + 'static,
{
    type Sandbox = K8sSandboxView<V, R, H>;

    async fn create(&self, id: &str, s: SandboxOption) -> SandboxResult<()> {
        let req = self
            .parse_create_request(id, s)
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        self.engine
            .create_sandbox(id, req)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e).into())
    }

    async fn start(&self, id: &str) -> SandboxResult<()> {
        let result = self
            .engine
            .start_sandbox(id)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        tracing::info!(sandbox_id = %id, ready_ms = %result.ready_ms, "sandbox started");
        Ok(())
    }

    async fn update(&self, id: &str, data: SandboxData) -> SandboxResult<()> {
        let inst_arc = self
            .engine
            .get_sandbox(id)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let mut inst = inst_arc.lock().await;
        inst.data = data;
        inst.dump()
            .await
            .map_err(|e| anyhow::anyhow!("{}", e).into())
    }

    async fn stop(&self, id: &str, force: bool) -> SandboxResult<()> {
        self.engine
            .stop_sandbox(id, force)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e).into())
    }

    async fn delete(&self, id: &str) -> SandboxResult<()> {
        self.engine
            .delete_sandbox(id, false)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e).into())
    }

    async fn sandbox(&self, id: &str) -> SandboxResult<Arc<Mutex<Self::Sandbox>>> {
        let inst_mutex = self
            .engine
            .get_sandbox(id)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let inst = inst_mutex.lock().await;
        let containers = inst
            .containers
            .iter()
            .map(|(cid, cs)| {
                (
                    cid.clone(),
                    K8sContainer {
                        data: cs.data.clone(),
                    },
                )
            })
            .collect();
        let view = K8sSandboxView {
            engine: self.engine.clone(),
            id: id.to_string(),
            containers,
            block_check: Arc::new(|path| Box::pin(is_block_device(path))),
        };
        Ok(Arc::new(Mutex::new(view)))
    }
}

impl<V, R, H> K8sAdapter<V, R, H>
where
    V: Vmm + Serialize + DeserializeOwned + Default,
    R: GuestReadiness + ContainerRuntime,
    H: Hooks<V>,
{
    fn parse_create_request(
        &self,
        _id: &str,
        s: SandboxOption,
    ) -> anyhow::Result<CreateSandboxRequest> {
        let sandbox_data = s.sandbox.clone();
        let cgroup_parent = sandbox_data
            .config
            .as_ref()
            .and_then(|c| c.linux.as_ref())
            .map(|l| l.cgroup_parent.clone())
            .unwrap_or_default();
        Ok(CreateSandboxRequest {
            sandbox_data,
            netns: s.sandbox.netns.clone(),
            cgroup_parent,
            rootfs_disk: None,
        })
    }
}

// ── State conversion ──────────────────────────────────────────────────────────

fn sandbox_state_to_status(s: &SandboxState) -> SandboxStatus {
    match s {
        SandboxState::Creating => SandboxStatus::Created,
        SandboxState::Running => SandboxStatus::Running(0),
        SandboxState::Stopped | SandboxState::Deleted => SandboxStatus::Stopped(0, 0),
    }
}

// ── Storage helpers ───────────────────────────────────────────────────────────

const KUASAR_GUEST_SHARE_DIR: &str = "/run/kuasar/storage/containers/";

/// Attach storage mounts for a container.
///
/// - Block devices: hot-plug as `VirtioBlock`, record `device_id` in `StorageMount`.
/// - Bind mounts: bind-mount the host source into the shared virtiofs directory.
async fn attach_container_storages(
    inst: &mut SandboxInstance<impl Vmm>,
    container_id: &str,
    data: &mut ContainerData,
    is_block: &BlockCheckFn,
) -> anyhow::Result<()> {
    let mounts: Vec<_> = {
        let spec_mounts = data
            .spec
            .as_ref()
            .map(|s| s.mounts.clone())
            .unwrap_or_default();
        let rootfs = data.rootfs.clone();
        spec_mounts.into_iter().chain(rootfs.into_iter()).collect()
    };

    for m in &mounts {
        if is_block(m.source.clone()).await? {
            // Dedup: if already hot-plugged for another container, add ref
            if let Some(existing) = inst
                .storages
                .iter_mut()
                .find(|s| s.host_path == m.source && s.kind == StorageMountKind::Block)
            {
                if !existing.ref_containers.contains(&container_id.to_string()) {
                    existing.ref_containers.push(container_id.to_string());
                }
                continue;
            }
            inst.id_generator += 1;
            let dev_id = format!("blk{}", inst.id_generator);
            let result = inst
                .vmm
                .hot_attach(HotPlugDevice::VirtioBlock {
                    id: dev_id.clone(),
                    path: m.source.clone(),
                    read_only: m.options.contains(&"ro".to_string()),
                })
                .await?;
            let guest_path = format!("{}{}", KUASAR_GUEST_SHARE_DIR, dev_id);
            inst.storages.push(StorageMount {
                id: dev_id.clone(),
                ref_containers: vec![container_id.to_string()],
                host_path: m.source.clone(),
                mount_dest: None,
                guest_path,
                kind: StorageMountKind::Block,
                device_id: Some(result.device_id),
            });
        } else if is_bind_mount(m) {
            // Dedup: if already bind-mounted for another container, reuse
            if let Some(existing) = inst
                .storages
                .iter_mut()
                .find(|s| s.host_path == m.source && s.kind == StorageMountKind::VirtioFs)
            {
                if !existing.ref_containers.contains(&container_id.to_string()) {
                    existing.ref_containers.push(container_id.to_string());
                }
                continue;
            }
            inst.id_generator += 1;
            let storage_id = format!("storage{}", inst.id_generator);
            let host_dest = format!("{}/{}/{}", inst.base_dir, SHARED_DIR_SUFFIX, storage_id);
            bind_mount_into_shared(&m.source, &host_dest).await?;
            let guest_path = format!("{}{}", KUASAR_GUEST_SHARE_DIR, storage_id);
            inst.storages.push(StorageMount {
                id: storage_id,
                ref_containers: vec![container_id.to_string()],
                host_path: m.source.clone(),
                mount_dest: Some(host_dest),
                guest_path,
                kind: StorageMountKind::VirtioFs,
                device_id: None,
            });
        }
        // tmpfs, shm, overlay handled by vmm-task; skip on host side
    }
    Ok(())
}

/// Remove `container_id` from the `ref_containers` of each `StorageMount`.
/// Only unmounts and hot-detaches when `ref_containers` becomes empty.
pub async fn deference_container_storages(
    inst: &mut SandboxInstance<impl Vmm>,
    container_id: &str,
) -> anyhow::Result<()> {
    let mut remaining = vec![];
    for mut sm in inst.storages.drain(..) {
        sm.ref_containers.retain(|c| c != container_id);
        if !sm.ref_containers.is_empty() {
            remaining.push(sm);
            continue;
        }
        // Last reference removed — unmount bind-mount destination
        if let Some(ref dest) = sm.mount_dest {
            const MNT_DETACH: i32 = 0x2;
            vmm_common::mount::unmount(dest, MNT_DETACH).ok();
            if tokio::fs::metadata(dest)
                .await
                .map(|m| m.is_dir())
                .unwrap_or(false)
            {
                tokio::fs::remove_dir(dest).await.ok();
            } else {
                tokio::fs::remove_file(dest).await.ok();
            }
        }
        if let Some(dev_id) = sm.device_id {
            inst.vmm.hot_detach(&dev_id).await.ok();
        }
    }
    inst.storages = remaining;
    Ok(())
}

/// Attach container stdio using the IO model appropriate for this VMM backend.
async fn attach_io_pipes(
    inst: &mut SandboxInstance<impl Vmm>,
    container_id: &str,
    io: &containerd_sandbox::data::Io,
    io_devices: &mut Vec<String>,
    data: &mut ContainerData,
) -> anyhow::Result<()> {
    if inst.vmm.capabilities().virtio_serial {
        attach_io_pipes_char(inst, container_id, io, io_devices, data).await
    } else {
        attach_io_vsock_mux(inst, container_id, io, io_devices, data).await
    }
}

/// virtio-serial path: hot-attach one `CharDevice` per non-empty stdio pipe.
async fn attach_io_pipes_char(
    inst: &mut SandboxInstance<impl Vmm>,
    _container_id: &str,
    io: &containerd_sandbox::data::Io,
    io_devices: &mut Vec<String>,
    data: &mut ContainerData,
) -> anyhow::Result<()> {
    let mut new_io = io.clone();

    if !io.stdin.is_empty() && !io.stdin.contains("://") {
        let (dev_id, chardev_id) = hot_attach_pipe(inst, &io.stdin).await?;
        io_devices.push(dev_id);
        new_io.stdin = chardev_id;
    }
    if !io.stdout.is_empty() && !io.stdout.contains("://") {
        let (dev_id, chardev_id) = hot_attach_pipe(inst, &io.stdout).await?;
        io_devices.push(dev_id);
        new_io.stdout = chardev_id;
    }
    if !io.stderr.is_empty() && !io.stderr.contains("://") {
        let (dev_id, chardev_id) = hot_attach_pipe(inst, &io.stderr).await?;
        io_devices.push(dev_id);
        new_io.stderr = chardev_id;
    }

    data.io = Some(new_io);
    Ok(())
}

/// vsock-mux path: allocate one vsock port for this container's stdio.
async fn attach_io_vsock_mux(
    inst: &mut SandboxInstance<impl Vmm>,
    container_id: &str,
    _io: &containerd_sandbox::data::Io,
    io_devices: &mut Vec<String>,
    data: &mut ContainerData,
) -> anyhow::Result<()> {
    let port = inst.vsock_port_next;
    inst.vsock_port_next += 1;
    let dev_id = format!("vsockmux{}", port);
    inst.vmm
        .hot_attach(HotPlugDevice::VsockMuxIO {
            id: dev_id.clone(),
            container_id: container_id.to_string(),
            port,
        })
        .await?;
    io_devices.push(dev_id);
    let vsock_uri = format!("vsock://:{}", port);
    data.io = Some(containerd_sandbox::data::Io {
        stdin: vsock_uri.clone(),
        stdout: vsock_uri.clone(),
        stderr: vsock_uri.clone(),
        terminal: data.io.as_ref().map(|i| i.terminal).unwrap_or(false),
    });
    Ok(())
}

/// Hot-attach one named pipe as a virtio-serial `CharDevice`.
async fn hot_attach_pipe(
    inst: &mut SandboxInstance<impl Vmm>,
    path: &str,
) -> anyhow::Result<(String, String)> {
    inst.id_generator += 1;
    let n = inst.id_generator;
    let device_id = format!("virtioserial{}", n);
    let chardev_id = format!("chardev{}", n);
    inst.vmm
        .hot_attach(HotPlugDevice::CharDevice {
            id: device_id.clone(),
            chardev_id: chardev_id.clone(),
            name: chardev_id.clone(),
            path: path.to_string(),
        })
        .await?;
    Ok((device_id, chardev_id))
}

// ── Low-level helpers (stubs for skeleton) ────────────────────────────────────

/// Check if `path` is a block device.
async fn is_block_device(path: String) -> anyhow::Result<bool> {
    use std::os::unix::fs::FileTypeExt;
    match tokio::fs::metadata(&path).await {
        Ok(meta) => Ok(meta.file_type().is_block_device()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Check if mount `m` is a bind mount (not block, not tmpfs/overlay/proc/sys).
fn is_bind_mount(m: &containerd_sandbox::spec::Mount) -> bool {
    !m.source.is_empty()
        && !m.source.starts_with("/dev/")
        && m.r#type != "tmpfs"
        && m.r#type != "proc"
        && m.r#type != "sysfs"
        && m.r#type != "overlay"
        && m.r#type != "shm"
        && m.r#type != "devpts"
        && m.r#type != "cgroup"
        && m.r#type != "cgroup2"
        && m.r#type != "mqueue"
        && m.r#type != "hugetlbfs"
}

/// Bind-mount `source` to `dest` inside the shared virtiofs directory.
async fn bind_mount_into_shared(source: &str, dest: &str) -> anyhow::Result<()> {
    use nix::mount::{mount, MsFlags};

    // Create the mount point (directory or empty file) at dest
    let is_dir = tokio::fs::metadata(source)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false);
    if is_dir {
        tokio::fs::create_dir_all(dest)
            .await
            .map_err(|e| anyhow::anyhow!("create dest dir {}: {}", dest, e))?;
    } else {
        if let Some(parent) = std::path::Path::new(dest).parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        tokio::fs::write(dest, b"")
            .await
            .map_err(|e| anyhow::anyhow!("create dest file {}: {}", dest, e))?;
    }

    let src = source.to_string();
    let dst = dest.to_string();
    tokio::task::spawn_blocking(move || {
        mount(
            Some(src.as_str()),
            dst.as_str(),
            None::<&str>,
            MsFlags::MS_BIND,
            None::<&str>,
        )
        .map_err(|e| anyhow::anyhow!("bind mount {} -> {}: {}", src, dst, e))
    })
    .await
    .map_err(|e| anyhow::anyhow!("spawn_blocking bind_mount: {}", e))?
}
