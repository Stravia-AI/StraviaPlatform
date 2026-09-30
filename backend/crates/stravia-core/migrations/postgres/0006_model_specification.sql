-- Preserve Provider/Route/Target identity and all existing thinking maps.
UPDATE provider_models AS model SET metadata_json = jsonb_set(metadata_json, '{reasoning_efforts}',
    COALESCE((SELECT jsonb_agg(value ORDER BY origin, position, effort_position) FROM (
        SELECT DISTINCT ON (value) value, origin, position, effort_position FROM (
        SELECT effort.value, 0 AS origin, effort.ordinality AS position, 0::bigint AS effort_position
        FROM jsonb_array_elements(CASE WHEN jsonb_typeof(model.metadata_json->'reasoning_efforts') = 'array'
            THEN model.metadata_json->'reasoning_efforts' ELSE '[]'::jsonb END) WITH ORDINALITY AS effort(value, ordinality)
        WHERE jsonb_typeof(effort.value) = 'string' AND btrim(effort.value #>> '{}') <> '' AND lower(btrim(effort.value #>> '{}')) NOT IN ('default', 'null')
        UNION ALL
        SELECT effort.value, 1, option.ordinality, effort.ordinality
        FROM jsonb_array_elements(CASE WHEN jsonb_typeof(model.metadata_json->'reasoning_options') = 'array'
            THEN model.metadata_json->'reasoning_options' ELSE '[]'::jsonb END) WITH ORDINALITY AS option(value, ordinality),
            jsonb_array_elements(CASE WHEN option.value->>'type' = 'effort' AND jsonb_typeof(option.value->'values') = 'array'
                THEN option.value->'values' ELSE '[]'::jsonb END) WITH ORDINALITY AS effort(value, ordinality)
        WHERE COALESCE(jsonb_typeof(model.metadata_json->'reasoning_efforts'), '') <> 'array'
            AND jsonb_typeof(effort.value) = 'string' AND btrim(effort.value #>> '{}') <> ''
            AND lower(btrim(effort.value #>> '{}')) NOT IN ('default', 'null')
        ) AS candidates ORDER BY value, origin, position, effort_position
    ) AS efforts), '[]'::jsonb));
UPDATE provider_models SET metadata_json = metadata_json
    - 'attachment'
    - 'reasoning'
    - 'tool_call'
    - 'structured_output'
    - 'temperature'
    - 'interleaved'
    - 'reasoning_options'
    - 'reasoning_levels'
    - 'thinking_toggle'
    #- '{limit,input}' #- '{limit,output}';
UPDATE provider_models SET metadata_json = metadata_json - 'reasoning_efforts'
WHERE metadata_json->'reasoning_efforts' = '[]'::jsonb;
ALTER TABLE provider_models DROP COLUMN attachment;
ALTER TABLE provider_models DROP COLUMN reasoning;
ALTER TABLE provider_models DROP COLUMN tool_call;
ALTER TABLE provider_models DROP COLUMN structured_output;
ALTER TABLE provider_models DROP COLUMN temperature;
ALTER TABLE provider_models DROP COLUMN limit_input;
ALTER TABLE provider_models DROP COLUMN limit_output;
