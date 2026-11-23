---
status: accepted
---

# Vendor 身份独立于实现包与协议

Vendor 是独立、稳定的实现身份，不由 npm package、Provider Catalog 条目或 Protocol 推断。一个软件包对应一个 Vendor 身份，而不是一个供应商入口：`base` 通过多个 Provider Profile 提供通用接入，专属 Vendor 用自己的身份接管所属 Profile。普通 OpenAI 与 xAI API 接入与 Codex、Grok 专属接入不因共享协议或目录而合并。

Profile ID 标识供应商接入；目录 ID 用于关联目录；已保存 Provider 连接的 UUID 是另一身份。更换实现、共享 codec 或升级插件不应重建连接身份。专属 Profile 的全部操作与 channel 由其 Vendor 所有；插件缺失、能力缺失或失败必须如实报告，不得把该连接的单次操作回退到 `base`。

普通发行版默认只内嵌 `base`，专属包作为单独插件分发；分发不定义接入身份。当前组件名、描述符、准入和内嵌例外见 [Vendor Wasm 插件设计](../design/vendor-plugins.md)，来源与安装见[插件市场设计](../design/plugin-marketplace.md)。
