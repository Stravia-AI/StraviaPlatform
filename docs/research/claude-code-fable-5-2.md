# Claude Code 2.1.323 与 Fable 5.2、Sonnet 5.5、Opus 5.5 调用对齐

调研与采集日期：2026-11-13。对应实现：`backend/crates/stravia-vendor-claudecode/`。官方能力说明、原生客户端观测和 Stravia 实测分别记录，不能互相替代。

## 版本与模型身份

- [官方 GitHub latest API](https://api.github.com/repos/anthropics/claude-code/releases/latest)、[固定版本发布页](https://github.com/anthropics/claude-code/releases/tag/v2.1.323)与 [npm latest 元数据](https://registry.npmjs.org/@anthropic-ai/claude-code/latest)在调研时均指向 **2.1.323**。已下载运行的 `claude --version` 同样报告 2.1.323；不保证未来 latest 不变。
- [Fable 5.2 官方概览](https://platform.claude.com/docs/en/models/fable-5-2/overview)给出的 Claude API 模型 ID 是 **`claude-fable-5-2`**，发布日期为 2026-11-09。该代无日期 ID 是固定快照，不是滚动别名；不同平台的 ID 不应混用，见[模型列表](https://platform.claude.com/docs/en/models/overview)及[平台可用性](https://platform.claude.com/docs/en/models/fable-5-2/whats-new-fable-5-2#availability)。
- 官方规格为文本/图片输入、文本输出、1M 上下文与同步 Messages API 最大 128K 输出。API 默认 effort 为 `high`；这不等于 Claude Code 交互默认值，也不证明某个账号必然可用。

## 官方请求契约

依据 [Fable 5.2 thinking 与 tool-choice 说明](https://platform.claude.com/docs/en/models/fable-5-2/whats-new-fable-5-2#thinking-and-tool-choice)：

- Adaptive thinking 常开；省略 `thinking` 或传 `{"type":"adaptive"}`，不能发送 `disabled` 或手动 `enabled` + `budget_tokens`。
- 支持 `low`、`medium`、`high`、`xhigh`、`max` effort；强制 `any` 或具名工具选择会返回 400，自动选择可用。
- 不发送不受支持的采样参数，具体边界见[采样约束](https://platform.claude.com/docs/en/build-with-claude/thinking#sampling-parameters)。现有插件的 adaptive-only、采样投影与强制工具处理继续适用，本轮不建立平行规则。
- 保留 thinking block 时，改写前面的 system、tools 或历史消息可能触发前缀校验 400，见[前缀约束](https://platform.claude.com/docs/en/models/fable-5-2/whats-new-fable-5-2#editing-earlier-turns-invalidates-thinking-blocks)。本轮不新增丢弃思考或恢复重试。
- [思考块兼容说明](https://platform.claude.com/docs/en/models/fable-5-2/whats-new-fable-5-2#thinking-blocks-are-tied-to-the-model-that-produced-them)的 Fable/Mythos 5.2 互读规则受平台和账号边界约束，不意味着跨任意模型或账号可重放。
- [模型专属数据保留规则](https://platform.claude.com/docs/en/manage-claude/api-and-data-retention#model-specific-data-retention-requirements)要求 Fable 5.2 保留数据 30 天，除非获得 Anthropic 明确授权，否则不适用于零数据保留。部署方需单独核对适用政策。

## 原生客户端实测

使用隔离 HOME、仅当前 access token，不使用或刷新 refresh token。未覆盖 endpoint，通过 loopback HTTPS CONNECT 捕获第一方 `api.anthropic.com` 请求，转发真实上游。交互命令 `claude --model claude-fable-5-2 --effort medium` 返回 HTTP 200 SSE，并以 `message_stop` 结束。完整脱敏请求头和 ClientHello 保存在 [Fable 5.2 fixture](../../backend/crates/stravia-core/src/plugin/network/shape/fixtures/claude-code-fable-5-2.json)。

观测到：

|字段|原生值|
|---|---|
|User-Agent|`claude-cli/2.1.323 (external, cli)`|
|SDK / runtime|`0.128.0` / `v26.12.1`|
|model / max_tokens|`claude-fable-5-2` / `128000`|
|thinking|`{"type":"adaptive","display":"updates"}`|
|output_config|`{"effort":"medium"}`|
|context_management|`clear_thinking_20251015`，`keep: all`|
|请求类别|`x-claude-1p-request-class: main` 与 `x-claude-code-request-class: main`|

完整 ClientHello 与历史 2.1.319 交互 fixture 相同，JA4 为 `t13d1713h1_5b57614c22b0_7f527383e249`，因此不修改 `claude-code` shape 的连接修订。请求包含 Fable 5.2 的 `per-turn-control-2026-07-01` 与 `mid-conversation-tool-changes-2026-07-01`；插件能力表补入该明确版本，不推测后续型号。

原生二进制的请求类别头由 `CLAUDE_CODE_GATEWAY_HINT_HEADERS`、gateway 模式或第一方 rollout 开关控制，并按调用来源计算类别。本次两种入口均观测到 `main`。插件延续已有 main 推理 profile，补齐第一方类别头；不能把此观测描述为所有原生会话必发。

另一次 `--print` 调用也成功，但 entrypoint 为 `sdk-cli`，thinking display 为 `omitted`，不能拿它代替交互基准。交互样本额外带有 `mid-conversation-system-clear`、`dangerous-tool-use`、`afk` beta，以及 `safeguards: dangerous_tool_use`；这些依赖功能或会话状态，不无条件复制。CLI 的内置工具与 system prompt 也不替换 Stravia 调用者提供的工具与提示。

账号 bootstrap settings 请求曾返回 404，policy limits 为 200、随后 304；没有据此推断完整远端 feature assignment。此记录仅覆盖一个账号、Linux x86_64 及采集入口，不承诺所有账号、平台、TLS 时序或功能组合一致。

## Stravia 真实对照与回归

在全新 SQLite 中导入实际构建的 Wasm，仅使用只读提取的 access token，不保存刷新令牌。通过真实 Gateway `/v1/responses` 与宿主 TLS 访问同一第一方端点，Target retry budget 为 0；请求输出为 `OK`，HTTP 200。

初轮手工模型缺少原生 `max_tokens` 元数据，按既有未知型号回退发出 64000；不能用旧 `limit.output` 字段代替原生上限。改为真实 `/v1/models` 同步后，还复现了 `ROUTE_OUTPUT_MODALITY_MISMATCH`：发现结果未声明文本输出，而共享目录尚未收录新型号。修复在插件发现投影声明 Messages 文本输入/输出，图片与 PDF 只在上游明确支持时加入，不硬编码 Fable 名称。新增模态回归先失败后通过。

最终从真实模型同步、创建 Route 到推理完整通过。原生目录声明的 `max_tokens=128000` 实际进入请求；模型、max_tokens、thinking、output_config、context_management、stream 均与原生交互样本相同。完整 ClientHello、请求头顺序及非动态值相同。原生额外的 advanced-tool-use、afk、dangerous-tool-use、mid-conversation-system-clear beta 不存在于本次无工具、无相关功能的 Stravia 请求；没有 Stravia 独有 beta。system、tools、账号/会话标识和 billing 动态字段不做逐字相等承诺。

执行 `cargo test --locked -p stravia-vendor-claudecode --lib --test contract`：92 个库测试与 19 个 contract 通过；实际 Gateway TLS/header 验收、all-target Clippy（`-D warnings`）与 `cargo fmt --all -- --check` 通过。真实调用使用一次性 Gateway smoke 入口；不是用 mock 响应替代。本节 Fable 验证未覆盖其他账号、全部 effort、长历史思考重放、UI 或 Tauri。上线需要重新构建、导入 Claude Code Wasm 并同步模型。

## Sonnet 5.5 与 Opus 5.5 补充实测

同日另用固定版本 2.1.323 的交互 PTY 采集 `claude --model sonnet` 和 `claude --model opus`，分别省略 effort 以及显式传入 `--effort low|medium|high|xhigh|max`。原生别名在本次第一方账号上实际解析为 `claude-sonnet-5-5`、`claude-opus-5-5`；不把当前别名映射推广到其他提供商、覆盖配置或未来版本。官方[模型别名与 effort 配置](https://code.claude.com/docs/en/model-config)也说明别名会变化，Claude Code 的默认 effort 为 medium；[Messages API effort](https://platform.claude.com/docs/en/build-with-claude/effort)中的 API 默认是独立契约，不能用 Sonnet 的 API 默认 high 替代 CLI 默认。

使用只读 access-token 快照，不读取刷新令牌、不刷新凭据、不覆盖端点；每个 CLI session 独立启动，使用任务专属 HOME 和空工作区。主请求通过本地 HTTPS CONNECT 桥访问真实 `api.anthropic.com`，仅为这些隔离进程配置本地 CA。`platform.claude.com` 启动检查采用保持端到端 TLS 的普通隧道。初次代理环境大小写变量未隔离造成绕过捕获，修正后又补齐该启动检查的隧道许可；早先的屏幕成功或启动失败不作为协议证据，下述矩阵仅使用修正后的实际捕获。

|字段或场景|Sonnet 5.5|Opus 5.5|
|---|---|---|
|实际 model|`claude-sonnet-5-5`|`claude-opus-5-5`|
|省略 effort|显式发送 `medium`|显式发送 `medium`|
|显式 effort|low、medium、high、xhigh、max 均保留|low、medium、high、xhigh、max 均保留|
|max_tokens|128000|128000|
|thinking|`{"type":"adaptive","display":"updates"}`|同左|
|context_management|`clear_thinking_20251015`，`keep: all`|同左|
|stream|true|true|
|原生六档结果|全部 HTTP 200、SSE `message_stop`、`OK`|全部 HTTP 200、SSE `message_stop`、`OK`|
|Gateway 六档结果|全部 HTTP 200、`OK`|全部 HTTP 200、`OK`|

独立 medium 捕获分别保存为 [Sonnet fixture](../../backend/crates/stravia-core/src/plugin/network/shape/fixtures/claude-code-sonnet-2-1-323.json) 和 [Opus fixture](../../backend/crates/stravia-core/src/plugin/network/shape/fixtures/claude-code-opus-2-1-323.json)，不是将 Fable 的型号名替换后生成。两份 fixture 保留完整脱敏 ClientHello、请求头顺序及静态值、正文控制字段和响应完成性；账号、会话、凭据、原始提示及工具正文不保留。请求类别头的已观测静态值 `main` 保留。

Stravia 对照使用实际发行 Wasm、全新 SQLite、真实 `/v1/models` 同步及 Route 创建，再经 Gateway `/v1/responses` 与宿主 TLS 访问同一上游，Target retry budget 为 0。每个型号按默认、low、medium、high、xhigh、max 顺序调用，未指定客户端输出上限；原生目录的 `max_tokens=128000` 因此实际参与推理。首轮 Sonnet 六档已完成后，临时验证器复用本地 API Key 名称而报 `API_KEY_NAME_CONFLICT`，尚未发送 Opus 推理；改为按型号唯一命名后单独完成 Opus 六档。该验证器错误不是产品或上游模型失败。

逐档比较结果：model、max_tokens、thinking、output_config、context_management、stream 全部相等；完整 ClientHello、请求头名称顺序和非动态值相等。JA4 均为 `t13d1713h1_5b57614c22b0_7f527383e249`，与已有 Fable 样本相同，无需调整宿主 shape 修订。原生 beta 列表比本次无工具、无相关会话功能的 Stravia 请求多 `advanced-tool-use-2025-11-20`、`afk-mode-2026-01-31`、`dangerous-tool-use-2026-09-03`、`mid-conversation-system-clear-at-2026-08-21`；排除这四项后，beta 序列也逐项相等，无 Stravia 独有项。不复制原生 system prompt、内置 tools 或 safeguards，不宣称完整正文逐字相等。

现有共享实现已满足本次 Sonnet/Opus 对照，没有追加型号特判或重复的永久测试行；现有 `inference_defaults_follow_model_effort_and_beta_capabilities` 已覆盖两型号的默认 effort 和能力 beta。本次新增证据是两份独立原生 fixture 与十二组真实逐档对照，而非将 Fable 结果外推。重新执行 `cargo run --locked -p stravia-vendor-base --bin stravia-build-vendors -- --all` 成功，Claude 组件摘要与实际调用时一致。验证后删除临时 Gateway 入口、原始私有流量、安装目录、凭据副本和本地 CA，并停止采集服务。

范围仅为一个账号、Linux x86_64、交互入口的单轮文本推理；不覆盖其他代际 Sonnet/Opus、关闭思考或手动预算、强制工具选择、多轮签名重放、其他账号与平台、UI/Tauri。未将当前官方 [thinking 能力边界](https://platform.claude.com/docs/en/build-with-claude/thinking)当作这些未执行场景的运行证明。
