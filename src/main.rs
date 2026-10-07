//! Helm: a self-hosted kanban that will orchestrate coding agents. See `docs/architecture.md`.

mod assets;
mod config;
mod db;
mod routes;
mod store;

use std::error::Error;
use std::process::ExitCode;

use config::Config;
use db::Db;

const USAGE: &str = "\
Usage: helm [--help | --version]

Configuration comes from `helm.toml` (or the file named by HELM_CONFIG) and from the
HELM_BIND, HELM_DB and HELM_WORKER_THREADS environment variables. See README.md.";

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
    let runtime = build_runtime(config.worker_threads)?;
    let served = runtime.block_on(serve(&config, db.clone()));
    db.checkpoint();
    served
}

/// A deliberately small runtime: one worker by default, and a blocking pool no larger than
/// the single SQLite connection can use.
fn build_runtime(worker_threads: usize) -> std::io::Result<tokio::runtime::Runtime> {
    let mut builder = if worker_threads <= 1 {
        tokio::runtime::Builder::new_current_thread()
    } else {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.worker_threads(worker_threads);
        builder
    };
    builder.max_blocking_threads(2).enable_all().build()
}

async fn serve(config: &Config, db: Db) -> Result<(), Box<dyn Error>> {
    let state = routes::AppState::new(db, config.bind.ip().is_loopback());
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
