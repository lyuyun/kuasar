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

//! VM factory for Cloud Hypervisor appliance sandboxes.
//!
//! Differences from [`CloudHypervisorVMFactory`]:
//!
//! - **No virtiofs device**: appliance containers manage their own filesystem;
//!   the host does not need to expose a shared directory.
//! - **Reverse vsock direction**: the guest's `kuasar-init` connects *to* the
//!   host rather than the host connecting to a `vmm-task` server.  Cloud
//!   Hypervisor proxies guest-originated vsock connections to a Unix socket on
//!   the host at `<vsock_socket>_<port>` (i.e. `task.vsock_1024`).
//!   [`ApplianceRuntime`] binds that socket and waits for the READY message.

use containerd_sandbox::SandboxOption;

use crate::{
    cloud_hypervisor::{
        config::CloudHypervisorVMConfig,
        devices::{console::Console, pmem::Pmem, rng::Rng, vsock::Vsock},
        CloudHypervisorVM,
    },
    utils::get_netns,
    vm::VMFactory,
};

/// [`VMFactory`] for Cloud Hypervisor appliance sandboxes.
pub struct CloudHypervisorApplianceVMFactory {
    vm_config: CloudHypervisorVMConfig,
}

#[async_trait::async_trait]
impl VMFactory for CloudHypervisorApplianceVMFactory {
    type VM = CloudHypervisorVM;
    type Config = CloudHypervisorVMConfig;

    fn new(config: Self::Config) -> Self {
        Self { vm_config: config }
    }

    async fn create_vm(
        &self,
        id: &str,
        s: &SandboxOption,
    ) -> containerd_sandbox::error::Result<Self::VM> {
        let netns = get_netns(&s.sandbox);
        let mut vm = CloudHypervisorVM::new(id, &netns, &s.base_dir, &self.vm_config);

        // add image as a disk
        if !self.vm_config.common.image_path.is_empty() {
            let rootfs_device = Pmem::new("rootfs", &self.vm_config.common.image_path, true);
            vm.add_device(rootfs_device);
        }

        // add virtio-rng device
        if !self.vm_config.entropy_source.is_empty() {
            let rng = Rng::new("rng", &self.vm_config.entropy_source);
            vm.add_device(rng);
        }

        // Add vsock device for guest→host reverse connection.
        //
        // Cloud Hypervisor vsock reverse-connection naming convention:
        //   host socket path = <vsock_socket_path>_<port>
        //
        // The guest's kuasar-init connects to vsock CID 2 (host) port 1024.
        // Cloud Hypervisor proxies that connection to `task.vsock_1024` on the host.
        // ApplianceRuntime will bind and listen on that socket.
        let vsock_socket_path = format!("{}/task.vsock", s.base_dir);
        let vsock = Vsock::new(3, &vsock_socket_path, "vsock");
        vm.add_device(vsock);

        // The address ApplianceRuntime will bind to wait for the guest READY message.
        // Format: unix://<path> so ApplianceRuntime::wait_ready can parse the scheme.
        vm.agent_socket = format!("unix://{}/task.vsock_1024", s.base_dir);

        // add console device
        let console_path = format!("/tmp/{}-task.log", id);
        let console = Console::new(&console_path, "console");
        vm.add_device(console);

        // NOTE: No Fs (virtiofsd) device — appliance sandboxes have no shared directory.

        Ok(vm)
    }
}
