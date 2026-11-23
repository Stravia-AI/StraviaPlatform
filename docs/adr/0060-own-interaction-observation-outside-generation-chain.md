---
status: accepted
---

# 在 Generation Chain 之外拥有 Interaction Observation

Stravia 将 Connect Client Interaction、Inference Run、Model Turn、Target attempt、用量和 Debug Trace 的诊断投影放到独立的 Interaction Observation 模块。执行模块提交有类型的事实，Generation Chain 只保存完整交付的合法历史并提供已确认的父边与根；进行中、失败、取消、断线和拒绝不是模型历史节点。这让管理面查看完整诊断过程，并避免诊断丢失、清理或分组反过来改变执行。

不采用仅在 HTTP middleware 或请求完成后记录：两者都无法独立表达内部执行、重试与交付顺序。普通投影留在数据库以便关联与查询；大量 Wire 内容写入受管文件，不放大主库。存储边界由[存储设计](../design/data-layout-performance.md)拥有，捕获、分组、清理、实时与导出契约由[请求记录设计](../design/interaction-observation.md)拥有。

运行期存储失败由所属请求或任务处理，不停整个实例。关键同步写入失败不得报告成功；独立诊断写入失败只形成 gap／partial，不改变响应、重试、Target 选择或 Generation Chain。已交付内容不可撤回，缺失用量不补零、不估算、不重放；独立统计汇总须先保证应计事实已可靠汇总，否则相关破坏性清理失败。启动或升级仍可因必要存储失败而停止。这个选择保留无关工作的可用性，代价是诊断可能不完整，必须如实呈现。
