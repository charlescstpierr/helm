//! The git operations a run needs: a worktree per card, a commit count, a push. Each shells
//! out to `git`, so the user's own configuration (identity, credentials, hooks) applies.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

const PUSH_TIMEOUT: Duration = Duration::from_secs(120);

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

/// Commits on the worktree's branch that the default branch does not have.
pub async fn commits_ahead(repo: &Path, worktree: &Path) -> Result<u64> {
    let base = default_branch(repo).await?;
    let count = git(worktree, &["rev-list", "--count", &format!("{base}..HEAD")]).await?;
    count
        .parse()
        .map_err(|_| GitError(format!("unexpected commit count {count:?}")))
}

pub async fn has_uncommitted_changes(worktree: &Path) -> Result<bool> {
    Ok(!git(worktree, &["status", "--porcelain"]).await?.is_empty())
}

/// Pushes the branch to `origin`. Never forced: a rejected push is a failed run.
pub async fn push(worktree: &Path, branch: &str) -> Result<()> {
    let args = ["push", "--set-upstream", "origin", branch];
    match tokio::time::timeout(PUSH_TIMEOUT, git(worktree, &args)).await {
        Ok(result) => result.map(drop),
        Err(_) => error(format!(
            "git push did not finish within {} seconds",
            PUSH_TIMEOUT.as_secs()
        )),
    }
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
        assert_eq!(commits_ahead(&remote.repo, &wt.path).await.unwrap(), 0);
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
        assert_eq!(commits_ahead(&remote.repo, &again.path).await.unwrap(), 1);

        // The directory is deleted behind git's back; the branch and its commit survive.
        std::fs::remove_dir_all(&first.path).unwrap();
        let revived = prepare_worktree(&remote.repo, &remote.worktrees, "HELM", 1)
            .await
            .unwrap();
        assert!(revived.path.join("work.txt").exists());
        assert_eq!(commits_ahead(&remote.repo, &revived.path).await.unwrap(), 1);
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
