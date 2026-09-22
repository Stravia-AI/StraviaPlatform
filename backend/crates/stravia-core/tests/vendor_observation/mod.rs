use std::io::Read;

use futures::StreamExt;
use stravia_core::Gateway;
use stravia_core::admin::{
    BundleRequest, BundleResourceKind, ObservationStream, ObservationUpdate,
};

pub(super) async fn finished_observation(
    observations: &mut ObservationStream,
) -> anyhow::Result<(String, String)> {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut live = String::new();
        while let Some(update) = observations.next().await {
            match update {
                ObservationUpdate::LiveContent(block) => live.push_str(&block.text),
                ObservationUpdate::Event(event) => {
                    // 准备阶段失败只有结构化事件，不能仅检查已开始输出的文本块。
                    live.push_str(&serde_json::to_string(&event)?);
                    if event.kind == "run_finished" {
                        return Ok((
                            event
                                .interaction_id
                                .expect("finished run has an interaction"),
                            live,
                        ));
                    }
                }
                _ => {}
            }
        }
        anyhow::bail!("observation stream ended before the run finished")
    })
    .await?
}

pub(super) async fn observation_bundle_records(
    gateway: &Gateway,
    interaction_id: &str,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let admin = gateway.admin();
    let ticket = admin
        .issue_observation_bundle_ticket(BundleRequest {
            kind: BundleResourceKind::Interaction,
            resource_id: interaction_id.to_owned(),
            through_sequence: None,
        })
        .await?;
    let token = ticket
        .download_url
        .rsplit('/')
        .next()
        .expect("bundle ticket URL contains its token");
    let mut stream = admin.consume_observation_bundle_ticket(token).await?;
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        bytes.extend_from_slice(&chunk?);
    }
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    let mut records = Vec::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        if !entry.name().ends_with("/events.jsonl") {
            continue;
        }
        let mut text = String::new();
        entry.read_to_string(&mut text)?;
        records.extend(
            text.lines()
                .map(serde_json::from_str)
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    Ok(records)
}
