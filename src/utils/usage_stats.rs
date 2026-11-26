use crate::constants::usage_history_path;
use crate::logger::logger;
use chrono::{DateTime, Duration, Timelike, Utc};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fs;
use std::sync::{OnceLock, RwLock};

#[derive(Default)]
struct UsageState {
    history: BTreeMap<String, Value>,
    dirty: bool,
}

static USAGE_STATE: OnceLock<RwLock<UsageState>> = OnceLock::new();

fn state_lock() -> &'static RwLock<UsageState> {
    USAGE_STATE.get_or_init(|| {
        let mut st = UsageState::default();
        load_into(&mut st);
        RwLock::new(st)
    })
}

fn get_family(model_id: &str) -> &'static str {
    let lower = model_id.to_lowercase();
    if lower.contains("claude") {
        "claude"
    } else if lower.contains("gemini") {
        "gemini"
    } else {
        "other"
    }
}

fn get_short_name(model_id: &str, family: &str) -> String {
    if family == "other" {
        return model_id.to_string();
    }
    let prefix = format!("{family}-");
    if model_id.to_lowercase().starts_with(&prefix) {
        model_id[prefix.len()..].to_string()
    } else {
        model_id.to_string()
    }
}

fn normalize_history(history: &mut BTreeMap<String, Value>) -> bool {
    let migrations = [
        ("opus-5-5", "opus-4-6-thinking"),
        ("sonnet-5-5", "sonnet-4-6"),
        ("haiku-5-5", "sonnet-4-6"),
    ];
    let mut modified = false;
    for hour_val in history.values_mut() {
        let Some(hour_obj) = hour_val.as_object_mut() else {
            continue;
        };
        for (family, fam_val) in hour_obj.iter_mut() {
            if family.starts_with('_') {
                continue;
            }
            let Some(fam_obj) = fam_val.as_object_mut() else {
                continue;
            };
            for (legacy, target) in migrations {
                if let Some(count) = fam_obj.remove(legacy).and_then(|v| v.as_u64()) {
                    let prev = fam_obj
                        .get(target)
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    fam_obj.insert(target.to_string(), Value::from(prev + count));
                    modified = true;
                }
            }
        }
    }
    modified
}

fn load_into(state: &mut UsageState) {
    let path = usage_history_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if path.exists() {
        match fs::read_to_string(&path) {
            Ok(raw) => {
                if let Ok(parsed) = serde_json::from_str::<BTreeMap<String, Value>>(&raw) {
                    state.history = parsed;
                    if normalize_history(&mut state.history) {
                        state.dirty = true;
                        save_inner(state);
                    }
                }
            }
            Err(err) => {
                logger().error(format!("[UsageStats] Failed to load history: {err}"));
            }
        }
    }
}

fn save_inner(state: &mut UsageState) {
    if !state.dirty {
        return;
    }
    let path = usage_history_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json_str) = serde_json::to_string_pretty(&state.history) {
        let tmp = path.with_extension("json.tmp");
        if fs::write(&tmp, &json_str).is_ok() && fs::rename(&tmp, &path).is_ok() {
            state.dirty = false;
        }
    }
}

fn prune_inner(state: &mut UsageState) {
    let cutoff = Utc::now() - Duration::days(30);
    let keys_to_remove: Vec<String> = state
        .history
        .keys()
        .filter(|k| {
            DateTime::parse_from_rfc3339(k)
                .map(|dt| dt.with_timezone(&Utc) < cutoff)
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    if !keys_to_remove.is_empty() {
        for k in keys_to_remove {
            state.history.remove(&k);
        }
        state.dirty = true;
    }
}

pub fn init_background_flush() {
    let _ = state_lock();
    tokio::spawn(async {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            flush();
        }
    });
}

pub fn flush() {
    if let Ok(mut guard) = state_lock().write() {
        prune_inner(&mut guard);
        save_inner(&mut guard);
    }
}

pub fn track(model_id: &str) {
    let now = Utc::now();
    let rounded = now
        .with_minute(0)
        .and_then(|d| d.with_second(0))
        .and_then(|d| d.with_nanosecond(0))
        .unwrap_or(now);
    let key = rounded.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

    let family = get_family(model_id);
    let short_name = get_short_name(model_id, family);

    let Ok(mut guard) = state_lock().write() else {
        return;
    };

    let hour_val = guard
        .history
        .entry(key)
        .or_insert_with(|| serde_json::json!({ "_total": 0 }));

    let Some(hour_obj) = hour_val.as_object_mut() else {
        return;
    };

    let total = hour_obj.get("_total").and_then(|v| v.as_u64()).unwrap_or(0);
    hour_obj.insert("_total".to_string(), Value::from(total + 1));

    let fam_val = hour_obj
        .entry(family.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(fam_obj) = fam_val.as_object_mut() {
        let cur = fam_obj
            .get(&short_name)
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        fam_obj.insert(short_name, Value::from(cur + 1));

        let sub = fam_obj
            .get("_subtotal")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        fam_obj.insert("_subtotal".to_string(), Value::from(sub + 1));
    }

    guard.dirty = true;
}

pub fn get_history() -> Value {
    let Ok(guard) = state_lock().read() else {
        return Value::Object(Map::new());
    };
    serde_json::to_value(&guard.history).unwrap_or_else(|_| Value::Object(Map::new()))
}
