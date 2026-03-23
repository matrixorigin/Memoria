use std::sync::Arc;

use memoria_core::MemoryType;
use memoria_service::MemoryService;
use memoria_storage::{
    ExpandLevel, MemoryV2JobsRequest, MemoryV2RememberInput, ReflectV2Filter, SqlMemoryStore,
};
use serde_json::Value;
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

fn db_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria_test".to_string())
}

fn isolated_db_url() -> String {
    let base = db_url();
    let Some((prefix, db_name)) = base.rsplit_once('/') else {
        return base;
    };
    format!("{prefix}/{}_{}", db_name, Uuid::new_v4().simple())
}

fn uid() -> String {
    format!("service_v2_runtime_{}", Uuid::new_v4().simple())
}

async fn wait_for_v2_jobs_done(
    v2: &memoria_storage::MemoryV2Store,
    user_id: &str,
    memory_ids: &[&str],
) {
    let mut ready = false;
    for _ in 0..120 {
        let mut all_ready = true;
        for memory_id in memory_ids {
            let jobs = v2
                .jobs(
                    user_id,
                    MemoryV2JobsRequest {
                        memory_id: (*memory_id).to_string(),
                        limit: 10,
                    },
                )
                .await
                .expect("jobs");
            if jobs.pending_count != 0
                || jobs.in_progress_count != 0
                || jobs.failed_count != 0
                || jobs.derivation_state != "complete"
            {
                all_ready = false;
                break;
            }
        }
        if all_ready {
            ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    assert!(
        ready,
        "timed out waiting for spawned V2 worker to finish jobs"
    );
}

#[tokio::test]
async fn service_runtime_keeps_v2_worker_isolated_from_v1() {
    let db = isolated_db_url();
    let store = Arc::new(
        SqlMemoryStore::connect(&db, test_dim(), Uuid::new_v4().to_string())
            .await
            .expect("connect"),
    );
    store.migrate().await.expect("migrate");

    let v2 = store.v2_store();
    let user_id = uid();
    let family = v2
        .ensure_user_tables(&user_id)
        .await
        .expect("ensure v2 tables");
    let service = MemoryService::new_sql_with_llm(store.clone(), None, None).await;

    let v1_memory = service
        .store_memory(
            &user_id,
            "Legacy V1 runtime note that should remain outside V2 jobs",
            MemoryType::Semantic,
            Some("sess-v1-runtime".to_string()),
            None,
            None,
            None,
        )
        .await
        .expect("store v1 memory");

    let v1_before = sqlx::query(
        "SELECT content, is_active, superseded_by, updated_at FROM mem_memories WHERE memory_id = ?",
    )
    .bind(&v1_memory.memory_id)
    .fetch_one(store.pool())
    .await
    .expect("fetch v1 before");
    let v1_updated_at_before = v1_before
        .try_get::<chrono::NaiveDateTime, _>("updated_at")
        .expect("v1 updated_at before");

    let first = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "First V2 runtime memory with shared tags for isolation testing. This content needs to be long enough to trigger the derive_views background job so that the test can verify overview and detail text are populated. Platform teams rely on Rust for safe and reliable service infrastructure.".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-v2-runtime".to_string()),
                importance: Some(0.4),
                trust_tier: None,
                tags: vec!["shared".to_string(), "rust".to_string()],
                source: None,
                embedding: Some(dim_vec(1, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember first v2");
    let second = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Second V2 runtime memory that should link only within V2 table boundaries and not touch V1. This content is also made longer to trigger view derivation so the test can verify has_overview and has_detail after job processing. Shared platform tags ensure these memories are linked via tag_overlap.".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-v2-runtime".to_string()),
                importance: Some(0.4),
                trust_tier: None,
                tags: vec!["shared".to_string(), "platform".to_string()],
                source: None,
                embedding: Some(dim_vec(1, 0.92)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember second v2");

    let queued_for_v1 = sqlx::query(&format!(
        "SELECT COUNT(*) AS cnt FROM {} WHERE aggregate_id = ?",
        family.jobs_table
    ))
    .bind(&v1_memory.memory_id)
    .fetch_one(store.pool())
    .await
    .expect("queued jobs for v1")
    .try_get::<i64, _>("cnt")
    .unwrap_or_default();
    assert_eq!(queued_for_v1, 0);

    wait_for_v2_jobs_done(&v2, &user_id, &[&first.memory_id, &second.memory_id]).await;

    let mut expanded = None;
    for _ in 0..120 {
        let candidate = v2
            .expand(&user_id, &first.memory_id, ExpandLevel::Links)
            .await
            .expect("expand final");
        if candidate.overview_text.is_some()
            && candidate.detail_text.is_some()
            && candidate
                .links
                .as_ref()
                .map(|links| links.iter().any(|link| link.memory_id == second.memory_id))
                .unwrap_or(false)
        {
            expanded = Some(candidate);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    let expanded = expanded.expect("timed out waiting for runtime V2 expanded links");
    assert!(expanded.overview_text.is_some());
    assert!(expanded.detail_text.is_some());
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

    let v2_links_to_v1 = sqlx::query(&format!(
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
    assert_eq!(v2_links_to_v1, 0);

    let v1_get_v2 = service.get(&first.memory_id).await.expect("service get v2");
    assert!(v1_get_v2.is_none());

    let v1_list = service
        .list_active(&user_id, 10)
        .await
        .expect("service list");
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
async fn service_runtime_keeps_v2_reflect_internal_isolated_from_v1() {
    let db = isolated_db_url();
    let store = Arc::new(
        SqlMemoryStore::connect(&db, test_dim(), Uuid::new_v4().to_string())
            .await
            .expect("connect"),
    );
    store.migrate().await.expect("migrate");

    let v2 = store.v2_store();
    let user_id = uid();
    let family = v2
        .ensure_user_tables(&user_id)
        .await
        .expect("ensure v2 tables");
    let service = MemoryService::new_sql_with_llm(store.clone(), None, None).await;

    let v1_memory = service
        .store_memory(
            &user_id,
            "Legacy V1 reflect note that must stay outside V2 internal reflect",
            MemoryType::Semantic,
            Some("sess-v1-reflect-runtime".to_string()),
            None,
            None,
            None,
        )
        .await
        .expect("store v1 memory");

    let v1_before = sqlx::query(
        "SELECT content, is_active, superseded_by, updated_at FROM mem_memories WHERE memory_id = ?",
    )
    .bind(&v1_memory.memory_id)
    .fetch_one(store.pool())
    .await
    .expect("fetch v1 before");
    let v1_count_before = sqlx::query("SELECT COUNT(*) AS cnt FROM mem_memories WHERE user_id = ?")
        .bind(&user_id)
        .fetch_one(store.pool())
        .await
        .expect("count v1 before")
        .try_get::<i64, _>("cnt")
        .unwrap_or_default();
    let v1_updated_at_before = v1_before
        .try_get::<chrono::NaiveDateTime, _>("updated_at")
        .expect("v1 updated_at before");

    let first = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Alpha platform memory connected through shared operations and team workflows. This content is made long enough to trigger the derive_views background job for the isolation test. Platform engineers rely on shared alpha tags to link related memories across different service contexts.".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-reflect-a".to_string()),
                importance: Some(0.6),
                trust_tier: None,
                tags: vec!["shared".to_string(), "alpha".to_string()],
                source: None,
                embedding: Some(dim_vec(1, 1.0)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember first v2");
    let second = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Bridge operations memory joining alpha and beta service clusters across the platform. This content exceeds the abstract threshold so that derive_views is triggered and overview and detail text are populated. Shared tags connect this memory to related alpha and beta memories in the test corpus.".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-reflect-b".to_string()),
                importance: Some(0.8),
                trust_tier: None,
                tags: vec!["shared".to_string(), "beta".to_string()],
                source: None,
                embedding: Some(dim_vec(1, 0.92)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember second v2");
    let third = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Beta deployment memory linked through the bridge operations cluster in the platform. This memory content is long enough to trigger view derivation jobs in the background. Beta and gamma tags connect this memory to other test memories in the reflect isolation test for the V2 runtime service worker.".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-reflect-b".to_string()),
                importance: Some(0.5),
                trust_tier: None,
                tags: vec!["beta".to_string(), "gamma".to_string()],
                source: None,
                embedding: Some(dim_vec(1, 0.84)),
                actor: "tester".to_string(),
            },
        )
        .await
        .expect("remember third v2");

    wait_for_v2_jobs_done(
        &v2,
        &user_id,
        &[&first.memory_id, &second.memory_id, &third.memory_id],
    )
    .await;

    let mut reflected = None;
    for _ in 0..60 {
        let result = v2
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
        if result.scenes_created == 1 {
            reflected = Some(result);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    let reflected = reflected.expect("timed out waiting for runtime V2 reflect internal");
    assert_eq!(reflected.mode, "internal");
    assert!(reflected.synthesized);
    assert_eq!(reflected.scenes_created, 1);
    let candidate = reflected
        .candidates
        .iter()
        .find(|candidate| candidate.signal == "cross_session_linked_cluster")
        .expect("cross session reflect candidate");
    assert_eq!(candidate.memory_count, 3);
    assert!(candidate
        .memories
        .iter()
        .all(|memory| memory.memory_id != v1_memory.memory_id));

    let synth_row = sqlx::query(&format!(
        "SELECT memory_id, source_kind, source_json FROM {} WHERE forgotten_at IS NULL AND source_kind = ?",
        family.heads_table
    ))
    .bind("reflect_v2")
    .fetch_one(store.pool())
    .await
    .expect("synthesized v2 reflect row");
    let synth_memory_id: String = synth_row.try_get("memory_id").expect("synth memory id");
    let source_kind: Option<String> = synth_row.try_get("source_kind").ok();
    let source_json: Value = synth_row.try_get("source_json").expect("source_json");
    assert_eq!(source_kind.as_deref(), Some("reflect_v2"));
    let source_memory_ids = source_json["source_memory_ids"]
        .as_array()
        .expect("source memory ids");
    assert_eq!(source_memory_ids.len(), 3);
    assert!(source_memory_ids
        .iter()
        .all(|id| id != &Value::String(v1_memory.memory_id.clone())));

    let v2_links_touching_v1 = sqlx::query(&format!(
        "SELECT COUNT(*) AS cnt FROM {} WHERE memory_id = ? OR target_memory_id = ?",
        family.links_table
    ))
    .bind(&v1_memory.memory_id)
    .bind(&v1_memory.memory_id)
    .fetch_one(store.pool())
    .await
    .expect("v2 links touching v1")
    .try_get::<i64, _>("cnt")
    .unwrap_or_default();
    assert_eq!(v2_links_touching_v1, 0);

    let queued_for_v1 = sqlx::query(&format!(
        "SELECT COUNT(*) AS cnt FROM {} WHERE aggregate_id = ?",
        family.jobs_table
    ))
    .bind(&v1_memory.memory_id)
    .fetch_one(store.pool())
    .await
    .expect("queued jobs for v1 after reflect")
    .try_get::<i64, _>("cnt")
    .unwrap_or_default();
    assert_eq!(queued_for_v1, 0);

    let v1_get_v2_synth = service
        .get(&synth_memory_id)
        .await
        .expect("service get v2 synth");
    assert!(v1_get_v2_synth.is_none());

    let v1_list = service
        .list_active(&user_id, 10)
        .await
        .expect("service list");
    assert_eq!(v1_list.len(), 1);
    assert_eq!(v1_list[0].memory_id, v1_memory.memory_id);

    let v1_count_after = sqlx::query("SELECT COUNT(*) AS cnt FROM mem_memories WHERE user_id = ?")
        .bind(&user_id)
        .fetch_one(store.pool())
        .await
        .expect("count v1 after")
        .try_get::<i64, _>("cnt")
        .unwrap_or_default();
    assert_eq!(v1_count_after, v1_count_before);

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
