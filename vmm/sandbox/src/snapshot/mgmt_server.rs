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

//! Ttrpc management server for snapshot operations on running sandboxes.
//!
//! The server listens on a dedicated unix socket (default
//! `/run/kuasar-vmm/snapshot-mgmt.sock`) separate from the containerd sandbox
//! controller socket and the Prometheus metrics server.
//!
//! `kuasar-ctl snapshot create` connects to this socket and calls
//! `CreateSnapshot` with the sandbox ID and key name.

use std::{collections::HashMap, path::Path, sync::Arc};

use anyhow::anyhow;
use async_trait::async_trait;
use log::{error, info};
use serde::{de::DeserializeOwned, Serialize};
use tokio::sync::{Mutex, RwLock};
use ttrpc::r#async::{Server, TtrpcContext};
use vmm_common::api::{
    snapshot_mgmt::{CreateSnapshotRequest, CreateSnapshotResponse},
    snapshot_mgmt_ttrpc::{create_snapshot_mgmt_service, SnapshotMgmtService},
};

use crate::{
    sandbox::KuasarSandbox,
    snapshot::store::{SnapshotKey, SnapshotStore},
    vm::VM,
};

pub const DEFAULT_SNAPSHOT_MGMT_SOCK: &str = "/run/kuasar-vmm/snapshot-mgmt.sock";
const MGMT_SOCK_ENV: &str = "KUASAR_SNAPSHOT_MGMT_SOCK";

/// Start the snapshot management ttrpc server in a background task.
///
/// `sandboxes` is the shared sandbox map from `KuasarSandboxer`.  The server
/// runs until the process exits.
pub fn start<V>(
    sandboxes: Arc<RwLock<HashMap<String, Arc<Mutex<KuasarSandbox<V>>>>>>,
    store: SnapshotStore,
) where
    V: VM + Serialize + DeserializeOwned + Sync + Send + 'static,
{
    let sock_path = std::env::var(MGMT_SOCK_ENV)
        .unwrap_or_else(|_| DEFAULT_SNAPSHOT_MGMT_SOCK.to_string());

    if sock_path.is_empty() {
        info!("snapshot mgmt server disabled ({} is empty)", MGMT_SOCK_ENV);
        return;
    }

    tokio::spawn(async move {
        if let Err(e) = run_server(sock_path, sandboxes, store).await {
            error!("snapshot mgmt server exited with error: {}", e);
        }
    });
}

async fn run_server<V>(
    sock_path: String,
    sandboxes: Arc<RwLock<HashMap<String, Arc<Mutex<KuasarSandbox<V>>>>>>,
    store: SnapshotStore,
) -> anyhow::Result<()>
where
    V: VM + Serialize + DeserializeOwned + Sync + Send + 'static,
{
    if Path::new(&sock_path).exists() {
        tokio::fs::remove_file(&sock_path).await?;
    }
    if let Some(parent) = Path::new(&sock_path).parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let svc = Arc::new(Box::new(SnapshotMgmtServiceImpl { sandboxes, store })
        as Box<dyn SnapshotMgmtService + Send + Sync>);
    let methods = create_snapshot_mgmt_service(svc);

    let mut server = Server::new()
        .bind(&format!("unix://{}", sock_path))
        .map_err(|e| anyhow!("snapshot mgmt bind {}: {}", sock_path, e))?
        .register_service(methods);

    server
        .start()
        .await
        .map_err(|e| anyhow!("snapshot mgmt server start: {}", e))?;

    info!("snapshot mgmt server listening on unix://{}", sock_path);

    // Keep the server alive until the process exits.
    std::future::pending::<()>().await;
    Ok(())
}

struct SnapshotMgmtServiceImpl<V: VM + Sync + Send> {
    sandboxes: Arc<RwLock<HashMap<String, Arc<Mutex<KuasarSandbox<V>>>>>>,
    store: SnapshotStore,
}

#[async_trait]
impl<V> SnapshotMgmtService for SnapshotMgmtServiceImpl<V>
where
    V: VM + Serialize + DeserializeOwned + Sync + Send + 'static,
{
    async fn create_snapshot(
        &self,
        _ctx: &TtrpcContext,
        req: CreateSnapshotRequest,
    ) -> ttrpc::Result<CreateSnapshotResponse> {
        match self.do_create_snapshot(&req.sandbox_id, &req.key).await {
            Ok(resp) => Ok(resp),
            Err(e) => Err(ttrpc::Error::RpcStatus(ttrpc::get_status(
                ttrpc::Code::INTERNAL,
                format!("create_snapshot: {}", e),
            ))),
        }
    }
}

impl<V> SnapshotMgmtServiceImpl<V>
where
    V: VM + Serialize + DeserializeOwned + Sync + Send + 'static,
{
    async fn do_create_snapshot(
        &self,
        sandbox_id: &str,
        key: &str,
    ) -> anyhow::Result<CreateSnapshotResponse> {
        let sb_mutex = {
            let map = self.sandboxes.read().await;
            map.get(sandbox_id)
                .ok_or_else(|| anyhow!("sandbox '{}' not found", sandbox_id))?
                .clone()
        };

        self.store.init().await.map_err(|e| anyhow!("{}", e))?;
        let staging_dir = self
            .store
            .root()
            .join(format!("staging-{}", sandbox_id));
        tokio::fs::create_dir_all(&staging_dir).await?;
        let staging_url = format!("file://{}", staging_dir.display());

        info!(
            "create_snapshot: sandbox={} key={} staging={}",
            sandbox_id, key, staging_url
        );

        // Timing race fix: snapshot_live pauses vmm-task while it may have
        // in-progress ttrpc response writes on the host<->guest vsock connection.
        // When the snapshot is later restored, vmm-task resumes those writes on
        // a now-orphaned vsock fd (the new CHv instance has no matching
        // connection), producing "Broken pipe" errors.
        //
        // Mitigation: drop the host-side SandboxServiceClient *before* the
        // pause so that vmm-task's writer tasks see EOF and flush/close cleanly.
        // The VM is still paused by snapshot_live's internal pause call; the
        // brief window between our drop and that pause is safe because the ttrpc
        // server only closes connections, it does not crash.
        //
        // Note: sync_clock holds a clone of the client.  Dropping the sandbox's
        // reference may not immediately close the underlying socket if the clone
        // is still alive, but the sync_clock task sleeps for 60 s between
        // syncs, so in practice the socket is idle and the snapshot will capture
        // vmm-task without an active write in progress.
        {
            let sb = sb_mutex.lock().await;
            let mut guard = sb.client.lock().await;
            *guard = None;
            // Release guard so the drop of SandboxServiceClient propagates.
        }
        // Give the ttrpc connection teardown a moment to reach vmm-task before
        // the snapshot freeze.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        {
            let sb = sb_mutex.lock().await;
            sb.vm
                .snapshot_live(&staging_url)
                .await
                .map_err(|e| anyhow!("snapshot_live: {}", e))?;
        }

        let snap_key = SnapshotKey(key.to_string());
        let meta = self
            .store
            .import_from(&staging_dir, &snap_key, "cloud-hypervisor")
            .await
            .map_err(|e| anyhow!("import snapshot: {}", e))?;

        info!(
            "create_snapshot done: id={} key={} memory={}B state={}B",
            meta.id, meta.key, meta.memory_size_bytes, meta.state_size_bytes
        );

        let mut resp = CreateSnapshotResponse::new();
        resp.snapshot_id = meta.id.0;
        resp.key = meta.key.0;
        resp.memory_size_bytes = meta.memory_size_bytes;
        resp.state_size_bytes = meta.state_size_bytes;
        Ok(resp)
    }
}
