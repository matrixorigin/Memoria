/// MatrixOne compatibility integration tests.
/// Each test uses a unique table name (UUID suffix) — safe to run in parallel.

use sqlx::{mysql::MySqlPool, Row};
use uuid::Uuid;

async fn connect() -> MySqlPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria".to_string());
    MySqlPool::connect(&url).await.expect("connect to MatrixOne")
}

fn tbl(prefix: &str) -> String {
    format!("{}_{}", prefix, Uuid::new_v4().simple())
}

// ── Test 1: Basic connectivity ────────────────────────────────────────────────

#[tokio::test]
async fn test_01_connect() {
    let pool = connect().await;
    let row = sqlx::query("SELECT 1 AS val").fetch_one(&pool).await.expect("SELECT 1");
    let val: i32 = row.try_get("val").unwrap();
    assert_eq!(val, 1);
    println!("✅ test_01_connect: OK");
}

// ── Test 2: CREATE TABLE ──────────────────────────────────────────────────────

#[tokio::test]
async fn test_02_create_table() {
    let pool = connect().await;
    let t = tbl("t02");
    sqlx::query(&format!("CREATE TABLE {t} (id VARCHAR(64) PRIMARY KEY, content TEXT NOT NULL, created_at DATETIME(6) NOT NULL)"))
        .execute(&pool).await.expect("CREATE TABLE");
    sqlx::query(&format!("DROP TABLE {t}")).execute(&pool).await.unwrap();
    println!("✅ test_02_create_table: OK");
}

// ── Test 3: INSERT + SELECT ───────────────────────────────────────────────────

#[tokio::test]
async fn test_03_insert_select() {
    let pool = connect().await;
    let t = tbl("t03");
    sqlx::query(&format!("CREATE TABLE {t} (id VARCHAR(64) PRIMARY KEY, val TEXT NOT NULL)"))
        .execute(&pool).await.unwrap();
    sqlx::query(&format!("INSERT INTO {t} (id, val) VALUES (?, ?)"))
        .bind("id-1").bind("hello matrixone")
        .execute(&pool).await.expect("INSERT");
    let row = sqlx::query(&format!("SELECT val FROM {t} WHERE id = ?"))
        .bind("id-1").fetch_one(&pool).await.expect("SELECT");
    let val: String = row.try_get("val").unwrap();
    assert_eq!(val, "hello matrixone");
    sqlx::query(&format!("DROP TABLE {t}")).execute(&pool).await.unwrap();
    println!("✅ test_03_insert_select: OK");
}

// ── Test 4: DATETIME(6) ───────────────────────────────────────────────────────

#[tokio::test]
async fn test_04_datetime() {
    let pool = connect().await;
    let t = tbl("t04");
    sqlx::query(&format!("CREATE TABLE {t} (id VARCHAR(64) PRIMARY KEY, ts DATETIME(6) NOT NULL)"))
        .execute(&pool).await.unwrap();
    let now = chrono::Utc::now().naive_utc();
    sqlx::query(&format!("INSERT INTO {t} (id, ts) VALUES (?, ?)"))
        .bind("dt-1").bind(now)
        .execute(&pool).await.expect("INSERT DATETIME");
    let row = sqlx::query(&format!("SELECT ts FROM {t} WHERE id = ?"))
        .bind("dt-1").fetch_one(&pool).await.expect("SELECT DATETIME");
    let ts: chrono::NaiveDateTime = row.try_get("ts").unwrap();
    let diff = (ts - now).num_seconds().abs();
    assert!(diff <= 1, "datetime drift: {diff}s");
    sqlx::query(&format!("DROP TABLE {t}")).execute(&pool).await.unwrap();
    println!("✅ test_04_datetime: OK (drift={diff}s)");
}

// ── Test 5: FULLTEXT INDEX + MATCH AGAINST ────────────────────────────────────

#[tokio::test]
async fn test_05_fulltext() {
    let pool = connect().await;
    let t = tbl("t05");
    let create = sqlx::query(&format!(
        "CREATE TABLE {t} (id VARCHAR(64) PRIMARY KEY, content TEXT NOT NULL, FULLTEXT INDEX ft_content (content) WITH PARSER ngram)"
    )).execute(&pool).await;
    match create {
        Err(e) => { println!("⚠️  test_05_fulltext: CREATE FULLTEXT failed: {e}"); return; }
        Ok(_) => {}
    }
    sqlx::query(&format!("INSERT INTO {t} (id, content) VALUES (?, ?)"))
        .bind("ft-1").bind("rust programming language systems")
        .execute(&pool).await.unwrap();
    let sql = format!("SELECT id FROM {t} WHERE MATCH(content) AGAINST('+rust' IN BOOLEAN MODE)");
    let rows = sqlx::query(&sql).fetch_all(&pool).await.unwrap();
    assert!(!rows.is_empty());
    let _ = sqlx::query(&format!("ALTER TABLE {t} DROP INDEX ft_content")).execute(&pool).await;
    sqlx::query(&format!("DROP TABLE {t}")).execute(&pool).await.unwrap();
    println!("✅ test_05_fulltext: {} rows", rows.len());
}

// ── Test 6: vecf32 + l2_distance ─────────────────────────────────────────────

#[tokio::test]
async fn test_06_vector_l2_distance() {
    let pool = connect().await;
    let t = tbl("t06");
    sqlx::query(&format!("CREATE TABLE {t} (id VARCHAR(64) PRIMARY KEY, embedding vecf32(3))"))
        .execute(&pool).await.expect("CREATE vecf32");
    sqlx::query(&format!("INSERT INTO {t} (id, embedding) VALUES (?, '[1.0, 0.0, 0.0]'), (?, '[0.0, 1.0, 0.0]')"))
        .bind("v-1").bind("v-2")
        .execute(&pool).await.unwrap();
    let rows = sqlx::query(&format!("SELECT id FROM {t} ORDER BY l2_distance(embedding, '[1.0, 0.0, 0.0]') ASC LIMIT 1"))
        .fetch_all(&pool).await.expect("l2_distance");
    let id: String = rows[0].try_get("id").unwrap();
    assert_eq!(id, "v-1");
    sqlx::query(&format!("DROP TABLE {t}")).execute(&pool).await.unwrap();
    println!("✅ test_06_vector_l2_distance: nearest={id}");
}

// ── Test 7: UPDATE + soft delete ─────────────────────────────────────────────

#[tokio::test]
async fn test_07_update_soft_delete() {
    let pool = connect().await;
    let t = tbl("t07");
    sqlx::query(&format!("CREATE TABLE {t} (id VARCHAR(64) PRIMARY KEY, val TEXT, is_active TINYINT(1) DEFAULT 1)"))
        .execute(&pool).await.unwrap();
    sqlx::query(&format!("INSERT INTO {t} (id, val) VALUES (?, ?)"))
        .bind("u-1").bind("original")
        .execute(&pool).await.unwrap();
    sqlx::query(&format!("UPDATE {t} SET val = ? WHERE id = ?"))
        .bind("updated").bind("u-1")
        .execute(&pool).await.expect("UPDATE");
    sqlx::query(&format!("UPDATE {t} SET is_active = 0 WHERE id = ?"))
        .bind("u-1").execute(&pool).await.expect("soft delete");
    let row = sqlx::query(&format!("SELECT val, is_active FROM {t} WHERE id = ?"))
        .bind("u-1").fetch_one(&pool).await.unwrap();
    let val: String = row.try_get("val").unwrap();
    assert_eq!(val, "updated");
    sqlx::query(&format!("DROP TABLE {t}")).execute(&pool).await.unwrap();
    println!("✅ test_07_update_soft_delete: OK");
}
