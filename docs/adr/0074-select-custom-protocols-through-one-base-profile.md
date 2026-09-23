---
status: accepted
---

# 通过单一 base Custom Profile 选择已声明协议

`stravia-vendor-base` 注册单一 `custom` Provider Profile，并完整声明管理员可选择的上游协议；管理面从插件声明生成选项，不硬编码协议清单，也不接受任意协议字符串。该 Profile 可以选择 base 已实现的 OpenAI-compatible、Open Responses、Anthropic Messages、Gemini 等协议；专属 Vendor 的私有协议不自动加入。该形状由 base 单独拥有，不引入跨插件 Profile 合并，符合 [ADR-0067](0067-separate-vendor-identity-from-package-and-protocol.md) 的身份与整体接管规则。

现有四个 `protocol-*` 独立 Profile 入口并入 `custom`。迁移已有连接时保留连接 UUID、凭据与 Route，不因入口合并重建业务身份；具体选择标识、插件契约版本及迁移 SQL 是满足该迁移约束的实现选择，本轮不另设产品策略。除本文明确变更外，行为遵循 [ADR-0073 定义的插件化迁移前原生基线](0073-register-runtime-catalog-profiles-through-base-vendor.md#迁移基线)。
