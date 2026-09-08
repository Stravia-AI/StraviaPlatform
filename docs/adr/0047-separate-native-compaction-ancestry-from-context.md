---
status: accepted
---

# 分离原生压缩来源与执行窗口

客户端压缩会替换已提交的窗口，原有严格完整前缀不能证明跨边界父关系。Stravia 以独立 core Compaction 深模块持久登记 Provider 原生状态、精确身份和来源，把这作为显式续接证据；模型始终接收客户端压缩窗口，不借来源关联恢复已删除历史。客户端本地摘要的幸存尾部只能形成 Observation 推断。

## 契约与取舍

- 这是对 [ADR-0020](0020-discover-generation-parents-from-strict-canonical-history.md) 的显式原生引用扩展，不放宽普通 Automatic Parent Discovery。同一状态多次回传时，先确认当前窗口的严格后继；没有后继才跨登记边界，显式引用与状态冲突则拒绝，不按最新登记或可变 head 选父。
- [ADR-0022](0022-materialize-generation-chains-from-deltas.md) 的结构共享保留。压缩边界同时记录 client-shaped 与 effective Replace；普通后继继续保存 delta。冷物化不重跑 Hook，也不把祖先有效历史拼回压缩窗口。
- Standalone compact 通过既有 Model Turn Executor、Vendor 用途构造与 HTTP unary adapter 执行，返回原生 compact resource，不生成空 Generation。Inline 沿用 JSON、SSE 与 Responses WebSocket；每个完整状态在可回放字节交付前持久登记，不缓冲无关公开文本。
- 压缩操作的 instructions 不必等于原任务指令。确认操作来源时，可用平台已发布的 output item identity 定位同 Principal 的节点，并核验其完整 client-shaped 前缀；不能仅凭 ID 或正文相似度确认来源。这个查询不复用来源 controls，不改变普通 Generation 的严格父发现，也不将原生 compact ID 当成 Generation ID。
- 明确的原生控制或已开启的 Route 自动策略使用原生用途的来源准备：已验证的完整来源窗口可以提供父关系与输入 delta，即使本次没有追加 User。执行仍以当前客户端窗口与控制为准；阈值未触发时不伪造压缩事件。Standalone 使用相同来源与 delta 元数据分组，但不 stage/persist Generation。普通请求仍不把完全相同的已完成窗口视为严格后继。
- SQLite/PostgreSQL 是原生登记事实源，与诊断保留隔离。登记后、交付确认前已经可解析；未确认且未引用的记录保留一小时，确认交付或合法引用后保留七天并延长必要来源。状态重复使用与分支不是消费操作。SQLite 读后写事务与 Turn Chain 一样预先取得写锁，避免立即回传与交付确认的快照升级竞争。
- 原生状态绑定已知 Target、账号/配置 generation、模型与协议；这不扩大 [ADR-0028](0028-persist-hidden-history-behind-markers.md) 普通 Opaque Context Requirement。原生 compact ID、upstream response ID、Generation ID 不互换。Target Continuation 失效时，允许重放的完整上下文仍是当前压缩窗口。
- Route 策略默认关闭，仅在客户端原生控制缺省时注入；实际阈值由支持原生 server-side compaction 的 Target 判断，不使用累计 usage、字节估算或平台摘要回路。`context_management` 决定本次执行策略，不改变已经交付的历史身份；调整或取消阈值后仍可发现完整的压缩后继。请求与 Target Continuation 的语义指纹继续区分该控制的缺省、null 和空集合，不能复用不兼容的上游状态。
- Retained Tail Association 只保存诊断元数据。其敏感内容索引可丢失；重启后存在未索引保留候选、索引容量不足或搜索不完整时，返回 unavailable/resource-limit，不宣称唯一。已有持久关系仍可查询，清理后的卡片不从核心历史复活。

不采用静默本地摘要、放宽 unknown item、单独复制 compact HTTP 发送路径或把原生 state 包成 History Marker。它们分别改变客户端意图、掩盖有损转换、复制执行语义或混淆独立身份。
