---
status: accepted
---

# 以集合保存 API Key 的透明注入选择

API Key 的透明注入选择改为一个注入项集合：存储为 `api_keys.transparent_injections` JSON 字符串数组，Core 以注入项枚举的集合表示，管理 API 以同名数组字段收发，取值为 `web_search`、`media_understanding`、`media_generation`。原先分别控制联网搜索、多模态理解、媒体生成的三个列及对应的管理 API 字段删除，不保留别名。透明注入总开关 `transparent_injection_enabled` 仍是独立列：总开关关闭时已选择的注入项保留但不生效（[ADR-0016](0016-gate-advanced-capabilities-and-separate-transparent-injection.md)），它不是注入项。`mcp_access_enabled` 不变。

逐项一列时，每新增一个注入项都要写双后端 migration、重建 SQLite 的布尔契约触发器、重新导出参考 schema，并修改每条 `api_keys` 读写语句、Core 的 Key 结构体、管理 API 与 WebUI 类型，以及大量逐字段列出 Key 的测试数据；而注入项只是同一概念下的一个取值。改为集合后，新增注入项（例如 [ADR-0096](0096-recall-interaction-chains-through-read-paths.md) 的历史回看）只需新增枚举取值、读取规划、管理面文案与测试。

## 存储契约

数据库只约束该列是合法 JSON 且为数组，不枚举允许的取值，否则新增注入项仍要改 DDL。取值、去重与规范顺序（枚举声明顺序）由 Core 的注入项枚举负责；读到未知取值时报错，不静默忽略。删除某个注入项时，由 migration 从数组中移除该取值，与 `reversible_redaction_mappings.rule_ids` 移除 `kingfisher.*` 的方式一致。Key 认证按 token 读取整行，没有按注入项筛选的查询，因此数组放在行内，而不是子表。

## 管理 API 契约

创建时省略 `transparent_injections` 即为空集合；部分更新时省略表示保留原值，提供时整体替换。未知或重复的取值返回参数错误，并列出允许的取值。旧的 `inject_*` 字段与已删除的 `allow_*` 字段一样被拒绝。这是管理 API 的 breaking change：WebUI 与 Desktop 同步切换。

## Considered options

- **保持逐项一列**：数据库层类型约束最强，但扩展成本如上，且随注入项线性增长。
- **子表 `api_key_transparent_injections`**：关系模型最规范，可用外键级联；但每次 Key 认证都多一次 join 或查询，若对取值加 CHECK，新增注入项仍要改 DDL。
- **位掩码整数**：紧凑，但在面向 DBA 的参考 schema 中不可读，取值上限约束仍随注入项变化。
- **JSON 对象 `{"web_search": true}`**：能显式表达「未选」，但「缺省」与 `false` 成为同一状态的两种写法。
- **存储与 Core 改为集合、管理 API 保持布尔字段**：不改公共契约，但管理 API、WebUI 类型与表单绑定仍要逐项加字段。

## Consequences

- 失去数据库层的逐值约束；非法取值只能由 Core 拦截。
- 需要一次性双后端数据迁移：三列合成数组后删除；SQLite 须先删除引用旧列的布尔契约触发器，再删列并重建只覆盖剩余布尔列的触发器。`api_keys` 行数很少，迁移耗时可以忽略。
- 旧版本程序读到它不认识的取值时会拒绝该 Key，而不是丢弃该取值后在下次保存时抹掉它。
- 管理 API 的调用方需改用数组字段。
