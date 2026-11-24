---
status: accepted
---

# 收敛思考等级映射为 Effort/Hidden 并移除系统内部 Budget 计算

此前核心 IR（`ReasoningConfig`）与思考等级映射（`TargetThinkingControl`）同时混合了 `Effort`、`Budget`、`Enabled`、`Disabled`、`Hidden` 以及 `mode`、`budget_tokens` 和 `MODEL_CAPABILITY_THINKING_TOGGLE` 能力标志。模型厂商已全面转向基于等级／努力度（`effort` / `thinkingLevel` / `adaptive`）的思考控制接口，在网关内部维护从 token 预算反推等级、开关授权旁路和多态映射控制增加了不必要的配置复杂度和协议耦合。

## 决策

- **核心契约收敛**：`TargetThinkingControl` 仅保留 `Effort { value }` 与 `Hidden` 两种变体；`ReasoningConfig` 移除 `mode` 与 `budget_tokens`，仅保留 `level`、`summary`、`target_control`；删除 `ThinkingLevel::from_budget` 与 `MODEL_CAPABILITY_THINKING_TOGGLE` 及宿主中的 `toggle_declared` 授权旁路。
- **入站 fail-closed 拒绝旧预算语法（显式偏离 ADR-0081 静默丢弃规则）**：
  - Anthropic Messages 收到 `thinking.type = "enabled"` 或非空 `thinking.budget_tokens` 时返回 `400 Bad Request`，提示改用 `thinking: { "type": "adaptive" }` + `output_config.effort` 或 `thinking: { "type": "disabled" }`；省略 `output_config.effort` 的 `adaptive` 解码为 `high`；`disabled` 解码为 `off` 且拒绝搭配非关闭 `effort`。
  - Google Gemini 收到非空 `thinkingConfig.thinkingBudget` 时返回 `400 Bad Request`，提示改用 `thinkingConfig.thinkingLevel`。
  - 两协议中的显式 `null` 重置继续记录到 `ParameterReset`，不触发拒绝。
  - 选择 fail-closed 而非静默丢弃的原因：既符合 [ADR-0006](0006-own-protocol-conversion-behind-canonical-stages.md) 对废弃／非法输入显式报错的原则，也避免静默忽略 `thinkingBudget: 0` 等关闭思考指令导致意外产生高额思考开销。
- **旧型号预算与开关型供应商下沉到出口层**：
  - 统一静态出口预算表仅存在于 `stravia-protocol-codec` 出口层（`minimal: 1024`, `low: 2048`, `medium: 8192`, `high: 16384`, `xhigh: 24576`, `max: 32768`），绝不进入核心 IR 或管理界面。
  - Anthropic Messages、官方 Anthropic 插件、Claude Code 与 Bedrock Claude 共用型号代际规则（自适应 vs 手动静态预算、常开型号安全降级 `effort: "low"`、Sonnet ≥ 5.5 `between_tools`、显式与自动 `max_tokens` 缓冲约束）；删除 Claude Code 中的 `effort_from_budget`。
  - Google Gemini 出口保证 `thinkingLevel` 与 `thinkingBudget` 严格互斥：Gemini 3+ 与未知型号走 `thinkingLevel`（支持关闭时 `thinkingBudget: 0`，Pro 不支持关闭时安全降级为 `LOW`）；Gemini 2.5 按静态预算表输出 `thinkingBudget`（Pro 上限 32768、关闭降级 128；Flash 上限 24576、关闭为 0）。
  - OpenAI-compatible 开关型供应商（DeepSeek、Xiaomi、Zhipu/ZAI、Alibaba/Qwen、Baseten、MiniMax M3、特定 OpenCode 模型）与纯开关型 Cohere/Kiro 模型在发现阶段无显式声明时默认注入 `reasoning_efforts: ["none", "high"]`，出口直接将 `Effort` 翻译为对应开关或静态预算。
- **严格校验与兼容性**：
  - Provider Model 声明 `reasoning_efforts` 时作为严格白名单，未声明时回退到 7 个标准 effort 协议基线。
  - 历史生成链快照通过 runtime-contract 私有 legacy DTO 无损读取旧字段；`GENERATION_PREFIX_NAMESPACE` 升级为 `stravia-generation-history-v6:`；Vendor SDK format-1 兼容投影维持既有 WIT 版本不变。
  - SQLite 与 PostgreSQL 同步迁移 `0059_simplify_thinking_level_map.sql` 原地将 `model_backends.thinking_level_map` 中的 `disabled` / `budget(0)` 转为 `effort("none")`，`enabled` / `budget(>0)` 转为对应 `level` 的 `effort`（`off` 为 `"none"`），不修改表结构与外键，SQLite 全程保持 `PRAGMA foreign_keys = ON`。

## Considered Options

- **在入口把 `budget_tokens` / `thinkingBudget` 静默换算或静默丢弃。** 对旧客户端表面兼容，但保留了不可控的预算猜测逻辑，且静默忽略关闭思考意图会造成高成本或隐蔽语义偏移。
- **允许管理员在界面按 Target 自定义旧型号 token 预算数值。** 重新把协议代际差异暴露给所有路由配置，与本次收敛映射心智模型的目标背道而驰。

## Consequences

- 仍发送 Anthropic `thinking.type = "enabled"` / `budget_tokens` 或 Gemini `thinkingBudget` 的下游客户端需要升级为 `adaptive` + `output_config.effort` 或 `thinkingLevel`，否则收到 `400` 与迁移指引。
- 升级后首个续接请求因 `stravia-generation-history-v6:` 命名空间变更需重建一次可复用前缀。
- 独立分发的 Wasm Vendor 插件需要与宿主同步重建并重新导入。
- 历史迁移后若某行转换出的 effort 值不在模型严格 `reasoning_efforts` 白名单中，Route 加载与运行不受阻（运行时自动跳过并 `clamp`），管理界面高亮提示管理员选择受支持值或重置。
