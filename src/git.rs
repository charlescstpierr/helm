//! The git operations a run needs: a worktree per card, the branch's state on origin, a push.
//! Each shells out to `git`, so the user's own configuration (identity, credentials, hooks)
//! applies.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

const REMOTE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug)]
pub struct GitError(String);

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GitError {}

type Result<T> = std::result::Result<T, GitError>;

fn error<T>(message: impl Into<String>) -> Result<T> {
    Err(GitError(message.into()))
}

/// Runs `git -C <dir> <args>` and returns its trimmed stdout. A failure carries git's stderr.
async fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        // A credential prompt would hang an unattended run forever.
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| GitError(format!("cannot run git: {e}")))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    error(format!(
        "git {} failed ({}){}{detail}",
        args.first().copied().unwrap_or(""),
        output.status,
        if detail.is_empty() { "" } else { ": " },
    ))
}

async fn succeeds(dir: &Path, args: &[&str]) -> bool {
    git(dir, args).await.is_ok()
}

/// The project repository must be a git repository: checked once, at startup.
pub async fn check_repository(repo: &Path) -> Result<()> {
    git(repo, &["rev-parse", "--git-dir"])
        .await
        .map(drop)
        .map_err(|e| {
            GitError(format!(
                "{} is not a usable git repository: {e}",
                repo.display()
            ))
        })
}

/// The branch new worktrees start from: the one `origin/HEAD` names when it is set, else
/// `main`, else `master`, else whatever the repository has checked out.
pub async fn default_branch(repo: &Path) -> Result<String> {
    let exists = |name: String| async move {
        succeeds(
            repo,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{name}"),
            ],
        )
        .await
    };
    if let Ok(origin_head) = git(
        repo,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .await
    {
        if let Some(name) = origin_head.strip_prefix("origin/") {
            if exists(name.to_owned()).await {
                return Ok(name.to_owned());
            }
        }
    }
    for name in ["main", "master"] {
        if exists(name.to_owned()).await {
            return Ok(name.to_owned());
        }
    }
    git(repo, &["symbolic-ref", "--short", "HEAD"])
        .await
        .or_else(|_| {
            error("cannot tell the default branch: no main, no master, and HEAD is detached")
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
}

pub fn branch_name(key: &str, number: i64) -> String {
    format!("helm/{key}-{number}")
}

async fn canonicalize(path: &Path) -> Result<PathBuf> {
    tokio::fs::canonicalize(path)
        .await
        .map_err(|e| GitError(format!("cannot resolve {}: {e}", path.display())))
}

async fn belongs_to_repository(repo: &Path, path: &Path) -> Result<bool> {
    // Git also discovers repositories in parent directories. The card directory must
    // itself be the checkout root, not just a directory inside a matching checkout.
    let top_level = git(path, &["rev-parse", "--show-toplevel"]).await?;
    if canonicalize(Path::new(&top_level)).await? != canonicalize(path).await? {
        return Ok(false);
    }

    // Branch names alone do not identify a repository. Linked worktrees share the
    // same common git directory; an unrelated checkout does not.
    let common_dir = git(path, &["rev-parse", "--git-common-dir"]).await?;
    let repo_common_dir = git(repo, &["rev-parse", "--git-common-dir"]).await?;
    Ok(canonicalize(&path.join(common_dir)).await?
        == canonicalize(&repo.join(repo_common_dir)).await?)
}

/// Makes sure the card has its worktree and returns it. Safe to repeat: an existing worktree
/// from this repository on the card's branch is reused, an existing branch without a
/// worktree is checked out again, and stale worktree records are pruned first.
pub async fn prepare_worktree(
    repo: &Path,
    root: &Path,
    key: &str,
    number: i64,
) -> Result<Worktree> {
    if key.is_empty()
        || !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return error(format!("project key {key:?} cannot name a directory"));
    }
    let branch = branch_name(key, number);
    tokio::fs::create_dir_all(root)
        .await
        .map_err(|e| GitError(format!("cannot create {}: {e}", root.display())))?;
    // `git -C repo` resolves relative arguments from the repository, while this root
    // is configured relative to Helm's working directory.
    let path = std::path::absolute(root)
        .map_err(|e| GitError(format!("cannot resolve {}: {e}", root.display())))?
        .join(format!("{key}-{number}"));

    git(repo, &["worktree", "prune"]).await?;
    if path.exists() {
        let head = git(&path, &["symbolic-ref", "--short", "HEAD"]).await;
        return match head {
            Ok(current)
                if current == branch
                    && belongs_to_repository(repo, &path).await.unwrap_or(false) =>
            {
                Ok(Worktree { path, branch })
            }
            _ => error(format!(
                "{} exists but is not the worktree of branch {branch}",
                path.display()
            )),
        };
    }

    let path_arg = path.to_string_lossy().into_owned();
    if succeeds(
        repo,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .await
    {
        git(repo, &["worktree", "add", &path_arg, &branch]).await?;
    } else {
        let base = default_branch(repo).await?;
        git(repo, &["worktree", "add", "-b", &branch, &path_arg, &base]).await?;
    }
    Ok(Worktree { path, branch })
}

/// Reuses only the saved checkout for a correction. Its branch and repository must still
/// match; missing worktrees are not recreated, and unfinished files are left untouched.
pub async fn resume_worktree(repo: &Path, path: &Path, branch: &str) -> Result<Worktree> {
    if !tokio::fs::metadata(path)
        .await
        .is_ok_and(|metadata| metadata.is_dir())
    {
        return error(format!(
            "Le worktree à reprendre est introuvable ou inaccessible : {}",
            path.display()
        ));
    }
    if !belongs_to_repository(repo, path).await? {
        return error(format!(
            "{} n'est pas la racine d'un worktree de ce dépôt",
            path.display()
        ));
    }
    let current = git(path, &["symbolic-ref", "--short", "HEAD"]).await?;
    if current != branch {
        return error(format!(
            "Le worktree à reprendre est sur la branche {current}, attendue : {branch}"
        ));
    }
    Ok(Worktree {
        path: path.to_owned(),
        branch: branch.to_owned(),
    })
}

/// What a push of the card's branch would put on origin.
#[derive(Debug, PartialEq, Eq)]
pub enum Unpublished {
    /// Origin already has everything the branch holds.
    Nothing,
    /// This many commits the branch holds and origin lacks. Never zero.
    Commits(u64),
    /// Origin has commits the branch lacks (rewritten history, or work pushed from elsewhere):
    /// a push would be refused and Helm never forces.
    Diverged,
}

/// Compares the worktree's branch with the branch as origin holds it right now, or with the
/// default branch when origin has none yet. Asks origin through `ls-remote` rather than
/// trusting `refs/remotes/origin/*`, which can be stale: the branch may have been deleted on
/// the remote, or never fetched. Fails when origin cannot be reached.
pub async fn unpublished(repo: &Path, worktree: &Worktree) -> Result<Unpublished> {
    let dir = worktree.path.as_path();
    let range = match remote_tip(dir, &worktree.branch).await? {
        None => default_branch(repo).await?,
        Some(tip) => {
            let known = succeeds(dir, &["cat-file", "-e", &format!("{tip}^{{commit}}")]).await;
            if !known {
                return Ok(Unpublished::Diverged);
            }
            if succeeds(dir, &["merge-base", "--is-ancestor", "HEAD", &tip]).await {
                return Ok(Unpublished::Nothing);
            }
            if !succeeds(dir, &["merge-base", "--is-ancestor", &tip, "HEAD"]).await {
                return Ok(Unpublished::Diverged);
            }
            tip
        }
    };
    let count = git(dir, &["rev-list", "--count", &format!("{range}..HEAD")]).await?;
    match count.parse::<u64>() {
        Ok(0) => Ok(Unpublished::Nothing),
        Ok(n) => Ok(Unpublished::Commits(n)),
        Err(_) => error(format!("unexpected commit count {count:?}")),
    }
}

async fn remote_tip(dir: &Path, branch: &str) -> Result<Option<String>> {
    let full = format!("refs/heads/{branch}");
    let listing = with_remote_timeout(
        "git ls-remote",
        git(dir, &["ls-remote", "--heads", "origin", &full]),
    )
    .await?;
    Ok(listing.lines().find_map(|line| {
        let (sha, name) = line.split_once('\t')?;
        (name == full).then(|| sha.to_owned())
    }))
}

async fn with_remote_timeout(
    what: &str,
    run: impl std::future::Future<Output = Result<String>>,
) -> Result<String> {
    match tokio::time::timeout(REMOTE_TIMEOUT, run).await {
        Ok(result) => result,
        Err(_) => error(format!(
            "{what} did not finish within {} seconds",
            REMOTE_TIMEOUT.as_secs()
        )),
    }
}

pub async fn has_uncommitted_changes(worktree: &Path) -> Result<bool> {
    Ok(!git(worktree, &["status", "--porcelain"]).await?.is_empty())
}

/// The commit on the expected branch. Checks and PRs must never silently switch branches.
pub async fn checked_head(worktree: &Path, branch: &str, require_clean: bool) -> Result<String> {
    let current = git(worktree, &["symbolic-ref", "--short", "HEAD"]).await?;
    if current != branch {
        return error(format!("expected branch {branch}, found {current}"));
    }
    if require_clean && has_uncommitted_changes(worktree).await? {
        return error("des modifications non commitées restent dans le worktree");
    }
    git(worktree, &["rev-parse", "--verify", "HEAD^{commit}"]).await
}

/// Push the exact checked commit, even if a hook or another process moves the local ref.
pub async fn push_commit(worktree: &Path, branch: &str, commit: &str) -> Result<()> {
    if !matches!(commit.len(), 40 | 64) || !commit.bytes().all(|c| c.is_ascii_hexdigit()) {
        return error("invalid commit object id");
    }
    let refspec = format!("{commit}:refs/heads/{branch}");
    with_remote_timeout("git push", git(worktree, &["push", "origin", &refspec]))
        .await
        .map(drop)
}

/// Pushes the branch to `origin`. Never forced: a rejected push is a failed run.
pub async fn push(worktree: &Path, branch: &str) -> Result<()> {
    let args = ["push", "--set-upstream", "origin", branch];
    with_remote_timeout("git push", git(worktree, &args))
        .await
        .map(drop)
}

#[cfg(test)]
pub mod testing {
    //! A throwaway repository with a bare `origin`, for tests that need real git.

    use std::path::{Path, PathBuf};
    use std::process::Command;

    pub struct Remote {
        pub dir: PathBuf,
        pub repo: PathBuf,
        pub origin: PathBuf,
        pub worktrees: PathBuf,
    }

    pub fn run(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    impl Remote {
        pub fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("helm-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let repo = dir.join("repo");
            let origin = dir.join("origin.git");
            let worktrees = dir.join("worktrees");
            std::fs::create_dir_all(&repo).unwrap();
            std::fs::create_dir_all(&origin).unwrap();
            run(&origin, &["init", "--bare", "-q"]);
            run(&repo, &["init", "-q", "-b", "main"]);
            std::fs::write(repo.join("README.md"), "# project\n").unwrap();
            run(&repo, &["add", "."]);
            run(&repo, &["commit", "-q", "-m", "init"]);
            run(
                &repo,
                &["remote", "add", "origin", origin.to_str().unwrap()],
            );
            run(&repo, &["push", "-q", "-u", "origin", "main"]);
            Self {
                dir,
                repo,
                origin,
                worktrees,
            }
        }
    }

    impl Drop for Remote {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{Remote, run};
    use super::*;

    #[tokio::test]
    async fn a_worktree_is_created_on_a_branch_cut_from_the_default_branch() {
        let remote = Remote::new("git-create");
        let wt = prepare_worktree(&remote.repo, &remote.worktrees, "HELM", 3)
            .await
            .unwrap();

        assert_eq!(wt.branch, "helm/HELM-3");
        assert_eq!(wt.path, remote.worktrees.join("HELM-3"));
        assert!(wt.path.join("README.md").exists());
        assert_eq!(
            run(&wt.path, &["symbolic-ref", "--short", "HEAD"]),
            "helm/HELM-3"
        );
        assert_eq!(default_branch(&remote.repo).await.unwrap(), "main");
        // The main checkout stays on main and clean.
        assert_eq!(
            run(&remote.repo, &["symbolic-ref", "--short", "HEAD"]),
            "main"
        );
        assert_eq!(run(&remote.repo, &["status", "--porcelain"]), "");
    }

    #[tokio::test]
    async fn a_relative_worktree_root_is_resolved_from_the_process_directory() {
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        let remote = Remote::new("git-relative-root");
        let root =
            PathBuf::from("target").join(format!("helm-relative-worktrees-{}", std::process::id()));
        let _cleanup = Cleanup(root.clone());
        let expected = std::env::current_dir().unwrap().join(&root).join("HELM-1");
        let wt = prepare_worktree(&remote.repo, &root, "HELM", 1)
            .await
            .unwrap();

        assert!(
            expected.join("README.md").exists(),
            "{}",
            expected.display()
        );
        assert_eq!(wt.path, expected);
        assert_eq!(
            prepare_worktree(&remote.repo, &root, "HELM", 1)
                .await
                .unwrap(),
            wt
        );
    }

    #[tokio::test]
    async fn preparing_twice_reuses_the_worktree_and_a_deleted_one_is_recreated_on_its_branch() {
        let remote = Remote::new("git-idempotent");
        let first = prepare_worktree(&remote.repo, &remote.worktrees, "HELM", 1)
            .await
            .unwrap();
        std::fs::write(first.path.join("work.txt"), "w").unwrap();
        run(&first.path, &["add", "."]);
        run(&first.path, &["commit", "-q", "-m", "work"]);

        let again = prepare_worktree(&remote.repo, &remote.worktrees, "HELM", 1)
            .await
            .unwrap();
        assert_eq!(again, first);

        // The directory is deleted behind git's back; the branch and its commit survive.
        std::fs::remove_dir_all(&first.path).unwrap();
        let revived = prepare_worktree(&remote.repo, &remote.worktrees, "HELM", 1)
            .await
            .unwrap();
        assert!(revived.path.join("work.txt").exists());
    }

    #[tokio::test]
    async fn resuming_the_existing_worktree_preserves_commits_staged_and_untracked_changes() {
        let remote = Remote::new("git-resume-existing");
        let wt = card_worktree(&remote).await;
        commit(&wt, "first-attempt.txt").await;
        std::fs::write(wt.path.join("README.md"), "Correction in progress\n").unwrap();
        run(&wt.path, &["add", "README.md"]);
        std::fs::write(wt.path.join("notes.txt"), "keep these notes").unwrap();
        let head = run(&wt.path, &["rev-parse", "HEAD"]);
        let status = run(&wt.path, &["status", "--porcelain"]);

        let resumed = resume_worktree(&remote.repo, &wt.path, &wt.branch)
            .await
            .unwrap();

        assert_eq!(resumed, wt);
        assert_eq!(run(&wt.path, &["rev-parse", "HEAD"]), head);
        assert_eq!(run(&wt.path, &["status", "--porcelain"]), status);
        assert_eq!(
            std::fs::read_to_string(wt.path.join("notes.txt")).unwrap(),
            "keep these notes"
        );
    }

    #[tokio::test]
    async fn resuming_a_deleted_worktree_neither_recreates_it_nor_prunes_its_record() {
        let remote = Remote::new("git-resume-missing");
        let wt = card_worktree(&remote).await;
        commit(&wt, "first-attempt.txt").await;
        let head = run(&remote.repo, &["rev-parse", &wt.branch]);
        std::fs::remove_dir_all(&wt.path).unwrap();
        let registered = run(&remote.repo, &["worktree", "list", "--porcelain"]);

        assert!(
            resume_worktree(&remote.repo, &wt.path, &wt.branch)
                .await
                .is_err()
        );

        assert!(!wt.path.exists());
        assert_eq!(run(&remote.repo, &["rev-parse", &wt.branch]), head);
        assert_eq!(
            run(&remote.repo, &["worktree", "list", "--porcelain"]),
            registered
        );
    }

    #[tokio::test]
    async fn resuming_a_worktree_in_another_repository_is_refused_without_changes() {
        let remote = Remote::new("git-resume-repo");
        let other = Remote::new("git-resume-other-repo");
        let foreign = card_worktree(&other).await;
        std::fs::write(foreign.path.join("notes.txt"), "keep foreign notes").unwrap();

        assert!(
            resume_worktree(&remote.repo, &foreign.path, &foreign.branch)
                .await
                .is_err()
        );

        assert_eq!(
            run(&foreign.path, &["symbolic-ref", "--short", "HEAD"]),
            foreign.branch
        );
        assert_eq!(
            std::fs::read_to_string(foreign.path.join("notes.txt")).unwrap(),
            "keep foreign notes"
        );
    }

    #[tokio::test]
    async fn resuming_with_a_different_branch_or_detached_head_does_not_switch_it() {
        let remote = Remote::new("git-resume-branch");
        let wt = card_worktree(&remote).await;
        run(&wt.path, &["checkout", "-q", "-b", "other-work"]);
        std::fs::write(wt.path.join("notes.txt"), "keep current notes").unwrap();

        assert!(
            resume_worktree(&remote.repo, &wt.path, &wt.branch)
                .await
                .is_err()
        );
        assert_eq!(
            run(&wt.path, &["symbolic-ref", "--short", "HEAD"]),
            "other-work"
        );

        run(&wt.path, &["checkout", "-q", "--detach"]);
        assert!(
            resume_worktree(&remote.repo, &wt.path, &wt.branch)
                .await
                .is_err()
        );
        assert_eq!(
            run(&wt.path, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "HEAD"
        );
        assert_eq!(
            std::fs::read_to_string(wt.path.join("notes.txt")).unwrap(),
            "keep current notes"
        );
    }

    #[tokio::test]
    async fn resuming_a_subdirectory_of_the_expected_worktree_is_refused() {
        let remote = Remote::new("git-resume-subdir");
        let wt = card_worktree(&remote).await;
        let subdir = wt.path.join("src");
        std::fs::create_dir_all(&subdir).unwrap();
        std::fs::write(subdir.join("notes.txt"), "keep nested notes").unwrap();

        assert!(
            resume_worktree(&remote.repo, &subdir, &wt.branch)
                .await
                .is_err()
        );

        assert_eq!(
            std::fs::read_to_string(subdir.join("notes.txt")).unwrap(),
            "keep nested notes"
        );
    }

    #[tokio::test]
    async fn resuming_a_plain_directory_or_file_is_refused_without_modifying_it() {
        let remote = Remote::new("git-resume-not-checkout");
        let path = remote.dir.join("plain");
        std::fs::create_dir_all(&path).unwrap();
        let file = path.join("notes.txt");
        std::fs::write(&file, "keep plain notes").unwrap();

        assert!(
            resume_worktree(&remote.repo, &path, "helm/HELM-1")
                .await
                .is_err()
        );
        assert!(
            resume_worktree(&remote.repo, &file, "helm/HELM-1")
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(file).unwrap(), "keep plain notes");
    }

    async fn commit(wt: &Worktree, file: &str) {
        std::fs::write(wt.path.join(file), file).unwrap();
        run(&wt.path, &["add", "."]);
        run(&wt.path, &["commit", "-q", "-m", file]);
    }

    async fn card_worktree(remote: &Remote) -> Worktree {
        prepare_worktree(&remote.repo, &remote.worktrees, "HELM", 1)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_branch_origin_does_not_have_is_compared_with_the_default_branch() {
        let remote = Remote::new("git-unpublished-new");
        let wt = card_worktree(&remote).await;
        let state = unpublished(&remote.repo, &wt).await.unwrap();
        assert_eq!(state, Unpublished::Nothing);

        commit(&wt, "a").await;
        commit(&wt, "b").await;
        let state = unpublished(&remote.repo, &wt).await.unwrap();
        assert_eq!(state, Unpublished::Commits(2));
    }

    #[tokio::test]
    async fn a_branch_equal_to_origins_has_nothing_to_publish_and_one_ahead_has_the_difference() {
        let remote = Remote::new("git-unpublished-equal");
        let wt = card_worktree(&remote).await;
        commit(&wt, "a").await;
        push(&wt.path, &wt.branch).await.unwrap();
        let state = unpublished(&remote.repo, &wt).await.unwrap();
        assert_eq!(state, Unpublished::Nothing);

        commit(&wt, "b").await;
        let state = unpublished(&remote.repo, &wt).await.unwrap();
        assert_eq!(state, Unpublished::Commits(1));

        run(&wt.path, &["reset", "-q", "--hard", "HEAD~2"]);
        let state = unpublished(&remote.repo, &wt).await.unwrap();
        assert_eq!(state, Unpublished::Nothing, "behind origin adds nothing");
    }

    #[tokio::test]
    async fn a_branch_deleted_on_origin_is_republished_despite_a_stale_tracking_ref() {
        let remote = Remote::new("git-unpublished-stale");
        let wt = card_worktree(&remote).await;
        commit(&wt, "a").await;
        push(&wt.path, &wt.branch).await.unwrap();
        run(&remote.origin, &["branch", "-q", "-D", &wt.branch]);
        assert_eq!(
            run(&wt.path, &["rev-parse", "origin/helm/HELM-1"]),
            run(&wt.path, &["rev-parse", "HEAD"]),
            "the local tracking ref still says the branch is on origin"
        );

        let state = unpublished(&remote.repo, &wt).await.unwrap();
        assert_eq!(state, Unpublished::Commits(1));
    }

    #[tokio::test]
    async fn a_branch_whose_pushed_history_was_rewritten_or_extended_elsewhere_has_diverged() {
        let remote = Remote::new("git-unpublished-diverged");
        let wt = card_worktree(&remote).await;
        commit(&wt, "a").await;
        push(&wt.path, &wt.branch).await.unwrap();
        run(&wt.path, &["commit", "-q", "--amend", "-m", "amended"]);
        let state = unpublished(&remote.repo, &wt).await.unwrap();
        assert_eq!(state, Unpublished::Diverged);

        // Origin gains a commit this repository has never seen.
        let other = remote.dir.join("other");
        run(
            &remote.dir,
            &["clone", "-q", remote.origin.to_str().unwrap(), "other"],
        );
        run(&other, &["checkout", "-q", &wt.branch]);
        std::fs::write(other.join("elsewhere"), "x").unwrap();
        run(&other, &["add", "."]);
        run(&other, &["commit", "-q", "-m", "elsewhere"]);
        run(&other, &["push", "-q", "--force", "origin", &wt.branch]);
        let state = unpublished(&remote.repo, &wt).await.unwrap();
        assert_eq!(state, Unpublished::Diverged);
    }

    #[tokio::test]
    async fn an_unreachable_origin_is_an_error_not_an_empty_answer() {
        let remote = Remote::new("git-unpublished-down");
        let wt = card_worktree(&remote).await;
        commit(&wt, "a").await;
        run(
            &wt.path,
            &["remote", "set-url", "origin", "/nonexistent/origin.git"],
        );
        let err = unpublished(&remote.repo, &wt).await.unwrap_err();
        assert!(err.to_string().starts_with("git ls-remote failed"), "{err}");
    }

    #[tokio::test]
    async fn a_directory_that_is_not_the_cards_worktree_is_refused_not_overwritten() {
        let remote = Remote::new("git-refuse");
        std::fs::create_dir_all(remote.worktrees.join("HELM-1")).unwrap();
        let err = prepare_worktree(&remote.repo, &remote.worktrees, "HELM", 1)
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("is not the worktree of branch helm/HELM-1"),
            "{err}"
        );
        let err = prepare_worktree(&remote.repo, &remote.worktrees, "../x", 1)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot name a directory"), "{err}");
    }

    #[tokio::test]
    async fn a_worktree_from_another_repository_with_the_same_branch_is_refused() {
        let remote = Remote::new("git-refuse-foreign");
        let other = Remote::new("git-foreign");
        let path = remote.worktrees.join("HELM-1");
        run(
            &other.repo,
            &[
                "worktree",
                "add",
                "-b",
                "helm/HELM-1",
                path.to_str().unwrap(),
            ],
        );
        std::fs::write(path.join("keep.txt"), "untouched").unwrap();

        let err = prepare_worktree(&remote.repo, &remote.worktrees, "HELM", 1)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("is not the worktree"), "{err}");
        assert_eq!(
            std::fs::read_to_string(path.join("keep.txt")).unwrap(),
            "untouched"
        );
    }

    #[tokio::test]
    async fn a_subdirectory_of_the_matching_repository_and_branch_is_refused() {
        let remote = Remote::new("git-refuse-nested");
        run(&remote.repo, &["checkout", "-q", "-b", "helm/HELM-1"]);
        let path = remote.repo.join("HELM-1");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("keep.txt"), "untouched").unwrap();

        let err = prepare_worktree(&remote.repo, &remote.repo, "HELM", 1)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("is not the worktree"), "{err}");
        assert_eq!(
            std::fs::read_to_string(path.join("keep.txt")).unwrap(),
            "untouched"
        );
    }

    #[tokio::test]
    async fn pushing_publishes_the_branch_to_origin_and_a_missing_remote_is_an_error() {
        let remote = Remote::new("git-push");
        let wt = prepare_worktree(&remote.repo, &remote.worktrees, "HELM", 2)
            .await
            .unwrap();
        std::fs::write(wt.path.join("HELLO.md"), "hello\n").unwrap();
        run(&wt.path, &["add", "."]);
        run(&wt.path, &["commit", "-q", "-m", "Add HELLO"]);
        assert!(
            has_uncommitted_changes(&wt.path)
                .await
                .is_ok_and(|dirty| !dirty)
        );

        push(&wt.path, &wt.branch).await.unwrap();
        let tip = run(&remote.origin, &["log", "-1", "--format=%s", "helm/HELM-2"]);
        assert_eq!(tip, "Add HELLO");

        run(
            &wt.path,
            &["remote", "set-url", "origin", "/nonexistent/origin.git"],
        );
        let err = push(&wt.path, &wt.branch).await.unwrap_err();
        assert!(err.to_string().starts_with("git push failed"), "{err}");
    }

    #[tokio::test]
    async fn the_default_branch_follows_origin_head_then_main_then_master_then_head() {
        let remote = Remote::new("git-default");
        run(&remote.repo, &["branch", "-m", "main", "trunk"]);
        run(&remote.repo, &["push", "-q", "-u", "origin", "trunk"]);
        // No origin/HEAD, no main, no master: the checked-out branch.
        assert_eq!(default_branch(&remote.repo).await.unwrap(), "trunk");
        run(&remote.repo, &["remote", "set-head", "origin", "trunk"]);
        run(&remote.repo, &["checkout", "-q", "-b", "feature"]);
        assert_eq!(default_branch(&remote.repo).await.unwrap(), "trunk");
        run(&remote.repo, &["remote", "set-head", "origin", "--delete"]);
        run(&remote.repo, &["branch", "master", "trunk"]);
        assert_eq!(default_branch(&remote.repo).await.unwrap(), "master");
    }

    #[tokio::test]
    async fn only_a_git_repository_passes_the_startup_check() {
        let remote = Remote::new("git-check");
        check_repository(&remote.repo).await.unwrap();
        assert!(check_repository(&remote.dir).await.is_err());
    }
}
