use std::path::PathBuf;

use clap::Parser;
use tracing::error;

/// Command-line interface for the Symphony daemon.
#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Run the Symphony issue orchestration service."
)]
struct Cli {
    /// Optional path to the workflow contract file.
    workflow_path: Option<PathBuf>,
    /// Optional port for the loopback HTTP dashboard and API.
    #[arg(long)]
    port: Option<u16>,
}

/// Boot the async runtime, initialize logging, and run the service.
#[tokio::main]
async fn main() {
    init_tracing();

    let cli = Cli::parse();
    let workflow_path = cli
        .workflow_path
        .unwrap_or_else(|| PathBuf::from("./WORKFLOW.md"));

    if let Err(error) = symphony::orchestrator::run_service(workflow_path, cli.port).await {
        error!(error = %error, "service startup failed");
        std::process::exit(1);
    }
}

/// Initialize JSON logs so operators can inspect the daemon without a debugger.
fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,symphony=debug".into()),
        )
        .json()
        .with_current_span(false)
        .with_span_list(false)
        .init();
}
