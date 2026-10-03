-- 删除共享池额度及关联；保留目的地自身限额，原池成员恢复不限。
UPDATE settings
SET value = json_remove(json_set(value, '$.destinations', json(COALESCE((
    SELECT json_group_array(json(json_remove(destination.value, '$.rpm_pool_id')))
    FROM json_each(settings.value, '$.destinations') AS destination
), '[]'))), '$.pools')
WHERE name = 'rpm_admission';
