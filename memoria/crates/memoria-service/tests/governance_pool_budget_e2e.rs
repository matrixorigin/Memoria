use std::{
    future::Future,
    sync::{Mutex, OnceLock},
};

use memoria_service::{Config, GovernanceScheduler};

async fn with_env_async<F, Fut, T>(vars: &[(&str, Option<&str>)], f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = T>,
{
    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    let _lock = ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    struct EnvGuard(Vec<(String, Option<std::ffi::OsString>)>);

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, old) in &self.0 {
                match old {
                    Some(value) => unsafe { std::env::set_var(key, value) },
                    None => unsafe { std::env::remove_var(key) },
                }
            }
        }
    }

    let _restore = EnvGuard(
        vars.iter()
            .map(|(key, value)| {
                let old = std::env::var_os(key);
                match value {
                    Some(value) => unsafe { std::env::set_var(key, value) },
                    None => unsafe { std::env::remove_var(key) },
                }
                (key.to_string(), old)
            })
            .collect(),
    );

    f().await
}

fn base_governance_envs<'a>(
    budget: Option<&'a str>,
    governance_pool: Option<&'a str>,
) -> [(&'a str, Option<&'a str>); 10] {
    [
        ("MEMORIA_MULTI_DB_POOL_BUDGET", budget),
        ("GOVERNANCE_POOL_SIZE", governance_pool),
        ("MEMORIA_GOVERNANCE_ENABLED", Some("true")),
        ("MEMORIA_SHARED_POOL_MAX_CONNECTIONS", Some("")),
        ("MEMORIA_SHARED_MAIN_POOL_MAX_CONNECTIONS", Some("")),
        ("MEMORIA_GIT_POOL_MAX_CONNECTIONS", Some("")),
        ("MEMORIA_MERGED_SHARED_POOL_MAX_CONNECTIONS", Some("")),
        ("MEMORIA_GLOBAL_USER_POOL_MAX", Some("")),
        ("MEMORIA_USER_SCHEMA_INIT_POOL_MAX_CONNECTIONS", Some("")),
        ("MEMORIA_AUTH_POOL_MAX_CONNECTIONS", Some("")),
    ]
}

fn db_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria".to_string())
}

async fn assert_governance_pool_case(
    case_name: &str,
    vars: &[(&str, Option<&str>)],
    expected: Option<u32>,
) {
    with_env_async(vars, || async {
        let ctx = memoria_test_utils::MultiDbTestContext::new(
            &db_url(),
            "governance_pool_budget_e2e",
            1024,
            None,
            None,
        )
        .await;
        let scheduler = GovernanceScheduler::from_config(ctx.service(), &Config::from_env())
            .await
            .expect("build governance scheduler");

        assert_eq!(
            scheduler.isolated_pool_max_connections(),
            expected,
            "{case_name}: isolated governance pool size"
        );
        if let Some(expected) = expected {
            assert_eq!(
                scheduler.sql_store_configured_max_connections(),
                Some(expected),
                "{case_name}: isolated governance store configured max"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn test_governance_pool_budget_matrix() {
    assert_governance_pool_case(
        "default budget",
        &base_governance_envs(Some(""), Some("")),
        Some(26),
    )
    .await;

    assert_governance_pool_case(
        "low budget floor",
        &base_governance_envs(Some("0"), Some("")),
        Some(10),
    )
    .await;

    assert_governance_pool_case(
        "configured budget",
        &base_governance_envs(Some("1024"), Some("")),
        Some(51),
    )
    .await;

    assert_governance_pool_case(
        "explicit override",
        &base_governance_envs(Some("1024"), Some("77")),
        Some(77),
    )
    .await;

    assert_governance_pool_case(
        "override clamps to cap",
        &base_governance_envs(Some("4096"), Some("99999")),
        Some(102),
    )
    .await;

    assert_governance_pool_case(
        "zero disables isolated pool",
        &base_governance_envs(Some("1024"), Some("0")),
        None,
    )
    .await;
}
