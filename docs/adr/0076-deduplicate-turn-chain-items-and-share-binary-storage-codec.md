---
status: accepted
---

# Turn Chain 按内容项去重并共用二进制 storage codec

Turn Chain 的不可变节点继续拥有模型历史事实，存储表示改为 format 2：同一 Principal 内完全相同的 raw JSON 内容项共享一条内容记录，节点只保存结构与引用。这样消除完整窗口与重复配置的存储成本，而不改变 Canonical Item 语义、payload_version、父链、前缀匹配、保留期或物化结果；普通思考与工具正文仍保留在普通存储，不依赖 Debug 开关。

外置门槛按 raw JSON 序列化字节计，至少 256B 才抽出；这不是 codec 的 128B 压缩门槛。抽取槽位为 `/client_delta/system`、`/client_delta/messages/i`、`/client_output/i`、`/effective_history_mutation/items/i`、`/client_history_mutation/items/i`、`/effective_output/items/i`、`/effective_system`、`/tools`、Agent 节点的 `/transcript/i`，以及嵌套的 `__open_responses_response_profile` / `__open_responses_effective_request`（含 instructions）。摘要依据该值原始 JSON 序列化字节计算 SHA-256，不用 Cache Affinity 的语义 Canonical Item Hash，也不跨 Principal 共享。

节点 envelope 为 `{"data": <外置槽位为 null 的原 payload>, "slots": [[json_pointer,key_index], ...], "contents": [content_key, ...]}`。contents 中每个摘要只出现一次，slots 保留重复项、数组位置与路径，key_index 指向 contents；嵌套父容器先于子槽恢复。节点 payload 只编码/解码一次，物化按整条链汇集唯一摘要并批量读取内容，验证解码正文 SHA-256 与 content_key 一致后按记录顺序还原。

`turn_chain_contents` 使用 `(principal, content_key)` 唯一键、`(principal,id)` 唯一约束及单调分配的数值 id。`turn_chain_node_contents` 以 `(node_id,content_id)` 为主键，只保存节点对不同内容的引用集合，不保存 path；复合外键 `(node_id,principal)` 与 `(principal,content_id)` 保证归属一致。SQLite 此关联表使用 WITHOUT ROWID。写入先批量查询已存在内容（PostgreSQL `FOR KEY SHARE`），只对缺失项批量 INSERT RETURNING id，并写入不同内容引用；并发插入冲突后定向查询，不用无意义 DO UPDATE，不在热写入重复比较摘要冲突正文。

同一 Gateway 的 SQLite 历史与普通 Observation 写入口共享异步写锁，在取得连接与写事务前排队，持有到提交完成，避免两类写入在 SQLite busy handler 中竞争。历史清理、引用重建与 Observation 生命周期写入使用同一锁；独立只读查询与文件 I/O 不持有它，也不在外部持锁后调用写入口。其他业务存储和其他进程不由此锁协调；PostgreSQL 不使用它，仍遵循数据库锁协议。

节点 envelope、内容行及 Observation payload 共用 `crate::storage_codec::{encode,decode}`。body 后固定追加 14 字节 trailer：第 0 字节 codec（0 raw / 1 zstd），第 1–4 字节 little-endian `u32 dict_id`（当前必须为 0），第 5–12 字节 little-endian `u64` 原始字节长度，第 13 字节版本（1）。原始载荷至少 128 字节且压缩 body 确实更小时才选择 zstd level 3；压缩帧开启 checksum。压缩与解压上下文按线程复用，不共享全局热锁。读取拒绝未知版本、codec、字典、长度不符、损坏帧和多余帧字节。字典槽只是格式预留，不启用字典压缩。

## 升级与回退

冻结的 0001–0005 不改写；0006 引入历史结构，0007 引入观测结构。启动在迁移互斥范围内按以下顺序执行：SQL 至 0005 → SQL 0006 → `crate::turn_chain::upgrade::convert_history_sqlite/postgres(&mut Connection)` → `crate::interaction_observation::upgrade::export_debug_manifests_sqlite/postgres(conn, Option<&Path>)` → SQL 0007 → `convert_event_storage_sqlite/postgres(conn)` → 后续 SQL。钩子以短事务批次幂等转换并可中断续跑；历史每批最多 100 个 `storage_format < 2` 节点，引用、payload 与转换工作状态在同一事务提交，全部完成后以 `DROP IF EXISTS` 删除 `turn_chain_legacy_refs` / `turn_chain_legacy_contents`，最后删除 `legacy_payload` 列；后续启动检测列不存在即跳过，清理中断也可续跑。不能仅凭 SQLx migration 已记录成功就跳过尚未完成的数据转换。

存量 format 0/1 无损转换为 format 2，转换结束后清除旧内容/引用及过渡字段，正常读取不保留旧格式双路径。migration 入口为 `migrate_sqlite/postgres(pool, Option<&Path>)`；运行时与离线维护提供 `Some(paths.diagnostics())`，测试与 schema exporter 的空库可提供 `None`。非空旧 Debug manifest 表在无诊断目录时明确拒绝升级，不能静默丢数据。schema exporter 应用全部迁移及空库钩子，导出最终结构而非拼接 migration 文本。

升级前备份完整数据根及外部 PostgreSQL；数据库、Trace 与本地导入文件必须配套。回退只能恢复备份，旧二进制不能读取新 schema。SQLite 文件物理缩小通过停机后离线 VACUUM；不更改 auto_vacuum、TTL 或增加搜索系统。

## Considered options

- **只压缩节点。** 能缩小重复字节，却仍对相同窗口反复存储与压缩；内容项去重先消除重复，再压缩独有字节。
- **跨 Principal 共享摘要。** 节省更多空间，但增加租户信息泄漏与清理耦合；隔离优先。
- **长期兼容两套格式。** 增加每条读路径的分支与维护成本；采用启动可恢复转换和备份恢复回退。

## Consequences

- PostgreSQL 写入在节点工作之前持有 contents 的 `ROW EXCLUSIVE` 表锁，GC 持有 `SHARE ROW EXCLUSIVE` 表锁，既有内容取号使用 `FOR KEY SHARE`；GC 在锁内确认无引用后删除，避免写入与扫描删除交错，不靠捕获外键异常重试实现正确性。
- 数值 id 便于将来派生索引定位与反查，但不是并发 PostgreSQL 的安全提交水位：序列分配顺序不等于提交顺序，`id > watermark` 可漏掉迟提交事务。搜索、墓碑与可靠增量消费需另行决策，不能把预留 id 宣称为已完成的索引协议。
- 体积数字只是特定快照的估算，不保证压缩率或最终大小。性能验收采用配对 HTTP 回放，首内容按单次配对差值的中位数低于 1 ms 收口；同时独立保留首 Token 与请求耗时的 p50/p95/p99、SQL 开销及历史物化耗时，历史 commit、prefix lookup 与工具去重同样纳入检查。配对差值不代替整体耗时分位数，也不据此宣称所有分位差均低于 1 ms。不采用 3% 等百分比容忍带，不以降级功能规避验收。暖请求使用持续 HTTP 连接；另行保留新建连接的端到端结果与实测 connect 耗时，不能把连接抖动混入存储操作归因，也不能以服务端 span 代替客户端首内容或 EOF 测量。此处只定义门禁，不声明具体基准结果。
- Observation 的事件收敛与文件 manifest 契约见 [ADR-0077](0077-slim-interaction-observation-and-file-debug-manifests.md)。
