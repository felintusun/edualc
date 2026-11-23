//! Requests that wait for you: a tool's permission, Claude's questions, a plan to approve, an
//! ACP agent's choices, and the answers sent back.

use serde::Deserialize;
use serde_json::Value;

use crate::call::{self, AcpCall, AcpOption, AcpOptionKind};
use crate::chat::ToolState;
use crate::external::wire::{
    self, Destination, PermissionResult, PermissionUpdate, RequestId, ToolInput, ToolName,
    ToolUseId,
};
use crate::plan;
use crate::present::Frontend;

use super::{App, Doing, Mode, PermissionMode};

/// A request from the session waiting for the user.
#[derive(Debug)]
pub struct Ask {
    pub request: RequestId,
    pub tool_use: Option<ToolUseId>,
    pub kind: AskKind,
}

#[derive(Debug)]
pub enum AskKind {
    /// May the tool run?
    Tool {
        tool: ToolName,
        input: Value,
        /// Rules the CLI offers to remember.
        suggestions: Vec<wire::Offered>,
        description: String,
    },
    /// AskUserQuestion: what is picked, by question and option, what was typed instead, and
    /// the notes that go with each answer.
    Questions {
        input: Value,
        asked: wire::Questions,
        picked: Vec<Vec<bool>>,
        other: Vec<String>,
        notes: Vec<String>,
    },
    /// ExitPlanMode: the plan's parts with the comment on each, and the part whose comment is
    /// being written.
    Plan {
        input: wire::PlanInput,
        parts: Vec<plan::PlanPart>,
        editing: Option<usize>,
    },
    /// May an ACP agent's call go ahead? Answered with one of the agent's choices.
    Acp {
        call: AcpCall,
        options: Vec<AcpOption>,
    },
}

impl AskKind {
    /// The input Claude Code asked about, given back as it was; an ACP agent's call has none.
    pub(super) fn as_asked(&self) -> Option<ToolInput> {
        match self {
            Self::Tool { input, .. } | Self::Questions { input, .. } => {
                Some(ToolInput::AsAsked(input.clone()))
            }
            Self::Plan { input, .. } => Some(ToolInput::Plan(input.clone())),
            Self::Acp { .. } => None,
        }
    }
}

impl<F: Frontend> App<F> {
    /// The waiting request the user answered, if it is still the one shown: an answer meant for
    /// a request that was cancelled or replaced meanwhile is not given to the next.
    pub(super) fn take_ask(&mut self, request: &RequestId) -> Option<Ask> {
        if self.asks.front().is_some_and(|ask| &ask.request == request) {
            return self.asks.pop_front();
        }
        tracing::warn!(%request, "an answer for a request no longer shown");
        self.redraw.prompt = true;
        None
    }

    /// Allows the waiting tool call; `always` also takes the first rule the CLI offered.
    pub(super) fn allow(&mut self, request: &RequestId, always: bool) {
        let Some(ask) = self.take_ask(request) else {
            return;
        };
        if let AskKind::Acp { options, .. } = &ask.kind {
            let kinds: &[AcpOptionKind] = if always {
                &[AcpOptionKind::AllowAlways, AcpOptionKind::AllowOnce]
            } else {
                &[AcpOptionKind::AllowOnce, AcpOptionKind::AllowAlways]
            };
            let chosen = call::first_of(options, kinds).map(|option| option.id.clone());
            self.choose(&ask, chosen, ToolState::Running);
            return;
        }
        let mut updated_permissions = Vec::new();
        if always
            && let AskKind::Tool { suggestions, .. } = &ask.kind
            && let Some(offered) = suggestions.first()
            && let Some(rule) = &offered.rule
        {
            // A mode it switches to is shown in the menu at once.
            if let wire::Suggestion::SetMode { mode } = rule {
                self.status.modes.claude_in(mode.clone());
            }
            updated_permissions.push(PermissionUpdate::Offered(offered.raw.clone()));
        }
        let Some(updated_input) = ask.kind.as_asked() else {
            return;
        };
        let response = PermissionResult::Allow {
            updated_input,
            updated_permissions,
        };
        self.answer(&ask, response, ToolState::Running);
    }

    /// Denies the waiting request. A Claude Code tool call denied with nothing said stops the
    /// turn, as in the terminal; with what to do instead, Claude carries on with that.
    pub(super) fn deny(&mut self, request: &RequestId, feedback: &str) {
        let Some(ask) = self.take_ask(request) else {
            return;
        };
        let feedback = feedback.trim();
        let (message, interrupt) = match &ask.kind {
            AskKind::Questions { .. } => ("The user chose not to answer.".to_owned(), false),
            AskKind::Plan { input, parts, .. } => {
                let revise = plan::feedback(&input.plan, parts, feedback);
                if revise.is_empty() {
                    ("The user wants to keep planning.".to_owned(), false)
                } else {
                    (revise, false)
                }
            }
            // Another session's message, turned down: nothing of the session's own is stopped.
            AskKind::Tool { tool, .. } if *tool == ToolName::EdualcIncomingMessage => {
                ("The user turned the message down.".to_owned(), false)
            }
            AskKind::Tool { .. } if feedback.is_empty() => (
                "The user denied this tool use. Stop and wait for their instructions.".to_owned(),
                true,
            ),
            AskKind::Tool { .. } => (feedback.to_owned(), false),
            // The agent's own rejection, and the agent goes on from it as it would on its own.
            AskKind::Acp { options, .. } => {
                let kinds = [AcpOptionKind::RejectOnce, AcpOptionKind::RejectAlways];
                let chosen = call::first_of(options, &kinds).map(|option| option.id.clone());
                self.choose(&ask, chosen, ToolState::Denied);
                return;
            }
        };
        if interrupt {
            self.status.stop_asked = self.status.turns > 0;
        }
        let response = PermissionResult::Deny { message, interrupt };
        self.answer(&ask, response, ToolState::Denied);
    }

    /// Approves the plan, with the comments on it as notes in the plan Claude works from.
    pub(super) fn approve_plan(
        &mut self,
        request: &RequestId,
        mode: PermissionMode,
        overall: &str,
    ) {
        let Some(ask) = self.take_ask(request) else {
            return;
        };
        let updated_input = match &ask.kind {
            AskKind::Plan { input, parts, .. } => ToolInput::Plan(wire::PlanInput {
                plan: plan::annotated(&input.plan, parts, overall),
                rest: input.rest.clone(),
            }),
            AskKind::Tool { .. } | AskKind::Questions { .. } | AskKind::Acp { .. } => {
                match ask.kind.as_asked() {
                    Some(input) => input,
                    None => {
                        tracing::warn!(%request, "a plan approved for an agent's call");
                        self.asks.push_front(ask);
                        return;
                    }
                }
            }
        };
        self.status.modes.claude_in(Mode::Known(mode));
        let response = PermissionResult::Allow {
            updated_input,
            updated_permissions: vec![PermissionUpdate::SetMode {
                mode,
                destination: Destination::Session,
            }],
        };
        self.answer(&ask, response, ToolState::Running);
    }

    pub(super) fn toggle_option(&mut self, question: usize, option: usize) {
        let Some(Ask {
            kind: AskKind::Questions { asked, picked, .. },
            ..
        }) = self.asks.front_mut()
        else {
            return;
        };
        let multi = asked
            .questions
            .get(question)
            .is_some_and(|q| q.multi_select);
        let Some(picks) = picked.get_mut(question) else {
            return;
        };
        let Some(was) = picks.get(option).copied() else {
            return;
        };
        if !multi {
            picks.iter_mut().for_each(|pick| *pick = false);
        }
        if let Some(pick) = picks.get_mut(option) {
            *pick = !was;
        }
        self.redraw.prompt = true;
    }

    pub(super) fn set_other(&mut self, question: usize, text: String) {
        if let Some(Ask {
            kind: AskKind::Questions { other, .. },
            ..
        }) = self.asks.front_mut()
            && let Some(slot) = other.get_mut(question)
        {
            *slot = text;
            // Writing something answers the question: its tab is marked.
            self.redraw.prompt = true;
        }
    }

    pub(super) fn set_notes(&mut self, question: usize, text: String) {
        if let Some(Ask {
            kind: AskKind::Questions { notes, .. },
            ..
        }) = self.asks.front_mut()
            && let Some(slot) = notes.get_mut(question)
        {
            *slot = text;
        }
    }

    /// Sends the answers to Claude's questions: the labels picked, or what was typed; with the
    /// notes written, and the preview of the option picked.
    pub(super) fn answer_questions(&mut self, request: &RequestId) {
        let Some(ask) = self.take_ask(request) else {
            return;
        };
        let updated_input = match &ask.kind {
            AskKind::Questions {
                asked,
                picked,
                other,
                notes,
                ..
            } => ToolInput::Answered(answered(asked, picked, other, notes)),
            AskKind::Tool { .. } | AskKind::Plan { .. } | AskKind::Acp { .. } => {
                match ask.kind.as_asked() {
                    Some(input) => input,
                    None => {
                        tracing::warn!(%request, "questions answered for an agent's call");
                        self.asks.push_front(ask);
                        return;
                    }
                }
            }
        };
        let response = PermissionResult::Allow {
            updated_input,
            updated_permissions: Vec::new(),
        };
        self.answer(&ask, response, ToolState::Running);
    }

    pub(super) fn edit_comment(&mut self, section: Option<usize>) {
        if let Some(Ask {
            kind: AskKind::Plan { parts, editing, .. },
            ..
        }) = self.asks.front_mut()
        {
            *editing = section.filter(|&section| section < parts.len());
            self.redraw.prompt = true;
        }
    }

    pub(super) fn set_comment(&mut self, section: usize, text: &str) {
        if let Some(Ask {
            kind: AskKind::Plan { parts, editing, .. },
            ..
        }) = self.asks.front_mut()
        {
            if let Some(part) = parts.get_mut(section) {
                text.trim().clone_into(&mut part.comment);
            }
            *editing = None;
            self.redraw.prompt = true;
        }
    }

    pub(super) fn answer(&mut self, ask: &Ask, response: PermissionResult, state: ToolState) {
        self.write(|process| process.answer(&ask.request, response));
        self.answered(ask, state);
    }

    /// Answers an ACP agent's request with one of its choices; with none of the kind wanted
    /// among them, the request is turned down, which the agent takes as cancelled.
    pub(super) fn choose(&mut self, ask: &Ask, chosen: Option<call::OptionId>, state: ToolState) {
        match chosen {
            Some(option) => self.write(|process| process.choose(&ask.request, &option)),
            None => self
                .write(|process| process.refuse(&ask.request, "the agent offered no such choice")),
        }
        self.answered(ask, state);
    }

    pub(super) fn answered(&mut self, ask: &Ask, state: ToolState) {
        tracing::info!(request = %ask.request, ?state, "answered a permission request");
        if let Some(id) = &ask.tool_use {
            self.chat.set_tool_state(id, state);
        }
        self.status.doing = Doing::Working;
        self.redraw.prompt = true;
        self.redraw.menus = true;
        // The prompt (and the field in it that had the focus) is gone: the writing goes on in
        // the composer, unless another request waits.
        if self.asks.is_empty() {
            self.view.focus_composer();
        }
    }

    /// A permission request from the CLI, waiting for the user.
    pub(super) fn ask(
        &mut self,
        request: RequestId,
        tool: ToolName,
        input: Value,
        suggestions: Vec<Value>,
        description: String,
        tool_use: Option<ToolUseId>,
    ) {
        tracing::info!(%tool, "claude asks for permission");
        if let Some(id) = &tool_use {
            self.chat.show_asked(id, &tool, &input);
            self.chat.set_tool_state(id, ToolState::Awaiting);
        }
        // Questions and plans have their own prompts; one that doesn't parse is asked about as
        // any tool is.
        let special = match &tool {
            ToolName::AskUserQuestion => questions_asked(&input),
            ToolName::ExitPlanMode => plan_offered(&input),
            ToolName::Bash
            | ToolName::BashOutput
            | ToolName::KillShell
            | ToolName::Monitor
            | ToolName::Read
            | ToolName::NotebookRead
            | ToolName::Write
            | ToolName::Edit
            | ToolName::MultiEdit
            | ToolName::NotebookEdit
            | ToolName::Glob
            | ToolName::Grep
            | ToolName::Ls
            | ToolName::WebFetch
            | ToolName::WebSearch
            | ToolName::Task
            | ToolName::Agent
            | ToolName::Skill
            | ToolName::TodoWrite
            | ToolName::SendMessage
            | ToolName::EdualcSendMessage
            | ToolName::EdualcListSessions
            | ToolName::EdualcIncomingMessage
            | ToolName::Other(_) => None,
        };
        let suggestions = suggestions.into_iter().map(wire::Offered::read).collect();
        let kind = special.unwrap_or(AskKind::Tool {
            tool,
            input,
            suggestions,
            description,
        });
        self.asks.push_back(Ask {
            request,
            tool_use,
            kind,
        });
        self.redraw.prompt = true;
    }

    /// An ACP agent asks whether its call may go ahead, offering its choices.
    pub(super) fn ask_acp(
        &mut self,
        request: RequestId,
        tool_use: ToolUseId,
        call: AcpCall,
        options: Vec<AcpOption>,
    ) {
        tracing::info!(title = %call.title, "the agent asks for permission");
        self.chat.acp_call(&tool_use, &call, None);
        self.chat.set_tool_state(&tool_use, ToolState::Awaiting);
        self.asks.push_back(Ask {
            request,
            tool_use: Some(tool_use),
            kind: AskKind::Acp { call, options },
        });
        self.redraw.prompt = true;
    }

    /// The process no longer waits for this request (the turn was interrupted).
    pub(super) fn cancel(&mut self, request: &RequestId) {
        if let Some(at) = self.asks.iter().position(|ask| &ask.request == request)
            && let Some(ask) = self.asks.remove(at)
            && let Some(id) = &ask.tool_use
        {
            self.chat.set_tool_state(id, ToolState::Cancelled);
        }
        self.redraw.prompt = true;
    }

    /// The process ended without being asked to: says why, as far as it said.
    pub(super) fn ended(&mut self, code: Option<i32>) {
        self.process = None;
        let turns = self.status.turns;
        self.forget_process();
        self.chat.end_turn();
        if turns > 0 || code != Some(0) {
            let how = match code {
                Some(code) => format!(" (exit code {code})"),
                None => " (killed)".to_owned(),
            };
            let mut lines = vec![format!("{} stopped{how}.", self.status.backend.program())];
            lines.extend(self.log.iter().cloned());
            lines.push("Sending a message starts it again.".to_owned());
            self.chat.error(lines.join("\n"));
        }
    }
}

/// Claude's questions with the user's answers (the labels picked, or what was typed), the notes
/// written, and the preview of the option picked, by question.
pub(super) fn answered(
    asked: &wire::Questions,
    picked: &[Vec<bool>],
    other: &[String],
    notes: &[String],
) -> wire::Questions {
    let mut answered = asked.clone();
    for (index, question) in asked.questions.iter().enumerate() {
        let typed = other.get(index).map_or("", |text| text.trim());
        let chosen: Vec<&wire::QuestionOption> = question
            .options
            .iter()
            .zip(picked.get(index).into_iter().flatten())
            .filter(|(_, picked)| **picked)
            .map(|(option, _)| option)
            .collect();
        let answer = if typed.is_empty() {
            chosen
                .iter()
                .map(|option| option.label.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            typed.to_owned()
        };
        if !answer.is_empty() {
            answered.answers.insert(question.question.clone(), answer);
        }
        let written = notes.get(index).map_or("", |text| text.trim());
        let annotation = wire::Annotation {
            notes: (!written.is_empty()).then(|| written.to_owned()),
            // A preview goes with a single option picked, not with typed text.
            preview: match (typed.is_empty(), chosen.as_slice()) {
                (true, [option]) if !option.preview.is_empty() => Some(option.preview.clone()),
                _ => None,
            },
        };
        if annotation != wire::Annotation::default() {
            answered
                .annotations
                .insert(question.question.clone(), annotation);
        }
    }
    answered
}

pub(super) fn questions_asked(input: &Value) -> Option<AskKind> {
    match wire::Questions::deserialize(input) {
        Ok(asked) => Some(AskKind::Questions {
            input: input.clone(),
            picked: asked
                .questions
                .iter()
                .map(|question| vec![false; question.options.len()])
                .collect(),
            other: vec![String::new(); asked.questions.len()],
            notes: vec![String::new(); asked.questions.len()],
            asked,
        }),
        Err(error) => {
            tracing::warn!(%error, "questions that don't parse");
            None
        }
    }
}

pub(super) fn plan_offered(input: &Value) -> Option<AskKind> {
    match wire::PlanInput::deserialize(input) {
        Ok(input) => Some(AskKind::Plan {
            parts: plan::parts(&input.plan),
            input,
            editing: None,
        }),
        Err(error) => {
            tracing::warn!(%error, "a plan that doesn't parse");
            None
        }
    }
}

/// Why a control request of ours brought no answer to take.
#[derive(Debug, thiserror::Error)]
pub enum AnswerError {
    #[error("{0}")]
    Refused(String),
    #[error(
        "Claude Code answered in a form this edualc doesn't know, so its version and edualc's may \
         not fit together ({0})"
    )]
    Unreadable(#[source] serde_json::Error),
}

/// The answer to a control request of ours, as `T`.
pub fn answer_as<T: serde::de::DeserializeOwned>(
    outcome: Result<Value, String>,
) -> Result<T, AnswerError> {
    let answer = outcome.map_err(AnswerError::Refused)?;
    T::deserialize(answer).map_err(AnswerError::Unreadable)
}
