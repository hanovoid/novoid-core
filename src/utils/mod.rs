pub mod claude_config;
pub mod server_presets;
pub mod usage_stats;

use crate::config::get_config;
use rand::Rng;
use reqwest::Client;
use std::sync::OnceLock;
use std::time::Duration;

static HTTP_CLIENT: OnceLock<Client> = OnceLock::new();

pub fn http_client() -> &'static Client {
    HTTP_CLIENT.get_or_init(|| {
        let mut builder = Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .tcp_keepalive(Duration::from_secs(30))
            .tcp_nodelay(true)
            .http2_adaptive_window(true)
            .http2_keep_alive_interval(Some(Duration::from_secs(20)))
            .http2_keep_alive_timeout(Duration::from_secs(10))
            .http2_keep_alive_while_idle(true)
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(16);

        // reqwest automatically reads HTTP_PROXY/HTTPS_PROXY/ALL_PROXY unless disabled,
        // and we keep explicit support if lowercase/uppercase is present.
        if let Ok(proxy_url) = std::env::var("HTTP_PROXY")
            .or_else(|_| std::env::var("http_proxy"))
            .or_else(|_| std::env::var("HTTPS_PROXY"))
            .or_else(|_| std::env::var("https_proxy"))
        {
            if !proxy_url.trim().is_empty() {
                if let Ok(proxy) = reqwest::Proxy::all(&proxy_url) {
                    builder = builder.proxy(proxy);
                }
            }
        }

        builder.build().unwrap_or_else(|_| Client::new())
    })
}

pub async fn maybe_throttle() {
    let cfg = get_config();
    if cfg.request_throttling_enabled && cfg.request_delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(cfg.request_delay_ms)).await;
    }
}

pub fn format_duration(ms: u64) -> String {
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let seconds = ms / 1000;
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let secs = seconds % 60;

    if hours > 0 {
        format!("{hours}h{minutes}m{secs}s")
    } else if minutes > 0 {
        format!("{minutes}m{secs}s")
    } else {
        format!("{secs}s")
    }
}

pub fn generate_jitter_ms(max_jitter_ms: u64) -> i64 {
    let half = (max_jitter_ms / 2) as i64;
    let mut rng = rand::thread_rng();
    rng.gen_range(-half..=half)
}

pub fn is_network_error(err: &reqwest::Error) -> bool {
    if err.is_connect() || err.is_timeout() || err.is_request() {
        return true;
    }
    let msg = err.to_string().to_lowercase();
    msg.contains("connection reset")
        || msg.contains("timed out")
        || msg.contains("broken pipe")
        || msg.contains("eof")
        || msg.contains("dns")
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
