use memoria_storage::SqlMemoryStore;

#[tokio::test]
async fn test_update_affected_rows() {
    let database_url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria_test".to_string());
    let instance_id = uuid::Uuid::new_v4().to_string();
    let store = SqlMemoryStore::connect(&database_url, 1536, instance_id.clone())
        .await
        .expect("Failed to connect");
    store.migrate().await.expect("Failed to migrate");

    let lock_key = format!("test_lock_{}", uuid::Uuid::new_v4());
    let expires_at = chrono::Utc::now().naive_utc() + chrono::Duration::seconds(10);

    // 1. 插入一条锁记录
    let insert_result = sqlx::query(
        "INSERT INTO mem_distributed_locks (lock_key, holder_id, acquired_at, expires_at) \
         VALUES (?, ?, NOW(), ?)",
    )
    .bind(&lock_key)
    .bind("holder1")
    .bind(expires_at)
    .execute(store.pool())
    .await
    .expect("Insert failed");
    
    println!("✅ INSERT affected_rows: {}", insert_result.rows_affected());

    // 2. 尝试 UPDATE 未过期的锁（应该失败，affected_rows = 0）
    let update_result = sqlx::query(
        "UPDATE mem_distributed_locks \
         SET holder_id = ?, acquired_at = NOW(), expires_at = ? \
         WHERE lock_key = ? AND expires_at < NOW()",
    )
    .bind("holder2")
    .bind(expires_at)
    .bind(&lock_key)
    .execute(store.pool())
    .await
    .expect("Update failed");
    
    println!("✅ UPDATE (not expired) affected_rows: {}", update_result.rows_affected());
    assert_eq!(update_result.rows_affected(), 0, "Should not update non-expired lock");

    // 3. 等待锁过期
    tokio::time::sleep(tokio::time::Duration::from_secs(11)).await;

    // 4. 尝试 UPDATE 已过期的锁（应该成功，affected_rows = 1）
    let update_result2 = sqlx::query(
        "UPDATE mem_distributed_locks \
         SET holder_id = ?, acquired_at = NOW(), expires_at = ? \
         WHERE lock_key = ? AND expires_at < NOW()",
    )
    .bind("holder2")
    .bind(expires_at)
    .bind(&lock_key)
    .execute(store.pool())
    .await
    .expect("Update failed");
    
    println!("✅ UPDATE (expired) affected_rows: {}", update_result2.rows_affected());
    assert_eq!(update_result2.rows_affected(), 1, "Should update expired lock");

    // 5. 验证 holder_id 已更新
    let (holder,): (String,) = sqlx::query_as(
        "SELECT holder_id FROM mem_distributed_locks WHERE lock_key = ?",
    )
    .bind(&lock_key)
    .fetch_one(store.pool())
    .await
    .expect("Select failed");
    
    assert_eq!(holder, "holder2", "Holder should be updated");
    println!("✅ All tests passed");
}
