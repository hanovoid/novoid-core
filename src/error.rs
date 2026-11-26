use axum::http::StatusCode;
use regex::Regex;
use serde_json::{json, Value};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum NovoidError {
    #[error("RATE_LIMITED: {message}")]
    RateLimited {
        message: String,
        reset_ms: Option<u64>,
        account_email: Option<String>,
        is_dedup: bool,
    },

    #[error("CAPACITY_EXHAUSTED: {message}")]
    CapacityExhausted {
        message: String,
        retry_after_ms: Option<u64>,
    },

    #[error("AUTH_INVALID: {message}")]
    AuthInvalid {
        message: String,
        account_email: Option<String>,
    },

    #[error("AUTH_INVALID_PERMANENT: {message}")]
    AuthInvalidPermanent {
        message: String,
        account_email: Option<String>,
    },

    #[error("AUTH_NETWORK_ERROR: {message}")]
    AuthNetwork { message: String },

    #[error("ACCOUNT_FORBIDDEN: {message}")]
    AccountForbidden {
        message: String,
        account_email: Option<String>,
    },

    #[error("ACCOUNT_BANNED: {message}")]
    AccountBanned {
        message: String,
        account_email: Option<String>,
    },

    #[error("MODEL_NOT_SUPPORTED: {message}")]
    ModelNotSupported {
        message: String,
        account_email: Option<String>,
        model_id: Option<String>,
    },

    #[error("{message}")]
    NoAccounts {
        message: String,
        all_rate_limited: bool,
    },

    #[error("invalid_request_error: {message}")]
    InvalidRequest { message: String },

    #[error("No content received from API")]
    EmptyResponse,

    #[error("{message}")]
    MaxRetries { message: String, attempts: usize },

    #[error("API error {status_code}: {message}")]
    UpstreamApi { status_code: u16, message: String },

    #[error("Network error: {message}")]
    Network { message: String },

    #[error("{message}")]
    Internal { message: String },
}

impl NovoidError {
    pub fn is_rate_limit(&self) -> bool {
        matches!(self, Self::RateLimited { .. } | Self::CapacityExhausted { .. })
    }

    pub fn is_auth_error(&self) -> bool {
        matches!(self, Self::AuthInvalid { .. } | Self::AuthInvalidPermanent { .. })
    }

    pub fn is_account_forbidden(&self) -> bool {
        matches!(self, Self::AccountForbidden { .. } | Self::AccountBanned { .. })
    }

    pub fn is_model_not_supported(&self) -> bool {
        matches!(self, Self::ModelNotSupported { .. })
    }

    pub fn is_empty_response(&self) -> bool {
        matches!(self, Self::EmptyResponse)
    }

    pub fn is_network_or_5xx(&self) -> bool {
        match self {
            Self::Network { .. } | Self::AuthNetwork { .. } => true,
            Self::UpstreamApi { status_code, .. } => *status_code >= 500,
            _ => false,
        }
    }

    /// Map structured error to Anthropic-compatible HTTP status and JSON body.
    pub fn to_anthropic_response(&self) -> (StatusCode, Value) {
        let msg = self.to_string();
        let (status, error_type, error_message) = match self {
            Self::ModelNotSupported { .. } => (StatusCode::NOT_FOUND, "not_found_error", msg),
            Self::AuthInvalid { .. } | Self::AuthInvalidPermanent { .. } => (
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "Authentication failed. Please refresh or re-authenticate your Google account."
                    .to_string(),
            ),
            Self::RateLimited { .. } | Self::CapacityExhausted { .. } => {
                // Use 400 invalid_request_error so clients do not trigger retry storms
                let reset_re = Regex::new(r"(?i)quota will reset after ([\dh\dm\ds]+)").ok();
                let model_re = Regex::new(r#"Rate limited on ([^.]+)\.|"model":\s*"([^"]+)""#).ok();

                let reset_str = reset_re
                    .and_then(|re| re.captures(&msg))
                    .and_then(|c| c.get(1))
                    .map(|m| m.as_str().to_string());
                let model = model_re
                    .and_then(|re| re.captures(&msg))
                    .and_then(|c| c.get(1).or_else(|| c.get(2)))
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_else(|| "the model".to_string());

                let formatted = if let Some(reset) = reset_str {
                    format!(
                        "RESOURCE_EXHAUSTED: You have exhausted your capacity on {model}. Quota will reset after {reset}."
                    )
                } else {
                    format!(
                        "RESOURCE_EXHAUSTED: You have exhausted your capacity on {model}. Please wait for your quota to reset."
                    )
                };
                (StatusCode::BAD_REQUEST, "invalid_request_error", formatted)
            }
            Self::InvalidRequest { message } => {
                let extracted = Regex::new(r#""message":"([^"]+)""#)
                    .ok()
                    .and_then(|re| re.captures(message))
                    .and_then(|c| c.get(1))
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_else(|| message.clone());
                (StatusCode::BAD_REQUEST, "invalid_request_error", extracted)
            }
            Self::AccountForbidden { .. } | Self::AccountBanned { .. } => {
                (StatusCode::FORBIDDEN, "permission_error", msg)
            }
            Self::NoAccounts { message, .. } => {
                (StatusCode::SERVICE_UNAVAILABLE, "api_error", message.clone())
            }
            Self::UpstreamApi { status_code, .. } if *status_code == 403 => {
                (StatusCode::FORBIDDEN, "permission_error", msg)
            }
            Self::UpstreamApi { status_code, .. } if *status_code == 401 => (
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "Authentication failed. Please refresh or re-authenticate your Google account."
                    .to_string(),
            ),
            Self::UpstreamApi { status_code, .. } if *status_code == 404 => {
                (StatusCode::NOT_FOUND, "not_found_error", msg)
            }
            _ => {
                if msg.contains("All endpoints failed") {
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        "api_error",
                        "Unable to connect to Google Cloud Code API.".to_string(),
                    )
                } else if msg.contains("RESOURCE_EXHAUSTED")
                    || msg.contains("QUOTA_EXHAUSTED")
                    || msg.contains("Rate limited on ")
                {
                    (StatusCode::BAD_REQUEST, "invalid_request_error", msg)
                } else {
                    (StatusCode::INTERNAL_SERVER_ERROR, "api_error", msg)
                }
            }
        };

        (
            status,
            json!({
                "type": "error",
                "error": {
                    "type": error_type,
                    "message": error_message
                }
            }),
        )
    }
}
