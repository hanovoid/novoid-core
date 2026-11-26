use crate::logger::logger;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthScoreConfig {
    #[serde(default = "default_hs_initial")]
    pub initial: f64,
    #[serde(default = "default_hs_success")]
    pub success_reward: f64,
    #[serde(default = "default_hs_rate_limit")]
    pub rate_limit_penalty: f64,
    #[serde(default = "default_hs_failure")]
    pub failure_penalty: f64,
    #[serde(default = "default_hs_recovery")]
    pub recovery_per_hour: f64,
    #[serde(default = "default_hs_min_usable")]
    pub min_usable: f64,
    #[serde(default = "default_hs_max")]
    pub max_score: f64,
}

fn default_hs_initial() -> f64 { 70.0 }
fn default_hs_success() -> f64 { 1.0 }
fn default_hs_rate_limit() -> f64 { -10.0 }
fn default_hs_failure() -> f64 { -20.0 }
fn default_hs_recovery() -> f64 { 10.0 }
fn default_hs_min_usable() -> f64 { 50.0 }
fn default_hs_max() -> f64 { 100.0 }

impl Default for HealthScoreConfig {
    fn default() -> Self {
        Self {
            initial: default_hs_initial(),
            success_reward: default_hs_success(),
            rate_limit_penalty: default_hs_rate_limit(),
            failure_penalty: default_hs_failure(),
            recovery_per_hour: default_hs_recovery(),
            min_usable: default_hs_min_usable(),
            max_score: default_hs_max(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenBucketConfig {
    #[serde(default = "default_tb_max")]
    pub max_tokens: f64,
    #[serde(default = "default_tb_rate")]
    pub tokens_per_minute: f64,
    #[serde(default = "default_tb_initial")]
    pub initial_tokens: f64,
}

fn default_tb_max() -> f64 { 50.0 }
fn default_tb_rate() -> f64 { 6.0 }
fn default_tb_initial() -> f64 { 50.0 }

impl Default for TokenBucketConfig {
    fn default() -> Self {
        Self {
            max_tokens: default_tb_max(),
            tokens_per_minute: default_tb_rate(),
            initial_tokens: default_tb_initial(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaConfig {
    #[serde(default = "default_q_low")]
    pub low_threshold: f64,
    #[serde(default = "default_q_critical")]
    pub critical_threshold: f64,
    #[serde(default = "default_q_stale")]
    pub stale_ms: u64,
}

fn default_q_low() -> f64 { 0.10 }
fn default_q_critical() -> f64 { 0.05 }
fn default_q_stale() -> u64 { 300_000 }

impl Default for QuotaConfig {
    fn default() -> Self {
        Self {
            low_threshold: default_q_low(),
            critical_threshold: default_q_critical(),
            stale_ms: default_q_stale(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WeightsConfig {
    #[serde(default = "default_w_health")]
    pub health: f64,
    #[serde(default = "default_w_tokens")]
    pub tokens: f64,
    #[serde(default = "default_w_quota")]
    pub quota: f64,
    #[serde(default = "default_w_lru")]
    pub lru: f64,
}

fn default_w_health() -> f64 { 2.0 }
fn default_w_tokens() -> f64 { 5.0 }
fn default_w_quota() -> f64 { 3.0 }
fn default_w_lru() -> f64 { 0.1 }

impl Default for WeightsConfig {
    fn default() -> Self {
        Self {
            health: default_w_health(),
            tokens: default_w_tokens(),
            quota: default_w_quota(),
            lru: default_w_lru(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSelectionConfig {
    #[serde(default = "default_strategy")]
    pub strategy: String,
    #[serde(default)]
    pub health_score: HealthScoreConfig,
    #[serde(default)]
    pub token_bucket: TokenBucketConfig,
    #[serde(default)]
    pub quota: QuotaConfig,
    #[serde(default)]
    pub weights: WeightsConfig,
}

fn default_strategy() -> String { "hybrid".to_string() }

impl Default for AccountSelectionConfig {
    fn default() -> Self {
        Self {
            strategy: default_strategy(),
            health_score: HealthScoreConfig::default(),
            token_bucket: TokenBucketConfig::default(),
            quota: QuotaConfig::default(),
            weights: WeightsConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelConfigEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mapping: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub webui_password: String,
    #[serde(default)]
    pub debug: bool,
    #[serde(default)]
    pub dev_mode: bool,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default = "default_max_retries")]
    pub max_retries: usize,
    #[serde(default = "default_retry_base_ms")]
    pub retry_base_ms: u64,
    #[serde(default = "default_retry_max_ms")]
    pub retry_max_ms: u64,
    #[serde(default)]
    pub persist_token_cache: bool,
    #[serde(default = "default_cooldown_ms")]
    pub default_cooldown_ms: u64,
    #[serde(default = "default_max_wait_ms")]
    pub max_wait_before_error_ms: u64,
    #[serde(default = "default_max_accounts")]
    pub max_accounts: usize,
    #[serde(default)]
    pub global_quota_threshold: f64,
    #[serde(default)]
    pub request_throttling_enabled: bool,
    #[serde(default = "default_request_delay_ms")]
    pub request_delay_ms: u64,
    #[serde(default = "default_dedup_window_ms")]
    pub rate_limit_dedup_window_ms: u64,
    #[serde(default = "default_max_consecutive_failures")]
    pub max_consecutive_failures: u32,
    #[serde(default = "default_extended_cooldown_ms")]
    pub extended_cooldown_ms: u64,
    #[serde(default = "default_max_capacity_retries")]
    pub max_capacity_retries: usize,
    #[serde(default = "default_switch_account_delay_ms")]
    pub switch_account_delay_ms: u64,
    #[serde(default = "default_capacity_tiers")]
    pub capacity_backoff_tiers_ms: Vec<u64>,
    #[serde(default)]
    pub model_mapping: HashMap<String, ModelConfigEntry>,
    #[serde(default)]
    pub antigravity_client_version: String,
    #[serde(default)]
    pub antigravity_ide_version: String,
    #[serde(default)]
    pub account_selection: AccountSelectionConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_cache_ttl_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_config_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_retry_delay_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_state_reset_ms: Option<u64>,
}

fn default_log_level() -> String { "info".to_string() }
fn default_max_retries() -> usize { 2 }
fn default_retry_base_ms() -> u64 { 3000 }
fn default_retry_max_ms() -> u64 { 60000 }
fn default_cooldown_ms() -> u64 { 30000 }
fn default_max_wait_ms() -> u64 { 120_000 }
fn default_max_accounts() -> usize { 10 }
fn default_request_delay_ms() -> u64 { 200 }
fn default_dedup_window_ms() -> u64 { 5000 }
fn default_max_consecutive_failures() -> u32 { 2 }
fn default_extended_cooldown_ms() -> u64 { 180_000 }
fn default_max_capacity_retries() -> usize { 2 }
fn default_switch_account_delay_ms() -> u64 { 8000 }
fn default_capacity_tiers() -> Vec<u64> { vec![10_000, 20_000, 40_000, 60_000] }

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            webui_password: String::new(),
            debug: false,
            dev_mode: false,
            log_level: default_log_level(),
            max_retries: default_max_retries(),
            retry_base_ms: default_retry_base_ms(),
            retry_max_ms: default_retry_max_ms(),
            persist_token_cache: false,
            default_cooldown_ms: default_cooldown_ms(),
            max_wait_before_error_ms: default_max_wait_ms(),
            max_accounts: default_max_accounts(),
            global_quota_threshold: 0.0,
            request_throttling_enabled: false,
            request_delay_ms: default_request_delay_ms(),
            rate_limit_dedup_window_ms: default_dedup_window_ms(),
            max_consecutive_failures: default_max_consecutive_failures(),
            extended_cooldown_ms: default_extended_cooldown_ms(),
            max_capacity_retries: default_max_capacity_retries(),
            switch_account_delay_ms: default_switch_account_delay_ms(),
            capacity_backoff_tiers_ms: default_capacity_tiers(),
            model_mapping: HashMap::new(),
            antigravity_client_version: String::new(),
            antigravity_ide_version: String::new(),
            account_selection: AccountSelectionConfig::default(),
            port: None,
            token_cache_ttl_ms: None,
            account_config_path: None,
            first_retry_delay_ms: None,
            rate_limit_state_reset_ms: None,
        }
    }
}

pub fn config_dir() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".config").join("novoid-core")
}

pub fn config_file_path() -> PathBuf {
    config_dir().join("config.json")
}

fn ensure_config_dir() {
    let dir = config_dir();
    if !dir.exists() {
        if let Err(err) = fs::create_dir_all(&dir) {
            logger().warn(format!(
                "[Config] Could not create config directory {}: {err}",
                dir.display()
            ));
        }
    }
}

fn load_initial_config() -> AppConfig {
    ensure_config_dir();

    let mut cfg = AppConfig::default();
    let global_file = config_file_path();
    let local_file = PathBuf::from("config.json");

    let target = if global_file.exists() {
        Some(global_file)
    } else if local_file.exists() {
        Some(local_file)
    } else {
        None
    };

    if let Some(path) = target {
        match fs::read_to_string(&path) {
            Ok(content) => {
                if let Ok(user_val) = serde_json::from_str::<Value>(&content) {
                    if let Ok(mut base_val) = serde_json::to_value(&cfg) {
                        deep_merge(&mut base_val, &user_val);
                        if let Ok(parsed) = serde_json::from_value::<AppConfig>(base_val) {
                            cfg = parsed;
                        }
                    }
                }
            }
            Err(err) => {
                logger().error(format!("[Config] Error loading config: {err}"));
            }
        }
    }

    if let Ok(v) = std::env::var("API_KEY") {
        if !v.is_empty() {
            cfg.api_key = v;
        }
    }
    if let Ok(v) = std::env::var("WEBUI_PASSWORD") {
        if !v.is_empty() {
            cfg.webui_password = v;
        }
    }
    if std::env::var("DEBUG").as_deref() == Ok("true") {
        cfg.debug = true;
    }
    if std::env::var("DEV_MODE").as_deref() == Ok("true") {
        cfg.dev_mode = true;
    }
    if cfg.debug && !cfg.dev_mode {
        cfg.dev_mode = true;
    }

    cfg
}

pub fn deep_merge(target: &mut Value, source: &Value) {
    if let (Some(t_map), Some(s_map)) = (target.as_object_mut(), source.as_object()) {
        for (k, v) in s_map {
            if k == "__proto__" || k == "constructor" || k == "prototype" {
                continue;
            }
            if v.is_object() {
                if let Some(existing) = t_map.get_mut(k) {
                    if existing.is_object() {
                        deep_merge(existing, v);
                        continue;
                    }
                }
            }
            t_map.insert(k.clone(), v.clone());
        }
    }
}

static CONFIG: OnceLock<RwLock<AppConfig>> = OnceLock::new();

fn config_lock() -> &'static RwLock<AppConfig> {
    CONFIG.get_or_init(|| RwLock::new(load_initial_config()))
}

pub fn get_config() -> AppConfig {
    config_lock()
        .read()
        .map(|g| g.clone())
        .unwrap_or_default()
}

pub fn set_debug_flags(debug: bool, dev_mode: bool) {
    if let Ok(mut guard) = config_lock().write() {
        guard.debug = debug;
        guard.dev_mode = dev_mode;
    }
}

pub fn get_public_config() -> Value {
    let mut cfg = get_config();
    if !cfg.webui_password.is_empty() {
        cfg.webui_password = "********".to_string();
    }
    if !cfg.api_key.is_empty() {
        cfg.api_key = "********".to_string();
    }
    serde_json::to_value(&cfg).unwrap_or_else(|_| Value::Object(Default::default()))
}

pub fn save_config_updates(updates: &Value) -> Result<(), String> {
    ensure_config_dir();
    let mut guard = config_lock()
        .write()
        .map_err(|e| format!("Config lock poisoned: {e}"))?;

    let mut current_val =
        serde_json::to_value(&*guard).map_err(|e| format!("Serialize config failed: {e}"))?;
    deep_merge(&mut current_val, updates);

    let updated: AppConfig = serde_json::from_value(current_val.clone())
        .map_err(|e| format!("Invalid config update: {e}"))?;

    let formatted = serde_json::to_string_pretty(&updated)
        .map_err(|e| format!("Serialize config failed: {e}"))?;

    let target = config_file_path();
    let tmp = target.with_extension("json.tmp");
    fs::write(&tmp, &formatted).map_err(|e| format!("Write config failed: {e}"))?;
    fs::rename(&tmp, &target).map_err(|e| format!("Rename config failed: {e}"))?;

    *guard = updated;
    Ok(())
}
