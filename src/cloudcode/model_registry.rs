use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::{OnceLock, RwLock};

#[derive(Debug, Clone)]
pub struct RegisteredModel {
    pub family: Option<String>,
    pub supports_thinking: bool,
    pub max_output_tokens: Option<u64>,
    pub display_name: Option<String>,
}

static REGISTRY: OnceLock<RwLock<HashMap<String, RegisteredModel>>> = OnceLock::new();

fn registry() -> &'static RwLock<HashMap<String, RegisteredModel>> {
    REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

fn family_from_provider(api_provider: Option<&str>) -> Option<String> {
    let provider = api_provider.unwrap_or("").to_uppercase();
    if provider.contains("GOOGLE_GEMINI") {
        Some("gemini".to_string())
    } else if provider.contains("ANTHROPIC") {
        Some("claude".to_string())
    } else {
        None
    }
}

fn collect_agent_model_ids(data: &Value) -> HashSet<String> {
    let mut ids = HashSet::new();
    if let Some(sorts) = data.get("agentModelSorts").and_then(|v| v.as_array()) {
        for sort in sorts {
            if let Some(groups) = sort.get("groups").and_then(|v| v.as_array()) {
                for group in groups {
                    if let Some(model_ids) = group.get("modelIds").and_then(|v| v.as_array()) {
                        for id in model_ids {
                            if let Some(s) = id.as_str() {
                                ids.insert(s.to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    if let Some(tiered) = data.get("tieredModelIds").and_then(|v| v.as_object()) {
        for list in tiered.values() {
            if let Some(arr) = list.as_array() {
                for id in arr {
                    if let Some(s) = id.as_str() {
                        ids.insert(s.to_string());
                    }
                }
            }
        }
    }
    ids
}

fn has_family_in_name(model_id: &str) -> bool {
    let lower = model_id.to_lowercase();
    lower.contains("claude") || lower.contains("gemini")
}

pub fn record_models(data: &Value) {
    let Some(models) = data.get("models").and_then(|v| v.as_object()) else {
        return;
    };
    let agent_ids = collect_agent_model_ids(data);

    let Ok(mut guard) = registry().write() else {
        return;
    };

    for (model_id, model_data) in models {
        let api_provider = model_data.get("apiProvider").and_then(|v| v.as_str());
        let family = family_from_provider(api_provider);
        if !has_family_in_name(model_id) && !(family.is_some() && agent_ids.contains(model_id)) {
            continue;
        }

        let supports_thinking = model_data
            .get("supportsThinking")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let max_output_tokens = model_data
            .get("maxOutputTokens")
            .and_then(|v| v.as_u64());
        let display_name = model_data
            .get("displayName")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        guard.insert(
            model_id.clone(),
            RegisteredModel {
                family,
                supports_thinking,
                max_output_tokens,
                display_name,
            },
        );
    }
}

pub fn get_registered_model(model_id: &str) -> Option<RegisteredModel> {
    registry().read().ok()?.get(model_id).cloned()
}

pub fn clear_model_registry() {
    if let Ok(mut guard) = registry().write() {
        guard.clear();
    }
}
