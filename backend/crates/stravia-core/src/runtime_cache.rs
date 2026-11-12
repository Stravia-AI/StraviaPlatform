//! 可丢弃派生值缓存；全部类别共享预算，缓存不成为业务权威。

use std::{sync::Arc, time::Duration};

use serde::{Serialize, de::DeserializeOwned};

mod local;
mod policy;
mod redis;

const KIB: usize = 1024;
const ENTRY_OVERHEAD: usize = 256;
// Redis field 在两个 hash 和两个 zset 中的固定逻辑索引开销；非 allocator/RSS 承诺。
const REDIS_ENTRY_OVERHEAD: usize = 256;
// 五个固定 Redis key、容器和总量 / 类别计数的保守固定记账。
const REDIS_BASE_OVERHEAD: usize = 2048;

#[derive(Clone)]
pub(crate) struct RuntimeCache {
    backend: Backend,
}

#[derive(Clone)]
enum Backend {
    Local(Arc<local::LocalCache>),
    Redis(Arc<redis::RedisCache>),
}

impl RuntimeCache {
    pub(crate) fn tinyufo(capacity_bytes: usize) -> Self {
        Self {
            backend: Backend::Local(Arc::new(local::LocalCache::new(capacity_bytes))),
        }
    }

    pub(crate) async fn redis(url: &str, capacity_bytes: usize) -> anyhow::Result<Self> {
        Ok(Self {
            backend: Backend::Redis(Arc::new(redis::RedisCache::new(url, capacity_bytes).await?)),
        })
    }

    pub(crate) async fn get<T: DeserializeOwned + Send + Sync + 'static>(
        &self,
        domain: &'static str,
        key: &str,
    ) -> Option<Arc<T>> {
        match &self.backend {
            Backend::Local(cache) => cache.get(domain, key),
            Backend::Redis(cache) => cache.get(domain, key).await,
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
            Backend::Local(cache) => cache.admission_weight::<T>(domain, key, value_bytes),
            Backend::Redis(cache) => cache.admission_weight(domain, key, value_bytes),
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
        match &self.backend {
            Backend::Local(cache) => cache.put(domain, key, value, estimated_bytes, ttl),
            Backend::Redis(cache) => {
                cache.put(domain, key, value, estimated_bytes, ttl).await;
            }
        }
    }

    pub(crate) async fn remove(&self, domain: &'static str, key: &str) {
        match &self.backend {
            Backend::Local(cache) => cache.remove(domain, key),
            Backend::Redis(cache) => cache.remove(domain, key).await,
        }
    }

    pub(crate) async fn shutdown(&self) {
        if let Backend::Redis(cache) = &self.backend {
            cache.shutdown().await;
        }
    }
}

fn warn(operation: &'static str, domain: &str) {
    // 不记录可能包含凭据的底层错误；domain 也使用白名单。
    let safe_domain = match domain {
        "generation" | "generation.materialized" => "generation",
        "pricing" => "pricing",
        "affinity" => "affinity",
        "generation.catalog" => "generation_catalog",
        "provider_allowance" => "provider_allowance",
        "credential_mappings" => "credential_mappings",
        "credential_mapping_presence" => "credential_mapping_presence",
        "runtime" => "runtime",
        _ => "other",
    };
    tracing::warn!(
        operation,
        domain = safe_domain,
        "Runtime cache operation failed; treating derived value as unavailable"
    );
}

#[cfg(test)]
mod tests;
