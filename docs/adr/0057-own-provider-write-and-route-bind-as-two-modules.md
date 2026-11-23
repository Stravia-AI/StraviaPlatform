---
status: accepted
---

# Own Provider writes and Route binds behind two crate-private modules

Stravia 将管理侧的连接写入与 Target 绑定分别交给 `stravia-core` 的 Provider module 与 Route module，WebUI 与 Admin HTTP 只做 adapter。Provider module 拥有连接创建、校验与认证生命周期，供应商特有流程由 Vendor Plugin 提供。Route module 拥有 Provider Model 快照、可用性与同步、Route 和 Target 写入；不让调用方重复计算可用性或管理空 Route。

连接与路由有不同的不变量：把它们合成一个管理入口只会出现宽 intent 和浅分发，把规则留在 WebUI 则会复制到其他 adapter。连接模块不拥有上游发现的模型集合；它是 Route module 同步快照的输入。移除 Provider 在同一存储事务内摘除关联 Target 和被清空的 Route，不让管理 adapter 编排级联。Route ID 面向客户端；内部存储 ID、Provider ID、上游模型 ID 与 Target ID 各有独立身份，修改展示或启用状态不能隐式重建 Target。

[ADR-0084](0084-pin-provider-model-fields-individually.md) 删除整条重新导入。模型同步或逐字段恢复同步改变思考规格时，Route module 仍原子提交快照与关联 Target 的 Generated Mapping，基于事务内最新绑定保留 Overridden Mapping，并校验既有手工映射仍可表达。提交失败不能留下部分更新；成功返回后当前实例的新请求看到一份完整配置，不承诺跨实例同时切换。现行接口和事务边界见[整体架构](../design/architecture.md)。
