use crate::account::types::{Account, AccountsFile};
use crate::logger::logger;
use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;
use tokio::fs;
use tokio::sync::Mutex;

static WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn write_lock() -> &'static Mutex<()> {
    WRITE_LOCK.get_or_init(|| Mutex::new(()))
}

pub async fn load_accounts(config_path: &Path) -> AccountsFile {
    match fs::read_to_string(config_path).await {
        Ok(content) => match serde_json::from_str::<AccountsFile>(&content) {
            Ok(mut parsed) => {
                for acc in &mut parsed.accounts {
                    if acc.verify_url.is_none() {
                        acc.is_invalid = false;
                        acc.invalid_reason = None;
                    }
                }
                if parsed.active_index >= parsed.accounts.len() {
                    parsed.active_index = 0;
                }
                logger().info(format!(
                    "[AccountManager] Loaded {} account(s) from config",
                    parsed.accounts.len()
                ));
                parsed
            }
            Err(err) => {
                logger().error(format!("[AccountManager] Failed to parse accounts config: {err}"));
                AccountsFile::default()
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if let Some(env_raw) = std::env::var("NOVOID_ACCOUNTS_JSON")
                .ok()
                .or_else(|| std::env::var("ACCOUNTS_JSON").ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
            {
                let parsed_opt = serde_json::from_str::<AccountsFile>(&env_raw)
                    .ok()
                    .or_else(|| {
                        serde_json::from_str::<Vec<Account>>(&env_raw)
                            .ok()
                            .map(|accounts| AccountsFile {
                                accounts,
                                settings: HashMap::new(),
                                active_index: 0,
                            })
                    });
                if let Some(mut seeded) = parsed_opt {
                    for acc in &mut seeded.accounts {
                        if acc.verify_url.is_none() {
                            acc.is_invalid = false;
                            acc.invalid_reason = None;
                        }
                    }
                    if seeded.active_index >= seeded.accounts.len() {
                        seeded.active_index = 0;
                    }
                    save_accounts(
                        config_path,
                        &seeded.accounts,
                        &seeded.settings,
                        seeded.active_index,
                    )
                    .await;
                    logger().info(format!(
                        "[AccountManager] Seeded {} account(s) from environment into {}",
                        seeded.accounts.len(),
                        config_path.display()
                    ));
                    return seeded;
                }
            }
            logger().info(format!(
                "[AccountManager] No accounts file found at {}. Add accounts via WebUI or CLI.",
                config_path.display()
            ));
            AccountsFile::default()
        }
        Err(err) => {
            logger().error(format!("[AccountManager] Failed to load config: {err}"));
            AccountsFile::default()
        }
    }
}

pub async fn save_accounts(
    config_path: &Path,
    accounts: &[Account],
    settings: &HashMap<String, serde_json::Value>,
    active_index: usize,
) {
    let _guard = write_lock().lock().await;

    if let Some(parent) = config_path.parent() {
        let _ = fs::create_dir_all(parent).await;
    }

    let clean_accounts: Vec<Account> = accounts
        .iter()
        .map(|acc| {
            let mut copy = acc.clone();
            if copy.source != "oauth" {
                copy.refresh_token = None;
            }
            if copy.source != "manual" {
                copy.api_key = None;
            }
            copy
        })
        .collect();

    let payload = AccountsFile {
        accounts: clean_accounts,
        settings: settings.clone(),
        active_index,
    };

    match serde_json::to_string_pretty(&payload) {
        Ok(json_str) => {
            let tmp = config_path.with_extension("json.tmp");
            if let Err(err) = fs::write(&tmp, &json_str).await {
                logger().error(format!("[AccountManager] Failed to write temp accounts: {err}"));
                return;
            }
            if let Err(err) = fs::rename(&tmp, config_path).await {
                logger().error(format!("[AccountManager] Failed to rename accounts file: {err}"));
            }
        }
        Err(err) => {
            logger().error(format!("[AccountManager] Failed to serialize accounts: {err}"));
        }
    }
}
