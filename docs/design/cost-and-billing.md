# 成本与计费设计

> 状态：已实施。
> 更新：2026-10-30
> 相关决策：[ADR-0084](../adr/0084-pin-provider-model-fields-individually.md)、[ADR-0085](../adr/0085-record-cost-and-billing-with-interaction-observation.md)、[ADR-0086](../adr/0086-freeze-dual-currency-amounts-and-derive-history-from-first-rate.md)

## 1. 结论

Stravia 提供两套互相独立的金额事实，术语见 [GLOSSARY](../../GLOSSARY.md) 中的估算成本、计费、消费上限、自定义币种、按量成本与峰谷定价：

- **估算成本**：按 Provider Model 的上游价格（含上下文分档与峰谷定价）和 Confirmed Upstream Usage，估计本实例应付给上游的金额；订阅制 Provider 另按订阅价线性摊销。只用于展示，从不拦截请求。
- **计费**：按 Route 的计费价格（按次或按 Token）向 API Key 记账；设有消费上限的 Key 在已消费达到上限后，新的根请求返回 429。

两者共用一套价格结构与计价函数，金额随 Target 尝试的结束事件写入观测存储，由数据库触发器在同一事务内累加到用量统计桶和 API Key 的已消费累计行（[ADR-0085](../adr/0085-record-cost-and-billing-with-interaction-observation.md)）。每笔金额在发生时同时冻结美元与自定义币种两个值，之后不再改变（[ADR-0086](../adr/0086-freeze-dual-currency-amounts-and-derive-history-from-first-rate.md)）。

Provider Model 价格是成本的唯一输入，因此同时把规格的人工修改改为逐字段固定（[ADR-0084](../adr/0084-pin-provider-model-fields-individually.md)），并删除整条重新导入。

## 2. 范围

纳入：

- Provider 成本方式：不计算（`none`）、按量（默认）或订阅（月付／年付与价格），变更追加生效记录。
- Provider Model 价格的币种标记与峰谷规则；价格字段逐字段固定与恢复同步。
- Route 计费方式：不计费（默认）、按次、按 Token。
- API Key 消费上限、已消费、清零。
- 唯一一种自定义币种。
- 用量统计、请求详情、API Key、Provider、Route 与设置页面的展示和编辑入口。

不纳入：多于一种的自定义币种；中国法定节假日与调休；订阅的续费日、锚定日等超出生效记录的细节；自动周期重置的消费上限；按 Key 与模型组合的上限；面向 Key 持有者的自助查询接口；上游按次成本价；上游服务端搜索／抓取等附加费；本地累计上游套餐额度（继续使用现有 Provider Allowance Monitor 与守卫项）。

## 3. 金额与币种

### 3.1 金额表示

所有落库金额是 `i64` 纳单位（1 单位 = 10⁻⁹ 美元或 10⁻⁹ 自定义币种单位），列名以 `_nanos` 结尾。整数让 SQLite 与 PostgreSQL 的触发器累加精确且一致；单个累计值上限约 9.2×10⁹ 个币种单位。

计价在 Rust 中用 `rust_decimal::Decimal` 完成，每笔总额最后四舍五入（远离零）到纳单位一次，不逐项取整。溢出 `i64` 时该笔金额记为未知并写 `tracing::warn!`，不截断。

本功能的金额配置与统计管理 API 以十进制字符串收发金额；纳单位只存在于存储与 Core 内部。目录来源价格保留原有的 JSON 数字形状，但解析与序列化使用 `Decimal`，不经过 `f64`；标准化缓存中的十进制字符串和来源中的科学计数法数字都可读取。

### 3.2 自定义币种

存于 `settings` 表（`docs/database/sqlite.sql:485-489`）的单行 `name = 'custom_currency'`，`value` 为 JSON：

```json
{
  "symbol": "¥",
  "usd_rate": "7.1",
  "first_rate": "7.2",
  "configured_at": "2026-10-28T08:00:00Z"
}
```

- `usd_rate` 表示「1 美元 = X 自定义币种」，必须大于 0。
- `symbol` 去除首尾空白后为 1–8 个字符，只是显示符号，不是 ISO 代码。
- `first_rate` 是首次设定时的汇率，写入后永不改变；`configured_at` 同理。
- 不能删除自定义币种。修改 `symbol` 只改变显示，不换算；修改 `usd_rate` 只影响之后冻结的金额。

首次设定在一个事务内完成：写入该行，并把所有非空 `api_keys.spend_limit_nanos` 乘以 `first_rate` 后四舍五入。API Key 是管理配置，数量有限，这是唯一一次数据换算；不回填任何观测、统计或订阅记录。

通用的 `PUT /api/v1/settings/{key}`（`backend/apps/stravia-server/src/admin_routes/settings.rs`）必须拒绝写入 `custom_currency`，否则会绕过上述换算与不可删除约束。

Gateway 在内存中持有当前币种快照，专用写接口成功提交后替换。计价时读取这份快照，不在请求路径上读 `settings`。

### 3.3 双金额与展示换算

每笔金额（尝试成本、计费、订阅生效记录的价格）冻结两个值：

|列|含义|
|---|---|
|`*_usd_nanos`|美元金额，总是存在|
|`*_custom_nanos`|按当时汇率的自定义币种金额；未设定自定义币种时为 `NULL`|

价格按填写币种保存。以美元填写的部分在美元一侧精确、在自定义一侧按 `× usd_rate` 换算；以自定义币种填写的部分反之按 `÷ usd_rate` 换算。混合币种的价格分别累计两侧后再各自取整。

统计桶与已消费累计行不存美元总额，只存两列：

- `*_custom_nanos`：带有自定义金额的记录之和；
- `*_usd_unconverted_nanos`：没有自定义金额的记录（设定自定义币种之前产生的记录）的美元金额之和。

展示金额由 Core 计算，WebUI 不复制换算规则：

$$
\text{展示金额} =
\begin{cases}
\text{usd\_unconverted} & \text{未设定自定义币种} \\
\text{custom} + \text{round}(\text{usd\_unconverted} \times \text{first\_rate}) & \text{已设定}
\end{cases}
$$

设定前只有美元列有值，因此第一行就是美元合计；设定后的新记录都带自定义金额，旧记录统一按首次汇率展示，且结果不随后续改汇率而变化。请求详情展示单笔记录时使用同一规则：有自定义金额用自定义金额，否则美元 × `first_rate`。

## 4. 价格结构与计价

### 4.1 价格结构

Provider Model 价格与 Route 的按 Token 计费价格复用同一个类型 `ModelCost`（`backend/crates/stravia-core/src/provider_models.rs`），新增两个本地字段：

```json
{
  "input": "0.15",
  "output": "0.6",
  "cache_read": "0.003",
  "cache_write": null,
  "context_over_200k": null,
  "tiers": [],
  "custom_currency_fields": ["output"],
  "peak": {
    "timezone": "Asia/Shanghai",
    "weekdays": [1, 2, 3, 4, 5],
    "ranges": [
      { "start": "09:00", "end": "12:00" },
      { "start": "14:00", "end": "18:00" }
    ],
    "multiplier": "2"
  }
}
```

- 价格单位沿用现有约定：每百万 Token 的金额。
- `custom_currency_fields` 是以自定义币种填写的价格字段集合，取值为 `input`、`output`、`cache_read`、`cache_write`、`tiers`；`tiers` 同时覆盖 `context_over_200k` 与全部分档。缺省为空，即全部美元。未设定自定义币种时必须为空。
- `peak` 至多一条：IANA 时区、ISO 星期（1 = 周一）、同一天内的时段列表（`HH:MM`，含起点不含终点，终点可为 `24:00`，不跨午夜、不重叠）和一个大于 0 的倍率。
- 两个新字段只来自管理写入。目录导入与模型发现得到的价格一律去除这两个字段，来源价格总是美元。

`metadata_json` 已保存完整的 `ModelCost`（`backend/crates/stravia-core/src/storage/sqlite/provider_models.rs`），新字段随之保存，不新增列。`cost_input` 等投影列与 `provider_model_cost_rules` 继续保存填写值，不用于金额计算。

### 4.2 计价函数

Core 新增纯函数模块（建议 `backend/crates/stravia-core/src/pricing.rs`），输入价格、用量、尝试开始时间与币种快照，输出 `Option<Amount>`，`Amount { usd_nanos, custom_nanos: Option<i64> }`。估算成本与按 Token 计费共用此函数。

设总输入 $I$、输出 $O$、缓存读 $R$、缓存写 $W$（Total Input Tokens 已包含缓存输入）：

1. **未知判定**：$I$ 或 $O$ 未知时结果未知。$R$、$W$ 未知按 0 计，即全部按普通输入计价，宁可高估。
2. **选档**：把 `context_over_200k` 视为阈值 200000 的分档，与 `tiers` 合并；取满足 $I >$ 阈值的最大阈值档，没有则用基础价格。
3. **分项**：普通输入 $\max(0, I - R - W)$、输出 $O$、缓存读 $R$、缓存写 $W$。Token 数大于 0 的分项必须在所选档有价格，否则结果未知；Token 数为 0 的分项不需要价格。
4. **峰谷**：尝试开始时间换算到 `peak.timezone` 后，若星期在 `weekdays` 内且时刻落在某个时段内，所有分项单价乘以 `multiplier`。节假日不识别。
5. **汇总**：按 §3.3 分币种累计、换算、取整。

推理 Token 按协议语义已计入输出，不单独计价；实现时须按各协议的用量归一化核对 `output_tokens` 是否包含推理，以及 Total Input 是否包含缓存写入，不一致时修正本节公式而不是在计价函数中按协议分支。

时区换算需要 `chrono-tz`。它已在 workspace 依赖中（`Cargo.toml:61`），`stravia-core` 需新增对它的引用。

### 4.3 峰谷规则的编辑默认值

价格编辑器「添加峰谷定价」一次性预填：`Asia/Shanghai`、周一至周五、`09:00–12:00` 与 `14:00–18:00`、倍率 `2`。这与 DeepSeek 的高峰时段一致；models.stravia.cn 收录的 DeepSeek 价格是低谷美元价，因此高峰为 ×2。不提供命名预设，预填后与手工填写没有区别。

### 4.4 Provider Model 的来源价格

模型同步、手动添加预填与创建共用价格富化规则：人工固定值优先，其次保留渠道自己的价格；仅在来源整组缺价时，尝试从 `models.stravia.cn/providers/{provider_id}/models.json` 取得模型厂商的官方 API 参考价，否则保持未知。价格组包括基础输入／输出、缓存、多模态与上下文分档，任一分项有值（包括明确的 `0`）就不再补价；不会把渠道的部分报价与官方价拼接。

价格身份优先采用显式 `canonical_id` 或已有 Canonical 来源身份，否则按模型 ID、有限格式归一、最后按名称做唯一匹配。归一保留完整版本与型号，只容忍大小写、命名空间和空白、`-`、`_`、`.` 分隔差异；Claude 的尾部 `thinking` 可作为价格别名，推理 Token 仍按输出计价。官方目录项必须声明相同的 `canonical_id`，或匹配完整版本 ID；不得仅凭同名跨日期或版本引用价格。型号、preview、日期及其他 variant 不会被删除，任何阶段有歧义就不继续猜测。

模型厂商 scope 只采用 Canonical ID 第一段与 Provider ID 相同的目录，不将相近品牌、区域或套餐视作可替代身份。Antigravity 等混合渠道逐模型解析厂商，不整体绑定到某一家；没有官方报价的 GPT-OSS 等模型保持未知，不采用转售商报价冒充官方价。

富化只复制标准 `ModelCost`，仍以 `Decimal` 表示美元／百万 Token，并沿用逐字段固定、本地币种与峰谷规则；不改变渠道的上下文、模态、请求模型 ID、selector 或可用模型集合。官方 scope 每次同步按厂商复用现有目录缓存。可选价格目录暂不可用时明确记录失败，保留上次有效来源价格及固定值，不伪造报价，也不把成功的账号模型发现改成失败；成功刷新后官方不再提供价格时，未固定的旧参考价恢复未知。

官方价只用于参考按量成本，不代表订阅实际支出，不改写 Provider 订阅生效记录，也不改变 Route 对 API Key 的计费或已经冻结的历史金额。

## 5. 估算成本

### 5.1 Provider 成本方式

新表 `provider_cost_plans` 记录每次变更，只追加：

|列|类型|说明|
|---|---|---|
|`id`|TEXT PK||
|`provider_id`|TEXT NOT NULL|不加外键，Provider 删除后保留历史|
|`effective_from`|INTEGER NOT NULL|毫秒 UTC，等于写入时刻|
|`basis`|TEXT NOT NULL|`none`、`usage` 或 `subscription`|
|`period`|TEXT|`month` 或 `year`；仅订阅|
|`price_usd_nanos`|INTEGER|仅订阅|
|`price_custom_nanos`|INTEGER|仅订阅；未设定自定义币种时为空|
|`price_currency`|TEXT|填写币种 `usd` 或 `custom`；仅订阅|
|`created_at`|TEXT NOT NULL||

索引 `(provider_id, effective_from)`。没有任何记录的 Provider 视为按量。

- 三种方式互斥，记录只追加：
  - `none`（不计算）：不为该 Provider 的尝试估算或展示任何成本；尝试不写成本基准，也不计入未知成本。Route 计费与 Provider 成本方式无关，仍然照常记账。
  - `usage`（按量）：按 §4.2 计价，是没有任何记录时的默认值。
  - `subscription`（订阅）：按 §5.4 摊销，同时保留尝试的等价按量成本。
- `none` 与 `usage` 记录不携带 `period`、`price` 或 `price_currency`；订阅记录必须携带三者。
- 修改成本方式或订阅价都追加一条记录，生效时间为写入时刻，不支持回溯生效或编辑旧记录。任何一条新记录都结束前一段订阅的摊销；切到 `none` 同样会停止摊销。
- 删除 Provider 时，若当前为订阅，在同一事务内追加一条 `usage` 记录作为订阅终点，历史摊销保持不变。
- 复制 Provider（`/api/v1/providers/{id}/copy`）不复制成本方式，新 Provider 从按量开始，避免同一订阅被计两次。

### 5.2 Provider Model 价格与逐字段固定

实施 [ADR-0084](../adr/0084-pin-provider-model-fields-individually.md)：

- `SnapshotState`（`backend/crates/stravia-core/src/provider_models.rs`）改为 `Unregistered`、`Imported { source, pinned }` 与 `Local`。`pinned` 是已固定的规格字段集合；`Local` 是没有同步来源的手动规格，JSON 沿用现有的 `{"type":"edited","source":null}`。现有 CHECK 本来就允许额外的 `pinned` 键，因此不重建 `provider_models` 表；Rust 读取到带来源的 `edited` 视为存储不变量破坏并报错。
- 新列 `provider_models.source_metadata_json`（可空）保存最近一次同步得到的来源规格。每次成功同步都更新它，不论字段是否固定；同步只把未固定字段写入生效规格。手动模型仅在创建时复制模板内容，之后没有同步来源，不参与 discovery 对账。
- 字段标识与编辑器控件一一对应：`name`、`description`、`limit.context`、`modalities.input`、`modalities.output`、`reasoning_efforts`、`cost.input`、`cost.output`、`cost.cache_read`、`cost.cache_write`、`cost.tiers`。`custom_currency_fields` 中对应的标记随所属价格字段一起写入、固定和恢复。`cost.peak` 是纯本地字段，不参与固定。
- 管理写入只提交改动过的字段，提交的字段即被固定，即使值与来源相同。
- 恢复同步把来源值写回该字段并取消固定，不发起上游或目录请求；来源中没有该字段时恢复为未登记。恢复改变 `reasoning_efforts` 时，在同一事务内重算关联 Target 的 Generated Mapping，事务与锁顺序见 `docs/design/architecture.md` §9.2。
- 删除 `POST /api/v1/providers/{id}/model/reimport`、`AdminService::reimport_provider_model` 及 `RouteModule::reimport_provider_model`（`backend/crates/stravia-core/src/admin/routes/provider_model_records.rs`），以及 WebUI 的 `reimportModel`、`requestReimport` 与 `can_reimport`，不保留别名。

### 5.3 尝试成本的计算与写入

`AttemptObservation`（`backend/crates/stravia-core/src/model_turn/provider/mod.rs`）创建时额外持有：

- 尝试开始的墙钟时间 `DateTime<Utc>`，用于峰谷判定；
- 该 Target 的 Provider Model 价格 `Option<Arc<ModelCost>>`；
- Provider 当前成本方式（`usage`／`subscription`），从 Provider 运行时配置读取最新生效记录；
- 本次 Route 的计费设置，以及根 Principal 的 `api_key_id`（无 API Key 的 Principal 不计费）；
- 当前币种快照。

Provider Model 价格来自 `RoutePolicyState` 现有的定价派生缓存（`backend/crates/stravia-core/src/router/selection.rs`）。该缓存改为保存完整 `ModelCost`，路由流量权重从中派生 `f64`，以自定义币种填写的字段按当前汇率换算为美元后再求比值；不另建第二份价格缓存，失效代次与 TTL 不变。

`AttemptObservation::finish` 在发出事件前，用已确认用量调用计价函数，并在 `RunEvent::TargetAttemptFinished`（`backend/crates/stravia-core/src/interaction_observation/types.rs`）中新增：

```rust
pub struct AttemptAmounts {
    pub cost_basis: CostBasis,              // Usage | Subscription | Untracked
    pub cost: Option<Amount>,               // None = 未知或不计算
    pub billing: Option<BillingOutcome>,    // None = 不计费
}
pub enum BillingOutcome { Charged(Amount), Unknown }
```

每次尝试至多确认一次用量，且在 `finish` 之前或之中完成，因此金额在 `finish` 一次算定。除 `none` Provider 外，所有尝试都计成本：重试、故障切换、隐藏轮次与平台内部调用。成本未知的原因包括：用量未知、所需分项无价、找不到 Provider Model。新尝试通过 `RunEvent::TargetAttemptStarted` 保存 `cost_basis`；`none` Provider 的尝试写 `None`，`finish` 不估算金额，观测存储把 `cost_basis` 与金额列都留空，因此既不显示成本也不计入未知。崩溃恢复时补写的 `interrupted` 终态不带金额但保留该成本方式：有基准时记为成本未知，`none` 时仍为空。运行中的尝试，以及没有 `cost_basis` 的历史尝试，不计入未知成本次数。

观测写入（`backend/crates/stravia-core/src/interaction_observation/store.rs`）把金额写入 SQLite 与 PostgreSQL 的 `target_attempt_observations` 新列（§7.1），触发器负责累计。没有观测作用域的模型调用不产生任何金额；所有归属 API Key 的路径（代理请求、Agent 循环、MCP `tools/call` 调用的平台工具，包括外部搜索）都运行在观测作用域内。

### 5.4 实际成本、订阅摊销与价值对比

- **不计算 Provider**：不产生任何成本金额，Provider 统计的成本为 `null`，尝试也不计入未知成本。
- **按量 Provider**：实际成本 = `cost_basis = usage` 的尝试成本之和。
- **订阅 Provider**：实际成本 = 查询窗口与各订阅生效段重叠部分的线性摊销。月 = 365/12 天，年 = 365 天：

$$
\text{摊销} = \sum_{\text{订阅段}} \frac{\lvert [\text{段起},\text{段止}) \cap [\text{窗口起},\text{窗口止}) \rvert}{\text{周期长度}} \times \text{价格}
$$

  段止为下一条记录的 `effective_from`（任何成本方式，包括 `none`），最新一段止于当前时刻。订阅 Provider 的尝试成本仍按价格计算，作为等价按量成本（`usage_equivalent_cost`），不计入实际成本。
- **全局实际成本** = 全部按量尝试成本 + 全部订阅摊销（包括已删除 Provider 的历史段）。

订阅价值对比（`SubscriptionValue`）只在窗口与订阅段重叠时出现：

- `cost`：该窗口的摊销订阅费用；
- `usage_equivalent_cost`：同窗口内按订阅基准完成的尝试的等价按量成本；
- `multiple`：`usage_equivalent_cost / cost`，用展示币种的精确十进制相除，按「中点远离零」保留 4 位小数；分母为 0 或任一侧不可表示时为 `null`。

倍数是展示口径的比值，不参与准入、计费或路由判断。等价成本存在未知尝试时它是下界：真实按量成本只会更高，因此 WebUI 在 `usage_equivalent_cost.unknown_count > 0` 时标注「≥」，Core 仍返回原值。

保留「按窗口线性日摊销 + 同窗口比值」的理由：摊销是时间的线性函数，对任意窗口可加，多个相邻窗口之和等于合并窗口的结果；生效记录只保存变更时刻与价格，没有也不打算保存续费锚点或账单日，按锚点切分会让历史窗口的结果依赖后来才写入的信息。代价是订阅费用按自然时间均匀分摊，不反映某个月的实际天数或真实扣费日；对「这个窗口内订阅值不值」的判断足够，且同一规则适用于所有 Provider 与所有窗口。本次新增的是精确十进制的 4 位小数倍数、未知尝试时的下界标注，以及分母为 0 时不给出倍数。

摊销在查询时由 Core 从 `provider_cost_plans` 计算；记录数很少，不进入统计桶。订阅价格按 §3.3 的双金额规则展示。记录的生效时间无法表示时，统计查询返回错误，不跳过该记录生成不完整的已知金额。

## 6. 计费

### 6.1 Route 计费设置

`models` 表（Route）新增可空列 `billing_json`，`NULL` 表示不计费：

```json
{ "mode": "per_call", "amount": "0.02", "currency": "usd" }
```

```json
{ "mode": "per_token", "price": { "input": "1", "output": "4", "custom_currency_fields": [], "peak": null } }
```

- 按次：`amount ≥ 0`，`currency` 为 `usd` 或 `custom`。
- 按 Token：`price` 是 §4.1 的完整价格结构，由管理员手工填写，与上游成本无关。
- 计费设置随 Route 运行时配置加载，修改只影响之后的调用。

### 6.2 计费口径

- 只有状态为 `completed` 的尝试计费；失败、取消或中断的尝试不计费。每个 Model Turn 至多有一个这样的尝试。
- 按次：每个成功的 Model Turn 计一次，包括隐藏轮次与平台内部调用。
- 按 Token：用成功尝试的已确认用量按 §4.2 计价，峰谷以该尝试的开始时间判定；用量未知时记 `Unknown`，不计金额但计入未知数。
- 平台工具调用其他 Route 时，产生的 Model Turn 按被调用 Route 的计费方式计费，记在同一根 API Key 上。
- 计费不受 Provider 成本方式影响。

### 6.3 消费上限与准入

- `api_keys` 新增可空列 `spend_limit_nanos`（大于 0），单位为当前展示币种（§3.3）；`NULL` 表示不限。
- 检查发生在根请求准入：代理路径在 `gw.principal_admission.acquire` 之前（`backend/crates/stravia-core/src/proxy/dispatcher/inference_run/engine/mod.rs`），MCP 在 `tools/call` 的 RPM 准入之前（`backend/crates/stravia-core/src/mcp/server.rs`）。先查上限，使被拒的请求不占用 RPM 窗口。
- 检查只读取该 Key 的一行 `api_key_spend`（没有行即已消费为 0），按 §3.3 得到展示币种的已消费，与上限比较，不产生写入。读取失败时返回内部错误，不放行。
- 已消费 ≥ 上限时拒绝：HTTP 429，新增 `GatewayError::ApiKeySpendLimitReached`，稳定码 `STRAVIA_SPEND_LIMIT`，消息 `API key spend limit reached.`，不带 `Retry-After`。各协议错误体沿用额度暂停的形状（`backend/crates/stravia-core/src/proxy/dispatcher/inference_run.rs`）：OpenAI 兼容与 Open Responses 为 `{"error":{"type":"insufficient_quota","code":"api_key_spend_limit_reached",...}}`，Anthropic 为 `rate_limit_error`，Gemini 为 `RESOURCE_EXHAUSTED`。
- 拒绝按现有准入拒绝路径记录（代理为 `reject_before_admission(..., "admission", "spend_limit", ...)`），计入错误的请求。
- 只在根请求准入检查。已准入请求内的后续轮次、工具调用与故障切换继续执行，异步写入也有延迟，因此允许超出上限。
- 达到上限不改变 `is_enabled`；调高上限或清零后下一次准入即恢复。

### 6.4 已消费累计与清零

新表 `api_key_spend`：

|列|类型|说明|
|---|---|---|
|`api_key_id`|TEXT PK|外键 `api_keys(id) ON DELETE CASCADE`|
|`spent_custom_nanos`|INTEGER NOT NULL DEFAULT 0||
|`spent_usd_unconverted_nanos`|INTEGER NOT NULL DEFAULT 0||
|`reset_at`|TEXT|最近一次清零时间|
|`updated_at`|TEXT NOT NULL||

- 触发器在尝试行的 `billing_state` 从 `NULL` 变为 `charged`（插入或更新）时，按所属 Model Turn 的 `api_key_id` 累加该行金额。API Key 已删除时跳过（`WHERE EXISTS`），不得让外键失败中断观测写入。
- 清零只把两列置 0 并写 `reset_at`，单行事务；不删除或改写任何尝试行与统计桶。
- 清零与在途调用之间不做同步：清零前算定、清零后落库的金额计入清零后的已消费。

## 7. 数据库变更

两个后端同步新增 `0030_cost_and_billing.sql` 与 `0031_cost_and_billing_stats.sql`，中间夹一个 Rust 数据阶段；完成后用 `stravia-tools dump-schema` 重新生成 `docs/database/sqlite.sql` 与 `docs/database/postgres.sql`。

大表只做 `ADD COLUMN`，列可空或带常量默认值，且不带 CHECK：SQLite 与 PostgreSQL 都只改元数据，不扫描、不重写已有行，不会长时间持有写锁。取值约束由 Rust 写入方保证。

### 7.1 `0030_cost_and_billing.sql`

- `target_attempt_observations` 新增：`cost_basis TEXT`、`cost_usd_nanos INTEGER`、`cost_custom_nanos INTEGER`、`billing_state TEXT`（`NULL`／`charged`／`unknown`）、`billing_usd_nanos INTEGER`、`billing_custom_nanos INTEGER`。尝试开始时即保存 `cost_basis`；只有进入终态后，`cost_basis` 非空而 `cost_usd_nanos` 为空才计为成本未知，因此崩溃恢复不会漏计，仍在运行的尝试也不会提前计入未知数。`none` Provider 的尝试写 `NULL` 且不带金额，既不产生金额也不计入未知。历史行全部为空，不计入已知或未知尝试数。
- `stats_usage_buckets` 与 `stats_usage_checkpoints` 各新增 12 列，均为 `INTEGER NOT NULL DEFAULT 0`：
  - 全部尝试成本：`cost_custom_nanos`、`cost_usd_unconverted_nanos`、`cost_known_count`、`cost_unknown_count`；
  - 按量尝试成本：`payg_cost_custom_nanos`、`payg_cost_usd_unconverted_nanos`、`payg_cost_known_count`、`payg_cost_unknown_count`；
  - 计费：`billing_custom_nanos`、`billing_usd_unconverted_nanos`、`billing_charged_count`、`billing_unknown_count`。
- `api_keys` 新增 `spend_limit_nanos INTEGER CHECK (spend_limit_nanos IS NULL OR spend_limit_nanos > 0)`；配置表很小，CHECK 的一次校验可以接受。
- `models` 新增可空的 `billing_json`；SQLite 使用 `TEXT` 并约束为合法 JSON 对象，PostgreSQL 使用 `jsonb` 并施加同样的对象约束。
- `provider_models` 新增可空的 `source_metadata_json`，同样只允许 JSON 对象或 SQL `NULL`。
- 新建 `provider_cost_plans`（§5.1）与 `api_key_spend`（§6.4）。

### 7.2 Rust 数据阶段：固定已编辑快照

在 `0030` 之后、`0031` 之前运行，接入 `migrations.rs` 现有的阶段边界：

- 选取 `snapshot_state` 为 `edited` 且 `source` 为对象的行，改写为 `imported`，`pinned` 为全部字段；`edited` 且 `source` 为 `null` 的行保持不变（即 `Local`）。
- 每批最多 100 行，按 `(provider_id, model_id)` 游标推进，每批独立事务，SQLite 写协调随批释放；后批失败保留已提交进度并显式报错，不延长锁超时，不自动重试（`docs/design/architecture.md` §10.1）。
- 改写条件本身排除已转换的行，阶段可重入。`0031` 尚未应用就说明阶段未完成，下次启动会继续。
- 迁移行的 `source_metadata_json` 为空，直到下一次成功同步写入；在此之前这些字段显示为已固定但不能恢复同步。

### 7.3 `0031_cost_and_billing_stats.sql`

删除并重建 `0018_stats_usage.sql` 中的四个来源视图与全部统计触发器（`stats_usage_credit` 及 attempt／turn／run／rejection 的 insert／update／reclaim），使检查点与桶携带新列：

- 只有 `stats_usage_attempt_source` 输出金额与计数，其他来源输出 0。attempt 来源已经分解到 global／model／api_key／provider 四个范围，按所属 Model Turn 的 `started_at` 分桶，金额随之进入同一桶。
- 有自定义金额的行计入 `*_custom_nanos`，否则把美元金额计入 `*_usd_unconverted_nanos`。
- 检查点差额机制不变：尝试行先插入、结束时更新，触发器按新旧检查点之差累加；删除观测明细不回退统计。
- 新增 `api_key_spend` 的累加触发器（§6.4）。

PostgreSQL 以对应的 trigger function 实现同一语义。只有 DDL，不触碰历史数据。

## 8. 管理 API

|接口|变化|
|---|---|
|`GET /api/v1/currency`|返回 `{custom: null}` 或 §3.2 的对象|
|`PUT /api/v1/currency`|`{symbol, usd_rate}`；首次设定执行 §3.2 的换算；没有 DELETE|
|`GET /api/v1/providers/{id}/cost-plans`|生效记录列表，新到旧|
|`POST /api/v1/providers/{id}/cost-plans`|`{basis: "none" \| "usage" \| "subscription", period?, price?: {amount, currency}}`，追加一条；`none` 与 `usage` 不接受 `period`／`price`／`currency`|
|Provider 详情|增加当前成本方式（`none`／`usage`／`subscription`）|
|`PUT /api/v1/providers/{id}/model`|改为 `{model_id, revision, changes}`，`changes` 只含改动字段，出现即固定|
|`POST /api/v1/providers/{id}/model/restore-sync`|`{model_id, revision, fields}`，返回 `ProviderModelDetail`|
|`POST /api/v1/providers/{id}/model/reimport`|删除|
|`ProviderModelDetail`|删除 `can_reimport`；增加 `pinned_fields` 与 `restorable_fields`|
|`POST/PUT /api/v1/models[/{route_id}]`|增加 `billing`|
|`POST/PUT /api/v1/api-keys[/{id}]`|增加 `spend_limit`（十进制字符串或 `null`，展示币种）|
|`GET /api/v1/api-keys[/{id}]`|增加 `spend: {spent, reset_at}` 与 `spend_available`（展示币种下的剩余额度，十进制字符串；`max(0, spend_limit - spent)`，无限额时为 `null`）|
|`POST /api/v1/api-keys/{id}/spend/reset`|清零，返回新的 `spend`|
|`/api/v1/stats/*`|增加金额字段（见下）|
|请求详情|每个尝试增加 `cost` 与 `billing`；`none` Provider 的尝试两者与 `cost_basis` 均为 `null`|

金额字段统一为 `{amount: "12.34", known_count, unknown_count}`，另附 `currency: {kind: "usd" | "custom", symbol}`：

- `overview`：`actual_cost`（不变）；窗口与订阅段重叠时另有 `subscription: SubscriptionValue`。
- `series`：`actual_cost`（不变）。
- `providers`：`cost`（该 Provider 全部受追踪尝试的按量估价：按量尝试取实际成本，订阅尝试取等价按量成本；既无受追踪尝试也无订阅段重叠时为 `null`）与 `subscription`（`SubscriptionValue`，无订阅段重叠时为 `null`）。删除原 `actual_cost` 与 `usage_equivalent_cost`。
- `models`（Route）与 `api-keys`：`cost`（所有尝试的按量估价，包括订阅 Provider 的等价按量成本）与 `billing`；不把 Provider 订阅摊销分配到 Route 或 API Key。

`SubscriptionValue = {cost: 金额, usage_equivalent_cost: 金额, multiple: "1.2345" | null}`：`cost` 是窗口内摊销订阅费用（两个计数恒为 0，摊销溢出时为 `unknown_count: 1`），`usage_equivalent_cost` 是同窗口内订阅基准尝试的等价按量成本，`multiple` 是两者之比的 4 位小数（见 §5.4）。

## 9. WebUI

用户可见文案以英文为规范默认值，同步提供中文；改源文件后重新生成 paraglide 产物。

|位置|文件|变化|
|---|---|---|
|设置|`frontend/stravia-webui/src/routes/settings/+page.svelte`|「自定义币种」：符号与「1 USD = X」；首次保存前确认，说明设定后不能删除、现有消费上限会按此汇率换算|
|共享价格编辑器|`price-editor.svelte`、`price-form.ts`|四项价格、上下文分档、每项的币种切换（仅在设有自定义币种时出现）、「添加峰谷定价」；供 Provider Model 与 Route 共用|
|Provider Model 编辑|`provider-model-editor.svelte`、`provider-model-form.ts`、`provider-model-editing.ts`|字段标签旁显示已固定状态与「恢复同步」；只提交改动字段；删除重新导入菜单与相关文案|
|Provider|`provider-connection-view.svelte`、`routes/providers/[id]/+page.svelte`|成本方式是连接表单内联的 `FormSection`，与「连接」「网络」同级并位于最后，随「保存连接」一起提交；三种方式：不计算／按量／订阅（月付或年付、价格、币种）；显示当前生效记录与变更历史|
|Route|`model-editor.svelte`|计费：不计费／按次（金额与币种）／按 Token（共享价格编辑器）；当前只在 API Key 页面与模型设置页展示，不进入用量统计的 Route 行|
|API Key|`api-key-editor.svelte`、`routes/api-keys/[id]/+page.svelte`|消费上限输入；列表以「可用余额」列替代「模型访问」，详情显示已消费、可用余额、上次清零时间与「清零」按钮（需确认）|
|用量统计|`routes/stats/+page.svelte`|全局：实际成本与订阅价值（订阅费用、等价按量成本、价值倍数）；Provider 行：按量成本、订阅费用、价值倍数（`none` 不显示成本）；Route 与 API Key：按量成本与计费金额。所有金额在 UI 四舍五入到 2 位小数，Core 保留纳精度|
|请求详情|`observation-inspector-content.svelte`|每个尝试显示成本（订阅 Provider 标注为等价按量）与计费；未知显示「未知」|

价格编辑器负责必填项与数字、时间的输入语法；时区、星期、时段顺序与重叠、倍率有效性由 Core 统一校验。保存失败时显示 Core 错误并保留草稿，取消编辑或切换模型后清除对应错误。

订阅摊销增加已知金额，但不增加已知尝试数。即使 `known_count = 0` 且存在未知按量尝试，非零摊销金额仍须正常显示；只有金额为零、没有已知尝试且存在未知尝试时显示「未知」。金额卡片与成本趋势使用同一规则，未知尝试不能被画成已知零成本。

订阅价值倍数直接展示 Core 返回的 4 位小数；`usage_equivalent_cost.unknown_count > 0` 时显示为下界（「≥」）。`multiple` 为 `null`（订阅费用为零或任一侧不可表示）时不显示倍数。金额统一由 WebUI 格式化器四舍五入到 2 位小数，Core 返回的纳精度值不截断。

## 10. 验证计划

- **计价函数单元测试**：选档边界（等于阈值不进档）、零 Token 分项不需要价格、缺价未知、缓存未知按 0、峰谷时段起止边界、跨夏令时的时区、混合币种两侧精确与取整、溢出变未知；价值倍数的 4 位小数取整、分母为零与不可换算时不返回倍数。
- **成本方式测试**：`none` 计划不接受订阅字段；切到 `none` 会结束前一段订阅摊销；`none` Provider 的尝试写入空成本基准与空金额，且不计入未知成本；切回按量或订阅后恢复成本与订阅价值展示。
- **存储行为的 HTTP E2E（SQLite 与 PostgreSQL）**：尝试结束后桶的金额与计数；有无自定义金额分列；`api_key_spend` 累加、清零、已删除 Key 跳过；观测明细删除不回退统计；`provider_cost_plans` 追加与 Provider 删除时补终点。
- **迁移测试**：`edited` 带来源→`imported` 全固定，`edited` 无来源不变，阶段中途失败后重入，`0031` 未应用时阶段会重跑。
- **HTTP E2E**：达到上限后四种协议的 429 错误体，没有 `Retry-After`；调高上限与清零后恢复；MCP `tools/call` 被拒；被拒请求计入错误的请求；首次设定币种后上限换算。
- **WebUI**：价格编辑器峰谷预填与校验、只提交改动字段、恢复同步、币种切换只在设有自定义币种时出现；用 Chromium 实际验证统计页与请求详情的金额展示。

## 11. 实施时同步的文档

- `docs/design/architecture.md`：§8.3 准入流程加入消费上限；§9.2、§9.3 删除 re-import 并改为逐字段固定；§10.2 增加新表与新列。
- `docs/design/vendor-plugins.md` 第 162 与 286 行、`docs/design/media-generation.md` 第 142 行：把「人工编辑的规格整条接管、需显式 re-import」改为逐字段固定与恢复同步。
- 实施完成后把本文状态改为已实施，并补充 `CHANGELOG.md`。

## 12. 风险

- **过载丢失**：观测队列满时丢弃的事件不会补记成本与计费，已消费偏低，消费上限可能被超出（ADR-0085 已接受）。
- **超出上限**：只在根请求准入检查，长 Agent 运行可在一次准入内大幅超出上限。
- **高估**：节假日按工作日计高峰、缓存用量未知按普通输入计，都会高估成本，但不会低估。
- **协议用量语义**：§4.2 依赖 Total Input 包含缓存输入、输出包含推理 Token；实现前须逐协议核对。
- **观测作用域缺口**：不在观测作用域内的模型调用既不计成本也不计费；实现时须审计全部归属 API Key 的调用路径。
- **依赖**：`stravia-core` 新增对 workspace 已有依赖 `chrono-tz` 的引用。
