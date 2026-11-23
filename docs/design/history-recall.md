# 历史回看

History Recall 让平台对话中的模型只读浏览、读取和检索当前 API Key 的既往对话。它使用 Interaction Observation，不读取 Generation Chain，不构造 Session，也不尝试从其他来源补全缺失观测。
取舍见 [ADR-0096](../adr/0096-recall-interaction-chains-through-read-paths.md)。

## 来源与范围

- 链路和成员使用 [Interaction Observation 的归属与可见规则](interaction-observation.md#归属与交互链路)。
  链路路径使用 Observation 的 `root_id`，轮次使用 Interaction id；和请求记录管理面按同一根分组。
- 最终失败且从未交付客户端可见输出的 Interaction 不列出；MCP `tools/call` 形成的观测记录不是对话。
  用该 Key 发送的[控制台对话](console-chat.md)与普通请求采用相同归属。
- 内容包括用户输入、可见回答、工具参数与结果、观测缺口和 Run 失败。不会返回 system 指令、完整请求或思考内容。
  观测缺失时转录如实留缺，不从 Generation Chain 补写。
- 保留窗口为 `log_retention_days`。到期清理或「清除历史记录」完成后，相关内容立即不可读及不可检索。

## 路径与目录

| 路径 | 内容 |
| --- | --- |
| `stravia://history/` | 当前 Principal 的链路，最近活动在前。 |
| `stravia://history/<root-id>/` | 链路内可见 Interaction，按开始时间正序。 |
| `stravia://history/<root-id>/<interaction-id>` | 一轮 Interaction Transcript。 |
| `stravia://history/<root-id>/<interaction-id>/tools/<n>` | 第 `n` 次工具调用的完整参数和结果。 |

目录以 `/` 结尾，文件不得以 `/` 结尾；不提供别名。新 ID 为 28 位小写字母，既有路径段接受 `[A-Za-z0-9_-]{1,64}`。
Interaction 必须属于指定链路；工具序号从 1 起按调用开始事件分配，不取客户端 call ID。

选择器与输出字段见 [StraviaRead 统一契约](agent-core.md#straviaread-统一读取契约)。
文件可用行范围或游标，目录和检索结果仅用游标；历史路径都不支持 `raw` 或 `download`。
非空 `body.question` 在根目录、链路目录或交互转录上表示检索，与行范围或游标互斥；
工具文件不接受查询。`wait`、`attachments`、`allowed_domains` 只接受视同省略的零值。

不存在、已清理或不属于当前 Principal 的路径统一返回：

```text
History entry unavailable: it does not exist, has expired, or was cleared.
```

目录输出 Markdown，统一使用 UTC 秒精度。预览在凭据替换后截取根用户输入的首行（最多 80 字符）；
无用户输入则取可见回答，再无则写 `(no text)`。

- 根目录每页 50 个链路，以最近活动成员为代表（同一时间取较小 ID），使用时间和 ID 键集分页；
  状态取代表成员，模型取根 Interaction 首个 Run。
- 链路目录每页 100 轮，按开始时间和 ID 正序。只有父轮不是上一行时才标记 `branch from <parent-id>`；
  父轮未列出时注明 `(not listed)`，不按链深缩进。
- 可变的运行状态等信息不进入转录头，以免刷新投影时移动旧行。

## 交互转录与工具文件

转录以 Interaction、链路、父轮和开始信息开头，按已落盘的 Observation 事件顺序追加。
切换 Run 时插入含模型和开始时间的分界；用户输入、完整可见回答、工具调用与结果、观测缺口和失败各在实际出现的位置渲染。
结果不回填到调用处。媒体仅写 `[image]`、`[audio]` 等占位符。

典型转录（省略文件正文）：

```text
# Interaction <interaction-id>
- chain: stravia://history/<root-id>/
- parent: none
- started: 2026-11-09T06:20:11Z · protocol: responses

--- run · <model> · 2026-11-09T06:20:11Z
## user
核对配置字段
### tool call 1 · <tool-name> (client)
(full: stravia://history/<root-id>/<interaction-id>/tools/1)
```

工具调用与结果在转录中最多内联 20 行、2 KiB，随后链接至完整工具文件；截断显式标记。
找不到对应调用的结果注明 `unmatched call`，没有工具文件。
工具文件包含名称、client/platform、开始时间、完整参数和按事件顺序追加的结果段；结果未到时只显示调用。

### 行号稳定性

渲染只使用不可变事件、Run 准入时的事实和此前已确定的计数。进行中的文件只在末尾增长；
未完成的内容块不出现。父 Interaction 被清理时父行改为 `none`，仍占同一行。
已存在的正文行号保持稳定，跨行凭据替换保留原换行。

文件用实时追加内容分页，不需要静态快照。游标记录事件序号、行内位置和渲染上下文；不授予权限，每次都复验路径与事件归属。
直接跟随 `next_path` 顺序浏览。任意靠后行范围或 `:-K` 需从头计算行位置，并非 O(1)；
已知工具序号时直接读取工具文件。

## 全文检索

例如在根目录调用 `StraviaRead({"path":"stravia://history/","body":{"question":"数据库迁移"}})`，也可用链路或转录路径缩小范围。
搜索结果中的命中路径可直接读取对应行。

### 查询与分词

- 查询最多 256 字符；按空白切分为最多 8 个词项，双引号组合作为同一个词项。
  每个词项分词后最多 16 个相邻词元。
- 词项之间在 Interaction 级做 AND，可以分别命中不同消息。忽略没有词元的词项；完全没有有效词元报错，不退回列目录。
  不支持布尔语法、通配符或字段搜索。
- 索引和查询共用 Rust 分词：NFKC、Unicode 小写化；非 CJK 字母数字连续成词，CJK 连续片段取相邻双字，孤立片段保留单字；丢弃超过 64 字节的词元。
- 标点与 `_`、`-`、`.`、`/` 都分隔词，故 `run_admitted` 与 `"run admitted"` 等价。
  不切驼峰、不做词干或停用词过滤；`41` 不匹配 `0041`。
- 单个 CJK 字按词元前缀匹配，可能漏掉只在片段末尾出现的字；单个 ASCII 字符仍精确匹配。

### 索引与实时性

索引覆盖可见链路成员的用户输入、可见回答，以及 client/platform 工具名称与完整参数。
排除思考、system、工具结果、媒体占位文本与回看调用本身（参数中出现 `stravia://history/`）。
参数命中指向完整工具文件；工具名命中可指向转录。

SQLite FTS5 和 PostgreSQL tsvector 使用同源词元，不另存一份明文正文。
事件按重叠词元段索引，最长允许短语跨分段时仍可匹配。

索引异步读取已提交事件。正在运行的 Interaction 增量更新，终结时原子重建其索引以补齐可能乱序提交的事件；
终结后又追加的内容同样重新对账；实例启动、观测通知需重置及周期清理后的对账也会补齐其他实例写入。
索引失败不影响推理或 Observation，留待后续对账。删除历史时同步移除索引，不等待后台补齐。

转录能先读到尚未索引的内容。需要核对最新一轮时直接查目录或读转录；搜索未命中不证明内容不存在。

### 排序与局限

1. 每个词项最多保留最近命中的 2,000 个 Interaction，并记录最高角色权重：用户 3、可见回答 2、工具调用 1。
   触及上限时指出被截断的原始词项。
2. 求各词项候选交集，排除发起搜索的当前 Interaction。后续轮翻页仍排除首页发起轮。当前轮也不计入尚未索引数量。
3. 按各词项最高权重总和降序、最近命中时间降序、Interaction id 升序排列，每页 10 轮；
   每轮最多 3 个不同位置，优先覆盖更多词项，再选较新段。
4. 从转录定位命中位置，返回可读路径与凭据替换后的摘要；摘要最多 160 字符，命中用 `«»` 标示。
   索引与转录已变化、无法定位时明确要求重新检索。

`limitations` 报告回看时限、索引滞后、截断词项、搜索后被清理的匹配、观测缺口、单字 CJK 及超长行的分页限制。
不报告易随翻页变化的命中总数。

常见词先匹配和排序再应用候选上限，成本仍随命中量增长；优先使用较稀有词或缩小到链路、转录。
超长行的命中可能不在该行第一页内，沿 `next_path` 继续读。

## 暴露与凭据

History Recall 只由 API Key 的[透明注入](agent-core.md#平台能力门禁与透明注入)提供。
`history_recall` 默认未选，没有单独平台开关或独立设置页。显式声明的 StraviaRead、MCP 与内部 Agent 永不暴露历史范围；
未获该范围的调用明确拒绝。Key 设置变更从下一请求起生效。

**共用 Key 的人可互相看到历史对话。** 回看内容含既往网页、工具输出和可能影响模型的文字，始终按不可信输入处理；
取消该项不会关闭其他注入能力。

Observation 保存的原文不等于可以原样发给模型，参见 [Wire Debug 与脱敏边界](interaction-observation.md#wire-debug-与脱敏边界)。
所有历史输出在离开 Core 前应用当前 Principal 已知的可逆映射；替换在预览截取之前执行并保持行数。
不依赖当前脱敏开关，也不重新扫描从未检测过的秘密，不使用管理面的 `***` 遮盖。
标记的授权还原见[凭据保护](credential-protection.md)。
