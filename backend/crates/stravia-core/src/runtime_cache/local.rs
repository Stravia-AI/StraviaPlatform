use std::{
    any::Any,
    collections::{BTreeSet, HashMap, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    sync::Arc,
    time::{Duration, Instant},
};

use parking_lot::{Mutex, RwLock};
use tinyufo::TinyUfo;

use super::{
    ENTRY_OVERHEAD, KIB,
    policy::{self, CLASS_COUNT, CacheClass},
};
use crate::performance::{ResourceLease, ResourceUsage};

pub(super) struct LocalCache {
    // 类别队列只保存 64 位索引哈希与权重，负责 S3-FIFO 淘汰排序；
    // 实际条目存放在分桶 HashMap 中，淘汰/覆盖/过期时在锁外立即同步释放，
    // 避免大对象（如 MaterializedGeneration、CacheAffinity）滞留在
    // TinyUFO/SkipMap 底层的 crossbeam-epoch 延迟回收袋中放大常驻内存。
    entries: [TinyUfo<u64, u64>; CLASS_COUNT],
    values: [RwLock<HashMap<u64, Arc<LocalEntry>>>; CLASS_COUNT],
    mutation: Mutex<Ledger>,
    capacity_bytes: usize,
}

struct LocalEntry {
    domain: &'static str,
    key: Box<str>,
    value: Arc<dyn Any + Send + Sync>,
    expires_at: Instant,
    _usage: ResourceLease,
}

struct EntryMeta {
    expires_at: Instant,
    weight: usize,
}

struct Ledger {
    usage: [usize; CLASS_COUNT],
    counts: [usize; CLASS_COUNT],
    stale: [usize; CLASS_COUNT],
    entries: HashMap<(usize, u64), EntryMeta>,
    expiry: BTreeSet<(Instant, usize, u64)>,
    domains: HashMap<&'static str, ResourceUsage>,
}

fn local_hash(domain: &str, key: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    domain.hash(&mut hasher);
    key.hash(&mut hasher);
    hasher.finish()
}

impl LocalCache {
    pub(super) fn new(capacity_bytes: usize) -> Self {
        let units = capacity_bytes / KIB;
        // 拆分既有整池估算，避免每个类别都分配一份整池 sketch 和桶表。
        let estimate = units.clamp(1, 65_536).div_ceil(CLASS_COUNT);
        Self {
            entries: std::array::from_fn(|_| TinyUfo::new_compact(units, estimate)),
            values: std::array::from_fn(|_| RwLock::new(HashMap::new())),
            mutation: Mutex::new(Ledger {
                usage: [0; CLASS_COUNT],
                counts: [0; CLASS_COUNT],
                stale: [0; CLASS_COUNT],
                entries: HashMap::new(),
                expiry: BTreeSet::new(),
                domains: HashMap::new(),
            }),
            capacity_bytes,
        }
    }

    pub(super) fn get<T: Send + Sync + 'static>(
        &self,
        domain: &'static str,
        key: &str,
    ) -> Option<Arc<T>> {
        let class = CacheClass::for_domain(domain).index();
        let hash = local_hash(domain, key);
        let entry = self.values[class].read().get(&hash).cloned()?;
        if entry.domain != domain || entry.key.as_ref() != key {
            return None;
        }
        if Instant::now() >= entry.expires_at {
            let mut dropped = Vec::new();
            {
                let mut ledger = self.mutation.lock();
                // 等待锁期间可能已刷新同键值，不能删除替换后的新条目。
                let is_same = self.values[class]
                    .read()
                    .get(&hash)
                    .is_some_and(|current| Arc::ptr_eq(current, &entry));
                if is_same {
                    self.remove_indexed(&mut ledger, class, hash, &mut dropped);
                }
            }
            drop(dropped);
            return None;
        }
        let _ = self.entries[class].get(&hash);
        Arc::clone(&entry.value).downcast::<T>().ok()
    }

    pub(super) fn admission_weight<T>(
        &self,
        domain: &str,
        key: &str,
        value_bytes: usize,
    ) -> Option<usize> {
        let bytes = value_bytes
            .max(std::mem::size_of::<T>())
            .checked_add(key.len())?
            .checked_add(domain.len())?
            .checked_add(ENTRY_OVERHEAD)?;
        let units = bytes.div_ceil(KIB);
        if bytes > self.capacity_bytes || units > self.capacity_bytes / KIB {
            return None;
        }
        u16::try_from(units).ok().map(usize::from)
    }

    pub(super) fn put<T: Send + Sync + 'static>(
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
        let Some(expires_at) = Instant::now().checked_add(ttl) else {
            return;
        };
        let class = CacheClass::for_domain(domain);
        let index = class.index();
        let hash = local_hash(domain, key);
        let capacity = self.capacity_bytes / KIB;
        let mut dropped = Vec::new();
        {
            let mut ledger = self.mutation.lock();
            self.purge_expired(&mut ledger, Instant::now(), &mut dropped);
            if Instant::now() >= expires_at {
                return;
            }

            let old = self.values[index].read().get(&hash).cloned();
            // hash 只是索引；碰撞不能覆盖另一个完整身份。
            if old
                .as_ref()
                .is_some_and(|entry| entry.domain != domain || entry.key.as_ref() != key)
            {
                return;
            }
            let mut prospective = ledger.usage;
            if old.is_some()
                && let Some(meta) = ledger.entries.get(&(index, hash))
            {
                prospective[index] -= meta.weight;
            }
            if !policy::can_admit(capacity, &prospective, class, weight) {
                return;
            }

            // 无压力刷新保留原桶的热度，避免每次刷新都产生失效 FIFO 节点。
            // 压力淘汰时须排除同键旧条目，以免拒绝的替换破坏旧值。
            if old.is_some() {
                if prospective.iter().sum::<usize>() > capacity - weight {
                    self.remove_indexed(&mut ledger, index, hash, &mut dropped);
                } else {
                    self.forget(&mut ledger, index, hash);
                }
            }
            while ledger.usage.iter().sum::<usize>() > capacity - weight {
                let Some(victim) = policy::victim_class(capacity, &ledger.usage, class, weight)
                else {
                    return;
                };
                let victim_index = victim.index();
                let Some(evicted) = self.entries[victim_index].evict() else {
                    return;
                };
                let victim_hash = evicted.data;
                if let Some(prev) = self.values[victim_index].write().remove(&victim_hash) {
                    dropped.push(prev);
                }
                self.forget(&mut ledger, victim_index, victim_hash);
                self.compact_if_needed(&mut ledger, victim_index);
            }
            let usage = ledger
                .domains
                .entry(domain)
                .or_insert_with(|| ResourceUsage::new("runtime_cache_local_estimate", domain));
            let entry = Arc::new(LocalEntry {
                domain,
                key: key.into(),
                value,
                expires_at,
                _usage: usage.lease(1, (weight * KIB) as u64),
            });
            // 协调器已释放所需空间；优先级准入先于 TinyLFU，类内使用原生 S3-FIFO。
            let evicted = self.entries[index].put(hash, hash, weight as u16);
            let mut self_evicted = false;
            if !evicted.is_empty() {
                let mut values = self.values[index].write();
                for item in evicted {
                    let evicted_hash = item.data;
                    if evicted_hash == hash {
                        self_evicted = true;
                    } else if let Some(prev) = values.remove(&evicted_hash) {
                        dropped.push(prev);
                    }
                    self.forget(&mut ledger, index, evicted_hash);
                }
            }
            if self_evicted {
                if let Some(prev) = self.values[index].write().remove(&hash) {
                    dropped.push(prev);
                }
                dropped.push(entry);
                return;
            }
            if let Some(prev) = self.values[index].write().insert(hash, entry) {
                dropped.push(prev);
            }
            ledger.usage[index] += weight;
            ledger.counts[index] += 1;
            ledger
                .entries
                .insert((index, hash), EntryMeta { expires_at, weight });
            ledger.expiry.insert((expires_at, index, hash));
        }
        drop(dropped);
    }

    pub(super) fn remove(&self, domain: &'static str, key: &str) {
        let index = CacheClass::for_domain(domain).index();
        let hash = local_hash(domain, key);
        let mut dropped = Vec::new();
        {
            let mut ledger = self.mutation.lock();
            let matches = self.values[index]
                .read()
                .get(&hash)
                .is_some_and(|entry| entry.domain == domain && entry.key.as_ref() == key);
            if matches {
                self.remove_indexed(&mut ledger, index, hash, &mut dropped);
            }
        }
        drop(dropped);
    }

    fn forget(&self, ledger: &mut Ledger, index: usize, hash: u64) {
        if let Some(meta) = ledger.entries.remove(&(index, hash)) {
            ledger.usage[index] -= meta.weight;
            ledger.counts[index] -= 1;
            ledger.expiry.remove(&(meta.expires_at, index, hash));
        }
    }

    fn remove_indexed(
        &self,
        ledger: &mut Ledger,
        index: usize,
        hash: u64,
        dropped: &mut Vec<Arc<LocalEntry>>,
    ) {
        if self.entries[index].remove(&hash).is_some() {
            ledger.stale[index] = ledger.stale[index].saturating_add(1);
        }
        if let Some(prev) = self.values[index].write().remove(&hash) {
            dropped.push(prev);
        }
        self.forget(ledger, index, hash);
        self.compact_if_needed(ledger, index);
    }

    // 每次删除至多留下一个失效节点；摊还压缩把队列限制为活跃节点的倍数加常数。
    // mutation 锁排除全部队列写入，不阻塞未过期值的读取。
    fn compact_if_needed(&self, ledger: &mut Ledger, index: usize) {
        if ledger.stale[index] > ledger.counts[index].saturating_add(64) {
            self.entries[index].compact_queues();
            ledger.stale[index] = 0;
        }
    }

    fn purge_expired(&self, ledger: &mut Ledger, now: Instant, dropped: &mut Vec<Arc<LocalEntry>>) {
        while let Some(&(expires_at, index, hash)) = ledger.expiry.first() {
            if expires_at > now {
                break;
            }
            self.remove_indexed(ledger, index, hash, dropped);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: Duration = Duration::from_secs(60);

    #[test]
    fn local_accounting_follows_shared_cache_owner_lifetime() {
        let cache = Arc::new(LocalCache::new(KIB));
        let shared = Arc::clone(&cache);
        let value = Arc::new(String::from("retained"));
        cache.put("generation", "a", Arc::clone(&value), 8, TTL);
        let usage = cache.mutation.lock().domains["generation"].clone();
        assert_eq!(usage.totals(), (1, KIB as u64));
        drop(cache);
        assert_eq!(
            shared.get::<String>("generation", "a").as_deref(),
            Some(value.as_ref())
        );
        assert_eq!(usage.totals(), (1, KIB as u64));
        drop(shared);
        assert_eq!(Arc::strong_count(&value), 1);
        assert_eq!(usage.totals(), (0, 0));
    }

    #[test]
    fn hot_small_shard_can_be_reclaimed() {
        let cache = LocalCache::new(100 * KIB);
        // 热借用条目位于未达到 small 队列阈值的类别，提升后仍须能从 main 淘汰。
        cache.put("pricing", "large", Arc::new(1u64), 98 * KIB, TTL);
        cache.put("other", "hot", Arc::new(2u64), 8, TTL);
        for _ in 0..3 {
            assert_eq!(*cache.get::<u64>("other", "hot").unwrap(), 2);
        }
        cache.put("pricing", "new", Arc::new(3u64), 8, TTL);
        assert!(cache.get::<u64>("other", "hot").is_none());
        assert_eq!(*cache.get::<u64>("pricing", "large").unwrap(), 1);
        assert_eq!(*cache.get::<u64>("pricing", "new").unwrap(), 3);
    }

    #[test]
    fn repeated_replacements_and_removals_preserve_values_under_pressure() {
        let cache = LocalCache::new(2 * KIB);
        cache.put("generation", "anchor", Arc::new(42u64), 8, TTL);
        for n in 0..200u64 {
            cache.put("generation", "a", Arc::new(n), 8, TTL);
            cache.put("generation", "a", Arc::new(n + 1), 8, TTL);
            assert_eq!(*cache.get::<u64>("generation", "a").unwrap(), n + 1);
            cache.remove("generation", "a");
            assert!(cache.get::<u64>("generation", "a").is_none());
            assert_eq!(*cache.get::<u64>("generation", "anchor").unwrap(), 42);
        }
        cache.put("generation", "a", Arc::new(1u64), 8, TTL);
        cache.put("generation", "c", Arc::new(3u64), 8, TTL);
        assert_eq!(*cache.get::<u64>("generation", "c").unwrap(), 3);
        let retained = usize::from(cache.get::<u64>("generation", "anchor").is_some())
            + usize::from(cache.get::<u64>("generation", "a").is_some());
        assert_eq!(retained, 1);
        let ledger = cache.mutation.lock();
        assert_eq!(ledger.usage.iter().sum::<usize>(), 2);
    }

    #[test]
    fn local_expiry_and_hash_collision_are_misses() {
        let cache = LocalCache::new(4 * KIB);
        let class = CacheClass::for_domain("generation").index();
        let hash = local_hash("generation", "requested");
        let entry = |key: &str, expires_at| {
            Arc::new(LocalEntry {
                domain: "generation",
                key: key.into(),
                value: Arc::new(9u64),
                expires_at,
                _usage: ResourceUsage::new("runtime_cache_local_estimate", "generation")
                    .lease(1, KIB as u64),
            })
        };
        cache.entries[class].put(hash, hash, 1);
        cache.values[class]
            .write()
            .insert(hash, entry("collision", Instant::now() + TTL));
        assert!(cache.get::<u64>("generation", "requested").is_none());
        cache.remove("generation", "requested");
        assert_eq!(
            cache.values[class].read().get(&hash).unwrap().key.as_ref(),
            "collision"
        );
        cache.entries[class].put(hash, hash, 1);
        cache.values[class]
            .write()
            .insert(hash, entry("requested", Instant::now() - TTL));
        assert!(cache.get::<u64>("generation", "requested").is_none());
        cache.put("generation", "requested", Arc::new(10u64), 8, TTL);
        assert_eq!(*cache.get::<u64>("generation", "requested").unwrap(), 10);
    }

    #[test]
    fn overwritten_and_evicted_values_drop_immediately_without_epoch_retention() {
        let cache = Arc::new(LocalCache::new(8 * KIB));
        let mut tracked_overwrites = Vec::new();
        for i in 0..80u64 {
            let payload = Arc::new(vec![i as u8; 1024]);
            let weak = Arc::downgrade(&payload);
            cache.put("affinity", "hot-key", payload, 1024, TTL);
            if let Some(prev) = tracked_overwrites.last() {
                assert!(
                    std::sync::Weak::upgrade(prev).is_none(),
                    "overwritten affinity entry #{i} must drop synchronously, not wait on crossbeam-epoch"
                );
            }
            tracked_overwrites.push(weak);
        }
        // Only the final version of hot-key should remain alive.
        let alive_overwrites = tracked_overwrites
            .iter()
            .filter(|w| w.upgrade().is_some())
            .count();
        assert_eq!(alive_overwrites, 1);

        let mut tracked_evictions = Vec::new();
        for i in 0..40u64 {
            let payload = Arc::new(vec![i as u8; 2048]);
            tracked_evictions.push(Arc::downgrade(&payload));
            cache.put(
                "generation.materialized",
                &format!("gen-{i}"),
                payload,
                2048,
                TTL,
            );
        }
        let alive_evictions = tracked_evictions
            .iter()
            .filter(|w| w.upgrade().is_some())
            .count();
        assert!(
            alive_evictions <= 4,
            "evicted materialized generations must drop synchronously within budget; alive={alive_evictions}"
        );
        let usage = cache.mutation.lock().domains["generation.materialized"].clone();
        assert!(
            usage.totals().1 <= 8 * KIB as u64,
            "retained resource lease bytes must not exceed cache capacity: {:?}",
            usage.totals()
        );
    }
}
