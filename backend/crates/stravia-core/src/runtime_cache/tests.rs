use super::*;

const TTL: Duration = Duration::from_secs(60);

#[tokio::test]
async fn a_lower_priority_class_recovers_its_reserved_share_from_a_hot_borrower() {
    let cache = RuntimeCache::tinyufo(32 * KIB);
    let borrowed = Arc::new("x".repeat(30 * KIB));
    for _ in 0..8 {
        cache
            .put("pricing", "borrowed", borrowed.clone(), 30 * KIB, TTL)
            .await;
    }
    cache
        .put(
            "generation.catalog",
            "reference",
            Arc::new("catalog".to_owned()),
            KIB,
            TTL,
        )
        .await;
    assert_eq!(
        cache
            .get::<String>("generation.catalog", "reference")
            .await
            .as_deref()
            .map(String::as_str),
        Some("catalog")
    );
}

#[tokio::test]
async fn local_identity_type_and_remove() {
    let cache = RuntimeCache::tinyufo(16 * KIB);
    let clone = cache.clone();
    cache
        .put(
            "generation",
            "principal-a:item",
            Arc::new(String::from("派生值")),
            64,
            TTL,
        )
        .await;
    assert_eq!(
        clone
            .get::<String>("generation", "principal-a:item")
            .await
            .as_deref()
            .map(String::as_str),
        Some("派生值")
    );
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
    assert!(cache.get::<u64>("generation", "a").await.is_none());
    assert_eq!(cache.get::<u64>("pricing", "b").await.as_deref(), Some(&2));
    let large = RuntimeCache::tinyufo(128 * 1024 * KIB);
    large
        .put("generation", "u16", Arc::new(1u64), 65_536 * KIB, TTL)
        .await;
    assert!(large.get::<u64>("generation", "u16").await.is_none());
}

// 两路后端可用预算均为 32 KiB；Redis 另扣固定开销，本地权重向上取整至 KiB。
async fn pressure_policy(cache: &RuntimeCache) {
    // 类别可借用其他类别的空闲份额，不受自身保护目标限制。
    cache
        .put(
            "generation.catalog",
            "protected",
            Arc::new("catalog".to_owned()),
            KIB,
            TTL,
        )
        .await;
    cache
        .put(
            "pricing",
            "borrower",
            Arc::new("borrowed".to_owned()),
            20 * KIB,
            TTL,
        )
        .await;
    assert_eq!(
        cache
            .get::<String>("generation.catalog", "protected")
            .await
            .as_deref()
            .map(String::as_str),
        Some("catalog")
    );
    assert_eq!(
        cache
            .get::<String>("pricing", "borrower")
            .await
            .as_deref()
            .map(String::as_str),
        Some("borrowed")
    );

    // 低优先级借用不得挤掉其他类别已经占用的保护份额。
    cache.remove("pricing", "borrower").await;
    cache
        .put(
            "other",
            "too-large-to-borrow",
            Arc::new("rejected".to_owned()),
            31 * KIB,
            TTL,
        )
        .await;
    assert!(
        cache
            .get::<String>("other", "too-large-to-borrow")
            .await
            .is_none()
    );
    assert_eq!(
        cache
            .get::<String>("generation.catalog", "protected")
            .await
            .as_deref()
            .map(String::as_str),
        Some("catalog")
    );
    cache.remove("generation.catalog", "protected").await;
    cache
        .put(
            "other",
            "large",
            Arc::new("large".to_owned()),
            31 * KIB,
            TTL,
        )
        .await;
    assert_eq!(
        cache
            .get::<String>("other", "large")
            .await
            .as_deref()
            .map(String::as_str),
        Some("large")
    );
    // 高优先级写入从低优先级借用量回收空间。
    cache
        .put(
            "pricing",
            "important",
            Arc::new("important".to_owned()),
            KIB,
            TTL,
        )
        .await;
    assert_eq!(
        cache
            .get::<String>("pricing", "important")
            .await
            .as_deref()
            .map(String::as_str),
        Some("important")
    );
    assert!(cache.get::<String>("other", "large").await.is_none());
}

async fn replacement_and_expiry(cache: &RuntimeCache) {
    cache
        .put("pricing", "stable", Arc::new("old".to_owned()), 1, TTL)
        .await;
    cache
        .put(
            "pricing",
            "stable",
            Arc::new("oversized".to_owned()),
            64 * KIB,
            TTL,
        )
        .await;
    assert_eq!(
        cache
            .get::<String>("pricing", "stable")
            .await
            .as_deref()
            .map(String::as_str),
        Some("old")
    );
    cache
        .put(
            "pricing",
            "stable",
            Arc::new("zero ttl".to_owned()),
            1,
            Duration::ZERO,
        )
        .await;
    assert_eq!(
        cache
            .get::<String>("pricing", "stable")
            .await
            .as_deref()
            .map(String::as_str),
        Some("old")
    );
    cache
        .put("pricing", "stable", Arc::new("new".to_owned()), 1, TTL)
        .await;
    assert_eq!(
        cache
            .get::<String>("pricing", "stable")
            .await
            .as_deref()
            .map(String::as_str),
        Some("new")
    );
    cache.remove("pricing", "stable").await;
    // 替换值单独可入缓存，但不能挤掉别类已占用的保护份额；拒绝后保留旧值。
    cache
        .put(
            "pricing",
            "protected",
            Arc::new("protected".to_owned()),
            1,
            TTL,
        )
        .await;
    cache
        .put(
            "generation.catalog",
            "replace",
            Arc::new("original".to_owned()),
            1,
            TTL,
        )
        .await;
    cache
        .put(
            "generation.catalog",
            "replace",
            Arc::new("too big".to_owned()),
            32 * KIB - 400,
            TTL,
        )
        .await;
    assert_eq!(
        cache
            .get::<String>("generation.catalog", "replace")
            .await
            .as_deref()
            .map(String::as_str),
        Some("original")
    );
    assert_eq!(
        cache
            .get::<String>("pricing", "protected")
            .await
            .as_deref()
            .map(String::as_str),
        Some("protected")
    );
    cache.remove("generation.catalog", "replace").await;
    cache.remove("pricing", "protected").await;
    cache
        .put(
            "pricing",
            "expiring",
            Arc::new("short".to_owned()),
            31 * KIB,
            Duration::from_millis(100),
        )
        .await;
    assert_eq!(
        cache
            .get::<String>("pricing", "expiring")
            .await
            .as_deref()
            .map(String::as_str),
        Some("short")
    );
    tokio::time::sleep(Duration::from_millis(150)).await;
    // 已过期的高优先级借用不能阻塞低优先级写入。
    cache
        .put(
            "other",
            "after-expiry",
            Arc::new("fresh".to_owned()),
            31 * KIB,
            TTL,
        )
        .await;
    assert!(cache.get::<String>("pricing", "expiring").await.is_none());
    assert_eq!(
        cache
            .get::<String>("other", "after-expiry")
            .await
            .as_deref()
            .map(String::as_str),
        Some("fresh")
    );
}

#[tokio::test]
async fn local_pressure_borrowing_and_protection() {
    pressure_policy(&RuntimeCache::tinyufo(32 * KIB)).await;
}

#[tokio::test]
async fn local_replacement_and_expiry_release_space() {
    replacement_and_expiry(&RuntimeCache::tinyufo(32 * KIB)).await;
}

#[tokio::test]
async fn local_concurrent_writes_preserve_protected_entry_and_budget() {
    concurrent_pressure(&RuntimeCache::tinyufo(32 * KIB)).await;
}

async fn concurrent_pressure(cache: &RuntimeCache) {
    cache
        .put("pricing", "protected", Arc::new(99u64), 8, TTL)
        .await;
    let mut tasks = Vec::new();
    for index in 0..16 {
        let cache = cache.clone();
        tasks.push(tokio::spawn(async move {
            let key = format!("entry-{index}");
            cache
                .put("other", &key, Arc::new(index), 4 * KIB, TTL)
                .await;
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(
        cache.get::<u64>("pricing", "protected").await.as_deref(),
        Some(&99)
    );
    let mut retained = 0;
    for index in 0..16 {
        let key = format!("entry-{index}");
        if let Some(value) = cache.get::<i32>("other", &key).await {
            assert_eq!(*value, index);
            retained += 1;
        }
    }
    // 保护值之外，本地按 KiB 取整保留六项，Redis 按字节记账可保留七项。
    assert!((6..=7).contains(&retained));
}

async fn bulk_pressure(cache: &RuntimeCache) {
    cache
        .put("pricing", "protected", Arc::new(99u64), 8, TTL)
        .await;
    for index in 0..28u64 {
        cache
            .put("other", &format!("cold-{index}"), Arc::new(index), 8, TTL)
            .await;
    }
    cache
        .put(
            "generation.materialized",
            "bulk",
            Arc::new("materialized".to_owned()),
            24 * KIB,
            TTL,
        )
        .await;
    assert_eq!(
        cache
            .get::<String>("generation.materialized", "bulk")
            .await
            .as_deref()
            .map(String::as_str),
        Some("materialized")
    );
    assert_eq!(
        cache.get::<u64>("pricing", "protected").await.as_deref(),
        Some(&99)
    );
    assert!(cache.get::<u64>("other", "cold-0").await.is_none());
    assert_eq!(
        cache.get::<u64>("other", "cold-27").await.as_deref(),
        Some(&27)
    );
}

#[tokio::test]
async fn local_bulk_pressure_preserves_protected_and_recent_cold_values() {
    bulk_pressure(&RuntimeCache::tinyufo(32 * KIB)).await;
}

#[tokio::test]
async fn local_zero_and_sub_kib_budgets_never_cache_entries() {
    for capacity in [0, KIB - 1] {
        let cache = RuntimeCache::tinyufo(capacity);
        assert!(!cache.can_admit::<u64>("pricing", "key", 8));
        cache.put("pricing", "key", Arc::new(1u64), 8, TTL).await;
        assert!(cache.get::<u64>("pricing", "key").await.is_none());
    }
}

#[tokio::test]
#[ignore = "requires STRAVIA_TEST_REDIS_URL pointing to an isolated Redis 6.2+ instance"]
async fn redis_public_policy_and_namespace() {
    let url = std::env::var("STRAVIA_TEST_REDIS_URL").expect("isolated Redis URL required");
    let capacity = 32 * KIB + REDIS_BASE_OVERHEAD;
    let cache = RuntimeCache::redis(&url, capacity).await.unwrap();
    let independent = RuntimeCache::redis(&url, capacity).await.unwrap();
    pressure_policy(&cache).await;
    cache.shutdown().await;
    replacement_and_expiry(&cache).await;
    independent
        .put("pricing", "isolated", Arc::new(42u64), 8, TTL)
        .await;
    assert!(cache.get::<u64>("pricing", "isolated").await.is_none());
    cache.shutdown().await;
    assert_eq!(
        independent
            .get::<u64>("pricing", "isolated")
            .await
            .as_deref(),
        Some(&42)
    );
    independent.shutdown().await;
    cache.shutdown().await;
    concurrent_pressure(&cache).await;
    cache.shutdown().await;
    bulk_pressure(&cache).await;
    cache.shutdown().await;
    let reclaim = RuntimeCache::redis(&url, 32 * KIB).await.unwrap();
    for _ in 0..8 {
        reclaim
            .put(
                "pricing",
                "borrowed",
                Arc::new("hot".to_owned()),
                29 * KIB,
                TTL,
            )
            .await;
    }
    assert_eq!(
        reclaim
            .get::<String>("pricing", "borrowed")
            .await
            .as_deref()
            .map(String::as_str),
        Some("hot")
    );
    reclaim
        .put(
            "generation.catalog",
            "reference",
            Arc::new("catalog".to_owned()),
            KIB,
            TTL,
        )
        .await;
    assert_eq!(
        reclaim
            .get::<String>("generation.catalog", "reference")
            .await
            .as_deref()
            .map(String::as_str),
        Some("catalog")
    );
    assert!(reclaim.get::<String>("pricing", "borrowed").await.is_none());
    reclaim.shutdown().await;
    let zero = RuntimeCache::redis(&url, REDIS_BASE_OVERHEAD)
        .await
        .unwrap();
    zero.put("pricing", "key", Arc::new(1u64), 8, TTL).await;
    assert!(zero.get::<u64>("pricing", "key").await.is_none());
    zero.shutdown().await;
}
