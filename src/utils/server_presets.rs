use crate::constants::{default_server_presets, server_presets_path};
use crate::logger::logger;
use serde_json::{json, Value};
use std::collections::HashSet;
use tokio::fs;

pub async fn read_server_presets() -> Result<Vec<Value>, String> {
    let path = server_presets_path();
    let defaults = default_server_presets();
    let builtin_names: HashSet<String> = defaults
        .iter()
        .filter_map(|p| p.get("name").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .collect();

    match fs::read_to_string(&path).await {
        Ok(content) => {
            if content.trim().is_empty() {
                return Ok(defaults);
            }
            match serde_json::from_str::<Vec<Value>>(&content) {
                Ok(user_presets) => {
                    let mut merged = defaults;
                    for p in user_presets {
                        let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        let is_builtin =
                            p.get("builtIn").and_then(|v| v.as_bool()).unwrap_or(false);
                        if !builtin_names.contains(name) && !is_builtin {
                            merged.push(p);
                        }
                    }
                    Ok(merged)
                }
                Err(_) => Ok(defaults),
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent).await;
            }
            if let Ok(txt) = serde_json::to_string_pretty(&defaults) {
                let _ = fs::write(&path, txt).await;
            }
            Ok(defaults)
        }
        Err(err) => Err(format!("Failed to read server presets: {err}")),
    }
}

pub async fn save_server_preset(
    name: &str,
    config: Value,
    description: Option<&str>,
) -> Result<Vec<Value>, String> {
    let defaults = default_server_presets();
    if defaults
        .iter()
        .any(|p| p.get("name").and_then(|v| v.as_str()) == Some(name))
    {
        return Err(format!("Cannot overwrite built-in preset \"{name}\""));
    }

    let path = server_presets_path();
    let mut all_presets = read_server_presets().await?;

    let mut new_preset = json!({ "name": name, "config": config });
    if let Some(desc) = description {
        let trimmed = desc.trim();
        if !trimmed.is_empty() {
            new_preset["description"] = Value::String(trimmed.to_string());
        }
    }

    if let Some(pos) = all_presets.iter().position(|p| {
        p.get("name").and_then(|v| v.as_str()) == Some(name)
            && !p.get("builtIn").and_then(|v| v.as_bool()).unwrap_or(false)
    }) {
        all_presets[pos] = new_preset;
        logger().info(format!("[ServerPresets] Updated preset: {name}"));
    } else {
        all_presets.push(new_preset);
        logger().info(format!("[ServerPresets] Created preset: {name}"));
    }

    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent).await;
    }
    let formatted =
        serde_json::to_string_pretty(&all_presets).map_err(|e| format!("Serialize error: {e}"))?;
    fs::write(&path, formatted)
        .await
        .map_err(|e| format!("Failed to save server preset: {e}"))?;
    Ok(all_presets)
}

pub async fn update_server_preset(
    current_name: &str,
    new_name: Option<&str>,
    description: Option<&str>,
    config_updates: Option<&Value>,
) -> Result<Vec<Value>, String> {
    let defaults = default_server_presets();
    let is_builtin = |n: &str| {
        defaults
            .iter()
            .any(|p| p.get("name").and_then(|v| v.as_str()) == Some(n))
    };

    if is_builtin(current_name) {
        return Err(format!("Cannot edit built-in preset \"{current_name}\""));
    }

    let mut all_presets = read_server_presets().await?;
    let idx = all_presets
        .iter()
        .position(|p| {
            p.get("name").and_then(|v| v.as_str()) == Some(current_name)
                && !p.get("builtIn").and_then(|v| v.as_bool()).unwrap_or(false)
        })
        .ok_or_else(|| format!("Preset \"{current_name}\" not found"))?;

    if let Some(nn) = new_name {
        if is_builtin(nn) {
            return Err(format!("Cannot use built-in preset name \"{nn}\""));
        }
        if nn != current_name
            && all_presets.iter().any(|p| {
                p.get("name").and_then(|v| v.as_str()) == Some(nn)
                    && !p.get("builtIn").and_then(|v| v.as_bool()).unwrap_or(false)
            })
        {
            return Err(format!("A preset named \"{nn}\" already exists"));
        }
        all_presets[idx]["name"] = Value::String(nn.trim().to_string());
    }

    if let Some(desc) = description {
        let trimmed = desc.trim();
        if trimmed.is_empty() {
            if let Some(obj) = all_presets[idx].as_object_mut() {
                obj.remove("description");
            }
        } else {
            all_presets[idx]["description"] = Value::String(trimmed.to_string());
        }
    }

    if let Some(upd_cfg) = config_updates.and_then(|v| v.as_object()) {
        let allowed_keys = [
            "maxRetries",
            "retryBaseMs",
            "retryMaxMs",
            "defaultCooldownMs",
            "maxWaitBeforeErrorMs",
            "maxAccounts",
            "globalQuotaThreshold",
            "rateLimitDedupWindowMs",
            "maxConsecutiveFailures",
            "extendedCooldownMs",
            "maxCapacityRetries",
            "switchAccountDelayMs",
            "capacityBackoffTiersMs",
            "accountSelection",
        ];

        let mut existing = all_presets[idx]
            .get("config")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();

        for key in allowed_keys {
            if let Some(val) = upd_cfg.get(key) {
                if key == "accountSelection" {
                    if let Some(update_as) = val.as_object() {
                        let mut existing_as = existing
                            .get("accountSelection")
                            .and_then(|v| v.as_object())
                            .cloned()
                            .unwrap_or_default();
                        if let Some(strat) = update_as.get("strategy") {
                            existing_as.insert("strategy".to_string(), strat.clone());
                        }
                        for sub_key in ["healthScore", "tokenBucket", "quota", "weights"] {
                            if let Some(sub_upd) =
                                update_as.get(sub_key).and_then(|v| v.as_object())
                            {
                                let mut sub_cur = existing_as
                                    .get(sub_key)
                                    .and_then(|v| v.as_object())
                                    .cloned()
                                    .unwrap_or_default();
                                for (sk, sv) in sub_upd {
                                    sub_cur.insert(sk.clone(), sv.clone());
                                }
                                existing_as.insert(sub_key.to_string(), Value::Object(sub_cur));
                            }
                        }
                        existing.insert(
                            "accountSelection".to_string(),
                            Value::Object(existing_as),
                        );
                    }
                } else {
                    existing.insert(key.to_string(), val.clone());
                }
            }
        }
        all_presets[idx]["config"] = Value::Object(existing);
    }

    let path = server_presets_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent).await;
    }
    let formatted =
        serde_json::to_string_pretty(&all_presets).map_err(|e| format!("Serialize error: {e}"))?;
    fs::write(&path, formatted)
        .await
        .map_err(|e| format!("Failed to update server preset: {e}"))?;
    Ok(all_presets)
}

pub async fn delete_server_preset(name: &str) -> Result<Vec<Value>, String> {
    let defaults = default_server_presets();
    if defaults
        .iter()
        .any(|p| p.get("name").and_then(|v| v.as_str()) == Some(name))
    {
        return Err(format!("Cannot delete built-in preset \"{name}\""));
    }

    let path = server_presets_path();
    let mut all_presets = read_server_presets().await?;
    let orig_len = all_presets.len();
    all_presets.retain(|p| p.get("name").and_then(|v| v.as_str()) != Some(name));
    if all_presets.len() != orig_len {
        let formatted = serde_json::to_string_pretty(&all_presets)
            .map_err(|e| format!("Serialize error: {e}"))?;
        fs::write(&path, formatted)
            .await
            .map_err(|e| format!("Failed to delete server preset: {e}"))?;
        logger().info(format!("[ServerPresets] Deleted preset: {name}"));
    }
    Ok(all_presets)
}
