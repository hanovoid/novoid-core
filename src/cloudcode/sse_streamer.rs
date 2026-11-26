use crate::constants::{get_model_family, MIN_SIGNATURE_LENGTH};
use crate::error::NovoidError;
use crate::format::convert_google_to_anthropic;
use crate::format::signature_cache::{cache_signature, cache_thinking_signature};
use crate::logger::logger;
use async_stream::try_stream;
use futures_util::{Stream, StreamExt};
use serde_json::{json, Value};
use uuid::Uuid;

pub fn stream_sse_response(
    response: reqwest::Response,
    original_model: String,
) -> impl Stream<Item = Result<Value, NovoidError>> + Send {
    try_stream! {
        let message_id = format!("msg_{}", Uuid::new_v4().simple());
        let mut has_emitted_start = false;
        let mut block_index: usize = 0;
        let mut current_block_type: Option<&'static str> = None;
        let mut current_thinking_signature = String::new();
        let mut input_tokens: u64 = 0;
        let mut output_tokens: u64 = 0;
        let mut cache_read_tokens: u64 = 0;
        let mut stop_reason: Option<&'static str> = None;

        let mut byte_stream = response.bytes_stream();
        let mut buffer = Vec::<u8>::new();

        while let Some(chunk_res) = byte_stream.next().await {
            let chunk = chunk_res.map_err(|e| NovoidError::Network {
                message: e.to_string(),
            })?;
            buffer.extend_from_slice(&chunk);

            while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = buffer.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line_bytes);
                let line = line.trim_end_matches(&['\r', '\n'][..]);
                if !line.starts_with("data:") {
                    continue;
                }
                let json_text = line[5..].trim();
                if json_text.is_empty() {
                    continue;
                }

                let data: Value = match serde_json::from_str(json_text) {
                    Ok(v) => v,
                    Err(err) => {
                        logger().warn(format!("[CloudCode] SSE parse error: {err}"));
                        continue;
                    }
                };

                let inner = data.get("response").unwrap_or(&data);

                if let Some(usage) = inner.get("usageMetadata") {
                    if let Some(pt) = usage.get("promptTokenCount").and_then(|v| v.as_u64()) {
                        input_tokens = pt;
                    }
                    let cand = usage.get("candidatesTokenCount").and_then(|v| v.as_u64()).unwrap_or(0);
                    let th = usage.get("thoughtsTokenCount").and_then(|v| v.as_u64()).unwrap_or(0);
                    if cand + th > 0 {
                        output_tokens = cand + th;
                    }
                    if let Some(cr) = usage.get("cachedContentTokenCount").and_then(|v| v.as_u64()) {
                        cache_read_tokens = cr;
                    }
                }

                let first_candidate = inner
                    .get("candidates")
                    .and_then(|v| v.as_array())
                    .and_then(|arr| arr.first());

                let parts = first_candidate
                    .and_then(|c| c.get("content"))
                    .and_then(|c| c.get("parts"))
                    .and_then(|p| p.as_array())
                    .cloned()
                    .unwrap_or_default();

                if !has_emitted_start && !parts.is_empty() {
                    has_emitted_start = true;
                    yield json!({
                        "type": "message_start",
                        "message": {
                            "id": message_id,
                            "type": "message",
                            "role": "assistant",
                            "content": [],
                            "model": original_model,
                            "stop_reason": Value::Null,
                            "stop_sequence": Value::Null,
                            "usage": {
                                "input_tokens": input_tokens.saturating_sub(cache_read_tokens),
                                "output_tokens": 0,
                                "cache_read_input_tokens": cache_read_tokens,
                                "cache_creation_input_tokens": 0
                            }
                        }
                    });
                }

                for part in parts {
                    if part.get("thought").and_then(|v| v.as_bool()) == Some(true) {
                        let text = part.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        let sig = part.get("thoughtSignature").and_then(|v| v.as_str()).unwrap_or("");

                        if current_block_type != Some("thinking") {
                            if current_block_type.is_some() {
                                yield json!({ "type": "content_block_stop", "index": block_index });
                                block_index += 1;
                            }
                            current_block_type = Some("thinking");
                            current_thinking_signature.clear();
                            yield json!({
                                "type": "content_block_start",
                                "index": block_index,
                                "content_block": { "type": "thinking", "thinking": "" }
                            });
                        }

                        if sig.len() >= MIN_SIGNATURE_LENGTH {
                            current_thinking_signature = sig.to_string();
                            let family = get_model_family(&original_model);
                            cache_thinking_signature(sig, &family);
                        }

                        yield json!({
                            "type": "content_block_delta",
                            "index": block_index,
                            "delta": { "type": "thinking_delta", "thinking": text }
                        });
                    } else if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                        if text.is_empty() {
                            continue;
                        }
                        if current_block_type != Some("text") {
                            if current_block_type == Some("thinking") && !current_thinking_signature.is_empty() {
                                yield json!({
                                    "type": "content_block_delta",
                                    "index": block_index,
                                    "delta": { "type": "signature_delta", "signature": current_thinking_signature }
                                });
                                current_thinking_signature.clear();
                            }
                            if current_block_type.is_some() {
                                yield json!({ "type": "content_block_stop", "index": block_index });
                                block_index += 1;
                            }
                            current_block_type = Some("text");
                            yield json!({
                                "type": "content_block_start",
                                "index": block_index,
                                "content_block": { "type": "text", "text": "" }
                            });
                        }
                        yield json!({
                            "type": "content_block_delta",
                            "index": block_index,
                            "delta": { "type": "text_delta", "text": text }
                        });
                    } else if let Some(fc) = part.get("functionCall") {
                        let fc_sig = part.get("thoughtSignature").and_then(|v| v.as_str()).unwrap_or("");
                        if current_block_type == Some("thinking") && !current_thinking_signature.is_empty() {
                            yield json!({
                                "type": "content_block_delta",
                                "index": block_index,
                                "delta": { "type": "signature_delta", "signature": current_thinking_signature }
                            });
                            current_thinking_signature.clear();
                        }
                        if current_block_type.is_some() {
                            yield json!({ "type": "content_block_stop", "index": block_index });
                            block_index += 1;
                        }
                        current_block_type = Some("tool_use");
                        stop_reason = Some("tool_use");

                        let tool_id = fc
                            .get("id")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| format!("toolu_{}", Uuid::new_v4().simple()));

                        let mut tool_use_block = json!({
                            "type": "tool_use",
                            "id": tool_id,
                            "name": fc.get("name").cloned().unwrap_or(Value::String(String::new())),
                            "input": {}
                        });

                        if fc_sig.len() >= MIN_SIGNATURE_LENGTH {
                            tool_use_block["thoughtSignature"] = Value::String(fc_sig.to_string());
                            cache_signature(&tool_id, fc_sig);
                        }

                        yield json!({
                            "type": "content_block_start",
                            "index": block_index,
                            "content_block": tool_use_block
                        });

                        let args_str = serde_json::to_string(
                            fc.get("args").unwrap_or(&json!({}))
                        ).unwrap_or_else(|_| "{}".to_string());

                        yield json!({
                            "type": "content_block_delta",
                            "index": block_index,
                            "delta": {
                                "type": "input_json_delta",
                                "partial_json": args_str
                            }
                        });
                    } else if let Some(inline) = part.get("inlineData") {
                        if current_block_type == Some("thinking") && !current_thinking_signature.is_empty() {
                            yield json!({
                                "type": "content_block_delta",
                                "index": block_index,
                                "delta": { "type": "signature_delta", "signature": current_thinking_signature }
                            });
                            current_thinking_signature.clear();
                        }
                        if current_block_type.is_some() {
                            yield json!({ "type": "content_block_stop", "index": block_index });
                            block_index += 1;
                        }

                        yield json!({
                            "type": "content_block_start",
                            "index": block_index,
                            "content_block": {
                                "type": "image",
                                "source": {
                                    "type": "base64",
                                    "media_type": inline.get("mimeType").cloned().unwrap_or(Value::Null),
                                    "data": inline.get("data").cloned().unwrap_or(Value::Null)
                                }
                            }
                        });
                        yield json!({ "type": "content_block_stop", "index": block_index });
                        block_index += 1;
                        current_block_type = None;
                    }
                }

                if stop_reason.is_none() {
                    if let Some(fr) = first_candidate
                        .and_then(|c| c.get("finishReason"))
                        .and_then(|v| v.as_str())
                    {
                        if fr == "MAX_TOKENS" {
                            stop_reason = Some("max_tokens");
                        } else if fr == "STOP" {
                            stop_reason = Some("end_turn");
                        }
                    }
                }
            }
        }

        if !has_emitted_start {
            logger().warn("[CloudCode] No content parts received, throwing for retry");
            Err(NovoidError::EmptyResponse)?;
        }

        if current_block_type.is_some() {
            if current_block_type == Some("thinking") && !current_thinking_signature.is_empty() {
                yield json!({
                    "type": "content_block_delta",
                    "index": block_index,
                    "delta": { "type": "signature_delta", "signature": current_thinking_signature }
                });
            }
            yield json!({ "type": "content_block_stop", "index": block_index });
        }

        yield json!({
            "type": "message_delta",
            "delta": { "stop_reason": stop_reason.unwrap_or("end_turn"), "stop_sequence": Value::Null },
            "usage": {
                "input_tokens": input_tokens.saturating_sub(cache_read_tokens),
                "output_tokens": output_tokens,
                "cache_read_input_tokens": cache_read_tokens,
                "cache_creation_input_tokens": 0
            }
        });
        yield json!({ "type": "message_stop" });
    }
}

pub async fn parse_thinking_sse_response(
    response: reqwest::Response,
    original_model: &str,
) -> Result<Value, NovoidError> {
    let mut accumulated_thinking_text = String::new();
    let mut accumulated_thinking_sig = String::new();
    let mut accumulated_text = String::new();
    let mut final_parts: Vec<Value> = Vec::new();
    let mut usage_metadata = json!({});
    let mut finish_reason = "STOP".to_string();

    let flush_thinking = |th_text: &mut String, th_sig: &mut String, parts: &mut Vec<Value>| {
        if !th_text.is_empty() {
            parts.push(json!({
                "thought": true,
                "text": std::mem::take(th_text),
                "thoughtSignature": std::mem::take(th_sig)
            }));
        }
    };

    let flush_text = |txt: &mut String, parts: &mut Vec<Value>| {
        if !txt.is_empty() {
            parts.push(json!({ "text": std::mem::take(txt) }));
        }
    };

    let mut byte_stream = response.bytes_stream();
    let mut buffer = Vec::<u8>::new();

    while let Some(chunk_res) = byte_stream.next().await {
        let chunk = chunk_res.map_err(|e| NovoidError::Network {
            message: e.to_string(),
        })?;
        buffer.extend_from_slice(&chunk);

        while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = buffer.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line_bytes);
            let line = line.trim_end_matches(&['\r', '\n'][..]);
            if !line.starts_with("data:") {
                continue;
            }
            let json_text = line[5..].trim();
            if json_text.is_empty() {
                continue;
            }
            let Ok(data) = serde_json::from_str::<Value>(json_text) else {
                continue;
            };
            let inner = data.get("response").unwrap_or(&data);
            if let Some(um) = inner.get("usageMetadata") {
                usage_metadata = um.clone();
            }
            if let Some(first_cand) = inner
                .get("candidates")
                .and_then(|v| v.as_array())
                .and_then(|arr| arr.first())
            {
                if let Some(fr) = first_cand.get("finishReason").and_then(|v| v.as_str()) {
                    finish_reason = fr.to_string();
                }
                if let Some(parts) = first_cand
                    .get("content")
                    .and_then(|c| c.get("parts"))
                    .and_then(|p| p.as_array())
                {
                    for part in parts {
                        if part.get("thought").and_then(|v| v.as_bool()) == Some(true) {
                            flush_text(&mut accumulated_text, &mut final_parts);
                            if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                                accumulated_thinking_text.push_str(t);
                            }
                            if let Some(sig) = part.get("thoughtSignature").and_then(|v| v.as_str()) {
                                accumulated_thinking_sig = sig.to_string();
                            }
                        } else if part.get("functionCall").is_some() {
                            flush_thinking(
                                &mut accumulated_thinking_text,
                                &mut accumulated_thinking_sig,
                                &mut final_parts,
                            );
                            flush_text(&mut accumulated_text, &mut final_parts);
                            final_parts.push(part.clone());
                        } else if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                            if !t.is_empty() {
                                flush_thinking(
                                    &mut accumulated_thinking_text,
                                    &mut accumulated_thinking_sig,
                                    &mut final_parts,
                                );
                                accumulated_text.push_str(t);
                            }
                        } else if part.get("inlineData").is_some() {
                            flush_thinking(
                                &mut accumulated_thinking_text,
                                &mut accumulated_thinking_sig,
                                &mut final_parts,
                            );
                            flush_text(&mut accumulated_text, &mut final_parts);
                            final_parts.push(part.clone());
                        }
                    }
                }
            }
        }
    }

    flush_thinking(
        &mut accumulated_thinking_text,
        &mut accumulated_thinking_sig,
        &mut final_parts,
    );
    flush_text(&mut accumulated_text, &mut final_parts);

    let accumulated_resp = json!({
        "candidates": [{ "content": { "parts": final_parts }, "finishReason": finish_reason }],
        "usageMetadata": usage_metadata
    });

    Ok(convert_google_to_anthropic(
        &accumulated_resp,
        original_model,
    ))
}

pub fn empty_response_fallback_events(model: &str) -> Vec<Value> {
    let message_id = format!("msg_{}", Uuid::new_v4().simple());
    vec![
        json!({
            "type": "message_start",
            "message": {
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": model,
                "stop_reason": Value::Null,
                "stop_sequence": Value::Null,
                "usage": { "input_tokens": 0, "output_tokens": 0 }
            }
        }),
        json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "text", "text": "" }
        }),
        json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": "[No response after retries - please try again]" }
        }),
        json!({ "type": "content_block_stop", "index": 0 }),
        json!({
            "type": "message_delta",
            "delta": { "stop_reason": "end_turn", "stop_sequence": Value::Null },
            "usage": { "output_tokens": 0 }
        }),
        json!({ "type": "message_stop" }),
    ]
}
