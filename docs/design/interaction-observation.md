# Connect Client Interaction Observation 设计

## 1. 目标

把现有按单条请求展示的 `request_logs` 与仅限 debug 构建的 `wire_capture` 干净切换为统一的 `Interaction Observation`：

- 请求记录页以 `Connect Client Interaction` 为画布节点，以 `Generation Chain` 因果关系连接节点；
- 一次 Interaction 覆盖一次新 User 输入到最终生成响应之间的一个或多个 Inference Run，并允许 Run 子树分叉；
- 正在执行的状态、客户端可见输出和 Confirmed Upstream Usage 通过 SSE 在 1 秒内更新；
- Debug 按每个 Inference Run 准入时的进程级开关快照生效；
- Debug Trace 覆盖四个方向的应用协议级 wire、稳定 canonical checkpoint、HTTP/SSE/WebSocket；
- Interaction Debug Bundle 以版本化 ZIP 流式导出，明确完整、部分或缺失状态；
- 普通 Observation 保存可读思考及客户端、平台工具输入/返回，不保存完整 canonical 或 wire payload，也不采集模型思考的签名和密文；
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

### 3.5 凭据新增发现

凭据保护页从普通 `credential_mappings_created` 事件投影最近发现，不另建秘密目录或会话事实源。事件在映射实际新建提交后、可失败的替换前由 `RunObserver` 发出，载荷为 `discoveries: [{ rule_ids, source_types }]`，每个元素对应一个实际新建映射；不含秘密、指纹、可恢复引用、消息片段或完整 JSON 路径。来源类型为 `user_message`、`system_or_history`、`tool_arguments`、`tool_result`、`other_text`，来自本次提取和最终规则命中，不从历史正文补推。

映射预留与发现投递由同一独立任务持有，防止数据库已经提交、调用方尚未收到确认时取消导致遗漏。取消不等待此任务，也不执行后续替换、Provider 调用或发布；已启动的预留可以完成，并沿用未发布映射的既有保留期。

同一 Interaction 的多次发现合并，按最后一次新增事件时间倒序；后续复用、还原和状态更新不改变发现时间。请求结果单独读取现有 Interaction 状态，`interrupted` 的详情仍保留失败或取消的 Run。该投影不要求 Debug，也不等待客户端交付或 Generation Chain 成功；清理观察不删除保护映射，后续有效复用不会重新计数。

`GET /api/v1/reversible-redaction/discoveries` 经 `AdminService` 查询现有写者已处理的事件，采用有界 `limit` 与 `next_cursor` 翻页。返回 `items`、`next_cursor` 和 `observation_gap`；条目只含交互 ID、API Key 名称、发现时间、新增数量、规则与来源类型、请求状态及缺失标志。查询失败与空记录分别表达。迁移 `0036_credential_discovery_coverage` 把升级前保留的交互标为观察缺失，不扫描秘密库或历史补造发现。已归属交互的写入失败尽可能持久化 gap；尚不能落盘的准入或队列损失按 Run 保存易失的发生时间和代次，并用当前观察保留期判断是否仍可见。清理历史只移除本次实际删除的已知 Run 所属且代次未变的标记；活动记录、清理期间新发生的损失和无法确认归属的准入损失继续保留，直至当前保留期到期。该易失标记不承诺跨进程崩溃保留，Observation 仍是可丢失诊断投影。

## 4. 模块与 seam

### 原生压缩与保留尾部关联

`compaction_operation` 保存 standalone/inline、所属 Model Turn、来源、登记 ID、阶段、耗时与错误分类；所选 Target/Provider 沿用 Target attempt，usage 仅沿用每 attempt 一次的 `usage_confirmed`。Standalone 是真实操作，不落空 Generation；回放旧 state 不再登记压缩操作。

`native_compaction_associated` 表示原生状态跨越已登记边界，`retained_tail_associated` 仅表示客户端幸存上下文的诊断推断。两者与既有确定 Generation 关系在 `context_events`、forest/detail、SSE、详情及画布中分开；推断不改变父边、Target Continuation、有效输入或新 User 的 Interaction 分组。来源卡片已清理时不从核心存储复活。

尾部索引只接收实际收到的规范化 client-shaped 输入和已交付公开输出。流式与非流式输出均复用 Generation Chain 拥有的 ingress 历史整形规则，使诊断索引与客户端回放采用相同分块；整形失败记录 observation gap，不能把交付前的 canonical 分块作为后备索引。指纹仅筛选候选，完整语义再次核验；匹配旧历史后缀与新请求任意连续区间。只有顶层 leading system/developer 可排除，内部差异不能删除后拼接。完整工具 ID、参数、结果、角色、媒体与控制保持语义身份。

无签名、无密文且带合法 History Marker 的公开思考投影参与精确匹配，保留全部预览与标记字节，不恢复隐藏内容，也不把投影本身算作公开回答。它不截断相邻 User 与公开回答的完整交互，因此客户端切换模型并更新顶层提示后，仍可形成诊断关联。无 Marker 的原始思考、签名、密文和 native state 继续使用不可匹配边界；预览或 Marker 的改动不能跳过后拼接。完整交互、唯一来源、Principal 隔离及资源预算要求不变，Generation Chain 的严格父链规则不变。

隔离样本校准采用至少两个完整语义单元、256 canonical UTF-8 bytes、64 assistant UTF-8 bytes，并额外要求完整 User/Assistant 交互或闭合工具关系。短应答样本为 193/3 bytes，泛化短交互 220/30 bytes；具体英文任务 587/240 bytes、具体中文任务 567/213 bytes 达到长度门槛。长度达标不能替代唯一性、角色和工具闭合。

校准原文如下；只校准长度门槛，不将文本内容当成运行时特例。

|样本|User|Assistant|
|---|---|---|
|短应答|Please continue.|OK.|
|泛化短交互|Can you help me?|Yes, I can help you with that.|
|具体英文任务|Inspect the importer failure: invoice INV-2048 has a duplicated ledger entry after the retry. Preserve its original external reference and identify the transaction boundary.|The importer committed the ledger row before recording the external reference. Move both writes into the same database transaction, and keep the unique external reference constraint so a repeated invoice cannot create a second ledger entry.|
|具体中文任务|请检查订单导入失败的原因：订单编号为订单二零四八，重试后出现了重复的账目记录。请保留原始外部引用，并找出事务边界的问题。|导入器在记录外部引用之前提交了账目行。应该把这两个写入放进同一个数据库事务，并保留外部引用的唯一约束，以防止重复提交的订单创建第二条账目记录。|

重现时分别构造纯文本 User、Assistant `AiItem`，累加 `canonical::item_value` 返回的各个语义单元经 `serde_json::to_vec` 编码后的字节数。纯文本 User 单元包含 `role`、单元素 `content` 数组及值为 null 的 `tool_calls`、`tool_call_id`、`artifact_references`；Assistant 单元包含 `role` 和单个文本 `content` 对象。第二个数值是 Assistant 原文的 UTF-8 长度，中文不转义为 ASCII。资源预算用于限制候选集合、序列化和核验成本，不是从这些小样本推断出的性能保证；截断搜索必须保持未关联。

每窗口最多 512 单元/512 KiB，进程索引最多 128 候选/16 MiB，单次最多 65,536 单元检查和 8 MiB 内容核验。超过资源预算返回 `resource_limit`；保留候选未完整索引（包括冷启动）返回 `index_unavailable`；多个来源成立返回 `ambiguous`。这些情况不影响正常推理。敏感比对内容只在易失索引中保存，隐藏 reasoning/native state 使用不匹配边界；持久 Observation 只保存来源与匹配元数据。核心原生登记的重启保证与诊断索引可用性不是同一承诺。

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
- `model_thinking_delta`
- `model_thinking_finished`
- `platform_tool_started`
- `platform_tool_finished`
- `client_tool_handoff`
- `client_tool_result`
- `client_visible_content_delta`
- `usage_confirmed`
- `client_output_committed`
- `delivery_finished`
- `run_finished`
- `interaction_relinked`
- `observation_gap`
- `input_preview_recorded`：仅通知用户输入预览已更新，不在事件 payload 中重复保存输入正文。

`platform_tool_started.input` 和 `client_tool_handoff.input` 保存工具输入，`platform_tool_finished.content` 保存平台工具返回；输入为可解析的 JSON 时保留其类型，否则保留原始参数字符串。旧事件缺少这些可选字段时表示未采集，字段值为 `null` 则表示实际采集到 JSON null。`client_tool_result` 保存收到的客户端返回及其调用 ID、错误标记，兼容显式 `tool_result` 块和 `role=tool` 消息；只采集收到的 canonical 窗口，不从恢复后的模型历史重新提取。客户端返回先留在内存，凭据映射注册完成后与输入预览共用发布边界，没有新用户文本的工具续跑也会发布。

`model_thinking_delta` 只提取上游可读 thinking / reasoning summary 文本，以 Model Turn 和 Target attempt 隔离增量脱敏状态，避免跨分片泄露已知凭据或跨尝试拼接。正文、工具、结束、错误及 EOF 结束当前思考段；尝试结束、Run 结束或取消析构也会收尾。签名、密文、obfuscation 和不透明快照不作为普通思考正文。

`client_visible_content_delta` 仍只保存 Client Projection 已交付的可见内容。writer 按既有 500ms 周期合并同一 Run 中相邻且同作用域的正文或思考文本；作用域变化及其他事件边界先刷新，生命周期与终态不等待文本窗口。思考和工具内容不进入 `visible_tail`，沿用既有凭据脱敏及 `log_retention_days`，不受 Debug 开关控制；业务敏感内容仍可能保留，普通记录存储用量会增加。普通事件不保存完整 canonical request/response。

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

SQLite 的 Run admission 使用 `BEGIN IMMEDIATE`，在读取父 Run 状态前取得写锁，使父分支中断与子 Interaction 入库保持原子性，避免并发写入导致读事务升级失败。

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

每条记录包含 schema version、sequence、recorded_at、layer、direction/stage、transport、protocol、representation、status、headers、payload encoding 与 payload。普通可识别 UTF-8 内容保留原契约；只在已知协议媒体位置外置内容，标明 `artifact_externalized`，保留实际请求的 model、system、工具结果和 Provider 字段，不用入口快照覆盖后来请求。具有可证明内容身份时保存 Artifact Reference 与必要元数据；不能关联的媒体、尚未完成鉴权收存的正文或无法可靠识别的二进制正文记录明确的 omission／unrecoverable 状态，不以 base64 再存一份正文。普通文本中的媒体形状 JSON 和业务工具参数不因此当成媒体。HTTP/SSE 需要跨块识别凭据和媒体时重组完整应用消息并标记表示变化，不承诺原始 chunk 或网络 packet 边界。

`artifact_normalized_request` checkpoint 提供已收存输入的稳定引用；Provider 发送时生成的内联正文和签名地址不替代该内容身份。导出对引用执行只读可用性查询，缺失或逻辑过期内容标为不可恢复，不续期或打开文件。旧 Trace 与历史不回填或改写；旧记录参与新请求时，新记录仍采用外置契约。

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
- `stravia_upload_` 保留语法的上传授权在过期、重启或关闭上传注入／一般可逆脱敏后仍替换为 `<stravia-upload-key>`；签名下载路径中的 token 同样不进入日志。只有客户端实际交付可包含真实上传凭据，思考与 Platform Tool 参数不能签发；
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

- `start_at` / `end_at`：Unix 毫秒时间，必须同时提供，且 `0 < end_at - start_at <= 86400000`；按 `[start_at, end_at)` 查询，包含起点、不含终点，显式边界优先于旧参数；Interaction forest 与 Rejected Requests 使用相同约束；
- `anchor_at` / `window_index`：仅为现有 API 调用者保留的旧窗口参数；未提供显式边界时，0 为下界固定在 `anchor-24h`、无上界的实时页，后续历史页按 24 小时分段。WebUI 始终发送显式边界，包括实时预设；
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

页面 header 包含实时状态、时间预设、精确日期时间范围、全屏切换、筛选、Debug switch 和“清除历史记录”。全屏保留当前筛选、选中节点及检查器，支持工具栏退出和 Esc 退出。普通 CSV 导出删除。Debug Bundle 按选中的 Interaction/Rejected Request 提供。

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

使用 `@dagrejs/dagre` 在 worker 中计算 top-to-bottom 子树布局。布局使用画布实际显示的全部连接；被诊断关联连接的 Generation Chain 根归入同一视觉分组，后续 Interaction 位于来源下方，同父子节点横向分叉。不相连的分组沿用页面快照的根顺序，实时数据只重排受影响分组；关联变化也必须触发布局更新。视觉分组不改写后端根身份、根计数或执行父链，跟随视口以完整视觉分组计算范围。

保留尾部推断关联与已确认直连使用相同的底部 source、顶部 target、路径、颜色与线宽，只以虚线区别；单一续接上下对齐，不为跨根关联绕到卡片侧面。连线上与卡片预览中均不附加推断关联说明，具体关联类型仍可在诊断详情中查看。

一个时间页先加载最新一批根链的完整子树；横向接近已加载边缘时按 cursor 加载下一批。未加载完时显示 `loaded / total`。“适配全部”先加载剩余根链并显示进度，再计算完整 bounds。

### 10.3 时间页与迁移

实时预设包括 5、10、30 分钟以及 1、4、12、24 小时；前端随当前时间推进起止边界并发送显式 `[start_at, end_at)`，窗口宽度始终保持所选时长，不会因长时间打开而扩大。自定义范围通过本地日期时间输入转换为 Unix 毫秒，应用后保持固定边界，起点必须早于终点且跨度不得超过 24 小时；恰好 24 小时有效，超限不能应用。Interaction Chains 与 Rejected Requests 使用相同时间窗语义。时间边界只决定根链成员资格，不截断返回的因果上下文，也不限制详情中的完整 DAG。

根链若因新活动跨入更新的时间页：

- 当前已打开画布不立即删除或移动它；
- 显示“此链已迁移到较新的时间页”提示；
- 点击提示跳转新时间页并聚焦该链；
- 新查询严格按新的 last activity 归页，不在两页重复。

### 10.4 Interaction 卡片

固定尺寸卡片显示：

- `本地开始时间 · 首个 Run 的 Model Display Name`；为空回退 Route ID；模型名字号比原卡片标题缩小一级，为正文预览留出空间；
- 主状态；只有真实执行时显示绿色呼吸圆点；
- 模型名下显示用户输入预览框，约两行，展示本次交互用户消息开头；`input_preview` 为 nullable 文本，旧记录或没有文本输入时明确显示未记录，不从历史消息或工具结果伪造；
- Confirmed Upstream Usage；未知字段显示等待上游，不显示 0；
- 用量栏下显示同宽的输出预览框，约三行，底部对齐并裁去上方溢出，内容更新及字体或尺寸变化后仍显示最新一行；
- 两个正文框以安全过滤后的 Markdown 渲染，不加载图片或嵌入资源；悬停或键盘聚焦时通过 Tooltip 查看更多，触控点按打开可关闭的内容浮层；长内容限高滚动，不误触卡片详情；
- 输入预览在凭据脱敏后保留前 4,096 Unicode 字符，普通 Debug 关闭时也记录；同一交互的工具续跑不得覆盖初始输入，新用户子交互记录自己的输入。输入沿用请求记录的保留与清理周期；
- 输出框及其浮层只使用客户端可见内容的已保留尾部，不因 Debug 开关扩大为 canonical payload，也不宣称完整回答；
- 不显示底部 Debug 捕获、筛选命中或因果上下文标签；
- 保留选中路径、键盘 focus 与非命中节点弱化样式。

卡片不会因完整输出增长高度。Interaction 内多 Run 分支只在详情展开，不在卡片显示计数。

### 10.5 实时跟随

选择实时预设时，默认聚焦最新活动或正在执行的节点。用户一旦平移、缩放或选择旧节点，自动跟随暂停；视口外有活动时显示“有新活动 · 跟随”。点击后恢复并聚焦当前最新的真实执行节点。

### 10.6 右侧检查器

桌面使用可调宽、可关闭的右侧检查器，默认约占 40–55%；画布保留选中节点及其因果路径。窄屏使用全屏详情。

默认「对话」页以只读消息气泡展示当前 Interaction：用户靠右使用 primary 色，模型靠左使用中性底色。连续同一模型的 Run 共用一组头像与名称，正文和工具继续追加在同一块内，只在末尾显示最后一条消息的时间；换模型或出现用户消息时重新分组。时间旁不显示任何执行状态或预览说明，执行状态仍在画布与诊断中保留。用户消息取已脱敏的 `input_preview`，每个 Run 的回复只拼接按 sequence 排序的 `client_visible_content_delta.text`，不把 Debug 内容当作回复。没有公开文本事件的旧记录只回退一次到 `visible_tail`。没有用户正文时不生成用户消息，没有助手正文时隐藏气泡，但保留流式组件实例，保证首个实时增量仍可逐字显示。

思考和工具使用官方 shadcn-svelte Marker，默认折叠；有真实详情才提供展开操作，没有可读思考则不显示条目，只有工具名称时显示静态行，不增加「未记录」说明。思考置于所属 Run 正文前，工具置于正文后；展开显示可读思考、工具输入和返回。普通事件优先且无需开启 Debug；同一思考作用域或工具内容已由普通事件提供时，不再重复使用 Debug。旧记录只从 `debug_enabled` Run 中匹配 `run_id`、`layer=canonical`、`payload_encoding=json` 的既有 TraceRecord 补充缺失详情，不补录未采集的历史。Debug 思考读取 `canonical_delta` 与终态快照中的可读字段，完整快照替换而不重复拼接增量；`response_after_hook` 不带 attempt_id 时沿用同一 Model Turn 最近的尝试。签名、密文和 `redacted_thinking.data` 不进入思考正文，Wire 不被猜测解析。

工具调用按 Run 和调用 ID 关联：普通平台事件的 `tool_id` 就是调用 ID，Debug 平台结果使用 `call_id` 而不是工具类型 `tool_id`。客户端返回来自 `client_tool_result`；旧 Debug 才从 `decoded_request` 的 `tool_result` 块或显式 `role=tool`、`tool_call_id` 文本消息补充。返回仅匹配明确 `parent_run_id` 祖先，祖先路径上的历史重放不重复展示，兄弟分支各自收到的返回独立保留。工具输入与返回以安全纯文本或 JSON 呈现，不递归猜测业务 JSON、不执行 HTML 或加载远程媒体。每条 Marker 以稳定活动 ID 独立保存 localStorage 展开布尔值，折叠时删除该项；不保存正文，存储失败明确提示但不阻断展开。展开已有内容不制造「新活动」提示；后续真实内容变化仍可提示，且不收起已展开条目或抢走阅读位置。

正文复用卡片的安全 Markdown 渲染，lexer 与 parser 均显式启用 GFM，表格继续经过既有 HTML 安全白名单。历史首次打开立即显示；运行中新增后缀按 Unicode grapheme 逐字呈现，批量新增及时追平，结束、文本替换或减少动态效果开启时直接显示当前已收到的文本。此动画不改变后端最长一秒的合并与 SSE 更新契约。处于底部时随逐字增长跟随；用户向上翻阅或展开活动后保持阅读位置，只有点击「回到最新」或主动滚到底部才恢复。切换 Interaction 重置跟随，不滚动外层页面。

「诊断」页默认呈现可读的事件摘要、时间与已记录的关键事实和结果。保留 Run 分组与 Run 父子关系，但每个 Run 内的事件统一按 `occurred_at` 升序、同一时刻按 `sequence` 升序排列，不再将 Model Turn、Target attempt 或工具的子树整体提前展开，以免把较晚的完成事件放到较早的客户端输出之前。拒绝请求的事件采用相同排序规则。每个事件的「原始事件数据」默认折叠，展开后保留原始 kind 与完整 payload，因果关联字段不丢失；Run 和 Interaction ID 收在默认折叠的「技术标识」中。未知事件仍保留原始数据入口，不推断成功或其他未记录的结果。Run 标题优先使用模型显示名、缺失时使用 Route ID，状态、耗时与用量仍可见。

排序后相邻的 `client_visible_content_delta` 合并为默认折叠的计数分组；相邻且 `name` 相同、非空的 `client_tool_handoff` 同样合并，例如「Bash × 4 · 已交给客户端」。分组显示首次和末次事件时间，不跨越其他事件、工具名称或 Run。展开分组保留每条事件的时间与完整原文入口，实时追加保持已有分组的展开状态。事件行将原始数据入口收至标题右侧箭头，不再重复占用一行按钮；关键结果和错误仍直接可见，不因精简而隐藏。

`target_attempt_finished` 的耗时后显示 Token 速度。输出用量来自同一 Run、相同 `attempt_id` 的最后一条 `usage_confirmed`（按 sequence 判断），不累加累计快照，也不借用整个 Run 或其他 attempt 的用量。速度复用 `computeTps` / `formatTps`：有有效首 Token 时间时使用既有净生成耗时与非增量流判定，否则使用上游耗时；缺少用量或有效耗时显示未知。卡片输出浮层使用「模型输出预览」名称；画布的已确认执行来源边保留连线、取消重复文字标签。

普通诊断显示生命周期、Route/Target、协议、状态、耗时、Confirmed Upstream Usage、客户端可见事件。Debug Run 才能进入「Debug 记录」查看完整 canonical checkpoint、Wire 方向、headers、body/frame、复制与单事件下载能力。未开启 Debug 不影响普通诊断访问，实时刷新不得把选中的诊断页签切回对话。Rejected Request 默认显示简洁失败摘要，不伪造成模型对话；技术原因仍在诊断中。

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
