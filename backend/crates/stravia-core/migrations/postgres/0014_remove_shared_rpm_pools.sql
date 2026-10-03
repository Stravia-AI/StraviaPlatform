-- 删除共享池额度及关联；保留目的地自身限额，原池成员恢复不限。
UPDATE settings
SET value = (jsonb_set(value::jsonb, '{destinations}', COALESCE((
    SELECT jsonb_agg(destination.value - 'rpm_pool_id' ORDER BY destination.ordinality)
    FROM jsonb_array_elements(settings.value::jsonb->'destinations')
         WITH ORDINALITY AS destination(value, ordinality)
), '[]'::jsonb)) - 'pools')::text
WHERE name = 'rpm_admission';
