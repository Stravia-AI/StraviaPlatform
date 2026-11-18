# Kiro CLI 请求对齐调研

调研日期：2026-11-18。下文分开记录官方 SDK 的合成请求、完整 KAS 的隔离执行、2.35.1 的线上成功以及 3.4.0 重新授权前后的线上请求。它们不能相互替代。

## 3.4.0 重新登录后的线上成功回合

使用官方 `kiro-cli acp --agent-engine=v3 --auth-method=cli`，通过 `session/new` 建立会话并选择 `qwen3-coder-next`。仅允许读取临时工作区中的一个测试文件。模型目录与 `GetUsageLimits` 均为 HTTP 200。服务实际返回 `usageBreakdownList`，其 `resourceType: "CREDIT"` 标为 `unit: "INVOCATIONS"`，同时具有整数和高精度 usage / limit。只记录字段和单位，不公布账户、余额或订阅身份。

同一主会话三次 `GenerateAssistantResponse` 均为 HTTP 200：第一步产生 `read_file`；工具完成后第二次请求携带匹配的 `toolUseId`、`status: "success"` 与 text block，模型答对文件中的测试串。第二次用户输入要求不读文件而复述；第三次请求将工具调用和结果都放入 history，模型没有再使用工具，正确复述。两次用户输入均以 `end_turn` 完成。脱敏 ID 比较确认：三次主请求保留同一 `conversationId = rootConversationId`；工具续接不换 `agentContinuationId`，新一轮才换。另观察到官方客户端的一次独立 `simple-task` 辅助请求（独立 conversation ID、共享 root）；不属于主模型的三次调用，插件不模拟此后台行为。

成功连接协商 TLS 1.3、未协商 ALPN；另有[七次公网 ClientHello 实测](kiro-cli-tls.md#已授权的公网-tls-验证)与插件使用的参考样本一致。Google 登录分支请求携带 `profileArn`，未携带 `TokenType: API_KEY`。这次自定义 ACP 客户端的推理头是 `x-amzn-kiro-client-attribution: unrecognized`、`x-amzn-kiro-mode-identifier: default-v2`、`x-amzn-kiro-fallback: 0;max=0`、`amz-sdk-request: attempt=1; max=3`、`x-amzn-kiro-attempt: 1;max=3`；目录 SDK 最大尝试数为 3，额度为 1。Stravia 只实现 API key 分支，以自己的双 UA 和 `stravia` attribution 发出一次请求，不声称自己是官方客户端。真实 Wasm 已通过本地 TLS 网关向 `/` 发送完整有序 HTTP/1.1 请求并返回文本；另经 Wasmtime 验证管理域名、额度请求体、小数与单位。上述线上 200 **不是**插件 API key 的线上验收。

## 3.4.0 重新登录前的线上请求

使用校验过的 Linux x86_64 `kiro-cli 3.4.0`、KAS `0.66.111` 和 Node `22.23.4`，在临时目录以 `chat --v3 --output-format stream-json --model qwen3-coder-next --trust-tools=fs_read` 要求读取单个测试文件。官方 launcher 负责读取登录状态；在其 Node HTTP 请求边界采集字段名、顺序、非敏感取值及响应状态。不保存 bearer、profile ARN 值、可重放 ID、系统提示、文件正文或 TLS 解密材料。TLSv1.3、无协商 ALPN 是此次公网连接观测；完整 [ClientHello](kiro-cli-tls.md) 从同版本隔离连接获取。

- 目录及额度分别向 `management.us-east-1.kiro.dev` 发送 `ListAvailableModels` 和 `GetUsageLimits`，均为 `POST /`；Google 登录分支发 bearer 和 `profileArn`，**没有** API-key 分支的 `TokenType: API_KEY`。两者均返回 HTTP 400，无可用余额。
- 推理向 `runtime.us-east-1.kiro.dev` 发送 `KiroRuntimeService.GenerateAssistantResponse`，body 顶层为 `conversationState`、`profileArn`、`agentMode`；`currentMessage` 中的模型为 `qwen3-coder-next`。官方 CLI 两次尝试均收到 HTTP 403 `AccessDeniedException`（`The bearer token included in the request is invalid.`）。因此未产生线上工具成功回执或第二轮答案。
- 该推理分支在请求 API 层观察到的顺序为 `content-type`、`x-amz-target`、`content-length`、`x-amzn-kiro-client-attribution`、`x-amzn-kiro-mode-identifier`、`x-amz-user-agent`、`user-agent`、`host`、`amz-sdk-invocation-id`、`amz-sdk-request`、`x-amzn-kiro-attempt`、`Authorization`。Node 在封包时还可加入连接头。双 UA 包含 CLI、KAS 与 ACP client 版本/身份；额度的 SDK 最大尝试数是 1，模型目录和此次推理是 3。不要把这一组登录头当作 API-key 分支的固定字段。
- 原生账号环境中，`chat --v3 --list-models` 报 `Not logged in`，官方 `acp --agent-engine=v3 --auth-method=cli` 也在 `session/new` 报未登录。有效浏览器会话应交由 CLI 自行复用；失效令牌不能靠复制登录库恢复，CLI 登录不能换算为 Stravia API key。按[官方认证方式](https://kiro.dev/docs/getting-started/authentication/#api-key-authentication-cli)重新授权或配置有资格账号的 API key，不从 CLI 缓存抽取令牌。

独立的 [工具与多轮捕获](kiro-cli-tool-conversation.md) 在隔离服务中完成了完整回合；[额度映射](kiro-cli-credits.md)及 UI 验证使用合成数值，不代表该账号余额。

## 2.35.1 官方 CLI 真实服务探针

主代理经用户授权在空目录执行 `kiro-cli chat --v3 --model qwen3-coder-next --trust-tools= --no-interactive`，一次短文本返回 `KIRO_OK.`；stdout/stderr 观察到 `[KRS] GenerateAssistantResponseCommand done totalEvents=10`。这证明该次 CLI 调用获得了结果，但不证明 Wasm 中的 API key 可用，也不能证明 3.4.0 的线上请求格式或其他账户的模型可用性。

## 结论

- 官方 [3.4.0 更新日志](https://kiro.dev/changelog/cli/3-4/)是 2026-11-16 发布；[3.0.0](https://kiro.dev/changelog/cli/3-0/)起符合条件的主机新交互会话及新 ACP 客户端默认 V3，已有 ACP 客户端按文档保留 V2。
- 3.4.0 V3 KAS 静态默认模型操作是 `GenerateAssistantResponse`。提取 SDK 对 loopback 的实际请求为 `POST /`、`X-Amz-Target: KiroRuntimeService.GenerateAssistantResponse`，而非 schema 注解的 `/generateAssistantResponse`。特性实验 `AB_CREATE_RESPONSE_API=create_response` 可切换主调用至 `CreateResponse`，不能断言所有环境都走默认分支。
- 模型目录使用独立 management 客户端；实测为 `POST /`、`X-Amz-Target: KiroControlPlaneBearerService.ListAvailableModels`，JSON body 含 origin 和可选 nextToken。模型 ID 应来自账户实际目录。官方[模型目录](https://kiro.dev/docs/models/)列出 Qwen3 Coder Next；上述 2.35.1 探针只证明这次调用可用，不代表其他账户或区域。
- CLI/ACP 是客户端到本地进程的协议；本节分析的是 KAS 到上游的私有协议，不是 ACP JSON-RPC。官方文档不承诺该上游协议稳定。

## 3.4.0 KAS 上游静态证据

### 区域与端点

生成客户端与路由函数分别解析 `management` 和 `runtime`：商业 AWS 分区按 `https://management.{region}.kiro.dev` 和 `https://runtime.{region}.kiro.dev` 拼接；无有效区域时回退 `us-east-1`。显式 region override 优先，其次 profile ARN 第四段区域、再其次 identity 的 region；显式 endpoint override 替代对应主机。GovCloud 也走 `*.{region}.kiro.dev`；隔离分区代码使用 `https://kiro-{management|runtime}.{region}.{dnsSuffix}`，不能一概硬编码商业主机。

API key 没有显式区域且未同时指定两个端点时，代码通过 management `GetProfile` 探测，候选依次为 `us-east-1`、`eu-central-1`、`us-gov-east-1`、`us-gov-west-1`；显式 region 不需探测。区域是账户/登录路由信息，不应从模型名字推测，也不应对未获授权的区域发探测请求。

### 认证、登录与头

KAS 两个客户端以 identity 的 `accessToken` 创建 Smithy bearer token；发起 API key 操作还注入 `TokenType: API_KEY`。静态映射还包括 `IdC → SSO_OIDC`、`external_idp → EXTERNAL_IDP`、`machine_token → KIRO_MACHINE_TOKEN`；只有 authMethod 已知才添加对应头。`profileArn` 是可选账户字段：API key 的 `EnvAuthProvider.resolveIdentity()` 明确返回空 profile ARN，已登录组织身份可以有 profile ARN。不要将某用户 profile ARN、请求 ID 或机器标识硬编码或复用。

[官方认证文档](https://kiro.dev/docs/getting-started/authentication/)描述最简单的受支持用户登录：`kiro-cli login`，选择 GitHub、Google 或 AWS Builder ID 并在浏览器完成授权；远程设备流显示 URL 和一次性代码，组织管理员可限制设备流。组织 IAM Identity Center 登录需要管理员给出的 Start URL 和身份目录 AWS Region；外部 IdP 使用工作邮箱或 Kiro Start URL。该流程由官方 CLI 处理，不意味着可照搬本地令牌或实现未经授权的身份伪装。自动化的官方替代方案是有资格账户在 app.kiro.dev 创建 API key，在非交互 CLI 设置 `KIRO_API_KEY`；企业可能需要管理员启用，API key 的许可范围和账户计划以官方文档为准。KAS 环境变量分支的行为不应等同于完整 CLI 的认证优先级，后者以官方文档为准。

尽管 `GenerateAssistantResponse` schema 把 `agentMode` 标注为 `x-amzn-kiro-agent-mode`，提取 SDK 实际将它和 `profileArn` 都序列化在 JSON body；没有观察到该 agent-mode header。响应 `x-amzn-kiro-conversation-id` 是服务端会话标识。SDK 重试中间件为一次调用创建 `amz-sdk-invocation-id`，重试时沿用该 ID，并更新 `amz-sdk-request: attempt=…; max=…`；响应的 `x-amzn-requestid` 等是服务端每次请求的诊断 ID，与会话 ID 不同。相关中间件也存在省略重试头的配置，不能称所有请求都必带这些头。代码还存在按条件设置的 opt-out、客户端归属和模式标识等头，不应把测试专用 execution/session headers 或客户端指纹当成通用认证要求。

### 模型目录

尽管 schema 将模型目录标为 `GET /List-Available-Models` 及 query 参数，提取 SDK 实测走 AWS JSON 协议，`POST /` body 包括必需 `origin`，以及可选 `maxResults`、`nextToken`、`profileArn`、`modelProvider`。KAS 实际调用传自身 origin、非空 profile ARN 和上页 nextToken；没有显式传 maxResults 或 modelProvider。响应为 `models`、可选 `defaultModel` 和 `nextToken`；它按页累加有 `modelId` 的模型，提取 `modelName`、描述、provider、token limits、状态以及可选 effort schema 等，并取第一个有效 default model。KAS 限制最多 10 页，触顶可能丢弃后续页；实现分页不能把第一页当完整目录。

静态追踪的 origin resolver 将客户端名 `kiro-cli`、`kiro-cli-non-interactive` 映射 `KIRO_CLI`，`kiro-ide`、`kiro-vscode` 映射 `AI_EDITOR`；未识别情况通常回退 `AI_EDITOR`，web 客户端和其他模式存在其他分支。目录调用取得 server origin；请求消息取得其配置的 getOrigin。下方独立 SDK 测试中的 origin 是手动传入，不能作为 CLI 默认值的动态证明。

### 生成请求与流

Runtime schema 将操作标注为 `POST /generateAssistantResponse`，但实际 SDK 序列化为 `POST /`；请求对象必需 `conversationState`，其 `currentMessage` 与 `chatTriggerType` 必需，另有 `conversationId`、`history`、`rootConversationId`、`agentContinuationId`、`agentTaskType` 等可选字段。KAS 构造器将最近消息作为 currentMessage、之前消息作为 history，并设 `chatTriggerType: "MANUAL"`。消息 union 是 `userInputMessage` 或 `assistantResponseMessage`；user 消息包含必需 `content` 和可选 `origin`、`modelId`、图像、文档及 `userInputMessageContext`（工具定义、结果等）。请求顶层另有可选 `profileArn`、`additionalModelRequestFields`、`systemPrompt` 和 `agentMode`。静态 KAS `buildRequestCommand` 传入这些字段；`system_field_injection` 实验默认 false，启用时才将首条 human 文本抽取为 systemPrompt。这里不是经过服务端验证的最小请求。

KAS 本地 canonical session ID 生成器产生 `sess_` 加 UUID；模型 conversation ID 取消息上下文，缺失时另生成 UUID，根 conversation ID 可回退该值。这些与每次 SDK invocation ID、服务端 request ID 作用域不同，不能将一个当成另一个。

输出是事件流，schema 列出 assistant response、reasoning、tool use/result、metadata、metering、context usage 和异常等事件；KAS 此操作专用中间件调用二进制事件流解析器，不能把 ACP 的 JSON Lines 当上游流。流失败、传输错误和服务端验证行为未通过真实请求验证。

## 提取 SDK 的 loopback 动态验证

使用 3.4.0 自带 Node，从 bundle 截取未修改的 SDK 和事件流中间件，追加独立驱动；临时 HOME、空工作目录、合成 API key、显式 loopback endpoint。此项实验使用进程网络 guard，阻止非 loopback socket、其他 DNS 和子进程；没有运行完整 ACP/KAS，请求参数均为手动合成输入。它不证明 CLI 默认参数或真实上游服务行为。

模型目录两次请求均为 `POST /`，`Content-Type: application/x-amz-json-1.0`、`X-Amz-Target: KiroControlPlaneBearerService.ListAvailableModels`、`TokenType: API_KEY` 及 bearer authorization；分页 token 放 body 而非 query。合成的脱敏交互如下（仅示意，非生产凭据或真实账号目录）：

```json
{"origin":"CLI"}
{"models":[{"modelId":"qwen3-coder-next","modelName":"Qwen test"}],"nextToken":"test-page"}
{"origin":"CLI","nextToken":"test-page"}
{"models":[{"modelId":"qwen3-coder-next","modelName":"Qwen test"}]}
```

生成请求同为 `POST /`，target 为 `KiroRuntimeService.GenerateAssistantResponse`；相同的 AWS JSON content-type、API_KEY token type 和 bearer authorization。SDK 添加 `amz-sdk-invocation-id`、`amz-sdk-request`、`user-agent` 与 `x-amz-user-agent`，其中用户代理含 SDK、运行环境和手工指定的 `KiroAgent 0.1.0`；不能据此推断完整 KAS 的默认 user agent。请求 body 脱敏示例（占位值非有效身份或会话）：

```json
{
  "conversationState": {
    "currentMessage": {"userInputMessage": {"content":"synthetic prompt","origin":"KIRO_CLI","modelId":"qwen3-coder-next"}},
    "chatTriggerType":"MANUAL",
    "conversationId":"<synthetic>"
  },
  "profileArn":"<synthetic>",
  "agentMode":"vibe",
  "additionalModelRequestFields":{"reasoning":{"effort":"low"}},
  "systemPrompt":"synthetic system"
}
```

stub 返回 `application/vnd.amazon.eventstream`，事件帧包含 `:message-type=event`、`:event-type=assistantResponseEvent`、`:content-type=application/json` 和 `{"content":"synthetic answer"}`；提取的官方中间件实际解码为 `{"assistantResponseEvent":{"content":"synthetic answer"}}`，同时读取 stub 提供的会话与请求响应头。该帧由本地 stub 合成，不是服务端输出；其他事件类型和真实网络分片均未动态验证。

## 完整 KAS ACP 到 loopback 的动态验证

直接运行 3.4.0 官方 KAS JS 和其自带 Node，以 ACP `initialize`（合成 clientInfo `kiro-cli`）、`session/new`、`session/set_config_option` 选择 stub 返回的 `qwen3-coder-next`、`session/prompt` 驱动；不是调用已提取的 SDK。使用临时 HOME、空工作目录和合成 API key，显式 `--endpoint` 与 `--control-plane-endpoint` 都指向本地 HTTP stub；分别设置 `--region=us-east-1`、`--region=eu-central-1` 均到达本地生成请求。进程及子进程位于 `unshare -Urn` 创建的独立用户及网络命名空间，仅启用 loopback；没有真实账号或非 loopback 出站。由于覆盖了 endpoint，这不能动态证明实际 US/EU 公网主机可用性。

两次均观察到：

- 模型目录请求为 `POST /`，target 为 `KiroControlPlaneBearerService.ListAvailableModels`，body 为 `{"origin":"KIRO_CLI"}`；该目录、模型名称及 ID 均由本地 stub 合成，不能据此声称真实账号可用。
- 生成请求为 `POST /`，target 为 `KiroRuntimeService.GenerateAssistantResponse`；`Content-Type: application/x-amz-json-1.0`、bearer 和 `TokenType: API_KEY`。请求还包含 `x-amzn-codewhisperer-optout: true`、`x-amzn-kiro-client-attribution: kiro-cli`、`x-amzn-kiro-mode-identifier: vibe`、`x-amzn-kiro-fallback: 0;max=0`、`x-amzn-kiro-attempt`、SDK invocation/retry 头，以及 SDK、KAS 版本和 ACP client 信息组成的 user-agent。这些是本次配置下观察值，不保证所有都是必需头。
- 请求 body 的顶层只有 `conversationState` 和 `agentMode: "vibe"`，无 `profileArn`、`systemPrompt` 或 `additionalModelRequestFields`。`conversationState` 包含 `chatTriggerType: "MANUAL"`、`agentTaskType: "vibe"`、`conversationId`、`rootConversationId`、`agentContinuationId`、`history` 和 `currentMessage`。实际测试中前两者等于 ACP 的 `sess_` 会话 ID；continuation ID 是独立值。当前 user 消息携带 `origin: "KIRO_CLI"`、`modelId: "qwen3-coder-next"`、提示文本及 `userInputMessageContext.tools`；history 包含 user 和 assistant 消息。这些字段和工具因配置、模式、上下文而变化。
- stub 返回空事件流，KAS 因空结果发起新的生成尝试；两次生成请求的 SDK invocation ID 不同，符合它们是不同调用而非同一 SDK 调用内部传输重试。主动结束会话，并未取得真实生成结果或验证上游认证。


## 公开 ACP 与未验证范围

[官方 V3 ACP 迁移指南](https://kiro.dev/docs/cli/v3/acp-migration/)描述客户端与本地 Kiro 进程之间按行 JSON-RPC 2.0 的通信，包括 `initialize`、`session/set_config_option` 和 `session/prompt`；这不是上游推理协议。[官方防火墙清单](https://kiro.dev/docs/privacy-and-security/firewalls/)是网络允许列表，不能单独证明请求路径或用途。

2.35.1 探针没有抓取线上请求；3.4.0 loopback 测试没有访问真实服务。真实服务要求的全部头、不同账号与区域的模型可用性、真实响应分片和错误行为，以及 Wasm 凭据与上游的兼容性，均未由这些实验确认。
