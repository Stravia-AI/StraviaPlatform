---
status: accepted
---

# 用优先级泳道编辑 Route Target，而不是 failover 图

管理面上的节点画布容易被做成带边的流程图，或把自由坐标存进 Route。选路并没有 Target→Target 的边：已启用 Target 按 Target Priority 分组，组内走 Route Scheduling Strategy，可重试失败才落到更低组（ADR-0034）。布局坐标也不是领域状态。

因此编辑器是左栈右坞的分层泳道。栈里每一行是一个已启用优先级组，上高下低；同层并排只表示同组，不是尝试顺序或 Weight。坞是已禁用 Target（ADR-0035）。画布和弹窗都不展示 Priority 整数；改组只靠拖到某层或叠顶/叠底 ±1（撞号则加入该层，不顺移其他 Target）。层内左右位置不入库。First Token Timeout 与 Target Cooldown 在弹窗用秒编辑（可到毫秒精度），存储仍为毫秒。

## Considered options

- 自由节点图加 failover 边：要新持久化边和坐标，并改写 ADR-0034 的选路。
- 看起来像图画、保存再投影回 priority：打开会重排，用户摆的位置是假的。
- 插入层时把一侧整组顺移：会改写其他 Target 已存的 Priority。
