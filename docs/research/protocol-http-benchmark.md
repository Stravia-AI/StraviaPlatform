# 四协议真实 HTTP 端到端基准

入口：`uv run --locked --group test python -m tests.common.measure_protocol_http`。需要 Python 3.11+、锁定的 test 依赖，以及以 `test-harness` feature 构建的真实 `stravia-server`：

```sh
cargo run --locked -p stravia-vendor-base --bin stravia-build-vendors
cargo build --locked --release -p stravia-server --no-default-features --features test-harness --bin stravia-server
```

修改协议 codec 后必须重建 Vendor Wasm，再构建 Server；只重建宿主不能证明插件内解析器已更新。

只有模型和 catalog 使用本地 loopback fixture；鉴权、凭据识别/替换/还原、媒体 Artifact 归一化、路由、Provider/Wasm、HTTP/SSE 交付、普通 Observation、Generation Chain 与数据库均走真实产品实现。不关闭历史或普通观测。**当前完整链路口径启用 Debug 和可逆凭据保护**，并核验四方向 Wire 正文与请求性能因果链，因此不能与旧 Debug=false 或未注入凭据的成绩直接比较。初始化、管理查询、Wire 导出和只读历史/凭据核验在流量测量区间之外，仍保留独立资源采样。

脚本始终传 `--test-catalog-base-url`，不支持该 flag 的 binary 必须失败，不会回退生产 catalog。`tiktoken` 用于真实文本分词，`zstandard` 用于校验实际落盘的历史内容，`psycopg` 用于 PostgreSQL 只读快照；不替换产品的数据库驱动或存储实现。

## 命令

完整默认运行（16 协议组合 × unary/stream × open-loop/fixed-workers，共 64 个测量 case）：

```sh
uv run --locked --group test python -m tests.common.measure_protocol_http --binary target/release/stravia-server --label baseline --output target/benchmarks/protocol-baseline.json --source-commit YOUR_SHA --build-condition "release no-default-features test-harness"
```

Windows 使用对应 `.exe`。默认每 case 预热 admission window 5 秒、测量 admission window 120 秒，窗口结束后排完所有已接纳请求；长请求会使实际运行时间超过窗口。32 workers；open-loop 默认 0.5 请求/秒，fixed-workers 收到完整响应后继续发送。

每次请求由 seed、case、phase、请求序号生成可复现计划：

|项目|默认负载及计量|
|---|---|
|输入文本|凭据保护前的客户端完整多轮历史合计 50,000–150,000 `cl100k_base` tokens；逐条文本真实编码后求和，不用 bytes/4，不含协议 framing 或模型特定图片 token|
|图片|完整请求共 0–10 张有效 512×512 RGB PNG；从固定 seed 的 10 张随机像素图片池采样，保留历史图片，记录尺寸、字节数及 SHA256；这不是独立无限图片语料|
|首个可见 token|上游完整读取请求后 1–10 秒；不是 response headers 或 message-start|
|生成速率|上游模型正文 30–100 token/s，每个真实分词片段按单调时钟的绝对 deadline 发出，不累计 sleep 漂移；凭据引用会跨片段发送|
|输出时长|上游首末可见 token 相隔 10–60 秒；模型输出 token 数为 `rate × duration + 1`，包含关联 nonce 和凭据引用|
|内容熵|seeded 多样单词文本，不使用单字符重复正文美化压缩率；仍是合成语料，不代表自然语言分布|
|续接|每 worker 默认连续两轮，重放实际收到的 assistant 文本；新一轮输入总量包含历史，不能丢弃历史满足 token 上限|
|凭据探针|每请求两个新生成的高熵合成 GitHub PAT 格式值，第一个重复出现；上游回传实际保护引用，客户端必须完整还原；不连接 GitHub、不使用真实凭据|

输入 token/图片采样受已保留历史和后续输出预留空间约束，不声称每轮在整个区间独立均匀采样。`--chain-turns` 至少为 2；若范围不足以容纳续接则拒绝启动。unary 在完整模拟生成结束后返回整包，不能把 unary 的首字时间报告成流式 TTFT。

只跑 16 组合的常态流式模式：

```sh
uv run --locked --group test python -m tests.common.measure_protocol_http --binary target/release/stravia-server --label baseline --output target/benchmarks/protocol-16.json --mode open-loop --delivery stream
```

完整大小的有限样本 smoke：仍覆盖 16 组合、真实两轮历史和 10 秒生成，但不是稳态吞吐验收：

```sh
uv run --locked --group test python -m tests.common.measure_protocol_http --binary target/release/stravia-server --label smoke --output target/benchmarks/protocol-smoke.json --mode fixed-workers --delivery stream --warmup 0 --concurrency 1 --requests-per-case 2 --min-first-token-delay 1 --max-first-token-delay 1 --min-duration 10 --max-duration 10 --min-tokens-per-second 30 --max-tokens-per-second 30
```

定位单组合：追加 `--ingress openai-chat --upstream google-content --delivery stream --mode open-loop`。协议名为 `openai-chat`、`open-responses`、`anthropic-messages`、`google-content`；最后一项指 Gemini。`--ingress`、`--upstream`、`--mode`、`--delivery` 都可传多个值。

参数：`--seed`、`--chain-turns`、`--image-size`，以及 `--min/--max-input-tokens`、`--min/--max-images`、`--min/--max-first-token-delay`、`--min/--max-tokens-per-second`、`--min/--max-duration`（实际拼写例如 `--min-input-tokens`）。各范围只能在上表区间内收窄，不可用小正文或亚秒输出代替。`--duration` 是负载接纳窗口，`--min-duration` / `--max-duration` 是单请求生成时间。`--requests-per-case` 替代测量窗口的请求数上限，至少为 `concurrency × chain-turns`；预热仍按窗口执行。

`--sample-interval` 默认 0.25 秒，`--request-timeout` 默认 120 秒，`--audit-timeout` 默认 180 秒。open-loop 按请求而不是会话计 QPS；固定 worker 分配保证调度完成顺序不会改变 seed 对应历史。`--max-start-lag-ms` 默认 1000，超出 p95 接纳延迟判定负载未维持；`--max-pacing-lag-ms` 默认 250，上游错过 token deadline 则不能把该成绩归因于产品。旧 `--payload-bytes`、`--payload-pattern`、`--upstream-delay`、`--min-qps-ratio` 已移除。

每协议创建两个启用 Target 和一个优先级更高但禁用的 Target，分别使用不同 Provider/模型名，验证实际选择而不只是请求成功。上游检查凭据、endpoint、完整 Content-Length、全部角色/文本/图片 SHA256，Responses 原生 `previous_response_id` 由本地上游恢复后再校验完整上下文。客户端校验完整输出而非只找 nonce，并要求成功原生终态。

### 凭据保护与脱敏口径

脚本通过真实管理 API 开启 `reversible_redaction_enabled` 并读回确认，没有关闭凭据保护的成绩选项。上游将实际收到的引用逐个绑定到预先生成的合成探针，要求角色、图片以及所有非秘密文字完全一致；漏替换、部分替换、两个秘密共用引用、历史引用改变或正文被吞掉均失败。上游响应使用绑定后的引用并重新计算精确模型 token 预算，客户端必须收到完整还原正文；下一轮重放该真实响应，验证旧映射复用及两条新映射的发现和发布。

负载自身只包含 user/assistant 会话。平台因活动凭据引用追加的保护指令从各协议的实际 system/instructions carrier 独立读取，检查存在、无探针污染以及本次运行跨请求/协议的一致性，记录摘要并计入模型输入 token；不硬编码产品提示词文案，也不把所有 system 消息直接丢弃。多余的系统项或被改写的会话正文仍会失败。

保护引用与原凭据的 tokenizer 长度不同。因此 `input_tokens` 记录保护前客户端负载，`upstream_input_tokens` 记录真实受保护的模型输入；`output_tokens` 是按速率发送的模型输出，`client_output_tokens` / `actual_output_tokens` 是还原后的客户端文本。usage 对照模型侧实际计数，客户端 token/s 单独实测，不能用其中一个计数冒充另一个。

**普通 Observation 与 Debug/Wire 的捕获范围不同，永久脱敏规则相同。** [ADR-0063](../adr/0063-record-four-direction-wire-debug-at-transport-boundaries.md) 只要求替换实际 HTTP `Authorization` header 值，header 名不区分大小写；其他 header、URL、body 和普通诊断 metadata 不作全局凭据字面量过滤。普通观测的完整可见正文直接与客户端完整正文逐字节摘要核对，不生成另一份 blanket-redaction 预期。bench 检查实际 Authorization 已遮蔽，以及 Anthropic `x-api-key` / Gemini URL `key` 的原始保真，**不宣称普通 Observation 或 Debug ZIP 不含凭据**。四方向 body 摘要仍与实际收发字节完全一致，客户端与历史中的合法还原正文不得被错误脱敏；业务凭据保护的上游无明文、发现事件与 Principal-scoped 映射验证另行执行。

普通观测中的非凭据字段允许保留完整的可逆引用，例如 Responses 原生内容元数据；引用不是原秘密，也不自行授予访问权。不能把引用一律判为秘密泄露。发现事件仍不得包含秘密或可恢复引用，客户端最终可见文本则必须完成实际还原。

验收还从实际 SQLite/PostgreSQL 只读快照核对每条映射的秘密值、Principal、发布时间及有效期，并与历史 Principal 对齐；普通发现事件必须准确包含本轮两条新映射及识别规则/用户输入来源。报告仅保存计数、布尔证据和摘要，不保存探针值或可恢复引用。若 Server 日志泄露合成秘密，报告遮蔽该值并把整次运行判为失败，不以隐藏日志绕过验收。

可逆映射表与按契约保存的已还原客户端历史仍属于敏感数据；此处不是数据库加密或“全部落盘内容无秘密”的验收。

## PostgreSQL / Redis 的隔离要求

SQLite 默认使用每次运行的新临时 data-dir/数据库，结束后清理。PostgreSQL 不自动创建/删除数据库或 schema，必须由运行者提前准备**仅用于本次 benchmark 的全新、空的独立本地数据库/容器**，运行后自行销毁，不复用生产、开发或上一次测量数据库。loopback 不等于隔离；明确隔离标志是运行者对此要求的确认，脚本不能判断本机端口是否转发到生产。

```sh
uv run --locked --group test python -m tests.common.measure_protocol_http --binary target/release/stravia-server --label baseline-pg --output target/benchmarks/protocol-baseline-pg.json --backend postgres --database-url postgresql://bench:bench@127.0.0.1:55432/stravia_bench_baseline --confirm-isolated-database
```

只接受 loopback PostgreSQL 主机；通过现有 setup API 应用真实迁移。必须给 baseline/optimized 分别准备同规格的空数据库，不能把前次持久历史当作后次预热。脚本不启动 Docker，也不提供 SQL 替身；PostgreSQL/Redis 占用不混入 server PID 指标。对于专用本地 Docker 服务，可重复传入 `--resource-container CONTAINER_NAME`，使用真实 `docker stats` 连续采样；每个 case 的 `container_resources` 分别报告容器 working set 与 CPU。容器占用不能替代 Server RSS，Docker Linux working set 也不等价于 Windows private commit。指定的容器缺样本或采样失败时整次运行失败；未指定时不声称测量了外部服务。Docker 的采样周期和显示精度独立于 PID 的 `--sample-interval`。

优化后的 PostgreSQL binary 可追加 `--redis-url redis://127.0.0.1:56379/0`；Redis 同样必须是专用、空的本地 benchmark 实例。脚本向临时配置写入 `[cache] redis_url = "..."`，不使用另一个环境变量约定。未实现该 cache 配置的 baseline 不传此参数。可选 `--config` 复用非存储 Server TOML；禁止其中包含 database/storage/cache，隔离存储和 cache 由本脚本控制。JSON workload metadata 不保存数据库/Redis 连接凭据。

## 指标、审计与失败

JSON schema 为 **5**，`response_validation=full-chain-credentials-token-paced-v3`。记录 binary SHA256、平台、构建条件、tokenizer、seed、负载配置、实际 server PID、每请求 token/图片/生成计划和实测时序、HTTP 结果，以及 PID CPU/内存样本。不保存原始大正文、图片、API Key 或数据库凭据。每 case checkpoint，失败以非零退出。schema 5 按 ADR-0063 的 Authorization-only 策略验证捕获：普通可见输出使用客户端完整正文的同一摘要，不再生成另一份 blanket-redaction 预期。旧 schema 1–4 的负载或捕获契约不同，不能与新结果直接比较。

每个请求都必须取得以下真实证据，缺失或不一致使 `full_chain_verified=false` 和整个 case 无效：

1. HTTP 入站捕获的关联 header → 实际 Run ID；只有 Responses 响应 ID 是 Generation ID，不能拿 Chat/Anthropic/Gemini 的供应商消息 ID 代替。
2. 实际 Route、ModelTurn 与唯一成功 TargetAttempt，包含 Provider、上游模型及实际调度 Target key；配置 Target UUID 另存，不与 `provider:model` 调度 key 混用。
3. 客户端完整内容与原生成功终态；Run completed、`client_output_committed=true`、delivery completed/delivered、真实模型侧 input/output usage coverage，以及普通观测持久化可见输出与完整客户端正文的同一 SHA256。
4. 四方向 Wire body 原始字节的 SHA256，分别与客户端发出/收到和本地上游收到/发出的正文核对；不能以存在一个 body record 代替全文完整性。
5. SQLite/PostgreSQL 只读一致性快照中的真实 Generation 节点、父边和全部祖先；解压 codec、恢复 JSON Pointer slots、验证 Principal/kind、ordinal、引用与域前缀 SHA-256，核对持久化 client output 摘要。冻结基线的 format-2 envelope 仅经显式旧格式审计适配器读取，不是产品双读路径，也不是表行数推断。
6. `proxy.inference_run.args.request_id` 与真实 Run ID 一致，沿 `span_id` / `parent_id` 检查并发请求各自已结束的路由、Provider、历史和 SQL 因果子树。该内部 ID 仅进入 Debug 时间线，不进入指标标签，也不是用户提供的 `x-request-id`。
7. 实际凭据保护往返、所有捕获的实际 Authorization header 值脱敏、模型侧正文不含探针明文、准确发现事件和已发布的 Principal-scoped 映射。普通正文、其他 header、URL 和诊断 metadata 不适用全局凭据字面量过滤；审计结果自身仍仅保留安全标识、摘要、计数与失败码。不能仅凭设置已开启或凭据表行数增加宣布覆盖。

观测完成是异步状态：在既有截止时间内等待真实 `run_finished.delivery` 终态及终结的 trace，再读取历史和核验。交付收据可以先登记 `completed` 与 `delivery_completed_at`，完整 trace 也可以先于 Generation 发布和最终 Run 收口，不能把这些中间投影视为完整落盘。已知 Run ID 与通过捕获 header 关联的入口采用同一完成条件；失败 delivery 仍立即进入失败核验，永久缺失终态仍使成绩无效。不增加推理重试、不延长截止时间，也不靠固定 sleep 宣称落盘。每 phase 完整核验并导出后才切换 Debug epoch，避免前一批容量计数混入下一批；数据库历史、普通观测与产品缓存不清空。

- **CPU**：仅该 server PID 累计 user+kernel CPU 时间的差分除以实际采样间隔，`100% = 1 核`，多核可超过 100%。不采集整机 CPU、不除以逻辑核心数。loadgen 和 synthetic upstream 在同一个 Python PID，另标 `harness_loadgen_and_upstream`，不能与 server 样本相加冒充产品占用。
- **内存**：Windows `rss_bytes` 是 working set，`private_commit_bytes` 是 PROCESS_MEMORY_COUNTERS_EX.PrivateUsage，分别报告。Linux RSS 来自 `/proc/PID/stat`；`RssAnon + VmSwap` 另报，不等价于 Windows private commit；VmSize 不是实际占用。MB 为十进制，MiB 为 `bytes/1,048,576`。
- **延迟**：p50/p95/p99 采用 nearest-rank。queue 是排队到 worker 取出；preparation 单列分词、图片/请求构造；service 从准备完成到完整响应；end-to-end 从预定发送算起，包含以上成本。TTFT 从实际发送前开始，到首个含可见文本的完整 SSE event，不计 message-start、Thinking 或 metadata。另报客户端首末文本间隔及实际 token/s；unary TTFT 为 null。
- **吞吐**：实际/成功 QPS 包括有限 cohort 的 drain，明确不称为稳态完成率。接纳速率是否维持另看 p95 admission lag，不能用 60 秒输出必然产生的 drain 直接判低吞吐，也不能隐去过载排队。429、非200、断连、超时、正文不符、缺失/重复上游请求、协议错误终态、EOF 缺终态均失败。
- **持久路径**：逐请求链路核验是成功门槛；SQLite 最终表数量与 `storage_bytes.encoded_payloads` 只是额外统计，不能替代核验。节点、content、event 编码字节与条数共用只读快照；database/WAL/shared-memory 文件逻辑长度在之后独立读取，不是分配块数或累计写入量。压缩对比需对齐完成请求、token、图片、保留条数和 Debug 状态，不能把更多请求的总磁盘差当作压缩率变化。

## 对比与已知限制

baseline 与 optimized 使用同脚本、seed、case 顺序、token/图片/时序范围、模式、并发、采样间隔、Debug 状态、构建档位和主机背景条件。数据库与 Redis 各自使用同规格空实例。只有 binary/label/source-commit/output 等身份参数不同。旧 binary 若缺少请求身份时间线，需要先补同一采集改动，不能豁免完整链路校验。

使用 `uv run --locked --group test python -m tests.common.report_protocol_http baseline.json optimized.json --output comparison.csv`。要求 schema 5、同平台/核心数/tokenizer/凭据保护/采样口径、相同 workload 与 case 集合；拒绝缩小矩阵或不同 seed/范围比较。CSV 保留失败 case，输出 Server RSS/private commit 峰值、CPU p50/p95 核心数、成功 QPS、延迟、错误数、逐请求链路核验数以及平均输入 token/图片/模型输出 token。内存上限通过显式 `--budget-mb 250` 等参数指定；不指定时仅判断运行和完整链路有效，不宣称达到任何内存目标。外部服务资源另看独立样本。

一个运行共用一个 Server PID 和数据库，case 之间保留真实历史/普通观测与缓存；没有人为清空历史来压低内存。case 顺序固定，后续组合有累积数据，因此比较必须固定顺序，必要时用筛选单组合的新进程补充定位。预热同样写真实数据库，其错误也使case失败。JSON checkpoint 本身发生在 case 间，会消耗 harness CPU，可能使下一case带入短暂背景活动，不应当作主机资源独占结果。

只支持 Windows/Linux PID 采样；250ms 可能漏掉短峰值。当前覆盖普通多模态推理实际经过的完整链路，**不等于所有产品分支**：工具执行、故障重试/路由切换、WebSocket、音频、真实模型计算、Provider TLS、外网、WebUI读放大、多实例缓存一致性不在此矩阵内。每请求使用新 HTTP 连接；不代表连接池收益。loadgen/upstream 同 Python 进程，CPU/GIL 限制通过独立样本、preparation/admission/pacing lag 暴露；不能把它们的瓶颈算成产品吞吐。

执行结果必须同时给出实际命令、binary SHA256、workload 与有效性，不能把方法说明或未执行脚本当作性能结果。

## 当前零拷贝边界

以下借用、所有权转移及缓冲复用已实施；没有修改 canonical、持久化指纹、WIT 或数据库 schema。复用分配不等于完全不搬动字节，也不等于端到端零拷贝。

|路径|已实施|边界|
|---|---|---|
|凭据保护与还原|`text.rs::replace` 无变化返回 `Cow::Borrowed`，发生替换后按区间复制；读写 visitor 在确定变化前不拆分 `Arc<String>`；未转义 JSON token 与未变化流式 chunk 保留原缓冲|实际替换、转义解码与未闭合引用仍需要新缓冲。最长匹配、引用原子、过期规则、不重新扫描插入值和 fail-closed 语义不变；未跳过检测|
|媒体与协议编码|嵌套工具媒体从借用 JSON 反序列化，移走 Responses 的单次使用原生元数据；Data URL 解析和生成复用 owned String，并精确预留前缀空间，避免容量翻倍|删除或插入前缀仍会搬动字节；Base64 与最终协议 JSON 仍是新表示。不能仅把 IR 改为 `Arc<String>`、再在最终 `Value::String` 处复制，冒充消除了复制|
|Debug Wire|`WirePayload` 共享已有 `Bytes`，直接序列化 UTF-8；无效 UTF-8 的 Base64 通过显示编码器写入最终 JSON，不创建等长中间 String|Trace **仍在入队前同步编码**，队列只保留并计量最终 Vec，不把小 Bytes 切片及其大 backing 长期留在队列。Authorization 掩码、Header UTF-8 表示、捕获上限与 writer 错误保持原边界|
|普通可见正文观测|ledger/终态保留 `Arc<String>`，通过 `RunObserver::record_visible_delta(&str)` 把借用正文交给现有脱敏器；普通脱敏逐阶段复用未变化的 `Cow<str>`，只在真实替换处建立输出，ASCII scheme 比较不再逐词创建小写 String|脱敏后的内容、未完成秘密片段和持久化独立表示仍需拥有缓冲；引用原子、空白、已掩码字段和分隔符保真保持不变；没有把大正文 clone 延后到 writer 来宣称优化|
|SSE、Connect 与 WebSocket|四类公共协议共享借用分帧，部分帧只保留尾部并集中压紧；Devin 未压缩帧借用、Kilo 单行 data 借用；客户端 SSE→WebSocket 直接共享 `Bytes` 帧切片|跨 chunk 拼接、gzip 解压和协议 JSON 重建仍需分配。部分上游 WebSocket `Text(String)` 抓取仍需现有 owned 事件接口要求的独立快照，不计作零拷贝收益|
|历史存储|借用 envelope 字段进行序列化，RAW 解码返回 `Cow` 借用，RAW 编码复用 owned Vec；SQLx 接管 owned payload，最后一个恢复 slot 移走可独占的 Value|重复 slot 或仍被缓存持有的 Value 需要独立副本；zstd 解压需要新输出。唯一内容仍先无分配计长/哈希，避免为了“一次遍历”先物化所有重复内容|
|Trace 分段存储与读取|借用已编码正文的 `RawValue`，消除 `Vec→Value/String→Vec→最终记录` 的正文重复物化；只有可缓存且首次出现的正文才建立缓存副本，超出缓存总预算的记录不再计算无用的缓存摘要；按既有编码长度预留输出容量；读取时移动已有 JSON 子树|分段格式、引用偏移、8MiB 缓存边界和解码值不变。这里跳过的是不可能进入缓存的 Debug 记录摘要，不是 canonical 或持久化内容指纹|

已有的 `Arc<AiRequest>` / `Arc<String>`、borrowed canonical projection 不应重复当作未做优化。入站图片解码后的 `Bytes::from(Vec<u8>)` 已接管底层缓冲，并不复制整个图片，参见锁定的 [bytes 1.12.1 实现](https://docs.rs/bytes/1.12.1/src/bytes/bytes.rs.html#947-976)。Wasm Component 线性内存隔离、JSON 转义、Base64、压缩都会产生必要的新表示；跨 WIT 的真零拷贝不是局部替换 `Vec` 就能实现。

### 无插桩重复对照

Linux x64、Intel Core i5-13600K、20 logical CPUs。Responses 入站 × 四种上游 × unary/stream，共 8 case、16 请求，每 case 两轮续接、1 worker；完整 50k–150k token / 0–10 图片范围，固定上游等待 1 秒、30 token/s、生成 10 秒。seed、case 顺序、采样间隔、Debug/普通观测/历史/凭据保护均相同；测量期间不并行构建或测试，每轮使用新进程与新 SQLite 数据库，case 之间不清历史或缓存。

基线 SHA256 为 `b93b0200ea560d8f7526c49e4c74ea01e3b3094cf83798b89e0751a0fb70bb2e`，最终版本为下节的 `4c453f…`。内置 base Wasm 通过 `stravia-core/build.rs` 的 `include_bytes!` 编入各自二进制；复跑旧二进制不会加载工作树中新编译的 base 取代其 guest。

|运行|完整链路通过|Server CPU 采样窗口合计，秒|Server RSS 采样峰值，MB|有限 cohort 成功 QPS，含 drain|
|---|---:|---:|---:|---:|
|较早基线 A1|16/16|12.82|974.20|0.08480|
|最终版本 B1|16/16|12.40|1049.41|0.08533|
|最终版本 B2|16/16|12.34|1043.89|0.08525|
|反向复跑基线 A2|16/16|12.42|1057.53|0.08536|

A1 之后有开发、中间候选和单独诊断，B1→B2→A2 则连续串行测量；这不是严格相邻的完整 ABBA 实验。两轮 CPU 均值仅下降约 2%，最终版本与反向基线很接近，**不足以证明显著 CPU 收益；RSS 也没有稳定下降**。中间候选 `054a86…` 曾记录 10.80 秒，但不能沿用为最终二进制的成绩。不能把少复制等同于更低 RSS，更不能把等待上游主导的有限 cohort QPS 当成饱和吞吐上限。

这里的 CPU 秒数对每 case 的 `role=server, phase=measure` 样本取累计 CPU 最大值减最小值，再求和；250ms 采样会漏掉边缘区间，不是精确请求边界计数。RSS 来自未插桩运行，不能与下述 heaptrack 原生堆峰值混为一谈；两个重复也不构成统计置信区间。

B1 实际命令：

```sh
uv run --locked --group test python -m tests.common.measure_protocol_http --binary target/benchmarks/bin/zero-copy-final-stravia-server --label zero-copy-final-b1 --output target/benchmarks/zero-copy-final-b1.json --warmup 0 --requests-per-case 2 --concurrency 1 --mode fixed-workers --delivery unary stream --ingress open-responses --min-first-token-delay 1 --max-first-token-delay 1 --min-duration 10 --max-duration 10 --min-tokens-per-second 30 --max-tokens-per-second 30 --build-condition "release no-default-features test-harness; unprofiled"
```

B2 仅把 label/output 中的 `b1` 改为 `b2`；A2 使用 `zero-copy-baseline-stravia-server` 和 `zero-copy-baseline-a2` 的 label/output，其他参数不变。A1 原始报告为 `target/benchmarks/zero-copy-baseline.json`。逐 case 摘要保存在 `zero-copy-final-comparison-summary.json`；三次严格报告校验均通过，未指定内存目标：

```sh
uv run --locked --group test python -m tests.common.report_protocol_http target/benchmarks/zero-copy-baseline.json target/benchmarks/zero-copy-final-b1.json --output target/benchmarks/zero-copy-final-comparison-b1.csv
uv run --locked --group test python -m tests.common.report_protocol_http target/benchmarks/zero-copy-baseline.json target/benchmarks/zero-copy-final-b2.json --output target/benchmarks/zero-copy-final-comparison-b2.csv
uv run --locked --group test python -m tests.common.report_protocol_http target/benchmarks/zero-copy-baseline-a2.json target/benchmarks/zero-copy-final-b2.json --output target/benchmarks/zero-copy-final-comparison-reverse.csv
```

### 最终完整链路矩阵

最终二进制的 4×4 协议 × unary/stream 矩阵共 32 case、64 请求，全部逐请求核验通过，零错误。实际输入 51,387–149,858 tokens、0–10 张图片；准确发布 128 条本轮新映射，普通观测、四方向 Wire、路由、Generation 父边/祖先及 SQL 因果链均通过。续接重放真实回复，而保护引用的随机 ID 会影响模型侧 tokenizer 预算，所以不同运行的续接输入可能相差少量 tokens；这不是固定字节重放。

|完整矩阵指标|冻结基线|最终版本|
|---|---:|---:|
|完整链路通过请求|64/64|64/64|
|Server CPU 采样窗口合计，秒|52.19|52.45|
|Server RSS 采样峰值，MB|1808.63|1725.17|
|有限 cohort 成功 QPS，含 drain|0.08495|0.08478|

单轮完整矩阵的 RSS 低约 4.6%，但重复小矩阵没有稳定改善；CPU 与 cohort QPS 基本不变。不能只选择这一个 RSS 下降结果宣布整体性能目标已达成。

另外两条最终版本实测：

- PostgreSQL/Valkey：默认随机时序，2 workers、4 请求、两条真实续接全部通过；实际首字等待 3.46–7.00 秒、34–99 token/s、生成 14–55 秒，图片覆盖 0–10 张，Server RSS 采样峰值 735.12MB。使用独占空数据库与清空后重新启动的专用缓存，不复用 E2E 缓存状态。
- SQLite open-loop：Chat→Gemini stream、2 workers、0.1 请求/秒，4 请求全部通过，峰值 in-flight 为 2；p95 admission lag 为 65.01ms、p95 queue 为 0.44ms、RSS 峰值 648.69MB。不是饱和负载。

实际命令：

```sh
uv run --locked --group test python -m tests.common.measure_protocol_http --binary target/benchmarks/bin/zero-copy-final-stravia-server --label zero-copy-final-matrix --output target/benchmarks/zero-copy-final-matrix.json --warmup 0 --requests-per-case 2 --concurrency 1 --mode fixed-workers --delivery unary stream --min-first-token-delay 1 --max-first-token-delay 1 --min-duration 10 --max-duration 10 --min-tokens-per-second 30 --max-tokens-per-second 30 --build-condition "release no-default-features test-harness; rebuilt vendor component"

uv run --locked --group test python -m tests.common.measure_protocol_http --binary target/benchmarks/bin/zero-copy-final-stravia-server --label zero-copy-final-pg-random --output target/benchmarks/zero-copy-final-pg-random.json --warmup 0 --requests-per-case 4 --concurrency 2 --mode fixed-workers --delivery stream --ingress open-responses --upstream open-responses --backend postgres --database-url postgresql://bench@127.0.0.1:34913/stravia_zero_copy_final --confirm-isolated-database --redis-url redis://127.0.0.1:36037/0 --build-condition "release no-default-features test-harness; rebuilt vendor component"

uv run --locked --group test python -m tests.common.measure_protocol_http --binary target/benchmarks/bin/zero-copy-final-stravia-server --label zero-copy-final-open-loop --output target/benchmarks/zero-copy-final-open-loop.json --warmup 0 --requests-per-case 4 --concurrency 2 --qps 0.1 --mode open-loop --delivery stream --ingress openai-chat --upstream google-content --min-first-token-delay 1 --max-first-token-delay 1 --min-duration 10 --max-duration 10 --min-tokens-per-second 30 --max-tokens-per-second 30 --build-condition "release no-default-features test-harness; rebuilt vendor component"
```

临时数据库与缓存已停止；复跑须重新创建本次独占的空环境，不可把示例端口替换成其他服务。

完整矩阵的严格报告校验也已通过，未指定内存目标：

```sh
uv run --locked --group test python -m tests.common.report_protocol_http target/benchmarks/fullchain-credentials-matrix-final.json target/benchmarks/zero-copy-final-matrix.json --output target/benchmarks/zero-copy-final-matrix-comparison.csv
```

### CPU 与原生堆剖析

CPU 诊断单独运行 Responses 入站 × 四种上游的 stream，共 8 请求，全部全链路验证通过。`perf record` 只附加实际 Server PID，在每个 measure 区间通过 control/ack 屏障启停，使用 `cycles:u`、99Hz、`dwarf,16384`，不包含之后的管理审计。i5-13600K 有 P/E 两类事件；下表只列 **P-core 用户态 self 样本占比**，不是整个进程 CPU 时间占比，也不把两个事件的百分比相加。

|最终版本叶符号|四个流式 case 的 P-core self 样本占比|
|---|---:|
|`core::str::iter::MatchIndices<&str>::next`|28.61%–60.23%|
|`regex_automata::hybrid::search::find_fwd`|5.97%–9.50%|
|`sha2::sha256::x86_sha::compress`|1.37%–3.76%|

Aho-Corasick 搜索也出现在热点前列。单个 libc `memmove` 符号最高为 2.90%，但它不包含所有内联复制和 guest 内复制，不能当作全部复制成本。字符串/正则扫描仍比该复制符号、SHA256 更突出；部分 DWARF 栈被截断，不能把全部 `MatchIndices` 样本直接归到某一条凭据规则。没有据此更换摘要算法或削弱检测。

heaptrack 通过只对 Server 子进程注入 preload 采集，不包裹 PID。基线与最终版本均为 Responses→Chat stream、两轮请求，开启全部链路；**分配统计包括冷启动、这两个请求和管理审计**，不是每请求或稳态统计：

|原生堆指标|冻结基线|最终版本|
|---|---:|---:|
|allocation 调用次数|11,069,551|10,706,802|
|工具判定的 temporary allocations|1,361,655|1,208,601|
|原生堆峰值，MB|281.44|272.48|

本次原生分配调用减少约 3.3%，临时分配减少约 11.2%，原生堆峰值低约 3.2%。普通预览脱敏原先两个显著分配栈分别有 121,542 / 121,528 次调用，后者全为临时分配；已消除未变化词元的小写副本及输出 String。冷启动规则编译、词表构建仍占大量分配，不能误说为每请求重新编译规则。

对最终 heaptrack 的退出存活分配按完整调用栈做互斥归类，结果如下。这是**存活分配，不是泄漏证明**；检测器由 `OnceLock` 持有。

|分配调用栈归属|退出时存活原生堆，MB|
|---|---:|
|检测器初始化，排除 tokenizer|86.08|
|检测器搜索及其余路径，排除 tokenizer|82.31|
|检测器 `tiktoken` 词表|13.69|
|其他或未归因|18.06|

检测器及其 tokenizer 合计约 182.09MB，约占该次退出存活原生堆的 91%。搜索阶段多条大分配栈指向 lazy-DFA 状态缓存和搜索 scratch。此归因不覆盖 guest 线性内存、全部 mmap、分配器空闲页或 profiler 开销，不能与不同负载的 1.73GB RSS 相减后宣布余量都是碎片。

原始文件保留在 `target/benchmarks/`：`zero-copy-final-profile.json`、`zero-copy-final-profile.perf/*.data`、`zero-copy-final-cpu-symbols.json`，以及 `zero-copy-{baseline,final}-heap.{json,raw,interpreted}`、对应 `heap-report.txt`、`zero-copy-final-retained.stacks` 和 `zero-copy-final-memory-summary.json`。使用 heaptrack 1.5.0 的 `heaptrack_interpret` 解释原始流，`heaptrack_print -m 0 -n 6 -s 1` 输出未合并峰值报告；存活分类来自 `--flamegraph-cost-type leaked` 的 folded stacks，而非采样 RSS。临时采集驱动完成后移除，不作为产品依赖或 benchmark 性能结果的一部分。

### 后续降低内存的优先级

1. **优先收敛检测器搜索缓存。** 当前全局 `OnceLock<Detector>` 已避免重复初始化；`expression.rs` / `kingfisher.rs` 的 `size_limit` 是编译结果上限，不是搜索缓存总预算。锁定的 regex 1.13.1 另有 `dfa_size_limit`；regex-automata 0.4.18 的搜索 scratch 池按同时使用峰值扩张、不主动收缩，不应误报成“每个历史线程永久一份”。应分别评估缓存容量、受控扫描并发、等价预过滤，以及相同模式/flags 的编译结果能否共享。减缓存可能增加重建与 fallback CPU；不得删规则、关闭 Unicode 语义或跳过凭据检测。
2. **缩短整请求的额外持有。** `RunObserver::capture_input_preview` 的 `pending_input` 目前持有完整 `Arc<AiRequest>`；凭据保护可能因其他所有者而触发 `Arc::make_mut`。可研究只保留预览所需文本，避免观察者额外 pin 整请求；仍须先完整脱敏再截取，并保持映射注册后的发布时序。其他所有者仍可能要求 COW，尚未量化节省量。
3. **用在途字节预算约束并发峰值。** 代理入口的 140MiB 是单请求 JSON 上限，RPM admission 不是活跃请求字节上限。Trace writer 的 1024 槽、普通 writer 的 2048 槽也不等于总字节预算。预算应覆盖形成大缓冲之前、queued 与 writing 的生命周期；保留 Admission/Finish/清理控制容量，不以丢历史、更多 partial 或降低成功请求负载美化成绩。代价是排队延迟或明确拒绝，必须同时报告。
4. **实时订阅按共享正文与聚合预算治理。** Live mailbox 各有 8MiB 上限，当前为每个订阅者 clone 内容；多慢订阅者会放大内存。这是实际 UI/订阅负载的机会，不是没有实时订阅者的本次 HTTP 矩阵 RSS 的归因。
5. **Wasm 与分配器分别测量。** 内置 Engine/编译结果已共享，owned host calls 已提前释放 lowered payload；不要重复列为待做。每操作仍独立 Store，不能为省内存复用权限状态。可测量实例化等待期间的载荷生命周期；Store 内存限额是防失控措施，不保证降低当前有效请求的 RSS，过低会导致 OOM。分配器驻留仍需同负载 `/proc/PID/smaps` 与 allocator 统计后再决定是否换分配器，不能把 malloc 存活量与 RSS 的差额全部归因于 glibc。

默认共享派生缓存只有 16MiB 逻辑预算，单独缩小它不能解释或解决 GiB 级 RSS；也不建议关闭完整观测、降低历史保留或改变 zstd level 1 来换取表面成绩。后续检测器缓存预算实测见下节；其余方向除已列出的堆归因外尚无节省量测量，未追加实施。

### 零拷贝回归验证

最终冻结二进制为 `target/benchmarks/bin/zero-copy-final-stravia-server`，SHA256 为 `4c453f75960e821c40eea1b6b1953f8417f68e83acf6fe7ae02fd88693e45e47`。已重建 Vendor Wasm；普通观测的逐阶段借用修正只改变宿主，无须再次重建未变化的 guest。

实际执行：

```sh
cargo fmt --all
cargo test --locked -p stravia-core --lib interaction_observation::redaction::tests
cargo clippy --locked -p stravia-core --all-targets -- -D warnings
cargo test --locked -p stravia-core --lib
cargo build --locked --release -p stravia-server --no-default-features --features test-harness --bins
```

普通脱敏定向测试 24 项通过，Core 全量 1294 项通过、4 项按现有配置忽略；Clippy 与 release 构建通过。新的行为回归覆盖已掩码前缀后继续脱敏、大小写混合的连续认证 scheme，以及带引用的 form 中空分量、分隔符和尾部保真，不用内部 `Cow` 变体断言代替安全语义。

最终二进制还实际通过 30 项 HTTP/WebSocket 与 SQLite/PostgreSQL E2E，覆盖四方向 Wire、无效 UTF-8、重复 Header、跨片段 SSE、内联图片、WebSocket 多轮与断连、真实重启后的凭据映射还原、历史压缩及观测/Trace 重放。命令使用本次独占本地 PostgreSQL/Valkey，`STRAVIA_BINARY` 指向上述冻结文件，`STRAVIA_STORAGE_HARNESS_BINARY` 指向本次 release 构建的 `stravia-storage-test-harness`：

```sh
uv run --locked --group test python -m pytest \
  tests/e2e/admin/test_observation_items.py \
  tests/e2e/admin/test_observations.py::test_websocket_waiting_client_disconnects_without_losing_generation \
  tests/e2e/storage/test_compaction_storage.py \
  tests/e2e/storage/test_interaction_continuation.py \
  tests/e2e/storage/test_storage.py::test_redaction_reuses_and_restores_mappings_after_real_restart \
  tests/e2e/storage/test_storage.py::test_observation_tool_replay_and_trace_survive_restart \
  tests/e2e/storage/test_storage.py::test_failed_request_projection_survives_restart_and_clear -q
```

这里的 WebSocket 行为覆盖不把 HTTP 基准扩充成 WebSocket 性能矩阵；`DB_URL` 与 `STRAVIA_TEST_REDIS_URL` 仍须满足仓库的隔离测试环境要求。

## 检测器缓存预算与重复规则

### 固定规则，仅改变 lazy-DFA 缓存预算

结论：**保留锁定版本 regex 1.13.1 的默认 2MiB 预算**。这是单个 Regex 搜索缓存的容量参数，不是检测器或进程的总内存上限。减小它没有按比例降低 RSS，却明显增加 CPU；本轮不新增生产配置或依赖。

新增 `tests/common/measure_credential_detection.py`，经真实 Server、认证、SQLite 和 `/api/v1/reversible-redaction/test` 测量，而不是直接调用孤立 Regex。该诊断接口按既有契约不生成历史、普通观测或凭据映射；脚本未关闭这些功能，但**诊断成绩不是推理全链路成绩**。最终产品另跑带完整证据核验的 HTTP 请求链路。

负载固定为精确计数的 50k、100k、150k `cl100k_base` token 文本和 11 个小探针，共 14 个输入。探针覆盖 Unicode/UTF-16 位置、重复秘密、组合规则正反例、熵过滤、跨行私钥、引用边界、Bittrex/Plaid 重复规则及 Brevo/Sendinblue 不同置信度。每种预算新进程、新库；先单独记录启动、首次检测和冷输入，再依次测并发 1 与 4，各预热 1 轮、测量 3 轮。每轮每个输入发送与并发数相同的副本，因此 warm CPU 总量分别对应 42 和 168 个请求；并发 4 阶段保留前一阶段的缓存高水位。

同一个未去重的实验二进制按 `2MiB → 1MiB → 256KiB → 64KiB → 64KiB → 256KiB → 1MiB → 2MiB` 顺序执行，测量期间不并行构建或测试。CPU 使用 Server PID 的累计 user+kernel 端点差，RSS 为 20ms 采样峰值。下表 CPU 为两次运行均值，RSS 为两次峰值的范围，单位 MB 为十进制；两个样本不足以构造统计置信区间。

|每 Regex 缓存预算|并发 1 warm CPU，s|并发 4 warm CPU，s|并发 4 CPU 相对默认|并发 1 RSS 峰值，MB|并发 4 RSS 峰值，MB|
|---|---:|---:|---:|---:|---:|
|2MiB，默认|2.790|11.115|基线|560.03–560.81|689.95–692.21|
|1MiB|3.220|12.775|+14.9%|550.31–552.59|664.17–681.21|
|256KiB|3.865|15.460|+39.1%|560.47–570.72|669.36–678.43|
|64KiB|4.480|17.715|+59.4%|550.97–552.57|655.50–659.39|

64KiB 的并发 4 平均峰值只比默认低约 33.64MB（4.9%），CPU 却增加约 59.4%；256KiB 的单并发 RSS 甚至没有稳定改善。首次 150k 检测 CPU 也从默认 0.75–0.78s 增至 64KiB 的 0.97–0.98s。RSS 包含编译结果、scratch、分配器驻留及其他服务内存，不能把缓存容量当作已分配字节，更不能把此结果外推成完整推理服务的吞吐提升。

八次实验的输入摘要和完整检测元组摘要全部一致。比较字段为 `rule_id/start/end/start_line/start_column/end_line/end_column`，不是只比较匹配数。实验二进制冻结为 `target/benchmarks/bin/detector-budget-confirmed-server`，SHA256 为 `32e9110d83e73ffc8447e8f84d5cba97bae9cc06704c385877a413657b5852d8`；它使用去重前的规则，仅在临时实验副本中为检测器 Regex builders 注入预算，并在首次实际使用时确认 `STRAVIA_BENCH_DFA_CACHE_APPLIED`。生产源码没有该环境变量开关或实验路径。普通产品二进制不得冒充已应用预算，脚本在未收到确认时返回失败。

可在当前工作目录使用冻结实验文件重放，例如：

```sh
uv run --locked --group test python -m tests.common.measure_credential_detection \
  --binary target/benchmarks/bin/detector-budget-confirmed-server \
  --output target/benchmarks/detector-budget-replay.json \
  --label replay-1m --cache-budget-bytes 1048576
```

省略 `--cache-budget-bytes` 测默认预算；指定预算仅适用于已插桩实验文件，不是普通 release/test-harness 构建的功能。原始八轮数据为 `target/benchmarks/detector-confirmed-*.json`，聚合为 `detector-cache-budget-summary.json`；插桩差异与源码摘要保存在 `detector-budget-instrumentation.patch`、`detector-budget-source-hashes.json`，临时源码副本已移除。较早 `detector-cache-*.json` 使用未扩展的探针集合，不与本表混合。

### 删除等价身份，不删除检测覆盖

在编译规则之前删除以下三条重复身份，保留同源首次出现者及扫描顺序：

|移除身份|保留身份|
|---|---|
|`bittrex-secret-key`|`bittrex-access-key`|
|`kingfisher.jfrog.1`|`kingfisher.artifactory.2`|
|`kingfisher.plaid.3`|`kingfisher.plaid.2`|

候选仅限已审查的身份对，并逐项比较有效策略；过滤、熵、置信度、可见性、捕获、校验或组件条件不同就不合并。Betterleaks 组件引用同步迁移；若父规则同时依赖两个身份，则保留两者以免折叠多重条件。不会跨 Betterleaks/Kingfisher 适配器合并 Unicode、safe-list 或过滤语义不同的规则。Brevo/Sendinblue 虽然模式相同，但置信度不同，实际检测仍保留两条。

上游原始快照、版本校验与许可保留；有效目录从 1475 降为 1472。新检测不再返回上述三个旧 ID；旧历史不重写，现有 UI 对目录外 ID 显示原 ID。没有添加兼容别名、数据库迁移或改变正文指纹。

最终产品文件为 `target/benchmarks/bin/detector-deduplicated-server`，SHA256 为 `145616fe00ed0ad5de8365e46040f36e21f06259ae42edba057373a425261c91`。真实诊断 API 对全部 14 个输入验证：旧结果仅替换上述 ID 并消除完全相同的元组后，与新结果逐位置相等；新目录恰好只少这三个身份。证据为 `detector-deduplicated-diagnostics.json`、`detector-dedup-semantic-proof.json`。本轮没有以少三个规则就推断可观的内存或吞吐收益。

实际通过 `cargo test --locked -p stravia-credential-protection --lib` 的 32 项测试、该 crate 全 target Clippy、`cargo check --locked -p stravia-core` 及最终 release 构建。使用最终冻结文件，`test_credential_protection.py`、`test_reversible_redaction.py`、`test_redaction_diagnostics.py`、`test_redaction_wire.py` 共 39 项真实 HTTP E2E 通过，覆盖目录、位置、组件隔离、只读边界、保护与还原和 Wire 行为。本轮没有重跑 PostgreSQL、桌面或全仓测试矩阵。

最终产品还完成 Responses 入口到四类 upstream、unary/stream 共 8 cases、16 次真实请求。输入使用原 schema 4 的 50k–150k token 与随机 0–10 张图片，本次缩短生成计划为首 token 延迟 1s、30 token/s、持续 10s，以验证行为而不是代替完整性能矩阵。16/16 请求通过路由选择、父链/chain ID、四方向 Wire 摘要、普通观测脱敏、凭据发现/映射发布/流式还原、usage、交付及解码历史逐项核验；持久化 Run、Model Turn、Target attempt 和历史节点均为 16，普通事件 176。凭据日志泄漏、外键失败和最终入队失败告警均为零；未关闭普通观测、Debug、凭据保护或历史。RSS 采样峰值 1117.58MB，没有同期配对矩阵，不能据此宣称去重降低了整体内存或达到 250MB 参考。

```sh
uv run --locked --group test python -m tests.common.measure_protocol_http \
  --binary target/benchmarks/bin/detector-deduplicated-server \
  --label detector-final-fullchain --output target/benchmarks/detector-final-fullchain.json \
  --warmup 0 --requests-per-case 2 --concurrency 1 --mode fixed-workers \
  --delivery unary stream --ingress open-responses \
  --min-first-token-delay 1 --max-first-token-delay 1 \
  --min-duration 10 --max-duration 10 \
  --min-tokens-per-second 30 --max-tokens-per-second 30 \
  --build-condition 'release no-default-features test-harness; unprofiled'
```

原始与汇总证据分别为 `detector-final-fullchain.json`、`detector-final-fullchain-summary.json`。最后用同一产品文件给诊断脚本传 `--cache-budget-bytes 65536`，实际返回 1，报告 `failure_reason=cache_budget_not_confirmed`；这是预算未生效时必须拒绝成绩的预期检查，记录在 `detector-budget-rejected.json`。

## 优化前长上下文与凭据全链路基线（schema 4）

以下为零拷贝实施前的真实基线，不是旧短输出成绩的重新标注。Server SHA256 为 `b93b0200ea560d8f7526c49e4c74ea01e3b3094cf83798b89e0751a0fb70bb2e`；Linux x64，release、`--no-default-features --features test-harness`，Vendor Wasm 已重建。所有运行开启普通 Observation、Debug、历史与可逆凭据保护；测量期间不并行构建或测试。

|运行|case / 请求|逐请求全链路通过|总运行秒，含初始化与核验|Server RSS 采样峰值 MB|Server CPU 采样峰值，核|
|---|---:|---:|---:|---:|---:|
|SQLite，4×4 协议 × unary/stream，1 worker|32 / 64|64 / 64|788.28|1808.63|1.08|
|PostgreSQL，Responses→Responses stream，2 workers，默认随机时序|1 / 4|4 / 4|123.62|710.82|1.96|
|SQLite，Chat→Gemini stream，2 workers，open-loop 0.1 请求/秒|1 / 4|4 / 4|44.35|684.40|1.04|

矩阵使用每请求首字延迟 1 秒、模型生成 30 token/s、持续 10 秒，以完整大小的有限 cohort 验证全部协议/交付组合。实际客户端输入为 51,387–149,860 tokens，图片覆盖 0–10 张；流式客户端 TTFT 为 1,295.96–2,052.04ms，包含实际网关处理成本，不等于模拟上游的 1 秒等待。64 个请求准确发现并发布 128 条新映射，每条续接复用父轮映射；普通观测、Wire、路由、历史及凭据只读数据库证据全部通过，Server 日志未命中合成秘密。

PostgreSQL 使用独占本地 PostgreSQL **16.15** 与 Valkey **9.1.2**，不是 Redis 8 容器；Docker 在该环境不可用。数据库与缓存各自新建、仅监听 loopback，完成后已停止并清理。该次实际随机计划为首字延迟 3.46–7.00 秒、34–99 token/s、14–55 秒生成，图片 0–10 张，模型输出 589–5,347 tokens；4 个请求包含两条真实续接，峰值 in-flight 为 2。表中只统计 Server PID，不包含 PostgreSQL/Valkey 进程占用。

open-loop 按计划接纳 4 个请求，峰值 in-flight 为 2、真实续接 2 次、零错误；p95 admission lag 为 57.71ms，p95 queue 为 0.35ms。有限 cohort 含 drain 的成功 QPS 为 0.0961，**不是系统吞吐上限或稳态性能成绩**。没有运行默认 64 case、32 workers、120 秒 admission window 的持续负载矩阵。

**该基线没有达到 250MB。** 显式 `--budget-mb 250` 的报告验收返回 1。1,808.63MB 是同一 Server PID 按固定顺序保留真实历史、观测与缓存跑完整矩阵的采样峰值，不是单请求所需内存，也不是内存泄漏证明。该轮未采集 CPU/heap profile；后续零拷贝调查另行采集，不能把这里的 SQL 壁钟耗时、低占用等待区间或源码候选冒充 CPU 热点排名。

实际命令如下；复跑 PostgreSQL 命令前须重新准备同规格的独占空数据库和缓存，不能把已清理的临时端口替换成其他环境：

```sh
uv run --locked --group test python -m tests.common.measure_protocol_http --binary target/release/stravia-server --label fullchain-credentials-matrix-final --output target/benchmarks/fullchain-credentials-matrix-final.json --warmup 0 --requests-per-case 2 --concurrency 1 --mode fixed-workers --delivery unary stream --min-first-token-delay 1 --max-first-token-delay 1 --min-duration 10 --max-duration 10 --min-tokens-per-second 30 --max-tokens-per-second 30 --build-condition "release no-default-features test-harness; rebuilt vendor component"

uv run --locked --group test python -m tests.common.measure_protocol_http --binary target/release/stravia-server --label fullchain-credentials-pg-random --output target/benchmarks/fullchain-credentials-pg-random.json --warmup 0 --requests-per-case 4 --concurrency 2 --mode fixed-workers --delivery stream --ingress open-responses --upstream open-responses --backend postgres --database-url postgresql://bench@127.0.0.1:54537/stravia_fullchain_credentials --confirm-isolated-database --redis-url redis://127.0.0.1:55241/0 --build-condition "release no-default-features test-harness; rebuilt vendor component"

uv run --locked --group test python -m tests.common.measure_protocol_http --binary target/release/stravia-server --label fullchain-credentials-open-loop --output target/benchmarks/fullchain-credentials-open-loop.json --warmup 0 --requests-per-case 4 --concurrency 2 --qps 0.1 --mode open-loop --delivery stream --ingress openai-chat --upstream google-content --min-first-token-delay 1 --max-first-token-delay 1 --min-duration 10 --max-duration 10 --min-tokens-per-second 30 --max-tokens-per-second 30 --build-condition "release no-default-features test-harness; rebuilt vendor component"
```

原始 JSON 保留在上述 `target/benchmarks/` 路径。`fullchain-report-smoke.csv` 用最终矩阵与自身对照验证报告 CLI，不代表优化前后收益；旧 schema 被实际拒绝，显式 250MB 上限被实际判为未达到。基准相关 Python 回归 163 项通过；Anthropic codec 相关回归 124 项、凭据保护 crate 23 项通过，Rust 格式检查与 release 构建通过。

相关回归与报告 smoke 的实际命令：

```sh
uv run --locked --group test python -m pytest tests/e2e/proxy/test_protocol_workload.py tests/e2e/proxy/test_protocol_audit.py tests/e2e/proxy/test_protocol_history_audit.py tests/e2e/proxy/test_protocol_benchmark.py tests/e2e/proxy/test_history_benchmark.py -q
cargo test --locked -p stravia-protocol-codec --lib anthropic
cargo test --locked -p stravia-credential-protection --lib
cargo fmt --all -- --check
uv run --locked --group test python -m tests.common.report_protocol_http target/benchmarks/fullchain-credentials-matrix-final.json target/benchmarks/fullchain-credentials-matrix-final.json --output target/benchmarks/fullchain-report-smoke.csv
```

### 基准暴露的问题及纠正

- Anthropic 初始已知 output=0 遮蔽只含最终 output 的 usage 修订：已在协议解析器按字段合并累计值，真实矩阵验证最终 usage 与实际模型输出一致，未改写历史数据。
- Gemini 响应级元数据错误切断跨片段凭据引用：已修复流式还原的 item 边界判断；修复前真实客户端收到原引用，修复后客户端和持久历史均匹配完整还原正文。
- fixture 原先漏读保护系统指令，以及把普通观测中合法保留的完整引用误判为秘密泄露：按现有协议与脱敏契约修正 oracle，没有关闭保护或改变产品安全策略。早期失败 JSON 保留，未改写成通过结果。

## 2026-10-09 实测

以下为旧 schema/负载的历史结果，不能作为当前长上下文、图片、长输出及 Debug 全链路口径的成绩。

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

## Linux：deep-module 切换后的可靠性与压缩评估

本轮在 Linux 6.18.52、i5-13600K（20 logical CPUs）、62 GiB RAM 上执行，无 CPU quota。先 fast-forward 引入 `optimize-deep-module` 的 `dd88d6d0886cc07c12a42d1fbb45e42db4e5eafd`，再修复观测可靠性并复用不可变历史语义投影。两者使用相同构建条件：

```bash
cargo build --locked -p stravia-server --bin stravia-server \
  --release --no-default-features --features test-harness \
  --config profile.release.debug=1
```

分支基线二进制 SHA256 为 `c644057a43fec1fe480e71f9e58b9449b5e40658afd9bb73f0a845498cd2b326`，可靠性修复后、切换默认压缩等级前的 level 3 二进制为 `7e5b4efb9b3e8be885233bb9663f61771436a3a7cc8a38bafa7233184519e684`。`source_commit` 标明分支基点，不冒充未提交工作树的完整身份；具体测量以冻结二进制及其 SHA 为准。

低等级实验只把 `storage_codec.rs` 的 `Compressor::new(3)` 临时改为 `new(1)`，冻结二进制 SHA256 为 `ac0bb1f3a8e5f9e0e0eda946c642ff3c06cbdec10c3500b702b0243c5eb59a18`。随后恢复源码并重新构建默认等级，SHA 与上述 level 3 一致。所有运行保持历史和普通 Observation 开启、Debug Observation 关闭；writer 告警归零不证明记录全量，仍需检查 Run/Turn/Attempt 与历史 inventory。

### 可靠性验证

在旧代码上分别复现了拒绝准入后继续发送孤儿 Run 命令、缺失 Model Turn 开始导致事件批次外键失败，以及后台发布句柄存活时 Finish 丢失历史关联。修复后的真实 HTTP 队列满场景又暴露出已交付响应的 `client_output_committed=false`；现由实际输出事实进入保留 Finish，不从 HTTP 状态推断。生命周期与缺口契约见 [Interaction Observation](../design/interaction-observation.md#53-顺序与-sse-cursor)。

使用隔离的本机 PostgreSQL 16.15、Redis 协议服务 Valkey 9.1.2，以及测试内自建的 loopback catalog/upstream，不访问生产服务。真实 server HTTP 回归：

- `test_diagnostic_limits_preserve_real_host_inference -k server`：SQLite/PostgreSQL 队列满与 Trace 限额，共 4 passed；验证请求成功、Run 终态、已确认 Client Output Commit 与已提交历史关联。
- `test_storage_failure_preserves_active_and_following_inference -k 'server and observation'`：SQLite/PostgreSQL 注入 I/O、容量失败，共 6 passed；验证当前及后续真实推理。

### 压缩纯 CPU 与编码体积

使用生产依赖 `zstd 0.13.3`，复现线程内 Compressor 复用、checksum、无字典及 14 字节 storage trailer；每个等级的预热输出均由生产 `decode` 逐字节校验。5 轮轮转等级顺序，以 `[3, 1, 2]` 为起点；表中为每次编码的 CPU 时间中位数，CPU clock 取子进程 user+system。三类输入每轮分别运行 100、20,000、500 次；原文大小、SHA256、次数和原始 CSV 见 `target/benchmarks/compression-cost-summary.json` 与 `compression-{varied-5mb,varied-8kb,repetitive-5mb}.csv`。

|输入 / 原文字节|zstd 等级|CPU 中位数，ms/次|编码字节（含 trailer）|
|---|---:|---:|---:|
|varied / 5000074|1|3.000|2502028|
|varied / 5000074|2|3.900|2503440|
|varied / 5000074|3|19.500|2601813|
|varied / 8266|1|0.008|4283|
|varied / 8266|2|0.017|4407|
|varied / 8266|3|0.024|4395|
|repetitive / 5000074|1|0.560|245|
|repetitive / 5000074|2|0.560|245|
|repetitive / 5000074|3|0.760|245|

varied padding 与 HTTP benchmark 使用相同 SHA256 counter-hex 生成方法，但此微测 JSON 原文大小不等于 HTTP 的精确 5000000 字节。纯 codec 不含 JSON、SHA、SQL、Delivery 或观测成本，不能把这里的 CPU 降幅当作整个 Server 的降幅。输入也不是实际中文/代码语料；上述评估结束时默认仍为 level 3，后续按用户决定切换为 level 1，见下节。不增加配置开关，不改变历史、观测、trailer 或持久化指纹契约。

### 完整 HTTP 链路与体积代价

使用新进程及新 SQLite、`Gemini → Chat / stream / open-loop`、精确 5000000 字节 varied 输入、10 QPS、32 workers 上限、1 秒 upstream delay、5 秒 warmup、120 秒测量、250 ms 采样。默认 CPU 亲和性为 `0-19`。按 level `3 → 1 → 1 → 3` 运行，全部保留，不挑最快样本；四次均 1200 次测量加 50 次 warmup 成功，原生协议终态验证通过。

|等级 / 顺序|CPU，约 ms/测量请求|CPU p50，核|端到端 p95，ms|RSS 峰值，MB|采样 Run 条数|
|---|---:|---:|---:|---:|---:|
|3 / 首轮|127.27|1.238|1184.43|865.17|1250|
|1 / 首轮|149.97|1.238|4196.28|1375.76|1221|
|1 / 反序|137.81|1.317|1263.81|915.26|1250|
|3 / 反序|161.43|1.557|1276.10|889.44|1250|

CPU/请求为该 case 测量阶段首末 Server 样本的累计 user+kernel CPU 差除以 1200，包含已采到的 drain；边界最多漏掉一个采样间隔附近的工作，不是精确逐请求计费。CPU 中位数不能代表全部累计成本，首轮 level 1 的突发、排队与内存上升仍保留。

每次均保留 1250 个历史节点、1250 条外置正文及对应引用；level 1 首轮采样时只有 1221 个 Run、1210 个 Target attempt，其余三次均各 1250。两类观测写入/最终入队告警及外键 787 均为零，但这不能证明普通 Observation 全量；采样也不保证 writer 已完全排空。不能把不同观测覆盖下的 CPU 当作严格等工作量的因果结论。默认亲和性的四次 CPU 成本区间重叠，尚不能宣称低等级让整个 Server 稳定更快。

相同历史条数下，默认亲和性两轮的存储范围如下；GB/MB 均为十进制，数据库和 WAL 是文件逻辑长度：

|等级|外置正文 BLOB，GB|节点 payload，MB|SQLite database，GB|WAL，MB|
|---|---:|---:|---:|---:|
|3|3.252209–3.252212|1.229–1.230|3.279917–3.282551|6.856–6.872|
|1|3.127497–3.127497|1.246–1.246|3.153871–3.154559|6.802–6.810|

这组输入中 level 1 外置正文编码量下降约 3.83%，小型节点元数据略增。未测文件系统已分配块数、写放大或 SSD 寿命，不能把编码率当作全部磁盘 I/O 成本。原始结果为 `reliable-level{3,1}-compression-{pair,repeat}.json`，聚合及限制保存在 `compression-http-repetitions.json`。

另以 `taskset --cpu-list 0,2,4,6,8,10` 启动同一入口，harness 与 server 继承相同亲和性。`lscpu` 确认这六个逻辑 CPU 属于不同物理核心、各自最大频率为 5100 MHz；未更改整机 governor。负载参数不变：

|等级 / 受限亲和性|case 有效|成功 QPS（含 drain）|CPU，约 ms/测量请求|端到端 p95，ms|采样 Run 条数|
|---|---|---:|---:|---:|---:|
|3|否|9.329|193.78|8090.75|1207|
|1|是|9.909|127.48|1428.50|1250|

两次仍各完成 1250 次请求并保留完整历史，无 HTTP 错误。level 3 未达到原有 95% QPS 门槛，作为过载证据保留，不能冒充稳定 10 QPS 基线，也不放宽门槛。此组改变了进程资源约束，不能与默认亲和性混算；本轮没有逐线程运行 CPU 证据，不能据此把默认组方差归因于 P/E 迁移。原始结果为 `reliable-level{3,1}-compression-pcores.json`，限制与聚合见 `compression-http-pcores-comparison.json`。

### 同参数 64-case 矩阵：没有证明全面提速

分支基线与修复版本均运行四协议两两转换、unary/stream、open-loop/fixed-workers 的完整 64 cases，采用默认 30 秒测量、5 秒 warmup、精确 5 MB varied 输入、1 秒 upstream delay、10 QPS 或 32 workers、250 ms 采样。每次完整矩阵内共享进程、SQLite 与缓存，跨版本新建进程与数据库；顺序固定，数据规模随 fixed-workers 实际吞吐累积，不能当作独立随机试验。测量期间不并行构建、测试或其他剖析。

|指标|deep-module 分支基线|可靠性及投影修复，level 3|
|---|---:|---:|
|有效 cases|64/64|63/64|
|HTTP/原生协议错误，含 warmup|0|0|
|成功请求，含 warmup|38822|34230|
|历史节点|38822|34230|
|采样 Run 条数|36174|28071|
|采样 Target attempt 条数|35407|26579|
|观测事件写入失败告警|236|0|
|其中外键 `ForeignKeyViolation:787`|236|0|
|`observation finalization unavailable`|928|0|
|Server RSS 峰值，MB|2039.87|1943.25|

|负载 / 版本|有效 cases|成功 QPS 范围|各 case CPU p50，核|各 case 端到端 p95，ms|
|---|---:|---:|---:|---:|
|open-loop / 分支基线|32/32|9.629–9.669|1.158–1.518|1142–1372|
|open-loop / 修复|31/32|9.312–9.667|1.318–3.192|1183–4053|
|fixed-workers / 分支基线|32/32|15.272–24.992|3.990–5.526|1426–2725|
|fixed-workers / 修复|32/32|8.256–23.720|3.253–5.661|1446–5026|

修复版本的 `Gemini → Chat / stream / open-loop` 以 9.312 QPS 未达到原有门槛，矩阵退出 1；不是 HTTP 错误，也不隐藏该 case。最差配对 `Anthropic → Gemini / unary / fixed-workers` 从 23.870 降为 8.256 QPS。采样 Run/成功请求比例从约 93.18% 变为 82.01%；只能称为采样覆盖，不能据此推断最终丢失数。历史条数与所有成功请求一致不等于普通观测全量，零告警也不等于没有准入拒绝或有界事件缺口。

因此，本轮**不能宣称全矩阵提速，也不能把更低 RSS 当作同等有效工作量下的内存收益**。250 MB 的既有内存参考仍未达到，未放宽门槛，也没有关闭历史或普通观测。

原始数据为 `deep-module-baseline-full.json`、`reliable-final-full.json`；逐 case 配对保存在 `reliable-final-paired-metrics.json`。最差组合另做相同 60 秒、32 workers、新进程/新库的低开销 leaf 剖析对照：

|诊断样本|成功 QPS|CPU，约 ms/测量请求|采样 Run / 总成功请求|外键告警|最终入队告警|
|---|---:|---:|---:|---:|---:|
|分支基线|15.671|216.55|716 / 1073|4|260|
|修复版本|23.089|191.89|1113 / 1520|0|0|

新库诊断样本出现相反方向，不能拿它替换共享进程/累积库的正式矩阵，也不能据一轮矩阵把退化直接归因于新增父检查或正文投影。两次保留相同 upstream delay 与真实历史写入；普通观测均开启，采样仍非完整。父检查是事务内的一次存在性查询；本轮 CPU self 样本不能证明它是墙钟瓶颈，不能以猜测移除可靠性判断。诊断文件为 `deep-module-baseline-worst-fixed-profile.json`、`reliable-worst-fixed-profile.json`。

完整矩阵的 `report_protocol_http` 按原 250 MB 阈值导出 `deep-module-vs-reliable-final.csv`，返回 1，`candidate_valid=false`、`normal_memory_below_budget=false`、`meets_target=false`。压缩首轮对照导出 `compression-level3-vs-level1.csv` 同样返回 1：该单 case 本身有效，但内存参考未通过。没有用放宽阈值把报告改成通过。

### 可靠性修复后、level 3 时的 CPU 热点

另对默认亲和性、新进程/新库的 `Gemini → Chat / stream / open-loop` 采集 60 秒稳态 leaf profile。600 次测量加 50 次 warmup 全部成功，650 个 Run、Model Turn、Target attempt 与历史节点均已进入采样快照；成功 QPS 为 9.830，Server CPU p50 为 1.318 核，端到端 p95 为 1528 ms。此诊断结果不替代上面的失败矩阵。

采用 `perf-linux 7.2.10`、`cpu-clock:u`、99 Hz、`--clockid mono`，不采 DWARF 或调用链。报告同时按真实 Server PID 与该 case 测量阶段的 monotonic 窗口过滤，使用 `--percentage relative`，按原始 period 加权，不累加四舍五入后的百分比。稳态 6422 个样本、高并发修复版本 20812 个样本，lost samples 均为零：

|可识别 self 符号分组|Gemini → Chat / 10 QPS，%|Anthropic → Gemini / 32 workers，%|
|---|---:|---:|
|Zstd 及 codec helpers|23.36|23.38|
|JSON 序列化/反序列化|21.99|23.30|
|SHA256|18.09|14.52|
|内存复制/清零|8.86|13.94|
|未解析 JIT/unknown|10.34|9.75|
|其他|17.36|15.12|

可明确识别的热函数包括 `ZSTD_compressBlock_doubleFast`、`sha2::sha256::x86_sha::compress`、`__memmove_avx_unaligned_erms`，以及写入 canonical `HashWriter`、`json::ByteCount`、外置内容 `HashWriter` 的 `serde_json::ser::format_escaped_str`。token 估算的 `CountBytes` 与普通观测脱敏扫描也仍有可见成本。JSON 分组不能自动等同所有 Provider/Wasm JSON 成本；未解析 JIT 不强行归给任何转换器，memmove 的 leaf 也不能证明其调用方。

本轮已消除为了单元计数而重建 owned 历史投影的路径，并把最终不可变客户端投影的摘要与单元数一起计算、传入提交；没有宣称消除了所有正文扫描。canonical 长度前缀仍要求计字节与散列两遍，保持持久化字节契约；canonical、外置内容键、token 估算不能未经语义证明互换长度或摘要。剩余主 CPU 成本仍是压缩、JSON 扫描和 SHA。leaf self 不包含锁、I/O 或 async 等待，不能用这个 CPU 表解释所有墙钟吞吐抖动。

原始数据及元数据分别在 `reliable-google-chat-leaf-profile.json`、`reliable-google-chat-leaf.perf.data`、`reliable-google-chat-leaf-perf-report.txt`、`reliable-current-cpu-summary.json`；高并发对应 `reliable-worst-fixed.perf.data` 与 `final-worst-fixed-cpu-summary.json`。Rust 定向回归、Core/Server check、全工作区格式检查与真实双后端 HTTP 验证记录见 `target/benchmarks/verification-summary.json`。上述 level 3 评估未运行全仓 `task test:all` 或桌面实机验证；当时未修改迁移、锁文件或默认压缩等级的 ADR。后续默认等级切换如下。

## 生产默认切换为 level 1 与 SHA256 后端筛选

按用户决定，生产 `storage_codec::encode` 的默认 zstd 等级从 3 改为 1。128B 门槛、仅在压缩后更小时选用 zstd、checksum、线程内上下文复用、14 字节 trailer 及版本均不变；不关闭历史或普通 Observation。`reads_existing_large_level3_payload` 使用切换前生产编码器捕获的固定帧，校验完整 5000000 字节旧载荷可读；连同二进制/Unicode 往返及损坏拒绝用例，定向 `cargo test --locked -p stravia-core --lib storage_codec::tests::` 为 3 passed。没有修改摘要算法、持久化键、schema 或锁文件。

使用上文相同发布构建条件重新构建，冻结为 `target/benchmarks/bin/default-level1-stravia-server`，SHA256 为 `b5adf6b85e94f77417b3e990afad56e6da0b70588ded53b710b02e18a274b5f7`。随后对新进程、新 SQLite 运行真实 HTTP 冒烟：

```bash
uv run --locked --group test python -m tests.common.measure_protocol_http \
  --binary target/benchmarks/bin/default-level1-stravia-server \
  --label default-level1-http-smoke \
  --output target/benchmarks/default-level1-http-smoke.json \
  --source-commit dd88d6d0886cc07c12a42d1fbb45e42db4e5eafd \
  --build-condition 'dirty worktree reliability and immutable projection fixes; production zstd level 1; checksum and history/ordinary Observation retained; release no-default-features test-harness debug=1; binary SHA identifies full artifact' \
  --ingress google-content --upstream openai-chat --delivery stream \
  --mode open-loop --duration 60 --warmup 5
```

其余使用原有 5 MB varied、10 QPS、32 workers 上限、1 秒 upstream delay、250 ms 采样默认值。600 次测量加 50 次 warmup 全部成功，HTTP/原生终态错误为零，case 有效；历史节点、外置正文、引用、Run、Model Turn 和 Target attempt 在同一采样快照中均为 650 条，事件为 6500 条。外键、观测事件写入和最终入队失败告警均为零；保留两条隔离 loopback 测试的管理 Origin/明文 HTTP 安全提示，未把全部告警宣称为零。

成功吞吐为 9.834 QPS（含 drain），端到端 p95 为 1125.41 ms，Server CPU p50 为 0.959 核，约 96.62 CPU ms/测量请求，RSS 峰值为 782.68 MB。外置正文 BLOB 共 1626298416 字节；完整节点、事件及数据库/WAL 长度见 `target/benchmarks/default-level1-http-smoke-summary.json`。这是单 case 冒烟，没有同期配对基线，不能据此宣称整个 Server 的提速比例；也未达到原 250 MB 内存参考。本次没有重跑 64-case、PostgreSQL HTTP 或桌面验证，不覆盖上述正式矩阵的失败结果。

### 同 SHA256，替换实现是否值得

当前 `sha2 0.11.0` 已有运行时硬件检测；官方的 x86 SHA-NI 后端见 [sha2 Backends](https://docs.rs/sha2/0.11.0/sha2/#backends)。上文实际 profile 命中 `sha2::sha256::x86_sha::compress`，不是尚未开启加速的纯软件路径。

使用已存在的 release 依赖产物，把 RustCrypto `sha2 0.11.0` 与 `aws-lc-rs 1.18.1` 的 SHA256 单独对照；[AWS digest API](https://docs.rs/aws-lc-rs/1.18.1/aws_lc_rs/digest/index.html) 支持一次性与增量摘要。没有增加生产依赖或改变产品调用。探针由 Rust 1.99.0、`rustc --edition=2024 -O` 编译，固定到 CPU 0 的单个 P-core，无并发构建、测试或 HTTP 测量。每个 case 各预热 3 次，5 轮交替实现先后次序，取 `CLOCK_PROCESS_CPUTIME_ID` 的 CPU 中位数：

|输入 / 更新方式|每轮次数|sha2，µs/摘要|aws-lc-rs，µs/摘要|
|---|---:|---:|---:|
|32B / 一次性|500000|0.040637|0.045018|
|8266B / 一次性|20000|3.361169|3.360016|
|5000074B / 一次性|50|2016.543980|2014.706100|
|5000074B / 4096B 分块|50|2018.474840|2018.907100|
|5000074B / 64B 分块|50|2183.803400|2202.641040|

空串与 `abc` 的已知 SHA256 向量，以及每组相同输入的完整/增量摘要均一致。大正文一次性摘要差约 0.09%，分块未见收益，32B 输入的 AWS 实现反而更慢；没有值得增加直接依赖、改调用接口的速度证据。64B 分块只是碎片化敏感性，不冒充实际 JSON writer 的更新分布；本微测不含序列化、压缩、SQL 或编排成本。

原始数据、输入 SHA256、编译条件和范围限制见 `target/benchmarks/sha256-backend-cost.csv` 与 `sha256-backend-cost-summary.json`。冻结探针为 `target/benchmarks/sha256-cost`，SHA256 为 `557f5aaa7ecfa284c4beae3181e05facddd3ee6efb19dfcd55a76a7362054b9b`；可用 `taskset -c 0 target/benchmarks/sha256-cost target/benchmarks/sha256-backend-cost-replay.csv` 重放，依赖同目录的两份 varied 输入文件。临时 Rust 源码已清理。

### 换算法不是兼容替换

[BLAKE3 官方实现](https://github.com/BLAKE3-team/BLAKE3#readme) 支持 SIMD、树形并行与增量摘要，是后续算法对照候选；官方示例速度图使用 2019 年 Cascade Lake 的 16 KiB 输入，不能直接移作本机已有 SHA-NI 的收益。本轮没有测量 BLAKE3，不能承诺本机提速。

BLAKE3 默认同为 256-bit 输出，不代表摘要等于 SHA256。按 [ADR-0076](../adr/0076-deduplicate-turn-chain-items-and-share-binary-storage-codec.md)，内容键和读回校验明确使用 raw JSON 的 SHA256，canonical 指纹也必须保留既有域、字节序与版本。因此不直接替换算法；若将来授权改变契约，必须一起设计算法标识、内容键与节点引用迁移，以及既有指纹的重建，而不是只替换 `Sha256` 类型。

同算法换实现原则上可保留摘要，但全局后端切换仍需检查平台边界：[runtime-contract](../../backend/crates/stravia-runtime-contract/Cargo.toml) 被宿主与 Wasm 共享，[仓库目标](../../rust-toolchain.toml) 含 `wasm32-wasip2`，而 [AWS 支持矩阵](https://aws.github.io/aws-lc-rs/platform_support.html) 列出的 Wasm 是 Emscripten，不能用本机 native 微测代替 WASI 验证。当前保留 `sha2` 和全部持久化指纹契约。

## 真实数据库副本：凭据保护与历史读取的整体内存

此项验证使用真实数据库的一致性副本和配套 Debug 捕获，与上文新库、合成大正文的矩阵分开。原库只读，经 SQLite backup 获得 1,493,147,648 字节快照，`quick_check` 为 `ok`；2,324 条捕获请求全部成功索引。选择 claude-code、devin、antigravity、openai-codex、workbuddy 各一条中位附近和一条大请求，共十条，入口正文为 107,045–2,423,632 字节。它们覆盖真实 Vendor 的 Anthropic、Devin Connect、Gemini、Responses WebSocket 和 Chat Completions 上游路径。

所有回放运行在仅有 loopback 的 Linux network namespace 内，以 CONNECT/TLS/HTTP/WebSocket 返回已录制的上游响应；保留原 Vendor、域名与区域校验，使用独立测试 CA 验证 TLS，不替换成 generic Provider。数据库写入、历史节点和凭据映射只落副本，正文、凭据和完整日志不进入仓库。并发批次必须恰好消费预期数量的录制推理 exchange，不允许额外推理 POST 请求；源码声明的模型发现、账户发现及额度 POST 查询仍被隔离阻断并单独计数，不伪造成功响应或关闭这些后台功能。其他辅助 GET 同样返回隔离错误，但未纳入此 POST 计数。比较客户端终态、文本、思考（含 encrypted content）和工具调用，归一化新生成的 History Marker 身份；不把 HTTP 200 视为输出等价，也不声称该比较器覆盖 usage、全部 metadata 或 Marker 授权。

候选组合包括跨线程借还 regex 搜索工作区、每个输入只解析一次既有 Marker 范围、逐节点恢复后消费 JSON 解码，以及归因历史窗口借用内容项；独立 Server 入口使用 mimalloc，库与桌面入口不设置全局 allocator。不删检测规则，不关闭保护、历史或 Observation，不改变请求内容、Principal 边界、有效期与请求局部共享预算，也不调整 allocator purge 参数。

初始 mimalloc 配置出现系统态 CPU 回退；内核采样定位到 `_mi_meta_zalloc` / `mi_page_extend_free` 引发透明大页缺页，进入 `compact_zone`、`migrate_pages` 和文件缓存页搬迁，而用户态扫描成本未增加。启用依赖已有的 `no_thp` feature，跳过 allocator 的显式 `MADV_HUGEPAGE` 建议；不修改主机 THP 策略，不以延长或缩短回收参数代替修复。在本机 `madvise` 策略下，这避免为 allocator 映射同步整理连续大页。诊断采样保存在私有 `cpu-kernel-candidate.perf.data`，不用于正式性能比值。

测量采用无 profiler 的 `cargo build --locked --release -p stravia-server --no-default-features` 产物，Linux x64、Rust 1.99.1、20 个可用逻辑处理器。每次从同一快照新建副本；先预热两版各自的 Wasm 产物，再冻结并复制同一份受信任编译缓存，避免只命中基线缓存而让候选重新编译。每条 case 启动独立 Server，worker 复用 HTTP 连接，每条 case 跑八轮，前两轮只从热态 CPU 与吞吐统计中排除。并发一和八分别执行两组配对，顺序为基线→候选、候选→基线。

整体峰值取子进程 `ru_maxrss`，不是请求前后 RSS 差值或局部分配器峰值；全生命周期 CPU 包含启动、全部请求及后台写入排空，另列热态 CPU/请求和客户端完整响应吞吐。回放器等待正常退出并要求退出码为零：通用 E2E 的八秒清理窗口曾强杀基线的大历史 case，不能把那些截断结果用于验收。WebSocket 回放的解掩码改用内置整数 XOR，边界结果与逐字节实现一致，避免 Python 字节循环在并发回放中成为瓶颈；同一驱动同时用于基线和候选。此结果只代表已选真实样本与负载，不外推为生产上游延迟或所有机器的保证。

最终验收采用 `clean-c1`、`clean-c8` 两组记录。基线二进制 SHA256 为 `ae541d6f4e0a629e5379c983190ff333a3f24ab29e2c0f085bcb16a2ad76e694`；优化后二进制为 `51a1d4fd77066d179cd8b69e19fb7571496d57a8b173c205ba4f164827643816`。同一版本在全部配对中的摘要一致。

|并发|配对 / 版本|整体峰值 RSS，MiB|全生命周期 CPU，ms/请求|热态 CPU，ms/请求|热态吞吐，请求/秒|
|---:|---|---:|---:|---:|---:|
|1|0 / 基线|1875.72|668.41|347.50|3.742|
|1|0 / 优化|821.69|565.67|297.83|4.272|
|1|1 / 基线|1699.88|642.79|330.50|3.829|
|1|1 / 优化|824.03|563.97|296.00|4.257|
|8|0 / 基线|3040.66|1011.67|390.92|14.147|
|8|0 / 优化|1413.21|778.69|352.88|15.640|
|8|1 / 基线|3050.98|994.41|383.56|13.259|
|8|1 / 优化|1453.33|790.12|350.23|15.646|

四次配对均满足整体峰值至少减半，且全生命周期 CPU、热态 CPU 与热态吞吐均无回退。共 2,880 次 HTTP 请求全部为 200，比较范围内的输出全部一致，录制推理 exchange 数量全部符合预期，断流、未知推理 POST 与计时批次内被阻断的辅助 POST 均为零；80 个 Server 进程全部正常退出，没有强杀截断。

退出后将每份副本与初始快照按记录 ID 作只读差集核对：每次回放分别新增预期的 80 或 640 个 Generation 节点与 Run。合计 2,880 个新 Run 全为 `completed`，客户端输出已提交，完成与交付时间非空，全部关联 Generation，且 `background_active` 与 `user_interrupted` 均为 false；没有用缺失持久化记录或遗留后台工作换取资源数字。

最终当前状态复核重新构建 Server，二进制摘要仍与实测产物一致；重新执行上述完整 Rust 单测和隔离 PostgreSQL contract，并从原始响应字节重新比较全部回放输出。八份测量副本的凭据保护开关均为 true，均有五条新建映射（四条已发布）和三条续期映射；按 Principal、受保护值与发布状态比较，基线与候选一致。快照未配置自定义规则，复核未添加或删除规则。另以只读连接抽查生产库中的八个不可变节点，载荷与快照一致；全部复核只输出计数，不输出正文或凭据。

热态吞吐只是有限批次的客户端完整响应速率，不等于长期稳态承载上限。并发八的大历史 case 在最后一批响应后仍有后台归因写入排空：基线两次约 343 / 358 秒，优化后约 230 / 244 秒。这些工作全部计入上表的生命周期 CPU 与整体峰值，不计入热态吞吐；不通过丢弃后台任务或缩短超时掩盖积压。此路径仍有继续优化空间，但不在本次达标结论之外另作性能保证。

准备完整 Vendor fixtures 后，Core 为 1,414 passed / 4 ignored，Credential Protection 为 33 passed，Runtime Contract 为 93 passed；另外使用仅 Unix socket 的隔离 PostgreSQL 实际执行并通过 reusable-prefix store contract，不以未配置 PostgreSQL 时的提前返回计数。全工作区 Rust 格式检查、Credential Protection 的 `clippy --all-targets -- -D warnings` 与 Server release 构建通过。Core 广域 Clippy 仍有原有的未使用 `bundled`、参数数量和可合并 `if` 告警；未执行全仓 `task test:all` 或桌面实机验证。

早期强杀、编译缓存不对等、默认 THP 配置及诊断进程遗留的记录均不用于验收。另有一轮 `no-thp-c1` 在一条 case 启动阶段超时，系统态 CPU 异常；随后定向内核采样与最终完整配对未复现。其与当时宿主内存压力、诊断进程遗留的因果关系未证实，不能声称已证明所有启动场景稳定。原始失败记录保留，不与最终成功样本混合。

脱敏汇总为 `target/test-results/memory-replay-summary.json`。真实数据、录制内容、证书、日志、驱动与逐轮结果仅留在权限 `0700` 的 `/tmp/stravia-memory-ejfi0vja/`，未写入仓库；复测需要该私有目录及有效测试证书。相同条件可用新标签运行以下命令，不能覆盖已有结果目录：

```bash
.venv/bin/python /tmp/stravia-memory-ejfi0vja/run-pairs.py \
  --label repeat-c1 --concurrency 1 --rounds 8 --pairs 2
.venv/bin/python /tmp/stravia-memory-ejfi0vja/run-pairs.py \
  --label repeat-c8 --concurrency 8 --rounds 8 --pairs 2
```
