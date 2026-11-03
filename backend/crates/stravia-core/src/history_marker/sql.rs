use super::*;

use sqlx::FromRow;

macro_rules! backend {
    ($store:expr, $pool:ident, $body:block) => {
        match $store {
            SqlHistoryMarkerStore::Sqlite($pool) => $body,
            SqlHistoryMarkerStore::Postgres($pool) => $body,
        }
    };
}

fn process_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(stravia_runtime_contract::identifier::new_id)
}

#[derive(Clone)]
pub enum SqlHistoryMarkerStore {
    Sqlite(sqlx::SqlitePool),
    Postgres(sqlx::PgPool),
}

#[derive(FromRow)]
struct MarkerRow {
    reference: String,
    kind: String,
    activity: String,
    call_payload: Option<String>,
    segment_payload: Option<Vec<u8>>,
    execution_state: Option<String>,
    lease_expires_at: Option<i64>,
    execution_deadline: Option<i64>,
    published_at: Option<i64>,
}

impl SqlHistoryMarkerStore {
    pub fn sqlite(pool: sqlx::SqlitePool) -> Self {
        Self::Sqlite(pool)
    }

    pub fn postgres(pool: sqlx::PgPool) -> Self {
        Self::Postgres(pool)
    }

    fn now() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    fn after(now: i64, duration: Duration) -> i64 {
        now.saturating_add(i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
    }

    async fn insert_thinking(
        &self,
        principal: &Principal,
        reference: String,
        input: ThinkingMarkerInput,
    ) -> Result<HistoryMarker, HistoryMarkerError> {
        if !validate_activity(&input.activity)
            || !validate_thinking(&input.block)
            || !valid_reference(&reference)
        {
            return Err(HistoryMarkerError::InvalidPayload);
        }
        let principal = principal.continuation_key();
        let segment = HiddenHistorySegment::Thinking {
            block: input.block,
            source: input.source,
        };
        let payload = super::content::encode_segment(&segment).map_err(segment_storage)?;
        let now = Self::now();
        let pending_until = Self::after(now, PENDING_RETENTION);
        match self {
            Self::Sqlite(pool) => {
                let mut connection = pool.acquire_write().await.map_err(storage)?;
                let mut transaction = connection.begin().await.map_err(storage)?;
                let (envelope, ids) =
                    super::payload::sqlite::intern_segment(&mut transaction, &principal, payload)
                        .await
                        .map_err(segment_storage)?;
                sqlx::query(
                    "INSERT INTO history_markers \
                     (reference, principal, kind, activity, segment_payload, created_at, updated_at, pending_until) \
                     VALUES (?, ?, 'thinking', ?, ?, ?, ?, ?)",
                )
                .bind(&reference)
                .bind(principal)
                .bind(&input.activity)
                .bind(&envelope)
                .bind(now)
                .bind(now)
                .bind(pending_until)
                .execute(&mut *transaction)
                .await
                .map_err(storage)?;
                super::payload::sqlite::replace_refs(&mut transaction, &reference, &ids)
                    .await
                    .map_err(segment_storage)?;
                transaction.commit().await.map_err(storage)?;
                connection.finish().await.map_err(storage)?;
            }
            Self::Postgres(pool) => {
                let mut transaction = pool.begin().await.map_err(storage)?;
                let (envelope, ids) =
                    super::payload::postgres::intern_segment(&mut transaction, &principal, payload)
                        .await
                        .map_err(segment_storage)?;
                sqlx::query(
                    "INSERT INTO history_markers \
                     (reference, principal, kind, activity, segment_payload, created_at, updated_at, pending_until) \
                     VALUES ($1, $2, 'thinking', $3, $4, $5, $5, $6)",
                )
                .bind(&reference)
                .bind(principal)
                .bind(&input.activity)
                .bind(&envelope)
                .bind(now)
                .bind(pending_until)
                .execute(&mut *transaction)
                .await
                .map_err(storage)?;
                super::payload::postgres::replace_refs(&mut transaction, &reference, &ids)
                    .await
                    .map_err(segment_storage)?;
                transaction.commit().await.map_err(storage)?;
            }
        }
        Ok(HistoryMarker {
            reference,
            kind: HistoryMarkerKind::Thinking,
            activity: input.activity,
        })
    }

    async fn row(
        &self,
        principal: &str,
        reference: &str,
    ) -> Result<Option<MarkerRow>, HistoryMarkerError> {
        let result = match self {
            Self::Sqlite(pool) => sqlx::query_as::<_, MarkerRow>(
                "SELECT reference, kind, activity, call_payload, segment_payload, execution_state, \
         lease_expires_at, execution_deadline, published_at \
         FROM history_markers WHERE principal = ? AND reference = ? \
         AND NOT EXISTS (SELECT 1 FROM public_tool_executions p WHERE p.marker_reference = history_markers.reference AND p.expires_at <= ?)",
            )
            .bind(principal)
            .bind(reference)
            .bind(Self::now())
            .fetch_optional(pool)
            .await,
            Self::Postgres(pool) => sqlx::query_as::<_, MarkerRow>(
                "SELECT reference, kind, activity, call_payload, segment_payload, execution_state, \
         lease_expires_at, execution_deadline, published_at \
         FROM history_markers WHERE principal = $1 AND reference = $2 \
         AND NOT EXISTS (SELECT 1 FROM public_tool_executions p WHERE p.marker_reference = history_markers.reference AND p.expires_at <= $3)",
            )
            .bind(principal)
            .bind(reference)
            .bind(Self::now())
            .fetch_optional(pool)
            .await,
        };
        result.map_err(storage)
    }

    async fn rows(
        &self,
        principal: &Principal,
        references: &[String],
    ) -> Result<Vec<MarkerRow>, HistoryMarkerError> {
        if references.is_empty() {
            return Ok(Vec::new());
        }
        let principal = principal.continuation_key();
        let now = Self::now();
        const SELECT: &str = "SELECT reference, kind, activity, call_payload, segment_payload, \
     execution_state, lease_expires_at, execution_deadline, published_at \
     FROM history_markers WHERE principal = ";
        let result = match self {
            Self::Sqlite(pool) => {
                let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(SELECT);
                query.push_bind(&principal).push(" AND reference IN (");
                let mut separated = query.separated(", ");
                for reference in references {
                    separated.push_bind(reference);
                }
                separated.push_unseparated(") AND NOT EXISTS (SELECT 1 FROM public_tool_executions p WHERE p.marker_reference = history_markers.reference AND p.expires_at <= ");
                query.push_bind(now).push(")");
                query.build_query_as::<MarkerRow>().fetch_all(pool).await
            }
            Self::Postgres(pool) => {
                let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new(SELECT);
                query.push_bind(&principal).push(" AND reference IN (");
                let mut separated = query.separated(", ");
                for reference in references {
                    separated.push_bind(reference);
                }
                separated.push_unseparated(") AND NOT EXISTS (SELECT 1 FROM public_tool_executions p WHERE p.marker_reference = history_markers.reference AND p.expires_at <= ");
                query.push_bind(now).push(")");
                query.build_query_as::<MarkerRow>().fetch_all(pool).await
            }
        };
        result.map_err(storage)
    }

    /// 批量恢复段载荷；同一 Principal 的引用与正文各一次查询。
    async fn decode_rows(
        &self,
        principal: &str,
        rows: Vec<MarkerRow>,
    ) -> Result<Vec<ResolvedHistoryMarker>, HistoryMarkerError> {
        let payloads: Vec<(String, Option<Vec<u8>>)> = rows
            .iter()
            .map(|row| (row.reference.clone(), row.segment_payload.clone()))
            .collect();
        let segments = match self {
            Self::Sqlite(pool) => {
                super::payload::sqlite::read_segments(pool, principal, &payloads).await
            }
            Self::Postgres(pool) => {
                super::payload::postgres::read_segments(pool, principal, &payloads).await
            }
        }
        .map_err(segment_storage)?;
        rows.into_iter()
            .zip(segments)
            .map(|(row, segment)| {
                Ok(ResolvedHistoryMarker {
                    marker: HistoryMarker {
                        reference: row.reference,
                        kind: kind_from_db(&row.kind)?,
                        activity: row.activity,
                    },
                    execution_state: state_from_db(row.execution_state.as_deref())?,
                    execution_deadline_unix_ms: row.execution_deadline,
                    segment,
                    published: row.published_at.is_some(),
                })
            })
            .collect()
    }

    async fn interrupt_if_stale(
        &self,
        principal: &str,
        reference: &str,
    ) -> Result<(), HistoryMarkerError> {
        let Some(row) = self.row(principal, reference).await? else {
            return Ok(());
        };
        if !matches!(row.execution_state.as_deref(), Some("pending" | "running")) {
            return Ok(());
        }
        let now = Self::now();
        let deadline_expired = row
            .execution_deadline
            .is_some_and(|deadline| deadline <= now);
        let lease_expired = match row.execution_state.as_deref() {
            Some("running") => row.lease_expires_at.is_some_and(|lease| lease <= now),
            Some("pending") => false,
            _ => false,
        };
        if !deadline_expired && !lease_expired {
            return Ok(());
        }
        let (terminal_state, message) = if lease_expired {
            (
                "interrupted",
                "Platform tool execution was interrupted before a terminal result was recorded. The model may request it again.",
            )
        } else {
            (
                "failed",
                "Platform tool execution reached its registered deadline.",
            )
        };
        let call: ToolCall = serde_json::from_str(
            row.call_payload
                .as_deref()
                .ok_or(HistoryMarkerError::InvalidPayload)?,
        )
        .map_err(|error| HistoryMarkerError::Storage(error.to_string()))?;
        let segment = HiddenHistorySegment::Platform {
            result: ContentBlock::ToolResult {
                content_kind: Some(
                    stravia_runtime_contract::protocol::ir::ToolResultContentKind::Json,
                ),
                tool_use_id: call.id.clone(),
                content: serde_json::Value::String(message.into()),
                is_error: Some(true),
                cache_control: None,
            },
            call,
        };
        let payload = super::content::encode_segment(&segment).map_err(segment_storage)?;
        match self {
            Self::Sqlite(pool) => {
                let mut connection = pool.acquire_write().await.map_err(storage)?;
                let mut transaction = connection.begin().await.map_err(storage)?;
                let (envelope, ids) =
                    super::payload::sqlite::intern_segment(&mut transaction, principal, payload)
                        .await
                        .map_err(segment_storage)?;
                super::payload::sqlite::replace_refs(&mut transaction, reference, &ids)
                    .await
                    .map_err(segment_storage)?;
                let updated = sqlx::query(
                    "UPDATE history_markers SET execution_state = ?, segment_payload = ?, \
                     execution_owner = NULL, lease_expires_at = NULL, updated_at = ? \
                     WHERE principal = ? AND reference = ? AND execution_state = ? \
                     AND ((? AND execution_deadline <= ?) OR (NOT ? AND lease_expires_at <= ?))",
                )
                .bind(terminal_state)
                .bind(&envelope)
                .bind(now)
                .bind(principal)
                .bind(reference)
                .bind(row.execution_state.expect("stale execution state"))
                .bind(deadline_expired)
                .bind(now)
                .bind(deadline_expired)
                .bind(now)
                .execute(&mut *transaction)
                .await
                .map_err(storage)?
                .rows_affected();
                if updated == 1 {
                    transaction.commit().await.map_err(storage)?;
                } else {
                    transaction.rollback().await.map_err(storage)?;
                }
                connection.finish().await.map_err(storage)?;
            }
            Self::Postgres(pool) => {
                let mut transaction = pool.begin().await.map_err(storage)?;
                let (envelope, ids) =
                    super::payload::postgres::intern_segment(&mut transaction, principal, payload)
                        .await
                        .map_err(segment_storage)?;
                super::payload::postgres::replace_refs(&mut transaction, reference, &ids)
                    .await
                    .map_err(segment_storage)?;
                let updated = sqlx::query(
                    "UPDATE history_markers SET execution_state = $1, segment_payload = $2, \
                     execution_owner = NULL, lease_expires_at = NULL, updated_at = $3 \
                     WHERE principal = $4 AND reference = $5 AND execution_state = $6 \
                     AND (($7 AND execution_deadline <= $8) OR (NOT $7 AND lease_expires_at <= $8))",
                )
                .bind(terminal_state)
                .bind(&envelope)
                .bind(now)
                .bind(principal)
                .bind(reference)
                .bind(row.execution_state.expect("stale execution state"))
                .bind(deadline_expired)
                .bind(now)
                .execute(&mut *transaction)
                .await
                .map_err(storage)?
                .rows_affected();
                if updated == 1 {
                    transaction.commit().await.map_err(storage)?;
                } else {
                    transaction.rollback().await.map_err(storage)?;
                }
            }
        }
        Ok(())
    }
}

fn storage(error: sqlx::Error) -> HistoryMarkerError {
    HistoryMarkerError::Storage(error.to_string())
}

fn segment_storage(error: anyhow::Error) -> HistoryMarkerError {
    HistoryMarkerError::Storage(error.to_string())
}

fn json_storage(error: serde_json::Error) -> HistoryMarkerError {
    HistoryMarkerError::Storage(error.to_string())
}

fn kind_from_db(value: &str) -> Result<HistoryMarkerKind, HistoryMarkerError> {
    match value {
        "platform" => Ok(HistoryMarkerKind::Platform),
        "thinking" => Ok(HistoryMarkerKind::Thinking),
        _ => Err(HistoryMarkerError::InvalidPayload),
    }
}

fn state_from_db(
    value: Option<&str>,
) -> Result<Option<PlatformExecutionState>, HistoryMarkerError> {
    value
        .map(|value| match value {
            "pending" => Ok(PlatformExecutionState::Pending),
            "running" => Ok(PlatformExecutionState::Running),
            "completed" => Ok(PlatformExecutionState::Completed),
            "failed" => Ok(PlatformExecutionState::Failed),
            "interrupted" => Ok(PlatformExecutionState::Interrupted),
            _ => Err(HistoryMarkerError::InvalidPayload),
        })
        .transpose()
}

fn state_to_db(state: PlatformExecutionState) -> &'static str {
    match state {
        PlatformExecutionState::Pending => "pending",
        PlatformExecutionState::Running => "running",
        PlatformExecutionState::Completed => "completed",
        PlatformExecutionState::Failed => "failed",
        PlatformExecutionState::Interrupted => "interrupted",
    }
}

fn validate_thinking(block: &ContentBlock) -> bool {
    match block {
        ContentBlock::Thinking {
            signature: Some(signature),
            ..
        } => !signature.is_empty(),
        ContentBlock::Thinking {
            thinking,
            signature: None,
        } => !thinking.is_empty(),
        ContentBlock::Reasoning {
            encrypted_content: Some(signature),
            ..
        } => !signature.is_empty(),
        ContentBlock::Reasoning {
            summary,
            content,
            encrypted_content: None,
        } => summary.iter().chain(content).any(|text| !text.is_empty()),
        ContentBlock::RedactedThinking { data } => !data.is_empty(),
        _ => false,
    }
}

fn validate_activity(activity: &str) -> bool {
    activity.trim() == activity
        && !activity.is_empty()
        && activity.chars().count() <= 120
        && !activity
            .chars()
            .any(|character| character.is_control() || matches!(character, '<' | '>' | '`'))
}

fn validate_platform_segment(segment: &HiddenHistorySegment, call_payload: &str) -> bool {
    let HiddenHistorySegment::Platform { call, result } = segment else {
        return false;
    };
    let Ok(stored_call) = serde_json::from_str::<ToolCall>(call_payload) else {
        return false;
    };
    let calls_match = serde_json::to_value(call).ok() == serde_json::to_value(stored_call).ok();
    let result_matches = matches!(
        result,
        ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == &call.id
    );
    calls_match && result_matches
}

#[async_trait]
impl HistoryMarkerStore for SqlHistoryMarkerStore {
    async fn create_public_execution(
        &self,
        principal: &Principal,
        marker_reference: &str,
        input: PublicExecutionInput,
    ) -> Result<String, HistoryMarkerError> {
        let id = stravia_runtime_contract::identifier::new_id();
        let principal = principal.continuation_key();
        let now = Self::now();
        let retention_ms = i64::try_from(input.retention.as_millis()).unwrap_or(i64::MAX);
        if input.deadline_unix_ms <= now {
            return Err(HistoryMarkerError::TerminalConflict);
        }
        backend!(self, pool, {
            let mut connection = pool.acquire_write().await.map_err(storage)?;
            let mut tx = connection.begin().await.map_err(storage)?;
            // Preserve the original absolute acceptance deadline.
            sqlx::query(
                "UPDATE history_markers SET execution_deadline = CASE WHEN execution_deadline < $1 \
                 THEN execution_deadline ELSE $1 END WHERE reference = $2 AND principal = $3 AND kind = 'platform'",
            )
            .bind(input.deadline_unix_ms)
            .bind(marker_reference)
            .bind(&principal)
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
            let updated = sqlx::query(
                "INSERT INTO public_tool_executions \
                 (id, marker_reference, principal, source_kind, source_run_id, process_id, retention_ms) \
                 SELECT $1, reference, principal, $2, $3, $4, $5 FROM history_markers \
                 WHERE reference = $6 AND principal = $7 AND kind = 'platform' AND execution_state IN ('pending', 'running') AND execution_deadline > $8",
            )
            .bind(&id)
            .bind(if input.source_run_id.is_some() { "inference_run" } else { "mcp" })
            .bind(&input.source_run_id)
            .bind(process_id())
            .bind(retention_ms)
            .bind(marker_reference)
            .bind(&principal)
            .bind(Self::now())
            .execute(&mut *tx)
            .await
            .map_err(storage)?
            .rows_affected();
            if updated != 1 {
                return Err(HistoryMarkerError::TerminalConflict);
            }
            tx.commit().await.map_err(storage)?;
            connection.finish().await.map_err(storage)?;
        });
        Ok(id)
    }

    async fn resolve_public_execution(
        &self,
        principal: &Principal,
        id: &str,
    ) -> Result<Option<PublicExecution>, HistoryMarkerError> {
        let key = principal.continuation_key();
        let reference: Option<String> = backend!(self, pool, {
            sqlx::query_scalar(
                "SELECT marker_reference FROM public_tool_executions WHERE id = $1 AND principal = $2 \
                 AND (expires_at IS NULL OR expires_at > $3)",
            )
            .bind(id)
            .bind(&key)
            .bind(Self::now())
            .fetch_optional(pool)
            .await
            .map_err(storage)?
        });
        let Some(reference) = reference else {
            return Ok(None);
        };
        let Some(marker) = self.resolve(principal, &reference).await? else {
            return Ok(None);
        };
        // Recheck expiry after resolving a stale owner into a terminal result.
        let exists: i64 = backend!(self, pool, {
            sqlx::query_scalar(
                "SELECT COUNT(*) FROM public_tool_executions WHERE id = $1 AND principal = $2 \
                 AND (expires_at IS NULL OR expires_at > $3)",
            )
            .bind(id)
            .bind(&key)
            .bind(Self::now())
            .fetch_one(pool)
            .await
            .map_err(storage)?
        });
        if exists == 0 {
            return Ok(None);
        }
        Ok(Some(PublicExecution {
            id: id.into(),
            marker_reference: reference,
            state: marker
                .execution_state
                .ok_or(HistoryMarkerError::InvalidPayload)?,
            deadline_unix_ms: marker
                .execution_deadline_unix_ms
                .ok_or(HistoryMarkerError::InvalidPayload)?,
            segment: marker.segment,
        }))
    }

    async fn handoff_public_execution(
        &self,
        principal: &Principal,
        id: &str,
        before_commit: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<(), HistoryMarkerError> {
        let key = principal.continuation_key();
        backend!(self, pool, {
            let mut connection = pool.acquire_write().await.map_err(storage)?;
            let mut transaction = connection.begin().await.map_err(storage)?;
            let updated = sqlx::query(
                "UPDATE public_tool_executions SET handed_off_at = COALESCE(handed_off_at, $1) \
                 WHERE id = $2 AND principal = $3 AND id IN \
                 (SELECT id FROM public_tool_executions WHERE expires_at IS NULL OR expires_at > $1)",
            )
            .bind(Self::now())
            .bind(id)
            .bind(&key)
            .execute(&mut *transaction)
            .await
            .map_err(storage)?
            .rows_affected();
            if updated != 1 || !before_commit() {
                transaction.rollback().await.map_err(storage)?;
                connection.finish().await.map_err(storage)?;
                return Err(HistoryMarkerError::TerminalConflict);
            }
            match transaction.commit().await {
                Ok(()) => connection.finish().await.map_err(storage),
                Err(commit_error) => {
                    connection.finish().await.map_err(storage)?;
                    // A failed acknowledgement does not establish that COMMIT
                    // failed. Read the durable fact without replaying the update.
                    let persisted: Result<i64, sqlx::Error> = sqlx::query_scalar(
                        "SELECT COUNT(*) FROM public_tool_executions \
                         WHERE id = $1 AND principal = $2 AND handed_off_at IS NOT NULL",
                    )
                    .bind(id)
                    .bind(&key)
                    .fetch_one(pool)
                    .await;
                    match persisted {
                        Ok(1) => Ok(()),
                        Ok(_) => Err(storage(commit_error)),
                        Err(readback_error) => Err(HistoryMarkerError::Storage(format!(
                            "handoff COMMIT failed: {commit_error}; durable handoff readback failed: {readback_error}"
                        ))),
                    }
                }
            }
        })
    }

    async fn recover_public_executions(&self) -> Result<(), HistoryMarkerError> {
        let interrupted: Vec<(String, String)> = backend!(self, pool, {
            let mut connection = pool.acquire_write().await.map_err(storage)?;
            // Reuse the existing stale-owner transition and payload construction.
            // No owner from another process may finish or reclaim these markers.
            let interrupted = sqlx::query_as(
                "UPDATE history_markers SET execution_state = 'running', lease_expires_at = 0, \
                 execution_owner = NULL WHERE execution_state IN ('pending', 'running') \
                 AND reference IN (SELECT marker_reference FROM public_tool_executions WHERE process_id <> $1) \
                 RETURNING principal, reference",
            )
            .bind(process_id())
            .fetch_all(&mut *connection)
            .await
            .map_err(storage)?;
            connection.finish().await.map_err(storage)?;
            interrupted
        });
        for (principal, reference) in interrupted {
            self.interrupt_if_stale(&principal, &reference).await?;
        }
        Ok(())
    }

    async fn create_platform(
        &self,
        principal: &Principal,
        input: PlatformMarkerInput,
    ) -> Result<HistoryMarker, HistoryMarkerError> {
        if input.tool_id.trim().is_empty()
            || !validate_activity(&input.activity)
            || input.execution_limit.is_zero()
        {
            return Err(HistoryMarkerError::InvalidPayload);
        }
        let reference = new_reference();
        let principal = principal.continuation_key();
        let call_payload = serde_json::to_string(&input.call)
            .map_err(|error| HistoryMarkerError::Storage(error.to_string()))?;
        let now = Self::now();
        let deadline = Self::after(now, input.execution_limit);
        let pending_until = Self::after(now, PENDING_RETENTION);
        match self {
            Self::Sqlite(pool) => {
                let mut connection = pool.acquire_write().await.map_err(storage)?;
                sqlx::query(
                    "INSERT INTO history_markers \
             (reference, principal, kind, activity, tool_id, call_payload, execution_state, \
              execution_deadline, created_at, updated_at, pending_until) \
             VALUES (?, ?, 'platform', ?, ?, ?, 'pending', ?, ?, ?, ?)",
                )
                .bind(&reference)
                .bind(principal)
                .bind(&input.activity)
                .bind(input.tool_id)
                .bind(call_payload)
                .bind(deadline)
                .bind(now)
                .bind(now)
                .bind(pending_until)
                .execute(&mut *connection)
                .await
                .map_err(storage)?;
                connection.finish().await.map_err(storage)?;
            }
            Self::Postgres(pool) => {
                sqlx::query(
                    "INSERT INTO history_markers \
             (reference, principal, kind, activity, tool_id, call_payload, execution_state, \
              execution_deadline, created_at, updated_at, pending_until) \
             VALUES ($1, $2, 'platform', $3, $4, $5, 'pending', $6, $7, $7, $8)",
                )
                .bind(&reference)
                .bind(principal)
                .bind(&input.activity)
                .bind(input.tool_id)
                .bind(call_payload)
                .bind(deadline)
                .bind(now)
                .bind(pending_until)
                .execute(pool)
                .await
                .map_err(storage)?;
            }
        }
        Ok(HistoryMarker {
            reference,
            kind: HistoryMarkerKind::Platform,
            activity: input.activity,
        })
    }

    async fn create_thinking(
        &self,
        principal: &Principal,
        input: ThinkingMarkerInput,
    ) -> Result<HistoryMarker, HistoryMarkerError> {
        self.insert_thinking(principal, new_reference(), input)
            .await
    }

    async fn create_reserved_thinking(
        &self,
        principal: &Principal,
        reserved: &HistoryMarker,
        input: ThinkingMarkerInput,
    ) -> Result<HistoryMarker, HistoryMarkerError> {
        if reserved.kind != HistoryMarkerKind::Thinking || reserved.activity != input.activity {
            return Err(HistoryMarkerError::InvalidPayload);
        }
        self.insert_thinking(principal, reserved.reference.clone(), input)
            .await
    }

    async fn resolve(
        &self,
        principal: &Principal,
        reference: &str,
    ) -> Result<Option<ResolvedHistoryMarker>, HistoryMarkerError> {
        self.interrupt_if_stale(&principal.continuation_key(), reference)
            .await?;
        let Some(row) = self.row(&principal.continuation_key(), reference).await? else {
            return Ok(None);
        };
        let mut resolved = self
            .decode_rows(&principal.continuation_key(), vec![row])
            .await?;
        Ok(resolved.pop())
    }

    async fn resolve_many(
        &self,
        principal: &Principal,
        references: &[String],
    ) -> Result<Vec<Option<ResolvedHistoryMarker>>, HistoryMarkerError> {
        let rows = self.rows(principal, references).await?;
        let key = principal.continuation_key();
        let mut rows_by_reference: std::collections::HashMap<String, MarkerRow> = rows
            .into_iter()
            .map(|row| (row.reference.clone(), row))
            .collect();
        let mut pending = std::collections::HashSet::new();
        let mut batch = Vec::new();
        for reference in references {
            let Some(row) = rows_by_reference.remove(reference) else {
                continue;
            };
            if matches!(row.execution_state.as_deref(), Some("pending" | "running")) {
                pending.insert(reference.clone());
            } else {
                batch.push(row);
            }
        }
        let decoded = self.decode_rows(&key, batch).await?;
        let mut decoded: std::collections::HashMap<String, ResolvedHistoryMarker> = decoded
            .into_iter()
            .map(|resolved| (resolved.marker.reference.clone(), resolved))
            .collect();
        let mut resolved = Vec::with_capacity(references.len());
        for reference in references {
            if let Some(marker) = decoded.remove(reference) {
                resolved.push(Some(marker));
            } else if pending.contains(reference) {
                // The serial path owns stale-interrupt writes; keep them out of
                // the batch read so decode stays pure.
                resolved.push(self.resolve(principal, reference).await?);
            } else {
                resolved.push(None);
            }
        }
        Ok(resolved)
    }

    async fn claim_execution(
        &self,
        principal: &Principal,
        reference: &str,
        owner_id: &str,
        lease: Duration,
    ) -> Result<ClaimOutcome, HistoryMarkerError> {
        if owner_id.trim().is_empty() || lease.is_zero() {
            return Err(HistoryMarkerError::InvalidPayload);
        }
        self.interrupt_if_stale(&principal.continuation_key(), reference)
            .await?;
        let now = Self::now();
        let lease_expires_at = Self::after(now, lease);
        let principal_key = principal.continuation_key();
        let updated = match self {
            Self::Sqlite(pool) => {
                let mut connection = pool.acquire_write().await.map_err(storage)?;
                let updated = sqlx::query(
                "UPDATE history_markers SET execution_state = 'running', execution_owner = ?, \
     lease_expires_at = ?, updated_at = ? WHERE principal = ? AND reference = ? \
     AND kind = 'platform' AND execution_state = 'pending' AND execution_deadline > ? \
     AND NOT EXISTS (SELECT 1 FROM public_tool_executions p WHERE p.marker_reference = history_markers.reference AND p.expires_at <= ?)",
            )
            .bind(owner_id)
            .bind(lease_expires_at)
            .bind(now)
            .bind(principal_key)
            .bind(reference)
            .bind(now)
            .bind(now)
            .execute(&mut *connection)
            .await
            .map_err(storage)?
            .rows_affected();
                connection.finish().await.map_err(storage)?;
                updated
            }
            Self::Postgres(pool) => sqlx::query(
                "UPDATE history_markers SET execution_state = 'running', execution_owner = $1, \
     lease_expires_at = $2, updated_at = $3 WHERE principal = $4 AND reference = $5 \
     AND kind = 'platform' AND execution_state = 'pending' AND execution_deadline > $3 \
     AND NOT EXISTS (SELECT 1 FROM public_tool_executions p WHERE p.marker_reference = history_markers.reference AND p.expires_at <= $3)",
            )
            .bind(owner_id)
            .bind(lease_expires_at)
            .bind(now)
            .bind(principal_key)
            .bind(reference)
            .execute(pool)
            .await
            .map_err(storage)?
            .rows_affected(),
        };
        if updated == 1 {
            return Ok(ClaimOutcome::Claimed);
        }
        Ok(match self.resolve(principal, reference).await? {
            None => ClaimOutcome::NotFound,
            Some(marker) => match marker.execution_state {
                Some(PlatformExecutionState::Pending | PlatformExecutionState::Running) => {
                    ClaimOutcome::Busy
                }
                Some(_) | None => ClaimOutcome::Terminal,
            },
        })
    }

    async fn finish_execution(
        &self,
        principal: &Principal,
        reference: &str,
        owner_id: &str,
        state: PlatformExecutionState,
        segment: HiddenHistorySegment,
    ) -> Result<(), HistoryMarkerError> {
        if !matches!(
            state,
            PlatformExecutionState::Completed
                | PlatformExecutionState::Failed
                | PlatformExecutionState::Interrupted
        ) {
            return Err(HistoryMarkerError::InvalidPayload);
        }
        let Some(row) = self.row(&principal.continuation_key(), reference).await? else {
            return Err(HistoryMarkerError::TerminalConflict);
        };
        let call_payload = row
            .call_payload
            .as_deref()
            .ok_or(HistoryMarkerError::InvalidPayload)?;
        if !validate_platform_segment(&segment, call_payload) {
            return Err(HistoryMarkerError::InvalidPayload);
        }
        let result_is_error = match &segment {
            HiddenHistorySegment::Platform {
                result: ContentBlock::ToolResult { is_error, .. },
                ..
            } => is_error.unwrap_or(false),
            _ => return Err(HistoryMarkerError::InvalidPayload),
        };
        if result_is_error != (state != PlatformExecutionState::Completed) {
            return Err(HistoryMarkerError::InvalidPayload);
        }
        self.interrupt_if_stale(&principal.continuation_key(), reference)
            .await?;
        let payload = super::content::encode_segment(&segment).map_err(segment_storage)?;
        let expected = serde_json::to_value(&segment).map_err(json_storage)?;
        let now = Self::now();
        let principal_key = principal.continuation_key();
        let updated = match self {
            Self::Sqlite(pool) => {
                let mut connection = pool.acquire_write().await.map_err(storage)?;
                let mut transaction = connection.begin().await.map_err(storage)?;
                let (envelope, ids) = super::payload::sqlite::intern_segment(
                    &mut transaction,
                    &principal_key,
                    payload,
                )
                .await
                .map_err(segment_storage)?;
                super::payload::sqlite::replace_refs(&mut transaction, reference, &ids)
                    .await
                    .map_err(segment_storage)?;
                let updated = sqlx::query(
                "UPDATE history_markers SET execution_state = ?, segment_payload = ?, \
         execution_owner = NULL, lease_expires_at = NULL, updated_at = ? \
         WHERE principal = ? AND reference = ? AND execution_state = 'running' \
         AND execution_owner = ? AND execution_deadline > ? AND lease_expires_at > ? \
         AND NOT EXISTS (SELECT 1 FROM public_tool_executions p WHERE p.marker_reference = history_markers.reference AND p.expires_at <= ?)",
            )
            .bind(state_to_db(state))
            .bind(&envelope)
            .bind(now)
            .bind(&principal_key)
            .bind(reference)
            .bind(owner_id)
            .bind(now)
            .bind(now)
            .bind(now)
            .execute(&mut *transaction)
            .await
            .map_err(storage)?
            .rows_affected();
                if updated == 1 {
                    transaction.commit().await.map_err(storage)?;
                } else {
                    transaction.rollback().await.map_err(storage)?;
                }
                connection.finish().await.map_err(storage)?;
                updated
            }
            Self::Postgres(pool) => {
                let mut transaction = pool.begin().await.map_err(storage)?;
                let (envelope, ids) = super::payload::postgres::intern_segment(
                    &mut transaction,
                    &principal_key,
                    payload,
                )
                .await
                .map_err(segment_storage)?;
                super::payload::postgres::replace_refs(&mut transaction, reference, &ids)
                    .await
                    .map_err(segment_storage)?;
                let updated = sqlx::query(
                "UPDATE history_markers SET execution_state = $1, segment_payload = $2, \
         execution_owner = NULL, lease_expires_at = NULL, updated_at = $3 \
         WHERE principal = $4 AND reference = $5 AND execution_state = 'running' \
         AND execution_owner = $6 AND execution_deadline > $3 \
         AND lease_expires_at > $3 \
         AND NOT EXISTS (SELECT 1 FROM public_tool_executions p WHERE p.marker_reference = history_markers.reference AND p.expires_at <= $3)",
            )
            .bind(state_to_db(state))
            .bind(&envelope)
            .bind(now)
            .bind(&principal_key)
            .bind(reference)
            .bind(owner_id)
            .execute(&mut *transaction)
            .await
            .map_err(storage)?
            .rows_affected();
                if updated == 1 {
                    transaction.commit().await.map_err(storage)?;
                } else {
                    transaction.rollback().await.map_err(storage)?;
                }
                updated
            }
        };
        if updated == 1 {
            return Ok(());
        }
        let exists: Option<i32> = backend!(self, pool, {
            sqlx::query_scalar(
                "SELECT 0 FROM history_markers WHERE reference = $1 AND principal = $2 \
                 AND NOT EXISTS (SELECT 1 FROM public_tool_executions p WHERE p.marker_reference = history_markers.reference AND p.expires_at <= $3)",
            )
            .bind(reference)
            .bind(principal.continuation_key())
            .bind(Self::now())
            .fetch_optional(pool)
            .await
            .map_err(storage)?
        });
        if exists != Some(0) {
            return Err(HistoryMarkerError::TerminalConflict);
        }
        let resolved = self.resolve(principal, reference).await?;
        let idempotent = resolved.as_ref().is_some_and(|resolved| {
            resolved.execution_state == Some(state)
                && resolved.segment.as_ref().is_some_and(|stored| {
                    serde_json::to_value(stored).is_ok_and(|stored| stored == expected)
                })
        });
        if idempotent {
            Ok(())
        } else {
            Err(HistoryMarkerError::TerminalConflict)
        }
    }

    async fn wait_terminal(
        &self,
        principal: &Principal,
        reference: &str,
    ) -> Result<Option<ResolvedHistoryMarker>, HistoryMarkerError> {
        loop {
            let marker = self.resolve(principal, reference).await?;
            if marker.as_ref().is_some_and(|marker| {
                matches!(
                    marker.execution_state,
                    Some(PlatformExecutionState::Pending | PlatformExecutionState::Running)
                )
            }) {
                tokio::time::sleep(Duration::from_millis(25)).await;
            } else {
                return Ok(marker);
            }
        }
    }

    async fn publish(
        &self,
        principal: &Principal,
        references: &[String],
    ) -> Result<(), HistoryMarkerError> {
        if references.is_empty() {
            return Ok(());
        }
        let principal = principal.continuation_key();
        let now = Self::now();
        macro_rules! publish {
            ($pool:expr, $begin:literal) => {{
                let mut connection = $pool.acquire_write().await.map_err(storage)?;
                let mut transaction = connection.begin_with($begin).await.map_err(storage)?;
                for reference in references {
                    let updated = sqlx::query(
                        "UPDATE history_markers SET published_at = COALESCE(published_at, $1), \
                         pending_until = NULL, updated_at = $1 \
                         WHERE principal = $2 AND reference = $3",
                    )
                    .bind(now)
                    .bind(&principal)
                    .bind(reference)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage)?
                    .rows_affected();
                    if updated != 1 {
                        return Err(HistoryMarkerError::Storage(
                            "history marker unavailable during publication".into(),
                        ));
                    }
                }
                transaction.commit().await.map_err(storage)?;
                connection.finish().await.map_err(storage)?;
            }};
        }
        match self {
            Self::Sqlite(pool) => publish!(pool, "BEGIN IMMEDIATE"),
            Self::Postgres(pool) => publish!(pool, "BEGIN"),
        }
        Ok(())
    }

    /// 期限扫描负责入队，队列回收是唯一删除方；返回本轮删除的 marker 数。
    async fn sweep(&self, now: i64, retention: Duration) -> Result<u64, HistoryMarkerError> {
        let threshold =
            now.saturating_sub(i64::try_from(retention.as_millis()).unwrap_or(i64::MAX));
        macro_rules! sweep {
            ($pool:expr, $begin:literal, $lock:expr, $skip_locked:literal, $enqueue:path) => {{
                let mut connection = $pool.acquire_write().await.map_err(storage)?;
                let mut tx = connection.begin_with($begin).await.map_err(storage)?;
                if !$lock.is_empty() { sqlx::query($lock).execute(&mut *tx).await.map_err(storage)?; }
                // 到期的公开工具执行先删除；触发器把失去执行记录的 marker 入队。
                sqlx::query("DELETE FROM public_tool_executions WHERE expires_at <= $1")
                    .bind(now)
                    .execute(&mut *tx)
                    .await
                    .map_err(storage)?;
                for (name, column, horizon) in [
                    (WATERMARK_PENDING, "pending_until", now),
                    (WATERMARK_PUBLISHED, "published_at", threshold),
                ] {
                    let watermark: i64 = sqlx::query_scalar(
                        "SELECT position FROM history_retention_watermarks WHERE name = $1",
                    ).bind(name).fetch_one(&mut *tx).await.map_err(storage)?;
                    if horizon < watermark {
                        sqlx::query("UPDATE history_retention_watermarks SET position = $2 WHERE name = $1")
                            .bind(name).bind(horizon).execute(&mut *tx).await.map_err(storage)?;
                        continue;
                    }
                    let query = format!(
                        "INSERT INTO history_marker_gc(reference) \
                         SELECT m.reference FROM history_markers m \
                         WHERE m.{column} > $1 AND m.{column} <= $2 \
                         AND NOT EXISTS(SELECT 1 FROM turn_node_markers r WHERE r.reference=m.reference) \
                         AND NOT EXISTS(SELECT 1 FROM public_tool_executions p WHERE p.marker_reference=m.reference) \
                         ON CONFLICT(reference) DO NOTHING",
                    );
                    sqlx::query(sqlx::AssertSqlSafe(query.as_str()))
                        .bind(watermark).bind(horizon)
                        .execute(&mut *tx).await.map_err(storage)?;
                    sqlx::query("UPDATE history_retention_watermarks SET position = $2 WHERE name = $1")
                        .bind(name).bind(horizon).execute(&mut *tx).await.map_err(storage)?;
                }
                tx.commit().await.map_err(storage)?;
                connection.finish().await.map_err(storage)?;
                let mut removed = 0u64;
                loop {
                    let mut connection = $pool.acquire_write().await.map_err(storage)?;
                    let mut tx = connection.begin_with($begin).await.map_err(storage)?;
                    if !$lock.is_empty() { sqlx::query($lock).execute(&mut *tx).await.map_err(storage)?; }
                    let claim = format!(
                        "SELECT g.reference FROM history_marker_gc g \
                         JOIN history_markers m ON m.reference = g.reference \
                         ORDER BY g.reference LIMIT {SWEEP_BATCH}{}",
                        $skip_locked,
                    );
                    let claimed: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(claim.as_str()))
                        .fetch_all(&mut *tx).await.map_err(storage)?;
                    if claimed.is_empty() {
                        tx.commit().await.map_err(storage)?;
                        connection.finish().await.map_err(storage)?;
                        break;
                    }
                    for reference in &claimed {
                        // 先取回正文 ID：删除 Marker 会级联移除引用行，随后登记失去引用的正文。
                        let released: Vec<i64> = sqlx::query_scalar(
                            "SELECT content_id FROM history_marker_contents WHERE reference = $1",
                        )
                        .bind(reference)
                        .fetch_all(&mut *tx).await.map_err(storage)?;
                        let deleted = sqlx::query(
                            "DELETE FROM history_markers WHERE reference = $1 \
                             AND ((published_at IS NULL AND pending_until <= $2) \
                                  OR (published_at IS NOT NULL AND published_at <= $3)) \
                             AND NOT EXISTS(SELECT 1 FROM turn_node_markers r WHERE r.reference=history_markers.reference) \
                             AND NOT EXISTS(SELECT 1 FROM public_tool_executions p WHERE p.marker_reference=history_markers.reference)",
                        )
                        .bind(reference).bind(now).bind(threshold)
                        .execute(&mut *tx).await.map_err(storage)?.rows_affected();
                        if deleted == 0 {
                            sqlx::query("DELETE FROM history_marker_gc WHERE reference = $1")
                                .bind(reference).execute(&mut *tx).await.map_err(storage)?;
                        } else {
                            $enqueue(&mut tx, &released).await.map_err(segment_storage)?;
                            removed = removed.saturating_add(deleted);
                        }
                    }
                    tx.commit().await.map_err(storage)?;
                    connection.finish().await.map_err(storage)?;
                }
                Ok(removed)
            }};
        }
        match self {
            Self::Sqlite(pool) => sweep!(
                pool,
                "BEGIN IMMEDIATE",
                "",
                "",
                crate::content_store::sqlite::enqueue_unreferenced
            ),
            Self::Postgres(pool) => sweep!(
                pool,
                "BEGIN",
                crate::history_retention::PG_GC_LOCK,
                " FOR UPDATE OF g, m SKIP LOCKED",
                crate::content_store::postgres::enqueue_unreferenced
            ),
        }
    }
}

const SWEEP_BATCH: i64 = 512;
const WATERMARK_PENDING: &str = "history_markers.pending_until";
const WATERMARK_PUBLISHED: &str = "history_markers.published_at";

#[cfg(test)]
impl SqlHistoryMarkerStore {
    /// 只供旧领域 payload fixture 使用：逐个 Marker 经领域编码重写段载荷，不依赖内部 descriptor 布局。
    pub(crate) async fn rewrite_segments(
        &self,
        mut edit: impl FnMut(&mut serde_json::Value),
    ) -> anyhow::Result<()> {
        let Self::Sqlite(pool) = self else {
            anyhow::bail!("rewrite_segments is only available for SQLite fixtures");
        };
        let rows: Vec<(String, String, Vec<u8>)> = sqlx::query_as(
            "SELECT reference, principal, segment_payload FROM history_markers \
             WHERE segment_payload IS NOT NULL",
        )
        .fetch_all(pool)
        .await?;
        for (reference, principal, envelope) in rows {
            let segment = super::payload::sqlite::read_segments(
                pool,
                &principal,
                &[(reference.clone(), Some(envelope))],
            )
            .await?
            .pop()
            .flatten()
            .ok_or_else(|| anyhow::anyhow!("history marker segment is missing"))?;
            let mut value = serde_json::to_value(&segment)?;
            edit(&mut value);
            let segment: HiddenHistorySegment = serde_json::from_value(value)?;
            let payload = super::content::encode_segment(&segment)?;
            let mut connection = pool.acquire_write().await?;
            let mut transaction = connection.begin().await?;
            let (envelope, ids) =
                super::payload::sqlite::intern_segment(&mut transaction, &principal, payload)
                    .await?;
            super::payload::sqlite::replace_refs(&mut transaction, &reference, &ids).await?;
            sqlx::query("UPDATE history_markers SET segment_payload = ? WHERE reference = ?")
                .bind(envelope)
                .bind(&reference)
                .execute(&mut *transaction)
                .await?;
            transaction.commit().await?;
            connection.finish().await?;
        }
        Ok(())
    }
}
