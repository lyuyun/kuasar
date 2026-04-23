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

//! Warm pool of pre-restored VMs for ultra-fast sandbox startup.
//!
//! The pool maintains `desired_per_key` VMs per snapshot key in a READY state
//! (restored but not yet assigned to any sandbox).  When a sandbox requests a
//! VM via [`WarmPool::acquire`], it receives one instantly without waiting for
//! the restore operation.  A background task replenishes the slot immediately
//! after each acquire.
//!
//! ## Lifecycle
//!
//! ```text
//! SnapshotKey → [restore] → WarmSlot (ready) → [acquire] → Sandbox
//!                                ↑___________________________|
//!                               (replenish triggered on acquire)
//! ```
//!
//! ## Back-pressure
//!
//! `MAX_CONCURRENT_RESTORES` prevents thundering-herd situations when many
//! sandboxes start simultaneously for the same key.

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use anyhow::anyhow;
use containerd_sandbox::{data::SandboxData, error::Result, SandboxOption};
use log::{debug, error, info, warn};
use tokio::{sync::Mutex, task::JoinHandle};
use uuid::Uuid;

use crate::vm::{VMFactory, VM};

use super::{
    metrics,
    store::{SnapshotKey, SnapshotStore},
};

pub const WARM_POOL_DEFAULT_WORK_DIR: &str = "/var/run/kuasar/warmpool";

/// Maximum number of concurrent restore operations per key (back-pressure).
const MAX_CONCURRENT_RESTORES: usize = 2;

/// A pre-restored VM slot waiting to be assigned to a sandbox.
pub struct WarmSlot<V> {
    /// Temporary sandbox ID used during restore.
    pub temp_id: String,
    /// Base directory the VM's sockets and state are rooted at.
    pub base_dir: String,
    /// The pre-restored VM instance.
    pub vm: V,
}

type SlotQueue<V> = VecDeque<WarmSlot<V>>;

struct KeyState<V> {
    slots: SlotQueue<V>,
    in_flight: usize,
}

impl<V> KeyState<V> {
    fn new() -> Self {
        Self {
            slots: VecDeque::new(),
            in_flight: 0,
        }
    }

    fn ready_count(&self) -> usize {
        self.slots.len()
    }
}

/// Warm pool of pre-restored VMs.
pub struct WarmPool<F: VMFactory> {
    factory: Arc<F>,
    store: SnapshotStore,
    state: Arc<Mutex<HashMap<String, KeyState<F::VM>>>>,
    desired_per_key: usize,
    /// Host directory under which per-slot base dirs are created.
    work_dir: String,
}

impl<F> WarmPool<F>
where
    F: VMFactory + Sync + Send + 'static,
    F::VM: VM + Sync + Send + 'static,
{
    pub fn new(
        factory: Arc<F>,
        store: SnapshotStore,
        desired_per_key: usize,
        work_dir: impl Into<String>,
    ) -> Self {
        Self {
            factory,
            store,
            state: Arc::new(Mutex::new(HashMap::new())),
            desired_per_key,
            work_dir: work_dir.into(),
        }
    }

    /// Pre-warm slots for `key`.  Spawns background restore tasks up to
    /// `desired_per_key`.  Safe to call repeatedly — excess calls are no-ops.
    pub async fn prime(&self, key: &SnapshotKey) {
        self.trigger_replenish(key).await;
    }

    /// Acquire a pre-restored VM for `sandbox_id`.
    ///
    /// Returns `Some(slot)` if a warm VM is immediately available.  `None`
    /// means the pool is empty; the caller should fall back to on-demand
    /// restore.  A background replenishment task is always triggered.
    pub async fn acquire(&self, key: &SnapshotKey, sandbox_id: &str) -> Option<WarmSlot<F::VM>> {
        let mut state = self.state.lock().await;
        let entry = state.entry(key.0.clone()).or_insert_with(KeyState::new);

        let slot = match entry.slots.pop_front() {
            Some(s) => s,
            None => {
                metrics::record_warm_pool_miss(key.as_str());
                return None;
            }
        };

        info!(
            "sandbox {}: warm slot acquired (temp_id={}, key={})",
            sandbox_id, slot.temp_id, key
        );
        metrics::record_warm_pool_hit(key.as_str());
        metrics::set_warm_pool_size(key.as_str(), entry.ready_count());

        let need = self
            .desired_per_key
            .saturating_sub(entry.ready_count() + entry.in_flight);
        let to_start = need.min(MAX_CONCURRENT_RESTORES.saturating_sub(entry.in_flight));
        entry.in_flight += to_start;
        drop(state);

        for _ in 0..to_start {
            self.spawn_restore_task(key.clone());
        }

        Some(slot)
    }

    /// Return an unused acquired slot (e.g. if sandbox creation fails after
    /// acquire).  The VM is stopped; a replenishment task is triggered.
    pub async fn release(&self, key: &SnapshotKey, slot: WarmSlot<F::VM>) {
        let mut vm = slot.vm;
        if let Err(e) = vm.stop(false).await {
            warn!("warm pool: failed to stop released VM for key {}: {}", key, e);
        }
        if let Err(e) = tokio::fs::remove_dir_all(&slot.base_dir).await {
            warn!("warm pool: remove {}: {}", slot.base_dir, e);
        }
        self.trigger_replenish(key).await;
    }

    /// Drain and stop all warm slots for `key` (used when a snapshot is
    /// deleted or invalidated).
    pub async fn drain_key(&self, key: &SnapshotKey) {
        let slots = {
            let mut state = self.state.lock().await;
            state
                .remove(&key.0)
                .map(|ks| ks.slots)
                .unwrap_or_default()
        };
        let count = slots.len();
        for slot in slots {
            let mut vm = slot.vm;
            if let Err(e) = vm.stop(false).await {
                warn!("warm pool drain: stop {}: {}", slot.temp_id, e);
            }
            let _ = tokio::fs::remove_dir_all(&slot.base_dir).await;
        }
        if count > 0 {
            info!("warm pool: drained {} slot(s) for key {}", count, key);
        }
    }

    /// Ready slot counts per key (for metrics / diagnostics).
    pub async fn ready_counts(&self) -> HashMap<String, usize> {
        self.state
            .lock()
            .await
            .iter()
            .map(|(k, v)| (k.clone(), v.ready_count()))
            .collect()
    }

    // ──────────────────────────────────────────────
    // Internals
    // ──────────────────────────────────────────────

    async fn trigger_replenish(&self, key: &SnapshotKey) {
        let mut state = self.state.lock().await;
        let entry = state.entry(key.0.clone()).or_insert_with(KeyState::new);
        let need = self
            .desired_per_key
            .saturating_sub(entry.ready_count() + entry.in_flight);
        let to_start = need.min(MAX_CONCURRENT_RESTORES.saturating_sub(entry.in_flight));
        entry.in_flight += to_start;
        drop(state);
        for _ in 0..to_start {
            self.spawn_restore_task(key.clone());
        }
    }

    fn spawn_restore_task(&self, key: SnapshotKey) {
        let factory = Arc::clone(&self.factory);
        let store = self.store.clone();
        let state = Arc::clone(&self.state);
        let work_dir = self.work_dir.clone();
        let desired = self.desired_per_key;

        tokio::spawn(async move {
            let result = Self::restore_one(&factory, &store, &key, &work_dir).await;

            let mut locked = state.lock().await;
            let entry = locked.entry(key.0.clone()).or_insert_with(KeyState::new);
            entry.in_flight = entry.in_flight.saturating_sub(1);

            match result {
                Ok(slot) => {
                    entry.slots.push_back(slot);
                    let size = entry.ready_count();
                    debug!("warm pool: slot ready for key {} (pool={})", key, size);
                    metrics::set_warm_pool_size(key.as_str(), size);

                    let still_need = desired.saturating_sub(entry.ready_count() + entry.in_flight);
                    let can_start = MAX_CONCURRENT_RESTORES
                        .saturating_sub(entry.in_flight)
                        .min(still_need);
                    entry.in_flight += can_start;
                    drop(locked);

                    for _ in 0..can_start {
                        let (f2, s2, st2, wd2, k2) = (
                            Arc::clone(&factory),
                            store.clone(),
                            Arc::clone(&state),
                            work_dir.clone(),
                            key.clone(),
                        );
                        tokio::spawn(async move {
                            let r = Self::restore_one(&f2, &s2, &k2, &wd2).await;
                            let mut l = st2.lock().await;
                            let e = l.entry(k2.0.clone()).or_insert_with(KeyState::new);
                            e.in_flight = e.in_flight.saturating_sub(1);
                            if let Ok(slot) = r {
                                e.slots.push_back(slot);
                                metrics::set_warm_pool_size(k2.as_str(), e.ready_count());
                            }
                        });
                    }
                }
                Err(e) => {
                    error!("warm pool: restore failed for key {}: {}", key, e);
                }
            }
        });
    }

    async fn restore_one(
        factory: &F,
        store: &SnapshotStore,
        key: &SnapshotKey,
        work_dir: &str,
    ) -> Result<WarmSlot<F::VM>> {
        let temp_id = format!("warmpool-{}", Uuid::new_v4().simple());
        let base_dir = format!("{}/{}/{}", work_dir, key.as_str(), temp_id);

        tokio::fs::create_dir_all(&base_dir)
            .await
            .map_err(|e| anyhow!("warm pool mkdir {}: {}", base_dir, e))?;

        let meta = store
            .get_by_key(key)
            .await
            .map_err(|e| anyhow!("warm pool: get snapshot for key '{}': {}", key, e))?;

        let source_url = store.source_url(&meta.id);

        debug!(
            "warm pool: restoring for key {} (snapshot {}) into {}",
            key, meta.id, base_dir
        );

        // Construct a minimal SandboxOption for the restore call.
        // Network namespace is left empty so the VM uses the host netns;
        // the real netns and network config are applied by post-restore-init.
        let s = SandboxOption {
            base_dir: base_dir.clone(),
            sandbox: SandboxData {
                id: temp_id.clone(),
                ..SandboxData::default()
            },
        };

        let vm = factory
            .restore_vm(&temp_id, &s, &source_url)
            .await
            .map_err(|e| anyhow!("warm pool restore_vm: {}", e))?;

        Ok(WarmSlot {
            temp_id,
            base_dir,
            vm,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use async_trait::async_trait;
    use containerd_sandbox::{error::Result, SandboxOption};
    use serde::{Deserialize, Serialize};
    use temp_dir::TempDir;

    use crate::{
        device::{BusType, DeviceInfo},
        snapshot::store::{SnapshotKey, SnapshotStore, MEMORY_FILE, STATE_FILE},
        vm::{Pids, VcpuThreads, VMFactory, VM},
    };

    use super::WarmPool;

    // ── Mock VM ──────────────────────────────────────────────────────────────

    #[derive(Clone, Serialize, Deserialize)]
    struct MockVM;

    #[async_trait]
    impl VM for MockVM {
        async fn start(&mut self) -> Result<u32> {
            Ok(0)
        }
        async fn stop(&mut self, _force: bool) -> Result<()> {
            Ok(())
        }
        async fn attach(&mut self, _d: DeviceInfo) -> Result<()> {
            Ok(())
        }
        async fn hot_attach(&mut self, _d: DeviceInfo) -> Result<(BusType, String)> {
            Ok((BusType::PCI, "0000:00:01.0".into()))
        }
        async fn hot_detach(&mut self, _id: &str) -> Result<()> {
            Ok(())
        }
        async fn ping(&self) -> Result<()> {
            Ok(())
        }
        fn socket_address(&self) -> String {
            String::new()
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

    // ── Mock Factory ─────────────────────────────────────────────────────────

    struct MockVMFactory;

    #[async_trait]
    impl VMFactory for MockVMFactory {
        type VM = MockVM;
        type Config = ();

        fn new(_config: ()) -> Self {
            MockVMFactory
        }

        async fn create_vm(&self, _id: &str, _s: &SandboxOption) -> Result<Self::VM> {
            Ok(MockVM)
        }

        fn supports_app_snapshot(&self) -> bool {
            true
        }

        async fn restore_vm(
            &self,
            _id: &str,
            _s: &SandboxOption,
            _source_url: &str,
        ) -> Result<Self::VM> {
            Ok(MockVM)
        }
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    async fn make_store_with_snapshot(root: &std::path::Path, key: &str) -> SnapshotStore {
        let store = SnapshotStore::new(root);
        store.init().await.unwrap();

        let staging = root.join("staging");
        tokio::fs::create_dir_all(&staging).await.unwrap();
        tokio::fs::write(staging.join(MEMORY_FILE), b"fake-memory").await.unwrap();
        tokio::fs::write(staging.join(STATE_FILE), b"fake-state").await.unwrap();

        store
            .import_from(&staging, &SnapshotKey(key.into()), "mock")
            .await
            .unwrap();

        store
    }

    async fn wait_for_pool_size(pool: &WarmPool<MockVMFactory>, key: &SnapshotKey, want: usize) {
        for _ in 0..50 {
            let counts = pool.ready_counts().await;
            if counts.get(key.as_str()).copied().unwrap_or(0) >= want {
                return;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }
        panic!(
            "warm pool size for key '{}' did not reach {} within 2.5s",
            key, want
        );
    }

    // ── Tests ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_acquire_empty_pool_returns_none() {
        let tmp = TempDir::new().unwrap();
        let work_dir = tmp.path().join("warmpool");
        let store_dir = tmp.path().join("store");

        let store = make_store_with_snapshot(&store_dir, "test-key").await;
        let factory = Arc::new(MockVMFactory);
        // desired_per_key = 0 → no slots ever filled
        let pool = WarmPool::new(factory, store, 0, work_dir.to_str().unwrap());

        let key = SnapshotKey("test-key".into());
        assert!(pool.acquire(&key, "sandbox-1").await.is_none());
    }

    #[tokio::test]
    async fn test_prime_and_acquire_slot() {
        let tmp = TempDir::new().unwrap();
        let work_dir = tmp.path().join("warmpool");
        let store_dir = tmp.path().join("store");

        let store = make_store_with_snapshot(&store_dir, "my-app").await;
        let factory = Arc::new(MockVMFactory);
        let pool = Arc::new(WarmPool::new(
            factory,
            store,
            1,
            work_dir.to_str().unwrap(),
        ));

        let key = SnapshotKey("my-app".into());
        pool.prime(&key).await;
        wait_for_pool_size(&pool, &key, 1).await;

        let slot = pool.acquire(&key, "sandbox-123").await.expect("warm slot");
        assert!(!slot.temp_id.is_empty());
        assert!(!slot.base_dir.is_empty());
    }

    #[tokio::test]
    async fn test_acquire_triggers_replenish() {
        let tmp = TempDir::new().unwrap();
        let work_dir = tmp.path().join("warmpool");
        let store_dir = tmp.path().join("store");

        let store = make_store_with_snapshot(&store_dir, "replenish-key").await;
        let factory = Arc::new(MockVMFactory);
        let pool = Arc::new(WarmPool::new(
            factory,
            store,
            1,
            work_dir.to_str().unwrap(),
        ));

        let key = SnapshotKey("replenish-key".into());
        pool.prime(&key).await;
        wait_for_pool_size(&pool, &key, 1).await;

        // Consume the slot — replenish should kick off automatically.
        let _slot = pool.acquire(&key, "sb-1").await.expect("warm slot");

        // Pool should refill to 1 again.
        wait_for_pool_size(&pool, &key, 1).await;
    }

    #[tokio::test]
    async fn test_drain_key_removes_all_slots() {
        let tmp = TempDir::new().unwrap();
        let work_dir = tmp.path().join("warmpool");
        let store_dir = tmp.path().join("store");

        let store = make_store_with_snapshot(&store_dir, "drain-key").await;
        let factory = Arc::new(MockVMFactory);
        let pool = Arc::new(WarmPool::new(
            factory,
            store,
            2,
            work_dir.to_str().unwrap(),
        ));

        let key = SnapshotKey("drain-key".into());
        pool.prime(&key).await;
        wait_for_pool_size(&pool, &key, 2).await;

        pool.drain_key(&key).await;

        let counts = pool.ready_counts().await;
        assert_eq!(counts.get(key.as_str()).copied().unwrap_or(0), 0);
    }

    #[tokio::test]
    async fn test_release_triggers_replenish() {
        let tmp = TempDir::new().unwrap();
        let work_dir = tmp.path().join("warmpool");
        let store_dir = tmp.path().join("store");

        let store = make_store_with_snapshot(&store_dir, "release-key").await;
        let factory = Arc::new(MockVMFactory);
        let pool = Arc::new(WarmPool::new(
            factory,
            store,
            1,
            work_dir.to_str().unwrap(),
        ));

        let key = SnapshotKey("release-key".into());
        pool.prime(&key).await;
        wait_for_pool_size(&pool, &key, 1).await;

        // Acquire, then release — pool should remain at desired=1.
        let slot = pool.acquire(&key, "sb-release").await.unwrap();
        pool.release(&key, slot).await;

        wait_for_pool_size(&pool, &key, 1).await;
    }
}

/// Background loop that periodically calls `prime()` for each key, recovering
/// from restore failures on a schedule.  Returns a handle that can be aborted
/// on shutdown.
pub fn start_replenish_loop<F>(
    pool: Arc<WarmPool<F>>,
    keys: Vec<SnapshotKey>,
    interval_secs: u64,
) -> JoinHandle<()>
where
    F: VMFactory + Sync + Send + 'static,
    F::VM: VM + Sync + Send + 'static,
{
    tokio::spawn(async move {
        let interval = tokio::time::Duration::from_secs(interval_secs);
        loop {
            tokio::time::sleep(interval).await;
            for key in &keys {
                pool.prime(key).await;
            }
        }
    })
}
