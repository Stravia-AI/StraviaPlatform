---
status: accepted
---

# Own Open Responses as a dated protocol

Stravia 将官方 Open Responses `2026-04-24` 作为唯一 Responses-shaped canonical baseline，以 `open-responses/responses/2026-04-24` 独立于 OpenAI rolling Responses API；OpenAI 仅保留 vendor adapter 所有权。Ingress 以客户端兼容为优先：结构安全的 additive 顶层字段和非 Namespaced hosted tool 声明可以进入 compatibility envelope，同协议 Target 原样保留，跨协议 Target 在不改变任务内容、工具硬要求或成本上限时可以省略。Request、response、stream 与 continuation 仍收敛到一个 ordered `AiItem` Graph，硬语义继续服从 representability gate。

## Considered options

- **把现有 adapter 直接改名为 Open Responses**：改动较小，但会保留 OpenAI-owned types、重复 response facts 和含混的协议身份，拒绝。
- **同时保留 OpenAI Responses 与 Open Responses**：可以分别追随两个 surface，但会形成两个高度重叠的 canonical contract、registry 和测试矩阵，拒绝。
- **完整跟随 OpenAI rolling Responses API**：字段面更广，但任何 Provider 更新都可能无版本地改变 Stravia canonical semantics，拒绝。
- **只接受固定 dated schema**：边界最严格，但会让 Codex 等 rolling 客户端在 additive metadata 或 hosted tools 上无谓失败，拒绝。
- **固定 canonical baseline 加 compatibility envelope**：以 dated release 固定已理解的语义，同时接受并按目标能力保留或省略 additive surface，采用。

## Consequences

- `OpenAIResponsesV1`、`openai-responses` aliases 和旧目录一次性删除，不保留兼容层或未发布数据迁移。
- Dated OpenAPI 继续作为 canonical baseline 和回归基线，不再作为 ingress additive surface 的封闭 allowlist。
- 同协议 Target 保留未知 additive 顶层字段和 rolling hosted tools；未知 `owner:*` Namespaced Extension 仍须显式注册。
- 跨协议路径优先成功转换：`client_metadata`、`include`、response decoration 以及 `tool_choice=auto/none` 下目标不能执行的 hosted tools 可以省略。
- 无法表达的内容、身份、引用、结构、required/named tool 或其它硬约束仍在 Provider call 前拒绝。
- `background` 和 `compact` 以稳定的 unsupported-feature contract 返回，不以 stub 伪装支持。
- Additive 兼容不要求新的 dated identity；改变 canonical semantics、生命周期或硬约束仍必须引入新 identity 与决策。

完整协议 profile、transport、state、extension、error 与验收契约见 [`docs/design/open-responses.md`](../design/open-responses.md)；规范事实与来源见 [`docs/research/open-responses-standard.md`](../research/open-responses-standard.md)。
