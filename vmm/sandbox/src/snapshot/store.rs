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

use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::fs;

use super::error::SnapshotError;

pub const DEFAULT_SNAPSHOT_ROOT: &str = "/var/lib/kuasar/snapshots";

pub const MEMORY_FILE: &str = "memory.bin";
pub const STATE_FILE: &str = "snapshot.json";
pub const METADATA_FILE: &str = "metadata.json";
pub const CHECKSUM_FILE: &str = "checksums.json";

/// Opaque identifier derived from the SHA-256 of snapshot content.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SnapshotId(pub String);

impl SnapshotId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SnapshotId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Human-assigned key used by callers (e.g. annotation value) to look up
/// a snapshot.  Multiple keys may map to the same [`SnapshotId`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SnapshotKey(pub String);

impl SnapshotKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SnapshotKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<&str> for SnapshotKey {
    fn from(s: &str) -> Self {
        SnapshotKey(s.to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMetadata {
    pub id: SnapshotId,
    pub key: SnapshotKey,
    pub hypervisor: String,
    pub created_at: SystemTime,
    /// File sizes in bytes for quick capacity accounting.
    pub memory_size_bytes: u64,
    pub state_size_bytes: u64,
}

/// Checksums for all snapshot files, used for integrity verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotChecksums {
    memory_sha256: String,
    state_sha256: String,
}

/// Content-addressable local store for application snapshots.
///
/// Layout under `root`:
/// ```text
/// <root>/
///   <snapshot-id>/
///     memory.bin          – VM memory image
///     snapshot.json       – CHv device state
///     metadata.json       – SnapshotMetadata
///     checksums.json      – SHA-256 of memory.bin and snapshot.json
///   keys/
///     <snapshot-key>      – file whose content is the snapshot-id
/// ```
#[derive(Debug, Clone)]
pub struct SnapshotStore {
    root: PathBuf,
}

impl SnapshotStore {
    pub fn new(root: impl AsRef<Path>) -> Self {
        SnapshotStore {
            root: root.as_ref().to_path_buf(),
        }
    }

    pub fn default_store() -> Self {
        Self::new(DEFAULT_SNAPSHOT_ROOT)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn snapshot_dir(&self, id: &SnapshotId) -> PathBuf {
        self.root.join(&id.0)
    }

    fn keys_dir(&self) -> PathBuf {
        self.root.join("keys")
    }

    fn key_file(&self, key: &SnapshotKey) -> PathBuf {
        self.keys_dir().join(&key.0)
    }

    pub fn memory_path(&self, id: &SnapshotId) -> PathBuf {
        self.snapshot_dir(id).join(MEMORY_FILE)
    }

    pub fn state_path(&self, id: &SnapshotId) -> PathBuf {
        self.snapshot_dir(id).join(STATE_FILE)
    }

    /// Returns the destination URL that Cloud Hypervisor expects for snapshot.
    /// Format: `file://<absolute-dir>`
    pub fn destination_url(&self, id: &SnapshotId) -> String {
        format!("file://{}", self.snapshot_dir(id).display())
    }

    /// Returns the source URL for Cloud Hypervisor restore.
    pub fn source_url(&self, id: &SnapshotId) -> String {
        format!("file://{}", self.snapshot_dir(id).display())
    }

    /// Ensure base directories exist.
    pub async fn init(&self) -> Result<(), SnapshotError> {
        fs::create_dir_all(&self.root).await?;
        fs::create_dir_all(self.keys_dir()).await?;
        Ok(())
    }

    /// Resolve a human-readable key to its snapshot ID.
    pub async fn resolve_key(&self, key: &SnapshotKey) -> Result<SnapshotId, SnapshotError> {
        let key_file = self.key_file(key);
        let id_str = fs::read_to_string(&key_file)
            .await
            .map_err(|_| SnapshotError::NotFound(key.to_string()))?;
        Ok(SnapshotId(id_str.trim().to_string()))
    }

    /// Retrieve metadata for a snapshot, verifying integrity first.
    pub async fn get(&self, id: &SnapshotId) -> Result<SnapshotMetadata, SnapshotError> {
        self.verify_integrity(id).await?;
        let meta_path = self.snapshot_dir(id).join(METADATA_FILE);
        let meta_bytes = fs::read(&meta_path)
            .await
            .map_err(|_| SnapshotError::NotFound(id.to_string()))?;
        let meta: SnapshotMetadata = serde_json::from_slice(&meta_bytes)?;
        Ok(meta)
    }

    /// Retrieve metadata by key, verifying integrity.
    pub async fn get_by_key(&self, key: &SnapshotKey) -> Result<SnapshotMetadata, SnapshotError> {
        let id = self.resolve_key(key).await?;
        self.get(&id).await
    }

    /// Verify SHA-256 checksums of snapshot files.
    pub async fn verify_integrity(&self, id: &SnapshotId) -> Result<(), SnapshotError> {
        let checksum_path = self.snapshot_dir(id).join(CHECKSUM_FILE);
        let checksum_bytes = fs::read(&checksum_path)
            .await
            .map_err(|_| SnapshotError::NotFound(id.to_string()))?;
        let expected: SnapshotChecksums = serde_json::from_slice(&checksum_bytes)?;

        let memory_actual = sha256_file(self.memory_path(id)).await?;
        if memory_actual != expected.memory_sha256 {
            return Err(SnapshotError::IntegrityFailed {
                expected: expected.memory_sha256,
                actual: memory_actual,
            });
        }

        let state_actual = sha256_file(self.state_path(id)).await?;
        if state_actual != expected.state_sha256 {
            return Err(SnapshotError::IntegrityFailed {
                expected: expected.state_sha256,
                actual: state_actual,
            });
        }

        Ok(())
    }

    /// Record the snapshot in the store after Cloud Hypervisor has written the
    /// files to `self.snapshot_dir(id)`. Computes checksums, writes metadata,
    /// and registers the key mapping.
    pub async fn commit(
        &self,
        id: &SnapshotId,
        key: &SnapshotKey,
        hypervisor: &str,
    ) -> Result<SnapshotMetadata, SnapshotError> {
        let snap_dir = self.snapshot_dir(id);

        let memory_sha256 = sha256_file(self.memory_path(id)).await?;
        let state_sha256 = sha256_file(self.state_path(id)).await?;

        let checksums = SnapshotChecksums {
            memory_sha256,
            state_sha256,
        };
        let checksum_bytes = serde_json::to_vec_pretty(&checksums)?;
        fs::write(snap_dir.join(CHECKSUM_FILE), checksum_bytes).await?;

        let memory_size_bytes = fs::metadata(self.memory_path(id)).await?.len();
        let state_size_bytes = fs::metadata(self.state_path(id)).await?.len();

        let meta = SnapshotMetadata {
            id: id.clone(),
            key: key.clone(),
            hypervisor: hypervisor.to_string(),
            created_at: SystemTime::now(),
            memory_size_bytes,
            state_size_bytes,
        };
        let meta_bytes = serde_json::to_vec_pretty(&meta)?;
        fs::write(snap_dir.join(METADATA_FILE), meta_bytes).await?;

        // Register key → id mapping.
        fs::create_dir_all(self.keys_dir()).await?;
        fs::write(self.key_file(key), id.0.as_bytes()).await?;

        Ok(meta)
    }

    /// Generate a snapshot ID from the content of the snapshot files.
    /// Must be called after Cloud Hypervisor has finished writing the files.
    pub async fn compute_id(&self, staging_dir: &Path) -> Result<SnapshotId, SnapshotError> {
        let memory_hash = sha256_file(staging_dir.join(MEMORY_FILE)).await?;
        let state_hash = sha256_file(staging_dir.join(STATE_FILE)).await?;
        let combined = format!("{}{}", memory_hash, state_hash);
        let mut hasher = Sha256::new();
        hasher.update(combined.as_bytes());
        let id = format!("{:x}", hasher.finalize());
        Ok(SnapshotId(id))
    }

    /// Move snapshot files from a temporary staging directory into the
    /// content-addressable store directory.
    pub async fn import_from(
        &self,
        staging_dir: &Path,
        key: &SnapshotKey,
        hypervisor: &str,
    ) -> Result<SnapshotMetadata, SnapshotError> {
        let id = self.compute_id(staging_dir).await?;
        let dest_dir = self.snapshot_dir(&id);

        if dest_dir.exists() {
            // Snapshot with identical content already stored; reuse it.
            fs::remove_dir_all(staging_dir).await.ok();
        } else {
            fs::rename(staging_dir, &dest_dir).await?;
        }

        // Register the key even if content was already present.
        self.commit(&id, key, hypervisor).await
    }

    /// List all snapshot metadata entries in the store.
    pub async fn list(&self) -> Result<Vec<SnapshotMetadata>, SnapshotError> {
        let mut result = Vec::new();
        let mut dir = match fs::read_dir(&self.root).await {
            Ok(d) => d,
            Err(_) => return Ok(result),
        };

        while let Ok(Some(entry)) = dir.next_entry().await {
            let entry_path = entry.path();
            if !entry_path.is_dir() || entry_path.file_name() == Some(std::ffi::OsStr::new("keys")) {
                continue;
            }
            let id = SnapshotId(
                entry_path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_string(),
            );
            if let Ok(meta) = self.get(&id).await {
                result.push(meta);
            }
        }
        Ok(result)
    }

    /// Delete a snapshot and all its files.
    pub async fn delete(&self, id: &SnapshotId) -> Result<(), SnapshotError> {
        let snap_dir = self.snapshot_dir(id);
        if snap_dir.exists() {
            fs::remove_dir_all(snap_dir).await?;
        }
        Ok(())
    }
}

async fn sha256_file(path: impl AsRef<Path>) -> Result<String, SnapshotError> {
    let data = fs::read(path).await?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use temp_dir::TempDir;

    async fn make_store() -> (TempDir, SnapshotStore) {
        let dir = TempDir::new().expect("tempdir");
        let store = SnapshotStore::new(dir.path());
        store.init().await.expect("init");
        (dir, store)
    }

    /// Write synthetic memory.bin and snapshot.json into `dir`.
    async fn write_fake_snapshot(dir: &PathBuf) {
        fs::write(dir.join(MEMORY_FILE), b"fake-memory-content").await.unwrap();
        fs::write(dir.join(STATE_FILE), b"fake-state-content").await.unwrap();
    }

    #[tokio::test]
    async fn test_import_and_get_by_key() {
        let (tmp, store) = make_store().await;
        let staging = tmp.path().join("staging");
        fs::create_dir_all(&staging).await.unwrap();
        write_fake_snapshot(&staging).await;

        let key = SnapshotKey("test-key".into());
        let meta = store
            .import_from(&staging, &key, "cloud-hypervisor")
            .await
            .expect("import_from");

        assert_eq!(meta.key, key);
        assert_eq!(meta.hypervisor, "cloud-hypervisor");
        assert!(meta.memory_size_bytes > 0);
        assert!(meta.state_size_bytes > 0);

        // Round-trip via get_by_key.
        let loaded = store.get_by_key(&key).await.expect("get_by_key");
        assert_eq!(loaded.id, meta.id);
    }

    #[tokio::test]
    async fn test_idempotent_import() {
        // Importing the same content twice should produce the same ID and not error.
        let (tmp, store) = make_store().await;

        let staging1 = tmp.path().join("staging1");
        fs::create_dir_all(&staging1).await.unwrap();
        write_fake_snapshot(&staging1).await;
        let meta1 = store
            .import_from(&staging1, &SnapshotKey("k1".into()), "chv")
            .await
            .unwrap();

        let staging2 = tmp.path().join("staging2");
        fs::create_dir_all(&staging2).await.unwrap();
        write_fake_snapshot(&staging2).await;
        let meta2 = store
            .import_from(&staging2, &SnapshotKey("k1".into()), "chv")
            .await
            .unwrap();

        assert_eq!(meta1.id, meta2.id);
    }

    #[tokio::test]
    async fn test_integrity_check_detects_tampering() {
        let (tmp, store) = make_store().await;
        let staging = tmp.path().join("staging");
        fs::create_dir_all(&staging).await.unwrap();
        write_fake_snapshot(&staging).await;

        let key = SnapshotKey("tamper-key".into());
        let meta = store
            .import_from(&staging, &key, "chv")
            .await
            .unwrap();

        // Tamper with memory.bin after import.
        let memory_path = store.memory_path(&meta.id);
        fs::write(&memory_path, b"tampered!").await.unwrap();

        let err = store.get(&meta.id).await.unwrap_err();
        assert!(
            matches!(err, SnapshotError::IntegrityFailed { .. }),
            "expected IntegrityFailed, got: {:?}",
            err
        );
    }

    #[tokio::test]
    async fn test_not_found_on_missing_key() {
        let (_tmp, store) = make_store().await;
        let err = store
            .get_by_key(&SnapshotKey("no-such-key".into()))
            .await
            .unwrap_err();
        assert!(matches!(err, SnapshotError::NotFound(_)));
    }

    #[tokio::test]
    async fn test_list_and_delete() {
        let (tmp, store) = make_store().await;

        // Import two distinct snapshots.
        for (i, content) in [b"content-a".as_ref(), b"content-b".as_ref()]
            .iter()
            .enumerate()
        {
            let staging = tmp.path().join(format!("staging-{}", i));
            fs::create_dir_all(&staging).await.unwrap();
            fs::write(staging.join(MEMORY_FILE), content).await.unwrap();
            fs::write(staging.join(STATE_FILE), b"state").await.unwrap();
            store
                .import_from(&staging, &SnapshotKey(format!("key-{}", i)), "chv")
                .await
                .unwrap();
        }

        let list = store.list().await.unwrap();
        assert_eq!(list.len(), 2);

        // Delete one.
        store.delete(&list[0].id).await.unwrap();
        let list2 = store.list().await.unwrap();
        assert_eq!(list2.len(), 1);
    }

    #[tokio::test]
    async fn test_source_and_destination_urls() {
        let store = SnapshotStore::new("/var/lib/kuasar/snapshots");
        let id = SnapshotId("abc123".into());
        let url = store.source_url(&id);
        assert_eq!(url, "file:///var/lib/kuasar/snapshots/abc123");
        assert_eq!(store.destination_url(&id), url);
    }
}
