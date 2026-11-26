use crate::constants::{get_model_family, MIN_SIGNATURE_LENGTH};
use crate::format::signature_cache::{cache_signature, cache_thinking_signature};
use serde_json::{json, Map, Value};
use uuid::Uuid;

pub fn convert_google_to_anthropic(google_response: &Value, model: &str) -> Value {
    let response = google_response.get("response").unwrap_or(google_response);
    let candidates = response
        .get("candidates")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let first_candidate = candidates.first().cloned().unwrap_or_else(|| json!({}));
    let parts = first_candidate
        .get("content")
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();

    let mut anthropic_content = Vec::new();
    let mut has_tool_calls = false;

    for part in parts {
        if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
            if part.get("thought").and_then(|v| v.as_bool()) == Some(true) {
                let sig = part
                    .get("thoughtSignature")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if sig.len() >= MIN_SIGNATURE_LENGTH {
                    let family = get_model_family(model);
                    cache_thinking_signature(sig, &family);
                }
                anthropic_content.push(json!({
                    "type": "thinking",
                    "thinking": text,
                    "signature": sig
                }));
            } else {
                anthropic_content.push(json!({
                    "type": "text",
                    "text": text
                }));
            }
        } else if let Some(fc) = part.get("functionCall") {
            let tool_id = fc
                .get("id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("toolu_{}", Uuid::new_v4().simple()));

            let mut block = Map::new();
            block.insert("type".to_string(), Value::String("tool_use".to_string()));
            block.insert("id".to_string(), Value::String(tool_id.clone()));
            block.insert(
                "name".to_string(),
                fc.get("name").cloned().unwrap_or(Value::String(String::new())),
            );
            block.insert(
                "input".to_string(),
                fc.get("args").cloned().unwrap_or_else(|| json!({})),
            );

            if let Some(sig) = part.get("thoughtSignature").and_then(|v| v.as_str()) {
                if sig.len() >= MIN_SIGNATURE_LENGTH {
                    block.insert(
                        "thoughtSignature".to_string(),
                        Value::String(sig.to_string()),
                    );
                    cache_signature(&tool_id, sig);
                }
            }

            anthropic_content.push(Value::Object(block));
            has_tool_calls = true;
        } else if let Some(inline) = part.get("inlineData") {
            anthropic_content.push(json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": inline.get("mimeType").cloned().unwrap_or(Value::Null),
                    "data": inline.get("data").cloned().unwrap_or(Value::Null)
                }
            }));
        }
    }

    let finish_reason = first_candidate
        .get("finishReason")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let stop_reason = if finish_reason == "MAX_TOKENS" {
        "max_tokens"
    } else if finish_reason == "TOOL_USE" || has_tool_calls {
        "tool_use"
    } else {
        "end_turn"
    };

    let usage_meta = response
        .get("usageMetadata")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let prompt_tokens = usage_meta
        .get("promptTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cached_tokens = usage_meta
        .get("cachedContentTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let candidates_tokens = usage_meta
        .get("candidatesTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let thoughts_tokens = usage_meta
        .get("thoughtsTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    if anthropic_content.is_empty() {
        anthropic_content.push(json!({ "type": "text", "text": "" }));
    }

    json!({
        "id": format!("msg_{}", Uuid::new_v4().simple()),
        "type": "message",
        "role": "assistant",
        "content": anthropic_content,
        "model": model,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": prompt_tokens.saturating_sub(cached_tokens),
            "output_tokens": candidates_tokens + thoughts_tokens,
            "cache_read_input_tokens": cached_tokens,
            "cache_creation_input_tokens": 0
        }
    })
}
