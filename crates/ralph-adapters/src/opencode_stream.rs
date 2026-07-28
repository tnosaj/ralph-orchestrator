//! OpenCode stream event types for parsing `--output-format json` output.
//!
//! When invoked with `opencode run --format json`, OpenCode emits
//! newline-delimited JSON events. This module provides typed Rust structures
//! for deserializing and processing these events — the same role
//! `claude_stream.rs` plays for Claude's `--output-format stream-json`.
//!
//! Confirmed live against a running `opencode` CLI (2026-07-28): a plain
//! turn emits `step_start` → `text` → `step_finish` (`reason: "stop"`); a
//! turn that uses a tool emits a `tool_use` event before an intermediate
//! `step_finish` (`reason: "tool-calls"`), then repeats until a final
//! `step_finish` with `reason: "stop"`. `tool_use.state.metadata.output.exit`
//! carries the real exit code of the underlying shell command — the
//! authoritative signal for whether e.g. a `ralph emit ...` call actually
//! succeeded, independent of whatever happens to the surrounding process
//! afterward.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Token usage reported on a `step_finish` event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct OpencodeTokens {
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub reasoning: u64,
    #[serde(default)]
    pub cache: OpencodeCacheTokens,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct OpencodeCacheTokens {
    #[serde(default)]
    pub write: u64,
    #[serde(default)]
    pub read: u64,
}

/// Events emitted by `opencode run --format json`.
///
/// Deliberately handles only the subset OpenCode's own `type` values map to
/// today (`step_start`, `text`, `tool_use`, `step_finish`); anything else
/// falls through `parse_line`'s `Err` branch and is skipped, same
/// tolerance-of-unknown-lines policy as `claude_stream.rs`.
#[derive(Debug, Clone, PartialEq)]
pub enum OpencodeStreamEvent {
    /// A new assistant step (turn segment) has begun.
    StepStart,
    /// Assistant text content.
    Text { text: String },
    /// A tool invocation completed (only `bash` is inspected today — other
    /// tool names are still parsed but carry `command: None`).
    ToolUse {
        tool: String,
        command: Option<String>,
        exit_code: Option<i32>,
        output: Option<String>,
    },
    /// A step completed. `reason: "stop"` means the assistant is genuinely
    /// done with its turn; `reason: "tool-calls"` means more steps follow.
    StepFinish {
        reason: String,
        tokens: OpencodeTokens,
        cost: f64,
    },
}

/// Parses NDJSON lines from OpenCode's `--format json` output.
pub struct OpencodeStreamParser;

impl OpencodeStreamParser {
    /// Parse a single line of NDJSON output.
    ///
    /// Returns `None` for empty lines, malformed JSON, or event shapes not
    /// handled above (logged at debug level, never a hard error — a single
    /// unparseable line must not abort the stream).
    pub fn parse_line(line: &str) -> Option<OpencodeStreamEvent> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }

        let value: Value = serde_json::from_str(trimmed).ok()?;
        let outer_type = value.get("type")?.as_str()?;
        let part = value.get("part")?;

        match outer_type {
            "step_start" => Some(OpencodeStreamEvent::StepStart),
            "text" => {
                let text = part.get("text")?.as_str()?.to_string();
                Some(OpencodeStreamEvent::Text { text })
            }
            "tool_use" => {
                let tool = part.get("tool")?.as_str().unwrap_or("unknown").to_string();
                let state = part.get("state");
                let command = state
                    .and_then(|s| s.get("input"))
                    .and_then(|i| i.get("command"))
                    .and_then(|c| c.as_str())
                    .map(str::to_string);
                let exit_code = state
                    .and_then(|s| s.get("metadata"))
                    .and_then(|m| m.get("exit"))
                    .and_then(|e| e.as_i64())
                    .map(|e| e as i32);
                let output = state
                    .and_then(|s| s.get("output"))
                    .and_then(|o| o.as_str())
                    .map(str::to_string);
                Some(OpencodeStreamEvent::ToolUse {
                    tool,
                    command,
                    exit_code,
                    output,
                })
            }
            "step_finish" => {
                let reason = part
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let tokens = part
                    .get("tokens")
                    .and_then(|t| serde_json::from_value(t.clone()).ok())
                    .unwrap_or_default();
                let cost = part.get("cost").and_then(|c| c.as_f64()).unwrap_or(0.0);
                Some(OpencodeStreamEvent::StepFinish {
                    reason,
                    tokens,
                    cost,
                })
            }
            _ => None,
        }
    }
}

/// Returns true if `command` looks like the `ralph emit` handoff call this
/// project's hat instructions use (`ralph emit <topic> ... `). Used to
/// identify which `ToolUse` event's exit code is the authoritative
/// completion signal for the turn, as opposed to any other bash call the
/// model happened to make.
pub fn command_is_ralph_emit(command: &str) -> bool {
    command.contains("ralph emit")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_step_start() {
        let json = r#"{"type":"step_start","timestamp":1,"sessionID":"s","part":{"type":"step-start"}}"#;
        let event = OpencodeStreamParser::parse_line(json).unwrap();
        assert_eq!(event, OpencodeStreamEvent::StepStart);
    }

    #[test]
    fn test_parse_text() {
        let json = r#"{"type":"text","timestamp":1,"sessionID":"s","part":{"type":"text","text":"done"}}"#;
        let event = OpencodeStreamParser::parse_line(json).unwrap();
        assert_eq!(
            event,
            OpencodeStreamEvent::Text {
                text: "done".to_string()
            }
        );
    }

    #[test]
    fn test_parse_tool_use_success() {
        let json = r#"{"type":"tool_use","part":{"type":"tool","tool":"bash",
            "state":{"status":"completed","input":{"command":"ralph emit build.done \"ok\""},
            "output":"Event emitted: build.done\n",
            "metadata":{"output":"Event emitted: build.done\n","exit":0}}}}"#;
        let event = OpencodeStreamParser::parse_line(json).unwrap();
        match event {
            OpencodeStreamEvent::ToolUse {
                tool,
                command,
                exit_code,
                output,
            } => {
                assert_eq!(tool, "bash");
                assert_eq!(command.as_deref(), Some("ralph emit build.done \"ok\""));
                assert_eq!(exit_code, Some(0));
                assert_eq!(output.as_deref(), Some("Event emitted: build.done\n"));
            }
            _ => panic!("Expected ToolUse event"),
        }
    }

    #[test]
    fn test_parse_tool_use_failure_exit_code() {
        let json = r#"{"type":"tool_use","part":{"type":"tool","tool":"bash",
            "state":{"status":"completed","input":{"command":"this-command-does-not-exist"},
            "output":"zsh: command not found\n",
            "metadata":{"output":"zsh: command not found\n","exit":127}}}}"#;
        let event = OpencodeStreamParser::parse_line(json).unwrap();
        match event {
            OpencodeStreamEvent::ToolUse { exit_code, .. } => {
                assert_eq!(exit_code, Some(127));
            }
            _ => panic!("Expected ToolUse event"),
        }
    }

    #[test]
    fn test_parse_step_finish_stop_with_tokens() {
        let json = r#"{"type":"step_finish","part":{"type":"step-finish","reason":"stop",
            "tokens":{"total":5887,"input":5866,"output":7,"reasoning":14,
                      "cache":{"write":1,"read":2}},"cost":0.01}}"#;
        let event = OpencodeStreamParser::parse_line(json).unwrap();
        match event {
            OpencodeStreamEvent::StepFinish {
                reason,
                tokens,
                cost,
            } => {
                assert_eq!(reason, "stop");
                assert_eq!(tokens.total, 5887);
                assert_eq!(tokens.input, 5866);
                assert_eq!(tokens.output, 7);
                assert_eq!(tokens.reasoning, 14);
                assert_eq!(tokens.cache.write, 1);
                assert_eq!(tokens.cache.read, 2);
                assert!((cost - 0.01).abs() < f64::EPSILON);
            }
            _ => panic!("Expected StepFinish event"),
        }
    }

    #[test]
    fn test_parse_step_finish_tool_calls() {
        let json = r#"{"type":"step_finish","part":{"type":"step-finish","reason":"tool-calls",
            "tokens":{"total":1,"input":1,"output":0,"reasoning":0,"cache":{"write":0,"read":0}},
            "cost":0}}"#;
        let event = OpencodeStreamParser::parse_line(json).unwrap();
        match event {
            OpencodeStreamEvent::StepFinish { reason, .. } => assert_eq!(reason, "tool-calls"),
            _ => panic!("Expected StepFinish event"),
        }
    }

    #[test]
    fn test_parse_empty_line() {
        assert!(OpencodeStreamParser::parse_line("").is_none());
        assert!(OpencodeStreamParser::parse_line("   ").is_none());
    }

    #[test]
    fn test_parse_malformed_json() {
        assert!(OpencodeStreamParser::parse_line("{not valid json}").is_none());
        assert!(OpencodeStreamParser::parse_line("plain text").is_none());
    }

    #[test]
    fn test_parse_unknown_type_is_skipped() {
        let json = r#"{"type":"something_new","part":{"type":"whatever"}}"#;
        assert!(OpencodeStreamParser::parse_line(json).is_none());
    }

    #[test]
    fn test_command_is_ralph_emit() {
        assert!(command_is_ralph_emit(
            "ralph emit build.done \"tests: pass\""
        ));
        assert!(!command_is_ralph_emit("echo hello"));
        assert!(!command_is_ralph_emit("go test ./..."));
    }
}
