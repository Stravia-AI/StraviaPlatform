---
status: accepted
---

# 冻结基线 Schema，并从基线保留数据增量升级

## 决定

SQLite 与 PostgreSQL 的历史 SQLx migration 已合并为 `0001_baseline.sql`。该基线是受支持数据的兼容性起点，不再修改其内容或 checksum；之后的结构变化通过两后端对应的新增 migration 交付。启动时检查 `_sqlx_migrations`：

- 无该表且无任何用户表：空库，应用基线及全部增量迁移。
- 已应用历史是当前 migration 列表的连续、成功前缀：保留数据并应用剩余迁移。
- 历史含未知版本、缺口或失败记录：明确拒绝启动。
- 无该表但存在用户表：未识别数据，拒绝在其上初始化。

版本相同但 checksum 不一致时，只接受同一 SQL 的完整 LF 或 CRLF 表示：按当前 SQL 分别计算两种换行形式的 SHA-384，历史 checksum 必须匹配其中之一。其他字节、末尾换行数量或历史混合换行造成的不一致仍拒绝，不允许真实 SQL 改动。此规则同时用于 SQLite、PostgreSQL 和离线复制的源历史检查。

确认换行等价后，只在本次迁移 runner 的内存副本中使用历史 checksum，使 SQLx 后续校验与预检一致；不更新 `_sqlx_migrations` 的已有 checksum、安装时间或执行记录。尚未应用的迁移仍使用当前源文件的 checksum。迁移源文件继续由 `.gitattributes` 固定为 LF，不修改已发布 SQL，也不通过重置数据库处理历史换行差异。

基线之前的历史链仍不受支持；已经采用本基线的实例可以就地增量升级。升级前备份完整数据根及外部数据库；不满足新约束的数据使迁移事务失败，不自动删除或修正业务数据。回退使用升级前备份，不能用旧二进制直接打开更新后的 schema。

路由输出模态通过 SQLite、PostgreSQL 对应的 `0021_route_output_modality.sql` 增量迁移添加，不改写冻结基线或已应用 migration 的 checksum。新增的 `output_modality` 只接受 `text`、`image` 或历史待配置的 `NULL`；新建 Route 必须显式选择用途。后续 `0022_default_existing_routes_to_text.sql` 将全部未配置用途的历史 Route（含禁用项和图片生成所引用项）统一回填为 `text`，不再要求逐条重新配置。已明确保存的文字或图片用途保持不变；尚未应用 `0021` 和已经应用 `0021` 的实例均通过同一增量链升级。

迁移保留 Route ID、展示名、Target 绑定、启用偏好、优先级、超时、重试和其他调度参数，也保留图片生成引用。自动回填只补 Route 的用途，不修改 Provider Model 的输出登记，也不绕过目标可用性与模态校验；原先禁用的 Route 不会自动启用。历史图片生成引用若指向被回填为文字的 Route，须将其明确配置为图片用途并修正失配目标，或更换、清空引用；不自动推断图片用途。回填持久化，重启不重置管理员后续选择。升级前应停机并备份完整数据根及外部 PostgreSQL 数据库；回退仍只使用升级前备份，不新增降级转换器。

SQLite 重建表时只在固定连接上临时关闭外键检查，迁移后校验全部外键并恢复检查；失败或取消不能把外键关闭的连接放回池。PostgreSQL 的迁移专用连接在结束时关闭，避免失败或取消遗留会话级迁移锁。

当连续成功历史已覆盖当前 migration 列表时，只校验历史与 checksum，不探测或重新运行已经完成的数据转换。SQLite 在返回前仍启用并确认外键约束，但不在每次普通重启时执行全库 `foreign_key_check`；只有待升级路径保留迁移前、后的全量外键检查。该取舍使无升级的迁移预检不再扫描业务行，但不把启动成功当作全库完整性证明；既有坏数据不会因此自动修复，新写入仍受外键约束。离线复制优化继续执行完整性与外键检查。

## 边界

本决定取代 [ADR-0041](0041-own-database-connection-in-config-file.md) 中"旧布局停机后经 `stravia-tools migrate-data` 复制转换"的路径：`migrate-data` 只搬迁与优化具有受支持迁移前缀的数据根，不修改源 schema，由目标宿主启动时应用剩余迁移。旧布局与基线前 schema 仍拒绝；`--config`（旧 `database.path` 转换）与 `--webview-from`（旧 WebView 目录）入口仍移除。配置文件作为数据库连接唯一来源、统一数据根等契约本身不变。

随基线切换一并移除读取旧持久化数据的兼容层：历史 payload 版本区间、旧字段名 serde alias（generation 状态中的 `provider` 键、管理 API 的 `route_ids`/`strategy`/`modelsEndpoint`/`modelsSource`、Devin token 响应的 snake_case）、为缺列旧行服务的 `COALESCE` 兜底、Turn Chain 旧内联格式的离线重写入口、旧 trace 记录透传解码、旧 observation `visible_tail` 前端回退，以及无调用方的 `auth_key`/`bearer_auth` 静态认证残留。基线 schema 同步把这些列收紧为 `NOT NULL DEFAULT`。

`storage_format` 与 `payload_version` 独立于数据库 schema 版本。Rust 类型变强而有效 JSON 契约不变时，不递增 payload 或 SDK 版本，也不批量重写历史；当前 v6 历史继续可读。真正改变持久化格式时必须明确版本和迁移路径，不能仅靠改 serde 类型让历史失效。`usage_stats` 的名称兜底与 `is_key_expired` 的文本时间格式是当前写入端仍允许的语义，不属于本决定范围。
