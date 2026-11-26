pub mod assets;
pub mod webui;

use crate::account::AccountManager;
use crate::cloudcode::handler::{send_message, send_message_stream_prepared};
use crate::cloudcode::model_api::{
    get_model_quotas, get_subscription_tier, is_valid_model, list_models_for_accounts,
};
use crate::config::get_config;
use crate::constants::{
    account_config_path, resolve_builtin_model_mapping, REQUEST_BODY_LIMIT_BYTES,
};
use crate::error::NovoidError;
use crate::format::signature_cache::clear_all_signature_caches;
use crate::logger::logger;
use crate::utils::{format_duration, now_ms, usage_stats};
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Query, Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use tokio::sync::{Mutex, RwLock};
use tower_http::cors::{Any, CorsLayer};

const ACCOUNT_LIMITS_CACHE_TTL_MS: u64 = 10_000;

struct CachedLimits {
    cached_at: u64,
    account_count: usize,
    payload: Value,
}

static LIMITS_CACHE: OnceLock<RwLock<Option<CachedLimits>>> = OnceLock::new();
static LIMITS_FLIGHT_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

static SHUTDOWN_TX: OnceLock<tokio::sync::watch::Sender<bool>> = OnceLock::new();

fn shutdown_tx() -> &'static tokio::sync::watch::Sender<bool> {
    SHUTDOWN_TX.get_or_init(|| tokio::sync::watch::channel(false).0)
}

pub fn subscribe_shutdown() -> tokio::sync::watch::Receiver<bool> {
    shutdown_tx().subscribe()
}

pub fn trigger_shutdown() {
    let _ = shutdown_tx().send(true);
}

fn limits_cache() -> &'static RwLock<Option<CachedLimits>> {
    LIMITS_CACHE.get_or_init(|| RwLock::new(None))
}

fn limits_flight_lock() -> &'static Mutex<()> {
    LIMITS_FLIGHT_LOCK.get_or_init(|| Mutex::new(()))
}

pub async fn invalidate_limits_cache() {
    if let Ok(mut guard) = limits_cache().try_write() {
        *guard = None;
    } else {
        let mut guard = limits_cache().write().await;
        *guard = None;
    }
}

#[derive(Clone)]
pub struct AppState {
    pub account_manager: Arc<AccountManager>,
    pub fallback_enabled: bool,
    pub oauth_flows: Arc<RwLock<webui::OAuthFlowStore>>,
    pub port: u16,
}

pub fn build_router(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let v1_routes = Router::new()
        .route("/messages", post(handle_messages))
        .route("/models", get(handle_list_models))
        .route("/messages/count_tokens", post(handle_count_tokens))
        .layer(middleware::from_fn(v1_auth_middleware));

    let webui_routes = webui::router().layer(middleware::from_fn(webui_auth_middleware));

    Router::new()
        .nest("/v1", v1_routes)
        .merge(webui_routes)
        .route("/", get(assets::serve_embedded_asset).post(handle_silent_ok))
        .route("/api/event_logging/batch", post(handle_silent_ok))
        .route(
            "/test/clear-signature-cache",
            post(handle_clear_signature_cache),
        )
        .route(
            "/health",
            get(handle_health).layer(middleware::from_fn(webui_auth_middleware)),
        )
        .route(
            "/account-limits",
            get(handle_account_limits).layer(middleware::from_fn(webui_auth_middleware)),
        )
        .route("/refresh-token", post(handle_refresh_token))
        .fallback(assets::serve_embedded_asset)
        .layer(DefaultBodyLimit::max(REQUEST_BODY_LIMIT_BYTES))
        .layer(cors)
        .layer(middleware::from_fn(request_logging_middleware))
        .with_state(state)
}

async fn request_logging_middleware(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let uri = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/").to_string();
    let start = std::time::Instant::now();
    let resp = next.run(req).await;
    let status = resp.status().as_u16();
    let duration = start.elapsed().as_millis();
    let log_msg = format!("[{method}] {uri} {status} ({duration}ms)");

    if uri == "/api/event_logging/batch"
        || uri.starts_with("/v1/messages/count_tokens")
        || uri.starts_with("/.well-known/")
    {
        logger().debug(log_msg);
    } else if status >= 500 {
        logger().error(log_msg);
    } else if status >= 400 {
        logger().warn(log_msg);
    } else {
        logger().info(log_msg);
    }
    resp
}

async fn v1_auth_middleware(req: Request, next: Next) -> Response {
    let cfg = get_config();
    if cfg.api_key.is_empty() {
        return next.run(req).await;
    }

    let auth_header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let x_api_key = req
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok());

    let provided = if let Some(h) = auth_header {
        h.strip_prefix("Bearer ").unwrap_or("")
    } else if let Some(k) = x_api_key {
        k
    } else {
        ""
    };

    if provided.is_empty() || provided != cfg.api_key {
        logger().warn("[API] Unauthorized request, invalid API key");
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "type": "error",
                "error": {
                    "type": "authentication_error",
                    "message": "Invalid or missing API key"
                }
            })),
        )
            .into_response();
    }

    next.run(req).await
}

async fn webui_auth_middleware(req: Request, next: Next) -> Response {
    let cfg = get_config();
    if cfg.webui_password.is_empty() {
        return next.run(req).await;
    }

    let path = req.uri().path().to_string();
    let method = req.method().clone();
    let is_api_route = path.starts_with("/api/");
    let is_auth_url = path == "/api/auth/url";
    let is_oauth_callback = path == "/oauth-callback";
    let is_config_get = path == "/api/config" && method == Method::GET;
    let is_protected = (!is_oauth_callback && is_api_route && !is_auth_url && !is_config_get)
        || path == "/account-limits"
        || path == "/health";

    if is_protected {
        let header_pwd = req
            .headers()
            .get("x-webui-password")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let query_pwd = req.uri().query().and_then(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .find(|(k, _)| k == "password")
                .map(|(_, v)| v.into_owned())
        });
        let provided = header_pwd.or(query_pwd).unwrap_or_default();
        if provided != cfg.webui_password {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "status": "error", "error": "Unauthorized: Password required" })),
            )
                .into_response();
        }
    }

    next.run(req).await
}

async fn handle_silent_ok() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn handle_clear_signature_cache() -> Json<Value> {
    clear_all_signature_caches();
    logger().debug("[Test] Cleared all signature caches");
    Json(json!({ "success": true, "message": "All signature caches cleared" }))
}

async fn handle_count_tokens() -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "type": "error",
            "error": {
                "type": "not_implemented",
                "message": "Token counting is not implemented. Use /v1/messages with max_tokens or configure your client to skip token counting."
            }
        })),
    )
}

async fn handle_list_models(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    state.account_manager.ensure_initialized().await;
    let all = state.account_manager.get_all_accounts().await;
    let enabled_count = all.iter().filter(|a| !a.is_invalid && a.enabled).count();
    if enabled_count == 0 {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "type": "error",
                "error": {
                    "type": "api_error",
                    "message": "No accounts available"
                }
            })),
        )
            .into_response();
    }

    let force_refresh = params.get("refresh").map(|v| v == "true").unwrap_or(false);
    let models = list_models_for_accounts(&state.account_manager, force_refresh).await;
    Json(models).into_response()
}

async fn handle_refresh_token(State(state): State<AppState>) -> Response {
    state.account_manager.ensure_initialized().await;
    state.account_manager.clear_token_cache(None).await;
    state.account_manager.clear_project_cache(None).await;
    invalidate_limits_cache().await;

    let available = state.account_manager.get_available_accounts(None).await;
    if available.is_empty() {
        return Json(json!({
            "status": "ok",
            "message": "Token caches cleared (no active accounts configured)"
        }))
        .into_response();
    }

    match state
        .account_manager
        .get_token_for_account(&available[0])
        .await
    {
        Ok(tok) => {
            let prefix: String = tok.chars().take(10).collect();
            Json(json!({
                "status": "ok",
                "message": "Token caches cleared and refreshed",
                "tokenPrefix": format!("{prefix}...")
            }))
            .into_response()
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "status": "error",
                "error": err.to_string()
            })),
        )
            .into_response(),
    }
}

async fn handle_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> Response {
    state.account_manager.ensure_initialized().await;

    let mut requested_model = body
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("claude-3-5-sonnet-20241022")
        .to_string();

    let cfg = get_config();
    if let Some(entry) = cfg.model_mapping.get(&requested_model) {
        if let Some(ref target) = entry.mapping {
            if !target.is_empty() {
                logger().info(format!(
                    "[Server] Mapping model {requested_model} -> {target}"
                ));
                requested_model = target.clone();
            }
        }
    } else if let Some(target) = resolve_builtin_model_mapping(&requested_model) {
        logger().info(format!(
            "[Server] Built-in mapping model {requested_model} -> {target}"
        ));
        requested_model = target.to_string();
    }

    let model_id = requested_model.clone();
    body["model"] = Value::String(model_id.clone());

    if !is_valid_model(&model_id, &state.account_manager).await {
        let err = NovoidError::InvalidRequest {
            message: format!("Invalid model: {model_id}. Use /v1/models to see available models."),
        };
        let (st, json_body) = err.to_anthropic_response();
        return (st, Json(json_body)).into_response();
    }

    if state.account_manager.is_all_rate_limited(Some(&model_id)).await
        && state
            .account_manager
            .should_optimistic_retry(&model_id, 5 * 60 * 1000)
            .await
    {
        logger().warn(format!(
            "[Server] All supporting accounts rate-limited for {model_id}. Resetting {model_id} limits for optimistic retry."
        ));
        state
            .account_manager
            .reset_all_rate_limits(Some(&model_id))
            .await;
    }

    let Some(messages) = body.get("messages").and_then(|v| v.as_array()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "type": "error",
                "error": {
                    "type": "invalid_request_error",
                    "message": "messages is required and must be an array"
                }
            })),
        )
            .into_response();
    };

    if messages.len() == 1 && messages[0].get("content").and_then(|v| v.as_str()) == Some("count") {
        return Json(json!({})).into_response();
    }

    if body.get("max_tokens").is_none() || body["max_tokens"].is_null() {
        body["max_tokens"] = Value::from(4096);
    }

    let is_stream = body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    logger().info(format!(
        "[API] Request for model: {model_id}, stream: {is_stream}"
    ));

    let no_fb_hdr = headers
        .get("x-novoid-no-fallback")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let disable_fallback = no_fb_hdr == "1" || no_fb_hdr == "true";
    let effective_fallback = state.fallback_enabled && !disable_fallback;

    if is_stream {
        match send_message_stream_prepared(&body, &state.account_manager, effective_fallback).await {
            Ok((actual_model, mut tail_stream)) => {
                usage_stats::track(&actual_model);

                let sse_stream = async_stream::stream! {
                    let format_sse = |ev: &Value| -> Bytes {
                        let ev_type = ev.get("type").and_then(|v| v.as_str()).unwrap_or("message");
                        let data_str = serde_json::to_string(ev).unwrap_or_default();
                        Bytes::from(format!("event: {ev_type}\ndata: {data_str}\n\n"))
                    };

                    // Immediately flush an SSE comment frame so clients/proxies receive HTTP 200 headers right away
                    yield Ok::<Bytes, std::convert::Infallible>(Bytes::from_static(b": keepalive\n\n"));

                    loop {
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(2),
                            tail_stream.next(),
                        )
                        .await
                        {
                            Ok(Some(Ok(ev))) => yield Ok(format_sse(&ev)),
                            Ok(Some(Err(err))) => {
                                logger().error(format!("[API] Mid-stream error: {err}"));
                                let (_, err_json) = err.to_anthropic_response();
                                let data_str = serde_json::to_string(&err_json).unwrap_or_default();
                                yield Ok(Bytes::from(format!("event: error\ndata: {data_str}\n\n")));
                                break;
                            }
                            Ok(None) => break,
                            Err(_) => {
                                // Keep SSE connection active while upstream thinking model computes first chunk
                                yield Ok(Bytes::from_static(b": keepalive\n\n"));
                            }
                        }
                    }
                };

                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .header(header::CACHE_CONTROL, "no-cache")
                    .header(header::CONNECTION, "keep-alive")
                    .header("X-Accel-Buffering", "no")
                    .body(Body::from_stream(sse_stream))
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
            }
            Err(err) => {
                logger().error(format!("[API] Initial stream error: {err}"));
                if err.is_auth_error() {
                    state.account_manager.clear_project_cache(None).await;
                    state.account_manager.clear_token_cache(None).await;
                }
                let (status, err_json) = err.to_anthropic_response();
                (status, Json(err_json)).into_response()
            }
        }
    } else {
        match send_message(&body, &state.account_manager, effective_fallback).await {
            Ok(resp) => {
                let tracked_model = resp
                    .get("model")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&model_id);
                usage_stats::track(tracked_model);
                Json(resp).into_response()
            }
            Err(err) => {
                logger().error(format!("[API] Error: {err}"));
                if err.is_auth_error() {
                    state.account_manager.clear_project_cache(None).await;
                    state.account_manager.clear_token_cache(None).await;
                }
                let (status, err_json) = err.to_anthropic_response();
                (status, Json(err_json)).into_response()
            }
        }
    }
}

async fn handle_health(State(state): State<AppState>) -> Response {
    let start = std::time::Instant::now();
    state.account_manager.ensure_initialized().await;

    let status = state.account_manager.get_status().await;
    let all_accounts = state.account_manager.get_all_accounts().await;

    let mut handles = Vec::with_capacity(all_accounts.len());
    for acc in all_accounts {
        let am = Arc::clone(&state.account_manager);
        handles.push(tokio::spawn(async move {
            let now = now_ms();
            let active_limits: Vec<u64> = acc
                .model_rate_limits
                .values()
                .filter(|l| l.is_rate_limited && l.reset_time.unwrap_or(0) > now)
                .filter_map(|l| l.reset_time)
                .collect();
            let is_rl = !active_limits.is_empty();
            let soonest = active_limits.into_iter().min();
            let cooldown_rem = soonest.map(|s| s.saturating_sub(now)).unwrap_or(0);

            let last_used_iso = acc.last_used.and_then(|ms| {
                chrono::DateTime::from_timestamp_millis(ms as i64).map(|d| d.to_rfc3339())
            });

            if acc.is_invalid {
                let lower = acc.invalid_reason.as_deref().unwrap_or("").to_lowercase();
                let is_banned =
                    lower.contains("banned") || lower.contains("terms of service");
                return json!({
                    "email": acc.email,
                    "lastUsed": last_used_iso,
                    "modelRateLimits": acc.model_rate_limits,
                    "rateLimitCooldownRemaining": cooldown_rem,
                    "status": if is_banned { "banned" } else { "invalid" },
                    "error": acc.invalid_reason,
                    "models": {}
                });
            }

            match am.get_token_for_account(&acc).await {
                Ok(token) => {
                    let project_id = acc.subscription.project_id.as_deref();
                    match get_model_quotas(&token, project_id).await {
                        Ok(quotas) => {
                            let mut formatted = serde_json::Map::new();
                            for (mid, info) in quotas {
                                let rem = info
                                    .remaining_fraction
                                    .map(|f| format!("{}%", (f * 100.0).round() as i64))
                                    .unwrap_or_else(|| "N/A".to_string());
                                formatted.insert(
                                    mid,
                                    json!({
                                        "remaining": rem,
                                        "remainingFraction": info.remaining_fraction,
                                        "resetTime": info.reset_time
                                    }),
                                );
                            }
                            json!({
                                "email": acc.email,
                                "lastUsed": last_used_iso,
                                "modelRateLimits": acc.model_rate_limits,
                                "rateLimitCooldownRemaining": cooldown_rem,
                                "status": if is_rl { "rate-limited" } else { "ok" },
                                "models": formatted
                            })
                        }
                        Err(err) => json!({
                            "email": acc.email,
                            "lastUsed": last_used_iso,
                            "modelRateLimits": acc.model_rate_limits,
                            "rateLimitCooldownRemaining": cooldown_rem,
                            "status": "error",
                            "error": err,
                            "models": {}
                        }),
                    }
                }
                Err(err) => json!({
                    "email": acc.email,
                    "lastUsed": last_used_iso,
                    "modelRateLimits": acc.model_rate_limits,
                    "rateLimitCooldownRemaining": cooldown_rem,
                    "status": "error",
                    "error": err.to_string(),
                    "models": {}
                }),
            }
        }));
    }

    let mut detailed = Vec::new();
    for h in handles {
        if let Ok(val) = h.await {
            detailed.push(val);
        }
    }

    Json(json!({
        "status": "ok",
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "latencyMs": start.elapsed().as_millis(),
        "summary": status["summary"],
        "counts": {
            "total": status["total"],
            "available": status["available"],
            "rateLimited": status["rateLimited"],
            "invalid": status["invalid"]
        },
        "accounts": detailed
    }))
    .into_response()
}

async fn handle_account_limits(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    state.account_manager.ensure_initialized().await;
    let format = params.get("format").map(|s| s.as_str()).unwrap_or("json");
    let include_history = params
        .get("includeHistory")
        .map(|s| s == "true")
        .unwrap_or(false);
    let force_refresh = params
        .get("refresh")
        .map(|s| s == "true")
        .unwrap_or(false);

    let all_accounts_initial = state.account_manager.get_all_accounts().await;

    if format == "json" && !force_refresh {
        if let Some(ref cached) = *limits_cache().read().await {
            if now_ms().saturating_sub(cached.cached_at) < ACCOUNT_LIMITS_CACHE_TTL_MS
                && cached.account_count == all_accounts_initial.len()
            {
                let mut resp = cached.payload.clone();
                if include_history {
                    resp["history"] = usage_stats::get_history();
                }
                return Json(resp).into_response();
            }
        }
    }

    let _flight = limits_flight_lock().lock().await;
    let all_accounts = state.account_manager.get_all_accounts().await;

    // Re-check cache after acquiring single-flight lock
    if format == "json" && !force_refresh {
        if let Some(ref cached) = *limits_cache().read().await {
            if now_ms().saturating_sub(cached.cached_at) < ACCOUNT_LIMITS_CACHE_TTL_MS
                && cached.account_count == all_accounts.len()
            {
                let mut resp = cached.payload.clone();
                if include_history {
                    resp["history"] = usage_stats::get_history();
                }
                return Json(resp).into_response();
            }
        }
    }

    let mut handles = Vec::with_capacity(all_accounts.len());
    for acc in all_accounts.clone() {
        let am = Arc::clone(&state.account_manager);
        handles.push(tokio::spawn(async move {
            if acc.is_invalid {
                return (
                    acc.email.clone(),
                    "invalid".to_string(),
                    acc.invalid_reason.clone(),
                    acc.subscription.clone(),
                    HashMap::new(),
                );
            }
            match am.get_token_for_account(&acc).await {
                Ok(token) => {
                    let has_cached_sub = acc.subscription.tier != "unknown"
                        && acc.subscription.project_id.is_some();

                    let (sub_res, quotas_res) = if !force_refresh && has_cached_sub {
                        (
                            Ok((acc.subscription.tier.clone(), acc.subscription.project_id.clone())),
                            get_model_quotas(&token, acc.subscription.project_id.as_deref()).await,
                        )
                    } else {
                        let existing_pid = acc
                            .subscription
                            .project_id
                            .as_deref()
                            .or(acc.project_id.as_deref());
                        tokio::join!(
                            get_subscription_tier(&token),
                            get_model_quotas(&token, existing_pid)
                        )
                    };

                    let sub = match sub_res {
                        Ok((tier, pid)) => crate::account::types::SubscriptionInfo {
                            tier,
                            project_id: pid,
                            detected_at: Some(Value::from(now_ms())),
                        },
                        Err(err) => {
                            if err.starts_with("ACCOUNT_BANNED:") {
                                am.mark_invalid(
                                    &acc.email,
                                    "Account banned — Gemini disabled for Terms of Service violation",
                                    None,
                                )
                                .await;
                                return (
                                    acc.email,
                                    "banned".to_string(),
                                    Some("Account banned — Gemini disabled for Terms of Service violation".to_string()),
                                    acc.subscription,
                                    HashMap::new(),
                                );
                            }
                            return (
                                acc.email,
                                "error".to_string(),
                                Some(err),
                                acc.subscription,
                                HashMap::new(),
                            );
                        }
                    };

                    match quotas_res {
                        Ok(quotas) => {
                            am.update_account_subscription_and_quota(
                                &acc.email,
                                sub.clone(),
                                quotas.clone(),
                            )
                            .await;
                            (acc.email, "ok".to_string(), None, sub, quotas)
                        }
                        Err(err) => {
                            if err.starts_with("ACCOUNT_BANNED:") {
                                am.mark_invalid(
                                    &acc.email,
                                    "Account banned — Gemini disabled for Terms of Service violation",
                                    None,
                                )
                                .await;
                                (
                                    acc.email,
                                    "banned".to_string(),
                                    Some("Account banned — Gemini disabled for Terms of Service violation".to_string()),
                                    acc.subscription,
                                    HashMap::new(),
                                )
                            } else {
                                (acc.email, "error".to_string(), Some(err), acc.subscription, HashMap::new())
                            }
                        }
                    }
                }
                Err(err) => (
                    acc.email,
                    "error".to_string(),
                    Some(err.to_string()),
                    acc.subscription,
                    HashMap::new(),
                ),
            }
        }));
    }

    let mut limits_results = Vec::new();
    let mut model_set = std::collections::BTreeSet::new();
    for h in handles {
        if let Ok(item) = h.await {
            for mid in item.4.keys() {
                model_set.insert(mid.clone());
            }
            limits_results.push(item);
        }
    }

    let sorted_models: Vec<String> = model_set.into_iter().collect();

    if format == "table" {
        let mut lines = Vec::new();
        let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        lines.push(format!("Account Limits ({timestamp})"));
        let status = state.account_manager.get_status().await;
        lines.push(format!(
            "Accounts: {} total, {} available, {} rate-limited, {} invalid",
            status["total"], status["available"], status["rateLimited"], status["invalid"]
        ));
        lines.push(String::new());

        let header = format!(
            "{:<25}{:<15}{:<25}{}",
            "Account", "Status", "Last Used", "Quota Reset"
        );
        lines.push(header);
        lines.push("─".repeat(90));

        for (email, st, err_msg, _, quotas) in &limits_results {
            let short_email: String = email.split('@').next().unwrap_or(email).chars().take(22).collect();
            let acc_obj = all_accounts.iter().find(|a| &a.email == email);
            let last_used = acc_obj
                .and_then(|a| a.last_used)
                .and_then(|ms| chrono::DateTime::from_timestamp_millis(ms as i64))
                .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string())
                .unwrap_or_else(|| "never".to_string());

            let acc_status = if st != "ok" {
                st.clone()
            } else {
                let total = quotas.len();
                let exhausted = quotas
                    .values()
                    .filter(|q| q.remaining_fraction.map(|f| f == 0.0).unwrap_or(true))
                    .count();
                if exhausted == 0 {
                    "ok".to_string()
                } else {
                    format!("({exhausted}/{total}) limited")
                }
            };

            let claude_model = sorted_models.iter().find(|m| m.contains("claude"));
            let reset_str = claude_model
                .and_then(|m| quotas.get(m))
                .and_then(|q| q.reset_time.as_deref())
                .unwrap_or("-");

            lines.push(format!(
                "{:<25}{:<15}{:<25}{}",
                short_email, acc_status, last_used, reset_str
            ));
            if let Some(e) = err_msg {
                lines.push(format!("  └─ {e}"));
            }
        }
        lines.push(String::new());

        let model_col_width = sorted_models
            .iter()
            .map(|m| m.len())
            .max()
            .unwrap_or(28)
            .max(28)
            + 2;
        let mut model_hdr = format!("{:<width$}", "Model", width = model_col_width);
        for (email, _, _, _, _) in &limits_results {
            let short: String = email.split('@').next().unwrap_or(email).chars().take(26).collect();
            model_hdr.push_str(&format!("{:<30}", short));
        }
        lines.push(model_hdr);
        lines.push("─".repeat(model_col_width + limits_results.len() * 30));

        for mid in &sorted_models {
            let mut row = format!("{:<width$}", mid, width = model_col_width);
            for (_, st, _, _, quotas) in &limits_results {
                let cell = if st != "ok" && st != "rate-limited" {
                    format!("[{st}]")
                } else if let Some(q) = quotas.get(mid) {
                    match q.remaining_fraction {
                        Some(f) if f > 0.0 => format!("{}%", (f * 100.0).round() as i64),
                        _ => {
                            if let Some(ref rt) = q.reset_time {
                                if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(rt) {
                                    let diff = dt.timestamp_millis() - chrono::Utc::now().timestamp_millis();
                                    if diff > 0 {
                                        format!("0% (wait {})", format_duration(diff as u64))
                                    } else {
                                        "0% (resetting...)".to_string()
                                    }
                                } else {
                                    "0% (exhausted)".to_string()
                                }
                            } else {
                                "0% (exhausted)".to_string()
                            }
                        }
                    }
                } else {
                    "-".to_string()
                };
                row.push_str(&format!("{:<30}", cell));
            }
            lines.push(row);
        }

        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(Body::from(lines.join("\n")))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    }

    let cfg = get_config();
    let status = state.account_manager.get_status().await;
    let meta_map: HashMap<String, Value> = status
        .get("accounts")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    a.get("email")
                        .and_then(|e| e.as_str())
                        .map(|e| (e.to_string(), a.clone()))
                })
                .collect()
        })
        .unwrap_or_default();

    let accounts_json: Vec<Value> = limits_results
        .into_iter()
        .map(|(email, st, err, sub, quotas)| {
            let meta = meta_map.get(&email);
            let mut limits_map = serde_json::Map::new();
            for mid in &sorted_models {
                if let Some(q) = quotas.get(mid) {
                    let rem = q
                        .remaining_fraction
                        .map(|f| format!("{}%", (f * 100.0).round() as i64))
                        .unwrap_or_else(|| "N/A".to_string());
                    limits_map.insert(
                        mid.clone(),
                        json!({
                            "remaining": rem,
                            "remainingFraction": q.remaining_fraction,
                            "resetTime": q.reset_time
                        }),
                    );
                } else {
                    limits_map.insert(mid.clone(), Value::Null);
                }
            }

            json!({
                "email": email,
                "status": st,
                "error": err,
                "source": meta.and_then(|m| m.get("source")).cloned().unwrap_or(json!("unknown")),
                "enabled": meta.and_then(|m| m.get("enabled")).and_then(|v| v.as_bool()).unwrap_or(true),
                "projectId": meta.and_then(|m| m.get("projectId")).cloned().unwrap_or(Value::Null),
                "isInvalid": meta.and_then(|m| m.get("isInvalid")).and_then(|v| v.as_bool()).unwrap_or(false),
                "invalidReason": meta.and_then(|m| m.get("invalidReason")).cloned().unwrap_or(Value::Null),
                "verifyUrl": meta.and_then(|m| m.get("verifyUrl")).cloned().unwrap_or(Value::Null),
                "lastUsed": meta.and_then(|m| m.get("lastUsed")).cloned().unwrap_or(Value::Null),
                "modelRateLimits": meta.and_then(|m| m.get("modelRateLimits")).cloned().unwrap_or(json!({})),
                "quotaThreshold": meta.and_then(|m| m.get("quotaThreshold")).cloned().unwrap_or(Value::Null),
                "modelQuotaThresholds": meta.and_then(|m| m.get("modelQuotaThresholds")).cloned().unwrap_or(json!({})),
                "subscription": sub,
                "limits": limits_map
            })
        })
        .collect();

    let resp = json!({
        "timestamp": chrono::Local::now().to_string(),
        "totalAccounts": all_accounts.len(),
        "models": sorted_models,
        "modelConfig": cfg.model_mapping,
        "globalQuotaThreshold": cfg.global_quota_threshold,
        "accounts": accounts_json
    });

    {
        let mut guard = limits_cache().write().await;
        *guard = Some(CachedLimits {
            cached_at: now_ms(),
            account_count: all_accounts.len(),
            payload: resp.clone(),
        });
    }

    // Persist updated subscription/quota snapshots in background
    let am_save = Arc::clone(&state.account_manager);
    tokio::spawn(async move {
        am_save.save_to_disk().await;
    });

    let mut out = resp;
    if include_history {
        out["history"] = usage_stats::get_history();
    }

    let _ = account_config_path();
    Json(out).into_response()
}
