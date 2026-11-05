---
status: accepted
---

# 按规则开关与强度分层管理凭据保护，并按最近使用回收映射

## 背景

凭据保护需要让管理员停用不适合本实例的内置规则，且开关必须覆盖该规则已经发现的秘密，而不是只影响下一次检测。映射仍须记住秘密字面值：检测依赖上下文，模型与工具后续可能只返回裸 token。所有秘密一律子串替换又会让短值 `pass` 误伤 `password`，因此复用匹配需要区分强弱。

[ADR-0089](0089-share-content-store-and-normalize-observation-writes.md) 让映射受 Turn 节点引用保护，无引用时按首次发布加 N 回收。凭据的跨对话复用不限于某条 Generation Chain，首次发布时间不能表达最近使用；同时，保留越多长期不用的明文映射，本地秘密存储边界越大。本决定保留 Principal 隔离与按引用保护，增加规则级联、强弱匹配和空闲过期。

## 决定

### 规则开关与映射级联

- 设置键 `reversible_redaction_disabled_rules` 保存内置规则 ID 的 JSON 字符串数组，缺省或空数组表示全部启用；非法值失败关闭。只接受内置目录 ID，`custom.*` 不属于此集合，自定义规则仍用各自的 `enabled`。通用设置接口不能写此键。
- 禁用的内置规则不再产生新命中，但仍可作为其他规则的组件提供前置证据，只是不再上报自身秘密。父规则由自身开关独立控制。
- 映射创建时的 `rule_ids` 保持不变。出站字符串替换有效当且仅当该集合为空（迁移前无法归因），或其中至少一条规则当前启用：内置规则不在禁用集，自定义规则必须存在且 `enabled`。因此停用或删除自定义规则也级联影响旧映射。本次被启用规则重新检出的秘密仍照常替换，即使旧映射的归因只含已禁用规则。
- 禁用、停用或删除只停止相应映射的出站替换与管理端观测遮罩，不删除映射，不阻止模型回答、客户端工具参数与平台工具参数中的旧占位符还原。命中位置回看（`locate_text`）不受禁用影响。
- 规则状态改变出站视图：同一段历史在禁用前后发往 Provider 的文本不同。Provider 侧续接只复用与当前 Provider 可见历史相等的前缀，规则状态变化后自然退回完整历史重发，不需要额外的失效机制；Provider 已缓存的旧前缀不再命中。
- `GET /api/v1/reversible-redaction/rules` 的 `data.rules[]` 增加 `enabled`。`PUT /api/v1/reversible-redaction/disabled-rules` 接受 `{ rule_ids: string[] }` 幂等替换整集合，返回 `{ data: { rule_ids: string[] } }`，去重并排序。未知 ID（含 `custom.*`）返回 400 `credential_rule_unknown`，带 `params.rule_id`；不可用返回 503 `credential_disabled_rules_unavailable`。自定义规则接口不变。

### 强弱秘密共用匹配语义

秘密强度只由字符串本身决定，不入库，不改变映射身份：字符数 ≥16 为强，<8 为弱；8–15 个字符时，小写字母、大写字母、数字、其他符号四类中出现至少三类为强，否则为弱。

强秘密任意位置出现都按子串替换。弱秘密只在词边界替换：若秘密首字符属于 ASCII 单词字符（`A-Za-z0-9_`），其前一个字符不能是单词字符，尾字符同理。`password`、`bypass` 中的 `pass` 不替换，`pass,` 仍替换；中文等非 ASCII 字符不算单词字符。自定义简易规则的检测、字符串替换与管理端观测遮罩共用同一强弱与边界判定；占位符还原不分强弱。没有新增 API。

### 按最近使用回收映射

N 复用 `log_retention_days`（默认 7 天，最小 1 天），不另设映射保留时长。创建算使用；后续使用指映射占位符出现在成功完成的模型回合共享 trace 中，包括出站替换、客户端重放历史与模型响应还原，回合末 `publish` 登记。管理端查看与观测遮罩不算使用。

迁移 0045 同时覆盖 SQLite/PostgreSQL：`reversible_redaction_mappings.updated_at` 改名为 `last_used_at`，已发布行的 `last_used_at` 置为 `published_at`，使存量映射的到期时刻保持升级前的 `published_at + N`，既不缩短也不让已到期的行复活（旧 `updated_at` 不是使用记录）；`published_at` 保留，仅表示已发布过；回收索引由 `reversible_redaction_mappings_published_at_idx` 换成 `reversible_redaction_mappings_last_used_at_idx`；水位 `reversible_redaction_mappings.published_at` 改名为 `reversible_redaction_mappings.last_used_at` 并重置为最小值。

续期搭既有 `publish` UPDATE 的便车，不新增独立事务。G 为一小时，条件 `published_at IS NULL OR last_used_at < now - G` 让同一映射 G 内重复使用只读不写；写前先只读筛选需要首次发布或续期的引用，没有则不开写事务，避免 SQLite 抢唯一写锁并同步 WAL。

已发布映射只有在无 `turn_node_redaction_mappings` 引用且 `last_used_at + N + G` 已过时才回收；未发布映射仍按一小时 `pending_until`。G 补偿节流导致记录时间早于最后真实使用的偏差，保证最近使用后至少保留 N 天，无引用时至多约 N 天 + G + 一个扫描周期。Turn 节点引用仍由 RESTRICT 外键保护。`active()` 不增加时间比较，到期但未被扫描删除的映射仍可用。

回收继续使用持久化水位范围扫描，视界为 `T = now - N - G`；续期写入的新值总大于当前视界，留待未来范围扫描，已入队又续期的行在领取时按新值重检后保留。每轮成本取决于本轮到期或释放引用的行数，不每轮重扫全部映射。该路径的性能尚未实测，不用既有 Observation 或 Turn Chain 测量外推数量级。

## 取舍

- **不只用规则、放弃字符串复用。** 规则依赖字段名、上下文与组合证据，认不出上下文的裸 token 会明文外发。检测扫描还可能包含上下文前缀、解码或规范化后的文本，扫描偏移不一定对应最终写回文本，不能简单按检测偏移替换来代替已知秘密字典。
- **强秘密不加词边界。** 宁可多换不漏；紧贴单词字符的字面 `\\nSECRET` 中，反斜杠后的 `n` 是 ASCII 单词字符，统一加边界会漏替换 `SECRET`。强度只是一条确定性匹配策略，不是对凭据熵或有效性的验证。
- **弱秘密有残余风险。** 边界减少短值误伤，但中文短词无法靠 ASCII 边界与更长词区分。检测器若在紧贴单词字符处捕获弱秘密，替换侧可能因边界不成立而漏替换，明文仍可能出站；不为此给个别输入加例外。
- **空闲过期复用 N。** 若映射窗口小于历史保留 N，观测遮罩依赖的映射可能先于对应观测记录消失。共用 N 不把 Observation 变成映射引用方，也不让管理端查看无限续期秘密。
- **有意局部取代 ADR-0089。** 映射不再按首次发布加 N 回收，但不恢复祖先续期、每次 Generation 提交的独立续期事务或全量重扫。续期搭已有 publish 写入且按 G 节流，写前只读预检；其写放大机制不会以旧形式回来，实际性能仍待测量。History Marker 与原生压缩登记不变。
- **过期有保护代价。** 同一凭据过期后被规则重新检出会生成新映射与新占位符；规则认不出的裸出现原样发往上游。禁用规则也有同样明确的出站保护取舍，不能当作凭据撤销；映射与占位符仍按 Principal（API Key）隔离，非按对话隔离。

## 与既有决策的关系

- [ADR-0042](0042-keep-reversible-redaction-detection-local.md) 的本地检测边界不变；规则目录增加启用状态，组件证据仍完整求值。
- [ADR-0043](0043-scope-redaction-mappings-to-api-keys.md) 的 Principal 隔离、同 Key 复用身份与本地存储安全边界不变；出站复用增加规则开关条件，已发布映射首次发布加 N 的回收约定被本决定局部取代。
- [ADR-0046](0046-complete-model-turn-after-redaction-publication.md) 的成功终态 gate 与取消裁决不变，最近使用登记搭其 publish 步骤。
- [ADR-0083](0083-record-credential-hits-with-reuse-and-block-locators.md) 的创建归因 `rule_ids` 与块级定位不改写；当前规则状态另外参与出站替换裁决，历史归因不等于永远启用。
- [ADR-0088](0088-mask-credentials-at-admin-boundary-and-page-reuses.md) 的后端遮罩、显式显示明文与复用分页契约不变；观测遮罩增加规则过滤及强弱边界，映射保留依据改为最近使用。
- [ADR-0089](0089-share-content-store-and-normalize-observation-writes.md) 中「已发布依赖按首次发布或确认 + N」仅对凭据映射局部被取代，映射水位与索引随之替换；History Marker、原生压缩登记、Turn 引用保留、共享正文及扫描作为唯一删除方的规则不变。
