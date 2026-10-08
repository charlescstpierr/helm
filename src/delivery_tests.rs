use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

use super::*;
use crate::agent::{Agent, PermissionMode};
use crate::git::testing::{Remote, run as git_run};
use crate::github::CiStatus;
use crate::process::CommandOutput;
use crate::runs::{NewRun, Outcome};
use crate::store::CardInput;

const OTHER_HEAD: &str = "abcdef0123456789abcdef0123456789abcdef01";

struct Fixture {
    remote: Remote,
    state: PathBuf,
    db: Db,
    delivery: Delivery,
    card_id: i64,
    run_id: RunId,
    head: String,
    branch: String,
}

impl Fixture {
    async fn new() -> Self {
        Self::with_verification(VerificationStatus::Passed).await
    }

    async fn with_verification(status: VerificationStatus) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let remote = Remote::new(&format!(
            "delivery-{}",
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let state = PathBuf::from(format!("{}.gh", remote.repo.display()));
        fs::create_dir_all(&state).unwrap();
        let workspace = git::prepare_worktree(&remote.repo, &remote.worktrees, "HELM", 1)
            .await
            .unwrap();
        git::push(&workspace.path, &workspace.branch).await.unwrap();
        let head = git_run(&workspace.path, &["rev-parse", "HEAD"]);
        let db = Db::open_in_memory().unwrap();
        let path = workspace.path.display().to_string();
        let branch = workspace.branch.clone();
        let verified_head = head.clone();
        let (card_id, run_id) = db
            .call(move |conn| {
                let card_id = store::create_card(
                    conn,
                    4,
                    &CardInput {
                        title: "Livrer une carte".to_owned(),
                        description: "Description avec `du code` et $(texte).".to_owned(),
                        ..CardInput::default()
                    },
                )?;
                let run_id = queue(conn, card_id)?;
                runs::claim_next_queued(conn)?;
                runs::record_workspace(conn, run_id, &path, &branch)?;
                let commands = if status == VerificationStatus::Skipped {
                    Vec::new()
                } else {
                    vec!["cargo test".to_owned()]
                };
                checks::start(conn, run_id, &verified_head, &commands)?;
                if matches!(
                    status,
                    VerificationStatus::Passed | VerificationStatus::Failed
                ) {
                    checks::start_check(conn, run_id, 0)?;
                    checks::finish_check(
                        conn,
                        run_id,
                        0,
                        &CommandOutput {
                            stdout: "résultat conservé".to_owned(),
                            stderr: String::new(),
                            exit_code: Some(if status == VerificationStatus::Passed {
                                0
                            } else {
                                1
                            }),
                            timed_out: false,
                        },
                    )?;
                    checks::finish(conn, run_id, status)?;
                } else if status == VerificationStatus::Interrupted {
                    checks::finish(conn, run_id, status)?;
                }
                runs::finish(conn, run_id, &Outcome::Succeeded)?;
                Ok::<_, StoreError>((card_id, run_id))
            })
            .await
            .unwrap();
        let delivery = Delivery::new(
            db.clone(),
            Changes::new(),
            RunGate::Open,
            Some(remote.repo.clone()),
            Some(Self::config()),
        );
        let fixture = Self {
            remote,
            state,
            db,
            delivery,
            card_id,
            run_id,
            head,
            branch: workspace.branch,
        };
        fixture.json("list.json", &json!([fixture.raw_pr()]));
        fixture.json("view.json", &fixture.raw_pr());
        fixture
    }

    fn config() -> GithubConfig {
        GithubConfig {
            repository: "owner/repo".to_owned(),
            command: Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-gh.sh"),
        }
    }

    fn pr(&self) -> PullRequest {
        PullRequest {
            number: 7,
            url: "https://github.com/owner/repo/pull/7".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head_sha: self.head.clone(),
            head_branch: self.branch.clone(),
            base_branch: "main".to_owned(),
            checks: CiStatus::Passed,
            mergeable: "MERGEABLE".to_owned(),
            merge_state_status: "CLEAN".to_owned(),
            review_decision: String::new(),
        }
    }

    fn raw_pr(&self) -> Value {
        json!({
            "number": 7,
            "url": "https://github.com/owner/repo/pull/7",
            "state": "OPEN",
            "isDraft": false,
            "headRefOid": self.head,
            "headRefName": self.branch,
            "baseRefName": "main",
            "statusCheckRollup": [{"__typename": "StatusContext", "state": "SUCCESS"}],
            "mergeable": "MERGEABLE",
            "mergeStateStatus": "CLEAN",
            "reviewDecision": "",
            "isCrossRepository": false,
        })
    }

    fn merged_pr(&self) -> Value {
        let mut pr = self.raw_pr();
        pr["state"] = json!("MERGED");
        pr
    }

    fn json(&self, name: &str, value: &Value) {
        fs::write(self.state.join(name), value.to_string()).unwrap();
    }

    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.state.join(name)).unwrap_or_default()
    }

    fn assert_no_github_commands(&self) {
        assert_eq!(self.read("commands.log"), "");
    }

    fn assert_no_merge_command(&self) {
        assert!(
            !self
                .read("commands.log")
                .lines()
                .any(|line| line == "merge")
        );
    }

    async fn attach(&self) {
        let card_id = self.card_id;
        let run_id = self.run_id;
        let pr = self.pr();
        self.db
            .call(move |conn| record(conn, card_id, run_id, "owner/repo", &pr))
            .await
            .unwrap();
    }

    async fn saved(&self) -> CardPullRequest {
        let card_id = self.card_id;
        self.db
            .call(move |conn| get(conn, card_id))
            .await
            .unwrap()
            .unwrap()
    }

    async fn category(&self) -> Category {
        let card_id = self.card_id;
        self.db
            .call(move |conn| store::card_category(conn, card_id))
            .await
            .unwrap()
    }

    async fn comments(&self) -> Vec<store::Comment> {
        let card_id = self.card_id;
        self.db
            .call(move |conn| store::list_comments(conn, card_id))
            .await
            .unwrap()
    }

    async fn add_run(&self, status: RunStatus) -> RunId {
        let card_id = self.card_id;
        let branch = self.branch.clone();
        let path = self.remote.worktrees.join("HELM-1").display().to_string();
        self.db
            .call(move |conn| {
                let id = queue(conn, card_id)?;
                if status != RunStatus::Queued {
                    runs::claim_next_queued(conn)?;
                    runs::record_workspace(conn, id, &path, &branch)?;
                    match status {
                        RunStatus::Succeeded => runs::finish(conn, id, &Outcome::Succeeded)?,
                        RunStatus::Failed => {
                            runs::finish(conn, id, &Outcome::Failed("échec".to_owned()))?
                        }
                        RunStatus::Running => {}
                        _ => panic!("unsupported fixture status"),
                    }
                }
                Ok::<_, StoreError>(id)
            })
            .await
            .unwrap()
    }
}

fn queue(conn: &mut Connection, card_id: i64) -> store::Result<RunId> {
    Ok(runs::enqueue(
        conn,
        &NewRun {
            card_id,
            agent: Agent::Claude,
            model: None,
            permission_mode: PermissionMode::DEFAULT,
            prompt: "Livrer la carte",
        },
    )?
    .expect("fixture has no active run"))
}

#[tokio::test]
async fn snapshot_round_trips_and_a_new_snapshot_clears_the_last_error() {
    let f = Fixture::new().await;
    let card_id = f.card_id;
    assert!(
        f.db.call(move |conn| get(conn, card_id))
            .await
            .unwrap()
            .is_none()
    );
    f.attach().await;
    let saved = f.saved().await;
    assert_eq!(saved.run_id, f.run_id);
    assert_eq!(saved.repository, "owner/repo");
    assert_eq!(saved.pr, f.pr());
    assert!(saved.refreshed_at > 0);
    assert!(saved.refreshed_iso().ends_with('Z'));
    assert!(saved.refreshed_display().contains("UTC"));
    f.delivery
        .save_error(f.card_id, "GitHub indisponible")
        .await;
    assert_eq!(
        f.saved().await.error.as_deref(),
        Some("GitHub indisponible")
    );

    let mut changed = f.pr();
    changed.checks = CiStatus::Pending;
    let expected = changed.clone();
    let run_id = f.run_id;
    f.db.call(move |conn| record(conn, card_id, run_id, "owner/repo", &changed))
        .await
        .unwrap();
    let saved = f.saved().await;
    assert_eq!(saved.pr, expected);
    assert!(saved.error.is_none());
    f.assert_no_github_commands();
}

#[tokio::test]
async fn expected_base_survives_snapshot_changes_and_resets_for_a_new_delivery() {
    let f = Fixture::new().await;
    f.attach().await;
    assert_eq!(f.saved().await.expected_base, "main");

    let card_id = f.card_id;
    let run_id = f.run_id;
    let mut retargeted = f.pr();
    retargeted.base_branch = "develop".to_owned();
    let snapshot = retargeted.clone();
    f.db.call(move |conn| record(conn, card_id, run_id, "owner/repo", &snapshot))
        .await
        .unwrap();
    let saved = f.saved().await;
    assert_eq!(saved.pr.base_branch, "develop");
    assert_eq!(saved.expected_base, "main");

    let new_run = f.add_run(RunStatus::Succeeded).await;
    let snapshot = retargeted.clone();
    f.db.call(move |conn| record(conn, card_id, new_run, "owner/repo", &snapshot))
        .await
        .unwrap();
    assert_eq!(f.saved().await.run_id, new_run);
    assert_eq!(f.saved().await.expected_base, "develop");

    retargeted.number = 8;
    retargeted.url = "https://github.com/owner/repo/pull/8".to_owned();
    retargeted.base_branch = "release".to_owned();
    f.db.call(move |conn| record(conn, card_id, new_run, "owner/repo", &retargeted))
        .await
        .unwrap();
    assert_eq!(f.saved().await.pr.number, 8);
    assert_eq!(f.saved().await.expected_base, "release");
    f.assert_no_github_commands();
}

#[tokio::test]
async fn publishing_records_the_verified_pr_and_announces_it_once() {
    let f = Fixture::new().await;
    f.json("list.json", &json!([]));
    f.json("after-create.json", &f.raw_pr());
    let mut changes = f.delivery.changes.subscribe();
    f.delivery.publish(f.run_id, &f.head).await.unwrap();
    assert!(changes.try_recv().is_ok());
    let saved = f.saved().await;
    assert_eq!(saved.run_id, f.run_id);
    assert_eq!(saved.pr.head_sha, f.head);
    assert_eq!(f.category().await, Category::InReview);
    let body = f.read("create.body");
    assert!(body.contains("HELM-1"));
    assert!(body.contains("Description avec `du code` et $(texte)."));
    assert!(body.contains(&f.head));
    assert!(body.contains("cargo test"));
    f.delivery.retry(f.card_id).await.unwrap();
    assert_eq!(f.comments().await.len(), 1);
    assert_eq!(
        f.read("commands.log")
            .lines()
            .filter(|action| *action == "create")
            .count(),
        1
    );
}

#[tokio::test]
async fn skipped_local_checks_are_reported_as_not_executed_in_the_pr() {
    let f = Fixture::with_verification(VerificationStatus::Skipped).await;
    f.json("list.json", &json!([]));
    f.json("after-create.json", &f.raw_pr());
    f.delivery.publish(f.run_id, &f.head).await.unwrap();
    assert!(
        f.read("create.body")
            .contains("Non exécutées : aucune commande configurée.")
    );
    assert_eq!(f.saved().await.pr.head_sha, f.head);
}

#[tokio::test]
async fn expected_head_mismatch_and_incomplete_local_checks_prevent_publication() {
    let f = Fixture::new().await;
    assert!(f.delivery.publish(f.run_id, OTHER_HEAD).await.is_err());
    f.assert_no_github_commands();
    assert!(!f.delivery.locks().contains(f.card_id));
    for status in [
        VerificationStatus::Failed,
        VerificationStatus::Running,
        VerificationStatus::Interrupted,
    ] {
        let f = Fixture::with_verification(status).await;
        f.attach().await;
        assert!(
            f.delivery.publish(f.run_id, &f.head).await.is_err(),
            "{status:?}"
        );
        assert!(f.delivery.merge(f.card_id).await.is_err(), "{status:?}");
        assert_eq!(f.category().await, Category::InReview);
        f.assert_no_github_commands();
    }
}

#[tokio::test]
async fn queued_and_running_runs_block_manual_delivery_operations() {
    for status in [RunStatus::Queued, RunStatus::Running] {
        let f = Fixture::new().await;
        f.attach().await;
        f.add_run(status).await;
        assert!(f.delivery.refresh(f.card_id).await.is_err());
        assert!(f.delivery.merge(f.card_id).await.is_err());
        assert!(f.delivery.retry(f.card_id).await.is_err());
        assert!(f.delivery.publish(f.run_id, &f.head).await.is_err());
        assert!(!f.delivery.locks().contains(f.card_id));
        assert_eq!(f.category().await, Category::InReview);
        f.assert_no_github_commands();
    }
}

#[tokio::test]
async fn delivery_reservations_are_shared_by_clones_and_release_after_errors() {
    let f = Fixture::new().await;
    f.attach().await;
    let delivery = f.delivery.clone();
    let (reservation, _) = f.delivery.reserve(f.card_id, None).await.unwrap();
    assert!(delivery.locks().contains(f.card_id));
    assert!(delivery.refresh(f.card_id).await.is_err());
    assert!(delivery.merge(f.card_id).await.is_err());
    assert!(delivery.publish(f.run_id, &f.head).await.is_err());
    f.assert_no_github_commands();
    drop(reservation);
    assert!(!delivery.locks().contains(f.card_id));
    assert!(delivery.publish(f.run_id, OTHER_HEAD).await.is_err());
    assert!(!delivery.locks().contains(f.card_id));
    delivery.refresh(f.card_id).await.unwrap();
    assert_eq!(f.read("commands.log"), "view\n");
}

#[tokio::test]
async fn a_newer_run_prevents_merging_or_republishing_the_previous_run() {
    for status in [RunStatus::Succeeded, RunStatus::Failed] {
        let f = Fixture::new().await;
        f.attach().await;
        assert_ne!(f.add_run(status).await, f.run_id);
        assert!(f.delivery.merge(f.card_id).await.is_err());
        assert!(f.delivery.publish(f.run_id, &f.head).await.is_err());
        assert_eq!(f.category().await, Category::InReview);
        f.assert_no_github_commands();
    }
}

#[tokio::test]
async fn fresh_pr_head_and_branch_must_still_match_the_verified_run() {
    for (field, value) in [("headRefOid", OTHER_HEAD), ("headRefName", "helm/other")] {
        let f = Fixture::new().await;
        f.attach().await;
        let mut changed = f.raw_pr();
        changed[field] = json!(value);
        f.json("view.json", &changed);
        assert!(f.delivery.merge(f.card_id).await.is_err());
        f.assert_no_merge_command();
        assert_eq!(f.category().await, Category::InReview);
        assert!(f.saved().await.error.is_some());
        assert!(!f.delivery.locks().contains(f.card_id));
    }
}

#[tokio::test]
async fn a_retargeted_base_blocks_manual_merge_before_saving_or_merging() {
    let f = Fixture::new().await;
    f.attach().await;
    let mut retargeted = f.raw_pr();
    retargeted["baseRefName"] = json!("develop");
    f.json("view.json", &retargeted);
    retargeted["state"] = json!("MERGED");
    f.json("after-merge.json", &retargeted);

    let result = f.delivery.merge(f.card_id).await;

    f.assert_no_merge_command();
    assert!(result.is_err());
    assert_eq!(f.saved().await.pr, f.pr());
    assert_eq!(f.category().await, Category::InReview);
    assert!(f.comments().await.is_empty());
    assert!(f.saved().await.error.is_some());
    assert!(!f.delivery.locks().contains(f.card_id));
}

#[tokio::test]
async fn refresh_rejects_a_retargeted_base_even_after_an_external_merge() {
    for state in ["MERGED", "OPEN"] {
        let f = Fixture::new().await;
        f.attach().await;
        let mut retargeted = f.raw_pr();
        retargeted["baseRefName"] = json!("develop");
        retargeted["state"] = json!(state);
        f.json("view.json", &retargeted);

        let result = f.delivery.refresh(f.card_id).await;

        assert_eq!(f.category().await, Category::InReview, "{state}");
        assert!(result.is_err(), "{state}");
        assert_eq!(f.saved().await.pr, f.pr(), "{state}");
        assert!(f.saved().await.error.is_some());
        assert!(f.comments().await.is_empty());
        assert!(!f.delivery.locks().contains(f.card_id));
        f.assert_no_merge_command();
    }
}

#[tokio::test]
async fn a_retargeted_merged_snapshot_cannot_redefine_the_expected_base_or_finish_the_card() {
    let f = Fixture::new().await;
    f.attach().await;
    let mut retargeted = f.pr();
    retargeted.base_branch = "develop".to_owned();
    retargeted.state = PullRequestState::Merged;

    f.delivery
        .save(f.card_id, f.run_id, "owner/repo", retargeted)
        .await
        .unwrap();

    let saved = f.saved().await;
    assert_eq!(saved.expected_base, "main");
    assert_eq!(saved.pr.base_branch, "develop");
    assert_eq!(saved.pr.state, PullRequestState::Merged);
    assert_eq!(f.category().await, Category::InReview);
    assert!(f.comments().await.is_empty());

    let mut fresh = f.merged_pr();
    fresh["baseRefName"] = json!("develop");
    f.json("view.json", &fresh);
    assert!(f.delivery.refresh(f.card_id).await.is_err());
    assert!(f.delivery.merge(f.card_id).await.is_err());
    assert_eq!(f.category().await, Category::InReview);
    assert_eq!(f.saved().await.expected_base, "main");
    f.assert_no_merge_command();
}

#[tokio::test]
async fn fresh_failed_or_pending_ci_blocks_merge_even_when_saved_checks_passed() {
    for (state, expected) in [
        ("FAILURE", CiStatus::Failed),
        ("PENDING", CiStatus::Pending),
    ] {
        let f = Fixture::new().await;
        f.attach().await;
        let mut fresh = f.raw_pr();
        fresh["statusCheckRollup"] = json!([{"__typename": "StatusContext", "state": state}]);
        f.json("view.json", &fresh);
        assert!(f.delivery.merge(f.card_id).await.is_err());
        assert_eq!(f.saved().await.pr.checks, expected);
        assert!(f.saved().await.error.is_some());
        assert_eq!(f.category().await, Category::InReview);
        assert_eq!(f.read("commands.log"), "view\n");
        f.assert_no_merge_command();
    }
}

#[tokio::test]
async fn confirmed_merge_moves_the_card_to_done_and_records_one_comment() {
    let f = Fixture::new().await;
    f.attach().await;
    f.json("after-merge.json", &f.merged_pr());
    let mut changes = f.delivery.changes.subscribe();
    f.delivery.merge(f.card_id).await.unwrap();
    assert_eq!(f.saved().await.pr.state, PullRequestState::Merged);
    assert_eq!(f.category().await, Category::Done);
    assert!(f.saved().await.error.is_none());
    assert!(changes.try_recv().is_ok());
    assert!(
        f.read("merge.args")
            .contains(&format!("--match-head-commit\n{}\n", f.head))
    );
    assert!(f.comments().await[0].body.contains("fusionnée"));
    f.delivery.merge(f.card_id).await.unwrap();
    f.delivery.refresh(f.card_id).await.unwrap();
    assert_eq!(f.comments().await.len(), 1);
    assert_eq!(
        f.read("commands.log")
            .lines()
            .filter(|action| *action == "merge")
            .count(),
        1
    );
}

#[tokio::test]
async fn a_successful_merge_command_without_confirmation_does_not_finish_the_card() {
    let f = Fixture::new().await;
    f.attach().await;
    assert!(f.delivery.merge(f.card_id).await.is_err());
    assert!(
        f.read("commands.log")
            .lines()
            .any(|action| action == "merge")
    );
    assert_eq!(f.saved().await.pr.state, PullRequestState::Open);
    assert!(f.saved().await.error.is_some());
    assert_eq!(f.category().await, Category::InReview);
    assert!(f.comments().await.is_empty());
}

#[tokio::test]
async fn refresh_recognizes_an_external_merge_of_the_latest_verified_run() {
    let f = Fixture::new().await;
    f.attach().await;
    f.json("view.json", &f.merged_pr());
    f.delivery.refresh(f.card_id).await.unwrap();
    assert_eq!(f.category().await, Category::Done);
    assert_eq!(f.saved().await.pr.state, PullRequestState::Merged);
    assert_eq!(f.comments().await.len(), 1);
    f.assert_no_merge_command();
}

#[tokio::test]
async fn an_external_merge_of_an_old_run_never_finishes_the_latest_run() {
    for status in [RunStatus::Succeeded, RunStatus::Failed] {
        let f = Fixture::new().await;
        f.attach().await;
        f.add_run(status).await;
        f.json("view.json", &f.merged_pr());
        f.delivery.refresh(f.card_id).await.unwrap();
        assert_eq!(f.saved().await.pr.state, PullRequestState::Merged);
        assert_eq!(f.saved().await.run_id, f.run_id);
        assert_eq!(f.category().await, Category::InReview);
        assert!(f.comments().await.is_empty());
        f.assert_no_merge_command();
    }
}

#[tokio::test]
async fn an_external_merge_requires_matching_head_branch_and_successful_local_checks() {
    for (field, value) in [("headRefOid", OTHER_HEAD), ("headRefName", "helm/other")] {
        let f = Fixture::new().await;
        f.attach().await;
        let mut merged = f.merged_pr();
        merged[field] = json!(value);
        f.json("view.json", &merged);
        f.delivery.refresh(f.card_id).await.unwrap();
        assert_eq!(f.category().await, Category::InReview);
        assert!(f.comments().await.is_empty());
    }
    let f = Fixture::with_verification(VerificationStatus::Failed).await;
    f.attach().await;
    f.json("view.json", &f.merged_pr());
    f.delivery.refresh(f.card_id).await.unwrap();
    assert_eq!(f.category().await, Category::InReview);
    assert!(f.comments().await.is_empty());
}

#[tokio::test]
async fn refresh_errors_preserve_the_last_snapshot_and_retry_clears_the_error() {
    let f = Fixture::new().await;
    f.attach().await;
    fs::write(f.state.join("view.exit"), "1").unwrap();
    fs::write(f.state.join("view.stderr"), "authentication failed").unwrap();
    assert!(
        f.delivery
            .refresh(f.card_id)
            .await
            .unwrap_err()
            .contains("authentication failed")
    );
    assert_eq!(f.saved().await.pr, f.pr());
    assert!(f.saved().await.error.is_some());
    assert!(!f.delivery.locks().contains(f.card_id));
    fs::remove_file(f.state.join("view.exit")).unwrap();
    fs::remove_file(f.state.join("view.stderr")).unwrap();
    f.delivery.refresh(f.card_id).await.unwrap();
    assert!(f.saved().await.error.is_none());
    assert_eq!(f.category().await, Category::InReview);
}

#[tokio::test]
async fn changing_the_configured_repository_blocks_operations_on_the_saved_pr() {
    let f = Fixture::new().await;
    f.attach().await;
    let mut config = Fixture::config();
    config.repository = "owner/different".to_owned();
    let delivery = Delivery::new(
        f.db.clone(),
        Changes::new(),
        RunGate::Open,
        Some(f.remote.repo.clone()),
        Some(config),
    );
    assert!(delivery.refresh(f.card_id).await.is_err());
    assert!(delivery.merge(f.card_id).await.is_err());
    f.assert_no_github_commands();
    assert_eq!(f.category().await, Category::InReview);
}

#[tokio::test]
async fn disabled_delivery_never_starts_an_external_process() {
    let f = Fixture::new().await;
    f.attach().await;
    for (gate, repo, config) in [
        (
            RunGate::NotLoopback,
            Some(f.remote.repo.clone()),
            Some(Fixture::config()),
        ),
        (
            RunGate::NoRepo,
            Some(f.remote.repo.clone()),
            Some(Fixture::config()),
        ),
        (RunGate::Open, None, Some(Fixture::config())),
        (RunGate::Open, Some(f.remote.repo.clone()), None),
    ] {
        let delivery = Delivery::new(f.db.clone(), Changes::new(), gate, repo, config);
        assert!(!delivery.enabled());
        assert!(delivery.publish(f.run_id, &f.head).await.is_err());
        assert!(delivery.retry(f.card_id).await.is_err());
        assert!(delivery.refresh(f.card_id).await.is_err());
        assert!(delivery.merge(f.card_id).await.is_err());
        tokio::time::timeout(Duration::from_secs(1), delivery.poll())
            .await
            .unwrap();
    }
    f.assert_no_github_commands();
    assert_eq!(f.category().await, Category::InReview);
}
