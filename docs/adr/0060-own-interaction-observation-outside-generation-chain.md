---
status: accepted
---

# 在 Generation Chain 之外拥有 Interaction Observation

Stravia 以 crate-private `InteractionObservation` 深模块拥有 Connect Client Interaction、Inference Run、Model Turn、Target attempt、Platform Tool、Delivery、Confirmed Upstream Usage 与 Debug Trace 的诊断投影。Inference Run 等执行模块只提交 typed observation event；Generation Chain 仍只保存完整交付的 `completed` / `incomplete` 模型历史，并仅向 Observation 提供已确认的 parent/root 关联。这样进行中、失败、取消、断线和拒绝请求可以实时展示并保留，而不会把可变运行状态塞进模型历史事实源。

普通事件和查询投影保存在 SQLite/PostgreSQL；大体积 Wire/canonical Debug payload 保存在 `data_dir` 下的受管分段文件，数据库只保存 manifest 与索引。Debug 在每个 Inference Run 准入时快照当前进程级开关，允许同一 Interaction 部分捕获；任何观察或抓包失败只标记 gap/partial，不得阻塞或改变推理结果。Admin 通过同一模块查询时间窗 forest、订阅带 cursor 的 SSE、清理历史并签发单次流式 ZIP 下载票据；Server 与 Desktop 不解释分组、Trace 或导出规则。

## Considered options

- 扩展 Generation Chain 保存 running/failed/cancelled/debug：接口表面更少，但会破坏其“完整交付模型历史”的不变量，并让诊断丢失或清理影响历史语义。
- 在 Axum middleware 记录：容易拿到 client wire，却看不到 core 内 Hook、Model Turn、Target failover、Platform Tool、Client Projection 与 Desktop 共用执行，业务规则会泄漏到 transport adapter。
- 继续使用完成后 `request_logs` 加独立 debug JSONL：改动较小，但无法可靠形成 Interaction、实时状态、Run 子树、SSE cursor、统一保留和原子清理，两个记录路径会继续漂移。
- 把 Debug payload 全放数据库：备份简单，但最多 2 GiB 的 chunk/frame 会放大数据库与 WAL，并把主存储健康绑定到临时诊断负载。
- 全部使用文件：延续现有 Wire Capture，但时间窗查询、关联、状态更新和统计必须扫描文件，无法支撑管理面实时投影。

## Consequences

- 旧 `request_logs`、`LogStore`、`STRAVIA_WIRE_CAPTURE_DIR` 与 `/api/v1/logs` contract 干净删除；升级不迁移旧请求记录。
- 用量分析及 Route Scheduling Strategy 原来依赖 `request_logs` 的聚合改读 Model Turn/Target attempt Observation，避免双写与两套 usage 事实。
- Observation 与 Debug Trace 跟随同一保留期；清理用 tombstone 和启动回收保证数据库索引与本地分段文件最终一致。
- Debug Trace 是应用协议级消息与稳定 canonical checkpoint，不是 packet capture；凭据在落盘前统一脱敏。
- 当前只承诺单 Gateway 实例实时行为。多实例通知、共享 Debug payload storage 和集群级易失开关需要另行决策，不能由 PostgreSQL 共享表暗示已经支持。
- 完整接口、状态、存储、Admin contract、画布和验收场景见 [`docs/design/interaction-observation.md`](../design/interaction-observation.md)。
