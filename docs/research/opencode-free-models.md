# OpenCode 免费模型获取方式研究

> **范围与证据等级**：本文以 `anomalyco/opencode` 仓库 `dev` 分支在 **2026-09-29** 读取的 commit `7945de208964a49300d7f770d1a71d078db9a4c4`（提交时间 2026-09-28T22:59:34Z）为源码证据；以 `opencode.ai/docs/zen/`（读取于 2026-09-29）和 `opencode.ai/legal/terms-of-service`（生效日期 2026-08-15）为官方文档证据；以公开 GitHub issue 与第三方桥接项目源码/README 为行为佐证。标记为「实测/社区观测」的内容来自第三方项目的实测记录，不是官方承诺；标记 [无法确认] 的内容无法从一手来源证实。本文不包含任何真实 token。

## 1. 先给结论

1. **Zen 是 OpenCode 自营的 AI 网关**，按 token 计费（pay-as-you-go），同时维护一组**限时免费模型**。截至 2026-09-29 官方文档列出的免费模型为：`big-pickle`、`space-bunny-free`、`longcat-2.5-preview-free`、`mimo-v2.6-flash-free`、`mimo-v2.5-free`、`ling-3.0-flash-fin-free`、`nemotron-3-ultra-free`、`nemotron-3.5-lightning-free`、`muse-spark-1.3-contributor-free`、`jev-1.13-free`。官方明确说这些免费是 "for a limited time"，用于收集反馈/改进模型——本质是模型方或 OpenCode 补贴的推广期，部分免费模型的对话数据会被用于训练（Big Pickle、MiMo、Ling、Nemotron、Muse Spark Contributor 等，见 §6 隐私段）。
2. **免费模型匿名可访问**：不配置 Zen API key 时，opencode 客户端把 `apiKey` 写成字面量 `"public"` 发送；服务端对 `allowAnonymous` 模型跳过 key 校验，改用**客户端公网 IP** 做限流（Redis 计数、UTC 自然日重置、社区估计默认约 200 请求/天但真实数值在私有 secret 里）。
3. **但免费层有客户端身份门控**，且持续收紧：opencode 客户端请求时注入 `User-Agent: opencode/<version>`、`x-opencode-session`、`x-opencode-request`、`x-opencode-client`、`x-opencode-project` 等头；第三方实测（OmniRoute，2026-09-17~21）表明免费层执行四条件契约——`stream:true`、非空 tools（且 **tool 名字被按模型检查**，编造名字在部分模型被拒）、`ses_` 形状 session 头、`opencode/≥1.17` UA，缺一则 `403 FreeTierError`。此外 `opencode.ai/zen/*` 路径前有 Cloudflare WAF（1010 browser-signature 规则）会按 UA/TLS 指纹拦非浏览器签名客户端。
4. **市面上已有一整类「反代免费层」项目**：它们统一的做法是直接调 `https://opencode.ai/zen/v1`，注入 `Bearer public` + opencode UA + 伪造的 `x-opencode-*` 头，再对外暴露 OpenAI/Anthropic 兼容 API。代表项目：`12errh/zen-proxy`、`bigdata2211it-web/opencode-free-proxy`、`Maicon501a/opencode-zen-proxy`、`diegosouzapw/OmniRoute`（契约实现最完整）、`lumishoang/opencode-proxy`（OpenCode Go）、`PandaDecSt/opencodeProxy`、`thelabcorner/opencode-zen-fut-api`（额度研究）。
5. **合规风险明确**：ToS 要求「仅限本人内部使用、不得为第三方利益使用」，禁止多账号绕过限额、禁止自动化提取；OpenCode 保留随时中止服务的裁量权。匿名免费层走 IP 限流无需账号，因此「封号」主要体现为 IP/UA 维度封禁与门槛收紧，历史上已发生多轮（header 校验 → UA 门控 → session 头 → stream+tools 校验 → Cloudflare WAF）。

## 2. Zen 是什么、免费模型与付费机制

### 2.1 定位

官方文档原文：「OpenCode Zen is an AI gateway that gives you access to these models」——Zen 不是模型提供方，而是 OpenCode 团队 benchmark 过的一组「模型 × 上游供应商」组合的**网关/路由层**，对请求做格式归一（OpenAI chat/responses、Anthropic messages、Google generateContent、SystemOne）后转发给下游供应商并计费。来源：[opencode.ai/docs/zen](https://opencode.ai/docs/zen/)。

第三方逆向文档对服务端内部结构的重建（`ProviderHelper`/`OpenAIHelper`/`AnthropicHelper`/`OpenAICompatibleHelper`）与公开仓库 `packages/console/app/src/routes/zen/util/` 下的真实文件一致（`provider/openai.ts`、`provider/anthropic.ts`、`provider/openai-compatible.ts`、`provider/google.ts`、`provider/systemone.ts`、`handler.ts`），可交叉印证。来源：[Maicon501a/opencode-zen-proxy REVERSE_ENGINEERING.md](https://github.com/Maicon501a/opencode-zen-proxy/blob/main/docs/REVERSE_ENGINEERING.md)、[handler.ts @7945de2](https://github.com/anomalyco/opencode/blob/7945de208964a49300d7f770d1a71d078db9a4c4/packages/console/app/src/routes/zen/util/handler.ts)。

### 2.2 Endpoint 与 API key

- 计费用户：在 `https://opencode.ai/auth` 登录（Zen 账号体系，workspace 概念），拿 API key；TUI 里 `/connect` 选 OpenCode Zen 粘贴 key，或直接用 `Authorization: Bearer <key>` 调 endpoint。来源：[opencode.ai/docs/zen](https://opencode.ai/docs/zen/)。
- 模型目录：`GET https://opencode.ai/zen/v1/models`。按模型走不同 endpoint：`/zen/v1/chat/completions`（OpenAI 兼容系）、`/zen/v1/responses`（GPT/Grok/Muse 系）、`/zen/v1/messages`（Claude 系）、`/zen/v1/models/<id>`（Gemini 系）、`/zen/v1/systemone`（Jev）。来源：同上。
- OpenCode Go（$10/月订阅档）走独立前缀 `https://opencode.ai/zen/go/v1`。来源：[opencode.ai/docs/go](https://opencode.ai/docs/go/)、[lumishoang/opencode-proxy 默认配置](https://github.com/lumishoang/opencode-proxy)。
- **历史上的 `api.opencode.ai`**：旧版本/社区材料中 Zen 曾用该主机名；当前官方文档全部指向 `opencode.ai/zen/...`，`api.opencode.ai` 已不是文档化入口（[无法确认] 官方是否发过正式迁移公告）。来源：[issue #39872](https://github.com/anomalyco/opencode/issues/39872)（该 issue 中讨论的是 Go 区域限制，主机名废弃为社区观察）。

### 2.3 免费模型是谁买单

官方没有逐模型说明出资方，但文档措辞给出机制：

- Big Pickle、Space Bunny 标为 **stealth model**（匿名打榜/试用模型）；
- MiMo、Ling、Nemotron、Muse Spark Contributor 等写明「the team is using this time to collect feedback and improve the model」，即**模型方以免费额度换反馈/数据**（Muse Spark Contributor Free 明确是「以折扣价换 Meta 训练数据授权」；Nemotron 免费端点走 NVIDIA API Trial Terms）。来源：[opencode.ai/docs/zen Privacy 与 free models 段](https://opencode.ai/docs/zen/)。

即：免费成本主要由上游模型方的推广/数据条款覆盖，OpenCode 承担网关与滥用防护成本（用 IP 限流与客户端门控控量）。

## 3. 访问条件与认证机制（源码层面）

### 3.1 客户端如何接入

`packages/opencode/src/provider/provider.ts` 中 `opencode` provider 的加载逻辑（[@7945de2 #L220-241](https://github.com/anomalyco/opencode/blob/7945de208964a49300d7f770d1a71d078db9a4c4/packages/opencode/src/provider/provider.ts)）：

```ts
const ok = hasKey || Boolean(await dep.auth(input.id)) || config.provider?.["opencode"]?.options?.apiKey
if (!ok) {
  // 没有任何 Zen 凭据时：删掉所有收费模型，只留 cost.input === 0 的模型
  for (const [key, value] of Object.entries(input.models)) {
    if (value.cost.input === 0) continue
    delete input.models[key]
  }
}
return { autoload: ..., options: ok ? {} : { apiKey: "public" } }
```

即：**无 key 时客户端发 `Authorization: Bearer public`**，模型列表被裁剪到只剩免费模型。这一点也被 `Maicon501a/opencode-zen-proxy` 从二进制中提取的同一行代码独立证实（`options: hasKey ? {} : { apiKey: "public" }`）。来源：[README](https://github.com/Maicon501a/opencode-zen-proxy)。

请求侧头注入在 `packages/opencode/src/session/llm/request.ts`（[@7945de2 #L186-199](https://github.com/anomalyco/opencode/blob/7945de208964a49300d7f770d1a71d078db9a4c4/packages/opencode/src/session/llm/request.ts)）：

```ts
// 仅当 providerID 以 "opencode" 开头时
headers: {
  "x-opencode-project": <project.id>,   // 有则
  "x-opencode-session": input.sessionID,
  "x-opencode-request": input.user.id,
  "x-opencode-client":  input.flags.client,
  "User-Agent": `opencode/${InstallationVersion}`,
}
```

非 opencode provider 则发 `x-session-affinity`/`X-Session-Id`。这是服务端识别「真 opencode 流量」的客户端侧实现。

### 3.2 服务端：`Bearer public` → 匿名 + IP 限流

`packages/console/app/src/routes/zen/util/handler.ts`（[@7945de2 #L120-150](https://github.com/anomalyco/opencode/blob/7945de208964a49300d7f770d1a71d078db9a4c4/packages/console/app/src/routes/zen/util/handler.ts)）：

- `authorization` 头解析出的 key 等于 `"public"` 时被归一为 `undefined`（`zenApiKey = rawZenApiKey === "public" ? undefined : rawZenApiKey`）；
- `x-real-ip` 取为限流身份，IPv6 截断到前 4 个 hextet；
- 模型配置带 `allowAnonymous` 时走 `createIpRateLimiter`（IP 限流），否则走 `createKeyRateLimiter`（按 API key 限流）；
- **限流检查先于 `authenticate()` 与计费路径选择**——所以有 Zen 余额/Go 订阅的用户走免费模型同样会撞上 IP 池耗尽（社区有大量此类报告，见 §7）。

`authenticate()`（同文件 ~L695-700）：无 key 且 `modelInfo.allowAnonymous` → 直接放行（返回 undefined）；无 key 且非 anonymous 模型 → `AuthError missingApiKey`。key 存在时查 `KeyTable` 联表 workspace/billing/user/subscription，被 `isBlocked`、`isFlaggedByAnthropic`（claude-* 模型）、`isFlaggedByOpenAI`（gpt-* 模型）标记的 workspace 抛 `AuthError`——即**封禁机制存在于 workspace 维度**。

### 3.3 IP 限流算法（公开源码，高精度）

`packages/console/app/src/routes/zen/util/ipRateLimiter.ts`（[@7945de2](https://github.com/anomalyco/opencode/blob/7945de208964a49300d7f770d1a71d078db9a4c4/packages/console/app/src/routes/zen/util/ipRateLimiter.ts)）：

- 配额值：`rateLimit ?? limits.dailyRequests`，两者都来自私有 secret（`ZEN_LIMITS`、`ZEN_MODELS1..30`），公开仓库**不含数值**；
- 桶键：默认模型 `YYYYMMDD`（UTC 自然日）；有模型级 `rateLimit` override 时为 `YYYYMMDD + modelId.substring(0,2)`——即**按模型 ID 前两个字符共享桶**（`ne` 会让两个 Nemotron 模型互相挤占）；
- 新 IP 宽限：lifetime 计数 < `dailyLimit*7` 的 IP 视为「新」，当天可消费 `dailyLimit*2`；override 模型不适用；
- 超限抛 `FreeUsageLimitError`，`Retry-After` = 距下一个 UTC 日界线的秒数；
- 计数粒度是**请求数**，不是 token 数（`incr` 每请求 +1）；
- 源码里留有被注释掉的 `checkHeaders` 校验（`limits.checkHeaders` 逐头比对），当前 `const headersExist = true` 恒真——提交历史中有 `zen: remove header check`（2026-04-05），说明**请求头校验曾在该层实施后被移除**（可能下沉到了私有层，见 §3.4）。

`Subscription.getFreeLimits()` 的 schema 为 `{ promoTokens, dailyRequests, dailyRequestsFallback, checkHeaders }`，值来自 `Resource.ZEN_LIMITS`（SST secret）；模型级 `rateLimit`/`allowAnonymous`/`trialProvider` 来自 `ZEN_MODELS1..30` 拼接的 secret。来源：[subscription.ts @7945de2](https://github.com/anomalyco/opencode/blob/7945de208964a49300d7f770d1a71d078db9a4c4/packages/console/core/src/subscription.ts)、[model.ts @7945de2](https://github.com/anomalyco/opencode/blob/7945de208964a49300d7f770d1a71d078db9a4c4/packages/console/core/src/model.ts)、[infra/console.ts](https://github.com/anomalyco/opencode/blob/dev/infra/console.ts)。

社区对默认 `dailyRequests` 的最佳估计是 **~200 请求/IP/UTC 天**（多篇 Reddit 实测 + FAQ 转述），但既然值在 secret 里，任何精确数字都不能从公开源码证明（置信度中）。来源：[thelabcorner/opencode-zen-fut-api RESEARCH.md](https://github.com/thelabcorner/opencode-zen-fut-api/blob/main/RESEARCH.md)。

### 3.4 客户端身份门控：UA、session 头、stream+tools、Cloudflare

公开源码中**找不到** `MissingSessionID`/`FreeTierError` 的抛出点——`routes/zen/` 下没有这两个错误类；结合 `handler.ts` 中 `console.*`/`inf.*` provider 前缀与新 inference 路由（`~/lib/inference-proxy.ts` 把请求转发到私有 `Resource.ConsoleMigration.inferenceUrl`），门控很可能已迁入**闭源 inference worker / Cloudflare 层**。[无法确认] 具体实现位置。可确认的实测行为：

- **UA 门控**（issue #42500，2026-08-14，实测）：匿名免费层只服务带 `User-Agent: opencode/<version>` 的请求；其它 OpenAI 兼容客户端（Cline/Roo/Continue/Aider）即使 `Bearer public` 也只得 `429 FreeUsageLimitError`；当时 `x-opencode-*` 头单独不能解锁。来源：[issue #42500](https://github.com/anomalyco/opencode/issues/42500)。
- **session 头**（zen-proxy README，实测）：随后缺 `x-opencode-session` 会返回 `400 MissingSessionID`；Go 付费档同样要求该头，但报错措辞不同——`400 MissingSessionID: "Request is missing x-opencode-session and cannot be routed efficiently"`（[issue #47763](https://github.com/anomalyco/opencode/issues/47763)，2026-09-07）。
- **stream+tools 校验**（zen-proxy README，实测）：免费层只接受「真实 agent 形态」的请求——`stream: true` 且 body 带真实 tool 定义；裸 curl/健康探测得 `403 FreeTierError`。详见 §3.4.1。
- **Cloudflare WAF**（issue #41320/#39374/#24284，实测）：`opencode.ai/zen/*` 前置 Cloudflare browser-signature 规则（error 1010），按 UA/TLS 指纹拦非 opencode 客户端；`codex-cli/*` UA 间歇性被 403，`Python-urllib` 直接 403，浏览器/curl UA 可通过。来源：[issue #41320](https://github.com/anomalyco/opencode/issues/41320)。

#### 3.4.1 免费层「客户端契约」实测细节（tools 匹配策略）

目前对门控最完整的公开实测来自 OmniRoute（一个自托管多渠道路由器）的 `open-sse/executors/opencodeFreeTierContract.ts` 与配套的 `opencodeRequestShape.ts`、`opencodeToolObservation.ts`。其文件头注释声明是 **2026-09-17~21 对线上 endpoint 在 3 个免费模型、Chat Completions 与 Responses 两个 surface 上的实测**。结论（来源：[opencodeFreeTierContract.ts](https://github.com/diegosouzapw/OmniRoute/blob/release/v3.8.51/open-sse/executors/opencodeFreeTierContract.ts)、[opencodeRequestShape.ts](https://github.com/diegosouzapw/OmniRoute/blob/release/v3.8.51/open-sse/executors/opencodeRequestShape.ts)、[opencodeToolObservation.ts](https://github.com/diegosouzapw/OmniRoute/blob/release/v3.8.51/open-sse/executors/opencodeToolObservation.ts)，均为第三方实测，非官方承诺）：

**四项硬性条件，缺一则 403 `FreeTierError`（"OpenCode's free tier can only be used from within OpenCode"）：**

1. **`stream: true`** —— body 必须流式；代理若收到客户端的非流式请求，需强制改写为流式并把 SSE 重组回 JSON（OmniRoute 的 `rebuildJsonFromForcedStream` 就是干这个的）。
2. **`tools` 数组非空** —— 但**有例外**：system prompt / `instructions` 里写明 "Never use tools" 的请求反而必须**不带** tools 才放行。即上游把「prompt 语义 ↔ tools 存在性」的一致性也纳入判定（OmniRoute 为此按 `provider|model|sha1(system prompt)` 做 shape 记忆与同请求双形态重放）。
3. **`x-opencode-session` 形状校验** —— 只查格式不查值：`ses_` + 12 hex + 14 base62（共 26 位后缀）；任意合规随机值可通过。zen-proxy 的实现是 `ses_` + 26 hex（`randomBytes(13).toString("hex")`），按客户端 IP 池化复用（[zen-proxy.mjs #L129-149](https://github.com/12errh/zen-proxy/blob/main/zen-proxy.mjs)）。
4. **`User-Agent: opencode/<version>`** —— 且版本号 ≥ 1.17；更旧的版本返回 **426 UpgradeRequired** 而非 403。

**tools 内容是按「名字」检查的，且规则按模型漂移、随时间变化：**

- 2026-09-18 实测：一个编造的工具名在 `big-pickle` 上被接受，但在 `nemotron-3.5-lightning-free` 和 `muse-spark-1.3-contributor-free` 上被拒——而 `big-pickle` 前一天刚接受过同名。
- 2026-09-21 实测：探索 agent 的 5 个真实工具名 `[glob, grep, read, webfetch, websearch]`（子集）在 `muse-spark-1.3-contributor-free` 上 11 次全拒——说明**不是简单「非空即过」**，而是要求声明中出现特定的（可能面向完整 opencode 工具集的）名字集合，且各模型阈值不同。
- 同一 body 数分钟内先拒后放的观测也存在（muse-spark，2026-09-18），说明判定含噪声/动态成分，不是静态白名单那么简单。
- `tool_choice` 只接受 `"auto"`：其它值返回 `400 invalid_request_error`（"only \"auto\" is supported for tool_choice"，2026-09-18 实测）。

**门控只作用于免费模型**：同 host 上的付费模型无 tools 请求会得到 `401 CreditsError` 而非 403。反过来，**付费 Go 面（`/zen/go/v1`）拒绝携带 tools 的请求**（"Endpoint is unavailable"，见 [issue #44300](https://github.com/anomalyco/opencode/issues/44300)、[#44382](https://github.com/anomalyco/opencode/issues/44382)）——两个 surface 的契约方向相反。

**opencode 客户端实际发送的工具集**（`packages/opencode/src/tool/registry.ts` + 各 `tool/*.ts` 的 `Tool.define` id）：`bash`（ShellTool，对外 id 固定为 bash）、`read`、`edit`、`write`、`glob`、`grep`、`task`、`todowrite`、`webfetch`、`websearch`、`question`、`plan_exit`、`lsp`、`apply_patch`、`skill`、`invalid`，外加 MCP/插件工具；按 agent/权限过滤（如启用 apply_patch 时去掉 edit/write）。tools 以**完整 JSON Schema**（AI SDK `inputSchema`，OpenAI 系模型强制 `strict: false`）随请求发送；`x-opencode-request` 头值为消息 id（`input.user.id` → `msg_*`）。来源：[registry.ts](https://github.com/anomalyco/opencode/blob/dev/packages/opencode/src/tool/registry.ts)、[llm/request.ts](https://github.com/anomalyco/opencode/blob/7945de208964a49300d7f770d1a71d078db9a4c4/packages/opencode/src/session/llm/request.ts)。

**公开源码中的位置**：`handler.ts`/`requestBody.ts`/`error.ts` 中**不存在** FreeTierError/MissingSessionID/tools 检查的任何代码；`handler.ts` 的提交历史里有连续的 `update inference headers` 与 `add client header replacement`（[commits](https://github.com/anomalyco/opencode/commits/dev/packages/console/app/src/routes/zen/util/handler.ts)），配合 `inference-proxy.ts` 把流量转向私有 `ConsoleMigration.inferenceUrl`，可确认该契约实施在**闭源 inference worker 层**，其具体判据（名字集合、prompt 一致性规则）[无法从一手源码确认]——以上全部依赖第三方实测，随时可变。

**对桥接实现的含义**（综合各项目做法）：UA 用 `opencode/<最新版本>`；session 头合成 `ses_`+随机后缀并按客户端粘住；`x-opencode-client: cli`、`x-opencode-request: msg_*`、`x-opencode-project` 一并携带；body 强制 `stream: true`；客户端已带 tools 时原样透传，没带时注入占位工具——OmniRoute 用 opencode 官方客户端同款的 `_noop` 占位名，并维护「哪个模型最近接受了哪些工具名」的观测表，被拒时把观测到的名字以空 parameters 形式追加进 tools 重放。zen-proxy 选择**不伪造** tool schema，直接把 FreeTierError 模型标记为 agent-only 交给真实 agent 流量验证。

**2026-09-16 起门控疑似下沉到 HTTP 层之下**：free-claude-code #1837 观测到头伪造（`x-opencode-session`/`x-opencode-client`/UA）整体失效，免费层改为要求「真实 OpenCode session」（[issue #1837](https://github.com/Alishahryar1/free-claude-code/issues/1837)）；issue #49621 的消除矩阵更激进——**逐字节重放真实客户端抓包请求（完整头序、官方 UA、真实 `ses_`/`msg_`、tools、stream）在 node/.NET/Python/Bun（含与官方完全相同的 Bun 1.3.14）上全部 403**，只有官方二进制本身返回 200。作者推断判别器在 HTTP 层之下：官方 build 的 TLS 握手指纹，或二进制内嵌的请求级秘密（[issue #49621](https://github.com/anomalyco/opencode/issues/49621)，closed，assignee fwang，无官方解释）。同期 opencode2api #19 报告自 2026-09-16 起免费模型全线 403/500 而官方客户端同 key 正常（[issue](https://github.com/6Kmfi6HP/opencode2api/issues/19)）。含义：§3.4.1 的 HTTP 层契约可能已变成**必要不充分**条件，纯 HTTP 栈桥接在 9 月中旬后大概率失效，剩下的可行路径是挂真实 `opencode serve` 会话（NeiP4n gist 的 opencode-llm-proxy 方案）或自带 key。

时间线（防护收紧）：早期仅 `Bearer public` + 无门控 → 服务端 `checkHeaders` 校验 → 2026-04-05 移除该层 header 校验（`zen: remove header check`）→ 2026-08 实测出现 UA 门控 → 之后增加 `x-opencode-session` 必填 → `stream:true`+tools 契约 + `tool_choice:"auto"` + UA≥1.17（§3.4.1）→ Cloudflare 1010 指纹拦截 → **2026-09-16~20 疑似 TLS/build 指纹或内嵌秘密，HTTP 层重放整体失效**。**这是快速演进的对抗面，桥接方案随时可能失效。**

#### 3.4.2 本仓库实测（2026-09-29，curl/schannel，Windows）

对 `opencode.ai/zen/v1` 直接探测（每个条件各 1 次请求，Bearer public）：

| 请求 | 结果 |
|---|---|
| `chat/completions` big-pickle，无任何 opencode 头、非流式、无 tools | `403 FreeTierError` |
| 同上 + 全套头（UA `opencode/1.18.31`、`x-opencode-session: ses_*`、client/request/project）+ `stream:true` + tools `[bash,read]` | **200**，正常 SSE 流式返回 |
| 同上但去掉 tools | `403 FreeTierError` |
| 同上但 `stream:false` | `403 FreeTierError` |
| `chat/completions` nemotron-3.5-lightning-free，全套头 + stream + tools `[bash,read]` | **200**（两个工具名即可，未触发按名拒绝） |
| `responses` muse-spark-1.3-contributor-free，全套头 + stream + tools `[bash]`（Responses 扁平 tools 形态） | `403 FreeTierError` |

结论：**契约门控当前在 `chat/completions` 面仍按 §3.4.1 生效，且满足后纯 HTTP 栈（非 Bun、非官方 TLS 指纹）可通过**——#49621 的「HTTP 层之下判别器」观测应限定于 `/responses` 面或 muse-spark 系模型；该面同契约仍 403，与 issue 一致。即可用性按 endpoint 分裂：`chat/completions` 系免费模型可桥接，`responses` 系（muse-spark 等）需要真实 opencode 会话或自带 key。

tools 检查对象的细化（同日实测，模型 big-pickle）：单工具 `bash`（完整 schema）→ 403；两工具 `[bash, read]`（完整 schema）→ 200；**两个编造名 + 完整 schema → 403**；两个真名裸声明（无 description/parameters）→ **400 invalid_request_error**（即已通过 FreeTierError 门，被上游 schema 校验拒）。推断：门控检查的是**工具名集合**（须含 opencode 真实工具名、单一名字不够，疑似要求 ≥2 或特定组合），**不校验 schema 内容**——schema 合法性由上游 provider 校验兜底。伪造 tools 时名字必须用真名，struct 只需合法、不需要仿真 opencode 的 schema。

### 3.5 区域与其它限制

- 按 `cf-ipcountry` 类来源做模型级区域限制（`isModelCountryRestricted` → `RegionError`）；Go 档对 deepseek 系列要求 workspace 区域含 `cn` 的显式 opt-in（[issue #39872](https://github.com/anomalyco/opencode/issues/39872)）。
- 另有 trial-provider 限流（`trialLimiter`）、模型级 TPM/TPS 限流（`modelTpmLimiter`/`modelTpsLimiter`）、provider 预算追踪（`providerBudgetTracker`）、sticky provider（按 sessionId/workspaceID/IP 粘路由）。来源：[handler.ts @7945de2](https://github.com/anomalyco/opencode/blob/7945de208964a49300d7f770d1a71d078db9a4c4/packages/console/app/src/routes/zen/util/handler.ts)。

## 4. 付费侧速查（对照）

| 机制 | 说明 | 来源 |
|---|---|---|
| 计费 | 按 1M token 计 input/output/cached，auto-reload（余额 < $5 充 $20，可关） | [docs/zen](https://opencode.ai/docs/zen/) |
| 限额 | workspace/成员级月度限额 | 同上 |
| BYOK | 可带自己的 OpenAI/Anthropic key，上游直接计费 | 同上；handler.ts `validateBilling` "byok" |
| Teams | workspace + admin/member 角色 + 模型开关（beta 期免费） | 同上 |
| Go 订阅 | $10/月，独立 `/zen/go/v1` 前缀，`modelList: "lite"` 路径 | [docs/go](https://opencode.ai/docs/go/)、handler.ts |

## 5. 市面第三方桥接/蹭用项目

| 项目 | 接入方式 | 状态/备注 |
|---|---|---|
| [12errh/zen-proxy](https://github.com/12errh/zen-proxy)（26★，MIT，单文件 `zen-proxy.mjs`） | 调 `opencode.ai/zen/v1`，注入 `Bearer public` + `User-Agent: opencode/<latest>`（自动跟踪 opencode release 更新 UA）+ 每客户端合成 `x-opencode-session`，转发真实客户端 IP；暴露 `/v1/chat/completions`、`/v1/models`、`/v1/responses`；带 dashboard、模型别名、429/5xx 自动 fallback、BYOK Zen key | 活跃（GitHub Action 每日同步免费模型清单）。作者即 issue #42500 提交者。明确声明**不伪造 tool schema 过 FreeTierError 门** |
| [bigdata2211it-web/opencode-free-proxy](https://github.com/bigdata2211it-web/opencode-free-proxy)（112★） | 同思路：逆向 opencode 二进制得到头集合 `Bearer public` + `User-Agent: opencode/1.15.0 ai-sdk/provider-utils/4.0.23 runtime/bun/1.3.13` + `x-opencode-client/request/session/project`；同时暴露 OpenAI + Anthropic 格式，本地 api-keys.json 鉴权 | 维护中；UA 串固定旧版，UA 收紧后可能需更新 |
| [Maicon501a/opencode-zen-proxy](https://github.com/Maicon501a/opencode-zen-proxy)（5★，MIT，Express） | keyless：直接转发到 Zen 并注入 `Bearer public`，剥 `opencode/` 前缀，透传 tools/stream；附详尽 [REVERSE_ENGINEERING.md](https://github.com/Maicon501a/opencode-zen-proxy/blob/main/docs/REVERSE_ENGINEERING.md)（2026-06-27） | README 写明其动机是绕过 `opencode serve` 本地代理层对 tool calls 的吞掉问题；未处理 UA/session 门控，在门控收紧后可能已部分失效 [无法确认当前可用性] |
| [lumishoang/opencode-proxy](https://github.com/lumishoang/opencode-proxy)（3★，MIT） | 面向 **OpenCode Go**（付费档）：用户自己的 `OPENCODE_GO_API_KEY` 调 `zen/go/v1`，代理成 OpenAI 格式给 OpenClaw；维护 session-id 缓存 | 非蹭免费层，是「自己的 key + 协议转换」，合规风险低 |
| [PandaDecSt/opencodeProxy](https://github.com/PandaDecSt/opencodeProxy)（1★，TS） | 免费模型 → OpenAI+Anthropic API，宣称 No API key required | 低 star，TECHNICAL.md 当前 404，细节 [无法确认] |
| [thelabcorner/opencode-zen-fut-api](https://github.com/thelabcorner/opencode-zen-fut-api) | 不是代理：从源码重建免费限流算法的**配额估算 API**，靠本地观测而非服务端查询 | 对限流机制最严谨的一手分析 |
| [diegosouzapw/OmniRoute](https://github.com/diegosouzapw/OmniRoute)（自托管多渠道路由器，`opencode` keyless provider） | 实现最完整的免费层契约：`open-sse/executors/opencode*.ts` 一系列模块负责 UA/session 头、`stream:true` 强制+SSE→JSON 重组、`_noop` 占位工具注入、「模型→已接受工具名」观测表与 bare/tools 双形态重放 | 维护活跃；其代码注释是目前公开渠道中对门控最细粒度的实测记录（见 §3.4.1） |
| [GuJi08233/opencode-free-gate](https://github.com/GuJi08233/opencode-free-gate)（13★，Go，Docker） | 全套 opencode 头 + **公共代理池轮换出口 IP** 规避按 IP 日配额（rendezvous 哈希粘会话），另有 ZenProxy relay（zenproxy.top）付费回退层；暴露 OpenAI/Anthropic/Codex 三协议，仅展示 `-free` 模型 + big-pickle | 维护中；IP 池轮换直接对应 §3.3 的 IP 限流，属多出口绕过，ToS 风险更高 |
| [opencode-llm-proxy](https://www.npmjs.com/package/opencode-llm-proxy)（npm 包）+ `opencode serve` | 不伪造客户端：本地跑真实 `opencode serve`（即有真实会话与官方 TLS 栈），用 `@opencode-ai/sdk` 驱动它，对外暴露 OpenAI 兼容 `/v1/chat/completions` | 对 §3.4.2 中 responses 面级别的指纹门控最鲁棒；NeiP4n gist 验证方案（2026-09-12） |
| `s12ryt/s12ryt-oc2api` | 声称直调 Zen 转 OpenAI Chat/Responses + Anthropic Messages | 仓库当前 404（已删除/改名/隐藏） |
| tungcorn/CLIProxyAPI 等 fork | 针对 OpenCode Zen 的 DeepSeek 推理模型做兼容优化（reasoning 字段） | 属「客户端侧兼容」，不是匿名桥接 |
| one-api/new-api | [无法确认] 官方渠道；社区做法是把 Zen 当普通 OpenAI-compatible 渠道 + 自备 key 或经上述代理 | — |

共同技术栈：Zen 本身就是 OpenAI 兼容 API，「桥接」的核心不是协议转换而是**身份伪装**：`Bearer public` + opencode UA + `x-opencode-session`/`x-opencode-client`/`x-opencode-request`/`x-opencode-project`，以及近来要求的 `stream:true`+tools 请求形态与可通过 Cloudflare 1010 的客户端指纹。

## 6. 隐私代价（为什么免费）

官方文档明示的例外条款（数据可能被用于训练）：Big Pickle、MiMo-V2.5/V2.6-Flash Free、Ling 3.0 Flash Fin Free、Nemotron 3 Ultra / 3.5 Lightning Free（NVIDIA API Trial Terms，明确「do not submit personal or confidential data」）、Muse Spark 1.3 Contributor Free（以价换 Meta 训练授权）；Space Bunny、LongCat 宣称 zero-retention。来源：[docs/zen Privacy](https://opencode.ai/docs/zen/)。

## 7. 风险与合规

### 7.1 ToS（生效 2026-08-15）

直接相关的条款（[opencode.ai/legal/terms-of-service](https://opencode.ai/legal/terms-of-service)）：

- 「only use the Services for your own internal use, and not on behalf of or for the benefit of any third party」——**把免费层反代出去服务他人**直接触碰此条；本地自用桥接是灰区；
- 禁止「accounts in bulk / multiple accounts to circumvent usage limits, access restrictions, billing obligations…」——多账号/多 IP 池放大免费额度明确违规；
- 禁止「automatically or programmatically extracts data or Output」「crawls/scrapes」「processes that run while you are not logged in」「unreasonable load」；
- 禁止「decompiles, reverse engineers… of the Services」——注意：opencode **开源客户端本身**按仓库 license 管，但 Zen 托管服务的逆向在禁止之列；
- 违约即可被终止访问，OpenCode 有单方裁量权；且有仲裁+集体诉讼弃权条款。

### 7.2 封禁/对抗机制史

- workspace 级封禁：`isBlocked`、`is_flagged_by_anthropic/openai`（见 §3.2），曾有用户因第三方 agent（hermes 等）流量被上游标记的社区报告（[reddit 讨论](https://www.reddit.com/r/opencodeCLI/comments/1w43z9q/)，二手线索，具体判据 [无法确认]）；
- 匿名层的「封禁」是 IP/指纹维度：IP 日配额耗尽（429 + Retry-After 至 UTC 零点）、UA/session/agent 形态门控（400/403）、Cloudflare 1010 指纹拦截；
- 2026-08-12 前后免费额度明显收紧（多 issue/reddit 报告同一 IP 上部分模型单独 429，符合 `rateLimit` per-模型 override 机制）。来源：[RESEARCH.md §6](https://github.com/thelabcorner/opencode-zen-fut-api/blob/main/RESEARCH.md)、[issue #42074](https://github.com/anomalyco/opencode/issues/42074)、[#42977](https://github.com/anomalyco/opencode/issues/42977)；
- 官方态度：issue #42500 直接询问「免费层是否可对第三方客户端开放」，截至读取时 **closed 且无官方人工回复**（仅 bot 标重复）——即官方未认可该用法，门控持续存在即答案。

### 7.3 稳定性风险结论

免费层是推广性质的共享池：模型名单月级别轮换（历史上的 grok-code-fast-1、code-supernova、qwen3-coder、minimax-m2 系已陆续退出或转付费），配额数值在私有 secret 中可随时改，门控规则在闭源层快速演进。**任何建立在匿名免费层上的接入都应视为易碎的临时方案**，对 Stravia 而言更稳妥的路径是：用户自带 Zen key（BYOK，文档化的正常用法）或 Go 订阅 key，把匿名免费层仅作为 fallback/试用通道并明确标注 ToS 风险。

### 7.4 本仓库实现

专属插件 `stravia-vendor-opencode-free`（`provider_id = "opencode-free"`，dedicated，本地导入分发）实现免费层契约：命中免费模型（`-free` 后缀或 `big-pickle`）→ 强制 `stream: true`、注入 `bash`/`read` 占位工具（客户端已声明同名则保留客户端版本）、占位名写入 instructions 禁令、`tool_choice` 归一为 auto；指纹头（UA / `x-opencode-*`）对全部模型发送与真实客户端一致，无 apiKey 时 `Bearer public`；discovery 只暴露免费模型；`userAgent` 配置字段可覆盖 UA 版本。与 base 的 `opencode` Profile 并存不接管，`opencode-go` 付费面不受影响。组件级合同测试见 `tests/contract.rs`（`cargo test -p stravia-vendor-opencode-free --test contract -- --ignored`，需先 `task build:vendors:all`）。

## 附：主要来源

- 官方：[Zen 文档](https://opencode.ai/docs/zen/)、[ToS](https://opencode.ai/legal/terms-of-service)、[Go 文档](https://opencode.ai/docs/go/)
- 源码（dev@7945de2）：[provider.ts](https://github.com/anomalyco/opencode/blob/7945de208964a49300d7f770d1a71d078db9a4c4/packages/opencode/src/provider/provider.ts)、[llm/request.ts](https://github.com/anomalyco/opencode/blob/7945de208964a49300d7f770d1a71d078db9a4c4/packages/opencode/src/session/llm/request.ts)、[zen/util/handler.ts](https://github.com/anomalyco/opencode/blob/7945de208964a49300d7f770d1a71d078db9a4c4/packages/console/app/src/routes/zen/util/handler.ts)、[ipRateLimiter.ts](https://github.com/anomalyco/opencode/blob/7945de208964a49300d7f770d1a71d078db9a4c4/packages/console/app/src/routes/zen/util/ipRateLimiter.ts)、[subscription.ts](https://github.com/anomalyco/opencode/blob/7945de208964a49300d7f770d1a71d078db9a4c4/packages/console/core/src/subscription.ts)、[model.ts](https://github.com/anomalyco/opencode/blob/7945de208964a49300d7f770d1a71d078db9a4c4/packages/console/core/src/model.ts)、[inference-proxy.ts](https://github.com/anomalyco/opencode/blob/dev/packages/console/app/src/lib/inference-proxy.ts)
- Issues：[#42500 UA 门控](https://github.com/anomalyco/opencode/issues/42500)、[#41320 Cloudflare 1010](https://github.com/anomalyco/opencode/issues/41320)、[#39872 Go 区域限制](https://github.com/anomalyco/opencode/issues/39872)、[#47763 MissingSessionID](https://github.com/anomalyco/opencode/issues/47763)、[#44300](https://github.com/anomalyco/opencode/issues/44300)/[#44382](https://github.com/anomalyco/opencode/issues/44382)（Go 面带 tools 被拒）
- 第三方：[12errh/zen-proxy](https://github.com/12errh/zen-proxy)、[bigdata2211it-web/opencode-free-proxy](https://github.com/bigdata2211it-web/opencode-free-proxy)、[Maicon501a/opencode-zen-proxy](https://github.com/Maicon501a/opencode-zen-proxy)（含 [REVERSE_ENGINEERING.md](https://github.com/Maicon501a/opencode-zen-proxy/blob/main/docs/REVERSE_ENGINEERING.md)）、[lumishoang/opencode-proxy](https://github.com/lumishoang/opencode-proxy)、[thelabcorner/opencode-zen-fut-api](https://github.com/thelabcorner/opencode-zen-fut-api)、[PandaDecSt/opencodeProxy](https://github.com/PandaDecSt/opencodeProxy)
