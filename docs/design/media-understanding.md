# 多模态理解

Media Understanding 通过 [StraviaRead](agent-core.md#straviaread-统一读取契约) 读取并回答有关图片、Office 文档或音频的问题。
普通模型请求和 MCP 使用同一报告契约；鉴权与自动暴露规则见[平台能力门禁与透明注入](agent-core.md#平台能力门禁与透明注入)。
源收存和下载属于 [Artifact 契约](agent-core.md#artifact-与单请求上传)，下载不需要调用理解模型。

[stravia-media](../../backend/crates/stravia-media/) 拥有预处理、Derivative、报告校验、bridge 规划与内部 Definition。
Core 装配宿主与管理接口；双方共享 canonical 类型，不通过 JSON 往返转换。
设计决策见 [ADR-0009](../adr/0009-add-media-understanding-as-capability-tool.md)。

## 读取与追问

| 输入 | 默认处理 | 带 `body.question` |
| --- | --- | --- |
| 所属或公网静态 JPEG/PNG/WebP | 描述内容并提取文字 | 依据图片回答问题 |
| 受支持 Office 文档 | 提取 Markdown，可按行读取 | 结合文档文本与内嵌图片回答 |
| 受支持音频 | 转录并总结 | 依据音频回答问题 |

公网源先收存，问题不会发送给源站。图片和音频不接受文本选择器，Office 不接受 `raw`；
明确请求下载时不解析或调用模型，关闭理解能力或格式不支持也不以下载冒充理解。

追问使用上一份报告的 `stravia://turns/<id>` 作为读取路径，并附后续问题。`body.attachments` 可加入同 Principal 的 Artifact 或公网 HTTP(S) 附件，祖先已保留的 source 不重复附加。
续接固定父 Turn 的 Definition 和模态；同一个可读父节点可派生多个分支。
通用续接规则见 [Turn Chain](agent-core.md#turn-chain-与-turn-reference)。

## Media Report 与 Media Understanding Turn

结果包含 `path`、`completion`、本次收到并收存的输入 `artifacts`，以及 `report.answer`、`report.artifacts` 和 `report.limitations`。

- 顶层 `artifacts` 是本次输入集合，即使报告没有引用也保留。
  `report.artifacts` 是答案实际引用的证据集合，可来自当前或可信祖先 Turn；两者不可互换。
- 答案使用 `[stravia://artifacts/<id>]` 引用报告中的源证据。校验器限制文本、列表尺寸和数量，验证每处引用对应已列出的同 Principal 证据。
  伪造引用、其他 Principal 对象与内部 Derivative ID 不可作为证据。
- `partial` 必须说明预算、deadline 或覆盖范围限制。完整报告校验并落盘后才对长答案分页，交付使用[统一 Read Snapshot](agent-core.md#straviaread-统一读取契约)。

成功结果形成不可变 Media Understanding Turn。图片与文档共用内部 Definition `media-understanding`；
音频使用 `audio-understanding`。二者共享 AgentRunner、报告校验和 capability-owned 授权，没有通用 `agent_*` 调用入口。

## 普通推理中的图片路由

1. 逻辑 Model 有合格原生 vision Target 时，优先原生传输。
   实际发送前以适配协议承载 Artifact，不由自动 bridge 改写。
2. 无原生 Target 时，只有具备工具能力的 parent Target 且本次[透明注入](agent-core.md#平台能力门禁与透明注入)允许 Media Understanding，才可使用 bridge。
3. 两条路径都不可用时，发送上游前返回能力或输入错误；一次 Run 选定路径后，失败不自动切换语义。

bridge 移除发往 non-vision parent 的原始图片 bytes/URL，在原位置保留 `Image N: [stravia://artifacts/<id>]` 并注入由平台拥有的说明。
`N` 按完整历史中的图像块位置编号；相同 Artifact 的不同出现位置仍各自编号。
父模型用这些引用调用 StraviaRead。

恢复媒体结果要求关联的 Assistant `StraviaRead` ToolCall 与成功 ToolResult 均可信且匹配；
受保护的第三方结果、失配和已清理记录不会触发 bridge。原文中出现 URI 不授予访问权或证据资格。

## 媒体处理与 Derivative

### 静态图片

支持 `image/jpeg`、`image/png` 和静态 `image/webp`，以容器解码为准。
GIF、动画 WebP、HEIC/HEIF、SVG、PDF 和视频不属于此能力。

每个 source 第一次使用时生成 write-once JPEG Derivative：应用图像方向，白底合成 alpha，约束尺寸并移除元数据；
持久保存映射并复用。公开引用始终指向 source，不泄露 Derivative ID。

原生图片输入与理解预处理不同。[OpenAI](https://platform.openai.com/docs/guides/images-vision)、[Anthropic](https://platform.claude.com/docs/en/build-with-claude/vision) 与 [Google Gemini](https://ai.google.dev/gemini-api/docs/image-understanding) 的官方图像接口均接受 `image/webp`；
Stravia 的对应 codecs 按协议透传 MIME，不会因为原生交付而默认转 JPEG。自定义和 OpenAI-compatible Provider 的支持范围不由协议名保证。
Rust `image` crate 的 WebP encoder 只支持无损编码；Stravia 的归一化 Derivative 使用 JPEG，不要求目标支持 WebP，也不做 WebP 重编码。

### Office 文档

支持 DOCX、XLSX、PPTX、DOC、XLS、PPT。预处理校验声明 MIME、文件大小与 ZIP/Compound File Binary 容器魔数，不接受损坏、加密、格式失配、ODF 或 XLSB。

调用方在进入 `office_oxide` 前检查 ZIP 中央目录：最多 10,000 个条目，声明解压总量最多 512 MiB。
该 preflight 是调用方的解压边界，不能以解析器能打开文件代替；
解析安排在 blocking pool，不在异步请求线程直接执行。

每个文档源生成 write-once manifest Derivative，记录提取的 Markdown 及独立收存的内嵌图片 Artifact、次序和归一化结果。
Markdown 图片位置采用 `![alt](stravia://artifacts/<id>)`；
复用 manifest 时逐项复验 Principal 访问权限。

问答向模型交付受文档和 Turn 文本预算约束的文档条目，并按顺序附入去重后的 JPEG 图片。
文档源与内嵌图片可作为报告证据，manifest 本身不可引用。

### 音频

支持 WAV、MP3、FLAC、OGG、AAC/ADTS 与 AIFF；校验声明 MIME 与容器魔数，不解码或转码。
单源及每 Turn 合计均最多 20 MiB，实际 Target 能否表达该封装依协议决定。
一次调用不得混用音频与图片/文档。

## 安全与 Admin

媒体数据与其中的文字不可信，不能改变 system instructions、权限、证据集合或工具策略。
公网读入沿用 [Web Access 公网规则](web-access-providers.md#公网读取安全)；每次读取复验 Artifact 归属。
普通日志不记录媒体字节、下载链接、工具正文或 Derivative 映射。
运行时撤权会在下一次受校验的隐藏操作前终止，不提交新 Turn。

- `GET /api/v1/media-understanding` 返回 `image`、`audio` 两个视图；
  `PUT /api/v1/media-understanding/{kind}` 提交 `enabled`、`model_id`、`thinking_level`，只改指定模态并返回完整视图。
- 两类候选都要求已启用的 `inference` Route。过滤禁用及不合用途的 Target 后，必须至少剩一项，且每项都支持所选模态的输入。
- 必须选择逻辑 Model 的 `supported_thinking_levels` 中的等级：它是已启用 Target 非 Hidden Mapping 的并集，不要求每个 Target 支持相同档位。
  执行及切换 Target 时都用原选档位重新匹配对应映射。映射规则见 [Vendor Plugin](vendor-plugins.md#自定义协议与思考等级映射)。
- 单个媒体问题最多 64 KiB UTF-8，附件数量由各模态入口校验，不是 Admin 参数。
- 管理页 `/media-understanding` 分别保存图片/文档与音频的启停、模型和思考等级，显示 `disabled`、`unavailable` 或 `available`。
  Admin 不配置格式边界、大小、像素、JPEG profile 或虚构的 OCR/描述子能力。
- 隐藏 Model 使用 capability-owned 授权执行；不要求列入 Key 的普通 `model_ids`，也不因此自动取得直接调用该 Model 的权限。
