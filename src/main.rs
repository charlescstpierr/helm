//! Helm: a self-hosted kanban that will orchestrate coding agents. See `docs/architecture.md`.

// Consumed by the supervisor, which arrives later in the same stack.
#[allow(dead_code)]
mod adapter;
mod agent;
mod assets;
mod config;
mod db;
mod mentions;
mod routes;
// Consumed by the supervisor, which arrives later in the same stack.
#[allow(dead_code)]
mod runs;
mod store;

use std::error::Error;
use std::process::ExitCode;

use config::Config;
use db::Db;

const USAGE: &str = "\
Usage: helm [--help | --version]

Configuration comes from `helm.toml` (or the file named by HELM_CONFIG) and from the
HELM_BIND and HELM_DB environment variables. See README.md.";

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        None => {}
        Some("--version" | "-V") => {
            println!("helm {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Some("--help" | "-h") => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Some(other) => {
            eprintln!("helm: unknown argument `{other}`\n{USAGE}");
            return ExitCode::FAILURE;
        }
    }
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("helm: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let config = Config::load()?;
    let db = Db::open(&config.db_path)?;
    let runtime = build_runtime()?;
    let served = runtime.block_on(serve(&config, db.clone()));
    db.checkpoint();
    served
}

/// A deliberately small runtime: a single thread, and a blocking pool no larger than the
/// single SQLite connection can use.
fn build_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(2)
        .enable_all()
        .build()
}

async fn serve(config: &Config, db: Db) -> Result<(), Box<dyn Error>> {
    let agents = routes::AgentsView {
        gate: config.run_gate(),
        default_model: config.agents.claude.model.clone(),
    };
    let state = routes::AppState::new(db, config.bind.ip().is_loopback(), agents);
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .map_err(|e| format!("cannot listen on {}: {e}", config.bind))?;
    eprintln!(
        "helm: listening on http://{} (database: {})",
        listener.local_addr()?,
        config.db_path.display()
    );
    if !config.bind.ip().is_loopback() {
        eprintln!("helm: warning: not bound to loopback and there is no authentication");
    }
    match config.run_gate().notice() {
        Some(notice) => eprintln!("helm: agents disabled: {notice}"),
        None => eprintln!("helm: agents enabled"),
    }
    // Open SSE streams never finish on their own, so stop serving as soon as a signal
    // arrives instead of waiting for connections to drain.
    tokio::select! {
        result = axum::serve(listener, routes::router(state)) => result?,
        () = shutdown_signal() => eprintln!("helm: shutting down"),
    }
    Ok(())
}

async fn shutdown_signal() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
}
