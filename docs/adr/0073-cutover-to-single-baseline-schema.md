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

版本相同但 checksum 不一致由 SQLx 自身拒绝。基线之前的历史链仍不受支持；已经采用本基线的实例可以就地增量升级。升级前备份完整数据根及外部数据库；不满足新约束的数据使迁移事务失败，不自动删除或修正业务数据。回退使用升级前备份，不能用旧二进制直接打开更新后的 schema。

SQLite 重建表时只在固定连接上临时关闭外键检查，迁移后校验全部外键并恢复检查；失败或取消不能把外键关闭的连接放回池。PostgreSQL 的迁移专用连接在结束时关闭，避免失败或取消遗留会话级迁移锁。

## 边界

本决定取代 [ADR-0041](0041-own-database-connection-in-config-file.md) 中"旧布局停机后经 `stravia-tools migrate-data` 复制转换"的路径：`migrate-data` 只搬迁与优化具有受支持迁移前缀的数据根，不修改源 schema，由目标宿主启动时应用剩余迁移。旧布局与基线前 schema 仍拒绝；`--config`（旧 `database.path` 转换）与 `--webview-from`（旧 WebView 目录）入口仍移除。配置文件作为数据库连接唯一来源、统一数据根等契约本身不变。

随基线切换一并移除读取旧持久化数据的兼容层：历史 payload 版本区间、旧字段名 serde alias（generation 状态中的 `provider` 键、管理 API 的 `route_ids`/`strategy`/`modelsEndpoint`/`modelsSource`、Devin token 响应的 snake_case）、为缺列旧行服务的 `COALESCE` 兜底、Turn Chain 旧内联格式的离线重写入口、旧 trace 记录透传解码、旧 observation `visible_tail` 前端回退，以及无调用方的 `auth_key`/`bearer_auth` 静态认证残留。基线 schema 同步把这些列收紧为 `NOT NULL DEFAULT`。

`storage_format` 与 `payload_version` 独立于数据库 schema 版本。Rust 类型变强而有效 JSON 契约不变时，不递增 payload 或 SDK 版本，也不批量重写历史；当前 v6 历史继续可读。真正改变持久化格式时必须明确版本和迁移路径，不能仅靠改 serde 类型让历史失效。`usage_stats` 的名称兜底与 `is_key_expired` 的文本时间格式是当前写入端仍允许的语义，不属于本决定范围。
