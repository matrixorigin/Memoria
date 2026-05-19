mod support;

use std::{
    future::Future,
    sync::{Mutex, OnceLock},
};

use support::multi_db::{spawn_api_server, ApiTestServer};

#[derive(Debug, Clone, Copy)]
struct ExpectedPools {
    shared: u32,
    global: u32,
    user_init: u32,
    auth: u32,
}

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

fn budget_envs<'a>(budget: Option<&'a str>) -> [(&'a str, Option<&'a str>); 8] {
    [
        ("MEMORIA_MULTI_DB_POOL_BUDGET", budget),
        ("MEMORIA_SHARED_POOL_MAX_CONNECTIONS", Some("")),
        ("MEMORIA_SHARED_MAIN_POOL_MAX_CONNECTIONS", Some("")),
        ("MEMORIA_GIT_POOL_MAX_CONNECTIONS", Some("")),
        ("MEMORIA_MERGED_SHARED_POOL_MAX_CONNECTIONS", Some("")),
        ("MEMORIA_GLOBAL_USER_POOL_MAX", Some("")),
        ("MEMORIA_USER_SCHEMA_INIT_POOL_MAX_CONNECTIONS", Some("")),
        ("MEMORIA_AUTH_POOL_MAX_CONNECTIONS", Some("")),
    ]
}

fn pool_override_envs<'a>(
    budget: &'a str,
    merged: Option<&'a str>,
    global: Option<&'a str>,
    user_init: Option<&'a str>,
    auth: Option<&'a str>,
) -> [(&'a str, Option<&'a str>); 8] {
    [
        ("MEMORIA_MULTI_DB_POOL_BUDGET", Some(budget)),
        ("MEMORIA_SHARED_POOL_MAX_CONNECTIONS", Some("")),
        ("MEMORIA_SHARED_MAIN_POOL_MAX_CONNECTIONS", Some("")),
        ("MEMORIA_GIT_POOL_MAX_CONNECTIONS", Some("")),
        ("MEMORIA_MERGED_SHARED_POOL_MAX_CONNECTIONS", merged),
        ("MEMORIA_GLOBAL_USER_POOL_MAX", global),
        ("MEMORIA_USER_SCHEMA_INIT_POOL_MAX_CONNECTIONS", user_init),
        ("MEMORIA_AUTH_POOL_MAX_CONNECTIONS", auth),
    ]
}

async fn spawn_budget_server_with_env(vars: &[(&str, Option<&str>)]) -> ApiTestServer {
    with_env_async(vars, || async {
        spawn_api_server(
            "pool_budget_e2e",
            1024,
            String::new(),
            None,
            None,
            None,
            true,
        )
        .await
    })
    .await
}

fn assert_expected_pools(case_name: &str, server: &ApiTestServer, expected: ExpectedPools) {
    assert_eq!(
        server.router().shared_pool_max_connections(),
        expected.shared,
        "{case_name}: shared merged pool size"
    );
    assert_eq!(
        server.shared_store().configured_max_connections(),
        Some(expected.shared),
        "{case_name}: shared store configured max"
    );
    assert_eq!(
        server.router().global_user_pool_max_connections(),
        expected.global,
        "{case_name}: global user pool size"
    );
    assert_eq!(
        server.router().user_init_pool_max_connections(),
        expected.user_init,
        "{case_name}: user init pool size"
    );
    assert_eq!(
        server.state().auth_pool_max_connections(),
        Some(expected.auth),
        "{case_name}: auth pool size"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn test_multi_db_pool_budget_defaults_and_boundaries() {
    let server = spawn_budget_server_with_env(&budget_envs(Some(""))).await;
    assert_expected_pools(
        "default budget",
        &server,
        ExpectedPools {
            shared: 102,
            global: 307,
            user_init: 26,
            auth: 51,
        },
    );
    drop(server);

    let server = spawn_budget_server_with_env(&budget_envs(Some("0"))).await;
    assert_expected_pools(
        "low budget floors",
        &server,
        ExpectedPools {
            shared: 64,
            global: 128,
            user_init: 10,
            auth: 25,
        },
    );
    drop(server);

    let server = spawn_budget_server_with_env(&budget_envs(Some("4096"))).await;
    assert_expected_pools(
        "high budget clamps to max",
        &server,
        ExpectedPools {
            shared: 410,
            global: 1229,
            user_init: 102,
            auth: 205,
        },
    );
}

#[tokio::test]
#[serial_test::serial]
async fn test_multi_db_pool_budget_overrides_and_clamps() {
    let server = spawn_budget_server_with_env(&pool_override_envs(
        "1024",
        Some(""),
        Some("333"),
        Some(""),
        Some("88"),
    ))
    .await;
    assert_expected_pools(
        "global/auth overrides keep other pools on budget defaults",
        &server,
        ExpectedPools {
            shared: 205,
            global: 333,
            user_init: 51,
            auth: 88,
        },
    );
    drop(server);

    let server = spawn_budget_server_with_env(&pool_override_envs(
        "1024",
        Some("250"),
        Some(""),
        Some("17"),
        Some(""),
    ))
    .await;
    assert_expected_pools(
        "shared/user-init overrides keep other pools on budget defaults",
        &server,
        ExpectedPools {
            shared: 250,
            global: 614,
            user_init: 17,
            auth: 102,
        },
    );
    drop(server);

    let server = spawn_budget_server_with_env(&pool_override_envs(
        "4096",
        Some("99999"),
        Some("99999"),
        Some("99999"),
        Some("99999"),
    ))
    .await;
    assert_expected_pools(
        "per-pool overrides clamp to derived caps",
        &server,
        ExpectedPools {
            shared: 410,
            global: 1229,
            user_init: 102,
            auth: 205,
        },
    );
}
