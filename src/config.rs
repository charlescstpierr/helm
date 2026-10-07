//! Runtime configuration: defaults, then an optional TOML file, then environment variables.

use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;

use serde::Deserialize;

pub const DEFAULT_BIND: &str = "127.0.0.1:7878";
pub const DEFAULT_DB_PATH: &str = "helm.db";
pub const DEFAULT_CONFIG_FILE: &str = "helm.toml";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub bind: SocketAddr,
    pub db_path: PathBuf,
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

        Ok(Self { bind, db_path })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn defaults_listen_on_loopback() {
        let config = Config::resolve(None, no_env).unwrap();
        assert!(config.bind.ip().is_loopback());
        assert_eq!(config.bind.to_string(), DEFAULT_BIND);
        assert_eq!(config.db_path, PathBuf::from(DEFAULT_DB_PATH));
    }

    #[test]
    fn file_overrides_defaults_and_env_overrides_file() {
        let file = "bind = \"127.0.0.1:9000\"\ndb_path = \"/tmp/a.db\"\n";
        let config = Config::resolve(Some(file), no_env).unwrap();
        assert_eq!(config.bind.port(), 9000);
        assert_eq!(config.db_path, PathBuf::from("/tmp/a.db"));

        let env = |key: &str| match key {
            "HELM_BIND" => Some("127.0.0.1:9001".to_owned()),
            "HELM_DB" => Some("/tmp/b.db".to_owned()),
            _ => None,
        };
        let config = Config::resolve(Some(file), env).unwrap();
        assert_eq!(config.bind.port(), 9001);
        assert_eq!(config.db_path, PathBuf::from("/tmp/b.db"));
    }

    #[test]
    fn rejects_invalid_values() {
        assert!(Config::resolve(Some("bind = \"nowhere\""), no_env).is_err());
        assert!(Config::resolve(Some("unknown_key = 1"), no_env).is_err());
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
            Config::resolve(Some(example), no_env).unwrap(),
            Config::resolve(None, no_env).unwrap()
        );
    }
}
