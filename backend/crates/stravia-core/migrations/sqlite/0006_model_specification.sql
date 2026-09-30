-- Preserve Provider/Route/Target identity and all existing thinking maps.
-- Only explicit effort strings survive; toggle/budget declarations are not efforts.
UPDATE provider_models AS model SET metadata_json = json_set(
    metadata_json, '$.reasoning_efforts', json(COALESCE((
        SELECT json_group_array(value) FROM (
            SELECT value FROM (
                SELECT value, origin, position, effort_position,
                    row_number() OVER (PARTITION BY value ORDER BY origin, position, effort_position) AS occurrence
                FROM (
                    SELECT value, 0 AS origin, CAST(key AS INTEGER) AS position, 0 AS effort_position
                    FROM json_each(CASE WHEN json_type(model.metadata_json, '$.reasoning_efforts') = 'array'
                        THEN json_extract(model.metadata_json, '$.reasoning_efforts') ELSE '[]' END)
                    WHERE type = 'text' AND trim(value) <> '' AND lower(trim(value)) NOT IN ('default', 'null')
                    UNION ALL
                    SELECT effort.value, 1, CAST(option.key AS INTEGER), CAST(effort.key AS INTEGER)
                    FROM json_each(CASE WHEN json_type(model.metadata_json, '$.reasoning_options') = 'array'
                        THEN json_extract(model.metadata_json, '$.reasoning_options') ELSE '[]' END) AS option,
                        json_each(CASE WHEN json_extract(option.value, '$.type') = 'effort'
                            AND json_type(option.value, '$.values') = 'array'
                            THEN json_extract(option.value, '$.values') ELSE '[]' END) AS effort
                    WHERE COALESCE(json_type(model.metadata_json, '$.reasoning_efforts'), '') <> 'array'
                        AND effort.type = 'text' AND trim(effort.value) <> ''
                        AND lower(trim(effort.value)) NOT IN ('default', 'null')
                )
            ) WHERE occurrence = 1 ORDER BY origin, position, effort_position
        )
    ), '[]')));
UPDATE provider_models SET metadata_json = json_remove(metadata_json,
'$.attachment', '$.reasoning', '$.tool_call', '$.structured_output', '$.temperature', '$.interleaved', '$.reasoning_options', '$.reasoning_levels', '$.thinking_toggle', '$.limit.input', '$.limit.output');
UPDATE provider_models SET metadata_json = json_remove(metadata_json, '$.reasoning_efforts')
WHERE json_array_length(metadata_json, '$.reasoning_efforts') = 0;
ALTER TABLE provider_models DROP COLUMN attachment;
ALTER TABLE provider_models DROP COLUMN reasoning;
ALTER TABLE provider_models DROP COLUMN tool_call;
ALTER TABLE provider_models DROP COLUMN structured_output;
ALTER TABLE provider_models DROP COLUMN temperature;
ALTER TABLE provider_models DROP COLUMN limit_input;
ALTER TABLE provider_models DROP COLUMN limit_output;
