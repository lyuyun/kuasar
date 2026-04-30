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
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::anyhow;
use async_trait::async_trait;
use containerd_sandbox::{
    cri::api::v1::NamespaceMode,
    data::SandboxData,
    error::{Error, Result},
    signal::ExitSignal,
    utils::cleanup_mounts,
    ContainerOption, Sandbox, SandboxOption, SandboxStatus, Sandboxer,
};
use containerd_shim::{
    api::{CreateTaskRequest as TaskCreateRequest, StartRequest as TaskStartRequest},
    protos::api::Envelope,
    util::write_str_to_file,
};
use log::{debug, error, info, warn};
use protobuf::{well_known_types::any::Any, MessageField};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use lazy_static::lazy_static;
use tokio::{
    fs::{copy, create_dir_all, remove_dir_all, OpenOptions},
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Mutex, RwLock, Semaphore},
};

lazy_static! {
    /// Limit concurrent VM restores to avoid exhausting host memory when many
    /// sandboxes start simultaneously from template snapshots.
    static ref RESTORE_SEMAPHORE: Semaphore = Semaphore::new(4);
}
use tracing::instrument;
use ttrpc::context::with_timeout;
use vmm_common::{
    api::{
        empty::Empty,
        sandbox::{ExecVMProcessRequest, SetupSandboxRequest},
        sandbox_ttrpc::SandboxServiceClient,
    },
    storage::Storage,
    ETC_HOSTS, ETC_RESOLV, HOSTNAME_FILENAME, HOSTS_FILENAME, KUASAR_STATE_DIR, RESOLV_FILENAME,
    SHARED_DIR_SUFFIX,
};

use crate::{
    cgroup::{SandboxCgroup, DEFAULT_CGROUP_PARENT_PATH},
    client::{
        client_check, client_setup_sandbox, client_sync_clock, new_sandbox_client,
        new_sandbox_client_fail_fast, new_task_client, DEFAULT_CLIENT_CHECK_TIMEOUT,
    },
    container::KuasarContainer,
    network::{Network, NetworkConfig},
    template::{CreateTemplateRequest, PooledTemplate, TemplateKey, TemplateMetrics, TemplatePool},
    utils::{get_dns_config, get_hostname, get_resources, get_sandbox_cgroup_parent_path},
    vm::{
        Hooks, Recoverable, RestoreSource, SnapshotMeta, Snapshottable, SnapshotPathOverrides,
        VMFactory, VM,
    },
};

pub const KUASAR_GUEST_SHARE_DIR: &str = "/run/kuasar/storage/containers/";

pub struct KuasarSandboxer<F: VMFactory, H: Hooks<F::VM>> {
    factory: F,
    hooks: H,
    #[allow(dead_code)]
    config: SandboxConfig,
    #[allow(clippy::type_complexity)]
    sandboxes: Arc<RwLock<HashMap<String, Arc<Mutex<KuasarSandbox<F::VM>>>>>>,
    pub template_pool: Option<Arc<TemplatePool>>,
}

impl<F, H> KuasarSandboxer<F, H>
where
    F: VMFactory,
    H: Hooks<F::VM>,
    F::VM: VM + Sync + Send,
{
    pub fn new(config: SandboxConfig, vmm_config: F::Config, hooks: H) -> Self {
        Self {
            factory: F::new(vmm_config),
            hooks,
            config,
            sandboxes: Arc::new(Default::default()),
            template_pool: None,
        }
    }

    /// Return pool metrics, if the pool has been initialized.
    pub fn pool_metrics(&self) -> Option<Arc<TemplateMetrics>> {
        self.template_pool.as_ref().map(|p| p.metrics.clone())
    }
}

impl<F, H> KuasarSandboxer<F, H>
where
    F: VMFactory,
    H: Hooks<F::VM>,
    F::VM: VM + DeserializeOwned + Recoverable + Sync + Send + 'static,
{
    #[instrument(skip_all)]
    pub async fn recover(&mut self, dir: &str) {
        let start = Instant::now();
        let mut subs = match tokio::fs::read_dir(dir).await {
            Ok(subs) => subs,
            Err(e) => {
                error!("FATAL! read working dir {} for recovery: {}", dir, e);
                return;
            }
        };

        let mut entries = Vec::new();
        while let Some(entry) = subs.next_entry().await.unwrap() {
            entries.push(entry);
        }

        // Limit the concurrency of sandbox recovery.
        // When a node has a large number of pods (e.g., 1k pods), unbounded concurrent
        // recovery could consume massive system resources, potentially preempting and
        // starving normal business workloads.
        const RECOVERY_CONCURRENCY: usize = 32;
        let semaphore = Arc::new(Semaphore::new(RECOVERY_CONCURRENCY));
        let mut handles = Vec::with_capacity(entries.len());

        for entry in entries {
            if let Ok(t) = entry.file_type().await {
                if !t.is_dir() {
                    continue;
                }
                let dir_path = dir.to_string();
                let sandboxes = self.sandboxes.clone();
                let permit = semaphore.clone().acquire_owned().await.unwrap();

                let handle = tokio::spawn(async move {
                    let _permit = permit;
                    debug!("recovering sandbox {:?}", entry.file_name());
                    let path = Path::new(&dir_path).join(entry.file_name());
                    match KuasarSandbox::recover(&path).await {
                        Ok(sb) => {
                            let status = sb.status.clone();
                            let sb_mutex = Arc::new(Mutex::new(sb));
                            // Only running sandbox should be monitored.
                            if let SandboxStatus::Running(_) = status {
                                let sb_clone = sb_mutex.clone();
                                monitor(sb_clone);
                            }
                            sandboxes
                                .write()
                                .await
                                .insert(entry.file_name().to_str().unwrap().to_string(), sb_mutex);
                            true
                        }
                        Err(e) => {
                            warn!("failed to recover sandbox {:?}, {:?}", entry.file_name(), e);
                            cleanup_mounts(path.to_str().unwrap())
                                .await
                                .unwrap_or_default();
                            remove_dir_all(&path).await.unwrap_or_default();
                            false
                        }
                    }
                });
                handles.push(handle);
            }
        }

        let total = handles.len();
        let mut success = 0;
        let mut fail = 0;
        for handle in handles {
            match handle.await {
                Ok(true) => success += 1,
                Ok(false) => fail += 1,
                Err(e) => {
                    error!("recovery task join error: {}", e);
                    fail += 1;
                }
            }
        }
        info!(
            "recover sandboxes finished, total: {}, success: {}, fail: {}, takes: {:.3}s",
            total,
            success,
            fail,
            start.elapsed().as_secs_f64()
        );
    }
}

#[derive(Serialize, Deserialize)]
pub struct KuasarSandbox<V: VM> {
    pub(crate) vm: V,
    pub(crate) id: String,
    pub(crate) status: SandboxStatus,
    pub(crate) base_dir: String,
    pub(crate) data: SandboxData,
    pub(crate) containers: HashMap<String, KuasarContainer>,
    pub(crate) storages: Vec<Storage>,
    pub(crate) id_generator: u32,
    pub(crate) network: Option<Network>,
    #[serde(skip, default)]
    pub(crate) client: Arc<Mutex<Option<SandboxServiceClient>>>,
    #[serde(skip, default)]
    pub(crate) exit_signal: Arc<ExitSignal>,
    #[serde(default)]
    pub(crate) sandbox_cgroups: SandboxCgroup,
    /// Set when this sandbox was restored from a template snapshot.
    #[serde(default)]
    pub(crate) template_id: Option<String>,
}

#[async_trait]
impl<F, H> Sandboxer for KuasarSandboxer<F, H>
where
    F: VMFactory + Clone + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
    H: Hooks<F::VM> + Sync + Send,
{
    type Sandbox = KuasarSandbox<F::VM>;

    #[instrument(skip_all)]
    async fn create(&self, id: &str, s: SandboxOption) -> Result<()> {
        if self.sandboxes.read().await.get(id).is_some() {
            return Err(Error::AlreadyExist("sandbox".to_string()));
        }

        let mut sandbox_cgroups = SandboxCgroup::default();
        let cgroup_parent_path = get_sandbox_cgroup_parent_path(&s.sandbox)
            .unwrap_or(DEFAULT_CGROUP_PARENT_PATH.to_string());
        // Currently only support cgroup V1, cgroup V2 is not supported now
        if !cgroups_rs::hierarchies::is_cgroup2_unified_mode() {
            // Create sandbox's cgroup and apply sandbox's resources limit
            let create_and_update_sandbox_cgroup = (|| {
                sandbox_cgroups =
                    SandboxCgroup::create_sandbox_cgroups(&cgroup_parent_path, &s.sandbox.id)?;
                sandbox_cgroups.update_res_for_sandbox_cgroups(&s.sandbox)?;
                Ok(())
            })();
            // If create and update sandbox cgroup failed, do rollback operation
            if let Err(e) = create_and_update_sandbox_cgroup {
                let _ = sandbox_cgroups.remove_sandbox_cgroups();
                return Err(e);
            }
        }
        let vm = self.factory.create_vm(id, &s).await?;
        let mut sandbox = KuasarSandbox {
            vm,
            id: id.to_string(),
            status: SandboxStatus::Created,
            base_dir: s.base_dir,
            data: s.sandbox.clone(),
            containers: Default::default(),
            storages: vec![],
            id_generator: 0,
            network: None,
            client: Arc::new(Mutex::new(None)),
            exit_signal: Arc::new(ExitSignal::default()),
            sandbox_cgroups,
            template_id: None,
        };

        // setup sandbox files: hosts, hostname and resolv.conf for guest
        sandbox.setup_sandbox_files().await?;
        self.hooks.post_create(&mut sandbox).await?;
        sandbox.dump().await?;
        self.sandboxes
            .write()
            .await
            .insert(id.to_string(), Arc::new(Mutex::new(sandbox)));
        Ok(())
    }

    #[instrument(skip_all)]
    async fn start(&self, id: &str) -> Result<()> {
        // Template pool fast path: if a matching pre-warmed snapshot is available,
        // restore from it instead of cold-booting.  Falls back to cold start
        // automatically on pool miss or restore failure.
        if let Some(pool) = &self.template_pool {
            let key = TemplateKey::new(
                self.factory.image_path(),
                self.factory.vcpus(),
                self.factory.memory_mb(),
            );
            if pool.depth(&key).await > 0 {
                return self.start_with_template(id, &key).await;
            }
        }

        let sandbox_mutex = self.sandbox(id).await?;
        let mut sandbox = sandbox_mutex.lock().await;
        self.hooks.pre_start(&mut sandbox).await?;

        // Prepare pod network if it has a private network namespace
        if !sandbox.data.netns.is_empty() {
            sandbox.prepare_network().await?;
        }

        if let Err(e) = sandbox.start().await {
            sandbox.destroy_network().await;
            return Err(e);
        }

        let sandbox_clone = sandbox_mutex.clone();
        monitor(sandbox_clone);

        if let Err(e) = sandbox.add_to_cgroup().await {
            if let Err(re) = sandbox.stop(true).await {
                warn!("roll back in add to cgroup {}", re);
                return Err(e);
            }
            sandbox.destroy_network().await;
            return Err(e);
        }

        if let Err(e) = self.hooks.post_start(&mut sandbox).await {
            if let Err(re) = sandbox.stop(true).await {
                warn!("roll back in sandbox post start {}", re);
                return Err(e);
            }
            sandbox.destroy_network().await;
            return Err(e);
        }

        if let Err(e) = sandbox.dump().await {
            if let Err(re) = sandbox.stop(true).await {
                warn!("roll back in sandbox start dump {}", re);
                return Err(e);
            }
            sandbox.destroy_network().await;
            return Err(e);
        }

        Ok(())
    }

    #[instrument(skip_all)]
    async fn update(&self, id: &str, data: SandboxData) -> Result<()> {
        let sandbox_mutex = self.sandbox(id).await?;
        let mut sandbox = sandbox_mutex.lock().await;
        sandbox.data = data;
        sandbox.dump().await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn sandbox(&self, id: &str) -> Result<Arc<Mutex<Self::Sandbox>>> {
        Ok(self
            .sandboxes
            .read()
            .await
            .get(id)
            .ok_or_else(|| Error::NotFound(id.to_string()))?
            .clone())
    }

    #[instrument(skip_all)]
    async fn stop(&self, id: &str, force: bool) -> Result<()> {
        let sandbox_mutex = match self.sandbox(id).await {
            Ok(sb) => sb,
            Err(Error::NotFound(_)) => {
                // Sandbox not found in the hashmap, nothing to stop.
                // This can happen during batch pod creation if a sandbox
                // was never fully created before KillPodSandbox is called.
                warn!("sandbox {} not found during stop, skipping", id);
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let mut sandbox = sandbox_mutex.lock().await;
        self.hooks.pre_stop(&mut sandbox).await?;
        sandbox.stop(force).await?;
        self.hooks.post_stop(&mut sandbox).await?;
        sandbox.dump().await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn delete(&self, id: &str) -> Result<()> {
        let sb_clone = self.sandboxes.read().await.clone();
        if let Some(sb_mutex) = sb_clone.get(id) {
            let mut sb = sb_mutex.lock().await;
            sb.stop(true).await?;

            // Currently only support cgroup V1, cgroup V2 is not supported now
            if !cgroups_rs::hierarchies::is_cgroup2_unified_mode() {
                // remove the sandbox cgroups
                sb.sandbox_cgroups.remove_sandbox_cgroups()?;
            }

            cleanup_mounts(&sb.base_dir).await?;
            // Should Ignore the NotFound error of base dir as it may be already deleted.
            if let Err(e) = remove_dir_all(&sb.base_dir).await {
                if e.kind() != ErrorKind::NotFound {
                    return Err(e.into());
                }
            }
        }
        self.sandboxes.write().await.remove(id);
        Ok(())
    }
}

impl<F, H> KuasarSandboxer<F, H>
where
    F: VMFactory + Sync + Send,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
    H: Hooks<F::VM> + Sync + Send,
{
    /// Start an already-created sandbox by restoring it from `snapshot_dir` rather than
    /// cold-booting the VM.  Falls back to a cold start if restore fails at any step.
    ///
    /// `template_id` is persisted to sandbox.json for audit when the restore came from
    /// the template pool.  Pass `None` for a direct/one-off snapshot restore.
    pub async fn start_from_snapshot(
        &self,
        id: &str,
        snapshot_dir: &Path,
        template_id: Option<String>,
    ) -> Result<()> {
        let sandbox_mutex = self
            .sandboxes
            .read()
            .await
            .get(id)
            .ok_or_else(|| Error::NotFound(id.to_string()))?
            .clone();

        let mut sandbox = sandbox_mutex.lock().await;

        self.hooks.pre_start(&mut sandbox).await?;

        if !sandbox.data.netns.is_empty() {
            sandbox.prepare_network().await?;
        }

        let work_dir = PathBuf::from(format!("{}/restore", sandbox.base_dir));
        let src = RestoreSource {
            snapshot_dir: snapshot_dir.to_path_buf(),
            work_dir,
            overrides: SnapshotPathOverrides {
                task_vsock: format!("{}/task.vsock", sandbox.base_dir),
                console_path: format!("/tmp/{}-task.log", sandbox.id),
            },
        };

        if let Err(e) = sandbox.start_from_snapshot(src).await {
            sandbox.destroy_network().await;
            return Err(e);
        }

        let sandbox_clone = sandbox_mutex.clone();
        monitor(sandbox_clone);

        if let Err(e) = sandbox.add_to_cgroup().await {
            if let Err(re) = sandbox.stop(true).await {
                warn!("sandbox {}: rollback add_to_cgroup (restore): {}", id, re);
                return Err(e);
            }
            sandbox.destroy_network().await;
            return Err(e);
        }

        if let Err(e) = self.hooks.post_start(&mut sandbox).await {
            if let Err(re) = sandbox.stop(true).await {
                warn!("sandbox {}: rollback post_start hook (restore): {}", id, re);
                return Err(e);
            }
            sandbox.destroy_network().await;
            return Err(e);
        }

        // Record which template this sandbox was restored from before persisting.
        sandbox.template_id = template_id;

        if let Err(e) = sandbox.dump().await {
            if let Err(re) = sandbox.stop(true).await {
                warn!("sandbox {}: rollback dump (restore): {}", id, re);
                return Err(e);
            }
            sandbox.destroy_network().await;
            return Err(e);
        }

        Ok(())
    }
}

#[async_trait]
impl<V> Sandbox for KuasarSandbox<V>
where
    V: VM + Sync + Send,
{
    type Container = KuasarContainer;

    #[instrument(skip_all)]
    fn status(&self) -> Result<SandboxStatus> {
        Ok(self.status.clone())
    }

    #[instrument(skip_all)]
    async fn ping(&self) -> Result<()> {
        self.vm.ping().await
    }

    #[instrument(skip_all)]
    async fn container(&self, id: &str) -> Result<&Self::Container> {
        let container = self
            .containers
            .get(id)
            .ok_or_else(|| Error::NotFound(id.to_string()))?;
        Ok(container)
    }

    #[instrument(skip_all)]
    async fn append_container(&mut self, id: &str, options: ContainerOption) -> Result<()> {
        let handler_chain = self.container_append_handlers(id, options)?;
        handler_chain.handle(self).await?;
        self.dump().await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn update_container(&mut self, id: &str, options: ContainerOption) -> Result<()> {
        let handler_chain = self.container_update_handlers(id, options).await?;
        handler_chain.handle(self).await?;
        self.dump().await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn remove_container(&mut self, id: &str) -> Result<()> {
        self.deference_container_storages(id).await?;

        let bundle = format!("{}/{}", self.get_sandbox_shared_path(), &id);
        if let Err(e) = tokio::fs::remove_dir_all(&*bundle).await {
            if e.kind() != ErrorKind::NotFound {
                return Err(anyhow!("failed to remove bundle {}, {}", bundle, e).into());
            }
        }
        let container = self.containers.remove(id);
        // TODO: remove processes first?
        match container {
            None => {}
            Some(c) => {
                for device_id in c.io_devices {
                    self.vm.hot_detach(&device_id).await?;
                }
            }
        }
        self.dump().await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn exit_signal(&self) -> Result<Arc<ExitSignal>> {
        Ok(self.exit_signal.clone())
    }

    #[instrument(skip_all)]
    fn get_data(&self) -> Result<SandboxData> {
        Ok(self.data.clone())
    }
}

impl<V> KuasarSandbox<V>
where
    V: VM + Snapshottable + Sync + Send,
{
    /// Restore this sandbox from a snapshot instead of cold-booting the VM.
    ///
    /// On restore failure the work_dir is cleaned up and execution falls back
    /// to a regular cold start, so the caller always gets a running sandbox.
    pub(crate) async fn start_from_snapshot(&mut self, src: RestoreSource) -> Result<()> {
        let work_dir = src.work_dir.clone();
        match self.try_restore(&src).await {
            Ok(()) => Ok(()),
            Err(e) => {
                warn!(
                    "sandbox {}: restore failed ({}), cleaning up and falling back to cold start",
                    self.id, e
                );
                if let Err(ce) = tokio::fs::remove_dir_all(&work_dir).await {
                    warn!("sandbox {}: cleanup restore work_dir: {}", self.id, ce);
                }
                self.start().await
            }
        }
    }

    async fn try_restore(&mut self, src: &RestoreSource) -> Result<()> {
        // vm.restore() launches CH, calls vm.restore API, and waits for agent ready.
        self.vm.restore(src).await?;

        let pid = self.vm.pids().vmm_pid.unwrap_or_default();

        if let Err(e) = self.init_client().await {
            if let Err(re) = self.vm.stop(true).await {
                warn!("sandbox {}: rollback init_client (restore): {}", self.id, re);
            }
            return Err(e);
        }

        if self.vm.sharefs_type() == "virtio-blk" {
            if let Err(e) = self.push_sandbox_files().await {
                if let Err(re) = self.vm.stop(true).await {
                    warn!(
                        "sandbox {}: rollback push_sandbox_files (restore): {}",
                        self.id, re
                    );
                }
                return Err(e);
            }
        }

        if let Err(e) = self.setup_sandbox().await {
            if let Err(re) = self.vm.stop(true).await {
                warn!("sandbox {}: rollback setup_sandbox (restore): {}", self.id, re);
            }
            return Err(e);
        }

        self.forward_events().await;
        self.status = SandboxStatus::Running(pid);
        Ok(())
    }
}

impl<V> KuasarSandbox<V>
where
    V: VM + Sync + Send,
{
    #[instrument(skip_all)]
    async fn dump(&self) -> Result<()> {
        let dump_data =
            serde_json::to_vec(&self).map_err(|e| anyhow!("failed to serialize sandbox, {}", e))?;
        let dump_path = format!("{}/sandbox.json", self.base_dir);
        let mut dump_file = match OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&dump_path)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                if e.kind() == ErrorKind::NotFound {
                    warn!("failed to open dump file {}, skip dump", dump_path);
                    return Ok(());
                }
                return Err(Error::IO(e));
            }
        };
        dump_file
            .write_all(dump_data.as_slice())
            .await
            .map_err(Error::IO)?;
        Ok(())
    }
}

impl<V> KuasarSandbox<V>
where
    V: VM + DeserializeOwned + Recoverable + Sync + Send,
{
    #[instrument(skip_all)]
    async fn recover<P: AsRef<Path>>(base_dir: P) -> Result<Self> {
        let start = Instant::now();
        let dump_path = base_dir.as_ref().join("sandbox.json");
        let mut dump_file = OpenOptions::new()
            .read(true)
            .open(&dump_path)
            .await
            .map_err(Error::IO)?;
        let mut content = vec![];
        dump_file
            .read_to_end(&mut content)
            .await
            .map_err(Error::IO)?;
        let mut sb = serde_json::from_slice::<KuasarSandbox<V>>(content.as_slice())
            .map_err(|e| anyhow!("failed to deserialize sandbox, {}", e))?;
        if let SandboxStatus::Running(_) = sb.status {
            if let Err(e) = sb.vm.recover().await {
                warn!("failed to recover vm {}: {}, then force kill it!", sb.id, e);
                if let Err(re) = sb.stop(true).await {
                    warn!("roll back in recover and stop: {}", re);
                    return Err(e);
                }
                return Err(e);
            };
            if let Err(e) = sb.init_client_fail_fast().await {
                if let Err(re) = sb.stop(true).await {
                    warn!("roll back in recover, init task client and stop: {}", re);
                    return Err(e);
                }
                return Err(e);
            }
            sb.sync_clock().await;
            sb.forward_events().await;
        }
        // recover the sandbox_cgroups in the sandbox object
        sb.sandbox_cgroups =
            SandboxCgroup::create_sandbox_cgroups(&sb.sandbox_cgroups.cgroup_parent_path, &sb.id)?;

        info!(
            "recover sandbox {} takes {}ms",
            sb.id,
            start.elapsed().as_millis()
        );
        Ok(sb)
    }
}

impl<V> KuasarSandbox<V>
where
    V: VM + Sync + Send,
{
    #[instrument(skip_all)]
    async fn start(&mut self) -> Result<()> {
        let pid = self.vm.start().await?;

        if let Err(e) = self.init_client().await {
            if let Err(re) = self.vm.stop(true).await {
                warn!("roll back in init task client: {}", re);
                return Err(e);
            }
            return Err(e);
        }

        // In virtio-blk mode there is no virtiofs to share sandbox config files.
        // Push them into the guest before setup_sandbox() which reads the hostname.
        if self.vm.sharefs_type() == "virtio-blk" {
            if let Err(e) = self.push_sandbox_files().await {
                if let Err(re) = self.vm.stop(true).await {
                    warn!("roll back in push sandbox files: {}", re);
                    return Err(e);
                }
                return Err(e);
            }
        }

        if let Err(e) = self.setup_sandbox().await {
            if let Err(re) = self.vm.stop(true).await {
                error!("roll back in setup sandbox client: {}", re);
                return Err(e);
            }
            return Err(e);
        }

        self.forward_events().await;

        self.status = SandboxStatus::Running(pid);
        Ok(())
    }

    #[instrument(skip_all)]
    async fn stop(&mut self, mut force: bool) -> Result<()> {
        match self.status {
            // If a sandbox is created:
            // 1. Just Created, vmm is not running: roll back and cleanup
            // 2. Created and vmm is running: roll back and cleanup
            // 3. Created and vmm is exited abnormally after running: status is Stopped
            SandboxStatus::Created => {
                // If a sandbox is in Created status, it means it was never successfully started.
                // We should treat this as a roll back and cleanup, and force kill any potential
                // vcpu or virtiofs-daemon processes.
                force = true;
            }
            SandboxStatus::Running(_) => {}
            SandboxStatus::Stopped(_, _) => {
                // Network should already be destroyed when sandbox is stopped.
                self.destroy_network().await;
                return Ok(());
            }
            _ => {
                return Err(
                    anyhow!("sandbox {} is in {:?} while stop", self.id, self.status).into(),
                );
            }
        }
        let container_ids: Vec<String> = self.containers.keys().map(|k| k.to_string()).collect();
        if force {
            for id in container_ids {
                if let Err(e) = self.remove_container(&id).await {
                    warn!("failed to remove container {} during stop, {}", id, e);
                }
            }
        } else {
            for id in container_ids {
                self.remove_container(&id).await?;
            }
        }

        self.vm.stop(force).await?;
        self.destroy_network().await;
        Ok(())
    }

    #[instrument(skip_all)]
    pub(crate) fn container_mut(&mut self, id: &str) -> Result<&mut KuasarContainer> {
        self.containers
            .get_mut(id)
            .ok_or_else(|| Error::NotFound(format!("no container with id {}", id)))
    }

    #[instrument(skip_all)]
    pub(crate) fn increment_and_get_id(&mut self) -> u32 {
        self.id_generator += 1;
        self.id_generator
    }

    #[instrument(skip_all)]
    async fn init_client(&mut self) -> Result<()> {
        let mut client_guard = self.client.lock().await;
        if client_guard.is_none() {
            let addr = self.vm.socket_address();
            if addr.is_empty() {
                return Err(anyhow!("VM address is empty").into());
            }
            let client = new_sandbox_client(&addr).await?;
            self.check_and_set_client(&mut client_guard, client).await?;
        }
        Ok(())
    }

    /// Use fail-fast connect strategy during recovery: if the guest agent
    /// socket returns a fatal error (e.g. broken pipe), bail out immediately
    /// instead of retrying until timeout.
    #[instrument(skip_all)]
    async fn init_client_fail_fast(&mut self) -> Result<()> {
        let mut client_guard = self.client.lock().await;
        if client_guard.is_none() {
            let addr = self.vm.socket_address();
            if addr.is_empty() {
                return Err(anyhow!("VM address is empty").into());
            }
            let client = new_sandbox_client_fail_fast(&addr).await?;
            self.check_and_set_client(&mut client_guard, client).await?;
        }
        Ok(())
    }

    async fn check_and_set_client(
        &self,
        guard: &mut tokio::sync::MutexGuard<'_, Option<SandboxServiceClient>>,
        client: SandboxServiceClient,
    ) -> Result<()> {
        debug!("connected to task server {}", self.id);
        client_check(&client, DEFAULT_CLIENT_CHECK_TIMEOUT).await?;
        **guard = Some(client);
        Ok(())
    }

    #[instrument(skip_all)]
    pub(crate) async fn setup_sandbox(&mut self) -> Result<()> {
        let mut req = SetupSandboxRequest::new();

        if let Some(client) = &*self.client.lock().await {
            // Set PodSandboxConfig
            if let Some(config) = &self.data.config {
                let config_str = serde_json::to_vec(config).map_err(|e| {
                    Error::Other(anyhow!(
                        "failed to marshal PodSandboxConfig to string, {:?}",
                        e
                    ))
                })?;

                let mut any = Any::new();
                any.type_url = "PodSandboxConfig".to_string();
                any.value = config_str;

                req.config = MessageField::some(any);
            }

            if let Some(network) = self.network.as_ref() {
                // Set interfaces
                req.interfaces = network.interfaces().iter().map(|x| x.into()).collect();

                // Set routes
                req.routes = network.routes().iter().map(|x| x.into()).collect();
            }

            client_setup_sandbox(client, &req).await?;
        }

        Ok(())
    }

    #[instrument(skip_all)]
    pub(crate) async fn sync_clock(&self) {
        if let Some(client) = &*self.client.lock().await {
            client_sync_clock(client, self.id.as_str(), self.exit_signal.clone());
        }
    }

    #[instrument(skip_all)]
    async fn setup_sandbox_files(&self) -> Result<()> {
        let shared_path = self.get_sandbox_shared_path();
        create_dir_all(&shared_path)
            .await
            .map_err(|e| anyhow!("create host sandbox path({}): {}", shared_path, e))?;

        // Handle hostname
        let mut hostname = get_hostname(&self.data);
        if hostname.is_empty() {
            hostname = hostname::get()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
        }
        hostname.push('\n');
        let hostname_path = Path::new(&shared_path).join(HOSTNAME_FILENAME);
        write_str_to_file(hostname_path, &hostname)
            .await
            .map_err(|e| anyhow!("write hostname: {:?}", e))?;

        // handle hosts
        let hosts_path = Path::new(&shared_path).join(HOSTS_FILENAME);
        copy(ETC_HOSTS, hosts_path)
            .await
            .map_err(|e| anyhow!("copy hosts: {}", e))?;

        // handle resolv.conf
        let resolv_path = Path::new(&shared_path).join(RESOLV_FILENAME);
        match get_dns_config(&self.data).map(|dns_config| {
            parse_dnsoptions(
                &dns_config.servers,
                &dns_config.searches,
                &dns_config.options,
            )
        }) {
            Some(resolv_content) if !resolv_content.is_empty() => {
                write_str_to_file(resolv_path, &resolv_content)
                    .await
                    .map_err(|e| anyhow!("write reslov.conf: {:?}", e))?;
            }
            _ => {
                copy(ETC_RESOLV, resolv_path)
                    .await
                    .map_err(|e| anyhow!("copy resolv.conf: {}", e))?;
            }
        }

        Ok(())
    }

    #[instrument(skip_all)]
    pub fn get_sandbox_shared_path(&self) -> String {
        format!("{}/{}", self.base_dir, SHARED_DIR_SUFFIX)
    }

    // Push sandbox config files (hostname, resolv.conf, hosts) directly into the guest
    // via exec_vm_process TTRPC calls. Used in virtio-blk mode where there is no virtiofs.
    // The three file pushes are issued concurrently after ensuring KUASAR_STATE_DIR exists.
    #[instrument(skip_all)]
    async fn push_sandbox_files(&self) -> Result<()> {
        let shared_path = self.get_sandbox_shared_path();
        let client_guard = self.client.lock().await;
        let client = match client_guard.as_ref() {
            Some(c) => c,
            None => return Ok(()),
        };
        let timeout_ns = Duration::from_secs(10).as_nanos() as i64;

        // Ensure KUASAR_STATE_DIR exists in the guest so containers can bind-mount from it
        let mut req = ExecVMProcessRequest::new();
        req.command = format!("mkdir -p {}", KUASAR_STATE_DIR);
        req.stdin = vec![];
        client
            .exec_vm_process(with_timeout(timeout_ns), &req)
            .await
            .map_err(|e| anyhow!("create kuasar state dir in guest: {}", e))?;

        // Read all config files from host shared dir
        let hostname_content = tokio::fs::read(format!("{}/{}", shared_path, HOSTNAME_FILENAME))
            .await
            .ok();
        let resolv_content = tokio::fs::read(format!("{}/{}", shared_path, RESOLV_FILENAME))
            .await
            .ok();
        let hosts_content = tokio::fs::read(format!("{}/{}", shared_path, HOSTS_FILENAME))
            .await
            .ok();

        // Issue all pushes concurrently to reduce sandbox start latency
        let push_hostname = async {
            if let Some(content) = hostname_content {
                let mut req = ExecVMProcessRequest::new();
                req.command = format!("cat > {}/{}", KUASAR_STATE_DIR, HOSTNAME_FILENAME);
                req.stdin = content;
                client
                    .exec_vm_process(with_timeout(timeout_ns), &req)
                    .await
                    .map_err(|e| anyhow!("push hostname to guest: {}", e))?;
            }
            Ok::<(), anyhow::Error>(())
        };

        let push_resolv = async {
            if let Some(content) = resolv_content {
                let mut req = ExecVMProcessRequest::new();
                req.command = format!(
                    "cat > {}/{} && mount --bind {}/{} /etc/resolv.conf",
                    KUASAR_STATE_DIR, RESOLV_FILENAME, KUASAR_STATE_DIR, RESOLV_FILENAME
                );
                req.stdin = content;
                client
                    .exec_vm_process(with_timeout(timeout_ns), &req)
                    .await
                    .map_err(|e| anyhow!("push resolv.conf to guest: {}", e))?;
            }
            Ok::<(), anyhow::Error>(())
        };

        let push_hosts = async {
            if let Some(content) = hosts_content {
                let mut req = ExecVMProcessRequest::new();
                req.command = format!("cat > {}/{}", KUASAR_STATE_DIR, HOSTS_FILENAME);
                req.stdin = content;
                client
                    .exec_vm_process(with_timeout(timeout_ns), &req)
                    .await
                    .map_err(|e| anyhow!("push hosts to guest: {}", e))?;
            }
            Ok::<(), anyhow::Error>(())
        };

        let (r1, r2, r3) = tokio::join!(push_hostname, push_resolv, push_hosts);
        r1.map_err(|e| containerd_sandbox::error::Error::Other(e))?;
        r2.map_err(|e| containerd_sandbox::error::Error::Other(e))?;
        r3.map_err(|e| containerd_sandbox::error::Error::Other(e))?;

        Ok(())
    }

    #[instrument(skip_all)]
    pub async fn prepare_network(&mut self) -> Result<()> {
        // get vcpu for interface queue, at least one vcpu
        let mut vcpu = 1;
        if let Some(resources) = get_resources(&self.data) {
            if resources.cpu_period > 0 && resources.cpu_quota > 0 {
                // get ceil of cpus if it is not integer
                let base = (resources.cpu_quota as f64 / resources.cpu_period as f64).ceil();
                vcpu = base as u32;
            }
        }

        let network_config = NetworkConfig {
            netns: self.data.netns.to_string(),
            sandbox_id: self.id.to_string(),
            queue: vcpu,
        };
        let network = Network::new(network_config).await?;
        network.attach_to(self).await?;
        Ok(())
    }

    //  If a sandbox is still running, destroy network may hang with its running
    #[instrument(skip_all)]
    pub async fn destroy_network(&mut self) {
        // Network should be destroyed only once, take it out here.
        if let Some(mut network) = self.network.take() {
            network.destroy().await;
        }
    }

    #[instrument(skip_all)]
    pub async fn add_to_cgroup(&self) -> Result<()> {
        // Currently only support cgroup V1, cgroup V2 is not supported now
        if !cgroups_rs::hierarchies::is_cgroup2_unified_mode() {
            // add vmm process into sandbox cgroup
            if let SandboxStatus::Running(vmm_pid) = self.status {
                let vcpu_threads = self.vm.vcpus().await?;
                debug!(
                    "vmm process pid: {}, vcpu threads pid: {:?}",
                    vmm_pid, vcpu_threads
                );
                self.sandbox_cgroups
                    .add_process_into_sandbox_cgroups(vmm_pid, Some(vcpu_threads))?;
                // move all vmm-related process into sandbox cgroup
                for pid in self.vm.pids().affiliated_pids {
                    self.sandbox_cgroups
                        .add_process_into_sandbox_cgroups(pid, None)?;
                }
            } else {
                return Err(Error::Other(anyhow!(
                    "sandbox status is not Running after started!"
                )));
            }
        }
        Ok(())
    }

    pub(crate) async fn forward_events(&mut self) {
        if let Some(client) = &*self.client.lock().await {
            let client = client.clone();
            let exit_signal = self.exit_signal.clone();
            tokio::spawn(async move {
                let fut = async {
                    loop {
                        match client.get_events(with_timeout(0), &Empty::new()).await {
                            Ok(resp) => {
                                if let Err(e) =
                                    crate::client::publish_event(convert_envelope(resp)).await
                                {
                                    error!("{}", e);
                                }
                            }
                            Err(err) => {
                                // if sandbox was closed, will get error Socket("early eof"),
                                // so we should handle errors except this unexpected EOF error.
                                if let ttrpc::error::Error::Socket(s) = &err {
                                    if s.contains("early eof") {
                                        break;
                                    }
                                }
                                error!("failed to get oom event error {:?}", err);
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
}

// parse_dnsoptions parse DNS options into resolv.conf format content,
// if none option is specified, will return empty with no error.
fn parse_dnsoptions(servers: &[String], searches: &[String], options: &[String]) -> String {
    let mut resolv_content = String::new();

    if !searches.is_empty() {
        resolv_content.push_str(&format!("search {}\n", searches.join(" ")));
    }

    if !servers.is_empty() {
        resolv_content.push_str(&format!("nameserver {}\n", servers.join("\nnameserver ")));
    }

    if !options.is_empty() {
        resolv_content.push_str(&format!("options {}\n", options.join(" ")));
    }

    resolv_content
}

pub fn has_shared_pid_namespace(data: &SandboxData) -> bool {
    if let Some(conf) = &data.config {
        if let Some(pid_ns_mode) = conf
            .linux
            .as_ref()
            .and_then(|l| l.security_context.as_ref())
            .and_then(|s| s.namespace_options.as_ref())
            .map(|n| n.pid())
        {
            return pid_ns_mode == NamespaceMode::Pod;
        }
    }
    false
}

fn convert_envelope(envelope: vmm_common::api::events::Envelope) -> Envelope {
    Envelope {
        timestamp: envelope.timestamp,
        namespace: envelope.namespace,
        topic: envelope.topic,
        event: envelope.event,
        special_fields: protobuf::SpecialFields::default(),
    }
}

#[derive(Default, Debug, Deserialize)]
pub struct SandboxConfig {
    #[serde(default)]
    pub log_level: String,
    #[serde(default)]
    pub enable_tracing: bool,
}

impl SandboxConfig {
    pub fn log_level(&self) -> String {
        self.log_level.to_string()
    }

    pub fn enable_tracing(&self) -> bool {
        self.enable_tracing
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticDeviceSpec {
    #[serde(default)]
    pub(crate) _host_path: Vec<String>,
    #[serde(default)]
    pub(crate) _bdf: Vec<String>,
}

fn monitor<V: VM + 'static>(sandbox_mutex: Arc<Mutex<KuasarSandbox<V>>>) {
    tokio::spawn(async move {
        let mut rx = {
            let sandbox = sandbox_mutex.lock().await;
            if let SandboxStatus::Running(_) = sandbox.status.clone() {
                if let Some(rx) = sandbox.vm.wait_channel().await {
                    rx
                } else {
                    error!("can not get wait channel when sandbox is running");
                    return;
                }
            } else {
                info!(
                    "sandbox {} is {:?} when monitor",
                    sandbox.id, sandbox.status
                );
                return;
            }
        };

        let (code, ts) = *rx.borrow();
        if ts == 0 {
            rx.changed().await.unwrap_or_default();
            let (code, ts) = *rx.borrow();
            let mut sandbox = sandbox_mutex.lock().await;
            info!("monitor sandbox {} terminated", sandbox.id);
            sandbox.status = SandboxStatus::Stopped(code, ts);
            sandbox.exit_signal.signal();
            // Network destruction should be done after sandbox status changed from running.
            sandbox.destroy_network().await;
            sandbox
                .dump()
                .await
                .map_err(|e| error!("dump sandbox {} in monitor: {}", sandbox.id, e))
                .unwrap_or_default();
        } else {
            let mut sandbox = sandbox_mutex.lock().await;
            info!("sandbox {} already terminated before monit it", sandbox.id);
            sandbox.status = SandboxStatus::Stopped(code, ts);
            sandbox.exit_signal.signal();
            // Network destruction should be done after sandbox status changed from running.
            sandbox.destroy_network().await;
            sandbox
                .dump()
                .await
                .map_err(|e| error!("dump sandbox {} in monitor: {}", sandbox.id, e))
                .unwrap_or_default();
        }
    });
}

// ---------------------------------------------------------------------------
// Template pool: create_template + start_with_template
// ---------------------------------------------------------------------------

/// Boot a fresh VM, wait for the guest agent, snapshot, then stop the VM.
/// The resulting snapshot is added to the template pool and stored on disk.
pub(crate) async fn create_template_worker<F>(
    factory: F,
    pool: Arc<TemplatePool>,
    req: CreateTemplateRequest,
) -> Result<PooledTemplate>
where
    F: VMFactory,
    F::VM: VM + Snapshottable + Sync + Send,
{
    let template_base = pool.store_dir.join(&req.id);
    let vm_base = template_base.join("vm");

    let result = create_template_inner(&factory, &pool, &req, &template_base, &vm_base).await;

    if let Err(ref e) = result {
        warn!("template {}: creation failed ({}), cleaning up", req.id, e);
        if let Err(ce) = tokio::fs::remove_dir_all(&template_base).await {
            warn!("template {}: cleanup on failure: {}", req.id, ce);
        }
    }
    result
}

async fn create_template_inner<F>(
    factory: &F,
    pool: &Arc<TemplatePool>,
    req: &CreateTemplateRequest,
    template_base: &PathBuf,
    vm_base: &PathBuf,
) -> Result<PooledTemplate>
where
    F: VMFactory,
    F::VM: VM + Snapshottable + Sync + Send,
{
    tokio::fs::create_dir_all(vm_base)
        .await
        .map_err(|e| anyhow!("create template vm dir: {}", e))?;

    let sandbox_opt = SandboxOption {
        base_dir: vm_base.to_string_lossy().to_string(),
        sandbox: SandboxData {
            id: req.id.clone(),
            ..Default::default()
        },
    };

    let mut vm = factory.create_vm(&req.id, &sandbox_opt).await?;
    vm.start().await?;

    // Wait for the guest agent to be ready, then flush fs journals and drop page
    // cache to produce a clean, compact snapshot.
    let agent_client = new_sandbox_client_fail_fast(&vm.socket_address())
        .await
        .map_err(|e| anyhow!("template {}: connect to agent: {}", req.id, e))?;
    let pre_snap_timeout_ns = Duration::from_secs(10).as_nanos() as i64;
    for cmd in &["sync", "echo 1 > /proc/sys/vm/drop_caches"] {
        let mut exec_req = ExecVMProcessRequest::new();
        exec_req.command = cmd.to_string();
        exec_req.stdin = vec![];
        if let Err(e) = agent_client
            .exec_vm_process(with_timeout(pre_snap_timeout_ns), &exec_req)
            .await
        {
            warn!("template {}: pre-snapshot '{}' failed: {}", req.id, cmd, e);
        }
    }

    let snapshot_dir = template_base.join("snapshot");
    tokio::fs::create_dir_all(&snapshot_dir)
        .await
        .map_err(|e| anyhow!("create snapshot dir: {}", e))?;

    let snap_start = Instant::now();
    let meta: SnapshotMeta = vm
        .snapshot(&snapshot_dir)
        .await
        .map_err(|e| anyhow!("template {}: snapshot failed: {}", req.id, e))?;
    info!(
        "template {}: snapshot captured in {:.3}s",
        req.id,
        snap_start.elapsed().as_secs_f64()
    );

    if let Err(e) = vm.stop(false).await {
        warn!("template {}: stop after snapshot: {}", req.id, e);
    }

    // Remove the temporary VM directory (sockets, sandbox.json, etc.); only the
    // snapshot directory under template_base is retained.
    if let Err(e) = tokio::fs::remove_dir_all(vm_base).await {
        warn!("template {}: cleanup vm dir: {}", req.id, e);
    }

    let key = TemplateKey::new(factory.image_path(), factory.vcpus(), factory.memory_mb());
    let tmpl = PooledTemplate::new(
        &req.id,
        key,
        meta.snapshot_dir,
        factory.image_path(),
        factory.kernel_path(),
        factory.vcpus(),
        factory.memory_mb(),
        meta.original_task_vsock,
        meta.original_console_path,
    );

    pool.add(tmpl.clone()).await?;
    info!(
        "template {}: added to pool (pool_depth={})",
        req.id,
        pool.depth(&tmpl.key).await
    );
    Ok(tmpl)
}

/// Boot a fresh VM, optionally start a container from `image` via the shimv2 task API,
/// then snapshot the VM and register the result as a template.
///
/// If `image` is empty, the VM is snapshotted immediately after the guest agent is ready
/// (equivalent to the old "create-fresh" behaviour).
pub(crate) async fn create_template_from_image_worker<F>(
    factory: F,
    pool: Arc<TemplatePool>,
    req: CreateTemplateRequest,
    image: String,
) -> Result<PooledTemplate>
where
    F: VMFactory,
    F::VM: VM + Snapshottable + Sync + Send,
{
    let template_base = pool.store_dir.join(&req.id);
    let vm_base = template_base.join("vm");

    let result =
        create_template_from_image_inner(&factory, &pool, &req, &image, &template_base, &vm_base)
            .await;

    if let Err(ref e) = result {
        warn!("template {}: creation failed ({}), cleaning up", req.id, e);
        if let Err(ce) = tokio::fs::remove_dir_all(&template_base).await {
            warn!("template {}: cleanup on failure: {}", req.id, ce);
        }
    }
    result
}

async fn create_template_from_image_inner<F>(
    factory: &F,
    pool: &Arc<TemplatePool>,
    req: &CreateTemplateRequest,
    image: &str,
    template_base: &PathBuf,
    vm_base: &PathBuf,
) -> Result<PooledTemplate>
where
    F: VMFactory,
    F::VM: VM + Snapshottable + Sync + Send,
{
    tokio::fs::create_dir_all(vm_base)
        .await
        .map_err(|e| anyhow!("create template vm dir: {}", e))?;

    let sandbox_opt = SandboxOption {
        base_dir: vm_base.to_string_lossy().to_string(),
        sandbox: SandboxData {
            id: req.id.clone(),
            ..Default::default()
        },
    };

    let mut vm = factory.create_vm(&req.id, &sandbox_opt).await?;
    vm.start().await?;

    let agent_addr = vm.socket_address();
    let agent_client = new_sandbox_client_fail_fast(&agent_addr)
        .await
        .map_err(|e| anyhow!("template {}: connect to agent: {}", req.id, e))?;

    if !image.is_empty() {
        start_warmup_container(req, image, &agent_addr, &agent_client).await?;
    }

    // Flush fs journals and drop page cache before snapshot.
    let pre_snap_timeout_ns = Duration::from_secs(10).as_nanos() as i64;
    for cmd in &["sync", "echo 1 > /proc/sys/vm/drop_caches"] {
        let mut exec_req = ExecVMProcessRequest::new();
        exec_req.command = cmd.to_string();
        exec_req.stdin = vec![];
        if let Err(e) = agent_client
            .exec_vm_process(with_timeout(pre_snap_timeout_ns), &exec_req)
            .await
        {
            warn!("template {}: pre-snapshot '{}' failed: {}", req.id, cmd, e);
        }
    }

    let snapshot_dir = template_base.join("snapshot");
    tokio::fs::create_dir_all(&snapshot_dir)
        .await
        .map_err(|e| anyhow!("create snapshot dir: {}", e))?;

    let snap_start = Instant::now();
    let meta: SnapshotMeta = vm
        .snapshot(&snapshot_dir)
        .await
        .map_err(|e| anyhow!("template {}: snapshot failed: {}", req.id, e))?;
    info!(
        "template {}: snapshot captured in {:.3}s",
        req.id,
        snap_start.elapsed().as_secs_f64()
    );

    if let Err(e) = vm.stop(false).await {
        warn!("template {}: stop after snapshot: {}", req.id, e);
    }

    if let Err(e) = tokio::fs::remove_dir_all(vm_base).await {
        warn!("template {}: cleanup vm dir: {}", req.id, e);
    }

    let key = TemplateKey::new(factory.image_path(), factory.vcpus(), factory.memory_mb());
    let tmpl = PooledTemplate::new(
        &req.id,
        key,
        meta.snapshot_dir,
        factory.image_path(),
        factory.kernel_path(),
        factory.vcpus(),
        factory.memory_mb(),
        meta.original_task_vsock,
        meta.original_console_path,
    );

    pool.add(tmpl.clone()).await?;
    info!(
        "template {}: added to pool (pool_depth={})",
        req.id,
        pool.depth(&tmpl.key).await
    );
    Ok(tmpl)
}

/// Push a minimal OCI bundle into the VM via exec_vm_process and start a warmup
/// container via the shimv2 task API.  Works in virtio-blk mode (no shared fs).
async fn start_warmup_container(
    req: &CreateTemplateRequest,
    image: &str,
    agent_addr: &str,
    agent_client: &SandboxServiceClient,
) -> Result<()> {
    let container_id = format!("warmup-{}", &req.id);
    let bundle_guest = format!("{}/{}", KUASAR_STATE_DIR, container_id);
    let push_timeout_ns = Duration::from_secs(10).as_nanos() as i64;

    // Create bundle dir inside the VM.
    let mut mkdir_req = ExecVMProcessRequest::new();
    mkdir_req.command = format!("mkdir -p {}", bundle_guest);
    mkdir_req.stdin = vec![];
    agent_client
        .exec_vm_process(with_timeout(push_timeout_ns), &mkdir_req)
        .await
        .map_err(|e| anyhow!("template {}: mkdir bundle dir: {}", req.id, e))?;

    // Push config.json into the VM.
    let spec = minimal_warmup_oci_spec(image);
    let mut push_req = ExecVMProcessRequest::new();
    push_req.command = format!("cat > {}/config.json", bundle_guest);
    push_req.stdin = spec.into_bytes();
    agent_client
        .exec_vm_process(with_timeout(push_timeout_ns), &push_req)
        .await
        .map_err(|e| anyhow!("template {}: push config.json: {}", req.id, e))?;

    // Connect to the vmm-task service (same vsock port 1024 as the agent).
    let task_client = new_task_client(agent_addr)
        .await
        .map_err(|e| anyhow!("template {}: connect to task service: {}", req.id, e))?;

    let ctx_ns = Duration::from_secs(30).as_nanos() as i64;

    let mut create_req = TaskCreateRequest::new();
    create_req.id = container_id.clone();
    create_req.stdin = "/dev/null".to_string();
    create_req.stdout = "/dev/null".to_string();
    create_req.stderr = "/dev/null".to_string();
    create_req.terminal = false;

    task_client
        .create(with_timeout(ctx_ns), &create_req)
        .await
        .map_err(|e| anyhow!("template {}: task.Create failed: {}", req.id, e))?;

    let mut start_req = TaskStartRequest::new();
    start_req.id = container_id.clone();

    task_client
        .start(with_timeout(ctx_ns), &start_req)
        .await
        .map_err(|e| anyhow!("template {}: task.Start failed: {}", req.id, e))?;

    info!(
        "template {}: warmup container {} started (image={})",
        req.id, container_id, image
    );
    Ok(())
}

/// Minimal OCI spec that runs `sleep infinity` inside the VM's own rootfs.
/// The `io.kuasar.storages` annotation prevents vmm-task from looking for a
/// storages file that doesn't exist.
fn minimal_warmup_oci_spec(image: &str) -> String {
    let image_json = serde_json::to_string(image).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        r#"{{
  "ociVersion": "1.0.0",
  "process": {{
    "terminal": false,
    "user": {{"uid": 0, "gid": 0}},
    "args": ["/bin/sh", "-c", "exec sleep infinity"],
    "env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
    "cwd": "/"
  }},
  "root": {{"path": "/", "readonly": false}},
  "linux": {{"namespaces": []}},
  "annotations": {{
    "io.kuasar.storages": "[]",
    "io.kuasar.template.image": {image_json}
  }}
}}"#
    )
}

impl<F, H> KuasarSandboxer<F, H>
where
    F: VMFactory + Clone + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
    H: Hooks<F::VM> + Sync + Send,
{
    /// Initialize the template pool from `store_dir`, rehydrating any previously
    /// persisted templates.
    ///
    /// Must be called before `create_template` or `start_with_template`.
    pub async fn init_template_pool(
        &mut self,
        store_dir: impl Into<PathBuf>,
        max_per_key: usize,
    ) -> Result<()> {
        let store_dir = store_dir.into();
        let pool = TemplatePool::load_from_disk(store_dir, max_per_key).await?;
        let total = pool.total_depth().await;
        let keys = pool.key_count().await;
        info!(
            "template pool initialized: {} templates across {} key(s) (store={})",
            total,
            keys,
            pool.store_dir.display(),
        );
        self.template_pool = Some(pool);
        Ok(())
    }

    /// Return a handle that exposes template-management operations over the admin
    /// socket without requiring ownership of the whole sandboxer.
    ///
    /// Returns `None` when the template pool has not been initialized.
    pub fn admin_handle(&self) -> Option<crate::admin::TemplateAdminHandle<F>> {
        self.template_pool.as_ref().map(|pool| crate::admin::TemplateAdminHandle {
            factory: self.factory.clone(),
            sandboxes: self.sandboxes.clone(),
            pool: pool.clone(),
        })
    }

    /// Boot a fresh VM, snapshot it once the guest agent is ready, stop it, and
    /// add the resulting snapshot to the template pool.
    pub async fn create_template(&self, req: CreateTemplateRequest) -> Result<PooledTemplate> {
        let pool = self
            .template_pool
            .as_ref()
            .ok_or_else(|| anyhow!("template pool not initialized"))?
            .clone();

        info!("creating template {}", req.id);
        create_template_worker(self.factory.clone(), pool, req).await
    }

    /// Try to start an already-created sandbox from the template pool.
    ///
    /// If a matching template exists in the pool it is consumed and the sandbox
    /// is restored from that snapshot (fast path, typically < 500 ms).
    /// On any restore failure the template is released back and the call falls
    /// back to a regular cold start so the caller always gets a running sandbox.
    ///
    /// If the pool is empty or not initialized, a cold start is performed.
    ///
    /// After a successful template-hit restore a background task is spawned to
    /// refill the pool with a fresh template.
    pub async fn start_with_template(&self, id: &str, key: &TemplateKey) -> Result<()> {
        let pool = match &self.template_pool {
            Some(p) => p.clone(),
            None => return self.start_cold(id).await,
        };

        let tmpl = pool.acquire(key).await;
        match tmpl {
            None => {
                pool.metrics.record_miss();
                info!("template pool miss for sandbox {}, cold-starting", id);
                self.start_cold(id).await
            }
            Some(tmpl) => {
                // Validate snapshot files before acquiring the restore semaphore slot.
                let state_json = tmpl.snapshot_dir.join("state.json");
                let snapshot_ok = tokio::fs::metadata(&state_json).await.is_ok()
                    && tokio::fs::metadata(&tmpl.pmem_path).await.is_ok();
                if !snapshot_ok {
                    warn!(
                        "sandbox {}: template {} snapshot files missing, releasing and cold-starting",
                        id, tmpl.id
                    );
                    pool.release(tmpl).await;
                    pool.metrics.record_miss();
                    return self.start_cold(id).await;
                }

                // Limit concurrent VM restores to cap host memory pressure during bursts.
                let _permit = RESTORE_SEMAPHORE.acquire().await.unwrap();

                let restore_start = Instant::now();
                let template_id = tmpl.id.clone();
                info!(
                    "template pool hit for sandbox {} (template {}), restoring",
                    id, tmpl.id
                );
                match self
                    .start_from_snapshot(id, &tmpl.snapshot_dir, Some(template_id.clone()))
                    .await
                {
                    Ok(()) => {
                        let ms = restore_start.elapsed().as_millis() as u64;
                        pool.metrics.record_hit(ms);

                        // Log aggregate pool metrics every 10 successful restores.
                        let hits = pool
                            .metrics
                            .pool_hits
                            .load(std::sync::atomic::Ordering::Relaxed);
                        if hits > 0 && hits % 10 == 0 {
                            info!(
                                "template pool: hit_rate={:.1}%, avg_restore={}ms, hits={}, misses={}",
                                pool.metrics.hit_rate() * 100.0,
                                pool.metrics.avg_restore_ms() as u64,
                                hits,
                                pool.metrics
                                    .pool_misses
                                    .load(std::sync::atomic::Ordering::Relaxed),
                            );
                        }

                        info!(
                            "sandbox {} restored from template {} in {}ms (pool hit_rate={:.1}%)",
                            id,
                            template_id,
                            ms,
                            pool.metrics.hit_rate() * 100.0
                        );
                        Ok(())
                    }
                    Err(e) => {
                        warn!(
                            "sandbox {}: template restore failed ({}), releasing template and cold-starting",
                            id, e
                        );
                        pool.release(tmpl).await;
                        pool.metrics.record_miss();
                        self.start_cold(id).await
                    }
                }
            }
        }
    }

    /// Internal helper: run the standard `Sandboxer::start` flow without
    /// template logic.  Needed because `start_with_template` can't call
    /// `Sandboxer::start` directly (trait method dispatch requires `Self: Sized`
    /// and additional async machinery; the impl below avoids that complexity).
    async fn start_cold(&self, id: &str) -> Result<()> {
        let sandbox_mutex = self
            .sandboxes
            .read()
            .await
            .get(id)
            .ok_or_else(|| containerd_sandbox::error::Error::NotFound(id.to_string()))?
            .clone();

        let mut sandbox = sandbox_mutex.lock().await;
        self.hooks.pre_start(&mut sandbox).await?;

        if !sandbox.data.netns.is_empty() {
            sandbox.prepare_network().await?;
        }

        if let Err(e) = sandbox.start().await {
            sandbox.destroy_network().await;
            return Err(e);
        }

        let sandbox_clone = sandbox_mutex.clone();
        monitor(sandbox_clone);

        if let Err(e) = sandbox.add_to_cgroup().await {
            if let Err(re) = sandbox.stop(true).await {
                warn!("sandbox {}: rollback add_to_cgroup (cold): {}", id, re);
                return Err(e);
            }
            sandbox.destroy_network().await;
            return Err(e);
        }

        if let Err(e) = self.hooks.post_start(&mut sandbox).await {
            if let Err(re) = sandbox.stop(true).await {
                warn!("sandbox {}: rollback post_start hook (cold): {}", id, re);
                return Err(e);
            }
            sandbox.destroy_network().await;
            return Err(e);
        }

        if let Err(e) = sandbox.dump().await {
            if let Err(re) = sandbox.stop(true).await {
                warn!("sandbox {}: rollback dump (cold): {}", id, re);
                return Err(e);
            }
            sandbox.destroy_network().await;
            return Err(e);
        }

        Ok(())
    }
}


#[cfg(test)]
mod tests {
    mod recovery {
        use std::{collections::HashMap, path::Path, sync::Arc};

        use async_trait::async_trait;
        use containerd_sandbox::{
            data::SandboxData, error::Result, signal::ExitSignal, SandboxOption, SandboxStatus,
        };
        use serde::{Deserialize, Serialize};
        use temp_dir::TempDir;
        use tokio::sync::Mutex;
        use vmm_common::storage::Storage;

        use crate::{
            cgroup::SandboxCgroup,
            container::KuasarContainer,
            device::{BusType, DeviceInfo},
            sandbox::{KuasarSandbox, KuasarSandboxer, SandboxConfig},
            vm::{Hooks, Pids, Recoverable, VMFactory, VcpuThreads, VM},
        };

        #[derive(Default, Serialize, Deserialize)]
        struct MockVM {
            fail_recover: bool,
            socket_address: String,
            stop_marker: String,
        }

        #[async_trait]
        impl VM for MockVM {
            async fn start(&mut self) -> Result<u32> {
                Ok(0)
            }

            async fn stop(&mut self, force: bool) -> Result<()> {
                let content = if force { "force" } else { "graceful" };
                if !self.stop_marker.is_empty() {
                    tokio::fs::write(&self.stop_marker, content)
                        .await
                        .map_err(containerd_sandbox::error::Error::IO)?;
                }
                Ok(())
            }

            async fn attach(&mut self, _device_info: DeviceInfo) -> Result<()> {
                Ok(())
            }

            async fn hot_attach(&mut self, _device_info: DeviceInfo) -> Result<(BusType, String)> {
                Ok((BusType::PCI, String::new()))
            }

            async fn hot_detach(&mut self, _id: &str) -> Result<()> {
                Ok(())
            }

            async fn ping(&self) -> Result<()> {
                Ok(())
            }

            fn socket_address(&self) -> String {
                self.socket_address.clone()
            }

            async fn wait_channel(&self) -> Option<tokio::sync::watch::Receiver<(u32, i128)>> {
                None
            }

            async fn vcpus(&self) -> Result<VcpuThreads> {
                Ok(VcpuThreads {
                    vcpus: HashMap::new(),
                })
            }

            fn pids(&self) -> Pids {
                Pids::default()
            }
        }

        #[async_trait]
        impl Recoverable for MockVM {
            async fn recover(&mut self) -> Result<()> {
                if self.fail_recover {
                    return Err(containerd_sandbox::error::Error::InvalidArgument(
                        "mock recover failure".to_string(),
                    ));
                }
                Ok(())
            }
        }

        struct MockFactory;

        #[async_trait]
        impl VMFactory for MockFactory {
            type VM = MockVM;
            type Config = ();

            fn new(_: Self::Config) -> Self {
                Self
            }

            async fn create_vm(&self, _: &str, _: &SandboxOption) -> Result<Self::VM> {
                Ok(MockVM::default())
            }
        }

        struct MockHooks;

        #[async_trait]
        impl Hooks<MockVM> for MockHooks {}

        fn mock_sandbox(
            base_dir: &str,
            status: SandboxStatus,
            vm: MockVM,
        ) -> KuasarSandbox<MockVM> {
            KuasarSandbox {
                vm,
                id: "test-sandbox".to_string(),
                status,
                base_dir: base_dir.to_string(),
                data: SandboxData::default(),
                containers: HashMap::<String, KuasarContainer>::new(),
                storages: Vec::<Storage>::new(),
                id_generator: 0,
                network: None,
                client: Arc::new(Mutex::new(None)),
                exit_signal: Arc::new(ExitSignal::default()),
                sandbox_cgroups: SandboxCgroup::default(),
                template_id: None,
            }
        }

        async fn write_dump<P: AsRef<Path>>(dir: P, sandbox: &KuasarSandbox<MockVM>) {
            let dump_path = dir.as_ref().join("sandbox.json");
            let content = serde_json::to_vec(sandbox).unwrap();
            tokio::fs::write(dump_path, content).await.unwrap();
        }

        #[tokio::test]
        async fn test_recover_fast_fail_paths_force_stop_vm() {
            let cases = vec![
                (
                    "recover-error",
                    MockVM {
                        fail_recover: true,
                        socket_address: "vsock://ignored".to_string(),
                        stop_marker: String::new(),
                    },
                ),
                (
                    "init-client-error",
                    MockVM {
                        fail_recover: false,
                        socket_address: String::new(),
                        stop_marker: String::new(),
                    },
                ),
            ];

            for (name, mut vm) in cases {
                let temp_dir = TempDir::new().unwrap();
                let stop_marker = temp_dir.path().join(format!("{name}.stop"));
                vm.stop_marker = stop_marker.to_string_lossy().to_string();

                let sandbox = mock_sandbox(
                    temp_dir.path().to_str().unwrap(),
                    SandboxStatus::Running(42),
                    vm,
                );
                write_dump(temp_dir.path(), &sandbox).await;

                let result = KuasarSandbox::<MockVM>::recover(temp_dir.path()).await;
                assert!(result.is_err(), "{name} should fail fast");

                let marker_content = tokio::fs::read_to_string(&stop_marker).await.unwrap();
                assert_eq!(marker_content, "force", "{name} should force stop the VM");
            }
        }

        #[tokio::test]
        async fn test_sandboxer_recover_cleans_failed_sandbox_dir() {
            let root = TempDir::new().unwrap();
            let failed_dir = root.path().join("failed");
            tokio::fs::create_dir_all(&failed_dir).await.unwrap();
            let stop_marker = root.path().join("failed.stop");

            let sandbox = mock_sandbox(
                failed_dir.to_str().unwrap(),
                SandboxStatus::Running(7),
                MockVM {
                    fail_recover: true,
                    socket_address: "vsock://ignored".to_string(),
                    stop_marker: stop_marker.to_string_lossy().to_string(),
                },
            );
            write_dump(&failed_dir, &sandbox).await;

            let mut sandboxer = KuasarSandboxer::<MockFactory, MockHooks>::new(
                SandboxConfig::default(),
                (),
                MockHooks,
            );
            sandboxer.recover(root.path().to_str().unwrap()).await;

            assert!(
                tokio::fs::metadata(&failed_dir).await.is_err(),
                "failed sandbox dir should be removed after fast failure"
            );
            assert_eq!(
                tokio::fs::read_to_string(&stop_marker).await.unwrap(),
                "force"
            );
            assert!(sandboxer.sandboxes.read().await.is_empty());
        }
    }

    mod dns {
        use crate::sandbox::parse_dnsoptions;

        #[derive(Default)]
        struct DnsConfig {
            servers: Vec<String>,
            searches: Vec<String>,
            options: Vec<String>,
        }

        #[test]
        fn test_parse_empty_dns_option() {
            let dns_test = DnsConfig::default();
            let resolv_content =
                parse_dnsoptions(&dns_test.servers, &dns_test.searches, &dns_test.options);
            assert!(resolv_content.is_empty())
        }

        #[test]
        fn test_parse_non_empty_dns_option() {
            let dns_test = DnsConfig {
                servers: vec!["8.8.8.8", "server.google.com"]
                    .into_iter()
                    .map(String::from)
                    .collect(),
                searches: vec![
                    "server0.google.com",
                    "server1.google.com",
                    "server2.google.com",
                    "server3.google.com",
                    "server4.google.com",
                    "server5.google.com",
                    "server6.google.com",
                ]
                .into_iter()
                .map(String::from)
                .collect(),
                options: vec!["timeout:1"].into_iter().map(String::from).collect(),
            };
            let expected_content = "search server0.google.com server1.google.com server2.google.com server3.google.com server4.google.com server5.google.com server6.google.com
nameserver 8.8.8.8
nameserver server.google.com
options timeout:1
".to_string();
            let resolv_content =
                parse_dnsoptions(&dns_test.servers, &dns_test.searches, &dns_test.options);
            assert!(!resolv_content.is_empty());
            assert_eq!(resolv_content, expected_content)
        }
    }
}
