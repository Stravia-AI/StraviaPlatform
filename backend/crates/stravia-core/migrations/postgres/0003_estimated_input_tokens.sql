ALTER TABLE model_turn_observations
    ADD COLUMN estimated_input_tokens bigint,
    ADD CONSTRAINT model_turn_observations_estimated_input_tokens_check
        CHECK (estimated_input_tokens >= 0);
