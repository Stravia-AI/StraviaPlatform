-- 旧观察没有新增凭据元数据；标记缺失而不从秘密表或历史正文补造发现。
UPDATE interaction_observations SET observation_gap = 1;
