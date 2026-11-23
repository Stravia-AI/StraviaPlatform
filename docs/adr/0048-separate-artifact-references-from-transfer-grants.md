---
status: accepted
---

# 分离 Artifact 引用与临时传输授权

Artifact Reference 标识归属 Principal 的文件，不是访问凭据。同一 API Key 可以跨对话解析，其他 Principal 不得解析。通过归属鉴权后签发单文件、限时、持有即授权的 Download Grant，让普通下载工具和不持有平台 API Key 的 Provider 都能取件；代价是泄露的链接在有效期内可被转用。公开下载入口只表示网络可达，不让文件永久公开。

Upload Grant 仅允许为指定 Principal 新上传，不授予推理、读取或管理权限。签发后固定有效 15 分钟，不因使用续期；在有效期内可独立上传多个文件，支持并行和重试而不收窄为一次性授权。外部上传是 [ADR-0092](0092-replace-multipart-artifact-upload-with-single-request.md) 的单请求上传，完整收存后按 Principal、声明 MIME 和全部字节复用文件身份；稳定引用可按 [ADR-0091 的读取契约](0091-read-paths-with-colon-selectors-and-plain-fields.md) 使用。

上传提示词注入默认关闭。启用后，模型只接触占位符；普通回答和客户端工具参数可替换成真实授权，思考与 Platform Tool 参数不可替换。同一响应复用仍有效的授权；到期后才签发新的凭据，关闭注入不撤销已签发授权，也不豁免 API Key 状态检查。客户端回传真实授权时，出站模型文本仍须恢复为占位符，关闭注入或凭据保护也不例外。

真实授权不进入模型上下文、平台历史、进程日志或普通客户端错误；普通 Observation 和 Debug 只过滤实际 HTTP Authorization header 值，其他位置的授权可能留在诊断面。具体交付和保留见 [Agent Core](../design/agent-core.md)，诊断边界见[请求记录](../design/interaction-observation.md)。
