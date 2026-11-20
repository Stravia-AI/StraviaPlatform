# Antigravity CLI OAuth 与模型协议调研

初始调研日期：2026-10-04；工具附件实测更新：2026-11-06；CLI 1.5.8 与 Hydrogen 请求更新：2026-11-13；CLI 1.5.13 隔离抓包：2026-11-20。对应实现：`backend/crates/stravia-vendor-antigravity/`。

## 结论与证据边界

Stravia 使用独立 `antigravity` Vendor Plugin 提供 consumer Google OAuth、账号模型发现、Gemini 形态的模型推理及账号共享额度监控。默认构建仍只内嵌 `base`；管理员本地导入专属 Wasm 后才有此接入。

**公开 CLI 仓库不是 CLI 网络层的开源源码。** [Antigravity CLI 仓库固定提交](https://github.com/google-antigravity/antigravity-cli/tree/65a3c69e388148c9327f307efe82ddb1c0c8d7d4) 提供 README、CHANGELOG、examples 和 demo，未提供所需的 OAuth、HTTP client 或序列化实现。初次调研先分析开源代理与监控源码，再静态分析实际 CLI 1.2.16 发布包：恢复 Go 函数名、反汇编 OAuth/请求头/序列化函数，解码内嵌 protobuf `FileDescriptorProto`。初次静态调研没有执行下载的 CLI 或读取本机凭据；此后的经授权真实运行、官方 CLI 抓包、隔离 Stravia 验证和证据边界分别记录于第 6 节及[模型映射调研](antigravity-model-mapping.md)。本文不能替代未公开的官方 CLI 源码或证明未采集版本的行为。

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
| 模型发现 | `POST /v1internal:fetchAvailableModels`，仅 `{"project":"<account-project>"}`。按 Agent 分组引用解析 `models` 的 map key 与真实废弃重定向，排除 disabled，再按明确档位生成家族与 selector 表；不把 ModelDetails.model enum 当成 ID。 |
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

模型、项目与额度 POST 采用 2026-11-20 实际捕获的 agy 1.5.13 Linux/amd64 consumer 身份，由插件固定构造身份与内容类型，宿主协商响应压缩：

```text
Authorization: Bearer <this connection's access token>
Content-Type: application/json
User-Agent: antigravity/cli/1.5.13 (aidev_client; os_type=linux; arch=amd64; cl=1025641825; auth_method=consumer)
Accept-Encoding: gzip
```

`/cli/` 不是可省略的展示片段：同一账号、project、端点和请求体下，缺少它时上游仅返回 8 个 Agent 档位；补回后返回 14 个，包含 Gemini 3.7／3.8 Flash。早期最小差分验证表明仅补回 `/cli/` 即可恢复清单，见 [模型映射调研第 9 节](antigravity-model-mapping.md#9-官方有-3738-而-stravia-缺项的根因)。当前版本与构建号取自官方 1.5.13 实际请求，不从发布标签或其它平台的二进制推测；旧版样本仍按其原始版本保留。

这里的模型后端指 agy 实际访问的 `daily-cloudcode-pa.googleapis.com`，不是另一种客户端。本插件的模型后端操作选择 HTTP/1.1；请求体不压缩，响应只声明 gzip 压缩，未压缩响应也可读取。推理声明唯一 `Transfer-Encoding: chunked` 作为宿主传输选项，由宿主实际分帧，不把已编码 JSON 字节伪装成分块正文。模型目录、项目、额度和生图使用定长 JSON。当前宿主网络层不补 `Accept`，推理请求头顺序与同连接 ClientHello 由原生采集 fixture 校验；不是早期 reqwest 默认头路径。1.5.8 的 HTTP ClientHello 新增 `http/1.1` ALPN，内置 `antigravity` shape 升为 revision 2。JSON 排列、TCP 时序及未采集的平台不在一致性承诺内。

OAuth 使用独立的 `oauth2.googleapis.com/token` profile：`Content-Type: application/x-www-form-urlencoded`、`User-Agent: Go-http-client/2.0`、`Accept-Encoding: gzip`，由传输层计算正文长度，当前宿主不补 `Accept`。它不使用模型后端的 CLI UA 或 HTTP/1-only 选项；保持默认 HTTP 协商，历史官方刷新样本使用 HTTP/2。2026-11-13 没有重新验证授权交换或刷新；1.5.13 捕获中的 OAuth HTTP/1.1 与 `Go-http-client/1.1` 受代理强制 HTTP/1.1 限制，不能证明原生默认协议改变。管理员的代理和强制 HTTP/1 配置仍按现有契约生效，不承诺这种非捕获配置下的协议／UA 组合与官方逐字一致。交换和刷新继续使用原有表单认证方式，不新增 Basic 探测或自动重试。

operation GET 不带 JSON Content-Type。二进制 `setHeaders` 没有默认注入社区代理常见的 `X-Goog-Api-Client` 或 `Client-Metadata`；本插件也不注入它们。不读取或合并 ProviderSnapshot.client_headers，不允许下游替换 Bearer、User-Agent 或增加供应商自定义头。Host、Content-Length、HTTP/2 framing 等传输层必需信息由宿主 HTTP 实现负责，不宣称是完整 CLI transport fingerprint。

### 外层与内层

推理统一请求 `POST /v1internal:streamGenerateContent?alt=sse`，非流式下游也由现有累积器收束。

外层只构造 `project`、`model`、`request`、`userAgent`、`requestType`、`requestId`。project 来自该账号，model 来自宿主 Target，requestId 随调用生成；不接受外部 envelope 覆盖。上述字段在实际外层 GenerateContentRequest 描述符中存在，2026-11-06 官方 agy 1.5.1 抓包确认推理的 `userAgent=antigravity`、`requestType=agent`，生图则为 `requestType=image_gen`。2026-10-22 核对的[官方 CLI 1.3.5 Linux 产物](https://github.com/google-antigravity/antigravity-cli/releases/download/1.3.5/agy_cli_linux_x64.tar.gz)表明，无原生操作上下文时 `computeRequestID` 使用 `requestType/UUIDv4`；文本推理保留 `agent/UUIDv4` fallback，独立生图按新抓包采用 `image_gen/<毫秒时间戳>/<UUIDv4>/1`，不伪造 CLI 的共享操作树或计数。`getSessionID` 对完整 UTF-8 使用 FNV-1a64，再以 signed int64 十进制输出；插件只转换可信宿主 session affinity。上层输入结构的准确字段名及服务端校验尚未证实。算法、作用域、向量与本地验证边界见[会话与请求 ID 契约](../design/vendor-plugins.md#会话与请求-id)。

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
| FunctionDeclaration | `name`、`description`、`parameters`、`response`；canonical 参数的官方样本投影见下方 |
| ToolConfig | 仅 `functionCallingConfig` 的 `mode`、`allowedFunctionNames` |
| SafetySetting | `category`、`threshold`、`method` |

同协议原生 `response`、生成配置的 `responseSchema` 与函数 `parameters` 的具体允许字段以 `wire.rs` 为实现来源。canonical 函数参数按下述官方样本投影到 typed `parameters`，不将类型数组、组合或引用臆造为等价约束；官方样本显示这一转换有损。枚举词汇与引用格式另对照 [Google Schema 官方文档](https://docs.cloud.google.com/gemini-enterprise-agent-platform/reference/rest/v1/Schema)，该公开文档不等于私有 RPC 的实测证明。

CLI 1.5.11 的 master `tool.proto` 描述符也声明 `parametersJsonSchema` 为 `google.protobuf.Value`，与 typed `parameters` 互斥。它的存在及若干上游 HTTP 200 不能证明官方 eager MCP 使用此字段或完成了工具闭环；官方实测使用 typed `parameters`，并对输入 schema 做有损投影，如下所述。

**CLI 1.5.11：Schema 与 print 模式的证据边界**

- [Google 公开 FunctionDeclaration proto](https://github.com/googleapis/googleapis/blob/master/google/ai/generativelanguage/v1beta/content.proto) 将 `parameters` 定义为 OpenAPI typed `Schema`，`parameters_json_schema` 定义为描述参数对象的 `google.protobuf.Value`，文档要求两者互斥；这说明可表达的协议形状，**不说明 CLI 实际选用哪一个，也不保证私有接口或所有模型接受任意关键字**。同一 proto 对 `response` 与 `response_json_schema` 也描述互斥；不要混淆响应 schema 与工具参数 schema。
- [官方 CLI 1.5.11 Linux 二进制](https://github.com/google-antigravity/antigravity-cli/releases/tag/1.5.11) 本地 Go 符号字符串包含 `mcp.loadRefJSONSchema`、`mcpcore.isValidJSONSchema` 和 `mcpToolWrapper.ParameterSchema`，但符号本身不能证明出站转换。2026-11-19 对同一诊断 schema 分别使用 Claude Opus 5.5 High、Gemini 3.8 Flash High、Gemini 4 Hydrogen High，三者的 typed `parameters` 投影逐字段一致，并非按模型分支：样本保留 `type`（大写 enum）、`description`、`default`、字符串 `enum`、`properties`、`required`、`items`、`minItems`/`maxItems`、`minLength`/`maxLength`（protojson 字符串）、`minimum`/`maximum` 和 `pattern`；删除 `$schema`、`$defs`、`$ref`、`anyOf`、`oneOf`、`not`、type 数组、`nullable`、`title`、`additionalProperties`、`minProperties`/`maxProperties`、`propertyOrdering`、`uniqueItems`、`exclusiveMinimum`、`multipleOf`、`examples`/`example`、`const`。样本中的 `anyOf`、引用和 type 数组属性留下 `{}`，没有把 `NULL` 转为 `nullable`；不能据此推断未覆盖关键字和其他输入形状。CLI eager MCP 额外提供 `toolSummary` 和包装后的 MCP description，执行时移除 `toolSummary`；不能将其伪造为 Stravia 用户工具元数据。
- 官方 MCP `inputSchema` 是 JSON Schema，样本中的 `nullable: true` 只是非标准 annotation；CLI 删除它但保留 `type: STRING`。Gemini 原生 typed `parameters` 的 `nullable: true` 则具有真实可空语义，经 GoogleDecoder 先规范为 `type: [string, null]`，再按相同 CLI 子集投影得到 `{}`；不能为字节匹配前者而把 native 可空参数错误收紧成 `STRING`，也不依赖 source dialect 或逆推原 schema 的框架。raw-native 回归应按此合同预期，canonical OpenResponses 与 raw `parametersJsonSchema` 仍按官方 corpus 对照；生产投影无需该场景特判。
- 扩展样本第一次得到 Google HTTP 400，不能计作成功闭环：官方将布尔 schema 属性 `allowed: true`、`forbidden: false` 从 `properties` 移除，直接 `{type: "null"}` 输出 `type: "NULL"`，保留 `format`，`allOf` 留 `{}`。`integer` 的 `enum: [0, 1]` 被官方输出为 `enum: ["", ""]`，上游以 `INVALID_ARGUMENT` / `enum[i]: cannot be empty` 拒绝。插件遵循 typed enum 的字符串数组合同，对包含空字符串或非字符串的 enum 整条省略，不复制官方空字符串缺陷，也不将值字符串化；模型侧投影有损，执行侧参数校验不因此改变，原 schema 的约束被省略不等于参数禁止 `null`。先前 canonical 参数改用 Value 无损透传的方案已由此共同 typed 子集策略取代，先前的 HTTP 200 只保留作上游接受性对照。
- 同次用会话 `permissions.allow=["mcp(schema/nullable_probe)"]` 精确授权后，三模型均实际完成 MCP `tools/call`、`functionResponse` 和最终 `OK`，各两次推理 HTTP 200；服务端参数精确保留 `flag: null`、`ready: {port: null}` 及诊断 `task`。对官方投影仅做 typed enum 不可表达选项省略的 native-safe 对照，Flash 3.8 High、Hydrogen High、Claude Opus 5.5 High 也各有两次主推理 HTTP 200，实际 MCP 收到相同的 null 结构和各自 `[aligned-<model>]` task，最终均返回 `OK`。Hydrogen 工具流程还观察到一个同 `streamGenerateContent` 端点的 `gemini-3.5-flash-lite` 辅助调用，`thinkingLevel=MINIMAL`，无 tools/labels；未捕获 prompt，不能断言是权限审查或摘要，也不属于下述空闲启动窗口。先前把标准 schema 放进 `parametersJsonSchema` 的 HTTP 200 样本没有授权 MCP、没有发生 `tools/call`，不能称为该字段的工具闭环。基线 `maxOutputTokens` / thinking level 分别为 Flash `65536` / `HIGH`、Hydrogen `262144` / `HIGH`、Claude `128000` / `HIGH`；新会话采集的三个生命周期 flags，前两者均为 false，Claude 均为 true，不能因此全局固定。脱敏证据位于 `/tmp/stravia-gemini-schema.OK09BPos/evidence.jsonl`，结论仅覆盖该账号、版本和构造的样本。
- `agy --help` 的 `--print` 是非交互单轮，`--settings` 接受仅作用于当前会话的 JSON 对象或文件；[官方权限文档](https://antigravity.google/docs/permissions?tab=cli)规定 MCP 默认 Ask，支持按 `mcp(server/tool)` 精确授权，`deny > ask > allow`。例如在隔离会话使用 `--settings '{"permissions":{"allow":["mcp(example_server/example_tool)"]}}'`，以实际 MCP 注册的 server 名和 `tools/list` 返回的原始 tool 名替换示例，并确保没有更高优先级冲突规则；它不是对所有工具的授权。[官方项目问题 #650](https://github.com/google-antigravity/antigravity-cli/issues/650)记录 print/headless 无法提示时自动拒绝 MCP 权限，但未指明被拒工具；该问题是用户报告，不是本次运行的证明。无需默认启用会自动批准**全部**工具的 `--dangerously-skip-permissions`。
- **待抓包证实**：HTTP 200 只表明推理请求被接受，不等于模型产生 function call、更不等于 MCP server 收到 `tools/call`。未收到调用可能是未选择工具、名称匹配错误或 headless 权限拒绝；应同时记录声明名称、模型 function call、权限诊断、server `tools/call` 和结果回填，再判断闭环及原因。不得把其他模型或旧版客户端的成功外推到本次测试。

CLI 1.5.8 的 master 描述符声明 `thinkingLevel` enum：`THINKING_LEVEL_UNSPECIFIED=0`、`LOW=1`、`MEDIUM=2`、`HIGH=3`、`MINIMAL=4`、`EXTRA_HIGH=5`、`MAX=6`、`XHIGH=7`。已知数字按 protojson 写为符号，未知 int32 数值原样保留；不能将目录未知数值推测成某个已知符号。`thinkingBudget` 保留真实 int32 预算（`-1` 表示上游动态预算），不从 Effort 硬编码预算。

family selector 用 Target Effort 选择实际请求 ID 和该 variant metadata 的强度控制：存在 `thinkingLevel` 时校验并使用目录中的 int32 数字 enum，否则使用合法的 `thinkingBudget`。两者同时存在时优先 level；非法 level 不回退到 budget，请求只携带一种强度控制。显式 Effort 对应的两种控制均缺失或所选值无效时，在 HTTP 前返回错误；不从 `minThinkingBudget` 或 Effort 名称猜测预算。显式 Target Budget/Enabled/Disabled 仍优先于目录强度；无 Target 控制时，合法原始 level/budget 优先于缺省目录强度，默认请求 ID 仍取目录默认 selector。

CLI 1.3.2 历史记录中的 Gemini 3.8 Flash Medium/High 预算分别为 `4000`/`-1`；2026-10-28 核对的本地账号目录快照则只有 High/Medium/Low 的 `thinkingLevel=3/2/1` 和 `minThinkingBudget=32`，没有 `thinkingBudget`。历史预算不能作为所有档位必须带预算的契约。[公开 Gemini 3.8 迁移指南](https://ai.google.dev/gemini-api/docs/generate-content/whats-new-gemini-3.8)要求使用 level，[CLI 1.3.4 发布说明](https://github.com/google-antigravity/antigravity-cli/releases/tag/1.3.4)也记录了 ADC 路径同时发送 budget 与 level 的冲突修复；这些公开资料不等同于私有接口对旧预算的接受保证。

`includeThoughts` 是独立的摘要意图，仍由现有 codec 编码进入内层 `generationConfig.thinkingConfig`，显式关闭摘要得到 `false`。没有显式抑制时请求摘要，不保证上游一定返回可读思考；不能为获得正文改变模型、预算或强度。private descriptor 另有 `includeRawThoughts`、`enableThinking`，此次不新增控制 API、不默认打开，也不扩展现有投影范围。2026-11-13 的 1.5.8 Hydrogen 实际目录、四档推理及重新同步要求见[模型映射记录](antigravity-model-mapping.md#2026-11-13cli-158-hydrogen-目录与推理)。

`args`、工具 `response`、schema `default`/`example` 是业务 JSON，不把其中名为 seed、metadata 或其他任意业务键当成协议参数删除。真实 thoughtSignature 原样保留，绝不生成伪造签名。

`functionResponse.parts` 中的 inlineData/fileData 在 Gemini 解码时转成与业务 ToolResult 同属一个调用的 canonical 媒体块；编码时重新放回该 functionResponse，连续的并行工具结果合并到同一轮 content。上下文重写保持复合结果原子性，Observation 记录完整结果；转换为 Anthropic 或 Responses 时，业务 JSON 与支持的附件仍绑定同一个工具调用。URL 图片沿用现有 canonical 载体，只保证 URL 身份，不承诺保留其可选 MIME 提示；不为仅支持文本工具结果的 Chat Completions 增加多模态扩展。

**删除边界**：cachedContent、labels、metadata、外部 project/session/request 身份、未知 Content/Part/Tool/typed Schema 字段，以及未纳入子集的 seed、presence/frequency penalty、logprobs、routingConfig、parallel-tool 控制等均不送上游。标准协议编码前先清除不支持的 canonical generation 控制和非 Google 协议扩展，编码后再投影 Google raw 扩展，防止同协议透传绕过白名单；不伪造原始协议标记来绕过共享 codec 校验。

可携带的工具定义、工具选择、stop 和响应格式由插件按 private master 边界构造；canonical 工具 JSON Schema 按官方样本投影，并非原样保留。工具选择映射为 AUTO/NONE/ANY，具名选择附带 allowedFunctionNames；同协议原始工具配置保留后再清理。函数 `strict`、响应格式的 name/strict 不进入 CLI 请求。其他插件和共享 codec 校验不变。

### 2026-11-19：Hydrogen 503 对照

用户捕获的 `gemini-4-hydrogen-high` 返回 `MODEL_CAPACITY_EXHAUSTED`，但官方 CLI 可以正常调用。同次排查通过 CLI 1.5.11、同一登录环境的隔离代理分别调用 High 和 Medium，两者均返回 HTTP 200。随后保留失败捕获的内层 `request` 与模型，只使用 CLI 本次生成的外层 `requestId` 重放；CLI 的 project 与捕获一致，仍返回 HTTP 200。该结果不能证明 503 已修复，也不能区分当时的短暂容量与传输路径差异；不据此添加自动重试、降低 effort 或改写模型。此处验证的是 CLI 经隔离代理的上游调用，不是线上 Stravia 实例的实时调用。

### 响应

云端 SSE 外层常为 `{"response": <Gemini response>, ...}`；先解包 response，再交现有 Gemini decoder/accumulator。SSE 分帧处理 CRLF、多个 data 行、分片 UTF-8 和 EOF 尾部；单事件有界，额度/认证 JSON 同样有界。保留工具参数、使用量与真实签名。HTTP 错误及流内 error 保留公共上游错误语义；没有 finishReason 的截断不返回伪成功。[DONE] 单独出现也不能制造成功。

私有 usage 的缺省语义通过官方 [CLI 1.2.16 Linux x64 发布产物](https://github.com/google-antigravity/antigravity-cli/releases/download/1.2.16/agy_cli_linux_x64.tar.gz) 内嵌的 FileDescriptorProto 核对，而非由公开 Gemini 文档推断。该归档为 61,494,901 字节，SHA-256 为 `d4247430e04cebdbe1ca93d9ccb483cd2f3daeb4cdb0ace5a71cd130e0bdab84`。解压后二进制的 descriptor 字节区间：

- `google/internal/cloud/code/v1internal/prediction_service.proto`：offset 87,407,172，length 5,458。`PredictionService.StreamGenerateContent` 返回私有 `GenerateContentResponse`，其 `response` 字段引用 master `GenerateContentResponse`。
- `google/cloud/aiplatform/master/prediction_service.proto`：offset 90,287,381，length 41,913，语法为 proto3。`GenerateContentResponse.UsageMetadata.cached_content_token_count` 是编号 5 的 int32 标量，没有 `optional` presence 或 oneof。

依据 [Protobuf field presence](https://protobuf.dev/programming-guides/field_presence/)，上述已存在消息中的隐式 presence 标量省略等于零；缺失整个消息并不等于存在且全零。插件因此仅在已见终态、`usageMetadata` 为对象且报告有效 prompt 与 output/total 计数时，将省略的 `cachedContentTokenCount` 归一化为零。终态前的部分 usage、缺失 usage、无效计数与显式缓存值保持原样。共享 Gemini codec 不采用该私有规则；`thoughtsTokenCount` 独立保留，累计输出包含思考一次，后续累计修订覆盖已有快照。

## 6. 本地及真实账号验证与限制

### 2026-11-20：CLI 1.5.13 官方发行证据

当日核对，[GitHub 官方 latest release API](https://api.github.com/repos/google-antigravity/antigravity-cli/releases/latest) 的稳定版与 [Linux amd64 官方 updater manifest](https://antigravity-cli-auto-updater-974169037036.us-central1.run.app/manifests/linux_amd64.json) 的版本一致，均为 `1.5.13`；GitHub 发布于 2026-11-19 21:48:43 UTC，非预发布。[Linux x64 GitHub 发行包](https://github.com/google-antigravity/antigravity-cli/releases/download/1.5.13/agy_cli_linux_x64.tar.gz)的 asset SHA-256 为 `7fa9cc6df0d09562693d637c406a86c1b0f52ab3364714b1fd2d79cc6060d4b2`；manifest 指向另一个下载地址并提供 SHA-512，不能仅凭版本一致断言两个归档字节相同。[发行说明](https://github.com/google-antigravity/antigravity-cli/releases/tag/1.5.13)称在 ADC 登录和企业模型网关下新增 Gemini 4 Neon 模型支持，并修复自定义模型的 `modelFeatures=false` 被忽略等问题。这些是官方发布元数据与说明，不是网络抓包，也不是上游接受 Stravia 请求的证据。

### 2026-11-20：CLI 1.5.13 隔离推理抓包

在临时 HOME/XDG、空工作区、隔离 OAuth 状态副本、进程级 CA 与 loopback TLS 代理下执行 `agy --model gemini-3.8-flash-low --disable-slash-commands --print "Reply exactly with OK."`。启动阶段 OAuth、项目、模型目录和额度请求转发 Google 并返回 HTTP 200；推理请求在本地截断，代理返回合成的 OK SSE，CLI 退出码为 0。没有向 Google 发出真实推理请求，不能据此声称 Google 接受推理。原始 HOME 未写入；未保留凭据、账号或项目 ID、提示正文等私密值。代理强制 HTTP/1.1，OAuth 观察到的 `Go-http-client/1.1` 不证明原生默认协议改变；旁路辅助连接出现 TLS 错误与 broken pipes，不能声称全部流量成功。完整脱敏结构见 [CLI 1.5.13 fixture](../../backend/crates/stravia-vendor-antigravity/tests/cli-request-1.5.13.json)。

推理为 `POST /v1internal:streamGenerateContent?alt=sse`，HTTP/1.1 chunked，声明响应 gzip；头顺序为 `Host`、`User-Agent`、`Transfer-Encoding`、`Authorization`、`Content-Type`、`Accept-Encoding`。UA 为 `antigravity/cli/1.5.13 (aidev_client; os_type=linux; arch=amd64; cl=1025641825; auth_method=consumer)`。外层键为 `model`、`project`、`request`、`requestId`、`requestType`、`userAgent`；内层键为 `contents`、`generationConfig`、`labels`、`sessionId`、`systemInstruction`、`tools`。14 个工具组各有一个 function declaration；labels 键与 1.5.11 相同：`cascade_id`、`last_step_index`、`model_enum`、`request_id`、`root_cascade_id`、`trajectory_id`、`used_claude`、`used_claude_conservative`、`used_non_gemini_model`。请求 ID 为五段 `agent/<uuid>/<decimal>/<uuid>/<decimal>`，单次样本不证明多轮生命周期。

CLI 系统文本为 20557 UTF-8 字节，不保留正文；身份前缀为 296 字节，SHA-256 `acea3799fe9d52bc0fbabb261219491f2e707e87f12a337a6d1c81aa717bcd39`，与 1.5.11 相同。仅采集的 ClientHello 字段：legacy version、cipher suites、SNI 是否存在、EC point formats、supported groups、signature algorithms、ALPN、supported versions、key share groups、extensions，均与 1.5.8 样本相等；因此不修改宿主 shape revision，不将这部分字段相等外推为完整 TLS/TCP 等价。

### 2026-11-19：官方 CLI 1.5.11 空闲 TUI 启动抓包

使用 [官方 CLI 1.5.11](https://github.com/google-antigravity/antigravity-cli/releases/tag/1.5.11)，经授权在空临时工作目录启动交互 TUI（不是 `--print`），不输入提示，不手动触发工具、模型或额度操作。沿用用户登录态；用户 HOME 的写入用用户命名空间中的 tmpfs overlay 隔离，接受的仅是该空目录的信任提示，不改真实配置、认证或权限，未启用危险权限跳过。独立本地 mitmproxy 同时配置大小写 HTTP(S) 代理、localhost NO_PROXY 和临时 CA；不持久化头值、完整正文、响应、终端原文或凭据，JSON 只留键及长度。界面固定词脱敏证据：0.816 秒出现 `Yes, I trust … folder`，接受后出现 `Antigravity`、`for shortcuts`、`Gemini`、`Auto mode`、`shift … to cycle`；6.456 秒出现 `esc`。这证实信任提示后已绘制交互界面，但单靠这些词不能证明输入框状态或每项后台初始化均已完成。30.019 秒发首次 Ctrl-C、两秒后第二次 Ctrl-C，进程退出码 0；代理继续观察关闭过程，不将其计入空闲启动。界面原文、邮箱及路径均未保存。

下表按**请求发出时刻**（相对启动秒）汇总本次 30.019 秒窗口。每行的次数、状态与大小逐次对应；大小为 JSON 解压后字节数，非 JSON 为捕获的原始内容字节数，空体记 0。`<segment>` 表示采集时即删除的路径段，不代表确切路径；`—` 表示无 JSON 顶层键或空 query。表仅是实际经过代理的流量，不能证明不存在直连或绕过代理的流量；多次相同操作也不能据此判断重试或依赖关系。

| 方法、主机与脱敏路径 | 请求秒 / HTTP 状态 | 请求键与字节 | 响应顶层键与字节 | query key |
| --- | --- | --- | --- | --- |
| GET `www.googleapis.com/oauth2/v2/userinfo` | 1.256、5.389 / 200、200 | — / 0 | `email`, `family_name`, `given_name`, `id`, `name`, `picture`, `verified_email` / 277、277 | — |
| POST `daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist` | 2.588、4.663、5.390、6.967 / 全 200 | `metadata`（子键 `ideType`）/ 各 38 | `allowedTiers`, `cloudaicompanionProject`, `currentTier`, `gcpManaged`, `paidTier`, `upgradeSubscriptionUri` / 各 3869 | — |
| GET `lh3.googleusercontent.com/<segment>/<segment>` | 2.955、5.867 / 200、200 | — / 0 | 非 JSON / 各 6182 | — |
| POST `daily-cloudcode-pa.googleapis.com/v1internal:fetchAdminControls` | 3.338 / 200 | `project` / 30 | 空对象 / 3 | — |
| POST `daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels` | 3.638 / 200 | `project` / 30 | `agentModelSorts`, `audioTranscriptionModelIds`, `commandModelIds`, `commitMessageModelIds`, `defaultAgentModelId`, `experimentIds`, `imageGenerationModelIds`, `models`, `mqueryModelIds`, `sharedModelExperiments`, `tieredModelIds`, `webSearchModelIds` / 338645 | — |
| POST `daily-cloudcode-pa.googleapis.com/v1internal:fetchUserInfo` | 4.112、6.257 / 200、200 | `project` / 各 30 | `regionCode`, `userSettings` / 各 79 | — |
| POST `daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary` | 4.129、6.971、7.481 / 全 200 | `project` / 各 30 | `description`, `groups` / 2756、2755、2755 | — |
| POST `daily-cloudcode-pa.googleapis.com/v1internal:listExperiments` | 4.668 / 200 | `project` / 30 | `experimentIds`, `flags` / 65388 | — |
| GET `aicode.googleapis.com/<segment>` | 5.686、6.088、6.964 / 200、未见响应、200 | — / 0 | `agySurfaceConfig`, `availableExperiences`, `experimentConfig` / 80281、未知、80281 | — |
| GET `daily-cloudcode-pa.googleapis.com/v1internal/<segment>` | 6.170 / 200 | — / 0 | `cascadeNuxes` / 1570 | `page_size` |
| POST `play.googleapis.com/log` | 6.723、11.944 / 200、200 | 非 JSON / 7817、1032 | 非 JSON / 各 100 | — |
| GET `www.gstatic.com/<segment>/<segment>/<segment>` | 6.900、6.901 / 304、304 | — / 0 | 空体 / 0 | — |

空闲窗口内共 24 请求，23 次观察到响应；另一次 `aicode` 请求未观察到响应，不推断其结果。关闭过程另见 `play.googleapis.com/log` 两次（32.030、32.218 秒；分别 200、未见响应），不算启动。账号身份与 project/admin/user 信息、模型目录、自动额度、实验配置及日志上报均在不发提示时发生；gstatic 与 googleusercontent 只按实际主机记录，不能从域名推断其完整用途。日志端点可归为 telemetry，但仅凭空闲抓包不能判定这些请求是否启动所必需、用户可否关闭或全部 payload 语义。未见 OAuth 刷新、onboard、模型推理；已有有效登录与缓存、窗口长度和代理覆盖限制均会影响结果，不能推广为其他账号或启动必然不发生。官方 [CLI 文档](https://github.com/google-antigravity/antigravity-cli) 与 [权限文档](https://antigravity.google/docs/permissions?tab=cli) 为客户端与权限背景，不是这些私有请求的公开契约。

同日再做独立的定向 TUI 采集，修正上表为避免泄露而过度遮盖路径的证据缺口：两次 `GET aicode.googleapis.com/v1alpha:fetchConfig` 在 4.747、4.877 秒发出，第一条 5.180 秒 HTTP 200，另一条未见响应；5.821 秒第三条同路径 GET，5.971 秒 HTTP 200。`GET daily-cloudcode-pa.googleapis.com/v1internal/cascadeNuxes?page_size=<redacted>` 在 5.211 秒发出，5.547 秒 HTTP 200；只保留 query key。`www.gstatic.com/antigravity/…` 两个资源 GET 分别于 5.540、5.541 秒发出，均 304；CLI 二进制固定 URL 与后续定向抓包确认两条路径为 `/antigravity/plugins/marketplace.json` 和 `/antigravity/plugins-partners/marketplace.json`（分别于 5.870、5.868 秒发出，均 304）。[官方 Marketplace 文档](https://antigravity.google/docs/marketplace)说明 CLI 插件市场功能，但不规定该内部 URL。这是独立运行的路径实测，不能反向假定前次被遮盖的每条路径都相同。

定向采集在同样的 tmpfs HOME overlay 与空工作目录中运行，0.753 秒接受隔离目录信任提示后，不输入提示；25.079 秒用 ANSI 终端模拟器得到以下**实际屏幕行**，用户名、URL、工作区、模型具体名称及未列入固定词表的内容用 `█` 遮盖；可见光标位于 `>` 后、该行第 3 列。该快照显示交互输入行及快捷键，支持 TUI 已就绪；不说明后台所有网络任务均结束。双 Ctrl-C 正常退出码 0。

```text
 ┃ esc to █
────────────────────────────────────────────────────────────────────
> Auto mode: █ & █ █ █ (shift+tab to cycle)
────────────────────────────────────────────────────────────────────
? for shortcuts                           auto · Gemini █ █ · █
```

### 2026-11-06：agy 1.5.1 请求头与工具附件

先对官方 agy、OMP 与 Stravia 的附件、多轮和工具循环进行实际对比，确认原生 `functionResponse.parts` 在共享 Gemini 解码时被丢弃，而不是 Antigravity 的出站白名单删除。修复后使用已授权的 agy 登录凭据，以相同 `gemini-3.8-flash-medium` 加载重新构建的真实 Wasm：PDF 工具结果返回 `PDFCHECK 7319`，PNG 工具结果识别为 `Red`，PDF 后续轮正确返回 `7320`，均未再次请求读文件。捕获的 6 次请求（模型发现及 5 次推理）全部 HTTP 200，UA 与 gzip 协商一致，PDF/PNG 附件字节摘要与输入相同；模型目录实际返回 gzip，SSE 未压缩。

上述第一轮真实调用覆盖 native codec → Wasm → Google 上游；隔离捕获桥负责 HTTP 与 gzip 解压，不等同于独立 Server 管理登录、HTTP 入口或 UI 的端到端验证。没有修改原登录文件，保留的证据不包含凭据、项目 ID、签名正文或附件正文。该轮未修改 requestId、labels、工具结果 role 和默认输出上限，不能据此宣称全部请求体或底层传输指纹与官方逐字一致。

### 2026-11-06：原始分帧与原生生图

官方 agy 1.5.1 的推理请求使用 HTTP/1.1 chunked；目录查询及原生 `generate_image` 使用 Content-Length。生图抓到三次 `POST /v1internal:generateContent`：前两次 HTTP 200 但仅有失败 `finishMessage`，第三次返回 gzip JSON、STOP 和 JPEG 图片。参考图编辑另抓到提示文本在前、`inlineData` 在后，默认 `aspectRatio: "1:1"`；该次真实响应为 429 RESOURCE_EXHAUSTED，提示约五小时后恢复，因此未重试。生图没有额外系统提示或 `responseModalities`，不是普通推理的 SSE 分支。

修改后重新构建独立插件与 Server，在全新 SQLite 实例执行管理登录、原生 Gemini HTTP 入口和 MCP。Provider/OAuth 记录使用隔离测试种子；真实 agy 凭据只由 loopback 桥在转发 Google 时读取，不写入 Server 测试库。五次真实上游推理全部成功：PDF 返回 `PDFCHECK 7319`、PNG 返回 `Red`、第三轮返回 `7320`，附件没有引发再次读文件。原始 HTTP 接收器确认每次都是 HTTP/1.1、chunked、无 Content-Length，并验证十六进制块长、CRLF、终止块和重组后的附件摘要。宿主回归还覆盖空正文、压缩后分帧、保留正文的 307 与清空正文的 303，以及歧义 framing 拒绝。

图片配额已耗尽，不能把修复后的真实上游生图成功记为通过。成功路径改用官方成功响应结构与摘要相同的图片字节回放（省略未消费的 thoughtSignature），实际经过 Wasm、宿主 gzip 解压、MCP `generate`、Core 图片校验、Artifact 收存和 HTTP 下载，得到字节一致的 1024×1024、340003 字节 JPEG。参考图顺序、所选模型、策略拒绝、失败诊断、缺失终态和 429 由真实 Wasm／本地 HTTP 合约覆盖；不冒充真实编辑结果。

保留证据位于本次工作区 `target/antigravity-compare-20261106-8rozh0ek/` 的 `framing-image-verification.json`、`native-image-captures/` 和 `server-wire-captures/`；鉴权、项目、签名及媒体正文均脱敏。原始分帧来自 Server→loopback 桥，不是 Server 直连 Google 的 TLS 捕获。未验证桌面壳/UI、HTTP/2 或 TLS 指纹完全一致；生图显式分辨率选项也未获本次官方样本验证。

### 2026-11-06：默认请求头与 OAuth profile 分离

以下是已撤销的默认头抑制补丁的历史验证，不代表当前 `Accept` 行为。当前实现恢复官方 reqwest 并接受额外的 `Accept: */*`，保留 HTTP 协商、OAuth UA、gzip 和请求体修复。

隔离复制 agy 登录文件并只修改副本的 expiry，执行官方 `agy models`，实际刷新返回 HTTP 200：`oauth2.googleapis.com/token` 使用 HTTP/2、`Go-http-client/2.0`、表单 Content-Type、准确的 Content-Length 和 gzip，既没有 Accept，也没有 Authorization；认证参数在表单中。同次 `loadCodeAssist`、`fetchAvailableModels`、`retrieveUserQuotaSummary` 均为 HTTP/1.1、CLI UA、定长 JSON、Bearer 和 gzip，无 Accept。

精确 1.5.1 二进制的 Go 函数／行号映射确认授权码交换与刷新共用 OAuth HTTP 构造路径，但也存在 Basic auth-style 探测分支。没有重新登录进行授权码交换抓包，未向 Google 发送伪造授权码，因此不能把刷新请求的认证形式当成授权码交换的实测结果。本次保留既有表单认证，不引入未经验证的探测或重试。

reqwest 在实际发送前补入默认 `Accept: */*`，仅修改插件 header 列表无法移除。当时为消除差异使用了逐请求补丁和显式宿主选项，保留原 TLS 验证与代理实现。在新建 Server／SQLite 中导入重建的 Wasm，用临时 CA 仅配置该 Server 进程，经本机 TLS 捕获代理转发真实 Google；原始 agy 登录文件未改，凭据副本仅存于随后删除的私有测试目录。

13 次真实 Google 请求全部 HTTP 200：1 次刷新、2 次项目读取、3 次模型清单、1 次额度、6 次推理。检查了完整 header 名称集合及非凭据值，忽略名称大小写和顺序，动态正文长度按实际字节数核对；无额外 Accept。OAuth 实际协商 HTTP/2，其余使用 HTTP/1.1；推理为 chunked、无 Content-Length，普通 JSON 使用准确长度。PDF、PNG 工具结果与第三轮分别得到 `PDFCHECK 7319`、`Red`、`7320`，媒体摘要不变且未重复读文件。

图片仍不作新的真实生成调用。最终的 gzip JSON 回放经过实际 MCP、Wasm、宿主、Artifact 与下载，得到与官方样本字节一致的 1024×1024 JPEG。回放夹具最初重复 gzip 编码，产生的六条 HTTP 200 错误正文均在报告中标记，不计为生成成功；修正的是临时夹具，不是产品错误处理或重试策略。新测试 Key 最初未开启 MCP，按既有管理 API 显式开启后再执行该路径。

本轮通过 Core 网络 35 项、SDK／Runtime／Antigravity 库 89 项、真实 Antigravity Component 合约 4 项及 Runtime Component 9 项，仓库 Rust 格式检查、相关 crate 全 target Clippy 和 Server／插件构建通过。移除 Cargo update 顺带产生的无关传递依赖重选后，锁定模式再次通过上述 137 项、Server 构建与 Clippy；重启最终构建的 Server，真实请求再次返回 `AGY_HEADERS_VERIFIED`，其冷启动模型查询与推理头也已核对。脱敏记录为 `target/antigravity-compare-20261106-8rozh0ek/header-verification.json`、`header-captures/` 与 `official-oauth/`。这不覆盖授权码交换的实际认证头、管理员强制 HTTP/1 下的 OAuth UA 回退、TLS 指纹、头顺序或 UI／桌面壳。

### 2026-11-07：撤销 reqwest 默认头补丁

恢复 registry 发布的 reqwest 0.13.5，删除默认头禁用字段、依赖补丁、准备脚本及 Task／CI／Docker／Nix／开发启动器接线；接受宿主自动添加 `Accept: */*`，不替换 TLS 实现。原有 HTTP/1、gzip、chunked、附件续接和原生生图修复保留。

锁定构建通过 Core 网络 35 项、SDK／Runtime／Antigravity 库 89 项、Runtime Component 9 项和 Antigravity Component 4 项，共 137 项。Server、10 个独立插件与 21 个测试组件重建成功；Rust 格式、相关 crate Clippy、工作流 actionlint、开发启动器语法检查和 Nix 求值通过。重建 Server 在全新 SQLite 中导入当前 Wasm，经 loopback 原始 HTTP 接收器实际完成模型查询和三次推理调用：均带 `Accept: */*` 与 gzip 协商，模型查询定长、推理为真实 chunked；压缩响应可解码，多轮历史与工具 PDF 字节保持完整。脱敏结果在 `target/reqwest-removal-verification.json`。此轮使用本地合成上游，不调用 Google；不覆盖真实 OAuth、图片生成、TLS 指纹或完整 Docker／Nix 构建。

### 早期接入验证

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

### 2026-10-06：Gemini 3.8 Flash 摘要意图与严格续接

在独立临时 SQLite 实例中，仅复制已授权连接的当前访问凭据和指定模型记录，导入重建的 Antigravity 组件；不复制刷新凭据，不修改日常数据库、客户端配置或模型强度。Target 重试预算为零、无 failover；loopback 转发门每次仅放行一次推理，硬限制最多三次外发。

实际使用两次推理：Responses SSE 返回只读 `read_local_receipt` 工具调用，客户端回填新生成的合成 receipt 后，非流式请求原样回放完整交付历史，不提供 `previous_response_id`。两轮均 completed，最终回答逐字使用工具 receipt；第二轮严格关联第一轮 Generation 节点，普通 Observation 只有一次输入预览。捕获中仅有两次 upstream request，没有恢复重放。

两次实际选择器均为既有 `gemini-3.8-flash-high`，内层均携带 `thinkingConfig: {"includeThoughts": true}`，没有重复强度字段。上游仍只返回真实受保护签名，没有可读 thought 文本；这不构成失败，也没有为取得正文而额外调用。两轮 prompt 合计 224、output 131、其中 reasoning 105；终态 usage 均省略缓存字段，按已核实的私有标量语义得到 cache read 0、净输入 224。临时实例与凭据文件已清理。

## 7. 条款与运营风险

[Shunt 的 Antigravity provider 文档](https://shunt.sh/providers/antigravity/) 引述 Google 条款禁止第三方应用使用 Antigravity OAuth，并提示暂停/终止账号风险。官方 [Google Antigravity Terms](https://antigravity.google/terms) 及安装脚本在本次环境请求中连接重置，未取得原文；不能把该引述当成已独立核实的法律结论。发布与真实账号使用前，应由使用者核对当时适用的官方条款与组织授权。

插件提供明确风险文案。consumer scope 包含 cloud-platform，授权页面展示权限范围；不得把此接入描述成 Google 批准的第三方 API。私有 v1internal 接口、application 参数、CLI 版本和上游客户端识别可能改变；协议字段白名单与实际传输实现的差异不能消除条款或封号风险。
