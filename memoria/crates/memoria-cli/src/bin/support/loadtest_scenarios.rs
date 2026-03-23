use super::{
    rand_pick, v2_loadtest_api, ApiClient, ApiVersion, Stats, CONTENTS, MEMORY_TYPES, QUERIES,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) async fn session_loop(c: &ApiClient, duration: Duration) -> Vec<Arc<Stats>> {
    let retrieve = Arc::new(Stats::new("retrieve"));
    let store = Arc::new(Stats::new("store"));
    let search = Arc::new(Stats::new("search"));
    let correct = Arc::new(Stats::new("correct"));
    let list = Arc::new(Stats::new("list"));
    let purge = Arc::new(Stats::new("purge"));

    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        for _ in 0..fastrand::usize(3..=8) {
            if Instant::now() >= deadline {
                break;
            }
            match fastrand::u32(0..10) {
                0..=4 => {
                    c.remember(rand_pick(CONTENTS), rand_pick(MEMORY_TYPES), &store)
                        .await;
                }
                5 => {
                    c.search(rand_pick(QUERIES), 5, &search, &[200]).await;
                }
                6..=7 => {
                    c.recall(rand_pick(QUERIES), 5, &retrieve, &[200]).await;
                }
                8 => {
                    c.correct(
                        rand_pick(QUERIES),
                        &format!("{} [updated]", rand_pick(CONTENTS)),
                        "session-correction",
                        &correct,
                        &[200],
                    )
                    .await;
                }
                _ => {
                    c.list_memories(&list, &[200]).await;
                }
            }
            if fastrand::u32(0..3) == 0 {
                tokio::time::sleep(Duration::from_millis(fastrand::u64(200..800))).await;
            } else {
                tokio::time::sleep(Duration::from_millis(fastrand::u64(20..80))).await;
            }
        }

        c.purge("load-test working", "session end", &purge, &[200])
            .await;
        c.remember(
            "Session Summary: load test session completed",
            "episodic",
            &store,
        )
        .await;

        tokio::time::sleep(Duration::from_millis(fastrand::u64(500..2000))).await;
    }
    vec![retrieve, store, search, correct, list, purge]
}

pub(crate) async fn git_loop(c: &ApiClient, duration: Duration) -> Vec<Arc<Stats>> {
    let snapshot = Arc::new(Stats::new("snapshot"));
    let branch = Arc::new(Stats::new("branch"));
    let checkout = Arc::new(Stats::new("checkout"));
    let diff = Arc::new(Stats::new("diff"));
    let merge = Arc::new(Stats::new("merge"));
    let rollback = Arc::new(Stats::new("rollback"));
    let store = Arc::new(Stats::new("store"));
    let snapshots_list = Arc::new(Stats::new("snapshots_list"));
    let branch_delete = Arc::new(Stats::new("branch_delete"));

    let deadline = Instant::now() + duration;
    let mut iter = 0u32;
    while Instant::now() < deadline {
        iter += 1;
        let tag = fastrand::u32(10000..99999);
        let branch_name = format!("lt-{}-{iter}-{tag}", c.user_id);
        let snap_name = format!("lt-s-{}-{iter}-{tag}", c.user_id);

        c.post(
            "/v1/snapshots",
            serde_json::json!({"name": snap_name, "description": "pre-experiment"}),
            &snapshot,
            &[200, 201],
        )
        .await;

        c.post(
            "/v1/branches",
            serde_json::json!({"name": branch_name}),
            &branch,
            &[200, 201],
        )
        .await;

        c.post(
            &format!("/v1/branches/{branch_name}/checkout"),
            serde_json::json!({}),
            &checkout,
            &[200],
        )
        .await;

        for _ in 0..fastrand::usize(2..=5) {
            if Instant::now() >= deadline {
                break;
            }
            c.post(
                "/v1/memories",
                serde_json::json!({
                    "content": format!("Branch experiment: {}", rand_pick(CONTENTS)),
                    "memory_type": "semantic",
                }),
                &store,
                &[201],
            )
            .await;
            tokio::time::sleep(Duration::from_millis(fastrand::u64(50..200))).await;
        }

        c.get(&format!("/v1/branches/{branch_name}/diff"), &diff, &[200])
            .await;

        c.post(
            "/v1/branches/main/checkout",
            serde_json::json!({}),
            &checkout,
            &[200],
        )
        .await;

        if fastrand::u32(0..10) < 7 {
            c.post(
                &format!("/v1/branches/{branch_name}/merge"),
                serde_json::json!({}),
                &merge,
                &[200],
            )
            .await;
        }

        c.delete(
            &format!("/v1/branches/{branch_name}"),
            &branch_delete,
            &[200, 204],
        )
        .await;

        if fastrand::u32(0..10) == 0 {
            c.post(
                &format!("/v1/snapshots/{snap_name}/rollback"),
                serde_json::json!({}),
                &rollback,
                &[200, 404],
            )
            .await;
        }

        c.get("/v1/snapshots?limit=10", &snapshots_list, &[200])
            .await;

        tokio::time::sleep(Duration::from_millis(fastrand::u64(300..1000))).await;
    }
    vec![
        snapshot,
        branch,
        checkout,
        diff,
        merge,
        rollback,
        store,
        snapshots_list,
        branch_delete,
    ]
}

pub(crate) async fn maintenance_loop(c: &ApiClient, duration: Duration) -> Vec<Arc<Stats>> {
    match c.api_version {
        ApiVersion::V1 => {
            let governance = Arc::new(Stats::new("governance"));
            let consolidate = Arc::new(Stats::new("consolidate"));
            let reflect = Arc::new(Stats::new("reflect"));
            let metrics = Arc::new(Stats::new("metrics"));
            let profile = Arc::new(Stats::new("profile"));

            let deadline = Instant::now() + duration;
            while Instant::now() < deadline {
                match fastrand::u32(0..10) {
                    0..=2 => {
                        c.post("/v1/governance", serde_json::json!({}), &governance, &[200])
                            .await;
                    }
                    3..=4 => {
                        c.post(
                            "/v1/consolidate",
                            serde_json::json!({}),
                            &consolidate,
                            &[200],
                        )
                        .await;
                    }
                    5 => {
                        c.post("/v1/reflect", serde_json::json!({}), &reflect, &[200])
                            .await;
                    }
                    6..=8 => {
                        c.get("/metrics", &metrics, &[200]).await;
                    }
                    _ => {
                        c.profile(&profile, &[200]).await;
                    }
                }
                tokio::time::sleep(Duration::from_millis(fastrand::u64(1000..3000))).await;
            }
            vec![governance, consolidate, reflect, metrics, profile]
        }
        ApiVersion::V2 => v2_loadtest_api::maintenance_loop(c, duration).await,
    }
}

pub(crate) async fn burst_loop(c: &ApiClient, duration: Duration) -> Vec<Arc<Stats>> {
    let retrieve = Arc::new(Stats::new("retrieve"));
    let store = Arc::new(Stats::new("store"));
    let search = Arc::new(Stats::new("search"));

    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        let burst_size = fastrand::usize(5..=15);
        let mut futs = Vec::new();
        for _ in 0..burst_size {
            match fastrand::u32(0..3) {
                0 => {
                    let r = &retrieve;
                    futs.push(Box::pin(async move {
                        c.recall(rand_pick(QUERIES), 5, r, &[200]).await;
                    })
                        as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>);
                }
                1 => {
                    let s = &store;
                    futs.push(Box::pin(async move {
                        c.remember(rand_pick(CONTENTS), rand_pick(MEMORY_TYPES), s)
                            .await;
                    }));
                }
                _ => {
                    let s = &search;
                    futs.push(Box::pin(async move {
                        c.search(rand_pick(QUERIES), 10, s, &[200]).await;
                    }));
                }
            }
        }
        futures::future::join_all(futs).await;
        tokio::time::sleep(Duration::from_millis(fastrand::u64(2000..5000))).await;
    }
    vec![retrieve, store, search]
}
