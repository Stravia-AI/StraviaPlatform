# Antigravity CLI 系统提示词结构研究

调研范围是官方 CLI 的普通 Agent 推理请求与当前 Stravia 插件的请求组装，不复制 CLI 的完整提示词。先检查仓库脱敏历史材料、官方公开资料和本机 1.5.11 二进制，再经授权捕获一条原生出站结构并实测当前插件。以下 JSON 形状不含提示正文或身份值。

## 证据与版本

1. [仓库官方 CLI 1.5.8 Linux amd64 推理脱敏样本](../../backend/crates/stravia-core/src/plugin/network/shape/fixtures/antigravity-inference.json)：独立 HOME、空工作区、consumer、`--print`，一次 `streamGenerateContent`。正文只留键名、合成提示存在性及生成配置，**明确不保存系统提示正文**；不能从它推断 part 数量、内容或 role。重复样本见 [repeat](../../backend/crates/stravia-core/src/plugin/network/shape/fixtures/antigravity-inference.repeat.json)。
2. [仓库 1.5.11 原生捕获研究](antigravity-model-mapping.md#11-agy-1511mcp-工具分组labels-和历史提示词对照)：本地截断的交互会话有 20 个单声明工具组及请求内 labels；另一次空目录短提示样本有 14 个单声明工具组，见[第 10 节](antigravity-model-mapping.md#10-opus-high上游-429-与原生-cli-对照2026-11-18)。它们是两种不同会话，不是所有请求的固定数量。对 MCP typed 参数的三模型观察与工具闭环见 [1.5.11 schema 记录](antigravity-oauth.md#外层与内层)及[脱敏诊断 corpus](../../backend/crates/stravia-vendor-antigravity/tests/tool-schema-cli-1.5.11.json)。这些记录不是新的 1.5.11 系统提示全文捕获。
3. 本机官方 CLI 1.5.11 Linux amd64 ELF 静态字符串包含 `TemplateProvider.GetSystemPromptString`、`GetSystemPromptStringWithData`、`SystemPromptConstructionConfig.GetElementSeparator`，以及 `systemprompt.Build`、`Construct`、`ConstructCustom`、`(*protoPromptSection).ShouldInclude`、`core.RenderSystemPrompt`、`DefaultSerializer.buildSystemPromptGroups` 等符号；还包含一些标题和模板文本。可复核官方[1.5.11 发布产物](https://github.com/google-antigravity/antigravity-cli/releases/tag/1.5.11)。这些符号显示分节、条件判断与分组相关机制存在，但不能单独证明某个标题或模板被选进某次请求、更不能证明请求 part 的数目或顺序。
4. Google [Gemini GenerateContent API](https://ai.google.dev/api/generate-content) 和 [Content / Part API](https://aistudio.google.com/api-reference/generate-content#Resource:Content) 是公开接口定义；[Antigravity Rules](https://antigravity.google/docs/rules.md)、[MCP](https://antigravity.google/docs/mcp.md) 是产品行为说明。公开 Gemini 协议是结构背景，不自动成为 Antigravity 私有 `v1internal` 的稳定合同；当前网页也不应追认为各历史版本的逐字实现。

## 请求层次和大小写

1.5.8 实际样本使用 `POST /v1internal:streamGenerateContent?alt=sse`。外层记录 `project`、`requestId`、`request`、`model`、`userAgent`、`requestType`；提示指令在内层 **`request.systemInstruction`**，不是外层的 `systemInstruction`，也不是 `contents` 中一个已证明的 system 消息。同一 `request` 还包含 `contents`、`tools`、`labels`、`generationConfig`、`sessionId`。这些是一次样本的字段存在性，非每次全部必填或服务端完整 schema。[1]

```text
外层 { project, model, requestId, userAgent, requestType,
       request: { systemInstruction, contents, tools, labels,
                  generationConfig, sessionId } }
```

序列化字段名为 lowerCamelCase 的 `systemInstruction`，不是 `system_instruction`；`labels` 的部分标签键则为 snake_case。这是 JSON wire 名称，不能把 protobuf 源字段名或语言 SDK 的命名等同于抓包 casing。[1][2]

## parts、上下文和语义章节

公开 Gemini API 定义 system instruction 为 `Content`，以 `parts` 承载文本；普通会话 `contents` 也是有序的 `Content` 列表，含角色和 `parts`。公开 API 的 `Part` 除文本外还可承载函数调用／结果及其他媒体类型；不应把这些类型全都视为 system 文本。[4] **1.5.8 脱敏样本只证明 `systemInstruction` 键存在**，没有保留内部结构；因此下图是公开 API 的一般模型与插件形状，不是对该捕获内部的断言：

```text
request.systemInstruction.parts[]  -> 指令的各内容部分（常见 text）
request.contents[]                -> 角色及 parts：用户输入、模型历史、工具调用/结果
request.tools[]                   -> 可用工具声明及参数 schema
```

不能根据这个示意推断官方 CLI 固定使用一个 `text`、多个 part、某个 `role`、固定分隔符，或 system 文本与全部历史的相对拼接方式。模型生成时可看到多个上下文来源，但它们在请求中不因此成为同一段 system prompt。[1][4]

就语义而言，可有依据地区分以下来源，而不是宣称它们都已经出现在本次样本的 system parts 中：

- **Agent 身份、操作方式和工具使用说明**：CLI 二进制有系统提示构造入口；工具可用性另外通过 `tools` 声明。仅凭符号不能恢复完整章节及条件。[2][3]
- **项目和用户规则**：官方文档称 CLI 在 prompt expansion 评估 workspace、全局及插件规则；`always_on` 规则进入每轮 system prompt，`model_decision` 先给索引，其他触发方式有各自条件与大小限制。空工作区的捕获不代表全局或插件规则必然为空；该文档是产品说明，不是对 1.5.8 样本具体规则的观察。[1][5]
- **用户请求与历史**：与 `systemInstruction` 分列于 `contents`；会话历史和后续工具交互不是工具声明本身。一次 print 样本不能确定多轮历史的实际 role、合并策略或工具结果布局。[1][4]
- **工具能力和调用**：工具声明位于 `request.tools`；1.5.11 的 MCP 样本中 eager 声明每项单独分组，lazy 工具走另一机制；已发生的调用／结果属于会话过程，不能与声明混为一谈。具体工具说明是否同时渲染进 system 指令，现有样本没有证明。[2]
- **运行环境、策略、输出格式及可选扩展**：这些可能因模式、工作区、配置和功能而改变。1.5.8 夹具没有保留正文；下文的 1.5.11 实测只保留标题类型和主题摘要，不能断言完整章节顺序。[1][3]

## 与当前插件对照

以当前 [wire.rs](../../backend/crates/stravia-vendor-antigravity/src/wire.rs) 为准，历史研究不覆盖此后的实现：

- `infer` 使用公共 Gemini codec 编码调用方请求，再递归 `sanitize_request`。内层只允许 `contents`、`systemInstruction`、`generationConfig`、`tools`、`toolConfig`、`safetySettings`；其后依可信宿主会话条件添加 `sessionId`，并由插件生成 `labels`。外层由插件构造 `project`、`model`、`request`、`userAgent`、`requestType`、`requestId`。这与 1.5.8 样本的位置和大小写相容，但不是完整字节等价。
- `content(..., true)` 将 system 对象投影为仅 `parts`；普通内容保留 `role` 和 `parts`。每个 part 按代码白名单投影，包括 `text`、媒体、`functionCall`、`functionResponse`、`thought`、`thoughtSignature` 等；这只是插件接受/输出的字段集合，不代表官方每类 part 都放入系统指令。插件不保留 system 的 `role`；仓库脱敏 1.5.8 样本不能判断该差异是否存在于原生普通推理。单独的 1.5.8 **搜索专用**请求静态研究记录 system role 为 `user`，但不是普通 Agent 提示词，不能外推。[搜索研究](antigravity-cli-web-search.md#3-请求构造与后端分支)
- 原生同协议非空函数声明组会拆成每组一个声明；否则 canonical 工具逐个转换为 `functionDeclarations`；schema 有损投影，工具选择另写 `toolConfig`。这对齐 1.5.11 已观测的分组，而不复制 CLI 内置工具或其 lazy MCP 注册逻辑。[2]
- 插件保留调用方的系统指令文本，不再删除 RFC 2119 片段或改写 OMP 关键句。默认仅添加 CLI 原生身份前缀及桥接说明，并将顶层指令移入合成首条 user 消息；独立 system/developer 消息仍在系统区。关闭 `system_prompt_prefix` 时不加前缀、不插入合成历史。它不复制 CLI 的完整 Agent system prompt。[实现](../../backend/crates/stravia-vendor-antigravity/src/wire.rs)及[历史对照背景](antigravity-model-mapping.md#11-agy-1511mcp-工具分组labels-和历史提示词对照)。

## 1.5.11 本机抓样与插件实测

在空工作区、临时 HOME 中运行官方 `agy --model gemini-3.8-flash-low --disable-slash-commands --print 'Reply exactly with OK.'`。CLI 在临时环境刷新现有账号登录态，OAuth 和模型目录返回 200。首条原生推理在本地代理截断；CLI 的 `OK` 来自本地 SSE，不是 Google 推理结果。采集不保存凭据、原始报文或提示正文。

|观察项|原生 CLI 1.5.11 对该模型的首轮请求|
|---|---|
|系统位置和形状|`request.systemInstruction = {role: "user", parts: [{text: …}]}`，正文 19,865 UTF-8 字节、221 行。该 `role` 在系统对象内，不是会话消息。|
|正文组织|一个文本 part 内混用 XML 风格标签和 Markdown 标题，不是每节一个 part。脱敏摘要按顺序识别到身份、产物、指南、技能、用户信息块；正文还检测到工具、工作流、编码、规划、环境、沟通和安全等主题。主题命中不是章节标题的完整清单。|
|用户输入|`request.contents` 有一条 `role: "user"` 消息、一个 text part，含测试提示；系统 part 不含测试提示。|
|工具和元数据|14 个 `request.tools` 项。`labels`、`sessionId`、`generationConfig` 与上述字段同级。|

随后独立宿主加载当前源码新构建的 `stravia-vendor-antigravity` Wasm，通过真实 Gemini codec 解码中性系统指令和同一条用户提示。仅将该次 CLI 请求的 OAuth access token 和 project 在内存中交给插件，未重放原生系统文本或工具。宿主在发出前断言插件的系统对象仅含 `parts`、普通用户消息独立、无工具、`maxOutputTokens=512`。插件向 Google 发送恰好一次推理，无重试或回退：**HTTP 200，解码结果为 `OK`**。还用同一个新构建运行 `system_instruction_compatibility_preserves_messages_and_tools`，1 项通过。测试未写生产库或原 CLI 登录文件；临时宿主不是正式 Gateway 出站。结果证明此账号的短提示可用，不证明角色差异在其他场景下无影响。

## 已知与未知

1.5.8 历史夹具只证实系统字段位置；新的 1.5.11 样本确认本场景的系统 `role`、parts 切分和正文组织，也确认插件推理投影不保留该 `role`。一次短提示的成功不能证明 1.5.11 在不同模型、模式、多轮、规则或工作区下都使用单 part，也不能说明服务端如何解释系统 `role`。

## OMP 18.22.2：移动原提示词，不替换关键句

历史 429 对照把未改写的 OMP 指令直接放在 `systemInstruction`；不是这个实验。此处从 agy 1.5.11 的同模型首轮请求提取原生 `<identity>` 块（295 字节，CLI 推理在本地截断）。临时桥把真实 OMP 请求改成：

1. `systemInstruction`：原生身份块，加一条指示模型将首个合成 user 消息作为本会话的编码指令。
2. `contents[0]` / `user`：OMP 的原始提示词（24,740 字节），逐字保留，包含四个已知片段。
3. `contents[1]` / `model`：合成确认「收到，我会使用该提示词作为系统提示。」
4. `contents[2]` / `user`：OMP 的原始首条任务，包括客户端附带的前置文本；11 个工具声明仍随请求发送。

本地用占位身份块先验证了三个消息的角色、顺序和文本相等。第一次带原生身份块的准备因临时宿主输入校验退出，没有取得 Google 推理状态；离线复现定位到 OMP 的前置文本也被算进任务字符串，不将这个失败计为上游 429。修正后经重新授权，真实 OMP 18.22.2 经过临时 Responses 桥和未修改的 Antigravity Wasm 向 `gemini-3.8-flash-low` 发送一次推理。实际出站系统区 421 字节，宿主逐字校验移入 `contents[0]` 的原提示词及合成历史。**Google HTTP 200；OMP 输出 `OK`，退出码 0**。无重试、回退或工具调用，未写入生产库或原 CLI 登录文件。

这证明该结构在此账号、模型和短任务上可以不替换四句话而跑通。未在同轮重新发送未重排的基线，不能断定只因角色变化而消除 429，也没有证明模型以系统级优先级执行移动后的提示词。这次实验只让宿主构造的已知首条消息参与提示，不把客户端的普通用户输入提升为系统指令。

## 插件前缀的精确边界

后续重新采集 agy 1.5.11 的首轮请求，在原生 `systemInstruction.parts[0].text` 的第 0 字节开始取样。`<identity>` 前没有空白；第 295 字节结束身份块，紧跟一个 LF，因此第一个原生段落及分隔符共 **296 字节**，SHA-256 为 `acea3799fe9d52bc0fbabb261219491f2e707e87f12a337a6d1c81aa717bcd39`。CLI 推理仍在本地截断，不转发、未执行插件线上推理。

插件 `system_prompt_prefix` 默认对所有推理客户端启用，从第 0 字节使用这段原生前缀。只有存在有效顶层指令时才追加引导语及合成用户/确认消息；引导语要求合成指令与原生身份前缀冲突时以合成指令为准，保留其他 system/developer 约束，后续真实用户消息仍是任务。真实 Wasm/loopback HTTP 契约用抓样摘要核对前 296 字节，覆盖 OMP、普通 Open Responses、Chat Completions、Anthropic Messages 和 Gemini 原生请求：顶层指令保持文本与可用分段，已替换或清除的顶层指令不会被旧 Gemini 快照覆盖，独立 system/developer 消息与真实历史保留。关闭选项时顶层指令留在系统区，不添加前缀或合成历史；RFC 与 OMP 原句均保留。上述跨协议验证未发送真实上游推理。

[1]: ../../backend/crates/stravia-core/src/plugin/network/shape/fixtures/antigravity-inference.json
[2]: antigravity-model-mapping.md#11-agy-1511mcp-工具分组labels-和历史提示词对照
[3]: https://github.com/google-antigravity/antigravity-cli/releases/tag/1.5.11
[4]: https://ai.google.dev/api/generate-content
[5]: https://antigravity.google/docs/rules.md
