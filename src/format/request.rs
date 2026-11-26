use crate::constants::{get_model_family, is_thinking_model, GEMINI_MAX_OUTPUT_TOKENS};
use crate::format::content::{convert_content_to_parts, convert_role};
use crate::format::schema_sanitizer::{clean_schema, sanitize_schema};
use crate::format::thinking::{
    clamp_gemini_thinking_budget, clean_cache_control, close_tool_loop_for_thinking,
    filter_unsigned_thinking_blocks, has_gemini_history, has_unsigned_thinking_blocks,
    needs_thinking_recovery, remove_trailing_thinking_blocks, reorder_assistant_content,
    restore_thinking_signatures,
};
use crate::logger::logger;
use regex::Regex;
use serde_json::{json, Map, Value};

fn strip_billing_header(text: &str) -> String {
    if !text.to_lowercase().contains("x-anthropic-billing-header:") {
        return text.to_string();
    }
    if let Ok(re) = Regex::new(r"(?im)^x-anthropic-billing-header:[^\n]*\n?") {
        re.replace_all(text, "").trim().to_string()
    } else {
        text.to_string()
    }
}

pub fn convert_anthropic_to_google(anthropic_request: &Value) -> Value {
    let raw_messages = anthropic_request
        .get("messages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let messages = clean_cache_control(&raw_messages);

    let model_name = anthropic_request
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let model_family = get_model_family(model_name);
    let is_claude_model = model_family == "claude";
    let is_gemini_model = model_family == "gemini";
    let is_thinking = is_thinking_model(model_name);

    let max_tokens = anthropic_request
        .get("max_tokens")
        .and_then(|v| v.as_u64());
    let tools = anthropic_request.get("tools").and_then(|v| v.as_array());
    let thinking = anthropic_request.get("thinking");

    let mut google_req = Map::new();
    let mut contents = Vec::new();
    let mut gen_config = Map::new();

    // System instruction
    let mut system_parts: Vec<Value> = Vec::new();
    if let Some(sys) = anthropic_request.get("system") {
        match sys {
            Value::String(s) => {
                let stripped = strip_billing_header(s);
                if !stripped.is_empty() {
                    system_parts.push(json!({ "text": stripped }));
                }
            }
            Value::Array(arr) => {
                for b in arr {
                    if b.get("type").and_then(|v| v.as_str()) == Some("text") {
                        if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                            let stripped = strip_billing_header(t);
                            if !stripped.is_empty() {
                                system_parts.push(json!({ "text": stripped }));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let has_tools = tools.map(|t| !t.is_empty()).unwrap_or(false);
    if is_claude_model && is_thinking && has_tools {
        let hint = "Interleaved thinking is enabled. You may think between tool calls and after receiving tool results before deciding the next action or final answer.";
        if let Some(last) = system_parts.last_mut() {
            if let Some(existing) = last.get("text").and_then(|v| v.as_str()) {
                *last = json!({ "text": format!("{existing}\n\n{hint}") });
            } else {
                system_parts.push(json!({ "text": hint }));
            }
        } else {
            system_parts.push(json!({ "text": hint }));
        }
    }

    if !system_parts.is_empty() {
        google_req.insert(
            "systemInstruction".to_string(),
            json!({ "parts": system_parts }),
        );
    }

    // Thinking recovery
    let mut processed_messages = messages;
    if is_gemini_model && is_thinking && needs_thinking_recovery(&processed_messages) {
        logger().debug("[RequestConverter] Applying thinking recovery for Gemini");
        processed_messages = close_tool_loop_for_thinking(processed_messages, "gemini");
    }

    let needs_claude_recovery =
        has_gemini_history(&processed_messages) || has_unsigned_thinking_blocks(&processed_messages);
    if is_claude_model
        && is_thinking
        && needs_claude_recovery
        && needs_thinking_recovery(&processed_messages)
    {
        logger().debug("[RequestConverter] Applying thinking recovery for Claude");
        processed_messages = close_tool_loop_for_thinking(processed_messages, "claude");
    }

    // Convert messages
    for msg in processed_messages {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");
        let mut msg_content = msg.get("content").cloned().unwrap_or(Value::Null);

        if (role == "assistant" || role == "model") && msg_content.is_array() {
            if let Some(arr) = msg_content.as_array().cloned() {
                let arr = restore_thinking_signatures(arr);
                let arr = remove_trailing_thinking_blocks(arr);
                let arr = reorder_assistant_content(arr);
                msg_content = Value::Array(arr);
            }
        }

        let mut parts = convert_content_to_parts(&msg_content, is_claude_model, is_gemini_model);
        if parts.is_empty() {
            logger().warn(
                "[RequestConverter] WARNING: Empty parts array after filtering, adding placeholder",
            );
            parts.push(json!({ "text": "." }));
        }

        contents.push(json!({
            "role": convert_role(role),
            "parts": parts
        }));
    }

    if is_claude_model {
        contents = filter_unsigned_thinking_blocks(contents);
    }
    google_req.insert("contents".to_string(), Value::Array(contents));

    // Generation config
    if let Some(mt) = max_tokens {
        gen_config.insert("maxOutputTokens".to_string(), Value::from(mt));
    }
    if let Some(temp) = anthropic_request.get("temperature") {
        if !temp.is_null() {
            gen_config.insert("temperature".to_string(), temp.clone());
        }
    }
    if let Some(top_p) = anthropic_request.get("top_p") {
        if !top_p.is_null() {
            gen_config.insert("topP".to_string(), top_p.clone());
        }
    }
    if let Some(top_k) = anthropic_request.get("top_k") {
        if !top_k.is_null() {
            gen_config.insert("topK".to_string(), top_k.clone());
        }
    }
    if let Some(stops) = anthropic_request
        .get("stop_sequences")
        .and_then(|v| v.as_array())
    {
        if !stops.is_empty() {
            gen_config.insert("stopSequences".to_string(), Value::Array(stops.clone()));
        }
    }

    // Thinking config
    if is_thinking {
        let explicit_budget = thinking
            .and_then(|t| t.get("budget_tokens"))
            .and_then(|v| v.as_u64());
        let explicit_enabled =
            thinking.and_then(|t| t.get("type")).and_then(|v| v.as_str()) == Some("enabled");

        if is_claude_model {
            let thinking_budget = explicit_budget.unwrap_or(32_000);
            if let Some(cur_max) = gen_config.get("maxOutputTokens").and_then(|v| v.as_u64()) {
                if cur_max <= thinking_budget {
                    gen_config.insert(
                        "maxOutputTokens".to_string(),
                        Value::from(thinking_budget + 8192),
                    );
                }
            }
            gen_config.insert(
                "thinkingConfig".to_string(),
                json!({
                    "include_thoughts": true,
                    "thinking_budget": thinking_budget
                }),
            );
        } else if is_gemini_model {
            let lower_model = model_name.to_lowercase();
            let has_native_tier_level = lower_model.ends_with("-xhigh")
                || lower_model.ends_with("-high")
                || lower_model.ends_with("-medium")
                || lower_model.ends_with("-low")
                || lower_model.ends_with("-tiered")
                || lower_model.ends_with("-sonar")
                || lower_model.ends_with("-lite")
                || lower_model == "gemini-3-flash";

            let include_thoughts = explicit_enabled
                || explicit_budget.is_some()
                || max_tokens.map(|m| m > 256).unwrap_or(true);

            if has_native_tier_level && explicit_budget.is_none() {
                gen_config.insert(
                    "thinkingConfig".to_string(),
                    json!({
                        "includeThoughts": include_thoughts
                    }),
                );
            } else {
                let requested_budget = if let Some(b) = explicit_budget {
                    clamp_gemini_thinking_budget(model_name, Some(b))
                } else if max_tokens.map(|m| m <= 1024).unwrap_or(false) {
                    32
                } else {
                    clamp_gemini_thinking_budget(model_name, None)
                };

                if let Some(cur_max) = gen_config.get("maxOutputTokens").and_then(|v| v.as_u64()) {
                    if cur_max <= requested_budget {
                        let adjusted =
                            (requested_budget + cur_max.max(8192)).min(GEMINI_MAX_OUTPUT_TOKENS);
                        gen_config.insert("maxOutputTokens".to_string(), Value::from(adjusted));
                    }
                }

                gen_config.insert(
                    "thinkingConfig".to_string(),
                    json!({
                        "includeThoughts": include_thoughts,
                        "thinkingBudget": requested_budget
                    }),
                );
            }
        }
    }

    // Tools
    if let Some(tools_arr) = tools {
        if !tools_arr.is_empty() {
            let decls: Vec<Value> = tools_arr
                .iter()
                .enumerate()
                .map(|(idx, tool)| {
                    let raw_name = tool
                        .get("name")
                        .or_else(|| tool.get("function").and_then(|f| f.get("name")))
                        .or_else(|| tool.get("custom").and_then(|c| c.get("name")))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| format!("tool-{idx}"));
                    let clean_name: String = raw_name
                        .chars()
                        .map(|c| {
                            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                                c
                            } else {
                                '_'
                            }
                        })
                        .take(64)
                        .collect();

                    let desc = tool
                        .get("description")
                        .or_else(|| tool.get("function").and_then(|f| f.get("description")))
                        .or_else(|| tool.get("custom").and_then(|c| c.get("description")))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");

                    let raw_schema = tool
                        .get("input_schema")
                        .or_else(|| tool.get("function").and_then(|f| f.get("input_schema")))
                        .or_else(|| tool.get("function").and_then(|f| f.get("parameters")))
                        .or_else(|| tool.get("custom").and_then(|c| c.get("input_schema")))
                        .or_else(|| tool.get("parameters"));

                    let sanitized = sanitize_schema(raw_schema);
                    let parameters = clean_schema(sanitized);

                    json!({
                        "name": clean_name,
                        "description": desc,
                        "parameters": parameters
                    })
                })
                .collect();

            google_req.insert("tools".to_string(), json!([{ "functionDeclarations": decls }]));

            if is_claude_model {
                google_req.insert(
                    "toolConfig".to_string(),
                    json!({
                        "functionCallingConfig": {
                            "mode": "VALIDATED"
                        }
                    }),
                );
            }
        }
    }

    if is_gemini_model {
        if let Some(cur_max) = gen_config.get("maxOutputTokens").and_then(|v| v.as_u64()) {
            if cur_max > GEMINI_MAX_OUTPUT_TOKENS {
                gen_config.insert(
                    "maxOutputTokens".to_string(),
                    Value::from(GEMINI_MAX_OUTPUT_TOKENS),
                );
            }
        }
    }

    google_req.insert("generationConfig".to_string(), Value::Object(gen_config));
    Value::Object(google_req)
}
