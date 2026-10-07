---
status: accepted
---

# 精简 Interaction Observation 事件并把 Debug manifest 移出数据库

Interaction Observation 只持久化无法由其他来源给出的事实；Debug Trace 的全部状态元数据移出数据库。本决定部分取代 [ADR-0060](0060-own-interaction-observation-outside-generation-chain.md) 中「数据库只保存 manifest 与索引」的约定，并落地 [ADR-0062](0062-persist-diagnostic-content-at-canonical-item-boundaries.md) 的 Canonical Item 收口持久化；ADR-0060 关于 Observation 所有权、Generation Chain 隔离、保留期与「观察失败不影响推理」的决策不变。

每条 Trace 的 manifest 改为 `data_dir/diagnostics/observation-debug/<trace_id>/manifest.json`，trace_id 即目录名；写入使用临时文件加 rename 原子替换，节奏沿用既有维护周期并在准入与 finish 时写盘。文件以 `FileTraceManifest` 扁平化保存 `TraceManifest` 的公开字段（包括 `schema_version: 2`、`trace_id`、`enabled`、`status`、`reasons`、`bytes_written`、`event_count`），加上 `run_id`/`rejection_id`、`relative_directory`、`created_at`、`completed_at`、`expires_at`、`tombstoned`；不构造独立 `owner` 包装，不保存绝对路径或下载凭据。进程内 `DebugTraceIndex` 在启动时扫描受管目录建立索引、由 Trace writer 增量更新；详情的 `debug_status`/`trace`、失败请求列表的 `debug_status` 与 Bundle 的 `runs[]` 改读索引，公开 API 结构不变。Run 终态事件提交并广播之前，`manifest.json` 必须已写入终态，使收到 `run_finished` 后重新拉取的详情能看到最终状态。索引与 Trace 文件一样只在持有它们的 Gateway 实例可见；这延续 ADR-0060 的单实例承诺，多实例下 Debug 状态只在拥有该 Trace 文件的实例上可见。

生命周期与删除改为纯文件语义：sweep 对已完成且 owner 可回收的 Trace 直接按持久化的 `expires_at` 删除过期目录，不以运行时长重算期限；启动恢复保留既有完成时间与过期时间。运行时扫描遇到单个损坏、不可读、符号链接或目录身份不符的 manifest 时仅记录不含内容或路径的警告并跳过，不覆盖该文件，也不阻止其他 Trace 查询及新捕获；受管根目录不可用仍显式失败。升级导出对已有文件的严格校验不变。「清除 Debug 数据」删除全部目录并清空索引；「清除历史记录」在 purge 之后删除 owner 已不存在的目录；中断的删除先改名为 `.deleting-*` 再由启动补完，没有 manifest 的孤儿目录照旧回收。随之删除 `debug_trace_manifests` 表及其索引与守卫、`trace_manifest_updated` 事件（含历史行），以及只写不读的 `rejected_request_observations.debug_status` 列；`debug_enabled` 保留，它是准入时的开关快照而非 Debug 内容。

manifest 的路径校验、临时文件写入、`sync_all`、关闭与原子替换在单个 blocking 任务中完成，避免在异步执行器上关闭文件或逐操作调度。已完成 Trace 不再发布执行中快照；重复的相同终态只有先前确实写入成功才跳过，变化或失败仍写入。终态后的补充状态保留首次完成时间与既有过期时间，不因延迟更新续期。准入发布 manifest 与 Wire writer 可并发创建同一受管目录；后者仅接纳同根下真实非符号链接目录，分段文件仍用 `create_new`，不覆盖既有内容。

升级顺序由 [ADR-0076](0076-deduplicate-turn-chain-items-and-share-binary-storage-codec.md) 定义：SQL 0007 与历史转换之后、SQL 0008 之前调用 `crate::interaction_observation::upgrade::export_debug_manifests_sqlite/postgres(conn, Option<&Path>)`，把旧 manifest 导出；SQL 0008 之后调用 `convert_event_storage_sqlite/postgres(conn)` 分短事务批次无损转换旧事件。两组钩子幂等、可中断续跑，既有 payload、顺序及独有诊断事实不因合并而丢弃。旧 `client_output_committed` 信号行删除而不改名为别名；admission 保存原始 sequence 的 `client_output_committed_sequence` 及 `legacy_lifecycle` 原始事实来源，固定截止水位的 Bundle 只在该 sequence 不超过 through-sequence 时应用提交事实。终态后迟到提交通过更高 sequence 的 `run_finished` 修订承载，保留原 finished_at。旧封块文本不伪造 Canonical Item 边界：删除旧随机 block_id 与合成 item，原元数据保存在 `legacy_text`，按旧 scope 与有序 parts 连续还原，不插入虚构空行。导出前已有 manifest 文件必须与数据库权威事实匹配，严格校验成功后才可删表。导出钩子把现存 `debug_trace_manifests` 行写成 `manifest.json`，随后才执行 DROP。表非空而宿主未提供诊断目录时迁移显式失败，不静默丢弃诊断状态；schema exporter 在空表上不受影响，离线副本由 `migrate-data`/`optimize` 路径携带真实根目录完成同样导出。

manifest 每批最多 200 条，在最多 8 个、随可用处理器确定的 blocking worker 中并行导出；错误先收口本批已启动的任务，再向上传播。旧事件批量并行解码，生命周期合并和数据库提交按原 sequence 串行执行；同批后续旧行如果已被合并修改，重新读取权威 payload，不能覆盖新增事实。转换期间为旧表建立 `(run_id, sequence)` 索引，并在 SQL 中过滤合并目标 kind，避免每条生命周期事实扫描全部旧事件或解码同 Run 的大段无关文本。旧表清理同时移除临时索引，不改变最终 schema 或已有 migration checksum。

普通 Observation 事件同步收敛：

实时正文的内部 source part 身份使用有序的 `(is_reasoning_content, index)`，摘要先于 reasoning content，原始索引不借位、不截断。该身份适用于 native 与 wasm32；对外仍发布 Canonical Item 的有序正文与既有 block 身份，不把内部键作为新协议字段。

普通 thinking 快照只保留可读 Thinking/Reasoning 正文和 Canonical graph 元数据：`id`、`__open_responses_item_reference`、`status`、`provenance`、`audience`。签名、密文、受保护 wire snapshot 与未知扩展不进入普通诊断；完整历史仍遵循无损存储契约。非 thinking 与已发布的 Item 在构造快照前排除，不复制随后会丢弃的整项正文。

- 仅作信号的事件不再作为独立事件落库：`generation_associated` 并入 admit 事务内的投影写入；`client_output_committed` 只保留 Run 投影列更新；`process_restarted` 不再单独持久化，改由启动恢复在同一事务内修正 run 投影并以 `run_state_changed`（`{"status":"interrupted","reason":"process_restarted"}`）承载，保持 SSE 唤醒与 Bundle 回放可见；`trace_manifest_updated` 删除。
- 生命周期合并：`usage_confirmed` 并入 `target_attempt_finished` 的 `usage` 字段——attempt 结束前已确认的用量随结束事件落库，失败或中断 attempt 已实际报告的部分用量同样保留，不再持久化独立 `usage_confirmed`。`delivery_finished` 并入 `run_finished` 的 `delivery` 字段（`status`/`reason`/`completed_at`），Run 投影增加 `delivery_completed_at`；早于终态的 usage/delivery 先更新投影，终态合并时保留已收到的部分事实，不因失败或取消清空。用量时间线与 SSE 在实际 `target_attempt_finished` 时显示合并结果；迟到事实以更高 sequence 的同 kind 终态修订承载，不新增独立 usage 事件或易失 usage/reset 协议。`client_output_committed` 布尔投影在终态事务内提交；删除信号事件不引入 `projection_updated` 别名或每事件完整快照。
- 诊断正文按 Canonical Item 持久化（实现 ADR-0062）：可读 thinking 与客户端可见内容每个 item 一行，新 kind 为 `model_thinking` 与 `client_visible_content`，payload 携带 item 身份、可选 `block_id`、项内 part 序列、汇总 text 与 `complete` 标记，`model_thinking` 另带 model_turn_id/attempt_id。`model_thinking_finished` 不再持久化；可处理的失败或取消把已实际收到的内容标为未完成，不补造未收到的尾部；不周期性写中间快照，进程崩溃允许丢失整个未收口 item，只留 `observation_gap`。未收口内容继续作为选中作用域的累计易失快照发布，首段与结束边界立即处理，中间修订遵循下述有界调度，不带 SSE ID、不推进持久 cursor。实时与持久正文使用同一 Canonical Item 身份构造 `block_id`：客户端内容在 Run 内、thinking 在 attempt 内保持真实 item 顺序身份，身份不因上游 item id 迟到而改变；不同 item 不合并，项内 parts 按 canonical 顺序呈现，实时修订保留累计正文与单调 revision。延迟 WebSocket 的可见内容只从已成功发送给客户端的协议帧累积；可处理的失败、取消或断线只收口已交付的部分并标记 `complete=false`，不把未发送的 provider 前缀或已暂存完整响应补入可见正文。
- `client_tool_result` 首先以 Generation Chain 的核验结果为准：父节点已核验时只从该节点的 client delta 捕获本次新增的工具结果，不再回放保存整段窗口；不复制父节点完整窗口；没有已核验父节点时不从父历史推断工具结果。客户端省略 thinking-only 输出导致父节点退回时，Observation 还可用同 Principal、未过期的工具收据和完整收到输入的 canonical 严格前缀证明旧结果回放，新增后缀必须只有普通 User 输入；User 内的 ToolResult 不满足此条件。成立时跳过该 Run 的重复结果捕获并排除当前工具／pending-tool 归并优先级，不改变模型输入或 Generation 父边。收到输入摘要使用有界易失索引，重启后仅从已保存 Generation 的输入边界恢复证明；完整证据不可用时不猜测。独立 sibling、结果变化和已有输入变化的分支仍保留各自收据，不按工具 ID 全局消费。
- `observation_events.payload` 与 Turn Chain 存储共用同一二进制 storage codec（布局见 [ADR-0076](0076-deduplicate-turn-chain-items-and-share-binary-storage-codec.md)：固定 14 字节 trailer，codec、零 dict_id、原始长度与版本；至少 128B 且 body 更小时采用带 checksum 的 zstd-3，线程本地上下文复用），SQL 查询用到的 JSON 字段提升为列：`tool_id`（client_tool_handoff/client_tool_result）与 `operation_id`（compaction_operation），其余内容不留列。旧 zip 文本编码在转换时一次性解码，不再产生。索引同步收敛：删除未被查询使用的 `observation_events_expiry_idx`；`observation_events_rejection_idx` 改为 `WHERE rejection_id IS NOT NULL` 部分索引；工具调用索引由 `json_extract` 表达式改为 `(run_id, tool_id, sequence DESC)` 部分索引。

## 有界实时通信补充决策

本补充明确替代原「未收口内容立即实时发布」的每次中间修订即时承诺：后端以固定首次截止的最多 100ms 窗口发布变化 block 的最新累计修订，第一段及完成、失败、取消边界立即处理；连续输入不能滚动延后截止。只替换易失预览，慢消费者的旧 block 修订可由最新修订替代，持久变化通知和 gap/恢复信号不得被正文积压吞掉。全文构造与发送编码不在每个原始 delta 上执行，推理和下游转发不等待观察调度。

五项降本一并实施：按 root 合并摘要刷新，并分别合并详情增量与失败列表；批量读取 Debug flags；真正限制易失正文发布；分离全局轻量通知与选中 Interaction 正文；常规按 root 返回变化节点和明确移除结果。全局 `observation` 使用安全元数据 `ObservationChange`，保留持久 sequence，但不广播正文或完整 payload；选中 `/interactions/{id}/live` 先给当前累计快照，再无遗漏订阅，并以无持久 ID 的 `live_finished` 立即收口展示。Canonical Item 持久化、稳定 block 身份、part 顺序、累计正文、单调 revision、完整生命周期事实与易失/持久水位区别均不变。

公开管理 `/interactions/{id}/summary` 及前端客户端接口干净删除，不保留 alias；选中及失败请求跳转读取 Interaction detail 并使用既有 root reveal。内部 store summary 保留为选中详情的组成，不再作为独立 HTTP 消费契约。

ForestQuery 增加可选且默认关闭的 `live_window`；实时 forest/changes 仍验证时间跨度，但不以查询上界排除时钟差或在途新增活动，历史固定窗口保持上界。root 移除原因按 `deleted > filter > window` 判定；已知历史 root 仅迁出窗口时仍返回变化摘要、只移除物理缺失成员，筛选失效则不能沿迁出语义保留。未知窗口外 root 不返回摘要。

持久事件 allocator/sequence 单调且已提交 ID 不重用，清历史或到期回收不重置分配器；查询 `snapshot_sequence` 是同一读取事务内仍保留的已提交事实水位，不是分配器位置。forest、root changes 与详情摘要使用一致数据库快照，以保留事件的已提交 MAX 为水位，空历史为 0；PostgreSQL 非事务 `last_value` 可包含未提交分配，绝不能作为视图水位。清理或 reset 后的 fresh snapshot 可以降低，不承诺跨重建单调。前端通过新 view epoch 重建并拒绝旧 epoch 结果，在成功前保留可用旧数据及明确恢复状态，失败显式 Retry；成功接受较低/零权威水位并替换旧基线，不保留幽灵节点。这不改变持久事实排序、正文 revision 或 Canonical Item 边界。

Workspace 最多合并等待 100ms，`StreamingMarkdown` 最多追赶 300ms，与后端 100ms 共用最多 500ms 主动调度等待预算；外部 I/O、计算和实际渲染服务时间另测，不宣称端到端 500ms。首尾立即，历史/重连快照、替换、终态与 reduced-motion 直接显示已收到内容。动画只覆盖选中正文、展开 Thinking 与复用同一订阅的选中卡片输出。

差量基线包含成员水位、matched 与 Debug 状态；必须更新受 root 筛选影响的兄弟状态，区分删除、筛选退出和时间窗迁移，不重发未变化兄弟。无法证明差量或重放完整时显式 reset 并重建权威视图。请求在途变化、查询失败的待处理状态与可见恢复、selection epoch、分页和历史迁出遵循 [观测设计 §9–10](../design/interaction-observation.md)。批量 Debug 判定仍读本实例文件索引，不变成共享事实或 TTL 缓存。此次补充不新增依赖或 schema，不改变认证、脱敏、Wire Capture、Bundle、Confirmed Upstream Usage、失败分类、单实例范围或「观察失败不影响推理」。

## Considered options

- **manifest 留在数据库。** 备份最简单，但 manifest 是单实例本地文件的镜像：每条 Trace 的周期性与生命周期写入构成观察 SQL 的最大单一来源（实测窗口内约占累计观察写入的 14%），把纯本实例状态放进共享表没有任何读者收益。
- **生命周期事件继续激进投影化**（连同 model turn/attempt/usage 事件一起删除）。需要重做 SSE 水位与 UI 时间线，风险大于收益；只采纳保守合并。
- **工具与思考正文只放 Debug。** Wire 捕获确实已含这些正文，但管理面详情需要改为解析 Trace 分段，与检查器的分层详情契约冲突；决定保留在普通存储并压缩。
- **保留旧格式读取器一段时间的双路径。** 事件投影是易失诊断数据而非历史事实源，clean cutover 配合导出钩子已能无损迁移；不为诊断投影维护两套读路径。

## Consequences

- SQLite 与 PostgreSQL 各获得一个增量 migration（0008），基线 schema 不受影响。升级前必须备份完整数据根及外部数据库；回退方式是恢复备份，旧二进制不能读取新 schema。迁移把现存事件 payload 解码旧编码后以新 codec 重写，属于一次性数据转换。
- 事件行数与字节估算均来自单一真实快照的离线重算，是估计值而非保证；合入验收采用配对 HTTP 回放，比较首 Token 与请求耗时的 p50/p99 及 SQL 开销，并隔离检查历史物化耗时；要求没有可重复复现的回退，而不是容忍固定百分比。这里定义验收门禁，不声明已执行基准或回放。
- `docs/database/*.sql` 仍由 `stravia-tools dump-schema` 从全部迁移重新生成，代表迁移后的最终形态；多实例部署继续不共享 Debug 状态与实时唤醒。
