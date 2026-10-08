//! Agent runs: the typed model of one launch of an agent on a card, its state machine, and
//! the store operations that move it through that machine.
//!
//! Like `store`, these functions are synchronous and take a plain connection.

use std::fmt;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{Connection, OptionalExtension, Row, Transaction};

use crate::agent::{Agent, ModelName, PermissionMode};
use crate::store::{self, Result, StoreError};

/// Identifies a run; kept apart from card and event ids, which are also plain integers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RunId(pub i64);

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl ToSql for RunId {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

impl FromSql for RunId {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        i64::column_result(value).map(Self)
    }
}

/// Where a run is in its life.
///
/// ```text
/// queued ──▶ running ──▶ succeeded
///   │           ├──────▶ failed
///   │           ├──────▶ cancelled
///   └──▶ cancelled      └▶ interrupted
/// ```
///
/// The four states on the right are final: a run never leaves them, and a new attempt is a
/// new run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
}

impl RunStatus {
    pub const ALL: [Self; 6] = [
        Self::Queued,
        Self::Running,
        Self::Succeeded,
        Self::Failed,
        Self::Cancelled,
        Self::Interrupted,
    ];

    pub const fn slug(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Queued => "En file",
            Self::Running => "En cours",
            Self::Succeeded => "Réussie",
            Self::Failed => "Échec",
            Self::Cancelled => "Annulée",
            Self::Interrupted => "Interrompue",
        }
    }

    /// Queued or running: the states that occupy a card's single run slot.
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }

    pub const fn can_become(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Queued, Self::Running | Self::Cancelled)
                | (
                    Self::Running,
                    Self::Succeeded | Self::Failed | Self::Cancelled | Self::Interrupted
                )
        )
    }
}

impl ToSql for RunStatus {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.slug().to_sql()
    }
}

impl FromSql for RunStatus {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        Self::ALL
            .into_iter()
            .find(|status| status.slug() == text)
            .ok_or_else(|| FromSqlError::Other(format!("unknown run status {text:?}").into()))
    }
}

/// How a running run ended. `Succeeded` is only constructed after the branch was pushed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Succeeded,
    Failed(String),
    Cancelled,
    Interrupted(String),
}

impl Outcome {
    pub const fn status(&self) -> RunStatus {
        match self {
            Self::Succeeded => RunStatus::Succeeded,
            Self::Failed(_) => RunStatus::Failed,
            Self::Cancelled => RunStatus::Cancelled,
            Self::Interrupted(_) => RunStatus::Interrupted,
        }
    }

    fn error(&self) -> Option<&str> {
        match self {
            Self::Failed(reason) | Self::Interrupted(reason) => Some(reason),
            Self::Succeeded | Self::Cancelled => None,
        }
    }
}

/// What an `agent_events` row is. The CLI-specific shape stays in `payload`; this is the
/// normalised classification the card renders from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// The CLI announced the session.
    Init,
    /// Assistant text or reasoning.
    Message,
    ToolUse,
    ToolResult,
    /// The CLI's final verdict, with cost and usage.
    Result,
    /// CLI bookkeeping (hooks, rate limits…): kept, but not worth showing by default.
    System,
    /// Helm's own account of what it did around the process (worktree, push…).
    Notice,
    Error,
    /// A stdout line that was not valid JSON.
    Malformed,
}

impl EventKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Init => "Session",
            Self::Message => "Agent",
            Self::ToolUse => "Outil",
            Self::ToolResult => "Résultat",
            Self::Result => "Fin",
            Self::System => "Système",
            Self::Notice => "Helm",
            Self::Error => "Erreur",
            Self::Malformed => "Illisible",
        }
    }

    pub const ALL: [Self; 9] = [
        Self::Init,
        Self::Message,
        Self::ToolUse,
        Self::ToolResult,
        Self::Result,
        Self::System,
        Self::Notice,
        Self::Error,
        Self::Malformed,
    ];

    pub const fn slug(self) -> &'static str {
        match self {
            Self::Init => "init",
            Self::Message => "message",
            Self::ToolUse => "tool_use",
            Self::ToolResult => "tool_result",
            Self::Result => "result",
            Self::System => "system",
            Self::Notice => "notice",
            Self::Error => "error",
            Self::Malformed => "malformed",
        }
    }
}

impl ToSql for EventKind {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.slug().to_sql()
    }
}

impl FromSql for EventKind {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        Self::ALL
            .into_iter()
            .find(|kind| kind.slug() == text)
            .ok_or_else(|| FromSqlError::Other(format!("unknown event kind {text:?}").into()))
    }
}

/// An event on its way into the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEvent {
    pub kind: EventKind,
    /// One line for the card.
    pub summary: String,
    /// The CLI's raw line, or Helm's text for a `Notice`/`Error`.
    pub payload: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunEvent {
    pub seq: i64,
    pub kind: EventKind,
    pub summary: String,
    pub created_at: i64,
}

impl RunEvent {
    pub fn time_iso(&self) -> String {
        store::utc_iso(self.created_at)
    }

    pub fn time_display(&self) -> String {
        store::utc_time_display(self.created_at)
    }
}

/// How many of the latest events a card shows; the log itself keeps them all.
pub const ACTIVITY_EVENT_LIMIT: i64 = 200;

/// What the card's activity panel shows: the latest run and the tail of its log.
#[derive(Debug, Clone, PartialEq)]
pub struct Activity {
    pub run: Option<Run>,
    /// Oldest first, without CLI bookkeeping.
    pub events: Vec<RunEvent>,
    /// Events of the run in total, shown or not.
    pub total_events: i64,
    /// Database time when this was read, so a running run's duration is current.
    pub now: i64,
}

impl Activity {
    pub fn duration_display(&self) -> String {
        self.run
            .as_ref()
            .map_or(String::new(), |run| run.duration_display(self.now))
    }

    pub fn hidden_events(&self) -> i64 {
        self.total_events - self.events.len() as i64
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    pub id: RunId,
    pub card_id: i64,
    pub agent: Agent,
    pub model: Option<ModelName>,
    pub permission_mode: PermissionMode,
    pub status: RunStatus,
    pub prompt: String,
    pub session_id: Option<String>,
    pub worktree_path: Option<String>,
    pub branch: Option<String>,
    pub pid: Option<i64>,
    pub exit_code: Option<i64>,
    pub error: Option<String>,
    pub stderr: String,
    pub cost_usd: Option<f64>,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
    pub queued_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub pushed_at: Option<i64>,
}

impl Run {
    pub fn cost_display(&self) -> String {
        self.cost_usd.map_or(String::new(), |cost| {
            format!("{cost:.4} $").replace('.', ",")
        })
    }

    pub fn tokens_display(&self) -> String {
        match (self.tokens_in, self.tokens_out) {
            (Some(tokens_in), Some(tokens_out)) => format!("{tokens_in} → {tokens_out}"),
            _ => String::new(),
        }
    }

    /// How long the run took so far, or in total once it is over.
    pub fn duration_display(&self, now: i64) -> String {
        let Some(started) = self.started_at else {
            return String::new();
        };
        let seconds = (self.finished_at.unwrap_or(now) - started).max(0);
        match seconds {
            0..=59 => format!("{seconds} s"),
            _ => format!("{} min {:02} s", seconds / 60, seconds % 60),
        }
    }
}

const RUN_COLUMNS: &str = "id, card_id, agent, model, permission_mode, status, prompt, session_id,
    worktree_path, branch, pid, exit_code, error, stderr, cost_usd, tokens_in, tokens_out,
    queued_at, started_at, finished_at, pushed_at";

fn run_from_row(row: &Row<'_>) -> rusqlite::Result<Run> {
    Ok(Run {
        id: row.get(0)?,
        card_id: row.get(1)?,
        agent: row.get(2)?,
        model: row.get(3)?,
        permission_mode: row.get(4)?,
        status: row.get(5)?,
        prompt: row.get(6)?,
        session_id: row.get(7)?,
        worktree_path: row.get(8)?,
        branch: row.get(9)?,
        pid: row.get(10)?,
        exit_code: row.get(11)?,
        error: row.get(12)?,
        stderr: row.get(13)?,
        cost_usd: row.get(14)?,
        tokens_in: row.get(15)?,
        tokens_out: row.get(16)?,
        queued_at: row.get(17)?,
        started_at: row.get(18)?,
        finished_at: row.get(19)?,
        pushed_at: row.get(20)?,
    })
}

/// What a new run is launched with, fixed when it is queued.
#[derive(Debug, Clone)]
pub struct NewRun<'a> {
    pub card_id: i64,
    pub agent: Agent,
    pub model: Option<&'a ModelName>,
    pub permission_mode: PermissionMode,
    pub prompt: &'a str,
}

/// Queues a run. `None` means the card already has an active run: its slot is taken.
pub fn enqueue(conn: &mut Connection, new: &NewRun<'_>) -> Result<Option<RunId>> {
    let id = conn
        .query_row(
            "INSERT INTO agent_runs (card_id, agent, model, permission_mode, prompt)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (card_id) WHERE status IN ('queued', 'running') DO NOTHING
             RETURNING id",
            (
                new.card_id,
                new.agent,
                new.model,
                new.permission_mode,
                new.prompt,
            ),
            |row| row.get(0),
        )
        .optional()?;
    Ok(id)
}

pub fn get_run(conn: &Connection, id: RunId) -> Result<Run> {
    conn.query_row(
        &format!("SELECT {RUN_COLUMNS} FROM agent_runs WHERE id = ?1"),
        [id],
        run_from_row,
    )
    .optional()?
    .ok_or(StoreError::NotFound)
}

/// The card's most recent run, whatever its state.
pub fn latest_run(conn: &Connection, card_id: i64) -> Result<Option<Run>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {RUN_COLUMNS} FROM agent_runs WHERE card_id = ?1 ORDER BY id DESC LIMIT 1"
            ),
            [card_id],
            run_from_row,
        )
        .optional()?)
}

/// The oldest queued run, now `running`: the supervisor's only way to start work.
pub fn claim_next_queued(conn: &mut Connection) -> Result<Option<Run>> {
    let tx = conn.transaction()?;
    let next: Option<RunId> = tx
        .query_row(
            "SELECT id FROM agent_runs WHERE status = 'queued' ORDER BY id LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(id) = next else {
        return Ok(None);
    };
    change_status(&tx, id, RunStatus::Running, None)?;
    let run = get_run(&tx, id)?;
    tx.commit()?;
    Ok(Some(run))
}

/// Ends a queued or running run. Illegal moves (finishing a finished run, succeeding a run
/// that never started) are rejected, not applied.
pub fn finish(conn: &mut Connection, id: RunId, outcome: &Outcome) -> Result<()> {
    let tx = conn.transaction()?;
    change_status(&tx, id, outcome.status(), outcome.error())?;
    tx.commit()?;
    Ok(())
}

/// The single place a run's status changes: every transition is checked against
/// [`RunStatus::can_become`] and applied only if the row is still in the state it was read in.
fn change_status(
    tx: &Transaction<'_>,
    id: RunId,
    to: RunStatus,
    error: Option<&str>,
) -> Result<()> {
    let from: RunStatus = tx
        .query_row("SELECT status FROM agent_runs WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .optional()?
        .ok_or(StoreError::NotFound)?;
    if !from.can_become(to) {
        return Err(StoreError::IllegalTransition { from, to });
    }
    let changed = tx.execute(
        "UPDATE agent_runs SET
             status = ?2,
             error = ?3,
             started_at  = CASE WHEN ?2 = 'running' THEN unixepoch() ELSE started_at END,
             finished_at = CASE WHEN ?2 IN ('queued', 'running') THEN NULL ELSE unixepoch() END,
             pushed_at   = CASE WHEN ?2 = 'succeeded' THEN unixepoch() ELSE pushed_at END
         WHERE id = ?1 AND status = ?4",
        (id, to, error, from),
    )?;
    if changed == 0 {
        return Err(StoreError::IllegalTransition { from, to });
    }
    Ok(())
}

/// Where the run works, once its worktree exists.
pub fn record_workspace(conn: &Connection, id: RunId, path: &str, branch: &str) -> Result<()> {
    conn.execute(
        "UPDATE agent_runs SET worktree_path = ?2, branch = ?3 WHERE id = ?1",
        (id, path, branch),
    )?;
    Ok(())
}

pub fn record_pid(conn: &Connection, id: RunId, pid: u32) -> Result<()> {
    conn.execute(
        "UPDATE agent_runs SET pid = ?2 WHERE id = ?1",
        (id, i64::from(pid)),
    )?;
    Ok(())
}

/// Keeps the first session id: it is the one a resume would name.
pub fn record_session(conn: &Connection, id: RunId, session_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE agent_runs SET session_id = COALESCE(session_id, ?2) WHERE id = ?1",
        (id, session_id),
    )?;
    Ok(())
}

pub fn record_usage(
    conn: &Connection,
    id: RunId,
    cost_usd: Option<f64>,
    tokens_in: Option<i64>,
    tokens_out: Option<i64>,
) -> Result<()> {
    conn.execute(
        "UPDATE agent_runs SET cost_usd = ?2, tokens_in = ?3, tokens_out = ?4 WHERE id = ?1",
        (id, cost_usd, tokens_in, tokens_out),
    )?;
    Ok(())
}

pub fn record_exit(
    conn: &Connection,
    id: RunId,
    exit_code: Option<i32>,
    stderr: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE agent_runs SET exit_code = ?2, stderr = ?3 WHERE id = ?1",
        (id, exit_code, stderr),
    )?;
    Ok(())
}

/// Cancels the card's queued run, if any. A running one is the supervisor's to stop.
pub fn cancel_queued(conn: &mut Connection, card_id: i64) -> Result<bool> {
    let queued: Option<RunId> = conn
        .query_row(
            "SELECT id FROM agent_runs WHERE card_id = ?1 AND status = 'queued'",
            [card_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(id) = queued else {
        return Ok(false);
    };
    finish(conn, id, &Outcome::Cancelled)?;
    Ok(true)
}

/// A run that was `running` when Helm stopped, with the pid it was last known by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Orphan {
    pub id: RunId,
    pub card_id: i64,
    pub pid: Option<i64>,
}

pub fn running_runs(conn: &Connection) -> Result<Vec<Orphan>> {
    Ok(conn
        .prepare("SELECT id, card_id, pid FROM agent_runs WHERE status = 'running' ORDER BY id")?
        .query_map([], |row| {
            Ok(Orphan {
                id: row.get(0)?,
                card_id: row.get(1)?,
                pid: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

/// The pids of the card's earlier runs that a killed Helm left `interrupted`: the only runs
/// whose agent may still be alive, since every other end signals the agent's group.
pub fn interrupted_pids(conn: &Connection, card_id: i64, before: RunId) -> Result<Vec<i64>> {
    Ok(conn
        .prepare(
            "SELECT pid FROM agent_runs
             WHERE card_id = ?1 AND id < ?2 AND status = 'interrupted' AND pid IS NOT NULL
             ORDER BY id DESC",
        )?
        .query_map((card_id, before), |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

/// Appends to a run's log and returns the event's `seq`.
pub fn append_event(conn: &Connection, run: RunId, event: &NewEvent) -> Result<i64> {
    Ok(conn.query_row(
        "INSERT INTO agent_events (run_id, seq, kind, summary, payload)
         SELECT ?1, COALESCE(MAX(seq), 0) + 1, ?2, ?3, ?4 FROM agent_events WHERE run_id = ?1
         RETURNING seq",
        (run, event.kind, &event.summary, &event.payload),
        |row| row.get(0),
    )?)
}

/// The run's events in order. `System` bookkeeping is left out and only the latest `limit`
/// are returned.
pub fn list_events(conn: &Connection, run: RunId, limit: i64) -> Result<Vec<RunEvent>> {
    let mut events: Vec<RunEvent> = conn
        .prepare(
            "SELECT seq, kind, summary, created_at FROM agent_events
             WHERE run_id = ?1 AND kind <> 'system' ORDER BY seq DESC LIMIT ?2",
        )?
        .query_map((run, limit), |row| {
            Ok(RunEvent {
                seq: row.get(0)?,
                kind: row.get(1)?,
                summary: row.get(2)?,
                created_at: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    events.reverse();
    Ok(events)
}

pub fn activity(conn: &Connection, card_id: i64) -> Result<Activity> {
    let now = conn.query_row("SELECT unixepoch()", [], |row| row.get(0))?;
    let Some(run) = latest_run(conn, card_id)? else {
        return Ok(Activity {
            run: None,
            events: Vec::new(),
            total_events: 0,
            now,
        });
    };
    let events = list_events(conn, run.id, ACTIVITY_EVENT_LIMIT)?;
    let total_events = conn.query_row(
        "SELECT COUNT(*) FROM agent_events WHERE run_id = ?1",
        [run.id],
        |row| row.get(0),
    )?;
    Ok(Activity {
        run: Some(run),
        events,
        total_events,
        now,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::store::{self, CardInput};

    fn conn_with_card() -> (Connection, i64) {
        let mut conn = Db::test_connection();
        let card = store::create_card(
            &mut conn,
            2,
            &CardInput {
                title: "Task".to_owned(),
                ..CardInput::default()
            },
        )
        .unwrap();
        (conn, card)
    }

    fn queue(conn: &mut Connection, card: i64, prompt: &str) -> Option<RunId> {
        enqueue(
            conn,
            &NewRun {
                card_id: card,
                agent: Agent::Claude,
                model: None,
                permission_mode: PermissionMode::DEFAULT,
                prompt,
            },
        )
        .unwrap()
    }

    #[test]
    fn the_state_machine_allows_exactly_the_documented_transitions() {
        use RunStatus::*;
        let legal = [
            (Queued, Running),
            (Queued, Cancelled),
            (Running, Succeeded),
            (Running, Failed),
            (Running, Cancelled),
            (Running, Interrupted),
        ];
        for from in RunStatus::ALL {
            for to in RunStatus::ALL {
                assert_eq!(
                    from.can_become(to),
                    legal.contains(&(from, to)),
                    "{from:?} -> {to:?}"
                );
            }
        }
        for status in RunStatus::ALL {
            assert_eq!(
                status.is_active(),
                matches!(status, Queued | Running),
                "{status:?}"
            );
        }
    }

    #[test]
    fn statuses_and_event_kinds_round_trip_through_the_database() {
        let conn = Db::test_connection();
        for status in RunStatus::ALL {
            let back: RunStatus = conn.query_row("SELECT ?1", [status], |r| r.get(0)).unwrap();
            assert_eq!(back, status);
        }
        for kind in EventKind::ALL {
            let back: EventKind = conn.query_row("SELECT ?1", [kind], |r| r.get(0)).unwrap();
            assert_eq!(back, kind);
        }
        assert!(
            conn.query_row("SELECT 'paused'", [], |r| r.get::<_, RunStatus>(0))
                .is_err()
        );
    }

    #[test]
    fn a_card_has_at_most_one_active_run_and_a_finished_one_frees_the_slot() {
        let (mut conn, card) = conn_with_card();
        let first = queue(&mut conn, card, "one").unwrap();
        assert_eq!(queue(&mut conn, card, "two"), None);

        claim_next_queued(&mut conn).unwrap().unwrap();
        assert_eq!(
            queue(&mut conn, card, "two"),
            None,
            "running still holds the slot"
        );

        finish(&mut conn, first, &Outcome::Failed("boom".to_owned())).unwrap();
        let second = queue(&mut conn, card, "two").unwrap();
        assert!(second > first);
        assert_eq!(latest_run(&conn, card).unwrap().unwrap().id, second);
    }

    #[test]
    fn claiming_starts_the_oldest_queued_run_once() {
        let mut conn = Db::test_connection();
        let cards: Vec<i64> = ["A", "B"]
            .iter()
            .map(|title| {
                store::create_card(
                    &mut conn,
                    2,
                    &CardInput {
                        title: (*title).to_owned(),
                        ..CardInput::default()
                    },
                )
                .unwrap()
            })
            .collect();
        let a = queue(&mut conn, cards[0], "a").unwrap();
        let b = queue(&mut conn, cards[1], "b").unwrap();

        let first = claim_next_queued(&mut conn).unwrap().unwrap();
        assert_eq!((first.id, first.status), (a, RunStatus::Running));
        assert!(first.started_at.is_some() && first.finished_at.is_none());
        assert_eq!(claim_next_queued(&mut conn).unwrap().unwrap().id, b);
        assert!(claim_next_queued(&mut conn).unwrap().is_none());
    }

    #[test]
    fn finishing_records_the_outcome_and_final_states_are_final() {
        let (mut conn, card) = conn_with_card();
        let id = queue(&mut conn, card, "p").unwrap();

        // Nothing but running can succeed or fail.
        for outcome in [Outcome::Succeeded, Outcome::Failed("x".into())] {
            assert!(matches!(
                finish(&mut conn, id, &outcome),
                Err(StoreError::IllegalTransition {
                    from: RunStatus::Queued,
                    ..
                })
            ));
        }
        assert_eq!(get_run(&conn, id).unwrap().status, RunStatus::Queued);

        claim_next_queued(&mut conn).unwrap();
        finish(&mut conn, id, &Outcome::Failed("push failed".to_owned())).unwrap();
        let run = get_run(&conn, id).unwrap();
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(run.error.as_deref(), Some("push failed"));
        assert!(run.finished_at.is_some() && run.pushed_at.is_none());

        for outcome in [
            Outcome::Succeeded,
            Outcome::Cancelled,
            Outcome::Interrupted("again".to_owned()),
        ] {
            assert!(matches!(
                finish(&mut conn, id, &outcome),
                Err(StoreError::IllegalTransition {
                    from: RunStatus::Failed,
                    ..
                })
            ));
        }
        assert_eq!(
            get_run(&conn, id).unwrap().error.as_deref(),
            Some("push failed")
        );
    }

    #[test]
    fn only_a_pushed_run_can_be_recorded_as_succeeded() {
        let (mut conn, card) = conn_with_card();
        let id = queue(&mut conn, card, "p").unwrap();
        claim_next_queued(&mut conn).unwrap();
        finish(&mut conn, id, &Outcome::Succeeded).unwrap();
        let run = get_run(&conn, id).unwrap();
        assert_eq!(run.status, RunStatus::Succeeded);
        assert!(run.pushed_at.is_some());

        // The database refuses the contradiction even if the application code were wrong.
        let other = queue(&mut conn, card, "again").unwrap();
        let skipped_push = conn.execute(
            "UPDATE agent_runs SET status = 'succeeded', finished_at = unixepoch() WHERE id = ?1",
            [other],
        );
        assert!(skipped_push.is_err());
    }

    #[test]
    fn a_queued_run_can_be_cancelled_and_unknown_runs_are_not_found() {
        let (mut conn, card) = conn_with_card();
        let id = queue(&mut conn, card, "p").unwrap();
        finish(&mut conn, id, &Outcome::Cancelled).unwrap();
        let run = get_run(&conn, id).unwrap();
        assert_eq!(run.status, RunStatus::Cancelled);
        assert!(run.started_at.is_none() && run.finished_at.is_some());
        assert!(matches!(
            finish(&mut conn, RunId(999), &Outcome::Cancelled),
            Err(StoreError::NotFound)
        ));
        assert!(matches!(
            get_run(&conn, RunId(999)),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn progress_is_recorded_on_the_run_and_the_first_session_id_wins() {
        let (mut conn, card) = conn_with_card();
        let id = queue(&mut conn, card, "p").unwrap();
        claim_next_queued(&mut conn).unwrap();

        record_workspace(&conn, id, "/wt/HELM-1", "helm/HELM-1").unwrap();
        record_pid(&conn, id, 4242).unwrap();
        record_session(&conn, id, "first").unwrap();
        record_session(&conn, id, "second").unwrap();
        record_usage(&conn, id, Some(0.25), Some(10), Some(20)).unwrap();
        record_exit(&conn, id, Some(0), "warn\n").unwrap();

        let run = get_run(&conn, id).unwrap();
        assert_eq!(run.worktree_path.as_deref(), Some("/wt/HELM-1"));
        assert_eq!(run.branch.as_deref(), Some("helm/HELM-1"));
        assert_eq!(run.pid, Some(4242));
        assert_eq!(run.session_id.as_deref(), Some("first"));
        assert_eq!(
            (run.cost_usd, run.tokens_in, run.tokens_out),
            (Some(0.25), Some(10), Some(20))
        );
        assert_eq!((run.exit_code, run.stderr.as_str()), (Some(0), "warn\n"));
    }

    #[test]
    fn only_a_queued_run_is_cancelled_by_cancel_queued() {
        let (mut conn, card) = conn_with_card();
        assert!(!cancel_queued(&mut conn, card).unwrap());
        let id = queue(&mut conn, card, "p").unwrap();
        assert!(cancel_queued(&mut conn, card).unwrap());
        assert_eq!(get_run(&conn, id).unwrap().status, RunStatus::Cancelled);

        let id = queue(&mut conn, card, "q").unwrap();
        claim_next_queued(&mut conn).unwrap();
        assert!(!cancel_queued(&mut conn, card).unwrap());
        assert_eq!(get_run(&conn, id).unwrap().status, RunStatus::Running);
        assert_eq!(running_runs(&conn).unwrap().len(), 1);
    }

    #[test]
    fn events_are_numbered_per_run_and_listed_in_order() {
        let (mut conn, card) = conn_with_card();
        let first = queue(&mut conn, card, "p").unwrap();
        finish(&mut conn, first, &Outcome::Cancelled).unwrap();
        let second = queue(&mut conn, card, "q").unwrap();
        let event = |kind, summary: &str| NewEvent {
            kind,
            summary: summary.to_owned(),
            payload: format!("{{\"s\":\"{summary}\"}}"),
        };

        assert_eq!(
            append_event(&conn, first, &event(EventKind::Init, "a")).unwrap(),
            1
        );
        assert_eq!(
            append_event(&conn, second, &event(EventKind::Init, "b")).unwrap(),
            1
        );
        assert_eq!(
            append_event(&conn, second, &event(EventKind::ToolUse, "c")).unwrap(),
            2
        );

        let events = list_events(&conn, second, 100).unwrap();
        let seen: Vec<(i64, EventKind, &str)> = events
            .iter()
            .map(|e| (e.seq, e.kind, e.summary.as_str()))
            .collect();
        assert_eq!(
            seen,
            [(1, EventKind::Init, "b"), (2, EventKind::ToolUse, "c")]
        );
        assert_eq!(list_events(&conn, first, 100).unwrap().len(), 1);
    }

    #[test]
    fn deleting_a_card_removes_its_runs_and_their_events() {
        let (mut conn, card) = conn_with_card();
        let id = queue(&mut conn, card, "p").unwrap();
        finish(&mut conn, id, &Outcome::Cancelled).unwrap();
        append_event(
            &conn,
            id,
            &NewEvent {
                kind: EventKind::Notice,
                summary: "n".to_owned(),
                payload: "n".to_owned(),
            },
        )
        .unwrap();

        store::delete_card(&mut conn, card).unwrap();

        for table in ["agent_runs", "agent_events"] {
            let rows: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(rows, 0, "{table}");
        }
    }

    #[test]
    fn a_card_with_an_active_run_cannot_be_deleted() {
        let (mut conn, card) = conn_with_card();
        let id = queue(&mut conn, card, "p").unwrap();
        assert!(matches!(
            store::delete_card(&mut conn, card),
            Err(StoreError::Invalid(_))
        ));
        assert!(store::get_card(&conn, card).is_ok());

        finish(&mut conn, id, &Outcome::Cancelled).unwrap();
        store::delete_card(&mut conn, card).unwrap();
    }
}
