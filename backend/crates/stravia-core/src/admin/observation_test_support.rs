//! 仅供跨 crate 路由测试使用的 Observation 播种入口（`test-support` 特性）。
//!
//! 数据走与生产一致的 `RunObserver` 写入路径与凭据映射存储，不绕过内容存储；
//! 这里只负责把“某主体发出了一条用户输入”这件事落库并返回可被引用的内容 ID。

use std::sync::Arc;

use stravia_runtime_contract::Principal;
use stravia_runtime_contract::protocol::ir::{AiItem, AiRequest, MessageContent, Role};

use super::AdminService;
use crate::interaction_observation::{
    AdmissionFacts, ForestQuery, IngressStart, RunOutcome, RunStart,
};

/// 播种结果：新 Interaction 的 ID 及其输入摘要引用的内容 ID。
#[derive(Debug, Clone)]
pub struct SeededInteraction {
    pub interaction_id: String,
    pub input_content_ids: Vec<i64>,
}

impl AdminService {
    /// 以 `principal` 记录一条已完成的用户输入，并等待写入落库。
    pub async fn seed_observation_input(
        &self,
        principal: &str,
        text: &str,
    ) -> anyhow::Result<SeededInteraction> {
        let run_id = format!("seed-{}", uuid::Uuid::new_v4().simple());
        let route_id = format!("route-{run_id}");
        let item = AiItem {
            role: Role::User,
            content: MessageContent::Text(Arc::new(text.into())),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        };
        let run = self
            .gw
            .observation
            .observe_ingress(IngressStart {
                id: run_id.clone(),
                method: "POST".into(),
                path: "/v1/responses".into(),
                protocol: "responses".into(),
            })
            .admit(
                RunStart {
                    id: run_id.clone(),
                    principal: principal.into(),
                    api_key_id: None,
                    api_key_name: None,
                    route_id: route_id.clone(),
                    model_display_name: None,
                    ingress_protocol: "responses".into(),
                },
                AdmissionFacts {
                    client_request: Arc::new(AiRequest::new("model", vec![item.clone()])),
                    has_new_user: true,
                    has_matching_pending_tool_result: false,
                    generation_root_id: None,
                    generation_parent_id: None,
                },
            );
        run.capture_user_input(&[item]);
        run.publish_user_input();
        run.finish(RunOutcome {
            client_output_committed: true,
            delivery: None,
            delivery_completed_at: None,
            status: "completed".into(),
            terminal_reason: None,
            generation_node_id: None,
            generation_root_id: None,
        });
        self.gw.observation.flush().await?;
        let forest = self
            .gw
            .observation
            .query_forest(ForestQuery::default())
            .await?;
        let summary = forest
            .roots
            .iter()
            .flat_map(|root| &root.interactions)
            .find(|interaction| interaction.first_route_id == route_id)
            .ok_or_else(|| anyhow::anyhow!("seeded interaction is not visible"))?;
        Ok(SeededInteraction {
            interaction_id: summary.id.clone(),
            input_content_ids: summary
                .input
                .refs
                .iter()
                .map(|reference| reference.content_id)
                .collect(),
        })
    }

    /// 为 `principal` 登记一条有效的凭据映射，使其观察正文在读取时被遮蔽。
    pub async fn seed_credential_mapping(
        &self,
        principal: &str,
        secret: &str,
    ) -> anyhow::Result<()> {
        self.gw
            .redaction
            .mappings
            .intern(&Principal::new(principal), &[secret.into()])
            .await?;
        Ok(())
    }
}
