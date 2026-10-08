//! A card's GitHub delivery, separate from the agent's execution and its local checks.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension};

use crate::changes::Changes;
use crate::checks::{self, VerificationStatus};
use crate::config::{GithubConfig, RunGate};
use crate::db::Db;
use crate::git;
use crate::github::{Github, PullRequest, PullRequestState};
use crate::runs::{self, Run, RunId, RunStatus};
use crate::store::{self, Author, Category, StoreError};

#[derive(Clone, Debug)]
pub struct CardPullRequest {
    pub run_id: RunId,
    pub repository: String,
    pub expected_base: String,
    pub pr: PullRequest,
    pub refreshed_at: i64,
    pub error: Option<String>,
}

impl CardPullRequest {
    pub fn refreshed_iso(&self) -> String {
        store::utc_iso(self.refreshed_at)
    }

    pub fn refreshed_display(&self) -> String {
        store::utc_display(self.refreshed_at)
    }
}

pub fn get(conn: &Connection, card_id: i64) -> store::Result<Option<CardPullRequest>> {
    Ok(conn.query_row(
        "SELECT run_id, repository, snapshot, refreshed_at, error, expected_base FROM card_pull_requests WHERE card_id = ?1",
        [card_id],
        |row| {
            let snapshot: String = row.get(2)?;
            let repository: String = row.get(1)?;
            let pr: PullRequest = serde_json::from_str(&snapshot).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
            })?;
            pr.validate_for_repository(&repository).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, error.into())
            })?;
            Ok(CardPullRequest {
                run_id: row.get(0)?, repository, pr,
                refreshed_at: row.get(3)?, error: row.get(4)?,
                expected_base: row.get(5)?,
            })
        },
    ).optional()?)
}

pub fn record(
    conn: &Connection,
    card_id: i64,
    run_id: RunId,
    repository: &str,
    pr: &PullRequest,
) -> store::Result<()> {
    pr.validate_for_repository(repository)
        .map_err(StoreError::Invalid)?;
    if runs::get_run(conn, run_id)?.card_id != card_id {
        return Err(StoreError::Invalid(
            "La PR ne correspond pas à cette carte.".to_owned(),
        ));
    }
    let snapshot = serde_json::to_string(pr)
        .map_err(|e| StoreError::Invalid(format!("État GitHub illisible : {e}")))?;
    conn.execute(
        "INSERT INTO card_pull_requests (card_id, run_id, repository, number, snapshot, expected_base, refreshed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, unixepoch())
         ON CONFLICT (card_id) DO UPDATE SET run_id = excluded.run_id,
           repository = excluded.repository, number = excluded.number,
           expected_base = CASE WHEN lower(card_pull_requests.repository) = lower(excluded.repository)
             AND card_pull_requests.number = excluded.number
             THEN card_pull_requests.expected_base ELSE excluded.expected_base END,
           snapshot = excluded.snapshot, refreshed_at = excluded.refreshed_at, error = NULL",
        (card_id, run_id, repository, pr.number, snapshot, &pr.base_branch),
    )?;
    Ok(())
}

/// A previous push can have succeeded while saving its GitHub snapshot failed.
/// Trust the last confirmed push in the lineage, never a refreshed remote snapshot.
fn resume_head(conn: &Connection, run: &Run, saved: &CardPullRequest) -> store::Result<String> {
    let mut ancestor_id = run.resumed_from;
    let mut child_id = run.id;
    while let Some(id) = ancestor_id {
        let ancestor = runs::get_run(conn, id)?;
        if ancestor.card_id != run.card_id || id >= child_id {
            return Err(StoreError::Invalid(
                "La lignée de reprise est invalide.".to_owned(),
            ));
        }
        if ancestor.status == RunStatus::Succeeded && ancestor.pushed_at.is_some() {
            return verified_resume_head(conn, id);
        }
        child_id = id;
        ancestor_id = ancestor.resumed_from;
    }
    verified_resume_head(conn, saved.run_id)
}

fn verified_resume_head(conn: &Connection, id: RunId) -> store::Result<String> {
    let verification = checks::get(conn, id)?
        .filter(|v| {
            matches!(
                v.status,
                VerificationStatus::Passed | VerificationStatus::Skipped
            )
        })
        .ok_or_else(|| {
            StoreError::Invalid(
                "Le commit publié de la session n'a pas de vérifications valides.".to_owned(),
            )
        })?;
    Ok(verification.commit_sha)
}

/// Shared with the orchestrator. Reservation and run-slot checks happen together on the
/// database worker, so a merge cannot race the queueing of another agent on the same card.
#[derive(Clone, Default)]
pub struct OperationLocks(Arc<Mutex<HashSet<i64>>>);

impl OperationLocks {
    pub fn contains(&self, card_id: i64) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(&card_id)
    }

    fn reserve(&self, card_id: i64) -> store::Result<Reservation> {
        if !self
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(card_id)
        {
            return Err(StoreError::Invalid(
                "Une opération GitHub est déjà en cours sur cette carte.".to_owned(),
            ));
        }
        Ok(Reservation {
            locks: self.clone(),
            card_id,
        })
    }
}

struct Reservation {
    locks: OperationLocks,
    card_id: i64,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.locks
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.card_id);
    }
}

#[derive(Clone)]
pub struct Delivery {
    db: Db,
    changes: Changes,
    gate: RunGate,
    repo: Option<PathBuf>,
    config: Option<GithubConfig>,
    locks: OperationLocks,
}

impl Delivery {
    pub fn new(
        db: Db,
        changes: Changes,
        gate: RunGate,
        repo: Option<PathBuf>,
        config: Option<GithubConfig>,
    ) -> Self {
        Self {
            db,
            changes,
            gate,
            repo,
            config,
            locks: OperationLocks::default(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.gate == RunGate::Open && self.repo.is_some() && self.config.is_some()
    }

    pub fn locks(&self) -> OperationLocks {
        self.locks.clone()
    }

    fn client(&self) -> Result<(Github, &std::path::Path, &str), String> {
        if !self.enabled() {
            return Err("GitHub est indisponible : configurez github.repository et un dépôt, avec une écoute locale.".to_owned());
        }
        let config = self.config.as_ref().expect("enabled config");
        Ok((
            Github::new(config.clone()),
            self.repo.as_deref().expect("enabled repo"),
            &config.repository,
        ))
    }

    async fn reserve(
        &self,
        card_id: i64,
        publishing: Option<RunId>,
    ) -> Result<(Reservation, Run), String> {
        let locks = self.locks.clone();
        self.db
            .call(move |conn| {
                store::get_card(conn, card_id)?;
                let run = runs::latest_run(conn, card_id)?
                    .ok_or_else(|| StoreError::Invalid("Aucune exécution à publier.".to_owned()))?;
                if run.status.is_active()
                    && !(publishing == Some(run.id) && run.status == RunStatus::Running)
                {
                    return Err(StoreError::Invalid(
                        "Attendez la fin de l'exécution avant cette opération GitHub.".to_owned(),
                    ));
                }
                if publishing.is_some_and(|id| id != run.id) {
                    return Err(StoreError::Invalid(
                        "Une exécution plus récente existe pour cette carte.".to_owned(),
                    ));
                }
                Ok::<_, StoreError>((locks.reserve(card_id)?, run))
            })
            .await
            .map_err(|e| e.to_string())
    }

    async fn verified_head(&self, id: RunId) -> Result<String, String> {
        self.db
            .call(move |conn| {
                let verification = checks::get(conn, id)?.ok_or_else(|| {
                    StoreError::Invalid(
                        "Cette exécution n'a pas de commit vérifié. Relancez la carte.".to_owned(),
                    )
                })?;
                if !matches!(
                    verification.status,
                    VerificationStatus::Passed | VerificationStatus::Skipped
                ) {
                    return Err(StoreError::Invalid(
                        "Les vérifications locales ne sont pas terminées avec succès.".to_owned(),
                    ));
                }
                Ok::<_, StoreError>(verification.commit_sha)
            })
            .await
            .map_err(|e| e.to_string())
    }

    /// Read the existing PR before resuming the agent. The local run occupies the
    /// card's slot, so a poll or a manual merge cannot replace its delivery meanwhile.
    pub async fn validate_resume(&self, run: &Run) -> Result<(), String> {
        if !self.enabled() || run.resumed_from.is_none() {
            return Ok(());
        }
        let card_id = run.card_id;
        let saved = self
            .db
            .call(move |conn| get(conn, card_id))
            .await
            .map_err(|e| e.to_string())?;
        let Some(saved) = saved else {
            return Ok(());
        };
        let (client, repo, repository) = self.client()?;
        if !saved.repository.eq_ignore_ascii_case(repository) {
            return Err("Cette PR appartient à un autre dépôt que celui configuré.".to_owned());
        }
        let (_reservation, run) = self.reserve(card_id, Some(run.id)).await?;
        let number = saved.pr.number;
        let base = saved.expected_base.clone();
        let (branch, expected_head) = self
            .db
            .call(move |conn| {
                let context = crate::resume::context(conn, &run)?;
                let head = resume_head(conn, &run, &saved)?;
                Ok::<_, StoreError>((context.branch, head))
            })
            .await
            .map_err(|e| e.to_string())?;
        let fresh = client.refresh(repo, number).await?;
        if fresh.state != PullRequestState::Open {
            return Err(
                "La PR liée à cette carte n'est plus ouverte. La session ne peut pas être reprise."
                    .to_owned(),
            );
        }
        if fresh.head_branch != branch || fresh.base_branch != base {
            return Err(
                "Les branches de la PR ont changé depuis sa publication. La reprise est arrêtée."
                    .to_owned(),
            );
        }
        if fresh.head_sha != expected_head {
            return Err(
                "Le commit de la PR a changé hors de cette session. La reprise est arrêtée."
                    .to_owned(),
            );
        }
        Ok(())
    }

    pub async fn publish(&self, id: RunId, expected_head: &str) -> Result<(), String> {
        let (client, repo, repository) = self.client()?;
        let run = self
            .db
            .call(move |conn| runs::get_run(conn, id))
            .await
            .map_err(|e| e.to_string())?;
        let (_reservation, run) = self.reserve(run.card_id, Some(id)).await?;
        if !matches!(run.status, RunStatus::Running | RunStatus::Succeeded) {
            return Err("Seule une exécution réussie peut être publiée.".to_owned());
        }
        let verified = self.verified_head(id).await?;
        if verified != expected_head {
            return Err("Le commit à publier ne correspond pas aux vérifications.".to_owned());
        }
        let branch = run
            .branch
            .as_deref()
            .ok_or("La branche de cette exécution est inconnue.")?;
        let card_id = run.card_id;
        let saved = if run.resumed_from.is_some() {
            self.db
                .call(move |conn| get(conn, card_id))
                .await
                .map_err(|e| e.to_string())?
        } else {
            None
        };
        let pr = if let Some(saved) = saved {
            if !saved.repository.eq_ignore_ascii_case(repository) {
                return Err("Cette PR appartient à un autre dépôt que celui configuré.".to_owned());
            }
            client
                .ensure_existing(
                    repo,
                    saved.pr.number,
                    branch,
                    &saved.expected_base,
                    expected_head,
                )
                .await?
        } else {
            let (card, key, number) = self
                .db
                .call(move |conn| {
                    let (key, number) = store::card_key(conn, card_id)?;
                    Ok::<_, StoreError>((store::get_card(conn, card_id)?, key, number))
                })
                .await
                .map_err(|e| e.to_string())?;
            let base = git::default_branch(repo).await.map_err(|e| e.to_string())?;
            let body = self
                .pr_body(
                    id,
                    &format!("{key}-{number}"),
                    &card.description,
                    expected_head,
                )
                .await?;
            client
                .ensure(
                    repo,
                    branch,
                    &base,
                    &format!("{key}-{number}: {}", card.title),
                    &body,
                )
                .await?
        };
        if pr.head_sha != expected_head || pr.head_branch != branch {
            return Err(
                "La PR ne porte pas le commit vérifié. Actualisez la carte avant de continuer."
                    .to_owned(),
            );
        }
        self.save(card_id, id, repository, pr).await
    }

    async fn pr_body(
        &self,
        id: RunId,
        key: &str,
        description: &str,
        head: &str,
    ) -> Result<String, String> {
        let verification = self
            .db
            .call(move |conn| checks::get(conn, id))
            .await
            .map_err(|e| e.to_string())?
            .ok_or("Vérifications introuvables.")?;
        let mut body = format!(
            "Carte {key}\n\n{description}\n\nVérifications exécutées par Helm sur le commit `{head}` :\n\n"
        );
        if verification.status == VerificationStatus::Skipped {
            body.push_str("Non exécutées : aucune commande configurée.\n");
        } else {
            for check in verification.checks {
                body.push_str(&format!(
                    "- `{}` : {}\n",
                    check.command,
                    check.status.label()
                ));
            }
        }
        Ok(body)
    }

    pub async fn retry(&self, card_id: i64) -> Result<(), String> {
        self.client()?;
        let run = self
            .db
            .call(move |conn| runs::latest_run(conn, card_id))
            .await
            .map_err(|e| e.to_string())?
            .ok_or("Aucune exécution à publier.")?;
        if run.status != RunStatus::Succeeded {
            return Err("La dernière exécution doit être réussie avant de créer la PR.".to_owned());
        }
        let head = self.verified_head(run.id).await?;
        self.publish(run.id, &head).await
    }

    pub async fn refresh(&self, card_id: i64) -> Result<(), String> {
        let (client, repo, repository) = self.client()?;
        let (_reservation, _) = self.reserve(card_id, None).await?;
        let saved = self.saved(card_id, repository).await?;
        let result = async {
            let pr = client.refresh(repo, saved.pr.number).await?;
            if pr.base_branch != saved.expected_base {
                return Err(
                    "La branche de base de la PR a changé depuis sa publication.".to_owned(),
                );
            }
            self.save(card_id, saved.run_id, repository, pr).await
        }
        .await;
        if let Err(error) = &result {
            self.save_error(card_id, error).await;
        }
        result
    }

    pub async fn merge(&self, card_id: i64) -> Result<(), String> {
        let (client, repo, repository) = self.client()?;
        let (_reservation, run) = self.reserve(card_id, None).await?;
        let saved = self.saved(card_id, repository).await?;
        if run.id != saved.run_id || run.status != RunStatus::Succeeded {
            return Err("La PR n'est pas celle de la dernière exécution réussie.".to_owned());
        }
        let expected = self.verified_head(run.id).await?;
        let result = async {
            let fresh = client.refresh(repo, saved.pr.number).await?;
            if fresh.base_branch != saved.expected_base {
                return Err("La branche de base de la PR a changé depuis sa publication.".to_owned());
            }
            self.save(card_id, run.id, repository, fresh.clone()).await?;
            if fresh.head_sha != expected || Some(fresh.head_branch.as_str()) != run.branch.as_deref() {
                return Err("Le commit de la PR a changé depuis les vérifications. Relancez les vérifications avant la fusion.".to_owned());
            }
            if fresh.state == PullRequestState::Merged {
                return Ok(());
            }
            if let Some(reason) = fresh.merge_blocker() {
                return Err(reason.to_owned());
            }
            let merged = client.merge(repo, fresh.number, &expected, &saved.expected_base).await?;
            self.save(card_id, run.id, repository, merged).await
        }.await;
        if let Err(error) = &result {
            self.save_error(card_id, error).await;
        }
        result
    }

    async fn saved(&self, card_id: i64, repository: &str) -> Result<CardPullRequest, String> {
        let saved = self
            .db
            .call(move |conn| get(conn, card_id))
            .await
            .map_err(|e| e.to_string())?
            .ok_or("Aucune PR liée à cette carte.")?;
        if !saved.repository.eq_ignore_ascii_case(repository) {
            return Err("Cette PR appartient à un autre dépôt que celui configuré.".to_owned());
        }
        Ok(saved)
    }

    async fn save(
        &self,
        card_id: i64,
        id: RunId,
        repository: &str,
        pr: PullRequest,
    ) -> Result<(), String> {
        let repository = repository.to_owned();
        self.db
            .call(move |conn| {
                let previous = get(conn, card_id)?;
                let expected_base = previous
                    .as_ref()
                    .filter(|old| {
                        old.pr.number == pr.number
                            && old.repository.eq_ignore_ascii_case(&repository)
                    })
                    .map_or(pr.base_branch.as_str(), |old| old.expected_base.as_str());
                record(conn, card_id, id, &repository, &pr)?;
                if previous
                    .as_ref()
                    .is_none_or(|old| old.pr.number != pr.number || old.repository != repository)
                {
                    store::add_comment(
                        conn,
                        card_id,
                        &Author::helm(),
                        &format!("PR #{} prête à relire : {}", pr.number, pr.url),
                    )?;
                }
                let latest = runs::latest_run(conn, card_id)?;
                let verification = checks::get(conn, id)?;
                let matches_verified = verification.is_some_and(|v| {
                    v.commit_sha == pr.head_sha
                        && matches!(
                            v.status,
                            VerificationStatus::Passed | VerificationStatus::Skipped
                        )
                });
                if pr.state == PullRequestState::Merged
                    && matches_verified
                    && pr.base_branch == expected_base
                    && latest.is_some_and(|run| {
                        run.id == id
                            && run.status == RunStatus::Succeeded
                            && run.branch.as_deref() == Some(&pr.head_branch)
                    })
                    && store::card_category(conn, card_id)? != Category::Done
                {
                    store::move_to_category(conn, card_id, Category::Done)?;
                    store::add_comment(
                        conn,
                        card_id,
                        &Author::helm(),
                        &format!("PR #{} fusionnée : {}", pr.number, pr.url),
                    )?;
                }
                Ok::<_, StoreError>(())
            })
            .await
            .map_err(|e| e.to_string())?;
        self.changes.publish();
        Ok(())
    }

    async fn save_error(&self, card_id: i64, error: &str) {
        let error = error.to_owned();
        let _ = self
            .db
            .call(move |conn| {
                conn.execute(
                    "UPDATE card_pull_requests SET error = ?2 WHERE card_id = ?1",
                    (card_id, error),
                )?;
                Ok::<_, StoreError>(())
            })
            .await;
        self.changes.publish();
    }

    /// Periodic refresh also sees merges performed directly on GitHub. Manual refresh is
    /// available without JavaScript; reading a page never starts a GitHub mutation.
    pub async fn poll(self) {
        if !self.enabled() {
            return;
        }
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let cards = self.db.call(|conn| {
                let mut query = conn.prepare("SELECT p.card_id FROM card_pull_requests p JOIN cards c ON c.id = p.card_id JOIN board_columns b ON b.id = c.column_id WHERE b.category <> 'done'")?;
                Ok::<Vec<i64>, StoreError>(query.query_map([], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?)
            }).await;
            if let Ok(cards) = cards {
                for card in cards {
                    let _ = self.refresh(card).await;
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "delivery_tests.rs"]
mod tests;
