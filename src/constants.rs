use crate::cloudcode::model_registry::get_registered_model;
use crate::config::{config_dir, get_config};
use crate::logger::logger;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE, USER_AGENT};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::OnceLock;

pub const ANTIGRAVITY_ENDPOINT_DAILY: &str = "https://daily-cloudcode-pa.googleapis.com";
pub const ANTIGRAVITY_ENDPOINT_PROD: &str = "https://cloudcode-pa.googleapis.com";

pub const ANTIGRAVITY_ENDPOINT_FALLBACKS: [&str; 2] =
    [ANTIGRAVITY_ENDPOINT_DAILY, ANTIGRAVITY_ENDPOINT_PROD];

pub const LOAD_CODE_ASSIST_ENDPOINTS: [&str; 2] =
    [ANTIGRAVITY_ENDPOINT_PROD, ANTIGRAVITY_ENDPOINT_DAILY];

pub const ONBOARD_USER_ENDPOINTS: [&str; 2] = ANTIGRAVITY_ENDPOINT_FALLBACKS;

pub const DEFAULT_PROJECT_ID: &str = "rising-fact-p41fc";
pub const REQUEST_BODY_LIMIT_BYTES: usize = 50 * 1024 * 1024;
pub const DEFAULT_PORT: u16 = 8080;

pub const MAX_EMPTY_RESPONSE_RETRIES: usize = 1;
pub const RATE_LIMIT_STATE_RESET_MS: u64 = 120_000;
pub const FIRST_RETRY_DELAY_MS: u64 = 3_000;
pub const MIN_BACKOFF_MS: u64 = 5_000;
pub const CAPACITY_JITTER_MAX_MS: u64 = 10_000;
pub const MIN_SIGNATURE_LENGTH: usize = 50;
pub const GEMINI_MAX_OUTPUT_TOKENS: u64 = 65536;
pub const GEMINI_SKIP_SIGNATURE: &str = "skip_thought_signature_validator";
pub const MODEL_VALIDATION_CACHE_TTL_MS: u64 = 5 * 60 * 1000;

pub const QUOTA_EXHAUSTED_BACKOFF_TIERS_MS: [u64; 4] = [60_000, 300_000, 1_800_000, 7_200_000];

pub const BACKOFF_RATE_LIMIT_EXCEEDED_MS: u64 = 60_000;
pub const BACKOFF_MODEL_CAPACITY_EXHAUSTED_MS: u64 = 30_000;
pub const BACKOFF_SERVER_ERROR_MS: u64 = 45_000;
pub const BACKOFF_UNKNOWN_MS: u64 = 120_000;

pub const SELECTION_STRATEGIES: [&str; 3] = ["sticky", "round-robin", "hybrid"];
pub const DEFAULT_SELECTION_STRATEGY: &str = "hybrid";

pub const ANTIGRAVITY_SYSTEM_INSTRUCTION: &str = "You are Antigravity, a powerful agentic AI coding assistant designed by the Google Deepmind team working on Advanced Agentic Coding.You are pair programming with a USER to solve their coding task. The task may require creating a new codebase, modifying or debugging an existing codebase, or simply answering a question.**Absolute paths only****Proactiveness**";

pub const DEFAULT_OAUTH_CLIENT_ID: &str =
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
pub const DEFAULT_OAUTH_CLIENT_SECRET: &str = "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";
pub const OAUTH_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const OAUTH_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const OAUTH_USER_INFO_URL: &str = "https://www.googleapis.com/oauth2/v1/userinfo";
pub const OAUTH_CALLBACK_FALLBACK_PORTS: [u16; 5] = [51122, 51123, 51124, 51125, 51126];
pub const OAUTH_SCOPES: [&str; 5] = [
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
];

pub fn account_config_path() -> PathBuf {
    let cfg = get_config();
    if let Some(custom) = cfg.account_config_path {
        if !custom.trim().is_empty() {
            return PathBuf::from(custom);
        }
    }
    config_dir().join("accounts.json")
}

pub fn usage_history_path() -> PathBuf {
    config_dir().join("usage-history.json")
}

pub fn signature_store_path() -> PathBuf {
    if let Ok(custom) = std::env::var("ANTIGRAVITY_SIGNATURE_STORE") {
        if !custom.trim().is_empty() {
            return PathBuf::from(custom);
        }
    }
    config_dir().join("signature-store.json")
}

pub fn claude_presets_path() -> PathBuf {
    config_dir().join("claude-presets.json")
}

pub fn server_presets_path() -> PathBuf {
    config_dir().join("server-presets.json")
}

pub fn pid_file_path() -> PathBuf {
    config_dir().join("server.pid")
}

pub fn oauth_callback_port() -> u16 {
    std::env::var("OAUTH_CALLBACK_PORT")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(51121)
}

pub fn oauth_client_id() -> String {
    std::env::var("OAUTH_CLIENT_ID")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_OAUTH_CLIENT_ID.to_string())
}

pub fn oauth_client_secret() -> String {
    std::env::var("OAUTH_CLIENT_SECRET")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_OAUTH_CLIENT_SECRET.to_string())
}

pub fn oauth_has_custom_client() -> bool {
    let id_ok = std::env::var("OAUTH_CLIENT_ID")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let sec_ok = std::env::var("OAUTH_CLIENT_SECRET")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    id_ok && sec_ok
}

pub fn oauth_default_redirect_uri() -> String {
    std::env::var("OAUTH_REDIRECT_URI")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| format!("http://localhost:{}/oauth-callback", oauth_callback_port()))
}

pub fn token_refresh_interval_ms() -> u64 {
    get_config().token_cache_ttl_ms.unwrap_or(50 * 60 * 1000)
}

pub fn get_platform_enum() -> u32 {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => 2,
        ("macos", _) => 1,
        ("linux", "aarch64") => 4,
        ("linux", _) => 3,
        ("windows", _) => 5,
        _ => 0,
    }
}

pub fn client_metadata() -> Value {
    json!({
        "ideType": 9,
        "platform": get_platform_enum(),
        "pluginType": 2
    })
}

fn read_product_json() -> Option<Value> {
    let home = dirs::home_dir().unwrap_or_default();
    let candidates = if cfg!(target_os = "macos") {
        vec![
            PathBuf::from("/Applications/Antigravity.app/Contents/Resources/app/product.json"),
            home.join("Applications/Antigravity.app/Contents/Resources/app/product.json"),
        ]
    } else if cfg!(target_os = "windows") {
        let local_app = std::env::var("LOCALAPPDATA").unwrap_or_default();
        let prog_files =
            std::env::var("ProgramFiles").unwrap_or_else(|_| "C:\\Program Files".to_string());
        let mut v = Vec::new();
        if !local_app.is_empty() {
            v.push(
                PathBuf::from(local_app)
                    .join("Programs/Antigravity/resources/app/product.json"),
            );
        }
        v.push(PathBuf::from(prog_files).join("Antigravity/resources/app/product.json"));
        v
    } else {
        vec![
            PathBuf::from("/usr/share/antigravity/resources/app/product.json"),
            PathBuf::from("/opt/antigravity/resources/app/product.json"),
            PathBuf::from("/opt/Antigravity/resources/app/product.json"),
            home.join(".local/share/antigravity/resources/app/product.json"),
            PathBuf::from("/snap/antigravity/current/resources/app/product.json"),
        ]
    };

    for path in candidates {
        if path.exists() {
            if let Ok(raw) = std::fs::read_to_string(&path) {
                if let Ok(val) = serde_json::from_str::<Value>(&raw) {
                    if val.get("version").is_some() || val.get("ideVersion").is_some() {
                        return Some(val);
                    }
                }
            }
        }
    }
    None
}

pub fn get_client_version() -> String {
    static CACHED: OnceLock<String> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            let fallback = std::env::var("ANTIGRAVITY_CLIENT_VERSION_FALLBACK")
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "1.110.0".to_string());

            if let Ok(env_ver) = std::env::var("ANTIGRAVITY_CLIENT_VERSION") {
                if !env_ver.is_empty() {
                    return env_ver;
                }
            }
            let cfg_ver = get_config().antigravity_client_version;
            if !cfg_ver.is_empty() {
                return cfg_ver;
            }
            if let Some(prod) = read_product_json() {
                if let Some(v) = prod.get("version").and_then(|v| v.as_str()) {
                    if !v.is_empty() {
                        return v.to_string();
                    }
                }
            }
            logger().debug(format!(
                "X-Client-Version: using fallback {fallback} (no local product.json)"
            ));
            fallback
        })
        .clone()
}

pub fn get_platform_user_agent() -> String {
    static CACHED: OnceLock<String> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            let fallback = std::env::var("FALLBACK_ANTIGRAVITY_VERSION")
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "2.32.0".to_string());
            let cfg_ver = get_config().antigravity_ide_version;
            let version = if !std::env::var("FALLBACK_ANTIGRAVITY_VERSION")
                .unwrap_or_default()
                .is_empty()
            {
                fallback
            } else if !cfg_ver.is_empty() {
                cfg_ver
            } else if let Some(prod) = read_product_json() {
                prod.get("ideVersion")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .unwrap_or(fallback)
            } else {
                fallback
            };

            let os_name = match std::env::consts::OS {
                "macos" => "darwin",
                "windows" => "win32",
                _ => "linux",
            };
            let arch = match std::env::consts::ARCH {
                "x86_64" => "x64",
                "aarch64" => "arm64",
                other => other,
            };
            format!("antigravity/hub/{version} {os_name}/{arch}")
        })
        .clone()
}

pub fn antigravity_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Ok(ua) = HeaderValue::from_str(&get_platform_user_agent()) {
        headers.insert(USER_AGENT, ua);
    }
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert("X-Client-Name", HeaderValue::from_static("antigravity"));
    if let Ok(cv) = HeaderValue::from_str(&get_client_version()) {
        headers.insert("X-Client-Version", cv);
    }
    headers.insert(
        "x-goog-api-client",
        HeaderValue::from_static("gl-node/18.18.2 fire/0.8.6 grpc/1.10.x"),
    );
    headers
}

pub fn get_model_family(model_name: &str) -> String {
    let lower = model_name.to_lowercase();
    if lower.contains("claude") {
        return "claude".to_string();
    }
    if lower.contains("gemini") {
        return "gemini".to_string();
    }
    get_registered_model(model_name)
        .and_then(|m| m.family)
        .unwrap_or_else(|| "unknown".to_string())
}

pub fn is_thinking_model(model_name: &str) -> bool {
    let lower = model_name.to_lowercase();
    if lower.contains("image") {
        return false;
    }
    if lower.contains("claude") && lower.contains("thinking") {
        return true;
    }
    if lower.contains("gemini") {
        if lower.contains("thinking") {
            return true;
        }
        if let Some(idx) = lower.find("gemini-") {
            let rest = &lower[idx + 7..];
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(ver) = digits.parse::<u32>() {
                return ver >= 3;
            }
        }
        return false;
    }
    if lower.contains("claude") {
        return false;
    }
    get_registered_model(model_name)
        .map(|m| m.supports_thinking)
        .unwrap_or(false)
}

pub fn get_fallback_model(model: &str) -> Option<&'static str> {
    match model {
        "gemini-4-hydrogen-high" => Some("claude-opus-4-6-thinking"),
        "gemini-4-hydrogen-medium" => Some("claude-sonnet-4-6"),
        "gemini-3-flash" => Some("gemini-2.5-flash"),
        "gemini-2.5-flash" => Some("claude-sonnet-4-6"),
        "claude-opus-4-6-thinking" => Some("claude-sonnet-4-6"),
        "claude-sonnet-4-6" => Some("gemini-3-flash"),
        _ => None,
    }
}

pub fn resolve_builtin_model_mapping(model: &str) -> Option<&'static str> {
    match model {
        "claude-3-5-sonnet-20241022" => Some("claude-sonnet-4-6"),
        "claude-3-7-sonnet-20250219" => Some("claude-sonnet-4-6"),
        "claude-opus-5-5" => Some("claude-opus-4-6-thinking"),
        "claude-sonnet-5-5" => Some("claude-sonnet-4-6"),
        "claude-haiku-5-5" => Some("claude-sonnet-4-6"),
        "claude-opus-5" => Some("claude-opus-4-6-thinking"),
        "claude-sonnet-5" => Some("claude-sonnet-4-6"),
        _ => None,
    }
}

pub fn get_strategy_label(name: &str) -> &'static str {
    match name.to_lowercase().as_str() {
        "sticky" => "Sticky (Cache Optimized)",
        "round-robin" | "roundrobin" => "Round Robin (Load Balanced)",
        "hybrid" => "Hybrid (Smart Distribution)",
        _ => "Hybrid (Smart Distribution)",
    }
}

pub fn default_claude_presets() -> Value {
    json!([
        {
            "name": "Claude Thinking",
            "config": {
                "ANTHROPIC_AUTH_TOKEN": "test",
                "ANTHROPIC_BASE_URL": "http://localhost:8080",
                "ANTHROPIC_MODEL": "claude-opus-4-6-thinking",
                "ANTHROPIC_DEFAULT_OPUS_MODEL": "claude-opus-4-6-thinking",
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "claude-sonnet-4-6",
                "ANTHROPIC_DEFAULT_HAIKU_MODEL": "claude-sonnet-4-6",
                "CLAUDE_CODE_SUBAGENT_MODEL": "claude-sonnet-4-6",
                "ENABLE_EXPERIMENTAL_MCP_CLI": "true"
            }
        },
        {
            "name": "Gemini 1M",
            "config": {
                "ANTHROPIC_AUTH_TOKEN": "test",
                "ANTHROPIC_BASE_URL": "http://localhost:8080",
                "ANTHROPIC_MODEL": "gemini-3-flash",
                "ANTHROPIC_DEFAULT_OPUS_MODEL": "gemini-3-flash",
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "gemini-2.5-flash",
                "ANTHROPIC_DEFAULT_HAIKU_MODEL": "gemini-2.5-flash",
                "CLAUDE_CODE_SUBAGENT_MODEL": "gemini-2.5-flash",
                "ENABLE_EXPERIMENTAL_MCP_CLI": "true"
            }
        }
    ])
}

pub fn default_server_presets() -> Vec<Value> {
    vec![
        json!({
            "name": "Default (3-5 Accounts)",
            "builtIn": true,
            "descriptionKey": "presetDefaultDesc",
            "config": {
                "maxRetries": 2,
                "retryBaseMs": 3000,
                "retryMaxMs": 60000,
                "defaultCooldownMs": 30000,
                "maxWaitBeforeErrorMs": 120000,
                "maxAccounts": 10,
                "globalQuotaThreshold": 0,
                "rateLimitDedupWindowMs": 5000,
                "maxConsecutiveFailures": 2,
                "extendedCooldownMs": 180000,
                "maxCapacityRetries": 2,
                "switchAccountDelayMs": 8000,
                "capacityBackoffTiersMs": [10000, 20000, 40000, 60000],
                "accountSelection": {
                    "strategy": "hybrid",
                    "healthScore": {
                        "initial": 70,
                        "successReward": 1,
                        "rateLimitPenalty": -10,
                        "failurePenalty": -20,
                        "recoveryPerHour": 10,
                        "minUsable": 50,
                        "maxScore": 100
                    },
                    "tokenBucket": {
                        "maxTokens": 50,
                        "tokensPerMinute": 6,
                        "initialTokens": 50
                    },
                    "quota": {
                        "lowThreshold": 0.10,
                        "criticalThreshold": 0.05,
                        "staleMs": 300000
                    },
                    "weights": {
                        "health": 2,
                        "tokens": 5,
                        "quota": 3,
                        "lru": 0.1
                    }
                }
            }
        }),
        json!({
            "name": "Many Accounts (10+)",
            "builtIn": true,
            "descriptionKey": "presetManyAccountsDesc",
            "config": {
                "maxRetries": 2,
                "retryBaseMs": 2000,
                "retryMaxMs": 30000,
                "defaultCooldownMs": 20000,
                "maxWaitBeforeErrorMs": 60000,
                "maxAccounts": 50,
                "globalQuotaThreshold": 0.10,
                "rateLimitDedupWindowMs": 3000,
                "maxConsecutiveFailures": 2,
                "extendedCooldownMs": 120000,
                "maxCapacityRetries": 1,
                "switchAccountDelayMs": 5000,
                "capacityBackoffTiersMs": [8000, 15000, 30000, 60000],
                "accountSelection": {
                    "strategy": "hybrid",
                    "healthScore": {
                        "initial": 70,
                        "successReward": 1,
                        "rateLimitPenalty": -15,
                        "failurePenalty": -25,
                        "recoveryPerHour": 5,
                        "minUsable": 40,
                        "maxScore": 100
                    },
                    "tokenBucket": {
                        "maxTokens": 30,
                        "tokensPerMinute": 8,
                        "initialTokens": 30
                    },
                    "quota": {
                        "lowThreshold": 0.15,
                        "criticalThreshold": 0.05,
                        "staleMs": 180000
                    },
                    "weights": {
                        "health": 5,
                        "tokens": 2,
                        "quota": 3,
                        "lru": 0.01
                    }
                }
            }
        }),
        json!({
            "name": "Conservative",
            "builtIn": true,
            "descriptionKey": "presetConservativeDesc",
            "config": {
                "maxRetries": 3,
                "retryBaseMs": 5000,
                "retryMaxMs": 90000,
                "defaultCooldownMs": 60000,
                "maxWaitBeforeErrorMs": 240000,
                "maxAccounts": 10,
                "globalQuotaThreshold": 0.20,
                "rateLimitDedupWindowMs": 8000,
                "maxConsecutiveFailures": 2,
                "extendedCooldownMs": 300000,
                "maxCapacityRetries": 3,
                "switchAccountDelayMs": 12000,
                "capacityBackoffTiersMs": [15000, 30000, 60000, 90000, 120000],
                "accountSelection": {
                    "strategy": "sticky",
                    "healthScore": {
                        "initial": 80,
                        "successReward": 2,
                        "rateLimitPenalty": -5,
                        "failurePenalty": -10,
                        "recoveryPerHour": 3,
                        "minUsable": 50,
                        "maxScore": 100
                    },
                    "tokenBucket": {
                        "maxTokens": 80,
                        "tokensPerMinute": 4,
                        "initialTokens": 80
                    },
                    "quota": {
                        "lowThreshold": 0.20,
                        "criticalThreshold": 0.10,
                        "staleMs": 300000
                    },
                    "weights": {
                        "health": 3,
                        "tokens": 4,
                        "quota": 2,
                        "lru": 0.05
                    }
                }
            }
        }),
    ]
}
