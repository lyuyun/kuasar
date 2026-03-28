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

//! `vmm-engine`: Unified VMM sandboxer entry point.
//!
//! Reads the TOML config file, selects the VMM backend based on `vmm_type`, and
//! starts the containerd Sandbox API server.

use std::path::Path;

use clap::Parser;
use serde::Deserialize;
use vmm_adapter_k8s::{K8sAdapter, K8sAdapterConfig};
use vmm_cloud_hypervisor::{CloudHypervisorHooks, CloudHypervisorVmmConfig};
use vmm_engine::{config::EngineConfig, SandboxEngine};
use vmm_qemu::{QemuHooks, QemuVmmConfig};
use vmm_runtime_vmm_task::{VmmTaskConfig, VmmTaskRuntime};
use vmm_stratovirt::{StratoVirtHooks, StratoVirtVmmConfig};

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "vmm-engine", about = "Kuasar VMM Engine")]
struct Args {
    /// Print version information and exit.
    #[arg(short = 'v', long)]
    version: bool,

    /// Path to the TOML config file.
    #[arg(
        short = 'c',
        long,
        value_name = "FILE",
        default_value = "/var/lib/kuasar/config.toml"
    )]
    config: String,

    /// Working directory for sandbox state files.
    #[arg(
        short = 'd',
        long,
        value_name = "DIR",
        default_value = "/run/kuasar-vmm"
    )]
    dir: String,

    /// Unix socket path for the containerd Sandbox API.
    #[arg(
        short = 'l',
        long,
        value_name = "FILE",
        default_value = "/run/vmm-sandboxer.sock"
    )]
    listen: String,

    /// Logging level [trace, debug, info, warn, error].
    /// If not set, falls back to the value in the config file (default: info).
    #[arg(long, value_name = "STRING")]
    log_level: Option<String>,
}

// ── Configuration ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct TopLevelConfig {
    pub engine: EngineSection,
    #[serde(rename = "cloud-hypervisor", default)]
    pub cloud_hypervisor: CloudHypervisorVmmConfig,
    #[serde(rename = "qemu", default)]
    pub qemu: QemuVmmConfig,
    #[serde(rename = "stratovirt", default)]
    pub stratovirt: StratoVirtVmmConfig,
    #[serde(rename = "vmm-task", default)]
    pub vmm_task: VmmTaskConfig,
    #[serde(default)]
    pub adapter: AdapterSection,
}

#[derive(Deserialize)]
pub struct EngineSection {
    /// Selects the VMM backend: "cloud-hypervisor" | "qemu" | "stratovirt".
    pub vmm_type: String,
    /// Runtime mode (reserved for future use): "standard".
    #[serde(default = "default_runtime_mode")]
    pub runtime_mode: String,
    /// Working directory for sandbox state files.
    /// If omitted, falls back to the `--dir` CLI argument.
    #[serde(default)]
    pub work_dir: String,
    #[serde(default)]
    pub log_level: String,
    #[serde(default = "default_ready_timeout")]
    pub ready_timeout_ms: u64,
    #[serde(default)]
    pub enable_tracing: bool,
}

fn default_runtime_mode() -> String {
    "standard".to_string()
}
fn default_ready_timeout() -> u64 {
    60_000
}

impl EngineSection {
    /// Convert into `EngineConfig`, using `fallback_dir` for `work_dir` when the
    /// TOML field is absent or empty.
    fn into_engine_config(self, fallback_dir: &str) -> EngineConfig {
        let work_dir = if self.work_dir.is_empty() {
            fallback_dir.to_string()
        } else {
            self.work_dir
        };
        EngineConfig {
            work_dir,
            ready_timeout_ms: self.ready_timeout_ms,
        }
    }
}

#[derive(Deserialize, Default)]
pub struct AdapterSection {
    #[serde(default)]
    pub k8s: K8sAdapterConfig,
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let args = Args::parse();

    if args.version {
        println!("vmm-engine {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    let config_str = match tokio::fs::read_to_string(&args.config).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: read config {}: {}", args.config, e);
            std::process::exit(1);
        }
    };
    let config: TopLevelConfig = match toml::from_str(&config_str) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: parse config {}: {}", args.config, e);
            std::process::exit(1);
        }
    };

    // CLI --log-level takes precedence over config; default to "info".
    let log_level = match args.log_level.as_deref() {
        Some(l) if !l.is_empty() => l.to_string(),
        _ => {
            if config.engine.log_level.is_empty() {
                "info".to_string()
            } else {
                config.engine.log_level.clone()
            }
        }
    };

    tracing_subscriber::fmt().with_env_filter(&log_level).init();

    tracing::info!(
        vmm_type = %config.engine.vmm_type,
        runtime_mode = %config.engine.runtime_mode,
        "vmm-engine starting"
    );

    // Notify systemd that we are ready to accept connections.
    if let Err(e) = sd_notify::notify(&[sd_notify::NotifyState::Ready]) {
        tracing::warn!("sd_notify ready failed: {}", e);
    }

    let vmm_type = config.engine.vmm_type.clone();
    let runtime_mode = config.engine.runtime_mode.clone();
    let engine_config = config.engine.into_engine_config(&args.dir);

    macro_rules! run_backend {
        ($vmm_cfg:expr, $runtime:expr, $hooks:expr) => {{
            let engine = SandboxEngine::new($vmm_cfg, $runtime, $hooks, engine_config);
            let adapter = K8sAdapter::new(engine, config.adapter.k8s);
            if Path::new(&args.dir).exists() {
                adapter.recover(&args.dir).await;
            }
            adapter
                .serve(&args.listen, &args.dir)
                .await
                .unwrap_or_else(|e| {
                    eprintln!("error: serve failed: {}", e);
                    std::process::exit(1);
                });
        }};
    }

    match (vmm_type.as_str(), runtime_mode.as_str()) {
        ("cloud-hypervisor", "standard") => run_backend!(
            config.cloud_hypervisor,
            VmmTaskRuntime::new(config.vmm_task),
            CloudHypervisorHooks
        ),
        ("qemu", "standard") => {
            run_backend!(config.qemu, VmmTaskRuntime::new(config.vmm_task), QemuHooks)
        }
        ("stratovirt", "standard") => run_backend!(
            config.stratovirt,
            VmmTaskRuntime::new(config.vmm_task),
            StratoVirtHooks
        ),
        (vmm, mode) => {
            eprintln!("error: unsupported vmm_type={vmm} runtime_mode={mode}");
            std::process::exit(1);
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use vmm_cloud_hypervisor::CloudHypervisorVmm;

    const CH_CONFIG: &str = r#"
[engine]
vmm_type = "cloud-hypervisor"
work_dir = "/run/kuasar/vmm"
ready_timeout_ms = 60000

[cloud-hypervisor]
binary = "/usr/local/bin/cloud-hypervisor"
vcpus = 2
memory_mb = 512
kernel_path = "/var/lib/kuasar/vmlinux"
image_path = "/var/lib/kuasar/rootfs.img"

[vmm-task]
connect_timeout_ms = 45000
ttrpc_timeout_ms = 10000
"#;

    const QEMU_CONFIG: &str = r#"
[engine]
vmm_type = "qemu"
work_dir = "/run/kuasar/vmm"
ready_timeout_ms = 60000

[qemu]
binary = "/usr/bin/qemu-system-x86_64"
vcpus = 2
memory_mb = 512
kernel_path = "/var/lib/kuasar/vmlinux"
image_path = "/var/lib/kuasar/rootfs.img"
"#;

    const SV_CONFIG: &str = r#"
[engine]
vmm_type = "stratovirt"
work_dir = "/run/kuasar/vmm"
ready_timeout_ms = 60000

[stratovirt]
binary = "/usr/bin/stratovirt"
vcpus = 1
memory_mb = 256
kernel_path = "/var/lib/kuasar/vmlinux"
image_path = "/var/lib/kuasar/rootfs.img"
"#;

    // Config without work_dir — should fall back to --dir.
    const NO_WORK_DIR_CONFIG: &str = r#"
[engine]
vmm_type = "cloud-hypervisor"
ready_timeout_ms = 30000

[cloud-hypervisor]
binary = "/usr/local/bin/cloud-hypervisor"
vcpus = 1
memory_mb = 512
kernel_path = "/var/lib/kuasar/vmlinux"
image_path = "/var/lib/kuasar/rootfs.img"
"#;

    #[test]
    fn config_parse_cloud_hypervisor() {
        let config: TopLevelConfig = toml::from_str(CH_CONFIG).unwrap();
        assert_eq!(config.engine.vmm_type, "cloud-hypervisor");
        assert_eq!(config.cloud_hypervisor.vcpus, 2);
        assert_eq!(config.vmm_task.connect_timeout_ms, 45000);
    }

    #[test]
    fn config_parse_qemu() {
        let config: TopLevelConfig = toml::from_str(QEMU_CONFIG).unwrap();
        assert_eq!(config.engine.vmm_type, "qemu");
        assert_eq!(config.qemu.binary, "/usr/bin/qemu-system-x86_64");
    }

    #[test]
    fn config_parse_stratovirt() {
        let config: TopLevelConfig = toml::from_str(SV_CONFIG).unwrap();
        assert_eq!(config.engine.vmm_type, "stratovirt");
        assert_eq!(config.stratovirt.memory_mb, 256);
    }

    #[test]
    fn k8s_adapter_new_constructs() {
        use vmm_engine::SandboxEngine;
        use vmm_vm_trait::NoopHooks;

        let engine = SandboxEngine::<CloudHypervisorVmm, _, _>::new(
            CloudHypervisorVmmConfig::default(),
            VmmTaskRuntime::new(VmmTaskConfig::default()),
            NoopHooks::default(),
            EngineConfig {
                work_dir: "/tmp".to_string(),
                ready_timeout_ms: 5000,
            },
        );
        let adapter = K8sAdapter::new(engine, K8sAdapterConfig::default());
        // Just verify construction doesn't panic.
        let _ = adapter;
    }

    #[test]
    fn engine_section_work_dir_falls_back_to_dir_arg() {
        let config: TopLevelConfig = toml::from_str(NO_WORK_DIR_CONFIG).unwrap();
        assert!(config.engine.work_dir.is_empty());
        let engine_cfg = config.engine.into_engine_config("/run/fallback");
        assert_eq!(engine_cfg.work_dir, "/run/fallback");
    }

    #[test]
    fn engine_section_explicit_work_dir_wins() {
        let config: TopLevelConfig = toml::from_str(CH_CONFIG).unwrap();
        let engine_cfg = config.engine.into_engine_config("/run/fallback");
        assert_eq!(engine_cfg.work_dir, "/run/kuasar/vmm");
        assert_eq!(engine_cfg.ready_timeout_ms, 60_000);
    }

    #[test]
    fn log_level_defaults_to_info_when_both_empty() {
        let config: TopLevelConfig = toml::from_str(NO_WORK_DIR_CONFIG).unwrap();
        assert!(config.engine.log_level.is_empty());
        // Simulate the main() log_level selection logic.
        let cli_level: Option<&str> = None;
        let log_level = match cli_level {
            Some(l) if !l.is_empty() => l.to_string(),
            _ => {
                if config.engine.log_level.is_empty() {
                    "info".to_string()
                } else {
                    config.engine.log_level.clone()
                }
            }
        };
        assert_eq!(log_level, "info");
    }

    #[test]
    fn log_level_cli_overrides_config() {
        let config: TopLevelConfig = toml::from_str(CH_CONFIG).unwrap();
        let cli_level: Option<&str> = Some("debug");
        let log_level = match cli_level {
            Some(l) if !l.is_empty() => l.to_string(),
            _ => {
                if config.engine.log_level.is_empty() {
                    "info".to_string()
                } else {
                    config.engine.log_level.clone()
                }
            }
        };
        assert_eq!(log_level, "debug");
    }
}
