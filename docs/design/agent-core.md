# Agent Core

Stravia 在同一套 Gateway 中运行代码内置的 Agent。AgentRunner 拥有有界模型—工具循环，Model Turn Executor 拥有单次模型调用，Turn Chain 保存可续接前缀。
产品能力提供自己的 Definition 和输入输出验证，不把业务语义写进循环。
决策见 [ADR-0007](../adr/0007-own-agent-execution-behind-native-seams.md)。

| 模块 | 契约 |
| --- | --- |
| [Agent Core](../../backend/crates/stravia-core/src/agent/) | Definition 注册、AgentRunner、Turn Chain、Artifact Store 和入口适配。 |
| [Model Turn Executor](../../backend/crates/stravia-core/src/model_turn/) | 一次 canonical 请求到终态的执行，不运行工具或负责交付。 |
| [stravia-runtime-contract](../../backend/crates/stravia-runtime-contract/) | 共享 canonical IR、Agent、Artifact、Hook 和执行接口。 |
| 产品模块 | [Web Search](web-search.md)、[多模态理解](media-understanding.md)、[媒体生成](media-generation.md)分别拥有其公开业务结果。 |

普通推理与 Agent 共用单轮推理接口，而不是让 Agent 重新进入 Gateway。模块调用顺序为：

```text
Ingress / PlatformTool / MCP
  → 产品 adapter（解析、授权和固定 Definition）
    → AgentRunner（上下文、模型—工具循环、提交）
      → Model Turn Executor（单次模型请求及 Target 尝试）
      → PlatformToolRegistry（版本化叶工具）
      → Turn Chain / Artifact Store（不可变历史与文件）
```

## Definition、Revision 与 Admin Config

Agent Definition 是代码注册的稳定身份，有唯一 slug、描述与公开结果契约。公开 Definition 可使用 `agent_<slug>`；
内部 Definition 不生成通用工具。不要把同一 slug 重新赋给不同契约。

Revision 是不可变执行规格，含版本、spec hash、指令、OutputSchema、带版本的工具 allowlist、预算、Artifact policy 和修复策略。
启动幂等注册：对象键顺序不构成变更，数组顺序有意义；同一 slug/version 若实际规格改变则启动就绪失败，不覆写历史。
能被续接的 Revision 所需工具版本必须仍可解析；注册时也检查预算、工具身份和 Artifact policy。
定义不再注册时不能创建新根，不因此删除已持久化的 Revision。

Agent Definition Config 保存启停、逻辑 Model 绑定及适用的 Thinking Level，默认停用、无绑定。
管理员通过各产品页面配置，不可创建 Definition 或修改指令、工具、schema 与预算；
不提供通用 Agent 管理 API 或页面。

## AgentRunner 与单轮执行

1. 入口确定 Definition 身份，把问题和附件追加为一条 user message；调用方不能伪造 system、assistant 或 tool 历史。
   无父 Turn 时冻结新根绑定；续接时恢复指定历史前缀。
2. 预处理显式媒体，组装 Revision 指令、完整前缀、工具 schema 及收束预留。
   若有 OutputSchema，模型收到完整约束，结果仍须由本地校验器验证。
3. 调用 Model Turn Executor；无工具且输出有效则提交 complete Turn。
   格式错误可在修复预算内重试，不能绕过 schema 结束。
4. 按版本化 allowlist 执行工具。只有声明并行安全的工具在上限内并行；其他串行。结果按原调用顺序写回。
   可恢复业务失败返回工具错误，授权、策略、取消与内部不变量失败终止 Run。
5. 工作预算耗尽后仅用预留执行一次无工具收束。成功提交 `partial` 并记录 `limit_reached`；
   收束失败不提交。续接时以受控元数据说明该状态，不修改原消息。

Revision 约束总时长、工作与收束时间、Model Turn 和工具调用次数、工具并行度、输入／输出／总 token 及修复次数。调用者不能扩容。
必须给收束预留上下文及输出额度。完整前缀连同预留无法放入窗口时，在首次上游调用前报错；
不自动截断、摘要或原地改写历史。

**禁止 Agent→Agent 嵌套。** Definition allowlist 不得包含 Agent capability 工具，Runner 不递归调用 HTTP ingress 或外层 dispatcher，不直接发送 Vendor 请求。
内部 Hook 不得以 `ExposeTool` 改写工具集合；Runner 的叶工具不代表客户端工具续接。

Run 是请求驱动的尝试，不产生独立 Run 记录。事件流只有在 poll 时推进，丢弃时取消；
预分配的 Turn ID 只有在提交成功后才可续接。
完全消费的流恰有一个 Completed、Partial 或 Failed 终态，终态后无事件。
事件不含私有推理、原始报文、凭据或完整工具正文。
取消、失败、进程退出不持久化执行中的 Runner，也不创建失败 Turn。
公开工具的后台所有权是外层[工具执行交接](#工具执行交接)，不改变 Runner 本身的契约。

### Model Turn Executor 与 Hook

Model Turn Executor 接收 canonical 请求和调用方固定的授权策略，复用 Target 选择、协议协商、上游重试、codec、流与用量。
同一 Model Turn 内的 Target retry 不开启新轮，也不执行工具。成功流在唯一 `Completed` 后停止，不等待 EOF；
产物交付和 Turn 提交是之后的独立动作。
`Completed` 前完成必要的可逆脱敏还原和映射发布，取消或时限仍可在发布前抢占；
发布边界见[凭据保护](credential-protection.md)。

同一 AgentRun 复用一个 HookSession。Hook 可修改 canonical 请求、响应与工具结果，也可拒绝或安全地提前返回，但不会取得上游凭据、原始 API Key 或 Artifact 存储键。
Request Hook 可读取完整上下文，只从宿主给出的 `input_start` 修改未处理输入。历史保存处理后的前缀及边界；
续接处理上一轮最后输出与新增输入，不把展开的媒体字节回写历史。
工具结果的文本和媒体类型由可信适配器保留，不按业务 JSON 的 `type` 猜测；文本脱敏不改写媒体块。

### 安全快照与 Principal

- 启动时冻结 Revision、根 Model、启停及执行规格。Admin 改配置不改写在途 Run；
  停用后拒绝新根和新续接，重新启用后可按仍可用的父 Turn 续接。
- 有历史时沿用根 Model 和原 Revision。创建新根才采用新的默认 Revision 或模型绑定；
  历史不会把其他 Principal、Definition 或 Turn 类型的权限带入本次执行。
- 在每一隐藏 Model Turn 前重查 API Key 状态、模型可用性、适用绑定或 capability grant 及实时额度；
  工具副作用前重查适用权限。普通 Target retry 不扩展该轮权限快照。
- 通用 Agent 受调用者的 Model binding 约束；产品隐藏模型只由代码拥有的 capability policy 间接授予，不能从输入指定，也不变成 Key 的普通模型权限。
- 隐藏执行复用外层 RootRequest 的入口准入及 RPM，实际上游发送各自经过目的地 RPM。
  子轮观测见[交互链路](interaction-observation.md#归属与交互链路)，不重复计量外层工具调用。

## Turn Chain 与 Turn Reference

Agent Turn 是一次输出通过校验的 Run 提交的不可变续接点，记录父节点、Principal、Definition Revision、根 Model、canonical 对话、引用及用量。
公共 Turn Reference 为 `stravia://turns/<id>`。续接精确恢复父链，可从任一可用节点分支，没有可变 Session head、隐式 latest、合并或请求幂等。

Turn Chain 事务内校验父节点可用及身份匹配，提交节点及所需历史引用后才交付成功终态。交付失败不回滚已提交节点。
不存在、回收、越权或类型失配统一为不可用，不提供存在性探测。Agent 保存 canonical 消息和工具状态、续接所需协议状态与媒体引用，不保存原始上游流、私有推理或媒体正文。
普通观测及诊断边界见[交互观测](interaction-observation.md)。

已提交节点可跨重启读取。保留期跟随 `log_retention_days`（默认 7 天，最小 1 天），按创建时间计算，不因读取、提交子节点或续接刷新。
有子节点或受保留依赖引用的节点不清理；到期叶节点等扫描回收，独立分支各自到期。共享存储见[数据布局](data-layout-performance.md#共享正文存储与引用回收)；
Responses 的业务语义见 [Open Responses](open-responses.md)。

通用 `agent_<slug>` adapter 可接收以下输入（引用为占位符）：

```json
{
  "prompt": "继续核对结论中的时间线",
  "artifacts": [{"path": "stravia://artifacts/<artifact-id>"}],
  "previous_path": "stravia://turns/<parent-id>"
}
```

`previous_path` 可省略，只属于通用 Agent 接口，不是 StraviaRead 字段。成功返回 `path`、`completion` 与 `output`；
输出为 schema 对应值或文本，内部用量、事件不进入模型上下文。
PlatformTool 的调用和结果留在父模型的隐藏上下文；MCP 直接接收工具结果。
Responses adapter 可以通过 `stravia:agent_result` 输出 Turn path，其他聊天协议不增加此元数据。

## 平台能力门禁与透明注入

Platform Capability Gate 决定平台是否提供高级能力；有效 Key 在 Gate 开启且能力已配置时可走支持的显式入口。
透明注入只决定未显式声明时本次自动暴露哪些能力，不授予显式调用权限；关闭 Gate 不删除已保存的注入项。
参见 [ADR-0016](../adr/0016-gate-advanced-capabilities-and-separate-transparent-injection.md) 与 [ADR-0097](../adr/0097-store-api-key-transparent-injections-as-a-set.md)。

| 能力 | 平台范围 | Key 注入项 |
| --- | --- | --- |
| Web Search / Web Fetch | 共用 Web Search 开关 | `web_search` |
| Media Understanding | 图片/文档和音频可分别启停 | `media_understanding` |
| Media Generation | 独立开关 | `media_generation` |
| Reversible Redaction | 独立保护开关，不是可发现的工具 | 无；见[凭据保护](credential-protection.md) |
| History Recall | 没有独立平台 Gate | `history_recall`，仅透明注入 |

API Key 的 `transparent_injection_enabled` 是总开关；`transparent_injections` 是已选集合，不含总开关或 MCP 权限。
创建省略该集合表示空；部分更新省略表示保留，提供时整体替换。未知或重复项拒绝。
关闭透明注入或平台 Gate 保留已选项；满足条件后重新生效。

| 调用面 | 权限与暴露 |
| --- | --- |
| 客户端显式声明 `StraviaRead`、`generate` 或支持的 hosted/native 声明 | 有效 Key 加适用 Gate；不要求对应透明注入项，不追加 History Recall。 |
| 模型请求透明注入 | Key 总开关、已选项和适用 Gate 决定本次暴露范围；基础 Read 不要求选中高级工具。 |
| `POST /mcp` | 另要求 `mcp_access_enabled`，高级工具仍检查平台 Gate；不读取透明注入选择，永不提供 History Recall。 |
| 内部 Agent | 由可信 registry 和不可变 Definition 装配，不继承父模型的透明注入或 MCP 工具集合。 |

工具描述、发现与执行都限定在本次范围；知道路径不等于可执行。基础 Read 允许所属 Artifact 下载和自己执行结果查询，不因此开启高级能力。
[History Recall](history-recall.md#暴露与凭据)只对选中该项的透明注入范围可用，不出现在显式 Read、MCP 或内部 Agent。

## StraviaRead 统一读取契约

公开工具的 wire name 为 `StraviaRead`、source Tool ID 为 `stravia-read`。顶层只接受必填字符串 `path` 和可省略或为 `null` 的对象 `body`。
参见 [ADR-0091](../adr/0091-read-paths-with-colon-selectors-and-plain-fields.md)及[路径解析器](../../backend/crates/stravia-web-access-contract/src/read_path.rs)。

| `body` 字段 | 用途 |
| --- | --- |
| `question` | 搜索文本、资源问题、续接追问或历史检索，取决于目标。 |
| `attachments` | 媒体续接新增的精确 Artifact 引用或公网 URL，不带读取选择器；重复项拒绝。 |
| `allowed_domains` | 搜索来源限制，语义见 [Web Search](web-search.md#搜索输入与报告)。 |
| `wait` | 工具执行查询最多等待的秒数，非负整数。 |

不认识的字段、错误类型及该路径不适用的非零值均拒绝；缺省、`null`、空白问题、空数组和零等待按未传处理。
模型声明使用 `strict: false`，允许声明 `path` 与可空 `body` 均必填；MCP schema 只要求 `path`。
描述按范围列出可用字段；宽松 schema 不替代运行时校验。

`stravia://search`、`stravia://turns/<id>`、`stravia://artifacts/<id>`、HTTP(S)、`stravia://tool-executions/<id>` 与获授权的 `stravia://history/` 按已知目标路由。
续接直接用 Turn Reference，先查 Turn kind 再分派；Responses Turn 不是 Read 的续接目标。
不解析旧 query／fragment 平台选项或裸 ID。

外层 Hook/Platform 和 MCP 装配公开 router；内层 Agent 单独装配同名基础读取 router。
内层 `stravia://search` 只取基础检索列表，不启动研究 Agent；它拒绝研究续接，不暴露历史范围和公开执行等待。
网页 URL 共用 Web Access。模型不能选择 registry 或执行层级；
插件贡献的可判定 Read domain 必须无冲突。

### Path 选择器

从 `path` 末尾按冒号剥离合法选择器，authority 内端口不是选择器。裸 `host:port` 要先加 `/`；
源 URL 字面以类似选择器结尾时，将那个冒号写为 `%3A`。源 URL 自身的 query 和 fragment 保留，`body.question` 不发给源站。

| 选择器 | 规则 |
| --- | --- |
| `:raw` | 读取支持的严格文本表示，可配行范围，不能与 `question` 同用。 |
| `:download` | 只下载，不执行模型；不能与其他选择器或问题同用。 |
| `:cursor=<base64url>` | 平台生成的文本快照或历史回看分页；不能与其他选择器或问题同用。 |
| `:N`、`:N-M`、`:N+K`、`:-K` | 从 N 到末尾、闭区间、从 N 取 K 行、最后 K 行。1-based；逗号合并最多 16 段，每种选择器只提供一次。 |

例如 `https://example.com:8443/doc:1-20` 读取前 20 行；若资源路径实际为 `/doc:20`，应写 `/doc%3A20`。
取得结果后直接使用平台给出的 `next_path`，不手写游标。

各内容类型仍可拒绝不适用的选择器。行范围排序并合并重叠或相邻段；LF 计行，保留 CRLF；
末尾 LF 不产生空行。起点越 EOF 报错，终点可截到 EOF；空文本裸读成功、显式行范围失败。

### Read Snapshot 与分页

首次文本读取建立所属 Principal 的不可变 Read Snapshot。复用快照不重新抓取、转换或调用模型；
历史转录因只追加，按[历史回看](history-recall.md#行号稳定性)的实时游标读取。

- 每页最多 32 KiB UTF-8 和 200 个源行。长单行按 UTF-8 边界续读，不漏字节。
  字节范围为 0-based 半开区间，行号从 1 开始。
- 输出含 `content`、`read_path`、`returned_ranges`、`representation`、`has_more`、`source_truncated`、`limitations`，可附 `source_url`、`question_applied`、`next_path`。
  正文不插行号、补上下文或省略符。
- 直接复用 `next_path`，末页不返回它。`source_truncated` 独立说明源数据不完整，不表示还有续页。
- 快照用 `application/vnd.stravia.read-snapshot` 封装。正文为 UTF-8，前置四字节大端 header 长度和最多 16 KiB 的版本化 JSON；下载只导出正文。
- 游标是无填充 base64url JSON（最多 8 KiB），定位 Artifact、范围与偏移，不签名、不授权。
  每次读取复验归属、期限、快照格式和边界。`source_url` 只是注记，不能由上传者自行签发证据。

搜索和媒体先验证并保存完整报告；长 `report.answer` 只在交付副本分页，来源、限制、Turn 引用和完成状态不变。
首包顶层 `pagination` 提供分页元数据，续页读答案正文；短报告不建立分页快照。

## Artifact 与单请求上传

Artifact 是归属 Principal 的不可变内容；引用 `stravia://artifacts/<id>` 不授权，不走 DNS 或本地路径解析。
同一 Principal 可跨对话复用。
最终 ID 用版本域、带长度的 Principal 与声明 MIME 及完整内容计算 SHA-256，编码为 55 位小写 base26；
不接受客户端摘要或旧随机 ID。暂存身份和物理存储键不公开，完整收存才发布稳定引用；同一身份并发发布串行。

### 上传

```http
POST /v1/artifacts
Content-Type: <MIME>

<原始文件字节>
```

- 使用 inference API Key 或临时 Upload Grant；匿名请求在读取文件体前拒绝。
- 请求体就是文件，缺少 Content-Type、`multipart/*` 或 `application/x-www-form-urlencoded` 返回 415。
  不提供断点续传或独立分片 API。
- 流式 ingest，不整体缓冲。`Content-Encoding` 支持 gzip、deflate、br、zstd，按解码字节校验身份与上限；损坏报 400，其他编码报 415。
- 单对象上限 100 MiB；每 Principal 暂存至多 16 项、合计 400 MiB。
  适用时叠加 Definition 的数量、大小、声明 MIME 策略。不按 MIME 嗅探替换声明。
- 成功为 HTTP 201 与 `{path, mime_type, size}`。暂存空闲 TTL 为一小时；
  成功写入刷新暂存，不代表已有可读对象。
- 无效、未找到、禁止、未认证、存储故障分别映射至 400、404、403、401、500。
  附件归一化使用 `attachment_ingest_failed`，不把平台存储错误归给用户或 Provider，也不回显底层详情。

上传协议决策见 [ADR-0092](../adr/0092-replace-multipart-artifact-upload-with-single-request.md)，实现见 [HTTP Artifact 接口](../../backend/crates/stravia-core/src/proxy/artifacts.rs)。

### Upload Grant 与 Download Grant

- **Upload Grant** 仅能为指定 Principal 上传，不能读取、推理或管理。固定 15 分钟，可在期间并行上传多个文件，不因使用续期。
  上传授权注入默认关闭；关闭后不撤销已签发授权。
- **Download Grant** 在归属鉴权后签发，单文件、固定期限、持有即授权；平台签名链接默认 15 分钟。
  文件引用本身不是 grant；通过已有链接下载不会续期文件。

上传授权使用占位符：仅交付普通回答和客户端工具参数时可替换为真实凭据，思考与 Platform Tool 参数不替换。
同一响应复用未到期授权，仍需验证 API Key；真实授权回传时，发给模型前恢复占位符，不依赖注入或脱敏开关。
授权不进入模型上下文、平台业务历史、普通错误或进程日志；诊断面边界见[授权分离决策](../adr/0048-separate-artifact-references-from-transfer-grants.md)及[交互观测](interaction-observation.md#wire-debug-与脱敏边界)。

默认内部存储，可选 S3；「外部签名下载」默认关闭，配置 S3 不自动开启。

| 下载模式 | Provider 文件交付 | StraviaRead 客户端下载 |
| --- | --- | --- |
| 关闭外部签名 | 内联 base64 | 已配置客户端入口的平台签名 URL |
| 开启，内部存储 | 文件公开入口的平台签名 URL | 同入口的平台签名 URL |
| 开启，S3 | 原生 S3 预签名 URL | 原生 S3 预签名 URL |

S3 直连不经平台中转，不要求存储桶公开。实际下载方须能访问该地址。
客户端入口包含协议、端口及部署前缀，保存后不从请求 Host 或转发头重算。
仅开启外部签名时才需配置文件公开入口；平台预填不证明下载方可达。
参见 [ADR-0050](../adr/0050-configure-s3-direct-download.md)。

Provider 发送或重试时签名地址必须还至少有五分钟有效期，必要时重新签发；S3 同时受签名凭据到期限制。
不支持该协议表示时以内联交付或明确拒绝，不发不可用的地址。

### 显式媒体快照与保留

按 [ADR-0049](../adr/0049-snapshot-explicit-media-inputs-as-artifacts.md)，只收存结构化媒体、显式上传或导入；
普通消息里出现的 URL 不触发抓取。
公网媒体需要通过 [Web Access 公网规则](web-access-providers.md#公网读取安全)，收存失败终止请求，不透传短命 URL 或丢弃附件继续。
Artifact URL 与 FileId 来源使用同一归属与保留检查；供应商自己的 file ID 保留原义。

发送前以目标协议允许的签名 URL 或 inline bytes 交付；[codec attachment 模块](../../backend/crates/stravia-protocol-codec/src/attachment.rs)是表示能力的事实源。
不能表达的模态明确拒绝，不在 codec 抽帧、转录或 OCR；历史只留稳定引用，不存展开的媒体。
媒体预处理见[多模态理解](media-understanding.md)。

保留期跟随 `log_retention_days`；经归属鉴权的实际使用或重新完整收存可延长期限。
已到期拒绝新使用，不因签名访问、Reader 或 Turn 引用复活。无公开删除操作。
清理保护正在进行的读取和未到期授权；对象删除失败保留元数据，完整校验重新收存才能修复已过期对象。
持锁的发布、续期和清理操作在调用者取消后仍负责收口，不继续接收已取消的网络请求体。
SQLite/PostgreSQL 须提供同样的可见性语义。

## 工具执行交接

公开工具的 MCP、显式调用与透明注入共享有界等待。工具可声明同步等待窗口，缺省 60 秒，包含排队及输入准备；
它不改变工具原 deadline。窗口内完成仍返回原结果或原错误。内部 Agent 的基础读取不走公开后台交接。
见 [ADR-0082](../adr/0082-expose-queryable-tool-executions-after-bounded-wait.md)。

### 交接与取消

- 窗口结束后，只有持久化交接成功才返回 `{path: "stravia://tool-executions/<id>", status: "pending"}` 或 `running`；
  引用标识一次平台执行，不是模型 call ID、History Marker 或 Artifact。
- 交接前取消或断连会取消本地执行。交接在不可逆提交前复查取消；提交与断连并发时以事务实际结果为准。
  提交报错但可读回确认的交接继续，否则报告存储故障，不虚构引用或自动重放。
- 交接后执行独立继续，即使引用未送达客户端。保留原 Principal、RootRequest、路由、安全与 RPM 归属、原执行 deadline；
  Gateway 关闭仍可中断。不保证已发往供应商的操作停止或不计费。

隐藏调用的 History Marker 先持久化，经发送确认后发布，才启动领域工作。Marker 发布不是交接；
此后交接前若原请求取消，仍保存明确的取消输出。终态不被晚到的取消改写。

领域终态与 Marker 当次交付各自保存。常规工具结果由本次请求的 ToolResult Hook 处理后封存；
交付的是执行引用则以后仍恢复该引用，不替换为后台结果。
原请求消失而 Hook 无法完成时，除系统已确定的取消输出外，Marker 保存交付处理错误，不放行未处理结果。
后台不重用旧 Hook，也不因 Marker 失败而撤销已交接执行；查询输出由查询所属请求处理。

### 查询与保留

```json
{"path": "stravia://tool-executions/<id>", "body": {"wait": 120}}
```

- `wait` 省略或为 0 时立即返回；正整数最多等待所给秒数，受查询和原执行剩余期限共同限制，可超过初次同步窗口。
- 终态提前唤醒等待者；取消查询只停止本次等待。查询不重放、不另建后台查询任务，也不延长执行或保留期。
- 未完成返回实际 `pending` / `running` 和稳定 `path`；完成返回 `completed` 及原工具 `result`。
  失败或进程中断返回 `failed` / `interrupted` 加 `error`，并非本次读取失败。
- 未知、过期、已清理或越权的引用是查询错误，不能用错误区分未知与其他 Principal。

未完成执行在重启后标为中断，不接管或重放；旧 owner 或迟到结果不能覆盖终态。
终态从完成、失败或判定中断时起按历史保留天数独立计时，不与来源诊断寿命绑定。
来源诊断清理、缺失或写入失败不取消已交接工作，也不使有效查询失效。
清理执行记录不删除独立 Artifact，文件也可能先到期。
Debug 保持来源归属和固定导出截止水位，参见[诊断包导出](interaction-observation.md#诊断包导出)。

数据库参考：[SQLite](../database/sqlite.sql)、[PostgreSQL](../database/postgres.sql)。
