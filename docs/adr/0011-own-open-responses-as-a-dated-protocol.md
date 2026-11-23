---
status: accepted
---

# Own Open Responses as a dated protocol

## 背景与决策

Stravia 以官方 Open Responses `2026-04-24`（`open-responses/responses/2026-04-24`）固定 Responses-shaped canonical 基线，不把 OpenAI rolling Responses API 作为第二套 canonical 协议。OpenAI 的供应商差异留在 Vendor adapter；请求、响应、流与续接共享有序 `AiItem` Graph。

Ingress 接受结构安全的 additive 字段和非命名空间 hosted tool 声明。同协议保留可承载的扩展；跨协议按照[表达能力与省略规则](../design/open-responses.md#表达能力与省略规则)保留可表达部分，不把缺失语义伪装成等价转换。Stravia 自有工具是 Inference Run 的隐藏 Platform Tool 轮次：协议声明不选 Web Provider，也不冒充上游原生 hosted tool；Web Access 的 Provider 选择由 [ADR-0002](0002-web-access-provider-seam.md) 独立负责。

## 权衡

只接受 dated schema 会无谓拒绝 rolling 客户端，直接追随 rolling API 又会让 canonical 语义无版本漂移。固定基线加兼容 envelope 可把两者分开：添加兼容字段不要求新 identity，改变 canonical 语义或生命周期则需要显式决策。包括 Responses-only Native Compact 在内的当前协议见 [Open Responses](../design/open-responses.md)。
