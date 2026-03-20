use memoria_storage::SqlMemoryStore;
use std::sync::Arc;

#[tokio::test]
async fn test_concurrent_lock_acquisition() {
    let database_url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria_test".to_string());
    
    let instance_a = uuid::Uuid::new_v4().to_string();
    let instance_b = uuid::Uuid::new_v4().to_string();
    
    let store_a = Arc::new(
        SqlMemoryStore::connect(&database_url, 1536, instance_a.clone())
            .await
            .expect("Failed to connect A"),
    );
    let store_b = Arc::new(
        SqlMemoryStore::connect(&database_url, 1536, instance_b.clone())
            .await
            .expect("Failed to connect B"),
    );
    
    store_a.migrate().await.expect("Failed to migrate");

    let lock_key = format!("concurrent_test_{}", uuid::Uuid::new_v4());

    // 场景1：两个实例同时尝试获取新锁
    let store_a_clone = store_a.clone();
    let store_b_clone = store_b.clone();
    let key_a = lock_key.clone();
    let key_b = lock_key.clone();
    
    let handle_a = tokio::spawn(async move {
        store_a_clone.try_acquire_lock(&key_a, 10).await
    });
    
    let handle_b = tokio::spawn(async move {
        store_b_clone.try_acquire_lock(&key_b, 10).await
    });

    let result_a = handle_a.await.unwrap().unwrap();
    let result_b = handle_b.await.unwrap().unwrap();

    println!("Instance A acquired: {}", result_a);
    println!("Instance B acquired: {}", result_b);

    // 关键断言：只有一个实例应该获取成功
    assert!(
        result_a ^ result_b,
        "Exactly one instance should acquire the lock (A={}, B={})",
        result_a,
        result_b
    );

    // 场景2：等待锁过期，两个实例同时尝试获取过期锁
    tokio::time::sleep(tokio::time::Duration::from_secs(11)).await;

    let store_a_clone = store_a.clone();
    let store_b_clone = store_b.clone();
    let key_a = lock_key.clone();
    let key_b = lock_key.clone();
    
    let handle_a = tokio::spawn(async move {
        store_a_clone.try_acquire_lock(&key_a, 10).await
    });
    
    let handle_b = tokio::spawn(async move {
        store_b_clone.try_acquire_lock(&key_b, 10).await
    });

    let result_a2 = handle_a.await.unwrap().unwrap();
    let result_b2 = handle_b.await.unwrap().unwrap();

    println!("After expiry - Instance A acquired: {}", result_a2);
    println!("After expiry - Instance B acquired: {}", result_b2);

    // 关键断言：只有一个实例应该获取成功
    assert!(
        result_a2 ^ result_b2,
        "Exactly one instance should acquire the expired lock (A={}, B={})",
        result_a2,
        result_b2
    );

    println!("✅ Concurrent lock acquisition test passed");
}
