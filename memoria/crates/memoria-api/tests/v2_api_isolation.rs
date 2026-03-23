#[path = "support/v2_api.rs"]
mod support;

use serde_json::{json, Value};
use support::{
    spawn_server, spawn_server_with_master_key, uid, V2_WAIT_ATTEMPTS, V2_WAIT_SLEEP_MS,
};

#[tokio::test]
async fn test_api_v1_and_v2_coexist_without_cross_visibility() {
    let (base, client) = spawn_server().await;
    let user_id = uid();

    let v1_remember = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Legacy V1 ledger note for zebra pipelines",
            "memory_type": "semantic"
        }))
        .send()
        .await
        .expect("remember v1");
    assert_eq!(v1_remember.status(), 201);
    let v1_body: Value = v1_remember.json().await.expect("v1 remember json");
    let v1_memory_id = v1_body["memory_id"]
        .as_str()
        .expect("v1 memory_id")
        .to_string();
    let v1_profile = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Legacy V1 profile preference for shell aliases",
            "memory_type": "profile"
        }))
        .send()
        .await
        .expect("remember v1 profile");
    assert_eq!(v1_profile.status(), 201);
    let v1_profile_body: Value = v1_profile.json().await.expect("v1 profile json");
    let v1_profile_id = v1_profile_body["memory_id"]
        .as_str()
        .expect("v1 profile memory_id")
        .to_string();

    let v2_remember = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Fresh V2 orchestration brief for rust runtimes",
            "type": "semantic",
            "session_id": "sess-coexist",
            "tags": ["rust", "runtime"]
        }))
        .send()
        .await
        .expect("remember v2");
    assert_eq!(v2_remember.status(), 201);
    let v2_body: Value = v2_remember.json().await.expect("v2 remember json");
    let v2_memory_id = v2_body["memory_id"]
        .as_str()
        .expect("v2 memory_id")
        .to_string();
    let v2_profile = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Fresh V2 profile preference for concise reviews",
            "type": "profile",
            "session_id": "sess-coexist"
        }))
        .send()
        .await
        .expect("remember v2 profile");
    assert_eq!(v2_profile.status(), 201);
    let v2_profile_body: Value = v2_profile.json().await.expect("v2 profile json");
    let v2_profile_id = v2_profile_body["memory_id"]
        .as_str()
        .expect("v2 profile memory_id")
        .to_string();

    let v1_retrieve = client
        .post(format!("{base}/v1/memories/retrieve"))
        .header("X-User-Id", &user_id)
        .json(&json!({ "query": "zebra pipelines", "top_k": 10 }))
        .send()
        .await
        .expect("v1 retrieve");
    assert_eq!(v1_retrieve.status(), 200);
    let v1_results: Value = v1_retrieve.json().await.expect("v1 retrieve json");
    let v1_results = v1_results.as_array().expect("v1 retrieve items");
    assert!(v1_results
        .iter()
        .all(|item| item["memory_id"] != v2_memory_id));
    assert!(v1_results
        .iter()
        .all(|item| item["memory_id"] != v2_profile_id));

    let v1_profile_view = client
        .get(format!("{base}/v1/profiles/me"))
        .header("X-User-Id", &user_id)
        .send()
        .await
        .expect("v1 profile view");
    assert_eq!(v1_profile_view.status(), 200);
    let v1_profile_view_body: Value = v1_profile_view.json().await.expect("v1 profile view json");
    let v1_profile_text = v1_profile_view_body["profile"]
        .as_str()
        .expect("v1 profile text");
    assert!(v1_profile_text.contains("Legacy V1 profile preference for shell aliases"));
    assert!(!v1_profile_text.contains("Fresh V2 profile preference for concise reviews"));

    let v1_get_v2 = client
        .get(format!("{base}/v1/memories/{v2_memory_id}"))
        .header("X-User-Id", &user_id)
        .send()
        .await
        .expect("v1 get v2 memory");
    assert_eq!(v1_get_v2.status(), 200);
    let v1_get_v2_body: Value = v1_get_v2.json().await.expect("v1 get v2 json");
    assert!(v1_get_v2_body.is_null());

    let v2_list = client
        .get(format!("{base}/v2/memory/list"))
        .header("X-User-Id", &user_id)
        .query(&[("session_id", "sess-coexist"), ("limit", "10")])
        .send()
        .await
        .expect("v2 list");
    assert_eq!(v2_list.status(), 200);
    let v2_list_body: Value = v2_list.json().await.expect("v2 list json");
    let v2_items = v2_list_body["items"].as_array().expect("v2 list items");
    assert!(v2_items.iter().any(|item| item["id"] == v2_memory_id));
    assert!(v2_items.iter().any(|item| item["id"] == v2_profile_id));
    assert!(v2_items.iter().all(|item| item["id"] != v1_memory_id));
    assert!(v2_items.iter().all(|item| item["id"] != v1_profile_id));

    let v2_recall = client
        .post(format!("{base}/v2/memory/recall"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "query": "rust runtimes",
            "top_k": 10,
            "max_tokens": 200,
            "scope": "session",
            "session_id": "sess-coexist"
        }))
        .send()
        .await
        .expect("v2 recall");
    assert_eq!(v2_recall.status(), 200);
    let v2_recall_body: Value = v2_recall.json().await.expect("v2 recall json");
    let v2_memories = v2_recall_body["memories"]
        .as_array()
        .expect("v2 recall memories");
    assert!(v2_memories.iter().any(|item| item["id"] == v2_memory_id));
    assert!(v2_memories.iter().any(|item| item["id"] == v2_profile_id));
    assert!(v2_memories.iter().all(|item| item["id"] != v1_memory_id));
    assert!(v2_memories.iter().all(|item| item["id"] != v1_profile_id));

    let v2_expand_v1 = client
        .post(format!("{base}/v2/memory/expand"))
        .header("X-User-Id", &user_id)
        .json(&json!({ "memory_id": v1_memory_id, "level": "overview" }))
        .send()
        .await
        .expect("v2 expand v1 memory");
    assert_eq!(v2_expand_v1.status(), 404);
}

#[tokio::test]
async fn test_api_v1_and_v2_cross_version_writes_are_rejected() {
    let mk = "test-master-key-v1-v2-cross-write";
    let (base, client) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let user_id = uid();

    let v1_remember = client
        .post(format!("{base}/v1/memories"))
        .header("Authorization", &auth)
        .header("X-User-Id", &user_id)
        .json(
            &json!({ "content": "Legacy V1 note that must reject V2 writes", "type": "semantic" }),
        )
        .send()
        .await
        .expect("remember v1");
    assert_eq!(v1_remember.status(), 201);
    let v1_body: Value = v1_remember.json().await.expect("v1 remember json");
    let v1_memory_id = v1_body["memory_id"]
        .as_str()
        .expect("v1 memory id")
        .to_string();

    let v2_remember = client
        .post(format!("{base}/v2/memory/remember"))
        .header("Authorization", &auth)
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Fresh V2 memory that must reject V1 writes",
            "session_id": "sess-cross-write",
            "tags": ["shared"]
        }))
        .send()
        .await
        .expect("remember v2");
    assert_eq!(v2_remember.status(), 201);
    let v2_body: Value = v2_remember.json().await.expect("v2 remember json");
    let v2_memory_id = v2_body["memory_id"]
        .as_str()
        .expect("v2 memory id")
        .to_string();

    let v1_delete_v2 = client
        .delete(format!("{base}/v1/memories/{v2_memory_id}"))
        .header("Authorization", &auth)
        .header("X-User-Id", &user_id)
        .send()
        .await
        .expect("v1 delete v2");
    assert_eq!(v1_delete_v2.status(), 404);

    let v1_purge_v2 = client
        .post(format!("{base}/v1/memories/purge"))
        .header("Authorization", &auth)
        .header("X-User-Id", &user_id)
        .json(&json!({ "memory_ids": [v2_memory_id] }))
        .send()
        .await
        .expect("v1 purge v2");
    assert_eq!(v1_purge_v2.status(), 404);

    let v1_feedback_v2 = client
        .post(format!("{base}/v1/memories/{v2_memory_id}/feedback"))
        .header("Authorization", &auth)
        .header("X-User-Id", &user_id)
        .json(&json!({ "signal": "useful", "context": "cross-version should fail" }))
        .send()
        .await
        .expect("v1 feedback v2");
    assert_eq!(v1_feedback_v2.status(), 404);

    let v1_correct_v2 = client
        .put(format!("{base}/v1/memories/{v2_memory_id}/correct"))
        .header("Authorization", &auth)
        .header("X-User-Id", &user_id)
        .json(&json!({ "new_content": "cross-version should fail" }))
        .send()
        .await
        .expect("v1 correct v2");
    assert_eq!(v1_correct_v2.status(), 404);

    let v2_forget_v1 = client
        .post(format!("{base}/v2/memory/forget"))
        .header("Authorization", &auth)
        .header("X-User-Id", &user_id)
        .json(&json!({ "memory_id": v1_memory_id, "reason": "cross-version should fail" }))
        .send()
        .await
        .expect("v2 forget v1");
    assert_eq!(v2_forget_v1.status(), 404);

    let v2_feedback_v1 = client
        .post(format!("{base}/v2/memory/{v1_memory_id}/feedback"))
        .header("Authorization", &auth)
        .header("X-User-Id", &user_id)
        .json(&json!({ "signal": "wrong", "context": "cross-version should fail" }))
        .send()
        .await
        .expect("v2 feedback v1");
    assert_eq!(v2_feedback_v1.status(), 404);

    let v2_update_v1 = client
        .patch(format!("{base}/v2/memory/update"))
        .header("Authorization", &auth)
        .header("X-User-Id", &user_id)
        .json(&json!({ "memory_id": v1_memory_id, "content": "cross-version should fail" }))
        .send()
        .await
        .expect("v2 update v1");
    assert_eq!(v2_update_v1.status(), 404);

    let v2_batch_forget_mixed = client
        .post(format!("{base}/v2/memory/batch-forget"))
        .header("Authorization", &auth)
        .header("X-User-Id", &user_id)
        .json(&json!({
            "memory_ids": [v2_memory_id, v1_memory_id],
            "reason": "cross-version mixed batch should fail atomically"
        }))
        .send()
        .await
        .expect("v2 batch forget mixed");
    assert_eq!(v2_batch_forget_mixed.status(), 404);

    let v1_get = client
        .get(format!("{base}/v1/memories/{v1_memory_id}"))
        .header("Authorization", &auth)
        .header("X-User-Id", &user_id)
        .send()
        .await
        .expect("get v1 after cross-version writes");
    assert_eq!(v1_get.status(), 200);
    let v1_get_body: Value = v1_get.json().await.expect("v1 get body");
    assert_eq!(v1_get_body["memory_id"], v1_memory_id);

    let v2_list = client
        .get(format!("{base}/v2/memory/list"))
        .header("Authorization", &auth)
        .header("X-User-Id", &user_id)
        .query(&[("session_id", "sess-cross-write"), ("limit", "10")])
        .send()
        .await
        .expect("v2 list after cross-version writes");
    assert_eq!(v2_list.status(), 200);
    let v2_list_body: Value = v2_list.json().await.expect("v2 list body");
    let v2_items = v2_list_body["items"].as_array().expect("v2 list items");
    assert!(v2_items.iter().any(|item| item["id"] == v2_memory_id));
}

#[tokio::test]
async fn test_admin_surfaces_include_v2_users_and_counts() {
    let mk = "test-master-key-admin-v2-observability";
    let (base, client) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let hybrid_user = uid();
    let v2_only_user = uid();

    let v1_remember = client
        .post(format!("{base}/v1/memories"))
        .header("Authorization", &auth)
        .header("X-User-Id", &hybrid_user)
        .json(&json!({ "content": "Hybrid user legacy memory", "type": "semantic" }))
        .send()
        .await
        .expect("remember v1");
    assert_eq!(v1_remember.status(), 201);

    for idx in 0..2 {
        let response = client
            .post(format!("{base}/v2/memory/remember"))
            .header("Authorization", &auth)
            .header("X-User-Id", &hybrid_user)
            .json(&json!({
                "content": format!("Hybrid user v2 memory {idx}"),
                "session_id": "sess-admin-observability"
            }))
            .send()
            .await
            .expect("remember hybrid v2");
        assert_eq!(response.status(), 201);
    }

    for idx in 0..2 {
        let response = client
            .post(format!("{base}/v2/memory/remember"))
            .header("Authorization", &auth)
            .header("X-User-Id", &v2_only_user)
            .json(&json!({
                "content": format!("V2 only user memory {idx}"),
                "session_id": "sess-admin-v2-only"
            }))
            .send()
            .await
            .expect("remember v2 only");
        assert_eq!(response.status(), 201);
    }

    let system_stats = client
        .get(format!("{base}/admin/stats"))
        .header("Authorization", &auth)
        .send()
        .await
        .expect("admin stats");
    assert_eq!(system_stats.status(), 200);
    let system_stats_body: Value = system_stats.json().await.expect("admin stats body");
    assert_eq!(system_stats_body["total_users"], 2);
    assert_eq!(system_stats_body["total_memories"], 5);

    let users = client
        .get(format!("{base}/admin/users"))
        .header("Authorization", &auth)
        .send()
        .await
        .expect("admin users");
    assert_eq!(users.status(), 200);
    let users_body: Value = users.json().await.expect("admin users body");
    let user_items = users_body["users"].as_array().expect("admin user items");
    assert_eq!(user_items.len(), 2);
    assert!(user_items.iter().any(|item| item["user_id"] == hybrid_user));
    assert!(user_items
        .iter()
        .any(|item| item["user_id"] == v2_only_user));

    let hybrid_stats = client
        .get(format!("{base}/admin/users/{hybrid_user}/stats"))
        .header("Authorization", &auth)
        .send()
        .await
        .expect("hybrid user stats");
    assert_eq!(hybrid_stats.status(), 200);
    let hybrid_stats_body: Value = hybrid_stats.json().await.expect("hybrid user stats body");
    assert_eq!(hybrid_stats_body["memory_count"], 3);

    let v2_only_stats = client
        .get(format!("{base}/admin/users/{v2_only_user}/stats"))
        .header("Authorization", &auth)
        .send()
        .await
        .expect("v2 only user stats");
    assert_eq!(v2_only_stats.status(), 200);
    let v2_only_stats_body: Value = v2_only_stats.json().await.expect("v2 only user stats body");
    assert_eq!(v2_only_stats_body["memory_count"], 2);
}

#[tokio::test]
async fn test_metrics_include_v2_users_and_memories() {
    let (base, client) = spawn_server().await;
    let hybrid_user = uid();
    let v2_only_user = uid();

    let v1_remember = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &hybrid_user)
        .json(&json!({
            "content": "Hybrid user legacy semantic memory",
            "memory_type": "semantic"
        }))
        .send()
        .await
        .expect("remember v1");
    assert_eq!(v1_remember.status(), 201);

    let hybrid_v2_remember = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &hybrid_user)
        .json(&json!({
            "content": "Hybrid user V2 profile memory",
            "type": "profile",
            "session_id": "sess-metrics-hybrid"
        }))
        .send()
        .await
        .expect("remember hybrid v2");
    assert_eq!(hybrid_v2_remember.status(), 201);

    let v2_only_remember = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &v2_only_user)
        .json(&json!({
            "content": "V2-only semantic memory",
            "session_id": "sess-metrics-v2-only"
        }))
        .send()
        .await
        .expect("remember v2 only");
    assert_eq!(v2_only_remember.status(), 201);

    let metrics = client
        .get(format!("{base}/metrics"))
        .send()
        .await
        .expect("metrics");
    assert_eq!(metrics.status(), 200);
    let body = metrics.text().await.expect("metrics body");
    assert!(
        body.contains("memoria_memories_total{type=\"semantic\"} 2"),
        "{body}"
    );
    assert!(
        body.contains("memoria_memories_total{type=\"profile\"} 1"),
        "{body}"
    );
    assert!(
        body.contains("memoria_memories_total{type=\"all\"} 3"),
        "{body}"
    );
    assert!(
        body.contains("memoria_memories_by_version_total{version=\"v1\"} 1"),
        "{body}"
    );
    assert!(
        body.contains("memoria_memories_by_version_total{version=\"v2\"} 2"),
        "{body}"
    );
    assert!(body.contains("memoria_users_total 2"), "{body}");
    assert!(
        body.contains("memoria_users_by_version_total{version=\"v1\"} 1"),
        "{body}"
    );
    assert!(
        body.contains("memoria_users_by_version_total{version=\"v2\"} 2"),
        "{body}"
    );
}

#[tokio::test]
async fn test_admin_governance_extract_entities_stays_v1_scoped() {
    let mk = "test-master-key-admin-governance-v1-scope";
    let (base, client) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let user_id = uid();

    let v1_memory = client
        .post(format!("{base}/v1/memories"))
        .header("Authorization", &auth)
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Legacy zebra-gateway operations note",
            "memory_type": "semantic"
        }))
        .send()
        .await
        .expect("remember v1 memory");
    assert_eq!(v1_memory.status(), 201);

    let remembered = client
        .post(format!("{base}/v2/memory/remember"))
        .header("Authorization", &auth)
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Rust and Docker keep the auth-service deployable across shared infrastructure environments. This content is intentionally long enough to trigger derive_views so the admin governance isolation test can wait on a fully processed V2 memory before asserting that V1 admin extraction does not leak into V2 entities. Platform operators rely on this auth-service deployment note during incident response.",
            "type": "semantic",
            "session_id": "sess-admin-governance"
        }))
        .send()
        .await
        .expect("remember v2 entity memory");
    assert_eq!(remembered.status(), 201);
    let remembered_body: Value = remembered.json().await.expect("remembered body");
    let memory_id = remembered_body["memory_id"]
        .as_str()
        .expect("v2 memory id")
        .to_string();

    for _ in 0..V2_WAIT_ATTEMPTS {
        let jobs = client
            .get(format!("{base}/v2/memory/jobs"))
            .header("Authorization", &auth)
            .header("X-User-Id", &user_id)
            .query(&[("memory_id", memory_id.as_str()), ("limit", "10")])
            .send()
            .await
            .expect("jobs with auth");
        assert_eq!(jobs.status(), 200);
        let jobs_body: Value = jobs.json().await.expect("jobs with auth json");
        if jobs_body["pending_count"].as_u64().unwrap_or(1) == 0
            && jobs_body["in_progress_count"].as_u64().unwrap_or(1) == 0
            && jobs_body["failed_count"].as_u64().unwrap_or_default() == 0
            && jobs_body["derivation_state"] == "complete"
        {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(V2_WAIT_SLEEP_MS)).await;
    }

    let before = client
        .get(format!("{base}/v2/memory/entities"))
        .header("Authorization", &auth)
        .header("X-User-Id", &user_id)
        .query(&[("limit", "20")])
        .send()
        .await
        .expect("list v2 entities before admin governance");
    assert_eq!(before.status(), 200);
    let before_body: Value = before.json().await.expect("before body");
    let before_items = before_body["items"]
        .as_array()
        .expect("before items")
        .clone();

    let triggered = client
        .post(format!(
            "{base}/admin/governance/{user_id}/trigger?op=extract_entities"
        ))
        .header("Authorization", &auth)
        .send()
        .await
        .expect("trigger admin extract entities");
    assert_eq!(triggered.status(), 200);
    let triggered_body: Value = triggered.json().await.expect("triggered body");
    assert_eq!(triggered_body["op"], "extract_entities");
    assert_eq!(triggered_body["scope"], "v1");

    let after_admin = client
        .get(format!("{base}/v2/memory/entities"))
        .header("Authorization", &auth)
        .header("X-User-Id", &user_id)
        .query(&[("limit", "20")])
        .send()
        .await
        .expect("list v2 entities after admin governance");
    assert_eq!(after_admin.status(), 200);
    let after_admin_body: Value = after_admin.json().await.expect("after admin body");
    let after_admin_items = after_admin_body["items"]
        .as_array()
        .expect("after admin items");
    assert_eq!(after_admin_items, &before_items);

    let extracted = client
        .post(format!("{base}/v2/memory/entities/extract"))
        .header("Authorization", &auth)
        .header("X-User-Id", &user_id)
        .json(&json!({ "memory_id": memory_id, "limit": 10 }))
        .send()
        .await
        .expect("extract v2 entities");
    let extracted_status = extracted.status();
    let extracted_text = extracted.text().await.expect("extract body text");
    assert_eq!(extracted_status, 200, "extract body: {extracted_text}");
    let extracted_body: Value = serde_json::from_str(&extracted_text).expect("extract body");
    assert!(
        extracted_body["entities_found"]
            .as_i64()
            .unwrap_or_default()
            >= 3
    );

    let after_v2_extract = client
        .get(format!("{base}/v2/memory/entities"))
        .header("Authorization", &auth)
        .header("X-User-Id", &user_id)
        .query(&[("limit", "20")])
        .send()
        .await
        .expect("list v2 entities after v2 extract");
    assert_eq!(after_v2_extract.status(), 200);
    let after_v2_extract_body: Value = after_v2_extract
        .json()
        .await
        .expect("after v2 extract body");
    assert!(
        after_v2_extract_body["items"]
            .as_array()
            .expect("after v2 extract items")
            .len()
            >= before_items.len()
    );
    let names = after_v2_extract_body["items"]
        .as_array()
        .expect("after v2 extract items")
        .iter()
        .filter_map(|item| item["name"].as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&"rust"));
    assert!(names.contains(&"docker"));
    assert!(names.contains(&"auth-service"));
    assert!(!names.contains(&"zebra-gateway"));
}
