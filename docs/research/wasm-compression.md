# Wasm 插件压缩与分发评估

日期：2026-11-10。对象：当前工作树构建的 Stravia `0.6.4` 自包含 `wasm32-wasip2` Component，不是浏览器运行的前端 Wasm。本次只做测量与研究，不修改产品代码、公共接口、发布流程或依赖。

## 结论

1. **压缩体积最小：XZ `-6e`。** 九个专属插件从 **53.076 MiB** 压到 **8.013 MiB**；先剥离函数名再压缩为 **7.301 MiB**，比最初减少 **86.24%**。本次所有十个组件上，`-6e` 与 `-9e` 压缩大小完全相同，但前者只需 8 MiB 字典，后者为 64 MiB；不推荐 `-9e`。
2. **当前手工下载、跨平台解包再导入：优先普通 ZIP。** 去函数名后合计 **12.018 MiB**，不是最小，但 Windows/macOS 图形界面解包方便，不需要改变当前导入器。它增加手工解包步骤，不能把 ZIP 直接拖入当前插件页。
3. **应用内自动解压不要求 Zstd；若只在安装/更新时解压一次，仍优先 XZ `-6e`。** 自动解压消除了用户手工解包的格式便利性差异，此时本次最小体积的 XZ 更符合下载分发目标。Zstd 19 去函数名后合计 **8.216 MiB**，比 XZ 多约 **0.915 MiB**，优势是解压更快且 Core 已有 `zstd` 依赖；仅在解压延迟或复用现有依赖优先时选择它。单个去函数名后的 Codex 为 XZ **0.859 MiB**、Zstd **0.967 MiB**，本机含进程与 I/O 开销的粗测解压约 **41 / 14 ms**；不应仅为一次性解压的这点差异默认牺牲压缩率。任何原生压缩导入仍须另行确认公开导入契约变更，本次没有实现该变更。
4. **不推荐为这批插件选 Brotli 11 或 Zstd 22 作为默认。** Brotli 11 比 XZ 大且编码更慢；Zstd 22 相对 19 的额外收益不足 0.1%。Brotli 仍适合已有 HTTP `br` 分发链路的其他场景，不能把它直接套用到当前本地插件安装。

验证：13 档原始产物方案、7 档去函数名后方案，共 **200 个组件/参数组合、1,600 次逐字节解压回环**全部一致；十个去函数名组件均通过真实 Wasmtime 加载、实例化与 descriptor 导出对比。未测完整供应商业务回归、网络分发、目标平台安装器与峰值 RSS。

## 当前分发边界

- [Vendor builder](../../backend/crates/stravia-vendor-base/src/bin/stravia-build-vendors.rs) 使用 `cargo build --locked --release --lib --target wasm32-wasip2`，移除宿主 `RUSTFLAGS` / `CARGO_ENCODED_RUSTFLAGS`，直接归档 Component，没有 strip 或外层压缩。根 [Cargo.toml](../../Cargo.toml) 未定义 release profile。
- [Release workflow](../../.github/workflows/release.yml) 单独发布九个专属插件的原始 `.wasm`，`base` 随宿主内嵌，不作为独立插件附件。因此主比较口径是九个插件**分别压缩后的大小之和**，不是把九个组件合并后压缩的大小。
- [管理上传入口](../../backend/apps/stravia-server/src/admin_routes/vendor_plugins.rs) 接收 `application/wasm` 原始字节，限制 64 MiB；[Core](../../backend/crates/stravia-core/src/plugin/manager.rs) 将字节直接送入插件准入。当前没有压缩包导入契约。
- `.wasm.br` / `.wasm.zst` / `.wasm.xz` / `.zip` 都需要先解压，再导入原始 `.wasm`。仅改后缀或 MIME 类型不能绕过这一要求。Wasmtime 的 [Component API](https://docs.rs/wasmtime/49.0.3/wasmtime/component/struct.Component.html) 接收 Component，而不是这些归档格式。
- 压缩传输不会等比例降低安装后的文件、guest 线性内存、JIT 镜像或运行时内存。`base` 单体的压缩结果也不能当作完整 Server/Desktop 安装包的节省量。

## 实测：九个发布插件分别压缩

原始输入合计 **53.076 MiB**。压缩后占比越低越好；时间为九个文件的中位数之和，未隔离 CPU，只作数量级参考。

| 方案 | 压缩后 MiB | 占原始体积 | 压缩合计秒 | 解压合计秒 |
|---|---:|---:|---:|---:|
| gzip -1 | 15.683 | 29.55% | 0.38 | 0.182 |
| gzip -9 | 13.133 | 24.74% | 3.31 | 0.186 |
| ZIP / DEFLATE -9 | 13.134 | 24.75% | 3.20 | 0.249 |
| Brotli 5 | 9.975 | 18.79% | 1.89 | 0.281 |
| Brotli 9 | 9.601 | 18.09% | 5.72 | 0.249 |
| Brotli 11 | 8.399 | 15.83% | 70.96 | 0.230 |
| Zstd 3 | 11.722 | 22.09% | 0.20 | 0.112 |
| Zstd 9 | 10.186 | 19.19% | 1.04 | 0.153 |
| Zstd 19 | 9.006 | 16.97% | 15.48 | 0.127 |
| Zstd 22 | 9.001 | 16.96% | 21.32 | 0.134 |
| XZ -6 | 8.062 | 15.19% | 16.02 | 0.451 |
| XZ -6e | 8.013 | 15.10% | 16.33 | 0.395 |
| XZ -9e | 8.013 | 15.10% | 21.10 | 0.425 |

可直接得出的结论：

- XZ `-9e` 比 Brotli 11 的下载体积再小 **4.60%**，编码成本也明显更低；但 Zstd 解压更快。Brotli 不是这批 Component 的最小格式。
- Zstd 22 对九个插件合计只比 19 少 **6,064 B**（约 0.064%），不值得作为默认分发档位。去函数名后，差距进一步缩小到 **2,694 B**。
- ZIP 比 gzip 多出的合计仅 **1,124 B**。若要用户手工解包，ZIP 的平台操作便利性比这点封装开销重要。
- XZ `-6` 相比 `-9e` 合计只多 **51,648 B**，不能只凭最高等级决定发布参数。
- 补测的 XZ `-6e` 在全部原始和去函数名组件上，与 `-9e` 大小完全相同。因此最小体积方案选择 `-6e`，不为没有实际收益的大字典买单。

### 去函数名后再压缩

| 方案 | 去函数名后压缩 MiB | 占最初原始体积 | 相比同算法未去函数名再减少 |
|---|---:|---:|---:|
| gzip -9 | 12.017 | 22.64% | 8.50% |
| ZIP / DEFLATE -9 | 12.018 | 22.64% | 8.50% |
| Brotli 11 | 7.661 | 14.43% | 8.79% |
| Zstd 19 | 8.216 | 15.48% | 8.77% |
| Zstd 22 | 8.214 | 15.48% | 8.74% |
| XZ -6e | 7.301 | 13.76% | 8.88% |
| XZ -9e | 7.301 | 13.76% | 8.88% |

去函数名使未压缩产物减少 **20.43%**，但叠加压缩后的额外收益只有约 **8.5–8.9%**；不能把两者的节省百分比直接相加。以单个 Codex 为例：原始 **6.146 MiB**，去函数名后 **4.919 MiB**；再用 XZ `-9e` 压到 **0.859 MiB**，或 Zstd 19 压到 **0.967 MiB**。

内嵌 `base` 作为独立参考：原始 **7.951 MiB**，去函数名后 **6.478 MiB**，再用 XZ `-9e` / Zstd 19 分别为 **1.168 / 1.311 MiB**；这不是宿主最终压缩包的实测结果。

## 方法与复现

构建入口：

```sh
task --color=false build:vendors:all
```

输入以 `target/vendor-plugins-all/manifest.json` 为准，每个输入先验证 SHA-256。十个组件均测量，包括作为独立参考的 `base`。压缩输出解压后逐字节与原文件比较。每个组合预热一次，计时取三次压缩、七次解压的中位数；汇总时间为九个独立文件的中位数之和，不是并行安装耗时。

环境：Linux x86_64 / WSL2，32 个逻辑 CPU；Rust `1.99.1`，gzip `1.14`，Brotli `1.2.0`，Zstandard `1.5.7`，XZ Utils `5.8.4`，Info-ZIP `3.0`。各编码器使用单线程；计时包括子进程启动、缓存文件读取、向父进程输出及 Python 接收字节，不是纯 codec 吞吐。测量期间存在其他评估任务，未进行 CPU 隔离，时间只作本机数量级参考，不能解释细小差异。体积不受这一限制影响。没有测量网络、冷盘、移动设备、峰值 RSS 或完整安装耗时。

所测命令（`input.wasm` 替换为同一个实际文件，输出由测量程序接收，不覆盖原件）：

```sh
gzip -n -1 -c input.wasm
gzip -n -9 -c input.wasm
zip -q -9 -X -j - input.wasm
brotli -q 5 -c input.wasm
brotli -q 9 -c input.wasm
brotli -q 11 -c input.wasm
zstd -q --single-thread -3 -c input.wasm
zstd -q --single-thread -9 -c input.wasm
zstd -q --single-thread -19 -c input.wasm
zstd -q --single-thread --ultra -22 -c input.wasm
xz -T1 -6 -c input.wasm
xz -T1 -6e -c input.wasm
xz -T1 -9e -c input.wasm
```

本机原始 JSON 留在 `target/wasm-compression-eval/`，不纳入版本控制；研究报告保留汇总、输入身份与命令。构建产物与测量文件不含用户数据，没有调用生产服务。

### 输入清单

MiB 均为 1,048,576 字节。九个专属插件原始合计 **53.076 MiB**（55,653,962 B），仅删除 core `name` 后为 **42.231 MiB**（44,282,490 B），减少 **20.43%**。`base` 不计入这一分发合计。

| Component | 原始 MiB | 去函数名 MiB | 原始体积减少 | 原始 SHA-256 |
|---|---:|---:|---:|---|
| base | 7.951 | 6.478 | 18.5% | `d3f810baad677b2388d2b9acabe40f3bb86b8691116756b15966ffc8df5cc5ce` |
| openai-codex | 6.146 | 4.919 | 20.0% | `f16abe42625a6434c0451e316b57ff23f39696912dab22d13e4520f5a23b6744` |
| xai-grok | 5.945 | 4.746 | 20.2% | `9eb9454986f9a6c8ced2f1fc532e94edcbbcd598d324c7a9e45a79879dbbb813` |
| command-code | 5.963 | 4.682 | 21.5% | `0d898a058a104272e00f05d714c587fb92e80f04b5f490893e4723e17d893117` |
| devin | 6.077 | 4.846 | 20.3% | `940ce4c05c572bd0e3dfb4b80e68762663eaae491e9139ef28c1b1ae79f02527` |
| opencode-free | 5.560 | 4.403 | 20.8% | `4c0809e4d36ba3371bbb4d61102ea8990a31c70230076b894ae75f0728afb049` |
| claude-code | 5.985 | 4.760 | 20.5% | `7ab37e443cf412b45ec485638097a80c381df4e87e24ade7e572561e69ff52a7` |
| antigravity | 5.939 | 4.759 | 19.9% | `31753739808d824d994879314b2cda4e511c8aa885989db17025363830002ae8` |
| muse-code | 5.830 | 4.651 | 20.2% | `bf4406d7314ff1c6c979bf9f542f985fd421c5fd5dd0935273f22bc36445bb4b` |
| kilo | 5.632 | 4.465 | 20.7% | `27c1b624601051411ec0678dd838295543274f658bff42a3954184705fc85381` |

## 产物自身瘦身与兼容性

当前组件包含约 1.2–1.5 MiB 的 core `name` section；它们是函数名等信息，不是 DWARF，不能因为 release 默认 `debug = false` 就认为没有可剥离的符号。采用明确的删除规则做实验：

```sh
wasm-tools strip -d '^name$' input.wasm -o stripped.wasm
wasm-tools validate stripped.wasm
wasm-tools component wit input.wasm
wasm-tools component wit stripped.wasm
```

测量工具为 `wasm-tools 1.260.0`。该规则递归去除 core 函数名，保留 `component-name`、`producers`、`target_features` 及其他 custom section。不要把默认 `wasm-tools strip` 当作同一操作：其默认保留 `name`；`-d` 是明确删除匹配项，而不是在默认规则上追加。见 [strip 源码](https://github.com/bytecodealliance/wasm-tools/blob/v1.260.0/src/bin/wasm-tools/strip.rs)。

十个剥离组件全部通过 `wasm-tools validate`，剥离前后的 `component wit` 输出完全相同。递归比对还确认：除删除的 `name` section 及随之改变的外层长度外，代码、数据、接口和其他 section 字节不变。

临时 `compression-smoke` example 通过真实 `VendorRuntime::load` 逐一编译、实例化原始与剥离版本，并执行 guest descriptor 导出；十个组件全部成功，前后 descriptor 相同。实际命令为 `CARGO_BUILD_JOBS=2 cargo run --locked -p stravia-vendor-runtime --example compression-smoke`，临时源码随后删除。此验证不是供应商完整推理、OAuth 或网络流程回归，不扩大其结论。

剥离函数名会降低堆栈回溯可读性；若正式采用，应保留对应未剥离构建件供诊断。删除 section 会改变 Component SHA-256、内容寻址身份与编译缓存键；必须在最终产物阶段生成 manifest 和发布校验和，不能给新字节沿用旧摘要。

其他候选没有在本次测量中重建，不宣称收益：

- `opt-level = "s"` / `"z"`、跨 crate thin/fat LTO、`codegen-units = 1` 可以进一步试验，但 Cargo 明确指出体积优化等级不保证比其他等级更小，且可能影响执行速度和构建成本。[Cargo profiles](https://doc.rust-lang.org/cargo/reference/profiles.html)
- 不建议为了 guest 瘦身直接修改全工作区 release 默认；它还影响 Server/Desktop。当前 builder 会移除宿主 Rust flags，实验必须确认选项实际传到 guest Cargo。
- `panic = "abort"` 不是当前目标的新收益：本机 `rustc --print cfg --target wasm32-wasip2` 已输出 `panic="abort"`，也符合 [Rust wasip2 文档](https://doc.rust-lang.org/stable/rustc/platform-support/wasm32-wasip2.html#unwinding)。
- 实际执行 Binaryen `132` 的 `wasm-opt <base.wasm> -Oz -o <temporary.wasm>` 返回 1，错误为 `this looks like a wasm component, which Binaryen does not support yet`。不能把针对 core module 的 `wasm-opt -Oz` 直接加到最终 Component 后处理。抽取内嵌 core modules、优化再重建涉及完整组件封装与 ABI 保真，不作为简单分发优化；参见 [Binaryen issue 6728](https://github.com/WebAssembly/binaryen/issues/6728)。

## 格式选择约束

- ZIP 使用普通 DEFLATE 时，Windows 文件资源管理器与 macOS Finder 都提供图形界面解包，适合当前“下载—手工解包—导入”链路；不能推广为所有 ZIP 算法都免依赖。[Microsoft ZIP 指南](https://support.microsoft.com/en-us/windows/experience/storage-filemanagement/zip-and-unzip-files)、[Apple ZIP 指南](https://support.apple.com/guide/mac-help/mchlp2528/mac)。
- Brotli quality 11 把更多编码成本留在发布端，适合离线预压缩；HTTP `br` 的支持不代表系统能手工解压 `.br`。[Brotli API](https://github.com/google/brotli/blob/v1.2.0/c/include/brotli/encode.h)、[RFC 7932](https://www.rfc-editor.org/rfc/rfc7932.html)。
- Zstandard 适合评估受控解码器下的下载/安装折中；不默认增加 `--long`。`--ultra` 解锁 20–22，可能增加编码和解码资源需求，应根据最终文件的窗口与实际收益选级别。[Zstandard 1.5.7 手册](https://github.com/facebook/zstd/blob/v1.5.7/programs/zstd.1.md)。
- XZ `-9` / `-9e` 使用 64 MiB 字典，手册列出的单线程解码内存约 65 MiB；本次文件不足 8 MiB，不应仅为“最高等级”忽略小文件的大字典代价。`-6` 的字典为 8 MiB，解码约 9 MiB。没有在本次实测峰值 RSS。[XZ 手册](https://tukaani.org/xz/man/xz.1.html)。
- 对实际 `xai-grok` 文件运行 `xz --robot --list -vv`：`-6e` 报告解码所需内存 **8,454,808 B（8.063 MiB）**，`-9e` 为 **67,175,064 B（64.063 MiB）**，两者压缩大小均为 **948,680 B**。这是格式/解码器报告的内存需求，不是进程峰值 RSS。实际 `zstd -lv` 显示同一输入的 19 与 22 都使用 **6,233,372 B** 窗口且无外部字典；不能泛称本次 22 的解码窗口必然更大。
- HTTP `Content-Encoding` 与独立下载包是两层契约：只有下载服务正确协商并透明解码时，用户得到的才仍是原始 `.wasm`；本仓库当前不提供在线插件获取流程。未来若使用 HTTP zstd，RFC 9659 要求帧窗口不超过 8 MB，不能直接把任意大窗口 `.zst` 当作合规 HTTP 编码。[RFC 9110](https://www.rfc-editor.org/rfc/rfc9110.html)、[RFC 9659 §3](https://www.rfc-editor.org/rfc/rfc9659.html#section-3)。

若以后让管理入口原生导入压缩文件，需要另行确认公开导入契约；优先单一压缩流，解压时执行 64 MiB 输出上限，不能先无限展开再检查。归档还涉及条目数量、路径与链接风险。发布校验和必须覆盖实际下载字节，Component 身份仍对应解压后的最终 Wasm；不能把压缩流自带的损坏检测当作发布者认证。
