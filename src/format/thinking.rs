use crate::constants::MIN_SIGNATURE_LENGTH;
use crate::format::signature_cache::{get_cached_signature, get_cached_signature_family};
use crate::logger::logger;
use serde_json::{json, Map, Value};

const GEMINI_DEFAULT_THINKING_BUDGET: u64 = 16_000;
const GEMINI_DEFAULT_THINKING_BUDGET_LIMIT: u64 = 128_000;

pub fn clamp_gemini_thinking_budget(model_name: &str, budget: Option<u64>) -> u64 {
    let requested = budget.unwrap_or(GEMINI_DEFAULT_THINKING_BUDGET);
    let lower = model_name.to_lowercase();
    let max_budget = if lower.contains("gemini-2.5") {
        24_576
    } else {
        GEMINI_DEFAULT_THINKING_BUDGET_LIMIT
    };

    if requested > max_budget {
        logger().debug(format!(
            "[ThinkingUtils] Clamping Gemini thinking budget from {requested} to {max_budget} for {model_name}"
        ));
        max_budget
    } else {
        requested
    }
}

pub fn clean_cache_control(messages: &[Value]) -> Vec<Value> {
    let mut removed = 0usize;
    let cleaned = messages
        .iter()
        .map(|msg| {
            let Some(mut obj) = msg.as_object().cloned() else {
                return msg.clone();
            };
            let Some(Value::Array(blocks)) = obj.get("content").cloned() else {
                return msg.clone();
            };
            let cleaned_blocks: Vec<Value> = blocks
                .into_iter()
                .map(|b| {
                    if let Some(mut b_obj) = b.as_object().cloned() {
                        if b_obj.remove("cache_control").is_some() {
                            removed += 1;
                        }
                        Value::Object(b_obj)
                    } else {
                        b
                    }
                })
                .collect();
            obj.insert("content".to_string(), Value::Array(cleaned_blocks));
            Value::Object(obj)
        })
        .collect();

    if removed > 0 {
        logger().debug(format!(
            "[ThinkingUtils] Removed cache_control from {removed} block(s)"
        ));
    }
    cleaned
}

fn is_thinking_part(part: &Value) -> bool {
    let t = part.get("type").and_then(|v| v.as_str());
    t == Some("thinking")
        || t == Some("redacted_thinking")
        || part.get("thinking").is_some()
        || part.get("thought").and_then(|v| v.as_bool()) == Some(true)
}

fn has_valid_signature(part: &Value) -> bool {
    let sig = if part.get("thought").and_then(|v| v.as_bool()) == Some(true) {
        part.get("thoughtSignature").and_then(|v| v.as_str())
    } else {
        part.get("signature").and_then(|v| v.as_str())
    };
    sig.map(|s| s.len() >= MIN_SIGNATURE_LENGTH)
        .unwrap_or(false)
}

pub fn has_gemini_history(messages: &[Value]) -> bool {
    messages.iter().any(|msg| {
        msg.get("content")
            .and_then(|v| v.as_array())
            .map(|blocks| {
                blocks.iter().any(|b| {
                    b.get("type").and_then(|v| v.as_str()) == Some("tool_use")
                        && b.get("thoughtSignature").is_some()
                })
            })
            .unwrap_or(false)
    })
}

pub fn has_unsigned_thinking_blocks(messages: &[Value]) -> bool {
    messages.iter().any(|msg| {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if role != "assistant" && role != "model" {
            return false;
        }
        msg.get("content")
            .and_then(|v| v.as_array())
            .map(|blocks| blocks.iter().any(|b| is_thinking_part(b) && !has_valid_signature(b)))
            .unwrap_or(false)
    })
}

fn sanitize_thinking_part(part: &Value) -> Value {
    if part.get("thought").and_then(|v| v.as_bool()) == Some(true) {
        let mut s = Map::new();
        s.insert("thought".to_string(), Value::Bool(true));
        if let Some(t) = part.get("text") {
            s.insert("text".to_string(), t.clone());
        }
        if let Some(sig) = part.get("thoughtSignature") {
            s.insert("thoughtSignature".to_string(), sig.clone());
        }
        return Value::Object(s);
    }

    if part.get("type").and_then(|v| v.as_str()) == Some("thinking")
        || part.get("thinking").is_some()
    {
        let mut s = Map::new();
        s.insert("type".to_string(), Value::String("thinking".to_string()));
        if let Some(t) = part.get("thinking") {
            s.insert("thinking".to_string(), t.clone());
        }
        if let Some(sig) = part.get("signature") {
            s.insert("signature".to_string(), sig.clone());
        }
        return Value::Object(s);
    }

    part.clone()
}

fn sanitize_anthropic_thinking_block(block: &Value) -> Value {
    match block.get("type").and_then(|v| v.as_str()) {
        Some("thinking") => {
            let mut s = Map::new();
            s.insert("type".to_string(), Value::String("thinking".to_string()));
            if let Some(t) = block.get("thinking") {
                s.insert("thinking".to_string(), t.clone());
            }
            if let Some(sig) = block.get("signature") {
                s.insert("signature".to_string(), sig.clone());
            }
            Value::Object(s)
        }
        Some("redacted_thinking") => {
            let mut s = Map::new();
            s.insert(
                "type".to_string(),
                Value::String("redacted_thinking".to_string()),
            );
            if let Some(d) = block.get("data") {
                s.insert("data".to_string(), d.clone());
            }
            Value::Object(s)
        }
        _ => block.clone(),
    }
}

fn sanitize_text_block(block: &Value) -> Value {
    if block.get("type").and_then(|v| v.as_str()) != Some("text") {
        return block.clone();
    }
    let mut s = Map::new();
    s.insert("type".to_string(), Value::String("text".to_string()));
    if let Some(t) = block.get("text") {
        s.insert("text".to_string(), t.clone());
    }
    Value::Object(s)
}

fn sanitize_tool_use_block(block: &Value) -> Value {
    if block.get("type").and_then(|v| v.as_str()) != Some("tool_use") {
        return block.clone();
    }
    let mut s = Map::new();
    s.insert("type".to_string(), Value::String("tool_use".to_string()));
    for k in ["id", "name", "input", "thoughtSignature"] {
        if let Some(v) = block.get(k) {
            s.insert(k.to_string(), v.clone());
        }
    }
    Value::Object(s)
}

pub fn filter_unsigned_thinking_blocks(contents: Vec<Value>) -> Vec<Value> {
    contents
        .into_iter()
        .map(|content| {
            let Some(mut obj) = content.as_object().cloned() else {
                return content;
            };
            let Some(Value::Array(parts)) = obj.remove("parts") else {
                return Value::Object(obj);
            };
            let filtered: Vec<Value> = parts
                .into_iter()
                .filter_map(|item| {
                    if !item.is_object() || !is_thinking_part(&item) {
                        Some(item)
                    } else if has_valid_signature(&item) {
                        Some(sanitize_thinking_part(&item))
                    } else {
                        logger().debug("[ThinkingUtils] Dropping unsigned thinking block");
                        None
                    }
                })
                .collect();
            obj.insert("parts".to_string(), Value::Array(filtered));
            Value::Object(obj)
        })
        .collect()
}

pub fn remove_trailing_thinking_blocks(content: Vec<Value>) -> Vec<Value> {
    if content.is_empty() {
        return content;
    }
    let mut end_index = content.len();
    for i in (0..content.len()).rev() {
        let block = &content[i];
        if !block.is_object() {
            break;
        }
        if is_thinking_part(block) {
            if !has_valid_signature(block) {
                end_index = i;
            } else {
                break;
            }
        } else {
            break;
        }
    }
    if end_index < content.len() {
        content[..end_index].to_vec()
    } else {
        content
    }
}

pub fn restore_thinking_signatures(content: Vec<Value>) -> Vec<Value> {
    content
        .into_iter()
        .filter_map(|block| {
            if block.get("type").and_then(|v| v.as_str()) != Some("thinking") {
                return Some(block);
            }
            if block
                .get("signature")
                .and_then(|v| v.as_str())
                .map(|s| s.len() >= MIN_SIGNATURE_LENGTH)
                .unwrap_or(false)
            {
                Some(sanitize_anthropic_thinking_block(&block))
            } else {
                None
            }
        })
        .collect()
}

pub fn reorder_assistant_content(content: Vec<Value>) -> Vec<Value> {
    if content.len() == 1 {
        let t = content[0].get("type").and_then(|v| v.as_str());
        if t == Some("thinking") || t == Some("redacted_thinking") {
            return vec![sanitize_anthropic_thinking_block(&content[0])];
        }
        return content;
    }

    let mut thinking_blocks = Vec::new();
    let mut text_blocks = Vec::new();
    let mut tool_use_blocks = Vec::new();

    for block in content {
        match block.get("type").and_then(|v| v.as_str()) {
            Some("thinking") | Some("redacted_thinking") => {
                thinking_blocks.push(sanitize_anthropic_thinking_block(&block));
            }
            Some("tool_use") => {
                tool_use_blocks.push(sanitize_tool_use_block(&block));
            }
            Some("text") => {
                let non_empty = block
                    .get("text")
                    .and_then(|v| v.as_str())
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false);
                if non_empty {
                    text_blocks.push(sanitize_text_block(&block));
                }
            }
            _ => text_blocks.push(block),
        }
    }

    thinking_blocks
        .into_iter()
        .chain(text_blocks)
        .chain(tool_use_blocks)
        .collect()
}

fn message_has_valid_thinking(message: &Value) -> bool {
    let Some(blocks) = message
        .get("content")
        .or_else(|| message.get("parts"))
        .and_then(|v| v.as_array())
    else {
        return false;
    };
    blocks.iter().any(|block| {
        let t = block.get("type").and_then(|v| v.as_str());
        if t == Some("tool_use") || block.get("functionCall").is_some() {
            if block
                .get("thoughtSignature")
                .and_then(|v| v.as_str())
                .map(|s| s.len() >= MIN_SIGNATURE_LENGTH)
                .unwrap_or(false)
            {
                return true;
            }
            if let Some(id) = block.get("id").and_then(|v| v.as_str()) {
                if get_cached_signature(id).is_some() {
                    return true;
                }
            }
            return false;
        }
        if !is_thinking_part(block) {
            return false;
        }
        has_valid_signature(block)
            || block
                .get("thoughtSignature")
                .and_then(|v| v.as_str())
                .map(|s| s.len() >= MIN_SIGNATURE_LENGTH)
                .unwrap_or(false)
    })
}

fn message_has_tool_use(message: &Value) -> bool {
    message
        .get("content")
        .or_else(|| message.get("parts"))
        .and_then(|v| v.as_array())
        .map(|blocks| {
            blocks.iter().any(|b| {
                b.get("type").and_then(|v| v.as_str()) == Some("tool_use")
                    || b.get("functionCall").is_some()
            })
        })
        .unwrap_or(false)
}

fn message_has_tool_result(message: &Value) -> bool {
    message
        .get("content")
        .or_else(|| message.get("parts"))
        .and_then(|v| v.as_array())
        .map(|blocks| {
            blocks.iter().any(|b| {
                b.get("type").and_then(|v| v.as_str()) == Some("tool_result")
                    || b.get("functionResponse").is_some()
            })
        })
        .unwrap_or(false)
}

fn is_plain_user_message(message: &Value) -> bool {
    if message.get("role").and_then(|v| v.as_str()) != Some("user") {
        return false;
    }
    let Some(content) = message.get("content").or_else(|| message.get("parts")) else {
        return false;
    };
    match content {
        Value::String(_) => true,
        Value::Array(blocks) => !blocks.iter().any(|b| {
            b.get("type").and_then(|v| v.as_str()) == Some("tool_result")
                || b.get("functionResponse").is_some()
        }),
        _ => false,
    }
}

struct ConversationState {
    in_tool_loop: bool,
    interrupted_tool: bool,
    turn_has_thinking: bool,
    tool_result_count: usize,
    last_assistant_idx: Option<usize>,
}

fn analyze_conversation_state(messages: &[Value]) -> ConversationState {
    let last_assistant_idx = messages.iter().rposition(|m| {
        let r = m.get("role").and_then(|v| v.as_str()).unwrap_or("");
        r == "assistant" || r == "model"
    });

    let Some(idx) = last_assistant_idx else {
        return ConversationState {
            in_tool_loop: false,
            interrupted_tool: false,
            turn_has_thinking: false,
            tool_result_count: 0,
            last_assistant_idx: None,
        };
    };

    let last_assistant = &messages[idx];
    let has_tool_use = message_has_tool_use(last_assistant);
    let turn_has_thinking = message_has_valid_thinking(last_assistant);

    let mut tool_result_count = 0usize;
    let mut has_plain_user_after = false;
    for msg in &messages[idx + 1..] {
        if message_has_tool_result(msg) {
            tool_result_count += 1;
        }
        if is_plain_user_message(msg) {
            has_plain_user_after = true;
        }
    }

    ConversationState {
        in_tool_loop: has_tool_use && tool_result_count > 0,
        interrupted_tool: has_tool_use && tool_result_count == 0 && has_plain_user_after,
        turn_has_thinking,
        tool_result_count,
        last_assistant_idx: Some(idx),
    }
}

pub fn needs_thinking_recovery(messages: &[Value]) -> bool {
    let state = analyze_conversation_state(messages);
    if !state.in_tool_loop && !state.interrupted_tool {
        return false;
    }
    !state.turn_has_thinking
}

fn strip_invalid_thinking_blocks(messages: Vec<Value>, target_family: &str) -> Vec<Value> {
    messages
        .into_iter()
        .map(|msg| {
            let Some(mut obj) = msg.as_object().cloned() else {
                return msg;
            };
            let key = if obj.get("content").and_then(|v| v.as_array()).is_some() {
                "content"
            } else if obj.get("parts").and_then(|v| v.as_array()).is_some() {
                "parts"
            } else {
                return Value::Object(obj);
            };
            let Some(Value::Array(blocks)) = obj.remove(key) else {
                return Value::Object(obj);
            };
            let filtered: Vec<Value> = blocks
                .into_iter()
                .filter(|block| {
                    if !is_thinking_part(block) {
                        return true;
                    }
                    if !has_valid_signature(block) {
                        return false;
                    }
                    if target_family == "gemini" {
                        let sig = if block.get("thought").and_then(|v| v.as_bool()) == Some(true) {
                            block.get("thoughtSignature").and_then(|v| v.as_str())
                        } else {
                            block.get("signature").and_then(|v| v.as_str())
                        };
                        let fam = sig.and_then(get_cached_signature_family);
                        if fam.as_deref() != Some(target_family) {
                            return false;
                        }
                    }
                    true
                })
                .collect();

            if filtered.is_empty() {
                if key == "content" {
                    obj.insert(
                        "content".to_string(),
                        json!([{ "type": "text", "text": "." }]),
                    );
                } else {
                    obj.insert("parts".to_string(), json!([{ "text": "." }]));
                }
            } else {
                obj.insert(key.to_string(), Value::Array(filtered));
            }
            Value::Object(obj)
        })
        .collect()
}

pub fn close_tool_loop_for_thinking(messages: Vec<Value>, target_family: &str) -> Vec<Value> {
    if std::env::var("ANTIGRAVITY_DISABLE_THINKING_RECOVERY").as_deref() == Ok("1")
        && target_family == "gemini"
    {
        return messages;
    }

    let state = analyze_conversation_state(&messages);
    if !state.in_tool_loop && !state.interrupted_tool {
        return messages;
    }

    let mut modified = strip_invalid_thinking_blocks(messages, target_family);

    if state.interrupted_tool {
        if let Some(last_idx) = state.last_assistant_idx {
            let insert_idx = (last_idx + 1).min(modified.len());
            modified.insert(
                insert_idx,
                json!({
                    "role": "assistant",
                    "content": [{ "type": "text", "text": "[Tool call was interrupted.]" }]
                }),
            );
            logger().debug("[ThinkingUtils] Applied thinking recovery for interrupted tool");
        }
    } else if state.in_tool_loop {
        let synthetic_text = if state.tool_result_count == 1 {
            "[Tool execution completed.]".to_string()
        } else {
            format!("[{} tool executions completed.]", state.tool_result_count)
        };
        modified.push(json!({
            "role": "assistant",
            "content": [{ "type": "text", "text": synthetic_text }]
        }));
        modified.push(json!({
            "role": "user",
            "content": [{ "type": "text", "text": "[Continue]" }]
        }));
        logger().debug("[ThinkingUtils] Applied thinking recovery for tool loop");
    }

    modified
}
