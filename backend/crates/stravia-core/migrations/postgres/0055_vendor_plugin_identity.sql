-- 只在升级时冻结历史连接身份；运行时不得根据协议或 catalog 猜测 Vendor。
-- 无法确定的旧连接保持原值，以不可用状态显式交给管理员修复。
UPDATE providers
SET vendor = CASE lower(trim(vendor))
    WHEN 'vertexai' THEN 'google-vertex'
    WHEN 'commandcode' THEN 'command-code'
    ELSE lower(trim(vendor))
END
WHERE lower(trim(vendor)) IN ('aihubmix', 'amazon-bedrock', 'anthropic', 'azure', 'cerebras', 'cloudflare-ai-gateway', 'cohere', 'command-code', 'custom', 'deepinfra', 'deepseek', 'devin', 'gateway', 'gitlab', 'google', 'google-vertex', 'google-vertex-anthropic', 'groq', 'merge-gateway', 'mistral', 'ollama', 'openai', 'openai-compatible', 'openrouter', 'perplexity', 'qvac', 'salad-cloud', 'sap-ai-core', 'togetherai', 'venice', 'vercel', 'watsonx', 'xai', 'openai-codex', 'xai-grok', 'protocol-gemini', 'protocol-openai-chat-completions', 'protocol-open-responses', 'protocol-anthropic-messages', 'github-copilot', 'kimi-for-coding', 'nano-gpt', 'zai-coding-plan', 'zhipuai-coding-plan', 'minimax-coding-plan', 'minimax-cn-coding-plan', 'wafer.ai', 'opencode-go', 'crof', 'neuralwatt', 'baseten', 'lilac', 'nvidia', 'opencode', 'xiaomi', 'xiaomi-token-plan-sgp', 'xiaomi-token-plan-cn', 'xiaomi-token-plan-ams', 'zai', 'zhipuai', 'alibaba', 'alibaba-cn', 'alibaba-coding-plan', 'alibaba-coding-plan-cn', 'alibaba-token-plan', 'alibaba-token-plan-cn', 'vertexai', 'commandcode');

UPDATE providers
SET vendor = CASE
    WHEN lower(trim(protocol)) IN ('openai-compatible', 'openai-compat', 'openai', 'openai/chat/v1', 'openai/embeddings/v1', 'openai-chat', 'openai-chat-completions', 'openai-embeddings', 'embeddings', 'openai-compatible/chat-completions/v1', 'openai-compatible/embeddings/v1', 'open-responses', 'open-responses/responses/2026-04-24') THEN 'openai'
    WHEN lower(trim(protocol)) IN ('anthropic-messages', 'anthropic-msgs', 'anthropic', 'claude', 'anthropic/messages/2023-06-01', 'anthropic-messages/messages/2023-06-01') THEN 'anthropic'
    WHEN lower(trim(protocol)) IN ('google-gemini', 'google-genai', 'google-generative-ai', 'google', 'gemini', 'google-generate', 'google-generate-content', 'google/generate/v1beta', 'google-gemini/generate-content/v1beta') THEN 'google'
    WHEN lower(trim(protocol)) IN ('bedrock-converse', 'bedrock', 'bedrock-converse/converse/v1') THEN 'amazon-bedrock'
    WHEN lower(trim(protocol)) IN ('cohere-chat', 'cohere', 'cohere-chat/chat/v2') THEN 'cohere'
    WHEN lower(trim(protocol)) IN ('watsonx-text-chat', 'watsonx', 'watsonx-text-chat/chat/v1') THEN 'watsonx'
    WHEN lower(trim(protocol)) IN ('gateway-language-model', 'gateway', 'gateway-language-model/language-model/v4') THEN 'gateway'
    WHEN lower(trim(protocol)) IN ('command-code', 'command-code-generate', 'commandcode', 'command-code/generate/v1') THEN 'command-code'
    WHEN lower(trim(protocol)) IN ('devin-connect', 'devin', 'windsurf-connect', 'devin-connect/get-chat-message/v1') THEN 'devin'
    ELSE vendor
END
WHERE vendor IS NULL OR trim(vendor) = '';

-- 这些 catalog 身份此前借通用适配器执行；现在由对应包统一提供原有能力。
UPDATE providers
SET vendor = lower(trim(preset_key))
WHERE (vendor IN ('openai', 'openai-compatible')
       AND lower(trim(preset_key)) IN ('deepseek', 'github-copilot', 'nano-gpt',
           'zai-coding-plan', 'zhipuai-coding-plan', 'wafer.ai', 'opencode-go', 'crof', 'neuralwatt', 'baseten', 'lilac', 'nvidia', 'opencode', 'xiaomi', 'xiaomi-token-plan-sgp', 'xiaomi-token-plan-cn', 'xiaomi-token-plan-ams', 'zai', 'zhipuai', 'alibaba', 'alibaba-cn', 'alibaba-coding-plan', 'alibaba-coding-plan-cn', 'alibaba-token-plan', 'alibaba-token-plan-cn'))
   OR (vendor = 'anthropic'
       AND lower(trim(preset_key)) IN ('kimi-for-coding', 'minimax-coding-plan', 'minimax-cn-coding-plan'));

UPDATE providers
SET channel = CASE
    WHEN vendor IN ('openai', 'openai-codex') AND EXISTS (
        SELECT 1 FROM provider_oauth_credentials c
        WHERE c.provider_id = providers.id AND c.driver_key IN ('codex', 'openai-codex')
    ) THEN 'codex'
    WHEN vendor = 'openai-codex' THEN 'codex'
    WHEN vendor = 'anthropic' AND EXISTS (
        SELECT 1 FROM provider_oauth_credentials c
        WHERE c.provider_id = providers.id AND c.driver_key = 'claude-code'
    ) THEN 'claude-code'
    WHEN vendor IN ('xai', 'xai-grok') AND EXISTS (
        SELECT 1 FROM provider_oauth_credentials c
        WHERE c.provider_id = providers.id AND c.driver_key IN ('grok', 'xai-grok')
    ) THEN 'grok'
    WHEN vendor = 'xai-grok' THEN 'grok'
    WHEN vendor = 'devin' THEN 'devin'
    WHEN vendor = 'google-vertex' AND lower(trim(protocol)) IN ('openai-compatible', 'openai-compat', 'openai', 'openai/chat/v1', 'openai/embeddings/v1', 'openai-chat', 'openai-chat-completions', 'openai-embeddings', 'embeddings', 'openai-compatible/chat-completions/v1', 'openai-compatible/embeddings/v1', 'open-responses', 'open-responses/responses/2026-04-24') THEN 'openai'
    WHEN vendor = 'google-vertex' AND lower(trim(protocol)) IN ('google-gemini', 'google-genai', 'google-generative-ai', 'google', 'gemini', 'google-generate', 'google-generate-content', 'google/generate/v1beta', 'google-gemini/generate-content/v1beta') THEN 'native'
    WHEN vendor != 'google-vertex' THEN 'default'
    ELSE channel
END
WHERE (channel IS NULL OR trim(channel) = '')
  AND vendor IN ('aihubmix', 'amazon-bedrock', 'anthropic', 'azure', 'cerebras', 'cloudflare-ai-gateway', 'cohere', 'command-code', 'custom', 'deepinfra', 'deepseek', 'devin', 'gateway', 'gitlab', 'google', 'google-vertex', 'google-vertex-anthropic', 'groq', 'merge-gateway', 'mistral', 'ollama', 'openai', 'openai-compatible', 'openrouter', 'perplexity', 'qvac', 'salad-cloud', 'sap-ai-core', 'togetherai', 'venice', 'vercel', 'watsonx', 'xai', 'openai-codex', 'xai-grok', 'protocol-gemini', 'protocol-openai-chat-completions', 'protocol-open-responses', 'protocol-anthropic-messages', 'github-copilot', 'kimi-for-coding', 'nano-gpt', 'zai-coding-plan', 'zhipuai-coding-plan', 'minimax-coding-plan', 'minimax-cn-coding-plan', 'wafer.ai', 'opencode-go', 'crof', 'neuralwatt', 'baseten', 'lilac', 'nvidia', 'opencode', 'xiaomi', 'xiaomi-token-plan-sgp', 'xiaomi-token-plan-cn', 'xiaomi-token-plan-ams', 'zai', 'zhipuai', 'alibaba', 'alibaba-cn', 'alibaba-coding-plan', 'alibaba-coding-plan-cn', 'alibaba-token-plan', 'alibaba-token-plan-cn');

-- Codex and Grok are independent provider profiles. Keep the connection UUID,
-- routes, history, and credential payloads while moving only identity metadata.
UPDATE providers
SET vendor = CASE
    WHEN vendor = 'openai' AND lower(trim(channel)) = 'codex' THEN 'openai-codex'
    WHEN vendor = 'xai' AND lower(trim(channel)) = 'grok' THEN 'xai-grok'
    ELSE vendor
END,
    channel = lower(trim(channel))
WHERE (vendor = 'openai' AND lower(trim(channel)) = 'codex')
   OR (vendor = 'xai' AND lower(trim(channel)) = 'grok');

-- Catalog-backed dedicated profiles reuse their supplier's existing model
-- scope. Preserve any different explicit catalog source and every static list.
UPDATE providers
SET preset_key = CASE
    WHEN vendor = 'openai-codex' THEN 'openai'
    WHEN vendor = 'xai-grok' THEN 'xai'
    ELSE preset_key
END
WHERE lower(trim(models_source)) = 'catalog'
  AND (
      (vendor = 'openai-codex' AND (preset_key IS NULL OR trim(preset_key) = '' OR lower(trim(preset_key)) = 'openai-codex'))
      OR
      (vendor = 'xai-grok' AND (preset_key IS NULL OR trim(preset_key) = '' OR lower(trim(preset_key)) = 'xai-grok'))
  );

UPDATE provider_oauth_credentials
SET driver_key = 'openai-codex'
WHERE driver_key IN ('openai', 'codex')
  AND provider_id IN (SELECT id FROM providers WHERE vendor = 'openai-codex');

UPDATE provider_oauth_credentials
SET driver_key = 'xai-grok'
WHERE driver_key IN ('xai', 'grok')
  AND provider_id IN (SELECT id FROM providers WHERE vendor = 'xai-grok');
