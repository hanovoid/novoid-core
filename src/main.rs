#[tokio::main]
async fn main() {
    if let Err(err) = novoid_core::cli::run_cli().await {
        eprintln!("Error: {err}");
        std::process::exit(1);
    }
}
