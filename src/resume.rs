//! A correction continues the card's existing session, with one atomic comment and run.

use rusqlite::{Connection, TransactionBehavior};

use crate::github::PullRequestState;
use crate::runs::{self, NewRun, Run, RunId};
use crate::store::{self, Author, Card, Category, StoreError};
use crate::{delivery, git, prompt};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeContext {
    pub session_id: String,
    pub worktree_path: String,
    pub branch: String,
}

fn conflict(message: &str) -> StoreError {
    StoreError::Conflict(message.to_owned())
}

/// A failed launch may never emit a session. Follow its recorded parent, never another card
/// or a newer run. Workspace information already recorded on that path must agree.
pub fn context(conn: &Connection, run: &Run) -> store::Result<ResumeContext> {
    let (key, number) = store::card_key(conn, run.card_id)?;
    let expected_branch = git::branch_name(&key, number);
    let mut current = run.clone();
    let mut expected_path = None;
    loop {
        if current.card_id != run.card_id || current.agent != run.agent {
            return Err(conflict(
                "La session ne correspond pas à cette carte et à son agent.",
            ));
        }
        if let Some(branch) = &current.branch {
            if branch != &expected_branch {
                return Err(conflict(
                    "La branche de la session ne correspond pas à cette carte.",
                ));
            }
        }
        if let Some(path) = &current.worktree_path {
            if path.trim().is_empty()
                || path.contains('\0')
                || expected_path.as_ref().is_some_and(|known| known != path)
            {
                return Err(conflict(
                    "Le worktree de la session est absent ou incohérent.",
                ));
            }
            expected_path = Some(path.clone());
        }
        if let Some(session_id) = &current.session_id {
            if session_id.len() > 200
                || !session_id
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                || !session_id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
            {
                return Err(conflict(
                    "L'identifiant de session ne permet pas une reprise.",
                ));
            }
            let Some(path) = current.worktree_path else {
                return Err(conflict(
                    "Le worktree de la session n'a pas été enregistré.",
                ));
            };
            let Some(branch) = current.branch else {
                return Err(conflict(
                    "La branche de la session n'a pas été enregistrée.",
                ));
            };
            return Ok(ResumeContext {
                session_id: session_id.clone(),
                worktree_path: path,
                branch,
            });
        }
        let parent = current.resumed_from.ok_or_else(|| {
            conflict("Aucune session enregistrée ne permet de reprendre cette exécution.")
        })?;
        if parent >= current.id {
            return Err(conflict("L'historique des reprises est incohérent."));
        }
        current = runs::get_run(conn, parent)?;
        if current.status.is_active() {
            return Err(conflict("La session précédente n'est pas terminée."));
        }
    }
}

fn candidate(
    conn: &Connection,
    card_id: i64,
    source_id: Option<RunId>,
) -> store::Result<(Run, Card, ResumeContext)> {
    let card = store::get_card(conn, card_id)?;
    if store::card_category(conn, card_id)? == Category::Done {
        return Err(conflict("Une carte terminée ne peut pas être reprise."));
    }
    let source = runs::latest_run(conn, card_id)?
        .ok_or_else(|| conflict("Cette carte n'a aucune exécution à reprendre."))?;
    if source_id.is_some_and(|id| id != source.id) {
        return Err(conflict(
            "L'exécution a changé. Actualisez la carte avant de reprendre.",
        ));
    }
    if source.status.is_active() {
        return Err(conflict(
            "Une exécution est déjà en file ou en cours sur cette carte.",
        ));
    }
    if card.agent != Some(source.agent) {
        return Err(conflict(
            "La carte doit rester attribuée à l'agent de la session.",
        ));
    }
    let context = context(conn, &source)?;
    if let Some(saved) = delivery::get(conn, card_id)? {
        if saved.pr.state != PullRequestState::Open {
            return Err(conflict(
                "La PR doit être ouverte pour reprendre cette carte.",
            ));
        }
        if saved.pr.head_branch != context.branch || saved.pr.base_branch != saved.expected_base {
            return Err(conflict("La branche ou la cible de la PR a changé."));
        }
    }
    Ok((source, card, context))
}

/// The UI and enqueue operation share the same eligibility rules. Database errors remain
/// errors rather than silently hiding the action.
pub fn available(conn: &Connection, card_id: i64) -> store::Result<bool> {
    match candidate(conn, card_id, None) {
        Ok(_) => Ok(true),
        Err(StoreError::Conflict(_)) => Ok(false),
        Err(error) => Err(error),
    }
}

pub fn enqueue(
    conn: &mut Connection,
    card_id: i64,
    source_id: RunId,
    feedback: &str,
) -> store::Result<RunId> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (source, card, context) = candidate(&tx, card_id, Some(source_id))?;
    let feedback_id = store::add_comment_in_transaction(&tx, card_id, &Author::moi(), feedback)?;
    let comments = store::list_comments(&tx, card_id)?;
    let (key, number) = store::card_key(&tx, card_id)?;
    let text = prompt::build_resume(
        &format!("{key}-{number}"),
        &context.branch,
        &card,
        &comments,
        feedback_id,
    );
    let id = runs::enqueue_resumed(
        &tx,
        &NewRun {
            card_id,
            agent: source.agent,
            model: source.model.as_ref(),
            permission_mode: source.permission_mode,
            prompt: &text,
        },
        source.id,
    )?
    .ok_or_else(|| conflict("Une exécution est déjà en file ou en cours sur cette carte."))?;
    tx.commit()?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Agent, ModelName, PermissionMode};
    use crate::db::Db;
    use crate::github::{CiStatus, PullRequest, PullRequestState};
    use crate::runs::{self, NewRun, Outcome, RunStatus};
    use crate::store::{Author, CardInput, Category};

    struct Fixture {
        conn: Connection,
        card: i64,
        source: RunId,
    }

    impl Fixture {
        fn new() -> Self {
            let mut conn = Db::test_connection();
            let card = store::create_card(
                &mut conn,
                4,
                &CardInput {
                    title: "Corriger le formulaire".to_owned(),
                    description: "Conserver les données.".to_owned(),
                    agent: "claude".to_owned(),
                    ..CardInput::default()
                },
            )
            .unwrap();
            let model = ModelName::parse_optional("sonnet").unwrap();
            let source = runs::enqueue(
                &conn,
                &NewRun {
                    card_id: card,
                    agent: Agent::Claude,
                    model: model.as_ref(),
                    permission_mode: PermissionMode::DEFAULT,
                    prompt: "Initial task",
                },
            )
            .unwrap()
            .unwrap();
            runs::claim_next_queued(&mut conn).unwrap();
            runs::record_workspace(&conn, source, "/worktrees/HELM-1", "helm/HELM-1").unwrap();
            runs::record_session(&conn, source, "session-42").unwrap();
            runs::finish(&mut conn, source, &Outcome::Succeeded).unwrap();
            Self { conn, card, source }
        }

        fn counts(&self) -> (i64, i64, i64) {
            self.conn.query_row(
                "SELECT (SELECT count(*) FROM comments), (SELECT count(*) FROM agent_runs), (SELECT count(*) FROM mentions)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).unwrap()
        }

        fn pr(&mut self, state: PullRequestState) {
            crate::delivery::record(
                &self.conn,
                self.card,
                self.source,
                "owner/repo",
                &PullRequest {
                    number: 7,
                    url: "https://github.com/owner/repo/pull/7".to_owned(),
                    state,
                    draft: false,
                    head_sha: "a".repeat(40),
                    head_branch: "helm/HELM-1".to_owned(),
                    base_branch: "main".to_owned(),
                    checks: CiStatus::Passed,
                    mergeable: "MERGEABLE".to_owned(),
                    merge_state_status: "CLEAN".to_owned(),
                    review_decision: String::new(),
                },
            )
            .unwrap();
        }
    }

    #[test]
    fn feedback_and_resumed_run_are_stored_together_with_current_context_and_original_settings() {
        let mut f = Fixture::new();
        store::add_comment(&mut f.conn, f.card, &Author::moi(), "Premier retour").unwrap();
        f.conn
            .execute(
                "UPDATE cards SET model = 'opus', description = 'Nouveau contexte' WHERE id = ?1",
                [f.card],
            )
            .unwrap();
        assert!(available(&f.conn, f.card).unwrap());
        let id = enqueue(
            &mut f.conn,
            f.card,
            f.source,
            "\n  Garde les accents @claude.\r\nMerci.  ",
        )
        .unwrap();
        let run = runs::get_run(&f.conn, id).unwrap();
        assert_eq!(run.status, RunStatus::Queued);
        assert_eq!(run.model.as_ref().map(ModelName::as_str), Some("sonnet"));
        assert_eq!(run.permission_mode, PermissionMode::DEFAULT);
        let parent: RunId = f
            .conn
            .query_row(
                "SELECT resumed_from FROM agent_runs WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(parent, f.source);
        let comments = store::list_comments(&f.conn, f.card).unwrap();
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[1].author, Author::moi());
        assert_eq!(comments[1].body, "  Garde les accents @claude.\nMerci.");
        assert!(run.prompt.contains("Nouveau contexte"));
        assert!(run.prompt.contains("Premier retour"));
        assert_eq!(run.prompt.matches("Garde les accents @claude.").count(), 1);
        assert!(run.prompt.contains("Latest correction request"));
        assert!(!available(&f.conn, f.card).unwrap());
        assert_eq!(context(&f.conn, &run).unwrap().session_id, "session-42");
    }

    #[test]
    fn repeated_or_stale_requests_add_no_comment_and_no_run() {
        let mut f = Fixture::new();
        let queued = enqueue(&mut f.conn, f.card, f.source, "Une correction").unwrap();
        let counts = f.counts();
        assert!(enqueue(&mut f.conn, f.card, f.source, "Duplicata").is_err());
        assert_eq!(f.counts(), counts);
        runs::finish(&mut f.conn, queued, &Outcome::Cancelled).unwrap();
        assert!(enqueue(&mut f.conn, f.card, f.source, "Ancienne page").is_err());
        assert_eq!(f.counts(), counts);
        assert!(enqueue(&mut f.conn, f.card, queued, "Nouvelle tentative").is_ok());
    }

    #[test]
    fn an_insert_failure_rolls_back_the_feedback_and_its_mentions() {
        let mut f = Fixture::new();
        f.conn.execute_batch("CREATE TRIGGER fail_resume BEFORE INSERT ON agent_runs BEGIN SELECT RAISE(ABORT, 'test insert failure'); END;").unwrap();
        let counts = f.counts();
        assert!(enqueue(&mut f.conn, f.card, f.source, "Corrige @claude").is_err());
        assert_eq!(f.counts(), counts);
    }

    #[test]
    fn invalid_feedback_leaves_no_partial_work() {
        let mut f = Fixture::new();
        let counts = f.counts();
        for text in [" \n\t".to_owned(), "é".repeat(store::MAX_COMMENT_CHARS + 1)] {
            assert!(enqueue(&mut f.conn, f.card, f.source, &text).is_err());
            assert_eq!(f.counts(), counts);
        }
        assert!(
            enqueue(
                &mut f.conn,
                f.card,
                f.source,
                &"é".repeat(store::MAX_COMMENT_CHARS)
            )
            .is_ok()
        );
    }

    #[test]
    fn done_unassigned_and_sessionless_cards_cannot_resume() {
        for change in [
            "UPDATE cards SET column_id = 5",
            "UPDATE cards SET agent = NULL",
            "UPDATE agent_runs SET session_id = NULL",
            "UPDATE agent_runs SET worktree_path = NULL",
            "UPDATE agent_runs SET branch = 'other'",
            "UPDATE agent_runs SET session_id = '--continue'",
            "UPDATE agent_runs SET session_id = 'two sessions'",
        ] {
            let mut f = Fixture::new();
            f.conn.execute_batch(change).unwrap();
            assert!(!available(&f.conn, f.card).unwrap(), "{change}");
            let counts = f.counts();
            assert!(
                enqueue(&mut f.conn, f.card, f.source, "Corrige").is_err(),
                "{change}"
            );
            assert_eq!(f.counts(), counts);
        }
    }

    #[test]
    fn open_pr_allows_correction_but_closed_merged_or_retargeted_prs_do_not() {
        let mut f = Fixture::new();
        f.pr(PullRequestState::Open);
        assert!(available(&f.conn, f.card).unwrap());
        for state in [PullRequestState::Closed, PullRequestState::Merged] {
            f.pr(state);
            assert!(!available(&f.conn, f.card).unwrap());
            assert!(enqueue(&mut f.conn, f.card, f.source, "Corrige").is_err());
        }
        f.pr(PullRequestState::Open);
        f.conn
            .execute(
                "UPDATE card_pull_requests SET expected_base = 'different-base'",
                [],
            )
            .unwrap();
        assert!(!available(&f.conn, f.card).unwrap());
        f.conn.execute("UPDATE card_pull_requests SET expected_base = 'main', snapshot = json_set(snapshot, '$.head_branch', 'other')", []).unwrap();
        assert!(!available(&f.conn, f.card).unwrap());
    }

    #[test]
    fn lineage_recovers_the_session_after_multiple_attempts_cancelled_before_launch() {
        let mut f = Fixture::new();
        let second = enqueue(&mut f.conn, f.card, f.source, "Deuxième").unwrap();
        runs::finish(&mut f.conn, second, &Outcome::Cancelled).unwrap();
        let third = enqueue(&mut f.conn, f.card, second, "Troisième").unwrap();
        runs::claim_next_queued(&mut f.conn).unwrap();
        runs::record_workspace(&f.conn, third, "/worktrees/HELM-1", "helm/HELM-1").unwrap();
        runs::finish(
            &mut f.conn,
            third,
            &Outcome::Failed("Lancement impossible".to_owned()),
        )
        .unwrap();
        let ctx = context(&f.conn, &runs::get_run(&f.conn, third).unwrap()).unwrap();
        assert_eq!(
            ctx,
            ResumeContext {
                session_id: "session-42".to_owned(),
                worktree_path: "/worktrees/HELM-1".to_owned(),
                branch: "helm/HELM-1".to_owned()
            }
        );
        assert!(available(&f.conn, f.card).unwrap());
    }

    #[test]
    fn a_newly_recorded_session_wins_over_its_parent() {
        let mut f = Fixture::new();
        let second = enqueue(&mut f.conn, f.card, f.source, "Corrige").unwrap();
        runs::record_workspace(&f.conn, second, "/worktrees/HELM-1", "helm/HELM-1").unwrap();
        runs::record_session(&f.conn, second, "session-next").unwrap();
        let ctx = context(&f.conn, &runs::get_run(&f.conn, second).unwrap()).unwrap();
        assert_eq!(ctx.session_id, "session-next");
    }

    #[test]
    fn malformed_lineage_cannot_escape_its_card_or_cycle() {
        let mut f = Fixture::new();
        let second = enqueue(&mut f.conn, f.card, f.source, "Corrige").unwrap();
        runs::finish(&mut f.conn, second, &Outcome::Cancelled).unwrap();
        for parent in [second, RunId(second.0 + 1)] {
            f.conn.pragma_update(None, "foreign_keys", false).unwrap();
            f.conn
                .execute(
                    "UPDATE agent_runs SET resumed_from = ?2 WHERE id = ?1",
                    (second, parent),
                )
                .unwrap();
            assert!(context(&f.conn, &runs::get_run(&f.conn, second).unwrap()).is_err());
        }
        let other_card = store::create_card(
            &mut f.conn,
            4,
            &CardInput {
                title: "Autre carte".to_owned(),
                ..CardInput::default()
            },
        )
        .unwrap();
        f.conn
            .execute(
                "UPDATE agent_runs SET card_id = ?2 WHERE id = ?1",
                (f.source, other_card),
            )
            .unwrap();
        f.conn
            .execute(
                "UPDATE agent_runs SET resumed_from = ?2 WHERE id = ?1",
                (second, f.source),
            )
            .unwrap();
        assert!(context(&f.conn, &runs::get_run(&f.conn, second).unwrap()).is_err());
    }

    #[test]
    fn enqueue_preserves_the_review_column_until_the_supervisor_claims_work() {
        let mut f = Fixture::new();
        enqueue(&mut f.conn, f.card, f.source, "Corrige").unwrap();
        assert_eq!(
            store::card_category(&f.conn, f.card).unwrap(),
            Category::InReview
        );
    }
}
