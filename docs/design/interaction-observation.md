# Connect Client Interaction Observation 设计

## 1. 目标

把现有按单条请求展示的 `request_logs` 与仅限 debug 构建的 `wire_capture` 干净切换为统一的 `Interaction Observation`：

- 请求记录页以 `Connect Client Interaction` 为画布节点，以 `Generation Chain` 因果关系连接节点；
- 一次 Interaction 覆盖一次新 User 输入到最终生成响应之间的一个或多个 Inference Run，并允许 Run 子树分叉；
- 正在执行的状态、客户端可见输出和 Confirmed Upstream Usage 通过 SSE 在 1 秒内更新；
- Debug 按每个 Inference Run 准入时的进程级开关快照生效；
- Debug Trace 覆盖四个方向的应用协议级 wire、稳定 canonical checkpoint、HTTP/SSE/WebSocket；
- Interaction Debug Bundle 以版本化 ZIP 流式导出，明确完整、部分或缺失状态；
- 普通 Observation 不保存 wire payload、隐藏 Thinking、Platform Tool 参数或结果；
- Observation 只服务诊断，不成为推理执行、Generation Chain 或模型历史的事实源。

本设计同时适用于 SQLite 和 PostgreSQL 存储，但实时状态与 Debug 开关只承诺单 Gateway 实例。多实例聚合不在本设计范围内。

## 2. 非目标

- 不记录 TLS、TCP、HTTP/2 frame 或操作系统 packet capture。
- 不改变 Generation Chain 只保存完整交付 `completed` / `incomplete` 节点的契约。
- 不用 Observation 恢复模型历史、重放 Hook 或驱动 Inference Run。
- 不持久化画布坐标，不允许用户拖动或重连节点。
- 不保留旧 `/api/v1/logs` contract、旧请求表格、普通 CSV 导出或升级前 `request_logs` 数据。
- 不通过连接、IP、User-Agent 或模糊文本相似度推断 Generation Chain。
- 不承诺多 Gateway 实例之间的实时事件、Debug 开关或本地 Trace 文件共享。

## 3. 领域关系与不变量

### 3.1 Interaction 边界

1. 一条新的 canonical `User` item 开启新的 Connect Client Interaction。
2. 无法归入已有 Interaction、且不含 User item 的合法根请求也开启新的 Interaction。
3. 客户端公开工具调用结束当前 Inference Run；工具结果续接若没有新 User item，仍属于原 Interaction。
4. 同一父响应的并发续接属于同一 Interaction，并在详情中形成 Run 子树。
5. 新 User item 总是开启新 Interaction；原 Interaction 中尚无最终响应的执行分支标记为 `user_interrupted`。
6. 有明确 Generation Chain parent、无新 User item 的失败重试恢复原 Interaction，并保留失败 Run。
7. 无 parent 的失败根 Run 只在以下条件全部满足时归并：
   - Principal 相同；
   - canonical request fingerprint 精确相同；
   - 前一个根 Run 已失败；
   - 前一个根 Run 从未发生 Client Output Commit；
   - 新 Run 在前次失败后两分钟内开始；
   - 没有相同 fingerprint 的 Run 正在执行。
8. 根重试 fingerprint 只保存在当前进程的短期索引中，不写入 Observation、日志或 API；进程重启后不再归并。
9. 根重试归并只改变 Observation 分组，不建立或伪造 Generation Chain parent。

### 3.2 Interaction 主状态

主状态只表达当前最需要用户注意的活动，不显示分支计数：

1. 任一 Run 正在执行：`running`，绿色圆点呼吸；
2. 无 Run 执行，但任一叶分支等待 Connect Client 工具结果：`waiting_client`，静态琥珀圆点；
3. 无活动分支且至少有最终生成响应：`completed`；
4. 其余终态：`interrupted`，详情保留 `failed`、`cancelled`、`delivery_failed`、`user_interrupted` 等原因。

等待客户端不使用猜测超时。它持续到合法续接到达、记录按保留期删除或用户清理历史；旧父节点发生合法晚到续接时，Interaction 可以重新进入活动状态。

HTTP 流式响应以 Delivery 确认的协议终态为完成边界，而不是客户端是否继续读取到 body EOF。Observation 在流处理任务完成 Generation Chain 提交尝试后记录最终状态与已提交的节点关联；协议终态之后关闭读取不能覆盖成功结果，终态之前断线仍按中断记录。公开工具交付后的 `waiting_client` 使用流处理任务最终确定的状态。

`prefers-reduced-motion: reduce` 下，`running` 使用静态绿色圆点，不播放呼吸动画。

### 3.3 用量口径

Interaction 卡片、详情与用量分析共享 `Confirmed Upstream Usage`：

- 汇总所有 Inference Run、隐藏 Model Turn、重试和 Target failover 中上游明确报告的 usage；
- input、output、cache read、cache write、reasoning 等字段分别累计，不把 cache 重复加进 total；
- 每个实际上游 attempt 的 usage 最多记一次；
- Target attempt 成功与明确报告的 usage 不因随后还原或映射发布失败而改写；Model Turn 的唯一终态由内部完成 gate 记录，只有发布完成且未被取消或超时抢占才记成功；
- 上游尚未报告或永不报告时保持 `unknown`，不显示为零，不用本地 tokenizer 估算；
- 收到新的上游 usage 后更新持久化投影并推送 SSE。

### 3.4 Observation 不影响执行

Observation 写入、SSE、Debug 分段文件、容量统计或导出失败不得改变 Inference Run 的响应、重试、Target 选择、Client Output Commit 或 Generation Chain 提交。无法记录时产生显式 `observation_gap` 或把 Debug Trace 标成 `partial`；不得阻塞、取消或伪造业务结果。

## 4. 模块与 seam

`stravia-core` 新增 crate-private 深模块 `interaction_observation/`。外部 seam 保持小：

```text
observe_ingress(IngressStart) -> IngressObserver
IngressObserver.reject(RejectedOutcome)
IngressObserver.admit(RunStart) -> RunObserver
RunObserver.record(RunEvent)
RunObserver.finish(RunOutcome)

query_forest(ForestQuery) -> ForestPage
get_interaction(InteractionId) -> InteractionDetail
query_rejections(RejectionQuery) -> RejectionPage
subscribe(after: EventSequence) -> ObservationStream
set_debug_enabled(bool) -> DebugState
issue_debug_bundle_ticket(BundleRequest) -> DownloadTicket
stream_debug_bundle(DownloadTicket) -> BundleStream
clear_history() -> ClearHistoryResult
```

接口名称是设计约束而非逐字 Rust 签名。实现必须保留以下所有权：

- `IngressObserver` 在认证/解码前捕获可形成 Rejected Request Observation 的最小元数据；
- `RunObserver` 持有该 Run 的 Debug 快照、Interaction 关联和终态 guard；
- typed `RunEvent` 表达 Model Turn、Target attempt、Platform Tool、canonical checkpoint、Client Projection、Delivery 与 usage；
- AdminService 只调用查询、开关、清理、票据和流式导出接口；
- Axum/Tauri adapter 不解释 Interaction 分组、Trace 完整度、ZIP 内容或保留策略；
- Generation Chain 只提供已确认的 node/root/parent 关联，不接收运行中或失败 Observation 状态。

删除旧 `logging::LogEntry`、`run_collector`、`LogStore`、`proxy::observability::send_log` 和旧 `wire_capture` 的平行写入路径。Header/URL 脱敏工具迁入 Observation 模块的单一 redaction policy；Provider/Delivery adapter 只提交原始应用协议事件。

## 5. 事件与投影

### 5.1 普通事件

普通 Observation 保存稳定、结构化、无 wire payload 的事件：

- `run_admitted`
- `run_started`
- `model_turn_started`
- `target_attempt_started`
- `target_attempt_finished`
- `platform_tool_started`
- `platform_tool_finished`
- `client_tool_handoff`
- `client_visible_content_delta`
- `usage_confirmed`
- `client_output_committed`
- `delivery_finished`
- `run_finished`
- `interaction_relinked`
- `observation_gap`

普通 Platform Tool 事件可保存 tool ID、状态、开始/结束时间和耗时，不保存参数或结果。普通 Model Turn / Target attempt 事件可保存 Route、Target、Provider、协议、状态、耗时和 Confirmed Upstream Usage，不保存 canonical request/response。

`client_visible_content_delta` 只保存 Client Projection 已交付的可见内容。文本以最长 1 秒窗口合并，避免逐 token 数据库写和 Svelte 更新；生命周期与终态事件不等待文本窗口。

### 5.2 Debug canonical checkpoint

Debug Run 额外写入以下稳定语义阶段：

1. `decoded_request`
2. `restored_request`
3. `effective_model_request`
4. 每个 Model Turn 的 `canonical_request`
5. 每个 Model Turn 的 `canonical_delta`
6. 每个 Model Turn 的 `canonical_terminal_response`
7. `platform_tool_call`
8. `platform_tool_result`
9. `response_after_hook`
10. `client_projection_event`
11. `delivery_terminal`

每项带 Interaction ID、Run ID、Model Turn ID、Target attempt ID（适用时）、单调事件序号与 UTC 时间。不得序列化锁、缓存、credential object、连接对象或其他临时 Rust 内部状态。

### 5.3 顺序与 SSE cursor

Observation writer 为持久化事件分配递增 `event_sequence`。事件及受影响摘要在同一数据库事务内提交后才广播；SSE event ID 等于 sequence。

页面先查询快照并取得 `snapshot_sequence`，再从该 sequence 订阅，避免查询与订阅之间丢事件。重连携带最后确认的 sequence：

- cursor 仍在保留范围内：补发缺失事件；
- cursor 已清理：发送 `reset_required`，前端重新查询当前时间页；
- SSE 断开不影响 Observation 写入。

当前只保证连接到同一 Gateway 实例的实时唤醒；SQLite/PostgreSQL 中已持久化的历史仍由查询接口读取。多实例通知与共享 Trace storage 不在本设计范围内。

## 6. 持久化

### 6.1 关系数据

SQLite 与 PostgreSQL 使用等价 schema 和索引。具体 SQL 由各自迁移拥有，逻辑表如下：

#### `interaction_observations`

- `id`
- `principal`
- `generation_root_id`（nullable）
- `root_run_id`
- `first_model_id`
- `first_model_display_name`
- `status`
- `started_at`
- `last_active_at`
- `visible_tail`
- Confirmed Upstream Usage 各字段及 known 标记
- `last_event_sequence`
- `expires_at`

标题固定使用 `started_at + first_model_display_name`；显示名为空时回退首个 Run 的 Route ID。后续模型变化不改标题，在详情中展示。

#### `inference_run_observations`

- `id`（沿用 request ID）
- `interaction_id`
- `parent_run_id`（nullable，自关联）
- `generation_node_id`（nullable）
- `generation_parent_id`（nullable）
- `ingress_protocol`
- `route_id` / `model_display_name`
- `status` / `terminal_reason`
- `debug_enabled`
- `client_output_committed`
- `started_at` / `last_active_at` / `finished_at`
- `last_event_sequence`

#### `model_turn_observations`

保存 Run 内每个 Model Turn 的身份、Route、开始/结束、状态和 usage。用量分析与 Route Scheduling Strategy 原来依赖 `request_logs` 的 24h/1h provider/model/latency/usage 聚合全部迁到此表；不得继续双写旧表。

#### `target_attempt_observations`

保存每个真实上游 attempt 的 Target、Provider、上游模型、协议、状态、耗时、错误分类和已报告 usage。失败、同 Target retry 与 failover 各自保留。

#### `observation_events`

保存 sequence、关联 ID、kind、occurred_at 和普通安全 payload。按 Interaction/Run/sequence、全局 sequence、过期时间建立索引。

#### `rejected_request_observations`

保存无法形成 Inference Run 的请求时间、method、脱敏 path、ingress 协议、失败阶段、稳定错误 code、HTTP status、Debug Trace manifest 关联和过期时间。它不保存 Principal，也不伪造 Interaction ID。

#### `debug_trace_manifests`

保存 Run 或 Rejected Request、相对目录、已写字节、事件数、`complete | partial`、部分原因、创建/完成/过期时间。绝不保存绝对路径或可下载凭据。

### 6.2 Debug 分段文件

payload 写入 `GatewayConfig.data_dir` 下由 Observation 模块拥有的目录，建议布局：

```text
observation-debug/
└── <trace-id>/
    ├── segment-000001.jsonl
    ├── segment-000002.jsonl
    └── ...
```

每条记录包含 schema version、sequence、recorded_at、layer、direction/stage、transport、protocol、representation、status、headers、payload encoding 与 payload。UTF-8 内容使用 text；非 UTF-8 使用 base64。WebSocket 保留 text/binary/ping/pong/close message 类型；HTTP/SSE 保留 adapter 看到的应用层 chunk 边界，不声称保留网络 packet 边界。

文件路径只接受模块生成的 opaque trace ID 与固定文件名，所有导出读取都在 canonicalized root 内，防止 path traversal。

### 6.3 容量与失败

- 每个 Inference Run 最多 64 MiB；
- 所有未过期 Trace 的总占用最多 2 GiB；
- 容量按实际落盘字节计；
- 达到上限后停止对应 Trace 写入，Inference Run 继续；
- manifest 标记 `partial`，原因使用 `run_size_limit`、`global_size_limit`、`writer_overflow` 或 `storage_error`；
- UI 在 Debug 开启但无法完整写入时显示常驻告警；
- 不为释放额度提前删除尚在保留期内的旧 Trace。

### 6.4 保留与清理

Observation、Rejected Request、Debug manifest 与 Trace 文件跟随 `log_retention_days`；默认 7 天。

清理顺序：

1. 在数据库把待删除 Trace 标记 tombstone；
2. 幂等删除对应受管目录；
3. 删除 manifest、events、runs、interactions/rejections；
4. 启动时扫描并回收无 manifest 的孤儿目录，以及完成遗留 tombstone。

“清除历史记录”只删除非活动 Interaction 与 Rejected Request；`running` 和 `waiting_client` 保留，并在结果中报告跳过数量。清理不取消 Inference Run。

## 7. Debug 开关与脱敏

### 7.1 开关

- 开关是当前 Gateway 进程的原子运行态；默认关闭，重启后关闭；
- 每次开启都显示确认：body 可能含提示词、工具参数和业务数据；64 MiB/Run；2 GiB 总上限；关闭后已有 Trace 仍按保留期存在；
- 每个 Inference Run 在准入时独立快照；同一 Interaction 可以完整、部分或完全没有 Trace；
- Rejected Request 在 ingress 时快照，并可生成只含 client request/platform error response 的独立 Trace；没有上游方向不算缺失；
- 关闭只影响之后准入的 Run，不删除已有数据。

### 7.2 脱敏

所有 Observation、Trace、错误、manifest、ZIP 与进程 log 共用单一 redaction policy：

- credential header（含 Authorization、API key、Cookie、Set-Cookie、Proxy Authorization）值永久替换为 `***`；
- URL userinfo 与 key/token/signature/credential 类 query 值永久替换为 `***`；
- JSON/form 等结构化 body 中明确的 key/token/secret/password/credential 字段递归替换为 `***`；
- Debug 识别协议凭据字段与单条应用消息内的完整凭据模式，不承诺拼接多条消息后再识别业务文本中的凭据；这类跨消息内容仍需按敏感数据处理；
- 其他 prompt、工具参数、工具结果和业务内容在 Debug Trace 中保留，因此开启确认必须明确敏感风险；
- redaction 在写入前完成；原始凭据不得先落临时文件、数据库或异步队列；
- Trace event 与 Bundle manifest 记录发生过哪些类别的 redaction，但不记录原值。

## 8. Interaction Debug Bundle

### 8.1 时间点快照

运行中也允许导出。签发票据时固定 `through_event_sequence`，Bundle 只承诺覆盖该 sequence 之前已落盘的事件，并记录：

- `exported_at`
- `through_event_sequence`
- `interaction_status`
- 每个 Run 的 debug 快照、Trace 状态、字节数和缺失/部分原因
- Bundle 总体 `complete | partial | none`

Interaction 结束后可以重新导出新的终态 Bundle。不得把运行中快照称为最终完整包。

### 8.2 ZIP 结构

```text
stravia-interaction-<id>.zip
├── manifest.json
├── interaction.json
├── README.txt
└── runs/
    ├── 001-<run-id>/events.jsonl
    ├── 002-<run-id>/events.jsonl
    └── ...
```

`manifest.json` 是机器契约；`README.txt` 只说明 schema version、脱敏边界、应用协议级而非 packet capture、partial 含义。ZIP 使用 stream mode 生成，不在内存中组装整个文件。

Rejected Request 导出使用同一 schema family，但 `kind = rejected_request`，只有一个 trace，不伪造 Interaction。

### 8.3 一次性下载票据

1. 管理员携带 Admin Bearer token 发起 POST；
2. Server 固定导出快照并签发 60 秒有效、单次使用的高熵 opaque ticket；
3. 浏览器以普通导航 GET ticket URL，Server 验证并消费 ticket 后流式返回 ZIP；
4. ticket 不写入 access log、Observation、Trace、Referer 或错误文本；
5. 过期、已消费或进程重启后的 ticket 返回统一不可用错误；
6. ticket 只授权固定 Interaction/Rejected Request、固定 sequence 的一次下载，不授权其他 Admin API。

## 9. Admin HTTP contract

浏览器页面路径继续使用 `/logs`。旧 `/api/v1/logs` 与 `/api/v1/logs/{id}` 删除，不保留 alias。

新资源：

```text
GET    /api/v1/observations/interactions
GET    /api/v1/observations/interactions/{id}
GET    /api/v1/observations/rejections
GET    /api/v1/observations/rejections/{id}
GET    /api/v1/observations/events?after=<sequence>
GET    /api/v1/observations/debug
PUT    /api/v1/observations/debug
DELETE /api/v1/observations/history
POST   /api/v1/observations/interactions/{id}/debug-bundle-tickets
POST   /api/v1/observations/rejections/{id}/debug-bundle-tickets
GET    /api/v1/observations/debug-bundles/{ticket}
```

Interaction forest 查询参数：

- `anchor_at`：页面打开时固定的 Unix 毫秒时间；
- `window_index`：0 为实时最新页，下界固定为 `anchor-24h`，持续接收 anchor 之后的新活动；1 表示 `[anchor-48h, anchor-24h)`，后续历史页依此类推；
- `cursor` / `limit`：同一时间页内按根链游标分批加载；
- `provider`、`model`、`api_key`、`status`：匹配任一 Interaction/Run 后返回完整根 DAG；
- 每个节点带 `matched`，前端对非命中节点降噪而不删除。

响应同时给出 window bounds、根链总数、next cursor 与 snapshot event sequence。根链按其最新 Interaction 的 `last_active_at` 归入且只归入一个时间页；返回时补全该根 DAG 在保留期内的全部 Interaction。

SSE 通过普通 `fetch` 携带 Admin Bearer header，并由 `eventsource-parser` 解析；Admin token 不进入 query string。

## 10. 请求记录页面

### 10.1 信息架构

页面标题和导航继续使用“请求记录”，主体分为：

- `交互链路`：默认页签，Interaction forest 无限画布；
- `拒绝的请求`：独立时间列表与详情，不伪造画布节点。

页面 header 包含实时状态、当前 24h 窗口、筛选、Debug switch 和“清除历史记录”。普通 CSV 导出删除。Debug Bundle 按选中的 Interaction/Rejected Request 提供。

### 10.2 画布

使用 `@xyflow/svelte`：

- 无限 viewport；
- 空白处拖拽平移；
- 滚轮以指针为中心缩放；
- touch pan 与 pinch zoom；
- 节点可选、可聚焦，但不可拖动、删除、重连或创建连接；
- `onlyRenderVisibleElements` 启用；
- 提供适配已加载内容、回到进行中、缩放、可折叠 minimap；
- 所有屏宽都使用同一画布；窄屏点击节点后详情全屏。

使用 `@dagrejs/dagre` 在 worker 中计算 top-to-bottom 子树布局。不同 Generation Chain 根按页面快照的最后活动时间从左到右固定顺序；链内 Interaction 沿因果向下，同父子节点横向分叉。实时数据只重排受影响根子树，已有根的相对顺序不改变。

一个时间页先加载最新一批根链的完整子树；横向接近已加载边缘时按 cursor 加载下一批。未加载完时显示 `loaded / total`。“适配全部”先加载剩余根链并显示进度，再计算完整 bounds。

### 10.3 时间页与迁移

页面打开时固定 `anchor_at`。历史窗口边界与已打开历史页的成员不随 wall clock 漂移；手动刷新才重置 anchor。最新页是实时窗口：下界固定为 `anchor_at-24h`，不以 anchor 限制后续活动，因此长时间打开时可覆盖超过 24 小时。Interaction Chains 与 Rejected Requests 使用相同时间窗语义。

根链若因新活动跨入更新的时间页：

- 当前已打开画布不立即删除或移动它；
- 显示“此链已迁移到较新的时间页”提示；
- 点击提示跳转新时间页并聚焦该链；
- 新查询严格按新的 last activity 归页，不在两页重复。

### 10.4 Interaction 卡片

固定尺寸卡片显示：

- `本地开始时间 · 首个 Run 的 Model Display Name`；为空回退 Route ID；
- 主状态；只有真实执行时显示绿色呼吸圆点；
- Confirmed Upstream Usage；未知字段显示等待上游，不显示 0；
- 固定 3–5 行尾部内容预览；
- 普通 Run 只用客户端可见内容生成预览；Debug Run 可以显示完整 canonical 尾部；
- Debug `complete | partial | none` 标记；
- 选中、键盘 focus、筛选命中与非命中有明确非颜色状态。

卡片不会因完整输出增长高度。Interaction 内多 Run 分支只在详情展开，不在卡片显示计数。

### 10.5 实时跟随

进入第 0 时间页时，默认聚焦最新活动或正在执行的节点。用户一旦平移、缩放或选择旧节点，自动跟随暂停；视口外有活动时显示“有新活动 · 跟随”。点击后恢复并聚焦当前最新的真实执行节点。

### 10.6 右侧检查器

桌面使用可调宽、可关闭的右侧检查器，默认约占 40–55%；画布保留选中节点及其因果路径。窄屏使用全屏详情。

详情按时间排序，同时保留层级和 Run 父子关系：

```text
Inference Run
├── Model Turn
│   ├── Target attempt
│   └── Platform Tool
├── Client Tool handoff
└── Delivery
```

普通详情显示生命周期、Route/Target、协议、状态、耗时、Confirmed Upstream Usage、客户端可见内容。Debug Run 才显示完整 canonical checkpoint、Wire 方向、headers、body/frame、复制与单事件下载能力。

### 10.7 视觉方向

视觉沿用当前 Stravia 冷灰蓝 token、IBM Plex Sans / Condensed / Mono 和轻边框，不建立黑绿终端主题。签名元素是“因果脊线”：

- 浅色参考色板：Canvas `#F3F5F6`、Ink `#171A1D`、Route Blue `#315F83`、Hairline `#D8DEE1`、Running `#2F6B53`、Waiting `#8A621E`；深色模式继续由现有语义 token 映射，不另建平行色板；
- IBM Plex Sans 承担正文与控件，IBM Plex Sans Condensed 承担结构标题，IBM Plex Mono 承担时间、ID、usage 与 wire；
- 默认连线使用低对比冷灰蓝；
- 选中节点后，从根到该 Interaction 的因果路径加深，其余节点与连线降噪；
- 执行状态绿只用于圆点，不把整条画布染成绿色；
- 背景使用稀疏点阵帮助判断平移和缩放；
- 唯一环境动画是运行圆点呼吸；遵守 reduced motion。

自检：黑底荧光绿、渐变统计卡和可拖便签都属于可套用到任意 AI 控制台的默认答案；本页只把视觉风险用在真实因果拓扑和选中路径上，其余继续服从现有管理面。

## 11. 迁移与删除

SQLite 与 PostgreSQL 新迁移执行：

1. 删除全部旧 `request_logs` 数据与表；
2. 创建 Observation 关系表和等价索引；
3. 不从 Turn Chain 回填旧 Interaction；升级后的请求记录从空状态开始；
4. 更新 `docs/database/schema.md`；
5. 由 migrations 重新生成 `deploy/schema/postgres.sql`，不手改 schema body。

代码 clean cutover：

- 删除旧 LogStore、RequestLog DTO、查询/详情/clear endpoints；
- 删除旧 WebUI table、LogDetailDialog 和 CSV；
- 删除 `STRAVIA_WIRE_CAPTURE_DIR`、`--wire-capture-dir` 与 debug-build 自动 capture；
- release 与 debug 构建都编译 Observation/Trace，运行时默认关闭；
- dev wire replay 需要时从版本化 Trace event 读取，不保留第二套旧 JSONL schema；
- 更新 `README.md` 与 `README_CN.md`，删除“开发命令自动开启 `.scratch/wire-captures`”说明；
- 更新 architecture、agent-core、ADR-0034 的实现引用：Route scheduling 和 stats 改读 Observation 的 Model Turn/Target attempt 聚合。

## 12. 依赖

前端新增：

- `@xyflow/svelte` 1.6.6
- `@dagrejs/dagre` 3.1.1
- `eventsource-parser` 4.1.0

Rust workspace 新增：

- `zip` 8.6.0，关闭默认重型 features，只开启流式写出所需的 deflate 能力。

依赖与 lockfile 必须由 Bun/Cargo 正常解析更新，不手改 lockfile。

## 13. 验收场景

### 13.1 分组与拓扑

- 一个 User 输入跨三次 client tool round-trip，只出现一个 Interaction 卡片；详情有三个 Run。
- 同一父响应并发两次工具结果，卡片仍唯一，详情显示 Run 子树与两个最终回答。
- 新 User 输入创建子 Interaction；原未完成分支显示 user interrupted。
- 明确 Generation parent 的失败重试恢复原 Interaction。
- 无 parent 根失败在两分钟、同 Principal、精确 fingerprint、无 Client Output Commit 时归并；超时、不同 Principal、已有 output commit、并发相同请求均不归并。
- 同一父 Interaction 的两个新 User 分支在画布真实分叉，不复制祖先。

### 13.2 时间与加载

- anchor 边界使用左闭右开规则，无重复和遗漏。
- 跨多个 24h 窗口的根 DAG 只按最后活动时间归一页，页面内显示保留期内完整链。
- 打开旧页后链发生新活动，视图固定并显示迁移提示。
- cursor 分批加载不改变已加载根的相对顺序；筛选返回完整根 DAG并正确标记 matched。

### 13.3 实时与状态

- 本机正常负载下，文字、状态和 usage 从 core event 到 UI 可见不超过 1 秒。
- 真实执行时绿点呼吸；等待客户端工具结果时静态琥珀；reduced motion 下不呼吸。
- 用户操作画布后停止自动跟随，新活动不抢镜头；点击跟随恢复。
- SSE 查询/订阅无缝衔接，断线按 cursor 补齐；过期 cursor 触发完整重载。

### 13.4 Debug

- 每个 Run 独立快照开关；同一 Interaction 的 ON/OFF/ON 形成明确 partial Bundle。
- HTTP、SSE 与 upstream Responses WebSocket 覆盖四方向适用消息、顺序、时间和 attempt ID。
- Debug Run 包含全部约定 canonical checkpoint；普通 Run 不持久化或返回隐藏 payload。
- credential header、URL 与结构化 body 凭据在落盘前脱敏；ZIP、API 和错误不出现原值。
- 单 Run 64 MiB 或全局 2 GiB 超限时请求继续、Trace partial、UI 告警。
- 运行中票据固定 sequence；ZIP manifest 与 events 一致。
- 下载 ticket 60 秒、单次、固定资源；过期/重放/跨资源使用失败。
- 清除历史只删除非活动 Observation 及 Trace，活动请求不中断。
- Observation writer、Trace writer、ZIP 生成失败均不改变推理响应或 Generation Chain。

### 13.5 产品表面

- 1280×800 Desktop、常见桌面浏览器和窄屏触控都能平移、缩放、选择节点与打开详情。
- 键盘可聚焦节点、打开详情、关闭检查器和操作画布控制；状态不只靠颜色。
- 英文与中文文案意图一致；页面仍名“请求记录”。
- 升级删除旧日志后，空状态明确说明新请求会在此出现，不暗示迁移失败。
