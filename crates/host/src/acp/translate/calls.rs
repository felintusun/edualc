//! Tool calls: kept until they have their arguments, then shown as the window knows them.

use serde_json::{Map, Value};

use crate::acp::Agent;
use crate::agent::Event;
use crate::call::{AcpCall, AcpKind, AcpStatus, Call, Diff};
use crate::edualc_tool::EdualcTool;
use crate::external::acp::{
    self, AgyArguments, Chunk, OpencodeArguments, OpencodeQuestionOption, ToolCall, ToolContent,
    ToolStatus,
};
use crate::external::tool_input::{AgentInput, KnownCall};
use crate::external::wire::{
    ApiMessage, ContentBlock, DENIED, Question, QuestionOption, Questions, ToolName, Usage,
    UserContent, UserLine,
};
use crate::ids::{MessageId, ToolUseId};

use super::{CallId, Effect, Translator, plan_todos};

/// A tool call the agent announced, kept until it has its arguments and then shown.
#[derive(Debug, Clone)]
pub(super) enum Entry {
    /// Waits for the call's arguments.
    Kept(ToolCall),
    Shown(Shown),
}

/// A tool call as the window was shown it.
#[derive(Debug, Clone)]
pub(super) struct Shown {
    call: ToolCall,
    pub(super) drawn: Call,
    pub(super) done: bool,
    pub(super) denied: bool,
    /// What edualc's tool answered a call of it. The agent tells nothing of an MCP tool's
    /// result.
    told: Option<String>,
}

impl Translator {
    /// Sends a message from the agent's messaging tool without asking, as a Claude Code
    /// session's own messages go.
    pub fn tool_called(&mut self, call: CallId, to: String, message: String) -> Vec<Effect> {
        if let Some(id) = self.open_call(&ToolName::Edualc(EdualcTool::SendMessage)) {
            self.calls.insert(call, id);
        }
        vec![Effect::Send { call, to, message }]
    }

    /// Lists the running sessions for the agent's messaging tool, without asking.
    pub fn listing_asked(&mut self, call: CallId) -> Vec<Effect> {
        if let Some(id) = self.open_call(&ToolName::Edualc(EdualcTool::ListSessions)) {
            self.calls.insert(call, id);
        }
        vec![Effect::List(call)]
    }

    /// Returns the agent's id for the call of `tool` under way that has no answer yet.
    fn open_call(&self, tool: &ToolName) -> Option<String> {
        let told: Vec<&String> = self.calls.values().collect();
        self.entries.iter().find_map(|(id, entry)| match entry {
            Entry::Shown(Shown {
                drawn: Call::Named { name, .. },
                done: false,
                ..
            }) if name == tool && !told.contains(&id) => Some(id.clone()),
            Entry::Kept(_) | Entry::Shown(_) => None,
        })
    }

    /// Keeps what edualc's tool answered `call`, shown as its result once the agent says it
    /// ended.
    pub fn tool_told(&mut self, call: CallId, answer: &crate::acp::tool::Answer) {
        if let Some(Entry::Shown(shown)) = self
            .calls
            .remove(&call)
            .and_then(|id| self.entries.get_mut(&id))
        {
            shown.told = Some(answer.text.clone());
        }
    }

    pub(super) fn tool_call(&mut self, call: &ToolCall) -> Vec<Effect> {
        match self.entries.get(&call.tool_call_id) {
            Some(Entry::Kept(kept)) => {
                let merged = merged(kept, call);
                return self.call_updated(merged);
            }
            Some(Entry::Shown(_)) => return self.tool_call_update(call),
            None => {}
        }
        self.call_updated(call.clone())
    }

    /// Takes a tool call's fields, kept until it has its arguments or is done, then shown.
    fn call_updated(&mut self, call: ToolCall) -> Vec<Effect> {
        if has_arguments(self.agent, &call) || is_done(&call) {
            self.show_call(&call)
        } else {
            self.entries
                .insert(call.tool_call_id.clone(), Entry::Kept(call));
            Vec::new()
        }
    }

    pub(super) fn show_call(&mut self, call: &ToolCall) -> Vec<Effect> {
        let drawn = drawn(self.agent, call);
        self.entries.insert(
            call.tool_call_id.clone(),
            Entry::Shown(Shown {
                call: call.clone(),
                drawn: drawn.clone(),
                done: false,
                denied: false,
                told: None,
            }),
        );
        let id = ToolUseId::from(call.tool_call_id.clone());
        let mut effects = match drawn {
            Call::Named { name, input } => {
                self.message_done(Some(ContentBlock::ToolUse { id, name, input }))
            }
            Call::Acp(call) => {
                let mut effects = self.message_done(None);
                effects.push(Effect::ToWindow(Event::ToolCall {
                    id,
                    call,
                    parent_tool_use_id: None,
                }));
                effects
            }
        };
        if is_done(call) {
            effects.extend(self.tool_call_update(call));
        }
        effects
    }

    pub(super) fn tool_call_update(&mut self, call: &ToolCall) -> Vec<Effect> {
        if let Some(Entry::Kept(kept)) = self.entries.get(&call.tool_call_id) {
            let merged = merged(kept, call);
            return self.call_updated(merged);
        }
        let mut effects = Vec::new();
        let mut updated = None;
        let finished = {
            let Some(Entry::Shown(shown)) = self.entries.get_mut(&call.tool_call_id) else {
                return Vec::new();
            };
            if shown.done {
                return Vec::new();
            }
            shown.call = merged(&shown.call, call);
            // A call shows as it first did: one the window acts on by its name stays one.
            let redrawn = drawn(self.agent, &shown.call);
            let same_tool = match (&redrawn, &shown.drawn) {
                (
                    Call::Named { name, .. },
                    Call::Named {
                        name: shown_name, ..
                    },
                ) => name == shown_name,
                (Call::Acp(_), Call::Acp(_)) => true,
                (Call::Named { .. }, Call::Acp(_)) | (Call::Acp(_), Call::Named { .. }) => false,
            };
            if same_tool && redrawn != shown.drawn {
                shown.drawn = redrawn.clone();
                updated = Some(redrawn);
            }
            let failed = match call.status {
                Some(ToolStatus::Completed) => Some(false),
                Some(ToolStatus::Failed) => Some(true),
                Some(ToolStatus::Pending | ToolStatus::InProgress) | None => None,
            };
            failed.map(|failed| {
                shown.done = true;
                // An ACP agent's call has its result in it, sent as it changed.
                let Call::Named { .. } = shown.drawn else {
                    return None;
                };
                let text = match (&shown.told, shown.denied) {
                    (_, true) => DENIED.to_owned(),
                    (Some(told), false) => told.clone(),
                    (None, false) => result_text(&shown.call, failed),
                };
                Some((text, failed || shown.denied))
            })
        };
        let id = ToolUseId::from(call.tool_call_id.clone());
        match updated {
            Some(Call::Named { name, input }) => {
                let message = ApiMessage {
                    id: MessageId::from(self.local_id("message")),
                    content: vec![ContentBlock::ToolUse { id, name, input }],
                    usage: Usage::default(),
                };
                effects.push(Effect::ToWindow(Event::Assistant {
                    message,
                    parent_tool_use_id: None,
                    error: None,
                }));
            }
            Some(Call::Acp(call)) => effects.push(Effect::ToWindow(Event::ToolCall {
                id,
                call,
                parent_tool_use_id: None,
            })),
            None => {}
        }
        let Some(result) = finished else {
            return effects;
        };
        if let Some((text, is_error)) = result {
            effects.extend(tool_result_effects(
                call.tool_call_id.clone(),
                text,
                is_error,
            ));
        }
        effects.extend(self.subagents());
        self.turn_tools += 1;
        if self.agent.reports_quotas() && self.turn_tools.is_multiple_of(5) {
            effects.push(Effect::Quotas { request: None });
        }
        effects
    }
}

pub(super) fn tool_result_effects(
    tool_use_id: String,
    text: String,
    is_error: bool,
) -> Vec<Effect> {
    let content = UserContent::Blocks(vec![ContentBlock::ToolResult {
        tool_use_id: ToolUseId::from(tool_use_id),
        content: Some(UserContent::Text(text)),
        is_error,
    }]);
    vec![Effect::ToWindow(Event::User(UserLine::of_session(content)))]
}

/// Returns `kept` with each field that `update` has replaced.
pub(super) fn merged(kept: &ToolCall, update: &ToolCall) -> ToolCall {
    let mut call = kept.clone();
    let update = update.clone();
    if update.name.is_some() {
        call.name = update.name;
    }
    if update.title.is_some() {
        call.title = update.title;
    }
    if update.kind.is_some() {
        call.kind = update.kind;
    }
    if update.status.is_some() {
        call.status = update.status;
    }
    if update.raw_input.is_some() {
        call.raw_input = update.raw_input;
    }
    if update.raw_output.is_some() {
        call.raw_output = update.raw_output;
    }
    if update.content.is_some() {
        call.content = update.content;
    }
    if update.locations.is_some() {
        call.locations = update.locations;
    }
    call
}

/// Returns whether a tool call's arguments have come. An argument the agent sends early (see
/// [`Agent::early_arguments`]) doesn't count, and a patch waits for its diff.
fn has_arguments(agent: Agent, call: &ToolCall) -> bool {
    if call
        .name
        .as_deref()
        .is_some_and(|name| matches!(name.to_ascii_lowercase().as_str(), "apply_patch" | "patch"))
        && !call
            .content
            .iter()
            .flatten()
            .any(|part| matches!(part, ToolContent::Diff { .. }))
    {
        return false;
    }
    match &call.raw_input {
        None => false,
        Some(Value::Object(fields)) => fields
            .keys()
            .any(|field| !agent.early_arguments().contains(&field.as_str())),
        Some(_) => true,
    }
}

fn is_done(call: &ToolCall) -> bool {
    matches!(
        call.status,
        Some(ToolStatus::Completed | ToolStatus::Failed)
    )
}

/// Returns a tool call as the window shows it: by the window's name for a call edualc acts on
/// (see [`named`]), else as the agent gives it.
pub(super) fn drawn(agent: Agent, call: &ToolCall) -> Call {
    match named(agent, call) {
        Some((name, input)) => Call::Named { name, input },
        None => Call::Acp(acp_call(call)),
    }
}

/// Returns the window's name and input for a call edualc acts on: a question for you, a to-do
/// list, a subagent whose steps nest under it, or edualc's messaging tools.
pub(in crate::acp) fn named(agent: Agent, call: &ToolCall) -> Option<(ToolName, Value)> {
    let input = call.raw_input.clone().unwrap_or(Value::Null);
    if let Some(name) = normalized_tool_name(call)
        && let Some(mapped) = extra_tool(agent, &name, call, &input)
    {
        return Some(mapped);
    }
    let known = call
        .name
        .as_deref()
        .and_then(|name| agent.tool(name, call))?;
    Some(as_known(&known, input))
}

/// Returns the call as the agent gives it, titled with the agent's words for it, else the
/// tool's name.
fn acp_call(call: &ToolCall) -> AcpCall {
    AcpCall {
        title: call
            .title
            .clone()
            .or_else(|| call.name.clone())
            .unwrap_or_else(|| "Tool".to_owned()),
        kind: call.kind.map(AcpKind::from),
        locations: call
            .locations
            .iter()
            .flatten()
            .map(|location| location.path.clone())
            .collect(),
        diffs: call
            .content
            .iter()
            .flatten()
            .filter_map(|part| match part {
                ToolContent::Diff {
                    path,
                    old_text,
                    new_text,
                } => Some(Diff {
                    path: path.clone(),
                    old_text: old_text.clone(),
                    new_text: new_text.clone(),
                }),
                ToolContent::Content { .. } | ToolContent::Other => None,
            })
            .collect(),
        input: call.raw_input.clone(),
        status: call.status.map_or(AcpStatus::Pending, AcpStatus::from),
        output: output_of(call),
    }
}

fn normalized_tool_name(call: &ToolCall) -> Option<String> {
    if let Some(name) = &call.name {
        return Some(name.to_ascii_lowercase());
    }
    let title = call.title.as_deref()?.trim();
    (!title.is_empty() && !title.contains(char::is_whitespace) && !title.contains('/'))
        .then(|| title.to_ascii_lowercase())
}

fn parse_args<Arguments: serde::de::DeserializeOwned + Default>(call: &ToolCall) -> Arguments {
    let Some(input) = call.raw_input.clone() else {
        return Arguments::default();
    };
    serde_json::from_value::<Option<Arguments>>(input)
        .ok()
        .flatten()
        .unwrap_or_default()
}

fn encode_tool(name: ToolName, value: &impl serde::Serialize, raw: &Value) -> (ToolName, Value) {
    let encoded = serde_json::to_value(value).unwrap_or_else(|error| {
        tracing::warn!(%error, tool = %name, "a tool's input that can't be written");
        raw.clone()
    });
    (name, encoded)
}

fn extra_tool(agent: Agent, name: &str, call: &ToolCall, raw: &Value) -> Option<(ToolName, Value)> {
    match agent {
        Agent::Agy => agy_extra_tool(name, call, raw),
        Agent::Opencode => opencode_extra_tool(name, call, raw),
    }
}

fn agy_extra_tool(name: &str, call: &ToolCall, raw: &Value) -> Option<(ToolName, Value)> {
    let given: AgyArguments = parse_args(call);
    let title = || call.title.clone().unwrap_or_default();
    match name {
        "invoke_subagent" => Some(as_known(
            &KnownCall::Agent(AgentInput {
                description: given.subagent_name.clone().unwrap_or_else(title),
                prompt: given.prompt.unwrap_or_default(),
                subagent_type: given.subagent_name,
            }),
            raw.clone(),
        )),
        _ => None,
    }
}

fn opencode_extra_tool(name: &str, call: &ToolCall, raw: &Value) -> Option<(ToolName, Value)> {
    let given: OpencodeArguments = parse_args(call);
    let title = || call.title.clone().unwrap_or_default();
    match name {
        "todowrite" | "todo_write" => Some(encode_tool(
            ToolName::TodoWrite,
            &plan_todos(&given.todos),
            raw,
        )),
        "task" | "agent" => Some(as_known(
            &KnownCall::Agent(AgentInput {
                description: given.description.unwrap_or_else(title),
                prompt: given.prompt.unwrap_or_default(),
                subagent_type: given.subagent_type,
            }),
            raw.clone(),
        )),
        "question" => {
            let questions = Questions {
                questions: given
                    .questions
                    .into_iter()
                    .map(|asked| Question {
                        question: asked.question,
                        header: asked.header,
                        options: asked
                            .options
                            .into_iter()
                            .map(|option| match option {
                                OpencodeQuestionOption::Detailed { label, description } => {
                                    QuestionOption {
                                        label,
                                        description,
                                        preview: String::new(),
                                        rest: Map::new(),
                                    }
                                }
                                OpencodeQuestionOption::Plain(label) => QuestionOption {
                                    label,
                                    description: String::new(),
                                    preview: String::new(),
                                    rest: Map::new(),
                                },
                            })
                            .collect(),
                        multi_select: asked.multiple,
                        rest: Map::new(),
                    })
                    .collect(),
                answers: std::collections::BTreeMap::new(),
                annotations: std::collections::BTreeMap::new(),
                rest: Map::new(),
            };
            Some(as_known(&KnownCall::Questions(questions), raw.clone()))
        }
        _ => None,
    }
}

/// Returns a known call's name and input for the window. Where the input can't be written (a
/// path that isn't UTF-8), the agent's own input is shown instead.
fn as_known(known: &KnownCall, raw: Value) -> (ToolName, Value) {
    let input = known.input().unwrap_or_else(|error| {
        tracing::warn!(%error, tool = %known.name(), "a tool's input that can't be written");
        raw
    });
    (known.name(), input)
}

/// Returns what a finished tool call gave, as text, or a word for how it ended.
pub(super) fn result_text(call: &ToolCall, failed: bool) -> String {
    output_of(call).unwrap_or_else(|| if failed { "Failed." } else { "Done." }.to_owned())
}

/// Returns what a call printed or said: its text, else the output in its raw result.
fn output_of(call: &ToolCall) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for content in call.content.iter().flatten() {
        match content {
            ToolContent::Content {
                content: Chunk::Text { text },
            } => parts.push(text.clone()),
            // The call's card shows the change itself.
            ToolContent::Diff { .. }
            | ToolContent::Content {
                content: Chunk::Other,
            }
            | ToolContent::Other => {}
        }
    }
    if parts.is_empty()
        && let Some(Ok(printed)) = call
            .raw_output
            .as_ref()
            .map(<acp::PrintedOutput as serde::Deserialize>::deserialize)
    {
        parts.push(printed.output);
    }
    (!parts.is_empty()).then(|| parts.join("\n"))
}
