---
status: accepted
---

# 命中记录同时记录新增与复用，并携带块级定位

## 决定

凭据保护「命中记录」原先只投影映射新建事件，来源类型在 Interaction 内取并集，复用不留痕，因此回答不了「哪条消息里保护了什么」。现决定：

- 新增普通事件 `credential_mappings_reused`，载荷与 `credential_mappings_created` 同形，在每个 Model Turn 中命中已有有效映射时发出。命中记录列表按 Interaction 聚合，新增与复用分开显示，时间取该 Interaction 在当前筛选下最近一次命中；复用不显示次数。
- 每个映射携带去重且有上限的 `locators`，逐项保存「规则 + 来源」配对，以及块级标识：工具参数与工具结果用 provider 原始 `tool_id`，用户消息只标记是否为最后一条用户输入，其余来源不带标识。不使用请求条目下标（`protect` 之前历史标记还原会拆分、注入条目），不保存字符偏移、消息片段或指纹。
- 请求记录从命中记录进入时，按 `tool_id` 与 Run 高亮命中块。解析不到的命中（来自更早的 Interaction、历史用户消息、system 与其他来源）只定位到 Run 并明确标注，不做块级高亮。
- 删除管理接口的页面级 `observation_gap`，其限制说明移入「保护详情与限制」。
- Debug 开启的 Run 中，两类事件的每个映射额外带占位符标记 `marker`，使上游 wire 里的标记能对应回规则和来源。Debug 关闭的 Run 不带；命中记录列表接口永不返回标记；不记录明文。

## 取舍

- 复用在每个 Model Turn 都会命中客户端重放的历史文本，写入量和噪声更大。采用载荷上限与展示层筛选控制，不在写入侧用不可靠的「新输入 / 历史」判定去重。
- 选择「事件加字段」而非给 Debug Trace 新增语义记录类型：后者违背 [ADR-0063](0063-record-four-direction-wire-debug-at-transport-boundaries.md) 的「Debug 只保存四方向 wire」，且只有写入 Trace 文件才能安全承载明文，而明文并无必要。
- `marker` 是对「事件不含可恢复引用」的有意例外，限于 Debug 开启的 Run；它不含秘密，持有它也不能脱离对应 Principal 的有效映射还原。
- 块级而非字符级高亮：提取偏移相对于提取文本（含上下文前缀、已解码的工具结果），与 Observation 里存储的 JSON 与渲染后的 Markdown 位置不一致，且 system、历史与其他来源没有保留正文。

## 后果

- 旧事件没有 `locators`，仍按规则与来源并集显示，进入请求记录时不高亮。
- 部分工具结果 ID 可能被 Generation Chain 重映射，此时命中退化为 Run 级定位。
- 无数据库迁移。
