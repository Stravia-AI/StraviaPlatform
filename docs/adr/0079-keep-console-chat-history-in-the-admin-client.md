---
status: accepted
---

# 控制台对话历史保存在管理端本地，并以 API Key 经网关发送

控制台对话（Console Chat）取代原概览页，作为管理面落地页。它是一个第一方调用方：浏览器或 Desktop 中的管理面以对话创建时选定的 Stravia API Key 直接请求网关 `/v1/responses`，每轮回放本地保存的完整客户端可见历史；对话列表与内容保存在管理面所在的浏览器或 Desktop 安装的本地存储中，Core 不新增对话实体、表或管理 API。

这样做使对话请求与 Connect Client 请求走同一条 Inference Run 路径，模型访问、Root RPM、透明注入、用量与观测无需第二套规则；同时保持 [ADR-0040](0040-separate-admin-identity-and-revoke-sessions.md) 的管理身份与推理身份分离，以及 [ADR-0007](0007-own-agent-execution-behind-native-seams.md) / [ADR-0064](0064-use-stravia-uri-references-without-sessions.md) 平台不引入 Session 的约束。对话历史属于这个客户端自己，正如 Codex 等 Connect Client 自行保存会话。

## Considered Options

- **Core 新建对话存储并提供管理端发送接口**：可跨设备同步，但需要 SQLite/PostgreSQL 新 schema、列表/详情/删除管理 API，以及让管理会话代表某个 API Key 发起推理的新入口。后者首次让管理身份触发模型调用，并在 Core 中引入一个带可变最新位置的对话实体；对个人开发者的本地工作台，收益不足以抵消这些边界变化。
- **复用 Generation Chain 或 Interaction Observation 作为对话列表**：Generation Chain 只能按精确节点物化，没有列表能力，按保留期过期，并混合同一 Principal 下所有 Connect Client 的流量；Interaction Observation 是可能缺失的诊断投影，不是历史事实源。
- **Responses `previous_response_id` 续接服务端历史**：续接依赖服务端保留期，过期后无法继续，且没有按 ID 取回响应的接口，本地仍需保存全文；因此采用每轮完整 `input` 回放。

## Consequences

- 历史按浏览器 origin 或 Desktop 安装隔离，不跨设备同步；清除站点数据会丢失。退出登录不清除本地对话，提供逐个删除与清除本浏览器全部对话；本地内容不加密。
- 一个对话固定所选 API Key：History Marker、可逆脱敏映射与 Artifact 只在同一 Principal 内解析，换 Key 会静默丢失隐藏历史。Key 删除、停用或过期后对话只读；模型可以逐条消息更换。
- History Marker 过期后，Core 解析时静默省略；继续较旧对话时，模型看不到当时的隐藏推理与平台工具原始结果，可见回答文本仍在历史中。前端不复制 Core 的保留期规则。
- 发送时 API Key 明文仅在浏览器内存中使用，不写入本地对话存储。跨 origin 部署管理面时，需要网关 CORS 放行管理入口。
- 对话不声明客户端工具；所选 Key 开启透明注入时，平台工具照常在 Inference Run 内执行并以 History Marker 投影，管理面原样保存与回放。
