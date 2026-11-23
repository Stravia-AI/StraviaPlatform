---
status: accepted
---

# 通过单一 base Custom Profile 选择已声明协议

`stravia-vendor-base` 注册单一 `custom` Provider Profile，并完整声明管理员可选择的上游协议；管理面从插件声明生成选项，不硬编码协议清单，也不接受任意协议字符串。该 Profile 可以选择 base 已实现的 OpenAI-compatible、Open Responses、Anthropic Messages、Gemini 等协议；专属 Vendor 的私有协议不自动加入。该形状由 base 单独拥有，不引入跨插件 Profile 合并，符合 [ADR-0067](0067-separate-vendor-identity-from-package-and-protocol.md) 的身份与整体接管规则。

合并曾按协议拆分的入口时保留连接 UUID、凭据与 Route，不因入口改变重建业务身份。现行自定义协议及 Profile 契约见 [Vendor Wasm 插件设计](../design/vendor-plugins.md)。
