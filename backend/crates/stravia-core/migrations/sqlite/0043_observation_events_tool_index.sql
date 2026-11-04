-- 命中详情按 tool_id 回溯工具事件（名称、参数、platform_tool_finished 正文）；
-- 客户端工具没有 platform_tool_finished，查询必然落到按主体回溯的分支。
-- 0042 删除此索引后该分支沿 sequence 主键扫描整张事件表，侧栏长时间停在加载中。
CREATE INDEX observation_events_tool_idx
    ON observation_events(tool_id, sequence)
    WHERE tool_id IS NOT NULL;
