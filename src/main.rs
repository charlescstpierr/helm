//! Helm: a self-hosted kanban that will orchestrate coding agents. See `docs/architecture.md`.

mod adapter;
mod agent;
mod assets;
mod changes;
mod checks;
mod config;
mod db;
mod delivery;
mod git;
mod github;
mod mentions;
mod process;
mod prompt;
mod resume;
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
    let delivery = delivery::Delivery::new(
        db.clone(),
        changes.clone(),
        gate,
        config.project.as_ref().map(|project| project.repo.clone()),
        config.github.clone(),
    );
    let state = state.with_delivery(delivery.clone());

    // Bind before recovery: a second instance on the same address must not interrupt
    // the runs still supervised by the instance that already owns the listener.
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .map_err(|e| format!("cannot listen on {}: {e}", config.bind))?;

    // A previous Helm may have died mid-run, whatever the gate says today.
    let interrupted = supervisor::recover(&db, &changes).await?;
    if interrupted > 0 {
        eprintln!("helm: {interrupted} run(s) left running by a previous Helm marked interrupted");
    }

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
                    checks: config.checks.clone(),
                },
            )
            .with_delivery(delivery.clone());
            Some(tokio::spawn(Arc::new(supervisor).run()))
        }
        _ => {
            if let Some(notice) = gate.notice() {
                eprintln!("helm: agents disabled: {notice}");
            }
            None
        }
    };
    let delivery_poll = tokio::spawn(delivery.poll());
    // Open SSE streams never finish on their own, so stop serving as soon as a signal
    // arrives instead of waiting for connections to drain.
    tokio::select! {
        result = axum::serve(listener, routes::router(state)) => result?,
        () = shutdown_signal() => eprintln!("helm: shutting down"),
    }
    delivery_poll.abort();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Agent, PermissionMode};
    use crate::runs::{NewRun, RunStatus};
    use crate::store::{CardInput, StoreError};

    #[tokio::test]
    async fn a_failed_bind_leaves_running_agents_and_their_threads_untouched() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = Config::resolve(None, |_| None).unwrap();
        config.bind = listener.local_addr().unwrap();
        let db = Db::open_in_memory().unwrap();
        let (card_id, run_id) = db
            .call(|conn| {
                let card_id = store::create_card(
                    conn,
                    2,
                    &CardInput {
                        title: "An agent is already working".to_owned(),
                        ..CardInput::default()
                    },
                )?;
                let run_id = runs::enqueue(
                    conn,
                    &NewRun {
                        card_id,
                        agent: Agent::Claude,
                        model: None,
                        permission_mode: PermissionMode::DEFAULT,
                        prompt: "Keep working",
                    },
                )?
                .unwrap();
                runs::claim_next_queued(conn)?;
                Ok::<_, StoreError>((card_id, run_id))
            })
            .await
            .unwrap();

        let error = serve(&config, db.clone()).await.unwrap_err();
        assert!(error.to_string().contains("cannot listen"), "{error}");
        db.call(move |conn| {
            let run = runs::get_run(conn, run_id)?;
            assert_eq!(run.status, RunStatus::Running);
            assert!(run.finished_at.is_none());
            assert!(store::list_comments(conn, card_id)?.is_empty());
            Ok::<_, StoreError>(())
        })
        .await
        .unwrap();
    }
}
