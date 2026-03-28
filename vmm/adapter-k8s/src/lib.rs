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

pub mod sandboxer;
pub mod task;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use vmm_engine::SandboxEngine;
use vmm_guest_runtime::{ContainerRuntime, GuestReadiness};
use vmm_vm_trait::{Hooks, Vmm};

/// K8s Adapter: wraps `SandboxEngine` and implements containerd Sandbox API + Task API.
pub struct K8sAdapter<V: Vmm, R: GuestReadiness + ContainerRuntime, H: Hooks<V>> {
    pub(crate) engine: Arc<SandboxEngine<V, R, H>>,
    #[allow(dead_code)]
    pub(crate) graceful_stop_timeout_ms: u64,
    // publisher: Option<RemotePublisher> — wired in `serve()` when the containerd
    // ttrpc address is available; None during unit tests.
}

impl<V: Vmm, R: GuestReadiness + ContainerRuntime, H: Hooks<V>> K8sAdapter<V, R, H> {
    pub fn new(engine: SandboxEngine<V, R, H>, config: K8sAdapterConfig) -> Self {
        Self {
            engine: Arc::new(engine),
            graceful_stop_timeout_ms: config.graceful_stop_timeout_ms,
        }
    }

    /// Re-hydrate in-flight sandboxes from state files in `dir`.
    ///
    /// Called once at startup before `serve`. Delegates to the engine's
    /// recovery logic which loads `{work_dir}/{id}.json` files and pings each VMM.
    pub async fn recover(&self, dir: &str)
    where
        V: serde::Serialize + serde::de::DeserializeOwned + 'static + Send + Sync,
        R: 'static,
        H: 'static,
    {
        self.engine.recover(dir).await;
    }

    /// Start serving the containerd Sandbox API on the given socket.
    pub async fn serve(self, listen: &str, dir: &str) -> Result<()>
    where
        V: serde::Serialize + serde::de::DeserializeOwned + 'static + Send + Sync,
        R: 'static,
        H: 'static,
    {
        containerd_sandbox::run("kuasar-vmm-sandboxer", listen, dir, self)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))
    }
}

/// Configuration for `K8sAdapter`.
#[derive(Deserialize, Clone)]
pub struct K8sAdapterConfig {
    /// How long (ms) to wait for graceful container shutdown before force-killing.
    pub graceful_stop_timeout_ms: u64,
}

impl Default for K8sAdapterConfig {
    fn default() -> Self {
        Self {
            graceful_stop_timeout_ms: 10_000,
        }
    }
}
