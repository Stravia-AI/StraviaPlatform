-- 收敛 model_backends.thinking_level_map：将已淘汰的 disabled / enabled / budget 控制
-- 迁移为 effort，保持现有 effort 与 hidden 行及 source 标记不变。
UPDATE model_backends
SET thinking_level_map = (
    SELECT json_group_array(json(rewritten))
    FROM (
        SELECT
            CASE
                WHEN json_extract(je.value, '$.control.type') IN ('disabled', 'enabled', 'budget') THEN
                    json_set(
                        je.value,
                        '$.control',
                        json_object(
                            'type', 'effort',
                            'value',
                            CASE
                                WHEN json_extract(je.value, '$.control.type') = 'disabled'
                                    OR (
                                        json_extract(je.value, '$.control.type') = 'budget'
                                        AND COALESCE(CAST(json_extract(je.value, '$.control.value') AS INTEGER), 0) <= 0
                                    )
                                THEN 'none'
                                WHEN lower(COALESCE(json_extract(je.value, '$.level'), '')) = 'off'
                                THEN 'none'
                                ELSE COALESCE(NULLIF(trim(json_extract(je.value, '$.level')), ''), 'high')
                            END
                        )
                    )
                ELSE je.value
            END AS rewritten
        FROM json_each(model_backends.thinking_level_map) AS je
        ORDER BY je.key
    )
)
WHERE json_valid(thinking_level_map)
  AND json_type(thinking_level_map) = 'array'
  AND EXISTS (
    SELECT 1
    FROM json_each(model_backends.thinking_level_map) AS je
    WHERE json_extract(je.value, '$.control.type') IN ('disabled', 'enabled', 'budget')
);
