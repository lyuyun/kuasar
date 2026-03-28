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

use std::net::IpAddr;

use anyhow::anyhow;
/// `IpNetwork` re-exported as `IpNet` for convenience.
///
/// This is the same type used by `vmm_guest_runtime::IpNet`, so
/// `DiscoveredInterface::ip_addresses` can be assigned directly to
/// `vmm_guest_runtime::NetworkInterface::ip_addresses` without conversion.
pub use ipnetwork::IpNetwork as IpNet;
use netlink_packet_route::route::RouteAddress;

/// Format a raw MAC address byte slice into the canonical `"xx:xx:xx:xx:xx:xx"` string.
pub fn mac_bytes_to_string(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<Vec<_>>()
        .join(":")
}

/// Convert a `RouteAddress` to a standard `IpAddr`.
pub fn convert_route_address_to_ip(address: RouteAddress) -> anyhow::Result<IpAddr> {
    match address {
        RouteAddress::Inet(addr) => Ok(IpAddr::V4(addr)),
        RouteAddress::Inet6(addr) => Ok(IpAddr::V6(addr)),
        _ => Err(anyhow!("unsupported route address {:?}", address)),
    }
}

/// Convert a `RouteAddress` to a display string (empty on unsupported variants).
pub fn route_address_to_string(addr: &RouteAddress) -> String {
    match addr {
        RouteAddress::Inet(v4) => v4.to_string(),
        RouteAddress::Inet6(v6) => v6.to_string(),
        _ => String::new(),
    }
}
