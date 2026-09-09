<h1 align="center">Stravia</h1>

<p align="center">
  本地运行、可自托管的 AI 接入与执行平台，统一模型协议，执行平台工具与内置 Agent，并集中管理访问、历史和用量。
</p>

<p align="center">
  <a href="README.md">English</a>
</p>

> **项目状态：** Stravia 当前版本为 `0.1.0`，仍在积极开发中。稳定版本发布前，配置格式和数据库兼容性可能发生变化。

## 项目简介

Stravia 连接 AI 客户端、模型提供商与平台自有能力。客户端继续使用自身支持的协议；Stravia 负责解析虚拟模型、选择上游后端，并在必要时转换请求与响应。

除了路由，Stravia 还会执行平台自有工具，将结果送回模型并继续后续轮次。其有界 Agent Runner 驱动本地 Agent 联网搜索，也被多模态理解复用。这些能力通过兼容的模型请求与 MCP 提供，共享身份、访问控制、历史、用量统计和诊断。

Agent 行为由平台实现定义并进行版本管理。管理员配置受支持的能力设置和模型绑定；Stravia 不是用户自定义 Agent 或可视化工作流构建器。

```text
Claude Code · Codex CLI · Gemini CLI · OpenCode · 各类 SDK
                            │
                            ▼
              Stravia 统一监听端口 :23471
                ├─ OpenAI 兼容 API
                ├─ Anthropic Messages API
                ├─ Gemini GenerateContent API
                ├─ MCP 工具
                ├─ 平台工具与内置 Agent 执行
                ├─ Admin API
                └─ WebUI
                            │
                            ▼
 OpenAI · Anthropic · Google · Vertex AI · DeepSeek · Ollama · …
```

同一套 Rust 核心支持两种部署形态：

- **桌面应用：** 基于 Tauri，在本地运行平台并提供集成管理界面。
- **独立服务端：** 单个二进制运行相同的平台能力，通过统一端口提供代理 API、MCP、Admin API、健康探针和内嵌 WebUI。

## 当前能力

### 平台工具与内置 Agent 执行

- **平台自有工具执行：** 向兼容的模型请求暴露工具，在 Stravia 内执行平台工具调用，并携带结果继续模型轮次。客户端自有工具仍由客户端负责执行。
- **有界 Agent 循环：** 在时间、轮次、token 和工具预算内协调模型与工具轮次，支持受控工具并发、取消和输出校验。
- **内置能力：** `web_search` 返回带来源的 Search Report；`understand_media` 为受支持的图片返回强校验 Media Report。两者均可通过 `previous_turn_id` 显式续接与分支。
- **MCP 与透明注入：** 将已启用能力提供给 MCP 客户端，或按配置将所选能力注入兼容的模型请求。
- **执行管理：** 将请求及嵌套执行归属于调用方 Principal，实施访问与并发限制，并记录历史、上游确认用量和诊断。

Local Web Search 使用模型与工具循环；当前多模态理解复用 Agent Runner，但自身不调用工具。下文各能力章节说明具体支持的输入、配置和限制。

### 协议网关

| 客户端协议                | 端点                                                   |
| ------------------------- | ------------------------------------------------------ |
| OpenAI Chat Completions   | `POST /v1/chat/completions`                            |
| Open Responses 2026-04-24 | `POST /v1/responses`（JSON、SSE、WebSocket）           |
| OpenAI Embeddings         | `POST /v1/embeddings`                                  |
| Anthropic Messages        | `POST /v1/messages`                                    |
| Gemini GenerateContent    | `POST /v1beta/models/{model}:generateContent`          |
| Gemini 流式生成           | `POST /v1beta/models/{model}:streamGenerateContent`    |

Stravia 支持 JSON、SSE 与 Open Responses WebSocket 交付、跨协议工具调用、推理内容、用量数据，以及上游无需修改时的同协议透传。

Open Responses 推理正文使用当前客户端采用的 rolling `response.reasoning_text.delta` / `response.reasoning_text.done` 事件名流式交付；reasoning item 与 dated `2026-04-24` 语义保持不变。

隐藏的 Platform Tool 续跑通过 HTML comment 形式的 History Marker 投影到客户端历史。OpenAI-compatible Chat Completions 在首个非空 `content` delta 前继续通过 `reasoning_content` 交付 Thinking；未请求 encrypted reasoning 时，Open Responses 的公开 summary delta 会保持实时交付，而在 item 开始时已明确标记的 protected reasoning 也会流式交付公开 summary，并把 opaque 字节保留在 Marker 后。之后的 Thinking 通过 `content` 以 Markdown 引用 Preview 流式交付，后续 Thinking Marker 与 Platform Marker 也使用 `content`，从而在客户端按字段聚合时保持顺序。纯文本客户端可能直接显示这些 Marker comment。Open Responses、Anthropic Messages 与 Gemini 保留原生有序 reasoning/thinking carrier；若所选协议无法表示已观察到的顺序，Stravia 会显式失败，而不会延迟普通 Text。

重新提交完整历史的客户端必须原样保留 History Marker 与 Projection Delimiter。Stravia 会删除仅用于展示的 Preview 字节，并在原位置恢复权威 Thinking、ToolCall 与 ToolResult；删除 Marker 或 Delimiter 会被视为有意编辑历史。客户端关闭流式传输时，Stravia 会先执行仅含 Platform Tool 的隐藏续轮，再一次性返回语义等价的 buffered projection。live stream 则在启动对应 Platform Tool 前交付并发布每个 Marker。

OpenAI direct 与 Codex OAuth 的生成 Target 会为 Chat Completions、Open Responses、Anthropic Messages 和 Gemini 请求使用上游 Responses WebSocket，不受客户端是否流式影响；Embeddings 仍只使用 HTTP。Hook 与协议可表示性检查完成后，Stravia 可从最长且严格等价的 canonical item 前缀续接；Principal、精确 Target、Provider 账号与配置、resolved model、instructions、tools、reasoning、response format 和请求控制必须全部一致。任一条件不匹配都会发送完整有效历史，不会削弱请求语义。

`POST /v1/responses` 以 Open Responses 2026-04-24 作为 canonical baseline，同时接受结构安全的 rolling additive 字段和 hosted tool 声明。同协议 Target 保留这层 compatibility envelope；跨协议 Target 可以省略 advisory 字段和未被强制选择的 hosted tools，但绝不省略内容或硬约束。后台执行仍不支持。

客户端发起的远程压缩请求转发给正常路由选定的 Target。`POST /v1/responses/compact` 是独立的 HTTP unary 操作，返回包含 retained items 与 opaque state 的完整下一窗口；后续必须完整回放该窗口，不能自行裁剪或改写。Responses 同时承载原生 compaction item、内嵌触发项，以及客户端提交的 `context_management` 控制。这些属于协议硬要求：Target 协议无法承载时返回不支持，不能静默丢弃；compact 操作不是空 Generation。

Stravia 不提供平台级压缩设置，不注入默认压缩控制，也不生成本地摘要。Target 压缩能力未知不阻止转发：由上游决定是否接受请求及客户端提交的阈值。成功或错误均返回客户端，不为完成压缩而重试或切换 Target。显式空集合与 null 仍由客户端控制。

已登记的原生状态在保留期内可跨重启恢复已知来源，但不会恢复已移除的历史；Target、账号与配置 generation、模型及协议必须保持兼容。监控区分已确认生成关系、原生桥接与保留尾部推断关联；推断关联不改变推理，也不启用 Target Continuation。清理监控历史不删除有效原生状态映射；普通监控不包含 opaque payload，未报告的压缩用量保持 unknown。

### 提供商与模型路由

当前内置的提供商元数据包括：

- OpenAI 与 Codex OAuth 通道
- Anthropic 与 Claude Code OAuth 通道
- Google Gemini 与 Vertex AI
- DeepSeek、Moonshot AI、Zhipu AI、Z.AI、MiniMax、xAI（API Key 与 Grok OAuth）和 NVIDIA
- OpenRouter、Ollama 以及自定义 OpenAI 兼容端点

客户端发送一个 **Model ID**。该值就是 Route ID，匹配时包含字母大小写在内完全精确。逻辑 Model 还可以设置可选、可重复的展示名称；展示为空时回退到 Model ID，并且永不参与路由、授权或绑定。对应 Route 可以同时保留已启用和已禁用 Target；已禁用 Target 保留配置但不会接收流量。Stravia 先选择可用的最高 Target Priority 组，再在组内使用 Traffic Equalization 或 Latency Preference；适用时，Conversation Affinity 与 Cache Affinity 可继续偏好此前成功的已启用 Target。Stravia 从 revisioned `models.stravia.cn` 索引刷新 Provider Catalog：轻量 Provider 与 Canonical Model 索引以同一 revision 原子更新，Provider-scoped inventory 仅在需要时加载。Catalog Provider 使用其 scoped inventory；账号级 discovery 仍决定可调用的模型 ID，Core 只为精确匹配补充元数据，不会加入仅存在于 Catalog 的模型。

添加提供商时，先选择完整的提供商/通道选项。API Key 与 OAuth 通道是独立选项，创建后不能互相转换。Codex 与 Claude Code OAuth 在桌面端和通过回环地址访问的 WebUI 中会自动接收回调；远程 WebUI 则会在浏览器登录后要求粘贴完整 callback URL。等待授权期间，三种环境都支持手动粘贴完整 callback URL，即使自动监听正常运行也可使用。Grok OAuth 使用 xAI device authorization flow：WebUI 打开验证页面，在需要时显示 user code，并轮询直到授权完成，无需填写 callback URL。

接入无需认证的服务时，创建仅使用 API Key 的连接（包括自定义 OpenAI 兼容端点）可以将密钥留空。模型发现和推理会省略默认认证，不发送空的 Bearer token；填写密钥后仍正常发送。OAuth、Setup Token、Vertex 及结构化 Adapter Credentials 的凭据要求保持不变。客户端访问 Stravia 仍须使用有效的 Stravia API Key。

Codex Provider Model 同步使用当前上游客户端契约，因此同步后可以发现新加入版本门禁的模型。生成请求会携带 Codex 后端要求的模型与可选 service tier 路由提示。

模型探测遵循 Provider 已保存的代理选择及既有全局出站代理设置；关闭该 Provider 的代理选择时，探测保持直连。已启用的代理配置无效时明确报错，不会悄悄绕过代理。模型列表端点采用 Vendor 的 Models 认证约定，可以与推理不同；自定义端点继承该约定，无需新增设置，也不会自动试探认证方式。Provider Model 同步失败时保留已保存的模型清单。

WebUI 为每种资源保留唯一编辑表面。添加或编辑逻辑 Model 时，Model ID 组合框可以按名称或 ID 搜索 Canonical Model，并在目录不可用时继续接受自定义 ID；选择模板会复制其展示名称，两个字段都可继续编辑。手动 Provider Model 仍可搜索 Canonical Model 模板；选择不会创建 Backend，也不会保存隐藏 binding。新 Provider 保存后会进入详情页并开始同步 Provider Model；详情视图分别管理连接设置、持久化 Provider Model 清单和 Route 引用。Provider Model metadata 在独立抽屉中保存，Selection Policy 则立即生效，并且只控制新 Target 候选的 Effective Availability。Provider Model 变为不可用不会改写已有 Route Target。管理员可以从精确 Provider Catalog Entry 显式 re-import 已发现的 Provider Model；普通同步不会覆盖本地 metadata。

可用模型清单提供独立的**模型规格**列，与 Target 编辑区和模型详情共用展示规则。规格来自已保存、可人工编辑的 Provider Model 快照，不代表实测能力，不使用运行时默认值，也不混入平台补充能力。限额采用无损十进制 K/M 缩写（1K = 1,000 tokens），悬停或键盘聚焦可查看完整 token 数。输入与输出模态分别展示，功能声明保留支持、不支持、未登记三态。规格列支持按上下文与最大输出下限、输入输出模态及全部五项功能筛选；所有选中条件必须同时满足，未知值不能满足对应条件。规格筛选可与搜索、可用状态、添加方式和使用情况组合，也可一并清除。

桌面端规格筛选复用表格的标准列筛选菜单：点击**应用**使草稿生效，点击**清除**移除该列筛选，未应用便关闭菜单则放弃本次修改。移动端的规格条件统一放在原有的**筛选模型**抽屉中。

WebUI 统一使用共享控件呈现请求恢复、敏感输入显隐、加载与进度。后台刷新失败时，已加载的数据仍可使用。Model ID 建议保留自由输入与输入法组合行为，高级设置折叠时保留草稿。导航保留已保存的折叠偏好；移动端诊断检查器约束焦点，不改变桌面覆盖式面板。关闭更新通知不会跳过该版本。

高级功能的启停开关即时保存，不再需要额外点击保存。多模态理解与联网搜索须先保存完整配置才能启用；模型绑定、搜索方式及相关多字段草稿仍通过**保存设置**提交。切换启停只改变已保存配置的启用状态，不顺带提交或清空配置草稿。即时保存失败时保留服务器确认的状态，并就地显示错误。联网搜索仅保留一个能力总开关；内部搜索与网页读取来源的选择、排序即时保存，不再要求额外开启。设置未加载时不会显示可编辑的默认值。同级页签在窄屏自然换行，模型服务详情导航仍保留真实链接与浏览器历史。

概览根据已加载的配置推荐一个下一步操作，不以请求记录判断接入是否完成。连接模型服务后，可搜索清单并沿用上游 ID 添加所需模型；Model ID 精确相同时，会把该服务加入已有模型而非重复创建。添加动作始终可见，成功后可选择接入客户端，也可留在清单继续添加。已配置与已启用数量只描述保存的设置，不代表上游连接已经验证成功。

接入客户端页面会根据所选 API 密钥有权使用的 Route 生成 Stravia provider 增量补丁。默认复用已启用、未过期且有权访问已启用模型的密钥：只有一个候选时自动选中，多个候选时由用户选择。缺少资源时可进入其既有编辑器，通过「继续接入」保留本次页面任务中仍有效的选择，不存储 secret 或流程进度。Stravia Desktop 以写入 Connect Client 全局配置为主操作，并保留复制；独立 server 只提供复制。成功反馈仅确认复制或写入，不声称客户端已连接，接入流程也不会自动发送验证请求。Apply 不选择当前/默认模型，也不写入融合 provider 与 model 的键。Claude Code 是唯一例外：必须选择并合并默认、Haiku、Sonnet 和 Opus 四套模型映射，但不会改动 `effortLevel` 或 `autoCompactWindow`。

独立 server 的配置预览使用可移植的客户端路径，不依赖服务器的 `HOME`、`USERPROFILE` 或客户端目录环境变量。只有 Desktop 在读取和写入客户端配置时才解析本机目录。

Route Builder 使用独立页面。选择 Provider 后会自动加载其可用 Provider Model；如需绑定清单外的 upstream model ID，必须显式进入未经验证的自定义分支。已启用 Target 按优先级从上到下分层，已禁用的备用 Target 保留在右侧坞中；详情弹窗用秒编辑 First Token Timeout 与 Target Cooldown，并编辑 Target Retry Budget 和 Thinking Level Map，不暴露 Priority 整数。Route 可选择同层 Target 使用 Traffic Equalization 或 Latency Preference。删除 Provider 时会在同一事务内移除其 Target、删除由此变空的 Route，并保留仍有其他 Target 的 Route。

### 联网搜索与 MCP

可选的联网搜索只公开一个 `web_search` 能力，返回终态、带来源的 Search Report，而不是单页搜索结果。成功结果包含答案、已引用的公网 HTTP(S) 来源、限制、完成状态、用量和稳定 `turn_id`。将该 ID 作为 `previous_turn_id` 传入，可从同一 Principal 的完整祖先链续接或创建独立分支；Stravia 不会隐式选择“最新”Turn。

在 WebUI 中配置一个 Search Backend。Local Search 使用有界 Agent 编排有序的内部 Web Access Search/Fetch 来源：自动创建的进程内 Local Provider、Exa 或智谱。每个 Web Provider 都可独立选择是否使用 Gateway 代理。Codex Agentic Search 固定到一个精确且兼容的 Codex OAuth Responses Provider/model，不使用 Local budget。Local 与 Codex 之间不做 fallback。

进程内 Local Provider 的 Search 与 Fetch 使用 `wreq` 和 `wreq-util` 提供 Chrome 风格的 HTTP 传输。动态页面由真实 Chrome/Chromium 渲染，按需以 headless 模式启动，Rust 直接通过 CDP 控制；不再需要 Moli 运行时或 Node/Bun sidecar。桌面端与服务端都必须能解析到 Chrome/Chromium 可执行文件，才能开启 Local Search 或 Fetch，包括普通 HTTP 访问。请在运行 Stravia 的机器上安装浏览器后重新打开 Local 服务编辑窗口以刷新检测，或通过 `STRAVIA_CHROME_PATH` 指定可执行文件后重启 Stravia。没有浏览器时，界面禁止开启 Local，管理 API 返回 `WEB_ACCESS_BROWSER_REQUIRED` 且不保存此次更改。已保存的 Local 选择仍可关闭，但运行时不可用；远程 Exa 与智谱服务不受影响。Stravia 不会自动下载浏览器。

在 **联网搜索 → 搜索与网页来源 → Local → 编辑** 中，**浏览器可执行文件** 输入框会填入当前配置或检测到的路径。桌面端可点击 **浏览** 打开操作系统文件选择窗口；服务端可手填或修改服务器上的路径。选择文件只修改草稿，点击 **保存服务** 才生效；清空输入框并保存则移除手动覆盖。请填写可执行文件的绝对路径，macOS 使用 `.app` 包内的可执行文件。设置仅保存在当前实例本机，不进入共享数据库，重启后仍然有效。优先级为手动路径、`STRAVIA_CHROME_PATH`、自动检测；未修改自动带入的路径时，保存服务不会将其固定为手动路径。显式路径无效时不会回退到其他浏览器。保存后无需重启，对后续网页访问请求生效，正在进行的请求保留原选择。检查路径不会启动浏览器，桌面端也不捆绑浏览器。

渲染器移植了 [OMP 的浏览器补丁](https://github.com/can1357/oh-my-pi/tree/daf07999c2fee9b22edc7bf8fea1fb6272e0df5e/packages/coding-agent/src/tools/puppeteer)，包括全部 14 个隐身脚本、UA metadata、不启用 `Runtime.enable` 的隔离世界求值，以及不注入 source URL 的求值路径。这些措施用于减少指纹暴露，不保证绕过反爬检测。HTTP 与浏览器路径保留所选 Gateway 代理快照、独立的 Cookie/profile 归属和 Fetch 安全限制。浏览器流量经过校验出口代理，不进行 TLS 中间人解密；证书校验与 Chrome 沙箱保持启用。

平台联网搜索总开关统一控制所有有效 API Key 的显式访问。每个 Key 分别控制 MCP 访问和透明注入；透明注入只把所选且已启用的能力加入兼容请求，不限制显式调用或 MCP。MCP 客户端连接 `POST /mcp`，通过 `Authorization: Bearer <key>` 认证，并且只在 MCP 权限与平台能力都开启时发现 `web_search`。OpenAI Responses 的原生 web-search 声明与隐藏 tool continuation 使用同一个 Search contract。

联网搜索与 Web Access 配置属于部署本地状态，不参与配置导出/导入。Search Turn 只保留 Report 元数据与引用 URL，不保存抓取的网页正文或内部 Agent transcript。

Local Fetch 和浏览器出站检查会拒绝去除主机尾随点后成为非公网 IP 的 URL，例如 `http://127.0.0.1../`，与 Web Access 准入保持一致。无需配置迁移，代理选择和 DNS 职责分工不变。

### Media Understanding

Media Understanding 公开一个用于静态 JPEG、PNG 与 WebP 图片的 `understand_media` 能力。若父 Route 存在支持图片的 Target，Stravia 会原样发送图片；否则，支持工具的父 Model 可调用已配置的隐藏视觉 Model，并获得包含 source ArtifactId 与可分支 `turn_id` 的强校验 Media Report。原生视觉 Route 失败后不会 fallback 到隐藏 Model。

在**多模态理解**页面启用平台能力、选择逻辑 Model 并设置思考等级。选择器只列出所有 Target 都明确声明图片输入能力的已启用 Model；思考等级选择器只列出每个 Target 都支持的等级。启用后，所有有效 API Key 都能显式调用 `understand_media`；MCP 访问和透明注入仍由每个 Key 独立控制。隐藏调用计入调用方配额，但不会授予所选 Model 的直接访问权。外部图片 URL 仅允许公网 HTTPS 目标，并会在使用前创建 snapshot。预处理始终生成有界的有损 JPEG derivative，忽略 ICC profile，因此精确颜色或细小文本 OCR 可能不准确。

### 凭据保护

在**高级功能 → 凭据保护**中切换开关，即时保存实例级凭据文本保护设置。功能**默认关闭**，启用后统一应用于所有有效的 Stravia API Key，不设单 Key 豁免，不是 MCP 工具，也不提供透明注入选项。独立服务器与 Desktop 共用核心设置和行为；客户端沿用原有协议及明文视图。

页面默认打开**现有规则**，以可搜索、排序、分页的表格展示当前版本的完整只读目录。列表展示规则名、关键词，以及存在的路径、组合或仅作组合条件标记，不再重复名称与 ID；规则 ID 不显示，但仍可用于搜索。详情侧栏分别展示关键词、匹配表达式、排除条件、路径限制和必需或可选的组合匹配，凭据提取、优先级、置信度与规则用途收在**规则参数**中。**命中记录**按客户端交互汇总新建的保护映射，直接关联对应请求记录。有效映射复用或占位符还原不算新发现，过期后重建重新计入；不同 API Key 分别判断，并发创建只计一次。请求失败或取消后，已经发生的发现仍可保留。摘要仅展示规则、来源类型、数量和请求状态，不展示秘密、占位符或消息片段。观察可能缺失，沿用请求记录保留期，不构成安全审计保证。

**匹配测试**页签在宽屏并排展示输入与结果，手机上改为上下布局。测试器接受单个密钥或带上下文的多行文本，选择命中结果可选中对应原文；仅将主动提交的文本发送给当前 Stravia 实例。即使保护关闭，也使用同一个本地检测器并返回规则与原输入位置。测试不保存输入、不写入诊断、不联系模型供应商或验证服务、不检索已保存凭据、不建立映射或观察记录，也不修改设置。**未匹配到现有规则**不代表内容安全或凭据无效。

每次请求模型前，Stravia 在本地将系统指令、用户与历史消息、工具参数、工具结果及平台内部请求中检测命中的凭据替换为不透明占位符。内置 Betterleaks 规则随 Stravia 版本更新，运行时不下载规则，也不联网验证凭据。同一 API Key 下有效映射中的已知秘密也会按原文精确替换，包括首次识别到秘密的请求中其他位置的全部相同原文。周围非秘密文本、协议结构及必要的上游连接认证保持不变。

返回的有效占位符会在回答、客户端工具参数及平台工具执行参数中还原为明文，流式响应同样支持。启用期间，工具结果再次进入模型请求前会重新受保护。映射按 API Key 隔离，重启后仍保留；同一 Key 下相同秘密在映射有效期内跨对话、跨分支复用占位符，共享 Key 就共享此访问边界。保留期沿用 History Marker 规则：发布前一小时，发布时延长至至少七天，随后随保留的历史续期，但不能复活过期映射。未知、过期或属于其他 Key 的占位符保持原样。检测、替换或映射存储故障会明确使请求失败或终止已经开始的流，不能绕过保护。

**关闭只停止新的检测与替换，不停止还原。** 映射不会被删除：已有有效占位符仍会在回答、客户端工具参数和平台工具参数中还原，直至过期。关闭期间，新的出站文本（包括含还原原文的工具结果）不再受本功能保护。

Model Turn 只有在还原及所需映射发布完成后才报告成功，不保存历史的内部 Agent 执行也遵循此约定。取消或超时可以打断发布等待，但不会撤销已经发布的映射。此后客户端交付若失败，该响应仍不会成为 Generation Chain 节点。

**旧工具历史：** 如果旧对话中的工具输出数组无法确认应如何解释，保护会在联系 Provider 前拒绝请求；此前以文本形式保存的数组也适用。请开始新对话以继续使用保护。关闭保护时，这类含义不明的旧载荷保持原样，不猜测其中的文本与媒体边界。

这是凭据文本保护，**不是通用 DLP，也不保证识别所有秘密**。检测可能误报或漏报，不扫描图片、音频、视频、二进制附件与不透明载荷。本地映射和客户端可见历史可能包含明文；数据库、磁盘及备份仍依赖部署环境保护，不新增应用层静态加密。模型可以将占位符放入 URL 或其他工具参数，使工具将还原后的凭据外传。既有工具授权与出站控制仍不可缺少；此开关不能撤销凭据，也不能阻止工具外传秘密。

还原后的秘密进入响应及工具执行诊断记录前，会被永久替换为 `***`，Debug Bundle 同样适用。客户端原始入站载荷仍沿用既有诊断脱敏策略；这不代表提示词、工具结果或诊断导出已普遍去除敏感信息。

### 本地管理

SvelteKit WebUI 可管理：

- 提供商、认证、模型发现和连通性检查
- 虚拟模型及其上游后端
- 可自动生成或自定义并编辑完整密钥的 API Key、模型绑定、有效期、Principal Concurrency Limit 和执行权限
- 支持全屏的实时 Interaction 因果森林请求记录、Rejected Request、时间序检查器及 Confirmed Upstream Usage 统计；实时预设按 5、10、30 分钟及 1、4、12、24 小时滚动，也可精确选择本地日期时间范围，边界固定且最长 24 小时。时间筛选按根链最新活动选中整棵根链并保留完整因果上下文；工具栏可进入或退出全屏，Esc 可退出全屏
- 在**额度总览**矩阵中查看 Provider 上报的配额、请求额度和余额，并按条件筛选、查看重置时间轴及基于 30 分钟采样的当前窗口耗尽预报；现场读取仍支持三分钟缓存、单个 Provider 刷新，并在刷新失败时保留上次成功结果
- 运行时设置
- SDK 与 AI 编码工具的可复制集成示例

Interaction Observation 在 Debug 与 Release 构建中均可用。进程级 **Debug** 开关每次重启后默认为关闭，启用前必须确认；每个新准入的 Inference Run 独立快照当时开关，因此切换只影响之后准入的 Run。Debug 记录 canonical checkpoint 与有序 HTTP、SSE、WebSocket 应用协议消息，不是 TLS record、TCP packet、HTTP/2 frame，也不保证应用 adapter 以下的 packet/chunk 保真。凭据 header、URL userinfo、疑似凭据的 query value 和结构化凭据字段会在持久化前永久脱敏；提示词、业务正文及工具输入/输出仍可能属于敏感数据。

Debug 脱敏覆盖协议凭据字段与单条应用消息中的完整可识别模式。跨多条消息拼接业务文本后才形成的凭据仍可能保留，必须继续按敏感数据处理。

Observation 元数据与托管 Debug Trace segment 共用 `log_retention_days`（默认七天）。每个 Run 的 Debug 上限为 64 MiB，总上限为 2 GiB；容量、队列、writer 或存储造成的丢失会显示为明确 gap 或 partial Trace，绝不改变推理结果。清除历史会保留 running 与 waiting-client Interaction，并报告跳过数量。Interaction 或 Rejected Request 可通过 60 秒、单次使用的下载 ticket 导出时间点固定的流式 ZIP；manifest 会记录 event sequence 边界和 `complete`、`partial` 或 `none` 状态。实时更新、Debug 状态、Trace 存储及 ticket 均限于单个 Gateway 进程；不提供集群级 fanout、共享 capture 存储或跨实例 ticket。

界面支持英文与简体中文、响应式导航，以及浅色、深色和跟随操作系统三种主题。首次使用时，简体中文（`Hans`）客户端 locale 会选择 `zh-CN`，不支持的 locale 使用英文；可在 Login 页面或**设置 → 外观**中无刷新切换语言，每个浏览器或桌面 WebView 分别记住自己的选择。

管理面会检查公开 GitHub Releases 中的可选更新。Stravia Desktop 启动时检查，只在用户操作后下载签名的 Windows x86_64/ARM64 NSIS 或 Linux x86_64/ARM64 AppImage 更新；standalone server 只报告精确 Release，绝不覆盖自身程序。成功结果缓存 24 小时，失败的自动尝试限流 1 小时，**设置 → 更新**始终可以立即重试。更新流量在实例启用 Outbound proxy 时使用该代理，否则直连 GitHub。

### 存储与部署

- 首次设置流程可选择 **SQLite** 或 **PostgreSQL**。
- 所选数据库连接只保存在 `server.toml`；不支持数据库命令行参数或环境变量覆盖。
- SQLx migrations 会保留当前受支持 schema 的数据，并在正常 Gateway 就绪前执行。
- `GET /healthz` 是存活探针；设置未完成或 Gateway 无法启动时，`GET /readyz` 返回未就绪。

PostgreSQL 数据库必须已由部署者创建，连接账户只需能创建和迁移 Stravia 自身表；Stravia 不创建数据库，也不要求 `CREATEDB` 权限。不兼容的旧 schema 会明确失败，而不是被删除或重建。

## 发布版本

版本 tag 会通过 [GitHub Releases](https://github.com/Stravia-AI/StraviaPlatform/releases) 发布 Server 压缩包和 Desktop 安装包，同时发布多架构容器镜像和 Nix package。当前发布范围：

- Server：Linux 和 Windows 的 x86_64、ARM64 架构；Linux 同时提供 GNU 与 musl 压缩包。
- Desktop：签名的 Tauri updater 产物，以及普通下载用的 Linux AppImage 和 Windows NSIS 安装包，均覆盖 x86_64 与 ARM64。
- 容器：`ghcr.io/stravia-ai/straviaplatform` 下的 `linux/amd64` 与 `linux/arm64`。
- Nix：仓库 flake 提供原生 `x86_64-linux` 和 `aarch64-linux` package，release 构建会推送到 [`stravia-platform` Cachix cache](https://app.cachix.org/cache/stravia-platform)。

当前不提供 macOS 产物。Linux GNU Server 压缩包和 Desktop AppImage 以 Ubuntu 24.04 为兼容基线；旧版 Linux 发行版应使用 musl Server 压缩包。Windows Desktop 安装包具有 updater 签名，但暂未进行 Authenticode 签名，因此仍可能触发 Microsoft Defender SmartScreen。

`SHA256SUMS` 列出了所有可下载构建产物。Desktop updater 产物同时提供 `.sig` 和版本化 `stravia-updater.json` 清单；应用内嵌的 updater 公钥会在安装前验证所选包。手动运行二进制或安装包前请先校验 `SHA256SUMS`：

```bash
sha256sum path/to/downloaded-asset
```

将结果与 `SHA256SUMS` 中对应文件的记录对比。Windows 用户可运行 `Get-FileHash C:\path\to\downloaded-asset -Algorithm SHA256`。

维护者应使用 `vMAJOR.MINOR.PATCH` 或 SemVer 预发布 tag；版本必须与 `Cargo.toml`、`package.json`、`tauri.conf.json` 一致，且 tag 对应 commit 必须属于 `master`。稳定版会更新完整版本、次版本、主版本和 `latest` 镜像 tag；预发布版只更新完整版本 tag。首次推送 GHCR 时创建的 package 默认为 Private，维护者需要在 package settings 中将可见性一次性改为 **Public**。

## 从源码快速开始

### 环境要求

- Rust `1.98.1`
- Bun `1.4.0`
- [Task](https://taskfile.dev/) `3.52.0`
- Python E2E 测试需要 uv `0.11.28`
- 原生 HTTP 依赖需要 CMake 和 Clang/libclang；Windows 构建还需要 NASM
- 动态 Local Search/Fetch 和 `task test:browser` 需要 Chrome/Chromium
- 构建桌面应用时需要 Tauri 对应平台依赖

### 运行独立服务端

```bash
# 使用 Vite WebUI 的开发模式
task dev:server

# 构建内嵌 WebUI 和 release 服务端二进制
task build:server

# macOS / Linux
./target/release/stravia-server

# Windows
.\target\release\stravia-server.exe
```

服务端监听 `127.0.0.1:23471`。Debug 构建使用仓库内的 `.stravia-dev/` 数据目录；Release 构建使用 `~/.stravia`。首次启动不会隐式选择数据库：控制台会打印一次性设置令牌，在 <http://127.0.0.1:23471/setup> 输入令牌后选择 SQLite 或 PostgreSQL，并创建唯一管理员。完成后用该用户名和密码登录，再配置提供商和模型路由。设置令牌在首次成功领取时即被消费；若设置尚未完成而进程重启，会生成新令牌。

### 使用 Nix 运行服务端

```bash
# 当前 checkout
nix run .

# 已发布 tag；将 vX.Y.Z 替换为所需版本
nix run github:Stravia-AI/StraviaPlatform/vX.Y.Z
```

flake 支持 `x86_64-linux` 和 `aarch64-linux`，会把内嵌 WebUI 与 Server 构建为一个 package，附带用于动态 Local Search/Fetch 的 Chromium，并将公开的 `stravia-platform` Cachix cache 配置为 substituter。显式设置 `STRAVIA_CHROME_PATH` 可覆盖附带的浏览器路径。

在 NixOS 中，可以从 flake 导入 service module：

```nix
{
  inputs.stravia.url = "github:Stravia-AI/StraviaPlatform";

  outputs = { nixpkgs, stravia, ... }: {
    nixosConfigurations.gateway = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        stravia.nixosModules.default
        {
          services.stravia.enable = true;
        }
      ];
    };
  };
}
```

service 默认监听 `127.0.0.1:23471`，使用动态系统用户运行，并将数据及 `/var/lib/stravia/server.toml` 持久化到 `/var/lib/stravia`。如需对外提供服务，请配置 `services.stravia.host`、`port` 和 `openFirewall`。监听非回环地址还必须将 `STRAVIA_PUBLIC_ORIGIN` 设为规范 HTTPS origin；把该非数据库设置放入 `services.stravia.environmentFile`，并由反向代理终止 TLS。数据库设置绝不从环境变量读取。已有 PostgreSQL 部署升级前，必须先按下文格式写入 `/var/lib/stravia/server.toml`，再启动升级后的 service。

### 使用 Docker 运行服务端

```bash
# 拉取最新稳定版多架构镜像
docker pull ghcr.io/stravia-ai/straviaplatform:latest

docker run --rm \
  --publish 127.0.0.1:23471:23471 \
  --env STRAVIA_PUBLIC_ORIGIN=https://gateway.example.com \
  --mount source=stravia-data,target=/data \
  ghcr.io/stravia-ai/straviaplatform:latest
```

如需从当前 checkout 构建，请运行 `docker build --tag stravia-server:local .`，并把最后的镜像名替换为 `stravia-server:local`。镜像内嵌生产 WebUI，在容器内监听 `0.0.0.0:23471`，以非 root 用户运行，并把 `server.toml` 和 SQLite 数据持久化到 `/data`。请在仅发布到回环地址的端口前放置 HTTPS 反向代理，并将 `STRAVIA_PUBLIC_ORIGIN` 设为完全一致的外部 origin；管理 Cookie 使用 Secure，非安全管理请求必须具有相同 origin 和 Stravia 的 CSRF header。不得通过 HTTP 直接暴露容器端口。内置健康检查会请求 `GET /healthz`；完成设置且 Gateway 成功启动前，就绪探针仍返回未就绪。

镜像附带 Chromium。动态渲染要求宿主机和容器策略允许 Chrome 沙箱及其 Linux namespace；启动被拒绝时，Stravia 不会退回 `--no-sandbox`。

创建名为 `my-model` 的虚拟模型后，可以通过任意受支持协议调用：

```bash
curl http://127.0.0.1:23471/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer YOUR_PROXY_KEY" \
  -d '{
    "model": "my-model",
    "messages": [{"role": "user", "content": "Hello"}]
  }'
```

只有当所选模型路由受到 API Key 保护时，才需要携带 Authorization 请求头。

### 运行桌面应用

```bash
# 开发模式
task dev:desktop

# 生产应用包
task build:desktop

# Windows NSIS 安装包
task build:desktop:installer
```

开发构建会把服务端和桌面端运行状态（包括 `gateway.db` 和桌面端固定端口配置）统一放在仓库根目录下已忽略的 `.stravia-dev/` 目录中。Release 服务端使用 `~/.stravia`；Release 桌面端仍使用操作系统的应用数据目录。

启用 `desktop-e2e` feature 的桌面构建（包括 Debug 模式）使用独立且已忽略的 `.stravia-desktop-e2e/` 目录。`task test:e2e:desktop` 会在其中写入假的 `9.9.9` 更新，用于验证下载和安装流程，不会下载或安装真实发布版本；这些测试夹具不得进入日常开发或生产数据。

桌面进程会在 `127.0.0.1` 上启动同一个统一 HTTP 应用。首次使用时，应用优先绑定默认固定端口 `23471`；后续启动会优先使用在 **设置 → 桌面端** 保存的固定端口。若首选端口无法绑定，Stravia 仍会使用临时随机端口保持可用，在概览页报告冲突，并允许用户无需重启即可重新检测或更换固定端口。此桌面本机设置不会改变下方独立服务端的参数。

## 服务端配置

常用命令行参数和环境变量：

| 命令行参数               | 环境变量                       | 默认值       |
| ------------------------ | ------------------------------ | ------------ |
| `--host`                 | `STRAVIA_HOST`                 | `127.0.0.1`  |
| `--port`                 | `STRAVIA_PORT`                 | `23471`      |
| `--public-origin`        | `STRAVIA_PUBLIC_ORIGIN`        | 回环地址自动推导；其他地址必须提供 HTTPS origin |
| `--config`               | —                              | `<data-dir>/server.toml` |
| `--data-dir`             | `STRAVIA_DATA_DIR`             | Debug：`.stravia-dev`；Release：`~/.stravia` |
| `--log-level`            | `STRAVIA_LOG_LEVEL`            | `info`       |
| `--config-poll-interval` | `STRAVIA_CONFIG_POLL_INTERVAL` | `3` 秒       |

`--config` 选择唯一的数据库配置来源。`--data-dir` 仍用于运行时产物及默认配置文件路径，不选择或覆盖数据库。配置文件缺失时进入首次设置；配置文件损坏、已配置数据库不可达或 schema 不兼容时启动失败，绝不回退到 SQLite。

设置流程会原子写入以下两种格式之一：

```toml
[database]
backend = "sqlite"
path = "/var/lib/stravia/gateway.db"
```

SQLite 文件名必须是 `gateway.db`。相对路径（包括设置向导默认的 `gateway.db`）以 `server.toml` 所在目录为基准，不依赖进程工作目录。向导保存解析后的绝对路径；已有绝对路径保持不变。使用默认 Debug 配置时，数据库位于 `<workspace>/.stravia-dev/gateway.db`。

对于已创建好的 PostgreSQL 数据库：

```toml
[database]
backend = "postgres"
url = "postgresql://stravia:replace-me@postgres.example.com:5432/stravia"
max_connections = 10
min_connections = 1
idle_timeout_seconds = 300
```

三个连接池设置均可省略。PostgreSQL URL 可能包含凭据，因此必须保护 `server.toml`。连接账户需有权在该数据库中运行 Stravia migrations，但无需创建数据库。已有 PostgreSQL 部署必须在首次运行升级版本**之前**，用当前连接 URL 创建此文件。只删除旧数据库环境变量而未创建该文件，会按设计进入设置流程；Stravia 不会推断旧 PostgreSQL 数据库，也不会静默选择 SQLite。

对于未配置数据库或已配置但还没有管理员的数据库，控制台令牌只能由 `POST /api/v1/setup/claim` 领取；得到的 `stravia_setup` HttpOnly、`SameSite=Strict` Cookie（`Path=/api/v1`，HTTPS 下同时为 `Secure`）可调用 `/api/v1/setup/test` 和 `/api/v1/setup/complete`。设置权限不能调用管理 API；数据库已有管理员时设置入口会关闭。`GET /api/v1/auth/state` 会报告设置、可用性与当前认证状态，但不会刷新凭据。Server 正常认证使用 `/api/v1/auth/login`、`/api/v1/auth/refresh`、`/api/v1/auth/logout` 和 `/api/v1/auth/credentials`。访问与刷新凭据只保存在 `HttpOnly`、`SameSite=Strict` Cookie（`stravia_access` 使用 `Path=/`，`stravia_refresh` 使用 `Path=/api/v1/auth`）中，不写入浏览器存储；HTTPS origin 下同时设置 `Secure`。远程管理必须使用 HTTPS 规范 origin。浏览器客户端会发送 `X-Stravia-CSRF: 1`；会修改状态的请求其 `Origin` 与 `--public-origin` 不一致时，Stravia 会拒绝请求。

忘记凭据时，使用同一配置运行本地交互命令：

```bash
./target/release/stravia-server --config /var/lib/stravia/server.toml recover-admin
```

该命令会提示输入用户名，并无回显地读取新密码及确认；密码不接受命令行参数。它会原地更新已有唯一管理员，并撤销全部旧管理会话；不会删除业务数据或重新开放数据库设置。

监听非回环地址前，必须通过 `--public-origin` 指定可信且可从外部访问的 Gateway origin（例如 `https://gateway.example.com`）。Stravia 不会信任转发 header 来推导管理 origin 或签名 Artifact URL。请由反向代理终止 HTTPS，并将请求原样转发至 Stravia listener：

```bash
./target/release/stravia-server \
  --host 127.0.0.1 \
  --port 23471 \
  --public-origin https://gateway.example.com
```

## 开发

```text
backend/crates/stravia-core/       与传输层无关的网关、协议、提供商、存储和管理服务
backend/crates/stravia-runtime-contract/ 共用 canonical IR、Hook、Agent、Artifact 与历史契约
backend/crates/stravia-media/      媒体理解实现与配置策略
backend/crates/stravia-web-search/ 搜索 Backend、报告、工具与配置策略
backend/crates/stravia-credential-protection/ 本地凭据检测、可逆保护与映射存储
backend/crates/stravia-devtools/   开发与协议 fixture 工具
backend/apps/stravia-server/       独立统一 HTTP 服务端
backend/apps/stravia-desktop/      Tauri 桌面外壳
frontend/stravia-webui/            SvelteKit 管理界面
tests/e2e/                         Python 后端 E2E 套件与协议录制样本
```

三个能力 crate 由 `stravia-core` 在编译期装配，只依赖共享契约，不反向依赖 core。Core 提供模型执行、授权、存储和观测的 Host Adapter。不引入动态加载或热卸载，HTTP/MCP 契约及持久化数据格式不变。Rust 调用方从 `stravia-runtime-contract` 导入共享类型，从各能力所属 crate 导入能力类型。

常用命令：

| 命令                     | 用途                                                 |
| ------------------------ | ---------------------------------------------------- |
| `task dev:web`           | 启动 WebUI 开发服务器                                |
| `task dev:server`        | 启动 Vite WebUI 和 debug 独立服务端                  |
| `task dev:desktop`       | 以开发模式启动 Tauri 桌面应用                        |
| `task check`             | 运行 WebUI 检查、ESLint、Rust 格式和 Cargo 检查      |
| `task test`              | 运行 WebUI 和受支持的 Rust 单元测试                  |
| `task test:browser`      | 使用本地夹具运行真实 headless Chrome 回归            |
| `task test:e2e:web`      | 运行 Chromium WebUI E2E 测试                         |
| `task test:e2e:desktop`  | 运行 Windows Tauri/WebView2 冒烟测试                 |
| `DB_URL=… task test:e2e` | 运行完整 Proxy、Admin、SQLite 和 PostgreSQL E2E 套件 |

如需单独验证 Google，请运行 `cargo test --locked -p stravia-web-access live_google_returns_parsable_destination_urls -- --ignored --nocapture`。该检查通过系统代理快照访问 Google，验证真实结果标题与目标 URL，不属于默认测试套件。

后端 Python 测试使用 `pyproject.toml` 中锁定的 `test` 依赖组，Task 通过 `uv run --locked` 执行。
Debug 服务端构建不会内嵌或提供 WebUI 资源。`task dev:server` 会同时启动 Vite 开发服务器和后端；Release 服务端构建仍会内嵌 WebUI。

`task dev:server` 会先启动 Vite，再把实际监听地址作为 `--public-origin` 传给后端。端口 `5173` 被占用时，Vite 会自动选择其他端口；请打开终端输出的精确 **Local** 地址。多个 workspace 并行开发时，各后端使用不同的 `STRAVIA_PORT`；前端端口无需固定。

如果分别启动 `task dev:web` 和后端，请把 WebUI 的实际来源传给后端，例如 `cargo run -p stravia-server -- --public-origin http://localhost:5174`。`localhost` 和 `127.0.0.1` 是不同的浏览器来源。首次设置未完成时重启服务，需要使用新 Server 进程输出的新设置令牌。

## 文档

- [架构设计](docs/design/architecture.md)
- [数据库结构](docs/database/schema.md)

## 许可证

Stravia 采用 [GNU Affero General Public License v3.0 only](LICENSE)（`AGPL-3.0-only`）许可。
单独许可的组件和资源继续适用各自条款。`stravia-web-access` 的原有代码采用 `CC0-1.0`；随附的 OMP 隐身脚本采用 [MIT](backend/crates/stravia-web-access/src/browser/stealth/LICENSE)，许可声明也包含在编译后的注入脚本中。内置字体采用各自许可证。
