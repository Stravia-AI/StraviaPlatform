---
status: accepted
---

# 统一内容读取与显式下载

StraviaRead 对客户端、外层模型和内部 Agent 只公开一个必填字符串 `path`，不保留顶层 `url`、可选参数或旧 `query://` 别名。搜索使用 `search://`，资源使用公网 HTTP(S) URL 或所属 Artifact Reference。此修订取代此前裸 Artifact 默认只下载、仅 Artifact query 可以指定问题的规则，使相同内容不因取得方式不同而改变默认行为。

## 内容决定默认行为

JPEG、PNG、WebP 默认调用 Media Understanding，描述图片并提取可读文字；公网图片先按原有准入规则收存，所属图片直接使用原 Artifact。HTML 默认返回 Markdown，JSON/XML/文本直接读取；不支持的二进制返回文件与下载信息，并明确说明内容未读取。不会解包、执行文件或新增 PDF、视频等理解能力。

公网 HTTP(S) 资源以 `#stravia?question=...` 指定问题；所属 Artifact 使用 `sa:<id>?question=...`，不使用 fragment。HTML 带问题只返回 Markdown 及 `question_applied=false`，不额外调用模型。`raw=1` 严格解码原文，不进行正文抽取或格式转换；无效编码失败，不把有损文本称为原文。显式 `download=1` 与其他选项互斥，只下载、不调用模型；已请求理解但能力关闭或类型不支持时，不能以下载冒充成功。

该选择消除了外部图片与 Artifact 图片的行为差异。代价是仅想下载图片的调用者必须显式指定下载；默认图片读取可能产生模型用量。

## URL 选项隔离

公网 HTTP(S) URL 仅解析 `#stravia?` fragment 内的平台资源选项，不把源站 query 中同名字段当作指令。源 path/query 保留原始顺序和编码，问题不发给源站，不破坏签名 URL。Artifact 使用独立的 `sa:` 引用，query 只承载平台选项；剥离选项后按共享解析器校验 55 位小写内容身份，拒绝 fragment 和旧伪 HTTP URL。两种输入复用同一资源选项语义，不放宽 Principal 归属检查。

搜索选项是 `search://` query 中的可重复 `allowed_domains` 和单值 `previous_turn_id`。严格解码、去重和边界检查由共享读取契约统一拥有，所有输入生产者使用同一个格式化器，不再向工具调用注入额外顶层字段。

## 执行层级与来源约束

外层 `search://` 执行完整 Web Search，内部 Agent 的相同形式只做基础检索并拒绝研究续接 ID；模型不能选择执行层级。网页读取复用 Web Access，完整研究与内部检索不合并 owner 或执行身份。

新研究省略允许域名表示无限制；续接省略则继承父策略，非空列表整体替换。最终 Search Report 必须满足本次解析后的来源约束，内部工具自写更宽的列表不能放宽父研究的最终来源范围。来源约束不是网络访问授权，既有公网地址、凭据、DNS pinning 和重定向安全检查保留。

Stravia 管理的域名黑名单能力删除：输入、持久化策略恢复、Provider 参数生成和来源过滤都不再执行它。新调用或原生转工具声明含 `blocked_domains` 时明确拒绝；历史 payload 保持原样，只在读取时忽略旧字段，不重写来源或报告。普通第三方原生不透明直通不进行全局 JSON 字段剥离。

## 不可变文本续读

文本结果通过 Read Snapshot 固定首次表示，后续页不回源、不重复转换、不执行模型。行范围与游标针对这一表示，不能把 Markdown 行号套到原 HTML。每页正文有 UTF-8 字节和源行预算，超长单行也可通过生成的 `next_path` 完整读完；正文不插入行号、上下文或省略符。

快照只复用 ArtifactStore、Principal 归属、保留期和已有本地/S3 读取句柄，不新增 Session 或数据库事实源。快照 MIME 和游标均不是权限凭据：上传的同 MIME 封装也必须校验，游标最多选择同一已授权快照的其他内容。快照下载导出 UTF-8 正文而非私有封装，支持空文本。来源注记本身不构成公网证据。

分页只保证完整读完已经取得的文本；上游截断通过独立的 `source_truncated` 表达。搜索与媒体完整报告先验证、落盘，再裁剪工具交付副本的长 answer；首包保留来源等报告字段，后续页不创建新 Turn。代价是快照使用存储并受现有过期规则限制，过期后不能回源补读。

## 能力门控与透明注入

联网搜索与网页读取共用已有 Web Search 开关及透明注入选择项，不新增网页开关。媒体理解受 Media Understanding 开关控制；所属 Artifact 的内容读取与显式下载保留 Principal 归属检查，图片理解仍须具备本次媒体暴露范围。

API Key 的透明注入分别沿用联网和媒体选择。它是自动提供工具的偏好，不是显式调用或 MCP 的能力授权；本次工具暴露范围在执行层检查，不能只靠省略说明限制能力。平台关闭的能力不能通过显式声明重新开放，既有勾选状态不迁移或扩张。

## 直接切换与插件职责

新根 Local Search 使用 Definition Revision 4，并使用 `[sc:<turn_id>:<ordinal>]` 引用来源。旧不可变 Revision 的续接明确报 `incompatible_definition_revision`，不重写历史指令，也不静默重开根。标识编码与引用外壳按全新数据库契约干净切换；Generation、Artifact 和 Turn 历史不批量重写，也不自动删除旧数据。

插件继续贡献领域、描述与处理入口；统一工具负责解析、分流、公共文件处理与安全检查，搜索和媒体仍执行各自拥有的能力。重复领域、冲突身份或无处理描述明确失败，不依赖加载顺序。复用既有插件体系，不另建安装系统。

本决策继续部分取代 ADR-0009、ADR-0017 的旧工具身份约定，保留原有执行 owner。当前实现和完整 wire contract 见 [Web Search 设计](../design/web-search.md#统一资源读取与文本分页) 与 [Media Understanding 设计](../design/media-understanding.md)。
