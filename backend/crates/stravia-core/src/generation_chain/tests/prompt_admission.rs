use super::*;
use crate::storage::{MemoryStorage, Storage};

fn tool_result() -> AiItem {
    AiItem {
        role: Role::User,
        content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
            tool_use_id: "call".into(),
            content: serde_json::json!("result"),
            content_kind: None,
            is_error: None,
            cache_control: None,
        }]),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    }
}

#[tokio::test]
async fn admission_survives_failed_execution_and_separates_principals() {
    let storage = MemoryStorage::new(vec![], vec![], vec![]);
    let chain = generation_chain()
        .await
        .with_prompt_admissions(storage.prompt_admissions());
    let owner = principal("alice");
    let mut first = chain
        .begin(owner.clone(), responses_request(vec![user_message("one")]))
        .await
        .unwrap();
    first.admit_user_prompt().await.unwrap();
    assert_eq!(first.user_prompt_index(), Some(1));
    let first_window = first.virtual_window().unwrap();
    assert_eq!(first_window.index, 0);
    // No stage or persist: the accepted input still anchors an extended history.
    let mut next = chain
        .begin(
            owner.clone(),
            responses_request(vec![user_message("one"), user_message("two")]),
        )
        .await
        .unwrap();
    next.admit_user_prompt().await.unwrap();
    assert_eq!(next.user_prompt_index(), Some(2));
    assert_eq!(next.virtual_window(), Some(first_window));
    let mut other = chain
        .begin(
            principal("bob"),
            responses_request(vec![user_message("one"), user_message("two")]),
        )
        .await
        .unwrap();
    other.admit_user_prompt().await.unwrap();
    assert_eq!(other.user_prompt_index(), Some(2));
    assert_ne!(other.virtual_window(), Some(first_window));
}

#[tokio::test]
async fn committed_forks_and_tool_only_continuation_follow_exact_parent() {
    let storage = MemoryStorage::new(vec![], vec![], vec![]);
    let backend = Arc::new(crate::turn_chain::test_store().await);
    let chain = GenerationChain::from_turn_chain(
        backend.clone(),
        None,
        crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024),
        crate::history_retention::HistoryRetention::for_tests(),
    )
    .with_prompt_admissions(storage.prompt_admissions());
    let owner = principal("alice");
    let question = user_message("question");
    let mut root = chain
        .begin(owner.clone(), responses_request(vec![question.clone()]))
        .await
        .unwrap();
    root.admit_user_prompt().await.unwrap();
    let root_metadata = root.user_prompt_metadata().unwrap();
    let root_window = root.virtual_window().unwrap();
    assert_eq!(root_metadata.context_window_id, Some(root_window.id));
    assert_eq!(root_metadata.context_window_index, Some(0));
    assert_eq!(root_metadata.user_turn_index, Some(1));
    assert_eq!(root_metadata.dialogue_prompt_index, Some(1));
    assert_eq!(
        uuid::Uuid::parse_str(&root_metadata.user_prompt_id)
            .unwrap()
            .to_string(),
        root_metadata.user_prompt_id
    );
    let mut answer = AiResponse::new("upstream", "model");
    answer.push_output_text("answer");
    assert!(root.stage(&mut answer, &generation_source(), None));
    root.persist().await.unwrap();
    // Live generations retain their proof after an older admission is swept.
    storage.prompt_admissions().clear().await.unwrap();
    let chain = GenerationChain::from_turn_chain(
        backend,
        None,
        crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024),
        crate::history_retention::HistoryRetention::for_tests(),
    )
    .with_prompt_admissions(storage.prompt_admissions());
    let base = vec![question, AiItem::output_text("answer")];
    let mut left_items = base.clone();
    left_items.push(user_message("left"));
    let mut right_items = base.clone();
    right_items.push(user_message("right"));
    let mut left = chain
        .begin(owner.clone(), responses_request(left_items))
        .await
        .unwrap();
    let mut right = chain
        .begin(owner.clone(), responses_request(right_items))
        .await
        .unwrap();
    left.admit_user_prompt().await.unwrap();
    right.admit_user_prompt().await.unwrap();
    assert_eq!(left.user_prompt_index(), Some(2));
    assert_eq!(right.user_prompt_index(), Some(2));
    assert_eq!(left.virtual_window(), Some(root_window));
    assert_eq!(right.virtual_window(), Some(root_window));
    let left_metadata = left.user_prompt_metadata().unwrap();
    let right_metadata = right.user_prompt_metadata().unwrap();
    assert_ne!(left_metadata.user_prompt_id, right_metadata.user_prompt_id);
    assert_eq!(left_metadata.user_turn_index, Some(2));
    assert_eq!(right_metadata.user_turn_index, Some(2));
    assert_eq!(
        left_metadata.client_conversation_id,
        root_metadata.client_conversation_id
    );
    assert_eq!(
        right_metadata.client_conversation_created_at_ms,
        root_metadata.client_conversation_created_at_ms
    );
    let mut tool_items = base;
    tool_items.push(tool_result());
    let mut tool = chain
        .begin(owner, responses_request(tool_items))
        .await
        .unwrap();
    tool.admit_user_prompt().await.unwrap();
    assert_eq!(tool.user_prompt_index(), Some(1));
    assert_eq!(tool.user_prompt_metadata(), Some(root_metadata));
}

#[tokio::test]
async fn identical_failed_siblings_start_an_unproven_isolated_continuation() {
    let storage = MemoryStorage::new(vec![], vec![], vec![]);
    let chain = generation_chain()
        .await
        .with_prompt_admissions(storage.prompt_admissions());
    let owner = principal("alice");
    let mut siblings = Vec::new();
    for _ in 0..2 {
        let mut first = chain
            .begin(owner.clone(), responses_request(vec![user_message("one")]))
            .await
            .unwrap();
        first.admit_user_prompt().await.unwrap();
        siblings.push((
            first.virtual_window().unwrap(),
            first.logical_turn().unwrap(),
        ));
    }
    let mut next = chain
        .begin(
            owner.clone(),
            responses_request(vec![user_message("one"), user_message("two")]),
        )
        .await
        .unwrap();
    next.admit_user_prompt().await.unwrap();
    assert_eq!(next.user_prompt_index(), None);
    assert_eq!(next.user_prompt_metadata(), None);
    for (window, turn) in siblings {
        assert_ne!(next.virtual_window(), Some(window));
        assert_ne!(next.logical_turn(), Some(turn));
    }
    let admission = next.prompt_admission.as_ref().unwrap();
    assert_eq!(admission.ordinal, 0);
    assert_eq!(admission.user_turn_index, None);
    assert_eq!(admission.dialogue_prompt_index, None);
    assert_eq!(admission.parent_id, None);

    // Extending the isolated history does not turn its unknown ordinal into a count.
    let mut later = chain
        .begin(
            owner,
            responses_request(vec![
                user_message("one"),
                user_message("two"),
                user_message("three"),
            ]),
        )
        .await
        .unwrap();
    later.admit_user_prompt().await.unwrap();
    assert_eq!(later.user_prompt_index(), None);
    assert_eq!(later.user_prompt_metadata(), None);
}

#[tokio::test]
async fn committed_explicit_parent_wins_over_duplicate_failed_prefixes() {
    let storage = MemoryStorage::new(vec![], vec![], vec![]);
    let chain = generation_chain()
        .await
        .with_prompt_admissions(storage.prompt_admissions());
    let owner = principal("explicit-over-ambiguous");
    let mut root = chain
        .begin(owner.clone(), responses_request(vec![user_message("one")]))
        .await
        .unwrap();
    root.admit_user_prompt().await.unwrap();
    let root_metadata = root.user_prompt_metadata().unwrap();
    let root_window = root.virtual_window().unwrap();
    let mut answer = AiResponse::new("upstream", "model");
    answer.push_output_text("answer");
    assert!(root.stage(&mut answer, &generation_source(), None));
    root.persist().await.unwrap();

    for _ in 0..2 {
        let mut failed = chain
            .begin(owner.clone(), responses_request(vec![user_message("one")]))
            .await
            .unwrap();
        failed.admit_user_prompt().await.unwrap();
    }
    let mut request = responses_request(vec![user_message("two")]);
    crate::router::stamp_previous_response_id(&mut request, root.id());
    let mut next = chain.begin(owner, request).await.unwrap();
    next.admit_user_prompt().await.unwrap();
    let metadata = next.user_prompt_metadata().unwrap();
    assert_eq!(metadata.user_prompt_index, 2);
    assert_eq!(
        metadata.client_conversation_id,
        root_metadata.client_conversation_id
    );
    assert_eq!(next.virtual_window(), Some(root_window));
}

#[tokio::test]
async fn mixed_user_text_and_tool_result_contains_one_user_prompt() {
    let storage = MemoryStorage::new(vec![], vec![], vec![]);
    let chain = generation_chain()
        .await
        .with_prompt_admissions(storage.prompt_admissions());
    let mut item = tool_result();
    let MessageContent::Blocks(blocks) = &mut item.content else {
        unreachable!()
    };
    blocks.push(ContentBlock::Text {
        text: Arc::new("new instruction".into()),
        cache_control: None,
    });
    let mut write = chain
        .begin(principal("mixed"), responses_request(vec![item]))
        .await
        .unwrap();
    write.admit_user_prompt().await.unwrap();
    assert_eq!(write.user_prompt_index(), Some(1));
    assert_eq!(
        write.user_prompt_metadata().unwrap().user_turn_index,
        Some(0)
    );
    assert_eq!(
        write.user_prompt_metadata().unwrap().dialogue_prompt_index,
        Some(0)
    );
}

#[tokio::test]
async fn legacy_parent_without_admission_starts_isolated_declared_lineage() {
    let storage = MemoryStorage::new(vec![], vec![], vec![]);
    let chain = generation_chain()
        .await
        .with_prompt_admissions(storage.prompt_admissions());
    let owner = principal("legacy");
    let mut old = chain
        .begin(owner.clone(), responses_request(vec![user_message("old")]))
        .await
        .unwrap();
    // A generation written before prompt admission has no ordinal proof.
    let mut answer = AiResponse::new("upstream", "model");
    answer.push_output_text("answer");
    assert!(old.stage(&mut answer, &generation_source(), None));
    old.persist().await.unwrap();
    let mut child = chain
        .begin(
            owner,
            responses_request(vec![
                user_message("old"),
                AiItem::output_text("answer"),
                user_message("new"),
            ]),
        )
        .await
        .unwrap();
    child.admit_user_prompt().await.unwrap();
    let metadata = child.user_prompt_metadata().unwrap();
    assert_eq!(metadata.user_prompt_index, 2);
    assert_eq!(metadata.user_turn_index, Some(2));
    assert_eq!(metadata.client_conversation_id, metadata.user_prompt_id);
}

#[tokio::test]
async fn legacy_explicit_parent_does_not_count_only_the_suffix() {
    let storage = MemoryStorage::new(vec![], vec![], vec![]);
    let chain = generation_chain()
        .await
        .with_prompt_admissions(storage.prompt_admissions());
    let owner = principal("legacy-explicit");
    let mut old = chain
        .begin(owner.clone(), responses_request(vec![user_message("old")]))
        .await
        .unwrap();
    let mut answer = AiResponse::new("legacy-answer", "model");
    answer.push_output_text("answer");
    assert!(old.stage(&mut answer, &generation_source(), None));
    old.persist().await.unwrap();

    let mut suffix = responses_request(vec![user_message("new")]);
    crate::router::stamp_previous_response_id(&mut suffix, old.id());
    let prepared = chain
        .prepare_compaction(owner.clone(), suffix.clone())
        .await
        .unwrap();
    assert_eq!(
        prepared.user_prompt_metadata_for_admission(uuid::Uuid::new_v4(), 42),
        None
    );
    let mut child = chain.begin(owner.clone(), suffix).await.unwrap();
    child.admit_user_prompt().await.unwrap();
    assert_eq!(child.user_prompt_index(), None);
    assert_eq!(child.user_prompt_metadata(), None);
    let mut response = AiResponse::new("child-answer", "model");
    response.push_output_text("done");
    assert!(child.stage(&mut response, &generation_source(), None));
    child.persist().await.unwrap();
    let mut next = responses_request(vec![user_message("again")]);
    crate::router::stamp_previous_response_id(&mut next, child.id());
    let mut next = chain.begin(owner, next).await.unwrap();
    next.admit_user_prompt().await.unwrap();
    assert_eq!(next.user_prompt_index(), None);
    assert_eq!(next.user_prompt_metadata(), None);
}

#[tokio::test]
async fn standalone_compaction_binds_isolated_and_proven_prompt_positions() {
    let storage = MemoryStorage::new(vec![], vec![], vec![]);
    let chain = generation_chain()
        .await
        .with_prompt_admissions(storage.prompt_admissions());
    let owner = principal("compact-position");
    let admission = uuid::Uuid::new_v4();
    let prepared = chain
        .prepare_compaction(
            owner.clone(),
            responses_request(vec![user_message("one"), user_message("two")]),
        )
        .await
        .unwrap();
    let root_id = uuid::Uuid::new_v4();
    assert_eq!(
        prepared.source_window_for_admission(root_id),
        VirtualWindow {
            id: root_id,
            index: 0
        }
    );
    let isolated = prepared
        .user_prompt_metadata_for_admission(admission, 42)
        .unwrap();
    assert_eq!(
        prepared.logical_turn_for_admission(admission, 42),
        Some(LogicalTurn {
            id: admission,
            created_at_ms: 42
        })
    );
    assert_eq!(isolated.user_prompt_index, 2);
    assert_eq!(isolated.user_turn_index, Some(2));
    assert_eq!(isolated.dialogue_prompt_index, Some(2));
    assert_eq!(isolated.user_prompt_id, admission.to_string());
    assert_eq!(isolated.client_conversation_created_at_ms, 42);

    let mut root = chain
        .begin(owner.clone(), responses_request(vec![user_message("one")]))
        .await
        .unwrap();
    root.admit_user_prompt().await.unwrap();
    let root_metadata = root.user_prompt_metadata().unwrap();
    let mut answer = AiResponse::new("upstream", "model");
    answer.push_output_text("answer");
    assert!(root.stage(&mut answer, &generation_source(), None));
    root.persist().await.unwrap();
    let history = vec![user_message("one"), AiItem::output_text("answer")];
    let pure = chain
        .prepare_compaction(owner.clone(), responses_request(history.clone()))
        .await
        .unwrap();
    assert_eq!(
        pure.logical_turn_for_admission(admission, 42),
        root.logical_turn()
    );
    assert_eq!(
        pure.source_window_for_admission(uuid::Uuid::new_v4()),
        root.virtual_window().unwrap()
    );
    assert_eq!(
        pure.user_prompt_metadata_for_admission(admission, 42),
        Some(root_metadata.clone())
    );
    let mut appended = history;
    appended.push(user_message("two"));
    let advanced = chain
        .prepare_compaction(owner, responses_request(appended))
        .await
        .unwrap();
    assert_eq!(
        advanced.logical_turn_for_admission(admission, 42),
        Some(LogicalTurn {
            id: admission,
            created_at_ms: 42
        })
    );
    let advanced = advanced
        .user_prompt_metadata_for_admission(admission, 42)
        .unwrap();
    assert_eq!(advanced.user_prompt_index, 2);
    assert_eq!(advanced.user_turn_index, Some(2));
    assert_eq!(advanced.dialogue_prompt_index, Some(2));
    assert_eq!(advanced.user_prompt_id, admission.to_string());
    assert_eq!(
        advanced.client_conversation_id,
        root_metadata.client_conversation_id
    );
}

#[tokio::test]
async fn atomic_admissions_bind_forks_and_isolate_unproven_legacy_history() {
    let pool = crate::test_support::migrated_sqlite_pool().await.unwrap();
    let storage = crate::storage::SqliteStorage::from_pool(pool.clone());
    let chain = GenerationChain::from_turn_chain(
        Arc::new(crate::turn_chain::SqlTurnChainStore::sqlite(
            pool.clone(),
            crate::history_retention::HistoryRetention::for_tests(),
        )),
        None,
        crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024),
        crate::history_retention::HistoryRetention::for_tests(),
    )
    .with_prompt_admissions(storage.prompt_admissions())
    .with_atomic_state(storage.vendor_plugins().clone());
    let owner = principal("atomic-owner");
    let mut root = chain
        .begin(
            owner.clone(),
            responses_request(vec![user_message("question")]),
        )
        .await
        .unwrap();
    root.admit_user_prompt().await.unwrap();
    let root_context = root.atomic_conversation_context().unwrap();
    assert_eq!(
        root.history_boundary_candidate().unwrap().source_item_gap,
        0
    );
    assert_eq!(
        root.history_boundary_candidate()
            .unwrap()
            .source_admission_id,
        root_context.admission_id
    );
    let mut answer = AiResponse::new("upstream", "model");
    answer.push_output_text("answer");
    assert!(root.stage(&mut answer, &generation_source(), None));
    root.persist().await.unwrap();
    let history = vec![user_message("question"), AiItem::output_text("answer")];
    let mut left_history = history.clone();
    left_history.push(user_message("left"));
    let mut right_history = history;
    right_history.push(user_message("right"));
    let mut left = chain
        .begin(owner.clone(), responses_request(left_history))
        .await
        .unwrap();
    let mut right = chain
        .begin(owner, responses_request(right_history))
        .await
        .unwrap();
    left.admit_user_prompt().await.unwrap();
    right.admit_user_prompt().await.unwrap();
    assert_eq!(
        left.history_boundary_candidate().unwrap().source_item_gap,
        2
    );
    assert_eq!(
        right.history_boundary_candidate().unwrap().source_item_gap,
        2
    );
    let left = left.atomic_conversation_context().unwrap();
    let right = right.atomic_conversation_context().unwrap();
    assert_ne!(left.admission_id, right.admission_id);
    assert_ne!(left.atomic_lease_id, right.atomic_lease_id);

    let owner = principal("failed-owner");
    let mut failed = chain
        .begin(
            owner.clone(),
            responses_request(vec![user_message("unanswered")]),
        )
        .await
        .unwrap();
    failed.admit_user_prompt().await.unwrap();
    let mut continuation = chain
        .begin(
            owner,
            responses_request(vec![user_message("unanswered"), user_message("next")]),
        )
        .await
        .unwrap();
    continuation.admit_user_prompt().await.unwrap();
    assert_eq!(
        continuation
            .history_boundary_candidate()
            .unwrap()
            .source_item_gap,
        1
    );
    let continuation = continuation.atomic_conversation_context().unwrap();
    assert_ne!(
        continuation.admission_id,
        failed.atomic_conversation_context().unwrap().admission_id
    );

    // A pre-upgrade admission carries no atomic lineage marker, so the same
    // visible history starts a fresh virtual lineage instead of inventing state.
    let legacy_chain = GenerationChain::from_turn_chain(
        Arc::new(crate::turn_chain::SqlTurnChainStore::sqlite(
            pool,
            crate::history_retention::HistoryRetention::for_tests(),
        )),
        None,
        crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024),
        crate::history_retention::HistoryRetention::for_tests(),
    )
    .with_prompt_admissions(storage.prompt_admissions());
    let owner = principal("old-admission");
    let mut old = legacy_chain
        .begin(owner.clone(), responses_request(vec![user_message("old")]))
        .await
        .unwrap();
    old.admit_user_prompt().await.unwrap();
    let old_identity = old.user_prompt_metadata().unwrap().client_conversation_id;
    let mut rebase = chain
        .begin(
            owner,
            responses_request(vec![user_message("old"), user_message("new")]),
        )
        .await
        .unwrap();
    rebase.admit_user_prompt().await.unwrap();
    let metadata = rebase.user_prompt_metadata().unwrap();
    assert_eq!(metadata.user_prompt_index, 2);
    assert_ne!(metadata.client_conversation_id, old_identity);
}

#[tokio::test]
async fn ambiguous_failed_admissions_do_not_checkpoint_or_fork_either_sibling() {
    let pool = crate::test_support::migrated_sqlite_pool().await.unwrap();
    let storage = crate::storage::SqliteStorage::from_pool(pool.clone());
    let chain = GenerationChain::from_turn_chain(
        Arc::new(crate::turn_chain::SqlTurnChainStore::sqlite(
            pool.clone(),
            crate::history_retention::HistoryRetention::for_tests(),
        )),
        None,
        crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024),
        crate::history_retention::HistoryRetention::for_tests(),
    )
    .with_prompt_admissions(storage.prompt_admissions())
    .with_atomic_state(storage.vendor_plugins().clone());
    let owner = principal("ambiguous-atomic");
    let mut siblings = Vec::new();
    for _ in 0..2 {
        let mut failed = chain
            .begin(owner.clone(), responses_request(vec![user_message("one")]))
            .await
            .unwrap();
        failed.admit_user_prompt().await.unwrap();
        siblings.push(failed);
    }
    let mut next = chain
        .begin(
            owner,
            responses_request(vec![user_message("one"), user_message("two")]),
        )
        .await
        .unwrap();
    next.admit_user_prompt().await.unwrap();
    assert_eq!(next.user_prompt_metadata(), None);
    assert_eq!(
        next.history_boundary_candidate().unwrap().source_item_gap,
        0
    );
    let next_context = next.atomic_conversation_context().unwrap();
    for sibling in &siblings {
        assert_ne!(
            next_context.admission_id,
            sibling.atomic_conversation_context().unwrap().admission_id
        );
        assert_ne!(next.virtual_window(), sibling.virtual_window());
    }
    // Forking a failed admission first creates its checkpoint. Neither sibling
    // was checkpointed, so the new lineage cannot contain either sibling's cells.
    let checkpoint_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM vendor_atomic_checkpoints")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(checkpoint_count, 0);
}

#[tokio::test]
async fn no_user_generation_retains_window_through_delivery() {
    let storage = MemoryStorage::new(vec![], vec![], vec![]);
    let chain = generation_chain()
        .await
        .with_prompt_admissions(storage.prompt_admissions());
    let owner = principal("no-user-window");
    let mut root = chain
        .begin(owner.clone(), responses_request(vec![]))
        .await
        .unwrap();
    root.admit_user_prompt().await.unwrap();
    let window = root.virtual_window().unwrap();
    let root_turn = root.logical_turn().unwrap();
    assert_eq!(window.index, 0);
    assert!(root.user_prompt_metadata().is_none());
    let mut answer = AiResponse::new("upstream", "model");
    answer.push_output_text("answer");
    assert!(root.stage(&mut answer, &generation_source(), None));
    root.persist().await.unwrap();
    storage.prompt_admissions().clear().await.unwrap();
    let compact = chain
        .prepare_compaction(
            owner.clone(),
            responses_request(vec![AiItem::output_text("answer")]),
        )
        .await
        .unwrap();
    assert_eq!(
        compact.logical_turn_for_admission(uuid::Uuid::new_v4(), 42),
        Some(root_turn)
    );
    let compact_with_user = chain
        .prepare_compaction(
            owner.clone(),
            responses_request(vec![AiItem::output_text("answer"), user_message("first")]),
        )
        .await
        .unwrap();
    let compact_id = uuid::Uuid::new_v4();
    assert_eq!(
        compact_with_user.logical_turn_for_admission(compact_id, 42),
        Some(LogicalTurn {
            id: compact_id,
            created_at_ms: 42
        })
    );
    assert_eq!(
        compact_with_user
            .user_prompt_metadata_for_admission(compact_id, 42)
            .unwrap()
            .client_conversation_id,
        root_turn.id.to_string()
    );
    let mut continuation = chain
        .begin(
            owner.clone(),
            responses_request(vec![AiItem::output_text("answer"), tool_result()]),
        )
        .await
        .unwrap();
    continuation.admit_user_prompt().await.unwrap();
    assert_eq!(continuation.logical_turn(), Some(root_turn));
    assert_eq!(continuation.user_prompt_index(), None);
    let mut child = chain
        .begin(
            owner,
            responses_request(vec![AiItem::output_text("answer"), user_message("first")]),
        )
        .await
        .unwrap();
    child.admit_user_prompt().await.unwrap();
    assert_eq!(child.virtual_window(), Some(window));
    assert_ne!(child.logical_turn(), Some(root_turn));
    assert_eq!(
        child.logical_turn().unwrap().id.to_string(),
        child.user_prompt_metadata().unwrap().user_prompt_id
    );
    assert_eq!(
        child.user_prompt_metadata().unwrap().context_window_id,
        Some(window.id)
    );
}

#[tokio::test]
async fn failed_no_human_admission_anchors_following_turns() {
    let storage = MemoryStorage::new(vec![], vec![], vec![]);
    let chain = generation_chain()
        .await
        .with_prompt_admissions(storage.prompt_admissions());
    let owner = principal("failed-no-human");
    let mut first = chain
        .begin(owner.clone(), responses_request(vec![tool_result()]))
        .await
        .unwrap();
    first.admit_user_prompt().await.unwrap();
    let root_turn = first.logical_turn().unwrap();
    assert_eq!(first.user_prompt_index(), None);
    let mut continuation = chain
        .begin(
            owner.clone(),
            responses_request(vec![tool_result(), AiItem::output_text("continued")]),
        )
        .await
        .unwrap();
    continuation.admit_user_prompt().await.unwrap();
    assert_eq!(continuation.logical_turn(), Some(root_turn));
    let mut human = chain
        .begin(
            owner,
            responses_request(vec![tool_result(), user_message("first human")]),
        )
        .await
        .unwrap();
    human.admit_user_prompt().await.unwrap();
    let metadata = human.user_prompt_metadata().unwrap();
    assert_eq!(metadata.user_prompt_index, 1);
    assert_eq!(metadata.client_conversation_id, root_turn.id.to_string());
    assert_ne!(human.logical_turn(), Some(root_turn));
    assert_eq!(
        human.logical_turn().unwrap().id.to_string(),
        metadata.user_prompt_id
    );
}

#[test]
fn history_boundaries_preserve_gap_identity_and_reject_intra_item_merge() {
    use stravia_runtime_contract::protocol::ir::ToolCall;
    let window = VirtualWindow::root();
    let admission = uuid::Uuid::new_v4();
    let call = ToolCall {
        id: "call_1".into(),
        name: "lookup".into(),
        arguments: "{\"value\":1}".into(),
    };
    let split = responses_request(vec![
        AiItem::thinking("reasoning", Some("opaque".into())),
        AiItem::output_text("answer"),
        AiItem::function_call(call.clone()),
    ]);
    let retained = history_boundary_candidate(admission, window, &split, 1).unwrap();
    let current = history_boundary_candidate(admission, window, &split, 3).unwrap();
    assert_ne!(retained.id, current.id);
    let repeated = history_boundary_candidate(admission, window, &split, 1).unwrap();
    assert_ne!(retained.id, repeated.id);
    let map = project_history_boundaries(&split, &current, &[retained.clone(), repeated.clone()])
        .unwrap();
    assert_eq!(map.boundaries[0].id, retained.id.to_string());
    assert_eq!(map.boundaries[1].id, repeated.id.to_string());
    assert_eq!(
        (map.boundaries[0].item_index, map.boundaries[1].item_index),
        (1, 1)
    );
    assert_eq!(map.boundaries[2].item_index, 3);

    let merged = responses_request(vec![AiItem {
        role: Role::Assistant,
        content: MessageContent::Blocks(vec![
            ContentBlock::Thinking {
                thinking: "reasoning".into(),
                signature: Some("opaque".into()),
            },
            ContentBlock::Text {
                text: "answer".to_owned().into(),
                cache_control: None,
            },
            ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "lookup".into(),
                input: serde_json::json!({"value": 1}),
                cache_control: None,
            },
        ]),
        tool_calls: Some(vec![call]),
        tool_call_id: None,
        meta: None,
    }]);
    assert!(
        stravia_runtime_contract::protocol::ir::canonical::history_items_equal(
            &split.items,
            &merged.items
        )
    );
    let merged_current = history_boundary_candidate(admission, window, &merged, 1).unwrap();
    assert!(project_history_boundaries(&merged, &merged_current, &[retained]).is_none());
}

struct PausedAdmissionStore {
    inner: Arc<dyn crate::storage::PromptAdmissionStore>,
    pause_before_epoch: bool,
    reached: Arc<tokio::sync::Notify>,
    resume: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl crate::storage::PromptAdmissionStore for PausedAdmissionStore {
    fn memory_history_lock(&self) -> Option<Arc<tokio::sync::RwLock<()>>> {
        self.inner.memory_history_lock()
    }
    async fn history_epoch(&self) -> anyhow::Result<i64> {
        if self.pause_before_epoch {
            self.reached.notify_one();
            self.resume.notified().await;
        }
        self.inner.history_epoch().await
    }
    async fn insert_at_epoch(
        &self,
        record: &crate::storage::PromptAdmission,
        epoch: i64,
    ) -> anyhow::Result<()> {
        if !self.pause_before_epoch {
            self.reached.notify_one();
            self.resume.notified().await;
        }
        self.inner.insert_at_epoch(record, epoch).await
    }
    async fn get(
        &self,
        principal: &str,
        id: &str,
    ) -> anyhow::Result<Option<crate::storage::PromptAdmission>> {
        self.inner.get(principal, id).await
    }
    async fn find_prefixes(
        &self,
        principal: &str,
        controls: &str,
        prefixes: &[(i64, String)],
    ) -> anyhow::Result<Vec<crate::storage::PromptAdmission>> {
        self.inner
            .find_prefixes(principal, controls, prefixes)
            .await
    }
    async fn prune_before(&self, cutoff: i64) -> anyhow::Result<u64> {
        self.inner.prune_before(cutoff).await
    }
    async fn purge_principal(&self, principal: &str) -> anyhow::Result<u64> {
        self.inner.purge_principal(principal).await
    }
    async fn clear(&self) -> anyhow::Result<u64> {
        self.inner.clear().await
    }
}

#[tokio::test]
async fn memory_clear_serializes_before_fork_and_between_fork_and_insert() {
    for pause_before_epoch in [true, false] {
        let storage = MemoryStorage::new(vec![], vec![], vec![]);
        let inner = storage.prompt_admissions();
        let plugins = storage.vendor_plugins().clone();
        let reached = Arc::new(tokio::sync::Notify::new());
        let resume = Arc::new(tokio::sync::Notify::new());
        let paused: Arc<dyn crate::storage::PromptAdmissionStore> =
            Arc::new(PausedAdmissionStore {
                inner: inner.clone(),
                pause_before_epoch,
                reached: reached.clone(),
                resume: resume.clone(),
            });
        let chain = generation_chain()
            .await
            .with_prompt_admissions(paused)
            .with_atomic_state(plugins.clone());
        let owner = principal("memory-clear-race");
        let mut write = chain
            .begin(
                owner.clone(),
                responses_request(vec![user_message("before clear")]),
            )
            .await
            .unwrap();
        let admission = tokio::spawn(async move {
            write.admit_user_prompt().await.unwrap();
            write
        });
        reached.notified().await;
        let clear_plugins = plugins.clone();
        let clear_store = inner.clone();
        let clear = tokio::spawn(async move {
            clear_plugins
                .clear_conversation_atomic_state(clear_store.as_ref())
                .await
                .unwrap();
        });
        tokio::task::yield_now().await;
        assert!(!clear.is_finished());
        resume.notify_one();
        let admitted = admission.await.unwrap();
        let admitted_id = admitted.prompt_admission.as_ref().unwrap().id.clone();
        clear.await.unwrap();
        assert!(
            inner
                .get(&owner.continuation_key(), &admitted_id)
                .await
                .unwrap()
                .is_none()
        );
        let fresh = generation_chain()
            .await
            .with_prompt_admissions(inner.clone())
            .with_atomic_state(plugins);
        let mut next = fresh
            .begin(owner, responses_request(vec![user_message("after clear")]))
            .await
            .unwrap();
        next.admit_user_prompt().await.unwrap();
        assert_eq!(next.user_prompt_index(), Some(1));
        assert_ne!(next.logical_turn(), admitted.logical_turn());
    }
}

#[test]
fn verified_window_proof_is_deterministic_and_scoped() {
    let owner = principal("window-owner");
    let source = VirtualWindow::root();
    let mut boundary = crate::compaction::ResolvedCompaction {
        logical_turn: None,
        source_history: Some(source_history_proof(&responses_request(vec![
            user_message("source"),
        ]))),
        source_window: Some(source),
        user_prompt_metadata: None,
        atomic_checkpoint_id: None,
        record_ids: vec!["registration-one".into()],
        source_generation_id: Some("generation".into()),
        operation_id: "operation-one".into(),
        target: crate::compaction::CompactionTarget {
            target_key: "target".into(),
            namespace: "namespace".into(),
            model: "model".into(),
            protocol: "protocol".into(),
        },
        window: vec![user_message("full native replacement")],
    };
    let first = source.install(&owner, &boundary).unwrap();
    assert_eq!(first.index, 1);
    assert_ne!(first.id, source.id);
    boundary.record_ids = vec!["equivalent-registration".into()];
    boundary.atomic_checkpoint_id = Some(uuid::Uuid::new_v4());
    boundary.operation_id = "equivalent-operation".into();
    assert_eq!(source.install(&owner, &boundary).unwrap(), first);
    assert_ne!(
        source
            .install(&principal("other-owner"), &boundary)
            .unwrap(),
        first
    );
    boundary.window.push(user_message("different replacement"));
    assert_ne!(source.install(&owner, &boundary).unwrap(), first);
    assert!(first.install(&owner, &boundary).is_err());
}
