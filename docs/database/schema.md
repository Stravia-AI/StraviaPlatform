# Database Schema

Stravia supports **SQLite** and **PostgreSQL** only. Both use the logical schema below; PostgreSQL uses native `BOOLEAN`, `TIMESTAMPTZ`, and `BIGINT` where SQLite uses `INTEGER` or `TEXT`.

## Entity Relationship

```
providers ──1:N── model_backends ──N:1── models ──M:N── api_keys (via api_key_models)
    ├──1:1── provider_oauth_credentials
    ├──1:N── provider_models ──1:N── provider_model_cost_rules
    └──1:N── provider_allowance_samples
web_providers (Local Web Search / Fetch upstreams)
interaction_observations ──1:N── inference_run_observations ──1:N── model_turn_observations ──1:N── target_attempt_observations
    ├──1:N── observation_events
    └──1:N── debug_trace_manifests (managed Trace files)
rejected_request_observations ──1:N── observation_events / debug_trace_manifests
turn_chain_nodes (principal-scoped Response / Agent / Web Search DAG)
native_compactions ──1:N── native_compaction_states / native_compaction_sources
history_markers (principal-scoped hidden history and Platform execution state)
reversible_redaction_mappings (principal-scoped persistent secret placeholders)
agent_definition_revisions ──1:1── agent_definition_configs
artifacts ──1:0..1── artifact_uploads ──1:N── artifact_upload_parts
    ├──1:N── artifact_download_grants
    └──1:0..1── media_derivatives ──1:1── artifacts (JPEG derivative)
admin_identity ──1:N── admin_sessions
settings (key-value, including Web Access and revisioned Web Search configuration)
```

---

## providers

AI 模型供应商配置（API endpoint、密钥、认证方式等）。

| Column | Type | Default | Description |
|---|---|---|---|
| `id` | TEXT PK | — | 主键，UUID |
| `name` | TEXT NOT NULL | — | 显示名称 |
| `vendor` | TEXT | NULL | 供应商标识（如 `openai`、`anthropic`） |
| `protocol` | TEXT NOT NULL | — | 默认通信协议（如 `openai-compatible`） |
| `base_url` | TEXT NOT NULL | — | API 端点基础 URL |
| `preset_key` | TEXT | NULL | 预设模板 key（内置供应商模板标识） |
| `channel` | TEXT | NULL | 预设通道 ID（如 `default`、`azure`） |
| `models_source` | TEXT | NULL | 模型列表获取方式 |
| `static_models` | TEXT | NULL | 静态模型列表（`\n` 分隔） |
| `api_key` | TEXT NOT NULL | — | API 密钥 |
| `adapter_credentials` | JSONB / TEXT | `'{}'` | Vendor 声明的上游凭据字段；secret 值不通过 Admin API 回显 |
| `auth_mode` | TEXT | `'apikey'` | 认证方式：`apikey` 或 `oauth` |
| `access_token` | TEXT | NULL | Provider 级 OAuth access token |
| `refresh_token` | TEXT | NULL | Provider 级 OAuth refresh token |
| `expires_at` | TEXT | NULL | Provider 级 OAuth token 过期时间 |
| `use_proxy` | INTEGER | `0` | 是否通过代理发送请求 |
| `last_test_success` | INTEGER | NULL | 最近一次连通性测试是否成功 |
| `last_test_at` | TEXT | NULL | 最近一次连通性测试时间 |
| `is_enabled` | INTEGER | `1` | 是否启用 |
| `priority` | INTEGER | `0` | 优先级（预留） |
| `created_at` | TEXT | `datetime('now')` | 创建时间 |
| `updated_at` | TEXT | `datetime('now')` | 更新时间 |

---

## models

Route 记录。`model_id` 保存客户端请求使用的 Route ID，`display_name` 保存可选的人类可读名称；Targets 只存于 `model_backends`。

| Column | Type | Default | Description |
|---|---|---|---|
| `id` | TEXT PK | — | 主键，UUID |
| `model_id` | TEXT NOT NULL | — | Route ID；客户端模型 ID，精确且大小写敏感匹配 |
| `display_name` | TEXT NULL | `NULL` | 可选展示名称；空值由应用层回退为 `model_id` |
| `balance` | TEXT | `'traffic_equalization'` | Route Scheduling Strategy：`traffic_equalization` 或 `latency_preference`；管理接口对旧值做写入归一化，读取只返回新值 |
| `is_enabled` | INTEGER | `1` | 是否启用 |
| `priority` | INTEGER | `0` | 优先级（预留） |
| `created_at` | TEXT | `datetime('now')` | 创建时间 |

**唯一索引**：`idx_models_route_id` on `model_id`

---

## model_backends

Target 列表；一条 Route 对应一个或多个 Provider + Provider Model 组合。

| Column | Type | Default | Description |
|---|---|---|---|
| `id` | TEXT PK | — | 主键，UUID |
| `model_id` | TEXT NOT NULL | — | 所属 Route 的存储主键（FK → models.id, ON DELETE CASCADE） |
| `provider_id` | TEXT NOT NULL | — | 供应商 ID（FK → providers.id） |
| `model` | TEXT NOT NULL | — | 上游模型名（发送给 provider 的模型标识） |
| `enabled` | BOOLEAN / INTEGER | `true` / `1` | Target 是否启用；已禁用 Target 仍属于 Route，但不参与选择、亲和或冷却 |
| `priority` | INTEGER | `0` | Target Priority，范围 -2147483648–2147483647，数值越大越优先；仅已启用且相同值的 Target 组成一个调度组 |
| `thinking_level_map` | JSON | 七行 Hidden Mapping | Target 的七行 Thinking Level Map，包含 Control 与 Generated/Overridden 来源；SQLite 使用 JSON 文本，PostgreSQL 使用 JSONB |
| `first_token_timeout_ms` | BIGINT / INTEGER | `60000` | First Token Timeout（毫秒）；`0` 表示关闭 |
| `target_retry_budget` | INTEGER | `5` | 同一 Target 的额外重试次数 |
| `target_cooldown_ms` | BIGINT / INTEGER | `120000` | 放弃 Target 后阻止新请求选中它的进程内冷却时长（毫秒） |
| `created_at` | TEXT | `datetime('now')` | 创建时间 |

**索引**：`idx_model_backends_model_id` on `model_id`

---

## api_keys

API 密钥管理，用于代理端口和 MCP 的访问认证及并发执行数控制。

| Column | Type | Default | Description |
|---|---|---|---|
| `id` | TEXT PK | — | 主键，UUID |
| `token` | TEXT NOT NULL UNIQUE | — | 完整密钥值；创建时可由系统生成或由管理员自定义，之后可修改 |
| `name` | TEXT NOT NULL | — | 显示名称 |
| `concurrency_limit` | INTEGER CHECK (`> 0`) | NULL | 此 API Key 允许同时运行的最大根执行数；NULL 表示不限 |
| `is_enabled` | INTEGER | `1` | 是否启用 |
| `mcp_access_enabled` | INTEGER | `0` | 是否允许此 API key 访问 Stravia MCP Server |
| `transparent_injection_enabled` | INTEGER | `0` | 是否允许 Stravia 在兼容模型请求中自动暴露所选高级功能；不控制显式调用或 MCP |
| `inject_media_understanding` | INTEGER | `0` | Transparent Injection 开启时是否选择 Media Understanding；平台 Gate 关闭时保留但不生效 |
| `inject_web_search` | INTEGER | `0` | Transparent Injection 开启时是否选择 Web Search；平台 Gate 关闭时保留但不生效 |
| `expires_at` | TEXT | NULL | 过期时间 |
| `created_at` | TEXT | `datetime('now')` | 创建时间 |
| `updated_at` | TEXT | `datetime('now')` | 更新时间 |

**索引**：`idx_api_keys_token` on `token`

---

## api_key_models

API Key 与模型的访问绑定关系（M:N 关联表）。所有模型请求都必须使用有效且已绑定到目标模型的 API Key。

| Column | Type | Description |
|---|---|---|
| `api_key_id` | TEXT NOT NULL | API Key ID（FK → api_keys.id, ON DELETE CASCADE） |
| `model_id` | TEXT NOT NULL | 模型 ID（FK → models.id, ON DELETE CASCADE） |

**主键**：`(api_key_id, model_id)`

**索引**：`idx_api_key_models_model_id` on `model_id`

---

## admin_identity

实例级唯一管理员身份。`singleton_id = 1` 的数据库约束保证并发初始化也只能创建一个管理员；管理身份与 API Key / Principal 完全独立。

| Column | Type | Default | Description |
|---|---|---|---|
| `singleton_id` | SMALLINT / INTEGER PK | — | 固定为 `1` 的 singleton key |
| `username` | TEXT UNIQUE | NULL | Server 管理员用户名；Desktop 原生管理员为 NULL |
| `password_hash` | TEXT | NULL | Argon2id PHC 验证材料；Desktop 原生管理员为 NULL |
| `jwt_secret` | TEXT NOT NULL | — | 实例本地 JWT 签名秘密，不通过管理 interface 回显 |
| `credential_revision` | BIGINT / INTEGER | `1` | 凭据 revision；凭据修改或本地恢复时递增，使旧会话失效 |

**约束**：`username` 与 `password_hash` 必须同时为 NULL 或同时非 NULL。

---

## admin_sessions

可撤销的管理员登录会话。访问 JWT 每次认证时都会回查此表和 `admin_identity.credential_revision`；refresh token 只以 SHA-256 摘要持久化，并通过条件更新原子轮换。

| Column | Type | Default | Description |
|---|---|---|---|
| `id` | TEXT PK | — | 会话 UUID，同时写入访问 JWT |
| `identity_id` | SMALLINT / INTEGER | `1` | 固定为 `1`，FK → `admin_identity.singleton_id`，ON DELETE CASCADE |
| `credential_revision` | BIGINT / INTEGER NOT NULL | — | 创建会话时的凭据 revision |
| `refresh_hash` | TEXT NOT NULL UNIQUE | — | 当前 refresh token 的不可逆摘要 |
| `expires_at` | BIGINT / INTEGER NOT NULL | — | 固定登录总期限，Unix 秒；refresh 不延长该期限 |
| `revoked` | BOOLEAN / INTEGER | `false` / `0` | 普通退出撤销当前会话；凭据修改或恢复撤销全部会话 |

**索引**：`idx_admin_sessions_expiry` on `expires_at`

---

## provider_oauth_credentials

OAuth 凭据存储，用于需要 OAuth 认证的供应商（如 Google Vertex AI）。

| Column | Type | Default | Description |
|---|---|---|---|
| `provider_id` | TEXT PK | — | 供应商 ID（FK → providers.id, ON DELETE CASCADE） |
| `connection_id` | TEXT NOT NULL UNIQUE | — | OAuth 连接 generation 标识；新连接/重连时写入 UUID，token refresh 不变（历史行由 migration 填充唯一 legacy ID） |
| `scheme` | TEXT | `''` | 认证方案 |
| `access_token` | TEXT | `''` | OAuth access token |
| `refresh_token` | TEXT | NULL | OAuth refresh token |
| `expires_at` | TEXT | NULL | Token 过期时间 |
| `resource_url` | TEXT | NULL | 资源 URL（部分 OAuth 流程需要） |
| `subject_id` | TEXT | NULL | 认证主体 ID |
| `scopes` | TEXT | `'[]'` | OAuth 权限范围（JSON 数组） |
| `meta` | TEXT | `'{}'` | 扩展元数据（JSON） |
| `status` | TEXT | `'connected'` | 连接状态 |
| `status_version` | INTEGER | `0` | 状态版本号（乐观锁） |
| `last_error` | TEXT | NULL | 最近一次错误信息 |
| `last_refresh_at` | TEXT | NULL | 最近一次 token 刷新时间 |
| `created_at` | TEXT | `datetime('now')` | 创建时间 |
| `updated_at` | TEXT | `datetime('now')` | 更新时间 |

---

## provider_models

Provider 实例拥有的上游模型快照。Provider discovery 负责新增及 presence 对账；模型元数据由管理员直接编辑，删除 Provider 时通过外键级联删除。

| Column | Type | Default | Description |
|---|---|---|---|
| `provider_id` | TEXT NOT NULL | — | Provider ID（FK → providers.id, ON DELETE CASCADE） |
| `model_id` | TEXT NOT NULL | — | 上游模型 ID |
| `source_kind` | TEXT NOT NULL | — | 来源：`discovered` 或 `manual` |
| `metadata_source_provider_id` | TEXT | NULL | 发现时使用的 Provider Catalog provider ID |
| `presence` | TEXT NOT NULL | — | 最近一次对账结果：`present` 或 `missing` |
| `lifecycle_status` | TEXT | NULL | Provider Catalog 生命周期：`alpha`、`beta` 或 `deprecated` |
| `selection_policy` | TEXT NOT NULL | `auto` | `auto`、`force_enabled` 或 `force_disabled` |
| `name` | TEXT | NULL | 可查询的显示名称投影 |
| `family` | TEXT | NULL | 可查询的模型 family 投影 |
| `attachment` | BOOLEAN / INTEGER | NULL | Attachment 能力投影 |
| `reasoning` | BOOLEAN / INTEGER | NULL | Reasoning 能力投影 |
| `tool_call` | BOOLEAN / INTEGER | NULL | Tool call 能力投影 |
| `open_weights` | BOOLEAN / INTEGER | NULL | Open weights 投影 |
| `structured_output` | BOOLEAN / INTEGER | NULL | Structured output 能力投影 |
| `temperature` | BOOLEAN / INTEGER | NULL | Temperature 能力投影 |
| `limit_context` | BIGINT / INTEGER | NULL | Context token limit |
| `limit_input` | BIGINT / INTEGER | NULL | Input token limit |
| `limit_output` | BIGINT / INTEGER | NULL | Output token limit |
| `cost_input` | NUMERIC / TEXT | NULL | USD / 1M input tokens；SQLite 以精确十进制文本保存 |
| `cost_output` | NUMERIC / TEXT | NULL | USD / 1M output tokens |
| `cost_reasoning` | NUMERIC / TEXT | NULL | USD / 1M reasoning tokens |
| `cost_cache_read` | NUMERIC / TEXT | NULL | USD / 1M cache-read tokens |
| `cost_cache_write` | NUMERIC / TEXT | NULL | USD / 1M cache-write tokens |
| `cost_input_audio` | NUMERIC / TEXT | NULL | USD / 1M audio input tokens |
| `cost_output_audio` | NUMERIC / TEXT | NULL | USD / 1M audio output tokens |
| `metadata_json` | JSONB / TEXT | — | 完整 Provider Model metadata 与未知扩展 |
| `revision` | BIGINT / INTEGER | `1` | 乐观并发 revision |
| `created_at` | TIMESTAMPTZ / TEXT | `NOW()` / `datetime('now')` | 创建时间 |
| `updated_at` | TIMESTAMPTZ / TEXT | `NOW()` / `datetime('now')` | 更新时间 |

**主键**：`(provider_id, model_id)`

**索引**：`idx_provider_models_provider_state`、`idx_provider_models_provider_name`

## provider_model_cost_rules

Provider Model 的有序分层价格规则。`context_over_200k` 与 Provider Catalog `cost.tiers` 规范化为同一关系；删除 Provider Model 时级联删除。

| Column | Type | Description |
|---|---|---|
| `provider_id` | TEXT NOT NULL | Provider ID |
| `model_id` | TEXT NOT NULL | 上游模型 ID |
| `rule_index` | INTEGER NOT NULL | 规则稳定顺序 |
| `rule_kind` | TEXT NOT NULL | `context_over_200k` 或 `tier` |
| `threshold_tokens` | BIGINT / INTEGER | Context threshold |
| `cost_input` … `cost_output_audio` | NUMERIC / TEXT | 该规则的精确价格分量 |

**主键**：`(provider_id, model_id, rule_index)`

**外键**：`(provider_id, model_id)` → `provider_models`，ON DELETE CASCADE

**唯一索引**：`idx_provider_model_cost_rules_threshold` on `(provider_id, model_id, rule_kind, threshold_tokens)`

---

## provider_allowance_samples

Provider 账户级额度的历史样本，用于估算当前重置窗口内的耗尽风险。Gateway 每 30 分钟刷新所有可监控 Provider；仅成功取得的 fresh 账户级额度会写入，模型级额度、stale 快照和失败结果不写入。Gateway 启动、每轮后台采样及写入时删除超过 14 天的样本。

| Column | Type | Default | Description |
|---|---|---|---|
| `id` | TEXT PK | — | 样本 ID，UUID |
| `provider_id` | TEXT NOT NULL | — | Provider ID（FK → `providers.id`, ON DELETE CASCADE） |
| `allowance_key` | TEXT NOT NULL | — | Provider 快照内稳定的账户级额度键 |
| `sampled_at` | BIGINT / INTEGER NOT NULL | — | 采样时间，Unix 毫秒时间戳 |
| `used_value` | DOUBLE PRECISION / REAL | NULL | 已用原始数值 |
| `remaining_value` | DOUBLE PRECISION / REAL | NULL | 剩余原始数值 |
| `limit_value` | DOUBLE PRECISION / REAL | NULL | 总量原始数值 |
| `used_percent` | DOUBLE PRECISION / REAL | NULL | 已用百分比 |
| `amount_unit` | TEXT | NULL | 数值单位 |
| `currency` | TEXT | NULL | 余额币种 |
| `reset_at` | BIGINT / INTEGER | NULL | 当前额度窗口重置时间，Unix 毫秒时间戳 |

**索引**：
- `idx_provider_allowance_samples_item_time` on `(provider_id, allowance_key, sampled_at)`
- `idx_provider_allowance_samples_sampled_at` on `sampled_at`

---

## Interaction Observation

Migration 34 removes `request_logs` and its rows without backfill, then installs the Observation schema below. Ordinary Observation is the diagnostic fact source for the Request Records forest, inspector, Confirmed Upstream Usage analytics, and Route scheduling; it is separate from the immutable Generation Chain. All timestamps and expiry values are Unix milliseconds. SQLite uses integer booleans and an `observation_sequence` singleton; PostgreSQL uses native booleans and `observation_event_sequence`.

### observation_sequence (SQLite only)

| Column | Type | Description |
|---|---|---|
| `singleton_id` | INTEGER PK | Fixed to `1` |
| `next_sequence` | INTEGER NOT NULL | Next monotonic persisted event sequence |

PostgreSQL provides the equivalent with the `observation_event_sequence` `BIGINT` sequence.

### interaction_observations

One row per Connect Client Interaction. `root_id` and `parent_interaction_id` project the outer Generation Chain forest without changing Generation Chain facts.

| Column | Type | Default | Description |
|---|---|---|---|
| `id` | TEXT PK | — | Interaction UUID |
| `principal` | TEXT NOT NULL | — | Authenticated Principal |
| `api_key_id`, `api_key_name` | TEXT | NULL | API-key identity/name snapshot |
| `generation_root_id` | TEXT | NULL | Confirmed Generation Chain root when present |
| `parent_interaction_id` | TEXT FK | NULL | Parent Interaction; ON DELETE SET NULL |
| `root_id`, `root_run_id` | TEXT NOT NULL | — | Observation forest root and first Run |
| `first_route_id` | TEXT NOT NULL | — | Stable title fallback |
| `first_model_display_name` | TEXT | NULL | First Run display name snapshot |
| `status` | TEXT NOT NULL | — | Activity-first Interaction status |
| `started_at`, `last_active_at` | BIGINT / INTEGER | — | Lifecycle times |
| `input_preview` | TEXT | NULL | Opening 4096 Unicode characters of the initiating latest user message's text blocks, joined with newlines; registered-secret and credential filtering precede truncation. Captured from the canonical client window, published after Model Turn protection succeeds, independent of Debug. Historical, non-text, or pre-protection failed input remains NULL; continuation Runs cannot overwrite it. |
| `visible_tail` | TEXT NOT NULL | `''` | Coalesced Client Projection tail only |
| `input_tokens`, `output_tokens`, `cache_read_tokens`, `cache_write_tokens`, `reasoning_tokens` | BIGINT / INTEGER | NULL | Nullable Confirmed Upstream Usage; NULL remains unknown |
| `observation_gap` | BOOLEAN / INTEGER | `false` / `0` | Explicit projection/recording loss |
| `last_event_sequence` | BIGINT / INTEGER | `0` | Last applied persisted event |
| `expires_at` | BIGINT / INTEGER | — | Retention boundary |

**索引**：`interaction_observations_window_idx`、`interaction_observations_generation_idx`、`interaction_observations_filter_idx`、`interaction_observations_expiry_idx`

### inference_run_observations

| Column | Type | Default | Description |
|---|---|---|---|
| `id` | TEXT PK | — | Inference Run UUID |
| `interaction_id` | TEXT FK NOT NULL | — | Owner Interaction; ON DELETE CASCADE |
| `parent_run_id` | TEXT FK | NULL | Run branch parent; ON DELETE SET NULL |
| `generation_node_id`, `generation_parent_id` | TEXT | NULL | Confirmed Generation Chain associations |
| `ingress_protocol` | TEXT NOT NULL | — | Client protocol snapshot |
| `route_id` | TEXT NOT NULL | — | Effective Route ID |
| `model_display_name` | TEXT | NULL | Display-name snapshot |
| `status` | TEXT NOT NULL | — | Run lifecycle state |
| `terminal_reason` | TEXT | NULL | Stable terminal reason |
| `user_interrupted` | BOOLEAN / INTEGER | `false` / `0` | Superseded by later User input |
| `background_active` | BIGINT / INTEGER | `0` | Active internal work count |
| `debug_enabled` | BOOLEAN / INTEGER NOT NULL | — | Process Debug state snapshotted at admission |
| `client_output_committed` | BOOLEAN / INTEGER | `false` / `0` | Client Output Commit boundary |
| `started_at`, `last_active_at`, `finished_at` | BIGINT / INTEGER | `finished_at` NULL | Lifecycle times |
| `last_event_sequence` | BIGINT / INTEGER | `0` | Last applied persisted event |
| `expires_at` | BIGINT / INTEGER | — | Retention boundary |

**索引**：`inference_runs_interaction_idx`、`inference_runs_generation_idx`、`inference_runs_status_idx`

### model_turn_observations

| Column | Type | Default | Description |
|---|---|---|---|
| `id` | TEXT PK | — | Model Turn UUID |
| `run_id` | TEXT FK NOT NULL | — | Owner Run; ON DELETE CASCADE |
| `interaction_id` | TEXT FK NOT NULL | — | Owner Interaction; ON DELETE CASCADE |
| `route_id` | TEXT NOT NULL | — | Effective Route |
| `model_display_name` | TEXT | NULL | Display-name snapshot |
| `api_key_id`, `api_key_name` | TEXT | NULL | API-key identity/name snapshot |
| `status` | TEXT NOT NULL | — | Turn lifecycle state |
| `started_at`, `finished_at` | BIGINT / INTEGER | `finished_at` NULL | Timing |
| `input_tokens`, `output_tokens`, `cache_read_tokens`, `cache_write_tokens`, `reasoning_tokens` | BIGINT / INTEGER | NULL | Nullable confirmed usage rollup |
| `last_event_sequence` | BIGINT / INTEGER NOT NULL | — | Last applied persisted event |

**索引**：`model_turns_interaction_idx`、`model_turns_analytics_idx`

### target_attempt_observations

One row per real upstream Target attempt, including retries and failovers.

| Column | Type | Default | Description |
|---|---|---|---|
| `id` | TEXT PK | — | Attempt UUID |
| `model_turn_id`, `run_id`, `interaction_id` | TEXT FK NOT NULL | — | Owner Turn, Run, and Interaction; ON DELETE CASCADE |
| `target_id` | TEXT NOT NULL | — | Actual Target ID |
| `provider_id`, `provider_name` | TEXT NOT NULL | — | Provider identity/name snapshot |
| `upstream_model` | TEXT NOT NULL | — | Actual upstream model |
| `protocol` | TEXT NOT NULL | — | Egress protocol |
| `status` | TEXT NOT NULL | — | Attempt lifecycle result |
| `status_code` | BIGINT / INTEGER | NULL | Upstream status when applicable |
| `error_code` | TEXT | NULL | Stable error classification |
| `started_at`, `finished_at` | BIGINT / INTEGER | `finished_at` NULL | Timing |
| `duration_ms`, `first_token_ms` | BIGINT / INTEGER | NULL | Attempt and first canonical output latency |
| `input_tokens`, `output_tokens`, `cache_read_tokens`, `cache_write_tokens`, `reasoning_tokens` | BIGINT / INTEGER | NULL | Provider-reported usage |
| `usage_recorded` | BOOLEAN / INTEGER | `false` / `0` | Attempt-level usage deduplication guard |
| `last_event_sequence` | BIGINT / INTEGER NOT NULL | — | Last applied persisted event |

**索引**：`target_attempts_turn_idx`、`target_attempts_analytics_idx`

### rejected_request_observations

Pre-admission decode, protocol, or authentication failures remain outside Principal and Generation Chain.

| Column | Type | Default | Description |
|---|---|---|---|
| `id` | TEXT PK | — | Rejected Request UUID |
| `occurred_at` | BIGINT / INTEGER NOT NULL | — | Ingress time |
| `method`, `path`, `ingress_protocol` | TEXT NOT NULL | — | Minimal ingress identity |
| `stage`, `code` | TEXT NOT NULL | — | Rejection classification |
| `status_code` | BIGINT / INTEGER NOT NULL | — | Client HTTP status |
| `debug_enabled` | BOOLEAN / INTEGER NOT NULL | — | Debug state snapshotted at ingress |
| `debug_status` | TEXT NOT NULL | — | Capture result |
| `last_event_sequence` | BIGINT / INTEGER NOT NULL | — | Last applied persisted event |
| `expires_at` | BIGINT / INTEGER NOT NULL | — | Retention boundary |

**索引**：`rejected_requests_window_idx`、`rejected_requests_expiry_idx`

### debug_trace_manifests

| Column | Type | Default | Description |
|---|---|---|---|
| `trace_id` | TEXT PK | — | Opaque managed Trace identity |
| `run_id` | TEXT FK | NULL | Owner Run; ON DELETE CASCADE |
| `rejection_id` | TEXT FK | NULL | Owner Rejected Request; ON DELETE CASCADE |
| `relative_directory` | TEXT UNIQUE NOT NULL | — | Managed relative identity beneath the data directory; never an absolute path |
| `bytes_written`, `event_count` | BIGINT / INTEGER | `0` | Persisted capture size/count |
| `status` | TEXT NOT NULL | — | `running`, `complete`, or `partial` capture state |
| `partial_reason` | TEXT | NULL | Stable missing/partial reason(s) |
| `tombstoned` | BOOLEAN / INTEGER | `false` / `0` | Pending idempotent managed-file deletion |
| `created_at`, `completed_at` | BIGINT / INTEGER | `completed_at` NULL | Lifecycle times |
| `expires_at` | BIGINT / INTEGER NOT NULL | — | Retention boundary |

Exactly one of `run_id` and `rejection_id` is non-NULL. Large Debug payloads are not stored in relational rows. They are segmented JSONL under the managed `observation-debug` data directory, capped at 64 MiB per Run and 2 GiB retained total. The relational manifest permits startup reconciliation of tombstones and orphan managed directories.

**索引**：`debug_manifests_expiry_idx`

### observation_events

| Column | Type | Default | Description |
|---|---|---|---|
| `sequence` | BIGINT / INTEGER PK | PostgreSQL `nextval`; SQLite explicit | Monotonic persisted sequence; SSE event IDs use this value |
| `occurred_at` | BIGINT / INTEGER NOT NULL | — | Event time |
| `interaction_id` | TEXT FK | NULL | Interaction association; ON DELETE CASCADE |
| `run_id` | TEXT FK | NULL | Run association; ON DELETE CASCADE |
| `rejection_id` | TEXT FK | NULL | Rejected Request association; ON DELETE CASCADE |
| `kind` | TEXT NOT NULL | — | Typed Observation event kind |
| `payload` | JSONB / TEXT NOT NULL | — | Redacted structured event payload |
| `expires_at` | BIGINT / INTEGER NOT NULL | — | Retention boundary |

**索引**：`observation_events_interaction_idx`、`observation_events_run_idx`、`observation_events_rejection_idx`、`observation_events_expiry_idx`

Credential discoveries use the ordinary `credential_mappings_created` event kind with payload `{"discoveries":[{"rule_ids":[...],"source_types":[...]}]}`. Each element represents one actually created mapping, not one occurrence or rule match. No secret value, recoverable placeholder, fingerprint, or message excerpt is stored in this metadata. Summaries aggregate retained events by `interaction_id` and order by the latest discovery time; mapping reuse, renewal, and restoration do not produce discovery events.

Migration `0036_credential_discovery_coverage` introduces no new tables or columns. It sets `interaction_observations.observation_gap` for retained pre-feature rows on both backends because they lack discovery metadata; it does not reconstruct discoveries from mapping storage or historical content. Discovery events follow the existing Observation retention and cascade boundaries. Clearing them does not alter `reversible_redaction_mappings` or make valid reuse a new discovery.

### Usage statistics and retention

`UsageStatsStore` computes overview, hourly, model, provider, API-key, and Route-scheduling projections directly from `model_turn_observations` and `target_attempt_observations`; there is no separate `usage_stats` table. Provider-reported values are counted once per attempt, and a dimension remains NULL when any applicable attempt is unknown. Route scheduling uses 24-hour token totals and one-hour attempt success/latency from Target attempts; a failed refresh returns the last successful in-process snapshot marked stale.

Observation rows, Rejected Requests, events, manifests, and managed Trace segments use the `log_retention_days` setting, default seven days. Expiry and Clear History preserve `running` and `waiting_client` Interactions; Clear History reports them as skipped. Trace deletion is tombstoned and reconciled before owning rows are removed.

Debug starts disabled for each process and requires explicit confirmation to enable. A Run snapshots Debug at admission, while a rejected request snapshots it at ingress. Credential headers, URL userinfo, credential-like query values, and explicit structured credential fields are redacted before queueing or persistence, but other prompts, business content, and tool inputs/results may remain sensitive. Capture records application-level HTTP, SSE, and WebSocket messages; it is not TLS, TCP, HTTP/2-frame, or packet capture.

Interaction and Rejected Request bundles are streamed, versioned point-in-time ZIPs. Their manifests fix a through-sequence and report `complete`, `partial`, or `none`, per-Run capture state, byte counts, and missing reasons. Authenticated issuance returns a high-entropy, 60-second, single-use download ticket; expiry, replay, another resource, or process restart makes it unusable. Observation realtime delivery, Debug state, Trace storage, and tickets are single-Gateway only; cluster-wide fanout and shared Trace storage are not implemented.

---

## web_providers

Local Web Search 的内部 Search 与 Fetch 上游配置。每个部署恰好有一条不可删除的 `local` 记录；Exa 与智谱使用 `api_key`。Codex Search Backend 不属于此表。

| Column | Type | Default | Description |
|---|---|---|---|
| `id` | TEXT PK | — | 主键，UUID |
| `name` | TEXT NOT NULL UNIQUE | — | 管理员可见名称 |
| `kind` | TEXT NOT NULL | — | `local`、`exa` 或 `zhipu`；`local` 由唯一部分索引约束为单例 |
| `api_key` | TEXT | NULL | Exa/智谱必填；Local 必须为空；Admin API 不回显 |
| `use_proxy` | INTEGER/BOOLEAN NOT NULL | `0`/`FALSE` | 是否通过 Gateway `proxy_url` 出站 |
| `local_engines` | TEXT/JSONB | NULL | 仅 Local 使用的 HTML 检索引擎配置；至少一个引擎启用，私有设置不由 Admin API 回显 |
| `last_test_success` | INTEGER | NULL | 最近一次连接测试是否成功 |
| `last_test_at` | TEXT | NULL | 最近一次连接测试时间 |
| `created_at` | TEXT | `datetime('now')` | 创建时间 |
| `updated_at` | TEXT | `datetime('now')` | 更新时间 |

表不再保留 `provider_id`、Codex、Brave 或 Tavily 行。凭据约束要求远程 Provider 具有非空 `api_key` 且没有 `local_engines`，Local 则恰好相反。

---

## turn_chain_nodes

Generation Chain（其 Responses 投影为 Response Chain）、Agent Turn 与 Search Turn 共用的 principal-scoped 会话 DAG。节点不可变；Generation Chain 在调用方未给出父节点时只以严格 canonical 历史前缀自动选择同 Principal 父链，Web Search continuation 只接受调用方显式给出的同 Principal 父节点；两者都会物化完整祖先链形成独立分支。TTL 到期后仅在不存在存活子节点时清理。

| Column | Type | Default | Description |
|---|---|---|---|
| `id` | TEXT PK | — | opaque Turn ID |
| `kind` | TEXT NOT NULL | — | `response`、`agent` 或 `web_search` |
| `parent_id` | TEXT | NULL | 父节点（FK → turn_chain_nodes.id, ON DELETE RESTRICT） |
| `principal` | TEXT NOT NULL | — | 所属调用主体 |
| `payload_version` | INTEGER NOT NULL | — | Canonical transcript / Search Turn payload 版本 |
| `payload` | JSON/TEXT NOT NULL | — | Generation Chain 节点的 canonical 输入 delta、最终输出和 resolved profile delta；或 Agent/Search Turn payload（不含网页正文和内部 Agent transcript） |
| `prefix_namespace` | TEXT | NULL | Reusable Response Prefix 的 Principal 外 Target/Provider/config/model/effective-profile namespace hash；仅可安全复用的已完成 Response 节点写入 |
| `prefix_fingerprint` | TEXT | NULL | 节点完整 canonical effective context 的 SHA-256 指纹 |
| `prefix_item_count` | INTEGER | NULL | 指纹覆盖的完整 canonical item 数量 |
| `prefix_completed_at` | BIGINT/INTEGER | NULL | 上游 `completed` 时间（Unix 毫秒），用于同长度候选的确定性排序 |
| `created_at` | BIGINT/INTEGER NOT NULL | — | 创建时间（Unix 毫秒） |
| `expires_at` | BIGINT/INTEGER NOT NULL | — | 到期时间（Unix 毫秒） |

**索引**:`idx_turn_chain_parent`、`idx_turn_chain_principal_kind`、`idx_turn_chain_expiry`、`idx_turn_chain_reusable_prefix`(`principal, kind, prefix_namespace, prefix_fingerprint, prefix_item_count DESC, prefix_completed_at DESC, expires_at, id DESC`,仅索引非 NULL namespace)

Generation Chain 的新 Response payload 使用版本 5，工具结果保存可选 `content_kind`（`json` 或 `content_blocks`），缺失值表示旧记录没有语义证明。普通 Tool Text 与此前编码成字符串的 content blocks 使用内部消息标记区分；读取版本 1–4 时仅从真实 `AiItem.meta` 移除该保留键，不改写业务 JSON 或无关元数据。旧记录仍可读取，但可逆脱敏开启时会拒绝无法明确解释的工具数组历史。Agent/Search payload 的版本规则不变；此调整不新增 SQL 列。

---

## native_compactions

原生压缩的受保护核心记录，不属于 Observation。登记事务完成后即可解析，交付确认不作为第二次开启解析的开关；默认 pending 保留一小时，确认交付或合法引用后至少保留七天。引用与分支延长必要前序记录和 Generation 祖先；过期记录不被回传复活。

| Column | Type | Default | Description |
|---|---|---|---|
| `id` | TEXT PK | — | 平台内部不可变登记 ID，不改写原生 compaction ID |
| `principal` | TEXT NOT NULL | — | API Key 对应的隔离身份 |
| `source_generation_id` | TEXT FK | NULL | 已确认来源，引用 `turn_chain_nodes.id`，ON DELETE RESTRICT |
| `operation_id` | TEXT NOT NULL | — | 产生状态的操作 ID，不是 upstream response ID |
| `payload` | TEXT NOT NULL | — | 原生完整窗口、Target/账号配置 namespace、模型/协议及前序来源；SQLite 校验 JSON |
| `created_at` | BIGINT / INTEGER NOT NULL | — | 登记时间，Unix 毫秒 |
| `delivered_at` | BIGINT / INTEGER | NULL | 已确认交付时间 |
| `referenced_at` | BIGINT / INTEGER | NULL | 首次合法引用时间 |
| `expires_at` | BIGINT / INTEGER NOT NULL | — | 保留期，Unix 毫秒 |

**索引**：`idx_native_compactions_expiry`。普通诊断与错误不复制 `payload`。

### native_compaction_states

| Column | Type | Description |
|---|---|---|
| `record_id` | TEXT FK NOT NULL | 登记 ID，ON DELETE CASCADE |
| `principal` | TEXT NOT NULL | 查询隔离身份 |
| `native_identity` | TEXT NULL | Provider 原生状态 ID，可缺省 |
| `fingerprint` | TEXT NOT NULL | 完整原生状态的稳定精确指纹 |
| `state_payload` | TEXT NOT NULL | 用于内容核验的完整原生状态；SQLite 校验 JSON |

主键 `(record_id, fingerprint)`；分别按 `(principal, fingerprint)`、`(principal, native_identity)` 建索引。索引不设跨登记唯一性：同 ID 不同内容、同状态不同来源不能互相覆盖，解析时显式处理冲突/歧义。

### native_compaction_sources

| Column | Type | Description |
|---|---|---|
| `record_id` | TEXT FK NOT NULL | 新登记，ON DELETE CASCADE |
| `source_id` | TEXT FK NOT NULL | 不可变前序压缩登记，ON DELETE RESTRICT |

主键 `(record_id, source_id)`；禁止自引用，`idx_native_compaction_source` 索引前序来源。新记录只引用已存在且同 Principal 的有效记录，形成不可变边界图。

## history_markers

History Marker Store 的持久化事实源。每行只保存一个受保护 Thinking block，或一个 Platform Tool Execution 的完整 call 与 terminal result；`principal + reference` 解析不依赖周边历史。Platform execution 通过条件更新从 `pending` 原子进入 `running`；失效 lease 转为 `interrupted`，绝对 deadline 到期转为 `failed`，均不会被其他 Gateway 自动接管。

| Column | Type | Default | Description |
|---|---|---|---|
| `reference` | TEXT PK | — | 客户端 Markdown 中可见的 opaque Marker reference |
| `principal` | TEXT NOT NULL | — | 所属认证 Principal；跨 Principal 查询按不存在处理 |
| `kind` | TEXT NOT NULL | — | `platform` 或 `thinking` |
| `activity` | TEXT NOT NULL | — | 注册元数据提供的安全英文活动说明 |
| `tool_id` | TEXT | NULL | Platform Tool 注册 ID；Thinking Marker 必须为 NULL |
| `call_payload` | JSON/TEXT | NULL | 单个完整 Platform call；Thinking Marker 必须为 NULL |
| `segment_payload` | JSON/TEXT | NULL | Thinking block 与可选来源绑定，或 terminal Platform call/result 对 |
| `execution_state` | TEXT | NULL | Platform 的 `pending`、`running`、`completed`、`failed` 或 `interrupted` |
| `execution_owner` | TEXT | NULL | 当前原子 claim owner；terminal 时清空 |
| `lease_expires_at` | BIGINT/INTEGER | NULL | running owner lease 到期时间（Unix 毫秒） |
| `execution_deadline` | BIGINT/INTEGER | NULL | 创建时固定的工具绝对执行期限（Unix 毫秒） |
| `published_at` | BIGINT/INTEGER | NULL | Marker 首次进入客户端输出的本地发布时间 |
| `created_at` | BIGINT/INTEGER NOT NULL | — | 创建时间（Unix 毫秒） |
| `updated_at` | BIGINT/INTEGER NOT NULL | — | 最近状态迁移时间（Unix 毫秒） |
| `expires_at` | BIGINT/INTEGER NOT NULL | — | pending 或 Generation Chain 引用保留期限 |

**索引**：`idx_history_markers_principal_reference`、`idx_history_markers_execution`、`idx_history_markers_expiry`

Platform terminal `segment_payload` 的工具结果保留同一 `content_kind` 语义，Hook 和隐藏历史重建不能把缺失值自动补为可信 JSON。业务 JSON 中同名字段只是业务数据，不作为内部语义标记。

Thinking `segment_payload` 的可选 `source` 保存 `namespace`、`protocol`、`actual_model` 和 `target_id`，用于选择原生或降级回放；旧 JSON 缺失该字段时保持来源未知。来源是平台私有元数据，不发送给客户端或上游。Target 请求副本的降级不修改 `block` 或来源，切回兼容来源时可以恢复原密文。该可选 JSON 字段不改变 SQLite/PostgreSQL 表结构或现有 migration。

---

## reversible_redaction_mappings

Persistent reversible secret mappings, isolated solely by the API Key's authenticated Principal. References can be restored across conversations, branches and restarts. There are deliberately no conversation or Generation Chain foreign keys: deleting source history must not delete a still-live mapping. Plaintext is permitted within the existing local database security boundary and must never be included in diagnostics or storage errors.

| Column | Type | Default | Description |
|---|---|---|---|
| `reference` | TEXT PK NOT NULL | — | Opaque `~stravia-secret:<32 lowercase UUID hex digits>~` placeholder; never rebound |
| `principal` | TEXT NOT NULL | — | Existing `api-key:<API Key ID>` Principal identity; foreign Principal lookups reveal no mapping |
| `secret` | TEXT NOT NULL | — | Exact secret plaintext, including multiline values |
| `published_at` | BIGINT / INTEGER | NULL | First publication time, Unix milliseconds |
| `created_at` | BIGINT / INTEGER NOT NULL | — | Creation time, Unix milliseconds |
| `updated_at` | BIGINT / INTEGER NOT NULL | — | Last retention/publication update, Unix milliseconds |
| `expires_at` | BIGINT / INTEGER NOT NULL | — | Exclusive validity boundary, Unix milliseconds |

**Indexes**: `idx_reversible_redaction_mappings_principal_expiry` on `(principal, expires_at)` and `idx_reversible_redaction_mappings_expiry` on `expires_at`.

Creation retains an unpublished mapping for one hour. Publication extends it to at least seven days; Generation Chain retention only extends still-live published mappings and never shortens their lifetime. Expired mappings neither participate in restoration or known-secret detection nor revive through publication or renewal. Cleanup deletes expired rows. Disabling new redaction does not delete mappings or prevent restoration of live references.

Interning serializes lookup and insertion within a database transaction (SQLite `BEGIN IMMEDIATE`; PostgreSQL Principal-scoped advisory transaction lock). Concurrent requests therefore reuse the same live mapping for identical plaintext within one Principal. Expired rows are not reused, and a fresh UUID is allocated instead. Secret plaintext is not B-tree indexed, avoiding PostgreSQL index-size limits for long private keys; Principal and validity indexes bound the lookup scope.

---

## agent_definition_revisions

代码注册的不可变 Agent Definition 修订。`definition_id + version` 固定 instructions、工具 allowlist、预算、Artifact policy 与 output schema。

| Column | Type | Default | Description |
|---|---|---|---|
| `definition_id` | TEXT NOT NULL | — | 稳定 Definition ID |
| `slug` | TEXT NOT NULL | — | 外部工具名称使用的稳定 slug |
| `version` | INTEGER NOT NULL | — | 修订号，大于 0 |
| `spec_hash` | TEXT NOT NULL | — | 新记录为递归排序 JSON 对象键后的 Definition 内容 SHA-256；既有 hash 保持原值，内容等价比较不受键顺序影响 |
| `spec_json` | JSON/TEXT NOT NULL | — | 完整不可变 Definition spec |
| `created_at` | BIGINT/INTEGER NOT NULL | — | 创建时间（Unix 毫秒） |

**主键**：`(definition_id, version)`；**唯一约束**：`(slug, version)`

---

## agent_definition_configs

管理员可变的 Agent Definition 运行配置；不承载用户自定义 prompt、工具或 schema。

| Column | Type | Default | Description |
|---|---|---|---|
| `definition_id` | TEXT PK | — | Definition ID |
| `enabled` | BOOLEAN/INTEGER NOT NULL | false | 是否向内部调用面公开 |
| `model_id` | TEXT | NULL | 绑定逻辑模型（FK → models.id, ON DELETE SET NULL） |
| `thinking_level` | TEXT | NULL | 内部 Model Turn 使用的思考等级：`off`、`minimal`、`low`、`medium`、`high`、`xhigh` 或 `max` |
| `updated_at` | BIGINT/INTEGER NOT NULL | — | 更新时间（Unix 毫秒） |

---

## artifacts

API key principal-scoped 的不可变媒体／文件对象。上传完成前为 `staging`，完成后为 `ready`；公共稳定引用为 `https://stravia/artifact/<id>`，内部 Agent input 使用 opaque `ArtifactId`。引用不授予访问权。逻辑过期不能复活；已开始的读取与未过期下载授权只保护物理内容，不延长逻辑保留期。

| Column | Type | Default | Description |
|---|---|---|---|
| `id` | TEXT PK | — | opaque Artifact ID |
| `principal` | TEXT NOT NULL | — | 所属调用主体 |
| `mime_type` | TEXT NOT NULL | — | 声明 MIME type |
| `size` | BIGINT/INTEGER NOT NULL | — | 字节数 |
| `backend_key` | TEXT NOT NULL | — | 本地/S3-compatible object key |
| `storage_backend` | TEXT NOT NULL | `'internal'` | `internal` 或 `s3`；既有对象保持内部存储 |
| `storage_endpoint` | TEXT NULL | NULL | S3 对象所属 endpoint，内部存储为空 |
| `storage_bucket` | TEXT NULL | NULL | S3 对象所属 bucket，内部存储为空 |
| `state` | TEXT NOT NULL | — | `staging` 或 `ready` |
| `expires_at` | BIGINT/INTEGER NOT NULL | — | 到期时间（Unix 毫秒） |
| `created_at` | BIGINT/INTEGER NOT NULL | — | 创建时间（Unix 毫秒） |

**索引**：`idx_artifacts_expiry`

---

## artifact_download_grants

单文件临时下载授权的不可逆校验与物理清理保护。平台 URL 的 token 只向下载方交付，数据库仅保存 SHA-256；原生 S3 预签名 URL 同样建立到期保护记录。访问下载 URL 不刷新 Artifact 保留期。授权默认十五分钟；S3 还受签名凭据到期时间约束。

| Column | Type | Default | Description |
|---|---|---|---|
| `token_hash` | TEXT PK | — | 随机下载 token 的 SHA-256，不保存明文 token 或签名 URL |
| `artifact_id` | TEXT NOT NULL | — | Artifact（FK → artifacts.id, ON DELETE CASCADE） |
| `expires_at` | BIGINT/INTEGER NOT NULL | — | 授权及物理删除保护截止时间（Unix 毫秒） |

**索引**：`idx_artifact_download_grants_hold` on `(artifact_id, expires_at)`。

---
## media_derivatives

Media Understanding 源 Artifact 到内部 JPEG Media Derivative 的 principal-scoped、write-once 关系。公开输入与 Media Report 只引用 source Artifact ID；模型只读取 derivative Artifact bytes。

| Column | Type | Default | Description |
|---|---|---|---|
| `principal` | TEXT NOT NULL | — | source 与 derivative 共同所属调用主体 |
| `source_artifact_id` | TEXT PK | — | 源 Artifact（FK → artifacts.id, ON DELETE CASCADE） |
| `derivative_artifact_id` | TEXT NOT NULL UNIQUE | — | 内部 JPEG Artifact（FK → artifacts.id, ON DELETE CASCADE） |
| `created_at` | BIGINT/INTEGER NOT NULL | — | 建立 write-once mapping 的时间（Unix 毫秒） |

`source_artifact_id` 与 `derivative_artifact_id` 必须不同。任一 Artifact 删除时 mapping 级联删除；实现不会为已有 source identity 替换或重算 derivative。

---


## artifact_uploads

Artifact multipart 上传会话；只存 upload token hash，完成后删除。

同一 Principal 的未完成且未过期任务最多十六个，声明大小合计最多 400 MiB；单文件最多 100 MiB。完成任务不占暂存名额，不限制已保存对象的聚合容量。创建准入通过 SQLite 写事务或 PostgreSQL Principal advisory lock 协调。

| Column | Type | Default | Description |
|---|---|---|---|
| `id` | TEXT PK | — | upload ID |
| `artifact_id` | TEXT NOT NULL | — | Artifact（FK → artifacts.id, ON DELETE CASCADE） |
| `principal` | TEXT NOT NULL | — | 所属调用主体 |
| `token_hash` | TEXT NOT NULL | — | upload token SHA-256 |
| `declared_size` | BIGINT/INTEGER NOT NULL | — | 声明总字节数 |
| `received_size` | BIGINT/INTEGER NOT NULL | 0 | 已上传字节数 |
| `expires_at` | BIGINT/INTEGER NOT NULL | — | 到期时间（Unix 毫秒） |
| `created_at` | BIGINT/INTEGER NOT NULL | — | 创建时间（Unix 毫秒） |

**索引**：`idx_artifact_uploads_expiry`

---

## artifact_upload_parts

| Column | Type | Default | Description |
|---|---|---|---|
| `upload_id` | TEXT NOT NULL | — | 上传会话（FK → artifact_uploads.id, ON DELETE CASCADE） |
| `part_number` | INTEGER NOT NULL | — | 从 1 开始的 part 序号 |
| `etag` | TEXT NOT NULL | — | part 内容摘要 |
| `size` | BIGINT/INTEGER NOT NULL | — | part 字节数 |

**主键**：`(upload_id, part_number)`

---


## settings

系统配置键值对。`web_search_config` 保存带 revision 的完整替换配置；Web Access 保存 Local backend 使用的有序 Search / Fetch source IDs。`log_retention_days` 控制 Observation、Rejected Request、event、Debug manifest 与托管 Trace segment 的共同保留期；未设置或不可读时运行时使用 7 天。

`artifact_settings` 原子保存 `client_base_url`、`external_signed_downloads`、`file_public_base_url`、`upload_prompt_injection` 与可选 `s3`。两个开关默认关闭；地址保存完整 base URL，不从后续 Host／转发头更新。`s3` 包含 `endpoint`、`region`、`bucket`、`access_key_id`、`secret_access_key`、可选 `session_token` 与 Unix 毫秒 `credentials_expires_at`。文件相关读取对配置加载或解析失败明确报错，不伪装为关闭。

内部键 `artifact_upload_signing_key` 保存随机签名密钥，通过冲突忽略插入保证并发初始化与重启稳定；通用管理设置读写拒绝访问此键。上传凭据本身不入库：签名载荷包含 Principal、固定十五分钟截止时间、随机 nonce 与限定的上传用途。上传认证每次检查所属 API Key 状态；重放脱敏识别保留的凭据语法，不依赖有效凭据表或一般可逆脱敏开关。

| Column | Type | Default | Description |
|---|---|---|---|
| `name` | TEXT PK | — | 配置键 |
| `value` | TEXT NOT NULL | — | 配置值 |
| `updated_at` | TEXT | `datetime('now')` | 更新时间 |

---

## 迁移说明

SQLite 与 PostgreSQL 的 SQLx versioned migrations 是 schema 的唯一来源。Gateway 在任一支持后端启动时、监听 Proxy 和 Admin API 之前应用尚未执行的 migration；migration 失败会终止启动。

Web Research migration 10 是历史 migration：新增旧 `api_keys.allow_web_research`，删除旧 Codex `web_providers` 行和 `provider_id`，加入旧 Research Turn identity，并写入 `web_research_config`。该 migration 已执行版本保持不可变。

Media Understanding migration 11 新增旧 `api_keys.allow_media_understanding`；migration 12 新增 `media_derivatives` write-once mapping。已执行的 migration 必须保持不可变，后续 schema 变更使用新的版本号。
Reusable Response Prefix migration 15 为 `turn_chain_nodes` 增加 nullable prefix namespace/fingerprint/item-count/completed-at 字段与 lookup index。升级前节点不回填索引；只有升级后完整交付、上游 `completed` 且 Hook 未改变输出语义的 Response 节点可写入。该 migration 同时执行 Anonymous Principal clean cutover：删除 `principal = 'anonymous'` 的 Turn Chain、Artifact 和 upload 数据，关联子表按外键级联。认证 API key 数据保持不变。

Advanced Capabilities / Web Search migration 18 是 destructive clean cutover：SQLite 与 PostgreSQL 都删除旧 `allow_web_research`、`allow_media_understanding` 和 `web_search_injection_enabled`，加入 `transparent_injection_enabled`、`inject_media_understanding` 和 `inject_web_search`；把旧自动行为映射到对应 selection；把 settings key 移到 `web_search_config`；删除旧 Research Turn；并把 kind 约束切换为 `web_search`。SQLite 重建 Turn 表时保留 migration 15 的 reusable-prefix 字段和索引。升级前必须备份数据库和匹配二进制；回滚必须恢复 migration 18 之前的数据库，不能只回退应用文件。

Revisioned Provider Catalog migration 20 不改变表 shape。它仅把可由既有 `preset_key` 或旧 source identity 确定的 Provider `models_source` 转换为 `catalog`，并补齐缺失的 Catalog Provider ID；无法安全确定 identity 的行保持原值，以便管理员诊断和修复。Provider 凭据、channel、路由与既有 Provider Model metadata 均不修改。

History Marker migration 22 新增 Principal-scoped `history_markers` 表及 Platform Tool Execution 的 durable claim、lease、deadline、terminal、publication 与 retention 字段。隐藏 payload 沿用现有 SQLite/PostgreSQL 部署安全边界，不引入独立加密密钥。

Route Target Aggregate migration 27 先把仅存在于旧 `models.target_provider` / `models.target_model` 的主 Target 补入 `model_backends`，再删除这两个重复列，并为大小写敏感的 Route ID `models.name` 建立唯一索引。升级后 Route 与全部 Targets 由同一聚合写入事务维护。

Web Access Adapter migration 28 是不兼容旧二进制的 clean cutover：把 kind 约束收紧为 `local|exa|zhipu`，删除 Brave/Tavily 行，加入 `use_proxy` 与 Local Search Engine 配置，并写入唯一 Local Provider。Search/Fetch 列表保留既有 Exa/智谱顺序；移除旧 kind 后为空的列表改为仅 Local。升级前必须备份数据库；回滚必须恢复 migration 28 之前的数据库，不能只回退应用文件。

Layer Route Target Selection migration 31 删除 `model_backends.weight`，把既有 Target Priority 全部重置为 `0`，增加 First Token Timeout、Target Retry Budget 与 Target Cooldown，并把旧 `weighted|priority|cooldown|latency` Strategy 归一化为 `traffic_equalization|latency_preference`。

Route Target Enabled migration 32 为 `model_backends` 增加缺省为已启用的 `enabled`；既有 Target 全部保持参与选择，写入省略该字段时也按已启用处理。

Interaction Observation migration 34 是 clean cutover：SQLite 与 PostgreSQL 都先删除 `request_logs` 及其全部历史行，不从 Generation Chain 回填，再创建 Interaction、Inference Run、Model Turn、Target attempt、Rejected Request、Debug manifest、event 与单调 event sequence schema。升级后 Request Records、usage analytics 与 Route scheduling 只读取 Observation；没有旧日志别名或 dual-write。升级前若需要旧日志必须另行备份；仅回退应用二进制不能恢复已删除行。

Allowance Samples migration 29 新增 `provider_allowance_samples`。样本随 Provider 删除而级联删除；应用按 14 天 TTL 清理，预报只读取当前重置窗口内且语义一致的样本。

Route Display Name migration 30 把 `models.name` 原值逐字节迁移为 `models.model_id`，新增 nullable `display_name`，并把 `idx_models_route_id` 移到 `model_id`。迁移不会从 Canonical Model 目录推断历史展示名称，也不会改写 Target 或 API Key 的内部 Route 主键绑定。

`0036_credential_discovery_coverage`、`0037_route_native_compaction` 与 `0038_native_compaction` 保持既有版本、内容与校验和不变。`0037` 曾为 `models` 新增 `compaction_enabled` 与 `compaction_threshold`；`0039_remove_route_compaction_policy` 先删除带有交叉列 CHECK 约束的阈值列，再删除开关列，只移除这两项已废弃的 Route 策略设置，不删除 Route、Target 或原生压缩状态。客户端显式压缩控制直接透传至当前选中的 Target，平台不存储自动压缩策略。`0038` 创建的 `native_compactions`、`native_compaction_states` 和 `native_compaction_sources` 及其既有数据、`turn_chain_nodes` 引用保持不变；不从 Observation 或历史内容回填压缩记录。两个后端按版本顺序应用迁移；SQLite 使用与既有 DROP COLUMN 迁移相同的现代 SQLite 要求（3.35.0 或更新）。

SQLite 与 PostgreSQL 必须保持 API Key 字段默认值、Turn kind、settings identity、唯一约束和 Artifact 外键等价。

`0040_interaction_input_preview` 保留主仓库已经应用的输入预览列迁移；`0041_artifact_transfers` 同时为两个后端增加存储位置元数据与下载授权表。迁移不扫描媒体正文、不抓取旧 URL、不改写旧历史／工具记录，也不续期或删除既有对象。SQLite 通过跨 Store 实例的文件锁保护进行中的读取，PostgreSQL 使用独立连接池中的事务 advisory lock；清理取得排他保护并重新检查逻辑过期和授权后才删除，失败不报告已删除。

首个 migration 直接使用最终表名 `models`、`model_backends` 和 `api_key_models`；后续 schema 变更通过 SQLite/PostgreSQL 对应版本的 migration 演进。MySQL 不受支持。

`deploy/schema/postgres.sql` 是由 `stravia-tools dump-schema --backend postgres` 从 PostgreSQL migration 导出的 DBA 审阅参考产物，不能直接执行来初始化数据库：直接执行不会记录 SQLx migration 历史。应让 `stravia-server` 对空数据库应用 migrations。MySQL reference schema 不再提供。
