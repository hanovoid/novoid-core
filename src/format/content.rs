use crate::constants::{GEMINI_SKIP_SIGNATURE, MIN_SIGNATURE_LENGTH};
use crate::format::signature_cache::{get_cached_signature, get_cached_signature_family};
use crate::logger::logger;
use serde_json::{json, Map, Value};

pub fn convert_role(role: &str) -> &'static str {
    if role == "assistant" || role == "model" {
        "model"
    } else {
        "user"
    }
}

pub fn convert_content_to_parts(
    content: &Value,
    is_claude_model: bool,
    is_gemini_model: bool,
) -> Vec<Value> {
    if let Some(s) = content.as_str() {
        return vec![json!({ "text": s })];
    }

    let Some(blocks) = content.as_array() else {
        return vec![json!({ "text": content.to_string() })];
    };

    let mut parts = Vec::new();
    let mut deferred_inline_data = Vec::new();

    for block in blocks {
        let Some(block_obj) = block.as_object() else {
            continue;
        };
        let b_type = block_obj
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        match b_type {
            "text" => {
                if let Some(text) = block_obj.get("text").and_then(|v| v.as_str()) {
                    if !text.trim().is_empty() {
                        parts.push(json!({ "text": text }));
                    }
                }
            }
            "image" => {
                if let Some(source) = block_obj.get("source").and_then(|v| v.as_object()) {
                    match source.get("type").and_then(|v| v.as_str()) {
                        Some("base64") => {
                            parts.push(json!({
                                "inlineData": {
                                    "mimeType": source.get("media_type").cloned().unwrap_or(Value::Null),
                                    "data": source.get("data").cloned().unwrap_or(Value::Null)
                                }
                            }));
                        }
                        Some("url") => {
                            parts.push(json!({
                                "fileData": {
                                    "mimeType": source.get("media_type").and_then(|v| v.as_str()).unwrap_or("image/jpeg"),
                                    "fileUri": source.get("url").cloned().unwrap_or(Value::Null)
                                }
                            }));
                        }
                        _ => {}
                    }
                }
            }
            "document" => {
                if let Some(source) = block_obj.get("source").and_then(|v| v.as_object()) {
                    match source.get("type").and_then(|v| v.as_str()) {
                        Some("base64") => {
                            parts.push(json!({
                                "inlineData": {
                                    "mimeType": source.get("media_type").cloned().unwrap_or(Value::Null),
                                    "data": source.get("data").cloned().unwrap_or(Value::Null)
                                }
                            }));
                        }
                        Some("url") => {
                            parts.push(json!({
                                "fileData": {
                                    "mimeType": source.get("media_type").and_then(|v| v.as_str()).unwrap_or("application/pdf"),
                                    "fileUri": source.get("url").cloned().unwrap_or(Value::Null)
                                }
                            }));
                        }
                        _ => {}
                    }
                }
            }
            "tool_use" => {
                let name = block_obj
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let args = block_obj
                    .get("input")
                    .cloned()
                    .unwrap_or_else(|| json!({}));

                let mut func_call = Map::new();
                func_call.insert("name".to_string(), Value::String(name.to_string()));
                func_call.insert("args".to_string(), args);

                if is_claude_model {
                    if let Some(id) = block_obj.get("id").and_then(|v| v.as_str()) {
                        func_call.insert("id".to_string(), Value::String(id.to_string()));
                    }
                }

                let mut part = Map::new();
                part.insert("functionCall".to_string(), Value::Object(func_call));

                if is_gemini_model {
                    let mut sig = block_obj
                        .get("thoughtSignature")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    if sig.is_none() {
                        if let Some(id) = block_obj.get("id").and_then(|v| v.as_str()) {
                            if let Some(cached) = get_cached_signature(id) {
                                logger().debug(format!(
                                    "[ContentConverter] Restored signature from cache for: {id}"
                                ));
                                sig = Some(cached);
                            }
                        }
                    }
                    part.insert(
                        "thoughtSignature".to_string(),
                        Value::String(sig.unwrap_or_else(|| GEMINI_SKIP_SIGNATURE.to_string())),
                    );
                }

                parts.push(Value::Object(part));
            }
            "tool_result" => {
                let mut image_parts = Vec::new();
                let response_content = match block_obj.get("content") {
                    Some(Value::String(s)) => json!({ "result": s }),
                    Some(Value::Array(items)) => {
                        let mut texts = Vec::new();
                        for item in items {
                            if item.get("type").and_then(|v| v.as_str()) == Some("image") {
                                if let Some(src) = item.get("source").and_then(|v| v.as_object()) {
                                    if src.get("type").and_then(|v| v.as_str()) == Some("base64") {
                                        image_parts.push(json!({
                                            "inlineData": {
                                                "mimeType": src.get("media_type").cloned().unwrap_or(Value::Null),
                                                "data": src.get("data").cloned().unwrap_or(Value::Null)
                                            }
                                        }));
                                    }
                                }
                            } else if item.get("type").and_then(|v| v.as_str()) == Some("text") {
                                if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                                    texts.push(t.to_string());
                                }
                            }
                        }
                        let joined = texts.join("\n");
                        let res = if !joined.is_empty() {
                            joined
                        } else if !image_parts.is_empty() {
                            "Image attached".to_string()
                        } else {
                            String::new()
                        };
                        json!({ "result": res })
                    }
                    Some(other) => other.clone(),
                    None => Value::Null,
                };

                let tool_use_id = block_obj
                    .get("tool_use_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");

                let mut func_resp = Map::new();
                func_resp.insert("name".to_string(), Value::String(tool_use_id.to_string()));
                func_resp.insert("response".to_string(), response_content);

                if is_claude_model {
                    if let Some(id) = block_obj.get("tool_use_id").and_then(|v| v.as_str()) {
                        func_resp.insert("id".to_string(), Value::String(id.to_string()));
                    }
                }

                parts.push(json!({ "functionResponse": Value::Object(func_resp) }));
                deferred_inline_data.extend(image_parts);
            }
            "thinking" => {
                let sig = block_obj
                    .get("signature")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if sig.len() >= MIN_SIGNATURE_LENGTH {
                    let sig_family = get_cached_signature_family(sig);
                    let target_family = if is_claude_model {
                        Some("claude")
                    } else if is_gemini_model {
                        Some("gemini")
                    } else {
                        None
                    };

                    if is_claude_model && sig_family.as_deref() != Some("claude") {
                        logger().debug(
                            "[ContentConverter] Dropping untrusted thinking signature for Claude model",
                        );
                        continue;
                    }

                    if is_gemini_model {
                        if let (Some(sf), Some(tf)) = (sig_family.as_deref(), target_family) {
                            if sf != tf {
                                logger().debug(format!(
                                    "[ContentConverter] Dropping incompatible {sf} thinking for {tf} model"
                                ));
                                continue;
                            }
                        }
                        if sig_family.is_none() && target_family.is_some() {
                            logger().debug(
                                "[ContentConverter] Dropping thinking with unknown signature origin",
                            );
                            continue;
                        }
                    }

                    parts.push(json!({
                        "text": block_obj.get("thinking").cloned().unwrap_or(Value::String(String::new())),
                        "thought": true,
                        "thoughtSignature": sig
                    }));
                }
            }
            _ => {}
        }
    }

    parts.extend(deferred_inline_data);
    parts
}
