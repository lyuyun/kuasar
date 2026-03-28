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
    ffi::CStr,
    fmt::{Display, Formatter},
    os::unix::prelude::{AsRawFd, OwnedFd},
};

use anyhow::anyhow;
use libc::{IFF_MULTI_QUEUE, IFF_NO_PI, IFF_TAP, IFF_VNET_HDR};
use netlink_packet_route::link::{
    InfoData, InfoIpVlan, InfoKind, InfoMacVlan, InfoMacVtap, InfoVlan, InfoVxlan, LinkInfo,
    LinkMessage,
};
use nix::{
    ioctl_read_bad, ioctl_write_ptr_bad,
    sys::socket::{socket, AddressFamily, SockFlag, SockType},
    unistd::close,
};

// ── ioctl constants ───────────────────────────────────────────────────────────

const SIOCETHTOOL: u64 = 0x8946;
const ETHTOOL_GDRVINFO: u32 = 0x00000003;

const TUNSETIFF: u64 = 0x400454ca;
const TUNSETPERSIST: u64 = 0x400454cb;

// ── ioctl structs ─────────────────────────────────────────────────────────────

/// `ifreq` struct used for the TUNSETIFF / TUNSETPERSIST ioctls.
#[repr(C)]
#[derive(Debug)]
pub struct TunIfreq {
    pub ifr_name: [u8; 16],
    pub ifr_flags: u16,
    pub ifr_pad: [u8; 22],
}

/// `ifreq` struct used for the SIOCETHTOOL ioctl (ifr_data points to `EthtoolDrvInfo`).
#[repr(C)]
pub struct EthtoolIfreq {
    pub ifr_name: [u8; 16],
    pub ifr_data: *mut libc::c_void,
}

/// Driver info returned by `ETHTOOL_GDRVINFO`.
#[repr(C)]
pub struct EthtoolDrvInfo {
    pub cmd: u32,
    pub driver: [u8; 32],
    pub version: [u8; 32],
    pub fw_version: [u8; 32],
    pub bus_info: [u8; 32],
    pub erom_version: [u8; 32],
    pub reserved2: [u8; 12],
    pub n_priv_flags: u32,
    pub n_stats: u32,
    pub testinfo_len: u32,
    pub eedump_len: u32,
    pub regdump_len: u32,
}

ioctl_write_ptr_bad!(ioctl_tun_set_iff, TUNSETIFF, TunIfreq);
ioctl_write_ptr_bad!(ioctl_tun_set_persist, TUNSETPERSIST, u64);
ioctl_read_bad!(ioctl_get_drive_info, SIOCETHTOOL, EthtoolIfreq);

// ── LinkType ──────────────────────────────────────────────────────────────────

/// The type of a network link as detected via netlink.
#[derive(Debug, Clone)]
pub enum LinkType {
    Unknown,
    Bridge,
    Veth,
    Vlan(u16),
    Vxlan(u32),
    Bond,
    Ipvlan(u16),
    Macvlan(u32),
    Macvtap(u32),
    Tun,
    /// VhostUser socket path (filled externally from CNI config, not via netlink).
    VhostUser(String),
    /// Physical NIC: `(bdf, driver)` e.g. `("0000:00:1f.0", "ixgbe")`.
    Physical(String, String),
    Tap,
    Loopback,
}

impl Default for LinkType {
    fn default() -> Self {
        Self::Unknown
    }
}

impl Display for LinkType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            LinkType::Unknown => "",
            LinkType::Bridge => "bridge",
            LinkType::Veth => "veth",
            LinkType::Vlan(_) => "vlan",
            LinkType::Vxlan(_) => "vxlan",
            LinkType::Bond => "bond",
            LinkType::Ipvlan(_) => "ipvlan",
            LinkType::Macvlan(_) => "macvlan",
            LinkType::Macvtap(_) => "macvtap",
            LinkType::Tun => "tun",
            LinkType::VhostUser(_) => "vhostuser",
            LinkType::Physical(_, _) => "physical",
            LinkType::Tap => "tap",
            LinkType::Loopback => "loopback",
        };
        write!(f, "{}", s)
    }
}

impl From<InfoData> for LinkType {
    fn from(d: InfoData) -> Self {
        match d {
            InfoData::Bridge(_) => Self::Bridge,
            InfoData::Tun(_) => Self::Tun,
            InfoData::Vlan(infos) => {
                if let Some(InfoVlan::Id(i)) = infos.first() {
                    return Self::Vlan(*i);
                }
                Self::Unknown
            }
            InfoData::Veth(_) => Self::Veth,
            InfoData::Vxlan(infos) => {
                if let Some(InfoVxlan::Id(i)) = infos.first() {
                    return Self::Vxlan(*i);
                }
                Self::Unknown
            }
            InfoData::Bond(_) => Self::Bond,
            InfoData::IpVlan(infos) => {
                if let Some(InfoIpVlan::Mode(i)) = infos.first() {
                    return Self::Ipvlan(*i);
                }
                Self::Unknown
            }
            InfoData::MacVlan(infos) => {
                if let Some(InfoMacVlan::Mode(i)) = infos.first() {
                    return Self::Macvlan(*i);
                }
                Self::Unknown
            }
            InfoData::MacVtap(infos) => {
                if let Some(InfoMacVtap::Mode(i)) = infos.first() {
                    return Self::Macvtap(*i);
                }
                Self::Unknown
            }
            _ => Self::Unknown,
        }
    }
}

/// Determine the `LinkType` of a link from its `LinkMessage`.
///
/// Returns `LinkType::Loopback` for loopback links.
/// Returns `LinkType::Veth` for veth links (InfoKind is used since veth
/// carries no InfoData).
/// All other types are derived from `InfoData`.
pub fn link_type_from_message(msg: &LinkMessage) -> LinkType {
    use netlink_packet_route::link::{LinkAttribute, LinkFlag};

    if msg
        .header
        .flags
        .iter()
        .any(|f| matches!(f, LinkFlag::Loopback))
    {
        return LinkType::Loopback;
    }

    for attr in &msg.attributes {
        if let LinkAttribute::LinkInfo(infos) = attr {
            for info in infos {
                match info {
                    LinkInfo::Kind(InfoKind::Veth) => return LinkType::Veth,
                    LinkInfo::Data(d) => return LinkType::from(d.clone()),
                    _ => {}
                }
            }
        }
    }
    LinkType::Unknown
}

// ── Tap device creation ───────────────────────────────────────────────────────

/// Create a persistent multi-queue tap device with the given name.
///
/// Opens `/dev/net/tun` `queue` times, applying `TUNSETIFF` to each fd,
/// then calls `TUNSETPERSIST` on the first fd so the device outlives the fds.
///
/// Must be called from within the target network namespace.
pub fn create_tap_device(tap_name: &str, mut queue: u32) -> anyhow::Result<Vec<OwnedFd>> {
    if tap_name.len() > 15 {
        return Err(anyhow!("tap name '{}' exceeds 15 characters", tap_name));
    }
    let mut if_name_arr = [0u8; 16];
    if_name_arr[..tap_name.len()].copy_from_slice(tap_name.as_bytes());
    if queue == 0 {
        queue = 1;
    }

    let flags = (IFF_TAP | IFF_NO_PI | IFF_MULTI_QUEUE | IFF_VNET_HDR) as u16;
    let req = TunIfreq {
        ifr_name: if_name_arr,
        ifr_flags: flags,
        ifr_pad: [0; 22],
    };

    let mut fds: Vec<OwnedFd> = Vec::new();
    for _ in 0..queue {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/net/tun")
            .map_err(|e| anyhow!("open /dev/net/tun: {}", e))?;
        unsafe {
            ioctl_tun_set_iff(f.as_raw_fd(), &req)
                .map_err(|e| anyhow!("TUNSETIFF {}: {}", tap_name, e))?;
        }
        fds.push(OwnedFd::from(f));
    }
    if let Some(first) = fds.first() {
        unsafe {
            ioctl_tun_set_persist(first.as_raw_fd(), &1u64)
                .map_err(|e| anyhow!("TUNSETPERSIST {}: {}", tap_name, e))?;
        }
    }
    Ok(fds)
}

// ── Physical NIC detection ────────────────────────────────────────────────────

/// Get the PCI BDF address of a network interface via `SIOCETHTOOL / ETHTOOL_GDRVINFO`.
///
/// Must be called from within the target network namespace.
pub fn get_bdf_for_eth(if_name: &str) -> anyhow::Result<String> {
    if if_name.len() > 15 {
        return Err(anyhow!(
            "interface name '{}' exceeds 15 characters",
            if_name
        ));
    }
    let sock = socket(
        AddressFamily::Inet,
        SockType::Datagram,
        SockFlag::empty(),
        None,
    )
    .map_err(|e| anyhow!("create inet socket for ethtool: {}", e))?;

    let mut drv_info = EthtoolDrvInfo {
        cmd: ETHTOOL_GDRVINFO,
        driver: [0u8; 32],
        version: [0u8; 32],
        fw_version: [0u8; 32],
        bus_info: [0u8; 32],
        erom_version: [0u8; 32],
        reserved2: [0u8; 12],
        n_priv_flags: 0,
        n_stats: 0,
        testinfo_len: 0,
        eedump_len: 0,
        regdump_len: 0,
    };

    let mut if_name_arr = [0u8; 16];
    if_name_arr[..if_name.len()].copy_from_slice(if_name.as_bytes());
    let mut req = EthtoolIfreq {
        ifr_name: if_name_arr,
        ifr_data: &mut drv_info as *mut _ as *mut libc::c_void,
    };

    let result = unsafe { ioctl_get_drive_info(sock, &mut req) };
    close(sock).unwrap_or_default();
    result.map_err(|e| anyhow!("SIOCETHTOOL {} failed: {}", if_name, e))?;

    let bdf = unsafe { CStr::from_ptr(drv_info.bus_info.as_ptr() as *const libc::c_char) };
    let bdf = bdf
        .to_str()
        .map_err(|e| anyhow!("BDF string for {} is invalid UTF-8: {}", if_name, e))?;
    Ok(bdf.to_string())
}

pub const DEVICE_DRIVER_VFIO: &str = "vfio-pci";

/// Bind a PCI device to `driver` via sysfs `driver_override` → `unbind` → `drivers_probe`.
///
/// No-op if the device is already bound to `driver`.
pub async fn bind_device_to_driver(driver: &str, bdf: &str) -> anyhow::Result<()> {
    if get_pci_driver(bdf).await.as_deref().unwrap_or("") == driver {
        return Ok(());
    }
    let override_path = format!("/sys/bus/pci/devices/{}/driver_override", bdf);
    tokio::fs::write(&override_path, driver)
        .await
        .map_err(|e| anyhow!("write driver_override {}: {}", bdf, e))?;
    let unbind_path = format!("/sys/bus/pci/devices/{}/driver/unbind", bdf);
    if std::path::Path::new(&unbind_path).exists() {
        tokio::fs::write(&unbind_path, bdf)
            .await
            .map_err(|e| anyhow!("unbind {}: {}", bdf, e))?;
    }
    tokio::fs::write("/sys/bus/pci/drivers_probe", bdf)
        .await
        .map_err(|e| anyhow!("drivers_probe {}: {}", bdf, e))?;
    let new_driver = get_pci_driver(bdf).await?;
    if new_driver != driver {
        return Err(anyhow!(
            "device {} driver expected {} but got {}",
            bdf,
            driver,
            new_driver
        ));
    }
    Ok(())
}

/// Read the PCI driver currently bound to `bdf` from sysfs.
pub async fn get_pci_driver(bdf: &str) -> anyhow::Result<String> {
    let driver_path = format!("/sys/bus/pci/devices/{}/driver", bdf);
    let dest = tokio::fs::read_link(&driver_path)
        .await
        .map_err(|e| anyhow!("readlink {}: {}", driver_path, e))?;
    let name = dest
        .file_name()
        .ok_or_else(|| anyhow!("no file name in {:?}", dest))?
        .to_str()
        .ok_or_else(|| anyhow!("non-UTF-8 driver name in {:?}", dest))?
        .to_string();
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::create_tap_device;

    #[test]
    fn tap_name_too_long_is_rejected() {
        let name = "a_very_long_tap_device_name";
        assert!(create_tap_device(name, 1).is_err());
    }
}
