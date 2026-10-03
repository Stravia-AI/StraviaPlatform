-- 无模型的旧 Target 无法推导真实模型，直接删除并保留所属 Route。
DELETE FROM model_backends WHERE model IS NULL;
ALTER TABLE model_backends ALTER COLUMN model SET NOT NULL;
ALTER TABLE model_backends DROP CONSTRAINT model_backends_model_nonblank;
ALTER TABLE model_backends ADD CONSTRAINT model_backends_model_nonblank CHECK (btrim(model) <> '');

-- RPM 目的地同样必须包含模型；保留其他配置和有效成员的顺序。
UPDATE settings
SET value = jsonb_set(value::jsonb, '{destinations}', COALESCE((
    SELECT jsonb_agg(destination.value ORDER BY destination.ordinality)
    FROM jsonb_array_elements(settings.value::jsonb->'destinations')
         WITH ORDINALITY AS destination(value, ordinality)
    WHERE destination.value->>'model' IS NOT NULL
), '[]'::jsonb))::text
WHERE name = 'rpm_admission' AND jsonb_typeof(value::jsonb->'destinations') = 'array';
