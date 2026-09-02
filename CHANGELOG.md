# Changelog

## Unreleased

- HTTP/SSE Target Continuation now replays full history once when the upstream conclusively rejects a previous response before execution, including xAI Zero Data Retention responses. Non-retryable request errors no longer quarantine the Target.
- OpenAI-compatible streams now deliver ordinary Text immediately even when Platform Tools are exposed. Public Open Responses reasoning summaries remain live when encrypted reasoning was not requested, and protected reasoning identified at item start also streams its public summary; Thinking after the first Text is projected as a Markdown blockquote in `content` with an authoritative History Marker, preserving replay order without Model Leg-sized buffering. Empty reasoning content is encoded as an array for every Open Responses upstream instead of the provider-incompatible `null`.
- Admin Route payloads now use required `model_id` and optional `display_name`; the legacy `name` field is rejected. Existing Route IDs are preserved during migration, while WebUI labels, model discovery, request logs, and supported client display fields use the effective display name without changing routing identity.
- Web Access 现在自动提供不可删除的 Local Provider，并由 `stravia-web-access` 统一实现 Local、Exa 与 Zhipu 适配器；升级会删除 Brave/Tavily Web Provider，把被清空的 Search/Fetch 列表切换到 Local。
- Route IDs are client model IDs and are now compared exactly, including letter case, across proxy matching, administration, binding, and persistence.
