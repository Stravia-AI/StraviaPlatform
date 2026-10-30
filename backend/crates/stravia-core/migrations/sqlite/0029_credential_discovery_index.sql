-- 发现列表只聚合两类凭据发现事件；部分索引以 kind 前导，新增/复用筛选
-- 都能先按 kind 定位交互，再读取当前页发现时间与过期上界，避免全表扫描。
CREATE INDEX observation_events_credential_discovery_idx
    ON observation_events(kind, interaction_id, occurred_at DESC, expires_at)
    WHERE kind IN ('credential_mappings_created','credential_mappings_reused');
