use std::collections::{BTreeMap, BTreeSet};

use sqlx::{MySqlPool, Row};

const V2_REGISTRY_TABLE: &str = "mem_v2_user_tables";

fn validate_table_name(table: &str) -> Result<(), String> {
    if table.is_empty()
        || table.len() > 64
        || !table
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(format!("Invalid table name: {table}"));
    }
    Ok(())
}

async fn table_exists(pool: &MySqlPool, table_name: &str) -> Result<bool, String> {
    validate_table_name(table_name)?;
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = DATABASE() AND table_name = ?",
    )
    .bind(table_name)
    .fetch_one(pool)
    .await
    .map_err(|e| e.to_string())?;
    Ok(count > 0)
}

pub async fn active_memory_count_from_table(
    pool: &MySqlPool,
    heads_table: &str,
) -> Result<i64, String> {
    validate_table_name(heads_table)?;
    if !table_exists(pool, heads_table).await? {
        return Ok(0);
    }
    sqlx::query_scalar::<_, i64>(&format!(
        "SELECT COUNT(*) FROM {} WHERE forgotten_at IS NULL",
        heads_table
    ))
    .fetch_one(pool)
    .await
    .map_err(|e| e.to_string())
}

pub async fn active_memory_count_for_user(pool: &MySqlPool, user_id: &str) -> Result<i64, String> {
    let family = memoria_storage::MemoryV2TableFamily::for_user(user_id);
    active_memory_count_from_table(pool, &family.heads_table).await
}

pub async fn active_user_counts(pool: &MySqlPool) -> Result<Vec<(String, i64)>, String> {
    if !table_exists(pool, V2_REGISTRY_TABLE).await? {
        return Ok(vec![]);
    }

    let rows = sqlx::query(&format!(
        "SELECT user_id, heads_table FROM {} ORDER BY user_id",
        V2_REGISTRY_TABLE
    ))
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;

    let mut users = Vec::new();
    for row in rows {
        let user_id: String = row.try_get("user_id").map_err(|e| e.to_string())?;
        let heads_table: String = row.try_get("heads_table").map_err(|e| e.to_string())?;
        let active_count = active_memory_count_from_table(pool, &heads_table).await?;
        if active_count > 0 {
            users.push((user_id, active_count));
        }
    }
    Ok(users)
}

pub async fn reset_user_access_counts(pool: &MySqlPool, user_id: &str) -> Result<i64, String> {
    let family = memoria_storage::MemoryV2TableFamily::for_user(user_id);
    if !table_exists(pool, &family.stats_table).await? {
        return Ok(0);
    }
    let result = sqlx::query(&format!(
        "UPDATE {} SET access_count = 0",
        family.stats_table
    ))
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;
    Ok(result.rows_affected() as i64)
}

pub async fn soft_delete_user_memories(pool: &MySqlPool, user_id: &str) -> Result<(), String> {
    let family = memoria_storage::MemoryV2TableFamily::for_user(user_id);
    if !table_exists(pool, &family.heads_table).await? {
        return Ok(());
    }

    sqlx::query(&format!(
        "UPDATE {} SET is_active = 0, forgotten_at = COALESCE(forgotten_at, NOW(6)), updated_at = NOW(6) \
         WHERE forgotten_at IS NULL",
        family.heads_table
    ))
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub async fn collect_metrics(
    pool: &MySqlPool,
) -> Result<(BTreeMap<String, i64>, BTreeSet<String>), String> {
    if !table_exists(pool, V2_REGISTRY_TABLE).await? {
        return Ok((BTreeMap::new(), BTreeSet::new()));
    }

    let rows = sqlx::query(&format!(
        "SELECT user_id, heads_table FROM {} ORDER BY user_id",
        V2_REGISTRY_TABLE
    ))
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;

    let mut counts_by_type = BTreeMap::new();
    let mut active_users = BTreeSet::new();

    for row in rows {
        let user_id: String = row.try_get("user_id").map_err(|e| e.to_string())?;
        let heads_table: String = row.try_get("heads_table").map_err(|e| e.to_string())?;
        validate_table_name(&heads_table)?;
        if !table_exists(pool, &heads_table).await? {
            continue;
        }

        let type_rows: Vec<(String, i64)> = sqlx::query_as(&format!(
            "SELECT memory_type, COUNT(*) FROM {} WHERE forgotten_at IS NULL GROUP BY memory_type",
            heads_table
        ))
        .fetch_all(pool)
        .await
        .map_err(|e| e.to_string())?;

        let user_total = type_rows.iter().map(|(_, count)| *count).sum::<i64>();
        if user_total == 0 {
            continue;
        }

        active_users.insert(user_id);
        for (memory_type, count) in type_rows {
            *counts_by_type.entry(memory_type).or_insert(0) += count;
        }
    }

    Ok((counts_by_type, active_users))
}
