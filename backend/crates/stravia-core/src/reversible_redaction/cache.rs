use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use stravia_credential_protection::store::{
    DetectedSecret, InternedMappings, Mapping, MappingSnapshot, MappingStore,
};
use stravia_runtime_contract::{Principal, redaction::RedactionError};
use tokio::sync::Mutex;

use crate::runtime_cache::RuntimeCache;

const MAPPINGS: &str = "credential_mappings";
const PRESENCE: &str = "credential_mapping_presence";
const REFRESH_AFTER: Duration = Duration::from_secs(60);
const LOAD_SHARDS: usize = 32;
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

/// 只协调 SQL 加载和失效；值、预算、淘汰与后端全部由 Gateway 的 RuntimeCache 管理。
pub(super) struct CachedMappingStore {
    inner: Arc<dyn MappingStore>,
    cache: RuntimeCache,
    owner: u64,
    generation: AtomicU64,
    loads: [Mutex<()>; LOAD_SHARDS],
    presence_load: Mutex<()>,
}

// 刻意不实现 Debug，避免凭据原文进入诊断。owner 隔离替换适配器后的本地协调状态。
#[derive(Serialize, Deserialize)]
struct Cached<T> {
    owner: u64,
    generation: u64,
    value: T,
}

struct Mutation<'a> {
    generation: &'a AtomicU64,
    armed: bool,
}

impl Drop for Mutation<'_> {
    fn drop(&mut self) {
        if self.armed {
            // SQL 可能已经提交而确认尚未返回；取消不能让旧缓存继续有效。
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
    }
}

impl CachedMappingStore {
    pub(super) fn new(inner: Arc<dyn MappingStore>, cache: RuntimeCache) -> Self {
        Self {
            inner,
            cache,
            owner: NEXT_OWNER.fetch_add(1, Ordering::Relaxed),
            generation: AtomicU64::new(0),
            loads: std::array::from_fn(|_| Mutex::new(())),
            presence_load: Mutex::new(()),
        }
    }

    async fn cached<T: DeserializeOwned + Send + Sync + 'static>(
        &self,
        domain: &'static str,
        key: &str,
    ) -> Option<Arc<Cached<T>>> {
        let generation = self.generation.load(Ordering::Acquire);
        let entry = self.cache.get::<Cached<T>>(domain, key).await?;
        (entry.owner == self.owner
            && entry.generation == generation
            && self.generation.load(Ordering::Acquire) == generation)
            .then_some(entry)
    }

    fn load_shard(&self, principal: &str) -> &Mutex<()> {
        let mut hash = DefaultHasher::new();
        principal.hash(&mut hash);
        &self.loads[hash.finish() as usize % LOAD_SHARDS]
    }

    async fn remember<T: Serialize + Send + Sync + 'static>(
        &self,
        domain: &'static str,
        key: &str,
        value: T,
        estimated_bytes: usize,
        started: Instant,
        generation: u64,
    ) {
        if self.generation.load(Ordering::Acquire) != generation
            || !self
                .cache
                .can_admit::<Cached<T>>(domain, key, estimated_bytes)
        {
            return;
        }
        self.cache
            .put(
                domain,
                key,
                Arc::new(Cached {
                    owner: self.owner,
                    generation,
                    value,
                }),
                estimated_bytes,
                REFRESH_AFTER.saturating_sub(started.elapsed()),
            )
            .await;
        // Redis 写入可与失效并发；值内的代次让迟到写入只能导致 miss，不能复活旧快照。
    }
}

fn snapshot_weight(mappings: &Vec<Mapping>) -> usize {
    let mut bytes = std::mem::size_of::<Cached<MappingSnapshot>>()
        .saturating_add(std::mem::size_of::<Vec<Mapping>>())
        .saturating_add(
            mappings
                .capacity()
                .saturating_mul(std::mem::size_of::<Mapping>()),
        );
    for mapping in mappings {
        bytes = bytes
            .saturating_add(mapping.reference.capacity())
            .saturating_add(mapping.secret.capacity())
            .saturating_add(std::mem::size_of_val(mapping.rule_ids.as_slice()));
        for rule in mapping.rule_ids.as_slice() {
            bytes = bytes.saturating_add(rule.capacity());
        }
    }
    bytes
}

#[async_trait]
impl MappingStore for CachedMappingStore {
    async fn active(&self, principal: &Principal) -> Result<MappingSnapshot, RedactionError> {
        let key = principal.api_key_id();
        if let Some(entry) = self.cached::<MappingSnapshot>(MAPPINGS, key).await {
            return Ok(Arc::clone(&entry.value));
        }
        let _load = self.load_shard(key).lock().await;
        if let Some(entry) = self.cached::<MappingSnapshot>(MAPPINGS, key).await {
            return Ok(Arc::clone(&entry.value));
        }
        let started = Instant::now();
        let generation = self.generation.load(Ordering::Acquire);
        let mappings = self.inner.active(principal).await?;
        self.remember(
            MAPPINGS,
            key,
            Arc::clone(&mappings),
            snapshot_weight(&mappings),
            started,
            generation,
        )
        .await;
        Ok(mappings)
    }

    async fn any_active(&self) -> Result<bool, RedactionError> {
        if let Some(entry) = self.cached::<bool>(PRESENCE, "instance").await {
            return Ok(entry.value);
        }
        let _load = self.presence_load.lock().await;
        if let Some(entry) = self.cached::<bool>(PRESENCE, "instance").await {
            return Ok(entry.value);
        }
        let started = Instant::now();
        let generation = self.generation.load(Ordering::Acquire);
        let exists = self.inner.any_active().await?;
        self.remember(
            PRESENCE,
            "instance",
            exists,
            std::mem::size_of::<Cached<bool>>(),
            started,
            generation,
        )
        .await;
        Ok(exists)
    }

    async fn intern(
        &self,
        principal: &Principal,
        secrets: &[DetectedSecret],
    ) -> Result<InternedMappings, RedactionError> {
        if secrets.is_empty() {
            return self.inner.intern(principal, secrets).await;
        }
        let mut mutation = Mutation {
            generation: &self.generation,
            armed: true,
        };
        let result = self.inner.intern(principal, secrets).await;
        if let Ok(interned) = &result {
            if !interned.created.is_empty() {
                self.generation.fetch_add(1, Ordering::AcqRel);
            }
            // 已有映射的字段不变；每次规则命中都失效会抵消跨请求缓存。
            mutation.armed = false;
        }
        result
    }

    async fn publish(
        &self,
        principal: &Principal,
        references: &[String],
    ) -> Result<(), RedactionError> {
        self.inner.publish(principal, references).await
    }

    async fn sweep(&self, now: i64, retention: Duration) -> Result<u64, RedactionError> {
        let mut mutation = Mutation {
            generation: &self.generation,
            armed: true,
        };
        let result = self.inner.sweep(now, retention).await;
        if let Ok(removed) = &result {
            if *removed != 0 {
                self.generation.fetch_add(1, Ordering::AcqRel);
            }
            mutation.armed = false;
        }
        result
    }

    async fn plaintext(&self, reference: &str) -> Result<Option<String>, RedactionError> {
        self.inner.plaintext(reference).await
    }
}
