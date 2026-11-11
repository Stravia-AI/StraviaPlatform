---
status: accepted
---

# 以 StraviaRead 路径回看交互链路

平台对话中的模型需要按需找回同一 API Key 过去的对话：浏览、精确读取并按词检索。Stravia 决定以只读的历史回看（History Recall）满足这一需求：在 StraviaRead 下新增 `stravia://history/` 命名空间，把 Interaction Observation 的交互链路建模为目录，把单个 Connect Client Interaction 建模为只追加的转录文件；`question` 在历史路径上表示在该路径范围内检索，命中返回可直接读取的「路径 + 行范围」。不新增 Session、可变 latest head 或对话实体，也不读取 Generation Chain。完整契约见 [`docs/design/history-recall.md`](../design/history-recall.md)。本决定只确认方向与契约，不表示已经实现。

## 来源与边界

回看的唯一来源是 Interaction Observation。交互链路由 `root_id` 相同的 Interaction 组成，[ADR-0053](0053-keep-one-interaction-across-generation-roots.md) 已允许它跨越多个 Generation Chain 根，正是「同一段对话」的现有表达；只有词汇表定义的交互链路成员可见。Observation 不保存 system 指令与完整请求，只保存用户输入、可见回答、可读思考与工具活动，比 Generation Chain 更接近人读到的对话。

这扩展了 [ADR-0060](0060-own-interaction-observation-outside-generation-chain.md) 中 Observation 的使用方：除管理面诊断外，它还成为历史回看的只读来源。Observation 仍不是推理执行、Generation Chain 或模型历史的事实源：回看内容只作为工具结果交给调用方，不用于重建模型输入、续接、Target Continuation 或授权。观测缺口、到期清理与「清除历史记录」造成的缺失如实标注，不从 Generation Chain 或其他来源补全。

回看与 Turn Chain、Artifact 采用相同的 Principal 边界：只能读取当前 API Key 对应 Principal 的链路，路径不授予权限，不存在、已清理与不属于该 Principal 的条目返回同一错误。同一 Key 下全部 Connect Client 的对话互相可见，这是该能力的目的；多人共用一个 Key 时也会互相可见。

## 路径与检索

路径分四级：`stravia://history/` 列出链路，`…/history/<root-id>/` 列出链路内的 Interaction，`…/<root-id>/<interaction-id>` 是转录文件，`…/<interaction-id>/tools/<n>` 是第 n 次工具调用的完整参数与结果。身份全部复用 Observation 既有 id：Interaction 用其 id，链路用 Observation 的链路根 id（关联 Generation Chain 时为根节点 id，否则为首个 Interaction 的 id）。路径沿用 [ADR-0091](0091-read-paths-with-colon-selectors-and-plain-fields.md) 的 `:` 选择器：文件接受行范围与 `cursor`，目录与检索结果只接受 `cursor`。本决定局部扩展 ADR-0091：`cursor` 不再只对 Artifact 有效，`question` 在历史路径上承载检索文本，与 `stravia://search` 中「`question` 是搜索文本」的语义一致；暴露范围包含历史时，`question` 也出现在 schema 中（ADR-0091 原为仅在 `networking || media` 时出现），其说明仍不超过一句。

转录按 Observation 事件顺序渲染且只追加，可变状态只出现在目录列表中，因此已完成 Interaction 的行号稳定，检索命中可以长期指向同一行范围。默认省略可读思考；工具结果在转录中只给开头部分与完整结果路径。

检索为词法检索。一个 Rust 分词器（字母数字词与 CJK 相邻双字）同时为 SQLite 与 PostgreSQL 生成同一词元序列，分别写入 SQLite FTS5 contentless 表与 PostgreSQL 的 tsvector 字面量加 GIN 索引；两库都不另存明文；不引入新的第三方 crate，NFKC 规范化直接使用依赖图中已有的 `unicode-normalization`。只索引用户输入、可见回答与工具调用参数，不索引思考、工具结果与 system。候选按时间倒序取有界集合，再用与后端无关的规则排序，使两库返回同一结果集。

## 暴露与凭据

历史回看只经 API Key 的透明注入提供：Key 的透明注入选项新增「历史回看」，与联网搜索、多模态理解、媒体生成并列，默认不选；只有 Key 开启透明注入且选中该项时，Inference Run 内注入的 StraviaRead 才带有历史范围。它不设平台总开关，也不在管理面新增独立页面。客户端在请求中显式声明的 StraviaRead、MCP 与内部 Agent 都不提供历史路径，传入时明确失败。Key 设置变更与其他注入项一样，从下一次请求起生效。

这是 [ADR-0016](0016-gate-advanced-capabilities-and-separate-transparent-injection.md) 的有意例外。ADR-0016 中透明注入只是自动暴露的偏好，不是授权；但历史回看没有显式调用或 MCP 入口，Key 上的选择因而同时决定它是否可用。它不按 Advanced Capability 处理，也不受 Platform Capability Gate 控制，因为它不产生上游费用、不外发数据，风险只取决于该 Key 是否被多人共用，按 Key 决定正好对应这一风险。

Observation 按原样保存正文。回看在输出边界用所属 Principal 当前有效的可逆脱敏映射，把已知秘密原文替换为凭据标记，而不是管理面使用的 `***`，使模型仍能在工具参数中引用，由平台还原（[ADR-0044](0044-restore-secrets-in-tool-call-arguments.md)）；可逆脱敏总开关关闭时出站保护不再检测新秘密，因此不依赖它。可回看的时间范围就是历史保留天数，不另设保留期。

## Considered options

- **以 Generation Chain 为来源**：内容精确，但只能按节点物化，没有按 Principal 与时间的列表，载荷含 system 指令与 History Marker；[ADR-0079](0079-keep-console-chat-history-in-the-admin-client.md) 已因此拒绝用它列对话。
- **新建对话或 Session 存储**：可以精确建模，但违反 [ADR-0007](0007-own-agent-execution-behind-native-seams.md) 与 [ADR-0064](0064-use-stravia-uri-references-without-sessions.md) 不引入 Session 的约束，并形成第三份对话事实源。
- **把命名空间叫 `sessions` 或 `interactions`**：前者与词汇表冲突；后者贴近领域术语，但对模型不如 `history` 直观。
- **独立搜索工具**：会多出一个工具声明与第二套范围语义；把检索放进路径范围加 `question`，与 grep 先定范围再找的工作方式一致。
- **SQLite FTS5 trigram 与 PostgreSQL `pg_trgm`**：两库分词与匹配语义不同，`pg_trgm` 还依赖扩展与数据库 locale，违背两库行为一致的要求。
- **外置 tantivy 等索引**：排序能力更强，但索引位于数据库之外，到期清理、清除历史与多实例都需要额外的一致性机制，且新增生产依赖。
- **语义或向量检索**：可以召回措辞不同的内容，但需要 embedding 路由、费用与向量扩展，并把历史外发给 embedding 提供方；首期由调用方改写查询弥补。
- **作为 Advanced Capability 加平台总开关**：权限分层与联网搜索一致，但需要新的设置项与管理面页面；历史回看不产生费用、不外发数据，平台级开关只是在 Key 选项之外再加一层同义控制。
- **作为基础 Read 的一部分、无独立选项**：没有 schema 与界面变更；但要停止某个 Key 的历史回看，只能关闭它的整个透明注入，连带停掉 Artifact 下载等基础 Read 与其他注入工具。
- **显式声明与 MCP 也提供**：外部客户端可以直接使用；但这些入口不受 Key 的透明注入选择约束，无法按 Key 关闭，MCP 输出也不经过 Inference Run 的出站保护。首期只经透明注入提供，需要时另行决策。

## Consequences

- 回看是有损视图：观测缺口、观测写入失败与到期清理会让对话不完整，输出必须说明；不能把「未检索到」当作「不存在」。
- 共用 API Key 的使用者会看到彼此的对话；Key 编辑器中的「历史回看」选项处必须说明这一风险与回看时限。取消该选项即可单独停止，不影响其他注入工具。
- 客户端显式声明 StraviaRead 时透明注入不生效，因此即使 Key 选中了「历史回看」，也没有历史路径。
- 「历史回看」是 Key 注入集合的新取值 `history_recall`（[ADR-0097](0097-store-api-key-transparent-injections-as-a-set.md)），不改 `api_keys` schema；需同步 Key 编辑器与中英文文案。
- 回看内容含历史工具结果和网页内容，以工具结果的形式重新进入上下文，属于不可信输入。
- 检索新增 SQLite 与 PostgreSQL 表、索引与后台索引任务；索引滞后只影响检索完整性并在结果中说明，不影响推理或 Observation 写入。
- 现有文档中「Interaction Observation 是管理面诊断投影」「`cursor` 只对 Artifact 有效」的表述随实现同步修订。
