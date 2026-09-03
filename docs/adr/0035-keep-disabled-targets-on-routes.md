---
status: accepted
---

# Route 上保留已禁用 Target

Target 过去只要出现在 Route 上就会进入选择。管理面需要「配好备用、先不参与选路」，删除会丢掉超时、思考映射等配置，仅编辑期暂存则刷新即丢。

因此 Target 增加持久化的启用/禁用。已禁用 Target 仍属于该 Route，但不参与选择、亲和或冷却。缺省为已启用，现有数据全部视为已启用。一条 Route 必须至少有一个已启用 Target；已启用 Target 必须已配置 Provider 与上游 model。

## Considered options

- 拖出即删除：无法保留备用配置。
- 仅编辑期坞、保存时丢掉未入栈卡片：刷新后备用消失，禁用不是领域状态。

## Consequences

- 选择、Conversation Affinity、Cache Affinity 与 Target Cooldown 只考虑已启用 Target。
- 管理面用栈（已启用、按 Target Priority 分层）和坞（已禁用）表达该状态；手势见 ADR-0037。
- API 省略启用字段时必须视为已启用，以免旧客户端写出全禁用 Route。
