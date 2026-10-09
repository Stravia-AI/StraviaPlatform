# Connect Client Interaction Observation 设计

> Canonical Item 的诊断持久化边界以 [ADR-0062](../adr/0062-persist-diagnostic-content-at-canonical-item-boundaries.md) 为准；Debug 的 wire-only 捕获边界以 [ADR-0063](../adr/0063-record-four-direction-wire-debug-at-transport-boundaries.md) 为准；事件收敛与 Debug manifest 文件化以 [ADR-0077](../adr/0077-slim-interaction-observation-and-file-debug-manifests.md) 为准。

## 1. 目标

把现有按单条请求展示的 `request_logs` 与仅限 debug 构建的 `wire_capture` 干净切换为统一的 `Interaction Observation`：

- 请求记录页以 `Connect Client Interaction` 为画布节点，以 `Generation Chain` 因果关系连接节点；
- 一次 Interaction 覆盖一次新 User 输入到最终生成响应之间的一个或多个 Inference Run，并允许 Run 子树分叉；
- 正在执行的状态、客户端可见输出和 Confirmed Upstream Usage 通过轻量变更通知、按 root 的差量查询与选中正文 SSE 更新；主动调度等待共享 500ms 预算，网络、数据库与实际渲染服务时间另计，不承诺固定端到端完成时间；
- Debug 按每个 Inference Run 准入时的进程级开关快照生效；
- Debug Trace 只覆盖四个方向的原始应用协议级收发：上游 HTTP 在 reqwest 请求/响应边界，WebSocket 与客户端在各自传输边界；不记录 canonical、Hook 或 Client Projection 中间阶段；
- Interaction Debug Bundle 以版本化 ZIP 流式导出，明确完整、部分或缺失状态；
- 普通 Observation 保存生命周期、usage、工具事件，以及按 Canonical Item 收口的可读思考与客户端可见内容；不保存完整 canonical request/response 或 wire payload，也不采集模型思考的签名和密文；
- Observation 只服务诊断，不成为推理执行、Generation Chain 或模型历史的事实源。

本设计同时适用于 SQLite 和 PostgreSQL 存储，但实时状态与 Debug 开关只承诺单 Gateway 实例。多实例聚合不在本设计范围内。

Observation 使用统一平台身份契约：随机不透明 ID 是由密码学安全随机生成器均匀采样的 28 位 ASCII 小写字母（约 131.6 bit），完整 SHA-256 派生身份则用 55 位 ASCII 小写字母保留全部 256 bit。Artifact Reference 为 `stravia://artifacts/<55 位 ID>`，Turn Reference 为 `stravia://turns/<28 位 ID>`，Search Source 为 `stravia://turns/<28 位 ID>/sources/<ordinal>`；History Marker 为 `<!--sh:<28 位 ID>-->`，Projection Delimiter 为 `<!--sp:<28 位 ID>:<t|p>:<ordinal>:<s|e>-->`，可逆脱敏引用为 `<!--sr:<28 位 ID>-->`。这些外壳不授予访问权，外部 Provider／客户端 ID 与真实凭据 token 不变；旧平台引用不提供兼容解析，已有不可变或外部历史不回写。

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

1. 一条新的 canonical `User` item 通常开启新的 Connect Client Interaction；已确认的当前工具续接与两秒快速续接按以下规则归并。
2. 无法归入已有 Interaction、且不含 User item 的合法根请求也开启新的 Interaction。
3. 客户端公开工具调用结束当前 Inference Run。同一 Principal 下精确续接父响应、没有新增 User item 的请求继续原 Interaction；请求 delta 的当前输入尾段提交父历史中尚未得到结果的工具调用所对应的结果时，即使夹带新增 User item 也继续原 Interaction，不限时间。当前尾段从 delta 最后一个 Assistant item 之后开始；顶层工具返回与 User 内容块中的 ToolResult 使用相同判定。此外，Observation 独立核验完整输入当前尾段的工具结果：若唯一对应同 Principal 已交付来源的待完成调用，则优先归入该来源 Interaction，并以来源 Run 作为观测父节点，即使严格前缀退回较早 Generation parent。完整历史中的旧工具结果不构成归并证据，历史编辑导致父节点退回更早位置也不例外。
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

Generation parent 存在但对应父观察不可用时，新准入记录 `generation_parent_observation_unavailable` gap，不伪造该父节点的观察；独立确认的当前工具来源仍可决定观测归属。未捕获、已清理、过期或查询失败都可能造成这类缺失，不能仅据此断言历史存储故障。真正的交付后 Generation 提交失败保留 `settlement_generation_commit` 诊断，界面明确“响应已交付，历史未保存”，不把已交付响应改报为请求失败，也不自动重放。

### 3.2 Interaction 主状态

主状态只表达当前最需要用户注意的活动，不显示分支计数：

1. 任一 Run 正在执行：`running`，绿色圆点呼吸；
2. 无 Run 执行，但任一叶分支等待 Connect Client 工具结果：`waiting_client`，静态琥珀圆点；
3. 无活动分支且至少有最终生成响应：`completed`；
4. 既无活动分支，也无最终生成响应，且仍有因客户端连接关闭或等待超时结束等待的叶分支：`disconnected`，显示“已断开”；
5. 其余终态：`interrupted`，详情保留 `failed`、`cancelled`、`delivery_failed`、`user_interrupted` 等原因。

`failed_request` 表示历史中存在失败请求，不覆盖 `completed`、`running` 或 `waiting_client` 主状态；这些状态使用独立的低权重历史失败标记。无活动且以失败结束的交互仍显示失败。`visible_tail` 在每个 Model Turn 开始时（已有输出且尚未以空行结尾）追加一个空行，使预览按 Markdown 段落分隔各 Turn 的输出。`visible_tail` 为空或仅包含空白字符，而 `client_output_delivered` 为真时显示“已交付输出，暂无文本预览”，不能否定已经交付的工具调用或思考预览。该判断不修改 Markdown 原文或代码缩进，也不把思考内容替换为公开正文。

响应完成不等于后台工具完成。仅在最后一个 `RunObserver` 释放、确认不会再产生该 Run 的事件后，writer 才把残留运行中的 Model Turn／Target attempt 记为 `interrupted`，释放残留活动计数并追加 `unfinished_observation_activity` gap。后台执行持有的观察句柄继续保护真实活动；收口不改写 Run 的交付状态、Generation 关联或已确认 usage，不将缺失的结束事实推断为成功。

等待客户端的常规解除只凭客户端回传证据，不使用会话级猜测超时。同一 Interaction 内，一个 Run 的全部公开工具交接都已收到其他 Run 中 sequence 严格更晚的同 ID `client_tool_result` 时，该分支不再参与活动等待聚合，即使结果来自 sibling、结构上仍是叶节点。错误工具结果同样证明客户端已回传，但不证明最终生成成功；没有最终生成响应时，不能因此把 Interaction 标为 `completed`。判定只读取工具 ID 与事件顺序，不读取参数、结果正文或 Debug 文件；缺失或空 ID、没有 handoff、先于 handoff 的结果、部分回传以及同 ID 多次 handoff 均保守保留等待，重复 result 幂等，不跨 Interaction 或 Principal 匹配。状态计算使用尚未物理清除的历史证据，不按事件各自的到期时间截断；Bundle 只使用导出快照内的事件，不能借未来结果解除过去的等待。原 RunOutcome、工具历史和 Generation Chain 父边不改写。

Run 级别的等待分支在两条路径上终结，不无限滞留：续接 Run 准入同一 Interaction 时（`parent_run_id` 落在本 Interaction 内、非新交互打断），仍停在 `waiting_client` 的父 Run 在准入同一事务内转为终态 `superseded`、`terminal_reason='superseded'`，并发布 `run_state_changed`（payload 带 `superseded_by` 指向续接 Run）。`superseded` 是终态：不持有最终生成响应、不参与 `completed` 判定、不再被中断/断连/重启清扫改写，按既有规则过期清理。仅 sibling 证据解除、结构上仍是叶节点的等待分支保持 `waiting_client` 投影（不参与活动等待聚合），由断连、进程重启或新输入清扫终结；新输入清扫只把本 Interaction 内尚无续接 Run 的叶分支记为 `user_interrupted`，已有本 Interaction 续接的分支一律记 `superseded`（其他 Interaction 的新分支不算续接），不当作中断。

续接准入可能早于来源 Run 的 Generation 提交和 finish。来源随后以工具等待结束时，finish 事务核验同一 Interaction 内已经存在的 child，保留 `superseded` 并以该实际状态写入 `run_finished`，不能把已续接的分支退回 `waiting_client`。既有 `user_interrupted` 和真实失败的优先级不变。

HTTP 等待允许一个兜底闲置边界：叶等待 Run 的 `last_active_at` 超过 24 小时仍无回传时，随保留清理按 `client_wait_expired` 转为 `disconnected` 并发布 `run_state_changed`。窗口必须覆盖合法的长时客户端工具执行，不能缩短到会话级别；客户端可能在边界后补传结果，逾期转换不删除历史也不阻止该结果照常落库。

WebSocket 连接关闭时，该连接所属、仍在等待、没有后继 Run 且尚未由完整工具回传解除等待的分支转为 `disconnected`，记录 `client_disconnected` 原因并发布 `run_state_changed`；已完整交付的结果与 Generation Chain 保留。结果先到时不追加虚假断线；关闭先到时保留真实断线历史。其他连接的等待、已续接分支与最终生成响应不受影响。HTTP/SSE 响应正常结束不能证明客户端离线，同一进程内仍等待合法续接、24 小时闲置超时或保留期清理。旧父节点发生合法晚到续接时，Interaction 可以重新进入活动状态；无法证明连接归属的旧记录不回填 `client_disconnected`。

启动时，在 writer 与对外服务启动前以同一恢复事务修正上一进程遗留的 `running` / `waiting_client` 投影：先沿用运行活动恢复，再排除已有完整工具回传的等待叶；只剩未解决、没有 child 的旧等待 Run 转为 `interrupted`，以 `run_state_changed` 承载，事件 payload 为 `{"status":"interrupted","reason":"process_restarted"}`；不再单独持久化 `process_restarted` kind。重启只证明原观察进程结束，不证明第三方客户端离线，不取消或重放客户端工具，也不阻止旧 Generation 的合法晚到续接。恢复保留原 `finished_at`、交付完成时间、Generation 关联、committed、usage、`last_active_at` 和 `expires_at`；仅重算投影时保留原 sequence，追加恢复事件时递增 sequence，并以恢复判定时刻记录事件 `occurred_at`，但不推进请求活动时间。恢复幂等，事务失败整体回滚，不手工部分补写；不自动删除历史。既有手动清除仍保护正在运行及真正等待的记录，恢复后的 completed/interrupted 历史按既有规则可清除或过期。

HTTP 流式响应以 Delivery 确认的协议终态为完成边界，而不是客户端是否继续读取到 body EOF。终帧被 delivery stream 交出时，同步投递交付收据，先于生产任务恢复和 Generation Chain 提交；非流式响应沿用完整 body 交付确认。收据使用准入时保留的队列槽，按事件顺序先于后续准入持久化 `delivery_completed_at` 和分支终态：最终响应为 `completed`，工具交接为 `waiting_client`，已有同 Interaction 续接则保留 `superseded`。已记录的真实失败或中断不改为成功；后台工具活动独立保留。

收据不依赖可捕获的历史窗口；窗口可用时再登记客户端投影的来源索引，使立即回传工具结果的请求不依赖来源 Generation 已落盘。观察尚无结束时间时先以实际交付时间收口，迟到 finish 补充执行结束时间与已提交节点关联，保留最早实际交付时间，不把已经交付的最终响应因新 User 准入改为 `user_interrupted`。缺少保留队列槽或收据持久化失败时显式记录 observation gap，不伪造持续运行的投影。协议终态之后关闭读取不能覆盖成功结果，终态之前断线仍按中断记录。

WebSocket 在 socket writer 的实际终帧发送成功后、发送 ACK 之前同步登记交付收据与可用的已交付来源；不能把生产任务开始转发或消息入队当成交付。收据按已发送结果登记最终响应或工具等待，流生产任务随后补充执行终态和 Generation 关联。交付发布沿用已经取得的 Vendor fences，并重新检查取消、deadline 和 epoch，不等待新的写者之后重新取得读锁。连接关闭与最终交接采用同一连接范围内的同步登记，关闭先发生或后发生均能结束等待，不额外延长 Inference Run 的执行期限。

`prefers-reduced-motion: reduce` 下，`running` 使用静态绿色圆点，不播放呼吸动画。

### 3.3 用量口径

Interaction 卡片、详情与用量分析共享 `Confirmed Upstream Usage`：

- 用量分析总览、时间分桶和 Provider 汇总的错误数与「失败的请求」共用同一请求级查询：包括准入前拒绝，以及终态为 `failed` 且已结束的 Inference Run；排除内部恢复后成功、进行中、单纯取消、断线和中断。按请求开始时间归入窗口和分桶，并排除已过保留期的记录；同一请求的多次尝试或隐藏 Model Turn 只计一次。只有拒绝请求、没有 Model Turn 的时间桶也必须显示错误；
- 上述三处的请求数和错误率分母同样按客户端请求计数，不按 Model Turn 或 Target attempt 计数；准入前拒绝计入总览和时间分桶。Provider 汇总按请求涉及的服务归属，每个服务内对同一请求去重；没有涉及任何服务的拒绝不归属 Provider，一次跨服务的最终失败可分别计入多个服务，不能将各服务错误数相加当作全局错误数。Token、耗时和吞吐量仍沿用各自的模型轮次或尝试口径；
- 汇总所有 Inference Run、隐藏 Model Turn、重试和 Target failover 中上游明确报告的 usage；
- 管理面 `input_tokens` 统一表示净输入：每个 attempt 的总输入与缓存读取均已知时，先计算 `max(input_tokens - cache_read_tokens, 0)`，再累计各 attempt 的已知净值；缓存写入不参与扣减。任一操作数未知时，该 attempt 的净输入未知，`missing_input_tokens` 计入该缺失；
- output 已包含 reasoning，不再累加或单列思考指标；cache read 与 cache write 保留独立展示。管理面按输入、输出分别呈现，不以缺少缓存分项的相加结果冒充总 Token；
- 原始 IR、attempt 用量、持久化事件与 wire debug trace 保留上游总输入及 reasoning 子项；管理统计、列表、详情、分页事件查询与 Bundle 汇总在读取边界使用净输入投影，前端不再次扣减。Bundle 导出的原始 events 保持上游值，汇总先按 attempt 合并修订再计算净输入。历史查询立即使用新口径，无需改写数据库、事件或新增接口字段；轻量全局 SSE 通知仍不携带事件正文；
- 每个实际上游 attempt 的 usage 最多记一次；
- 流式 usage 在接收时观察，不以客户端输出已经提交为前提。正常完成以 Vendor 的完整返回值确认；失败或取消没有完整返回值时，以已收到的最后已知快照确认。多帧报告是同一 attempt 的修订，不重复累加；未知字段不抹掉已知值，明确报告的零可覆盖先前数值，完全未报告的字段保持 `null`；
- Target attempt 成功与明确报告的 usage 不因随后还原或映射发布失败而改写；Model Turn 的唯一终态由内部完成 gate 记录，只有发布完成且未被取消或超时抢占才记成功；
- 上游尚未报告或永不报告时保持 `unknown`，不显示为零，不用本地 tokenizer 估算；
- Interaction、Run 与 Bundle 聚合按字段累计已报告部分；某次 attempt 的未知值不抹掉其他 attempt 的已确认值。全部未报告时该字段保持 `null`，明确报告的零保留为零。失败但已报告的用量同样累计，重复报告不重复计数；用量分析的 overview、series、model、API Key 汇总只统计成功的 Target attempt，按字段累计已报告部分：某次成功 attempt 的字段未知只不计入该值，不抹掉组内其他已确认用量，全部未知时该字段保持 `null`。失败或未完成 attempt 不参与这些成功尝试统计，但不否认其在 Run、Interaction 与 Bundle 中已报告的用量；
- 全平台 TPS 使用测量对象自身的完整耗时，不扣首 Token 等待，也不使用 50ms 回退规则。首 Token 时间独立展示，不参与分母；
- Run 耗时与 TPS 共用客户端接收到交付结束的时间：`delivery_completed_at - started_at`，包括内部重试、failover、平台工具与交付等待。分子使用 Run 的全部已报告输出，包括失败 attempt 已报告的输出；coverage 表明任一输出未知时 TPS 保持 `null`。运行中、成功但缺交付时间、无有效正耗时或输出未知时不猜测速率；失败、取消与中断等未交付终态可用 `finished_at` 收口。迟到 usage 修订不延长交付耗时，客户端跨请求执行工具的间隔不属于任一 Run；
- Attempt TPS 使用自己的 `duration_ms`；Provider `avg_output_tps` 使用成功 attempt 的 `Σoutput_tokens / (Σduration_ms / 1000)`，不是单次 TPS 的平均值。成功样本缺输出或耗时、或总耗时为零时为 `null`；已知零输出保留为零。低延迟选路同样使用完整成功 attempt 耗时，仍按一小时成功率加权，保留 20 个成功样本、同组至少两个有效 Target 的门槛及既有亲和、优先级和 fallback；
- 管理统计 `StatsOverview` 与 `StatsSeries` 的 `avg_output_tps` 使用同一成功 attempt 加权比率，单位为 tok/s；无成功样本、任一成功样本缺输出或 `duration_ms`、或完整耗时总和不为正时显式返回 JSON `null`，不省略字段。窗口和桶归属沿用所属 Model Turn 的开始时间与调用方时区，不按 attempt 完成时间归属，也不平均桶或 Provider 的 TPS。成功 attempt 不因随后 Model Turn 发布失败或客户端交付失败被排除。`avg_first_token_ms` 仍对已记录的首次 canonical 输出时间取平均，包含 Thinking，保留零；缺首字时间本身不使 TPS 失效。原有 `avg_duration_ms` 与部分已知 Token 累计规则不变；
- Interaction、Run 与 Bundle 的既有聚合 `usage.coverage` 包含 `attempt_count` 和五项 `missing_*_tokens`；其中 `missing_input_tokens` 表示因总输入或缓存读取缺失而无法计算净输入的尝试数，其余项表示对应字段未报告的尝试数。`target_attempt_finished.usage` 不携带聚合 coverage；正在运行与终态未报告的区别仍由 attempt 状态表达。统计接口不新增覆盖字段或完整性标记。coverage 不替代 `observation_gap`，无法记录的 attempt 不计入已观察尝试总数；
- 查询从现存 attempt 记录派生已确认累计与覆盖信息，旧版保存的 `null` 汇总不遮蔽仍然存在的用量；无需改写旧事件或自动拆分历史 Interaction。SQLite 与 PostgreSQL 使用相同计量规则，Route Scheduling 与成本计算仍读取原始用量；
- 收到新的上游 usage 后立即更新持久化数值投影供查询；时间线与 SSE 在实际 `target_attempt_finished` 时显示合并结果，迟到事实以更高 sequence 的同 kind 终态修订承载，不新增独立 `usage_confirmed`、易失 usage 或 reset 协议。

请求记录的链路 Token 阈值按整个根 DAG（含子孙）的已确认净输入与输出累计，不加回缓存分项；用量活动图与输入/输出构成图采用相同口径，缓存仍独立展示，输入标签保持「输入」。尚在运行且没有任何 Target attempt 报告 usage 的 Model Turn，临时加入该轮未缓存输入估算，不再次扣除缓存，使大输入请求无需等待首轮响应结束即可显示。估算每轮只计一次，不随重试重复累计；任一 attempt 报告 usage（包括明确的零）或该轮结束后，停止使用该轮估算。真实合计低于阈值时，链路可能重新隐藏。列表、总数、分页与实时匹配采用同一规则，0 表示不过滤。

用量页展示「延迟与速度」图：首字延迟为左轴秒值和钢蓝实线，TPS 为右轴 tok/s 和钢蓝虚线，两轴独立线性缩放、从零起点自动扩展上限。用量页保留 6 小时、24 小时、3 天和 7 天，图与窗口汇总每 30 秒刷新，折线仍按一小时分桶。顶部原始窗口汇总兼作图例；悬停或通过方向键、Home/End 聚焦时间桶时读取同桶的两项值与单位。空桶和单项 `null` 各自断线，未知显示「—」，有效零输出显示 `0 tok/s`；不插值、不补零，不额外扩展首末数据点外的占位桶。说明按钮支持焦点和触摸，明确首字延迟包含 Thinking、TPS 包含首字等待。其他耗时展示保留。两种存储及 Server/Desktop 共用 Core 计量，无需新增列、迁移或历史回填。

输入估算不进入 Confirmed Upstream Usage、卡片数值、用量统计或计费。普通模型请求的筛选与路由调度共用同一估算：将消息 `items`、独立系统提示词 `instructions` 和工具定义 `tools` 一并计算 JSON 序列化字节数，除以 4 向上取整；工具说明与参数 schema 也属于输入，不能只按用户消息估算。它不是模型 tokenizer 的精确计数。估算随 `model_turn_started` 写入独立的 nullable 字段，已有记录不重算，不从截断的输入预览或 Debug 内容回填。

客户端响应的 Run 用量账本只合并实际执行的隐藏轮次。没有隐藏轮次时，保留终态响应已有的数值与 known 标志，包括明确报告的零；空账本不得把已知用量降级为未知。该规则不把未知值补零；客户端 wire 仍保留协议规定的总输入，管理面另在读取边界计算净输入。

Gemini `thoughtsTokenCount` 保留为 reasoning 子项，输出仍只包含一次该部分；终态之后的累计 usage 修订覆盖同一 attempt 的快照，不作为新增用量相加。标准 Gemini 缺失缓存字段仍为未知。Antigravity 仅对已确认完整的私有终态 usage 使用其 proto3 标量缺省零语义，缺失整个 usage 或中间帧不补零；事实来源和消息存在性边界见 [Antigravity 响应协议](../research/antigravity-oauth.md#响应)。这些归一化仅影响后续请求，不补写存量事件。

首内容超时在取消执行 future 前标记原因，未正常结束的 attempt 记录 `first_token_timeout`；`attempt_aborted` 仅作为没有明确结束原因的释放兜底。两者均不伪造 usage，也不改变原有超时配置、重试预算或调度策略，每个 attempt 仍只有一个终态。

### 3.4 Observation 不影响执行

Observation 写入、SSE、Debug 分段文件、容量统计或导出失败不得改变 Inference Run 的响应、重试、Target 选择、Client Output Commit 或 Generation Chain 提交。无法记录时产生显式 `observation_gap` 或把 Debug Trace 标成 `partial`；不得阻塞、取消或伪造业务结果。

### 3.5 凭据新增发现

凭据保护页从普通 `credential_mappings_created` 事件投影最近发现，不另建秘密目录或会话事实源。事件在映射实际新建提交后、可失败的替换前由 `RunObserver` 发出，载荷为 `discoveries: [{ rule_ids, source_types }]`，每个元素对应一个实际新建映射；不含秘密、指纹、可恢复引用、消息片段或完整 JSON 路径。来源类型为 `user_message`、`system_or_history`、`tool_arguments`、`tool_result`、`other_text`，来自本次提取和最终规则命中，不从历史正文补推。

映射预留与发现投递由同一独立任务持有，防止数据库已经提交、调用方尚未收到确认时取消导致遗漏。取消不等待此任务，也不执行后续替换、Provider 调用或发布；已启动的预留可以完成，并沿用未发布映射的既有保留期。

同一 Interaction 的多次发现合并，按最后一次新增事件时间倒序；后续复用、还原和状态更新不改变发现时间。请求结果单独读取现有 Interaction 状态，`interrupted` 的详情仍保留失败或取消的 Run。该投影不要求 Debug，也不等待客户端交付或 Generation Chain 成功；清理观察不删除保护映射，后续有效复用不会重新计数。

`GET /api/v1/reversible-redaction/discoveries` 经 `AdminService` 查询现有写者已处理的事件，采用有界 `limit` 与 `next_cursor` 翻页。返回 `items`、`next_cursor` 和 `observation_gap`；条目只含交互 ID、API Key 名称、发现时间、新增数量、规则与来源类型、请求状态及缺失标志。查询失败与空记录分别表达。迁移 `0036_credential_discovery_coverage` 把升级前保留的交互标为观察缺失，不扫描秘密库或历史补造发现。已归属交互的写入失败尽可能持久化 gap；尚不能落盘的准入或队列损失按 Run 保存易失的发生时间和代次，并用当前观察保留期判断是否仍可见。清理历史只移除本次实际删除的已知 Run 所属且代次未变的标记；活动记录、清理期间新发生的损失和无法确认归属的准入损失继续保留，直至当前保留期到期。该易失标记不承诺跨进程崩溃保留，Observation 仍是可丢失诊断投影。

## 4. 模块与 seam

### 普通准入队列与输入预览

普通准入在入队前从收到的 canonical 请求生成 `PreparedAdmission`：只保留有界 received-input 证据、溢出或指纹缺口标志、Generation 已确认事实及完整请求的 canonical fingerprint，不让 writer 等待 SQL 时持有完整原文请求。fingerprint 覆盖完整请求语义，不把有界尾窗当成完整请求；证据超限、缺失或无法无损重建时按既有规则记录 gap，不凭截断内容确认归属。

输入预览不作为上述准入证据，也不提前写入普通队列。活动请求先完成凭据保护并登记全部有效映射，成功后才生成和发布最多 4,096 Unicode 字符的受保护预览；保护失败或取消不能发布未经保护的正文。该内存优化不关闭普通 Observation、进程日志、Generation Chain 历史或 Debug 契约，不缩减 SQL 历史；Debug 原文捕获仍只在明确启用时遵循 §7 的独立边界。

### 原生压缩与保留尾部关联

目标契约按 [ADR-0053](../adr/0053-keep-one-interaction-across-generation-roots.md) 扩展：唯一、完整的保留尾部精确匹配在五分钟窗口内可以自动归入原 Interaction，即使本次没有当前工具结果。该行为对启用后准入的请求生效。

已确认的当前工具续接优先于 Generation parent 的常规观测分组：本次回传来源 Run 当前待完成工具调用的结果时，即使已有 Generation parent、同时夹带额外的 User 输入，也继续来源 Interaction，并将观测父节点指向来源 Run，不受尾部归并五分钟窗口限制。Generation 父边不变。历史回放中的旧工具结果不能作为当前续接证据；不能只在完整输入中找到相同工具 ID 就触发归并。

客户端可能省略 thinking-only 输出，再把上次收到的完整输入加一条 User 提醒重新提交。只有同 Principal、未过期的 `client_tool_result` 收据和完整收到输入的 canonical 严格前缀共同证明这种回放，且新增后缀仅为普通 User 输入时，才排除当前工具／pending-tool 归并优先级并跳过本 Run 的重复结果捕获。User 内容块中的 ToolResult 是新工具结果，不属于 User-only 后缀。该判定只影响 Observation；原有精确父节点、两秒快速续接和尾部规则继续适用，模型输入与 Generation 父边不变。

收到输入的证明包含 leading system/developer、媒体和原生控制的 canonical 语义；忽略范围严格沿用既有 canonical 规则，包括交付/graph 元数据与缓存指令。进程内使用准入时的完整输入摘要；缓存不可用时，由已保存 Generation 重建收到输入边界，不把本次输出混入证明。截断、缺失、过期或无法无损重建时不推断回放。相同输入的独立 sibling、结果值变化和已有输入变化仍保留自己的工具收据，不按工具 ID 全局消费或去重。

只有未满足当前工具续接条件且没有 Generation parent 时，才用保留尾部决定交互归属；先确认来源，再按以下规则分组：

- 匹配区间之后没有新的 User 输入，且满足时间窗口：归入来源 Interaction。
- 匹配区间之后有新的 User 输入，或已超出归并窗口：创建新的 Interaction，并在诊断树中连接来源 Interaction；两个交互分别汇总状态、用量和 Debug Bundle 范围。

时间窗口按本次请求入口接收时间减去被匹配来源 Run 完整交付给客户端的时间计算，差值须位于 `[0, 300000]` 毫秒，包含两端。任务开始时间、Interaction 的 `last_active_at`、writer 处理时间和数据库写入时间不参与计时；其他分支活动不得延长来源的归并资格。恰好五分钟可归并，多一毫秒则创建新 Interaction 并保留满足条件的诊断来源连接。

归并窗口不限制诊断来源连接。来源记录仍须在保留期内，匹配仍须完整且唯一；不能先按归并窗口过滤较旧候选，再把剩余候选宣称为唯一来源。诊断连接不建立 Generation Chain 执行父边。

`compaction_operation` 保存 standalone/inline、所属 Model Turn、来源、登记 ID、阶段、耗时与错误分类；所选 Target/Provider 沿用 Target attempt，usage 仅沿用每 attempt 一次的确认用量投影及 `target_attempt_finished.usage`。Standalone 是真实操作，不落空 Generation；回放旧 state 不再登记压缩操作。

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

收到输入的证明与已交付尾部共用 16 MiB 进程索引预算，并额外要求包含 leading 前缀的完整输入不超过 512 items/512 KiB；超限后缀仍可用于既有尾部诊断，但不能证明结果回放。输入证明只保留 canonical 摘要，不向普通 Observation 新增完整请求、签名或密文。

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
- `model_thinking`
- `platform_tool_started`
- `platform_tool_finished`
- `client_tool_handoff`
- `client_tool_result`
- `client_visible_content`
- `run_finished`
- `interaction_relinked`
- `observation_gap`
- `input_preview_recorded`：每个带有新增用户输入的 Run 至多记录一次，`payload.text` 保存完成凭据保护后的最多 4,096 字符输入预览。同一 Interaction 的追加输入也记录事件，但只有初始 Run 更新卡片的 `input_preview`；重复发布不覆盖已有正文。旧事件缺少 `text` 时不推断或补录输入。

没有已核验 Generation parent 时，完整请求中的历史 User 不自动视为新增：只有已选定的唯一续接来源或已观察父节点提供完整 received-input canonical 前缀证明，且当前请求仅追加 assistant/tool items、末尾为当前工具结果，才令新 Run 的 `has_new_user=false` 并抑制输入预览。证明包含 system/developer 前缀与私有控制，不凭预览文字相同去重；追加同文 User、前缀变化、证据缺失或超过既有窗口预算时均保留输入。重启可通过保留的 Generation 证据恢复前缀，无法恢复时保守记录。该判定只影响后续观察事件，不改变分组、模型输入或 Generation 父关系，不删除、改写或隐藏旧重复事件。

`platform_tool_started.input` 和 `client_tool_handoff.input` 保存工具输入，`platform_tool_finished.content` 保存平台工具返回；输入为可解析的 JSON 时保留其类型，否则保留原始参数字符串。旧事件缺少这些可选字段时表示未采集，字段值为 `null` 则表示实际采集到 JSON null。`client_tool_result` 保存收到的客户端返回及其调用 ID、错误标记，兼容显式 `tool_result` 块和 `role=tool` 消息；只采集收到的 canonical 窗口，不从恢复后的模型历史重新提取。客户端返回先留在内存，凭据映射注册完成后与输入预览共用发布边界，没有新用户文本的工具续跑也会发布。

客户端工具结果只从已核验 Generation Chain 父节点的本次 `client_delta` 捕获，不再复制父节点完整窗口；没有已核验父节点时不从父历史推断结果。收到的本次新增结果再按批次查询当前 Run 及明确 `parent_run_id` 祖先，只使用同一 Principal、仍在保留期内的调用证据。最近一次 `client_tool_handoff` 确定调用边界；相同 ID 的新 handoff 是新调用。只有与该调用最近结果的脱敏后正文、`is_error` 均相同时才跳过重复写入。正文变化、错误状态变化、分支结果与新调用保留；没有 handoff 证据，或最近结果正文缺失、为 null 时，不跨越该不确定边界去重。比较状态只存在于当前批次，不另存正文副本或原始凭据摘要。工具去重本身不回写或删除既有结果；升级时事件编码与生命周期合并另按 ADR-0077 无损转换。

生命周期事件把 `usage_confirmed` 合并到 `target_attempt_finished.usage`（`ConfirmedUsage | null`），把 `delivery_finished` 合并到 `run_finished.delivery`（`{status, reason, completed_at} | null`）；失败、中断前已收到的用量与交付事实也保留，早到事实先更新投影，不能因尚未终态而丢弃。`delivery_completed_at` 保存于 Run 投影。`generation_associated`、`client_output_committed` 只更新投影，不再落独立事件；启动恢复以 `run_state_changed` 维持 SSE 唤醒。

推理尝试已有失败诊断时，`target_attempt_finished.error` 保存可选的 `FailureDiagnostic`（`source`、`code`、`message`、`status_code`、`upstream_code`）。它沿用普通 Observation 的结构化凭据与已知秘密脱敏，随事件查询和 Bundle 导出；即使后续重试成功，该尝试的安全原因摘要也不会被丢弃。既有 `error_code` 与状态语义不变，成功尝试不附带 `error`，旧记录与没有诊断的恢复终态允许缺省；无需数据库迁移，也不为旧记录补造原因。恢复成功的内部失败仍不成为最终 Failed Request。

`model_thinking_delta` 与 `client_visible_content_delta` 仅用于易失实时通道，不是持久化 kind。`model_thinking_delta` 只接收上游可读 thinking / reasoning summary 文本，并以 Model Turn、Target attempt、Canonical Item 与项内 part 隔离增量脱敏和汇聚状态。签名、密文、obfuscation 和不透明快照不作为普通思考正文。`client_visible_content_delta` 仍只接收 Client Projection 已交付的可见内容。

普通诊断内容按 Canonical Item 收口持久化为 `model_thinking` 或 `client_visible_content`：每个 item 一行，payload 保存 `text`、`parts`、`block_id`、`item` 与 `complete`，思考内容另带 Model Turn/attempt 关联。同一 item 的流式碎片汇聚为一项，保留项内 part 的边界和顺序；具有独立身份的 item 即使类型相同也不得合并。正常结束保存完整 item；可处理的失败或取消保存已实际收到的内容并标为未完成，不补造未收到的尾部。item 首次落盘只发生在收口时，不周期性持久化中间快照；进程突然崩溃可以丢失整个尚未落盘的 item。实时与持久内容共享 scope 与真实源 item ordinal：可见内容在 Run 内、thinking 在 attempt 内稳定编号，迟到 provider ID 不改变 block_id。WebSocket 仅累计已成功发送并确认的客户端帧，失败或断线收口为 `complete=false`，不补入未发送正文。

旧封块行不能推断真实 Canonical Item 边界：转换移除随机 block_id 与合成 item，原元数据收进 `legacy_text`，读者按原 scope 与有序 parts 连续呈现，不插入虚构空行。旧提交信号行删除，admission 中的 `client_output_committed_sequence` 与 `legacy_lifecycle` 保留原水位与事实来源；Bundle 仅应用 sequence 不超过 through-sequence 的事实，终态后的迟到提交以更高 sequence 的 `run_finished` 修订承载并保留原完成时间。

物理存储可以为容量、压缩或文件布局分块，但物理块不得成为新的语义 item，也不得改变 item 身份或 part 边界；最终 schema 由迁移与生成的参考 SQL 定义。现有队列容量、背压与 gap 行为继续成立，容量边界不得以时间或字节阈值强制把一个 Canonical Item 持久化成多个内容项。

工具结果批次先对查询 ID 去重，再沿既有祖先与 handoff 边界比较。批内比较引用已接收事件的位置，不再次复制大正文；不按跨交互的相同 payload 全局去重，缺失调用证据、正文变化及 null 边界仍保留。

未收口内容作为选中 Interaction 的累计易失快照发布并替换显示，不带 SSE ID、不推进持久 cursor；界面明确未保存状态。第一段与完成、失败、取消边界立即处理，中间修订按固定首次截止的最多 100ms 发布窗口合并，只发布变化 block 的最新修订；持续输入不延后截止。同一 block 尚未消费的旧修订可被新修订替换，不能让累计全文无限排队或拖累持久通知。发送用的全文构造和编码只在发布时进行，不在每个原始 delta 上重复执行。下游转发与推理不等待观察发布、诊断 item 收口或落盘，也不因有无管理订阅改变记录行为。持久化失败、live gap 与进程崩溃丢失分别表达，不能把易失预览截断误报成已落盘历史丢失。

思考和工具内容不进入 `visible_tail`，沿用普通 Observation 既有凭据脱敏及 `log_retention_days`，不受 Debug 开关控制；业务敏感内容仍可能保留。普通事件不保存完整 canonical request/response。

### 5.2 Debug 原始 Wire 捕获

Debug Trace 只记录四个方向的原始应用协议级收发：Connect Client → Stravia、Stravia → upstream、upstream → Stravia、Stravia → Connect Client。上游 HTTP 在 reqwest 实际请求与响应边界捕获；WebSocket 与客户端方向在各自实际传输边界捕获 handshake 元数据和应用 message。它不记录 TLS、TCP、HTTP/2 frame 或操作系统 packet，也不采集 decoded/restored/effective/canonical request、canonical content/terminal response、Hook 前后、Platform Tool 中间态、Client Projection、delivery terminal 或 stage timing 等语义阶段。

每条 Wire 记录保留必要的关联元数据，包括适用的 Interaction、Run、Model Turn、Target attempt、方向、协议、transport、顺序与 UTC 时间。既有 Target attempt 身份与生命周期、usage、工具事件、失败与取消继续由普通 Observation 持久化并随 Bundle 导出，不把旧 `target_selected` 迁入普通 Observation，也不复制到 Debug Trace。Debug 原始字节不等待 Canonical Item 收口，普通 Observation 的内容收口也不阻塞 wire 捕获或下游转发。

Wire 记录直接写入 Debug Trace 队列，不逐条写入普通 `observation_events`，也不占用普通事件队列。普通生命周期、item 内容与工具结果持久化并驱动 SSE；Trace manifest 仅写本地文件，不产生普通事件。Trace 使用下一持久观察边界作为水位，同一 Trace 中排队记录的水位保持非递减；manifest 按既有维护周期或显式生命周期边界持久化。Interaction 导出票据排空目标 Interaction 的 Trace 后固定截止水位；既有 ZIP 截止水位不能包含之后的新捕获。

原始 body chunk、SSE 字节与 WebSocket 应用 message 在传输边界观察到后即可排队，不以完整 JSON、SSE、NDJSON、Connect message 或 Canonical Item 收口作为记录前提。只有 HTTP `Authorization` header 的值在入队前替换，媒体与其他 header、URL、body 和 message 内容原样保留；编码进分段文件与物理批处理不得改变可恢复的字节、方向和顺序。

Trace 继续使用既有有界队列与写入批次；队列、捕获缓冲或存储失败不等待、不反压推理，Trace 标记 `partial` 或 gap。manifest 的字节及事件水位只公布已确认可读取的数据；分段切换、snapshot、finish 与关闭保持显式 flush，不能因后续 finish 成功而把缺失捕获伪装成完整。

### 5.3 顺序与 SSE cursor

Observation writer 为持久化事件分配递增 `event_sequence`；持久事件 allocator 不因清历史或到期回收而倒退，已提交事件 ID 不重用。事件及受影响摘要在同一数据库事务内提交后才广播；SSE event ID 等于 sequence。此单调分配契约不同于查询的 `snapshot_sequence`：后者只描述同一读取事务内仍保留的已提交事实，跨清理或 reset 重建不保证单调。

批次先过滤不持久化的事件并完成 payload 编码，再用一次数据库调用为实际事件取号；空批次不打开事务。SQLite 使用带整数上界检查的 `UPDATE ... RETURNING` 预留连续范围，单事件也复用同一取号路径，事务回滚不消耗序号。PostgreSQL 在同一查询中逐次调用 `nextval`，使用实际返回的每个值，不假设并发写者之间的序号连续；回滚仍可留下序号空隙。事件、状态投影、终态顺序及提交后广播规则不变，不调整 writer flush 周期。

全局通知的 SSE event 名保持 `observation`，ID 等于持久 sequence，但正文为轻量 `ObservationChange`，不是完整持久事件。先分批重放已提交变化（每批最多 512 条），再接续实时通知；完整 payload 仍由详情事件接口按需读取。通知进度、成功应用的视图水位与正文 revision 分别维护，过滤造成的 sequence 空洞不是持久丢包证明。过旧、超前、清理后失效或无法安全恢复的游标返回 `reset_required`，消费者须重建权威 forest、详情与选中正文，而非把空响应当作恢复成功。

选中作用域的 `live_content`、`live_snapshot`、`live_gap` 与 `live_finished` 不带 SSE ID，不推进持久 cursor。建立作用域先取得当前累计快照（包括空快照），再无遗漏地接续更新；`live_finished` 表达 Run 收口并触发已收到正文立即追平，不替代持久终态事实。重连、reset 或断线时替换或清除旧易失状态，不按文本猜测去重，也不重播历史动画。易失预览不承诺重启恢复。

作用域注册与当前快照读取在同一原子边界完成；正常 delta 更新当前累计镜像，不为每次输入克隆完整正文。收口先立即发布待处理正文，再退役 block 并发送 `live_finished`，保留实际顺序。作用域 mailbox 按 block latest-wins，容量溢出显式发送 `live_gap`，不把易失背压变成全局持久 reset。清历史先清作用域待发正文，再发送空快照与 `history_invalidated` live gap；清 Debug 或历史失效同时要求全局视图重建，不能由重连恢复已删除数据。

SQLite 的 Run admission 使用 `BEGIN IMMEDIATE`，在读取父 Run 状态前取得写锁，使父分支中断与子 Interaction 入库保持原子性，避免并发写入导致读事务升级失败。

同一 Gateway 内，历史与普通 Observation 的 SQLite 写入先取得共享异步写锁，再取得连接和事务；锁覆盖实际写入至提交，不覆盖独立只读查询或 manifest/Trace 文件操作。清理中的 owner 快照读事务在文件 tombstone 操作前提交，不持有 SQLite 写锁等待文件系统。PostgreSQL 保持原数据库并发与锁协议。

页面先查询快照并取得 `snapshot_sequence`，再从该 sequence 订阅，避免查询与订阅之间丢事件。重连携带最后确认的 sequence：

- cursor 仍在保留范围内：补发缺失事件；
- cursor 已清理：发送 `reset_required`，前端重新查询当前时间页；只按保留窗口下界判断，不把删除或合并造成的内部稀疏 sequence 当作失效 cursor，范围内正常重放；
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

保存 sequence、关联 ID、kind、occurred_at 和普通安全 payload。payload 为共用 storage codec 编码的二进制（SQLite BLOB / PostgreSQL BYTEA）；SQL 过滤字段 `tool_id`、`operation_id` 提升为列。Interaction/Run/sequence 与全局 sequence 索引保留；移除未使用的 event expiry 索引，rejection 索引仅覆盖非空行，工具索引使用 `(run_id, tool_id, sequence DESC)` 部分索引。二进制布局与升级顺序见 [ADR-0076](../adr/0076-deduplicate-turn-chain-items-and-share-binary-storage-codec.md)。

#### `rejected_request_observations`

保存无法形成 Inference Run 的请求时间、method、脱敏 path、ingress 协议、失败阶段、稳定错误 code、HTTP status、Debug Trace manifest 关联和过期时间。Migration 0045 起额外保存可空的 `started_at`、`duration_ms`、`failure_json` 与来源快照 `request_model`、`api_key_id`、`api_key_name`，供失败请求投影使用；这些列全部可空、不回填历史，也不引入新的 Principal 外键。它不保存 Principal，也不伪造 Interaction ID。

#### Debug manifest（文件，不是关系表）

`debug_trace_manifests` 已由迁移 0008 删除。状态存放在 `diagnostics/observation-debug/<trace_id>/manifest.json`，以临时文件加 rename 原子替换，启动扫描建立进程内 `DebugTraceIndex`，writer 增量更新。详情、失败请求列表和 Bundle 通过索引读取状态，API 结构不变；多实例只在拥有 Trace 文件的实例上可见。Run 终态提交并广播之前先写入 manifest 终态，不再持久化或广播 `trace_manifest_updated`。Rejected Request 保留 `debug_enabled` 准入快照，不再保存冗余 `debug_status` 列。运行时逐文件扫描隔离损坏或不可读 manifest，仅警告并跳过，不覆盖坏文件，不影响健康 Trace 查询与新捕获；升级导出则严格核对已有文件与数据库权威事实，匹配后才允许删表。升级的有界并发、临时查询索引和中断续迁契约见 [ADR-0077](../adr/0077-slim-interaction-observation-and-file-debug-manifests.md)。

### 6.2 Debug 分段文件

WebSocket 捕获 handshake 元数据与应用 message；Ping/Pong 控制帧的既有限制保留，只记录事件类型、方向与时间，不保存控制帧载荷，并明确标记策略性省略。该省略不把 Trace 标为 partial；历史 Trace 不回填或改写。

实际收到的 Close 在复用连接的提前失败判定之前捕获一次，保留 code、reason 及所属 attempt。EOF、接收异常、本地取消与连接到期没有收到 Close 时，不得补造入站帧；原因由普通 Observation 表达。本地主动发出的 Close 只归入上游请求方向，不冒充上游响应。

payload 写入 `GatewayConfig.data_dir` 下由 Observation 模块拥有的目录，建议布局：

```text
diagnostics/observation-debug/
└── <trace-id>/
    ├── manifest.json
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

宿主网络传输错误的原因摘要保留外层错误及其 `Error::source()` 原因链，避免 WebSocket 包装错误的通用类别名遮蔽异常关闭、协议帧或 I/O 失败等底层原因。完整摘要沿用普通 Observation 的既有脱敏规则后进入日志、推理尝试终态及最终失败请求记录；不改变 Wire Debug 的捕获与脱敏边界，也不能恢复旧记录中已丢失的原因。

文件路径只接受模块生成的 opaque trace ID 与固定文件名，所有导出读取都在 canonicalized root 内，防止 path traversal。

### 6.3 容量与失败

- Trace 落盘不设 Run 级或全局容量上限；`retained_bytes` 只统计实际落盘字节；
- 请求体、传输帧或消息的捕获缓冲仍有既定内存上限，超限只截断对应方向的捕获，不等待完整应用消息后才开始记录；
- 队列溢出不等待 writer，未能入队的捕获内容使 Trace 标记 `partial`；存储错误停止对应 Trace 写入，Inference Run 继续；
- manifest 使用既有捕获容量超限、捕获丢失或截断、分段写入损坏、writer overflow、storage error 与 debug data cleared 原因表达 `partial`；已完整捕获但协议内容非法不属于 capture partial，策略允许的 Ping/Pong payload 省略也不算失败；
- 不完整原因只出现在 Interaction 详情和诊断包中，请求记录页不显示常驻告警。

### 6.4 保留与清理

Observation、Rejected Request、Debug manifest 与 Trace 文件跟随 `log_retention_days`；默认 7 天。

清理不再依赖数据库 manifest 或 tombstone：按 manifest 的 `expires_at` 回收目录；删除先 rename 为 `.deleting-*`，随后幂等删除，启动补完中断的删除并回收无 manifest 的孤儿目录。关系数据清理后另删 owner 已不存在的 Trace 目录。

“清除历史记录”只删除非活动 Interaction 与 Rejected Request；`running` 和 `waiting_client` 保留，并在结果中报告跳过数量。清理不取消 Inference Run。

“清除 Debug 数据”关闭活动 writer 后删除全部受管目录并清空进程内索引。活动 Run 的 Trace 停止并标记 `debug_data_cleared` partial；请求记录、Rejected Request 与 Debug 开关状态不变，后续准入的 Run 继续正常捕获。

## 7. Debug 开关与脱敏

### 7.1 开关

- 唯一 Debug 开关位于设置页「诊断」，同时控制性能指标采集与原始 Wire 捕获；请求记录页不再提供启停或清除 Debug 数据入口，仍保留已有 Trace 查看及 Bundle 导出。
- 关闭立即停止新的性能样本；Wire 捕获继续遵循下述准入快照契约，不中断在途 Run，不删除已有 Trace。普通 Observation 不受影响。
- 设置页可下载 Prometheus 文本性能快照（`GET /api/v1/performance/metrics`）及 Chrome Trace JSON 时间线（`GET /api/v1/performance/timeline`）。两者均要求管理员鉴权，以禁止缓存的附件返回，不另开监听端口。关闭 Debug 后仍能下载本进程已有数据；进程重启后不保留。清除 Debug 数据仍仅清除 Wire Trace，不重置性能指标或时间线。
- Desktop 下载沿用宿主默认保存位置；设置页等待原生下载完成后提示实际文件路径，取消或失败不报告成功。Web 下载仍交由浏览器处理，管理员鉴权与附件格式不变。
- 性能指标包含静态命名操作耗时、按操作归因的 SQLx 查询与成功获取连接的耗时分布、观察写入队列深度、历史物化缓存记账字节及 hit/miss 次数，以及每五秒采样的宿主进程 RSS／CPU。CPU 在开启后的首个采样点不输出缺少差分基线的读数，多核 CPU 百分比可以超过 100%。不计入浏览器子进程或远端数据库内存；缓存记账容量不等于实际堆占用。
- SQLx 指标只读取数值，不导出 SQL 文本、参数、请求正文或动态身份标签。查询事件不能说明成功／失败或所属连接池；连接获取耗时包含建连和健康检查，超时失败不在成功样本中。关闭再开启期间尚未完成的 span 不写入新启用周期。
- 开关是当前 Gateway 进程的原子运行态；默认关闭，重启后关闭；
- 每次开启都显示确认：除 HTTP `Authorization` header 值外，Trace 会原样保存其他 header、URL、body、提示词、工具参数、业务数据与媒体；关闭后已有 Trace 仍按保留期存在；
- 开启状态下提供「清除 Debug 数据」操作，删除全部已保留 Trace，不影响开关与请求记录；
- 每个 Inference Run 在准入时独立快照；同一 Interaction 可以完整、部分或完全没有 Trace；
- Rejected Request 在 ingress 时快照，并可生成只含 client request/platform error response 的独立 Trace；没有上游方向不算缺失；
- Wire 捕获的关闭只影响之后准入的 Run，不删除已有数据。

#### 7.1.1 统一性能 span 与时间线

命名操作统一使用 `tracing` 的 `stravia::perf` target。`PerformanceLayer` 从同一个 span 生成耗时直方图与有界时间线，不再维护独立手写 Timer。覆盖请求根、路由选择、Model Turn、Vendor、Target Attempt／首 token、Agent／工具、客户端交付、历史物化与观察写入。

常规异步函数使用 `#[tracing::instrument(target = "stravia::perf", name = "router.select", skip_all, fields(status))]` 这类静态声明；局部 future、独立任务通过 `.instrument(span)` 传播上下文。不得跨 `.await` 保留 `span.enter()` guard。响应体、WebSocket 与后台 producer 必须持有对应 span，直到真实生命周期结束；有非性能中间 span 时，仍关联最近的性能祖先。

Layer 只保留静态操作名、内部生成的 span／parent ID、时间、白名单状态，以及 `node_count`、`reference_count`、`candidate_count`、`event_count` 四个非负整数工作量字段；忽略其他 span 字段，不使用 `ret`／`err` 自动记录业务值。默认终态为 `closed`，仅表示 span 生命周期结束；业务代码在已知结果时显式记录 `completed`、`error`、`cancelled` 或 `abandoned`。不能把函数返回、future 被丢弃或父 span 关闭自动解释为成功。

时间线默认保留 1,024 个活动 span 与 10,000 个结束记录，可用 `STRAVIA_PERF_TRACE_ACTIVE_CAPACITY` 与 `STRAVIA_PERF_TRACE_COMPLETED_CAPACITY` 环境变量在进程启动时调整；活动区满时拒收新 span（相应耗时也不入直方图），结束区满时淘汰最旧记录。JSON `metadata.capacity`、`dropped_active`、`dropped_completed`、`incomplete` 与 `incomplete_total` 显式描述容量、丢弃数、当前未完成记录数与累计未完成数，不承诺完整请求树。关闭 Debug 将活动记录冻结为未完成记录；随后关闭的旧 span 不补写终态或直方图，重新开启只接收新周期的数据。

导出使用标准 Chrome Trace 事件：已结束 span 为 `X`，未结束或被 Debug 关闭截断的 span 只有 `B`，不伪造结束时间；可见父子间附带 flow。每个 span 使用合成 track，`tid` 不是操作系统线程。可导入 Perfetto 或 Chrome trace viewer 查看墙钟时长与父子关联；`active_us` 仅表示 span 被 enter 的区间并集（重入或并发 enter 不重复累加），不是 CPU 时间，未 enter 的 span 不输出该值。该时间线不是 CPU／堆 profiler；进程 CPU／RSS 仍是独立全局采样，不能归因到单个 span。

#### 7.1.2 查询次数与调用来源

`stravia_sql_query_duration_seconds` 和 `stravia_sql_pool_acquire_duration_seconds` 使用静态 `operation` 标签，取事件所属的最近一层性能 span；没有可归因的性能上下文时标记为 `unattributed`。不再输出无标签的并行总量，全部操作的 `_count` 求和即当前累计总次数。SQLite worker 传播调用侧 span，因此异步线程上的查询仍归属于发起它的操作；关联旧 Debug 周期 span 的延迟事件不会计入新周期。

这里的查询次数是 SQLx 查询日志事件数，不是 SELECT 次数、网络往返数或数据库引擎执行的全部语句数。读、写和失败执行都可能产生事件；SQLite 原生事务控制和连接健康检查不一定产生查询事件，多语句执行也不能据此拆成逐条语句。成功获取连接的 `_count` 单独计数，不应与查询次数相加。`stravia_operation_duration_seconds_count` 则是操作调用次数，操作可以执行零条或多条 SQL。

时间线中每个 span 的 `args` 可包含：

|字段|含义|
|---|---|
|`sql_query_count` / `sql_query_duration_us`|直属 SQLx 查询事件数及其累计耗时；没有查询事件时省略|
|`sql_pool_acquire_count` / `sql_pool_acquire_duration_us`|直属成功连接获取次数及其累计耗时；没有事件时省略|
|`node_count`|本次底层历史物化或内容恢复的节点数|
|`reference_count`|本次内容写入或恢复的引用条目数|
|`candidate_count`|前缀查询返回或对应发现阶段已统计的候选数；未完成统计时可能省略|
|`event_count`|`observation.writer.persist_events` 收到的事件批量大小，包含随后可能被过滤掉的事件|

SQL 事件只计入最近的 span，不重复累加到祖先。父 span 的直属计数为零不表示整条请求没有 SQL；分析请求总量需要沿子树汇总，且必须检查记录淘汰和不完整标记。SQL 耗时是事件耗时之和，并发时可超过 span 墙钟时长，不是 CPU 时间。不会为每条 SQL 创建时间线记录，也不输出 SQL 文本、参数、Principal、Run 或节点身份作为指标标签。

用于区分查询来源的主要操作：

|操作|用途|
|---|---|
|`generation_chain.parent.*`、`generation_chain.references.resolve_available`、`generation_chain.ancestor.*`|区分显式父节点、前缀发现、压缩来源、引用恢复及后台祖先访问|
|`generation_chain.history.load`|Generation Materialization Cache 未命中或 Item Reference 恢复所需的祖先历史读取，不能代表所有底层读链|
|`turn_chain.materialize` → `turn_chain.ancestor.select` / `turn_chain.content.restore`|覆盖包括绕过 Generation 缓存的直接物化，拆分祖先查询与共享内容恢复|
|`turn_chain.commit` → `turn_chain.content.put`|区分节点提交与按引用执行的内容 upsert／引用 insert|
|`turn_chain.prefix.lookup`|持久前缀候选查询|
|`observation.attribution.admit` / `observation.writer.admit_persist`|区分后台交互归属判定与准入记录落盘|
|`observation.writer.persist_events`、`persist_tail_source`、`filter_client_tool_results` 等|观察事件批次、工具尾迹与工具结果归属查询|
|`observation.manifest.*` / `observation.maintenance.*`|Debug manifest 写入、计数与保留期维护；不在空闲的每个 writer tick 上建立 span|

`stravia_generation_materialization_cache_access_total{result="hit"|"miss"}` 统计普通物化与含 Item Reference 的父节点恢复对 Generation Materialization Cache 的访问。物化对象和按 ingress 隔离的祖先引用目录现共用[统一派生缓存](architecture.md#统一派生缓存与-server-配置)：Memory / SQLite 使用 TinyUFO，PostgreSQL 使用 Redis。普通父节点恢复命中缓存后不再读链；含 Item Reference 时，若对应 ingress 的引用目录也命中则不再读链，否则仍需读取祖先历史构造目录。物化 hit 本身不证明目录 hit。直接调用底层 Turn Chain 物化不会经过该缓存，因此不能仅凭命中率推断底层读取次数。逻辑缓存记账不等于进程或 Redis 实际内存。

调查时，在同一进程、同一段复现操作前后各导出一次 metrics，按 `operation` 比较 `_count` 和 `_sum` 差值，并在操作完成后立即导出 timeline。先按查询次数判断高频来源，再按耗时判断慢路径；用工作量字段区分大批次与过多小批次，用父子关系识别重复物化。指标是累计值，关闭或清除 Debug 数据不会归零；有界 timeline 与累计 metrics 也不覆盖相同时间窗口，不能直接相除推导每请求查询量。

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

#### 8.1.1 导出职责与事实读取

`interaction_observation::bundle::BundleExport` 拥有完整导出过程：先等待 Interaction writer 屏障，再固定水位、读取导出事实、派生摘要、获取 Trace 物理前缀、判断完整性、签发票据并流式生成 ZIP。`InteractionObservation::issue_bundle_ticket` 与 `consume_bundle_ticket` 保留原有接口，只委托给该模块；下载阶段使用票据保存的快照，不重新查询当前状态。

Storage 的专用读取只返回根身份、截止水位前准入的 Run、按 sequence 排序的管理事件和所属 Trace manifest；不构造 `InteractionDetail`，不加载根下其他 Interaction，也不派生导出状态、用量或输出预览。普通 detail、forest 和 SSE 继续使用各自既有读取路径。SQLite 与 PostgreSQL 遵循同一读取契约。

Bundle 的状态、用量与输出预览在同一轮事件消费中派生，工具交付分支复用既有 `grouping` 证据规则。每个 Target Attempt 最多计入一次用量：迟到终态修订仅替换新报告的字段，未报告字段保留此前已确认值，显式零值仍有效；不同 attempt 的用量才相加。事件中的管理用量已经完成 cache-read 拆分，导出不得再次扣除。未完成的后台活动、尚未返回的工具分支与 observation gap 不得被后续实时状态掩盖。

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
POST   /api/v1/observations/interactions/changes
GET    /api/v1/observations/interactions/{id}
GET    /api/v1/observations/interactions/{id}/events
GET    /api/v1/observations/interactions/{id}/live
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
- `live_window`：ForestQuery 的可选布尔值，默认 `false`。实时 forest/changes 使用 `true` 时仍校验显式边界及跨度，但不因 `last_active_at >= end_at` 拒绝新活动，避免时钟差或查询在途造成实时遗漏；历史和自定义固定窗口仍遵守上界。此例外不改变失败列表或拒绝请求的固定时间范围语义。
- `anchor_at` / `window_index`：仅为现有 API 调用者保留的旧窗口参数；未提供显式边界时，0 为下界固定在 `anchor-24h`、无上界的实时页，后续历史页按 24 小时分段。WebUI 始终发送显式边界，包括实时预设；
- `cursor` / `limit`：同一时间页内按根链游标分批加载；
- `provider`、`model`、`api_key`、`status`：匹配任一 Interaction/Run 后返回完整根 DAG；
- 每个节点带 `matched`，前端对非命中节点降噪而不删除；节点另带 `failed_request` 与 `client_output_delivered` 聚合：前者表示该交互含至少一条按「失败的请求」口径的失败 Run（判定谓词与失败列表一致），后者表示任一 Run 已向客户端提交过输出（首个客户端可见字节）。两者是事实字段，不改变 forest 返回的完整性。

响应同时给出 window bounds、根链总数、next cursor 与 snapshot event sequence。根链按其最新 Interaction 的 `last_active_at` 归入且只归入一个时间页；返回时补全该根 DAG 在保留期内的全部 Interaction。`last_active_at` 只由真实请求准入、请求事件与完成活动推进；断连判定、等待超时、重启恢复、残留观察收口，以及新请求对旧 Run 的打断或接替，不推进旧 Run 的活动时间。Interaction 从所属 Run 聚合活动时间；状态判定事件仍保留实际 `occurred_at` 与递增 sequence，不能因较晚发现中断而把旧链重新归入最近时间窗。旧版本已写入的错误活动时间不自动回填。

SSE 通过普通 `fetch` 携带 Admin Bearer header，并由 `eventsource-parser` 解析；Admin token 不进入 query string。

全局 `GET /api/v1/observations/events?after=<sequence>` 只发送 `ObservationChange { sequence, occurred_at, interaction_id?, root_id?, run_id?, rejection_id?, kind, boundary }` 与显式恢复信号，不发送 `live_*`、完整 payload、正文或工具参数/结果。`root_id` 用于发现尚未加载的新匹配链路，`boundary` 用于首尾立即刷新。通知不是新的事实存储；生命周期、usage、delivery、工具与 Canonical Item 正文仍可由持久时间线读取。选中正文独立使用 `GET /api/v1/observations/interactions/{id}/live`，仅发送该 Interaction 所属 Run 的可见输出、可读 Thinking 与缺口/收口信号；`live_finished` 为 `{ interaction_id, run_id }`。两条订阅独立存续，未选详情或关闭检查器时不订阅完整正文；切换先取消旧作用域，并以 selection epoch 拒绝旧快照和消息。画布选中输出复用此订阅，不增加 HTTP 或 SSE；Thinking 不充当客户端输出，未交付的暂存内容不能提前显示。

`POST /api/v1/observations/interactions/changes` 接收 JSON：

```text
RootChangesQuery {
  filters: ForestQuery,
  roots: [{
    root_id, after_sequence,
    known_interactions: [{ id, last_event_sequence, matched, debug_status }]
  }]
}
RootChangesPage {
  snapshot_sequence, root_total, reset_required,
  changes: [{
    root_id, last_active_at, interactions: InteractionSummary[],
    removed_interaction_ids: string[],
    removal_reason: null | "deleted" | "filter" | "window"
  }]
}
```

初次加载和必要恢复仍使用完整 forest；常规更新按 root 合并查询，只返回变化节点和必要关系信息，不重发未变化兄弟节点。未知 root 使用空 `known_interactions`，返回必要完整上下文；新节点与必要祖先可首次返回。基线还包含 `matched` 和 `debug_status`，整根筛选结果或 Debug 状态改变时返回所有受影响摘要，即使其直接事件水位未变。移除结果区分物理删除、筛选失效与时间窗变化；历史 root 后续活动仍遵循 §10.3 的迁出提示，不机械删除已加载历史。`root_total` 使用当前筛选和时间窗的真实总数，keyset 分页与深链 reveal 不变。基线无效或不能证明差量完整时设置 `reset_required`，不伪装为空变化。摘要读取和快照水位继续遵守现有提交并发约束，不能用提前读取的全局 sequence 声称覆盖尚未应用的投影。

移除原因按 `deleted > filter > window` 判定。`deleted` 或 `filter` 的 root 移除列出所有已知成员；已知历史 root 仅因活动迁出窗口时，`window` 同时返回变化摘要（包括终态更新），`removed_interaction_ids` 只列物理缺失成员，不把全部已知成员当作删除。筛选失效优先于历史迁出保留，不能保留已不命中的历史 root。未知且在窗口外的 root 不返回摘要，不能借恢复上下文扩大成员范围。

负数或超前基线、已知成员水位超过所声明基线、基线早于保留事件范围均要求 reset。root changes 使用 SQLite 读事务或 PostgreSQL 只读 `REPEATABLE READ` 事务，使 roots、total、节点、matched、Run Debug flags、关联事件与 sequence 上下界来自同一数据库快照；正常并发写入不触发 reset。本地文件 `DebugTraceIndex` 仍表达当前实例文件事实，不声称与数据库共享事务快照。root 不存在对应 `deleted`，root 不再命中对应 `filter`，活动时间离开所选窗口对应 `window`；是否保留已加载历史画布由 Workspace 的既有迁出语义决定，不由后端移除原因自行改写。

forest、root changes 与详情摘要共用上述一致快照读取。其 `snapshot_sequence` 取同一快照内仍保留的已提交 `observation_events` 的最大 sequence，空历史为 0；不使用 SQLite 分配计数器或 PostgreSQL 非事务 `last_value` 代替已提交水位。清除最高事件使旧基线失效并要求一次权威重建，随后以当前已提交最大值建立新基线，不能持续陷入 reset。内部订阅分配计数器与查询快照水位用途不同；稀疏事件 ID 本身不是缺口。

全局 SSE 的 `reset_required.snapshot_sequence` 仍表示订阅分配高水位，不保证等于重建查询的已提交水位。清历史保留活动请求，因此共享实例的 forest 也不保证变为 0。恢复必须读取权威查询快照，而不能把 SSE reset 字段当作已应用视图水位；后续真实事件的 ID 仍严格超过清理前已分配 ID。

清历史、到期回收或 reset 后的 fresh snapshot 可以低于旧视图水位，全部删除时可以为 0；不得以跨重建的 `max(old, fresh)` 保留幽灵节点。PostgreSQL `last_value` 可包含尚未提交的分配，绝不能作为视图已覆盖水位。前端以新 view epoch 明确重建，拒绝旧 epoch 的在途结果，并接受当前权威快照的较低水位；普通同 epoch 增量仍遵守已应用基线，不能把迟到旧响应当作合法重建。

forest、root changes 与详情摘要共用批量 Run Debug flags 读取，并按 Interaction 分组读取本实例 `DebugTraceIndex`。空 Run 集合或全部未启用 Debug 为 `none`；全部 Run 启用且全部对应 Trace 完整为 `complete`；其余为 `partial`。按数据库参数限制分块，SQL 随必要批次数而非逐节点增长；选中摘要复用结果。Trace 索引仍是当前文件事实，不引入共享数据库状态或固定 TTL 缓存；终态与清 Debug 后读取不得保留陈旧 `complete`。认证、权限、脱敏、Wire Capture 与 Bundle 票据及 ZIP 结构保持不变。

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

页面 header 包含实时状态、时间预设、精确日期时间范围、全屏切换、筛选和“清除历史记录”。Debug 开关及“清除 Debug 数据”统一位于设置页「诊断」。全屏保留当前筛选、选中节点及检查器，支持工具栏退出和 Esc 退出。普通 CSV 导出删除。Debug Bundle 按选中的 Interaction/Rejected Request 提供。

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

选中卡片的输出保持有界尾窗，不把详情累计全文每帧复制给画布。尾窗因追加而滑动时，若实际保留后缀仍重叠，继续同轮逐字追赶且不重置 300ms 截止；非追加替换或无重叠直接同步，不能把正常尾窗裁剪误判为替换后瞬间显示全部新尾部。

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

`ObservationWorkspaceController`（`frontend/stravia-webui/src/lib/observation-workspace.ts`）拥有通知、forest、selection、range、failure、cursor 和正文编排。常规摘要更新使用 §9 的 root changes；公开的单节点 `/summary` 接口及其客户端路径删除，不保留兼容 alias。选中详情与失败请求跳转通过 Interaction detail 获取摘要和 root 上下文，并按既有 deep-link reveal 规则定位画布；内部 store summary 只作为详情组成，不是独立管理资源。按 root 汇集通知，最多等待 100ms，截止以第一条待处理变化固定；首段及终态立即触发，已有在途请求不因此无限并发。同一 root 仅一个在途刷新，请求期间的新变化保留，响应只清除实际水位覆盖的待处理变化，未覆盖的继续补拉。

选中详情初次加载有界详情，后续事件按持久 sequence 合并增量补齐，分页固定 `through_sequence` 上界、去重并升序；失败列表使用独立单一待刷新状态合并终态通知，不能把每个 `run_finished` 都判为失败。通知消费不等待每条 HTTP 请求，失败页不逐事件刷新隐藏画布，切回恢复正确视图。查询失败保留可用数据、待处理变化与可见错误/恢复操作；已收到通知游标不能冒充成功应用水位。切换 selection、range、filter、tab 或关闭详情后，旧响应不得污染新上下文。向上历史分页保持滚动锚点，未变化 Run、消息和节点沿用原引用。易失正文仅更新选中预览，不触发 HTTP。Forest、changes 与 detail 共用有界批量关联事件读取，保持 sequence 顺序与快照上界。点击下载不沿用页面旧截止序号，由服务端完成目标屏障后固定票据快照。

reset 恢复开启新的 view epoch，取消或拒绝旧上下文结果；在权威重建成功前保留当前可用旧数据，并明确未刷新或恢复中的状态，不提前清成看似成功的空视图。恢复失败显示真实错误和显式 Retry，保留恢复需求；成功后以 fresh forest、详情与选中正文替换旧基线，即使其 `snapshot_sequence` 更低或为 0，也不得重新合入已删除旧节点。

#### 10.2.1 通信优化配对测量

测量入口为 `tests/common/measure_observation_http.py` 与 `frontend/stravia-webui/scripts/measure-observation-browser.ts`。使用隔离 SQLite、真实 default-features release 服务、实际 WebUI 和 Chromium，不拦截观测 API。原始版本为 `d04c6a55a0709ca5540b42164173ec356f3b467e`，独立原始 worktree 构建的服务 SHA-256 为 `fc8723672c74f6ce7eeb19c944cf92be627bb4752f94deb1eb77dfb48a62a06c`；最终功能优化工作树服务 SHA-256 为 `8ab52d44fe60f3939b3038efd691b44d6ce9e8ace454c3b85053a921432c6626`，包含固定动画截止 timer 与不切碎限频窗口的读屏障。两者使用 `cargo build --locked --release -p stravia-server`、既有 Provider 构建与实际 WebUI 构建，不混用 test-harness 服务。

两个版本的负载完全一致：每条流 80 个累计正文 delta，首块后保留 2000ms 的本地 Provider 间隔以完成真实选择，后续每 10ms 一块；输入重复 4000 次，Provider 报告 prompt 10000 / completion 12000 tokens，不降低页面默认筛选。深链先完成 12 次间隔 2100ms 的真实 follow-up，两边均形成 14 个 Interaction、深度 12；多根为 8 次请求，多观察者为 4 个独立页面，慢消费者增加 50ms 读取间隔并对 Chromium 施加 4 倍 CPU 节流。数字不是生产流量推断或固定性能阈值。

下表为一次配对运行的实际观测。请求数和字节只来自实际浏览器观测 API；API 字节包含 SSE，SSE 字节是其中的单独诊断维度，不能再次相加。SQL 数量来自服务已有 recorder，包含场景准备及浏览器和显式 HTTP 客户端，排除额外三条只读数据库盘点查询；不能将它称为纯浏览器 SQL。`ScriptDuration` 为所有观察页面的主线程脚本时间增量。

|场景|浏览器请求数，前 → 后|API 解码字节，前 → 后|SSE 字节，前 → 后|服务 SQL 数，前 → 后|脚本时间 ms，前 → 后|
|---|---:|---:|---:|---:|---:|
|未选详情|9 → 8|2,115,124 → 95,227|2,014,362 → 4,275|394 → 381|59.25 → 38.18|
|长正文选中详情|14 → 12|2,260,634 → 259,363|2,015,468 → 112,982|449 → 428|183.65 → 89.37|
|失败请求页|18 → 5|155,504 → 10,075|20,528 → 6,676|570 → 449|18.85 → 18.90|
|深 root|15 → 12|2,322,977 → 312,094|2,011,520 → 126,497|2,002 → 1,842|240.37 → 85.01|
|多 root|20 → 16|8,396,935 → 458,284|7,913,088 → 117,938|1,273 → 1,135|251.65 → 121.59|
|四观察者|47 → 47|8,600,086 → 1,067,394|7,748,074 → 468,788|868 → 868|991.74 → 424.00|
|慢消费者|10 → 12|2,192,309 → 265,730|1,998,104 → 119,726|439 → 420|817.97 → 434.18|
|断线重连|14 → 14|2,227,927 → 290,360|2,015,481 → 92,755|491 → 481|56.03 → 39.96|

实际机制证据：未选详情与失败页不再收到任何 `live_snapshot` / `live_content`；失败页不再请求隐藏画布摘要；长正文实际 `live_content` 从两个请求共 160 次变为选中请求的 9 次，四观察者的正文更新为 36 次而非 576 次。所有场景的浏览器错误和 SSE 捕获错误均为空，服务 timeline 无丢弃记录，结束时 writer queue depth 为 0。

首段显示以所选 A/B 的真实 Provider 首块写入时间到详情首次出现正文计算；终段以该 Provider 终块写入到详情最后正文变化计算，包括传输、选择、查询与实际呈现，不等同纯调度等待。四观察者给出范围；后续页面依次选中，首段结果含真实选择准备时间。慢消费者保留 CPU 节流，重连场景保留真实离线与恢复时间。

|场景|首段可见 ms，前 → 后|终段可见 ms，前 → 后|全局连接建立数，前 → 后|正文连接建立数，前 → 后|reset 帧，前 / 后|
|---|---:|---:|---:|---:|---:|
|未选详情|不适用|不适用|1 → 1|0 → 0|0 / 0|
|长正文选中详情|362.04 → 260.89|446.00 → 182.48|2 → 2|0 → 1|0 / 0|
|失败请求页|不适用|不适用|1 → 1|0 → 0|0 / 0|
|深 root|379.81 → 273.03|362.16 → 120.71|2 → 2|0 → 1|0 / 0|
|多 root|未可靠配对|未可靠配对|2 → 2|0 → 1|0 / 0|
|四观察者|624.41–2465.61 → 345.79–1365.79|197.60–449.58 → 272.31–287.31|8 → 8|0 → 4|0 / 0|
|慢消费者|1297.45 → 859.04|1216.72 → 1191.98|2 → 2|0 → 1|0 / 0|
|断线重连|3510.00 → 3410.87|674.74 → 587.59|3 → 3|0 → 2|0 / 0|

连接数包含打开页面和既有 deep-link 导航，不把普通选中时的两次全局连接称为断线恢复。断线场景显式执行一次离线/恢复，两边实际全局连接比普通选中多一次，优化侧正文也重建一次。多 root 使用重复 A/B marker，不能可靠地把所选 ID 与八条 Provider emission 一对一配对，故保留原始时间但不报告可能为负数的错误延迟。

服务时间另列，不从累计服务 CPU 简单相减来制造“纯等待”。布局与样式为所有页面的 Chromium `LayoutDuration + RecalcStyleDuration` 增量，SQL 为 recorder 中实际累计查询时间。

|场景|SQL 服务时间 ms，前 → 后|布局与样式 CPU ms，前 → 后|节点 / 持久事件，前后相同|
|---|---:|---:|---:|
|未选详情|45.82 → 72.09|108.66 → 171.04|2 / 18|
|长正文选中详情|71.41 → 64.71|817.94 → 718.72|2 / 18|
|失败请求页|55.20 → 69.57|10.68 → 9.13|2 / 28|
|深 root|174.91 → 183.47|695.79 → 499.53|14 / 126|
|多 root|116.90 → 99.39|742.62 → 772.17|8 / 72|
|四观察者|104.06 → 126.58|2881.95 → 3096.56|2 / 18|
|慢消费者|74.79 → 61.58|1459.48 → 1756.04|2 / 18|
|断线重连|59.84 → 52.50|101.42 → 80.79|2 / 18|

主动安排的等待独立采用后端发布最多 100ms、Workspace 合并最多 100ms、动画追赶最多 300ms，合计最多 500ms。各自固定截止及首尾绕过路径通过受控时间回归验证；真实 SSE/HTTP 接收和上述 DOM 时间仍包含服务成本，不能倒推出精确的单层等待。实际正文直接消费选中 scope，不必串行经过一次摘要 HTTP；这里保留的是保守合成上界，而不是声称每一更新实际等待了 500ms。

受控预算证据与真实时延表分开：`fixed_publish_deadline_coalesces_revisions_without_stale_scope_snapshot` 在准入数据库工作结束后暂停 Tokio 时间，40ms 时再追加、99ms 时不发布、100ms 时一次发布最新全文，输出 `backend_publish_wait_ms=100`；中途新订阅拿到尚未发布的当前全文。Workspace 的持续同 root 通知用例记录实际 changes 调用时刻，输出 `frontend_merge_wait_ms=100`。Chromium 用 MutationObserver 记录最新追加真正进入 DOM 的受控时间，正文、Thinking 和滑动卡片在 300ms 内完成，动画轮的固定 timer 不等待下一个 rAF，持续追加不延后它。三层受控等待合成上界为 100 + 100 + 300 = 500ms。读己之写屏障排空观测队列并刷新 Debug，不额外发布中间正文以切碎窗口。

这些数据只证明本次同负载通信与执行结果，不证明每项资源都下降：慢消费者请求数增加；未选详情、失败页、深 root 和四观察者的 SQL 累计服务时间上升，未选详情、多 root、四观察者和慢消费者的浏览器布局与样式 CPU 上升，失败页 ScriptDuration 也略增。四观察者的部分终段可见耗时增加。最终 RSS 和 Chromium heap 有混合结果。单次运行受调度、GC 与宿主负载影响。当前服务 recorder 只提供 RSS，没有独立序列化 CPU 或分配器 instrumentation，也未精确分离端到端延迟中的每一层实际等待；这些维度不宣称已经精确测量。500ms 调度预算与真实可见延迟分别报告，不把服务时间排除解释成统一端到端承诺。

原始 JSON、SQL timeline、接收/呈现时间、资源采样与实际页面截图保存在忽略的 `target/observation-measurements/baseline-release*` 和 `optimized-final-release*`；`optimized-release*` 是收紧最后截止与屏障之前的中间测量，不与最终表混用。重新测量须显式指定各自源码构建的 executable、对应 `--web-dist`、`--browser-script`、版本标签与输出路径；优化版本增加 `--optimized`，保持上面的负载参数及默认页面筛选不变。显式 HTTP 客户端请求与浏览器刷新请求分别记录，不合并统计或用前者代替产品页面。

### 10.3 时间页与迁移

实时预设的 forest/changes 携带 `live_window=true`，适用 §9 的上界例外；所显示窗口宽度仍随当前时间推进，不扩大用户所选时长。下述固定 `[start_at, end_at)` 上界适用于历史/自定义范围及失败、拒绝请求列表。

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

以下加载分层是已确认的目标边界，当前实现尚未迁移：

- 画布只需要 Interaction 摘要；打开交互卡片后，检查器加载足以直接阅读会话的首屏、可分页的会话历史与 Run 摘要。会话文本不要求用户逐个展开 Run。
- 只有用户显式展开某个 Run 的诊断详情时，才进入该 Run 的重型诊断内容需求，包括可读思考、工具参数与结果、Target attempt 详细内容。现有「对话」页中的思考或工具 Marker 展开，以及「诊断」页中对应 Run 的展开，均属于这项显式详情需求；不增加额外操作层级。
- 关闭检查器时退出该 Interaction 的会话与 Run 详情需求；收起对应 Marker 或 Run 诊断时退出该层的重型诊断内容需求。此分层只约束管理页何时获取和订阅所需详情，不改变后台 Interaction Observation 的记录、保留、Debug 开关或 Debug Trace 契约。

默认「对话」页以只读消息气泡展示当前 Interaction：用户靠右使用 primary 色，模型靠左使用中性底色。连续同一模型的 Run 共用一组头像与名称，正文和工具继续追加在同一块内，只在末尾显示最后一条消息的时间；换模型或出现用户消息时重新分组。时间旁不显示任何执行状态或预览说明，执行状态仍在画布与诊断中保留。初始用户消息取已脱敏的 `input_preview`，缺失时可用初始 Run 的 `input_preview_recorded.text` 补足；后续 Run 的输入事件在所属回复前生成独立用户消息，不因文本相同而去重。每个 Run 的回复拼接按 sequence 排序的持久 `client_visible_content`，并以稳定 block 身份叠加尚未落盘的选中易失正文；收口落盘后对应易失 block 退役，不重复显示同一正文。不把 Debug 或 Thinking 当作客户端回复。没有用户正文时不生成用户消息，没有助手正文时隐藏气泡，但保留流式组件实例，保证首个实时增量仍可逐字显示。

思考和工具使用官方 shadcn-svelte Marker，默认折叠；有真实详情才提供展开操作，没有可读思考则不显示条目，只有工具名称时显示静态行，不增加「未记录」说明。思考置于所属 Run 正文前，工具置于正文后；展开显示普通观察事件记录的可读思考、工具输入和返回，无需开启 Debug。详情与对话不读取 Debug Trace，也不从旧 Trace 补充内容或补录未采集的历史。签名、密文不进入普通思考正文。

工具调用按 Run 和调用 ID 关联：普通平台事件的 `tool_id` 就是调用 ID，客户端返回来自 `client_tool_result`。返回仅匹配明确 `parent_run_id` 祖先，祖先路径上的历史重放不重复展示，兄弟分支各自收到的返回独立保留。工具输入与返回以安全纯文本或 JSON 呈现，不递归猜测业务 JSON、不执行 HTML 或加载远程媒体。每条 Marker 以稳定活动 ID 独立保存 localStorage 展开布尔值，折叠时删除该项；不保存正文，存储失败明确提示但不阻断展开。展开已有内容不制造「新活动」提示；后续真实内容变化仍可提示，且不收起已展开条目或抢走阅读位置。

正文复用 `StreamingMarkdown` 的安全 Markdown、追加识别、Unicode grapheme 与 reduced-motion 能力，lexer 与 parser 均显式启用 GFM，表格继续经过既有 HTML 安全白名单。仅选中详情正文、展开的 Thinking 与选中卡片输出预览逐字显示；未选卡片、输入、用量和工具结构化数据不动画。后端发布最多 100ms、Workspace 合并最多 100ms、动画追赶最多 300ms，共享最多 500ms 主动等待预算；I/O、计算与实际渲染另测，不以不同主机墙钟同步解释预算。大量追加自适应增加每帧 grapheme 数，持续更新不无限推迟追平，不设置固定字符速度的新队列。首段尽快呈现，历史首次打开、作用域/重连快照、非追加替换、完成/失败/取消及 reduced-motion 直接同步已收到最新内容，不补造尾部。离开选中表面、隐藏或卸载时取消动画积压，恢复不慢放历史；不每字符重解析全部历史或深链路。处于底部时随逐字增长跟随；用户向上翻阅或展开活动后保持阅读位置，只有点击「回到最新」或主动滚到底部才恢复。切换 Interaction 重置跟随，不滚动外层页面。通用动画规则见 [`DESIGN.md`](../../DESIGN.md)。

「对话」与「诊断」共用同一种向上加载的记录视图：打开时停在最新内容，处于底部时随新事件自动滚动。详情只带最新一页事件；仍有更早事件时，已加载记录的起点（对话页在初始用户消息之后）保留一行固定高度的加载行，向上滚到该行即显示转圈并读取更早一页，读取完成后在绘制前按到底部的距离恢复滚动位置，已在视口中的内容不发生位移。加载行在转圈、失败与移除前后高度不变；读取失败时改为「重试」操作，不在视口顶部反复请求。补入的历史不算新活动，不触发「回到最新」提示。

「诊断」页默认呈现可读的事件摘要、时间与已记录的关键事实和结果。`parent_run_id` 表达续接与因果而非包含关系：Run 按 `started_at` 拍平为并列分段并依序编号（R1、R2…），不再嵌套缩进；续接关系以分段上的「续接自 Rₙ」标记表达，父 Run 属于同一 Interaction 时可点击回跳，属于其他 Interaction 或尚在未加载的更早历史中时仅显示静态标记。编号覆盖整个 Interaction 的全部 Run；排在最新事件页最早 Run 之前、尚无已加载事件的 Run 不显示为空分段，随更早事件加载在加载行下方出现。相邻 Run 交付结束与下次开始之间超过快速续接窗口（2 秒）的等待显示为间隔行：上一请求以客户端工具调用结束时标注所执行的工具名，否则只标注间隔时长。

每个 Run 内的事件统一按 `occurred_at` 升序、同一时刻按 `sequence` 升序排列，不再将 Model Turn、Target attempt 或工具的子树整体提前展开，以免把较晚的完成事件放到较早的客户端输出之前。拒绝请求的事件采用相同排序规则。每个事件的「原始事件数据」默认折叠，展开后保留原始 kind 与完整 payload，因果关联字段不丢失；Run 和 Interaction ID 收在默认折叠的「技术标识」中。未知事件仍保留原始数据入口，不推断成功或其他未记录的结果。

Run 分段默认折叠为摘要：标题行只显示编号、模型显示名（缺失时使用 Route ID）与开始偏移，不放状态、中断或 Debug 标签：Run 状态由主干圆点的形状与颜色表达并为读屏保留状态文字，用户中断见事件流，记录不完整见 Run 告警，Debug 捕获状态见交互概览；其下两行小字分别显示实际服务的上游模型与模型服务、耗时、首 Token 与 Token 速度，以及输入、输出、缓存读取与缓存写入用量，未报告的值显示中性破折号。上游模型取该 Run 已完成的 Target attempt（平台工具循环可有多个），没有已完成 attempt 时取最近开始的 attempt，回退前失败的 attempt 不冒充服务方；同一 attempt 的修订完成事件以最后一条为准。首 Token 取第一个服务 attempt；Run Token 速度使用 Run 已确认输出除以客户端完整请求耗时，遵循 §3.3 的缺失覆盖规则，不借服务 attempts 的速度代替。展开 Run 才显示技术标识与事件流；Trace 不完整的提示不随之折叠。

排序后相邻、已经分别按 Canonical Item 收口的 `client_visible_content` 只在界面上组成默认折叠的计数分组，不改写或合并独立 item；相邻且 `name` 相同、非空的 `client_tool_handoff` 同样仅作展示分组，例如「Bash × 4 · 已交给客户端」。分组显示首次和末次事件时间，不跨越其他事件、工具名称或 Run。展开分组保留每条事件的时间与完整原文入口，实时追加保持已有分组的展开状态。事件行将原始数据入口收至标题右侧箭头，不再重复占用一行按钮；展开 Run 后，关键结果和错误直接可见，不因精简而隐藏。

`target_attempt_finished` 的耗时后显示 Token 速度。输出用量来自同一 Run、相同 `attempt_id` 的 `target_attempt_finished.usage`，迟到修订按 sequence 应用并保留此前已确认而新修订未提供的字段，不累加同一 attempt 的累计快照，也不借用整个 Run 或其他 attempt 的用量。速度复用 `computeTps` / `formatTps`，分母始终为该 attempt 的完整 `duration_ms`；缺少用量或有效正耗时显示未知。Run 摘要的耗时与 TPS 同用实际交付生命周期，首 Token 继续单独显示。卡片输出浮层使用「模型输出预览」名称；画布的已确认执行来源边保留连线、取消重复文字标签。

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

本节落实 [ADR-0053](../adr/0053-keep-one-interaction-across-generation-roots.md)：Generation Chain 仍只记录真实执行父边；Interaction Observation 优先用当前工具续接做诊断分组，只有该证据不成立且没有执行父边时才用保留尾部决定归属。新规则只作用于启用后准入的请求。

### 14.1 范围

- 只对启用新规则后准入的请求执行新归属判定，不重新分配已有 Run、不改写已有来源关系，也不提供存量 Observation 重建工具。
- 新请求正常续接已有 Interaction 时，继续按现有生命周期更新活动状态和汇总；这不表示重算历史 Run 的归属。
- 不改写 Generation Chain 节点、恢复裁剪内容、重放 Hook 或据此启用 Target Continuation。
- 不从 User 正文中的通知标签推断可信请求用途，不按 session、模型、Route 或时间最近猜测来源；标题、摘要、子代理不因共用 session 而归入主任务。
- 实现保持 SQLite/PostgreSQL 等价，不新增生产依赖；具体 schema 或公共响应字段如需扩展，在实施前明确变更契约。

### 14.2 实现要点

1. **归属判定集中在 Observation 模块。** 先独立核验当前工具来源；不成立时，已有 Generation parent 走原路径，没有执行父边才用保留尾部决定归并、创建有来源的新 Interaction 或保持独立。Server、Desktop 与 WebUI 不复制判定规则。
2. **当前工具续接。** 用同 Principal 已交付调用的未完成工具 ID 与当前输入尾段精确匹配；旧结果回放、重复或冲突来源、缺失交付证据不能宣称唯一。确认后优先归入来源 Interaction，以来源 Run 作为 `parent_run_id`，即使已有 Generation parent、夹带 User 或超过尾部五分钟窗口；`generation_parent_id` 不变。
3. **尾部指纹索引。** 以最后 canonical 单元哈希筛选候选，再做完整语义核验。同 Principal 历史超过 128 个不再导致全部匹配失败。指纹不代替核验，也不按时间窗口排除潜在冲突来源。
4. **按需物化。** 缺失窗口从仍保留的 Generation Chain `client_items` 重建；先合并内存和持久化候选、去重并检查候选预算，再批量读取缺失窗口。已确认父节点本身属于候选时优先展开其父链，在完整校验后按 root 到 head 折叠客户端历史，同链祖先候选复用本次遍历的窗口，不各自重新展开整链。只保留本次候选窗口，不缓存所有完整历史前缀，也不引入跨请求缓存；重复来源和其他分支仍参与原有歧义核验。进程缓存可淘汰，过期或已清理来源不复活。核验超过资源预算时返回 `resource_limit` 或 `index_unavailable`，不把部分检查包装成唯一匹配。
5. **准入时持久化。** `run_admitted` 同时保存 `grouping_reason` 与 `diagnostic_source_run_id`；尾部核验结果以 `retained_tail_associated` 同轮写入。诊断来源不是 `generation_parent_id`。只有新增 User 打断父交互时才 `interrupt_predecessors`；归入本 Interaction 的续接准入在同一事务内把仍等待的父 Run 终结为 `superseded`（见 3.2），不归入中断。
6. **派生视图。** 合并后的 Interaction 共用状态与用量；诊断连接的新子交互分别汇总。失败、取消和交付事实不因后续成功改写。
7. **契约。** README 两种语言、schema 文档与 `0047_observation_tail_sources` 迁移同步。页面继续区分确认边与诊断边。

### 14.3 决策表

本表按顺序判定。当前工具续接不要求缺少执行父边；保留尾部归属路径仅在工具证据不成立且没有 Generation parent 时适用。

| 条件 | 结果 |
|---|---|
| 唯一确认当前工具续接，包括已有 Generation parent 或夹带新增 User | 归入来源 Interaction，观测父节点指向来源 Run，不受尾部五分钟窗口限制 |
| 未满足工具续接；已有 Generation parent | 按 3.1 的父观察规则分组，不用保留尾部覆盖归属 |
| 未满足工具续接；唯一完整尾部匹配，无新增 User，间隔在 `[0, 300000]` 毫秒内 | 归入来源 Interaction |
| 未满足工具续接；唯一完整尾部匹配，匹配区间之后有新增 User | 新 Interaction，诊断连接来源 |
| 未满足工具续接；唯一完整尾部匹配，无新增 User，但超出五分钟窗口 | 新 Interaction，诊断连接来源 |
| 无法证明满足时间窗口，但完整唯一的来源证据仍成立 | 不自动归并，保留来源连接 |
| 来源缺失、匹配不完整、有歧义或候选核验未完成 | 不自动归并，不猜诊断父节点 |

### 14.4 回归与运行验证

三个实际断点转为不含凭据或业务原文的隔离样本，通过真实准入、归属、持久化和查询路径验证，不将执行 ID、正文或源文件内容写成特例。

- 删除首条 User 图片，保留原文本并提交来源的三个当前工具结果：同一 Interaction、新 Generation 根，裁剪图片不重新进入模型输入。
- 删除旧工具截图，同时新增另一张工具截图且图片总数不变，并提交四个当前工具结果：仍正确确认来源，最终回复留在同一 Interaction。
- 连续改写旧工具结果，使严格前缀反复退回较早父节点，同时回传最新工具调用的结果：Generation 父边保持原样，观测父节点逐轮跟随最新来源，最终回复留在同一 Interaction，改写后的输入原样交给模型。
- 旧历史被摘要替换，保留精确连续尾部并提交当前工具结果：正确续接；另设没有当前工具结果的样本单独验证尾部归并，避免工具路径掩盖尾部缺陷。
- 尾部无新增 User：恰好 `300000` 毫秒归并，`300001` 毫秒创建新交互但仍连接来源；缺少有效时间证据不自动归并。
- 尾部后新增 User：窗口内外都创建子交互；同时满足当前工具续接时，验证工具续接优先。
- 同 Principal 超过 128 个保留调用、进程重启或缓存淘汰后，具备完整证据的来源仍可被发现；核验预算耗尽时不误报唯一来源。
- 旧工具结果回放、重复 handoff ID、不同 Principal、并列来源、仅短文本或不完整交互均不能触发误归并；窗口外的冲突候选不能因时间过滤被忽略。
- 同模型或 session 的标题、摘要及独立子代理不被错误归入主交互；模型或 Route 不同也不单独成为拒绝合法来源的理由。
- 合并后的状态、用量与 Debug Bundle 不重复计算，子交互分别汇总；历史失败与交付记录保持原事实。SSE 和刷新后的详情、forest 得到一致归属。
- 来源过期或被清理后不复活节点；新规则不重分配既有 Run。诊断失败不改变客户端响应、执行重试或 Generation Chain。

验证先运行最直接的 Rust 回归，再扩大到 core 检查和相关 SQLite/PostgreSQL 存储用例。使用隔离、非生产的模型服务进行实际 HTTP 请求和浏览器检查，观察新请求形成的交互、父子连接及最终回复；实现触及 Desktop 特有行为时再验证实际桌面应用。
