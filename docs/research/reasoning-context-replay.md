# Reasoning context / history replay 研究记录

| 项 | 结论 |
| --- | --- |
| 研究截点 | 2026-10-07 至 2026-10-08 |
| 范围 | 官方 OMP、OpenCode 源码与 IFM 文档；真实 OMP CLI 对隔离的本地回环 SSE 服务进行抓包，并执行 IFM 原始聊天模板。未调用真实模型或生产 API，未读取或修改用户凭据、模型配置。 |
| 本机版本对应源码 | 执行 `omp --version` 得到 `omp/18.7.0`；官方 tag `v18.7.0` 对应 `e0fc1cf4ea354b445a359b37fa5eb58deaa85598`。[tag](https://github.com/can1357/oh-my-pi/releases/tag/v18.7.0) |
| 当前 OMP main 对应源码 | `53f253fb709fe890adf1fa37f0bc69cf02a5d86c`，提交时间 2026-10-07；以下 OMP 非 release 引用均固定此 SHA。[commit](https://github.com/can1357/oh-my-pi/commit/53f253fb709fe890adf1fa37f0bc69cf02a5d86c) |
| OpenCode 对应源码 | 本机 1.18.25 对应 `cb7d8b2f5e44876ef98b661dc10590c915af3a9f`；上游 dev 1.18.35 对应 `ecc4916b5a9608c30e6dd58a67f2137b594407ca`。IFM 文档验证的 1.18.32 对应 `545f51d26cc39a907d2867492d498d9607ea5fa4`。 |
| 核心判断 | **OMP 18.7.0 和当前 main 不支持 `requiresReasoningContentOnAssistantMessages` 这个同名配置；支持另一个真实 runtime 字段 `requiresReasoningContentForAllAssistantTurns`。要保证普通空-trace assistant 历史也带 `reasoning_content: ""`，须配合 `requiresReasoningContentForToolCalls: true` 与 `allowsSyntheticReasoningContentForToolCalls: false`。这些是历史编码选项，不是启动或增强思考的开关。** |

## 1. OMP：同名字段不支持，但存在可实现全 assistant 字段保留的组合

### 1.1 不能只根据 docs 或 schema 缺项判断

- `OpenAICompat` 真正声明了 `requiresReasoningContentForToolCalls`、`requiresReasoningContentForAllAssistantTurns`、`allowsSyntheticReasoningContentForToolCalls`、`syntheticReasoningContentFallback` 和 `replayReasoningContent`；没有 `requiresReasoningContentOnAssistantMessages`。[release types L339–369][O1]
- `requiresReasoningContentForAllAssistantTurns` 与 `replayReasoningContent` 没列在 curated `models.yml` schema 的 `OpenAICompatFields` 中，但属于已注册 runtime wire vocabulary。配置 schema 的缺项不等于运行时不支持。[release schema L40–95][O2]、[wire axes L161–168][O3]
- runtime `applyCompatOverrides` 仅赋值到 resolved compat 原本存在的键：不认识的 `requiresReasoningContentOnAssistantMessages` 即使配置加载成功也不会成为有效 compat 行为。[apply L1–15][O4]。当前 main 额外添加了 unknown compat key 非致命警告；其识别集合同时考虑 schema 和 runtime axes，因此 `requiresReasoningContentForAllAssistantTurns` 不会因为 schema 缺项而被警告。**18.7.0 尚没有这套新增警告，不应以无警告推断键有效。**[main config L142–191][O5]
- 官方 docs 的真实位置是 `~/.omp/agent/models.yml` / `models.yaml`；`providers` 才是被消费的根键，provider、`models[]`、`modelOverrides.<id>` 都有 `compat` 槽。[docs L18–45][O6]、[release schema L202–231、L263–290、L341–352][O2]

### 1.2 可用配置：全 assistant 字段保留，包括没有工具调用的空 trace

以下是配置结构示例，不写入用户配置、不宣称示例 endpoint/model 是真实服务。实际使用时保留自己已存在的 provider URL、认证和模型 ID，只迁移这里的 compat 字段。`requiresReasoningContentForToolCalls` 虽然名字像“只用于工具”，运行时代码还把它当作缺失字段回填路径的总 gate，故不能省略。

```yaml
providers:
  my-provider:
    api: openai-completions
    baseUrl: https://api.example.com/v1
    apiKey: MY_PROVIDER_API_KEY  # 环境变量名，不是密钥字面值
    compat:
      supportsDeveloperRole: false
      reasoningContentField: reasoning_content
      requiresReasoningContentForToolCalls: true
      requiresReasoningContentForAllAssistantTurns: true
      allowsSyntheticReasoningContentForToolCalls: false
    models:
      - id: my-reasoning-model
        reasoning: true
        input: [text]
        contextWindow: 128000
        maxTokens: 16384
```

`contextWindow` / `maxTokens` 应替换为服务真实限制；这些示例数值不是兼容性要求。若已有 bundled/discovered 模型，只需对现有 provider/model 做 override：

```yaml
providers:
  my-provider:
    modelOverrides:
      my-reasoning-model:
        reasoning: true
        compat:
          reasoningContentField: reasoning_content
          requiresReasoningContentForToolCalls: true
          requiresReasoningContentForAllAssistantTurns: true
          allowsSyntheticReasoningContentForToolCalls: false
```

provider compat 是 baseline；model / `modelOverrides` 的 compat 可覆盖它，`extraBody`、routing、`whenThinking` 按嵌套对象合并。不要把这些字段放在 provider 根、`reasoning` 对象内或 root `models` 下。[docs L291–300][O7]

**为什么是三字段组合：** `convertMessages` 的 `needsReasoningField = requiresReasoningContentForAllAssistantTurns || toolCalls.length > 0` 决定覆盖哪些 assistant，但 Tier 1/2 仍额外要求 `requiresReasoningContentForToolCalls && !allowsSyntheticReasoningContentForToolCalls` 才会回填。因此只设 `requiresReasoningContentForAllAssistantTurns: true` 不能保证空 trace 的普通 assistant 有字段；设 synthetic 为 true 也不能保证普通非工具轮有字段。[release encoder L2571–2643][O8]

该组合满足 wire 层“所有 assistant 均带字符串字段，包括 `""`、不发送 `null`”的要求。**这不是承诺所有可能来源的 reasoning 字节原样保留**：多 thinking blocks 会用 `"\n"` 连接；跨模型转换可能 demote 或移除原 thinking；代理从未返回的 trace 无法复原。普通同模型、单文本 trace 的保留路径没有主动 trim 文本（trim 只用于判断是否为空），但保留仍以实际接收到的内容为边界。[encoder L2463–2564、L2598–2636][O8]

## 2. OMP：关键函数实际做什么

### 2.1 生成、接收/存储、历史回传是三件事

| 层 | 实际行为 | 不应推断 |
| --- | --- | --- |
| 声明模型能力 | `model.reasoning` 是能力 gate；`thinking` 描述 effort/budget 等 metadata。 | 仅 `reasoning: true` 不代表某次请求已开启 thinking。 |
| 本次生成策略 | `resolveOpenAICompatPolicy` 要求调用的 `options.reasoning` 非 undefined、未 `disableReasoning`、模型支持且无工具策略冲突；再判定 effort 映射是否等于禁用值。 | history compat 的 `requires*` 不会触发 generation。 |
| 生成 wire 参数 | `applyChatCompletionsCompatPolicy` 按 dialect 编码 `reasoning_effort`、`thinking`、`enable_thinking`、`chat_template_kwargs` 等；关闭可能发 `none` / `false` / `disabled`，也可能只省略参数。 | “UI 关闭 thinking”不保证每个 upstream 都完全不进行内部 reasoning。 |
| 接收与存储 | streaming decoder 从 `reasoning_content`、`reasoning`、`reasoning_text` 按顺序取第一个非空字符串，记入 `ThinkingContent.thinking`，并把字段名记为 `thinkingSignature`。接收段不以 `model.reasoning` / `options.reasoning` 为 gate。 | 这只能存上游返回的 trace，不是读取上游未公开的内部推理。纯空字符串 delta 不创建该 thinking block。 |
| 历史 replay | `buildParams` 根据最终有效工具策略重新算 compat，然后 `convertMessages(model, context, compat)` 编码全部历史；它本身没有请求 reasoning 开关参数。 | 已收到 reasoning 并非所有 endpoint 都无条件得到原生 reasoning 字段。 |

证据：policy [release shared L970–1063][O9]；生成 dialect [main shared L1086–1249][O10]；接收 [release completions L1406–1454][O15]；最终 compat 与编码顺序 [release completions L2117–2127][O16]。

### 2.2 非空真实 reasoning 并非“总回传”

`convertMessages` 对保留下来的 thinking blocks 按顺序选择以下路径：[release L2463–2564][O8]

1. Mistral typed-thinking content parts 特例；
2. `requiresThinkingAsText`：render/demote 成 assistant 普通 `content`；
3. `requiresReasoningContentForToolCalls`：回传真实 thinking 文本。**此非空回传分支没有 tool-call 条件，普通 assistant 也能回传**；是否使用 streamed 字段名或 configured `reasoningContentField` 受 synthetic policy 控制；
4. `thinkingFormat === "zai" && model.reasoning`：作为 continuation hint 回传 configured 字段；
5. `replayReasoningContent`：回传每个携带非空 thinking 的 assistant，通常用于本地 chat-template / KV cache 前缀一致性；它不是占位字段选项，也不生成 thinking。

除这些路径外，不能把“存了 thinking block”理解为“下次一定带结构化 reasoning_content”。`replayReasoningContent` 本身不补空字段。默认 resolver 会为本地 OpenAI-compatible backend 自动开 replay，也对 DeepSeek/Kimi/MiMo/OpenRouter 按 endpoint/model policy 设置必需字段与占位策略，不应推广成所有兼容 endpoint 都自动支持。[release resolve L539–551][O11]

### 2.3 缺失字段：空字符串与 `"."` 不是思考

- exact-replay provider：先尝试从所有 thinking blocks（含空文本且有已知字段名 signature 的 block）恢复；仍缺失则发 `syntheticReasoningContentFallback ?? ""`。
- 接受 synthetic 的 provider：仅工具调用轮、且 format 为 `openai` / `openrouter` / `zai` 时，仍缺字段才发 `"."`。
- `""` / `"."` 是 wire-schema 兼容值，不会凭空恢复 chain-of-thought。某些 backend 校验 exact value，故不能用 `"."` 代替服务要求的空字符串。
- 不要给本次目标配置 `syntheticReasoningContentFallback: " "`；resolver 中有代理专属需求才这样配置，空字符串协议应保持默认 `""`。[types L347–355][O1]、[encoder L2571–2643][O8]

### 2.4 `whenThinking`、关闭 thinking 与工具循环

`whenThinking` 是一个部分 compat override，不允许递归包含另一个 `whenThinking`；resolver 提前构建 baseline 的变体，handler 在本次 policy `enabled` 时选择变体。不只是看 `model.reasoning: true`，还要有本次 effort 且没有禁用/工具冲突。[schema L90–95][O2]、[resolver L659–677][O11]、[policy L990–1012][O9]

合法形状示例（仅当服务要求 thinking-on 才带字段时使用）：

```yaml
providers:
  my-provider:
    compat:
      requiresReasoningContentForToolCalls: false
      requiresReasoningContentForAllAssistantTurns: false
      whenThinking:
        reasoningContentField: reasoning_content
        requiresReasoningContentForToolCalls: true
        requiresReasoningContentForAllAssistantTurns: true
        allowsSyntheticReasoningContentForToolCalls: false
```

**如果服务要求所有轮始终带字段，则用 §1.2 baseline 配置，不能只放在 `whenThinking`。** baseline replay 字段不会因当前 `disableReasoning` 自动关闭；放在 `whenThinking` 的字段会随本轮 policy 退回 baseline。forced / named / auto tool choice、是否携带 tools 都可能影响是否使用 thinking variant；`buildParams` 必须在最终 tool policy 后重新计算，源码和现有 regression tests 明确覆盖该差异。[encoder L2113–2127][O8]、[tests L1484–1776][O12]

工具 loop 与普通 user 多轮都走同一历史编码器；差别是 history assistant 是否含 `toolCall` 决定缺失字段规则。`requiresReasoningContentForAllAssistantTurns` 扩大空字段回填的范围，不是给下一轮增加 reasoning effort。

## 3. 跨模型历史和 encrypted / signed reasoning 的边界

- `transformMessages` 的 same-model 判定是 **provider、api、model ID 三者相等**，不是模型名称相似。[main transform L703–709][O17]
- 同模型带签名 thinking 保留；跨模型的空文本、无可用 anchor block 可能丢弃。跨 API 的 foreign thinking 只对已有明确 unsigned-native replay policy 的 target 保持原生（Z.AI reasoning target、Anthropic `replayUnsignedThinking`）；一般 target 会 demote 成可见文本、用目标 thinking dialect 包裹。`requiresReasoningContent*` / 本地 cache replay **不会让 foreign reasoning 自动成为有语义的 native thinking**。[main transform L380–395、L882–910][O13]
- 因此模型切换后可能看到“前模型 trace 留在普通 content，必需 reasoning_content 是空字符串”的合法 wire 形状；它不等于把前模型的原生 reasoning 状态移植过去。已有 DeepSeek regression test 明确覆盖该情况。[release test L308–336][O14]
- Completions 文本的 `thinkingSignature: "reasoning_content"` 只是字段名标记；不等于 Anthropic cryptographic signature 或 Responses encrypted blob。
- LiteLLM `thinking_blocks` 另行保留签名 / redacted data，优先于重复文本 alias；编码器跳过字段名型 signatures，重建真正 opaque-signed `thinking_blocks`。这些不靠 `reasoningContentField` 替代。[release completions L245–287][O18]、[L1424–1454][O15]
- `includeEncryptedReasoning` / `filterReasoningHistory` 面向 OpenAI **Responses** 的 native reasoning items；不是当前 Chat Completions `reasoning_content` 开关。不可把三者当作同一种通用字段。[types L333–336][O1]

## 4. Release / 当前 main 差异与其他 Pi 的界线

本次逐文件比较官方 `v18.7.0` 与固定 main：types、compat resolver、axes、apply 和 reasoning regression tests 完全相同；`openai-completions.ts` 只有 `getEnvApiKey` import 改动，reasoning replay 逻辑和引用行号相同；request-policy 部分也相同。当前 main 的主要相关配置差异是新增 unknown compat key 警告，而不是新增本文 replay 能力。`transform-messages.ts` 有 copy-on-write redaction 改动，未改变上述 thinking 分支语义，main 行号比 release 对应段多 4 行。

本文结论明确针对 **oh-my-pi / OMP**，不能用其他 Pi distribution 的 `~/.pi/agent/models.json` 示例或其字段名称替代 OMP schema/runtime 证据。旧 Pi 是否支持同名字段应以其独立官方版本为证，不在本文把其能力归给 OMP。

## 5. IFM：为什么空字段也是协议的一部分

### 5.1 文档要求所有 assistant 原样回传，不只是工具轮

IFM 的 multi-turn 文档明确：服务无状态，每次请求携带完整历史；普通回复、工具调用、流式回复遵循同一规则。`reasoning_content` 应原样回传，`""` 是合法值，不能删除字段或改成 `null`。流式响应先组装 deltas，再加入历史。[Multi-turn][I1]

Hermes 文档验证版本为 0.19.0。其 provider plugin 注册 IFM，并把 `_needs_thinking_reasoning_pad()` 对 IFM provider/endpoint 的返回值改成 `True`；文档将其效果描述为保留/补齐 replay 的 reasoning 字段。它不是给模型增加思考预算，也不是保存隐藏的服务端计算状态。文档说明缺字段会得到 `400 Add a supported thinking field to each assistant message in the multi-turn conversation history`。[Hermes][I2]

IFM 的 Pi 示例明确针对 **Earendil Works Pi**，使用 `~/.pi/agent/models.json` 和 `requiresReasoningContentOnAssistantMessages: true`；不是 OMP 的配置示例。该文档特别说明没有此选项时，空 trace 的字段会被丢弃。[Pi][I3]

### 5.2 模型实际接收到什么

官方 K2 Horizon 的 `chat_template.jinja` 提供了比配置名称更直接的证据：[固定模板版本][I5]

- 每个历史 assistant 必须提供 `think`、`reasoning`、`reasoning_content`、`think_fast`、`think_faster` 中至少一个字符串。缺字段与非字符串值分别抛异常。
- `reasoning_content: "原始推理"` 被渲染进 `<ifm|think>\n原始推理</ifm|think>`，随后才是回答内容；历史推理因此成为下一次模型输入的实际 token，而不只是 UI 展示信息。
- `reasoning_content: ""` 被渲染为空的 thinking 区段。空值能满足模板契约，但不能恢复从未收到或已丢失的 trace。
- 新一轮生成的前缀另由 `reasoning_effort` 决定：`high` 使用 `<ifm|think>`，`medium` 使用 `<ifm|think_fast>`，`low` 使用 `<ifm|think_faster>`。默认是 `high`；模板拒绝其他值。

因此 **history replay 与 generation effort 是两条独立路径**。不能把“回传上轮 trace”说成“本轮被强制开启更深思考”，也不能把字符串占位说成恢复完整思考过程。

### 5.3 真正控制 IFM 新一轮思考的请求参数

官方 reasoning 文档要求把 effort 放进请求 body 的 `chat_template_kwargs`，支持 `low` / `medium` / `high`；推荐生产使用 `high`，低档需自己评估延迟、成本与质量取舍。[Reasoning][I4]

```json
{
  "chat_template_kwargs": {
    "reasoning_effort": "high"
  }
}
```

在 OMP 自定义 `openai-completions` provider 中可通过以下 compat 发送静态参数：

```yaml
compat:
  supportsReasoningEffort: false
  extraBody:
    chat_template_kwargs:
      reasoning_effort: high
```

这与 §1.2 的 replay 三字段是不同功能。`supportsReasoningEffort: false` 不代表服务不思考，而是避免 OMP 再发送通用顶层 `reasoning_effort`；`extraBody` 的静态 `high` 仍会发送。此配置不会自动随 UI thinking 档位变化，用户切到 `off` 也不能据此推断 IFM 已禁用思考。若需要动态档位映射，必须另核对 transport 的 thinking dialect，不能靠 replay 选项替代。

### 5.4 效果与风险边界

回传 trace 能满足 IFM 的强制历史契约，并保留模型模板的历史思考区段。它不等于保持一个有状态的服务端“思维对象”，也不保证回答质量一定提高。历史 trace 会成为后续 prompt 的一部分；上下文占用、输入计费及缓存命中效果取决于服务实现与价格，本文未进行真实计费或质量评测。

## 6. 实际验证与未验证范围

### 6.1 本机 OMP 18.7.0 的真实 HTTP 请求

执行 `omp --version`，观察到 `omp/18.7.0`。随后通过 `Bun.spawn` 运行真实 `omp.exe`：

```text
omp --mode rpc --no-ui --no-session --no-tools --no-lsp
    --no-extensions --no-skills --no-rules --no-title
    --thinking high --model replay-probe/replay-probe
    --system-prompt "Local serialization probe."
```

每个实例使用临时 `PI_CODING_AGENT_DIR` 和临时工作目录，子进程仅继承必要的 OS 环境变量，不继承 API keys；模型 endpoint 指向仅监听 `127.0.0.1` 的本地 SSE 服务。服务依次返回空 trace、非空的合成标记 `SYNTHETIC_LOCAL_TRACE`、空 trace；发送三次 RPC `prompt`，截取下一轮 HTTP body 中的历史 assistant。这里的 SSE 服务只用来观察真实客户端编码，不代表真实模型推理。

显式设置 `replayReasoningContent: false`，排除 loopback endpoint 的自动非空 replay 规则，模拟无默认 replay 的自定义远端 provider。每组均收到三次 `prompt_result.status: completed`，CLI exit code 为 0，stderr 为空。

| 配置组 | 第二次请求中的首轮空 trace | 第三次请求中的第二轮非空 trace |
| --- | --- | --- |
| §1.2 三字段组合 | `reasoning_content: ""` | `reasoning_content: "SYNTHETIC_LOCAL_TRACE"` |
| 只有错误同名键 `requiresReasoningContentOnAssistantMessages: true` | 无 reasoning 字段 | 无 reasoning 字段 |
| 仅 `requiresReasoningContentForAllAssistantTurns: true`，ToolCalls gate 为 false | 无 reasoning 字段 | 无 reasoning 字段 |

三字段组合另配 §5.3 的 `extraBody`，抓包确认每次请求包含 `chat_template_kwargs.reasoning_effort: "high"`，不含顶层 `reasoning_effort`。

### 6.2 执行 IFM 原始聊天模板

通过 `uv run --no-project --with jinja2 python -` 执行固定版本的完整模板；为 Transformers 的 `{% generation %}` 标记提供透明渲染 extension，不改模板的历史校验或生成前缀逻辑。退出码为 0：

| 输入/参数 | 观察结果 |
| --- | --- |
| 历史 assistant 没有 thinking 字段 | 拒绝：`Assistant message is missing a thinking field...` |
| `reasoning_content: null` | 拒绝：`Assistant thinking fields must be strings...` |
| `reasoning_content: ""` | 通过，渲染空 thinking 区段 |
| `reasoning_content: "Two plus two is four."` | 通过，trace 原样进入历史 thinking 区段 |
| 新轮 effort 为 `high` / `medium` / `low` | 分别生成 `think` / `think_fast` / `think_faster` 前缀 |

### 6.3 限制

未调用 IFM hosted API，未验证真实模型质量、计费、服务端缓存或 GPU 推理。OMP 与 OpenCode 实跑均覆盖普通三轮 replay；工具循环、跨模型和 signed/encrypted reasoning 的结论来自源码与现有测试的静态阅读，没有宣称这些测试已运行。未运行项目构建、lint 或无关测试；仓库变更只有本研究文档。临时抓包服务、进程、配置与会话数据库已清理。

## 7. OpenCode：`interleaved` 的合法形状与实际回传

### 7.1 字符串确实合法，不应误判为必须改成对象

IFM 文档验证版本为 OpenCode 1.18.32，给出的配置是 model entry 上的字符串 `[I6]`：

```json
{
  "provider": {
    "ifm": {
      "npm": "@ai-sdk/openai-compatible",
      "options": {
        "baseURL": "{env:IFM_BASE_URL}",
        "apiKey": "{env:IFM_API_KEY}"
      },
      "models": {
        "{env:IFM_MODEL}": {
          "interleaved": "reasoning_content"
        }
      }
    }
  }
}
```

本机执行 `opencode --version` 得到 **1.18.25**。在隔离环境执行 `opencode --pure debug config`，字符串 `"reasoning_content"` 与旧对象 `{"field":"reasoning_content"}` 均 exit code 0，resolved config 保留各自形状，stderr 为空。当前本机版本无需把 IFM 文档中的字符串改为对象。

源码也确认 1.18.25、1.18.32 与 dev 1.18.35 同时接受 string、`{field}` 和 boolean；string 会在 provider loading 中规范化为同一个内部 `{field}`，两种写法没有 replay 算法分叉。仅 `interleaved: true` 不提供 field，不等于 `"reasoning_content"`。schema 允许任意字段名字符串，但这不保证 adapter 能解析服务响应中的同名字段。[1.18.25 schema][C1]、[1.18.32 schema][C2]、[dev schema][C3]、[provider normalization][C4]

`interleaved` 应位于 `provider.<id>.models.<model-id>`，不是 provider 根。IFM 文档明确把此设置与多步骤 reasoning 回传、空 trace 保留关联，而不是 effort 控制。[OpenCode][I6]

### 7.2 实现数据流

1. AI SDK 解析 SSE 中的 `reasoning_content`，或 fallback `reasoning`，产生非空 reasoning events；OpenCode adapter 将其映射为 `reasoningDelta`，processor 写入会话的 assistant reasoning part。此接收路径不以 `interleaved` 为 gate。[SDK parser][C9]、[event adapter][C5]、[persistence][C6]
2. 下一轮通过 `MessageV2.toModelMessages` 重建历史。相同 provider/model 保留 reasoning parts；跨模型时仅把非空 reasoning 降成普通 text，并不移植原模型 reasoning metadata。[history conversion][C7]
3. `ProviderTransform.message` 对带 field 的 `capabilities.interleaved`、且 assistant content 为数组时，把 reasoning parts 的文本用 `join("")` 拼接，移除原 reasoning parts，再写进 `providerOptions.openaiCompatible[field]`。**即使拼接结果为空，也显式写 `""`。** 普通 text 和 tool-call parts 留在原 assistant 中。该通用变换排除了 `@openrouter/ai-sdk-provider`，不能推广成所有 adapter 都是同一路径。[1.18.25 transform][C8]、[dev transform][C10]
4. LLM middleware 在每次请求前应用该 transform；SDK 再将 message-level metadata 展开到最终 HTTP assistant 对象，与 `tool_calls` 同条消息发送。工具循环与普通用户续轮都重建历史后走这个编码链，但本次实际运行只覆盖普通续轮。[LLM middleware][C11]、[prompt loop][C12]、[SDK assistant encoding][C13]

所以这个选项是**内部 reasoning parts 到指定 wire 字段的映射与空值保留策略**，不是“开始思考”“显示思考”或“提高思考档位”。`model.reasoning` 映射为另外的 `capabilities.reasoning`；它参与内建 effort variants 的生成，与 `interleaved` 不是同一个开关。[capability mapping][C4]、[variants gate][C14]

不配置 `interleaved` 时，OpenAI-compatible SDK 默认也把非空 reasoning parts 编码成 `reasoning_content`，但不会为没有 reasoning 的消息自动写空字段；这解释了下面的实跑对照。字段映射不负责恢复 opaque signature / encrypted reasoning items。SDK 输出 parser 只识别文中确认的 `reasoning_content` / `reasoning`，不能因 outgoing 配置可写 `reasoning_text` 就推断 incoming 也被支持。[SDK encoding][C13]、[SDK parser][C9]

依赖证据边界：上游 workspace 声明兼容 SDK 2.0.41，但锁文件 package resolution 项还出现 2.0.37；两版发布包上述 reasoning 入/出参语义相同。本笔记不以声明版本断言本机二进制内唯一实际 SDK 版本，实际 wire 行为以下面的 CLI 抓包为准。

### 7.3 本机真实 CLI 与 HTTP 抓包

通过临时 HOME、XDG 路径、`OPENCODE_CONFIG`、工作目录及仅含必要 OS 值的子进程环境运行：

```text
opencode --pure run --format json --model probe/probe
    --title "Local serialization probe." "Local serialization turn 1"
opencode --pure run --format json --model probe/probe
    --title "Local serialization probe." --continue "Local serialization turn 2"
```

第三轮同样使用 `--continue`，每组观察到相同 session ID。三个配置组使用 `@ai-sdk/openai-compatible` 与独立临时状态；没有显式设置 model `reasoning: true`。仅回环 SSE 服务依次返回空 trace、非空合成标记、空 trace，实际截取客户端后续 HTTP body。九次 CLI 运行均 exit code 0、stderr 为空、没有 error event。

| 模型配置 | 空 trace 的历史 assistant | 非空 trace 的历史 assistant |
| --- | --- | --- |
| `"interleaved": "reasoning_content"` | `reasoning_content: ""` | 保留原标记 |
| `"interleaved": {"field":"reasoning_content"}` | `reasoning_content: ""` | 保留原标记 |
| 未设置 `interleaved` | 不带 reasoning 字段 | 仍带原 `reasoning_content` 标记 |

这说明不能简单说“没有 `interleaved` 就完全丢弃所有思考”。本机此 adapter 路径原本就能回传非空 trace，差异尤其在于**空值字段是否存在**；这恰好是 IFM 严格模板校验的边界。该实验不证明全部 adapter、全部 OpenCode 版本或工具循环具有相同行为。

## 来源（固定 SHA / 精确行号）

[O1]: https://github.com/can1357/oh-my-pi/blob/e0fc1cf4ea354b445a359b37fa5eb58deaa85598/packages/catalog/src/types.ts#L333-L369
[O2]: https://github.com/can1357/oh-my-pi/blob/e0fc1cf4ea354b445a359b37fa5eb58deaa85598/packages/coding-agent/src/config/models-config-schema-bundle.ts#L40-L352
[O3]: https://github.com/can1357/oh-my-pi/blob/e0fc1cf4ea354b445a359b37fa5eb58deaa85598/packages/catalog/src/compat/axes.ts#L161-L168
[O4]: https://github.com/can1357/oh-my-pi/blob/e0fc1cf4ea354b445a359b37fa5eb58deaa85598/packages/catalog/src/compat/apply.ts#L1-L15
[O5]: https://github.com/can1357/oh-my-pi/blob/53f253fb709fe890adf1fa37f0bc69cf02a5d86c/packages/coding-agent/src/config/models-config.ts#L142-L191
[O6]: https://github.com/can1357/oh-my-pi/blob/53f253fb709fe890adf1fa37f0bc69cf02a5d86c/docs/models.md#L18-L45
[O7]: https://github.com/can1357/oh-my-pi/blob/53f253fb709fe890adf1fa37f0bc69cf02a5d86c/docs/models.md#L291-L300
[O8]: https://github.com/can1357/oh-my-pi/blob/e0fc1cf4ea354b445a359b37fa5eb58deaa85598/packages/ai/src/providers/openai-completions.ts#L2463-L2643
[O9]: https://github.com/can1357/oh-my-pi/blob/e0fc1cf4ea354b445a359b37fa5eb58deaa85598/packages/ai/src/providers/openai-shared.ts#L970-L1063
[O10]: https://github.com/can1357/oh-my-pi/blob/53f253fb709fe890adf1fa37f0bc69cf02a5d86c/packages/ai/src/providers/openai-shared.ts#L1086-L1249
[O11]: https://github.com/can1357/oh-my-pi/blob/e0fc1cf4ea354b445a359b37fa5eb58deaa85598/packages/catalog/src/compat/resolve.ts#L539-L677
[O12]: https://github.com/can1357/oh-my-pi/blob/e0fc1cf4ea354b445a359b37fa5eb58deaa85598/packages/ai/test/openai-completions-compat.test.ts#L1484-L1776
[O13]: https://github.com/can1357/oh-my-pi/blob/53f253fb709fe890adf1fa37f0bc69cf02a5d86c/packages/ai/src/providers/transform-messages.ts#L882-L910
[O14]: https://github.com/can1357/oh-my-pi/blob/e0fc1cf4ea354b445a359b37fa5eb58deaa85598/packages/ai/test/deepseek-reasoning-content.test.ts#L308-L546
[O15]: https://github.com/can1357/oh-my-pi/blob/e0fc1cf4ea354b445a359b37fa5eb58deaa85598/packages/ai/src/providers/openai-completions.ts#L1406-L1454
[O16]: https://github.com/can1357/oh-my-pi/blob/e0fc1cf4ea354b445a359b37fa5eb58deaa85598/packages/ai/src/providers/openai-completions.ts#L2117-L2127
[O17]: https://github.com/can1357/oh-my-pi/blob/53f253fb709fe890adf1fa37f0bc69cf02a5d86c/packages/ai/src/providers/transform-messages.ts#L703-L709
[O18]: https://github.com/can1357/oh-my-pi/blob/e0fc1cf4ea354b445a359b37fa5eb58deaa85598/packages/ai/src/providers/openai-completions.ts#L245-L287
[I1]: https://docs.ifm.ai/#/multi-turn
[I2]: https://docs.ifm.ai/#/hermes
[I3]: https://docs.ifm.ai/#/pi
[I4]: https://docs.ifm.ai/#/reasoning
[I5]: https://huggingface.co/IFM/K2-Horizon-375B-A23B/blob/b2f9bb66cd7aaf515e08cb256a4bd2ee00590229/chat_template.jinja
[I6]: https://docs.ifm.ai/#/opencode
[C1]: https://github.com/anomalyco/opencode/blob/cb7d8b2f5e44876ef98b661dc10590c915af3a9f/packages/core/src/v1/config/provider.ts#L8-L30
[C2]: https://github.com/anomalyco/opencode/blob/545f51d26cc39a907d2867492d498d9607ea5fa4/packages/core/src/v1/config/provider.ts#L8-L30
[C3]: https://github.com/anomalyco/opencode/blob/ecc4916b5a9608c30e6dd58a67f2137b594407ca/packages/core/src/v1/config/provider.ts#L8-L30
[C4]: https://github.com/anomalyco/opencode/blob/ecc4916b5a9608c30e6dd58a67f2137b594407ca/packages/opencode/src/provider/provider.ts#L1571-L1598
[C5]: https://github.com/anomalyco/opencode/blob/ecc4916b5a9608c30e6dd58a67f2137b594407ca/packages/opencode/src/session/llm/ai-sdk.ts#L161-L179
[C6]: https://github.com/anomalyco/opencode/blob/ecc4916b5a9608c30e6dd58a67f2137b594407ca/packages/opencode/src/session/processor.ts#L279-L312
[C7]: https://github.com/anomalyco/opencode/blob/ecc4916b5a9608c30e6dd58a67f2137b594407ca/packages/opencode/src/session/message-v2.ts#L375-L389
[C8]: https://github.com/anomalyco/opencode/blob/cb7d8b2f5e44876ef98b661dc10590c915af3a9f/packages/opencode/src/provider/transform.ts#L321-L348
[C9]: https://unpkg.com/@ai-sdk/openai-compatible@2.0.41/dist/index.mjs#L711-L724
[C10]: https://github.com/anomalyco/opencode/blob/ecc4916b5a9608c30e6dd58a67f2137b594407ca/packages/opencode/src/provider/transform.ts#L321-L350
[C11]: https://github.com/anomalyco/opencode/blob/ecc4916b5a9608c30e6dd58a67f2137b594407ca/packages/opencode/src/session/llm.ts#L280-L343
[C12]: https://github.com/anomalyco/opencode/blob/ecc4916b5a9608c30e6dd58a67f2137b594407ca/packages/opencode/src/session/prompt.ts#L1261-L1329
[C13]: https://unpkg.com/@ai-sdk/openai-compatible@2.0.41/dist/index.mjs#L235-L247
[C14]: https://github.com/anomalyco/opencode/blob/ecc4916b5a9608c30e6dd58a67f2137b594407ca/packages/opencode/src/provider/transform.ts#L790-L793
