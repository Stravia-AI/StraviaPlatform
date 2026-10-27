-- 发现列表只聚合凭据发现事件，避免扫描无关观测；同时支持当前页按交互和发现时间读取载荷。
CREATE INDEX observation_events_credential_discovery_idx
    ON observation_events(interaction_id, occurred_at DESC, expires_at)
    WHERE kind = 'credential_mappings_created';
