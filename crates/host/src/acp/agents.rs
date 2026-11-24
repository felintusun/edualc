//! The agents edualc runs over ACP, and what each does its own way.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::{Child, Command};

use crate::external::acp::{AgyArguments, AgyQuestion, RpcError, SwarmAction, ToolCall};
use crate::external::tool_input::{AgentInput, KnownCall, ListSessionsInput, SendMessageInput};
use crate::external::wire::{Question, QuestionOption, Questions};

use super::Error;

/// The tool agy sees edualc's messaging tool as, by the MCP server's name and the tool's.
const AGY_SEND: &str = "mcp_edualc_send_message";
/// edualc's tool listing the sessions, as agy names it.
const AGY_LIST: &str = "mcp_edualc_list_sessions";

/// The agents edualc runs through the Agent Client Protocol, and what each does its own way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, strum::Display)]
#[strum(serialize_all = "snake_case")]
pub(crate) enum Agent {
    Agy,
    Opencode,
}

impl Agent {
    /// Returns the program that speaks ACP.
    pub(super) fn program(self) -> &'static str {
        match self {
            Self::Agy => "agy",
            Self::Opencode => "opencode",
        }
    }

    /// Returns the arguments that make it speak ACP on stdio.
    fn arguments(self) -> &'static [&'static str] {
        match self {
            Self::Agy => &["acp", "--disable-telemetry"],
            Self::Opencode => &["acp"],
        }
    }

    /// Returns how to get it, told when it isn't on PATH.
    pub(super) fn install_hint(self) -> &'static str {
        match self {
            Self::Agy => "install it from antigravity.google",
            Self::Opencode => "install it from opencode.ai",
        }
    }

    /// Returns whether the agent loads a session of its own again (`session/load`), so that
    /// edualc can continue it.
    pub(super) fn loads_sessions(self) -> bool {
        match self {
            Self::Agy | Self::Opencode => true,
        }
    }

    /// Returns whether the agent takes `session/fork`.
    pub(super) fn can_fork(self) -> bool {
        self.backend().can_fork()
    }

    pub(super) fn backend(self) -> crate::provider::Backend {
        match self {
            Self::Agy => crate::provider::Backend::Agy,
            Self::Opencode => crate::provider::Backend::Opencode,
        }
    }

    /// Returns whether it keeps records of the subagents and swarms it starts, which edualc
    /// shows as the session's background tasks (see [`brain`](super::brain)).
    pub(super) fn keeps_subagents(self) -> bool {
        match self {
            Self::Agy => true,
            Self::Opencode => false,
        }
    }

    /// Returns the arguments that make it print its plan's quotas (see
    /// [`quotas`](super::quotas::quotas)); `None` for an agent with no quotas to say.
    pub(super) fn quotas_arguments(self) -> Option<&'static [&'static str]> {
        match self {
            Self::Agy => Some(&["-p", "/usage", "--output-format", "json"]),
            Self::Opencode => None,
        }
    }

    /// Returns whether it reports its plan's quotas, which the window shows as the plan's
    /// limits.
    pub(super) fn reports_quotas(self) -> bool {
        self.quotas_arguments().is_some()
    }

    /// Returns the arguments it sends with a tool call before the call's own, as opencode
    /// sends the working folder first.
    pub(super) fn early_arguments(self) -> &'static [&'static str] {
        match self {
            Self::Agy => &[],
            Self::Opencode => &["cwd"],
        }
    }

    /// Starts the agent in `cwd`, with ACP on its stdio.
    pub(super) fn start(self, cwd: &Path) -> Result<Child, Error> {
        let program = on_path(self.program()).ok_or(Error::NotOnPath(self))?;
        Command::new(&program)
            .args(self.arguments())
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| Error::Start(self, source))
    }

    /// Returns a tool call named `name` that edualc acts on, as the window knows it: a message,
    /// a list of the sessions, a swarm's new member, a question. `None` for any other, which
    /// the window shows as the agent gives it.
    pub(super) fn tool(self, name: &str, call: &ToolCall) -> Option<KnownCall> {
        match self {
            Self::Agy => {
                let given: AgyArguments = arguments(name, call);
                match name {
                    AGY_SEND => Some(KnownCall::SendMessage(SendMessageInput {
                        to: given.to.unwrap_or_default(),
                        message: given.message.unwrap_or_default(),
                    })),
                    AGY_LIST => Some(KnownCall::ListSessions(ListSessionsInput {})),
                    // A swarm's board, read as a message to it.
                    "post_message" => Some(KnownCall::SendMessage(SendMessageInput {
                        to: "the board".to_owned(),
                        message: given.text.unwrap_or_default(),
                    })),
                    "swarm" if given.action == Some(SwarmAction::Add) => {
                        Some(KnownCall::Agent(AgentInput {
                            description: "Joins the swarm".to_owned(),
                            prompt: given.brief.unwrap_or_default(),
                            subagent_type: given.name,
                        }))
                    }
                    "ask_question" => Some(KnownCall::Questions(questions(given.questions))),
                    _ => None,
                }
            }
            // Its question, to-do list and subagents are read by the translator's names.
            Self::Opencode => None,
        }
    }

    /// Returns whether the agent refused a prompt for want of a valid sign-in: by ACP's code,
    /// or in its own words for a token Google's API refused (`UNAUTHENTICATED`, HTTP 401) or
    /// for no sign-in at all.
    pub(super) fn sign_in_refused(self, error: &RpcError) -> bool {
        error.sign_in_refused()
            || match self {
                Self::Agy => {
                    error.message.contains("UNAUTHENTICATED")
                        || error.message.contains("not logged into Antigravity")
                        || error.message.contains("Authentication required")
                }
                Self::Opencode => false,
            }
    }
}

/// Finds `program` on PATH.
pub(super) fn on_path(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|folder| folder.join(program))
            .find(|candidate| candidate.is_file())
    })
}

/// Reads the arguments of tool call `name`, as far as the agent has given them. None yet, or
/// ones edualc can't read (which are logged), read as none.
fn arguments<Arguments>(name: &str, call: &ToolCall) -> Arguments
where
    Arguments: serde::de::DeserializeOwned + Default,
{
    let Some(input) = call.raw_input.clone() else {
        return Arguments::default();
    };
    match serde_json::from_value::<Option<Arguments>>(input) {
        Ok(given) => given.unwrap_or_default(),
        Err(error) => {
            tracing::warn!(%error, tool = name, "a tool's arguments edualc can't read");
            Arguments::default()
        }
    }
}

/// Returns agy's questions as Claude Code's AskUserQuestion asks them.
fn questions(asked: Vec<AgyQuestion>) -> Questions {
    Questions {
        questions: asked
            .into_iter()
            .map(|asked| Question {
                question: asked.question,
                header: String::new(),
                options: asked
                    .options
                    .into_iter()
                    .map(|label| QuestionOption {
                        label,
                        description: String::new(),
                        preview: String::new(),
                        rest: serde_json::Map::new(),
                    })
                    .collect(),
                multi_select: asked.is_multi_select,
                rest: serde_json::Map::new(),
            })
            .collect(),
        answers: std::collections::BTreeMap::new(),
        annotations: std::collections::BTreeMap::new(),
        rest: serde_json::Map::new(),
    }
}
