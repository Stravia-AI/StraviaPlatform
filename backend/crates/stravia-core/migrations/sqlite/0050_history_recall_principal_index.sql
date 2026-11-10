-- 加速按 principal 筛选并按最近活动排序的观测查询。
CREATE INDEX interaction_observations_principal_activity_idx
    ON interaction_observations(principal, last_active_at DESC, id);
