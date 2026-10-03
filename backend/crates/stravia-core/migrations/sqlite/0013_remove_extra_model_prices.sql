-- Remove retired prices without changing snapshot identity, revision, or model capabilities.
UPDATE provider_models SET metadata_json = json_remove(metadata_json,
    '$.cost.reasoning', '$.cost.input_audio', '$.cost.output_audio',
    '$.cost.context_over_200k.reasoning', '$.cost.context_over_200k.input_audio',
    '$.cost.context_over_200k.output_audio');
-- Edit each tier in place: reconstructing JSON numeric values through json_each
-- would round arbitrary-precision prices through SQLite REAL.
WITH RECURSIVE cleaned(provider_id, model_id, position, tier_count, metadata) AS (
    SELECT provider_id, model_id, 0, json_array_length(metadata_json, '$.cost.tiers'), metadata_json
    FROM provider_models WHERE json_type(metadata_json, '$.cost.tiers') = 'array'
    UNION ALL
    SELECT provider_id, model_id, position + 1, tier_count,
        json_remove(metadata,
            '$.cost.tiers[' || position || '].reasoning',
            '$.cost.tiers[' || position || '].input_audio',
            '$.cost.tiers[' || position || '].output_audio')
    FROM cleaned WHERE position < tier_count
)
UPDATE provider_models AS model SET metadata_json = (
    SELECT metadata FROM cleaned
    WHERE cleaned.provider_id = model.provider_id AND cleaned.model_id = model.model_id
        AND position = tier_count
) WHERE json_type(metadata_json, '$.cost.tiers') = 'array';
ALTER TABLE provider_models DROP COLUMN cost_reasoning;
ALTER TABLE provider_models DROP COLUMN cost_input_audio;
ALTER TABLE provider_models DROP COLUMN cost_output_audio;
ALTER TABLE provider_model_cost_rules DROP COLUMN cost_reasoning;
ALTER TABLE provider_model_cost_rules DROP COLUMN cost_input_audio;
ALTER TABLE provider_model_cost_rules DROP COLUMN cost_output_audio;
