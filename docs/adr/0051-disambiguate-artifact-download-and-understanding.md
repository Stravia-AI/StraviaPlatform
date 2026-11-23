---
status: accepted
---

# 统一内容读取与显式下载

StraviaRead 统一资源读取：公网 URL 与所属 Artifact 按内容选择默认行为，而不按取得渠道区分。图片默认理解，HTML 默认抽取 Markdown，不支持的二进制只报告未读取和可用的文件信息；明确指定下载时不调用模型或转换正文。代价是只想下载而不理解图片的调用方必须显式选择下载。支持的媒体类型与能力边界由[多模态理解设计](../design/media-understanding.md)拥有。

[ADR-0091](0091-read-paths-with-colon-selectors-and-plain-fields.md) 取代 URL query／fragment 选项：现在只用 `path` 的末尾 `:` 选择器（如 `:download`）与可选 `body`，搜索使用 `stravia://search`，续接使用 Turn Reference。公网源 URL 的 query 属于源站，平台问题不转发。完整语法、分流与能力授权见 [Agent Core](../design/agent-core.md)。

长文本以不可变 Read Snapshot 续读：后续页不回源、不重复转换或执行模型，且游标不授予权限；这是为表示稳定性接受的存储与保留期开销。搜索与媒体完整报告先验证和持久化，再裁剪工具交付副本；上游未取得的内容不伪装成可续读。外层搜索和内部基础检索仍是不同执行身份，域名策略限制结果来源而不授予网络权限，见 [Web Search 设计](../design/web-search.md)。
