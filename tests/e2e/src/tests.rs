/*
Copyright 2025 The Kuasar Authors.

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

//! Kuasar E2E Tests
//!
//! This module contains end-to-end tests for all Kuasar runtimes.

use crate::{E2EConfig, E2EContext};
use serial_test::serial;
use std::time::Duration;
use tokio::time::timeout;
use tracing::info;

/// Test timeout for individual runtime tests
const TEST_TIMEOUT: Duration = Duration::from_secs(300); // 5 minutes

/// Setup function for tests - ensures clean environment
async fn setup_test_context() -> E2EContext {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let config = E2EConfig::default();
    E2EContext::new_with_config(config)
        .await
        .expect("Failed to create test context")
}

#[cfg(test)]
mod runtime_tests {
    use super::*;

    #[tokio::test]
    #[serial]
    async fn test_runc_runtime_lifecycle() {
        info!("Starting runc runtime lifecycle test");

        let mut ctx = setup_test_context().await;

        // Start services
        ctx.start_services(&["runc"])
            .await
            .expect("Failed to start runc service");

        // Run the test with timeout
        let result = timeout(TEST_TIMEOUT, ctx.test_runtime("runc"))
            .await
            .expect("Test timed out")
            .expect("Test execution failed");

        assert!(
            result.success,
            "runc runtime test failed: {:?}",
            result.error
        );
        assert!(result.sandbox_created, "Sandbox should be created");
        assert!(result.container_created, "Container should be created");
        assert!(result.container_started, "Container should be started");
        assert!(
            result.container_running,
            "Container should reach running state"
        );
        assert!(result.cleanup_completed, "Cleanup should complete");

        // Cleanup explicitly
        ctx.cleanup().await.expect("Cleanup should succeed");

        info!("runc runtime lifecycle test completed successfully");
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;

    #[tokio::test]
    #[serial]
    async fn test_service_startup_and_readiness() {
        info!("Testing service startup and readiness");

        let mut ctx = setup_test_context().await;
        let runtimes = ["runc"];

        // Start services and verify they become ready
        ctx.start_services(&runtimes)
            .await
            .expect("Services should start and become ready");

        // Verify socket files exist
        for runtime in &runtimes {
            let runtime_config = ctx
                .get_runtime_config(runtime)
                .expect("Should get runtime config");

            let socket_path = std::path::Path::new(&runtime_config.socket_path);
            assert!(
                socket_path.exists(),
                "Socket file should exist for runtime {}: {}",
                runtime,
                runtime_config.socket_path
            );
        }

        // Cleanup explicitly
        ctx.cleanup().await.expect("Cleanup should succeed");

        info!("Service startup and readiness test completed successfully");
    }

    #[tokio::test]
    #[serial]
    async fn test_configuration_files() {
        info!("Testing configuration files");

        let ctx = setup_test_context().await;
        let runtimes = ["runc"];

        for runtime in &runtimes {
            let runtime_config = ctx
                .get_runtime_config(runtime)
                .expect("Should get runtime config");

            assert!(
                runtime_config.sandbox_config.exists(),
                "Sandbox config should exist for runtime {}: {:?}",
                runtime,
                runtime_config.sandbox_config
            );

            assert!(
                runtime_config.container_config.exists(),
                "Container config should exist for runtime {}: {:?}",
                runtime,
                runtime_config.container_config
            );
        }

        info!("Configuration files test completed successfully");
    }

    #[tokio::test]
    async fn test_e2e_context_creation() {
        info!("Testing E2E context creation");

        let ctx = E2EContext::new().await.expect("Should create E2E context");

        assert!(!ctx.test_id.is_empty(), "Test ID should not be empty");
        assert!(ctx.config.repo_root.exists(), "Repo root should exist");
        assert!(ctx.config.config_dir.exists(), "Config dir should exist");

        info!("E2E context creation test completed successfully");
    }
}

/// Appliance mode tests.
///
/// These tests verify the appliance protocol and framework configuration without
/// requiring a running VM or guest.  They can be run with `make test-e2e-framework`.
#[cfg(test)]
mod appliance_tests {
    use super::*;
    use std::time::Duration;
    use tokio::{
        io::AsyncWriteExt,
        net::UnixStream,
        time::timeout,
    };

    fn unique_socket_path() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("/tmp/kuasar-e2e-appliance-{}.sock", nanos)
    }

    /// Verify the e2e framework can be configured with an appliance socket entry
    /// and that `get_runtime_config` returns the expected socket path.
    #[tokio::test]
    async fn test_appliance_runtime_config_entry() {
        let mut config = E2EConfig::default();
        config.sockets.insert(
            "vmm-appliance".to_string(),
            "/run/kuasar-vmm-appliance.sock".to_string(),
        );

        let ctx = E2EContext::new_with_config(config)
            .await
            .expect("Should create context");

        let runtime_config = ctx
            .get_runtime_config("vmm-appliance")
            .expect("Should get vmm-appliance config");

        assert_eq!(
            runtime_config.socket_path,
            "/run/kuasar-vmm-appliance.sock"
        );
        assert_eq!(runtime_config.name, "vmm-appliance");
    }

    /// Documents the wire format of the READY message that `ApplianceRuntime`
    /// expects: a JSON Lines object with `type == "READY"` and a matching
    /// `sandbox_id`.  This test exercises the raw socket protocol rather than
    /// calling `ApplianceRuntime` directly; see `vmm/sandbox` unit tests for
    /// tests that call `wait_ready` end-to-end.
    #[tokio::test]
    async fn test_appliance_wire_format_ready_message() {
        let socket_path = unique_socket_path();

        // Act as the host-side listener.
        let listener = tokio::net::UnixListener::bind(&socket_path)
            .expect("Should bind Unix socket");

        // Spawn a "guest" that sends a valid READY message.
        let path_clone = socket_path.clone();
        let sandbox_id = "e2e-sb-001";
        let msg = format!(r#"{{"type":"READY","sandbox_id":"{}"}}"#, sandbox_id);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let mut stream = UnixStream::connect(&path_clone)
                .await
                .expect("guest: connect failed");
            stream
                .write_all(format!("{}\n", msg).as_bytes())
                .await
                .expect("guest: write failed");
        });

        // Host accepts and reads the READY message.
        let (stream, _) = timeout(Duration::from_secs(2), listener.accept())
            .await
            .expect("accept timed out")
            .expect("accept failed");

        use tokio::io::AsyncBufReadExt;
        let mut reader = tokio::io::BufReader::new(stream);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .expect("read_line failed");

        let msg: serde_json::Value =
            serde_json::from_str(line.trim_end()).expect("Should parse JSON");
        assert_eq!(msg["type"].as_str().unwrap(), "READY");
        assert_eq!(msg["sandbox_id"].as_str().unwrap(), sandbox_id);

        let _ = std::fs::remove_file(&socket_path);
    }

    /// Documents the wire format of the FATAL message that `ApplianceRuntime`
    /// must handle: a JSON Lines object with `type == "FATAL"` and a `reason`
    /// field.  This test exercises the raw socket protocol rather than calling
    /// `ApplianceRuntime` directly; see `vmm/sandbox` unit tests for tests that
    /// call `wait_ready` with a FATAL response.
    #[tokio::test]
    async fn test_appliance_wire_format_fatal_message() {
        let socket_path = unique_socket_path();
        let listener = tokio::net::UnixListener::bind(&socket_path)
            .expect("Should bind Unix socket");

        let path_clone = socket_path.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let mut stream = UnixStream::connect(&path_clone)
                .await
                .expect("guest: connect failed");
            let msg = r#"{"type":"FATAL","reason":"init.d hook failed"}"#;
            stream
                .write_all(format!("{}\n", msg).as_bytes())
                .await
                .expect("guest: write failed");
        });

        let (stream, _) = timeout(Duration::from_secs(2), listener.accept())
            .await
            .expect("accept timed out")
            .expect("accept failed");

        use tokio::io::AsyncBufReadExt;
        let mut reader = tokio::io::BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("read_line failed");

        let msg: serde_json::Value =
            serde_json::from_str(line.trim_end()).expect("Should parse JSON");
        assert_eq!(msg["type"].as_str().unwrap(), "FATAL");
        assert!(
            msg["reason"].as_str().is_some(),
            "FATAL message must include a reason field"
        );

        let _ = std::fs::remove_file(&socket_path);
    }

    /// Verify that no service is running for vmm-appliance when none was started.
    #[tokio::test]
    async fn test_appliance_service_not_started() {
        let mut config = E2EConfig::default();
        config.sockets.insert(
            "vmm-appliance".to_string(),
            "/run/kuasar-vmm-appliance.sock".to_string(),
        );
        let ctx = E2EContext::new_with_config(config)
            .await
            .expect("Should create context");

        // Ensure socket doesn't exist from a prior run.
        let _ = std::fs::remove_file("/run/kuasar-vmm-appliance.sock");

        let result = timeout(Duration::from_secs(5), ctx.test_runtime("vmm-appliance"))
            .await
            .expect("Should complete within timeout")
            .expect("Should not panic");

        assert!(
            !result.success,
            "Test must fail when appliance service is not running"
        );
        assert!(result.error.is_some(), "Error should be reported");
    }
}

#[cfg(test)]
mod error_handling_tests {
    use super::*;

    #[tokio::test]
    async fn test_invalid_runtime() {
        info!("Testing invalid runtime handling");

        let ctx = setup_test_context().await;

        // Test with invalid runtime name
        let result = ctx.get_runtime_config("invalid-runtime");
        assert!(result.is_err(), "Should fail with invalid runtime name");

        info!("Invalid runtime handling test completed successfully");
    }

    #[tokio::test]
    #[serial]
    async fn test_service_not_started() {
        info!("Testing behavior when service is not started");

        let ctx = setup_test_context().await;
        // Intentionally not starting services

        // Clean up any existing socket files to ensure clean state
        let runtime_config = ctx
            .get_runtime_config("runc")
            .expect("Should get runc config");
        let socket_path = std::path::Path::new(&runtime_config.socket_path);
        if socket_path.exists() {
            let _ = std::fs::remove_file(socket_path);
        }

        let result = timeout(Duration::from_secs(10), ctx.test_runtime("runc"))
            .await
            .expect("Test should complete within timeout")
            .expect("Test execution should not panic");

        assert!(
            !result.success,
            "Test should fail when service is not started"
        );
        assert!(result.error.is_some(), "Error should be reported");

        info!("Service not started test completed successfully");
    }
}
