/// Service layer unit tests using in-memory mock store.
use async_trait::async_trait;
use memoria_core::{
    interfaces::{EmbeddingProvider, MemoryStore},
    MemoriaError, Memory, MemoryType, TrustTier,
};
use memoria_service::MemoryService;
use memoria_storage::OwnedEditLogEntry;
use std::sync::{Arc, Mutex};

// ── Mock store ────────────────────────────────────────────────────────────────

#[derive(Default)]
struct MockStore {
    memories: Mutex<Vec<Memory>>,
}

#[async_trait]
impl MemoryStore for MockStore {
    async fn insert(&self, memory: &Memory) -> Result<(), MemoriaError> {
        self.memories.lock().unwrap().push(memory.clone());
        Ok(())
    }
    async fn get(&self, memory_id: &str) -> Result<Option<Memory>, MemoriaError> {
        Ok(self
            .memories
            .lock()
            .unwrap()
            .iter()
            .find(|m| m.memory_id == memory_id && m.is_active)
            .cloned())
    }
    async fn update(&self, memory: &Memory) -> Result<(), MemoriaError> {
        let mut store = self.memories.lock().unwrap();
        if let Some(m) = store.iter_mut().find(|m| m.memory_id == memory.memory_id) {
            *m = memory.clone();
        }
        Ok(())
    }
    async fn soft_delete(&self, memory_id: &str) -> Result<(), MemoriaError> {
        let mut store = self.memories.lock().unwrap();
        if let Some(m) = store.iter_mut().find(|m| m.memory_id == memory_id) {
            m.is_active = false;
        }
        Ok(())
    }
    async fn list_active(&self, user_id: &str, limit: i64) -> Result<Vec<Memory>, MemoriaError> {
        Ok(self
            .memories
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.user_id == user_id && m.is_active)
            .take(limit as usize)
            .cloned()
            .collect())
    }
    async fn search_fulltext(
        &self,
        user_id: &str,
        query: &str,
        limit: i64,
    ) -> Result<Vec<Memory>, MemoriaError> {
        Ok(self
            .memories
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.user_id == user_id && m.is_active && m.content.contains(query))
            .take(limit as usize)
            .cloned()
            .collect())
    }
    async fn search_vector(
        &self,
        _user_id: &str,
        _embedding: &[f32],
        _limit: i64,
    ) -> Result<Vec<Memory>, MemoriaError> {
        Ok(vec![]) // mock: no vector search, falls back to fulltext
    }
}

// ── Mock embedder ─────────────────────────────────────────────────────────────

struct MockEmbedder;

#[async_trait]
impl EmbeddingProvider for MockEmbedder {
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, MemoriaError> {
        Ok(vec![0.1, 0.2, 0.3, 0.4])
    }
    fn dimension(&self) -> usize {
        4
    }
}

fn make_service() -> MemoryService {
    MemoryService::new(
        Arc::new(MockStore::default()),
        Some(Arc::new(MockEmbedder)),
        None,
    )
}

fn make_service_with_entries() -> (MemoryService, Arc<Mutex<Vec<OwnedEditLogEntry>>>) {
    MemoryService::new_with_test_entries(
        Arc::new(MockStore::default()),
        Some(Arc::new(MockEmbedder)),
    )
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_store_and_retrieve() {
    let svc = make_service();
    let m = svc
        .store_memory(
            "u1",
            "rust is fast",
            MemoryType::Semantic,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    assert!(!m.memory_id.is_empty());
    assert_eq!(m.content, "rust is fast");
    assert!(m.embedding.is_some());

    // retrieve falls back to fulltext (mock vector returns empty)
    let results = svc.retrieve("u1", "rust", 5).await.unwrap();
    assert!(!results.is_empty());
    assert_eq!(results[0].content, "rust is fast");
    println!("✅ store_and_retrieve");
}

#[tokio::test]
async fn test_correct() {
    let svc = make_service();
    let m = svc
        .store_memory(
            "u1",
            "old content",
            MemoryType::Semantic,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    let corrected = svc
        .correct("u1", &m.memory_id, "new content")
        .await
        .unwrap();
    assert_eq!(corrected.content, "new content");
    assert!(corrected.embedding.is_some());
    println!("✅ correct");
}

#[tokio::test]
async fn test_purge() {
    let svc = make_service();
    let m = svc
        .store_memory(
            "u1",
            "to delete",
            MemoryType::Working,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    svc.purge("u1", &m.memory_id).await.unwrap();
    let got = svc.get(&m.memory_id).await.unwrap();
    assert!(got.is_none());
    println!("✅ purge");
}

#[tokio::test]
async fn test_list_active_excludes_deleted() {
    let svc = make_service();
    svc.store_memory(
        "u1",
        "keep this",
        MemoryType::Semantic,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let del = svc
        .store_memory(
            "u1",
            "delete this",
            MemoryType::Working,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    svc.purge("u1", &del.memory_id).await.unwrap();

    let list = svc.list_active("u1", 10).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].content, "keep this");
    println!("✅ list_active_excludes_deleted");
}

#[tokio::test]
async fn test_purge_by_session_id_filters_memory_type() {
    let svc = make_service();
    for (content, memory_type, session_id) in [
        (
            "remove working a",
            MemoryType::Working,
            Some("sess-target".to_string()),
        ),
        (
            "remove working b",
            MemoryType::Working,
            Some("sess-target".to_string()),
        ),
        (
            "keep semantic",
            MemoryType::Semantic,
            Some("sess-target".to_string()),
        ),
        (
            "keep other session",
            MemoryType::Working,
            Some("sess-other".to_string()),
        ),
    ] {
        svc.store_memory("u1", content, memory_type, session_id, None, None, None)
            .await
            .unwrap();
    }

    let memory_types = [MemoryType::Working];
    let result = svc
        .purge_by_session_id("u1", "sess-target", Some(&memory_types))
        .await
        .unwrap();
    assert_eq!(result.purged, 2);

    let list = svc.list_active("u1", 10).await.unwrap();
    let contents: Vec<&str> = list.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(list.len(), 2);
    assert!(contents.contains(&"keep semantic"));
    assert!(contents.contains(&"keep other session"));
    println!("✅ purge_by_session_id filters working memories only");
}

#[tokio::test]
async fn test_purge_by_session_id_fallback_is_not_capped() {
    let svc = make_service();
    let target_count = 10_005usize;
    for index in 0..target_count {
        svc.store_memory(
            "u1",
            &format!("target working {index}"),
            MemoryType::Working,
            Some("sess-target".to_string()),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    }
    svc.store_memory(
        "u1",
        "keep semantic",
        MemoryType::Semantic,
        Some("sess-target".to_string()),
        None,
        None,
        None,
    )
    .await
    .unwrap();

    let memory_types = [MemoryType::Working];
    let result = svc
        .purge_by_session_id("u1", "sess-target", Some(&memory_types))
        .await
        .unwrap();
    assert_eq!(result.purged, target_count);

    let list = svc.list_active("u1", i64::MAX).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].content, "keep semantic");
    println!("✅ purge_by_session_id fallback scans full active set");
}

#[tokio::test]
async fn test_memory_types() {
    let svc = make_service();
    for mt in [
        MemoryType::Semantic,
        MemoryType::Profile,
        MemoryType::Procedural,
        MemoryType::Working,
        MemoryType::ToolResult,
        MemoryType::Episodic,
    ] {
        let m = svc
            .store_memory("u1", "content", mt.clone(), None, None, None, None)
            .await
            .unwrap();
        assert_eq!(m.memory_type, mt);
    }
    println!("✅ all 6 memory types");
}

#[tokio::test]
async fn test_trust_tiers() {
    let svc = make_service();
    for (tier, expected_conf) in [
        (TrustTier::T1Verified, 0.95f64),
        (TrustTier::T2Curated, 0.85),
        (TrustTier::T3Inferred, 0.65),
        (TrustTier::T4Unverified, 0.40),
    ] {
        let m = svc
            .store_memory(
                "u1",
                "content",
                MemoryType::Semantic,
                None,
                Some(tier),
                None,
                None,
            )
            .await
            .unwrap();
        assert!((m.initial_confidence - expected_conf).abs() < 1e-6);
    }
    println!("✅ all 4 trust tiers");
}

#[tokio::test]
async fn test_no_embedder_still_works() {
    let svc = MemoryService::new(Arc::new(MockStore::default()), None, None);
    let m = svc
        .store_memory(
            "u1",
            "no embedding",
            MemoryType::Semantic,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    assert!(m.embedding.is_none());
    println!("✅ no_embedder_still_works");
}

#[tokio::test]
async fn test_flush_edit_log_drains_in_memory_buffer() {
    let (svc, entries) = make_service_with_entries();
    svc.send_edit_log("u1", "inject", Some("m1"), Some("{}"), "store_memory", None);

    assert!(
        entries.lock().unwrap().is_empty(),
        "entries should remain buffered until an explicit flush in this test"
    );

    svc.flush_edit_log().await;

    let drained = entries.lock().unwrap();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].user_id, "u1");
    assert_eq!(drained[0].operation, "inject");
    assert_eq!(drained[0].reason, "store_memory");
    println!("✅ flush_edit_log_drains_in_memory_buffer");
}
