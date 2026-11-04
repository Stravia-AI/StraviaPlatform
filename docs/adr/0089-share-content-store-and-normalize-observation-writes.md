---
status: accepted
---

# 共享正文存储、按引用回收依赖并规范化 Interaction Observation 写入

Interaction Observation、Turn Chain 与 History Marker 各自保存同一段正文；两个域又各把一套到期时间写进每一行；Observation 写入按 writer 命令各开事务，队列空时一条事件就是一个事务。本决定让三方共享一张按 Principal 隔离、内容寻址的正文表，正文只存一份；Turn Chain 与 Observation 共用管理员可调的保留天数，只有 Interaction、拒绝请求与 Turn 节点计时，Marker、可逆脱敏映射与原生压缩登记按 Turn 节点的引用保留，扫描是唯一的删除方；Observation 事件表只记录变更顺序，状态归投影列、正文归内容表，并按执行阶段合并写入事务。这样既消除冗余副本和截断，又不让任何一方拥有另一方的语义或生命周期。

依据来自本地开发库的只读统计与代码阅读，不代表生产负载：跨协议思考正文最多同时存在 5 份（Observation 行内 `text`/`parts`/`item` 3 份、Turn 投影 1 份、Marker 1 份）；`client_tool_result` 168,128 行中 81% 与已有行重复，99.9% 与 Turn Chain 已存的工具结果正文逐字节相同；按字符串比较，256B 以上的思考正文 94%、可见正文 96% 能在 Turn Chain 或 Marker 中找到同一份，用户输入 97.6% 与 Turn 已存正文相同。每个 Inference Run 平均写 12.0 条事件、约 10 个事务；SQLite 使用默认的 `synchronous=FULL`，它与 PostgreSQL 都在每次提交时同步 WAL。

实施状态：已实施。共享内容存储与引用保留已落地为 SQLite/PostgreSQL 迁移 0037–0040（含 Rust 升级钩子），Observation 规范化与阶段化写入为 0041/0042，读取接口与 WebUI 同步更新，`docs/database/*.sql` 由升级一节的 `dump-schema` 命令重新生成。本 ADR 其余章节写的是决定；实现中补充的具体约定见文末「实施说明」，实测数字见「实施后实测」，未填写处表示尚未测量。被取代的既有决定已在落地时同步修订，见「与既有决策的关系」。

## 共享内容存储

`turn_contents`、`turn_content_gc` 与 Principal 字典 `turn_scopes` 从 `turn_chain` 迁出，成为独立的 crate-private 内容存储模块，表名改为 `contents`、`content_gc`、`content_scopes`。Turn Chain、History Marker 与 Observation 是同级使用方；内容存储只按引用判活，不理解任何一方的语义，也不保存到期时间。

- `contents` 的列：不复用的数值 `id`、`scope_id`、32 字节 `digest`、`format`（0 = UTF-8 文本，1 = 紧凑 JSON）、`raw_len`、`body`；唯一键 `(scope_id, digest)`。`body` 经 `storage_codec` 编码，压缩只发生在这一层；有了 `raw_len`，展示长度和返回摘要都不必解码。
- 内容身份改为 `SHA256(b"stravia.content.v1\0" + format + raw_bytes)`。`format` 计入摘要，字节相同的文本与 JSON 不会合并。文本单元保存原始 UTF-8，不做 JSON 转义，读出即字符串，不经过 `serde_json`。
- 引用表统一为 `(owner, ordinal) → content_id`，外键 `RESTRICT`，`content_id` 有反向索引：已有的 `turn_node_contents`，以及新增的 `history_marker_contents(reference, ordinal, content_id)`、`observation_event_contents(sequence, ordinal, content_id)` 与 `observation_tool_results(sequence, ordinal, tool_id, is_error, content_id)`。
- 引用方删除引用时，在同一事务内把失去最后引用的内容登记进 `content_gc`（Observation 的到期回收与「清除历史记录」`purge_clear_rows` 同样如此，否则只被 Observation 引用的正文永远不会回收）；最终删除在锁协议内重检四个反向引用均不存在。批次数量与字节上限沿用 [ADR-0076](0076-deduplicate-turn-chain-items-and-share-binary-storage-codec.md)。
- 写入方没有固定先后角色：先到的一方插入正文，其余方按摘要命中已有行，只写引用，不再压缩。Observation 通常在 item 收口时最先写入，Marker 交付与 Turn 提交多数只登记引用。摘要命中只是提示，不是权威，事务内重检。
- Observation 取所属 Interaction 的 Principal 对应的 scope：`interaction_observations.principal` 保存的是 API key id，scope 键为 `api-key:<id>`，与 Turn Chain、History Marker 使用的 `Principal::continuation_key()` 是同一个键（换算只在 `interaction_observation/content_refs.rs::scope_key` 一处，写入、读取与升级都经它），三方才落在同一 scope、同一段正文只存一份。Observation 先写入的正文只是可按摘要复用的字节：Turn 提交仍自行计算摘要、核验并登记引用，Observation 丢失或被清理不影响历史；[ADR-0060](0060-own-interaction-observation-outside-generation-chain.md) 的所有权边界与「Observation 不是执行事实源」不变。

## 正文单元与三方表示

哪些值算正文，由 `stravia-runtime-contract` canonical 投影旁的单个函数按 IR 类型定义：`ContentBlock::Text.text`、`MessageContent::Text`、`ContentBlock::Thinking.thinking`、`ContentBlock::Reasoning` 的 `summary`/`content` 各元素，以及 `ToolResult`/`ServerToolResult` 的 `content`（字符串为文本单元，其他 JSON 为 JSON 单元）。三方只调用这一个定义，不再用 `ITEM_ARRAY_KEYS`/`SCALAR_KEYS` 等键名推断正文；item、system、tools 等外层 JSON 单元的切分沿用 ADR-0076。正文单元不设大小门槛，空字符串同样寻址，这与 ADR-0076 取消 256B 分流的决定一致。

外壳各存各的，只共享正文。开发库中能与 Turn 对上的 Observation item，`meta.id` 没有一条相同，Turn 还多出 `provenance`、`audience`、`status` 等 meta 键；Observation 写入时只保留 5 个 meta 键，并去掉 `signature`/`encrypted_content`。因此按块或按 item 共享不可行。

- **Turn Chain**：descriptor 槽位增加格式标记。`[pointer, ordinal]` 为 JSON 单元，`[pointer, ordinal, "t"]` 为文本单元，拼接槽位 `[pointer, ordinal, "t", prefix, suffix]` 的值为 `prefix + 正文 + suffix`。item 外壳是 JSON 单元，正文是嵌套的文本槽位，父容器先于嵌套槽恢复（现有规则）。跨协议投影文本在写入时用 `history_marker::syntax` 拆成两段字面量与正文，先校验拼回后逐字节相同，不同则整串存为一个文本单元；读取得到的就是当初交付的字节，不重新渲染。公开 TurnNodeId 与 `payload_version` 不变。
- **History Marker**：`segment_payload` 由未压缩 TEXT 改为 `storage_codec` 编码的外壳，`signature`、`encrypted_content` 留在外壳内，正文经 `history_marker_contents` 引用。Marker 仍是 hidden payload 的唯一事实源（[ADR-0028](0028-persist-hidden-history-behind-markers.md)）；Turn Chain 仍只保存 Marker reference 与自己的投影文本。
- **Observation**：`model_thinking` 与 `client_visible_content` 只保存块元数据和 item 外壳（`retain_diagnostic_metadata` 的现有裁剪不变），再加正文引用；`text`、`parts` 在读取时推导。

结果：跨协议思考正文由 5 份变为 1 份，同协议思考正文与可见正文由 4 份变为 1 份。

## 统一保留期：根对象计时，依赖按引用保留

Turn Chain 与 Observation 共用 `log_retention_days`（0024 迁移的触发器已读取此键），界面称「历史保留」。后端最小值由 0 改为 1，因为共用后 0 会让所有续接在下一次扫描时失效。`GatewayBuilder::generation_chain_ttl`、`Gateway::new` 中固定的 7 天与原生压缩登记的 `RETENTION` 删除，各域读取同一份内存中的天数 N。

只有根对象计时，用的都是已有的列。Marker、可逆脱敏映射与原生压缩登记（下称依赖）不保存到期时间：被 Turn 节点引用时不回收，没有引用后才按下限窗口回收。

|记录|保留规则（N 为当前天数）|
|---|---|
|Interaction|`last_active_at + N` 后回收；`running` 不回收（现状）|
|Inference Run、observation event|随所属 Interaction 删除；事件不再按自身 `occurred_at` 过滤|
|拒绝请求|`occurred_at + N` 后回收|
|Turn 节点|`created_at + N` 已过、没有子节点、且不是任何原生登记的来源时回收；最后一个子节点删除后父节点由触发器重新入队|
|依赖，被任一节点引用|不回收|
|依赖，未被引用且未发布或未确认|绝对期限 `pending_until`；一小时规则不变，不随 N 变化|
|依赖，未被引用且已发布或已确认|首次发布或确认时刻 `+ N`；覆盖已交付但提交失败、客户端稍后带回的情形|

Turn 节点写入后不再更新时间。节点 X 的子树中只要还有一个叶子未到期，X 到该叶子路径上的每个节点都有子节点，都不会被回收；子树全部到期后才自下而上删除。所以 X 实际保留到子树中最晚的 `created_at + N`，与续期祖先的结果相同，但提交不再写祖先。

**引用表。** 新增三张表，沿用 `turn_node_contents` 的外键模式：节点侧 `ON DELETE CASCADE`，依赖侧 `RESTRICT` 并建反向索引。引用行与节点在同一事务写入，此后不变；引用集合从本次提交的数据机械得出，不需要判断「这次该给谁续期」。

|表|引用来源|
|---|---|
|`turn_node_markers(node_pk, reference)`|节点请求增量与响应 item 中的 Marker reference，含跨协议投影文本中的 Marker 语法与 Responses reasoning `encrypted_content` 中的 Marker 令牌（ADR-0088）。祖先窗口中的 Marker 已由祖先节点引用，提交不再解析整个父窗口|
|`turn_node_redaction_mappings(node_pk, reference)`|本次请求登记、复用或还原的映射，即现在 `persist_inner` 续期的集合。ADR-0043 说明 Generation Chain 可能保存原文而不是占位符，所以只能在提交时取自请求，不能从载荷反推|
|`turn_node_compactions(node_pk, record_id)`|节点保存的 `compaction_record_ids`；登记之间的来源边沿用 `native_compaction_sources` 的 `RESTRICT`|

提交在事务内按 Principal 锁定被引用的依赖行，再写引用行；客户端提交的 reference 不存在时不登记（现状），是否已发布不影响登记。被引用的依赖必须先于节点存在，原生登记的来源节点又必须先于登记存在，引用行写入后不再增加，所以引用关系不会成环。`persist_inner` 中三次各自独立事务的 `extend_retention`、原生登记对 `expires_at` 与来源节点的延长、提交时对祖先 `expires_at` 的 UPDATE 全部删除；节点与它的引用原子提交，不再出现「依赖已续期、节点未保存」的中间状态。

依赖的 `expires_at` 改名为 `pending_until`，只在未发布或未确认时有值，发布或确认时置空；CHECK 约束保证二者一致，已发布的行上不再有可被误读的到期时间。原生登记的 `delivered_at` 与 `referenced_at` 只为续期而区分，合并为 `confirmed_at`，即首次交付确认或被客户端回传解析的时刻。

**扫描是唯一的删除方。** 读取不带时间条件：Turn 只排除 `deleting`，Observation 与依赖只看行是否存在；Marker 仍拒绝 Platform Tool Execution 已到期的记录，存在执行记录时也不回收（现状，执行记录的期限不在本决定范围）。保证由「到期立即不可读」改为「至少保留 N 天；不再使用的记录到期后，在一个扫描周期内删除」。到期但尚未删除的记录仍可读、可续接、可被新节点引用，被引用后继续保留。

扫描每小时一次（`STORE_SWEEP_INTERVAL`），顺序改为 Observation、Turn 节点、依赖、正文：先删引用方，同一轮释放的依赖与正文在同一轮删除。入口 `HistoryMaintenance::sweep_history` 把 Turn 节点、Marker、映射与原生登记四步作为一轮重复执行，直到某一轮没有删除任何行才进入正文回收：被删原生登记的来源节点、节点释放的 Marker 与映射、由此再释放的来源登记，都会在同一次扫描内收完，不留到下一个周期（每轮至少删除一行，所以必然终止；某一轮出现失败则不再重复）。正文回收每批最多 512 行或 8 MiB，逐批提交并释放写协调，在一次调用内排空队列。每一步都会被尝试：某一步失败只记录告警，其余步骤（尤其是依赖已成功回收之后的正文回收）照常执行，最后合并各步错误一并返回。`turn_node_retention` 只保留 `deleting`。

Turn 节点与依赖只有一条回收路径：两个入口把候选放进各自的回收队列，回收步骤是唯一的删除方。一行在「过了下限窗口」与「失去最后一个引用」中较晚发生的那一刻入队，所以一生只被检查常数次；每轮成本取决于本轮到期或刚失去引用的行数，不随保留的历史增长。

- **期限跨越扫描。** 每个期限列一个持久化水位 `W`：Turn 节点的 `created_at`、依赖的 `published_at` 与 `confirmed_at`（阈值 `T = now − N`），以及 `pending_until`（阈值 `T = now`）。每轮只扫期限落在 `(W, T]` 的行，没有引用的入队，并在同一事务把 `W` 设为 `T`。`T < W`（N 变大或时钟回拨）时只把 `W` 退回 `T`，中间的行以后再扫一次，不影响正确性；多实例并发时每次都从读到的 `W` 开始，最多重扫，不会遗漏。Turn 节点使用新增的 `turn_nodes(created_at, node_pk)` 索引。
- **引用归零入队。** 每张引用表挂 `AFTER DELETE` 触发器，删掉某个对象的最后一条引用时把它放进回收队列：子节点删除时入队父节点（取代 `turn_chain/gc.rs` 中由代码做的父节点回队），三张节点引用表、`native_compaction_sources`、原生登记指向的来源节点、Platform Tool Execution 指向的 Marker 同理。外键级联删除同样会触发（已在 SQLite 3.53.3 与 PostgreSQL 16.15 上验证），所以任何删除引用的代码路径都不必记得入队。正文的 `content_gc` 沿用现有的批量入队，不在此改动。
- **回收。** 从队列领取候选，加锁重检「按当前 N 已过下限窗口、没有任何引用」：满足即删除，否则移出队列。尚未到期的会由跨越扫描再次放入，仍被引用的会在最后一条引用删除时由触发器再次放入；入队后又被新节点引用的行由这次重检拦下。原生登记删除时在同一事务删除它自己的来源边（现状）。

修改 N 不重写任何行，`update_retention` 的四条全表 UPDATE 删除；保存设置后立即触发一次扫描，不等下一个周期。物化缓存以头节点 `created_at + N` 为有效期，命中时按当前 N 计算，在此之前头节点及其祖先都不可能被回收。提交仍在事务内沿祖先链重检每个节点存在且不在 `deleting`，只去掉到期条件与续期 UPDATE。Debug Trace manifest 保持 [ADR-0077](0077-slim-interaction-observation-and-file-debug-manifests.md) 的文件语义。

`observation_pending_tools`、`observation_tail_sources`、Platform Tool Execution（0024 触发器）、媒体派生、Artifact 与 MCP 读取等按各自绝对期限保留的记录不在本决定范围内，保持现状。

## Observation 规范化结构

每个事实只存一处：状态和用量在投影列，正文在内容表，事件表只记录变更顺序，供 SSE 游标、查询快照水位与 Bundle 截止序号使用。

```text
observation_events
  sequence PK, occurred_at, interaction_id, run_id, rejection_id, kind,
  tool_id, operation_id,
  attrs BLOB NULL               -- storage_codec；只放该 kind 独有的小属性
observation_blocks              -- thinking / visible 块
  sequence PK → observation_events, interaction_id, run_id, model_turn_id,
  attempt_id, item_ordinal, channel (thinking|visible), complete, envelope NULL
  INDEX (interaction_id, channel, sequence DESC)
observation_event_contents (sequence, ordinal, content_id)
observation_tool_results   (sequence, ordinal, tool_id, is_error, content_id)
  INDEX (tool_id, sequence)
interaction_observations      - input_preview, visible_tail, expires_at
                              + input_sequence  -- 根 Run 最新的 user_input_recorded
inference_run_observations    - expires_at
                              + inferred_retry, grouping_reason,
                                diagnostic_source_run_id, has_new_user, superseded_by
rejected_request_observations - expires_at
```

|kind|新形态|
|---|---|
|`model_thinking`、`client_visible_content`|块元数据进 `observation_blocks`，item 外壳进 `envelope`，正文进引用；`attrs` 为 NULL|
|`input_preview_recorded` → `user_input_recorded`|保存最新用户 item 的有序文本单元引用，读取时以 `\n` 连接，不截断；不保留旧名或别名|
|`client_tool_result`|每批一行事件，各条结果进 `observation_tool_results`，按 `tool_id` 的查询改走子表索引；每个 Run 的收据行仍保留，满足 ADR-0077「兄弟分支各自保留收据」|
|`run_admitted`|不带 payload；原先只在 payload 中的 4 个字段成为 Run 列|
|`request_failed`|不带 payload；失败信息已在 `failure_json`|
|`run_state_changed`|不带 payload，`superseded_by` 成为 Run 列；同一事务内有其他事件时不写这一行，由该事务的事件触发 SSE 通知|
|`model_turn_started`|同一行携带首个 Target attempt 的开始，不再为它单独写 `target_attempt_started`；后续 failover attempt 仍各自写|
|`model_turn_finished`|同一行携带最后一个 attempt 的结束与用量，进用量事务；其余 attempt 仍各自写 `target_attempt_finished`|
|其他 kind|payload 原样迁入 `attrs`|

开发库 99.9% 的 Model Turn 只有一个 attempt（30,252 次尝试对应 30,227 个轮次）。`model_turn_observations` 与 `target_attempt_observations` 仍是状态与用量的权威，事件合并不改变投影。

## 写入：按阶段合并事务并精简事务内语句

**按阶段提交。** 阶段边界由生产方（`RunObserver`）在已有同步点确定，每个阶段向 writer 发送一个命令、写一个事务。不引入等待窗口或定时器；不在同一同步点产生的写入，不强行合并。

|阶段|同一事务内的写入|
|---|---|
|准入|Admit、状态变化、客户端工具结果；Admit 附带的父链缺口（`generation_parent_observation_unavailable`）与 Retained Tail 诊断事件在同一事务内、`run_admitted` 之后按序写入（`ObservationStore::admit` 的 `follow_ups`）。随附事件在事务内的保存点写入：可选诊断失败只回滚它们，准入照常提交，缺口交给缺口重试|
|轮次开始|合并后的 turn/attempt 开始行；首轮附带 `user_input_recorded`（它须等凭据映射登记完成才发布，见 `model_turn/live.rs` 的 `RunObserver::publish_user_input`）与同批发布的客户端工具结果，事件序号按生产方到达顺序分配。生产方进入可能长时间阻塞的 RPM 排队等待之前调用 `RunObserver::commit_turn_start`（`WriterCommand::CommitTurnStart`），缓冲的 Turn 开始随即单独落盘，等待期间 Turn 已可读，等待之后的 attempt 另写自己的行；没有等待时仍与首个 attempt 合并|
|流式|item 收口事件（thinking、visible、handoff，含最后一个 item）按到达顺序合批，沿用队列中最多 64 条的合批|
|结算|用量事务：合并后的 turn 结束行，按 `persist_turn_finish_phase` 的现有规则单独提交，可选诊断写入失败不能回滚用量事实。收尾事务（`finish_run_with_completion`）：ClientCompletion、tail 来源、`run_finished` 在同一事务内提交。`finalize_activity` 不在这个事务里：它在最后一个观察句柄释放后由 writer 单独执行，先做一次只读检查，仅当 Run 仍有残留运行中活动时才开事务写入 `observation_gap`。此外，`UsageConfirmed` 在发布终态给客户端之前（`model_turn/live/lifecycle.rs` 的 `publish_terminal`，先于 `send_vendor_output().await`）单独提交一次只更新 attempt 用量的事务：它是独立同步点，不写事件行|

事件源顺序与 sequence 顺序不变；writer 对轮次开始、attempt 结束与客户端交付结算的缓冲按 Run 分开（`PhaseBuffers`），并发 Run 的阶段事件在队列中交错到达时互不占用、互不打断，各自仍合并成行。`FlushInteraction`、`Barrier`、`Shutdown` 屏障照旧立即提交。一个阶段事务失败时，该阶段的事件一起丢失，记为一个 `observation_gap`，仍归所属诊断任务处理（ADR-0060）。

**事务内语句精简：**

- 删除每批对 `visible_tail` 的 UPDATE（含 Turn 分隔符拼接），该列已删除。
- 事件 INSERT 与 Run/Interaction UPDATE 不再写 `expires_at`。
- 同一事务内，每个 Run 及其 Interaction 只更新一次 `last_active_at` 与 `last_event_sequence`。
- 事件、块、内容与引用行使用多行 INSERT。
- 首个 attempt 与 turn 同行写入，不再需要 `TargetAttemptStarted` 的父 Turn EXISTS 检查；后续 attempt 保留该检查，以覆盖有界队列丢失父事件的缺口路径。
- `interaction_observations_expiry_idx (expires_at, id)` 换成服务扫描的 `(last_active_at, id)` 索引。`last_active_at` 每批都会变化，所以这一项只是转移、而非消除索引维护。`rejected_requests_expiry_idx` 删除，扫描复用已有的 `rejected_requests_window_idx (occurred_at DESC, id)`。Run 与事件行本来没有到期索引，只是列变少。

按开发库 10-29 之后（当前工具结果捕获规则）的 6,919 个 Run 估算，以同一 Run 内相邻事件间隔小于 50ms 近似为同一阶段：

|方案|事件行/Run|事务/Run|
|---|---:|---:|
|现在|12.0|10.0|
|阶段合并 + 事件合并|7.7|4.7|

表中只计会写事件行的事务。ClientCompletion、tail 来源与 `finalize_activity` 现在每个 Run 至少还有 2 个不写事件行的事务，阶段合并会把它们并入收尾事务，当时据此推断实际降幅应更大；合成负载的实测没有复现 4.7（单轮 6.25、工具流 8.01 个 Observation 写事务/Run，口径差异见「实施后实测」）。语句精简的效果未单独估算。已知限制：流式事件批次只合并同一 Run 连续到达的事件，遇到另一个 Run 的命令即把它交回主循环，所以多个 Run 逐事件交错到达时仍是每个事件行一次提交，阶段合并对这种形态不减少事务数（上表是单 Run 近似，不描述这种交错）。实施后以 `stravia::perf` span 实测确认。

## 管理面读取：摘要带引用，正文按需取

存储与响应都不截断：删除 `INPUT_PREVIEW_CHARS`（4,096 字符）、`visible_tail`、凭据命中详情的 `MAX_DISCOVERY_TEXT_BYTES`（1 MiB）与 `CredentialDiscoveryHitDetail::Available.truncated`。`live.rs` 的 8 MiB 实时镜像和 Wire 抓包的 64 MiB 上限是资源安全阀，保留。

- 列表与 `POST /api/v1/observations/interactions/changes` 的输入、输出摘要只返回有序内容引用与 `raw_len`。分页按 root 计数，一页的 Interaction 条数没有上限；持久化 SSE 只推送小通知，前端再经 changes 取摘要。因此响应大小必须随引用数增长，而不随全文长度增长；列表也不再对每条全文做凭据遮蔽。
- 新增 `GET /api/v1/observations/contents?ids=`，批量取正文。它只返回被 `observation_event_contents` 或 `observation_tool_results` 引用、且属于可见 Interaction 的内容；Turn Chain 或 Marker 独有的 system、完整历史与 signature 不能经此读出。「普通事件不保存完整 canonical request/response」的边界不变；完整保存最新用户输入是这一边界下的明确例外，它只引用 Turn 已存的正文，不扩大为保存完整 request。存储的内容不可变，但响应按当前凭据映射遮蔽，因此 ETag 由摘要与凭据映射版本组成，响应带 `Cache-Control: private, no-cache`，客户端每次使用都要用 `If-None-Match` 重新验证，不得按 id 直接复用；凭据遮蔽在此端点按正文执行，主体没有有效映射时不扫描。流式进行中，前端只取新增的块。
- 详情、会话视图与凭据命中详情在服务端按引用批量取正文，返回全文。
- 前端卡片只渲染可见部分，打开 Tooltip 时再取全文渲染。
- Bundle 导出时把引用展开为正文，输出摘要改从 `observation_blocks` 推导。

## PostgreSQL 锁协议

内容表退出表级锁协议：写入不再 `LOCK TABLE … turn_contents`，GC 也不再对内容表取 `SHARE ROW EXCLUSIVE`；`turn_nodes` 的节点提交与 GC 锁序不变。

- 复用已有内容时用 `SELECT … FOR KEY SHARE`（ADR-0076 已有做法）。内容不存在时 `INSERT … ON CONFLICT DO NOTHING RETURNING id`，冲突则再以 `FOR KEY SHARE` 读取。
- GC 以 `FOR UPDATE SKIP LOCKED` 领取候选内容，再用新语句重检四个反向引用均不存在后删除。被在途写入锁住的内容会被跳过；写入方若等待的行已被 GC 删除，重读得到空结果后重新插入。正确性不依赖捕获外键错误后重试。
- 依赖（Marker、映射、原生登记）与节点的提交路径只加不冲突的行锁：提交以 `FOR KEY SHARE` 锁定被引用的依赖行，读不到的按「引用表」一节的规则处理；Turn 提交读取 scope 同样 `FOR KEY SHARE`（`turn_chain/sql.rs` 的提交路径与 `content_store` 的 `resolve_scope` 一致），清理方因此不会删掉写入方正在使用的 scope。
- 依赖回收以 `FOR UPDATE OF g, m SKIP LOCKED` 同时锁住回收队列行 `g` 与对象行 `m`，领取之后用新语句重检引用表与来源边，再删除。提交持有的 `FOR KEY SHARE` 会使对象行被跳过，所以回收不会撞上 `RESTRICT` 外键错误；入队后又被新节点引用的行由这次重检拦下。
- 释放触发器（`AFTER DELETE`）与 `enqueue_unreferenced` 先对被引用的对象行或候选内容行加 `FOR NO KEY UPDATE`，取得锁之后才重检剩余引用并入队。它与回收方的 `FOR UPDATE`、另一个同时删除兄弟引用的事务互斥，既不会在回收方「仍有引用，移出队列」的同一刻漏登记，也不会因为看不到兄弟事务尚未提交的删除而两边都漏登记。
- 节点、Marker 与原生登记的回收事务取独占的事务级咨询锁 `PG_GC_LOCK`（`pg_advisory_xact_lock`）；批量更新对象行的写入方——Marker 发布、映射发布、原生登记确认与启动时的 `recover_public_executions`——先取同一把锁的共享模式（`PG_GC_LOCK_SHARED`）。原因是锁序环：回收删除一个节点时，释放触发器按行删除顺序对它引用的多个对象依次加 `FOR NO KEY UPDATE`；发布方的 `UPDATE` 按调用方给定的顺序锁同一批对象并持有先拿到的行，两边顺序不同就会互相等待。二者互斥后取锁顺序不会成环；回收与回收之间也因独占锁而串行。代价是一次发布可能等待一个有界的回收批次。提交路径只加 `FOR KEY SHARE`，不取咨询锁。映射回收与内容回收不取咨询锁，只靠 `SKIP LOCKED` 领取，不在持有他人所需的锁时等待。曾经的 `remove_source_tool_executions` 触发器（0023）已由 0024 删除，Observation 清理不再触发任何 Marker 行更新。
- 节点回收**先取 `turn_nodes` 的 `SHARE ROW EXCLUSIVE` 表锁，再取 `PG_GC_LOCK`**。表锁要等所有在途提交（`ROW EXCLUSIVE`）结束，若咨询锁在前，这段等待期间持有的独占咨询锁会让 Marker 发布、映射发布与原生登记确认（含客户端回传解析路径上的确认）排队；表锁在前则等待提交期间不持有咨询锁，发布只会等待一个正在执行的回收批次。该顺序不成环，因为持有咨询锁的事务（Marker 回收、原生登记回收、发布、确认、恢复）从不申请 `turn_nodes` 的 `ROW EXCLUSIVE` 及以上表锁（它们对 `turn_nodes` 至多读取或 `FOR NO KEY UPDATE` 单行，对应 `ROW SHARE`，与 `SHARE ROW EXCLUSIVE` 不冲突）。
- Marker 回收整批只调用一次 `enqueue_unreferenced`：先逐个删除本批 Marker 并收集其释放的正文 ID，全部删除之后一次性按 ID 升序加 `FOR NO KEY UPDATE`。逐 Marker 入队会让不同 Marker 的正文行锁在批内交叉，与 Observation 清理（一次调用、按 ID 升序、不取咨询锁）互相等待而死锁。写入 Marker 的单行事务（`interrupt_if_stale`、`finish_execution`）先由 `intern_segment` 对复用的正文行加 `FOR KEY SHARE` 或插入新正文，再 `UPDATE` Marker 行，最后由 `replace_refs` 对释放的旧正文加 `FOR NO KEY UPDATE`；节点回收也把正文入队放在节点删除之后，因此排他正文锁（`FOR NO KEY UPDATE`）一律在对象行锁之后。前置的 `FOR KEY SHARE` 只与 `FOR UPDATE` 冲突，而持有 `FOR UPDATE` 的内容回收一律 `SKIP LOCKED`，不会等待它。
- 各类 PostgreSQL 事务的取锁顺序（`→` 表示先后；方括号内是同一语句里的有序批）。全局层级为：表锁 → 咨询锁 → 对象行锁 → 排他正文行锁（`FOR NO KEY UPDATE`），每个事务只取其中若干层且不逆序；`FOR KEY SHARE` 不受该层级约束，理由见末项「无环依据」：
  - 节点提交（持久化）：`turn_nodes` ROW EXCLUSIVE → scope 行 `FOR KEY SHARE` → 新节点各行（插入）→ 依赖行 `FOR KEY SHARE` → 正文行 `FOR KEY SHARE`/插入。不取咨询锁。
  - 节点回收：`turn_nodes` SHARE ROW EXCLUSIVE → `PG_GC_LOCK` 独占 → 候选节点行 `FOR UPDATE SKIP LOCKED` → 删除节点及其引用行（触发器依次对父节点行、Marker/映射/原生登记行 `FOR NO KEY UPDATE`）→ [本轮释放的正文行 `FOR NO KEY UPDATE`，ID 升序，一次调用，放在对象行之后] → scope 行 `FOR UPDATE SKIP LOCKED`。
  - Marker 回收：分两类事务，均先取 `PG_GC_LOCK` 独占。① 到期事务：到期的 `public_tool_executions` 删除（触发器对 Marker 行 `FOR NO KEY UPDATE`）→ 推进水位并把到期 Marker 入队，提交。② 之后每批一个事务：队列行与 Marker 行 `FOR UPDATE OF g, m SKIP LOCKED` → 删除 Marker（级联引用行）→ [本批全部释放的正文行 `FOR NO KEY UPDATE`，ID 升序，一次调用]。
  - 映射回收：不取咨询锁；队列行与映射行 `FOR UPDATE OF g, m SKIP LOCKED` → 删除。
  - 原生登记回收：`PG_GC_LOCK` 独占 → 队列行与登记行 `FOR UPDATE OF g, c SKIP LOCKED` → 删除登记（触发器依次对来源登记行、来源节点行 `FOR NO KEY UPDATE`）。
  - 内容回收：不取咨询锁；队列行与内容行 `FOR UPDATE SKIP LOCKED` → 重检后删除；scope 清理 `FOR UPDATE SKIP LOCKED`。
  - Marker 发布、映射发布、原生登记确认、启动恢复：`PG_GC_LOCK_SHARED` →（映射发布再取 Principal 咨询锁）→ 目标行 `UPDATE`。不取 `turn_nodes` 表锁。
  - Marker 单行执行状态更新（`interrupt_if_stale`、`finish_execution`）：复用的正文行 `FOR KEY SHARE`/新正文插入（`intern_segment`）→ Marker 行 `UPDATE` → 释放的旧正文行 `FOR NO KEY UPDATE`（`replace_refs`）。不取咨询锁。
  - Observation 到期清理与「清除历史记录」：不取咨询锁；Observation 各表行删除 → [释放的正文行 `FOR NO KEY UPDATE`，ID 升序，一次调用]。不触及 Marker、映射、登记或 `turn_nodes`。
  - 无环依据：表锁只出现在最前，且持有咨询锁的事务从不申请 ROW EXCLUSIVE 及以上表锁；咨询锁的独占/共享把所有“多行对象锁”事务（回收与发布类）互相排成串或隔开；正文排他锁位于最末层，持有它的事务（含 Observation 清理）之后不再等待对象行，且各事务都在一次有序调用里取完，不同事务之间不会以相反顺序等待同一批正文行；提交路径与 Marker 单行更新对对象行与正文行先加的锁只是 `FOR KEY SHARE`，只与 `FOR UPDATE` 冲突，而 `FOR UPDATE` 的持有者（各回收的领取）一律 `SKIP LOCKED`，从不等待它。
- 清理 scope（`prune_scopes`）以 `FOR UPDATE SKIP LOCKED` 领取候选 scope，被写入方 `FOR KEY SHARE` 的 scope 留给之后的回收，再用新语句重检节点与正文后删除，不依赖外键错误。
- 原生登记续期对 `turn_nodes` 取的 `ROW EXCLUSIVE` 表锁随续期一起删除；节点回收仍沿用 `LOCK TABLE turn_nodes IN SHARE ROW EXCLUSIVE MODE`，提交路径的 `turn_nodes` 锁序不变。
- SQLite 继续使用共享写租约与 `BEGIN IMMEDIATE`。

这样，Observation 写入与内容写入不再等待 GC 的表锁（Turn 提交沿用 `turn_nodes` 的 `ROW EXCLUSIVE`，仍与节点回收的 `SHARE ROW EXCLUSIVE` 互斥）；Marker 发布、映射发布与原生登记确认只会等待一个正在执行的回收批次（上文咨询锁），不会因节点回收等待在途提交而排队。并发行为由 `history_retention` 的 PostgreSQL 并发测试（`publication_waits_for_an_in_flight_reclaim`、`reclaim_waits_for_an_in_flight_publication`、`publication_does_not_wait_for_node_reclaim_blocked_on_a_commit`、`marker_reclaim_orders_content_locks_across_its_whole_batch`、`startup_recovery_waits_for_an_in_flight_reclaim` 等）覆盖；并发写入加强制扫描的争用压测中死锁为 0，请求尾延迟与实施前相当（见「实施后实测」），发布等待回收批次的时长没有单独测量。

## 升级

升级时一次性离线重写存量，不提供边读边迁的读取路径。旧二进制不能读取新 schema，回退只能恢复升级前的备份（同 ADR-0076）。

1. **内容**：按新域前缀重算摘要；从已有 JSON 单元中拆出文本单元，把 descriptor 重写为文本槽位与拼接槽位。
2. **Marker**：`segment_payload` 拆为外壳与正文引用。
3. **Observation**：按上表转换各 kind 的 payload；存量 turn/attempt 的首尾事件按同一规则合并；kind 改名；迁移 Run 列；删除 `expires_at`、`input_preview`、`visible_tail`。已截断的旧预览保持截断，不从 Turn Chain 回填。
4. **保留期**：删除 `turn_node_retention.expires_at` 及其索引。现有叶子节点的到期时间就是提交时刻加当时的保留期，被原生登记延长的来源节点改由外键保护，所以 Turn 节点无需折算。从节点载荷回填 `turn_node_markers` 与 `turn_node_compactions`；映射引用无法回填（见「引用表」）。因此已发布依赖的首次发布或确认时刻取 `max(原值, expires_at − 升级前生效的保留期)`，至少保留到现有到期时间，之后再被使用时由新提交登记引用。Marker 与原生登记按同一规则折算，覆盖载荷中没有引用、却已被续期的记录，例如续期后提交失败。未发布行的 `expires_at` 原样成为 `pending_until`。各水位初值为最小值，升级后第一次扫描检查全部存量一次。

SQLite 与 PostgreSQL 各写一份迁移，用 `stravia-tools dump-schema` 重新生成 `docs/database/*.sql`，并验证两后端的存储测试。开发库规模为 observation_events 49 万行、turn_contents 17.8 万行、Marker 1.7 万行；大库会延长升级时间，资源成本须按 ADR-0076 的方式实测。

## 与既有决策的关系

已在落地时同步修订（各 ADR 开头的引用块或正文已写明）：

- ADR-0076：内容表归属、内容身份公式、按 JSON 键名切分正文、descriptor 槽位格式、PostgreSQL 表锁覆盖内容表。
- ADR-0077：`client_tool_result` 的行形态、thinking/visible 的 payload 形态，以及 `observation_events.payload` 改为 `attrs`。
- [ADR-0087](0087-list-credential-hits-per-mapping-with-retained-item-context.md)：最新用户输入「最多 4,096 字符」与命中详情截断。
- [ADR-0012](0012-discover-target-continuation-from-effective-prefixes.md)、ADR-0028、[ADR-0043](0043-scope-redaction-mappings-to-api-keys.md)、[ADR-0047](0047-separate-native-compaction-ancestry-from-context.md)：「七天」「按 Chain TTL / Generation Chain 保留期延长」「分支延长祖先时同步延长引用的 Marker」改为统一天数与按引用保留；「过期记录不能通过续期复活」改为扫描是唯一删除方，扫描前的记录仍可读、可被引用。ADR-0043 的映射寿命改为取决于引用它的节点与下限窗口，访问边界仍只看 Principal。
- `docs/design/interaction-observation.md`、`docs/design/data-layout-performance.md`、`docs/design/architecture.md` 的 schema 片段，以及 `docs/database/*.sql`。
- 引用同一旧约定的措辞同步更新：ADR-0019（部署者管理保留的依据由 Chain TTL 改为历史保留天数）、ADR-0046（保留期测试描述）、ADR-0088（映射可能先于 Interaction 回收；`reveal=true` 不再截断）。

ADR-0060 关于 Observation 所有权、Generation Chain 隔离、诊断写入失败语义，以及 Observation 与 Debug Trace 共用保留期的决定，均不变。

_Contradicts ADR-0076（`turn_contents` 由 Turn Chain 拥有，「此格式不改变……保留期」）与 `docs/design/data-layout-performance.md` §4「没有……合并不同所有权域的 TTL」——但值得重开：三方保存同一正文后，内容表不能再由单一域拥有，回收也不能依赖两套时钟。合并的只是天数设置；引用仍由各域各自登记、各自删除。_

## Considered options

- **用数据库列压缩替代 `storage_codec`**：在 PostgreSQL 16.15 上导入开发库全量数据，TOAST lz4 合计 931.2MB、pglz 923.5MB，比应用层 zstd 的 670.6MB 大约 39%。PostgreSQL 只压缩超过约 2KB 的行，Observation 83%、内容 77% 的行低于这一阈值，而且没有字典。SQLite 没有可放心用于生产的列压缩（sqlite-zstd 的作者不承诺数据安全与向后兼容）；去掉应用层压缩会让两后端的行为或体积分叉。因此保留应用层压缩；字典压缩在新结构上重新测量后另行决定。
- **按块或按 item 共享**：外壳（meta、id、signature）各方不同，实测 `meta.id` 零匹配，共享率低。
- **直接复用 Turn Chain 拥有的 `turn_contents`**：GC 只认 `turn_node_contents`，Observation 引用的内容会被误删。改为独立模块，各引用表平等判活。
- **只加哈希列，或在同一 Interaction 内跳过重复写入**：前者只加快比较，不省空间；后者违反 ADR-0077「兄弟分支各自保留收据」。
- **投影文本存为 IR 类型节点、读取时渲染**：客户端回传的历史须与存储逐字节一致，渲染规则一变旧链就对不上，需要永久保留各版本的渲染器。
- **存活动时间、读取时按当前天数精确过滤**：节点与依赖都保存最近活动时间，提交推进祖先及其引用的依赖，读取与扫描使用同一谓词。到期立即不可读、改天数立即生效，但「节点可读 ⇒ 依赖可读」要靠每次提交由代码推进每一种依赖，新增依赖漏接一处就会破坏它；每条读取 SQL 都要带到期条件；提交按链深度写祖先。按引用保留把这一不变量交给外键，读取没有时间条件可漏，代价是到期后最多一个扫描周期的可读期。
- **只靠子节点保护、依赖仍单独计时**：已到期但受子节点保护的祖先仍可读，而它依赖的 Marker 与映射可能先被清理，续接会缺失 hidden history。引用表消除了这一缺口。
- **每轮按下限窗口重扫、反连接排除仍被引用的行**：实现最简单，但已过期限却仍被引用的行每轮都要重新检查，长期活跃的会话越多成本越高，而现有种子扫描在写事务内执行。开发库中这类行只有个位数到十几行，但链都很短；按 1,000 个连续活跃 30 天、每天提交 200 次的会话、N = 7 推算，每轮约重扫 460 万行（推断）。
- **维护叶子集合或引用计数**：扫描成本低，但每次提交都要写父节点或依赖行，重新引入提交期写入与热点行争用。
- **在删除引用的代码中入队**：每条删除引用的路径都要记得入队，与「每种依赖都要记得续期」是同一类易漏规则；触发器把它变成 schema 约束。
- **正文单元设 256B 门槛**：违背 ADR-0076 的统一寻址，短串仍会存多份。
- **摘要只返回 4,096 字符摘录**：按用户决定不限制长度，改为摘要带引用、正文按需取。
- **文本单元存 JSON；边读边迁**：前者多一轮转义与解析；后者要长期保留旧格式的读取路径。
- **跨 Run 组提交、有界等待窗口**：不在本决定范围内。前者拉长 SQLite 写租约的持有时间，可能挤压 Turn Chain 与统计写入；后者增加 SSE 与读己之写的延迟，崩溃时丢失的内存中事件也更多。阶段合并落地并实测后再评估。
- **SQLite `synchronous=NORMAL`**：会改变用量与计费事实的持久性，须单独决策。

## Consequences

- 单个 item 的写入由 1 行变为事件、块、引用 3 行（正文首次出现时另加内容行），单个事务更重，靠事务数下降来抵消。正文单元不设门槛也会增加内容行与引用行，行数增幅没有单独统计；合成负载上的各表字节与编码载荷见「实施后实测」。
- SQLite 单写租约下单次持有变长；实测中 Observation 每 Run 的持有与排队总时长下降，Turn Chain 与统计写入的排队均值没有变差，但新二进制有 2 个 case 出现原因未定的提交停顿。PostgreSQL 行级锁协议的并发压测见「实施后实测」。
- 清理 Observation 历史只删除 Observation 的引用；仍被 Turn Chain 或 Marker 引用的正文，要等那些引用删除后才回收。界面与文档须写明。
- 与其他事件同时发生的 Run 状态转移不再单独留行，时间线不能显示这些中间状态；SSE 的 kind 集合发生变化，前端时间线与摘要映射需同步修改。
- 到期不再立即不可读：到期后最多一个扫描周期内仍可读、可续接，续接后旧链继续保留。修改天数不重写任何行，在保存后触发的扫描中生效；缩短天数会让这次扫描删除大批数据。
- 每个期限列多一行水位，每张引用表多一个触发器。触发器中「已无剩余引用」的判断，SQLite 逐行执行，PostgreSQL 的 AFTER ROW 触发器在语句结束时执行；两后端须各自测试「最后一条引用删除时才入队」。漏挂触发器的后果是记录无法回收而不是被误删；以「时钟越过全部下限窗口后扫描至结束，节点、依赖、正文与所有队列均为空」的不变量测试防止新增引用类型时遗漏。
- 每次 Turn 提交多写几行引用，但不再写祖先，也不再有三个独立的续期事务。
- 长期活跃的 Interaction 会一直显示全部事件，直到整个 Interaction 被回收。
- 凭据命中详情引用的映射可能先于 Interaction 被回收，此时走 ADR-0087 已有的「映射已清理」退化路径。不为 Observation 增加对映射的引用，否则凭据原文会随诊断记录保留更久。
- 管理 API 有不兼容变更：摘要字段改为引用、新增内容端点、删除 `truncated`、事件改名；不保留旧字段或别名。

## 实施说明

本节只记录实现时在正文之外确定的具体约定，不改变上文的决定。读取接口的字段以管理 API 的实现与测试为准。

- **迁移与升级钩子。** 0037 → Rust `content_upgrade`（`stravia-core` 顶层模块，不在 `content_store` 内：转换要理解 Turn Chain 与 Marker 的载荷语义，`content_store` 保持与语义无关；进度表 `content_upgrade`：节点、Marker、校验，可中断续跑，进度行缺失或更新行数不为 1 时明确报错）→ 0038；0039 → `turn_chain::upgrade::retention_*`（回填引用与首次发布/确认时刻，幂等重算，无进度表）→ 0040；0041 → `interaction_observation::upgrade_normalize`（进度表 `observation_normalization_upgrade`，`stage` 为 0–4，共四个阶段：1 payload 转 `attrs` 与块、2 首个 attempt 并入 turn 开始、3 最后一个 attempt 与用量并入 turn 结束、4 `input_sequence` 回填）→ 0042。SQLite 的 `turn_scopes` 改名为 `content_scopes` 由钩子在短暂开启外键的会话内执行（外键关闭时 `ALTER TABLE … RENAME` 不会改写子表的 `REFERENCES`），改名后检查 `foreign_key_check` 再恢复关闭。
- **合并行的 `attrs` 形状。** `model_turn_started.attrs` 是原 turn 开始属性加 `attempt`（首个 `target_attempt_started` 去掉 `kind` 的属性）与 `attempt_at`（该 attempt 事件的发生时间，毫秒）；`model_turn_finished.attrs` 是原 turn 结束属性加最后一个 attempt 的 `attempt`、`attempt_at` 与 `usage`（与 `attempt.usage` 相同）。后续 failover attempt 仍写 `target_attempt_started`/`target_attempt_finished`。`usage_confirmed` 不再是事件；持久化 SSE 通知的 `error_source` 对合并行取 `attempt.error.source`。升级与新写入产生相同形状。升级时一个 Turn 有多个 attempt（failover）的，开始行只并入首个 attempt 的开始，结束行只并入先于结束行的最后一个 attempt 的结束与用量；被取代的 attempt 开始/结束保持各自独立的行（`merge_last_attempts`）。升级遇到无法解码的历史载荷（storage codec 或 JSON 失败）时，该事件的载荷按 `{}` 迁移并逐条 `tracing::warn!`，结束时再汇总告警（数量与首个 sequence）：ADR-0089 之前的 `convert_event_storage` 对这类载荷是报错，现有升级测试（载荷 `X'FF'`）要求升级成功，所以这是有意接受的数据损失，不是静默容错。
- **无 payload 的事件。** `run_admitted`、`request_failed`、`run_state_changed` 的信息从 Run 列与 `failure_json` 读取，读取投影把它们拼回统一的 `payload` 视图；进程重启造成的中断以 Run 的 `terminal_reason = process_restarted` 表达，`run_state_changed` 不再带 `reason`。
- **内容读取端点。** `GET /api/v1/observations/contents?ids=` 最多 256 个 id，缺失、空元素、非正整数或超限一律返回 `400` 与 `invalid_observation_ids`；重复 id 去重，响应按首次出现顺序，不可见的 id 直接省略。响应 `data.contents` 的每项为 `{ content_id, format: "text" | "json", raw_len, body }`（`ObservationContent`；摘要只参与 ETag，不进入响应）。ETag 为 `sha256(凭据映射版本 + 返回正文的内容摘要)` 的十六进制并加引号；200 与 304 都带 `ETag` 和 `Cache-Control: private, no-cache`，客户端可缓存但每次使用前必须带 `If-None-Match` 重新验证，不得按 id 直接复用，因为同一 id 的正文随当前凭据映射遮蔽而变化。支持 `If-None-Match`（`304`）；映射新增或失效会改变 ETag。id 校验（正数、非空、不超过 256 个）在 Core 的 `observation_contents`（`ObservationQueryError::InvalidContentIds`），路由只做传输解析与错误映射。正文经共享内容存储读取并校验长度与摘要。WebUI 按请求键（排序后的 id 分片）缓存 ETag 与正文，每次读取发送 `If-None-Match`，304 复用已缓存的正文；不按 id 永久缓存，也不记忆「缺失」的 id，同一时刻对同一请求键的并发读取共用一次请求。
- **摘要结构。** `InteractionSummary.input/output` 为 `{ refs: [{ content_id, raw_len }], raw_len }`；`raw_len` 是引用正文原始字节数之和（不含拼接分隔符）。详情另给 `input_text`/`output_text` 全文。Bundle 摘要删除 `visible_tail`，改为 `latest_output_text`：最新可见块的完整文本，来自 `observation_blocks`。
- **保留期。** `log_retention_days` 小于 1 的保存请求返回错误，保存成功后同步执行一次 `sweep_history`；启动时缺失按默认 7、无法解析时记录告警后按默认 7、低于 1 的值按最小 1 处理。扫描顺序为 Observation、Turn 节点、History Marker、可逆脱敏映射、原生压缩登记、正文，每小时一次。水位表 `history_retention_watermarks` 每个期限列一行，共七行（`turn_nodes.created_at`，以及 Marker、映射的 `pending_until`/`published_at`，原生登记的 `pending_until`/`confirmed_at`）。未发布或未确认窗口为常量一小时，不随 N 变化。保留期句柄 `HistoryRetention` 是 Turn Chain、Generation Chain 与 Observation 构造时的必填参数，没有隐含的 7 天默认。
- **Marker 外壳。** `history_markers.segment_payload` 存放 `storage_codec` 编码的外壳（原为未压缩 TEXT），signature 与 encrypted_content 留在外壳内。
- **回收队列与触发器。** 依赖队列表 `history_marker_gc`、`redaction_mapping_gc`、`native_compaction_gc` 的行随对象级联删除，Turn 节点沿用 `turn_node_gc`；七个 `AFTER DELETE` 触发器覆盖子节点、三张节点引用表、`native_compaction_sources`、原生登记指向的来源节点与 Platform Tool Execution 指向的 Marker。
- **内容 GC 批次。** 每批最多 512 行或 8 MiB（单个超过 8 MiB 的正文自成一批）；每批一个事务，提交后释放写协调，循环到队列排空，所以一次 `sweep_history` 就清空队列。PostgreSQL 以 `FOR UPDATE OF c SKIP LOCKED` 领取。
- **Observation 写入路径。** 生产方（`RunObserver`）负责阶段边界：`capture_user_input` / `publish_user_input` 控制用户输入的发布，`commit_turn_start` 在 RPM 排队等待前提交缓冲的 Turn 开始。writer 的轮次开始、attempt 结束与客户端交付结算缓冲按 Run 分开。收尾事务 `finish_run_with_completion` 把 ClientCompletion、tail 来源与 `run_finished` 放在同一事务；`finalize_activity` 不在其中，它在最后一个观察句柄释放后运行，仅在残留运行中活动时才写入，否则只是一次读。准入事务（`ObservationStore::admit`）同时写入随附的父链缺口与 Retained Tail 诊断事件，随附部分走保存点。事件、块、事件引用与工具结果使用多行 INSERT（`event_writes.rs` 的 `EVENT_ROWS_PER_INSERT`、`BLOCK_ROWS_PER_INSERT`、`REF_ROWS_PER_INSERT`、`TOOL_ROWS_PER_INSERT`），同批工具结果的正文在同一次 `intern` 中登记；Run 与 Interaction 的 `last_active_at` / `last_event_sequence` 每个事务只更新一次（`persist_turn_start`、`persist_user_input` 同样）。Observation 的正文 scope 是 `api-key:<principal>`（见「共享内容存储」）。「清除历史记录」（`purge_clear_rows`）与到期回收一样，在删除交互的同一事务内对失去引用的正文调用 `enqueue_unreferenced`，随后由内容回收删除。
- **索引。** 映射表的 Principal 索引由 `idx_reversible_redaction_mappings_principal_expiry`（`(principal, expires_at)`）改为 `idx_reversible_redaction_mappings_principal_pending`（`(principal, pending_until)`）；0042 在两个后端都以 `DROP INDEX` 删除 `interaction_observations_expiry_idx`、`rejected_requests_expiry_idx` 与没有查询再依赖的 `observation_events_tool_idx`。
- **物化缓存与原生压缩确认。** 物化缓存的有效期在命中时按头节点 `created_at` 加当前 N 计算（`finish_context_read`），不再取前缀与节点各自到期时间的较小值。`Compaction::resolve` 解析客户端回传的登记时调用 `confirm_pending`：登记仍未确认时设置 `confirmed_at`，与首次交付确认取先到者。

## 实施后实测

以下由集成者在合成负载上测量，日期 2026-11-04。未能测量的格写明原因，不用本地开发库估算值或推断代替。工作负载是脚本化 mock 上游产生的合成负载，不是开发库的真实 Run 分布，不能与上文 ADR 的估算（6,919 个真实 Run：事件行 12.0 → 7.7、事务 10.0 → 4.7）直接比较。

|指标|实施前|实施后|条件|
|---|---:|---:|---|
|事件行 / Inference Run|13.33（工具流）/ 11.0（单轮）|9.42 / 8.0|SQLite，计数与负载无关；明细见下|
|写事务 / Inference Run（Observation 归因）|11.49（工具流）/ 12.50（单轮）|8.01 / 6.25|同上；PostgreSQL 工具流 11.57 → 8.03|
|写事务 / Inference Run（非 Observation 归因）|3.57（工具流）/ 11.1（单轮）|3.97 / 10.1|同上；Observation 之外的写入：单轮减少 1.0，工具流增加 0.4（见下）|
|SQLite 写租约：Observation 每 Run 持有总时长|21.8–36.6 ms|19.5–24.4 ms（两个 case 因磁盘提交停顿为 136.3 与 148.5 ms）|空闲机，每二进制 4 次重复；单租约持有变长：均值 1.6–2.9 → 3.2–4.1 ms|
|SQLite 写租约：Observation 每 Run 排队总时长|27.4–66.7 ms|8.2–15.3 ms（两个停顿 case 为 613.8 与 60.4 ms）|同上；非 Observation 写入的排队均值没有变差|
|PostgreSQL 32 worker 固定并发，unary 准入阶段尾延迟（p95 / p99）|483–485 / 552–568 ms|386–438 / 413–542 ms|120 s × 2 次；stream 用例被 mock 的节奏门禁作废，未测（见下）|
|PostgreSQL 并发写入 + 强制扫描的死锁|18 与 10 次（`pg_stat_database`）|0 与 0|32 worker、120 s、扫描每 0.5 s 交替 7/8 天，各 2 次|
|各表字节（`contents`、`observation_*`、`history_markers`）|1,441,792 B 页面（工具流，SQLite）|1,261,568 B（−12.5%）|合成小库，页面粒度；编码载荷 923,667 → 734,368 B（−20.5%）；明细见下|
|升级耗时（0037–0042，含 Rust 钩子）|不适用|SQLite 74.1 / 109.3 s，PostgreSQL 137.9 / 87.2 s|约 490k 事件 / 178k 内容 / 17k Marker，各 2 次；波动大，不给均值|
|升级峰值空间（新旧结构并存）|不适用|SQLite 1.150 GB（库 788 MB + 未检查点 WAL；升级前 668 MB）；PostgreSQL 数据库 1.472 GB（升级前 1.006 GB）|升级后不回收：VACUUM 后 587 MB / 852 MB，比同数据旧布局的 646 MB / 880 MB 小 9.1% / 3.2%|
|每轮扫描耗时（常态 / 缩短天数后的首轮）|Observation 扫描 109–199 ms + 保留期更新 0.19 s（SQLite）/ 5.9–8.6 s（PostgreSQL）|`history.sweep` 118–134 ms / 125–169 ms，保留期更新 0|同一升级库上的扫描；夹具内全部数据在 1 天之内，缩短天数没有触发大批删除（见下）|

### 二进制、命令与机器状态

实施前 = `aecfc21e` 的 `test-harness` release 构建，SHA256 `c8e9fda9974b69d451f982b82ce93abd7d73ea5066c02309ad8d3f390446ca6e`；实施后 = `d68284d5`，SHA256 `7c1bd726d659422d86dc976472b69c8a8800903194e2c37ba9e90edea8d27704`（升级库生成器所用 `stravia-storage-test-harness` 为 `ba3e1a81397a280b919cfe928493e3eb324cb3edd03b957eda83c2fd9b177e2f`）。`d68284d5` 之后的提交（`12ae3ab2`）只改测试与脚本，不改产品代码。构建：先 `cargo run --locked -p stravia-vendor-base --bin stravia-build-vendors` 构建 Wasm，再 `CARGO_BUILD_JOBS=8 cargo build --locked --release -p stravia-server --no-default-features --features test-harness --bin stravia-server --bin stravia-storage-test-harness`，两侧各在独立 worktree 与 target 目录。

命令都经 `tests/common/adr0089_measure.sh MODE BINARY LABEL`（说明见 [研究笔记](../research/protocol-http-benchmark.md) 的 ADR-0089 一节）：`counts`、`CONVERSATIONS=6 … toolflow`、`BACKEND=postgres CONVERSATIONS=4 … toolflow`、`lease`、`pg`、`contention`、`upgrade-gen`、`upgrade`（`KEEP_UPGRADED=1`）、`sweep`（`DATA_DIR=…` 复用升级库）。每个 PostgreSQL 运行前执行 `redis-cli -p 56379 -n 0 flushdb`（专用 Redis，只清 DB 0），数据库用后即删。写租约、固定并发、扫描、争用与升级的运行只在机器空闲时进行：每次运行前 `/proc/loadavg` 的 1 分钟值为 0.4–3.4（超过 2.4 的都紧接在前一次测量或副本准备之后，是测量自身的残留），20 核 / 62 GB，后台只有 qbittorrent、IDE 等常驻进程（各约 5–10% 单核）。第一批运行（租约、固定并发，04:10–04:56）期间后台采样每 5 秒记录非测量进程的 CPU，超过 30% 单核的只有：`podman` 31.6% 一次（04:11，基线 r1 租约运行内）、`gh`/`git` 31–83% 单核（04:36–04:38 约 2 分钟，基线 r2 固定并发运行内；04:41、04:46、04:49、04:51 为 33–50% 的零星一次，在新 r2 固定并发运行内）；采样同样覆盖升级、扫描与争用的运行（均无超过 30% 的外部进程）。后加的租约重复 r3、r4 没有采样，r3 紧接在 cargo 构建与单元测试之后（1 分钟 loadavg 1.5、5 分钟 4.7）。PostgreSQL 16.15 与 Redis 为本机独立实例（`fsync=off`、`max_connections=400`），数据目录在 `/srv/nas` 下的联合文件系统。

### 单轮队列（`counts`）

2 个入口 × 2 个上游（openai-chat、anthropic-messages）× unary/stream，每 case 4 个请求，并发 1，共 32 个 Run，SQLite，Debug 开启。每个 case 的 `dropped_*` 均为 0，全链路审计通过。

|每 Run|实施前（unary / stream）|实施后（unary / stream）|
|---|---:|---:|
|事件行|11.0 / 11.0|8.0 / 8.0|
|Observation 写租约|13.0 / 13.5|6.0 / 6.0|
|Observation 写事务（外层提交）|12.25 / 12.75|6.25 / 6.25|
|写租约内 Observation 语句|74.4 / 74.9|53.7 / 54.0|
|非 Observation 写事务|10.75 / 11.44|9.75 / 10.44|

事务计数只数外层 `sql.transaction.commit`：sqlx 嵌套 `begin()` 产生的保存点释放也是 `sql.transaction.commit` span，准入事务里随附诊断事件的那个保存点会被算成第二个事务，分析脚本沿同一父 span 的 begin/commit 深度把它排除（`savepoint_commits` 另记，实施后每 Run 1.0 个）。事务按 writer 动作拆分（unary）：实施前 `admit_persist` 1、`persist_events`（准入事件）1、`persist_input_preview` 1、`finish_run` 1、`persist_tail_source` 0.56、其余各自提交的单事件命令 7.69；实施后 `admit_persist` 1、`persist_turn_start` 1、`finish_run` 1、其余各自提交的单事件命令 3.25。实施前的 11 行事件为 `run_admitted`、`input_preview_recorded`、`credential_mappings_created`、`retained_tail_associated`、`model_turn_started`、`target_attempt_started`、`client_visible_content`、`target_attempt_finished`、`model_turn_finished`、`run_state_changed`、`run_finished` 各 1；实施后的 8 行为 `input_preview_recorded` 换成 `user_input_recorded`，去掉两个 `target_attempt_*` 与 `run_state_changed`。

### 工具调用流（`toolflow`）

6 轮对话 × {openai-compatible, google-gemini} × {unary, stream}，每个变体 6 个对话（PostgreSQL 4 个），共 144 个 Run（PostgreSQL 96 个），每个对话一个 Debug epoch 与独立 API Key。SQLite 24/24 个对话有效，PostgreSQL 16/16 个对话有效。

|每 Run|实施前|实施后|
|---|---:|---:|
|事件行|13.33|9.42|
|Observation 写事务（SQLite / PostgreSQL）|11.49 / 11.57|8.01 / 8.03|
|非 Observation 写事务（SQLite / PostgreSQL）|3.57 / 3.62|3.97 / 3.99|
|两类合计（SQLite / PostgreSQL）|15.06 / 15.19|11.97 / 12.02|
|Observation 写租约（SQLite）|11.46|7.00|
|写租约内 Observation 语句（SQLite）|80.4|68.0|

非 Observation 写事务（Turn Chain、统计等）单轮队列每 Run 少 1.0 个，工具流多约 0.4 个；perf span 不带 SQL 文本，没有逐项归因这些事务。事件行按 kind 的变化（每 Run）：`target_attempt_started`/`target_attempt_finished` 各 1.0 → 0（并入 Model Turn 行）；`run_state_changed` 1.83 → 0.83；`client_tool_result` 1.75 → 0.83（工具结果同时写入 `observation_tool_results`，共 252 行）；`input_preview_recorded` 0.17 → `user_input_recorded` 0.17；其余（`client_tool_handoff` 1.75、`model_thinking` 1.42、`run_admitted`/`model_turn_started`/`model_turn_finished`/`run_finished` 各 1、`client_visible_content` 0.25、`retained_tail_associated` 0.17）不变。

**与 ADR 估算的差异。** 单轮队列的事件行（8.0）接近估算的 7.7，工具流（9.42）高于它，因为工具流每 Run 平均有 1.75 个 `client_tool_handoff` 与 1.42 个 `model_thinking` 事件，这些是 item 收口事件，不会被阶段合并消除。Observation 写事务没有达到估算的 4.7：单轮队列 6.25，工具流 8.01。实施后单轮 Run 的 6.25 个事务是 3 个阶段事务（准入、Turn 开始、收尾）加 3.25 个仍各自提交的单事件命令；perf span 不带 SQL 文本，无法从跟踪中直接指出这 3.25 个命令对应哪几种 kind。按代码路径推断，它们是流式阶段里 item 收口批、`UsageConfirmed` 只更新用量的事务与 `persist_turn_finish` 的用量事务，这是推断，未对每个 span 逐一验证。估算把“相邻事件间隔小于 50 ms”都算作同一阶段，实现按生产方可确定的阶段边界合并，并且写入线程遇到另一个 Run 的命令即结束批次（跨 Run 合批不在本 ADR 范围）；两者口径不同，估算未被实测复现。

### 共享内容（`verify_content_sharing.py`）

每个 API Key 一个 `content_scopes` 行（形态 `api-key:<id>`），Turn、Marker 与 Observation 共用。单轮队列（SQLite）：1 个 scope，`contents` 112 行，其中 Turn 与 Observation 同时引用 64 行、仅 Turn 引用 48 行，没有仅被 Observation 引用的行，即 Observation 的输入/输出正文没有第二份存储。工具流（SQLite，24 个 API Key）：24 个 scope，`contents` 1,704 行，仅 Turn 1,308、Turn+Observation 384、Turn+Marker+Observation 12；PostgreSQL（16 个 API Key）：16 个 scope，`contents` 1,136 行，仅 Turn 872、Turn+Observation 256、Turn+Marker+Observation 8。三组的 `api_keys_with_more_than_one_scope` 与 `bodies_in_more_than_one_scope_of_the_same_api_key` 均为 0。查询为 `SELECT COUNT(*) FROM (SELECT <去掉 api-key: 前缀的 principal> AS k, c.digest FROM contents c JOIN content_scopes s ON s.id = c.scope_id GROUP BY k, c.digest HAVING COUNT(*) > 1) t`。工具流里跨 API Key 出现相同摘要（SQLite 与 PostgreSQL 各 36）是作用域隔离的预期结果，不算重复。

### 各表字节

SQLite 用 dbstat（含索引，页面粒度 4 KiB）、PostgreSQL 用 `pg_table_size + pg_indexes_size`（8 KiB 页），两者在 100 多个 Run 的小库上都受页面粒度主导，不外推到开发库规模。

|工具流 SQLite|实施前|实施后|
|---|---:|---:|
|内容（`turn_contents` → `contents`）|462,848|512,000|
|Observation（`observation_events` → 加 `observation_blocks`、`observation_event_contents`、`observation_tool_results`）|847,872|589,824（479,232 + 73,728 + 8,192 + 28,672）|
|Marker（`history_markers` → 加 `history_marker_contents`）|131,072|159,744（122,880 + 36,864）|
|三者合计|1,441,792|1,261,568（−12.5%）|
|编码载荷合计（`turn_node_payloads`、内容、Marker 外壳、事件 `payload`/`attrs`、块 `envelope`）|923,667|734,368（−20.5%）|

PostgreSQL 工具流（96 个 Run）：三者合计页面字节 1,679,360 → 1,818,624（+8.3%；内容 516,096 → 581,632，Observation 950,272 → 974,848，Marker 212,992 → 262,144），编码载荷合计 615,554 → 489,903（−20.4%）。小库上新增的表各带至少一个堆页与索引页，页面总量没有下降；载荷字节的下降才反映内容共享与规范化的效果。单轮队列（输入输出正文占绝大部分字节，SQLite）：内容 7,921,664 → 7,872,512 B，Observation 327,680 → 151,552 B，Marker 20,480 → 36,864 B，三者合计 8,269,824 → 8,060,928 B，编码载荷合计 8,061,642 → 7,867,524 B（−2.4%）。

### SQLite 写租约（`lease`）

openai-chat → openai-chat，unary 与 stream 各一个 case，并发 8，每 case 16 个 Run（2 轮续接 × 8 个并发链），`--mode fixed-workers`；二进制各重复 4 次（r1、r2 先基线后新，r3、r4 先新后基线）；每个 case 的 `dropped_*` 均为 0。数据目录与 perf 快照由 `--data-dir`、`--save-performance-snapshots` 保留。上游 mock 固定 10 s 输出，吞吐约 0.68 QPS，写租约队列远未饱和，下面的排队时间是该负载下的值，不代表高并发写入的上限。单租约统计（`sqlite.write_gate.hold` / `.wait`，微秒，按 span 的 `parent_id` 链归属到 Observation 与其余写入）：

|case|二进制|Observation 租约/Run|持有均值 / p95|排队均值 / p50 / p95|持有总长 / Run|排队总长 / Run|
|---|---|---:|---:|---:|---:|---:|
|unary|实施前 r1–r4|12.4–12.8|2.0–2.9 ms / 4.1–6.9 ms|2.2–5.2 ms / 1.1–4.1 ms / 7.7–14.6 ms|25.6–36.6 ms|27.9–66.7 ms|
|unary|实施后 r2–r4|6.0|3.2–4.1 ms / 9.6–12.7 ms|2.0–2.6 ms / 0.005–1.05 ms / 6.2–7.3 ms|19.5–24.4 ms|11.9–15.3 ms|
|unary|实施后 r1（停顿）|6.0|22.7 ms / 128.2 ms|102.3 ms / 2.7 ms / 673.4 ms|136.3 ms|613.8 ms|
|stream|实施前 r1–r4|13.0–13.3|1.6–2.1 ms / 3.2–5.3 ms|2.1–2.6 ms / 1.3–1.7 ms / 6.8–9.7 ms|21.8–28.0 ms|27.4–33.9 ms|
|stream|实施后 r1、r2、r4|6.0–6.06|3.3–3.5 ms / 7.7–10.8 ms|1.4–1.7 ms / 0.006–0.35 ms / 4.6–8.7 ms|19.8–20.9 ms|8.2–10.6 ms|
|stream|实施后 r3（停顿）|6.0|24.8 ms / 93.1 ms|10.1 ms / 0.003 ms / 8.9 ms|148.5 ms|60.4 ms|

结论：同一负载下，Observation 的租约次数减半；每 Run 总排队时间下降约 70%（unary 40.4 → 13.4 ms，stream 30.8 → 9.4 ms，均为各自重复的均值，新二进制不含停顿 case），总持有时间下降约 18–26%（unary 29.5 → 21.7 ms，stream 24.9 → 20.4 ms），代价是单次持有更长（均值约 +1.1–1.5 ms，p95 约翻倍）。非 Observation 写入（Turn Chain、统计等）的排队均值没有变差（unary 实施前 2.8–7.5 ms，实施后 2.8–4.0 ms；stream 实施前 2.6–3.5 ms，实施后 2.2–3.0 ms）。**两次停顿**：新二进制的 unary r1 与 stream r3 各有一个 case 出现数十到数百毫秒的持有；停顿期间 `sqlite.write_gate.hold` 内耗时最长的是 `sql.transaction.commit`（例如 451 ms 的持有里 447 ms 是提交）与非 Observation 写入自己的 `sql.query`（最长 425 ms），Observation 与其余写入同时受影响；unary r1 的 e2e p95 因此升到 23.5 s（正常 11.7–11.9 s）。基线 8 个 case 没有出现，新二进制 8 个 case 中出现 2 个。停顿集中在 SQLite 提交（磁盘同步）上，且所有写入者同时受影响，更像数据目录所在联合文件系统的同步停顿（r3 还紧接在 cargo 构建之后，磁盘写回可能未结束），而不是 Observation 写入路径的行为；但没有额外探针（例如同时记录磁盘同步延迟）去证明，原因未定，如实保留两个 case 并单列。

### PostgreSQL 32 worker 固定并发（`pg`）

openai-chat → openai-chat，`--mode fixed-workers --concurrency 32 --warmup 5 --duration 120 --seed 1`，unary 与 stream 各一个 case；二进制各重复 2 次。**stream case 被 harness 的节奏门禁作废**（`synthetic upstream missed token pacing budget`，`--max-pacing-lag-ms 250`），四次运行都是如此，基线与新二进制相同；缩小输入、去掉图片、固定节奏、并发降到 16/24 的探测运行同样没有通过门禁（32 worker 时最大滞后 496–558 ms，门禁为 250 ms；探测结果未保留），说明单进程 Python mock 无法在 32 个并发 stream 下保持要求的节奏，是 harness 的限制，不是被测服务的问题，因此 PostgreSQL stream 的尾延迟记为未测，不放宽门禁。unary case 有效（已完成请求 110–112，凭据泄漏 0）：

|unary|端到端 p50 / p95 / p99|准入阶段 p50 / p95 / p99|
|---|---:|---:|
|实施前 r1|42.9 / 62.5 / 67.7 s|66 / 485 / 552 ms|
|实施前 r2|42.8 / 62.5 / 67.7 s|64 / 483 / 568 ms|
|实施后 r1|42.8 / 62.7 / 67.6 s|68 / 438 / 542 ms|
|实施后 r2|42.9 / 62.7 / 67.6 s|63 / 386 / 413 ms|

端到端延迟由 mock 上游 10–60 s 的输出时长决定，看不出差别；准入阶段（写入与排队相关的部分）实施后 p95 为 386–438 ms，实施前 483–485 ms；每个二进制只有 2 次重复，且基线 r2 的运行内有约 2 分钟的外部单核占用，只作为“没有变差”的证据，不当作改善幅度。

### PostgreSQL 并发写入与 GC 争用（`contention`）

`measure_pg_contention`：32 个 worker 持续 120 s 的 unary 请求，同时以 0.5 s 间隔交替把 `log_retention_days` 设为 7 和 8 强制扫描（每次扫描内部串行，请求在上一次完成后才发下一次），各 2 次。所有请求 200，错误 0：

|二进制|请求数 / QPS|e2e p50 / p95 / p99|准入 p95|扫描次数（PUT 墙钟 p50 / p95）|`pg_stat_database` 死锁 / 回滚|服务端日志中的死锁与观察写入失败|有 Lock 等待的采样（250 ms 间隔）|每请求提交数|
|---|---:|---:|---:|---:|---:|---:|---:|---:|
|实施前 c1|3,020 / 24.9|1,225 / 1,648 / 1,868 ms|15.5 ms|24（5.1 s / 10.3 s）|18 / 19|16（另有 3 条 `database_error`）|95 / 499|74.8|
|实施前 c2|2,966 / 24.4|1,254 / 1,635 / 1,860 ms|30.1 ms|19（7.0 s / 8.7 s）|10 / 11|9（另有 2 条）|68 / 490|73.6|
|实施后 c1|2,938 / 24.2|1,286 / 1,612 / 1,853 ms|19.8 ms|19（6.1 s / 11.8 s）|0 / 1|0|13 / 484|69.0|
|实施后 c2|2,998 / 24.7|1,245 / 1,604 / 1,864 ms|19.2 ms|37（1.7 s / 9.4 s）|0 / 1|0|13 / 510|69.7|

实施前两次运行都有死锁（服务端日志里是 `deadlock detected`，对应 Observation 的 tail、完成与事件写入失败；请求本身都返回 200）；实施后两次均为 0 死锁，`clean` 为真（FK、序列化、唯一约束等错误类别全为 0）。采样到的 `Lock:transactionid` 等待实施前合计 158 与 93 次，实施后为 0，剩下的只有 `Lock:advisory`（PostgreSQL 写协调，30–64 次对 32–41 次）。请求尾延迟两侧相当。强制扫描的 PUT 墙钟在两侧都很长（秒级），因为扫描与 32 个写入者争用；这是在持续写入压力下的值，不是空闲扫描耗时。实施前 `sweep_spans_valid` 为假是因为基线二进制没有 `history.sweep` span（扫描耗时只看 PUT 墙钟）。每次运行 1 个数据库、一次性，样本量小，不外推到更大并发。

### 升级（`upgrade-gen` / `upgrade`）

**升级前库。** `scale_upgrade_fixture` 用新二进制所在提交的 `stravia-storage-test-harness` 在空库里克隆已录制的夹具单元，写成与升级前（schema 36，`legacy-adr0076-turn-contents`）一致的存量；夹具 SHA256 `eb0a68ddf88e8cd0ef6d786434ba95eb7766ee463eb6325d416720ca87e354b8`。目标规模 490000 / 178000 / 17000，达成 `observation_events` 489,997、`turn_contents` 177,833、`history_markers` 16,999（另有 `turn_nodes` 35,846、`turn_node_contents` 294,639、`interaction_observations` 32,654、`inference_run_observations` 40,845）；SQLite 668,262,400 B，PostgreSQL 数据库 1,006,107,671 B。抽样的 500 个内容做了摘要校验，500 个事件做了解码校验，用旧（实施前）二进制经管理 API 读出 40 个抽样 Interaction 无失败。这是克隆的合成存量，不是开发库的真实分布。升级后：`observation_events` 408,461（合并 attempt 与去掉 `usage_confirmed`）、`contents` 179,410、`history_marker_contents` 16,999、`observation_blocks` 46,661、`observation_event_contents` 79,315、`observation_tool_results` 16,066、`content_scopes` 6。

**升级成本**（新二进制启动并经 `setup/complete` 触发迁移；`getrusage` 的子进程资源，`/proc` 的峰值 RSS 与 5 秒采样的磁盘占用；每后端 2 次，副本不复用）：

|后端|墙钟|服务端 CPU|峰值 RSS（VmHWM）|峰值磁盘|
|---|---:|---:|---:|---:|
|SQLite r1 / r2|74.1 / 109.3 s|75.2 / 85.3 s|590 / 758 MB|1,150,337,905 / 1,150,560,996 B（库文件 787,935,232 B，其余为 WAL；升级前 668,262,400 B）|
|PostgreSQL r1 / r2|137.9 / 87.2 s|38.0 / 39.7 s|562 / 571 MB|数据库 1,472,315,919 / 1,471,978,519 B（升级前 1,006,107,671 B），WAL 目录峰值 1,073,741,824 B|

两次重复的墙钟差距很大（SQLite +47%，PostgreSQL −37%），原因未查明，只报区间，不给均值。服务端日志中的阶段标签：`Converting shared content storage` 到下一个 `Applying database schema migrations` 之间 SQLite 约 68.7 / 104.6 s、PostgreSQL 约 134.9 / 84.3 s，其后到 `Verifying database integrity` / `Releasing database migration lock` 之间约 2.0–3.4 s，迁移总计 `elapsed_ms` 为 73,258 / 108,404 与 136,946 / 86,317；这些标签之间的区间包含 Rust 钩子的转换，但标签只在阶段开始处打印，转换各阶段（内容、保留期回填、Observation 规范化）的耗时没有单独记录。升级结束时 SQLite 库文件比升级前大 17.9%（`observation_events` 含索引 223 MB → 312 MB）、PostgreSQL 大 45.8%（死元组），推断是转换按行 UPDATE 重写，释放的空间留在页内或成为死元组（未单独验证）；回收需要 VACUUM：`VACUUM INTO` 后 SQLite 587,382,784 B，对照同一存量在旧布局上同样 `VACUUM INTO` 的 646,275,072 B（−9.1%）；PostgreSQL `VACUUM FULL` 后 851,672,087 B，对照旧布局 880,065,559 B（−3.2%）。升级本身不执行 VACUUM，升级后文件占用高于升级前、直到回收，是运维需要知道的成本。

**空库迁移。** 全新空库上 36 → 42 个迁移的 `_sqlx_migrations.execution_time`（release 二进制，从空闲机上的租约运行的新建库读取，每侧 2 个库）：实施前总计 964–1,036 ms（`0033` 664–667 ms 为最大项，另有 `0030` 约 60 ms）；实施后 1,715–1,719 ms，新增 `0038` 94–95 ms、`0040` 185–188 ms、`0042` 476–484 ms，三者合计约 0.76 s，其余新迁移均低于 50 ms。对应的单元测试 `sqlite_baseline_applies_to_empty_database_and_is_idempotent`（debug 构建，内存 SQLite，含一次幂等重放）实施前 1.01 / 0.90 / 0.88 s，实施后 1.74 / 1.72 / 1.73 s，增量约 0.85 s，与上面迁移的增量同量级。

### 扫描耗时（`sweep`）

在上述升级库的副本上，基线二进制与新二进制各启动一次，`steady-repeats 3`、`lower-to 1`：先 3 次稳态（`log_retention_days=30`），再调低到 1 天的首轮，再 1 次稳态。基线副本是同一升级前存量经基线二进制启动（`setup/complete`）后的库，新副本是上面 `KEEP_UPGRADED=1` 的升级结果；每个二进制各 2 个副本。基线的扫描 span 是 `observation.maintenance.sweep`（`SWEEP_SPAN`），新二进制是 `history.sweep`；两者范围不同，基线只含 Observation，另外还有 `observation.maintenance.update_retention`：基线代码（`retention.rs`）在保存天数时对 `interaction_observations`、`inference_run_observations`、`observation_events`、`rejected_request_observations` 四张表逐行 `UPDATE … SET expires_at`，新二进制没有这一步（修改天数不重写任何行）。

|后端|二进制|稳态扫描 span|缩短天数后首轮扫描 span|保留期更新 span（稳态 / 首轮）|PUT 墙钟（稳态 / 首轮）|SQL 查询数 / 次扫描|
|---|---|---:|---:|---:|---:|---:|
|SQLite|实施前 r1 / r2|112.6–156.7 / 109.4–157.8 ms|113.1 / 113.4 ms|188–193 ms / 2,754 / 1,238 ms|312–361 ms / 2,879 / 1,362 ms|1|
|SQLite|实施后 r1 / r2|117.7–124.2 / 121.0–126.6 ms|124.9 / 138.3 ms|0|126–134 ms / 135 / 148 ms|28（首轮 61）|
|PostgreSQL|实施前 r1 / r2|161.5–177.0 / 160.9–199.3 ms|180.7 / 188.3 ms|5,917–8,584 ms / 7,890 / 7,693 ms|6.1–8.8 s / 8.1 / 7.9 s|1|
|PostgreSQL|实施后 r1 / r2|128.8–133.5 / 131.4–133.0 ms|142.4 / 169.0 ms|0|137–142 ms / 151 / 180 ms|34（首轮 75）|

所有 case 的 `dropped_*` 为 0，凭据泄漏 0。**局限：** 夹具里所有行的时间都在 1 天之内，而 `log_retention_days` 的最小值是 1，所以缩短天数后的首轮只删除了 3 行（Marker、Marker 内容与可逆脱敏映射各 1），基线删除 0 行——这次测的是扫描本身的固定成本（span 内 active 部分约 55–80 ms），**不是**大批删除的成本。要测批量删除需要时间戳回溯的存量，本次没有，未测。基线的 PUT 墙钟里保留期更新占大头（SQLite 缩短天数后 1.2–2.8 s，PostgreSQL 6–8.6 s），新二进制没有这一步，这是设计上“修改天数不重写任何行”的实测体现。

### 需要留意的两点

没有发现需要停下的产品缺陷。一是升级后文件/数据库占用先升，需 VACUUM 才降（见上）；二是租约停顿 case 的原因未定，不能排除新二进制较长的单事务更容易撞上磁盘同步停顿（单租约持有变长这一点是确定的）。
