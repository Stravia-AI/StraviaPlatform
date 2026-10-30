# 当前项目内存数据结构紧凑化调查

日期：2026-10-28。范围：后端常驻索引/缓存、canonical 请求与历史快照、Observation 实时分发，以及 WebUI 对话与图状态。以当前工作树源码、一手库文档和隔离分配实验为依据。

结论：存在可操作的节省空间，**最稳妥的首项是私有模型索引，潜在大收益在未共享的大 payload**。本轮只新增这份研究报告；没有改产品代码、公共 API、数据库、生产依赖或锁文件，没有使用真实用户数据。实测是特定构建/合成输入下的存活请求分配，不是生产 RSS 或端到端性能结论。未实测的方案明确作为候选，不给承诺降幅。

后续验证：[EcoString 与 Arc<String> 对照实验](ecostring-arc-string-benchmark.md)已经完成。结果不支持全局替换现有 `Arc<String>`；长正文多快照几乎不省存活内存，owned String 转入/转出还会增加复制。EcoString 的优势限定在短串和部分共享后追加场景，不能把它当作通用大正文内存优化。

## 仓库结论与实施顺序

本轮建议分两条线推进：**先用纯内部索引改动取得低风险收益；再以真实多模态续接回放验证大 payload 共享。** 不建议全仓替换 `String`、`Vec`、`HashMap`，也不以缩短保留期、丢弃历史或放宽身份校验换取内存。以下优先级是按源码生命周期、合成测量与改动边界排列，不是生产 heap profile 的热点排名。

|顺序|具体结构与证据|紧凑表示建议|边界与风险|
|---|---|---|---|
|1：低风险试点|`ModelIndex` 持有 records、`HashMap<String, usize>` exact、`HashMap<String, Option<usize>>` normalized；构建时再次拥有模型 ID。见 [models.rs](../../backend/crates/stravia-core/src/runtime_config/models.rs#L8) 8–34 行|先只改私有索引：key 用 `Box<str>`；normalized position 用 `Option<NonZeroUsize>`，以 position + 1 编码。短 ID 占比高时再比较 `CompactString`|不改 `ProviderModelRecord`/SQL/HTTP；保留 exact 优先、最右段 ASCII 忽略大小写、歧义返回 None。索引不是整个模型记录，其收益不能外推全部目录|
|2：大 payload 优先验证|`Text` 已共享，但 `Thinking.thinking`、`MediaSource::Base64.data`、reasoning 字符串、Tool JSON 仍拥有正文。见 [request.rs](../../backend/crates/stravia-runtime-contract/src/protocol/ir/request.rs#L37) 37–200 行；真实 clone 在 [project.rs](../../backend/crates/stravia-core/src/generation_chain/project.rs#L25) 25–61 行与 [store.rs](../../backend/crates/stravia-core/src/generation_chain/store.rs#L992) 992–1011 行|对不可变大正文延续现有共享/COW 模式；先比较 `Arc<String>`，再评估 `EcoString`/`SmolStr`。不是用短字符串库解决深拷贝|必须覆盖脱敏、Hook 修改、分支隔离、序列化和 Wasm/WIT 转换；`Value` 的对象/数组共享不是换一个字符串类型就完成。修改 exported canonical 类型须先确认公共 API 范围|
|3：缓存命中避免重复拥有历史|`CachedCatalog.catalog: Result<Vec<AiItem>, String>` 在本地 get 返回 `entry.catalog.clone()`，put 也 clone；见 [store.rs](../../backend/crates/stravia-core/src/generation_chain/store.rs#L1184) 1184–1234 行|让只读消费者借用/共享整个 catalog；比较 `Arc<Vec<AiItem>>` 或冻结的 `Arc<[AiItem]>`，只在必须修改时建立独立副本|普通文本已是 Arc，不能把所有 catalog 字节都当复制量；Redis 每次读仍需反序列化。保留 Principal、ingress、到期边界和错误结果语义|
|4：长期运行基数风险|`RoutePolicyStateInner.conversation_targets` 的 key 拥有 principal、route、identity，value 拥有 target。成功写入与选路读取见 [selector.rs](../../backend/crates/stravia-core/src/router/selector.rs#L202) 202–236、397–404、621–632 行；该字段未见 clear/remove/TTL|先共享反复出现的 principal/route/target，唯一 identity 可用 `Box<str>`；用有限生命周期的所有权域复用字符串，不建立永不回收的全局 interner|紧凑表示只能降低每项成本，不能解决随唯一身份增长。新增 TTL/淘汰会改变软亲和保留语义，须另行决策，不能冒充无行为变更；见 [ADR-0034](../adr/0034-layer-route-target-selection.md)|
|5：多订阅者场景|Live mirror 的 `publish` 对每个 Mailbox clone block，订阅时 `ordered` clone 快照；见 [live.rs](../../backend/crates/stravia-core/src/interaction_observation/live.rs#L170) 170–192、270–279 行|每个 revision 构造一次不可变共享快照，多个 Mailbox 引用同一份；先测订阅者数量与积压|mirror 的 `append` 仍会 `push_str`（249–261 行），不是不可变对象；若直接 COW，每次慢消费者持有旧 revision 时仍会复制。不可改变 coalescing、LiveFinished 顺序、8 MiB/256 项边界与 interaction 隔离；本轮未测此路径|

`Option<NonZeroUsize>` 与 `usize` 同尺寸有 [Rust 标准库布局保证](https://doc.rust-lang.org/std/num/type.NonZeroUsize.html)。这里 `None` 继续代表 normalized 键歧义，而不是模型索引 0；位置编码需要 checked/有证明的加一，不能直接 `NonZeroUsize::new(position)` 丢掉首项。

### 浏览器侧：先减少完整历史驻留与中间快照，不先换 TypedArray

- **Console Chat**：[adapter](../../frontend/stravia-webui/src/lib/console-chat-adapters.ts#L61) 的 `getAll()` 加载完整对话，消息包含原图 data URL 与完整 output items；[controller](../../frontend/stravia-webui/src/lib/console-chat.ts#L420) 420–436 行与 adapter 91–94 行均有 `structuredClone`。普通 UI snapshot 主要复制对象壳，不能说它每次深拷贝所有图片。优先验证能否在保留异步保存快照隔离的前提下去掉重复 clone；再评估列表只保留摘要、按需装载完整对话。后者需完整处理刷新、重试、发送与 IndexedDB 契约，不得压缩原图或删除旧消息。[现有产品边界](../design/console-chat.md)。
- **Observation/布局**：[workspace](../../frontend/stravia-webui/src/lib/observation-workspace.ts#L935) 的 canvas 派生数组主要共享 interaction 对象，`loadMore` 会累计 roots；[layout](../../frontend/stravia-webui/src/lib/interaction-layout.ts) 的 Worker 往返复制拓扑 ID/边/坐标，不是完整 observation payload。先测累计节点数和主线程/Worker 两侧堆；只有数值密集传输确为热点时才考虑 TypedArray，不能忽略新增 string-ID 索引的成本。
- **Query cache**：[QueryClient 配置](../../frontend/stravia-webui/src/routes/+layout.svelte#L106) 没有显式设置 gcTime；本轮没有测缓存回收，不给“无界泄漏”结论。

前端为只读源码调查，未启动浏览器、未读取用户 IndexedDB，也未测实际 heap snapshot。

### 已做的优化与不应优先做的替换

- [RuntimeCache](../../backend/crates/stravia-core/src/runtime_cache.rs#L35) 已采用 `TinyUfo::new_compact`、`Box<str>` key 和 `Arc` value；默认 **16 MiB 逻辑预算**在各 domain 间共享，不是每个 domain 16 MiB，也不是 RSS 上限。[配置契约](../design/architecture.md#统一派生缓存与-server-配置)。
- [DiscoveryWorkspace](../../backend/crates/stravia-core/src/generation_chain/store.rs#L3) 与 [TurnReadContext](../../backend/crates/stravia-runtime-contract/src/turn_chain.rs#L143) 是 **16 MiB 请求局部预算**，不是常驻完整历史缓存；节点/内容共享、逐节点解码、借用祖先窗口已实施。[ADR-0076](../adr/0076-deduplicate-turn-chain-items-and-share-binary-storage-codec.md)。
- 现有 `Arc<AiRequest>`/`Arc<String>`、`Bytes::from(Vec)`、Trace RawValue 与借用 storage JSON 不应重复算新收益。[已有优化记录](protocol-http-benchmark.md)（123–137 行；以内容为准）。
- 平台普通 ID 是 **28 个小写字母，约 131.6 bit**；内容摘要是 **55 字符 base26，完整 256 bit**，不是通用 UUID 或 hex64。[identifier.rs](../../backend/crates/stravia-runtime-contract/src/identifier.rs#L5)。不可整体换成 128-bit UUID；28 字节也超出 CompactString 的 24 字节内联容量。对仅内部存储的固定数组表示可以另做实验，但不改变公共/持久文本身份，也不能忽略 `TurnNodeId::new` 的现有接受范围。
- [CacheAffinity](../../backend/crates/stravia-core/src/router/cache_affinity.rs#L32) 的 item hash 已为 `[u8; 32]`；每 key 最多 1,024 条并受共享缓存预算与 TTL 管理，不需要再建议“把 hash String 改二进制”。
- [TailIndex](../../backend/crates/stravia-core/src/interaction_observation/tail.rs#L466) 有多个索引，但 key 域并不全相同：`last_hashes` 按 hash、`pending_tools` 按 tool ID，不能机械合并成一张 run 表。匹配还会比较完整 `Value`（699 行），不能只保留 digest 丢掉精确核验。其 JSON 表示紧凑化需要单独实验，本轮不估算收益。
- 不凭 enum 尺寸直接承诺 boxing 后的目标大小。`ContentBlock`/`AiStreamDelta` 的多个变体都可能决定布局；Box 会为被装箱变体增加分配。Tokio channel 的逻辑 capacity 也不能直接冒充一次性预分配的槽位数量。

## 新库选择：按所有权与长度分布决定

已查一手文档的版本：`compact_str 0.9.1`、`smol_str 0.3.7`、`ecow 0.3.1`、`smallvec 1.16.2`、`thin-vec 0.2.21`、`lasso 0.7.3`、`string_cache 0.11.0`。版本是本轮证据锚点，不代表建议一律升级。仓库已有 bytes 直接依赖；compact_str/smallvec 已在锁内但属于传递依赖，直接用于产品仍需声明依赖。EcoString、SmolStr、ThinVec 未在当前仓库锁中使用；首轮未运行这三个库，后续已在隔离临时程序中测试 EcoString，未新增生产依赖。

|候选|官方契约与适用情形|当前项目选择|
|---|---|---|
|[Box<str> / Box<[T]>](https://doc.rust-lang.org/std/string/struct.String.html#method.into_boxed_str)|去掉 capacity 字段；冻结时丢弃富余容量，可能重分配；clone 仍复制|不可变私有索引的首选；不要在读写热路径反复 freeze/unfreeze|
|[Arc<String> / Arc<str>](https://doc.rust-lang.org/std/sync/struct.Arc.html)|共享 clone；make_mut 在有其它强引用时分离数据。Arc::new(String) 移动原 buffer；转换到 Arc<str> 需要新布局|优先复用已有 Arc<String> 模式；Arc<str> 仅在冻结/共享关系明确时比较，不做全局替换|
|[CompactString](https://docs.rs/compact_str/0.9.1/compact_str/)|x64 可内联 24 字节；长字符串 clone 仍 O(n)；支持修改|适合短名称/短 key；不解决大 Thinking/Base64 clone。已有传递依赖不意味着使用成本为零|
|[SmolStr](https://docs.rs/smol_str/0.3.7/smol_str/struct.SmolStr.html)|常规 UTF-8 字符串可内联 23 字节；堆字符串以 Arc 共享，clone O(1)；不可变|不可变且频繁复制的标识/名称候选；从已有长 String 转入需要复制，不能用来内联平台 28-byte ID|
|[EcoString](https://docs.rs/ecow/0.3.1/ecow/struct.EcoString.html)|常规小端平台对象 16 B、内联 15 B；长字符串原子引用计数共享，修改时 COW；可选 serde|后续实测不支持替换现有 Arc<String>；仅短串、共享后追加且少跨 String 边界的局部结构值得继续评估，详见[对照实验](ecostring-arc-string-benchmark.md)|
|[SmallVec](https://docs.rs/smallvec/1.16.2/smallvec/struct.SmallVec.html)|把 N 个元素内联到父对象；溢出仍用堆；union 特性也不能消除大元素占位|不作为 ContentBlock Vec 的默认替代。只在小元素、几乎总非空且长度分布明确的局部结构试验|
|[ThinVec](https://docs.rs/thin-vec/0.2.21/thin_vec/)|对象指针大小，空值不分配；len/cap 转移到非空堆分配头；不能与 Vec/Box/String 零成本往返|大量高空率字段值得测，例如 tool_calls 的容器；非空时总内存未必下降，不能直接解决正文 clone|
|[lasso](https://docs.rs/lasso/0.7.3/lasso/struct.Spur.html) / [string_cache](https://docs.rs/string_cache/0.11.0/string_cache/struct.Atom.html)|Spur 为 4-byte interner 句柄；Atom 为 8-byte 表示，含静态/短串/动态驻留路径|只有集合有界且重复率足够高的词汇值得评估；必须计入表/arena 开销、并发与生命周期，不驻留用户正文或任意 prompt cache key|
|[Bytes](https://docs.rs/bytes/1.12.1/bytes/struct.Bytes.html)|cheap clone/slice，Vec buffer 可转交所有权；slice 保留 backing|项目已使用；优先检查所有者生命周期，而不是继续机械替换 Vec|

EcoString 的 [From<String>](https://docs.rs/ecow/0.3.1/src/ecow/string.rs.html#523-530) 转换在长字符串时需要重新分配；迁移时应尽量在解析/构造阶段直接建立目标表示，避免每个边界往返。其 [serde feature](https://docs.rs/crate/ecow/0.3.1/features) 需要显式启用；64-bit big-endian 上大小/内联容量不同。长字符串共享的收益必须同时计入输入转换与修改分离的峰值，不能只量 clone。

`Box<str>` 的近似净节省为 `8 + old_capacity - len`（x64、忽略对齐/allocator）；`Box<[T]>` 为 `8 + (capacity - len) * size_of::<T>()`。String 的 heap 成本是 capacity，不是 len + capacity。共享 payload 的收益取决于同时存活副本数，而不是调用 clone 的累计次数。

不建议仅为省内存改成 [hashbrown](https://docs.rs/hashbrown/0.17.1/hashbrown/struct.HashMap.html)、[IndexMap](https://docs.rs/indexmap/2.14.2/indexmap/map/struct.IndexMap.html) 或 [RoaringBitmap](https://docs.rs/roaring/0.11.5/roaring/bitmap/struct.RoaringBitmap.html)：分别解决哈希表、顺序映射、u32 集合问题；后者不保留重复和序列顺序。替换 hasher 不解决重复所有权，且 hashbrown 默认 hasher 的 HashDoS 边界不同，不能降低不可信输入的保护。

## 本轮实际内存测量

### 方法与限制

运行独立临时 Rust crate，以 path dependency 编译当前 `stravia-runtime-contract`；没有修改仓库依赖或产品代码。Linux x64、`rustc 1.99.1`、release 构建。探针用 `GlobalAlloc` 包装 `System`，测构造完成时仍存活的**请求分配字节与分配块数**；不计对象的栈空间、allocator 元数据/碎片、mimalloc 页保留、Wasm 堆或 RSS。每例销毁后核对两种计数均恢复基线。

直接依赖锁定 `compact_str 0.9.1`、`smallvec 1.16.2`、`bytes 1.12.1`、`uuid 1.28.0`；探针解析到的 serde 1.0.229 / serde_json 1.0.154 与仓库锁文件相同。隔离 crate 的 feature graph 不代表 Server/Desktop 所有特性合并后的布局；除标准库明确保证外，`size_of` 是该构建的测量值，不是跨版本 ABI 承诺。

模型索引和 affinity 为**按源码字段复刻的数据形状**，不是直接调用私有 Core 类型。只含合成输入，没有读取数据库/配置中的用户数据。第二轮索引实验显式消除 `format!` 的多余 String capacity，使基线也是 `len == capacity`；下表只使用该公平比较结果。未测吞吐、CPU 回归、真实 HTTP 续接或进程 RSS，不能把分配减少比例写成产品整体内存降幅。

实际运行命令（临时目录已在研究完成后移除，完整重建材料见附录）：

```sh
rustc --version
cargo --version
cargo run --offline --release --manifest-path /tmp/stravia-memory-probe-uqi7ywxx/Cargo.toml
cargo run --offline --release --manifest-path /tmp/stravia-memory-probe-uqi7ywxx/Cargo.toml -- indexes
```

### 索引：不需要新库也有收益

ModelIndex 输入为 10,000 个不同的 20-byte 模型名，各有 exact 和 normalized 索引，不计 records 本体。Affinity 输入为 10,000 个唯一 identity、共享相同 principal/route/target；principal/route/identity 长度均为 36，target 为 27 字节。这是用于分离重复所有权成本的合成样本，**不是平台 ID 格式或真实条目分布**。优化侧真正复用三个 Arc 所有者，而非为每条字符串重新 `Arc::from`。

|数据形状|存活请求分配字节|存活分配块数|相对本组基线减少|
|---|---:|---:|---:|
|模型索引：String + Option<usize>|1,612,448|20,002|—|
|模型索引：Box<str> + Option<NonZeroUsize>|1,219,232|20,002|24.39%|
|模型索引：CompactString + Option<NonZeroUsize>|1,081,376|2|32.94%|
|Affinity：全部独立 String|3,070,336|40,001|—|
|Affinity：重复字段共享 Arc<str>，唯一 identity 用 Box<str>|1,556,208|10,004|49.31%|

模型名超过内联容量时，CompactString 的分配块收益会消失；本轮的 36-byte 独立字符串实验中，它和 String 均为 6,000,000 B / 100,001 块（100,000 个元素）。不能按 20-byte 样本承诺任意模型目录节省三分之一。

### 当前 canonical 类型的 clone 成本

下面直接构造项目的 `ContentBlock`，保留 **1 个原件 + 8 个 clone**，每项正文 1 MiB，容器为预留 9 项的 Vec。8 是实验参数，不是源码已证明的生产同时副本数；ToolUse 的 Value 是单个大字符串，复杂 JSON 树需另测。

|真实类型/变体|存活请求分配字节|存活分配块数|
|---|---:|---:|
|Text（已有 Arc<String>）|1,049,624|3|
|Thinking（String）|9,438,192|10|
|Image/Base64（String）|9,438,273|19|
|ToolUse input（Value::String）|9,438,282|28|

这证明了不同字段的 clone 分配差异，不证明每个请求实际复制九份。共享的理论收益随**同时存活且内容相同的副本数**与 payload 大小增长；只读共享不会省去最终协议序列化、转义、Base64 或跨 WIT 复制。

### 容器反例与布局

|该构建中的类型|size_of，B|
|---|---:|
|String / CompactString|24|
|Box<str> / Arc<str>|16|
|Arc<String>|8|
|Bytes|32|
|Vec<ContentBlock>|24|
|SmallVec<[ContentBlock; 1]>|128|
|SmallVec<[ContentBlock; 2]>|240|
|ContentBlock|112|
|AiItem|88|
|AiStreamDelta|104|
|AiRequest|1,456|
|Option<usize>|16|
|Option<NonZeroUsize>|8|

- 100,000 个空 `Vec<ContentBlock>` 的外层 Vec 分配为 2,400,000 B；空 `SmallVec<[ContentBlock; 1]>` 为 12,800,000 B。每项恰有一个无 payload block 时，前者为 13,600,000 B / 100,001 块，后者为 12,800,000 B / 1 块。应先测 empty/single/multi 分布，不能只看到“少分配”。
- 1 MiB 的 `Bytes` 仅保留 16-byte slice，仍保留 1,048,600 B / 2 块；显式复制该小片段后为 16 B / 1 块。零拷贝不等于低保留内存，现有 Trace 入队前编码正有这类生命周期考虑。
- 同样 1 MiB 正文、9 个共享句柄，`Arc<String>` 原型为 1,048,688 B / 3 块，`Arc<str>` 为 1,048,736 B / 2 块。后者少一个分配，但 fat-pointer 句柄更多；不能只比较 header 就全局替换。

## 后续验证门槛

1. 私有模型索引试点：覆盖 exact 优先、normalized 碰撞、索引 0、空目录与长模型名；使用真实形状的合成目录比较内存、查询和重建成本。
2. 大正文共享试点：沿现有 HTTP 基准做同负载配对回放，保留普通历史、Debug、凭据保护、Hook、跨协议与续接行为；同时观察 allocator 分配与进程 RSS，不能只看 `size_of`。
3. affinity：先量化 unique key 数与复用率；表示优化和生命周期/淘汰决策分开。不因节省空间改变健康世代、半开探测或软亲和优先级。
4. Live/前端：用纯合成对话、图与慢订阅者测量；验证切换/卸载后的保留堆、revision 隔离、gap、排序、图片与历史回放。未测前不宣称已优化。

本轮只交付研究文档；不改 API、数据库、TTL、安全策略、依赖清单或锁文件。不新增永久测试；未运行仓库测试、服务端或浏览器。

## 附录：隔离探针复现材料

以下是本轮执行的最终探针源码，不是产品实现或永久测试。将 manifest 中 `<repo>` 替换为仓库绝对路径，在仓库外临时目录放置 `Cargo.toml` 与 `src/main.rs`。运行 `cargo run --release --manifest-path <probe>/Cargo.toml`；依赖均已缓存时可加 `--offline`。仅重跑公平容量索引比较时追加 `-- indexes`。本轮未测的库不在 manifest 中。

<details>
<summary>Cargo.toml</summary>

```toml
[package]
name = "stravia-memory-probe"
version = "0.0.0"
edition = "2024"

[dependencies]
stravia-runtime-contract = { path = "<repo>/backend/crates/stravia-runtime-contract" }
compact_str = "=0.9.1"
smallvec = "=1.16.2"
bytes = "=1.12.1"
uuid = "=1.28.0"
serde_json = "1"
```

</details>

<details>
<summary>src/main.rs</summary>

```rust
use std::{alloc::{GlobalAlloc, Layout, System}, sync::{Arc, atomic::{AtomicUsize, Ordering}}, mem::size_of, hint::black_box};
use stravia_runtime_contract::protocol::ir::{AiItem, AiRequest, ContentBlock, MessageContent, MediaSource, ToolCall, AiStreamDelta};
use compact_str::CompactString;
use smallvec::SmallVec;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static BLOCKS: AtomicUsize = AtomicUsize::new(0);
struct Count;
unsafe impl GlobalAlloc for Count {
 unsafe fn alloc(&self, l: Layout) -> *mut u8 { let p=unsafe{System.alloc(l)}; if !p.is_null(){ LIVE.fetch_add(l.size(),Ordering::Relaxed); BLOCKS.fetch_add(1,Ordering::Relaxed); } p }
 unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 { let p=unsafe{System.alloc_zeroed(l)}; if !p.is_null(){ LIVE.fetch_add(l.size(),Ordering::Relaxed); BLOCKS.fetch_add(1,Ordering::Relaxed); } p }
 unsafe fn dealloc(&self,p:*mut u8,l:Layout){ unsafe{System.dealloc(p,l)}; LIVE.fetch_sub(l.size(),Ordering::Relaxed); BLOCKS.fetch_sub(1,Ordering::Relaxed); }
 unsafe fn realloc(&self,p:*mut u8,l:Layout,n:usize)->*mut u8{let q=unsafe{System.realloc(p,l,n)};if !q.is_null(){if n>=l.size(){LIVE.fetch_add(n-l.size(),Ordering::Relaxed);}else{LIVE.fetch_sub(l.size()-n,Ordering::Relaxed);}}q}
}
#[global_allocator] static A: Count=Count;
fn measure<T>(name:&str, build:impl FnOnce()->T){let b=LIVE.load(Ordering::Relaxed);let n=BLOCKS.load(Ordering::Relaxed);let value=black_box(build());let bytes=LIVE.load(Ordering::Relaxed)-b;let blocks=BLOCKS.load(Ordering::Relaxed)-n;black_box(&value);drop(value);assert_eq!(LIVE.load(Ordering::Relaxed),b,"unreleased allocation");assert_eq!(BLOCKS.load(Ordering::Relaxed),n);println!("case,{name},{bytes},{blocks}");}
fn cloned_block(block:ContentBlock)->Vec<ContentBlock>{let mut v=Vec::with_capacity(9);for _ in 0..8{v.push(block.clone())}v.push(block);v}
fn main(){
 if std::env::args().any(|a| a == "indexes") { index_probe(); return; }
 println!("kind,name,bytes,live_allocations");
 macro_rules! sizes {($($t:ty),*)=>{$(println!("size,{},{}",stringify!($t),size_of::<$t>());)*}}
 sizes!(String,Box<str>,Arc<str>,Arc<String>,CompactString,bytes::Bytes,uuid::Uuid,Vec<u8>,Box<[u8]>,Vec<ContentBlock>,SmallVec<[ContentBlock;1]>,SmallVec<[ContentBlock;2]>,ContentBlock,MessageContent,AiItem,AiRequest,ToolCall,AiStreamDelta,serde_json::Value);
 const N:usize=100_000;
 for s in ["assistant", "01234567-89ab-cdef-0123-456789abcdef"] {
 measure(&format!("String len={} n={N}",s.len()), ||(0..N).map(|_|s.to_owned()).collect::<Vec<_>>());
 measure(&format!("Box<str> len={} n={N}",s.len()), ||(0..N).map(|_|Box::<str>::from(s)).collect::<Vec<_>>());
 measure(&format!("CompactString len={} n={N}",s.len()), ||(0..N).map(|_|CompactString::new(s)).collect::<Vec<_>>());
 }
 measure("Uuid n=100000", || (0..N).map(|i|uuid::Uuid::from_u128(i as u128)).collect::<Vec<_>>());
 measure("String hex digest len=64 n=100000", || (0..N).map(|_|"a".repeat(64)).collect::<Vec<_>>());
 measure("binary digest [u8;32] n=100000", ||vec![[0u8;32];N]);
 measure("Vec<ContentBlock> empty n=100000", ||(0..N).map(|_|Vec::<ContentBlock>::new()).collect::<Vec<_>>());
 measure("SmallVec<[ContentBlock;1]> empty n=100000", ||(0..N).map(|_|SmallVec::<[ContentBlock;1]>::new()).collect::<Vec<_>>());
 measure("Vec<ContentBlock> single trigger n=100000", ||(0..N).map(|_|vec![ContentBlock::CompactionTrigger{}]).collect::<Vec<_>>());
 measure("SmallVec<[ContentBlock;1]> single trigger n=100000", ||(0..N).map(|_|{let mut v=SmallVec::<[ContentBlock;1]>::new();v.push(ContentBlock::CompactionTrigger{});v}).collect::<Vec<_>>());
 const M:usize=1024*1024;
 measure("actual Text 1MiB original+8 clones", ||cloned_block(ContentBlock::Text{text:Arc::new("x".repeat(M)),cache_control:None}));
 measure("actual Thinking 1MiB original+8 clones", ||cloned_block(ContentBlock::Thinking{thinking:"x".repeat(M),signature:None}));
 measure("actual Base64 image 1MiB original+8 clones", ||cloned_block(ContentBlock::Image{source:MediaSource::Base64{media_type:"image/png".into(),data:"x".repeat(M)},detail:None,cache_control:None}));
 measure("actual ToolUse JSON string 1MiB original+8 clones", ||cloned_block(ContentBlock::ToolUse{id:"call_1".into(),name:"tool".into(),input:serde_json::Value::String("x".repeat(M)),cache_control:None}));
 measure("prototype Arc<String> 1MiB original+8 clones", ||{let s=Arc::new("x".repeat(M));vec![s;9]});
 measure("prototype Arc<str> 1MiB original+8 clones", ||{let s=Arc::<str>::from("x".repeat(M));vec![s;9]});
 measure("prototype Bytes 1MiB original+8 clones", ||{let s=bytes::Bytes::from(vec![0u8;M]);vec![s;9]});
 measure("Bytes retained 16B slice from 1MiB", ||{let s=bytes::Bytes::from(vec![0u8;M]);s.slice(0..16)});
 measure("Bytes copied 16B slice from 1MiB", ||{let s=bytes::Bytes::from(vec![0u8;M]);bytes::Bytes::copy_from_slice(&s[..16])});
 index_probe();
}

fn index_probe() {
 use std::{collections::HashMap, num::NonZeroUsize};
 const N:usize=10_000;
 println!("size,Option<usize>,{}",size_of::<Option<usize>>());
 println!("size,Option<NonZeroUsize>,{}",size_of::<Option<NonZeroUsize>>());
 measure("ModelIndex index-only String/Option<usize> n=10000", || {
  let mut exact=HashMap::with_capacity(N);let mut normalized=HashMap::with_capacity(N);
  for i in 0..N {let s=format!("model_{i:014}").into_boxed_str().into_string();exact.insert(s.clone(),i);normalized.insert(s,Some(i));}
  assert_eq!(exact.get("model_00000000000042"),Some(&42));
  (exact,normalized)
 });
 measure("ModelIndex index-only Box<str>/Option<NonZeroUsize> n=10000", || {
  let mut exact=HashMap::with_capacity(N);let mut normalized=HashMap::with_capacity(N);
  for i in 0..N {let s=format!("model_{i:014}");exact.insert(s.clone().into_boxed_str(),i);normalized.insert(s.into_boxed_str(),NonZeroUsize::new(i+1));}
  assert_eq!(normalized.get("model_00000000000042").copied().flatten().map(|v|v.get()-1),Some(42));
  (exact,normalized)
 });
 measure("ModelIndex index-only CompactString/Option<NonZeroUsize> n=10000", || {
  let mut exact=HashMap::with_capacity(N);let mut normalized=HashMap::with_capacity(N);
  for i in 0..N {let s=CompactString::new(format!("model_{i:014}"));exact.insert(s.clone(),i);normalized.insert(s,NonZeroUsize::new(i+1));}
  assert_eq!(normalized.get("model_00000000000042").copied().flatten().map(|v|v.get()-1),Some(42));
  (exact,normalized)
 });
 #[derive(Hash,PartialEq,Eq)] enum Identity<T>{Parent(T),Prompt(T)}
 let principal="01234567-89ab-cdef-0123-456789abcdef";
 let route="11234567-89ab-cdef-0123-456789abcdef";
 let target="provider:model-example-2026";
 measure("Affinity shape all String n=10000", || {
  let mut map=HashMap::with_capacity(N);
  for i in 0..N {let id=format!("{i:036}").into_boxed_str().into_string();let identity=if i%2==0{Identity::Parent(id)}else{Identity::Prompt(id)};map.insert((principal.to_owned(),route.to_owned(),identity),target.to_owned());}
  map
 });
 measure("Affinity shape shared Arc<str>+unique Box<str> n=10000", || {
  let principal=Arc::<str>::from(principal);let route=Arc::<str>::from(route);let target=Arc::<str>::from(target);
  let mut map=HashMap::with_capacity(N);
  for i in 0..N {let id=format!("{i:036}").into_boxed_str();let identity=if i%2==0{Identity::Parent(id)}else{Identity::Prompt(id)};map.insert((principal.clone(),route.clone(),identity),target.clone());}
  map
 });
}
```

</details>
