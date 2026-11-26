use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, RwLock};
use tokio::sync::broadcast;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub timestamp: String,
    pub level: String,
    pub message: String,
}

const MAX_HISTORY: usize = 1000;

pub struct AppLogger {
    debug_enabled: AtomicBool,
    history: RwLock<VecDeque<LogEntry>>,
    tx: broadcast::Sender<LogEntry>,
}

impl AppLogger {
    fn new() -> Self {
        let (tx, _) = broadcast::channel(512);
        Self {
            debug_enabled: AtomicBool::new(false),
            history: RwLock::new(VecDeque::with_capacity(MAX_HISTORY)),
            tx,
        }
    }

    pub fn set_debug(&self, enabled: bool) {
        self.debug_enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn is_debug_enabled(&self) -> bool {
        self.debug_enabled.load(Ordering::Relaxed)
    }

    pub fn get_history(&self) -> Vec<LogEntry> {
        self.history
            .read()
            .map(|h| h.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LogEntry> {
        self.tx.subscribe()
    }

    fn print(&self, level: &str, color: &str, message: String) {
        let timestamp = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        println!("\x1b[90m[{timestamp}]\x1b[0m {color}[{level}]\x1b[0m {message}");

        let entry = LogEntry {
            timestamp,
            level: level.to_string(),
            message,
        };

        if let Ok(mut guard) = self.history.write() {
            if guard.len() >= MAX_HISTORY {
                guard.pop_front();
            }
            guard.push_back(entry.clone());
        }

        let _ = self.tx.send(entry);
    }

    pub fn info(&self, msg: impl Into<String>) {
        self.print("INFO", "\x1b[34m", msg.into());
    }

    pub fn success(&self, msg: impl Into<String>) {
        self.print("SUCCESS", "\x1b[32m", msg.into());
    }

    pub fn warn(&self, msg: impl Into<String>) {
        self.print("WARN", "\x1b[33m", msg.into());
    }

    pub fn error(&self, msg: impl Into<String>) {
        self.print("ERROR", "\x1b[31m", msg.into());
    }

    pub fn debug(&self, msg: impl Into<String>) {
        if self.is_debug_enabled() {
            self.print("DEBUG", "\x1b[35m", msg.into());
        }
    }
}

static LOGGER: OnceLock<AppLogger> = OnceLock::new();

pub fn logger() -> &'static AppLogger {
    LOGGER.get_or_init(AppLogger::new)
}
