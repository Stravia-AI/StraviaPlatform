---
status: accepted
---

# Add Media Understanding as a capability-owned internal Agent

## 背景与决策

Media Understanding 由能力专用 adapter 调用一个仅内部可用的 Agent Definition，并将其 Agent Turn 投影为 Media Understanding Turn；无需新增 Runner、Backend enum 或历史节点类型。公开入口是 `StraviaRead`，普通推理的隐藏工具和 MCP 直调复用同一能力契约。管理员设置能力和内部逻辑 Model；平台能力授权不要求 API Key 直接绑定内部 Model，但仍动态检查 API Key 与适用额度。

媒体预处理归 Media Understanding 所有，既不改写 Artifact 原始字节，也不把规范化策略灌入通用 AgentRunner。模型使用适用的 derivative，报告的来源仍指向原始 Artifact。JPEG 规范化优先考虑跨 Provider 互操作：官方服务支持 WebP 不等于兼容端点接受 WebP，也不必为通用路径增加 WebP 编码依赖。Office 提取边界、当前输入格式与报告契约见 [Media Understanding](../design/media-understanding.md)。

## 权衡

直接暴露 generic Agent 虽然复用更多接口，却不能稳定表达来源、媒体转换与产品授权；复制一套运行器会重复 Agent 的版本、取消、归档和统计职责。专用 adapter 保留产品语义，通用执行仍由 [Agent Core](../design/agent-core.md) 负责。
