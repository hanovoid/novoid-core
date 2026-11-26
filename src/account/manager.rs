use crate::account::model_support::mark_account_model_unsupported;
use crate::account::oauth::{
    format_refresh_parts, get_default_tier_id, onboard_user, parse_refresh_parts,
    refresh_access_token,
};
use crate::account::rate_limits::{
    clear_expired_limits, get_available_accounts, get_min_wait_time_ms, is_all_rate_limited,
    mark_invalid as rl_mark_invalid, mark_rate_limited as rl_mark_rate_limited,
    reset_all_rate_limits, should_optimistic_retry, CooldownReason,
};
use crate::account::storage::{load_accounts, save_accounts};
use crate::account::strategy::StrategyImpl;
use crate::account::types::{Account, SubscriptionInfo};
use crate::cloudcode::model_api::{clear_model_cache, parse_tier_id};
use crate::config::get_config;
use crate::constants::{
    antigravity_headers, client_metadata, get_strategy_label, token_refresh_interval_ms,
    DEFAULT_PROJECT_ID, DEFAULT_SELECTION_STRATEGY, LOAD_CODE_ASSIST_ENDPOINTS,
};
use crate::error::NovoidError;
use crate::logger::logger;
use crate::utils::{http_client, maybe_throttle, now_ms};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use tokio::sync::{Mutex, RwLock};

#[derive(Clone)]
struct CachedToken {
    token: String,
    extracted_at: u64,
}

struct PoolInner {
    accounts: Vec<Account>,
    settings: HashMap<String, Value>,
    current_index: usize,
    strategy: StrategyImpl,
    strategy_name: String,
    initialized: bool,
    token_cache: HashMap<String, CachedToken>,
    project_cache: HashMap<String, String>,
    subscription_in_flight: HashSet<String>,
}

pub struct AccountManager {
    config_path: PathBuf,
    strategy_override: Option<String>,
    init_lock: Mutex<()>,
    inner: RwLock<PoolInner>,
}

impl AccountManager {
    pub fn new(config_path: PathBuf, strategy_override: Option<String>) -> Self {
        let cfg = get_config();
        let initial_name = strategy_override
            .clone()
            .or_else(|| std::env::var("ACCOUNT_STRATEGY").ok())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                if cfg.account_selection.strategy.is_empty() {
                    DEFAULT_SELECTION_STRATEGY.to_string()
                } else {
                    cfg.account_selection.strategy.clone()
                }
            });

        let strategy = StrategyImpl::from_name(&initial_name, &cfg.account_selection);
        Self {
            config_path,
            strategy_override,
            init_lock: Mutex::new(()),
            inner: RwLock::new(PoolInner {
                accounts: Vec::new(),
                settings: HashMap::new(),
                current_index: 0,
                strategy,
                strategy_name: initial_name,
                initialized: false,
                token_cache: HashMap::new(),
                project_cache: HashMap::new(),
                subscription_in_flight: HashSet::new(),
            }),
        }
    }

    pub async fn ensure_initialized(&self) {
        if self.inner.read().await.initialized {
            return;
        }
        let _guard = self.init_lock.lock().await;
        if self.inner.read().await.initialized {
            return;
        }
        self.initialize_inner().await;
    }

    async fn initialize_inner(&self) {
        let loaded = load_accounts(&self.config_path).await;
        let cfg = get_config();
        let strat_name = self
            .strategy_override
            .clone()
            .or_else(|| std::env::var("ACCOUNT_STRATEGY").ok().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| cfg.account_selection.strategy.clone());

        let mut inner = self.inner.write().await;
        inner.accounts = loaded.accounts;
        inner.settings = loaded.settings;
        inner.current_index = loaded.active_index;
        inner.strategy_name = strat_name.clone();
        inner.strategy = StrategyImpl::from_name(&strat_name, &cfg.account_selection);

        if inner.accounts.is_empty() {
            logger().warn(format!(
                "[AccountManager] No accounts configured in {}. Add accounts via WebUI or CLI.",
                self.config_path.display()
            ));
        }

        let cleared = clear_expired_limits(&mut inner.accounts);
        inner.initialized = true;
        let summary = Self::build_summary(&inner.accounts);
        let accounts_snap = inner.accounts.clone();
        let settings_snap = inner.settings.clone();
        let idx_snap = inner.current_index;
        drop(inner);

        if cleared > 0 {
            save_accounts(&self.config_path, &accounts_snap, &settings_snap, idx_snap).await;
        }

        logger().info(format!(
            "[AccountManager] Using {} selection strategy",
            get_strategy_label(&strat_name)
        ));
        logger().success(format!("[Server] Account pool initialized: {summary}"));
    }

    pub async fn reload(&self) {
        let _guard = self.init_lock.lock().await;
        clear_model_cache().await;
        let old_creds: HashMap<String, (Option<String>, Option<String>)> = {
            let mut inner = self.inner.write().await;
            inner.initialized = false;
            inner
                .accounts
                .iter()
                .map(|a| {
                    (
                        a.email.clone(),
                        (a.refresh_token.clone(), a.api_key.clone()),
                    )
                })
                .collect()
        };
        self.initialize_inner().await;

        // Preserve valid token/project caches for accounts whose credentials didn't change
        {
            let mut inner = self.inner.write().await;
            let active_same: HashSet<String> = inner
                .accounts
                .iter()
                .filter(|a| {
                    old_creds
                        .get(&a.email)
                        .map(|(rt, ak)| rt == &a.refresh_token && ak == &a.api_key && !a.is_invalid)
                        .unwrap_or(false)
                })
                .map(|a| a.email.clone())
                .collect();
            inner.token_cache.retain(|k, _| active_same.contains(k));
            inner.project_cache.retain(|k, _| active_same.contains(k));
        }

        logger().info("[AccountManager] Accounts reloaded from disk");
    }

    fn build_summary(accounts: &[Account]) -> String {
        let total = accounts.len();
        let available = get_available_accounts(accounts, None).len();
        let now = now_ms();
        let rate_limited = accounts
            .iter()
            .filter(|a| {
                a.model_rate_limits
                    .values()
                    .any(|l| l.is_rate_limited && l.reset_time.unwrap_or(0) > now)
            })
            .count();
        let invalid = accounts.iter().filter(|a| a.is_invalid).count();
        format!(
            "{total} total, {available} available, {rate_limited} rate-limited, {invalid} invalid"
        )
    }

    pub async fn save_to_disk(&self) {
        let (accounts, settings, idx) = {
            let inner = self.inner.read().await;
            (
                inner.accounts.clone(),
                inner.settings.clone(),
                inner.current_index,
            )
        };
        save_accounts(&self.config_path, &accounts, &settings, idx).await;
    }

    pub async fn get_account_count(&self) -> usize {
        self.inner.read().await.accounts.len()
    }

    pub async fn get_all_accounts(&self) -> Vec<Account> {
        self.inner.read().await.accounts.clone()
    }

    pub async fn get_settings(&self) -> HashMap<String, Value> {
        self.inner.read().await.settings.clone()
    }

    pub async fn get_strategy_label(&self) -> &'static str {
        let name = self.inner.read().await.strategy_name.clone();
        get_strategy_label(&name)
    }

    pub async fn clear_expired_limits(&self) -> usize {
        let cleared = {
            let mut inner = self.inner.write().await;
            clear_expired_limits(&mut inner.accounts)
        };
        if cleared > 0 {
            self.save_to_disk().await;
        }
        cleared
    }

    pub async fn get_available_accounts(&self, model_id: Option<&str>) -> Vec<Account> {
        let inner = self.inner.read().await;
        get_available_accounts(&inner.accounts, model_id)
            .into_iter()
            .cloned()
            .collect()
    }

    pub async fn is_all_accounts_invalid(&self) -> bool {
        let inner = self.inner.read().await;
        let enabled: Vec<&Account> = inner.accounts.iter().filter(|a| a.enabled).collect();
        !enabled.is_empty() && enabled.iter().all(|a| a.is_invalid)
    }

    pub async fn get_invalid_accounts(&self) -> Vec<Account> {
        let inner = self.inner.read().await;
        inner
            .accounts
            .iter()
            .filter(|a| a.is_invalid)
            .cloned()
            .collect()
    }

    pub async fn is_all_rate_limited(&self, model_id: Option<&str>) -> bool {
        let inner = self.inner.read().await;
        is_all_rate_limited(&inner.accounts, model_id)
    }

    pub async fn should_optimistic_retry(&self, model_id: &str, min_age_ms: u64) -> bool {
        let inner = self.inner.read().await;
        should_optimistic_retry(&inner.accounts, model_id, min_age_ms)
    }

    pub async fn reset_all_rate_limits(&self, model_id: Option<&str>) {
        let mut inner = self.inner.write().await;
        reset_all_rate_limits(&mut inner.accounts, model_id);
    }

    pub async fn get_min_wait_time_ms(&self, model_id: Option<&str>) -> u64 {
        let inner = self.inner.read().await;
        get_min_wait_time_ms(&inner.accounts, model_id)
    }

    pub async fn has_supporting_account(&self, model_id: &str) -> bool {
        let inner = self.inner.read().await;
        inner.accounts.iter().any(|acc| {
            !acc.is_invalid
                && acc.enabled
                && crate::account::model_support::is_model_supported_by_account(acc, model_id)
        })
    }

    pub async fn select_account(&self, model_id: &str) -> (Option<Account>, u64) {
        let (acc, wait_ms, should_save) = {
            let mut inner = self.inner.write().await;
            let cur_idx = inner.current_index;
            let PoolInner {
                ref mut accounts,
                ref mut strategy,
                ..
            } = *inner;
            let res = strategy.select_account(accounts, model_id, cur_idx);
            inner.current_index = res.new_cursor;
            let acc = res.index.map(|i| inner.accounts[i].clone());
            (acc, res.wait_ms, res.index.is_some())
        };
        if should_save {
            self.save_to_disk().await;
        }
        (acc, wait_ms)
    }

    pub async fn notify_success(&self, email: &str) {
        let mut inner = self.inner.write().await;
        inner.strategy.on_success(email);
        if let Some(acc) = inner.accounts.iter_mut().find(|a| a.email == email) {
            acc.consecutive_failures = 0;
        }
    }

    pub async fn notify_rate_limit(&self, email: &str) {
        let mut inner = self.inner.write().await;
        inner.strategy.on_rate_limit(email);
    }

    pub async fn notify_failure(&self, email: &str) {
        let mut inner = self.inner.write().await;
        inner.strategy.on_failure(email);
    }

    pub async fn get_consecutive_failures(&self, email: &str) -> u32 {
        let inner = self.inner.read().await;
        inner
            .accounts
            .iter()
            .find(|a| a.email == email)
            .map(|a| a.consecutive_failures)
            .unwrap_or(0)
    }

    pub async fn increment_consecutive_failures(&self, email: &str) -> u32 {
        let mut inner = self.inner.write().await;
        if let Some(acc) = inner.accounts.iter_mut().find(|a| a.email == email) {
            acc.consecutive_failures += 1;
            acc.consecutive_failures
        } else {
            0
        }
    }

    pub async fn mark_rate_limited(&self, email: &str, reset_ms: Option<u64>, model_id: &str) {
        {
            let mut inner = self.inner.write().await;
            rl_mark_rate_limited(&mut inner.accounts, email, reset_ms, model_id);
        }
        self.save_to_disk().await;
    }

    pub async fn mark_invalid(&self, email: &str, reason: &str, verify_url: Option<String>) {
        {
            let mut inner = self.inner.write().await;
            rl_mark_invalid(&mut inner.accounts, email, reason, verify_url);
        }
        self.save_to_disk().await;
    }

    pub async fn clear_invalid(&self, email: &str) {
        {
            let mut inner = self.inner.write().await;
            crate::account::rate_limits::clear_invalid(&mut inner.accounts, email);
        }
        self.save_to_disk().await;
    }

    pub async fn mark_model_unsupported(&self, email: &str, model_id: &str) {
        let mut inner = self.inner.write().await;
        if let Some(acc) = inner.accounts.iter_mut().find(|a| a.email == email) {
            mark_account_model_unsupported(acc, model_id);
        }
    }

    pub async fn update_account_models_and_quota(
        &self,
        email: &str,
        supported: HashSet<String>,
        quotas: HashMap<String, crate::account::types::ModelQuota>,
    ) {
        let mut inner = self.inner.write().await;
        if let Some(acc) = inner.accounts.iter_mut().find(|a| a.email == email) {
            if !supported.is_empty() {
                for m in &supported {
                    acc.unsupported_models.remove(m);
                }
                acc.supported_models = supported;
            }
            if !quotas.is_empty() {
                acc.quota.models = quotas;
                acc.quota.last_checked = Some(now_ms());
            }
        }
    }

    pub async fn update_account_subscription_and_quota(
        &self,
        email: &str,
        sub: SubscriptionInfo,
        quotas: HashMap<String, crate::account::types::ModelQuota>,
    ) {
        {
            let mut inner = self.inner.write().await;
            if let Some(acc) = inner.accounts.iter_mut().find(|a| a.email == email) {
                acc.subscription = sub;
                acc.quota.models = quotas;
                acc.quota.last_checked = Some(now_ms());
            }
        }
    }

    pub async fn clear_token_cache(&self, email: Option<&str>) {
        let mut inner = self.inner.write().await;
        if let Some(e) = email {
            inner.token_cache.remove(e);
        } else {
            inner.token_cache.clear();
        }
    }

    pub async fn clear_project_cache(&self, email: Option<&str>) {
        let mut inner = self.inner.write().await;
        if let Some(e) = email {
            inner.project_cache.remove(e);
        } else {
            inner.project_cache.clear();
        }
    }

    pub async fn mark_account_cooling_down(
        &self,
        email: &str,
        cooldown_ms: u64,
        reason: CooldownReason,
    ) {
        let mut inner = self.inner.write().await;
        if let Some(acc) = inner.accounts.iter_mut().find(|a| a.email == email) {
            acc.cooling_down_until = Some(now_ms() + cooldown_ms);
            acc.cooldown_reason = Some(reason.as_str().to_string());
        }
    }

    pub async fn get_token_for_account(&self, account: &Account) -> Result<String, NovoidError> {
        let ttl = token_refresh_interval_ms();
        {
            let inner = self.inner.read().await;
            if let Some(cached) = inner.token_cache.get(&account.email) {
                if now_ms().saturating_sub(cached.extracted_at) < ttl {
                    return Ok(cached.token.clone());
                }
            }
        }

        let token = if account.source == "oauth" {
            if let Some(ref refresh_token) = account.refresh_token {
                match refresh_access_token(refresh_token).await {
                    Ok(tok) => {
                        if account.is_invalid {
                            self.clear_invalid(&account.email).await;
                        }
                        logger().success(format!(
                            "[AccountManager] Refreshed OAuth token for: {}",
                            account.email
                        ));
                        tok
                    }
                    Err((err_msg, is_net)) => {
                        if is_net {
                            logger().warn(format!(
                                "[AccountManager] Failed to refresh token for {} due to network error: {err_msg}",
                                account.email
                            ));
                            return Err(NovoidError::AuthNetwork { message: err_msg });
                        }
                        logger().error(format!(
                            "[AccountManager] Failed to refresh token for {}: {err_msg}",
                            account.email
                        ));
                        self.mark_invalid(&account.email, &err_msg, None).await;
                        return Err(NovoidError::AuthInvalid {
                            message: format!("{}: {err_msg}", account.email),
                            account_email: Some(account.email.clone()),
                        });
                    }
                }
            } else {
                let msg = "Account missing refreshToken; re-authentication required";
                self.mark_invalid(&account.email, msg, None).await;
                return Err(NovoidError::AuthInvalid {
                    message: format!("{}: {msg}", account.email),
                    account_email: Some(account.email.clone()),
                });
            }
        } else if account.source == "manual" {
            if let Some(ref key) = account.api_key {
                key.clone()
            } else {
                let msg = "Account missing apiKey";
                self.mark_invalid(&account.email, msg, None).await;
                return Err(NovoidError::AuthInvalid {
                    message: format!("{}: {msg}", account.email),
                    account_email: Some(account.email.clone()),
                });
            }
        } else {
            let msg = "Account missing refreshToken or apiKey; re-authentication required";
            self.mark_invalid(&account.email, msg, None).await;
            return Err(NovoidError::AuthInvalid {
                message: format!("{}: {msg}", account.email),
                account_email: Some(account.email.clone()),
            });
        };

        {
            let mut inner = self.inner.write().await;
            inner.token_cache.insert(
                account.email.clone(),
                CachedToken {
                    token: token.clone(),
                    extracted_at: now_ms(),
                },
            );
        }
        Ok(token)
    }

    pub async fn get_project_for_account(&self, account: &Account, token: &str) -> String {
        {
            let inner = self.inner.read().await;
            if let Some(pid) = inner.project_cache.get(&account.email) {
                return pid.clone();
            }
        }

        let parts = account
            .refresh_token
            .as_deref()
            .map(parse_refresh_parts);

        if let Some(ref p) = parts {
            if let Some(ref managed) = p.managed_project_id {
                {
                    let mut inner = self.inner.write().await;
                    inner
                        .project_cache
                        .insert(account.email.clone(), managed.clone());
                }
                if account.subscription.tier == "unknown" {
                    self.fetch_and_save_subscription(&account.email, token, account.project_id.as_deref())
                        .await;
                }
                return managed.clone();
            }
        }

        if let Some(ref pid) = account.project_id {
            {
                let mut inner = self.inner.write().await;
                inner
                    .project_cache
                    .insert(account.email.clone(), pid.clone());
            }
            if account.subscription.tier == "unknown" {
                self.fetch_and_save_subscription(&account.email, token, Some(pid))
                    .await;
            }
            return pid.clone();
        }

        let part_proj = parts.as_ref().and_then(|p| p.project_id.as_deref());
        let (project, subscription) = discover_project(token, part_proj).await;

        let mut needs_save = false;
        {
            let mut inner = self.inner.write().await;
            if let Some(acc) = inner.accounts.iter_mut().find(|a| a.email == account.email) {
                if project != DEFAULT_PROJECT_ID {
                    if let Some(ref p) = parts {
                        acc.refresh_token = Some(format_refresh_parts(
                            &p.refresh_token,
                            p.project_id.as_deref(),
                            Some(&project),
                        ));
                        needs_save = true;
                    } else if acc.source == "manual" {
                        acc.project_id = Some(project.clone());
                        needs_save = true;
                    }
                }
                if let Some(sub) = subscription {
                    acc.subscription = sub;
                    needs_save = true;
                }
            }
            inner
                .project_cache
                .insert(account.email.clone(), project.clone());
        }

        if needs_save {
            self.save_to_disk().await;
        }
        project
    }

    async fn fetch_and_save_subscription(
        &self,
        email: &str,
        token: &str,
        project_id: Option<&str>,
    ) {
        {
            let mut inner = self.inner.write().await;
            if !inner.subscription_in_flight.insert(email.to_string()) {
                return;
            }
        }

        let (_, subscription) = discover_project(token, project_id).await;
        let mut updated = false;

        {
            let mut inner = self.inner.write().await;
            inner.subscription_in_flight.remove(email);
            if let Some(sub) = subscription {
                if sub.tier != "unknown" {
                    if let Some(acc) = inner.accounts.iter_mut().find(|a| a.email == email) {
                        acc.subscription = sub;
                        updated = true;
                    }
                }
            }
        }

        if updated {
            self.save_to_disk().await;
        }
    }

    pub async fn get_status(&self) -> Value {
        let inner = self.inner.read().await;
        let total = inner.accounts.len();
        let available = get_available_accounts(&inner.accounts, None).len();
        let now = now_ms();
        let rate_limited = inner
            .accounts
            .iter()
            .filter(|a| {
                a.model_rate_limits
                    .values()
                    .any(|l| l.is_rate_limited && l.reset_time.unwrap_or(0) > now)
            })
            .count();
        let invalid = inner.accounts.iter().filter(|a| a.is_invalid).count();

        let accounts_json: Vec<Value> = inner
            .accounts
            .iter()
            .map(|a| {
                json!({
                    "email": a.email,
                    "source": a.source,
                    "enabled": a.enabled,
                    "projectId": a.project_id,
                    "modelRateLimits": a.model_rate_limits,
                    "isInvalid": a.is_invalid,
                    "invalidReason": a.invalid_reason,
                    "verifyUrl": a.verify_url,
                    "lastUsed": a.last_used,
                    "quotaThreshold": a.quota_threshold,
                    "modelQuotaThresholds": a.model_quota_thresholds,
                    "subscription": a.subscription
                })
            })
            .collect();

        json!({
            "total": total,
            "available": available,
            "rateLimited": rate_limited,
            "invalid": invalid,
            "summary": format!("{total} total, {available} available, {rate_limited} rate-limited, {invalid} invalid"),
            "accounts": accounts_json
        })
    }

    pub async fn get_strategy_health_data(&self) -> Value {
        let inner = self.inner.read().await;
        let strategy_name = &inner.strategy_name;
        let StrategyImpl::Hybrid(ref hybrid) = inner.strategy else {
            return json!({
                "strategy": strategy_name,
                "trackers": Value::Null
            });
        };

        let accounts: Vec<Value> = inner
            .accounts
            .iter()
            .filter(|a| a.enabled)
            .map(|a| {
                let hs = (hybrid.health.get_score(&a.email) * 10.0).round() / 10.0;
                let is_usable = hybrid.health.is_usable(&a.email);
                let failures = hybrid.health.get_consecutive_failures(&a.email);
                let tokens = (hybrid.token_bucket.get_tokens(&a.email) * 10.0).round() / 10.0;
                let has_tokens = hybrid.token_bucket.has_tokens(&a.email);
                let max_tokens = hybrid.token_bucket.get_max_tokens();

                json!({
                    "email": a.email,
                    "healthScore": hs,
                    "isUsable": is_usable,
                    "consecutiveFailures": failures,
                    "tokens": tokens,
                    "hasTokens": has_tokens,
                    "maxTokens": max_tokens
                })
            })
            .collect();

        json!({
            "strategy": strategy_name,
            "trackers": {
                "accounts": accounts
            }
        })
    }
}

async fn discover_project(
    token: &str,
    project_id: Option<&str>,
) -> (String, Option<SubscriptionInfo>) {
    let mut metadata = client_metadata();
    if let Some(pid) = project_id {
        if let Some(obj) = metadata.as_object_mut() {
            obj.insert("duetProject".to_string(), Value::String(pid.to_string()));
        }
    }

    let mut load_data = None;
    let mut got_ok = false;

    for endpoint in LOAD_CODE_ASSIST_ENDPOINTS {
        maybe_throttle().await;
        let resp = http_client()
            .post(format!("{endpoint}/v1internal:loadCodeAssist"))
            .bearer_auth(token)
            .headers(antigravity_headers())
            .json(&json!({ "metadata": metadata, "mode": 1 }))
            .send()
            .await;

        if let Ok(r) = resp {
            if !r.status().is_success() {
                continue;
            }
            if let Ok(data) = r.json::<Value>().await {
                got_ok = true;
                let sub = extract_subscription(&data);
                if let Some(p) = data
                    .get("cloudaicompanionProject")
                    .and_then(|v| v.as_str())
                {
                    return (p.to_string(), Some(sub));
                }
                if let Some(p) = data
                    .get("cloudaicompanionProject")
                    .and_then(|v| v.get("id"))
                    .and_then(|v| v.as_str())
                {
                    return (p.to_string(), Some(sub));
                }
                load_data = Some(data);
                break;
            }
        }
    }

    if got_ok {
        if let Some(ref data) = load_data {
            let tier_id = get_default_tier_id(data.get("allowedTiers"))
                .unwrap_or_else(|| "free-tier".to_string());
            if let Some(onboarded) = onboard_user(token, &tier_id, project_id).await {
                let sub = extract_subscription(data);
                return (onboarded, Some(sub));
            }
        }
    }

    if let Some(pid) = project_id {
        (pid.to_string(), None)
    } else {
        (DEFAULT_PROJECT_ID.to_string(), None)
    }
}

fn extract_subscription(data: &Value) -> SubscriptionInfo {
    let mut tier = "free".to_string();
    if let Some(id) = data
        .get("paidTier")
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
    {
        tier = parse_tier_id(id);
    } else if let Some(id) = data
        .get("currentTier")
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
    {
        tier = parse_tier_id(id);
    }

    let project_id = data
        .get("cloudaicompanionProject")
        .and_then(|v| {
            v.as_str()
                .map(|s| s.to_string())
                .or_else(|| v.get("id").and_then(|i| i.as_str()).map(|s| s.to_string()))
        });

    SubscriptionInfo {
        tier,
        project_id,
        detected_at: Some(Value::String(chrono::Utc::now().to_rfc3339())),
    }
}
