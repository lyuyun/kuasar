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

/// A network route discovered in the pod netns.
#[derive(Debug, Default, Clone)]
pub struct DiscoveredRoute {
    /// Destination CIDR, e.g. `"10.244.0.0/24"` (empty string for the default route).
    pub dest: String,
    /// Gateway IP address string (empty if not set).
    pub gateway: String,
    /// Output device name (the name of the veth endpoint in the pod netns).
    pub device: String,
    /// Source IP address string (from `RouteAttribute::Source`, often empty).
    pub source: String,
    /// Route scope (`RT_SCOPE_UNIVERSE` = 0, etc.).
    pub scope: u8,
    /// Address family (`AF_INET` = 2, `AF_INET6` = 10).
    pub family: u8,
    /// Route flags as a bitmask.
    pub flags: u32,
}
