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
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::anyhow;
use containerd_sandbox::error::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::utils::write_file_atomic;

/// Identifies a class of templates that share the same VM configuration.
/// Two sandboxes with the same TemplateKey can be started from the same pool.
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct TemplateKey {
    /// Absolute path to the ext4 pmem rootfs image (shared read-only).
    pub image_path: String,
    pub vcpus: u32,
    pub memory_mb: u32,
}

impl TemplateKey {
    pub fn new(image_path: impl Into<String>, vcpus: u32, memory_mb: u32) -> Self {
        Self {
            image_path: image_path.into(),
            vcpus,
            memory_mb,
        }
    }
}

/// A single pre-warmed VM snapshot ready to be restored into a sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PooledTemplate {
    pub id: String,
    pub key: TemplateKey,
    /// Directory containing CH snapshot files (memory-ranges/, state.json, config.json).
    pub snapshot_dir: PathBuf,
    pub pmem_path: String,
    pub kernel_path: String,
    pub vcpus: u32,
    pub memory_mb: u32,
    /// Unix timestamp (seconds) when this template was created.
    pub created_at_secs: u64,
    /// Original hvsock path recorded at snapshot time (needed to compute overrides).
    pub original_task_vsock: String,
    /// Original console log path recorded at snapshot time.
    pub original_console_path: String,
}

impl PooledTemplate {
    /// Persist to `{dir}/pooled_template.json` atomically.
    pub async fn save(&self, dir: &Path) -> Result<()> {
        let content = serde_json::to_string_pretty(self)
            .map_err(|e| anyhow!("serialize PooledTemplate: {}", e))?;
        write_file_atomic(&dir.join("pooled_template.json"), &content).await
    }

    /// Load from `{dir}/pooled_template.json`.
    pub async fn load(dir: &Path) -> Result<Self> {
        let content = tokio::fs::read_to_string(dir.join("pooled_template.json"))
            .await
            .map_err(|e| anyhow!("read pooled_template.json from {}: {}", dir.display(), e))?;
        serde_json::from_str(&content)
            .map_err(|e| anyhow!("parse pooled_template.json: {}", e).into())
    }

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    pub fn new(
        id: impl Into<String>,
        key: TemplateKey,
        snapshot_dir: PathBuf,
        pmem_path: impl Into<String>,
        kernel_path: impl Into<String>,
        vcpus: u32,
        memory_mb: u32,
        original_task_vsock: impl Into<String>,
        original_console_path: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            key,
            snapshot_dir,
            pmem_path: pmem_path.into(),
            kernel_path: kernel_path.into(),
            vcpus,
            memory_mb,
            created_at_secs: Self::now_secs(),
            original_task_vsock: original_task_vsock.into(),
            original_console_path: original_console_path.into(),
        }
    }
}

/// Atomic counters for pool health monitoring.
#[derive(Default)]
pub struct TemplateMetrics {
    pub templates_created: AtomicU64,
    pub pool_hits: AtomicU64,
    pub pool_misses: AtomicU64,
    /// Cumulative restore latency for computing average.
    pub restore_total_ms: AtomicU64,
    pub restore_count: AtomicU64,
}

impl TemplateMetrics {
    pub fn record_hit(&self, restore_ms: u64) {
        self.pool_hits.fetch_add(1, Ordering::Relaxed);
        self.restore_total_ms
            .fetch_add(restore_ms, Ordering::Relaxed);
        self.restore_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_miss(&self) {
        self.pool_misses.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_template_created(&self) {
        self.templates_created.fetch_add(1, Ordering::Relaxed);
    }

    /// Pool hit rate as a value in [0.0, 1.0]. Returns 0.0 if no requests yet.
    pub fn hit_rate(&self) -> f64 {
        let hits = self.pool_hits.load(Ordering::Relaxed) as f64;
        let misses = self.pool_misses.load(Ordering::Relaxed) as f64;
        let total = hits + misses;
        if total == 0.0 {
            0.0
        } else {
            hits / total
        }
    }

    /// Average restore latency in milliseconds across all pool-hit restores.
    pub fn avg_restore_ms(&self) -> f64 {
        let total = self.restore_total_ms.load(Ordering::Relaxed) as f64;
        let count = self.restore_count.load(Ordering::Relaxed) as f64;
        if count == 0.0 {
            0.0
        } else {
            total / count
        }
    }
}

/// LIFO pool of pre-warmed VM templates, keyed by VM configuration.
///
/// Pool entries are stored both in memory and on disk under `store_dir`.
/// On process restart the pool can be rehydrated via `load_from_disk()`.
pub struct TemplatePool {
    available: Mutex<HashMap<TemplateKey, VecDeque<PooledTemplate>>>,
    pub store_dir: PathBuf,
    /// Maximum number of templates to retain per key; oldest are evicted when exceeded.
    pub max_per_key: usize,
    pub metrics: Arc<TemplateMetrics>,
}

impl TemplatePool {
    /// Create an empty pool backed by `store_dir`.
    pub fn new(store_dir: PathBuf, max_per_key: usize) -> Arc<Self> {
        Arc::new(Self {
            available: Mutex::new(HashMap::new()),
            store_dir,
            max_per_key,
            metrics: Arc::new(TemplateMetrics::default()),
        })
    }

    /// Scan `store_dir` on startup and reload any previously persisted templates.
    pub async fn load_from_disk(store_dir: PathBuf, max_per_key: usize) -> Result<Arc<Self>> {
        let pool = Self::new(store_dir.clone(), max_per_key);
        let mut entries = match tokio::fs::read_dir(&store_dir).await {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(pool),
            Err(e) => return Err(anyhow!("read template store dir: {}", e).into()),
        };
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| anyhow!("iterate template store dir: {}", e))?
        {
            let dir = entry.path();
            match PooledTemplate::load(&dir).await {
                Ok(tmpl) => {
                    let mut map = pool.available.lock().await;
                    map.entry(tmpl.key.clone())
                        .or_default()
                        .push_back(tmpl);
                }
                Err(e) => {
                    log::warn!(
                        "template pool: failed to load template from {}: {}",
                        dir.display(),
                        e
                    );
                }
            }
        }
        Ok(pool)
    }

    /// Add a template to the pool, persisting it to disk.
    ///
    /// If this key already has `max_per_key` entries, the oldest one is evicted
    /// (its on-disk directory is removed).
    pub async fn add(&self, tmpl: PooledTemplate) -> Result<()> {
        let tmpl_dir = self.store_dir.join(&tmpl.id);
        tokio::fs::create_dir_all(&tmpl_dir)
            .await
            .map_err(|e| anyhow!("create template dir {}: {}", tmpl_dir.display(), e))?;
        tmpl.save(&tmpl_dir).await?;

        let mut map = self.available.lock().await;
        let queue = map.entry(tmpl.key.clone()).or_default();
        queue.push_back(tmpl);

        // Evict oldest entries if over limit
        while queue.len() > self.max_per_key {
            if let Some(evicted) = queue.pop_front() {
                let evicted_dir = self.store_dir.join(&evicted.id);
                if let Err(e) = tokio::fs::remove_dir_all(&evicted_dir).await {
                    log::warn!(
                        "template pool: evict {}: remove dir {}: {}",
                        evicted.id,
                        evicted_dir.display(),
                        e
                    );
                }
            }
        }

        self.metrics.record_template_created();
        Ok(())
    }

    /// Acquire a template for the given key (LIFO — most recently added).
    ///
    /// The template is removed from the pool; the caller is responsible for either
    /// consuming it (sandbox restore) or returning it via `release()` on failure.
    pub async fn acquire(&self, key: &TemplateKey) -> Option<PooledTemplate> {
        let mut map = self.available.lock().await;
        map.get_mut(key)?.pop_back()
    }

    /// Return a template to the pool (e.g. after a failed restore).
    pub async fn release(&self, tmpl: PooledTemplate) {
        let mut map = self.available.lock().await;
        map.entry(tmpl.key.clone()).or_default().push_back(tmpl);
    }

    /// Number of available templates for the given key.
    pub async fn depth(&self, key: &TemplateKey) -> usize {
        self.available
            .lock()
            .await
            .get(key)
            .map(|q| q.len())
            .unwrap_or(0)
    }

    /// Total number of available templates across all keys.
    pub async fn total_depth(&self) -> usize {
        self.available
            .lock()
            .await
            .values()
            .map(|q| q.len())
            .sum()
    }

    /// Number of distinct keys that have at least one available template.
    pub async fn key_count(&self) -> usize {
        self.available
            .lock()
            .await
            .values()
            .filter(|q| !q.is_empty())
            .count()
    }
}

/// Request to create a pre-warmed VM template snapshot by booting a fresh VM.
pub struct CreateTemplateRequest {
    /// Unique identifier for this template entry.
    pub id: String,
}

impl CreateTemplateRequest {
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use temp_dir::TempDir;

    fn make_key() -> TemplateKey {
        TemplateKey::new("/var/lib/kuasar/rootfs.img", 2, 512)
    }

    fn make_template(id: &str) -> PooledTemplate {
        PooledTemplate::new(
            id,
            make_key(),
            PathBuf::from(format!("/var/lib/kuasar/templates/{}/snapshot", id)),
            "/var/lib/kuasar/rootfs.img",
            "/var/lib/kuasar/vmlinux.bin",
            2,
            512,
            format!("/var/lib/kuasar/templates/{}/task.vsock", id),
            format!("/tmp/{}-task.log", id),
        )
    }

    #[tokio::test]
    async fn test_pooled_template_save_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        let tmpl = make_template("tmpl-001");
        tmpl.save(dir.path()).await.unwrap();
        let loaded = PooledTemplate::load(dir.path()).await.unwrap();
        assert_eq!(loaded.id, tmpl.id);
        assert_eq!(loaded.key, tmpl.key);
        assert_eq!(loaded.vcpus, 2);
        assert_eq!(loaded.memory_mb, 512);
    }

    #[tokio::test]
    async fn test_pooled_template_load_missing_returns_error() {
        let dir = TempDir::new().unwrap();
        let result = PooledTemplate::load(dir.path()).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("pooled_template.json"), "got: {}", msg);
    }

    #[tokio::test]
    async fn test_pool_add_acquire_lifo() {
        let dir = TempDir::new().unwrap();
        let pool = TemplatePool::new(dir.path().to_path_buf(), 3);
        let key = make_key();

        let t1 = make_template("t1");
        let t2 = make_template("t2");
        pool.add(t1).await.unwrap();
        pool.add(t2).await.unwrap();

        // LIFO: t2 was added last, should be acquired first
        let acquired = pool.acquire(&key).await.unwrap();
        assert_eq!(acquired.id, "t2");
        assert_eq!(pool.depth(&key).await, 1);
    }

    #[tokio::test]
    async fn test_pool_eviction_on_overflow() {
        let dir = TempDir::new().unwrap();
        // Create subdirs so remove_dir_all has something to delete
        for name in &["t1", "t2", "t3", "t4"] {
            tokio::fs::create_dir_all(dir.path().join(name))
                .await
                .unwrap();
        }
        let pool = TemplatePool::new(dir.path().to_path_buf(), 2);
        let key = make_key();

        for id in &["t1", "t2", "t3"] {
            let mut tmpl = make_template(id);
            // point snapshot_dir into temp dir so eviction can find the dir
            tmpl.id = id.to_string();
            pool.add(tmpl).await.unwrap();
        }

        // max_per_key=2: t1 (oldest) should have been evicted
        assert_eq!(pool.depth(&key).await, 2);

        let a = pool.acquire(&key).await.unwrap();
        let b = pool.acquire(&key).await.unwrap();
        // LIFO: t3, then t2 (t1 evicted)
        assert_eq!(a.id, "t3");
        assert_eq!(b.id, "t2");
        assert!(pool.acquire(&key).await.is_none());
    }

    #[tokio::test]
    async fn test_pool_release_puts_back() {
        let dir = TempDir::new().unwrap();
        let pool = TemplatePool::new(dir.path().to_path_buf(), 3);
        let key = make_key();

        pool.add(make_template("t1")).await.unwrap();
        let tmpl = pool.acquire(&key).await.unwrap();
        assert!(pool.acquire(&key).await.is_none());

        pool.release(tmpl).await;
        assert_eq!(pool.depth(&key).await, 1);
    }

    #[tokio::test]
    async fn test_metrics_hit_rate() {
        let metrics = TemplateMetrics::default();
        assert_eq!(metrics.hit_rate(), 0.0);

        metrics.record_hit(100);
        metrics.record_hit(200);
        metrics.record_miss();

        // 2 hits, 1 miss → hit_rate = 2/3
        let rate = metrics.hit_rate();
        assert!((rate - 2.0 / 3.0).abs() < 1e-9, "got {}", rate);
        let avg = metrics.avg_restore_ms();
        assert!((avg - 150.0).abs() < 1e-9, "got {}", avg);
    }

    #[tokio::test]
    async fn test_load_from_disk_rehydrates() {
        let dir = TempDir::new().unwrap();
        {
            let pool = TemplatePool::new(dir.path().to_path_buf(), 5);
            pool.add(make_template("t1")).await.unwrap();
            pool.add(make_template("t2")).await.unwrap();
        }
        // Reload from disk
        let pool2 = TemplatePool::load_from_disk(dir.path().to_path_buf(), 5)
            .await
            .unwrap();
        assert_eq!(pool2.total_depth().await, 2);
    }
}
