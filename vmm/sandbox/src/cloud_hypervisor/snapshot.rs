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

impl TemplateMeta {
    /// Serialize to `{dir}/metadata.json` atomically.
    pub async fn save(&self, dir: &Path) -> Result<()> {
        let content = serde_json::to_string_pretty(self)
            .map_err(|e| anyhow!("serialize TemplateMeta: {}", e))?;
        write_file_atomic(&dir.join("metadata.json"), &content).await
    }

    /// Deserialize from `{dir}/metadata.json`.
    pub async fn load(dir: &Path) -> Result<Self> {
        let content = tokio::fs::read_to_string(dir.join("metadata.json"))
            .await
            .map_err(|e| anyhow!("read metadata.json from {}: {}", dir.display(), e))?;
        serde_json::from_str(&content).map_err(|e| anyhow!("parse metadata.json: {}", e).into())
    }
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

    // vsock socket path — required; fail early if structure is unexpected.
    // CH config.json: vsock is a top-level field with a "socket" key (not under "payload").
    match cfg.pointer_mut("/vsock/socket") {
        Some(v) => *v = serde_json::Value::String(overrides.task_vsock.clone()),
        None => {
            return Err(
                anyhow!("config.json missing /vsock/socket — unexpected CH config format").into(),
            )
        }
    }

    // console log file — optional (may not be present in all CH configs)
    if let Some(v) = cfg.pointer_mut("/console/file") {
        *v = serde_json::Value::String(overrides.console_path.clone());
    }

    // Strip hot-plugged container blk devices from the restore config.
    // These paths (e.g. {sandbox_dir}/{storage_id}.img) are per-sandbox and do not
    // exist in the new sandbox's directory. Containers re-attach their blk devices
    // via hot-plug after the VM is restored, so clearing this array is safe.
    if let Some(disks) = cfg.get_mut("disks") {
        *disks = serde_json::Value::Array(vec![]);
    }

    let serialized = serde_json::to_string_pretty(&cfg)
        .map_err(|e| anyhow!("serialize patched config.json: {}", e))?;
    write_file_atomic(dst, &serialized).await
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use temp_dir::TempDir;

    use super::*;

    #[tokio::test]
    async fn test_patch_snapshot_config() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("config.json");
        let dst = dir.path().join("config_patched.json");

        // Mirrors actual CH config.json: vsock and console are top-level fields.
        // "payload" in CH config is only for kernel/cmdline/initramfs.
        // "disks" contains hot-plugged container blk images (per-sandbox, must be stripped).
        let original = serde_json::json!({
            "payload": {
                "kernel": "/var/lib/kuasar/vmlinux.bin",
                "cmdline": "console=hvc0 root=/dev/pmem0p1 ro"
            },
            "vsock": {
                "socket": "/old/sandbox-abc/task.vsock",
                "cid": 3,
                "iommu": false
            },
            "console": {
                "file": "/tmp/sandbox-abc-task.log",
                "mode": "File"
            },
            "pmem": [
                {"file": "/var/lib/kuasar/rootfs.img", "discard_writes": true}
            ],
            "disks": [
                {"path": "/old/sandbox-abc/container-1.img", "readonly": false}
            ]
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

        assert_eq!(patched["vsock"]["socket"], "/new/sandbox-xyz/task.vsock");
        assert_eq!(patched["console"]["file"], "/tmp/sandbox-xyz-task.log");
        // pmem path must remain unchanged
        assert_eq!(patched["pmem"][0]["file"], "/var/lib/kuasar/rootfs.img");
        // container blk devices must be stripped — they will be re-hot-plugged after restore
        assert_eq!(patched["disks"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn test_template_meta_save_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        let meta = TemplateMeta {
            id: "tmpl-001".to_string(),
            snapshot_dir: PathBuf::from("/var/lib/kuasar/templates/tmpl-001/snapshot"),
            original_task_vsock: "/var/lib/kuasar/templates/tmpl-001/task.vsock".to_string(),
            original_console_path: "/tmp/tmpl-001-task.log".to_string(),
        };

        meta.save(dir.path()).await.unwrap();

        let loaded = TemplateMeta::load(dir.path()).await.unwrap();
        assert_eq!(loaded.id, meta.id);
        assert_eq!(loaded.snapshot_dir, meta.snapshot_dir);
        assert_eq!(loaded.original_task_vsock, meta.original_task_vsock);
        assert_eq!(loaded.original_console_path, meta.original_console_path);
    }

    #[tokio::test]
    async fn test_template_meta_load_missing_returns_error() {
        let dir = TempDir::new().unwrap();
        let result = TemplateMeta::load(dir.path()).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("metadata.json"));
    }

    #[tokio::test]
    async fn test_patch_snapshot_config_missing_vsock_returns_error() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("config.json");
        let dst = dir.path().join("config_patched.json");

        // config.json without vsock field
        let cfg = serde_json::json!({ "pmem": [], "console": { "mode": "Null" } });
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
        assert!(msg.contains("missing /vsock/socket"), "got: {}", msg);
    }
}
