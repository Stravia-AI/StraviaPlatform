---
status: accepted
---

# 跨 Anthropic 与 Responses 承载原生压缩

Remote Compaction Request 在 Open Responses（`POST /v1/responses/compact`）与 Anthropic Messages（`compaction` 请求参数、`compaction` 内容块）上各有原生机制。两个入口都按协议原生方式接收，由当前 Target 的上游执行，结果再翻译回客户端协议。不引入本地摘要、不为完成压缩切换 Target。上游覆盖范围见 `docs/research/upstream-remote-compaction.md`：目前只有 Anthropic、OpenAI 与 xAI（仅独立压缩）提供原生机制，Gemini 与其余兼容上游没有。

## 契约与取舍

- **入口**：Anthropic `/v1/messages` 带非空 `compaction` 对象即进入独立压缩（`ModelTurnPurpose::Compact`），与 `/v1/responses/compact` 共用 Model Turn 路径，不创建 Generation，不跑 Agent 循环。`compaction: null` 视为未请求。请求参数与 `stop_sequences`、`tool_choice`、`output_config.format`、`context_management` 互斥，向 Anthropic 上游发送前剥离，且强制 unary；`stream: true` 的 Anthropic 客户端由入口把整块结果按 Anthropic 的方式一次性发成 SSE。
- **出口**：Anthropic 上游经 `anthropic-beta: compact-2026-09-04` 调用，与已有 beta 合并；Responses 上游走 `/v1/responses/compact`（Codex 例外，见下）。结果按 `NativeCompactionResponse.wire` 的实际形态自识别，入口协议相同则字节转发，不同则翻译；翻译只依赖上游响应体，不依赖请求侧状态。
- **无状态信封**：另一协议的原生结果封进本协议的不透明字段：Anthropic 的 `compaction` 块整块封进 Responses `encrypted_content`，Responses 的替换窗口（保留的用户消息加 compaction 条目）整体封进 Anthropic 块的 `signature`，`content` 用固定占位文本。信封是带版本前缀的 base64url JSON；原生签名或密文不会以该前缀开头，所以解码时能无歧义区分。解码总是还原成原生 IR，编码在目标协议不同才封装。不新增持久化表。
- **IR**：`ContentBlock::Compaction` 增加可选 `summary`。`summary` 存在即 Anthropic 来源：`encrypted_content` 存签名，`summary` 存可读 `content`，二者都进入原生状态指纹。条目 ID 取自签名的哈希（`cmp_<hash>`），登记时（上游解码）与回放时（客户端回传）得到同一指纹，既有 Compaction 登记和解析不需要改动。
- **回放**：Anthropic 目标且 Target 绑定相同，回放原生块（逐字节，API 会校验签名与内容，同一请求只能有一个块，且必须是 `messages` 的首块，同时带上 beta 头）。其他情形，即出口协议不能承载 Anthropic 块，或解析出的 Target 绑定与当前不同，且所有待回放状态都是 Anthropic 来源，则把摘要降级为一条普通用户文本，以中性英文说明其为被压缩历史的摘要。含有无摘要的 Responses 原生状态时保持原行为：省略（跨协议）或 `compaction_target_mismatch`（跨 Target）。
- **与 ADR-0047 的关系**：ADR-0047 规定原生状态绑定 Target 且不适用历史降级。该规则对不透明的 Responses 状态不变。Anthropic 状态是特例：上游返回的 `content` 本来就是可读摘要，降级保留的是上游生成的历史记录，不由 Stravia 生成。这是有意放宽，而不是放宽其校验：来源、Principal、有效期仍按登记校验，降级只在已通过校验或没有登记的状态上发生。
- **凭据保护**：Anthropic 压缩块视作不透明载荷，不还原其中的占位符，否则会破坏签名。回放给上游的是原样占位符，不泄露凭据。
- **客户端配置**：`connect_client_apply` 为 omp 的 Stravia provider 写入 `remoteCompaction: { enabled: true }`。omp 对自定义 provider 默认不启用远端压缩，必须显式开启才会发 `/responses/compact`。该设置是 provider 级常开：路由到不支持压缩的上游（Gemini、DeepSeek 等）时压缩请求会返回上游错误；omp 在 `compaction.methodOrder` 含后续方法（例如 `soft`）时会据此回退到本地摘要，只配置 `remote` 则直接报错。
- **范围**：本决定只覆盖独立压缩（omp V1 路径）与回放。跨协议的 inline 压缩控制与触发条目仍按 ADR-0006 省略。Anthropic 基础 Vendor、声明为 Anthropic 协议的 custom 通道与 Claude Code OAuth 插件提供 Compact；目录里其他 Anthropic 兼容 Vendor、Bedrock、Vertex 暂不提供，待上游确认后另议。
- **Codex 插件**：ChatGPT-Codex 后端不再提供 `/responses/compact`（实测 404，`openai/codex` #38323、#38706 同），当前官方客户端用 V2：普通流式 Responses 请求末尾追加 `compaction_trigger`。Codex 插件的 Compact 因此是该 Target 自己的原生独立压缩端点，而不是跨 Target 重试或本地摘要：请求走与推理相同的 `/responses` 流水线（`store=false`、`stream=true`、同一套头与字段白名单），末尾追加 trigger；上游流里只取恰好一个 `compaction` 条目（数量不为一视为错误），按官方客户端的规则拼出替换窗口，即保留的用户文本消息（最新优先，约 64000 估算 token，丢弃图片与音频，边界消息截断）加该条目，再包成 `response.compaction` 返回。上游的非 2xx 响应原样透传。旧的 `/responses/compact` 路径与字段白名单已删除，不保留回退。官方客户端附带的 `x-codex-turn-metadata` 压缩标注未复刻；用同一 ChatGPT OAuth 账号实测，请求被接受，窗口回放后模型能复述压缩前的细节。独立 Codex 插件需重新构建并导入。
- **Claude Code 插件**：Claude Code 的请求字段集是封闭的（`CLAUDE_CODE_FIELDS`），压缩控制 `compaction` 在收窄前取出、收窄后写回，且只在 unary 压缩请求里存在；推理请求即使带该字段也会被丢弃。压缩 beta 由请求体内容推导：只要请求带 `compaction` 控制，或回放的 `messages` 里有 `compaction` 块，就声明 `compact-2026-09-04`，否则上游以 `Input tag 'compaction' ... does not match any of the expected tags` 拒绝回放。缓存断点策略把 `compaction` 视为不可标注块，避免附加 `cache_control` 破坏逐字节回放。真实上游已验证 Claude Code 订阅账号可以执行压缩并接受回放。

不采用本地摘要回退、把摘要塞进 Responses `instructions`，或维护跨协议转换表。它们分别违背 Remote Compaction Request 的约定、混淆指令与历史，并引入需要持久化与回收的新状态。
