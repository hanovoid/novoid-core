use crate::account::oauth::{
    complete_oauth_flow, extract_code_from_input, get_authorization_url, start_callback_server,
};
use crate::account::storage::{load_accounts, save_accounts};
use crate::account::types::Account;
use crate::config::{get_config, get_public_config, save_config_updates};
use crate::constants::{
    account_config_path, default_claude_presets, oauth_callback_port,
    oauth_default_redirect_uri, oauth_has_custom_client,
};
use crate::logger::logger;
use crate::server::AppState;
use crate::utils::{claude_config, now_ms, server_presets, usage_stats};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use futures_util::Stream;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::oneshot;

pub struct PendingOAuthFlow {
    pub verifier: String,
    pub state: String,
    pub redirect_uri: String,
    pub timestamp: u64,
    pub abort_tx: Option<oneshot::Sender<()>>,
}

#[derive(Clone)]
pub struct CompletedOAuthFlow {
    pub status: String,
    pub email: Option<String>,
    pub error: Option<String>,
    pub timestamp: u64,
}

#[derive(Default)]
pub struct OAuthFlowStore {
    pub pending: HashMap<String, PendingOAuthFlow>,
    pub completed: HashMap<String, CompletedOAuthFlow>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/accounts", get(list_accounts))
        .route("/api/accounts/reload", post(reload_accounts))
        .route("/api/accounts/export", get(export_accounts))
        .route("/api/accounts/import", post(import_accounts))
        .route("/api/accounts/:email/refresh", post(refresh_account))
        .route("/api/accounts/:email/toggle", post(toggle_account))
        .route(
            "/api/accounts/:email",
            delete(delete_account).patch(patch_account),
        )
        .route("/api/config", get(get_api_config).post(post_api_config))
        .route("/api/config/password", post(post_change_password))
        .route("/api/settings", get(get_runtime_settings))
        .route(
            "/api/claude/config",
            get(get_claude_cfg).post(post_claude_cfg),
        )
        .route("/api/claude/config/restore", post(restore_claude_cfg))
        .route(
            "/api/claude/mode",
            get(get_claude_mode).post(post_claude_mode),
        )
        .route(
            "/api/claude/presets",
            get(get_claude_presets).post(post_claude_preset),
        )
        .route("/api/claude/presets/:name", delete(delete_claude_preset))
        .route(
            "/api/server/presets",
            get(get_server_presets).post(post_server_preset),
        )
        .route(
            "/api/server/presets/:name",
            patch(patch_server_preset).delete(delete_server_preset),
        )
        .route("/api/models/config", post(post_model_config))
        .route("/api/logs", get(get_logs))
        .route("/api/logs/stream", get(stream_logs))
        .route("/api/strategy/health", get(get_strategy_health))
        .route("/api/stats/history", get(get_stats_history))
        .route("/api/auth/url", get(get_auth_url))
        .route("/api/auth/status", get(get_auth_status))
        .route("/api/auth/complete", post(post_auth_complete))
        .route("/oauth-callback", get(oauth_callback_handler))
}

async fn add_or_update_account(
    email: String,
    refresh_token: Option<String>,
    api_key: Option<String>,
    project_id: Option<String>,
    source: &str,
) -> Result<(), String> {
    let path = account_config_path();
    let mut loaded = load_accounts(&path).await;
    let max_accounts = get_config().max_accounts;

    if let Some(existing) = loaded.accounts.iter_mut().find(|a| a.email == email) {
        existing.source = source.to_string();
        if let Some(rt) = refresh_token {
            existing.refresh_token = Some(rt);
        }
        if let Some(ak) = api_key {
            existing.api_key = Some(ak);
        }
        if let Some(pid) = project_id {
            existing.project_id = Some(pid);
        }
        existing.enabled = true;
        existing.is_invalid = false;
        existing.invalid_reason = None;
        existing.verify_url = None;
        if existing.added_at.is_none() {
            existing.added_at = Some(chrono::Utc::now().to_rfc3339());
        }
        logger().info(format!("[WebUI] Account {email} updated"));
    } else {
        if loaded.accounts.len() >= max_accounts {
            return Err(format!(
                "Maximum of {max_accounts} accounts reached. Update maxAccounts in config to increase the limit."
            ));
        }
        let mut acc = Account::default();
        acc.email = email.clone();
        acc.source = source.to_string();
        acc.refresh_token = refresh_token;
        acc.api_key = api_key;
        acc.project_id = project_id;
        acc.enabled = true;
        acc.added_at = Some(chrono::Utc::now().to_rfc3339());
        loaded.accounts.push(acc);
        logger().info(format!("[WebUI] Account {email} added"));
    }

    save_accounts(&path, &loaded.accounts, &loaded.settings, loaded.active_index).await;
    Ok(())
}

async fn list_accounts(State(state): State<AppState>) -> Json<Value> {
    state.account_manager.ensure_initialized().await;
    let status = state.account_manager.get_status().await;
    Json(json!({
        "status": "ok",
        "accounts": status["accounts"],
        "summary": {
            "total": status["total"],
            "available": status["available"],
            "rateLimited": status["rateLimited"],
            "invalid": status["invalid"]
        }
    }))
}

async fn refresh_account(
    State(state): State<AppState>,
    Path(email): Path<String>,
) -> Json<Value> {
    state.account_manager.ensure_initialized().await;
    state.account_manager.clear_token_cache(Some(&email)).await;
    state.account_manager.clear_project_cache(Some(&email)).await;
    crate::server::invalidate_limits_cache().await;

    let all = state.account_manager.get_all_accounts().await;
    if let Some(acc) = all.iter().find(|a| a.email == email) {
        if acc.is_invalid && acc.verify_url.is_some() {
            state.account_manager.clear_invalid(&email).await;
        }
    }

    Json(json!({
        "status": "ok",
        "message": format!("Token cache cleared for {email}")
    }))
}

async fn toggle_account(
    State(state): State<AppState>,
    Path(email): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let Some(enabled) = body.get("enabled").and_then(|v| v.as_bool()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "status": "error", "error": "enabled must be a boolean" })),
        )
            .into_response();
    };

    let path = account_config_path();
    let mut loaded = load_accounts(&path).await;
    let Some(acc) = loaded.accounts.iter_mut().find(|a| a.email == email) else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "status": "error", "error": format!("Account {email} not found") })),
        )
            .into_response();
    };
    acc.enabled = enabled;
    save_accounts(&path, &loaded.accounts, &loaded.settings, loaded.active_index).await;
    state.account_manager.reload().await;
    crate::server::invalidate_limits_cache().await;

    Json(json!({
        "status": "ok",
        "message": format!("Account {email} {}", if enabled { "enabled" } else { "disabled" })
    }))
    .into_response()
}

async fn delete_account(
    State(state): State<AppState>,
    Path(email): Path<String>,
) -> Response {
    let path = account_config_path();
    let mut loaded = load_accounts(&path).await;
    let Some(idx) = loaded.accounts.iter().position(|a| a.email == email) else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "status": "error", "error": format!("Account {email} not found") })),
        )
            .into_response();
    };
    loaded.accounts.remove(idx);
    let new_idx = if loaded.active_index >= loaded.accounts.len() && !loaded.accounts.is_empty() {
        loaded.accounts.len() - 1
    } else if loaded.accounts.is_empty() {
        0
    } else {
        loaded.active_index
    };
    save_accounts(&path, &loaded.accounts, &loaded.settings, new_idx).await;
    state.account_manager.reload().await;
    crate::server::invalidate_limits_cache().await;

    Json(json!({
        "status": "ok",
        "message": format!("Account {email} removed")
    }))
    .into_response()
}

async fn patch_account(
    State(state): State<AppState>,
    Path(email): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let path = account_config_path();
    let mut loaded = load_accounts(&path).await;
    let Some(acc) = loaded.accounts.iter_mut().find(|a| a.email == email) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "status": "error", "error": format!("Account {email} not found") })),
        )
            .into_response();
    };

    if let Some(qt_val) = body.get("quotaThreshold") {
        if qt_val.is_null() {
            acc.quota_threshold = None;
        } else if let Some(n) = qt_val.as_f64() {
            if (0.0..1.0).contains(&n) {
                acc.quota_threshold = Some(n);
            } else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "status": "error", "error": "quotaThreshold must be 0-0.99 or null" })),
                )
                    .into_response();
            }
        } else {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "status": "error", "error": "quotaThreshold must be 0-0.99 or null" })),
            )
                .into_response();
        }
    }

    if let Some(mqt_val) = body.get("modelQuotaThresholds") {
        if mqt_val.is_null() {
            acc.model_quota_thresholds.clear();
        } else if let Some(obj) = mqt_val.as_object() {
            if obj.is_empty() {
                acc.model_quota_thresholds.clear();
            } else {
                let mut next = HashMap::new();
                for (mid, tv) in obj {
                    let Some(n) = tv.as_f64() else {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(json!({ "status": "error", "error": format!("Invalid threshold for model {mid}: must be 0-0.99") })),
                        )
                            .into_response();
                    };
                    if !(0.0..1.0).contains(&n) {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(json!({ "status": "error", "error": format!("Invalid threshold for model {mid}: must be 0-0.99") })),
                        )
                            .into_response();
                    }
                    next.insert(mid.clone(), n);
                }
                acc.model_quota_thresholds = next;
            }
        } else {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "status": "error", "error": "modelQuotaThresholds must be an object or null" })),
            )
                .into_response();
        }
    }

    let resp_acc = json!({
        "email": acc.email,
        "quotaThreshold": acc.quota_threshold,
        "modelQuotaThresholds": acc.model_quota_thresholds
    });
    save_accounts(&path, &loaded.accounts, &loaded.settings, loaded.active_index).await;
    state.account_manager.reload().await;
    crate::server::invalidate_limits_cache().await;

    Json(json!({
        "status": "ok",
        "message": format!("Account {email} thresholds updated"),
        "account": resp_acc
    }))
    .into_response()
}

async fn reload_accounts(State(state): State<AppState>) -> Json<Value> {
    state.account_manager.reload().await;
    crate::server::invalidate_limits_cache().await;
    let status = state.account_manager.get_status().await;
    Json(json!({
        "status": "ok",
        "message": "Accounts reloaded from disk",
        "summary": status["summary"]
    }))
}

async fn export_accounts() -> Json<Value> {
    let loaded = load_accounts(&account_config_path()).await;
    let exported: Vec<Value> = loaded
        .accounts
        .into_iter()
        .filter(|a| a.source != "database")
        .map(|a| {
            let mut obj = Map::new();
            obj.insert("email".to_string(), Value::String(a.email));
            if let Some(rt) = a.refresh_token {
                obj.insert("refresh_token".to_string(), Value::String(rt));
            }
            if let Some(ak) = a.api_key {
                obj.insert("api_key".to_string(), Value::String(ak));
            }
            if let Some(pid) = a.project_id {
                obj.insert("projectId".to_string(), Value::String(pid));
            }
            if a.subscription.tier != "unknown" || a.subscription.project_id.is_some() {
                if let Ok(sub_val) = serde_json::to_value(&a.subscription) {
                    obj.insert("subscription".to_string(), sub_val);
                }
            }
            Value::Object(obj)
        })
        .collect();
    Json(Value::Array(exported))
}

async fn import_accounts(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Response {
    let import_list = if let Some(arr) = body.as_array() {
        arr.clone()
    } else if let Some(arr) = body.get("accounts").and_then(|v| v.as_array()) {
        arr.clone()
    } else {
        Vec::new()
    };

    if import_list.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "status": "error", "error": "accounts must be a non-empty array" })),
        )
            .into_response();
    }

    let path = account_config_path();
    let mut loaded = load_accounts(&path).await;
    let max_accounts = get_config().max_accounts;

    let mut added = Vec::new();
    let mut updated = Vec::new();
    let mut failed = Vec::new();

    for acc in import_list {
        let Some(email) = acc.get("email").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) else {
            failed.push(json!({ "email": "unknown", "reason": "Missing email" }));
            continue;
        };
        let rt = acc
            .get("refresh_token")
            .or_else(|| acc.get("refreshToken"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let ak = acc
            .get("api_key")
            .or_else(|| acc.get("apiKey"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        if rt.is_none() && ak.is_none() {
            failed.push(json!({ "email": email, "reason": "Missing refresh_token or api_key" }));
            continue;
        }

        let pid = acc
            .get("project_id")
            .or_else(|| acc.get("projectId"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let sub = acc
            .get("subscription")
            .and_then(|v| serde_json::from_value::<crate::account::types::SubscriptionInfo>(v.clone()).ok());
        let enabled = acc.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
        let source = if ak.is_some() { "manual" } else { "oauth" };

        if let Some(existing) = loaded.accounts.iter_mut().find(|a| a.email == email) {
            existing.source = source.to_string();
            if let Some(rt_val) = rt {
                existing.refresh_token = Some(rt_val);
            }
            if let Some(ak_val) = ak {
                existing.api_key = Some(ak_val);
            }
            if let Some(pid_val) = pid {
                existing.project_id = Some(pid_val);
            }
            if let Some(sub_val) = sub {
                existing.subscription = sub_val;
            }
            existing.enabled = enabled;
            existing.is_invalid = false;
            existing.invalid_reason = None;
            existing.verify_url = None;
            if existing.added_at.is_none() {
                existing.added_at = Some(chrono::Utc::now().to_rfc3339());
            }
            updated.push(email.to_string());
            logger().info(format!("[WebUI] Account {email} updated"));
        } else {
            if loaded.accounts.len() >= max_accounts {
                failed.push(json!({
                    "email": email,
                    "reason": format!("Maximum of {max_accounts} accounts reached. Update maxAccounts in config to increase the limit.")
                }));
                continue;
            }
            let mut new_acc = Account::default();
            new_acc.email = email.to_string();
            new_acc.source = source.to_string();
            new_acc.refresh_token = rt;
            new_acc.api_key = ak;
            new_acc.project_id = pid;
            if let Some(sub_val) = sub {
                new_acc.subscription = sub_val;
            }
            new_acc.enabled = enabled;
            new_acc.added_at = Some(chrono::Utc::now().to_rfc3339());
            loaded.accounts.push(new_acc);
            added.push(email.to_string());
            logger().info(format!("[WebUI] Account {email} added"));
        }
    }

    if !added.is_empty() || !updated.is_empty() {
        save_accounts(&path, &loaded.accounts, &loaded.settings, loaded.active_index).await;
    }

    state.account_manager.reload().await;
    crate::server::invalidate_limits_cache().await;

    let am = Arc::clone(&state.account_manager);
    tokio::spawn(async move {
        crate::cloudcode::model_api::populate_model_cache_for_accounts(&am, true).await;
    });

    Json(json!({
        "status": "ok",
        "results": {
            "added": added,
            "updated": updated,
            "failed": failed
        },
        "message": format!("Imported {} accounts", added.len() + updated.len())
    }))
    .into_response()
}

fn validate_config_fields(input: &Value) -> Map<String, Value> {
    let mut updates = Map::new();
    let Some(obj) = input.as_object() else {
        return updates;
    };

    let check_range = |k: &str, min: f64, max: f64, out: &mut Map<String, Value>| {
        if let Some(v) = obj.get(k) {
            if let Some(n) = v.as_f64() {
                if n >= min && n <= max {
                    out.insert(k.to_string(), v.clone());
                }
            }
        }
    };

    check_range("maxRetries", 1.0, 20.0, &mut updates);
    check_range("retryBaseMs", 100.0, 10000.0, &mut updates);
    check_range("retryMaxMs", 1000.0, 120_000.0, &mut updates);
    check_range("defaultCooldownMs", 1000.0, 300_000.0, &mut updates);
    check_range("maxWaitBeforeErrorMs", 0.0, 600_000.0, &mut updates);
    check_range("maxAccounts", 1.0, 100.0, &mut updates);
    check_range("rateLimitDedupWindowMs", 1000.0, 30_000.0, &mut updates);
    check_range("maxConsecutiveFailures", 1.0, 10.0, &mut updates);
    check_range("extendedCooldownMs", 10_000.0, 600_000.0, &mut updates);
    check_range("maxCapacityRetries", 1.0, 10.0, &mut updates);
    check_range("switchAccountDelayMs", 1000.0, 60_000.0, &mut updates);

    if let Some(v) = obj.get("globalQuotaThreshold") {
        if let Some(n) = v.as_f64() {
            if (0.0..1.0).contains(&n) {
                updates.insert("globalQuotaThreshold".to_string(), v.clone());
            }
        }
    }

    if let Some(tiers) = obj.get("capacityBackoffTiersMs").and_then(|v| v.as_array()) {
        if !tiers.is_empty() && tiers.len() <= 10 {
            let valid = tiers
                .iter()
                .all(|t| t.as_u64().map(|n| (1000..=300_000).contains(&n)).unwrap_or(false));
            if valid {
                updates.insert(
                    "capacityBackoffTiersMs".to_string(),
                    Value::Array(tiers.clone()),
                );
            }
        }
    }

    if let Some(acct) = obj.get("accountSelection").and_then(|v| v.as_object()) {
        let mut acct_upd = Map::new();
        if let Some(strat) = acct.get("strategy").and_then(|v| v.as_str()) {
            if ["sticky", "round-robin", "hybrid"].contains(&strat) {
                acct_upd.insert("strategy".to_string(), Value::String(strat.to_string()));
            }
        }
        for sub in ["healthScore", "tokenBucket", "quota", "weights"] {
            if let Some(sub_obj) = acct.get(sub).and_then(|v| v.as_object()) {
                if !sub_obj.is_empty() {
                    acct_upd.insert(sub.to_string(), Value::Object(sub_obj.clone()));
                }
            }
        }
        if !acct_upd.is_empty() {
            updates.insert("accountSelection".to_string(), Value::Object(acct_upd));
        }
    }

    updates
}

async fn get_api_config() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "config": get_public_config(),
        "version": env!("CARGO_PKG_VERSION"),
        "note": "Edit ~/.config/novoid-core/config.json or use env vars to change these values"
    }))
}

async fn post_api_config(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Response {
    let mut updates = validate_config_fields(&body);

    if let Some(dev_mode) = body.get("devMode").and_then(|v| v.as_bool()) {
        updates.insert("devMode".to_string(), Value::Bool(dev_mode));
        updates.insert("debug".to_string(), Value::Bool(dev_mode));
        logger().set_debug(dev_mode);
    } else if let Some(debug) = body.get("debug").and_then(|v| v.as_bool()) {
        updates.insert("debug".to_string(), Value::Bool(debug));
        updates.insert("devMode".to_string(), Value::Bool(debug));
        logger().set_debug(debug);
    }

    if let Some(lvl) = body.get("logLevel").and_then(|v| v.as_str()) {
        if ["info", "warn", "error", "debug"].contains(&lvl) {
            updates.insert("logLevel".to_string(), Value::String(lvl.to_string()));
        }
    }
    if let Some(ptc) = body.get("persistTokenCache").and_then(|v| v.as_bool()) {
        updates.insert("persistTokenCache".to_string(), Value::Bool(ptc));
    }
    if let Some(rte) = body.get("requestThrottlingEnabled").and_then(|v| v.as_bool()) {
        updates.insert("requestThrottlingEnabled".to_string(), Value::Bool(rte));
    }
    if let Some(rd) = body.get("requestDelayMs").and_then(|v| v.as_u64()) {
        if (100..=5000).contains(&rd) {
            updates.insert("requestDelayMs".to_string(), Value::from(rd));
        }
    }

    if updates.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "status": "error", "error": "No valid configuration updates provided" })),
        )
            .into_response();
    }

    let strat_changed = updates
        .get("accountSelection")
        .and_then(|a| a.get("strategy"))
        .is_some();

    let upd_val = Value::Object(updates.clone());
    match save_config_updates(&upd_val) {
        Ok(()) => {
            if strat_changed {
                state.account_manager.reload().await;
            crate::server::invalidate_limits_cache().await;
            }
            Json(json!({
                "status": "ok",
                "message": "Configuration saved. Restart server to apply some changes.",
                "updates": upd_val,
                "config": get_public_config()
            }))
            .into_response()
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "status": "error", "error": err })),
        )
            .into_response(),
    }
}

async fn post_change_password(Json(body): Json<Value>) -> Response {
    let Some(new_pwd) = body.get("newPassword").and_then(|v| v.as_str()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "status": "error", "error": "New password is required" })),
        )
            .into_response();
    };
    if new_pwd.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "status": "error", "error": "New password is required" })),
        )
            .into_response();
    }

    let cur_pwd = get_config().webui_password;
    if !cur_pwd.is_empty() {
        let old_pwd = body.get("oldPassword").and_then(|v| v.as_str()).unwrap_or("");
        if old_pwd != cur_pwd {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "status": "error", "error": "Invalid current password" })),
            )
                .into_response();
        }
    }

    match save_config_updates(&json!({ "webuiPassword": new_pwd })) {
        Ok(()) => Json(json!({
            "status": "ok",
            "message": "Password changed successfully"
        }))
        .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "status": "error", "error": err })),
        )
            .into_response(),
    }
}

async fn get_runtime_settings(State(state): State<AppState>) -> Json<Value> {
    let mut settings = state.account_manager.get_settings().await;
    settings.insert("port".to_string(), Value::from(state.port));
    Json(json!({ "status": "ok", "settings": settings }))
}

async fn get_claude_cfg() -> Response {
    match claude_config::read_claude_config().await {
        Ok(cfg) => Json(json!({
            "status": "ok",
            "config": cfg,
            "path": claude_config::get_claude_config_path().to_string_lossy()
        }))
        .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "status": "error", "error": err })),
        )
            .into_response(),
    }
}

async fn post_claude_cfg(Json(body): Json<Value>) -> Response {
    if !body.is_object() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "status": "error", "error": "Invalid config updates" })),
        )
            .into_response();
    }
    match claude_config::update_claude_config(&body).await {
        Ok(cfg) => Json(json!({
            "status": "ok",
            "config": cfg,
            "message": "Claude configuration updated"
        }))
        .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "status": "error", "error": err })),
        )
            .into_response(),
    }
}

async fn restore_claude_cfg() -> Response {
    let proxy_vars = [
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_MODEL",
        "CLAUDE_CODE_SUBAGENT_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "ENABLE_EXPERIMENTAL_MCP_CLI",
    ];

    match claude_config::read_claude_config().await {
        Ok(mut cfg) => {
            if let Some(env_obj) = cfg.get_mut("env").and_then(|v| v.as_object_mut()) {
                for k in proxy_vars {
                    env_obj.remove(k);
                }
                if env_obj.is_empty() {
                    if let Some(root) = cfg.as_object_mut() {
                        root.remove("env");
                    }
                }
            }
            match claude_config::replace_claude_config(&cfg).await {
                Ok(new_cfg) => Json(json!({
                    "status": "ok",
                    "config": new_cfg,
                    "message": "Claude CLI configuration restored to defaults"
                }))
                .into_response(),
                Err(err) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "status": "error", "error": err })),
                )
                    .into_response(),
            }
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "status": "error", "error": err })),
        )
            .into_response(),
    }
}

async fn get_claude_mode() -> Response {
    match claude_config::read_claude_config().await {
        Ok(cfg) => {
            let base_url = cfg
                .get("env")
                .and_then(|e| e.get("ANTHROPIC_BASE_URL"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let is_proxy = base_url.contains("localhost")
                || base_url.contains("127.0.0.1")
                || base_url.contains("::1")
                || base_url.contains("0.0.0.0");
            Json(json!({
                "status": "ok",
                "mode": if is_proxy { "proxy" } else { "paid" }
            }))
            .into_response()
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "status": "error", "error": err })),
        )
            .into_response(),
    }
}

async fn post_claude_mode(Json(body): Json<Value>) -> Response {
    let Some(mode) = body.get("mode").and_then(|v| v.as_str()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "status": "error", "error": "mode must be \"proxy\" or \"paid\"" })),
        )
            .into_response();
    };
    if mode != "proxy" && mode != "paid" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "status": "error", "error": "mode must be \"proxy\" or \"paid\"" })),
        )
            .into_response();
    }

    match claude_config::read_claude_config().await {
        Ok(mut cfg) => {
            if mode == "proxy" {
                let first_preset = default_claude_presets()
                    .as_array()
                    .and_then(|a| a.first())
                    .and_then(|p| p.get("config"))
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                cfg["env"] = first_preset;
            } else if let Some(obj) = cfg.as_object_mut() {
                obj.remove("env");
            }

            match claude_config::replace_claude_config(&cfg).await {
                Ok(new_cfg) => Json(json!({
                    "status": "ok",
                    "mode": mode,
                    "config": new_cfg,
                    "message": format!("Switched to {} mode. Restart Claude CLI to apply.", if mode == "proxy" { "Proxy" } else { "Paid (Anthropic API)" })
                }))
                .into_response(),
                Err(err) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "status": "error", "error": err })),
                )
                    .into_response(),
            }
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "status": "error", "error": err })),
        )
            .into_response(),
    }
}

async fn get_claude_presets() -> Response {
    match claude_config::read_presets().await {
        Ok(presets) => Json(json!({ "status": "ok", "presets": presets })).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "status": "error", "error": err })),
        )
            .into_response(),
    }
}

async fn post_claude_preset(Json(body): Json<Value>) -> Response {
    let Some(name) = body
        .get("name")
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "status": "error", "error": "Preset name is required" })),
        )
            .into_response();
    };
    let Some(cfg) = body.get("config").filter(|v| v.is_object()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "status": "error", "error": "Config object is required" })),
        )
            .into_response();
    };

    match claude_config::save_preset(name, cfg).await {
        Ok(presets) => Json(json!({
            "status": "ok",
            "presets": presets,
            "message": format!("Preset \"{name}\" saved")
        }))
        .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "status": "error", "error": err })),
        )
            .into_response(),
    }
}

async fn delete_claude_preset(Path(name): Path<String>) -> Response {
    match claude_config::delete_preset(&name).await {
        Ok(presets) => Json(json!({
            "status": "ok",
            "presets": presets,
            "message": format!("Preset \"{name}\" deleted")
        }))
        .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "status": "error", "error": err })),
        )
            .into_response(),
    }
}

async fn get_server_presets() -> Response {
    match server_presets::read_server_presets().await {
        Ok(presets) => Json(json!({ "status": "ok", "presets": presets })).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "status": "error", "error": err })),
        )
            .into_response(),
    }
}

async fn post_server_preset(Json(body): Json<Value>) -> Response {
    let Some(name) = body
        .get("name")
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "status": "error", "error": "Preset name is required" })),
        )
            .into_response();
    };
    if name.len() > 50 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "status": "error", "error": "Preset name must be 50 characters or fewer" })),
        )
            .into_response();
    }
    let Some(cfg) = body.get("config").filter(|v| v.is_object()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "status": "error", "error": "Config object is required" })),
        )
            .into_response();
    };
    let validated = validate_config_fields(cfg);
    if validated.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "status": "error", "error": "No valid config fields provided" })),
        )
            .into_response();
    }
    let desc = body.get("description").and_then(|v| v.as_str());
    match server_presets::save_server_preset(name, Value::Object(validated), desc).await {
        Ok(presets) => Json(json!({
            "status": "ok",
            "presets": presets,
            "message": format!("Server preset \"{name}\" saved")
        }))
        .into_response(),
        Err(err) => {
            let st = if err.contains("built-in") {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (st, Json(json!({ "status": "error", "error": err }))).into_response()
        }
    }
}

async fn patch_server_preset(
    Path(current_name): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let new_name = body.get("name").and_then(|v| v.as_str());
    if let Some(nn) = new_name {
        let trimmed = nn.trim();
        if trimmed.is_empty() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "status": "error", "error": "Preset name is required" })),
            )
                .into_response();
        }
        if trimmed.len() > 50 {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "status": "error", "error": "Preset name must be 50 characters or fewer" })),
            )
                .into_response();
        }
    }
    let desc = body.get("description").and_then(|v| v.as_str());
    let cfg_val = body.get("config").and_then(|c| {
        let v = validate_config_fields(c);
        if v.is_empty() {
            None
        } else {
            Some(Value::Object(v))
        }
    });

    if new_name.is_none() && desc.is_none() && cfg_val.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "status": "error", "error": "No updates provided" })),
        )
            .into_response();
    }

    match server_presets::update_server_preset(&current_name, new_name, desc, cfg_val.as_ref())
        .await
    {
        Ok(presets) => Json(json!({
            "status": "ok",
            "presets": presets,
            "message": format!("Server preset \"{current_name}\" updated")
        }))
        .into_response(),
        Err(err) => {
            let st = if err.contains("built-in")
                || err.contains("not found")
                || err.contains("already exists")
            {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (st, Json(json!({ "status": "error", "error": err }))).into_response()
        }
    }
}

async fn delete_server_preset(Path(name): Path<String>) -> Response {
    match server_presets::delete_server_preset(&name).await {
        Ok(presets) => Json(json!({
            "status": "ok",
            "presets": presets,
            "message": format!("Server preset \"{name}\" deleted")
        }))
        .into_response(),
        Err(err) => {
            let st = if err.contains("built-in") {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (st, Json(json!({ "status": "error", "error": err }))).into_response()
        }
    }
}

async fn post_model_config(Json(body): Json<Value>) -> Response {
    let Some(model_id) = body.get("modelId").and_then(|v| v.as_str()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "status": "error", "error": "Invalid parameters" })),
        )
            .into_response();
    };
    let Some(new_cfg) = body.get("config").and_then(|v| v.as_object()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "status": "error", "error": "Invalid parameters" })),
        )
            .into_response();
    };

    let mut current_mapping = serde_json::to_value(get_config().model_mapping)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();

    let mut entry = current_mapping
        .get(model_id)
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    for (k, v) in new_cfg {
        entry.insert(k.clone(), v.clone());
    }
    let entry_val = Value::Object(entry);
    current_mapping.insert(model_id.to_string(), entry_val.clone());

    match save_config_updates(&json!({ "modelMapping": current_mapping })) {
        Ok(()) => {
            crate::server::invalidate_limits_cache().await;
            Json(json!({ "status": "ok", "modelConfig": entry_val })).into_response()
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "status": "error", "error": err })),
        )
            .into_response(),
    }
}

async fn get_logs() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "logs": logger().get_history()
    }))
}

async fn stream_logs(
    Query(params): Query<HashMap<String, String>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let send_history = params.get("history").map(|v| v == "true").unwrap_or(false);
    let history = if send_history {
        logger().get_history()
    } else {
        Vec::new()
    };
    let mut rx = logger().subscribe();
    let mut shutdown_rx = crate::server::subscribe_shutdown();

    let stream = async_stream::stream! {
        for log in history {
            if let Ok(data) = serde_json::to_string(&log) {
                yield Ok(Event::default().data(data));
            }
        }
        loop {
            if *shutdown_rx.borrow() {
                break;
            }
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    break;
                }
                recv = rx.recv() => {
                    match recv {
                        Ok(log) => {
                            if let Ok(data) = serde_json::to_string(&log) {
                                yield Ok(Event::default().data(data));
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn get_strategy_health(State(state): State<AppState>) -> Response {
    if !get_config().dev_mode {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "status": "error", "error": "Developer mode is not enabled" })),
        )
            .into_response();
    }
    let mut health = state.account_manager.get_strategy_health_data().await;
    if let Some(obj) = health.as_object_mut() {
        obj.insert("status".to_string(), Value::String("ok".to_string()));
    }
    Json(health).into_response()
}

async fn get_stats_history() -> Json<Value> {
    Json(usage_stats::get_history())
}

fn is_local_hostname(hostname: &str) -> bool {
    let clean = hostname.to_lowercase();
    let clean = clean.trim_matches(|c| c == '[' || c == ']');
    clean == "localhost" || clean == "127.0.0.1" || clean == "::1"
}

fn resolve_oauth_target(
    headers: &HeaderMap,
    params: &HashMap<String, String>,
) -> (String, bool, &'static str) {
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(|s| s.trim())
        .unwrap_or("http");
    let host = headers
        .get("x-forwarded-host")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(|s| s.trim())
        .or_else(|| headers.get(header::HOST).and_then(|v| v.to_str().ok()));

    let origin = host.map(|h| format!("{proto}://{h}"));

    if let Ok(custom_uri) = std::env::var("OAUTH_REDIRECT_URI") {
        if !custom_uri.is_empty() {
            let mut use_web = false;
            if let Ok(parsed) = url::Url::parse(&custom_uri) {
                let cb_port = oauth_callback_port();
                let port = parsed
                    .port()
                    .unwrap_or(if parsed.scheme() == "https" { 443 } else { 80 });
                let h = parsed.host_str().unwrap_or("");
                if parsed.path() == "/oauth-callback" && (!is_local_hostname(h) || port != cb_port) {
                    use_web = true;
                }
            }
            return (
                custom_uri,
                use_web,
                if use_web { "web" } else { "local-callback" },
            );
        }
    }

    if oauth_has_custom_client() {
        if let Some(orig) = origin {
            return (format!("{orig}/oauth-callback"), true, "web");
        }
    }

    let client_remote = params.get("local").map(|v| v == "0").unwrap_or(false);
    if !client_remote {
        if let Some(ref orig) = origin {
            if let Ok(parsed) = url::Url::parse(orig) {
                if parsed.scheme() == "http" && is_local_hostname(parsed.host_str().unwrap_or("")) {
                    return (format!("{orig}/oauth-callback"), true, "web");
                }
            }
        }
    }

    (oauth_default_redirect_uri(), false, "remote-loopback")
}

async fn get_auth_url(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let now = now_ms();
    {
        let mut store = state.oauth_flows.write().await;
        let expired: Vec<String> = store
            .pending
            .iter()
            .filter(|(_, v)| now.saturating_sub(v.timestamp) > 10 * 60 * 1000)
            .map(|(k, _)| k.clone())
            .collect();
        for k in expired {
            if let Some(mut flow) = store.pending.remove(&k) {
                if let Some(tx) = flow.abort_tx.take() {
                    let _ = tx.send(());
                }
            }
        }
        store
            .completed
            .retain(|_, v| now.saturating_sub(v.timestamp) <= 5 * 60 * 1000);
    }

    let (redirect_uri, use_web_callback, mode) = resolve_oauth_target(&headers, &params);
    let auth = get_authorization_url(Some(&redirect_uri));

    let mut abort_tx_opt = None;
    if !use_web_callback {
        {
            let mut store = state.oauth_flows.write().await;
            for flow in store.pending.values_mut() {
                if let Some(tx) = flow.abort_tx.take() {
                    let _ = tx.send(());
                }
            }
        }
        if let Ok(handle) = start_callback_server(auth.state.clone(), 120_000).await {
            abort_tx_opt = Some(handle.abort_tx);
            let flows_arc = Arc::clone(&state.oauth_flows);
            let am_arc = Arc::clone(&state.account_manager);
            let st_key = auth.state.clone();
            let verifier_clone = auth.verifier.clone();
            let redirect_clone = redirect_uri.clone();

            tokio::spawn(async move {
                if let Ok(Ok(code)) = handle.code_rx.await {
                    logger().info("[WebUI] Received OAuth callback, completing flow...");
                    match complete_oauth_flow(&code, &verifier_clone, Some(&redirect_clone)).await {
                        Ok(acc_data) => {
                            let res = add_or_update_account(
                                acc_data.email.clone(),
                                Some(acc_data.refresh_token),
                                None,
                                acc_data.project_id,
                                "oauth",
                            )
                            .await;
                            let mut store = flows_arc.write().await;
                            store.pending.remove(&st_key);
                            match res {
                                Ok(()) => {
                                    drop(store);
                                    am_arc.reload().await;
                                    crate::server::invalidate_limits_cache().await;
                                    let mut store = flows_arc.write().await;
                                    store.completed.insert(
                                        st_key,
                                        CompletedOAuthFlow {
                                            status: "ok".to_string(),
                                            email: Some(acc_data.email.clone()),
                                            error: None,
                                            timestamp: now_ms(),
                                        },
                                    );
                                    logger().success(format!(
                                        "[WebUI] Account {} added successfully",
                                        acc_data.email
                                    ));
                                }
                                Err(err) => {
                                    store.completed.insert(
                                        st_key,
                                        CompletedOAuthFlow {
                                            status: "error".to_string(),
                                            email: None,
                                            error: Some(err),
                                            timestamp: now_ms(),
                                        },
                                    );
                                }
                            }
                        }
                        Err(err) => {
                            let mut store = flows_arc.write().await;
                            store.pending.remove(&st_key);
                            store.completed.insert(
                                st_key,
                                CompletedOAuthFlow {
                                    status: "error".to_string(),
                                    email: None,
                                    error: Some(err),
                                    timestamp: now_ms(),
                                },
                            );
                        }
                    }
                }
            });
        }
    }

    {
        let mut store = state.oauth_flows.write().await;
        store.pending.insert(
            auth.state.clone(),
            PendingOAuthFlow {
                verifier: auth.verifier,
                state: auth.state.clone(),
                redirect_uri: redirect_uri.clone(),
                timestamp: now_ms(),
                abort_tx: abort_tx_opt,
            },
        );
    }

    Json(json!({
        "status": "ok",
        "url": auth.url,
        "state": auth.state,
        "mode": mode,
        "redirectUri": redirect_uri,
        "hasCustomClient": oauth_has_custom_client()
    }))
    .into_response()
}

async fn get_auth_status(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(st_key) = params.get("state").filter(|s| !s.is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "status": "error", "error": "Missing state parameter" })),
        )
            .into_response();
    };

    let store = state.oauth_flows.read().await;
    if let Some(done) = store.completed.get(st_key) {
        return Json(json!({
            "status": done.status,
            "email": done.email,
            "error": done.error
        }))
        .into_response();
    }
    if store.pending.contains_key(st_key) {
        return Json(json!({ "status": "pending" })).into_response();
    }
    Json(json!({ "status": "not_found" })).into_response()
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn render_callback_page(
    ok: bool,
    title: &str,
    detail: &str,
    state_val: &str,
    email: Option<&str>,
) -> (StatusCode, Html<String>) {
    let safe_title = escape_html(title);
    let safe_detail = escape_html(detail);
    let payload = json!({
        "type": "oauth-complete",
        "status": if ok { "ok" } else { "error" },
        "state": state_val,
        "email": email,
        "error": if ok { None } else { Some(detail) }
    });
    let payload_str = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
    let color = if ok { "#16a34a" } else { "#dc2626" };
    let hint = if ok {
        "This window will close automatically..."
    } else {
        "You can close this window and try again."
    };
    let close_script = if ok {
        "setTimeout(function () { window.close(); }, 1200);"
    } else {
        ""
    };

    let html = format!(
        "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"UTF-8\"><title>{safe_title}</title><style>body{{font-family:system-ui,sans-serif;background:#f8fafc;color:#0f172a;display:flex;align-items:center;justify-content:center;min-height:100vh;margin:0;padding:24px;}}.card{{background:#fff;border:1px solid #e2e8f0;border-radius:12px;padding:32px;max-width:440px;width:100%;text-align:center;}}h1{{font-size:20px;margin:0 0 10px;color:{color};}}p{{font-size:14px;color:#475569;margin:0 0 16px;word-break:break-word;}}.hint{{font-size:12px;color:#94a3b8;}}</style></head><body><div class=\"card\"><h1>{safe_title}</h1><p>{safe_detail}</p><div class=\"hint\">{hint}</div></div><script>try{{if(window.opener&&!window.opener.closed){{window.opener.postMessage({payload_str},window.location.origin);}}}}catch(e){{}}{close_script}</script></body></html>"
    );

    (
        if ok {
            StatusCode::OK
        } else {
            StatusCode::BAD_REQUEST
        },
        Html(html),
    )
}

async fn oauth_callback_handler(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> (StatusCode, Html<String>) {
    let st_key = params.get("state").cloned().unwrap_or_default();

    if let Some(err) = params.get("error") {
        if !st_key.is_empty() {
            let mut store = state.oauth_flows.write().await;
            store.pending.remove(&st_key);
            store.completed.insert(
                st_key.clone(),
                CompletedOAuthFlow {
                    status: "error".to_string(),
                    email: None,
                    error: Some(err.clone()),
                    timestamp: now_ms(),
                },
            );
        }
        return render_callback_page(
            false,
            "Authentication Failed",
            &format!("Google returned an error: {err}"),
            &st_key,
            None,
        );
    }

    if st_key.is_empty() {
        return render_callback_page(
            false,
            "Authentication Failed",
            "Missing OAuth state parameter.",
            "",
            None,
        );
    }

    let flow_opt = {
        let mut store = state.oauth_flows.write().await;
        if let Some(flow) = store.pending.remove(&st_key) {
            Some(Ok(flow))
        } else if let Some(done) = store.completed.get(&st_key) {
            if done.status == "ok" {
                Some(Err(done.email.clone().unwrap_or_default()))
            } else {
                None
            }
        } else {
            None
        }
    };

    let flow = match flow_opt {
        Some(Ok(f)) => f,
        Some(Err(email)) => {
            return render_callback_page(
                true,
                "Authentication Successful",
                &format!("Account {email} has been connected."),
                &st_key,
                Some(&email),
            );
        }
        None => {
            return render_callback_page(
                false,
                "Authentication Expired",
                "OAuth session not found or expired. Please start again from the WebUI.",
                &st_key,
                None,
            );
        }
    };

    let Some(code) = params.get("code").filter(|s| !s.is_empty()) else {
        return render_callback_page(
            false,
            "Authentication Failed",
            "No authorization code received from Google.",
            &st_key,
            None,
        );
    };

    match complete_oauth_flow(code, &flow.verifier, Some(&flow.redirect_uri)).await {
        Ok(acc_data) => {
            if let Err(err) = add_or_update_account(
                acc_data.email.clone(),
                Some(acc_data.refresh_token),
                None,
                acc_data.project_id,
                "oauth",
            )
            .await
            {
                let mut store = state.oauth_flows.write().await;
                store.completed.insert(
                    st_key.clone(),
                    CompletedOAuthFlow {
                        status: "error".to_string(),
                        email: None,
                        error: Some(err.clone()),
                        timestamp: now_ms(),
                    },
                );
                return render_callback_page(
                    false,
                    "Authentication Failed",
                    &err,
                    &st_key,
                    None,
                );
            }
            if let Some(tx) = flow.abort_tx {
                let _ = tx.send(());
            }
            state.account_manager.reload().await;
            crate::server::invalidate_limits_cache().await;
            {
                let mut store = state.oauth_flows.write().await;
                store.completed.insert(
                    st_key.clone(),
                    CompletedOAuthFlow {
                        status: "ok".to_string(),
                        email: Some(acc_data.email.clone()),
                        error: None,
                        timestamp: now_ms(),
                    },
                );
            }
            logger().success(format!(
                "[WebUI] Account {} added via web callback",
                acc_data.email
            ));
            render_callback_page(
                true,
                "Authentication Successful",
                &format!("Account {} connected successfully.", acc_data.email),
                &st_key,
                Some(&acc_data.email),
            )
        }
        Err(err) => {
            let mut store = state.oauth_flows.write().await;
            store.completed.insert(
                st_key.clone(),
                CompletedOAuthFlow {
                    status: "error".to_string(),
                    email: None,
                    error: Some(err.clone()),
                    timestamp: now_ms(),
                },
            );
            render_callback_page(false, "Authentication Failed", &err, &st_key, None)
        }
    }
}

async fn post_auth_complete(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Response {
    let Some(cb_input) = body.get("callbackInput").and_then(|v| v.as_str()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "status": "error", "error": "Missing callbackInput" })),
        )
            .into_response();
    };
    let req_state = body.get("state").and_then(|v| v.as_str());

    let (code, input_state) = match extract_code_from_input(cb_input) {
        Ok(v) => v,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "status": "error", "error": err })),
            )
                .into_response();
        }
    };

    let effective_state = input_state.or_else(|| req_state.map(|s| s.to_string()));
    let Some(st_key) = effective_state else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "status": "error", "error": "Missing OAuth state" })),
        )
            .into_response();
    };

    let flow = {
        let mut store = state.oauth_flows.write().await;
        if let Some(f) = store.pending.remove(&st_key) {
            f
        } else if let Some(done) = store.completed.get(&st_key) {
            if done.status == "ok" {
                let email = done.email.clone().unwrap_or_default();
                return Json(json!({
                    "status": "ok",
                    "email": email,
                    "message": format!("Account {email} added successfully")
                }))
                .into_response();
            }
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "status": "error", "error": "OAuth flow not found or expired." })),
            )
                .into_response();
        } else {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "status": "error", "error": "OAuth flow not found or expired." })),
            )
                .into_response();
        }
    };

    match complete_oauth_flow(&code, &flow.verifier, Some(&flow.redirect_uri)).await {
        Ok(acc_data) => {
            if let Err(err) = add_or_update_account(
                acc_data.email.clone(),
                Some(acc_data.refresh_token),
                None,
                acc_data.project_id,
                "oauth",
            )
            .await
            {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "status": "error", "error": err })),
                )
                    .into_response();
            }
            if let Some(tx) = flow.abort_tx {
                let _ = tx.send(());
            }
            state.account_manager.reload().await;
            crate::server::invalidate_limits_cache().await;
            {
                let mut store = state.oauth_flows.write().await;
                store.completed.insert(
                    st_key,
                    CompletedOAuthFlow {
                        status: "ok".to_string(),
                        email: Some(acc_data.email.clone()),
                        error: None,
                        timestamp: now_ms(),
                    },
                );
            }
            Json(json!({
                "status": "ok",
                "email": acc_data.email,
                "message": format!("Account {} added successfully", acc_data.email)
            }))
            .into_response()
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "status": "error", "error": err })),
        )
            .into_response(),
    }
}
