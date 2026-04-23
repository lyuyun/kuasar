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

//! Post-restore initialisation for snapshot-restored sandboxes.
//!
//! After Cloud Hypervisor restores a VM from snapshot, two steps must run
//! before the guest is safe to use:
//!
//! 1. **EntropyInjector** — The guest kernel's entropy pool was frozen in the
//!    snapshot.  All restored instances start with identical PRNG state, which
//!    can lead to TLS key collisions.  We inject 256 bytes of fresh host
//!    entropy via `ExecVMProcess` → `dd` → `/dev/random`.
//!
//! 2. **ClockSynchronizer** — Reuse the existing `client_sync_clock` helper to
//!    correct the guest clock drift caused by the restore time gap.
//!
//! Network reconfiguration is handled by the caller (`KuasarSandbox::post_restore_init`)
//! because `Network` is not `Clone`.

use std::sync::Arc;

use anyhow::anyhow;
use containerd_sandbox::error::Result;
use log::{debug, info, warn};
use tokio::sync::Mutex;
use vmm_common::api::sandbox_ttrpc::SandboxServiceClient;

use crate::client::{client_inject_entropy, client_sync_clock};
use containerd_sandbox::signal::ExitSignal;

/// Runs entropy injection and clock synchronisation after every snapshot restore.
pub struct PostRestoreInitializer {
    pub client: Arc<Mutex<Option<SandboxServiceClient>>>,
    pub exit_signal: Arc<ExitSignal>,
    pub sandbox_id: String,
}

impl PostRestoreInitializer {
    pub async fn run(&self) -> Result<()> {
        info!("sandbox {}: starting post-restore initialisation", self.sandbox_id);

        let client_guard = self.client.lock().await;
        let client = client_guard
            .as_ref()
            .ok_or_else(|| anyhow!("post-restore: ttrpc client not initialised"))?;

        // Step 1: entropy injection (must precede any crypto operations).
        if let Err(e) = client_inject_entropy(client).await {
            // Non-fatal: log and continue — the guest will still accumulate
            // entropy over time, just with a wider initial collision window.
            warn!("sandbox {}: entropy injection failed: {}", self.sandbox_id, e);
        } else {
            debug!("sandbox {}: entropy injected", self.sandbox_id);
        }

        // Step 2: clock synchronisation.
        client_sync_clock(client, &self.sandbox_id, self.exit_signal.clone());
        debug!("sandbox {}: clock sync scheduled", self.sandbox_id);

        info!(
            "sandbox {}: post-restore initialisation complete",
            self.sandbox_id
        );
        Ok(())
    }
}
