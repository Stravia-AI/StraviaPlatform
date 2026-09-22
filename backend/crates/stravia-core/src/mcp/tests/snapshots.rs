use super::*;
use stravia_runtime_contract::{
    Principal,
    artifact::{ArtifactId, ArtifactRef, ArtifactSource, bytes_stream},
};

const SNAPSHOT_MIME: &str = "application/vnd.stravia.read-snapshot";

async fn put(app: &TestApp, mime: &str, bytes: Vec<u8>) -> ArtifactRef {
    app.gateway
        .artifact_store()
        .unwrap()
        .ingest(
            &Principal::new(app.key_id.clone()),
            mime,
            Some(bytes.len() as u64),
            bytes_stream(Bytes::from(bytes)),
            Duration::from_secs(3600),
        )
        .await
        .expect("fixture Artifact")
}

async fn call(client: &SdkClient, path: &str) -> rmcp::model::CallToolResult {
    client
        .call_tool(
            CallToolRequestParams::new("StraviaRead")
                .with_arguments(json!({"path":path}).as_object().unwrap().clone()),
        )
        .await
        .expect("MCP tools/call")
}

async fn page(client: &SdkClient, path: &str) -> Value {
    let result = call(client, path).await;
    assert_ne!(
        result.is_error,
        Some(true),
        "{:?}",
        result.structured_content
    );
    result.structured_content.expect("text page")
}

fn frame(mut header: Value, body: &[u8]) -> Vec<u8> {
    header["text_bytes"] = json!(body.len());
    let encoded = serde_json::to_vec(&header).unwrap();
    let mut bytes = (encoded.len() as u32).to_be_bytes().to_vec();
    bytes.extend(encoded);
    bytes.extend(body);
    bytes
}

fn header() -> Value {
    json!({"version":1,"representation":"text","source_truncated":false,"limitations":[]})
}

#[tokio::test]
async fn long_unicode_line_is_complete_and_independent_of_the_original_artifact() {
    let (app, _, calls) = media_test_app().await;
    let client = connect(&app).await;
    let listed = client.list_tools(None).await.unwrap();
    let tool = listed
        .tools
        .iter()
        .find(|tool| tool.name == "StraviaRead")
        .unwrap();
    assert_eq!(tool.input_schema.get("required"), Some(&json!(["path"])));
    assert_eq!(
        tool.input_schema
            .get("properties")
            .unwrap()
            .as_object()
            .unwrap()
            .len(),
        1
    );
    let original = "中文🙂".repeat(9 * 1024);
    let source = put(
        &app,
        "text/plain; charset=utf-8",
        original.as_bytes().to_vec(),
    )
    .await;
    let mut current = page(&client, &source.reference()).await;
    let read_path = current["read_path"].as_str().unwrap().to_owned();
    assert_ne!(read_path, source.reference());
    let reader = app
        .gateway
        .artifact_store()
        .unwrap()
        .open(&Principal::new(app.key_id.clone()), &source.id)
        .await
        .unwrap();
    let ArtifactSource::LocalPath(path) = reader.source.clone() else {
        panic!("local source")
    };
    drop(reader);
    tokio::fs::remove_file(path).await.unwrap();
    let mut assembled = String::new();
    let mut pages = 0;
    loop {
        let body = current["content"].as_str().unwrap();
        assert!(!body.is_empty() && body.len() <= 32 * 1024);
        assert_eq!(current["returned_ranges"][0]["start_line"], 1);
        assert_eq!(current["returned_ranges"][0]["end_line"], 1);
        assert_eq!(
            current["returned_ranges"][0]["start_byte"]
                .as_u64()
                .unwrap(),
            assembled.len() as u64
        );
        assembled.push_str(body);
        assert_eq!(
            current["returned_ranges"][0]["end_byte"].as_u64().unwrap(),
            assembled.len() as u64
        );
        assert_eq!(current["read_path"], read_path);
        pages += 1;
        assert!(pages <= 4, "continuation must make progress");
        let Some(next) = current["next_path"].as_str().map(str::to_owned) else {
            break;
        };
        assert_eq!(current["has_more"], true);
        current = page(&client, &next).await;
    }
    assert_eq!(assembled.as_bytes(), original.as_bytes());
    assert_eq!(current["has_more"], false);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn selected_crlf_ranges_share_the_line_budget_without_context_or_phantom_lines() {
    let (app, _, _) = media_test_app().await;
    let client = connect(&app).await;
    let lines: Vec<String> = (1..=260).map(|line| format!("line {line}\r\n")).collect();
    let source = put(&app, "text/plain", lines.concat().into_bytes()).await;
    let first = page(
        &client,
        &format!("{}?lines=1-150,201-260", source.reference()),
    )
    .await;
    assert_eq!(
        first["content"],
        [lines[..150].concat(), lines[200..250].concat()].concat()
    );
    assert_eq!(first["returned_ranges"][1]["end_line"], 250);
    let last = page(&client, first["next_path"].as_str().unwrap()).await;
    assert_eq!(last["content"], lines[250..].concat());
    assert_eq!(last["returned_ranges"][0]["start_line"], 251);
    assert_eq!(last["has_more"], false);
    let snapshot = first["read_path"].as_str().unwrap();
    let selected = page(&client, &format!("{snapshot}?lines=-1")).await;
    assert_eq!(selected["content"], "line 260\r\n");
    let counted = page(&client, &format!("{snapshot}?lines=259%2B2")).await;
    assert_eq!(counted["content"], "line 259\r\nline 260\r\n");
    for range in ["0", "-0", "-+1", "261", "18446744073709551615%2B2"] {
        assert_eq!(
            call(&client, &format!("{snapshot}?lines={range}"))
                .await
                .is_error,
            Some(true)
        );
    }
}

#[tokio::test]
async fn empty_snapshot_reads_and_downloads_exactly_empty_text() {
    let (app, _, calls) = media_test_app().await;
    let client = connect(&app).await;
    let source = put(&app, "text/plain", Vec::new()).await;
    let empty = page(&client, &source.reference()).await;
    assert_eq!(empty["content"], "");
    assert_eq!(empty["returned_ranges"], json!([]));
    assert_eq!(empty["has_more"], false);
    let reference = empty["read_path"].as_str().unwrap();
    assert_eq!(
        call(&client, &format!("{reference}?lines=1"))
            .await
            .is_error,
        Some(true)
    );
    let download = page(&client, &format!("{reference}?download=1")).await;
    let response = reqwest::get(download["download_url"].as_str().unwrap())
        .await
        .unwrap();
    assert!(response.status().is_success());
    assert_eq!(response.bytes().await.unwrap().as_ref(), b"");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cursor_validation_and_expiry_cannot_bypass_snapshot_ownership() {
    let (app, _, _) = media_test_app().await;
    let client = connect(&app).await;
    let source = put(&app, "text/plain", "🙂".repeat(10_000).into_bytes()).await;
    let first = page(&client, &source.reference()).await;
    let reference = first["read_path"].as_str().unwrap();
    let next = first["next_path"].as_str().unwrap();
    let encoded = next.split_once("cursor=").unwrap().1;
    let cursor: Value = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .unwrap(),
    )
    .unwrap();
    let mut mutations = Vec::new();
    let mut foreign = cursor.clone();
    foreign["artifact_id"] = json!("another_snapshot");
    mutations.push(foreign);
    let mut boundary = cursor.clone();
    boundary["offset"] = json!(1);
    mutations.push(boundary);
    let mut bounds = cursor.clone();
    bounds["ranges"][0][1] = json!(40001);
    mutations.push(bounds);
    let mut index = cursor.clone();
    index["range_index"] = json!(10);
    mutations.push(index);
    for mutation in mutations {
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&mutation).unwrap());
        assert_eq!(
            call(&client, &format!("{reference}?cursor={encoded}"))
                .await
                .is_error,
            Some(true)
        );
    }
    assert_eq!(
        call(&client, &format!("{}?cursor={encoded}", source.reference()))
            .await
            .is_error,
        Some(true)
    );
    let other = app
        .gateway
        .admin()
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Snapshot other owner".into(),
            concurrency_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: false,
            inject_web_search: false,
            inject_media_generation: false,
            model_ids: vec![],
            inject_media_understanding: false,
        })
        .await
        .unwrap();
    let transport = StreamableHttpClientTransport::with_client(
        reqwest::Client::new(),
        StreamableHttpClientTransportConfig::with_uri(app.endpoint.clone())
            .auth_header(other.token),
    );
    let other_client = ClientInfo::default()
        .serve_with_lifecycle(transport, ClientLifecycleMode::Initialize)
        .await
        .unwrap();
    assert_eq!(call(&other_client, next).await.is_error, Some(true));
    let id = ArtifactId::from_reference(reference).unwrap();
    let pool = crate::db::init_pool(app._data_dir.path()).await.unwrap();
    sqlx::query("UPDATE artifacts SET expires_at=0 WHERE id=?")
        .bind(id.as_str())
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(call(&client, next).await.is_error, Some(true));
}

#[tokio::test]
async fn media_answer_pagination_preserves_full_history_without_another_model_turn() {
    use stravia_runtime_contract::turn_chain::{TurnNodeId, TurnNodeKind};
    let prefix = "图像内容🙂".repeat(3000);
    let (app, source, calls) = media_test_app_with_answer(&prefix).await;
    let client = connect(&app).await;
    let source_path = format!("stravia://artifacts/{}", source.as_str());
    let first = page(&client, &source_path).await;
    let original = format!("{prefix} [{source_path}]");
    let mut assembled = first["report"]["answer"].as_str().unwrap().to_owned();
    assert!(assembled.len() <= 32 * 1024);
    assert!(
        first["path"]
            .as_str()
            .is_some_and(|path| path.starts_with("stravia://turns/"))
    );
    assert_eq!(first["completion"], "complete");
    assert_eq!(first["artifacts"][0]["path"], source_path);
    assert_eq!(first["report"]["artifacts"][0]["path"], source_path);
    jsonschema::validator_for(&stravia_media::platform::output_schema())
        .unwrap()
        .validate(&first)
        .unwrap();
    let mut next = first["pagination"]["next_path"].as_str().map(str::to_owned);
    while let Some(path) = next {
        let following = page(&client, &path).await;
        assembled.push_str(following["content"].as_str().unwrap());
        next = following["next_path"].as_str().map(str::to_owned);
    }
    assert_eq!(assembled, original);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let nodes = app
        .gateway
        .turn_chains
        .materialize(
            &Principal::new(app.key_id.clone()),
            TurnNodeKind::Agent,
            &TurnNodeId::from_reference(first["path"].as_str().unwrap()).expect("Media Turn path"),
        )
        .await
        .expect("full media history");
    assert_eq!(nodes.last().unwrap().payload["output"]["answer"], original);
    assert_eq!(
        nodes.last().unwrap().payload["output"]["artifacts"][0]["path"],
        source_path
    );
}

#[tokio::test]
async fn untrusted_snapshot_envelopes_are_validated_and_source_truncation_survives_the_last_page() {
    let (app, _, _) = media_test_app().await;
    let client = connect(&app).await;
    let mut unsupported = header();
    unsupported["version"] = json!(99);
    let mut invalid_body = vec![b'a'; 40_000];
    invalid_body.push(0xff);
    for bytes in [
        vec![0, 0, 255, 255],
        frame(unsupported, b"hello"),
        frame(header(), &invalid_body),
        {
            let mut bytes = frame(header(), b"hello");
            bytes.pop();
            bytes
        },
    ] {
        let source = put(&app, SNAPSHOT_MIME, bytes).await;
        assert_eq!(
            call(&client, &source.reference()).await.is_error,
            Some(true)
        );
    }
    let original = "known text\n".repeat(230);
    let mut metadata = header();
    metadata["source_truncated"] = json!(true);
    metadata["source_url"] = json!("http://127.0.0.1:1/unavailable-origin");
    let source = put(&app, SNAPSHOT_MIME, frame(metadata, original.as_bytes())).await;
    let first = page(&client, &source.reference()).await;
    let last = page(&client, first["next_path"].as_str().unwrap()).await;
    assert_eq!(
        format!(
            "{}{}",
            first["content"].as_str().unwrap(),
            last["content"].as_str().unwrap()
        ),
        original
    );
    assert_eq!(last["source_truncated"], true);
    assert_eq!(last["has_more"], false);
    assert!(last.get("next_path").is_none());
    let download = page(&client, &format!("{}?download=1", source.reference())).await;
    let response = reqwest::get(download["download_url"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        response.bytes().await.unwrap().as_ref(),
        original.as_bytes()
    );
}
