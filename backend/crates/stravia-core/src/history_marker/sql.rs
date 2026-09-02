use super::*;

use sqlx::FromRow;

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
    segment_payload: Option<String>,
    execution_state: Option<String>,
    execution_owner: Option<String>,
    lease_expires_at: Option<i64>,
    execution_deadline: Option<i64>,
    published_at: Option<i64>,
    expires_at: i64,
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
        let segment = serde_json::to_string(&HiddenHistorySegment::Thinking { block: input.block })
            .map_err(|error| HistoryMarkerError::Storage(error.to_string()))?;
        let now = Self::now();
        let expires_at = Self::after(now, input.pending_retention);
        match self {
            Self::Sqlite(pool) => {
                sqlx::query(
                    "INSERT INTO history_markers \
                     (reference, principal, kind, activity, segment_payload, created_at, updated_at, expires_at) \
                     VALUES (?, ?, 'thinking', ?, ?, ?, ?, ?)",
                )
                .bind(&reference)
                .bind(principal)
                .bind(&input.activity)
                .bind(segment)
                .bind(now)
                .bind(now)
                .bind(expires_at)
                .execute(pool)
                .await
                .map_err(storage)?;
            }
            Self::Postgres(pool) => {
                sqlx::query(
                    "INSERT INTO history_markers \
                     (reference, principal, kind, activity, segment_payload, created_at, updated_at, expires_at) \
                     VALUES ($1, $2, 'thinking', $3, $4, $5, $5, $6)",
                )
                .bind(&reference)
                .bind(principal)
                .bind(&input.activity)
                .bind(segment)
                .bind(now)
                .bind(expires_at)
                .execute(pool)
                .await
                .map_err(storage)?;
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
        principal: &Principal,
        reference: &str,
    ) -> Result<Option<MarkerRow>, HistoryMarkerError> {
        let principal = principal.continuation_key();
        let result = match self {
            Self::Sqlite(pool) => sqlx::query_as::<_, MarkerRow>(
                "SELECT reference, kind, activity, call_payload, segment_payload, execution_state, \
                 execution_owner, lease_expires_at, execution_deadline, published_at, expires_at \
                 FROM history_markers WHERE principal = ? AND reference = ? AND expires_at > ?",
            )
            .bind(principal)
            .bind(reference)
            .bind(Self::now())
            .fetch_optional(pool)
            .await,
            Self::Postgres(pool) => sqlx::query_as::<_, MarkerRow>(
                "SELECT reference, kind, activity, call_payload, segment_payload, execution_state, \
                 execution_owner, lease_expires_at, execution_deadline, published_at, expires_at \
                 FROM history_markers WHERE principal = $1 AND reference = $2 AND expires_at > $3",
            )
            .bind(principal)
            .bind(reference)
            .bind(Self::now())
            .fetch_optional(pool)
            .await,
        };
        result.map_err(storage)
    }

    async fn interrupt_if_stale(
        &self,
        principal: &Principal,
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
                tool_use_id: call.id.clone(),
                content: serde_json::Value::String(message.into()),
                is_error: Some(true),
                cache_control: None,
            },
            call,
        };
        let payload = serde_json::to_string(&segment)
            .map_err(|error| HistoryMarkerError::Storage(error.to_string()))?;
        let principal = principal.continuation_key();
        match self {
            Self::Sqlite(pool) => {
                sqlx::query(
                    "UPDATE history_markers SET execution_state = ?, segment_payload = ?, \
                     execution_owner = NULL, lease_expires_at = NULL, updated_at = ? \
                     WHERE principal = ? AND reference = ? AND execution_state = ? \
                     AND ((? AND execution_deadline <= ?) OR (NOT ? AND lease_expires_at <= ?))",
                )
                .bind(terminal_state)
                .bind(payload)
                .bind(now)
                .bind(principal)
                .bind(reference)
                .bind(row.execution_state.expect("stale execution state"))
                .bind(deadline_expired)
                .bind(now)
                .bind(deadline_expired)
                .bind(now)
                .execute(pool)
                .await
                .map_err(storage)?;
            }
            Self::Postgres(pool) => {
                sqlx::query(
                    "UPDATE history_markers SET execution_state = $1, segment_payload = $2, \
                     execution_owner = NULL, lease_expires_at = NULL, updated_at = $3 \
                     WHERE principal = $4 AND reference = $5 AND execution_state = $6 \
                     AND (($7 AND execution_deadline <= $8) OR (NOT $7 AND lease_expires_at <= $8))",
                )
                .bind(terminal_state)
                .bind(payload)
                .bind(now)
                .bind(principal)
                .bind(reference)
                .bind(row.execution_state.expect("stale execution state"))
                .bind(deadline_expired)
                .bind(now)
                .execute(pool)
                .await
                .map_err(storage)?;
            }
        }
        Ok(())
    }
}

fn storage(error: sqlx::Error) -> HistoryMarkerError {
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
        let expires_at = Self::after(now, input.pending_retention);
        match self {
            Self::Sqlite(pool) => {
                sqlx::query(
                    "INSERT INTO history_markers \
                     (reference, principal, kind, activity, tool_id, call_payload, execution_state, \
                      execution_deadline, created_at, updated_at, expires_at) \
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
                .bind(expires_at)
                .execute(pool)
                .await
                .map_err(storage)?;
            }
            Self::Postgres(pool) => {
                sqlx::query(
                    "INSERT INTO history_markers \
                     (reference, principal, kind, activity, tool_id, call_payload, execution_state, \
                      execution_deadline, created_at, updated_at, expires_at) \
                     VALUES ($1, $2, 'platform', $3, $4, $5, 'pending', $6, $7, $7, $8)",
                )
                .bind(&reference)
                .bind(principal)
                .bind(&input.activity)
                .bind(input.tool_id)
                .bind(call_payload)
                .bind(deadline)
                .bind(now)
                .bind(expires_at)
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
        self.interrupt_if_stale(principal, reference).await?;
        let Some(row) = self.row(principal, reference).await? else {
            return Ok(None);
        };
        let kind = kind_from_db(&row.kind)?;
        let segment = row
            .segment_payload
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|error| HistoryMarkerError::Storage(error.to_string()))?;
        let _ = (&row.execution_owner, row.expires_at);
        Ok(Some(ResolvedHistoryMarker {
            marker: HistoryMarker {
                reference: row.reference,
                kind,
                activity: row.activity,
            },
            execution_state: state_from_db(row.execution_state.as_deref())?,
            execution_deadline_unix_ms: row.execution_deadline,
            segment,
            published: row.published_at.is_some(),
        }))
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
        self.interrupt_if_stale(principal, reference).await?;
        let now = Self::now();
        let lease_expires_at = Self::after(now, lease);
        let principal_key = principal.continuation_key();
        let updated = match self {
            Self::Sqlite(pool) => sqlx::query(
                "UPDATE history_markers SET execution_state = 'running', execution_owner = ?, \
                 lease_expires_at = ?, updated_at = ? WHERE principal = ? AND reference = ? \
                 AND kind = 'platform' AND execution_state = 'pending' AND execution_deadline > ? \
                 AND expires_at > ?",
            )
            .bind(owner_id)
            .bind(lease_expires_at)
            .bind(now)
            .bind(principal_key)
            .bind(reference)
            .bind(now)
            .bind(now)
            .execute(pool)
            .await
            .map_err(storage)?
            .rows_affected(),
            Self::Postgres(pool) => sqlx::query(
                "UPDATE history_markers SET execution_state = 'running', execution_owner = $1, \
                 lease_expires_at = $2, updated_at = $3 WHERE principal = $4 AND reference = $5 \
                 AND kind = 'platform' AND execution_state = 'pending' AND execution_deadline > $3 \
                 AND expires_at > $3",
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
        let Some(row) = self.row(principal, reference).await? else {
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
        self.interrupt_if_stale(principal, reference).await?;
        let payload = serde_json::to_string(&segment)
            .map_err(|error| HistoryMarkerError::Storage(error.to_string()))?;
        let now = Self::now();
        let principal_key = principal.continuation_key();
        let updated = match self {
            Self::Sqlite(pool) => sqlx::query(
                "UPDATE history_markers SET execution_state = ?, segment_payload = ?, \
                 execution_owner = NULL, lease_expires_at = NULL, updated_at = ? \
                 WHERE principal = ? AND reference = ? AND execution_state = 'running' \
                 AND execution_owner = ? AND execution_deadline > ? AND lease_expires_at > ?",
            )
            .bind(state_to_db(state))
            .bind(&payload)
            .bind(now)
            .bind(principal_key)
            .bind(reference)
            .bind(owner_id)
            .bind(now)
            .bind(now)
            .execute(pool)
            .await
            .map_err(storage)?
            .rows_affected(),
            Self::Postgres(pool) => sqlx::query(
                "UPDATE history_markers SET execution_state = $1, segment_payload = $2, \
                 execution_owner = NULL, lease_expires_at = NULL, updated_at = $3 \
                 WHERE principal = $4 AND reference = $5 AND execution_state = 'running' \
                 AND execution_owner = $6 AND execution_deadline > $3 \
                 AND lease_expires_at > $3",
            )
            .bind(state_to_db(state))
            .bind(&payload)
            .bind(now)
            .bind(principal_key)
            .bind(reference)
            .bind(owner_id)
            .execute(pool)
            .await
            .map_err(storage)?
            .rows_affected(),
        };
        if updated == 1 {
            return Ok(());
        }
        let resolved = self.resolve(principal, reference).await?;
        let idempotent = resolved.as_ref().is_some_and(|resolved| {
            resolved.execution_state == Some(state)
                && resolved
                    .segment
                    .as_ref()
                    .and_then(|stored| serde_json::to_string(stored).ok())
                    .as_deref()
                    == Some(payload.as_str())
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
        retention: Duration,
    ) -> Result<(), HistoryMarkerError> {
        if references.is_empty() {
            return Ok(());
        }
        let now = Self::now();
        let expires_at = Self::after(now, retention);
        let principal = principal.continuation_key();
        match self {
            Self::Sqlite(pool) => {
                let mut transaction = pool.begin().await.map_err(storage)?;
                for reference in references {
                    let updated = sqlx::query(
                        "UPDATE history_markers SET published_at = COALESCE(published_at, ?), \
                     expires_at = MAX(expires_at, ?), updated_at = ? \
                     WHERE principal = ? AND reference = ? AND expires_at > ?",
                    )
                    .bind(now)
                    .bind(expires_at)
                    .bind(now)
                    .bind(&principal)
                    .bind(reference)
                    .bind(now)
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
            }
            Self::Postgres(pool) => {
                let mut transaction = pool.begin().await.map_err(storage)?;
                for reference in references {
                    let updated = sqlx::query(
                        "UPDATE history_markers SET published_at = COALESCE(published_at, $1), \
                     expires_at = GREATEST(expires_at, $2), updated_at = $1 \
                     WHERE principal = $3 AND reference = $4 AND expires_at > $1",
                    )
                    .bind(now)
                    .bind(expires_at)
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
            }
        }
        Ok(())
    }

    async fn extend_retention(
        &self,
        principal: &Principal,
        references: &[String],
        retention: Duration,
    ) -> Result<(), HistoryMarkerError> {
        if references.is_empty() {
            return Ok(());
        }
        let now = Self::now();
        let expires_at = Self::after(now, retention);
        let principal = principal.continuation_key();
        for reference in references {
            let updated = match self {
                Self::Sqlite(pool) => sqlx::query(
                        "UPDATE history_markers SET expires_at = MAX(expires_at, ?), updated_at = ? \
                         WHERE principal = ? AND reference = ? AND expires_at > ?",
                    )
                    .bind(expires_at)
                    .bind(now)
                    .bind(&principal)
                    .bind(reference)
                    .bind(now)
                    .execute(pool)
                    .await
                    .map_err(storage)?
                    .rows_affected(),
                Self::Postgres(pool) => sqlx::query(
                        "UPDATE history_markers SET expires_at = GREATEST(expires_at, $1), updated_at = $2 \
                         WHERE principal = $3 AND reference = $4 AND expires_at > $2",
                    )
                    .bind(expires_at)
                    .bind(now)
                    .bind(&principal)
                    .bind(reference)
                    .execute(pool)
                    .await
                    .map_err(storage)?
                    .rows_affected(),
            };
            if updated != 1 {
                return Err(HistoryMarkerError::Storage(
                    "trusted Generation Chain references an unavailable history marker".into(),
                ));
            }
        }
        Ok(())
    }

    async fn cleanup_expired(&self) -> Result<u64, HistoryMarkerError> {
        let now = Self::now();
        match self {
            Self::Sqlite(pool) => sqlx::query("DELETE FROM history_markers WHERE expires_at <= ?")
                .bind(now)
                .execute(pool)
                .await
                .map(|result| result.rows_affected())
                .map_err(storage),
            Self::Postgres(pool) => {
                sqlx::query("DELETE FROM history_markers WHERE expires_at <= $1")
                    .bind(now)
                    .execute(pool)
                    .await
                    .map(|result| result.rows_affected())
                    .map_err(storage)
            }
        }
    }
}
