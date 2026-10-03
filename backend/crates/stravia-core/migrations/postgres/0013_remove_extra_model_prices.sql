-- Remove retired prices without changing snapshot identity, revision, or model capabilities.
UPDATE provider_models SET metadata_json = metadata_json
    #- '{cost,reasoning}' #- '{cost,input_audio}' #- '{cost,output_audio}'
    #- '{cost,context_over_200k,reasoning}' #- '{cost,context_over_200k,input_audio}'
    #- '{cost,context_over_200k,output_audio}';
UPDATE provider_models AS model SET metadata_json = jsonb_set(metadata_json, '{cost,tiers}',
    COALESCE((SELECT jsonb_agg(value - 'reasoning' - 'input_audio' - 'output_audio' ORDER BY position)
        FROM jsonb_array_elements(model.metadata_json #> '{cost,tiers}') WITH ORDINALITY AS tier(value, position)), '[]'::jsonb))
WHERE jsonb_typeof(metadata_json #> '{cost,tiers}') = 'array';
ALTER TABLE provider_models DROP COLUMN cost_reasoning;
ALTER TABLE provider_models DROP COLUMN cost_input_audio;
ALTER TABLE provider_models DROP COLUMN cost_output_audio;
ALTER TABLE provider_model_cost_rules DROP COLUMN cost_reasoning;
ALTER TABLE provider_model_cost_rules DROP COLUMN cost_input_audio;
ALTER TABLE provider_model_cost_rules DROP COLUMN cost_output_audio;
