ALTER TABLE agent_definition_configs
ADD COLUMN thinking_level TEXT
CHECK (thinking_level IN ('off', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'));
