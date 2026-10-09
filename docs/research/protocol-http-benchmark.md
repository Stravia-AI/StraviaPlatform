# 四协议真实 HTTP 端到端基准

入口：`python -m tests.common.measure_protocol_http`。脚本只使用 Python 标准库与现有 `tests.common.helpers`，需要 Python 3.11+ 和以 `test-harness` feature 构建的真实 `stravia-server`（`cargo build --locked -p stravia-server --release --features test-harness`，或 `task build:test:services`）。脚本始终传 `--test-catalog-base-url` 指向自身 loopback catalog fixture，提供合法 version.json、一个合成 Provider 和一个 Canonical Model，保留真实 catalog bootstrap 路径并记录 GET；不访问生产 catalog。不支持该 flag 的 binary 必须失败，不会 fallback 到生产。本方法不是 fixture replay 或直接函数微基准：通过真实 HTTP 初始化 Server/Admin，登记四个 Provider、Provider Model、Route 与 API Key，随后走真实协议转换/Wasm、SQL、普通 Observation、Generation Chain 路径。不会关闭普通观测、跳过历史或裁剪请求。启动冷编译预算为 180 秒，要求 Observation Debug 为 false；因此不依赖 Debug 开关控制的内置 performance recorder。

## 命令

完整默认运行（16 协议组合 × unary/stream × 常态/高并发，共 64 个测量 case）：

```sh
python -m tests.common.measure_protocol_http --binary target/release/stravia-server --label baseline --output artifacts/protocol-baseline.json --source-commit YOUR_SHA --build-condition "task build:test:services"
```

Windows 使用对应 `.exe` 路径。默认每个 case 预热 5 秒、测量 30 秒；请求 JSON 总字节数精确为 5,000,000，loopback synthetic 上游读取并校验完整请求后等待 1 秒，再返回带该请求唯一锚点的短文本。这里 5MB 使用十进制，不是 5MiB。固定上游等待模拟真实在途请求，但不是真实模型生成耗时。默认并发 worker 数 32；常态 open-loop 目标 10QPS，高并发 fixed-workers 各 worker 连续发送、收到完整响应后再发下一次。

只跑 16 组合的常态流式模式：

```sh
python -m tests.common.measure_protocol_http --binary target/release/stravia-server --label baseline --output artifacts/protocol-16.json --mode open-loop --delivery stream
```

小请求快速 smoke，仍覆盖 16 组合但不是正式内存/QPS验收：

```sh
python -m tests.common.measure_protocol_http --binary target/release/stravia-server --label smoke --output artifacts/protocol-smoke.json --mode open-loop --delivery unary --payload-bytes 4096 --warmup 0 --duration 2 --upstream-delay 0.05
```

定位单组合：追加 `--ingress openai-chat --upstream google-content --delivery stream --mode open-loop`。协议名为 `openai-chat`、`open-responses`、`anthropic-messages`、`google-content`；最后一项指 Gemini。`--ingress`、`--upstream`、`--mode`、`--delivery` 都可传多个值。

负载参数：`--duration`、`--warmup`、`--qps`、`--concurrency`、`--payload-bytes`、`--payload-pattern`、`--upstream-delay`、`--sample-interval`（默认 0.25 秒）、`--request-timeout`（默认 120 秒）、`--min-qps-ratio`（默认 0.95）。正文默认 `varied`，使用固定计数器的 SHA256 十六进制序列，避免重复单字符的极高压缩率掩盖 SQL 编解码成本；`repetitive` 单独测可压缩输入，不能替代默认验收。`--payload-bytes` 至少 512 字节；协议自身 JSON framing 占一部分，其余为 user 文本。每次请求前缀均为独立 nonce，避免相同历史前缀误命中父链。所有成功响应必须含该请求锚点；必须恰好一次到达真实上游。上游分别校验 endpoint、Content-Length 完整读取、转换后全部 user 文本的 UTF-8 字节长度与 SHA256。协议转换会改变 JSON framing，因此不要求上游 wire bytes 等于 ingress wire bytes，两者都保留在原始记录中。

## PostgreSQL / Redis 的隔离要求

SQLite 默认使用每次运行的新临时 data-dir/数据库，结束后清理。PostgreSQL 不自动创建/删除数据库或 schema，必须由运行者提前准备**仅用于本次 benchmark 的全新、空的独立本地数据库/容器**，运行后自行销毁，不复用生产、开发或上一次测量数据库。loopback 不等于隔离；明确隔离标志是运行者对此要求的确认，脚本不能判断本机端口是否转发到生产。

```sh
python -m tests.common.measure_protocol_http --binary target/release/stravia-server --label baseline-pg --output artifacts/protocol-baseline-pg.json --backend postgres --database-url postgresql://bench:bench@127.0.0.1:55432/stravia_bench_baseline --confirm-isolated-database
```

只接受 loopback PostgreSQL 主机；通过现有 setup API 应用真实迁移。必须给 baseline/optimized 分别准备同规格的空数据库，不能把前次持久历史当作后次预热。脚本不启动 Docker，也不提供 SQL 替身；PostgreSQL/Redis 占用不混入 server PID 指标。对于专用本地 Docker 服务，可重复传入 `--resource-container CONTAINER_NAME`，使用真实 `docker stats` 连续采样；每个 case 的 `container_resources` 分别报告容器 working set 与 CPU。容器占用不能替代 Server RSS，Docker Linux working set 也不等价于 Windows private commit。指定的容器缺样本或采样失败时整次运行失败；未指定时不声称测量了外部服务。Docker 的采样周期和显示精度独立于 PID 的 `--sample-interval`。

优化后的 PostgreSQL binary 可追加 `--redis-url redis://127.0.0.1:56379/0`；Redis 同样必须是专用、空的本地 benchmark 实例。脚本向临时配置写入 `[cache] redis_url = "..."`，不使用另一个环境变量约定。未实现该 cache 配置的 baseline 不传此参数。可选 `--config` 复用非存储 Server TOML；禁止其中包含 database/storage/cache，隔离存储和 cache 由本脚本控制。JSON workload metadata 不保存数据库/Redis 连接凭据。

## 指标、审计与失败

JSON schema 为 2，`response_validation=native-terminal-v1`。包含 binary SHA256、平台、逻辑核心数、source commit/build condition、完整 workload metadata、实际 server PID、每次请求 schedule/start/finish wall timestamp、service/排队/端到端延迟、状态码、请求/响应字节数、nonce、上游到达时间/字节数/文本 digest，以及逐时间点 PID CPU/内存采样。不会保留 5MB 重复正文。每 case 完成后 checkpoint，异常也写 fatal_error 并以非零退出；最终 `valid=false` 表示不能当作达标数据。旧 schema 1 只检查文本锚点，没有完整终态校验，不能直接进入新口径对比，必须重跑。

- **CPU**：仅该 server PID 累计 user+kernel CPU 时间的差分除以实际采样间隔，`100% = 1 核`，多核可超过 100%。不采集整机 CPU、不除以逻辑核心数。loadgen 和 synthetic upstream 在同一个 Python PID，另标 `harness_loadgen_and_upstream`，不能与 server 样本相加冒充产品占用。
- **内存**：Windows `rss_bytes` 是 working set，`private_commit_bytes` 是 PROCESS_MEMORY_COUNTERS_EX.PrivateUsage，分别报告，不互相替代。Linux RSS 来自 `/proc/PID/stat`；同时报告 `RssAnon + VmSwap`，这是匿名 resident+swap 近似，不是 Windows private commit；`private_commit_bytes=null`，VmSize 仅补充，不充当实际内存。250MB 目标应分别评估 working set/RSS 与 private commit，十进制 `bytes/1,000,000`；需要 MiB 时另用 `bytes/1,048,576`。
- **延迟**：p50/p95/p99 采用 nearest-rank。end-to-end 从预定发送时刻算到完整响应读取；service 从 worker 开始（包括编码/请求上传）到完整响应；queue 显示 worker 等待与调度延迟。TTFT 从 worker 开始到第一个完成的、包含协议原生可见文本的 SSE event，不把 message-start、Thinking 或其它 metadata 算作 token。unary TTFT 为 null；不是 tokenizer 精确首 token 时间。
- **吞吐**：报告 target QPS、实际 complete QPS、成功 QPS、预定/完成请求数、峰值 worker 在途、loadgen queue 峰值、错误/状态码和 drain 后总耗时。open-loop scheduler 不因 worker 忙而丢请求，采用显式队列并排完；因此过载会增加排队/结束耗时，而不是美化成功吞吐。小于目标 `min-qps-ratio` 判失败。429/非200、断连、超时、锚点错误、缺失或重复上游到达都失败。HTTP 200 和可见文本还不够：必须解析该 ingress 的成功终态，Chat 流还要求最终 `[DONE]`；Responses failed、其它原生 error、以及输出文本后缺少终态的 EOF 都失败。不能将长 drain 后的结果描述为稳定10QPS。
- **持久路径**：SQLite 测量结束后只读统计 observation 相关表，以及 `turn_chain_nodes`、`turn_chain_contents`、`turn_chain_node_contents`，列入 storage_inventory；这些 inventory 查询在测量之外。条数是采样时已经落盘的状态，不保证后台队列已排空；需要结合普通 Observation warning 判断记录缺口。PostgreSQL 不加入新的驱动或 shell SQL 查询；其真实存储路径由 setup/请求经过，但此入口没有独立 PostgreSQL 表计数。

## 对比与已知限制

baseline 与 optimized 用同一脚本、相同 case 顺序、时长、payload、上游延迟、模式、并发、采样间隔、构建档位与机器背景条件，只有 binary/label/source-commit/output 改变（PostgreSQL/Redis 换同规格空实例；优化新增Redis按实际部署配置记录）。建议按 ingress/upstream/delivery/mode 对齐结果，比较 RSS/commit 的峰值、CPU p50/p95、延迟、成功 QPS和 drain；先检查所有 case valid，再判断低于250MB是否现实。

使用 `python -m tests.common.report_protocol_http baseline.json optimized.json --output comparison.csv` 导出逐 case 的前后对比。它要求 schema 2、同平台/核心数/采样口径、完全相同 workload 与 case 集合，拒绝用缩小矩阵或不同请求大小作比较；保留失败 case。CSV 包含 Server RSS/private commit 峰值、CPU p50/p95（核心数）、成功 QPS、端到端/TTFT/队列 p95 和错误数。默认常态内存目标为严格低于十进制 250 MB，Windows 同时检查 private commit；候选运行无效或目标未达时返回 1，仍输出完整 CSV 和真实判定。只有明确放宽验收时才传 `--budget-mb`，不能靠改预算掩盖吞吐失败。外部容器资源仍看原始 JSON 的独立指标，CSV 不声称代表整套 PostgreSQL/Redis 占用。

一个运行共用一个 Server PID 和数据库，case 之间保留真实历史/普通观测与缓存；没有人为清空历史来压低内存。case 顺序固定，后续组合有累积数据，因此比较必须固定顺序，必要时用筛选单组合的新进程补充定位。预热同样写真实数据库，其错误也使case失败。JSON checkpoint 本身发生在 case 间，会消耗 harness CPU，可能使下一case带入短暂背景活动，不应当作主机资源独占结果。

只支持 Windows/Linux 的标准库 PID 采样；250ms 采样可能漏掉短暂峰值，Windows private commit 与 Linux匿名resident+swap不能直接等价比较。synthetic 上游响应很短，不覆盖真实长输出、工具调用、图片/音频、tokenizer、provider TLS、真实外部网络、WebUI读放大或多实例分布式缓存一致性。本基准不裁剪真实请求/历史，但常态真实模型响应比本基准更长时必须另做相同负载实测。loadgen 与upstream共享Python进程/GIL；其资源另报，若其排队/CPU成为瓶颈，应提高并发或换负载机器并重新比较，不可把不足吞吐掩盖成Server性能。单次HTTP请求使用新连接，未模拟客户端连接池收益。

执行结果必须同时给出实际命令、binary SHA256、workload 与有效性，不能把方法说明或未执行脚本当作性能结果。

## 2026-10-09 实测

**250MB 未达到；完整矩阵也未通过 10QPS 验收。** 本轮保留沙箱、完整请求、严格续接与 SQL 历史，削减已确认的请求深拷贝、排队 Admission 的正文保留，以及 Vendor 编码中间值。统一缓存已经接入；不能把缓存逻辑容量当作整个程序的内存上限，也不能用发生错误后的低 CPU/内存证明性能改善。

### 环境与可复现入口

|项目|值|
|---|---|
|机器|AMD Ryzen 9 8940HX，32 logical CPUs，50.70GB physical RAM|
|系统|Windows 10.0.26100 x64|
|构建|相同 release services/test-harness 构建路径；无临时 profiler；测量期间不并行运行构建或测试|
|原版本|基于 `e6c12394445b00cb2d87db18fda145be5a306aff`，仅增加本地 catalog 测试入口；SHA256 `bf9138761b29bc24ff228b8b6e1b167713e2720625f4e9b7a42c7fd00c0ad895`|
|优化版本|同一提交上的本次工作树；SHA256 `cf02964a931de2b293121304768e8ab7fcd70f640063dc3acb3dd37478ccd00b`|
|每个版本/后端|16 protocol pairs × unary/stream × open-loop/fixed-workers，共64 cases；每 case warmup 5s、measure 30s|
|请求与上游|精确5,000,000 bytes，varied payload，唯一 nonce/全文 digest；本地上游完整读取后等待1s，返回短文本|
|负载与采样|常态提交10QPS；高并发32 workers；250ms采样；Debug=false，普通 Observation 与 Generation Chain 开启|
|缓存|优化版共享16MiB逻辑容量；SQLite TinyUFO；PostgreSQL Redis|
|外部服务|Docker PostgreSQL 16、Redis 8-alpine；loopback独占实例，前后分别重建空实例；Redis 512MiB/noeviction、不启用持久化|

原始 JSON、CSV 与冻结二进制保存在本次工作树的 `target/benchmarks/`，该目录不是提交内容。命令从仓库根目录执行：

```sh
uv run --locked --group test python -m tests.common.measure_protocol_http --binary target/benchmarks/bin/baseline-stravia-server.exe --label baseline-sqlite-v2 --output target/benchmarks/baseline-sqlite-v2.json --source-commit e6c12394445b00cb2d87db18fda145be5a306aff --build-condition release-services-hidden-test-harness
uv run --locked --group test python -m tests.common.measure_protocol_http --binary target/benchmarks/bin/optimized-stravia-server.exe --label optimized-sqlite-v2 --output target/benchmarks/optimized-sqlite-v2.json --source-commit e6c12394445b00cb2d87db18fda145be5a306aff --build-condition release-services-hidden-test-harness
```

PostgreSQL 对应命令在上述参数后追加：

```sh
--backend postgres --database-url <本次独占空数据库的loopback URL> --confirm-isolated-database --resource-container stravia-benchmark-postgres-bluegill --resource-container stravia-benchmark-redis-bluegill
```

分别使用 `baseline-postgres-v2` / `optimized-postgres-v2` label 和 JSON 文件名；仅优化版再追加 `--redis-url redis://127.0.0.1:26379/0`。这里的 `<…>` 是部署参数，不是可直接执行的 shell 语法；不能替换成生产连接。

四次运行都完整完成64 cases、`fatal_error=null`，但退出码均为1，未通过的 case 保留原样。逐 case 对比已执行并输出：

```sh
uv run --locked --group test python -m tests.common.report_protocol_http target/benchmarks/baseline-sqlite-v2.json target/benchmarks/optimized-sqlite-v2.json --output target/benchmarks/comparison-sqlite-v2.csv
uv run --locked --group test python -m tests.common.report_protocol_http target/benchmarks/baseline-postgres-v2.json target/benchmarks/optimized-postgres-v2.json --output target/benchmarks/comparison-postgres-v2.csv
```

两个 report 均退出1：`candidate_valid=false`、`normal_memory_below_budget=false`、`meets_target=false`。旧 schema 1 的先导数据不用于这些前后对比。

### 吞吐、CPU 与有效性

范围来自对应32 cases；CPU列是每 case CPU p50 的核心数范围，**不是整机百分比，也不是跨 case 的总体 p50**。QPS列包含 drain，PostgreSQL的低值包含失败/cooldown场景，不能视为有效容量。

|后端/负载|版本|成功QPS范围|Server CPU p50，核|测量请求错误|有效cases|
|---|---|---:|---:|---:|---:|
|SQLite / 10QPS open-loop|原版|8.25–9.51|5.90–6.88|0|2/32|
|SQLite / 10QPS open-loop|优化|8.56–9.54|5.87–6.84|0|7/32|
|SQLite / 32 workers|原版|8.41–9.62|5.87–6.70|0|32/32|
|SQLite / 32 workers|优化|6.68–10.14|5.64–6.43|0|32/32|
|PostgreSQL / 10QPS open-loop|原版|5.37–9.48|4.65–9.70|55|0/32|
|PostgreSQL+Redis / 10QPS open-loop|优化|4.10–9.49|1.84–7.64|166|0/32|
|PostgreSQL / 32 workers|原版|4.85–10.08|1.61–9.82|21|31/32|
|PostgreSQL+Redis / 32 workers|优化|2.78–10.39|0.12–8.33|29|30/32|

SQLite 两次运行含 warmup 共22,586/22,768次请求，全部成功，且相应 `turn_chain_nodes` 条数与请求总数一致。普通 Observation 在采样时仅有4,724/8,785条 run；分别有17,321/13,337条 `observation finalization unavailable` 告警。完整 Generation Chain 不等于普通诊断完整，告警计数也不能直接映射全部缺失记录。

PostgreSQL 原版共22,699次、测量76次错误；优化版共22,670次、测量195次错误，另有8次 warmup错误。日志包含 synthetic upstream 的 TCP connect `os error 10060`，随后出现502与模型不可用503；优化版记录207条此类连接超时告警，未记录 `runtime_cache` WARN。**只证明本机测试路径出现传输超时，不证明真实 Provider 故障，也不证明缓存导致或消除了超时。** Docker资源采样没有错误。

优化版 PostgreSQL 测量结束后另行执行只读 SQL：`turn_chain_nodes=22,467`，等于22,670次总请求减203次错误；`inference_run_observations=14,306`、`observation_events=142,340`。这不是 collector 自带的 PostgreSQL inventory，也不是测量期指标；原版独占实例已重建，没有对应的原版 SQL 计数可比。

CPU整体没有明显下降，部分高并发组合吞吐退化；不能宣称 CPU 或所有路径吞吐已优化达标。本轮没有采集 CPU 调用栈，不能把总占用直接归因于 Wasmtime、JSON、压缩或数据库中的某一项。

### 4×4 内存结果

下表单元格为**原版 → 优化版 Server private commit 峰值，十进制MB**；每个 protocol pair 取 unary/stream 两种 delivery 的较高峰值。Windows working set 和所有分位数仍保留在 JSON/CSV。`†` 表示该 pair 的任一版本、任一 delivery 有请求或 warmup 错误；这些单元格只记录观测，不作同等成功工作量的性能结论。

SQLite，10QPS提交负载：

|Ingress / Upstream|Chat|Responses|Anthropic|Gemini|
|---|---:|---:|---:|---:|
|Chat|4761 → 2104|4822 → 2664|4159 → 2051|4756 → 2018|
|Responses|4370 → 2224|4857 → 3006|4431 → 2825|4273 → 2817|
|Anthropic|4394 → 2917|4865 → 2980|4428 → 2270|4723 → 2471|
|Gemini|4862 → 3179|4858 → 3220|4918 → 3178|4639 → 3073|

SQLite，32 workers：

|Ingress / Upstream|Chat|Responses|Anthropic|Gemini|
|---|---:|---:|---:|---:|
|Chat|5759 → 2891|5880 → 2957|6012 → 3191|5845 → 2874|
|Responses|5926 → 3142|5808 → 3209|6052 → 3189|5818 → 2942|
|Anthropic|5886 → 3182|5828 → 3265|5949 → 3309|5758 → 3027|
|Gemini|5990 → 3342|5887 → 3423|5961 → 3478|5927 → 3169|

PostgreSQL → PostgreSQL+Redis，10QPS提交负载：

|Ingress / Upstream|Chat|Responses|Anthropic|Gemini|
|---|---:|---:|---:|---:|
|Chat|3811 → 2708|3686 → 2496|4288 → 3321†|4011 → 2949†|
|Responses|3947 → 3257|3928 → 3245†|3940 → 2560|3649 → 2729|
|Anthropic|3867 → 2759|3849 → 2393|4741 → 3197†|5435 → 2103†|
|Gemini|5566 → 3388|5453 → 3576†|4109 → 2844|4057 → 3234|

PostgreSQL → PostgreSQL+Redis，32 workers：

|Ingress / Upstream|Chat|Responses|Anthropic|Gemini|
|---|---:|---:|---:|---:|
|Chat|5637 → 3286|5565 → 3426|5627 → 3402|5506 → 3113|
|Responses|5630 → 3371|5465 → 3446|5653 → 3420|5566 → 3122|
|Anthropic|5721 → 3313|5631 → 3421|5591 → 3452|5487 → 3110|
|Gemini|5659 → 3458†|5557 → 3644|5561 → 3671|5518 → 3300†|

SQLite逐 case private commit峰值下降33.6%–57.6%，但部分30秒场景存在明显排队，因此不是全部组合稳定10QPS下的250MB结果。PostgreSQL对比包含错误，不能用错误/cooldown时的低占用宣称同等负载收益。

外部服务分别统计，不能藏在 Server 进程指标之外，也不能把不同时间的各自峰值相加当作同时峰值：

|外部服务|原版运行 working set峰值MB|优化版运行 working set峰值MB|
|---|---:|---:|
|PostgreSQL|1271.31|1259.50|
|Redis|11.24（原版不使用，空闲对照）|13.65|

### 持续负载与内存放宽依据

另对 SQLite `Gemini → Responses / stream / open-loop` 进行相同前后120秒测量，warmup/大小/延迟/并发/采样保持不变。命令使用上述 SQLite 入口，追加 `--ingress google-content --upstream open-responses --delivery stream --mode open-loop --duration 120`；原始文件为 `baseline-sqlite-steady.json` / `optimized-sqlite-steady.json`。各1,200次测量请求全部成功，两次仍退出1。

|指标|原版|优化版|
|---|---:|---:|
|成功QPS，含drain|7.39|8.45|
|Server working set峰值MB|4082.60|3038.20|
|Server private commit峰值MB|4359.45|3233.34|
|Server CPU p50，核|6.34|5.75|
|端到端p95，秒|39.31|21.98|
|最早30s提交请求的排队p95，秒|10.05|3.30|
|最后30s提交请求的排队p95，秒|36.48|18.91|

这不是单纯30秒结束边界造成的QPS低估：排队随持续提交增长。优化减少了峰值和排队，但该组合仍无法稳定消化10QPS；loadgen的32个worker上限限制在途，不代表真实无界入口下的内存上界。

定位阶段曾临时通过 Wasmtime ResourceLimiter 计数成功 Store 的线性内存增长，单worker、相同5MB Chat→Chat输入5次调用：每次Infer为21,626,880 bytes，选择阶段为11,665,408 bytes；原始证据在 `ownership-wasm-profile.json`。这是线性内存大小，不是RSS或整机堆分解，不能按在途数相乘冒充物理占用。计数器已删除，正式矩阵不含该 profiler。

**本次实测的临时容量规划可放宽为 Server 峰值4GB，而不是250MB**；这给已测 Server 峰值留出余量，不是产品硬限额或稳定10QPS承诺。PostgreSQL/Redis需另外规划；长输出、真实网络、更多在途请求、桌面/WebView及多实例仍未覆盖。严格250MB report继续失败，放宽内存也不能让吞吐或传输错误的case变成通过。
