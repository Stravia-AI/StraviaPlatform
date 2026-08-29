ALTER TABLE models
ADD COLUMN supported_thinking_levels JSONB NOT NULL DEFAULT '[]'::jsonb;

ALTER TABLE model_backends
ADD COLUMN thinking_level_map JSONB NOT NULL DEFAULT '[{"level":"off","control":{"type":"hidden"},"source":"generated"},{"level":"minimal","control":{"type":"hidden"},"source":"generated"},{"level":"low","control":{"type":"hidden"},"source":"generated"},{"level":"medium","control":{"type":"hidden"},"source":"generated"},{"level":"high","control":{"type":"hidden"},"source":"generated"},{"level":"xhigh","control":{"type":"hidden"},"source":"generated"},{"level":"max","control":{"type":"hidden"},"source":"generated"}]'::jsonb;
