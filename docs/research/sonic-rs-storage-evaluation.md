# sonic-rs 在 Stravia 存储与查询边界的可行性评估

**决策：已取消 sonic-rs 集成，继续使用 serde_json。** 本文保留研究结果与当时考虑过的试点条件；下文 Phase/下一步不是当前执行计划，不因上游发布修复而自动恢复集成。当前方案以[数据结构与存储性能设计](../design/data-layout-performance.md)为准。

日期：2026-10-28。评估基线：`sonic-rs 0.5.10`（已发布）与上游 `main f63d185`（未发布）。仓库工作树 `9d467d31`；没有改产品代码、数据库、锁文件或现有用户文件。本专项新增本文，总体设计另见[数据结构与存储性能方案](../design/data-layout-performance.md)。引用的“父任务 smoke”是隔离合成探针的实测结果，不是生产负载或端到端结论。下文版本行为及限制按当时证据保留，不构成采纳建议。

**结论先行：**

1. **0.5.10 不能替换 `serde_json` 作为 Stravia 的通用 JSON 后端。** 当前 workspace 在 `stravia-runtime-contract`、`stravia-core` 和多个 vendor crate 启用了 `serde_json/arbitrary_precision`；该特性用私有 token 传递任意精度数字。`sonic-rs` 只识别自己的 `$sonic_rs::private::JsonNumber`，不识别 serde_json 的 token。结果是：`sonic_rs::from_str::<serde_json::Value>` 的大整数退化为 `f64`，`1e400` 直接报错；`serde_json::RawValue` 字段在 sonic 下没有匹配的 visitor，按源码推断会报类型错误（尚待 smoke 固定）。反向 `serde_json::Value` 经 sonic 序列化会把每个数字写成 `{"$serde_json::private::Number":"..."}` 对象。父任务 smoke 已实测，甚至在 sonic 开启 `arbitrary_precision + sort_keys` 后，桥接到 `serde_json::Value` 仍然丢精度；真实 `ContentBlock ToolUse.input` 同样丢精度。**不能把 bridge 描述为大数无损，也不能笼统推荐 `from_slice::<现有 IR>` 直换。**
2. **0.5.10 自身的 `Value` 可以保留 raw 数字**，但这是 sonic 私有 DOM 的能力，不是跨 serde_json 的互操作能力。sonic `Value` 的共享对象保留输入顺序与重复 key，`Object::get` 返回第一个；一旦 `to_mut`/物化为 owned 对象，重复 key 后者覆盖、顺序丢失，默认 `AHashMap` 的序列化顺序不稳定。`sort_keys` 只排序，不去重。
3. **COW clone 的收益和陷阱都被父任务实测到**：约 1 MiB 合成正文，9 个 serde_json clone 存活约 9.45 MB，9 个 sonic `Value` clone 只约 1.05 MB；但只 clone 一个 tiny 子树并 drop 根，仍保留整份 1,049,228 B。sonic clone 适合短生命周期快照，不适合长期缓存小字段。
4. **可试点面很窄**：已解压 observation payload 的只读 `get_many`/`LazyValue` 提取；不改变持久化字节、不进入 IR、不做 digest、不双 DOM。`OwnedLazyValue` 在 0.5.10 有未解析视图 panic，必须 pin main 修复或绕开。
5. **不作为 canonical bytes 写入器**：sonic `Value`/`Object` 没有“规范 JSON”保证；当前如果直接序列化，顺序、重复 key、数字文本、escape 都可能与 `serde_json` 字节不同。任何持久化/digest/ID 变更都必须独立版本化并做字节级 golden，不得静默重映射。
6. **Wasm 暂不采用**：0.5.7 起声明支持 Wasm，但上游 CI 只验证 `wasm32-wasip1`；Stravia vendor 使用 `wasm32-wasip2`，且现有 [vendor JSON 边界](../../docs/design/vendor-plugins.md#L406-L412) 明确保留 serde_json。sonic 在当前仓库也没有 `target-cpu`/`target-feature` 构建配置，默认 x86_64 构建不会自动获得 README benchmark 的 SIMD 前提。

## 1. 版本、来源与发布/主干分栏

### 1.1 已发布基线

| 项目 | 值 | 一手来源 |
|---|---|---|
| crates.io 最高稳定版 | `0.5.10` | [crates.io/sonic-rs/0.5.10](https://crates.io/crates/sonic-rs/0.5.10) |
| crates.io 发布时间 | `2026-09-11T08:34:33.860840Z` | [crates.io API](https://crates.io/api/v1/crates/sonic-rs) |
| GitHub release | `v0.5.10`，`2026-09-11T08:36:03Z` | [releases/tag/v0.5.10](https://github.com/cloudwego/sonic-rs/releases/tag/v0.5.10) |
| tag commit | `7eca25c81884264043eb5434c1fa3fcba49600b1` | [tag v0.5.10](https://github.com/cloudwego/sonic-rs/tree/v0.5.10) |
| docs.rs | latest 为 `0.5.10` | [docs.rs/sonic-rs/0.5.10](https://docs.rs/sonic-rs/0.5.10/sonic_rs/) |
| 许可证 / edition | Apache-2.0 / 2021 | [Cargo.toml](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/Cargo.toml) |

`0.5.9` 在同日 `08:28:22Z` 发布，`0.5.10` 约 6 分钟后发布；发布说明显示 0.5.10 是修复 `StreamDeserializer` 尾随错误的小版本。发布间隔不是成熟度证据，实际适用性以下面的源码与兼容验证为准。

### 1.2 未发布主干（只作为“下一版预期”，不计入当前能力）

`main` HEAD 为 `f63d1851de596fb9c53bf2a67ec84828c1d65c3c`，提交时间 `2026-10-11T14:04:37+08:00`，版本号仍是 `0.5.10`，tag 之后有 8 个提交。相关修复：

| 提交 | 影响 | 与本文的关系 |
|---|---|---|
| [`f7bdde1`](https://github.com/cloudwego/sonic-rs/commit/f7bdde1) | 给 `Value` 反序列化加深度限制 | 0.5.10 解析深层不可信 JSON 可栈溢出；主修复未发布 |
| [`444063c`](https://github.com/cloudwego/sonic-rs/commit/444063c) | 修复 `OwnedLazyValue::as_array/as_object` 在未解析值上的 panic | 直接影响 `OwnedLazyValue` 试点 |
| [`443f111`](https://github.com/cloudwego/sonic-rs/commit/443f111) | 修复 `serialize_char` 越界读 4 字节缓冲，避免非法 UTF-8/UB | 0.5.10 仍含该缺陷；typed serializer 的 unsafe 风险 |
| [`9de5a49`](https://github.com/cloudwego/sonic-rs/commit/9de5a49) | 修复 `utf8_lossy` 下 `Value` end offset | 只用严格模式可降低影响 |
| [`c34a535`](https://github.com/cloudwego/sonic-rs/commit/c34a535) | 修复 `Entry::key` 对 occupied entry 的返回 | 与本评估的低层 API 无关 |
| [`cd1e5ab`](https://github.com/cloudwego/sonic-rs/commit/cd1e5ab) | 从 sequence 反序列化 struct variant | typed serde 边界 |
| [`8d5110c`](https://github.com/cloudwego/sonic-rs/commit/8d5110c) | fuzz 混合深嵌套生成边界 | 深度问题 |
| [`f63d185`](https://github.com/cloudwego/sonic-rs/commit/f63d1851de596fb9c53bf2a67ec84828c1d65c3c) | `StreamDeserializer` 最后一个值后不报错 | 流式消费者 |

来源：[`v0.5.10...main` 对比](https://github.com/cloudwego/sonic-rs/compare/v0.5.10...main)。docs.rs 没有 main 的文档；下文的 main 行为均指源码，不指已发布 API。

### 1.3 特性存在性：按 tag 抽查，不把旧能力当 0.5 新特性

“首次观察到”只表示在该 tag 已存在，不声称是首次引入；要精确到引入 commit 需逐提交考古，本文不把抽查包装成考古结论。

| 能力 | 最早抽查到 | 证据 |
|---|---|---|
| `Value` 公开 | `0.1.1` | [0.1.1 lib.rs](https://github.com/cloudwego/sonic-rs/blob/0.1.1/src/lib.rs) 中 `pub mod value; pub use value::*;` |
| `Shared` Arc DOM / COW 结构 | `0.3.0` | [0.3.0 node.rs](https://github.com/cloudwego/sonic-rs/blob/0.3.0/src/value/node.rs) 已有 `Shared`、`ROOT_NODE`、自定义 `Clone` |
| `get`、`get_many`、`LazyValue`、`OwnedLazyValue`、`ArrayJsonIter`、`ObjectJsonIter`、`RawNumber` | `0.3.0` | [0.3.0 lib.rs 导出](https://github.com/cloudwego/sonic-rs/blob/0.3.0/src/lib.rs#L30-L43) |
| `LazyArray`、`LazyObject`、`StreamDeserializer` | `0.4.0` | [0.4.0 lib.rs](https://github.com/cloudwego/sonic-rs/blob/0.4.0/src/lib.rs) 导出对比 |
| `sanitize` feature | `0.4.0` | [0.4.0 Cargo.toml](https://github.com/cloudwego/sonic-rs/blob/0.4.0/Cargo.toml) |
| `arbitrary_precision` feature | `0.3.17` | [0.3.17 Cargo.toml](https://github.com/cloudwego/sonic-rs/blob/0.3.17/Cargo.toml) |
| `non_trailing_zero` | `0.5.3` | [0.5.3 Cargo.toml](https://github.com/cloudwego/sonic-rs/blob/0.5.3/Cargo.toml) |
| `sort_keys`、`avx512` | `0.5.6` | [0.5.6 release](https://github.com/cloudwego/sonic-rs/releases/tag/v0.5.6)；[Cargo.toml](https://github.com/cloudwego/sonic-rs/blob/v0.5.6/Cargo.toml) |
| `utf8_lossy` | `0.5.6`（0.5.5 无） | [0.5.6 Cargo.toml](https://github.com/cloudwego/sonic-rs/blob/v0.5.6/Cargo.toml) 与 [0.5.5](https://github.com/cloudwego/sonic-rs/blob/v0.5.5/Cargo.toml) 对比 |
| Wasm 支持声明 | `0.5.7` | [0.5.7 release](https://github.com/cloudwego/sonic-rs/releases/tag/v0.5.7) |

因此，`get`/`get_many`/`LazyValue`/COW **不是** 0.5 新特性；评估其风险不能只看 0.5.10 的 release note。

## 2. `Value` 的内存、所有权与并发模型

### 2.1 布局：16 字节句柄 + 共享文档

`Value` 的定义是两个字段：`Meta`（8 字节，含类型标记和长度/索引）加 `Data`（8 字节 union，可能是内联数字、指针或容器索引），64 位下共 16 B。源码：[node.rs `pub struct Value`](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/value/node.rs#L141-L151)。

根 `Value` 关联一个 `Arc<Shared>`：`Shared` 保存完整输入 JSON 的 `Vec<u8>`（尾部有 padding）和 `Bump` arena；节点/容器头也在该 arena。源码：[shared.rs `Shared`](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/value/shared.rs#L9-L23)。因此：

- 解析后的 `Value` 不是一棵独立节点树，而是“句柄 + 整篇文档 + arena”的视图。
- 根或共享子节点的 `Clone` 是 O(1) Arc 引用计数；源码见 [`Value::clone`](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/value/node.rs#L725-L737)。docs.rs 的注释说 clone 会“new allocator”，与实现不符；以源码为准。
- 修改共享容器时 `to_mut` 做 COW：`Arc::make_mut` 复制容器（以及容器内的 16 B 句柄数组），嵌套子容器仍可共享；源码：[`to_mut`](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/value/node.rs#L615-L646)。
- **clone 一个子字段仍保留整份 `Shared`**：只要返回值里还有该根/子句柄，整篇输入 JSON 和 arena 就不会释放。父任务 smoke 的 `sonic_tiny_keeps=1049228` 正是这个现象。

### 2.2 解析时的复制与 staging 预留

`parse_with_padding` 把输入复制到 `Vec::with_capacity(json.len() + PADDING_SIZE)`，再追加 `b"x"` 和 61 个 0，作为 SIMD 可越界读的 padding；该 buffer 被 `Shared` 长期持有。源码：[node.rs `parse_with_padding`](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/value/node.rs#L1460-L1489)。

节点 staging 使用 thread-local `NodeBuf`，容量为 `json_len / 2 + 2` 个 `Value`；按 16 B/个计算，**单是解析期预留上限就约 8×输入字节**，尚未计最终 DOM、容器头和 arena。源码：[`NodeBuf::with_capacity`/`max_len`](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/value/node.rs#L1511-L1521)、[`TlsBuf`](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/value/node.rs#L15-L45)。这是**峰值/分配器预留**，不是最终存活；staging 在 parse 后释放或留在 thread-local 容量中，不能从下表推断峰值 RSS。

父任务内存实测的最终存活基线：

| 1 MiB 级合成正文（父任务 smoke，release，tracking allocator） | 存活字节 |
|---|---|
| serde_json 1 份 | 1,049,852 |
| serde_json 9 份 clone | 9,448,924 |
| sonic `Value` 1 份 | 1,049,228 |
| sonic `Value` 9 份 clone | 1,049,356 |
| sonic 只留 tiny 子树 clone、drop 根 | 1,049,228 |

**解读：** sonic 的 clone 共享整篇文档，所以 9 份克隆几乎不增长；代价是 tiny 子树仍钉住整篇 1 MiB。serde_json 的 clone 是深拷贝，9 份接近 9×。这不是“sonic 更省内存”的无条件结论，而是“共享换保留”的权衡。父任务 smoke 是单机、合成数据、release，不能外推生产 RSS。

### 2.3 对象：共享 DOM 是 pair 数组；owned 才是 map

解析后的共享 `Object` 是交替 key/value 的 `Value` 数组（`obj_pairs`），保持输入顺序；`get_key_value` 线性扫描，O(n)，不是哈希。源码：[`Object` 文档](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/value/object.rs#L13-L20)、[`get_key_value`](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/value/object.rs#L141-L166)、[`get_key_value` 热路径](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/value/node.rs#L1311-L1336)。

`to_mut` 后默认物化为 `AHashMap<FastStr, Value>`；`sort_keys` 时物化为 `BTreeMap`。源码：[object.rs `Object` 类型](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/value/object.rs#L64-L82)、[node.rs `to_mut` map 分支](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/value/node.rs#L690-L724)。这意味着：

- 共享 DOM：顺序稳定、重复 key 全保留；`get` 找第一个；序列化按 pair 顺序输出。
- owned DOM：重复 key 后者覆盖；默认 `AHashMap` 迭代顺序不确定；`sort_keys` 才变 BTreeMap 有序。
- 不要把“sonic 保留顺序/重复”与“sonic 是 canonical JSON”混为一谈；两种状态行为不同。

### 2.4 并发与 unsafe

`Value` 手动实现 `Send`/`Sync`（[node.rs `unsafe impl`](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/value/node.rs#L750-L755)）；`Shared` 内部有 `bumpalo::Bump`，本身不是 `Sync`，代码依赖“解析后只读 + Arc 原子计数”的不变量，用 unsafe 声明。不能据此断言已证明 sound；只能说明上游以此方式暴露安全 API。0.5.10 还已知 `serialize_char` 越界读（main 修复），说明该 unsafe 面并非无风险。

父任务 smoke 的跨线程只读 clone/查询通过；这不等于并发写、`to_mut` 竞争、`OwnedLazyValue` 首次 parse 竞争都已验证。`OwnedLazyValue` 的懒解析缓存是 `AtomicPtr<Parsed>`，首次并发 parse 的语义需要单独 smoke/Miri。

## 3. Lazy/raw/iterator 与 typed serde

### 3.1 `get` / `get_many`

- `get` 沿路径逐层扫描；安全版会验证路径、并对已消费前缀做 UTF-8 检查，缺失返回 `Err`（`is_not_found`），类型不匹配返回 `is_unmatched_type`。源码：[lazyvalue/get.rs `get`](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/lazyvalue/get.rs#L398-L452)。
- `get_many` 用 `PointerTree` 一次扫描返回 `Vec<Option<LazyValue>>`，缺失是 `None`；路径重复允许，输出按添加顺序。源码：[get.rs `get_many`](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/lazyvalue/get.rs#L453-L536)、[PointerTree](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/pointer/tree.rs#L55-L103)。
- `PointerTree` 的每一层是 `HashMap<FastStr, PointerTreeNode>` 或 `HashMap<usize, ...>`，所以 `get_many` 的对象匹配平均 O(1)，而单次 `get` 需要每次重新扫描；源码：[tree.rs `MultiKey`](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/pointer/tree.rs#L135-L137)。
- 复杂度结论：N 个路径、文档 M 字节，`N` 次 `get` 最坏 O(N·M)；`get_many` 一次遍历 O(M + 匹配成本)，但 `PointerTree` 构建/内存 O(N)。适合固定 schema 的多字段提取，不适合一次性随机 key。
- **`get`/`get_many` 不是全文校验器**：它们只解析到目标路径，未调用 `parse_trailing`；源码见 [`get`](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/lazyvalue/get.rs#L398-L417) 与 [`parse_trailing`](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/parser.rs#L1603-L1620)。父任务 smoke 也观察到尾随垃圾可通过。存储入口必须先用能检查 trailing 的全文验证路径（例如 `from_slice` 解析为忽略类型，或现有 serde_json），再用 `get` 提取；不能把 `get` 的 `Ok` 当成“整篇合法”。

### 3.2 `LazyValue` / `OwnedLazyValue`

- `LazyValue<'a>` 借用原始 JSON 的 `&str`/`&[u8]`，只记录区间；字符串 `as_str` 才做 unescape，未转义缓存用 `Arc`/原子指针共享。源码：[lazyvalue/value.rs](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/lazyvalue/value.rs#L121-L150)、[`as_raw_str`](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/lazyvalue/value.rs#L351-L360)、[`Clone`](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/lazyvalue/value.rs#L368-L390)。
- `OwnedLazyValue` 把子区间复制成自己的 `FastStr`（若源已是共享 `FastStr` 可复用），因此可脱离整篇文档；转换不复制整篇。源码：[owned.rs `From<LazyValue>`](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/lazyvalue/owned.rs#L579-L620)。
- `OwnedLazyValue::clone` 必须按状态看：未解析时执行 `FastStr::clone` 并建立独立懒解析指针，不等于重新复制全部 raw 字节；已解析时克隆 `Parsed`，数组/对象的 Vec 容器递归克隆，字符串缓冲仍按 FastStr 的共享规则处理。不能把它概括成始终 O(1) 或始终复制全部正文。源码：[clone_lazyraw](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/lazyvalue/owned.rs#L250-L269)、[LazyPacked/Parsed](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/lazyvalue/owned.rs#L277-L310)。
- 0.5.10 的 `as_array`/`as_object` 对未解析 raw 会 panic（索引 `Parsed::Unparsed`）；main `444063c` 修复。源码：[owned.rs `Deref`](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/lazyvalue/owned.rs#L640-L667)。如果试点需要数组/对象视图，必须 pin main 或显式先 `as_array_mut`/`as_object_mut` 物化；不能把 0.5.10 当作可用。
- 这四种 clone 成本必须分开说：`Value` O(1) 但保留整篇；`LazyValue` 借用、clone 便宜但生命周期绑输入；`OwnedLazyValue` 复制子串、可脱离；`serde_json::Value` 深拷贝。不能用一句“sonic clone 很便宜”覆盖。

### 3.3 迭代器与 raw JSON

`ArrayJsonIter`/`ObjectJsonIter` 惰性逐项解析 raw JSON，产出 `Result<LazyValue>`；遇到非法输入会在迭代中途报错。源码：[iterator.rs](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/lazyvalue/iterator.rs#L1-L80)。`LazyArray`/`LazyObject`（0.4.0）提供按索引/键的惰性视图；`LazyObject::get` 仍是线性扫描。

`LazyValue` 的 `Serialize` 是 sonic 私有 raw 透传，只有 sonic Serializer 认识；交给 serde_json/axum `Json` 会变成 magic object（见 §5）。因此：
- 可用：内部只读提取、诊断、把 raw 子串交给另一个明确解析器。
- 不可用：直接塞进现有 `serde_json` 响应体并期望原样 JSON。

### 3.4 typed serde 的边界

`sonic_rs::from_str::<T>`/`to_string(&T)` 对纯 serde 类型可以工作；对包含 `serde_json::Value`、`serde_json::Number`、`serde_json::RawValue` 的 IR 不成立（§5）。`from_str` 的 `use_rawnumber()` 只配置 sonic 自己的 `DocumentVisitor`；普通 typed 数字字段仍走 `deserialize_any`/`parse_number`。源码：[serde/de.rs `use_rawnumber`](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/serde/de.rs#L58-L60)、[`deserialize_value`](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/serde/de.rs#L370-L390)、[`deserialize_any`](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/serde/de.rs#L454-L570)。

`sonic_rs::Value` 自身在开启 `arbitrary_precision` 时把数字存成 `RawNum`，原样序列化；源码：[de.rs `visit_raw_number`](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/serde/de.rs#L1296-L1308)、[rawnumber.rs](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/serde/rawnumber.rs#L1-L80)。这是 sonic 私有能力，不等于 serde_json 字段也保留。

## 4. 语义矩阵：重复 key、顺序、null/missing、数字、UTF-8

下表把“解析后的共享 DOM”“物化后的 owned DOM”“serde_json 默认”分开；这是本次评估最重要的行为边界。父任务 smoke 使用 0.5.10，`precision=true, sorted=true` 的结果也列入。

| 语义点 | sonic 共享 `Value`（刚解析） | sonic owned（`to_mut`/`Object`） | `serde_json::Value`（默认，无 preserve_order） | 父任务 smoke |
|---|---|---|---|---|
| 重复 key `{"a":1,"a":2}` | 两个 pair 都保留；序列化 `{"a":1,"a":2}`；`get("a")` 第一个 = 1 | `AHashMap::insert` 后者覆盖 = 2；顺序丢失 | `BTreeMap::insert` 后者覆盖 = 2 | `serde={"a":2}`，`sonic={"a":1,"a":2}`，`get=1`；`sort_keys` 仍保留重复 |
| 对象顺序 `{"z":0,"a":1}` | 源序 `z,a` | 默认 AHashMap 顺序不确定；`sort_keys` 才 `a,z` | 默认 BTreeMap 排序 `a,z` | `sorted=true` 时 `sonic={"a":1,"z":0}`；默认源序 |
| 显式 `null` vs 缺失 | `get` 返回 `Some(Null)` vs `None`；`pointer` 也是 `Option` | 同左 | `Value::get` 同样 `Option`；`Index` 返回 `Null` 两者不可分 | null/missing 通过 |
| `1e400` | 默认 `FloatMustBeFinite` 错误；`arbitrary_precision` 保留 raw | raw 保留 | `arbitrary_precision` 保留 raw；普通 `f64` 模式拒绝 infinity | 默认 `ERR`；sonic precision `{"n":1e400}`；桥接 `ERR` |
| 超过 u64/i64 的整数 | 默认退化 `f64`；`arbitrary_precision` 保留 raw | raw 保留 | `arbitrary_precision` 保留原始十进制 | `18446744073709551616` → `1.8446744073709552e+19`；precision 保留 |
| `1.2300` / `-0.0` | 默认 shortest/规范化，`-0.0`→`0.0`；`non_trailing_zero` 控制尾零；precision 保留文本 | raw 保留 | `arbitrary_precision` 保留文本；普通模式 shortest | 默认 `1.23/0.0`；precision `1.2300/-0.0`；桥接 `1.23/0.0` |
| 非法 UTF-8 / 尾随垃圾 | `from_slice` 做全文 UTF-8+尾随校验；`get/get_many` 只检查已消费前缀，尾随垃圾可通过；`_unchecked` 跳过 UTF-8 | 取决于来源 | 严格 UTF-8；尾随垃圾报错 | 三个非法输入拒绝；尾随垃圾通过（`get` 探针） |
| 深度嵌套 | 0.5.10 `Value` DOM 无深度限制，深嵌套可栈溢出；main 加限制 | 同左 | 有递归限制/`unbounded_depth` 可选 | 未测 |

### 4.1 重复 key 的三种“正确”答案

- **原始 JSON 语义**：RFC 8259 不要求去重；重复 key 是合法文本但互操作语义未定义。sonic 共享 DOM 保留全部，是“文本保真”而非“对象语义”。
- **serde_json 语义**：默认 `Map` 是 `BTreeMap`，`insert` 后者覆盖；因此默认解析即去重+排序。源码：[serde_json map 默认 BTreeMap](https://github.com/serde-rs/json/blob/v1.0.154/src/map.rs#L19-L24)。
- **Stravia 现有代码**：大量依赖 `serde_json::Value`；不能假设“默认保留重复”。一旦 sonic 在某个边界保留重复，下游 `Object`/`Value` 的 `get` 取第一个还是最后一个、序列化是否输出两个，都会改变语义。**任何会进入持久化/身份计算的 JSON 都必须先明确采用哪种策略，并在 golden corpus 固定。**

### 4.2 顺序与 canonical

sonic 共享 DOM 保留源序不是 canonical：不同客户端发同一对象的不同 key 顺序，序列化字节不同。sonic owned 默认 AHashMap 顺序不稳定，`sort_keys` 才排序；但 `sort_keys` 只影响序列化/owned map，不会把共享对象里的重复 key 去掉。**因此 `sonic_rs::to_vec(&Value)` 不是 canonical JSON writer**；不能把它的输出直接当 digest 输入，除非额外定义并测试规范化规则。

### 4.3 数字与文本精度

sonic 默认的 `parse_number` 对整数只在 `i64`/`u64` 范围内精确；超出后转 `f64`。源码：[parser.rs `parse_number`](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/parser.rs#L1987-L2035)、[`FloatMustBeFinite`](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/parser.rs#L398-L410)。`arbitrary_precision` 下 sonic `Value` 存 `RawNum`，可保留任意长度和 `1e400`；但 typed `serde_json::Value` 字段不走这条路径。

`serde_json` 在 `arbitrary_precision` 下把数字存成字符串，并在序列化时通过私有 token 还原；这意味着 **同一份 `serde_json::Value` 的数字文本可能被保留**（父任务 smoke：`1.2300`、`-0.0`、`1e400` 都保留）。这既是 Stravia 现有语义，也是 sonic 跨序列化器失败的原因（§5）。

### 4.4 escape、UTF-8 与非法输入

- 安全入口 `from_str` 假设输入已是 `&str`（Rust 已保证 UTF-8）；`from_slice` 会做最终 UTF-8 校验；`from_slice_unchecked` 只跳过 UTF-8 校验，`get_unchecked`/`get_many_unchecked` 则假定 JSON 合法、可能返回意外结果；**均不可用于存储/网络不可信输入**。源码：[serde/de.rs `from_slice`](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/serde/de.rs#L1318-L1334)、[`from_slice_unchecked`](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/serde/de.rs#L1336-L1348)、[get.rs 安全说明](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/lazyvalue/get.rs#L50-L80)。
- `utf8_lossy` 会把非法 UTF-8 替换为 U+FFFD；这不是“容错解析”而是语义改变。0.5.10 的 end offset 还有 main 修复；存储路径应保持严格模式。
- `LazyValue::as_raw_str` 保留原始 escape 文本；`as_str` 才 unescape。若直接把 raw 写入另一个 JSON 文档，必须确认它是合法 JSON 值；若是字符串片段，不能当已转义内容拼接。
- 0.5.10 的 `serialize_char` 越界读 4 字节缓冲（main `443f111`），可能输出非法 UTF-8。任何 typed `char` 字段都不要在 0.5.10 上走 sonic serializer。

## 5. 关键阻断：`serde_json/arbitrary_precision` 与 sonic 私有 token 不兼容

### 5.1 两边都用“私有 struct token”传递数字

`serde_json` 1.0.154（仓库锁定版本）在 `arbitrary_precision` 下：

- `Number` 内部是 `String`；`Serialize` 无条件走 `serialize_struct("$serde_json::private::Number", 1)`，字段名是 `"$serde_json::private::Number"`。源码：[number.rs `Serialize`](https://github.com/serde-rs/json/blob/v1.0.154/src/number.rs#L385-L396)。
- 反序列化时，`Value` 的 `visit_map` 识别这个 magic key；`Number` 的 visitor 也实现 `visit_map` 读取它。源码：[value/de.rs](https://github.com/serde-rs/json/blob/v1.0.154/src/value/de.rs#L120-L130)、[number.rs `Deserialize`](https://github.com/serde-rs/json/blob/v1.0.154/src/number.rs#L398-L458)。
- `RawValue` 使用 `$serde_json::private::RawValue` token；源码：[raw.rs](https://github.com/serde-rs/json/blob/v1.0.154/src/raw.rs#L342-L352)。

`sonic-rs` 只识别自己的：

- `$sonic_rs::private::JsonNumber`（`RawNumber` 序列化/反序列化）；源码：[serde/ser.rs `serialize_struct`](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/serde/ser.rs#L405-L411)、[value/ser.rs](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/value/ser.rs#L279-L286)、[de.rs token 匹配](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/serde/de.rs#L732-L747)。
- `$sonic_rs::LazyValue`、`$sonic::OwnedLv` 等自己的 token。
- 没有 serde_json 的 token 分支（源码无匹配；docs.rs 的 `Value` trait list 也没有 `From<serde_json::Value>`）。

这不是“两个 feature 名字相同所以兼容”，而是两套私有 wire protocol 互不识别。

### 5.2 四个方向的实测/源码后果

| 方向 | 结果 | 证据 |
|---|---|---|
| `sonic_rs::from_str::<serde_json::Value>` 大整数 | sonic 的 `deserialize_any` 走 `u64`→`f64`，serde_json visitor 得到 `f64`，精度丢失 | 父任务 smoke：`18446744073709551616` → `1.8446744073709552e+19`；`-9223372036854775809` → `-9.223372036854776e+18` |
| `sonic_rs::from_str::<serde_json::Value>` 的 `1e400` | `deserialize_any` 路径直接 `FloatMustBeFinite` 错误，无法到达 serde_json 的任意精度 visitor | 父任务 smoke：`bridge=ERR`；[parser.rs](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/parser.rs#L398-L410) |
| `serde_json::RawValue` 字段经 sonic 反序列化 | sonic 的 `deserialize_newtype_struct` 不识别 serde_json token；serde_json 的 `ReferenceVisitor` 只实现 `visit_map`，默认 `visit_newtype_struct` 报类型错误（源码推断，需 smoke） | [sonic de.rs 732–747](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/serde/de.rs#L732-L747)；[serde_json raw.rs 342–403](https://github.com/serde-rs/json/blob/v1.0.154/src/raw.rs#L342-L403) |
| `sonic_rs::to_string(&serde_json::Value)` / `sonic_rs::to_value(&serde_json::Value)` | serde_json `Number` 发出 magic struct；sonic Serializer 不认识，按普通 map 输出 `{"$serde_json::private::Number":"..."}`；每个数字都中招 | [serde_json number.rs 385–396](https://github.com/serde-rs/json/blob/v1.0.154/src/number.rs#L385-L396)；[sonic ser.rs 405–411](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/serde/ser.rs#L405-L411)；sonic 无该 token 分支 |
| `serde_json::to_string(&sonic_rs::Value)` / `to_value` | sonic `RawNum` 发出 `$sonic_rs::private::JsonNumber`；serde_json 不认识，同样变成 magic object；`LazyValue` 也类似 | [sonic value/ser.rs 279–286](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/value/ser.rs#L279-L286)；[serde_json ser.rs](https://github.com/serde-rs/json/blob/v1.0.154/src/ser.rs) 只匹配自己的 token |

父任务 smoke 的关键结果：

```text
# 默认 features precision=false sorted=false
compat {"n":18446744073709551616}
  serde={"n":18446744073709551616}
  sonic={"n":1.8446744073709552e+19}
  bridge={"n":1.8446744073709552e+19}

# precision=true sorted=true
compat {"n":18446744073709551616}
  serde={"n":18446744073709551616}
  sonic={"n":18446744073709551616}     # sonic::Value 自身保住了
  bridge={"n":1.8446744073709552e+19}  # 进入 serde_json::Value 仍丢
```

`typed_arbitrary_precision` 和真实 `ContentBlock ToolUse.input` 的 smoke 也都是 `1.8446744073709552e+19`。这说明：**sonic 的 `arbitrary_precision` 只能保护 sonic 自己的 `Value`/`RawNumber`，不能保护任何 `serde_json::Value` 字段。**

### 5.3 为什么现有 IR 特别危险

Stravia 的协议 IR 里存在 `serde_json::Value`/`serde_json::Number` 字段，且 `stravia-runtime-contract`、`stravia-core` 等启用了 `arbitrary_precision`：

- [`backend/crates/stravia-runtime-contract/Cargo.toml:20`](../../backend/crates/stravia-runtime-contract/Cargo.toml#L20) `serde_json = { workspace = true, features = ["arbitrary_precision"] }`
- [`backend/crates/stravia-core/Cargo.toml:40`](../../backend/crates/stravia-core/Cargo.toml#L40) 同样启用；workspace 根 `serde_json = "1"`。
- 代码里已有为非 JSON 序列化器适配 serde_json 任意精度数字的先例：[`connect_client_apply.rs:986`](../../backend/crates/stravia-core/src/connect_client_apply.rs#L986-L1015)（YAML 会看到带私有 tag 的 struct）。

因此：

- `sonic_rs::from_slice::<AiRequest>` 看似能编译，但所有 `serde_json::Value` 字段的数字都可能退化/报错；`1e400`、超大整数、`RawValue` 字段都是雷。
- `sonic_rs::to_string(&AiRequest)` 会把 `serde_json::Value` 中的每个数字写成 magic object；这不是“格式略有差异”，而是协议语义破坏。
- 即使给 `serde_json::Value` 字段加 `#[serde(with = ...)]`，也需要逐字段 adapter；不能靠 feature 名字解决。
- `sonic_rs::Value` 与 `serde_json::Value` 没有 `From`/`TryFrom`；docs.rs `Value` trait list 只有 `From<&[Pair]>`/`From<Object>` 等，没有 serde_json。所谓“互转”只能逐节点遍历重建，O(n) 时间 + 额外 DOM/分配；或者先 `to_string` 再重新 parse（同样 O(n)，还会改变数字/顺序/重复 key）。
- **结论：不要把“sonic 解析更快”当成“sonic 可以接现有 IR”。在 IR 仍含 serde_json 类型时，typed round-trip 是禁用面，不是待优化面。**

### 5.4 可行的桥接方式（仅迁移/边缘，不是热路径）

如果未来确实需要跨两个 DOM：

1. **先用拥有方序列化，再用另一方解析**：`serde_json::to_vec(&serde_json_value)` → `sonic_rs::from_slice::<sonic_rs::Value>`；或反向。必须接受 O(n)、额外分配，以及数字文本/顺序/重复 key 的语义变化。
2. **写显式 lossless adapter**：遍历 `serde_json::Value`，对 `Number` 用 `sonic_rs::RawNumber::from_str`（或 `sonic_rs::Value::from` 的精确分支）构造 sonic 节点；反向遍历 sonic `RawNum` 用 `serde_json::Number::from_str`。这仍 O(n)，但可控制大整数/`1e400`/`-0.0`；需要自己的测试矩阵。
3. **不要把 serde 私有 token 当桥**：任何依赖 `serde_json::Value` 的 `Serialize` 直接喂给 sonic（或反向）的方案，都会在 `arbitrary_precision` 下坏掉；这不是稳定 API。

现有 ADR/设计也倾向单一 JSON 后端：[`docs/design/vendor-plugins.md:406-412`](../../docs/design/vendor-plugins.md#L406-L412) 明确“serde_json 是 vendor JSON backend，避免把 canonical payload 解码成第二套 native IR”。sonic 若要进入，必须先改这条设计约束，而不是在代码里偷偷多一个 DOM。

## 6. 存储、canonical bytes 与摘要绑定

### 6.1 当前存储编码

`backend/crates/stravia-core/src/storage_codec.rs` 是 v1：zstd/raw 二选一，加 14 B trailer（版本/标志/原始长度），**没有 digest、没有 dedup**；源码：[`storage_codec.rs`](../../backend/crates/stravia-core/src/storage_codec.rs#L5-L20)、[`decode`](../../backend/crates/stravia-core/src/storage_codec.rs#L85-L140)。Observation 事件的写入是 `serde_json::to_vec(event.payload)` 后 `storage_codec::encode`，读取是 `decode` 后 `serde_json::from_slice`；源码：[`store.rs:2470-2477`](../../backend/crates/stravia-core/src/interaction_observation/store.rs#L2470-L2477)、[`store.rs:2852`](../../backend/crates/stravia-core/src/interaction_observation/store.rs#L2852)。payload 列在 SQLite 是 BLOB、PostgreSQL 是 BYTEA，后端中立。

### 6.2 当前字节不是“随便一个 JSON 序列化器”都能复现

在没有显式 `preserve_order` 的情况下，`serde_json` 的 `Map` 是 `BTreeMap`，对象 key 排序；源码：[map.rs](https://github.com/serde-rs/json/blob/v1.0.154/src/map.rs#L19-L24)。本仓库直接 manifest 未启用 `preserve_order`，但 serde_json 的 feature 可被传递依赖统一开启；改字节路径前必须用 `cargo tree -e features` 确认。若未开启，当前 `serde_json::to_vec(&Value)` 对对象是**按键排序**的确定性输出（数字文本/escape 仍取输入规范形态）。但：

- typed struct 的字段顺序是声明顺序，不是 BTreeMap；不同结构体的字段重排会改字节。
- `arbitrary_precision` 会保留输入数字文本（`1.2300`、`-0.0`、`1e400`），普通模式会规范化；同一语义可能不同字节。
- `serde_json::Value` 默认重复 key 后者覆盖；sonic 共享 DOM 保留重复；owned DOM 又覆盖。
- 任何“规范 JSON”都必须明确：key 排序规则、重复 key 策略、数字文本、escape、UTF-8、浮点 `-0`/指数、Unicode normalization（是否做）、最大深度。

sonic 没有提供以上规范保证：

- 共享 `Value` 序列化按 pair 源序，重复 key 全输出；[`Value::serialize`](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/value/node.rs#L1751-L1805)。
- owned `Object` 默认 `AHashMap`，迭代顺序不稳定；`sort_keys` 才是 `BTreeMap`；[object.rs](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/value/object.rs#L64-L82)。
- `sort_keys` 只排序，不去重；共享对象仍是 pair 数组。
- `Value::from(&[Pair])` 用 `insert`，重复 key 后者覆盖；与“共享 DOM 保留”不同。

**所以：不能把 `sonic_rs::to_vec(&Value)` 直接作为 canonical bytes / digest 输入。** 如果未来要用，必须另写 canonical writer，或继续用现有 `serde_json` 字节路径；sonic 只做解析/查询/中间表示。

### 6.3 现有哈希/身份路径的敏感性

| 路径 | 当前输入 | 换 JSON 序列化器的影响 |
|---|---|---|
| `retry_request_hash` | 自定义 streaming serde serializer；digest entries 排序，不依赖对象顺序；文档称其为 process-local fingerprint，不是持久化 wire format | 若字段的 `Serialize` 表示变化（尤其 serde_json `Value` 的 token/数字），摘要会变；对象顺序本身不敏感。若该指纹已进入 observation/分组，仍按身份数据处理。源码：[canonical.rs:42-50](../../backend/crates/stravia-runtime-contract/src/protocol/ir/canonical.rs#L42-L50)、[retry.rs:276-289](../../backend/crates/stravia-runtime-contract/src/protocol/ir/canonical/retry.rs#L276-L289) |
| `item_hash`/`item_hashes` | `serde_json::to_writer` 的流式字节（与 compact `to_vec` 同形） | 任何字节差异都会改 hash；会影响去重/引用。源码：[canonical.rs:52-82](../../backend/crates/stravia-runtime-contract/src/protocol/ir/canonical.rs#L52-L82) |
| tail 匹配 `hash_json` | 精确的 `serde_json` 输出字节 | 换序列化器即改匹配键；旧索引会失配。源码：[interaction_observation/tail.rs:49-88](../../backend/crates/stravia-core/src/interaction_observation/tail.rs#L49-L88) |
| `query_id` | SQL 模板的 SHA-256，不是 JSON（[performance.rs:442](../../backend/crates/stravia-core/src/performance.rs#L442)） | 不受 JSON 后端影响；不要把它和 JSON digest 混在一起改 |

规则：

- 任何持久化/对外暴露的 ID、hash、去重键，**不得静默重映射**。如果必须改字节，采用新版本前缀/新列/新索引，双读旧值，明确迁移；不要原地重算后覆盖。
- 如果只是把“解析器”从 serde_json 换成 sonic，但写回仍用 serde_json，字节可以不变（前提是解析→重新序列化不改变语义；对重复 key/大数仍需测试）。
- 如果让 sonic 写持久化字节，必须定义 canonical v2，并做旧/新 corpus 的字节级对比；不能只做“语义相等”测试。

### 6.4 BLAKE3 是独立迁移，不是 JSON 后端迁移的附带项

现有哈希路径使用 SHA-256（`sha2`）；BLAKE3 的替换会改变所有 digest 的十六进制值、长度假设、域分隔和已有索引。它应与 JSON 后端变更分开：

1. 先固定 JSON canonical bytes（或继续 serde_json 字节）；
2. 再为 BLAKE3 定义新域前缀/版本，例如 `stravia:<object>:v2:`；
3. 在可恢复迁移阶段读取并校验旧格式、生成新 BLAKE3 数据；完成后干净切换，正式运行不保留双写/双读或双算法 fallback；
4. 绝不因为“BLAKE3 更快”而静默重算公开 ID。

父任务 smoke 的 `blake3_stream_and_domain=PASS` 只说明该探针的流式/域分隔实现可用，不证明全仓哈希迁移已验收。同探针的合成耗时（非固定 CPU、非交错配对，仅示例）：8,556 B 时 SHA-256 3.497 µs、BLAKE3 4.111 µs；1,086,109 B 时 SHA-256 443.829 µs、BLAKE3 208.759 µs。不能据此承诺全仓收益，尤其小对象上 BLAKE3 可能更慢。

### 6.5 SQLite / PostgreSQL 一致性

- Observation payload 是 BLOB/BYTEA，`storage_codec` 的字节在两个后端相同；sonic 若只影响“解码后如何读”，不改变存储字节，风险局限在读取语义。
- PostgreSQL 的 `metadata_json` 是 `jsonb`（[0001_baseline.sql:341](../../backend/crates/stravia-core/migrations/postgres/0001_baseline.sql#L341)），`snapshot_state` 也是 `jsonb`（[0002_data_contracts.sql:3](../../backend/crates/stravia-core/migrations/postgres/0002_data_contracts.sql#L3)），会规范化 key 顺序并去重；SQLite 对应列是 `TEXT`（[0001_baseline.sql:346](../../backend/crates/stravia-core/migrations/sqlite/0001_baseline.sql#L346)、[0002_data_contracts.sql:46](../../backend/crates/stravia-core/migrations/sqlite/0002_data_contracts.sql#L46)）。**不能把数据库返回的 JSON 文本当 canonical bytes，也不能假设两个后端返回相同字节。**
- 如果未来在 SQL 层对 JSONB 做 digest/比较，先确认 PostgreSQL 的规范化结果；SQLite 没有同等 JSONB 规范化。身份应基于应用层语义/canonical writer，或基于 BLOB 原始字节，而不是 DB 文本。
- 事务/清理：JSON 解析/序列化尽量在事务外完成。清理按结构化归属和引用索引判断，不应为了删除而解码全部正文；必要的解码或迁移失败应显式报错并保留进度，不能静默跳过或 panic。旧 decoder 仅用于一次性可恢复转换，正式读取在 cutover 后只保留新格式，不能假设所有历史 payload 都能被新 parser 无损解析。
- ropefish 工作区（`../ropefish` HEAD `4a2447c3`，157 个已跟踪未提交改动、22 个未跟踪文件）的前置核查显示其改动集中在 billing/chrono-tz observation 字段，未改 `storage_codec.rs`；但这些字段会改变 typed JSON 形状与序列化字节。**合并后必须重新生成 golden，不能复用 jiaolong 的字节基线**；也不能用 sonic 的“保留顺序”去替代 ropefish 分支已有的语义。

## 7. Wasm、SIMD、unsafe 与部署边界

### 7.1 Wasm：有支持声明，但没有 Stravia 目标的验证

- 0.5.7 release 声明支持 Wasm；上游 CI 在 tag/main 使用 `cargo build --tests --target wasm32-wasip1`，再用 wasmtime 执行；源码：[ci.yml](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/.github/workflows/ci.yml#L39-L59)。
- Stravia vendor 构建目标是 `wasm32-wasip2`（[vendor-plugins 设计](../../docs/design/vendor-plugins.md#L406-L412) 与 `stravia-build-vendors` 的 `--target wasm32-wasip2`）。上游没有 wasip2 的 CI 证据。
- 父任务 2026-10-28 的隔离 probe 已用 `cargo build --release --target wasm32-wasip2 --bin portable --features precision,sorted` 编译成功（artifact://156）；但环境没有 wasmtime CLI，**只证明可编译，不证明可运行**。wasip2 的组件模型/线程/`std` 支持与 wasip1 不同，不能仅凭“编译通过”推断运行可用。
- 非 x86_64/aarch64 的 SIMD 走 fallback；Wasm 没有原生 SIMD 路径，性能可能显著低于 README 的 x86_64 数据。运行行为仍需 wasmtime/wasip2 host smoke。
- 现有 vendor JSON 契约明确保留 `serde_json`；sonic 进入 Wasm 边界会引入第二个 JSON 语义（重复 key/顺序/数字），与跨 guest/host 的 canonical payload 冲突。**当前不建议在 vendor/Wasm 边界采用 sonic。**

### 7.2 Native SIMD 是编译期选择，不是运行时 dispatch

源码用 `cfg!(target_feature = ...)` 选择 x86_64 的 `pclmulqdq + avx2 + sse2` 或 fallback；aarch64 用 neon；其他架构 fallback。源码：[util/arch/mod.rs](https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/util/arch/mod.rs#L1-L40)。这意味着：

- 默认 `x86_64-unknown-linux-gnu` 构建没有 `avx2`/`pclmulqdq` 目标特性时，走 fallback；**README benchmark 的 SIMD 前提不自动成立**。
- README 建议 `-C target-cpu=native`；但本项目仓库没有设置 `RUSTFLAGS`/`target-cpu`/`target-feature`（全仓检索未发现，[vendor 构建还显式移除宿主 RUSTFLAGS](../../backend/crates/stravia-vendor-base/src/bin/stravia-build-vendors.rs#L72-L73)）。若为了 sonic 全局加 `target-cpu=native`，分发的二进制可能依赖构建机 CPU，迁移到旧机器会非法指令；应使用明确的 baseline 或按部署机分构建。
- `avx512` 是 opt-in feature，需要较新的 Rust/CPU；不适合作为默认。`sanitize` 只用于 LLVM sanitizer，README 明说生产开启约 30% 序列化损失；不要在生产 feature 中开启。
- 结论：sonic 在当前仓库的“默认构建”下不能假定有 SIMD 加速；性能验收必须在真实发布 flags 下做，而不是拿上游 benchmark 对比。

### 7.3 unsafe 面与已知缺陷

0.5.10 的 `src/` 中约有 250 处包含 `unsafe` 的代码行、44 个 `unsafe fn`（grep 计数，不是审计结论）；没有 `#![forbid(unsafe_code)]`。关键风险：

- `Value` 的 `Send`/`Sync` 是手动 unsafe 声明（§2.4）。
- 0.5.10 `serialize_char` 越界读（main 修复），说明 typed serializer 的 unsafe 路径有真实 UB 风险。
- 0.5.10 `Value` DOM 无深度限制，深嵌套不可信 JSON 可栈溢出（main 修复）。
- `from_slice_unchecked` 跳过 UTF-8 检查；`get_unchecked` 假定 JSON 合法；存储路径禁止。
- 0.5.10 发布后 main 仍有 8 个未发布修复（§1.2），版本成熟度不足以承担协议边界。

### 7.4 隐私/脱敏

现有观测脱敏在 `serde_json::Value` 上解析/重写（[interaction_observation/redaction.rs](../../backend/crates/stravia-core/src/interaction_observation/redaction.rs#L684)，`redact_embedded_json`）。sonic 路径的风险：

- `LazyValue`/`OwnedLazyValue` 的 raw 透传可以绕过基于 `serde_json::Value` 的脱敏；如果先提取 raw 再写日志/响应，敏感字段不会被改写。
- `Value`/`OwnedLazyValue` 的 `Debug`/`Display` 可能输出整段 raw JSON；不能把未脱敏值放进错误日志、tracing span、panic message。
- 若用 sonic 只做解析后立即转回 serde_json，仍要经过原有脱敏；不要为了性能在脱敏前暴露 raw。
- 观测 payload 可能含用户正文；任何“只读提取”试点都要限定在脱敏后的 payload 或内部非用户字段，且禁止把 raw 写入诊断输出。

## 8. 候选/禁用面矩阵

| 面 | 结论 | 条件/理由 |
|---|---|---|
| 已解压 observation payload 的只读多字段提取 | **可试点** | 用 `get_many` + `LazyValue`，固定路径集合；严格 UTF-8；明确重复 key/缺失策略；不写回、不做 digest；不跨 API 响应直接暴露 raw |
| 诊断/离线工具解析大型只读 JSON | **可试点** | 低风险、非热路径；输出必须脱敏；不能把 `Value` clone 长期缓存 |
| `OwnedLazyValue` 提取后立即复制到业务字符串/字节 | **条件可用** | 0.5.10 有未解析视图 panic；需 pin main `444063c` 或绕开 `as_array/as_object`；限制保留大小/生命周期 |
| `Value` 作为短生命周期快照/COW 分支 | **条件可用** | clone O(1) 但保留整篇；只适合请求内、不会长期缓存 tiny 子树；需测峰值 RSS |
| 替换 `serde_json` 解析 typed `AiRequest`/`AiResponse` | **禁用** | `serde_json::Value`/`Number`/`RawValue` 与 sonic token 不兼容；大数丢精度/`1e400` 失败；真实 ContentBlock 已实测 |
| 用 sonic 写持久化/canonical bytes | **禁用（当前）** | 无 canonical 保证；顺序/重复/数字文本不稳定；会改 digest/ID |
| 用 sonic 计算 `item_hash`/tail `hash_json`/retry hash | **禁用（当前）** | 字节变化会改身份/匹配；必须版本化并双读，不得静默 |
| 用 sonic 序列化 API 响应（axum `Json`） | **禁用** | `LazyValue`/`RawNum` 经 serde_json 会变 magic object；typed IR 数字会坏 |
| Wasm vendor 插件 | **暂缓/禁用** | 上游只测 wasip1，Stravia 是 wasip2；现有 ADR 保留 serde_json；未实测 |
| 热路径双 DOM（serde_json + sonic 各解析一遍） | **禁用** | O(n) 重复解析+双份内存，抵消收益；除非一次性迁移/边缘 |
| `from_slice_unchecked`/`get_unchecked` 处理存储/网络输入 | **禁用** | `from_slice_unchecked` 跳过 UTF-8 校验；`get_*_unchecked` 假定 JSON 合法；0.5.10 还有深度/UB 缺陷 |

### 8.1 适用性判断（不是泛化提速承诺）

sonic 的 README benchmark 是特定硬件（Xeon 8260 等）、特定数据集、无 Stravia 的 `serde_json::Value`/`arbitrary_precision`/Wasm/重复 key 约束；不能把“快 3–6 倍”当作本项目收益。父任务隔离探针反而显示：

| 合成操作（release，单次探针） | 8,556 B | 1,086,109 B |
|---|---:|---:|
| `serde_json` parse（median） | 8.139 µs | 922.364 µs |
| `sonic_rs::Value` parse（precision+sort，median） | 12.305 µs | 1,986.341 µs |
| `serde_json` serialize | 6.571 µs | 636.624 µs |
| `sonic_rs` serialize | 2.693 µs | 184.203 µs |
| sonic `Value` clone | 0.015 µs | 0.013 µs |

这些数字只说明：**在该合成形状/feature 下，sonic parse 不一定更快，serialize/clone 更快；不能外推。** 真实收益取决于对象大小、重复 key、字符串/转义比例、是否已解压、是否真的需要 DOM。若只是 `get_many` 提取几个字段，sonic 的 Lazy 路径可能避免完整 DOM；若随后还要转 serde_json，收益会被 O(n) bridge 吃掉。

## 9. 分阶段迁移与验收矩阵

原则：**先解析/查询，后写回；先边缘，后协议；先语义，后字节；先单 DOM，后谈替换。** 不在热路径同时保留两个 DOM。

### Phase 0：准备（0.5.10，只读研究）

- 固定 `sonic-rs = "=0.5.10"`，显式关闭不需要的 feature；如果试点只需要 Lazy/`get_many`，不要为了“顺便”开 `sort_keys`/`avx512`。
- 先记录当前 JSON 边界清单：哪些路径是 `serde_json::Value`、`Number`、`RawValue`，哪些是纯 typed struct，哪些是持久化 bytes。调用面跨 runtime-contract、protocol-codec、Core 和 vendor，不能用文本替换完成迁移。父任务执行 `cargo tree --locked -p stravia-core -e features -i serde_json --depth 1`，确认运行依赖包含 `arbitrary_precision`、`float_roundtrip`、`raw_value`，该输出中未启用 `preserve_order`；build-dependencies 的 feature 集单独显示，不能混作运行时行为。
- 建立合成 golden corpus：重复 key、顺序、null/missing、u64/i64 边界、超长整数、`1e400`、`-0.0`、`1.2300`、转义、非法 UTF-8、深嵌套、空对象/数组。禁止使用真实用户数据/凭据。
- 验收门：研究阶段不改产品代码或持久化字节，sonic 只在隔离 probe 中。正式采纳时按 Module 切换，不建立长期 feature flag 双路径。

### Phase 1：只读 `get_many`/`LazyValue` 试点（低风险）

- 对象：已解压的 observation payload 或离线报告；路径固定、数量有限。
- 实现约束：
  - 使用安全 `from_slice`/`get_many`，禁止 `_unchecked`；
  - `get/get_many` 不校验尾随垃圾；在边界先用现有全文校验路径（或能检查 trailing 的 parser）确认合法，再提取；不要把 `get` 的 `Ok` 当整篇合法；
  - 先确认输入 UTF-8、深度上限（0.5.10 无 DOM 深度限制；Lazy 遍历也要防恶意深嵌套）；
  - 重复 key 明确策略（建议与现有 serde_json 语义一致：后者覆盖；若要保留重复，必须在该业务层显式定义）；
  - 缺失返回 `None`，显式 `null` 返回 raw `null`，不得混为同一状态；
  - 不把 `LazyValue` 放进 serde_json 响应；不把 raw 写入日志。
- 验收：
  - 与现有 serde_json 读取结果逐字段对比（值、缺失、重复、数字文本）；
  - 1 MiB 正文的峰值 RSS/保留内存不高于现有路径 + 可解释的 Lazy 收益；
  - 不产生双份完整 DOM；
  - 试点不能证明兼容或净收益就不采用；正式路径显式传播解析错误，不按输入失败自动回退另一解析器。

### Phase 2：`OwnedLazyValue` 分离提取（条件）

- 仅当确实需要“脱离源 buffer 的 raw 子字段”，且字段是字符串/数字/小对象；不要用它做通用 DOM。
- 0.5.10 的 `as_array`/`as_object` panic 是阻断；选择：
  - pin 到 main `444063c`（但 main 未发布，需要 vendor patch/自建 fork），或
  - 只调用 `as_raw_str`/`as_str`/`as_i64` 等不触发未解析视图的 API，或
  - 先 `as_array_mut`/`as_object_mut` 物化（会失去 Lazy 收益）。
- 验收：drop 源 buffer 后仍可读；保留字节有上限；分别测试未解析的 FastStr 共享和已解析的容器克隆；并发首次 parse 用适用的工具或多线程 smoke 验证，不能只看 Send/Sync 声明。

### Phase 3：typed 边界试验（仅限不含 serde_json 类型的 DTO）

- 选一个内部、无 `serde_json::Value`/`Number`/`RawValue` 的 DTO，比较 `serde_json` 与 sonic 的 parse/serialize 语义与性能。
- 必须测试 `char`、大整数、`1e400`、未知字段、`deny_unknown_fields`、enum variant。
- 0.5.10 的 `serialize_char` UB 阻断任何含 `char` 的 typed serializer；必须 pin main 或等下一版。
- 验收：字段级语义一致；错误分类一致；没有 magic token；性能收益可复现且不是靠 `_unchecked`。

### Phase 4：若试点成功，建立内部 JSON seam

- 在已有读取 Module 中隐藏解析与字段提取细节；当前写入仍由 serde_json 拥有。不为了可替换库名另造只有一套实现的通用 JSON trait。
- 每个边界只允许一个 JSON 后端：读用 sonic，写用 serde_json 时，必须有明确的“读→业务值→写”转换，不能把 sonic `Value` 直接交给 serde_json serializer。
- 当前不把 `sonic_rs::Value`/`LazyValue` 直接放进公共结构体或长期缓存。未来如按总体设计迁移内部 IR，必须连同所有消费者、序列化器和 backing ownership 一起改，不能向仍使用 serde_json 的边界暴露这些类型。
- 代码审查门：搜索 `sonic_rs` 只出现在允许的模块；`serde_json::Value` 不进入 sonic serializer。

### Phase 5：写路径/canonical v2（高门槛，暂不建议）

- 只有 Phase 1–4 证明有净收益，且业务需要字节稳定的 canonical 表示时，才设计 `canonical_json_v2`：明确 key 排序、重复 key 策略、数字文本、escape、UTF-8、`-0`、指数、深度、Unicode normalization。
- 新版本写入新列/新前缀；旧数据经可恢复转换后干净切换，不保留正式双读；`item_hash`/tail hash/retry hash 分别建立版本化域。BLAKE3 的算法契约独立决策，必要的数据重写可以与 descriptor 转换合并执行。
- 验收：
  - 同一语义的多种输入（不同 key 顺序、重复 key、等价数字文本）产生相同 canonical bytes（按定义）；
  - 旧/新字节对同一 payload 的差异有清单；
  - 旧 ID 不被静默重算；回滚可恢复；
  - PostgreSQL JSONB 与 SQLite TEXT 的规范化差异不进入身份；
  - 转换中断后可继续；切换前旧事实完整，切换后新格式独立可读，回退使用配套备份。

### 9.1 验收矩阵

| 门 | 必须通过的证据 | 当前状态 |
|---|---|---|
| 版本/来源 | `=0.5.10` 固定；crates/tag/docs.rs 一致；main 修复不混入发布声明 | 已证（§1） |
| 语义 parity | 重复 key/顺序/null/missing/大数/`1e400`/`-0`/escape/UTF-8/深度 | 部分已证；typed IR 失败（父任务 smoke） |
| serde_json 互操作 | `Value`/`Number`/`RawValue` 四方向均无 magic token | **失败**；token 不兼容 |
| 持久化字节 | 对 representative corpus 字节级一致，或 canonical v2 明确 | 未测；当前不可保证 |
| 摘要/ID | 旧 hash 不变，或新版本+双读；无静默重映射 | 未测；设计门 |
| 内存 | 峰值/保留内存可解释；tiny clone 不长期钉住大文档 | COW 收益与保留均已实测（父任务） |
| 并发 | 跨线程只读、`OwnedLazyValue` 首次 parse、无数据竞争 | 只读通过；其余待测 |
| unsafe | 0.5.10 已知 UB/深度缺陷有规避；Miri/审查 | 已知缺陷存在；未审计 |
| Wasm | `wasm32-wasip2` 编译+运行；非仅 wasip1 | 编译已过（父任务 probe）；运行未验证 |
| Native SIMD | 发布 flags 明确；不依赖构建机 `target-cpu=native` | 仓库未设置；需显式决定 |
| 隐私 | 脱敏后路径；raw 不泄漏到日志/响应 | 设计门；现有脱敏在 serde_json |
| 回滚 | feature flag/旧 decoder 可回退；无双写不一致 | 设计门 |

## 10. 最小 smoke 建议（供父任务执行；本轮未运行）

以下都用合成数据，放在 `/tmp` 隔离 crate；不要用生产 payload/凭据。重点不是“跑通”，而是把失败模式固定成断言。

### 10.1 arbitrary_precision token（最高优先级）

```rust
// serde_json 启用 arbitrary_precision；sonic-rs 0.5.10
let sj: serde_json::Value = serde_json::from_str(r#"{"n":1}"#).unwrap();
let out = sonic_rs::to_string(&sj).unwrap();
assert_eq!(out, r#"{"n":1}"#); // 预期失败：{"n":{"$serde_json::private::Number":"1"}}
```

```rust
let big = r#"{"n":18446744073709551616}"#;
let bridge: serde_json::Value = sonic_rs::from_str(big).unwrap();
assert_eq!(serde_json::to_string(&bridge).unwrap(), big); // 预期失败：f64
```

```rust
let raw = serde_json::value::RawValue::from_string("123456789012345678901234567890".into()).unwrap();
let out = sonic_rs::to_string(&raw).unwrap();
assert_eq!(out, "123456789012345678901234567890"); // 预期失败：magic object/错误
```

同时跑 `sonic_rs::Value` 自身（开 `arbitrary_precision`）确认 raw 保留，以区分“sonic DOM 能保”与“桥接不能保”。

### 10.2 重复 key / 顺序

```rust
let v: sonic_rs::Value = sonic_rs::from_str(r#"{"z":0,"a":1,"a":2}"#).unwrap();
assert_eq!(sonic_rs::to_string(&v).unwrap(), r#"{"z":0,"a":1,"a":2}"#);
assert_eq!(v["a"].as_i64(), Some(1)); // 共享 DOM 第一个
let mut owned = v;
owned["x"] = 1.into(); // to_mut 物化
// 断言：重复 key 只剩 a=2；顺序不再保证（AHashMap）
```

```rust
let mut tree = sonic_rs::PointerTree::new();
tree.add_path(&["a"]);
tree.add_path(&["z"]);
let vals = sonic_rs::get_many(r#"{"a":1,"a":2,"z":null}"#, &tree).unwrap();
// 明确断言重复 key 的 get_many 结果（按实现/业务策略），不要只打印
```

### 10.3 号码与 `type` 字段

对每个输入同时比较：`serde_json::Value`、`sonic_rs::Value`（默认/`arbitrary_precision`）、`sonic_rs::from_str::<serde_json::Value>`、`serde_json::to_string(&sonic_value)`：

- `18446744073709551616`
- `-9223372036854775809`
- `1e400`
- `1.2300`
- `-0.0`
- `{"type":"tool_use","input":{"n":18446744073709551616}}`（真实 ContentBlock 形状的合成版）

不要用“解析成功”作为通过；必须断言输出文本/精度/错误类型。

### 10.4 COW/保留内存

- parse 1 MiB 合成正文，clone 根 9 次，drop 原根，测 tracking allocator 存活字节；预期 sonic 约 1×、serde_json 约 9×（父任务已做）。
- 只 clone tiny 子树、drop 根，测存活字节；预期整篇仍保留（父任务 `1049228`）。如果业务要长期缓存 tiny 字段，这个测试必须失败/改用 `OwnedLazyValue` 或复制到业务类型。
- 修改 clone 的嵌套对象，断言原值不变（COW 正确性）。

### 10.5 `OwnedLazyValue` 生命周期/视图

```rust
let bytes = bytes::Bytes::from_static(br#"{"a":{"k":1},"b":2}"#);
let owned = {
    let lv = sonic_rs::get(&bytes, &["a"]).unwrap();
    sonic_rs::OwnedLazyValue::from(lv)
};
drop(bytes);
assert_eq!(owned.as_raw_str(), r#"{"k":1}"#); // 脱离源
let _ = owned.as_object().unwrap(); // 0.5.10 未解析时预期 panic；main 444063c 修复
```

### 10.6 线程/Unsafe/Wasm

- `Value` 跨线程只读 clone/查询；`OwnedLazyValue` 多线程首次 `as_raw_str`/`as_object`；用 `cargo miri`（若 sonics 的 SIMD/asm 不允许，记录为“无法 Miri”而不是通过）。
- `cargo check --target wasm32-wasip2` + wasmtime 运行最小 Value/get/OwnedLazyValue；同时跑 `wasm32-wasip1` 作为上游对照。不要只跑 native。
- 构建两次：默认 flags vs `-C target-cpu=native`；记录是否进入 SIMD、二进制是否在旧 CPU 上非法指令。不要把 native 构建产物当通用发布物。

## 11. 明确未测/未知

- `wasm32-wasip2` 的运行语义（父任务已用 `--target wasm32-wasip2 --bin portable --features precision,sorted` 编译成功，但无 wasmtime CLI；上游只测 wasip1）。
- PostgreSQL JSONB 与 SQLite 的端到端字节/语义对比；本报告只从列类型和 `serde_json` 行为推断。
- 真实 Stravia payload 的对象/字符串/转义/重复 key 分布；没有用生产数据，不能给“实际提速百分比”。
- `get_many` 在重复 key/超深嵌套/非法 UTF-8 下的完整行为；需合成 corpus。
- `OwnedLazyValue` 并发首次 parse 的 soundness；需 Miri/loom 或至少 stress。
- BLAKE3 全仓迁移的兼容性；不在本文范围，不能与 JSON 后端一起“顺手”切。
- ropefish 工作区合并后的字节基线；必须在其工作树重新生成，不能复用本文/jiaolong 的 golden。

## 12. 结论

- **现在可做**：在隔离 probe/低风险只读路径用 0.5.10 的 `get_many`/`LazyValue` 验证收益；严格输入校验；不写回、不做 ID、不 dual DOM。
- **现在不可做**：用 sonic 替换 `serde_json` 的 typed IR round-trip；用 sonic 写持久化/canonical bytes；用 sonic 序列化 API 响应；在 Wasm 插件边界采用；在 0.5.10 上使用含 `char` 的 typed serializer 或未修复的 `OwnedLazyValue` 视图。
- **下一步**：native 边界 smoke 和 Wasm 编译已完成，Wasm 运行、真实路径与并发验证仍缺。优先实现总体设计的查询工作集、清理批次和共享所有权；sonic 的后续采纳须选含必要修复的固定版本，并完成语义/性能门禁。sonic 不是解决“查询/写入多、删除低效”的默认答案。

## 13. 一手来源

### sonic-rs

- 发布页：<https://github.com/cloudwego/sonic-rs/releases/tag/v0.5.10>
- crates.io：<https://crates.io/crates/sonic-rs/0.5.10>；API：<https://crates.io/api/v1/crates/sonic-rs>
- docs.rs：<https://docs.rs/sonic-rs/0.5.10/sonic_rs/>
- `Value` 文档：<https://docs.rs/sonic-rs/0.5.10/sonic_rs/struct.Value.html>
- `LazyValue` 文档：<https://docs.rs/sonic-rs/0.5.10/sonic_rs/struct.LazyValue.html>
- `OwnedLazyValue` 文档：<https://docs.rs/sonic-rs/0.5.10/sonic_rs/struct.OwnedLazyValue.html>
- tag 源码：<https://github.com/cloudwego/sonic-rs/tree/v0.5.10>
- `Value`：<https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/value/node.rs>
- `Object`：<https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/value/object.rs>
- `LazyValue`：<https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/lazyvalue/value.rs>
- `OwnedLazyValue`：<https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/lazyvalue/owned.rs>
- `get`/`get_many`：<https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/lazyvalue/get.rs>
- iterators：<https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/lazyvalue/iterator.rs>
- `RawNumber`：<https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/serde/rawnumber.rs>
- serde 数字/值：<https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/serde/de.rs>、<https://github.com/cloudwego/sonic-rs/blob/v0.5.10/src/serde/ser.rs>
- 兼容性文档：<https://github.com/cloudwego/sonic-rs/blob/v0.5.10/docs/serdejson_compatibility.md>
- README：<https://github.com/cloudwego/sonic-rs/blob/v0.5.10/README.md>
- CI：<https://github.com/cloudwego/sonic-rs/blob/v0.5.10/.github/workflows/ci.yml>
- main HEAD：<https://github.com/cloudwego/sonic-rs/commit/f63d1851de596fb9c53bf2a67ec84828c1d65c3c>
- main 修复 `OwnedLazyValue`：<https://github.com/cloudwego/sonic-rs/commit/444063c>
- main 修复 `serialize_char`：<https://github.com/cloudwego/sonic-rs/commit/443f111>
- main 深度限制：<https://github.com/cloudwego/sonic-rs/commit/f7bdde1>

### serde_json 1.0.154

- `Number`/token：<https://github.com/serde-rs/json/blob/v1.0.154/src/number.rs>
- `Value` 反序列化：<https://github.com/serde-rs/json/blob/v1.0.154/src/value/de.rs>
- `RawValue` token：<https://github.com/serde-rs/json/blob/v1.0.154/src/raw.rs>
- Serializer token 分支：<https://github.com/serde-rs/json/blob/v1.0.154/src/ser.rs>
- 默认 map：<https://github.com/serde-rs/json/blob/v1.0.154/src/map.rs>
