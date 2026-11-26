use crate::account::model_support::is_model_supported_by_account;
use crate::account::types::{Account, ModelRateLimit};
use crate::config::get_config;
use crate::logger::logger;
use crate::utils::{format_duration, now_ms};

#[derive(Debug, Clone, Copy)]
pub enum CooldownReason {
    RateLimit,
    AuthFailure,
    ConsecutiveFailures,
    ServerError,
}

impl CooldownReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RateLimit => "rate_limit",
            Self::AuthFailure => "auth_failure",
            Self::ConsecutiveFailures => "consecutive_failures",
            Self::ServerError => "server_error",
        }
    }
}

pub fn is_all_rate_limited(accounts: &[Account], model_id: Option<&str>) -> bool {
    if accounts.is_empty() {
        return true;
    }
    let Some(mid) = model_id.filter(|s| !s.is_empty()) else {
        return false;
    };

    let supporting: Vec<&Account> = accounts
        .iter()
        .filter(|acc| !acc.is_invalid && acc.enabled && is_model_supported_by_account(acc, mid))
        .collect();

    if supporting.is_empty() {
        return false;
    }

    let now = now_ms();
    supporting.iter().all(|acc| {
        acc.model_rate_limits
            .get(mid)
            .map(|l| l.is_rate_limited && l.reset_time.unwrap_or(0) > now)
            .unwrap_or(false)
    })
}

pub fn get_available_accounts<'a>(accounts: &'a [Account], model_id: Option<&str>) -> Vec<&'a Account> {
    let now = now_ms();
    accounts
        .iter()
        .filter(|acc| {
            if acc.is_invalid || !acc.enabled {
                return false;
            }
            if let Some(mid) = model_id.filter(|s| !s.is_empty()) {
                if !is_model_supported_by_account(acc, mid) {
                    return false;
                }
                if let Some(limit) = acc.model_rate_limits.get(mid) {
                    if limit.is_rate_limited && limit.reset_time.unwrap_or(0) > now {
                        return false;
                    }
                }
            }
            true
        })
        .collect()
}

pub fn clear_expired_limits(accounts: &mut [Account]) -> usize {
    let now = now_ms();
    let mut cleared = 0;
    for account in accounts.iter_mut() {
        for (model_id, limit) in account.model_rate_limits.iter_mut() {
            if limit.is_rate_limited && limit.reset_time.unwrap_or(0) <= now {
                limit.is_rate_limited = false;
                limit.reset_time = None;
                cleared += 1;
                logger().success(format!(
                    "[AccountManager] Rate limit expired for: {} (model: {model_id})",
                    account.email
                ));
            }
        }
    }
    cleared
}

pub fn reset_all_rate_limits(accounts: &mut [Account], model_id: Option<&str>) {
    for account in accounts.iter_mut() {
        if let Some(mid) = model_id {
            if let Some(entry) = account.model_rate_limits.get_mut(mid) {
                entry.is_rate_limited = false;
                entry.reset_time = None;
            }
        } else {
            for entry in account.model_rate_limits.values_mut() {
                entry.is_rate_limited = false;
                entry.reset_time = None;
            }
        }
    }
    if let Some(mid) = model_id {
        logger().warn(format!(
            "[AccountManager] Reset rate limits for {mid} (optimistic retry)"
        ));
    } else {
        logger().warn("[AccountManager] Reset all rate limits for optimistic retry");
    }
}

pub fn should_optimistic_retry(accounts: &[Account], model_id: &str, min_age_ms: u64) -> bool {
    if model_id.is_empty() || !is_all_rate_limited(accounts, Some(model_id)) {
        return false;
    }
    let now = now_ms();
    accounts
        .iter()
        .filter(|acc| !acc.is_invalid && acc.enabled && is_model_supported_by_account(acc, model_id))
        .any(|acc| match acc.model_rate_limits.get(model_id) {
            None => true,
            Some(l) if !l.is_rate_limited => true,
            Some(l) if l.reset_time.map(|rt| rt <= now).unwrap_or(false) => true,
            Some(l) => match l.marked_at {
                None => true,
                Some(m) => now.saturating_sub(m) >= min_age_ms,
            },
        })
}

pub fn mark_rate_limited(
    accounts: &mut [Account],
    email: &str,
    reset_ms: Option<u64>,
    model_id: &str,
) -> bool {
    let Some(account) = accounts.iter_mut().find(|a| a.email == email) else {
        return false;
    };
    let default_cd = get_config().default_cooldown_ms;
    let actual_reset_ms = reset_ms.filter(|&r| r > 0).unwrap_or(default_cd);
    let now = now_ms();

    account.model_rate_limits.insert(
        model_id.to_string(),
        ModelRateLimit {
            is_rate_limited: true,
            reset_time: Some(now + actual_reset_ms),
            actual_reset_ms: Some(actual_reset_ms),
            marked_at: Some(now),
        },
    );
    account.consecutive_failures += 1;

    if actual_reset_ms > default_cd {
        logger().warn(format!(
            "[AccountManager] Quota exhausted: {email} (model: {model_id}). Resets in {}",
            format_duration(actual_reset_ms)
        ));
    } else {
        logger().warn(format!(
            "[AccountManager] Rate limited: {email} (model: {model_id}). Available in {}",
            format_duration(actual_reset_ms)
        ));
    }
    true
}

pub fn mark_invalid(
    accounts: &mut [Account],
    email: &str,
    reason: &str,
    verify_url: Option<String>,
) -> bool {
    let Some(account) = accounts.iter_mut().find(|a| a.email == email) else {
        return false;
    };
    account.is_invalid = true;
    account.invalid_reason = Some(reason.to_string());
    account.invalid_at = Some(now_ms());
    account.verify_url = verify_url.clone();

    logger().error(format!("[AccountManager] Account INVALID: {email}"));
    logger().error(format!("[AccountManager]   Reason: {reason}"));
    if let Some(url) = verify_url {
        logger().error(format!("[AccountManager]   Verification URL: {url}"));
    }
    true
}

pub fn clear_invalid(accounts: &mut [Account], email: &str) -> bool {
    let Some(account) = accounts.iter_mut().find(|a| a.email == email) else {
        return false;
    };
    account.is_invalid = false;
    account.invalid_reason = None;
    account.verify_url = None;
    logger().info(format!("[AccountManager] Account re-enabled: {email}"));
    true
}

pub fn get_min_wait_time_ms(accounts: &[Account], model_id: Option<&str>) -> u64 {
    let default_cd = get_config().default_cooldown_ms;
    if !is_all_rate_limited(accounts, model_id) {
        return 0;
    }
    let Some(mid) = model_id else {
        return default_cd;
    };

    let now = now_ms();
    let mut min_wait: Option<u64> = None;
    let mut soonest_email: Option<&str> = None;

    for account in accounts {
        if account.is_invalid || !account.enabled {
            continue;
        }
        if !is_model_supported_by_account(account, mid) {
            continue;
        }
        if let Some(limit) = account.model_rate_limits.get(mid) {
            if limit.is_rate_limited {
                if let Some(rt) = limit.reset_time {
                    if rt > now {
                        let wait = rt - now;
                        if min_wait.map(|m| wait < m).unwrap_or(true) {
                            min_wait = Some(wait);
                            soonest_email = Some(&account.email);
                        }
                    }
                }
            }
        }
    }

    if let (Some(wait), Some(email)) = (min_wait, soonest_email) {
        logger().info(format!(
            "[AccountManager] Shortest wait: {} (account: {email})",
            format_duration(wait)
        ));
        wait
    } else {
        default_cd
    }
}

pub fn is_account_cooling_down(account: &mut Account) -> bool {
    let Some(until) = account.cooling_down_until else {
        return false;
    };
    if now_ms() >= until {
        account.cooling_down_until = None;
        account.cooldown_reason = None;
        false
    } else {
        true
    }
}

pub fn is_account_cooling_down_ro(account: &Account) -> bool {
    account
        .cooling_down_until
        .map(|until| now_ms() < until)
        .unwrap_or(false)
}
