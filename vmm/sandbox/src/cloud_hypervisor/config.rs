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

use sandbox_derive::{CmdLineParamSet, CmdLineParams};
use serde::{Deserialize, Serialize};

use crate::{profile::SandboxProfile, vm::HypervisorCommonConfig};

// ── Standard mode ─────────────────────────────────────────────────────────────

const DEFAULT_KERNEL_PARAMS: &str = "console=hvc0 \
root=/dev/pmem0p1 \
rootflags=data=ordered,errors=remount-ro \
ro rootfstype=ext4 \
task.sharefs_type=virtiofs";

// ── Appliance mode ────────────────────────────────────────────────────────────
//
// The image occupies the whole pmem device (no partition table), so `root`
// points to `/dev/pmem0` rather than `/dev/pmem0p1`.
//
// Both variants mount the root read-only.  kuasar-init layers a tmpfs upper
// via overlayfs so the workload can write freely without touching the shared image.

/// Appliance mode with ext4 root filesystem (default, broad guest kernel support).
///
/// Uses `pmem0` (whole device, no partition table).  Appliance images are
/// flat filesystems written directly to the image file, not the DAX-headed
/// partitioned layout used by Standard mode.
const DEFAULT_KERNEL_PARAMS_APPLIANCE_EXT4: &str =
    "console=hvc0 root=/dev/pmem0 ro rootfstype=ext4";

/// Appliance mode with erofs root filesystem (more compact images; requires
/// guest kernel ≥ 5.4, production use needs ≥ 5.16).
///
/// Uses `pmem0` (whole device, no partition table) because `mkfs.erofs` packs
/// a directory tree directly into a flat image without a partition table.
const DEFAULT_KERNEL_PARAMS_APPLIANCE_EROFS: &str =
    "console=hvc0 root=/dev/pmem0 ro rootfstype=erofs";

/// Root filesystem format used for the Appliance mode pmem disk image.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApplianceFsType {
    /// ext4 — mounted read-only; widely supported by guest kernels.
    /// This is the default when the field is absent from the config file.
    #[default]
    Ext4,
    /// erofs — read-only compressed filesystem; requires guest kernel ≥ 5.4.
    Erofs,
}

#[derive(Deserialize)]
pub struct CloudHypervisorVMConfig {
    pub path: String,
    #[serde(flatten)]
    pub common: HypervisorCommonConfig,
    pub hugepages: bool,
    pub entropy_source: String,
    pub task: TaskConfig,
    pub virtiofsd: VirtiofsdConfig,
    /// Filesystem type of the Appliance mode pmem disk image.
    /// Ignored in Standard mode.  Defaults to `ext4`.
    #[serde(default)]
    pub appliance_fs_type: ApplianceFsType,
    /// Default application executable path for Appliance mode.
    /// Overridden per-pod by the `io.kuasar.appliance.app` annotation.
    /// Ignored in Standard mode.
    #[serde(default)]
    pub appliance_app: String,
}

impl Default for CloudHypervisorVMConfig {
    fn default() -> Self {
        Self {
            path: "/usr/local/bin/cloud-hypervisor".to_string(),
            common: HypervisorCommonConfig::default(),
            hugepages: false,
            entropy_source: "/dev/urandom".to_string(),
            task: TaskConfig::default(),
            virtiofsd: VirtiofsdConfig::default(),
            appliance_fs_type: ApplianceFsType::default(),
            appliance_app: String::new(),
        }
    }
}

#[derive(Deserialize, Default)]
pub struct TaskConfig {
    pub debug: bool,
    pub enable_tracing: bool,
}

#[derive(CmdLineParamSet, Deserialize, Clone, Serialize)]
pub struct VirtiofsdConfig {
    #[param(ignore)]
    pub path: String,
    pub log_level: String,
    pub cache: String,
    pub thread_pool_size: u32,
    #[serde(default)]
    pub socket_path: String,
    #[serde(default)]
    pub shared_dir: String,
    #[serde(default)]
    pub syslog: bool,
}

impl Default for VirtiofsdConfig {
    fn default() -> Self {
        Self {
            path: "/usr/local/bin/virtiofsd".to_string(),
            log_level: "info".to_string(),
            cache: "never".to_string(),
            thread_pool_size: 4,
            socket_path: "".to_string(),
            shared_dir: "".to_string(),
            syslog: true,
        }
    }
}

#[derive(CmdLineParamSet, Default, Clone, Serialize, Deserialize)]
pub struct CloudHypervisorConfig {
    #[param(ignore)]
    pub path: String,
    pub api_socket: String,
    pub cpus: Cpus,
    pub memory: Memory,
    pub kernel: String,
    pub cmdline: String,
    pub initramfs: Option<String>,
    pub log_file: Option<String>,
    #[param(ignore)]
    pub debug: bool,
}

#[derive(CmdLineParams, Default, Clone, Serialize, Deserialize)]
pub struct Cpus {
    pub(crate) boot: u32,
    pub(crate) max: Option<u32>,
    pub(crate) topology: Option<String>,
    #[property(generator = "crate::utils::vec_to_string")]
    pub(crate) affinity: Vec<String>,
    #[property(generator = "crate::utils::vec_to_string")]
    pub(crate) features: Vec<String>,
}

impl Cpus {
    pub fn new(boot: u32) -> Self {
        Self {
            boot,
            max: None,
            topology: None,
            affinity: vec![],
            features: vec![],
        }
    }
}

#[derive(CmdLineParams, Default, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub(crate) size: u64,
    #[property(generator = "crate::utils::bool_to_on_off")]
    pub(crate) shared: bool,
    #[property(generator = "crate::utils::bool_to_on_off")]
    pub(crate) hugepages: bool,
    #[property(key = "hugepage_size")]
    pub(crate) hugepage_size: Option<String>,
    #[property(generator = "crate::utils::bool_to_on_off")]
    pub(crate) prefault: Option<bool>,
    #[property(generator = "crate::utils::bool_to_on_off")]
    pub(crate) thp: Option<bool>,
}

impl Memory {
    pub fn new(size: u64, shared: bool, hugepages: bool) -> Self {
        Self {
            size,
            shared,
            hugepages,
            hugepage_size: None,
            prefault: None,
            thp: None,
        }
    }
}

impl CloudHypervisorConfig {
    pub fn from(vm_config: &CloudHypervisorVMConfig, profile: &SandboxProfile) -> Self {
        let cpus = Cpus::new(vm_config.common.vcpus);
        let memory = Memory::new(
            (vm_config.common.memory_in_mb as u64) * 1024 * 1024,
            true,
            vm_config.hugepages,
        );

        let base_params = match profile {
            SandboxProfile::Standard => DEFAULT_KERNEL_PARAMS,
            SandboxProfile::Appliance => match vm_config.appliance_fs_type {
                ApplianceFsType::Ext4 => DEFAULT_KERNEL_PARAMS_APPLIANCE_EXT4,
                ApplianceFsType::Erofs => DEFAULT_KERNEL_PARAMS_APPLIANCE_EROFS,
            },
        };
        let mut cmdline = format!("{} {}", base_params, vm_config.common.kernel_params);

        // vmm-task params are only meaningful in Standard mode; Appliance VMs
        // run kuasar-init directly and have no vmm-task process inside.
        if matches!(profile, SandboxProfile::Standard) {
            if vm_config.task.debug {
                cmdline.push_str(" task.log_level=debug");
            }
            cmdline.push_str(&format!(
                " task.enable_tracing={}",
                vm_config.task.enable_tracing
            ));
        }

        Self {
            path: vm_config.path.to_string(),
            api_socket: "".to_string(),
            cpus,
            memory,
            kernel: vm_config.common.kernel_path.to_string(),
            cmdline,
            initramfs: None,
            log_file: None,
            debug: vm_config.common.debug,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        cloud_hypervisor::config::{
            ApplianceFsType, CloudHypervisorConfig, CloudHypervisorVMConfig, Cpus, Memory,
        },
        config::Config,
        param::ToCmdLineParams,
        profile::SandboxProfile,
    };

    #[test]
    fn test_cmdline() {
        let config = CloudHypervisorConfig {
            path: "/use/local/bin/cloud-hypervisor".to_string(),
            api_socket: "/tmp/test-api.sock".to_string(),
            cpus: Cpus {
                boot: 2,
                max: None,
                topology: Some("1:1:2:2".to_string()),
                affinity: vec!["[0@[0-3],1@[4-5,8]]".to_string()],
                features: vec!["amx".to_string()],
            },
            memory: Memory {
                size: 1024 * 1024 * 1024 * 4,
                shared: false,
                hugepages: true,
                hugepage_size: Some("2M".to_string()),
                prefault: None,
                thp: None,
            },
            kernel: "/path/to/kernel".to_string(),
            cmdline: "task.sharefs_type=virtiofs".to_string(),
            initramfs: None,
            log_file: None,
            debug: false,
        };
        let params = config.to_cmdline_params("--");
        assert_eq!(params[0], "--api-socket");
        assert_eq!(params[1], "/tmp/test-api.sock");
        assert_eq!(params[2], "--cpus");
        assert_eq!(
            params[3],
            "boot=2,topology=1:1:2:2,affinity=[0@[0-3],1@[4-5,8]],features=amx"
        );
        assert_eq!(params[4], "--memory");
        assert_eq!(
            params[5],
            "size=4294967296,shared=off,hugepages=on,hugepage_size=2M"
        );
        assert_eq!(params[6], "--kernel");
        assert_eq!(params[7], "/path/to/kernel");
        assert_eq!(params[8], "--cmdline");
        assert_eq!(params[9], "task.sharefs_type=virtiofs");
    }

    #[test]
    fn test_toml() {
        let toml_str = "
[sandbox]
enable_tracing = false
[hypervisor]
path = \"/usr/local/bin/cloud-hypervisor\"
vcpus = 1
memory_in_mb = 1024
kernel_path = \"/var/lib/kuasar/vmlinux.bin\"
image_path = \"/var/lib/kuasar/kuasar.img\"
initrd_path = \"\"
kernel_params = \"\"
hugepages = true
entropy_source = \"/dev/urandom\"
[hypervisor.task]
debug = true
enable_tracing = false
[hypervisor.virtiofsd]
path = \"/usr/local/bin/virtiofsd\"
log_level = \"info\"
cache = \"never\"
thread_pool_size = 4
";
        let config: Config<CloudHypervisorVMConfig> = toml::from_str(toml_str).unwrap();
        assert_eq!(config.sandbox.enable_tracing, false);
        assert_eq!(config.hypervisor.path, "/usr/local/bin/cloud-hypervisor");
        assert_eq!(
            config.hypervisor.common.kernel_path,
            "/var/lib/kuasar/vmlinux.bin"
        );
        assert_eq!(config.hypervisor.task.debug, true);
        assert_eq!(config.hypervisor.task.enable_tracing, false);

        assert_eq!(config.hypervisor.common.vcpus, 1);
        assert!(config.hypervisor.hugepages);
        assert_eq!(config.hypervisor.virtiofsd.thread_pool_size, 4);
        assert_eq!(config.hypervisor.virtiofsd.path, "/usr/local/bin/virtiofsd");
    }

    #[test]
    fn test_task_cmdline() {
        let toml_str = "
[sandbox]
enable_tracing = false
[hypervisor]
path = \"/usr/local/bin/cloud-hypervisor\"
vcpus = 1
memory_in_mb = 1024
kernel_path = \"/var/lib/kuasar/vmlinux.bin\"
image_path = \"/var/lib/kuasar/kuasar.img\"
initrd_path = \"\"
kernel_params = \"\"
hugepages = true
entropy_source = \"/dev/urandom\"
[hypervisor.task]
debug = true
enable_tracing = false
[hypervisor.virtiofsd]
path = \"/usr/local/bin/virtiofsd\"
log_level = \"info\"
cache = \"never\"
thread_pool_size = 4
";

        let config: Config<CloudHypervisorVMConfig> = toml::from_str(toml_str).unwrap();
        let chc = CloudHypervisorConfig::from(&config.hypervisor, &SandboxProfile::Standard);

        assert_eq!(chc.cmdline, "console=hvc0 root=/dev/pmem0p1 rootflags=data=ordered,errors=remount-ro ro rootfstype=ext4 task.sharefs_type=virtiofs  task.log_level=debug task.enable_tracing=false");
    }

    // ── Appliance cmdline tests ───────────────────────────────────────────────

    fn appliance_vm_config(fs_type: ApplianceFsType) -> CloudHypervisorVMConfig {
        CloudHypervisorVMConfig {
            appliance_fs_type: fs_type,
            ..CloudHypervisorVMConfig::default()
        }
    }

    #[test]
    fn test_appliance_ext4_cmdline() {
        let vm_cfg = appliance_vm_config(ApplianceFsType::Ext4);
        let chc = CloudHypervisorConfig::from(&vm_cfg, &SandboxProfile::Appliance);

        assert!(
            chc.cmdline.contains("rootfstype=ext4"),
            "expected ext4 in cmdline, got: {}",
            chc.cmdline
        );
        assert!(
            chc.cmdline.contains("root=/dev/pmem0 ") || chc.cmdline.ends_with("root=/dev/pmem0"),
            "appliance ext4 must use whole-device (flat image), got: {}",
            chc.cmdline
        );
        assert!(
            !chc.cmdline.contains("root=/dev/pmem0p"),
            "appliance ext4 must NOT use a partition suffix (image is flat), got: {}",
            chc.cmdline
        );
        assert!(
            !chc.cmdline.contains("task.sharefs_type"),
            "virtiofs param must be absent in Appliance mode"
        );
        assert!(
            !chc.cmdline.contains("task.enable_tracing"),
            "vmm-task param must be absent in Appliance mode"
        );
    }

    #[test]
    fn test_appliance_erofs_cmdline() {
        let vm_cfg = appliance_vm_config(ApplianceFsType::Erofs);
        let chc = CloudHypervisorConfig::from(&vm_cfg, &SandboxProfile::Appliance);

        assert!(
            chc.cmdline.contains("rootfstype=erofs"),
            "expected erofs in cmdline, got: {}",
            chc.cmdline
        );
        assert!(
            !chc.cmdline.contains("task.sharefs_type"),
            "virtiofs param must be absent in Appliance mode"
        );
    }

    #[test]
    fn test_appliance_default_fs_type_is_ext4() {
        let vm_cfg = CloudHypervisorVMConfig::default();
        assert_eq!(vm_cfg.appliance_fs_type, ApplianceFsType::Ext4);
        let chc = CloudHypervisorConfig::from(&vm_cfg, &SandboxProfile::Appliance);
        assert!(chc.cmdline.contains("rootfstype=ext4"));
    }

    #[test]
    fn test_appliance_fs_type_toml_default() {
        // When appliance_fs_type is absent from the TOML the field defaults to Ext4.
        let toml_str = "
[sandbox]
enable_tracing = false
[hypervisor]
path = \"/usr/local/bin/cloud-hypervisor\"
vcpus = 1
memory_in_mb = 1024
kernel_path = \"/var/lib/kuasar/vmlinux.bin\"
image_path = \"/var/lib/kuasar/appliance.img\"
initrd_path = \"\"
kernel_params = \"\"
hugepages = false
entropy_source = \"/dev/urandom\"
[hypervisor.task]
debug = false
enable_tracing = false
[hypervisor.virtiofsd]
path = \"/usr/local/bin/virtiofsd\"
log_level = \"info\"
cache = \"never\"
thread_pool_size = 4
";
        let config: Config<CloudHypervisorVMConfig> = toml::from_str(toml_str).unwrap();
        assert_eq!(config.hypervisor.appliance_fs_type, ApplianceFsType::Ext4);
    }

    #[test]
    fn test_appliance_fs_type_toml_erofs() {
        let toml_str = "
[sandbox]
enable_tracing = false
[hypervisor]
path = \"/usr/local/bin/cloud-hypervisor\"
vcpus = 1
memory_in_mb = 1024
kernel_path = \"/var/lib/kuasar/vmlinux.bin\"
image_path = \"/var/lib/kuasar/appliance.erofs\"
initrd_path = \"\"
kernel_params = \"\"
hugepages = false
entropy_source = \"/dev/urandom\"
appliance_fs_type = \"erofs\"
[hypervisor.task]
debug = false
enable_tracing = false
[hypervisor.virtiofsd]
path = \"/usr/local/bin/virtiofsd\"
log_level = \"info\"
cache = \"never\"
thread_pool_size = 4
";
        let config: Config<CloudHypervisorVMConfig> = toml::from_str(toml_str).unwrap();
        assert_eq!(config.hypervisor.appliance_fs_type, ApplianceFsType::Erofs);
        let chc = CloudHypervisorConfig::from(&config.hypervisor, &SandboxProfile::Appliance);
        assert!(chc.cmdline.contains("rootfstype=erofs"));
    }

    #[test]
    fn test_standard_mode_unaffected_by_fs_type_field() {
        // appliance_fs_type must have no effect when profile is Standard.
        let vm_cfg = appliance_vm_config(ApplianceFsType::Erofs);
        let chc = CloudHypervisorConfig::from(&vm_cfg, &SandboxProfile::Standard);
        assert!(chc.cmdline.contains("rootfstype=ext4"));
        assert!(chc.cmdline.contains("task.sharefs_type=virtiofs"));
    }
}
