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

use std::fmt;

#[derive(Debug)]
pub enum SnapshotError {
    NotFound(String),
    Io(std::io::Error),
    IntegrityFailed { expected: String, actual: String },
    Unsupported,
    RestoreFailed(String),
    PostRestoreFailed(String),
    Serialization(serde_json::Error),
    InvalidSnapshotId(String),
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SnapshotError::NotFound(id) => write!(f, "snapshot not found: {}", id),
            SnapshotError::Io(e) => write!(f, "snapshot store I/O error: {}", e),
            SnapshotError::IntegrityFailed { expected, actual } => {
                write!(
                    f,
                    "snapshot integrity check failed: expected {}, got {}",
                    expected, actual
                )
            }
            SnapshotError::Unsupported => {
                write!(f, "hypervisor does not support app snapshots")
            }
            SnapshotError::RestoreFailed(msg) => write!(f, "restore failed: {}", msg),
            SnapshotError::PostRestoreFailed(msg) => {
                write!(f, "post-restore initialization failed: {}", msg)
            }
            SnapshotError::Serialization(e) => write!(f, "serialization error: {}", e),
            SnapshotError::InvalidSnapshotId(id) => {
                write!(f, "invalid snapshot id: {}", id)
            }
        }
    }
}

impl std::error::Error for SnapshotError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SnapshotError::Io(e) => Some(e),
            SnapshotError::Serialization(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for SnapshotError {
    fn from(e: std::io::Error) -> Self {
        SnapshotError::Io(e)
    }
}

impl From<serde_json::Error> for SnapshotError {
    fn from(e: serde_json::Error) -> Self {
        SnapshotError::Serialization(e)
    }
}

impl From<SnapshotError> for containerd_sandbox::error::Error {
    fn from(e: SnapshotError) -> Self {
        containerd_sandbox::error::Error::Other(anyhow::anyhow!(e.to_string()))
    }
}
