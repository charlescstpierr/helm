//! GitHub pull requests through the user's gh executable.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::GithubConfig;
use crate::process;

const TIMEOUT: Duration = Duration::from_secs(120);
const PR_FIELDS: &str = "number,url,state,isDraft,headRefOid,headRefName,baseRefName,statusCheckRollup,mergeable,mergeStateStatus,reviewDecision,isCrossRepository";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PullRequestState {
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CiStatus {
    None,
    Pending,
    Passed,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    pub number: i64,
    pub url: String,
    pub state: PullRequestState,
    pub draft: bool,
    pub head_sha: String,
    pub head_branch: String,
    pub base_branch: String,
    pub checks: CiStatus,
    pub mergeable: String,
    pub merge_state_status: String,
    pub review_decision: String,
}

impl PullRequest {
    pub fn state_label(&self) -> &'static str {
        match self.state {
            PullRequestState::Open if self.draft => "Brouillon",
            PullRequestState::Open => "Ouverte",
            PullRequestState::Closed => "Fermée",
            PullRequestState::Merged => "Fusionnée",
        }
    }

    pub fn checks_label(&self) -> &'static str {
        match self.checks {
            CiStatus::None => "Aucun contrôle",
            CiStatus::Pending => "En cours",
            CiStatus::Passed => "Réussis",
            CiStatus::Failed => "Échoués",
            CiStatus::Unknown => "Inconnus",
        }
    }

    pub fn merge_blocker(&self) -> Option<&'static str> {
        if self.state != PullRequestState::Open {
            return Some("La PR n'est pas ouverte.");
        }
        if self.draft {
            return Some("La PR est encore un brouillon.");
        }
        match self.checks {
            CiStatus::Pending => return Some("Les contrôles GitHub sont en cours."),
            CiStatus::Failed => return Some("Les contrôles GitHub ont échoué."),
            CiStatus::Unknown => return Some("L'état des contrôles GitHub est inconnu."),
            CiStatus::None | CiStatus::Passed => {}
        }
        match self.review_decision.as_str() {
            "CHANGES_REQUESTED" => return Some("Une revue demande des modifications."),
            "REVIEW_REQUIRED" => return Some("Une approbation est requise sur GitHub."),
            "" | "APPROVED" => {}
            _ => return Some("L'état des revues GitHub est inconnu."),
        }
        if self.mergeable != "MERGEABLE" {
            return Some("GitHub ne confirme pas que la PR peut être fusionnée.");
        }
        if self.merge_state_status != "CLEAN" {
            return Some("La PR ne satisfait pas les conditions de fusion GitHub.");
        }
        None
    }

    /// Validate even a persisted snapshot before using its identity or linking its URL.
    pub fn validate_for_repository(&self, repository: &str) -> Result<(), String> {
        let expected_url = format!("https://github.com/{repository}/pull/{}", self.number);
        if self.number <= 0
            || !self.url.eq_ignore_ascii_case(&expected_url)
            || !valid_sha(&self.head_sha)
            || self.head_branch.is_empty()
            || self.base_branch.is_empty()
        {
            return Err("L'identité de la PR renvoyée par GitHub est invalide.".to_owned());
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPullRequest {
    number: i64,
    url: String,
    state: String,
    is_draft: bool,
    head_ref_oid: String,
    head_ref_name: String,
    base_ref_name: String,
    is_cross_repository: bool,
    #[serde(default)]
    status_check_rollup: Value,
    mergeable: Option<String>,
    merge_state_status: Option<String>,
    review_decision: Option<String>,
}

fn valid_sha(sha: &str) -> bool {
    matches!(sha.len(), 40 | 64) && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn check_status(check: &Value) -> CiStatus {
    match check.get("__typename").and_then(Value::as_str) {
        Some("CheckRun") => match check.get("status").and_then(Value::as_str) {
            Some("QUEUED" | "IN_PROGRESS" | "PENDING" | "REQUESTED" | "WAITING") => {
                CiStatus::Pending
            }
            Some("COMPLETED") => match check.get("conclusion").and_then(Value::as_str) {
                Some("SUCCESS" | "NEUTRAL" | "SKIPPED") => CiStatus::Passed,
                Some(
                    "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE"
                    | "STALE",
                ) => CiStatus::Failed,
                _ => CiStatus::Unknown,
            },
            _ => CiStatus::Unknown,
        },
        Some("StatusContext") => match check.get("state").and_then(Value::as_str) {
            Some("SUCCESS") => CiStatus::Passed,
            Some("PENDING" | "EXPECTED") => CiStatus::Pending,
            Some("ERROR" | "FAILURE") => CiStatus::Failed,
            _ => CiStatus::Unknown,
        },
        _ => CiStatus::Unknown,
    }
}

fn ci_status(rollup: &Value) -> CiStatus {
    let Some(checks) = rollup.as_array() else {
        return CiStatus::Unknown;
    };
    if checks.is_empty() {
        return CiStatus::None;
    }
    let statuses: Vec<_> = checks.iter().map(check_status).collect();
    for status in [CiStatus::Failed, CiStatus::Unknown, CiStatus::Pending] {
        if statuses.contains(&status) {
            return status;
        }
    }
    CiStatus::Passed
}

pub struct Github {
    config: GithubConfig,
}

impl Github {
    pub fn new(config: GithubConfig) -> Self {
        Self { config }
    }

    pub async fn ensure(
        &self,
        cwd: &Path,
        branch: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<PullRequest, String> {
        if branch.is_empty() || base.is_empty() {
            return Err("La branche de la PR ou sa branche de base est vide.".to_owned());
        }
        if let Some(pr) = self.find_open(cwd, branch, base).await? {
            return self.ready(cwd, pr, branch, base).await;
        }
        let created = self
            .run(
                cwd,
                "create",
                &[
                    "--head",
                    branch,
                    "--base",
                    base,
                    "--title",
                    title,
                    "--body-file",
                    "-",
                ],
                Some(body),
            )
            .await;
        // A network error can follow a successful creation. Discover the PR again;
        // never issue a second creation merely because the first response was lost.
        match self.find_open(cwd, branch, base).await {
            Ok(Some(pr)) => self.ready(cwd, pr, branch, base).await,
            Ok(None) => Err(created.err().unwrap_or_else(|| {
                "La création a été demandée, mais GitHub ne confirme pas la PR.".to_owned()
            })),
            Err(error) => Err(match created {
                Ok(_) => error,
                Err(create_error) => format!("{create_error} Actualisation impossible : {error}"),
            }),
        }
    }

    pub async fn refresh(&self, cwd: &Path, number: i64) -> Result<PullRequest, String> {
        if number <= 0 {
            return Err("Le numéro de PR est invalide.".to_owned());
        }
        let output = self
            .run(
                cwd,
                "view",
                &[&number.to_string(), "--json", PR_FIELDS],
                None,
            )
            .await?;
        let pr = self.decode(&output)?;
        if pr.number != number {
            return Err("GitHub a renvoyé une autre PR que celle demandée.".to_owned());
        }
        Ok(pr)
    }

    pub async fn merge(
        &self,
        cwd: &Path,
        number: i64,
        expected_head: &str,
        expected_base: &str,
    ) -> Result<PullRequest, String> {
        if !valid_sha(expected_head) {
            return Err("Le commit attendu pour la fusion est invalide.".to_owned());
        }
        let current = self.refresh(cwd, number).await?;
        if current.base_branch != expected_base {
            return Err("La branche de base de la PR a changé depuis sa publication.".to_owned());
        }
        if current.head_sha != expected_head {
            return Err("Le commit de la PR a changé depuis les vérifications.".to_owned());
        }
        if current.state == PullRequestState::Merged {
            return Ok(current);
        }
        if let Some(blocker) = current.merge_blocker() {
            return Err(blocker.to_owned());
        }
        let merged = self
            .run(
                cwd,
                "merge",
                &[
                    &number.to_string(),
                    "--squash",
                    "--match-head-commit",
                    expected_head,
                ],
                None,
            )
            .await;
        let confirmed = self
            .refresh(cwd, number)
            .await
            .map_err(|error| match &merged {
                Ok(_) => format!("La fusion n'a pas pu être confirmée : {error}"),
                Err(merge_error) => format!("{merge_error} Confirmation impossible : {error}"),
            })?;
        if confirmed.base_branch != expected_base {
            return Err("La branche de base de la PR a changé pendant la fusion.".to_owned());
        }
        if confirmed.head_sha != expected_head {
            return Err("Le commit de la PR a changé pendant la fusion.".to_owned());
        }
        if confirmed.state == PullRequestState::Merged {
            return Ok(confirmed);
        }
        Err(merged
            .err()
            .unwrap_or_else(|| "GitHub n'a pas encore confirmé la fusion de cette PR.".to_owned()))
    }

    async fn run(
        &self,
        cwd: &Path,
        action: &str,
        args: &[&str],
        stdin: Option<&str>,
    ) -> Result<String, String> {
        // Qualifying the host also overrides a user's ambient GH_HOST setting.
        let repository = format!("github.com/{}", self.config.repository);
        let arguments: Vec<String> = ["pr", action, "--repo", &repository]
            .into_iter()
            .chain(args.iter().copied())
            .map(str::to_owned)
            .collect();
        let output = process::capture(&self.config.command, &arguments, cwd, stdin, TIMEOUT)
            .await
            .map_err(|error| format!("Impossible d'exécuter GitHub : {error}"))?;
        if output.timed_out {
            return Err(format!(
                "Le délai de la commande GitHub « {action} » est dépassé."
            ));
        }
        if !output.success() {
            return Err(format!(
                "La commande GitHub « {action} » a échoué : {}",
                output.stderr.trim()
            ));
        }
        Ok(output.stdout)
    }

    async fn find_open(
        &self,
        cwd: &Path,
        branch: &str,
        base: &str,
    ) -> Result<Option<PullRequest>, String> {
        let output = self
            .run(
                cwd,
                "list",
                &[
                    "--state", "open", "--head", branch, "--base", base, "--limit", "100",
                    "--json", PR_FIELDS,
                ],
                None,
            )
            .await?;
        let raw: Vec<RawPullRequest> = serde_json::from_str(&output)
            .map_err(|error| format!("La liste des PR GitHub est invalide : {error}"))?;
        if raw.len() > 1 {
            return Err("Plusieurs PR ouvertes correspondent à cette branche.".to_owned());
        }
        raw.into_iter()
            .next()
            .map(|raw| {
                let pr = self.convert(raw)?;
                Self::check_branches(&pr, branch, base)?;
                Ok(pr)
            })
            .transpose()
    }

    fn check_branches(pr: &PullRequest, branch: &str, base: &str) -> Result<(), String> {
        if pr.state != PullRequestState::Open || pr.head_branch != branch || pr.base_branch != base
        {
            return Err("La PR GitHub ne correspond pas aux branches attendues.".to_owned());
        }
        Ok(())
    }

    async fn ready(
        &self,
        cwd: &Path,
        pr: PullRequest,
        branch: &str,
        base: &str,
    ) -> Result<PullRequest, String> {
        if !pr.draft {
            return Ok(pr);
        }
        self.run(cwd, "ready", &[&pr.number.to_string()], None)
            .await?;
        let ready = self.refresh(cwd, pr.number).await?;
        Self::check_branches(&ready, branch, base)?;
        if ready.draft {
            return Err("GitHub n'a pas confirmé que la PR est prête à relire.".to_owned());
        }
        Ok(ready)
    }

    fn decode(&self, input: &str) -> Result<PullRequest, String> {
        let raw = serde_json::from_str(input)
            .map_err(|error| format!("Les données de la PR GitHub sont invalides : {error}"))?;
        self.convert(raw)
    }

    fn convert(&self, raw: RawPullRequest) -> Result<PullRequest, String> {
        if raw.is_cross_repository {
            return Err("La branche de la PR appartient à un autre dépôt.".to_owned());
        }
        let state = match raw.state.as_str() {
            "OPEN" => PullRequestState::Open,
            "CLOSED" => PullRequestState::Closed,
            "MERGED" => PullRequestState::Merged,
            _ => return Err("L'état de la PR GitHub est inconnu.".to_owned()),
        };
        let pr = PullRequest {
            number: raw.number,
            url: raw.url,
            state,
            draft: raw.is_draft,
            head_sha: raw.head_ref_oid,
            head_branch: raw.head_ref_name,
            base_branch: raw.base_ref_name,
            checks: ci_status(&raw.status_check_rollup),
            mergeable: raw.mergeable.unwrap_or_else(|| "UNKNOWN".to_owned()),
            merge_state_status: raw
                .merge_state_status
                .unwrap_or_else(|| "UNKNOWN".to_owned()),
            review_decision: raw.review_decision.unwrap_or_else(|| "UNKNOWN".to_owned()),
        };
        pr.validate_for_repository(&self.config.repository)?;
        Ok(pr)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use serde_json::{Value, json};

    use super::*;

    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";
    const OTHER_HEAD: &str = "abcdef0123456789abcdef0123456789abcdef01";

    fn raw_pr() -> Value {
        json!({
            "number": 7,
            "url": "https://github.com/owner/repo/pull/7",
            "state": "OPEN",
            "isDraft": false,
            "headRefOid": HEAD,
            "headRefName": "helm/CARD-1",
            "baseRefName": "main",
            "statusCheckRollup": [],
            "mergeable": "MERGEABLE",
            "mergeStateStatus": "CLEAN",
            "reviewDecision": "",
            "isCrossRepository": false,
        })
    }

    struct Fixture {
        cwd: PathBuf,
        state: PathBuf,
        github: Github,
    }

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let cwd = std::env::temp_dir().join(format!(
                "helm-github-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let state = PathBuf::from(format!("{}.gh", cwd.display()));
            fs::create_dir_all(&cwd).unwrap();
            fs::create_dir_all(&state).unwrap();
            let github = Github::new(GithubConfig {
                repository: "owner/repo".to_owned(),
                command: Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-gh.sh"),
            });
            let fixture = Self { cwd, state, github };
            fixture.json("list.json", &json!([]));
            fixture.json("view.json", &raw_pr());
            fixture
        }

        fn json(&self, name: &str, value: &Value) {
            self.write(name, &value.to_string());
        }

        fn write(&self, name: &str, value: &str) {
            fs::write(self.state.join(name), value).unwrap();
        }

        fn read(&self, name: &str) -> String {
            fs::read_to_string(self.state.join(name)).unwrap_or_default()
        }

        async fn ensure(&self) -> Result<PullRequest, String> {
            self.github
                .ensure(
                    &self.cwd,
                    "helm/CARD-1",
                    "main",
                    "Titre avec ' et $(jamais)",
                    "Un corps\navec `du code` et $(jamais).\n",
                )
                .await
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.cwd);
            let _ = fs::remove_dir_all(&self.state);
        }
    }

    #[test]
    fn empty_checks_are_distinct_from_success_and_missing_checks_are_unknown() {
        let f = Fixture::new();
        let pr = f.github.decode(&raw_pr().to_string()).unwrap();
        assert_eq!(pr.checks, CiStatus::None);
        assert_eq!(pr.merge_blocker(), None);
        assert_eq!(pr.checks_label(), "Aucun contrôle");
        assert_eq!(pr.state_label(), "Ouverte");
        for absent in [Value::Null, json!({}), json!([{}])] {
            let mut raw = raw_pr();
            raw["statusCheckRollup"] = absent;
            let pr = f.github.decode(&raw.to_string()).unwrap();
            assert_eq!(pr.checks, CiStatus::Unknown);
            assert!(pr.merge_blocker().is_some());
        }
        let mut raw = raw_pr();
        raw.as_object_mut().unwrap().remove("statusCheckRollup");
        assert_eq!(
            f.github.decode(&raw.to_string()).unwrap().checks,
            CiStatus::Unknown
        );
    }

    #[test]
    fn decodes_check_runs_and_legacy_statuses_without_assuming_unknown_is_success() {
        let f = Fixture::new();
        for (check, expected) in [
            (
                json!({"__typename":"CheckRun","status":"COMPLETED","conclusion":"SUCCESS"}),
                CiStatus::Passed,
            ),
            (
                json!({"__typename":"CheckRun","status":"COMPLETED","conclusion":"NEUTRAL"}),
                CiStatus::Passed,
            ),
            (
                json!({"__typename":"CheckRun","status":"COMPLETED","conclusion":"SKIPPED"}),
                CiStatus::Passed,
            ),
            (
                json!({"__typename":"CheckRun","status":"IN_PROGRESS","conclusion":null}),
                CiStatus::Pending,
            ),
            (
                json!({"__typename":"CheckRun","status":"QUEUED","conclusion":""}),
                CiStatus::Pending,
            ),
            (
                json!({"__typename":"CheckRun","status":"WAITING","conclusion":""}),
                CiStatus::Pending,
            ),
            (
                json!({"__typename":"CheckRun","status":"COMPLETED","conclusion":"FAILURE"}),
                CiStatus::Failed,
            ),
            (
                json!({"__typename":"CheckRun","status":"COMPLETED","conclusion":"CANCELLED"}),
                CiStatus::Failed,
            ),
            (
                json!({"__typename":"CheckRun","status":"COMPLETED","conclusion":"TIMED_OUT"}),
                CiStatus::Failed,
            ),
            (
                json!({"__typename":"CheckRun","status":"COMPLETED","conclusion":null}),
                CiStatus::Unknown,
            ),
            (
                json!({"__typename":"CheckRun","status":"FUTURE","conclusion":"SUCCESS"}),
                CiStatus::Unknown,
            ),
            (
                json!({"__typename":"StatusContext","state":"SUCCESS"}),
                CiStatus::Passed,
            ),
            (
                json!({"__typename":"StatusContext","state":"PENDING"}),
                CiStatus::Pending,
            ),
            (
                json!({"__typename":"StatusContext","state":"ERROR"}),
                CiStatus::Failed,
            ),
            (
                json!({"__typename":"StatusContext","state":"FUTURE"}),
                CiStatus::Unknown,
            ),
            (
                json!({"__typename":"FutureCheck","state":"SUCCESS"}),
                CiStatus::Unknown,
            ),
        ] {
            let mut raw = raw_pr();
            raw["statusCheckRollup"] = json!([check]);
            assert_eq!(
                f.github.decode(&raw.to_string()).unwrap().checks,
                expected,
                "{check}"
            );
        }
        let mut raw = raw_pr();
        raw["statusCheckRollup"] = json!([
            {"__typename":"StatusContext","state":"SUCCESS"},
            {"__typename":"StatusContext","state":"PENDING"},
            {"__typename":"StatusContext","state":"FAILURE"}
        ]);
        assert_eq!(
            f.github.decode(&raw.to_string()).unwrap().checks,
            CiStatus::Failed
        );
    }

    #[test]
    fn malformed_pr_identity_and_untrusted_urls_are_rejected() {
        let f = Fixture::new();
        for (field, value) in [
            ("number", json!(0)),
            ("number", json!("7")),
            ("headRefOid", json!("abc")),
            ("headRefName", json!("")),
            ("baseRefName", json!("")),
            ("isDraft", Value::Null),
            ("state", json!("FUTURE")),
            ("url", json!("javascript:alert(1)")),
            (
                "url",
                json!("https://github.com.attacker.test/owner/repo/pull/7"),
            ),
            ("url", json!("https://github.com/other/repo/pull/7")),
            ("url", json!("https://github.com/owner/repo/pull/8")),
        ] {
            let mut raw = raw_pr();
            raw[field] = value;
            assert!(f.github.decode(&raw.to_string()).is_err(), "{raw}");
        }
        assert!(f.github.decode("not json").is_err());
        assert!(f.github.decode("{}").is_err());
    }

    #[tokio::test]
    async fn creates_ready_pr_with_explicit_repo_and_stdin_body() {
        let f = Fixture::new();
        f.json("after-create.json", &raw_pr());
        let pr = f.ensure().await.unwrap();
        assert_eq!(pr.number, 7);
        assert!(!pr.draft);
        assert_eq!(
            f.read("create.body"),
            "Un corps\navec `du code` et $(jamais).\n"
        );
        let create = f.read("create.args");
        assert!(create.contains("--body-file\n-\n"));
        assert!(!create.contains("--draft"));
        for action in ["create", "list"] {
            assert!(
                f.read(&format!("{action}.args"))
                    .contains("--repo\ngithub.com/owner/repo\n")
            );
        }
        assert!(f.read("list.args").contains("--head\nhelm/CARD-1\n"));
        assert!(f.read("list.args").contains("--base\nmain\n"));
    }

    #[tokio::test]
    async fn reuses_the_open_pr_and_readies_existing_drafts() {
        let f = Fixture::new();
        f.json("list.json", &json!([raw_pr()]));
        assert_eq!(f.ensure().await.unwrap().number, 7);
        assert_eq!(f.read("commands.log"), "list\n");
        let mut draft = raw_pr();
        draft["isDraft"] = json!(true);
        f.json("list.json", &json!([draft]));
        f.json("after-ready.json", &raw_pr());
        assert!(!f.ensure().await.unwrap().draft);
        assert!(
            f.read("ready.args")
                .contains("--repo\ngithub.com/owner/repo\n")
        );
        assert!(!f.read("commands.log").contains("create"));
    }

    #[tokio::test]
    async fn never_creates_when_discovery_is_uncertain_or_mismatched() {
        let f = Fixture::new();
        f.write("list.exit", "1");
        f.write("list.stderr", "authentication failed");
        assert!(
            f.ensure()
                .await
                .unwrap_err()
                .contains("authentication failed")
        );
        assert!(!f.read("commands.log").contains("create"));
        fs::remove_file(f.state.join("list.exit")).unwrap();
        for (field, value) in [
            ("headRefName", json!("other")),
            ("baseRefName", json!("develop")),
            ("isCrossRepository", json!(true)),
            ("state", json!("CLOSED")),
        ] {
            let mut raw = raw_pr();
            raw[field] = value;
            f.json("list.json", &json!([raw]));
            assert!(f.ensure().await.is_err(), "{field}");
            assert!(!f.read("commands.log").contains("create"));
        }
        f.json("list.json", &json!([raw_pr(), raw_pr()]));
        assert!(f.ensure().await.is_err());
        assert!(!f.read("commands.log").contains("create"));
    }

    #[tokio::test]
    async fn uncertain_create_recovers_the_existing_pr_without_a_second_creation() {
        let f = Fixture::new();
        f.json("after-create.json", &raw_pr());
        f.write("create.exit", "1");
        f.write("create.stderr", "connection lost after creation");
        assert_eq!(f.ensure().await.unwrap().number, 7);
        assert_eq!(f.ensure().await.unwrap().number, 7);
        assert_eq!(
            f.read("commands.log")
                .lines()
                .filter(|line| *line == "create")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn create_error_without_confirmation_remains_an_error() {
        let f = Fixture::new();
        f.write("create.exit", "1");
        f.write("create.stderr", "permission denied");
        assert!(f.ensure().await.unwrap_err().contains("permission denied"));
        assert_eq!(
            f.read("commands.log")
                .lines()
                .filter(|line| *line == "create")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn refresh_requires_the_requested_pr_number() {
        let f = Fixture::new();
        let pr = f.github.refresh(&f.cwd, 7).await.unwrap();
        assert_eq!(pr.number, 7);
        assert!(
            f.read("view.args")
                .contains("--repo\ngithub.com/owner/repo\n")
        );
        assert!(f.github.refresh(&f.cwd, 8).await.is_err());
        assert!(f.github.refresh(&f.cwd, -1).await.is_err());
    }

    #[tokio::test]
    async fn merge_requires_fresh_matching_head_and_nonblocking_checks_and_review() {
        let f = Fixture::new();
        for (field, value) in [
            ("headRefOid", json!(OTHER_HEAD)),
            ("baseRefName", json!("develop")),
            ("isDraft", json!(true)),
            ("state", json!("CLOSED")),
            ("mergeable", json!("CONFLICTING")),
            ("mergeable", json!("UNKNOWN")),
            ("mergeStateStatus", json!("BLOCKED")),
            ("mergeStateStatus", json!("UNKNOWN")),
            ("mergeStateStatus", json!("BEHIND")),
            ("reviewDecision", json!("CHANGES_REQUESTED")),
            ("reviewDecision", json!("REVIEW_REQUIRED")),
            ("reviewDecision", Value::Null),
            (
                "statusCheckRollup",
                json!([{"__typename":"StatusContext","state":"PENDING"}]),
            ),
            (
                "statusCheckRollup",
                json!([{"__typename":"StatusContext","state":"FAILURE"}]),
            ),
            ("statusCheckRollup", Value::Null),
        ] {
            let mut raw = raw_pr();
            raw[field] = value;
            f.json("view.json", &raw);
            assert!(
                f.github.merge(&f.cwd, 7, HEAD, "main").await.is_err(),
                "{field}"
            );
            assert!(!f.read("commands.log").lines().any(|line| line == "merge"));
        }
    }

    #[tokio::test]
    async fn merge_matches_head_without_admin_and_confirms_merged() {
        let f = Fixture::new();
        let mut merged = raw_pr();
        merged["state"] = json!("MERGED");
        f.json("after-merge.json", &merged);
        let pr = f.github.merge(&f.cwd, 7, HEAD, "main").await.unwrap();
        assert_eq!(pr.state, PullRequestState::Merged);
        let args = f.read("merge.args");
        assert!(args.contains("--repo\ngithub.com/owner/repo\n"));
        assert!(args.contains("--squash\n"));
        assert!(args.contains(&format!("--match-head-commit\n{HEAD}\n")));
        assert!(!args.contains("--admin"));
        assert!(!args.contains("--auto"));
        assert_eq!(f.read("commands.log"), "view\nmerge\nview\n");
    }

    #[tokio::test]
    async fn successful_merge_command_is_not_confirmation_and_changed_head_is_rejected() {
        let f = Fixture::new();
        assert!(f.github.merge(&f.cwd, 7, HEAD, "main").await.is_err());
        let mut merged = raw_pr();
        merged["state"] = json!("MERGED");
        merged["headRefOid"] = json!(OTHER_HEAD);
        f.json("after-merge.json", &merged);
        assert!(f.github.merge(&f.cwd, 7, HEAD, "main").await.is_err());
    }

    #[tokio::test]
    async fn changed_base_during_merge_is_not_confirmation() {
        let f = Fixture::new();
        let mut merged = raw_pr();
        merged["state"] = json!("MERGED");
        merged["baseRefName"] = json!("develop");
        f.json("after-merge.json", &merged);
        assert!(f.github.merge(&f.cwd, 7, HEAD, "main").await.is_err());
    }

    #[tokio::test]
    async fn already_merged_matching_head_is_idempotent_and_persistence_round_trips() {
        let f = Fixture::new();
        let mut merged = raw_pr();
        merged["state"] = json!("MERGED");
        f.json("view.json", &merged);
        let pr = f.github.merge(&f.cwd, 7, HEAD, "main").await.unwrap();
        assert_eq!(pr.state, PullRequestState::Merged);
        assert!(!f.read("commands.log").lines().any(|line| line == "merge"));
        let persisted = serde_json::to_string(&pr).unwrap();
        assert_eq!(serde_json::from_str::<PullRequest>(&persisted).unwrap(), pr);
        assert!(f.github.merge(&f.cwd, 7, OTHER_HEAD, "main").await.is_err());
    }
}
