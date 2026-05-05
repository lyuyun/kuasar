/*
Copyright 2024 The Kuasar Authors.

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

//! Admin Unix-socket API for template management.
//!
//! Protocol: one JSON object per line, one request/response per connection.
//!
//! Request:
//!   `{"action":"from-sandbox","sandbox_id":"<id>","template_id":"<id>"}`
//!   `{"action":"from-image","image":"<ref>","template_id":"<id>"}` (image may be "")
//!
//! Response:
//!   `{"ok":true,"template_id":"<id>"}`
//!   `{"ok":false,"error":"<message>"}`

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use anyhow::anyhow;
use containerd_sandbox::error::Result;
use log::{error, info, warn};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{Mutex, RwLock},
};

use crate::{
    sandbox::{create_template_from_image_worker, create_template_worker, KuasarSandbox},
    template::{CreateTemplateRequest, PooledTemplate, TemplateKey, TemplatePool},
    vm::{DiskSnapshot, Snapshottable, VMFactory, VM},
};

/// Shared state extracted from `KuasarSandboxer` for the admin server.
/// All fields are cheaply cloneable (`Arc` / `Clone`).
pub struct TemplateAdminHandle<F>
where
    F: VMFactory + Clone + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
{
    pub factory: F,
    pub sandboxes: Arc<RwLock<HashMap<String, Arc<Mutex<KuasarSandbox<F::VM>>>>>>,
    pub pool: Arc<TemplatePool>,
}

/// Listens on a Unix socket and dispatches template-management commands.
pub struct TemplateAdminServer<F>
where
    F: VMFactory + Clone + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
{
    handle: Arc<TemplateAdminHandle<F>>,
    sock_path: PathBuf,
}

impl<F> TemplateAdminServer<F>
where
    F: VMFactory + Clone + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
{
    pub fn new(handle: TemplateAdminHandle<F>, sock_path: impl Into<PathBuf>) -> Self {
        Self {
            handle: Arc::new(handle),
            sock_path: sock_path.into(),
        }
    }

    /// Run the admin server, accepting connections until the process exits.
    pub async fn serve(self) {
        // Remove stale socket from a previous run.
        let _ = tokio::fs::remove_file(&self.sock_path).await;

        let listener = match UnixListener::bind(&self.sock_path) {
            Ok(l) => l,
            Err(e) => {
                error!("admin: failed to bind {:?}: {}", self.sock_path, e);
                return;
            }
        };
        info!("admin: listening on {:?}", self.sock_path);

        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let handle = self.handle.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(handle, stream).await {
                            warn!("admin: connection error: {}", e);
                        }
                    });
                }
                Err(e) => {
                    error!("admin: accept error: {}", e);
                }
            }
        }
    }
}

async fn handle_connection<F>(
    handle: Arc<TemplateAdminHandle<F>>,
    stream: UnixStream,
) -> anyhow::Result<()>
where
    F: VMFactory + Clone + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
{
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let line = lines
        .next_line()
        .await?
        .ok_or_else(|| anyhow!("empty request"))?;

    let req: Value = serde_json::from_str(&line)?;
    let action = req["action"].as_str().unwrap_or("").to_string();

    let response = match action.as_str() {
        "from-sandbox" => {
            let sandbox_id = req["sandbox_id"]
                .as_str()
                .ok_or_else(|| anyhow!("missing sandbox_id"))?
                .to_string();
            let template_id = req["template_id"]
                .as_str()
                .ok_or_else(|| anyhow!("missing template_id"))?
                .to_string();
            match create_from_sandbox(&handle, &sandbox_id, &template_id).await {
                Ok(tmpl) => json!({"ok": true, "template_id": tmpl.id}),
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            }
        }
        "from-image" => {
            let image = req["image"].as_str().unwrap_or("").to_string();
            let template_id = req["template_id"]
                .as_str()
                .ok_or_else(|| anyhow!("missing template_id"))?
                .to_string();
            match create_from_image(&handle, &image, &template_id).await {
                Ok(tmpl) => json!({"ok": true, "template_id": tmpl.id}),
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            }
        }
        other => json!({"ok": false, "error": format!("unknown action: {}", other)}),
    };

    let mut out = serde_json::to_string(&response)?;
    out.push('\n');
    writer.write_all(out.as_bytes()).await?;
    Ok(())
}

/// Snapshot a running sandbox's VM and add the result to the template pool.
///
/// The sandbox's VM is paused, snapshotted, and immediately resumed so the
/// original container continues running.
async fn create_from_sandbox<F>(
    handle: &TemplateAdminHandle<F>,
    sandbox_id: &str,
    template_id: &str,
) -> Result<PooledTemplate>
where
    F: VMFactory + Clone + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
{
    let sandbox_mutex = handle
        .sandboxes
        .read()
        .await
        .get(sandbox_id)
        .ok_or_else(|| anyhow!("sandbox {} not found", sandbox_id))?
        .clone();

    let mut sandbox = sandbox_mutex.lock().await;

    let snapshot_dir = handle.pool.store_dir.join(template_id).join("snapshot");
    tokio::fs::create_dir_all(&snapshot_dir)
        .await
        .map_err(|e| anyhow!("create snapshot dir: {}", e))?;

    // Capture id_generator before snapshot so restored sandboxes start their device ID
    // counter above any IDs already embedded in the VM state (avoids IdentifierNotUnique).
    let source_id_generator = sandbox.id_generator;

    // Collect ext4 disk images that are hot-attached as virtio-blk devices.
    // These are copied into the snapshot directory while the VM is paused (inside snapshot()).
    let disks: Vec<DiskSnapshot> = sandbox
        .storages
        .iter()
        .filter(|s| s.fstype == "ext4")
        .filter_map(|s| {
            s.device_id.as_ref().map(|did| DiskSnapshot {
                storage_id: s.id.clone(),
                device_id: did.clone(),
                img_path: format!("{}/{}.img", sandbox.base_dir, s.id),
            })
        })
        .collect();

    // vm.snapshot() handles pause → disk copy → CH snapshot → resume internally.
    let meta = sandbox
        .vm
        .snapshot(&snapshot_dir, &disks)
        .await
        .map_err(|e| anyhow!("snapshot sandbox {}: {}", sandbox_id, e))?;

    let key = TemplateKey::new(
        handle.factory.image_path(),
        handle.factory.vcpus(),
        handle.factory.memory_mb(),
    );
    let mut tmpl = PooledTemplate::new(
        template_id,
        key,
        meta.snapshot_dir,
        handle.factory.image_path(),
        handle.factory.kernel_path(),
        handle.factory.vcpus(),
        handle.factory.memory_mb(),
        meta.original_task_vsock,
        meta.original_console_path,
    );
    // Snapshot was taken from a running sandbox — guest namespaces are already live.
    tmpl.ns_preinitialized = true;
    tmpl.id_generator = source_id_generator;
    tmpl.disk_images = meta.disk_images;

    handle.pool.add(tmpl.clone()).await?;
    info!(
        "template {}: created from sandbox {} (pool_depth={})",
        template_id,
        sandbox_id,
        handle.pool.depth(&tmpl.key).await,
    );
    Ok(tmpl)
}

/// Boot a fresh VM, optionally start a container with `image`, snapshot, and add to pool.
///
/// When `image` is empty the VM is snapshotted immediately after the guest agent is ready.
async fn create_from_image<F>(
    handle: &TemplateAdminHandle<F>,
    image: &str,
    template_id: &str,
) -> Result<PooledTemplate>
where
    F: VMFactory + Clone + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
{
    if image.is_empty() {
        create_template_worker(
            handle.factory.clone(),
            handle.pool.clone(),
            CreateTemplateRequest::new(template_id),
        )
        .await
    } else {
        create_template_from_image_worker(
            handle.factory.clone(),
            handle.pool.clone(),
            CreateTemplateRequest::new(template_id),
            image.to_string(),
        )
        .await
    }
}
