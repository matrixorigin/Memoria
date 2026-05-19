//! Lightweight HTTP load-test for the Memoria API server.
//!
//! Not included in the release binary — built separately via:
//!   cargo run -p memoria-cli --bin loadtest
//!
//! Or via Makefile:
//!   make dev-bench

use clap::Parser;
use reqwest::Client;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

#[derive(Parser)]
#[command(name = "memoria-loadtest", about = "Load-test the Memoria API server")]
struct Args {
    #[arg(long, default_value = "http://127.0.0.1:8100")]
    api_url: String,
    #[arg(long, default_value = "test-master-key-for-docker-compose")]
    token: String,
    /// Seconds per scenario
    #[arg(long, default_value = "60")]
    duration: u64,
    /// Concurrent users
    #[arg(long, default_value = "10")]
    users: usize,
    /// Scenario: session, git, maintenance, burst, all
    #[arg(long, default_value = "all")]
    scenario: String,
    /// Skip preflight checks
    #[arg(long)]
    skip_preflight: bool,
    /// Seed memories per user before load test
    #[arg(long, default_value = "20")]
    seed: usize,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    run(&args).await
}

// ── Stats ────────────────────────────────────────────────────────────────────

struct Stats {
    name: String,
    ok: AtomicU64,
    err: AtomicU64,
    latencies: Mutex<Vec<f64>>,
    errors: Mutex<Vec<String>>,
}

impl Stats {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            ok: AtomicU64::new(0),
            err: AtomicU64::new(0),
            latencies: Mutex::new(Vec::new()),
            errors: Mutex::new(Vec::new()),
        }
    }

    async fn record(&self, ms: f64, status: u16, expected: &[u16]) {
        self.latencies.lock().await.push(ms);
        if expected.contains(&status) {
            self.ok.fetch_add(1, Relaxed);
        } else {
            self.err.fetch_add(1, Relaxed);
            let mut errs = self.errors.lock().await;
            if errs.len() < 5 {
                errs.push(format!("HTTP {status}"));
            }
        }
    }

    async fn report(&self) {
        let ok = self.ok.load(Relaxed);
        let err = self.err.load(Relaxed);
        let total = ok + err;
        if total == 0 { return; }
        let mut lat = self.latencies.lock().await.clone();
        lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p50 = lat[lat.len() / 2];
        let p95 = lat[(lat.len() as f64 * 0.95) as usize];
        let p99 = lat[(lat.len() as f64 * 0.99) as usize];
        let errs = self.errors.lock().await;
        let err_info = if errs.is_empty() {
            String::new()
        } else {
            format!("  errors: {:?}", &*errs)
        };
        println!(
            "  {:<30}  total={total:>5}  ok={ok:>5}  err={err:>3}  \
             p50={p50:>7.1}ms  p95={p95:>7.1}ms  p99={p99:>7.1}ms{err_info}",
            self.name
        );
    }
}

async fn merge_stats(merged: &mut BTreeMap<String, Arc<Stats>>, incoming: Vec<Arc<Stats>>) {
    for s in incoming {
        if let Some(existing) = merged.get(&s.name) {
            existing.ok.fetch_add(s.ok.load(Relaxed), Relaxed);
            existing.err.fetch_add(s.err.load(Relaxed), Relaxed);
            existing.latencies.lock().await.extend(s.latencies.lock().await.iter());
            let mut dst = existing.errors.lock().await;
            for e in s.errors.lock().await.iter() {
                if dst.len() < 5 { dst.push(e.clone()); }
            }
        } else {
            merged.insert(s.name.clone(), s);
        }
    }
}

// ── ApiClient ────────────────────────────────────────────────────────────────

fn rand_pick<'a>(items: &'a [&str]) -> &'a str {
    items[fastrand::usize(..items.len())]
}

const CONTENTS: &[&str] = &[
    "User prefers concise answers without markdown headers",
    "Project uses Go 1.22 with modules and MatrixOne as primary DB",
    "Deploy command: make build && kubectl apply -f deploy/",
    "Run tests with: cargo test --workspace",
    "API follows REST conventions, versioned under /v1/",
    "Prefers pytest over unittest for Python projects",
    "Uses ruff for Python formatting, replaces black",
    "OAuth token refresh needs 5-minute buffer before expiry",
    "Database migrations run automatically on startup via migrate()",
    "Embedding dimension is locked at schema creation time",
    "CI pipeline: lint → test → build → deploy to staging",
    "Use feature flags for gradual rollout of new features",
    "Database connection pool size should be 2x CPU cores",
    "Retry policy: 3 attempts with exponential backoff, max 30s",
    "Log format: structured JSON with trace_id for distributed tracing",
];

const QUERIES: &[&str] = &[
    "deployment command", "database configuration", "testing framework",
    "user preferences", "API conventions", "formatting tools",
    "authentication", "embedding setup", "CI pipeline",
    "retry policy", "connection pool", "feature flags",
];

const MEMORY_TYPES: &[&str] = &["semantic", "profile", "procedural", "working", "episodic"];

struct ApiClient {
    client: Arc<Client>,
    base: String,
    token: String,
    user_id: String,
}

impl ApiClient {
    fn new(base: &str, token: &str, user_id: &str, client: Arc<Client>) -> Self {
        Self {
            client,
            base: base.to_string(),
            token: token.to_string(),
            user_id: user_id.to_string(),
        }
    }

    async fn post(&self, path: &str, body: serde_json::Value, stats: &Stats,
                  expected: &[u16]) -> (u16, Option<serde_json::Value>) {
        let t0 = Instant::now();
        let res = self.client.post(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .header("X-Impersonate-User", &self.user_id)
            .json(&body).send().await;
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        match res {
            Ok(r) => {
                let status = r.status().as_u16();
                let data = r.json().await.ok();
                stats.record(ms, status, expected).await;
                (status, data)
            }
            Err(e) => {
                stats.record(ms, 0, expected).await;
                let mut errs = stats.errors.lock().await;
                if errs.len() < 5 { errs.push(format!("{e}")); }
                (0, None)
            }
        }
    }

    async fn get(&self, path: &str, stats: &Stats,
                 expected: &[u16]) -> (u16, Option<serde_json::Value>) {
        let t0 = Instant::now();
        let res = self.client.get(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .header("X-Impersonate-User", &self.user_id)
            .send().await;
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        match res {
            Ok(r) => {
                let status = r.status().as_u16();
                let data = r.json().await.ok();
                stats.record(ms, status, expected).await;
                (status, data)
            }
            Err(e) => {
                stats.record(ms, 0, expected).await;
                let mut errs = stats.errors.lock().await;
                if errs.len() < 5 { errs.push(format!("{e}")); }
                (0, None)
            }
        }
    }

    async fn delete(&self, path: &str, stats: &Stats, expected: &[u16]) -> u16 {
        let t0 = Instant::now();
        let res = self.client.delete(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .header("X-Impersonate-User", &self.user_id)
            .send().await;
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        match res {
            Ok(r) => { let s = r.status().as_u16(); stats.record(ms, s, expected).await; s }
            Err(e) => {
                stats.record(ms, 0, expected).await;
                let mut errs = stats.errors.lock().await;
                if errs.len() < 5 { errs.push(format!("{e}")); }
                0
            }
        }
    }
}

// ── Scenario 1: MCP Session (realistic agent conversation) ──────────────────
//
// Simulates: retrieve → store/correct/search interleaved → purge working → store episodic
// This is what actually happens when an AI agent uses Memoria.

async fn session_loop(c: &ApiClient, duration: Duration) -> Vec<Arc<Stats>> {
    let retrieve = Arc::new(Stats::new("retrieve"));
    let store = Arc::new(Stats::new("store"));
    let search = Arc::new(Stats::new("search"));
    let correct = Arc::new(Stats::new("correct"));
    let list = Arc::new(Stats::new("list"));
    let purge = Arc::new(Stats::new("purge"));

    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        // Phase 1: bootstrap — retrieve context (agent always does this first)
        c.post("/v1/memories/retrieve",
            serde_json::json!({"query": rand_pick(QUERIES), "top_k": 5}),
            &retrieve, &[200]).await;

        // small think pause
        tokio::time::sleep(Duration::from_millis(fastrand::u64(100..500))).await;
        if Instant::now() >= deadline { break; }

        // Phase 2: mid-session — 3-8 operations (store, search, correct, list)
        let mid_ops = fastrand::usize(3..=8);
        for _ in 0..mid_ops {
            if Instant::now() >= deadline { break; }
            match fastrand::u32(0..10) {
                0..=3 => {
                    // store a working or semantic memory
                    let mtype = if fastrand::bool() { "working" } else { "semantic" };
                    c.post("/v1/memories",
                        serde_json::json!({
                            "content": rand_pick(CONTENTS),
                            "memory_type": mtype,
                        }),
                        &store, &[201]).await;
                }
                4..=5 => {
                    c.post("/v1/memories/search",
                        serde_json::json!({"query": rand_pick(QUERIES), "top_k": 10}),
                        &search, &[200]).await;
                }
                6..=7 => {
                    c.post("/v1/memories/retrieve",
                        serde_json::json!({"query": rand_pick(QUERIES), "top_k": 5}),
                        &retrieve, &[200]).await;
                }
                8 => {
                    c.post("/v1/memories/correct",
                        serde_json::json!({
                            "query": rand_pick(QUERIES),
                            "new_content": format!("{} [updated]", rand_pick(CONTENTS)),
                            "reason": "session-correction",
                        }),
                        &correct, &[200]).await;
                }
                _ => {
                    c.get("/v1/memories", &list, &[200]).await;
                }
            }
            // burst: agent sends a few requests quickly, then pauses to "think"
            if fastrand::u32(0..3) == 0 {
                tokio::time::sleep(Duration::from_millis(fastrand::u64(200..800))).await;
            } else {
                tokio::time::sleep(Duration::from_millis(fastrand::u64(20..80))).await;
            }
        }

        // Phase 3: wrap-up — purge working memories, store episodic summary
        c.post("/v1/memories/purge",
            serde_json::json!({"topic": "load-test working", "reason": "session end"}),
            &purge, &[200]).await;

        c.post("/v1/memories",
            serde_json::json!({
                "content": "Session Summary: load test session completed",
                "memory_type": "episodic",
            }),
            &store, &[201]).await;

        // gap between sessions
        tokio::time::sleep(Duration::from_millis(fastrand::u64(500..2000))).await;
    }
    vec![retrieve, store, search, correct, list, purge]
}

// ── Scenario 2: Git-for-Data (snapshot/branch/merge) ────────────────────────
//
// Simulates branch-based experimentation — the core differentiator of Memoria.

async fn git_loop(c: &ApiClient, duration: Duration) -> Vec<Arc<Stats>> {
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

        // create safety snapshot
        c.post("/v1/snapshots",
            serde_json::json!({"name": snap_name, "description": "pre-experiment"}),
            &snapshot, &[200, 201]).await;

        // create branch
        c.post("/v1/branches",
            serde_json::json!({"name": branch_name}),
            &branch, &[200, 201]).await;

        // checkout branch
        c.post(&format!("/v1/branches/{branch_name}/checkout"),
            serde_json::json!({}),
            &checkout, &[200]).await;

        // store a few memories on branch
        for _ in 0..fastrand::usize(2..=5) {
            if Instant::now() >= deadline { break; }
            c.post("/v1/memories",
                serde_json::json!({
                    "content": format!("Branch experiment: {}", rand_pick(CONTENTS)),
                    "memory_type": "semantic",
                }),
                &store, &[201]).await;
            tokio::time::sleep(Duration::from_millis(fastrand::u64(50..200))).await;
        }

        // diff (GET)
        c.get(&format!("/v1/branches/{branch_name}/diff"),
            &diff, &[200]).await;

        // checkout main
        c.post("/v1/branches/main/checkout",
            serde_json::json!({}),
            &checkout, &[200]).await;

        // 70% merge, 30% abandon
        if fastrand::u32(0..10) < 7 {
            c.post(&format!("/v1/branches/{branch_name}/merge"),
                serde_json::json!({}),
                &merge, &[200]).await;
        }

        // cleanup branch
        c.delete(&format!("/v1/branches/{branch_name}"), &branch_delete, &[200, 204]).await;

        // occasionally rollback to snapshot (10%)
        if fastrand::u32(0..10) == 0 {
            c.post(&format!("/v1/snapshots/{snap_name}/rollback"),
                serde_json::json!({}),
                &rollback, &[200, 404]).await;
        }

        // list snapshots
        c.get("/v1/snapshots?limit=10", &snapshots_list, &[200]).await;

        tokio::time::sleep(Duration::from_millis(fastrand::u64(300..1000))).await;
    }
    vec![snapshot, branch, checkout, diff, merge, rollback, store, snapshots_list, branch_delete]
}

// ── Scenario 3: Maintenance (governance/consolidate/reflect + metrics) ──────
//
// These do full-table scans and are the most likely to cause blocking.

async fn maintenance_loop(c: &ApiClient, duration: Duration) -> Vec<Arc<Stats>> {
    let governance = Arc::new(Stats::new("governance"));
    let consolidate = Arc::new(Stats::new("consolidate"));
    let reflect = Arc::new(Stats::new("reflect"));
    let metrics = Arc::new(Stats::new("metrics"));
    let profile = Arc::new(Stats::new("profile"));

    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        match fastrand::u32(0..10) {
            0..=2 => {
                c.post("/v1/governance", serde_json::json!({}),
                    &governance, &[200]).await;
            }
            3..=4 => {
                c.post("/v1/consolidate", serde_json::json!({}),
                    &consolidate, &[200]).await;
            }
            5 => {
                c.post("/v1/reflect", serde_json::json!({}),
                    &reflect, &[200]).await;
            }
            6..=8 => {
                c.get("/metrics", &metrics, &[200]).await;
            }
            _ => {
                c.get(&format!("/v1/profiles/{}", c.user_id), &profile, &[200]).await;
            }
        }
        // these are heavy — longer pauses
        tokio::time::sleep(Duration::from_millis(fastrand::u64(1000..3000))).await;
    }
    vec![governance, consolidate, reflect, metrics, profile]
}

// ── Scenario 4: Burst (sudden spike of concurrent requests) ─────────────────
//
// Simulates multiple agents waking up simultaneously after idle period.

async fn burst_loop(c: &ApiClient, duration: Duration) -> Vec<Arc<Stats>> {
    let retrieve = Arc::new(Stats::new("retrieve"));
    let store = Arc::new(Stats::new("store"));
    let search = Arc::new(Stats::new("search"));

    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        // burst: fire 5-15 requests as fast as possible
        let burst_size = fastrand::usize(5..=15);
        let mut futs = Vec::new();
        for _ in 0..burst_size {
            match fastrand::u32(0..3) {
                0 => {
                    let r = &retrieve;
                    futs.push(Box::pin(async move {
                        c.post("/v1/memories/retrieve",
                            serde_json::json!({"query": rand_pick(QUERIES), "top_k": 5}),
                            r, &[200]).await;
                    }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>);
                }
                1 => {
                    let s = &store;
                    futs.push(Box::pin(async move {
                        c.post("/v1/memories",
                            serde_json::json!({
                                "content": rand_pick(CONTENTS),
                                "memory_type": rand_pick(MEMORY_TYPES),
                            }),
                            s, &[201]).await;
                    }));
                }
                _ => {
                    let s = &search;
                    futs.push(Box::pin(async move {
                        c.post("/v1/memories/search",
                            serde_json::json!({"query": rand_pick(QUERIES), "top_k": 10}),
                            s, &[200]).await;
                    }));
                }
            }
        }
        futures::future::join_all(futs).await;

        // long idle between bursts (simulates agent thinking)
        tokio::time::sleep(Duration::from_millis(fastrand::u64(2000..5000))).await;
    }
    vec![retrieve, store, search]
}

// ── Seed data ────────────────────────────────────────────────────────────────

async fn seed_user(c: &ApiClient, count: usize) {
    let dummy = Stats::new("_seed");
    for i in 0..count {
        let mtype = MEMORY_TYPES[i % MEMORY_TYPES.len()];
        c.post("/v1/memories",
            serde_json::json!({
                "content": format!("{} (seed #{})", CONTENTS[i % CONTENTS.len()], i),
                "memory_type": mtype,
            }),
            &dummy, &[201]).await;
    }
}

// ── Runner ───────────────────────────────────────────────────────────────────

fn print_header(label: &str, users: usize, duration: Duration) {
    println!(
        "\n{}\n[{label}] — {users} users × {:.0}s\n{}",
        "=".repeat(70), duration.as_secs_f64(), "=".repeat(70),
    );
}

fn shared_client() -> Arc<Client> {
    Arc::new(Client::builder()
        .pool_max_idle_per_host(32)
        .timeout(Duration::from_secs(30))
        .no_proxy()
        .build()
        .unwrap())
}

async fn run_scenario<F, Fut>(
    label: &str, user_ids: Vec<String>, duration: Duration,
    base: &str, token: &str, seed: usize, user_fn: F,
) where
    F: Fn(Arc<ApiClient>, Duration) -> Fut + Send + Sync + Clone + 'static,
    Fut: std::future::Future<Output = Vec<Arc<Stats>>> + Send,
{
    print_header(label, user_ids.len(), duration);
    let client = shared_client();

    // seed data
    if seed > 0 {
        print!("  Seeding {seed} memories per user...");
        let mut seed_handles = Vec::new();
        for uid in &user_ids {
            let c = Arc::new(ApiClient::new(base, token, uid, client.clone()));
            let n = seed;
            seed_handles.push(tokio::spawn(async move { seed_user(&c, n).await }));
        }
        for h in seed_handles { let _ = h.await; }
        println!(" done");
    }

    let mut handles = Vec::new();
    for uid in user_ids {
        let c = Arc::new(ApiClient::new(base, token, &uid, client.clone()));
        let f = user_fn.clone();
        let d = duration;
        handles.push(tokio::spawn(async move { f(c, d).await }));
    }

    let mut merged: BTreeMap<String, Arc<Stats>> = BTreeMap::new();
    for h in handles {
        if let Ok(stats_vec) = h.await {
            merge_stats(&mut merged, stats_vec).await;
        }
    }
    for s in merged.values() { s.report().await; }
}

// ── Preflight ────────────────────────────────────────────────────────────────

async fn preflight(base: &str, token: &str) -> bool {
    println!("Preflight checks:");
    let client = shared_client();
    let c = ApiClient::new(base, token, "lt-preflight", client.clone());
    let d = Stats::new("_");
    let mut ok = true;

    match client.get(format!("{base}/health")).send().await {
        Ok(r) if r.status().as_u16() == 200 => println!("  ✓ health: 200"),
        Ok(r) => { println!("  ✗ health: {}", r.status()); return false; }
        Err(e) => { println!("  ✗ health: {e}"); return false; }
    }

    let checks: Vec<(&str, u16)> = vec![
        ("store",      { let (s,_) = c.post("/v1/memories", serde_json::json!({"content":"preflight","memory_type":"semantic"}), &d, &[201]).await; s }),
        ("retrieve",   { let (s,_) = c.post("/v1/memories/retrieve", serde_json::json!({"query":"preflight","top_k":5}), &d, &[200]).await; s }),
        ("search",     { let (s,_) = c.post("/v1/memories/search", serde_json::json!({"query":"preflight","top_k":5}), &d, &[200]).await; s }),
        ("correct",    { let (s,_) = c.post("/v1/memories/correct", serde_json::json!({"query":"preflight","new_content":"updated","reason":"preflight"}), &d, &[200]).await; s }),
        ("list",       { let (s,_) = c.get("/v1/memories", &d, &[200]).await; s }),
        ("purge",      { let (s,_) = c.post("/v1/memories/purge", serde_json::json!({"topic":"preflight","reason":"cleanup"}), &d, &[200]).await; s }),
        ("create_key", { let (s,_) = c.post("/auth/keys", serde_json::json!({"user_id":"lt-preflight","name":"preflight-key"}), &d, &[201]).await; s }),
        ("list_keys",  { let (s,_) = c.get("/auth/keys", &d, &[200]).await; s }),
        ("metrics",    { let (s,_) = c.get("/metrics", &d, &[200]).await; s }),
    ];

    for (name, status) in &checks {
        if *status == 200 || *status == 201 {
            println!("  ✓ {name}: {status}");
        } else {
            println!("  ✗ {name}: {status}");
            ok = false;
        }
    }
    if ok { println!("\nAll preflight checks passed."); }
    else  { println!("\nPreflight FAILED — fix errors above."); }
    ok
}

// ── Main ─────────────────────────────────────────────────────────────────────

async fn run(args: &Args) -> anyhow::Result<()> {
    let base = args.api_url.trim_end_matches('/');
    let dur = Duration::from_secs(args.duration);
    let token = &args.token;
    let n = args.users;
    let seed = args.seed;

    if !args.skip_preflight && !preflight(base, token).await {
        anyhow::bail!("preflight failed");
    }

    let s = args.scenario.as_str();

    if s == "session" || s == "all" {
        let ids: Vec<String> = (0..n).map(|i| format!("lt-sess-{i}")).collect();
        run_scenario("session (MCP agent conversation)", ids, dur, base, token, seed,
            |c, d| async move { session_loop(&c, d).await }).await;
    }

    if s == "git" || s == "all" {
        let git_n = (n / 3).max(1);
        let ids: Vec<String> = (0..git_n).map(|i| format!("lt-git-{i}")).collect();
        run_scenario("git-for-data (snapshot/branch/merge)", ids, dur, base, token, seed,
            |c, d| async move { git_loop(&c, d).await }).await;
    }

    if s == "maintenance" || s == "all" {
        let maint_n = (n / 5).max(1);
        let ids: Vec<String> = (0..maint_n).map(|i| format!("lt-maint-{i}")).collect();
        run_scenario("maintenance (governance/consolidate/reflect)", ids, dur, base, token, 0,
            |c, d| async move { maintenance_loop(&c, d).await }).await;
    }

    if s == "burst" || s == "all" {
        let ids: Vec<String> = (0..n).map(|i| format!("lt-burst-{i}")).collect();
        run_scenario("burst (concurrent spike)", ids, dur, base, token, seed,
            |c, d| async move { burst_loop(&c, d).await }).await;
    }

    Ok(())
}
