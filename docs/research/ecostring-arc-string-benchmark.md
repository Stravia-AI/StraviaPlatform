# EcoString 与 Arc<String> 实测对照

日期：2026-10-28。问题：EcoString 是否适合替换 Stravia 现有 `Arc<String>`，降低长正文、多快照场景的内存成本。背景：[内存结构调查](compact-memory-structures.md)。

## 结论

**不建议全局替换现有 `Arc<String>`，也不据此新增生产依赖。** 两者已经共享长正文，EcoString 并不会再省一份大 payload；本轮的多快照句柄数组甚至略大。项目已有 owned String 输入/输出边界，EcoString 在这些边界复制正文，转换峰值与耗时明显增加。

EcoString 的局部优势是：短串可以不分配；共享后追加能把 COW 分离与容量预留合并。本轮“反复追加并保留快照”的合成场景有存活分配收益，但它不等于只读多快照，也不能外推整个 Server RSS。

本轮只修改研究文档、生成隔离测试结果；未改产品代码、公开类型、数据库、生产依赖或仓库 Cargo.lock。没有使用用户数据库、对话、凭据或模型上游。

## 已执行的验证与口径

- 临时 crate 使用 **ecow 0.3.1 + serde**，并以 path dependency 使用当前 `stravia-runtime-contract 0.5.3`。
- Linux x86_64 小端，Intel Core i5-13600K，rustc 1.99.1，release；与 Server 相同的 **mimalloc 0.1.52 / no_thp**。采样进程固定到逻辑 CPU 0，不代表整台机器独占或固定频率。
- 两个独立可执行文件：`track` 构建用 GlobalAlloc 包装 mimalloc，只测分配；无 `track` 构建直接用 mimalloc，**耗时不受分配计数器插桩影响**。不是 debug、Miri 或 loom 构建。
- **224 个内存样本**：长度 0/8/15/16/28/64/1 KiB/64 KiB/1 MiB/8 MiB，涵盖构造、1/9/33 句柄、共享改写/追加、独占追加、owned 往返、JSON 输入/输出，以及独占/带快照的流式累积。每例释放后都核对存活字节和块数恢复基线。
- **1,332 个计时样本**：148 个类型/操作/长度组，每组 9 个批次；配对执行并轮换类型顺序。批次前预热，black_box 防止消除；迭代数按较慢类型预估并限制输入占用。同一对使用相同迭代数，1 MiB 多数操作每批 64 次；clone+drop 每批 87,719 次，JSON 输出每批 49 次。下文是每次操作的批次中位数及 min/max，不是置信区间。
- **正确性烟测通过**：9 种输入，包含空串、15/16-byte 边界、Unicode、换行/引号/反斜杠、长至 8 MiB；检查 serde 双向兼容、共享后修改不改变原件、累积快照内容和顺序。隔离 EcoString Text 变体的 JSON 与项目真实 `ContentBlock::Text` 字节一致，并可由当前 ContentBlock 解码。没有把这个单变体检查扩大为全部 canonical/WIT 兼容证明。

### 指标边界

“存活请求分配”包含 Vec 中实际保存的句柄槽位、共享头和正文；不包含栈上的 Vec 头、测量外预备的借用输入、allocator 元数据/内部对齐/碎片或保留页。每个例子单独构造与释放，不把累计申请量当作保留量。

“API 层峰值”来自 GlobalAlloc 可见 alloc/dealloc/realloc；**看不到 allocator 内部 realloc 期间旧/新块短暂共存**。累计请求字节把 realloc 的新 size 也计入，既不是实际复制字节，也不是物理分配页数。这些都不是 RSS。

句柄比较是 `Vec<Arc<String>>` 与 `Vec<EcoString>`，没有把生产 ContentBlock 改型；真实 enum 的 padding 可能吸收字段差异，不能用每句柄差额直接推算整个 canonical 对象布局。性能载荷为合成 ASCII，JSON 无转义；Unicode/转义只做了正确性检查。未测多线程引用计数争用、HTTP/Wasm/WIT、真实凭据保护、浏览器或进程 RSS。

## 内存：长正文多快照没有净优势

|输入/操作|指标|Arc<String>，B|EcoString，B|
|---|---|---:|---:|
|1 MiB，从 &str 构造一份|存活请求分配|1,048,624|1,048,608|
|1 MiB，原件 + 8 个 clone|存活请求分配|1,048,688|1,048,736|
|1 MiB，原件 + 32 个 clone|存活请求分配|1,048,880|1,049,120|
|1 MiB，从 owned String 转入|API 层峰值|1,048,624|2,097,168|
|1 MiB，从 owned String 转入并保留 9 份|API 层峰值|1,048,688|2,097,168|
|1 MiB，9 份中改写一份的首字符|存活请求分配|2,097,304|2,097,328|
|1 MiB，9 份中对一份追加 7 B|存活请求分配|3,145,880|3,145,904|
|同上，累计请求量|累计分配/重分配请求|4,194,456|3,145,904|
|1 MiB，String → 共享类型 → String|累计分配/重分配请求|1,048,616|3,145,744|

原因是两者都只保留共享正文。EcoString 的长串分配把头和字节放在同一块，少一个堆块，但句柄较宽；把已有 owned String 转入又需要重新分配并复制。9 句柄、1 MiB 实验的存活块数是 Arc 的 3 块与 Eco 的 2 块，**少一块分配不等于明显少字节**。

共享后追加的存活容量接近，但 EcoString 累计请求量更低。其 `push_str` 在 COW 分离时已为追加预留足够容量；当前对照是 `Arc::make_mut(...).push_str(...)`，共享时先克隆 String，再增长。这解释的是特定增长路径，不是纯 clone 优势。

### 短串与反复追加

|输入/操作|指标|Arc<String>，B|EcoString，B|
|---|---|---:|---:|
|15 B，一份|存活请求分配|63|16|
|15 B，原件 + 8 个 clone|存活请求分配|127|144|
|16 B，一份|存活请求分配|64|48|
|28 B，一份|存活请求分配|76|60|
|28 B，原件 + 8 个 clone|存活请求分配|140|188|
|独占累积 128×32 B|存活请求分配|4,144|4,128|
|每 4 次追加保留快照，最终 33 个句柄|存活请求分配|128,648|88,464|
|同上，累计请求量|累计分配/重分配请求|192,232|88,592|

短串单份有明显优势；同一短串被多份 Arc 共享后，更多内联副本也可能比共享表示大。流式累积样本每次追加 32 B、共 128 次；带快照版本每 4 次保留一份，共 32 个历史快照加最终句柄。两种类型保持完全相同的快照内容；不能通过少存快照取得结果。

## 耗时：owned String 边界是主要反例

下表正文均为 1 MiB。纯 clone 行包含 **8 次 clone 及这些 clone 的 drop**，不含构造。COW 行保留原件，修改临时 clone。所有操作均包含相应结果的释放。

|操作|Arc<String> 中位数 [min, max]，µs|EcoString 中位数 [min, max]，µs|
|---|---:|---:|
|从 &str 构造并释放|23.871 [23.444, 24.507]|24.560 [24.385, 25.462]|
|从已备好的 String 转入并释放|0.221 [0.196, 0.341]|72.445 [66.557, 76.411]|
|独占值转回 String 并释放|0.213 [0.186, 0.273]|63.895 [61.915, 70.739]|
|8 次 clone + drop，不复制正文|0.077 [0.071, 0.077]|0.071 [0.071, 0.071]|
|共享后改首个 ASCII 字符|22.575 [22.282, 89.503]|22.101 [21.929, 22.904]|
|共享后追加 7 B|58.556 [58.007, 62.568]|23.662 [23.458, 24.116]|
|共享值转回 String 并释放|22.499 [22.234, 22.797]|22.069 [21.816, 22.343]|
|serde JSON 输出|268.724 [268.046, 342.752]|267.380 [265.496, 411.178]|
|serde JSON 输入|94.185 [93.135, 95.271]|95.235 [94.102, 96.273]|

`owned_in` 的输入 String 在计时前批量准备，测转换本身；`unique_out` 的共享类型也在计时前准备，测转回 String。两者可能处理多份不同 buffer；`borrowed_in` 重复读取同一源字符串。因此**只在同一行比较类型，不跨行推算总链路时间**。Arc 独占移入/移出不扫描 payload，EcoString 必须复制；这与当前 codec 的 owned 边界直接相关。

纯 clone、同长度改写和 JSON 编解码没有足以支持产品切换的明显优势；小差异与长尾不作稳定加速承诺。共享追加在本轮结果中更快，但这条路径也可以先调查现有 Arc/String 的容量管理，不能直接归结为必须引入新库。

### 流式样本耗时

|操作|Arc<String> 中位数 [min, max]，µs|EcoString 中位数 [min, max]，µs|
|---|---:|---:|
|独占累积 128×32 B|1.340 [1.309, 1.425]|1.107 [1.098, 1.155]|
|每 4 次追加保留快照，最终保留 33 个句柄|18.230 [5.452, 41.171]|4.567 [4.235, 25.346]|

带快照样本波动明显，两种类型均有高尾值；不能把中位数之比宣传为稳定加速倍数。其分配字节差异是本轮更可靠的证据。单独以“边追加边保留快照且很少转回 String”的私有缓冲作为候选，才有继续做真实负载验证的价值。

## 一手源码解释

|行为|契约与来源|对本项目的含义|
|---|---|---|
|长串 clone|EcoString/EcoVec 与 Arc 均为原子引用计数增加；[EcoVec clone/drop](https://github.com/typst/ecow/blob/v0.3.1/src/vec.rs#L891-L923)、[Arc](https://doc.rust-lang.org/1.99.1/std/sync/struct.Arc.html)|现有共享 Text 不会从 O(n) 变成 O(1)，因为它已经是 O(1)|
|String → EcoString|大于内联容量时重新分配并复制；[From<String>](https://github.com/typst/ecow/blob/v0.3.1/src/string.rs#L523-L530)|不能接管 codec 已拥有的 String buffer|
|EcoString → String|构造 owned String，需要复制；[From<EcoString>](https://github.com/typst/ecow/blob/v0.3.1/src/string.rs#L553-L558)|没有 Arc::unwrap_or_clone 独占时直接移出 String 的等价路径|
|共享后追加|reserve 先决定目标容量，shared 时按该容量分配并复制；[EcoVec::reserve](https://github.com/typst/ecow/blob/v0.3.1/src/vec.rs#L501-L526)|本次 COW 可带足 extra，避免“先 clone 再增长”的一次额外请求；实际 CPU 仍需实测|
|serde|Serialize 借用 as_str；Deserialize visitor 没有 visit_string；[serde 实现](https://github.com/typst/ecow/blob/v0.3.1/src/string.rs#L684-L727)、[Visitor 默认行为](https://docs.rs/serde/1.0.229/serde/de/trait.Visitor.html#method.visit_string)|输入已经是 owned String 时仍会复制；本轮计时只覆盖 JSON 字符串输入，未单测 Value::String 反序列化|
|修改 API|EcoString::make_mut 返回 &mut str；[API](https://docs.rs/ecow/0.3.1/ecow/struct.EcoString.html)|不等价于当前 &mut String，变长改写接口不能直接替换|
|并发|EcoVec 使用 AtomicUsize，满足元素条件时 Send/Sync；[实现](https://github.com/typst/ecow/blob/v0.3.1/src/vec.rs#L870-L872)|支持跨线程共享，但本轮没有测多线程争用性能|

## 当前仓库的落点与决定

1. **保持现有 canonical Text 的 Arc<String>。** [Anthropic decoder](../../backend/crates/stravia-protocol-codec/src/codec/anthropic/messages/decoder.rs#L420) 将已有 String 移入 Arc；[Gemini encoder](../../backend/crates/stravia-protocol-codec/src/codec/google/gemini/encoder.rs#L348) 用 Arc::unwrap_or_clone 移出，并可能拼接。更换 EcoString 会把这些已优化的独占边界变成复制。
2. **不能直接替换改写 API。** [投影替换](../../backend/crates/stravia-core/src/proxy/dispatcher/inference_run/engine/projection.rs#L99) 使用 Arc::make_mut；[ProtectedSecrets::text](../../backend/crates/stravia-core/src/interaction_observation/redaction.rs#L112) 要求 &mut String，且可能重新序列化。这里需要接口和所有权设计，不是字段名替换。
3. **不要把本次结果误套到裸 String 的 Thinking/Base64。** [request.rs](../../backend/crates/stravia-runtime-contract/src/protocol/ir/request.rs#L37) 中这些字段仍可能深拷贝，改为共享表示依然值得验证；但本次没有证明 EcoString 优于直接复用 Arc<String>。应先消除深拷贝，再比较具体共享容器。
4. **EcoString 只保留为限定场景候选。** 如果私有缓冲从构造到消费都留在该类型内、确有短串或反复追加快照分布，再做真实路径配对实验。不要为复现微基准收益增加 String/EcoString 双路径、全局 interner 或降低历史保真。

## 实际命令与原始证据

执行并成功退出的命令（临时目录在完成后清理）：

```sh
cargo build --release --manifest-path /tmp/stravia-ecow-bench-4xw7wrak/Cargo.toml --features track --target-dir /tmp/stravia-ecow-bench-4xw7wrak/target-track
cargo build --release --manifest-path /tmp/stravia-ecow-bench-4xw7wrak/Cargo.toml --target-dir /tmp/stravia-ecow-bench-4xw7wrak/target-timing
# 固定探针容器容量后的增量构建使用相同命令并加 --offline --locked。
/tmp/stravia-ecow-bench-4xw7wrak/target-timing/release/stravia-ecow-bench correctness
taskset -c 0 /tmp/stravia-ecow-bench-4xw7wrak/target-track/release/stravia-ecow-bench memory
taskset -c 0 /tmp/stravia-ecow-bench-4xw7wrak/target-timing/release/stravia-ecow-bench timing
```

构建有探针条件编译产生的 unused import/associated constant 警告，未以此声称 lint 通过；所有执行与正确性断言通过。没有运行仓库全量测试或服务端。

原始文件位于 `target/benchmarks/ecostring-arc-string-4xw7wrak/`：`memory.csv`、`timing.csv`、`metadata.json`、隔离 crate 的 `Cargo.lock`。该目录是本地 benchmark 结果，不是生产依赖修改；metadata 记录 source/CSV/可执行文件 SHA-256。完整结果行数及口径已在上文说明，不能只选择有利 case。

## 附录：完整复现探针

在仓库外新建临时 crate，按下列 manifest/source 构建；将 `<repo>` 替换成当前仓库绝对路径。需要严格复用本轮依赖解析时，将上述结果目录中的 Cargo.lock 放入该临时 crate 并加 `--locked`。计数与计时必须使用不同的 target-dir/feature 构建，分别执行 `correctness`、`memory`、`timing`。CPU 编号应属于当前进程允许的 affinity 集；本轮使用 0。

<details>
<summary>Cargo.toml</summary>

```toml
[package]
name = "stravia-ecow-bench"
version = "0.0.0"
edition = "2024"

[features]
track = []

[dependencies]
ecow = { version = "=0.3.1", features = ["serde"] }
mimalloc = { version = "=0.1.52", features = ["no_thp"] }
serde = { version = "=1.0.229", features = ["derive", "rc"] }
serde_json = "=1.0.154"
stravia-runtime-contract = { path = "<repo>/backend/crates/stravia-runtime-contract" }
```

</details>

<details>
<summary>src/main.rs（本轮已执行的最终源码）</summary>

```rust
use ecow::EcoString;
use serde::{Serialize, de::DeserializeOwned};
use std::{hint::black_box, sync::Arc, time::Instant};

#[cfg(not(feature = "track"))]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(feature = "track")]
mod tracked {
    use std::alloc::{GlobalAlloc, Layout};
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    pub static LIVE: AtomicUsize = AtomicUsize::new(0);
    pub static BLOCKS: AtomicUsize = AtomicUsize::new(0);
    pub static PEAK: AtomicUsize = AtomicUsize::new(0);
    pub static BYTES: AtomicUsize = AtomicUsize::new(0);
    pub static CALLS: AtomicUsize = AtomicUsize::new(0);
    pub struct Counting;
    fn add(n: usize) {
        let live = LIVE.fetch_add(n, Relaxed) + n;
        PEAK.fetch_max(live, Relaxed);
        BYTES.fetch_add(n, Relaxed);
        CALLS.fetch_add(1, Relaxed);
    }
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let p = unsafe { mimalloc::MiMalloc.alloc(layout) };
            if !p.is_null() { add(layout.size()); BLOCKS.fetch_add(1, Relaxed); }
            p
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let p = unsafe { mimalloc::MiMalloc.alloc_zeroed(layout) };
            if !p.is_null() { add(layout.size()); BLOCKS.fetch_add(1, Relaxed); }
            p
        }
        unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
            unsafe { mimalloc::MiMalloc.dealloc(p, layout) };
            LIVE.fetch_sub(layout.size(), Relaxed);
            BLOCKS.fetch_sub(1, Relaxed);
        }
        unsafe fn realloc(&self, p: *mut u8, old: Layout, new: usize) -> *mut u8 {
            let q = unsafe { mimalloc::MiMalloc.realloc(p, old, new) };
            if !q.is_null() {
                LIVE.fetch_sub(old.size(), Relaxed);
                add(new);
            }
            q
        }
    }
}
#[cfg(feature = "track")]
#[global_allocator]
static ALLOC: tracked::Counting = tracked::Counting;

trait Text: Clone + Serialize + DeserializeOwned {
    const NAME: &'static str;
    fn borrowed(s: &str) -> Self;
    fn owned(s: String) -> Self;
    fn text(&self) -> &str;
    fn append(&mut self, s: &str);
    fn edit_first(&mut self);
    fn into_owned(self) -> String;
}
impl Text for Arc<String> {
    const NAME: &'static str = "Arc<String>";
    fn borrowed(s: &str) -> Self { Arc::new(s.to_owned()) }
    fn owned(s: String) -> Self { Arc::new(s) }
    fn text(&self) -> &str { self.as_str() }
    fn append(&mut self, s: &str) { Arc::make_mut(self).push_str(s); }
    fn edit_first(&mut self) {
        if !self.is_empty() { Arc::make_mut(self).get_mut(..1).unwrap().make_ascii_uppercase(); }
    }
    fn into_owned(self) -> String { Arc::unwrap_or_clone(self) }
}
impl Text for EcoString {
    const NAME: &'static str = "EcoString";
    fn borrowed(s: &str) -> Self { Self::from(s) }
    fn owned(s: String) -> Self { Self::from(s) }
    fn text(&self) -> &str { self.as_str() }
    fn append(&mut self, s: &str) { self.push_str(s); }
    fn edit_first(&mut self) {
        if !self.is_empty() { self.make_mut().get_mut(..1).unwrap().make_ascii_uppercase(); }
    }
    fn into_owned(self) -> String { self.into() }
}

struct Held<T> { _values: Vec<T>, _wire: Vec<u8>, _owned: Option<String> }
fn build<T: Text>(case: &str, source: &str, json: &str) -> Held<T> {
    let mut out = Held { _values: Vec::new(), _wire: Vec::new(), _owned: None };
    if case == "deserialize" {
        out._values = Vec::with_capacity(1);
        out._values.push(serde_json::from_str::<T>(json).unwrap());
        return out;
    }
    if case.starts_with("stream") {
        let mut text = T::borrowed("");
        out._values = Vec::with_capacity(if case == "stream_snapshots" { 33 } else { 1 });
        for i in 0..128 {
            text.append("abcdefghijklmnopqrstuvwxyz012345");
            if case == "stream_snapshots" && i % 4 == 3 { out._values.push(text.clone()); }
        }
        out._values.push(text);
        return out;
    }
    let text = if matches!(case, "owned_in" | "owned_roundtrip" | "snapshots_owned") {
        T::owned(source.to_owned())
    } else { T::borrowed(source) };
    if case == "owned_roundtrip" { out._owned = Some(text.into_owned()); return out; }
    let copies = match case { "snapshots33" => 33, "snapshots9" | "snapshots_owned" | "cow_edit" | "cow_append" => 9, _ => 1 };
    out._values = Vec::with_capacity(copies);
    for _ in 1..copies { out._values.push(text.clone()); }
    out._values.push(text);
    match case {
        "cow_edit" => out._values.last_mut().unwrap().edit_first(),
        "cow_append" | "unique_append" => out._values.last_mut().unwrap().append(" suffix"),
        "serialize" => out._wire = serde_json::to_vec(out._values.last().unwrap()).unwrap(),
        _ => (),
    }
    out
}

fn correctness() {
    use stravia_runtime_contract::protocol::ir::{CacheControl, ContentBlock};
    #[derive(Serialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum EcoBlock {
        Text { text: EcoString, #[serde(skip_serializing_if = "Option::is_none")] cache_control: Option<CacheControl> },
    }
    let mut inputs: Vec<String> = ["", "a", "你好🙂\n\t\"\\", "abcdefghijklmno", "abcdefghijklmnop"]
        .into_iter().map(str::to_owned).collect();
    inputs.extend([28, 1024, 1024 * 1024, 8 * 1024 * 1024].into_iter().map(|n| "x".repeat(n)));
    for source in &inputs {
        let arc = Arc::<String>::owned(source.clone());
        let eco = EcoString::owned(source.clone());
        let encoded_arc = serde_json::to_vec(&arc).unwrap();
        let encoded_eco = serde_json::to_vec(&eco).unwrap();
        assert_eq!(encoded_arc, encoded_eco);
        assert_eq!(serde_json::from_slice::<EcoString>(&encoded_arc).unwrap().as_str(), source);
        assert_eq!(serde_json::from_slice::<Arc<String>>(&encoded_eco).unwrap().as_str(), source);
        let mut arc_changed = arc.clone();
        let mut eco_changed = eco.clone();
        arc_changed.append("追加\"\\\n"); eco_changed.append("追加\"\\\n");
        assert_eq!(arc_changed.text(), eco_changed.text());
        assert_eq!(arc.text(), source); assert_eq!(eco.text(), source);
        assert_eq!(arc_changed.into_owned(), eco_changed.into_owned());
        let current = ContentBlock::Text { text: arc, cache_control: None };
        let alternate = EcoBlock::Text { text: eco, cache_control: None };
        let current_wire = serde_json::to_vec(&current).unwrap();
        let alternate_wire = serde_json::to_vec(&alternate).unwrap();
        assert_eq!(current_wire, alternate_wire);
        let restored: ContentBlock = serde_json::from_slice(&alternate_wire).unwrap();
        assert_eq!(serde_json::to_vec(&restored).unwrap(), current_wire);
    }
    let source = "x".repeat(65536);
    let json = serde_json::to_string(&source).unwrap();
    for case in ["cow_edit", "cow_append", "stream_unique", "stream_snapshots"] {
        let arc = build::<Arc<String>>(case, &source, &json);
        let eco = build::<EcoString>(case, &source, &json);
        assert_eq!(arc._values.len(), eco._values.len());
        for (a, e) in arc._values.iter().zip(&eco._values) { assert_eq!(a.text(), e.text()); }
        if case.starts_with("cow") { assert_eq!(arc._values[0].text(), source); }
        if case == "stream_snapshots" {
            for (i, value) in arc._values[..32].iter().enumerate() { assert_eq!(value.len(), (i+1)*128); }
        }
    }
    println!("correctness,passed,unicode-escaping-inline-boundaries-cow-snapshots-canonical-json,{}", inputs.len());
}

#[cfg(feature = "track")]
fn memory_one<T: Text>(case: &str, source: &str, json: &str) {
    use tracked::*;
    use std::sync::atomic::Ordering::Relaxed;
    let before = LIVE.load(Relaxed);
    let blocks = BLOCKS.load(Relaxed);
    let total = BYTES.load(Relaxed);
    let calls = CALLS.load(Relaxed);
    PEAK.store(before, Relaxed);
    let held = black_box(build::<T>(case, source, json));
    let retained = LIVE.load(Relaxed) - before;
    let live_blocks = BLOCKS.load(Relaxed) - blocks;
    let peak = PEAK.load(Relaxed) - before;
    let allocated = BYTES.load(Relaxed) - total;
    let allocations = CALLS.load(Relaxed) - calls;
    black_box(&held);
    drop(held);
    assert_eq!(LIVE.load(Relaxed), before);
    assert_eq!(BLOCKS.load(Relaxed), blocks);
    println!("{case},{},{},{retained},{live_blocks},{peak},{allocated},{allocations}", source.len(), T::NAME);
}
#[cfg(feature = "track")]
fn memory() {
    println!("scenario,len,type,retained_bytes,live_blocks,peak_bytes,requested_bytes,allocation_calls");
    for len in [0, 8, 15, 16, 28, 64, 1024, 65536, 1048576, 8388608] {
        let source = "x".repeat(len);
        let json = serde_json::to_string(&source).unwrap();
        for case in ["borrowed_in", "owned_in", "snapshots9", "snapshots33", "snapshots_owned", "cow_edit", "cow_append", "unique_append", "owned_roundtrip", "deserialize", "serialize"] {
            memory_one::<Arc<String>>(case, &source, &json);
            memory_one::<EcoString>(case, &source, &json);
        }
    }
    for case in ["stream_unique", "stream_snapshots"] {
        memory_one::<Arc<String>>(case, "", "\"\"");
        memory_one::<EcoString>(case, "", "\"\"");
    }
}

#[cfg(not(feature = "track"))]
fn time_batch<T: Text>(case: &str, source: &str, json: &str, iterations: usize) -> f64 {
    if case == "owned_in" {
        let inputs: Vec<_> = (0..iterations).map(|_| source.to_owned()).collect();
        let start = Instant::now();
        for input in inputs { drop(black_box(T::owned(black_box(input)))); }
        return start.elapsed().as_nanos() as f64 / iterations as f64;
    }
    if case == "unique_out" {
        let inputs: Vec<_> = (0..iterations).map(|_| T::owned(source.to_owned())).collect();
        let start = Instant::now();
        for input in inputs { drop(black_box(input.into_owned())); }
        return start.elapsed().as_nanos() as f64 / iterations as f64;
    }
    let root = T::borrowed(source);
    let start = Instant::now();
    for _ in 0..iterations {
        match case {
            "borrowed_in" => drop(black_box(T::borrowed(black_box(source)))),
            "clone_drop8" => {
                let copies: [T; 8] = std::array::from_fn(|_| black_box(&root).clone());
                black_box(&copies); drop(copies);
            }
            "cow_edit" => { let mut v = root.clone(); v.edit_first(); drop(black_box(v)); }
            "cow_append" => { let mut v = root.clone(); v.append(" suffix"); drop(black_box(v)); }
            "shared_out" => drop(black_box(root.clone().into_owned())),
            "deserialize" => drop(black_box(serde_json::from_str::<T>(black_box(json)).unwrap())),
            "serialize" => drop(black_box(serde_json::to_vec(black_box(&root)).unwrap())),
            "stream_unique" | "stream_snapshots" => drop(black_box(build::<T>(case, "", "\"\""))),
            _ => panic!("unknown timing case"),
        }
    }
    start.elapsed().as_nanos() as f64 / iterations as f64
}
#[cfg(not(feature = "track"))]
fn timing() {
    println!("scenario,len,type,iterations,round,ns_per_operation");
    for len in [8, 15, 16, 28, 64, 65536, 1048576, 8388608] {
        let source = "x".repeat(len);
        let json = serde_json::to_string(&source).unwrap();
        for case in ["borrowed_in", "owned_in", "unique_out", "clone_drop8", "cow_edit", "cow_append", "shared_out", "deserialize", "serialize"] {
            let a = time_batch::<Arc<String>>(case, &source, &json, 4);
            let e = time_batch::<EcoString>(case, &source, &json, 4);
            let cap = if case == "clone_drop8" { 500_000 } else { (64*1024*1024 / len.max(64)).clamp(4, 500_000) };
            let count = ((15_000_000.0 / a.max(e).max(1.0)) as usize).clamp(4, cap);
            // 相同迭代数、九个配对批次、轮换执行顺序；计时构建不含分配计数器。
            for round in 0..9 {
                let (arc, eco) = if round % 2 == 0 {
                    (time_batch::<Arc<String>>(case, &source, &json, count), time_batch::<EcoString>(case, &source, &json, count))
                } else {
                    let eco = time_batch::<EcoString>(case, &source, &json, count);
                    (time_batch::<Arc<String>>(case, &source, &json, count), eco)
                };
                println!("{case},{len},Arc<String>,{count},{round},{arc:.3}");
                println!("{case},{len},EcoString,{count},{round},{eco:.3}");
            }
        }
    }
    for case in ["stream_unique", "stream_snapshots"] {
        for round in 0..9 {
            let count = 2000;
            let (arc, eco) = if round % 2 == 0 {
                (time_batch::<Arc<String>>(case, "", "\"\"", count), time_batch::<EcoString>(case, "", "\"\"", count))
            } else {
                let eco = time_batch::<EcoString>(case, "", "\"\"", count);
                (time_batch::<Arc<String>>(case, "", "\"\"", count), eco)
            };
            println!("{case},0,Arc<String>,{count},{round},{arc:.3}");
            println!("{case},0,EcoString,{count},{round},{eco:.3}");
        }
    }
}
fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "correctness".into());
    match mode.as_str() {
        "correctness" => correctness(),
        #[cfg(feature = "track")] "memory" => memory(),
        #[cfg(not(feature = "track"))] "timing" => timing(),
        _ => panic!("mode does not match allocator build"),
    }
}
```

</details>
