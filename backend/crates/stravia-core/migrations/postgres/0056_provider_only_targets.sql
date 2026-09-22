ALTER TABLE model_backends ALTER COLUMN model DROP NOT NULL;
ALTER TABLE model_backends
ADD CONSTRAINT model_backends_model_nonblank CHECK (model IS NULL OR btrim(model) <> '');
