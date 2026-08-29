-- Store Vendor-declared credentials separately from the legacy API-key mirror.
ALTER TABLE providers ADD COLUMN adapter_credentials TEXT NOT NULL DEFAULT '{}';

-- Catalog identities are discovery keys. Runtime Vendor identities are keyed by npm package.
UPDATE providers
SET vendor = CASE
        WHEN vendor IN (
            'aihubmix'
        ) THEN 'aihubmix'
        WHEN vendor IN (
            'amazon-bedrock'
        ) THEN 'amazon-bedrock'
        WHEN vendor IN (
            'anthropic',
            'freemodel',
            'kimi-for-coding',
            'minimax',
            'minimax-cn',
            'minimax-cn-coding-plan',
            'minimax-coding-plan',
            'subconscious',
            'thinkingmachines'
        ) THEN 'anthropic'
        WHEN vendor IN (
            'azure',
            'azure-cognitive-services'
        ) THEN 'azure'
        WHEN vendor IN (
            'cerebras'
        ) THEN 'cerebras'
        WHEN vendor IN (
            'cloudflare-ai-gateway'
        ) THEN 'cloudflare-ai-gateway'
        WHEN vendor IN (
            'cohere'
        ) THEN 'cohere'
        WHEN vendor IN (
            'deepinfra'
        ) THEN 'deepinfra'
        WHEN vendor IN (
            'vercel'
        ) THEN 'gateway'
        WHEN vendor IN (
            'gitlab'
        ) THEN 'gitlab'
        WHEN vendor IN (
            'google'
        ) THEN 'google'
        WHEN vendor IN (
            'google-vertex'
        ) THEN 'google-vertex'
        WHEN vendor IN (
            'google-vertex-anthropic'
        ) THEN 'google-vertex-anthropic'
        WHEN vendor IN (
            'groq'
        ) THEN 'groq'
        WHEN vendor IN (
            'merge-gateway'
        ) THEN 'merge-gateway'
        WHEN vendor IN (
            'mistral'
        ) THEN 'mistral'
        WHEN vendor IN (
            'meta',
            'openai',
            'perplexity-agent',
            'vivgrid'
        ) THEN 'openai'
        WHEN vendor IN (
            '302ai',
            'abacus',
            'abliteration-ai',
            'ai-router',
            'aiand',
            'aki-io',
            'alibaba',
            'alibaba-cn',
            'alibaba-coding-plan',
            'alibaba-coding-plan-cn',
            'alibaba-token-plan',
            'alibaba-token-plan-cn',
            'ambient',
            'amd',
            'anyapi',
            'arcee',
            'atomic-chat',
            'auriko',
            'bailing',
            'baseten',
            'berget',
            'blueclaw',
            'chutes',
            'clarifai',
            'claudinio',
            'cline-pass',
            'cloudferro-sherlock',
            'cloudflare-workers-ai',
            'coralbricks',
            'cortecs',
            'crof',
            'crossmodel',
            'crusoe',
            'daoxe',
            'databricks',
            'deepseek',
            'digitalocean',
            'dinference',
            'drun',
            'ebcloud',
            'echo',
            'edenai',
            'empiriolabs',
            'evroc',
            'fastrouter',
            'fireworks-ai',
            'friendli',
            'frogbot',
            'github-copilot',
            'gmicloud',
            'greenpt',
            'helicone',
            'hetzner',
            'hpc-ai',
            'huggingface',
            'hyper',
            'iflowcn',
            'impossibl',
            'inception',
            'inceptron',
            'inference',
            'inferx',
            'infomaniak',
            'io-net',
            'jalapeno',
            'jiekou',
            'kenari',
            'kilo',
            'kosmik',
            'kuae-cloud-coding-plan',
            'lilac',
            'llama',
            'llmgateway',
            'llmtr',
            'lmstudio',
            'longcat',
            'lucidquery',
            'lynkr',
            'meganova',
            'mixlayer',
            'moark',
            'modal',
            'model-oracle-ai',
            'modelis',
            'modelscope',
            'moonshotai',
            'moonshotai-cn',
            'morph',
            'nano-gpt',
            'nearai',
            'nebius',
            'neon',
            'neuralwatt',
            'nova',
            'novita-ai',
            'nvidia',
            'ofox',
            'ollama-cloud',
            'opencode',
            'opencode-go',
            'orcarouter',
            'ovhcloud',
            'pioneer',
            'poe',
            'poolside',
            'privatemode-ai',
            'qihang-ai',
            'qiniu-ai',
            'regolo-ai',
            'requesty',
            'routing-run',
            'runinfra',
            'sakana',
            'sarvam',
            'scaleway',
            'scnet-token-plan',
            'scx-ai',
            'siliconflow',
            'siliconflow-cn',
            'snowflake-cortex',
            'stackit',
            'stepfun',
            'stepfun-ai',
            'stepfun-ai-step-plan',
            'stepfun-step-plan',
            'submodel',
            'synthetic',
            'tencent-coding-plan',
            'tencent-token-plan',
            'tencent-tokenhub',
            'tensorx',
            'the-grid-ai',
            'tinfoil',
            'trustedrouter',
            'umans-ai',
            'umans-ai-coding-plan',
            'unorouter',
            'upstage',
            'vultr',
            'wafer.ai',
            'wandb',
            'xiaomi',
            'xiaomi-token-plan-ams',
            'xiaomi-token-plan-cn',
            'xiaomi-token-plan-sgp',
            'xpersona',
            'zai',
            'zai-coding-plan',
            'zeldoc',
            'zenifra',
            'zenmux',
            'zhipuai',
            'zhipuai-coding-plan'
        ) THEN 'openai-compatible'
        WHEN vendor IN (
            'openrouter'
        ) THEN 'openrouter'
        WHEN vendor IN (
            'perplexity'
        ) THEN 'perplexity'
        WHEN vendor IN (
            'qvac'
        ) THEN 'qvac'
        WHEN vendor IN (
            'salad-cloud'
        ) THEN 'salad-cloud'
        WHEN vendor IN (
            'sap-ai-core'
        ) THEN 'sap-ai-core'
        WHEN vendor IN (
            'togetherai'
        ) THEN 'togetherai'
        WHEN vendor IN (
            'venice'
        ) THEN 'venice'
        WHEN vendor IN (
            'v0'
        ) THEN 'vercel'
        WHEN vendor IN (
            'watsonx'
        ) THEN 'watsonx'
        WHEN vendor IN (
            'xai'
        ) THEN 'xai'
        WHEN vendor = 'vertexai' THEN 'google-vertex'
        ELSE vendor
    END
WHERE vendor IN (
        '302ai',
        'abacus',
        'abliteration-ai',
        'ai-router',
        'aiand',
        'aihubmix',
        'aki-io',
        'alibaba',
        'alibaba-cn',
        'alibaba-coding-plan',
        'alibaba-coding-plan-cn',
        'alibaba-token-plan',
        'alibaba-token-plan-cn',
        'amazon-bedrock',
        'ambient',
        'amd',
        'anthropic',
        'anyapi',
        'arcee',
        'atomic-chat',
        'auriko',
        'azure',
        'azure-cognitive-services',
        'bailing',
        'baseten',
        'berget',
        'blueclaw',
        'cerebras',
        'chutes',
        'clarifai',
        'claudinio',
        'cline-pass',
        'cloudferro-sherlock',
        'cloudflare-ai-gateway',
        'cloudflare-workers-ai',
        'cohere',
        'coralbricks',
        'cortecs',
        'crof',
        'crossmodel',
        'crusoe',
        'daoxe',
        'databricks',
        'deepinfra',
        'deepseek',
        'digitalocean',
        'dinference',
        'drun',
        'ebcloud',
        'echo',
        'edenai',
        'empiriolabs',
        'evroc',
        'fastrouter',
        'fireworks-ai',
        'freemodel',
        'friendli',
        'frogbot',
        'github-copilot',
        'gitlab',
        'gmicloud',
        'google',
        'google-vertex',
        'google-vertex-anthropic',
        'greenpt',
        'groq',
        'helicone',
        'hetzner',
        'hpc-ai',
        'huggingface',
        'hyper',
        'iflowcn',
        'impossibl',
        'inception',
        'inceptron',
        'inference',
        'inferx',
        'infomaniak',
        'io-net',
        'jalapeno',
        'jiekou',
        'kenari',
        'kilo',
        'kimi-for-coding',
        'kosmik',
        'kuae-cloud-coding-plan',
        'lilac',
        'llama',
        'llmgateway',
        'llmtr',
        'lmstudio',
        'longcat',
        'lucidquery',
        'lynkr',
        'meganova',
        'merge-gateway',
        'meta',
        'minimax',
        'minimax-cn',
        'minimax-cn-coding-plan',
        'minimax-coding-plan',
        'mistral',
        'mixlayer',
        'moark',
        'modal',
        'model-oracle-ai',
        'modelis',
        'modelscope',
        'moonshotai',
        'moonshotai-cn',
        'morph',
        'nano-gpt',
        'nearai',
        'nebius',
        'neon',
        'neuralwatt',
        'nova',
        'novita-ai',
        'nvidia',
        'ofox',
        'ollama-cloud',
        'openai',
        'opencode',
        'opencode-go',
        'openrouter',
        'orcarouter',
        'ovhcloud',
        'perplexity',
        'perplexity-agent',
        'pioneer',
        'poe',
        'poolside',
        'privatemode-ai',
        'qihang-ai',
        'qiniu-ai',
        'qvac',
        'regolo-ai',
        'requesty',
        'routing-run',
        'runinfra',
        'sakana',
        'salad-cloud',
        'sap-ai-core',
        'sarvam',
        'scaleway',
        'scnet-token-plan',
        'scx-ai',
        'siliconflow',
        'siliconflow-cn',
        'snowflake-cortex',
        'stackit',
        'stepfun',
        'stepfun-ai',
        'stepfun-ai-step-plan',
        'stepfun-step-plan',
        'subconscious',
        'submodel',
        'synthetic',
        'tencent-coding-plan',
        'tencent-token-plan',
        'tencent-tokenhub',
        'tensorx',
        'the-grid-ai',
        'thinkingmachines',
        'tinfoil',
        'togetherai',
        'trustedrouter',
        'umans-ai',
        'umans-ai-coding-plan',
        'unorouter',
        'upstage',
        'v0',
        'venice',
        'vercel',
        'vivgrid',
        'vultr',
        'wafer.ai',
        'wandb',
        'watsonx',
        'xai',
        'xiaomi',
        'xiaomi-token-plan-ams',
        'xiaomi-token-plan-cn',
        'xiaomi-token-plan-sgp',
        'xpersona',
        'zai',
        'zai-coding-plan',
        'zeldoc',
        'zenifra',
        'zenmux',
        'zhipuai',
        'zhipuai-coding-plan'
    )
    OR vendor = 'vertexai';

-- Preserve every existing secret under the Vendor field it now represents.
UPDATE providers
SET adapter_credentials = CASE
    WHEN (vendor IN ('google-vertex', 'google-vertex-anthropic')
          OR preset_key IN ('vertexai', 'google-vertex', 'google-vertex-anthropic'))
         AND ltrim(api_key) LIKE '{%' THEN json_object('credentials', api_key)
    WHEN trim(api_key) <> '' THEN json_object('apiKey', api_key)
    ELSE '{}'
END;
