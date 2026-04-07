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

//! Migration from legacy `KuasarSandbox` state files to `SandboxInstance`.
//!
//! When `vmm-engine` shares the same working directory as the old `vmm-sandboxer`,
//! it encounters `sandbox.json` files in the old `KuasarSandbox` format.  This
//! module converts them transparently during recovery.
//!
//! # Format differences
//!
//! | Old field       | New field      | Notes                                    |
//! |-----------------|----------------|------------------------------------------|
//! | `"vm"`          | `"vmm"`        | Backend-specific; use `Vmm::from_legacy_vm` |
//! | `"status"`      | `"state"`      | Enum variant mapping                     |
//! | `"containers"`  | `"containers"` | Same field names; direct re-use          |
//! | `"storages"`    | `"storages"`   | `Storage` → `StorageMount`               |
//! | `"network"`     | `"network"`    | `Network` → `NetworkState`               |
//! | *(in vm JSON)*  | `"netns"`      | Extracted from `vm.netns`                |
//! | *(absent)*      | `vsock_port_next` | Defaulted to 1025                     |

use std::collections::HashMap;
use std::sync::Arc;

use containerd_sandbox::data::SandboxData;
use containerd_sandbox::signal::ExitSignal;
use serde::Deserialize;
use serde_json::Value;
use vmm_guest_runtime::{IpNet, NetworkInterface, Route};
use vmm_vm_trait::Vmm;

use crate::instance::{
    ContainerState, NetworkState, SandboxCgroup, SandboxInstance, StorageMount, StorageMountKind,
};
use crate::state::SandboxState;

/// Try to migrate a raw JSON value that was written by the old `KuasarSandbox`
/// (i.e. it has a `"vm"` key but no `"vmm"` key) into a `SandboxInstance<V>`.
pub fn migrate<V: Vmm + Default>(raw: &Value) -> anyhow::Result<SandboxInstance<V>> {
    // ── Parse the legacy top-level shape ──────────────────────────────────────
    #[derive(Deserialize)]
    struct LegacyShape {
        id: String,
        vm: Value,
        status: Value,
        base_dir: String,
        data: SandboxData,
        #[serde(default)]
        containers: HashMap<String, Value>,
        #[serde(default)]
        storages: Vec<Value>,
        #[serde(default)]
        id_generator: u32,
        #[serde(default)]
        network: Option<Value>,
    }

    let legacy: LegacyShape = serde_json::from_value(raw.clone())
        .map_err(|e| anyhow::anyhow!("parse legacy shape: {}", e))?;

    // Extract netns: all three old VM structs serialize their `netns` field.
    let netns = legacy
        .vm
        .get("netns")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // ── Reconstruct VMM from the old "vm" value ────────────────────────────────
    let vmm = V::from_legacy_vm(legacy.vm, &legacy.id, &legacy.base_dir)
        .map_err(|e| anyhow::anyhow!("from_legacy_vm: {}", e))?;

    // ── Status → State ────────────────────────────────────────────────────────
    let state = status_to_state(&legacy.status);

    // ── Containers (KuasarContainer ≈ ContainerState — same field names/types) ─
    let containers = legacy
        .containers
        .into_iter()
        .map(|(k, v)| {
            let c: ContainerState =
                serde_json::from_value(v).map_err(|e| anyhow::anyhow!("container {}: {}", k, e))?;
            Ok((k, c))
        })
        .collect::<anyhow::Result<HashMap<_, _>>>()?;

    // ── Storages ──────────────────────────────────────────────────────────────
    let storages = legacy
        .storages
        .into_iter()
        .enumerate()
        .map(|(i, v)| storage_to_mount(v).map_err(|e| anyhow::anyhow!("storage[{}]: {}", i, e)))
        .collect::<anyhow::Result<Vec<_>>>()?;

    // ── Network (best-effort; tap_names needed for cleanup) ───────────────────
    let network = legacy.network.and_then(|n| network_to_state(n).ok());

    Ok(SandboxInstance {
        id: legacy.id,
        vmm,
        state,
        base_dir: legacy.base_dir,
        data: legacy.data,
        netns,
        network,
        storages,
        containers,
        id_generator: legacy.id_generator,
        vsock_port_next: 1025,
        cgroup: SandboxCgroup::default(),
        exit_signal: Arc::new(ExitSignal::default()),
    })
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Map old `SandboxStatus` JSON → new `SandboxState`.
///
/// Old serialization (serde default enum):
/// - `"Created"`            → Creating
/// - `{"Running": <pid>}`   → Running
/// - `{"Stopped": [c, ts]}` → Stopped
fn status_to_state(status: &Value) -> SandboxState {
    match status {
        Value::String(s) if s == "Created" => SandboxState::Creating,
        Value::Object(m) if m.contains_key("Running") => SandboxState::Running,
        Value::Object(m) if m.contains_key("Stopped") => SandboxState::Stopped,
        _ => SandboxState::Stopped,
    }
}

/// Convert a legacy `Storage` JSON value into a `StorageMount`.
fn storage_to_mount(raw: Value) -> anyhow::Result<StorageMount> {
    #[derive(Deserialize)]
    struct LegacyStorage {
        id: String,
        host_source: String,
        device_id: Option<String>,
        #[serde(default)]
        ref_container: HashMap<String, u32>,
        #[serde(default)]
        source: String,
        driver: String,
        #[serde(default)]
        mount_point: String,
    }

    let s: LegacyStorage =
        serde_json::from_value(raw).map_err(|e| anyhow::anyhow!("parse storage: {}", e))?;

    let kind = match s.driver.as_str() {
        "virtio-fs" => StorageMountKind::VirtioFs,
        "9p" => StorageMountKind::Virtio9P,
        _ => StorageMountKind::Block,
    };

    Ok(StorageMount {
        id: s.id,
        ref_containers: s.ref_container.into_keys().collect(),
        host_path: s.host_source,
        mount_dest: if s.mount_point.is_empty() {
            None
        } else {
            Some(s.mount_point)
        },
        guest_path: s.source,
        kind,
        device_id: s.device_id,
    })
}

/// Convert a legacy `Network` JSON value into a `NetworkState`.
///
/// The conversion is best-effort: `tap_names` (needed for cleanup on stop)
/// are inferred from the `device` field of each old `NetworkInterface`.
/// IP addresses are parsed from `CniIPAddress { address, mask }` pairs.
/// Fields that don't exist in the old format (`physical_nics`) are empty.
fn network_to_state(raw: Value) -> anyhow::Result<NetworkState> {
    #[derive(Deserialize)]
    struct LegacyNetwork {
        #[serde(default)]
        intfs: Vec<Value>,
        #[serde(default)]
        routes: Vec<Value>,
    }

    let n: LegacyNetwork =
        serde_json::from_value(raw).map_err(|e| anyhow::anyhow!("parse network: {}", e))?;

    let mut tap_names = Vec::new();
    let mut interfaces = Vec::new();

    for intf_raw in n.intfs {
        #[derive(Deserialize)]
        struct LegacyIntf {
            #[serde(default)]
            device: String,
            #[serde(default)]
            name: String,
            #[serde(rename = "IPAddresses", default)]
            cni_ip_addresses: Vec<Value>,
            // Serialized as a plain MAC string via serde(from/into "String")
            #[serde(rename = "hwAddr", default)]
            mac_address: String,
            #[serde(default)]
            mtu: u32,
        }

        let intf: LegacyIntf = match serde_json::from_value(intf_raw) {
            Ok(i) => i,
            Err(_) => continue,
        };

        // The "device" field is the tap device name on the host.
        if !intf.device.is_empty() {
            tap_names.push(intf.device.clone());
        }

        // Convert CniIPAddress list → Vec<IpNet>.
        let ip_addresses: Vec<IpNet> = intf
            .cni_ip_addresses
            .iter()
            .filter_map(|v| {
                let addr = v.get("address").and_then(|a| a.as_str()).unwrap_or("");
                let mask = v.get("mask").and_then(|m| m.as_str()).unwrap_or("");
                if addr.is_empty() {
                    return None;
                }
                let cidr = if mask.is_empty() {
                    addr.to_string()
                } else {
                    format!("{}/{}", addr, mask)
                };
                cidr.parse::<IpNet>().ok()
            })
            .collect();

        interfaces.push(NetworkInterface {
            name: if intf.name.is_empty() {
                intf.device
            } else {
                intf.name
            },
            mac: intf.mac_address,
            ip_addresses,
            mtu: intf.mtu,
        });
    }

    let routes: Vec<Route> = n
        .routes
        .iter()
        .filter_map(|r| {
            #[derive(Deserialize)]
            struct LegacyRoute {
                #[serde(default)]
                device: String,
                #[serde(default)]
                dest: String,
                #[serde(default)]
                gateway: String,
            }
            let r: LegacyRoute = serde_json::from_value(r.clone()).ok()?;
            Some(Route {
                dest: r.dest,
                gateway: r.gateway,
                device: r.device,
            })
        })
        .collect();

    Ok(NetworkState {
        interfaces,
        routes,
        tap_names,
        physical_nics: vec![],
    })
}
