---
status: accepted
---

# 集中管理 MCP 服务、多作用域入口与 HTTP 转 MCP 桥接（商业版扩展架构）

> **发行边界说明**：社区版（CE）仅保留最小内置 `/mcp` 服务端（暴露内置平台工具，由 API Key `mcp_access_enabled` 控制）及通用 SPI 扩展缝（`McpCatalogProvider`、`DynamicPlatformToolLoader`、`GatewayExtensionFactory`、`HttpAppExtensions`、WebUI `#webui-extensions`）。外部 MCP 接入管控、HTTP 转 MCP 桥接、分组与 API Key 细粒度 MCP 策略的完整实现、独立迁移（`_stravia_ee_migrations`）与管理界面物理位于商业版仓库 `StraviaPlatformEE`，并由离线签名 License 的 `mcp_external` / `mcp_http_bridge` capability（Pro / Enterprise）在控制面与数据面双重门禁控制。

Stravia 商业版在既有内置 `/mcp` 服务端之上引入集中式 MCP 管理：按「内部 MCP」「外部 MCP（Streamable HTTP）」「HTTP 转 MCP 服务」三类组织工具，支持「自定义分组」与 API Key 细粒度 MCP 访问范围；三类能力统一经 `/mcp*` 暴露，外部 MCP 额外支持在显式启用后走模型推理的 `Transparent Injection`（透明工具调用），所有外部与 HTTP 工具执行统一复用 [ADR-0082](0082-expose-queryable-tool-executions-after-bounded-wait.md) 的有界同步等待与后台执行交接机制，且默认执行超时与平台当前后台执行超时保持一致（900 秒 / 15 分钟）。

## 决定

- **分命名空间多作用域入口。** 保留统一入口 `/mcp`（向后兼容：聚合当前 Key 有权访问的全部已启用工具，关闭 `mcp_access_enabled` 时保持握手成功并返回空列表以配合 `tools/list_changed`），同时新增三类独立作用域入口：`/mcp/category/<internal|external|http>`、`/mcp/server/<slug>`、`/mcp/group/<slug>`。独立入口遇到未知 slug 返回 404，遇到已禁用或未授权返回 403；实际可见工具为「入口作用域 ∩ API Key 授权范围并集」。
- **聚合入口命名空间前缀。** 单服务入口 `/mcp/server/<slug>` 保持服务内工具名；所有多服务聚合入口（`/mcp`、`/mcp/category/*`、`/mcp/group/*`）中，内部 MCP 保持无前缀原名（`StraviaRead`、`generate` 等），外部 MCP 与 HTTP 服务统一以 `<tool_prefix>__<tool_name>` 暴露（`tool_prefix` 默认等于 `slug` 且全局唯一），消除跨服务重名冲突。
- **外部 MCP 支持透明工具调用（`Transparent Injection`）。** 外部 MCP 服务提供显式开关 `allow_transparent_injection`（默认关闭），API Key 的 `Transparent Injection` 集合新增 `external_mcp` 选项及可选的服务/分组范围（与 `/mcp*` 访问范围解耦）。仅当服务启用且开启 `allow_transparent_injection`、API Key 开启透明注入总开关并勾选 `external_mcp` 且命中配置范围时，对应非 `stale` 外部 MCP 工具才自动注入模型推理请求，沿用 `<tool_prefix>__<tool_name>` 经碰撞安全避让命名并由平台隐藏循环执行；HTTP 转 MCP 服务暂仅经 `/mcp*` 暴露。
- **复用既有后台执行交接逻辑，且默认超时与后台执行超时一致（900 秒）。** 外部 MCP 与 HTTP 转 MCP 服务均提供可配置执行超时 `timeout_secs`，默认与平台当前后台工具执行超时一致——**900 秒（15 分钟）**，允许配置 `1–900` 秒（不突破既有 `public_tool_executions` 的 900 秒围栏上限；HTTP 工具可覆盖所属服务超时）。工具执行统一复用 [ADR-0082](0082-expose-queryable-tool-executions-after-bounded-wait.md) 的有界同步等待与后台执行模块（默认 60 秒同步等待窗口；若配置的 `timeout_secs` ≤ 60 秒则按配置超时直接收口）：快速完成直接返回原结果；超出同步等待窗口且仍在配置超时内时自动持久化转后台并交付 `stravia://tool-executions/<id>`，由调用方或模型经 `StraviaRead` 查询终态结果，完整共享既有取消分界、Principal 隔离与独立保留期，不另建第二套异步任务框架。
- **两级 HTTP 转 MCP 模型与管理员可信出站边界。** HTTP 接口按「HTTP 服务（公共 Base URL + 认证 + 静态/敏感 Header）→ 多个工具」两级建模，与外部 MCP 结构对称。由于目标地址由实例管理员显式配置，允许访问本机与私网地址；运行时锁定 scheme/host 并对 Path 参数做百分号编码，防止工具调用参数引发路径穿越或 host 篡改。
- **复用 Artifact 的 Multipart 附件与纯文本解码注入。** 不新增独立文件上传协议，直接复用 `POST /v1/artifacts` 与 ADR-0048 的 Upload Grant 机制（工具 schema 仅引用 `stravia://artifacts/<id>` 或 55 位 Artifact ID，不向模型提示使用 API Key 上传）。`multipart/form-data` 文件字段支持单文件、多文件数组及可选自定义 `filename`，按当前 Principal 读取原始字节与 MIME 组装上传；JSON「附件文本绑定」按 JSON Path 递归定位字段，遇到 Artifact 引用时按 Principal 校验并严格纯文本解码注入（上限 10 MiB，非纯文本、越权或缺失直接报错），遇到普通字符串原样透传；HTTP 二进制或超长（> 512 KiB）响应自动落盘为当前 Principal 的 Artifact 并返回引用。
- **外部 MCP 同步保留本地覆写与凭据遮盖。** 外部 MCP 重新同步时保留管理员的自定义工具名、描述覆写与启停状态，仅刷新上游 schema 与新增工具，上游移除的工具标记为 stale 而非静默删除。服务级敏感凭据与敏感 Header 按管理端遮盖边界存储，API 永不回传明文，编辑留空即保留原值。
- **可插拔元数据生成扩展点。** 在 `stravia-ee-mcp` 定义异步 `McpMetadataGenerator` trait，本期提供确定性规则生成器（服务于 cURL / OpenAPI / `mcpServers` JSON 导入与手动刷新元信息），为后续接入平台模型润色预留 `MetadataGenerationOptions` 契约，不耦合数据库与 HTTP 路由结构。
- **API Key 向后兼容的细粒度授权与 Schema 隔离。** 商业版在独立迁移账本 `_stravia_ee_migrations` 中维护 `ee_mcp_*`、`ee_api_key_mcp_policies`、`ee_api_key_mcp_scopes` 与 `ee_api_key_transparent_mcp_scopes` 表（通过 `api_key_id REFERENCES api_keys(id) ON DELETE CASCADE` 关联），绝不修改社区版 `api_keys` 表结构或 `_sqlx_migrations`；既有开启 `mcp_access_enabled` 的 Key 缺省策略为允许全部已启用 MCP，零破坏升级。

## Considered Options

- **扁平路径 `/mcp/<slug>`**：被否。需要维护分类保留字且服务与分组 slug 互相抢占；分命名空间路径语义清晰，管理页与连接页均提供一键复制。
- **附件文本绑定走 Office/PDF 转换**：被否。调用方常见场景是把长纯文本/Markdown/代码/JSON 文章避免塞进 JSON 载荷；按纯文本严格解码既保真又零新增生产依赖，非纯文本直接报错避免隐式内容变形。
- **外部 MCP 同步全量覆盖**：被否。每次上游同步都会抹除管理员定制的中文描述、别名与启停选择。
- **为长耗时 MCP 另建独立异步任务表或直接无限同步阻塞**：被否。直接同步阻塞超过 60 秒易触发客户端或反向代理断连；复用 ADR-0082 既有 `tool_execution` 后台交接与 `StraviaRead` 查询契约，让 `/mcp*` 与 `Transparent Injection` 推理调用共享一致的取消、持久化与恢复语义，零平行框架。

## Consequences

- 平台 MCP 注册表与 `PlatformToolRegistry` 均从启动期静态只读快照演进为支持运行时热更新与动态解析的目录，支撑外部 MCP 走 `Transparent Injection` 与多作用域 `/mcp*` 路由。
- 社区版保留最小内置 `/mcp`（`BuiltinMcpCatalogProvider`），商业版通过独立 `stravia-ee-mcp` crate 与 `_stravia_ee_migrations`（`0002_ee_mcp_management.sql`，SQLite 与 PostgreSQL 同步）落地集中式 MCP 管理；删除服务、工具或分组时在事务内同步清理多态引用（`ee_mcp_group_items`、`ee_api_key_mcp_scopes` 与 `ee_api_key_transparent_mcp_scopes`），删除 API Key 时依赖外键 `ON DELETE CASCADE` 自动清理。
- 外部 MCP 暂只支持 Streamable HTTP（导入 `mcpServers` JSON 遇到 `stdio` 时明确标明跳过原因），后续扩展新传输类型只需扩展服务 `kind`/配置解析。
