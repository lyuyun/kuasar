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

//! VmmTaskRuntime: ttrpc over vsock to `vmm-task`.

use std::sync::Arc;

use async_trait::async_trait;
use containerd_sandbox::{data::SandboxData, error::Result, signal::ExitSignal};
use protobuf::MessageField;
use vmm_common::api::{
    empty::Empty, sandbox::SetupSandboxRequest, sandbox_ttrpc::SandboxServiceClient,
};

use super::GuestRuntime;
use crate::{
    client::{
        client_check, client_setup_sandbox, client_sync_clock, new_sandbox_client, publish_event,
    },
    network::Network,
};

/// After a successful [`wait_ready`] call the runtime holds the connected
/// `SandboxServiceClient`; subsequent `start_sync_clock` / `start_forward_events`
/// calls use that client.
pub struct VmmTaskRuntime {
    pub(super) client: Option<SandboxServiceClient>,
}

impl VmmTaskRuntime {
    pub fn new() -> Self {
        Self { client: None }
    }
}

impl Default for VmmTaskRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl GuestRuntime for VmmTaskRuntime {
    async fn wait_ready(
        &mut self,
        address: &str,
        network: Option<&Network>,
        data: &SandboxData,
    ) -> Result<()> {
        // Connect to vmm-task's ttrpc server (retries internally for up to 45 s).
        let client = new_sandbox_client(address).await?;
        // Poll vmm-task::check() until the agent is ready (up to 45 s).
        client_check(&client).await?;
        // Send network configuration to the guest agent.
        let req = build_setup_sandbox_request(network, data)?;
        client_setup_sandbox(&client, &req).await?;
        self.client = Some(client);
        Ok(())
    }

    async fn reconnect(&mut self, address: &str) -> Result<()> {
        // Re-establish the ttrpc connection to the already-running vmm-task.
        let client = new_sandbox_client(address).await?;
        // Confirm the agent is still alive (does not reconfigure the guest).
        client_check(&client).await?;
        self.client = Some(client);
        Ok(())
    }

    fn start_sync_clock(&self, id: &str, exit_signal: Arc<ExitSignal>) {
        if let Some(client) = &self.client {
            client_sync_clock(client, id, exit_signal);
        } else {
            debug_assert!(false, "start_sync_clock called before wait_ready");
            log::warn!("start_sync_clock: ttrpc client not yet initialised — call ignored");
        }
    }

    fn start_forward_events(&self, exit_signal: Arc<ExitSignal>) {
        if let Some(client) = &self.client {
            let client = client.clone();
            use ttrpc::context::with_timeout;
            tokio::spawn(async move {
                let fut = async {
                    loop {
                        match client.get_events(with_timeout(0), &Empty::new()).await {
                            Ok(resp) => {
                                if let Err(e) = publish_event(convert_envelope(resp)).await {
                                    log::error!("publish guest event: {}", e);
                                }
                            }
                            Err(err) => {
                                if let ttrpc::error::Error::Socket(s) = &err {
                                    if s.contains("early eof") {
                                        break;
                                    }
                                }
                                log::error!("get guest events error: {:?}", err);
                                break;
                            }
                        }
                    }
                };
                tokio::select! {
                    _ = fut => {},
                    _ = exit_signal.wait() => {},
                }
            });
        } else {
            debug_assert!(false, "start_forward_events called before wait_ready");
            log::warn!("start_forward_events: ttrpc client not yet initialised — call ignored");
        }
    }
}

fn build_setup_sandbox_request(
    network: Option<&Network>,
    data: &SandboxData,
) -> containerd_sandbox::error::Result<SetupSandboxRequest> {
    use anyhow::anyhow;
    use containerd_sandbox::error::Error;
    use protobuf::well_known_types::any::Any;

    let mut req = SetupSandboxRequest::new();

    if let Some(config) = &data.config {
        let config_str = serde_json::to_vec(config)
            .map_err(|e| Error::Other(anyhow!("marshal PodSandboxConfig: {:?}", e)))?;
        let mut any = Any::new();
        any.type_url = "PodSandboxConfig".to_string();
        any.value = config_str;
        req.config = MessageField::some(any);
    }

    if let Some(network) = network {
        req.interfaces = network.interfaces().iter().map(|x| x.into()).collect();
        req.routes = network.routes().iter().map(|x| x.into()).collect();
    }

    Ok(req)
}

fn convert_envelope(
    envelope: vmm_common::api::events::Envelope,
) -> containerd_shim::protos::api::Envelope {
    containerd_shim::protos::api::Envelope {
        timestamp: envelope.timestamp,
        namespace: envelope.namespace,
        topic: envelope.topic,
        event: envelope.event,
        special_fields: protobuf::SpecialFields::default(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        time::{SystemTime, UNIX_EPOCH},
    };

    use containerd_sandbox::data::SandboxData;
    use vmm_common::api::sandbox_ttrpc::create_sandbox_service;

    use super::*;

    /// A minimal in-process ttrpc sandbox service that:
    /// - responds to `check()` with Ok (agent alive),
    /// - responds to `setup_sandbox()` with Ok and records the call.
    struct MockSandboxService {
        setup_sandbox_called: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl vmm_common::api::sandbox_ttrpc::SandboxService for MockSandboxService {
        async fn check(
            &self,
            _ctx: &ttrpc::r#async::TtrpcContext,
            _req: vmm_common::api::sandbox::CheckRequest,
        ) -> ttrpc::Result<vmm_common::api::empty::Empty> {
            Ok(vmm_common::api::empty::Empty::new())
        }

        async fn setup_sandbox(
            &self,
            _ctx: &ttrpc::r#async::TtrpcContext,
            _req: vmm_common::api::sandbox::SetupSandboxRequest,
        ) -> ttrpc::Result<vmm_common::api::empty::Empty> {
            self.setup_sandbox_called.store(true, Ordering::SeqCst);
            Ok(vmm_common::api::empty::Empty::new())
        }
    }

    fn unique_socket_path() -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        format!("/tmp/kuasar-test-{}.sock", nanos)
    }

    async fn start_mock_server(
        socket_path: &str,
        setup_sandbox_called: Arc<AtomicBool>,
    ) -> ttrpc::r#async::Server {
        let svc = MockSandboxService {
            setup_sandbox_called,
        };
        let services = create_sandbox_service(Arc::new(Box::new(svc)));
        let mut server = ttrpc::r#async::Server::new()
            .bind(&format!("unix://{}", socket_path))
            .expect("bind mock server socket")
            .register_service(services);
        server.start().await.expect("start mock server");
        server
    }

    #[test]
    fn test_vmm_task_runtime_new_has_no_client() {
        let rt = VmmTaskRuntime::new();
        assert!(rt.client.is_none());
    }

    /// `reconnect` must re-establish the ttrpc channel and verify agent liveness,
    /// but must NOT send `SetupSandboxRequest` (which would reconfigure an already
    /// running guest network and likely fail with duplicate-interface errors).
    #[tokio::test]
    async fn test_reconnect_does_not_call_setup_sandbox() {
        let socket_path = unique_socket_path();
        let setup_called = Arc::new(AtomicBool::new(false));
        let mut server = start_mock_server(&socket_path, setup_called.clone()).await;

        let address = format!("unix://{}", socket_path);
        let mut runtime = VmmTaskRuntime::new();
        runtime
            .reconnect(&address)
            .await
            .expect("reconnect should succeed against a live server");

        assert!(
            !setup_called.load(Ordering::SeqCst),
            "reconnect must not call setup_sandbox"
        );
        assert!(
            runtime.client.is_some(),
            "reconnect should populate the ttrpc client"
        );

        server.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&socket_path);
    }

    /// `wait_ready` must call `setup_sandbox` to send the guest its network
    /// configuration during the initial boot path.
    #[tokio::test]
    async fn test_wait_ready_calls_setup_sandbox() {
        let socket_path = unique_socket_path();
        let setup_called = Arc::new(AtomicBool::new(false));
        let mut server = start_mock_server(&socket_path, setup_called.clone()).await;

        let address = format!("unix://{}", socket_path);
        let data = SandboxData::default();
        let mut runtime = VmmTaskRuntime::new();
        runtime
            .wait_ready(&address, None, &data)
            .await
            .expect("wait_ready should succeed against a live server");

        assert!(
            setup_called.load(Ordering::SeqCst),
            "wait_ready must call setup_sandbox to configure the guest network"
        );
        assert!(
            runtime.client.is_some(),
            "wait_ready should populate the ttrpc client"
        );

        server.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&socket_path);
    }
}
