---
status: accepted
---

# Own Provider writes and Route binds behind two crate-private modules

Stravia 将管理员侧的连接写入与 Target 绑定收成 `stravia-core` 里两条 crate-private deep module interface，而不是一个管理接入 façade，也不是 WebUI 里的领域实现。Provider module 写入 Provider：source 为 Catalog Entry 或 custom，拥有 Catalog Entry 解析、Adapter Credentials 校验、Base URL 组装与连通性测试；OAuth（Claude Code、Codex）是内部 seam。Route module 拥有 Provider Model snapshot、Selection Policy、sync、手动条目、Canonical Model 一次性模板，以及 Route / Route ID / Target。WebUI 与现有 Admin HTTP 只做 adapter。这样 Catalog 扁平化、OAuth session、Effective Availability 算术和空 Route 清理不再漏到调用方。

## Considered options

- 合成一个管理接入 module：少一条协作，但连接不变量（凭证、OAuth、Base URL）与 Route 不变量（可用性、Target、Route ID）会挤在接近 implementation 的浅 interface 上。
- 只加深 WebUI：改动面小，但 `admin/providers`、`provider_models`、`models` 仍要跳，deletion test 失败。
- core 里放未保存连接草稿：让校验、OAuth bind、Base URL 组装出现第三种状态；表单瞬时 state 留在 WebUI adapter。
- 单个 `apply(intent)`：入口最少，但 intent 与现有 AdminService 一样宽，`apply` 会变成浅分发器。
- 为未来 Target policy 做开放 `kind + parameters`：现在没有第二条 policy adapter，是假 seam。
- 把上游 `/models` discovery 放进 Provider：发现列表是 snapshot 的输入，不是连接本身。
- 禁止一键用 upstream ID 当 Route ID：与「Route ID 就是客户端请求里的模型 ID」冲突；一键默认等于 Provider Model upstream ID，同一 Route ID 即同一 Route。
- 删除 Provider 时若仍有 Target 则拒绝：更 fail-closed，但本决策选择级联摘 Target，空 Route 一并删除；手动摘掉最后一条 Target 同样删除空 Route。

## Consequences

- 领域词不新增。对象仍是 Provider、Catalog Entry、Provider Model、Route、Route ID、Target。
- Provider interface：`catalog_choices`、`save(Catalog | Custom)`、`test(Existing | Candidate)`、`reconnect(Start | Callback)`、`delete`。`save` 不自动打网。调用方看不见 OAuth session / bind / refresh。
- Route interface：`bind(one_click | at)`、`unbind`、`change`、`add_provider_model`、`sync`。一键 Route ID 默认等于 upstream ID；Provider Model 缺失或 Effective Availability 不可用时不能加新 Target；同一 Provider + Provider Model 在同一 Route 上幂等。
- 内部 adapter 只在已有真实差异处：Storage（SQLite / PostgreSQL / Memory）直注；CatalogSource；OAuthDriver；Provider 的连通性 HTTP 与 Route 的 model-ID discovery HTTP 分开。Effective Availability 不是 port。
- Provider 删除由 Storage 在同一事务里摘除关联 Target 并删除空 Route；HTTP 看不到 Target 行，也不编排持久化细节。
- `ProviderCatalog` 仍是 revision/索引事实源。Canonical Model 只给 Route 当一次性模板。custom 是第二种 source，不是伪装的 Catalog Entry。
- 测试打这两条 module interface。被替代的表单扁平化、手搓 Route 引用、重复 `/models` parse 与浅 Admin 编排测试一并删除。
