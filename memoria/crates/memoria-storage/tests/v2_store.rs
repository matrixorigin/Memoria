use async_trait::async_trait;
use chrono::{Duration, Utc};
use memoria_core::{interfaces::MemoryStore, MemoriaError, Memory, MemoryType, TrustTier};
use memoria_storage::{
    EntityV2Filter, ExpandLevel, FocusV2Input, LinkDirection, ListV2Filter, MemoryV2JobEnricher,
    MemoryV2JobsRequest, MemoryV2LinksRequest, MemoryV2RelatedRequest, MemoryV2RememberInput,
    MemoryV2UpdateInput, ProfileV2Filter, RecallV2Request, ReflectV2Filter, SqlMemoryStore,
    V2DerivedViews, V2LinkCandidate, V2LinkSuggestion,
};
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

fn test_dim() -> usize {
    std::env::var("EMBEDDING_DIM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024)
}

fn dim_vec(idx: usize, val: f32) -> Vec<f32> {
    let mut v = vec![0.0f32; test_dim()];
    v[idx] = val;
    v
}

async fn setup() -> (SqlMemoryStore, memoria_storage::MemoryV2Store, String) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria_test".to_string());
    let store = SqlMemoryStore::connect(&url, test_dim(), Uuid::new_v4().to_string())
        .await
        .expect("connect");
    store.migrate().await.expect("migrate");
    let user_id = format!("v2_test_{}", Uuid::new_v4().simple());
    let v2 = store.v2_store();
    (store, v2, user_id)
}

fn is_schema_visibility_error(message: &str) -> bool {
    message.contains("Unknown database")
        || message.contains("1049 (HY000)")
        || message.contains("does not exist")
        || message.contains("SQL parser error: table")
}

fn is_transient_table_ready_error(message: &str) -> bool {
    is_schema_visibility_error(message)
        || message.contains("connection reset by peer")
        || message.contains("broken pipe")
        || message.contains("driver: bad connection")
        || message.contains("expected to read 4 bytes")
        || message.contains("got 0 bytes at EOF")
}

async fn ensure_user_tables_ready(
    store: &SqlMemoryStore,
    v2: &memoria_storage::MemoryV2Store,
    user_id: &str,
) -> memoria_storage::MemoryV2TableFamily {
    let mut last_error = None;
    for attempt in 0..10 {
        match v2.ensure_user_tables(user_id).await {
            Ok(family) => {
                let probe = format!("SELECT COUNT(*) AS cnt FROM {}", family.heads_table);
                match sqlx::query(&probe).fetch_one(store.pool()).await {
                    Ok(_) => return family,
                    Err(err) if attempt < 9 && is_transient_table_ready_error(&err.to_string()) => {
                        last_error = Some(err.to_string());
                        tokio::time::sleep(tokio::time::Duration::from_millis(
                            50 * (attempt as u64 + 1),
                        ))
                        .await;
                    }
                    Err(err) => panic!("ensure tables visible: {err:?}"),
                }
            }
            Err(err) if attempt < 9 && is_transient_table_ready_error(&err.to_string()) => {
                last_error = Some(err.to_string());
                tokio::time::sleep(tokio::time::Duration::from_millis(
                    50 * (attempt as u64 + 1),
                ))
                .await;
            }
            Err(err) => panic!("ensure tables: {err:?}"),
        }
    }
    panic!(
        "ensure tables ready: {}",
        last_error.unwrap_or_else(|| "unknown transient table readiness error".to_string())
    );
}

async fn wait_for_current_jobs_done(
    store: &SqlMemoryStore,
    v2: &memoria_storage::MemoryV2Store,
    family: &memoria_storage::MemoryV2TableFamily,
    user_id: &str,
    first_memory_id: &str,
    second_memory_id: &str,
    enricher: Option<&dyn MemoryV2JobEnricher>,
    expected_done: i64,
) -> i64 {
    let mut done = 0i64;
    for _ in 0..30 {
        v2.process_user_pending_jobs_with_enricher_pass(user_id, 10, enricher)
            .await
            .expect("process pending jobs");
        done = sqlx::query(&format!(
            "SELECT COUNT(*) AS cnt FROM {} WHERE status = 'done' AND aggregate_id IN (?, ?)",
            family.jobs_table
        ))
        .bind(first_memory_id)
        .bind(second_memory_id)
        .fetch_one(store.pool())
        .await
        .expect("current done jobs")
        .try_get::<i64, _>("cnt")
        .unwrap_or_default();
        if done >= expected_done {
            break;
        }
    }
    done
}

#[tokio::test]
async fn test_v2_remember_list_expand_forget_and_queue_jobs() {
    let (store, v2, user_id) = setup().await;
    let family = ensure_user_tables_ready(&store, &v2, &user_id).await;

    let remembered = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Rust ownership and borrowing keep systems code safe.".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-v2".to_string()),
                importance: Some(0.7),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec![
                    "Rust".to_string(),
                    "systems".to_string(),
                    "rust".to_string(),
                ],
                source: Some(json!({
                    "kind": "chat",
                    "app": "copilot",
                    "message_id": "msg-1",
                    "turn_id": "turn-1"
                })),
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember");

    assert_eq!(
        remembered.abstract_text,
        "Rust ownership and borrowing keep systems code safe."
    );
    assert!(!remembered.has_overview);
    assert!(!remembered.has_detail);

    let jobs_row = sqlx::query(&format!(
        "SELECT COUNT(*) AS cnt FROM {} WHERE aggregate_id = ?",
        family.jobs_table
    ))
    .bind(&remembered.memory_id)
    .fetch_one(store.pool())
    .await
    .expect("job count");
    assert_eq!(jobs_row.try_get::<i64, _>("cnt").unwrap_or_default(), 2);

    let tags_row = sqlx::query(&format!(
        "SELECT COUNT(*) AS cnt FROM {} WHERE memory_id = ?",
        family.tags_table
    ))
    .bind(&remembered.memory_id)
    .fetch_one(store.pool())
    .await
    .expect("tag count");
    assert_eq!(tags_row.try_get::<i64, _>("cnt").unwrap_or_default(), 2);

    let listed = v2
        .list(
            &user_id,
            ListV2Filter {
                limit: 10,
                cursor: None,
                memory_type: Some(MemoryType::Semantic),
                session_id: Some("sess-v2".to_string()),
            },
        )
        .await
        .expect("list");
    assert_eq!(listed.items.len(), 1);
    assert_eq!(listed.items[0].memory_id, remembered.memory_id);
    assert_eq!(listed.items[0].session_id.as_deref(), Some("sess-v2"));

    let expanded = v2
        .expand(&user_id, &remembered.memory_id, ExpandLevel::Detail)
        .await
        .expect("expand");
    assert_eq!(expanded.abstract_text, remembered.abstract_text);
    assert!(expanded.overview_text.is_none());
    assert!(expanded.detail_text.is_none());

    v2.forget(&user_id, &remembered.memory_id, Some("cleanup"), "tester")
        .await
        .expect("forget");

    let listed_after_forget = v2
        .list(
            &user_id,
            ListV2Filter {
                limit: 10,
                cursor: None,
                memory_type: None,
                session_id: None,
            },
        )
        .await
        .expect("list after forget");
    assert!(listed_after_forget.items.is_empty());

    let err = v2
        .expand(&user_id, &remembered.memory_id, ExpandLevel::Overview)
        .await
        .expect_err("forgotten memory should not expand");
    assert!(matches!(err, MemoriaError::NotFound(_)));
}

#[tokio::test]
async fn test_v2_batch_remember_and_forget() {
    let (store, v2, user_id) = setup().await;
    let family = ensure_user_tables_ready(&store, &v2, &user_id).await;

    let remembered = v2
        .remember_batch(
            &user_id,
            vec![
                MemoryV2RememberInput {
                    content: "Rust systems handbook".to_string(),
                    memory_type: MemoryType::Semantic,
                    session_id: Some("sess-batch".to_string()),
                    importance: Some(0.4),
                    trust_tier: Some(TrustTier::T2Curated),
                    tags: vec![
                        "Rust".to_string(),
                        "systems".to_string(),
                        "rust".to_string(),
                    ],
                    source: None,
                    embedding: Some(dim_vec(0, 1.0)),
                    actor: "tester".to_string(),
                },
                MemoryV2RememberInput {
                    content: "Python data handbook".to_string(),
                    memory_type: MemoryType::Semantic,
                    session_id: Some("sess-batch".to_string()),
                    importance: Some(0.4),
                    trust_tier: Some(TrustTier::T2Curated),
                    tags: vec!["python".to_string(), "data".to_string()],
                    source: None,
                    embedding: Some(dim_vec(1, 1.0)),
                    actor: "tester".to_string(),
                },
                MemoryV2RememberInput {
                    content: "Infra runbook for deployments".to_string(),
                    memory_type: MemoryType::Procedural,
                    session_id: Some("sess-batch".to_string()),
                    importance: Some(0.6),
                    trust_tier: Some(TrustTier::T3Inferred),
                    tags: vec!["infra".to_string()],
                    source: None,
                    embedding: Some(dim_vec(2, 1.0)),
                    actor: "tester".to_string(),
                },
            ],
        )
        .await
        .expect("batch remember");
    assert_eq!(remembered.len(), 3);
    assert!(remembered
        .iter()
        .all(|item| !item.has_overview && !item.has_detail));

    let jobs_row = sqlx::query(&format!(
        "SELECT COUNT(*) AS cnt FROM {} WHERE aggregate_id IN (?, ?, ?)",
        family.jobs_table
    ))
    .bind(&remembered[0].memory_id)
    .bind(&remembered[1].memory_id)
    .bind(&remembered[2].memory_id)
    .fetch_one(store.pool())
    .await
    .expect("batch job count");
    assert_eq!(jobs_row.try_get::<i64, _>("cnt").unwrap_or_default(), 6);

    let tags_row = sqlx::query(&format!(
        "SELECT COUNT(*) AS cnt FROM {} WHERE memory_id = ?",
        family.tags_table
    ))
    .bind(&remembered[0].memory_id)
    .fetch_one(store.pool())
    .await
    .expect("batch tag count");
    assert_eq!(tags_row.try_get::<i64, _>("cnt").unwrap_or_default(), 2);

    let listed = v2
        .list(
            &user_id,
            ListV2Filter {
                limit: 10,
                cursor: None,
                memory_type: None,
                session_id: Some("sess-batch".to_string()),
            },
        )
        .await
        .expect("list batch");
    assert_eq!(listed.items.len(), 3);

    let forgotten = v2
        .forget_batch(
            &user_id,
            &[
                remembered[0].memory_id.clone(),
                remembered[2].memory_id.clone(),
            ],
            Some("cleanup"),
            "tester",
        )
        .await
        .expect("batch forget");
    assert_eq!(
        forgotten,
        vec![
            remembered[0].memory_id.clone(),
            remembered[2].memory_id.clone()
        ]
    );

    let listed_after_forget = v2
        .list(
            &user_id,
            ListV2Filter {
                limit: 10,
                cursor: None,
                memory_type: None,
                session_id: Some("sess-batch".to_string()),
            },
        )
        .await
        .expect("list after batch forget");
    assert_eq!(listed_after_forget.items.len(), 1);
    assert_eq!(
        listed_after_forget.items[0].memory_id,
        remembered[1].memory_id
    );

    let forgotten_heads = sqlx::query(&format!(
        "SELECT COUNT(*) AS cnt FROM {} WHERE memory_id IN (?, ?) AND forgotten_at IS NOT NULL",
        family.heads_table
    ))
    .bind(&remembered[0].memory_id)
    .bind(&remembered[2].memory_id)
    .fetch_one(store.pool())
    .await
    .expect("forgotten heads");
    assert_eq!(
        forgotten_heads.try_get::<i64, _>("cnt").unwrap_or_default(),
        2
    );
}

#[tokio::test]
async fn test_v2_recall_focus_reorders_results_and_tracks_access() {
    let (store, v2, user_id) = setup().await;
    let family = ensure_user_tables_ready(&store, &v2, &user_id).await;

    let rust_memory = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Rust platform guide for systems teams".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-rank".to_string()),
                importance: Some(0.2),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["rust".to_string()],
                source: None,
                embedding: Some(dim_vec(1, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember rust");
    let python_memory = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Python platform guide for data teams".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-rank".to_string()),
                importance: Some(0.2),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["python".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember python");

    let query = RecallV2Request {
        query: "platform guide".to_string(),
        top_k: 2,
        max_tokens: 200,
        session_only: false,
        session_id: None,
        memory_type: None,
        tags: vec![],
        tag_filter_mode: "any".to_string(),
        created_after: None,
        created_before: None,
        with_overview: false,
        with_links: false,
        expand_links: false,
        query_embedding: Some(dim_vec(0, 1.0)),
    };
    let baseline = v2
        .recall(&user_id, query.clone())
        .await
        .expect("baseline recall");
    assert_eq!(baseline.memories.len(), 2);
    assert_eq!(baseline.memories[0].id(), python_memory.memory_id);

    let focus = v2
        .focus(
            &user_id,
            FocusV2Input {
                focus_type: "memory_id".to_string(),
                value: rust_memory.memory_id.clone(),
                boost: Some(5.0),
                ttl_secs: Some(300),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("focus");
    assert_eq!(focus.value, rust_memory.memory_id);

    let focused = v2.recall(&user_id, query).await.expect("focused recall");
    assert_eq!(focused.memories.len(), 2);
    assert_eq!(focused.memories[0].id(), rust_memory.memory_id);

    let stats_row = sqlx::query(&format!(
        "SELECT memory_id, access_count FROM {} WHERE memory_id IN (?, ?) ORDER BY memory_id ASC",
        family.stats_table
    ))
    .bind(&rust_memory.memory_id)
    .bind(&python_memory.memory_id)
    .fetch_all(store.pool())
    .await
    .expect("stats rows");
    assert_eq!(stats_row.len(), 2);
    for row in stats_row {
        assert!(row.try_get::<i32, _>("access_count").unwrap_or_default() >= 1);
    }
}

#[tokio::test]
async fn test_v2_recall_prefers_requested_session_when_scores_are_close() {
    let (_store, v2, user_id) = setup().await;

    let same_session = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "OAuth platform guide for refresh flows same-session note".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-affinity".to_string()),
                importance: Some(0.2),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec![],
                source: None,
                embedding: Some(dim_vec(0, 0.95)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember same-session");
    let other_session = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "OAuth platform guide for refresh flows other-session note".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-other".to_string()),
                importance: Some(0.2),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec![],
                source: None,
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember other-session");

    let baseline = v2
        .recall(
            &user_id,
            RecallV2Request {
                query: "OAuth platform guide refresh flows note".to_string(),
                top_k: 2,
                max_tokens: 200,
                session_only: false,
                session_id: None,
                memory_type: Some(MemoryType::Semantic),
                tags: vec![],
                tag_filter_mode: "any".to_string(),
                created_after: None,
                created_before: None,
                with_overview: false,
                with_links: false,
                expand_links: false,
                query_embedding: Some(dim_vec(0, 1.0)),
            },
        )
        .await
        .expect("baseline recall");
    assert_eq!(baseline.memories.len(), 2);
    assert_eq!(baseline.memories[0].id(), other_session.memory_id);

    let boosted = v2
        .recall(
            &user_id,
            RecallV2Request {
                query: "OAuth platform guide refresh flows note".to_string(),
                top_k: 2,
                max_tokens: 200,
                session_only: false,
                session_id: Some("sess-affinity".to_string()),
                memory_type: Some(MemoryType::Semantic),
                tags: vec![],
                tag_filter_mode: "any".to_string(),
                created_after: None,
                created_before: None,
                with_overview: false,
                with_links: false,
                expand_links: false,
                query_embedding: Some(dim_vec(0, 1.0)),
            },
        )
        .await
        .expect("boosted recall");
    assert_eq!(boosted.memories.len(), 2);
    assert_eq!(boosted.memories[0].id(), same_session.memory_id);
    assert!(boosted.memories[0].score > boosted.memories[1].score);
}

#[tokio::test]
async fn test_v2_recall_link_expansion_surfaces_one_hop_related_memory() {
    let (store, v2, user_id) = setup().await;
    let family = ensure_user_tables_ready(&store, &v2, &user_id).await;

    let seed = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "OAuth token gateway hardening guide for staged rollouts".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-hop".to_string()),
                importance: Some(0.4),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["oauth".to_string(), "shared-bridge".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember seed");
    let linked = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Incident runbook for midnight refresh failures".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-hop".to_string()),
                importance: Some(0.2),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["incident".to_string(), "shared-bridge".to_string()],
                source: None,
                embedding: Some(dim_vec(1, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember linked");
    let distractor = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "OAuth token gateway checklist for release owners".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-hop".to_string()),
                importance: Some(0.1),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["oauth".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 0.35)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember distractor");

    for _ in 0..6 {
        v2.process_user_pending_jobs_pass(&user_id, 10)
            .await
            .expect("process pending jobs");
    }
    let done = sqlx::query(&format!(
        "SELECT COUNT(*) AS cnt FROM {} WHERE status = 'done' AND aggregate_id IN (?, ?, ?)",
        family.jobs_table
    ))
    .bind(&seed.memory_id)
    .bind(&linked.memory_id)
    .bind(&distractor.memory_id)
    .fetch_one(store.pool())
    .await
    .expect("done jobs")
    .try_get::<i64, _>("cnt")
    .unwrap_or_default();
    assert_eq!(done, 6);

    let without_expansion = v2
        .recall(
            &user_id,
            RecallV2Request {
                query: "OAuth token gateway hardening".to_string(),
                top_k: 2,
                max_tokens: 300,
                session_only: false,
                session_id: None,
                memory_type: Some(MemoryType::Semantic),
                tags: vec![],
                tag_filter_mode: "any".to_string(),
                created_after: None,
                created_before: None,
                with_overview: false,
                with_links: false,
                expand_links: false,
                query_embedding: Some(dim_vec(0, 1.0)),
            },
        )
        .await
        .expect("recall without expansion");
    assert!(without_expansion
        .memories
        .iter()
        .any(|item| item.id() == seed.memory_id));
    assert!(without_expansion
        .memories
        .iter()
        .any(|item| item.id() == distractor.memory_id));
    assert!(without_expansion
        .memories
        .iter()
        .all(|item| item.id() != linked.memory_id));

    let with_expansion = v2
        .recall(
            &user_id,
            RecallV2Request {
                query: "OAuth token gateway hardening".to_string(),
                top_k: 3,
                max_tokens: 300,
                session_only: false,
                session_id: None,
                memory_type: Some(MemoryType::Semantic),
                tags: vec![],
                tag_filter_mode: "any".to_string(),
                created_after: None,
                created_before: None,
                with_overview: false,
                with_links: false,
                expand_links: true,
                query_embedding: Some(dim_vec(0, 1.0)),
            },
        )
        .await
        .expect("recall with expansion");
    assert_eq!(with_expansion.memories[0].id(), seed.memory_id);
    let linked_index = with_expansion
        .memories
        .iter()
        .position(|item| item.id() == linked.memory_id)
        .expect("linked memory recalled via one-hop expansion");
    assert!(linked_index > 0);
    assert!(with_expansion.memories[0].score > with_expansion.memories[linked_index].score);
}

#[tokio::test]
async fn test_v2_recall_ranking_breakdown_exposes_components_and_link_bonus() {
    let (store, v2, user_id) = setup().await;
    let family = ensure_user_tables_ready(&store, &v2, &user_id).await;

    let seed = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "OAuth token gateway hardening guide for staged rollouts".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-ranking".to_string()),
                importance: Some(0.4),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["oauth".to_string(), "shared-bridge".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember seed");
    let linked = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Incident runbook for midnight refresh failures".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-ranking".to_string()),
                importance: Some(0.2),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["incident".to_string(), "shared-bridge".to_string()],
                source: None,
                embedding: Some(dim_vec(1, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember linked");

    let done = wait_for_current_jobs_done(
        &store,
        &v2,
        &family,
        &user_id,
        &seed.memory_id,
        &linked.memory_id,
        None,
        4,
    )
    .await;
    assert_eq!(done, 4);

    v2.focus(
        &user_id,
        FocusV2Input {
            focus_type: "memory_id".to_string(),
            value: seed.memory_id.clone(),
            boost: Some(4.0),
            ttl_secs: Some(300),
            actor: "tester".to_string(),
        },
    )
    .await
    .expect("focus seed");

    let recalled = v2
        .recall(
            &user_id,
            RecallV2Request {
                query: "OAuth token gateway hardening".to_string(),
                top_k: 5,
                max_tokens: 400,
                session_only: false,
                session_id: Some("sess-ranking".to_string()),
                memory_type: Some(MemoryType::Semantic),
                tags: vec![],
                tag_filter_mode: "any".to_string(),
                created_after: None,
                created_before: None,
                with_overview: false,
                with_links: false,
                expand_links: true,
                query_embedding: Some(dim_vec(0, 1.0)),
            },
        )
        .await
        .expect("recall with ranking");

    let recalled_seed = recalled
        .memories
        .iter()
        .find(|item| item.id() == seed.memory_id)
        .expect("recalled seed");
    assert!(recalled_seed.ranking.session_affinity_applied);
    assert_eq!(recalled_seed.ranking.session_affinity_multiplier, 1.12);
    assert_eq!(recalled_seed.ranking.focus_boost, 4.0);
    assert!(recalled_seed.ranking.vector_component > 0.0);
    assert!(recalled_seed.ranking.base_score >= recalled_seed.ranking.vector_component);
    assert!(recalled_seed.ranking.confidence_component > 0.0);
    assert!(recalled_seed
        .ranking
        .focus_matches
        .iter()
        .any(|focus| focus.focus_type == "memory_id" && focus.value == seed.memory_id));
    assert!((recalled_seed.score - recalled_seed.ranking.final_score).abs() < 1e-9);

    let recalled_linked = recalled
        .memories
        .iter()
        .find(|item| item.id() == linked.memory_id)
        .expect("recalled linked");
    assert!(recalled_linked.ranking.link_bonus > 0.0);
    assert!(recalled_linked.ranking.linked_expansion_applied);
    assert!(recalled_linked.ranking.base_score >= recalled_linked.ranking.link_bonus);
    assert!(!recalled_linked.ranking.expansion_sources.is_empty());
    assert_eq!(
        recalled_linked.ranking.expansion_sources[0].seed_memory_id,
        seed.memory_id
    );
    assert!(recalled_linked.ranking.expansion_sources[0].bonus > 0.0);
    assert_eq!(
        recalled_linked.ranking.expansion_sources[0].link_type,
        "tag_overlap"
    );
}

#[tokio::test]
async fn test_v2_recall_retrieval_path_distinguishes_direct_and_expanded_results() {
    let (store, v2, user_id) = setup().await;
    let family = ensure_user_tables_ready(&store, &v2, &user_id).await;

    let seed = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "OAuth token gateway hardening guide for staged rollouts".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-path".to_string()),
                importance: Some(0.4),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["shared-bridge".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember seed");
    let expanded_only = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "zzqv narwhal lattice handbook".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-path".to_string()),
                importance: Some(0.2),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["shared-bridge".to_string()],
                source: None,
                embedding: None,
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember expanded-only");
    let direct_only = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "OAuth platform hardening checklist for release owners".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-path".to_string()),
                importance: Some(0.2),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["isolated".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 0.7)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember direct-only");
    let hybrid = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "OAuth hardening bridge playbook for rollout incidents".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-path".to_string()),
                importance: Some(0.3),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["shared-bridge".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 0.8)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember hybrid");
    let isolated_direct = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Release owner checklist with unique zebra marker".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-path".to_string()),
                importance: Some(0.2),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["zebra-unique".to_string()],
                source: None,
                embedding: Some(dim_vec(2, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember isolated direct");

    for _ in 0..8 {
        v2.process_user_pending_jobs_pass(&user_id, 10)
            .await
            .expect("process pending jobs");
    }
    let done = sqlx::query(&format!(
        "SELECT COUNT(*) AS cnt FROM {} WHERE status = 'done' AND aggregate_id IN (?, ?, ?, ?, ?)",
        family.jobs_table
    ))
    .bind(&seed.memory_id)
    .bind(&expanded_only.memory_id)
    .bind(&direct_only.memory_id)
    .bind(&hybrid.memory_id)
    .bind(&isolated_direct.memory_id)
    .fetch_one(store.pool())
    .await
    .expect("done jobs")
    .try_get::<i64, _>("cnt")
    .unwrap_or_default();
    assert_eq!(done, 10);

    let recalled = v2
        .recall(
            &user_id,
            RecallV2Request {
                query: "OAuth hardening rollout".to_string(),
                top_k: 10,
                max_tokens: 500,
                session_only: false,
                session_id: Some("sess-path".to_string()),
                memory_type: Some(MemoryType::Semantic),
                tags: vec![],
                tag_filter_mode: "any".to_string(),
                created_after: None,
                created_before: None,
                with_overview: false,
                with_links: false,
                expand_links: true,
                query_embedding: Some(dim_vec(0, 1.0)),
            },
        )
        .await
        .expect("recall retrieval paths");

    let expanded_only_item = recalled
        .memories
        .iter()
        .find(|item| item.id() == expanded_only.memory_id)
        .expect("expanded-only item");
    assert_eq!(expanded_only_item.retrieval_path.as_str(), "expanded_only");

    let hybrid_item = recalled
        .memories
        .iter()
        .find(|item| item.id() == hybrid.memory_id)
        .expect("hybrid item");
    assert_eq!(hybrid_item.retrieval_path.as_str(), "direct_and_expanded");
    assert!(!hybrid_item.ranking.expansion_sources.is_empty());

    let isolated_recall = v2
        .recall(
            &user_id,
            RecallV2Request {
                query: "unique zebra marker".to_string(),
                top_k: 5,
                max_tokens: 300,
                session_only: false,
                session_id: Some("sess-path".to_string()),
                memory_type: Some(MemoryType::Semantic),
                tags: vec![],
                tag_filter_mode: "any".to_string(),
                created_after: None,
                created_before: None,
                with_overview: false,
                with_links: false,
                expand_links: false,
                query_embedding: Some(dim_vec(2, 1.0)),
            },
        )
        .await
        .expect("isolated direct recall");
    let isolated_direct_item = isolated_recall
        .memories
        .iter()
        .find(|item| item.id() == isolated_direct.memory_id)
        .expect("isolated direct item");
    assert_eq!(isolated_direct_item.retrieval_path.as_str(), "direct");
}

#[tokio::test]
async fn test_v2_recall_summary_tracks_path_buckets_and_truncation() {
    let (store, v2, user_id) = setup().await;
    let family = ensure_user_tables_ready(&store, &v2, &user_id).await;

    let seed = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "OAuth token gateway hardening guide for staged rollouts".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-summary".to_string()),
                importance: Some(0.4),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["shared-bridge".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember seed");
    let expanded_only = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "zzqv narwhal lattice handbook".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-summary".to_string()),
                importance: Some(0.2),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["shared-bridge".to_string()],
                source: None,
                embedding: None,
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember expanded-only");

    let done = wait_for_current_jobs_done(
        &store,
        &v2,
        &family,
        &user_id,
        &seed.memory_id,
        &expanded_only.memory_id,
        None,
        4,
    )
    .await;
    assert_eq!(done, 4);

    let recalled = v2
        .recall(
            &user_id,
            RecallV2Request {
                query: "OAuth token gateway hardening".to_string(),
                top_k: 1,
                max_tokens: 300,
                session_only: false,
                session_id: Some("sess-summary".to_string()),
                memory_type: Some(MemoryType::Semantic),
                tags: vec![],
                tag_filter_mode: "any".to_string(),
                created_after: None,
                created_before: None,
                with_overview: false,
                with_links: false,
                expand_links: true,
                query_embedding: Some(dim_vec(0, 1.0)),
            },
        )
        .await
        .expect("recall summary");

    assert!(recalled.has_more);
    assert_eq!(recalled.summary.discovered_count, 2);
    assert_eq!(recalled.summary.returned_count, 1);
    assert!(recalled.summary.truncated);

    let direct_bucket = recalled
        .summary
        .by_retrieval_path
        .iter()
        .find(|bucket| bucket.retrieval_path.as_str() == "direct")
        .expect("direct bucket");
    assert_eq!(direct_bucket.discovered_count, 1);
    assert_eq!(direct_bucket.returned_count, 1);

    let expanded_bucket = recalled
        .summary
        .by_retrieval_path
        .iter()
        .find(|bucket| bucket.retrieval_path.as_str() == "expanded_only")
        .expect("expanded-only bucket");
    assert_eq!(expanded_bucket.discovered_count, 1);
    assert_eq!(expanded_bucket.returned_count, 0);

    let hybrid_bucket = recalled
        .summary
        .by_retrieval_path
        .iter()
        .find(|bucket| bucket.retrieval_path.as_str() == "direct_and_expanded")
        .expect("hybrid bucket");
    assert_eq!(hybrid_bucket.discovered_count, 0);
    assert_eq!(hybrid_bucket.returned_count, 0);
}

#[tokio::test]
async fn test_v2_recall_temporal_decay_prefers_fresh_and_longer_lived_memories() {
    let (store, v2, user_id) = setup().await;
    let family = ensure_user_tables_ready(&store, &v2, &user_id).await;

    let fresh_working = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Auth outage triage anchor".to_string(),
                memory_type: MemoryType::Working,
                session_id: None,
                importance: Some(0.2),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec![],
                source: None,
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember fresh working");
    let stale_working = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Auth outage triage anchor".to_string(),
                memory_type: MemoryType::Working,
                session_id: None,
                importance: Some(0.2),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec![],
                source: None,
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember stale working");
    let stale_semantic = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Auth outage triage anchor".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: None,
                importance: Some(0.2),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec![],
                source: None,
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember stale semantic");

    let stale_created_at = (Utc::now() - Duration::days(10)).naive_utc();
    sqlx::query(&format!(
        "UPDATE {} SET created_at = ? WHERE memory_id IN (?, ?)",
        family.heads_table
    ))
    .bind(stale_created_at)
    .bind(&stale_working.memory_id)
    .bind(&stale_semantic.memory_id)
    .execute(store.pool())
    .await
    .expect("age stale memories");

    let recalled = v2
        .recall(
            &user_id,
            RecallV2Request {
                query: "Auth outage triage anchor".to_string(),
                top_k: 5,
                max_tokens: 400,
                session_only: false,
                session_id: None,
                memory_type: None,
                tags: vec![],
                tag_filter_mode: "any".to_string(),
                created_after: None,
                created_before: None,
                with_overview: false,
                with_links: false,
                expand_links: false,
                query_embedding: Some(dim_vec(0, 1.0)),
            },
        )
        .await
        .expect("recall temporal decay");

    assert_eq!(recalled.memories[0].id(), fresh_working.memory_id);
    assert_eq!(recalled.memories[1].id(), stale_semantic.memory_id);
    assert_eq!(recalled.memories[2].id(), stale_working.memory_id);

    let stale_semantic_item = recalled
        .memories
        .iter()
        .find(|item| item.id() == stale_semantic.memory_id)
        .expect("stale semantic item");
    let stale_working_item = recalled
        .memories
        .iter()
        .find(|item| item.id() == stale_working.memory_id)
        .expect("stale working item");
    let fresh_working_item = recalled
        .memories
        .iter()
        .find(|item| item.id() == fresh_working.memory_id)
        .expect("fresh working item");

    assert!(fresh_working_item.ranking.temporal_multiplier > 0.999);
    assert!(!fresh_working_item.ranking.temporal_decay_applied);
    assert!(stale_semantic_item.ranking.temporal_decay_applied);
    assert!(stale_working_item.ranking.temporal_decay_applied);
    assert_eq!(stale_semantic_item.ranking.temporal_half_life_hours, 2160.0);
    assert_eq!(stale_working_item.ranking.temporal_half_life_hours, 48.0);
    assert!(stale_working_item.ranking.age_hours > 200.0);
    assert!(
        stale_working_item.ranking.temporal_multiplier
            < stale_semantic_item.ranking.temporal_multiplier
    );
    assert!(stale_semantic_item.score > stale_working_item.score);
}

#[tokio::test]
async fn test_v2_recall_filters_by_tags_any_and_all() {
    let (_store, v2, user_id) = setup().await;
    let mut query_embedding = vec![0.0f32; test_dim()];
    query_embedding[0] = 1.0;
    query_embedding[1] = 1.0;

    let both = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Rust systems handbook".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-tags".to_string()),
                importance: Some(0.2),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["rust".to_string(), "systems".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember both");
    let python = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Python data handbook".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-tags".to_string()),
                importance: Some(0.2),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["python".to_string(), "data".to_string()],
                source: None,
                embedding: Some(dim_vec(1, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember python");
    let rust_only = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Rust runtime cheatsheet".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-tags".to_string()),
                importance: Some(0.2),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["rust".to_string(), "runtime".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 0.9)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember rust only");

    let any = v2
        .recall(
            &user_id,
            RecallV2Request {
                query: "handbook".to_string(),
                top_k: 10,
                max_tokens: 400,
                session_only: true,
                session_id: Some("sess-tags".to_string()),
                memory_type: None,
                tags: vec!["SYSTEMS".to_string(), "python".to_string()],
                tag_filter_mode: "any".to_string(),
                created_after: None,
                created_before: None,
                with_overview: false,
                with_links: false,
                expand_links: false,
                query_embedding: Some(query_embedding.clone()),
            },
        )
        .await
        .expect("recall any");
    assert_eq!(any.memories.len(), 2);
    assert!(any.memories.iter().any(|item| item.id() == both.memory_id));
    assert!(any
        .memories
        .iter()
        .any(|item| item.id() == python.memory_id));
    assert!(any
        .memories
        .iter()
        .all(|item| item.id() != rust_only.memory_id));

    let all = v2
        .recall(
            &user_id,
            RecallV2Request {
                query: "handbook".to_string(),
                top_k: 10,
                max_tokens: 400,
                session_only: true,
                session_id: Some("sess-tags".to_string()),
                memory_type: None,
                tags: vec!["rust".to_string(), "systems".to_string()],
                tag_filter_mode: "all".to_string(),
                created_after: None,
                created_before: None,
                with_overview: false,
                with_links: false,
                expand_links: false,
                query_embedding: Some(query_embedding.clone()),
            },
        )
        .await
        .expect("recall all");
    assert_eq!(all.memories.len(), 1);
    assert_eq!(all.memories[0].id(), both.memory_id);

    let unfiltered = v2
        .recall(
            &user_id,
            RecallV2Request {
                query: "handbook".to_string(),
                top_k: 10,
                max_tokens: 400,
                session_only: true,
                session_id: Some("sess-tags".to_string()),
                memory_type: None,
                tags: vec![],
                tag_filter_mode: "all".to_string(),
                created_after: None,
                created_before: None,
                with_overview: false,
                with_links: false,
                expand_links: false,
                query_embedding: Some(query_embedding),
            },
        )
        .await
        .expect("recall unfiltered");
    assert_eq!(unfiltered.memories.len(), 3);
}

#[tokio::test]
async fn test_v2_recall_rejects_invalid_tag_filter_mode() {
    let (_store, v2, user_id) = setup().await;

    let err = v2
        .recall(
            &user_id,
            RecallV2Request {
                query: "anything".to_string(),
                top_k: 10,
                max_tokens: 200,
                session_only: false,
                session_id: None,
                memory_type: None,
                tags: vec!["rust".to_string()],
                tag_filter_mode: "bad".to_string(),
                created_after: None,
                created_before: None,
                with_overview: false,
                with_links: false,
                expand_links: false,
                query_embedding: None,
            },
        )
        .await
        .expect_err("invalid tag filter mode should fail");
    assert!(matches!(err, MemoriaError::Validation(_)));
}

#[tokio::test]
async fn test_v2_profile_lists_only_active_profile_memories() {
    let (_store, v2, user_id) = setup().await;

    let first_profile = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Prefers concise changelog entries".to_string(),
                memory_type: MemoryType::Profile,
                session_id: Some("sess-profile-a".to_string()),
                importance: Some(0.9),
                trust_tier: Some(TrustTier::T1Verified),
                tags: vec!["preference".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember first profile");
    let semantic = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Semantic memory that should not appear in profile listing".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-profile-a".to_string()),
                importance: Some(0.2),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["semantic".to_string()],
                source: None,
                embedding: Some(dim_vec(1, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember semantic");
    let second_profile = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Uses cargo test before submitting patches".to_string(),
                memory_type: MemoryType::Profile,
                session_id: Some("sess-profile-b".to_string()),
                importance: Some(0.7),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["workflow".to_string()],
                source: None,
                embedding: Some(dim_vec(2, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember second profile");

    let listed = v2
        .profile(
            &user_id,
            ProfileV2Filter {
                limit: 10,
                cursor: None,
                session_id: None,
            },
        )
        .await
        .expect("list profiles");
    assert_eq!(listed.items.len(), 2);
    assert_eq!(listed.items[0].memory_id, second_profile.memory_id);
    assert_eq!(
        listed.items[0].content,
        "Uses cargo test before submitting patches"
    );
    assert_eq!(listed.items[0].trust_tier, TrustTier::T2Curated);
    assert_eq!(listed.items[1].memory_id, first_profile.memory_id);
    assert!(listed
        .items
        .iter()
        .all(|item| item.memory_id != semantic.memory_id));

    let filtered = v2
        .profile(
            &user_id,
            ProfileV2Filter {
                limit: 10,
                cursor: None,
                session_id: Some("sess-profile-a".to_string()),
            },
        )
        .await
        .expect("filter profiles by session");
    assert_eq!(filtered.items.len(), 1);
    assert_eq!(filtered.items[0].memory_id, first_profile.memory_id);

    let paged = v2
        .profile(
            &user_id,
            ProfileV2Filter {
                limit: 1,
                cursor: None,
                session_id: None,
            },
        )
        .await
        .expect("paged profile");
    assert_eq!(paged.items.len(), 1);
    assert!(paged.next_cursor.is_some());

    v2.forget(
        &user_id,
        &second_profile.memory_id,
        Some("cleanup"),
        "tester",
    )
    .await
    .expect("forget second profile");
    let after_forget = v2
        .profile(
            &user_id,
            ProfileV2Filter {
                limit: 10,
                cursor: None,
                session_id: None,
            },
        )
        .await
        .expect("profiles after forget");
    assert_eq!(after_forget.items.len(), 1);
    assert_eq!(after_forget.items[0].memory_id, first_profile.memory_id);
}

#[tokio::test]
async fn test_v2_entities_extract_list_refresh_and_forget() {
    let (_store, v2, user_id) = setup().await;

    let remembered = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Rust and Docker keep the auth-service deployable".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-entities".to_string()),
                importance: Some(0.5),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["infra".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember for entities");

    let extracted = v2
        .extract_entities(&user_id, 10, None)
        .await
        .expect("extract entities");
    assert_eq!(extracted.processed_memories, 1);
    assert!(extracted.entities_found >= 3);
    assert!(extracted.links_written >= 3);

    let listed = v2
        .list_entities(
            &user_id,
            EntityV2Filter {
                limit: 20,
                cursor: None,
                query: None,
                entity_type: None,
                memory_id: None,
            },
        )
        .await
        .expect("list entities");
    let names = listed
        .items
        .iter()
        .map(|item| item.name.as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&"rust"));
    assert!(names.contains(&"docker"));
    assert!(names.contains(&"auth-service"));
    assert!(listed.items.iter().all(|item| item.memory_count == 1));

    let filtered = v2
        .list_entities(
            &user_id,
            EntityV2Filter {
                limit: 20,
                cursor: None,
                query: Some("dock".to_string()),
                entity_type: Some("tech".to_string()),
                memory_id: Some(remembered.memory_id.clone()),
            },
        )
        .await
        .expect("filter entities");
    assert_eq!(filtered.items.len(), 1);
    assert_eq!(filtered.items[0].name, "docker");

    v2.update(
        &user_id,
        MemoryV2UpdateInput {
            memory_id: remembered.memory_id.clone(),
            content: Some("Python powers the billing-gateway".to_string()),
            importance: None,
            trust_tier: None,
            tags_add: vec![],
            tags_remove: vec![],
            embedding: Some(dim_vec(1, 1.0)),
            actor: "tester".to_string(),
            reason: Some("refresh entities".to_string()),
        },
    )
    .await
    .expect("update content");

    let stale_hidden = v2
        .list_entities(
            &user_id,
            EntityV2Filter {
                limit: 20,
                cursor: None,
                query: None,
                entity_type: None,
                memory_id: None,
            },
        )
        .await
        .expect("list entities after update");
    assert!(stale_hidden.items.is_empty());

    let refreshed = v2
        .extract_entities(&user_id, 10, Some(&remembered.memory_id))
        .await
        .expect("refresh extracted entities");
    assert_eq!(refreshed.processed_memories, 1);

    let relisted = v2
        .list_entities(
            &user_id,
            EntityV2Filter {
                limit: 20,
                cursor: None,
                query: None,
                entity_type: None,
                memory_id: None,
            },
        )
        .await
        .expect("relist entities");
    let refreshed_names = relisted
        .items
        .iter()
        .map(|item| item.name.as_str())
        .collect::<Vec<_>>();
    assert!(refreshed_names.contains(&"python"));
    assert!(refreshed_names.contains(&"billing-gateway"));
    assert!(!refreshed_names.contains(&"rust"));
    assert!(!refreshed_names.contains(&"docker"));

    v2.forget(&user_id, &remembered.memory_id, Some("done"), "tester")
        .await
        .expect("forget entities memory");
    let after_forget = v2
        .list_entities(
            &user_id,
            EntityV2Filter {
                limit: 20,
                cursor: None,
                query: None,
                entity_type: None,
                memory_id: None,
            },
        )
        .await
        .expect("entities after forget");
    assert!(after_forget.items.is_empty());
}

#[tokio::test]
async fn test_v2_extract_entities_is_idempotent_under_concurrent_reentry() {
    let (_store, v2, user_id) = setup().await;
    let remembered = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Rust and Docker keep the auth-service deployable while Rust automation keeps Docker workflows consistent.".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-entities-reentry".to_string()),
                importance: Some(0.4),
                trust_tier: None,
                tags: vec!["shared".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember");

    let first = v2.extract_entities(&user_id, 10, Some(&remembered.memory_id));
    let second = v2.extract_entities(&user_id, 10, Some(&remembered.memory_id));
    let (first, second) = tokio::join!(first, second);
    let first = first.expect("first concurrent extract");
    let second = second.expect("second concurrent extract");

    assert_eq!(first.processed_memories, 1);
    assert_eq!(second.processed_memories, 1);

    let listed = v2
        .list_entities(
            &user_id,
            EntityV2Filter {
                limit: 20,
                cursor: None,
                query: None,
                entity_type: None,
                memory_id: Some(remembered.memory_id.clone()),
            },
        )
        .await
        .expect("list entities after concurrent extract");
    let names = listed
        .items
        .iter()
        .map(|item| item.name.as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&"rust"));
    assert!(names.contains(&"docker"));
    assert!(names.contains(&"auth-service"));
}

#[tokio::test]
async fn test_v2_reflect_candidates_use_v2_links_and_ignore_v1() {
    let (store, v2, user_id) = setup().await;
    let v1_memory = Memory {
        memory_id: Uuid::new_v4().simple().to_string(),
        user_id: user_id.clone(),
        memory_type: MemoryType::Semantic,
        content: "Legacy V1 note that should stay outside V2 reflect".to_string(),
        initial_confidence: TrustTier::T1Verified.initial_confidence(),
        embedding: Some(dim_vec(9, 1.0)),
        source_event_ids: vec![],
        superseded_by: None,
        is_active: true,
        access_count: 0,
        session_id: Some("sess-v1-reflect".to_string()),
        observed_at: Some(chrono::Utc::now()),
        created_at: None,
        updated_at: None,
        extra_metadata: None,
        trust_tier: TrustTier::T1Verified,
        retrieval_score: None,
    };
    store.insert(&v1_memory).await.expect("insert v1 memory");

    let first = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Alpha platform memory connected through shared operations".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-reflect-a".to_string()),
                importance: Some(0.6),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["shared".to_string(), "alpha".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember first");
    let second = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Bridge operations memory joining alpha and beta".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-reflect-b".to_string()),
                importance: Some(0.8),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["shared".to_string(), "beta".to_string()],
                source: None,
                embedding: Some(dim_vec(1, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember second");
    let third = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Beta deployment memory linked through the bridge".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-reflect-b".to_string()),
                importance: Some(0.5),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["beta".to_string(), "gamma".to_string()],
                source: None,
                embedding: Some(dim_vec(2, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember third");

    let mut reflected = None;
    for _ in 0..30 {
        v2.process_user_pending_jobs_pass(&user_id, 10)
            .await
            .expect("process jobs");
        let result = v2
            .reflect(
                &user_id,
                ReflectV2Filter {
                    limit: 10,
                    mode: "auto".to_string(),
                    session_id: None,
                    min_cluster_size: 2,
                    min_link_strength: 0.35,
                },
            )
            .await
            .expect("reflect candidates");
        if !result.candidates.is_empty() {
            reflected = Some(result);
            break;
        }
    }
    let reflected = reflected.expect("linked reflect candidate");
    let candidate = reflected.candidates.first().expect("first candidate");
    assert_eq!(candidate.signal, "cross_session_linked_cluster");
    assert_eq!(candidate.memory_count, 3);
    assert_eq!(candidate.session_count, 2);
    assert!(candidate.link_count >= 2);
    let memory_ids = candidate
        .memories
        .iter()
        .map(|memory| memory.memory_id.as_str())
        .collect::<Vec<_>>();
    assert!(memory_ids.contains(&first.memory_id.as_str()));
    assert!(memory_ids.contains(&second.memory_id.as_str()));
    assert!(memory_ids.contains(&third.memory_id.as_str()));
    assert!(!memory_ids.contains(&v1_memory.memory_id.as_str()));
}

#[tokio::test]
async fn test_v2_reflect_candidates_fall_back_to_session_groups() {
    let (_store, v2, user_id) = setup().await;
    let first = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Notebook summary for a quiet retrospective".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-reflect-fallback".to_string()),
                importance: Some(0.4),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["notebook".to_string()],
                source: None,
                embedding: Some(dim_vec(3, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember first fallback");
    let second = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Calendar reminder for the same session".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-reflect-fallback".to_string()),
                importance: Some(0.6),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["calendar".to_string()],
                source: None,
                embedding: Some(dim_vec(4, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember second fallback");

    for _ in 0..10 {
        v2.process_user_pending_jobs_pass(&user_id, 10)
            .await
            .expect("process fallback jobs");
    }

    let reflected = v2
        .reflect(
            &user_id,
            ReflectV2Filter {
                limit: 10,
                mode: "candidates".to_string(),
                session_id: Some("sess-reflect-fallback".to_string()),
                min_cluster_size: 2,
                min_link_strength: 0.95,
            },
        )
        .await
        .expect("reflect fallback candidates");
    let candidate = reflected.candidates.first().expect("session candidate");
    assert_eq!(candidate.signal, "session_cluster");
    assert_eq!(candidate.memory_count, 2);
    assert_eq!(candidate.session_count, 1);
    assert_eq!(candidate.link_count, 0);
    let memory_ids = candidate
        .memories
        .iter()
        .map(|memory| memory.memory_id.as_str())
        .collect::<Vec<_>>();
    assert!(memory_ids.contains(&first.memory_id.as_str()));
    assert!(memory_ids.contains(&second.memory_id.as_str()));
}

#[tokio::test]
async fn test_v2_reflect_internal_writes_synthesized_memory_and_dedupes() {
    let (store, v2, user_id) = setup().await;
    let family = ensure_user_tables_ready(&store, &v2, &user_id).await;

    let first = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Alpha platform memory connected through shared operations".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-reflect-a".to_string()),
                importance: Some(0.6),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["shared".to_string(), "alpha".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember first");
    let second = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Bridge operations memory joining alpha and beta".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-reflect-b".to_string()),
                importance: Some(0.8),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["shared".to_string(), "beta".to_string()],
                source: None,
                embedding: Some(dim_vec(1, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember second");
    let third = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Beta deployment memory linked through the bridge".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-reflect-b".to_string()),
                importance: Some(0.5),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["beta".to_string(), "gamma".to_string()],
                source: None,
                embedding: Some(dim_vec(2, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember third");

    for _ in 0..30 {
        v2.process_user_pending_jobs_pass(&user_id, 10)
            .await
            .expect("process reflect jobs");
        let ready = v2
            .reflect(
                &user_id,
                ReflectV2Filter {
                    limit: 10,
                    mode: "auto".to_string(),
                    session_id: None,
                    min_cluster_size: 2,
                    min_link_strength: 0.35,
                },
            )
            .await
            .expect("reflect auto readiness");
        if !ready.candidates.is_empty() {
            break;
        }
    }

    let reflected = v2
        .reflect(
            &user_id,
            ReflectV2Filter {
                limit: 10,
                mode: "internal".to_string(),
                session_id: None,
                min_cluster_size: 2,
                min_link_strength: 0.35,
            },
        )
        .await
        .expect("reflect internal");
    assert_eq!(reflected.mode, "internal");
    assert_eq!(reflected.synthesized, true);
    assert_eq!(reflected.scenes_created, 1);
    let candidate = reflected.candidates.first().expect("internal candidate");
    assert_eq!(candidate.signal, "cross_session_linked_cluster");
    assert_eq!(candidate.memory_count, 3);

    let listed = v2
        .list(
            &user_id,
            ListV2Filter {
                limit: 10,
                cursor: None,
                memory_type: None,
                session_id: None,
            },
        )
        .await
        .expect("list after reflect internal");
    assert_eq!(listed.items.len(), 4);
    let synthesized = listed
        .items
        .iter()
        .find(|item| {
            item.memory_id != first.memory_id
                && item.memory_id != second.memory_id
                && item.memory_id != third.memory_id
        })
        .expect("synthesized memory");

    let synthesized_row = sqlx::query(&format!(
        "SELECT source_kind, source_json FROM {} WHERE memory_id = ?",
        family.heads_table
    ))
    .bind(&synthesized.memory_id)
    .fetch_one(store.pool())
    .await
    .expect("synthesized row");
    let source_kind: Option<String> = synthesized_row.try_get("source_kind").ok();
    assert_eq!(source_kind.as_deref(), Some("reflect_v2"));
    let source_json: serde_json::Value = synthesized_row
        .try_get("source_json")
        .expect("synthesized source json");
    assert_eq!(source_json["mode"], "internal");
    assert_eq!(source_json["signal"], "cross_session_linked_cluster");

    let links = v2
        .links(
            &user_id,
            MemoryV2LinksRequest {
                memory_id: synthesized.memory_id.clone(),
                direction: LinkDirection::Both,
                limit: 10,
                link_type: Some("reflection".to_string()),
                min_strength: 0.0,
            },
        )
        .await
        .expect("links for synthesized memory");
    assert_eq!(links.items.len(), 3);
    assert!(links
        .items
        .iter()
        .all(|item| item.direction == LinkDirection::Inbound));
    let linked_ids = links
        .items
        .iter()
        .map(|item| item.memory_id.as_str())
        .collect::<Vec<_>>();
    assert!(linked_ids.contains(&first.memory_id.as_str()));
    assert!(linked_ids.contains(&second.memory_id.as_str()));
    assert!(linked_ids.contains(&third.memory_id.as_str()));

    let rerun = v2
        .reflect(
            &user_id,
            ReflectV2Filter {
                limit: 10,
                mode: "internal".to_string(),
                session_id: None,
                min_cluster_size: 2,
                min_link_strength: 0.35,
            },
        )
        .await
        .expect("reflect internal rerun");
    assert_eq!(rerun.scenes_created, 0);

    let post_rerun = v2
        .list(
            &user_id,
            ListV2Filter {
                limit: 10,
                cursor: None,
                memory_type: None,
                session_id: None,
            },
        )
        .await
        .expect("list after rerun");
    assert_eq!(post_rerun.items.len(), 4);

    let candidates = v2
        .reflect(
            &user_id,
            ReflectV2Filter {
                limit: 10,
                mode: "candidates".to_string(),
                session_id: None,
                min_cluster_size: 2,
                min_link_strength: 0.35,
            },
        )
        .await
        .expect("reflect candidates after synth");
    assert!(candidates.candidates.iter().all(|candidate| {
        candidate
            .memories
            .iter()
            .all(|memory| memory.memory_id != synthesized.memory_id)
    }));
}

#[tokio::test]
async fn test_v2_rejects_wrong_embedding_dimension() {
    let (_store, v2, user_id) = setup().await;

    let err = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "dimension mismatch".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: None,
                importance: None,
                trust_tier: None,
                tags: vec![],
                source: None,
                embedding: Some(vec![0.1, 0.2, 0.3]),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect_err("remember should reject bad dimensions");
    assert!(matches!(err, MemoriaError::Validation(_)));
}

#[tokio::test]
async fn test_v2_entities_auto_extract_via_jobs_and_refresh_on_update() {
    let (store, v2, user_id) = setup().await;
    let family = ensure_user_tables_ready(&store, &v2, &user_id).await;

    let remembered = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "MatrixOne and Docker power the deploy-service".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-auto-entities".to_string()),
                importance: Some(0.5),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["infra".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember auto entities");

    let initial_jobs = v2
        .jobs(
            &user_id,
            MemoryV2JobsRequest {
                memory_id: remembered.memory_id.clone(),
                limit: 10,
            },
        )
        .await
        .expect("initial jobs");
    assert_eq!(initial_jobs.pending_count, 2);
    assert_eq!(initial_jobs.items.len(), 2);
    assert_eq!(initial_jobs.job_types.len(), 2);
    assert!(initial_jobs
        .job_types
        .iter()
        .any(|item| item.job_type == "extract_entities"));

    for _ in 0..20 {
        v2.process_user_pending_jobs_pass(&user_id, 10)
            .await
            .expect("process jobs");
        let current = v2
            .jobs(
                &user_id,
                MemoryV2JobsRequest {
                    memory_id: remembered.memory_id.clone(),
                    limit: 10,
                },
            )
            .await
            .expect("current jobs");
        let entities = v2
            .list_entities(
                &user_id,
                EntityV2Filter {
                    limit: 20,
                    cursor: None,
                    query: None,
                    entity_type: None,
                    memory_id: Some(remembered.memory_id.clone()),
                },
            )
            .await
            .expect("entities after jobs");
        if current.done_count == 2
            && current.pending_count == 0
            && entities.items.iter().any(|item| item.name == "matrixone")
            && entities.items.iter().any(|item| item.name == "docker")
            && entities
                .items
                .iter()
                .any(|item| item.name == "deploy-service")
        {
            break;
        }
    }

    let mention_rows = sqlx::query(&format!(
        "SELECT COUNT(*) AS cnt FROM {} WHERE memory_id = ? AND content_version_id = ?",
        family.memory_entities_table
    ))
    .bind(&remembered.memory_id)
    .bind(
        sqlx::query(&format!(
            "SELECT current_content_version_id FROM {} WHERE memory_id = ?",
            family.heads_table
        ))
        .bind(&remembered.memory_id)
        .fetch_one(store.pool())
        .await
        .expect("current content version")
        .try_get::<String, _>("current_content_version_id")
        .expect("current content version id"),
    )
    .fetch_one(store.pool())
    .await
    .expect("mention rows")
    .try_get::<i64, _>("cnt")
    .unwrap_or_default();
    assert!(mention_rows >= 3);

    v2.update(
        &user_id,
        MemoryV2UpdateInput {
            memory_id: remembered.memory_id.clone(),
            content: Some("Python runs the billing-gateway".to_string()),
            importance: None,
            trust_tier: None,
            tags_add: vec![],
            tags_remove: vec![],
            embedding: Some(dim_vec(1, 1.0)),
            actor: "tester".to_string(),
            reason: Some("refresh auto entities".to_string()),
        },
    )
    .await
    .expect("update content for auto entities");

    for _ in 0..20 {
        v2.process_user_pending_jobs_pass(&user_id, 10)
            .await
            .expect("process update jobs");
        let entities = v2
            .list_entities(
                &user_id,
                EntityV2Filter {
                    limit: 20,
                    cursor: None,
                    query: None,
                    entity_type: None,
                    memory_id: Some(remembered.memory_id.clone()),
                },
            )
            .await
            .expect("entities after update jobs");
        let names = entities
            .items
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>();
        if names.contains(&"python") && names.contains(&"billing-gateway") {
            assert!(!names.contains(&"matrixone"));
            assert!(!names.contains(&"docker"));
            break;
        }
    }
}

#[tokio::test]
async fn test_v2_recall_uses_entity_candidates_when_index_signals_miss() {
    let (store, v2, user_id) = setup().await;
    let family = ensure_user_tables_ready(&store, &v2, &user_id).await;

    let remembered = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "MatrixOne production rollout for storage platform".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-entity-recall".to_string()),
                importance: Some(0.4),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["database".to_string()],
                source: None,
                embedding: Some(dim_vec(1, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember entity recall target");

    for _ in 0..20 {
        v2.process_user_pending_jobs_pass(&user_id, 10)
            .await
            .expect("process jobs");
        let entities = v2
            .list_entities(
                &user_id,
                EntityV2Filter {
                    limit: 20,
                    cursor: None,
                    query: Some("matrix".to_string()),
                    entity_type: None,
                    memory_id: Some(remembered.memory_id.clone()),
                },
            )
            .await
            .expect("list entities");
        if entities.items.iter().any(|item| item.name == "matrixone") {
            break;
        }
    }

    let current_index_doc_id = sqlx::query(&format!(
        "SELECT current_index_doc_id FROM {} WHERE memory_id = ?",
        family.heads_table
    ))
    .bind(&remembered.memory_id)
    .fetch_one(store.pool())
    .await
    .expect("current index doc")
    .try_get::<String, _>("current_index_doc_id")
    .expect("current index doc id");
    sqlx::query(&format!(
        "UPDATE {} SET recall_text = ?, embedding = ? WHERE index_doc_id = ?",
        family.index_docs_table
    ))
    .bind("completely unrelated abstract for ranking isolation")
    .bind(None::<String>)
    .bind(&current_index_doc_id)
    .execute(store.pool())
    .await
    .expect("rewrite index doc signals");

    let recalled = v2
        .recall(
            &user_id,
            RecallV2Request {
                query: "MatrixOne".to_string(),
                top_k: 5,
                max_tokens: 200,
                session_only: false,
                session_id: None,
                memory_type: Some(MemoryType::Semantic),
                tags: vec![],
                tag_filter_mode: "any".to_string(),
                created_after: None,
                created_before: None,
                with_overview: false,
                with_links: false,
                expand_links: false,
                query_embedding: Some(dim_vec(0, 1.0)),
            },
        )
        .await
        .expect("entity-backed recall");
    assert!(recalled
        .memories
        .iter()
        .any(|memory| memory.id() == remembered.memory_id));
}

#[tokio::test]
async fn test_v2_entities_surface_remains_isolated_from_v1_graph_entities() {
    let (store, v2, user_id) = setup().await;

    let v1_memory = Memory {
        memory_id: Uuid::new_v4().simple().to_string(),
        user_id: user_id.clone(),
        memory_type: MemoryType::Semantic,
        content: "Legacy zebra-gateway knowledge".to_string(),
        initial_confidence: TrustTier::T1Verified.initial_confidence(),
        embedding: Some(dim_vec(0, 1.0)),
        source_event_ids: vec![],
        superseded_by: None,
        is_active: true,
        access_count: 0,
        session_id: Some("sess-v1-entities".to_string()),
        observed_at: Some(chrono::Utc::now()),
        created_at: None,
        updated_at: None,
        extra_metadata: None,
        trust_tier: TrustTier::T1Verified,
        retrieval_score: None,
    };
    store.insert(&v1_memory).await.expect("insert v1 memory");
    let graph = store.graph_store();
    let (entity_id, _) = graph
        .upsert_entity(&user_id, "zebra-gateway", "zebra-gateway", "project")
        .await
        .expect("upsert v1 entity");
    graph
        .batch_upsert_memory_entity_links(&user_id, &[(&v1_memory.memory_id, &entity_id, "manual")])
        .await
        .expect("link v1 entity");

    let v2_memory = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Rust deploy worker".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-v2-entities".to_string()),
                importance: Some(0.3),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec![],
                source: None,
                embedding: Some(dim_vec(1, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember v2 memory");
    v2.extract_entities(&user_id, 10, Some(&v2_memory.memory_id))
        .await
        .expect("extract v2 entities");

    let listed = v2
        .list_entities(
            &user_id,
            EntityV2Filter {
                limit: 20,
                cursor: None,
                query: None,
                entity_type: None,
                memory_id: None,
            },
        )
        .await
        .expect("list v2 entities");
    let names = listed
        .items
        .iter()
        .map(|item| item.name.as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&"rust"));
    assert!(!names.contains(&"zebra-gateway"));
}

#[tokio::test]
async fn test_v2_job_processing_stays_within_v2_tables() {
    let (store, v2, user_id) = setup().await;
    let family = ensure_user_tables_ready(&store, &v2, &user_id).await;

    let v1_memory = Memory {
        memory_id: Uuid::new_v4().simple().to_string(),
        user_id: user_id.clone(),
        memory_type: MemoryType::Semantic,
        content: "Legacy V1 shared note that should stay outside V2 jobs".to_string(),
        initial_confidence: TrustTier::T1Verified.initial_confidence(),
        embedding: Some(dim_vec(0, 1.0)),
        source_event_ids: vec![],
        superseded_by: None,
        is_active: true,
        access_count: 0,
        session_id: Some("sess-v1-jobs".to_string()),
        observed_at: Some(chrono::Utc::now()),
        created_at: None,
        updated_at: None,
        extra_metadata: None,
        trust_tier: TrustTier::T1Verified,
        retrieval_score: None,
    };
    store.insert(&v1_memory).await.expect("insert v1 memory");

    let mut v1_before = None;
    for _ in 0..10 {
        let row = sqlx::query(
            "SELECT content, is_active, superseded_by, updated_at FROM mem_memories WHERE memory_id = ?",
        )
        .bind(&v1_memory.memory_id)
        .fetch_optional(store.pool())
        .await
        .expect("fetch optional v1 before");
        if row.is_some() {
            v1_before = row;
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }
    let v1_before = v1_before.expect("fetch v1 before");
    let v1_updated_at_before = v1_before
        .try_get::<chrono::NaiveDateTime, _>("updated_at")
        .expect("v1 updated_at before");

    let first = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "First V2 shared memory for isolated job processing within the V2 table family. This memory tests that background jobs stay within V2 table boundaries and do not mutate legacy V1 memory rows. View derivation requires sufficient content length to trigger the derive_views job processing.".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-v2-jobs".to_string()),
                importance: Some(0.4),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["shared".to_string(), "rust".to_string()],
                source: None,
                embedding: Some(dim_vec(1, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember first");
    let second = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Second V2 shared memory that should link only inside V2 table boundaries. This content must be longer than the abstract threshold to trigger the derive_views enrichment job. Platform teams use shared Rust service patterns for reliable and safe infrastructure across distributed services.".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-v2-jobs".to_string()),
                importance: Some(0.4),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["shared".to_string(), "platform".to_string()],
                source: None,
                embedding: Some(dim_vec(1, 0.92)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember second");

    let queued_for_v1 = sqlx::query(&format!(
        "SELECT COUNT(*) AS cnt FROM {} WHERE aggregate_id = ?",
        family.jobs_table
    ))
    .bind(&v1_memory.memory_id)
    .fetch_one(store.pool())
    .await
    .expect("v1 queued jobs")
    .try_get::<i64, _>("cnt")
    .unwrap_or_default();
    assert_eq!(queued_for_v1, 0);

    let mut done = 0i64;
    for _ in 0..30 {
        v2.process_user_pending_jobs_pass(&user_id, 10)
            .await
            .expect("process v2 jobs");
        done = sqlx::query(&format!(
            "SELECT COUNT(*) AS cnt FROM {} WHERE status = 'done' AND aggregate_id IN (?, ?)",
            family.jobs_table
        ))
        .bind(&first.memory_id)
        .bind(&second.memory_id)
        .fetch_one(store.pool())
        .await
        .expect("done jobs")
        .try_get::<i64, _>("cnt")
        .unwrap_or_default();
        let expanded = v2
            .expand(&user_id, &first.memory_id, ExpandLevel::Links)
            .await
            .expect("expand first");
        if done == 6
            && expanded.overview_text.is_some()
            && expanded.detail_text.is_some()
            && expanded
                .links
                .as_ref()
                .map(|links| links.iter().any(|link| link.memory_id == second.memory_id))
                .unwrap_or(false)
        {
            break;
        }
    }
    assert_eq!(done, 6);

    let expanded = v2
        .expand(&user_id, &first.memory_id, ExpandLevel::Links)
        .await
        .expect("expand final");
    assert!(expanded
        .links
        .as_ref()
        .map(|links| links.iter().any(|link| link.memory_id == second.memory_id))
        .unwrap_or(false));
    assert!(expanded
        .links
        .as_ref()
        .map(|links| links
            .iter()
            .all(|link| link.memory_id != v1_memory.memory_id))
        .unwrap_or(true));

    let direct_v2_links_to_v1 = sqlx::query(&format!(
        "SELECT COUNT(*) AS cnt FROM {} WHERE memory_id IN (?, ?) AND target_memory_id = ?",
        family.links_table
    ))
    .bind(&first.memory_id)
    .bind(&second.memory_id)
    .bind(&v1_memory.memory_id)
    .fetch_one(store.pool())
    .await
    .expect("v2 links to v1")
    .try_get::<i64, _>("cnt")
    .unwrap_or_default();
    assert_eq!(direct_v2_links_to_v1, 0);

    let v1_list = store
        .list_active(&user_id, 10)
        .await
        .expect("v1 list after jobs");
    assert_eq!(v1_list.len(), 1);
    assert_eq!(v1_list[0].memory_id, v1_memory.memory_id);

    let v1_after = sqlx::query(
        "SELECT content, is_active, superseded_by, updated_at FROM mem_memories WHERE memory_id = ?",
    )
    .bind(&v1_memory.memory_id)
    .fetch_one(store.pool())
    .await
    .expect("fetch v1 after");
    assert_eq!(
        v1_after.try_get::<String, _>("content").unwrap_or_default(),
        v1_before
            .try_get::<String, _>("content")
            .unwrap_or_default()
    );
    assert_eq!(
        v1_after.try_get::<i8, _>("is_active").unwrap_or_default(),
        v1_before.try_get::<i8, _>("is_active").unwrap_or_default()
    );
    assert_eq!(
        v1_after
            .try_get::<Option<String>, _>("superseded_by")
            .unwrap_or_default(),
        v1_before
            .try_get::<Option<String>, _>("superseded_by")
            .unwrap_or_default()
    );
    assert_eq!(
        v1_after
            .try_get::<chrono::NaiveDateTime, _>("updated_at")
            .expect("v1 updated_at after"),
        v1_updated_at_before
    );
}

#[tokio::test]
async fn test_v2_feedback_reorders_recall_and_related() {
    let (store, v2, user_id) = setup().await;
    let family = ensure_user_tables_ready(&store, &v2, &user_id).await;

    let anchor = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Anchor memory for feedback ranking".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-feedback".to_string()),
                importance: Some(0.2),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["shared".to_string(), "anchor".to_string()],
                source: None,
                embedding: Some(dim_vec(1, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember anchor");
    let positive = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Platform guide candidate positive".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-other".to_string()),
                importance: Some(0.2),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["shared".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 0.95)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember positive");
    let negative = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Platform guide candidate negative".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-feedback".to_string()),
                importance: Some(0.2),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["shared".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember negative");

    let baseline_recall = v2
        .recall(
            &user_id,
            RecallV2Request {
                query: "platform guide candidate".to_string(),
                top_k: 2,
                max_tokens: 200,
                session_only: false,
                session_id: None,
                memory_type: None,
                tags: vec![],
                tag_filter_mode: "any".to_string(),
                created_after: None,
                created_before: None,
                with_overview: false,
                with_links: false,
                expand_links: false,
                query_embedding: Some(dim_vec(0, 1.0)),
            },
        )
        .await
        .expect("baseline recall");
    assert_eq!(baseline_recall.memories.len(), 2);
    assert_eq!(baseline_recall.memories[0].id(), negative.memory_id);

    let mut baseline_related = None;
    for _ in 0..30 {
        v2.process_user_pending_jobs_with_enricher_pass(&user_id, 10, None)
            .await
            .expect("process pending jobs");
        let current = v2
            .related(
                &user_id,
                memoria_storage::MemoryV2RelatedRequest {
                    memory_id: anchor.memory_id.clone(),
                    limit: 10,
                    min_strength: 0.0,
                    max_hops: 1,
                },
            )
            .await
            .expect("baseline related");
        if current.items.len() >= 2 {
            baseline_related = Some(current);
            break;
        }
        baseline_related = Some(current);
    }
    let baseline_related = baseline_related.expect("baseline related result");
    assert_eq!(baseline_related.items.len(), 2);
    assert_eq!(baseline_related.items[0].memory_id, negative.memory_id);

    v2.record_feedback(&user_id, &positive.memory_id, "useful", Some("good match"))
        .await
        .expect("record useful");
    v2.record_feedback(
        &user_id,
        &positive.memory_id,
        "useful",
        Some("still useful"),
    )
    .await
    .expect("record useful again");
    v2.record_feedback(&user_id, &negative.memory_id, "wrong", Some("bad match"))
        .await
        .expect("record wrong");

    let feedback_rows = sqlx::query(&format!(
        "SELECT memory_id, feedback_useful, feedback_wrong FROM {} WHERE memory_id IN (?, ?) ORDER BY memory_id",
        family.stats_table
    ))
    .bind(&positive.memory_id)
    .bind(&negative.memory_id)
    .fetch_all(store.pool())
    .await
    .expect("feedback rows");
    assert_eq!(feedback_rows.len(), 2);
    assert!(feedback_rows.iter().any(|row| {
        row.try_get::<String, _>("memory_id").unwrap_or_default() == positive.memory_id
            && row.try_get::<i32, _>("feedback_useful").unwrap_or_default() == 2
    }));
    assert!(feedback_rows.iter().any(|row| {
        row.try_get::<String, _>("memory_id").unwrap_or_default() == negative.memory_id
            && row.try_get::<i32, _>("feedback_wrong").unwrap_or_default() == 1
    }));

    let feedback_log_count = sqlx::query(&format!(
        "SELECT COUNT(*) AS cnt FROM {} WHERE memory_id IN (?, ?)",
        family.feedback_table
    ))
    .bind(&positive.memory_id)
    .bind(&negative.memory_id)
    .fetch_one(store.pool())
    .await
    .expect("feedback log count")
    .try_get::<i64, _>("cnt")
    .unwrap_or_default();
    assert_eq!(feedback_log_count, 3);

    let boosted_recall = v2
        .recall(
            &user_id,
            RecallV2Request {
                query: "platform guide candidate".to_string(),
                top_k: 2,
                max_tokens: 200,
                session_only: false,
                session_id: None,
                memory_type: None,
                tags: vec![],
                tag_filter_mode: "any".to_string(),
                created_after: None,
                created_before: None,
                with_overview: false,
                with_links: false,
                expand_links: false,
                query_embedding: Some(dim_vec(0, 1.0)),
            },
        )
        .await
        .expect("boosted recall");
    assert_eq!(boosted_recall.memories[0].id(), positive.memory_id);

    let boosted_related = v2
        .related(
            &user_id,
            memoria_storage::MemoryV2RelatedRequest {
                memory_id: anchor.memory_id.clone(),
                limit: 10,
                min_strength: 0.0,
                max_hops: 1,
            },
        )
        .await
        .expect("boosted related");
    assert_eq!(boosted_related.items[0].memory_id, positive.memory_id);

    let invalid = v2
        .record_feedback(&user_id, &positive.memory_id, "bad_signal", None)
        .await
        .expect_err("invalid feedback should fail");
    assert!(matches!(invalid, MemoriaError::Validation(_)));
}

#[tokio::test]
async fn test_v2_feedback_stats_and_breakdown() {
    let (_store, v2, user_id) = setup().await;

    let trusted = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Trusted feedback target".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-feedback-stats".to_string()),
                importance: Some(0.3),
                trust_tier: Some(TrustTier::T1Verified),
                tags: vec!["stats".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember trusted");
    let curated = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Curated feedback target".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-feedback-stats".to_string()),
                importance: Some(0.3),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["stats".to_string()],
                source: None,
                embedding: Some(dim_vec(1, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember curated");

    v2.record_feedback(
        &user_id,
        &trusted.memory_id,
        "useful",
        Some("trusted useful"),
    )
    .await
    .expect("trusted useful");
    v2.record_feedback(
        &user_id,
        &curated.memory_id,
        "irrelevant",
        Some("curated irrelevant"),
    )
    .await
    .expect("curated irrelevant");
    v2.record_feedback(
        &user_id,
        &curated.memory_id,
        "outdated",
        Some("curated outdated"),
    )
    .await
    .expect("curated outdated");

    let stats = v2
        .get_feedback_stats(&user_id)
        .await
        .expect("feedback stats");
    assert_eq!(stats.total, 3);
    assert_eq!(stats.useful, 1);
    assert_eq!(stats.irrelevant, 1);
    assert_eq!(stats.outdated, 1);
    assert_eq!(stats.wrong, 0);

    let breakdown = v2
        .get_feedback_by_tier(&user_id)
        .await
        .expect("feedback by tier");
    assert!(breakdown
        .iter()
        .any(|item| item.tier == "T1" && item.signal == "useful" && item.count == 1));
    assert!(breakdown
        .iter()
        .any(|item| item.tier == "T2" && item.signal == "irrelevant" && item.count == 1));
    assert!(breakdown
        .iter()
        .any(|item| item.tier == "T2" && item.signal == "outdated" && item.count == 1));
}

#[tokio::test]
async fn test_v2_memory_history_reads_v2_events() {
    let (_store, v2, user_id) = setup().await;

    let remembered = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Legacy rust handbook for history".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-history".to_string()),
                importance: Some(0.3),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["legacy".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember");

    v2.update(
        &user_id,
        MemoryV2UpdateInput {
            memory_id: remembered.memory_id.clone(),
            content: Some("Updated rust handbook for history".to_string()),
            importance: Some(0.95),
            trust_tier: Some(TrustTier::T1Verified),
            tags_add: vec!["shared".to_string()],
            tags_remove: vec!["legacy".to_string()],
            reason: Some("clarified".to_string()),
            embedding: Some(dim_vec(1, 1.0)),
            actor: "tester".to_string(),
        },
    )
    .await
    .expect("update");

    v2.forget(&user_id, &remembered.memory_id, Some("cleanup"), "tester")
        .await
        .expect("forget");

    let history = v2
        .memory_history(&user_id, &remembered.memory_id, 10)
        .await
        .expect("history");
    assert_eq!(history.memory_id, remembered.memory_id);
    assert_eq!(history.items.len(), 3);
    assert_eq!(history.items[0].event_type, "forgotten");
    assert_eq!(history.items[0].actor, "tester");
    assert_eq!(history.items[0].processing_state, "committed");
    assert_eq!(history.items[0].payload["reason"], "cleanup");
    assert_eq!(history.items[1].event_type, "updated");
    assert_eq!(history.items[1].payload["reason"], "clarified");
    assert_eq!(history.items[1].payload["content_updated"], true);
    assert_eq!(history.items[2].event_type, "remembered");
    assert_eq!(history.items[2].payload["type"], "semantic");
    assert!(history.items[2].payload.get("memory_type").is_none());
    assert_eq!(history.items[2].payload["session_id"], "sess-history");

    let limited = v2
        .memory_history(&user_id, &remembered.memory_id, 2)
        .await
        .expect("limited history");
    assert_eq!(limited.items.len(), 2);
    assert_eq!(limited.items[0].event_type, "forgotten");
    assert_eq!(limited.items[1].event_type, "updated");
}

#[tokio::test]
async fn test_v2_stats_summarize_memory_state() {
    let (store, v2, user_id) = setup().await;
    let family = ensure_user_tables_ready(&store, &v2, &user_id).await;

    let first = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Rust platform guide for shared services".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-stats-a".to_string()),
                importance: Some(0.4),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["rust".to_string(), "guide".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember first");
    let second = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Rust infrastructure handbook for platform teams".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-stats-b".to_string()),
                importance: Some(0.5),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["rust".to_string(), "infra".to_string()],
                source: None,
                embedding: Some(dim_vec(1, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember second");
    let archived = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Archived incident note for retired service".to_string(),
                memory_type: MemoryType::Episodic,
                session_id: Some("sess-stats-c".to_string()),
                importance: Some(0.1),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["archive".to_string()],
                source: None,
                embedding: Some(dim_vec(2, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember archived");

    let current_done = wait_for_current_jobs_done(
        &store,
        &v2,
        &family,
        &user_id,
        &first.memory_id,
        &second.memory_id,
        Some(&MockV2Enricher),
        4,
    )
    .await;
    assert_eq!(current_done, 4);

    v2.focus(
        &user_id,
        FocusV2Input {
            focus_type: "tag".to_string(),
            value: "rust".to_string(),
            boost: Some(1.5),
            ttl_secs: Some(300),
            actor: "tester".to_string(),
        },
    )
    .await
    .expect("focus");

    v2.record_feedback(&user_id, &first.memory_id, "useful", Some("first useful"))
        .await
        .expect("first feedback");
    v2.record_feedback(
        &user_id,
        &archived.memory_id,
        "wrong",
        Some("archived wrong"),
    )
    .await
    .expect("archived feedback");

    v2.forget(&user_id, &archived.memory_id, Some("archive"), "tester")
        .await
        .expect("forget archived");

    let stats = v2.stats(&user_id).await.expect("stats");
    assert_eq!(stats.total_memories, 3);
    assert_eq!(stats.active_memories, 2);
    assert_eq!(stats.forgotten_memories, 1);
    assert_eq!(stats.distinct_sessions, 2);
    assert_eq!(stats.has_overview_count, 0);
    assert_eq!(stats.has_detail_count, 0);
    assert!(stats.active_direct_links >= 1);
    assert_eq!(stats.active_focus_count, 1);
    assert_eq!(stats.tags.unique_count, 3);
    assert_eq!(stats.tags.assignment_count, 4);
    assert_eq!(stats.jobs.total_count, 6);
    assert_eq!(stats.jobs.pending_count, 0);
    assert_eq!(stats.jobs.in_progress_count, 0);
    assert_eq!(stats.jobs.done_count, 6);
    assert_eq!(stats.jobs.failed_count, 0);
    assert_eq!(stats.feedback.total, 2);
    assert_eq!(stats.feedback.useful, 1);
    assert_eq!(stats.feedback.irrelevant, 0);
    assert_eq!(stats.feedback.outdated, 0);
    assert_eq!(stats.feedback.wrong, 1);
    assert!(stats
        .by_type
        .iter()
        .any(|item| item.memory_type == "semantic"
            && item.total_count == 2
            && item.active_count == 2
            && item.forgotten_count == 0));
    assert!(stats
        .by_type
        .iter()
        .any(|item| item.memory_type == "episodic"
            && item.total_count == 1
            && item.active_count == 0
            && item.forgotten_count == 1));
}

#[tokio::test]
async fn test_v2_memory_feedback_summary() {
    let (_store, v2, user_id) = setup().await;

    let remembered = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Memory feedback summary target".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-feedback-read".to_string()),
                importance: Some(0.2),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["feedback".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember");

    let empty = v2
        .get_memory_feedback(&user_id, &remembered.memory_id)
        .await
        .expect("empty summary");
    assert_eq!(empty.memory_id, remembered.memory_id);
    assert_eq!(empty.feedback.useful, 0);
    assert_eq!(empty.feedback.irrelevant, 0);
    assert_eq!(empty.feedback.outdated, 0);
    assert_eq!(empty.feedback.wrong, 0);
    assert!(empty.last_feedback_at.is_none());

    v2.record_feedback(&user_id, &remembered.memory_id, "useful", Some("great"))
        .await
        .expect("record useful");
    v2.record_feedback(&user_id, &remembered.memory_id, "wrong", Some("bad"))
        .await
        .expect("record wrong");

    let summary = v2
        .get_memory_feedback(&user_id, &remembered.memory_id)
        .await
        .expect("feedback summary");
    assert_eq!(summary.memory_id, remembered.memory_id);
    assert_eq!(summary.feedback.useful, 1);
    assert_eq!(summary.feedback.irrelevant, 0);
    assert_eq!(summary.feedback.outdated, 0);
    assert_eq!(summary.feedback.wrong, 1);
    assert!(summary.last_feedback_at.is_some());

    let missing = v2
        .get_memory_feedback(&user_id, "missing-memory")
        .await
        .expect_err("missing summary should fail");
    assert!(matches!(missing, MemoriaError::NotFound(_)));
}

#[tokio::test]
async fn test_v2_memory_feedback_history() {
    let (_store, v2, user_id) = setup().await;

    let remembered = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Memory feedback history target".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-feedback-history".to_string()),
                importance: Some(0.2),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["feedback".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember");

    v2.record_feedback(&user_id, &remembered.memory_id, "useful", Some("first"))
        .await
        .expect("record first");
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    v2.record_feedback(&user_id, &remembered.memory_id, "wrong", Some("second"))
        .await
        .expect("record second");
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    v2.record_feedback(&user_id, &remembered.memory_id, "outdated", None)
        .await
        .expect("record third");

    let history = v2
        .get_memory_feedback_history(&user_id, &remembered.memory_id, 2)
        .await
        .expect("feedback history");
    assert_eq!(history.memory_id, remembered.memory_id);
    assert_eq!(history.items.len(), 2);
    assert_eq!(history.items[0].signal, "outdated");
    assert!(history.items[0].context.is_none());
    assert_eq!(history.items[1].signal, "wrong");
    assert_eq!(history.items[1].context.as_deref(), Some("second"));
    assert!(!history.items[0].feedback_id.is_empty());
    assert!(history.items[0].created_at >= history.items[1].created_at);

    let full_history = v2
        .get_memory_feedback_history(&user_id, &remembered.memory_id, 10)
        .await
        .expect("full feedback history");
    assert_eq!(full_history.items.len(), 3);
    assert!(full_history
        .items
        .iter()
        .any(|item| item.signal == "useful" && item.context.as_deref() == Some("first")));

    let missing = v2
        .get_memory_feedback_history(&user_id, "missing-memory", 10)
        .await
        .expect_err("missing history should fail");
    assert!(matches!(missing, MemoriaError::NotFound(_)));
}

#[tokio::test]
async fn test_v2_feedback_feed_filters() {
    let (store, v2, user_id) = setup().await;
    let _family = ensure_user_tables_ready(&store, &v2, &user_id).await;
    let first = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Alpha feedback feed memory".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-feedback-feed".to_string()),
                importance: Some(0.5),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["feedback".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember first");
    let second = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Beta feedback feed memory".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-feedback-feed".to_string()),
                importance: Some(0.5),
                trust_tier: Some(TrustTier::T1Verified),
                tags: vec!["feedback".to_string()],
                source: None,
                embedding: Some(dim_vec(1, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember second");

    v2.record_feedback(&user_id, &first.memory_id, "useful", Some("alpha useful"))
        .await
        .expect("record first useful");
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    v2.record_feedback(&user_id, &second.memory_id, "wrong", Some("beta wrong"))
        .await
        .expect("record second wrong");
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    v2.record_feedback(&user_id, &first.memory_id, "outdated", None)
        .await
        .expect("record first outdated");

    let full = v2
        .list_feedback_history(&user_id, None, None, 10)
        .await
        .expect("full feedback feed");
    assert_eq!(full.items.len(), 3);
    assert_eq!(full.items[0].signal, "outdated");
    assert_eq!(full.items[0].memory_id, first.memory_id);
    assert_eq!(full.items[1].signal, "wrong");
    assert_eq!(full.items[1].memory_id, second.memory_id);
    assert_eq!(full.items[2].signal, "useful");
    assert_eq!(full.items[2].memory_id, first.memory_id);
    assert_eq!(
        full.items[0].abstract_text.as_deref(),
        Some(first.abstract_text.as_str())
    );
    assert_eq!(
        full.items[1].abstract_text.as_deref(),
        Some(second.abstract_text.as_str())
    );

    let memory_filtered = v2
        .list_feedback_history(&user_id, Some(&first.memory_id), None, 10)
        .await
        .expect("memory filtered feed");
    assert_eq!(memory_filtered.items.len(), 2);
    assert!(memory_filtered
        .items
        .iter()
        .all(|item| item.memory_id == first.memory_id));

    let signal_filtered = v2
        .list_feedback_history(&user_id, None, Some("wrong"), 10)
        .await
        .expect("signal filtered feed");
    assert_eq!(signal_filtered.items.len(), 1);
    assert_eq!(signal_filtered.items[0].memory_id, second.memory_id);
    assert_eq!(
        signal_filtered.items[0].context.as_deref(),
        Some("beta wrong")
    );

    let invalid = v2
        .list_feedback_history(&user_id, None, Some("bad_signal"), 10)
        .await
        .expect_err("invalid signal filter should fail");
    assert!(matches!(invalid, MemoriaError::Validation(_)));
}

#[tokio::test]
async fn test_v2_links_and_related_navigation() {
    let (store, v2, user_id) = setup().await;
    let family = ensure_user_tables_ready(&store, &v2, &user_id).await;
    let first = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Alpha rust handbook for platform teams".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-links".to_string()),
                importance: Some(0.4),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["shared".to_string(), "alpha".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember first");
    let middle = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Shared beta rust guide for platform teams".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-links".to_string()),
                importance: Some(0.5),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["shared".to_string(), "beta".to_string()],
                source: None,
                embedding: Some(dim_vec(1, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember middle");
    let third = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Gamma beta operations guide for platform teams".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-links".to_string()),
                importance: Some(0.4),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["beta".to_string(), "gamma".to_string()],
                source: None,
                embedding: Some(dim_vec(2, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember third");

    for _ in 0..30 {
        v2.process_user_pending_jobs_pass(&user_id, 10)
            .await
            .expect("process jobs");
        let links = v2
            .links(
                &user_id,
                MemoryV2LinksRequest {
                    memory_id: middle.memory_id.clone(),
                    direction: LinkDirection::Both,
                    limit: 10,
                    link_type: Some("tag_overlap".to_string()),
                    min_strength: 0.0,
                },
            )
            .await
            .expect("links");
        if links.items.len() >= 4 {
            break;
        }
    }

    let outbound = v2
        .links(
            &user_id,
            MemoryV2LinksRequest {
                memory_id: middle.memory_id.clone(),
                direction: LinkDirection::Outbound,
                limit: 10,
                link_type: Some("tag_overlap".to_string()),
                min_strength: 0.0,
            },
        )
        .await
        .expect("outbound links");
    assert_eq!(outbound.items.len(), 2);
    assert_eq!(outbound.summary.outbound_count, 2);
    assert_eq!(outbound.summary.inbound_count, 2);
    assert_eq!(outbound.summary.total_count, 4);
    assert!(outbound.summary.link_types.iter().any(|item| {
        item.link_type == "tag_overlap" && item.outbound_count >= 1 && item.inbound_count >= 1
    }));
    assert_eq!(
        outbound
            .summary
            .link_types
            .iter()
            .map(|item| item.outbound_count + item.inbound_count)
            .sum::<i64>(),
        4
    );
    assert!(outbound
        .items
        .iter()
        .all(|item| item.direction == LinkDirection::Outbound));
    assert!(outbound
        .items
        .iter()
        .all(|item| item.provenance.primary_evidence_type.is_some()));
    assert!(outbound
        .items
        .iter()
        .all(|item| item.provenance.primary_evidence_strength.is_some()));
    assert!(outbound.items.iter().all(|item| item
        .provenance
        .evidence_types
        .iter()
        .any(|kind| kind == "tag_overlap")));
    assert!(outbound
        .items
        .iter()
        .all(|item| item.provenance.evidence.len() == 1));
    assert!(outbound.items.iter().all(|item| {
        item.provenance.evidence.iter().any(|detail| {
            detail.evidence_type == "tag_overlap"
                && detail.overlap_count == Some(1)
                && detail.source_tag_count == Some(2)
                && detail.target_tag_count.is_some()
                && detail.vector_distance.is_none()
        })
    }));
    assert!(outbound.items.iter().all(|item| {
        item.provenance
            .extraction_trace
            .as_ref()
            .map(|trace| {
                trace.content_version_id.is_some()
                    && trace.derivation_state.as_deref() == Some("complete")
                    && trace.latest_job_status.as_deref() == Some("done")
                    && trace.latest_job_attempts == Some(1)
                    && trace.latest_job_updated_at.is_some()
            })
            .unwrap_or(false)
    }));
    assert!(outbound.items.iter().all(|item| !item.provenance.refined));
    assert!(outbound
        .items
        .iter()
        .any(|item| item.memory_id == first.memory_id));
    assert!(outbound
        .items
        .iter()
        .any(|item| item.memory_id == third.memory_id));

    let expanded = v2
        .expand(&user_id, &middle.memory_id, ExpandLevel::Links)
        .await
        .expect("expand links with provenance");
    let expanded_links = expanded.links.expect("expanded links");
    assert_eq!(expanded_links.len(), 2);
    assert!(expanded_links
        .iter()
        .all(|link| link.provenance.primary_evidence_type.is_some()));
    assert!(expanded_links
        .iter()
        .all(|link| link.provenance.primary_evidence_strength.is_some()));
    assert!(expanded_links.iter().all(|link| link
        .provenance
        .evidence_types
        .iter()
        .any(|kind| kind == "tag_overlap")));
    assert!(expanded_links.iter().all(|link| !link.provenance.refined));
    assert!(expanded_links.iter().all(|link| {
        link.provenance
            .extraction_trace
            .as_ref()
            .map(|trace| trace.latest_job_status.as_deref() == Some("done"))
            .unwrap_or(false)
    }));

    let recalled = v2
        .recall(
            &user_id,
            RecallV2Request {
                query: "shared beta rust".to_string(),
                top_k: 3,
                max_tokens: 500,
                session_only: true,
                session_id: Some("sess-links".to_string()),
                memory_type: Some(MemoryType::Semantic),
                tags: vec![],
                tag_filter_mode: "any".to_string(),
                created_after: None,
                created_before: None,
                with_overview: false,
                with_links: true,
                expand_links: false,
                query_embedding: Some(dim_vec(1, 1.0)),
            },
        )
        .await
        .expect("recall with inline links");
    let recalled_middle = recalled
        .memories
        .iter()
        .find(|item| item.memory_id == middle.memory_id)
        .expect("recalled middle");
    let recalled_links = recalled_middle.links.as_ref().expect("recalled links");
    assert_eq!(recalled_links.len(), 2);
    assert!(recalled_links
        .iter()
        .all(|link| link.provenance.primary_evidence_type.is_some()));
    assert!(recalled_links.iter().all(|link| !link.provenance.refined));
    assert!(recalled_links
        .iter()
        .all(|link| link.provenance.evidence.len() == 1));
    assert!(recalled_links.iter().all(|link| {
        link.provenance
            .extraction_trace
            .as_ref()
            .map(|trace| trace.latest_job_status.as_deref() == Some("done"))
            .unwrap_or(false)
    }));

    let inbound = v2
        .links(
            &user_id,
            MemoryV2LinksRequest {
                memory_id: middle.memory_id.clone(),
                direction: LinkDirection::Inbound,
                limit: 10,
                link_type: Some("tag_overlap".to_string()),
                min_strength: 0.0,
            },
        )
        .await
        .expect("inbound links");
    assert_eq!(inbound.items.len(), 2);
    assert_eq!(inbound.summary.total_count, 4);
    assert!(inbound
        .items
        .iter()
        .all(|item| item.direction == LinkDirection::Inbound));

    let related = v2
        .related(
            &user_id,
            MemoryV2RelatedRequest {
                memory_id: middle.memory_id.clone(),
                limit: 10,
                min_strength: 0.0,
                max_hops: 1,
            },
        )
        .await
        .expect("related");
    assert_eq!(related.items.len(), 2);
    assert_eq!(related.summary.discovered_count, 2);
    assert_eq!(related.summary.returned_count, 2);
    assert!(!related.summary.truncated);
    assert_eq!(related.summary.by_hop.len(), 1);
    assert_eq!(related.summary.by_hop[0].hop_distance, 1);
    assert_eq!(related.summary.by_hop[0].count, 2);
    assert!(related
        .summary
        .link_types
        .iter()
        .any(|item| item.link_type == "tag_overlap"));
    assert!(related.items.iter().any(|item| {
        item.memory_id == first.memory_id
            && item.directions == vec![LinkDirection::Outbound, LinkDirection::Inbound]
            && item.link_types.iter().any(|ty| ty == "tag_overlap")
            && item.lineage.len() == 1
            && item.lineage[0].from_memory_id == middle.memory_id
            && item.lineage[0].to_memory_id == first.memory_id
            && item.lineage[0].direction == LinkDirection::Inbound
            && item.lineage[0].provenance.primary_evidence_type.as_deref() == Some("tag_overlap")
    }));
    assert!(related.items.iter().any(|item| {
        item.memory_id == third.memory_id
            && item.lineage.len() == 1
            && item.lineage[0].from_memory_id == middle.memory_id
            && item.lineage[0].to_memory_id == third.memory_id
            && item.lineage[0].direction == LinkDirection::Inbound
    }));

    v2.forget(&user_id, &third.memory_id, Some("cleanup"), "tester")
        .await
        .expect("forget third");
    let links_after_forget = v2
        .links(
            &user_id,
            MemoryV2LinksRequest {
                memory_id: middle.memory_id.clone(),
                direction: LinkDirection::Both,
                limit: 10,
                link_type: Some("tag_overlap".to_string()),
                min_strength: 0.0,
            },
        )
        .await
        .expect("links after forget");
    assert_eq!(links_after_forget.summary.outbound_count, 1);
    assert_eq!(links_after_forget.summary.inbound_count, 1);
    assert_eq!(links_after_forget.summary.total_count, 2);
    let after_forget = v2
        .related(
            &user_id,
            MemoryV2RelatedRequest {
                memory_id: middle.memory_id.clone(),
                limit: 10,
                min_strength: 0.0,
                max_hops: 1,
            },
        )
        .await
        .expect("related after forget");
    assert_eq!(after_forget.items.len(), 1);
    assert_eq!(after_forget.summary.discovered_count, 1);
    assert_eq!(after_forget.summary.returned_count, 1);
    assert!(!after_forget.summary.truncated);
    assert_eq!(after_forget.items[0].memory_id, first.memory_id);

    let stats_rows = sqlx::query(&format!(
        "SELECT memory_id, access_count FROM {} WHERE memory_id IN (?, ?)",
        family.stats_table
    ))
    .bind(&first.memory_id)
    .bind(&third.memory_id)
    .fetch_all(store.pool())
    .await
    .expect("stats rows");
    assert!(stats_rows.iter().any(|row| {
        row.try_get::<String, _>("memory_id").unwrap_or_default() == first.memory_id
            && row.try_get::<i32, _>("access_count").unwrap_or_default() >= 1
    }));
}

#[tokio::test]
async fn test_v2_job_observability() {
    let (_store, v2, user_id) = setup().await;
    let remembered = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Observable async enrichment job for V2 memory processing and view derivation pipeline. This content has been made sufficiently long to trigger the derive_views background job. The system enriches memories with overview and detail text derived from the full source content for retrieval purposes.".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-jobs-view".to_string()),
                importance: Some(0.5),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["jobs".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember");

    let initial = v2
        .jobs(
            &user_id,
            MemoryV2JobsRequest {
                memory_id: remembered.memory_id.clone(),
                limit: 10,
            },
        )
        .await
        .expect("initial jobs");
    assert_eq!(initial.pending_count, 3);
    assert_eq!(initial.in_progress_count, 0);
    assert_eq!(initial.done_count, 0);
    assert_eq!(initial.failed_count, 0);
    assert_eq!(initial.items.len(), 3);
    assert_eq!(initial.derivation_state, "pending");
    assert_eq!(initial.job_types.len(), 3);
    assert!(initial.job_types.iter().all(|item| item.pending_count == 1));
    assert!(initial
        .job_types
        .iter()
        .any(|item| item.job_type == "extract_entities"));
    assert!(initial
        .job_types
        .iter()
        .all(|item| item.latest_status == "pending"));
    assert!(initial.items.iter().all(|item| item.status == "pending"));
    assert!(!initial.has_overview);
    assert!(!initial.has_detail);
    assert_eq!(initial.link_count, 0);

    for _ in 0..20 {
        v2.process_user_pending_jobs_pass(&user_id, 10)
            .await
            .expect("process jobs");
        let current = v2
            .jobs(
                &user_id,
                MemoryV2JobsRequest {
                    memory_id: remembered.memory_id.clone(),
                    limit: 10,
                },
            )
            .await
            .expect("current jobs");
        if current.done_count == 3 && current.has_overview && current.has_detail {
            assert_eq!(current.pending_count, 0);
            assert_eq!(current.failed_count, 0);
            assert_eq!(current.derivation_state, "complete");
            assert_eq!(current.job_types.len(), 3);
            assert!(current.job_types.iter().all(|item| item.done_count == 1));
            assert!(current
                .job_types
                .iter()
                .all(|item| item.latest_status == "done"));
            assert!(current.items.iter().all(|item| item.status == "done"));
            return;
        }
    }

    panic!("expected V2 jobs to finish and derived views to appear");
}

#[tokio::test]
async fn test_v2_related_supports_multi_hop_traversal() {
    let (_store, v2, user_id) = setup().await;
    let first = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Alpha memory linked to bridge".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-hops".to_string()),
                importance: Some(0.4),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["alpha".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember first");
    let middle = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Bridge memory linked to alpha and beta".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-hops".to_string()),
                importance: Some(0.5),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["alpha".to_string(), "beta".to_string()],
                source: None,
                embedding: Some(dim_vec(1, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember middle");
    let third = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Beta memory reachable in two hops".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-hops".to_string()),
                importance: Some(0.4),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["beta".to_string()],
                source: None,
                embedding: Some(dim_vec(2, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember third");

    for _ in 0..30 {
        v2.process_user_pending_jobs_pass(&user_id, 10)
            .await
            .expect("process jobs");
        let bridge_links = v2
            .links(
                &user_id,
                MemoryV2LinksRequest {
                    memory_id: middle.memory_id.clone(),
                    direction: LinkDirection::Outbound,
                    limit: 10,
                    link_type: Some("tag_overlap".to_string()),
                    min_strength: 0.0,
                },
            )
            .await
            .expect("bridge links");
        if bridge_links.items.len() >= 2 {
            break;
        }
    }

    let direct_only = v2
        .related(
            &user_id,
            MemoryV2RelatedRequest {
                memory_id: first.memory_id.clone(),
                limit: 10,
                min_strength: 0.0,
                max_hops: 1,
            },
        )
        .await
        .expect("direct related");
    assert_eq!(direct_only.items.len(), 1);
    assert_eq!(direct_only.summary.discovered_count, 1);
    assert_eq!(direct_only.summary.returned_count, 1);
    assert!(!direct_only.summary.truncated);
    assert_eq!(direct_only.items[0].memory_id, middle.memory_id);
    assert_eq!(direct_only.items[0].hop_distance, 1);
    assert!(direct_only.items[0].via_memory_ids.is_empty());
    assert_eq!(direct_only.items[0].lineage.len(), 1);
    assert!(direct_only.items[0].supporting_path_count >= 1);
    assert_eq!(
        direct_only.items[0].supporting_paths_truncated,
        direct_only.items[0].supporting_path_count as usize
            > direct_only.items[0].supporting_paths.len()
    );
    assert!(direct_only.items[0]
        .supporting_paths
        .iter()
        .enumerate()
        .all(|(index, path)| path.path_rank == index as i64 + 1));
    let selected_direct_paths = direct_only.items[0]
        .supporting_paths
        .iter()
        .filter(|path| path.selected)
        .collect::<Vec<_>>();
    assert_eq!(selected_direct_paths.len(), 1);
    assert_eq!(selected_direct_paths[0].path_rank, 1);
    assert_eq!(selected_direct_paths[0].hop_distance, 1);
    assert!(selected_direct_paths[0].via_memory_ids.is_empty());
    assert_eq!(selected_direct_paths[0].lineage.len(), 1);
    assert_eq!(selected_direct_paths[0].selection_reason, "best_path");
    assert!(direct_only.items[0]
        .supporting_paths
        .iter()
        .filter(|path| !path.selected)
        .all(|path| matches!(
            path.selection_reason.as_str(),
            "higher_hop_distance" | "lower_strength" | "tie_break"
        )));
    assert_eq!(
        direct_only.items[0].lineage[0].from_memory_id,
        first.memory_id
    );
    assert_eq!(
        direct_only.items[0].lineage[0].to_memory_id,
        middle.memory_id
    );
    assert_eq!(
        direct_only.items[0].lineage[0].direction,
        LinkDirection::Outbound
    );
    assert_eq!(
        direct_only.items[0].lineage[0]
            .provenance
            .primary_evidence_type
            .as_deref(),
        Some("tag_overlap")
    );
    assert_eq!(
        direct_only.items[0].lineage[0]
            .provenance
            .extraction_trace
            .as_ref()
            .and_then(|trace| trace.latest_job_status.as_deref()),
        Some("done")
    );

    let multi_hop = v2
        .related(
            &user_id,
            MemoryV2RelatedRequest {
                memory_id: first.memory_id.clone(),
                limit: 10,
                min_strength: 0.0,
                max_hops: 2,
            },
        )
        .await
        .expect("multi hop related");
    assert_eq!(multi_hop.items.len(), 2);
    assert_eq!(multi_hop.summary.discovered_count, 2);
    assert_eq!(multi_hop.summary.returned_count, 2);
    assert!(!multi_hop.summary.truncated);
    assert!(multi_hop
        .summary
        .by_hop
        .iter()
        .any(|item| item.hop_distance == 1 && item.count == 1));
    assert!(multi_hop
        .summary
        .by_hop
        .iter()
        .any(|item| item.hop_distance == 2 && item.count == 1));
    assert_eq!(multi_hop.items[0].memory_id, middle.memory_id);
    assert_eq!(multi_hop.items[0].hop_distance, 1);
    assert!(multi_hop.items[0].via_memory_ids.is_empty());
    assert!(multi_hop.items.iter().any(|item| {
        let selected_paths = item
            .supporting_paths
            .iter()
            .filter(|path| path.selected)
            .collect::<Vec<_>>();
        item.memory_id == third.memory_id
            && item.hop_distance == 2
            && item.via_memory_ids == vec![middle.memory_id.clone()]
            && item.lineage.len() == 2
            && item.supporting_path_count >= 1
            && item.supporting_paths_truncated
                == (item.supporting_path_count as usize > item.supporting_paths.len())
            && item
                .supporting_paths
                .iter()
                .enumerate()
                .all(|(index, path)| path.path_rank == index as i64 + 1)
            && selected_paths.len() == 1
            && selected_paths[0].path_rank == 1
            && selected_paths[0].hop_distance == 2
            && selected_paths[0].via_memory_ids == vec![middle.memory_id.clone()]
            && selected_paths[0].lineage.len() == 2
            && selected_paths[0].selection_reason == "best_path"
            && item
                .supporting_paths
                .iter()
                .filter(|path| !path.selected)
                .all(|path| {
                    matches!(
                        path.selection_reason.as_str(),
                        "higher_hop_distance" | "lower_strength" | "tie_break"
                    )
                })
            && item.lineage[0].from_memory_id == first.memory_id
            && item.lineage[0].to_memory_id == middle.memory_id
            && item.lineage[0].direction == LinkDirection::Outbound
            && item.lineage[1].from_memory_id == middle.memory_id
            && item.lineage[1].to_memory_id == third.memory_id
            && item.lineage[1].direction == LinkDirection::Inbound
    }));

    let truncated = v2
        .related(
            &user_id,
            MemoryV2RelatedRequest {
                memory_id: first.memory_id.clone(),
                limit: 1,
                min_strength: 0.0,
                max_hops: 2,
            },
        )
        .await
        .expect("truncated related");
    assert_eq!(truncated.summary.discovered_count, 2);
    assert_eq!(truncated.summary.returned_count, 1);
    assert!(truncated.summary.truncated);
    assert_eq!(truncated.items.len(), 1);
}

#[tokio::test]
async fn test_v2_related_reorders_with_session_affinity_and_focus() {
    let (_store, v2, user_id) = setup().await;
    let root = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Root memory for related ranking".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-related-rank".to_string()),
                importance: Some(0.4),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["shared".to_string(), "root".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember root");
    let focused_other = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Priority memory in another session".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-other".to_string()),
                importance: Some(0.4),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["shared".to_string(), "priority".to_string()],
                source: None,
                embedding: Some(dim_vec(1, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember focused other");
    let same_session = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Same-session related memory".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-related-rank".to_string()),
                importance: Some(0.4),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["shared".to_string(), "same".to_string()],
                source: None,
                embedding: Some(dim_vec(2, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember same session");

    for _ in 0..30 {
        v2.process_user_pending_jobs_pass(&user_id, 10)
            .await
            .expect("process jobs");
        let root_links = v2
            .links(
                &user_id,
                MemoryV2LinksRequest {
                    memory_id: root.memory_id.clone(),
                    direction: LinkDirection::Outbound,
                    limit: 10,
                    link_type: Some("tag_overlap".to_string()),
                    min_strength: 0.0,
                },
            )
            .await
            .expect("root links");
        if root_links.items.len() >= 2 {
            break;
        }
    }

    let baseline = v2
        .related(
            &user_id,
            MemoryV2RelatedRequest {
                memory_id: root.memory_id.clone(),
                limit: 10,
                min_strength: 0.0,
                max_hops: 1,
            },
        )
        .await
        .expect("baseline related");
    assert_eq!(baseline.items.len(), 2);
    assert_eq!(baseline.items[0].memory_id, same_session.memory_id);
    assert!(baseline.items[0].ranking.session_affinity_applied);
    assert_eq!(baseline.items[0].ranking.session_affinity_multiplier, 1.08);
    assert_eq!(baseline.items[0].ranking.focus_boost, 1.0);
    assert!(baseline.items[0].ranking.focus_matches.is_empty());

    v2.focus(
        &user_id,
        FocusV2Input {
            focus_type: "tag".to_string(),
            value: "priority".to_string(),
            boost: Some(3.0),
            ttl_secs: Some(300),
            actor: "tester".to_string(),
        },
    )
    .await
    .expect("focus priority tag");

    let focused = v2
        .related(
            &user_id,
            MemoryV2RelatedRequest {
                memory_id: root.memory_id.clone(),
                limit: 10,
                min_strength: 0.0,
                max_hops: 1,
            },
        )
        .await
        .expect("focused related");
    assert_eq!(focused.items.len(), 2);
    assert_eq!(focused.items[0].memory_id, focused_other.memory_id);
    assert_eq!(focused.items[0].ranking.focus_boost, 3.0);
    assert!(focused.items[0]
        .ranking
        .focus_matches
        .iter()
        .any(|focus| focus.focus_type == "tag" && focus.value == "priority" && focus.boost == 3.0));
    assert!(focused.items[0].ranking.same_hop_score > focused.items[1].ranking.same_hop_score);
}

#[tokio::test]
async fn test_v2_update_content_and_tags() {
    let (store, v2, user_id) = setup().await;
    let family = ensure_user_tables_ready(&store, &v2, &user_id).await;
    let first = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Legacy rust handbook for platform teams".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-update".to_string()),
                importance: Some(0.2),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["legacy".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 0.8)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember first");
    let second = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Shared rust service guide for platform teams".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-update".to_string()),
                importance: Some(0.3),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["shared".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember second");

    let updated = v2
        .update(
            &user_id,
            MemoryV2UpdateInput {
                memory_id: first.memory_id.clone(),
                content: Some("Updated rust handbook for shared platform teams covering the complete Rust development lifecycle. This guide helps platform engineers understand ownership, borrowing, and lifetimes in shared service contexts. Teams building on shared platform infrastructure benefit from Rust safety guarantees.".to_string()),
                importance: Some(0.9),
                trust_tier: Some(TrustTier::T1Verified),
                tags_add: vec!["shared".to_string(), "verified".to_string()],
                tags_remove: vec!["legacy".to_string()],
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
                reason: Some("clarified".to_string()),
            },
        )
        .await
        .expect("update");
    assert!(updated
        .abstract_text
        .starts_with("Updated rust handbook for shared platform teams"));
    assert!(!updated.has_overview);
    assert!(!updated.has_detail);

    let shared_rows = sqlx::query(&format!(
        "SELECT COUNT(*) AS cnt FROM {} WHERE tag = ?",
        family.tags_table
    ))
    .bind("shared")
    .fetch_one(store.pool())
    .await
    .expect("shared tag rows")
    .try_get::<i64, _>("cnt")
    .unwrap_or_default();
    assert_eq!(shared_rows, 2);

    let verified_rows = sqlx::query(&format!(
        "SELECT COUNT(*) AS cnt FROM {} WHERE tag = ?",
        family.tags_table
    ))
    .bind("verified")
    .fetch_one(store.pool())
    .await
    .expect("verified tag rows")
    .try_get::<i64, _>("cnt")
    .unwrap_or_default();
    assert_eq!(verified_rows, 1);

    let legacy_rows = sqlx::query(&format!(
        "SELECT COUNT(*) AS cnt FROM {} WHERE tag = ?",
        family.tags_table
    ))
    .bind("legacy")
    .fetch_one(store.pool())
    .await
    .expect("legacy tag rows")
    .try_get::<i64, _>("cnt")
    .unwrap_or_default();
    assert_eq!(legacy_rows, 0);

    let tags = v2.list_tags(&user_id, 10, None).await.expect("list tags");
    assert!(tags
        .iter()
        .any(|tag| tag.tag == "shared" && tag.memory_count == 2));
    assert!(tags
        .iter()
        .any(|tag| tag.tag == "verified" && tag.memory_count == 1));
    assert!(!tags.iter().any(|tag| tag.tag == "legacy"));

    let filtered_tags = v2
        .list_tags(&user_id, 10, Some("ver"))
        .await
        .expect("filtered tags");
    assert_eq!(filtered_tags.len(), 1);
    assert_eq!(filtered_tags[0].tag, "verified");

    for _ in 0..20 {
        v2.process_user_pending_jobs_pass(&user_id, 10)
            .await
            .expect("process user jobs");
        let expanded = v2
            .expand(&user_id, &first.memory_id, ExpandLevel::Links)
            .await
            .expect("expand");
        if expanded
            .links
            .as_ref()
            .map(|links| links.iter().any(|link| link.memory_id == second.memory_id))
            .unwrap_or(false)
            && expanded
                .detail_text
                .as_deref()
                .unwrap_or("")
                .contains("shared platform teams")
        {
            return;
        }
    }

    panic!("expected update-triggered enrichment to complete");
}

#[tokio::test]
async fn test_v2_process_pending_jobs_derives_views_and_links() {
    let (store, v2, user_id) = setup().await;
    let family = ensure_user_tables_ready(&store, &v2, &user_id).await;

    let first = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Rust ownership and borrowing keep systems code safe against memory errors and data races. Teams use Rust for reliable infrastructure because compile-time checks prevent entire classes of bugs. Platform engineers adopt Rust for critical services where both performance and memory safety matter greatly.".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-jobs".to_string()),
                importance: Some(0.6),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["rust".to_string(), "infra".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember first");
    let second = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Rust platform guide for infrastructure teams working on shared services and distributed systems. This reference covers patterns used by platform engineers building reliable Rust services. Memory safety and zero-cost abstractions are key features valued by the infrastructure community.".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-jobs".to_string()),
                importance: Some(0.5),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["rust".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 0.9)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember second");

    let current_done = wait_for_current_jobs_done(
        &store,
        &v2,
        &family,
        &user_id,
        &first.memory_id,
        &second.memory_id,
        None,
        6,
    )
    .await;
    assert_eq!(current_done, 6);

    let detail = v2
        .expand(&user_id, &first.memory_id, ExpandLevel::Detail)
        .await
        .expect("expand detail");
    assert!(detail
        .overview_text
        .as_deref()
        .unwrap_or("")
        .contains("Rust"));
    assert!(detail
        .detail_text
        .as_deref()
        .unwrap_or("")
        .contains("infrastructure"));

    let links = v2
        .expand(&user_id, &first.memory_id, ExpandLevel::Links)
        .await
        .expect("expand links");
    let link_refs = links.links.expect("links");
    assert!(link_refs
        .iter()
        .any(|link| link.memory_id == second.memory_id));

    let jobs_done = sqlx::query(&format!(
        "SELECT COUNT(*) AS cnt FROM {} WHERE status = 'done' AND aggregate_id IN (?, ?)",
        family.jobs_table
    ))
    .bind(&first.memory_id)
    .bind(&second.memory_id)
    .fetch_one(store.pool())
    .await
    .expect("done jobs")
    .try_get::<i64, _>("cnt")
    .unwrap_or_default();
    assert_eq!(jobs_done, 6);

    let cver = sqlx::query(&format!(
        "SELECT derivation_state, has_overview, has_detail FROM {} \
         WHERE memory_id = ?",
        family.content_versions_table
    ))
    .bind(&first.memory_id)
    .fetch_one(store.pool())
    .await
    .expect("content version");
    assert_eq!(
        cver.try_get::<String, _>("derivation_state")
            .unwrap_or_default(),
        "complete"
    );
    assert_eq!(cver.try_get::<i8, _>("has_overview").unwrap_or_default(), 1);
    assert_eq!(cver.try_get::<i8, _>("has_detail").unwrap_or_default(), 1);
}

struct MockV2Enricher;

#[async_trait]
impl MemoryV2JobEnricher for MockV2Enricher {
    async fn derive_views(
        &self,
        _source_text: &str,
        _abstract_text: &str,
    ) -> Result<Option<V2DerivedViews>, MemoriaError> {
        Ok(Some(V2DerivedViews {
            overview_text: "LLM overview for V2 memory".to_string(),
            detail_text: "LLM detail for V2 memory".to_string(),
        }))
    }

    async fn refine_links(
        &self,
        _source_abstract: &str,
        candidates: &[V2LinkCandidate],
    ) -> Result<Option<Vec<V2LinkSuggestion>>, MemoriaError> {
        Ok(candidates.first().map(|candidate| {
            vec![V2LinkSuggestion {
                target_memory_id: candidate.target_memory_id.clone(),
                link_type: "supports".to_string(),
                strength: 0.93,
            }]
        }))
    }
}

#[tokio::test]
async fn test_v2_process_pending_jobs_uses_enricher_when_available() {
    let (store, v2, user_id) = setup().await;
    let family = ensure_user_tables_ready(&store, &v2, &user_id).await;

    let first = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Primary memory content with enough detail to summarize and process through the view derivation pipeline. The enricher will be called to provide LLM-generated overview and detail text for this memory. This content needs to be longer than the abstract threshold to trigger the derive_views job.".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-llm".to_string()),
                importance: Some(0.5),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["shared".to_string(), "alpha".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember first");
    let second = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Second related memory for enrichment processing through the V2 job pipeline. This content has been made longer to ensure the derive_views job is triggered and the enricher is invoked. Shared tags alpha and beta connect this memory to other closely related memories in the test corpus.".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-llm".to_string()),
                importance: Some(0.4),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["shared".to_string(), "alpha".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 0.8)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember second");
    let third = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Third related memory kept from fallback candidates in the enrichment pipeline. This memory is used to test that the tag-overlap link type is correctly assigned when processing extract_links jobs. The content must be sufficiently long to trigger the full view derivation processing job.".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-llm".to_string()),
                importance: Some(0.3),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["shared".to_string()],
                source: None,
                embedding: Some(dim_vec(0, 0.7)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember third");

    let mut done = 0i64;
    for _ in 0..30 {
        v2.process_user_pending_jobs_with_enricher_pass(&user_id, 10, Some(&MockV2Enricher))
            .await
            .expect("process pending jobs");
        done = sqlx::query(&format!(
            "SELECT COUNT(*) AS cnt FROM {} WHERE status = 'done' AND aggregate_id IN (?, ?, ?)",
            family.jobs_table
        ))
        .bind(&first.memory_id)
        .bind(&second.memory_id)
        .bind(&third.memory_id)
        .fetch_one(store.pool())
        .await
        .expect("done jobs")
        .try_get::<i64, _>("cnt")
        .unwrap_or_default();
        let expanded = v2
            .expand(&user_id, &first.memory_id, ExpandLevel::Links)
            .await
            .expect("expand");
        if done == 9
            && expanded
                .links
                .as_ref()
                .map(|links| links.iter().any(|link| link.memory_id == second.memory_id))
                .unwrap_or(false)
            && expanded
                .links
                .as_ref()
                .map(|links| links.iter().any(|link| link.memory_id == third.memory_id))
                .unwrap_or(false)
        {
            break;
        }
    }
    assert_eq!(done, 9);

    let expanded = v2
        .expand(&user_id, &first.memory_id, ExpandLevel::Links)
        .await
        .expect("expand");
    assert_eq!(
        expanded.overview_text.as_deref(),
        Some("LLM overview for V2 memory")
    );
    assert_eq!(
        expanded.detail_text.as_deref(),
        Some("LLM detail for V2 memory")
    );
    assert!(expanded
        .links
        .as_ref()
        .map(|links| links
            .iter()
            .any(|link| link.memory_id == second.memory_id && link.link_type == "supports"))
        .unwrap_or(false));
    let supports_inline = expanded
        .links
        .as_ref()
        .and_then(|links| links.iter().find(|link| link.memory_id == second.memory_id))
        .expect("supports inline link");
    assert_eq!(supports_inline.link_type, "supports");
    assert!(supports_inline.provenance.refined);
    assert_eq!(
        supports_inline.provenance.primary_evidence_type.as_deref(),
        Some("tag_overlap")
    );
    assert_eq!(
        supports_inline
            .provenance
            .extraction_trace
            .as_ref()
            .and_then(|trace| trace.derivation_state.as_deref()),
        Some("complete")
    );
    assert_eq!(
        supports_inline
            .provenance
            .extraction_trace
            .as_ref()
            .and_then(|trace| trace.latest_job_status.as_deref()),
        Some("done")
    );
    assert!(supports_inline.provenance.evidence.iter().any(|detail| {
        detail.evidence_type == "tag_overlap"
            && detail.overlap_count == Some(2)
            && detail.source_tag_count == Some(2)
            && detail.target_tag_count == Some(2)
    }));
    assert!(supports_inline.provenance.evidence.iter().any(|detail| {
        detail.evidence_type == "semantic_related"
            && detail
                .vector_distance
                .map(|dist| (dist - 0.2).abs() < 0.0001)
                .unwrap_or(false)
    }));
    assert!(expanded
        .links
        .as_ref()
        .map(|links| links
            .iter()
            .any(|link| link.memory_id == third.memory_id && link.link_type != "supports"))
        .unwrap_or(false));
    let fallback_inline = expanded
        .links
        .as_ref()
        .and_then(|links| links.iter().find(|link| link.memory_id == third.memory_id))
        .expect("fallback inline link");
    assert!(!fallback_inline.provenance.refined);

    let recalled = v2
        .recall(
            &user_id,
            RecallV2Request {
                query: "Primary memory".to_string(),
                top_k: 3,
                max_tokens: 500,
                session_only: true,
                session_id: Some("sess-llm".to_string()),
                memory_type: Some(MemoryType::Semantic),
                tags: vec![],
                tag_filter_mode: "any".to_string(),
                created_after: None,
                created_before: None,
                with_overview: false,
                with_links: true,
                expand_links: false,
                query_embedding: Some(dim_vec(0, 1.0)),
            },
        )
        .await
        .expect("recall with refined inline links");
    let recalled_first = recalled
        .memories
        .iter()
        .find(|item| item.memory_id == first.memory_id)
        .expect("recalled first");
    let recalled_links = recalled_first
        .links
        .as_ref()
        .expect("recalled inline links");
    let recalled_supports = recalled_links
        .iter()
        .find(|link| link.memory_id == second.memory_id)
        .expect("recalled supports link");
    assert_eq!(recalled_supports.link_type, "supports");
    assert!(recalled_supports.provenance.refined);
    assert_eq!(
        recalled_supports
            .provenance
            .primary_evidence_type
            .as_deref(),
        Some("tag_overlap")
    );
    assert!(recalled_supports
        .provenance
        .evidence
        .iter()
        .any(
            |detail| detail.evidence_type == "semantic_related" && detail.vector_distance.is_some()
        ));

    let outbound = v2
        .links(
            &user_id,
            MemoryV2LinksRequest {
                memory_id: first.memory_id.clone(),
                direction: LinkDirection::Outbound,
                limit: 10,
                link_type: None,
                min_strength: 0.0,
            },
        )
        .await
        .expect("outbound with provenance");
    let supports = outbound
        .items
        .iter()
        .find(|item| item.memory_id == second.memory_id)
        .expect("supports item");
    assert_eq!(supports.link_type, "supports");
    assert!(supports.provenance.refined);
    assert_eq!(
        supports.provenance.primary_evidence_type.as_deref(),
        Some("tag_overlap")
    );
    assert!(supports
        .provenance
        .evidence_types
        .iter()
        .any(|kind| kind == "tag_overlap"));
    let fallback = outbound
        .items
        .iter()
        .find(|item| item.memory_id == third.memory_id)
        .expect("fallback item");
    assert!(!fallback.provenance.refined);
}

trait RecallItemExt {
    fn id(&self) -> &str;
}

impl RecallItemExt for memoria_storage::MemoryV2RecallItem {
    fn id(&self) -> &str {
        &self.memory_id
    }
}
