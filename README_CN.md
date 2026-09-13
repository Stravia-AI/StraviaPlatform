<h1 align="center">Stravia</h1>

<p align="center">
  本地运行、可自托管的 Agent infra（智能体基础设施）——提供模型接入、工具执行与内置 Agent 运行能力，统一访问控制、历史与可观测性。
</p>

<p align="center">
  <a href="README.md">English</a>
</p>

> **项目状态：** Stravia 当前版本为 `0.1.0`，仍在积极开发中。稳定版本发布前，配置格式和数据库兼容性可能发生变化。

## 项目简介

Stravia 定位为 **Agent infra（智能体基础设施）**，面向使用 AI 编程客户端或构建智能体应用的开发者，将模型接入、平台自有工具执行与有界内置 Agent 循环整合到一个可本地部署的系统中。

协议网关是模型接入层，而不是产品的全部。客户端继续使用自身支持的协议；Stravia 负责解析虚拟模型、选择上游后端，并在必要时转换请求与响应。

执行层运行平台自有工具，将结果送回模型并继续后续轮次。其有界 Agent Runner 驱动本地 Agent 联网搜索，也被多模态理解复用。这些能力通过兼容的模型请求与 MCP 提供；共享的身份、访问控制、历史、用量统计和诊断让开发者统一管理模型接入与平台执行。

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

Windows 任务栏与托盘采用透明底「节律」图形，随系统颜色模式自动切换黑白，隐藏到托盘后仍生效。Stravia 未运行时，固定快捷方式使用静态应用图标。

## 当前能力

### 平台工具与内置 Agent 执行

- **平台自有工具执行：** 向兼容的模型请求暴露工具，在 Stravia 内执行平台工具调用，并携带结果继续模型轮次。客户端自有工具仍由客户端负责执行。
- **有界 Agent 循环：** 在时间、轮次、token 和工具预算内协调模型与工具轮次，支持受控工具并发、取消和输出校验。
- **内置能力：** `StraviaRead` 统一读取文件与网页，通过 `query://` 返回带来源的 Search Report，并回答受支持图片的问题。搜索与媒体结果保留 `previous_turn_id` 显式续接与分支。
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

OpenAI-compatible Thinking Preview 使用 Markdown 空行分隔独立块及 summary/content part；同一 part 内的 delta 保持连续。每个权威 Thinking block 各有一个 History Marker，包括公开的无签名 Thinking；同一块中的多个 part 共享该 Marker。段落空白只属于 Preview，不改变原始 Thinking。流式与非流式交付产生相同的可见排版。

重新提交完整历史的客户端必须原样保留 History Marker 与 Projection Delimiter。Stravia 会删除仅用于展示的 Preview 字节，包括新增的段落空白，并在原位置恢复权威 Thinking、ToolCall 与 ToolResult。Thinking 原文保留原始空白和 part 边界；向兼容上游回放时使用原始签名或密文，而不是展示用 Markdown。删除 Marker 或 Delimiter 会被视为有意编辑历史。客户端关闭流式传输时，Stravia 会先执行仅含 Platform Tool 的隐藏续轮，再一次性返回语义等价的 buffered projection。live stream 则在启动对应 Platform Tool 前交付并发布每个 Marker。

切换 Target 时优先继续会话。历史推理的 Target、账号/配置、模型与协议来源兼容时原生回放；否则仅在该 Target 的请求中保留可见文本，省略不可用的密文或签名。原始历史保持不变：只要历史与 Marker 仍可用，包括重启之后，切回兼容来源仍可重新使用原密文。旧记录缺少来源时不伪造来源；同协议可尝试原生回放，跨协议保守降级。上游在任何输出前明确拒绝 encrypted content 或 thinking signature 时，允许一次省略受保护推理的完整回放，不扩展为普通错误重试。该策略不放宽工具、普通消息或原生压缩的硬要求。

OpenAI direct 与 Codex OAuth 的生成 Target 会为 Chat Completions、Open Responses、Anthropic Messages 和 Gemini 请求使用上游 Responses WebSocket，不受客户端是否流式影响；Embeddings 仍只使用 HTTP。Hook 与协议可表示性检查完成后，Stravia 可从最长且严格等价的 canonical item 前缀续接；Principal、精确 Target、Provider 账号与配置、resolved model、instructions、tools、reasoning、response format 和请求控制必须全部一致。任一条件不匹配都会发送面向当前 Target 的完整历史；推理降级后不复用不兼容的续接前缀。

`POST /v1/responses` 以 Open Responses 2026-04-24 作为 canonical baseline，同时接受结构安全的 rolling additive 字段和 hosted tool 声明。同协议 Target 保留这层 compatibility envelope；跨协议 Target 可以省略 advisory 字段和未被强制选择的 hosted tools，但绝不省略普通内容或硬约束。历史推理采用上述面向当前 Target 的回放策略。后台执行仍不支持。

客户端发起的远程压缩请求转发给正常路由选定的 Target。`POST /v1/responses/compact` 是独立的 HTTP unary 操作，返回包含 retained items 与 opaque state 的完整下一窗口；后续必须完整回放该窗口，不能自行裁剪或改写。Responses 同时承载原生 compaction item、内嵌触发项，以及客户端提交的 `context_management` 控制。这些属于协议硬要求：Target 协议无法承载时返回不支持，不能静默丢弃；compact 操作不是空 Generation。

Stravia 不提供平台级压缩设置，不注入默认压缩控制，也不生成本地摘要。Target 压缩能力未知不阻止转发：由上游决定是否接受请求及客户端提交的阈值。成功或错误均返回客户端，不为完成压缩而重试或切换 Target。显式空集合与 null 仍由客户端控制。

已登记的原生状态在保留期内可跨重启恢复已知来源，但不会恢复已移除的历史；Target、账号与配置 generation、模型及协议必须保持兼容。监控区分已确认生成关系、原生桥接与保留尾部推断关联；推断关联不改变推理，也不启用 Target Continuation。清理监控历史不删除有效原生状态映射；普通监控不包含 opaque payload，未报告的压缩用量保持 unknown。

自动历史父节点发现比较完整消息语义，而不是追踪元数据。客户端省略应用元数据与内部追踪字段，不会破坏其他内容完全一致的历史前缀；消息角色、内容、工具调用与结果关联、受保护推理、原生压缩状态及未分类协议扩展仍参与比较。启动时按当前语义重建旧历史前缀索引，不改写原始历史或父边；上游续接仍要求 Target 与请求控制兼容。

客户端切换模型并更新顶层提示后，未改动的完整交互（包括公开思考预览及其 History Marker）仍可在同一 API Key 下，与唯一来源形成诊断关联，但不建立执行父链。被修改的预览或标记、私有思考、歧义证据或不可用的进程内索引不能用于这种关联；既有请求记录不回填。

### 提供商与模型路由

当前内置的提供商元数据包括：

- OpenAI 与 Codex OAuth 通道
- Anthropic 与 Claude Code OAuth 通道
- Google Gemini 与 Vertex AI
- DeepSeek、Moonshot AI、Zhipu AI、Z.AI、MiniMax、xAI（API Key 与 Grok OAuth）和 NVIDIA
- OpenRouter、Ollama 以及自定义 OpenAI 兼容端点

客户端发送一个 **Model ID**。该值就是 Route ID，匹配时包含字母大小写在内完全精确。逻辑 Model 还可以设置可选、可重复的展示名称；展示为空时回退到 Model ID，并且永不参与路由、授权或绑定。对应 Route 可以同时保留已启用和已禁用 Target；已禁用 Target 保留配置但不会接收流量。Stravia 先选择可用的最高 Target Priority 组，再在组内使用 Traffic Equalization 或 Latency Preference；适用时，Conversation Affinity 与 Cache Affinity 可继续偏好此前成功的已启用 Target。Stravia 从 revisioned `models.stravia.cn` 索引刷新 Provider Catalog：轻量 Provider 与 Canonical Model 索引以同一 revision 原子更新，Provider-scoped inventory 仅在需要时加载。Catalog Provider 使用其 scoped inventory；账号级 discovery 仍决定可调用的模型 ID，Core 只为精确匹配补充元数据，不会加入仅存在于 Catalog 的模型。

当前 revision 下没有可用的 Provider inventory 缓存时，Stravia 会先刷新全局索引，再下载该模型目录，因此本地索引落后时无需另行手动刷新。下载期间目录再次变版或刷新失败时，操作仍会明确失败，且不会改动已保存的 Provider Models。

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

可选的联网搜索通过 `StraviaRead` 调用，输入为 `{"url":"query://URL-encoded%20question"}`，返回终态、带来源的 Search Report，而不是单页搜索结果。成功结果包含答案、已引用的公网 HTTP(S) 来源、限制、完成状态、用量和稳定 `turn_id`。将该 ID 作为 `previous_turn_id` 传入，可从同一 Principal 的完整祖先链续接或创建独立分支；Stravia 不会隐式选择“最新”Turn。网页 URL 返回 Markdown，不启动研究 Agent。内部搜索 Agent 使用相同工具名，但其 `query://` 只执行基础检索，不递归启动研究。

在 WebUI 中配置一个 Search Backend。Local Search 使用有界 Agent 编排有序的内部 Web Access Search/Fetch 来源：自动创建的进程内 Local Provider、Exa 或智谱。每个 Web Provider 都可独立选择是否使用 Gateway 代理。Codex Agentic Search 固定到一个精确且兼容的 Codex OAuth Responses Provider/model，不使用 Local budget。Local 与 Codex 之间不做 fallback。

进程内 Local Provider 内嵌 [Stravia 的 Moli 引擎](https://github.com/Stravia-AI/moli-stealth)：`moli-stealth-net` 负责 HTTP Search/Fetch，`moli-core` 使用 V8 渲染动态页面。桌面端与服务端均不需要安装 Chrome/Chromium、外部 Moli 可执行文件或 Node/Bun sidecar。浏览器执行按需在专用所有者线程启动。

在 **联网搜索 → 搜索与网页来源** 中直接选择 Local，无需配置浏览器路径。浏览器路径管理接口及 `STRAVIA_CHROME_PATH` 设置已移除。已有 `web-access-browser.json` 和 `desktop-browser.json` 文件保留不动，但不再读写。远程 Exa 与智谱服务保持不变。

HTTP 使用 Moli 的 Chrome 传输指纹；指纹缓解措施不保证绕过反爬检测。HTTP 与浏览器路径保留所选 Gateway 代理快照、分离的 Cookie 归属和 Fetch 安全限制。浏览器 HTTP 与 WebSocket 流量经过校验出口代理，不进行 TLS 中间人解密，证书校验保持启用。直连固定到校验通过的公网地址；显式选择的上游代理仍负责自身 DNS 解析。Moli 在 Stravia 进程内执行，不再使用具有操作系统沙箱的 Chrome 子进程；部署时应采用最小权限，并为不可信页面执行配置适当的宿主机或容器隔离。

平台联网搜索总开关同时控制所有有效 API Key 的搜索与网页读取。每个 Key 分别控制 MCP 访问和透明注入；选中且已开启的联网与媒体能力合并为一个 `StraviaRead` 声明，执行层强制检查本次暴露范围。注入偏好不限制显式调用或 MCP。MCP 客户端连接 `POST /mcp`，通过 `Authorization: Bearer <key>` 认证并发现 `StraviaRead`；文件下载检查归属，联网和媒体操作还分别检查平台开关。Provider 原生 web-search 工具类型不变。旧平台 `web_search`、`web_fetch` 和 `understand_media` 调用别名不再注册。

Google 浏览器搜索检测到验证码或异常流量页时，会提前返回明确的自动流量拦截错误，包括首页 preflight 阶段，而不是继续等待搜索结果超时。这不会自动解答验证码或绕过 Google 的网络限制。

联网搜索与 Web Access 配置属于部署本地状态，不参与配置导出/导入。Search Turn 只保留 Report 元数据与引用 URL，不保存抓取的网页正文或内部 Agent transcript。

Local Fetch 和浏览器出站检查会拒绝去除主机尾随点后成为非公网 IP 的 URL，例如 `http://127.0.0.1../`，与 Web Access 准入保持一致。无需配置迁移，代理选择和 DNS 职责分工不变。

### Media Understanding

Media Understanding 通过 `StraviaRead` 读取静态 JPEG、PNG 与 WebP 图片，例如 `{"url":"https://stravia/artifact/<id>?question=Describe%20the%20image"}`。裸 Artifact Reference 只返回下载信息，不调用模型。外部图片 URL 先收存，再描述内容并提取可读文字；原 URL 的查询参数完整保留，不作为平台指令。若父 Route 存在支持图片的 Target，Stravia 会原生交付已保存的图片；否则，支持工具的父 Model 可调用已配置的隐藏视觉 Model，并获得包含 source ArtifactId 与可分支 `turn_id` 的强校验 Media Report。新 Artifact 问题可同时携带 `previous_turn_id` 续接。原生视觉 Route 失败后不会 fallback 到隐藏 Model。

在**多模态理解**页面启用平台能力、选择逻辑 Model 并设置思考等级。选择器只列出所有 Target 都明确声明图片输入能力的已启用 Model；思考等级选择器只列出每个 Target 都支持的等级。启用后，所有有效 API Key 都能通过 `StraviaRead` 请求理解；MCP 访问和透明注入仍由每个 Key 独立控制。隐藏调用计入调用方配额，但不会授予所选 Model 的直接访问权。外部文件仅允许公网 HTTP(S)，校验 DNS、固定实际连接地址并逐跳校验重定向。预处理始终生成有界的有损 JPEG derivative，忽略 ICC profile，因此精确颜色或细小文本 OCR 可能不准确。

### 文件存储与临时传输

未配置 S3 时直接使用内部存储。可选 S3 沿用相同上传步骤：`POST /v1/artifacts/uploads` 创建，携带 `x-upload-token` 调用 `PUT /v1/artifacts/uploads/{upload_id}/parts/{part_number}`，最后调用 `POST /v1/artifacts/uploads/{upload_id}/complete`。完成结果保留原有文件元数据，并增加形如 `https://stravia/artifact/<opaque-id>` 的 `reference`。这是稳定、Principal-scoped 的文件身份，不是网络下载地址或凭据。同一 API Key 可跨对话使用，其他 Principal 无权解析。

结构化内联媒体和远程附件 URL 必须先保存成功，模型调用才会开始。普通文本链接和类似 base64 的文本不会自动下载或改写。`StraviaRead` 导入普通文件 URL，返回引用、MIME、大小、文件名（或不透明回退值）和临时下载地址，不自动解压、执行或理解文件。

在初始化或设置中保存完整的**客户端访问地址**，包括协议、端口与部署路径前缀。保存值始终有效，后续 Host 和转发头不能替换它。**外部签名下载**和**上传提示词注入**分别默认关闭：

| 外部签名下载 | Provider 输入 | 客户端下载 |
|---|---|---|
| 关闭，内部或 S3 | 从 Artifact 生成 base64 | 保存的客户端地址下的平台签名 URL |
| 开启，内部存储 | 文件公开地址下的平台签名 URL | 相同公开入口 |
| 开启，S3 | 原生 S3 预签名 URL | 原生 S3 预签名 URL |

启用外部签名下载时必须填写文件公开地址，初始带入客户端地址或 S3 endpoint。保存成功不代表可达性已验证；实际客户端和 Provider 必须能够访问该入口，S3 存储桶无须公开。URL 通常有效十五分钟；每次实际调用 Provider 前，包括重试和后续模型轮次，至少剩余五分钟。协议不能表达 URL 时，在发送前选择其支持的内联形式；上游 URL 抓取失败不会触发自动 base64 重跑。

上传提示词注入提供真实的分片上传 curl 流程。模型只见 `<stravia-upload-key>`；仅交付给客户端的普通回答和客户端工具参数会替换为临时上传凭据，思考和平台工具参数不签发。同一响应复用仍有效的凭据；每个凭据固定有效十五分钟，支持多文件且只授权上传。关闭注入不撤销已有凭据，但撤销所属 API Key 仍会拒绝上传。客户端回传的凭据，包括过期凭据，在发送给 Provider 或平台持久化前均恢复为占位符，不依赖一般凭据保护开关。

单文件上限为 100 MiB。每个 Principal 最多保留十六个未完成且未过期的上传任务，声明大小合计最多 400 MiB；完成上传释放暂存名额。不设已保存文件总容量上限。鉴权后的文件使用按请求保留期续期；签名下载和文本提及不续期。过期引用不能复活；进行中读取和未过期下载授权只延迟物理清理，不延长逻辑保留期。签名 URL 是可转交的临时凭据：持有者在有效期内可下载对应文件。

新历史和诊断以引用与元数据外置结构化媒体，不把媒体捕获称为原始 wire 字节；缺失或过期正文明确标为不可恢复。升级不回填、改写或续期旧历史。修改或移除 S3 endpoint／bucket 凭据不会迁移现有对象；对象仍在使用时，应保留匹配的存储配置。

### 凭据保护

在**高级功能 → 凭据保护**中切换开关，即时保存实例级凭据文本保护设置。功能**默认关闭**，启用后统一应用于所有有效的 Stravia API Key，不设单 Key 豁免，不是 MCP 工具，也不提供透明注入选项。独立服务器与 Desktop 共用核心设置和行为；客户端沿用原有协议及明文视图。

页面默认打开**现有规则**，以可搜索、排序、分页的表格展示当前版本的完整只读目录。列表展示规则名、关键词，以及存在的路径、组合或仅作组合条件标记，不再重复名称与 ID；规则 ID 不显示，但仍可用于搜索。详情侧栏分别展示关键词、匹配表达式、排除条件、路径限制和必需或可选的组合匹配，凭据提取、优先级、置信度与规则用途收在**规则参数**中。**命中记录**按客户端交互汇总新建的保护映射，直接关联对应请求记录。有效映射复用或占位符还原不算新发现，过期后重建重新计入；不同 API Key 分别判断，并发创建只计一次。请求失败或取消后，已经发生的发现仍可保留。摘要仅展示规则、来源类型、数量和请求状态，不展示秘密、占位符或消息片段。观察可能缺失，沿用请求记录保留期，不构成安全审计保证。

**匹配测试**页签在宽屏并排展示输入与结果，手机上改为上下布局。测试器接受单个密钥或带上下文的多行文本，选择命中结果可选中对应原文；仅将主动提交的文本发送给当前 Stravia 实例。即使保护关闭，也使用同一个本地检测器并返回规则与原输入位置。测试不保存输入、不写入诊断、不联系模型供应商或验证服务、不检索已保存凭据、不建立映射或观察记录，也不修改设置。**未匹配到现有规则**不代表内容安全或凭据无效。

每次请求模型前，Stravia 在本地将系统指令、用户与历史消息、工具参数、工具结果及平台内部请求中检测命中的凭据替换为不透明占位符。内置 Betterleaks 与 Kingfisher v1.109.0 离线规则随 Stravia 版本更新，运行时不下载规则，也不联网验证凭据。合并目录包含 462 条 Betterleaks 规则及 1,013 条 Kingfisher 规则（861 条可报告规则、152 条隐藏辅助规则），可补充识别没有变量名的智谱格式及部分 `sk-` 凭据。格式命中不证明凭据的厂商归属或有效性，扩大检测范围也可能增加误报。同一 API Key 下有效映射中的已知秘密也会按原文精确替换，包括首次识别到秘密的请求中其他位置的全部相同原文。周围非秘密文本、协议结构及必要的上游连接认证保持不变。

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

同一 API Key 下精确续接父响应的请求，在没有新增用户输入、返回待完成客户端工具调用的结果（不限时间），或于该父响应完整交付后两秒内到达时，继续归入原 Interaction。后两种情况允许夹带新增用户输入，已完成的 Interaction 也可重新激活。两秒规则同样会归并真人快速追问，不表示识别出了 harness hook，不丢弃输入，也不改变模型执行；其他新增用户输入开启新的 Interaction。

Desktop 点击 **Debug 诊断包**后，通过一次性下载票据交由系统浏览器下载，桌面检查器保持打开。WebUI 则由当前浏览器处理下载。

Interaction 卡片在缩小的模型名下分别展示用户输入与模型输出预览。悬停或键盘聚焦任一预览可查看更多内容；触控设备点按预览即可打开。两个预览均使用经过安全过滤的 Markdown，不加载图片或嵌入资源。输入展示用户消息开头，输出在内容更新时持续显示最新一行。卡片保持统一固定高度，底部不显示 Debug 捕获或筛选命中标签。

新交互随请求记录保存最多 4,096 字符的凭据脱敏用户输入文本，无需开启 Debug。该预览不包含系统指令、历史消息或工具结果，工具续跑不会覆盖原始输入。旧记录、没有文本的消息，以及输入保护完成前就终止的请求，在卡片预览中说明未记录文本。展开的输出预览展示已保留的输出尾部，不保证包含完整回答。

观察侧栏默认显示只读对话：用户消息靠右、模型回复靠左，正文安全渲染 Markdown，包括 GFM 表格。连续同一模型的回复共用一次头像和名称，只在整块末尾保留最后一条消息的时间。对话时间旁不重复显示预览说明或执行状态，缺少正文时不生成空白气泡。**诊断**仍保留可读事件时间线、已记录的结果、原始事件数据和技术标识。历史消息直接显示，实时新增文本逐字呈现，不拆分 emoji 或组合字符。向上翻阅时暂停自动跟随，点击**回到最新**恢复；减少动态效果开启时直接显示新文本。回复气泡仍只来自已保留的客户端可见输出事件，不把思考或工具 payload 当作模型回复，既有观察更新频率不变。

新请求记录在 Debug 关闭时也采集模型可读思考、客户端和平台工具的输入及返回。这些内容沿用既有凭据脱敏策略和请求记录保留期，但仍可能包含敏感业务数据，并增加存储用量。模型思考的签名和密文不进入普通思考采集。旧记录中未采集的内容不会补录。

Debug Trace 内容仅通过 **Debug 诊断包**下载提供，不再设置内嵌的 Debug 记录页签。交互与拒绝请求的详情响应只包含普通观察事件和捕获元数据，不读取 Trace 分段，也不返回 `debug_events`。关闭 Debug 不影响下载此前已采集的记录。

画布实时更新使用只含目标交互及其完整根链的轻量摘要查询；只有检查器选中的交互才加载 Run 事件和工具正文。关联事件按有界批次读取，不再为每个交互单独查询数据库。筛选和因果上下文语义保持不变。

画布保留完整的已加载拓扑，只挂载当前视口及可见连线端点需要的卡片。新卡片等待 worker 计算位置，不再为了测量而先把整张图堆到原点渲染。拖动、缩放、跟随活动与打开详情的行为保持不变。

检查器首次加载最新 200 条事件，之后增量读取；可分页加载更早记录并保持阅读位置。实时文本先以未保存状态显示，提交后替换预览，不重复文本，也不拆开 Markdown 消息。新文本块达到 16 KiB 或约两秒时封口，只在体积下降时压缩。正常调度下允许约两秒未落盘窗口，进程崩溃可能丢失这部分观察文本；存储失败单独提示，该窗口不是故障期间的持久化时限保证。普通详情读取不强制落盘，下载诊断包只刷新选中的 Interaction，并包含固定截止点内的完整历史。既有事件不改写、不删除。

新写入的客户端工具观察记录只有在调用 ID、同一 Principal 的明确 Run 祖先关系、最近 handoff、脱敏后结果正文及错误状态均证明结果重复时，才跳过回放写入。新调用、变化的结果与不确定边界仍保留。Debug Wire 和 canonical checkpoint 直接进入 Trace，不再每帧追加普通数据库事件；ZIP 采集与固定快照截止水位保持可用。这些变更不回写或删除既有历史。

对话中的**思考**与**调用工具** Marker 展示普通观察事件中的已记录内容，不自动开启 Debug，也不从 Debug Trace 回退补充。有详情的条目默认折叠，每条独立在当前浏览器、当前站点记住展开状态，只保存状态与标识，不保存正文。没有可读思考正文时不显示思考条目；工具没有详情时只显示名称，不添加缺失说明。完整 canonical 与应用协议记录仍需开启 Debug，并在下载的诊断包中检查。

每个 Run 内的诊断事件按记录时间排序，同一时间按 sequence 排列，不再按模型或工具的父子树展开。连续的响应文本更新或同名客户端工具交接默认折叠为带次数和时间范围的分组，展开后可逐条检查；每条事件右侧的箭头用于查看原始数据。交付、结束、失败及中间出现的其他事件仍单独保留，实时追加不会收起已展开的分组。模型服务尝试显示 Token 输出速度，使用该次尝试最后确认的输出用量和既有生成耗时计算规则；缺少对应用量或有效耗时时显示未知，不作估算。

输出浮层命名为**模型输出预览**；已确认执行来源的连线保留，但不再重复显示文字标签。保留尾部推断关联沿用直连的上下布局与连接路径，仅改用虚线，连线和卡片预览均不添加推断说明；关联详情仍可在诊断中查看。

Debug 脱敏覆盖协议凭据字段与单条应用消息中的完整可识别模式。跨多条消息拼接业务文本后才形成的凭据仍可能保留，必须继续按敏感数据处理。

WebSocket Ping/Pong 事件保留类型、方向与时间，其任意字节载荷按策略省略，并标注 `control_frame_payload_omitted`。这种策略性省略不将 Trace 标为 partial，也不把载荷归类为媒体。已有 Trace 记录不会改写。

Observation 元数据与托管 Debug Trace segment 共用 `log_retention_days`（默认七天）。每个 Run 的 Debug 上限为 64 MiB，总上限为 2 GiB；容量、队列、writer 或存储造成的丢失会显示为明确 gap 或 partial Trace，绝不改变推理结果。清除历史会保留 running 与 waiting-client Interaction，并报告跳过数量。Interaction 或 Rejected Request 可通过 60 秒、单次使用的下载 ticket 导出时间点固定的流式 ZIP；manifest 会记录 event sequence 边界和 `complete`、`partial` 或 `none` 状态。实时更新、Debug 状态、Trace 存储及 ticket 均限于单个 Gateway 进程；不提供集群级 fanout、共享 capture 存储或跨实例 ticket。

界面支持英文与简体中文、响应式导航，以及浅色、深色和跟随操作系统三种主题。首次使用时，简体中文（`Hans`）客户端 locale 会选择 `zh-CN`，不支持的 locale 使用英文；可在 Login 页面或**设置 → 外观**中无刷新切换语言，每个浏览器或桌面 WebView 分别记住自己的选择。

管理面会检查公开 GitHub Releases 中的可选更新。Stravia Desktop 启动时检查，只在用户操作后下载签名的 Windows x86_64/ARM64 NSIS 或 Linux x86_64/ARM64 AppImage 更新；standalone server 只报告精确 Release，绝不覆盖自身程序。成功结果缓存 24 小时，失败的自动尝试限流 1 小时，**设置 → 更新**始终可以立即重试。更新流量在实例启用 Outbound proxy 时使用该代理，否则直连 GitHub。

缓存中的更新在提供给用户前会与当前运行版本重新比较。升级后，即使离线，也不会再提供相同或更旧的缓存版本。

### 存储与部署

- 首次设置流程可选择 **SQLite** 或 **PostgreSQL**。
- 所选数据库连接只保存在 `server.toml`；不支持数据库命令行参数或环境变量覆盖。
- SQLx migrations 会保留当前受支持 schema 的数据，并在正常 Gateway 就绪前执行。
- `GET /healthz` 是存活探针；设置未完成或 Gateway 无法启动时，`GET /readyz` 返回未就绪。

PostgreSQL 数据库必须已由部署者创建，连接账户只需能创建和迁移 Stravia 自身表；Stravia 不创建数据库，也不要求 `CREATEDB` 权限。不兼容的旧 schema 会明确失败，而不是被删除或重建。

审阅数据库结构时，可执行 `stravia-tools dump-schema --backend sqlite --output deploy/schema/sqlite.sql`，导出隔离内存数据库执行全部迁移后的结构。PostgreSQL 使用 `--backend postgres --output deploy/schema/postgres.sql`，需要开发环境的 `DATABASE_URL`、`CREATEDB` 权限和兼容的 `pg_dump`；工具创建并删除临时数据库，不迁移源数据库。这些仅是开发工具要求，不是 Gateway 部署要求。两种导出均只含结构，供审阅而非初始化部署。详见 [Database Schema](docs/database/schema.md)。

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
- 原生 Moli/V8 依赖需要 CMake、Clang/libclang、Go 1.24 或更高版本及 Python；Windows 构建还需要 NASM 与 MSVC C++ 工具链
- 首次构建下载固定版本的 V8 预编译归档；部署后的程序不下载浏览器
- Linux 构建需要 pkg-config 与 Fontconfig 开发头文件；运行镜像需要 Fontconfig
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

flake 支持 `x86_64-linux` 和 `aarch64-linux`，会把内嵌 WebUI、Moli 引擎与 Server 构建为一个 package，并将公开的 `stravia-platform` Cachix cache 配置为 substituter。V8 归档在沙箱构建前通过固定哈希获取，不捆绑外部浏览器。

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

service 默认监听 `127.0.0.1:23471`，使用动态系统用户运行，并将数据及 `/var/lib/stravia/server.toml` 持久化到 `/var/lib/stravia`。如需对外提供服务，请配置 `services.stravia.host`、`port` 和 `openFirewall`。HTTP 无需入口配置即可使用，包括非回环监听。如需限制入口或信任反向代理，可在 `services.stravia.environmentFile` 中设置可选的 `STRAVIA_ADMIN_ORIGINS` 与 `STRAVIA_TRUSTED_PROXIES`；代理契约见下文，不会自动信任任何代理网段。数据库设置绝不从环境变量读取。已有 PostgreSQL 部署升级前，必须先按下文格式写入 `/var/lib/stravia/server.toml`，再启动升级后的 service。

### 使用 Docker 运行服务端

```bash
# 拉取最新稳定版多架构镜像
docker pull ghcr.io/stravia-ai/straviaplatform:latest

docker run --rm \
  --publish 127.0.0.1:23471:23471 \
  --mount source=stravia-data,target=/data \
  ghcr.io/stravia-ai/straviaplatform:latest
```

如需从当前 checkout 构建，请运行 `docker build --tag stravia-server:local .`，并把最后的镜像名替换为 `stravia-server:local`。镜像内嵌生产 WebUI，在容器内监听 `0.0.0.0:23471`，以非 root 用户运行，并把 `server.toml` 和 SQLite 数据持久化到 `/data`。此示例无需入口配置即可通过 `http://127.0.0.1:23471` 直连 HTTP。远程暴露时优先使用 HTTPS 反向代理及显式 `STRAVIA_ADMIN_ORIGINS` 列表。`STRAVIA_TRUSTED_PROXIES` 只能配置容器实际看到的代理对端（Docker NAT 可能使它成为网桥地址而非 `127.0.0.1`）；请检查网络拓扑，不要信任所有容器或全部网络。遵循下文转发契约并隔离后端端口。内置健康检查会请求 `GET /healthz`；完成设置且 Gateway 成功启动前，就绪探针仍返回未就绪。

镜像内嵌 Moli，不再安装 Chromium。浏览器代码运行在 Stravia 进程内，应保留非 root 容器运行方式及正常的宿主机隔离。

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
| `--admin-origin`         | `STRAVIA_ADMIN_ORIGINS`        | 未配置：不限制管理入口 |
| `--trusted-proxy`        | `STRAVIA_TRUSTED_PROXIES`      | 未配置：不信任任何代理 |
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

对于未配置数据库或已配置但还没有管理员的数据库，控制台令牌只能由 `POST /api/v1/setup/claim` 领取；得到的 `stravia_setup` HttpOnly、`SameSite=Strict` Cookie（`Path=/api/v1`，HTTPS 下同时为 `Secure`）可调用 `/api/v1/setup/test` 和 `/api/v1/setup/complete`。设置权限不能调用管理 API；数据库已有管理员时设置入口会关闭。`GET /api/v1/auth/state` 会报告设置、可用性与当前认证状态，但不会刷新凭据。Server 正常认证使用 `/api/v1/auth/login`、`/api/v1/auth/refresh`、`/api/v1/auth/logout` 和 `/api/v1/auth/credentials`。访问与刷新凭据只保存在 `HttpOnly`、`SameSite=Strict` Cookie（`stravia_access` 使用 `Path=/`，`stravia_refresh` 使用 `Path=/api/v1/auth`）中，不写入浏览器存储；HTTPS origin 下同时设置 `Secure`。HTTP 和 HTTPS 入口均支持完整管理流程。浏览器客户端会发送 `X-Stravia-CSRF: 1`；会修改状态的请求其 `Origin` 与独立恢复的请求外部源不一致时，Stravia 会拒绝请求。

忘记凭据时，使用同一配置运行本地交互命令：

```bash
./target/release/stravia-server --config /var/lib/stravia/server.toml recover-admin
```

该命令会提示输入用户名，并无回显地读取新密码及确认；密码不接受命令行参数。它会原地更新已有唯一管理员，并撤销全部旧管理会话；不会删除业务数据或重新开放数据库设置。

### 管理入口与反向代理

默认无需入口配置：所有可达的有效 HTTP 或 HTTPS 入口均可进行首次设置、登录及管理，仍受身份认证和同源／CSRF 保护。默认监听保持 `127.0.0.1:23471`；非回环监听也支持 HTTP，无需“不安全模式”开关。TLS 由反向代理终止，Stravia 不内建 TLS listener。

如需限制整个管理面（WebUI、设置、登录、认证状态及全部管理 API 读写，包含初始化与不可用状态），重复使用 `--admin-origin`，例如 `--admin-origin http://192.168.1.20:23471 --admin-origin https://gateway.example.com`，或设置 `STRAVIA_ADMIN_ORIGINS=http://192.168.1.20:23471,https://gateway.example.com`。入口按规范化的协议／主机／有效端口精确匹配；IPv6 使用方括号，例如 `http://[::1]:23471`。不允许通配符、凭据、页面路径、query 或 fragment。省略表示不限入口；显式空项或非法项会使启动失败，不会退回放行。列表不改变模型 API、MCP、健康探针及其既有授权／CORS。两个入口均被允许不表示可以跨源管理：每个写请求的 `Origin` 必须独立匹配恢复的请求外部源，继续满足 CSRF 和 JSON 要求。这不是管理 CORS 允许列表。

默认不信任任何代理：外部源来自直连 `Host` 和 HTTP，不受信 TCP 对端的转发头被忽略。可重复使用 `--trusted-proxy` 配置实际直接对端 IP 或 CIDR，或使用逗号分隔的 `STRAVIA_TRUSTED_PROXIES`；非法项或显式空项会使启动失败。信任只来自实际 TCP 对端，绝不依据 `X-Forwarded-For` 等客户端声明。只信任自己控制的代理地址；宽泛 CIDR（尤其 `0.0.0.0/0` 或 `::/0`）会让其他客户端冒充外部入口。受信代理必须覆盖客户端传入的 `X-Forwarded-Proto`，使其为唯一的 `http` 或 `https`，并覆盖 `X-Forwarded-Host`，使其为唯一的浏览器侧 authority，包含非默认端口。两者必须同时提供；重复头、逗号链、非法／冲突信息及任何 RFC `Forwarded` 头均被拒绝。受信对端既不发送这两个头也不发送 `Forwarded` 时，按直连 Host／HTTP 处理。多级代理只能由最后一个受信对端提供一对权威且已清洗的声明；Stravia 不解析转发链。代理信任不能豁免入口列表或 CSRF。

例如，在 Stravia 同一主机运行 Nginx，在所示路径安装证书及私钥，然后使用下列配置。`$http_host` 保留外部端口；`$scheme` 表示浏览器连接到此 TLS 边缘的协议。HTTP 代理可改用 HTTP 监听及 HTTP 允许入口，沿用相同头指令。位于其他 TLS 终止器之后的内部代理必须使用另行保护、清洗的上游契约，不能把自身 HTTP 回源协议声称为浏览器协议。

```bash
./target/release/stravia-server \
  --host 127.0.0.1 \
  --port 23471 \
  --admin-origin https://gateway.example.com \
  --trusted-proxy 127.0.0.1
```

```nginx
server {
    listen 443 ssl;
    server_name gateway.example.com;
    ssl_certificate /etc/nginx/tls/gateway.crt;
    ssl_certificate_key /etc/nginx/tls/gateway.key;

    location / {
        proxy_pass http://127.0.0.1:23471;
        proxy_set_header Host $http_host;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header X-Forwarded-Host $http_host;
        proxy_set_header Forwarded "";
    }
}
```

两个列表仅属于部署配置，重启生效，不写入 WebUI／数据库。列表误配置时可本地修改并重启恢复。升级时删除旧 `--public-origin`／`STRAVIA_PUBLIC_ORIGIN` 及 Server `--admin-cors-origin` 设置；不保留兼容别名。残留的 `STRAVIA_PUBLIC_ORIGIN` 会使 Server 启动失败并提示迁移，避免静默丢弃旧入口限制。客户端／文件访问地址仍是独立配置。

设置、访问和刷新 Cookie 的设置与清除均按每个请求恢复的外部协议决定：HTTPS 使用 `Secure`，即使代理通过 HTTP 回源。HTTP 可用不以削弱 HTTPS Cookie 为代价；`HttpOnly`、`SameSite=Strict`、撤销和 CSRF 继续有效。不承诺不同主机间会话互通。同一主机的 Cookie 不按端口或协议隔离：混用 HTTP 与 HTTPS 时，HTTP 可能无法替换已有 Secure Cookie。应优先采用不同主机名或统一 HTTPS，而非取消 Secure 或扩大 Cookie Domain。HTTP 也不保证需要安全上下文的浏览器 API 可用。

**安全警告：** HTTP 会使密码、会话与管理请求遭到窃听或篡改。允许 HTTP 或未限制入口时的启动警告说明配置风险，不代表某个 HTTPS 代理请求实际走了明文。默认不限入口会失去固定主机允许列表提供的部分 DNS rebinding 防护。CSRF、SameSite、内网和代理信任均不能替代 TLS 或网络隔离。不可信网络应使用 HTTPS、显式入口列表及防火墙／后端端口隔离；能连接的客户端可构造 Host，因此入口列表不是防火墙。

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
| `task test:browser`      | 使用本地夹具运行内嵌 Moli 回归                       |
| `task test:e2e:web`      | 运行 Chromium WebUI E2E 测试                         |
| `task test:e2e:desktop`  | 运行 Windows Tauri/WebView2 冒烟测试                 |
| `DB_URL=… task test:e2e` | 运行完整 Proxy、Admin、SQLite 和 PostgreSQL E2E 套件 |

### Rust 构建复用

仓库根目录的裸 `cargo build`、`cargo check` 和 `cargo test` 默认只选择 `stravia-server`，使默认构建的依赖特性与 `task dev:server` 一致，避免先编译 server／desktop 的 workspace 特性并集。全量构建使用 `cargo build --workspace`，全工作区检查使用 `task check`，受支持的单元测试使用 `task test`；这些命令的范围保持不变。

- **服务端开发：** 直接运行 `task dev:server`，或用 `cargo build --locked` / `task build:server:debug` 预编译，不需要先构建整个 workspace。
- **桌面开发：** 直接运行 `task dev:desktop`。Tauri 会选择自己的依赖特性并传入 build script 配置；裸 workspace 构建不等于桌面开发模式的精确预编译。
- **Rust 测试预编译：** 使用 `cargo test --locked --workspace --exclude stravia-desktop --no-run`，与 `task test` 对齐。测试 harness、`cfg(test)` 和 dev-dependencies 启用的特性需要额外产物，普通 build 不能代替。运行局部测试时，预编译与执行保持相同的 `-p` 选择。

Cargo 复用的是输入一致的产物，不是 `target/debug` 中的任意产物。切换包选择、features、工具链、target triple、编译参数或 build script 环境，均可能触发重新编译。`cargo check` 不等于代码生成预编译，release 产物也不能代替 debug 产物。保持这些输入稳定，不要把 `cargo clean` 当作日常开发步骤。仓库已启用增量编译，Windows 已使用 LLD；第三方依赖在开发模式下使用 `opt-level = 3`，以较慢的首次编译换取运行速度。

为避免在 server、desktop 和 tests 之间切换时重编 Moli 的原生 TLS，`stravia-web-access` 有意将 `libc`、`regex` 和 `serde_core` 声明为构建依赖，统一它们的构建期特性。即使没有本地 `build.rs`，这些声明也是特性解析的统一锚点，不是未使用的运行期依赖。根目录 dev profile 还固定了 `bitflags` 2.x 和 `glob` 的 debuginfo，避免 Cargo 的构建期／运行期产物共享机制在不同入口下改变实际编译参数。该优化不统一运行期代理、JSON 或测试专属 features，其他配置专属产物仍然必要。调整这些声明后可能需要一次重新编译；之后保留生成的缓存即可。

Server 集成测试还会使用测试依赖图构建普通可执行文件。因此切回 `cargo build -p stravia-server` 时，即使所有库产物都已复用，仍可能重新编译／链接这个二进制（`UnitDependencyInfoChanged`）；这不等于重编原生 TLS 或依赖缓存丢失。

如需单独验证 Google，请运行 `cargo test --locked -p stravia-web-access live_google_returns_parsable_destination_urls -- --ignored --nocapture`。该检查通过系统代理快照访问 Google，验证真实结果标题与目标 URL，不属于默认测试套件。

后端 Python 测试使用 `pyproject.toml` 中锁定的 `test` 依赖组，Task 通过 `uv run --locked` 执行。
`task test:e2e:web` 也会构建生产 Server，以运行真实管理入口浏览器测试。这些测试要求 PATH 中存在 `openssl`，创建隔离的本地 TLS 代理，并仅在测试浏览器中信任临时证书公钥；不会修改系统信任或全局关闭证书校验。
Debug 服务端构建不会内嵌或提供 WebUI 资源。`task dev:server` 会同时启动 Vite 开发服务器和后端；Release 服务端构建仍会内嵌 WebUI。

`task dev:server` 会先启动 Vite，再把实际监听地址作为 `--admin-origin` 传给后端，并仅信任 Vite 代理的 `127.0.0.1` TCP 对端。Vite 用 `http` 和传入的 authority（含实际端口）覆盖转发头对，并移除 `Forwarded`。端口 `5173` 被占用时，Vite 会自动选择其他端口；请打开终端输出的精确 **Local** 地址。多个 workspace 并行开发时，各后端使用不同的 `STRAVIA_PORT`；前端端口无需固定。

如果分别启动 `task dev:web` 和后端，请把 WebUI 的实际来源传给后端，例如 `cargo run -p stravia-server -- --admin-origin http://localhost:5174 --trusted-proxy 127.0.0.1`。`localhost` 和 `127.0.0.1` 是不同的浏览器来源。首次设置未完成时重启服务，需要使用新 Server 进程输出的新设置令牌。

## 文档

- [架构设计](docs/design/architecture.md)
- [数据库结构](docs/database/schema.md)

## 许可证

Stravia 采用 [GNU Affero General Public License v3.0 only](LICENSE)（`AGPL-3.0-only`）许可。
单独许可的组件和资源继续适用各自条款。`stravia-web-access` 的原有代码采用 `CC0-1.0`；随附的 OMP 隐身脚本采用 [MIT](backend/crates/stravia-web-access/src/browser/stealth/LICENSE)，许可声明也包含在编译后的注入脚本中。内置字体采用各自许可证。
