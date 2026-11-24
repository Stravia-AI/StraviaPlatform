-- 收敛 model_backends.thinking_level_map：将已淘汰的 disabled / enabled / budget 控制
-- 迁移为 effort，保持现有 effort 与 hidden 行及 source 标记不变。
UPDATE model_backends
SET thinking_level_map = (
    SELECT jsonb_agg(
        CASE
            WHEN elem -> 'control' ->> 'type' IN ('disabled', 'enabled', 'budget') THEN
                jsonb_set(
                    elem,
                    '{control}',
                    jsonb_build_object(
                        'type', 'effort',
                        'value',
                        CASE
                            WHEN elem -> 'control' ->> 'type' = 'disabled'
                                OR (
                                    elem -> 'control' ->> 'type' = 'budget'
                                    AND COALESCE(
                                        CASE
                                            WHEN jsonb_typeof(elem -> 'control' -> 'value') = 'number'
                                            THEN (elem -> 'control' ->> 'value')::numeric
                                            ELSE 0
                                        END,
                                        0
                                    ) <= 0
                                )
                            THEN 'none'
                            WHEN lower(COALESCE(elem ->> 'level', '')) = 'off'
                            THEN 'none'
                            ELSE COALESCE(NULLIF(btrim(elem ->> 'level'), ''), 'high')
                        END
                    ),
                    true
                )
            ELSE elem
        END
        ORDER BY ord
    )
    FROM jsonb_array_elements(model_backends.thinking_level_map) WITH ORDINALITY AS t(elem, ord)
)
WHERE thinking_level_map IS NOT NULL
  AND jsonb_typeof(thinking_level_map) = 'array'
  AND EXISTS (
      SELECT 1
      FROM jsonb_array_elements(model_backends.thinking_level_map) AS elem
      WHERE elem -> 'control' ->> 'type' IN ('disabled', 'enabled', 'budget')
  );
