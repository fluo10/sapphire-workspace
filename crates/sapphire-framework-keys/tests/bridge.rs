//! `BridgeVerifier` against a real bridge: accepted, refused, cached, unavailable.

use std::sync::Arc;
use std::time::Duration;

use sapphire_bridge_api::{
    BridgeClient, ExternalDeviceAddParams, ExternalDeviceOutcome, ExternalDeviceRequest,
};
use sapphire_framework_bridge::{Bridge, BridgeDir, LoopbackNetwork, NetConfig, Workgroup};
use sapphire_framework_keys::{BridgeVerifier, Verdict, Verifier};
use sapphire_ipc::Endpoint;

/// A bridge with a workgroup, on its own runtime so the test can stop it.
fn start(tmp: &tempfile::TempDir, control: &Endpoint) -> tokio::runtime::Runtime {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let guard = rt.enter();
    let dir = BridgeDir::at(tmp.path().join("bridge")).unwrap();
    Workgroup::create(&dir, "home", "this", "aaaa").unwrap();
    let net = LoopbackNetwork::new();
    let bridge = Bridge::new(dir, Arc::new(net.transport("aaaa")), "0.0.0")
        .unwrap()
        .control_endpoint(control.clone())
        .data_endpoint(Endpoint::in_dir("bridge-data", control.dir.clone()));
    drop(guard);
    rt.spawn(async move {
        let _ = Arc::new(bridge).run_shared(NetConfig::default()).await;
    });
    rt
}

async fn wait_listening(control: &Endpoint) {
    for _ in 0..100 {
        if sapphire_ipc::probe(control).await.unwrap() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the bridge never listened");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_verifier_asks_the_bridge_and_caches_a_success() {
    let tmp = tempfile::tempdir().unwrap();
    let run = tmp.path().join("run");
    std::fs::create_dir_all(&run).unwrap();
    let control = Endpoint::in_dir("bridge", run);
    let rt = start(&tmp, &control);
    wait_listening(&control).await;

    let client = BridgeClient::connect_at(&control, "test", "0.0.0")
        .await
        .unwrap()
        .unwrap();
    let ExternalDeviceOutcome::WithToken(added) = client
        .external_device_request(ExternalDeviceRequest::Add(ExternalDeviceAddParams {
            name: "pendant".into(),
            description: None,
            apps: vec!["agent".into()],
        }))
        .await
        .unwrap()
    else {
        panic!("add returns a token");
    };
    let token = added.token.expose().to_owned();

    let agent = BridgeVerifier::new("agent", "0.0.0")
        .at(control.clone())
        .cache_ttl(Duration::from_secs(3600));
    let Verdict::Accepted(who) = agent.verify(&token).await else {
        panic!("the right token for its app is accepted");
    };
    assert_eq!(
        (who.id, who.name.as_str()),
        (added.external_device.id, "pendant")
    );
    assert_eq!(agent.verify("sapphire-ed-wrong").await, Verdict::Refused);
    let journal = BridgeVerifier::new("journal", "0.0.0").at(control.clone());
    assert_eq!(
        journal.verify(&token).await,
        Verdict::Refused,
        "an app it may not use"
    );

    // The bridge stops: a cached success still stands, anything else cannot be checked.
    drop(client);
    rt.shutdown_background();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(matches!(agent.verify(&token).await, Verdict::Accepted(_)));
    assert_eq!(
        agent.verify("sapphire-ed-other").await,
        Verdict::Unavailable
    );
}
