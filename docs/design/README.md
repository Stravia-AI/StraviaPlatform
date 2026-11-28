# 设计文档地图

`docs/design/` 描述 Stravia **当前生效**的系统设计与模块契约。每项主题只有一个权威文档；跨文档引用一律使用链接，不重复抄录规则、SQL 列定义或源码签名。

- **名词定义**见根目录 [`GLOSSARY.md`](../../GLOSSARY.md)
- **架构决策（为什么这么选）**见 [`docs/adr/`](../adr/README.md)
- **数据库最终结构参考**见 [`docs/database/sqlite.sql`](../database/sqlite.sql) 与 [`docs/database/postgres.sql`](../database/postgres.sql)
- **发版与插件目录签名流程**见 [`docs/releasing.md`](../releasing.md)
- **上游协议抓包与基准证据**见 [`docs/research/`](../research/README.md)

## 文档职责边界

| 文档 | 唯一负责范围 | 不负责 |
| --- | --- | --- |
| [architecture.md](architecture.md) | 系统总览、端到端请求管线、协议转换与 Hook 运行时、路由/限流/模型目录、共享出站代理、Nix 部署与 `patch-crate` 开发准备 | 观测明细、可逆脱敏细节、插件运行时细节、SQL 表结构逐列说明 |
| [data-layout-performance.md](data-layout-performance.md) | SQLite/PostgreSQL 存储后端、TinyUFO/Redis 派生缓存与 Server 配置、共享正文存储与引用回收、Generation Chain 物化、持久化与迁移原则 | 观测事件语义、具体数据库列定义 |
| [open-responses.md](open-responses.md) | Open Responses 2026-04-24 日期协议契约、Canonical Item 与表达能力省略、HTTP/SSE/WebSocket、Response Chain / Target Continuation、Responses 原生压缩 | 通用网关 Hook、自定义插件运行时 |
| [vendor-plugins.md](vendor-plugins.md) | Vendor 身份与信任边界、Wasmtime Component / WIT 契约、受控出站网络（rama + patched rustls / Egress Shape / Slot Header）、自定义协议与思考等级映射、核心供应商集成要点 | 插件市场分发与安装生命周期、发版签名步骤、图片生成适配器细节 |
| [plugin-marketplace.md](plugin-marketplace.md) | 插件源与签名目录信任、`.wasm.zst` 分发、市场/本地导入/内置插件的统一安装、更新、覆盖与卸载生命周期 | 插件发布构建与签名操作（见 `releasing.md`） |
| [agent-core.md](agent-core.md) | 内置 Agent 运行时与 Turn Chain、**平台能力门禁与透明注入集合**、**`StraviaRead` 统一读取语法与分页**、**Artifact 身份/单请求上传/授权**、**工具执行有界等待与后台交接** | 各具体能力（搜索/多模态/媒体生成/历史回看）的业务细节 |
| [web-access-providers.md](web-access-providers.md) | `WebProviderFactory` 宿主扩展缝、外部 Web Provider 注册/校验/调度、脱敏管理视图与公网抓取安全边界 | 高层搜索报告与研究循环语义 |
| [web-search.md](web-search.md) | 联网搜索与网页读取契约、Search Report/Turn/Source 续接、Local Agent 与 External Route 后端语义 | 通用 `StraviaRead` 语法与门禁规则、底层 Web Provider 注册 |
| [media-understanding.md](media-understanding.md) | 多模态理解报告与派生物、图片规范化与上游 WebP 支持边界、Office 文档提取与 ZIP 预检安全边界 | 通用 `StraviaRead`、Artifact 上传与门禁规则 |
| [media-generation.md](media-generation.md) | `StraviaGenerate` 媒体生成工具契约、`image_generation` 路由绑定、各厂商图片适配器映射、失败重试与配额限制 | 通用工具交接与 Artifact 传输契约 |
| [history-recall.md](history-recall.md) | `stravia://history/` 命名空间、交互链路可见性、只追加转录与稳定行号、全文检索与索引延迟、仅透明注入暴露边界 | 通用 `StraviaRead` 语法、观测落盘实现 |
| [credential-protection.md](credential-protection.md) | 本地可逆脱敏规则与强度分级、API Key/Principal 映射与占位符还原、发布顺序、按最近检出（`last_detected_at`）过期、命中/复用管理端脱敏 | Wire Debug 抓包过滤（仅 `Authorization` 头） |
| [interaction-observation.md](interaction-observation.md) | 交互归属与交互链路、请求终态与成功率指标、Confirmed Usage 与长期 Stats、事件流/SSE、Debug 开关与四方向 Wire 抓包、诊断包导出、请求记录交互契约 | 凭据脱敏规则、存储底层引用回收、表结构逐列抄录 |
| [cost-and-billing.md](cost-and-billing.md) | 双币种冻结金额、价格分档与峰谷定价、Provider 成本方式与按字段固定、API Key 计费与消费上限准入 | 用量采集与观测落盘 |
| [admin-auth-bootstrap.md](admin-auth-bootstrap.md) | 管理用户与推理 Principal 分离、会话撤销、Setup Token 首次初始化、启动迁移进度、显式管理入口与受信代理 | 业务数据模型与日常配置 |
| [console-chat.md](console-chat.md) | 管理面内置控制台对话的身份语义、浏览器/Desktop 本地存储隔离、流式恢复与限制 | Responses 线协议与通用工具注入规则 |
