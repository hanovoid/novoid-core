use crate::config::deep_merge;
use crate::constants::{claude_presets_path, default_claude_presets};
use crate::logger::logger;
use serde_json::{json, Value};
use std::path::PathBuf;
use tokio::fs;

pub fn get_claude_config_path() -> PathBuf {
    if let Ok(custom) = std::env::var("CLAUDE_CONFIG_PATH") {
        if !custom.trim().is_empty() {
            return PathBuf::from(custom).join("settings.json");
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("settings.json")
}

pub async fn read_claude_config() -> Result<Value, String> {
    let path = get_claude_config_path();
    match fs::read_to_string(&path).await {
        Ok(content) => {
            if content.trim().is_empty() {
                return Ok(json!({ "env": {} }));
            }
            match serde_json::from_str::<Value>(&content) {
                Ok(val) => Ok(val),
                Err(_) => {
                    logger().error(format!(
                        "[ClaudeConfig] Invalid JSON in config at {}. Returning safe default.",
                        path.display()
                    ));
                    Ok(json!({ "env": {} }))
                }
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            logger().warn(format!(
                "[ClaudeConfig] Config file not found at {}, returning empty default",
                path.display()
            ));
            Ok(json!({ "env": {} }))
        }
        Err(err) => Err(format!("Failed to read Claude config: {err}")),
    }
}

pub async fn update_claude_config(updates: &Value) -> Result<Value, String> {
    let mut current = read_claude_config().await?;
    deep_merge(&mut current, updates);
    replace_claude_config(&current).await
}

pub async fn replace_claude_config(config: &Value) -> Result<Value, String> {
    let path = get_claude_config_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent).await;
    }
    let formatted = serde_json::to_string_pretty(config)
        .map_err(|e| format!("Failed to serialize Claude config: {e}"))?;
    fs::write(&path, formatted)
        .await
        .map_err(|e| format!("Failed to write Claude config: {e}"))?;
    logger().info(format!("[ClaudeConfig] Saved config at {}", path.display()));
    Ok(config.clone())
}

pub async fn read_presets() -> Result<Vec<Value>, String> {
    let path = claude_presets_path();
    match fs::read_to_string(&path).await {
        Ok(content) => {
            if content.trim().is_empty() {
                return Ok(default_claude_presets()
                    .as_array()
                    .cloned()
                    .unwrap_or_default());
            }
            match serde_json::from_str::<Vec<Value>>(&content) {
                Ok(list) => Ok(list),
                Err(_) => Ok(default_claude_presets()
                    .as_array()
                    .cloned()
                    .unwrap_or_default()),
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let defaults = default_claude_presets();
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent).await;
            }
            if let Ok(txt) = serde_json::to_string_pretty(&defaults) {
                let _ = fs::write(&path, txt).await;
            }
            Ok(defaults.as_array().cloned().unwrap_or_default())
        }
        Err(err) => Err(format!("Failed to read presets: {err}")),
    }
}

pub async fn save_preset(name: &str, cfg: &Value) -> Result<Vec<Value>, String> {
    let path = claude_presets_path();
    let mut presets = read_presets().await?;
    let new_preset = json!({ "name": name, "config": cfg });

    if let Some(pos) = presets
        .iter()
        .position(|p| p.get("name").and_then(|v| v.as_str()) == Some(name))
    {
        presets[pos] = new_preset;
        logger().info(format!("[ClaudePresets] Updated preset: {name}"));
    } else {
        presets.push(new_preset);
        logger().info(format!("[ClaudePresets] Created preset: {name}"));
    }

    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent).await;
    }
    let formatted =
        serde_json::to_string_pretty(&presets).map_err(|e| format!("Serialize error: {e}"))?;
    fs::write(&path, formatted)
        .await
        .map_err(|e| format!("Failed to save preset: {e}"))?;
    Ok(presets)
}

pub async fn delete_preset(name: &str) -> Result<Vec<Value>, String> {
    let path = claude_presets_path();
    let mut presets = read_presets().await?;
    let orig_len = presets.len();
    presets.retain(|p| p.get("name").and_then(|v| v.as_str()) != Some(name));
    if presets.len() != orig_len {
        let formatted =
            serde_json::to_string_pretty(&presets).map_err(|e| format!("Serialize error: {e}"))?;
        fs::write(&path, formatted)
            .await
            .map_err(|e| format!("Failed to delete preset: {e}"))?;
        logger().info(format!("[ClaudePresets] Deleted preset: {name}"));
    }
    Ok(presets)
}
