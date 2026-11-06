---
status: accepted
---

# 用 `:` 选择器与纯文本字段读取路径

StraviaRead 的输入不再是一个必须转义的 URL 字符串，而是 `path` 加少量可空字段：`path` 只承载地址和写在末尾的 `:` 选择器，自由文本放在 `question`，`attachments`、`allowed_domains`、`wait` 按暴露范围出现。本决定局部取代 [ADR-0051](0051-disambiguate-artifact-download-and-understanding.md) 的公开输入约定（只公开 `path`、`search://`、`#stravia?` fragment、Artifact query 选项与搜索 query 参数）、[ADR-0064](0064-use-stravia-uri-references-without-sessions.md) 中 StraviaRead query 参数 `previous_path` 的约定，以及 [ADR-0082](0082-expose-queryable-tool-executions-after-bounded-wait.md) 中 `?wait=` 查询参数的约定。

## 背景

旧契约把搜索文本、允许域名、问题、行范围、下载与续接都编码进一个字符串：`search://<文本>?allowed_domains=…`、`<URL>#stravia?...`、`stravia://artifacts/<id>?question=...`。调用方必须在调用前做 percent-encoding：表单解码把 `+` 当作空格，非法的 `%` 会导致参数错误，续接还需把 Turn Reference 编码成 `previous_path`，形成嵌套编码。自由文本改用 JSON 字段，结构化选项改用受限字符集的 `:` 选择器，不再要求模型编码这些参数。选择器是 Stravia 的读取语法，不是通用 URL 标准；源 URL 以选择器形态结尾时仍需转义字面冒号。

## path 与选择器

`path` 只承载地址和无需编码的 `:` 选择器，与 omp `read` 工具的形态一致。选择器从 path 末尾按 `:` 切出 token，自右向左逐个剥离，类型只有四种：

- `raw`；
- `download`；
- `cursor=<1+ 个 [A-Za-z0-9_-]>`（平台生成的 base64url 无填充游标）；
- 行范围：逗号分隔的段，每段为 `N`（到 EOF）、`N-M`、`N+K` 或 `-K`，均为 1-based 正整数，最多 `MAX_LINE_SELECTIONS` 段，运算做溢出检查。

剥离位置的底线是：Artifact 为 55 位 id 结束处，HTTP(S) 为 authority 结束处，即 `scheme://` 之后第一个 `/`、`?` 或 `#`。没有这种字符时不剥离，因此 `https://host:8080` 里的 `:8080` 仍是端口。token 严格符合语法才剥离，否则停止，剩余部分按字面作为地址。每种 token 最多出现一次；多个行范围 token 报错并提示用逗号合并；`:raw:1-20` 与 `:1-20:raw` 两种顺序都合法。Artifact 剥离后若仍有多余字符，报“无效 Artifact Reference”并附选择器提示。

源 URL 本身以选择器形态结尾时，把属于 URL 的那个 `:` 写成 `%3A`，`%3A` 原样发往源站，不做任何解码。裸 `host:port` 后需要跟一个 `/` 才能追加选择器。

组合约束沿用旧规则：`raw` 与 `question` 互斥；`download` 不能与任何其他选择器或 `question` 同用；`cursor` 只对 Artifact 有效，不能与其他选择器或 `question` 同用；行范围可与 `question` 共存，具体内容类型限制由 core 决定。续页 `next_path` 由平台生成为 `{read_path}:cursor={token}`。

## 自由文本走字段

唯一解析入口是 `parse_read_input(&ReadInput)`，输入结构固定为：

```rust
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReadInput {
    pub path: String,
    #[serde(default)] pub question: Option<String>,
    #[serde(default)] pub attachments: Option<Vec<String>>,
    #[serde(default)] pub allowed_domains: Option<Vec<String>>,
    #[serde(default)] pub wait: Option<u64>,
}
```

JSON 里的 `null` 与缺省含义相同。`question` 承载搜索文本、网页/媒体问题与续接追问；搜索轮与续接的 `question` 必须非空白，搜索轮不超过 `MAX_SEARCH_TEXT_BYTES`；`attachments`、`allowed_domains`、`wait` 可空。`allowed_domains` 沿用现有规则：原始条目 ≤ 20、每项非空白、经 `normalize_domains` 规范化去重、规范化后非空；显式空数组报错并说明“省略或 null 表示继承或不限制，非空列表表示替换”，不得静默变成不限制。`attachments` 的 `[]` 等同于缺省；每项必须是精确的 Artifact Reference（`stravia://artifacts/<55 lowercase>`）或公网 http(s) URL，按字面值处理，不剥离选择器，也不接受 `#stravia?`；重复项报错，数量上限由媒体执行侧负责。`wait` 只对 tool-executions path 有效，缺省为 0。

解析器不知道 Turn 的类型：`attachments` 只适用于媒体轮，`allowed_domains` 只适用于搜索轮，由 core 在查出 Turn 类型后校验。

## 按范围裁剪

字段按暴露范围裁剪，以控制 token 成本。`ReadSchemaScope { networking, media, internal }` 决定字段集：`path` 总是有；`wait` 仅在 `!internal` 时出现；`question` 在 `networking || media` 时出现；`attachments` 在 `media` 时出现；`allowed_domains` 在 `networking` 时出现。`FULL` 表示 networking 与 media 均为 true、internal 为 false。内部 Agent 使用 `{networking: true, media: true, internal: true}`，即 path、question、attachments、allowed_domains。

为减少 schema 长度，字段 `description` 每个不超过一句。`pagination_schema()` 不变。

## 续接

续接直接把上一轮结果里的 `stravia://turns/<id>` 作为 `path`，`question` 是追问，不再有独立的 `previous_path` 参数。`attachments` 保住“续接时换新图”的能力：媒体续接在 `attachments` 中列出新增的 Artifact path 或公网 URL；搜索续接省略 `allowed_domains` 时继承父策略。Turn 类型由平台用 `TurnChainStore::node_kind` 查出：WebSearch 轮分派搜索，Agent 轮分派媒体，Response 轮或不可用 Turn 报“Turn cannot be continued with StraviaRead”或“Turn unavailable”。内部 Agent 仍拒绝研究续接。

## 取代旧语法

`stravia://search` 取代 `search://`；Artifact 与 HTTP(S) 资源的 `:` 选择器取代 `?`／`#` 选项；续接 path 取代 `previous_path` 参数；`wait` 字段取代 `?wait=`。旧语法不保留别名，只给出可执行的报错：

- `search://…` 或 `query://…` → “use path `stravia://search` and put the search text in question”；
- Artifact 带 `?` 或 `#` → “options are `:` selectors such as :raw, :download or :1-50; put questions in the question field”；
- HTTP fragment 以 `stravia?` 开头 → 同上提示；
- tool-executions 带 `?wait=` → “use the wait field”；
- Turn 带后缀 → 无效 Turn Reference。

其他 `stravia://`、`sa:`、`https://stravia/artifact/…` 继续拒绝；HTTP 普通 fragment 保留源语义。报错不回显完整 URL、question 或字段值。

## 严格 schema 的可空写法

模型侧声明采用“全部出现的字段都必填 + 可空”的严格形式：`model_input_schema(scope)` 把所有出现的字段列入 `required`，可选字段类型写成 `[T, "null"]`（数组字段写 `"type": ["array", "null"]` 并带 `items`），`additionalProperties: false`，`strict_schema()` 保持 true。MCP 使用可选形式：`input_schema(scope)` 只有 `path` 必填，`additionalProperties: false`。

原因是暴露工具时还不知道目标协议，跨协议改写 strict 会改变客户端看到的工具声明；既有 web-search 平台工具已采用“全部必填 + 可空”的写法（`stravia-web-search/src/platform.rs`），各编码器都能处理。模型仍可传 `null` 表示不设置，语义与省略一致。

## Considered options

- **全部 JSON 字段**：搜索文本、问题、行范围、下载、续接全部做成独立字段。字段多、schema 与每次调用的 token 成本高，也偏离 URL 规范方向；路径本身就是地址，选择器比一堆并列字段更贴近用户看到的 `next_path`。
- **保留 query 与 fragment 选项**：即旧契约。表单解码中 `+` 的问题、非法 `%` 直接报错、`previous_path` 的嵌套编码，都会让 LLM 在真实调用中反复失败，且这些错误对模型不可见或难以自纠。
- **保留 `previous_path` 字段**：同一续接会同时存在“Turn 作为 path”和“Turn 作为 `previous_path`”两种写法，解析、schema 与文档都要维护两条路径，调用方也会混用。

## Consequences

- 旧语法被干净切换，没有别名；旧搜索路径、Artifact query 选项、专用 HTTP fragment 和执行查询参数会得到指明新写法的错误。外部 HTTP(S) URL 的普通 query 属于源站，不能将其中的 `question` 或 `download` 当作旧平台参数拒绝。
- 外部 URL 字面以 `:数字` 结尾时会被当作行范围。用户必须把属于 URL 的 `:` 写成 `%3A`；这是唯一的编码要求。
- 内部 Agent 仍拒绝研究续接；它的字段集不含 `wait`，但包含 `attachments` 与 `allowed_domains`。
- 续接只有一种写法；`attachments` 只影响媒体轮，`allowed_domains` 只影响搜索轮，错用会在 core 查明 Turn 类型后报错，而不是在解析器里猜测。
- 输入不再有 percent-decode 路径，安全校验不再需要先解码再判断选项归属。

## 与既有决策的关系

- [ADR-0051](0051-disambiguate-artifact-download-and-understanding.md)：只公开 `path`、`search://`、`#stravia?` fragment、Artifact query 选项与搜索 query 参数这几项输入约定被本决定取代；内容决定默认行为、来源约束、不可变文本续读与能力门控不变。
- [ADR-0064](0064-use-stravia-uri-references-without-sessions.md)：仅 StraviaRead query 参数 `previous_path` 改为续接 path；平台工具 JSON 字段 `previous_path` 仍有效，引用外壳与 Principal 边界不变。
- [ADR-0082](0082-expose-queryable-tool-executions-after-bounded-wait.md)：仅 StraviaRead 的 `?wait=` 改为 `wait` 字段；同步等待窗口、执行引用、取消与查询语义不变。
