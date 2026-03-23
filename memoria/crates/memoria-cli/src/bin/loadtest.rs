//! Lightweight HTTP load-test for the Memoria API server.
//!
//! Not included in the release binary — built separately via:
//!   cargo run -p memoria-cli --bin loadtest
//!
//! Or via Makefile:
//!   make dev-bench

#[path = "support/loadtest_runtime.rs"]
mod loadtest_runtime;
#[path = "support/loadtest_scenarios.rs"]
mod loadtest_scenarios;
#[path = "v1/loadtest_api.rs"]
mod v1_loadtest_api;
#[path = "v2/loadtest_api.rs"]
mod v2_loadtest_api;

use clap::{Parser, ValueEnum};
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
    /// API version: v1 or v2
    #[arg(long, value_enum, default_value_t = ApiVersion::V1)]
    api_version: ApiVersion,
    /// Skip preflight checks
    #[arg(long)]
    skip_preflight: bool,
    /// Seed memories per user before load test
    #[arg(long, default_value = "20")]
    seed: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ApiVersion {
    V1,
    V2,
}

impl ApiVersion {
    fn as_str(self) -> &'static str {
        match self {
            Self::V1 => "v1",
            Self::V2 => "v2",
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    loadtest_runtime::run(&args).await
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
        if total == 0 {
            return;
        }
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
            existing
                .latencies
                .lock()
                .await
                .extend(s.latencies.lock().await.iter());
            let mut dst = existing.errors.lock().await;
            for e in s.errors.lock().await.iter() {
                if dst.len() < 5 {
                    dst.push(e.clone());
                }
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
    "deployment command",
    "database configuration",
    "testing framework",
    "user preferences",
    "API conventions",
    "formatting tools",
    "authentication",
    "embedding setup",
    "CI pipeline",
    "retry policy",
    "connection pool",
    "feature flags",
];

const MEMORY_TYPES: &[&str] = &["semantic", "profile", "procedural", "working", "episodic"];

struct ApiClient {
    client: Arc<Client>,
    base: String,
    token: String,
    user_id: String,
    api_version: ApiVersion,
}

impl ApiClient {
    fn new(
        base: &str,
        token: &str,
        user_id: &str,
        api_version: ApiVersion,
        client: Arc<Client>,
    ) -> Self {
        Self {
            client,
            base: base.to_string(),
            token: token.to_string(),
            user_id: user_id.to_string(),
            api_version,
        }
    }

    pub(crate) fn session_id(&self) -> String {
        format!("lt-{}", self.user_id)
    }

    pub(crate) async fn post_raw(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> (u16, Option<serde_json::Value>) {
        match self
            .client
            .post(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .header("X-Impersonate-User", &self.user_id)
            .json(&body)
            .send()
            .await
        {
            Ok(r) => (r.status().as_u16(), r.json().await.ok()),
            Err(_) => (0, None),
        }
    }

    pub(crate) async fn patch_raw(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> (u16, Option<serde_json::Value>) {
        match self
            .client
            .patch(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .header("X-Impersonate-User", &self.user_id)
            .json(&body)
            .send()
            .await
        {
            Ok(r) => (r.status().as_u16(), r.json().await.ok()),
            Err(_) => (0, None),
        }
    }

    pub(crate) async fn get_raw(&self, path: &str) -> (u16, Option<serde_json::Value>) {
        match self
            .client
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .header("X-Impersonate-User", &self.user_id)
            .send()
            .await
        {
            Ok(r) => (r.status().as_u16(), r.json().await.ok()),
            Err(_) => (0, None),
        }
    }

    pub(crate) async fn record_compound(
        &self,
        stats: &Stats,
        expected: &[u16],
        start: Instant,
        status: u16,
    ) -> u16 {
        stats
            .record(start.elapsed().as_secs_f64() * 1000.0, status, expected)
            .await;
        status
    }

    async fn post(
        &self,
        path: &str,
        body: serde_json::Value,
        stats: &Stats,
        expected: &[u16],
    ) -> (u16, Option<serde_json::Value>) {
        let t0 = Instant::now();
        let (status, data) = self.post_raw(path, body).await;
        stats
            .record(t0.elapsed().as_secs_f64() * 1000.0, status, expected)
            .await;
        (status, data)
    }

    async fn get(
        &self,
        path: &str,
        stats: &Stats,
        expected: &[u16],
    ) -> (u16, Option<serde_json::Value>) {
        let t0 = Instant::now();
        let (status, data) = self.get_raw(path).await;
        stats
            .record(t0.elapsed().as_secs_f64() * 1000.0, status, expected)
            .await;
        (status, data)
    }

    async fn delete(&self, path: &str, stats: &Stats, expected: &[u16]) -> u16 {
        let t0 = Instant::now();
        let res = self
            .client
            .delete(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .header("X-Impersonate-User", &self.user_id)
            .send()
            .await;
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        match res {
            Ok(r) => {
                let s = r.status().as_u16();
                stats.record(ms, s, expected).await;
                s
            }
            Err(e) => {
                stats.record(ms, 0, expected).await;
                let mut errs = stats.errors.lock().await;
                if errs.len() < 5 {
                    errs.push(format!("{e}"));
                }
                0
            }
        }
    }

    async fn remember(&self, content: &str, memory_type: &str, stats: &Stats) -> Option<String> {
        match self.api_version {
            ApiVersion::V1 => v1_loadtest_api::remember(self, content, memory_type, stats).await,
            ApiVersion::V2 => v2_loadtest_api::remember(self, content, memory_type, stats).await,
        }
    }

    async fn recall(
        &self,
        query: &str,
        top_k: i64,
        stats: &Stats,
        expected: &[u16],
    ) -> (u16, Option<serde_json::Value>) {
        match self.api_version {
            ApiVersion::V1 => v1_loadtest_api::recall(self, query, top_k, stats, expected).await,
            ApiVersion::V2 => v2_loadtest_api::recall(self, query, top_k, stats, expected).await,
        }
    }

    async fn search(&self, query: &str, top_k: i64, stats: &Stats, expected: &[u16]) -> u16 {
        match self.api_version {
            ApiVersion::V1 => v1_loadtest_api::search(self, query, top_k, stats, expected).await,
            ApiVersion::V2 => self.recall(query, top_k, stats, expected).await.0,
        }
    }

    async fn list_memories(&self, stats: &Stats, expected: &[u16]) -> u16 {
        match self.api_version {
            ApiVersion::V1 => v1_loadtest_api::list_memories(self, stats, expected).await,
            ApiVersion::V2 => v2_loadtest_api::list_memories(self, stats, expected).await,
        }
    }

    async fn profile(&self, stats: &Stats, expected: &[u16]) -> u16 {
        match self.api_version {
            ApiVersion::V1 => v1_loadtest_api::profile(self, stats, expected).await,
            ApiVersion::V2 => v2_loadtest_api::profile(self, stats, expected).await,
        }
    }

    async fn correct(
        &self,
        query: &str,
        new_content: &str,
        reason: &str,
        stats: &Stats,
        expected: &[u16],
    ) -> u16 {
        match self.api_version {
            ApiVersion::V1 => {
                v1_loadtest_api::correct(self, query, new_content, reason, stats, expected).await
            }
            ApiVersion::V2 => {
                v2_loadtest_api::correct(self, query, new_content, reason, stats, expected).await
            }
        }
    }

    async fn purge(&self, query: &str, reason: &str, stats: &Stats, expected: &[u16]) -> u16 {
        match self.api_version {
            ApiVersion::V1 => v1_loadtest_api::purge(self, query, reason, stats, expected).await,
            ApiVersion::V2 => v2_loadtest_api::purge(self, query, reason, stats, expected).await,
        }
    }
}

// ── Seed data ────────────────────────────────────────────────────────────────

async fn seed_user(c: &ApiClient, count: usize) {
    let dummy = Stats::new("_seed");
    for i in 0..count {
        let mtype = MEMORY_TYPES[i % MEMORY_TYPES.len()];
        c.remember(
            &format!("{} (seed #{})", CONTENTS[i % CONTENTS.len()], i),
            mtype,
            &dummy,
        )
        .await;
    }
}

// ── Runner ───────────────────────────────────────────────────────────────────

fn print_header(label: &str, users: usize, duration: Duration) {
    println!(
        "\n{}\n[{label}] — {users} users × {:.0}s\n{}",
        "=".repeat(70),
        duration.as_secs_f64(),
        "=".repeat(70),
    );
}

fn shared_client() -> Arc<Client> {
    Arc::new(
        Client::builder()
            .pool_max_idle_per_host(32)
            .timeout(Duration::from_secs(30))
            .no_proxy()
            .build()
            .unwrap(),
    )
}

#[allow(clippy::too_many_arguments)]
async fn run_scenario<F, Fut>(
    label: &str,
    user_ids: Vec<String>,
    duration: Duration,
    base: &str,
    token: &str,
    api_version: ApiVersion,
    seed: usize,
    user_fn: F,
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
            let c = Arc::new(ApiClient::new(
                base,
                token,
                uid,
                api_version,
                client.clone(),
            ));
            let n = seed;
            seed_handles.push(tokio::spawn(async move { seed_user(&c, n).await }));
        }
        for h in seed_handles {
            let _ = h.await;
        }
        println!(" done");
    }

    let mut handles = Vec::new();
    for uid in user_ids {
        let c = Arc::new(ApiClient::new(
            base,
            token,
            &uid,
            api_version,
            client.clone(),
        ));
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
    for s in merged.values() {
        s.report().await;
    }
}

// ── Preflight ────────────────────────────────────────────────────────────────
