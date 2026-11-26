use crate::constants::{
    antigravity_headers, client_metadata, oauth_callback_port, oauth_client_id,
    oauth_client_secret, oauth_default_redirect_uri, ANTIGRAVITY_ENDPOINT_FALLBACKS,
    OAUTH_AUTH_URL, OAUTH_CALLBACK_FALLBACK_PORTS, OAUTH_SCOPES, OAUTH_TOKEN_URL,
    OAUTH_USER_INFO_URL, ONBOARD_USER_ENDPOINTS,
};
use crate::logger::logger;
use crate::utils::{http_client, maybe_throttle};
use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngCore;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, Mutex};
use url::Url;

static ONBOARD_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static ONBOARD_COOLDOWN_UNTIL_MS: AtomicU64 = AtomicU64::new(0);

fn onboard_lock() -> &'static Mutex<()> {
    ONBOARD_LOCK.get_or_init(|| Mutex::new(()))
}

fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub struct RefreshParts {
    pub refresh_token: String,
    pub project_id: Option<String>,
    pub managed_project_id: Option<String>,
}

pub fn parse_refresh_parts(refresh: &str) -> RefreshParts {
    let mut parts = refresh.split('|');
    let refresh_token = parts.next().unwrap_or("").to_string();
    let project_id = parts.next().filter(|s| !s.is_empty()).map(|s| s.to_string());
    let managed_project_id = parts.next().filter(|s| !s.is_empty()).map(|s| s.to_string());
    RefreshParts {
        refresh_token,
        project_id,
        managed_project_id,
    }
}

pub fn format_refresh_parts(
    refresh_token: &str,
    project_id: Option<&str>,
    managed_project_id: Option<&str>,
) -> String {
    let base = format!("{}|{}", refresh_token, project_id.unwrap_or(""));
    if let Some(m) = managed_project_id.filter(|s| !s.is_empty()) {
        format!("{base}|{m}")
    } else {
        base
    }
}

pub struct AuthUrlResult {
    pub url: String,
    pub verifier: String,
    pub state: String,
    pub redirect_uri: String,
}

pub fn get_authorization_url(custom_redirect_uri: Option<&str>) -> AuthUrlResult {
    let mut verifier_bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut verifier_bytes);
    let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);

    let challenge_hash = Sha256::digest(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(challenge_hash);

    let mut state_bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut state_bytes);
    let state = hex::encode(state_bytes);

    let redirect_uri = custom_redirect_uri
        .map(|s| s.to_string())
        .unwrap_or_else(oauth_default_redirect_uri);

    let mut auth_url = Url::parse(OAUTH_AUTH_URL).expect("valid auth url");
    auth_url
        .query_pairs_mut()
        .append_pair("client_id", &oauth_client_id())
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", &OAUTH_SCOPES.join(" "))
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent")
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state);

    AuthUrlResult {
        url: auth_url.to_string(),
        verifier,
        state,
        redirect_uri,
    }
}

pub fn extract_code_from_input(input: &str) -> Result<(String, Option<String>), String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("No input provided".to_string());
    }

    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        let parsed = Url::parse(trimmed).map_err(|_| "Invalid URL format".to_string())?;
        let mut code = None;
        let mut state = None;
        let mut err = None;
        for (k, v) in parsed.query_pairs() {
            match k.as_ref() {
                "code" => code = Some(v.to_string()),
                "state" => state = Some(v.to_string()),
                "error" => err = Some(v.to_string()),
                _ => {}
            }
        }
        if let Some(e) = err {
            return Err(format!("OAuth error: {e}"));
        }
        let code = code.ok_or_else(|| "No authorization code found in URL".to_string())?;
        return Ok((code, state));
    }

    if trimmed.len() < 10 {
        return Err("Input is too short to be a valid authorization code".to_string());
    }
    Ok((trimmed.to_string(), None))
}

pub struct CallbackServerHandle {
    pub code_rx: oneshot::Receiver<Result<String, String>>,
    pub abort_tx: oneshot::Sender<()>,
}

pub async fn start_callback_server(
    expected_state: String,
    timeout_ms: u64,
) -> Result<CallbackServerHandle, String> {
    let host = std::env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
    let primary_port = oauth_callback_port();
    let mut ports = vec![primary_port];
    ports.extend_from_slice(&OAUTH_CALLBACK_FALLBACK_PORTS);

    let mut bound_listener = None;
    let mut bound_port = primary_port;
    let mut errors = Vec::new();

    for port in ports {
        let addr = format!("{host}:{port}");
        match TcpListener::bind(&addr).await {
            Ok(listener) => {
                bound_port = port;
                bound_listener = Some(listener);
                break;
            }
            Err(err) => {
                let msg = format!("Failed to bind port {port}: {err}");
                logger().warn(format!("[OAuth] {msg}"));
                errors.push(msg);
            }
        }
    }

    let listener = bound_listener.ok_or_else(|| {
        format!(
            "Failed to start OAuth callback server.\nErrors:\n{}",
            errors.join("\n")
        )
    })?;

    if bound_port != primary_port {
        logger().warn(format!(
            "[OAuth] Primary port {primary_port} unavailable, using fallback port {bound_port}"
        ));
    } else {
        logger().info(format!("[OAuth] Callback server listening on {host}:{bound_port}"));
    }

    let (code_tx, code_rx) = oneshot::channel::<Result<String, String>>();
    let (abort_tx, mut abort_rx) = oneshot::channel::<()>();
    let shared_tx = Arc::new(Mutex::new(Some(code_tx)));
    let expected = Arc::new(expected_state);

    let app = Router::new().route(
        "/oauth-callback",
        get({
            let shared_tx = Arc::clone(&shared_tx);
            let expected = Arc::clone(&expected);
            move |Query(params): Query<HashMap<String, String>>| {
                let shared_tx = Arc::clone(&shared_tx);
                let expected = Arc::clone(&expected);
                async move {
                    let finish = |res: Result<String, String>| async {
                        if let Some(tx) = shared_tx.lock().await.take() {
                            let _ = tx.send(res);
                        }
                    };

                    if let Some(err) = params.get("error") {
                        finish(Err(format!("OAuth error: {err}"))).await;
                        return (
                            StatusCode::BAD_REQUEST,
                            Html(format!("<h1>Authentication Failed</h1><p>Error: {err}</p>")),
                        );
                    }
                    if params.get("state").map(|s| s.as_str()) != Some(expected.as_str()) {
                        finish(Err("State mismatch".to_string())).await;
                        return (
                            StatusCode::BAD_REQUEST,
                            Html("<h1>Authentication Failed</h1><p>State mismatch.</p>".to_string()),
                        );
                    }
                    let Some(code) = params.get("code").filter(|s| !s.is_empty()).cloned() else {
                        finish(Err("No authorization code".to_string())).await;
                        return (
                            StatusCode::BAD_REQUEST,
                            Html("<h1>Authentication Failed</h1><p>No authorization code.</p>".to_string()),
                        );
                    };

                    finish(Ok(code)).await;
                    (
                        StatusCode::OK,
                        Html(
                            "<html><body style=\"font-family:system-ui;padding:40px;text-align:center;\"><h1 style=\"color:#16a34a;\">Authentication Successful!</h1><p>You can close this window.</p><script>setTimeout(()=>window.close(),1500);</script></body></html>".to_string(),
                        ),
                    )
                }
            }
        }),
    );

    tokio::spawn(async move {
        let server = axum::serve(listener, app).with_graceful_shutdown(async move {
            let timeout = tokio::time::sleep(std::time::Duration::from_millis(timeout_ms));
            tokio::select! {
                _ = &mut abort_rx => {}
                _ = timeout => {
                    if let Some(tx) = shared_tx.lock().await.take() {
                        let _ = tx.send(Err("OAuth callback timeout - no response received".to_string()));
                    }
                }
            }
        });
        let _ = server.await;
    });

    Ok(CallbackServerHandle { code_rx, abort_tx })
}

pub struct ExchangeResult {
    pub access_token: String,
    pub refresh_token: String,
}

pub async fn exchange_code(
    code: &str,
    verifier: &str,
    custom_redirect_uri: Option<&str>,
) -> Result<ExchangeResult, String> {
    let redirect_uri = custom_redirect_uri
        .map(|s| s.to_string())
        .unwrap_or_else(oauth_default_redirect_uri);

    maybe_throttle().await;
    let resp = http_client()
        .post(OAUTH_TOKEN_URL)
        .form(&[
            ("client_id", oauth_client_id()),
            ("client_secret", oauth_client_secret()),
            ("code", code.to_string()),
            ("code_verifier", verifier.to_string()),
            ("grant_type", "authorization_code".to_string()),
            ("redirect_uri", redirect_uri),
        ])
        .send()
        .await
        .map_err(|e| format!("Token exchange request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        logger().error(format!("[OAuth] Token exchange failed: {status} {text}"));
        return Err(format!("Token exchange failed: {text}"));
    }

    let tokens: Value = resp
        .json()
        .await
        .map_err(|e| format!("Invalid token JSON: {e}"))?;
    let access_token = tokens
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "No access token received".to_string())?
        .to_string();
    let refresh_token = tokens
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Ok(ExchangeResult {
        access_token,
        refresh_token,
    })
}

pub async fn refresh_access_token(composite_refresh: &str) -> Result<String, (String, bool)> {
    let parts = parse_refresh_parts(composite_refresh);
    maybe_throttle().await;

    let resp = http_client()
        .post(OAUTH_TOKEN_URL)
        .form(&[
            ("client_id", oauth_client_id()),
            ("client_secret", oauth_client_secret()),
            ("refresh_token", parts.refresh_token),
            ("grant_type", "refresh_token".to_string()),
        ])
        .send()
        .await
        .map_err(|e| (format!("Token refresh network error: {e}"), true))?;

    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err((format!("Token refresh failed: {text}"), false));
    }

    let tokens: Value = resp
        .json()
        .await
        .map_err(|e| (format!("Invalid refresh response: {e}"), false))?;

    tokens
        .get("access_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| ("No access_token in refresh response".to_string(), false))
}

pub async fn get_user_email(access_token: &str) -> Result<String, String> {
    maybe_throttle().await;
    let resp = http_client()
        .get(OAUTH_USER_INFO_URL)
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|e| format!("Failed to get user info: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("Failed to get user info: {}", resp.status()));
    }

    let info: Value = resp
        .json()
        .await
        .map_err(|e| format!("Invalid user info JSON: {e}"))?;
    info.get("email")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "Missing email in user info".to_string())
}

pub fn get_default_tier_id(allowed_tiers: Option<&Value>) -> Option<String> {
    let arr = allowed_tiers?.as_array()?;
    if arr.is_empty() {
        return None;
    }
    for t in arr {
        if t.get("isDefault").and_then(|v| v.as_bool()) == Some(true) {
            if let Some(id) = t.get("id").and_then(|v| v.as_str()) {
                return Some(id.to_string());
            }
        }
    }
    arr[0]
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

pub async fn onboard_user(
    token: &str,
    tier_id: &str,
    project_id: Option<&str>,
) -> Option<String> {
    let now = now_epoch_ms();
    if now < ONBOARD_COOLDOWN_UNTIL_MS.load(Ordering::Relaxed) {
        return None;
    }
    let _guard = onboard_lock().lock().await;
    let now = now_epoch_ms();
    if now < ONBOARD_COOLDOWN_UNTIL_MS.load(Ordering::Relaxed) {
        return None;
    }

    let mut metadata = client_metadata();
    if let Some(pid) = project_id {
        if let Some(obj) = metadata.as_object_mut() {
            obj.insert("duetProject".to_string(), Value::String(pid.to_string()));
        }
    }
    let body = json!({
        "tierId": tier_id,
        "metadata": metadata
    });

    for endpoint in ONBOARD_USER_ENDPOINTS {
        for attempt in 0..3 {
            maybe_throttle().await;
            let resp = http_client()
                .post(format!("{endpoint}/v1internal:onboardUser"))
                .bearer_auth(token)
                .headers(antigravity_headers())
                .json(&body)
                .send()
                .await;

            match resp {
                Ok(r) if r.status().is_success() => {
                    if let Ok(data) = r.json::<Value>().await {
                        let done = data.get("done").and_then(|v| v.as_bool()).unwrap_or(false);
                        let managed = data
                            .get("response")
                            .and_then(|r| r.get("cloudaicompanionProject"))
                            .and_then(|c| c.get("id"))
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        if done {
                            if managed.is_some() {
                                return managed;
                            }
                            if let Some(pid) = project_id {
                                return Some(pid.to_string());
                            }
                        }
                        if attempt < 2 {
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        }
                    }
                }
                Ok(r) => {
                    let st = r.status();
                    logger().warn(format!(
                        "[Onboarding] onboardUser failed at {endpoint}: {st}"
                    ));
                    if st.as_u16() == 429 {
                        ONBOARD_COOLDOWN_UNTIL_MS
                            .store(now_epoch_ms() + 120_000, Ordering::Relaxed);
                        return None;
                    }
                    break;
                }
                Err(err) => {
                    logger().warn(format!("[Onboarding] onboardUser error at {endpoint}: {err}"));
                    break;
                }
            }
        }
    }
    None
}

pub async fn discover_project_id(access_token: &str) -> Option<String> {
    let mut load_data = None;
    for endpoint in ANTIGRAVITY_ENDPOINT_FALLBACKS {
        maybe_throttle().await;
        let resp = http_client()
            .post(format!("{endpoint}/v1internal:loadCodeAssist"))
            .bearer_auth(access_token)
            .headers(antigravity_headers())
            .json(&json!({ "metadata": client_metadata() }))
            .send()
            .await;

        if let Ok(r) = resp {
            if !r.status().is_success() {
                continue;
            }
            if let Ok(data) = r.json::<Value>().await {
                if let Some(s) = data
                    .get("cloudaicompanionProject")
                    .and_then(|v| v.as_str())
                {
                    return Some(s.to_string());
                }
                if let Some(id) = data
                    .get("cloudaicompanionProject")
                    .and_then(|v| v.get("id"))
                    .and_then(|v| v.as_str())
                {
                    return Some(id.to_string());
                }
                load_data = Some(data);
                break;
            }
        }
    }

    if let Some(data) = load_data {
        let tier_id = get_default_tier_id(data.get("allowedTiers"))
            .unwrap_or_else(|| "FREE".to_string());
        if let Some(onboarded) = onboard_user(access_token, &tier_id, None).await {
            return Some(onboarded);
        }
    }
    None
}

pub struct CompletedOAuthAccount {
    pub email: String,
    pub refresh_token: String,
    pub access_token: String,
    pub project_id: Option<String>,
}

pub async fn complete_oauth_flow(
    code: &str,
    verifier: &str,
    custom_redirect_uri: Option<&str>,
) -> Result<CompletedOAuthAccount, String> {
    let tokens = exchange_code(code, verifier, custom_redirect_uri).await?;
    let email = get_user_email(&tokens.access_token).await?;
    let project_id = discover_project_id(&tokens.access_token).await;
    Ok(CompletedOAuthAccount {
        email,
        refresh_token: tokens.refresh_token,
        access_token: tokens.access_token,
        project_id,
    })
}
