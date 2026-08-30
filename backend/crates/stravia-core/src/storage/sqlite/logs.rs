use super::*;

#[derive(Clone)]
pub(super) struct SqliteLogStore {
    pub(super) pool: SqlitePool,
}

#[async_trait]
impl LogStore for SqliteLogStore {
    async fn append_batch(&self, entries: Vec<LogEntry>) -> anyhow::Result<()> {
        for entry in entries {
            let id = uuid::Uuid::new_v4().to_string();
            sqlx::query(
                r#"INSERT INTO request_logs
                    (id, created_at, api_key_id, api_key_name,
                     client_protocol, upstream_protocol, provider_id, provider_name, model_id, model_name, upstream_url,
                     client_model, upstream_model,
                     method, path,
                     client_request_headers, client_request_body,
                     client_response_headers, client_response_body,
                     upstream_request_headers, upstream_request_body,
                     upstream_response_headers, upstream_response_body,
                     upstream_status_code, client_status_code,
                     latency_total_ms, latency_upstream_ms,
                     input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, thinking_level,
                     is_stream, stream_chunks_count, stream_first_chunk_ms)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
            )
            .bind(&id)
            .bind(entry.created_at)
            .bind(&entry.api_key_id)
            .bind(&entry.api_key_name)
            .bind(&entry.client_protocol)
            .bind(&entry.upstream_protocol)
            .bind(&entry.provider_id)
            .bind(&entry.provider_name)
            .bind(&entry.model_id)
            .bind(&entry.model_name)
            .bind(&entry.upstream_url)
            .bind(&entry.client_model)
            .bind(&entry.upstream_model)
            .bind(&entry.method)
            .bind(&entry.path)
            .bind(&entry.client_request_headers)
            .bind(&entry.client_request_body)
            .bind(&entry.client_response_headers)
            .bind(&entry.client_response_body)
            .bind(&entry.upstream_request_headers)
            .bind(&entry.upstream_request_body)
            .bind(&entry.upstream_response_headers)
            .bind(&entry.upstream_response_body)
            .bind(entry.upstream_status_code)
            .bind(entry.client_status_code)
            .bind(entry.latency_total_ms)
            .bind(entry.latency_upstream_ms)
            .bind(entry.input_tokens())
            .bind(entry.output_tokens())
            .bind(entry.cache_read_tokens())
            .bind(entry.cache_write_tokens())
            .bind(entry.thinking_level.map(|level| level.as_str()))
            .bind(entry.is_stream)
            .bind(entry.stream_chunks_count)
            .bind(entry.stream_first_chunk_ms)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    async fn query(&self, query: LogQuery) -> anyhow::Result<LogPage> {
        let mut count_sql = String::from("SELECT COUNT(*) AS total FROM request_logs WHERE 1=1");
        // List query skips the heavy body/header columns (NULL placeholders preserve struct layout).
        let mut data_sql = String::from(
            "SELECT id, COALESCE(CAST(created_at AS INTEGER), 0) AS created_at, api_key_id, api_key_name, \
             client_protocol, upstream_protocol, provider_id, provider_name, model_id, model_name, upstream_url, \
             client_model, upstream_model, method, path, \
             NULL AS client_request_headers, NULL AS client_request_body, \
             NULL AS client_response_headers, NULL AS client_response_body, \
             NULL AS upstream_request_headers, NULL AS upstream_request_body, \
             NULL AS upstream_response_headers, NULL AS upstream_response_body, \
             upstream_status_code, client_status_code, \
             CAST(latency_total_ms AS INTEGER) AS latency_total_ms, latency_upstream_ms, \
             input_tokens, output_tokens, COALESCE(cache_read_tokens, 0) AS cache_read_tokens, \
             COALESCE(cache_write_tokens, 0) AS cache_write_tokens, thinking_level, \
             COALESCE(is_stream, 0) AS is_stream, stream_chunks_count, stream_first_chunk_ms \
             FROM request_logs WHERE 1=1",
        );
        let mut bind_values: Vec<String> = Vec::new();
        if let Some(provider) = query.provider.filter(|v| !v.is_empty()) {
            count_sql.push_str(" AND provider_id = ?");
            data_sql.push_str(" AND provider_id = ?");
            bind_values.push(provider);
        }
        if let Some(model) = query.model.filter(|v| !v.is_empty()) {
            count_sql.push_str(" AND upstream_model = ?");
            data_sql.push_str(" AND upstream_model = ?");
            bind_values.push(model);
        }
        if let Some(status_min) = query.status_min {
            count_sql.push_str(" AND client_status_code >= ?");
            data_sql.push_str(" AND client_status_code >= ?");
            bind_values.push(status_min.to_string());
        }
        if let Some(status_max) = query.status_max {
            count_sql.push_str(" AND client_status_code <= ?");
            data_sql.push_str(" AND client_status_code <= ?");
            bind_values.push(status_max.to_string());
        }
        if let Some(api_key) = query.api_key.filter(|v| !v.is_empty()) {
            count_sql.push_str(" AND api_key_id = ?");
            data_sql.push_str(" AND api_key_id = ?");
            bind_values.push(api_key);
        }

        data_sql.push_str(" ORDER BY created_at DESC LIMIT ? OFFSET ?");
        let mut count_query = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(count_sql));
        let mut data_query = sqlx::query_as::<_, RequestLog>(sqlx::AssertSqlSafe(data_sql));
        for value in &bind_values {
            count_query = count_query.bind(value);
            data_query = data_query.bind(value);
        }
        let total = count_query.fetch_one(&self.pool).await?;
        let items = data_query
            .bind(query.limit.unwrap_or(50))
            .bind(query.offset.unwrap_or(0))
            .fetch_all(&self.pool)
            .await?;
        Ok(LogPage { items, total })
    }

    async fn find_by_id(&self, id: &str) -> anyhow::Result<Option<RequestLog>> {
        let row = sqlx::query_as::<_, RequestLog>(
            "SELECT id, COALESCE(CAST(created_at AS INTEGER), 0) AS created_at, api_key_id, api_key_name, \
             client_protocol, upstream_protocol, provider_id, provider_name, model_id, model_name, upstream_url, \
             client_model, upstream_model, method, path, \
             client_request_headers, client_request_body, \
             client_response_headers, client_response_body, \
             upstream_request_headers, upstream_request_body, \
             upstream_response_headers, upstream_response_body, \
             upstream_status_code, client_status_code, \
             CAST(latency_total_ms AS INTEGER) AS latency_total_ms, latency_upstream_ms, \
             input_tokens, output_tokens, COALESCE(cache_read_tokens, 0) AS cache_read_tokens, \
             COALESCE(cache_write_tokens, 0) AS cache_write_tokens, thinking_level, \
             COALESCE(is_stream, 0) AS is_stream, stream_chunks_count, stream_first_chunk_ms \
             FROM request_logs WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn cleanup_before(&self, cutoff_expression: &str) -> anyhow::Result<u64> {
        // created_at is Unix milliseconds; convert cutoff to ms via strftime.
        let result = sqlx::query(
            "DELETE FROM request_logs WHERE created_at < CAST(strftime('%s', 'now', ?) AS INTEGER) * 1000"
        )
            .bind(cutoff_expression)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    async fn clear_all(&self) -> anyhow::Result<u64> {
        let result = sqlx::query("DELETE FROM request_logs")
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    async fn stats_overview(&self, hours: Option<i64>) -> anyhow::Result<StatsOverview> {
        if let Some(hours) = hours {
            Ok(sqlx::query_as::<_, StatsOverview>(
                "SELECT COUNT(*) AS total_requests, COALESCE(SUM(input_tokens), 0) AS total_input_tokens, COALESCE(SUM(output_tokens), 0) AS total_output_tokens, COALESCE(SUM(cache_read_tokens), 0) AS total_cache_read_tokens, COALESCE(SUM(cache_write_tokens), 0) AS total_cache_write_tokens, COALESCE(AVG(latency_total_ms), 0.0) AS avg_duration_ms, AVG(stream_first_chunk_ms) AS avg_first_token_ms, COALESCE(SUM(CASE WHEN client_status_code >= 400 THEN 1 ELSE 0 END), 0) AS error_count FROM request_logs WHERE created_at >= CAST(strftime('%s', 'now', ?) AS INTEGER) * 1000",
            )
            .bind(format!("-{hours} hours"))
            .fetch_one(&self.pool)
            .await?)
        } else {
            Ok(sqlx::query_as::<_, StatsOverview>(
                "SELECT COUNT(*) AS total_requests, COALESCE(SUM(input_tokens), 0) AS total_input_tokens, COALESCE(SUM(output_tokens), 0) AS total_output_tokens, COALESCE(SUM(cache_read_tokens), 0) AS total_cache_read_tokens, COALESCE(SUM(cache_write_tokens), 0) AS total_cache_write_tokens, COALESCE(AVG(latency_total_ms), 0.0) AS avg_duration_ms, AVG(stream_first_chunk_ms) AS avg_first_token_ms, COALESCE(SUM(CASE WHEN client_status_code >= 400 THEN 1 ELSE 0 END), 0) AS error_count FROM request_logs",
            )
            .fetch_one(&self.pool)
            .await?)
        }
    }

    async fn stats_hourly(&self, hours: i64) -> anyhow::Result<Vec<StatsHourly>> {
        Ok(sqlx::query_as::<_, StatsHourly>(
            "SELECT strftime('%Y-%m-%d %H:00:00', datetime(created_at/1000, 'unixepoch')) AS hour, COUNT(*) AS request_count, COALESCE(SUM(CASE WHEN client_status_code >= 400 THEN 1 ELSE 0 END), 0) AS error_count, COALESCE(SUM(input_tokens), 0) AS total_input_tokens, COALESCE(SUM(output_tokens), 0) AS total_output_tokens, COALESCE(SUM(cache_read_tokens), 0) AS total_cache_read_tokens, COALESCE(SUM(cache_write_tokens), 0) AS total_cache_write_tokens, COALESCE(AVG(latency_total_ms), 0.0) AS avg_duration_ms, AVG(stream_first_chunk_ms) AS avg_first_token_ms FROM request_logs WHERE created_at >= CAST(strftime('%s', 'now', ?) AS INTEGER) * 1000 GROUP BY hour ORDER BY hour ASC",
        )
        .bind(format!("-{hours} hours"))
        .fetch_all(&self.pool)
        .await?)
    }

    async fn stats_by_model(&self, hours: Option<i64>) -> anyhow::Result<Vec<ModelStats>> {
        if let Some(hours) = hours {
            Ok(sqlx::query_as::<_, ModelStats>(
                "SELECT COALESCE(NULLIF(model_name, ''), NULLIF(client_model, ''), NULLIF(model_id, ''), '') AS model, COUNT(*) AS request_count, COALESCE(SUM(input_tokens), 0) AS total_input_tokens, COALESCE(SUM(output_tokens), 0) AS total_output_tokens, COALESCE(AVG(latency_total_ms), 0.0) AS avg_duration_ms FROM request_logs WHERE created_at >= CAST(strftime('%s', 'now', ?) AS INTEGER) * 1000 GROUP BY COALESCE(NULLIF(model_name, ''), NULLIF(client_model, ''), NULLIF(model_id, ''), '') ORDER BY request_count DESC",
            )
            .bind(format!("-{hours} hours"))
            .fetch_all(&self.pool)
            .await?)
        } else {
            Ok(sqlx::query_as::<_, ModelStats>(
                "SELECT COALESCE(NULLIF(model_name, ''), NULLIF(client_model, ''), NULLIF(model_id, ''), '') AS model, COUNT(*) AS request_count, COALESCE(SUM(input_tokens), 0) AS total_input_tokens, COALESCE(SUM(output_tokens), 0) AS total_output_tokens, COALESCE(AVG(latency_total_ms), 0.0) AS avg_duration_ms FROM request_logs GROUP BY COALESCE(NULLIF(model_name, ''), NULLIF(client_model, ''), NULLIF(model_id, ''), '') ORDER BY request_count DESC",
            )
            .fetch_all(&self.pool)
            .await?)
        }
    }

    async fn stats_by_provider(&self, hours: Option<i64>) -> anyhow::Result<Vec<ProviderStats>> {
        if let Some(hours) = hours {
            Ok(sqlx::query_as::<_, ProviderStats>(
                "SELECT COALESCE(provider_name, provider_id, '') AS provider, COUNT(*) AS request_count, COALESCE(SUM(CASE WHEN client_status_code >= 400 THEN 1 ELSE 0 END), 0) AS error_count, COALESCE(AVG(latency_total_ms), 0.0) AS avg_duration_ms FROM request_logs WHERE created_at >= CAST(strftime('%s', 'now', ?) AS INTEGER) * 1000 GROUP BY COALESCE(provider_name, provider_id, '') ORDER BY request_count DESC",
            )
            .bind(format!("-{hours} hours"))
            .fetch_all(&self.pool)
            .await?)
        } else {
            Ok(sqlx::query_as::<_, ProviderStats>(
                "SELECT COALESCE(provider_name, provider_id, '') AS provider, COUNT(*) AS request_count, COALESCE(SUM(CASE WHEN client_status_code >= 400 THEN 1 ELSE 0 END), 0) AS error_count, COALESCE(AVG(latency_total_ms), 0.0) AS avg_duration_ms FROM request_logs GROUP BY COALESCE(provider_name, provider_id, '') ORDER BY request_count DESC",
            )
            .fetch_all(&self.pool)
            .await?)
        }
    }

    async fn stats_by_api_key(&self, hours: Option<i64>) -> anyhow::Result<Vec<ApiKeyStats>> {
        if let Some(hours) = hours {
            Ok(sqlx::query_as::<_, ApiKeyStats>(
                "SELECT COALESCE(api_key_id, '') AS api_key_id, COALESCE(MAX(api_key_name), api_key_id, '') AS api_key_name, COUNT(*) AS request_count, COALESCE(SUM(input_tokens), 0) AS total_input_tokens, COALESCE(SUM(output_tokens), 0) AS total_output_tokens, COALESCE(SUM(cache_read_tokens), 0) AS cache_read_tokens, COALESCE(SUM(cache_write_tokens), 0) AS cache_write_tokens, MAX(created_at) AS last_used_at FROM request_logs WHERE api_key_id IS NOT NULL AND api_key_id <> '' AND created_at >= CAST(strftime('%s', 'now', ?) AS INTEGER) * 1000 GROUP BY api_key_id ORDER BY request_count DESC",
            )
            .bind(format!("-{hours} hours"))
            .fetch_all(&self.pool)
            .await?)
        } else {
            Ok(sqlx::query_as::<_, ApiKeyStats>(
                "SELECT COALESCE(api_key_id, '') AS api_key_id, COALESCE(MAX(api_key_name), api_key_id, '') AS api_key_name, COUNT(*) AS request_count, COALESCE(SUM(input_tokens), 0) AS total_input_tokens, COALESCE(SUM(output_tokens), 0) AS total_output_tokens, COALESCE(SUM(cache_read_tokens), 0) AS cache_read_tokens, COALESCE(SUM(cache_write_tokens), 0) AS cache_write_tokens, MAX(created_at) AS last_used_at FROM request_logs WHERE api_key_id IS NOT NULL AND api_key_id <> '' GROUP BY api_key_id ORDER BY request_count DESC",
            )
            .fetch_all(&self.pool)
            .await?)
        }
    }
}
