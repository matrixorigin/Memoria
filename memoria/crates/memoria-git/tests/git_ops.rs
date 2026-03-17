/// Git-for-Data integration tests against real MatrixOne.
/// Run: DATABASE_URL=mysql://root:111@localhost:6001/memoria \
///      SQLX_OFFLINE=true cargo test -p memoria-git --test git_ops -- --nocapture

use memoria_git::GitForDataService;
use sqlx::mysql::MySqlPool;
use uuid::Uuid;

use memoria_storage::SqlMemoryStore;

async fn setup() -> (GitForDataService, String) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria".to_string());
    let pool = MySqlPool::connect(&url).await.expect("connect");
    // Ensure full schema exists (branch/count tests need mem_memories)
    let dim: usize = std::env::var("EMBEDDING_DIM").ok()
        .and_then(|v| v.parse().ok()).unwrap_or(384);
    SqlMemoryStore::new(pool.clone(), dim).migrate().await.expect("migrate");
    let db_name = url.rsplit('/').next().unwrap_or("memoria_test");
    let svc = GitForDataService::new(pool, db_name);
    let suffix = Uuid::new_v4().simple().to_string()[..8].to_string();
    (svc, suffix)
}

#[tokio::test]
async fn test_snapshot_create_list_drop() {
    let (svc, suffix) = setup().await;
    let name = format!("rs_test_{suffix}");

    // Create
    let snap = svc.create_snapshot(&name).await.expect("create_snapshot");
    assert_eq!(snap.snapshot_name, name);
    println!("✅ create_snapshot: {}", snap.snapshot_name);

    // List — should contain our snapshot
    let snaps = svc.list_snapshots().await.expect("list_snapshots");
    assert!(snaps.iter().any(|s| s.snapshot_name == name));
    println!("✅ list_snapshots: {} total", snaps.len());

    // Drop
    svc.drop_snapshot(&name).await.expect("drop_snapshot");
    let snaps = svc.list_snapshots().await.unwrap();
    assert!(!snaps.iter().any(|s| s.snapshot_name == name));
    println!("✅ drop_snapshot: OK");
}

#[tokio::test]
async fn test_branch_create_drop() {
    let (svc, suffix) = setup().await;
    let branch = format!("rs_branch_{suffix}");

    svc.create_branch(&branch, "mem_memories").await.expect("create_branch");
    println!("✅ create_branch: {branch}");

    svc.drop_branch(&branch).await.expect("drop_branch");
    println!("✅ drop_branch: OK");
}

#[tokio::test]
async fn test_count_at_snapshot() {
    let (svc, suffix) = setup().await;
    let snap_name = format!("rs_count_{suffix}");

    svc.create_snapshot(&snap_name).await.expect("create_snapshot");

    // Count at snapshot (user_id that doesn't exist → 0)
    let count = svc.count_at_snapshot("mem_memories", &snap_name, "nonexistent_user")
        .await.expect("count_at_snapshot");
    assert_eq!(count, 0);
    println!("✅ count_at_snapshot: {count}");

    svc.drop_snapshot(&snap_name).await.unwrap();
}

#[tokio::test]
async fn test_invalid_identifier_rejected() {
    let (svc, _) = setup().await;
    // SQL injection attempt should be rejected
    let result = svc.create_snapshot("valid'; DROP TABLE mem_memories; --").await;
    assert!(result.is_err());
    println!("✅ invalid identifier rejected: {:?}", result.unwrap_err());
}
