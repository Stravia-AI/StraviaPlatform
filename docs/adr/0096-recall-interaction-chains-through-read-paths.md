---
status: accepted
---

# 以 StraviaRead 路径回看交互链路

将 Interaction Observation 作为历史回看的唯一来源：用 `stravia://history/` 浏览当前 API Key 的交互链路，按 Interaction 读取只追加的转录和关联工具文件。复用 Observation 已有链路与身份，即使链路跨过多个 Generation Chain 根也不拆开。不新建 Session 或对话实体；Generation Chain 含模型历史与 system 指令，不适合作为人可见对话目录。观测缺口和历史清理如实反映，不用其他来源补全，也不把回看用于模型状态恢复、续接或授权。

选择现有 StraviaRead 路径范围加 `body.question` 做词法检索，而非独立工具或向量服务。结果给出可直接读取的路径与行范围；转录只追加，以免已返回的行号随状态更新而改变。两种数据库共享分词和排序语义，不依赖不同的数据库分词器，也不把历史外发给 embedding Provider。索引滞后或观测缺口意味着未命中不等于没有发生。

历史回看只经 API Key 明确选中的透明注入提供（默认不选），不开放显式 StraviaRead、MCP 或内部 Agent，也不设平台总开关。选择同时决定是否可用，不只是工具曝光偏好：风险是共用 Key 的人可互看对话，应允许按 Key 单独关闭，不影响其他工具。路径不授予权限，不公开条目是缺失、已清理还是属于其他 Principal。回看只覆盖现有历史保留范围；输出替换当前 Principal 已知且有效的秘密为可还原标记，历史网页和工具返回仍是不可信输入。

[历史回看设计](../design/history-recall.md)拥有命名空间、路径选择器、转录、检索与访问契约。
