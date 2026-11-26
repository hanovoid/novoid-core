use crate::cloudcode::rate_limit_parser::{parse_rate_limit_reason, RateLimitReason};
use crate::config::get_config;
use crate::constants::{
    BACKOFF_MODEL_CAPACITY_EXHAUSTED_MS, BACKOFF_RATE_LIMIT_EXCEEDED_MS, BACKOFF_SERVER_ERROR_MS,
    BACKOFF_UNKNOWN_MS, CAPACITY_JITTER_MAX_MS, FIRST_RETRY_DELAY_MS, MIN_BACKOFF_MS,
    QUOTA_EXHAUSTED_BACKOFF_TIERS_MS, RATE_LIMIT_STATE_RESET_MS,
};
use crate::utils::{generate_jitter_ms, now_ms};
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

#[derive(Clone)]
struct RateLimitState {
    consecutive_429: u32,
    last_at: u64,
}

static STATE_MAP: OnceLock<Mutex<HashMap<String, RateLimitState>>> = OnceLock::new();

fn state_map() -> &'static Mutex<HashMap<String, RateLimitState>> {
    STATE_MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn init_background_cleanup() {
    let _ = state_map();
    tokio::spawn(async {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            let reset_ms = get_config()
                .rate_limit_state_reset_ms
                .unwrap_or(RATE_LIMIT_STATE_RESET_MS);
            let cutoff = now_ms().saturating_sub(reset_ms);
            if let Ok(mut guard) = state_map().lock() {
                guard.retain(|_, v| v.last_at >= cutoff);
            }
        }
    });
}

pub struct BackoffDecision {
    pub attempt: u32,
    pub delay_ms: u64,
    pub is_duplicate: bool,
}

pub fn get_rate_limit_backoff(
    email: &str,
    model: &str,
    server_retry_after_ms: Option<u64>,
) -> BackoffDecision {
    let cfg = get_config();
    let dedup_window = cfg.rate_limit_dedup_window_ms;
    let state_reset = cfg
        .rate_limit_state_reset_ms
        .unwrap_or(RATE_LIMIT_STATE_RESET_MS);
    let first_delay = cfg.first_retry_delay_ms.unwrap_or(FIRST_RETRY_DELAY_MS);

    let now = now_ms();
    let key = format!("{email}:{model}");
    let mut guard = state_map().lock().expect("rate limit state lock");

    if let Some(prev) = guard.get(&key) {
        if now.saturating_sub(prev.last_at) < dedup_window {
            let base = server_retry_after_ms.unwrap_or(first_delay);
            let exp = base.saturating_mul(1u64 << prev.consecutive_429.saturating_sub(1).min(10));
            let delay = exp.min(60_000).max(base);
            return BackoffDecision {
                attempt: prev.consecutive_429,
                delay_ms: delay,
                is_duplicate: true,
            };
        }
    }

    let attempt = match guard.get(&key) {
        Some(prev) if now.saturating_sub(prev.last_at) < state_reset => prev.consecutive_429 + 1,
        _ => 1,
    };

    guard.insert(
        key,
        RateLimitState {
            consecutive_429: attempt,
            last_at: now,
        },
    );

    let base = server_retry_after_ms.unwrap_or(first_delay);
    let exp = base.saturating_mul(1u64 << attempt.saturating_sub(1).min(10));
    let delay = exp.min(60_000).max(base);

    BackoffDecision {
        attempt,
        delay_ms: delay,
        is_duplicate: false,
    }
}

pub fn clear_rate_limit_state(email: &str, model: &str) {
    let key = format!("{email}:{model}");
    if let Ok(mut guard) = state_map().lock() {
        guard.remove(&key);
    }
}

pub fn is_permanent_auth_failure(error_text: &str) -> bool {
    let lower = error_text.to_lowercase();
    lower.contains("invalid_grant")
        || lower.contains("token revoked")
        || lower.contains("token has been expired or revoked")
        || lower.contains("token_revoked")
        || lower.contains("invalid_client")
        || lower.contains("credentials are invalid")
}

pub fn is_validation_required(error_text: &str) -> bool {
    let lower = error_text.to_lowercase();
    lower.contains("validation_required")
        || lower.contains("account_disabled")
        || lower.contains("user_disabled")
}

pub fn extract_verification_url(error_text: &str) -> Option<String> {
    if error_text.is_empty() {
        return None;
    }
    if let Ok(parsed) = serde_json::from_str::<Value>(error_text) {
        if let Some(details) = parsed
            .get("error")
            .and_then(|e| e.get("details"))
            .and_then(|d| d.as_array())
        {
            for d in details {
                if let Some(url) = d
                    .get("metadata")
                    .and_then(|m| m.get("validation_url"))
                    .and_then(|v| v.as_str())
                {
                    return Some(url.to_string());
                }
            }
        }
    }
    if let Ok(re) = Regex::new(r#"https://accounts\.google\.com/signin/continue\?[^\s"\\]+"#) {
        if let Some(m) = re.find(error_text) {
            let cleaned = m
                .as_str()
                .trim_end_matches(&[',', '.', ')', '>', '}', ']'][..]);
            return Some(cleaned.to_string());
        }
    }
    None
}

pub fn is_account_banned(error_text: &str) -> bool {
    let lower = error_text.to_lowercase();
    lower.contains("has been disabled") && lower.contains("violation of terms of service")
}

pub fn is_model_capacity_exhausted(error_text: &str) -> bool {
    let lower = error_text.to_lowercase();
    lower.contains("model_capacity_exhausted")
        || lower.contains("capacity_exhausted")
        || lower.contains("model is currently overloaded")
        || lower.contains("service temporarily unavailable")
}

pub fn calculate_smart_backoff(
    error_text: &str,
    server_reset_ms: Option<u64>,
    consecutive_failures: u32,
) -> u64 {
    if let Some(ms) = server_reset_ms {
        if ms > 0 {
            return ms.max(MIN_BACKOFF_MS);
        }
    }

    match parse_rate_limit_reason(error_text, None) {
        RateLimitReason::QuotaExhausted => {
            let idx = (consecutive_failures as usize).min(QUOTA_EXHAUSTED_BACKOFF_TIERS_MS.len() - 1);
            QUOTA_EXHAUSTED_BACKOFF_TIERS_MS[idx]
        }
        RateLimitReason::RateLimitExceeded => BACKOFF_RATE_LIMIT_EXCEEDED_MS,
        RateLimitReason::ModelCapacityExhausted => {
            let jitter = generate_jitter_ms(CAPACITY_JITTER_MAX_MS);
            (BACKOFF_MODEL_CAPACITY_EXHAUSTED_MS as i64 + jitter).max(MIN_BACKOFF_MS as i64) as u64
        }
        RateLimitReason::ServerError => BACKOFF_SERVER_ERROR_MS,
        RateLimitReason::Unknown => BACKOFF_UNKNOWN_MS,
    }
}
