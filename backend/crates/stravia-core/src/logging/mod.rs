use tokio::sync::mpsc;

use crate::protocol::ir::Usage;
use crate::storage::DynStorage;

const DEFAULT_RETENTION_DAYS: i64 = 7;
pub const LOG_RETENTION_DAYS_KEY: &str = "log_retention_days";

#[derive(Debug, Clone)]
pub struct LogEntry {
    // === 标识 ===
    pub api_key_id: Option<String>,
    pub api_key_name: Option<String>,
    /// Unix 毫秒时间戳
    pub created_at: i64,

    // === 模型 ===
    pub client_protocol: String,
    pub upstream_protocol: String,
    pub provider_id: String,
    pub provider_name: String,
    pub model_id: Option<String>,
    pub model_name: Option<String>,
    pub upstream_url: Option<String>,
    pub client_model: String,
    pub upstream_model: String,

    // === HTTP 元 ===
    pub method: Option<String>,
    pub path: Option<String>,

    // === 客户端 wire ===
    pub client_request_headers: Option<String>,
    pub client_request_body: Option<String>,
    pub client_response_headers: Option<String>,
    pub client_response_body: Option<String>,

    // === 上游 wire ===
    pub upstream_request_headers: Option<String>,
    pub upstream_request_body: Option<String>,
    pub upstream_response_headers: Option<String>,
    pub upstream_response_body: Option<String>,

    // === 状态 ===
    pub upstream_status_code: Option<i32>,
    pub client_status_code: i32,

    // === 性能 ===
    pub latency_total_ms: i64,
    pub latency_upstream_ms: Option<i64>,
    pub usage: Usage,
    pub thinking_level: Option<crate::thinking::ThinkingLevel>,

    // === 流式 ===
    /// 客户端请求中声明的 stream 标志（stream: true），比 stream_chunks_count > 0 更严谨
    pub is_stream: bool,
    /// 收到的上游流 chunk 数；> 0 表示上游使用流式传输，非流式为 0
    pub stream_chunks_count: i32,
    /// 从上游流调用开始到首个 chunk 的延迟（ms）；非流式为 None
    pub stream_first_chunk_ms: Option<i64>,
}

impl LogEntry {
    pub fn input_tokens(&self) -> i32 {
        self.usage.prompt_tokens as i32
    }

    pub fn output_tokens(&self) -> i32 {
        self.usage.completion_tokens as i32
    }

    pub fn cache_read_tokens(&self) -> i32 {
        self.usage.cache_read_tokens.unwrap_or(0) as i32
    }

    pub fn cache_write_tokens(&self) -> i32 {
        self.usage.cache_creation_tokens.unwrap_or(0) as i32
    }
}

pub async fn run_collector(mut rx: mpsc::Receiver<LogEntry>, storage: DynStorage) {
    let mut buffer: Vec<LogEntry> = Vec::with_capacity(32);
    let mut flush_interval = tokio::time::interval(std::time::Duration::from_secs(2));
    let mut cleanup_interval = tokio::time::interval(std::time::Duration::from_secs(600));

    loop {
        tokio::select! {
            Some(entry) = rx.recv() => {
                buffer.push(entry);
                if buffer.len() >= 32 {
                    flush(storage.clone(), &mut buffer).await;
                }
            }
            _ = flush_interval.tick() => {
                if !buffer.is_empty() {
                    flush(storage.clone(), &mut buffer).await;
                }
            }
            _ = cleanup_interval.tick() => {
                cleanup_old_logs(storage.clone()).await;
            }
        }
    }
}

async fn cleanup_old_logs(storage: DynStorage) {
    let days = storage
        .settings()
        .get(LOG_RETENTION_DAYS_KEY)
        .await
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_RETENTION_DAYS);

    let cutoff = format!("-{days} days");
    if let Ok(deleted) = storage.logs().cleanup_before(&cutoff).await
        && deleted > 0
    {
        tracing::info!("cleaned up {deleted} logs older than {days} days");
    }
}

async fn flush(storage: DynStorage, buffer: &mut Vec<LogEntry>) {
    let entries = std::mem::take(buffer);
    let _ = storage.logs().append_batch(entries).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::LogQuery;
    use crate::{Gateway, config::GatewayConfig};

    fn payload_entry() -> LogEntry {
        LogEntry {
            api_key_id: None,
            api_key_name: None,
            created_at: chrono::Utc::now().timestamp_millis(),
            client_protocol: "openai-chat".into(),
            upstream_protocol: "openai-chat".into(),
            provider_id: "provider".into(),
            provider_name: "Provider".into(),
            model_id: Some("model".into()),
            model_name: Some("Model".into()),
            upstream_url: Some("https://provider.example/v1/chat/completions".into()),
            client_model: "client-model".into(),
            upstream_model: "upstream-model".into(),
            method: Some("POST".into()),
            path: Some("/v1/chat/completions".into()),
            client_request_headers: Some(r#"{"client-request":"header"}"#.into()),
            client_request_body: Some(r#"{"client-request":"body"}"#.into()),
            client_response_headers: Some(r#"{"client-response":"header"}"#.into()),
            client_response_body: Some(r#"{"client-response":"body"}"#.into()),
            upstream_request_headers: Some(r#"{"upstream-request":"header"}"#.into()),
            upstream_request_body: Some(r#"{"upstream-request":"body"}"#.into()),
            upstream_response_headers: Some(r#"{"upstream-response":"header"}"#.into()),
            upstream_response_body: Some(r#"{"upstream-response":"body"}"#.into()),
            upstream_status_code: Some(200),
            client_status_code: 200,
            latency_total_ms: 10,
            latency_upstream_ms: Some(8),
            usage: Usage {
                prompt_tokens: 11,
                completion_tokens: 7,
                cache_read_tokens: Some(3),
                cache_creation_tokens: Some(4),
                ..Usage::default()
            },
            thinking_level: Some(crate::thinking::ThinkingLevel::High),
            is_stream: false,
            stream_chunks_count: 0,
            stream_first_chunk_ms: None,
        }
    }

    #[tokio::test]
    async fn flush_persists_payloads_unconditionally() {
        let data_dir = tempfile::tempdir().expect("temporary data directory");
        let (gateway, _logs) = Gateway::new(GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .expect("gateway");

        let mut buffer = vec![payload_entry()];
        flush(gateway.storage.clone(), &mut buffer).await;

        assert!(buffer.is_empty());
        let page = gateway
            .storage
            .logs()
            .query(LogQuery::default())
            .await
            .expect("request log page");
        let stored = gateway
            .storage
            .logs()
            .find_by_id(&page.items.first().expect("request log").id)
            .await
            .expect("request log lookup")
            .expect("stored request log");
        assert_eq!(
            stored.client_request_body.as_deref(),
            Some(r#"{"client-request":"body"}"#)
        );
        assert_eq!(
            stored.client_response_body.as_deref(),
            Some(r#"{"client-response":"body"}"#)
        );
        assert_eq!(
            stored.upstream_request_body.as_deref(),
            Some(r#"{"upstream-request":"body"}"#)
        );
        assert_eq!(
            stored.upstream_response_body.as_deref(),
            Some(r#"{"upstream-response":"body"}"#)
        );
        assert_eq!(stored.input_tokens, 11);
        assert_eq!(stored.output_tokens, 7);
        assert_eq!(stored.cache_read_tokens, 3);
        assert_eq!(stored.cache_write_tokens, 4);
        assert_eq!(stored.thinking_level.as_deref(), Some("high"));
    }

    #[tokio::test]
    async fn model_stats_group_by_logical_model_with_legacy_client_model_fallback() {
        let data_dir = tempfile::tempdir().expect("temporary data directory");
        let (gateway, _logs) = Gateway::new(GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .expect("gateway");

        let mut current = payload_entry();
        current.model_name = Some("Logical model".into());
        current.client_model = "logical-route".into();
        current.upstream_model = "upstream-a".into();
        let mut legacy = payload_entry();
        legacy.model_id = None;
        legacy.model_name = None;
        legacy.client_model = "legacy-logical-route".into();
        legacy.upstream_model = "upstream-b".into();
        gateway
            .storage
            .logs()
            .append_batch(vec![current, legacy])
            .await
            .expect("append request logs");

        let stats = gateway
            .storage
            .logs()
            .stats_by_model(None)
            .await
            .expect("model stats");

        let mut models = stats
            .iter()
            .map(|item| (item.model.as_str(), item.request_count))
            .collect::<Vec<_>>();
        models.sort_unstable();
        assert_eq!(models, [("Logical model", 1), ("legacy-logical-route", 1)]);
    }
}
