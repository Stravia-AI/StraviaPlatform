-- Convert only known preset identities. Unidentified legacy values remain
-- visible for administrator repair rather than being mapped to an unrelated
-- Provider Catalog scope.
UPDATE providers
SET
    models_source = 'catalog',
    preset_key = CASE
        WHEN preset_key IS NULL OR trim(preset_key) = ''
            THEN substr(models_source, length('ai://models.dev/') + 1)
        ELSE preset_key
    END
WHERE models_source GLOB 'ai://models.dev/*'
  AND substr(models_source, length('ai://models.dev/') + 1) IN (
      'openai', 'anthropic', 'google', 'vertexai', 'xai', 'deepseek',
      'moonshotai', 'minimax', 'zhipuai', 'zai', 'nvidia', 'openrouter', 'ollama'
  )
  AND (
      preset_key IS NULL
      OR trim(preset_key) = ''
      OR preset_key = substr(models_source, length('ai://models.dev/') + 1)
  );

UPDATE providers
SET
    models_source = 'catalog',
    preset_key = CASE
        WHEN preset_key IS NOT NULL AND trim(preset_key) <> '' THEN preset_key
        ELSE vendor
    END
WHERE models_source = 'ai://models.dev'
  AND (
      preset_key IN (
          'openai', 'anthropic', 'google', 'vertexai', 'xai', 'deepseek',
          'moonshotai', 'minimax', 'zhipuai', 'zai', 'nvidia', 'openrouter', 'ollama'
      )
      OR (
          (preset_key IS NULL OR trim(preset_key) = '')
          AND vendor IN (
              'openai', 'anthropic', 'google', 'vertexai', 'xai', 'deepseek',
              'moonshotai', 'minimax', 'zhipuai', 'zai', 'nvidia', 'openrouter', 'ollama'
          )
      )
  );
-- Convert only known preset identities. Unidentified legacy values remain
