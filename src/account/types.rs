use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelRateLimit {
    #[serde(default)]
    pub is_rate_limited: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_time: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_reset_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marked_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionInfo {
    #[serde(default = "default_tier")]
    pub tier: String,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub detected_at: Option<Value>,
}

fn default_tier() -> String {
    "unknown".to_string()
}

impl Default for SubscriptionInfo {
    fn default() -> Self {
        Self {
            tier: default_tier(),
            project_id: None,
            detected_at: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelQuota {
    #[serde(default)]
    pub remaining_fraction: Option<f64>,
    #[serde(default)]
    pub reset_time: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct QuotaInfo {
    #[serde(default)]
    pub models: HashMap<String, ModelQuota>,
    #[serde(default)]
    pub last_checked: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub email: String,
    #[serde(default = "default_source")]
    pub source: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db_path: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_at: Option<String>,
    #[serde(default)]
    pub is_invalid: bool,
    #[serde(default)]
    pub invalid_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid_at: Option<u64>,
    #[serde(default)]
    pub verify_url: Option<String>,
    #[serde(default)]
    pub model_rate_limits: HashMap<String, ModelRateLimit>,
    #[serde(default)]
    pub last_used: Option<u64>,
    #[serde(default)]
    pub subscription: SubscriptionInfo,
    #[serde(default)]
    pub quota: QuotaInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_threshold: Option<f64>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub model_quota_thresholds: HashMap<String, f64>,

    // Runtime-only fields
    #[serde(skip)]
    pub consecutive_failures: u32,
    #[serde(skip)]
    pub cooling_down_until: Option<u64>,
    #[serde(skip)]
    pub cooldown_reason: Option<String>,
    #[serde(skip)]
    pub supported_models: HashSet<String>,
    #[serde(skip)]
    pub unsupported_models: HashMap<String, u64>,
}

fn default_source() -> String {
    "oauth".to_string()
}

fn default_true() -> bool {
    true
}

impl Default for Account {
    fn default() -> Self {
        Self {
            email: String::new(),
            source: default_source(),
            enabled: true,
            db_path: None,
            refresh_token: None,
            api_key: None,
            project_id: None,
            added_at: None,
            is_invalid: false,
            invalid_reason: None,
            invalid_at: None,
            verify_url: None,
            model_rate_limits: HashMap::new(),
            last_used: None,
            subscription: SubscriptionInfo::default(),
            quota: QuotaInfo::default(),
            quota_threshold: None,
            model_quota_thresholds: HashMap::new(),
            consecutive_failures: 0,
            cooling_down_until: None,
            cooldown_reason: None,
            supported_models: HashSet::new(),
            unsupported_models: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AccountsFile {
    #[serde(default)]
    pub accounts: Vec<Account>,
    #[serde(default)]
    pub settings: HashMap<String, Value>,
    #[serde(default)]
    pub active_index: usize,
}
