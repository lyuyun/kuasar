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

//! Prometheus metrics for application snapshot operations.
//!
//! All metrics are registered in the default global registry.
//!
//! ## Metric catalogue
//!
//! | Name | Type | Labels | Description |
//! |---|---|---|---|
//! | `kuasar_snapshot_create_total` | Counter | — | Snapshots successfully created |
//! | `kuasar_snapshot_restore_total` | Counter | `result` (ok/err) | Restore attempts |
//! | `kuasar_snapshot_restore_duration_seconds` | Histogram | — | Restore latency |
//! | `kuasar_snapshot_store_size_bytes` | Gauge | `type` (memory/state) | Store size |
//! | `kuasar_warm_pool_size` | GaugeVec | `key` | Ready slots per snapshot key |
//! | `kuasar_warm_pool_acquire_total` | CounterVec | `key`, `result` (hit/miss) | Acquire outcomes |

use std::time::Instant;

use lazy_static::lazy_static;
use prometheus::{
    exponential_buckets, register_counter, register_counter_vec, register_gauge_vec,
    register_histogram, Counter, CounterVec, GaugeVec, Histogram,
};

lazy_static! {
    pub static ref SNAPSHOT_CREATE_TOTAL: Counter = register_counter!(
        "kuasar_snapshot_create_total",
        "Total number of application snapshots successfully created"
    )
    .expect("register kuasar_snapshot_create_total");

    pub static ref SNAPSHOT_RESTORE_TOTAL: CounterVec = register_counter_vec!(
        "kuasar_snapshot_restore_total",
        "Total number of VM restore operations",
        &["result"]
    )
    .expect("register kuasar_snapshot_restore_total");

    pub static ref SNAPSHOT_RESTORE_DURATION_SECONDS: Histogram = register_histogram!(
        "kuasar_snapshot_restore_duration_seconds",
        "Time taken to restore a VM from snapshot (seconds)",
        // Buckets: 100ms … 60s — covers fast NVMe (< 1s) to slow network storage (> 30s).
        exponential_buckets(0.1, 2.0, 10).expect("buckets")
    )
    .expect("register kuasar_snapshot_restore_duration_seconds");

    pub static ref SNAPSHOT_STORE_SIZE_BYTES: GaugeVec = register_gauge_vec!(
        "kuasar_snapshot_store_size_bytes",
        "Total bytes consumed by snapshot files, by type",
        &["file_type"]
    )
    .expect("register kuasar_snapshot_store_size_bytes");

    pub static ref WARM_POOL_SIZE: GaugeVec = register_gauge_vec!(
        "kuasar_warm_pool_size",
        "Number of ready (pre-restored) VM slots in the warm pool",
        &["key"]
    )
    .expect("register kuasar_warm_pool_size");

    pub static ref WARM_POOL_ACQUIRE_TOTAL: CounterVec = register_counter_vec!(
        "kuasar_warm_pool_acquire_total",
        "Total warm-pool acquire calls, labelled by hit/miss outcome",
        &["key", "result"]
    )
    .expect("register kuasar_warm_pool_acquire_total");
}

// ──────────────────────────────────────────────
// Recording helpers — thin wrappers that callers use instead of touching
// the raw Prometheus metrics directly.
// ──────────────────────────────────────────────

/// Record a successful snapshot create.
#[inline]
pub fn record_snapshot_created() {
    SNAPSHOT_CREATE_TOTAL.inc();
}

/// Record a restore attempt outcome.
#[inline]
pub fn record_restore(success: bool) {
    let label = if success { "ok" } else { "err" };
    SNAPSHOT_RESTORE_TOTAL.with_label_values(&[label]).inc();
}

/// Record restore duration.  Call `RestoreTimer::start()` before the
/// restore and `timer.observe()` once it completes.
pub struct RestoreTimer(Instant);

impl RestoreTimer {
    pub fn start() -> Self {
        RestoreTimer(Instant::now())
    }

    pub fn observe(self) {
        let elapsed = self.0.elapsed().as_secs_f64();
        SNAPSHOT_RESTORE_DURATION_SECONDS.observe(elapsed);
    }
}

/// Update the store-size gauge for both file types.
pub fn record_store_sizes(memory_bytes: u64, state_bytes: u64) {
    SNAPSHOT_STORE_SIZE_BYTES
        .with_label_values(&["memory"])
        .set(memory_bytes as f64);
    SNAPSHOT_STORE_SIZE_BYTES
        .with_label_values(&["state"])
        .set(state_bytes as f64);
}

/// Set the current warm-pool size for a key.
pub fn set_warm_pool_size(key: &str, count: usize) {
    WARM_POOL_SIZE
        .with_label_values(&[key])
        .set(count as f64);
}

/// Record a warm-pool acquire hit.
pub fn record_warm_pool_hit(key: &str) {
    WARM_POOL_ACQUIRE_TOTAL
        .with_label_values(&[key, "hit"])
        .inc();
}

/// Record a warm-pool acquire miss.
pub fn record_warm_pool_miss(key: &str) {
    WARM_POOL_ACQUIRE_TOTAL
        .with_label_values(&[key, "miss"])
        .inc();
}

/// Serialize all registered metrics to the Prometheus text exposition format.
/// Wire this up to an HTTP endpoint (e.g. `/metrics`) to allow scraping.
pub fn gather_text() -> Result<String, prometheus::Error> {
    use prometheus::Encoder;
    let encoder = prometheus::TextEncoder::new();
    let mut buf = Vec::new();
    encoder.encode(&prometheus::gather(), &mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}
