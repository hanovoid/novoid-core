use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};
use uuid::Uuid;

static RUNTIME_SESSION_STORE: OnceLock<RwLock<HashMap<String, String>>> = OnceLock::new();

fn session_store() -> &'static RwLock<HashMap<String, String>> {
    RUNTIME_SESSION_STORE.get_or_init(|| RwLock::new(HashMap::new()))
}

fn extract_first_user_text(anthropic_request: &Value) -> Option<String> {
    let messages = anthropic_request.get("messages")?.as_array()?;
    for msg in messages {
        if msg.get("role").and_then(|v| v.as_str()) != Some("user") {
            continue;
        }
        let Some(content) = msg.get("content") else {
            continue;
        };
        let text = match content {
            Value::String(s) => Some(s.clone()),
            Value::Array(blocks) => {
                let parts: Vec<String> = blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(|v| v.as_str()) == Some("text"))
                    .filter_map(|b| b.get("text").and_then(|v| v.as_str()).map(|s| s.to_string()))
                    .collect();
                if parts.is_empty() {
                    None
                } else {
                    Some(parts.join("\n"))
                }
            }
            _ => None,
        };
        if let Some(t) = text {
            if !t.trim().is_empty() {
                return Some(t);
            }
        }
    }
    None
}

fn derive_conversation_session_id(anthropic_request: &Value, account_email: &str) -> Option<String> {
    let first_user_text = extract_first_user_text(anthropic_request)?;
    let email_part = if account_email.is_empty() {
        "anonymous"
    } else {
        account_email
    };
    let mut hasher = Sha256::new();
    hasher.update(format!("{email_part}\u{0000}{first_user_text}").as_bytes());
    let digest = hex::encode(hasher.finalize());

    let uuid = format!(
        "{}-{}-{}-{}-{}",
        &digest[0..8],
        &digest[8..12],
        &digest[12..16],
        &digest[16..20],
        &digest[20..32]
    );
    let tail_val = u128::from_str_radix(&digest[32..45], 16).unwrap_or(0);
    Some(format!("{uuid}{tail_val}"))
}

fn generate_binary_style_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{}{}", Uuid::new_v4(), now)
}

pub fn derive_session_id(anthropic_request: &Value, account_email: &str) -> String {
    if let Some(conv_id) = derive_conversation_session_id(anthropic_request, account_email) {
        return conv_id;
    }
    if account_email.is_empty() {
        return generate_binary_style_id();
    }
    if let Ok(guard) = session_store().read() {
        if let Some(existing) = guard.get(account_email) {
            return existing.clone();
        }
    }
    let new_id = generate_binary_style_id();
    if let Ok(mut guard) = session_store().write() {
        guard.insert(account_email.to_string(), new_id.clone());
    }
    new_id
}

pub fn clear_session_store() {
    if let Ok(mut guard) = session_store().write() {
        guard.clear();
    }
}
