# OpenCode OpenAI-compatible thinking/reasoning 适配契约

> 调研快照：OpenCode `anomalyco/opencode` `dev` commit
> [`10765ff2a9da8c3b88e4de873aa383a49c318912`](https://github.com/anomalyco/opencode/commit/10765ff2a9da8c3b88e4de873aa383a49c318912)，提交日期 2026-08-30；`packages/opencode/package.json` 版本为 `1.18.25`。AI SDK 锁定为 `@ai-sdk/openai-compatible@2.0.41`（`@ai-sdk/provider@3.0.8`、`@ai-sdk/provider-utils@4.0.23`）。
>
> 本文只描述一手源码事实；供应商官方能力不等于 OpenCode 已实现能力。

## 1. 结论摘要

* `--thinking` 不是请求开关：`packages/opencode/src/cli/cmd/run.ts` 的 yargs 选项描述为 “show thinking blocks”，它只控制 TUI/CLI 是否显示 reasoning 块。真正选择请求变体的是 `--variant`、会话 `user.model.variant` 或配置的 `model.variants`。
* 完整链路是：`run --variant`/UI → `Session.PromptInput.variant` → `session/llm/request.ts:LLMRequestPrep.prepare` 读取 `input.model.variants[input.user.model.variant]` → 与 `ProviderTransform.options()`、model options、agent options 深合并 → `ProviderTransform.providerOptions()` 包入以 providerID 命名的 namespace（message replay 另用 `openaiCompatible`）→ `streamText()` → AI SDK `OpenAICompatibleChatLanguageModel.getArgs()`。
* locked `@ai-sdk/openai-compatible@2.0.41` 将标准 reasoning option 写成 Chat Completions body 的 `reasoning_effort`（snake case），来源是 namespace 中的 `reasoningEffort`。未知字段（例如 `thinking`、`enable_thinking`）从同一 namespace 原样透传；这不是 SDK 的通用语义。
* 响应端 AI SDK 读取 `message.reasoning_content`（优先）或 `message.reasoning`；流式读取 `delta.reasoning_content`（优先）或 `delta.reasoning`。OpenCode `normalizeMessages()` 将上一次 assistant reasoning part 回放到 catalog 声明的 `interleaved.field`，通常是 `reasoning_content`。
* OpenCode 明确实现的 OpenAI-compatible 特例：DashScope `alibaba-cn` 自动发 `enable_thinking: true`；Z.AI `zai`/`zhipuai` 发 `thinking: {type:"enabled", clear_thinking:false}`；GLM-5.2 的 compat variants 是 `reasoningEffort: high|max`。Xiaomi MiMo 没有 provider/model-name transform 特例；若 catalog 给出 `interleaved.field=reasoning_content`，只走通用回放逻辑。

## 2. 从 variant/config 到请求体

### 2.1 入口、schema、持久化

`packages/opencode/src/cli/cmd/run.ts` 注册 `--variant`，创建 session 与发送 prompt 时把值放进 `model.variant`。`packages/opencode/src/session/prompt.ts` 的 `PromptInput`/user message 持久化它；无显式输入时，只在历史 agent variant 存在于 `full?.variants` 时复用。

`packages/opencode/src/provider/provider.ts` 的 `Provider.Model` schema 定义：

```ts
options: Record<string, any>
variants?: Record<string, Record<string, any>>
capabilities.reasoning: boolean
capabilities.interleaved: boolean | { field: "reasoning" | "reasoning_content" | "reasoning_text" | string }
```

对应用户配置的 `packages/core/src/v1/config/provider.ts:ConfigProviderV1.Model`：`options`、`variants` 都是任意键。不存在 OpenCode 全局的 `enable_thinking` 或 `thinking.type` schema；它们可以作为 model option 透传，是否接受由上游决定。

### 2.2 变体生成

`packages/opencode/src/provider/provider.ts:fromModelsDevModel()` 从 models.dev 资料建立 `base`，随后执行：

```ts
ProviderTransform.reasoningVariants(model, base) ?? ProviderTransform.variants(base)
```

`packages/opencode/src/provider/transform.ts:reasoningVariants()` 优先级：

1. `reasoning_options` 的 `type:"effort"` → `effortVariants()`；
2. `type:"toggle"` 与 `type:"budget_tokens"` → `reasoningToggle()` + `budgetVariants()`；
3. 无可转换 option 才回落到 `variants(model)` 的内建规则。

对 `@ai-sdk/openai-compatible`，`reasoningEffort()` 生成 `{ reasoningEffort: effort }`，最终由 SDK 写成 `reasoning_effort`。`reasoningBudget()` 对该 npm 返回 undefined，所以 compat 没有统一 token-budget 变体。

### 2.3 合并、providerOptions、AI SDK

`packages/opencode/src/session/llm/request.ts:LLMRequestPrep.prepare` 的实质代码：

```ts
const variant = !input.small && input.model.variants && input.user.model.variant
  ? input.model.variants[input.user.model.variant] : {}
const base = ProviderTransform.options({ model: input.model, sessionID, providerOptions })
const options = mergeOptions(mergeOptions(mergeOptions(base, input.model.options), input.agent.options), variant)
```

优先级是 base defaults < model options < agent options < selected variant；同名嵌套对象用 `mergeDeep` 合并。

`packages/opencode/src/session/llm.ts` 的 AI SDK 路径调用：

```ts
streamText({
  providerOptions: ProviderTransform.providerOptions(input.model, prepared.params.options),
  messages: prepared.messages,
  model: wrapLanguageModel({ middleware: { transformParams(...) {
    args.params.prompt = ProviderTransform.message(...)
  }}}),
})
```

`ProviderTransform.providerOptions()` 对 compat 返回 `{ [providerID 的点号前缀]: normalized }`。注意：OpenCode 的 SDK factory 以 `name: model.providerID` 创建 compat provider，因此该 key 通常是 provider id（如 `alibaba-cn`、`zai`），不是固定的 `openaiCompatible`；`openaiCompatible` 只用于 OpenCode 给 message metadata 写 interleaved 字段。这一步只做 provider namespace 包装，不改字段名。

## 3. locked `@ai-sdk/openai-compatible@2.0.41` 的 wire 行为

精确版本来自 npm 官方 tarball [`openai-compatible-2.0.41.tgz`](https://registry.npmjs.org/@ai-sdk/openai-compatible/-/openai-compatible-2.0.41.tgz)；版本元数据和依赖见 [`package.json`](https://registry.npmjs.org/@ai-sdk/openai-compatible/2.0.41)。tarball 内附 `package/src/`，以下符号均指该版本。

### 请求

`src/chat/openai-compatible-chat-options.ts:openaiCompatibleLanguageModelChatOptions` 只声明 `user`、`reasoningEffort`、`textVerbosity`、`strictJsonSchema`；reasoning 注释默认 `medium`，但不等于 OpenCode 给所有模型写入 medium。

`src/chat/openai-compatible-chat-language-model.ts:OpenAICompatibleChatLanguageModel.getArgs` 依次读取 `openai-compatible`（弃用）、`openaiCompatible`、原始 provider name、provider name camelCase，构造：

```ts
reasoning_effort: compatibleOptions.reasoningEffort,
verbosity: compatibleOptions.textVerbosity,
...providerOptions[providerName],
...providerOptions[toCamelCase(providerName)],
```

已知四个 option 会从 unknown spread 中过滤；`thinking`、`enable_thinking`、`chat_template_args`、`chat_template_kwargs`、`reasoning_content` 不在该 schema，因此放在 provider namespace 会成为顶层请求字段。`transformRequestBody` 是 SDK 提供的最后 body 改写钩子；OpenCode 对自定义 compat provider 默认没有安装转换器。

### assistant 历史消息

`src/chat/convert-to-openai-compatible-chat-messages.ts:convertToOpenAICompatibleChatMessages` 把 AI SDK reasoning parts 拼接为 `reasoning_content`（仅文本非空时），并把 `providerOptions.openaiCompatible` metadata spread 到 assistant message。OpenCode 的 interleaved 回放可覆盖空字段；仅依赖 SDK 原生转换时，空 reasoning 不会自动生成。

### 响应

`src/chat/openai-compatible-api-types.ts:OpenAICompatibleAssistantMessage` 声明 `reasoning_content?`（并允许扩展字段）；locked schema 还接受 `reasoning?`。`doGenerate()` 与 `doStream()` 都执行：

```ts
const reasoning = choice.message.reasoning_content ?? choice.message.reasoning
const reasoningContent = delta.reasoning_content ?? delta.reasoning
```

流式依次发 `reasoning-start`/`reasoning-delta`/`reasoning-end`，在 text-start 前结束 reasoning；`completion_tokens_details.reasoning_tokens` 映射到 reasoning usage。SDK 不读取 `thinking`、`thoughts` 或 `chat_template_kwargs` 响应字段。

## 4. OpenCode reasoning_content / thinking 边界

`packages/opencode/src/provider/transform.ts:normalizeMessages()` 有两条独立逻辑：

1. **DeepSeek 形状修复**：model id 含 `deepseek` 时，assistant 没有 reasoning part 就追加 `{ type:"reasoning", text:"" }`。这是满足历史消息协议，不是开启 thinking。
2. **interleaved 回放**：当 `capabilities.interleaved` 是 `{field}` 且 npm 不是 OpenRouter，提取 assistant reasoning parts，移除这些 parts，在 `providerOptions.openaiCompatible[field]` 写回拼接文本；即使空字符串也写入。

`packages/opencode/src/provider/provider.ts` 的配置模型默认顺序是：用户 `interleaved` > 既有 catalog 值 > 新的 compat DeepSeek（若 API id 含 `deepseek`）自动 `{field:"reasoning_content"}` > `false`。所以 `reasoning_content` 既可能是响应字段，也可能是下一轮 assistant 请求字段；不是通用启用开关。

## 5. 供应商官方协议（对照 OpenCode 实现）

以下是供应商官方文档中的 Chat Completions 约定；它们用于判断迁移目标的真实 wire contract，不表示 OpenCode 会自动发出对应字段：

* [Alibaba Model Studio deep thinking](https://www.alibabacloud.com/help/en/model-studio/deep-thinking)：OpenAI-compatible 请求使用 `enable_thinking`，输出为 `reasoning_content`，可用 `thinking_budget`；官方文档也说明关闭 hybrid-thinking 模式要显式传 `enable_thinking:false`。
* [DeepSeek Thinking Mode](https://api-docs.deepseek.com/guides/thinking_mode)：OpenAI 格式同时支持 `thinking.type` 与 `reasoning_effort`；文档说明 thinking 默认开启、默认 effort 为 high，并要求带 tools 的后续请求完整回传 `reasoning_content`。
* [Z.AI OpenAI SDK](https://docs.z.ai/guides/develop/openai/python) / [Chat Completion](https://docs.z.ai/api-reference/llm/chat-completion)：`thinking.type` 为 `enabled`/`disabled`，reasoning 在 `message.reasoning_content` 或流式 `delta.reasoning_content`；保留思维链时可用 `clear_thinking:false`。
* [Xiaomi MiMo Chat OpenAI API](https://mimo.mi.com/docs/en-US/api/chat/openai-api) / [passing back reasoning_content](https://mimo.mi.com/docs/en-US/usage-guide/passing-back-reasoning_content)：MiMo Chat Completions 使用 `thinking.type` 和 `reasoning_content`，并要求 agent/tool 多轮保留完整 reasoning。MiMo 另有 [Responses API](https://mimo.mi.com/docs/en-US/api/chat/responses) 的 `reasoning.effort`；不能把两个协议的字段混用。

这组对照解释一个关键迁移风险：MiMo、DeepSeek、Z.AI 的官方 Chat Completions 文档都使用 `thinking.type`，而 OpenCode 当前 transform 对 MiMo 没有特例、对 DeepSeek 也没有自动写 `thinking`，只有 Z.AI provider id 才自动写 `thinking`。Stravia 必须按 endpoint/provider 显式 adapter，而不是因“OpenAI-compatible”标签统一猜字段。

## 6. 供应商/模型矩阵

| provider / model 条件 | OpenCode 实际 variant / 默认请求字段 | response / replay | 协议与迁移建议 |
|---|---|---|---|
| 通用 `@ai-sdk/openai-compatible`、reasoning=true | 内建 `variants()` 是 low/medium/high → `reasoningEffort` → body `reasoning_effort`；catalog 的 effort values 优先 | `reasoning_content` 优先、其次 `reasoning`；有 interleaved field 时回放 | Chat Completions 兼容。不可把 effort 视为所有上游都支持 |
| DeepSeek + compat | `variants()` 对 `deepseek-chat`、`deepseek-reasoner`、`deepseek-r1`、`deepseek-v3` 返回 `{}`；`deepseek-v4` 走 compat 的 low/medium/high/max 规则；不会按 id 自动发 `enable_thinking` | 无 reasoning part 的 assistant 补空 part；interleaved/compat DeepSeek 默认回放 `reasoning_content` | 空 reasoning 是历史协议修复，不是请求开关 |
| DashScope `alibaba-cn` + compat（Qwen、DeepSeek R1、Qwen3 等 reasoning=true） | `ProviderTransform.options()` 强制 `enable_thinking:true`；compat toggle 不生成 off/on；budget 不转换 | SDK 读取两种 response 字段；catalog interleaved 时回放 | Alibaba OpenAI-compatible。关闭/等级不能猜，必须显式 provider/API 配置 |
| generic Qwen / `qwen*` | `variants()` 对 qwen id 返回 `{}`；非 `alibaba-cn` 不自动 `enable_thinking` | 标准 compat response/replay | 不把 DashScope 规则复制到所有 Qwen endpoint |
| Kimi/Moonshot + compat（id/provider/url 含 kimi/moonshot） | `variants()` 对 kimi/k2p 返回 `{}`；无自动 thinking 字段；K2 temperature 特例不是 reasoning 开关 | 标准 `reasoning_content`/`reasoning` 与 interleaved | Moonshot Chat Completions；不要套用 Anthropic 规则 |
| Kimi/Moonshot + `@ai-sdk/anthropic` 或 Vertex Anthropic | `isKimiFamily()` → `thinking:{type:"adaptive",display:"summarized"}, effort`；options 默认 effort high | Anthropic thinking blocks/signatures，不是 compat reasoning_content | 仅 Anthropic Messages 协议 |
| Z.AI `zai`/`zhipuai` + compat（GLM-4/5 reasoning=true） | `options()` 默认 `thinking:{type:"enabled",clear_thinking:false}`；普通 GLM variants 常为空 | compat 两种 response 字段；catalog interleaved 时回放 | Zhipu OpenAI-compatible；该 thinking object 是特例 |
| GLM-5.2 + compat | catalog effort 或内建 `variants()` 精确为 high→`reasoningEffort:"high"`、max→`reasoningEffort:"max"`；最终 `reasoning_effort`；Z.AI provider 仍可合并 thinking default | 同上 | 不把 `max` 当成 universal effort |
| Xiaomi provider / MiMo（catalog `npm:"@ai-sdk/openai-compatible"`） | `transform.ts` 无 Xiaomi/MiMo 分支；当前官方 fixture 的 MiMo entries 若含 toggle + `interleaved.field:"reasoning_content"`，compat toggle 不生成 wire field，也无 `enable_thinking` 自动注入 | SDK 读取/回放 `reasoning_content` | Xiaomi OpenAI-compatible；请求字段需以 Xiaomi 官方 API 为准，不能由 OpenCode 推断 |
| 任意自定义 provider config | model options/variant 任意键深合并，未知字段透传 | 仍受 SDK 两个 response 字段限制 | Stravia 应显式配置 per-model adapter，不按名称猜协议 |

### 当前 OpenCode fixture 的一手 vendor 证据

`packages/opencode/test/tool/fixtures/models-api.json` 的 provider entries：

* `alibaba-cn`：`npm:"@ai-sdk/openai-compatible"`、DashScope URL，包含 `qwen-plus`（toggle + budget）和 `deepseek-r1-0528`（reasoning=true）。
* `zai`/`zhipuai`：compat npm，GLM entries 标 reasoning=true；GLM-5.2 的 `reasoning_options` 为 effort `["high","max"]`，并标 interleaved field `reasoning_content`。
* `xiaomi`：compat npm、`https://api.xiaomimimo.com/v1`；MiMo entries 标 reasoning=true、toggle 和 `interleaved.field:"reasoning_content"`。

fixture 是 OpenCode 官方仓库锁定的测试输入；它描述 catalog 能力，不是供应商 API 规范。

### 明确没有源码证据的字段

* `chat_template_kwargs.enable_thinking`：当前 OpenCode provider/transform 与 locked SDK 没有该精确字段。OpenCode 有 `chat_template_args.enable_thinking`（仅 `baseten` 或 OpenCode `kimi-k2-thinking`/`glm-4.6`），以及 MiniMax NVIDIA/Lilac 变体的 `chat_template_kwargs.thinking_mode`；两者都不能改写成 `chat_template_kwargs.enable_thinking`。
* `thinking.type`：OpenCode 在 Z.AI compat、MiniMax、Anthropic/Gateway/Bedrock 等分支使用，但 locked compat SDK 不解释它，只透传为 body 字段。
* `enable_thinking`：只有 `alibaba-cn` compat reasoning model 的 `options()` 自动设置 true；`@ai-sdk/alibaba` 的 `reasoningToggle()` 使用 camelCase `enableThinking`，不是同一路径。
* `reasoning_effort`：是 locked SDK wire field；OpenCode variant 通常先写 camelCase `reasoningEffort`。若 config 直接把 snake case 作为 unknown option 传入，可能与 SDK 生成的字段重复，应避免。

## 7. Stravia 最小迁移矩阵

内部契约建议拆成 `reasoning_enabled`（是否请求）、`reasoning_level`（用户等级）、`reasoning_response_field`（回放字段）。未知 provider 必须显式配置。

| adapter | enabled/on | level | assistant replay |
|---|---|---|---|
| `openai-compatible` 默认 | 不自动开启；只在显式 model option/variant 传入 | low/medium/high → `reasoning_effort`；只使用 provider 声明值 | `reasoning_content ?? reasoning`；需要 interleave 时回放声明字段 |
| DashScope `alibaba-cn` | reasoning model 默认 `enable_thinking:true`；关闭需 provider-specific | 不把 medium/high/max 猜成 Qwen budget | `reasoning_content` |
| Z.AI GLM | 默认 `thinking:{type:"enabled",clear_thinking:false}` | GLM-5.2 high/max → `reasoning_effort`；其他按声明 | `reasoning_content` |
| Xiaomi MiMo | 不自动加字段；遵循显式 API 配置 | toggle/等级按 Xiaomi API | catalog 声明时 `reasoning_content` |
| DeepSeek | 不把补空 reasoning 当启用；API 要求时显式配置 | 只发 API 声明的 effort/budget | 多轮 tool replay 保留 `reasoning_content`，包括空值 |

## 8. 不应通用化

1. `reasoningEffort`（OpenCode/SDK option）与 `reasoning_effort`（API body）是不同层字段；`thinking.type`、`enable_thinking`、`chat_template_*` 是 provider 扩展，不能互相做别名。
2. response `reasoning_content` 只说明上游返回 reasoning 或历史需回放，不证明请求已开启 thinking。
3. 模型名字包含 qwen/kimi/deepseek/glm/mimo 不能决定协议；同一模型可走 Chat Completions、Anthropic Messages、Gateway 或 OpenRouter。先看 SDK npm、provider id、catalog capabilities。
4. `--thinking` 是输出显示偏好，不是 provider option；不应发送给上游。

## 来源索引（OpenCode 与 locked SDK 一手源码）

* OpenCode pinned [`transform.ts`（ProviderTransform.variants/options/providerOptions/message/normalizeMessages）](https://github.com/anomalyco/opencode/blob/10765ff2a9da8c3b88e4de873aa383a49c318912/packages/opencode/src/provider/transform.ts)。
* OpenCode pinned [`request.ts`（LLMRequestPrep.prepare）](https://github.com/anomalyco/opencode/blob/10765ff2a9da8c3b88e4de873aa383a49c318912/packages/opencode/src/session/llm/request.ts)。
* OpenCode pinned [`llm.ts`（streamText 与 prompt middleware）](https://github.com/anomalyco/opencode/blob/10765ff2a9da8c3b88e4de873aa383a49c318912/packages/opencode/src/session/llm.ts)。
* OpenCode pinned [`provider.ts`（Provider.Model、fromModelsDevModel、配置 merge）](https://github.com/anomalyco/opencode/blob/10765ff2a9da8c3b88e4de873aa383a49c318912/packages/opencode/src/provider/provider.ts)。
* OpenCode pinned [`ConfigProviderV1.Model`](https://github.com/anomalyco/opencode/blob/10765ff2a9da8c3b88e4de873aa383a49c318912/packages/core/src/v1/config/provider.ts)。
* OpenCode pinned [`run.ts`（--variant/--thinking）](https://github.com/anomalyco/opencode/blob/10765ff2a9da8c3b88e4de873aa383a49c318912/packages/opencode/src/cli/cmd/run.ts)。
* OpenCode pinned [`models-api.json`（provider/model capability fixture）](https://github.com/anomalyco/opencode/blob/10765ff2a9da8c3b88e4de873aa383a49c318912/packages/opencode/test/tool/fixtures/models-api.json)。
* Locked SDK exact v2.0.41 [`chat model`](https://unpkg.com/@ai-sdk/openai-compatible@2.0.41/src/chat/openai-compatible-chat-language-model.ts)、[`message conversion`](https://unpkg.com/@ai-sdk/openai-compatible@2.0.41/src/chat/convert-to-openai-compatible-chat-messages.ts)、[`API types`](https://unpkg.com/@ai-sdk/openai-compatible@2.0.41/src/chat/openai-compatible-api-types.ts)。
* Locked SDK [`CHANGELOG`](https://unpkg.com/@ai-sdk/openai-compatible@2.0.41/CHANGELOG.md)：2.0.20 记录 multi-turn `reasoning_content` 修复，2.0.41 为当前锁定版本。
* 供应商官方对照文档： [Alibaba](https://www.alibabacloud.com/help/en/model-studio/deep-thinking)、[DeepSeek](https://api-docs.deepseek.com/guides/thinking_mode)、[Z.AI](https://docs.z.ai/guides/develop/openai/python)、[Xiaomi MiMo](https://mimo.mi.com/docs/en-US/api/chat/openai-api)。
