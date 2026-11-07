use std::{collections::BTreeMap, sync::Arc, time::Duration};

use axum::{
    Router,
    body::Bytes,
    extract::WebSocketUpgrade,
    http::HeaderMap,
    routing::{get, post},
};
use stravia_runtime_contract::{CancellationToken, Deadline};
use stravia_vendor_runtime::{OperationScope, RuntimeEvent, VendorRuntime};
use stravia_vendor_sdk::{
    AiRequest, AiResponse, OperationInput, OperationOutput, ProviderSnapshot,
};
use tokio::sync::mpsc;

use super::{
    VendorNetwork,
    shape::ShapeRegistry,
    transport::{SystemResolver, VendorTransport},
};
use crate::{
    db::models::CreateProviderRecord,
    plugin::{
        execution::released_compat_services,
        lifecycle::VendorOperationTracker,
        permissions::{NetworkOrigin, NetworkPolicy},
        store::InstalledPlugin,
    },
    storage::{DynStorage, MemoryStorage},
};

#[tokio::test]
async fn released_component_uses_real_native_http_and_websocket() -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let (observed, mut requests) = mpsc::channel(2);
    let http_observed = observed.clone();
    let app = Router::new()
        .route(
            "/compat",
            post(move |headers: HeaderMap, body: Bytes| {
                let observed = http_observed.clone();
                async move {
                    assert_eq!(headers["x-compat"], "snapshot");
                    assert!(!headers.contains_key("accept"));
                    assert_eq!(body.as_ref(), b"released-http");
                    observed.send("http").await.unwrap();
                    ([("x-compat-response", "http-ok")], "http-response")
                }
            }),
        )
        .route(
            "/socket",
            get(move |upgrade: WebSocketUpgrade| {
                let observed = observed.clone();
                async move {
                    upgrade.on_upgrade(move |mut socket| async move {
                        let message = socket.recv().await.unwrap().unwrap();
                        assert_eq!(message.into_text().unwrap(), "released-ws");
                        let response = AiResponse::new("native-released-response", "compat-model");
                        socket
                            .send(axum::extract::ws::Message::Text(
                                serde_json::to_string(&response).unwrap().into(),
                            ))
                            .await
                            .unwrap();
                        assert!(matches!(
                            socket.recv().await.unwrap().unwrap(),
                            axum::extract::ws::Message::Close(_)
                        ));
                        observed.send("websocket").await.unwrap();
                    })
                }
            }),
        );
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let cancellation = CancellationToken::new();
    let vendor_operation = VendorOperationTracker::default().begin("released-native-compat")?;
    let storage: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
    storage
        .vendor_plugins()
        .install(
            &InstalledPlugin {
                vendor_id: "fixture.net-1-0-compat".into(),
                version: "1.0.0".into(),
                source: "local".into(),
                descriptor: "{}".into(),
                component: Bytes::new(),
                digest: "test".into(),
                revision: 1,
                data_epoch: 1,
                installed_at: 1,
            },
            None,
            &[],
        )
        .await?;
    let stored_provider = storage
        .providers()
        .create(CreateProviderRecord {
            name: "released compatibility".into(),
            vendor: Some("fixture.net-1-0-compat".into()),
            protocol: "fixture-compat".into(),
            base_url: format!("http://{address}"),
            preset_key: None,
            channel: Some("default".into()),
            models_source: None,
            static_models: None,
            api_key: String::new(),
            adapter_credentials: "{}".into(),
            vendor_options: "{}".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
        })
        .await?;
    let network = VendorNetwork::new(
        Arc::new(VendorTransport::new()),
        super::VendorNetworkConfig {
            shapes: Arc::new(ShapeRegistry::builtin()?),
            resolver: Arc::new(SystemResolver),
        },
        crate::outbound_proxy::OutboundProxySettings::default(),
        NetworkPolicy::compile([
            NetworkOrigin::Exact(format!("http://{address}")),
            NetworkOrigin::Exact(format!("ws://{address}")),
        ])?,
        vendor_operation.clone(),
        cancellation.clone(),
        "fixture-compat".into(),
    );
    let runtime = VendorRuntime::new()?;
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../target/vendor-test-fixtures/net-1.0-compat.wasm");
    let plugin = runtime.load(&std::fs::read(fixture)?).await?;
    let provider = ProviderSnapshot {
        provider_id: "fixture.net-1-0-compat".into(),
        channel: "default".into(),
        base_url: format!("http://{address}"),
        protocol: "fixture-compat".into(),
        options: BTreeMap::from([(
            "wsUrl".into(),
            serde_json::json!(format!("ws://{address}/socket")),
        )]),
        credentials: BTreeMap::new(),
        model: None,
        model_metadata: None,
        client_headers: Vec::new(),
        operation_metadata: BTreeMap::new(),
    };
    let (operation, input) = OperationInput::Infer {
        provider,
        request: AiRequest::new("compat-model", Vec::new()),
    }
    .encode_for_host()?;
    let (events, mut emitted) = mpsc::channel(4);
    let deadline = Deadline::from_now(Duration::from_secs(10));
    let services = released_compat_services(
        network,
        storage.vendor_plugins().clone(),
        stored_provider.id,
        vendor_operation,
        cancellation.clone(),
        deadline.clone(),
        events,
    );
    let scope = OperationScope::new(services, cancellation, deadline, 0);
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        runtime.execute(&plugin, "default", operation, input, scope),
    )
    .await;
    let OperationOutput::Infer(response) = result?? else {
        anyhow::bail!("expected inference response")
    };
    assert_eq!(response.id, "native-released-response");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), requests.recv()).await?,
        Some("http")
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), requests.recv()).await?,
        Some("websocket")
    );
    server.abort();
    assert!(matches!(
        emitted.recv().await.map(|event| event.event),
        Some(RuntimeEvent::UpstreamStarted)
    ));
    assert!(matches!(
        emitted.recv().await.map(|event| event.event),
        Some(RuntimeEvent::Completed)
    ));
    Ok(())
}
