//! Runtime configuration: defaults, then an optional TOML file, then environment variables.

use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::agent::{ModelName, PermissionMode};

pub const DEFAULT_BIND: &str = "127.0.0.1:7878";
pub const DEFAULT_DB_PATH: &str = "helm.db";
pub const DEFAULT_CONFIG_FILE: &str = "helm.toml";
pub const DEFAULT_CLAUDE_COMMAND: &str = "claude";
pub const DEFAULT_MAX_CONCURRENT_RUNS: usize = 2;
pub const DEFAULT_RUN_TIMEOUT_MINUTES: u64 = 60;
pub const DEFAULT_CHECK_TIMEOUT_MINUTES: u64 = 10;
pub const DEFAULT_GITHUB_COMMAND: &str = "gh";
const DEFAULT_WORKTREE_SUBPATH: &str = ".local/share/helm/worktrees";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub bind: SocketAddr,
    pub db_path: PathBuf,
    /// `None` leaves the board as a plain kanban: agents cannot be assigned.
    pub project: Option<ProjectConfig>,
    pub agents: AgentsConfig,
    pub checks: ChecksConfig,
    pub github: Option<GithubConfig>,
}

/// The git repository the board's cards are worked on in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectConfig {
    pub repo: PathBuf,
    /// Each card gets `<worktree_root>/<KEY>-<number>`.
    pub worktree_root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentsConfig {
    pub max_concurrent: usize,
    pub run_timeout: Duration,
    pub claude: ClaudeConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeConfig {
    pub command: PathBuf,
    pub permission_mode: PermissionMode,
    /// Used when a card names no model; `None` leaves the choice to the CLI.
    pub model: Option<ModelName>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecksConfig {
    pub commands: Vec<String>,
    pub timeout: Duration,
}

impl Default for ChecksConfig {
    fn default() -> Self {
        Self {
            commands: Vec::new(),
            timeout: Duration::from_secs(DEFAULT_CHECK_TIMEOUT_MINUTES * 60),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubConfig {
    pub repository: String,
    pub command: PathBuf,
}

/// Whether Helm may start agents, decided once from the configuration.
///
/// A run is arbitrary code execution with the permissions of the user, and Helm has no
/// authentication. Anything that can reach the port could start one, so runs need loopback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunGate {
    Open,
    NoRepo,
    NotLoopback,
}

impl RunGate {
    /// The explanation shown in the UI and at startup, `None` when runs may start.
    pub const fn notice(self) -> Option<&'static str> {
        match self {
            Self::Open => None,
            Self::NoRepo => Some(
                "Aucun dépôt n'est configuré (project.repo dans helm.toml) : l'assignation d'un agent est indisponible.",
            ),
            Self::NotLoopback => Some(
                "Helm n'écoute pas sur une adresse loopback et n'a pas d'authentification : aucune exécution d'agent ne sera lancée.",
            ),
        }
    }
}

#[derive(Debug)]
pub struct ConfigError(String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "configuration: {}", self.0)
    }
}

impl std::error::Error for ConfigError {}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    bind: Option<String>,
    db_path: Option<PathBuf>,
    #[serde(default)]
    project: FileProject,
    #[serde(default)]
    agents: FileAgents,
    #[serde(default)]
    checks: FileChecks,
    github: Option<FileGithub>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileProject {
    repo: Option<PathBuf>,
    worktree_root: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileAgents {
    max_concurrent: Option<usize>,
    run_timeout_minutes: Option<u64>,
    #[serde(default)]
    claude: FileClaude,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileClaude {
    command: Option<PathBuf>,
    permission_mode: Option<PermissionMode>,
    model: Option<ModelName>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileChecks {
    #[serde(default)]
    commands: Vec<String>,
    timeout_minutes: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileGithub {
    repository: String,
    command: Option<PathBuf>,
}

fn read_optional(path: &str) -> Result<Option<String>, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ConfigError(format!("cannot read {path}: {e}"))),
    }
}

impl Config {
    /// Loads the configuration from the process environment and the config file, if any.
    ///
    /// `HELM_CONFIG` names the file explicitly (it must exist); otherwise `helm.toml` in the
    /// working directory is used when present.
    pub fn load() -> Result<Self, ConfigError> {
        let env = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
        let file = match env("HELM_CONFIG") {
            Some(path) => Some(
                std::fs::read_to_string(&path)
                    .map_err(|e| ConfigError(format!("cannot read {path}: {e}")))?,
            ),
            None => read_optional(DEFAULT_CONFIG_FILE)?,
        };
        Self::resolve(file.as_deref(), env)
    }

    /// Pure merge of file contents and environment lookups, so it can be tested.
    pub fn resolve(
        file: Option<&str>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        let file: FileConfig = match file {
            Some(text) => {
                toml::from_str(text).map_err(|e| ConfigError(format!("invalid TOML: {e}")))?
            }
            None => FileConfig::default(),
        };

        let bind = env("HELM_BIND")
            .or(file.bind)
            .unwrap_or_else(|| DEFAULT_BIND.to_owned());
        let bind = bind
            .parse::<SocketAddr>()
            .map_err(|e| ConfigError(format!("bind address `{bind}`: {e}")))?;

        let db_path = env("HELM_DB")
            .map(PathBuf::from)
            .or(file.db_path)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_DB_PATH));

        // SQLite treats an empty filename as a throwaway temporary database.
        if db_path.as_os_str().is_empty() {
            return Err(ConfigError("db_path must not be empty".to_owned()));
        }

        let home = env("HOME").map(PathBuf::from);
        let project = match (file.project.repo, file.project.worktree_root) {
            (None, None) => None,
            (None, Some(_)) => {
                return Err(ConfigError(
                    "project.worktree_root needs project.repo".to_owned(),
                ));
            }
            (Some(repo), worktree_root) => {
                let repo = expand_home(repo, home.as_deref())?;
                let worktree_root = match worktree_root {
                    Some(path) => expand_home(path, home.as_deref())?,
                    None => home
                        .map(|home| home.join(DEFAULT_WORKTREE_SUBPATH))
                        .ok_or_else(|| {
                            ConfigError(
                                "project.worktree_root is unset and HOME is not defined".to_owned(),
                            )
                        })?,
                };
                if repo.as_os_str().is_empty() || worktree_root.as_os_str().is_empty() {
                    return Err(ConfigError(
                        "project.repo and project.worktree_root must not be empty".to_owned(),
                    ));
                }
                require_absolute("project.repo", &repo)?;
                require_absolute("project.worktree_root", &worktree_root)?;
                Some(ProjectConfig {
                    repo,
                    worktree_root,
                })
            }
        };

        let max_concurrent = file
            .agents
            .max_concurrent
            .unwrap_or(DEFAULT_MAX_CONCURRENT_RUNS);
        if max_concurrent == 0 {
            return Err(ConfigError(
                "agents.max_concurrent must be at least 1".to_owned(),
            ));
        }
        let timeout_minutes = file
            .agents
            .run_timeout_minutes
            .unwrap_or(DEFAULT_RUN_TIMEOUT_MINUTES);
        let run_timeout = minutes_to_duration("agents.run_timeout_minutes", timeout_minutes)?;
        let claude = file.agents.claude;
        let command = claude
            .command
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CLAUDE_COMMAND));
        if command.as_os_str().is_empty() {
            return Err(ConfigError(
                "agents.claude.command must not be empty".to_owned(),
            ));
        }

        for (position, command) in file.checks.commands.iter().enumerate() {
            if command.trim().is_empty() {
                return Err(ConfigError(format!(
                    "checks.commands[{position}] must not be empty"
                )));
            }
        }
        let checks = ChecksConfig {
            commands: file.checks.commands,
            timeout: minutes_to_duration(
                "checks.timeout_minutes",
                file.checks
                    .timeout_minutes
                    .unwrap_or(DEFAULT_CHECK_TIMEOUT_MINUTES),
            )?,
        };
        let github = file
            .github
            .map(|github| {
                if !valid_github_repository(&github.repository) {
                    return Err(ConfigError(
                        "github.repository must name a GitHub repository as owner/repo".to_owned(),
                    ));
                }
                let command = github
                    .command
                    .unwrap_or_else(|| PathBuf::from(DEFAULT_GITHUB_COMMAND));
                if command.as_os_str().is_empty() {
                    return Err(ConfigError("github.command must not be empty".to_owned()));
                }
                Ok(GithubConfig {
                    repository: github.repository,
                    command,
                })
            })
            .transpose()?;

        Ok(Self {
            bind,
            db_path,
            project,
            agents: AgentsConfig {
                max_concurrent,
                run_timeout,
                claude: ClaudeConfig {
                    command,
                    permission_mode: claude.permission_mode.unwrap_or(PermissionMode::DEFAULT),
                    model: claude.model,
                },
            },
            checks,
            github,
        })
    }

    pub fn run_gate(&self) -> RunGate {
        if !self.bind.ip().is_loopback() {
            RunGate::NotLoopback
        } else if self.project.is_none() {
            RunGate::NoRepo
        } else {
            RunGate::Open
        }
    }
}

fn minutes_to_duration(key: &str, minutes: u64) -> Result<Duration, ConfigError> {
    minutes
        .checked_mul(60)
        .filter(|seconds| *seconds > 0 && *seconds <= u64::from(u32::MAX))
        .map(Duration::from_secs)
        .ok_or_else(|| {
            ConfigError(format!(
                "{key} must be between 1 and {} (got {minutes})",
                u32::MAX / 60
            ))
        })
}

fn valid_github_repository(repository: &str) -> bool {
    let Some((owner, name)) = repository.split_once('/') else {
        return false;
    };
    !owner.is_empty()
        && owner.len() <= 39
        && owner.starts_with(|c: char| c.is_ascii_alphanumeric())
        && owner.ends_with(|c: char| c.is_ascii_alphanumeric())
        && owner.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !name.is_empty()
        && name.len() <= 100
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

fn require_absolute(key: &str, path: &Path) -> Result<(), ConfigError> {
    if path.is_absolute() {
        return Ok(());
    }
    Err(ConfigError(format!(
        "{key} must be an absolute path (got `{}`); write the full path or start it with `~/`",
        path.display()
    )))
}

/// Expands a leading `~` or `~/`; the shell does not do it for a path read from a file.
fn expand_home(path: PathBuf, home: Option<&Path>) -> Result<PathBuf, ConfigError> {
    let Ok(rest) = path.strip_prefix("~") else {
        return Ok(path);
    };
    home.map(|home| home.join(rest)).ok_or_else(|| {
        ConfigError(format!(
            "cannot expand `{}`: HOME is not defined",
            path.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn home_env(key: &str) -> Option<String> {
        (key == "HOME").then(|| "/home/dev".to_owned())
    }

    #[test]
    fn defaults_listen_on_loopback() {
        let config = Config::resolve(None, home_env).unwrap();
        assert!(config.bind.ip().is_loopback());
        assert_eq!(config.bind.to_string(), DEFAULT_BIND);
        assert_eq!(config.db_path, PathBuf::from(DEFAULT_DB_PATH));
    }

    #[test]
    fn file_overrides_defaults_and_env_overrides_file() {
        let file = "bind = \"127.0.0.1:9000\"\ndb_path = \"/tmp/a.db\"\n";
        let config = Config::resolve(Some(file), home_env).unwrap();
        assert_eq!(config.bind.port(), 9000);
        assert_eq!(config.db_path, PathBuf::from("/tmp/a.db"));

        let env = |key: &str| match key {
            "HELM_BIND" => Some("127.0.0.1:9001".to_owned()),
            "HELM_DB" => Some("/tmp/b.db".to_owned()),
            other => home_env(other),
        };
        let config = Config::resolve(Some(file), env).unwrap();
        assert_eq!(config.bind.port(), 9001);
        assert_eq!(config.db_path, PathBuf::from("/tmp/b.db"));
    }

    #[test]
    fn rejects_invalid_values() {
        assert!(Config::resolve(Some("bind = \"nowhere\""), home_env).is_err());
        assert!(Config::resolve(Some("unknown_key = 1"), home_env).is_err());
        assert!(Config::resolve(Some("db_path = \"\""), home_env).is_err());
    }

    #[test]
    fn agents_default_to_full_autonomy_and_the_board_needs_no_repo_or_home() {
        let config = Config::resolve(None, no_env).unwrap();
        assert_eq!(config.project, None);
        assert_eq!(config.agents.max_concurrent, DEFAULT_MAX_CONCURRENT_RUNS);
        assert_eq!(config.agents.run_timeout, Duration::from_secs(60 * 60));
        assert_eq!(config.agents.claude.command, PathBuf::from("claude"));
        assert_eq!(
            config.agents.claude.permission_mode,
            PermissionMode::BypassPermissions
        );
        assert_eq!(config.agents.claude.model, None);
    }

    #[test]
    fn worktrees_default_under_the_data_directory_of_the_home() {
        let file = "[project]\nrepo = \"/r\"\n";
        let project = Config::resolve(Some(file), home_env)
            .unwrap()
            .project
            .unwrap();
        assert_eq!(
            project.worktree_root,
            PathBuf::from("/home/dev/.local/share/helm/worktrees")
        );
        assert!(Config::resolve(Some(file), no_env).is_err());
        let explicit = "[project]\nrepo = \"/r\"\nworktree_root = \"/srv/wt\"\n";
        assert!(Config::resolve(Some(explicit), no_env).is_ok());
    }

    #[test]
    fn project_and_agent_settings_are_read_and_the_home_prefix_is_expanded() {
        let file = r#"
            [project]
            repo = "~/code/app"
            worktree_root = "/srv/wt"

            [agents]
            max_concurrent = 4
            run_timeout_minutes = 90

            [agents.claude]
            command = "/opt/claude"
            permission_mode = "acceptEdits"
            model = "sonnet"
        "#;
        let config = Config::resolve(Some(file), home_env).unwrap();
        let project = config.project.unwrap();
        assert_eq!(project.repo, PathBuf::from("/home/dev/code/app"));
        assert_eq!(project.worktree_root, PathBuf::from("/srv/wt"));
        assert_eq!(config.agents.max_concurrent, 4);
        assert_eq!(config.agents.run_timeout, Duration::from_secs(90 * 60));
        assert_eq!(config.agents.claude.command, PathBuf::from("/opt/claude"));
        assert_eq!(
            config.agents.claude.permission_mode,
            PermissionMode::AcceptEdits
        );
        assert_eq!(config.agents.claude.model.unwrap().as_str(), "sonnet");
    }

    #[test]
    fn invalid_agent_settings_fail_the_startup() {
        for bad in [
            "[agents]\nmax_concurrent = 0",
            "[agents]\nrun_timeout_minutes = 0",
            "[agents]\nrun_timeout_minutes = 18446744073709551615",
            "[agents]\nunknown = 1",
            "[agents.claude]\npermission_mode = \"yolo\"",
            "[agents.claude]\nmodel = \"--oops\"",
            "[agents.claude]\ncommand = \"\"",
            "[project]\nrepo = \"\"",
            "[project]\nrepo = \"code/app\"",
            "[project]\nrepo = \"./app\"",
            "[project]\nrepo = \"/r\"\nworktree_root = \"worktrees\"",
            "[project]\nrepo = \"/r\"\nworktree_root = \"../wt\"",
            "[project]\nbranch = \"main\"",
            "[project]\nworktree_root = \"/srv/wt\"",
        ] {
            assert!(Config::resolve(Some(bad), home_env).is_err(), "{bad}");
        }
    }

    #[test]
    fn accepts_checks_with_defaults_or_custom_settings() {
        for (file, commands, timeout_minutes) in [
            ("[checks]", vec![], 10),
            (
                "[checks]\ncommands = [\"cargo fmt --check\", \"cargo test\"]\ntimeout_minutes = 5",
                vec!["cargo fmt --check", "cargo test"],
                5,
            ),
            ("[checks]\ntimeout_minutes = 71582788", vec![], 71582788),
        ] {
            let config = Config::resolve(Some(file), no_env).unwrap();
            assert_eq!(config.checks.commands, commands);
            assert_eq!(
                config.checks.timeout,
                Duration::from_secs(timeout_minutes * 60)
            );
        }
    }

    #[test]
    fn checks_default_to_no_commands_and_github_is_disabled() {
        let config = Config::resolve(None, no_env).unwrap();
        assert_eq!(config.checks, ChecksConfig::default());
        assert!(config.checks.commands.is_empty());
        assert_eq!(config.checks.timeout, Duration::from_secs(10 * 60));
        assert_eq!(config.github, None);
    }

    #[test]
    fn accepts_github_with_default_or_custom_command() {
        for (file, repository, command) in [
            ("[github]\nrepository = \"owner/repo\"", "owner/repo", "gh"),
            (
                "[github]\nrepository = \"my-org/my.repo_2\"\ncommand = \"/opt/bin/gh\"",
                "my-org/my.repo_2",
                "/opt/bin/gh",
            ),
            (
                "[github]\nrepository = \"owner/.github\"",
                "owner/.github",
                "gh",
            ),
        ] {
            let github = Config::resolve(Some(file), no_env).unwrap().github.unwrap();
            assert_eq!(github.repository, repository);
            assert_eq!(github.command, PathBuf::from(command));
        }
    }

    #[test]
    fn rejects_unknown_or_wrongly_typed_checks_and_github_settings() {
        for file in [
            "[checks]\ncommand = \"cargo test\"",
            "[checks]\ncommands = \"cargo test\"",
            "[checks]\ncommands = [1]",
            "[checks]\ntimeout_minutes = -1",
            "[checks]\ntimeout_minutes = 0.5",
            "[github]\nrepository = \"owner/repo\"\nunknown = true",
            "[github]\nrepository = 1",
            "[github]\nrepository = \"owner/repo\"\ncommand = 1",
        ] {
            assert!(Config::resolve(Some(file), no_env).is_err(), "{file}");
        }
    }

    #[test]
    fn rejects_invalid_checks_settings_with_the_setting_name() {
        for value in ["0", "71582789", "18446744073709551615"] {
            let file = format!("[checks]\ntimeout_minutes = {value}");
            let error = Config::resolve(Some(&file), no_env)
                .unwrap_err()
                .to_string();
            assert!(error.contains("checks.timeout_minutes"), "{error}");
        }
    }

    #[test]
    fn rejects_blank_check_commands_with_their_position() {
        for (file, position) in [
            (
                r#"[checks]
commands = [""]"#,
                0,
            ),
            (
                r#"[checks]
commands = [" \n"]"#,
                0,
            ),
            (
                r#"[checks]
commands = ["cargo test", ""]"#,
                1,
            ),
        ] {
            let error = Config::resolve(Some(file), no_env).unwrap_err().to_string();
            assert!(
                error.contains(&format!("checks.commands[{position}]")),
                "{error}"
            );
        }
    }

    #[test]
    fn preserves_nonempty_check_commands_verbatim() {
        let file = r#"[checks]
commands = ["  cargo test \n", "printf 'hello world'"]"#;
        let checks = Config::resolve(Some(file), no_env).unwrap().checks;
        assert_eq!(checks.commands, ["  cargo test \n", "printf 'hello world'"]);
    }

    #[test]
    fn rejects_invalid_github_repository_with_the_setting_name() {
        for repository in [
            "",
            "repo",
            "owner/",
            "/repo",
            "owner/repo/extra",
            "owner /repo",
            "owner/re po",
            "https://github.com/owner/repo",
            "-owner/repo",
            "owner/..",
            "owner/repo?arg=1",
        ] {
            let file = format!("[github]\nrepository = {repository:?}");
            let error = Config::resolve(Some(&file), no_env)
                .unwrap_err()
                .to_string();
            assert!(error.contains("github.repository"), "{repository}: {error}");
        }
    }

    #[test]
    fn rejects_github_without_a_repository_or_with_an_empty_command() {
        for (file, key) in [
            ("[github]", "repository"),
            (
                "[github]\nrepository = \"owner/repo\"\ncommand = \"\"",
                "github.command",
            ),
        ] {
            let error = Config::resolve(Some(file), no_env).unwrap_err().to_string();
            assert!(error.contains(key), "{error}");
        }
    }

    #[test]
    fn a_relative_repo_or_worktree_root_names_the_key_and_the_value() {
        let relative_root = "[project]\nrepo = \"/r\"\nworktree_root = \"wt/cards\"";
        let message = Config::resolve(Some(relative_root), home_env)
            .unwrap_err()
            .to_string();
        assert!(
            message.contains("project.worktree_root")
                && message.contains("wt/cards")
                && message.contains("absolute"),
            "{message}"
        );
        let message = Config::resolve(Some("[project]\nrepo = \"code/app\""), home_env)
            .unwrap_err()
            .to_string();
        assert!(
            message.contains("project.repo") && message.contains("code/app"),
            "{message}"
        );
        let relative_home = |key: &str| (key == "HOME").then(|| "home/dev".to_owned());
        assert!(Config::resolve(Some("[project]\nrepo = \"/r\""), relative_home).is_err());
    }

    #[test]
    fn runs_are_gated_on_a_loopback_bind_and_a_configured_repo() {
        let repo = "[project]\nrepo = \"/r\"\n";
        let gate = |file: &str, bind: &str| {
            let env = |key: &str| match key {
                "HOME" => Some("/home/dev".to_owned()),
                "HELM_BIND" => Some(bind.to_owned()),
                _ => None,
            };
            Config::resolve(Some(file), env).unwrap().run_gate()
        };
        assert_eq!(gate(repo, "127.0.0.1:7878"), RunGate::Open);
        assert_eq!(gate(repo, "[::1]:7878"), RunGate::Open);
        assert_eq!(gate("", "127.0.0.1:7878"), RunGate::NoRepo);
        assert_eq!(gate(repo, "0.0.0.0:7878"), RunGate::NotLoopback);
        assert_eq!(gate(repo, "192.168.1.5:7878"), RunGate::NotLoopback);
        // A missing repo on a public bind reports the security problem first.
        assert_eq!(gate("", "0.0.0.0:7878"), RunGate::NotLoopback);
        assert!(RunGate::Open.notice().is_none());
        assert!(RunGate::NotLoopback.notice().unwrap().contains("loopback"));
    }

    #[test]
    fn missing_config_file_is_skipped_but_unreadable_one_is_an_error() {
        let dir = std::env::temp_dir();
        let missing = dir.join("helm-config-that-does-not-exist.toml");
        assert_eq!(read_optional(missing.to_str().unwrap()).unwrap(), None);
        let unreadable = read_optional(dir.to_str().unwrap());
        assert!(unreadable.unwrap_err().to_string().contains("cannot read"));
    }

    #[test]
    fn example_file_is_valid() {
        let example = include_str!("../helm.example.toml");
        assert_eq!(
            Config::resolve(Some(example), home_env).unwrap(),
            Config::resolve(None, home_env).unwrap()
        );
    }
}
