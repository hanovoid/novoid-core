use chrono::DateTime;
use regex::Regex;
use reqwest::header::HeaderMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitReason {
    RateLimitExceeded,
    QuotaExhausted,
    ModelCapacityExhausted,
    ServerError,
    Unknown,
}

pub fn parse_reset_time(headers: Option<&HeaderMap>, error_text: &str) -> Option<u64> {
    let mut reset_ms: Option<i64> = None;

    if let Some(hdrs) = headers {
        if let Some(retry_after) = hdrs.get("retry-after").and_then(|v| v.to_str().ok()) {
            if let Ok(secs) = retry_after.trim().parse::<i64>() {
                reset_ms = Some(secs * 1000);
            } else if let Ok(dt) = DateTime::parse_from_rfc2822(retry_after.trim()) {
                let delta = dt.timestamp_millis() - chrono::Utc::now().timestamp_millis();
                if delta > 0 {
                    reset_ms = Some(delta);
                }
            }
        }

        if reset_ms.is_none() {
            if let Some(rl_reset) = hdrs.get("x-ratelimit-reset").and_then(|v| v.to_str().ok()) {
                if let Ok(ts_secs) = rl_reset.trim().parse::<i64>() {
                    let delta = ts_secs * 1000 - chrono::Utc::now().timestamp_millis();
                    if delta > 0 {
                        reset_ms = Some(delta);
                    }
                }
            }
        }

        if reset_ms.is_none() {
            if let Some(reset_after) = hdrs
                .get("x-ratelimit-reset-after")
                .and_then(|v| v.to_str().ok())
            {
                if let Ok(secs) = reset_after.trim().parse::<i64>() {
                    if secs > 0 {
                        reset_ms = Some(secs * 1000);
                    }
                }
            }
        }
    }

    if reset_ms.is_none() && !error_text.is_empty() {
        if let Ok(re) = Regex::new(r#"(?i)quotaResetDelay[:\s"]+(\d+(?:\.\d+)?)(ms|s)"#) {
            if let Some(cap) = re.captures(error_text) {
                if let (Ok(val), Some(unit)) =
                    (cap[1].parse::<f64>(), cap.get(2).map(|m| m.as_str().to_lowercase()))
                {
                    reset_ms = Some(if unit == "s" {
                        (val * 1000.0).ceil() as i64
                    } else {
                        val.ceil() as i64
                    });
                }
            }
        }

        if reset_ms.is_none() {
            if let Ok(re) = Regex::new(r#"(?i)quotaResetTimeStamp[:\s"]+(\d{4}-\d{2}-\d{2}T[\d:.]+Z?)"#) {
                if let Some(cap) = re.captures(error_text) {
                    if let Ok(dt) = DateTime::parse_from_rfc3339(&cap[1]) {
                        reset_ms = Some(dt.timestamp_millis() - chrono::Utc::now().timestamp_millis());
                    }
                }
            }
        }

        if reset_ms.is_none() {
            if let Ok(re) =
                Regex::new(r#"(?i)(?:retry[-_]?after[-_]?ms|retryDelay)[:\s"]+([\d.]+)(?:s\b|s")"#)
            {
                if let Some(cap) = re.captures(error_text) {
                    if let Ok(val) = cap[1].parse::<f64>() {
                        reset_ms = Some((val * 1000.0).ceil() as i64);
                    }
                }
            }
        }

        if reset_ms.is_none() {
            if let Ok(re) = Regex::new(r#"(?i)(?:retry[-_]?after[-_]?ms|retryDelay)[:\s"]+(\d+)(?:\s*ms)?"#) {
                if let Some(cap) = re.captures(error_text) {
                    if let Ok(ms) = cap[1].parse::<i64>() {
                        reset_ms = Some(ms);
                    }
                }
            }
        }

        if reset_ms.is_none() {
            if let Ok(re) = Regex::new(r"(?i)retry\s+(?:after\s+)?(\d+)\s*(?:sec|s\b)") {
                if let Some(cap) = re.captures(error_text) {
                    if let Ok(secs) = cap[1].parse::<i64>() {
                        reset_ms = Some(secs * 1000);
                    }
                }
            }
        }

        if reset_ms.is_none() {
            if let Ok(re) = Regex::new(r"(?i)(\d+)h(\d+)m(\d+)s|(\d+)m(\d+)s|(\d+)s") {
                if let Some(cap) = re.captures(error_text) {
                    if let (Some(h), Some(m), Some(s)) = (cap.get(1), cap.get(2), cap.get(3)) {
                        let total = h.as_str().parse::<i64>().unwrap_or(0) * 3600
                            + m.as_str().parse::<i64>().unwrap_or(0) * 60
                            + s.as_str().parse::<i64>().unwrap_or(0);
                        reset_ms = Some(total * 1000);
                    } else if let (Some(m), Some(s)) = (cap.get(4), cap.get(5)) {
                        let total = m.as_str().parse::<i64>().unwrap_or(0) * 60
                            + s.as_str().parse::<i64>().unwrap_or(0);
                        reset_ms = Some(total * 1000);
                    } else if let Some(s) = cap.get(6) {
                        let total = s.as_str().parse::<i64>().unwrap_or(0);
                        reset_ms = Some(total * 1000);
                    }
                }
            }
        }

        if reset_ms.is_none() {
            if let Ok(re) = Regex::new(r#"(?i)reset[:\s"]+(\d{4}-\d{2}-\d{2}T[\d:.]+Z?)"#) {
                if let Some(cap) = re.captures(error_text) {
                    if let Ok(dt) = DateTime::parse_from_rfc3339(&cap[1]) {
                        let delta = dt.timestamp_millis() - chrono::Utc::now().timestamp_millis();
                        if delta > 0 {
                            reset_ms = Some(delta);
                        }
                    }
                }
            }
        }
    }

    reset_ms.map(|ms| {
        if ms <= 0 {
            500
        } else if ms < 500 {
            (ms + 200) as u64
        } else {
            ms as u64
        }
    })
}

pub fn parse_rate_limit_reason(error_text: &str, status: Option<u16>) -> RateLimitReason {
    if let Some(st) = status {
        if st == 529 || st == 503 {
            return RateLimitReason::ModelCapacityExhausted;
        }
        if st == 500 {
            return RateLimitReason::ServerError;
        }
    }

    let lower = error_text.to_lowercase();
    if lower.contains("quota_exhausted")
        || lower.contains("quotaresetdelay")
        || lower.contains("quotaresettimestamp")
        || lower.contains("resource_exhausted")
        || lower.contains("daily limit")
        || lower.contains("quota exceeded")
    {
        return RateLimitReason::QuotaExhausted;
    }

    if lower.contains("model_capacity_exhausted")
        || lower.contains("capacity_exhausted")
        || lower.contains("model is currently overloaded")
        || lower.contains("service temporarily unavailable")
    {
        return RateLimitReason::ModelCapacityExhausted;
    }

    if lower.contains("rate_limit_exceeded")
        || lower.contains("rate limit")
        || lower.contains("too many requests")
        || lower.contains("throttl")
    {
        return RateLimitReason::RateLimitExceeded;
    }

    if lower.contains("internal server error")
        || lower.contains("server error")
        || lower.contains("503")
        || lower.contains("502")
        || lower.contains("504")
    {
        return RateLimitReason::ServerError;
    }

    RateLimitReason::Unknown
}
