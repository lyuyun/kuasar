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

//! Application readiness detection.
//!
//! Three modes (selected via `KUASAR_READY` kernel cmdline parameter):
//!
//! | Mode | `KUASAR_READY` value | Description |
//! |------|---------------------|-------------|
//! | A    | `immediate` (default) | Ready as soon as the app process is fork+exec'd. |
//! | B    | `tcp:<port>`          | Poll the app's TCP port until it accepts connections. |
//! | C    | `file:<path>`         | Wait until the app creates a designated ready file. |
//!
//! Mode B and C have a configurable timeout (`KUASAR_READY_TIMEOUT_MS`);
//! the default is 30 000 ms.

use std::{path::PathBuf, time::Duration};

use anyhow::{anyhow, Result};
use log::info;
use tokio::time::{sleep, timeout};

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const POLL_INTERVAL_MS: u64 = 100;

/// How to detect that the application is ready to serve.
#[derive(Debug, Clone)]
pub enum ReadinessCheck {
    /// Return immediately once the app process is running (Mode A).
    Immediate,
    /// Connect to `localhost:<port>` until it succeeds (Mode B).
    TcpPort { port: u16, timeout_ms: u64 },
    /// Wait until `path` exists (Mode C).
    File { path: PathBuf, timeout_ms: u64 },
}

impl ReadinessCheck {
    /// Parse from the `KUASAR_READY` and `KUASAR_READY_TIMEOUT_MS` cmdline values.
    pub fn from_cmdline(ready: Option<&str>, timeout_ms: Option<u64>) -> Result<Self> {
        let t = timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        match ready.unwrap_or("immediate") {
            "immediate" => Ok(Self::Immediate),
            s if s.starts_with("tcp:") => {
                let port: u16 = s[4..]
                    .parse()
                    .map_err(|_| anyhow!("invalid port in KUASAR_READY={}", s))?;
                Ok(Self::TcpPort {
                    port,
                    timeout_ms: t,
                })
            }
            s if s.starts_with("file:") => Ok(Self::File {
                path: PathBuf::from(&s[5..]),
                timeout_ms: t,
            }),
            s => Err(anyhow!("unknown KUASAR_READY value: {:?}", s)),
        }
    }

    /// Wait until the application is ready.  Returns immediately for Mode A.
    pub async fn wait(&self) -> Result<()> {
        match self {
            Self::Immediate => Ok(()),
            Self::TcpPort { port, timeout_ms } => {
                info!(
                    "kuasar-init: waiting for TCP port {} (timeout {}ms)",
                    port, timeout_ms
                );
                let addr = format!("127.0.0.1:{}", port);
                let dur = Duration::from_millis(*timeout_ms);
                timeout(dur, poll_tcp(&addr)).await.map_err(|_| {
                    anyhow!("TCP readiness timeout after {}ms on {}", timeout_ms, addr)
                })?
            }
            Self::File { path, timeout_ms } => {
                info!(
                    "kuasar-init: waiting for ready file {:?} (timeout {}ms)",
                    path, timeout_ms
                );
                let dur = Duration::from_millis(*timeout_ms);
                timeout(dur, poll_file(path)).await.map_err(|_| {
                    anyhow!("file readiness timeout after {}ms: {:?}", timeout_ms, path)
                })?
            }
        }
    }
}

async fn poll_tcp(addr: &str) -> Result<()> {
    loop {
        match tokio::net::TcpStream::connect(addr).await {
            Ok(_) => return Ok(()),
            Err(_) => sleep(Duration::from_millis(POLL_INTERVAL_MS)).await,
        }
    }
}

async fn poll_file(path: &PathBuf) -> Result<()> {
    loop {
        if path.exists() {
            return Ok(());
        }
        sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_immediate() {
        let c = ReadinessCheck::from_cmdline(None, None).unwrap();
        assert!(matches!(c, ReadinessCheck::Immediate));
    }

    #[test]
    fn test_parse_immediate_explicit() {
        let c = ReadinessCheck::from_cmdline(Some("immediate"), None).unwrap();
        assert!(matches!(c, ReadinessCheck::Immediate));
    }

    #[test]
    fn test_parse_tcp_port() {
        let c = ReadinessCheck::from_cmdline(Some("tcp:8080"), Some(5000)).unwrap();
        assert!(matches!(
            c,
            ReadinessCheck::TcpPort {
                port: 8080,
                timeout_ms: 5000
            }
        ));
    }

    #[test]
    fn test_parse_tcp_invalid_port() {
        assert!(ReadinessCheck::from_cmdline(Some("tcp:notaport"), None).is_err());
    }

    #[test]
    fn test_parse_file() {
        let c = ReadinessCheck::from_cmdline(Some("file:/run/app-ready"), None).unwrap();
        assert!(matches!(c, ReadinessCheck::File { .. }));
        if let ReadinessCheck::File { path, .. } = c {
            assert_eq!(path, PathBuf::from("/run/app-ready"));
        }
    }

    #[test]
    fn test_parse_unknown() {
        assert!(ReadinessCheck::from_cmdline(Some("invalid:value"), None).is_err());
    }

    #[tokio::test]
    async fn test_immediate_returns_instantly() {
        ReadinessCheck::Immediate
            .wait()
            .await
            .expect("immediate always succeeds");
    }

    #[tokio::test]
    async fn test_tcp_readiness_timeout() {
        // Port 1 is never open; should time out in 200ms.
        let check = ReadinessCheck::TcpPort {
            port: 1,
            timeout_ms: 200,
        };
        let result = check.wait().await;
        assert!(result.is_err(), "must time out when no server is listening");
    }

    #[tokio::test]
    async fn test_file_readiness_timeout() {
        let check = ReadinessCheck::File {
            path: PathBuf::from("/tmp/kuasar-init-test-ready-nonexistent"),
            timeout_ms: 200,
        };
        let result = check.wait().await;
        assert!(result.is_err(), "must time out when file never appears");
    }

    #[tokio::test]
    async fn test_file_readiness_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ready");

        let path_clone = path.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            std::fs::write(&path_clone, b"").unwrap();
        });

        let check = ReadinessCheck::File {
            path,
            timeout_ms: 2000,
        };
        check.wait().await.expect("file appeared before timeout");
    }
}
