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

pub mod error;
pub mod manager;
pub mod metrics;
pub mod mgmt_server;
pub mod post_restore;
pub mod store;
pub mod warm_pool;

pub use manager::{get_snapshot_key, SnapshotManager, SNAPSHOT_KEY_ANNOTATION};
pub use mgmt_server::{start as start_mgmt_server, DEFAULT_SNAPSHOT_MGMT_SOCK};
pub use store::{SnapshotId, SnapshotKey, SnapshotMetadata, SnapshotStore};
pub use warm_pool::{start_replenish_loop, WarmPool};
