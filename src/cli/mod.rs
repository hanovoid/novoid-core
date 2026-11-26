use crate::account::oauth::{
    complete_oauth_flow, extract_code_from_input, get_authorization_url, get_user_email,
    refresh_access_token, start_callback_server,
};
use crate::account::storage::{load_accounts, save_accounts};
use crate::account::types::Account;
use crate::account::AccountManager;
use crate::cloudcode::rate_limit_state;
use crate::config::{get_config, set_debug_flags};
use crate::constants::{
    account_config_path, pid_file_path, DEFAULT_PORT, SELECTION_STRATEGIES,
};
use crate::format::signature_cache;
use crate::logger::logger;
use crate::server::{build_router, webui::OAuthFlowStore, AppState};
use crate::utils::{now_ms, usage_stats};
use clap::{Parser, Subcommand};
use std::collections::HashMap;
use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::RwLock;

#[derive(Parser)]
#[command(name = "novoid-core", version, about = "Novoid Core Proxy Server")]
pub struct Cli {
    /// Enable debug / developer mode
    #[arg(long, alias = "dev-mode", global = true)]
    pub debug: bool,

    /// Enable model fallback when quota exhausted
    #[arg(long, global = true)]
    pub fallback: bool,

    /// Account selection strategy (hybrid, sticky, round-robin)
    #[arg(long, global = true)]
    pub strategy: Option<String>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Launch proxy as background service (or foreground with --log)
    Start {
        #[arg(long)]
        log: bool,
    },
    /// Shut down the background proxy
    Stop,
    /// Relaunch the background proxy
    Restart {
        #[arg(long)]
        log: bool,
    },
    /// View proxy status and details
    Status,
    /// Open WebUI dashboard in browser
    Ui,
    /// Manage Google accounts
    Accounts {
        #[command(subcommand)]
        action: Option<AccountAction>,
        #[arg(long, global = true)]
        no_browser: bool,
    },
    /// Run server directly in foreground (internal/npm start)
    Serve,
}

#[derive(Subcommand)]
pub enum AccountAction {
    /// Add Google account via OAuth
    Add {
        #[arg(long)]
        no_browser: bool,
    },
    /// List all linked accounts
    List,
    /// Remove an account interactively
    Remove,
    /// Verify account refresh tokens
    Verify,
    /// Clear all accounts
    Clear,
}

fn get_port() -> u16 {
    std::env::var("PORT")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .or_else(|| get_config().port)
        .unwrap_or(DEFAULT_PORT)
}

fn get_host() -> String {
    std::env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string())
}

fn read_pid() -> Option<u32> {
    let path = pid_file_path();
    let content = std::fs::read_to_string(path).ok()?;
    content.trim().parse::<u32>().ok()
}

fn is_process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
            || Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

fn is_service_running() -> bool {
    let Some(pid) = read_pid() else {
        return false;
    };
    if is_process_alive(pid) {
        true
    } else {
        cleanup_pid();
        false
    }
}

fn save_pid(pid: u32) {
    let path = pid_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, pid.to_string());
}

fn cleanup_pid() {
    let _ = std::fs::remove_file(pid_file_path());
}

fn send_sigterm(pid: u32) -> bool {
    #[cfg(unix)]
    {
        Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = Command::new("cmd").args(["/c", "start", "", url]).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = Command::new("xdg-open")
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

pub async fn run_cli() -> Result<(), String> {
    let cli = Cli::parse();

    let is_debug = cli.debug
        || std::env::var("DEBUG").as_deref() == Ok("true")
        || std::env::var("DEV_MODE").as_deref() == Ok("true");
    let is_fallback = cli.fallback || std::env::var("FALLBACK").as_deref() == Ok("true");

    let strategy_override = cli.strategy.as_deref().and_then(|s| {
        let lower = s.to_lowercase();
        if SELECTION_STRATEGIES.contains(&lower.as_str()) {
            Some(lower)
        } else {
            logger().warn(format!(
                "[Startup] Invalid strategy \"{s}\". Valid options: {}. Using default.",
                SELECTION_STRATEGIES.join(", ")
            ));
            None
        }
    });

    match cli.command {
        None | Some(Commands::Serve) => {
            run_server(is_debug, is_fallback, strategy_override).await
        }
        Some(Commands::Start { log }) => {
            if log {
                run_server(is_debug, is_fallback, strategy_override).await
            } else {
                start_background(is_debug, is_fallback, strategy_override).await
            }
        }
        Some(Commands::Stop) => {
            stop_server();
            Ok(())
        }
        Some(Commands::Restart { log }) => {
            if is_service_running() {
                if let Some(pid) = read_pid() {
                    send_sigterm(pid);
                    cleanup_pid();
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
            if log {
                run_server(is_debug, is_fallback, strategy_override).await
            } else {
                start_background(is_debug, is_fallback, strategy_override).await
            }
        }
        Some(Commands::Status) => {
            show_status();
            Ok(())
        }
        Some(Commands::Ui) => {
            if !is_service_running() {
                start_background(is_debug, is_fallback, strategy_override).await?;
                tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            }
            let url = format!("http://localhost:{}/", get_port());
            println!("\n🖥️  Opening dashboard → {url}\n");
            open_browser(&url);
            Ok(())
        }
        Some(Commands::Accounts { action, no_browser }) => {
            run_accounts_cli(action, no_browser).await
        }
    }
}

async fn start_background(
    is_debug: bool,
    is_fallback: bool,
    strategy_override: Option<String>,
) -> Result<(), String> {
    if is_service_running() {
        let pid = read_pid().unwrap_or(0);
        let port = get_port();
        println!("\n╭────────────────────────────────────────────────╮");
        println!("│  ⚡ Novoid Core is already in orbit             │");
        println!("╰────────────────────────────────────────────────╯\n");
        println!("   ┌─ PID: {pid}");
        println!("   ├─ Local: http://localhost:{port}");
        println!("   └─ Dashboard: http://localhost:{port}\n");
        return Ok(());
    }

    let exe = std::env::current_exe().map_err(|e| format!("Failed to find current exe: {e}"))?;
    let mut cmd = Command::new(exe);
    cmd.arg("serve");
    if is_debug {
        cmd.arg("--debug");
    }
    if is_fallback {
        cmd.arg("--fallback");
    }
    if let Some(strat) = strategy_override {
        cmd.arg("--strategy").arg(strat);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn background server: {e}"))?;
    save_pid(child.id());

    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    let port = get_port();
    println!("╭────────────────────────────────────────────────╮");
    println!("│ ⚡ Proxy is now in orbit!                      │");
    println!("╰────────────────────────────────────────────────╯\n");
    println!("   ┌─ Process ID: {}", child.id());
    println!("   ├─ Local: http://localhost:{port}");
    println!("   └─ Dashboard: http://localhost:{port}/\n");
    Ok(())
}

fn stop_server() {
    if !is_service_running() {
        println!("\n🌑 Proxy is not running\n");
        cleanup_pid();
        return;
    }
    if let Some(pid) = read_pid() {
        if send_sigterm(pid) {
            cleanup_pid();
            println!("\n🌙 Proxy has been taken offline\n");
        } else {
            cleanup_pid();
            eprintln!("\n⚠️  Shutdown failed for PID {pid}\n");
        }
    }
}

fn show_status() {
    println!("\n╭────────────────────────────────────────────────╮");
    println!("│  ⚡ Novoid Core                                 │");
    println!("╰────────────────────────────────────────────────╯\n");
    if is_service_running() {
        let pid = read_pid().unwrap_or(0);
        let port = get_port();
        println!("  STATUS\n  ⚡ Proxy is active\n");
        println!("  DETAILS");
        println!("  ├─ PID: {pid}");
        println!("  ├─ Port: {port}");
        println!("  ├─ API: http://localhost:{port}");
        println!("  └─ Dashboard: http://localhost:{port}/\n");
    } else {
        println!("  STATUS\n  🌑 Proxy is offline\n");
        println!("  TO LAUNCH\n  • novoid-core start\n");
    }
}

async fn run_server(
    is_debug: bool,
    is_fallback: bool,
    strategy_override: Option<String>,
) -> Result<(), String> {
    logger().set_debug(is_debug);
    if is_debug {
        set_debug_flags(true, true);
        logger().debug("Developer mode enabled");
    }
    if is_fallback {
        logger().info("Model fallback mode enabled");
    }

    signature_cache::init_background_flush();
    usage_stats::init_background_flush();
    rate_limit_state::init_background_cleanup();

    let account_manager = Arc::new(AccountManager::new(
        account_config_path(),
        strategy_override,
    ));
    account_manager.ensure_initialized().await;

    let port = get_port();
    let host = get_host();
    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("Failed to bind {addr}: {e}"))?;

    let state = AppState {
        account_manager: Arc::clone(&account_manager),
        fallback_enabled: is_fallback,
        oauth_flows: Arc::new(RwLock::new(OAuthFlowStore::default())),
        port,
    };
    let app = build_router(state);

    let strategy_label = account_manager.get_strategy_label().await;
    let ver = env!("CARGO_PKG_VERSION");
    println!("\n╔══════════════════════════════════════════════════════════════╗");
    println!("║{:^62}║", format!("Novoid Core Server v{ver} (Rust)"));
    println!("╠══════════════════════════════════════════════════════════════╣");
    println!(
        "║  {:<58}  ║",
        format!(
            "Server & WebUI: http://{}:{port}",
            if host == "0.0.0.0" { "localhost" } else { &host }
        )
    );
    println!("║  {:<58}  ║", format!("Bound to: {addr}"));
    println!("║  {:<58}  ║", format!("Strategy: {strategy_label}"));
    println!(
        "║  {:<58}  ║",
        format!("Fallback: {}", if is_fallback { "enabled" } else { "disabled" })
    );
    println!(
        "║  {:<58}  ║",
        format!("Storage: {}", crate::config::config_dir().display())
    );
    println!("╚══════════════════════════════════════════════════════════════╝\n");
    logger().success(format!("Server started successfully on port {port}"));

    let server = axum::serve(listener, app).with_graceful_shutdown(async {
        shutdown_signal().await;
        crate::server::trigger_shutdown();
    });

    let mut shutdown_watch = crate::server::subscribe_shutdown();
    tokio::select! {
        res = server => {
            res.map_err(|e| format!("Server error: {e}"))?;
        }
        _ = async {
            while !*shutdown_watch.borrow() {
                if shutdown_watch.changed().await.is_err() {
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        } => {
            logger().warn("Force closing remaining idle HTTP keep-alive connections");
        }
    }

    signature_cache::flush_signature_store_sync();
    usage_stats::flush();
    logger().success("Server stopped cleanly");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    logger().info("Shutting down server...");
}

fn prompt_line(prompt: &str) -> String {
    print!("{prompt}");
    let _ = io::stdout().flush();
    let mut input = String::new();
    let _ = io::stdin().read_line(&mut input);
    input.trim().to_string()
}

async fn ensure_server_stopped_for_cli() {
    let port = get_port();
    let addr = format!("127.0.0.1:{port}");
    if tokio::time::timeout(
        std::time::Duration::from_millis(500),
        TcpStream::connect(&addr),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false)
    {
        eprintln!("\n\x1b[31mError: Novoid Core server is currently running on port {port}.\x1b[0m\nPlease stop the server before modifying accounts via CLI.\n");
        std::process::exit(1);
    }
}

async fn run_accounts_cli(
    action: Option<AccountAction>,
    global_no_browser: bool,
) -> Result<(), String> {
    println!("╔════════════════════════════════════════╗");
    println!("║      Novoid Core Account Manager       ║");
    println!("║   Use --no-browser for headless mode   ║");
    println!("╚════════════════════════════════════════╝");

    let cfg_path = account_config_path();
    let mut loaded = load_accounts(&cfg_path).await;

    match action {
        Some(AccountAction::List) => {
            if loaded.accounts.is_empty() {
                println!("\nNo accounts configured.");
            } else {
                println!("\n{} account(s) saved:", loaded.accounts.len());
                let now = now_ms();
                for (i, acc) in loaded.accounts.iter().enumerate() {
                    let limited = acc
                        .model_rate_limits
                        .values()
                        .any(|l| l.is_rate_limited && l.reset_time.unwrap_or(0) > now);
                    println!(
                        "  {}. {}{}",
                        i + 1,
                        acc.email,
                        if limited { " (rate-limited)" } else { "" }
                    );
                }
                println!("\nConfig file: {}", cfg_path.display());
            }
        }
        Some(AccountAction::Verify) => {
            if loaded.accounts.is_empty() {
                println!("\nNo accounts to verify.");
                return Ok(());
            }
            println!("\nVerifying accounts...\n");
            for acc in &loaded.accounts {
                let Some(ref rt) = acc.refresh_token else {
                    println!("  ✗ {} - missing refreshToken", acc.email);
                    continue;
                };
                match refresh_access_token(rt).await {
                    Ok(tok) => match get_user_email(&tok).await {
                        Ok(email) => println!("  ✓ {email} - OK"),
                        Err(e) => println!("  ✗ {} - {e}", acc.email),
                    },
                    Err((e, _)) => println!("  ✗ {} - {e}", acc.email),
                }
            }
        }
        Some(AccountAction::Clear) => {
            ensure_server_stopped_for_cli().await;
            let confirm = prompt_line("\nAre you sure you want to remove all accounts? [y/N]: ");
            if confirm.eq_ignore_ascii_case("y") {
                save_accounts(&cfg_path, &[], &HashMap::new(), 0).await;
                println!("All accounts removed.");
            } else {
                println!("Cancelled.");
            }
        }
        Some(AccountAction::Remove) => {
            ensure_server_stopped_for_cli().await;
            if loaded.accounts.is_empty() {
                println!("\nNo accounts to remove.");
                return Ok(());
            }
            for (i, acc) in loaded.accounts.iter().enumerate() {
                println!("  {}. {}", i + 1, acc.email);
            }
            let sel = prompt_line("\nEnter account number to remove (or 0 to cancel): ");
            if let Ok(idx) = sel.parse::<usize>() {
                if idx > 0 && idx <= loaded.accounts.len() {
                    let removed = loaded.accounts.remove(idx - 1);
                    save_accounts(&cfg_path, &loaded.accounts, &loaded.settings, 0).await;
                    println!("Removed {}", removed.email);
                }
            }
        }
        act @ (Some(AccountAction::Add { .. }) | None) => {
            let add_no_browser = matches!(act, Some(AccountAction::Add { no_browser: true }));
            let nb = add_no_browser || global_no_browser;
            ensure_server_stopped_for_cli().await;
            let auth = get_authorization_url(None);
            let completed = if nb {
                println!("\nOpen this URL in a browser:\n\n  {}\n", auth.url);
                let input = prompt_line("Paste the callback URL or authorization code: ");
                let (code, _) = extract_code_from_input(&input)?;
                complete_oauth_flow(&code, &auth.verifier, None).await?
            } else {
                println!("\nOpening browser for Google sign-in...\n  {}\n", auth.url);
                open_browser(&auth.url);
                let handle = start_callback_server(auth.state.clone(), 120_000).await?;
                let code = handle
                    .code_rx
                    .await
                    .map_err(|_| "OAuth channel closed".to_string())??;
                complete_oauth_flow(&code, &auth.verifier, None).await?
            };

            if let Some(existing) = loaded
                .accounts
                .iter_mut()
                .find(|a| a.email == completed.email)
            {
                existing.refresh_token = Some(completed.refresh_token);
                existing.project_id = completed.project_id;
                existing.added_at = Some(chrono::Utc::now().to_rfc3339());
                println!("Updated tokens for {}", completed.email);
            } else {
                let mut acc = Account::default();
                acc.email = completed.email.clone();
                acc.source = "oauth".to_string();
                acc.refresh_token = Some(completed.refresh_token);
                acc.project_id = completed.project_id;
                acc.added_at = Some(chrono::Utc::now().to_rfc3339());
                loaded.accounts.push(acc);
                println!("Added account {}", completed.email);
            }
            save_accounts(&cfg_path, &loaded.accounts, &loaded.settings, 0).await;
        }
    }
    Ok(())
}
