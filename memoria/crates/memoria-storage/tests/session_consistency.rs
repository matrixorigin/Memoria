/// Strict session-consistency integration tests against real MatrixOne.
/// Uses dedicated MySQL connections so each connection is a stable database
/// session instead of a pooled, interchangeable handle.
use chrono::Utc;
use sqlx::{mysql::MySqlConnection, Connection, Row};
use tokio::time::{sleep, Duration, Instant};
use uuid::Uuid;

const WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const STRESS_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const FAST_POLL_INTERVAL: Duration = Duration::from_millis(5);
const STRESS_VERSIONS: i64 = 32;

fn db_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria".to_string())
}

fn tbl(prefix: &str) -> String {
    format!("{}_{}", prefix, Uuid::new_v4().simple())
}

async fn connect_session() -> MySqlConnection {
    MySqlConnection::connect(&db_url())
        .await
        .expect("connect to MatrixOne")
}

async fn connection_id(conn: &mut MySqlConnection) -> u32 {
    let row = sqlx::query("SELECT CONNECTION_ID() AS id")
        .fetch_one(conn)
        .await
        .expect("fetch connection id");
    row.try_get("id").expect("decode connection id")
}

async fn recreate_table(table: &str) {
    let mut conn = connect_session().await;
    sqlx::query(&format!("DROP TABLE IF EXISTS {table}"))
        .execute(&mut conn)
        .await
        .expect("drop old test table");
    sqlx::query(&format!(
        "CREATE TABLE {table} (
            id VARCHAR(64) PRIMARY KEY,
            version BIGINT NOT NULL,
            note TEXT NOT NULL,
            updated_at DATETIME(6) NOT NULL
        )"
    ))
    .execute(&mut conn)
    .await
    .expect("create test table");
}

async fn drop_table(table: &str) {
    let mut conn = connect_session().await;
    sqlx::query(&format!("DROP TABLE IF EXISTS {table}"))
        .execute(&mut conn)
        .await
        .expect("drop test table");
}

async fn insert_row(conn: &mut MySqlConnection, table: &str, id: &str, version: i64, note: &str) {
    sqlx::query(&format!(
        "INSERT INTO {table} (id, version, note, updated_at) VALUES (?, ?, ?, ?)"
    ))
    .bind(id)
    .bind(version)
    .bind(note)
    .bind(Utc::now().naive_utc())
    .execute(conn)
    .await
    .expect("insert row");
}

async fn update_row(conn: &mut MySqlConnection, table: &str, id: &str, version: i64, note: &str) {
    sqlx::query(&format!(
        "UPDATE {table} SET version = ?, note = ?, updated_at = ? WHERE id = ?"
    ))
    .bind(version)
    .bind(note)
    .bind(Utc::now().naive_utc())
    .bind(id)
    .execute(conn)
    .await
    .expect("update row");
}

async fn read_row(conn: &mut MySqlConnection, table: &str, id: &str) -> Option<(i64, String)> {
    let row = sqlx::query(&format!("SELECT version, note FROM {table} WHERE id = ?"))
        .bind(id)
        .fetch_optional(conn)
        .await
        .expect("read row");

    row.map(|row| {
        (
            row.try_get("version").expect("decode version"),
            row.try_get("note").expect("decode note"),
        )
    })
}

async fn read_required_row(conn: &mut MySqlConnection, table: &str, id: &str) -> (i64, String) {
    read_row(conn, table, id)
        .await
        .unwrap_or_else(|| panic!("row {id} should exist in {table}"))
}

async fn begin_transaction(conn: &mut MySqlConnection) {
    sqlx::raw_sql("BEGIN")
        .execute(conn)
        .await
        .expect("start transaction");
}

async fn commit_transaction(conn: &mut MySqlConnection) {
    sqlx::raw_sql("COMMIT")
        .execute(conn)
        .await
        .expect("commit transaction");
}

async fn assert_row_absent_for(
    conn: &mut MySqlConnection,
    table: &str,
    id: &str,
    duration: Duration,
) {
    let deadline = Instant::now() + duration;
    loop {
        let row = read_row(conn, table, id).await;
        assert!(
            row.is_none(),
            "{table}.{id} became visible before it should: {row:?}"
        );
        if Instant::now() >= deadline {
            return;
        }
        sleep(FAST_POLL_INTERVAL).await;
    }
}

fn assert_non_decreasing(history: &[i64], label: &str) {
    assert!(
        !history.is_empty(),
        "{label} should capture some observations"
    );
    for window in history.windows(2) {
        assert!(
            window[1] >= window[0],
            "{label} regressed: saw {:?}",
            history
        );
    }
}

async fn observe_monotonic_history(
    mut conn: MySqlConnection,
    table: String,
    id: String,
    final_version: i64,
    label: &str,
) -> Vec<i64> {
    let deadline = Instant::now() + STRESS_TIMEOUT;
    let mut seen = Vec::new();
    let mut last_seen = i64::MIN;

    loop {
        let (version, _) = read_required_row(&mut conn, &table, &id).await;
        assert!(
            version >= last_seen,
            "{label} violated monotonicity: saw {version} after {last_seen}"
        );
        last_seen = version;
        seen.push(version);
        if version == final_version {
            for _ in 0..10 {
                let (stable, _) = read_required_row(&mut conn, &table, &id).await;
                assert_eq!(
                    stable, final_version,
                    "{label} regressed after observing final version"
                );
            }
            return seen;
        }

        assert!(
            Instant::now() < deadline,
            "{label} never observed final version {final_version}"
        );
        sleep(FAST_POLL_INTERVAL).await;
    }
}

async fn wait_for_min_version(
    conn: &mut MySqlConnection,
    table: &str,
    id: &str,
    min_version: i64,
) -> (i64, String) {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    loop {
        if let Some((version, note)) = read_row(conn, table, id).await {
            if version >= min_version {
                return (version, note);
            }
        }

        assert!(
            Instant::now() < deadline,
            "timed out waiting for {table}.{id} to reach version >= {min_version}"
        );
        sleep(POLL_INTERVAL).await;
    }
}

#[tokio::test]
async fn test_session_read_your_writes() {
    let table = tbl("sess_ryw");
    recreate_table(&table).await;

    let mut writer = connect_session().await;
    let mut observer = connect_session().await;
    let writer_id = connection_id(&mut writer).await;
    let observer_id = connection_id(&mut observer).await;
    assert_ne!(writer_id, observer_id, "must use distinct DB sessions");

    insert_row(&mut writer, &table, "state", 1, "writer-v1").await;
    let first = read_required_row(&mut writer, &table, "state").await;
    assert_eq!(first.0, 1);
    assert_eq!(first.1, "writer-v1");

    update_row(&mut writer, &table, "state", 2, "writer-v2").await;
    let second = read_required_row(&mut writer, &table, "state").await;
    assert_eq!(second.0, 2);
    assert_eq!(second.1, "writer-v2");

    let observer_final = wait_for_min_version(&mut observer, &table, "state", 2).await;
    assert_eq!(observer_final.0, 2);

    drop_table(&table).await;
}

#[tokio::test]
async fn test_session_transaction_read_your_writes_and_commit_visibility() {
    let table = tbl("sess_tx");
    recreate_table(&table).await;

    let mut writer = connect_session().await;
    let mut observer = connect_session().await;
    assert_ne!(
        connection_id(&mut writer).await,
        connection_id(&mut observer).await,
        "must use distinct DB sessions"
    );

    begin_transaction(&mut writer).await;
    insert_row(&mut writer, &table, "state", 1, "tx-v1").await;

    let first = read_required_row(&mut writer, &table, "state").await;
    assert_eq!(first.0, 1);
    assert_eq!(first.1, "tx-v1");
    assert_row_absent_for(&mut observer, &table, "state", Duration::from_millis(250)).await;

    update_row(&mut writer, &table, "state", 2, "tx-v2").await;
    let second = read_required_row(&mut writer, &table, "state").await;
    assert_eq!(second.0, 2);
    assert_eq!(second.1, "tx-v2");
    assert_row_absent_for(&mut observer, &table, "state", Duration::from_millis(250)).await;

    commit_transaction(&mut writer).await;

    let visible = wait_for_min_version(&mut observer, &table, "state", 2).await;
    assert_eq!(visible.0, 2);
    assert_eq!(visible.1, "tx-v2");

    drop_table(&table).await;
}

#[tokio::test]
async fn test_session_monotonic_reads() {
    let table = tbl("sess_mr");
    recreate_table(&table).await;

    let mut writer = connect_session().await;
    let mut reader = connect_session().await;
    assert_ne!(
        connection_id(&mut writer).await,
        connection_id(&mut reader).await,
        "must use distinct DB sessions"
    );

    insert_row(&mut writer, &table, "state", 1, "v1").await;

    let first = read_required_row(&mut reader, &table, "state").await;
    assert_eq!(first.0, 1);

    update_row(&mut writer, &table, "state", 2, "v2").await;
    let mut last_seen = wait_for_min_version(&mut reader, &table, "state", 2)
        .await
        .0;
    assert_eq!(last_seen, 2);

    update_row(&mut writer, &table, "state", 3, "v3").await;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (version, _) = read_required_row(&mut reader, &table, "state").await;
        assert!(
            version >= last_seen,
            "monotonic read violated in session: saw {version} after {last_seen}"
        );
        last_seen = version;
        if version == 3 {
            break;
        }

        assert!(Instant::now() < deadline, "reader never observed version 3");
        sleep(Duration::from_millis(20)).await;
    }

    for _ in 0..10 {
        let (version, _) = read_required_row(&mut reader, &table, "state").await;
        assert_eq!(
            version, 3,
            "reader regressed after observing the latest version"
        );
    }

    drop_table(&table).await;
}

#[tokio::test]
async fn test_session_monotonic_reads_stress_multi_reader() {
    let table = tbl("sess_mr_stress");
    recreate_table(&table).await;

    let mut writer = connect_session().await;
    let mut reader1 = connect_session().await;
    let mut reader2 = connect_session().await;

    let writer_id = connection_id(&mut writer).await;
    let reader1_id = connection_id(&mut reader1).await;
    let reader2_id = connection_id(&mut reader2).await;
    assert_ne!(writer_id, reader1_id);
    assert_ne!(writer_id, reader2_id);
    assert_ne!(reader1_id, reader2_id);

    insert_row(&mut writer, &table, "state", 0, "v0").await;

    let writer_table = table.clone();
    let writer_task = async move {
        let mut writer = writer;
        for version in 1..=STRESS_VERSIONS {
            update_row(
                &mut writer,
                &writer_table,
                "state",
                version,
                &format!("writer-v{version}"),
            )
            .await;
            sleep(Duration::from_millis(3 + (version % 5) as u64)).await;
        }
    };

    let reader1_task = observe_monotonic_history(
        reader1,
        table.clone(),
        "state".to_string(),
        STRESS_VERSIONS,
        "reader1",
    );
    let reader2_task = observe_monotonic_history(
        reader2,
        table.clone(),
        "state".to_string(),
        STRESS_VERSIONS,
        "reader2",
    );

    let (_, seen1, seen2) = tokio::join!(writer_task, reader1_task, reader2_task);
    assert_non_decreasing(&seen1, "reader1");
    assert_non_decreasing(&seen2, "reader2");
    assert_eq!(*seen1.last().expect("reader1 final"), STRESS_VERSIONS);
    assert_eq!(*seen2.last().expect("reader2 final"), STRESS_VERSIONS);

    drop_table(&table).await;
}

#[tokio::test]
async fn test_session_monotonic_writes() {
    let table = tbl("sess_mw");
    recreate_table(&table).await;

    let mut writer = connect_session().await;
    let mut observer = connect_session().await;
    assert_ne!(
        connection_id(&mut writer).await,
        connection_id(&mut observer).await,
        "must use distinct DB sessions"
    );

    insert_row(&mut writer, &table, "state", 0, "v0").await;

    let writer_task = async {
        for version in 1..=5 {
            update_row(
                &mut writer,
                &table,
                "state",
                version,
                &format!("writer-v{version}"),
            )
            .await;
            sleep(Duration::from_millis(35)).await;
        }
    };

    let observer_task = async {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = Vec::new();
        loop {
            let (version, _) = read_required_row(&mut observer, &table, "state").await;
            seen.push(version);
            if version == 5 {
                return seen;
            }

            assert!(
                Instant::now() < deadline,
                "observer never reached version 5"
            );
            sleep(Duration::from_millis(10)).await;
        }
    };

    let (_, seen) = tokio::join!(writer_task, observer_task);
    assert!(
        !seen.is_empty(),
        "observer should capture at least one committed value"
    );

    for window in seen.windows(2) {
        assert!(
            window[1] >= window[0],
            "monotonic writes violated: observer saw {:?}",
            seen
        );
    }

    let final_observed = read_required_row(&mut observer, &table, "state").await;
    assert_eq!(final_observed.0, 5);

    drop_table(&table).await;
}

#[tokio::test]
async fn test_session_writes_follow_reads() {
    let table = tbl("sess_wfr");
    recreate_table(&table).await;

    let mut source_writer = connect_session().await;
    let mut reader_writer = connect_session().await;
    let mut observer = connect_session().await;

    let source_writer_id = connection_id(&mut source_writer).await;
    let reader_writer_id = connection_id(&mut reader_writer).await;
    let observer_id = connection_id(&mut observer).await;
    assert_ne!(source_writer_id, reader_writer_id);
    assert_ne!(source_writer_id, observer_id);
    assert_ne!(reader_writer_id, observer_id);

    insert_row(&mut source_writer, &table, "source", 1, "source-v1").await;
    update_row(&mut source_writer, &table, "source", 2, "source-v2").await;

    let source_seen = wait_for_min_version(&mut reader_writer, &table, "source", 2).await;
    assert_eq!(source_seen.0, 2);
    assert_eq!(source_seen.1, "source-v2");

    insert_row(
        &mut reader_writer,
        &table,
        "derived",
        2,
        "derived-after-reading-source-v2",
    )
    .await;

    let derived_seen = wait_for_min_version(&mut observer, &table, "derived", 2).await;
    assert_eq!(derived_seen.0, 2);

    for _ in 0..10 {
        let (source_version, source_note) =
            read_required_row(&mut observer, &table, "source").await;
        assert_eq!(
            source_version, 2,
            "writes-follow-reads violated: observer saw derived@2 before source@2"
        );
        assert_eq!(source_note, "source-v2");
    }

    drop_table(&table).await;
}

#[tokio::test]
async fn test_session_writes_follow_reads_stress() {
    let table = tbl("sess_wfr_stress");
    recreate_table(&table).await;

    let mut source_writer = connect_session().await;
    let mut relay = connect_session().await;
    let mut observer = connect_session().await;

    let source_writer_id = connection_id(&mut source_writer).await;
    let relay_id = connection_id(&mut relay).await;
    let observer_id = connection_id(&mut observer).await;
    assert_ne!(source_writer_id, relay_id);
    assert_ne!(source_writer_id, observer_id);
    assert_ne!(relay_id, observer_id);

    insert_row(&mut source_writer, &table, "source", 0, "source-v0").await;
    insert_row(&mut relay, &table, "derived", 0, "derived-v0").await;

    for version in 1..=STRESS_VERSIONS {
        update_row(
            &mut source_writer,
            &table,
            "source",
            version,
            &format!("source-v{version}"),
        )
        .await;

        let seen_source = wait_for_min_version(&mut relay, &table, "source", version).await;
        assert_eq!(seen_source.0, version);

        update_row(
            &mut relay,
            &table,
            "derived",
            version,
            &format!("derived-after-source-v{version}"),
        )
        .await;

        let seen_derived = wait_for_min_version(&mut observer, &table, "derived", version).await;
        assert_eq!(seen_derived.0, version);

        for _ in 0..3 {
            let (observed_source, observed_note) =
                read_required_row(&mut observer, &table, "source").await;
            assert!(
                observed_source >= version,
                "writes-follow-reads violated at version {version}: observer saw derived first but source={observed_source}"
            );
            assert!(
                observed_note.starts_with("source-v"),
                "unexpected source note while checking causality: {observed_note}"
            );
        }
    }

    let (final_source, _) = read_required_row(&mut observer, &table, "source").await;
    let (final_derived, _) = read_required_row(&mut observer, &table, "derived").await;
    assert_eq!(final_source, STRESS_VERSIONS);
    assert_eq!(final_derived, STRESS_VERSIONS);

    drop_table(&table).await;
}
