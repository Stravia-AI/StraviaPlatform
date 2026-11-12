---
status: accepted
---

# Turn Chain 规范化内容寻址、生命周期与二进制 storage codec

> [ADR-0089](0089-share-content-store-and-normalize-observation-writes.md) 修订本文的五处约定：内容表由 Turn Chain 独占改为 Turn Chain、History Marker 与 Interaction Observation 共用的 `content_scopes`/`contents`/`content_gc`；内容身份公式加入 `format` 字节并换用 `content.v1` 域；正文不再按 JSON 键名推断，而由 `stravia-runtime-contract` 中的单个正文单元函数定义；descriptor 槽位增加文本与拼接格式；PostgreSQL 不再对内容表取表级锁。节点期限列、续期与固定 7 天的描述同样由 ADR-0089 的统一保留期取代。下文已按当前状态改写，0032/0033 的历史转换细节原样保留并注明被 0037 及以后迁移取代的部分。

Turn Chain 的不可变节点继续拥有模型历史事实。0032/0033 将内部存储切换为规范化格式 v3：同一 Principal 内完全相同的内容单元共享一条内容记录，公开 TurnNodeId 与内部数值身份分离，节点结构、正文和生命周期分别存放。此格式不改变 Canonical Item 语义、payload_version、父链、前缀匹配或物化结果；普通思考与工具正文仍保留在普通存储，不依赖 Debug 开关。

所有选定语义槽中的非 null 值统一内容寻址，包括短字符串、空字符串、空对象和空数组；删除原有 256B 分流。null 与缺省仍由节点结构表达，opaque Item 或未知扩展不拆成逐标量 EAV。Response 的输入/输出/mutation/profile、Agent transcript 与 WebSearch 的既有语义抽取边界不变；哪些值是文本正文单元（`ContentBlock::Text`、`Thinking`、`Reasoning` 各元素、`ToolResult` 等）由 `stravia-runtime-contract` 的单个函数按 IR 类型定义，不按 JSON 键名推断，Turn Chain、History Marker 与 Observation 共用该定义（[ADR-0089](0089-share-content-store-and-normalize-observation-writes.md)）。内容身份为完整 32 字节 `SHA256(b"stravia.content.v1\0" + format + raw_bytes)`：`format` 为 0（UTF-8 文本，保存原始字节，不做 JSON 转义）或 1（紧凑 JSON），字节相同的文本与 JSON 不合并；使用已有 sha2，不新增 BLAKE3 依赖。0032/0033 曾使用 `b"stravia.turn-content.v1\0" + raw_compact_json`，0037/0038 升级时按新域前缀重算。域版本与存储格式、协议 payload_version 独立；字节去重不是 Cache Affinity 的语义相等，也不跨 Principal 共享。

节点 descriptor 为 `{"data": <外置槽位为 null 的原 payload>, "slots": [slot, ...]}`，不再携带摘要或内容 ID 副本。槽位有三种线格式：`[json_pointer, ordinal]` 是 JSON 单元，`[json_pointer, ordinal, "t"]` 是文本单元，`[json_pointer, ordinal, "t", prefix, suffix]` 是拼接槽位，恢复值为 `prefix + 正文 + suffix`。item 外壳是 JSON 单元，正文是嵌套的文本槽位；跨协议投影文本在写入时用 `history_marker::syntax` 拆成字面量前后缀与正文，先校验拼回后逐字节相同，不同则整串存为一个文本单元，读取得到的就是当初交付的字节。`turn_node_contents(node_pk,scope_id,ordinal,content_id)` 是 ordinal 到数值内容 ID 的唯一权威；同一内容的多个 occurrence 共用 ordinal，slots 保留位置、顺序和路径，父容器先于嵌套子槽恢复。读取检查 ordinal 连续、映射完整、内容归属和摘要，按有界 ID 批次读取缺失正文。存储层向恢复器传原始 codec bytes，仅解码一次；不能把已解压正文再交给 codec。这里仍使用 JSON Pointer descriptor 和逐节点 JSON 恢复，不宣称已实现按 kind 完全 typed、无中间 DOM 的存储读取器。

请求局部 Generation 读取在每个节点恢复后立即消费其 JSON 树并解码为领域节点，不同时保留整条链的展开 JSON 与解码结果。全部节点解码和领域折叠成功后，才允许将领域节点及暂存的外置内容纳入同一个请求局部字节预算；JSON 容器、字符串及任意精度数字保留的字面量均计入，不能只计算 Value 的固定大小。提前解码不等于缓存命中，也不能绕过预算。存储失败与领域解码失败分别传播，失败读取清除本次暂存内容，不污染已验证祖先或跨越 Principal、kind 与有效期边界。普通原始物化仍返回完整独立 JSON。

已解码 Item 用 serde 透明的 `FrozenItem(Arc<AiItem>)` 共享，client/effective 窗口、mutation、响应和引用目录复制句柄而非整棵 JSON。只有真正修改时才 COW，写入边界可直接借用已有请求的 Item slice；已有 owned canonical 结果直接接管。相等判断仍按完整 canonical units 展平，不以 AiItem 分组方式代替语义。共享预算是逻辑估重，不是 allocator 或 RSS 硬上限；不替换 TinyUFO/Redis，也不引入持久内容缓存。

归因只读访问祖先窗口时，累计视图借用已解码的内容项，不逐祖先深拷贝工具正文与元数据；仅缺少历史客户端输出的旧节点生成自有回退内容。Append、Replace 与输入／输出分界仍复用同一套客户端历史选择规则，借用只持续到访问回调结束，窗口预算和完整历史事实不变。

独立 Server 使用不显式申请透明大页的 mimalloc，减少大型历史跨线程释放后的页驻留，并避免在碎片化主机上为透明大页同步整理内存；不更改存储格式、缓存预算、回收延迟或主机配置。真实数据库副本的整体进程测量、CPU 与吞吐口径见[内存回放验证](../research/protocol-http-benchmark.md#真实数据库副本凭据保护与历史读取的整体内存)。

`content_scopes`（0037 前称 `turn_scopes`）将 Principal 映射为不复用的整数身份，由 Turn Chain、History Marker 与 Observation 共用。`turn_nodes` 保存公开 ID、scope、kind、parent_pk、payload_version 与派生前缀；`turn_node_payloads` 保存 immutable descriptor；`turn_node_retention` 只保存 `deleting`，节点不再有到期列（见 ADR-0089 的统一保留期）。父边复合外键约束相同 scope/kind，并延迟到事务提交校验，以便有界删除闭合子图。公开 ID 和业务版本保留，不把内部 ID 当作提交水位。

`contents`（0037 前称 `turn_contents`，不再由 Turn Chain 独占）使用 `(scope_id,digest)` 唯一键、32 字节摘要约束、`format`、`raw_len`、经 `storage_codec` 编码的 `body` 及不复用的数值 ID。`turn_node_contents` 以 `(node_pk,ordinal)` 为主键、`(node_pk,content_id)` 为唯一约束，并以复合外键保证节点和内容的 scope 一致；SQLite 使用 WITHOUT ROWID，content_id 反向索引用于判活。History Marker 与 Observation 通过各自的引用表（`history_marker_contents`、`observation_event_contents`、`observation_tool_results`）与它平级引用同一张表，最终删除只在内容 GC 中重检四个反向引用后进行。写入先查询已有项，只压缩缺失候选；事务内重新裁决并批量插入/取号，PostgreSQL 对已有内容取 `FOR KEY SHARE`，不用无意义 DO UPDATE 或自动重试。

祖先递归查询只携带窄身份和生命周期，最终节点查询同时投影 payload、scope 与整数 ordinal 映射，再按 parent_pk 恢复顺序。Principal/kind 由节点查询核验，恢复器继续校验映射和正文归属；不以 FK 存在为由接受损坏引用，也不重复传输大正文。缺父、环、缺正文或缺生命周期明确失败，不依赖 SQL 或 JSON 聚合输出顺序。节点写入后不再更新任何时间：提交不续期祖先，祖先在子树中仍有未到期叶子时因有子节点而不会被回收（ADR-0089）；不引入 root/head lease、可达计数或周期完整快照。

GC 使用持久 `turn_node_gc` 候选与共享的 `content_gc`。节点候选来自 `created_at` 期限水位扫描与引用归零触发器（子节点删除后登记 parent）；被其他 child 或 native compaction source 保留的节点不可删，删除前重检按当前保留天数已过下限窗口且无引用。每批最多处理 128 个节点、2,048 条引用、512 个内容及 8 MiB 内容字节，单个不可拆大内容例外；节点和内容共享一个有界事务，释放写协调后再取下一批。首批引用删除与 retention.deleting 标记原子提交，并在同一事务内只为失去最后引用的内容登记候选；仍被共享的内容不反复入队和出队。数据库读取与前缀拒绝 deleting，恢复继续处理其持久队列。祖先按需求值，只物化并复用小批节点候选；内容先领取有界候选，再按主键读正文，避免 SQLite 为排序扫描整个正文表。最终删除仍重检 NOT EXISTS，不靠 refcount 推断活性，也不让级联一次删除大节点的全部引用。批次公平性与总 GC CPU 成本分别验证。

同一 Gateway 的 SQLite 写入沿用 pool-owned 写租约，经 `Pool::begin_write` 在取得连接与写事务前排队，持有到提交或回滚完成；业务存储只声明写事务，不接触租约、BEGIN 语句或完成屏障。历史、Observation、Artifact、Marker、凭据映射及管理/后台持久化共用协调，不在 SQLite busy handler 中互相争抢。Gateway 的 SQLite 池在租约外以 `PRAGMA query_only` 保持只读，漏走租约的写入会立即失败，而不是在并发时与租约写者争用写锁。独立只读查询与文件 I/O 不持有租约，也不在外部持租约后再次调用写入口。它不协调其他进程；PostgreSQL 不使用这份租约，仍遵循数据库锁协议。

节点 descriptor、内容 `body`、History Marker 外壳及 Observation 的 `attrs`/`envelope` 共用 `crate::storage_codec::{encode,decode}`。body 后固定追加 14 字节 trailer：第 0 字节 codec（0 raw / 1 zstd），第 1–4 字节 little-endian `u32 dict_id`（当前必须为 0），第 5–12 字节 little-endian `u64` 原始字节长度，第 13 字节版本（1）。原始载荷至少 128 字节且压缩 body 确实更小时才选择 zstd level 1；压缩帧开启 checksum。压缩与解压上下文按线程复用，不共享全局热锁。读取拒绝未知版本、codec、字典、长度不符、损坏帧和多余帧字节。字典槽只是格式预留，不启用字典压缩。

PostgreSQL 的 `turn_node_payloads.payload` 与 `contents.body`（0037 前为 `turn_contents.content`）延续旧表的 `STORAGE EXTERNAL` 策略：TOAST 可外置大值，但不再尝试压缩已由应用 codec 编码的正文。请求工作区、物化缓存与 precommit 复用同一 JSON 堆估重，按保留容器而非编码长度准入；数字词法、内容条目的 Arc 头与槽位均计费。解码 delta 的响应 vendor metadata 与错误也属于工作区预算，不靠再序列化整节点估算。

压缩默认等级由 3 调整为 1，只影响新写入帧的编码成本；等级不作为 trailer 的版本或解码条件。存量 level 3 帧原样读取，不触发数据重写、schema 迁移或指纹重算，历史与普通 Observation 均保持启用。选择依据及实测限制见[协议 HTTP 基准](../research/protocol-http-benchmark.md#生产默认切换为-level-1-与-sha256-后端筛选)。

## 升级与回退

### 历史 0007/0008 钩子

冻结的 0001–0006 不改写；0006 保留模型规格升级，0007 引入历史结构，0008 引入观测结构。启动在迁移互斥范围内按以下顺序执行：SQL 至 0006 → SQL 0007 → `crate::turn_chain::upgrade::convert_history_sqlite/postgres(&mut Connection)` → `crate::interaction_observation::upgrade::export_debug_manifests_sqlite/postgres(conn, Option<&Path>)` → SQL 0008 → `convert_event_storage_sqlite/postgres(conn)` → 后续 SQL。钩子以短事务批次幂等转换并可中断续跑；历史每批最多 100 个 `storage_format < 2` 节点，按 ID 游标推进并批量读取旧引用。有界 blocking worker 并行还原 JSON、计算摘要、编码 envelope，并对批次唯一内容压缩；worker 数随可用处理器确定，最多 8 个。单一连接串行写入，引用、payload 与转换工作状态在同一事务提交，全部完成后以 `DROP IF EXISTS` 删除 `turn_chain_legacy_refs` / `turn_chain_legacy_contents`，最后删除 `legacy_payload` 列；后续启动检测列不存在即跳过，清理中断也可续跑。不能仅凭 SQLx migration 已记录成功就跳过尚未完成的数据转换。

旧引用工作表在仍有 format 1 节点时建立迁移期 `(node_id, principal)` 索引，保证批量查询同时按节点和归属定位；不能只依赖 `(principal, content_key)` 索引，否则 SQLite 可能为每个节点扫描该 Principal 的全部旧引用。删旧表时同时移除此索引，不改变最终 schema 或既有 migration checksum。所有节点已转换、但旧表清理中途被关闭时，不再尝试为已删除的旧表建索引，继续完成剩余清理。

存量 format 0/1 先无损转换为历史 format 2；该冻结转换器仍使用旧的无域 SHA-256 与 256B 规则，仅为后续统一转换建立确定输入，不是正常运行的备用读路径。migration 入口为 `migrate_sqlite/postgres(pool, Option<&Path>)`；运行时与离线维护提供 `Some(paths.diagnostics())`，测试与 schema exporter 的空库可提供 `None`。非空旧 Debug manifest 表在无诊断目录时明确拒绝升级，不能静默丢数据。schema exporter 应用全部迁移及空库钩子，导出最终结构而非拼接 migration 文本。

### 0032/0033 规范化切换

本节描述 0032/0033 当时的转换，其中 `turn_scopes`/`turn_contents` 表名、`turn-content.v1` 摘要域和节点到期期限已由 0037/0038（共享内容存储）与 0039/0040（引用保留）取代，见下节「0037–0042 后续升级」。

0001–0031 checksum 不改。完成既有 0030 模型字段 pinning 与 0031 后，先提交 0032 结构，再由 `turn_chain::upgrade::normalize` 按持久阶段/游标继续：薄节点与 scope → 校验旧摘要并计算新域摘要 → descriptor、父边、retention 与 ordinal 关联 → 全量结构验证和孤儿清理。阶段 4 完成后才允许 0033 切换 native compaction 外键并删除旧表/工作表。0032 已提交不等于数据转换完成；0033 已成功的普通重启不再访问已删除的旧表。

每批数据与游标同事务提交；阶段 2 必须核对旧 descriptor 声明与旧引用表恰好相等，不能根据同 Principal 的内容存在“修复”缺失或多余引用。共享 profile 按 `(Principal, key)` 一批只解码/验证一次，profile 内部局部内容序号不能混成合并后的批次偏移。先读 encoded 长度与 codec decoded-size hint，以两者较大值及唯一 profile 依赖选连续节点前缀，再读取正文；8 MiB 是逻辑源字节预算，不是 RSS 上限，首个不可拆节点及依赖可超预算。hint 不替代实际 frame、长度、checksum 与内容摘要验证；未选中的尾部必须留给下一批，不能因取回行数少就提前结束阶段。

验证保留全部公开身份、Principal/kind、父边、payload_version、期限、前缀 metadata 和精确 JSON 值。PostgreSQL 旧 INT4 字段进入 i64 行类型时必须显式投影 BIGINT，非空值与 NULL 均覆盖；SQLite 不足以证明这项兼容性。孤儿清理按单调内容 ID 游标有界推进，不回扫整个 live 集合，只清真正无节点和内容的 scope。普通运行只读新结构，不保留 fallback。

升级前备份完整数据根及外部 PostgreSQL；数据库、Trace 与本地导入文件必须配套。回退只能恢复备份，旧二进制不能读取新 schema。SQLite 文件物理缩小通过停机后离线 VACUUM；不更改 auto_vacuum 或增加搜索系统。

### 0037–0042 后续升级

[ADR-0089](0089-share-content-store-and-normalize-observation-writes.md) 在同一套约束上继续：0037/0038 以 Rust 钩子 `content_upgrade`（`stravia-core` 顶层模块，转换需要理解 Turn Chain 与 Marker 语义，所以不放在与语义无关的 `content_store` 内）按持久进度（`content_upgrade`，节点 → Marker → 校验）把内容表改为共享的 `contents`，用新域前缀重算摘要，并把 JSON 单元中的文本正文拆成文本槽位与拼接槽位；0039/0040 删除节点与依赖的到期列并回填引用；Observation 规范化为 0041/0042。均为一次性离线重写，可中断续跑，不提供边读边迁的读取路径，回退只能恢复升级前备份。

## Considered options

- **只压缩节点。** 能缩小重复字节，却仍对相同窗口反复存储与压缩；内容项去重先消除重复，再压缩独有字节。
- **跨 Principal 共享摘要。** 节省更多空间，但增加租户信息泄漏与清理耦合；隔离优先。
- **长期兼容两套格式。** 增加每条读路径的分支与维护成本；采用启动可恢复转换和备份恢复回退。

## Consequences

- PostgreSQL 内容表不取表级锁：写入对已有内容用 `FOR KEY SHARE`，缺失时 `INSERT … ON CONFLICT DO NOTHING RETURNING id` 并在冲突后以 `FOR KEY SHARE` 重读；GC 以 `FOR UPDATE SKIP LOCKED` 领取候选，再用新语句重检四个反向引用后删除，被在途写入锁住的内容会被跳过，写入方等待的行已被删除时重读为空并重新插入。`turn_nodes` 的节点提交与 GC 锁序不变。SQLite 使用同一写协调与 BEGIN IMMEDIATE。正确性不靠捕获外键异常重试实现（[ADR-0089](0089-share-content-store-and-normalize-observation-writes.md) 取代了此前对内容表的 `ROW EXCLUSIVE`/`SHARE ROW EXCLUSIVE` 表锁及原生压缩续期的 `turn_nodes` 表锁）。
- 数值 id 便于将来派生索引定位与反查，但不是并发 PostgreSQL 的安全提交水位：序列分配顺序不等于提交顺序，`id > watermark` 可漏掉迟提交事务。搜索、墓碑与可靠增量消费需另行决策，不能把预留 id 宣称为已完成的索引协议。
- 体积数字只是特定快照的估算，不保证压缩率或最终大小。性能验收采用配对 HTTP 回放，首内容按单次配对差值的中位数低于 1 ms 收口；同时独立保留首 Token 与请求耗时的 p50/p95/p99、SQL 开销及历史物化耗时，历史 commit、prefix lookup 与工具去重同样纳入检查。配对差值不代替整体耗时分位数，也不据此宣称所有分位差均低于 1 ms。不采用 3% 等百分比容忍带，不以降级功能规避验收。暖请求使用持续 HTTP 连接；另行保留新建连接的端到端结果与实测 connect 耗时，不能把连接抖动混入存储操作归因，也不能以服务端 span 代替客户端首内容或 EOF 测量。此处只定义门禁，不声明具体基准结果。
- Observation 的事件收敛与文件 manifest 契约见 [ADR-0077](0077-slim-interaction-observation-and-file-debug-manifests.md)。
