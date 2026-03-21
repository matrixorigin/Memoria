/// MatrixOne correct usage tests — based on Python implementation analysis.
///
/// Key findings:
/// 1. JSON: use sqlx::types::Json<T> or CAST(col AS CHAR) — not raw String
/// 2. Vector: vecf32(N) column + l2_distance(col, '[...]') string literal
/// 3. Fulltext: MATCH(col) AGAINST('+term' IN BOOLEAN MODE) with NGRAM parser, inline string
use sqlx::{mysql::MySqlPool, Row};

async fn connect() -> MySqlPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria_test".to_string());
    MySqlPool::connect(&url)
        .await
        .expect("connect to MatrixOne")
}

// ── JSON: correct approach — CAST(col AS CHAR) on read ───────────────────────

#[tokio::test]
async fn test_json_cast_char_read() {
    let pool = connect().await;
    sqlx::query("DROP TABLE IF EXISTS _t_json")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE _t_json (id VARCHAR(64) PRIMARY KEY, data JSON)")
        .execute(&pool)
        .await
        .unwrap();

    let payload = serde_json::json!({"nums": [1, 2, 3], "name": "test"});
    let payload_str = payload.to_string();

    sqlx::query("INSERT INTO _t_json (id, data) VALUES (?, ?)")
        .bind("j1")
        .bind(&payload_str)
        .execute(&pool)
        .await
        .unwrap();

    // Read back with CAST(data AS CHAR)
    let row = sqlx::query("SELECT CAST(data AS CHAR) AS data_str FROM _t_json WHERE id = ?")
        .bind("j1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let s: String = row.try_get("data_str").unwrap();
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert_eq!(v["name"], "test");
    assert_eq!(v["nums"][0], 1);

    sqlx::query("DROP TABLE IF EXISTS _t_json")
        .execute(&pool)
        .await
        .unwrap();
    println!("✅ JSON: CAST(data AS CHAR) works correctly");
}

// ── Vector: vecf32(N) + l2_distance with string literal ─────────────────────

#[tokio::test]
async fn test_vector_vecf32_crud() {
    let pool = connect().await;
    sqlx::query("DROP TABLE IF EXISTS _t_vec")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE _t_vec (id VARCHAR(64) PRIMARY KEY, embedding vecf32(4))")
        .execute(&pool)
        .await
        .unwrap();

    // INSERT: use string format '[f1, f2, f3, f4]'
    let vecs: &[(&str, [f32; 4])] = &[
        ("v1", [1.0, 0.0, 0.0, 0.0]),
        ("v2", [0.0, 1.0, 0.0, 0.0]),
        ("v3", [0.0, 0.0, 1.0, 0.0]),
    ];
    for (id, v) in vecs {
        let vec_str = format!("[{}, {}, {}, {}]", v[0], v[1], v[2], v[3]);
        sqlx::query("INSERT INTO _t_vec (id, embedding) VALUES (?, ?)")
            .bind(id)
            .bind(&vec_str)
            .execute(&pool)
            .await
            .unwrap();
    }

    // Query: l2_distance with inline string literal (NOT ? binding)
    // Build the query string with the vector literal inlined
    let query_vec = [1.0f32, 0.0, 0.0, 0.0];
    let vec_literal = format!(
        "[{}, {}, {}, {}]",
        query_vec[0], query_vec[1], query_vec[2], query_vec[3]
    );
    let sql = format!(
        "SELECT id FROM _t_vec ORDER BY l2_distance(embedding, '{}') ASC LIMIT 2",
        vec_literal
    );
    let rows = sqlx::query(&sql).fetch_all(&pool).await.unwrap();
    let nearest: String = rows[0].try_get("id").unwrap();
    assert_eq!(nearest, "v1");
    println!("✅ Vector: vecf32 + l2_distance string literal, nearest={nearest}");

    // Also verify we can read the embedding back — vecf32 returns as String "[f1,f2,...]"
    let row = sqlx::query("SELECT embedding FROM _t_vec WHERE id = ?")
        .bind("v1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let emb_str: String = row.try_get("embedding").unwrap();
    println!("  embedding read back as String: {emb_str}");
    // Parse "[1,0,0,0]" → Vec<f32>
    let parsed: Vec<f32> = emb_str
        .trim_matches(|c| c == '[' || c == ']')
        .split(',')
        .map(|s| s.trim().parse().unwrap())
        .collect();
    assert_eq!(parsed[0], 1.0f32);

    sqlx::query("DROP TABLE IF EXISTS _t_vec")
        .execute(&pool)
        .await
        .unwrap();
}

// ── Fulltext: NGRAM parser + MATCH AGAINST with inline string ────────────────

#[tokio::test]
async fn test_fulltext_ngram_boolean_mode() {
    let pool = connect().await;
    sqlx::query("DROP TABLE IF EXISTS _t_ft")
        .execute(&pool)
        .await
        .unwrap();

    // Python uses FulltextParserType.NGRAM — try WITH PARSER ngram
    let create = sqlx::query(
        "CREATE TABLE _t_ft (
            id VARCHAR(64) PRIMARY KEY,
            content TEXT NOT NULL,
            FULLTEXT INDEX ft_content (content) WITH PARSER ngram
        )",
    )
    .execute(&pool)
    .await;

    match create {
        Err(e) => {
            println!("⚠️  NGRAM parser failed: {e}");
            // Fallback: try without parser
            sqlx::query("DROP TABLE IF EXISTS _t_ft")
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query(
                "CREATE TABLE _t_ft (
                    id VARCHAR(64) PRIMARY KEY,
                    content TEXT NOT NULL,
                    FULLTEXT INDEX ft_content (content)
                )",
            )
            .execute(&pool)
            .await
            .unwrap();
            println!("  Fallback: FULLTEXT without NGRAM parser");
        }
        Ok(_) => println!("  FULLTEXT with NGRAM parser created OK"),
    }

    sqlx::query("INSERT INTO _t_ft (id, content) VALUES (?, ?), (?, ?), (?, ?)")
        .bind("e1")
        .bind("rust programming language systems performance")
        .bind("e2")
        .bind("python memory service embedding vector")
        .bind("e3")
        .bind("matrixone database vector search fulltext")
        .execute(&pool)
        .await
        .unwrap();

    // Python pattern: MATCH(content) AGAINST('+term' IN BOOLEAN MODE) — inline string
    let term = "rust";
    let safe_term = term.replace('\'', "").replace('\\', "");
    let sql = format!(
        "SELECT id, MATCH(content) AGAINST('+{safe_term}' IN BOOLEAN MODE) AS score \
         FROM _t_ft \
         WHERE MATCH(content) AGAINST('+{safe_term}' IN BOOLEAN MODE) \
         ORDER BY score DESC"
    );
    let rows = sqlx::query(&sql).fetch_all(&pool).await.unwrap();
    println!("  FULLTEXT '+rust': {} rows", rows.len());
    assert!(!rows.is_empty());
    let id: String = rows[0].try_get("id").unwrap();
    assert_eq!(id, "e1");
    println!("✅ Fulltext: MATCH AGAINST inline string works, top={id}");

    // Test Chinese with NGRAM
    sqlx::query("INSERT INTO _t_ft (id, content) VALUES (?, ?)")
        .bind("e4")
        .bind("向量数据库内存检索系统")
        .execute(&pool)
        .await
        .unwrap();

    let cn_term = "向量";
    let sql2 =
        format!("SELECT id FROM _t_ft WHERE MATCH(content) AGAINST('{cn_term}' IN BOOLEAN MODE)");
    let rows2 = sqlx::query(&sql2).fetch_all(&pool).await;
    match rows2 {
        Err(e) => println!("⚠️  Chinese fulltext failed: {e}"),
        Ok(rows) => println!("✅ Chinese fulltext: {} rows for '向量'", rows.len()),
    }

    // Drop index before table to avoid MatrixOne internal index metadata leak
    let _ = sqlx::query("ALTER TABLE _t_ft DROP INDEX ft_content")
        .execute(&pool)
        .await;
    let _ = sqlx::query("ALTER TABLE _t_ft DROP INDEX ft_content2")
        .execute(&pool)
        .await;
    sqlx::query("DROP TABLE IF EXISTS _t_ft")
        .execute(&pool)
        .await
        .unwrap();
}

// ── Combined: the actual mem_memories schema ─────────────────────────────────

#[tokio::test]
async fn test_mem_memories_schema() {
    let pool = connect().await;
    let dim = std::env::var("EMBEDDING_DIM")
        .unwrap_or_else(|_| "4".to_string())
        .parse::<usize>()
        .unwrap_or(4);

    sqlx::query("DROP TABLE IF EXISTS _t_memories")
        .execute(&pool)
        .await
        .unwrap();

    let create_sql = format!(
        "CREATE TABLE _t_memories (
            memory_id VARCHAR(64) PRIMARY KEY,
            user_id VARCHAR(64) NOT NULL,
            memory_type VARCHAR(20) NOT NULL,
            content TEXT NOT NULL,
            embedding vecf32({dim}),
            session_id VARCHAR(64),
            source_event_ids JSON,
            extra_metadata JSON,
            is_active TINYINT(1) NOT NULL DEFAULT 1,
            superseded_by VARCHAR(64),
            trust_tier VARCHAR(10) DEFAULT 'T3',
            initial_confidence FLOAT DEFAULT 0.75,
            observed_at DATETIME(6) NOT NULL,
            created_at DATETIME(6) NOT NULL,
            updated_at DATETIME(6),
            INDEX idx_user_active (user_id, is_active),
            FULLTEXT INDEX ft_content (content) WITH PARSER ngram
        )"
    );

    sqlx::query(&create_sql)
        .execute(&pool)
        .await
        .expect("CREATE mem_memories schema");
    println!("✅ mem_memories schema created with vecf32({dim}) + FULLTEXT NGRAM");

    // Insert a test row
    let now = chrono::Utc::now().naive_utc();
    let vec_str = format!("[{}]", vec!["0.1"; dim].join(", "));
    sqlx::query(
        "INSERT INTO _t_memories
         (memory_id, user_id, memory_type, content, embedding, source_event_ids, is_active, observed_at, created_at)
         VALUES (?, ?, ?, ?, ?, ?, 1, ?, ?)"
    )
    .bind("m1").bind("user1").bind("semantic")
    .bind("test memory content for rust rewrite")
    .bind(&vec_str)
    .bind("[]")
    .bind(now).bind(now)
    .execute(&pool).await.expect("INSERT test memory");

    // Read back — JSON via CAST
    let row = sqlx::query(
        "SELECT memory_id, content, is_active, CAST(source_event_ids AS CHAR) AS src_ids
         FROM _t_memories WHERE memory_id = ?",
    )
    .bind("m1")
    .fetch_one(&pool)
    .await
    .unwrap();

    let mid: String = row.try_get("memory_id").unwrap();
    let content: String = row.try_get("content").unwrap();
    let is_active: i8 = row.try_get("is_active").unwrap();
    let src_ids: String = row.try_get("src_ids").unwrap();
    assert_eq!(mid, "m1");
    assert_eq!(is_active, 1);
    let _: serde_json::Value = serde_json::from_str(&src_ids).unwrap();
    println!("✅ mem_memories INSERT+SELECT: id={mid}, content={content:?}, is_active={is_active}");

    let _ = sqlx::query("ALTER TABLE _t_memories DROP INDEX ft_content")
        .execute(&pool)
        .await;
    sqlx::query("DROP TABLE IF EXISTS _t_memories")
        .execute(&pool)
        .await
        .unwrap();
}
