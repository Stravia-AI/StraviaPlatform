# Connect Client Interaction Observation 设计

> **目标契约已接受，相关实现尚未迁移。** Canonical Item 的诊断持久化边界以 [ADR-0062](../adr/0062-persist-diagnostic-content-at-canonical-item-boundaries.md) 为准；Debug 的 wire-only 捕获边界以 [ADR-0063](../adr/0063-record-four-direction-wire-debug-at-transport-boundaries.md) 为准。下文描述目标行为，不表示当前存储与捕获实现已经完成切换。

## 1. 目标

把现有按单条请求展示的 `request_logs` 与仅限 debug 构建的 `wire_capture` 干净切换为统一的 `Interaction Observation`：

- 请求记录页以 `Connect Client Interaction` 为画布节点，以 `Generation Chain` 因果关系连接节点；
- 一次 Interaction 覆盖一次新 User 输入到最终生成响应之间的一个或多个 Inference Run，并允许 Run 子树分叉；
- 正在执行的状态、客户端可见输出和 Confirmed Upstream Usage 通过 SSE 在 1 秒内更新；
- Debug 按每个 Inference Run 准入时的进程级开关快照生效；
- Debug Trace 只覆盖四个方向的原始应用协议级收发：上游 HTTP 在 reqwest 请求/响应边界，WebSocket 与客户端在各自传输边界；不记录 canonical、Hook 或 Client Projection 中间阶段；
- Interaction Debug Bundle 以版本化 ZIP 流式导出，明确完整、部分或缺失状态；
- 普通 Observation 保存生命周期、usage、工具事件，以及按 Canonical Item 收口的可读思考与客户端可见内容；不保存完整 canonical request/response 或 wire payload，也不采集模型思考的签名和密文；
- Observation 只服务诊断，不成为推理执行、Generation Chain 或模型历史的事实源。

本设计同时适用于 SQLite 和 PostgreSQL 存储，但实时状态与 Debug 开关只承诺单 Gateway 实例。多实例聚合不在本设计范围内。

Observation 使用统一平台身份契约：随机不透明 ID 是由密码学安全随机生成器均匀采样的 28 位 ASCII 小写字母（约 131.6 bit），完整 SHA-256 派生身份则用 55 位 ASCII 小写字母保留全部 256 bit。Artifact 引用为 `sa:<55 位 ID>`，可带 query、禁止 fragment；History Marker 为 `<!--sh:<28 位 ID>-->`；Projection Delimiter 为 `<!--sp:<28 位 ID>:<t|p>:<ordinal>:<s|e>-->`；可逆脱敏引用为 `<!--sr:<28 位 ID>-->`。这些外壳不授予访问权，外部 Provider／客户端 ID 与真实凭据 token 不变。新格式只用于新部署和全新数据库，不兼容读取旧平台 ID，也不回写不可变或外部历史；旧数据库与用户数据应另外保留而非删除，新会话使用全新数据库。

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

已确认的目标领域契约见 [ADR-0053](../adr/0053-keep-one-interaction-across-generation-roots.md)：充分证据确认的同一任务续接可以跨多个 Generation Chain 根，仍归入同一个 Interaction。新准入请求按该决策分组；已有 Observation 不重算。以下编号规则描述当前归并行为。

1. 一条新的 canonical `User` item 通常开启新的 Connect Client Interaction；精确父响应的工具续接与两秒快速续接按以下规则归并。
2. 无法归入已有 Interaction、且不含 User item 的合法根请求也开启新的 Interaction。
3. 客户端公开工具调用结束当前 Inference Run。同一 Principal 下精确续接父响应、没有新增 User item 的请求继续原 Interaction；请求 delta 的当前输入尾段提交父历史中尚未得到结果的工具调用所对应的结果时，即使夹带新增 User item 也继续原 Interaction，不限时间。当前尾段从 delta 最后一个 Assistant item 之后开始；顶层工具返回与 User 内容块中的 ToolResult 使用相同判定。完整历史中的旧工具结果不构成归并证据，历史编辑导致父节点退回更早位置也不例外。
4. 同一父响应的并发续接属于同一 Interaction，并在详情中形成 Run 子树。
5. 同一 Principal 下精确续接父响应的新请求，其 ingress 接收时间距父响应完整交付时间在 `[0, 2000]` 毫秒内时，即使包含新增 User item 或父 Interaction 已完成，也继续原 Interaction；已完成交互重新进入活动状态。时间不取 admission 处理时间或可变的 `last_active_at`。这项规则仅表示快速续接，不识别或信任 harness hook，真人快速追问同样归并。其他新增 User item 开启新 Interaction；仅此时原 Interaction 中尚无最终响应、且尚无续接 Run 的执行分支标记为 `user_interrupted`；已被续接的等待分支记为 `superseded`，不当作中断。
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
10. 工具续接与快速续接同样只改变 Observation 分组，保留独立 Run 与全部输入，不修改 Generation Chain 父边、权限或模型执行。诊断记录保留归并依据；缺少精确父关系或完整交付时间时，不猜测快速归并。

Generation parent 存在但对应父观察不可用时，新准入记录 `generation_parent_observation_unavailable` gap，不伪造父关联。未捕获、已清理、过期或查询失败都可能造成这类缺失，不能仅据此断言历史存储故障。真正的交付后 Generation 提交失败保留 `settlement_generation_commit` 诊断，界面明确“响应已交付，历史未保存”，不把已交付响应改报为请求失败，也不自动重放。

### 3.2 Interaction 主状态

主状态只表达当前最需要用户注意的活动，不显示分支计数：

1. 任一 Run 正在执行：`running`，绿色圆点呼吸；
2. 无 Run 执行，但任一叶分支等待 Connect Client 工具结果：`waiting_client`，静态琥珀圆点；
3. 无活动分支且至少有最终生成响应：`completed`；
4. 既无活动分支，也无最终生成响应，且仍有因客户端连接关闭或等待超时结束等待的叶分支：`disconnected`，显示“已断开”；
5. 其余终态：`interrupted`，详情保留 `failed`、`cancelled`、`delivery_failed`、`user_interrupted` 等原因。

`failed_request` 表示历史中存在失败请求，不覆盖 `completed`、`running` 或 `waiting_client` 主状态；这些状态使用独立的低权重历史失败标记。无活动且以失败结束的交互仍显示失败。`visible_tail` 为空而 `client_output_delivered` 为真时显示“已交付输出，暂无文本预览”，不能否定已经交付的工具调用或思考预览。

响应完成不等于后台工具完成。仅在最后一个 `RunObserver` 释放、确认不会再产生该 Run 的事件后，writer 才把残留运行中的 Model Turn／Target attempt 记为 `interrupted`，释放残留活动计数并追加 `unfinished_observation_activity` gap。后台执行持有的观察句柄继续保护真实活动；收口不改写 Run 的交付状态、Generation 关联或已确认 usage，不将缺失的结束事实推断为成功。

等待客户端的常规解除只凭客户端回传证据，不使用会话级猜测超时。同一 Interaction 内，一个 Run 的全部公开工具交接都已收到其他 Run 中 sequence 严格更晚的同 ID `client_tool_result` 时，该分支不再参与活动等待聚合，即使结果来自 sibling、结构上仍是叶节点。错误工具结果同样证明客户端已回传，但不证明最终生成成功；没有最终生成响应时，不能因此把 Interaction 标为 `completed`。判定只读取工具 ID 与事件顺序，不读取参数、结果正文或 Debug 文件；缺失或空 ID、没有 handoff、先于 handoff 的结果、部分回传以及同 ID 多次 handoff 均保守保留等待，重复 result 幂等，不跨 Interaction 或 Principal 匹配。状态计算使用尚未物理清除的历史证据，不按事件各自的到期时间截断；Bundle 只使用导出快照内的事件，不能借未来结果解除过去的等待。原 RunOutcome、工具历史和 Generation Chain 父边不改写。

Run 级别的等待分支在两条路径上终结，不无限滞留：续接 Run 准入同一 Interaction 时（`parent_run_id` 落在本 Interaction 内、非新交互打断），仍停在 `waiting_client` 的父 Run 在准入同一事务内转为终态 `superseded`、`terminal_reason='superseded'`，并发布 `run_state_changed`（payload 带 `superseded_by` 指向续接 Run）。`superseded` 是终态：不持有最终生成响应、不参与 `completed` 判定、不再被中断/断连/重启清扫改写，按既有规则过期清理。仅 sibling 证据解除、结构上仍是叶节点的等待分支保持 `waiting_client` 投影（不参与活动等待聚合），由断连、进程重启或新输入清扫终结；新输入清扫只把本 Interaction 内尚无续接 Run 的叶分支记为 `user_interrupted`，已有本 Interaction 续接的分支一律记 `superseded`（其他 Interaction 的新分支不算续接），不当作中断。

HTTP 等待允许一个兜底闲置边界：叶等待 Run 的 `last_active_at` 超过 24 小时仍无回传时，随保留清理按 `client_wait_expired` 转为 `disconnected` 并发布 `run_state_changed`。窗口必须覆盖合法的长时客户端工具执行，不能缩短到会话级别；客户端可能在边界后补传结果，逾期转换不删除历史也不阻止该结果照常落库。

WebSocket 连接关闭时，该连接所属、仍在等待、没有后继 Run 且尚未由完整工具回传解除等待的分支转为 `disconnected`，记录 `client_disconnected` 原因并发布 `run_state_changed`；已完整交付的结果与 Generation Chain 保留。结果先到时不追加虚假断线；关闭先到时保留真实断线历史。其他连接的等待、已续接分支与最终生成响应不受影响。HTTP/SSE 响应正常结束不能证明客户端离线，同一进程内仍等待合法续接、24 小时闲置超时或保留期清理。旧父节点发生合法晚到续接时，Interaction 可以重新进入活动状态；无法证明连接归属的旧记录不回填 `client_disconnected`。

启动时，在 writer 与对外服务启动前以同一恢复事务修正上一进程遗留的 `running` / `waiting_client` 投影：先沿用运行活动恢复，再排除已有完整工具回传的等待叶；只剩未解决、没有 child 的旧等待 Run 转为 `interrupted`，记录 `process_restarted`，事件 payload 为 `{"status":"interrupted","reason":"process_restarted"}`。重启只证明原观察进程结束，不证明第三方客户端离线，不取消或重放客户端工具，也不阻止旧 Generation 的合法晚到续接。恢复保留原 `finished_at`、交付完成时间、Generation 关联、committed、usage 和 `expires_at`；仅重算投影时保留 Interaction 原活动时间与 sequence，真正追加恢复事件时才使用该事件时间与 sequence。恢复幂等，事务失败整体回滚，不手工部分补写；不自动删除历史。既有手动清除仍保护正在运行及真正等待的记录，恢复后的 completed/interrupted 历史按既有规则可清除或过期。

HTTP 流式响应以 Delivery 确认的协议终态为完成边界，而不是客户端是否继续读取到 body EOF。Observation 在流处理任务完成 Generation Chain 提交尝试后记录最终状态与已提交的节点关联；协议终态之后关闭读取不能覆盖成功结果，终态之前断线仍按中断记录。公开工具交付后的 `waiting_client` 使用流处理任务最终确定的状态。

WebSocket 同样等待流生产任务的最终结果，再记录成功交付与工具交接；不能使用开始转发时的终态快照。连接关闭与最终交接采用同一连接范围内的同步登记，关闭先发生或后发生均能结束等待，不额外延长 Inference Run 的执行期限。

`prefers-reduced-motion: reduce` 下，`running` 使用静态绿色圆点，不播放呼吸动画。

### 3.3 用量口径

Interaction 卡片、详情与用量分析共享 `Confirmed Upstream Usage`：

- 汇总所有 Inference Run、隐藏 Model Turn、重试和 Target failover 中上游明确报告的 usage；
- 管理面 input 在每个 attempt 上计算 `max(input_tokens - cache_read_tokens, 0)` 后累计；任一操作数未知时，该 attempt 的净输入未知，`missing_input_tokens` 同时计数。缓存写入不在此扣除范围内；
- output 已包含 reasoning，不再累加或单列思考指标；cache read 与 cache write 保留独立展示。概览按输入、输出分别呈现，不以缺少缓存分项的相加结果冒充总 Token；
- 原始 IR、attempt 用量、持久化事件与 wire debug trace 保留上游口径及 reasoning 子项；列表、详情、事件查询、实时 SSE、重放 SSE 与 Bundle 的管理汇总和事件均在读取或发布边界转换，历史数据无需改写，也不得将管理投影再次写入原始用量；
- 每个实际上游 attempt 的 usage 最多记一次；
- Target attempt 成功与明确报告的 usage 不因随后还原或映射发布失败而改写；Model Turn 的唯一终态由内部完成 gate 记录，只有发布完成且未被取消或超时抢占才记成功；
- 上游尚未报告或永不报告时保持 `unknown`，不显示为零，不用本地 tokenizer 估算；
- Interaction、Run 与 Bundle 聚合按字段累计已报告部分；某次 attempt 的未知值不抹掉其他 attempt 的已确认值。全部未报告时该字段保持 `null`，明确报告的零保留为零。失败但已报告的用量同样累计，重复报告不重复计数；用量分析的 overview、series、model、API Key 汇总只统计成功的 Target attempt，按字段累计已报告部分：某次成功 attempt 的字段未知只不计入该值，不抹掉组内其他已确认用量，全部未知时该字段保持 `null`；失败或未完成 attempt 定义上没有已确认用量，不参与统计；
- Provider 汇总的 `avg_output_tps` 按已完成 attempt 的 `Σoutput_tokens / Σ净生成耗时` 计算；净生成耗时取 `duration_ms - first_token_ms`，首 Token 未报告或差值小于 50ms 时回退 `duration_ms`。任一已完成 attempt 未报告输出或耗时、或总生成耗时为零时为 `null`；
- Interaction、Run 与 Bundle 的聚合 `usage.coverage` 包含 `attempt_count` 和五项 `missing_*_tokens`，分别表示尝试总数及对应字段未报告的尝试数。单个 `usage_confirmed` 事件不携带聚合 coverage；正在运行与终态未报告的区别仍由 attempt 状态表达。coverage 不替代 `observation_gap`，无法记录的 attempt 不计入已观察尝试总数；
- 查询从现存 attempt 记录派生已确认累计与覆盖信息，旧版保存的 `null` 汇总不遮蔽仍然存在的用量；无需改写旧事件或自动拆分历史 Interaction。SQLite 与 PostgreSQL 使用相同计量规则，Route Scheduling 与成本计算仍读取原始用量；
- 收到新的上游 usage 后更新持久化投影并推送 SSE。

客户端响应的 Run 用量账本只合并实际执行的隐藏轮次。没有隐藏轮次时，保留终态响应已有的数值与 known 标志，包括明确报告的零；空账本不得把已知用量降级为未知。该规则不把未知值补零，也不改变管理面的净输入和按字段汇总口径。

首内容超时在取消执行 future 前标记原因，未正常结束的 attempt 记录 `first_token_timeout`；`attempt_aborted` 仅作为没有明确结束原因的释放兜底。两者均不伪造 usage，也不改变原有超时配置、重试预算或调度策略，每个 attempt 仍只有一个终态。

### 3.4 Observation 不影响执行

Observation 写入、SSE、Debug 分段文件、容量统计或导出失败不得改变 Inference Run 的响应、重试、Target 选择、Client Output Commit 或 Generation Chain 提交。无法记录时产生显式 `observation_gap` 或把 Debug Trace 标成 `partial`；不得阻塞、取消或伪造业务结果。

### 3.5 凭据新增发现

凭据保护页从普通 `credential_mappings_created` 事件投影最近发现，不另建秘密目录或会话事实源。事件在映射实际新建提交后、可失败的替换前由 `RunObserver` 发出，载荷为 `discoveries: [{ rule_ids, source_types }]`，每个元素对应一个实际新建映射；不含秘密、指纹、可恢复引用、消息片段或完整 JSON 路径。来源类型为 `user_message`、`system_or_history`、`tool_arguments`、`tool_result`、`other_text`，来自本次提取和最终规则命中，不从历史正文补推。

映射预留与发现投递由同一独立任务持有，防止数据库已经提交、调用方尚未收到确认时取消导致遗漏。取消不等待此任务，也不执行后续替换、Provider 调用或发布；已启动的预留可以完成，并沿用未发布映射的既有保留期。

同一 Interaction 的多次发现合并，按最后一次新增事件时间倒序；后续复用、还原和状态更新不改变发现时间。请求结果单独读取现有 Interaction 状态，`interrupted` 的详情仍保留失败或取消的 Run。该投影不要求 Debug，也不等待客户端交付或 Generation Chain 成功；清理观察不删除保护映射，后续有效复用不会重新计数。

`GET /api/v1/reversible-redaction/discoveries` 经 `AdminService` 查询现有写者已处理的事件，采用有界 `limit` 与 `next_cursor` 翻页。返回 `items`、`next_cursor` 和 `observation_gap`；条目只含交互 ID、API Key 名称、发现时间、新增数量、规则与来源类型、请求状态及缺失标志。查询失败与空记录分别表达。迁移 `0036_credential_discovery_coverage` 把升级前保留的交互标为观察缺失，不扫描秘密库或历史补造发现。已归属交互的写入失败尽可能持久化 gap；尚不能落盘的准入或队列损失按 Run 保存易失的发生时间和代次，并用当前观察保留期判断是否仍可见。清理历史只移除本次实际删除的已知 Run 所属且代次未变的标记；活动记录、清理期间新发生的损失和无法确认归属的准入损失继续保留，直至当前保留期到期。该易失标记不承诺跨进程崩溃保留，Observation 仍是可丢失诊断投影。

## 4. 模块与 seam

### 原生压缩与保留尾部关联

目标契约按 [ADR-0053](../adr/0053-keep-one-interaction-across-generation-roots.md) 扩展：唯一、完整的保留尾部精确匹配在五分钟窗口内可以自动归入原 Interaction，即使本次没有当前工具结果。该行为对启用后准入的请求生效。

已确认的当前工具续接优先：本次回传来源 Run 当前待完成工具调用的结果时，即使同时夹带额外的 User 输入，也继续原 Interaction，不受尾部归并五分钟窗口限制。历史回放中的旧工具结果不能作为当前续接证据；不能只在完整输入中找到相同工具 ID 就触发归并。

只有未满足当前工具续接条件时才进入保留尾部路径，先确认来源，再按以下规则决定交互归属：

- 匹配区间之后没有新的 User 输入，且满足时间窗口：归入来源 Interaction。
- 匹配区间之后有新的 User 输入，或已超出归并窗口：创建新的 Interaction，并在诊断树中连接来源 Interaction；两个交互分别汇总状态、用量和 Debug Bundle 范围。

时间窗口按本次请求入口接收时间减去被匹配来源 Run 完整交付给客户端的时间计算，差值须位于 `[0, 300000]` 毫秒，包含两端。任务开始时间、Interaction 的 `last_active_at`、writer 处理时间和数据库写入时间不参与计时；其他分支活动不得延长来源的归并资格。恰好五分钟可归并，多一毫秒则创建新 Interaction 并保留满足条件的诊断来源连接。

归并窗口不限制诊断来源连接。来源记录仍须在保留期内，匹配仍须完整且唯一；不能先按归并窗口过滤较旧候选，再把剩余候选宣称为唯一来源。诊断连接不建立 Generation Chain 执行父边。

`compaction_operation` 保存 standalone/inline、所属 Model Turn、来源、登记 ID、阶段、耗时与错误分类；所选 Target/Provider 沿用 Target attempt，usage 仅沿用每 attempt 一次的 `usage_confirmed`。Standalone 是真实操作，不落空 Generation；回放旧 state 不再登记压缩操作。

`native_compaction_associated` 表示原生状态跨越已登记边界，`retained_tail_associated` 仅表示客户端幸存上下文的诊断推断。两者与既有确定 Generation 关系在 `context_events`、forest/detail、SSE、详情及画布中分开；推断不改变父边、Target Continuation 或有效输入。分组按 ADR-0053 诊断规则，不恢复已删除历史。来源卡片已清理时不从核心存储复活。

尾部索引只接收实际收到的规范化 client-shaped 输入和已交付公开输出。流式与非流式输出均复用 Generation Chain 拥有的 ingress 历史整形规则，使诊断索引与客户端回放采用相同分块；整形失败记录 observation gap，不能把交付前的 canonical 分块作为后备索引。指纹仅筛选候选，完整语义再次核验；匹配旧历史后缀与新请求任意连续区间。只有顶层 leading system/developer 可排除，内部差异不能删除后拼接。完整工具 ID、参数、结果、角色、媒体与控制保持语义身份。有 Generation parent 时仍对本次收到的 client 输入做尾部诊断。多个来源同时匹配时，若其中唯一一个匹配的语义单元与字节均严格更长，采用该更长来源；并列等长匹配仍为 `ambiguous`。

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

每窗口最多 512 单元/512 KiB：超限时保留最新后缀，而不是丢弃整个窗口；单个语义单元超过字节上限仍返回 `resource_limit`。进程索引最多 128 候选/16 MiB，单次最多 65,536 单元检查和 8 MiB 内容核验。候选核验超过资源预算返回 `resource_limit`；保留候选未完整索引（包括冷启动）返回 `index_unavailable`；多个来源成立返回 `ambiguous`。这些情况不影响正常推理。敏感比对内容只在易失索引中保存，隐藏 reasoning/native state 使用不匹配边界；持久 Observation 只保存来源与匹配元数据。核心原生登记的重启保证与诊断索引可用性不是同一承诺。

`stravia-core` 新增 crate-private 深模块 `interaction_observation/`。外部 seam 保持小：

```text
observe_ingress(IngressStart) -> IngressObserver
IngressObserver.reject(RejectedOutcome)
IngressObserver.admit(RunStart, AdmissionFacts) -> RunObserver
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
- typed `RunEvent` 表达普通 Observation 的 Model Turn、Target attempt、Platform Tool、按 Canonical Item 收口的内容、Delivery 与 usage；Debug Trace 不借 `RunEvent` 采集语义中间阶段；
- AdminService 只调用查询、开关、清理、票据和流式导出接口；
- Axum/Tauri adapter 不解释 Interaction 分组、Trace 完整度、ZIP 内容或保留策略；
- Generation Chain 提供已确认的 node/root/parent 关联及按 Principal 隔离的祖先客户端历史读取，不接收运行中或失败 Observation 状态；
- 归并判定集中在内部 Run Attribution 深模块：writer 在 Admit 处理中把 `RunStart` 与 `AdmissionFacts`（收到的 canonical client 请求与 Generation Chain 已确认证据）交给它，canonical fingerprint、入口接收时间与合并规则只在该模块内计算；writer 保留顺序、背压、持久化与发布职责。

删除旧 `logging::LogEntry`、`run_collector`、`LogStore`、`proxy::observability::send_log` 和旧 `wire_capture` 的平行写入路径。普通 Observation 保留既有脱敏；Debug Wire 在 reqwest、WebSocket 与客户端传输边界捕获，并只替换 HTTP `Authorization` header 值。

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
- `input_preview_recorded`：每个带有新增用户输入的 Run 至多记录一次，`payload.text` 保存完成凭据保护后的最多 4,096 字符输入预览。同一 Interaction 的追加输入也记录事件，但只有初始 Run 更新卡片的 `input_preview`；重复发布不覆盖已有正文。旧事件缺少 `text` 时不推断或补录输入。

`platform_tool_started.input` 和 `client_tool_handoff.input` 保存工具输入，`platform_tool_finished.content` 保存平台工具返回；输入为可解析的 JSON 时保留其类型，否则保留原始参数字符串。旧事件缺少这些可选字段时表示未采集，字段值为 `null` 则表示实际采集到 JSON null。`client_tool_result` 保存收到的客户端返回及其调用 ID、错误标记，兼容显式 `tool_result` 块和 `role=tool` 消息；只采集收到的 canonical 窗口，不从恢复后的模型历史重新提取。客户端返回先留在内存，凭据映射注册完成后与输入预览共用发布边界，没有新用户文本的工具续跑也会发布。

客户端工具结果按收到的批次查询当前 Run 及明确 `parent_run_id` 祖先，只使用同一 Principal、仍在保留期内的调用证据。最近一次 `client_tool_handoff` 确定调用边界；相同 ID 的新 handoff 是新调用。只有与该调用最近结果的脱敏后正文、`is_error` 均相同时才跳过重复写入。正文变化、错误状态变化、分支结果与新调用保留；没有 handoff 证据，或最近结果正文缺失、为 null 时，不跨越该不确定边界去重。比较状态只存在于当前批次，不另存正文副本或原始凭据摘要。既有历史事件不回写、不删除。

`model_thinking_delta` 只接收上游可读 thinking / reasoning summary 文本，并以 Model Turn、Target attempt、Canonical Item 与项内 part 隔离增量脱敏和汇聚状态。签名、密文、obfuscation 和不透明快照不作为普通思考正文。`client_visible_content_delta` 仍只接收 Client Projection 已交付的可见内容。

普通诊断内容按 Canonical Item 收口持久化：同一 item 的流式碎片汇聚为一项，保留项内 part 的边界和顺序；具有独立身份的 item 即使类型相同也不得合并。正常结束保存完整 item；可处理的失败或取消保存已实际收到的内容并标为未完成，不补造未收到的尾部。item 首次落盘只发生在收口时，不周期性持久化中间快照；进程突然崩溃可以丢失整个尚未落盘的 item。

物理存储可以为容量、压缩或文件布局分块，但物理块不得成为新的语义 item，也不得改变 item 身份或 part 边界；本设计不预先指定迁移后的 schema。现有队列容量、背压与 gap 行为继续成立，容量边界不得以时间或字节阈值强制把一个 Canonical Item 持久化成多个内容项。

工具结果批次先对查询 ID 去重，再沿既有祖先与 handoff 边界比较。批内比较引用已接收事件的位置，不再次复制大正文；不按跨交互的相同 payload 全局去重，缺失调用证据、正文变化及 null 边界仍保留。

未收口内容继续作为易失快照实时发布并替换显示，不带 SSE ID、不推进持久 cursor；界面明确未保存状态。实时 Observation 与下游转发在收到内容后尽快推进，不等待诊断 item 收口或落盘。持久化失败、预览容量不足与进程崩溃丢失分别表达，不能把易失预览截断误报成已落盘历史丢失。

思考和工具内容不进入 `visible_tail`，沿用普通 Observation 既有凭据脱敏及 `log_retention_days`，不受 Debug 开关控制；业务敏感内容仍可能保留。普通事件不保存完整 canonical request/response。

### 5.2 Debug 原始 Wire 捕获

Debug Trace 只记录四个方向的原始应用协议级收发：Connect Client → Stravia、Stravia → upstream、upstream → Stravia、Stravia → Connect Client。上游 HTTP 在 reqwest 实际请求与响应边界捕获；WebSocket 与客户端方向在各自实际传输边界捕获 handshake 元数据和应用 message。它不记录 TLS、TCP、HTTP/2 frame 或操作系统 packet，也不采集 decoded/restored/effective/canonical request、canonical content/terminal response、Hook 前后、Platform Tool 中间态、Client Projection、delivery terminal 或 stage timing 等语义阶段。

每条 Wire 记录保留必要的关联元数据，包括适用的 Interaction、Run、Model Turn、Target attempt、方向、协议、transport、顺序与 UTC 时间。既有 Target attempt 身份与生命周期、usage、工具事件、失败与取消继续由普通 Observation 持久化并随 Bundle 导出，不把旧 `target_selected` 迁入普通 Observation，也不复制到 Debug Trace。Debug 原始字节不等待 Canonical Item 收口，普通 Observation 的内容收口也不阻塞 wire 捕获或下游转发。

Wire 记录直接写入 Debug Trace 队列，不逐条写入普通 `observation_events`，也不占用普通事件队列。普通生命周期、item 内容、工具结果及 Trace manifest 状态仍持久化并驱动 SSE。Trace 使用下一持久观察边界作为水位，同一 Trace 中排队记录的水位保持非递减；manifest 按既有维护周期或显式生命周期边界持久化。Interaction 导出票据排空目标 Interaction 的 Trace 后固定截止水位；既有 ZIP 截止水位不能包含之后的新捕获。

原始 body chunk、SSE 字节与 WebSocket 应用 message 在传输边界观察到后即可排队，不以完整 JSON、SSE、NDJSON、Connect message 或 Canonical Item 收口作为记录前提。只有 HTTP `Authorization` header 的值在入队前替换，媒体与其他 header、URL、body 和 message 内容原样保留；编码进分段文件与物理批处理不得改变可恢复的字节、方向和顺序。

Trace 继续使用既有有界队列与写入批次；队列、捕获缓冲或存储失败不等待、不反压推理，Trace 标记 `partial` 或 gap。manifest 的字节及事件水位只公布已确认可读取的数据；分段切换、snapshot、finish 与关闭保持显式 flush，不能因后续 finish 成功而把缺失捕获伪装成完整。

### 5.3 顺序与 SSE cursor

Observation writer 为持久化事件分配递增 `event_sequence`。事件及受影响摘要在同一数据库事务内提交后才广播；SSE event ID 等于 sequence。

`live_content`、`live_snapshot` 和 `live_gap` 不带 SSE ID，不推进持久 cursor。订阅先分批重放已提交事件（每批最多 512 条），再发送完整易失快照，包括空快照。重连、reset 或断线时替换或清除旧易失状态，不按文本猜测去重。易失预览不承诺重启恢复。

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
- `request_model`（请求的路由 model_id 快照，可空）
- `failure_json`（最终失败诊断快照，可空；`code` 为 Stravia 稳定失败词表，上游错误体自带的错误码单独保存在 `upstream_code`）
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

保存无法形成 Inference Run 的请求时间、method、脱敏 path、ingress 协议、失败阶段、稳定错误 code、HTTP status、Debug Trace manifest 关联和过期时间。Migration 0045 起额外保存可空的 `started_at`、`duration_ms`、`failure_json` 与来源快照 `request_model`、`api_key_id`、`api_key_name`，供失败请求投影使用；这些列全部可空、不回填历史，也不引入新的 Principal 外键。它不保存 Principal，也不伪造 Interaction ID。

#### `debug_trace_manifests`

保存 Run 或 Rejected Request、相对目录、已写字节、事件数、`complete | partial`、部分原因、创建/完成/过期时间。绝不保存绝对路径或可下载凭据。

### 6.2 Debug 分段文件

WebSocket 捕获 handshake 元数据与应用 message；Ping/Pong 控制帧的既有限制保留，只记录事件类型、方向与时间，不保存控制帧载荷，并明确标记策略性省略。该省略不把 Trace 标为 partial；历史 Trace 不回填或改写。

payload 写入 `GatewayConfig.data_dir` 下由 Observation 模块拥有的目录，建议布局：

```text
diagnostics/observation-debug/
└── <trace-id>/
    ├── segment-000001.jsonl
    ├── segment-000002.jsonl
    └── ...
```

每条逻辑记录保存原始 Wire 字节及必要的 sequence、recorded_at、方向、transport、protocol、status 和关联身份。除 HTTP `Authorization` header 值外，header、URL、body、SSE 字节、WebSocket 应用 message 与媒体均按捕获边界原样保留；不再产生 canonical、Hook、Client Projection 或其他语义内容记录，也不以 Artifact 引用替换媒体。

既有 Target attempt 身份与生命周期（包括同 Target retry 与 failover 的独立 attempt）、错误、usage 与工具活动继续由普通 Observation 表达；旧 `target_selected` 不迁入普通 Observation。Debug Trace 只把 Wire 记录关联到相应 attempt，不复制或推断普通 Observation 没有提供的语义阶段。

物理分段采用 `trace_storage=1` JSONL：sequence 与 recorded_at 独立存储，元数据与 payload 分别内联或引用本段更早记录的字节偏移。只复用完整内容相等的定义，引用不递归、不跨分段；每个活跃 writer 的候选缓存最多 8 MiB，超限只放弃复用，不删除内容。滚动分段时重置候选缓存。此结构不使用压缩算法；旧 JSONL 仍可读取。快照保持已落盘字节前缀，导出统一还原逻辑记录，外部不会收到存储引用。已有 ZIP 导出封装不变。

`stravia-tools migrate-data --optimize-storage` 可在独占的离线目标副本中去重已有分段：逐条比较还原结果与旧记录，校验成功且字节数减少才替换；失败不发布目标副本。旧记录的 schema version、内容、时间、顺序及独有诊断信息保持不变，manifest 的字节数更新为实际落盘值。

协议专用 parser 可以用于普通执行，但 Debug 不为脱敏或媒体外置等待 NDJSON、Connect、JSON 或 SSE 完整消息，也不另存 decoded frame。既有捕获缓冲、单帧和累计容量上限继续生效；只有捕获链路丢失或截断、捕获缓冲超限、分段写入损坏等捕获不完整才使对应方向标记 `partial`。传输边界已完整收到并原样保存的非法 JSON、SSE、NDJSON 或 Connect 内容仍是完整原始捕获，其协议错误由普通 Observation 表达。

传输失败、协议解码失败和规范化错误沿用普通 Observation 的错误分类、阶段、状态与安全原因摘要，不作为 Debug 内容记录。对应方向没有实际收发时不得补造 Wire；Trace manifest 如实表达缺失或 `partial`。这些诊断不改变重试、回退、超时或成功判定，也不为旧 Trace 补录原因。

文件路径只接受模块生成的 opaque trace ID 与固定文件名，所有导出读取都在 canonicalized root 内，防止 path traversal。

### 6.3 容量与失败

- Trace 落盘不设 Run 级或全局容量上限；`retained_bytes` 只统计实际落盘字节；
- 请求体、传输帧或消息的捕获缓冲仍有既定内存上限，超限只截断对应方向的捕获，不等待完整应用消息后才开始记录；
- 队列溢出不等待 writer，未能入队的捕获内容使 Trace 标记 `partial`；存储错误停止对应 Trace 写入，Inference Run 继续；
- manifest 使用既有捕获容量超限、捕获丢失或截断、分段写入损坏、writer overflow、storage error 与 debug data cleared 原因表达 `partial`；已完整捕获但协议内容非法不属于 capture partial，策略允许的 Ping/Pong payload 省略也不算失败；
- 不完整原因只出现在 Interaction 详情和诊断包中，请求记录页不显示常驻告警。

### 6.4 保留与清理

Observation、Rejected Request、Debug manifest 与 Trace 文件跟随 `log_retention_days`；默认 7 天。

清理顺序：

1. 在数据库把待删除 Trace 标记 tombstone；
2. 幂等删除对应受管目录；
3. 删除 manifest、events、runs、interactions/rejections；
4. 启动时扫描并回收无 manifest 的孤儿目录，以及完成遗留 tombstone。

“清除历史记录”只删除非活动 Interaction 与 Rejected Request；`running` 和 `waiting_client` 保留，并在结果中报告跳过数量。清理不取消 Inference Run。

“清除 Debug 数据”只删除 Debug 内容：全部 manifest 标记 tombstone、关闭活动 writer 句柄后删除受管目录、删除 manifest 行。活动 Run 的 Trace 停止并标记 `debug_data_cleared` partial；请求记录、Rejected Request 与 Debug 开关状态不变，后续准入的 Run 继续正常捕获。

## 7. Debug 开关与脱敏

### 7.1 开关

- 开关是当前 Gateway 进程的原子运行态；默认关闭，重启后关闭；
- 每次开启都显示确认：除 HTTP `Authorization` header 值外，Trace 会原样保存其他 header、URL、body、提示词、工具参数、业务数据与媒体；关闭后已有 Trace 仍按保留期存在；
- 开启状态下提供「清除 Debug 数据」操作，删除全部已保留 Trace，不影响开关与请求记录；
- 每个 Inference Run 在准入时独立快照；同一 Interaction 可以完整、部分或完全没有 Trace；
- Rejected Request 在 ingress 时快照，并可生成只含 client request/platform error response 的独立 Trace；没有上游方向不算缺失；
- 关闭只影响之后准入的 Run，不删除已有数据。

### 7.2 脱敏

普通 Observation、错误与进程 log 的既有脱敏不变：credential header、URL userinfo 和凭据类 query、结构化 body 中明确的凭据字段，以及上传授权和签名下载 token 继续在写入前按现有规则替换；可逆脱敏引用仍作为机器原子处理。普通 Observation 不因 Debug 的捕获策略放宽保护。

Debug Trace 是明确的例外：只把 HTTP `Authorization` header 的值替换为 `***`，且必须在进入异步队列、临时文件或分段存储前完成。其他 header（包括 API key、Cookie、Set-Cookie 与 Proxy Authorization）、URL、query、body、prompt、工具参数、工具结果、媒体和 WebSocket message 均原样保留，不执行结构化字段递归脱敏、协议专用凭据识别、消息重组脱敏或媒体外置。Bundle 原样封装该 Trace，并在 manifest/README 说明只发生 Authorization redaction；开启确认必须明确这会保存其他凭据与全部业务内容。

## 8. Interaction Debug Bundle

### 8.1 时间点快照

运行中也允许导出。签发票据时固定 `through_event_sequence`，Bundle 只承诺覆盖该 sequence 之前已落盘的事件，并记录：

- `exported_at`
- `through_event_sequence`
- `interaction_status`
- 每个 Run 的 debug 快照、Trace 状态、字节数和缺失/部分原因
- Bundle 总体 `complete | partial | none`

Interaction 结束后可以重新导出新的终态 Bundle。不得把运行中快照称为最终完整包。

Trace writer 在 snapshot 屏障处 flush，并固定最后分段及其已落盘字节水位。后台读取不得越过该物理前缀；屏障后的记录即使使用相同 event sequence，也不能进入既有快照。

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

`manifest.json` 是机器契约；`README.txt` 只说明 schema version、Debug 仅替换 HTTP `Authorization` header 值、其他内容与媒体原样保留、捕获属于应用协议级而非 TLS/TCP packet capture，以及 partial 的含义。ZIP 使用 stream mode 生成，不在内存中组装整个文件。

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
GET    /api/v1/observations/interactions/{id}/summary
GET    /api/v1/observations/interactions/{id}
GET    /api/v1/observations/interactions/{id}/events
GET    /api/v1/observations/rejections
GET    /api/v1/observations/rejections/{id}
GET    /api/v1/observations/failed-requests
GET    /api/v1/observations/failed-requests/{kind}/{id}
GET    /api/v1/observations/events?after=<sequence>
GET    /api/v1/observations/debug
PUT    /api/v1/observations/debug
DELETE /api/v1/observations/debug
DELETE /api/v1/observations/history
POST   /api/v1/observations/interactions/{id}/debug-bundle-tickets
POST   /api/v1/observations/rejections/{id}/debug-bundle-tickets
GET    /api/v1/observations/debug-bundles/{ticket}
```

Interaction forest 查询参数：

- `start_at` / `end_at`：Unix 毫秒时间，必须同时提供，且 `0 < end_at - start_at <= 86400000`；按 `[start_at, end_at)` 查询，包含起点、不含终点，显式边界优先于旧参数；Interaction forest、Rejected Requests 与 Failed Requests 使用相同约束；
- `anchor_at` / `window_index`：仅为现有 API 调用者保留的旧窗口参数；未提供显式边界时，0 为下界固定在 `anchor-24h`、无上界的实时页，后续历史页按 24 小时分段。WebUI 始终发送显式边界，包括实时预设；
- `cursor` / `limit`：同一时间页内按根链游标分批加载；
- `provider`、`model`、`api_key`、`status`：匹配任一 Interaction/Run 后返回完整根 DAG；
- 每个节点带 `matched`，前端对非命中节点降噪而不删除；节点另带 `failed_request` 与 `client_output_delivered` 聚合：前者表示该交互含至少一条按「失败的请求」口径的失败 Run（判定谓词与失败列表一致），后者表示任一 Run 已向客户端提交过输出（首个客户端可见字节）。两者是事实字段，不改变 forest 返回的完整性。

响应同时给出 window bounds、根链总数、next cursor 与 snapshot event sequence。根链按其最新 Interaction 的 `last_active_at` 归入且只归入一个时间页；返回时补全该根 DAG 在保留期内的全部 Interaction。

SSE 通过普通 `fetch` 携带 Admin Bearer header，并由 `eventsource-parser` 解析；Admin token 不进入 query string。

普通详情只返回整个 Interaction 最新 200 条事件及 `older_events_cursor`。事件分页使用互斥的 `after_sequence` / `before_sequence`，以及固定快照上界 `through_sequence`；`limit` 默认 200、最大 500。负游标、超出快照的游标及无效 limit 返回 400。返回 `runs`、`snapshot_sequence`、`next_cursor`，每个 Run 包含本页事件，页内按 sequence 升序；仅在仍有后续页时返回游标。增量读取固定同一上界直至分页完成，历史加载向前翻页。诊断包独立读取完整截止历史，不受详情窗口限制。

失败请求列表 `GET /api/v1/observations/failed-requests` 合并准入前 Rejected Request 与最终 `status=failed` 的 Inference Run，是既有观察数据的查询投影，不新建执行记录或重复累计用量：

- 查询参数沿用 Interaction forest 的 `start_at` / `end_at`（或兼容的 `anchor_at` / `window_index`）与 `cursor` / `limit`（默认 50、最大 200），另支持 `provider`、`model`、`api_key` 筛选；`model` 匹配模型路由的 28 位 ASCII 小写字母不透明 ID，`api_key` 匹配 key ID 或名称，`provider` 匹配该 Run 实际调用过的模型服务；
- 排序为 `started_at` DESC、`kind` ASC、`id` ASC，`next_cursor` 是不透明的 JSON keyset 游标，相同时间记录跨页不重复、不遗漏；`total` 为当前时间范围与筛选条件下的总数；
- 响应为 `{ data: { items, total, next_cursor, snapshot_sequence } }`；每项 `FailedRequestSummary` 包含 `id`、`kind`（`rejection | run`）、`request_id`、`started_at`、`duration_ms`、`api_key_id`、`api_key_name`、`client`、`model`、`model_display_name`、`services`（实际调用过的模型服务去重列表）、`error`（`source` 取 `platform | upstream | null`，附 `code`、`message`、`status_code`）、`interaction_id`、`root_id`、`run_id`、`debug_status`（`none`、`partial` 或 manifest 状态）与 `observation_gap`；
- 分类以一次客户端请求的最终结果为准：内部重试或切换模型服务后最终成功的请求不进入列表；单纯主动取消或断线（含 499、`request_aborted`、`cancelled`、`client_disconnected`、`websocket_delivery_dropped`）被排除；后来重新发起并成功的请求不抹掉早先失败行；
- 失败 Run 的开始时间取 ingress 接收时间，结束时间在终止方截取，不使用 writer 入队时间；历史记录缺少的字段如实表达缺口：0045 之前的 Rejected Request 无 `started_at` 时回退按 `occurred_at` 排序并置 `observation_gap=true`，缺失诊断保持未知，不能补回；
- `GET /api/v1/observations/failed-requests/{kind}/{id}` 返回 `{ data: { request, events, trace, snapshot_sequence } }`；`trace` 是既有 Debug manifest 元数据。诊断包仍通过既有 Interaction/Rejection ticket 与下载入口获取，该列表不新增 Debug 捕获，也不改变脱敏与保留期规则。

## 10. 请求记录页面

### 10.1 信息架构

页面标题和导航继续使用“请求记录”，主体分为：

- `交互链路`：默认页签，Interaction forest 无限画布；
- `失败的请求`：Failed Requests 表格列表与详情，不伪造画布节点。

页面 header 包含实时状态、时间预设、精确日期时间范围、全屏切换、筛选、Debug switch 和“清除历史记录”；Debug 开启时额外提供“清除 Debug 数据”。全屏保留当前筛选、选中节点及检查器，支持工具栏退出和 Esc 退出。普通 CSV 导出删除。Debug Bundle 按选中的 Interaction/Rejected Request 提供。

#### 失败请求列表

“失败的请求 / Failed Requests”页签使用传统表格列表，不使用卡片或拓扑。失败口径遵循 `CONTEXT.md` 的 Failed Request 定义，以每次客户端请求的最终结果判断，而非仅按 HTTP 状态码或内部上游尝试判断：准入前拒绝与最终 `status=failed` 的 Run 进入列表；内部重试或切换模型服务后最终成功的请求，以及单纯由客户端主动取消或断线终止的请求不进入列表；后来重新发起并成功的请求不抹掉早先失败行。已归属 Interaction 且交付过客户端可见输出的失败 Run 仍保留在原交互链路中；从未交付任何客户端可见输出且最终失败的交互按 `CONTEXT.md`「交互链路」成员规则默认不进画布，可从失败列表跳转或深链回看。准入前失败只在失败列表展示，不伪造交互关联。

列表按请求开始时间倒序（`started_at` DESC、`kind` ASC、`id` ASC，契约见第 9 节），每次失败请求一行，默认七列为：

| 列 | 内容 |
| --- | --- |
| 时间 | 请求开始时间；缺少开始时间的旧拒绝记录回退按 ingress 时间排序并标记 `observation_gap`。 |
| 客户端 / API Key | API Key 名称或客户端来源；未认证时显示“未认证”，不可得时显示“—”。 |
| 模型 | 请求的路由模型；无法解析时显示“—”。 |
| 模型服务 | 实际调用的模型服务；未调用上游时显示“—”。 |
| 错误来源 | 平台错误或上游错误；未知时显示“—”。 |
| 错误 | 简短原因及可用的 HTTP 状态码，完整内容进入详情。 |
| 耗时 | 请求开始到终止的持续时间；不可得时显示“—”。 |

点击行打开失败请求详情，查看完整错误、请求标识、事件与已有 Debug manifest 状态；存在所属 Interaction 时提供跳转对应交互节点的入口，否则不生成无效跳转。列表不展开大段错误。历史缺失的来源快照与失败诊断不能补回，缺失字段如实显示，不虚构数据。诊断包沿用既有 Interaction/Rejection 下载入口，不新增 Debug 捕获；Debug 保留期与脱敏规则不变。

### 10.2 画布

使用 `@xyflow/svelte`：

- 无限 viewport；
- 空白处拖拽平移；
- 滚轮以指针为中心缩放；
- touch pan 与 pinch zoom；
- 节点可选、可聚焦，但不可拖动、删除、重连或创建连接；
- `onlyRenderVisibleElements` 启用；
- 节点、布局与连线共用固定的 288×256 CSS 像素几何及上下连接点；未取得 worker 布局的新节点不挂到原点，不为测量尺寸或连接点而一次性挂载全量卡片。保留完整图数据，由视口及可见连线端点决定实际挂载，移动与缩放时按需更新；
- “适配全部”直接按完整布局的已知几何计算视口，不等待屏外节点的 DOM 测量，也不只适配已经挂载的卡片；
- 未改变的节点沿用原引用；同一确认父节点下的候选共享祖先判定，避免长链反复回溯相同前缀。该缓存仅属于当前判定，不改变确认、推断与原生压缩关联语义；
- 提供适配已加载内容、回到进行中、缩放、可折叠 minimap；
- 所有屏宽都使用同一画布；窄屏点击节点后详情全屏。

使用 `@dagrejs/dagre` 在 worker 中计算 top-to-bottom 子树布局。布局使用画布实际显示的全部连接；被诊断关联连接的 Generation Chain 根归入同一视觉分组，后续 Interaction 位于来源下方。同一确认父节点的多个确认子节点横向分叉。若确认父边跳过了更早的推断续接，且该确认子的 `retained_tail` 推断来源指向该中间节点（输入含中间轮，例如切模型后再切回并带上其中间输出），画布把它接到中间节点下方，不并列分叉。推断来源仍是确认父、无匹配或 `ambiguous` 时保持分叉（输入不含中间轮，是从原链真实分叉）。旧记录缺少尾部事件时，仍按时间接到最新推断中间节点。这只改变视觉父边，不改写 `parent_interaction_id` 或 Generation Chain。不相连的分组沿用页面快照的根顺序，实时数据只重排受影响分组；关联变化也必须触发布局更新。视觉分组不改写后端根身份、根计数或执行父链。分组拓扑改变后按当前节点重新计算宽度，收回已消失分叉占用的空间，不保留历史最大宽度。跟随、直接链接与“回到进行中”按目标卡片居中，空间允许时恢复 1:1 缩放；窄小视口按卡片宽高缩小，不为容纳完整视觉分组而压缩目标。“适配全部”仍按完整布局显示总览。

保留尾部推断关联与已确认直连使用相同的底部 source、顶部 target、路径、颜色与线宽，只以虚线区别；单一续接上下对齐，不为跨根关联绕到卡片侧面。连线上与卡片预览中均不附加推断关联说明，具体关联类型仍可在诊断详情中查看。

一个时间页先加载最新一批根链的完整子树；横向接近已加载边缘时按 cursor 加载下一批。未加载完时显示 `loaded / total`。“适配全部”先加载剩余根链并显示进度，再计算完整 bounds。

实时更新使用 `GET /api/v1/observations/interactions/{id}/summary`，返回 `InteractionSnapshot { interaction, root, snapshot_sequence }`。该接口保留原详情的筛选、完整根链和时间参数校验，但只读取已持久化的摘要与关联事件，不读取 Run、普通事件正文或 Trace，也不等待 Trace flush。检查器初次加载有界详情，之后按 sequence 增量读取；向上滚动接近对话顶部时自动向前分页并保持滚动锚点，不再显示手动加载按钮，未变更 Run 与消息沿用原引用。易失通知只更新文本预览，不触发 HTTP 请求。选择、关闭或时间范围变化后，旧请求不得覆盖新的检查器状态。Forest、summary 与 detail 共用有界批量关联事件查询，按交互分组并保持 sequence 顺序及快照上界。点击下载时不沿用页面的旧截止序号，由服务端完成目标屏障后固定票据快照。

### 10.3 时间页与迁移

实时预设包括 5、10、30 分钟以及 1、4、12、24 小时；前端随当前时间推进起止边界并发送显式 `[start_at, end_at)`，窗口宽度始终保持所选时长，不会因长时间打开而扩大。自定义范围通过本地日期时间输入转换为 Unix 毫秒，应用后保持固定边界，起点必须早于终点且跨度不得超过 24 小时；恰好 24 小时有效，超限不能应用。Interaction Chains、Rejected Requests 与 Failed Requests 使用相同时间窗语义。时间边界只决定根链成员资格，不截断返回的因果上下文，也不限制详情中的完整 DAG。

根链若因新活动跨入更新的时间页：

- 当前已打开画布不立即删除或移动它；
- 显示“此链已迁移到较新的时间页”提示；
- 点击提示跳转新时间页并聚焦该链；
- 新查询严格按新的 last activity 归页，不在两页重复。

### 10.4 Interaction 卡片

固定尺寸卡片显示：

- `本地开始时间 · 首个 Run 的 Model Display Name`；为空回退 Route ID；模型名字号比原卡片标题缩小一级，为正文预览留出空间；
- 主状态；只有真实执行时显示绿色呼吸圆点；含失败 Run 的交互终态按「失败的请求」口径显示为「失败」（destructive 菱形），运行中与等待客户端保持过程状态；
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

选择实时预设时，默认聚焦最新活动或正在执行的节点。用户一旦平移、缩放或选择旧节点，自动跟随暂停；程序定位产生的无输入源移动结束事件不暂停跟随，也不触发边缘分页。视口外有活动时显示“有新活动 · 跟随”。点击后恢复并聚焦当前最新的真实执行节点。

### 10.6 右侧检查器

桌面使用可调宽、可关闭的右侧检查器，默认约占 40–55%；画布保留选中节点及其因果路径。窄屏使用全屏详情。

默认「对话」页以只读消息气泡展示当前 Interaction：用户靠右使用 primary 色，模型靠左使用中性底色。连续同一模型的 Run 共用一组头像与名称，正文和工具继续追加在同一块内，只在末尾显示最后一条消息的时间；换模型或出现用户消息时重新分组。时间旁不显示任何执行状态或预览说明，执行状态仍在画布与诊断中保留。初始用户消息取已脱敏的 `input_preview`，缺失时可用初始 Run 的 `input_preview_recorded.text` 补足；后续 Run 的输入事件在所属回复前生成独立用户消息，不因文本相同而去重。每个 Run 的回复只拼接按 sequence 排序的 `client_visible_content_delta.text`，不把 Debug 内容当作回复。没有公开文本事件的旧记录只回退一次到 `visible_tail`。没有用户正文时不生成用户消息，没有助手正文时隐藏气泡，但保留流式组件实例，保证首个实时增量仍可逐字显示。

思考和工具使用官方 shadcn-svelte Marker，默认折叠；有真实详情才提供展开操作，没有可读思考则不显示条目，只有工具名称时显示静态行，不增加「未记录」说明。思考置于所属 Run 正文前，工具置于正文后；展开显示普通观察事件记录的可读思考、工具输入和返回，无需开启 Debug。详情与对话不读取 Debug Trace，也不从旧 Trace 补充内容或补录未采集的历史。签名、密文不进入普通思考正文。

工具调用按 Run 和调用 ID 关联：普通平台事件的 `tool_id` 就是调用 ID，客户端返回来自 `client_tool_result`。返回仅匹配明确 `parent_run_id` 祖先，祖先路径上的历史重放不重复展示，兄弟分支各自收到的返回独立保留。工具输入与返回以安全纯文本或 JSON 呈现，不递归猜测业务 JSON、不执行 HTML 或加载远程媒体。每条 Marker 以稳定活动 ID 独立保存 localStorage 展开布尔值，折叠时删除该项；不保存正文，存储失败明确提示但不阻断展开。展开已有内容不制造「新活动」提示；后续真实内容变化仍可提示，且不收起已展开条目或抢走阅读位置。

正文复用卡片的安全 Markdown 渲染，lexer 与 parser 均显式启用 GFM，表格继续经过既有 HTML 安全白名单。历史首次打开立即显示；运行中新增后缀按 Unicode grapheme 逐字呈现，批量新增及时追平，结束、文本替换或减少动态效果开启时直接显示当前已收到的文本。此动画不改变后端最长一秒的合并与 SSE 更新契约。处于底部时随逐字增长跟随；用户向上翻阅或展开活动后保持阅读位置，只有点击「回到最新」或主动滚到底部才恢复。切换 Interaction 重置跟随，不滚动外层页面。

「诊断」页默认呈现可读的事件摘要、时间与已记录的关键事实和结果。`parent_run_id` 表达续接与因果而非包含关系：Run 按 `started_at` 拍平为并列分段并依序编号（R1、R2…），不再嵌套缩进；续接关系以分段上的「续接自 Rₙ」标记表达，父 Run 属于同一 Interaction 时可点击回跳，属于其他 Interaction 时仅显示静态标记。相邻 Run 结束与开始之间超过快速续接窗口（2 秒）的等待显示为间隔行：上一请求以客户端工具调用结束时标注所执行的工具名，否则只标注间隔时长。每个 Run 内的事件统一按 `occurred_at` 升序、同一时刻按 `sequence` 升序排列，不再将 Model Turn、Target attempt 或工具的子树整体提前展开，以免把较晚的完成事件放到较早的客户端输出之前。拒绝请求的事件采用相同排序规则。每个事件的「原始事件数据」默认折叠，展开后保留原始 kind 与完整 payload，因果关联字段不丢失；Run 和 Interaction ID 收在默认折叠的「技术标识」中。未知事件仍保留原始数据入口，不推断成功或其他未记录的结果。Run 标题优先使用模型显示名、缺失时使用 Route ID，状态、耗时与用量仍可见。

排序后相邻、已经分别按 Canonical Item 收口的 `client_visible_content_delta` 只在界面上组成默认折叠的计数分组，不改写或合并独立 item；相邻且 `name` 相同、非空的 `client_tool_handoff` 同样仅作展示分组，例如「Bash × 4 · 已交给客户端」。分组显示首次和末次事件时间，不跨越其他事件、工具名称或 Run。展开分组保留每条事件的时间与完整原文入口，实时追加保持已有分组的展开状态。事件行将原始数据入口收至标题右侧箭头，不再重复占用一行按钮；关键结果和错误仍直接可见，不因精简而隐藏。

`target_attempt_finished` 的耗时后显示 Token 速度。输出用量来自同一 Run、相同 `attempt_id` 的最后一条 `usage_confirmed`（按 sequence 判断），不累加累计快照，也不借用整个 Run 或其他 attempt 的用量。速度复用 `computeTps` / `formatTps`：有有效首 Token 时间时使用既有净生成耗时与非增量流判定，否则使用上游耗时；缺少用量或有效耗时显示未知。卡片输出浮层使用「模型输出预览」名称；画布的已确认执行来源边保留连线、取消重复文字标签。

普通诊断显示生命周期、Route/Target、协议、状态、耗时、Confirmed Upstream Usage、客户端可见事件。Debug 的四方向 Wire headers 与原始 body/frame 只通过 Debug Bundle 下载提供，不再内嵌展示、复制或提供单事件下载；Bundle 不包含 canonical、Hook 或 Client Projection 中间阶段。Interaction 与 Rejected Request 详情不返回 `debug_events`，不打开或解析 Trace 分段；保留 manifest 状态与缺失原因，运行中 manifest 可从内存捕获状态更新。下载沿用有界快照与单次 ticket，不改变捕获、脱敏、保留或清理规则。未开启 Debug 不影响普通诊断访问，实时刷新不得把选中的诊断页签切回对话。Rejected Request 默认显示简洁失败摘要，不伪造成模型对话；技术原因仍在诊断中。

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
4. 每次新增或修改 migration，使用 `stravia-tools dump-schema` 从全部 migrations 同步重新生成 `docs/database/postgres.sql` 与 `docs/database/sqlite.sql`，不手改 schema 正文；
5. 验证相关 SQLite 与 PostgreSQL 存储测试。

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
- 超出快速续接窗口且不提交待完成工具结果的新 User 输入创建子 Interaction；原未完成分支显示 user interrupted。
- 工具结果夹带 User 提醒时仍归入原 Interaction，工具执行耗时不影响判断；历史中的旧工具结果不能触发此规则。
- 精确父响应完整交付后 2,000 毫秒到达的新 User 输入归并，2,001 毫秒到达且不满足工具续接的请求分开；排队处理耗时不改变判定。
- 已完成响应后两秒内精确续接可重新激活原 Interaction；真人快速追问与自动提醒遵循同一规则，原 Run 完成记录不变。
- 明确 Generation parent 的失败重试恢复原 Interaction。
- 无 parent 根失败在两分钟、同 Principal、精确 fingerprint、无 Client Output Commit 时归并；超时、不同 Principal、已有 output commit、并发相同请求均不归并。
- 同一父 Interaction 的两个不满足归并条件的新 User 分支在画布真实分叉，不复制祖先。

### 13.2 时间与加载

- anchor 边界使用左闭右开规则，无重复和遗漏。
- 跨多个 24h 窗口的根 DAG 只按最后活动时间归一页，页面内显示保留期内完整链。
- 打开旧页后链发生新活动，视图固定并显示迁移提示。
- cursor 分批加载不改变已加载根的相对顺序；筛选返回完整根 DAG并正确标记 matched。

### 13.3 实时与状态

- 本机正常负载下，文字、状态和 usage 从 core event 到 UI 可见不超过 1 秒；实时内容不等待诊断 item 落盘，也不推进持久 cursor。
- 同一 Canonical Item 的 delta 在收口时形成一条持久诊断内容，独立 item 与项内 part 边界保持不变；失败或取消保存已收到部分并标为未完成，硬崩溃允许丢失未收口 item。
- 真实执行时绿点呼吸；等待客户端工具结果时静态琥珀；reduced motion 下不呼吸。
- 用户操作画布后停止自动跟随，新活动不抢镜头；点击跟随恢复。
- SSE 查询/订阅无缝衔接，断线按 cursor 补齐；过期 cursor 触发完整重载。

### 13.4 Debug

- 每个 Run 独立快照开关；同一 Interaction 的 ON/OFF/ON 形成明确 partial Bundle。
- HTTP/SSE 在客户端边界及上游 reqwest 请求/响应边界、WebSocket 在相应传输边界覆盖四方向原始收发、顺序、时间和 attempt 关联。
- Debug Trace 只含 Wire 与必要关联元数据，不含 canonical request/response、Hook 前后、Platform Tool、Client Projection 或其他语义中间阶段；普通 Observation 继续保存生命周期、usage、工具事件和按 item 收口的诊断内容。
- 原始 Wire 到达即可排队，不等待完整应用消息或 Canonical Item 收口；媒体原样保留，只有 HTTP `Authorization` header 值在落盘前替换，ZIP 明确披露其他凭据与业务内容不脱敏。
- Trace 落盘不设容量上限，只统计保留字节；既有捕获缓冲、队列、writer 或存储限制继续生效，丢失时请求继续、Trace partial；不完整原因只出现在详情和诊断包中。
- 运行中票据固定 sequence；ZIP manifest 与 events 一致。
- 下载 ticket 60 秒、单次、固定资源；过期/重放/跨资源使用失败。
- 清除历史只删除非活动 Observation 及 Trace，活动请求不中断。
- Observation writer、Trace writer、ZIP 生成失败均不改变推理响应或 Generation Chain。

### 13.5 产品表面

- 1280×800 Desktop、常见桌面浏览器和窄屏触控都能平移、缩放、选择节点与打开详情。
- 键盘可聚焦节点、打开详情、关闭检查器和操作画布控制；状态不只靠颜色。
- 英文与中文文案意图一致；页面仍名“请求记录”。
- 升级删除旧日志后，空状态明确说明新请求会在此出现，不暗示迁移失败。

## 14. 跨 Generation Chain 根归属

本节落实 [ADR-0053](../adr/0053-keep-one-interaction-across-generation-roots.md)：Generation Chain 仍只记录真实执行父边；Interaction Observation 在没有执行父边时用当前工具续接或保留尾部做诊断分组。新规则只作用于启用后准入的请求。

### 14.1 范围

- 只对启用新规则后准入的请求执行新归属判定，不重新分配已有 Run、不改写已有来源关系，也不提供存量 Observation 重建工具。
- 新请求正常续接已有 Interaction 时，继续按现有生命周期更新活动状态和汇总；这不表示重算历史 Run 的归属。
- 不改写 Generation Chain 节点、恢复裁剪内容、重放 Hook 或据此启用 Target Continuation。
- 不从 User 正文中的通知标签推断可信请求用途，不按 session、模型、Route 或时间最近猜测来源；标题、摘要、子代理不因共用 session 而归入主任务。
- 实现保持 SQLite/PostgreSQL 等价，不新增生产依赖；具体 schema 或公共响应字段如需扩展，在实施前明确变更契约。

### 14.2 实现要点

1. **归属判定集中在 Observation 模块。** 已确认 Generation parent 走原路径；没有执行父边时独立解析诊断来源，再决定归并、创建有来源的新 Interaction 或保持独立。Server、Desktop 与 WebUI 不复制判定规则。
2. **当前工具续接。** 用同 Principal 已交付调用的未完成工具 ID 与当前输入尾段精确匹配；旧结果回放、重复或冲突来源、缺失交付证据不能宣称唯一。确认后优先归入来源 Interaction，即使夹带 User 或超过尾部五分钟窗口。
3. **尾部指纹索引。** 以最后 canonical 单元哈希筛选候选，再做完整语义核验。同 Principal 历史超过 128 个不再导致全部匹配失败。指纹不代替核验，也不按时间窗口排除潜在冲突来源。
4. **按需物化。** 缺失窗口从仍保留的 Generation Chain `client_items` 重建；先合并内存和持久化候选、去重并检查候选预算，再批量读取缺失窗口。已确认父节点本身属于候选时优先展开其父链，在完整校验后按 root 到 head 折叠客户端历史，同链祖先候选复用本次遍历的窗口，不各自重新展开整链。只保留本次候选窗口，不缓存所有完整历史前缀，也不引入跨请求缓存；重复来源和其他分支仍参与原有歧义核验。进程缓存可淘汰，过期或已清理来源不复活。核验超过资源预算时返回 `resource_limit` 或 `index_unavailable`，不把部分检查包装成唯一匹配。
5. **准入时持久化。** `run_admitted` 同时保存 `grouping_reason` 与 `diagnostic_source_run_id`；尾部核验结果以 `retained_tail_associated` 同轮写入。诊断来源不是 `generation_parent_id`。只有新增 User 打断父交互时才 `interrupt_predecessors`；归入本 Interaction 的续接准入在同一事务内把仍等待的父 Run 终结为 `superseded`（见 3.2），不归入中断。
6. **派生视图。** 合并后的 Interaction 共用状态与用量；诊断连接的新子交互分别汇总。失败、取消和交付事实不因后续成功改写。
7. **契约。** README 两种语言、schema 文档与 `0047_observation_tail_sources` 迁移同步。页面继续区分确认边与诊断边。

### 14.3 决策表

本表适用于没有已确认执行父边、需要跨根诊断归属的请求。

| 条件 | 结果 |
|---|---|
| 唯一确认当前工具续接，包括夹带新增 User | 归入来源 Interaction，不受尾部五分钟窗口限制 |
| 未满足工具续接；唯一完整尾部匹配，无新增 User，间隔在 `[0, 300000]` 毫秒内 | 归入来源 Interaction |
| 未满足工具续接；唯一完整尾部匹配，匹配区间之后有新增 User | 新 Interaction，诊断连接来源 |
| 未满足工具续接；唯一完整尾部匹配，无新增 User，但超出五分钟窗口 | 新 Interaction，诊断连接来源 |
| 无法证明满足时间窗口，但完整唯一的来源证据仍成立 | 不自动归并，保留来源连接 |
| 来源缺失、匹配不完整、有歧义或候选核验未完成 | 不自动归并，不猜诊断父节点 |

### 14.4 回归与运行验证

三个实际断点转为不含凭据或业务原文的隔离样本，通过真实准入、归属、持久化和查询路径验证，不将执行 ID、正文或源文件内容写成特例。

- 删除首条 User 图片，保留原文本并提交来源的三个当前工具结果：同一 Interaction、新 Generation 根，裁剪图片不重新进入模型输入。
- 删除旧工具截图，同时新增另一张工具截图且图片总数不变，并提交四个当前工具结果：仍正确确认来源，最终回复留在同一 Interaction。
- 旧历史被摘要替换，保留精确连续尾部并提交当前工具结果：正确续接；另设没有当前工具结果的样本单独验证尾部归并，避免工具路径掩盖尾部缺陷。
- 尾部无新增 User：恰好 `300000` 毫秒归并，`300001` 毫秒创建新交互但仍连接来源；缺少有效时间证据不自动归并。
- 尾部后新增 User：窗口内外都创建子交互；同时满足当前工具续接时，验证工具续接优先。
- 同 Principal 超过 128 个保留调用、进程重启或缓存淘汰后，具备完整证据的来源仍可被发现；核验预算耗尽时不误报唯一来源。
- 旧工具结果回放、重复 handoff ID、不同 Principal、并列来源、仅短文本或不完整交互均不能触发误归并；窗口外的冲突候选不能因时间过滤被忽略。
- 同模型或 session 的标题、摘要及独立子代理不被错误归入主交互；模型或 Route 不同也不单独成为拒绝合法来源的理由。
- 合并后的状态、用量与 Debug Bundle 不重复计算，子交互分别汇总；历史失败与交付记录保持原事实。SSE 和刷新后的详情、forest 得到一致归属。
- 来源过期或被清理后不复活节点；新规则不重分配既有 Run。诊断失败不改变客户端响应、执行重试或 Generation Chain。

验证先运行最直接的 Rust 回归，再扩大到 core 检查和相关 SQLite/PostgreSQL 存储用例。使用隔离、非生产的模型服务进行实际 HTTP 请求和浏览器检查，观察新请求形成的交互、父子连接及最终回复；实现触及 Desktop 特有行为时再验证实际桌面应用。
