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

use anyhow::anyhow;
use containerd_sandbox::SandboxOption;

use crate::{
    cloud_hypervisor::{
        config::CloudHypervisorVMConfig,
        devices::{console::Console, fs::Fs, pmem::Pmem, rng::Rng, vsock::Vsock},
        CloudHypervisorVM,
    },
    profile::{SandboxProfile, ANNOTATION_APPLIANCE_APP, ANNOTATION_APPLIANCE_IMAGE},
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

    fn supports_appliance_mode(&self) -> bool {
        true
    }

    /// Cluster-wide default app executable for Appliance mode; read from the
    /// `appliance_app` field in the `[hypervisor]` TOML section.  Used by
    /// `ApplianceRuntime` as a fallback when the pod annotation is absent.
    fn appliance_default_app(&self) -> &str {
        &self.vm_config.appliance_app
    }

    async fn create_vm(
        &self,
        id: &str,
        s: &SandboxOption,
    ) -> containerd_sandbox::error::Result<Self::VM> {
        let netns = get_netns(&s.sandbox);
        let mut vm =
            CloudHypervisorVM::new(id, &netns, &s.base_dir, &self.vm_config, &self.profile);

        // Resolve the VM disk image path.
        //
        // Appliance mode (workaround): the pod annotation `io.kuasar.appliance.image`
        // must be set to the **absolute local path** of the pre-staged erofs image.
        // Falls back to `image_path` from the hypervisor config when the annotation is
        // absent (useful when all pods boot the same appliance image).
        //
        // TODO: replace with automatic resolution via ContainerData.rootfs once the
        // deferred-boot flow (append_container → boot) is implemented.
        //
        // Standard mode: always use the configured `image_path`.
        let image_path: &str = if matches!(self.profile, SandboxProfile::Appliance) {
            s.sandbox
                .config
                .as_ref()
                .and_then(|c| c.annotations.get(ANNOTATION_APPLIANCE_IMAGE))
                .map(String::as_str)
                .filter(|p| !p.is_empty())
                .unwrap_or(self.vm_config.common.image_path.as_str())
        } else {
            self.vm_config.common.image_path.as_str()
        };

        if image_path.is_empty() {
            if matches!(self.profile, SandboxProfile::Appliance) {
                return Err(anyhow!(
                    "Appliance mode requires a VM image; set annotation '{}' on the pod \
                     to the absolute path of the disk image, or set 'image_path' in \
                     the hypervisor config for a single shared image",
                    ANNOTATION_APPLIANCE_IMAGE
                )
                .into());
            }
        } else {
            vm.add_device(Pmem::new("rootfs", image_path, true));
        }

        // add virtio-rng device
        if !self.vm_config.entropy_source.is_empty() {
            let rng = Rng::new("rng", &self.vm_config.entropy_source);
            vm.add_device(rng);
        }

        // add vsock device
        // set guest cid
        // cid seems not important for cloud hypervisor
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

        // Appliance mode: inject only KUASAR_SANDBOX_ID into the kernel cmdline.
        // All other sandbox configuration (app, args, env, hostname, DNS, …) is
        // delivered to kuasar-init via the vsock BOOTSTRAP message after boot,
        // removing the 4096-byte cmdline length limit and all escaping constraints.
        //
        // Early validation: fail here (at sandbox create time) if no app is configured,
        // rather than discovering the problem later at VM boot.
        if matches!(self.profile, SandboxProfile::Appliance) {
            vm.config
                .cmdline
                .push_str(&format!(" KUASAR_SANDBOX_ID={}", id));

            let app = s
                .sandbox
                .config
                .as_ref()
                .and_then(|c| c.annotations.get(ANNOTATION_APPLIANCE_APP))
                .map(String::as_str)
                .filter(|p| !p.is_empty())
                .unwrap_or(self.vm_config.appliance_app.as_str());

            if app.is_empty() {
                return Err(anyhow!(
                    "Appliance mode requires an application path; set annotation '{}' on \
                     the pod or set 'appliance_app' in the hypervisor config",
                    ANNOTATION_APPLIANCE_APP
                )
                .into());
            }
            // app is valid — ApplianceRuntime will include it in the BOOTSTRAP message.
        }

        // add console device
        // TODO add log path parameter
        let console_path = format!("/tmp/{}-task.log", id);
        let console = Console::new(&console_path, "console");
        vm.add_device(console);

        // add virtio-fs device (skipped when profile does not need virtiofsd)
        if self.profile.needs_virtiofsd() && !vm.virtiofsd_config.socket_path.is_empty() {
            let fs = Fs::new("fs", &vm.virtiofsd_config.socket_path, "kuasar");
            vm.add_device(fs);
        }

        Ok(vm)
    }
}
