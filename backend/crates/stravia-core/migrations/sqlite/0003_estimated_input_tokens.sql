ALTER TABLE model_turn_observations
    ADD COLUMN estimated_input_tokens INTEGER
        CHECK (estimated_input_tokens IS NULL OR
               (typeof(estimated_input_tokens) = 'integer' AND estimated_input_tokens >= 0));
