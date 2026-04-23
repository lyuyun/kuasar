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

//! Minimal HTTP server that exposes Prometheus metrics on `/metrics`.
//!
//! # Configuration
//!
//! Set the `KUASAR_METRICS_ADDR` environment variable (default `127.0.0.1:9090`)
//! to control the listen address.  Set it to an empty string to disable.
//!
//! # Usage
//!
//! Call [`start`] once at process startup.  It spawns a background Tokio task
//! and returns immediately.  The task runs until the process exits.

use std::net::SocketAddr;

use hyper::{
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server, StatusCode,
};
use log::{error, info};

const DEFAULT_METRICS_ADDR: &str = "127.0.0.1:9090";
const METRICS_ADDR_ENV: &str = "KUASAR_METRICS_ADDR";

/// Spawn the metrics HTTP server in the background.
///
/// Returns immediately.  The server runs until the process exits.
pub fn start() {
    let addr_str = std::env::var(METRICS_ADDR_ENV)
        .unwrap_or_else(|_| DEFAULT_METRICS_ADDR.to_string());

    if addr_str.is_empty() {
        info!("metrics server disabled (KUASAR_METRICS_ADDR is empty)");
        return;
    }

    let addr: SocketAddr = match addr_str.parse() {
        Ok(a) => a,
        Err(e) => {
            error!("invalid KUASAR_METRICS_ADDR '{}': {}", addr_str, e);
            return;
        }
    };

    tokio::spawn(async move {
        let make_svc = make_service_fn(|_conn| async {
            Ok::<_, hyper::Error>(service_fn(handle))
        });

        match Server::try_bind(&addr) {
            Ok(builder) => {
                info!("metrics server listening on http://{}/metrics", addr);
                if let Err(e) = builder.serve(make_svc).await {
                    error!("metrics server error: {}", e);
                }
            }
            Err(e) => error!("metrics server bind {}: {}", addr, e),
        }
    });
}

async fn handle(req: Request<Body>) -> Result<Response<Body>, hyper::Error> {
    match req.uri().path() {
        "/metrics" => {
            let body = match crate::snapshot::metrics::gather_text() {
                Ok(text) => text,
                Err(e) => {
                    return Ok(Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(Body::from(e.to_string()))
                        .unwrap());
                }
            };
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header(
                    hyper::header::CONTENT_TYPE,
                    "text/plain; version=0.0.4; charset=utf-8",
                )
                .body(Body::from(body))
                .unwrap())
        }
        "/healthz" => Ok(Response::new(Body::from("ok"))),
        _ => Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("not found\n"))
            .unwrap()),
    }
}
