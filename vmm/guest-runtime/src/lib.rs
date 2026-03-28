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

#![deny(unused_imports)]

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use containerd_sandbox::data::SandboxData;
pub use ipnetwork::IpNetwork as IpNet;
use serde::{Deserialize, Serialize};

// ── Supporting types ──────────────────────────────────────────────────────────

/// Result of a successful wait_ready call.
pub struct ReadyResult {
    pub sandbox_id: String,
    pub timestamp_ms: u64,
}

/// Exit status of a process or VM.
pub struct ExitStatus {
    pub exit_code: i32,
    pub exited_at_ms: u64,
}

/// Resource statistics for a sandbox or container.
pub struct ContainerStats {
    pub cpu_usage_ns: u64,
    pub memory_rss_bytes: u64,
    pub pids_current: u64,
}

/// OCI container specification (passed to vmm-task).
/// Wraps the raw bytes to avoid coupling guest-runtime to OCI crate.
pub struct ContainerSpec {
    pub id: String,
    pub bundle: String,
    pub rootfs: Vec<Mount>,
    pub io: ContainerIo,
    /// OCI spec JSON bytes forwarded verbatim to vmm-task.
    pub spec_json: Vec<u8>,
}

pub struct ContainerIo {
    pub stdin: String,
    pub stdout: String,
    pub stderr: String,
    pub terminal: bool,
}

pub struct ContainerInfo {
    pub pid: u32,
}

pub struct ExecSpec {
    pub exec_id: String,
    pub spec_json: Vec<u8>,
    pub io: ContainerIo,
}

pub struct ProcessInfo {
    pub pid: u32,
}

pub struct Mount {
    pub kind: String,
    pub source: String,
    pub target: String,
    pub options: Vec<String>,
}

// ── Supporting types for GuestReadiness ──────────────────────────────────────

/// Discovered network interface configuration (from the pod netns).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkInterface {
    pub name: String,
    pub mac: String,
    pub ip_addresses: Vec<IpNet>, // CIDR notation
    pub mtu: u32,
}

/// IP route entry discovered from the pod netns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    pub dest: String, // CIDR or "default"
    pub gateway: String,
    pub device: String,
}

/// Request sent to vmm-task via SetupSandbox ttrpc call after the VM boots.
/// Contains everything the guest needs to configure networking and identity.
#[derive(Debug)]
pub struct SandboxSetupRequest {
    pub interfaces: Vec<NetworkInterface>,
    pub routes: Vec<Route>,
    /// Provides hostname, DNS config, pod annotations.
    pub sandbox_data: SandboxData,
}

// ── Traits ────────────────────────────────────────────────────────────────────

/// Guest readiness abstraction used by SandboxEngine.
///
/// Contains only the three methods the engine itself calls: `wait_ready`,
/// `setup_sandbox`, `forward_events`. Process-management operations
/// (`kill_process`, `wait_process`, `container_stats`) belong in
/// `ContainerRuntime` — they are only called by `K8sAdapter`.
///
/// Implementation: `VmmTaskRuntime` (`vmm/runtime-vmm-task`): ttrpc over vsock
/// to vmm-task.
#[async_trait]
pub trait GuestReadiness: Send + Sync + 'static {
    /// Wait for the guest to become ready to serve.
    /// Calls ttrpc Check(), then SetupSandbox(); returns when vmm-task is responsive.
    /// Returns Err if timeout expires; the sandbox transitions to Stopped.
    async fn wait_ready(&self, sandbox_id: &str, vsock_path: &str) -> Result<ReadyResult>;

    /// Send network interface + route configuration to the guest via
    /// SetupSandboxRequest, and push PodSandboxConfig for DNS/hostname
    /// resolution inside the VM. Called once after wait_ready() succeeds,
    /// before the sandbox is marked Running.
    async fn setup_sandbox(&self, sandbox_id: &str, req: &SandboxSetupRequest) -> Result<()>;

    /// Spawn a background task that polls vmm-task for OOM/exit events and
    /// forwards them to containerd. Runs until the exit_signal fires.
    /// Called once after the sandbox transitions to Running.
    async fn forward_events(
        &self,
        sandbox_id: &str,
        exit_signal: Arc<containerd_sandbox::signal::ExitSignal>,
    );

    /// Release any per-sandbox resources held by the runtime (e.g. ttrpc client
    /// connections). Called after a sandbox is stopped or deleted.
    /// Implementations that do not hold per-sandbox state may use the default no-op.
    async fn cleanup_sandbox(&self, _sandbox_id: &str) {}
}

/// Container lifecycle and process-management operations via vmm-task ttrpc.
/// `VmmTaskRuntime` implements this trait.
///
/// `ContainerRuntime` is intentionally independent from `GuestReadiness` —
/// they can be implemented by different types or combined via a where bound at
/// the call site. `SandboxEngine` only requires `R: GuestReadiness` (readiness
/// + network setup). `K8sAdapter` additionally requires
/// `R: ContainerRuntime` for Task API delegation, expressed as a combined
/// bound: `R: GuestReadiness + ContainerRuntime`.
#[async_trait]
pub trait ContainerRuntime: Send + Sync + 'static {
    /// Create a container inside the VM via ttrpc create_container().
    async fn create_container(
        &self,
        sandbox_id: &str,
        spec: ContainerSpec,
    ) -> Result<ContainerInfo>;

    /// Start the main process of a container via ttrpc start_process().
    async fn start_process(&self, sandbox_id: &str, container_id: &str) -> Result<ProcessInfo>;

    /// Execute an additional process inside a container via ttrpc exec_process().
    async fn exec_process(
        &self,
        sandbox_id: &str,
        container_id: &str,
        spec: ExecSpec,
    ) -> Result<ProcessInfo>;

    /// Signal a process to stop via ttrpc signal_process().
    async fn kill_process(
        &self,
        sandbox_id: &str,
        container_id: &str,
        pid: u32,
        signal: u32,
    ) -> Result<()>;

    /// Wait for a process to exit and return its status via ttrpc wait_process().
    async fn wait_process(
        &self,
        sandbox_id: &str,
        container_id: &str,
        pid: u32,
    ) -> Result<ExitStatus>;

    /// Read container/sandbox resource statistics via ttrpc get_stats().
    async fn container_stats(&self, sandbox_id: &str, container_id: &str)
        -> Result<ContainerStats>;
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Compile-time independence checks ─────────────────────────────────────
    //
    // These functions verify that GuestReadiness and ContainerRuntime are
    // independent traits: a type can implement one without the other, and a
    // bound can require both via `R: GuestReadiness + ContainerRuntime`.

    fn _requires_guest_readiness<R: GuestReadiness>() {}
    fn _requires_container_runtime<R: ContainerRuntime>() {}
    fn _requires_both<R: GuestReadiness + ContainerRuntime>() {}

    #[allow(dead_code)]
    struct GuestOnly;

    #[async_trait]
    impl GuestReadiness for GuestOnly {
        async fn wait_ready(&self, _sandbox_id: &str, _vsock_path: &str) -> Result<ReadyResult> {
            Ok(ReadyResult {
                sandbox_id: String::new(),
                timestamp_ms: 0,
            })
        }

        async fn setup_sandbox(&self, _sandbox_id: &str, _req: &SandboxSetupRequest) -> Result<()> {
            Ok(())
        }

        async fn forward_events(
            &self,
            _sandbox_id: &str,
            _exit_signal: Arc<containerd_sandbox::signal::ExitSignal>,
        ) {
        }
    }

    #[allow(dead_code)]
    struct RuntimeOnly;

    #[async_trait]
    impl ContainerRuntime for RuntimeOnly {
        async fn create_container(
            &self,
            _sandbox_id: &str,
            _spec: ContainerSpec,
        ) -> Result<ContainerInfo> {
            Ok(ContainerInfo { pid: 0 })
        }

        async fn start_process(
            &self,
            _sandbox_id: &str,
            _container_id: &str,
        ) -> Result<ProcessInfo> {
            Ok(ProcessInfo { pid: 0 })
        }

        async fn exec_process(
            &self,
            _sandbox_id: &str,
            _container_id: &str,
            _spec: ExecSpec,
        ) -> Result<ProcessInfo> {
            Ok(ProcessInfo { pid: 0 })
        }

        async fn kill_process(
            &self,
            _sandbox_id: &str,
            _container_id: &str,
            _pid: u32,
            _signal: u32,
        ) -> Result<()> {
            Ok(())
        }

        async fn wait_process(
            &self,
            _sandbox_id: &str,
            _container_id: &str,
            _pid: u32,
        ) -> Result<ExitStatus> {
            Ok(ExitStatus {
                exit_code: 0,
                exited_at_ms: 0,
            })
        }

        async fn container_stats(
            &self,
            _sandbox_id: &str,
            _container_id: &str,
        ) -> Result<ContainerStats> {
            Ok(ContainerStats {
                cpu_usage_ns: 0,
                memory_rss_bytes: 0,
                pids_current: 0,
            })
        }
    }

    #[allow(dead_code)]
    struct Combined;

    #[async_trait]
    impl GuestReadiness for Combined {
        async fn wait_ready(&self, _sandbox_id: &str, _vsock_path: &str) -> Result<ReadyResult> {
            Ok(ReadyResult {
                sandbox_id: String::new(),
                timestamp_ms: 0,
            })
        }

        async fn setup_sandbox(&self, _sandbox_id: &str, _req: &SandboxSetupRequest) -> Result<()> {
            Ok(())
        }

        async fn forward_events(
            &self,
            _sandbox_id: &str,
            _exit_signal: Arc<containerd_sandbox::signal::ExitSignal>,
        ) {
        }
    }

    #[async_trait]
    impl ContainerRuntime for Combined {
        async fn create_container(
            &self,
            _sandbox_id: &str,
            _spec: ContainerSpec,
        ) -> Result<ContainerInfo> {
            Ok(ContainerInfo { pid: 0 })
        }

        async fn start_process(
            &self,
            _sandbox_id: &str,
            _container_id: &str,
        ) -> Result<ProcessInfo> {
            Ok(ProcessInfo { pid: 0 })
        }

        async fn exec_process(
            &self,
            _sandbox_id: &str,
            _container_id: &str,
            _spec: ExecSpec,
        ) -> Result<ProcessInfo> {
            Ok(ProcessInfo { pid: 0 })
        }

        async fn kill_process(
            &self,
            _sandbox_id: &str,
            _container_id: &str,
            _pid: u32,
            _signal: u32,
        ) -> Result<()> {
            Ok(())
        }

        async fn wait_process(
            &self,
            _sandbox_id: &str,
            _container_id: &str,
            _pid: u32,
        ) -> Result<ExitStatus> {
            Ok(ExitStatus {
                exit_code: 0,
                exited_at_ms: 0,
            })
        }

        async fn container_stats(
            &self,
            _sandbox_id: &str,
            _container_id: &str,
        ) -> Result<ContainerStats> {
            Ok(ContainerStats {
                cpu_usage_ns: 0,
                memory_rss_bytes: 0,
                pids_current: 0,
            })
        }
    }

    // Compile-time check: GuestOnly satisfies GuestReadiness but NOT ContainerRuntime
    fn _check_guest_only() {
        _requires_guest_readiness::<GuestOnly>();
    }

    // Compile-time check: RuntimeOnly satisfies ContainerRuntime but NOT GuestReadiness
    fn _check_runtime_only() {
        _requires_container_runtime::<RuntimeOnly>();
    }

    // Compile-time check: Combined satisfies both
    fn _check_combined() {
        _requires_guest_readiness::<Combined>();
        _requires_container_runtime::<Combined>();
        _requires_both::<Combined>();
    }

    // ── Unit tests ────────────────────────────────────────────────────────────

    #[test]
    fn ready_result_fields() {
        let r = ReadyResult {
            sandbox_id: "sb-1".to_string(),
            timestamp_ms: 12345,
        };
        assert_eq!(r.sandbox_id, "sb-1");
        assert_eq!(r.timestamp_ms, 12345);
    }

    #[test]
    fn exit_status_fields() {
        let s = ExitStatus {
            exit_code: 137,
            exited_at_ms: 9999,
        };
        assert_eq!(s.exit_code, 137);
        assert_eq!(s.exited_at_ms, 9999);
    }

    #[test]
    fn container_stats_fields() {
        let s = ContainerStats {
            cpu_usage_ns: 1_000_000,
            memory_rss_bytes: 65536,
            pids_current: 3,
        };
        assert_eq!(s.cpu_usage_ns, 1_000_000);
        assert_eq!(s.memory_rss_bytes, 65536);
        assert_eq!(s.pids_current, 3);
    }

    #[test]
    fn mount_fields() {
        let m = Mount {
            kind: "bind".to_string(),
            source: "/host/path".to_string(),
            target: "/guest/path".to_string(),
            options: vec!["ro".to_string()],
        };
        assert_eq!(m.kind, "bind");
        assert_eq!(m.options.len(), 1);
    }

    #[test]
    fn network_interface_fields() {
        let iface = NetworkInterface {
            name: "eth0".to_string(),
            mac: "02:00:00:00:00:01".to_string(),
            ip_addresses: vec![],
            mtu: 1500,
        };
        assert_eq!(iface.mtu, 1500);
    }

    #[test]
    fn route_fields() {
        let r = Route {
            dest: "default".to_string(),
            gateway: "192.168.1.1".to_string(),
            device: "eth0".to_string(),
        };
        assert_eq!(r.dest, "default");
        assert_eq!(r.gateway, "192.168.1.1");
    }
}
