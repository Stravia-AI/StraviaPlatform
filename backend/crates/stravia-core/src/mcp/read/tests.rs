use super::*;

fn office_docx_bytes(text: &str) -> bytes::Bytes {
    use office_oxide::ir::{DocumentIR, Element, InlineContent, Paragraph, Section, TextSpan};
    let ir = DocumentIR {
        metadata: office_oxide::ir::Metadata {
            format: office_oxide::DocumentFormat::Docx,
            ..Default::default()
        },
        sections: vec![Section {
            elements: text
                .lines()
                .map(|line| {
                    Element::Paragraph(Paragraph {
                        content: vec![InlineContent::Text(TextSpan {
                            text: line.to_owned(),
                            ..Default::default()
                        })],
                        ..Default::default()
                    })
                })
                .collect(),
            ..Default::default()
        }],
    };
    let mut out = std::io::Cursor::new(Vec::new());
    office_oxide::create::create_from_ir_to_writer(
        &ir,
        office_oxide::DocumentFormat::Docx,
        &mut out,
    )
    .expect("synthesize docx");
    bytes::Bytes::from(out.into_inner())
}

async fn document_fixture() -> (
    tempfile::TempDir,
    ReadTool,
    ToolExecutionContext,
    ArtifactRef,
) {
    let directory = tempfile::tempdir().unwrap();
    let gateway = crate::Gateway::new(crate::config::GatewayConfig {
        data_dir: directory.path().to_owned(),
        ..Default::default()
    })
    .await
    .unwrap();
    gateway
        .admin()
        .set_setting(
            "artifact_settings",
            &serde_json::to_string(&ArtifactSettings {
                client_base_url: "http://localhost".to_owned(),
                ..Default::default()
            })
            .expect("Artifact settings"),
        )
        .await
        .expect("configure Artifact downloads");
    let reader = ReadTool::new(&gateway, &mut Vec::new()).unwrap();
    let key = gateway
        .admin()
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "read key".into(),
            concurrency_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: true,
            inject_web_search: true,
            model_ids: vec![],
            inject_media_understanding: false,
        })
        .await
        .expect("API key");
    let context = ToolExecutionContext {
        request_id: "doc".into(),
        run_id: "doc".into(),
        principal: Principal::new(key.id),
        read_scope: ReadExposureScope::FULL,
        cancellation: CancellationToken::new(),
        progress: None,
    };
    let bytes = office_docx_bytes("Quarterly report heading");
    let artifact = gateway
        .artifact_store()
        .unwrap()
        .ingest(
            &context.principal,
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            Some(bytes.len() as u64),
            stravia_runtime_contract::artifact::bytes_stream(bytes),
            Duration::from_secs(3600),
        )
        .await
        .unwrap();
    (directory, reader, context, artifact)
}

#[tokio::test]
async fn office_document_reads_as_extracted_markdown() {
    let (_dir, reader, context, artifact) = document_fixture().await;
    let result = reader
        .read(
            json!({"path": format!("sa:{}", artifact.id.as_str())}),
            context,
        )
        .await
        .unwrap();
    assert!(!result.is_error);
    let (value, _) = crate::hook::tool::blocks_to_value(result.content).unwrap();
    assert_eq!(value["representation"], "markdown");
    assert!(
        value["content"]
            .as_str()
            .unwrap()
            .contains("Quarterly report heading"),
        "extracted Markdown carries the document text: {value}"
    );
    assert_eq!(value["has_more"], false);
}

#[tokio::test]
async fn office_document_lines_select_from_extracted_markdown() {
    let (_dir, reader, context, _) = document_fixture().await;
    let bytes = office_docx_bytes("first heading\nsecond paragraph");
    let artifact = reader
        .gateway
        .artifact_store()
        .unwrap()
        .ingest(
            &context.principal,
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            Some(bytes.len() as u64),
            stravia_runtime_contract::artifact::bytes_stream(bytes),
            Duration::from_secs(3600),
        )
        .await
        .unwrap();
    let result = reader
        .read(
            json!({"path": format!("sa:{}?lines=1-1", artifact.id.as_str())}),
            context,
        )
        .await
        .unwrap();
    assert!(!result.is_error);
    let (value, _) = crate::hook::tool::blocks_to_value(result.content).unwrap();
    let content = value["content"].as_str().unwrap();
    assert!(
        content.contains("first heading") && !content.contains("second paragraph"),
        "line selection reads from the extracted Markdown: {value}"
    );
}

#[tokio::test]
async fn office_document_download_skips_extraction() {
    let (_dir, reader, context, _) = document_fixture().await;
    // Bytes that fail container preflight still download: ?download=1 must not
    // run extraction or the model path.
    let corrupt = gateway_corrupt_docx(&reader, &context).await;
    let result = reader
        .read(
            json!({"path": format!("sa:{}?download=1", corrupt.id.as_str())}),
            context.clone(),
        )
        .await
        .unwrap();
    assert!(!result.is_error);
    let (value, _) = crate::hook::tool::blocks_to_value(result.content).unwrap();
    assert!(
        value["download_url"].is_string(),
        "download output: {value}"
    );

    // The same corrupt Artifact without ?download fails as a document error.
    let result = reader
        .read(
            json!({"path": format!("sa:{}", corrupt.id.as_str())}),
            context,
        )
        .await
        .unwrap();
    assert!(result.is_error);
    let (value, _) = crate::hook::tool::blocks_to_value(result.content).unwrap();
    assert_eq!(value["error"]["code"], "media_document_invalid");
}

async fn gateway_corrupt_docx(reader: &ReadTool, context: &ToolExecutionContext) -> ArtifactRef {
    let mut corrupt = b"PK\x03\x04".to_vec();
    corrupt.extend_from_slice(&[0u8; 128]);
    reader
        .gateway
        .artifact_store()
        .unwrap()
        .ingest(
            &context.principal,
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            Some(corrupt.len() as u64),
            stravia_runtime_contract::artifact::bytes_stream(bytes::Bytes::from(corrupt)),
            Duration::from_secs(3600),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn office_document_rejects_raw() {
    let (_dir, reader, context, artifact) = document_fixture().await;
    let error = reader
        .read(
            json!({"path": format!("sa:{}?raw=1", artifact.id.as_str())}),
            context,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("raw"), "raw rejection: {error}");
}

#[tokio::test]
async fn office_document_question_routes_to_media_understanding() {
    let (_dir, reader, context, artifact) = document_fixture().await;
    // No media service is configured on a bare Gateway: reaching Media
    // Understanding (rather than extraction or file bytes) surfaces its
    // availability error.
    let error = reader
        .read(
            json!({"path": format!("sa:{}?question=summary", artifact.id.as_str())}),
            context,
        )
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "Media Understanding is unavailable");
}

#[tokio::test]
async fn search_report_delivery_keeps_sources_and_continues_only_the_answer() {
    let directory = tempfile::tempdir().unwrap();
    let gateway = crate::Gateway::new(crate::config::GatewayConfig {
        data_dir: directory.path().to_owned(),
        ..Default::default()
    })
    .await
    .unwrap();
    let reader = ReadTool::new(&gateway, &mut Vec::new()).unwrap();
    let context = ToolExecutionContext {
        request_id: "delivery".into(),
        run_id: "delivery".into(),
        principal: Principal::new("owner"),
        read_scope: ReadExposureScope::FULL,
        cancellation: CancellationToken::new(),
        progress: None,
    };
    let turn_id = "abcdefghijklmnopqrstuvwxyzab";
    let source_id = format!("{turn_id}:1");
    let answer = format!("{} [sc:{source_id}]", "verified fact\n".repeat(220));
    let complete = json!({
        "turn_id":turn_id, "completion":"complete",
        "report":{
            "answer":answer,
            "sources":[{"id":source_id,"url":"https://8.8.8.8/article","title":"Source"}],
            "limitations":["The source describes only the current version."]
        }
    });
    let projected = reader
        .paginate_report(output(complete.clone()), &context)
        .await
        .unwrap();
    let (first, _) = crate::hook::tool::blocks_to_value(projected.content).unwrap();
    jsonschema::validator_for(&stravia_web_search::platform::output_schema())
        .unwrap()
        .validate(&first)
        .unwrap();
    assert_eq!(first["turn_id"], complete["turn_id"]);
    assert_eq!(first["report"]["sources"], complete["report"]["sources"]);
    assert_eq!(
        first["report"]["limitations"],
        complete["report"]["limitations"]
    );
    let ReadTarget::Resource(next) =
        parse_read_path(first["pagination"]["next_path"].as_str().unwrap()).unwrap()
    else {
        panic!("snapshot path")
    };
    let id = ArtifactId::from_reference(&next.url).unwrap();
    let opened = gateway
        .artifact_store()
        .unwrap()
        .open(&context.principal, &id)
        .await
        .unwrap();
    let last = text::read(opened, &next.options).await.unwrap();
    assert_eq!(
        format!(
            "{}{}",
            first["report"]["answer"].as_str().unwrap(),
            last["content"].as_str().unwrap()
        ),
        answer
    );
    assert_eq!(last["has_more"], false);
    assert!(last.get("report").is_none());
}
