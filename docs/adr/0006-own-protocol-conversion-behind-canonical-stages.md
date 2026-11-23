---
status: accepted
---

# Own Protocol Conversion behind a pair-bound canonical interface

## 背景与决策

标准协议按独有 wire contract 登记，不按 Vendor 增设协议。`ProtocolTransform::bind(ingress, egress)` 得到绑定协议对的 `ProtocolPair`，统一 request、response 与有状态 stream 转换；编排层只处理以有序 `AiItem` Graph 为核心的 canonical IR。转换不负责 Hook 生命周期、历史身份、凭据或网络。插件私有 wire 由所属 Vendor 编解码，不进入共享协议目录。

转换保留目标可表达的内容和约束，默认省略不能表达的部分，包括目标无法承载的硬约束；不设置 strict toggle，也不填造工具参数、关联结果或 usage。非法输入、解析失败和独立安全约束仍必须报错。请求、响应、stream 与续接适用同一原则，原始历史快照不被目标投影改写。详见 [表达能力与省略规则](../design/open-responses.md#表达能力与省略规则)。

## 权衡

逐协议对直连会让映射、流状态机和修复数量随协议增长；引入以某一供应商协议为中心的外部 IR 则带来第二组语义和隐式损失。绑定 façade 保留一个调用边界，避免为同样的 decode/encode 操作再造 stage wrappers。兼容优先明确接受跨协议有损，调用方不能假定未表达的要求得到执行。

模块与数据流见 [系统架构](../design/architecture.md)；协议身份以 [ADR-0056](0056-grow-protocols-by-unique-wire.md) 所述 wire 边界为准。
