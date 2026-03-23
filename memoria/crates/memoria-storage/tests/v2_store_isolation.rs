use chrono::Utc;
use memoria_core::{interfaces::MemoryStore, MemoriaError, Memory, MemoryType, TrustTier};
use memoria_storage::{
    ExpandLevel, ListV2Filter, MemoryV2RememberInput, ProfileV2Filter, RecallV2Request,
    SqlMemoryStore,
};
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

#[tokio::test]
async fn test_v1_and_v2_storage_surfaces_remain_isolated() {
    let (store, v2, user_id) = setup().await;

    let v1_memory = Memory {
        memory_id: Uuid::new_v4().simple().to_string(),
        user_id: user_id.clone(),
        memory_type: MemoryType::Semantic,
        content: "Legacy V1 ledger note for zebra pipelines".to_string(),
        initial_confidence: TrustTier::T1Verified.initial_confidence(),
        embedding: Some(dim_vec(0, 1.0)),
        source_event_ids: vec![],
        superseded_by: None,
        is_active: true,
        access_count: 0,
        session_id: Some("sess-v1-isolation".to_string()),
        observed_at: Some(Utc::now()),
        created_at: None,
        updated_at: None,
        extra_metadata: None,
        trust_tier: TrustTier::T1Verified,
        retrieval_score: None,
    };
    store.insert(&v1_memory).await.expect("insert v1 memory");
    let v1_profile = Memory {
        memory_id: Uuid::new_v4().simple().to_string(),
        user_id: user_id.clone(),
        memory_type: MemoryType::Profile,
        content: "Legacy V1 profile note that should stay outside V2".to_string(),
        initial_confidence: TrustTier::T1Verified.initial_confidence(),
        embedding: Some(dim_vec(1, 1.0)),
        source_event_ids: vec![],
        superseded_by: None,
        is_active: true,
        access_count: 0,
        session_id: Some("sess-v1-profile".to_string()),
        observed_at: Some(Utc::now()),
        created_at: None,
        updated_at: None,
        extra_metadata: None,
        trust_tier: TrustTier::T1Verified,
        retrieval_score: None,
    };
    store.insert(&v1_profile).await.expect("insert v1 profile");

    let v2_memory = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Fresh V2 orchestration brief for rust runtimes".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-v2-isolation".to_string()),
                importance: Some(0.6),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["rust".to_string(), "runtime".to_string()],
                source: None,
                embedding: Some(dim_vec(1, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember v2 memory");
    let v2_profile = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Fresh V2 profile note for patch review habits".to_string(),
                memory_type: MemoryType::Profile,
                session_id: Some("sess-v2-isolation".to_string()),
                importance: Some(0.8),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["profile".to_string()],
                source: None,
                embedding: Some(dim_vec(2, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember v2 profile");

    let v1_list = store.list_active(&user_id, 10).await.expect("v1 list");
    assert!(v1_list
        .iter()
        .any(|memory| memory.memory_id == v1_memory.memory_id));
    assert!(v1_list
        .iter()
        .all(|memory| memory.memory_id != v2_memory.memory_id));

    let v1_get_v2 = store.get(&v2_memory.memory_id).await.expect("v1 get v2");
    assert!(v1_get_v2.is_none());

    let v1_search = store
        .search_fulltext(&user_id, "zebra pipelines", 10)
        .await
        .expect("v1 search");
    assert!(v1_search
        .iter()
        .any(|memory| memory.memory_id == v1_memory.memory_id));
    assert!(v1_search
        .iter()
        .all(|memory| memory.memory_id != v2_memory.memory_id));

    let v1_search_for_v2_phrase = store
        .search_fulltext(&user_id, "rust runtimes", 10)
        .await
        .expect("v1 search for v2 phrase");
    assert!(v1_search_for_v2_phrase
        .iter()
        .all(|memory| memory.memory_id != v2_memory.memory_id));
    assert!(v1_search_for_v2_phrase
        .iter()
        .all(|memory| memory.memory_id != v2_profile.memory_id));

    let v2_list = v2
        .list(
            &user_id,
            ListV2Filter {
                limit: 10,
                cursor: None,
                memory_type: Some(MemoryType::Semantic),
                session_id: Some("sess-v2-isolation".to_string()),
            },
        )
        .await
        .expect("v2 list");
    assert!(v2_list
        .items
        .iter()
        .any(|memory| memory.memory_id == v2_memory.memory_id));
    assert!(v2_list
        .items
        .iter()
        .all(|memory| memory.memory_id != v1_memory.memory_id));
    assert!(v2_list
        .items
        .iter()
        .all(|memory| memory.memory_id != v1_profile.memory_id));

    let v2_recall = v2
        .recall(
            &user_id,
            RecallV2Request {
                query: "rust runtimes".to_string(),
                top_k: 10,
                max_tokens: 200,
                session_only: true,
                session_id: Some("sess-v2-isolation".to_string()),
                memory_type: None,
                tags: vec![],
                tag_filter_mode: "any".to_string(),
                created_after: None,
                created_before: None,
                with_overview: false,
                with_links: false,
                expand_links: false,
                query_embedding: Some(dim_vec(1, 1.0)),
            },
        )
        .await
        .expect("v2 recall");
    assert!(v2_recall
        .memories
        .iter()
        .any(|memory| memory.memory_id == v2_memory.memory_id));
    assert!(v2_recall
        .memories
        .iter()
        .all(|memory| memory.memory_id != v1_memory.memory_id));
    assert!(v2_recall
        .memories
        .iter()
        .all(|memory| memory.memory_id != v1_profile.memory_id));

    let v2_profile_list = v2
        .profile(
            &user_id,
            ProfileV2Filter {
                limit: 10,
                cursor: None,
                session_id: Some("sess-v2-isolation".to_string()),
            },
        )
        .await
        .expect("v2 profile list");
    assert!(v2_profile_list
        .items
        .iter()
        .any(|memory| memory.memory_id == v2_profile.memory_id));
    assert!(v2_profile_list
        .items
        .iter()
        .all(|memory| memory.memory_id != v1_profile.memory_id));
    assert!(v2_profile_list
        .items
        .iter()
        .all(|memory| memory.memory_id != v2_memory.memory_id));

    let v2_recall_for_v1_phrase = v2
        .recall(
            &user_id,
            RecallV2Request {
                query: "zebra pipelines".to_string(),
                top_k: 10,
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
                query_embedding: None,
            },
        )
        .await
        .expect("v2 recall for v1 phrase");
    assert!(v2_recall_for_v1_phrase
        .memories
        .iter()
        .all(|memory| memory.memory_id != v1_memory.memory_id));

    let v2_expand_v1 = v2
        .expand(&user_id, &v1_memory.memory_id, ExpandLevel::Overview)
        .await
        .expect_err("v2 expand should not read v1 memory");
    assert!(matches!(v2_expand_v1, MemoriaError::NotFound(_)));
}
