---
status: accepted
---

# 用 `:` 选择器与纯文本字段读取路径

StraviaRead 采用必填的 `path` 和可选的 `body`。`path` 只承载地址与末尾的 `:` 选择器；问题、搜索文本及追问以 `body.question` 原样传入，`attachments`、`allowed_domains` 和 `wait` 也在 `body`。联网搜索用 `stravia://search`，续接直接用 `stravia://turns/<turn-id>` 作 `path`；搜索和媒体续接各自校验其适用字段，不由解析器猜测 Turn 类型。

把自由文本与地址分开，避免表单解码把 `+` 当作空格、拒绝字面 `%`，以及为嵌套 Turn 地址再次编码。行范围、`raw`、`download` 与 `cursor=…` 是受限的路径后缀，不把所有选项都平铺成工具 schema 字段；下一页路径因此可直接复制。选择器的适用范围由目标资源决定。普通 HTTP(S) 查询与片段仍属于源站，端口不会被识别成行范围；外部 URL 确以类似选择器的内容结尾时，用 `%3A` 表达属于 URL 的字面冒号。旧平台选项语法不再接受，也不提供别名。

模型 schema 不展开 `body` 内的所有字段，避免强制模型填入无关参数，参数说明按暴露范围裁剪。`null`、缺省和空占位值按未设置处理；宽松声明不放宽运行时类型、未知字段、路径适用性或授权校验。通用语法、分页和执行范围由 [Agent Core 设计](../design/agent-core.md)维护；历史路径及其检索字段矩阵由[历史回看设计](../design/history-recall.md)维护。
