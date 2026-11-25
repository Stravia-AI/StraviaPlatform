# 交互观测与请求记录

Interaction Observation 记录一次客户端任务中的 Inference Run、Model Turn、Target 尝试、工具交接、实际交付及已确认用量，供管理诊断与分析。
失败、取消、重试和分支保持各自事实。普通记录不保存完整 canonical 请求／响应，不保存思考签名或密文；原始应用协议字节另由显式 Wire Debug 捕获。

## 执行隔离与存储失败

Observation 不是模型历史、执行调度或 Generation Chain 的事实源。它不恢复模型输入、不重放 Hook 或生成，也不按 IP、连接、客户端名称或模糊文字推断执行历史。
实时通知、Debug 开关和文件只保证同一 Gateway 进程内可用；持久历史可跨进程读取，不承诺实时聚合或共享本地 Trace。

- 观测队列过载、写入、SSE、文件、容量统计或导出失败不改变推理、路由重试、Client Output Commit 或 Generation Chain 提交，也不终止实例。能确认的缺失标记 `observation_gap` 或 Trace `partial`，不能补造用量或重放请求。
- 关键同步存储失败只让所属请求或任务按原错误失败；异步诊断失败只归诊断任务，不取消推理或其他工作。见 [ADR-0060](../adr/0060-own-interaction-observation-outside-generation-chain.md)。
- 已交付但 Generation 提交失败记录 `settlement_generation_commit`，显示「响应已交付，历史未保存」，不改报失败。Generation parent 存在而其观察不可用记录 `generation_parent_observation_unavailable`，不伪造父节点或断言历史库故障。
- 独立统计写入失败时保护破坏性清理，见[长期 Stats Usage](#长期-stats-usage)。

## 归属与交互链路

**Interaction** 汇总一个客户端任务的一个或多个 Run，可有并发子树。按同一 Principal 的已证实来源做诊断归属；充分证据可以跨 Generation 根，但不新增或改写执行父边，不恢复裁掉的输入，也不增加 Target Continuation 权限。见 [ADR-0053](../adr/0053-keep-one-interaction-across-generation-roots.md)。
`parent_run_id` 是观测来源，`generation_parent_id` 是执行父边。记录归属理由与可确认来源，不重分配已保存历史。

### 归属优先级

1. **已证实的工具结果后 User 尾段编辑。** 已保存收据与完整 canonical 输入证明工具结果及此前历史完全相同、只编辑其后非空普通 User 尾段时，另建 child Interaction，连来源 Interaction 和 Run。该结论不受两秒或五分钟规则覆盖，即使模型或控制变更导致执行父边缺失。
2. **Current Tool Continuation。** 由本次真实收到输入的当前尾段，核验同一 Principal 下已交付来源的待回传工具。尾段从最后一个 Assistant 后开始，接受顶层结果及 User 中的 ToolResult；本批结果须唯一对应同一个来源 Run，交付不晚于本次 ingress。
   确认后归入来源 Interaction，并把来源 Run 作为观测父节点；即使夹带新 User、执行父边更早或缺失、超过五分钟，结论不变。同一父响应的并发合法续接仍各保留 Run。
3. **已确认的 Generation parent。** 无新 User 的精确续接／失败重试归入父 Interaction；其精确 delta 回传父历史的当前待处理工具时也归入父 Interaction。否则有新 User 时，只有 ingress 距父响应完整交付 `[0, 2000]` ms 且父 Generation node 未被其他 Run 续接，才快速归并；失败且无输出的尝试不占此资格。
   快速续接可重新激活已完成 Interaction，不区分自动提醒与真人输入；已有续接的后到支路或其余新 User 新建 child Interaction。
4. **Retained Tail Association。** 无当前工具来源也无执行父边时，唯一完整尾部匹配、匹配后无新 User 且 ingress 距来源完整交付 `[0, 300000]` ms，归入来源 Interaction。
   若有新 User、已超窗或不能证实时间，但来源仍完整且唯一，创建新 Interaction 并保留诊断来源；不先按时间排除较旧的冲突候选。已存在执行父边时可独立记录尾部诊断，但不覆盖执行父边主导的归属。
5. **失败根重试与独立根。** 无父边且无前述来源时，同 Principal、全 canonical fingerprint 相同、候选 Run 已失败且无 Client Output Commit、间隔小于两分钟、无同指纹活动 Run 才合并根重试。
   指纹只排除尝试级 `parameters.session.request_id`，不使用输入短文本或任意 session 标签；索引只在本进程，重启后不猜。在同一归属内不再重复发布此重试的用户输入，仍保留两条 Run。
   证据不成立就新建独立根，即使合法请求没有 User。

### 工具回放证据

- 交付收据建立待回传调用索引；合并易失与持久有效来源，不等 Generation 提交或窗口重建。旧工具结果不算当前回传；相同 ID、冲突来源或仅含历史截断证据不足。
- 已保留 `client_tool_result` 收据加上完整旧输入是严格前缀、且新后缀仅是普通 User，证明旧结果重放：不参与当前工具归属，不再捕获该结果，仍应用正常父节点和时间规则。
- 前缀证明保留 leading system/developer、媒体和原生控制等 canonical 语义，只忽略既定交付、graph 与缓存元数据。进程缓存缺失可从仍保留的 Generation 重建 received-input；截断、过期或无法无损重建时不猜。
- 同 ID 的新 handoff 是不同调用；仅沿可信祖先比较既有收据，最近结果的内容与错误标记都相同才可去重。结果有变、并发兄弟分支、证据缺失或重复 handoff 不全局吞掉。
- 已核对的工具续跑若能证明有序用户项与来源一致、且当前以新工具结果结尾，不把完整回放里的 User 误报为新增。证明缺失或确有新增／删除／重排时保留输入。

### 尾部匹配与原生压缩

索引只以已收到的规范化 client-shaped 输入与实际已交付输出为证据。旧后缀须完整匹配本次输入的连续区间；只可排除顶层 leading system/developer，不能删除内部差异后拼接。
指纹仅筛选候选，随后核验角色、内容、媒体、工具参数／结果及控制。各来源取自身结束最晚的强匹配；跨来源依次比较结束位置、语义单元跨度、证据字节，并列为 `ambiguous`。

- 强匹配须达到完整语义单元和公开字节门槛，含完整 User／Assistant 交互或闭合工具关系；典型最低为 2 单元、256 canonical UTF-8 字节、64 Assistant 字节。仅短提示或相同会话名不足以归并。
- 带合法 History Marker 的公开思考投影可核对原样连续性，不恢复隐藏内容，也不计为公开回答。签名、密文等不透明原生状态只以域分隔摘要校验连续性，不计入强匹配阈值；变化即不匹配。无可证回放身份的私有思考使用不可匹配边界。
- 冷缺窗口可从仍保留的 Generation 客户端视图按需重建；不能完整索引或超出候选核验预算，报告 `index_unavailable` 或 `resource_limit`，不把部分扫描当唯一来源。物理清理过的来源不能复活。
- `retained_tail_associated` 是诊断推断；`native_compaction_associated` 是已登记原生状态跨越已验证边界，独立于确认的 Generation 父边。三者不互相替代。
- 支持的[原生压缩](open-responses.md#原生压缩)只来自 Responses。记录实际 standalone／inline `compaction_operation`，按所属 Target 尝试计量；回放已登记状态不算新压缩，独立压缩不造空 Generation 节点。

尾部窗口按最多 512 个语义单元／512 KiB 保留最新后缀；单个单元本身超限不截断后伪匹配。核验按候选和读取字节限额执行，超过预算只影响诊断。
能用于严格回放的输入证明还需要包含 leading 前缀的完整 received-input；可做尾部诊断的窗口不自动等于完整回放证据。

归属计算由 [Core Run Attribution](../../backend/crates/stravia-core/src/interaction_observation/attribution.rs) 完成，Server、Desktop 与 WebUI 不复制判定。
作者必须区分 ingress 接收时刻、父响应完整交付时刻和 writer 排队时刻；只有前两者参与归并窗口，其他分支活动不延长资格。

### Interaction Chains 成员与状态

交付过客户端可见输出的交互保留在链上，包含之后失败的 Run；尚未产生最终失败的活动节点可显示。**从未交付任何客户端可见输出且最终失败的 Interaction 不进默认交互链路**，仍可从客户端错误或深链打开；准入前拒绝不伪造节点。
筛选可保留因果上下文，但不会把被排除的失败节点当默认成员。[历史回看](history-recall.md)复用链路成员规则，不读取 Debug。

主状态按优先级为 `running`、`waiting_client`、有最终响应的 `completed`、等待断线后的 `disconnected`，其余为 `interrupted` 并保留真实原因。
历史失败标志独立于主状态，不覆盖正在运行、等待或最终成功。Run 与工具活动的生命周期不相同。

- Delivery 确认终态才产生交付收据：HTTP/SSE 终帧交给实际响应流，WebSocket 终帧实际发送成功，非流式为完整 body。生产者入队、HTTP 200 或 Generation ID 不等于 Client Output Commit。
- 同 Interaction 的续接在准入事务内将仍等待的观测父 Run 终结为 `superseded`，不算用户中断；来源 finish 比续接晚时仍保留接替。新 User 另开任务仅中断原未完成且未接替的等待分支。
- 同交互更晚的完整工具收据也可解除等待聚合，即使来自 sibling；错误结果算回传、不算最终生成成功。缺失、部分、空 ID 或不确定调用边界保守保留等待。
- WS 关闭只结束本连接上未接替且未获完整回传的等待；HTTP/SSE 正常响应结束不意味着客户端离线，闲置 24 小时可记 `client_wait_expired`。合法晚到续接仍可再次激活。
- 启动收口上一进程遗留活动与真正未解决等待，以 `run_state_changed` 记录 `process_restarted`，不推定客户端离线、不重放工具，也不改以前的交付时间与用量。
- 最后一个观察句柄释放才以 `unfinished_observation_activity` 收口残留 Turn／attempt；已交接后台执行不因请求诊断收口、删除或过期而被取消，见[工具执行交接](agent-core.md#工具执行交接)。

### 状态与摘要的独立性

交付收据先于迟到的执行 finish 或 Generation 提交记录；没有可索引历史窗口不阻止记录实际交付。
只按已确认收据设置交付结束时间，迟到 finish 补齐执行事实而不延后已知交付时刻。收据失败仅记录观测缺口。

Bundle 内的分支状态只从截止快照内的交付和回传推导；晚到结果可结束当前页面等待，但不能改变先前导出的快照。

## 请求终态与成功率指标

分类依据真实上游发送及用户请求终态，而非 HTTP 状态码或是否建立了本地尝试记录。见 [ADR-0083（请求终态与上游错误）](../adr/0083-separate-rejected-requests-from-failed-requests-and-count-upstream-errors.md)。

| 类别 | 口径 |
| --- | --- |
| Rejected Request（错误的请求） | 准入前拒绝，或尚未触发上游时的平台校验拒绝，如模型不存在、无权使用、Hook 拒绝或参数错误。 |
| Failed Request（失败的请求） | 排除上述拒绝及取消、断线后，最终失败的请求；含无可用 Target、额度暂停、真实上游 4xx 或已经发送后的本地错误。 |
| Client Errors（客户端错误） | 单次请求最终 Failed 或 Rejected；不包括同一请求内已吸收的上游失败。后来另发请求成功不抹掉旧错误。 |
| Upstream Error（上游错误） | 每次来源为上游的失败 Target 尝试，含独立能力、真实非 2xx、连接／传输失败、首 Token 超时、200 后流错误和凭据拒绝。未发送的本地准备、主动取消、中断或未知来源不入。 |

请求级分母 `T` 是非取消、非断线、非 Rejected 的已终结 Run：最终完成、完整交付工具（`waiting_client` 或 `superseded`）或最终失败。隐藏轮与多次尝试不增加该分母。
设 `F` 为 Failed Request、`R` 为 Recovered Request：

- **Final Success Rate**：`(T - F) / T`。
- **Recovered Request**：同一请求经自动重试或切换从失败恢复且最终成功；每请求最多一次。后来发起的请求不追改旧行，也不将安全恢复过程中的非上游发送误作上游失败。
- **First-pass Success Rate**：`(T - F - R) / T`；只在 `recovery_known_request_count = T > 0` 时展示，恢复次数及百分点提升同样要求完整覆盖，未知不填零。
- **Upstream Attempt Success Rate**：`已完成的 Target 尝试 / (已完成尝试 + 上游错误)`。实际重试和 failover 各计，包含客户端无感的内部错误；不与请求分母混用。

请求分类按 Run 开始时间分桶；上游尝试在所属 Model Turn 开始时间入桶。没有 Model Turn 的拒绝仍计入客户端请求量，但不进成功率。
Provider 汇总用上游口径，不是客户端请求分解。比率从窗口计数重算，不平均桶比率；无有效分母或覆盖未知不当成零。

## Confirmed Upstream Usage

每个实际 Target 尝试最多确认一次用量。成功采用 Vendor 完整值，失败、取消或中断只确认最后已收到的报告。
多帧是同一 attempt 的字段修订，不累加累计值；未知不盖掉既知值，明确零可替换旧值，不以 tokenizer 估算填补。
后续发布、恢复或交付失败不改写该尝试已确认用量，Model Turn 成功仍取决于其自身发布终态。

- **Total Input Tokens** 是原始协议归一后的总输入；原始事件、尝试与客户端 wire 保留总输入。输出已包含 reasoning，不再次累加；缓存读、缓存写单列。
- **Net Input Tokens** 是管理面输入。每 attempt 总输入与缓存读均已知才算 `max(total input - cache read, 0)`，不扣缓存写。聚合累加各已知净值；缺任一操作数计入缺失覆盖，而非把已知部分作全量。UI 不再扣一次。
- OpenAI-compatible 和 Anthropic 在各自协议归一入口合并累计字段；标准 Gemini 缺缓存字段仍未知。Antigravity 只对确认完整的私有终态 usage 应用 proto3 标量省略为零；整个 usage 缺失或中间帧不补零。见[上游响应证据](../research/antigravity-oauth.md#响应)。
- Interaction、Run 与 Bundle 累计各尝试已报告字段，含失败、隐藏轮和 failover；全部未报告仍未知。覆盖只反映已观测 attempt，不替代 gap。
- 客户端终态使用独立 Run 账本，不读 Observation；隐藏轮与重试的用量只合计一次，缺失的缓存及推理分项不冒充完整。见[客户端协议](open-responses.md)。
- Run TPS 使用已交付全耗时（未交付终态可用实际结束时刻）和已知完整输出；Attempt TPS 用自身全耗时。首 Token 延迟含首次 canonical 输出（含可读思考），不从 TPS 分母扣除；缺少有效耗时或必要用量时不猜。

管理面保留每项已报告覆盖和缺失信息。迟到报告先修正可查询数值，再发布同 kind 的持久终态修订，不发单独的易失 usage 通道。
部分观察丢失、确认用量字段本身未知与没有输出是三种不同状态。

请求记录的 Token 筛选取完整根链（含子孙）的已知净输入与输出，不加回缓存项。
尚运行且未获任何用量的 Model Turn 临时计一次本地输入估算；收到任意报告（含零）或轮次结束即停用估算，链路可能重新不命中。
估算不进确认用量、卡片计量、统计和计费。计算与路由共享 canonical 输入、instructions 和工具定义的 JSON 字节量口径，不当作模型 tokenizer 结果。

确认用量可以先于尝试终态被写入和查询；终态附带最后一次已知快照。新报告的已知部分替换对应字段，不删除未更新的已知字段。
Attempt 自身没有聚合 coverage；Run 与 Interaction 的覆盖同时报告已观察尝试数与各字段的缺失数，不能把丢失的观察尝试算作已报告。

### 长期 Stats Usage

长期统计读取独立数值汇总，不以仍保留的 Observation 明细代替。SQLite 与 PostgreSQL 维护全局、Route、Provider 与 API Key 的绝对时间整刻钟桶，不保存正文、工具内容、Trace 或长期逐请求账本。

- 用量分析的请求级分类来自请求／拒绝事实；通常展示的 Token、首字与 TPS 从已成功尝试汇总。加权 TPS 是 `Σ output / Σ 完整耗时`；缺输出或耗时、分母无效为未知，已知零保留。
- 同一来源及修订在事务内以 checkpoint 差额更新桶；删除明细只回收工作 checkpoint，不扣长期累计。历史身份按内部 ID，配置删改不抹掉已累计事实，同名重建不合并。
- 汇总写入失败回滚来源事务；若进程不能证明携带统计的事实可靠入账，拒绝手动或自动破坏性清理，不能先删来源再丢贡献。纯正文、工具或 Debug 缺口不触发此闸；未保存事实不可从旧 Debug 或上游重建。
- 查询窗口为绝对时间整刻钟 `[start_at, end_at)`；长窗口合并充分聚合量再算均值与比率。Provider／API Key 边际过滤互斥，不提供不可推导的请求级恢复／成功率或模型占比。
- 清历史、Debug 或本地聊天都不重置 Stats。选路使用自身短窗口原始事实，不读长期桶冒充实时调度。金额规则见[成本与计费](cost-and-billing.md)。

独立持久汇总不设自动过期或长周期降采样，空间随时间和统计维度增长；只剩汇总时，不能追溯已删除正文或恢复秒级历史。
统计展示保留未知与零的区别，不以所选范围中的小桶均值代替整体；按量金额与用量各有独立口径。

## 普通事件、内容与实时游标

### Canonical Item 与事件形态

普通时间线保存准入／执行／交付／终态、模型轮、尝试、工具、使用量、归属和 gap。事件只保存变更顺序及小属性，正文引用共享内容存储；结构见 [SQLite](../database/sqlite.sql)、[PostgreSQL](../database/postgres.sql) 与[共享正文存储](data-layout-performance.md#共享正文存储与引用回收)。

- `model_thinking` 与 `client_visible_content` 按真实 Canonical Item 收口，保留 item 与 part 边界。成功保存完整项；可处理的失败或取消仅保存已收到部分并标未完成。崩溃可丢失未落盘项。
- 可读思考不是客户端可见输出，不含签名、密文或混淆快照；WS 可见正文仅来自实际发成功的消息。物理分块不制造逻辑项。见 [ADR-0062](../adr/0062-persist-diagnostic-content-at-canonical-item-boundaries.md)。
- `user_input_recorded` 保存本轮输入的完整文本单元（不含媒体或纯工具结果），只在模型侧凭据映射完成后发布；失败或取消不提前发布。它与客户端工具结果仍保留实际收到的内容，采集不等于模型侧替换。
- 工具开始与结果保留可解析 JSON 或原始参数；无字段表示未采集，JSON null 是实际值。客户端结果每批一行，依调用 ID 关联；同一调用在 UI 上就地合并，不把结果重复展示为新调用。
- Model Turn 开始／结束合并首个／最后一个 attempt；仍需独立观察的 retry、failover 或等待中的后续尝试分别保留。用量进入尝试终态，迟到报告以更高 sequence 修订同一事实；交付并入 Run 结束。见 [ADR-0077](../adr/0077-slim-interaction-observation-and-file-debug-manifests.md)。
- 缺父观察事实时记录 gap，不造孤儿开始事件、假 parent 或零用量。收口并不重新取发生时间：事件保留生产边界的 `occurred_at`，writer 的接收顺序、实际发生时间和持久 sequence 各有意义。

用户输入与客户端工具结果共用映射登记后的发布边界；纯工具续接没有新文本时仍可发表真实结果。
摘要只取起点输入和全部客户端可见块，后续工具结果、思考或 observations 本身不代替客户端输出。

实践中排队准入、交付收据、执行 Finish 和最终清理各有边界；有界队列无法准入时不继续为该 Run 写孤立后续事件。
后台写入按请求阶段合并，统计事实的写入不由可选诊断是否成功决定。事务与通用写协调详见[数据布局与性能](data-layout-performance.md)。

### SSE 与恢复

持久事件与受影响投影同事务提交后广播，ID 为递增 sequence；清理不重用已提交 ID，稀疏不是缺包。
先取同一数据库快照下的权威视图与 `snapshot_sequence`，再从该游标订阅，先补已提交变化再接续实时。

- 全局 `observation` SSE 只传轻量变化通知；正文、工具内容和完整事件另按需查。基线过旧、超前、清除后失效或不能安全恢复时返回 `reset_required`。
- 恢复重新查询 forest、详情及选中实时作用域。采用重建快照的实际水位，即使低于旧水位或为零；订阅分配水位不是已应用事实，过滤产生的内部空隙不是 reset 依据。
- 仅选中的 Interaction 订阅 `live_content`、`live_snapshot`、`live_gap` 和 `live_finished`。先读累计快照（可为空）再续流；这些信号无 SSE ID，不推进持久游标。
- 同一块的易失更新以最新累计值替换，不重播历史动画；收口后用对应持久 item 接替，不拼接重复正文。易失溢出是独立 gap，断线或重启不承诺恢复未落盘内容。
- 事件分页固定 `through_sequence` 后前翻历史或补增量，选择、过滤及窗口变化后拒绝旧上下文响应；恢复失败保留已有视图和明确重试入口，不冒充空结果。

易失通道按块保留最近累计更新和显式结束，首段与终态不等待后续合批。
单作用域实时内容镜像有 8 MiB 边界；超限不扩大队列、不反压推理，也不意味着已保存历史被截断。

清理与 reset 使旧通知基线和易失作用域失效。
全局 SSE 断开不阻止后台持久化；恢复只能重放仍存在的历史，不复活已清除记录。

### 保留与清理

普通观测、Rejected Request 和 Debug 使用 `log_retention_days`（默认 7，最少 1）。Interaction 按真实最晚活动时间，拒绝按发生时间判断；Run 与事件随 Interaction 删除，不单独到期。Debug 使用 manifest 的截止时间，后台捕获不为已结束请求续期。
周期清理分批、写事务内复检资格；过期但尚未被物理删除的记录仍可读，不等于恢复已删除记录。调整保留期触发清理，不改写每条 Observation。共享正文只回收失去全部引用的内容。

- 「清除历史记录」保留执行中与真正等待客户端的详情并报告跳过，不取消正在执行的推理。只删除自身诊断引用，不改写 Generation Chain、History Marker 或后台执行结果。
- 「清除 Debug 数据」停止活动 Trace、删除受管文件并清空当前实例索引；活动 Trace 标记清除缺口。不改开关、不删普通 Observation；后续准入仍按开关捕获。
- 断线、进程重启、接替及观测收口等状态诊断不推进历史活动时间，不把旧链重新移入最新时间页。长期 Stats 的清理保护与保留周期独立。

周期扫描按小时触发；缩短保留期后另安排清理。
运行中的 Interaction 不回收；清理按根对象原子删除，不暴露半条链；批与批之间允许正常运行事件继续提交。
Debug 目录先重命名再删除，启动会完成中断删除并清理孤儿；这不延长已删除资源的授权。

清理后的原始正文不能从长期桶取回；相反，正文仍有历史引用并不意味着普通观测接口仍可查到。
已删除的内容引用、Trace 和实时索引立即从各自接口失效；客户端须按 SSE 恢复规则重建视图，不保留已删除节点。

### 凭据命中摘要

创建与复用由普通 `credential_mappings_created` / `credential_mappings_reused` 事件投影，不依赖 Debug。记录规则、来源和有限块标识，不持久化秘密、指纹或消息片段来归因。
列表每行代表一个新映射，后续复用为同映射内位置记录，不冒充新发现；缺失或已清理的记录不证明没有发现。
映射到期按 **`last_detected_at`（最后一次规则检出）**，不是最后使用；简单替换、还原、查看和遮罩不续期。完整定义、位置回看与管理端遮罩见[凭据保护](credential-protection.md)。

## Wire Debug 与脱敏边界

设置页「诊断」唯一 Debug 开关是进程态，默认关闭、重启不恢复。每个 Run 在准入时、Rejected Request 在 ingress 时独立快照；关闭只影响之后的新 Wire 捕获，不删除既有 Trace，同一 Interaction 可为混合状态。

四方向捕获只记录实际应用协议收发：

1. Connect Client → Stravia。
2. Stravia → 上游。
3. 上游 → Stravia。
4. Stravia → Connect Client。

HTTP/SSE、WebSocket 和远程工具按各自实际传输边界取头、正文、应用消息、顺序与时间，不等待语义解析或 Canonical Item 收口。工具及其后台执行继承原 Run/Trace；绑定实际执行上下文，不串到其他请求。
不录 TLS/TCP packet、HTTP/2 frame、canonical 阶段、Hook、Client Projection 内部态或工具中间态。WebSocket Ping/Pong 只留类型、方向和时间；只记录实际收到或发出的 Close，不为 EOF 或取消合成帧。见 [ADR-0063](../adr/0063-record-four-direction-wire-debug-at-transport-boundaries.md)。

- 读取边界拿不到的字节、自动跳转隐藏的中间跳、能力模块无法完整提供的原始内容、队列溢出与文件失败标 `partial`，不为 Debug 额外消耗响应或改变执行。非法协议内容完整捕获后仍是完整字节；拒绝请求无实际出站不算缺失。
- Trace 不逐条写普通事件；分段文件和 `diagnostics/observation-debug/<trace-id>/manifest.json` 保存可读物理水位与状态。manifest 原子发布，本实例索引支持查询，损坏单文件不阻断其他 Trace。终态发布前 flush 已排队数据；持有观察句柄的后台执行可继续至最后收口。见 [ADR-0077](../adr/0077-slim-interaction-observation-and-file-debug-manifests.md)。

Trace 按方向和实际关联保留上下游、Run、Model Turn、attempt、顺序、时间和传输类型。HTTP 头可保留有序原始字节、重复字段；不以重排后的 JSON 猜测原始内容。
捕获队列和缓冲有界，落盘量仅统计真实文件字节，不承诺每次 Trace 或整个实例固定磁盘上限。

捕获可包括已实际读取的非成功响应、二进制正文、原始 SSE 分隔符及 WebSocket 握手与应用消息。不能读取的正文不回填推测内容。
可提供浏览器网络证据的扩展只记录可归属的实际观测，不以 DOM、错误字符串或重新编码内容充当原始字节；浏览器协议限制如实标 `partial`。

**Authorization-only 捕获过滤**在进入队列、易失通道或落盘前，只替换实际 HTTP `Authorization` header 的值，头名不分大小写。普通 Observation 和 Wire Debug 对同一字段执行同一规则，**开关开或关都不改变普通 Observation 的过滤**。
普通 JSON、工具参数中同名字段并不是 HTTP header，不遮；Cookie、API Key 头、URL 与 query、body、媒体、思考、工具输出、上传／下载授权或其他原始凭据可能留在各自捕获范围内。

这不是[凭据保护的管理响应遮罩](credential-protection.md)：普通详情、正文、错误列表和实时流在响应边界按有效映射遮罩，但不会改写已捕获或已存储事实。诊断包是原始取证路径，不应用管理响应遮罩；进程日志和普通面向客户端错误继续独立强脱敏。导出及共享前按敏感数据处理，记录下载授权不表示可续签。

Debug 同时启停独立性能采集：设置页 `GET /api/v1/performance/export` 下载 Prometheus 快照和 Chrome Trace ZIP，不含 SQL 正文、绑定值或请求正文。
关开关停止新采样、释放会话临时时间线；不清已累计指标。清 Wire Trace 不是重置性能会话。性能时间线的墙钟与逻辑字节不能当 CPU 栈或 RSS 分解。

性能导出把 Prometheus 快照与同一会话时间线封成一个附件。记录器不可用或压缩失败返回明确错误，不生成残缺的成功归档。
关闭 Debug 后可查本进程累计指标，关闭会话的时间线不保留；重新开启不纳入上一轮迟到 span。
指标只归因到可观察的宿主边界，例如 SQL driver 操作或池获取，不能当成服务端数据库执行时间、引擎语句数或真正 CPU 耗时。

## 诊断包导出

Interaction Debug Bundle 是按选中 Interaction 的固定截止快照，不包含同根其他独立 Interaction。运行中可导出，完成后须重新签发才能看到其余收发；禁止把运行中快照当最终完整包。

1. 等目标 Interaction 的 writer 屏障并固定 `through_event_sequence`；在 Trace snapshot 屏障 flush 并固定分段物理字节前缀。同 sequence 后写入的字节也不能进入旧快照。
2. 流式 ZIP 含机器可读 `manifest.json`、`interaction.json`、说明和按 Run 的 `events.jsonl`；导出展开正文引用及捕获记录，保持实际字节与事件顺序，不受详情分页限制。
3. manifest 标记导出时状态、每 Run 的开关快照、文件字节及缺失原因，整体为 `complete`、`partial` 或 `none`。全部未捕获为 none，有期待但缺失、不完整或开关混合为 partial；完成度不等于模型请求成功。
4. Rejected Request 使用同一家族的 `rejected_request` 包，不伪造 Interaction。导出摘要只聚合快照内事实及每尝试的最新确认用量，不借未来收据解除过去的等待。

管理会话通过对应 Interaction／Rejection 的 `debug-bundle-tickets` POST 固定快照，签发 60 秒有效、进程内单次下载票据。
浏览器 GET `/api/v1/observations/debug-bundles/{ticket}` 后消费票据并流式返回 ZIP；它不授予其他管理权限，过期、已消费或重启后不可用，不进日志、Observation、Trace、Referer 或错误文本。

Bundle 摘要从同一快照重新派生，输入与输出引用按需解析，不能直接复用刷新过的在线 Summary。
导出的原始事件用量保留上游总输入，管理摘要先合并同一尝试的修订，再计算净输入。
导出读取失败或归档中断不改变普通记录、推理或未来下载的内容。

## Admin HTTP 与请求记录

实现入口为 [Core Observation](../../backend/crates/stravia-core/src/interaction_observation/mod.rs) 与 [Server 管理 HTTP](../../backend/apps/stravia-server/src/admin_routes/observations.rs)。以下是客户端依赖的行为，不是 DTO 字段清单。

管理路由统一在 `/api/v1/observations`，遵循[管理身份与入口策略](admin-auth-bootstrap.md)。浏览器页面为 `/logs`，提供交互链路、客户端错误和上游错误三个视图。

- `interactions` 是完整 forest；`interactions/{id}` 打开单个 Interaction，`interactions/{id}/events` 向较早读取或补增量，`interactions/{id}/live` 只订阅其易失正文。
- `events` 提供全局通知；`client-errors` 与 `upstream-errors` 独立分页和筛选；`rejections` 直接访问未形成 Run 的记录。原始证据须通过诊断包，不能从普通错误详情反查任意 Trace。

- Forest 按根链游标加载；绝对时间 `[start_at, end_at)` 最大跨度 24 小时。根按最新真实活动归唯一时间页，返回保留期内完整因果上下文，非命中成员弱化而不裁掉祖先。实时 forest 可包含已过查询上边界的新活动；固定历史窗与错误列表仍严格遵守边界。
- `interactions/changes` 按已知根链返回变化、必要上下文、删失原因与快照，不能证明完整时 reset。物理删除、筛选失效与窗口迁出不同；已打开历史链迁到更新页时保留当前视图并提供跳转，新查询不在两页重复。
- 摘要只有有序正文引用及长度：输入取根 Run、输出按顺序取全部客户端可见块。`contents` 仅供仍被有效观察引用的资源，不能读取仅被 Turn Chain 或 Marker 引用的历史；按遮罩版本校验私有缓存。无文本但已交付工具等内容时明确「已交付，暂无文本预览」。
- 详情提供有界最新事件与向上翻页；「对话」按首次接收顺序交错正文、可读思考及工具，结果回填原工具块；「诊断」按发生时刻与 sequence 查 Run、重试、状态和原因。普通详情只展示 Trace 状态，不内嵌原始 Wire，取证用 ZIP。
- 客户端错误按请求列出 failed／rejected，上游错误按实际尝试列出，可查看该 Run 的尝试时序及其恢复、失败、中断或仍在运行结果。存在所属 Interaction 时可跳转，缺失数据不补造节点。
- 自动跟随只在有新用户输入或输出时移动到新活动；用户主动浏览、缩放或选旧节点会暂停，显式恢复才追随。时间线向上读历史不被新消息拉走。
- 深链 `/logs?interaction=<id>` 可定位仍保留的 Interaction，凭据命中可进一步定位 Run 或有证据的块；缺失内容明确提示。无法显示块级命中时定位到 Run，不用相似正文猜位置。无选择不订阅实时正文，错误页不刷新隐藏画布。

### 浏览与实时一致性

画布允许平移、缩放、选择与键盘访问，不允许用户改写节点或因果边。确认执行关系、推断来源与原生压缩关联在诊断中分别显示；跨根的视觉分组不改变后端身份或统计。

历史页可继续向前读取，已有阅读位置不因补入旧事件跳动。固定时间窗不会随新流量漂移；实时预设保持所选时长。
实时首尾快照与按状态更新相互独立：单纯状态、用量、思考或工具活动更新可以刷新卡片，但不抢焦点。

检查器显示实际记录的用量、耗时、交付及错误；未知字段与加载错误均不画成零。
仅为当前选择维护实时正文，选中或重连时先同步快照；普通对话和诊断不需要开 Debug。

客户端错误列表按请求起时倒序；上游错误按尝试开始时间倒序。
上游错误的 recovered、caused_failure、in_progress 和 interrupted 从所属 Run 现状派生，不重复记为新请求。
错误筛选的范围与分页与画布各自保留；切换期间不以旧查询结果覆盖新范围。

凭据深链从命中记录显式携带 `highlight=credential-hits`，可向前翻页寻找对应块。
清除高亮或改选后停止高亮；无法定位的命中保留摘要与 Run 入口。
