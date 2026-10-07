//! Runtime configuration: defaults, then an optional TOML file, then environment variables.

use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::agent::{ModelName, PermissionMode};

pub const DEFAULT_BIND: &str = "127.0.0.1:7878";
pub const DEFAULT_DB_PATH: &str = "helm.db";
pub const DEFAULT_CONFIG_FILE: &str = "helm.toml";
pub const DEFAULT_CLAUDE_COMMAND: &str = "claude";
pub const DEFAULT_MAX_CONCURRENT_RUNS: usize = 2;
const DEFAULT_WORKTREE_SUBPATH: &str = ".local/share/helm/worktrees";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub bind: SocketAddr,
    pub db_path: PathBuf,
    /// `None` leaves the board as a plain kanban: agents cannot be assigned.
    pub project: Option<ProjectConfig>,
    pub agents: AgentsConfig,
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
    pub claude: ClaudeConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeConfig {
    pub command: PathBuf,
    pub permission_mode: PermissionMode,
    /// Used when a card names no model; `None` leaves the choice to the CLI.
    pub model: Option<ModelName>,
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
        let claude = file.agents.claude;
        let command = claude
            .command
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CLAUDE_COMMAND));
        if command.as_os_str().is_empty() {
            return Err(ConfigError(
                "agents.claude.command must not be empty".to_owned(),
            ));
        }

        Ok(Self {
            bind,
            db_path,
            project,
            agents: AgentsConfig {
                max_concurrent,
                claude: ClaudeConfig {
                    command,
                    permission_mode: claude.permission_mode.unwrap_or(PermissionMode::DEFAULT),
                    model: claude.model,
                },
            },
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
            "[agents]\nunknown = 1",
            "[agents.claude]\npermission_mode = \"yolo\"",
            "[agents.claude]\nmodel = \"--oops\"",
            "[agents.claude]\ncommand = \"\"",
            "[project]\nrepo = \"\"",
            "[project]\nbranch = \"main\"",
            "[project]\nworktree_root = \"/srv/wt\"",
        ] {
            assert!(Config::resolve(Some(bad), home_env).is_err(), "{bad}");
        }
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
