# 架构决策记录（ADR）

这里只收录已接受且仍有效的架构决策。只有同时满足三个条件才写 ADR：难以反转、缺少背景时会让人意外、存在需要说明的真实权衡。ADR 记录选择及其理由；当前契约和实现细节见[设计文档](../design/README.md)。

保留既有编号。同号 ADR 用完整文件名区分。

## 推理与协议

- [ADR-0001](0001-inference-run-lifecycle-seam.md)：统一 Inference Run 编排与交付边界。
- [ADR-0006](0006-own-protocol-conversion-behind-canonical-stages.md)：通过绑定协议对转换 canonical IR。
- [ADR-0011](0011-own-open-responses-as-a-dated-protocol.md)：以 dated Open Responses 为协议基线。
- [ADR-0013](0013-own-provider-transport-behind-model-turn-executor.md)：在 Model Turn 内统一 Provider Transport。
- [ADR-0033](0033-stream-text-and-project-post-text-thinking.md)：立即交付正文并投影正文后的思考。
- [ADR-0056](0056-grow-protocols-by-unique-wire.md)：按独有 wire contract 扩展协议。
- [ADR-0080](0080-try-protected-reasoning-once-and-block-only-foreign-origins.md)：乐观重放受保护推理，按来源记忆拒绝。
- [ADR-0081](0081-unify-request-controls-without-vendor-parameters.md)：统一请求控制，不暴露供应商参数。
- [0088-carry-responses-thinking-markers-in-encrypted-content.md](0088-carry-responses-thinking-markers-in-encrypted-content.md)：在 Responses encrypted content 中携带 Thinking Marker。

## 历史链与上下文

- [ADR-0014](0014-discover-target-continuation-from-effective-prefixes.md)：用严格有效前缀发现 Target Continuation。
- [ADR-0019](0019-persist-generation-history-independently-of-upstream-store.md)：本地留存不受上游 `store` 控制。
- [ADR-0020](0020-discover-generation-parents-from-strict-canonical-history.md)：用严格 canonical 历史发现父节点。
- [ADR-0022](0022-materialize-generation-chains-from-deltas.md)：从增量物化 Generation Chain。
- [ADR-0027](0027-own-client-visible-history-behind-generation-chain.md)：将客户端历史投影和节点合法性归入 Generation Chain。
- [ADR-0028](0028-persist-hidden-history-behind-markers.md)：用 Marker 引用隐藏历史。
- [ADR-0047](0047-separate-native-compaction-ancestry-from-context.md)：分离 Responses 压缩祖先与有效上下文。
- [ADR-0076](0076-deduplicate-turn-chain-items-and-share-binary-storage-codec.md)：去重 Turn Chain 项并共享二进制存储编码。
- [ADR-0096](0096-recall-interaction-chains-through-read-paths.md)：通过只读路径回看交互链。
- [ADR-0098](0098-remove-anthropic-remote-compaction.md)：将原生压缩限定于 Responses。

## 路由与模型目录

- [ADR-0018](0018-use-revisioned-stravia-model-catalog.md)：使用版本化 Stravia Model Catalog 作为模板源。
- [ADR-0023](0023-prefer-targets-by-canonical-cache-prefix.md)：按 canonical 前缀偏好有缓存的 Target。
- [ADR-0032](0032-persist-allowance-samples-for-exhaustion-forecast.md)：持久化额度样本以预测耗尽。
- [ADR-0034](0034-layer-route-target-selection.md)：分层选择 Route Target。
- [ADR-0035](0035-keep-disabled-targets-on-routes.md)：保留 Route 上的禁用 Target。
- [ADR-0036](0036-use-signed-i32-target-priority.md)：使用完整的有符号 32 位优先级范围。
- [ADR-0037](0037-present-route-targets-as-priority-lanes.md)：用优先级泳道编辑 Target 分组。
- [ADR-0054](0054-persist-provider-model-snapshots.md)：持久化 Provider Model 快照。
- [ADR-0057](0057-own-provider-write-and-route-bind-as-two-modules.md)：分开 Provider 写入与 Route 绑定。
- [0073-mark-provider-credential-invalid-on-upstream-auth-rejection.md](0073-mark-provider-credential-invalid-on-upstream-auth-rejection.md)：上游鉴权拒绝后标记 Provider 凭据无效。
- [0073-register-runtime-catalog-profiles-through-base-vendor.md](0073-register-runtime-catalog-profiles-through-base-vendor.md)：由 base Vendor 注册运行时目录 Profile。
- [ADR-0078](0078-suspend-provider-routing-on-guarded-allowance-exhaustion.md)：受守护额度耗尽可暂停 Provider 路由。
- [ADR-0084](0084-pin-provider-model-fields-individually.md)：逐字段钉住 Provider Model 快照。

## Vendor 插件与网络

- [ADR-0002](0002-web-access-provider-seam.md)：Core 拥有 Web Access，宿主注入 Provider adapter。
- [ADR-0067](0067-separate-vendor-identity-from-package-and-protocol.md)：分离 Vendor 身份、安装包与协议。
- [ADR-0068](0068-trust-vendor-plugins-with-connection-credentials.md)：Vendor 插件以受信任代码访问连接凭据。
- [ADR-0069](0069-bundle-pinned-codecs-in-vendor-plugins.md)：Vendor 插件携带固定版本的 codec。
- [ADR-0070](0070-run-all-model-vendors-as-wasm-plugins.md)：所有模型 Vendor 通过 Wasm 插件运行。
- [ADR-0071](0071-orchestrate-vendors-through-host-network-capabilities.md)：经宿主受控网络能力编排 Vendor。
- [ADR-0072](0072-use-wasmtime-components-and-wit-for-vendor-plugins.md)：用 Wasmtime Component 和 WIT 定义插件边界。
- [ADR-0074](0074-select-custom-protocols-through-one-base-profile.md)：用单一 base Profile 选择自定义协议。
- [0091-authenticate-plugin-catalogs-and-pin-install-origins.md](0091-authenticate-plugin-catalogs-and-pin-install-origins.md)：验证插件目录并钉住安装来源。
- [ADR-0093](0093-split-controlled-network-into-stravia-net-with-major-version-admission.md)：`stravia-net` 按主版本准入。
- [ADR-0094](0094-own-vendor-egress-with-rama-and-patched-rustls.md)：用 rama 和补丁版 rustls 统一 Vendor 出站。
- [ADR-0095](0095-describe-client-egress-as-host-shapes.md)：通过宿主 Egress Shape 配置客户端出站。

## 平台工具与内置 Agent

- [ADR-0003](0003-general-mcp-tool-server.md)：MCP Server 独立于 Web Access。
- [ADR-0007](0007-own-agent-execution-behind-native-seams.md)：原生 AgentRunner 复用 Model Turn 和 Turn Chain。
- [ADR-0009](0009-add-media-understanding-as-capability-tool.md)：以能力专用的内置 Agent 提供 Media Understanding。
- [ADR-0016](0016-gate-advanced-capabilities-and-separate-transparent-injection.md)：分离平台能力门禁与透明注入。
- [ADR-0017](0017-rename-web-research-to-web-search-and-split-tool-identities.md)：统一 Web Search 运行器，分离公开与内部工具身份。
- [ADR-0048](0048-separate-artifact-references-from-transfer-grants.md)：分离 Artifact 引用与传输授权。
- [ADR-0049](0049-snapshot-explicit-media-inputs-as-artifacts.md)：将显式媒体输入快照为 Artifact。
- [ADR-0050](0050-configure-s3-direct-download.md)：配置 S3 直连下载。
- [ADR-0051](0051-disambiguate-artifact-download-and-understanding.md)：区分 Artifact 下载和内容理解。
- [ADR-0061](0061-bind-media-generation-to-routes.md)：将媒体生成绑定至 Route。
- [ADR-0064](0064-use-stravia-uri-references-without-sessions.md)：用无 Session 的 Stravia URI 引用资源。
- [ADR-0082](0082-expose-queryable-tool-executions-after-bounded-wait.md)：工具执行在有界等待后凭引用查询。
- [0091-read-paths-with-colon-selectors-and-plain-fields.md](0091-read-paths-with-colon-selectors-and-plain-fields.md)：统一读取路径、冒号选择器和纯文本字段。
- [ADR-0092](0092-replace-multipart-artifact-upload-with-single-request.md)：用单次请求上传 Artifact。
- [ADR-0097](0097-store-api-key-transparent-injections-as-a-set.md)：将 API Key 的透明注入选择保存为集合。

## 凭据保护

- [ADR-0005](0005-client-credential-security-seam.md)：统一客户端认证和 Model 授权规则。
- [ADR-0042](0042-keep-reversible-redaction-detection-local.md)：本地检测可逆脱敏的凭据。
- [ADR-0043](0043-scope-redaction-mappings-to-api-keys.md)：将可逆映射限定到归属 API Key。
- [ADR-0044](0044-restore-secrets-in-tool-call-arguments.md)：在工具调用参数中恢复凭据。
- [ADR-0046](0046-complete-model-turn-after-redaction-publication.md)：先发布脱敏映射，再完成模型轮次。
- [ADR-0087](0087-list-credential-hits-per-mapping-with-retained-item-context.md)：按映射列出留存的命中和复用位置。
- [0088-mask-credentials-at-admin-boundary-and-page-reuses.md](0088-mask-credentials-at-admin-boundary-and-page-reuses.md)：在管理边界掩码凭据，并对复用记录分页。
- [ADR-0090](0090-gate-credential-protection-by-rule-and-strength-and-expire-mappings.md)：由规则和强度控制保护，按最近检测时间过期。

## 交互观测与计费

- [ADR-0053](0053-keep-one-interaction-across-generation-roots.md)：跨 Generation Root 保留同一交互身份。
- [ADR-0060](0060-own-interaction-observation-outside-generation-chain.md)：交互观测独立于 Generation Chain。
- [ADR-0062](0062-persist-diagnostic-content-at-canonical-item-boundaries.md)：按 canonical item 边界持久化诊断正文。
- [ADR-0063](0063-record-four-direction-wire-debug-at-transport-boundaries.md)：在传输边界记录四向 Wire Debug。
- [ADR-0077](0077-slim-interaction-observation-and-file-debug-manifests.md)：精简交互观测写入和文件诊断清单。
- [ADR-0083](0083-separate-rejected-requests-from-failed-requests-and-count-upstream-errors.md)：区分拒绝与失败，并计量上游错误。
- [ADR-0085](0085-record-cost-and-billing-with-interaction-observation.md)：在交互观测中记录成本和计费。
- [ADR-0086](0086-freeze-dual-currency-amounts-and-derive-history-from-first-rate.md)：冻结双币金额，仅用首个汇率派生历史。
- [0088-let-allowance-plugins-suggest-provider-billing.md](0088-let-allowance-plugins-suggest-provider-billing.md)：允许额度插件建议 Provider 计费规则。
- [ADR-0089](0089-share-content-store-and-normalize-observation-writes.md)：共享正文存储，规范化观测写入。

## 管理与基础设施

- [ADR-0038](0038-own-live-and-staged-client-projection-in-one-session.md)：由同一会话管理实时和暂存客户端投影。
- [ADR-0039](0039-pin-tauri-updater-signing-key.md)：固定 Tauri Updater 签名公钥。
- [ADR-0040](0040-separate-admin-identity-and-revoke-sessions.md)：分离管理员身份并支持会话撤销。
- [ADR-0041](0041-own-database-connection-in-config-file.md)：数据库连接归配置文件管理。
- [ADR-0052](0052-allow-explicit-http-admin-origin.md)：支持显式配置管理 HTTP Origin。
- [ADR-0058](0058-own-connect-client-apply-in-desktop.md)：Desktop 拥有 Connect Client Apply。
- [ADR-0059](0059-connect-client-apply-upserts-provider-not-model.md)：Connect Client Apply 更新 Provider 而非逻辑 Model。
- [0073-cutover-to-single-baseline-schema.md](0073-cutover-to-single-baseline-schema.md)：每种数据库仅维护一个建库基线。
- [ADR-0079](0079-keep-console-chat-history-in-the-admin-client.md)：对话历史留在管理客户端本地。
