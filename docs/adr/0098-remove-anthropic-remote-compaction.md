---
status: accepted
---

# 删除 Anthropic 远程压缩，仅保留 Responses 原生状态

本决定取代 [ADR-0089](0089-carry-native-compaction-across-anthropic-and-responses.md) 的跨协议压缩设计，并撤销 [ADR-0047](0047-separate-native-compaction-ancestry-from-context.md) 中 Anthropic 可读摘要降级的例外。此前对官方 Claude Code 2.1.323 的真实手动 `/compact` 抓包表明，它使用普通 Messages 文本摘要，与原来的 on-demand 签名压缩并不相同。本次选择删除支持，不实施 CLI 摘要替代方案；厂商 API 的独立机制见[研究报告](../research/upstream-remote-compaction.md)。

## 契约

- 独立压缩只接受 Responses V2：`POST /v1/responses` 或 WebSocket `response.create`，流式请求末尾恰好一个 `compaction_trigger`。沿既有 Model Turn 执行，不生成 Generation，不运行 Agent 循环，不暴露旧 `/v1/responses/compact` 路由。
- 上游压缩只走支持 Compact 的 Responses Vendor，使用普通流式 `/responses`、`store=false` 与唯一末尾触发器。没有显式 `AiRequest.instructions` 时补入既有交接摘要指令；显式指令不覆盖。保留正常请求编码支持的工具、reasoning 等控制。
- Anthropic Messages 不再接受顶层压缩请求、`compact_*` 编辑或历史 compaction 块。不再解析、输出或回放签名摘要，不发送压缩 beta，不把 Responses 状态包装成 Anthropic 签名，也不把 Anthropic 签名包装成 Responses 密文。旧 Stravia 跨协议信封不可回放。
- Claude Code Vendor 不再声明或执行 Compact；base Anthropic 和使用 Anthropic Messages 的自定义连接同样不支持。显式独立压缩返回 `compaction_unsupported`，不发送上游、不重试、不切换 Target、不转换为普通推理或本地摘要。向 Anthropic 投影明确的压缩控制或不透明压缩状态也返回错误，避免静默丢失状态。
- 普通 Claude/Anthropic 推理、thinking、工具调用、Message Threads 和非压缩类上下文编辑保持原有职责。其他协议原有的普通 inline 控制兼容投影规则不因此扩大变更。
- `ContentBlock::Compaction` 仅承载 Responses `encrypted_content`；删除 Anthropic 摘要字段、原生块 metadata 与签名指纹分支。`CompactionControl` 仅保留 Responses 阈值，不再有 Anthropic 专用摘要任务指令。SDK 的 format-1 投影不再派生 Anthropic context-management 编辑或旧 metadata 别名，规范化也不再将它们导入压缩控制。
- 原生输出必须完整终止且恰好包含一个有效 compaction item。错误、拒绝、截断、重复状态和未完成输出不能登记为成功。返回普通 Response SSE 和 `response.completed`，客户端自行保留尾部并构造窗口。
- Principal 隔离、精确身份与指纹、来源和 Target 绑定、发布确认、SQLite/PostgreSQL 登记与共享保留策略不变。不得把来源祖先全文拼回客户端压缩窗口，不再存在可读摘要跨 Target 降级。
- `supports_remote_compaction` 继续为只读派生能力：至少一个 Target，且所有已配置 Target（包括禁用者）均通过实际插件 Compact 准入并选择 Open Responses。客户端重新接入时按此结果清除 Anthropic/Claude 模型的远程压缩配置。Codex 的 provider 级 V2 声明仍要求全部可见文本模型支持。

## 切换与风险

不清理用户历史或执行数据库数据迁移；旧 Anthropic 状态不再具有执行能力，也不转换为普通摘要。已有依赖这类状态的会话需要重新提供未压缩历史或新建会话。历史诊断保留不代表允许继续回放。

需要同时更新宿主和重新构建、导入独立 Claude Code 插件。旧插件的能力声明不能令宿主把 Anthropic 标记为支持压缩，协议投影同样拒绝这条执行路径。Responses/Codex 原生状态、续接与客户端配置不改为 Stravia 自有引用。
