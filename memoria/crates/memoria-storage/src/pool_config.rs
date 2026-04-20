pub const MULTI_DB_POOL_BUDGET_ENV: &str = "MEMORIA_MULTI_DB_POOL_BUDGET";
pub const MULTI_DB_POOL_BUDGET_DEFAULT: u32 = 512;
pub const MULTI_DB_POOL_BUDGET_MAX: u32 = 2048;

const RATIO_DENOMINATOR: u64 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultiDbPoolKind {
    GlobalUser,
    SharedMerged,
    Auth,
    UserInit,
    Governance,
}

impl MultiDbPoolKind {
    fn ratio_percent(self) -> u32 {
        match self {
            Self::GlobalUser => 60,
            Self::SharedMerged => 20,
            Self::Auth => 10,
            Self::UserInit => 5,
            Self::Governance => 5,
        }
    }

    fn floor(self) -> u32 {
        match self {
            Self::GlobalUser => 128,
            Self::SharedMerged => 64,
            Self::Auth => 25,
            Self::UserInit => 10,
            Self::Governance => 10,
        }
    }
}

pub fn configured_multi_db_pool_budget() -> u32 {
    std::env::var(MULTI_DB_POOL_BUDGET_ENV)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(MULTI_DB_POOL_BUDGET_DEFAULT)
        .clamp(1, MULTI_DB_POOL_BUDGET_MAX)
}

pub fn multi_db_pool_default_size(kind: MultiDbPoolKind) -> u32 {
    scaled_size(configured_multi_db_pool_budget(), kind.ratio_percent()).max(kind.floor())
}

pub fn multi_db_pool_max_size(kind: MultiDbPoolKind) -> u32 {
    scaled_size(MULTI_DB_POOL_BUDGET_MAX, kind.ratio_percent())
}

pub fn configured_multi_db_pool_size(env_name: &str, kind: MultiDbPoolKind) -> u32 {
    match std::env::var(env_name)
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
    {
        Some(raw) => raw.clamp(1, multi_db_pool_max_size(kind)),
        None => multi_db_pool_default_size(kind),
    }
}

pub fn split_pool_budget(total: u32, weights: &[u32]) -> Vec<u32> {
    let weight_sum: u64 = weights.iter().map(|&w| u64::from(w)).sum();
    if weights.is_empty() || weight_sum == 0 {
        return vec![0; weights.len()];
    }

    let mut parts = Vec::with_capacity(weights.len());
    let mut remainders = Vec::with_capacity(weights.len());
    let mut used = 0u32;

    for (idx, weight) in weights.iter().copied().enumerate() {
        let numerator = u64::from(total) * u64::from(weight);
        let base = (numerator / weight_sum) as u32;
        parts.push(base);
        used = used.saturating_add(base);
        remainders.push((idx, numerator % weight_sum));
    }

    let mut remaining = total.saturating_sub(used);
    remainders.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    for (idx, _) in remainders {
        if remaining == 0 {
            break;
        }
        parts[idx] = parts[idx].saturating_add(1);
        remaining -= 1;
    }

    parts
}

fn scaled_size(total: u32, ratio_percent: u32) -> u32 {
    ((u64::from(total) * u64::from(ratio_percent) + (RATIO_DENOMINATOR / 2)) / RATIO_DENOMINATOR)
        as u32
}

#[cfg(test)]
mod tests {
    use super::{
        configured_multi_db_pool_budget, configured_multi_db_pool_size, multi_db_pool_default_size,
        multi_db_pool_max_size, split_pool_budget, MultiDbPoolKind, MULTI_DB_POOL_BUDGET_ENV,
    };
    use std::sync::{Mutex, OnceLock};

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn with_env<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
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

        f();
    }

    #[test]
    fn defaults_follow_budget_split() {
        with_env(&[(MULTI_DB_POOL_BUDGET_ENV, None)], || {
            assert_eq!(configured_multi_db_pool_budget(), 512);
            assert_eq!(multi_db_pool_default_size(MultiDbPoolKind::GlobalUser), 307);
            assert_eq!(
                multi_db_pool_default_size(MultiDbPoolKind::SharedMerged),
                102
            );
            assert_eq!(multi_db_pool_default_size(MultiDbPoolKind::Auth), 51);
            assert_eq!(multi_db_pool_default_size(MultiDbPoolKind::UserInit), 26);
            assert_eq!(multi_db_pool_default_size(MultiDbPoolKind::Governance), 26);
        });
    }

    #[test]
    fn caps_follow_budget_ceiling() {
        with_env(&[(MULTI_DB_POOL_BUDGET_ENV, Some("4096"))], || {
            assert_eq!(configured_multi_db_pool_budget(), 2048);
            assert_eq!(multi_db_pool_max_size(MultiDbPoolKind::GlobalUser), 1229);
            assert_eq!(multi_db_pool_max_size(MultiDbPoolKind::SharedMerged), 410);
            assert_eq!(multi_db_pool_max_size(MultiDbPoolKind::Auth), 205);
            assert_eq!(multi_db_pool_max_size(MultiDbPoolKind::UserInit), 102);
            assert_eq!(multi_db_pool_max_size(MultiDbPoolKind::Governance), 102);
        });
    }

    #[test]
    fn explicit_pool_envs_override_budget_without_flooring() {
        with_env(
            &[
                (MULTI_DB_POOL_BUDGET_ENV, Some("512")),
                ("MEMORIA_GLOBAL_USER_POOL_MAX", Some("4")),
                ("MEMORIA_AUTH_POOL_MAX_CONNECTIONS", Some("999")),
            ],
            || {
                assert_eq!(
                    configured_multi_db_pool_size(
                        "MEMORIA_GLOBAL_USER_POOL_MAX",
                        MultiDbPoolKind::GlobalUser,
                    ),
                    4
                );
                assert_eq!(
                    configured_multi_db_pool_size(
                        "MEMORIA_AUTH_POOL_MAX_CONNECTIONS",
                        MultiDbPoolKind::Auth,
                    ),
                    205
                );
            },
        );
    }

    #[test]
    fn split_pool_budget_preserves_total() {
        assert_eq!(
            split_pool_budget(512, &[60, 20, 10, 5, 5]),
            vec![307, 102, 51, 26, 26]
        );
        assert_eq!(split_pool_budget(64, &[40, 40, 20]), vec![26, 25, 13]);
    }
}
