use crate::account::types::Account;
use crate::config::{HealthScoreConfig, QuotaConfig, TokenBucketConfig};
use crate::utils::now_ms;
use std::collections::HashMap;

#[derive(Debug, Clone)]
struct HealthRecord {
    score: f64,
    last_updated: u64,
    consecutive_failures: u32,
}

pub struct HealthTracker {
    scores: HashMap<String, HealthRecord>,
    config: HealthScoreConfig,
}

impl HealthTracker {
    pub fn new(config: HealthScoreConfig) -> Self {
        Self {
            scores: HashMap::new(),
            config,
        }
    }

    pub fn get_score(&self, email: &str) -> f64 {
        let Some(rec) = self.scores.get(email) else {
            return self.config.initial;
        };
        let hours = (now_ms().saturating_sub(rec.last_updated) as f64) / (1000.0 * 3600.0);
        let recovered = rec.score + hours * self.config.recovery_per_hour;
        recovered.min(self.config.max_score)
    }

    pub fn record_success(&mut self, email: &str) {
        let cur = self.get_score(email);
        let next = (cur + self.config.success_reward).min(self.config.max_score);
        self.scores.insert(
            email.to_string(),
            HealthRecord {
                score: next,
                last_updated: now_ms(),
                consecutive_failures: 0,
            },
        );
    }

    pub fn record_rate_limit(&mut self, email: &str) {
        let prev_failures = self
            .scores
            .get(email)
            .map(|r| r.consecutive_failures)
            .unwrap_or(0);
        let cur = self.get_score(email);
        let next = (cur + self.config.rate_limit_penalty).max(0.0);
        self.scores.insert(
            email.to_string(),
            HealthRecord {
                score: next,
                last_updated: now_ms(),
                consecutive_failures: prev_failures + 1,
            },
        );
    }

    pub fn record_failure(&mut self, email: &str) {
        let prev_failures = self
            .scores
            .get(email)
            .map(|r| r.consecutive_failures)
            .unwrap_or(0);
        let cur = self.get_score(email);
        let next = (cur + self.config.failure_penalty).max(0.0);
        self.scores.insert(
            email.to_string(),
            HealthRecord {
                score: next,
                last_updated: now_ms(),
                consecutive_failures: prev_failures + 1,
            },
        );
    }

    pub fn is_usable(&self, email: &str) -> bool {
        self.get_score(email) >= self.config.min_usable
    }

    pub fn get_consecutive_failures(&self, email: &str) -> u32 {
        self.scores
            .get(email)
            .map(|r| r.consecutive_failures)
            .unwrap_or(0)
    }
}

#[derive(Debug, Clone)]
struct BucketRecord {
    tokens: f64,
    last_updated: u64,
}

pub struct TokenBucketTracker {
    buckets: HashMap<String, BucketRecord>,
    config: TokenBucketConfig,
}

impl TokenBucketTracker {
    pub fn new(config: TokenBucketConfig) -> Self {
        Self {
            buckets: HashMap::new(),
            config,
        }
    }

    pub fn get_tokens(&self, email: &str) -> f64 {
        let Some(b) = self.buckets.get(email) else {
            return self.config.initial_tokens;
        };
        let mins = (now_ms().saturating_sub(b.last_updated) as f64) / 60_000.0;
        let regen = mins * self.config.tokens_per_minute;
        (b.tokens + regen).min(self.config.max_tokens)
    }

    pub fn has_tokens(&self, email: &str) -> bool {
        self.get_tokens(email) >= 1.0
    }

    pub fn consume(&mut self, email: &str) -> bool {
        let cur = self.get_tokens(email);
        if cur < 1.0 {
            return false;
        }
        self.buckets.insert(
            email.to_string(),
            BucketRecord {
                tokens: cur - 1.0,
                last_updated: now_ms(),
            },
        );
        true
    }

    pub fn refund(&mut self, email: &str) {
        let cur = self.get_tokens(email);
        let next = (cur + 1.0).min(self.config.max_tokens);
        self.buckets.insert(
            email.to_string(),
            BucketRecord {
                tokens: next,
                last_updated: now_ms(),
            },
        );
    }

    pub fn get_max_tokens(&self) -> f64 {
        self.config.max_tokens
    }

    pub fn get_min_time_until_token(&self, emails: &[String]) -> u64 {
        if emails.is_empty() {
            return 0;
        }
        let mut min_wait = u64::MAX;
        for email in emails {
            let cur = self.get_tokens(email);
            if cur >= 1.0 {
                return 0;
            }
            let needed = 1.0 - cur;
            let mins = needed / self.config.tokens_per_minute;
            let ms = (mins * 60_000.0).ceil() as u64;
            min_wait = min_wait.min(ms);
        }
        if min_wait == u64::MAX {
            0
        } else {
            min_wait
        }
    }
}

pub struct QuotaTracker {
    config: QuotaConfig,
    unknown_score: f64,
}

impl QuotaTracker {
    pub fn new(config: QuotaConfig) -> Self {
        Self {
            config,
            unknown_score: 50.0,
        }
    }

    pub fn get_quota_fraction(&self, account: &Account, model_id: &str) -> Option<f64> {
        account
            .quota
            .models
            .get(model_id)
            .and_then(|q| q.remaining_fraction)
    }

    pub fn is_quota_fresh(&self, account: &Account) -> bool {
        let Some(last) = account.quota.last_checked else {
            return false;
        };
        now_ms().saturating_sub(last) < self.config.stale_ms
    }

    pub fn is_quota_critical(
        &self,
        account: &Account,
        model_id: &str,
        threshold_override: Option<f64>,
    ) -> bool {
        let Some(fraction) = self.get_quota_fraction(account, model_id) else {
            return false;
        };
        if !self.is_quota_fresh(account) {
            return false;
        }
        let threshold = threshold_override
            .filter(|&t| t > 0.0)
            .unwrap_or(self.config.critical_threshold);
        fraction <= threshold
    }

    pub fn get_score(&self, account: &Account, model_id: &str) -> f64 {
        let Some(fraction) = self.get_quota_fraction(account, model_id) else {
            return self.unknown_score;
        };
        let mut score = fraction * 100.0;
        if !self.is_quota_fresh(account) {
            score *= 0.9;
        }
        score
    }
}
