use super::*;

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
    let answer = format!("{} [source-wst_delivery-1]", "verified fact\n".repeat(220));
    let complete = json!({
        "turn_id":"wst_delivery", "completion":"complete",
        "report":{
            "answer":answer,
            "sources":[{"id":"source-wst_delivery-1","url":"https://8.8.8.8/article","title":"Source"}],
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
