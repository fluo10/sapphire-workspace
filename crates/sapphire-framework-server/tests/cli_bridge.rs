//! Integration tests for framework CLI commands that go through the bridge.

use std::sync::Arc;

use sapphire_framework_bridge::{Bridge, BridgeDir, LoopbackNetwork, NetConfig};
use sapphire_framework_server::{DeviceCommand, WorkgroupCommand};
use sapphire_ipc::Endpoint;

/// Process-wide lock for environment variable manipulation.
static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Start a bridge with no workgroup on the default endpoints of the given runtime directory.
async fn start_bridge_no_workgroup(runtime: &std::path::Path) -> (Arc<Bridge>, Endpoint) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = BridgeDir::at(tmp.path().join("bridge")).unwrap();

    let control = Endpoint::in_dir("bridge", runtime.to_path_buf());
    let data = Endpoint::in_dir("bridge-data", runtime.to_path_buf());

    let node_id = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";
    let bridge = Arc::new(
        Bridge::new(
            dir,
            Arc::new(LoopbackNetwork::new().transport(node_id)),
            "0.0.0",
        )
        .unwrap()
        .net(NetConfig::default())
        .control_endpoint(control.clone())
        .data_endpoint(data),
    );

    let shared = Arc::clone(&bridge);
    tokio::spawn(async move {
        let _ = shared.run_shared(NetConfig::default()).await;
    });

    // Wait for the control endpoint to come up
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !sapphire_ipc::probe(&control).await.unwrap_or(false) {
        assert!(
            std::time::Instant::now() < deadline,
            "the bridge never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    // Keep tmp alive for the duration of the test
    std::mem::forget(tmp);

    (bridge, control)
}

#[tokio::test(flavor = "multi_thread")]
async fn workgroup_create_succeeds() {
    let _lock = ENV.lock().await;

    // Set up temporary runtime directory
    let tmp = tempfile::tempdir().unwrap();
    let runtime = tmp.path().to_path_buf();

    // Set environment variable
    unsafe {
        std::env::set_var(sapphire_ipc::RUNTIME_DIR_ENV, &runtime);
    }

    // Start the bridge with no workgroup
    let (bridge, _control) = start_bridge_no_workgroup(&runtime).await;

    // Test WorkgroupCommand::Create
    let result = WorkgroupCommand::Create {
        name: "home".into(),
        device_name: "desk".into(),
    }
    .dispatch("0.0.0")
    .await;

    assert!(result.is_ok(), "Create should succeed: {:?}", result);
    assert_eq!(result.unwrap(), 0, "Create should return exit code 0");

    // Verify the workgroup was created by connecting and checking status
    let client = sapphire_bridge_client::BridgeClient::connect_running("test", "0.0.0")
        .await
        .expect("should connect to bridge");
    let status = client.status().await.expect("should get status");

    assert!(
        status.workgroup.is_some(),
        "workgroup should exist after create"
    );
    assert_eq!(
        status.workgroup.as_ref().unwrap().name,
        "home",
        "workgroup name should be 'home'"
    );

    // Keep bridge alive
    drop(bridge);

    // Clean up
    unsafe {
        std::env::remove_var(sapphire_ipc::RUNTIME_DIR_ENV);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn device_retire_own_device_fails() {
    let _lock = ENV.lock().await;

    // Set up temporary runtime directory
    let tmp = tempfile::tempdir().unwrap();
    let runtime = tmp.path().to_path_buf();

    // Set environment variable
    unsafe {
        std::env::set_var(sapphire_ipc::RUNTIME_DIR_ENV, &runtime);
    }

    // Start the bridge with no workgroup
    let (bridge, _control) = start_bridge_no_workgroup(&runtime).await;

    // Create a workgroup first
    let create_result = WorkgroupCommand::Create {
        name: "home".into(),
        device_name: "desk".into(),
    }
    .dispatch("0.0.0")
    .await;

    assert!(create_result.is_ok(), "Create should succeed");

    // Try to retire the current device by name
    let retire_result = DeviceCommand::Retire {
        selector: "desk".into(),
    }
    .dispatch("0.0.0")
    .await;

    assert!(
        retire_result.is_err(),
        "Retire own device should fail: {:?}",
        retire_result
    );

    let err_msg = format!("{:?}", retire_result.unwrap_err());
    assert!(
        err_msg.contains("own device") || err_msg.to_lowercase().contains("own device"),
        "Error should mention 'own device', got: {}",
        err_msg
    );

    // Keep bridge alive
    drop(bridge);

    // Clean up
    unsafe {
        std::env::remove_var(sapphire_ipc::RUNTIME_DIR_ENV);
    }
}
