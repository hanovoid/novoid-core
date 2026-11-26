use crate::account::AccountManager;
use crate::cloudcode::rate_limit_parser::parse_reset_time;
use crate::cloudcode::rate_limit_state::{
    calculate_smart_backoff, clear_rate_limit_state, extract_verification_url,
    get_rate_limit_backoff, is_account_banned, is_model_capacity_exhausted,
    is_permanent_auth_failure, is_validation_required,
};
use crate::cloudcode::request_builder::{build_cloudcode_request, build_headers};
use crate::cloudcode::sse_streamer::{
    empty_response_fallback_events, parse_thinking_sse_response, stream_sse_response,
};
use crate::config::get_config;
use crate::constants::{
    get_fallback_model, is_thinking_model, ANTIGRAVITY_ENDPOINT_FALLBACKS,
    BACKOFF_MODEL_CAPACITY_EXHAUSTED_MS, MAX_EMPTY_RESPONSE_RETRIES,
};
use crate::error::NovoidError;
use crate::format::convert_google_to_anthropic;
use crate::logger::logger;
use crate::utils::{format_duration, http_client, is_network_error, maybe_throttle, now_ms};
use async_stream::try_stream;
use futures_util::{Stream, StreamExt};
use regex::Regex;
use serde_json::Value;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

fn is_400_unsupported_model(err_text: &str) -> bool {
    Regex::new(r"(?i)model.*not found|not supported|unknown model|entity was not found")
        .map(|re| re.is_match(err_text))
        .unwrap_or(false)
}

struct ReadyStream {
    response: reqwest::Response,
    endpoint_url: String,
    token: String,
    payload: Value,
    session_id: String,
    account_email: String,
    model: String,
}

async fn acquire_upstream_response(
    anthropic_request: &Value,
    account_manager: &Arc<AccountManager>,
    stream: bool,
    fallback_enabled: bool,
) -> Result<(ReadyStream, Option<Value>), NovoidError> {
    let model = anthropic_request
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let is_thinking = is_thinking_model(&model);
    let use_sse = stream || is_thinking;
    let accept = if use_sse {
        "text/event-stream"
    } else {
        "application/json"
    };

    let cfg = get_config();
    let max_attempts = cfg.max_retries.max(account_manager.get_account_count().await);
    let mut attempt: isize = 0;

    while (attempt as usize) < max_attempts {
        account_manager.clear_expired_limits().await;
        let available = account_manager.get_available_accounts(Some(&model)).await;

        if available.is_empty() {
            if account_manager.is_all_accounts_invalid().await {
                let invalids = account_manager.get_invalid_accounts().await;
                let mut reasons: Vec<String> = invalids
                    .into_iter()
                    .filter_map(|a| a.invalid_reason)
                    .collect();
                reasons.sort();
                reasons.dedup();
                let r_str = if reasons.is_empty() {
                    "unknown reason".to_string()
                } else {
                    reasons.join("; ")
                };
                return Err(NovoidError::NoAccounts {
                    message: format!(
                        "All accounts are invalid: {r_str}. Visit the WebUI to fix them."
                    ),
                    all_rate_limited: false,
                });
            }

            if account_manager.is_all_rate_limited(Some(&model)).await {
                let min_wait_ms = account_manager.get_min_wait_time_ms(Some(&model)).await;
                if min_wait_ms > cfg.max_wait_before_error_ms {
                    if fallback_enabled {
                        if let Some(fb_model) = get_fallback_model(&model) {
                            logger().warn(format!(
                                "[CloudCode] All accounts exhausted for {model} ({} wait). Attempting fallback to {fb_model}",
                                format_duration(min_wait_ms)
                            ));
                            let mut fb_req = anthropic_request.clone();
                            fb_req["model"] = Value::String(fb_model.to_string());
                            return Box::pin(acquire_upstream_response(
                                &fb_req,
                                account_manager,
                                stream,
                                false,
                            ))
                            .await;
                        }
                    }
                    let reset_ts = chrono::DateTime::from_timestamp_millis(
                        (now_ms() + min_wait_ms) as i64,
                    )
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_default();
                    return Err(NovoidError::RateLimited {
                        message: format!(
                            "RESOURCE_EXHAUSTED: Rate limited on {model}. Quota will reset after {}. Next available: {reset_ts}",
                            format_duration(min_wait_ms)
                        ),
                        reset_ms: Some(min_wait_ms),
                        account_email: None,
                        is_dedup: false,
                    });
                }

                let acc_count = account_manager.get_account_count().await;
                logger().warn(format!(
                    "[CloudCode] All {acc_count} account(s) rate-limited. Waiting {}...",
                    format_duration(min_wait_ms)
                ));
                tokio::time::sleep(Duration::from_millis(min_wait_ms + 500)).await;
                account_manager.clear_expired_limits().await;
                continue;
            }

            if !account_manager.has_supporting_account(&model).await {
                return Err(NovoidError::ModelNotSupported {
                    message: format!(
                        "MODEL_NOT_SUPPORTED: Model {model} is not supported by any available account."
                    ),
                    account_email: None,
                    model_id: Some(model),
                });
            }

            return Err(NovoidError::NoAccounts {
                message: "No accounts available".to_string(),
                all_rate_limited: false,
            });
        }

        let (maybe_account, wait_ms) = account_manager.select_account(&model).await;
        if maybe_account.is_none() && wait_ms > 0 {
            logger().info(format!(
                "[CloudCode] Waiting {} for account...",
                format_duration(wait_ms)
            ));
            tokio::time::sleep(Duration::from_millis(wait_ms + 500)).await;
            continue;
        }
        if maybe_account.is_some() && wait_ms > 0 {
            logger().debug(format!(
                "[CloudCode] Throttling request ({wait_ms}ms) - fallback mode active"
            ));
            tokio::time::sleep(Duration::from_millis(wait_ms)).await;
        }

        let Some(account) = maybe_account else {
            attempt += 1;
            continue;
        };

        let attempt_res = async {
            let token = account_manager.get_token_for_account(&account).await?;
            let project = account_manager
                .get_project_for_account(&account, &token)
                .await;
            if account_manager.is_account_invalid(&account.email).await {
                return Err(NovoidError::AccountForbidden {
                    message: format!("Account {} requires verification", account.email),
                    account_email: Some(account.email.clone()),
                });
            }
            let (payload, session_id) =
                build_cloudcode_request(anthropic_request, &project, &account.email);

            let mut last_error: Option<NovoidError> = None;
            let mut capacity_retries = 0usize;
            let mut endpoint_idx = 0usize;

            while endpoint_idx < ANTIGRAVITY_ENDPOINT_FALLBACKS.len() {
                let endpoint = ANTIGRAVITY_ENDPOINT_FALLBACKS[endpoint_idx];
                let url = if use_sse {
                    format!("{endpoint}/v1internal:streamGenerateContent?alt=sse")
                } else {
                    format!("{endpoint}/v1internal:generateContent")
                };

                maybe_throttle().await;
                let resp_res = http_client()
                    .post(&url)
                    .bearer_auth(&token)
                    .headers(build_headers(&model, accept, Some(&session_id)))
                    .json(&payload)
                    .send()
                    .await;

                let response = match resp_res {
                    Ok(r) => r,
                    Err(err) => {
                        logger().warn(format!("[CloudCode] Error at {endpoint}: {err}"));
                        last_error = Some(if is_network_error(&err) {
                            NovoidError::Network {
                                message: err.to_string(),
                            }
                        } else {
                            NovoidError::Internal {
                                message: err.to_string(),
                            }
                        });
                        endpoint_idx += 1;
                        continue;
                    }
                };

                let status = response.status().as_u16();
                if status < 400 {
                    return Ok((
                        ReadyStream {
                            response,
                            endpoint_url: url,
                            token,
                            payload,
                            session_id,
                            account_email: account.email.clone(),
                            model: model.clone(),
                        },
                        if fallback_enabled {
                            None
                        } else {
                            Some(anthropic_request.clone())
                        },
                    ));
                }

                let resp_headers = response.headers().clone();
                let err_text = response.text().await.unwrap_or_default();
                logger().warn(format!(
                    "[CloudCode] Error at {endpoint}: {status} - {err_text}"
                ));

                if status == 401 {
                    if is_permanent_auth_failure(&err_text) {
                        account_manager
                            .mark_invalid(
                                &account.email,
                                "Token revoked - re-authentication required",
                                None,
                            )
                            .await;
                        return Err(NovoidError::AuthInvalidPermanent {
                            message: err_text,
                            account_email: Some(account.email.clone()),
                        });
                    }
                    account_manager.clear_token_cache(Some(&account.email)).await;
                    account_manager
                        .clear_project_cache(Some(&account.email))
                        .await;
                    endpoint_idx += 1;
                    continue;
                }

                if status == 429 {
                    let reset_ms = parse_reset_time(Some(&resp_headers), &err_text);
                    let consec = account_manager.get_consecutive_failures(&account.email).await;

                    if is_model_capacity_exhausted(&err_text) {
                        if capacity_retries < cfg.max_capacity_retries {
                            let tier_idx = capacity_retries
                                .min(cfg.capacity_backoff_tiers_ms.len().saturating_sub(1));
                            let wait_ms = reset_ms.unwrap_or_else(|| {
                                cfg.capacity_backoff_tiers_ms
                                    .get(tier_idx)
                                    .copied()
                                    .unwrap_or(10_000)
                            });
                            capacity_retries += 1;
                            account_manager
                                .increment_consecutive_failures(&account.email)
                                .await;
                            logger().info(format!(
                                "[CloudCode] Model capacity exhausted, retry {capacity_retries}/{} after {}...",
                                cfg.max_capacity_retries,
                                format_duration(wait_ms)
                            ));
                            tokio::time::sleep(Duration::from_millis(wait_ms)).await;
                            continue;
                        }
                        logger().warn(format!(
                            "[CloudCode] Max capacity retries ({}) exceeded, switching account",
                            cfg.max_capacity_retries
                        ));
                    }

                    let backoff = get_rate_limit_backoff(&account.email, &model, reset_ms);

                    if let Some(rms) = reset_ms {
                        if rms < 1000 {
                            logger().info(format!(
                                "[CloudCode] Short rate limit on {} ({rms}ms), waiting and retrying...",
                                account.email
                            ));
                            tokio::time::sleep(Duration::from_millis(rms)).await;
                            continue;
                        }
                    }

                    if backoff.is_duplicate {
                        let smart_ms = calculate_smart_backoff(&err_text, reset_ms, consec);
                        logger().info(format!(
                            "[CloudCode] Skipping retry due to recent rate limit on {} (attempt {}), switching account...",
                            account.email, backoff.attempt
                        ));
                        account_manager
                            .mark_rate_limited(&account.email, Some(smart_ms), &model)
                            .await;
                        return Err(NovoidError::RateLimited {
                            message: err_text,
                            reset_ms: Some(smart_ms),
                            account_email: Some(account.email.clone()),
                            is_dedup: true,
                        });
                    }

                    let smart_ms = calculate_smart_backoff(&err_text, reset_ms, consec);

                    if backoff.attempt == 1 && smart_ms <= cfg.default_cooldown_ms {
                        let wait_ms = backoff.delay_ms;
                        account_manager
                            .mark_rate_limited(&account.email, Some(wait_ms), &model)
                            .await;
                        logger().info(format!(
                            "[CloudCode] First rate limit on {}, retrying after {}...",
                            account.email,
                            format_duration(wait_ms)
                        ));
                        tokio::time::sleep(Duration::from_millis(wait_ms)).await;
                        continue;
                    } else if smart_ms > cfg.default_cooldown_ms {
                        logger().info(format!(
                            "[CloudCode] Quota exhausted for {} ({}), switching account after {} delay...",
                            account.email,
                            format_duration(smart_ms),
                            format_duration(cfg.switch_account_delay_ms)
                        ));
                        tokio::time::sleep(Duration::from_millis(cfg.switch_account_delay_ms)).await;
                        account_manager
                            .mark_rate_limited(&account.email, Some(smart_ms), &model)
                            .await;
                        return Err(NovoidError::RateLimited {
                            message: err_text,
                            reset_ms: Some(smart_ms),
                            account_email: Some(account.email.clone()),
                            is_dedup: false,
                        });
                    } else {
                        let wait_ms = backoff.delay_ms;
                        account_manager
                            .mark_rate_limited(&account.email, Some(wait_ms), &model)
                            .await;
                        logger().info(format!(
                            "[CloudCode] Rate limit on {} (attempt {}), waiting {}...",
                            account.email,
                            backoff.attempt,
                            format_duration(wait_ms)
                        ));
                        tokio::time::sleep(Duration::from_millis(wait_ms)).await;
                        continue;
                    }
                }

                if (status == 503 || status == 529) && is_model_capacity_exhausted(&err_text) {
                    let reset_ms = parse_reset_time(Some(&resp_headers), &err_text);
                    if capacity_retries < cfg.max_capacity_retries {
                        let tier_idx = capacity_retries
                            .min(cfg.capacity_backoff_tiers_ms.len().saturating_sub(1));
                        let fallback_wait = cfg
                            .capacity_backoff_tiers_ms
                            .get(tier_idx)
                            .copied()
                            .unwrap_or(10_000);
                        let wait_ms = reset_ms
                            .map(|ms| ms.clamp(300, fallback_wait))
                            .unwrap_or(fallback_wait);
                        capacity_retries += 1;
                        account_manager
                            .increment_consecutive_failures(&account.email)
                            .await;
                        logger().info(format!(
                            "[CloudCode] {status} Model capacity exhausted, retry {capacity_retries}/{} after {}...",
                            cfg.max_capacity_retries,
                            format_duration(wait_ms)
                        ));
                        tokio::time::sleep(Duration::from_millis(wait_ms)).await;
                        continue;
                    }
                    logger().warn(format!(
                        "[CloudCode] Max capacity retries ({}) exceeded on {status}, switching account",
                        cfg.max_capacity_retries
                    ));
                    account_manager
                        .mark_rate_limited(
                            &account.email,
                            Some(reset_ms.unwrap_or(BACKOFF_MODEL_CAPACITY_EXHAUSTED_MS)),
                            &model,
                        )
                        .await;
                    return Err(NovoidError::CapacityExhausted {
                        message: err_text,
                        retry_after_ms: Some(reset_ms.unwrap_or(BACKOFF_MODEL_CAPACITY_EXHAUSTED_MS)),
                    });
                }

                if status == 400 {
                    if is_400_unsupported_model(&err_text) {
                        return Err(NovoidError::ModelNotSupported {
                            message: format!(
                                "MODEL_NOT_SUPPORTED: API error 400 for {model} on {}: {err_text}",
                                account.email
                            ),
                            account_email: Some(account.email.clone()),
                            model_id: Some(model.clone()),
                        });
                    }
                    return Err(NovoidError::InvalidRequest { message: err_text });
                }

                if status == 403 && is_validation_required(&err_text) {
                    let verify_url = extract_verification_url(&err_text);
                    logger().warn(format!(
                        "[CloudCode] 403 VALIDATION_REQUIRED/PERMISSION_DENIED for {}, marking invalid and rotating...",
                        account.email
                    ));
                    account_manager
                        .mark_invalid(&account.email, "Account requires verification", verify_url)
                        .await;
                    return Err(NovoidError::AccountForbidden {
                        message: err_text,
                        account_email: Some(account.email.clone()),
                    });
                }

                if status == 403 && is_account_banned(&err_text) {
                    logger().warn(format!(
                        "[CloudCode] 403 ToS BANNED for {}, marking invalid permanently...",
                        account.email
                    ));
                    account_manager
                        .mark_invalid(
                            &account.email,
                            "Account banned — Gemini disabled for Terms of Service violation",
                            None,
                        )
                        .await;
                    return Err(NovoidError::AccountBanned {
                        message: err_text,
                        account_email: Some(account.email.clone()),
                    });
                }

                if status == 404 {
                    last_error = Some(NovoidError::ModelNotSupported {
                        message: format!(
                            "MODEL_NOT_SUPPORTED: API error 404 for {model} on {}: {err_text}",
                            account.email
                        ),
                        account_email: Some(account.email.clone()),
                        model_id: Some(model.clone()),
                    });
                    endpoint_idx += 1;
                    continue;
                }

                last_error = Some(NovoidError::UpstreamApi {
                    status_code: status,
                    message: err_text,
                });
                if status >= 500 {
                    logger().warn(format!(
                        "[CloudCode] {status} error, waiting 3s before retry..."
                    ));
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
                endpoint_idx += 1;
            }

            Err(last_error.unwrap_or_else(|| NovoidError::Internal {
                message: "All endpoints failed".to_string(),
            }))
        }
        .await;

        match attempt_res {
            Ok(ready) => return Ok(ready),
            Err(err) => {
                if let NovoidError::RateLimited { is_dedup, .. } = &err {
                    account_manager.notify_rate_limit(&account.email).await;
                    logger().info(format!(
                        "[CloudCode] Account {} rate-limited, trying next...",
                        account.email
                    ));
                    if !*is_dedup {
                        attempt += 1;
                    }
                    continue;
                }
                if matches!(err, NovoidError::CapacityExhausted { .. }) {
                    account_manager.notify_rate_limit(&account.email).await;
                    attempt += 1;
                    continue;
                }
                if err.is_auth_error() {
                    logger().warn(format!(
                        "[CloudCode] Account {} has invalid credentials, trying next...",
                        account.email
                    ));
                    attempt += 1;
                    continue;
                }
                if err.is_account_forbidden() {
                    account_manager.notify_failure(&account.email).await;
                    attempt += 1;
                    continue;
                }
                if err.is_model_not_supported() {
                    account_manager
                        .mark_model_unsupported(&account.email, &model)
                        .await;
                    attempt += 1;
                    continue;
                }
                if err.is_network_or_5xx() {
                    account_manager.notify_failure(&account.email).await;
                    let cur_failures = account_manager
                        .get_consecutive_failures(&account.email)
                        .await;
                    if cur_failures + 1 >= cfg.max_consecutive_failures {
                        logger().warn(format!(
                            "[CloudCode] Account {} has {} consecutive failures, applying extended cooldown ({})",
                            account.email,
                            cur_failures + 1,
                            format_duration(cfg.extended_cooldown_ms)
                        ));
                        account_manager
                            .mark_rate_limited(
                                &account.email,
                                Some(cfg.extended_cooldown_ms),
                                &model,
                            )
                            .await;
                    } else {
                        account_manager
                            .increment_consecutive_failures(&account.email)
                            .await;
                    }
                    if matches!(err, NovoidError::Network { .. } | NovoidError::AuthNetwork { .. }) {
                        tokio::time::sleep(Duration::from_secs(3)).await;
                    }
                    attempt += 1;
                    continue;
                }

                return Err(err);
            }
        }
    }

    if fallback_enabled {
        if let Some(fb_model) = get_fallback_model(&model) {
            logger().warn(format!(
                "[CloudCode] All retries exhausted for {model}. Attempting fallback to {fb_model}"
            ));
            let mut fb_req = anthropic_request.clone();
            fb_req["model"] = Value::String(fb_model.to_string());
            return Box::pin(acquire_upstream_response(
                &fb_req,
                account_manager,
                stream,
                false,
            ))
            .await;
        }
    }

    if !account_manager.has_supporting_account(&model).await {
        return Err(NovoidError::ModelNotSupported {
            message: format!(
                "MODEL_NOT_SUPPORTED: Model {model} is not supported by any available account."
            ),
            account_email: None,
            model_id: Some(model),
        });
    }

    if account_manager.is_all_rate_limited(Some(&model)).await {
        let min_wait_ms = account_manager.get_min_wait_time_ms(Some(&model)).await;
        let reset_ts = chrono::DateTime::from_timestamp_millis((now_ms() + min_wait_ms) as i64)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_default();
        return Err(NovoidError::RateLimited {
            message: format!(
                "RESOURCE_EXHAUSTED: Rate limited on {model}. Quota will reset after {}. Next available: {reset_ts}",
                format_duration(min_wait_ms)
            ),
            reset_ms: Some(min_wait_ms),
            account_email: None,
            is_dedup: false,
        });
    }

    Err(NovoidError::MaxRetries {
        message: "Max retries exceeded".to_string(),
        attempts: max_attempts,
    })
}

pub async fn send_message(
    anthropic_request: &Value,
    account_manager: &Arc<AccountManager>,
    fallback_enabled: bool,
) -> Result<Value, NovoidError> {
    let (ready, _) =
        acquire_upstream_response(anthropic_request, account_manager, false, fallback_enabled)
            .await?;

    let is_thinking = is_thinking_model(&ready.model);
    let result = if is_thinking {
        parse_thinking_sse_response(ready.response, &ready.model).await?
    } else {
        let data: Value = ready
            .response
            .json()
            .await
            .map_err(|e| NovoidError::Internal {
                message: format!("Failed to parse upstream JSON: {e}"),
            })?;
        convert_google_to_anthropic(&data, &ready.model)
    };

    clear_rate_limit_state(&ready.account_email, &ready.model);
    account_manager.notify_success(&ready.account_email).await;
    Ok(result)
}

pub async fn send_message_stream_prepared(
    anthropic_request: &Value,
    account_manager: &Arc<AccountManager>,
    fallback_enabled: bool,
) -> Result<
    (
        String,
        Pin<Box<dyn Stream<Item = Result<Value, NovoidError>> + Send>>,
    ),
    NovoidError,
> {
    let (ready, _) =
        acquire_upstream_response(anthropic_request, account_manager, true, fallback_enabled)
            .await?;

    let actual_model = ready.model.clone();
    let am = Arc::clone(account_manager);

    let stream: Pin<Box<dyn Stream<Item = Result<Value, NovoidError>> + Send>> =
        Box::pin(try_stream! {
            let mut empty_retries = 0usize;
            let mut cur_response = Some(ready.response);
            while let Some(resp_to_stream) = cur_response.take() {
                let mut s = Box::pin(stream_sse_response(
                    resp_to_stream,
                    ready.model.clone(),
                ));

                match s.next().await {
                    Some(Ok(first_event)) => {
                        let email = ready.account_email.clone();
                        let model_name = ready.model.clone();
                        yield first_event;
                        while let Some(ev_res) = s.next().await {
                            let ev = ev_res?;
                            yield ev;
                        }
                        clear_rate_limit_state(&email, &model_name);
                        am.notify_success(&email).await;
                        break;
                    }
                    Some(Err(NovoidError::EmptyResponse)) | None => {
                        if empty_retries >= MAX_EMPTY_RESPONSE_RETRIES {
                            logger().error(format!(
                                "[CloudCode] Empty response after {MAX_EMPTY_RESPONSE_RETRIES} retries"
                            ));
                            for ev in empty_response_fallback_events(&ready.model) {
                                yield ev;
                            }
                            break;
                        }

                        let backoff_ms = 500 * (1u64 << empty_retries);
                        empty_retries += 1;
                        logger().warn(format!(
                            "[CloudCode] Empty response, retry {empty_retries}/{MAX_EMPTY_RESPONSE_RETRIES} after {backoff_ms}ms..."
                        ));
                        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;

                        maybe_throttle().await;
                        let retry_resp = http_client()
                            .post(&ready.endpoint_url)
                            .bearer_auth(&ready.token)
                            .headers(build_headers(
                                &ready.model,
                                "text/event-stream",
                                Some(&ready.session_id),
                            ))
                            .json(&ready.payload)
                            .send()
                            .await
                            .map_err(|e| NovoidError::Network {
                                message: e.to_string(),
                            })?;

                        let status = retry_resp.status().as_u16();
                        if status >= 400 {
                            let hdrs = retry_resp.headers().clone();
                            let text = retry_resp.text().await.unwrap_or_default();
                            let err = if status == 429 {
                                let rms = parse_reset_time(Some(&hdrs), &text);
                                am.mark_rate_limited(&ready.account_email, rms, &ready.model)
                                    .await;
                                NovoidError::RateLimited {
                                    message: text,
                                    reset_ms: rms,
                                    account_email: Some(ready.account_email.clone()),
                                    is_dedup: false,
                                }
                            } else if status == 401 {
                                if is_permanent_auth_failure(&text) {
                                    am.mark_invalid(
                                        &ready.account_email,
                                        "Token revoked - re-authentication required",
                                        None,
                                    )
                                    .await;
                                    NovoidError::AuthInvalidPermanent {
                                        message: text,
                                        account_email: Some(ready.account_email.clone()),
                                    }
                                } else {
                                    am.clear_token_cache(Some(&ready.account_email)).await;
                                    NovoidError::AuthInvalid {
                                        message: text,
                                        account_email: Some(ready.account_email.clone()),
                                    }
                                }
                            } else {
                                NovoidError::UpstreamApi {
                                    status_code: status,
                                    message: text,
                                }
                            };
                            Err(err)?;
                        } else {
                            cur_response = Some(retry_resp);
                        }
                    }
                    Some(Err(err)) => {
                        Err(err)?;
                    }
                }
            }
        });

    Ok((actual_model, stream))
}
