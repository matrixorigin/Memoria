use super::{
    loadtest_scenarios, run_scenario, shared_client, v2_loadtest_api, ApiClient, ApiVersion, Args,
    Stats,
};
use std::time::Duration;

pub(crate) async fn preflight(base: &str, token: &str, api_version: ApiVersion) -> bool {
    println!("Preflight checks:");
    let client = shared_client();
    let c = ApiClient::new(base, token, "lt-preflight", api_version, client.clone());
    let d = Stats::new("_");
    let mut ok = true;

    match client.get(format!("{base}/health")).send().await {
        Ok(r) if r.status().as_u16() == 200 => println!("  ✓ health: 200"),
        Ok(r) => {
            println!("  ✗ health: {}", r.status());
            return false;
        }
        Err(e) => {
            println!("  ✗ health: {e}");
            return false;
        }
    }

    let store_id = c.remember("preflight", "semantic", &d).await;
    let retrieve_status = c.recall("preflight", 5, &d, &[200]).await.0;
    let search_status = c.search("preflight", 5, &d, &[200]).await;
    let correct_status = c
        .correct("preflight", "updated", "preflight", &d, &[200])
        .await;
    let list_status = c.list_memories(&d, &[200]).await;
    let purge_status = c.purge("updated", "cleanup", &d, &[200]).await;
    let profile_status = c.profile(&d, &[200]).await;
    let create_key_status = c
        .post(
            "/auth/keys",
            serde_json::json!({"user_id":"lt-preflight","name":"preflight-key"}),
            &d,
            &[201],
        )
        .await
        .0;
    let list_keys_status = c.get("/auth/keys", &d, &[200]).await.0;
    let metrics_status = c.get("/metrics", &d, &[200]).await.0;
    let extra_status = match api_version {
        ApiVersion::V1 => {
            c.post("/v1/governance", serde_json::json!({}), &d, &[200])
                .await
                .0
        }
        ApiVersion::V2 => v2_loadtest_api::preflight_extra(base, token, client.clone()).await,
    };
    let checks: Vec<(&str, u16)> = vec![
        ("store", if store_id.is_some() { 201 } else { 0 }),
        ("retrieve", retrieve_status),
        ("search", search_status),
        ("correct", correct_status),
        ("list", list_status),
        ("purge", purge_status),
        ("profile", profile_status),
        (
            match api_version {
                ApiVersion::V1 => "governance",
                ApiVersion::V2 => "stats",
            },
            extra_status,
        ),
        ("create_key", create_key_status),
        ("list_keys", list_keys_status),
        ("metrics", metrics_status),
    ];

    for (name, status) in &checks {
        if *status == 200 || *status == 201 {
            println!("  ✓ {name}: {status}");
        } else {
            println!("  ✗ {name}: {status}");
            ok = false;
        }
    }
    if ok {
        println!("\nAll preflight checks passed.");
    } else {
        println!("\nPreflight FAILED — fix errors above.");
    }
    ok
}

pub(crate) async fn run(args: &Args) -> anyhow::Result<()> {
    let base = args.api_url.trim_end_matches('/');
    let dur = Duration::from_secs(args.duration);
    let token = &args.token;
    let n = args.users;
    let seed = args.seed;
    let api_version = args.api_version;

    if !args.skip_preflight && !preflight(base, token, api_version).await {
        anyhow::bail!("preflight failed");
    }

    let s = args.scenario.as_str();

    if s == "session" || s == "all" {
        let ids: Vec<String> = (0..n).map(|i| format!("lt-sess-{i}")).collect();
        run_scenario(
            &format!(
                "session [{}] (MCP agent conversation)",
                api_version.as_str()
            ),
            ids,
            dur,
            base,
            token,
            api_version,
            seed,
            |c, d| async move { loadtest_scenarios::session_loop(&c, d).await },
        )
        .await;
    }

    if matches!(api_version, ApiVersion::V1) && (s == "git" || s == "all") {
        let git_n = (n / 3).max(1);
        let ids: Vec<String> = (0..git_n).map(|i| format!("lt-git-{i}")).collect();
        run_scenario(
            "git-for-data (snapshot/branch/merge)",
            ids,
            dur,
            base,
            token,
            api_version,
            seed,
            |c, d| async move { loadtest_scenarios::git_loop(&c, d).await },
        )
        .await;
    } else if matches!(api_version, ApiVersion::V2) && s == "git" {
        anyhow::bail!(
            "git scenario is V1-only; V2 loadtest supports session, maintenance, burst, and all"
        );
    } else if matches!(api_version, ApiVersion::V2) && s == "all" {
        println!("Skipping git scenario for v2 loadtest: no V2 branch/snapshot API");
    }

    if s == "maintenance" || s == "all" {
        let maint_n = (n / 5).max(1);
        let ids: Vec<String> = (0..maint_n).map(|i| format!("lt-maint-{i}")).collect();
        run_scenario(
            &format!("maintenance [{}]", api_version.as_str()),
            ids,
            dur,
            base,
            token,
            api_version,
            0,
            |c, d| async move { loadtest_scenarios::maintenance_loop(&c, d).await },
        )
        .await;
    }

    if s == "burst" || s == "all" {
        let ids: Vec<String> = (0..n).map(|i| format!("lt-burst-{i}")).collect();
        run_scenario(
            &format!("burst [{}] (concurrent spike)", api_version.as_str()),
            ids,
            dur,
            base,
            token,
            api_version,
            seed,
            |c, d| async move { loadtest_scenarios::burst_loop(&c, d).await },
        )
        .await;
    }

    Ok(())
}
