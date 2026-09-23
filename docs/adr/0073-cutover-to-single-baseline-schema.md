---
status: accepted
---

# 合并为单一基线 Schema，硬切旧数据库

## 决定

SQLite 与 PostgreSQL 的历史 SQLx migration 合并为单个 `0001_baseline.sql` 基线；运行时不再保留增量升级链。启动时检查 `_sqlx_migrations`：

- 无该表且无任何用户表：空库，应用基线。
- 该表存在但版本集不是 `{1}`：旧版本创建的数据库，明确拒绝启动。
- 无该表但存在用户表：未识别数据，拒绝在其上初始化。

版本相同但 checksum 不一致由 SQLx 自身拒绝。任何旧版数据库都不能就地升级；部署本版本必须使用全新数据库或同版本备份。

## 边界

本决定取代 [ADR-0041](0041-own-database-connection-in-config-file.md) 中"旧布局停机后经 `stravia-tools migrate-data` 复制转换"的路径：`migrate-data` 收敛为仅搬迁与优化当前版本数据根，旧布局与旧 schema 一律拒绝；`--config`（旧 `database.path` 转换）与 `--webview-from`（旧 WebView 目录）入口移除。配置文件作为数据库连接唯一来源、统一数据根等契约本身不变。

随基线切换一并移除读取旧持久化数据的兼容层：历史 payload 版本区间、旧字段名 serde alias（generation 状态中的 `provider` 键、管理 API 的 `route_ids`/`strategy`/`modelsEndpoint`/`modelsSource`、Devin token 响应的 snake_case）、为缺列旧行服务的 `COALESCE` 兜底、Turn Chain 旧内联格式的离线重写入口、旧 trace 记录透传解码、旧 observation `visible_tail` 前端回退，以及无调用方的 `auth_key`/`bearer_auth` 静态认证残留。基线 schema 同步把这些列收紧为 `NOT NULL DEFAULT`。

`storage_format` 与 `payload_version` 列保留为当前格式的显式版本门，不是旧格式读路径；`usage_stats` 的名称兜底与 `is_key_expired` 的文本时间格式是当前写入端仍允许的语义，不属于本决定范围。
