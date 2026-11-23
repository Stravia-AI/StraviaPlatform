---
status: accepted
---

# Gate advanced capabilities at platform level and separate transparent injection

## 背景与决策

Platform Capability Gate 决定一项高级能力是否可用；Gate 开启后，有效 API Key 可走该能力支持的显式入口。MCP 单独检查 Key 的访问开关。透明注入只决定未显式声明时自动暴露哪一项能力，不充当显式授权，也不会在平台 Gate 关闭时绕过它。Key 记录透明注入选择集合；平台关闭某项时不丢掉已保存选择。

基础 `StraviaRead` 不依赖联网搜索、多模态理解或媒体生成被选中，也不因此获得跨 Principal 读取或高级能力权限。历史回看是透明注入专用能力，没有显式或 MCP 入口（[ADR-0096](0096-recall-interaction-chains-through-read-paths.md)）。

## 权衡

逐 Key 授权与平台 Gate、透明注入重叠，易误解；启用注入时自动暴露所有能力，则可能向已有 Key 静默加入新工具。独立 Gate 与显式选择集合分开可用性和自动暴露，管理员须了解开启平台能力会扩大显式权限。详见 [平台能力门禁与透明注入](../design/agent-core.md#平台能力门禁与透明注入)及[集合决策](0097-store-api-key-transparent-injections-as-a-set.md)。
