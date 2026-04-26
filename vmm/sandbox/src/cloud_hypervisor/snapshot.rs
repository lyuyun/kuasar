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

use std::path::{Path, PathBuf};

use anyhow::anyhow;
use containerd_sandbox::error::Result;
use serde::{Deserialize, Serialize};

use crate::{utils::write_file_atomic, vm::SnapshotPathOverrides};

/// Minimal metadata written alongside a template snapshot, sufficient for Phase 2 pool restore.
#[derive(Debug, Serialize, Deserialize)]
pub struct TemplateMeta {
    pub id: String,
    pub snapshot_dir: PathBuf,
    pub original_task_vsock: String,
    pub original_console_path: String,
}

/// Patch a Cloud Hypervisor `config.json` by updating sandbox-specific socket and log paths.
///
/// CH's config.json records absolute paths for the vsock and console devices, which are unique
/// per-sandbox.  During restore these must point to the *new* sandbox's paths, not the template's.
/// pmem (rootfs) paths are deliberately left unchanged — they are shared read-only.
pub async fn patch_snapshot_config(
    src: &Path,
    dst: &Path,
    overrides: &SnapshotPathOverrides,
) -> Result<()> {
    let content = tokio::fs::read_to_string(src)
        .await
        .map_err(|e| anyhow!("read {}: {}", src.display(), e))?;
    let mut cfg: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| anyhow!("parse config.json: {}", e))?;

    // vsock socket path — required; fail early if structure is unexpected
    match cfg.pointer_mut("/payload/vsock/socket_path") {
        Some(v) => *v = serde_json::Value::String(overrides.task_vsock.clone()),
        None => {
            return Err(
                anyhow!("config.json missing /payload/vsock/socket_path — CH version mismatch?")
                    .into(),
            )
        }
    }

    // console log file — optional (may not be present in all CH configs)
    if let Some(v) = cfg.pointer_mut("/payload/console/file") {
        *v = serde_json::Value::String(overrides.console_path.clone());
    }

    let serialized = serde_json::to_string_pretty(&cfg)
        .map_err(|e| anyhow!("serialize patched config.json: {}", e))?;
    write_file_atomic(dst, &serialized).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use temp_dir::TempDir;

    #[tokio::test]
    async fn test_patch_snapshot_config() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("config.json");
        let dst = dir.path().join("config_patched.json");

        let original = serde_json::json!({
            "payload": {
                "vsock": {
                    "socket_path": "/old/sandbox-abc/task.vsock",
                    "cid": 3
                },
                "console": {
                    "file": "/tmp/sandbox-abc-task.log",
                    "mode": "File"
                },
                "pmem": [
                    {"file": "/var/lib/kuasar/rootfs.img", "discard_writes": true}
                ]
            }
        });
        tokio::fs::write(&src, serde_json::to_string_pretty(&original).unwrap())
            .await
            .unwrap();

        let overrides = SnapshotPathOverrides {
            task_vsock: "/new/sandbox-xyz/task.vsock".to_string(),
            console_path: "/tmp/sandbox-xyz-task.log".to_string(),
        };
        patch_snapshot_config(&src, &dst, &overrides).await.unwrap();

        let patched: serde_json::Value =
            serde_json::from_str(&tokio::fs::read_to_string(&dst).await.unwrap()).unwrap();

        assert_eq!(
            patched["payload"]["vsock"]["socket_path"],
            "/new/sandbox-xyz/task.vsock"
        );
        assert_eq!(
            patched["payload"]["console"]["file"],
            "/tmp/sandbox-xyz-task.log"
        );
        // pmem path must remain unchanged
        assert_eq!(
            patched["payload"]["pmem"][0]["file"],
            "/var/lib/kuasar/rootfs.img"
        );
    }

    #[tokio::test]
    async fn test_patch_snapshot_config_missing_vsock_returns_error() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("config.json");
        let dst = dir.path().join("config_patched.json");

        // config.json without vsock field
        let cfg = serde_json::json!({ "payload": { "pmem": [] } });
        tokio::fs::write(&src, serde_json::to_string_pretty(&cfg).unwrap())
            .await
            .unwrap();

        let overrides = SnapshotPathOverrides {
            task_vsock: "/new/task.vsock".to_string(),
            console_path: "/tmp/console.log".to_string(),
        };
        let result = patch_snapshot_config(&src, &dst, &overrides).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("missing /payload/vsock/socket_path"), "got: {}", msg);
    }
}
