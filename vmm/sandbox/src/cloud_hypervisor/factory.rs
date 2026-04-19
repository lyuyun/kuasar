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

use containerd_sandbox::SandboxOption;

use crate::{
    cloud_hypervisor::{
        config::CloudHypervisorVMConfig,
        devices::{console::Console, fs::Fs, pmem::Pmem, rng::Rng, vsock::Vsock},
        CloudHypervisorVM,
    },
    profile::SandboxProfile,
    utils::get_netns,
    vm::VMFactory,
};

/// Cloud Hypervisor vsock reverse-connection naming convention:
///   host socket path = <vsock_socket_path>_<port>
///
/// In Appliance mode the guest's init connects to vsock CID 2 (host) port 1024.
/// Cloud Hypervisor proxies that connection to `task.vsock_1024` on the host.
/// [`ApplianceRuntime`] binds and listens on that path.
///
/// [`ApplianceRuntime`]: crate::guest_runtime::ApplianceRuntime
const APPLIANCE_VSOCK_PORT: u32 = 1024;

pub struct CloudHypervisorVMFactory {
    vm_config: CloudHypervisorVMConfig,
    profile: SandboxProfile,
}

impl CloudHypervisorVMFactory {
    pub fn new(vm_config: CloudHypervisorVMConfig, profile: SandboxProfile) -> Self {
        Self { vm_config, profile }
    }
}

#[async_trait::async_trait]
impl VMFactory for CloudHypervisorVMFactory {
    type VM = CloudHypervisorVM;

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

        // Add vsock device.
        //
        // Standard: host connects to guest vmm-task ttrpc server via hvsock.
        // Appliance: guest connects back to host; CLH proxies the connection to
        //   a Unix socket at `<vsock_socket_path>_<port>` on the host.
        let vsock_socket_path = format!("{}/task.vsock", s.base_dir);
        let vsock = Vsock::new(3, &vsock_socket_path, "vsock");
        vm.add_device(vsock);

        vm.agent_socket = match &self.profile {
            SandboxProfile::Standard => format!("hvsock://{}:1024", vsock_socket_path),
            SandboxProfile::Appliance => {
                format!("unix://{}_{}", vsock_socket_path, APPLIANCE_VSOCK_PORT)
            }
        };

        // add console device
        let console_path = format!("/tmp/{}-task.log", id);
        let console = Console::new(&console_path, "console");
        vm.add_device(console);

        // add virtio-fs device (Appliance sandboxes have no shared directory)
        if self.profile.needs_virtiofsd() && !vm.virtiofsd_config.socket_path.is_empty() {
            let fs = Fs::new("fs", &vm.virtiofsd_config.socket_path, "kuasar");
            vm.add_device(fs);
        }

        Ok(vm)
    }
}
