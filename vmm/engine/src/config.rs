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

use serde::Deserialize;

/// Runtime-level configuration for `SandboxEngine`.
#[derive(Deserialize, Clone)]
pub struct EngineConfig {
    /// Directory where per-sandbox state directories are created.
    pub work_dir: String,
    /// How long (milliseconds) to wait for the guest ttrpc endpoint to become ready.
    pub ready_timeout_ms: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            work_dir: "/run/kuasar/engine".to_string(),
            ready_timeout_ms: 45_000,
        }
    }
}
