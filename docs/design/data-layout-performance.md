---
status: accepted
---

# 数据结构与存储性能重构

更新：2026-10-30。当前决策覆盖 Core 历史存储、共享内存表示、Observation 批写、缓存计量与 SQLite checkpoint 窗口。第 6–7 节保留先前隔离探针和库选型证据，不代表正式 HTTP 性能；当前行为以源码、生成 schema 和 [ADR-0076](../adr/0076-deduplicate-turn-chain-items-and-share-binary-storage-codec.md) 为准。

## 1. 结论与边界

- 重构测量前已快进合并 dev，比较基线固定为 `37b1f0ff`，不再以合并前 ropefish 的未提交状态设计并行 schema。存储重构提交为 `7a3cb9ab`；随后集成的 dev 响应边界修复不自动继承这些冻结二进制的性能成绩。保留 0030/0031 的成本、计费、完整 ModelCost、字段级 pinning 和冻结金额契约。
- 历史采用 Principal 字典、整数内部身份、immutable descriptor/正文、窄 retention 与持久 GC 候选；公开 TurnNodeId 和协议 payload_version 不变。
- 所有非 null 语义内容单元统一寻址，取消 256B 分流；空字符串、对象和数组也外置。opaque Item 不拆成标量 EAV，null/缺省由结构表达。codec 的 128B 压缩门槛是独立选择，继续保留。
- 正式内容身份是 `SHA256(b"stravia.turn-content.v1\0" + raw_compact_json)` 的完整 32 字节，使用已有 sha2。**不引入 BLAKE3、sonic-rs 或 foyer**；外部 hash、认证、canonical 指纹、Artifact ID、工具 ID 和 Wasm ABI 不变。
- `FrozenItem(Arc<AiItem>)` 共享已完成 Item；真正改写才 COW。借用写入、接管已有 owned canonical 数据、消费已准备投影，避免 client/effective/catalog 反复复制大正文。
- Observation 移除无读者的 turn/interaction Token 副本和相应 SUM 写；attempt 是用量权威。批内已有 usage 不再触发多余 durable 查询，finish 合并必要投影更新；事件顺序、迟到修订、unknown/zero 和费用冻结不变。
- 继续使用 TinyUFO / PostgreSQL 的 Redis 部署契约、五域共享逻辑预算和独立请求局部预算。不新增持久内容副本、预热路径或全局 interner；容量须由产品负载而非命中率单独决定。

原[内存调查](../research/compact-memory-structures.md)、[EcoString 实验](../research/ecostring-arc-string-benchmark.md)及[sonic-rs 评估](../research/sonic-rs-storage-evaluation.md)保留。Text 本已共享，不能把它重复算作本轮收益。

## 2. 规范化持久结构

|结构|职责与约束|
|---|---|
|`turn_scopes`|Principal 唯一映射到不复用的整数 scope_id。|
|`turn_nodes`|内部 node_pk、唯一公开 id、scope/kind、parent_pk、payload_version、创建时间与前缀派生列；父边复合 FK 约束相同 scope/kind。|
|`turn_node_payloads`|按 node_pk 保存 immutable descriptor，不随续期重写大行。|
|`turn_node_retention`|expires_at、deleting；expiry 索引驱动到期候选。|
|`turn_contents`|scope 内的 32 字节 digest 与 codec bytes；唯一 `(scope_id,digest)`，数值内容 ID 不复用。|
|`turn_node_contents`|`(node_pk,ordinal)` 主键、`(node_pk,content_id)` 唯一；复合 FK 约束归属，content_id 反向索引用于判活；SQLite WITHOUT ROWID。|
|`turn_node_gc` / `turn_content_gc`|持久候选，不是删除授权；领取时仍在正确锁协议内重检。|

Descriptor 只有 `data` 与 `slots`；slot 为 JSON Pointer / ordinal，关联表是 ordinal→content_id 的唯一权威。重复 occurrence 共用 ordinal，父 profile 先于 nested slot 恢复；拒绝缺失、多余、不连续、跨 scope 或损坏引用。未知扩展和精确 JSON 数字继续保留，不引入 JCS 或 f64 归一化。

正式读取仍逐节点恢复 JSON 并解码领域对象；没有实现按 kind 完全 typed、完全不经中间 DOM 的 descriptor。这个选择使 payload 的领域演进不要求重写内部表结构，且避免同时改写所有历史协议的编解码契约。

整数复合键中的 scope/kind 冗余用于数据库隔离约束，不作为“重复信息”删除。内部 ID 不是 PostgreSQL 提交水位；分配顺序不等于提交顺序。前缀仍是可重建派生信息，指纹只能否决候选，不能替代完整 canonical 核验。

## 3. 读写、GC 与共享内存

### 历史提交与读取

CPU 准备尽量在写协调外完成：复用序列化 scratch，计算原始 JSON 内容身份，查已存在内容提示，仅压缩缺失项。事务内重检父链、scope/kind、期限与内容存在性，节点、payload、retention、ordinal 关联和前缀同事务提交；提示不是权威。PostgreSQL 按 nodes→contents 锁序协调写入与 GC，已有内容按稳定 digest 顺序取锁，不靠无意义 UPDATE 或自动重试解决并发。

祖先 CTE 携带窄身份和生命周期，最终节点查询同时返回 payload、scope 与整数 ordinal 映射，不为映射再发一次 SQL，也不通过 join 重复传输正文。查询负责 Principal/kind 授权，恢复器仍核验 scope、连续 ordinal 与内容摘要；即使关闭 FK 后引用行的 scope 被破坏，也必须拒绝。按 parent_pk 恢复顺序并检查环、断父和缺失行，不依赖 SQL 或 JSON 聚合输出顺序。缺失正文按最多 256 个数值 ID 分批读取。请求局部上下文可以复用已验证祖先和内容，但失效的 cutoff proof 必须权威重读，不能把过期证明误当不存在。

祖先独立可引用，因此续期仍为 **O(depth)**，只更新确实增长的窄 retention 行。没有改成 root/head lease、闭包表或可达计数，也不保存周期完整历史快照。

### 候选 GC

到期索引补入叶子，删除 child 后登记 parent；live descendant 和 native compaction source 保护祖先。每事务有界处理节点、引用和内容，释放 SQLite 写协调后再进入下一批。首批删除引用与 deleting 标记原子提交，数据库读/发现/续期拒绝 deleting，崩溃后从持久候选继续；不能靠时钟回拨或早开始、晚取锁的 writer 复活半删除节点。

引用删除与新孤儿候选登记在同一事务，只为失去最后引用的内容入队，最终删除仍重检反向引用。祖先按需求值并固定小批节点集合；内容先物化有界候选，再按主键取正文，避免为排序扫描全部 live content。节点和内容共享一个小事务，但各自数量及字节上限不扩大，不能让级联一次清空超大节点，也不以 refcount 判断祖先活性。批间真实 native login 可完成的测试证明公平性，**不等于总 GC CPU 已测得改善**。

### 共享与预算

client/effective/parent/replacement/pending 窗口、FrozenResponse 与引用目录持有共享 Item 句柄。序列化仍是原有 Item 数组，真正可变 AiRequest 边界才生成 owned 数据；没有改公开 IR 或协议。canonical 比较按语义单元展平，保留 grouped/split Item 等价，不把分组形式当历史身份。

缓存估重包含共享窗口的唯一 Item allocation、metadata 的完整 JSON 结构与任意精度数字；深层值不能因固定深度截断漏算。effective_request 的 instructions、工具/schema、response_format、parameters/native_tools 同属预算，不能用“小 items”绕过准入。请求局部工作区还计入解码 delta 保留的响应 vendor metadata、错误及 embedding 数据，不能把很短的 JSON 编码长度当作容器占用。内容、物化缓存与 precommit 共用借用式 JSON 估重：容器保留空间、保守 map 条目开销和数字词法均计入；内容条目另计数值键、Arc 头与槽位。发现阶段不再为准入把整节点序列化一遍，也不构造第二棵 DOM。逻辑预算不是 allocator/RSS 硬上限，跨条目共享、容器和分配器开销仍须用整体进程实测观察。

metadata 的扩展对象同样复用 JSON map 的保守条目估重，而不只计算 key 和 Value 本体；大量 `null` 字段也占用容器空间。真实 TinyUFO 回归使用 3,072 个小扩展字段，在 256 KiB 预算下拒绝缓存准入、第二次恢复仍走权威存储，并保持完整 metadata。此修订及共享 Item trait、Observation usage 吸收谓词的维护性收口，已通过 118 项 Generation Chain 与两后端 durable usage 用例；这些结果不冒用此前冻结二进制的产品验证。

## 4. Observation 与索引

`interaction_observations` 和 `model_turn_observations` 各删除 input/output/cache-read/cache-write/reasoning 五个无生产读者的 Token 副本，保留 estimated_input_tokens。查询从 attempt 事实聚合；删除逐 confirmation 的 turn/interaction SUM 更新。

批内使用已有 usage，只有真正缺失的 finish 才查询 durable attempt。finish 的状态、金额与已知 usage 合并写入；后续同批 confirmation 在相应 UPDATE 成功后才可吸收。迟到修订、sequence、事件 kind/payload、未知字段合并和 at-most-once 金额累计遵循原契约，不扩大成可靠结算账本。

移除不服务当前查询的 `model_turns_analytics_idx`、`interaction_observations_generation_idx`，收窄 inference-run generation 索引。拒绝请求的 Client Errors 查询使用 `COALESCE(started_at,occurred_at)`；以相同过滤谓词的 partial expression index 替换无效 started_at 索引，保留普通 occurred_at/expiry 用途。SQLite/PG 合成 SQL 计划和结果已核对，但不把单条 SQL 探针倍率称为 HTTP 提速。

完整价格、字段级 pinning、来源模型快照、USD/custom 冻结金额、known/unknown/not-billed、spend 清零与长期 stats 均不变。没有替换 stats 触发器、重放历史收费或合并不同所有权域的 TTL。

## 5. 升级、已取得证据与限制

### 转换与回退

冻结 0001–0031 checksum。旧 0007 hook 仍把 format 0/1 转为历史 format 2；0032 建立新结构后，数据 hook 按持久阶段和游标转换，完整验证后才允许 0033 切换 native compaction FK 并删除旧表。正常运行只读新结构；旧 SHA/256B converter 与显式冻结基线审计适配器不是产品双读 fallback。

stage 2 先读薄 metadata，使用 encoded 长度与 decoded-size hint 的较大值，加唯一 profile 依赖，选择连续前缀；8 MiB 是逻辑源预算，首个不可拆节点可超预算。hint 不替代 codec/hash 校验。共享 profile 每批只处理一次，局部内容序号不能混成批次偏移；旧 ref-set 必须与 descriptor 完全一致。数据和 cursor 同事务，未选中的尾部留给下一批；最终孤儿/scope 清理由单调游标推进。

升级前停机、排空执行并备份完整数据根和外部数据库，保留迁移并存阶段的额外磁盘空间。恢复可继续已提交阶段；回退必须恢复配套备份，不能让旧二进制读取新 schema。普通已完成库启动不全库扫描 FK 或重跑转换，SQLite FK enforcement 仍开启。两后端参考 schema 必须经 `stravia-tools dump-schema` 从全部迁移生成，不能手编最终 DDL。

### 真实 SQLite 一致性副本

仅通过在线 backup 取得只读一致性副本；没有修改生产数据库，也没有启动含生产凭据的 Gateway。源迁移到 0029，先离线升到 0031，再转换到 0033。归一化测量使用直接调用真实 Core migrator 的 release 程序、单 SQLite 连接和 System allocator，不是 Server mimalloc/HTTP 性能。

|schema31→33 指标|实测|
|---|---:|
|归一化 wall time，含最终 checkpoint|138.58 s|
|user / system CPU|70.16 / 68.53 s|
|进程峰值 RSS|49,913,856 B|
|25ms 采样 WAL 峰值|39,325,432 B|
|采样数据库文件峰值|3,467,395,072 B|
|SQLite total_changes|5,898,859|
|源 / 新库有效页字节|2,723,639,296 / 1,716,076,544 B|

有效页减少约 **37%**，但物理文件没有 VACUUM，不能称数据库文件已缩小 37%。迁移有新旧结构并存期；采样峰值不是绝对峰值，filesystem block counters 不是 SSD NAND 写放大。0029→31 另耗时 32.50 s，不混入上述归一化耗时。

全量审计验证 43,771 个节点的公开 ID、Principal/kind、父边、版本、创建/到期时间、全部前缀 metadata 和完整恢复 payload；48 张非历史表逐行一致，仅排除 migration bookkeeping 与授权移除的十个 Token 投影。integrity_check、foreign_key_check、零 orphan/空 scope/GC 队列检查通过。审计保留大整数、超大指数、负零和数字词法，不输出正文或身份。审计器的内容加载/命中数不是产品 SQL 或缓存计数。

只读结构查询得到 4,905 个根；下表按全部节点统计父边深度，根深度为零。8/16 轮短链探针不足以代表此分布，不能仅据短链结果判断规范化续期在长链上的成本。

|父边深度 p50|p95|p99|最大值|
|---:|---:|---:|---:|
|27|116|160|217|

### 产品测量口径

冻结 merged-dev release Server 与候选 Server，用相同合成数据、固定参数和交错顺序比较；只允许合成数据启动 HTTP 服务。覆盖 128B、4KiB、64KiB 输入，显式父节点、完整历史自动发现、原生压缩、共享祖先、前缀 decoy 与被拒 session；独立测容量 0/4/8/16/32 MiB。此历史探针是 unary：first_content 表示完整响应正文可用，不是流式首 Token；connect 与不含 connect 的 HTTP EOF 单列。SQL、历史阶段、进程 CPU 和采样 RSS 分别报告，不相加嵌套 span，也不把含诊断读取的整场请求数/耗时当作服务端极限吞吐。

基线 Server SHA-256 前缀为 `4f739d22000e`。从 `git archive 37b1f0ff` 干净源码重新构建的 base Wasm 组件（`a2d1c6e1dc8d`）与其内嵌字节完全一致；当前候选使用对应工作树构建的组件（`d4414d007c63`）。组件字节不同，因此不能仅凭相同版本标签假定插件相同；两端均核实构建来源。

完成内存计量修正后的候选（Server SHA-256 前缀 `9d6545288008`）完成 18 次交错配对与 8 次容量运行，26 次均通过行为审计；逐 pair、case、round 比较有效上游输入 hash 无差异。下表为每场景三个配对运行的中位数，箭头为 merged-dev 基线 → 当前候选。请求 SQL 仍增加，CPU 和 RSS 不能宣称全面改善。

|输入|请求 SQL 数|SQL 总耗时 ms|进程 CPU 采样窗口 s|峰值 RSS MiB|
|---|---:|---:|---:|---:|
|128 B|2716 → 2925|125.998 → 145.970|12.96 → 13.07|575.47 → 582.62|
|4 KiB|2716 → 2925|132.628 → 139.821|12.91 → 12.91|572.84 → 587.53|
|64 KiB|4206 → 4535|236.025 → 250.464|33.43 → 32.85|785.26 → 777.59|

按因果父链归属、跨快照去重后的后台 Observation writer SQL 见下表；不能把后台减写与前台 SQL 增加混为一项收益，也不能相加嵌套 span。首轮 `721c565e3f36` 的请求 SQL 为短场景 3113、64 KiB 场景 4852；合并 scope 探测与 ordinal 读取等往返后降到上表数值，但仍高于基线。HTTP 有改善也有回退，例如 4 KiB 的压缩后续接 p50 为 17.12 → 24.20 ms，64 KiB 的无投影候选拒绝为 92.18 → 79.33 ms；这些有限样本不支持普遍加速结论。

|输入|基线后台 SQL 中位数|当前候选后台 SQL 中位数|
|---|---:|---:|
|128 B|5555|5235|
|4 KiB|5569|5264|
|64 KiB|9026|8539|

修正估重后的容量对照均正确。下表为 64 KiB 场景各容量两次正反序运行的中位数；16 MiB 使用同一候选的三个配对运行。4/8/16 MiB 的请求 SQL 相同，32 MiB 仅再减少少量 SQL，没有稳定延迟或 RSS 优势。保留现有 16 MiB 作为五域共享的保守默认，不为掩盖写路径回退扩容，也不把单一合成工作集的最小有效容量当成生产最优值。

|容量 MiB|请求 SQL|CPU 采样窗口 s|峰值 RSS MiB|
|---:|---:|---:|---:|
|0|4587|33.39|759.54|
|4|4535|33.30|800.81|
|8|4535|32.95|806.69|
|16|4535|32.85|777.59|
|32|4517|33.04|828.87|

未锁 CPU 频率、未清 OS page cache，CPU 窗口包含 setup/inspection，RSS 包含 Wasm 与 tracing，结果不代表远程 PG 或生产负载。`active_us` 不是进程 CPU，事务等待也不能仅凭 span 时长归因于 fsync。完整测试、迁移审计、空间收益和产品性能分别验收。

### 128 轮完整历史对照

修复时间线采集后，以 `c6a4921ccdf3` 候选与同一基线进行三个交错配对。每轮使用 4 KiB 文本、128 轮历史、128 轮共享祖先历史、8 个候选和 16 MiB 共享缓存；六次运行各完成 674 个请求，共 4,044 个请求全部有效，整组耗时 585.68 秒。逐 `(case,round)` 比较三个配对的全部有效上游输入 SHA-256，无差异；每轮唯一完整因果快照均通过身份、父链、丢失计数和同步子树完整性校验。

下表为三个运行指标的中位数。报告 schema 3 的 CPU/RSS 窗口排除首尾完整时间线导出和 Python 离线归因，包含 provision、推理及 Observation 检查；不能与上方旧 schema 2 的全窗口资源数据直接比较。

|指标|基线|候选 `c6a4921ccdf3`|
|---|---:|---:|
|服务端 CPU s|55.36|52.70|
|采样峰值 RSS MiB|793.96|805.41|
|前台请求 SQL 次数|24,804|27,045|
|前台请求 SQL ms|2,901.226|2,827.669|
|Observation writer SQL 次数|58,350|54,997|
|管理检查 SQL 次数|15,152|14,623|

CPU 中位数下降约 4.8%，但前台 SQL 增加约 9.0%、RSS 略增，不能据此验收整体性能目标。部分 HTTP 场景仍回退；下表是各运行 HTTP p50 的中位数，不是合并全部样本后计算的分位数。

|场景|基线 HTTP ms|候选 HTTP ms|
|---|---:|---:|
|automatic-full-history|52.270|50.480|
|explicit-parent|53.111|53.570|
|native-compact-operation|59.551|67.147|
|native-compacted-continuation|40.563|61.506|
|shared-ancestor-unprojected-candidates-match|52.820|63.165|

后三类每次运行各只有一个请求，波动较大，仍须保留回退而不能当作噪声删除。

### 关闭 Debug 的正常运行对照

同一基线与 `c6a4921ccdf3` 使用报告 schema 4、`--performance-debug disabled` 做两组各三个交错配对：128 轮、每轮 4 KiB 文本，共 4,044 请求；16 轮、每轮 64 KiB 用户文本并附带工具 JSON，共 684 请求。填充文本由固定 SHA-256 序列生成，工具参数使用其分块和精确大整数，并非只重复单个字符；工具形状的完整请求体不止 64 KiB。12 次运行全部有效，每个 `(case,round)` 的完整上游输入 digest 序列在配对两端一致，未测 SQL/stage 字段全部为 `null`。

下表为每端三次运行的中位数，箭头为基线 → 候选；不是全部请求的混合分位数。进程 I/O 包括 provision、推理与 Observation 检查期间的所有服务端文件，不等于数据库独占写入或设备写放大，取消写入计数不从 `write_bytes` 擅自扣减。

|负载|服务端 CPU s|采样峰值 RSS MiB|进程 `write_bytes` MiB|进程 `cancelled_write_bytes` MiB|
|---|---:|---:|---:|---:|
|128 轮 4 KiB 文本|52.74 → 48.65|905.97 → 875.92|1484.04 → 1158.88|132.16 → 21.13|
|16 轮 64 KiB 工具历史|8.25 → 7.98|741.95 → 727.61|241.41 → 235.32|1.48 → 1.32|

文本长链 CPU 下降约 7.8%，进程写入计数下降约 21.9%；工具历史的对应差异约为 3.3% 和 2.5%，不能推广成所有负载都有同等收益。HTTP 仍有增有减：

|负载 / 场景|基线 HTTP p50 ms|候选 HTTP p50 ms|
|---|---:|---:|
|文本 / automatic-full-history|54.465|54.544|
|文本 / explicit-parent|59.564|59.771|
|文本 / native-compaction|53.618|55.648|
|文本 / native-compacted-continuation|49.523|42.190|
|文本 / unprojected-candidates-match|55.773|62.829|
|工具 / automatic-full-history|87.709|82.791|
|工具 / explicit-parent|84.720|86.351|
|工具 / native-compaction|83.311|89.002|
|工具 / native-compacted-continuation|32.423|25.777|
|工具 / unprojected-candidates-match|189.927|193.424|

continuation 与 match 每次运行只有一个请求，其余表内场景分别为 128 或 16 个请求。未绑核、锁频或清 OS page cache；关闭模式不提供因果归因，不能用不同批次的 Debug 开/关结果直接推导诊断开销。该 Server 不含后续集合式 GC 或实验中的 SQLite 参数调整，不能作为它们的最终产品验证。

### 最终版本的 HTTP 历史验收

最终 Server `9e52514d9e03` 与冻结 merged-dev 基线完成 36 次交错运行、共 **10,344 请求**：Debug 关闭的 128 轮文本长链 4,044 请求、64 KiB 工具 JSON 684 请求；Debug 开启的同形长链 4,044 请求；Debug 关闭的 128B/4KiB/64KiB 短链共 1,572 请求。每种条件三个配对，全部有效，每个 `(repetition,case,round)` 的完整上游输入 digest 序列均相等。开启模式的整轮因果捕获与关闭模式的独立控制分别通过，关闭模式 SQL/stage 仍为 `null`，不以零替代未知。

下表为三个运行指标的中位数，基线 → 最终版本；CPU/RSS 排除首尾时间线导出，包含 provision、推理与 Observation 检查。I/O 是整个 Server 的 Linux 进程计数，不是数据库独占或设备写放大；未减去取消写入，也未绑核、锁频或清 OS page cache。

|输入与模式|进程 CPU s|采样峰值 RSS MiB|进程 write_bytes MiB|cancelled_write_bytes MiB|
|---|---:|---:|---:|---:|
|4 KiB 文本，128 轮，Debug off|51.10 → 47.78|876.98 → 848.92|1,474.16 → 1,147.47|131.27 → 21.82|
|64 KiB 工具 JSON，16 轮，Debug off|7.83 → 7.56|737.41 → 712.76|244.64 → 232.98|1.31 → 1.54|
|4 KiB 文本，128 轮，Debug on|53.60 → 51.22|796.54 → 774.76|1,820.54 → 1,485.23|131.83 → 21.80|
|128 B 文本，8 轮，Debug off|2.21 → 2.32|554.88 → 573.02|136.96 → 131.35|0.62 → 0.60|
|4 KiB 文本，8 轮，Debug off|2.21 → 2.16|449.87 → 581.25|140.49 → 135.57|0.69 → 0.68|
|64 KiB 文本，16 轮，Debug off|4.85 → 5.17|566.51 → 574.21|222.87 → 214.96|1.35 → 1.46|

正常文本长链 CPU 降约 6.5%、进程写入量降约 22.2%，但短链不是全面收益：128B/64KiB 的 CPU 增加，4KiB 短链的整进程 RSS 明显更高。RSS 包含 Wasmtime 等全部服务端内存，当前未定位该差异来源；不能把共享 Item 的结构收益宣传成所有负载的 RSS 降低，也不能把逻辑缓存预算当作进程上限。开启 Debug 的长链前台 SQL 数中位数仍从 24,800 增到 27,045，SQL 累计时间为 2,755.019 → 2,702.619 ms；规范化并未减少每条前台路径的数据库往返。

HTTP 表同样使用每次运行 p50 的中位数，而不是混合请求分位数。automatic、explicit-parent、native-compaction 每轮分别有 128 或 16 个样本，其余下列操作每轮只有一个样本。

|模式与场景|HTTP ms，基线 → 最终版本|
|---|---:|
|文本长链 off，automatic-full-history|51.790 → 48.927|
|文本长链 off，explicit-parent|54.970 → 54.744|
|文本长链 off，native-compaction|51.991 → 51.621|
|文本长链 off，compacted-continuation|40.932 → 41.365|
|文本长链 off，rejected-session-candidates|37.747 → 46.361|
|工具 JSON off，automatic-full-history|82.962 → 81.452|
|工具 JSON off，explicit-parent|98.620 → 93.915|
|工具 JSON off，native-compact-operation|108.728 → 123.892|
|工具 JSON off，shared-ancestor-source|88.520 → 126.091|
|工具 JSON off，unprojected-candidates-match|154.725 → 175.600|
|文本长链 on，unprojected-candidates-match|47.271 → 86.554|

以上保留实际回退；稀疏场景不能证明稳定分位数，也不能被排除为噪声。较大 checkpoint 窗口的回写合并不保证每次提交或请求更快。完整形状校验、正确性、CPU、内存、I/O 与 HTTP 延迟分别验收，不能互相替代。

### Client Errors 的真实 HTTP 对照

候选 `9d6545288008` 与同一 merged-dev 基线分别启动隔离 SQLite Server，经真实初始化和管理员会话访问 `/api/v1/observations/client-errors`。使用 1,000 / 25,000 条合成拒绝记录、1% / 50% 时间窗口，混入空 `started_at`、499 与 cancelled；逐次验证完整总数及按 `COALESCE(started_at,occurred_at)`、ID 排序的首页。每种条件三个交错配对，每次 5 次预热后测 100 次新连接请求，共 2,400 次计时请求均有效。下表为各配对运行指标的中位数，箭头仍为基线 → 候选。

|行数|时间窗口覆盖率|HTTP p50 ms|HTTP p95 ms|100 请求的服务端 CPU s|
|---:|---:|---:|---:|---:|
|1,000|1%|8.182 → 7.834|14.687 → 11.588|0.09 → 0.08|
|1,000|50%|8.465 → 8.209|15.855 → 15.574|0.13 → 0.11|
|25,000|1%|15.343 → 7.929|23.957 → 12.353|0.79 → 0.09|
|25,000|50%|20.304 → 14.551|33.057 → 27.142|1.38 → 0.74|

CPU 窗口排除建库、插入和预热，包含服务端全部线程，受操作系统 CPU tick 分辨率限制；HTTP 计时包含新连接和完整响应。该结果证明拒绝请求索引在所测 HTTP 负载下减少工作，尤其是较大数据量；查询中的失败推理分支为空，不能外推为整个 Client Errors 混合负载、推理生成路径或 PostgreSQL 的同等收益。未锁 CPU/频率或清 OS cache，较小差异不作为稳定收益结论。

### 真实副本的完整 GC 对照

直接调用真实 `SqlTurnChainStore::sweep_expired`，不启动 Gateway 或网络。两端分别从已审计的 0031 / 0033 副本在线备份为独立实验库，每库 43,771 节点；旧版 202,121 条内容、3,959,292 条引用，新版 244,232 条内容、5,482,608 条引用。统一小内容寻址增加了引用工作量，不能只比较 SQL 条数。基线探针 SHA-256 前缀 `d72523038f5e`，使用有界集合判定新孤儿、等值反连接及合并小事务的候选为 `8f178633907c`。候选显式设为 8,192 页 checkpoint，与下文实际连接池冒烟确认的 4 KiB 页默认策略一致；探针自行建池，不是 Server 二进制测试，报告保留显式配置标记。

全部存活场景将期限设到未来，预热一次后计时三次空闲 sweep；全部过期场景仅在实验副本移除 native compaction 保护并设为过期，计时一次完整回收。每场景两个正反序配对，共八次运行均有效。每次全量回收删除 43,771 节点，节点、正文、引用均清空，候选的 scope/GC 队列为空，外键检查通过；存活场景节点数保持不变。

下表为两个交错配对的运行中位数，箭头为基线 → 候选。双方探针均限定 CPU 2、4；CPU 使用 Linux 进程 CPU clock 包含所有探针线程，排除建副本与预热，**包含 sweep 和最终显式 checkpoint**，不将回写移出验收窗口。

|场景|CPU s|命令耗时 s|采样峰值 RSS MiB|采样峰值 WAL MiB|
|---|---:|---:|---:|---:|
|三次空闲 sweep|1.949648 → 0.000338|1.155509 → 0.000336|11.97 → 10.72|0 → 0|
|全部过期回收|57.7417 → 60.8047|113.9000 → 86.5238|13.41 → 16.54|524.64 → 37.24|

空闲全表读取已消除；全过期回收的 wall 降约 24%，但 CPU 仍高约 5.3%、RSS 也更高。该场景整进程 `write_bytes` 中位数为 21.736 → 22.032 GiB，约增加 1.4%；`cancelled_write_bytes` 为 0 → 3.532 GiB，不能擅自扣除，不能把更低 WAL 峰值宣称为总写入量减少或 GC 全面加速。候选的实际 commit hook 计数为每次全量回收 2,697 次，包含空事务，不能当作 fsync 次数。单 SQLite 连接、System allocator、WAL、实际 `synchronous=2`；未锁频或清 OS page cache。RSS/WAL 每 25 ms 采样只是峰值下界，候选空闲运行仅取得一个样本，不能据此宣称捕获其瞬时内存峰值。本对照不是 HTTP、PostgreSQL 或日常混合过期比例的测量。

此前候选 `dd5ce6755502` 保留 1,000 页触发、未绑核的完整对照也全部有效，全过期 CPU 为 63.8298 → 90.8341 秒，sweep wall 为 98.8122 → 119.3882 秒，且没有计入最终显式 checkpoint。原始回退报告保留，不能与新口径混算；下节另以同一实现、同一规范化数据隔离 checkpoint 参数的作用。

集合判定在同一写锁和事务中物化最多 2,048 条待删引用，只为窗口之外无引用的内容入队，再删除相同有序窗口；最终正文删除仍重检反向引用。SQLite 的复合 `NOT IN` 在未命中时会扫描窗口处理 NULL 语义，首个中间版本虽正确回收，却把全过期 CPU 提高到约 223 秒，不能采用。当前等值 `NOT EXISTS` 在窗口上使用自动覆盖索引；双后端差分原型的 8 个场景、24 次状态转换与旧路径一致，引用 ID 不再逐行回传 Rust。随后真实 store 的 53 项历史测试、PostgreSQL retention 专项及 Core/Server Clippy 均通过，上表是真实副本复测，而不是从执行计划推测收益。

另以同一二进制、相同规范化数据、双方均按主键重建引用表的方式，仅在实验副本移除 `UNIQUE(node_pk,content_id)`。四次全过期运行均正确；保留/移除该约束的 CPU 中位数分别为 83.6200 / 79.3748 秒，只有约 5% 差异，无法解释主要回退。该结果是布局反事实，不是已交付 schema 的成绩；产品保留该约束。早先验证器错误按 baseline 标签选择旧表名的失败报告保留，修复后使用新实验标签重新执行，未覆盖或重标失败证据。

### SQLite checkpoint 窗口决策

只对自行启动的隔离探针做 GDB 符号栈采样，未附加生产进程、读取参数或放宽主机权限。有效运行取得 2,518 次全线程墙钟采样、149 个唯一栈；SQLite worker 的 1,077 个样本处于自动 checkpoint 的 fsync，345 个处于提交同步。这是定位等待位置的证据，不是 CPU profile，也不能把受调试器扰动的耗时作为性能成绩；`perf record` 仍因权限限制未取得 CPU 调用栈。

参数对照使用同一 `8f178633907c` 探针、原始规范化副本、完整约束及原事务限额。发现机器包含 P/E 核后，将探针限定在两个不同 P 核的 CPU 2、4；不锁频或清 OS page cache。每个条件两个交错运行，全部回收 43,771 节点、244,232 内容、5,482,608 引用，scope/队列为空且外键检查通过；实际 SQLite 3.51.3、4 KiB 页、`synchronous=2`、`cache_size=-2000`，每次 2,697 个 commit hook。下表包含 sweep 和最终显式 checkpoint，避免将回写移出计时窗口；旧报告通用 limits 文案中的未绑核描述未及时更新，实际 affinity 以每次读取核验的 `[2,4]` 为准，原报告未覆盖。

|两次运行中位数|1,000 页触发|8,192 页触发|
|---|---:|---:|
|进程 CPU s|92.5504|66.8237|
|其中 user / system CPU s，tick 口径|20.33 / 72.22|19.855 / 46.96|
|sweep + checkpoint wall s|130.2663|90.4584|
|整进程 write_bytes GiB|32.728|22.217|
|整进程 cancelled_write_bytes GiB|3.532|3.532|
|sweep 采样峰值 RSS MiB|16.570|16.682|
|sweep 采样峰值 WAL MiB|9.890|37.236|

进程 I/O 不是数据库独占 I/O 或设备写放大，取消写入不从 `write_bytes` 中擅自扣除。这证明同一有界 GC 的回写和系统 CPU 成本可以下降，不是与旧 schema 31 的完整配对，也不证明所有 HTTP 延迟改善。未绑核的 16,384 页实验没有形成足以支付更大 WAL 的稳定额外收益；8 MiB SQLite 页缓存也未稳定改善 CPU，且增加 RSS，均不采用。

另用同一 CPU affinity 对比“512 个队列节点内按创建时间选 128 个节点”的局部性实验，双方采用原 1,000 页触发：CPU 中位数 93.2261 → 93.3583 秒，wall 129.6354 → 132.6484 秒，没有收益，已撤回；没有重排迁移身份或按真实副本的 Principal 数特化。

采用共享 `db::init_pool` 每连接按实际页大小换算 **32 MiB 自动 checkpoint 触发窗口**，不扩大 GC 批次、不改提交同步或 SQLite 页缓存。真实连接池冒烟分别创建 4/16/64 KiB 页的合成数据库，同时检查每库五条连接，随后各提交 200 次、共 50 MiB 的数据；三库均保留全部行，WAL 实际峰值分别为 33,792,272 / 33,652,840 / 33,828,992 B，`synchronous=2`、页缓存与外键设置不变。触发窗口不是 WAL 硬限，也不是 16 MiB 派生缓存的扩容。前述离线升级资源数字仍属于原测量配置，不冒充此策略下的升级资源成绩。

### 正确性与产品面覆盖

旧 schema 2 的 128 轮长链实验在 6,166 秒后人工中断，留下 585/674 次请求的无效报告；中断栈停在完整性能时间线导出。逐请求重复导出和遍历累积时间线使诊断工作随请求数放大，不能把该整轮耗时或资源窗口当作推理性能。报告 schema 3 已改为整轮前后各一次导出及按真实 Run ID 的共享因果索引；44 项归因回归通过，基线与 `93089f3fe704` 候选的 8 轮短链冒烟共 148 请求全部有效、总耗时 19.27 秒。这只证明采集路径可用，不替代完整长链配对，也不与旧 schema 2 的 CPU/RSS 直接比较。

估重与维护性修订后的中间版本完整 Core 为 1,506 passed / 5 ignored；另在隔离 PostgreSQL 与 Valkey 下执行其中四项外部契约，全部通过。该轮冻结 Server SHA-256 为 `c6a4921ccdf3984c147768419d659752aa52d6346d5978fdf8aa714cc96d15a2`，不包含随后集合式 GC 的改动。补充 tool-json 与 Debug-disabled 控制后，时间线归因及协议审计共 187 项 Python 检查通过。真实副本无损升级审计与这些测试分开记录，不将测试数当成性能证据；新构建也不自动继承旧冻结二进制的 HTTP 或原生产品验证。

含集合式 GC 和 32 MiB checkpoint 策略的最终源码重新通过完整 Core：1,506 passed / 5 ignored，544.05 秒；其中四项隔离 PostgreSQL/Valkey 外部契约另行执行全部通过，S3 仍因无隔离 endpoint/bucket 未运行。runtime-contract 的 93 项测试、Core/runtime-contract/Server/devtools 指定全 target Clippy、`cargo fmt --all -- --check`、release Server/devtools 及原生 Desktop 构建通过。冻结 Server 为 `9e52514d9e0352ab71b9862b56b188acf83a29e25d2ac4bea853d0f467d13b25`，Tools 为 `ebbd8c89d784`、storage harness 为 `cdc83e193946`、Desktop 为 `505fbd7ba82a`。

上述最新二进制的 SQLite storage 为 73 passed / 10 skipped，PostgreSQL storage 为 74 passed、所选 Admin 套件为 58 passed、Proxy 套件为 140 passed，均无失败；SQLite 套件跳过的十项原生用例在隔离 Xvfb/D-Bus 中用最新 Desktop 另行执行，10 passed / 0 skipped。原生会话仍出现非阻断的 systemd1 D-Bus activation 日志，不把它隐藏为产品修复。临时 Rust 探针源码已保存到私有证据目录并从仓库移除；实际性能成绩仍按各自的二进制与采集口径记录。

后续与 dev 集成时，共享历史恢复同时保留完整响应归属，并修复 Marker 展开丢失该归属的问题；真实持久化回放验证同一响应的 reasoning、正文和调用在 Chat 中归为一条 assistant，工具结果与下一响应仍按序独立。集成工作树构建的 Server `499cd90805f4` 另行通过 SQLite storage 73 passed / 10 skipped、Proxy 140 passed；该轮 Tools 与 storage harness 继续使用上段无相关改动的冻结版本。合并后的 PostgreSQL、原生 Desktop 和大负载性能配对未重跑，不借用合并前结果声称已覆盖这些版本与场景。

报告 schema 4 的 tool-json 负载已用基线及 `c6a4921ccdf39` 做真实 HTTP 冒烟：Debug 开启、关闭分别各 148 次请求全部有效。关闭模式保持完整内容、父关系、终态与压缩窗口校验，未测 SQL/stage 字段为 `null`；Linux 进程 I/O 窗口也实际取得。每种模式仅一个配对，不能据此宣称稳定性能或内存收益；它们验证的是新采集分支与结构化输入，并非最新 GC 的产品冒烟。

`accounted` 版本的 SQLite storage 为 73 passed，PostgreSQL storage 原为 73 passed / 1 个旧索引数量断言失败；删除该偶然断言后，真实重连用例独立复验通过，未伪改原失败报告。Admin 58 passed、proxy 117 passed。包含当时有界 GC 实现的原生 Desktop（二进制 SHA-256 前缀 `02884f4f8832`）已在隔离 Xvfb、D-Bus 与已安装托盘运行库下通过全部 10 项存储故障用例，0 skipped；此前启动 abort 位于 `libappindicator_sys` 动态库加载，补测试进程搜索路径后消失，未修改产品安全控制。相同源码的 PostgreSQL compaction retention 专项再次通过。S3 opt-in 专项仍缺隔离环境，未运行；本轮没有修改其实现。

### 大输入跨协议产品对照

包含当时有界 GC 实现的 Server SHA-256 前缀 `93089f3fe704` 与上述冻结基线对照：四个入口 Chat / Responses / Anthropic / Google 均转到 Responses 上游，分别 unary/stream，SQLite 与 PostgreSQL 两种后端，每版本每后端八个 case。共 32 个 case、128 次计时请求全部有效；逐请求完整正文、四方向 Wire、真实 Wasm、持久历史、Principal 与凭据保护证据均通过。使用 schema 5 / `full-chain-credentials-token-paced-v3` 审计，真实比较命令成功输出两份 CSV；没有设置内存目标，报告的 `meets_target` 只代表有效性，不是性能验收。

每次输入 50k–75k tokens、1–2 张图片；两个 worker、两轮会话、每 case 四次计时请求及独立预热。上游先等待 1 秒，再按 30 tokens/s 输出 10 秒，不能把其有限 cohort 吞吐当成饱和吞吐。下表峰值为该运行八个 case 的采样最大值；TTFT 为四个流式 case 各自 p95 的范围，不是混合样本分位数。

|后端|Server 峰值 RSS MiB（基线 → 候选）|流式各 case TTFT p95 范围 ms（基线 → 候选）|
|---|---:|---:|
|SQLite|826.05 → 837.32|1,154.64–1,202.25 → 1,190.82–1,986.82|
|PostgreSQL|837.98 → 830.88|1,179.10–1,253.00 → 1,155.76–1,248.50|

每版本每后端只有一次 cohort，不能据此认定稳定收益或把 SQLite 回退排除为噪声。未锁 CPU/频率、未清 OS cache；Server CPU 不含 PostgreSQL/Valkey。SQLite 每次新临时库，PostgreSQL 每组新建并在结束后删除独立数据库；专用 Valkey 实例由不同 Server 的随机 runtime-cache namespace 隔离，而非每组重启实例。早先复用已升级 PostgreSQL 库的基线启动失败已原样保留，不计入有效对照；修正实验隔离后，两端全部通过。完整方法与能力边界见 [跨协议基准](../research/protocol-http-benchmark.md)。

### 最终版本的跨协议验收

同一最终 Server `9e52514d9e03` 与基线 `4f739d22000e` 完成上述大输入形状的双后端验收：**32 个 case、128 次计时请求、64 次预热请求**全部通过完整链路检查。每请求均核验实际 Route/Target、四方向 Wire、真实 Wasm 路径、Generation 历史与精确正文、Principal 和凭据映射，报告仍是 schema 5。两份正式比较 CSV 通过契约检查；没有配置内存上限，`meets_target` 仍只表示有效性。

首轮 PostgreSQL 候选的首个 Chat unary 预热请求曾被判无效：HTTP、交付收据和 trace 已完成，但审计器仅凭 `completed`/trace 终态提前读取，尚未取得最终 `run_finished` 与 Generation 关联。原失败报告完整保留，整组不计入有效成绩。通过已有 `audit_requests` 接口，以“已交付、延后收口、永久缺失”三种状态和 Run ID/header 两个入口复现竞争；修正为等待真实 `run_finished.delivery` 终态，六项回归通过，相关 Python 检查共 **193 passed**。未改产品二进制、审计截止时间、失败判定或推理重试策略。修正后 PostgreSQL 候选→基线在全新独立数据库中各完成八个有效 case；SQLite 使用首轮已完整通过的两端成绩。

|后端|最终运行 Server 峰值 RSS MiB（基线 → 最终）|流式各 case TTFT p95 范围 ms（基线 → 最终）|
|---|---:|---:|
|SQLite|859.31 → 832.83|1,210.54–1,281.71 → 1,194.96–1,232.17|
|PostgreSQL|842.91 → 834.09|1,180.11–1,229.00 → 1,183.32–1,244.58|

仍不能宣称各协议普遍加速：SQLite Chat stream 的 TTFT p95 为 1,215.93 → 1,232.17 ms，PostgreSQL 同 case 为 1,180.11 → 1,189.67 ms。每 case 仅四次计时请求，nearest-rank p95 在此样本数下就是最大值；每后端/版本只有一个 cohort，不是稳定排名、合并分位数或饱和吞吐。资源及隔离边界与上一节相同，Server CPU 不包含 PostgreSQL/Valkey。

原始文件为私有证据目录下 `final-protocol/` 的 SQLite 两端、`final-protocol-delivery-settlement/` 的 PostgreSQL 两端；汇总为 `final-protocol-analysis.json`。初次 PostgreSQL 无效报告留在前一个目录，未覆盖、未改成通过，也未与最终有效 cohort 混算。

## 6. 前期隔离探针：不是最终产品性能

### 6.1 真实 SQL 形状的合成 SQLite 场景

使用 Python 内置 SQLite **3.53.3**、`:memory:`、`foreign_keys=ON`。历史表及索引取自重构前的 `docs/database/sqlite.sql`，执行当时的叶子 DELETE；候选探针保留同样的 FK/TTL/无子节点判定，每事务最多处理 128 个候选，删除后把 parent 放回候选集合。只有线性过期链，无正文、内容引用或真实磁盘 fsync；不是产品完整 GC 性能测试。

|节点数|重构前 DELETE/提交轮数，含末尾零结果|候选探针提交轮数，含末尾零结果|重构前 SQLite VM 步数，约值|候选探针 VM 步数，约值|
|---:|---:|---:|---:|---:|
|64|65|2|40,000|13,400|
|256|257|3|553,400|54,300|
|1,024|1,025|9|8,505,300|217,800|

VM 步数通过每 100 指令一次的 progress handler 估计，不是 SQL 语句数或真实 IO。当前查询计划已使用 expiry 和 parent 索引，仍出现上述增长；**问题是反复扫描的工作集，不是简单缺索引**。两种场景均删除预期全部节点，候选方案 `foreign_key_check` 为空。没有验证 PostgreSQL 并发或故障恢复。

这个候选探针为隔离工作集效应而逐节点查询/删除，并未证明 SQL round trips 减少；照搬到远程 PostgreSQL 可能被额外往返抵消。正式 Adapter 应集合领取、集合验证与删除同层候选，收集父节点/内容 ID 后批量入队。严格线性链在保留现有即时 RESTRICT FK 时仍有逐层工作，不能把提交批次减少说成全部 SQL 次数变成 O(1)。

另以当前 Interaction 表及索引生成 50,000 个成员、1,000 个根，查询返回同样的 50 个根，保留既有 `ORDER BY root_id`：

|查询形状|SQLite VM 步数，约值|计划|
|---|---:|---|
|当前按成员聚合 root MAX|462,200|扫描 `interaction_observations_window_idx` 覆盖索引|
|预计算根投影，按根活动时间过滤|600|按 `root_time(last_active_at,root_id)` 范围搜索，再临时排序 root_id|

这只支持“根级投影减少此查询的读取工作”，未计入维护投影的新增写成本、token/provider 筛选、根重归属或并发。不能将步数之比宣传为 HTTP 加速倍数。

### 6.2 sonic-rs / BLAKE3 隔离程序

版本：`sonic-rs 0.5.10`、`serde_json 1.0.154`（与项目一样启用 `arbitrary_precision`）、`blake3 1.8.7`、`sha2 0.11.1`、Rust 1.99.1、Linux x64、release。使用真实 `stravia-runtime-contract` path dependency 验证 `ContentBlock::ToolUse`，没有修改项目依赖。

已观察到的兼容性结果：

- `sonic_rs::Value` 共享 clone 后修改，原值不变；跨线程只读、null 与缺失键区分正常。所测非法 UTF-8、非法 token、尾随垃圾被完整 DOM 解析拒绝。
- `{"a":1,"a":2}`：serde_json 结果只剩 `a=2`；sonic Value 序列化仍保留两个键，lazy `get(a)` 返回第一个 `1`。`sort_keys` 不等于去重。
- **即使 sonic 开启 `arbitrary_precision`，`sonic_rs::from_str::<serde_json::Value>` 仍把 `18446744073709551616` 转成 `1.8446744073709552e+19`。** 同样问题在项目真实 `ContentBlock::ToolUse.input` 上重现。
- sonic 自身的 Value 在该 feature 下保留上述整数、`1e400`、`1.2300` 和 `-0.0`；桥接到 serde_json Value 时，所测大数/格式仍改变，`1e400` 报非有限浮点错误。不能依靠两个库同时开启 feature 获得无损桥接。
- 补充互操作烟测：`sonic_rs::to_string(&serde_json_value)` 将数字输出为 `$serde_json::private::Number` 对象，反向序列化则出现 `$sonic_rs::private::JsonNumber`；`get` 接受所测文档后的尾随垃圾；修改重复键对象中无关的 `b` 字段后，`a` 从共享视图的 first-match 语义转成后者覆盖。不能只验证“未修改时读得到值”。
- BLAKE3 分块与一次性摘要相同，不同固定 domain context 产生不同摘要。
- `wasm32-wasip2` 的 sonic Value/COW/大整数与 BLAKE3 探针编译成功；环境无 `wasmtime` CLI，**未执行 Wasm 程序，也未测 Wasm SIMD 性能**。

1 MiB 合成正文的 GlobalAlloc 存活请求分配（System allocator，含句柄容器；不是 RSS）：

|状态|serde_json Value，B|sonic Value，B|
|---|---:|---:|
|一份 DOM|1,049,852|1,049,228|
|原件加八份 clone|9,448,924|1,049,356|
|仅保留 tiny 子树，释放根|未测|1,049,228|

sonic clone 的收益明确，但**小子树可钉住整个解析 arena**。不能用子树序列化长度给这种缓存项计重。

下面是混合中文、转义换行和长文本的单一合成形状，7 个批次的每次操作中位数。未固定 CPU、未交错配对，也未模拟生产并发；仅记录反例与选型信号，不作速度承诺。sonic 启用 `precision + sort_keys`。

|操作|输入字节|serde_json / SHA-256，µs|sonic-rs / BLAKE3，µs|
|---|---:|---:|---:|
|完整 DOM 解析|8,556|8.139|12.305|
|完整 DOM 解析|1,086,109|922.364|1,986.341|
|DOM 序列化|1,086,109|636.624|184.203|
|摘要|8,556|3.497|4.111|
|摘要|1,086,109|443.829|208.759|

尤其大输入 sonic 解析批次范围为 1,049.900–2,069.031µs，波动明显。结论不是“sonic 解析必慢”，而是**不能用库名或官方跑分预设本项目加速；BLAKE3 也不是每个小输入都更快**。

本轮命令在仓库外临时 crate 运行：`cargo run --release --manifest-path <probe>/Cargo.toml`，分别以默认 feature、`precision,sorted,track -- memory`、`precision,sorted`、`precision,sorted -- timing` 执行；补充边界用 `--bin interop --features precision,sorted`；Wasm 使用 `cargo build --release --manifest-path <probe>/Cargo.toml --target wasm32-wasip2 --bin portable --features precision,sorted`。探针未设置 RUSTFLAGS/CARGO_ENCODED_RUSTFLAGS，不能把结果当作开启全部 SIMD 的成绩。临时程序不是产品实现，不保留为永久测试。

## 7. foyer 实测决策：本轮不引入

### 7.1 最终选择

**不添加 foyer 生产依赖，不替换 RuntimeCache，也不为新历史内容加入持久磁盘缓存。** 保留当前本地 TinyUFO / PostgreSQL 配置 Redis 的部署契约；正式实现采用共享 FrozenItem、数值 ordinal descriptor、带域 SHA-256 与候选 GC；不把根投影、统计差额或 BLAKE3 当成已采纳的后续阶段。缓存粒度调整仍应复用统一预算，不自动再增加一份独立预算。

原因不是 foyer 无法缓存，而是本轮目标读取路径的净收益不足：热点命中时与 TinyUFO 相近，冷热混合虽然能免去正文 SQL，p95 却明显增加；扫描压力下也未形成稳定优势，且有额外写入和生命周期复杂度。普通本地 SQL 的集合读取很便宜，磁盘命中仍需逐内容读取、解压、解析、校验和分配，不能把命中率当作端到端收益。

本结论针对本地/自托管的所测内容读取形状，不推断“foyer 对所有负载都慢”。高延迟远程数据库、数据库容量瓶颈、S3 重复下载均未实测，也不是本轮默认集成路线。S3 消费者需要 LocalPath，若另有明确需求，应独立比较有界文件缓存。

### 7.2 对比方法与公平性

本轮使用仓库外临时 Rust crate，直接引用项目 [storage_codec.rs](../../backend/crates/stravia-core/src/storage_codec.rs)，不复制其实现；使用 serde_json 1.0.154 的 arbitrary_precision、zstd 0.14.3、BLAKE3 1.8.7、SQLx 0.9.0。四种模式为无额外缓存、TinyUFO 0.9.0、foyer 0.22.6 仅内存 LRU、foyer 0.22.6 LRU＋磁盘。

- 数据：4,096 条参与测量的合成内容，另有 4 条权限/到期/删除探针；四个 Principal。正文共 94,921,380 字节，SQL 编码后 12,936,265 字节；递归估计并按 KiB 取整的逻辑权重共 244,872,192 字节。混合中文文本、结构化工具结果和超出 u64 的整数，没有真实用户数据。
- 单项最大原始 JSON 1,042,495 字节、最大逻辑权重 4,202,496 字节。全部缓存返回相同 Arc<Item>，使用相同权重、16 MiB 逻辑内存预算和相同内容验证。预算不是 RSS 上限，TinyUFO 的记账方式参照现有 RuntimeCache，但这不是直接替换现有五个缓存域的实验。
- foyer 最终采用 2 个内存分片、256 MiB 磁盘预算、4 MiB block、1 个 flusher、1 个 reclaimer、4 MiB buffer pool、8 MiB submit queue threshold、WriteOnInsertion、flush_on_close 和 tombstone log。除内存项外还存在缓冲、在途引用、索引与分配器占用。
- foyer::Code 手写编码：仍走同一 serde_json 与 storage codec；不使用 bincode 直接解码 Value，不把序列化开销从磁盘路径藏掉。Key 固定 Principal、格式版本和完整 BLAKE3；所有模式先向 SQL 核验归属/期限，再查内容缓存；miss 集合批量查正文。
- 两个实际后端：NVMe 上的 SQLite 文件（WAL）和独立 PostgreSQL 16.15（Unix socket，仅测试库）。不是内存假数据库，不用 sleep 模拟网络，也没有连接用户配置的 DB_URL。
- 每请求 16 个不重复内容；4 个独立 Tokio 请求任务，4 个 worker。热点与混合各 512 请求/阶段，扫描 256 请求/阶段。每模式测空应用缓存、同进程第二轮、新进程恢复；恢复含真正退出旧进程，不仅 drop/reopen 一个对象。
- 三种负载：热点为前 128 项（逻辑权重 8,666,112 字节，可驻留）；混合按 90% 热区候选、10% 全量其余候选生成并去重，不声称实际命中率恰为 90%；扫描每轮遍历全部内容，工作集远超预算。
- 每个组合 5 次，交错/轮换模式顺序；最终 120 个组合，共 153,600 个计时请求。下面是各轮统计量的中位数，不是合并样本后的整体 p95，也不是置信区间。
- 主机为 i5-13600K；客户端固定 CPU 0–3，PostgreSQL 父进程固定 CPU 4–5；System allocator。文件系统为 NVMe/Btrfs，带 zstd:3 压缩。未丢弃 OS 页缓存，未固定 CPU 频率；“冷”仅指应用缓存空，不是冷 SSD。

探索阶段发现 4 分片使最大项略超单片预算，已改成 2 分片；同时将单父任务中的并发 I/O 改为独立请求任务，避免给 foyer 内部解码并行不公平的优势。探索结果不计入下表，最终四种模式全部按同一调度重新测量。

### 7.3 暖态请求 p95

单位 ms，包含权威查询、缓存访问、正文 miss 批读及解码验证，不包含模型调用或 HTTP 编码。

|后端 / 负载|无缓存|TinyUFO|foyer 仅内存|foyer 内存＋磁盘|
|---|---|---|---|---|
|SQLite / 热点驻留|9.755|0.153|0.152|0.158|
|SQLite / 冷热混合|9.894|1.211|2.239|2.022|
|SQLite / 全量扫描|8.370|9.582|11.389|11.385|
|PostgreSQL / 热点驻留|9.761|0.097|0.100|0.102|
|PostgreSQL / 冷热混合|9.301|1.212|2.082|2.188|
|PostgreSQL / 全量扫描|7.978|9.579|11.768|11.043|

热点的几个微秒差别不作为显著收益/退化。混合负载的差别更明确：SQLite 的 TinyUFO 每轮 p95 范围为 1.038–1.368 ms，foyer hybrid 为 1.666–2.320 ms；PostgreSQL 分别为 1.148–1.485 和 1.903–2.399 ms。按中位数比较，hybrid 混合读取约慢 67% / 81%。

扫描下三种缓存都没有稳定胜过无缓存；这提示准入和工作集比“强行缓存全部内容”更重要，不据此未经产品回放直接关闭当前缓存。

### 7.4 SQL、内存和写入：不能只看命中率

下表正文 SQL 为暖态一轮的查询数；RSS 为该新进程冷读＋暖读后的 VmHWM；缓存写入为 foyer 自身 storage statistics 在冷读＋暖读＋close 后的累计写字节，不是 SSD NAND 写放大。各列是独立统计量的中位数。

|后端 / 负载|正文 SQL：TinyUFO → foyer|峰值 RSS：TinyUFO → foyer，MiB|foyer 缓存写入，MiB|
|---|---|---|---|
|SQLite / 热点驻留|0 → 0|47.0 → 47.9|1.21|
|SQLite / 冷热混合|454 → 0|79.3 → 68.7|6.82|
|SQLite / 全量扫描|256 → 124|115.7 → 183.4|21.75|
|PostgreSQL / 热点驻留|0 → 0|43.7 → 25.3|0.85|
|PostgreSQL / 冷热混合|454 → 0|64.7 → 61.0|6.94|
|PostgreSQL / 全量扫描|256 → 143|91.7 → 165.4|20.39|

每请求的权威检查仍存在：热点/混合各 512 次、扫描 256 次，不因命中而跳过。混合负载正文命中率由 TinyUFO 约 87.8% 提高到 foyer 约 100%，但 SQL 省下来的成本不足以抵消缓存磁盘路径的开销。RSS 不是所有场景都增加；混合场景反而较低，不能只摘取扫描数字宣称全面更耗内存。

扫描中普通写入策略下 foyer 第二轮命中率中位数仅约 SQLite 54.5% / PostgreSQL 47.0%，没有假定写入即可靠持久化。后台 admission、缓冲和回收有独立约束；本轮没有采集完整的拒绝原因计数，不能把这些 miss 全部归因于某一种队列机制。

### 7.5 真正重启与“完全暖盘”上界

正常恢复的新进程：下面计入 Cache::open 恢复耗时及整个首轮请求的 wall time，单位 ms；不含 OS 创建进程和 SQL 建池时间（所有模式相同排除）。

|后端 / 负载|TinyUFO|foyer 内存＋磁盘|
|---|---|---|
|SQLite / 热点驻留|27.31|44.78|
|SQLite / 冷热混合|70.01|98.27|
|SQLite / 全量扫描|219.93|232.81|
|PostgreSQL / 热点驻留|28.34|45.29|
|PostgreSQL / 冷热混合|68.83|98.67|
|PostgreSQL / 全量扫描|227.99|247.14|

foyer 的恢复打开耗时在各场景中位数约 17.6–23.5 ms，TinyUFO 新建约 0.14–0.16 ms。恢复磁盘字节不等于恢复解析后的 DOM。

为了排除异步写入尚未全部准入造成的不利条件，另做 10 个上界场景：逐批预填全部内容，调用正式 storage.wait 屏障、close，再启动新进程扫描；预填不计入请求延迟，也没有用 sleep 或重试来凑命中率。预填约 0.49 s，仅用于选型上界，不是建议的产品预热流程。

|后端|预填后重启：正文命中率 / SQL|foyer p95，ms|foyer 请求/秒|
|---|---|---|---|
|SQLite|100% / 0|10.383|1181|
|PostgreSQL|100% / 0|11.882|1116|

此时正文命中率确实全部为 100%，仍没有形成低于 TinyUFO 正常恢复扫描的稳定 p95 收益。其新进程峰值 RSS 中位数约 SQLite 61.2 MiB / PostgreSQL 58.1 MiB；不能把正常写入压力阶段的峰值套用到完全预填后的只读阶段。

### 7.6 正确性：tombstone 不等于立即不可读

两后端独立验证了实际 JSON（含超大整数）往返、跨 Principal 拒绝、到期不可见、删除权威引用后不可见，以及超预算单项绕过；这些依赖读取前的权威检查，不是由缓存库替代。

另有一项**未满足的严格缓存契约**：`insert → remove → get` 在有在途写入时仍读到了值，原烟测以 `remove not applied` 失败。未添加等待、重试或特殊输入绕过它。源码已解释原因：

- HybridCache::remove 删除内存项，再调用 Store::delete。
- Store::load 先读 keeper；Store::delete 只写 engine 删除，不清 keeper。
- 上游 0.22.6 的 `test_may_contains_in_flight_entry_after_delete` 明确覆盖“删除后仍从 keeper 读到，flush 完后 tombstone 才生效”。见 [foyer-storage 0.22.6 store.rs](https://docs.rs/crate/foyer-storage/0.22.6/source/src/store.rs)，源码 154–174、238–254、628–670 行。

因此 tombstone log 解决的持久删除问题不能被宣传成同步失效保证，更不等于磁盘安全擦除。本轮不修改上游库、不降低删除可见性要求；该生命周期成本计入不采纳决策。前序字符串缓存的恢复烟测仍是其所测场景的有效证据，但不覆盖这次在途写入情况。

### 7.7 已执行范围与未覆盖项

临时目录为 `/tmp/stravia-foyer-compare-9xazaxdu`。实际执行 release build（`cargo build --release --locked --manifest-path <probe>/Cargo.toml`），随后驱动 `seed`、`smoke`、`policy`、`run`、`resume`、`prefill` 子命令；性能子进程均通过 `taskset -c 0-3` 运行。最终矩阵由 `matrix(..., trials=5, workloads=[hot,skew,scan], backends=[sqlite,postgres], concurrency=4)` 驱动；原始 JSON 随本次会话保留。临时数据库、进程与程序在取证后清理，不把探针当作产品实现或永久测试。

本实验是拟议不可变内容读取位置的原型，复用真实 codec/SQLx/缓存库，不是完整 TurnChainStore、HTTP、Desktop 或现有 Redis 与 foyer 的 A/B。PostgreSQL 在本机，未测远程 RTT、生产分布、服务器资源竞争或数据库容量瓶颈；未将本轮普通关闭恢复当作崩溃/磁盘满/损坏故障矩阵。没有跑产品全套测试，也没有改产品实现、依赖或 migration。

**最终方案收口：保留 serde_json 与现有缓存后端，先完成主结构重构；不增加 foyer 开关、空 Adapter、后台预热或第二份持久内容缓存。** 若未来出现不同的、已测量的远程 I/O 需求，应作为新决策处理，不保留本轮的自动集成待办。


## 8. 未采纳的替代设计

以下不是已实现能力，也不是自动执行的后续阶段：

- **全仓自有摘要切 BLAKE3、重发 token 或改公开 ID**：超出内容存储所需边界，且没有新增生产依赖授权；保留既有算法和身份。隔离探针的 BLAKE3 数据不是产品选型。
- **按 kind 完全 typed descriptor / 新 canonical 指纹公式**：本次不同时重写协议解码与历史身份；采用稳定 data/slots + ordinal 关系，领域 payload_version 独立演进。
- **observation_roots、terminal-event locator、stats 差额替代触发器**：尚无维护成本在内的产品净收益证据；不能为了省读而默默增加投影写入、改变 snapshot 水位或费用累计。现有触发器及公开查询契约保留。
- **Observation tombstone 工作流、retention policy epoch、文件 owner 反向索引**：未实施。Observation 仍按既有所有权和清理契约运行，单个很大的 Interaction 仍有不可拆删除成本。不能把历史链的候选 GC 冒充这些路径已优化。
- **根/head lease、全局 interner、S3 持久 read-through、PG 分区**：没有以本轮证据采纳；各自会增加生命周期、隔离或跨后端维护成本。

## 9. 源码与维护入口

- [历史内容与 descriptor](../../backend/crates/stravia-core/src/turn_chain/content.rs)、[SQL/续期](../../backend/crates/stravia-core/src/turn_chain/sql.rs)、[候选 GC](../../backend/crates/stravia-core/src/turn_chain/gc.rs)、[规范化转换](../../backend/crates/stravia-core/src/turn_chain/upgrade/normalize.rs)。
- [共享窗口物化与估重](../../backend/crates/stravia-core/src/generation_chain/materialize.rs)、[提交投影](../../backend/crates/stravia-core/src/generation_chain/project.rs)、[Observation 写入](../../backend/crates/stravia-core/src/interaction_observation/store.rs)。
- [ADR-0022：delta 与物化](../adr/0022-materialize-generation-chains-from-deltas.md)、[ADR-0076：规范化历史](../adr/0076-deduplicate-turn-chain-items-and-share-binary-storage-codec.md)、[ADR-0077：Observation 事实与 manifest](../adr/0077-slim-interaction-observation-and-file-debug-manifests.md)、[成本与计费](cost-and-billing.md)。
- [架构与 schema 导出约定](architecture.md)、[既有协议基准及限制](../research/protocol-http-benchmark.md)。
- SQLite [WAL](https://www.sqlite.org/wal.html)、[FK 索引](https://www.sqlite.org/foreignkeys.html#fk_indexes)、[WITHOUT ROWID](https://www.sqlite.org/withoutrowid.html)；PostgreSQL [锁](https://www.postgresql.org/docs/16/explicit-locking.html)。这些是设计依据，不替代本项目行为验证。
