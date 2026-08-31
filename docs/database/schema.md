# Database Schema

Stravia supports **SQLite** (default) and **PostgreSQL** only. Both use the logical schema below; PostgreSQL uses native `BOOLEAN`, `TIMESTAMPTZ`, and `BIGINT` where SQLite uses `INTEGER` or `TEXT`.

## Entity Relationship

```
providers ──1:N── model_backends ──N:1── models ──M:N── api_keys (via api_key_models)
    ├──1:1── provider_oauth_credentials
    └──1:N── provider_models ──1:N── provider_model_cost_rules
web_providers (Local Web Search / Fetch upstreams)
request_logs (append-only)
turn_chain_nodes (principal-scoped Response / Agent / Web Search DAG)
history_markers (principal-scoped hidden history and Platform execution state)
agent_definition_revisions ──1:1── agent_definition_configs
artifacts ──1:0..1── artifact_uploads ──1:N── artifact_upload_parts
    └──1:0..1── media_derivatives ──1:1── artifacts (JPEG derivative)
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

虚拟模型配置，定义客户端请求的模型名如何映射到后端。

| Column | Type | Default | Description |
|---|---|---|---|
| `id` | TEXT PK | — | 主键，UUID |
| `name` | TEXT NOT NULL | — | 显示名称，同时作为客户端请求的模型匹配键（路由唯一键的一部分） |
| `balance` | TEXT | `'weighted'` | 多后端负载均衡策略：`weighted`、`priority`、`cooldown`、`latency` |
| `target_provider` | TEXT NOT NULL | — | 默认后端 provider ID（FK → providers.id） |
| `target_model` | TEXT NOT NULL | — | 默认后端使用的上游模型名 |
| `is_enabled` | INTEGER | `1` | 是否启用 |
| `priority` | INTEGER | `0` | 优先级（预留） |
| `created_at` | TEXT | `datetime('now')` | 创建时间 |

---

## model_backends

模型后端列表，一个 model 可对应多个 provider + 上游模型的组合。

| Column | Type | Default | Description |
|---|---|---|---|
| `id` | TEXT PK | — | 主键，UUID |
| `model_id` | TEXT NOT NULL | — | 所属模型 ID（FK → models.id, ON DELETE CASCADE） |
| `provider_id` | TEXT NOT NULL | — | 供应商 ID（FK → providers.id） |
| `model` | TEXT NOT NULL | — | 上游模型名（发送给 provider 的模型标识） |
| `weight` | INTEGER | `100` | 权重（`weighted` 策略下生效） |
| `priority` | INTEGER | `1` | 优先级，数值越小越优先（`priority` 策略下生效） |
| `thinking_level_map` | JSON | 七行 Hidden Mapping | Target 的七行 Thinking Level Map，包含 Control 与 Generated/Overridden 来源；SQLite 使用 JSON 文本，PostgreSQL 使用 JSONB |
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

## request_logs

请求日志（追加写入，记录每次代理请求的完整信息）。

| Column | Type | Default | Description |
|---|---|---|---|
| `id` | TEXT PK | — | 日志 ID |
| `created_at` | INTEGER | `0` | Unix 毫秒时间戳 |
| `api_key_id` | TEXT | NULL | 认证使用的 API Key ID |
| `api_key_name` | TEXT | NULL | API Key 名称（快照） |
| `client_protocol` | TEXT | NULL | 客户端协议（如 `openai/chat/v1`） |
| `upstream_protocol` | TEXT | NULL | 上游协议 |
| `provider_id` | TEXT | NULL | 供应商 ID |
| `provider_name` | TEXT | NULL | 供应商名称（快照） |
| `model_id` | TEXT | NULL | 匹配到的模型 ID |
| `model_name` | TEXT | NULL | 模型名称（快照） |
| `upstream_url` | TEXT | NULL | 上游请求 URL |
| `client_model` | TEXT | NULL | 客户端请求中的模型名 |
| `upstream_model` | TEXT | NULL | 实际发送给上游的模型名 |
| `method` | TEXT | NULL | HTTP 方法 |
| `path` | TEXT | NULL | 请求路径 |
| `client_request_headers` | TEXT | NULL | 客户端请求头（JSON，可选记录） |
| `client_request_body` | TEXT | NULL | 客户端请求体（可选记录） |
| `client_response_headers` | TEXT | NULL | 客户端响应头（JSON，可选记录） |
| `client_response_body` | TEXT | NULL | 客户端响应体（可选记录） |
| `upstream_request_headers` | TEXT | NULL | 上游请求头（JSON，可选记录） |
| `upstream_request_body` | TEXT | NULL | 上游请求体（可选记录） |
| `upstream_response_headers` | TEXT | NULL | 上游响应头（JSON，可选记录） |
| `upstream_response_body` | TEXT | NULL | 上游响应体（可选记录） |
| `upstream_status_code` | INTEGER | NULL | 上游 HTTP 状态码 |
| `client_status_code` | INTEGER | NULL | 返回给客户端的 HTTP 状态码 |
| `latency_total_ms` | INTEGER | NULL | 总延迟（毫秒） |
| `latency_upstream_ms` | INTEGER | NULL | 上游延迟（毫秒） |
| `input_tokens` | INTEGER | `0` | 输入 token 数 |
| `output_tokens` | INTEGER | `0` | 输出 token 数 |
| `cache_read_tokens` | INTEGER | `0` | 缓存输入（命中）token 数 |
| `cache_write_tokens` | INTEGER | `0` | 缓存输出（创建）token 数 |
| `thinking_level` | TEXT | NULL | 请求实际使用的思考等级 |
| `is_stream` | INTEGER | `0` | 是否为流式请求 |
| `stream_chunks_count` | INTEGER | `0` | 流式分块数量 |
| `stream_first_chunk_ms` | INTEGER | NULL | 首个分块延迟（毫秒） |

**索引**：
- `idx_logs_created_at` on `created_at`
- `idx_logs_provider_id` on `provider_id`
- `idx_logs_client_status` on `client_status_code`
- `idx_logs_upstream_model` on `upstream_model`
- `idx_logs_api_key` on `api_key_id`
- `idx_logs_client_protocol` on `client_protocol`
- `idx_logs_upstream_protocol` on `upstream_protocol`

---

## web_providers

Local Web Search 的内部 Search 与 Fetch 上游配置。`api_key` 用于 Exa、Brave、Tavily 与智谱 Coding Plan；Codex Search Backend 不属于此表。

| Column | Type | Default | Description |
|---|---|---|---|
| `id` | TEXT PK | — | 主键，UUID |
| `name` | TEXT NOT NULL UNIQUE | — | 管理员可见名称 |
| `kind` | TEXT NOT NULL | — | `exa`、`brave`、`tavily` 或 `zhipu` |
| `api_key` | TEXT NOT NULL | — | Web Provider 凭据；Admin API 不回显 |
| `last_test_success` | INTEGER | NULL | 最近一次连接测试是否成功 |
| `last_test_at` | TEXT | NULL | 最近一次连接测试时间 |
| `created_at` | TEXT | `datetime('now')` | 创建时间 |
| `updated_at` | TEXT | `datetime('now')` | 更新时间 |

表不再保留 `provider_id` 或 Codex 行。

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

---

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
| `segment_payload` | JSON/TEXT | NULL | Thinking block，或 terminal Platform call/result 对 |
| `execution_state` | TEXT | NULL | Platform 的 `pending`、`running`、`completed`、`failed` 或 `interrupted` |
| `execution_owner` | TEXT | NULL | 当前原子 claim owner；terminal 时清空 |
| `lease_expires_at` | BIGINT/INTEGER | NULL | running owner lease 到期时间（Unix 毫秒） |
| `execution_deadline` | BIGINT/INTEGER | NULL | 创建时固定的工具绝对执行期限（Unix 毫秒） |
| `published_at` | BIGINT/INTEGER | NULL | Marker 首次进入客户端输出的本地发布时间 |
| `created_at` | BIGINT/INTEGER NOT NULL | — | 创建时间（Unix 毫秒） |
| `updated_at` | BIGINT/INTEGER NOT NULL | — | 最近状态迁移时间（Unix 毫秒） |
| `expires_at` | BIGINT/INTEGER NOT NULL | — | pending 或 Generation Chain 引用保留期限 |

**索引**：`idx_history_markers_principal_reference`、`idx_history_markers_execution`、`idx_history_markers_expiry`

---

## agent_definition_revisions

代码注册的不可变 Agent Definition 修订。`definition_id + version` 固定 instructions、工具 allowlist、预算、Artifact policy 与 output schema。

| Column | Type | Default | Description |
|---|---|---|---|
| `definition_id` | TEXT NOT NULL | — | 稳定 Definition ID |
| `slug` | TEXT NOT NULL | — | 外部工具名称使用的稳定 slug |
| `version` | INTEGER NOT NULL | — | 修订号，大于 0 |
| `spec_hash` | TEXT NOT NULL | — | Definition 内容 SHA-256 |
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

API key principal-scoped 的媒体/文件对象。上传完成前为 `staging`，完成后为 `ready`；Agent input 仅接受 opaque `ArtifactId`。

| Column | Type | Default | Description |
|---|---|---|---|
| `id` | TEXT PK | — | opaque Artifact ID |
| `principal` | TEXT NOT NULL | — | 所属调用主体 |
| `mime_type` | TEXT NOT NULL | — | 声明 MIME type |
| `size` | BIGINT/INTEGER NOT NULL | — | 字节数 |
| `backend_key` | TEXT NOT NULL | — | 本地/S3-compatible object key |
| `state` | TEXT NOT NULL | — | `staging` 或 `ready` |
| `expires_at` | BIGINT/INTEGER NOT NULL | — | 到期时间（Unix 毫秒） |
| `created_at` | BIGINT/INTEGER NOT NULL | — | 创建时间（Unix 毫秒） |

**索引**：`idx_artifacts_expiry`

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

系统配置键值对。`web_search_config` 保存带 revision 的完整替换配置；Web Access 保存 Local backend 使用的有序 Search / Fetch source IDs。

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

SQLite 与 PostgreSQL 必须保持 API Key 字段默认值、Turn kind、settings identity、唯一约束和 Artifact 外键等价。

首个 migration 直接使用最终表名 `models`、`model_backends` 和 `api_key_models`；后续 schema 变更通过 SQLite/PostgreSQL 对应版本的 migration 演进。MySQL 不受支持。

`deploy/schema/postgres.sql` 是由 `stravia-tools dump-schema --backend postgres` 从 PostgreSQL migration 导出的 DBA 审阅参考产物，不能直接执行来初始化数据库：直接执行不会记录 SQLx migration 历史。应让 `stravia-server` 对空数据库应用 migrations。MySQL reference schema 不再提供。
