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

use anyhow::Result;
use async_trait::async_trait;
use containerd_sandbox::data::SandboxData;

use crate::vmm::Vmm;

/// Lightweight context passed to every hook call.
/// Defined here (not in vmm/engine) so backend crates can implement Hooks<V>
/// without depending on the engine crate.
pub struct SandboxCtx<'a, V: Vmm> {
    pub vmm: &'a mut V,
    pub data: &'a mut SandboxData,
    pub base_dir: &'a str,
}

/// Backend lifecycle customisation hooks.
///
/// Hooks receive a `SandboxCtx<V>` — a lightweight view of the sandbox that
/// exposes only the VMM instance, sandbox data, and base directory. This keeps
/// backend crates independent of the engine's full `SandboxInstance` type.
///
/// All methods have no-op defaults so backends only implement what they need.
#[async_trait]
pub trait Hooks<V: Vmm>: Send + Sync + 'static {
    /// Called in create_sandbox after SandboxInstance is constructed.
    /// Can do backend-specific setup that requires the sandbox directory to exist.
    /// Default: no-op.
    async fn post_create(&self, _ctx: &mut SandboxCtx<'_, V>) -> Result<()> {
        Ok(())
    }

    /// Called in start_sandbox before add_disk / add_network / boot.
    /// Typical use: read pod resource limits from ctx.data, apply to ctx.vmm
    /// via backend-specific methods (not on Vmm trait).
    /// Default: no-op.
    async fn pre_start(&self, _ctx: &mut SandboxCtx<'_, V>) -> Result<()> {
        Ok(())
    }

    /// Called in start_sandbox after wait_ready() succeeds and before state → Running.
    /// Typical use: set ctx.data.task_address, publish metrics.
    /// Default: sets task_address = ctx.vmm.task_address().
    async fn post_start(&self, ctx: &mut SandboxCtx<'_, V>) -> Result<()> {
        ctx.data.task_address = ctx.vmm.task_address();
        Ok(())
    }

    /// Called in stop_sandbox before vmm.stop(), only when force=false.
    /// Typical use: graceful shutdown notification, save snapshot (Firecracker).
    /// Default: no-op.
    async fn pre_stop(&self, _ctx: &mut SandboxCtx<'_, V>) -> Result<()> {
        Ok(())
    }

    /// Called in stop_sandbox after vmm.stop() completes.
    /// Typical use: clean up backend-specific resources (sockets, host devices).
    /// Default: no-op.
    async fn post_stop(&self, _ctx: &mut SandboxCtx<'_, V>) -> Result<()> {
        Ok(())
    }
}

/// No-op implementation for use in tests and as a default when no customisation is needed.
pub struct NoopHooks<V>(std::marker::PhantomData<V>);

impl<V: Vmm> Default for NoopHooks<V> {
    fn default() -> Self {
        Self(std::marker::PhantomData)
    }
}

#[async_trait]
impl<V: Vmm> Hooks<V> for NoopHooks<V> {}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use anyhow::Result;
    use async_trait::async_trait;
    use containerd_sandbox::data::SandboxData;
    use tokio::sync::watch;

    use super::*;
    use crate::vmm::{
        DiskConfig, ExitInfo, HotPlugDevice, HotPlugResult, Pids, VcpuThreads, VmmCapabilities,
        VmmNetworkConfig,
    };

    // ── MockVmm ──────────────────────────────────────────────────────────────

    #[derive(Default, Clone)]
    struct MockVmmConfig;

    impl<'de> serde::Deserialize<'de> for MockVmmConfig {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
            // Accept any map/unit
            serde::de::IgnoredAny::deserialize(d)?;
            Ok(MockVmmConfig)
        }
    }

    struct MockVmm {
        vsock_path_value: String,
        // exit_tx kept alive so the receiver never sees a closed channel
        _exit_tx: watch::Sender<Option<ExitInfo>>,
        exit_rx: watch::Receiver<Option<ExitInfo>>,
    }

    impl MockVmm {
        fn new(vsock_path: &str) -> Self {
            let (tx, rx) = watch::channel(None);
            Self {
                vsock_path_value: vsock_path.to_string(),
                _exit_tx: tx,
                exit_rx: rx,
            }
        }
    }

    #[async_trait]
    impl Vmm for MockVmm {
        type Config = MockVmmConfig;

        async fn create(
            _id: &str,
            _base_dir: &str,
            _config: &Self::Config,
            _vsock_cid: u32,
        ) -> Result<Self> {
            Ok(MockVmm::new("mock://test"))
        }

        async fn boot(&mut self) -> Result<()> {
            Ok(())
        }

        async fn stop(&mut self, _force: bool) -> Result<()> {
            Ok(())
        }

        fn subscribe_exit(&self) -> watch::Receiver<Option<ExitInfo>> {
            self.exit_rx.clone()
        }

        async fn recover(&mut self) -> Result<()> {
            Ok(())
        }

        fn add_disk(&mut self, _disk: DiskConfig) -> Result<()> {
            Ok(())
        }

        fn add_network(&mut self, _net: VmmNetworkConfig) -> Result<()> {
            Ok(())
        }

        async fn hot_attach(&mut self, device: HotPlugDevice) -> Result<HotPlugResult> {
            let id = match &device {
                HotPlugDevice::VirtioBlock { id, .. } => id.clone(),
                HotPlugDevice::VirtioFs { id, .. } => id.clone(),
                HotPlugDevice::CharDevice { id, .. } => id.clone(),
                HotPlugDevice::VsockMuxIO { id, .. } => id.clone(),
            };
            Ok(HotPlugResult {
                device_id: id,
                bus_addr: "0000:00:01.0".to_string(),
            })
        }

        async fn hot_detach(&mut self, _id: &str) -> Result<()> {
            Ok(())
        }

        async fn ping(&self) -> Result<()> {
            Ok(())
        }

        async fn vcpus(&self) -> Result<VcpuThreads> {
            Ok(VcpuThreads {
                vcpus: HashMap::new(),
            })
        }

        fn pids(&self) -> Pids {
            Pids {
                vmm_pid: Some(1234),
                affiliated_pids: vec![],
            }
        }

        fn vsock_path(&self) -> Result<String> {
            Ok(self.vsock_path_value.clone())
        }

        // Override task_address to count calls
        fn task_address(&self) -> String {
            // Note: can't mutate self here since the trait takes &self;
            // we count indirectly by checking the returned value in tests.
            format!("ttrpc+{}", self.vsock_path_value)
        }

        fn capabilities(&self) -> VmmCapabilities {
            VmmCapabilities {
                virtio_serial: true,
                ..Default::default()
            }
        }
    }

    // ── SandboxCtx tests ─────────────────────────────────────────────────────

    #[test]
    fn sandbox_ctx_construction_and_field_access() {
        let mut vmm = MockVmm::new("mock://test");
        let mut data = SandboxData::default();
        let base_dir = "/run/kuasar/sandbox-1";

        let ctx = SandboxCtx {
            vmm: &mut vmm,
            data: &mut data,
            base_dir,
        };

        assert_eq!(ctx.base_dir, base_dir);
        assert_eq!(ctx.data.task_address, "");
    }

    #[test]
    fn sandbox_ctx_data_mutability() {
        let mut vmm = MockVmm::new("mock://test");
        let mut data = SandboxData::default();

        {
            let ctx = SandboxCtx {
                vmm: &mut vmm,
                data: &mut data,
                base_dir: "/tmp/sb",
            };
            ctx.data.task_address = "ttrpc+test://addr".to_string();
        }

        assert_eq!(data.task_address, "ttrpc+test://addr");
    }

    // ── NoopHooks tests ───────────────────────────────────────────────────────

    // Compile-time check: NoopHooks<MockVmm> implements Hooks<MockVmm>
    fn _assert_noop_hooks_impl(_: &dyn Hooks<MockVmm>) {}
    fn _check() {
        _assert_noop_hooks_impl(&NoopHooks::<MockVmm>::default());
    }

    #[tokio::test]
    async fn noop_hooks_default_constructs() {
        let _hooks: NoopHooks<MockVmm> = NoopHooks::default();
    }

    #[tokio::test]
    async fn noop_hooks_post_create_is_noop() {
        let hooks = NoopHooks::<MockVmm>::default();
        let mut vmm = MockVmm::new("mock://test");
        let mut data = SandboxData::default();
        let mut ctx = SandboxCtx {
            vmm: &mut vmm,
            data: &mut data,
            base_dir: "/tmp",
        };
        assert!(hooks.post_create(&mut ctx).await.is_ok());
        assert_eq!(ctx.data.task_address, ""); // unchanged
    }

    #[tokio::test]
    async fn noop_hooks_pre_start_is_noop() {
        let hooks = NoopHooks::<MockVmm>::default();
        let mut vmm = MockVmm::new("mock://test");
        let mut data = SandboxData::default();
        let mut ctx = SandboxCtx {
            vmm: &mut vmm,
            data: &mut data,
            base_dir: "/tmp",
        };
        assert!(hooks.pre_start(&mut ctx).await.is_ok());
        assert_eq!(ctx.data.task_address, "");
    }

    #[tokio::test]
    async fn noop_hooks_pre_stop_is_noop() {
        let hooks = NoopHooks::<MockVmm>::default();
        let mut vmm = MockVmm::new("mock://test");
        let mut data = SandboxData::default();
        let mut ctx = SandboxCtx {
            vmm: &mut vmm,
            data: &mut data,
            base_dir: "/tmp",
        };
        assert!(hooks.pre_stop(&mut ctx).await.is_ok());
        assert_eq!(ctx.data.task_address, "");
    }

    #[tokio::test]
    async fn noop_hooks_post_stop_is_noop() {
        let hooks = NoopHooks::<MockVmm>::default();
        let mut vmm = MockVmm::new("mock://test");
        let mut data = SandboxData::default();
        let mut ctx = SandboxCtx {
            vmm: &mut vmm,
            data: &mut data,
            base_dir: "/tmp",
        };
        assert!(hooks.post_stop(&mut ctx).await.is_ok());
        assert_eq!(ctx.data.task_address, "");
    }

    #[tokio::test]
    async fn noop_hooks_post_start_sets_task_address() {
        // post_start has a non-trivial default: sets task_address = vmm.task_address()
        let hooks = NoopHooks::<MockVmm>::default();
        let expected = "ttrpc+mock://test";
        let mut vmm = MockVmm::new("mock://test");
        let mut data = SandboxData::default();
        let mut ctx = SandboxCtx {
            vmm: &mut vmm,
            data: &mut data,
            base_dir: "/tmp",
        };
        assert!(hooks.post_start(&mut ctx).await.is_ok());
        assert_eq!(ctx.data.task_address, expected);
    }

    // ── VmmCapabilities default test ──────────────────────────────────────────

    #[test]
    fn vmm_capabilities_default_all_false() {
        let caps = VmmCapabilities::default();
        assert!(!caps.hot_plug_disk);
        assert!(!caps.hot_plug_net);
        assert!(!caps.hot_plug_cpu);
        assert!(!caps.hot_plug_mem);
        assert!(!caps.pmem_dax);
        assert!(!caps.vfio);
        assert!(!caps.resize);
        assert!(!caps.virtiofs);
        assert!(!caps.virtio_serial);
        assert!(!caps.snapshot_restore);
    }

    #[test]
    fn vmm_capabilities_explicit_construction() {
        let caps = VmmCapabilities {
            hot_plug_disk: true,
            virtiofs: true,
            virtio_serial: true,
            ..Default::default()
        };
        assert!(caps.hot_plug_disk);
        assert!(caps.virtiofs);
        assert!(caps.virtio_serial);
        assert!(!caps.snapshot_restore);
    }
}
