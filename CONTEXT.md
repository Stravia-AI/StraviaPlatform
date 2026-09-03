# 术语

## Stravia

Stravia 是本产品的唯一品牌名称。用户可见文本、技术标识、配置键、持久化目录、错误代码和发布元数据均使用 Stravia 派生名称；不保留旧品牌兼容名称或别名。

## Principal

Principal 是由有效 Stravia API Key 建立、用于归属 Turn Chain、Artifact、配额、并发限制与执行状态的认证客户端身份。Stravia 不存在 Anonymous Principal，也不以连接或 Session 代替认证身份。
_避免使用_：Anonymous Principal、Client Session

## Principal Concurrency Limit

Principal Concurrency Limit 是同一 Principal 在单一受支持 Gateway 实例中可拥有的活跃客户端执行根请求的最大数量；每个 Proxy Inference Run 与每次 MCP `tools/call` 各占一个名额。所有活跃根请求都会被计数，`NULL` 表示不限，正整数表示上限；它在 Principal 认证成功、Request Hook 或 MCP 工具执行尚未开始时占用，持续到完整交付或终止清理完成。根请求内的重试、隐藏 Model Turn、透明 Platform Tool call、透明 function call 与嵌套执行复用同一名额，不额外占用并发；已发布 History Marker 的后台 Platform Tool Execution 继承原根请求名额直到执行终态。匹配该 Marker 的请求可以在不占名额时等待 execution，汇合后再正常竞争名额；除此之外，超过上限的新请求会立即被拒绝，不进入等待队列。更新后的限制只影响后续准入，不中断已开始的执行。
_避免使用_：RPM、RPD、TPM、TPD、连接数

## Hook

Hook 是受信平台管理员配置的进程内推理扩展。它读取规范化的请求或响应语义，并以受限动作改写、拒绝或提前完成一次推理，而不直接接触客户端凭据或上游凭据。

## Inference Run

Inference Run 是处理一次客户端生成请求直到一个响应完整交付或终止的执行过程。一次 run 可以包含多个隐藏 Model Turn 和 Platform Tool 调用；一旦向客户端交付公开工具调用，该 run 即结束，客户端提交工具结果时开始新的 run。

## Model Turn

Model Turn 是一次完整的规范化模型交互。一次 Model Turn 可以包含同一逻辑模型路由内的上游重试，但不包含平台工具执行；工具结果引发的下一次模型交互属于新的 Model Turn。

## Effective Model Request

Effective Model Request 是一次 Model Turn 在历史恢复、继承状态与 Hook 变更全部解析后，准备交给某个 Target 的完整 canonical 任务语义。它不等于客户端原始 wire request，也不包含纯 transport 或交付格式差异。
_避免使用_：Raw Request、Wire Request

## Model Turn Executor

Model Turn Executor 是执行一个 Model Turn 的深模块。调用方提交 Principal、Effective Model Request、授权方式、可选的允许转发上游提示，以及 cancel / deadline。授权方式是 Route 模型绑定，或 Advanced Capability grant；它不是 API Key 上的独立能力开关。Executor 负责授权、Route / Target 选择、第一次 canonical 输出之前的 Target failover、Provider Transport（含 Responses WebSocket 与可选 Target Continuation）以及 live canonical stream。它返回 Route、canonical stream 与本次 Target 身份。Target Continuation 由注入的 lookup 发现，不把 Generation Chain 纳入 interface。它不拥有 Hook、Platform Tool 循环、Client Output Commit、客户端交付、ingress 协议或 Generation Chain / Agent Turn 落盘；请求中的 tools 是 Target 的硬约束。调用方消费 stream，Executor 不另提供 unary 完成方法。
_避免使用_：在 Inference Run 或 Agent Runner 内复制 Target 循环；让 Gateway 自身充当 Executor；把 Generation Chain Store 传入一次 Model Turn


## Agent Definition

Agent Definition 是平台程序拥有的稳定 agent 用例。它规定输入输出契约、exposure policy，并以不可变 Revision 固定 instructions、可用 Platform Tool、运行预算和 Artifact policy；管理员不能创建或改写其行为。

## Agent Definition Revision

Agent Definition Revision 是 Agent Definition 行为的一次不可变版本。新根使用当前 Revision，已有 Agent Turn 始终沿用其根节点固定的 Revision。

## Agent Definition Config

Agent Definition Config 是管理员对 Agent Definition 的本地启用状态和逻辑 Model 绑定；它不改变 Definition 的输入输出契约或行为版本。

## Agent Runner

Agent Runner 是按一个 Agent Definition 推进有界模型与平台工具循环的深模块。它可在调用方明确选择时提交 Agent Turn，也可作为另一个深模块的 ephemeral 内部执行器；ephemeral execution 不形成可续接 Agent 历史。

## Agent Run

Agent Run 是 Agent Runner 的一次执行尝试。它可以包含多个 Model Turn 和 Platform Tool 调用；失败、取消或执行实例失联不会产生可续接历史节点。

## Agent Turn

Agent Turn 是 Agent Run 成功完成或受预算约束完成后提交的不可变续接点。每个 Agent Turn 由 AgentTurnId 标识，并可引用一个父 Turn；同一父 Turn 可以产生多个互不合并的分支。

## Turn Chain

Turn Chain 是由持久化不可变历史节点组成、归属于一个认证主体的有向无环父链。调用方以节点 ID 选择精确前缀，平台据此恢复完整规范化历史；不存在内存事实源、可变 latest head 或独立 Session 身份。

## Artifact

Artifact 是归属于一个认证主体的不可变媒体或大对象。外部以 opaque ArtifactId 引用它；内容身份、保留期和历史引用由平台管理。

## Artifact Store

Artifact Store 是保存并读取 Artifact 内容的深模块。它向调用方隐藏本地文件或对象存储差异，不负责解释媒体语义或执行模型任务。

## Platform Tool

Platform Tool 是由平台拥有、注册和执行的模型工具。平台向模型暴露它、拦截其调用并将结果仅送回模型；客户端看不到调用参数或工具结果，但协议可以暴露不含敏感内容的执行生命周期。

## Client Projection

Client Projection 是把 canonical response 变成客户端可见视图：Platform Tool call/result 与 authoritative Thinking 可替换为 History Marker，普通可见 Text 不因潜在或实际 Platform ToolCall 而延迟交付；OpenAI-compatible 的 Post-Text Thinking 以 Markdown 引用 Preview、Projection Delimiter 与一对一 History Marker 经 Text carrier 交付，其他协议保持原生 carrier，不能表示该顺序时显式失败而不回退缓冲。它拥有 Thinking History Marker 的 reserve → 落盘 → 交付 → publish 顺序；不拥有 Platform Tool Execution，也不拥有 Marker 的持久化事实源。它不是 Protocol Conversion，也不拥有 ingress 协议形态改写；Generation Chain 保存的是投影完成之后、按 ingress 协议落盘的结果。
_避免使用_：History Marker Projection；把 Generation Chain 的协议形态改写称为 Client Projection；把 Platform Tool Execution 生命周期称为 Client Projection

## Post-Text Thinking

Post-Text Thinking 是同一 Inference Run 的 Client Projection 顺序中首个非空 Text 后出现的 canonical Thinking，不因 Model Leg 切换而重置；live 与 staged 都在 Client Projection 产出该非空 Text 时进入 Post-Text，不因 Delivery 的 Sent 或 Client Output Commit 翻转；空 delta 不触发，非空空白属于 Text。OpenAI-compatible Client Projection 仅将其中原本可公开的字节作为 Markdown 引用 Preview，权威内容由一对一 Thinking History Marker 恢复；protected payload 没有公开 Preview 时只交付 Marker。
_避免使用_：Late Reasoning、Quoted Text

## Quoted Thinking Preview

Quoted Thinking Preview 是 Post-Text Thinking 中可公开字节的 Markdown blockquote 展示，每个物理行及空行都属于同一外层引用块；它只保证内容不逃逸该引用块，不保证内部 Markdown 采用统一样式。其布局字节属于 Projection Delimiter span，authoritative Thinking 仍由 Marker 恢复；仅编辑 Preview 而保留 Marker 不改变 authoritative Thinking，删除 Marker 后引用按普通客户端 Text 保留。编码器可以保留不超过最长私有语法前缀的固定 lookbehind，以跨 delta 转义 Marker 与 Delimiter 伪造，这不构成 Model Leg Text 缓冲。
_避免使用_：Thinking Text、Authoritative Thinking

## History Marker

History Marker 是 Client Projection 中、归属于 Principal 的 opaque 历史引用，用于在原位置等待并恢复一个 Hidden History Segment。一个 Marker 只能引用一个 Platform Tool Execution（其 call 与 terminal result）或一个 authoritative Thinking block，禁止聚合多个工具执行或多个 block。新 Marker 默认以无签名 Thinking block 中仅供机器读取的 HTML comment 呈现；OpenAI-compatible 客户端收到首个 Text 后，所有 Thinking 与 Platform Marker 均改用 Text carrier，以保留它们与后续内容的顺序。Markdown renderer 通常隐藏该 comment，纯文本客户端可能直接显示它，这是 Text carrier 的显式协议行为；周边客户端历史可以独立修改，同一 Marker 在保留期内可以被重试和并发分支重复使用。
_避免使用_：占位文本、Platform Tool Call、Client History Token

## Reserved Thinking Marker

Reserved Thinking Marker 是每个 canonical Post-Text Thinking block 的首个 delta 到达时由 Client Projection 在当前 Inference Run 中分配、尚未落盘且未发布的 History Marker reference；它允许 Preview 立即流式交付，只有完整 block 以同一 reference 原子落盘、Marker 按顺序交付并发布后才能恢复，后续 Text 必须等待该过程完成。落盘或发布失败必须显式终止交付且不得提交 Generation Chain，不得把 Preview 降级为 canonical Text；失败或未完整交付的 reference 必须废弃。无原生 block identity 时，一个连续 Thinking delta run 构成一个 block。
_避免使用_：Published Marker、Partial Thinking

## Projection Delimiter

Projection Delimiter 是 Client Projection 中围绕一段可见字节的成对、无状态机器语法，表示该段 canonical content 为客户端展示临时使用了不同 carrier；Text mode 将 Thinking carrier 中的可见字节恢复为 canonical Text，Preview mode 在绑定 Marker 可恢复时删除仅供展示的字节，由 Marker 提供 authoritative content。Delimiter span 拥有为展示插入的换行、Markdown 引用前缀与转义字节，它们不属于 canonical content；客户端回放时只有绑定的 Principal-scoped History Marker 仍存在、Delimiter 正确配对且 Marker 可解析，范围内字节才按 mode 恢复。删除或破坏任一边界属于显式历史编辑。
_避免使用_：History Marker、Hidden History Segment、Projection Record

## Hidden History Segment

Hidden History Segment 是 Client Projection 省略、但属于模型有效上下文的单个受保护单元；它是一个 Thinking block，或一个 Platform Tool Execution 的 call/result 对。恢复只替换对应 History Marker，不覆盖 Marker 之外的客户端历史，也不拥有同一模型轮次中客户端可见的工具调用。一次恢复同时给出模型可见历史，以及仍带 Marker 的 client-shaped 对照；Generation Chain 落盘 effective request 时使用该对照，不自己回锚 Marker。
_避免使用_：Hidden Client History、完整历史快照

## Opaque Context Requirement

Opaque Context Requirement 是恢复受保护推理后对 Target 施加的无损协议表示约束，不绑定原 Provider、credential namespace 或 model。Target 不能满足该约束时必须跳过；没有可用 Target 时，本次生成必须拒绝，不能丢弃密文或降级为明文继续。
_避免使用_：Reasoning Fallback、Best-effort Thinking

## History Marker Store

History Marker Store 是在 Marker 交付前持久化 Principal-scoped 隐藏历史及其 Platform Tool Execution 状态的事实源；执行完成后形成不可变 Hidden History Segment。它独立于只在完整交付后形成节点的 Generation Chain；未被完整交付引用的记录由保留策略清理。
_避免使用_：Marker Cache、Tool Continuation Store

## Platform Tool Execution

Platform Tool Execution 是由 History Marker Store 跟踪、可与客户端工具并行推进的一次平台工具执行。进程崩溃时未落盘完成的执行转为失败结果并提示模型重新请求工具，Stravia 不自动重放可能已经产生副作用的调用。
_避免使用_：Tool Retry Job、Inference Run

## Generation Chain

Generation Chain 是归属于 Principal、由已完整交付且终态为 `completed` 或 `incomplete` 的生成请求与最终响应组成的不可变历史有向无环图，独立于 ingress 协议。每个节点保存的是该次交付在 ingress 协议下的形态，其内容来自已完成的 Client Projection，不等于 Effective Model Request 的 items；ingress 形态改写由 Generation Chain 拥有，Marker 与 Projection Delimiter 规则不属于它。Stravia 始终记录每个这类生成请求；每个请求形成一个可分支的节点。`failed`、取消、客户端断线与 delivery failure 不形成节点。它不拥有可变 Session head 或 Inference Run 生命周期状态。
_避免使用_：Client History（当作独立事实源）；把客户端可见投影当作 Protocol Conversion；把 Client Projection 当作 Generation Chain 的阶段

## Generation Chain Write

Generation Chain Write 是一次尚未落盘的 Generation Chain 节点写入尝试，由进行中的 Inference Run 持有；完整交付后才成为节点，失败或中止则丢弃。Write 拥有节点合法性（仅 `completed` 或 `incomplete` 可落盘，并在 stage 时写入 ingress 协议形态与 Target 身份）；effective request 在观察时必须已是恢复给出的 client-shaped 对照，Write 不回锚 History Marker。Inference Run 只在完整交付后提交，不解释投影或终态合法性。它不是 Model Turn、Agent Turn、Search Turn、Media Understanding Turn，也不是已持久化的 Generation Chain 节点。
_避免使用_：GenerationChainTurn、GenerationChainDraft、Session

## Generation Materialization Cache


Generation Materialization Cache 是 Gateway 进程内、按字节上限淘汰的 LRU 缓存，保存从 immutable Generation Chain delta 精确物化的 execution context。它不是历史事实源；重启或淘汰后，Stravia 必须从 Generation Chain 按父节点顺序重建，不能重跑 Hook。

## Automatic Parent Discovery

Automatic Parent Discovery 是在调用方未显式给出父节点时，为 Generation Chain 选择父节点的规则。它只在同一 Principal 内比较严格 canonical 历史前缀，选择最长且仍留下新 input item 的候选；任何语义差异或无候选都创建新根。它不从连接、网络属性或模糊文本推断父节点。

## Upstream Store Hint

Upstream Store Hint 是 Open Responses `store` 传达给 Target 的上游状态保留偏好。它只约束 Provider 是否可保存或续接其自身状态，不约束 Stravia 对 Generation Chain 的持久化。

## Response Chain

Response Chain 是 Generation Chain 面向 Responses 协议、以平台生成 response ID 连接的投影。客户端可显式引用旧节点，平台也可从 Effective Model Request 发现 Reusable Response Prefix；每个分支开启新的 Inference Run，不共享原 run 的可变生命周期状态。

## Target Continuation

Target Continuation 是由 Target 持有、以上游 response ID 引用的可复用模型状态。它独立于 Response Chain；其保留期、连接亲和与失效由 Provider 契约决定。
当 Reusable Response Prefix 成立时，Stravia 优先使用 Target Continuation；仅作用于已继承历史的新 Prompt Cache Directive 不阻止续接。只有 Provider 明确证明 continuation reference 不存在且本次执行尚未开始时，Stravia 才能完整重放历史；执行状态不确定时不得重放。
_避免使用_：Upstream Session、Response Chain ID

## Reusable Response Prefix

Reusable Response Prefix 是与一个已成功交付的 Generation Chain 节点在模型可见语义上精确相等、且对应 Target Continuation 仍可用的 Effective Model Request 完整 item 前缀。它只在同一 Principal 与等价 Target 语义内成立；Prompt Cache Directive 不属于模型可见语义，不改变该前缀。
_避免使用_：最长链、文本前缀、Session 命中

## Open Responses Protocol

Open Responses Protocol 是 Stravia 以官方日期版本为 canonical baseline 的多供应商 Responses wire contract。Ingress 兼容 rolling Responses 的 additive 字段和 hosted tool 声明：同协议 Target 原样保留，跨协议 Target 在不改变任务内容、工具硬要求或成本上限时可以省略。它不等同于任何单一供应商的滚动产品协议。

## Namespaced Extension

Namespaced Extension 是 Open Responses Protocol 核心契约之外、由一个平台或供应商明确拥有的可选能力。扩展身份使用所有者的 canonical slug；Stravia 自有扩展使用 `stravia:*`。

## Representability

Representability 是一段 canonical 语义能否在目标协议中等价表达的性质。它区分等价转换与会改变任务内容、身份、结构或硬约束的 lossy conversion。

## Context Rewrite

Context Rewrite 是 hook 对一段连续历史的稳定语义替换。替换依据原始上下文的稳定身份命中，并在历史仅追加后继续生效。

## Lab

Lab 是 Canonical Model 的作者身份，也是 Canonical Model ID 的命名空间；它不同于负责提供模型访问渠道的 Provider。
_避免使用_：Provider、Vendor

## Canonical Model

Canonical Model 是归属于一个 Lab、独立于具体 Provider 的模型身份与共享规格；其 ID 固定为 `{lab_id}/{model_id}`，多个 Provider Catalog Entry 可以关联同一个 Canonical Model。它不同于负责客户端路由的 Route。
_避免使用_：Base Model、模型数据元、Catalog Model

## Provider Catalog Entry

Provider Catalog Entry 是共享目录中某个 Provider 对一个 upstream model ID 的完整供应记录；它已经包含 Canonical Model 共享规格与该 Provider 的覆盖，并通过 canonical identity 关联前者。它不属于任何已保存的 Provider 实例。
_避免使用_：Provider Offering、Catalog Model

## Provider Model

Provider Model 是属于一个已保存 Provider 实例、以 upstream model ID 标识的持久化模型快照；它不同于共享 Provider Catalog 条目，也不同于负责客户端路由的 Route 和 Target。
_避免使用_：Provider Model Override、Catalog Model

## Selection Policy

Selection Policy 是管理员对 Provider Model 新路由候选资格的本地策略，取值为自动跟随、强制可用或强制禁用。
_避免使用_：Enabled、Availability Override

## Effective Availability

Effective Availability 是 Selection Policy、Provider discovery presence 与上游生命周期共同计算出的“可用/禁用”结果；它只约束新的路由候选，不改写已有 Target。
_避免使用_：Provider Status、Model Status

## Route

Route 将 Route ID 映射到一组 Target，并规定这些 Target 的选择策略。

## Route Scheduling Strategy

Route Scheduling Strategy 是同一 Target Priority 组内选择 Target 的策略，取值为 Traffic Equalization 或 Latency Preference；缺省为 Traffic Equalization。它不替代 Target Continuation、Conversation Affinity、Cache Affinity 或 Target Priority。
_避免使用_：balance、weighted、priority（当指 Route 旧四档）、cooldown（当指 Route 旧四档）、平均调度、Cost Equalization

## Traffic Equalization

Traffic Equalization 是把下一个请求分给同组内近期加权 token 流量最低的 Target 的调度策略。它均衡的是流量，不是美元成本。
_避免使用_：平均调度、Cost Equalization、weighted、round-robin、计费


## Latency Preference

Latency Preference 是按上游近期 Token 速度与成功率选择同组 Target 的调度策略。
_避免使用_：latency（当指 Route 旧四档）、EMA 延迟


## Route ID

Route ID 是客户端请求里填写的模型 ID。它是调用身份，不是展示标签，也不是存储主键。查找与绑定都按精确、大小写敏感比较；存储主键不是客户端查找键。一键接入时默认等于 Provider Model 的 upstream ID；同一 Route ID 即同一 Route。
_避免使用_：Route 名、展示名、昵称、大小写折叠、用存储主键兜底查找

## Model Display Name

Model Display Name 是 Route 可选、可重复的人类可读标签。它不参与路由、授权、绑定或 Target 选择；为空时，面向人的展示统一回退到 Route ID。
_避免使用_：Route ID、模型身份、查找键

## Conversation Affinity

Conversation Affinity 是客户端给出 Generation Chain 父节点或 Prompt Cache Directive 路由键时，对同一身份优先选择该身份上次成功 Target 的软性偏好。不同身份不共享该偏好；两种身份都没有时不生效。它按 Principal 与 Route 隔离，可压过 Target Priority，不形成 Session，也不替代 Target Continuation。
_避免使用_：Session、Session Affinity、Client Session

## Cache Affinity

Cache Affinity 是在 Conversation Affinity 不生效时，为提高上游 Prompt Cache 复用率施加的软性偏好。它按 Principal 隔离；对一个由 Canonical Item Hash 顺序组成的 Cache Prefix，它优先选择曾成功处理该前缀、且报告 `prompt_tokens` 不少于 20,000 的合格 Target；没有合格命中时，回退到 Target Priority 与 Route Scheduling Strategy。它可压过 Target Priority，不形成客户端、连接或 Session 绑定，不改变 Effective Model Request，也不复用响应或 Target Continuation。

## Canonical Item Hash

Canonical Item Hash 是单个 `AiItem` 语义 canonical 序列化的 SHA-256 摘要。Cache Affinity 记录请求中每个 `AiItem` 的 Hash，并从有序 Hash 序列推导 Cache Prefix；它不记录 raw wire 文本，也不把 Hash 写入日志。

## Cache Prefix

Cache Prefix 是 Effective Model Request 中可由上游 Prompt Cache 复用的连续开头语义。它只由可缓存 material 与明确影响目标 Provider 缓存匹配的控制正向构成，不通过序列化完整请求后排除无关字段来构造，也不按原始 wire 文本拼接匹配。

## Prompt Cache

Prompt Cache 是上游 Provider 对已计算输入前缀的复用。它由 Provider 拥有；Stravia 仅以 Cache Affinity 提高请求到达该 Target 的可能性，不读取、构造或保证其中的内容。

## Prompt Cache Directive

Prompt Cache Directive 是调用方对 Prompt Cache 的可选策略提示，包括 Cache Breakpoint、保留期、模式与路由键。它不属于模型可见历史，不改变 Reusable Response Prefix；Target 无法在 continuation delta 中表达作用于已继承历史的 Directive 时，Stravia 允许降级该提示，不为执行它放弃可用的 Target Continuation。

## Cache Breakpoint

Cache Breakpoint 是 Prompt Cache Directive 中标记可缓存前缀结束位置的提示。它使从 prompt 开头到该位置的完整 material 成为缓存候选；它不是缓存内容、缓存身份、缓存命中结果或 continuation 节点。

## Cache Prefix Token Count

Cache Prefix Token Count 是 Target 在成功处理 Cache Prefix 后报告的 `prompt_tokens`。只有已知的 Cache Prefix Token Count 可以满足基于 token 的 Cache Affinity 阈值；未报告计数的前缀不满足该条件。


## Target

Target 是 Route 中一个可尝试的上游目的地。只有当前 Target 的上游失败被明确判定为可重试时，当前 Run 才会按 Route 的选择策略尝试下一个 Target；Hook、Platform Tool、状态不变量错误与取消不会触发 Target 切换。

## Target Priority

Target Priority 是 Target 上的分组整数，取值 0–100000，缺省为 0；数值越高越优先。相同数值的 Target 属于同一优先级组。它不是列表顺序，也不是 Weight。
_避免使用_：列表序号、唯一排名、Weight（当指优先级）


## First Token

First Token 是当前 Target 本次尝试从上游收到的第一个 canonical 输出，包含 Thinking。它不是 Client Output Commit，也不要求该输出已经对客户端可见。
_避免使用_：首个可见 Text、Client Output Commit

## First Token Timeout

First Token Timeout 是 Target 在发出上游请求后等待 First Token 的最长时限；缺省 60 秒，0 表示关闭。超时按瞬时失败处置。
_避免使用_：总超时、Connect Timeout

## Target Cooldown

Target Cooldown 是 Target 在被本次请求放弃后，一段时间内不再承接新请求的状态；缺省 120 秒。它不阻止当前请求的同 Target 重试。
_避免使用_：HealthRegistry、熔断（当指这个冷却）

## Target Retry Budget

Target Retry Budget 是瞬时失败时在更换 Target 前对同一 Target 的额外尝试次数；缺省 5 次（含首次共 6 次），间隔指数退避并 full jitter。
_避免使用_：Route 重试、循环重试


## Client Output Commit

Client Output Commit 是一次 Run 的输出首次不可逆地对客户端可见的时点。此前，当前 Target 的明确可重试上游失败可以触发 Target 切换；此后禁止切换 Target。它不表示响应正文已完整交付，完整交付只在正文成功结束时成立。
_避免使用_：Output Started、Response Committed、Delivery Commit


## Protocol Conversion

Protocol Conversion 是在改变客户端与上游 wire protocol 时保留一次推理的 canonical semantics。目标协议无法表示任务内容、实际使用的工具、身份、结构或硬约束时必须拒绝；additive metadata、响应装饰和未被强制选择的 hosted tool 可以兼容性省略。它不把响应投影成客户端历史。
_避免使用_：把硬约束丢失称为兼容、把客户端可见投影当作 Protocol Conversion 的阶段

## Thinking Level

Thinking Level 是一次推理请求选择的规范思考强度，取值为 off、minimal、low、medium、high、xhigh、max。关闭档的规范名是 off，与 OpenAI 协议及现有 IR 中的 none 表示同一档；它不是任何协议的原生字段名。
_避免使用_：Reasoning Effort（当指规范档）、Think Level、Client Thinking

## Thinking Level Map

Thinking Level Map 是一个 Target 拥有的、从 Thinking Level 到该 Target 原生思考控制的对照表。
_避免使用_：reasoningEffortMap、Reasoning Options Map、Route Thinking Map

## Generated Mapping

Generated Mapping 是 Thinking Level Map 中仍由 Provider Model 快照的 reasoning_options 推导、可在该快照显式 re-import 时重算的一行。
_避免使用_：Synced Mapping、Catalog Mapping

## Overridden Mapping

Overridden Mapping 是 Thinking Level Map 中被手改过、不再跟随目录推导的一行。
_避免使用_：Frozen Mapping、Manual Mapping

## Hidden Mapping

Hidden Mapping 是 Thinking Level Map 中不产出 Target Thinking Control 的一行；该 Thinking Level 对该 Target 不可见。它必须显式记录，不能靠缺 key 表示。
_避免使用_：Omitted Mapping、Null Level、Unsupported Level（当指对照表行）

## Supported Thinking Levels

Supported Thinking Levels 是一条 Route 的所有 Target 均能执行的 Thinking Level 子集，供管理面、模型发现面和请求钳制使用。它始终由各 Target 非 Hidden Mapping 的交集派生，不由管理员单独配置；任一 Target 缺少某档 Target Thinking Control，该档就不受 Route 支持。它不是 catalog 的 reasoning_options，也不是对照表里的上游值。
_避免使用_：Advertised Thinking Levels、Visible Thinking Levels、Supported Reasoning

## Target Thinking Control

Target Thinking Control 是 Thinking Level Map 为某个 Thinking Level 产出的、该 Target 原生的思考控制（effort 值、toggle 或 budget）。它不是客户端请求字段，也不是 Protocol Conversion 写出的 egress wire。
_避免使用_：Egress Wire Value、Native Reasoning Option、Mapped Effort

## Provider

Provider 是管理员保存的一条上游连接，包含 Catalog 身份、Vendor、Protocol、base URL 与 Adapter Credentials；base URL 是保存时由 Vendor 从 Adapter Credentials 派生的快照，管理员显式覆盖时保存覆盖值，推理期间不重新派生。它不是 Provider Catalog Entry，也不是客户端 Route。
_避免使用_：Vendor（当指这条保存记录）

## Provider Allowance

Provider Allowance 是上游 Provider 对当前账户报告的可消费额度快照，包括订阅配额窗口、请求额度与账户余额；它不等于 Stravia 从请求日志汇总的 token、请求数或成本统计，也不改变 Provider 的路由资格或健康状态。
_避免使用_：Provider Plan Usage、Token Usage、Request Usage、Stats Usage

## Provider Allowance Monitor

Provider Allowance Monitor 是 Stravia Core 按 Provider 的 Catalog 身份选择、读取并规范化上游额度的受信实现；只有启用且命中本地 Monitor registry 的 Provider 才具有 Provider Allowance。Monitor 只使用该 Provider 已保存的 Adapter Credentials 或 OAuth Credential。
_避免使用_：Catalog Allowance Capability、Quota Endpoint、远端解析规则

## Allowance Item

Allowance Item 是 Provider Allowance 快照里账户级的一条可消费额度，是配额窗口、请求额度或账户余额之一。它以 Monitor 给出的稳定 key 标识，归属一个 Provider。模型级额度不是 Allowance Item，不进入额度矩阵、重置时间轴或预计耗尽。
_避免使用_：Quota、把 Model Allowance 当作账户级行

## Allowance Condition

Allowance Condition 是 Allowance Item 的展示态，由当前快照的剩余或已用比例派生：已用 ≥ 100% 或剩余 ≤ 0 为耗尽，剩余 < 20% 为紧张，其余可计算的为正常。它不是 Provider 健康，不落盘，也不改变路由资格。
_避免使用_：Health、Provider Status、把 fresh/stale/error 叫作成色

## Allowance Sample

Allowance Sample 是一次成功 Monitor 读取后，对一个 Allowance Item 在某一时刻的 used / remaining / reset 观察记录。它只服务于趋势和预计耗尽，保留 14 天；它不是 live 快照，不是 Stats Usage，也不表示 Provider 健康或路由资格。
_避免使用_：Provider Allowance Snapshot、用量历史、Quota History

## Exhaustion Forecast

Exhaustion Forecast 是对一个 Allowance Item、用当前重置窗口内 Allowance Sample 做线性外推后得到的耗尽估计：是否会在 reset_at 之前耗尽，以及窗口结束时的预计剩余。它不是 Stats Usage；跨重置的 Sample 不参与计算，样本不足时不给出估计。没有 reset_at 的余额只估计何时到 0，不使用「重置前」语义。
_避免使用_：7 日均用量、Usage Forecast

## Vendor

Vendor 是按 npm 包标识的上游运行时适配器；同一 npm 的多个 Provider Catalog Entry 共用它。它拥有 Adapter Credentials 校验、base URL 组装、鉴权与供应商 headers，不拥有 wire codec，也不执行 npm 包。
_避免使用_：SDK、Provider Adapter

## Adapter Credentials

Adapter Credentials 是一条 Provider 上由其 Vendor 声明的多字段上游凭据；它不同于客户端 API Key，也不同于 OAuth Credential。

## Media Understanding

Media Understanding 是针对一个或多个多模态 Artifact 回答开放问题的平台能力。它面向图片、PDF、视频、音频等内容持续扩展；文字提取、描述、比较与推理都是同一能力的用例，不形成彼此重叠的专用工具。用户可见名称为“多模态理解”。
_避免使用_：Image Understanding、Vision、OCR Tool

## Media Understanding Turn

Media Understanding Turn 是由内置 Media Understanding Definition 提交的 Agent Turn，也是一次完整或有效 partial 媒体理解的不可变续接点。它复用 Turn Chain 的 Principal、父链、分支与保留期语义；后续 Turn 只追加 Artifact 和问题，不改写已有前缀。

## Media Report

Media Report 是 Media Understanding 返回的强校验工具结果，由 Markdown answer、实际引用的媒体 Artifact 和 limitations 组成；它不同于读取该工具结果的父模型最终回答。

## Media Derivative

Media Derivative 是 Media Understanding 为一个源 Artifact 生成并复用的内部、write-once 规范化表示。Media Report 始终引用源 Artifact；同一源 Artifact 的 Derivative 内容不会被重算或替换。

## Web Search

Web Search 是把查询转换为有来源 Search Report 的平台能力。用户可见名称为“联网搜索”；公开工具名是 `web_search`；具体执行由管理员选择的 Search Backend 拥有。

## Search Backend

Search Backend 是 Web Search 的执行方式，当前取值为 Local Agent 或 Codex Agentic Search。一个根 Search Turn 固定其 Backend 和模型绑定，后续续接不会随管理员配置切换。Local Agent 使用平台配置的研究预算；Codex Agentic Search 不受 Stravia 研究步数和总时长预算控制，但仍受调用方取消、请求生命周期和传输安全边界约束。

## Search Report

Search Report 是 Web Search 返回的强校验工具结果，由 Markdown answer、实际引用的 Search Sources 和 limitations 组成；它不同于读取该工具结果的父模型最终回答。

## Search Turn

Search Turn 是一次完整或有效 partial Web Search 提交的不可变续接点。它复用 Turn Chain 的 Principal、父链、分支与保留期语义，但不保存网页正文或内部 Agent transcript。

## Search Source

Search Source 是 Search Report 实际引用、且可追溯到当前或历史已验证 Web evidence 的公网 URL。未被 Report 引用的 consulted URL 不是 Search Source。

## Web Access

Web Access 是管理员配置的内部联网能力，为 Local Web Search 提供统一的 Internal Web Search 与 Internal Web Fetch leaves；它不再直接形成公开工具契约。
_避免使用_：Web Search Add-on、Web Tools

## Internal Web Search Leaf

Internal Web Search Leaf 是 Local Agent 执行 Web Search 时使用的隐藏搜索工具，内部 Agent 可见名称为 `web_search`；它不进入客户端或 MCP discovery。

## Internal Web Fetch Leaf

Internal Web Fetch Leaf 是 Local Agent 执行 Web Search 时使用的隐藏页面读取工具，内部 Agent 可见名称为 `web_fetch`；它不进入客户端或 MCP discovery。

## Fetched Page

Fetched Page 是一次公网 HTTP(S) URL 的完整主内容抽取结果，包含请求 URL、最终 URL、title 和 Markdown 正文。抽取不足时仍返回较好的一份 Markdown，并附带 limitations。给模型看的字符窗口由 Web Access 截断，不属于 Fetched Page，也不通过 Artifact 或续接身份读取剩余。
_避免使用_：Artifact、Document、Markdown Document、Article

## Static Extraction

Static Extraction 是对 HTTP 响应 HTML 做正文抽取并得到 Markdown，不执行 JavaScript。
_避免使用_：SSR、静态页面

## Rendered Extraction

Rendered Extraction 是对浏览器渲染后的 HTML 做正文抽取并得到 Markdown。
_避免使用_：SPA、动态页面、浏览器抓取

## Extraction Fallback

Extraction Fallback 是 Static Extraction 得到的 Markdown 属于 Low-Quality Extraction 时，对该 URL 再做一次 Rendered Extraction。
_避免使用_：重试、Provider fallback

## Low-Quality Extraction

Low-Quality Extraction 是被判定为壳页或导航页的 Markdown：过短且含 JavaScript 门提示，或短行占比过高。它触发 Extraction Fallback，不是页面架构。
_避免使用_：SSR、SPA、低质量页面

## Web Provider

Web Provider 是 Local Web Search 中执行 Internal Web Search、Internal Web Fetch 之一或两者的已配置上游；它独立于模型路由的 Target，也不包含 Codex Search Backend。每条记录拥有与模型 Provider 同构的 `use_proxy`：开启时必须有可用的 Gateway `proxy_url`，关闭时直连。
_避免使用_：Search Engine、Web Backend

## Local Web Provider

Local Web Provider 是在进程内执行 Internal Web Search 和/或 Internal Web Fetch 的 Web Provider，不使用第三方 search/fetch API key。每个部署恰好一条且不可删除；不用它时从 search/fetch 有序列表移除，而不是销毁记录。它拥有是否经 Gateway 代理出站的开关，以及各 Local Search Engine 的配置。该开关与模型 Provider 的 `use_proxy` 同构：开启时必须有可用的 Gateway `proxy_url`，关闭时直连。它仍会向公网发出查询和抓取；它不是本地网页索引，也不是 Search Backend，也不是独立于 Web Provider 记录的内置运行时。
_避免使用_：元搜索、Metasearch、Search Engine、Local Search、Local Search Backend、内置 Local 运行时

## Local Search Engine

Local Search Engine 是 Local Web Provider 在 Internal Web Search 中查询的一个公网 HTML 检索服务。它的配置属于那条唯一的 Local Web Provider 记录，包括启用与否以及仅由该引擎解释的私有设置；私有设置可以包含须按凭据保管的值。该记录必须至少启用一个 Local Search Engine；要停用 Local 搜索时从 search 列表移除，而不是关光引擎。它不是 Web Provider，不是 Search Backend，也不是 Search Report 引用的 Search Source。
_避免使用_：Search Engine（当指 Web Provider）、Metasearch、引擎 Provider、全局 cookie jar、把计算器或页面后处理当作 Local Search Engine

## Local Web Outbound Proxy Mode

Local Web Outbound Proxy Mode 是 Local Web Provider 对 Internal Web Search、Static Extraction 与 Rendered Extraction（含页面子资源）的单一出站结果，由该记录的 `use_proxy` 与 Gateway `proxy_url` 派生：关闭则直连，开启则全部走 `proxy_url`。它不是独立的 Direct/System/Explicit 管理面选项，也不是操作系统 GUI、PAC 或 WinHTTP 代理。
_避免使用_：System 代理档、独立 Local 代理 URL、wreq proxy、browser proxy、系统代理（未限定时）

## Advanced Capability


Advanced Capability 是面向用户提供、由平台级总开关控制可用性的高级能力；当前包括用户可见名称为“多模态理解”的 Media Understanding，以及用户可见名称为“联网搜索”的 Web Search。

## Platform Capability Gate

Platform Capability Gate 是单个 Advanced Capability 的平台级总开关。总开关打开后，该能力对所有有效 API Key 可用；总开关关闭时，任何 API Key 都不能使用该能力。它同时约束显式工具调用与 MCP；API Key 不再单独授予或撤销 Media Understanding、Web Search。

## Transparent Injection

Transparent Injection 是平台在客户端未显式声明工具时，按 API Key 配置把可用 Advanced Capability 自动暴露给模型请求的行为。它独立于 Platform Capability Gate 和 MCP 访问；关闭总开关的能力不能被透明注入。透明注入关闭不影响客户端显式调用已开启的平台能力。总开关关闭期间，API Key 已选择的注入项保留但不生效，重新打开后恢复。
