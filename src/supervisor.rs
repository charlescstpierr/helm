//! Runs agents: turns a card entering a `todo` column into a queued run, and a queued run
//! into a supervised process in the card's worktree.
//!
//! Two halves share a registry of cancel handles. [`Orchestrator`] is what the HTTP handlers
//! hold: it decides whether a card change queues a run and cancels runs. [`Supervisor`] is the
//! background task that claims queued runs and drives each to a final state.

#[cfg(not(unix))]
compile_error!("the orchestrator launches agents in process groups and needs a Unix platform");

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::Connection;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Notify, Semaphore};

use crate::adapter::{self, AgentAdapter, Finish, Launch, ParsedLine, Verdict};
use crate::agent::{Agent, ModelName, PermissionMode};
use crate::changes::Changes;
use crate::checks::{self, VerificationStatus};
use crate::config::ChecksConfig;
use crate::config::RunGate;
use crate::db::Db;
use crate::delivery::{Delivery, OperationLocks};
use crate::git;
use crate::prompt;
use crate::runs::{self, EventKind, NewEvent, NewRun, Outcome, Run, RunId, RunStatus};
use crate::store::{self, Author, Category, StoreError};

/// How often a busy run tells the open browsers it has news.
const PUBLISH_EVERY: Duration = Duration::from_millis(250);
const TERMINATE_GRACE: Duration = Duration::from_secs(3);
const MAX_LINE_BYTES: usize = 1024 * 1024;
const SHUTDOWN_WAIT: Duration = Duration::from_secs(10);
const STDERR_DRAIN: Duration = Duration::from_secs(2);
const STDERR_KEPT_BYTES: usize = 256 * 1024;

/// What a run is launched with when the card does not say otherwise.
#[derive(Debug, Clone)]
pub struct RunDefaults {
    pub permission_mode: PermissionMode,
    pub model: Option<ModelName>,
}

struct Shared {
    gate: RunGate,
    claude: RunDefaults,
    wake: Notify,
    /// One handle per running run, inserted when the run is claimed and removed when it ends.
    cancels: Mutex<HashMap<RunId, Arc<Notify>>>,
    stopping: AtomicBool,
    delivery_locks: Mutex<Option<OperationLocks>>,
}

/// The HTTP side: queues and cancels. Cheap to clone.
#[derive(Clone)]
pub struct Orchestrator {
    shared: Arc<Shared>,
}

impl Orchestrator {
    pub fn new(gate: RunGate, claude: RunDefaults) -> Self {
        Self {
            shared: Arc::new(Shared {
                gate,
                claude,
                wake: Notify::new(),
                cancels: Mutex::new(HashMap::new()),
                stopping: AtomicBool::new(false),
                delivery_locks: Mutex::new(None),
            }),
        }
    }

    pub fn gate(&self) -> RunGate {
        self.shared.gate
    }

    pub fn set_delivery_locks(&self, locks: OperationLocks) {
        *lock(&self.shared.delivery_locks) = Some(locks);
    }

    pub fn ensure_card_available(&self, card_id: i64) -> Result<(), StoreError> {
        if lock(&self.shared.delivery_locks)
            .as_ref()
            .is_some_and(|locks| locks.contains(card_id))
        {
            return Err(StoreError::Invalid(
                "Une opération GitHub est en cours sur cette carte. Réessayez dans un instant."
                    .to_owned(),
            ));
        }
        Ok(())
    }

    pub fn default_model(&self) -> Option<&ModelName> {
        self.shared.claude.model.as_ref()
    }

    fn defaults(&self, agent: Agent) -> &RunDefaults {
        match agent {
            Agent::Claude => &self.shared.claude,
        }
    }

    /// Call after a card was created, moved or edited. `entered_column` is whether it just
    /// entered a different column.
    ///
    /// Entering a `todo` column with an agent assigned queues a run, if the gate is open and
    /// the card's single run slot is free. Sitting in any other column, a card has no queued
    /// run: it is cancelled. Edits that leave the card in place change nothing, so a failed
    /// run is never retried by saving the form.
    pub fn after_placement(
        &self,
        conn: &mut Connection,
        card_id: i64,
        entered_column: bool,
    ) -> Result<(), StoreError> {
        self.ensure_card_available(card_id)?;
        if store::card_category(conn, card_id)? != Category::Todo {
            runs::cancel_queued(conn, card_id)?;
            return Ok(());
        }
        if !entered_column || self.shared.gate != RunGate::Open {
            return Ok(());
        }
        let card = store::get_card(conn, card_id)?;
        let Some(agent) = card.agent else {
            return Ok(());
        };
        let (key, number) = store::card_key(conn, card_id)?;
        let comments = store::list_comments(conn, card_id)?;
        let text = prompt::build(
            &format!("{key}-{number}"),
            &git::branch_name(&key, number),
            &card,
            &comments,
        );
        let defaults = self.defaults(agent);
        let model = card.model.as_ref().or(defaults.model.as_ref());
        let queued = runs::enqueue(
            conn,
            &NewRun {
                card_id,
                agent,
                model,
                permission_mode: defaults.permission_mode,
                prompt: &text,
            },
        )?;
        if queued.is_some() {
            self.shared.wake.notify_one();
        }
        Ok(())
    }

    /// Cancels a run and returns the id of its card. A queued run is cancelled at once; a
    /// running one is told to stop and its supervisor records the end. A finished run cannot
    /// be cancelled.
    pub async fn cancel(&self, db: &Db, id: RunId) -> Result<i64, StoreError> {
        let this = self.clone();
        db.call(move |conn| {
            let run = runs::get_run(conn, id)?;
            let handle = this.cancel_handle(id);
            match (run.status, handle) {
                (RunStatus::Running, Some(handle)) => handle.notify_one(),
                (RunStatus::Queued | RunStatus::Running, None) => {
                    runs::finish(conn, id, &Outcome::Cancelled)?;
                    comment(conn, run.card_id, format!("Exécution {id} annulée."))?;
                }
                (status, _) => {
                    return Err(StoreError::IllegalTransition {
                        from: status,
                        to: RunStatus::Cancelled,
                    });
                }
            }
            Ok(run.card_id)
        })
        .await
    }

    pub async fn shutdown(&self) {
        {
            let cancels = lock(&self.shared.cancels);
            self.shared.stopping.store(true, Ordering::SeqCst);
            for handle in cancels.values() {
                handle.notify_one();
            }
        }
        let deadline = tokio::time::Instant::now() + SHUTDOWN_WAIT;
        while !lock(&self.shared.cancels).is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn stopping(&self) -> bool {
        self.shared.stopping.load(Ordering::SeqCst)
    }

    fn cancel_handle(&self, id: RunId) -> Option<Arc<Notify>> {
        lock(&self.shared.cancels).get(&id).cloned()
    }

    /// Claims the oldest queued run and registers its cancel handle in the same step, so a
    /// cancel can never see a running run without one.
    fn claim_next(&self, conn: &mut Connection) -> Result<Option<Run>, StoreError> {
        if self.stopping() {
            return Ok(None);
        }
        let run = runs::claim_next_queued(conn)?;
        if let Some(run) = &run {
            let mut cancels = lock(&self.shared.cancels);
            let handle = Arc::new(Notify::new());
            if self.stopping() {
                handle.notify_one();
            }
            cancels.insert(run.id, handle);
        }
        Ok(run)
    }

    fn forget(&self, id: RunId) {
        lock(&self.shared.cancels).remove(&id);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn comment(conn: &mut Connection, card_id: i64, body: String) -> Result<(), StoreError> {
    store::add_comment(conn, card_id, &Author::helm(), &body).map(drop)
}

fn log_store_error(context: &str, error: &StoreError) {
    eprintln!("helm: {context}: {error}");
}

fn still_running_message(pid: i64) -> String {
    format!(
        "un groupe de processus {pid} existe encore et pourrait être l'agent d'une exécution précédente : Helm ne lance pas un second agent dans le même worktree. Vérifiez d'abord ce qu'il est (par exemple `pgrep -a -g {pid}`), car ce numéro a pu être réutilisé par un autre programme. Si c'est bien l'agent, arrêtez-le avec `kill -- -{pid}`, puis remettez la carte dans « À faire »."
    )
}

const STOPPED_BY_SHUTDOWN: &str = "Helm s'est arrêté pendant l'exécution.";

/// Marks every run left `running` by a previous Helm as `interrupted`. Nothing is relaunched:
/// whether to try again is the human's call. Returns how many runs were interrupted.
pub async fn recover(db: &Db, changes: &Changes) -> Result<usize, StoreError> {
    let count = db
        .call(|conn| {
            checks::interrupt_unfinished(conn)?;
            let orphans = runs::running_runs(conn)?;
            for orphan in &orphans {
                let still_alive = orphan.pid.is_some_and(group_exists);
                let reason = if still_alive {
                    "Helm s'est arrêté pendant l'exécution. Le processus de l'agent existe encore mais n'est plus supervisé."
                } else {
                    STOPPED_BY_SHUTDOWN
                };
                runs::finish(conn, orphan.id, &Outcome::Interrupted(reason.to_owned()))?;
                comment(
                    conn,
                    orphan.card_id,
                    format!("Exécution {} interrompue : {reason}", orphan.id),
                )?;
            }
            Ok::<_, StoreError>(orphans.len())
        })
        .await?;
    if count > 0 {
        changes.publish();
    }
    Ok(count)
}

fn group_exists(pid: i64) -> bool {
    // 0 and 1 would not name a group: `kill(-0)` and `kill(-1)` reach this process's group and
    // every process.
    let Some(pid) = i32::try_from(pid).ok().filter(|pid| *pid > 1) else {
        return false;
    };
    // SAFETY: signal 0 only checks that the group exists and can be signalled.
    let rc = unsafe { libc::kill(-pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Kills everything in a run's process group when dropped: the agent and the commands it
/// started. Dropping is how a cancelled, panicked or abandoned run cleans up.
struct ProcessGroup(i32);

impl ProcessGroup {
    fn of(child: &Child) -> Option<Self> {
        child.id().and_then(|pid| i32::try_from(pid).ok()).map(Self)
    }

    fn signal(&self, signal: i32) {
        // SAFETY: the group id is the pid of a child this process spawned as a group leader.
        unsafe {
            libc::kill(-self.0, signal);
        }
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.signal(libc::SIGKILL);
    }
}

/// Runs `future` unless the run is cancelled first; `None` means cancelled.
async fn unless_cancelled<T>(cancel: &Notify, future: impl Future<Output = T>) -> Option<T> {
    tokio::select! {
        biased;
        () = cancel.notified() => None,
        value = future => Some(value),
    }
}

enum Ended {
    Exited(Exit),
    Cancelled,
    TimedOut,
}

fn describe(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 60 && seconds % 60 == 0 {
        format!("{} min", seconds / 60)
    } else {
        format!("{seconds} s")
    }
}

#[derive(Default)]
struct Line {
    kept: Vec<u8>,
    dropped: usize,
}

impl Line {
    fn is_empty(&self) -> bool {
        self.kept.is_empty() && self.dropped == 0
    }

    fn clear(&mut self) {
        self.kept.clear();
        self.dropped = 0;
    }
}

/// Cancel-safe: `line` holds the progress between calls.
async fn read_line(
    reader: &mut (impl AsyncBufRead + Unpin),
    line: &mut Line,
    cap: usize,
) -> std::io::Result<bool> {
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            return Ok(!line.is_empty());
        }
        let newline = chunk.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(chunk.len(), |at| at + 1);
        let keep = take.min(cap.saturating_sub(line.kept.len()));
        line.kept.extend_from_slice(&chunk[..keep]);
        line.dropped += take - keep;
        reader.consume(take);
        if newline.is_some() {
            return Ok(true);
        }
    }
}

/// How the process ended, reduced to what a verdict needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Exit {
    code: Option<i32>,
    signal: Option<i32>,
}

impl From<ExitStatus> for Exit {
    fn from(status: ExitStatus) -> Self {
        use std::os::unix::process::ExitStatusExt;
        Self {
            code: status.code(),
            signal: status.signal(),
        }
    }
}

/// Decides whether the agent succeeded: its own final verdict, and a clean exit.
fn judge(exit: Exit, verdict: Option<&Verdict>, stderr: &str) -> Result<(), String> {
    let hint = stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map_or(String::new(), |line| {
            format!(" Dernière ligne d'erreur : {line}")
        });
    if let Some(Verdict::Failure(why)) = verdict {
        return Err(why.clone());
    }
    if let Some(signal) = exit.signal {
        return Err(format!("l'agent a été tué par le signal {signal}.{hint}"));
    }
    match (exit.code, verdict) {
        (Some(0), Some(Verdict::Success)) => Ok(()),
        (Some(0), None) => Err(format!(
            "l'agent s'est arrêté sans rapport final : le flux a été coupé.{hint}"
        )),
        (code, _) => Err(format!(
            "l'agent a quitté avec le code {}.{hint}",
            code.map_or("inconnu".to_owned(), |c| c.to_string())
        )),
    }
}

pub struct SupervisorConfig {
    pub repo: PathBuf,
    pub worktree_root: PathBuf,
    pub max_concurrent: usize,
    pub run_timeout: Duration,
    pub checks: ChecksConfig,
}

/// The background task: claims queued runs, up to the concurrency limit, and drives them.
pub struct Supervisor {
    db: Db,
    orchestrator: Orchestrator,
    adapter: Arc<dyn AgentAdapter>,
    changes: Changes,
    config: SupervisorConfig,
    delivery: Option<Delivery>,
}

impl Supervisor {
    pub fn new(
        db: Db,
        orchestrator: Orchestrator,
        adapter: Arc<dyn AgentAdapter>,
        changes: Changes,
        config: SupervisorConfig,
    ) -> Self {
        Self {
            db,
            orchestrator,
            adapter,
            changes,
            config,
            delivery: None,
        }
    }

    pub fn with_delivery(mut self, delivery: Delivery) -> Self {
        self.orchestrator.set_delivery_locks(delivery.locks());
        self.delivery = Some(delivery);
        self
    }

    /// Never returns; abort the task to stop it.
    pub async fn run(self: Arc<Self>) {
        let slots = Arc::new(Semaphore::new(self.config.max_concurrent));
        loop {
            let Ok(slot) = Arc::clone(&slots).acquire_owned().await else {
                return;
            };
            let orchestrator = self.orchestrator.clone();
            let claimed = self
                .db
                .call(move |conn| orchestrator.claim_next(conn))
                .await;
            match claimed {
                Ok(Some(run)) => {
                    let this = Arc::clone(&self);
                    tokio::spawn(async move {
                        this.execute(run).await;
                        drop(slot);
                    });
                }
                Ok(None) => {
                    drop(slot);
                    self.orchestrator.shared.wake.notified().await;
                }
                Err(e) => {
                    log_store_error("cannot claim a run", &e);
                    drop(slot);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }

    async fn execute(&self, run: Run) {
        let id = run.id;
        let Some(cancel) = self.orchestrator.cancel_handle(id) else {
            return;
        };
        let outcome = self.drive(&run, &cancel).await;
        self.conclude(id, outcome).await;
        self.orchestrator.forget(id);
        self.changes.publish();
    }

    /// Everything between "claimed" and "has an outcome".
    async fn drive(&self, run: &Run, cancel: &Notify) -> Outcome {
        let id = run.id;
        let card_id = run.card_id;
        let key = self
            .db
            .call(move |conn| {
                store::move_to_category(conn, card_id, Category::InProgress)?;
                store::card_key(conn, card_id)
            })
            .await;
        let (key, number) = match key {
            Ok(key) => key,
            Err(e) => return Outcome::Failed(format!("la carte est introuvable : {e}")),
        };
        self.changes.publish();

        let earlier = self
            .db
            .call(move |conn| runs::possibly_orphaned_pid(conn, card_id, id))
            .await;
        match earlier {
            Err(e) => return Outcome::Failed(format!("exécutions précédentes illisibles : {e}")),
            Ok(Some(pid)) if group_exists(pid) => {
                return self.fail(id, still_running_message(pid)).await;
            }
            Ok(_) => {}
        }

        let prepared = unless_cancelled(
            cancel,
            git::prepare_worktree(&self.config.repo, &self.config.worktree_root, &key, number),
        )
        .await;
        let worktree = match prepared {
            None => return self.stopped(),
            Some(Err(e)) => return Outcome::Failed(format!("worktree impossible : {e}")),
            Some(Ok(worktree)) => worktree,
        };
        let (path, branch) = (
            worktree.path.to_string_lossy().into_owned(),
            worktree.branch.clone(),
        );
        let recorded = {
            let (path, branch) = (path.clone(), branch.clone());
            self.db
                .call(move |conn| runs::record_workspace(conn, id, &path, &branch))
                .await
        };
        if let Err(e) = recorded {
            return Outcome::Failed(format!("worktree non enregistré : {e}"));
        }
        self.note(
            id,
            EventKind::Notice,
            format!("Worktree prêt : {path} (branche {branch})"),
        )
        .await;

        let mut child = match self.spawn_agent(run, &worktree.path) {
            Ok(child) => child,
            Err(e) => return Outcome::Failed(e),
        };
        let group = ProcessGroup::of(&child);
        if let Some(pid) = child.id() {
            let _ = self
                .db
                .call(move |conn| runs::record_pid(conn, id, pid))
                .await;
        }
        let stderr_task = tokio::spawn(read_capped(child.stderr.take()));

        let mut finish = None;
        let ended = self.stream(run, &mut child, &mut finish, cancel).await;
        let exit = match ended {
            Ended::Exited(exit) => exit,
            Ended::TimedOut => {
                terminate(&mut child, group.as_ref()).await;
                self.record_stderr(id, None, stderr_task).await;
                let why = format!(
                    "l'agent a dépassé la limite de {} et a été arrêté.",
                    describe(self.config.run_timeout)
                );
                return self.fail(id, why).await;
            }
            Ended::Cancelled => {
                terminate(&mut child, group.as_ref()).await;
                self.record_stderr(id, None, stderr_task).await;
                let outcome = self.stopped();
                let notice = match &outcome {
                    Outcome::Interrupted(why) => why.clone(),
                    _ => "Annulée par l'utilisateur.".to_owned(),
                };
                self.note(id, EventKind::Notice, notice).await;
                return outcome;
            }
        };
        let stderr = self.record_stderr(id, exit.code, stderr_task).await;
        if let Err(why) = judge(exit, finish.as_ref().map(|f| &f.verdict), &stderr) {
            self.note(id, EventKind::Error, why.clone()).await;
            return Outcome::Failed(why);
        }

        self.publish_branch(id, &worktree, cancel).await
    }

    fn spawn_agent(&self, run: &Run, cwd: &std::path::Path) -> Result<Child, String> {
        if self.adapter.agent() != run.agent {
            return Err(format!(
                "aucun adaptateur pour l'agent {} dans cette configuration",
                run.agent.slug()
            ));
        }
        let spec = self.adapter.command(&Launch {
            prompt: &run.prompt,
            model: run.model.as_ref(),
            permission_mode: run.permission_mode,
        });
        let mut child = Command::new(&spec.program)
            .args(&spec.args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0)
            .spawn()
            .map_err(|e| format!("impossible de lancer `{}` : {e}", spec.program.display()))?;
        if let Some(mut stdin) = child.stdin.take() {
            // Written from its own task: a prompt larger than the pipe must not block the
            // reader below, or the agent and Helm would wait on each other.
            let prompt = spec.stdin;
            tokio::spawn(async move {
                let _ = stdin.write_all(prompt.as_bytes()).await;
            });
        }
        Ok(child)
    }

    /// Reads the agent's stdout to its end, storing each line, then waits for the exit.
    async fn stream(
        &self,
        run: &Run,
        child: &mut Child,
        finish: &mut Option<Finish>,
        cancel: &Notify,
    ) -> Ended {
        let id = run.id;
        let Some(stdout) = child.stdout.take() else {
            return Ended::Exited(Exit {
                code: None,
                signal: None,
            });
        };
        let reading = async {
            let mut reader = BufReader::new(stdout);
            let mut line = Line::default();
            let mut session_recorded = false;
            let mut unpublished = false;
            let mut tick = tokio::time::interval(PUBLISH_EVERY);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    read = read_line(&mut reader, &mut line, MAX_LINE_BYTES) => match read {
                        Ok(false) => break,
                        Ok(true) => {
                            let text = String::from_utf8_lossy(&line.kept).into_owned();
                            let parsed = if line.dropped > 0 {
                                Some(adapter::truncated(&text, line.kept.len() + line.dropped))
                            } else {
                                self.adapter.parse_line(&text)
                            };
                            line.clear();
                            if let Some(parsed) = parsed {
                                self.store_line(id, parsed, &mut session_recorded, finish).await;
                                unpublished = true;
                            }
                        }
                        Err(e) => {
                            self.note(id, EventKind::Error, format!("lecture de la sortie impossible : {e}")).await;
                            break;
                        }
                    },
                    _ = tick.tick(), if unpublished => {
                        unpublished = false;
                        self.changes.publish();
                    }
                }
            }
            child.wait().await
        };
        let limited = tokio::time::timeout(self.config.run_timeout, reading);
        let Some(timed) = unless_cancelled(cancel, limited).await else {
            return Ended::Cancelled;
        };
        match timed {
            Err(_) => Ended::TimedOut,
            Ok(Ok(status)) => Ended::Exited(status.into()),
            Ok(Err(e)) => {
                self.note(
                    id,
                    EventKind::Error,
                    format!("attente du processus impossible : {e}"),
                )
                .await;
                Ended::Exited(Exit {
                    code: None,
                    signal: None,
                })
            }
        }
    }

    async fn store_line(
        &self,
        id: RunId,
        parsed: ParsedLine,
        session_recorded: &mut bool,
        finish: &mut Option<Finish>,
    ) {
        let ParsedLine {
            event,
            session_id,
            finish: line_finish,
        } = parsed;
        let record_session = session_id.filter(|_| !*session_recorded);
        *session_recorded |= record_session.is_some();
        let usage = line_finish.as_ref().map(|f| f.usage.clone());
        let stored = self
            .db
            .call(move |conn| {
                runs::append_event(conn, id, &event)?;
                if let Some(session) = record_session {
                    runs::record_session(conn, id, &session)?;
                }
                if let Some(usage) = usage {
                    runs::record_usage(
                        conn,
                        id,
                        usage.cost_usd,
                        usage.tokens_in,
                        usage.tokens_out,
                    )?;
                }
                Ok::<_, StoreError>(())
            })
            .await;
        if let Err(e) = stored {
            log_store_error("cannot store an agent event", &e);
        }
        if line_finish.is_some() {
            *finish = line_finish;
        }
    }

    async fn record_stderr(
        &self,
        id: RunId,
        code: Option<i32>,
        task: tokio::task::JoinHandle<String>,
    ) -> String {
        // A grandchild that outlives the agent can keep the pipe open; do not wait for it.
        let stderr = match tokio::time::timeout(STDERR_DRAIN, task).await {
            Ok(Ok(text)) => text,
            _ => String::new(),
        };
        let kept = stderr.clone();
        let stored = self
            .db
            .call(move |conn| runs::record_exit(conn, id, code, &kept))
            .await;
        if let Err(e) = stored {
            log_store_error("cannot store the agent's exit", &e);
        }
        stderr
    }

    async fn publish_branch(
        &self,
        id: RunId,
        worktree: &git::Worktree,
        cancel: &Notify,
    ) -> Outcome {
        let head = match self.verify(id, worktree, cancel).await {
            Ok(head) => head,
            Err(outcome) => return outcome,
        };
        let branch = &worktree.branch;
        let pending = unless_cancelled(cancel, git::unpublished(&self.config.repo, worktree)).await;
        match pending {
            None => return self.stopped(),
            Some(Err(e)) => {
                return self
                    .fail(
                        id,
                        format!("la branche {branch} ne peut pas être comparée à origin : {e}"),
                    )
                    .await;
            }
            Some(Ok(git::Unpublished::Nothing)) => {
                return self
                    .fail(
                        id,
                        format!("rien à pousser : origin a déjà tous les commits de {branch}."),
                    )
                    .await;
            }
            Some(Ok(git::Unpublished::Diverged)) => {
                return self
                    .fail(
                        id,
                        format!(
                            "origin a sur {branch} des commits que le worktree n'a pas (historique réécrit ou travail poussé d'ailleurs) : Helm ne force jamais le push. Réconciliez la branche à la main, puis relancez."
                        ),
                    )
                    .await;
            }
            Some(Ok(git::Unpublished::Commits(count))) => {
                self.note(
                    id,
                    EventKind::Notice,
                    format!("{count} commit(s) à pousser sur {branch}."),
                )
                .await;
            }
        }
        if git::has_uncommitted_changes(&worktree.path)
            .await
            .unwrap_or(false)
        {
            self.note(
                id,
                EventKind::Notice,
                "Des modifications non commitées restent dans le worktree ; elles ne sont pas poussées.".to_owned(),
            )
            .await;
        }
        self.note(
            id,
            EventKind::Notice,
            format!("Push de {} vers origin.", worktree.branch),
        )
        .await;
        let strict = !self.config.checks.commands.is_empty()
            || self.delivery.as_ref().is_some_and(Delivery::enabled);
        match git::checked_head(&worktree.path, branch, strict).await {
            Ok(current) if current == head => {}
            _ => return self.fail(id, "Le commit ou les fichiers ont changé depuis les vérifications : rien n'est poussé.".to_owned()).await,
        }
        let pushing = async {
            if strict {
                git::push_commit(&worktree.path, &worktree.branch, &head).await
            } else {
                git::push(&worktree.path, &worktree.branch).await
            }
        };
        match unless_cancelled(cancel, pushing).await {
            None => self.stopped(),
            Some(Err(e)) => self.fail(id, format!("git push a échoué : {e}")).await,
            Some(Ok(())) => {
                self.note(
                    id,
                    EventKind::Notice,
                    format!("{} poussée sur origin.", worktree.branch),
                )
                .await;
                if let Some(delivery) = &self.delivery {
                    if delivery.enabled() {
                        // The agent, checks and push are complete. An interrupted GitHub
                        // request must leave this delivery retryable without rerunning code.
                        let publication = unless_cancelled(cancel, delivery.publish(id, &head))
                            .await
                            .unwrap_or_else(|| Err("Publication GitHub interrompue.".to_owned()));
                        if let Err(error) = publication {
                            let message = format!(
                                "La branche est poussée, mais la PR n'a pas pu être créée : {error} Utilisez « Créer la PR » sur la carte pour réessayer."
                            );
                            self.note(id, EventKind::Error, message.clone()).await;
                            let _ = self
                                .db
                                .call(move |conn| {
                                    let run = runs::get_run(conn, id)?;
                                    comment(conn, run.card_id, message)
                                })
                                .await;
                        }
                    }
                }
                Outcome::Succeeded
            }
        }
    }

    async fn verify(
        &self,
        id: RunId,
        worktree: &git::Worktree,
        cancel: &Notify,
    ) -> Result<String, Outcome> {
        let strict = !self.config.checks.commands.is_empty()
            || self.delivery.as_ref().is_some_and(Delivery::enabled);
        let head = match unless_cancelled(
            cancel,
            git::checked_head(&worktree.path, &worktree.branch, strict),
        )
        .await
        {
            None => return Err(self.stopped()),
            Some(Err(error)) => {
                return Err(self
                    .fail(id, format!("Vérification impossible : {error}"))
                    .await);
            }
            Some(Ok(head)) => head,
        };
        let (commit, commands) = (head.clone(), self.config.checks.commands.clone());
        if let Err(error) = self
            .db
            .call(move |conn| checks::start(conn, id, &commit, &commands))
            .await
        {
            return Err(self
                .fail(id, format!("Vérifications non enregistrées : {error}"))
                .await);
        }
        self.changes.publish();
        for (position, command) in self.config.checks.commands.iter().enumerate() {
            let position = position as i64;
            if let Err(error) = self
                .db
                .call(move |conn| checks::start_check(conn, id, position))
                .await
            {
                return Err(self
                    .fail(id, format!("Vérification non démarrée : {error}"))
                    .await);
            }
            self.changes.publish();
            self.note(id, EventKind::Notice, format!("Vérification : {command}"))
                .await;
            let output = match unless_cancelled(
                cancel,
                checks::execute(command, &worktree.path, self.config.checks.timeout),
            )
            .await
            {
                None => return Err(self.stopped()),
                Some(Ok(output)) => output,
                Some(Err(error)) => crate::process::CommandOutput {
                    stdout: String::new(),
                    stderr: error,
                    exit_code: None,
                    timed_out: false,
                },
            };
            let success = output.success();
            if let Err(error) = self
                .db
                .call(move |conn| checks::finish_check(conn, id, position, &output))
                .await
            {
                return Err(self
                    .fail(
                        id,
                        format!("Résultat de vérification non enregistré : {error}"),
                    )
                    .await);
            }
            self.changes.publish();
            if !success {
                let _ = self
                    .db
                    .call(move |conn| checks::finish(conn, id, VerificationStatus::Failed))
                    .await;
                return Err(self
                    .fail(
                        id,
                        format!("La vérification `{command}` a échoué. Aucun commit n'est poussé."),
                    )
                    .await);
            }
        }
        if strict {
            match unless_cancelled(
                cancel,
                git::checked_head(&worktree.path, &worktree.branch, true),
            )
            .await
            {
                None => return Err(self.stopped()),
                Some(Ok(after)) if after == head => {}
                _ => {
                    let _ = self
                        .db
                        .call(move |conn| checks::finish(conn, id, VerificationStatus::Failed))
                        .await;
                    return Err(self.fail(id, "Les vérifications ont modifié le commit ou les fichiers : rien n'est poussé.".to_owned()).await);
                }
            }
        }
        if !self.config.checks.commands.is_empty() {
            if let Err(error) = self
                .db
                .call(move |conn| checks::finish(conn, id, VerificationStatus::Passed))
                .await
            {
                return Err(self
                    .fail(id, format!("Vérifications non enregistrées : {error}"))
                    .await);
            }
            self.note(
                id,
                EventKind::Notice,
                format!("Toutes les vérifications ont réussi sur {head}."),
            )
            .await;
        }
        self.changes.publish();
        Ok(head)
    }

    fn stopped(&self) -> Outcome {
        if self.orchestrator.stopping() {
            Outcome::Interrupted(STOPPED_BY_SHUTDOWN.to_owned())
        } else {
            Outcome::Cancelled
        }
    }

    async fn fail(&self, id: RunId, why: String) -> Outcome {
        self.note(id, EventKind::Error, why.clone()).await;
        Outcome::Failed(why)
    }

    /// Writes Helm's own line in the run's log.
    async fn note(&self, id: RunId, kind: EventKind, text: String) {
        let event = NewEvent {
            kind,
            summary: text.clone(),
            payload: text,
        };
        let stored = self
            .db
            .call(move |conn| runs::append_event(conn, id, &event))
            .await;
        if let Err(e) = stored {
            log_store_error("cannot store a run note", &e);
        }
    }

    /// Records the end: the run's final state, the card's column, and a line in its thread.
    async fn conclude(&self, id: RunId, outcome: Outcome) {
        let concluded = self
            .db
            .call(move |conn| {
                if checks::get(conn, id)?.is_some() {
                    checks::finish(conn, id, VerificationStatus::Interrupted)?;
                }
                runs::finish(conn, id, &outcome)?;
                let run = runs::get_run(conn, id)?;
                match &outcome {
                    Outcome::Succeeded => {
                        let in_progress =
                            store::card_category(conn, run.card_id)? == Category::InProgress;
                        if in_progress {
                            store::move_to_category(conn, run.card_id, Category::InReview)?;
                        }
                        let branch = run.branch.as_deref().unwrap_or("?");
                        let left_alone = if in_progress {
                            ""
                        } else {
                            " La carte n'a pas été déplacée : elle n'est plus dans « En cours »."
                        };
                        comment(
                            conn,
                            run.card_id,
                            format!("Exécution {id} réussie : la branche `{branch}` est poussée sur origin.{left_alone}"),
                        )
                    }
                    Outcome::Failed(why) => comment(
                        conn,
                        run.card_id,
                        format!("Exécution {id} échouée : {why}"),
                    ),
                    Outcome::Interrupted(why) => comment(
                        conn,
                        run.card_id,
                        format!("Exécution {id} interrompue : {why}"),
                    ),
                    Outcome::Cancelled => {
                        comment(conn, run.card_id, format!("Exécution {id} annulée."))
                    }
                }
            })
            .await;
        if let Err(e) = concluded {
            log_store_error("cannot record the end of a run", &e);
        }
    }
}

/// Asks the whole group to stop, then insists.
async fn terminate(child: &mut Child, group: Option<&ProcessGroup>) {
    if let Some(group) = group {
        group.signal(libc::SIGTERM);
    }
    if tokio::time::timeout(TERMINATE_GRACE, child.wait())
        .await
        .is_err()
    {
        if let Some(group) = group {
            group.signal(libc::SIGKILL);
        }
        let _ = child.wait().await;
    }
}

/// Reads a pipe to its end, keeping the first [`STDERR_KEPT_BYTES`]. The rest is still read
/// so the process never blocks on a full pipe.
async fn read_capped(pipe: Option<tokio::process::ChildStderr>) -> String {
    let Some(mut pipe) = pipe else {
        return String::new();
    };
    let mut kept = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = STDERR_KEPT_BYTES.saturating_sub(kept.len());
                kept.extend_from_slice(&chunk[..n.min(room)]);
            }
        }
    }
    String::from_utf8_lossy(&kept).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::ClaudeAdapter;
    use crate::git::testing::{Remote, run as git_run};
    use crate::store::CardInput;

    const FAKE_CLAUDE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake-claude.sh");

    #[derive(Clone, Copy)]
    struct Limits {
        max_concurrent: usize,
        run_timeout: Duration,
    }

    /// A board with one project repo, a bare origin, and a supervisor driving the fake agent.
    struct Harness {
        remote: Remote,
        db: Db,
        orchestrator: Orchestrator,
        changes: Changes,
        limits: Limits,
        checks: ChecksConfig,
        task: tokio::task::JoinHandle<()>,
    }

    impl Harness {
        fn new(name: &str) -> Self {
            Self::with_limit(name, 2)
        }

        fn with_limit(name: &str, max_concurrent: usize) -> Self {
            Self::build(
                name,
                Limits {
                    max_concurrent,
                    run_timeout: Duration::from_secs(60),
                },
            )
        }

        fn with_timeout(name: &str, run_timeout: Duration) -> Self {
            Self::build(
                name,
                Limits {
                    max_concurrent: 2,
                    run_timeout,
                },
            )
        }

        fn build(name: &str, limits: Limits) -> Self {
            let remote = Remote::new(name);
            let db = Db::open_in_memory().unwrap();
            let orchestrator = Orchestrator::new(
                RunGate::Open,
                RunDefaults {
                    permission_mode: PermissionMode::BypassPermissions,
                    model: ModelName::parse_optional("sonnet").unwrap(),
                },
            );
            let changes = Changes::new();
            let task = tokio::spawn(Self::supervisor(
                &remote,
                &db,
                &orchestrator,
                &changes,
                limits,
                ChecksConfig::default(),
            ));
            Self {
                remote,
                db,
                orchestrator,
                changes,
                limits,
                checks: ChecksConfig::default(),
                task,
            }
        }

        fn supervisor(
            remote: &Remote,
            db: &Db,
            orchestrator: &Orchestrator,
            changes: &Changes,
            limits: Limits,
            checks: ChecksConfig,
        ) -> impl Future<Output = ()> + use<> {
            let supervisor = Supervisor::new(
                db.clone(),
                orchestrator.clone(),
                Arc::new(ClaudeAdapter {
                    command: PathBuf::from(FAKE_CLAUDE),
                }),
                changes.clone(),
                SupervisorConfig {
                    repo: remote.repo.clone(),
                    worktree_root: remote.worktrees.clone(),
                    max_concurrent: limits.max_concurrent,
                    run_timeout: limits.run_timeout,
                    checks,
                },
            );
            Arc::new(supervisor).run()
        }

        async fn wait_for_file(&self, name: &str) {
            let path = self.remote.worktrees.join(name);
            for _ in 0..200 {
                if path.exists() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            panic!("{name} never appeared");
        }

        async fn child_pid(&self, worktree: &str) -> i64 {
            let marker = self.remote.worktrees.join(format!("{worktree}.child"));
            for _ in 0..200 {
                let pid = std::fs::read_to_string(&marker)
                    .ok()
                    .and_then(|text| text.trim().parse::<i64>().ok());
                if let Some(pid) = pid {
                    return pid;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            panic!("the agent never started a child");
        }

        async fn requeue(&self, card: i64) {
            let orchestrator = self.orchestrator.clone();
            self.db
                .call(move |conn| {
                    store::move_card(conn, card, 1, 0)?;
                    let entered = store::move_card(conn, card, 2, 0)?;
                    orchestrator.after_placement(conn, card, entered)
                })
                .await
                .unwrap();
        }

        fn restart(&mut self) {
            self.task.abort();
            self.task = tokio::spawn(Self::supervisor(
                &self.remote,
                &self.db,
                &self.orchestrator,
                &self.changes,
                self.limits,
                self.checks.clone(),
            ));
        }

        fn with_checks(name: &str, commands: &[&str], timeout: Duration) -> Self {
            let mut harness = Self::new(name);
            harness.checks = ChecksConfig {
                commands: commands
                    .iter()
                    .map(|command| (*command).to_owned())
                    .collect(),
                timeout,
            };
            harness.restart();
            harness
        }

        fn enable_github(&mut self) -> Delivery {
            self.enable_github_command(PathBuf::from(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/fake-gh.sh"
            )))
        }

        fn enable_github_command(&mut self, command: PathBuf) -> Delivery {
            self.task.abort();
            let delivery = Delivery::new(
                self.db.clone(),
                self.changes.clone(),
                RunGate::Open,
                Some(self.remote.repo.clone()),
                Some(crate::config::GithubConfig {
                    repository: "owner/repo".to_owned(),
                    command,
                }),
            );
            let supervisor = Supervisor::new(
                self.db.clone(),
                self.orchestrator.clone(),
                Arc::new(ClaudeAdapter {
                    command: PathBuf::from(FAKE_CLAUDE),
                }),
                self.changes.clone(),
                SupervisorConfig {
                    repo: self.remote.repo.clone(),
                    worktree_root: self.remote.worktrees.clone(),
                    max_concurrent: self.limits.max_concurrent,
                    run_timeout: self.limits.run_timeout,
                    checks: self.checks.clone(),
                },
            )
            .with_delivery(delivery.clone());
            self.task = tokio::spawn(Arc::new(supervisor).run());
            delivery
        }

        /// A card with the agent assigned, created straight in `À faire` so a run is queued.
        async fn card(&self, scenario: &str, model: &str) -> i64 {
            let input = CardInput {
                title: format!("Task {scenario}"),
                description: format!("SCENARIO: {scenario}"),
                agent: "claude".to_owned(),
                model: model.to_owned(),
                ..CardInput::default()
            };
            let orchestrator = self.orchestrator.clone();
            self.db
                .call(move |conn| {
                    let id = store::create_card(conn, 2, &input)?;
                    orchestrator.after_placement(conn, id, true)?;
                    Ok::<_, StoreError>(id)
                })
                .await
                .unwrap()
        }

        async fn latest(&self, card: i64) -> Run {
            self.db
                .call(move |conn| runs::latest_run(conn, card))
                .await
                .unwrap()
                .expect("a run")
        }

        async fn settled(&self, card: i64) -> Run {
            self.wait_for(card, |run| !run.status.is_active()).await
        }

        async fn wait_for(&self, card: i64, ready: impl Fn(&Run) -> bool) -> Run {
            for _ in 0..400 {
                let run = self.latest(card).await;
                if ready(&run) {
                    return run;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            panic!(
                "run did not reach the expected state: {:?}",
                self.latest(card).await
            );
        }

        async fn column_category(&self, card: i64) -> Category {
            self.db
                .call(move |conn| store::card_category(conn, card))
                .await
                .unwrap()
        }

        async fn events(&self, run: RunId) -> Vec<(EventKind, String)> {
            self.db
                .call(move |conn| runs::list_events(conn, run, 1000))
                .await
                .unwrap()
                .into_iter()
                .map(|e| (e.kind, e.summary))
                .collect()
        }

        async fn comments(&self, card: i64) -> Vec<String> {
            self.db
                .call(move |conn| store::list_comments(conn, card))
                .await
                .unwrap()
                .into_iter()
                .map(|c| c.body)
                .collect()
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    #[tokio::test]
    async fn passing_checks_are_recorded_against_the_pushed_commit() {
        let h = Harness::with_checks(
            "sup-checks-pass",
            &["test -f HELLO.md", "printf checked"],
            Duration::from_secs(2),
        );
        let card = h.card("success", "").await;
        let run = h.settled(card).await;
        assert_eq!(run.status, RunStatus::Succeeded, "{:?}", run.error);
        let verification =
            h.db.call(move |conn| checks::get(conn, run.id))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(verification.status, VerificationStatus::Passed);
        assert_eq!(
            verification.commit_sha,
            git_run(&h.remote.origin, &["rev-parse", "helm/HELM-1"])
        );
        assert_eq!(verification.checks[1].stdout, "checked");
        assert!(
            verification
                .checks
                .iter()
                .all(|check| check.exit_code == Some(0))
        );
    }

    #[tokio::test]
    async fn a_failed_check_stops_later_checks_and_never_pushes() {
        let h = Harness::with_checks(
            "sup-checks-fail",
            &["printf failed >&2; exit 9", "touch should-not-run"],
            Duration::from_secs(2),
        );
        let card = h.card("success", "").await;
        let run = h.settled(card).await;
        assert_eq!(run.status, RunStatus::Failed);
        let verification =
            h.db.call(move |conn| checks::get(conn, run.id))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(verification.status, VerificationStatus::Failed);
        assert_eq!(verification.checks[0].exit_code, Some(9));
        assert_eq!(verification.checks[0].stderr, "failed");
        assert_eq!(verification.checks[1].status, checks::CheckStatus::Pending);
        assert!(!h.remote.worktrees.join("HELM-1/should-not-run").exists());
        assert!(
            !git_run(&h.remote.origin, &["branch", "--list", "helm/HELM-1"])
                .contains("helm/HELM-1")
        );
    }

    #[tokio::test]
    async fn checks_that_change_the_tree_or_commit_cannot_publish_their_result() {
        for (name, command) in [
            ("sup-checks-dirty", "printf changed >> HELLO.md"),
            (
                "sup-checks-commit",
                "git -c user.name=Test -c user.email=test@example.com commit --allow-empty -qm 'new commit'",
            ),
        ] {
            let h = Harness::with_checks(name, &[command], Duration::from_secs(2));
            let card = h.card("success", "").await;
            let run = h.settled(card).await;
            assert_eq!(run.status, RunStatus::Failed, "{name}");
            assert!(run.error.unwrap().contains("modifié"));
            let verification =
                h.db.call(move |conn| checks::get(conn, run.id))
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(verification.status, VerificationStatus::Failed);
            assert_eq!(
                git_run(&h.remote.origin, &["branch", "--list", "helm/HELM-1"]),
                ""
            );
        }
    }

    #[tokio::test]
    async fn a_timed_out_check_keeps_its_output_and_does_not_publish() {
        let h = Harness::with_checks(
            "sup-checks-timeout",
            &["printf before; sleep 30"],
            Duration::from_millis(100),
        );
        let card = h.card("success", "").await;
        let run = h.settled(card).await;
        assert_eq!(run.status, RunStatus::Failed);
        let verification =
            h.db.call(move |conn| checks::get(conn, run.id))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(verification.status, VerificationStatus::Failed);
        assert_eq!(verification.checks[0].stdout, "before");
        assert!(verification.checks[0].error.is_some());
        assert_eq!(
            git_run(&h.remote.origin, &["branch", "--list", "helm/HELM-1"]),
            ""
        );
    }

    #[tokio::test]
    async fn cancelling_a_check_interrupts_its_result_and_does_not_push() {
        let h = Harness::with_checks(
            "sup-checks-cancel",
            &["touch ../check-started; sleep 30"],
            Duration::from_secs(60),
        );
        let card = h.card("success", "").await;
        h.wait_for_file("check-started").await;
        let run = h.latest(card).await;
        h.orchestrator.cancel(&h.db, run.id).await.unwrap();
        let run = h.settled(card).await;
        assert_eq!(run.status, RunStatus::Cancelled);
        let verification =
            h.db.call(move |conn| checks::get(conn, run.id))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(verification.status, VerificationStatus::Interrupted);
        assert_eq!(
            verification.checks[0].status,
            checks::CheckStatus::Interrupted
        );
        assert_eq!(
            git_run(&h.remote.origin, &["branch", "--list", "helm/HELM-1"]),
            ""
        );
    }

    fn github_snapshot(head: &str) -> serde_json::Value {
        serde_json::json!({
            "number": 7, "url": "https://github.com/owner/repo/pull/7",
            "state": "OPEN", "isDraft": false, "headRefOid": head,
            "headRefName": "helm/HELM-1", "baseRefName": "main", "isCrossRepository": false,
            "statusCheckRollup": [], "mergeable": "MERGEABLE", "mergeStateStatus": "CLEAN", "reviewDecision": ""
        })
    }

    #[tokio::test]
    async fn successful_checks_and_push_create_a_ready_pr_with_the_actual_commit() {
        let mut h = Harness::with_checks(
            "sup-checks-github",
            &["touch ../check-started; while [ ! -f ../check-continue ]; do sleep 0.01; done"],
            Duration::from_secs(5),
        );
        h.enable_github();
        let state = h.remote.repo.with_extension("gh");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("list.json"), "[]").unwrap();
        let card = h.card("success", "").await;
        h.wait_for_file("check-started").await;
        let head = git_run(&h.remote.repo, &["rev-parse", "helm/HELM-1"]);
        std::fs::write(
            state.join("after-create.json"),
            github_snapshot(&head).to_string(),
        )
        .unwrap();
        std::fs::write(h.remote.worktrees.join("check-continue"), "go").unwrap();
        let run = h.settled(card).await;
        assert_eq!(run.status, RunStatus::Succeeded, "{:?}", run.error);
        let pr =
            h.db.call(move |conn| crate::delivery::get(conn, card))
                .await
                .unwrap()
                .expect("created PR");
        assert_eq!(pr.run_id, run.id);
        assert_eq!(pr.pr.head_sha, head);
        assert!(!pr.pr.draft);
        assert!(
            std::fs::read_to_string(state.join("create.body"))
                .unwrap()
                .contains(&head)
        );
        assert_eq!(h.column_category(card).await, Category::InReview);
    }

    #[tokio::test]
    async fn cancelling_pr_publication_keeps_the_pushed_commit_available_for_retry() {
        use std::os::unix::fs::PermissionsExt;
        let mut h = Harness::with_checks("sup-published-cancel", &["true"], Duration::from_secs(2));
        let state = h.remote.repo.with_extension("gh");
        std::fs::create_dir_all(&state).unwrap();
        let command = h.remote.dir.join("slow-gh");
        let fake = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake-gh.sh");
        std::fs::write(&command, format!("#!/bin/sh\nif [ ! -f \"$PWD.gh/retry\" ]; then touch \"$PWD.gh/started\"; sleep 30; fi\nexec '{fake}' \"$@\"\n")).unwrap();
        std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o755)).unwrap();
        let delivery = h.enable_github_command(command);
        let card = h.card("success", "").await;
        for _ in 0..200 {
            if state.join("started").exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(state.join("started").exists());
        let active = h.latest(card).await;
        h.orchestrator.cancel(&h.db, active.id).await.unwrap();
        let run = h.settled(card).await;
        assert_eq!(
            run.status,
            RunStatus::Succeeded,
            "a successful push remains publishable"
        );
        assert!(run.pushed_at.is_some());
        let head = git_run(&h.remote.origin, &["rev-parse", "helm/HELM-1"]);
        std::fs::write(state.join("retry"), "go").unwrap();
        std::fs::write(state.join("list.json"), "[]").unwrap();
        std::fs::write(
            state.join("after-create.json"),
            github_snapshot(&head).to_string(),
        )
        .unwrap();
        delivery.retry(card).await.unwrap();
        assert!(
            h.db.call(move |conn| crate::delivery::get(conn, card))
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn a_card_moved_to_todo_runs_the_agent_pushes_its_branch_and_lands_in_review() {
        let h = Harness::new("sup-success");
        let card = h.card("success", "haiku").await;

        let run = h.settled(card).await;

        assert_eq!(run.status, RunStatus::Succeeded, "{:?}", run.error);
        assert_eq!(h.column_category(card).await, Category::InReview);
        assert_eq!(run.branch.as_deref(), Some("helm/HELM-1"));
        let worktree = h.remote.worktrees.join("HELM-1");
        assert_eq!(run.worktree_path.as_deref(), worktree.to_str());
        assert!(worktree.join("HELLO.md").exists());
        // The commit is on the branch in the bare origin.
        assert_eq!(
            git_run(
                &h.remote.origin,
                &["log", "-1", "--format=%s", "helm/HELM-1"]
            ),
            "Add HELLO.md"
        );
        // Recorded from the stream: session, cost, tokens, exit.
        assert_eq!(
            run.session_id.as_deref(),
            Some("d157be31-f3e0-44f0-9aa9-7c88253236bf")
        );
        assert_eq!(run.cost_usd, Some(0.0465764));
        assert_eq!((run.tokens_in, run.tokens_out), (Some(17), Some(347)));
        assert_eq!(run.exit_code, Some(0));
        assert!(run.pushed_at.is_some() && run.finished_at.is_some() && run.pid.is_some());
        // The agent saw the flags the config asked for, and the card's model won.
        let args = std::fs::read_to_string(format!("{}.args", worktree.display())).unwrap();
        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            [
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--permission-mode",
                "bypassPermissions",
                "--model",
                "haiku"
            ]
        );
        // The prompt it was sent is the one stored on the run.
        assert!(run.prompt.contains("SCENARIO: success") && run.prompt.contains("Task success"));
        // Every stdout line became an event, and Helm's own steps are interleaved.
        let events = h.events(run.id).await;
        assert!(
            events
                .iter()
                .filter(|(kind, _)| *kind == EventKind::ToolUse)
                .count()
                >= 2
        );
        assert!(
            events
                .iter()
                .any(|(k, s)| *k == EventKind::Notice && s.contains("poussée sur origin"))
        );
        assert_eq!(h.comments(card).await.len(), 1);
        assert!(h.comments(card).await[0].contains("réussie"));
    }

    #[tokio::test]
    async fn the_project_default_model_is_used_when_the_card_names_none() {
        let h = Harness::new("sup-default-model");
        let card = h.card("success", "").await;
        let run = h.settled(card).await;
        assert_eq!(run.model.as_ref().map(ModelName::as_str), Some("sonnet"));
        let args = std::fs::read_to_string(format!(
            "{}.args",
            h.remote.worktrees.join("HELM-1").display()
        ))
        .unwrap();
        assert!(args.lines().any(|l| l == "sonnet"));
    }

    #[tokio::test]
    async fn a_failing_run_keeps_the_card_in_progress_and_shows_why() {
        let h = Harness::new("sup-failure");
        let card = h.card("bad_model", "").await;

        let run = h.settled(card).await;

        assert_eq!(run.status, RunStatus::Failed);
        assert!(
            run.error
                .as_deref()
                .unwrap()
                .contains("issue with the selected model")
        );
        assert_eq!(run.exit_code, Some(1));
        assert!(run.stderr.contains("unrecognized_model"));
        assert!(run.pushed_at.is_none());
        assert_eq!(h.column_category(card).await, Category::InProgress);
        assert!(
            h.comments(card).await[0].contains("échouée")
                && h.comments(card).await[0].contains("selected model")
        );
        let origin_branches = git_run(&h.remote.origin, &["branch", "--list", "helm/*"]);
        assert_eq!(origin_branches, "", "nothing is pushed for a failed run");
    }

    #[tokio::test]
    async fn malformed_output_lines_are_kept_and_do_not_stop_the_run() {
        let h = Harness::new("sup-malformed");
        let card = h.card("malformed", "").await;

        let run = h.settled(card).await;

        assert_eq!(run.status, RunStatus::Succeeded, "{:?}", run.error);
        let events = h.events(run.id).await;
        let malformed: Vec<&String> = events
            .iter()
            .filter(|(kind, _)| *kind == EventKind::Malformed)
            .map(|(_, summary)| summary)
            .collect();
        assert_eq!(malformed.len(), 2, "{events:?}");
        assert!(malformed[0].contains("this is not json"));
        let seqs =
            h.db.call(move |conn| runs::list_events(conn, run.id, 1000))
                .await
                .unwrap();
        assert!(seqs.windows(2).all(|w| w[1].seq > w[0].seq));
    }

    #[tokio::test]
    async fn an_agent_killed_mid_stream_fails_the_run_and_keeps_what_it_said() {
        let h = Harness::new("sup-killed");
        let card = h.card("killed", "").await;

        let run = h.settled(card).await;

        assert_eq!(run.status, RunStatus::Failed);
        let error = run.error.as_deref().unwrap();
        assert!(
            error.contains("signal 9") && error.contains("about to die"),
            "{error}"
        );
        assert_eq!(run.exit_code, None);
        assert!(run.stderr.contains("about to die"));
        assert!(
            h.events(run.id).await.len() >= 5,
            "events read before the kill are kept"
        );
        assert_eq!(h.column_category(card).await, Category::InProgress);
        assert!(run.pushed_at.is_none());
    }

    #[tokio::test]
    async fn an_oversized_output_line_is_truncated_and_recorded_without_failing_the_run() {
        let h = Harness::new("sup-long-line");
        let card = h.card("long_line", "").await;

        let run = h.settled(card).await;

        assert_eq!(run.status, RunStatus::Succeeded, "{:?}", run.error);
        let events = h.events(run.id).await;
        let truncated: Vec<&String> = events
            .iter()
            .filter(|(kind, summary)| *kind == EventKind::Malformed && summary.contains("tronquée"))
            .map(|(_, summary)| summary)
            .collect();
        assert_eq!(truncated.len(), 1, "{events:?}");
        assert!(truncated[0].contains("3000001"), "{}", truncated[0]);
        let largest: i64 =
            h.db.call(move |conn| {
                conn.query_row(
                    "SELECT MAX(length(payload)) FROM agent_events WHERE run_id = ?1",
                    [run.id],
                    |row| row.get(0),
                )
                .map_err(StoreError::from)
            })
            .await
            .unwrap();
        assert!(largest <= MAX_LINE_BYTES as i64 + 4096, "{largest}");
        assert!(
            events.iter().any(|(kind, _)| *kind == EventKind::Result),
            "the lines after the long one are still read"
        );
    }

    #[tokio::test]
    async fn a_run_still_going_after_the_time_limit_is_stopped_and_fails_with_the_reason() {
        let h = Harness::with_timeout("sup-timeout", Duration::from_secs(1));
        let card = h.card("group_child", "").await;
        let running = h.wait_for(card, |r| r.status == RunStatus::Running).await;
        let child = h.child_pid("HELM-1").await;

        let run = h.wait_for(card, |r| !r.status.is_active()).await;

        assert_eq!(run.status, RunStatus::Failed, "{:?}", run.error);
        assert_eq!(run.id, running.id);
        let error = run.error.as_deref().unwrap();
        assert!(error.contains("limite de 1 s"), "{error}");
        assert!(run.pushed_at.is_none());
        assert_eq!(h.column_category(card).await, Category::InProgress);
        assert!(h.comments(card).await[0].contains("limite de 1 s"));
        wait_until_gone(child).await;
    }

    #[tokio::test]
    async fn a_card_a_human_moved_while_the_run_was_going_stays_where_they_put_it() {
        let h = Harness::new("sup-moved");
        let card = h.card("wait_then_success", "").await;
        h.wait_for(card, |r| r.status == RunStatus::Running).await;
        h.wait_for_file("HELM-1.args").await;
        assert_eq!(h.column_category(card).await, Category::InProgress);

        let done_column = 5;
        h.db.call(move |conn| store::move_card(conn, card, done_column, 0))
            .await
            .unwrap();
        std::fs::write(h.remote.worktrees.join("HELM-1.go"), "").unwrap();
        let run = h.settled(card).await;

        assert_eq!(run.status, RunStatus::Succeeded, "{:?}", run.error);
        assert_eq!(h.column_category(card).await, Category::Done);
        let comments = h.comments(card).await;
        assert!(
            comments[0].contains("réussie") && comments[0].contains("pas été déplacée"),
            "{comments:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_push_fails_the_run_instead_of_reporting_success() {
        let h = Harness::new("sup-push-fail");
        git_run(
            &h.remote.repo,
            &["remote", "set-url", "origin", "/nonexistent/origin.git"],
        );
        let card = h.card("success", "").await;

        let run = h.settled(card).await;

        assert_eq!(run.status, RunStatus::Failed);
        assert!(
            run.error
                .as_deref()
                .unwrap()
                .contains("ne peut pas être comparée à origin"),
            "{:?}",
            run.error
        );
        assert!(run.pushed_at.is_none());
        assert_eq!(run.exit_code, Some(0), "the agent itself succeeded");
        assert_eq!(h.column_category(card).await, Category::InProgress);
        assert!(h.comments(card).await[0].contains("origin"));
        // The commit is still in the worktree for a human to deal with.
        assert!(h.remote.worktrees.join("HELM-1/HELLO.md").exists());
    }

    #[tokio::test]
    async fn a_run_that_commits_nothing_fails_and_leftover_changes_are_reported() {
        let h = Harness::new("sup-nothing");
        let card = h.card("no_commit", "").await;
        let run = h.settled(card).await;
        assert_eq!(run.status, RunStatus::Failed);
        assert!(
            run.error.as_deref().unwrap().contains("rien à pousser"),
            "{:?}",
            run.error
        );

        let card = h.card("silent_success", "").await;
        let run = h.settled(card).await;
        assert_eq!(
            run.error.as_deref().map(|e| e.contains("rapport final")),
            Some(true),
            "{:?}",
            run.error
        );

        let card = h.card("dirty", "").await;
        let run = h.settled(card).await;
        assert_eq!(run.status, RunStatus::Succeeded, "{:?}", run.error);
        assert!(
            h.events(run.id)
                .await
                .iter()
                .any(|(_, s)| s.contains("non commitées"))
        );
        let pushed = git_run(&h.remote.origin, &["ls-tree", "--name-only", "helm/HELM-3"]);
        assert!(
            pushed.contains("HELLO.md") && !pushed.contains("LEFTOVER.md"),
            "{pushed}"
        );
    }

    #[tokio::test]
    async fn a_rerun_that_adds_nothing_fails_when_origin_already_has_the_branch_tip() {
        let h = Harness::new("sup-rerun");
        let card = h.card("success", "").await;
        let first = h.settled(card).await;
        assert_eq!(first.status, RunStatus::Succeeded, "{:?}", first.error);

        let orchestrator = h.orchestrator.clone();
        h.db.call(move |conn| {
            let entered = store::move_card(conn, card, 2, 0)?;
            orchestrator.after_placement(conn, card, entered)
        })
        .await
        .unwrap();
        let second = h
            .wait_for(card, |run| run.id != first.id && !run.status.is_active())
            .await;

        assert_eq!(second.status, RunStatus::Failed, "{:?}", second.error);
        assert!(
            second.error.as_deref().unwrap().contains("rien à pousser"),
            "{:?}",
            second.error
        );
        assert!(second.pushed_at.is_none());
        assert_eq!(h.column_category(card).await, Category::InProgress);
    }

    #[tokio::test]
    async fn a_rerun_publishes_commits_an_earlier_run_made_but_could_not_push() {
        let h = Harness::new("sup-retry-push");
        let origin = h.remote.origin.to_str().unwrap().to_owned();
        git_run(
            &h.remote.repo,
            &["remote", "set-url", "origin", "/nonexistent/origin.git"],
        );
        let card = h.card("success", "").await;
        let first = h.settled(card).await;
        assert_eq!(first.status, RunStatus::Failed, "{:?}", first.error);
        assert!(h.remote.worktrees.join("HELM-1/HELLO.md").exists());

        git_run(&h.remote.repo, &["remote", "set-url", "origin", &origin]);
        h.requeue(card).await;
        let second = h
            .wait_for(card, |run| run.id != first.id && !run.status.is_active())
            .await;

        assert_eq!(second.status, RunStatus::Succeeded, "{:?}", second.error);
        assert!(second.pushed_at.is_some());
        let landed = git_run(
            &h.remote.origin,
            &["log", "-1", "--format=%s", "helm/HELM-1"],
        );
        assert_eq!(landed, "Add HELLO.md");
        assert_eq!(h.column_category(card).await, Category::InReview);
    }

    #[tokio::test]
    async fn a_rerun_after_pushed_history_was_rewritten_fails_and_never_forces() {
        let h = Harness::new("sup-diverged");
        let card = h.card("success", "").await;
        let first = h.settled(card).await;
        assert_eq!(first.status, RunStatus::Succeeded, "{:?}", first.error);
        let pushed = git_run(&h.remote.origin, &["rev-parse", "helm/HELM-1"]);

        let worktree = h.remote.worktrees.join("HELM-1");
        git_run(&worktree, &["commit", "-q", "--amend", "-m", "Rewritten"]);
        h.requeue(card).await;
        let second = h
            .wait_for(card, |run| run.id != first.id && !run.status.is_active())
            .await;

        assert_eq!(second.status, RunStatus::Failed, "{:?}", second.error);
        assert!(
            second.error.as_deref().unwrap().contains("ne force jamais"),
            "{:?}",
            second.error
        );
        assert!(second.pushed_at.is_none());
        assert_eq!(
            git_run(&h.remote.origin, &["rev-parse", "helm/HELM-1"]),
            pushed,
            "origin's branch is untouched"
        );
        assert_eq!(h.column_category(card).await, Category::InProgress);
    }

    #[tokio::test]
    async fn cancelling_a_running_run_stops_the_process_and_leaves_the_card_in_progress() {
        let h = Harness::new("sup-cancel");
        let card = h.card("hang", "").await;
        let running = h
            .wait_for(card, |run| {
                run.status == RunStatus::Running && run.pid.is_some()
            })
            .await;
        // Wait until the agent has produced its first event, so it is really mid-run.
        for _ in 0..200 {
            if !h
                .events(running.id)
                .await
                .iter()
                .all(|(k, _)| *k == EventKind::Notice)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        h.orchestrator.cancel(&h.db, running.id).await.unwrap();
        let run = h.settled(card).await;

        assert_eq!(run.status, RunStatus::Cancelled);
        assert_eq!(h.column_category(card).await, Category::InProgress);
        assert!(run.pushed_at.is_none());
        let pid = running.pid.unwrap();
        assert!(
            !process_exists(pid) || is_zombie(pid),
            "the agent process is gone"
        );
        assert!(h.comments(card).await[0].contains("annulée"));
        // Cancelling again is refused: the run is final.
        assert!(matches!(
            h.orchestrator.cancel(&h.db, running.id).await,
            Err(StoreError::IllegalTransition {
                from: RunStatus::Cancelled,
                ..
            })
        ));
    }

    fn process_exists(pid: i64) -> bool {
        let Ok(pid) = i32::try_from(pid) else {
            return false;
        };
        // SAFETY: signal 0 only checks that the process exists and can be signalled.
        let rc = unsafe { libc::kill(pid, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    enum Stop {
        Cancel,
        Shutdown,
    }

    fn gone(pid: i64) -> bool {
        !process_exists(pid) || is_zombie(pid)
    }

    async fn wait_until_gone(pid: i64) {
        for _ in 0..200 {
            if gone(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("process {pid} outlived its run");
    }

    async fn assert_stop_reaps_the_agents_children(name: &str, scenario: &str, stop: Stop) {
        let h = Harness::new(name);
        let card = h.card(scenario, "").await;
        let running = h.wait_for(card, |r| r.status == RunStatus::Running).await;
        let child = h.child_pid("HELM-1").await;
        assert!(!gone(child), "the child runs before the stop");

        match stop {
            Stop::Cancel => {
                h.orchestrator.cancel(&h.db, running.id).await.unwrap();
            }
            Stop::Shutdown => h.orchestrator.shutdown().await,
        }
        let run = h.settled(card).await;

        let expected = match stop {
            Stop::Cancel => RunStatus::Cancelled,
            Stop::Shutdown => RunStatus::Interrupted,
        };
        assert_eq!(run.status, expected, "{scenario}: {:?}", run.error);
        wait_until_gone(child).await;
    }

    #[tokio::test]
    async fn cancelling_a_run_stops_the_agents_children_too() {
        assert_stop_reaps_the_agents_children("sup-cancel-group", "group_child", Stop::Cancel)
            .await;
        assert_stop_reaps_the_agents_children("sup-cancel-session", "session_child", Stop::Cancel)
            .await;
    }

    #[tokio::test]
    async fn shutting_down_stops_the_agents_children_too_and_interrupts_the_run() {
        assert_stop_reaps_the_agents_children("sup-stop-group", "group_child", Stop::Shutdown)
            .await;
        assert_stop_reaps_the_agents_children("sup-stop-session", "session_child", Stop::Shutdown)
            .await;
    }

    fn is_zombie(pid: i64) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| stat.contains(") Z "))
    }

    #[tokio::test]
    async fn the_concurrency_limit_holds_runs_in_the_queue_until_a_slot_frees() {
        let h = Harness::with_limit("sup-limit", 1);
        let first = h.card("hang", "").await;
        let second = h.card("success", "").await;

        let running = h.wait_for(first, |r| r.status == RunStatus::Running).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            h.latest(second).await.status,
            RunStatus::Queued,
            "no free slot"
        );

        h.orchestrator.cancel(&h.db, running.id).await.unwrap();
        let run = h.settled(second).await;
        assert_eq!(run.status, RunStatus::Succeeded, "{:?}", run.error);
    }

    #[tokio::test]
    async fn a_queued_run_is_cancelled_directly_and_leaving_todo_cancels_it_too() {
        let h = Harness::with_limit("sup-queued", 1);
        let blocker = h.card("hang", "").await;
        let blocking = h
            .wait_for(blocker, |r| r.status == RunStatus::Running)
            .await;
        let queued_card = h.card("success", "").await;
        let queued = h.latest(queued_card).await;
        assert_eq!(queued.status, RunStatus::Queued);

        // Dragging the card out of "À faire" withdraws the request.
        let orchestrator = h.orchestrator.clone();
        h.db.call(move |conn| {
            let entered = store::move_card(conn, queued_card, 1, 0)?;
            orchestrator.after_placement(conn, queued_card, entered)
        })
        .await
        .unwrap();
        assert_eq!(h.latest(queued_card).await.status, RunStatus::Cancelled);

        let other = h.card("success", "").await;
        let queued = h.latest(other).await;
        assert_eq!(
            h.orchestrator.cancel(&h.db, queued.id).await.unwrap(),
            other
        );
        assert_eq!(h.latest(other).await.status, RunStatus::Cancelled);

        h.orchestrator.cancel(&h.db, blocking.id).await.unwrap();
        h.settled(blocker).await;
    }

    #[tokio::test]
    async fn only_entering_a_todo_column_with_an_agent_queues_a_run() {
        let remote = Remote::new("sup-trigger");
        let _ = &remote;
        let orchestrator = Orchestrator::new(
            RunGate::Open,
            RunDefaults {
                permission_mode: PermissionMode::DEFAULT,
                model: None,
            },
        );
        let mut conn = crate::db::Db::test_connection();
        let make = |conn: &mut Connection, column: i64, agent: &str| {
            store::create_card(
                conn,
                column,
                &CardInput {
                    title: "T".to_owned(),
                    agent: agent.to_owned(),
                    ..CardInput::default()
                },
            )
            .unwrap()
        };
        let runs_of = |conn: &Connection, card: i64| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM agent_runs WHERE card_id = ?1",
                [card],
                |r| r.get(0),
            )
            .unwrap()
        };

        // No agent: nothing. Agent in backlog: nothing. Agent in todo: one run.
        let plain = make(&mut conn, 2, "");
        orchestrator
            .after_placement(&mut conn, plain, true)
            .unwrap();
        let backlog = make(&mut conn, 1, "claude");
        orchestrator
            .after_placement(&mut conn, backlog, true)
            .unwrap();
        assert_eq!((runs_of(&conn, plain), runs_of(&conn, backlog)), (0, 0));

        let entered = store::move_card(&mut conn, backlog, 2, 0).unwrap();
        orchestrator
            .after_placement(&mut conn, backlog, entered)
            .unwrap();
        assert_eq!(runs_of(&conn, backlog), 1);

        // Reordering inside the column, or saving the form, does not queue another.
        let entered = store::move_card(&mut conn, backlog, 2, 1).unwrap();
        assert!(!entered);
        orchestrator
            .after_placement(&mut conn, backlog, entered)
            .unwrap();
        assert_eq!(runs_of(&conn, backlog), 1);

        // A closed gate queues nothing.
        let closed = Orchestrator::new(
            RunGate::NotLoopback,
            RunDefaults {
                permission_mode: PermissionMode::DEFAULT,
                model: None,
            },
        );
        let card = make(&mut conn, 2, "claude");
        closed.after_placement(&mut conn, card, true).unwrap();
        assert_eq!(runs_of(&conn, card), 0);
    }

    #[tokio::test]
    async fn a_run_left_running_by_a_dead_helm_becomes_interrupted_and_is_never_relaunched() {
        let mut h = Harness::new("sup-recover");
        h.task.abort();
        let card = h.card("success", "").await;
        // Simulate the previous Helm: the run was claimed, its process is gone.
        let id =
            h.db.call(|conn| {
                let run = runs::claim_next_queued(conn)?.unwrap();
                runs::record_pid(conn, run.id, 2_000_000_000)?;
                Ok::<_, StoreError>(run.id)
            })
            .await
            .unwrap();

        let count = recover(&h.db, &h.changes).await.unwrap();

        assert_eq!(count, 1);
        let run = h.latest(card).await;
        assert_eq!((run.id, run.status), (id, RunStatus::Interrupted));
        assert!(run.error.as_deref().unwrap().contains("Helm s'est arrêté"));
        assert!(run.finished_at.is_some());
        assert!(h.comments(card).await[0].contains("interrompue"));
        assert_eq!(recover(&h.db, &h.changes).await.unwrap(), 0, "idempotent");

        // A fresh supervisor does not pick the interrupted run up again.
        h.restart();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(h.latest(card).await.status, RunStatus::Interrupted);
        assert!(!h.remote.worktrees.join("HELM-1").exists());
    }

    #[tokio::test]
    async fn a_new_run_is_refused_while_the_previous_agent_still_lives_in_the_worktree() {
        let mut h = Harness::new("sup-orphan");
        h.task.abort();
        let card = h.card("success", "").await;
        use std::os::unix::process::CommandExt;
        let mut orphan = std::process::Command::new("sleep")
            .arg("301")
            .process_group(0)
            .spawn()
            .unwrap();
        let orphan_id = orphan.id();
        let orphan_pid = i64::from(orphan_id);
        h.db.call(move |conn| {
            let run = runs::claim_next_queued(conn)?.unwrap();
            runs::record_pid(conn, run.id, orphan_id)?;
            Ok::<_, StoreError>(())
        })
        .await
        .unwrap();
        recover(&h.db, &h.changes).await.unwrap();
        h.restart();

        let interrupted = h.latest(card).await;
        assert_eq!(interrupted.status, RunStatus::Interrupted);
        h.requeue(card).await;
        let refused = h
            .wait_for(card, |r| r.id > interrupted.id && !r.status.is_active())
            .await;
        assert_eq!(refused.status, RunStatus::Failed, "{:?}", refused.error);

        let error = refused.error.as_deref().unwrap();
        assert!(
            error.contains(&format!("kill -- -{orphan_pid}")),
            "the card says how to stop the old agent: {error}"
        );
        assert!(refused.pid.is_none(), "no second agent was launched");
        assert!(!h.remote.worktrees.join("HELM-1").exists());
        assert!(
            group_exists(orphan_pid),
            "Helm does not kill a process it cannot identify"
        );

        assert!(
            error.contains(&format!("pgrep -a -g {orphan_pid}")),
            "the card says to check what the process is first: {error}"
        );

        // The refusal is itself the latest run, and it must not lift the block.
        h.requeue(card).await;
        let refused_again = h
            .wait_for(card, |r| r.id > refused.id && !r.status.is_active())
            .await;
        assert_eq!(
            refused_again.status,
            RunStatus::Failed,
            "{:?}",
            refused_again.error
        );
        assert!(refused_again.pid.is_none());

        orphan.kill().unwrap();
        orphan.wait().unwrap();
        h.requeue(card).await;
        let run = h
            .wait_for(card, |r| r.id > refused_again.id && !r.status.is_active())
            .await;
        assert_eq!(run.status, RunStatus::Succeeded, "{:?}", run.error);
    }

    #[tokio::test]
    async fn a_pid_from_before_a_later_run_that_launched_an_agent_no_longer_blocks_the_card() {
        let mut h = Harness::new("sup-stale-pid");
        h.task.abort();
        let card = h.card("success", "").await;
        let interrupted =
            h.db.call(|conn| {
                let run = runs::claim_next_queued(conn)?.unwrap();
                runs::record_pid(conn, run.id, 2_000_000_000)?;
                Ok::<_, StoreError>(run.id)
            })
            .await
            .unwrap();
        recover(&h.db, &h.changes).await.unwrap();
        h.restart();
        h.requeue(card).await;
        let later = h
            .wait_for(card, |r| r.id > interrupted && !r.status.is_active())
            .await;
        assert_eq!(later.status, RunStatus::Succeeded, "{:?}", later.error);
        assert!(later.pid.is_some(), "the later run launched an agent");

        // Long afterwards the old pid names an unrelated, live process group.
        use std::os::unix::process::CommandExt;
        let mut stranger = std::process::Command::new("sleep")
            .arg("302")
            .process_group(0)
            .spawn()
            .unwrap();
        let stranger_pid = stranger.id();
        h.db.call(move |conn| {
            conn.execute(
                "UPDATE agent_runs SET pid = ?2 WHERE id = ?1",
                (interrupted, i64::from(stranger_pid)),
            )?;
            Ok::<_, StoreError>(())
        })
        .await
        .unwrap();
        h.requeue(card).await;
        let run = h
            .wait_for(card, |r| r.id > later.id && !r.status.is_active())
            .await;
        stranger.kill().unwrap();
        stranger.wait().unwrap();

        let reason = run.error.as_deref().unwrap_or_default();
        assert!(
            !reason.contains(&format!("-{stranger_pid}")),
            "an old interrupted run no longer blocks: {reason}"
        );
        assert!(run.pid.is_some(), "the agent was launched: {reason}");
    }

    #[tokio::test]
    async fn a_line_longer_than_the_cap_is_counted_but_only_its_start_is_kept() {
        let long = 50_000_000;
        let stream = tokio::io::repeat(b'x')
            .take(long)
            .chain(&b"\nnext\nlast"[..]);
        let mut reader = BufReader::new(stream);
        let mut line = Line::default();

        assert!(read_line(&mut reader, &mut line, 1024).await.unwrap());
        assert_eq!(
            (line.kept.len(), line.dropped),
            (1024, long as usize + 1 - 1024)
        );
        line.clear();
        assert!(read_line(&mut reader, &mut line, 1024).await.unwrap());
        assert_eq!((line.kept.as_slice(), line.dropped), (&b"next\n"[..], 0));
        line.clear();
        assert!(read_line(&mut reader, &mut line, 1024).await.unwrap());
        assert_eq!(line.kept, b"last");
        line.clear();
        assert!(!read_line(&mut reader, &mut line, 1024).await.unwrap());
    }

    #[test]
    fn the_verdict_needs_the_agent_to_say_so_and_the_process_to_exit_cleanly() {
        let clean = Exit {
            code: Some(0),
            signal: None,
        };
        assert_eq!(judge(clean, Some(&Verdict::Success), ""), Ok(()));
        assert_eq!(
            judge(clean, Some(&Verdict::Failure("no".to_owned())), ""),
            Err("no".to_owned())
        );
        assert!(
            judge(clean, None, "")
                .unwrap_err()
                .contains("sans rapport final")
        );
        let crashed = Exit {
            code: Some(3),
            signal: None,
        };
        assert!(
            judge(crashed, Some(&Verdict::Success), "oops\n\n")
                .unwrap_err()
                .contains("code 3. Dernière ligne d'erreur : oops")
        );
        let killed = Exit {
            code: None,
            signal: Some(9),
        };
        assert!(judge(killed, None, "").unwrap_err().contains("signal 9"));
    }

    #[test]
    fn a_dead_pid_is_not_a_live_process() {
        assert!(!group_exists(2_000_000_000));
        assert!(!group_exists(i64::MAX));
        assert!(!group_exists(0));
        assert!(!group_exists(-1));
    }
}
