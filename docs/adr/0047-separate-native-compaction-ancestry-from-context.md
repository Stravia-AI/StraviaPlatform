---
status: accepted
---

# 分离原生压缩来源与执行窗口

客户端压缩会替换先前已提交的窗口，普通严格前缀不足以跨边界证明父关系。Stravia 持久登记 Responses 原生压缩状态、精确身份、来源与 Target 绑定，以显式证据保留执行祖先；不放宽普通 Automatic Parent Discovery。客户端提交的新窗口仍是模型实际接收的上下文，来源只用于证明祖先，不能把已裁剪历史拼回来。客户端本地摘要及保留尾部匹配只能建立[诊断归属](../design/interaction-observation.md)，不能当作执行证据。

[ADR-0098](0098-remove-anthropic-remote-compaction.md) 将现行原生 Compact 限定在 Open Responses：不跨 Target 重放、不降级成可读摘要、不接受或还原 Anthropic 原生压缩状态。独立 Compact 不创建空 Generation；普通后继仍使用结构共享。把原生状态包装成 History Marker 会混淆两种身份，静默本地摘要则会改变客户端意图，均不采用。

原生 Compact、来源确认与回放详见 [Open Responses 设计](../design/open-responses.md)，登记持久化与引用保留详见[存储设计](../design/data-layout-performance.md)。
