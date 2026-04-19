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

use containerd_sandbox::error::Result;
use log::info;

use crate::{
    cloud_hypervisor::CloudHypervisorVM,
    profile::SandboxProfile,
    sandbox::KuasarSandbox,
    utils::get_resources,
    vm::Hooks,
};

pub struct CloudHypervisorHooks {
    profile: SandboxProfile,
}

impl CloudHypervisorHooks {
    pub fn new(profile: SandboxProfile) -> Self {
        Self { profile }
    }
}

#[async_trait::async_trait]
impl Hooks<CloudHypervisorVM> for CloudHypervisorHooks {
    /// Inject the profile-appropriate [`GuestRuntime`], replacing the default
    /// `VmmTaskRuntime` set at sandbox construction time.
    ///
    /// [`GuestRuntime`]: crate::guest_runtime::GuestRuntime
    async fn post_create(&self, sandbox: &mut KuasarSandbox<CloudHypervisorVM>) -> Result<()> {
        sandbox.runtime_kind = self.profile.runtime_kind();
        sandbox.runtime = Some(self.profile.create_runtime(&sandbox.id));
        Ok(())
    }

    /// Configure VM resources and, for Standard mode, start virtiofsd before
    /// the VM boots.
    async fn pre_start(&self, sandbox: &mut KuasarSandbox<CloudHypervisorVM>) -> Result<()> {
        process_config(sandbox).await?;
        if self.profile.needs_virtiofsd() && !sandbox.vm.virtiofsd_config.socket_path.is_empty() {
            let pid = sandbox.vm.start_virtiofsd().await?;
            info!(
                "virtiofsd for sandbox {} started with pid {}",
                sandbox.id, pid
            );
            sandbox.vm.pids.affiliated_pids.push(pid);
        }
        Ok(())
    }

    /// After the VM is running and the guest runtime has completed its
    /// handshake, set the containerd task address (Standard only) and start
    /// background clock sync.
    async fn post_start(&self, sandbox: &mut KuasarSandbox<CloudHypervisorVM>) -> Result<()> {
        if let Some(addr) = self.profile.task_address(&sandbox.vm.agent_socket) {
            sandbox.data.task_address = addr;
            sandbox.start_clock_sync();
        }
        info!("sandbox {} started (profile: {:?})", sandbox.id, self.profile);
        Ok(())
    }
}

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
