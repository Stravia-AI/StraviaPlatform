# Antigravity CLI 网页搜索实现调研

调研日期：2026-11-17。范围：官方 CLI 1.5.8 Linux x64 的静态实现、官方文档及 Stravia 当前接入边界。没有执行 CLI、读取登录凭据或发起模型请求。

## 结论

- CLI 的网页工具是 `search_web`，公开参数为必填 `query`、可选 `domain`。它构造另一轮模型生成请求，依赖上游搜索能力，再把摘要与来源整理回工具结果；不是本地解析 Google 搜索结果页。
- CodeAssistClient 从账号模型目录的 `webSearchModelIds` 取首项作为搜索模型，不是固定使用当前聊天模型，也不是硬编码 `gemini-3.1-flash-lite`。此次没有读取账号目录，不能给出实际搜索模型 ID。
- CCPA 路径使用非流式 `generateContent`，`requestType = "web_search"`；内部 API 路径为 `/v1internal:generateContent`。其他认证与后端路径不能一概套用该 endpoint。
- `domain` 在该请求构造器中是追加到查询的 ` site:` 条件，不是 Stravia 的严格允许域名校验，不能据此承诺来源一定在指定域内。
- 调研时 Stravia Antigravity 插件没有完整 Web Search capability；本轮已按现有 External Route 契约实现，模型发现、操作身份和安全来源解析见第 7 节。

## 1. 一手证据与方法

分析对象：[官方 CLI 1.5.8 Linux x64 归档](https://github.com/google-antigravity/antigravity-cli/releases/download/1.5.8/agy_cli_linux_x64.tar.gz)，[发布元数据](https://api.github.com/repos/google-antigravity/antigravity-cli/releases/tags/1.5.8)。下载归档大小为 104,805,905 字节，计算 SHA-256 与官方 release digest 一致：

```text
905d4accfbd61bc00bee84bfaf24c59b85d334cabd786cf99237b66cfe06ab98
```

仅解压、读取 ELF，恢复 Go PCL 函数名、解码内嵌 protobuf 描述符并用 `objdump` 核对调用与分支。所有地址、偏移只适用于该版本与架构。临时归档、二进制及分析文件已清理；没有将二进制作为程序执行。

官方 [SDK 工具文档](https://antigravity.google/docs/sdk/tools/)也列出 `search_web`（Google Search）与 `read_url_content`（URL 读取），但 SDK 清单不能单独证明 CLI 每个版本的实现。[CLI 命令参考](https://antigravity.google/docs/cli/reference/)中的 `/search` 则是 `/codesearch` 的别名，搜索工作区代码，不是网页搜索命令。

## 2. CLI 工具与搜索调用链

上述 ELF 中直接恢复的函数与地址：

|函数|链接虚拟地址|可确认行为|
|---|---|---|
|`SearchWebTool.ParameterSchema`|`0x8f7ca80`|泛型符号包含参数 schema：`query` 必填，`domain` 可选|
|`SearchWebTool.Execute`|`0x8f7cae0`|拒绝空查询；调用 `ResolveModelFromTierConfig`，经接口调用搜索，再格式化结果|
|`ResolveModelFromTierConfig`|`0x89c5360`|上层工具使用的模型解析入口；不能与下层目录选择混为一步|
|`formatSearchWebResult`|`0x8f7d9c0`|工具结果格式化入口|
|`CodeAssistClient.GetWebSearchResults`|`0x9552300`|选择搜索模型、读取模型信息与 project，构造请求，调用生成并转换响应|
|`getWebSearchModelID`|`0x9534220`|读取缓存目录的 `webSearchModelIds[0]`；无候选时报 `no web search models available`|
|`websearch.NewCCPARequest`|`0x94edf80`|构造搜索请求、域名查询条件和 system instruction|
|`CodeAssistClient.GenerateContent`|`0x95257e0`|按条件分派后端|
|`generateContentResponseToWebSearchResults`|`0x95526e0`|转换生成响应，并处理 attachment 到知识库结果的路径|
|`websearch.FormatMasterResponse`|`0x94ee5a0`|提取摘要、插入引用并整理来源|

`Execute` 的搜索调用是接口间接调用；上述 CodeAssistClient 是已确认存在的实现，不能由此断言所有认证环境都走它。二进制另有 Gemini API 搜索实现。

底层 `GetWebSearchResultsRequest` 描述符位于文件偏移 `0x6a09e98`，包含 `metadata` #1、`query` #2、`limit` #3、`domain` #4、`thirdPartyConfig` #5、`imageSearch` #6、`model` #7、`customModelInfoOverride` #8。这些是内部接口字段，不代表都暴露为 `search_web` 参数。空查询错误为 `search query cannot be empty`。

`FetchAvailableModelsResponse` 描述符位于 `0x6627a36`，`webSearchModelIds` 为字段 #8。此前[模型映射研究](antigravity-model-mapping.md)也识别过该用途清单；此次进一步直接确认 CodeAssistClient 的搜索模型选择读取它。

## 3. 请求构造与后端分支

### CCPA 请求

`NewCCPARequest` 对 `domain` 做 `TrimSpace`，非空时将 ` site:` 与域名拼接到 query；构造 user 内容、要求执行网页搜索的 system instruction，并设置 `requestType = "web_search"`。该 instruction 字符串位于文件偏移 `0x64653eb`，不是普通聊天请求原样转发。

`generateContentFromCCPA` 在 `0x9525c67` 向 POST helper 传入方法名 `generateContent`。API 描述符在 `0x661158a` 包含 `/v1internal:generateContent`。外层 `GenerateContentRequest` 描述符位于 `0x661062f`：

|字段号|JSON 字段|
|---|---|
|1|`project`|
|2|`requestId`|
|3|`request`|
|4|`model`|
|5|`userPromptId`|
|6|`userAgent`|
|7|`requestType`|
|8|`enabledCreditTypes`|

描述符只证明字段存在，不证明每次请求都填充所有字段。此路径与仓库 [1.5.8 普通推理捕获](../../backend/crates/stravia-core/src/plugin/network/shape/fixtures/antigravity-inference.json)中的 `streamGenerateContent?alt=sse` 不是同一个请求场景。

### 搜索工具分支：当前确证范围

`GetWebSearchResults` 的 `0x9552444–0x9552505` 比较认证模式 `business_licensed_enterprise`、`business_agent_platform_paygo`；其他路径检查所选 `ModelInfo.apiProvider == 3`。产生的布尔值传入 `NewCCPARequest`，后者在 `0x94ee007` 分支构造不同工具对象。

补充静态分析已通过 Go runtime typedesc 与对象字段偏移确认：该布尔值为 true 时构造 `enterpriseWebSearch`，false 时构造 `googleSearch`；`apiProvider == 3` 对应 `API_PROVIDER_GOOGLE_VERTEX`。目录 `ModelDetails.apiProvider` 为字段 #16，转换为 `ModelInfo` 时原样复制，缺失时按 protobuf 默认值 0。不能照搬旧版的 `tier.usesGcpTos` 条件。

普通 Google 分支固定构造 `googleSearch.enhancedContent.imageSearch.maxResultCount = 5`，不等于下游搜索来源数量；enterprise 分支不带该字段。`generationConfig` 仅设置 `candidateCount = 1`。`systemInstruction` 的 role 也是 `user`，使用搜索专用指令，不携带 CLI Agent 提示词。具体证据为 `NewCCPARequest` 的 `0x94ee007` 分支、Tool 对象的 `+0x48` EnterpriseWebSearch 与 `+0x28` GoogleSearch 字段、参数模板文件偏移 `0x6ca25a8` 的 `+0x64 = 5`、GenerationConfig 的 `+0x20 = 1`；目录字段描述符位于 `0x6626be1`。

作为有版本限定的补充：[CLI issue #1086](https://github.com/google-antigravity/antigravity-cli/issues/1086)的报告者提供了 1.2.9 抓包和静态分析，观察到 `search_web` 向 `daily-cloudcode-pa.googleapis.com/v1internal:generateContent` 发送 `requestType: "web_search"`，当次搜索模型为 `gemini-3.1-flash-lite`，工具为 `enterpriseWebSearch`；其分析将工具选择关联到认证模式与 `currentTier.usesGcpTos`。这是报告者的一手观察，**不是本次对 1.5.8 的实测或官方稳定契约**。

报告者还观察到 HTTP 200、非空文本却没有 `groundingMetadata`，并称替换成 `googleSearch` 后也不是每次都触发 grounding。维护者在同一版本的自身环境中得到正常搜索结果。因此不能把该故障推广到所有账号或版本，也不能把 HTTP 200 当作已搜索证明。

## 4. 摘要、引用与来源

`FormatMasterResponse` 在无 candidates、无 summary 时明确报错；调用 `summaryWithCitations`、`citationSources`。前者解析引用偏移并稳定排序插入引用标记；后者可输出 `[%d] [%s](%s)` 或 `[%d] %s`，处理空白标题、缺失标题 `Untitled` 和 Markdown destination 转义。底层响应描述符位于 `0x6a0a052`，字段为 `results` #1、`webSearchUrl` #2、`summary` #3。

这证明 CLI 会加工模型搜索结果，而不是仅返回原始链接列表。本次没有完整核验所有无效 offset、重复引用、多 candidate 边界，也没有证明 1.5.8 是否强制拒绝缺少 grounding 元数据的结果。1.2.9 报告提到的 `webSearchQueries`、`groundingChunks`、`groundingSupports` 属于旧版实际响应证据，不应替代当前版本响应验证。

## 5. Stravia 接入边界

调研时 [Antigravity 插件能力与分派](../../backend/crates/stravia-vendor-antigravity/src/lib.rs)包含 Infer、MediaImage、AuthOauth、ModelDiscovery、Allowance，没有完整 Web Search operation。本轮新增的 Search Adapter 与部署要求见第 7 节。

[请求工具投影](../../backend/crates/stravia-vendor-antigravity/src/wire.rs)允许 `googleSearch`、`googleSearchRetrieval` 等；`googleSearch` 仅投影 `excludeDomains`、`blockingConfidence`、`includedDomains`、`timeRangeFilter`。它没有允许 `enterpriseWebSearch`，也不保留旧版报告中的 `enhancedContent`。这只是推理请求投影，不是上述 CLI 搜索调用链。

[Stravia Web Search](../design/web-search.md)的 External Route 要求 Vendor Plugin 返回完整、有引用的 `SearchReport`；宿主继续验证报告、来源公网 URL、DNS 与允许域名。Local 则由 Stravia 运行搜索／读取循环，校验实际 evidence。两者不能与 CLI 的内部 `search_web` 返回值直接画等号。

接入必须确认搜索模型目录、工具分支、grounding、引用偏移、域名限制、计费与条款，并将真实来源归一到完整 capability。仅有生成文本、HTTP 200 或 `site:` 条件不足以满足 Stravia 的搜索证据契约。本轮已完成上述静态协议确认并实现 Adapter；真实账号、地域和当前服务端行为仍需经授权调用验证，不能由本地响应回放推断。

## 6. 静态复核入口

重新下载、核对上述哈希并解压后，`readelf -S -W antigravity` 可定位 `.lrodata` 中的 Go PCL 头（文件偏移 `0x1776cd0`）。可用 Go `debug/gosym.NewLineTable` 解析，函数 entry 加 `0x6f68080` 得到上表链接地址。关键函数可用以下命令检查：

```bash
objdump -d --start-address=0x9534220 --stop-address=0x9534300 antigravity
objdump -d --start-address=0x9552300 --stop-address=0x95526e0 antigravity
objdump -d --start-address=0x94edf80 --stop-address=0x94ee5a0 antigravity
objdump -d --start-address=0x94ee5a0 --stop-address=0x94ef740 antigravity
```

上述 message 偏移处可按 protobuf wire 格式读取长度界定的 `DescriptorProto`；其字段 #2 是 `FieldDescriptorProto`，后者的 name #1、number #3、label #4、type #5、type-name #6 可用于复核字段身份。静态分析不证明实时服务端支持、账号权限、地域可用性或实际返回内容。

## 7. Stravia 完整搜索接入

- [catalog.rs](../../backend/crates/stravia-vendor-antigravity/src/catalog.rs) 独立发现 `webSearchModelIds`，为真实搜索身份声明 `search`，不把搜索专用项加入推理清单。`antigravity_search` 扩展保存 `model_id` 与可选的原始 `api_provider`；同公开 ID 碰撞时保留唯一记录及原有推理 selector／档位表，搜索使用已保存的操作身份，不在执行时切换到目录首项。
- [search.rs](../../backend/crates/stravia-vendor-antigravity/src/search.rs) 独立构造非流式搜索请求，不经过 Infer 白名单。复用 OAuth、project 和出站形态，按目录的 provider enum 选择已核验的工具对象。来源数量限制和输出语言通过搜索指令表达，域名通过 `site:` 提示；最终域名约束仍由宿主执行。
- Adapter 只接受单个 `STOP` candidate，连接非 thought 文本，按原始 Part 的 UTF-8 字节半开区间插入 `[sc:<id>]`，对重复 URL／引用去重。缺 grounding、无来源、坏索引、非法字符边界、引用文本不匹配、空白摘要或非完整终态明确失败，不复制 CLI 的模糊文本查找 fallback，也不发布未验证摘要。用量复用 Gemini codec，缺失保持未知。
- [宿主搜索适配](../../backend/crates/stravia-core/src/web_search/host.rs)仅解析精确 Google grounding 跳转 URL，复用执行成功时的 Vendor 代理快照、来源限额和同次去重，保留来源 ID 与引用。公网最终 URL 解析复用 [Fetch](../../backend/crates/stravia-web-access/src/fetch.rs) 的无正文重定向路径；直连校验全部 DNS 答案并固定连接，代理保留既有源站 DNS 信任语义。最终结果仍由完整报告 validator 执行公网 DNS 与允许域名校验。失败不忽略来源或发布原跳转 URL。
- 更新宿主、重建并导入独立插件后重新同步模型，再将搜索模型绑定到 Route 并在 Web Search 选择外部 Route。已有绑定与开关不自动变化。不新增公开工具、WIT、配置字段或数据库 schema；实际账号资格、地域、额度和第三方 OAuth 条款风险保持不变。

### 隔离验证

- `cargo test --locked -p stravia-vendor-antigravity --lib`：39 项通过。
- `cargo test --locked -p stravia-web-access --lib fetch::tests`：33 项通过。
- `cargo run --locked -p stravia-vendor-base --bin stravia-build-vendors -- --only=antigravity`：生成 1 个自包含 Wasm。
- `cargo test --locked -p stravia-vendor-antigravity --test contract search_discovery_grounding_and_errors_at_real_http_boundary -- --exact --ignored`：1 项通过，真实 Wasm 与 loopback HTTP 覆盖目录身份、两种上游工具、中文引用和去重、域名检索提示、未知用量及错误响应。
- `cargo test --locked -p stravia-core --lib web_search::host::grounding_tests`：5 项通过，覆盖精确跳转匹配、普通来源零请求、来源限额、最终域名检查和可见失败。
- `cargo test --locked -p stravia-core --lib gateway::vendor_auxiliary_tests::antigravity_search_tests::antigravity_discovered_search_route_through_stravia_read -- --exact`：1 项通过，从真实模型发现与 Route 绑定，经公开 `StraviaRead` 搜索返回 Turn-scoped 来源与引用；选择的模型不会被目录首项或普通推理模型替换。

上述 79 项检查未调用 Google；未运行完整仓库或桌面测试矩阵。

### 真实账号验证

按用户授权，只读复用当前运行实例的有效 access token、过期时间与已保存项目；不复制 refresh token、不重新授权、不修改原实例。一次性隔离 SQLite Gateway 通过 SSH 转发复用实例代理，加载本次构建的 Antigravity Wasm，执行真实模型发现、Route／External 配置及公开 `StraviaRead(stravia://search)` 查询。此前两次验收脚本在模型同步前因 Gateway 已关闭而失败，未发出搜索请求；修正脚本生命周期后通过。

- 问题：`Who maintains the Rust programming language? Search the official rust-lang.org website and cite the source.`
- 实际搜索模型：`gemini-3.5-flash-lite`。
- 结果：`completion=complete`，答案 787 UTF-8 字节，1 个已通过宿主公网来源校验的来源，答案包含有效 Turn-scoped 引用。
- 真实验收测试：1 项通过；临时凭据文件、验收脚本与 SSH 转发均已删除或关闭。

此结果仅证明该账号、本次模型目录与请求路径可用，不保证其他账号的资格、地区或额度；不能据此推断第三方 OAuth 获得官方支持。



