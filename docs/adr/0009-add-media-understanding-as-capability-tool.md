---
status: accepted
---

# Add Media Understanding as a capability-owned internal Agent

Stravia 以公开工具 `understand_media` 提供 Media Understanding：普通推理使用隐藏 PlatformTool，MCP 直调同一 contract；专用 adapters 调用一个 internal-only Agent Definition，并直接把其 Agent Turn 投影为 Media Understanding Turn，而不新增 Runner、Backend enum 或 Turn kind。当前 Revision 只支持静态 JPEG/PNG/WebP 源 Artifact；Media preprocessor 在 AgentRunner 前生成 write-once JPEG Q85/4:4:4 derivative，模型接收 derivative，tool/Report 始终引用 principal-scoped source ArtifactId。这样 ArtifactStore 与 AgentRunner 保持通用，Report provenance、媒体压缩和 outer non-vision rewrite 各有单一 owner。

完整 contracts、状态、压缩、routing、persistence、安全边界、迁移与已接受风险见 [`docs/design/media-understanding.md`](../design/media-understanding.md)。WebP 上游支持的一手研究见 [`docs/research/media-understanding-webp-support.md`](../research/media-understanding-webp-support.md)。

本决策部分取代 ADR-0007/ADR-0008 的两项既有约束：Media 与 Web Research 的 capability flag 授权使用各自管理员配置的 hidden Model，不再要求 API key 的 `model_ids` 包含该 Model，但每个 hidden Model Turn 仍动态检查 key 状态并计入 key quota；现有 generic Agent list/patch Admin API clean cutover 删除，内置产品能力改由专用 Admin surface 管理。普通 proxy 与 generic `agent_<slug>` 的 model binding 不变，AgentDefinitionRegistry、AgentRunner 和 generic adapters 继续保留。

## Considered options

- **复制 WebResearchRunner/Backend enum**：Media 当前只有一种 ModelTurn/Agent execution 路径，会重复 AgentRunner 已拥有的 repair、Turn、cancellation、usage 与 persistence。
- **直接公开 `agent_media`**：实现最少，但无法提供稳定的 `understand_media` schema、Media Report provenance、outer image rewrite、专用 capability ACL 与产品 Admin surface。
- **把 JPEG normalization 放入 ArtifactStore 或 AgentRunner**：前者破坏 immutable byte-store 边界，后者把用例特定媒体策略扩散到通用 Agent core；因此由 adapters 共用的 MediaInputPreprocessor 在 Runner 前拥有。
- **给 hidden capability Model 继续要求 key model binding**：保持原安全 seam，但 capability grant仍不能独立授权平台服务；最终让 capability flag拥有间接执行授权，同时保留动态 key/quota检查。
- **统一 lossy WebP**：OpenAI、Anthropic、Gemini均官方支持，Stravia codec也透传 MIME；但 custom/OpenAI-compatible Provider 未证实，且 lossy encode需要额外 libwebp。最终选择基线 JPEG以缩小 compatibility和dependency风险。
- **保留 generic Agent Admin API**：对未来通用 Definitions灵活，但当前产品能力都有独立 owner，generic surface会暴露内部 Definition并形成第二管理入口。

## Consequences

- `understand_media` 是稳定公共身份；本次 discovery必须明确只支持静态图片。未来 audio/video通过新 Definition Revision扩展，旧 Turn保持原 Revision。
- non-vision bridge把图片就地替换为 source Artifact marker；hidden tool只能读取当前 Inference Run与 parent Media Turn，MCP只能读取 principal-owned ready Artifact。
- 父 Route优先 native vision；capability plan在 Inference Run开始固定，原生路径失败不切 bridge。父模型既无 vision又无 tools时前置拒绝。
- transcript严格 append-only以保持同 Target canonical prefix identity；Provider cache hit与 Target affinity仍是 best-effort。
- 自动快照先保留1小时，成功/有效 partial Turn后 source+derivative提升到7天；`media_derivatives`必须跨重启保持 write-once映射。
- 本决策接受无媒体预处理并发上限、累计媒体最终超 context/provider limit、JPEG generation loss、忽略ICC、ArtifactId进入模型上下文、request-bound无幂等/后台恢复，以及 generic Admin API breaking clean cutover的风险。
