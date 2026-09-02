ALTER TABLE models RENAME COLUMN name TO model_id;
ALTER TABLE models ADD COLUMN display_name TEXT;

DROP INDEX idx_models_route_id;
CREATE UNIQUE INDEX idx_models_route_id ON models(model_id);
