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

use std::{collections::HashMap, process::Stdio};

use containerd_sandbox::spec::Mount;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

pub const ANNOTATION_KEY_STORAGE: &str = "io.kuasar.storages";

pub const DRIVER9PTYPE: &str = "9p";
pub const DRIVERVIRTIOFSTYPE: &str = "virtio-fs";
pub const DRIVERBLKTYPE: &str = "blk";
pub const DRIVERMMIOBLKTYPE: &str = "mmioblk";
pub const DRIVERSCSITYPE: &str = "scsi";
pub const DRIVERNVDIMMTYPE: &str = "nvdimm";
pub const DRIVEREPHEMERALTYPE: &str = "ephemeral";
pub const DRIVERLOCALTYPE: &str = "local";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Storage {
    pub host_source: String,
    pub r#type: String,
    pub id: String,
    pub device_id: Option<String>,
    pub ref_container: HashMap<String, u32>,
    pub need_guest_handle: bool,
    pub source: String,
    pub driver: String,
    pub driver_options: Vec<String>,
    pub fstype: String,
    pub options: Vec<String>,
    pub mount_point: String,
}

/// Probe the filesystem type of a block device using `blkid`.
pub async fn get_fstype(path: &str) -> anyhow::Result<String> {
    let output = Command::new("blkid")
        .args(["-p", "-s", "TYPE", "-s", "PTTYPE", "-o", "export", path])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("failed to execute blkid: {}", e))?;
    if !output.status.success() {
        let error_msg = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!(
            "blkid failed ({}): {}",
            output.status,
            error_msg
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for l in stdout.lines() {
        let kv: Vec<&str> = l.split('=').collect();
        if kv.len() == 2 && kv[0] == "TYPE" {
            return Ok(kv[1].to_string());
        }
    }
    Err(anyhow::anyhow!("failed to get fstype of {}", path))
}

impl Storage {
    pub fn is_for_mount(&self, m: &Mount) -> bool {
        self.host_source == m.source && self.r#type == m.r#type
    }

    pub fn ref_count(&self) -> u32 {
        self.ref_container.iter().fold(0, |acc, (_k, v)| acc + v)
    }

    pub fn refer(&mut self, container_id: &str) {
        match self.ref_container.get_mut(container_id) {
            None => {
                self.ref_container.insert(container_id.to_string(), 1);
            }
            Some(r) => *r += 1,
        }
    }

    pub fn defer(&mut self, container_id: &str) {
        self.ref_container.remove(container_id);
    }
}
