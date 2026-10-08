//! Commit-bound verification results, stored independently from agent output.

use std::path::Path;
use std::time::Duration;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{Connection, OptionalExtension};

use crate::process::CommandOutput;
use crate::runs::RunId;
use crate::store::{Result, StoreError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationStatus {
    Running,
    Passed,
    Failed,
    Skipped,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Pending,
    Running,
    Passed,
    Failed,
    Interrupted,
}

impl VerificationStatus {
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::Interrupted => "interrupted",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Running => "En cours",
            Self::Passed => "Réussies",
            Self::Failed => "Échec",
            Self::Skipped => "Non exécutées",
            Self::Interrupted => "Interrompues",
        }
    }
}

impl CheckStatus {
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Pending => "En attente",
            Self::Running => "En cours",
            Self::Passed => "Réussi",
            Self::Failed => "Échec",
            Self::Interrupted => "Interrompu",
        }
    }
}

impl ToSql for VerificationStatus {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.slug().to_sql()
    }
}

impl FromSql for VerificationStatus {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value.as_str()? {
            "running" => Ok(Self::Running),
            "passed" => Ok(Self::Passed),
            "failed" => Ok(Self::Failed),
            "skipped" => Ok(Self::Skipped),
            "interrupted" => Ok(Self::Interrupted),
            value => Err(FromSqlError::Other(
                format!("unknown verification status {value:?}").into(),
            )),
        }
    }
}

impl ToSql for CheckStatus {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.slug().to_sql()
    }
}

impl FromSql for CheckStatus {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value.as_str()? {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "passed" => Ok(Self::Passed),
            "failed" => Ok(Self::Failed),
            "interrupted" => Ok(Self::Interrupted),
            value => Err(FromSqlError::Other(
                format!("unknown check status {value:?}").into(),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verification {
    pub commit_sha: String,
    pub status: VerificationStatus,
    pub checks: Vec<CheckResult>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    pub position: i64,
    pub command: String,
    pub status: CheckStatus,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
}

pub fn start(
    conn: &Connection,
    run_id: RunId,
    commit_sha: &str,
    commands: &[String],
) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    let status = if commands.is_empty() {
        VerificationStatus::Skipped
    } else {
        VerificationStatus::Running
    };
    tx.execute(
        "INSERT INTO run_verifications (run_id, commit_sha, status) VALUES (?1, ?2, ?3)",
        (run_id, commit_sha, status),
    )?;
    for (position, command) in commands.iter().enumerate() {
        tx.execute(
            "INSERT INTO run_checks (run_id, position, command) VALUES (?1, ?2, ?3)",
            (run_id, position as i64, command),
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub fn start_check(conn: &Connection, run_id: RunId, position: i64) -> Result<()> {
    let changed = conn.execute(
        "UPDATE run_checks SET status = 'running' WHERE run_id = ?1 AND position = ?2
         AND status = 'pending' AND EXISTS (
             SELECT 1 FROM run_verifications WHERE run_id = ?1 AND status = 'running'
         )",
        (run_id, position),
    )?;
    require_changed(changed)
}

pub fn finish_check(
    conn: &Connection,
    run_id: RunId,
    position: i64,
    output: &CommandOutput,
) -> Result<()> {
    let status = if output.success() {
        CheckStatus::Passed
    } else {
        CheckStatus::Failed
    };
    let error = if output.timed_out {
        Some("Le délai maximal de la commande a été dépassé.")
    } else if output.exit_code.is_none() {
        Some("La commande s'est arrêtée sans code de retour.")
    } else {
        None
    };
    let changed = conn.execute(
        "UPDATE run_checks SET status = ?3, stdout = ?4, stderr = ?5, exit_code = ?6, error = ?7
         WHERE run_id = ?1 AND position = ?2 AND status = 'running' AND EXISTS (
             SELECT 1 FROM run_verifications WHERE run_id = ?1 AND status = 'running'
         )",
        (
            run_id,
            position,
            status,
            &output.stdout,
            &output.stderr,
            output.exit_code,
            error,
        ),
    )?;
    require_changed(changed)
}

pub fn finish(conn: &Connection, run_id: RunId, status: VerificationStatus) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    let current: VerificationStatus = tx
        .query_row(
            "SELECT status FROM run_verifications WHERE run_id = ?1",
            [run_id],
            |row| row.get(0),
        )
        .optional()?
        .ok_or(StoreError::NotFound)?;
    if current != VerificationStatus::Running {
        // Run cancellation may race with the already completed verification.
        return if current == status || status == VerificationStatus::Interrupted {
            Ok(())
        } else {
            Err(StoreError::Invalid(
                "Les vérifications sont déjà terminées.".into(),
            ))
        };
    }
    let (total, passed): (i64, i64) = tx.query_row(
        "SELECT COUNT(*), COALESCE(SUM(status = 'passed'), 0) FROM run_checks WHERE run_id = ?1",
        [run_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if status == VerificationStatus::Running
        || (status == VerificationStatus::Passed && (total == 0 || passed != total))
        || (status == VerificationStatus::Skipped && total != 0)
    {
        return Err(StoreError::Invalid(
            "L'état demandé ne correspond pas aux résultats des vérifications.".into(),
        ));
    }
    if status == VerificationStatus::Interrupted {
        tx.execute("UPDATE run_checks SET status = 'interrupted' WHERE run_id = ?1 AND status IN ('pending', 'running')", [run_id])?;
    }
    tx.execute(
        "UPDATE run_verifications SET status = ?2 WHERE run_id = ?1",
        (run_id, status),
    )?;
    tx.commit()?;
    Ok(())
}

pub fn get(conn: &Connection, run_id: RunId) -> Result<Option<Verification>> {
    let verification = conn
        .query_row(
            "SELECT commit_sha, status FROM run_verifications WHERE run_id = ?1",
            [run_id],
            |row| {
                Ok(Verification {
                    commit_sha: row.get(0)?,
                    status: row.get(1)?,
                    checks: Vec::new(),
                })
            },
        )
        .optional()?;
    let Some(mut verification) = verification else {
        return Ok(None);
    };
    verification.checks = conn
        .prepare(
            "SELECT position, command, status, stdout, stderr, exit_code, error FROM run_checks
         WHERE run_id = ?1 ORDER BY position",
        )?
        .query_map([run_id], |row| {
            Ok(CheckResult {
                position: row.get(0)?,
                command: row.get(1)?,
                status: row.get(2)?,
                stdout: row.get(3)?,
                stderr: row.get(4)?,
                exit_code: row.get(5)?,
                error: row.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Some(verification))
}

pub fn interrupt_unfinished(conn: &Connection) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "UPDATE run_checks SET status = 'interrupted' WHERE status IN ('pending', 'running')
         AND run_id IN (SELECT run_id FROM run_verifications WHERE status = 'running')",
        [],
    )?;
    tx.execute(
        "UPDATE run_verifications SET status = 'interrupted' WHERE status = 'running'",
        [],
    )?;
    tx.commit()?;
    Ok(())
}

fn require_changed(changed: usize) -> Result<()> {
    if changed == 1 {
        Ok(())
    } else {
        Err(StoreError::Invalid(
            "Cette vérification ne peut plus changer d'état.".into(),
        ))
    }
}
pub async fn execute(
    command: &str,
    cwd: &Path,
    timeout: Duration,
) -> std::result::Result<CommandOutput, String> {
    crate::process::capture(
        Path::new("sh"),
        &["-c".into(), command.into()],
        cwd,
        None,
        timeout,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn setup() -> Connection {
        let conn = Db::test_connection();
        conn.execute_batch(
            "INSERT INTO cards (id, project_id, column_id, number, title, position)
                 VALUES (1, 1, 1, 1, 'Checks', 0);
             INSERT INTO agent_runs (id, card_id, agent, permission_mode, prompt)
                 VALUES (1, 1, 'claude', 'bypassPermissions', 'check');",
        )
        .unwrap();
        conn
    }

    fn output(code: i32) -> CommandOutput {
        CommandOutput {
            stdout: "résultat".into(),
            stderr: "diagnostic".into(),
            exit_code: Some(code),
            timed_out: false,
        }
    }

    #[test]
    fn no_commands_are_persisted_as_skipped_not_passed() {
        let conn = setup();
        assert!(get(&conn, RunId(1)).unwrap().is_none());
        start(&conn, RunId(1), "abc123", &[]).unwrap();
        let verification = get(&conn, RunId(1)).unwrap().unwrap();
        assert_eq!(verification.commit_sha, "abc123");
        assert_eq!(verification.status, VerificationStatus::Skipped);
        assert!(verification.checks.is_empty());
        assert!(finish(&conn, RunId(1), VerificationStatus::Passed).is_err());
    }

    #[test]
    fn each_command_keeps_its_output_and_unexecuted_commands_stay_pending() {
        let conn = setup();
        start(
            &conn,
            RunId(1),
            "abc123",
            &["first".into(), "second".into(), "third".into()],
        )
        .unwrap();
        assert!(finish(&conn, RunId(1), VerificationStatus::Passed).is_err());
        start_check(&conn, RunId(1), 0).unwrap();
        finish_check(&conn, RunId(1), 0, &output(0)).unwrap();
        start_check(&conn, RunId(1), 1).unwrap();
        finish_check(&conn, RunId(1), 1, &output(9)).unwrap();
        finish(&conn, RunId(1), VerificationStatus::Failed).unwrap();
        let verification = get(&conn, RunId(1)).unwrap().unwrap();
        assert_eq!(verification.status, VerificationStatus::Failed);
        assert_eq!(
            verification
                .checks
                .iter()
                .map(|c| (c.position, c.command.as_str(), c.status))
                .collect::<Vec<_>>(),
            vec![
                (0, "first", CheckStatus::Passed),
                (1, "second", CheckStatus::Failed),
                (2, "third", CheckStatus::Pending)
            ]
        );
        assert_eq!(verification.checks[1].exit_code, Some(9));
        assert_eq!(verification.checks[1].stdout, "résultat");
        assert_eq!(verification.checks[1].stderr, "diagnostic");
        assert!(verification.checks[2].exit_code.is_none());
        assert!(start_check(&conn, RunId(1), 2).is_err());
    }

    #[test]
    fn timed_out_commands_are_failed_even_with_a_zero_exit_code() {
        let conn = setup();
        start(&conn, RunId(1), "abc123", &["slow".into()]).unwrap();
        start_check(&conn, RunId(1), 0).unwrap();
        let mut result = output(0);
        result.timed_out = true;
        finish_check(&conn, RunId(1), 0, &result).unwrap();
        let check = get(&conn, RunId(1)).unwrap().unwrap().checks.remove(0);
        assert_eq!(check.status, CheckStatus::Failed);
        assert!(check.error.unwrap().contains("délai"));
        assert!(finish(&conn, RunId(1), VerificationStatus::Passed).is_err());
    }

    #[test]
    fn interruption_updates_unfinished_commands_but_preserves_completed_results() {
        let conn = setup();
        start(
            &conn,
            RunId(1),
            "abc123",
            &["complete".into(), "running".into(), "pending".into()],
        )
        .unwrap();
        start_check(&conn, RunId(1), 0).unwrap();
        finish_check(&conn, RunId(1), 0, &output(0)).unwrap();
        start_check(&conn, RunId(1), 1).unwrap();
        interrupt_unfinished(&conn).unwrap();
        let result = get(&conn, RunId(1)).unwrap().unwrap();
        assert_eq!(result.status, VerificationStatus::Interrupted);
        assert_eq!(
            result.checks.iter().map(|c| c.status).collect::<Vec<_>>(),
            vec![
                CheckStatus::Passed,
                CheckStatus::Interrupted,
                CheckStatus::Interrupted
            ]
        );
        assert_eq!(result.checks[0].stdout, "résultat");
        interrupt_unfinished(&conn).unwrap();
        assert_eq!(get(&conn, RunId(1)).unwrap().unwrap(), result);
    }

    #[test]
    fn a_finished_verification_cannot_be_rewritten_or_interrupted() {
        let conn = setup();
        start(&conn, RunId(1), "abc123", &["ok".into()]).unwrap();
        assert!(finish_check(&conn, RunId(1), 0, &output(0)).is_err());
        start_check(&conn, RunId(1), 0).unwrap();
        finish_check(&conn, RunId(1), 0, &output(0)).unwrap();
        finish(&conn, RunId(1), VerificationStatus::Passed).unwrap();
        finish(&conn, RunId(1), VerificationStatus::Interrupted).unwrap();
        interrupt_unfinished(&conn).unwrap();
        assert!(start(&conn, RunId(1), "changed", &["different".into()]).is_err());
        assert!(finish_check(&conn, RunId(1), 0, &output(3)).is_err());
        assert_eq!(
            get(&conn, RunId(1)).unwrap().unwrap().status,
            VerificationStatus::Passed
        );
    }

    #[test]
    fn deleting_a_card_removes_checks_and_pull_request_snapshot() {
        let conn = setup();
        start(&conn, RunId(1), "abc123", &["ok".into()]).unwrap();
        conn.execute("INSERT INTO card_pull_requests (card_id, run_id, repository, number, expected_base, snapshot, refreshed_at) VALUES (1, 1, 'owner/repo', 7, 'main', '{}', 123)", []).unwrap();
        conn.execute("DELETE FROM cards WHERE id = 1", []).unwrap();
        for table in ["run_verifications", "run_checks", "card_pull_requests"] {
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "{table}");
        }
    }

    #[test]
    fn schema_rejects_passed_checks_without_a_successful_exit_code() {
        let conn = setup();
        start(&conn, RunId(1), "abc123", &["ok".into()]).unwrap();
        assert!(
            conn.execute(
                "UPDATE run_checks SET status = 'passed' WHERE run_id = 1",
                []
            )
            .is_err()
        );
        assert!(
            conn.execute(
                "UPDATE run_checks SET status = 'passed', exit_code = 7 WHERE run_id = 1",
                []
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn executes_shell_commands_without_extra_interpolation() {
        let output = execute(
            "printf '%s' 'literal $HOME ; word'; exit 3",
            Path::new("/tmp"),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert_eq!(output.stdout, "literal $HOME ; word");
        assert_eq!(output.exit_code, Some(3));
    }
}
