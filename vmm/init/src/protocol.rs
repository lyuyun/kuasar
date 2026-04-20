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

//! JSON Lines protocol between kuasar-init (guest) and ApplianceRuntime (host).
//!
//! Each message is a single JSON object followed by `\n`.  The connection
//! direction is guest → host: kuasar-init connects to vsock CID 2 port 1024;
//! Cloud Hypervisor's vsock device proxies this to a Unix socket on the host
//! that ApplianceRuntime is listening on.

use serde::{Deserialize, Serialize};

pub const VSOCK_HOST_CID: u32 = 2; // VMADDR_CID_HOST
pub const VSOCK_PORT: u32 = 1024;

// ── Guest → Host ──────────────────────────────────────────────────────────────

/// Messages sent by kuasar-init to the host.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GuestMessage {
    /// Application is ready to serve; first message on a new connection.
    Ready {
        sandbox_id: String,
        /// Protocol version — always "1" for now.
        version: String,
        /// Identifies this init implementation.
        init: String,
    },
    /// Periodic liveness signal sent every `heartbeat_interval_ms`.
    Heartbeat {
        sandbox_id: String,
        timestamp_ms: u64,
    },
    /// Unrecoverable error; host should reclaim the VM.
    Fatal {
        sandbox_id: String,
        reason: String,
        exit_code: i32,
    },
}

// ── Host → Guest ──────────────────────────────────────────────────────────────

/// Messages received by kuasar-init from the host.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HostMessage {
    /// Graceful shutdown request.  kuasar-init must power off within
    /// `deadline_ms` milliseconds; force-kill the app if it does not exit.
    Shutdown { deadline_ms: u64 },
    /// One-time configuration injection (DNS, hostname, env vars, …).
    Config(ConfigPayload),
    /// Connectivity probe; no response required.
    Ping,
}

#[derive(Debug, Deserialize, Default)]
pub struct ConfigPayload {
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub dns_servers: Vec<String>,
    #[serde(default)]
    pub search_domains: Vec<String>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
}

/// Encode a GuestMessage as a JSON Line (trailing `\n` included).
pub fn encode(msg: &GuestMessage) -> anyhow::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(msg)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Decode a HostMessage from a JSON Line (trailing `\n` stripped by caller).
pub fn decode(line: &str) -> anyhow::Result<HostMessage> {
    serde_json::from_str(line).map_err(Into::into)
}
