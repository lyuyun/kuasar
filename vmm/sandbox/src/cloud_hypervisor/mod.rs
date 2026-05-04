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

use std::{os::fd::OwnedFd, process::Stdio, time::{Duration, Instant}};

use anyhow::anyhow;
use async_trait::async_trait;
use containerd_sandbox::error::{Error, Result};
use log::{debug, error, info, warn};
use nix::{errno::Errno::ESRCH, sys::signal, unistd::Pid};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::{
    fs::create_dir_all,
    process::Child,
    sync::watch::{channel, Receiver, Sender},
    task::JoinHandle,
};
use tracing::instrument;
use vmm_common::SHARED_DIR_SUFFIX;

use ttrpc::context::with_timeout;
use vmm_common::api::sandbox::{CheckRequest, ExecVMProcessRequest};

use crate::{
    client::{new_sandbox_client, new_sandbox_client_fail_fast},
    cloud_hypervisor::{
        client::ChClient,
        config::{CloudHypervisorConfig, CloudHypervisorVMConfig, VirtiofsdConfig},
        devices::{
            block::Disk, vfio::VfioDevice, virtio_net::VirtioNetDevice, CloudHypervisorDevice,
        },
        snapshot::patch_snapshot_config,
    },
    device::{BusType, DeviceInfo},
    param::ToCmdLineParams,
    utils::{read_std, set_cmd_fd, set_cmd_netns, wait_channel, wait_pid, write_file_atomic},
    vm::{Pids, RestoreSource, SnapshotMeta, Snapshottable, VcpuThreads, VM},
};

mod client;
pub mod config;
pub mod devices;
pub mod factory;
pub mod hooks;
pub mod snapshot;

const VCPU_PREFIX: &str = "vcpu";

#[derive(Default, Serialize, Deserialize)]
pub struct CloudHypervisorVM {
    id: String,
    config: CloudHypervisorConfig,
    #[serde(skip)]
    devices: Vec<Box<dyn CloudHypervisorDevice + Sync + Send>>,
    netns: String,
    base_dir: String,
    agent_socket: String,
    virtiofsd_config: VirtiofsdConfig,
    sharefs_type: String,
    #[serde(skip)]
    wait_chan: Option<Receiver<(u32, i128)>>,
    #[serde(skip)]
    client: Option<ChClient>,
    #[serde(skip)]
    fds: Vec<OwnedFd>,
    pids: Pids,
}

impl CloudHypervisorVM {
    pub fn new(id: &str, netns: &str, base_dir: &str, vm_config: &CloudHypervisorVMConfig) -> Self {
        let mut config = CloudHypervisorConfig::from(vm_config);
        config.api_socket = format!("{}/api.sock", base_dir);
        if !vm_config.common.initrd_path.is_empty() {
            config.initramfs = Some(vm_config.common.initrd_path.clone());
        }

        let sharefs_type = vm_config.sharefs_type().to_string();
        let mut virtiofsd_config = vm_config.virtiofsd.clone();
        // Only configure virtiofsd when using virtiofs sharefs type
        if sharefs_type == "virtiofs" {
            virtiofsd_config.socket_path = format!("{}/virtiofs.sock", base_dir);
            virtiofsd_config.shared_dir = format!("{}/{}", base_dir, SHARED_DIR_SUFFIX);
        }
        Self {
            id: id.to_string(),
            config,
            devices: vec![],
            netns: netns.to_string(),
            base_dir: base_dir.to_string(),
            agent_socket: "".to_string(),
            virtiofsd_config,
            sharefs_type,
            wait_chan: None,
            client: None,
            fds: vec![],
            pids: Pids::default(),
        }
    }

    pub fn add_device(&mut self, device: impl CloudHypervisorDevice + 'static) {
        self.devices.push(Box::new(device));
    }

    fn pid(&self) -> Result<u32> {
        match self.pids.vmm_pid {
            None => Err(anyhow!("empty pid from vmm_pid").into()),
            Some(pid) => Ok(pid),
        }
    }

    async fn create_client(&self) -> Result<ChClient> {
        ChClient::new(self.config.api_socket.to_string()).await
    }

    fn get_client(&mut self) -> Result<&mut ChClient> {
        self.client.as_mut().ok_or(Error::NotFound(
            "cloud hypervisor client not inited".to_string(),
        ))
    }

    async fn start_virtiofsd(&self) -> Result<u32> {
        create_dir_all(&self.virtiofsd_config.shared_dir).await?;
        let params = self.virtiofsd_config.to_cmdline_params("--");
        let mut cmd = tokio::process::Command::new(&self.virtiofsd_config.path);
        cmd.args(params.as_slice());
        debug!("start virtiofsd with cmdline: {:?}", cmd);
        set_cmd_netns(&mut cmd, self.netns.to_string())?;
        cmd.stderr(Stdio::piped());
        cmd.stdout(Stdio::piped());
        let child = cmd
            .spawn()
            .map_err(|e| anyhow!("failed to spawn virtiofsd command: {}", e))?;
        let pid = child
            .id()
            .ok_or(anyhow!("the virtiofsd has been polled to completion"))?;
        info!("virtiofsd for {} is running with pid {}", self.id, pid);
        spawn_wait(child, format!("virtiofsd {}", self.id), None, None);
        Ok(pid)
    }

    fn append_fd(&mut self, fd: OwnedFd) -> usize {
        self.fds.push(fd);
        self.fds.len() - 1 + 3
    }

    async fn wait_stop(&mut self, t: Duration) -> Result<()> {
        if let Some(rx) = self.wait_channel().await {
            let (_, ts) = *rx.borrow();
            if ts == 0 {
                wait_channel(t, rx).await?;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl VM for CloudHypervisorVM {
    #[instrument(skip_all)]
    async fn start(&mut self) -> Result<u32> {
        create_dir_all(&self.base_dir).await?;
        // Validate host tool availability before committing to virtio-blk mode
        if self.sharefs_type == crate::vm::SHAREFS_VIRTIO_BLK {
            check_virtio_blk_host_tools().await?;
        }
        // Only start virtiofsd when sharefs_type is virtiofs
        if self.sharefs_type == "virtiofs" {
            let virtiofsd_pid = self.start_virtiofsd().await?;
            self.pids.affiliated_pids.push(virtiofsd_pid);
        }
        let mut params = self.config.to_cmdline_params("--");
        for d in self.devices.iter() {
            params.extend(d.to_cmdline_params("--"));
        }

        // the log level is single hyphen parameter, has to handle separately
        if self.config.debug {
            params.push("-vv".to_string());
        }

        // Drop cmd immediately to let the fds in pre_exec be closed.
        let child = {
            let mut cmd = tokio::process::Command::new(&self.config.path);
            cmd.args(params.as_slice());

            set_cmd_fd(&mut cmd, self.fds.drain(..).collect())?;
            set_cmd_netns(&mut cmd, self.netns.to_string())?;
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());
            info!("start cloud hypervisor with cmdline: {:?}", cmd);
            cmd.spawn()
                .map_err(|e| anyhow!("failed to spawn cloud hypervisor command: {}", e))?
        };
        let pid = child.id();
        info!(
            "cloud hypervisor for {} is running with pid {}",
            self.id,
            pid.unwrap_or_default()
        );
        self.pids.vmm_pid = pid;
        let pid_file = format!("{}/pid", self.base_dir);
        let (tx, rx) = channel((0u32, 0i128));
        self.wait_chan = Some(rx);
        spawn_wait(
            child,
            format!("cloud-hypervisor {}", self.id),
            Some(pid_file),
            Some(tx),
        );

        match self.create_client().await {
            Ok(client) => self.client = Some(client),
            Err(e) => {
                if let Err(re) = self.stop(true).await {
                    warn!("roll back in create clh api client: {}", re);
                    return Err(e);
                }
                return Err(e);
            }
        };
        Ok(pid.unwrap_or_default())
    }

    #[instrument(skip_all)]
    async fn stop(&mut self, force: bool) -> Result<()> {
        let signal = if force {
            signal::SIGKILL
        } else {
            signal::SIGTERM
        };

        let pids = self.pids();
        if let Some(vmm_pid) = pids.vmm_pid {
            if vmm_pid > 0 {
                // TODO: Consider pid reused
                match signal::kill(Pid::from_raw(vmm_pid as i32), signal) {
                    Err(e) => {
                        if e != ESRCH {
                            return Err(anyhow!("kill vmm process {}: {}", vmm_pid, e).into());
                        }
                    }
                    Ok(_) => self.wait_stop(Duration::from_secs(10)).await?,
                }
            }
        }
        for affiliated_pid in pids.affiliated_pids {
            if affiliated_pid > 0 {
                // affiliated process may exits automatically, so it's ok not handle error
                signal::kill(Pid::from_raw(affiliated_pid as i32), signal).unwrap_or_default();
            }
        }

        Ok(())
    }

    #[instrument(skip_all)]
    async fn attach(&mut self, device_info: DeviceInfo) -> Result<()> {
        match device_info {
            DeviceInfo::Block(blk_info) => {
                let device = Disk::new(&blk_info.id, &blk_info.path, blk_info.read_only, true);
                self.add_device(device);
            }
            DeviceInfo::Tap(tap_info) => {
                let mut fd_ints = vec![];
                for fd in tap_info.fds {
                    let index = self.append_fd(fd);
                    fd_ints.push(index as i32);
                }
                let device = VirtioNetDevice::new(
                    &tap_info.id,
                    Some(tap_info.name),
                    &tap_info.mac_address,
                    fd_ints,
                );
                self.add_device(device);
            }
            DeviceInfo::Physical(vfio_info) => {
                let device = VfioDevice::new(&vfio_info.id, &vfio_info.bdf);
                self.add_device(device);
            }
            DeviceInfo::VhostUser(_vhost_user_info) => {
                todo!()
            }
            DeviceInfo::Char(_char_info) => {
                unimplemented!()
            }
        };
        Ok(())
    }

    #[instrument(skip_all)]
    async fn hot_attach(&mut self, device_info: DeviceInfo) -> Result<(BusType, String)> {
        let client = self.get_client()?;
        let addr = client.hot_attach(device_info)?;
        Ok((BusType::PCI, addr))
    }

    #[instrument(skip_all)]
    async fn hot_detach(&mut self, id: &str) -> Result<()> {
        let client = self.get_client()?;
        client.hot_detach(id)?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn ping(&self) -> Result<()> {
        if self.agent_socket.is_empty() {
            return Ok(());
        }
        let client = new_sandbox_client_fail_fast(&self.agent_socket)
            .await
            .map_err(|e| anyhow!("ping: connect to agent socket: {}", e))?;
        let req = CheckRequest::new();
        client
            .check(
                with_timeout(Duration::from_secs(3).as_nanos() as i64),
                &req,
            )
            .await
            .map_err(|e| anyhow!("ping: agent check RPC: {}", e))?;
        Ok(())
    }

    #[instrument(skip_all)]
    fn socket_address(&self) -> String {
        self.agent_socket.to_string()
    }

    #[instrument(skip_all)]
    async fn wait_channel(&self) -> Option<Receiver<(u32, i128)>> {
        self.wait_chan.clone()
    }

    #[instrument(skip_all)]
    async fn vcpus(&self) -> Result<VcpuThreads> {
        // Refer to https://github.com/firecracker-microvm/firecracker/issues/718
        Ok(VcpuThreads {
            vcpus: procfs::process::Process::new(self.pid()? as i32)
                .map_err(|e| anyhow!("failed to get process {}", e))?
                .tasks()
                .map_err(|e| anyhow!("failed to get tasks {}", e))?
                .flatten()
                .filter_map(|t| {
                    t.stat()
                        .map_err(|e| anyhow!("failed to get stat {}", e))
                        .ok()?
                        .comm
                        .strip_prefix(VCPU_PREFIX)
                        .and_then(|comm| comm.parse().ok())
                        .map(|index| (index, t.tid as i64))
                })
                .collect(),
        })
    }

    #[instrument(skip_all)]
    fn pids(&self) -> Pids {
        self.pids.clone()
    }

    fn sharefs_type(&self) -> &str {
        &self.sharefs_type
    }
}

#[async_trait]
impl crate::vm::Recoverable for CloudHypervisorVM {
    #[instrument(skip_all)]
    async fn recover(&mut self) -> Result<()> {
        let pid = self.pid()?;
        // Fast-fail: if the process is gone, skip the 10-s socket connect timeout.
        signal::kill(Pid::from_raw(pid as i32), None).map_err(|_| {
            anyhow!("vm process {} is no longer running", pid)
        })?;
        if !std::path::Path::new(&self.config.api_socket).exists() {
            return Err(anyhow!(
                "api socket {} does not exist, vm process may have died",
                self.config.api_socket
            )
            .into());
        }
        self.client = Some(self.create_client().await?);
        let (tx, rx) = channel((0u32, 0i128));
        tokio::spawn(async move {
            let wait_result = wait_pid(pid as i32).await;
            tx.send(wait_result).unwrap_or_default();
        });
        self.wait_chan = Some(rx);
        Ok(())
    }
}

impl CloudHypervisorVM {
    fn vsock_path(&self) -> String {
        format!("{}/task.vsock", self.base_dir)
    }

    fn console_log_path(&self) -> String {
        format!("/tmp/{}-task.log", self.id)
    }

    /// Poll the hvsock agent with a short-connect fast-fail client until it responds to a Check
    /// RPC or the timeout expires.  Called after vm.restore() to confirm the guest agent is up.
    async fn wait_agent_ready(&self, timeout_secs: u64) -> Result<()> {
        let agent_socket = self.agent_socket.clone();
        let check_loop = async move {
            loop {
                match new_sandbox_client_fail_fast(&agent_socket).await {
                    Ok(client) => {
                        let req = CheckRequest::new();
                        let t = Duration::from_secs(3).as_nanos() as i64;
                        if client.check(with_timeout(t), &req).await.is_ok() {
                            return;
                        }
                    }
                    Err(_) => {}
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(timeout_secs), check_loop)
            .await
            .map_err(|_| {
                anyhow!(
                    "{}s timeout waiting for agent ready after restore",
                    timeout_secs
                )
                .into()
            })
    }

    async fn sync_guest_fs(&self) -> Result<()> {
        let client = new_sandbox_client(&self.agent_socket).await?;
        let timeout_ns = Duration::from_secs(10).as_nanos() as i64;
        let mut req = ExecVMProcessRequest::new();
        req.command = "sync".to_string();
        client
            .exec_vm_process(with_timeout(timeout_ns), &req)
            .await
            .map_err(|e| anyhow!("guest sync: {}", e))?;
        Ok(())
    }

    async fn launch_for_restore(&mut self) -> Result<()> {
        create_dir_all(&self.base_dir).await?;
        let child = {
            let mut cmd = tokio::process::Command::new(&self.config.path);
            cmd.arg("--api-socket").arg(&self.config.api_socket);
            if self.config.debug {
                cmd.arg("-vv");
            }
            set_cmd_netns(&mut cmd, self.netns.to_string())?;
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());
            info!("start cloud-hypervisor for restore: {:?}", cmd);
            cmd.spawn()
                .map_err(|e| anyhow!("failed to spawn cloud-hypervisor for restore: {}", e))?
        };
        let pid = child.id();
        self.pids.vmm_pid = pid;
        let pid_file = format!("{}/pid", self.base_dir);
        let (tx, rx) = channel((0u32, 0i128));
        self.wait_chan = Some(rx);
        spawn_wait(
            child,
            format!("cloud-hypervisor-restore {}", self.id),
            Some(pid_file),
            Some(tx),
        );
        match self.create_client().await {
            Ok(client) => self.client = Some(client),
            Err(e) => {
                if let Err(re) = self.stop(true).await {
                    warn!("rollback in restore launch: {}", re);
                }
                return Err(e);
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Snapshottable for CloudHypervisorVM {
    async fn snapshot(&mut self, dest_dir: &std::path::Path) -> Result<SnapshotMeta> {
        let t0 = Instant::now();
        let id = self.id.clone();
        tokio::fs::create_dir_all(dest_dir).await?;

        // Flush guest ext4 journals so snapshot captures fully-committed state.
        if let Err(e) = self.sync_guest_fs().await {
            warn!("guest sync before snapshot failed, snapshot may be inconsistent: {}", e);
        }
        info!("snapshot {id}: guest sync done in {}ms", t0.elapsed().as_millis());

        let client = self.get_client()?;

        // Freeze CPU and all device queues for a consistent point-in-time capture.
        client.vm_pause().map_err(|e| anyhow!("vm.pause: {}", e))?;
        info!("snapshot {id}: vm paused in {}ms", t0.elapsed().as_millis());

        let dest_url = format!("file://{}", dest_dir.display());
        let snap_result = client.vm_snapshot(&dest_url);

        // Always resume — a stuck VM is worse than a skipped snapshot.
        if let Err(e) = client.vm_resume() {
            error!("vm.resume after snapshot failed: {}", e);
        }
        info!("snapshot {id}: vm.snapshot + resume done in {}ms", t0.elapsed().as_millis());

        snap_result.map_err(|e| anyhow!("vm.snapshot: {}", e))?;

        Ok(SnapshotMeta {
            snapshot_dir: dest_dir.to_path_buf(),
            original_task_vsock: self.vsock_path(),
            original_console_path: self.console_log_path(),
        })
    }

    async fn restore(&mut self, src: &RestoreSource) -> Result<()> {
        let t0 = Instant::now();
        tokio::fs::create_dir_all(&src.work_dir).await?;

        // 1. Write patched config.json (sandbox-specific socket paths updated).
        patch_snapshot_config(
            &src.snapshot_dir.join("config.json"),
            &src.work_dir.join("config.json"),
            &src.overrides,
        )
        .await?;

        // 2. Symlink the immutable snapshot artefacts into the per-sandbox work dir.
        let mr_link = src.work_dir.join("memory-ranges");
        if !mr_link.exists() {
            tokio::fs::symlink(src.snapshot_dir.join("memory-ranges"), &mr_link)
                .await
                .map_err(|e| anyhow!("symlink memory-ranges: {}", e))?;
        }
        let state_link = src.work_dir.join("state.json");
        if !state_link.exists() {
            tokio::fs::symlink(src.snapshot_dir.join("state.json"), &state_link)
                .await
                .map_err(|e| anyhow!("symlink state.json: {}", e))?;
        }

        // 3. Start CH with only --api-socket; all VM config comes from the snapshot.
        self.launch_for_restore().await?;
        info!("restore {}: CH process ready in {}ms", self.id, t0.elapsed().as_millis());

        // 4. Trigger restore; CH loads config.json + state.json + memory-ranges from work_dir.
        // CH restores vCPUs in a paused state (snapshot was taken while paused).
        // Call vm.resume immediately after so the guest starts executing.
        let source_url = format!("file://{}", src.work_dir.display());
        {
            let client = self.get_client()?;
            client
                .vm_restore(&source_url, false)
                .map_err(|e| anyhow!("vm.restore API: {}", e))?;
            client
                .vm_resume()
                .map_err(|e| anyhow!("vm.resume after restore: {}", e))?;
        }
        info!("restore {}: vm.restore+resume done in {}ms", self.id, t0.elapsed().as_millis());

        // 5. Wait for the guest agent to come back up over hvsock (15 s timeout).
        if let Err(e) = self.wait_agent_ready(15).await {
            if let Err(ke) = self.stop(true).await {
                warn!("restore {}: kill CH after agent timeout: {}", self.id, ke);
            }
            return Err(anyhow!("restore {}: agent not ready: {}", self.id, e).into());
        }
        info!(
            "restore {}: agent ready, total restore time {}ms",
            self.id,
            t0.elapsed().as_millis()
        );

        Ok(())
    }
}

// Verify that host tools required for virtio-blk container layer preparation are available.
// Called before VM start when sharefs_type == "virtio-blk".
async fn check_virtio_blk_host_tools() -> containerd_sandbox::error::Result<()> {
    for tool in &["mkfs.ext4", "rsync"] {
        let ok = tokio::process::Command::new("which")
            .arg(tool)
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return Err(containerd_sandbox::error::Error::Other(anyhow::anyhow!(
                "virtio-blk mode requires '{}' on the host but it was not found",
                tool
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    /// Verify that wait_agent_ready times out promptly when no agent is listening.
    #[tokio::test]
    async fn test_wait_agent_ready_times_out() {
        let vm = CloudHypervisorVM {
            agent_socket: "hvsock:///nonexistent-socket-path.vsock:1024".to_string(),
            ..Default::default()
        };
        let result = vm.wait_agent_ready(1).await;
        assert!(result.is_err(), "expected timeout error");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("timeout"), "expected 'timeout' in error, got: {msg}");
    }

    /// End-to-end snapshot → restore roundtrip.
    ///
    /// Requires cloud-hypervisor binary, kernel, and rootfs.  Set the env vars:
    ///   CH_BINARY, CH_KERNEL, CH_ROOTFS
    /// Run with:
    ///   cargo test -p vmm-sandboxer -- --ignored snapshot_restore_roundtrip
    #[tokio::test]
    #[ignore = "requires host CH binary, kernel, and rootfs; set CH_BINARY/CH_KERNEL/CH_ROOTFS"]
    async fn snapshot_restore_roundtrip() {
        use crate::cloud_hypervisor::{
            config::CloudHypervisorVMConfig,
            devices::{console::Console, pmem::Pmem, vsock::Vsock},
        };
        use crate::vm::{RestoreSource, SnapshotPathOverrides, Snapshottable, VM};
        use temp_dir::TempDir;

        let ch_binary = std::env::var("CH_BINARY")
            .unwrap_or_else(|_| "/usr/local/bin/cloud-hypervisor".to_string());
        let kernel =
            std::env::var("CH_KERNEL").unwrap_or_else(|_| "/var/lib/kuasar/vmlinux".to_string());
        let rootfs =
            std::env::var("CH_ROOTFS").unwrap_or_else(|_| "/var/lib/kuasar/rootfs.img".to_string());

        for p in [&ch_binary, &kernel, &rootfs] {
            if !std::path::Path::new(p).exists() {
                eprintln!("snapshot_restore_roundtrip: skipping, {p} not found");
                return;
            }
        }

        let tmp = TempDir::new().unwrap();

        // ── template VM ──────────────────────────────────────────────────────
        let tmpl_dir = tmp.path().join("sandbox-template");
        tokio::fs::create_dir_all(&tmpl_dir).await.unwrap();

        let mut vm_config = CloudHypervisorVMConfig::default();
        vm_config.path = ch_binary.clone();
        vm_config.common.kernel_path = kernel.clone();
        vm_config.common.image_path = rootfs.clone();

        let mut tmpl_vm = CloudHypervisorVM::new("tmpl", "", tmpl_dir.to_str().unwrap(), &vm_config);
        tmpl_vm.add_device(Pmem::new("rootfs", &rootfs, true));
        let tmpl_vsock = format!("{}/task.vsock", tmpl_dir.display());
        tmpl_vm.add_device(Vsock::new(3, &tmpl_vsock, "vsock"));
        tmpl_vm.agent_socket = format!("hvsock://{}:1024", tmpl_vsock);
        tmpl_vm.add_device(Console::new("/tmp/tmpl-task.log", "console"));

        let t_cold = Instant::now();
        tmpl_vm.start().await.expect("template VM cold start failed");
        eprintln!("cold start: {}ms", t_cold.elapsed().as_millis());

        // snapshot
        let snapshot_dir = tmp.path().join("snapshot");
        let meta = tmpl_vm
            .snapshot(&snapshot_dir)
            .await
            .expect("snapshot failed");
        eprintln!("snapshot: {:?}", meta);

        tmpl_vm.stop(true).await.unwrap();

        // ── restore into a new sandbox ────────────────────────────────────────
        let restore_dir = tmp.path().join("sandbox-restore");
        tokio::fs::create_dir_all(&restore_dir).await.unwrap();

        let mut restore_vm =
            CloudHypervisorVM::new("restore", "", restore_dir.to_str().unwrap(), &vm_config);
        let restore_vsock = format!("{}/task.vsock", restore_dir.display());
        restore_vm.agent_socket = format!("hvsock://{}:1024", restore_vsock);

        let src = RestoreSource {
            snapshot_dir: snapshot_dir.clone(),
            work_dir: restore_dir.join("restore-work"),
            overrides: SnapshotPathOverrides {
                task_vsock: restore_vsock,
                console_path: "/tmp/restore-task.log".to_string(),
            },
        };

        let t_restore = Instant::now();
        restore_vm.restore(&src).await.expect("restore failed");
        let restore_ms = t_restore.elapsed().as_millis();
        eprintln!("restore (agent ready): {}ms", restore_ms);

        // P99 target: < 800 ms for 256 MB RAM
        assert!(
            restore_ms < 800,
            "restore took {restore_ms}ms, exceeds 800ms P99 target"
        );

        restore_vm.stop(true).await.unwrap();
    }
}

macro_rules! read_stdio {
    ($stdio:expr, $cmd_name:ident) => {
        if let Some(std) = $stdio {
            let cmd_name_clone = $cmd_name.clone();
            tokio::spawn(async move {
                read_std(std, &cmd_name_clone).await.unwrap_or_default();
            });
        }
    };
}

fn spawn_wait(
    child: Child,
    cmd_name: String,
    pid_file_path: Option<String>,
    exit_chan: Option<Sender<(u32, i128)>>,
) -> JoinHandle<()> {
    let mut child = child;
    tokio::spawn(async move {
        if let Some(pid_file) = pid_file_path {
            if let Some(pid) = child.id() {
                write_file_atomic(&pid_file, &pid.to_string())
                    .await
                    .unwrap_or_default();
            }
        }

        read_stdio!(child.stdout.take(), cmd_name);
        read_stdio!(child.stderr.take(), cmd_name);

        match child.wait().await {
            Ok(status) => {
                if !status.success() {
                    error!("{} exit {}", cmd_name, status);
                }
                let now = OffsetDateTime::now_utc();
                if let Some(tx) = exit_chan {
                    tx.send((
                        status.code().unwrap_or_default() as u32,
                        now.unix_timestamp_nanos(),
                    ))
                    .unwrap_or_default();
                }
            }
            Err(e) => {
                error!("{} wait error {}", cmd_name, e);
                let now = OffsetDateTime::now_utc();
                if let Some(tx) = exit_chan {
                    tx.send((0, now.unix_timestamp_nanos())).unwrap_or_default();
                }
            }
        }
    })
}
