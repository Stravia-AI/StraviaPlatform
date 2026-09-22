---
status: accepted
---

# 统一 Stravia 引用外壳但不引入 Session

## 引用外壳与边界

Stravia 采用 `stravia://artifacts/<artifact-id>` 表达 Artifact Reference，采用 `stravia://turns/<turn-id>` 表达 Turn Reference。两者都是稳定、不透明的引用外壳；选择 `turns` 而不是 `sessions`，是为了保持 [ADR-0007](0007-own-agent-execution-behind-native-seams.md) 已确定的不可变节点语义：Turn Reference 精确选择一个续接节点，无隐式 latest、可变 head 或独立 Session 身份，从任意可访问旧节点续接仍会自然产生互不合并的分支。

引用格式不改变 Principal 归属和授权边界。Artifact Reference 与 Turn Reference 本身都不授予访问权限，解析时仍须按当前 Principal 校验；Artifact 的传输继续遵守 [ADR-0048](0048-separate-artifact-references-from-transfer-grants.md)，实际下载仍使用单独签发的真实 HTTP(S) Artifact Download Grant，不以 `stravia://` 引用替换下载 URL。

本决策只确认上述引用外壳及其语义，不表示已经实现。首期由平台内置 Agent Definition 的统一调用入口、Web Search、Media Understanding，以及 StraviaRead query 中的平台续接输入使用 `stravia://turns/<turn-id>`；各入口仍保留既有 Principal、Turn kind、Agent Definition 与其他能力约束，统一外壳不使不同 Turn 种类互换。Open Responses 对外 `id` / `previous_response_id` 保持现有协议身份，内部持久化的 `TurnNodeId` 也不改变，且不引入 Session。

## 直接切换与续接输入

引用格式采用直接切换：平台 Artifact Reference 与 Turn Reference 的输入输出只接受 `stravia://artifacts/<artifact-id>` 和 `stravia://turns/<turn-id>`。旧 `sa:` Artifact Reference 与平台裸 Turn ID 不提供兼容解析或 alias，旧 `https://stravia/artifact` wrapper 继续拒绝；已有历史不批量重写或删除。旧客户端必须更新，原样回传含旧引用的历史可能因引用不可解析而失败；内部 Artifact ID、`TurnNodeId` 或历史载荷保持不变，不表示旧引用外壳仍可使用。

平台续接输入的 JSON 字段与 StraviaRead query 参数统一命名为 `previous_path`，值必须精确为单个 `stravia://turns/<turn-id>`；平台内置 Agent Definition 的统一调用入口、Web Search、Media Understanding 与 StraviaRead query 均只接受 `previous_path` 字段或参数，不为旧 `previous_turn_id` 提供 alias。父节点值不得使用 Artifact URI、Search Source URI，或带 query、fragment、额外路径段的其他 URI。除该字段改名与引用值替换外，Artifact Reference 既有 `question`、`raw`、`download`、`lines`、`cursor` query 语义及组合限制保持不变；嵌套的 Turn Reference 沿用现有 query 参数编码规则，不为 Turn Reference 新增 query 或 fragment 语法。平台内置 Agent Definition 的统一调用入口、Web Search、Media Understanding 对外结果自身的 Turn 引用字段统一使用 `path`，值为完整的 `stravia://turns/<turn-id>`，不为旧 `turn_id` 输出保留 alias；内部 `TurnNodeId` 与 Open Responses 对外 `id` / `previous_response_id` 保持不变。

## Search Source 与资源自身引用

Search Source 的引用统一为 `stravia://turns/<turn-id>/sources/<ordinal>`，覆盖 Search Report 的来源标识与模型 prompt 中的 source identity。Source 是某个 Search Turn 已验证报告的从属来源条目，不是独立 Session 或存储对象，不新增读取 API、不授予访问权限，也不能作为 `previous_path`；父 Turn 引用仍精确使用 `stravia://turns/<turn-id>`。

平台自有稳定资源对象的自身引用字段（包括作为引用的 `artifact_id`、`artifact_reference`、文件 `reference` 与 `sources[].id`）不另设 `artifact_path`、`source_path` 或自身资源引用的 `turn_path`，统一使用 `path`，由 `stravia://` URI 类型区分：`artifacts[]` 元素使用 `path`，`sources[]` 元素使用 `path`，平台结果自身的 Turn 引用使用 `path`，generate 等 Artifact 结果自身也使用 `path`。这些 `path` 值分别为 Artifact、Search Source 或 Turn URI，覆盖平台 Agent 输入/输出、媒体报告与内部 prompt、generate 结果、StraviaRead 下载结果及上传完成 HTTP 辅助结果。同一文件不再额外重复输出裸 Artifact ID，旧自身资源引用字段不保留 alias；内部存储 ID、真实 `source_url` / `download_url`、协议关联 ID、opaque `cursor` 与上传 `upload_id` 等不变。自身资源引用字段的统一不改变 `previous_path`、`read_path` 或 `next_path` 等关系/操作角色字段；`reference_images` 数组名及无关对象结构不因本决策推断改变。

## 报告正文证据引用

已确认，报告正文中的证据引用使用完整 URI：Artifact 引用写为 `[stravia://artifacts/<artifact-id>]`，Search Source 引用写为 `[stravia://turns/<turn-id>/sources/<ordinal>]`。URI 文本本身不是凭据，也不构成证据；现有来源、归属与证据校验继续生效，旧 `[sa:...]` / `[sc:...]` 正文引用语法不提供 alias。该规则仅适用于报告正文证据引用，不限制普通 Markdown 图像或网页链接。媒体桥不保留特有的 `[sm:文件引用 媒体类型 序号]` 外壳；图片序号必须保留，原位呈现顺序不变。该序号是本次桥接请求内按 message/block 顺序遇到 Image 从 1 开始累计的顺序号，不是持久 ID 或文件页码，也不承诺只对应当前新上传图片。已确认替代呈现为在原图片位置给出可读编号与方括号包裹的完整 Artifact URI，编号沿用上述本次请求顺序语义；默认英文格式为 `Image 1: [stravia://artifacts/<artifact-id>]`，本地化遵循产品规范。不额外加入 MIME 展示，但不删除内部 MIME 元数据。

## 媒体工具结果与续接

媒体工具结果统一保留 `report` 嵌套结构，JSON 形态为 `{path: <Turn URI>, completion, artifacts: [{path: <Artifact URI>}], report: {answer, artifacts: [{path: <Artifact URI>}], limitations}}`。顶层 `artifacts` 是本次输入文件；`report.artifacts` 是正文实际证据集合，可引用历史 Artifact，不等于本次输入，二者不能合并。上述嵌套结构确定媒体结果中 Turn 与 Artifact 引用的字段归属；其余平台对象沿既有结构，本决策不推断额外重构。

媒体结果不再生成独立 `[st]` 提示，也不再投影非标准 Open Responses `stravia:media_result` 扩展；依赖该扩展的客户端接受 breaking 影响。标准 Open Responses `id` / `previous_response_id` 保持不变。媒体续接身份由受保护的成功 ToolResult 与既有可信历史归属恢复，不依靠文本提示或新增 fallback；相关结果提取点按此契约同步，但本 ADR 不表示已经实现。分页只分页 `report.answer`，首包保留续接与来源字段；用户删除历史或 marker 失效沿用原有边界。

平台仍保留 `stravia:agent_result` 客户端投影扩展；它不属于被删除的媒体扩展。其 payload 中的 `turn_id` 按已定结果自身规则改为 `path`，值为 Turn URI；自身 `id` 仍是协议 output-item 身份，即使当前字符串复用裸 Turn ID，也不作为 Turn Reference，不 URI 化该 `id`，且不删除该 Agent 扩展。
