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

//! Lifecycle hooks for Cloud Hypervisor appliance sandboxes.
//!
//! Key differences from [`CloudHypervisorHooks`]:
//!
//! - **`post_create`**: replaces the default `VmmTaskRuntime` with
//!   [`ApplianceRuntime`] and sets `runtime_kind = RuntimeKind::Appliance`
//!   so the correct runtime is reconstructed on recovery.
//! - **`pre_start`**: skips virtiofsd startup entirely — appliance containers
//!   manage their own filesystem.
//! - **`post_start`**: does not set a ttrpc `task_address` — appliance
//!   sandboxes do not use the containerd task API over vsock.

use containerd_sandbox::error::Result;
use log::info;

use crate::{
    cloud_hypervisor::CloudHypervisorVM,
    guest_runtime::{ApplianceRuntime, RuntimeKind},
    sandbox::KuasarSandbox,
    utils::get_resources,
    vm::Hooks,
};

/// Lifecycle hooks for Cloud Hypervisor appliance sandboxes.
#[derive(Default)]
pub struct CloudHypervisorApplianceHooks {}

#[async_trait::async_trait]
impl Hooks<CloudHypervisorVM> for CloudHypervisorApplianceHooks {
    /// Inject [`ApplianceRuntime`] into the sandbox, replacing the default
    /// `VmmTaskRuntime` that `KuasarSandboxer::create()` sets.
    async fn post_create(&self, sandbox: &mut KuasarSandbox<CloudHypervisorVM>) -> Result<()> {
        sandbox.runtime_kind = RuntimeKind::Appliance;
        sandbox.runtime = Some(Box::new(ApplianceRuntime::new(&sandbox.id)));
        Ok(())
    }

    /// Configure VM resources.  Virtiofsd is intentionally NOT started here.
    async fn pre_start(&self, sandbox: &mut KuasarSandbox<CloudHypervisorVM>) -> Result<()> {
        process_config(sandbox).await?;
        Ok(())
    }

    /// Called after the VM is running and `ApplianceRuntime::wait_ready` has
    /// returned successfully.
    ///
    /// Appliance sandboxes do not use the containerd ttrpc task API, so there
    /// is no `task_address` to set.  Clock sync and event forwarding are no-ops
    /// (handled in `KuasarSandbox::start` via the runtime).
    async fn post_start(&self, sandbox: &mut KuasarSandbox<CloudHypervisorVM>) -> Result<()> {
        info!("appliance sandbox {} started", sandbox.id);
        Ok(())
    }
}

/// Adjust VM CPU / memory limits from pod resource requests.
async fn process_config(sandbox: &mut KuasarSandbox<CloudHypervisorVM>) -> Result<()> {
    if let Some(resources) = get_resources(&sandbox.data) {
        if resources.cpu_period > 0 && resources.cpu_quota > 0 {
            let base = (resources.cpu_quota as f64 / resources.cpu_period as f64).ceil();
            sandbox.vm.config.cpus.boot = base as u32;
            sandbox.vm.config.cpus.max = Some(base as u32);
        }
        if resources.memory_limit_in_bytes > 0 {
            sandbox.vm.config.memory.size = resources.memory_limit_in_bytes as u64;
        }
    }
    Ok(())
}
