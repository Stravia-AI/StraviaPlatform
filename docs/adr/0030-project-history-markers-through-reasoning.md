---
status: accepted
---

# Project History Markers through reasoning carriers

本决策取代 ADR 0028 中“普通可见 delta 立即发送”和 Marker 载体相关部分。ADR 0028 的 Principal 隔离、one-to-one Hidden History Segment、durable execution、publish、lease、deadline、取消及崩溃恢复不变量保持不变。

## Context

一个 Proxy Inference Run 可以因隐藏 Platform Tool Execution 连续执行多个 Model Turn。若第一轮已经发送普通 Text，随后第二轮再发送 Thinking，客户端会观察到 `reasoning → content → reasoning → content`。按字段聚合历史的客户端还会丢失 Thinking、Text、隐藏 ToolCall 与 ToolResult 的相对顺序。仅保留 SSE chunk 边界不能解决下一轮回放。

在 Platform ToolCall 完整出现或 Model Leg 结束前，Stravia 无法判断首个 Text 是最终答案还是平台工具前置叙述。根据措辞、长度、超时或工具名猜测都会改变模型语义。

## Decision

- Provider canonical response 与 client projection 是两个视图。Hooks、隐藏续跑和 Effective Model Request 只使用 canonical response；客户端及 Generation Chain 使用 client projection。
- 新 History Marker 一律表示为无签名 `ContentBlock::Thinking`。各生成协议 Adapter 必须使用原生 reasoning、thinking 或 thought 载体编解码；不得回退到普通 Text。保留期内旧 Text-carried Marker 仍可读取。
- 包含 Platform ToolCall 的 Model Leg 中，canonical Text 只在 client projection 中改用 Thinking 承载。最终无 Platform ToolCall 的 Text 保持普通 content。
- 每个被投影的连续 Text span 由一对 Projection Delimiter 包围。Delimiter 绑定既有 Principal-scoped Marker reference 和 span ordinal，不持久化 payload，也不创建第二种 Hidden History Segment。
- History Marker resolver 按客户端提交顺序解析 Text、Thinking、reasoning、Delimiter 和 Marker。只有同一请求保留了 Marker、Delimiter 配对有效且 Marker 对当前 Principal 已发布时，才把范围内字节恢复为 canonical Text。未知、过期、未发布、重复或未授权 Marker 只清除私有语法。
- Protected Thinking preview 使用独立 preview delimiter。恢复时删除 preview，并在 Marker 原位置插入 Store 中的 signed、encrypted 或 redacted authoritative block。
- 当 Inference Run 暴露 Platform Tools 时，首个 Text 之前的 genuine reasoning 立即发送；从首个 Text 开始只缓冲该 Model Leg 的可见后缀。确认 Platform ToolCall 后，将其中 Text 投影为 delimited Thinking；Model Leg 无 Platform ToolCall 时按原顺序回放。禁止用超时、阈值或文本分类提前决定。
- live stream 与 terminal staged projection 使用相同 Marker reference、span ordinal、Delimiter 顺序和可见字节。Generation Chain 保存实际交付的 client projection；canonical accumulator 始终接收原始 delta。
- Marker 的安全顺序保持为：create/claim，交付 Marker projection，确认 delivery progress，publish，启动 Platform Tool Execution。提交前不可表示 Thinking 时返回 typed error；提交后失败使用 ingress 协议 terminal error。

## Consequences

- 正确性要求缓冲含 Platform Tools 的 Model Leg 中从首个 Text 开始的后缀。无 Platform ToolCall 时，最终 Text 的首字节延迟到 Model Leg 结束；这是结构性延迟，不提供配置开关或启发式旁路。
- Genuine reasoning 仍可实时显示；不缓冲整个 Model Turn。
- one-shot buffered 请求无法同时保持自动隐藏续跑和“Marker 交付后才执行”不变量。只包含隐藏 Platform Tool 的 Model Leg 返回 typed `409 history_marker_delivery_required` 且不创建 Marker、不启动工具；调用方需改用 live stream。
- Protocol codecs 只机械地转换 canonical Thinking。平台工具分类、Marker 放置和 Text 投影集中在 Inference Run；有序恢复集中在 History Marker 模块。
- Projection Delimiter 的耐久性依赖客户端保留协议 reasoning history。客户端删除 Marker 或 Delimiter 即表示显式历史编辑，Stravia 不从周边文本猜测或恢复被删除的内容。
- 数据库 schema 与 SQLite/PostgreSQL Store 契约不变。
