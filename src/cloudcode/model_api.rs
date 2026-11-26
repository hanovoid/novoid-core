use crate::account::types::ModelQuota;
use crate::account::AccountManager;
use crate::cloudcode::model_registry::record_models;
use crate::constants::{
    antigravity_headers, client_metadata, get_model_family, ANTIGRAVITY_ENDPOINT_FALLBACKS,
    LOAD_CODE_ASSIST_ENDPOINTS, MODEL_VALIDATION_CACHE_TTL_MS,
};
use crate::logger::logger;
use crate::utils::{http_client, maybe_throttle, now_ms};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use tokio::sync::{Mutex, RwLock};

#[derive(Default)]
struct ModelCache {
    valid_models: HashSet<String>,
    models_by_email: HashMap<String, HashSet<String>>,
    model_entries_by_email: HashMap<String, HashMap<String, Value>>,
    model_entries: HashMap<String, Value>,
    last_fetched: u64,
}

static MODEL_CACHE: OnceLock<RwLock<ModelCache>> = OnceLock::new();
static FETCH_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn model_cache() -> &'static RwLock<ModelCache> {
    MODEL_CACHE.get_or_init(|| RwLock::new(ModelCache::default()))
}

fn fetch_lock() -> &'static Mutex<()> {
    FETCH_LOCK.get_or_init(|| Mutex::new(()))
}

pub async fn clear_model_cache() {
    let mut guard = model_cache().write().await;
    guard.valid_models.clear();
    guard.models_by_email.clear();
    guard.model_entries_by_email.clear();
    guard.model_entries.clear();
    guard.last_fetched = 0;
}

fn is_supported_model(model_id: &str) -> bool {
    let fam = get_model_family(model_id);
    fam == "claude" || fam == "gemini"
}

fn is_disabled_placeholder(model_id: &str) -> bool {
    model_id.ends_with("-disabled")
}

fn is_hidden_model(model_id: &str) -> bool {
    model_id == "gemini-pro-agent" || is_disabled_placeholder(model_id)
}

fn extract_account_model_data(
    data: &Value,
) -> (Vec<(String, Value)>, HashSet<String>, HashMap<String, ModelQuota>) {
    let Some(models_obj) = data.get("models").and_then(|v| v.as_object()) else {
        return (Vec::new(), HashSet::new(), HashMap::new());
    };

    record_models(data);

    let supported_entries: Vec<(String, Value)> = models_obj
        .iter()
        .filter(|(id, _)| is_supported_model(id) && !is_disabled_placeholder(id))
        .map(|(id, v)| (id.clone(), v.clone()))
        .collect();

    let has_any_quota = supported_entries
        .iter()
        .any(|(_, v)| v.get("quotaInfo").is_some());

    let mut supported_set = HashSet::new();
    let mut quotas = HashMap::new();

    for (id, model_data) in &supported_entries {
        if let Some(qi) = model_data.get("quotaInfo") {
            supported_set.insert(id.clone());
            let reset_time = qi
                .get("resetTime")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let remaining_fraction = qi
                .get("remainingFraction")
                .and_then(|v| v.as_f64())
                .or_else(|| reset_time.as_ref().map(|_| 0.0));
            quotas.insert(
                id.clone(),
                ModelQuota {
                    remaining_fraction,
                    reset_time,
                },
            );
        } else if !has_any_quota {
            supported_set.insert(id.clone());
        }
    }

    (supported_entries, supported_set, quotas)
}

pub async fn fetch_available_models(
    token: &str,
    project_id: Option<&str>,
) -> Result<Value, String> {
    let body = match project_id {
        Some(pid) if !pid.is_empty() => json!({ "project": pid }),
        _ => json!({}),
    };

    for endpoint in ANTIGRAVITY_ENDPOINT_FALLBACKS {
        maybe_throttle().await;
        let url = format!("{endpoint}/v1internal:fetchAvailableModels");
        let resp = http_client()
            .post(&url)
            .bearer_auth(token)
            .headers(antigravity_headers())
            .json(&body)
            .send()
            .await;

        match resp {
            Ok(r) => {
                let status = r.status();
                if !status.is_success() {
                    let err_text = r.text().await.unwrap_or_default();
                    logger().warn(format!(
                        "[CloudCode] fetchAvailableModels error at {endpoint}: {status}"
                    ));
                    if status.as_u16() == 403 {
                        let lower = err_text.to_lowercase();
                        if lower.contains("has been disabled")
                            && lower.contains("violation of terms of service")
                        {
                            return Err(format!("ACCOUNT_BANNED: {err_text}"));
                        }
                    }
                    continue;
                }
                if let Ok(json_val) = r.json::<Value>().await {
                    return Ok(json_val);
                }
            }
            Err(err) => {
                logger().warn(format!(
                    "[CloudCode] fetchAvailableModels failed at {endpoint}: {err}"
                ));
            }
        }
    }

    Err("Failed to fetch available models from all endpoints".to_string())
}

pub async fn populate_model_cache_for_accounts(
    account_manager: &Arc<AccountManager>,
    force_refresh: bool,
) {
    let now = now_ms();
    if !force_refresh {
        let guard = model_cache().read().await;
        if !guard.valid_models.is_empty()
            && now.saturating_sub(guard.last_fetched) < MODEL_VALIDATION_CACHE_TTL_MS
        {
            return;
        }
    }

    let _single_flight = fetch_lock().lock().await;
    if !force_refresh {
        let guard = model_cache().read().await;
        if !guard.valid_models.is_empty()
            && now_ms().saturating_sub(guard.last_fetched) < MODEL_VALIDATION_CACHE_TTL_MS
        {
            return;
        }
    }

    let accounts: Vec<_> = account_manager
        .get_all_accounts()
        .await
        .into_iter()
        .filter(|a| !a.is_invalid && a.enabled)
        .collect();

    if accounts.is_empty() {
        clear_model_cache().await;
        return;
    }

    let active_emails: HashSet<String> = accounts.iter().map(|a| a.email.clone()).collect();

    let mut handles = Vec::with_capacity(accounts.len());
    for acc in accounts {
        let am = Arc::clone(account_manager);
        handles.push(tokio::spawn(async move {
            let token = am.get_token_for_account(&acc).await.ok()?;
            let mut project_id = acc
                .subscription
                .project_id
                .clone()
                .or_else(|| acc.project_id.clone());
            if project_id.is_none() {
                project_id = Some(
                    am.get_project_for_account_with_onboard(&acc, &token, false)
                        .await,
                );
            }
            let data = fetch_available_models(&token, project_id.as_deref())
                .await
                .ok()?;
            Some((acc.email, data))
        }));
    }

    let mut success_count = 0usize;
    let mut new_by_email = HashMap::new();
    let mut new_entries_by_email = HashMap::new();

    for handle in handles {
        if let Ok(Some((email, data))) = handle.await {
            if data.get("models").is_none() {
                continue;
            }
            success_count += 1;
            let (entries, supported_set, quotas) = extract_account_model_data(&data);

            account_manager
                .update_account_models_and_quota(&email, supported_set.clone(), quotas)
                .await;

            let mut entry_map = HashMap::new();
            for (mid, mdata) in entries {
                if supported_set.contains(&mid) {
                    entry_map.insert(mid, mdata);
                }
            }
            new_by_email.insert(email.clone(), supported_set);
            new_entries_by_email.insert(email, entry_map);
        }
    }

    let mut next_valid = HashSet::new();
    let mut next_entries: HashMap<String, Value> = HashMap::new();

    for email in &active_emails {
        if let Some(emap) = new_entries_by_email.get(email) {
            for (mid, mdata) in emap {
                next_valid.insert(mid.clone());
                let replace = !next_entries.contains_key(mid) || mdata.get("quotaInfo").is_some();
                if replace {
                    next_entries.insert(mid.clone(), mdata.clone());
                }
            }
        }
    }

    if success_count > 0 && !next_valid.is_empty() {
        let mut guard = model_cache().write().await;
        guard.models_by_email = new_by_email;
        guard.model_entries_by_email = new_entries_by_email;
        guard.valid_models = next_valid;
        guard.model_entries = next_entries;
        guard.last_fetched = now_ms();
    }
}

const BUILTIN_FALLBACK_MODELS: &[&str] = &[
    "claude-opus-4-6-thinking",
    "claude-opus-5-5-high",
    "claude-opus-5-5-low",
    "claude-opus-5-5-medium",
    "claude-sonnet-4-6",
    "claude-sonnet-5-5-high",
    "claude-sonnet-5-5-low",
    "claude-sonnet-5-5-medium",
    "gemini-2.5-flash",
    "gemini-2.5-flash-lite",
    "gemini-2.5-flash-thinking",
    "gemini-3-flash",
    "gemini-3.1-flash-lite",
    "gemini-3.5-flash-lite",
    "gemini-3.8-flash-high",
    "gemini-3.8-flash-low",
    "gemini-3.8-flash-medium",
    "gemini-3.8-flash-tiered",
    "gemini-4-argon-high",
    "gemini-4-argon-low",
    "gemini-4-argon-medium",
    "gemini-4-carbon-high",
    "gemini-4-carbon-low",
    "gemini-4-carbon-medium",
    "gemini-4-hydrogen-high",
    "gemini-4-hydrogen-low",
    "gemini-4-hydrogen-medium",
    "gemini-4-hydrogen-sonar",
    "gemini-4-hydrogen-tiered-high",
    "gemini-4-hydrogen-tiered-xhigh",
    "gemini-4-hydrogen-xhigh",
    "gemini-4-neon-high",
    "gemini-4-neon-low",
    "gemini-4-neon-medium",
    "gemini-4-neon-xhigh",
];

pub async fn list_models_for_accounts(
    account_manager: &Arc<AccountManager>,
    force_refresh: bool,
) -> Value {
    let all_accounts = account_manager.get_all_accounts().await;
    let has_enabled = all_accounts.iter().any(|a| !a.is_invalid && a.enabled);

    if has_enabled {
        if !force_refresh {
            let (has_cached, is_stale) = {
                let guard = model_cache().read().await;
                (
                    !guard.model_entries.is_empty(),
                    now_ms().saturating_sub(guard.last_fetched) >= MODEL_VALIDATION_CACHE_TTL_MS,
                )
            };
            if has_cached {
                if is_stale {
                    let am = Arc::clone(account_manager);
                    tokio::spawn(async move {
                        populate_model_cache_for_accounts(&am, false).await;
                    });
                }
            } else {
                populate_model_cache_for_accounts(account_manager, false).await;
            }
        } else {
            populate_model_cache_for_accounts(account_manager, true).await;
        }
    }

    let created = (now_ms() / 1000) as u64;
    let guard = model_cache().read().await;

    let mut items: Vec<(String, Value)> = guard
        .model_entries
        .iter()
        .filter(|(id, _)| !is_hidden_model(id))
        .map(|(id, data)| (id.clone(), data.clone()))
        .collect();

    if items.is_empty() {
        let mut from_saved = HashSet::new();
        for acc in &all_accounts {
            for mid in acc.quota.models.keys() {
                if is_supported_model(mid) && !is_hidden_model(mid) {
                    from_saved.insert(mid.clone());
                }
            }
        }
        if !from_saved.is_empty() {
            for mid in from_saved {
                items.push((mid.clone(), json!({ "displayName": mid })));
            }
        } else {
            for &mid in BUILTIN_FALLBACK_MODELS {
                items.push((mid.to_string(), json!({ "displayName": mid })));
            }
        }
    }

    items.sort_by(|a, b| a.0.cmp(&b.0));

    let list: Vec<Value> = items
        .into_iter()
        .map(|(id, data)| {
            let desc = data
                .get("displayName")
                .and_then(|v| v.as_str())
                .unwrap_or(&id)
                .to_string();
            json!({
                "id": id,
                "object": "model",
                "created": created,
                "owned_by": "anthropic",
                "description": desc
            })
        })
        .collect();

    json!({
        "object": "list",
        "data": list
    })
}

pub async fn get_model_quotas(
    token: &str,
    project_id: Option<&str>,
) -> Result<HashMap<String, ModelQuota>, String> {
    let data = fetch_available_models(token, project_id).await?;
    let Some(models_obj) = data.get("models").and_then(|v| v.as_object()) else {
        return Ok(HashMap::new());
    };
    record_models(&data);

    let mut quotas = HashMap::new();
    for (id, model_data) in models_obj {
        if !is_supported_model(id) {
            continue;
        }
        if let Some(qi) = model_data.get("quotaInfo") {
            let reset_time = qi
                .get("resetTime")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let remaining_fraction = qi
                .get("remainingFraction")
                .and_then(|v| v.as_f64())
                .or_else(|| reset_time.as_ref().map(|_| 0.0));
            quotas.insert(
                id.clone(),
                ModelQuota {
                    remaining_fraction,
                    reset_time,
                },
            );
        }
    }
    Ok(quotas)
}

pub fn parse_tier_id(tier_id: &str) -> String {
    if tier_id.is_empty() {
        return "unknown".to_string();
    }
    let lower = tier_id.to_lowercase();
    if lower.contains("ultra") {
        "ultra".to_string()
    } else if lower == "standard-tier" || lower.contains("pro") || lower.contains("premium") {
        "pro".to_string()
    } else if lower == "free-tier" || lower.contains("free") {
        "free".to_string()
    } else {
        "unknown".to_string()
    }
}

pub async fn get_subscription_tier(token: &str) -> Result<(String, Option<String>), String> {
    for endpoint in LOAD_CODE_ASSIST_ENDPOINTS {
        maybe_throttle().await;
        let url = format!("{endpoint}/v1internal:loadCodeAssist");
        let resp = http_client()
            .post(&url)
            .bearer_auth(token)
            .headers(antigravity_headers())
            .json(&json!({ "metadata": client_metadata(), "mode": 1 }))
            .send()
            .await;

        match resp {
            Ok(r) => {
                let status = r.status();
                if !status.is_success() {
                    let err_text = r.text().await.unwrap_or_default();
                    logger().warn(format!(
                        "[CloudCode] loadCodeAssist error at {endpoint}: {status}"
                    ));
                    if status.as_u16() == 403 {
                        let lower = err_text.to_lowercase();
                        if lower.contains("has been disabled")
                            && lower.contains("violation of terms of service")
                        {
                            return Err(format!("ACCOUNT_BANNED: {err_text}"));
                        }
                    }
                    continue;
                }
                if let Ok(data) = r.json::<Value>().await {
                    let project_id = data.get("cloudaicompanionProject").and_then(|v| {
                        v.as_str()
                            .map(|s| s.to_string())
                            .or_else(|| v.get("id").and_then(|i| i.as_str()).map(|s| s.to_string()))
                    });

                    let mut tier = "unknown".to_string();
                    if let Some(id) = data
                        .get("paidTier")
                        .and_then(|v| v.get("id"))
                        .and_then(|v| v.as_str())
                    {
                        tier = parse_tier_id(id);
                    }
                    if tier == "unknown" {
                        if let Some(id) = data
                            .get("currentTier")
                            .and_then(|v| v.get("id"))
                            .and_then(|v| v.as_str())
                        {
                            tier = parse_tier_id(id);
                        }
                    }
                    if tier == "unknown" {
                        if let Some(allowed) = data.get("allowedTiers").and_then(|v| v.as_array()) {
                            let target = allowed
                                .iter()
                                .find(|t| {
                                    t.get("isDefault").and_then(|v| v.as_bool()) == Some(true)
                                })
                                .or_else(|| allowed.first());
                            if let Some(id) =
                                target.and_then(|t| t.get("id")).and_then(|v| v.as_str())
                            {
                                tier = parse_tier_id(id);
                            }
                        }
                    }
                    return Ok((tier, project_id));
                }
            }
            Err(err) => {
                logger().warn(format!(
                    "[CloudCode] loadCodeAssist failed at {endpoint}: {err}"
                ));
            }
        }
    }

    Ok(("free".to_string(), None))
}

pub async fn is_valid_model(model_id: &str, account_manager: &Arc<AccountManager>) -> bool {
    {
        let guard = model_cache().read().await;
        if !guard.valid_models.is_empty() {
            let is_stale =
                now_ms().saturating_sub(guard.last_fetched) >= MODEL_VALIDATION_CACHE_TTL_MS;
            let valid = guard.valid_models.contains(model_id);
            drop(guard);
            if is_stale {
                let am = Arc::clone(account_manager);
                tokio::spawn(async move {
                    populate_model_cache_for_accounts(&am, false).await;
                });
            }
            return valid;
        }
    }

    populate_model_cache_for_accounts(account_manager, false).await;
    let guard = model_cache().read().await;
    if guard.valid_models.is_empty() {
        return true; // Fail open when cache unavailable
    }
    guard.valid_models.contains(model_id)
}
