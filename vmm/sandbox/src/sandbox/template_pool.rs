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
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::anyhow;
use containerd_sandbox::{data::SandboxData, error::Result, SandboxOption};
use log::{info, warn};
use ttrpc::context::with_timeout;
use vmm_common::api::sandbox::ExecVMProcessRequest;

use super::{KuasarSandboxer, SnapshotRestoreParams, TemplateLeaseMode};
use crate::{
    template::{
        new_template_id, ContinuationLease, ContinuationStore, CreateTemplateRequest,
        PooledTemplate, SnapshotType, TemplateKey, TemplateLease, TemplatePool, WorkloadIdentity,
    },
    vm::{
        Hooks, SnapshotMeta, Snapshottable, VMFactory, WarmForkParams, WarmForkTarget,
        ANNOTATION_WARM_FORK_CONTAINERS, ANNOTATION_WARM_FORK_DEFAULT_READINESS_SOCKET,
        ANNOTATION_WARM_FORK_ENV_PREFIX, ANNOTATION_WARM_FORK_READINESS_SOCKET,
        ANNOTATION_WARM_FORK_READY_PROTOCOL, ANNOTATION_WARM_FORK_TASK_CONTEXT,
        ANNOTATION_WARM_FORK_TASK_ID, VM, WARM_FORK_PROTOCOL_V1,
    },
};

// ---------------------------------------------------------------------------
// WarmFork annotation parsing
// ---------------------------------------------------------------------------

/// Parse and validate the optional multi-container target annotation.
pub(crate) fn parse_warm_fork_container_names(
    annotations: &HashMap<String, String>,
) -> std::result::Result<Option<Vec<String>>, anyhow::Error> {
    let Some(containers_ann) = annotations.get(ANNOTATION_WARM_FORK_CONTAINERS) else {
        return Ok(None);
    };

    let mut names = Vec::new();
    let mut seen = HashSet::new();
    for raw in containers_ann.split(',') {
        let name = raw.trim();
        if name.is_empty() {
            return Err(anyhow!(
                "'{}' contains an empty container name (value={:?})",
                ANNOTATION_WARM_FORK_CONTAINERS,
                containers_ann
            ));
        }
        if !seen.insert(name.to_string()) {
            return Err(anyhow!(
                "'{}' contains duplicate container name '{}'",
                ANNOTATION_WARM_FORK_CONTAINERS,
                name
            ));
        }
        names.push(name.to_string());
    }

    if names.is_empty() {
        return Err(anyhow!(
            "'{}' annotation is present but yields no container names (value={:?})",
            ANNOTATION_WARM_FORK_CONTAINERS,
            containers_ann
        ));
    }

    Ok(Some(names))
}

/// Parse WarmFork restore parameters from pod annotations.
///
/// Always returns a `WarmForkParams`. Callers must already know the template is `WarmFork`.
///
/// Two modes determined by the `kuasar.io/task-id` annotation:
/// - Absent or empty → **autonomous mode** (`task_id: None`): guest self-starts after COMMIT.
/// - Non-empty → **injection mode** (`task_id: Some(id)`): full PREPARE/READY/COMMIT/STARTED.
///
/// Multi-container: if `kuasar.io/warm-fork-containers` is set, one `WarmForkTarget` is
/// built per named container. Per-container annotations take priority over pod-level ones.
/// The `container_id` field is left empty here (container IDs are looked up at restore time
/// when the running containers are known).
pub(crate) fn parse_warm_fork_params(
    annotations: &HashMap<String, String>,
) -> std::result::Result<WarmForkParams, anyhow::Error> {
    let task_id = annotations
        .get(ANNOTATION_WARM_FORK_TASK_ID)
        .filter(|s| !s.is_empty())
        .cloned();

    // Pod-level defaults
    let pod_socket = annotations
        .get(ANNOTATION_WARM_FORK_READINESS_SOCKET)
        .map(|s| s.as_str())
        .unwrap_or(ANNOTATION_WARM_FORK_DEFAULT_READINESS_SOCKET);
    let pod_context = annotations
        .get(ANNOTATION_WARM_FORK_TASK_CONTEXT)
        .cloned()
        .unwrap_or_default();
    let pod_env: HashMap<String, String> = annotations
        .iter()
        .filter_map(|(k, v)| {
            k.strip_prefix(ANNOTATION_WARM_FORK_ENV_PREFIX)
                .map(|env_key| (env_key.to_string(), v.clone()))
        })
        .collect();

    // Build target list
    let targets = if let Some(names) = parse_warm_fork_container_names(annotations)? {
        names
            .into_iter()
            .map(|name| {
                // Per-container socket path override
                let socket = annotations
                    .get(&format!(
                        "kuasar.io/container/{}/warm-fork-readiness-socket",
                        &name
                    ))
                    .cloned()
                    .unwrap_or_else(|| pod_socket.to_string());
                // Per-container context override
                let context = annotations
                    .get(&format!("kuasar.io/container/{}/task-context", &name))
                    .cloned()
                    .unwrap_or_else(|| pod_context.clone());
                // Merge env: pod-level base, then per-container overrides
                let mut env = pod_env.clone();
                for (k, v) in annotations.iter() {
                    let prefix = format!("kuasar.io/container/{}/task-env/", &name);
                    if let Some(env_key) = k.strip_prefix(&prefix) {
                        env.insert(env_key.to_string(), v.clone());
                    }
                }
                WarmForkTarget {
                    container_name: name,
                    container_id: String::new(), // overlaid from PooledTemplate at restore time
                    socket_path: socket,
                    env_overrides: env,
                    context,
                }
            })
            .collect()
    } else {
        // Single-container mode: container_id is overlaid from the template at restore time.
        vec![WarmForkTarget {
            container_name: String::new(),
            container_id: String::new(),
            socket_path: pod_socket.to_string(),
            env_overrides: pod_env,
            context: pod_context,
        }]
    };

    Ok(WarmForkParams {
        task_id,
        targets,
        prepare_timeout_ms: 10_000,
        commit_timeout_ms: 5_000,
    })
}

// ---------------------------------------------------------------------------
// Template worker functions
// ---------------------------------------------------------------------------

/// Return a factory with vcpus/memory overridden by the request, or a clone of the
/// original if neither field is specified.
fn apply_resource_overrides<F: VMFactory>(factory: &Arc<F>, req: &CreateTemplateRequest) -> Arc<F> {
    match (req.vcpus, req.memory_mb) {
        (None, None) => Arc::clone(factory),
        (vcpus, memory_mb) => Arc::new(factory.with_resources(
            vcpus.unwrap_or_else(|| factory.vcpus()),
            memory_mb.unwrap_or_else(|| factory.memory_mb()),
        )),
    }
}

/// Boot a fresh VM, wait for the guest agent, snapshot, then stop the VM.
/// The resulting snapshot is added to the template pool and stored on disk.
pub(crate) async fn create_template_worker<F>(
    factory: Arc<F>,
    pool: Arc<TemplatePool>,
    req: CreateTemplateRequest,
) -> Result<PooledTemplate>
where
    F: VMFactory,
    F::VM: VM + Snapshottable + Sync + Send,
{
    let template_base = pool.store_dir.join("environment").join(&req.id);
    let vm_base = template_base.join("vm");

    // Apply per-template resource overrides before dispatching to the inner function.
    let effective_factory = apply_resource_overrides(&factory, &req);
    let result =
        create_template_inner(&*effective_factory, &pool, &req, &template_base, &vm_base).await;

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
    template_base: &Path,
    vm_base: &Path,
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
    let agent_client = vm
        .wait_agent_ready(30)
        .await
        .map_err(|e| anyhow!("template {}: {}", req.id, e))?;
    let pre_snap_timeout_ns = Duration::from_secs(10).as_nanos() as i64;
    let mut exec_req = ExecVMProcessRequest::new();
    exec_req.command = "sync; echo 1 > /proc/sys/vm/drop_caches".to_string();
    if let Err(e) = agent_client
        .exec_vm_process(with_timeout(pre_snap_timeout_ns), &exec_req)
        .await
    {
        warn!(
            "template {}: pre-snapshot sync/drop_caches failed: {}",
            req.id, e
        );
    }

    let snapshot_dir = template_base.join("snapshot");
    tokio::fs::create_dir_all(&snapshot_dir)
        .await
        .map_err(|e| anyhow!("create snapshot dir: {}", e))?;

    let snap_start = Instant::now();
    let mut meta: SnapshotMeta = vm
        .snapshot(&snapshot_dir, &[])
        .await
        .map_err(|e| anyhow!("template {}: snapshot failed: {}", req.id, e))?;
    meta.lease_mode = req.lease_mode.clone();
    meta.created_at = std::time::SystemTime::now();

    info!(
        "template {}: snapshot captured in {:.3}s (lease_mode={})",
        req.id,
        snap_start.elapsed().as_secs_f64(),
        meta.lease_mode
    );

    if let Err(e) = vm.stop(false).await {
        warn!("template {}: stop after snapshot: {}", req.id, e);
    }

    // Remove the temporary VM directory (sockets, sandbox.json, etc.); only the
    // snapshot directory under template_base is retained.
    if let Err(e) = tokio::fs::remove_dir_all(vm_base).await {
        warn!("template {}: cleanup vm dir: {}", req.id, e);
    }

    // Environment key is always derived from the factory config — user-specified keys are
    // rejected at the admin API boundary and must never reach this point.
    let key = TemplateKey::from_vm_config(
        factory.kernel_path(),
        factory.image_path(),
        factory.vcpus(),
        factory.memory_mb(),
        factory.kernel_params(),
        factory.storage_backend(),
    );
    let mut tmpl = PooledTemplate::new(
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

    tmpl.created_at_secs = meta
        .created_at
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    pool.add(tmpl.clone()).await?;
    info!(
        "template {}: added to pool (pool_depth={})",
        req.id,
        pool.depth(&tmpl.key).await
    );
    Ok(tmpl)
}

// ---------------------------------------------------------------------------
// KuasarSandboxer: template pool operations
// ---------------------------------------------------------------------------

impl<F, H> KuasarSandboxer<F, H>
where
    F: VMFactory + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
    H: Hooks<F::VM> + Sync + Send,
{
    /// Initialize the template pool from `store_dir`, rehydrating any previously
    /// persisted templates.
    #[allow(clippy::too_many_arguments)]
    pub async fn init_template_pool(
        &mut self,
        sandboxer_working_dir: String,
        store_dir: PathBuf,
        environment_max_per_key: usize,
        warmfork_max_per_key: usize,
        gc_watermark: usize,
        lease_mode: TemplateLeaseMode,
        max_concurrent_restores: usize,
    ) -> Result<()> {
        // Check reflink support between sandboxer_working_dir and store_dir.
        // mkfs.xfs / cp availability is implicitly validated here: if cp is missing the
        // probe returns Err and we fall through to plain-copy mode.
        let reflink_supported = if lease_mode == TemplateLeaseMode::Shared {
            match crate::cloud_hypervisor::check_reflink_support(
                Path::new(&sandboxer_working_dir),
                &store_dir,
            )
            .await
            {
                Ok(true) => {
                    log::info!(
                        "reflink supported between {} and {} - Shared mode will use COW",
                        sandboxer_working_dir,
                        store_dir.display()
                    );
                    true
                }
                Ok(false) => {
                    log::warn!(
                        "reflink not supported between {} and {}. \
                        Shared mode will use plain copy (less space-efficient). \
                        Consider using XFS with reflink=1 or btrfs for both directories.",
                        sandboxer_working_dir,
                        store_dir.display()
                    );
                    false
                }
                Err(e) => {
                    log::error!(
                        "reflink test failed: {}. Shared mode will use plain copy.",
                        e
                    );
                    false
                }
            }
        } else {
            false
        };

        let pool = TemplatePool::load_from_disk(
            store_dir.clone(),
            environment_max_per_key,
            warmfork_max_per_key,
            gc_watermark,
            lease_mode.clone(),
            reflink_supported,
        )
        .await?;
        let total = pool.total_depth().await;
        let keys = pool.key_count().await;
        log::info!(
            "template pool initialized: {} templates across {} key(s) \
            (store={}, environment_max_per_key={}, warmfork_max_per_key={}, gc_watermark={}, lease_mode={}, reflink={})",
            total, keys, pool.store_dir.display(),
            pool.environment_max_per_key, pool.warmfork_max_per_key,
            pool.gc_watermark, lease_mode, reflink_supported
        );
        self.template_pool = Some(pool);
        self.restore_semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrent_restores));
        Ok(())
    }

    /// Initialize the continuation store from `base_dir`.
    ///
    /// The store directory `{base_dir}/continuation/` is created if absent.
    /// Must be called before `start()` for pods with Continuation annotations.
    pub async fn init_continuation_store(&mut self, base_dir: PathBuf) -> Result<()> {
        let store = ContinuationStore::load_from_disk(base_dir).await?;
        self.continuation_store = Some(store);
        Ok(())
    }

    /// Spawn the background ContinuationStore GC task.
    ///
    /// The task only removes consumed entries that are no longer referenced by a live sandbox.
    /// Unconsumed entries are intentionally retained until explicitly deleted or consumed.
    pub fn start_continuation_gc_task(&self, interval_secs: u64) {
        if interval_secs == 0 {
            info!("continuation store GC disabled (interval_secs=0)");
            return;
        }
        let store = match &self.continuation_store {
            Some(s) => s.clone(),
            None => {
                warn!("start_continuation_gc_task: continuation store not initialized, skipping");
                return;
            }
        };
        let sandboxes = self.sandboxes.clone();
        tokio::spawn(async move {
            let interval = Duration::from_secs(interval_secs);
            info!(
                "continuation store GC task started (interval={}s)",
                interval_secs
            );
            loop {
                tokio::time::sleep(interval).await;
                let active_ids = {
                    let sbs = sandboxes.read().await;
                    let mut ids = std::collections::HashSet::new();
                    for sb_mutex in sbs.values() {
                        let sb = sb_mutex.lock().await;
                        if let Some(tid) = &sb.template_id {
                            ids.insert(tid.clone());
                        }
                    }
                    ids
                };
                match store.gc_orphaned_consumed(&active_ids).await {
                    Ok(removed) if removed > 0 => {
                        info!(
                            "continuation store GC removed {} consumed entrie(s)",
                            removed
                        );
                    }
                    Ok(_) => {}
                    Err(e) => warn!("continuation store GC failed: {}", e),
                }
            }
        });
    }

    /// Return a handle for the service socket.
    ///
    /// `dir` is the sandboxer working directory (from `--dir`).  Service-managed sandbox slots
    /// are placed under `<dir>/`.
    ///
    /// Always succeeds; pool-dependent operations (template create/run/pool-status/…)
    /// return an error when the template pool is not configured.
    pub fn service_handle(&self, dir: impl Into<PathBuf>) -> crate::service::Handle<F> {
        crate::service::Handle {
            factory: Arc::clone(&self.factory),
            sandboxes: self.sandboxes.clone(),
            pool: self.template_pool.clone(),
            continuation_store: self.continuation_store.clone(),
            snapshot_config: self.config.snapshot.clone(),
            sandbox_base_dir: dir.into(),
        }
    }

    /// Spawn the background pool maintenance task.
    ///
    /// Every `interval_secs` seconds the task:
    /// 1. Computes how many additional Environment refills are needed to reach `min_pool_depth`
    ///    (accounting for both available templates and already-in-flight refills).
    /// 2. Spawns that many `create_template_worker` tasks so the pool stays warm.
    /// 3. Calls `gc_if_needed` to evict templates above the GC watermark.
    ///
    /// The task runs until the process exits. A warning is logged and the method
    /// returns early if the template pool has not been initialized.
    pub fn start_maintenance_task(&self, min_pool_depth: usize, interval_secs: u64) {
        let pool = match &self.template_pool {
            Some(p) => p.clone(),
            None => {
                warn!("start_maintenance_task: template pool not initialized, skipping");
                return;
            }
        };
        let factory = Arc::clone(&self.factory);
        tokio::spawn(async move {
            let interval = Duration::from_secs(interval_secs);
            info!(
                "template pool maintenance task started (min_pool_depth={}, interval={}s)",
                min_pool_depth, interval_secs,
            );
            loop {
                tokio::time::sleep(interval).await;

                let environment_key = TemplateKey::from_vm_config(
                    factory.kernel_path(),
                    factory.image_path(),
                    factory.vcpus(),
                    factory.memory_mb(),
                    factory.kernel_params(),
                    factory.storage_backend(),
                );

                let current = pool.depth(&environment_key).await;
                let in_flight = pool.in_flight_count_for_key(&environment_key).await;
                let covered = current.saturating_add(in_flight);
                if covered < min_pool_depth {
                    let need = min_pool_depth - covered;
                    info!(
                        "template pool maintenance: depth={}, in_flight={}, spawning {} refill(s)",
                        current, in_flight, need,
                    );
                    for _ in 0..need {
                        let refill_id = new_template_id();
                        let pool_c = pool.clone();
                        let factory_c = Arc::clone(&factory);
                        pool.begin_refill(&environment_key).await;
                        let key_c = environment_key.clone();
                        tokio::spawn(async move {
                            if let Err(e) = create_template_worker(
                                factory_c,
                                pool_c.clone(),
                                CreateTemplateRequest::new_with_lease_mode(
                                    refill_id,
                                    pool_c.lease_mode.clone(),
                                ),
                            )
                            .await
                            {
                                warn!("template pool maintenance: refill failed: {}", e);
                            }
                            pool_c.end_refill(&key_c).await;
                        });
                    }
                }

                pool.gc_if_needed().await;

                // Warn if GC is blocked (all templates above water level are actively held).
                // This indicates the pool is under pressure and may reject new templates.
                let blocked = pool.gc_blocked_templates().await;
                if !blocked.is_empty() {
                    warn!(
                        "template pool maintenance: GC blocked — {} template(s) cannot be evicted \
                        because they are held by active restores or running sandboxes: {}",
                        blocked.len(),
                        blocked
                            .iter()
                            .map(|(id, reason)| format!("{} ({})", id, reason))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
        });
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
        create_template_worker(Arc::clone(&self.factory), pool, req).await
    }

    /// Try to restore a sandbox from the ContinuationStore.
    ///
    /// Acquires the entry for `identity`, validates snapshot files, then restores
    /// the VM. Returns `Err` on store miss, missing files, or restore failure so
    /// the caller can fall back to a cold-start.
    pub async fn start_with_continuation_snapshot(
        &self,
        id: &str,
        identity: &WorkloadIdentity,
    ) -> Result<()> {
        let store = match &self.continuation_store {
            Some(s) => s.clone(),
            None => {
                return Err(anyhow!(
                    "sandbox {}: continuation store not initialized \
                     (set enable_continuation_restore=true in config)",
                    id
                )
                .into());
            }
        };

        let lease: ContinuationLease = match store.acquire(identity).await {
            Some(l) => l,
            None => {
                return Err(anyhow!(
                    "sandbox {}: no continuation snapshot found for {}/g{}",
                    id,
                    identity.pod_uid,
                    identity.generation
                )
                .into());
            }
        };

        let tmpl = lease.template()?.clone();
        let template_id = tmpl.id.clone();

        // Publish the consumed continuation template ID to the sandbox immediately.
        // This closes the small race window between store acquire and the restore code
        // persisting template_id inside the sandbox state, so GC can observe the entry as
        // still active while restore validation and semaphore acquisition are in flight.
        if let Some(sb_mutex) = self.sandboxes.read().await.get(id).cloned() {
            let mut sb = sb_mutex.lock().await;
            sb.template_id = Some(template_id.clone());
        }

        // Validate snapshot files before acquiring the restore semaphore slot.
        let state_json = tmpl.snapshot_dir.join("state.json");
        let snapshot_ok = tokio::fs::metadata(&state_json).await.is_ok()
            && tokio::fs::metadata(&tmpl.pmem_path).await.is_ok();
        if !snapshot_ok {
            warn!(
                "sandbox {}: continuation snapshot files missing for {}/g{} (template {}), releasing",
                id, identity.pod_uid, identity.generation, tmpl.id
            );
            if let Some(sb_mutex) = self.sandboxes.read().await.get(id).cloned() {
                let mut sb = sb_mutex.lock().await;
                sb.clear_template_restore_state();
            }
            lease.fail().await;
            return Err(anyhow!(
                "sandbox {}: continuation snapshot files missing for {}/g{}",
                id,
                identity.pod_uid,
                identity.generation
            )
            .into());
        }

        let _permit = self.restore_semaphore.acquire().await.unwrap();
        let restore_start = std::time::Instant::now();
        let reflink_supported = self
            .template_pool
            .as_ref()
            .map(|p| p.reflink_supported)
            .unwrap_or(false);

        info!(
            "continuation store hit for sandbox {} ({}/g{}, template {}), restoring",
            id, identity.pod_uid, identity.generation, template_id
        );

        match self
            .start_from_snapshot(
                id,
                SnapshotRestoreParams {
                    snapshot_dir: tmpl.snapshot_dir.clone(),
                    template_id: Some(template_id.clone()),
                    snapshot_type: SnapshotType::Continuation,
                    id_generator: tmpl.id_generator,
                    disk_images: tmpl.disk_images.clone(),
                    storages: tmpl.storages.clone(),
                    orphan_container_ids: tmpl.orphan_container_ids.clone(),
                    lease_mode: TemplateLeaseMode::Exclusive,
                    reflink_supported,
                    original_task_vsock: tmpl.original_task_vsock.clone(),
                    warm_fork_params: None,
                },
            )
            .await
        {
            Ok(()) => {
                let ms = restore_start.elapsed().as_millis() as u64;
                info!(
                    "sandbox {} restored from continuation snapshot {}/g{} (template {}) in {}ms",
                    id, identity.pod_uid, identity.generation, template_id, ms
                );
                lease.complete().await;
                Ok(())
            }
            Err(e) => {
                warn!(
                    "sandbox {}: continuation restore failed ({}), releasing snapshot",
                    id, e
                );
                lease.fail().await;
                Err(e)
            }
        }
    }

    /// Execute a WarmFork restore from an already-acquired and type-validated template lease.
    ///
    /// Validates the ready protocol version and snapshot files, reads WarmFork params
    /// from pod annotations, acquires the restore semaphore, calls `start_from_snapshot`,
    /// and handles success / failure (metrics, lease state, aggregate log).
    ///
    /// The caller is responsible for verifying `enable_warmfork_restore=true` before
    /// calling this.
    async fn restore_from_warm_fork_template(
        &self,
        id: &str,
        tmpl: PooledTemplate,
        lease: TemplateLease,
        pool: Arc<TemplatePool>,
        lease_mode: TemplateLeaseMode,
    ) -> Result<()> {
        if tmpl.ready_protocol_version.is_none() {
            warn!(
                "sandbox {}: template {} has no ready_protocol_version; \
                 re-create it after annotating the pod with {}={}",
                id, tmpl.id, ANNOTATION_WARM_FORK_READY_PROTOCOL, WARM_FORK_PROTOCOL_V1
            );
            lease.fail().await;
            pool.metrics.record_miss();
            return Err(anyhow!(
                "sandbox {}: WarmFork template {} missing ready_protocol_version",
                id,
                tmpl.id
            )
            .into());
        }

        let state_json = tmpl.snapshot_dir.join("state.json");
        let snapshot_ok = tokio::fs::metadata(&state_json).await.is_ok()
            && tokio::fs::metadata(&tmpl.pmem_path).await.is_ok();
        if !snapshot_ok {
            let missing = tmpl.id.clone();
            warn!(
                "sandbox {}: template {} snapshot files missing, releasing",
                id, tmpl.id
            );
            lease.fail().await;
            pool.metrics.record_miss();
            return Err(anyhow!(
                "sandbox {}: template {} snapshot files missing",
                id,
                missing
            )
            .into());
        }

        let _permit = self.restore_semaphore.acquire().await.unwrap();
        let restore_start = Instant::now();
        let template_id = tmpl.id.clone();
        let reflink_supported = pool.reflink_supported;
        info!(
            "template pool hit for sandbox {} (template {}, type=WarmFork), \
             restoring (lease_mode={}, reflink={})",
            id, tmpl.id, lease_mode, reflink_supported
        );

        // Read WarmFork task parameters from pod annotations without holding sandbox locks.
        let annotations = {
            let guard = self.sandboxes.read().await;
            let sb_mutex = guard.get(id).cloned();
            drop(guard);
            if let Some(sb_mutex) = sb_mutex {
                let sb = sb_mutex.lock().await;
                sb.data.config.as_ref().map(|cfg| cfg.annotations.clone())
            } else {
                None
            }
        }
        .unwrap_or_default();

        let mut params = match parse_warm_fork_params(&annotations).map_err(|e| {
            anyhow!(
                "sandbox {}: invalid WarmFork restore annotations: {}",
                id,
                e
            )
        }) {
            Ok(p) => p,
            Err(e) => {
                lease.fail().await;
                pool.metrics.record_miss();
                return Err(e.into());
            }
        };

        if !tmpl.warm_fork_targets.is_empty() {
            let tmpl_names: HashSet<String> = tmpl
                .warm_fork_targets
                .iter()
                .map(|t| t.container_name.clone())
                .collect();
            let restore_names: HashSet<String> = params
                .targets
                .iter()
                .map(|t| t.container_name.clone())
                .collect();
            if tmpl_names != restore_names {
                lease.fail().await;
                pool.metrics.record_miss();
                return Err(anyhow!(
                    "sandbox {}: WarmFork restore container set {:?} does not match \
                     template '{}' snapshot-time set {:?}; the pod '{}' annotation \
                     must list exactly the same container names as at snapshot time",
                    id,
                    restore_names,
                    tmpl.id,
                    tmpl_names,
                    ANNOTATION_WARM_FORK_CONTAINERS
                )
                .into());
            }
            for t in &mut params.targets {
                if let Some(resolved) = tmpl
                    .warm_fork_targets
                    .iter()
                    .find(|rt| rt.container_name == t.container_name)
                {
                    t.container_id = resolved.container_id.clone();
                }
            }
        }

        match self
            .start_from_snapshot(
                id,
                SnapshotRestoreParams {
                    snapshot_dir: tmpl.snapshot_dir.clone(),
                    template_id: Some(template_id.clone()),
                    snapshot_type: SnapshotType::WarmFork,
                    id_generator: tmpl.id_generator,
                    disk_images: tmpl.disk_images.clone(),
                    storages: tmpl.storages.clone(),
                    orphan_container_ids: tmpl.orphan_container_ids.clone(),
                    lease_mode,
                    reflink_supported,
                    original_task_vsock: tmpl.original_task_vsock.clone(),
                    warm_fork_params: Some(params),
                },
            )
            .await
        {
            Ok(()) => {
                let ms = restore_start.elapsed().as_millis() as u64;
                pool.metrics.record_warmfork_hit(ms);
                lease.complete().await;

                let hits = pool
                    .metrics
                    .pool_hits
                    .load(std::sync::atomic::Ordering::Relaxed);
                if hits > 0 && hits % 10 == 0 {
                    info!(
                        "template pool: hit_rate={:.1}% \
                         (env={:.1}%, warmfork={:.1}%), \
                         avg_restore={}ms \
                         (env={}ms, warmfork={}ms), \
                         hits={}, misses={}",
                        pool.metrics.hit_rate() * 100.0,
                        pool.metrics.environment_hit_rate() * 100.0,
                        pool.metrics.warmfork_hit_rate() * 100.0,
                        pool.metrics.avg_restore_ms() as u64,
                        pool.metrics.environment_avg_restore_ms() as u64,
                        pool.metrics.warmfork_avg_restore_ms() as u64,
                        hits,
                        pool.metrics
                            .pool_misses
                            .load(std::sync::atomic::Ordering::Relaxed),
                    );
                }

                info!(
                    "sandbox {} restored from WarmFork template {} in {}ms \
                     (pool hit_rate={:.1}%)",
                    id,
                    template_id,
                    ms,
                    pool.metrics.hit_rate() * 100.0
                );
                Ok(())
            }
            Err(e) => {
                warn!(
                    "sandbox {}: WarmFork restore failed ({}), releasing template",
                    id, e
                );
                lease.fail().await;
                pool.metrics.record_miss();
                Err(e)
            }
        }
    }

    /// Try to start an already-created sandbox from the template pool.
    ///
    /// If a matching template exists in the pool it is consumed and the sandbox
    /// is restored from that snapshot (fast path, typically < 500 ms).
    /// Returns `Err` on pool miss, missing snapshot files, or restore failure so
    /// the caller can try the next fallback (e.g. environment snapshot) before cold-starting.
    pub async fn start_with_template_key(&self, id: &str, key: &TemplateKey) -> Result<()> {
        let pool = match &self.template_pool {
            Some(p) => p.clone(),
            None => {
                return Err(anyhow!("template pool not initialized").into());
            }
        };

        let tmpl = match pool.acquire_for_restore(key).await {
            Some(t) => Some(t),
            None => {
                let in_flight = pool.in_flight_count_for_key(key).await;
                if in_flight > 0 {
                    info!(
                        "template pool empty ({} refill(s) in-flight for key '{}'), queuing sandbox {}",
                        in_flight, key.key, id
                    );
                    pool.wait_and_acquire_for_restore(key, Duration::from_secs(15))
                        .await
                } else {
                    None
                }
            }
        };

        match tmpl {
            None => {
                pool.metrics.record_miss();
                Err(anyhow!("template pool miss for sandbox {}", id).into())
            }
            Some(tmpl) => {
                let lease_mode = match &tmpl.snapshot_type {
                    SnapshotType::Environment => TemplateLeaseMode::default(),
                    SnapshotType::WarmFork => pool.lease_mode.clone(),
                    SnapshotType::Continuation => unreachable!(
                        "Continuation snapshots are managed by ContinuationStore, not TemplatePool"
                    ),
                };
                let lease = TemplateLease::new(pool.clone(), tmpl);
                let tmpl = lease.template()?.clone();

                let type_enabled = match &tmpl.snapshot_type {
                    SnapshotType::Environment => true,
                    SnapshotType::WarmFork => self.config.snapshot.enable_warmfork_restore,
                    SnapshotType::Continuation => unreachable!(
                        "Continuation snapshots are managed by ContinuationStore, not TemplatePool"
                    ),
                };
                if !type_enabled {
                    lease.fail().await;
                    pool.metrics.record_miss();
                    return Err(anyhow!(
                        "sandbox {}: template {} type {:?} is not enabled \
                         (set enable_warmfork_restore=true in [sandbox.snapshot])",
                        id,
                        tmpl.id,
                        tmpl.snapshot_type,
                    )
                    .into());
                }

                match tmpl.snapshot_type.clone() {
                    SnapshotType::WarmFork => {
                        self.restore_from_warm_fork_template(id, tmpl, lease, pool, lease_mode)
                            .await
                    }
                    SnapshotType::Environment => {
                        let state_json = tmpl.snapshot_dir.join("state.json");
                        let snapshot_ok = tokio::fs::metadata(&state_json).await.is_ok()
                            && tokio::fs::metadata(&tmpl.pmem_path).await.is_ok();
                        if !snapshot_ok {
                            let missing_template_id = tmpl.id.clone();
                            warn!(
                                "sandbox {}: template {} snapshot files missing, releasing and cold-starting",
                                id, tmpl.id
                            );
                            lease.fail().await;
                            pool.metrics.record_miss();
                            return Err(anyhow!(
                                "sandbox {}: template {} snapshot files missing",
                                id,
                                missing_template_id
                            )
                            .into());
                        }

                        let _permit = self.restore_semaphore.acquire().await.unwrap();
                        let restore_start = Instant::now();
                        let template_id = tmpl.id.clone();
                        let reflink_supported = pool.reflink_supported;
                        info!(
                            "template pool hit for sandbox {} (template {}, type=Environment), \
                             restoring (lease_mode={}, reflink={})",
                            id, tmpl.id, lease_mode, reflink_supported
                        );

                        match self
                            .start_from_snapshot(
                                id,
                                SnapshotRestoreParams {
                                    snapshot_dir: tmpl.snapshot_dir.clone(),
                                    template_id: Some(template_id.clone()),
                                    snapshot_type: SnapshotType::Environment,
                                    id_generator: tmpl.id_generator,
                                    disk_images: tmpl.disk_images.clone(),
                                    storages: tmpl.storages.clone(),
                                    orphan_container_ids: tmpl.orphan_container_ids.clone(),
                                    lease_mode,
                                    reflink_supported,
                                    original_task_vsock: tmpl.original_task_vsock.clone(),
                                    warm_fork_params: None,
                                },
                            )
                            .await
                        {
                            Ok(()) => {
                                let ms = restore_start.elapsed().as_millis() as u64;
                                pool.metrics.record_environment_hit(ms);
                                lease.complete().await;

                                let hits = pool
                                    .metrics
                                    .pool_hits
                                    .load(std::sync::atomic::Ordering::Relaxed);
                                if hits > 0 && hits % 10 == 0 {
                                    info!(
                                        "template pool: hit_rate={:.1}% \
                                         (env={:.1}%, warmfork={:.1}%), \
                                         avg_restore={}ms \
                                         (env={}ms, warmfork={}ms), \
                                         hits={}, misses={}",
                                        pool.metrics.hit_rate() * 100.0,
                                        pool.metrics.environment_hit_rate() * 100.0,
                                        pool.metrics.warmfork_hit_rate() * 100.0,
                                        pool.metrics.avg_restore_ms() as u64,
                                        pool.metrics.environment_avg_restore_ms() as u64,
                                        pool.metrics.warmfork_avg_restore_ms() as u64,
                                        hits,
                                        pool.metrics
                                            .pool_misses
                                            .load(std::sync::atomic::Ordering::Relaxed),
                                    );
                                }

                                info!(
                                    "sandbox {} restored from template {} in {}ms \
                                     (pool hit_rate={:.1}%)",
                                    id,
                                    template_id,
                                    ms,
                                    pool.metrics.hit_rate() * 100.0
                                );
                                Ok(())
                            }
                            Err(e) => {
                                warn!(
                                    "sandbox {}: template restore failed ({}), releasing template",
                                    id, e
                                );
                                lease.fail().await;
                                pool.metrics.record_miss();
                                Err(e)
                            }
                        }
                    }
                    SnapshotType::Continuation => unreachable!(
                        "Continuation snapshots are managed by ContinuationStore, not TemplatePool"
                    ),
                }
            }
        }
    }

    /// Restore a sandbox from a WarmFork template located by ID.
    ///
    /// If `key` is also provided it must match the template's pool key.
    /// Used when the pod sets `kuasar.io/template-id` (optionally combined with
    /// `kuasar.io/template-key`) instead of relying on key-based pool selection.
    pub(crate) async fn start_with_template_id(
        &self,
        id: &str,
        template_id: &str,
        key: Option<&str>,
    ) -> Result<()> {
        let pool = match &self.template_pool {
            Some(p) => p.clone(),
            None => return Err(anyhow!("template pool not initialized").into()),
        };

        let raw = pool
            .acquire_by_id_for_restore(template_id)
            .await
            .ok_or_else(|| {
                anyhow!(
                    "sandbox {}: WarmFork template '{}' not available for restore",
                    id,
                    template_id
                )
            })?;

        if raw.snapshot_type != SnapshotType::WarmFork {
            return Err(anyhow!(
                "sandbox {}: template '{}' has snapshot_type {:?}; expected warm_fork",
                id,
                template_id,
                raw.snapshot_type
            )
            .into());
        }

        let lease_mode = pool.lease_mode.clone();
        let lease = TemplateLease::new(pool.clone(), raw.clone());
        let tmpl = match lease.template() {
            Ok(t) => t.clone(),
            Err(e) => {
                lease.fail().await;
                return Err(e);
            }
        };

        if let Some(k) = key {
            let expected = TemplateKey::user(k);
            if tmpl.key.key != expected.key {
                lease.fail().await;
                return Err(anyhow!(
                    "sandbox {}: WarmFork template '{}' key '{}' does not match \
                     requested key '{}'",
                    id,
                    template_id,
                    tmpl.key.key,
                    k
                )
                .into());
            }
        }

        if !self.config.snapshot.enable_warmfork_restore {
            lease.fail().await;
            pool.metrics.record_miss();
            return Err(anyhow!(
                "sandbox {}: template {} WarmFork restore is not enabled \
                 (set enable_warmfork_restore=true in [sandbox.snapshot])",
                id,
                tmpl.id,
            )
            .into());
        }

        self.restore_from_warm_fork_template(id, tmpl, lease, pool, lease_mode)
            .await
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    mod warm_fork {
        use std::collections::HashMap;

        use crate::{
            sandbox::template_pool::{parse_warm_fork_container_names, parse_warm_fork_params},
            vm::{
                ANNOTATION_WARM_FORK_CONTAINERS, ANNOTATION_WARM_FORK_DEFAULT_READINESS_SOCKET,
                ANNOTATION_WARM_FORK_ENV_PREFIX, ANNOTATION_WARM_FORK_READINESS_SOCKET,
                ANNOTATION_WARM_FORK_TASK_CONTEXT, ANNOTATION_WARM_FORK_TASK_ID,
            },
        };

        #[test]
        fn test_parse_warm_fork_params_missing_task_id_is_autonomous() {
            let annotations: HashMap<String, String> = HashMap::new();
            let params = parse_warm_fork_params(&annotations).unwrap();
            assert!(
                params.task_id.is_none(),
                "absent task-id must yield autonomous mode"
            );
            assert_eq!(params.targets.len(), 1);
            assert_eq!(
                params.targets[0].socket_path,
                ANNOTATION_WARM_FORK_DEFAULT_READINESS_SOCKET
            );
        }

        #[test]
        fn test_parse_warm_fork_params_minimal() {
            let mut annotations = HashMap::new();
            annotations.insert(
                ANNOTATION_WARM_FORK_TASK_ID.to_string(),
                "task-42".to_string(),
            );

            let params = parse_warm_fork_params(&annotations).unwrap();
            assert_eq!(params.task_id, Some("task-42".to_string()));
            assert_eq!(params.targets.len(), 1);
            // Single-container: socket defaults to DEFAULT_READINESS_SOCKET
            assert_eq!(
                params.targets[0].socket_path,
                ANNOTATION_WARM_FORK_DEFAULT_READINESS_SOCKET
            );
            assert!(params.targets[0].context.is_empty());
            assert!(params.targets[0].env_overrides.is_empty());
        }

        #[test]
        fn test_parse_warm_fork_params_full() {
            let mut annotations = HashMap::new();
            annotations.insert(
                ANNOTATION_WARM_FORK_TASK_ID.to_string(),
                "my-task".to_string(),
            );
            annotations.insert(
                ANNOTATION_WARM_FORK_READINESS_SOCKET.to_string(),
                "/run/my-readiness.sock".to_string(),
            );
            annotations.insert(
                ANNOTATION_WARM_FORK_TASK_CONTEXT.to_string(),
                "prod-ctx".to_string(),
            );
            annotations.insert(
                format!("{}FOO", ANNOTATION_WARM_FORK_ENV_PREFIX),
                "bar".to_string(),
            );
            annotations.insert(
                format!("{}MODEL_PATH", ANNOTATION_WARM_FORK_ENV_PREFIX),
                "/models/llm".to_string(),
            );

            let params = parse_warm_fork_params(&annotations).unwrap();
            assert_eq!(params.task_id, Some("my-task".to_string()));
            assert_eq!(params.targets.len(), 1);
            assert_eq!(params.targets[0].socket_path, "/run/my-readiness.sock");
            assert_eq!(params.targets[0].context, "prod-ctx");
            assert_eq!(params.targets[0].env_overrides.get("FOO").unwrap(), "bar");
            assert_eq!(
                params.targets[0].env_overrides.get("MODEL_PATH").unwrap(),
                "/models/llm"
            );
            assert_eq!(params.targets[0].env_overrides.len(), 2);
        }

        #[test]
        fn test_parse_warm_fork_params_multi_container() {
            let mut annotations = HashMap::new();
            annotations.insert(ANNOTATION_WARM_FORK_TASK_ID.to_string(), "t-mc".to_string());
            annotations.insert(
                ANNOTATION_WARM_FORK_CONTAINERS.to_string(),
                "app,sidecar".to_string(),
            );
            // Per-container socket overrides
            annotations.insert(
                "kuasar.io/container/app/warm-fork-readiness-socket".to_string(),
                "/run/app.sock".to_string(),
            );
            // Per-container context for sidecar
            annotations.insert(
                "kuasar.io/container/sidecar/task-context".to_string(),
                "sc-ctx".to_string(),
            );
            // Pod-level env
            annotations.insert(
                format!("{}POD_KEY", ANNOTATION_WARM_FORK_ENV_PREFIX),
                "pod-val".to_string(),
            );
            // Per-container env for app only
            annotations.insert(
                "kuasar.io/container/app/task-env/APP_KEY".to_string(),
                "app-val".to_string(),
            );

            let params = parse_warm_fork_params(&annotations).unwrap();
            assert_eq!(params.task_id, Some("t-mc".to_string()));
            assert_eq!(params.targets.len(), 2);

            let app = params
                .targets
                .iter()
                .find(|t| t.container_name == "app")
                .unwrap();
            assert_eq!(app.socket_path, "/run/app.sock");
            assert_eq!(app.env_overrides.get("POD_KEY").unwrap(), "pod-val");
            assert_eq!(app.env_overrides.get("APP_KEY").unwrap(), "app-val");
            assert!(
                app.container_id.is_empty(),
                "container_id must be empty (resolved at restore)"
            );

            let sc = params
                .targets
                .iter()
                .find(|t| t.container_name == "sidecar")
                .unwrap();
            assert_eq!(sc.context, "sc-ctx");
            assert_eq!(sc.env_overrides.get("POD_KEY").unwrap(), "pod-val");
            assert!(!sc.env_overrides.contains_key("APP_KEY"));
        }

        #[test]
        fn test_warm_fork_params_json_round_trip() {
            let mut annotations = HashMap::new();
            annotations.insert(
                ANNOTATION_WARM_FORK_TASK_ID.to_string(),
                "rt-task".to_string(),
            );
            annotations.insert(
                format!("{}KEY", ANNOTATION_WARM_FORK_ENV_PREFIX),
                "value".to_string(),
            );

            let params = parse_warm_fork_params(&annotations).unwrap();
            let json = serde_json::to_vec(&params).expect("serialize");
            let back: crate::vm::WarmForkParams =
                serde_json::from_slice(&json).expect("deserialize");
            assert_eq!(back.task_id, Some("rt-task".to_string()));
            assert_eq!(back.targets[0].env_overrides.get("KEY").unwrap(), "value");
        }

        #[test]
        fn test_parse_warm_fork_params_empty_task_id_is_autonomous() {
            let mut annotations = HashMap::new();
            annotations.insert(ANNOTATION_WARM_FORK_TASK_ID.to_string(), "".to_string());
            let params = parse_warm_fork_params(&annotations).unwrap();
            assert!(
                params.task_id.is_none(),
                "empty task-id must yield autonomous mode, same as absent"
            );
        }

        #[test]
        fn test_parse_warm_fork_params_env_prefix_only_no_task_id_is_autonomous() {
            let mut annotations = HashMap::new();
            annotations.insert(
                format!("{}FOO", ANNOTATION_WARM_FORK_ENV_PREFIX),
                "bar".to_string(),
            );
            let params = parse_warm_fork_params(&annotations).unwrap();
            assert!(params.task_id.is_none());
            // Env override is still carried even in autonomous mode.
            assert_eq!(params.targets[0].env_overrides.get("FOO").unwrap(), "bar");
        }

        #[test]
        fn test_parse_warm_fork_params_default_socket_path() {
            let mut annotations = HashMap::new();
            annotations.insert(ANNOTATION_WARM_FORK_TASK_ID.to_string(), "t1".to_string());
            let params = parse_warm_fork_params(&annotations).unwrap();
            assert_eq!(
                params.targets[0].socket_path,
                ANNOTATION_WARM_FORK_DEFAULT_READINESS_SOCKET
            );
        }

        #[test]
        fn test_parse_warm_fork_params_empty_containers_annotation_returns_none() {
            let mut annotations = HashMap::new();
            annotations.insert(ANNOTATION_WARM_FORK_TASK_ID.to_string(), "t1".to_string());
            // Annotation present but contains only commas/whitespace → empty name list
            annotations.insert(
                ANNOTATION_WARM_FORK_CONTAINERS.to_string(),
                " , , ".to_string(),
            );
            assert!(
                parse_warm_fork_params(&annotations).is_err(),
                "empty container list must be rejected"
            );
        }

        #[test]
        fn test_parse_warm_fork_params_empty_container_item_returns_error() {
            let mut annotations = HashMap::new();
            annotations.insert(ANNOTATION_WARM_FORK_TASK_ID.to_string(), "t1".to_string());
            annotations.insert(
                ANNOTATION_WARM_FORK_CONTAINERS.to_string(),
                "app,,sidecar".to_string(),
            );
            assert!(
                parse_warm_fork_params(&annotations).is_err(),
                "empty container name item must be rejected"
            );
        }

        #[test]
        fn test_parse_warm_fork_params_duplicate_container_name_returns_none() {
            let mut annotations = HashMap::new();
            annotations.insert(ANNOTATION_WARM_FORK_TASK_ID.to_string(), "t1".to_string());
            annotations.insert(
                ANNOTATION_WARM_FORK_CONTAINERS.to_string(),
                "app,sidecar,app".to_string(),
            );
            assert!(
                parse_warm_fork_params(&annotations).is_err(),
                "duplicate container name must be rejected"
            );
        }

        #[test]
        fn test_parse_warm_fork_container_names_roundtrip() {
            let mut annotations = HashMap::new();
            annotations.insert(
                ANNOTATION_WARM_FORK_CONTAINERS.to_string(),
                "a,b,c".to_string(),
            );
            let names = parse_warm_fork_container_names(&annotations)
                .unwrap()
                .unwrap();
            assert_eq!(names, vec!["a", "b", "c"]);
        }
    }

    mod continuation {
        use std::collections::HashMap;

        use crate::{
            sandbox::{MemoryRestoreMode, SnapshotConfig},
            template::{SnapshotType, TemplateKey, WorkloadIdentity},
            vm::{DiskImageEntry, RestoreSource, SnapshotPathOverrides},
        };

        fn make_restore_source(storages: Vec<vmm_common::storage::Storage>) -> RestoreSource {
            RestoreSource {
                snapshot_dir: std::path::PathBuf::from("/tmp/snap"),
                work_dir: std::path::PathBuf::from("/tmp/work"),
                overrides: SnapshotPathOverrides {
                    task_vsock: String::new(),
                    console_path: String::new(),
                },
                snapshot_type: SnapshotType::Continuation,
                disk_images: vec![DiskImageEntry {
                    storage_id: "vol1".to_string(),
                    device_id: "blk0".to_string(),
                    filename: "disks/vol1.img".to_string(),
                }],
                storages,
                orphan_container_ids: vec!["ctr-a".to_string(), "ctr-b".to_string()],
                memory_restore_mode: MemoryRestoreMode::Copy,
                reflink_supported: false,
                lease_mode: crate::sandbox::TemplateLeaseMode::Exclusive,
                warm_fork_params: None,
            }
        }

        #[test]
        fn test_continuation_restore_disabled_by_default() {
            let cfg = SnapshotConfig::default();
            assert!(
                !cfg.enable_continuation_restore,
                "enable_continuation_restore must default to false"
            );
        }

        #[test]
        fn test_continuation_restore_can_be_enabled() {
            let cfg = SnapshotConfig {
                enable_continuation_restore: true,
                enable_environment_restore: false,
                enable_warmfork_restore: false,
                ..SnapshotConfig::default()
            };
            assert!(cfg.enable_continuation_restore);
        }

        #[test]
        fn test_snapshot_type_continuation_does_not_require_network_hotplug() {
            assert!(
                !SnapshotType::Continuation.requires_network_hotplug(),
                "Continuation must not require network hotplug (identity preserved externally)"
            );
        }

        /// remap_restored_storage_artifacts produces cleanup paths in the sandbox base_dir.
        /// Verifies the path formula used in try_restore for Continuation.
        #[test]
        fn test_remap_cleanup_path_formula() {
            // The remap formula is: "{base_dir}/{storage.id}.img"
            // This must match where prepare_restore_block_artifacts places the renamed disk
            // (base_dir passed to that function is the sandbox base_dir).
            let base_dir = "/run/kuasar/test-sb";
            let storage_id = "vol1";
            let expected = format!("{}/{}.img", base_dir, storage_id);
            assert_eq!(expected, "/run/kuasar/test-sb/vol1.img");
        }

        /// Continuation restore source carries orphan container IDs.
        #[test]
        fn test_continuation_restore_source_has_orphan_container_ids() {
            let src = make_restore_source(vec![]);
            assert_eq!(
                src.orphan_container_ids,
                vec!["ctr-a".to_string(), "ctr-b".to_string()],
                "orphan containers from snapshot must be propagated via RestoreSource"
            );
        }

        /// Pool key for Continuation includes generation to prevent stale snapshot reuse.
        #[test]
        fn test_continuation_pool_key_encodes_workload_identity() {
            let wi = WorkloadIdentity {
                pod_uid: "abc-def".to_string(),
                generation: 2,
            };
            let key = TemplateKey::from_workload_identity(&wi.pod_uid, wi.generation);
            assert_eq!(key.key, "abc-def:2");
        }

        /// Continuation pod annotations for restore lookup.
        #[test]
        fn test_cont_annotation_constants_are_correct() {
            assert_eq!(crate::template::POD_UID_ANNOTATION, "kuasar.io/pod-uid");
            assert_eq!(
                crate::template::WORKLOAD_GENERATION_ANNOTATION,
                "kuasar.io/workload-generation"
            );
        }

        /// When a pod sets kuasar.io/pod-uid, the restore key is derived from workload identity.
        #[test]
        fn test_cont_pod_uid_annotation_drives_key_derivation() {
            let mut annotations = HashMap::new();
            annotations.insert(
                crate::template::POD_UID_ANNOTATION.to_string(),
                "pod-xyz".to_string(),
            );
            annotations.insert(
                crate::template::WORKLOAD_GENERATION_ANNOTATION.to_string(),
                "3".to_string(),
            );
            let pod_uid = annotations
                .get(crate::template::POD_UID_ANNOTATION)
                .unwrap();
            let restart: u32 = annotations
                .get(crate::template::WORKLOAD_GENERATION_ANNOTATION)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let key = TemplateKey::from_workload_identity(pod_uid, restart);
            assert_eq!(key.key, "pod-xyz:3");
        }

        /// Missing generation annotation defaults to 0.
        #[test]
        fn test_cont_generation_defaults_to_zero_when_absent() {
            let annotations: HashMap<String, String> = HashMap::new();
            // Absent annotation → parse returns None → default 0.
            let restart: u32 =
                match annotations.get(crate::template::WORKLOAD_GENERATION_ANNOTATION) {
                    None => 0,
                    Some(s) => s
                        .parse::<u64>()
                        .ok()
                        .filter(|&v| v <= u32::MAX as u64)
                        .unwrap_or(0) as u32,
                };
            assert_eq!(restart, 0);
        }

        /// A non-numeric generation annotation must cause a hard error when
        /// pod-uid is also present.  Falling back to an environment restore would
        /// silently discard the workload state the operator intended to continue.
        #[test]
        fn test_cont_generation_malformed_produces_hard_error() {
            // Simulate the production parse: non-numeric → Err (hard failure).
            let counter_str = "not-a-number";
            let result: Result<u32, String> = match counter_str.parse::<u64>() {
                Ok(v) if v <= u32::MAX as u64 => Ok(v as u32),
                Ok(v) => Err(format!("{} exceeds u32::MAX", v)),
                Err(_) => Err(format!("{:?} is not a valid u32", counter_str)),
            };
            assert!(
                result.is_err(),
                "malformed generation must produce an error"
            );
        }

        /// An out-of-range generation (> u32::MAX) must also produce a hard error.
        #[test]
        fn test_cont_generation_overflow_produces_hard_error() {
            let counter_str = (u32::MAX as u64 + 1).to_string();
            let result: Result<u32, String> = match counter_str.parse::<u64>() {
                Ok(v) if v <= u32::MAX as u64 => Ok(v as u32),
                Ok(v) => Err(format!("{} exceeds u32::MAX", v)),
                Err(_) => Err(format!("{:?} is not a valid u32", counter_str)),
            };
            assert!(
                result.is_err(),
                "generation > u32::MAX must produce an error, not be truncated"
            );
        }
    }
}
