//! 可丢弃派生值缓存；预算由全部 domain 共享，SQL 仍是唯一权威数据源。

use std::{
    any::Any,
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use parking_lot::Mutex;
use serde::{Serialize, de::DeserializeOwned};
use tinyufo::TinyUfo;

const KIB: usize = 1024;
const ENTRY_OVERHEAD: usize = 256;
// Redis field 在两个 hash 和两个 zset 中的固定逻辑索引开销；非 allocator/RSS 承诺。
const REDIS_ENTRY_OVERHEAD: usize = 256;
// 五个固定 Redis key、容器和 used 计数的保守固定记账。
const REDIS_BASE_OVERHEAD: usize = 2048;

#[derive(Clone)]
pub(crate) struct RuntimeCache {
    backend: Backend,
}

#[derive(Clone)]
enum Backend {
    Local(Arc<LocalCache>),
    Redis(Arc<RedisCache>),
}

struct LocalCache {
    entries: TinyUfo<u64, Arc<LocalEntry>>,
    // 上游 put/remove 权重更新不是单次原子操作；仅串行 mutation，读取不加锁。
    mutation: Mutex<()>,
    capacity_bytes: usize,
}

struct LocalEntry {
    domain: &'static str,
    key: Box<str>,
    value: Arc<dyn Any + Send + Sync>,
    expires_at: Instant,
}

struct RedisCache {
    connection: redis::aio::MultiplexedConnection,
    keys: [String; 5],
    capacity_bytes: usize,
    get: redis::Script,
    put: redis::Script,
    remove: redis::Script,
}

impl RuntimeCache {
    pub(crate) fn tinyufo(capacity_bytes: usize) -> Self {
        let units = capacity_bytes / KIB;
        Self {
            backend: Backend::Local(Arc::new(LocalCache {
                entries: TinyUfo::new_compact(units, units.clamp(1, 65_536)),
                mutation: Mutex::new(()),
                capacity_bytes,
            })),
        }
    }

    pub(crate) async fn redis(url: &str, capacity_bytes: usize) -> anyhow::Result<Self> {
        // 不保留或透传上游错误：URL 解析及连接错误可能包含凭据。
        let client = redis::Client::open(url)
            .map_err(|_| anyhow::anyhow!("Runtime cache Redis URL is invalid"))?;
        // 可丢弃缓存不能无限阻塞 SQL 回退或退出；不重试，不改变模型请求 deadline。
        let connection_config = redis::AsyncConnectionConfig::new()
            .set_connection_timeout(Some(Duration::from_secs(5)))
            .set_response_timeout(Some(Duration::from_secs(1)));
        let connection = client
            .get_multiplexed_async_connection_with_config(&connection_config)
            .await
            .map_err(|_| anyhow::anyhow!("Runtime cache Redis connection failed"))?;
        let namespace = format!("stravia:runtime:v1:{}", uuid::Uuid::new_v4());
        Ok(Self {
            backend: Backend::Redis(Arc::new(RedisCache {
                connection,
                keys: ["values", "weights", "expiry", "order", "used"]
                    .map(|suffix| format!("{namespace}:{suffix}")),
                capacity_bytes,
                get: redis::Script::new(REDIS_GET),
                put: redis::Script::new(REDIS_PUT),
                remove: redis::Script::new(REDIS_REMOVE),
            })),
        })
    }

    pub(crate) async fn get<T: DeserializeOwned + Send + Sync + 'static>(
        &self,
        domain: &'static str,
        key: &str,
    ) -> Option<Arc<T>> {
        match &self.backend {
            Backend::Local(cache) => {
                let entry = cache.entries.get(&local_hash(domain, key))?;
                // TinyUFO 仅保留 u64 hash；必须核对完整身份，碰撞只能 miss。
                if entry.domain != domain
                    || entry.key.as_ref() != key
                    || Instant::now() >= entry.expires_at
                {
                    return None;
                }
                Arc::clone(&entry.value).downcast::<T>().ok()
            }
            Backend::Redis(cache) => {
                let mut connection = cache.connection.clone();
                let mut invocation = cache.get.prepare_invoke();
                for key in &cache.keys {
                    invocation.key(key);
                }
                let result: redis::RedisResult<Option<Vec<u8>>> = invocation
                    .arg(redis_field(domain, key))
                    .invoke_async(&mut connection)
                    .await;
                match result {
                    Ok(Some(payload)) => match serde_json::from_slice::<T>(&payload) {
                        Ok(value) => Some(Arc::new(value)),
                        Err(_) => {
                            warn("decode", domain);
                            None
                        }
                    },
                    Ok(None) => None,
                    Err(_) => {
                        warn("get", domain);
                        None
                    }
                }
            }
        }
    }

    pub(crate) fn can_admit<T>(
        &self,
        domain: &'static str,
        key: &str,
        estimated_bytes: usize,
    ) -> bool {
        self.admission_weight::<T>(domain, key, estimated_bytes)
            .is_some()
    }

    // 预检与实际写入共用完整身份和后端开销；Redis 编码后仍需按真实长度复核。
    fn admission_weight<T>(&self, domain: &str, key: &str, value_bytes: usize) -> Option<usize> {
        match &self.backend {
            Backend::Local(cache) => {
                let bytes = value_bytes
                    .max(std::mem::size_of::<T>())
                    .checked_add(key.len())?
                    .checked_add(domain.len())?
                    .checked_add(ENTRY_OVERHEAD)?;
                let units = bytes.div_ceil(KIB);
                if bytes > cache.capacity_bytes || units > cache.capacity_bytes / KIB {
                    return None;
                }
                u16::try_from(units).ok().map(usize::from)
            }
            Backend::Redis(cache) => {
                // field 是 domain 长度十进制前缀、冒号、domain 和完整 key。
                let prefix_bytes = domain.len().checked_ilog10().unwrap_or(0) as usize + 1;
                let field_bytes = prefix_bytes
                    .checked_add(1)?
                    .checked_add(domain.len())?
                    .checked_add(key.len())?;
                let weight = value_bytes
                    .checked_add(field_bytes.checked_mul(4)?)?
                    .checked_add(REDIS_ENTRY_OVERHEAD)?;
                let budget = cache.capacity_bytes.saturating_sub(REDIS_BASE_OVERHEAD);
                // Lua number 是 double；容量与权重必须保持精确整数。
                (weight <= budget && (cache.capacity_bytes as u128) < (1u128 << 53))
                    .then_some(weight)
            }
        }
    }

    pub(crate) async fn put<T: Serialize + Send + Sync + 'static>(
        &self,
        domain: &'static str,
        key: &str,
        value: Arc<T>,
        estimated_bytes: usize,
        ttl: Duration,
    ) {
        if ttl.is_zero() {
            return;
        }
        let Some(weight) = self.admission_weight::<T>(domain, key, estimated_bytes) else {
            return;
        };
        match &self.backend {
            Backend::Local(cache) => {
                let Some(expires_at) = Instant::now().checked_add(ttl) else {
                    return;
                };
                let entry = Arc::new(LocalEntry {
                    domain,
                    key: key.into(),
                    value,
                    expires_at,
                });
                let _guard = cache.mutation.lock();
                cache
                    .entries
                    .put(local_hash(domain, key), entry, weight as u16);
            }
            Backend::Redis(cache) => {
                // 使用调用开始时的绝对到期时间，序列化及网络排队不能延长 TTL。
                let Some(deadline) = SystemTime::now().checked_add(ttl) else {
                    return;
                };
                let Ok(deadline) = deadline.duration_since(UNIX_EPOCH) else {
                    return;
                };
                let expiry_millis = deadline.as_millis();
                if expiry_millis > (1u128 << 53) - 1 {
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
                let field = redis_field(domain, key);
                let budget = cache.capacity_bytes.saturating_sub(REDIS_BASE_OVERHEAD);
                let mut connection = cache.connection.clone();
                let mut invocation = cache.put.prepare_invoke();
                for key in &cache.keys {
                    invocation.key(key);
                }
                let result: redis::RedisResult<i64> = invocation
                    .arg(field)
                    .arg(payload)
                    .arg(weight)
                    .arg(budget)
                    .arg(expiry_millis as u64)
                    .invoke_async(&mut connection)
                    .await;
                if result.is_err() {
                    warn("put", domain);
                }
            }
        }
    }

    pub(crate) async fn remove(&self, domain: &'static str, key: &str) {
        match &self.backend {
            Backend::Local(cache) => {
                let _guard = cache.mutation.lock();
                let hash = local_hash(domain, key);
                if let Some(entry) = cache.entries.get(&hash)
                    && entry.domain == domain
                    && entry.key.as_ref() == key
                {
                    cache.entries.remove(&hash);
                }
            }
            Backend::Redis(cache) => {
                let mut connection = cache.connection.clone();
                let mut invocation = cache.remove.prepare_invoke();
                for key in &cache.keys {
                    invocation.key(key);
                }
                let result: redis::RedisResult<i64> = invocation
                    .arg(redis_field(domain, key))
                    .invoke_async(&mut connection)
                    .await;
                if result.is_err() {
                    warn("remove", domain);
                }
            }
        }
    }

    pub(crate) async fn shutdown(&self) {
        if let Backend::Redis(cache) = &self.backend {
            // 正常退出释放本次运行的五个容器；异常退出仍由容器 TTL 回收。
            let mut connection = cache.connection.clone();
            let result: redis::RedisResult<usize> = redis::cmd("DEL")
                .arg(&cache.keys)
                .query_async(&mut connection)
                .await;
            if result.is_err() {
                warn("shutdown", "runtime");
            }
        }
    }
}

fn local_hash(domain: &str, key: &str) -> u64 {
    let mut hash = DefaultHasher::new();
    (domain, key).hash(&mut hash);
    hash.finish()
}

fn redis_field(domain: &str, key: &str) -> String {
    // 长度前缀消除 domain/key 分隔歧义；不会将用户 key 用作 Redis key。
    format!("{}:{domain}{key}", domain.len())
}

fn warn(operation: &'static str, domain: &str) {
    // domain 由调用者选择，但日志仍使用白名单，避免未来传入敏感字符串。
    let safe_domain = match domain {
        "generation" | "generation.materialized" => "generation",
        "pricing" => "pricing",
        "affinity" => "affinity",
        "generation.catalog" => "generation_catalog",
        "provider_allowance" => "provider_allowance",
        "runtime" => "runtime",
        _ => "other",
    };
    tracing::warn!(
        operation,
        domain = safe_domain,
        "Runtime cache operation failed; treating derived value as unavailable"
    );
}

// 五个 key 全部显式传入 KEYS，淘汰只删除 hash field，无全库操作或动态 key。
// field 过期可保守占预算；下一次读取/淘汰会释放。容器 TTL 等于最大 field 到期时间，
// 不活跃实例的 payload 和全部索引最终一起消失。逻辑 TTL 不被容器 TTL 延长。
const REDIS_GET: &str = r#"
local expiry = tonumber(redis.call('ZSCORE', KEYS[3], ARGV[1]))
if not expiry then return false end
local t = redis.call('TIME')
local now = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)
if expiry <= now then
    local weight = tonumber(redis.call('HGET', KEYS[2], ARGV[1])) or 0
    redis.call('HDEL', KEYS[1], ARGV[1])
    redis.call('HDEL', KEYS[2], ARGV[1])
    redis.call('ZREM', KEYS[3], ARGV[1])
    redis.call('ZREM', KEYS[4], ARGV[1])
    if redis.call('EXISTS', KEYS[5]) == 1 then redis.call('DECRBY', KEYS[5], weight) end
    return false
end
return redis.call('HGET', KEYS[1], ARGV[1])
"#;

const REDIS_REMOVE: &str = r#"
local weight = tonumber(redis.call('HGET', KEYS[2], ARGV[1])) or 0
redis.call('HDEL', KEYS[1], ARGV[1])
redis.call('HDEL', KEYS[2], ARGV[1])
redis.call('ZREM', KEYS[3], ARGV[1])
redis.call('ZREM', KEYS[4], ARGV[1])
if redis.call('EXISTS', KEYS[5]) == 1 then redis.call('DECRBY', KEYS[5], weight) end
if redis.call('ZCARD', KEYS[3]) == 0 then redis.call('DEL', unpack(KEYS)) end
return weight
"#;

const REDIS_PUT: &str = r#"
local function put()
local weight = tonumber(ARGV[3])
local budget = tonumber(ARGV[4])
local expiry = tonumber(ARGV[5])
if weight > budget then return 0 end
local t = redis.call('TIME')
local now = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)
if expiry <= now or expiry > 9007199254740991 then return 0 end
local used = tonumber(redis.call('GET', KEYS[5])) or 0
local old = tonumber(redis.call('HGET', KEYS[2], ARGV[1])) or 0
used = used - old
redis.call('HDEL', KEYS[1], ARGV[1])
redis.call('HDEL', KEYS[2], ARGV[1])
redis.call('ZREM', KEYS[3], ARGV[1])
redis.call('ZREM', KEYS[4], ARGV[1])
while used + weight > budget do
    local victim = redis.call('ZRANGE', KEYS[4], 0, 0)[1]
    if not victim then error('cache budget metadata inconsistent') end
    used = used - (tonumber(redis.call('HGET', KEYS[2], victim)) or 0)
    redis.call('HDEL', KEYS[1], victim)
    redis.call('HDEL', KEYS[2], victim)
    redis.call('ZREM', KEYS[3], victim)
    redis.call('ZREM', KEYS[4], victim)
end
redis.call('HSET', KEYS[1], ARGV[1], ARGV[2])
redis.call('HSET', KEYS[2], ARGV[1], weight)
redis.call('ZADD', KEYS[3], expiry, ARGV[1])
redis.call('ZADD', KEYS[4], now, ARGV[1])
used = used + weight
redis.call('SET', KEYS[5], used)
local last = redis.call('ZRANGE', KEYS[3], -1, -1, 'WITHSCORES')
for _, key in ipairs(KEYS) do redis.call('PEXPIREAT', key, last[2]) end
-- 实际 payload 长度与保守逻辑索引开销同预算，不遍历全部 entry。
return 1
end
-- Lua 原子执行但不自动回滚；写入/记账失败时只清理本实例，绝不留下半写预算。
local ok, result = pcall(put)
if not ok then
    redis.call('DEL', unpack(KEYS))
    return redis.error_reply('runtime cache write failed')
end
return result
"#;

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: Duration = Duration::from_secs(60);

    #[tokio::test]
    async fn local_identity_type_remove_and_arc() {
        let cache = RuntimeCache::tinyufo(16 * KIB);
        let clone = cache.clone();
        let value = Arc::new(String::from("派生值"));
        cache
            .put("generation", "principal-a:item", value.clone(), 64, TTL)
            .await;
        let hit = clone
            .get::<String>("generation", "principal-a:item")
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&value, &hit));
        assert!(
            clone
                .get::<String>("generation", "principal-b:item")
                .await
                .is_none()
        );
        assert!(
            clone
                .get::<String>("pricing", "principal-a:item")
                .await
                .is_none()
        );
        assert!(
            clone
                .get::<u64>("generation", "principal-a:item")
                .await
                .is_none()
        );
        clone.remove("generation", "principal-a:item").await;
        assert!(
            cache
                .get::<String>("generation", "principal-a:item")
                .await
                .is_none()
        );
    }

    #[test]
    fn local_preflight_respects_rounded_budget_and_weight_boundaries() {
        let domain = "generation.catalog";
        let key = "principal:item";
        let overhead = domain.len() + key.len() + ENTRY_OVERHEAD;
        // 字节预算尚有余量也不能容纳第二个 KiB 权重单位。
        let partial = RuntimeCache::tinyufo(2 * KIB - 1);
        assert!(partial.can_admit::<u8>(domain, key, KIB - overhead));
        assert!(!partial.can_admit::<u8>(domain, key, KIB - overhead + 1));
        assert!(!partial.can_admit::<[u8; KIB]>(domain, key, 0));
        assert!(!partial.can_admit::<u8>(domain, key, usize::MAX));

        let large = RuntimeCache::tinyufo(65_536 * KIB);
        assert!(large.can_admit::<u8>(domain, key, 65_535 * KIB - overhead));
        assert!(!large.can_admit::<u8>(domain, key, 65_535 * KIB - overhead + 1));
    }

    #[tokio::test]
    async fn local_budget_zero_ttl_and_eviction() {
        let cache = RuntimeCache::tinyufo(KIB);
        cache
            .put("generation", "zero", Arc::new(1u64), 8, Duration::ZERO)
            .await;
        cache
            .put("generation", "large", Arc::new(1u64), KIB, TTL)
            .await;
        assert!(cache.get::<u64>("generation", "zero").await.is_none());
        assert!(cache.get::<u64>("generation", "large").await.is_none());
        cache.put("generation", "a", Arc::new(1u64), 8, TTL).await;
        cache.put("pricing", "b", Arc::new(2u64), 8, TTL).await;
        let a = cache.get::<u64>("generation", "a").await;
        let b = cache.get::<u64>("pricing", "b").await;
        assert_eq!(usize::from(a.is_some()) + usize::from(b.is_some()), 1);
        let large = RuntimeCache::tinyufo(128 * 1024 * KIB);
        large
            .put("generation", "u16", Arc::new(1u64), 65_536 * KIB, TTL)
            .await;
        assert!(large.get::<u64>("generation", "u16").await.is_none());
    }

    #[tokio::test]
    async fn local_expiry_and_hash_collision_are_misses() {
        let cache = RuntimeCache::tinyufo(4 * KIB);
        let Backend::Local(local) = &cache.backend else {
            unreachable!()
        };
        let hash = local_hash("generation", "requested");
        local.entries.put(
            hash,
            Arc::new(LocalEntry {
                domain: "generation",
                key: "collision".into(),
                value: Arc::new(9u64),
                expires_at: Instant::now() + TTL,
            }),
            1,
        );
        assert!(cache.get::<u64>("generation", "requested").await.is_none());
        cache.remove("generation", "requested").await;
        assert_eq!(local.entries.get(&hash).unwrap().key.as_ref(), "collision");
        local.entries.put(
            hash,
            Arc::new(LocalEntry {
                domain: "generation",
                key: "requested".into(),
                value: Arc::new(9u64),
                expires_at: Instant::now() - Duration::from_secs(1),
            }),
            1,
        );
        assert!(cache.get::<u64>("generation", "requested").await.is_none());
        cache
            .put("generation", "requested", Arc::new(10u64), 8, TTL)
            .await;
        assert_eq!(
            *cache.get::<u64>("generation", "requested").await.unwrap(),
            10
        );
    }

    #[tokio::test]
    #[ignore = "需要 STRAVIA_TEST_REDIS_URL 指向隔离的真实 Redis 6.2+ 实例"]
    async fn redis_expiry_eviction_remove_budget_and_namespace() {
        let url = std::env::var("STRAVIA_TEST_REDIS_URL").expect("需要隔离的真实 Redis URL");
        let capacity = 8 * KIB;
        let cache = RuntimeCache::redis(&url, capacity).await.unwrap();
        let independent = RuntimeCache::redis(&url, capacity).await.unwrap();
        let clone = cache.clone();
        cache
            .put("generation", "a", Arc::new("a".to_owned()), 1, TTL)
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
                TTL,
            )
            .await;
        cache
            .put("pricing", "b", Arc::new("b".repeat(3 * KIB)), 3 * KIB, TTL)
            .await;
        assert!(cache.get::<String>("generation", "a").await.is_none());
        assert!(cache.get::<String>("pricing", "b").await.is_some());
        cache
            .put(
                "generation",
                "oversize",
                Arc::new("x".repeat(capacity)),
                capacity,
                TTL,
            )
            .await;
        assert!(
            cache
                .get::<String>("generation", "oversize")
                .await
                .is_none()
        );
        let Backend::Redis(redis) = &cache.backend else {
            unreachable!()
        };
        let mut connection = redis.connection.clone();
        let used: usize = redis::cmd("GET")
            .arg(&redis.keys[4])
            .query_async(&mut connection)
            .await
            .unwrap();
        assert!(used <= capacity - REDIS_BASE_OVERHEAD);
        let values: Vec<Vec<u8>> = redis::cmd("HVALS")
            .arg(&redis.keys[0])
            .query_async(&mut connection)
            .await
            .unwrap();
        assert!(values.iter().map(Vec::len).sum::<usize>() <= used);
        let entries: usize = redis::cmd("ZCARD")
            .arg(&redis.keys[3])
            .query_async(&mut connection)
            .await
            .unwrap();
        assert!(entries * REDIS_ENTRY_OVERHEAD <= used);
        clone.remove("pricing", "b").await;
        assert!(cache.get::<String>("pricing", "b").await.is_none());
        let exists: usize = redis::cmd("EXISTS")
            .arg(&redis.keys)
            .query_async(&mut connection)
            .await
            .unwrap();
        assert_eq!(exists, 0);
        independent
            .put("pricing", "keep", Arc::new(2u64), 8, TTL)
            .await;
        cache
            .put("generation", "shutdown", Arc::new(1u64), 8, TTL)
            .await;
        cache.shutdown().await;
        assert!(cache.get::<u64>("generation", "shutdown").await.is_none());
        assert_eq!(*independent.get::<u64>("pricing", "keep").await.unwrap(), 2);
        independent.shutdown().await;
    }
}
