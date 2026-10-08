//! The vocabulary shared by cards, runs and configuration: which agent, which model, how much
//! autonomy. Everything here is parsed once at a boundary (a form, `helm.toml`, a database
//! row); the rest of the code passes the typed values around.

use std::fmt;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use serde::Deserialize;

/// The coding agents Helm can drive. Adding one means a variant here and an adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Claude,
}

impl Agent {
    pub const ALL: [Self; 1] = [Self::Claude];

    pub const fn slug(self) -> &'static str {
        match self {
            Self::Claude => "claude",
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|agent| agent.slug() == text)
    }
}

impl ToSql for Agent {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.slug().to_sql()
    }
}

impl FromSql for Agent {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        Self::parse(text)
            .ok_or_else(|| FromSqlError::Other(format!("unknown agent {text:?}").into()))
    }
}

pub const MAX_MODEL_CHARS: usize = 100;

/// A model name or alias passed to an agent CLI as `--model <name>`.
///
/// The alphabet is deliberately narrow and the first character must be alphanumeric, so a
/// name can never be read as another flag.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct ModelName(String);

#[derive(Debug, PartialEq, Eq)]
pub struct InvalidModelName;

impl fmt::Display for InvalidModelName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Nom de modèle invalide : lettres, chiffres et . _ - : / [ ] seulement, {MAX_MODEL_CHARS} caractères au plus."
        )
    }
}

impl std::error::Error for InvalidModelName {}

impl ModelName {
    /// `Ok(None)` for blank input, which means "use the project default".
    pub fn parse_optional(text: &str) -> Result<Option<Self>, InvalidModelName> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(None);
        }
        let allowed = |c: char| c.is_ascii_alphanumeric() || "._-:/[]".contains(c);
        let starts_well = text
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric());
        if text.chars().count() > MAX_MODEL_CHARS || !starts_well || !text.chars().all(allowed) {
            return Err(InvalidModelName);
        }
        Ok(Some(Self(text.to_owned())))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ModelName {
    type Error = String;

    fn try_from(text: String) -> Result<Self, String> {
        match Self::parse_optional(&text) {
            Ok(Some(name)) => Ok(name),
            Ok(None) => Err("model must not be empty".to_owned()),
            Err(e) => Err(e.to_string()),
        }
    }
}

impl fmt::Display for ModelName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl ToSql for ModelName {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

impl FromSql for ModelName {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        match Self::parse_optional(text) {
            Ok(Some(name)) => Ok(name),
            _ => Err(FromSqlError::Other(
                format!("invalid model name {text:?}").into(),
            )),
        }
    }
}

/// The `claude --permission-mode` values. The default is full autonomy: a run is an
/// unattended process and nobody is there to answer a prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    AcceptEdits,
    Auto,
    BypassPermissions,
    Manual,
    DontAsk,
    Plan,
}

impl PermissionMode {
    pub const DEFAULT: Self = Self::BypassPermissions;

    /// The value the CLI expects, also what the database stores.
    pub const fn cli_value(self) -> &'static str {
        match self {
            Self::AcceptEdits => "acceptEdits",
            Self::Auto => "auto",
            Self::BypassPermissions => "bypassPermissions",
            Self::Manual => "manual",
            Self::DontAsk => "dontAsk",
            Self::Plan => "plan",
        }
    }
}

impl ToSql for PermissionMode {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.cli_value().to_sql()
    }
}

impl FromSql for PermissionMode {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        [
            Self::AcceptEdits,
            Self::Auto,
            Self::BypassPermissions,
            Self::Manual,
            Self::DontAsk,
            Self::Plan,
        ]
        .into_iter()
        .find(|mode| mode.cli_value() == text)
        .ok_or_else(|| FromSqlError::Other(format!("unknown permission mode {text:?}").into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_round_trip_through_their_slug() {
        for agent in Agent::ALL {
            assert_eq!(Agent::parse(agent.slug()), Some(agent));
        }
        assert_eq!(Agent::parse("codex"), None);
        assert_eq!(Agent::parse(""), None);
    }

    #[test]
    fn model_names_cannot_smuggle_a_flag_or_whitespace() {
        let ok = [
            "sonnet",
            "claude-sonnet-4-5",
            "claude-opus-4-1-20250805",
            "opus[1m]",
        ];
        for name in ok {
            assert_eq!(
                ModelName::parse_optional(name).unwrap().unwrap().as_str(),
                name
            );
        }
        assert_eq!(ModelName::parse_optional("  \t"), Ok(None));
        assert_eq!(
            ModelName::parse_optional(" haiku ")
                .unwrap()
                .unwrap()
                .as_str(),
            "haiku"
        );
        for bad in [
            "--dangerously-skip-permissions",
            "-m",
            "a b",
            "a;b",
            "é",
            "a\nb",
        ] {
            assert_eq!(
                ModelName::parse_optional(bad),
                Err(InvalidModelName),
                "{bad}"
            );
        }
        assert!(ModelName::parse_optional(&"a".repeat(MAX_MODEL_CHARS + 1)).is_err());
    }

    #[test]
    fn permission_modes_match_the_cli_spelling_in_config_and_storage() {
        #[derive(Deserialize)]
        struct Holder {
            mode: PermissionMode,
        }
        for (text, mode) in [
            ("bypassPermissions", PermissionMode::BypassPermissions),
            ("acceptEdits", PermissionMode::AcceptEdits),
            ("dontAsk", PermissionMode::DontAsk),
            ("plan", PermissionMode::Plan),
        ] {
            let parsed: Holder = toml::from_str(&format!("mode = \"{text}\"")).unwrap();
            assert_eq!(parsed.mode, mode);
            assert_eq!(mode.cli_value(), text);
        }
        assert!(toml::from_str::<Holder>("mode = \"yolo\"").is_err());
        assert_eq!(PermissionMode::DEFAULT, PermissionMode::BypassPermissions);
    }
}
