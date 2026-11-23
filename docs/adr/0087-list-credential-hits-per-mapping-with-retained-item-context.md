---
status: accepted
---

# 命中记录按新增映射逐条列出，并从保留正文还原命中位置

## 背景与决策

Interaction 摘要或仅在 Debug Run 记录不能回答「哪个映射由何处创建，后来在哪里使用」。管理列表按 `credential_mappings_created` 的映射逐行投影；`credential_mappings_reused` 只记录既有映射的实际使用：出站只记录新提交内容（不计客户端重放），还原模型工具参数时按调用记录。两种事件都保留占位符身份以关联创建和复用，但不含秘密明文。

`locators` 保存规则与来源配对。工具参数或工具结果用上游原始 `tool_id` 定位；`latest_input` 只标记当前请求窗口最后一条用户输入，不标记 Agent 合成输入。规则取自映射创建归因；不存偏移、消息下标或秘密片段。正文还在才按需重跑检测器，正文或定位缺失时明确提示，不伪造高亮；管理 API 的遮罩与显式明文访问见 [ADR-0088：管理边界遮罩与复用分页](0088-mask-credentials-at-admin-boundary-and-page-reuses.md)。

## 权衡

写入偏移或片段会额外留存敏感内容且难与编码后的正文稳定对齐；按需重算接受规则变化时匹配位置不再完全一致。复用事件和索引登记同事务写入，多一次有界 upsert 换取按映射分页，不扫描整份事件流。契约见 [凭据保护](../design/credential-protection.md)，正文保留边界见 [交互观测](../design/interaction-observation.md)。
