//! Host-side process drivers normalizing backend wire formats (`claude -p --output-format
//! stream-json` vs ACP adapter `agent` JSON streams) behind typed [`crate::agent`] messages.

use std::collections::HashMap;

use crate::agent::{Command, Event, Lost, Output};
use crate::claude;
use crate::external::wire::{self, Reply, ResultKind, TurnResult};
use crate::provider::Backend;

/// How `edualcd` speaks to a running backend process over its stdio pipes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Driver {
    #[default]
    Claude,
    Acp,
}

/// One parsed stdout line from the backend process.
#[derive(Debug)]
pub enum Decoded {
    Event {
        event: Event,
        wire_line: String,
    },
    UnreadableRequest {
        reply_to_process: String,
        notice: String,
    },
    Log(String),
}

impl Driver {
    pub fn of(backend: Backend) -> Self {
        if backend.is_acp() {
            Self::Acp
        } else {
            Self::Claude
        }
    }

    /// Encodes a provider-neutral [`Command`] for this backend's stdin pipe.
    pub fn encode_command(self, command: Command) -> Result<String, serde_json::Error> {
        match self {
            Self::Claude => serde_json::to_string(&claude::to_input(command)),
            Self::Acp => command.encode(),
        }
    }

    /// Parses one stdout line from the backend process into a typed [`Event`] (and serialized
    /// forwarding payload) or diagnostic log.
    pub fn decode_output(self, line: String) -> Decoded {
        match self {
            Self::Acp => match serde_json::from_str::<Event>(&line) {
                Ok(event) => Decoded::Event {
                    event,
                    wire_line: line,
                },
                Err(_) => Decoded::Log(line),
            },
            Self::Claude => match claude::output_of_line(line) {
                Output::Event(event) => match serde_json::to_string(&event) {
                    Ok(wire_line) => Decoded::Event { event, wire_line },
                    Err(error) => Decoded::Log(format!("couldn't forward agent event: {error}")),
                },
                Output::Unreadable {
                    lost: Lost::Result,
                    error,
                } => {
                    let event = Event::Result(TurnResult {
                        subtype: ResultKind::ErrorDuringExecution,
                        is_error: true,
                        result: None,
                        errors: vec![serde_json::Value::String(format!(
                            "Claude Code ended the turn with a line edualc can't read: {error}"
                        ))],
                        total_cost_usd: None,
                        model_usage: HashMap::new(),
                        api_error_status: None,
                        duration_ms: None,
                        usage: wire::Usage::default(),
                    });
                    match serde_json::to_string(&event) {
                        Ok(wire_line) => Decoded::Event { event, wire_line },
                        Err(err) => Decoded::Log(format!(
                            "Claude Code ended the turn with an unreadable line ({error}; {err})"
                        )),
                    }
                }
                Output::Unreadable {
                    lost: Lost::Request(request_id),
                    error,
                } => {
                    let refusal = Command::ControlResponse {
                        response: Reply::Error {
                            request_id,
                            error: "edualc can't read this request".to_owned(),
                        },
                    };
                    let reply_to_process = self.encode_command(refusal).unwrap_or_default();
                    Decoded::UnreadableRequest {
                        reply_to_process,
                        notice: format!(
                            "Claude Code asked something edualc can't read, and was told so: {error}"
                        ),
                    }
                }
                Output::Log(line) => Decoded::Log(line),
                Output::Started { .. }
                | Output::Failed(_)
                | Output::TurnChanged(_)
                | Output::Ended(_) => Decoded::Log(String::new()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_driver_translates_commands_and_events() {
        let cmd = Command::Shell {
            command: "git status".to_owned(),
        };
        let encoded = Driver::Claude.encode_command(cmd).expect("encodes");
        assert!(encoded.contains("git status"));

        let decoded = Driver::Claude.decode_output(
            r#"{"type":"result","subtype":"success","result":"done","total_cost_usd":null}"#
                .to_owned(),
        );
        assert!(matches!(
            decoded,
            Decoded::Event {
                event: Event::Result(_),
                ..
            }
        ));
    }

    #[test]
    fn acp_driver_round_trips_agent_events_directly() {
        let event = Event::Result(TurnResult {
            subtype: ResultKind::Success,
            is_error: false,
            result: Some("ok".to_owned()),
            errors: Vec::new(),
            total_cost_usd: None,
            model_usage: HashMap::new(),
            api_error_status: None,
            duration_ms: None,
            usage: wire::Usage::default(),
        });
        let raw = serde_json::to_string(&event).expect("encodes");
        let decoded = Driver::Acp.decode_output(raw);
        assert!(matches!(
            decoded,
            Decoded::Event {
                event: Event::Result(_),
                ..
            }
        ));
    }
}
