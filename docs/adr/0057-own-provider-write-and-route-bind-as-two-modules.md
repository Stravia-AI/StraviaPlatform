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

### Route 数据契约

SQL 行、运行时 `RouteConfig` 与管理 `RouteView` 分离。SQL adapter 的 JSON 包装与查询列不进入运行时配置；管理投影可以附加展示信息，但不能成为第二份可写事实源。Route 存储主键、客户端 Route ID、Provider ID、上游模型 ID 与 Target ID 分属不同身份空间。

`targets` 是唯一 Target 写入入口；`target_provider` / `target_model` 只保留为派生读投影，不接受调用方指定 Target ID。更新省略 `targets` 时，存储事务不得删除或重建 Target 行；显式提交时才原子替换。显示名称与默认思考级别省略表示不改，`null` 表示清除；非空字段和 `targets` 不接受 `null`。这让修改展示或启用状态不会意外改变运行策略与 Target 身份。

### Provider Model reimport 的原子提交与生效

Route module 拥有 Provider Model 写策略及其与 Target 的协调；外侧管理 adapter 只转交操作与结果，不通过 Route module 回调管理 adapter 执行领域写入。Provider 连接 module 保持独立。

一次显式 reimport 必须将 Provider Model 快照与全部关联 Target 的 Generated Mapping 原子提交：任一持久化步骤失败，均不得留下部分更新。Provider Model 的 expected revision 检查继续有效，旧 revision 使整次操作失败。

关联 Route 以提交时的最新状态为准。并发人工编辑形成的 Overridden Mapping 必须保留，仅重新计算仍属 Generated 的行；不因读取后发生 Route 编辑就自动重试整次操作，也不以旧 Target 集合覆盖最新编辑。该保证由 SQLite、PostgreSQL 与 Memory 三个存储 adapter 一致实现。

既有 Thinking Control 可写性校验仍然有效。校验依据新规格和事务内最新的合并结果执行，包括 Generated 行未变化的情况；若手工映射已无法由新规格表示，整次 reimport 失败，不删除该映射，也不以放宽校验换取成功。

Route module 同时拥有持久提交、配置变更通知与当前实例完整运行时快照的切换，不逐条发布关联 Route 的中间状态。reimport 成功返回后，当前实例的新请求必须使用更新后的完整配置；这不承诺多个实例同时切换，也不将数据库与内存描述为同一事务。不得把已经提交后的失败表述为已全部回滚。

PostgreSQL 在该事务中串行化 Route 写入，以覆盖等待期间出现的新绑定；代价是事务期间其他 Route 写入需要等待。外部 Catalog 读取不在该事务内。普通同步保留人工快照、Vendor 写回许可及既有 Provider / Route 分工不因本次深化而改变。
