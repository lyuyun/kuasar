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
    os::unix::net::UnixStream,
    path::PathBuf,
    thread::sleep,
    time::{Duration, SystemTime},
};

use anyhow::anyhow;
use api_client::{simple_api_command, simple_api_full_command_with_fds_and_response};
use containerd_sandbox::error::Result;
use log::{debug, error, trace};
use serde::{Deserialize, Serialize};
use tokio::task::spawn_blocking;

use crate::{
    cloud_hypervisor::devices::{block::DiskConfig, AddDeviceResponse, RemoveDeviceRequest},
    device::DeviceInfo,
};

#[derive(Serialize, Debug)]
struct VmSnapshotConfig {
    destination_url: String,
}

#[derive(Serialize, Debug)]
struct VmRestoreConfig {
    source_url: PathBuf,
    #[serde(default)]
    prefault: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct NetConfig {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tap: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub fds: Vec<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_queues: Option<u32>,
}

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
pub struct AddNetResponse {
    pub id: String,
    pub bdf: String,
}

pub(crate) const CLOUD_HYPERVISOR_START_TIMEOUT_IN_SEC: u64 = 10;

pub struct ChClient {
    socket: UnixStream,
}

impl ChClient {
    pub async fn new(socket_path: String) -> Result<Self> {
        let s = socket_path.to_string();
        let start_time = SystemTime::now();
        let socket = spawn_blocking(move || -> Result<UnixStream> {
            loop {
                match UnixStream::connect(&socket_path) {
                    Ok(socket) => {
                        return Ok(socket);
                    }
                    Err(e) => {
                        trace!("failed to create client: {:?}", e);
                        if start_time.elapsed().unwrap().as_secs()
                            > CLOUD_HYPERVISOR_START_TIMEOUT_IN_SEC
                        {
                            error!("failed to create client: {:?}", e);
                            return Err(anyhow!("timeout connect client, {}", e).into());
                        }
                        sleep(Duration::from_millis(10));
                    }
                }
            }
        })
        .await
        .map_err(|e| anyhow!("failed to join thread {}", e))??;
        debug!("connected to api server {}", s);
        Ok(Self { socket })
    }

    pub fn hot_attach(&mut self, device_info: DeviceInfo) -> Result<String> {
        match device_info {
            DeviceInfo::Block(blk) => {
                let disk_config = DiskConfig {
                    path: blk.path,
                    readonly: blk.read_only,
                    direct: true,
                    vhost_user: false,
                    vhost_socket: None,
                    id: blk.id,
                };
                let request_body = serde_json::to_string(&disk_config)
                    .map_err(|e| anyhow!("failed to marshal {:?} to json, {}", disk_config, e))?;
                let response_opt = simple_api_full_command_with_fds_and_response(
                    &mut self.socket,
                    "PUT",
                    "vm.add-disk",
                    Some(&request_body),
                    vec![],
                )
                .map_err(|e| anyhow!("failed to hotplug disk {}, {}", request_body, e))?;
                if let Some(response_body) = response_opt {
                    let response = serde_json::from_str::<AddDeviceResponse>(&response_body)
                        .map_err(|e| {
                            anyhow!("failed to unmarshal response {}, {}", response_body, e)
                        })?;
                    Ok(response.bdf)
                } else {
                    Err(anyhow!("no response body from server").into())
                }
            }
            DeviceInfo::Tap(_) => {
                todo!()
            }
            DeviceInfo::Physical(_) => {
                todo!()
            }
            DeviceInfo::VhostUser(_) => {
                todo!()
            }
            DeviceInfo::Char(_) => {
                unimplemented!()
            }
        }
    }

    pub fn hot_detach(&mut self, device_id: &str) -> Result<()> {
        let request = RemoveDeviceRequest {
            id: device_id.to_string(),
        };
        let request_body = serde_json::to_string(&request)
            .map_err(|e| anyhow!("failed to marshal {:?} to json, {}", request, e))?;
        simple_api_command(
            &mut self.socket,
            "PUT",
            "remove-device",
            Some(&request_body),
        )
        .map_err(|e| anyhow!("failed to remove device {}, {}", request_body, e))?;
        Ok(())
    }

    /// Pause the VM.  Must be called before [`snapshot`].
    pub fn pause(&mut self) -> Result<()> {
        simple_api_command(&mut self.socket, "PUT", "pause", None)
            .map_err(|e| anyhow!("failed to pause VM: {}", e))?;
        Ok(())
    }

    /// Resume a previously paused VM.
    pub fn resume(&mut self) -> Result<()> {
        simple_api_command(&mut self.socket, "PUT", "resume", None)
            .map_err(|e| anyhow!("failed to resume VM: {}", e))?;
        Ok(())
    }

    /// Write a snapshot to `destination_url` (e.g. `file:///path/to/dir`).
    /// The VM must be paused before calling this.
    pub fn snapshot(&mut self, destination_url: &str) -> Result<()> {
        let cfg = VmSnapshotConfig {
            destination_url: destination_url.to_string(),
        };
        let body = serde_json::to_string(&cfg)
            .map_err(|e| anyhow!("failed to serialise snapshot config: {}", e))?;
        simple_api_command(&mut self.socket, "PUT", "snapshot", Some(&body))
            .map_err(|e| anyhow!("failed to take snapshot to {}: {}", destination_url, e))?;
        Ok(())
    }

    /// Restore a VM from a snapshot at `source_url`.
    /// `prefault` eagerly faults in all pages on restore (trades latency for
    /// predictable steady-state performance).
    pub fn restore(&mut self, source_url: &str, prefault: bool) -> Result<()> {
        let cfg = VmRestoreConfig {
            source_url: source_url.trim_start_matches("file://").into(),
            prefault,
        };
        let body = serde_json::to_string(&cfg)
            .map_err(|e| anyhow!("failed to serialise restore config: {}", e))?;
        simple_api_command(&mut self.socket, "PUT", "restore", Some(&body))
            .map_err(|e| anyhow!("failed to restore from {}: {}", source_url, e))?;
        Ok(())
    }

    /// Hot-plug a network device.  `fds` are the tap file descriptors that
    /// must be passed via SCM_RIGHTS alongside the request.  Returns the PCI
    /// BDF assigned to the new device if the server provides one.
    #[allow(dead_code)]
    pub fn add_net(&mut self, config: &NetConfig, fds: Vec<i32>) -> Result<Option<String>> {
        let body = serde_json::to_string(config)
            .map_err(|e| anyhow!("failed to serialise net config: {}", e))?;
        let resp_opt = simple_api_full_command_with_fds_and_response(
            &mut self.socket,
            "PUT",
            "vm.add-net",
            Some(&body),
            fds,
        )
        .map_err(|e| anyhow!("failed to add-net {}: {}", body, e))?;
        match resp_opt {
            Some(r) => {
                let parsed = serde_json::from_str::<AddNetResponse>(&r)
                    .map_err(|e| anyhow!("failed to parse add-net response: {}", e))?;
                Ok(Some(parsed.bdf))
            }
            None => Ok(None),
        }
    }
}
