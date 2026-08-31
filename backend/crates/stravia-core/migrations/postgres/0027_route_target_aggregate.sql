INSERT INTO model_backends (id, model_id, provider_id, model, weight, priority)
SELECT md5(random()::text || clock_timestamp()::text || models.id),
       models.id,
       models.target_provider,
       models.target_model,
       100,
       1
  FROM models
 WHERE NOT EXISTS (
           SELECT 1 FROM model_backends WHERE model_id = models.id
       );

ALTER TABLE models DROP COLUMN target_provider;
ALTER TABLE models DROP COLUMN target_model;

CREATE UNIQUE INDEX idx_models_route_id ON models(name);
