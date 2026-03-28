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

pub mod config;
pub mod engine;
pub mod error;
pub mod instance;
pub mod state;

#[cfg(test)]
mod tests;

pub use config::EngineConfig;
pub use engine::SandboxEngine;
pub use error::{Error, Result};
pub use instance::{
    ContainerState, NetworkState, ProcessState, SandboxInstance, SandboxSummary, StorageMount,
    StorageMountKind,
};
pub use state::{SandboxState, StateEvent};

use containerd_sandbox::data::SandboxData;
use vmm_vm_trait::DiskConfig;

/// Request passed to `SandboxEngine::create_sandbox`.
pub struct CreateSandboxRequest {
    pub sandbox_data: SandboxData,
    pub netns: String,
    pub cgroup_parent: String, // "" → use DEFAULT_CGROUP_PARENT_PATH
    pub rootfs_disk: Option<DiskConfig>,
}

/// Returned by `SandboxEngine::start_sandbox`.
#[derive(Debug)]
pub struct StartResult {
    pub ready_ms: u64,
    pub vmm_start_ms: u64,
}
