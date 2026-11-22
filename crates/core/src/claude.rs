//! Claude Code CLI process adapter codecs: mapping Claude Code's `stream-json` wire format
//! (`external::wire`) to and from edualc's provider-neutral [`crate::agent`] events and
//! commands.

use crate::agent;
use crate::external::wire::{ControlPeek, Input, Message, PeekedKind, RequestId};

/// The `claude` to run: `$CLAUDE_BIN` as the daemon was started with, or the one on `PATH`.
pub fn program() -> std::ffi::OsString {
    std::env::var_os("CLAUDE_BIN").unwrap_or_else(|| "claude".into())
}

/// Converts a parsed Claude `stream-json` message into a provider-neutral agent event, returning
/// `None` for unknown future message types (`Message::Other`).
pub fn from_message(message: Message) -> Option<agent::Event> {
    Some(match message {
        Message::StreamEvent {
            event,
            parent_tool_use_id,
        } => agent::Event::Stream {
            event,
            parent_tool_use_id,
        },
        Message::Assistant {
            message,
            parent_tool_use_id,
            error,
        } => agent::Event::Assistant {
            message,
            parent_tool_use_id,
            error,
        },
        Message::User(line) => agent::Event::User(line),
        Message::System(system) => agent::Event::System(system),
        Message::Result(result) => agent::Event::Result(result),
        Message::RateLimitEvent { rate_limit_info } => agent::Event::RateLimits(rate_limit_info),
        Message::ControlRequest {
            request_id,
            request,
        } => agent::Event::ControlRequest {
            request_id,
            request,
        },
        Message::ControlResponse { response } => agent::Event::ControlAnswered(response),
        Message::ControlCancelRequest { request_id } => {
            agent::Event::RequestCancelled { request_id }
        }
        Message::Other => return None,
    })
}

/// Converts a provider-neutral [`agent::Command`] into Claude Code's `stream-json` stdin line.
pub fn to_input(command: agent::Command) -> Input {
    match command {
        agent::Command::Prompt {
            shell,
            images,
            text,
        } => Input::prompt_with(&shell, images, &text),
        agent::Command::Shell { command } => Input::bash_command(&command),
        agent::Command::ControlRequest {
            request_id,
            request,
        } => Input::ControlRequest {
            request_id,
            request,
        },
        agent::Command::ControlResponse { response } => Input::ControlResponse { response },
    }
}

/// Parses a single stdout line written by `claude -p --output-format stream-json`.
pub fn output_of_line(line: String) -> agent::Output {
    let error = match serde_json::from_str::<Message>(&line) {
        Ok(message) => {
            return match from_message(message) {
                Some(event) => agent::Output::Event(event),
                None => agent::Output::Log(line),
            };
        }
        Err(error) => error,
    };
    let lost = serde_json::from_str::<ControlPeek>(&line)
        .ok()
        .and_then(|peek| match (peek.kind, peek.request_id) {
            (PeekedKind::Result, _) => Some(agent::Lost::Result),
            (PeekedKind::ControlRequest, Some(request)) => {
                Some(agent::Lost::Request(RequestId::from(request)))
            }
            (PeekedKind::ControlRequest, None)
            | (
                PeekedKind::System
                | PeekedKind::ControlCancelRequest
                | PeekedKind::ControlResponse
                | PeekedKind::User
                | PeekedKind::Assistant
                | PeekedKind::StreamEvent
                | PeekedKind::Other,
                _,
            ) => None,
        });
    match lost {
        Some(lost) => {
            tracing::warn!(%error, ?lost, "a line from claude the window can't read");
            agent::Output::Unreadable {
                lost,
                error: error.to_string(),
            }
        }
        None => {
            tracing::debug!(%error, "a line from claude that isn't a message");
            agent::Output::Log(line)
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn a_result_or_request_the_window_cant_read_is_told_as_lost() {
        let lost = |line: &str| match output_of_line(line.to_owned()) {
            agent::Output::Unreadable { lost, .. } => Some(lost),
            agent::Output::Log(_) => None,
            other @ (agent::Output::Started { .. }
            | agent::Output::Failed(_)
            | agent::Output::Event(_)
            | agent::Output::TurnChanged(_)
            | agent::Output::Ended(_)) => panic!("{line} read as {other:?}"),
        };
        assert_eq!(
            lost(r#"{"type":"result","subtype":"success","is_error":"no"}"#),
            Some(agent::Lost::Result)
        );
        assert_eq!(
            lost(
                r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":7}}"#
            ),
            Some(agent::Lost::Request(RequestId::from("r1".to_owned())))
        );
        assert_eq!(lost(r#"{"type":"assistant","message":5}"#), None);
        assert_eq!(lost("not JSON at all"), None);
        assert!(matches!(
            output_of_line(
                r#"{"type":"result","subtype":"success","result":"ok","total_cost_usd":null}"#
                    .to_owned()
            ),
            agent::Output::Event(agent::Event::Result(_))
        ));
    }

    #[test]
    fn command_encodes_prompt_and_control_to_stream_json() {
        let cmd = agent::Command::Prompt {
            shell: vec!["<bash-input>ls</bash-input>".to_owned()],
            images: Vec::new(),
            text: "hi".to_owned(),
        };
        let encoded = serde_json::to_string(&to_input(cmd)).expect("encodes");
        assert!(encoded.contains("<bash-input>ls</bash-input>"));
        assert!(encoded.contains("hi"));
    }
}
