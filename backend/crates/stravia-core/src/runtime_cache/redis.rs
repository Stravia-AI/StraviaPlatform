use std::{
    fmt::Write as _,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use serde::{Serialize, de::DeserializeOwned};

use super::{
    REDIS_BASE_OVERHEAD, REDIS_ENTRY_OVERHEAD,
    policy::{CLASS_COUNT, CacheClass, TOTAL_WEIGHT},
    warn,
};
use crate::performance::ResourceUsage;

pub(super) struct RedisCache {
    connection: ::redis::aio::MultiplexedConnection,
    keys: [String; 5],
    capacity_bytes: usize,
    get: ::redis::Script,
    put: ::redis::Script,
    remove: ::redis::Script,
    // 最近收到的原子快照；空闲 TTL 与乱序响应使它只能表示观察值，而非实时用量。
    observed_usage: ResourceUsage,
}

impl RedisCache {
    pub(super) async fn new(url: &str, capacity_bytes: usize) -> Result<Self> {
        let client = ::redis::Client::open(url)
            .map_err(|_| anyhow::anyhow!("Runtime cache Redis URL is invalid"))?;
        let config = ::redis::AsyncConnectionConfig::new()
            .set_connection_timeout(Some(Duration::from_secs(5)))
            .set_response_timeout(Some(Duration::from_secs(1)));
        let connection = client
            .get_multiplexed_async_connection_with_config(&config)
            .await
            .map_err(|_| anyhow::anyhow!("Runtime cache Redis connection failed"))?;
        let namespace = format!("stravia:runtime:v1:{}", uuid::Uuid::new_v4());
        // 两路后端使用同一份固定策略，Lua 不另行维护优先级和份额表。
        let mut header = String::with_capacity(256);
        let _ = writeln!(
            header,
            "local class_count = {CLASS_COUNT}\nlocal total_weight = {TOTAL_WEIGHT}"
        );
        for (name, values) in [
            (
                "priorities",
                CacheClass::ALL.map(|class| usize::from(class.priority())),
            ),
            ("shares", CacheClass::ALL.map(CacheClass::weight)),
        ] {
            let _ = write!(header, "local {name} = {{");
            for (index, value) in values.into_iter().enumerate() {
                if index > 0 {
                    header.push(',');
                }
                let _ = write!(header, "{value}");
            }
            header.push_str("}\n");
        }
        Ok(Self {
            connection,
            keys: ["values", "weights", "expiry", "order", "used"]
                .map(|suffix| format!("{namespace}:{suffix}")),
            capacity_bytes,
            get: ::redis::Script::new(&format!("{COMMON}\n{GET}")),
            put: ::redis::Script::new(&format!("{header}\n{COMMON}\n{PUT}")),
            remove: ::redis::Script::new(&format!("{COMMON}\n{REMOVE}")),
            observed_usage: ResourceUsage::new(
                "runtime_cache_remote_observed_logical",
                "remote_total",
            ),
        })
    }

    pub(super) fn admission_weight<T>(
        &self,
        domain: &str,
        key: &str,
        value_bytes: usize,
    ) -> Option<usize> {
        let prefix_bytes = domain.len().checked_ilog10().unwrap_or(0) as usize + 1;
        let field_bytes = prefix_bytes
            .checked_add(1)?
            .checked_add(domain.len())?
            .checked_add(key.len())?;
        let weight = value_bytes
            .checked_add(field_bytes.checked_mul(4)?)?
            .checked_add(REDIS_ENTRY_OVERHEAD)?;
        let budget = self.capacity_bytes.saturating_sub(REDIS_BASE_OVERHEAD);
        (weight <= budget && (self.capacity_bytes as u128) < (1u128 << 53)).then_some(weight)
    }

    pub(super) async fn get<T: DeserializeOwned + Send + Sync + 'static>(
        &self,
        domain: &'static str,
        key: &str,
    ) -> Option<Arc<T>> {
        let mut connection = self.connection.clone();
        let mut invocation = self.get.prepare_invoke();
        for key in &self.keys {
            invocation.key(key);
        }
        let result: ::redis::RedisResult<(Option<Vec<u8>>, u64, u64)> = invocation
            .arg(field(domain, key))
            .invoke_async(&mut connection)
            .await;
        let payload = match result {
            Ok((payload, items, bytes)) => {
                self.observe(items, bytes);
                payload?
            }
            Err(_) => {
                warn("get", domain);
                return None;
            }
        };
        match serde_json::from_slice(&payload) {
            Ok(value) => Some(Arc::new(value)),
            Err(_) => {
                warn("decode", domain);
                None
            }
        }
    }

    pub(super) async fn put<T: Serialize + Send + Sync + 'static>(
        &self,
        domain: &'static str,
        key: &str,
        value: Arc<T>,
        estimated_bytes: usize,
        ttl: Duration,
    ) {
        if ttl.is_zero()
            || self
                .admission_weight::<T>(domain, key, estimated_bytes)
                .is_none()
        {
            return;
        }
        let Some(deadline) = SystemTime::now().checked_add(ttl) else {
            return;
        };
        let Ok(deadline) = deadline.duration_since(UNIX_EPOCH) else {
            return;
        };
        let expiry = deadline.as_millis();
        if expiry > (1u128 << 53) - 1 {
            return;
        }
        let payload = match serde_json::to_vec(value.as_ref()) {
            Ok(payload) => payload,
            Err(_) => {
                warn("encode", domain);
                return;
            }
        };
        let Some(weight) =
            self.admission_weight::<T>(domain, key, payload.len().max(estimated_bytes))
        else {
            return;
        };
        let mut connection = self.connection.clone();
        let mut invocation = self.put.prepare_invoke();
        for key in &self.keys {
            invocation.key(key);
        }
        let result: ::redis::RedisResult<(i64, u64, u64)> = invocation
            .arg(field(domain, key))
            .arg(payload)
            .arg(weight)
            .arg(self.capacity_bytes.saturating_sub(REDIS_BASE_OVERHEAD))
            .arg(expiry as u64)
            .arg(CacheClass::for_domain(domain).index())
            .invoke_async(&mut connection)
            .await;
        match result {
            Ok((_, items, bytes)) => self.observe(items, bytes),
            Err(_) => warn("put", domain),
        }
    }

    pub(super) async fn remove(&self, domain: &'static str, key: &str) {
        let mut connection = self.connection.clone();
        let mut invocation = self.remove.prepare_invoke();
        for key in &self.keys {
            invocation.key(key);
        }
        let result: ::redis::RedisResult<(i64, u64, u64)> = invocation
            .arg(field(domain, key))
            .invoke_async(&mut connection)
            .await;
        match result {
            Ok((_, items, bytes)) => self.observe(items, bytes),
            Err(_) => warn("remove", domain),
        }
    }

    pub(super) async fn shutdown(&self) {
        let mut connection = self.connection.clone();
        let result: ::redis::RedisResult<usize> = ::redis::cmd("DEL")
            .arg(&self.keys)
            .query_async(&mut connection)
            .await;
        match result {
            Ok(_) => self.observe(0, 0),
            Err(_) => warn("shutdown", "runtime"),
        }
    }

    fn observe(&self, items: u64, bytes: u64) {
        self.observed_usage
            .set_observed(items, bytes, SystemTime::now());
    }
}

fn field(domain: &str, key: &str) -> String {
    format!("{}:{domain}{key}", domain.len())
}

// 全部 key 显式传入 KEYS；class:weight 元数据与总量 / 类别计数由 Lua 原子维护。
const COMMON: &str = r#"
local function metadata(field)
    local raw = redis.call('HGET', KEYS[2], field)
    if not raw then return nil, nil end
    local c, w = string.match(raw, '^(%d+):(%d+)$')
    if not c then error('invalid cache metadata') end
    return tonumber(c), tonumber(w)
end
local function drop(field)
    local c, w = metadata(field)
    if not c then return 0 end
    redis.call('HDEL', KEYS[1], field)
    redis.call('HDEL', KEYS[2], field)
    redis.call('ZREM', KEYS[3], field)
    redis.call('ZREM', KEYS[4], field)
    local decrement = '-' .. string.format('%.0f', w)
    redis.call('HINCRBY', KEYS[5], 'total', decrement)
    redis.call('HINCRBY', KEYS[5], tostring(c), decrement)
    return w
end
local function finish(result)
    if redis.call('ZCARD', KEYS[3]) == 0 then redis.call('DEL', unpack(KEYS)) end
    return {result, redis.call('HLEN', KEYS[1]), tonumber(redis.call('HGET', KEYS[5], 'total')) or 0}
end
local function now_ms()
    local t = redis.call('TIME')
    return tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)
end
"#;

const GET: &str = r#"
local expiry = tonumber(redis.call('ZSCORE', KEYS[3], ARGV[1]))
if not expiry then return finish(false) end
if expiry <= now_ms() then drop(ARGV[1]); return finish(false) end
return finish(redis.call('HGET', KEYS[1], ARGV[1]))
"#;

const REMOVE: &str = r#"
return finish(drop(ARGV[1]))
"#;

const PUT: &str = r#"
local function put()
    local weight, budget, expiry, incoming = tonumber(ARGV[3]), tonumber(ARGV[4]), tonumber(ARGV[5]), tonumber(ARGV[6]) + 1
    local now = now_ms()
    -- 各类 score 占用 2^50 的区间，七个类别仍保持整数精确；越界值不入缓存。
    if weight > budget or expiry <= now or expiry > 9007199254740991 or now >= 1125899906842624 then return 0 end
    -- 先按过期索引清理，工作量与过期条目数相关，不扫描整个缓存。
    while true do
        local expired = redis.call('ZRANGEBYSCORE', KEYS[3], '-inf', string.format('%.0f', now), 'LIMIT', 0, 1)[1]
        if not expired then break end
        if drop(expired) == 0 then error('missing expired metadata') end
    end
    local usage, quota = {}, {}
    local used = tonumber(redis.call('HGET', KEYS[5], 'total')) or 0
    for i = 1, class_count do
        usage[i] = tonumber(redis.call('HGET', KEYS[5], tostring(i - 1))) or 0
        quota[i] = math.floor(budget / total_weight) * shares[i] + math.floor((budget % total_weight) * shares[i] / total_weight)
    end
    local old_class, old_weight = metadata(ARGV[1])
    if old_class then
        usage[old_class + 1] = usage[old_class + 1] - old_weight
        used = used - old_weight
    end
    if used < 0 or used > budget then error('invalid cache usage') end
    local need = math.max(0, weight - (budget - used))
    -- 上界截到 need，避免近 2^53 的累计值失去整数精度。
    local freeable = math.min(need, usage[incoming])
    local higher = 0
    for i = 1, class_count do
        if i ~= incoming then
            local excess = math.max(0, usage[i] - quota[i])
            if priorities[i] >= priorities[incoming] then
                freeable = math.min(need, freeable + math.min(need - freeable, excess))
            else
                higher = math.min(need, higher + math.min(need - higher, excess))
            end
        end
    end
    freeable = math.min(need, freeable + math.min(need - freeable, quota[incoming], higher))
    if need > freeable then return 0 end
    -- 完成准入与对象规划后才修改活跃值，拒绝替换时保留旧值。
    local victims, cursors, ceilings = {}, {}, {}
    for i = 1, class_count do
        cursors[i] = string.format('%.0f', (i - 1) * 1125899906842624)
        ceilings[i] = string.format('%.0f', i * 1125899906842624 - 1)
    end
    while need > 0 do
        local donor, best_priority, best_excess = nil, -1, -1
        local reclaim = need <= math.max(0, quota[incoming] - usage[incoming])
        for i = 1, class_count do
            local excess = usage[i] - quota[i]
            if excess > 0 and (priorities[i] >= priorities[incoming] or reclaim) then
                if priorities[i] > best_priority or (priorities[i] == best_priority and excess > best_excess) then
                    donor, best_priority, best_excess = i, priorities[i], excess
                end
            end
        end
        if not donor and usage[incoming] > 0 then donor = incoming end
        if not donor then return 0 end
        local victim
        repeat
            -- score 唯一且递增；游标避免 LIMIT offset 反复扫描前缀导致二次方开销。
            local candidate = redis.call('ZRANGEBYSCORE', KEYS[4], cursors[donor], ceilings[donor], 'WITHSCORES', 'LIMIT', 0, 1)
            victim = candidate[1]
            if victim then cursors[donor] = '(' .. candidate[2] end
        until victim ~= ARGV[1]
        if not victim then error('missing cache victim') end
        local c, w = metadata(victim)
        if not c or c ~= donor - 1 or w > usage[donor] then error('invalid cache victim') end
        victims[#victims + 1] = victim
        usage[donor] = usage[donor] - w
        used = used - w
        need = math.max(0, weight - (budget - used))
    end
    if old_class then drop(ARGV[1]) end
    for _, victim in ipairs(victims) do drop(victim) end
    redis.call('HSET', KEYS[1], ARGV[1], ARGV[2])
    redis.call('HSET', KEYS[2], ARGV[1], tostring(incoming - 1) .. ':' .. ARGV[3])
    redis.call('ZADD', KEYS[3], ARGV[5], ARGV[1])
    local sequence_key = 's' .. tostring(incoming - 1)
    local sequence = math.max(now, (tonumber(redis.call('HGET', KEYS[5], sequence_key)) or 0) + 1)
    if sequence >= 1125899906842624 then error('cache order exhausted') end
    redis.call('HSET', KEYS[5], sequence_key, string.format('%.0f', sequence))
    redis.call('ZADD', KEYS[4], string.format('%.0f', (incoming - 1) * 1125899906842624 + sequence), ARGV[1])
    redis.call('HINCRBY', KEYS[5], 'total', ARGV[3])
    redis.call('HINCRBY', KEYS[5], tostring(incoming - 1), ARGV[3])
    local last = redis.call('ZRANGE', KEYS[3], -1, -1, 'WITHSCORES')
    for _, key in ipairs(KEYS) do redis.call('PEXPIREAT', key, last[2]) end
    return 1
end
-- Lua 原子执行但不在错误时回滚；写入或记账失败只能清理本实例的 namespace。
local ok, result = pcall(put)
if not ok then
    redis.call('DEL', unpack(KEYS))
    return redis.error_reply('runtime cache write failed')
end
return finish(result)
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_cache::KIB;

    #[tokio::test]
    #[ignore = "requires STRAVIA_TEST_REDIS_URL pointing to an isolated Redis 6.2+ instance"]
    async fn redis_expiry_eviction_remove_budget_and_namespace() {
        let url = std::env::var("STRAVIA_TEST_REDIS_URL").expect("isolated Redis URL required");
        let capacity = 8 * KIB;
        let cache = Arc::new(RedisCache::new(&url, capacity).await.unwrap());
        let clone = cache.clone();
        let independent = RedisCache::new(&url, capacity).await.unwrap();
        let ttl = Duration::from_secs(60);
        cache
            .put("generation", "a", Arc::new("a".to_owned()), 1, ttl)
            .await;
        assert!(clone.get::<String>("generation", "a").await.is_some());
        assert!(independent.get::<String>("generation", "a").await.is_none());
        assert!(cache.get::<String>("pricing", "a").await.is_none());
        clone.remove("generation", "a").await;
        assert!(cache.get::<String>("generation", "a").await.is_none());
        cache
            .put(
                "generation",
                "expires",
                Arc::new(1u64),
                8,
                Duration::from_millis(100),
            )
            .await;
        assert!(cache.get::<u64>("generation", "expires").await.is_some());
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(cache.get::<u64>("generation", "expires").await.is_none());
        cache
            .put(
                "generation",
                "a",
                Arc::new("a".repeat(3 * KIB)),
                3 * KIB,
                ttl,
            )
            .await;
        cache
            .put("pricing", "b", Arc::new("b".repeat(3 * KIB)), 3 * KIB, ttl)
            .await;
        assert!(cache.get::<String>("generation", "a").await.is_none());
        assert!(cache.get::<String>("pricing", "b").await.is_some());
        cache
            .put(
                "generation",
                "oversize",
                Arc::new("x".repeat(capacity)),
                capacity,
                ttl,
            )
            .await;
        assert!(
            cache
                .get::<String>("generation", "oversize")
                .await
                .is_none()
        );
        let mut connection = cache.connection.clone();
        let used: usize = ::redis::cmd("HGET")
            .arg(&cache.keys[4])
            .arg("total")
            .query_async(&mut connection)
            .await
            .unwrap();
        assert!(used <= capacity - REDIS_BASE_OVERHEAD);
        let values: Vec<Vec<u8>> = ::redis::cmd("HVALS")
            .arg(&cache.keys[0])
            .query_async(&mut connection)
            .await
            .unwrap();
        assert!(values.iter().map(Vec::len).sum::<usize>() <= used);
        let entries: usize = ::redis::cmd("ZCARD")
            .arg(&cache.keys[3])
            .query_async(&mut connection)
            .await
            .unwrap();
        assert!(entries * REDIS_ENTRY_OVERHEAD <= used);
        clone.remove("pricing", "b").await;
        assert!(cache.get::<String>("pricing", "b").await.is_none());
        let exists: usize = ::redis::cmd("EXISTS")
            .arg(&cache.keys)
            .query_async(&mut connection)
            .await
            .unwrap();
        assert_eq!(exists, 0);
        independent
            .put("pricing", "keep", Arc::new(2u64), 8, ttl)
            .await;
        cache
            .put("generation", "shutdown", Arc::new(1u64), 8, ttl)
            .await;
        cache.shutdown().await;
        assert!(cache.get::<u64>("generation", "shutdown").await.is_none());
        assert_eq!(*independent.get::<u64>("pricing", "keep").await.unwrap(), 2);
        independent.shutdown().await;
    }
}
