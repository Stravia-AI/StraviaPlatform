# Use the revisioned Stravia Model Catalog as a template source

Status: accepted

Stravia 将模型目录事实源从 models.dev 干净切换到 `models.stravia.cn`，以 `/version.json` 的 immutable revision 协调 `/providers.json`、`/models.json` 与按需 `/providers/{provider}/models.json`；运行时不再下载完整 `/api.json`，内嵌 bootstrap 也只保留 Provider 与 Canonical Model 两个轻量索引。这样保留现有 Provider 创建后自动同步与账号级 discovery seam，同时避免重复下载完整 Provider Model 目录。

Canonical Model 以 `{lab_id}/{model_id}` 标识，只作为创建逻辑 Model 与手动 Provider Model 的一次性模板，不形成持久绑定或自动 overlay。逻辑 Model 选择模板时使用完整 canonical ID 作为客户端模型名；手动 Provider Model 复制除 `id` 外的完整 canonical 记录，把末段 `model_id` 预填为仍可编辑的 upstream model ID。Provider-scoped 条目已由上游应用 Canonical Model 基础数据与 Provider 覆盖，Stravia 不再次合并；导入后继续由 ADR-0001 定义的可编辑 Provider Model 快照承接。

## Consequences

- 两个模板搜索框都按 Canonical Model 的名称与完整 ID 搜索，并继续允许目录外手工输入。
- Lab 只作为 Canonical Model ID 的命名空间；本次不缓存 Lab 数据、不代理 Lab logo，也不新增 Lab 浏览或展示 UI。
- Provider Model 模板由 Core 按 canonical ID 从当前 revision 复制；不固定用户选择时看到的旧 revision。没有模板时生成只含 upstream model ID 的 bare metadata，不再 fuzzy 猜测 Canonical Model。
- 全局 Provider 与 Canonical Model 索引按同一 revision 校验后原子切换；Provider-scoped cache 按需刷新。当前 revision 的 scoped 下载失败会使同步或 re-import 失败并保持本地状态，不会把旧 LKG 冒充为成功。
- 首次启动离线时仍可浏览内嵌 Provider 与 Canonical Model 索引；尚无 scoped last-known-good 的 Provider 无法依赖目录导入模型。
- Catalog revision 更新不会改写既有逻辑 Model 或 Provider Model；管理员现有路由与 metadata 编辑保持本地事实。
- 升级 migration 将可识别的 `ai://models.dev/*` source 一次性转换为现有 Catalog source identity；运行时不保留旧 URI alias，也不读取旧完整目录缓存。
