# 面向 Agent 运行时的长上下文压缩：前沿方案与实证比较

> 调研快照：2026-09-01。范围仅包括 Agent 在单次长程运行中对 action、observation、tool result、计划与工作状态的压缩或折叠。排除普通 RAG、跨任务长期记忆、KV cache 压缩、模型架构扩窗、静态文档 prompt 压缩，以及只改善检索输入的方法。
>
> 数据口径：只引用论文、官方项目页与官方仓库。不同论文的模型、任务、预算和评测器不同，跨论文数字不能直接排行；只有同一论文内的受控对照可作因果比较。

## 1. 结论

没有一个已经证明跨编码、研究、办公自动化、跨模型都最优的方案。当前最有价值的前沿结论是三件事：

1. **运行时架构首选 ACM 形状**：把被移出的原始轨迹永久归档；活动上下文只保留摘要与近期精确尾部；Agent 可按 `summary_id` 查询原文。它避免把有损摘要升级为事实源，并在同一 Qwen3.5-9B 主干的三项受控评测中整体优于 ReAct、ReSum、ACON 和 ACE。
2. **冻结模型的压缩策略优化首选 TRACE/ACON 思路**：不要只用摘要相似度或终局成功率评估压缩。TRACE 在同一环境状态的压缩边界上比较 raw-history 与 compressed-history 后续执行，直接惩罚 blocked action 和重复探索；ACON 用完整轨迹成功、压缩轨迹失败的反例优化结构化压缩规则。二者都不要求修改 Agent 权重。
3. **自有模型训练的上限路线是 Context-Folding/AgentFold/SUPO/MEM1**：让模型学习何时压缩、压缩什么，或把子任务放入临时分支后只返回结果。效果强，但不是协议网关可直接套用的通用中间件；需要特定模型、训练数据、RL/SFT 以及新的执行语义。

对 Stravia 的推荐不是照搬某篇论文，而是组合：

```text
canonical append-only trajectory
  -> deterministic protocol-safe pruning
  -> exact recent tail
  -> typed working checkpoint
  -> archived raw segments addressable by stable IDs
  -> explicit query_memory over plaintext archives
  -> TRACE-style boundary verification
```

**加密或签名 reasoning 不进入本地摘要器。** 它可以原样归档和按 ID 回放，但无法被语义检索或重写；只有原 Provider 的 native compact 能压缩其私有链。跨 Provider 时应从公开 checkpoint 开新链，不伪称保留原私有推理连续性。

## 2. 方案分层

|层级|方案|运行时机制|权重训练|开放性|适合的决策|
|---|---|---|---|---|---|
|平台架构|ACM|Agent 调用 `manage_context` 归档原文并生成摘要；`query_memory` 回查原段|可选；Base 不需要|官方 MIT 代码、数据、checkpoint|最适合作为可恢复 working-context 设计蓝本|
|压缩策略优化|TRACE|同一环境状态做 PRE/POST 闭环续跑，以执行退化优化压缩 prompt|不需要|代码和冻结数据公开，但仓库明确无 license；仅 source-available|最适合作为质量门和离线策略优化方法，不可直接复制分发|
|压缩策略优化|ACON|从完整上下文成功、压缩上下文失败的轨迹优化自然语言 guideline；可蒸馏 compressor|Agent 不需要；compressor 蒸馏可选|官方 MIT 代码|当前最成熟的模型无关、可移植压缩策略优化方案|
|固定触发摘要|ReSum|达到阈值后将 query、证据、缺口和下一步总结，重启 working context|ReSum-GRPO 可选|Apache-2.0 推理代码；专用 ReSumTool 权重发布状态不如代码清晰|简单研究 Agent 基线；不证明编码 Agent 泛化|
|学习式分支折叠|Context-Folding|`branch` 建临时子轨迹，`return` 只把结果折回主线|FoldGRPO|Apache-2.0 重实现；README 明确可能不同于论文训练代码|自有 coding/research Agent 的高上限路线|
|主动多尺度折叠|AgentFold|Agent 自主做 granular condensation 或 deep consolidation|SFT|Apache-2.0 仓库仅含精简推理文件|研究 Agent；复现完整训练链证据较弱|
|端到端滚动摘要|SUPO|摘要步骤并入 MDP，任务 reward 同时训练工具行为和摘要|GRPO/PPO 类 RL|论文公开；未找到作者官方实现|已有 RL 基础设施、希望训练摘要适配策略时|
|常量内部状态|MEM1|每轮用旧 memory + 新 observation 重写固定大小内部状态|PPO/RL|官方 MIT 代码和 checkpoint|模型研发路线；不适合作为跨 Provider 网关层|

## 3. 最强受控证据

### 3.1 ACM：当前最接近平台级正确架构

ACM 提供两个显式工具：

- `manage_context`：总结自上次管理点以来的消息，把原始消息移出活动上下文并写入外部存储，返回 `summary_id`；
- `query_memory(summary_id, query)`：在对应原始消息中查询细节。

论文把这种机制称为“lossless”，准确解释应是：**系统级原文仍在且可回查**；活动摘要本身仍是有损的，检索仍可能漏召回。

同一 Qwen3.5-9B 主干、同一工具与解码配置下的论文主表：

|方法|BrowseComp-Plus Pass@1 / peak tokens|DeepSearchQA Pass@1 / peak tokens|SWE-Bench Verified Pass@1 / peak tokens|
|---|---:|---:|---:|
|ReAct|0.570 / 63K|0.367 / 46K|0.489 / 59K|
|ReSum|0.608 / 68K|0.371 / 79K|0.475 / 61K|
|ACON|0.614 / 65K|0.380 / 54K|0.480 / 57K|
|ACE|0.589 / 71K|0.352 / 70K|0.494 / 65K|
|ACM Base|0.635 / 59K|0.405 / 42K|0.508 / 46K|
|ACM Post-Trained|0.727 / 54K|0.425 / 41K|0.530 / 50K|

这组数据的价值在于对照统一，不在于绝对分数。ACM Base 已在三项任务上同时超过 ReAct 和三种对照；post-training 进一步提升 Agent 判断“何时管理、何时不要管理”的能力。论文案例跨越 222K 原始历史、累计约节省 124K 活动上下文，并始终留在 128K 基础窗口内。

工程代价：

- Base 运行时只需工具、归档、摘要和检索；可先落地。
- 官方 post-training 流程依赖 GPT-5 标注、Qwen3.5-397B-A17B teacher 和大规模 GPU；不是首期工程前提。
- `query_memory` 只适用于平台可读的 plaintext；对 ciphertext 最多按 ID、时间、类型做精确定位。
- 论文承认弱模型可能在需要压缩前就结束或崩溃；上下文管理工具不是弱 Agent 的补救器。
- ReSum/ACON/ACE 是论文作者重实现，可能存在实现差异。

来源：[论文](https://arxiv.org/abs/2607.23809)、[官方 MIT 仓库](https://github.com/lixiaochuan2020/agentic-context-management)。

### 3.2 TRACE：最强的压缩边界验证方法

TRACE 不把“摘要看起来完整”当质量。它在每个真实 compaction boundary 重建完全相同的环境状态：

- PRE：保留压缩前 raw update；
- POST：使用压缩后的 summary；
- 两侧都闭环执行真实工具；
- 统计 POST 相对 PRE 增加的 blocked/error action 和 refetch/replay。

在 590 个 AppWorld 压缩边界、4,640 次续跑中，压缩后首步 blocked/error 平均增加 0.108；五步累计 union burden 增加 0.509。另一个干预实验显示，近期更新原样保留时相对 full-history 的 action-distribution divergence 为 0.149，压入摘要后升至 0.233，完全省略为 0.289。问题不是只有“事实丢失”，还包括 Agent 丢失当前执行位置、重复已完成动作或无法正确终止。

AppWorld test-normal、MiniMax-M3、4,096-token compression window 的受控结果：

|方法|Accuracy|Pass²|Pass@2|
|---|---:|---:|---:|
|No compression|85.7|77.4|94.0|
|最佳既有 compressed baseline（Prompting-O）|71.4|59.5|83.3|
|TRACE|77.1|67.3|86.9|

TRACE 相对 Prompting-O 分别提升 5.7、7.8、3.6 个百分点；硬任务 peak context 少于 full context 的一半，平均执行步数接近 full context。用 MiniMax-M3 优化的 template 未再训练即迁移到 Kimi-K2.7-Code，得到 84.5 Accuracy、79.2 Pass²、89.9 Pass@2；对应 full context 为 82.7、73.8、91.7。该结果只覆盖一个目标模型和 AppWorld，不足以证明普适迁移。

局限：

- 当前 verifier 只捕获显式 blocked/refetch，不捕获 silent state corruption。
- 需要可快照、可重放的环境；真实不可逆工具不能直接做 paired continuation。
- boundary score 只作筛选，最终仍必须多次端到端运行，防止摘要递归消费自身后漂移。
- 官方仓库明确声明**尚无 license 文件**。因此它不是可合法复用的开源实现；只能作为论文、数据和协议参考，等待许可证或独立实现。

来源：[论文](https://arxiv.org/abs/2608.06503)、[官方 source-available 仓库](https://github.com/nokia-applied-research/Trace)。

### 3.3 ACON：冻结 Agent 下最成熟的 guideline 优化

ACON 分别压缩 interaction history 和最新 observation。它从“完整上下文成功、压缩上下文失败”的反例中生成 failure feedback，再迭代自然语言 compression guideline；Agent 权重不变。优化后的 guideline 可用便宜模型执行，也可蒸馏到本地 compressor。

论文跨 AppWorld、OfficeBench 和 8-objective QA 报告：

- peak token 降低 26%–54%；
- 8-objective QA peak tokens 降 54.5%，dependency 降 61.5%；
- 小 Agent 在 AppWorld 由 25.6% 升至 33.9%，相对提升 32.4%；8-objective exact match 由 0.158 升至 0.230，相对提升 45.6%；
- 蒸馏 compressor 保留超过 95% teacher performance。

AppWorld 论文成本表：no compression 为每任务 0.331 美元、73.24 秒；ACON history 为 0.285 美元、87.68 秒；ACON observation 为 0.272 美元、101.92 秒。即 token/cost 下降，但额外 compressor 调用增加端到端延迟。

它比手写摘要 prompt 更可靠，但仍是 summary-only substitution：没有 ACM 的原文查询通道。TRACE 在另一套 AppWorld 配置中也显示 ACON prompt 不一定迁移到新模型，因此应在本平台真实任务上再优化和验证。

来源：[论文](https://arxiv.org/abs/2510.00615)、[官方 MIT 仓库](https://github.com/microsoft/acon)。

### 3.4 Context-Folding：编码 Agent 训练路线的最强直接证据

Context-Folding 不周期性总结整段历史，而是让 Agent 把局部子任务放进临时 sub-trajectory，完成后只把结果折回主线。它减少了摘要整条主线造成的执行位置丢失，也使上下文边界和任务分解边界一致。

官方项目页的同模型受控结果：

|Seed-OSS-36B 方案|活动 peak|总序列预算|BrowseComp-Plus|SWE-Bench Verified|
|---|---:|---:|---:|---:|
|ReAct|32K|32K|28.6|43.6|
|ReAct + GRPO|32K|32K|44.6|48.0|
|ReAct + GRPO 长窗|327K|327K|54.0|57.4|
|Summary + GRPO|32K|32K × 10|52.7|55.0|
|Folding + GRPO|32K|32K × 10|56.7|56.4|
|Folding + FoldGRPO|32K|32K × 10|62.0|58.0|

这是目前最直接的 coding-agent 证据：32K 活动窗口的 FoldGRPO 在 SWE-Bench Verified 达到 58.0，略高于 327K ReAct + GRPO 的 57.4，并高于 rolling summary 的 55.0。

代价：需要 branch/return 执行语义、训练模型和过程 reward。官方仓库是 Apache-2.0，但 README 明确它是基于 veRL 的开源重实现，可能不同于论文训练代码；复现主表仍需独立验证。

来源：[论文](https://arxiv.org/abs/2510.11967)、[官方结果页](https://context-folding.github.io/)、[Apache-2.0 仓库](https://github.com/sunnweiwei/FoldAgent)。

### 3.5 AgentFold、SUPO、ReSum、MEM1：有价值，但适用面更窄

**AgentFold** 让模型在每步自主选择 granular condensation 或 deep consolidation，并保留多尺度摘要与精确最新交互。论文在 BrowseComp/BrowseComp-ZH/WideSearch/GAIA 报告 36.2/47.3/62.1/67.0；100 turn 时平均活动上下文约 7K，相比 ReAct 少 84K、缩小 92%。但它是 web research SFT 路线；公开仓库当前主要是 `infer.py`，完整训练复现性弱于 ACM/ACON/MEM1。来源：[论文](https://arxiv.org/abs/2510.24699)、[Apache-2.0 仓库](https://github.com/Alibaba-NLP/DeepResearch/tree/main/WebAgent/AgentFold)。

**SUPO** 把摘要作为 RL trajectory 的一部分，rollout-level reward 同时优化工具行为与摘要。CodeGym 中 4K working/32K effective 得 47.7%，对照 GRPO 32K 为 44.5%；BrowseComp-Plus 自建 holdout 100 中 64K working/192K effective 得 53.0%，GRPO 64K 为 39.0%。作者明确该 holdout 仅作演示，不能与公开 BrowseComp 分数比较；未找到作者官方代码仓库。来源：[论文](https://arxiv.org/abs/2510.06727)。

**ReSum** 是最简单的外置摘要重启范式。论文报告 training-free ReSum 相对 ReAct 平均提升 4.5 个百分点，ReSum-GRPO 再提升 8.2 个百分点。证据限于 web research；官方代码需要多个模型和外部搜索/页面服务，README 在固定提交仍称 ReSumTool-30B “will be released soon”。来源：[论文](https://arxiv.org/abs/2509.13313v3)、[Apache-2.0 代码](https://github.com/Alibaba-NLP/DeepResearch/tree/f72f75d8c3eb842f2bbbab096a12206ff66e270f/WebAgent/WebResummer)。

**MEM1** 用固定大小内部状态合并旧 memory 与新 observation。MEM1-7B 在 16-objective multi-hop QA 相对 Qwen2.5-14B-Instruct 报告 3.5× performance、3.7× lower memory，并覆盖 retrieval QA、open web QA 和 WebShop。它证明“常量状态 + RL”可行，但所有历史最终都穿过一个有损瓶颈，且需要专有训练权重。来源：[论文](https://arxiv.org/abs/2506.15841)、[官方 MIT 仓库](https://github.com/MIT-MI/MEM1)。

## 4. 对 Stravia 与加密 reasoning chain 的适用性

当前设计要求 canonical parent chain 完整，超窗返回 `agent_context_limit_exceeded`；Provider codec 不执行 compaction；`POST /v1/responses/compact` 明确返回 `unsupported_feature`。新增能力应位于 Agent/ContextBuilder 边界，不应在协议 codec 中静默改写客户端历史。

### 4.1 可直接借鉴

1. **ACM 的双层状态**：canonical raw segment 与 active checkpoint 分离；checkpoint 记录覆盖的 event IDs。
2. **ACM 的显式回查**：只对平台可读的历史提供 `query_memory`；结果重新进入当前精确尾部。
3. **ACON 的 typed guideline**：目标、已完成、当前状态、变量/标识符、失败约束、未决项、下一步、source IDs；避免自由叙事摘要。
4. **TRACE 的边界验证**：对可重放任务，从同一 snapshot 比较压缩前后续跑；监测 blocked tool calls、重复 call signature、错误终止、输出 schema 破坏和任务成功率。
5. **Context-Folding 的局部性**：后续若平台拥有完整 Agent runtime，可把隔离子任务/子 Agent 的内部轨迹折为 typed return value，而不是总结整条主线。

### 4.2 加密或签名链的硬边界

- `reasoning signature`、`encrypted_content`、Provider continuation state 是 opaque protocol state，不是自然语言 observation。
- 本地 summarizer 不得解密、裁剪、改写、合并或伪造这些块；tool call/result 与签名块保持原子边界。
- ACM 式 archive 可以保存 ciphertext 和稳定 ID，但 `query_memory` 不能对 ciphertext 做语义问答。可提供的只是按 ID、Provider、turn、时间或类型精确取回。
- 同 Provider 且 Provider 支持 native compact 时，可把私有链压缩委托给原 Provider，并把返回的 opaque compact state 作为新的 target-specific state；它不能成为跨 Provider canonical fact source。
- Provider 不支持时，只能原样 pin 必需 opaque state，并压缩公开消息/tool history；若 opaque state 本身超限，必须显式失败。
- 跨 Provider continuation 必须从公开 typed checkpoint 开新 reasoning chain。旧 encrypted chain 仍在 canonical log 中供审计，但不能被新 Provider继续推理。

## 5. 推荐实施顺序

### 第一阶段：无需训练的可靠基线

- append-only canonical log；
- 按合法 event/tool-pair 边界切段；
- 大 tool result 外置为有 hash 和 source ID 的 artifact；
- typed checkpoint + exact recent tail；
- raw segment archive 与显式 lookup；
- 原子安装、失败不改变 active view、支持回滚；
- plaintext 与 opaque segment 分 lane。

这相当于 ACM 的可恢复状态形状，加上 ACON 的结构化 checkpoint，但压缩触发先由平台 token budget 控制，不要求 Agent 主动调用。

### 第二阶段：在真实 Stravia Agent 任务上优化

- 收集压缩前成功、压缩后失败的轨迹，做 ACON 式 guideline 优化；
- 对可快照环境做 TRACE 式 PRE/POST boundary replay；
- 评价至少包含 task success、Pass²、多轮 compaction 后漂移、blocked action、重复 tool call、peak tokens、总输入 tokens、额外延迟和 cost；
- 只有在 held-out end-to-end 重复运行上胜出才提升新 policy。

### 第三阶段：仅在平台拥有模型和训练栈时

- 编码/研究 Agent 试验 Context-Folding；
- 固定 rolling-summary Agent 试验 SUPO；
- 不把 AgentFold/MEM1 权重特化路径放进通用协议网关。

## 6. 明确排除

- **LLMLingua、RECOMP、xRAG 等静态 prompt/RAG compressor**：可用于已召回自然语言文档或大型 observation 的内部预处理，但不是 Agent trajectory 状态机。
- **ACE 等 cross-task memory evolution**：优化跨任务 playbook，不是当前 episode 的 working-context 压缩。
- **MemAgent 类长文顺序读取**：主要解决静态长文 QA，不覆盖 action–observation 闭环和工具状态。
- **KV cache 量化、稀疏注意力、模型扩窗、prompt caching**：降低推理成本或扩大窗口，不改变 Agent 可见逻辑历史。
- **只保留摘要且删除 canonical history**：不可恢复、不可审计，也无法修复压缩错误。

## 7. 最终选择

- **平台架构**：ACM 的 archive + summary + query，结合 exact recent tail。
- **冻结模型的压缩策略**：先采用 ACON；有可重放环境后加入 TRACE verifier。
- **编码 Agent 自有模型路线**：Context-Folding。
- **研究 Agent 简单基线**：ReSum；有训练预算再评估 AgentFold/SUPO。
- **禁止项**：对 encrypted/signed reasoning 做本地语义压缩，或在 proxy/codec 中静默重写历史。

当前证据最支持“可恢复分层状态 + 任务反馈优化”，不支持寻找一个通用摘要 prompt，也不支持把某项 web-search SOTA 直接外推到编码或跨 Provider Agent。
