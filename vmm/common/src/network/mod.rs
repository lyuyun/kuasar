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

//! Network discovery and device setup.
//!
//! Enters the pod network namespace, enumerates all relevant interfaces, and
//! prepares each one for VMM attachment:
//!
//! - **Veth** → creates a persistent tap device (`tap_kua_<index>`), sets up
//!   TC ingress + redirect (veth ↔ tap), collects IP addresses.
//! - **Physical NIC** (unknown link type with assigned IPs) → detects PCI BDF
//!   via ethtool, binds the device to the VFIO driver for passthrough.
//!
//! VhostUser interfaces are NOT auto-detected here because their socket path
//! comes from the CNI result, not from netlink.  Callers may post-process
//! returned `DiscoveredInterface` entries and populate `vhost_user_socket`
//! before passing them to `Vmm::add_network`.

use std::{collections::HashMap, os::unix::prelude::AsRawFd};

use anyhow::anyhow;
use futures_util::TryStreamExt;
use ipnetwork::IpNetwork;
use netlink_packet_route::{
    link::{InfoKind, LinkAttribute, LinkFlag, LinkInfo, LinkMessage},
    route::RouteAttribute,
};
use nix::sched::{setns, CloneFlags};
use rtnetlink::{new_connection, Handle, IpVersion};
use serde::{Deserialize, Serialize};
use tokio::task::spawn_blocking;

pub mod address;
pub mod link;
pub mod route;

pub use address::IpNet;
pub use link::{
    bind_device_to_driver, create_tap_device, get_bdf_for_eth, get_pci_driver, LinkType,
    DEVICE_DRIVER_VFIO,
};
pub use route::DiscoveredRoute;

use crate::network::address::route_address_to_string;

// ── Internal: netlink handle factory ─────────────────────────────────────────

/// Open a rtnetlink connection that operates inside `netns`.
///
/// If `netns` is empty, opens a connection in the current network namespace.
/// Otherwise spawns a blocking thread that enters the target netns, calls
/// `new_connection()`, then restores the original namespace.  The connection's
/// background task is spawned on the current tokio runtime.
async fn create_netlink_handle(netns: &str) -> anyhow::Result<Handle> {
    if netns.is_empty() {
        let (conn, handle, _) = new_connection().map_err(|e| anyhow!("netlink: {}", e))?;
        tokio::spawn(conn);
        return Ok(handle);
    }

    let netns = netns.to_string();
    let (conn, handle, _) = spawn_blocking(move || {
        let host_fd = std::fs::File::open("/proc/self/ns/net")
            .map_err(|e| anyhow!("open host netns: {}", e))?;
        let target_fd =
            std::fs::File::open(&netns).map_err(|e| anyhow!("open netns {}: {}", netns, e))?;
        setns(target_fd.as_raw_fd(), CloneFlags::CLONE_NEWNET)
            .map_err(|e| anyhow!("setns {}: {}", netns, e))?;
        let result = new_connection().map_err(|e| anyhow!("netlink in netns: {}", e));
        let _ = setns(host_fd.as_raw_fd(), CloneFlags::CLONE_NEWNET);
        result
    })
    .await
    .map_err(|e| anyhow!("spawn_blocking: {}", e))??;

    tokio::spawn(conn);
    Ok(handle)
}

// ── Public types ──────────────────────────────────────────────────────────────

/// A network interface discovered in the pod netns and prepared for VMM attachment.
///
/// The `link_type` field determines how this entry should be presented to the VMM:
/// - [`LinkType::Tap`] — a tap device was created; `name` is the tap device name.
/// - [`LinkType::Physical`] — a physical NIC bound to VFIO; `name` is the NIC name.
/// - [`LinkType::VhostUser`] — caller must set `vhost_user_socket` from CNI config;
///   `discover_network` never sets this variant on its own.
#[derive(Debug, Default)]
pub struct DiscoveredInterface {
    /// Interface name:
    /// - For [`LinkType::Tap`]: the tap device name, e.g. `"tap_kua_5"`.
    /// - For [`LinkType::Physical`]: the NIC interface name, e.g. `"eth0"`.
    pub name: String,
    /// MAC address string (colon-separated hex), from the original link.
    pub mac: String,
    /// MTU inherited from the original link.
    pub mtu: u32,
    /// Netlink link index of the original veth or NIC.
    pub veth_index: u32,
    /// IP addresses configured on the original veth or NIC.
    pub ip_addresses: Vec<IpNetwork>,
    /// Classified link type after discovery and preparation.
    pub link_type: LinkType,
    /// VhostUser socket path.  Always empty after `discover_network`; callers
    /// may populate this from CNI annotations before calling `Vmm::add_network`.
    pub vhost_user_socket: String,
    /// PCI BDF address for physical NICs (e.g. `"0000:00:1f.0"`).
    pub pci_address: String,
    /// Original PCI driver bound before VFIO rebind (e.g. `"igb"`, `"ixgbe"`).
    /// Used to restore the device to its original driver on sandbox stop.
    pub pci_driver: String,
}

// ── Network cleanup state ─────────────────────────────────────────────────────

/// Identifies a physical NIC that was rebound to VFIO during sandbox setup,
/// together with the driver it should be restored to on sandbox stop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhysicalNicState {
    /// PCI BDF address (e.g. `"0000:00:1f.0"`).
    pub bdf: String,
    /// Original driver before VFIO bind (e.g. `"igb"`).
    pub original_driver: String,
}

/// Network configuration discovered from the pod netns.
#[derive(Debug, Default)]
pub struct DiscoveredNetwork {
    pub interfaces: Vec<DiscoveredInterface>,
    pub routes: Vec<DiscoveredRoute>,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Discover network configuration from a pod network namespace and prepare
/// each interface for VMM attachment.
///
/// ### Veth interfaces
/// For each veth found in `netns`:
/// 1. Collects IP addresses from the veth.
/// 2. Creates a persistent tap device (`tap_kua_<index>`).
/// 3. Brings the tap up with the veth's MTU.
/// 4. Installs TC ingress qdiscs + redirect filters (veth ↔ tap).
///
/// ### Physical NICs
/// For each link of unknown type that has IP addresses:
/// 1. Runs `SIOCETHTOOL / ETHTOOL_GDRVINFO` to get the PCI BDF address.
/// 2. If a BDF is found, rebinds the device to `vfio-pci` via sysfs.
///
/// ### Routes
/// Collects all IPv4 and IPv6 routes in the main routing table that reference
/// a discovered interface.
pub async fn discover_network(
    netns: &str,
    _sandbox_id: &str,
    queue: u32,
) -> anyhow::Result<DiscoveredNetwork> {
    let handle = create_netlink_handle(netns).await?;

    // ── Enumerate all non-loopback links ──────────────────────────────────────
    let mut links_stream = handle.link().get().execute();
    // (index, name, mac, mtu, link_type)
    let mut raw_links: Vec<(u32, String, String, u32, LinkType)> = Vec::new();
    while let Some(msg) = links_stream
        .try_next()
        .await
        .map_err(|e| anyhow!("{}", e))?
    {
        if let Some(info) = parse_link_info(msg) {
            raw_links.push(info);
        }
    }

    // ── Process each link based on its type ───────────────────────────────────
    let mut interfaces = Vec::new();
    // Tracks (link_index, interface_name) for the route lookup below.
    let mut discovered_indices: Vec<(u32, String)> = Vec::new();

    for (index, name, mac, mtu, link_type) in raw_links {
        match &link_type {
            LinkType::Veth => {
                let ip_addresses = collect_ip_addresses(index, &handle).await?;
                let tap_name = format!("tap_kua_{}", index);

                // Create the tap device inside the pod netns.
                let tap_clone = tap_name.clone();
                let netns_str = netns.to_string();
                run_in_new_netns(&netns_str, move || create_tap_device(&tap_clone, queue))
                    .await??;

                // Set tap MTU + bring it up.
                let mut tap_stream = handle.link().get().match_name(tap_name.clone()).execute();
                if let Some(tap_msg) = tap_stream
                    .try_next()
                    .await
                    .map_err(|e| anyhow!("get tap {}: {}", tap_name, e))?
                {
                    handle
                        .link()
                        .set(tap_msg.header.index)
                        .mtu(mtu)
                        .up()
                        .execute()
                        .await
                        .map_err(|e| anyhow!("set tap {} mtu/up: {}", tap_name, e))?;
                }

                // TC ingress qdiscs + redirect filters: veth ↔ tap.
                set_up_tc_redirect(netns, &name, &tap_name).await?;

                discovered_indices.push((index, name.clone()));
                interfaces.push(DiscoveredInterface {
                    name: tap_name,
                    mac,
                    mtu,
                    veth_index: index,
                    ip_addresses,
                    link_type: LinkType::Tap,
                    ..DiscoveredInterface::default()
                });
            }

            LinkType::Unknown => {
                // Could be a physical NIC.  Only proceed if it has IP addresses.
                let ip_addresses = collect_ip_addresses(index, &handle).await?;
                if ip_addresses.is_empty() {
                    continue;
                }

                // Probe PCI BDF via ethtool (run inside the pod netns).
                let if_name_clone = name.clone();
                let netns_str = netns.to_string();
                let bdf_result = if !netns.is_empty() {
                    run_in_new_netns(&netns_str, move || get_bdf_for_eth(&if_name_clone)).await?
                } else {
                    get_bdf_for_eth(&if_name_clone)
                };
                let bdf = match bdf_result {
                    Ok(b) if !b.is_empty() => b,
                    _ => continue, // no BDF → not a physical NIC we can handle
                };

                // Capture the original driver before rebinding so it can be
                // restored when the sandbox stops.
                let original_driver = get_pci_driver(&bdf).await.unwrap_or_default();

                // Bind to VFIO for passthrough.
                if let Err(e) = bind_device_to_driver(DEVICE_DRIVER_VFIO, &bdf).await {
                    tracing::warn!("bind {} to vfio-pci failed: {}", bdf, e);
                    continue;
                }

                discovered_indices.push((index, name.clone()));
                interfaces.push(DiscoveredInterface {
                    name: name.clone(),
                    mac,
                    mtu,
                    veth_index: index,
                    ip_addresses,
                    link_type: LinkType::Physical(bdf.clone(), DEVICE_DRIVER_VFIO.to_string()),
                    pci_address: bdf,
                    pci_driver: original_driver,
                    ..DiscoveredInterface::default()
                });
            }

            // Bridge, Vlan, Vxlan, Tun, etc. — not relevant for VM networking.
            _ => {}
        }
    }

    // ── Collect routes from discovered interfaces ─────────────────────────────
    let idx_to_name: HashMap<u32, &str> = discovered_indices
        .iter()
        .map(|(i, n)| (*i, n.as_str()))
        .collect();

    let mut routes = Vec::new();
    collect_routes(IpVersion::V4, &handle, &idx_to_name, &mut routes).await?;
    collect_routes(IpVersion::V6, &handle, &idx_to_name, &mut routes).await?;

    Ok(DiscoveredNetwork { interfaces, routes })
}

/// Remove persistent tap devices previously created by [`discover_network`].
///
/// Only pass tap device names (i.e., `DiscoveredInterface` entries whose
/// `link_type` is [`LinkType::Tap`]).  Uses `ip tuntap del` to remove each.
pub async fn destroy_tap_devices(tap_names: &[String]) {
    for name in tap_names {
        let output = tokio::process::Command::new("ip")
            .args(["tuntap", "del", "dev", name, "mode", "tap"])
            .output()
            .await;
        if let Err(e) = output {
            tracing::warn!("destroy tap {}: {}", name, e);
        }
    }
}

/// Restore physical NICs previously rebound to VFIO by [`discover_network`].
///
/// For each entry, calls [`bind_device_to_driver`] to rebind the device back
/// to its original kernel driver so the NIC is usable by the host again.
/// Skips entries where `original_driver` is empty (driver was unknown at
/// setup time).  Failures are logged as warnings rather than returned as
/// errors so that all NICs are attempted even if one fails.
pub async fn restore_physical_devices(nics: &[PhysicalNicState]) {
    for nic in nics {
        if nic.original_driver.is_empty() {
            tracing::warn!(
                "restore physical NIC {}: original driver unknown, skipping rebind",
                nic.bdf
            );
            continue;
        }
        if let Err(e) = bind_device_to_driver(&nic.original_driver, &nic.bdf).await {
            tracing::warn!(
                "restore physical NIC {} to driver {}: {}",
                nic.bdf,
                nic.original_driver,
                e
            );
        }
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Parse a `LinkMessage` into `(index, name, mac, mtu, LinkType)`.
///
/// Returns `None` for loopback interfaces or links with no name.
fn parse_link_info(msg: LinkMessage) -> Option<(u32, String, String, u32, LinkType)> {
    let index = msg.header.index;
    let mut name = String::new();
    let mut mac = String::new();
    let mut mtu: u32 = 1500;
    let mut link_type = LinkType::Unknown;

    if msg
        .header
        .flags
        .iter()
        .any(|f| matches!(f, LinkFlag::Loopback))
    {
        return None;
    }

    for attr in msg.attributes {
        match attr {
            LinkAttribute::IfName(n) => name = n,
            LinkAttribute::Address(addr) => {
                mac = addr
                    .iter()
                    .map(|b| format!("{:02x}", b))
                    .collect::<Vec<_>>()
                    .join(":");
            }
            LinkAttribute::Mtu(m) => mtu = m,
            LinkAttribute::LinkInfo(infos) => {
                for info in infos {
                    match info {
                        LinkInfo::Kind(InfoKind::Veth) => {
                            link_type = LinkType::Veth;
                        }
                        LinkInfo::Data(d) => {
                            link_type = LinkType::from(d);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    if !name.is_empty() {
        Some((index, name, mac, mtu, link_type))
    } else {
        None
    }
}

/// Collect all IP addresses assigned to a link (by interface index).
async fn collect_ip_addresses(link_index: u32, handle: &Handle) -> anyhow::Result<Vec<IpNetwork>> {
    use netlink_packet_route::address::AddressAttribute;

    let mut addrs = Vec::new();
    let mut stream = handle
        .address()
        .get()
        .set_link_index_filter(link_index)
        .execute();
    while let Some(msg) = stream.try_next().await.map_err(|e| anyhow!("{}", e))? {
        for attr in msg.attributes {
            if let AddressAttribute::Address(ip) = attr {
                if let Ok(net) = IpNetwork::new(ip, msg.header.prefix_len) {
                    addrs.push(net);
                }
            }
        }
    }
    Ok(addrs)
}

/// Collect routes from the main routing table that reference a known interface.
async fn collect_routes(
    ip_version: IpVersion,
    handle: &Handle,
    idx_to_name: &HashMap<u32, &str>,
    out: &mut Vec<DiscoveredRoute>,
) -> anyhow::Result<()> {
    let mut stream = handle.route().get(ip_version).execute();
    while let Some(msg) = stream.try_next().await.map_err(|e| anyhow!("{}", e))? {
        if msg.header.table != libc::RT_TABLE_MAIN {
            continue;
        }

        let prefix_len = msg.header.destination_prefix_length;
        let scope: u8 = msg.header.scope.into();
        let family: u8 = msg.header.address_family.into();
        let flags: u32 = msg.header.flags.iter().map(|f| u32::from(*f)).sum();

        let mut dest = String::new();
        let mut gateway = String::new();
        let mut device = String::new();
        let mut source = String::new();

        for attr in msg.attributes {
            match attr {
                RouteAttribute::Destination(v) => {
                    dest = format!("{}/{}", route_address_to_string(&v), prefix_len);
                }
                RouteAttribute::Gateway(v) => {
                    gateway = route_address_to_string(&v);
                }
                RouteAttribute::Oif(idx) => {
                    if let Some(name) = idx_to_name.get(&idx) {
                        device = name.to_string();
                    }
                }
                RouteAttribute::Source(v) => {
                    source = route_address_to_string(&v);
                }
                _ => {}
            }
        }

        if !device.is_empty() {
            out.push(DiscoveredRoute {
                dest,
                gateway,
                device,
                source,
                scope,
                family,
                flags,
            });
        }
    }
    Ok(())
}

/// Set up TC ingress qdiscs and redirect filters so frames flow veth ↔ tap.
async fn set_up_tc_redirect(netns: &str, veth: &str, tap: &str) -> anyhow::Result<()> {
    tc_exec(netns, &["qdisc", "add", "dev", tap, "ingress"]).await?;
    tc_exec(netns, &["qdisc", "add", "dev", veth, "ingress"]).await?;
    tc_exec(
        netns,
        &[
            "filter", "add", "dev", tap, "parent", "ffff:", "protocol", "all", "u32", "match",
            "u8", "0", "0", "action", "mirred", "egress", "redirect", "dev", veth,
        ],
    )
    .await?;
    tc_exec(
        netns,
        &[
            "filter", "add", "dev", veth, "parent", "ffff:", "protocol", "all", "u32", "match",
            "u8", "0", "0", "action", "mirred", "egress", "redirect", "dev", tap,
        ],
    )
    .await?;
    Ok(())
}

async fn tc_exec(netns: &str, args: &[&str]) -> anyhow::Result<()> {
    let mut cmd = std::process::Command::new("tc");
    cmd.args(args);
    execute_in_netns(netns, cmd).await
}

async fn execute_in_netns(netns: &str, mut cmd: std::process::Command) -> anyhow::Result<()> {
    let output = if !netns.is_empty() {
        run_in_new_netns(netns, move || cmd.output()).await??
    } else {
        cmd.output().map_err(|e| anyhow!("{}", e))?
    };
    if !output.status.success() {
        return Err(anyhow!(
            "command failed ({}): stdout={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ));
    }
    Ok(())
}

/// Run a closure inside a given network namespace by spawning a blocking thread,
/// opening the netns fd, calling `setns`, executing the closure, then restoring.
async fn run_in_new_netns<F, R>(netns: &str, f: F) -> anyhow::Result<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let netns = netns.to_string();
    spawn_blocking(move || {
        // Open the host netns so we can restore it afterwards.
        let host_ns_fd = std::fs::File::open("/proc/self/ns/net")
            .map_err(|e| anyhow!("open host netns: {}", e))?;

        // Enter the target netns.
        let target_fd =
            std::fs::File::open(&netns).map_err(|e| anyhow!("open netns {}: {}", netns, e))?;
        setns(target_fd.as_raw_fd(), CloneFlags::CLONE_NEWNET)
            .map_err(|e| anyhow!("setns {}: {}", netns, e))?;

        let result = f();

        // Restore host netns (best-effort).
        let _ = setns(host_ns_fd.as_raw_fd(), CloneFlags::CLONE_NEWNET);

        Ok(result)
    })
    .await
    .map_err(|e| anyhow!("spawn_blocking: {}", e))?
}
