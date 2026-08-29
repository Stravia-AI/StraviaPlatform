---
status: accepted
---

# Gate advanced capabilities at platform level and separate transparent injection

Stravia 将用户可见的“多模态理解”和“联网搜索”定义为两个独立的 platform-gated Advanced Capability。每个总开关打开后，所有有效 API Key 都可以显式使用对应能力；关闭后任何 Key 都不可用。API Key 不再持有 `allow_web_research` 或 `allow_media_understanding` 能力授权，只保留独立的 MCP 访问开关，以及“透明注入”总开关和多模态理解/联网搜索的功能选择。透明注入只控制客户端未显式声明时的自动工具暴露，不影响显式工具调用或 MCP；平台能力关闭时，既有 Key 的注入选择保留并置灰，重新开启后恢复。

## Considered options

- **保留 API Key 能力授权**：权限更细，但与“平台开启后所有 API Key 可用”的产品模型重复，并造成平台总开关、Key grant、透明注入三层难以解释的权限。
- **让透明注入控制全部调用面**：开关更少，但会把客户端显式工具调用和 MCP 意外绑定到自动暴露策略。
- **开启透明注入即暴露全部能力**：新增 Advanced Capability 可能自动暴露给已有 Key；功能多选能保持最小自动暴露范围。
- **保留图片能力细节卡片**：格式、大小、像素和处理限制属于运行时约束，不应占据管理员配置主界面；本期名称可扩展到 PDF、视频和音频，但运行时仍只支持 JPEG/PNG/WebP。

## Consequences

- API Key 权限 schema 是 clean cutover：删除旧 `allow_*` 与旧单布尔注入语义，迁移既有自动行为到新的注入选择；旧能力授权字段不再控制显式可用性。
- MCP 发现和调用仍要求 API Key 的 MCP 开关，并受平台总开关约束；透明注入关闭不会隐藏已允许的 MCP 工具。
- 平台配置在“高级功能”分组下保留多模态理解与联网搜索独立页面；多模态页面只保留总开关、逻辑模型、状态和保存动作。
- “多模态理解”是当前 Media Understanding 的用户可见名称；未来格式扩展使用新的 Definition Revision，不把长期目标误报为本期支持。
- 平台开启高级能力会扩大所有有效 API Key 的显式权限；这是有意接受的管理员级风险，必须在平台总开关上保持清晰状态。
