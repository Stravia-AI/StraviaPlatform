# Antigravity CLI OAuth 与模型协议调研

调研日期：2026-10-04。对应实现：`backend/crates/stravia-vendor-antigravity/`。

## 结论与证据边界

Stravia 使用独立 `antigravity` Vendor Plugin 提供 consumer Google OAuth、账号模型发现、Gemini 形态的模型推理及账号共享额度监控。默认构建仍只内嵌 `base`；管理员本地导入专属 Wasm 后才有此接入。

**公开 CLI 仓库不是 CLI 网络层的开源源码。** [Antigravity CLI 仓库固定提交](https://github.com/google-antigravity/antigravity-cli/tree/65a3c69e388148c9327f307efe82ddb1c0c8d7d4) 提供 README、CHANGELOG、examples 和 demo，未提供所需的 OAuth、HTTP client 或序列化实现。本次先分析开源代理与监控源码，再静态分析实际 CLI 1.2.16 发布包：恢复 Go 函数名、反汇编 OAuth/请求头/序列化函数，解码内嵌 protobuf `FileDescriptorProto`。没有执行下载的 CLI，也没有读取本机 Antigravity 凭据；实现后的真实账号授权与 API 验证使用隔离 Stravia 实例，见第 6 节。本文不能替代未公开的官方 CLI 源码或原生 CLI 授权抓包。

证据分层：

1. **CLI 二进制直接证据**：OAuth 地址、consumer scope、固定网站回调、PKCE、User-Agent 构造、HTTP 头设置、protojson 序列化及可拥有的 protobuf 字段。
2. **社区源码交叉证据**：daily Cloud Code 端点、请求包裹取值、项目接入路径、模型与额度响应实例。不能因多个代理都发送某字段就认定它属于 CLI。
3. **本地运行证据**：真实 Wasm 导出与 loopback HTTP 的协议边界，以及实际 Stravia Server 管理 API；不代表 Google 接受当前客户端身份或账号资格。
4. **真实账号运行证据**：经使用者明确授权，在默认 Edge 登录 Google，由隔离 Server 交换并保存凭据，执行账号模型发现、Flash 多轮推理及真实额度读取。结果只适用于测试账号、模型与验证时点，不证明第三方接入获得 Google 批准。

本实现只投影已核对的协议字段子集，不透传未知请求参数。没有模拟 CLI 的 TLS/HTTP 实现、JSON 键顺序或运行平台；不能声称逐字节复刻或保证绕过上游客户端检查。

## 1. 开源项目分析

| 项目与固定提交 | 许可证及分析范围 | 用于决策的结论 |
| --- | --- | --- |
| [Shunt](https://github.com/pleaseai/shunt/tree/be606d783032a17c46288ee5fc3a919e2b76a171)，0.52.0 | Cargo 声明 `MIT OR Apache-2.0`；`src/auth/antigravity/{auth,usage}.rs`、`src/model/antigravity_request.rs` | OAuth、project、daily 端点、较新的 `retrieveUserQuotaSummary` 及响应 fixture。其 `src/adapters/antigravity/stream.rs` 解码 CLI stdout，不是云端 SSE，不能用作云端帧格式证明。 |
| [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI/tree/8ef43e4df3b216a42493105d31c2873b69191473) | MIT；`internal/auth/antigravity/`、`internal/runtime/executor/antigravity_executor_*` | 对照授权码与刷新表单、包裹结构、错误传播；其额外头、模型特判与重试策略不直接移植。 |
| [Antigravity Python Proxy](https://github.com/usamashehab/antigravity-proxy/tree/9db90a538c2db0f3ecb4afc1b80b33c1092b33c2) | MIT；`antigravity_proxy.py` | 与其他代理的 OAuth body、User-Agent、额外头及 seed/penalty 转换存在差异；这些差异说明社区代理不能充当 CLI 白名单。 |
| [OpenCode Antigravity Auth](https://github.com/NoeFabris/opencode-antigravity-auth/tree/16e0056431d0a1291ee66e5938c732720b13a851) | MIT，已 archived；主要核对 `docs/ANTIGRAVITY_API_SPEC.md`，未取得的源文件不作为证据 | API spec 自称直接 API 实测，仍是第三方逆向文档，不是官方契约。 |
| [OpenUsage Windows](https://github.com/mesomya/openusage-windows/tree/971f4ba68870218a302191d2760abb552c274709) | MIT；`plugins/antigravity/plugin.js`、`docs/providers/antigravity.md` | 区分本地 Language Server RPC、IDE token 存储与云端额度。Stravia 不读取 IDE SQLite、keyring 或 token 文件。 |
| [Antigravity OAuth Proxy](https://github.com/dvcrn/antigravity-oauth-proxy/tree/2d9e3084ec39640a9b424166790db3480f5575b3) | 未识别到声明许可证；只分析 `internal/antigravity/{request,client,constants}.go`，不复制代码 | 独立对照 envelope 与 header 处理；其实现仍不是官方 CLI。 |

实现沿用 Stravia SDK、宿主网络与现有协议 codec，不执行或引入上述项目，也不复制没有已识别许可证的代码。

## 2. 实际 CLI 静态分析

分析包：[CLI 1.2.16 Linux x64 发布包](https://github.com/google-antigravity/antigravity-cli/releases/download/1.2.16/agy_cli_linux_x64.tar.gz)，[release 元数据 API](https://api.github.com/repos/google-antigravity/antigravity-cli/releases/tags/1.2.16)。下载包 SHA-256 与 GitHub asset digest 一致：

```text
d4247430e04cebdbe1ca93d9ccb483cd2f3daeb4cdb0ace5a71cd130e0bdab84
```

归档大小 61,494,901 字节，其中 ELF `antigravity` 为 209,625,296 字节。发布资产来自上述仓库，二进制的 changelog 引用同一仓库；没有额外取得供应商签名或独立供应链认证。分析方法是读取 ELF/Go PCL 表、恢复函数地址、使用 `llvm-objdump` 反汇编并解码原始 protobuf 描述符；不是执行 CLI 或反编译取得完整 Go 源码。临时二进制不随项目提交或分发。

核对的函数边界包括 `cli/backend/auth/auth.getOauthParams`、`(*oauthMethod).getOAuthConfig`、`getScopesByAuthMethod`、`(*oauthMethod).StartInteractive`、`SubmitAuthorizationCode`、`codeassistclient.setHeaders`、`codeassistclient.UserAgent` 与 `marshalCCPARequest`。最后者使用 protojson，通常为 camelCase JSON 字段；部分模型路径可使用数字 enum，不能由此推导任意 JSON 扩展都被允许。

核对的描述符包括：

- `google/internal/cloud/code/v1internal/{prediction_service,quota_summary,onboarding,model_configs}.proto`
- `google/cloud/aiplatform/master/{prediction_service,content,tool,openapi}.proto`

内部 Google GenerateContent proto 很宽。字段存在证明 CLI 类型能拥有该字段，不证明某条 CLI 调用必定使用它。Stravia 对生成控制采用更窄的子集；不把全部 Vertex 内部配置自动开放给下游。

## 3. OAuth 契约

| 项目 | CLI 1.2.16 consumer 配置 |
| --- | --- |
| authorize | `https://accounts.google.com/o/oauth2/auth` |
| token/refresh | `https://oauth2.googleapis.com/token` |
| redirect URI | `https://antigravity.google/oauth-callback` |
| grant | 授权码；PKCE `S256`；`access_type=offline`、`prompt=consent` |
| 交互 | Google 网站回调后复制授权码，粘贴到 Stravia；不监听本地端口 |
| token body | `application/x-www-form-urlencoded` |
| 交换字段 | `grant_type=authorization_code`、`client_id`、公开 native-client `client_secret`、`code`、`redirect_uri`、`code_verifier` |
| 刷新字段 | `grant_type=refresh_token`、`client_id`、公开 native-client `client_secret`、`refresh_token` |

consumer 使用七个 scope：

```text
https://www.googleapis.com/auth/cloud-platform
https://www.googleapis.com/auth/userinfo.email
https://www.googleapis.com/auth/userinfo.profile
https://www.googleapis.com/auth/cclog
https://www.googleapis.com/auth/experimentsandconfigs
https://www.googleapis.com/auth/aicode
openid
```

社区实现常见的五个 scope、`/o/oauth2/v2/auth` 和 `http://localhost:51121/oauth-callback` 不等于本版本 CLI 的原生配置。本实现采用二进制核对的 consumer application 参数，不提供 GCP/企业登录，不接受下游覆盖 OAuth 地址、scope 或回调。代码中的公开 native-client application 参数不是用户令牌，不应据此声称 Stravia 是 Google 官方客户端。

宿主允许 `AuthorizationCode + callback=None + manual_input.type=text`；这仍是 OAuth 授权码流程，不伪装成只保存静态 secret 的 `Manual`。SDK 拒绝没有 callback 且没有手动 text 输入的授权码描述符。Server 返回有效模式 `manual`、`listener_state=not_required`、无端口；现有 WebUI 根据描述符展示 secret 输入框，不增加 Antigravity 专属 UI 分支。

插件生成独立 state 和 verifier，忽略下游自报 state/redirect。回调 URL 输入必须匹配固定 redirect、state，拒绝重复参数和畸形编码；手动原始授权码依赖会话 PKCE。刷新不因响应省略 refresh token 而清空原 token，保留账号与 project 绑定；永久 OAuth 拒绝与 HTTP/network 临时错误分开。可选 `id_token` 的身份声明仅作显示/绑定，不参与授权判定，也不保存完整 JWT。

## 4. 项目、模型与额度

默认根地址：`https://daily-cloudcode-pa.googleapis.com`。不增加 production/cloudcode host fallback、随机 project、猜测的 tier、sleep 或自动重试。

| 操作 | 请求及响应边界 |
| --- | --- |
| 项目发现 | `POST /v1internal:loadCodeAssist`，`{"metadata":{"ideType":"ANTIGRAVITY"}}`；该 enum 在实际 ClientMetadata 描述符中存在。读取 `cloudaicompanionProject`。 |
| 项目接入 | 确实无 project 时，从 `allowedTiers` 选择唯一 `isDefault=true` 的真实 tier，`POST /v1internal:onboardUser`，仅发送 `tierId` 和上述 metadata。 |
| 异步 operation | 保存返回的 `operations/...`，后续真实发现请求执行一次 `GET /v1internal/{operation}`；未完成明确报 pending，不阻塞等待。拒绝含 URL、查询参数或路径穿越的 operation name。 |
| 模型发现 | `POST /v1internal:fetchAvailableModels`，仅 `{"project":"<account-project>"}`。模型 ID 来自 `models` 的 map key，不把 ModelDetails.model enum 当成 ID；过滤 disabled/internal。 |
| 额度 | `POST /v1internal:retrieveUserQuotaSummary`，仅 `{"project":"<account-project>"}`。同时解析 `groups[].buckets` 与顶层 `buckets`，按 bucketId 去重并保留分组标签。 |

异步接入时，授权码完成只能返回宿主接受的 Credentials，不能返回授权码流程不支持的 Pending。成功交换的 token 在会话私有状态中保留；项目发现失败后可继续该会话而不重复消耗授权码。项目状态限定在连接/凭据作用域，不跨账号使用。

模型元数据只声明上游实际给出的输入模态、上下文及 thinking 原始信息。不从一个数值 budget 猜测 low/medium/high 档位；不登记输出 token 上限来擅自限制请求。

额度字段核对为 `bucketId`、`displayName`、`description`、`window`、`remainingFraction`、`remainingAmount`、`disabled`、`resetTime`：

- `used_percent = (1 - remainingFraction) * 100`。省略 fraction 不是零余额；非法值不截断成成功。
- `remainingAmount` 为 proto int64，按十进制原值保留；未声明单位时标为 `unknown`，不猜 token、积分或货币。
- 已知 `5h` 和 `weekly` 可给出窗口秒数；未知窗口仍展示，不丢弃新账号池。
- disabled 不自动等于 exhausted；真实零剩余才可耗尽。重复 bucket 的互相冲突值拒绝合并为成功快照。
- resetTime 按 RFC 3339 解析。极小非零额度不会因展示舍入被标成耗尽。
- 空或只有 reset/window 的结果不伪造可用额度。

## 5. 请求头与请求体白名单

### 请求头

模型、项目与额度 POST 由插件固定构造：

```text
Authorization: Bearer <this connection's access token>
Content-Type: application/json
User-Agent: antigravity/1.2.16 (aidev_client; os_type=linux; arch=amd64; auth_method=consumer)
```

operation GET 不带 JSON Content-Type。二进制 `setHeaders` 没有默认注入社区代理常见的 `X-Goog-Api-Client` 或 `Client-Metadata`；本插件也不注入它们。不读取或合并 ProviderSnapshot.client_headers，不允许下游替换 Bearer、User-Agent 或增加供应商自定义头。Host、Content-Length、HTTP/2 framing 等传输层必需信息由宿主 HTTP 实现负责，不宣称是完整 CLI transport fingerprint。

### 外层与内层

推理统一请求 `POST /v1internal:streamGenerateContent?alt=sse`，非流式下游也由现有累积器收束。

外层只构造 `project`、`model`、`request`、`userAgent`、`requestType`、`requestId`。project 来自该账号，model 来自宿主 Target，requestId 随调用生成；不接受外部 envelope 覆盖。上述字段在实际外层 GenerateContentRequest 描述符中存在；`userAgent=antigravity`、`requestType=agent` 和 `agent-UUID` 取值参考固定社区源码，不等同于逐条原生抓包证明。

内层递归投影：

| 位置 | 保留的字段 |
| --- | --- |
| request | `contents`、`systemInstruction`、`generationConfig`、`tools`、`toolConfig`、`safetySettings`；另外只由宿主 session_affinity 生成 `sessionId` |
| Content | 普通消息 `role`、`parts`；systemInstruction 仅 `parts` |
| Part | `text`、`inlineData`、`fileData`、`functionCall`、`functionResponse`、`thought`、`thoughtSignature`、`executableCode`、`codeExecutionResult`、`videoMetadata`、`mediaResolution` |
| Blob / FileData | `mimeType`、`data` 或 `fileUri`、`displayName` |
| FunctionCall | `id`、`name`、`args` |
| FunctionResponse | `id`、`name`、`response`、`parts`；其 parts 只接受已核对的 inlineData/fileData 结构 |
| GenerationConfig | `temperature`、`topP`、`topK`、`candidateCount`、`maxOutputTokens`、`stopSequences`、`thinkingConfig`、`responseMimeType`、`responseSchema` |
| ThinkingConfig | `includeThoughts`、`thinkingBudget`、`thinkingLevel` |
| Tool | `functionDeclarations`、`googleSearch`、`googleSearchRetrieval`、`codeExecution`、`urlContext`；这些结构继续分别投影，不透传任意嵌套对象 |
| FunctionDeclaration | `name`、`description`、`parameters`、`response` |
| ToolConfig | 仅 `functionCallingConfig` 的 `mode`、`allowedFunctionNames` |
| SafetySetting | `category`、`threshold`、`method` |

Schema 依据实际 `openapi.proto` 投影 type/format/title/description/nullable/default、数组与对象约束、enum、properties/propertyOrdering/required、数值与字符串约束、example、oneOf/anyOf/allOf/not、additionalProperties/additionalPropertiesSchema、ref/defs。properties/defs 的业务属性名保留，其 schema 值递归投影；type 规范化为 proto enum 名称，类型数组转换为原生组合约束，不把数组发送到 enum 字段。标准 JSON Schema 的 `$ref`/`$defs` 转换为 `ref`/`defs`，本地引用前缀转换为 `#/defs/`；int64 长度/数量约束按 protojson 转为字符串。枚举词汇与引用格式另对照 [Google Schema 官方文档](https://docs.cloud.google.com/gemini-enterprise-agent-platform/reference/rest/v1/Schema)，该公开文档不等于私有 RPC 的实测证明。具体允许字段以 `wire.rs` 为实现来源。

CLI master 的 `thinkingLevel` 是 int32，公开 Gemini 的 `"HIGH"` 等符号值不直接转发，也不猜测其私有数字映射。

`args`、工具 `response`、schema `default`/`example` 是业务 JSON，不把其中名为 seed、metadata 或其他任意业务键当成协议参数删除。真实 thoughtSignature 原样保留，绝不生成伪造签名。

**删除边界**：cachedContent、labels、metadata、外部 project/session/request 身份、未知 Content/Part/Tool/Schema 字段，以及未纳入子集的 seed、presence/frequency penalty、logprobs、routingConfig、parallel-tool 控制等均不送上游。标准协议编码前先清除不支持的 canonical generation 控制和非 Google 协议扩展，编码后再投影 Google raw 扩展，防止同协议透传绕过白名单；不伪造原始协议标记来绕过共享 codec 校验。

可携带的工具定义、工具选择、stop 和响应格式由插件按 private master 边界构造，再统一投影。工具选择映射为 AUTO/NONE/ANY，具名选择附带 allowedFunctionNames；同协议原始工具配置保留后再清理。函数 `strict`、响应格式的 name/strict 不进入 CLI 请求，实际 schema 约束与业务 default/example 保留。此处理避免公开 Gemini codec 拒绝已经清除的外来控制，或删除私有 Schema 已有的字段；其他插件和共享 codec 校验不变。

### 响应

云端 SSE 外层常为 `{"response": <Gemini response>, ...}`；先解包 response，再交现有 Gemini decoder/accumulator。SSE 分帧处理 CRLF、多个 data 行、分片 UTF-8 和 EOF 尾部；单事件有界，额度/认证 JSON 同样有界。保留工具参数、使用量与真实签名。HTTP 错误及流内 error 保留公共上游错误语义；没有 finishReason 的截断不返回伪成功。[DONE] 单独出现也不能制造成功。

## 6. 本地及真实账号验证与限制

已运行 native 编译、SDK 授权码策略用例及插件解析用例；`task build:vendors:all` 生成含新插件的八个自包含组件。临时可执行入口实际加载发布 Wasm，通过 loopback HTTP 运行授权开始、错误 state 拒绝、手动交换、异步 operation、账号模型、共享额度、刷新、推理文本/工具/签名、字段清理、截断与 429 错误。跨 OpenAI 入口还验证外来控制清除、具名工具选择、schema 引用与可空类型、业务默认值和结构化输出约束；对应 opt-in 回归位于 `tests/contract.rs`，并纳入 `task test:all` 的本地 ignored allowlist。SDK 17 个、插件 11 个、真实 Wasm/HTTP 合约 1 个、Server OAuth callback 8 个用例通过；定向 Clippy 和仓库 Rust 格式检查通过。

实际独立 Server 的隔离 SQLite 实例通过管理 HTTP 导入插件、初始化授权码会话、读取 pending 状态及取消会话；Server 定向单元回归也通过。

后续真实验证使用默认 Microsoft Edge 中已登录的 Google 账号。使用者明确确认 Google 的原生应用来源警告后完成授权；固定网站回调发生连接重置，但地址栏已包含有效 `code` 与 `state`。验证脚本在内存中核对 state，将解码后的授权码通过管理 API 的 `manual` 输入提交，交换成功并保存连接。完整回调 URL 不是本流程的手动输入值。该次测试没有通过 WebUI 授权对话框提交授权码，也没有改变回调地址、产品 deadline 或上游校验。

真实模型同步成功，使用发现的 `gemini-3.5-flash-lite`，输出上限为 256 tokens。下列每种协议均通过三轮：记住随机标记与整数 41、第二轮复述、第三轮保留标记并计算 42；前两轮同步返回，第三轮 SSE 返回正确正文与成功终态。

|客户端协议|多轮方式|第三轮终态|
|---|---|---|
|OpenAI Chat Completions|完整消息历史，包括返回的 History Marker|`finish_reason=stop` 与 `[DONE]`|
|OpenAI Responses|`previous_response_id`，`store=true`|`response.completed`，`status=completed`|
|Anthropic Messages|完整消息与返回的 content blocks|`message_delta.stop_reason=end_turn` 与 `message_stop`|
|Gemini|完整 contents，原样保留返回的真实 thought signature|`finishReason=STOP`|

真实额度刷新 API 返回 `fresh`，Claude/GPT 与 Gemini 各有 5 小时、每周两个窗口。此次小输入调用后，上游仍报告四项 `used_percent=0`；不能据此推断调用不消耗额度。默认 Edge 的额度页成功刷新，并显示剩余 100%、已用 0% 与对应重置时间；两种显示模式均通过实际页面和可访问性验证。

真实调用暴露并修复两处共享 Gemini codec 问题：流 metadata 不再成为未知内容项，避免非流式跨协议转换返回 422；空且无签名的 thought 占位不再建项，避免实时序号错位导致 `hook_failed`。真实晚到签名、正文、用量与原生响应 metadata 保留。两个回归均先失败后通过，codec 全部 418 个单元测试通过；修复后的 Wasm 已重新导入隔离实例，四协议真实调用通过。

### 2026-10-05：真实函数工具闭环

沿用同一隔离实例、已授权账号与 `gemini-3.5-flash-lite`。每种协议执行两条独立路径：同步返回工具调用、SSE 返回回填后的回答；SSE 返回工具调用、同步返回回填后的回答。最终八个闭环、十六次真实上游请求全部 HTTP 200，调用参数均为 `add_integers(a=17, b=25)`。客户端校验参数后实际计算 42，并生成模型事先不知道的随机 receipt；回填 `{sum, receipt}` 后，模型返回的 `42|receipt` 与客户端结果逐字一致，不只验证模型自行算出 42。

|客户端协议|同步调用 → SSE 回填|SSE 调用 → 同步回填|工具调用 / 回填后终态|
|---|---|---|---|
|OpenAI Chat Completions|通过|通过|`tool_calls` / `stop`；SSE 有 `[DONE]`|
|OpenAI Responses|通过|通过|均 `completed`；SSE 有 `response.completed`|
|Anthropic Messages|通过|通过|`tool_use` / `end_turn`；SSE 有 `message_stop`|
|Gemini|通过|通过|均 `STOP`，包括 SSE 工具调用终态|

Responses 通过 `previous_response_id` 与原始 `call_id` 续接；其它协议回放完整历史与返回的工具 ID。Anthropic 保留返回的 History Marker，Gemini 原样回放真实 thought signature。测试的是客户端声明并执行的函数工具，不是 Stravia 内置平台工具，也不是通过提示词模拟工具。

受保护的真实 Wire Debug Capture 定位了以下根因，修复后重建 Server 与独立 Wasm，并在同一隔离实例重跑以上全部路径：

- History Marker 恢复拆开了 Anthropic ToolUse block 与同 ID 的 canonical ToolCall 镜像，导致 Google 收到两次同 ID 调用、仅一次结果，返回 HTTP 400。恢复时现在把镜像保留在同一 fragment，保留块顺序、cache control 与未配对调用。
- Gemini 原生 ToolResult 被当作媒体嵌套到另一层 functionResponse，业务 JSON 变成空 `result`。编码现在直接保留显式 ToolResult 的响应对象、ID 与调用名；普通文本和真正多模态工具输出的既有封装不变。
- Google `STOP` 在存在函数调用时需要规范为 `tool_calls`，供 Anthropic 输出 `tool_use`；原生 Gemini SSE 再映射回合法 `STOP`。文本终态与 `MAX_TOKENS` 不变。

Codec 422 个单元测试、Core History Marker 22 个回归及真实 Wasm/loopback HTTP 合约通过；定向 Clippy（`-D warnings`）与仓库 Rust 格式检查通过。工具终态回归先失败后通过，回放重复与 JSON 丢失先由真实请求和抓包复现。初次编译命令超时未完成的检查随后完整执行；没有调整产品 deadline、上游校验或失败重试策略。插件更新预览确认凭据继承、无数据丢弃、无新增网络权限；临时 Debug 已关闭。

验证没有覆盖其它账号、全部模型、真实图片、内置 Web Search/Media Understanding、MCP、额度耗尽、付费资格或长期刷新与续接。WebUI 源码无需供应商分支，本次未修改。初次本地验证遇到的启动 descriptor 超时没有通过缩短产品超时或盲目重试绕过；后续隔离 Server 已成功启动并完成上述真实路径。

## 7. 条款与运营风险

[Shunt 的 Antigravity provider 文档](https://shunt.sh/providers/antigravity/) 引述 Google 条款禁止第三方应用使用 Antigravity OAuth，并提示暂停/终止账号风险。官方 [Google Antigravity Terms](https://antigravity.google/terms) 及安装脚本在本次环境请求中连接重置，未取得原文；不能把该引述当成已独立核实的法律结论。发布与真实账号使用前，应由使用者核对当时适用的官方条款与组织授权。

插件提供明确风险文案。consumer scope 包含 cloud-platform，授权页面展示权限范围；不得把此接入描述成 Google 批准的第三方 API。私有 v1internal 接口、application 参数、CLI 版本和上游客户端识别可能改变；协议字段白名单与实际传输实现的差异不能消除条款或封号风险。
