# 凭据保护

凭据保护是模型请求的本地可逆脱敏：以不透明标记替换识别到的凭据，接收模型结果时恢复调用者有权使用的原值。它不是 PII 或业务秘密防泄漏系统，也不是数据库静态加密、凭据撤销、工单审计或工具出站沙箱。

## 作用域与总开关

管理页的**可逆脱敏总开关**对应实例持久设置 `reversible_redaction_enabled`，默认关闭。开启时覆盖全部有效 API Key，不依赖透明注入或入口协议。普通生成、隐藏轮次和 Agent 模型轮次共用 Model Turn 的保护入口，Target 重试复用已准备的请求。

保护对象是模型可读 canonical 文本，包括指令与历史、消息文本、工具定义／参数中的可读字符串和可读工具结果。业务 JSON 遍历字符串值；明确声明为 content blocks 的工具结果只访问已知文本字段。不扫描图片、附件字节、原生密文或不透明载荷，不改协议标识。工具实际执行接收还原后的真实值，仍须遵守自身授权与出站约束。

检测、映射读取／登记、替换、还原或发布失败都**失败关闭**：请求失败，已开始的流走失败终态，不回退发送绕过保护的明文，不提交失败的 Generation Chain。这不承诺检测到每种秘密；未知值或因有效期、规则及边界无法匹配的值可能原样出站。

关闭总开关停止检测及出站替换，不禁止对仍可用的旧占位符还原。保护说明只附加到确实含本 Principal 可还原标记的当前模型请求，不写回客户端历史。

## 规则与强度

[Betterleaks 快照](../../backend/crates/stravia-credential-protection/src/detection/UPSTREAM.json)含 462 条上游离线规则及配套词表。编译时合并经审阅且策略完全一致的 `bittrex-secret-key` → `bittrex-access-key`，有效内置目录为 461 条。运行时不下载规则、词表，不执行上游 `validate` 或联网验证；文件专属条件以空路径求值。

| 规则来源 | 编辑和求值 |
| --- | --- |
| 内置 | `reversible_redaction_disabled_rules` 是禁用 ID 集合；缺省表示全启用，未知 ID 与 `custom.*` 不接受，非法保存值不放行。被禁用组件仍可为其它已启用组合规则提供前置证据，不上报自己的匹配。 |
| 自定义 `simple` | 非全空白的大小写敏感字面值；共用强弱和边界判定。 |
| 自定义 `pattern` | 正则、提取分组、关键词和对提取值的最小熵，保存前用同一检测器校验。 |

自定义 ID 为 `custom.<uuid>`，各自有 `enabled`。简易规则的明文可能就是秘密；仅按已有管理授权读取，列表和搜索索引不展示它。

| 强度 | 规则 | 字面复用 |
| --- | --- | --- |
| 强 | 至少 16 个字符；或 8–15 字符且小写字母、大写字母、数字、其它符号中至少三类 | 任意位置的子串 |
| 弱 | 其余值 | 仅匹配 ASCII 单词边界 |

弱值首尾若为 ASCII 单词字符 `[A-Za-z0-9_]`，相邻外部字符不得是单词字符。比如 `pass,` 可匹配 `pass`，`bypass` 不匹配。贴邻单词字符的弱值可能漏替换；中文等非 ASCII 文本不具有这种词边界。强度不入库，也不证明凭据有效。

### 管理规则接口

以下路径均在 `/api/v1/reversible-redaction` 下，由 Server 和 Desktop 共用 `AdminService`：

| 接口 | 契约 |
| --- | --- |
| `GET /rules` | 返回内置目录和每条规则的 `enabled`，不混入自定义规则。 |
| `PUT /disabled-rules` | 用 `{ rule_ids: string[] }` 幂等替换禁用集合；未知 ID 为 `400 credential_rule_unknown`，设置不可用为 `503 credential_disabled_rules_unavailable`。 |
| `GET /custom-rules`、`POST /custom-rules` | 列出或创建自定义规则。 |
| `PUT /custom-rules/{id}`、`DELETE /custom-rules/{id}` | 修改或删除指定规则；无效字段为 `custom_credential_rule_invalid`。 |

「密钥匹配测试」和正式保护使用同一有效规则状态：内置禁用集加已启用自定义规则。检测和目录编译使用阻塞工作线程，不占用异步请求执行线程。

## 可逆脱敏映射与发布

可逆脱敏映射以 Principal 为唯一权限范围：同 API Key 的并发请求与重启可复用有效记录，其他 Key 的字典不参与检测、替换或还原。**Redaction Marker** 为 `<!--sr:<28 位 ASCII 小写字母>-->`，使用独立随机 ID，无秘密或授权含义；它不是 History Marker。

1. 在已完成 Request Hook 与历史恢复的 canonical 请求中检测可读文本，按秘密值去重。先可靠登记新映射，再对本次出站视图做最长优先、单次精确替换，不能替换成未持久化标记。
2. 创建时保存规则归因 `rule_ids`。已知字面值只有在映射未过期且至少有一条归因规则仍启用时参与替换或管理遮罩；无法归因的保留映射可继续保护但不产生归因复用。真实启用规则重新检出时仍可替换，不篡改创建归因。
3. 保留还原前的 Provider 视图用于续接证明及依赖登记；恢复回答及客户端、平台工具参数的原值（[ADR-0044](../adr/0044-restore-secrets-in-tool-call-arguments.md)）。流式按字段和调用仅保留未决标记或 JSON 转义，不等待整轮完成。未知、跨 Key 或已回收标记原样保留，不泄漏归属差异。
4. 还原尾部已交出 → 发布当前共享引用 → 交出唯一 Model Turn `Completed`（[ADR-0046](../adr/0046-complete-model-turn-after-redaction-publication.md)）。首次发布写 `published_at` 并清除一小时 `pending_until`，不续期。取消可抢占等待，不回滚已提交的发布；完整客户端交付与 Generation 提交另有边界。

Generation Chain 保存客户端历史并在节点事务登记依赖，不把替换后的模型视图回写成客户端历史。开关或归因变更造成上游前缀不等价时不复用旧续接锚点。普通停用规则不删除已有映射、也不禁止旧标记还原；冻结的退役规则清理例外只移除失效归因，若映射已无现行归因则解除引用后删除，不为错误替换保留标记的还原能力。见 [ADR-0090](../adr/0090-gate-credential-protection-by-rule-and-strength-and-expire-mappings.md)。

## 检出时钟与回收

`last_detected_at` 只由真实规则检出并登记推进。这包括首次发现，但不包括已知裸值替换、占位符回放、还原、发布、保留引用、遮罩或管理端读取。更新按一小时 G 节流；N 为[历史保留天数](data-layout-performance.md#共享正文存储与引用回收)。

| 映射状态 | 行为 |
| --- | --- |
| `last_detected_at <= now − N − G` | 立即停止复用替换与管理端遮罩，不等后台清理 |
| 已过期且仍被 Turn 节点引用 | 保留旧标记的还原能力，不延长出站资格 |
| 已过期、已发布且无引用 | 统一维护扫描后回收；被再次真实检出且尚未回收可复用原标记，已回收则新建 |
| 尚未发布且无引用 | 按一小时 `pending_until` 回收 |

缓存快照携带检出时间，固定 60 秒刷新上限，空结果也可缓存；创建、实际推进检出时间和回收使旧代次失效，G 内未更新时间的命中不失效。缓存错误读权威存储；SQL 错误不使用失效快照或假空结果。当前 N 和规则在使用时裁决，不随快照冻结。缓存可含明文，存储要求见[统一派生缓存](data-layout-performance.md#统一派生缓存与-server-配置)。

## 命中记录与密钥匹配测试

**命中记录**一行对应一次实际新增映射，不对应每个请求或每次匹配。以 `credential_mappings_created` 的 `(sequence, 下标)` 定位，提交后、可能失败的替换前投递；失败或取消不撤销发现。已启动的登记与发现投递由同一任务收口，但取消后不会因此派发模型或发布引用。记录依赖仍保留的 Observation，不需要 Debug；缺失记录不证明没有检出。

**映射复用命中**记录两类事实：出站按已有映射替换本次新提交的文本（最后一个 Assistant 之后；没有 Assistant 则全部），以及完整模型响应中的工具参数标记还原。客户端重放历史不重复记录，半途断开的不完整工具响应不算。复用可来自检测器无法单独识别的已知字面值；归因来自映射保存的规则，而非重新推断。

- 两类事件最多保留 50 个映射，每个映射最多 50 个去重位置；溢出用计数表示。位置含规则、来源、工具 `tool_id` 或 `latest_input`，不存请求下标、秘密、消息片段、指纹和文本偏移。内部 Agent 不伪造客户端交互或最新用户输入。
- `marker` 关联创建和复用，命中列表不返回其值，仅以 `linkable` 表示能否关联。复用按同 Principal、同标记及所属 Interaction 中的位置去重，保存最近一次的时间与 Run；`total` 是位置数，不是使用次数。
- `GET /api/v1/reversible-redaction/discoveries` 只读已持久化的创建记录，时间范围为 `[start_at, end_at)`；`…/discoveries/{id}/reuses` 通过索引以稳定游标分页，不扫描事件。复用 `limit` 默认 20、最大 100；缺关联时返回空页。归属或保留规则见[交互观察](interaction-observation.md#归属与交互链路)。
- `GET …/discoveries/{id}/hits/{index}` 只找回 Observation 已保留的条目，按相同检测器复检；有映射明文时按秘密标记 `hit`，否则按规则。保留的工具参数／结果及最新用户输入可定位，不补造历史消息和 system 正文。默认在服务端把所有匹配区间合并遮罩为 `••••••`，按返回文本重算 UTF-16 偏移及行列，不返回原始长度。`?reveal=true` 须经 `AdminSession::may_reveal_credentials()`；未获允许为 403 `credential_reveal_forbidden`，明文不进日志。

### 保留正文与定位

| 命中来源 | 可定位的文本 |
| --- | --- |
| `user_message` / `latest_input=true` | 当前 Run 保留的用户正文；工具续接可沿同 Interaction 找此前最近一条。 |
| `tool_arguments` | 平台工具开始或客户端工具交接时记录的参数。 |
| `tool_result` | Observation 保留的客户端或平台工具结果。 |
| 历史用户、system、`other_text` 或缺少正文证据 | 只定位到 Run；区分未保留与已丢失，不从 Generation Chain 补写。 |

工具 ID 先在所属 Interaction 定位，无结果才在同 Principal 且不晚于命中事件的范围查最近条目。定位仅查看 retained item，不以当前业务数据反推历史。定位不了匹配时不改写创建时的来源。
未明确关联标记（`linkable=false`）时复用 `total=0` 且无后续游标。已确认的相同位置只续写索引中的最后复用事实，不增加创建行；关联事实随 Observation 清理，而不是作为映射的保留锚点。

### 密钥匹配测试

「密钥匹配测试」`POST /api/v1/reversible-redaction/test` 只扫描临时文本，使用同一规则实现并返回规则与范围（UTF-16 半开偏移、从 1 开始的 Unicode 字符行列）。不查询已保存的秘密字典、不创建映射与 Observation、不记录输入、不联网；检测失败不伪装成无匹配。命中详情对保留条目的定位复检不因原规则被禁用而丢失判定能力。

分页和明文裁决见 [ADR-0087](../adr/0087-list-credential-hits-per-mapping-with-retained-item-context.md) 与 [ADR-0088](../adr/0088-mask-credentials-at-admin-boundary-and-page-reuses.md)。

## 诊断与回看边界

### 普通 Observation 与 Wire Debug

普通 Observation 在发布与管理端正文读取边界按当前 Principal 有效映射匹配，用 `***` 遮罩已知秘密，保留 Redaction Marker；不是改写历史正文，也不执行额外检测。读取失败不返回未遮罩正文。映射过期、被回收或所有归因规则停用后，旧正文不再保证被遮罩。
摘要只含引用与长度时不加载明文；没有映射时跳过正文扫描，有映射时只按相应 Interaction 所属 Principal 查找。拒绝记录不保存客户端正文，也不为其推断主体。

恢复前先把有效映射注册到当前 Run 的普通诊断保护集合，避免还原后的响应与工具参数未经遮罩进入诊断队列。增量可见文本只保留尚未判定的前缀，不缓冲整轮；该集合不跨 Run 或 Principal 共享。

[Wire Debug Capture](interaction-observation.md#wire-debug-与脱敏边界) 是原始应用协议取证，只保证永久过滤 HTTP `Authorization` 值；其它头、正文、URL、工具数据和媒体可能含明文。Debug Bundle 不使用普通管理正文遮罩，不得视作无秘密导出。

[History Recall](history-recall.md) 是同 Principal 的模型可见历史投影，不读 Wire Debug；其凭据处理仍受自身入口及模型发送保护约束。无论读取、回放还是遮罩，都不延长映射保留期。
