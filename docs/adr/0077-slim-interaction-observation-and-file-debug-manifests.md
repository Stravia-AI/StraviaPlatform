---
status: accepted
---

# 精简 Interaction Observation 事件并把 Debug manifest 移出数据库

Interaction Observation 只持久化无法由其他来源给出的事实；Debug Trace 的全部状态元数据移出数据库。本决定部分取代 [ADR-0060](0060-own-interaction-observation-outside-generation-chain.md) 中「数据库只保存 manifest 与索引」的约定，并落地 [ADR-0062](0062-persist-diagnostic-content-at-canonical-item-boundaries.md) 的 Canonical Item 收口持久化；ADR-0060 关于 Observation 所有权、Generation Chain 隔离、保留期与「观察失败不影响推理」的决策不变。

每条 Trace 的 manifest 改为 `data_dir/diagnostics/observation-debug/<trace_id>/manifest.json`，trace_id 即目录名；写入使用临时文件加 rename 原子替换，节奏沿用既有维护周期并在准入与 finish 时写盘。文件以 `FileTraceManifest` 扁平化保存 `TraceManifest` 的公开字段（包括 `schema_version: 2`、`trace_id`、`enabled`、`status`、`reasons`、`bytes_written`、`event_count`），加上 `run_id`/`rejection_id`、`relative_directory`、`created_at`、`completed_at`、`expires_at`、`tombstoned`；不构造独立 `owner` 包装，不保存绝对路径或下载凭据。进程内 `DebugTraceIndex` 在启动时扫描受管目录建立索引、由 Trace writer 增量更新；详情的 `debug_status`/`trace`、失败请求列表的 `debug_status` 与 Bundle 的 `runs[]` 改读索引，公开 API 结构不变。Run 终态事件提交并广播之前，`manifest.json` 必须已写入终态，使收到 `run_finished` 后重新拉取的详情能看到最终状态。索引与 Trace 文件一样只在持有它们的 Gateway 实例可见；这延续 ADR-0060 的单实例承诺，多实例下 Debug 状态只在拥有该 Trace 文件的实例上可见。

生命周期与删除改为纯文件语义：sweep 对已完成且 owner 可回收的 Trace 直接按持久化的 `expires_at` 删除过期目录，不以运行时长重算期限；启动恢复保留既有完成时间与过期时间。运行时扫描遇到单个损坏、不可读、符号链接或目录身份不符的 manifest 时仅记录不含内容或路径的警告并跳过，不覆盖该文件，也不阻止其他 Trace 查询及新捕获；受管根目录不可用仍显式失败。升级导出对已有文件的严格校验不变。「清除 Debug 数据」删除全部目录并清空索引；「清除历史记录」在 purge 之后删除 owner 已不存在的目录；中断的删除先改名为 `.deleting-*` 再由启动补完，没有 manifest 的孤儿目录照旧回收。随之删除 `debug_trace_manifests` 表及其索引与守卫、`trace_manifest_updated` 事件（含历史行），以及只写不读的 `rejected_request_observations.debug_status` 列；`debug_enabled` 保留，它是准入时的开关快照而非 Debug 内容。

manifest 的路径校验、临时文件写入、`sync_all`、关闭与原子替换在单个 blocking 任务中完成，避免在异步执行器上关闭文件或逐操作调度。已完成 Trace 不再发布执行中快照；重复的相同终态只有先前确实写入成功才跳过，变化或失败仍写入。终态后的补充状态保留首次完成时间与既有过期时间，不因延迟更新续期。准入发布 manifest 与 Wire writer 可并发创建同一受管目录；后者仅接纳同根下真实非符号链接目录，分段文件仍用 `create_new`，不覆盖既有内容。

升级顺序由 [ADR-0076](0076-deduplicate-turn-chain-items-and-share-binary-storage-codec.md) 定义：SQL 0006 与历史转换之后、SQL 0007 之前调用 `crate::interaction_observation::upgrade::export_debug_manifests_sqlite/postgres(conn, Option<&Path>)`，把旧 manifest 导出；SQL 0007 之后调用 `convert_event_storage_sqlite/postgres(conn)` 分短事务批次无损转换旧事件。两组钩子幂等、可中断续跑，既有 payload、顺序及独有诊断事实不因合并而丢弃。旧 `client_output_committed` 信号行删除而不改名为别名；admission 保存原始 sequence 的 `client_output_committed_sequence` 及 `legacy_lifecycle` 原始事实来源，固定截止水位的 Bundle 只在该 sequence 不超过 through-sequence 时应用提交事实。终态后迟到提交通过更高 sequence 的 `run_finished` 修订承载，保留原 finished_at。旧封块文本不伪造 Canonical Item 边界：删除旧随机 block_id 与合成 item，原元数据保存在 `legacy_text`，按旧 scope 与有序 parts 连续还原，不插入虚构空行。导出前已有 manifest 文件必须与数据库权威事实匹配，严格校验成功后才可删表。导出钩子把现存 `debug_trace_manifests` 行逐条写成 `manifest.json`（仍未完成的 running/writing 状态改写为 partial/`process_interrupted`，因为持有它们的进程已不存在），随后才执行 DROP。表非空而宿主未提供诊断目录时迁移显式失败，不静默丢弃诊断状态；schema exporter 在空表上不受影响，离线副本由 `migrate-data`/`optimize` 路径携带真实根目录完成同样导出。

普通 Observation 事件同步收敛：

实时正文的内部 source part 身份使用有序的 `(is_reasoning_content, index)`，摘要先于 reasoning content，原始索引不借位、不截断。该身份适用于 native 与 wasm32；对外仍发布 Canonical Item 的有序正文与既有 block 身份，不把内部键作为新协议字段。

普通 thinking 快照只保留可读 Thinking/Reasoning 正文和 Canonical graph 元数据：`id`、`__open_responses_item_reference`、`status`、`provenance`、`audience`。签名、密文、受保护 wire snapshot 与未知扩展不进入普通诊断；完整历史仍遵循无损存储契约。非 thinking 与已发布的 Item 在构造快照前排除，不复制随后会丢弃的整项正文。

- 仅作信号的事件不再作为独立事件落库：`generation_associated` 并入 admit 事务内的投影写入；`client_output_committed` 只保留 Run 投影列更新；`process_restarted` 不再单独持久化，改由启动恢复在同一事务内修正 run 投影并以 `run_state_changed`（`{"status":"interrupted","reason":"process_restarted"}`）承载，保持 SSE 唤醒与 Bundle 回放可见；`trace_manifest_updated` 删除。
- 生命周期合并：`usage_confirmed` 并入 `target_attempt_finished` 的 `usage` 字段——attempt 结束前已确认的用量随结束事件落库，失败或中断 attempt 已实际报告的部分用量同样保留，不再持久化独立 `usage_confirmed`。`delivery_finished` 并入 `run_finished` 的 `delivery` 字段（`status`/`reason`/`completed_at`），Run 投影增加 `delivery_completed_at`；早于终态的 usage/delivery 先更新投影，终态合并时保留已收到的部分事实，不因失败或取消清空。用量时间线与 SSE 在实际 `target_attempt_finished` 时显示合并结果；迟到事实以更高 sequence 的同 kind 终态修订承载，不新增独立 usage 事件或易失 usage/reset 协议。`client_output_committed` 布尔投影在终态事务内提交；删除信号事件不引入 `projection_updated` 别名或每事件完整快照。
- 诊断正文按 Canonical Item 持久化（实现 ADR-0062）：可读 thinking 与客户端可见内容每个 item 一行，新 kind 为 `model_thinking` 与 `client_visible_content`，payload 携带 item 身份、可选 `block_id`、项内 part 序列、汇总 text 与 `complete` 标记，`model_thinking` 另带 model_turn_id/attempt_id。`model_thinking_finished` 不再持久化；可处理的失败或取消把已实际收到的内容标为未完成，不补造未收到的尾部；不周期性写中间快照，进程崩溃允许丢失整个未收口 item，只留 `observation_gap`。未收口内容继续作为易失快照立即实时发布，不带 SSE ID、不推进持久 cursor。实时与持久正文使用同一 Canonical Item 身份构造 `block_id`：客户端内容在 Run 内、thinking 在 attempt 内保持真实 item 顺序身份，身份不因上游 item id 迟到而改变；不同 item 不合并，项内 parts 按 canonical 顺序呈现，实时修订保留累计正文与单调 revision。延迟 WebSocket 的可见内容只从已成功发送给客户端的协议帧累积；可处理的失败、取消或断线只收口已交付的部分并标记 `complete=false`，不把未发送的 provider 前缀或已暂存完整响应补入可见正文。
- `client_tool_result` 去重改以 Generation Chain 为准：父节点已核验时只从该节点的 client delta 捕获本次新增的工具结果，不再回放保存整段窗口；不复制父节点完整窗口；没有已核验父节点时不从父历史推断工具结果。
- `observation_events.payload` 与 Turn Chain 存储共用同一二进制 storage codec（布局见 [ADR-0076](0076-deduplicate-turn-chain-items-and-share-binary-storage-codec.md)：固定 14 字节 trailer，codec、零 dict_id、原始长度与版本；至少 128B 且 body 更小时采用带 checksum 的 zstd-3，线程本地上下文复用），SQL 查询用到的 JSON 字段提升为列：`tool_id`（client_tool_handoff/client_tool_result）与 `operation_id`（compaction_operation），其余内容不留列。旧 zip 文本编码在转换时一次性解码，不再产生。索引同步收敛：删除未被查询使用的 `observation_events_expiry_idx`；`observation_events_rejection_idx` 改为 `WHERE rejection_id IS NOT NULL` 部分索引；工具调用索引由 `json_extract` 表达式改为 `(run_id, tool_id, sequence DESC)` 部分索引。

## Considered options

- **manifest 留在数据库。** 备份最简单，但 manifest 是单实例本地文件的镜像：每条 Trace 的周期性与生命周期写入构成观察 SQL 的最大单一来源（实测窗口内约占累计观察写入的 14%），把纯本实例状态放进共享表没有任何读者收益。
- **生命周期事件继续激进投影化**（连同 model turn/attempt/usage 事件一起删除）。需要重做 SSE 水位与 UI 时间线，风险大于收益；只采纳保守合并。
- **工具与思考正文只放 Debug。** Wire 捕获确实已含这些正文，但管理面详情需要改为解析 Trace 分段，与检查器的分层详情契约冲突；决定保留在普通存储并压缩。
- **保留旧格式读取器一段时间的双路径。** 事件投影是易失诊断数据而非历史事实源，clean cutover 配合导出钩子已能无损迁移；不为诊断投影维护两套读路径。

## Consequences

- SQLite 与 PostgreSQL 各获得一个增量 migration（0007），基线 schema 不受影响。升级前必须备份完整数据根及外部数据库；回退方式是恢复备份，旧二进制不能读取新 schema。迁移把现存事件 payload 解码旧编码后以新 codec 重写，属于一次性数据转换。
- 事件行数与字节估算均来自单一真实快照的离线重算，是估计值而非保证；合入验收采用配对 HTTP 回放，比较首 Token 与请求耗时的 p50/p99 及 SQL 开销，并隔离检查历史物化耗时；要求没有可重复复现的回退，而不是容忍固定百分比。这里定义验收门禁，不声明已执行基准或回放。
- `docs/database/*.sql` 仍由 `stravia-tools dump-schema` 从全部迁移重新生成，代表迁移后的最终形态；多实例部署继续不共享 Debug 状态与实时唤醒。
