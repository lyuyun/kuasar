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

use serde::{Deserialize, Serialize};

/// Memory restore mode sent to Cloud Hypervisor in the PUT /api/v1/vm.restore payload.
/// Config values are case-insensitive: `"copy"`, `"ondemand"`, `"filebackend"`, `"externaluffd"`.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub enum MemoryRestoreMode {
    /// Copy guest memory pages from snapshot into anonymous memory (default).
    #[default]
    Copy,
    /// Map pages on-demand from the snapshot file using userfaultfd.
    OnDemand,
    /// Map the snapshot file directly as a file-backed memory region.
    FileBackend,
    /// Delegate page faults to an external userfaultfd handler process.
    ExternalUffd,
}

impl<'de> Deserialize<'de> for MemoryRestoreMode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        match s.to_ascii_lowercase().as_str() {
            "copy" => Ok(Self::Copy),
            "ondemand" => Ok(Self::OnDemand),
            "filebackend" => Ok(Self::FileBackend),
            "externaluffd" => Ok(Self::ExternalUffd),
            _ => Err(serde::de::Error::unknown_variant(
                &s,
                &["copy", "ondemand", "filebackend", "externaluffd"],
            )),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SnapshotConfig {
    /// Enable Environment (bare VM) snapshot restore instead of cold-booting each sandbox.
    /// Safe to enable after validating that the hypervisor restore process works end-to-end.
    pub enable_environment_restore: bool,
    /// Enable WarmFork restore.  Requires end-to-end validation in your environment
    /// before enabling in production.  Defaults to false.
    pub enable_warmfork_restore: bool,
    /// Enable Continuation restore.  Restores a process with its full in-memory
    /// state and original network identity (virtual IP / Pod IP).  Network identity transfer
    /// is an external concern — the operator or CNI layer must route the original Pod IP to
    /// this node before the restore is triggered.  Kuasar trusts that transfer is complete
    /// when `start()` is called.  Existing TCP connections are not preserved; workloads must
    /// tolerate reconnect errors.  Defaults to false.
    pub enable_continuation_restore: bool,
    /// Maximum number of concurrent VM restores across all template kinds.
    /// Caps host memory pressure when many sandboxes start simultaneously.
    #[serde(default = "SnapshotConfig::default_max_concurrent_restores")]
    pub max_concurrent_restores: usize,
    pub fallback_to_fresh_boot: bool,
    pub default_memory_restore_mode: MemoryRestoreMode,
}

const DEFAULT_MAX_CONCURRENT_RESTORES: usize = 4;

impl Default for SnapshotConfig {
    fn default() -> Self {
        Self {
            enable_environment_restore: false,
            enable_warmfork_restore: false,
            enable_continuation_restore: false,
            max_concurrent_restores: DEFAULT_MAX_CONCURRENT_RESTORES,
            fallback_to_fresh_boot: true,
            default_memory_restore_mode: MemoryRestoreMode::Copy,
        }
    }
}

impl SnapshotConfig {
    fn default_max_concurrent_restores() -> usize {
        DEFAULT_MAX_CONCURRENT_RESTORES
    }
}

#[derive(Default, Debug, Deserialize)]
pub struct SandboxConfig {
    #[serde(default)]
    pub log_level: String,
    #[serde(default)]
    pub enable_tracing: bool,
    #[serde(default)]
    pub snapshot: SnapshotConfig,
}

impl SandboxConfig {
    pub fn log_level(&self) -> String {
        self.log_level.to_string()
    }

    pub fn enable_tracing(&self) -> bool {
        self.enable_tracing
    }
}

/// Template lease mode: how the pool manages a template entry after restore.
///
/// `Shared` — the template entry stays in the pool and is ref-counted. Multiple sandboxes can
/// restore from the same entry concurrently. The entry is GC-eligible only when its ref-count
/// reaches zero. This is the default and the main production path for WarmFork.
///
/// `Exclusive` — the template entry is consumed (removed from pool) on the first restore. The
/// snapshot files remain on disk until the sandbox is stopped and deleted. Use this for
/// one-shot or stateful-token workloads where sharing a ready-waiting process is not correct.
///
/// Note: "Shared" does not imply any underlying memory CoW or process fork capability.
/// It means the pool entry can be reused across multiple restores. Whether the guest VM state
/// can be shared at the memory level is a separate infrastructure concern not yet implemented.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum TemplateLeaseMode {
    #[default]
    Shared,
    Exclusive,
}

impl std::fmt::Display for TemplateLeaseMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TemplateLeaseMode::Shared => write!(f, "shared"),
            TemplateLeaseMode::Exclusive => write!(f, "exclusive"),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticDeviceSpec {
    #[serde(default)]
    pub(crate) _host_path: Vec<String>,
    #[serde(default)]
    pub(crate) _bdf: Vec<String>,
}
