//! What differs between agent CLIs: how to start one, and how to read its output.
//!
//! An adapter is pure. It builds a command line and turns one line of the CLI's stdout into a
//! normalised event; running the process, storing the events and deciding what a run's end
//! means belong to the supervisor. Adding an agent means one new adapter.

use std::path::PathBuf;

use serde_json::Value;

use crate::agent::{Agent, ModelName, PermissionMode};
use crate::runs::{EventKind, NewEvent};

/// What a run is launched with.
#[derive(Debug, Clone, Copy)]
pub struct Launch<'a> {
    pub prompt: &'a str,
    pub model: Option<&'a ModelName>,
    pub permission_mode: PermissionMode,
}

/// A process to start. The prompt travels on stdin: it holds the whole comment thread and
/// can outgrow what an argument may carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub stdin: String,
}

/// The CLI's own judgement of how the run went, from its final line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Success,
    Failure(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Usage {
    pub cost_usd: Option<f64>,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
}

/// Everything one stdout line says.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedLine {
    pub event: NewEvent,
    /// The CLI's session id, wherever it shows up. The first one seen is kept.
    pub session_id: Option<String>,
    /// Present on the final line only.
    pub finish: Option<Finish>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Finish {
    pub verdict: Verdict,
    pub usage: Usage,
}

pub trait AgentAdapter: Send + Sync {
    fn agent(&self) -> Agent;
    fn command(&self, launch: &Launch<'_>) -> CommandSpec;
    /// `None` for a blank line; anything else yields an event, even when it cannot be parsed.
    fn parse_line(&self, line: &str) -> Option<ParsedLine>;
}

const SUMMARY_CHARS: usize = 200;

/// First non-empty line, whitespace collapsed, cut to a readable length.
fn preview(text: &str) -> String {
    let first = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let collapsed = first.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    let head: String = chars.by_ref().take(SUMMARY_CHARS).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// `claude -p … --output-format stream-json`.
pub struct ClaudeAdapter {
    pub command: PathBuf,
}

impl AgentAdapter for ClaudeAdapter {
    fn agent(&self) -> Agent {
        Agent::Claude
    }

    fn command(&self, launch: &Launch<'_>) -> CommandSpec {
        // `--verbose` is required for stream-json in print mode. With no prompt argument,
        // `-p` reads the prompt from stdin.
        let mut args = ["-p", "--output-format", "stream-json", "--verbose"]
            .map(str::to_owned)
            .to_vec();
        args.push("--permission-mode".to_owned());
        args.push(launch.permission_mode.cli_value().to_owned());
        if let Some(model) = launch.model {
            args.push("--model".to_owned());
            args.push(model.as_str().to_owned());
        }
        CommandSpec {
            program: self.command.clone(),
            args,
            stdin: launch.prompt.to_owned(),
        }
    }

    fn parse_line(&self, line: &str) -> Option<ParsedLine> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return Some(malformed(line, "pas du JSON"));
        };
        let Some(kind) = value.get("type").and_then(Value::as_str) else {
            return Some(malformed(line, "JSON sans champ type"));
        };
        let (event_kind, summary, finish) = match kind {
            "system" => system_event(&value),
            "assistant" => assistant_event(&value),
            "user" => user_event(&value),
            "result" => result_event(&value),
            other => (EventKind::System, other.to_owned(), None),
        };
        Some(ParsedLine {
            event: NewEvent {
                kind: event_kind,
                summary,
                payload: line.to_owned(),
            },
            session_id: value
                .get("session_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned),
            finish,
        })
    }
}

fn malformed(line: &str, why: &str) -> ParsedLine {
    ParsedLine {
        event: NewEvent {
            kind: EventKind::Malformed,
            summary: format!("Ligne illisible ({why}) : {}", preview(line)),
            payload: line.to_owned(),
        },
        session_id: None,
        finish: None,
    }
}

type Classified = (EventKind, String, Option<Finish>);

fn system_event(value: &Value) -> Classified {
    let subtype = value.get("subtype").and_then(Value::as_str).unwrap_or("");
    if subtype == "init" {
        let model = value.get("model").and_then(Value::as_str).unwrap_or("?");
        return (EventKind::Init, format!("Session démarrée ({model})"), None);
    }
    (EventKind::System, format!("system:{subtype}"), None)
}

fn content_blocks(value: &Value) -> &[Value] {
    value
        .pointer("/message/content")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn assistant_event(value: &Value) -> Classified {
    let mut kind = EventKind::Message;
    let mut parts = Vec::new();
    for block in content_blocks(value) {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => parts.push(preview(
                block.get("text").and_then(Value::as_str).unwrap_or(""),
            )),
            Some("thinking") => parts.push("(réflexion)".to_owned()),
            Some("tool_use") => {
                kind = EventKind::ToolUse;
                parts.push(tool_use_summary(block));
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        parts.push("(message vide)".to_owned());
    }
    (kind, parts.join(" · "), None)
}

/// `Bash: ls -la`, `Write: /work/repo/HELLO.md`: the one argument that says what the tool does.
fn tool_use_summary(block: &Value) -> String {
    let name = block.get("name").and_then(Value::as_str).unwrap_or("outil");
    let input = block.get("input");
    let argument = [
        "command",
        "file_path",
        "path",
        "pattern",
        "url",
        "description",
    ]
    .iter()
    .find_map(|key| input?.get(key)?.as_str());
    match argument {
        Some(argument) => format!("{name}: {}", preview(argument)),
        None => name.to_owned(),
    }
}

fn user_event(value: &Value) -> Classified {
    let mut parts = Vec::new();
    for block in content_blocks(value) {
        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        let text = match block.get("content") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        let failed = block.get("is_error").and_then(Value::as_bool) == Some(true);
        let prefix = if failed { "Erreur d'outil : " } else { "" };
        parts.push(format!("{prefix}{}", preview(&text)));
    }
    if parts.is_empty() {
        return (EventKind::System, "user".to_owned(), None);
    }
    (EventKind::ToolResult, parts.join(" · "), None)
}

fn result_event(value: &Value) -> Classified {
    let text = value.get("result").and_then(Value::as_str).unwrap_or("");
    // A failed run can still carry `subtype: "success"`; `is_error` is the verdict.
    let subtype = value.get("subtype").and_then(Value::as_str).unwrap_or("");
    let failed = value.get("is_error").and_then(Value::as_bool) == Some(true)
        || !(subtype.is_empty() || subtype == "success");
    let usage = Usage {
        cost_usd: value.get("total_cost_usd").and_then(Value::as_f64),
        tokens_in: value.pointer("/usage/input_tokens").and_then(Value::as_i64),
        tokens_out: value
            .pointer("/usage/output_tokens")
            .and_then(Value::as_i64),
    };
    if failed {
        let reason = if text.is_empty() {
            format!("le CLI a terminé en erreur ({subtype})")
        } else {
            preview(text)
        };
        (
            EventKind::Result,
            format!("Échec : {reason}"),
            Some(Finish {
                verdict: Verdict::Failure(reason),
                usage,
            }),
        )
    } else {
        (
            EventKind::Result,
            format!("Terminé : {}", preview(text)),
            Some(Finish {
                verdict: Verdict::Success,
                usage,
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUCCESS: &str = include_str!("../tests/fixtures/claude-success.jsonl");
    const BAD_MODEL: &str = include_str!("../tests/fixtures/claude-bad-model.jsonl");

    fn adapter() -> ClaudeAdapter {
        ClaudeAdapter {
            command: PathBuf::from("claude"),
        }
    }

    fn parse_all(stream: &str) -> Vec<ParsedLine> {
        stream
            .lines()
            .filter_map(|line| adapter().parse_line(line))
            .collect()
    }

    #[test]
    fn the_command_line_is_headless_stream_json_with_the_prompt_on_stdin() {
        let model = ModelName::parse_optional("haiku").unwrap();
        let spec = adapter().command(&Launch {
            prompt: "do it",
            model: model.as_ref(),
            permission_mode: PermissionMode::BypassPermissions,
        });
        assert_eq!(spec.program, PathBuf::from("claude"));
        assert_eq!(
            spec.args,
            [
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--permission-mode",
                "bypassPermissions",
                "--model",
                "haiku"
            ]
        );
        assert_eq!(spec.stdin, "do it");

        let spec = adapter().command(&Launch {
            prompt: "x",
            model: None,
            permission_mode: PermissionMode::Plan,
        });
        assert!(!spec.args.contains(&"--model".to_owned()));
        assert_eq!(spec.args[5], "plan");
    }

    #[test]
    fn a_real_successful_stream_is_classified_line_by_line() {
        let lines = parse_all(SUCCESS);
        let kinds: Vec<EventKind> = lines.iter().map(|l| l.event.kind).collect();
        use EventKind::*;
        assert_eq!(
            kinds,
            [
                System, Init, System, Message, ToolUse, ToolUse, System, ToolResult, System,
                ToolResult, Message, Message, Result
            ]
        );
        let summaries: Vec<&str> = lines.iter().map(|l| l.event.summary.as_str()).collect();
        assert_eq!(summaries[1], "Session démarrée (claude-haiku-4-5-20251001)");
        assert_eq!(summaries[4], "Write: /work/repo/HELLO.md");
        assert!(summaries[5].starts_with("Bash: git add HELLO.md && git commit"));
        assert!(summaries[7].starts_with("File created successfully"));
        assert!(summaries[11].starts_with("Done. Created HELLO.md"));
        assert!(summaries[12].starts_with("Terminé : "));
    }

    #[test]
    fn the_session_id_is_reported_from_the_first_line_that_has_one() {
        let lines = parse_all(SUCCESS);
        let ids: Vec<Option<&str>> = lines.iter().map(|l| l.session_id.as_deref()).collect();
        assert!(
            ids.iter()
                .all(|id| *id == Some("d157be31-f3e0-44f0-9aa9-7c88253236bf"))
        );
        assert_eq!(lines[0].event.kind, EventKind::System, "hook line");
    }

    #[test]
    fn the_final_line_carries_the_verdict_cost_and_tokens() {
        let lines = parse_all(SUCCESS);
        let finishes: Vec<&Finish> = lines.iter().filter_map(|l| l.finish.as_ref()).collect();
        assert_eq!(finishes.len(), 1, "only the result line finishes the run");
        assert_eq!(finishes[0].verdict, Verdict::Success);
        assert_eq!(
            finishes[0].usage,
            Usage {
                cost_usd: Some(0.0465764),
                tokens_in: Some(17),
                tokens_out: Some(347)
            }
        );
        assert!(lines.last().unwrap().finish.is_some());
    }

    #[test]
    fn a_result_flagged_is_error_fails_even_though_its_subtype_says_success() {
        let lines = parse_all(BAD_MODEL);
        let finish = lines.last().unwrap().finish.as_ref().unwrap();
        assert_eq!(
            finish.verdict,
            Verdict::Failure(
                "There's an issue with the selected model (not-a-real-model-xyz). It may not exist or you may not have access to it. Run --model to pick a different model.".to_owned()
            )
        );
        assert_eq!(finish.usage.cost_usd, Some(0.0));
        assert!(lines.last().unwrap().event.summary.starts_with("Échec : "));
        assert_eq!(lines.iter().filter(|l| l.finish.is_some()).count(), 1);
    }

    #[test]
    fn error_subtypes_fail_without_an_is_error_flag() {
        let line = r#"{"type":"result","subtype":"error_max_turns","session_id":"s","total_cost_usd":0.5,"usage":{"input_tokens":1,"output_tokens":2}}"#;
        let parsed = adapter().parse_line(line).unwrap();
        let finish = parsed.finish.unwrap();
        assert!(
            matches!(finish.verdict, Verdict::Failure(ref why) if why.contains("error_max_turns"))
        );
        assert_eq!(finish.usage.cost_usd, Some(0.5));
    }

    #[test]
    fn malformed_lines_become_events_that_keep_the_raw_text() {
        for (line, why) in [
            ("{\"type\":\"assistant\",\"mess", "pas du JSON"),
            ("plain text from a wrapper script", "pas du JSON"),
            ("[1,2]", "JSON sans champ type"),
            ("{\"no\":\"type\"}", "JSON sans champ type"),
        ] {
            let parsed = adapter().parse_line(line).unwrap();
            assert_eq!(parsed.event.kind, EventKind::Malformed, "{line}");
            assert_eq!(parsed.event.payload, line);
            assert!(
                parsed.event.summary.contains(why),
                "{}",
                parsed.event.summary
            );
            assert!(parsed.finish.is_none() && parsed.session_id.is_none());
        }
        assert!(adapter().parse_line("").is_none());
        assert!(adapter().parse_line("  \r\n").is_none());
    }

    #[test]
    fn unknown_event_types_are_kept_as_system_events() {
        let parsed = adapter()
            .parse_line(r#"{"type":"future_thing","session_id":"abc"}"#)
            .unwrap();
        assert_eq!(parsed.event.kind, EventKind::System);
        assert_eq!(parsed.event.summary, "future_thing");
        assert_eq!(parsed.session_id.as_deref(), Some("abc"));
    }

    #[test]
    fn tool_errors_are_flagged_and_long_output_is_cut_to_one_line() {
        let long = "x".repeat(500);
        let line = format!(
            r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","is_error":true,"content":[{{"type":"text","text":"line one\n{long}"}}]}}]}}}}"#
        );
        let parsed = adapter().parse_line(&line).unwrap();
        assert_eq!(parsed.event.kind, EventKind::ToolResult);
        assert_eq!(parsed.event.summary, "Erreur d'outil : line one");

        let line = format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{long}"}}]}}}}"#
        );
        let summary = adapter().parse_line(&line).unwrap().event.summary;
        assert_eq!(summary.chars().count(), SUMMARY_CHARS + 1);
        assert!(summary.ends_with('…'));
    }

    #[test]
    fn the_payload_is_the_untouched_line() {
        let first = SUCCESS.lines().nth(1).unwrap();
        assert_eq!(adapter().parse_line(first).unwrap().event.payload, first);
    }
}
