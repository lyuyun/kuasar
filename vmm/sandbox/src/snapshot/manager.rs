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

use std::{path::PathBuf, sync::Arc};

use anyhow::anyhow;
use containerd_sandbox::{error::Result, SandboxOption};
use log::info;
use tokio::fs;

use crate::vm::{VMFactory, VM};

use super::{
    error::SnapshotError,
    metrics,
    store::{SnapshotKey, SnapshotMetadata, SnapshotStore},
};

/// Annotation key used by callers to request snapshot-restore startup.
pub const SNAPSHOT_KEY_ANNOTATION: &str = "snapshotter.kuasar.io/snapshot-key";

/// Annotation key used to identify the hypervisor type when committing.
pub const HYPERVISOR_TYPE_ANNOTATION: &str = "snapshotter.kuasar.io/hypervisor";

/// Orchestrates snapshot creation and VM restoration for a single hypervisor
/// factory type.
///
/// The manager is intentionally stateless — all persistent state lives in
/// the [`SnapshotStore`] on disk.
pub struct SnapshotManager<F> {
    factory: Arc<F>,
    store: SnapshotStore,
}

impl<F> SnapshotManager<F>
where
    F: VMFactory + Sync + Send,
    F::VM: VM + Sync + Send + 'static,
{
    pub fn new(factory: Arc<F>, store: SnapshotStore) -> Self {
        Self { factory, store }
    }

    pub fn with_default_store(factory: Arc<F>) -> Self {
        Self::new(factory, SnapshotStore::default_store())
    }

    /// Create a VM, wait for it to be ready, pause it, take a snapshot, and
    /// resume it.  The snapshot is stored under the given `key`.
    ///
    /// The returned [`SnapshotMetadata`] contains the content-addressable ID
    /// that can be used for subsequent restores.
    pub async fn create_snapshot(
        &self,
        sandbox_id: &str,
        key: &SnapshotKey,
        s: &SandboxOption,
        hypervisor: &str,
    ) -> Result<SnapshotMetadata> {
        if !self.factory.supports_app_snapshot() {
            return Err(SnapshotError::Unsupported.into());
        }

        // Ensure the store root exists.
        self.store.init().await.map_err(|e| anyhow!(e.to_string()))?;

        // Temporary staging directory; content is moved into the
        // content-addressable store once the snapshot is complete.
        let staging_dir: PathBuf = self
            .store
            .root()
            .join(format!("staging-{}", sandbox_id));

        fs::create_dir_all(&staging_dir)
            .await
            .map_err(|e| anyhow!("create staging dir: {}", e))?;

        let snapshot_url = format!("file://{}", staging_dir.display());

        info!(
            "taking app snapshot for sandbox {}, staging at {}",
            sandbox_id, snapshot_url
        );

        let mut vm = self
            .factory
            .create_vm(sandbox_id, s)
            .await
            .map_err(|e| anyhow!("create vm for snapshot: {}", e))?;

        // Start the VM so we can take the snapshot.
        vm.start()
            .await
            .map_err(|e| anyhow!("start vm for snapshot: {}", e))?;

        // Take the snapshot (pause → snapshot → resume).
        self.factory
            .snapshot_vm(&vm, &snapshot_url)
            .await
            .map_err(|e| anyhow!("snapshot vm: {}", e))?;

        // Stop the temporary VM — the snapshot files now live in staging_dir.
        let _ = vm.stop(false).await;

        // Move staging → content-addressable store.
        let meta = self
            .store
            .import_from(&staging_dir, key, hypervisor)
            .await
            .map_err(|e| anyhow!("import snapshot: {}", e))?;

        info!(
            "snapshot {} created for key '{}' (memory {} MB, state {} KB)",
            meta.id,
            meta.key,
            meta.memory_size_bytes / (1024 * 1024),
            meta.state_size_bytes / 1024,
        );

        metrics::record_snapshot_created();
        metrics::record_store_sizes(meta.memory_size_bytes, meta.state_size_bytes);
        Ok(meta)
    }

    /// Restore a VM from snapshot identified by `key`, building a new VM
    /// instance that is paused and ready for post-restore initialisation.
    pub async fn restore_vm_from_key(
        &self,
        sandbox_id: &str,
        key: &SnapshotKey,
        s: &SandboxOption,
    ) -> Result<F::VM> {
        if !self.factory.supports_app_snapshot() {
            return Err(SnapshotError::Unsupported.into());
        }

        let meta = self
            .store
            .get_by_key(key)
            .await
            .map_err(|e| anyhow!("get snapshot by key '{}': {}", key, e))?;

        info!(
            "restoring sandbox {} from snapshot {} (key='{}')",
            sandbox_id, meta.id, key
        );

        let source_url = self.store.source_url(&meta.id);

        let timer = metrics::RestoreTimer::start();
        let result = self
            .factory
            .restore_vm(sandbox_id, s, &source_url)
            .await
            .map_err(|e| anyhow!("restore vm from snapshot {}: {}", meta.id, e).into());
        let ok = result.is_ok();
        timer.observe();
        metrics::record_restore(ok);
        result
    }
}

/// Extract the snapshot key from sandbox annotations, if present.
pub fn get_snapshot_key(s: &SandboxOption) -> Option<SnapshotKey> {
    s.sandbox
        .config
        .as_ref()
        .and_then(|c| c.annotations.get(SNAPSHOT_KEY_ANNOTATION))
        .map(|v| SnapshotKey(v.clone()))
}
