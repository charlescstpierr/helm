//! Helm: a self-hosted kanban that will orchestrate coding agents. See `docs/architecture.md`.

mod adapter;
mod agent;
mod assets;
mod changes;
mod config;
mod db;
mod git;
mod mentions;
mod prompt;
mod routes;
mod runs;
mod store;
mod supervisor;

use std::error::Error;
use std::process::ExitCode;
use std::sync::Arc;

use adapter::ClaudeAdapter;
use config::{Config, RunGate};
use db::Db;
use supervisor::{Orchestrator, RunDefaults, Supervisor, SupervisorConfig};

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
    let gate = config.run_gate();
    let claude = &config.agents.claude;
    let orchestrator = Orchestrator::new(
        gate,
        RunDefaults {
            permission_mode: claude.permission_mode,
            model: claude.model.clone(),
        },
    );
    let state = routes::AppState::new(
        db.clone(),
        config.bind.ip().is_loopback(),
        orchestrator.clone(),
    );
    let changes = state.changes();

    // A previous Helm may have died mid-run, whatever the gate says today.
    let interrupted = supervisor::recover(&db, &changes).await?;
    if interrupted > 0 {
        eprintln!("helm: {interrupted} run(s) left running by a previous Helm marked interrupted");
    }

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
    let stopper = orchestrator.clone();
    let _supervisor = match (gate, &config.project) {
        (RunGate::Open, Some(project)) => {
            git::check_repository(&project.repo).await?;
            eprintln!(
                "helm: agents enabled (repo {}, worktrees in {}, at most {} at once)",
                project.repo.display(),
                project.worktree_root.display(),
                config.agents.max_concurrent
            );
            let supervisor = Supervisor::new(
                db,
                orchestrator,
                Arc::new(ClaudeAdapter {
                    command: claude.command.clone(),
                }),
                changes,
                SupervisorConfig {
                    repo: project.repo.clone(),
                    worktree_root: project.worktree_root.clone(),
                    max_concurrent: config.agents.max_concurrent,
                    run_timeout: config.agents.run_timeout,
                },
            );
            Some(tokio::spawn(Arc::new(supervisor).run()))
        }
        _ => {
            if let Some(notice) = gate.notice() {
                eprintln!("helm: agents disabled: {notice}");
            }
            None
        }
    };
    // Open SSE streams never finish on their own, so stop serving as soon as a signal
    // arrives instead of waiting for connections to drain.
    tokio::select! {
        result = axum::serve(listener, routes::router(state)) => result?,
        () = shutdown_signal() => eprintln!("helm: shutting down"),
    }
    stopper.shutdown().await;
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
