---
status: accepted
---

# 命中记录按新增映射逐条列出，并从保留正文还原命中位置

## 决定

[ADR-0083](0083-record-credential-hits-with-reuse-and-block-locators.md) 把命中记录做成 Interaction 级摘要：新增与复用合并为一行、不保存片段、占位符只在 Debug Run 记录。这回答不了管理员真正要问的问题：「这个映射是被哪条规则、在哪个字段、什么内容里创建的，之后又在哪里用到」。现决定：

- **一行 = 一次新增映射。** 命中记录从 `credential_mappings_created` 事件的 `discoveries[]` 逐项投影，每项一行：创建时间、API Key、规则集（内置 / 自定义，由规则 ID 前缀 `custom.` 推导）、创建规则、命中（规则 ← 来源）。创建映射只是绑定，行上不带请求状态，也不再有 `kind`、`status`、`observation_gap`、`new_credential_count`。复用不再是行，只在行的侧栏里作为该映射的后续使用列出。
- **占位符对所有 Run 记录。** 事件载荷的 `marker`（= 映射 `reference`）不再只限 Debug Run；`created` 与 `reused` 事件都记录。同一占位符即同一映射，用它把复用事件关联回创建行。升级前的事件没有占位符（Debug Run 除外），侧栏提示无法关联复用，不回填。
- **命中位置按需从保留正文还原，不新增明文存储。** 侧栏选中一条命中时，服务端按 `(source_type, tool_id, latest_input)` 找回 Observation 已保留的条目正文，对它运行与「密钥匹配测试」同一检测器，返回完整条目文本与匹配（含起止行列）。能取到映射明文时，只把秘密字面值等于该映射的匹配标为命中；取不到（映射已清理、无占位符）则退化为按所选规则标记，并如实标示。
  - 工具参数、工具结果：事件里的工具输入 / 返回。
  - 最新用户输入：`input_preview_recorded`，最多 4,096 字符。
  - system、历史用户消息、其他文本：Observation 不保留，显示「正文不可用」。
- **复用在写入时登记，读取时只做索引查找。** 写 `credential_mappings_reused` 事件的同一事务里，按 `(占位符, Interaction, 来源, 工具调用或最新输入)` 向 `credential_mapping_reuses` upsert 一行，保留最大 `last_reused_at` 与对应 Run；侧栏按占位符读取，不显示次数。事件载荷是 `storage_codec` 编码，SQL 无法按占位符过滤，早先「读时解码事件」的方案要线性扫描该主体的全部复用事件，且设了 50,000 事件的硬上限——按序号升序扫描，触顶丢掉的恰是最新的复用。登记后没有扫描上限，唯一的上限是展示的 200 个位置，超出时 `truncated=true`。
- **列表索引替换。** `observation_events_credential_discovery_idx`（`kind, interaction_id, occurred_at`）服务于旧的 Interaction 聚合，换成只覆盖创建事件的部分索引 `observation_events_credential_hit_idx`（`kind, sequence DESC`），支持按 `sequence` 倒序翻页；复用事件不再被任何查询读取，不进索引。
- **Kingfisher 规则 ID 从存量数据中移除。** 内置规则集只剩 Betterleaks（[ADR-0042](0042-keep-reversible-redaction-detection-local.md)）。迁移 0035 删除映射表 `rule_ids` 里的 `kingfisher.*`；同一次升级中，在 0035 应用之前，分批重写 `credential_mappings_created` / `credential_mappings_reused` 事件载荷，去掉 `kingfisher.*` 的规则与位置，删除变空的映射项与变空的事件。这样界面没有「已移除规则集」的特殊分支。

## 取舍

- 重写 Observation 事件打破了「事件只追加」。这是一次性的数据清理而非行为路径：只删除一个已不存在的规则集的归因；仅由 Kingfisher 规则创建的映射继续保护与还原，但归因为空，因此不再产生复用记录，其命中记录行消失。
- 管理端响应现在返回含明文的条目正文。这些正文本来就在普通 Observation 与请求记录里以原文保留（普通 Observation 只擦除 `Authorization`），密钥匹配测试也直接展示明文，管理端点沿用既有管理鉴权。界面默认遮罩命中片段，点击才显示，关闭侧栏或切换命中后重新遮罩；这是误触保护，不是访问控制。
- 重新运行检测器而不是在写入时保存偏移：保存偏移要求在请求路径上记录片段，违背「事件不含明文」，且偏移相对于提取文本，与 Observation 中存放的 JSON 或渲染文本不一致。代价是命中位置是对保留正文的重算：规则集之后变化或工具输入经 JSON 序列化后转义不同，都可能使结果与创建当时略有出入；映射明文可用时按字面值对齐，把偏差限制在「找不到」而不是「标错」。
- 占位符进入普通事件：它不含秘密，持有它也不能脱离对应主体的有效映射还原，而且侧栏已向管理员展示明文，保密收益有限。换来的是不依赖 Debug 的复用关联。
- 每事件最多 50 个映射的写入上限不变，超出部分不产生行，也不再统计「新增数」。
- 复用登记多一张表，并让每个复用事件的写入事务多一次 upsert；一个事件至多 50 个映射 × 50 个位置，按块写入。这笔写入换掉的是读取时对全部复用事件的线性解码，且事务内同时落事件与登记行，二者不会分叉。不回填：`credential_mappings_reused` 事件与占位符都未随正式版本发布（v0.5.3 不含），没有需要补登的已发布数据。

## 后果

- 取代 ADR-0083 中「Interaction 级聚合与 `kind` 筛选」「复用作为行」「`marker` 只在 Debug Run 记录」「不保存可定位片段」几项。其余仍然有效：`credential_mappings_reused` 事件及其「只记本次新提交文本」的口径、`locators`、映射 `rule_ids`、块级高亮入口 `highlight=credential-hits`。
- `GET /api/v1/reversible-redaction/discoveries` 响应不兼容变更：一行一映射，新增条目详情与复用两个只读端点；不保留旧字段或别名。
- 迁移 0035 同时覆盖 SQLite 与 PostgreSQL，新增 `credential_mapping_reuses` 并替换命中事件索引，参考 schema 一并重新生成。复用行随所属 Interaction 的保留期清理一并删除。
