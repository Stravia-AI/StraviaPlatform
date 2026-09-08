---
status: accepted
---

# Model Turn 在还原与映射发布后交出唯一成功终态

本 ADR 的终态迁移已落地：Live Model Turn Executor 内部 gate 拥有还原后的发布与唯一成功终态，三种消费路径不再持有独立发布义务。

迁移前，可逆脱敏映射的发布由 buffered Inference Run、live Inference Run 和 Agent Runner 分别触发。buffered 在读取 EOF 后发布，live 在 `Completed` 时发布但继续读取，Agent Runner 在 `Completed` 时发布并返回；调用方因此既要知道映射保留期义务，也要各自解释成功终点。

选择在现有 canonical stream 内部集中终态处理：`Completed` 是 Model Turn 唯一成功终态，只有该轮规范化输出还原与所需映射发布均成功后才能交出，之后永久结束，不再等待 EOF 或接受后续错误。调用方继续消费 canonical stream，不新增 unary 执行方法、executor decorator 或不可拆卸的公开 ModelTurn 消费 interface。

## 顺序与取消契约

内部流终态 gate 依次处理：

1. 读取 canonical 输出，执行现有还原与引用跟踪。
2. 收到上游完成响应后，完成响应还原，并先将还原器的尾部 delta 交给消费方。
3. 在尾部 delta 已交出后，读取共享 trace 当时的引用并发布映射。
4. 发布成功且未被取消或超时裁决为终止，才交出唯一 `Completed`；之后不再产生输出或失败。

还原、发布或上游错误均显式终止，不用成功终态掩盖。尚未收到完成响应就遇到 EOF，仍是流未完整完成；上游 producer 在成功终态之后的输出不是合法 Model Turn 内容。

取消与超时优先于仍未交出的成功终态，可以打断对发布结果的等待；不得为了收尾而新增后台写库任务或等待发布完成后才响应取消。数据库可能已经提交，因此取消不证明映射尚未发布，已成功写入的发布结果也不回滚。成功终态一旦交出，后续客户端取消不能反过来改变该 Model Turn 的终态，但所属 Inference Run 仍可能交付失败。

空 mappings 只允许绕过文本还原，不能绕过终态 gate。共享 trace 可能包含其他相关 Model Turn 已登记的引用，不能用构造时的空映射或早期引用快照代替发布时读取。复用现有还原能力，不为每个 delta 引入额外任务、payload 复制或不必要的堆分配。

## 与交付、存储及观测的关系

- 映射发布不等待客户端完整交付，也不合并到 Generation Chain persist；内部 Agent Model Turn 同样需要独立完成发布。
- Generation Chain 仍只在完整交付后提交。取消、交付失败或发布失败不得形成伪成功历史；已经发布的映射沿用原保留与清理规则。
- [ADR-0042](0042-keep-reversible-redaction-detection-local.md)、[ADR-0043](0043-scope-redaction-mappings-to-api-keys.md)、[ADR-0044](0044-restore-secrets-in-tool-call-arguments.md) 的本地检测、Principal 隔离、保留期及工具参数还原约束不变。
- Target attempt 的上游成功、Confirmed Upstream Usage 与健康统计不因后续映射发布失败而被改写为上游失败；Model Turn 成功观测则必须在最终 gate 成功后记录。复用既有 Observation，不形成两个终态 owner，不重复记录完成事件，观测失败仍不影响推理结果。
- [ADR-0001](0001-inference-run-lifecycle-seam.md) 的客户端交付所有权和 [ADR-0013](0013-own-provider-transport-behind-model-turn-executor.md) 的 Model Turn Executor 职责继续成立；gate 不吸收 Hook、Client Projection、Platform Tool 或 Delivery。

## 不采用的方案

- **原样保留三种完成时机：**虽然迁移较小，却仍需要消费方完成确认，发布义务只是改名，interface 的 depth 收益有限。
- **在客户端完整交付或 Chain persist 时发布：**把 Model Turn 成功与外部交付混为一谈，也不能覆盖不提交 Agent Turn 的内部执行。
- **executor decorator：**需要迁移装配与测试注入点，并增加包装和观测归属复杂性；当前没有足够独立替换需求。
- **隐藏公开 output、只能逐事件消费整个 ModelTurn：**构造约束更强，但会破坏现有公开字段与 Stream 消费方式；本次不需要这项额外迁移。
- **取消后回滚映射发布：**同一 Principal 的请求可以共享映射，回滚会伤及其他合法引用，也无法把取消当作数据库未提交的证明。

## 实现与验证边界

独立的 publication 类型、ModelTurn 发布字段及三处消费方的手动发布路径已删除；空映射只跳过不必要的文本还原，不能绕过 gate。buffered、live 和 Agent 消费方在 `Completed` 后结束读取。Model Turn 终态 guard 从执行开始移交给输出流，仅记录一个最终结果；Target attempt 保留真实上游结果。内部 stream 持有待决异步状态，丢弃一次 `next()` future 不会重新启动发布，也不新增后台写库任务。

测试从 stream interface 验证尾部 delta、发布与成功终态的顺序，以及发布失败、发布期间取消、期限到达、空映射但共享 trace 有引用、提前 EOF 和终态后不继续读取。取消测试允许数据库已经发布，不能断言“取消必然没有发布”，也不依赖 sleep 制造时序。

保留现有映射隔离、保留期单调性、过期不复活、重启恢复、工具参数还原及交付失败不提交历史的行为测试。当前所核对的两个生产输出构造路径都把 `Completed` 放在末尾；终态后错误用例是契约反例，不作为已复现生产故障的证明。
