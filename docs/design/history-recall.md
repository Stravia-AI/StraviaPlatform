# 历史回看

历史回看（History Recall）让平台对话中的模型通过 Inference Run 内的 StraviaRead，只读地浏览、读取并检索同一 API Key 过去的对话。它只经 API Key 的透明注入提供，需要在 Key 上单独选中「历史回看」，默认不选；显式声明的 StraviaRead、MCP 与内部 Agent 都不提供（第 6 节）。内容取自 Interaction Observation 的交互链路，不读取 Generation Chain，也不引入 Session。产品边界与已否决的方案见 [ADR-0096](../adr/0096-recall-interaction-chains-through-read-paths.md)，术语见 [GLOSSARY.md](../../GLOSSARY.md) 的「交互链路」「历史回看」「交互转录」。

> 状态：设计已确认，尚未实现。浏览与读取（P1）、检索（P2）按第 10 节分期交付。

## 1. 来源与范围

- **单位**：交互链路即一段对话，一个 Connect Client Interaction 即其中一轮。路径身份直接使用既有的 28 位小写 Interaction id；链路以根 Interaction 的 id（`root_id`）标识。
- **成员**：沿用词汇表的交互链路成员规则：最终失败且从未交付客户端可见输出的 Interaction 不列出。经 MCP `tools/call` 直接调用平台工具而形成的观测记录（Run 的入口协议为 `mcp`）不是对话，同样不列出。
- **内容**：用户输入、可见回答、工具调用参数与结果、观测缺口和 Run 失败。可读思考默认不出现；system 指令与完整请求本来就不在 Observation 中。
- **时间范围**：等于历史保留天数（`log_retention_days`）。到期清理和「清除历史记录」删除的内容立即不可见，也不可检索。
- **有损**：Observation 写入失败或缺口会让对话不完整。输出如实标注缺口，不从 Generation Chain 或其他来源补全。
- **控制台对话**：用某个 Key 在管理面发送的控制台对话同样形成 Inference Run，因而出现在该 Key 的历史回看中。

## 2. 路径

| 路径 | 类型 | 内容 |
|---|---|---|
| `stravia://history/` | 目录 | 当前 Principal 的交互链路，按最近活动倒序 |
| `stravia://history/<root-id>/` | 目录 | 链路内的 Interaction，按开始时间正序 |
| `stravia://history/<root-id>/<interaction-id>` | 文件 | 该 Interaction 的交互转录 |
| `stravia://history/<root-id>/<interaction-id>/tools/<n>` | 文件 | 该 Interaction 中第 n 次工具调用的完整参数与结果（n 从 1 开始） |

规则：

- 段数决定类型。目录必须以 `/` 结尾，文件不得以 `/` 结尾；写错时返回带正确写法的错误，不做别名。
- `<interaction-id>` 必须属于 `<root-id>` 所在的链路，否则视为不存在。
- 工具调用按开始事件的先后编号，不使用客户端提供的 call id，因为 call id 可能含 `:` 或 `/`。
- 选择器沿用 [ADR-0091](../adr/0091-read-paths-with-colon-selectors-and-plain-fields.md) 的语法。文件接受行范围（最多 16 段）或 `cursor=`；目录和检索结果只接受 `cursor=`。`raw` 与 `download` 被拒绝。
- `question` 只在根目录、链路目录和交互转录上有效，表示在该路径范围内检索（第 5 节）。它与行范围互斥。`cursor` 自带检索条件，再传 `question` 视为冲突。工具文件不接受 `question`，因为工具结果不进入索引。
- `wait`、`attachments`、`allowed_domains` 一律拒绝。
- 不存在、已清理和属于其他 Principal 的路径返回同一个错误：`History entry unavailable: it does not exist, has expired, or was cleared.`

## 3. 目录列表

目录输出 Markdown 列表，时间一律使用带 `Z` 的 UTC 秒精度。所有文本先经过第 7 节的凭据替换，再截取预览。

根目录每页 50 条链路，按「链路最近活动时间、根 id」键集分页：

```text
# stravia://history/ · 23 chains · newest activity first · retention 7 days
- 2026-11-10T07:40:12Z stravia://history/<root-id>/ · 9 interactions · completed · gpt-5-codex · "修复 PG 迁移 0041 升级钩子"
```

- 预览取根 Interaction 用户输入的第一行，不超过 80 个字符；没有用户输入时改用可见回答的第一行，两者都没有时写 `(no text)`。
- 状态取链路中最近活动的 Interaction；模型取根 Interaction 首个 Run 的模型展示名。

链路目录每页 100 条，按「开始时间、id」正序：

```text
# stravia://history/<root-id>/ · 9 interactions · started 2026-11-09T06:20:11Z
- 2026-11-09T06:20:11Z stravia://history/<root-id>/<id-1> · completed · gpt-5-codex · "PG 上迁移 0041 失败"
- 2026-11-09T06:31:40Z stravia://history/<root-id>/<id-2> · completed · gpt-5-codex · "改成分批提交"
- 2026-11-09T06:35:02Z stravia://history/<root-id>/<id-3> · failed · gpt-5-codex · "换个思路" · branch from <id-1>
```

Codex 等客户端每次新输入都形成上一轮的子 Interaction，链路通常是很深的单线，因此不按深度缩进。只有当父 Interaction 不是上一行时才标出 `branch from <parent-id>`；父 Interaction 不在列表中时追加 `(not listed)`。状态、用量等可变信息只出现在目录中。

## 4. 交互转录与工具文件

### 4.1 转录格式

```text
# Interaction <interaction-id>
- chain: stravia://history/<root-id>/
- parent: stravia://history/<root-id>/<parent-id>
- started: 2026-11-09T06:20:11Z · protocol: responses

--- run · gpt-5-codex · 2026-11-09T06:20:11Z
## user
PG 上迁移 0041 升级钩子失败了……
## assistant
先看 retention.rs 的批次边界……
### tool call 1 · exec_command (client)
{"cmd":"cargo test -p stravia-core retention"}
(full: stravia://history/<root-id>/<interaction-id>/tools/1)
--- run · gpt-5-codex · 2026-11-09T06:20:44Z
### tool result 1 · ok
test result: ok. 12 passed……
(full: stravia://history/<root-id>/<interaction-id>/tools/1)
> observation gap: <reason>
> run failed: <summary>
```

- 按 Observation 事件序号渲染。`user_input_recorded` 渲染为 `## user`；可见通道上已完成的 `client_visible_content` 渲染为 `## assistant`；`client_tool_handoff` 与 `platform_tool_started` 渲染为 tool call；`client_tool_result` 与 `platform_tool_finished` 渲染为 tool result；`observation_gap` 与 `request_failed` 渲染为引用行。思考、用量、尝试和压缩等其余事件不渲染。
- 事件所属 Run 与上一条已渲染事件不同时，先写一行 `--- run · <模型展示名> · <开始时间>`。
- tool call 与 tool result 只内联前 20 行、且不超过 2 KiB，随后总是写 `(full: …/tools/<n>)`。工具结果在其事件出现的位置渲染，不回填到调用处。结果的 call id 找不到对应调用时，写 `### tool result (unmatched call)`。
- 图片、音频等非文本部分写成 `[image]`、`[audio]` 这样的占位行。
- 父 Interaction 被清理后，`parent` 行写 `none`。行数不变。

### 4.2 行号稳定性

转录只追加。每个事件的渲染只依赖该事件自身的不可变内容、它所属 Run 的准入事实，以及之前事件已经确定的计数（工具编号、上一个 Run）。因此：

- 已完成 Interaction 的行号永不改变，检索命中的 `path:L-M` 可以长期复用；
- 进行中的 Interaction 只在末尾增长；转录只渲染已持久化的事件，未完成的内容块不出现；
- 凭据替换（第 7 节）保持行数：跨行秘密替换为标记后保留原有换行。

### 4.3 工具文件

先写头部（名称、client 或 platform、开始时间），再写完整参数（JSON 美化输出），随后按出现顺序追加每个结果段（状态、耗时与完整内容）。结果到达前文件只有调用部分，同样只追加。

### 4.4 分页与输出

文件分页沿用文本读取的预算：每页最多 32 KiB、200 行，行范围最多 16 段。游标是 base64url 编码的 JSON，不超过 8 KiB，记录下一个事件序号、该事件起始行号、事件内行偏移、已分配的工具编号和上一个 Run。游标属于不可信输入，每次重新校验路径归属与事件归属；无需固定快照，因为转录只追加。

输出沿用文本页字段：`content`、`read_path`（不含选择器的规范路径）、`representation: "markdown"`、`has_more`、`next_path`、`limitations`。文件读取另带 `returned_ranges`。目录、文件和检索结果使用同一组字段，与既有文本读取一样以一个 JSON 值返回。

## 5. 检索

### 5.1 查询语法

- `question` 按空白切分为词项，最多 8 个；双引号包住的内容是一个词项；整串不超过 256 个字符。
- 每个词项经分词后成为一个短语，要求词元相邻，所以 `run_admitted` 与 `"run admitted"` 等价。
- 词项之间是 AND，在 **Interaction 级**成立：各词项可以分别出现在同一 Interaction 的不同消息中。
- 只有一个 CJK 字符的词项按以该字开头的词元做前缀匹配；单个 ASCII 字符的词项按精确匹配处理。
- 没有任何可检索词元时返回错误，不退化为列目录。没有布尔运算符、通配符和字段语法。

### 5.2 分词

分词器用 Rust 实现，是索引和查询共用的唯一实现：

1. 先做 NFKC 规范化，再做 Unicode 小写化。
2. 把汉字、平假名、片假名和谚文视为 CJK 字符，其余字母和数字视为词字符，其他字符都是分隔符（`_`、`-`、`.`、`/` 也是）。
3. 连续的词字符组成一个词元。连续的 CJK 字符输出相邻双字；长度为 1 的 CJK 片段输出该单字。
4. 丢弃超过 64 字节的词元。

不切分驼峰，不提取词干，也没有停用词。

已知取舍：只出现在 CJK 片段末尾的字无法被单字词项命中；`41` 不匹配 `0041`；`camel` 不匹配 `camelCase`。

### 5.3 索引内容

只索引交互链路成员中的以下内容：

- 用户输入；
- 可见回答；
- 工具调用的名称与参数（client 和 platform 都包括）。

不索引思考、工具结果和 system。参数中出现 `stravia://history/` 的工具调用不进入索引，以免回看操作本身成为后续检索的命中。

### 5.4 存储

两个后端存储相同的词元序列，都不另存明文：

- 一个事件的词元按每段 256 个切分，相邻两段重叠 15 个词元，以保证不超过 16 个词元的短语不会跨段漏检。每段对应 `history_search_segments` 中的一行，记录事件序号、段序号、Interaction、Principal、`root_id`、角色权重、发生时间，以及该段在事件文本中的字节区间。
- **SQLite**：使用 FTS5 contentless 表（`content=''`、`contentless_delete=1`、`tokenize='ascii'`），rowid 等于段 id。写入的是用空格连接的词元。`ascii` 分词器把非 ASCII 字符都当作词元字符、只在 ASCII 分隔符处切分，因此 FTS5 得到的词元与 Rust 输出完全一致；`unicode61` 使用 Unicode 6.1 字符表，会与 Rust 的分类分歧，所以不用。打包的 SQLite 为 3.51.3，满足 `contentless_delete` 对 3.43 的要求。
- **PostgreSQL**：段行内的 `tsvector` 列由 `'<词元>':<位置>` 字面量直接转换而来，并建 GIN 索引。查询使用 `::tsquery` 字面量（`<->` 表示相邻、`:*` 表示前缀），不调用 `to_tsvector`/`to_tsquery`，因此不受文本搜索配置和 locale 影响。每段 256 个词元，正好避开 tsvector 单个词元最多 256 个位置的上限。
- 段行与所属 Interaction 一起删除。到期清理与「清除历史记录」在同一事务中显式删除段行，外键 `ON DELETE CASCADE` 只是兜底；SQLite 的 FTS 行由段行的 `AFTER DELETE` 触发器同步删除。

### 5.5 索引任务

`history_search_progress` 为每个 Interaction 记录已索引到的事件序号，以及建立索引时该 Interaction 是否已终结。

- **唤醒**：本实例的 Observation 持久化变更会唤醒索引任务。实例启动、收到 `ResetRequired`，以及每小时历史回收之后，执行一次全量对账，找出 `last_event_sequence` 或终结状态与进度不一致的 Interaction。各实例只因本实例的写入而被唤醒；其他实例写入的数据由对账补齐。
- **增量**：Interaction 进行中时，每个事务追加不超过 500 个新事件的段，并推进进度。
- **终结对账**：Interaction 进入终结状态（非 `running`、`waiting_client`）时，在一个事务内删除它的全部段并按已提交事件重建。这样多实例下序号未按提交顺序可见的事件也会被纳入。终结后又被续写时，按同样规则再次处理。
- **并发与失败**：段以（事件序号, 段序号）唯一。PostgreSQL 先锁住进度行，并发的实例因此不会重复写入。失败只记 `tracing::warn!` 并留给下一次对账，不影响推理和 Observation 写入。
- **回填**：升级后已有的 Interaction 由同一个任务在后台回填，不在启动迁移中进行。

### 5.6 排序与结果

1. 对每个词项，在范围内查询命中的 Interaction（按其最近命中时间倒序，最多 2,000 个），同时取得最高角色权重。权重为：用户输入 3，可见回答 2，工具调用 1。若某个词项达到上限，结果中注明查询过宽。
2. 取各词项结果的交集，再排除发起本次检索的 Interaction。
3. 得分为各词项最高角色权重之和。按得分降序、最近命中时间降序、Interaction id 升序排序。规则与后端无关，两个后端返回同一顺序。游标记录上一页最后一项的（得分, 时间, id）。
4. 每页 10 个 Interaction。每个 Interaction 最多列 3 个命中段，优先覆盖更多词项、再取更新的段。命中位置通过渲染该 Interaction 的转录，把段的字节区间换算为行范围。

```text
# 3 interactions match "迁移 0041" in stravia://history/ · index current · retention 7 days
1. stravia://history/<root-id>/<interaction-id> · 2026-11-09T06:20:20Z · "修复 PG 迁移 0041 升级钩子"
   stravia://history/<root-id>/<interaction-id>:41-58 · user · …PG 上«迁移»«0041»升级钩子失败后如何…
```

摘要在凭据替换后的文本上重新分词定位，以命中词元为中心，不超过 160 个字符，命中部分用 `«»` 标出。

### 5.7 局限说明

结果的 `limitations` 必须说明以下情况：

- 范围内尚未完成索引的 Interaction 数；
- 查询过宽被截断的词项；
- 回看时限；
- 存在观测缺口的命中。

没有命中时，提示「未命中不代表不存在」，并建议换词或缩小、扩大范围。

## 6. 暴露

历史回看只经 API Key 的透明注入提供，不设平台总开关，管理面也不新增独立页面：

- **Key 注入选项**：API Key 的透明注入新增「历史回看」，即注入集合 `transparent_injections` 中的取值 `history_recall`（[ADR-0097](../adr/0097-store-api-key-transparent-injections-as-a-set.md)），默认不含，与联网搜索、多模态理解、媒体生成并列。它不依赖平台能力是否可用，因此不会因平台状态而被禁用。透明注入总开关关闭期间，已有的选择保留但不生效，与其他注入项一致。
- **透明注入**：只有 Key 开启透明注入且选中「历史回看」时，读取规划才在注入的 StraviaRead 上加入历史范围；Artifact 下载与工具执行查询等基础 Read 照常提供。只选中「历史回看」时，注入的就是基础 Read 加历史范围。
- **显式声明**：客户端在请求中显式声明 StraviaRead 时透明注入不生效，因此即使 Key 选中了「历史回看」，也不包含历史范围。
- **MCP**：不暴露。描述与 schema 不出现历史说明。
- **内部 Agent**：永不暴露。
- **执行时校验**：执行范围不含历史时，历史路径一律返回 `History Recall is only available through this API key's transparent injection.`，MCP 与显式声明都适用这一规则。Key 设置变更从下一次请求起生效。
- **范围**：只读取当前 API Key 对应 Principal 的交互链路，不跨 Key。
- **描述与 schema**：只有当本次暴露包含历史范围时，工具描述才加入历史路径说明与「先检索、再读行范围」的用法，`question` 的说明才包含历史检索语义。
- **风险提示**：Key 编辑器在该选项旁说明：共用 Key 的使用者会互相看到对话，回看时限等于历史保留天数，回看内容属于不可信输入。取消该选项只停止历史回看，不影响其他注入工具。

## 7. 凭据

Observation 保存原文。所有历史输出在离开 Core 前，用所属 Principal 当前有效的可逆脱敏映射，把已知秘密原文替换为凭据标记。模型在工具参数中原样引用这些标记时，由 [ADR-0044](../adr/0044-restore-secrets-in-tool-call-arguments.md) 的机制还原。

- 可逆脱敏总开关关闭时，出站保护不检测新秘密，因此回看不依赖它，统一在这里替换。
- 替换只覆盖已有映射的秘密。从未被检测过的秘密保持原样，这与该 Principal 原本能看到的内容一致。
- `stravia-credential-protection` 目前只提供作用于 `AiRequest` 的替换，需要新增按 Principal 替换纯文本的公开接口。管理面使用的 `***` 遮盖不适用于这里。

## 8. 模块与 seam

- **路径解析**（`stravia-web-access-contract` 的 read path）：新增历史目标，包含节点类型（根、链路、转录、工具）、选择器与可选查询文本。这一层是纯函数，负责全部语法与字段冲突校验。
- **历史回看深模块**（`stravia-core` crate 私有模块）：只有一个入口，即按 Principal 与历史目标返回一页输出或一个错误。成员规则、渲染、分页、检索、排序与凭据替换都在模块内部。
  - 内部的转录渲染器和分词器是纯函数；
  - 检索索引有 SQLite 与 PostgreSQL 两个真实适配器，这是真正的 seam；
  - 索引任务由 Gateway lifecycle 启动与停止，模式同既有的 sweep 任务。
- **Observation 存储**：新增按 Principal 限定的链路列表、链路成员与事件分页查询。现有管理面查询不带 Principal 过滤，不复用作授权边界。
- **StraviaRead 分发**：负责历史范围的暴露与执行范围校验，然后转交历史回看模块。暴露范围增加 `history` 位，只由透明注入的读取规划在 Key 选中「历史回看」时设置；显式声明与 MCP 使用的完整范围不含它，内部 Agent 也不含它。
- **当前 Interaction**：执行上下文目前只带 Run id。检索要排除发起本次检索的 Interaction，需要从 Run 准入时已确定的 Interaction 归属中取得它，不能查询异步持久化的结果。

## 9. 存储变更

- **P1**：`interaction_observations` 新增（`principal`, `last_active_at DESC`, `id`）索引，供按 Principal 列出链路。`api_keys` 不变：「历史回看」只是注入集合的新取值，前提是注入集合已按 ADR-0097 落地。
- **P2**：新增 `history_search_segments`、`history_search_progress`；SQLite 另加 FTS5 表与触发器，PostgreSQL 另加 `tsvector` 列与 GIN 索引。
- 两期都只有 DDL。工作量随 `interaction_observations` 行数增长，启动期使用不定进度；按 [migration-startup](../../.agents/skills/migration-startup/SKILL.md) 的要求测量建索引耗时。检索回填在启动之后由索引任务完成，不阻塞启动。
- 每次迁移都同步两个后端，并重新生成 `docs/database/*.sql`。

## 10. 分期与验证

| 期 | 范围 | 关键验证 |
|---|---|---|
| P1 浏览 | 路径语法、目录列表、交互转录、工具文件、分页、成员规则、凭据替换、Key 的「历史回看」注入选项（注入集合新取值 `history_recall`、Key 编辑器、中英文），以及只经透明注入的暴露；显式声明、MCP 与内部 Agent 不暴露 | 解析器单测；两个后端的 Principal 隔离、清理后不可见与行号稳定；读取规划的暴露组合；Key CRUD；经 `/v1/responses` 与假上游让模型调用注入的 StraviaRead 读取历史的 HTTP E2E；显式声明与 MCP 拒绝历史路径；浏览器中英文 Key 编辑器 |
| P2 检索 | 分词器、索引表、索引任务、查询、排序、摘要、局限说明 | 分词器单测；两个后端对同一中、英、代码混合语料返回相同结果；清理后立即无命中；索引滞后可见；30 至 50 条真实查询的 recall@5，并对比 FTS5 trigram 方案 |

性能验收在实现阶段测量，并记录数据规模与构建模式：按 Principal 列链路、渲染长 Interaction、检索延迟，以及索引体积相对 Observation 正文的比例。不预设结论。
