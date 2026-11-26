use crate::account::model_support::is_model_supported_by_account;
use crate::account::rate_limits::is_account_cooling_down;
use crate::account::trackers::{HealthTracker, QuotaTracker, TokenBucketTracker};
use crate::account::types::Account;
use crate::config::{get_config, AccountSelectionConfig, WeightsConfig};
use crate::logger::logger;
use crate::utils::{format_duration, now_ms};

pub struct SelectionResult {
    pub index: Option<usize>,
    pub new_cursor: usize,
    pub wait_ms: u64,
}

fn is_account_usable(account: &mut Account, model_id: Option<&str>) -> bool {
    if account.is_invalid || !account.enabled {
        return false;
    }
    if let Some(mid) = model_id.filter(|s| !s.is_empty()) {
        if !is_model_supported_by_account(account, mid) {
            return false;
        }
    }
    if is_account_cooling_down(account) {
        return false;
    }
    if let Some(mid) = model_id.filter(|s| !s.is_empty()) {
        if let Some(limit) = account.model_rate_limits.get(mid) {
            if limit.is_rate_limited && limit.reset_time.unwrap_or(0) > now_ms() {
                return false;
            }
        }
    }
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FallbackLevel {
    Normal,
    Quota,
    Emergency,
    LastResort,
}

pub enum StrategyImpl {
    Hybrid(HybridStrategy),
    Sticky,
    RoundRobin { cursor: usize },
}

pub struct HybridStrategy {
    pub health: HealthTracker,
    pub token_bucket: TokenBucketTracker,
    pub quota: QuotaTracker,
    pub weights: WeightsConfig,
}

impl HybridStrategy {
    pub fn new(cfg: &AccountSelectionConfig) -> Self {
        Self {
            health: HealthTracker::new(cfg.health_score.clone()),
            token_bucket: TokenBucketTracker::new(cfg.token_bucket.clone()),
            quota: QuotaTracker::new(cfg.quota.clone()),
            weights: cfg.weights.clone(),
        }
    }

    fn effective_threshold(account: &Account, model_id: &str) -> Option<f64> {
        if let Some(&t) = account.model_quota_thresholds.get(model_id) {
            return Some(t);
        }
        if let Some(t) = account.quota_threshold {
            return Some(t);
        }
        let global = get_config().global_quota_threshold;
        if global > 0.0 {
            Some(global)
        } else {
            None
        }
    }

    fn calculate_score(&self, account: &Account, model_id: &str) -> f64 {
        let email = &account.email;
        let health_comp = self.health.get_score(email) * self.weights.health;

        let tokens = self.token_bucket.get_tokens(email);
        let max_tokens = self.token_bucket.get_max_tokens().max(1.0);
        let token_comp = ((tokens / max_tokens) * 100.0) * self.weights.tokens;

        let quota_comp = self.quota.get_score(account, model_id) * self.weights.quota;

        let last_used = account.last_used.unwrap_or(0);
        let since_last = now_ms().saturating_sub(last_used).min(3_600_000) as f64;
        let lru_comp = (since_last / 1000.0) * self.weights.lru;

        health_comp + token_comp + quota_comp + lru_comp
    }

    fn get_candidates(&self, accounts: &mut [Account], model_id: &str) -> (Vec<usize>, FallbackLevel) {
        let mut usable = Vec::new();
        for (idx, acc) in accounts.iter_mut().enumerate() {
            if is_account_usable(acc, Some(model_id)) {
                usable.push(idx);
            }
        }

        let normal: Vec<usize> = usable
            .iter()
            .copied()
            .filter(|&idx| {
                let acc = &accounts[idx];
                self.health.is_usable(&acc.email)
                    && self.token_bucket.has_tokens(&acc.email)
                    && !self.quota.is_quota_critical(
                        acc,
                        model_id,
                        Self::effective_threshold(acc, model_id),
                    )
            })
            .collect();
        if !normal.is_empty() {
            return (normal, FallbackLevel::Normal);
        }

        let quota_fb: Vec<usize> = usable
            .iter()
            .copied()
            .filter(|&idx| {
                let acc = &accounts[idx];
                self.health.is_usable(&acc.email) && self.token_bucket.has_tokens(&acc.email)
            })
            .collect();
        if !quota_fb.is_empty() {
            logger().warn("[HybridStrategy] All accounts have critical quota, using fallback");
            return (quota_fb, FallbackLevel::Quota);
        }

        let emergency: Vec<usize> = usable
            .iter()
            .copied()
            .filter(|&idx| self.token_bucket.has_tokens(&accounts[idx].email))
            .collect();
        if !emergency.is_empty() {
            logger().warn("[HybridStrategy] EMERGENCY: All accounts unhealthy, using least bad account");
            return (emergency, FallbackLevel::Emergency);
        }

        if !usable.is_empty() {
            logger().warn(
                "[HybridStrategy] LAST RESORT: All accounts exhausted, using any usable account",
            );
            return (usable, FallbackLevel::LastResort);
        }

        (Vec::new(), FallbackLevel::Normal)
    }

    fn diagnose_no_candidates(&mut self, accounts: &mut [Account], model_id: &str) -> (String, u64) {
        let mut unusable_cnt = 0usize;
        let mut unhealthy_cnt = 0usize;
        let mut no_tokens_cnt = 0usize;
        let mut critical_quota_cnt = 0usize;
        let mut no_tokens_emails = Vec::new();

        for acc in accounts.iter_mut() {
            if !is_account_usable(acc, Some(model_id)) {
                unusable_cnt += 1;
                continue;
            }
            if !self.health.is_usable(&acc.email) {
                unhealthy_cnt += 1;
                continue;
            }
            if !self.token_bucket.has_tokens(&acc.email) {
                no_tokens_cnt += 1;
                no_tokens_emails.push(acc.email.clone());
                continue;
            }
            if self.quota.is_quota_critical(
                acc,
                model_id,
                Self::effective_threshold(acc, model_id),
            ) {
                critical_quota_cnt += 1;
            }
        }

        if no_tokens_cnt > 0 && unusable_cnt == 0 && unhealthy_cnt == 0 {
            let wait_ms = self.token_bucket.get_min_time_until_token(&no_tokens_emails);
            return (
                format!("all {no_tokens_cnt} account(s) exhausted token bucket, waiting for refill"),
                wait_ms,
            );
        }

        let mut parts = Vec::new();
        if unusable_cnt > 0 {
            parts.push(format!("{unusable_cnt} unusable/disabled"));
        }
        if unhealthy_cnt > 0 {
            parts.push(format!("{unhealthy_cnt} unhealthy"));
        }
        if no_tokens_cnt > 0 {
            parts.push(format!("{no_tokens_cnt} no tokens"));
        }
        if critical_quota_cnt > 0 {
            parts.push(format!("{critical_quota_cnt} critical quota"));
        }
        let reason = if parts.is_empty() {
            "unknown".to_string()
        } else {
            parts.join(", ")
        };
        (reason, 0)
    }

    pub fn select(&mut self, accounts: &mut [Account], model_id: &str) -> SelectionResult {
        if accounts.is_empty() {
            return SelectionResult {
                index: None,
                new_cursor: 0,
                wait_ms: 0,
            };
        }

        let (candidates, fb_level) = self.get_candidates(accounts, model_id);
        if candidates.is_empty() {
            let (reason, wait_ms) = self.diagnose_no_candidates(accounts, model_id);
            logger().warn(format!("[HybridStrategy] No candidates available: {reason}"));
            return SelectionResult {
                index: None,
                new_cursor: 0,
                wait_ms,
            };
        }

        let mut scored: Vec<(usize, f64)> = candidates
            .into_iter()
            .map(|idx| (idx, self.calculate_score(&accounts[idx], model_id)))
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let (best_idx, best_score) = scored[0];
        let email = accounts[best_idx].email.clone();
        accounts[best_idx].last_used = Some(now_ms());

        if fb_level != FallbackLevel::LastResort {
            self.token_bucket.consume(&email);
        }

        let wait_ms = match fb_level {
            FallbackLevel::LastResort => 500,
            FallbackLevel::Emergency => 250,
            _ => 0,
        };

        let fb_info = match fb_level {
            FallbackLevel::Normal => "",
            FallbackLevel::Quota => ", fallback: quota",
            FallbackLevel::Emergency => ", fallback: emergency",
            FallbackLevel::LastResort => ", fallback: lastResort",
        };

        logger().info(format!(
            "[HybridStrategy] Using account: {email} ({}/{}, score: {best_score:.1}{fb_info})",
            best_idx + 1,
            accounts.len()
        ));

        SelectionResult {
            index: Some(best_idx),
            new_cursor: best_idx,
            wait_ms,
        }
    }
}

impl StrategyImpl {
    pub fn from_name(name: &str, cfg: &AccountSelectionConfig) -> Self {
        match name.to_lowercase().as_str() {
            "sticky" => Self::Sticky,
            "round-robin" | "roundrobin" => Self::RoundRobin { cursor: 0 },
            _ => Self::Hybrid(HybridStrategy::new(cfg)),
        }
    }

    pub fn select_account(
        &mut self,
        accounts: &mut [Account],
        model_id: &str,
        current_index: usize,
    ) -> SelectionResult {
        match self {
            Self::Hybrid(h) => h.select(accounts, model_id),
            Self::Sticky => select_sticky(accounts, model_id, current_index),
            Self::RoundRobin { cursor } => select_round_robin(accounts, model_id, cursor),
        }
    }

    pub fn on_success(&mut self, email: &str) {
        if let Self::Hybrid(h) = self {
            h.health.record_success(email);
        }
    }

    pub fn on_rate_limit(&mut self, email: &str) {
        if let Self::Hybrid(h) = self {
            h.health.record_rate_limit(email);
        }
    }

    pub fn on_failure(&mut self, email: &str) {
        if let Self::Hybrid(h) = self {
            h.health.record_failure(email);
            h.token_bucket.refund(email);
        }
    }
}

fn select_sticky(accounts: &mut [Account], model_id: &str, current_index: usize) -> SelectionResult {
    if accounts.is_empty() {
        return SelectionResult {
            index: None,
            new_cursor: current_index,
            wait_ms: 0,
        };
    }

    let idx = if current_index >= accounts.len() {
        0
    } else {
        current_index
    };

    if is_account_usable(&mut accounts[idx], Some(model_id)) {
        accounts[idx].last_used = Some(now_ms());
        return SelectionResult {
            index: Some(idx),
            new_cursor: idx,
            wait_ms: 0,
        };
    }

    let total = accounts.len();
    for i in 1..=total {
        let next_idx = (idx + i) % total;
        if is_account_usable(&mut accounts[next_idx], Some(model_id)) {
            accounts[next_idx].last_used = Some(now_ms());
            logger().info(format!(
                "[StickyStrategy] Switched to new account (failover): {}",
                accounts[next_idx].email
            ));
            return SelectionResult {
                index: Some(next_idx),
                new_cursor: next_idx,
                wait_ms: 0,
            };
        }
    }

    // Check if should wait for current sticky account
    let max_wait = get_config().max_wait_before_error_ms;
    let cur = &accounts[idx];
    if !cur.is_invalid && cur.enabled && is_model_supported_by_account(cur, model_id) {
        if let Some(limit) = cur.model_rate_limits.get(model_id) {
            if limit.is_rate_limited {
                if let Some(rt) = limit.reset_time {
                    let now = now_ms();
                    if rt > now && (rt - now) <= max_wait {
                        let wait_ms = rt - now;
                        logger().info(format!(
                            "[StickyStrategy] Waiting {} for sticky account: {}",
                            format_duration(wait_ms),
                            cur.email
                        ));
                        return SelectionResult {
                            index: None,
                            new_cursor: idx,
                            wait_ms,
                        };
                    }
                }
            }
        }
    }

    SelectionResult {
        index: None,
        new_cursor: idx,
        wait_ms: 0,
    }
}

fn select_round_robin(
    accounts: &mut [Account],
    model_id: &str,
    cursor: &mut usize,
) -> SelectionResult {
    if accounts.is_empty() {
        return SelectionResult {
            index: None,
            new_cursor: 0,
            wait_ms: 0,
        };
    }
    if *cursor >= accounts.len() {
        *cursor = 0;
    }
    let start = (*cursor + 1) % accounts.len();
    for i in 0..accounts.len() {
        let idx = (start + i) % accounts.len();
        if is_account_usable(&mut accounts[idx], Some(model_id)) {
            accounts[idx].last_used = Some(now_ms());
            *cursor = idx;
            logger().info(format!(
                "[RoundRobinStrategy] Using account: {} ({}/{})",
                accounts[idx].email,
                idx + 1,
                accounts.len()
            ));
            return SelectionResult {
                index: Some(idx),
                new_cursor: idx,
                wait_ms: 0,
            };
        }
    }
    SelectionResult {
        index: None,
        new_cursor: *cursor,
        wait_ms: 0,
    }
}
