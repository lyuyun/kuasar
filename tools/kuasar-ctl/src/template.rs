/*
Copyright 2026 The Kuasar Authors.

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

//! Template management via the vmm-sandboxer admin socket.
//!
//! All VM operations are delegated to the running sandboxer daemon; this module
//! only handles the client-side JSON-line protocol.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Send a single JSON request to the admin socket and return the parsed response.
async fn call_admin(admin_sock: &Path, request: Value) -> Result<Value> {
    let mut stream = UnixStream::connect(admin_sock)
        .await
        .with_context(|| format!("connect to admin socket {:?}", admin_sock))?;

    let mut line = serde_json::to_string(&request)?;
    line.push('\n');
    stream.write_all(line.as_bytes()).await?;

    let (reader, _) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let resp_line = lines
        .next_line()
        .await?
        .ok_or_else(|| anyhow!("admin socket closed without response"))?;

    serde_json::from_str(&resp_line).context("parse admin response")
}

fn check_response(resp: Value) -> Result<String> {
    if resp["ok"].as_bool().unwrap_or(false) {
        Ok(resp["template_id"].as_str().unwrap_or("").to_string())
    } else {
        Err(anyhow!(
            "sandboxer error: {}",
            resp["error"].as_str().unwrap_or("unknown error")
        ))
    }
}

/// Snapshot a running sandbox's VM and register the result as a template.
///
/// The sandbox continues running after the snapshot (VM is resumed immediately).
pub async fn create_from_sandbox(
    admin_sock: &Path,
    sandbox_id: &str,
    template_id: &str,
) -> Result<()> {
    let req = json!({
        "action": "from-sandbox",
        "sandbox_id": sandbox_id,
        "template_id": template_id,
    });
    let resp = call_admin(admin_sock, req).await?;
    let id = check_response(resp)?;
    println!("template {} created from sandbox {}", id, sandbox_id);
    Ok(())
}

/// Boot a fresh VM, optionally start a container with `image`, snapshot, and register as a template.
///
/// When `image` is empty the VM is snapshotted immediately after the guest agent is ready.
pub async fn create_from_image(admin_sock: &Path, image: &str, template_id: &str) -> Result<()> {
    let req = json!({
        "action": "from-image",
        "image": image,
        "template_id": template_id,
    });
    let resp = call_admin(admin_sock, req).await?;
    let id = check_response(resp)?;
    if image.is_empty() {
        println!("template {} created from fresh VM", id);
    } else {
        println!("template {} created from image {}", id, image);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_response_ok() {
        let resp = serde_json::json!({"ok": true, "template_id": "tmpl-001"});
        assert_eq!(check_response(resp).unwrap(), "tmpl-001");
    }

    #[test]
    fn test_check_response_error() {
        let resp = serde_json::json!({"ok": false, "error": "sandbox not found"});
        let err = check_response(resp).unwrap_err();
        assert!(err.to_string().contains("sandbox not found"));
    }
}
