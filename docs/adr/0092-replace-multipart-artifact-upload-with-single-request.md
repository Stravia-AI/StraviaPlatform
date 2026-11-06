---
status: accepted
---

# 用单请求上传取代三步分片上传

Artifact 的外部上传只保留一个端点：`POST /v1/artifacts`。请求体就是文件字节，`Content-Type` 就是 MIME，成功返回 201 与 `{"path","mime_type","size"}`。原先的「创建上传 → 逐片 PUT → 完成」三步接口以及 `x-upload-token` 一并删除，不留别名。本决定局部取代 [ADR-0049](0049-snapshot-explicit-media-inputs-as-artifacts.md) 中“一个文件的多个上传分片只计一个任务”的表述（额度规则本身不变），以及 [ADR-0048](0048-separate-artifact-references-from-transfer-grants.md) 中“上传会话仍各自独立”的措辞。

## 背景

旧流程要求客户端先创建上传拿到 `upload_id` 与 `upload_token`，再按片 PUT，最后提交 part 清单。为让 LLM 自行完成上传，Upload Grant 的说明包含 jq 解析 JSON、保存 token、拼装 part 清单的多步 shell。它增加了 jq 依赖与跨请求状态，而本次需求不要求保留断点续传，单文件上限仍为 100 MiB，因此选择单请求流式上传。

## 决定

- **单端点、原始字节。** `POST /v1/artifacts`，请求体即文件内容，不使用 multipart 或 JSON 包装。`Content-Type` 是声明 MIME，沿用 ingest 现有的校验与归一化，不新增规则。
- **拒绝会污染内容的 Content-Type。** 缺少 `Content-Type`、`application/x-www-form-urlencoded`、`multipart/*` 一律返回 415。curl 的 `--data-binary` 在不带 `-H` 时默认发送 form-urlencoded；multipart 则会把边界写进文件内容。两者都不是调用方想要的字节，因此明确拒绝，而不是悄悄存下一份错误文件。
- **鉴权不变。** 沿用推理 API Key 与临时 Upload Grant 的既有鉴权，Upload Grant 的作用域不放宽也不收窄，有效期仍为 15 分钟。匿名请求在读取请求体之前拒绝。
- **流式收存。** 请求体直接流入 `ArtifactStore::ingest`，保留期跟随 `log_retention_days`。100 MiB 单文件上限与每个 Principal 16 个、合计 400 MiB 的暂存额度由 ingest 现有路径执行；单请求上传期间占用一个任务名额，完成或失败后释放。
- **暂存期限 1 小时。** ingest 暂存的 idle TTL 由 15 分钟改为 1 小时，因为大文件在慢网络上的单请求上传可能超过 15 分钟，而单请求没有逐片刷新 TTL 的机会。
- **JSON 错误。** 错误体统一为与 Gateway 相同的 `{"error":{"message","type","code"}}` 外形；状态码分类不变：`Invalid`、`NotFound`、`Forbidden`、`Unauthorized`、`Storage` 分别对应 400、404、403、401、500。同一模块中的下载处理函数也走同一映射，不将文件的 404 误报为找不到模型。
- **上传说明改为单条 curl。** Upload Grant 的说明只需 curl 与 POSIX shell，不再依赖 jq；响应中的 `path` 是稳定引用，可直接用于结构化附件，或作为 StraviaRead 的 path 读取内容。该引用不是下载凭据，只有同一 Principal 能解析。
- **内部实现保留。** 暂存表 `artifact_uploads` 与 `artifact_upload_parts` 继续由 ingest 内部使用，不做 migration。原先的三个 trait 方法及其类型收进 `LocalArtifactStore`，不再是公开契约。

## Considered options

- **保留分片上传**：被否。本次需求选择移除外部分片协议，避免上传说明依赖 jq 及跨请求 token、part 清单；代价是中断后必须重传全部内容。
- **单端点同时接受 multipart**：被否。multipart 需要服务端解析边界，且 curl 的 `-F` 形态让文件名、字段名成为额外输入；原始字节更直接，也与“`Content-Type` 就是 MIME”的语义一致。
- **缺少 Content-Type 时按 `application/octet-stream` 处理**：被否。这会让漏写 `-H` 的调用悄悄成功并留下类型错误的文件。

## Consequences

- 不支持断点续传；中途失败须整体重传。
- 同一 Principal 对相同 MIME 与完整内容重复上传得到同一 `path`，与以往的内容身份规则一致，不受请求形态影响。
- 暂存表保留给 ingest 使用，因此不涉及数据库迁移。
- Upload Grant 的有效期与作用域不变；ADR-0048 中的凭据交付、历史保护与 15 分钟规则继续有效。
- 旧客户端若仍调用 `/v1/artifacts/uploads…`，将得到路由错误，没有兼容层；`x-upload-token` 不再参与鉴权，CORS 也不再允许该请求头。

## 与既有决策的关系

- [ADR-0049](0049-snapshot-explicit-media-inputs-as-artifacts.md)：暂存额度（16 个、400 MiB、单文件 100 MiB）不变；“一个文件的多个上传分片只计一个任务”不再适用，因为不再有分片，一次上传请求就是一个任务。
- [ADR-0048](0048-separate-artifact-references-from-transfer-grants.md)：Upload Grant 的授权范围、15 分钟有效期与凭据交付边界不变；“上传会话仍各自独立”改为“每次上传请求相互独立”，完整收存后按 Principal、声明 MIME 与全部字节复用文件身份的规则不变。
