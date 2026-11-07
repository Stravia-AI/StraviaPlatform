use super::{MemoryStorage, PromptAdmission, PromptAdmissionStore, SqliteStorage, Storage};

fn record(
    id: &str,
    principal: &str,
    parent: Option<&str>,
    ordinal: i64,
    count: i64,
    digest: &str,
) -> PromptAdmission {
    PromptAdmission {
        id: id.into(),
        principal: principal.into(),
        parent_id: parent.map(str::to_owned),
        atomic_lineage_created: false,
        ordinal,
        context_window_id: None,
        context_window_index: None,
        user_turn_index: Some(ordinal),
        dialogue_prompt_index: Some(ordinal),
        created_at_ms: 100,
        conversation_id: format!("conversation-{principal}"),
        conversation_created_at_ms: 100,
        user_prompt_id: Some(format!("prompt-{ordinal}")),
        user_prompt_created_at_ms: Some(100),
        logical_turn_id: format!("prompt-{ordinal}"),
        logical_turn_created_at_ms: 100,
        history_count: count,
        prefix_digest: digest.into(),
        controls_digest: "controls".into(),
    }
}

async fn contract(store: &dyn PromptAdmissionStore) -> anyhow::Result<()> {
    let root = record("root", "alice", None, 1, 1, "hash-a");
    store.insert(&root).await?;
    assert_eq!(store.get("alice", "root").await?, Some(root.clone()));
    assert_eq!(store.get("bob", "root").await?, None);
    assert!(store.insert(&root).await.is_err());
    let window_id = uuid::Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff")?;
    let mut window = record("window", "alice", None, 1, 4, "window");
    window.context_window_id = Some(window_id);
    window.context_window_index = Some(3);
    store.insert(&window).await?;
    assert_eq!(store.get("alice", "window").await?, Some(window.clone()));
    assert_eq!(
        store
            .find_prefixes("alice", "controls", &[(4, "window".into())])
            .await?,
        vec![window.clone()]
    );
    let mut incomplete = window.clone();
    incomplete.id = "incomplete-window".into();
    incomplete.context_window_index = None;
    assert!(store.insert(&incomplete).await.is_err());
    let mut overflow = window;
    overflow.id = "overflow-window".into();
    overflow.context_window_index = Some(u64::MAX);
    assert!(store.insert(&overflow).await.is_err());
    assert!(
        store
            .insert(&record("zero", "alice", None, 0, 0, ""))
            .await
            .is_err()
    );
    let mut no_human = record("no-human", "alice", None, 0, 0, "no-human");
    no_human.user_prompt_id = None;
    no_human.user_prompt_created_at_ms = None;
    no_human.logical_turn_id = "no-human".into();
    store.insert(&no_human).await?;
    assert_eq!(
        store.get("alice", "no-human").await?,
        Some(no_human.clone())
    );
    let mut continuation = no_human.clone();
    continuation.id = "no-human-continuation".into();
    continuation.parent_id = Some(no_human.id.clone());
    continuation.prefix_digest = "continuation".into();
    store.insert(&continuation).await?;
    let mut changed_turn = continuation;
    changed_turn.id = "changed-turn".into();
    changed_turn.logical_turn_id = "different".into();
    assert!(store.insert(&changed_turn).await.is_err());
    assert!(
        store
            .insert(&record("missing", "alice", Some("absent"), 2, 2, ""))
            .await
            .is_err()
    );
    assert!(
        store
            .insert(&record("foreign", "bob", Some("root"), 2, 2, ""))
            .await
            .is_err()
    );
    let left = record("left", "alice", Some("root"), 2, 2, "hash-b");
    let right = record("right", "alice", Some("root"), 2, 2, "hash-b");
    let (a, b) = tokio::join!(store.insert(&left), store.insert(&right));
    a?;
    b?;
    assert!(
        store
            .insert(&record("backwards", "alice", Some("left"), 1, 2, ""))
            .await
            .is_err()
    );
    let mut negative_turn = record("negative-turn", "alice", None, 1, 1, "negative");
    negative_turn.user_turn_index = Some(-1);
    assert!(store.insert(&negative_turn).await.is_err());
    let mut earlier_turn = record("earlier-turn", "alice", Some("left"), 3, 3, "earlier");
    earlier_turn.user_turn_index = Some(1);
    assert!(store.insert(&earlier_turn).await.is_err());
    let mut unknown_turn = record("unknown-turn", "alice", Some("left"), 3, 3, "unknown");
    unknown_turn.user_turn_index = None;
    store.insert(&unknown_turn).await?;
    assert_eq!(
        store.get("alice", "unknown-turn").await?,
        Some(unknown_turn)
    );
    let compacted = record("compacted", "alice", Some("left"), 2, 1, "hash-c");
    let mut mismatched_root = record("mismatched-root", "alice", None, 1, 1, "mismatch");
    mismatched_root.logical_turn_id = "other".into();
    assert!(store.insert(&mismatched_root).await.is_err());
    let mut wrong_conversation = compacted.clone();
    wrong_conversation.id = "wrong-conversation".into();
    wrong_conversation.conversation_id = "other".into();
    assert!(store.insert(&wrong_conversation).await.is_err());
    let mut wrong_prompt = compacted.clone();
    wrong_prompt.id = "wrong-prompt".into();
    wrong_prompt.user_prompt_id = Some("other".into());
    assert!(store.insert(&wrong_prompt).await.is_err());
    store.insert(&compacted).await?;
    let other = record("other", "bob", None, 1, 1, "hash-a");
    store.insert(&other).await?;
    let prefixes = vec![
        (2, "hash-b".into()),
        (1, "hash-a".into()),
        (2, "hash-b".into()),
    ];
    let mut alice = store.find_prefixes("alice", "controls", &prefixes).await?;
    alice.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(alice, vec![left, right, root]);
    assert_eq!(
        store.find_prefixes("bob", "controls", &prefixes).await?,
        vec![other]
    );
    assert!(
        store
            .find_prefixes("alice", "other-controls", &prefixes)
            .await?
            .is_empty()
    );
    assert!(
        store
            .find_prefixes("alice", "controls", &[])
            .await?
            .is_empty()
    );
    // Large lookup crosses SQL parameter chunks; duplicate fingerprints do not duplicate records.
    let mut many: Vec<_> = (0..650).map(|n| (n, format!("unmatched-{n}"))).collect();
    many.extend(prefixes);
    assert_eq!(
        store.find_prefixes("alice", "controls", &many).await?.len(),
        3
    );
    Ok(())
}

async fn retention_contract(store: &dyn PromptAdmissionStore) -> anyhow::Result<()> {
    store.clear().await?;
    store
        .insert(&record("old-root", "alice", None, 1, 1, "a"))
        .await?;
    store
        .insert(&record("old-orphan", "bob", None, 1, 1, "b"))
        .await?;
    let cutoff = 200;
    let mut live = record("live", "alice", Some("old-root"), 2, 2, "c");
    live.created_at_ms = cutoff;
    store.insert(&live).await?;
    assert_eq!(store.prune_before(cutoff).await?, 1);
    assert!(store.get("alice", "old-root").await?.is_some());
    assert!(store.get("bob", "old-orphan").await?.is_none());
    assert_eq!(store.purge_principal("alice").await?, 2);
    assert!(store.get("alice", "live").await?.is_none());
    store
        .insert(&record("clear", "alice", None, 1, 1, "d"))
        .await?;
    let old_epoch = store.history_epoch().await?;
    assert_eq!(store.clear().await?, 1);
    assert_ne!(store.history_epoch().await?, old_epoch);
    assert!(
        store
            .insert_at_epoch(&record("stale", "alice", None, 1, 1, "stale"), old_epoch)
            .await
            .is_err()
    );
    assert!(store.get("alice", "stale").await?.is_none());
    Ok(())
}

#[tokio::test]
async fn memory_contract() -> anyhow::Result<()> {
    let storage = MemoryStorage::new(vec![], vec![], vec![]);
    let store = storage.prompt_admissions();
    contract(store.as_ref()).await?;
    retention_contract(store.as_ref()).await
}

#[tokio::test]
async fn sqlite_contract_and_reopen() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("admissions.sqlite");
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await?;
    crate::migrations::migrate_sqlite(&pool, None).await?;
    contract(
        SqliteStorage::from_pool(pool.clone())
            .prompt_admissions()
            .as_ref(),
    )
    .await?;
    pool.close().await;
    let reopened = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?;
    let store = SqliteStorage::from_pool(reopened).prompt_admissions();
    assert_eq!(
        store.get("alice", "left").await?,
        Some(record("left", "alice", Some("root"), 2, 2, "hash-b"))
    );
    let window = store
        .get("alice", "window")
        .await?
        .expect("persisted window");
    assert_eq!(
        window.context_window_id,
        Some(uuid::Uuid::parse_str(
            "00112233-4455-6677-8899-aabbccddeeff"
        )?)
    );
    assert_eq!(window.context_window_index, Some(3));
    retention_contract(store.as_ref()).await
}

#[tokio::test]
async fn postgres_contract_and_reopen() -> anyhow::Result<()> {
    let Ok(url) = std::env::var("DB_URL") else {
        return Ok(());
    };
    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await?;
    let schema = format!(
        "stravia_prompt_admissions_{}",
        uuid::Uuid::new_v4().simple()
    );
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await?;
    let options: sqlx::postgres::PgConnectOptions = url.parse()?;
    let options = options.options([("search_path", schema.as_str())]);
    let result = async {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options.clone())
            .await?;
        crate::migrations::migrate_postgres(&pool, None).await?;
        let storage = super::postgres::PostgresStorage::from_pool(pool.clone());
        contract(storage.prompt_admissions().as_ref()).await?;
        drop(storage);
        pool.close().await;
        let reopened = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        let store = super::postgres::PostgresStorage::from_pool(reopened).prompt_admissions();
        assert_eq!(
            store.get("alice", "left").await?,
            Some(record("left", "alice", Some("root"), 2, 2, "hash-b"))
        );
        let window = store
            .get("alice", "window")
            .await?
            .expect("persisted window");
        assert_eq!(
            window.context_window_id,
            Some(uuid::Uuid::parse_str(
                "00112233-4455-6677-8899-aabbccddeeff"
            )?)
        );
        assert_eq!(window.context_window_index, Some(3));
        retention_contract(store.as_ref()).await
    }
    .await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await?;
    result
}
