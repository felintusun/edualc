//! What a window attaching later is told again, and what a session waits on you for.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::SystemTime;

use crate::agent;
use crate::external::wire::{self, IncomingRequest, TaskId};
use crate::ipc::HostedState;
use crate::tools;

use super::MAX_PENDING;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Asked {
    pub(super) summary: String,
    pub(super) line: String,
    pub(super) since: SystemTime,
}

/// What a window attaching is told again (sans-io): how many turns are under way, the
/// process's latest `init` (its session, model and mode), what runs in the background, then the
/// permission requests it asked and nothing answered yet. Stored as serialized [`agent::Event`]
/// lines ready to send over IPC.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Pending {
    pub(super) init: Option<String>,
    /// The latest list of background tasks: each one names them all.
    pub(super) tasks: Option<String>,
    /// What was said about each task begun in this turn and not yet ended: a foreground one is
    /// in no list of background tasks.
    pub(super) open_tasks: BTreeMap<TaskId, Vec<String>>,
    /// How far each running task got, as last said; a background one runs on after its turn.
    pub(super) progress: BTreeMap<TaskId, String>,
    pub(super) asked: BTreeMap<String, Asked>,
    /// Turns under way: prompts written, and not yet ended by a result. Counted as the window
    /// counts them, for one that attaches meanwhile.
    pub(super) turns: usize,
    /// A turn the session started itself, as for another session's message: no prompt was
    /// written for it, so the model's first words tell it, and its result ends it.
    pub(super) own_turn: bool,
}

impl Pending {
    /// Takes an event the process wrote, with the line it came as, to give windows that attach.
    pub(super) fn saw(&mut self, event: &agent::Event, wire_line: &str, cwd: &Path) {
        match event {
            agent::Event::System(wire::System::Init { .. }) => {
                self.init = Some(wire_line.to_owned());
            }
            agent::Event::System(wire::System::BackgroundTasksChanged { .. }) => {
                self.tasks = Some(wire_line.to_owned());
            }
            agent::Event::System(wire::System::TaskStarted { task_id, .. }) => {
                if self.open_tasks.len() < MAX_PENDING {
                    self.open_tasks
                        .insert(task_id.clone(), vec![wire_line.to_owned()]);
                }
            }
            agent::Event::System(wire::System::TaskUpdated { task_id, .. }) => {
                if let Some(said) = self.open_tasks.get_mut(task_id)
                    && said.len() < MAX_PENDING
                {
                    said.push(wire_line.to_owned());
                }
            }
            agent::Event::System(wire::System::TaskNotification { task_id }) => {
                self.open_tasks.remove(task_id);
                self.progress.remove(task_id);
            }
            agent::Event::System(wire::System::TaskProgress { task_id, .. }) => {
                if self.progress.len() < MAX_PENDING || self.progress.contains_key(task_id) {
                    self.progress.insert(task_id.clone(), wire_line.to_owned());
                }
            }
            agent::Event::ControlRequest {
                request_id,
                request,
            } if self.asked.len() < MAX_PENDING => {
                let asked = Asked {
                    summary: request_summary(request, cwd),
                    since: SystemTime::now(),
                    line: wire_line.to_owned(),
                };
                self.asked.insert(request_id.to_string(), asked);
            }
            agent::Event::PermissionAsked {
                request_id, call, ..
            } if self.asked.len() < MAX_PENDING => {
                let asked = Asked {
                    summary: acp_request_summary(call, cwd),
                    since: SystemTime::now(),
                    line: wire_line.to_owned(),
                };
                self.asked.insert(request_id.to_string(), asked);
            }
            agent::Event::RequestCancelled { request_id } => {
                self.asked.remove(&request_id.to_string());
            }
            agent::Event::Result(_) => {
                self.turns = self.turns.saturating_sub(1);
                self.own_turn = false;
                // No foreground task outlives its turn, and the list names those that run on.
                self.open_tasks.clear();
            }
            agent::Event::Assistant { .. }
            | agent::Event::Stream { .. }
            | agent::Event::ToolCall { .. }
                if self.turns == 0 =>
            {
                self.own_turn = true;
            }
            agent::Event::Stream { .. }
            | agent::Event::Assistant { .. }
            | agent::Event::ToolCall { .. }
            | agent::Event::User(_)
            | agent::Event::System(_)
            | agent::Event::RateLimits(_)
            | agent::Event::ControlRequest { .. }
            | agent::Event::PermissionAsked { .. }
            | agent::Event::ControlAnswered(_) => {}
        }
    }

    /// Takes a command the window sent to the session.
    pub(super) fn sent(&mut self, command: &agent::Command) {
        match command {
            agent::Command::ControlResponse { .. } | agent::Command::Choose { .. } => {
                if let Some(id) = command.answered_request_id() {
                    self.asked.remove(&id.to_string());
                }
            }
            agent::Command::Prompt { .. } => self.turns = self.turns.saturating_add(1),
            agent::Command::Shell { .. } | agent::Command::ControlRequest { .. } => {}
        }
    }

    /// Returns what the session does now; a question's summary was made when it was asked.
    pub(super) fn state(&self) -> HostedState {
        if let Some(asked) = self.asked.values().min_by_key(|asked| asked.since) {
            HostedState::Waiting {
                question: asked.summary.clone(),
                since: asked.since,
            }
        } else if self.in_turn() {
            HostedState::Busy
        } else {
            HostedState::Idle
        }
    }

    /// Returns whether a turn is under way, started by a prompt or by the session itself.
    pub(super) fn in_turn(&self) -> bool {
        self.turns > 0 || self.own_turn
    }

    pub(super) fn lines(&self) -> impl Iterator<Item = &String> {
        self.init
            .iter()
            .chain(self.tasks.iter())
            .chain(self.open_tasks.values().flatten())
            .chain(self.progress.values())
            .chain(self.asked.values().map(|asked| &asked.line))
    }
}

pub(super) fn request_summary(request: &IncomingRequest, cwd: &Path) -> String {
    let IncomingRequest::CanUseTool {
        tool_name,
        input,
        description,
        ..
    } = request
    else {
        return "Waiting for you".to_owned();
    };
    let view = tools::describe(tool_name, input, cwd, tools::Depth::Headline);
    let summary = match tool_name {
        wire::ToolName::AskUserQuestion => {
            if view.summary.trim().is_empty() {
                "Question for you".to_owned()
            } else {
                view.summary
            }
        }
        wire::ToolName::ExitPlanMode => "Plan ready for review".to_owned(),
        wire::ToolName::Bash
        | wire::ToolName::BashOutput
        | wire::ToolName::KillShell
        | wire::ToolName::Monitor
        | wire::ToolName::Read
        | wire::ToolName::Write
        | wire::ToolName::Edit
        | wire::ToolName::MultiEdit
        | wire::ToolName::NotebookRead
        | wire::ToolName::NotebookEdit
        | wire::ToolName::Glob
        | wire::ToolName::Grep
        | wire::ToolName::Ls
        | wire::ToolName::WebFetch
        | wire::ToolName::WebSearch
        | wire::ToolName::Task
        | wire::ToolName::Agent
        | wire::ToolName::TodoWrite
        | wire::ToolName::Skill
        | wire::ToolName::SendMessage
        | wire::ToolName::Edualc(_)
        | wire::ToolName::Other(_) => {
            if !description.trim().is_empty() {
                description.clone()
            } else if !view.summary.trim().is_empty() {
                format!("{}: {}", view.title, view.summary)
            } else {
                view.title
            }
        }
    };
    let first = summary.lines().next().unwrap_or(&summary).trim();
    if first.is_empty() {
        "Waiting for you".to_owned()
    } else {
        tools::truncate_chars(first, 120)
    }
}

/// Says what an ACP agent asks about, as its call's card does.
pub(super) fn acp_request_summary(call: &crate::call::AcpCall, cwd: &Path) -> String {
    let view = tools::describe_acp(call, cwd, tools::Depth::Headline);
    let summary = if view.summary.trim().is_empty() {
        view.title
    } else {
        format!("{}: {}", view.title, view.summary)
    };
    let first = summary.lines().next().unwrap_or(&summary).trim();
    tools::truncate_chars(first, 120)
}
