//! The adapter's core (sans-io): the agent's Agent Client Protocol on one side, Claude Code's
//! stream-json on the other, as the window and the daemon read it. Each line read in says what
//! to write where; the adapter's loop (see [`crate::acp`]) does it.
//!
//! A turn of the agent's becomes what a turn of Claude Code's looks like: an `init`, streamed
//! text and thinking (`stream_event`s), an assistant message for each stretch of it, a tool
//! result for each call, and a `result`. Its tool calls go as the agent gives them
//! ([`Event::ToolCall`]), but for those edualc acts on, which go by the names the window knows
//! (see [`named`]). Permission requests become `can_use_tool` requests, and the window's
//! answers the agent's options. Nothing is kept: the agent keeps its sessions, and lists and
//! loads them again.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::agent::{Command, Event};
use crate::app::Mode;
use crate::call::{AcpCall, AcpKind, AcpOption, AcpStatus, Call, Diff, OptionId};
use crate::external::acp::{
    self, AgentMethod, AgyArguments, AvailableCommand, Change, Chunk, ConfigOption, Incoming,
    Notified, OpencodeArguments, OpencodeQuestionOption, OptionKind, PermissionAsked, PlanEntry,
    PlanStatus, Prompted, RpcError, StopReason, ToolCall, ToolContent, ToolStatus,
};
use crate::external::agy::{self, QuotaWindow};
use crate::external::tool_input::{AgentInput, KnownCall, Todo, TodoStatus, TodoWriteInput};
use crate::external::wire::{
    self, Account, ApiMessage, AssistantError, CommandInfo, ContentBlock, ControlResponse, DENIED,
    Delta, IncomingRequest, Initialized, LimitKind, MessageId, MessageStart, ModeId, ModelInfo,
    ModelUsage, ProgramMode, ProgramModes, Question, QuestionOption, Questions, RequestId,
    ResultKind, Rule, RuleDestination, SessionId, StreamEvent, Suggestion, System, ToolName,
    ToolUseId, TurnResult, Usage, UserContent, UserLine,
};
use crate::settings::PeerMessages;

use super::Agent;
use super::outbox::unwrapped;

/// JSON-RPC's code for a method the receiver doesn't have.
const METHOD_NOT_FOUND: i64 = -32601;

/// A call of edualc's messaging tool, by the connection it came on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CallId(pub u64);

/// How the session starts: afresh, going on with a conversation of the agent's, or forking one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Start {
    New,
    /// The agent's id for the conversation; the window reads its history itself (agy's records).
    Resume(String),
    /// The agent's id for a conversation whose history the window can't read itself
    /// (opencode's): `session/load`'s replay of it is shown.
    ResumeReplay(String),
    /// Forks an existing conversation of the agent's via ACP `session/fork`.
    Fork(String),
}

/// What the agent is given to start edualc's messaging tool: this program, and the socket the
/// tool reaches the adapter at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolServer {
    pub program: PathBuf,
    pub socket: PathBuf,
}

/// What is to be done.
#[derive(Debug)]
pub enum Effect {
    /// A command to the agent.
    ToAgent(acp::ToAgent),
    /// An event to the window, through the daemon.
    ToWindow(Event),
    /// The session is known by this id from now on, the agent's own conversation's: the
    /// session's record names it, for other sessions and the running list.
    Named(SessionId),
    /// A message: carry it out (see [`super::outbox`]) and answer the call.
    Send {
        call: CallId,
        to: String,
        message: String,
    },
    /// The running sessions, for the call.
    List(CallId),
    /// The subagents and swarm agy keeps for conversation `conversation` may have changed: the
    /// window is told their tasks if they did (see [`super::brain`]).
    Subagents { conversation: String },
    /// Ask the agent's plan how much of its quotas is used (see [`Translator::quotas_told`]):
    /// for the window's `get_usage` request, or, with none, for the sidebar's bars.
    Quotas { request: Option<RequestId> },
}

/// Where the connection to the agent is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    /// `initialize` is asked.
    Connecting,
    /// The session is asked for; a session loaded first replays its history, which the window
    /// reads from the agent's records unless it is shown from the replay.
    Opening { loading: bool },
    /// The agent's id for the session.
    Ready(String),
    /// It couldn't be opened; nothing more goes to the agent.
    Failed,
}

/// Why a prompt's turn failed, as the window is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure<'a> {
    /// The agent said why: the window shows its words.
    Said(&'a str),
    /// The agent's sign-in was refused: the window offers to sign in and try again.
    SignIn(&'a str),
}

/// What a request to the agent was for, by its id.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Asked {
    Initialize,
    Open,
    /// The model; for the window's request, if it asked.
    Model {
        request: Option<RequestId>,
        model: String,
    },
    /// One of the agent's modes, for the window's request.
    Mode {
        request: RequestId,
        mode: String,
    },
    Prompt,
}

/// Something to be prompted with once the agent is free.
#[derive(Debug, Clone)]
enum Queued {
    /// The user's prompt, as ACP takes it.
    User(Vec<acp::PromptBlock>),
    /// Another session's message, in its envelope.
    Peer(String),
}

/// A message being streamed: the id it goes under and its blocks so far, the last one open.
#[derive(Debug, Default)]
struct Streaming {
    id: MessageId,
    blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Block {
    Text(String),
    Thinking(String),
}

/// A tool call the agent told: kept until it has its arguments, then shown to the window.
#[derive(Debug, Clone)]
enum Entry {
    /// Not shown yet: the agent hasn't sent the call's arguments.
    Kept(ToolCall),
    /// Shown to the window.
    Shown(Shown),
}

/// A tool call as the window was shown it.
#[derive(Debug, Clone)]
struct Shown {
    call: ToolCall,
    drawn: Call,
    done: bool,
    denied: bool,
    /// What edualc's tool answered, for a call of it: the agent tells nothing of an MCP tool's
    /// result.
    told: Option<String>,
}

/// A permission request of the agent's that the window was asked.
#[derive(Debug, Clone, PartialEq)]
struct Permission {
    id: Value,
    options: Vec<acp::PermissionOption>,
    tool: String,
}

#[derive(Debug)]
pub struct Translator {
    agent: Agent,
    /// The session's id: as the window started it, then the agent's own conversation's once it
    /// is open, as the agent keeps it to list and load it again.
    id: SessionId,
    cwd: PathBuf,
    start: Start,
    tool: ToolServer,
    phase: Phase,
    /// The model asked for, then the one in use.
    model: Option<String>,
    models: Vec<acp::Model>,
    /// When the agent configures models via ACP `configOptions` (e.g. opencode 2.3.0), its id.
    model_config_id: Option<String>,
    /// The agent's modes, as it opened the session with them.
    modes: Vec<acp::SessionMode>,
    /// The mode the agent is in, by its id.
    mode: Option<String>,
    /// When the agent configures modes via ACP `configOptions`, its id.
    mode_config_id: Option<String>,
    /// Slash commands announced via `available_commands_update`.
    commands: Vec<CommandInfo>,
    /// User prompt text accumulated from `user_message_chunk` updates while replaying an external
    /// session via `session/load`.
    replay_user: String,
    next_rpc: u64,
    asked: HashMap<u64, Asked>,
    /// What the window said before the session was open.
    held: Vec<Command>,
    /// The window's `initialize`, answered once the session is open.
    initialize: Option<RequestId>,
    /// A prompt is under way.
    busy: bool,
    queue: VecDeque<Queued>,
    streaming: Option<Streaming>,
    /// The turn's text, for its result.
    said: String,
    next_local: u64,
    entries: HashMap<String, Entry>,
    permissions: HashMap<RequestId, Permission>,
    /// The tool call each of edualc's tool's calls is, by the agent's id.
    calls: HashMap<CallId, String>,
    /// How full the model's context is, once the agent said.
    context: Option<ContextUsed>,
    /// Other sessions' messages waiting to come in, by the request asking the window.
    waiting: HashMap<RequestId, String>,
    /// The senders the user let in for the session, by the names they gave.
    senders_let_in: BTreeSet<String>,
    /// The agent's forms the window was asked as questions, by the request asking.
    forms: HashMap<RequestId, Form>,
    /// Most recent plan quotas reported by the agent, reused when switching models so the header
    /// chip updates immediately without waiting for the next turn.
    last_quotas: Option<agy::Quotas>,
    /// Tool calls finished during the current turn, to trigger periodic mid-turn quota refreshes.
    turn_tools: usize,
}

/// A form of the agent's, asked in the window as Claude Code's questions are.
#[derive(Debug)]
struct Form {
    /// The agent's request, answered with the form filled in.
    id: Value,
    questions: Vec<FormQuestion>,
}

/// A question of a form, and the fields its answer goes to.
#[derive(Debug, PartialEq, Eq)]
struct FormQuestion {
    question: String,
    field: String,
    takes: Takes,
    options: Vec<acp::FormOption>,
    /// The field for an answer in words, where the options don't hold it.
    other: Option<String>,
}

/// What a question's field takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Takes {
    OneOption,
    Options,
    Text,
}

/// How much of the model's context window the session uses, and the window's size, in tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ContextUsed {
    used: u64,
    size: u64,
}

impl Translator {
    pub fn new(
        agent: Agent,
        id: SessionId,
        cwd: PathBuf,
        start: Start,
        model: Option<String>,
        tool: ToolServer,
    ) -> Self {
        Self {
            agent,
            id,
            cwd,
            start,
            tool,
            phase: Phase::Connecting,
            model,
            models: Vec::new(),
            model_config_id: None,
            modes: Vec::new(),
            mode: None,
            mode_config_id: None,
            commands: Vec::new(),
            replay_user: String::new(),
            next_rpc: 0,
            asked: HashMap::new(),
            held: Vec::new(),
            initialize: None,
            busy: false,
            queue: VecDeque::new(),
            streaming: None,
            said: String::new(),
            next_local: 0,
            entries: HashMap::new(),
            permissions: HashMap::new(),
            calls: HashMap::new(),
            context: None,
            waiting: HashMap::new(),
            senders_let_in: BTreeSet::new(),
            forms: HashMap::new(),
            last_quotas: None,
            turn_tools: 0,
        }
    }

    /// What starts it: the agent is asked to initialize.
    pub fn start(&mut self) -> Vec<Effect> {
        let initialize = self.request(
            acp::ClientCall::Initialize(acp::InitializeParams {
                protocol_version: 1,
                client_capabilities: acp::ClientCapabilities {
                    fs: acp::FileCapabilities {
                        read_text_file: false,
                        write_text_file: false,
                    },
                    terminal: false,
                    elicitation: acp::ElicitationCapabilities {
                        form: acp::FormCapabilities {},
                    },
                },
                client_info: acp::ClientInfo {
                    name: "edualc".to_owned(),
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                },
            }),
            Asked::Initialize,
        );
        vec![initialize]
    }

    fn request(&mut self, call: acp::ClientCall, asked: Asked) -> Effect {
        self.next_rpc += 1;
        self.asked.insert(self.next_rpc, asked);
        Effect::ToAgent(acp::ToAgent::Request(acp::Request {
            jsonrpc: acp::JsonRpc::TwoPointZero,
            id: self.next_rpc,
            call,
        }))
    }

    /// The MCP servers the agent starts for the session: edualc's messaging tool.
    fn mcp_servers(&self) -> Vec<acp::McpServer> {
        vec![acp::McpServer {
            name: "edualc".to_owned(),
            command: self.tool.program.clone(),
            args: vec![PathBuf::from(super::TOOL_FLAG), self.tool.socket.clone()],
            env: Vec::new(),
        }]
    }

    /// An answer to a request of the agent's, under the id it came with.
    fn answer(id: Value, reply: acp::Reply<acp::PermissionAnswer>) -> Effect {
        Effect::ToAgent(acp::ToAgent::Answer(acp::Answer {
            jsonrpc: acp::JsonRpc::TwoPointZero,
            id,
            reply,
        }))
    }

    /// The answer to a form of the agent's, under the id it came with.
    fn form_answer(id: Value, answer: acp::ElicitationAnswer) -> Effect {
        Effect::ToAgent(acp::ToAgent::Elicited(acp::Answer {
            jsonrpc: acp::JsonRpc::TwoPointZero,
            id,
            reply: acp::Reply::Result(answer),
        }))
    }

    /// The agent asks the user to fill in a form: the window asks its questions as Claude
    /// Code's AskUserQuestion does, on the tool call the form is for.
    fn asked_form(&mut self, id: Value, form: acp::Elicitation) -> Vec<Effect> {
        let questions = form_questions(&form.requested_schema);
        if form.mode != acp::ElicitationMode::Form || questions.is_empty() {
            tracing::warn!(mode = ?form.mode, message = %form.message, "a form edualc can't ask: declined");
            return vec![Self::form_answer(id, acp::ElicitationAnswer::Decline)];
        }
        let asked = wire::Questions {
            questions: questions
                .iter()
                .map(|question| wire::Question {
                    question: question.question.clone(),
                    header: String::new(),
                    options: question
                        .options
                        .iter()
                        .map(|option| wire::QuestionOption {
                            label: option.title.clone(),
                            description: String::new(),
                            preview: String::new(),
                            rest: Map::new(),
                        })
                        .collect(),
                    multi_select: question.takes == Takes::Options,
                    rest: Map::new(),
                })
                .collect(),
            answers: std::collections::BTreeMap::new(),
            annotations: std::collections::BTreeMap::new(),
            rest: Map::new(),
        };
        let input = match serde_json::to_value(&asked) {
            Ok(input) => input,
            Err(error) => {
                tracing::warn!(%error, "a form's questions can't be written: cancelled");
                return vec![Self::form_answer(id, acp::ElicitationAnswer::Cancel)];
            }
        };
        let request = RequestId::from(self.local_id("form"));
        self.forms.insert(request.clone(), Form { id, questions });
        vec![Effect::ToWindow(Event::ControlRequest {
            request_id: request,
            request: IncomingRequest::CanUseTool {
                tool_name: ToolName::AskUserQuestion,
                input,
                permission_suggestions: Vec::new(),
                description: form.message,
                tool_use_id: form.tool_call_id.map(ToolUseId::from),
            },
        })]
    }

    /// The window's answer to a form's questions: the form filled in, or turned down.
    fn form_answered(form: Form, reply: wire::Reply) -> Effect {
        let answer = match reply {
            wire::Reply::Success {
                response: wire::PermissionResult::Allow { updated_input, .. },
                ..
            } => {
                let given = match updated_input {
                    wire::ToolInput::Answered(questions) => Some(questions.answers),
                    wire::ToolInput::AsAsked(input) => {
                        serde_json::from_value::<wire::GivenAnswers>(input)
                            .inspect_err(|error| {
                                tracing::warn!(%error, "a form's answers edualc can't read");
                            })
                            .ok()
                            .map(|given| given.answers)
                    }
                    wire::ToolInput::Plan(_) => None,
                };
                match given {
                    Some(answers) => acp::ElicitationAnswer::Accept {
                        content: filled_in(&form.questions, &answers),
                    },
                    None => acp::ElicitationAnswer::Cancel,
                }
            }
            wire::Reply::Success {
                response: wire::PermissionResult::Deny { .. },
                ..
            } => acp::ElicitationAnswer::Decline,
            wire::Reply::Error { error, .. } => {
                tracing::warn!(%error, "the window couldn't ask a form's questions: cancelled");
                acp::ElicitationAnswer::Cancel
            }
        };
        Self::form_answer(form.id, answer)
    }

    /// Tells the agent to stop the turn under way.
    fn cancel(session: String) -> Effect {
        Effect::ToAgent(acp::ToAgent::Notice(acp::Notice {
            jsonrpc: acp::JsonRpc::TwoPointZero,
            notice: acp::ClientNotice::Cancel(acp::CancelParams {
                session_id: session,
            }),
        }))
    }

    fn local_id(&mut self, kind: &str) -> String {
        self.next_local += 1;
        format!("{}-{kind}-{}", self.agent, self.next_local)
    }

    fn acp_session(&self) -> Option<&str> {
        match &self.phase {
            Phase::Ready(session) => Some(session),
            Phase::Connecting | Phase::Opening { .. } | Phase::Failed => None,
        }
    }

    /// For an agent that keeps records of the subagents it starts, a look at them: they change
    /// as its tool calls end, and as its turns do.
    fn subagents(&self) -> Option<Effect> {
        self.agent
            .keeps_subagents()
            .then(|| self.acp_session())
            .flatten()
            .map(|session| Effect::Subagents {
                conversation: session.to_owned(),
            })
    }

    /// A line from the agent.
    pub fn agent_said(&mut self, line: &str) -> Vec<Effect> {
        let incoming = match serde_json::from_str::<Incoming>(line) {
            Ok(incoming) => incoming,
            Err(error) => {
                tracing::debug!(%error, "a line from the agent that isn't JSON-RPC");
                return Vec::new();
            }
        };
        match incoming {
            Incoming::Response { id, result, error } => {
                let Some(asked) = id.as_u64().and_then(|id| self.asked.remove(&id)) else {
                    return Vec::new();
                };
                match error {
                    Some(error) => self.refused(&asked, &error),
                    None => self.answered(asked, result.unwrap_or(Value::Null)),
                }
            }
            Incoming::Notification {
                method: AgentMethod::SessionUpdate,
                params,
            } => match serde_json::from_value::<Notified>(params) {
                Ok(notified) => self.updated(notified.update),
                Err(error) => {
                    tracing::debug!(%error, "an update edualc doesn't read");
                    Vec::new()
                }
            },
            Incoming::Request {
                id,
                method: AgentMethod::RequestPermission,
                params,
            } => match serde_json::from_value::<PermissionAsked>(params) {
                Ok(asked) => self.asked_permission(id, asked),
                Err(error) => {
                    tracing::warn!(%error, "a permission request edualc can't read: denied");
                    vec![Self::answer(
                        id,
                        acp::Reply::Result(acp::PermissionAnswer {
                            outcome: acp::Outcome::Cancelled,
                        }),
                    )]
                }
            },
            Incoming::Request {
                id,
                method: AgentMethod::ElicitationCreate,
                params,
            } => match serde_json::from_value::<acp::Elicitation>(params) {
                Ok(form) => self.asked_form(id, form),
                Err(error) => {
                    tracing::warn!(%error, "a form edualc can't read: cancelled");
                    vec![Self::form_answer(id, acp::ElicitationAnswer::Cancel)]
                }
            },
            Incoming::Request { id, method, .. } => {
                tracing::debug!(?method, "a request of the agent's edualc doesn't serve");
                vec![Self::answer(
                    id,
                    acp::Reply::Error(acp::RpcErrorBody {
                        code: METHOD_NOT_FOUND,
                        message: "edualc doesn't serve this".to_owned(),
                    }),
                )]
            }
            Incoming::Notification { method, .. } => {
                tracing::debug!(?method, "a notification edualc doesn't read");
                Vec::new()
            }
        }
    }

    /// The agent's answer to a request.
    fn answered(&mut self, asked: Asked, result: Value) -> Vec<Effect> {
        match asked {
            Asked::Initialize => {
                let (call, loading) = match &self.start {
                    Start::New => (
                        acp::ClientCall::NewSession(acp::NewSessionParams {
                            cwd: self.cwd.clone(),
                            mcp_servers: self.mcp_servers(),
                        }),
                        false,
                    ),
                    Start::Resume(session) | Start::ResumeReplay(session) => (
                        acp::ClientCall::LoadSession(acp::LoadSessionParams {
                            session_id: session.clone(),
                            cwd: self.cwd.clone(),
                            mcp_servers: self.mcp_servers(),
                        }),
                        true,
                    ),
                    Start::Fork(session) => (
                        acp::ClientCall::ForkSession(acp::ForkSessionParams {
                            session_id: session.clone(),
                            cwd: self.cwd.clone(),
                        }),
                        false,
                    ),
                };
                self.phase = Phase::Opening { loading };
                vec![self.request(call, Asked::Open)]
            }
            Asked::Open => {
                let opened: acp::Opened = match serde_json::from_value(result) {
                    Ok(opened) => opened,
                    Err(error) => match &self.start {
                        Start::New | Start::Fork(_) => {
                            return self.failed(
                                &format!(
                                    "its answer to opening the session can't be read: {error}"
                                ),
                                false,
                            );
                        }
                        // The session asked for is open whatever else the answer says.
                        Start::Resume(_) | Start::ResumeReplay(_) => {
                            tracing::warn!(%error, agent = %self.agent, "the answer to loading the session can't be read: its models and modes are left out");
                            acp::Opened::default()
                        }
                    },
                };
                // A load answers without the id: the session is the one asked for.
                let session = match (&self.start, opened.session_id) {
                    (Start::Resume(session) | Start::ResumeReplay(session), _) => session.clone(),
                    (Start::New | Start::Fork(_), Some(session)) => session,
                    (Start::New | Start::Fork(_), None) => {
                        return self.failed(&format!("{} opened no session", self.agent), false);
                    }
                };
                let mut effects = Vec::new();
                if matches!(self.start, Start::ResumeReplay(_)) {
                    effects.extend(self.flush_replay_user());
                    effects.extend(self.message_done(None));
                }
                let named = SessionId::for_external(self.agent.backend(), &session);
                if named != self.id {
                    self.id = named.clone();
                    effects.push(Effect::Named(named));
                }
                if let Some(models) = opened.models {
                    self.models = models.available_models;
                    if self.model.is_none() {
                        self.model = models.current_model_id;
                    }
                }
                if let Some(modes) = opened.modes {
                    self.modes = modes.available_modes;
                    self.mode = modes.current_mode_id;
                }
                if !opened.config_options.is_empty() {
                    self.apply_config_options(&opened.config_options);
                }
                self.phase = Phase::Ready(session.clone());
                if let Some(model) = self.model.clone() {
                    effects.push(self.set_model_request(session, model, None));
                }
                if let Some(request) = self.initialize.take() {
                    effects.push(self.initialized(request));
                }
                for command in std::mem::take(&mut self.held) {
                    effects.extend(self.window_command(command));
                }
                effects.extend(self.subagents());
                effects
            }
            Asked::Model { request, model } => {
                self.model = Some(model);
                let mut effects: Vec<Effect> = request
                    .map(|request| success(request, Value::Object(Map::new())))
                    .into_iter()
                    .collect();
                if let Some(quotas) = &self.last_quotas {
                    let limits = plan_limits(quotas, self.active_model_name());
                    effects.push(Effect::ToWindow(Event::RateLimits(wire::RateLimits {
                        windows: limit_windows(&limits),
                    })));
                } else if self.agent.reports_quotas() {
                    effects.push(Effect::Quotas { request: None });
                }
                effects
            }
            Asked::Mode { request, mode } => {
                self.mode = Some(mode);
                vec![success(request, Value::Object(Map::new()))]
            }
            Asked::Prompt => match serde_json::from_value::<Prompted>(result) {
                Ok(prompted) => self.turn_ended(Some(&prompted), None),
                Err(error) => {
                    let said = format!("its answer to the prompt can't be read: {error}");
                    self.turn_ended(None, Some(Failure::Said(&said)))
                }
            },
        }
    }

    /// The agent refused a request.
    fn refused(&mut self, asked: &Asked, error: &RpcError) -> Vec<Effect> {
        let message = error.message.as_str();
        match asked {
            Asked::Initialize | Asked::Open => {
                let sign_in = self.agent.sign_in_refused(error);
                self.failed(message, sign_in)
            }
            Asked::Model {
                request: Some(request),
                ..
            }
            | Asked::Mode { request, .. } => {
                vec![failure(request.clone(), message)]
            }
            Asked::Model { request: None, .. } => {
                tracing::warn!(message, agent = %self.agent, "the agent didn't take the model");
                Vec::new()
            }
            Asked::Prompt if self.agent.sign_in_refused(error) => {
                self.turn_ended(None, Some(Failure::SignIn(message)))
            }
            Asked::Prompt => self.turn_ended(None, Some(Failure::Said(message))),
        }
    }

    fn failed(&mut self, message: &str, sign_in: bool) -> Vec<Effect> {
        self.phase = Phase::Failed;
        let agent = self.agent;
        let mut effects = vec![Effect::ToWindow(Event::Assistant {
            message: ApiMessage {
                id: MessageId::from(format!("{agent}-failed")),
                content: vec![ContentBlock::Text {
                    text: format!("{agent} couldn't open the session: {message}"),
                }],
                usage: Usage::default(),
            },
            parent_tool_use_id: None,
            error: sign_in.then_some(AssistantError::AuthenticationFailed),
        })];
        if let Some(request) = self.initialize.take() {
            effects.push(failure(request, message));
        }
        effects
    }

    /// The answer to the window's `initialize`: the agent's models, and its own modes with the
    /// one it is in.
    fn initialized(&self, request: RequestId) -> Effect {
        let agent = self.agent;
        let initialized = Initialized {
            models: self
                .models
                .iter()
                .map(|model| ModelInfo {
                    value: model.model_id.clone(),
                    display_name: model.name.clone(),
                    resolved_model: String::new(),
                    description: String::new(),
                })
                .collect(),
            commands: self
                .commands
                .iter()
                .map(|command| CommandInfo {
                    name: command.name.clone(),
                    description: command.description.clone(),
                    argument_hint: command.argument_hint.clone(),
                })
                .collect(),
            account: Account {
                api_provider: agent.to_string(),
                ..Account::default()
            },
            claude_code_version: format!("{agent}, through edualc {}", env!("CARGO_PKG_VERSION")),
            program_modes: Some(ProgramModes {
                offered: self
                    .modes
                    .iter()
                    .map(|mode| ProgramMode {
                        id: ModeId::from(mode.id.clone()),
                        name: mode.name.clone(),
                        description: mode.description.clone().unwrap_or_default(),
                    })
                    .collect(),
                current: self.mode.clone().map(ModeId::from),
            }),
        };
        match serde_json::to_value(initialized) {
            Ok(response) => success(request, response),
            Err(error) => failure(request, &error.to_string()),
        }
    }

    /// A serialized [`Command`] from the window.
    pub fn window_said(&mut self, line: &str) -> Vec<Effect> {
        let command = match serde_json::from_str::<Command>(line) {
            Ok(command) => command,
            Err(error) => return unreadable_from_window(line, &error),
        };
        match &self.phase {
            Phase::Ready(_) => self.window_command(command),
            Phase::Failed => match command {
                Command::ControlRequest { request_id, .. } => vec![failure(
                    request_id,
                    &format!("{} isn't running", self.agent),
                )],
                Command::Prompt { .. }
                | Command::ControlResponse { .. }
                | Command::Choose { .. }
                | Command::Shell { .. } => Vec::new(),
            },
            Phase::Connecting | Phase::Opening { .. } => {
                match command {
                    Command::ControlRequest {
                        request_id,
                        request: wire::ControlRequest::Initialize { .. },
                    } => self.initialize = Some(request_id),
                    held @ (Command::ControlRequest { .. }
                    | Command::Prompt { .. }
                    | Command::ControlResponse { .. }
                    | Command::Choose { .. }
                    | Command::Shell { .. }) => self.held.push(held),
                }
                Vec::new()
            }
        }
    }

    /// A parsed command from the window: a prompt, a `!` command, a request or an answer.
    fn window_command(&mut self, command: Command) -> Vec<Effect> {
        match command {
            Command::ControlRequest {
                request_id,
                request,
            } => self.window_request(request_id, request),
            Command::ControlResponse { response } => self.window_answered(response),
            Command::Prompt { images, text, .. } => {
                let prompt = command_prompt_blocks(&images, &text);
                if prompt.is_empty() {
                    return Vec::new();
                }
                self.queue.push_back(Queued::User(prompt));
                self.next_turn()
            }
            Command::Shell { command } => self.shell_refused(&command),
            Command::Choose { request_id, option } => self.window_chose(&request_id, &option),
        }
    }

    /// The user's choice among the agent's, for its call: an id the agent didn't offer
    /// cancels the request rather than pick another.
    fn window_chose(&mut self, request: &RequestId, option: &OptionId) -> Vec<Effect> {
        let Some(permission) = self.permissions.remove(request) else {
            return Vec::new();
        };
        let chosen = permission
            .options
            .iter()
            .find(|offered| offered.option_id == option.as_str());
        let outcome = match chosen {
            Some(chosen) => {
                let rejects = matches!(
                    chosen.kind,
                    OptionKind::RejectOnce | OptionKind::RejectAlways
                );
                if rejects && let Some(Entry::Shown(shown)) = self.entries.get_mut(&permission.tool)
                {
                    shown.denied = true;
                }
                acp::Outcome::Selected {
                    option_id: chosen.option_id.clone(),
                }
            }
            None => {
                tracing::warn!(
                    option = option.as_str(),
                    "a choice the agent didn't offer: cancelled"
                );
                acp::Outcome::Cancelled
            }
        };
        vec![Self::answer(
            permission.id,
            acp::Reply::Result(acp::PermissionAnswer { outcome }),
        )]
    }

    /// A `!` command: the agent has no shell beside the conversation, so it is told as not run,
    /// in the rows shown for one (its echo, then what it printed).
    fn shell_refused(&self, command: &str) -> Vec<Effect> {
        let agent = self.agent;
        let row = |text: String| {
            Effect::ToWindow(Event::User(UserLine::replayed(UserContent::Text(text))))
        };
        vec![
            row(format!("<bash-input>{}</bash-input>", escaped(command))),
            row(format!(
                "<bash-stdout></bash-stdout><bash-stderr>Not run: {agent} sessions don't run ! \
                 commands. Ask {agent} to run it, or run it in a terminal.</bash-stderr>\
                 <bash-exit-code>1</bash-exit-code>"
            )),
        ]
    }

    fn window_request(
        &mut self,
        request_id: RequestId,
        request: wire::ControlRequest,
    ) -> Vec<Effect> {
        let Some(session) = self.acp_session().map(str::to_owned) else {
            return vec![failure(
                request_id,
                &format!("{} isn't running", self.agent),
            )];
        };
        match request {
            wire::ControlRequest::Initialize { .. } => vec![self.initialized(request_id)],
            wire::ControlRequest::Interrupt => {
                let mut effects = vec![success(request_id, Value::Object(Map::new()))];
                if self.busy {
                    effects.push(Self::cancel(session));
                }
                effects
            }
            wire::ControlRequest::SetModel { model } => {
                vec![self.set_model_request(session, model, Some(request_id))]
            }
            wire::ControlRequest::SetProgramMode { mode } => {
                let mode = mode.to_string();
                vec![self.set_mode_request(session, mode, request_id)]
            }
            wire::ControlRequest::SetPermissionMode { .. } => {
                vec![failure(
                    request_id,
                    &format!("{} has modes of its own, not Claude Code's", self.agent),
                )]
            }
            wire::ControlRequest::GetUsage if self.agent.reports_quotas() => {
                vec![Effect::Quotas {
                    request: Some(request_id),
                }]
            }
            wire::ControlRequest::GetContextUsage => match self.context {
                Some(context) => vec![success_as(request_id, &context_report(context))],
                None => vec![failure(
                    request_id,
                    &format!("{} hasn't said how full the context is yet", self.agent),
                )],
            },
            wire::ControlRequest::StopTask { .. }
            | wire::ControlRequest::GetTaskOutput { .. }
            | wire::ControlRequest::SideQuestion { .. }
            | wire::ControlRequest::GetUsage => {
                vec![failure(
                    request_id,
                    &format!("{} doesn't do this", self.agent),
                )]
            }
        }
    }

    /// The window's answer to a permission request: the agent's, or the messaging tool's.
    fn window_answered(&mut self, reply: wire::Reply) -> Vec<Effect> {
        let request = match &reply {
            wire::Reply::Success { request_id, .. } | wire::Reply::Error { request_id, .. } => {
                request_id
            }
        };
        if let Some(form) = self.forms.remove(request) {
            return vec![Self::form_answered(form, reply)];
        }
        if let Some(content) = self.waiting.remove(request) {
            return match reply {
                wire::Reply::Success {
                    response:
                        wire::PermissionResult::Allow {
                            updated_permissions,
                            ..
                        },
                    ..
                } => self.let_in(content, &updated_permissions),
                // Turned down: the agent never sees it, and its turn goes on.
                wire::Reply::Success {
                    response: wire::PermissionResult::Deny { .. },
                    ..
                } => Vec::new(),
                wire::Reply::Error { error, .. } => {
                    tracing::warn!(%error, "the window couldn't ask about a message: turned down");
                    Vec::new()
                }
            };
        }
        let (request_id, verdict) = match reply {
            wire::Reply::Success {
                request_id,
                response:
                    wire::PermissionResult::Allow {
                        updated_permissions,
                        ..
                    },
            } => (
                request_id,
                Verdict::Allow {
                    always: !updated_permissions.is_empty(),
                },
            ),
            wire::Reply::Success {
                request_id,
                response: wire::PermissionResult::Deny { interrupt, .. },
            } => (request_id, Verdict::Deny { interrupt }),
            wire::Reply::Error { request_id, error } => {
                tracing::warn!(%error, "the window couldn't answer a permission request: cancelled");
                (request_id, Verdict::Unanswered)
            }
        };
        let Some(permission) = self.permissions.remove(&request_id) else {
            return Vec::new();
        };
        let outcome = match verdict {
            Verdict::Allow { always: true } => outcome_of_any(
                &permission.options,
                &[OptionKind::AllowAlways, OptionKind::AllowOnce],
            ),
            Verdict::Allow { always: false } => {
                outcome_of(&permission.options, OptionKind::AllowOnce)
            }
            Verdict::Deny { .. } => outcome_of(&permission.options, OptionKind::RejectOnce),
            Verdict::Unanswered => acp::Outcome::Cancelled,
        };
        if let Verdict::Deny { .. } = verdict
            && let Some(Entry::Shown(shown)) = self.entries.get_mut(&permission.tool)
        {
            shown.denied = true;
        }
        let mut effects = vec![Self::answer(
            permission.id,
            acp::Reply::Result(acp::PermissionAnswer { outcome }),
        )];
        if let Verdict::Deny { interrupt: true } = verdict
            && let Some(session) = self.acp_session()
        {
            effects.push(Self::cancel(session.to_owned()));
        }
        effects
    }

    /// The agent's messaging tool was called: the message goes without asking, as a Claude Code
    /// session's own do.
    pub fn tool_called(&mut self, call: CallId, to: String, message: String) -> Vec<Effect> {
        if let Some(id) = self.open_call(&ToolName::EdualcSendMessage) {
            self.calls.insert(call, id);
        }
        vec![Effect::Send { call, to, message }]
    }

    /// The agent's messaging tool asked for the running sessions: no question for that.
    pub fn listing_asked(&mut self, call: CallId) -> Vec<Effect> {
        if let Some(id) = self.open_call(&ToolName::EdualcListSessions) {
            self.calls.insert(call, id);
        }
        vec![Effect::List(call)]
    }

    /// The call of `tool` under way, by the agent's id, that no answer was told for yet.
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

    /// What edualc's tool answered `call`: the call's result, when the agent says it ended.
    pub fn tool_told(&mut self, call: CallId, answer: &super::tool::Answer) {
        if let Some(Entry::Shown(shown)) = self
            .calls
            .remove(&call)
            .and_then(|id| self.entries.get_mut(&id))
        {
            shown.told = Some(answer.text.clone());
        }
    }

    /// Another session's message came into the inbox. One from a sender the user let in, or
    /// any with `peer_messages` letting all in, comes in; another waits while the window asks.
    pub fn peer(&mut self, content: String, peer_messages: PeerMessages) -> Vec<Effect> {
        let (name, body) = unwrapped(&content);
        let let_in = match peer_messages {
            PeerMessages::LetAllIn => true,
            PeerMessages::AskEachSender => name
                .as_ref()
                .is_some_and(|name| self.senders_let_in.contains(name)),
        };
        if let_in {
            return self.came_in(content);
        }
        // A sender without a name can't be let in for later: each of its messages is asked.
        let request = match IncomingRequest::incoming_message(name.as_deref(), body) {
            Ok(request) => request,
            Err(error) => {
                tracing::warn!(%error, "a message waiting to come in can't be asked about: turned down");
                return Vec::new();
            }
        };
        let request_id = RequestId::from(self.local_id("message"));
        self.waiting.insert(request_id.clone(), content);
        vec![Effect::ToWindow(Event::ControlRequest {
            request_id,
            request,
        })]
    }

    /// A message the window was asked about: the user let it in, and maybe its sender.
    fn let_in(&mut self, content: String, offered: &[wire::PermissionUpdate]) -> Vec<Effect> {
        if let Some(sender) = wire::sender_let_in(offered) {
            self.senders_let_in.insert(sender);
        }
        self.came_in(content)
    }

    /// Another session's message, let in: shown as such, and given to the agent as a prompt
    /// once it is free.
    fn came_in(&mut self, content: String) -> Vec<Effect> {
        let (name, body) = unwrapped(&content);
        let mut effects = vec![Effect::ToWindow(Event::User(UserLine::from_peer(
            content.clone(),
            name,
            Some(body),
        )))];
        self.queue.push_back(Queued::Peer(content));
        effects.extend(self.next_turn());
        effects
    }

    /// Prompts the agent with what waits, if it is free.
    fn next_turn(&mut self) -> Vec<Effect> {
        if self.busy {
            return Vec::new();
        }
        let Some(session) = self.acp_session().map(str::to_owned) else {
            return Vec::new();
        };
        let Some(queued) = self.queue.pop_front() else {
            return Vec::new();
        };
        let mut effects = vec![Effect::ToWindow(Event::System(System::Init {
            session_id: self.id.clone(),
            model: self.model.clone().unwrap_or_default(),
            permission_mode: Mode::Unknown(self.mode.clone().unwrap_or_default()),
            cwd: self.cwd.clone(),
        }))];
        let prompt = match queued {
            Queued::User(prompt) => prompt,
            Queued::Peer(text) => vec![acp::PromptBlock::Text { text }],
        };
        self.busy = true;
        self.turn_tools = 0;
        self.said.clear();
        effects.push(self.request(
            acp::ClientCall::Prompt(acp::PromptParams {
                session_id: session,
                prompt,
            }),
            Asked::Prompt,
        ));
        effects
    }

    fn stream_event(event: StreamEvent) -> Effect {
        Effect::ToWindow(Event::Stream {
            event,
            parent_tool_use_id: None,
        })
    }

    /// Requests a model switch using `session/set_config_option` when the agent uses ACP
    /// `configOptions`, or `session/set_model` otherwise.
    fn set_model_request(
        &mut self,
        session: String,
        model: String,
        request: Option<RequestId>,
    ) -> Effect {
        let call = match &self.model_config_id {
            Some(config_id) => acp::ClientCall::SetConfigOption(acp::SetConfigOptionParams {
                session_id: session,
                config_id: config_id.clone(),
                value: model.clone(),
            }),
            None => acp::ClientCall::SetModel(acp::SetModelParams {
                session_id: session,
                model_id: model.clone(),
            }),
        };
        self.request(call, Asked::Model { request, model })
    }

    /// Requests a mode switch using `session/set_config_option` when the agent configures modes
    /// via `configOptions`, or `session/set_mode` otherwise.
    fn set_mode_request(&mut self, session: String, mode: String, request: RequestId) -> Effect {
        let call = match &self.mode_config_id {
            Some(config_id) => acp::ClientCall::SetConfigOption(acp::SetConfigOptionParams {
                session_id: session,
                config_id: config_id.clone(),
                value: mode.clone(),
            }),
            None => acp::ClientCall::SetMode(acp::SetModeParams {
                session_id: session,
                mode_id: mode.clone(),
            }),
        };
        self.request(call, Asked::Mode { request, mode })
    }

    /// Reads models and modes out of ACP `configOptions` when the agent sends them there instead
    /// of `Opened.models` / `Opened.modes`. Returns the new mode if the current mode changed.
    fn apply_config_options(&mut self, options: &[ConfigOption]) -> Option<String> {
        let mut mode_changed = None;
        for option in options {
            let category = option.category.as_deref();
            if category == Some("model") || (category.is_none() && option.id == "model") {
                self.model_config_id = Some(option.id.clone());
                let models: Vec<acp::Model> = option
                    .choices()
                    .into_iter()
                    .map(|choice| acp::Model {
                        model_id: choice.value.clone(),
                        name: if choice.name.is_empty() {
                            choice.value.clone()
                        } else {
                            choice.name.clone()
                        },
                    })
                    .collect();
                if !models.is_empty() {
                    self.models = models;
                }
                if self.model.is_none() {
                    self.model.clone_from(&option.current_value);
                }
            } else if category == Some("mode") || (category.is_none() && option.id == "mode") {
                self.mode_config_id = Some(option.id.clone());
                let modes: Vec<acp::SessionMode> = option
                    .choices()
                    .into_iter()
                    .map(|choice| acp::SessionMode {
                        id: choice.value.clone(),
                        name: if choice.name.is_empty() {
                            choice.value.clone()
                        } else {
                            choice.name.clone()
                        },
                        description: choice.description.clone(),
                    })
                    .collect();
                if !modes.is_empty() {
                    self.modes = modes;
                }
                if let Some(current) = &option.current_value
                    && self.mode.as_ref() != Some(current)
                {
                    self.mode = Some(current.clone());
                    mode_changed = Some(current.clone());
                }
            }
        }
        mode_changed
    }

    fn flush_replay_user(&mut self) -> Vec<Effect> {
        if self.replay_user.is_empty() {
            return Vec::new();
        }
        let text = std::mem::take(&mut self.replay_user);
        vec![Effect::ToWindow(Event::User(UserLine::replayed(
            UserContent::Text(text),
        )))]
    }

    /// What changed in the session.
    fn updated(&mut self, update: Change) -> Vec<Effect> {
        match update {
            // Context usage, commands, modes, config options, and session title still apply
            // while a loaded session replays its history.
            Change::UsageUpdate { used, size } => {
                self.context = Some(ContextUsed { used, size });
                return vec![Effect::ToWindow(Event::System(System::ContextUsed {
                    used,
                    size,
                }))];
            }
            Change::AvailableCommandsUpdate { available_commands } => {
                self.commands = available_commands.into_iter().map(command_info).collect();
                if matches!(self.phase, Phase::Ready(_)) {
                    return vec![Effect::ToWindow(Event::System(System::Commands {
                        commands: self.commands.clone(),
                    }))];
                }
                return Vec::new();
            }
            Change::ConfigOptionUpdate { config_options } => {
                let changed = self.apply_config_options(&config_options);
                return if matches!(self.phase, Phase::Ready(_))
                    && let Some(mode) = changed
                {
                    vec![Effect::ToWindow(Event::System(System::ProgramMode {
                        mode: ModeId::from(mode),
                    }))]
                } else {
                    Vec::new()
                };
            }
            Change::CurrentModeUpdate { current_mode_id } => {
                self.mode = Some(current_mode_id.clone());
                return if matches!(self.phase, Phase::Ready(_)) {
                    vec![Effect::ToWindow(Event::System(System::ProgramMode {
                        mode: ModeId::from(current_mode_id),
                    }))]
                } else {
                    Vec::new()
                };
            }
            // The agent keeps the title; `session/list` gives it.
            Change::SessionInfoUpdate { .. } => return Vec::new(),
            Change::AgentMessageChunk { .. }
            | Change::AgentThoughtChunk { .. }
            | Change::UserMessageChunk { .. }
            | Change::ToolCall(_)
            | Change::ToolCallUpdate(_)
            | Change::Plan { .. }
            | Change::Other => {}
        }
        let replaying = matches!(self.phase, Phase::Opening { loading: true });
        if replaying && !matches!(self.start, Start::ResumeReplay(_)) {
            // A loaded session's history: the window reads it from the agent's records.
            return Vec::new();
        }
        match update {
            Change::UserMessageChunk {
                content: Some(Chunk::Text { text }),
            } if replaying => {
                let effects = self.message_done(None);
                self.replay_user.push_str(&text);
                effects
            }
            Change::AgentMessageChunk {
                content: Chunk::Text { text },
            } => {
                let mut effects = if replaying {
                    self.flush_replay_user()
                } else {
                    Vec::new()
                };
                self.said.push_str(&text);
                effects.extend(self.streamed(&Block::Text(text)));
                effects
            }
            Change::AgentThoughtChunk {
                content: Chunk::Text { text },
            } => {
                let mut effects = if replaying {
                    self.flush_replay_user()
                } else {
                    Vec::new()
                };
                effects.extend(self.streamed(&Block::Thinking(text)));
                effects
            }
            Change::ToolCall(call) => {
                let mut effects = if replaying {
                    self.flush_replay_user()
                } else {
                    Vec::new()
                };
                effects.extend(self.tool_call(&call));
                effects
            }
            Change::ToolCallUpdate(call) => self.tool_call_update(&call),
            Change::Plan { entries } => self.plan_updated(&entries),
            Change::AgentMessageChunk { .. }
            | Change::AgentThoughtChunk { .. }
            | Change::UserMessageChunk { .. }
            | Change::SessionInfoUpdate { .. }
            | Change::CurrentModeUpdate { .. }
            | Change::AvailableCommandsUpdate { .. }
            | Change::ConfigOptionUpdate { .. }
            | Change::UsageUpdate { .. }
            | Change::Other => Vec::new(),
        }
    }

    fn plan_updated(&mut self, entries: &[PlanEntry]) -> Vec<Effect> {
        if entries.is_empty() {
            return Vec::new();
        }
        let id = self.local_id("plan");
        let input = serde_json::to_value(plan_todos(entries)).unwrap_or(Value::Null);
        let tool_use = ContentBlock::ToolUse {
            id: ToolUseId::from(id.clone()),
            name: ToolName::TodoWrite,
            input,
        };
        let mut effects = self.message_done(Some(tool_use));
        effects.extend(tool_result_effects(id, "Updated.".to_owned(), false));
        effects
    }

    /// A piece of text or thinking, streamed into the message under way.
    fn streamed(&mut self, piece: &Block) -> Vec<Effect> {
        let mut effects = Vec::new();
        if self.streaming.is_none() {
            let id = MessageId::from(self.local_id("message"));
            effects.push(Self::stream_event(StreamEvent::MessageStart {
                message: MessageStart { id: id.clone() },
            }));
            self.streaming = Some(Streaming {
                id,
                blocks: Vec::new(),
            });
        }
        let Some(streaming) = self.streaming.as_mut() else {
            return effects;
        };
        let same_kind = matches!(
            (streaming.blocks.last(), piece),
            (Some(Block::Text(_)), Block::Text(_)) | (Some(Block::Thinking(_)), Block::Thinking(_))
        );
        let mut events = Vec::new();
        if !same_kind {
            if let Some(open) = open_index(&streaming.blocks) {
                events.push(StreamEvent::ContentBlockStop { index: open });
            }
            let (block, content_block) = match piece {
                Block::Text(_) => (
                    Block::Text(String::new()),
                    ContentBlock::Text {
                        text: String::new(),
                    },
                ),
                Block::Thinking(_) => (
                    Block::Thinking(String::new()),
                    ContentBlock::Thinking {
                        thinking: String::new(),
                    },
                ),
            };
            streaming.blocks.push(block);
            if let Some(index) = open_index(&streaming.blocks) {
                events.push(StreamEvent::ContentBlockStart {
                    index,
                    content_block,
                });
            }
        }
        let Some(index) = open_index(&streaming.blocks) else {
            return effects;
        };
        let delta = match (piece, streaming.blocks.last_mut()) {
            (Block::Text(text), Some(Block::Text(grown))) => {
                grown.push_str(text);
                Delta::Text { text: text.clone() }
            }
            (Block::Thinking(text), Some(Block::Thinking(grown))) => {
                grown.push_str(text);
                Delta::Thinking {
                    thinking: text.clone(),
                }
            }
            (Block::Text(_) | Block::Thinking(_), _) => return effects,
        };
        events.push(StreamEvent::ContentBlockDelta { index, delta });
        effects.extend(events.into_iter().map(Self::stream_event));
        effects
    }

    /// Ends the message being streamed: its blocks, then `tool_use` if a tool call ends it, as
    /// one assistant message.
    fn message_done(&mut self, tool_use: Option<ContentBlock>) -> Vec<Effect> {
        let streaming = self.streaming.take();
        let mut effects = Vec::new();
        let (id, mut content) = match streaming {
            Some(Streaming { id, blocks }) => {
                if let Some(open) = open_index(&blocks) {
                    effects.push(Self::stream_event(StreamEvent::ContentBlockStop {
                        index: open,
                    }));
                }
                let content: Vec<ContentBlock> = blocks
                    .into_iter()
                    .map(|block| match block {
                        Block::Text(text) => ContentBlock::Text { text },
                        Block::Thinking(thinking) => ContentBlock::Thinking { thinking },
                    })
                    .collect();
                (id, content)
            }
            None if tool_use.is_some() => (MessageId::from(self.local_id("message")), Vec::new()),
            None => return effects,
        };
        content.extend(tool_use);
        let message = ApiMessage {
            id,
            content,
            usage: Usage::default(),
        };
        effects.push(Effect::ToWindow(Event::Assistant {
            message,
            parent_tool_use_id: None,
            error: None,
        }));
        effects
    }

    fn tool_call(&mut self, call: &ToolCall) -> Vec<Effect> {
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

    /// A tool call's fields have come: kept until it has its arguments or is done, then shown.
    fn call_updated(&mut self, call: ToolCall) -> Vec<Effect> {
        if has_arguments(self.agent, &call) || is_done(&call) {
            self.show_call(&call)
        } else {
            self.entries
                .insert(call.tool_call_id.clone(), Entry::Kept(call));
            Vec::new()
        }
    }

    fn show_call(&mut self, call: &ToolCall) -> Vec<Effect> {
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

    fn tool_call_update(&mut self, call: &ToolCall) -> Vec<Effect> {
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

    /// The agent asks whether a tool call may go ahead: the window asks the user.
    fn asked_permission(&mut self, id: Value, asked: PermissionAsked) -> Vec<Effect> {
        let tool = asked.tool_call.tool_call_id.clone();
        let mut effects = Vec::new();
        match self.entries.get(&tool) {
            Some(Entry::Kept(kept)) => {
                let call = merged(kept, &asked.tool_call);
                effects.extend(self.show_call(&call));
            }
            Some(Entry::Shown(_)) => {}
            None => effects.extend(self.show_call(&asked.tool_call)),
        }
        let drawn = match self.entries.get(&tool) {
            Some(Entry::Shown(shown)) => shown.drawn.clone(),
            Some(Entry::Kept(_)) | None => drawn(self.agent, &asked.tool_call),
        };
        let (name, input) = match drawn {
            Call::Named { name, input } => (name, input),
            // Asked about as the agent asks: the call, and the agent's choices.
            Call::Acp(call) => {
                let request = RequestId::from(self.local_id("permission"));
                let options = asked.options.iter().cloned().map(AcpOption::from).collect();
                self.permissions.insert(
                    request.clone(),
                    Permission {
                        id,
                        options: asked.options,
                        tool: tool.clone(),
                    },
                );
                effects.push(Effect::ToWindow(Event::PermissionAsked {
                    request_id: request,
                    tool_use_id: ToolUseId::from(tool),
                    call,
                    options,
                }));
                return effects;
            }
        };
        // The agent asks before every MCP tool, in every mode. edualc's own go without asking,
        // as a Claude Code session's messages do.
        if matches!(
            name,
            ToolName::EdualcSendMessage | ToolName::EdualcListSessions
        ) {
            let outcome = outcome_of(&asked.options, OptionKind::AllowOnce);
            effects.push(Self::answer(
                id,
                acp::Reply::Result(acp::PermissionAnswer { outcome }),
            ));
            return effects;
        }
        let permission_suggestions = asked
            .options
            .iter()
            .any(|option| option.kind == OptionKind::AllowAlways)
            .then(|| {
                serde_json::to_value(Suggestion::AddRules {
                    rules: vec![Rule {
                        tool_name: name.to_string(),
                        rule_content: None,
                    }],
                    destination: RuleDestination::Session,
                })
                .ok()
            })
            .flatten()
            .into_iter()
            .collect();
        let request = RequestId::from(self.local_id("permission"));
        self.permissions.insert(
            request.clone(),
            Permission {
                id,
                options: asked.options,
                tool: tool.clone(),
            },
        );
        effects.push(Effect::ToWindow(Event::ControlRequest {
            request_id: request,
            request: IncomingRequest::CanUseTool {
                tool_name: name,
                input,
                permission_suggestions,
                description: asked.tool_call.title.unwrap_or_default(),
                tool_use_id: Some(ToolUseId::from(tool)),
            },
        }));
        effects
    }

    /// The prompt's answer: the turn ends, and what waits goes next. A refused sign-in is told
    /// as an HTTP 401 in the turn's result, which is what it was, and the window offers to sign
    /// in and try again instead of showing the agent's words.
    fn turn_ended(&mut self, prompted: Option<&Prompted>, failure: Option<Failure>) -> Vec<Effect> {
        let mut effects = self.message_done(None);
        // A turn the agent cut short ends as failed, saying why, so it doesn't read as done.
        let failure = failure.or_else(|| {
            prompted
                .and_then(|prompted| cut_short(prompted.stop_reason))
                .map(Failure::Said)
        });
        let error = failure.map(|failure| match failure {
            Failure::Said(message) | Failure::SignIn(message) => message,
        });
        let agent = self.agent;
        if let Some(Failure::Said(error)) = failure {
            let id = MessageId::from(self.local_id("message"));
            let quota_hit = is_quota_exhausted(error);
            let text = if quota_hit {
                format!(
                    "{agent}: {error}\n\nSwitch to another model in the model menu below, or wait \
                     for the quota to reset."
                )
            } else {
                format!("{agent}: {error}")
            };
            effects.push(Effect::ToWindow(Event::Assistant {
                message: ApiMessage {
                    id,
                    content: vec![ContentBlock::Text { text }],
                    usage: Usage::default(),
                },
                parent_tool_use_id: None,
                error: Some(if quota_hit {
                    AssistantError::RateLimit
                } else {
                    AssistantError::Other
                }),
            }));
        }
        // Calls the agent never answered for, or never gave arguments for, are done now.
        let cancelled =
            prompted.is_some_and(|prompted| prompted.stop_reason == StopReason::Cancelled);
        let open: Vec<String> = self
            .entries
            .iter()
            .filter_map(|(id, entry)| match entry {
                Entry::Shown(shown) if !shown.done => Some(id.clone()),
                Entry::Kept(_) => Some(id.clone()),
                Entry::Shown(_) => None,
            })
            .collect();
        for id in open {
            effects.extend(self.tool_call_update(&ToolCall {
                tool_call_id: id,
                status: Some(ToolStatus::Failed),
                ..ToolCall::default()
            }));
        }
        let failed = error.is_some() || cancelled;
        let model = self.model.clone().unwrap_or_else(|| agent.to_string());
        effects.push(Effect::ToWindow(Event::Result(TurnResult {
            subtype: if failed {
                ResultKind::ErrorDuringExecution
            } else {
                ResultKind::Success
            },
            is_error: failed,
            result: Some(std::mem::take(&mut self.said)),
            errors: error
                .map(|error| Value::String(error.to_owned()))
                .into_iter()
                .collect(),
            total_cost_usd: None,
            model_usage: HashMap::from([(
                model,
                ModelUsage {
                    context_window: self.context.map(|context| context.size),
                },
            )]),
            api_error_status: matches!(failure, Some(Failure::SignIn(_))).then_some(401),
            // The agent says how many tokens the turn took, not how long it took.
            duration_ms: None,
            usage: prompted
                .and_then(|prompted| prompted.usage.as_ref())
                .map_or_default(|used| Usage {
                    input_tokens: used.input_tokens,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: used.cached_read_tokens,
                    output_tokens: used.output_tokens,
                }),
        })));
        self.busy = false;
        effects.extend(self.subagents());
        if self.agent.reports_quotas() {
            effects.push(Effect::Quotas { request: None });
        }
        effects.extend(self.next_turn());
        effects
    }
}

impl Translator {
    fn active_model_name(&self) -> Option<&str> {
        self.model.as_deref().and_then(|id| {
            self.models
                .iter()
                .find(|model| model.model_id == id)
                .map(|model| model.name.as_str())
        })
    }

    /// What the agent's plan said of its quotas: the answer to the window's `get_usage`, or the
    /// session's bars from the quotas the active model draws on.
    pub fn quotas_told(
        &mut self,
        request: Option<RequestId>,
        told: Result<agy::Quotas, super::QuotaError>,
    ) -> Vec<Effect> {
        let quotas = match told {
            Ok(quotas) => quotas,
            Err(error) => {
                tracing::warn!(%error, agent = %self.agent, "the plan's quotas couldn't be read");
                return request
                    .map(|request| {
                        failure(
                            request,
                            &format!("{} couldn't say its plan's quotas: {error}", self.agent),
                        )
                    })
                    .into_iter()
                    .collect();
            }
        };
        let limits = plan_limits(&quotas, self.active_model_name());
        self.last_quotas = Some(quotas);
        match request {
            Some(request) => vec![success_as(
                request,
                &wire::UsageReport {
                    rate_limits: wire::RateLimitReport { limits },
                    ..wire::UsageReport::default()
                },
            )],
            None => vec![Effect::ToWindow(Event::RateLimits(wire::RateLimits {
                windows: limit_windows(&limits),
            }))],
        }
    }
}

fn is_quota_exhausted(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("quota reached") || lower.contains("quota exceeded")
}

/// The questions a form asks: one for each field with options, or for text alone. agy 1.5.9
/// puts the field for an answer in words beside each question's as `<field>_other`, which goes
/// with that question rather than being one of its own.
fn form_questions(schema: &acp::FormSchema) -> Vec<FormQuestion> {
    let fields = &schema.properties;
    let other_of = |name: &str| {
        let other = format!("{name}_other");
        fields.contains_key(&other).then_some(other)
    };
    let beside = |name: &str| {
        name.strip_suffix("_other").is_some_and(|base| {
            fields
                .get(base)
                .is_some_and(|field| !options_of(field).1.is_empty())
        })
    };
    fields
        .iter()
        .filter(|(name, _)| !beside(name))
        .filter_map(|(name, field)| {
            let (takes, options) = options_of(field);
            if takes == Takes::Text && field.kind != acp::FieldKind::String {
                tracing::warn!(field = %name, "a form's field edualc doesn't ask: left out");
                return None;
            }
            Some(FormQuestion {
                question: field.title.clone().unwrap_or_else(|| name.clone()),
                field: name.clone(),
                other: (takes != Takes::Text).then(|| other_of(name)).flatten(),
                takes,
                options,
            })
        })
        .collect()
}

/// What a form's field takes, and its options.
fn options_of(field: &acp::FormField) -> (Takes, Vec<acp::FormOption>) {
    if !field.one_of.is_empty() {
        return (Takes::OneOption, field.one_of.clone());
    }
    match &field.items {
        Some(items) if !items.any_of.is_empty() => (Takes::Options, items.any_of.clone()),
        Some(_) | None => (Takes::Text, Vec::new()),
    }
}

/// The form filled in from the window's answers, by question. An answer that is an option's
/// label (or labels, for a question that takes several) is the option's value; anything else
/// was typed, and goes in the question's field for words, or its own.
fn filled_in(
    questions: &[FormQuestion],
    answers: &std::collections::BTreeMap<String, String>,
) -> acp::FormContent {
    let mut content = acp::FormContent::new();
    for question in questions {
        let Some(answer) = answers.get(&question.question) else {
            continue;
        };
        let value_of = |label: &str| {
            question
                .options
                .iter()
                .find(|option| option.title == label)
                .map(|option| option.value.clone())
        };
        let picked = match question.takes {
            Takes::OneOption => value_of(answer).map(acp::FieldValue::Text),
            Takes::Options => answer
                .split(", ")
                .map(value_of)
                .collect::<Option<Vec<String>>>()
                .map(acp::FieldValue::Picked),
            Takes::Text => None,
        };
        match (picked, &question.other) {
            (Some(picked), _) => {
                content.insert(question.field.clone(), picked);
            }
            (None, Some(other)) => {
                content.insert(other.clone(), acp::FieldValue::Text(answer.clone()));
            }
            (None, None) => {
                content.insert(
                    question.field.clone(),
                    acp::FieldValue::Text(answer.clone()),
                );
            }
        }
    }
    content
}

/// Why the agent ended a turn before it was done, in words; none for a turn it finished or the
/// user interrupted.
fn cut_short(reason: StopReason) -> Option<&'static str> {
    match reason {
        StopReason::EndTurn | StopReason::Cancelled => None,
        StopReason::MaxTokens => Some("stopped: the answer reached the most it may write at once"),
        StopReason::MaxTurnRequests => {
            Some("stopped: the turn reached the most requests it may make")
        }
        StopReason::Refusal => Some("refused to go on"),
        StopReason::Other => Some("stopped, for a reason edualc doesn't know"),
    }
}

/// Text as Claude Code puts it in its `<bash-…>` rows: `&`, `<` and `>` as entities.
fn escaped(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The index of the last of `blocks`, the open one, as the stream counts them.
fn open_index(blocks: &[Block]) -> Option<u64> {
    (0_u64..).zip(blocks).last().map(|(index, _)| index)
}

/// A line from the window that isn't one of its lines as edualc reads them. A control request
/// is answered with the reason, so the window doesn't wait on it; anything else is dropped.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandControlPeek {
    #[serde(rename = "ControlRequest")]
    control: CommandControlInner,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandControlInner {
    request_id: RequestId,
    #[serde(rename = "request")]
    _request: Value,
}

fn unreadable_from_window(line: &str, error: &serde_json::Error) -> Vec<Effect> {
    if let Ok(CommandControlPeek { control }) = serde_json::from_str(line) {
        return vec![failure(
            control.request_id,
            &format!("edualc can't read this request: {error}"),
        )];
    }
    tracing::warn!(%error, "a command from the window that can't be read: dropped");
    Vec::new()
}

/// What the window answered a permission request with, as edualc acts on it.
enum Verdict {
    Allow {
        always: bool,
    },
    Deny {
        interrupt: bool,
    },
    /// The window couldn't answer: the request is cancelled.
    Unanswered,
}

fn command_info(command: AvailableCommand) -> CommandInfo {
    CommandInfo {
        name: command.name,
        description: command.description,
        argument_hint: command.input.map_or_default(|input| input.hint),
    }
}

fn plan_todos(entries: &[PlanEntry]) -> TodoWriteInput {
    TodoWriteInput {
        todos: entries
            .iter()
            .map(|entry| Todo {
                content: entry.content.clone(),
                status: match entry.status {
                    PlanStatus::Pending => TodoStatus::Pending,
                    PlanStatus::InProgress => TodoStatus::InProgress,
                    PlanStatus::Completed => TodoStatus::Completed,
                    PlanStatus::Other => TodoStatus::Other,
                },
            })
            .collect(),
    }
}

fn tool_result_effects(tool_use_id: String, text: String, is_error: bool) -> Vec<Effect> {
    let content = UserContent::Blocks(vec![ContentBlock::ToolResult {
        tool_use_id: ToolUseId::from(tool_use_id),
        content: Some(UserContent::Text(text)),
        is_error,
    }]);
    vec![Effect::ToWindow(Event::User(UserLine::of_session(content)))]
}

/// The context's use as `get_context_usage` answers it: the conversation, and what is free.
fn context_report(context: ContextUsed) -> wire::ContextReport {
    let free = context.size.saturating_sub(context.used);
    wire::ContextReport {
        categories: [
            ("Conversation", context.used, wire::ContextKind::Used),
            ("Free space", free, wire::ContextKind::Free),
        ]
        .into_iter()
        .filter(|(_, tokens, _)| *tokens > 0)
        .map(|(name, tokens, kind)| wire::ContextCategory {
            name: name.to_owned(),
            tokens,
            kind,
        })
        .collect(),
        total_tokens: context.used,
        max_tokens: context.size,
    }
}

/// How much of a quota is used, from 0 to 1.
fn quota_used(quota: &agy::Quota) -> f32 {
    (1.0 - quota.remaining_fraction).clamp(0.0, 1.0)
}

/// agy's quotas as the plan's limits, each named by its group: those of the group that model
/// `model` draws on as the session's five hours and week, which the sidebar shows.
pub(crate) fn plan_limits(quotas: &agy::Quotas, model: Option<&str>) -> Vec<wire::PlanLimit> {
    quotas
        .groups
        .iter()
        .flat_map(|group| {
            let drawn_on = model.is_some_and(|model| group.covers(model));
            group.quotas.iter().map(move |quota| {
                let (kind, span) = match (quota.window, drawn_on) {
                    (QuotaWindow::FiveHours, true) => (LimitKind::Session, "5 hours"),
                    (QuotaWindow::Week, true) => (LimitKind::WeeklyAll, "this week"),
                    (QuotaWindow::FiveHours, false) => (LimitKind::Other, "5 hours"),
                    (QuotaWindow::Week, false) => (LimitKind::Other, "this week"),
                    (QuotaWindow::Other, _) => (LimitKind::Other, "another span"),
                };
                wire::PlanLimit {
                    kind,
                    percent: quota_used(quota) * 100.0,
                    resets_at: Some(quota.reset_time),
                    scope: None,
                    label: Some(format!("{}, {span}", group.name)),
                }
            })
        })
        .collect()
}

/// The sidebar's bars from the plan's limits: the session's five hours and its week.
fn limit_windows(limits: &[wire::PlanLimit]) -> wire::LimitWindows {
    let window = |kind| {
        limits
            .iter()
            .find(|limit| limit.kind == kind)
            .map(|limit| wire::LimitWindow {
                utilization: Some(limit.percent / 100.0),
                resets_at: limit.resets_at,
            })
    };
    wire::LimitWindows {
        five_hour: window(LimitKind::Session),
        seven_day: window(LimitKind::WeeklyAll),
    }
}

/// A success answer carrying `response`; a failure if it can't be written.
fn success_as(request: RequestId, response: &impl serde::Serialize) -> Effect {
    match serde_json::to_value(response) {
        Ok(response) => success(request, response),
        Err(error) => failure(request, &format!("the answer can't be written: {error}")),
    }
}

fn success(request: RequestId, response: Value) -> Effect {
    Effect::ToWindow(Event::ControlAnswered(ControlResponse::Success {
        request_id: request,
        response,
    }))
}

fn failure(request: RequestId, error: &str) -> Effect {
    Effect::ToWindow(Event::ControlAnswered(ControlResponse::Error {
        request_id: request,
        error: error.to_owned(),
    }))
}

/// Prompt parts from [`Command::Prompt`], formatted for ACP `session/prompt`.
fn command_prompt_blocks(images: &[wire::ImageBlock], text: &str) -> Vec<acp::PromptBlock> {
    let mut blocks: Vec<acp::PromptBlock> = images
        .iter()
        .map(|image| acp::PromptBlock::Image {
            mime_type: image.media_type(),
            data: image.data().to_owned(),
        })
        .collect();
    if !text.is_empty() {
        blocks.push(acp::PromptBlock::Text {
            text: text.to_owned(),
        });
    }
    blocks
}

/// The outcome of a permission request: the first option of `wanted`'s kind, or none.
fn outcome_of(options: &[acp::PermissionOption], wanted: OptionKind) -> acp::Outcome {
    outcome_of_any(options, &[wanted])
}

/// The first option matching any of `wanted` in preference order, or `Cancelled`.
fn outcome_of_any(options: &[acp::PermissionOption], wanted: &[OptionKind]) -> acp::Outcome {
    wanted
        .iter()
        .find_map(|kind| options.iter().find(|option| option.kind == *kind))
        .map_or(acp::Outcome::Cancelled, |option| acp::Outcome::Selected {
            option_id: option.option_id.clone(),
        })
}

/// The tool call `kept`, with each field `update` has replacing it.
fn merged(kept: &ToolCall, update: &ToolCall) -> ToolCall {
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

/// Whether a tool call's arguments have come: an argument the agent sends before them (see
/// [`Agent::early_arguments`]) doesn't count, and patch tools wait until a diff arrives.
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

/// A tool call as the window shows it: one edualc acts on by the name the window knows it by
/// (see [`named`]), any other as the agent gives it.
pub(super) fn drawn(agent: Agent, call: &ToolCall) -> Call {
    match named(agent, call) {
        Some((name, input)) => Call::Named { name, input },
        None => Call::Acp(acp_call(call)),
    }
}

/// The calls edualc acts on, by the names the window knows them by: a question it asks the
/// user, a to-do list, a subagent whose steps nest under it, edualc's messaging tools.
pub(super) fn named(agent: Agent, call: &ToolCall) -> Option<(ToolName, Value)> {
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

/// The call as the agent gives it, titled by the agent's words for it, else its name for the
/// tool.
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

/// A known call by the window's name and input; one whose input can't be written (a path
/// that isn't UTF-8) shows the agent's own input instead.
fn as_known(known: &KnownCall, raw: Value) -> (ToolName, Value) {
    let input = known.input().unwrap_or_else(|error| {
        tracing::warn!(%error, tool = %known.name(), "a tool's input that can't be written");
        raw
    });
    (known.name(), input)
}

/// What a finished tool call gave, as text.
/// The result of a call the window knows by its name, as Claude Code's tools give one.
fn result_text(call: &ToolCall, failed: bool) -> String {
    output_of(call).unwrap_or_else(|| if failed { "Failed." } else { "Done." }.to_owned())
}

/// What a call printed or said: its text, else the output its raw result gives.
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::app::PermissionMode;
    use crate::call::AcpOptionKind;
    use crate::external::acp::Location;
    use crate::external::wire::{ControlRequest, MediaType, ModeId};

    const ID: &str = "11111111-2222-4333-8444-555555555555";

    /// Serializes a window-bound command cleanly.
    fn said(command: &Command) -> String {
        command.encode().expect("a command the window writes")
    }

    /// A prompt of the window's, of text alone.
    fn prompt(text: &str) -> String {
        said(&Command::Prompt {
            shell: Vec::new(),
            images: Vec::new(),
            text: text.to_owned(),
        })
    }

    /// A control request of the window's.
    fn asks(request_id: &str, request: ControlRequest) -> String {
        said(&Command::ControlRequest {
            request_id: RequestId::from(request_id.to_owned()),
            request,
        })
    }

    fn event_as_json(event: &Event) -> Value {
        match event {
            Event::Stream {
                event,
                parent_tool_use_id,
            } => json!({
                "type": "stream_event",
                "event": event,
                "parent_tool_use_id": parent_tool_use_id,
            }),
            Event::Assistant {
                message,
                parent_tool_use_id,
                error,
            } => json!({
                "type": "assistant",
                "message": message,
                "parent_tool_use_id": parent_tool_use_id,
                "error": error,
            }),
            // edualc's own: Claude Code has no line for a call as the agent gives it.
            Event::ToolCall {
                id,
                call,
                parent_tool_use_id,
            } => json!({
                "type": "tool_call",
                "id": id,
                "call": call,
                "parent_tool_use_id": parent_tool_use_id,
            }),
            Event::PermissionAsked {
                request_id,
                tool_use_id,
                call,
                options,
            } => json!({
                "type": "permission_asked",
                "request_id": request_id,
                "tool_use_id": tool_use_id,
                "call": call,
                "options": options,
            }),
            Event::User(user) => {
                let mut value = serde_json::to_value(user).expect("user");
                if let Value::Object(map) = &mut value {
                    map.insert("type".to_owned(), json!("user"));
                }
                value
            }
            Event::System(system) => {
                let mut value = serde_json::to_value(system).expect("system");
                if let Value::Object(map) = &mut value {
                    map.insert("type".to_owned(), json!("system"));
                }
                value
            }
            Event::Result(result) => {
                let mut value = serde_json::to_value(result).expect("result");
                if let Value::Object(map) = &mut value {
                    map.insert("type".to_owned(), json!("result"));
                }
                value
            }
            Event::RateLimits(rate_limit_info) => json!({
                "type": "rate_limit_event",
                "rate_limit_info": rate_limit_info,
            }),
            Event::ControlRequest {
                request_id,
                request,
            } => json!({
                "type": "control_request",
                "request_id": request_id,
                "request": request,
            }),
            Event::ControlAnswered(response) => json!({
                "type": "control_response",
                "response": response,
            }),
            Event::RequestCancelled { request_id } => json!({
                "type": "control_cancel_request",
                "request_id": request_id,
            }),
        }
    }

    fn translator(start: Start) -> Translator {
        translator_for(Agent::Agy, start)
    }

    fn translator_for(agent: Agent, start: Start) -> Translator {
        Translator::new(
            agent,
            SessionId::try_from(ID.to_owned()).expect("an id"),
            PathBuf::from("/work"),
            start,
            Some("gemini-3.8-flash-high".to_owned()),
            ToolServer {
                program: PathBuf::from("/usr/bin/edualc"),
                socket: PathBuf::from("/run/user/1000/edualc-agy/tool.sock"),
            },
        )
    }

    /// The lines the window is sent, as the JSON it reads.
    fn window_lines(effects: &[Effect]) -> Vec<Value> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::ToWindow(event) => Some(event_as_json(event)),
                Effect::ToAgent(_)
                | Effect::Named(_)
                | Effect::Send { .. }
                | Effect::List(_)
                | Effect::Subagents { .. }
                | Effect::Quotas { .. } => None,
            })
            .collect()
    }

    /// The lines the agent is sent, as the JSON it reads.
    fn agent_lines(effects: &[Effect]) -> Vec<Value> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::ToAgent(line) => Some(line.to_line().expect("a line for the agent")),
                Effect::ToWindow(_)
                | Effect::Named(_)
                | Effect::Send { .. }
                | Effect::List(_)
                | Effect::Subagents { .. }
                | Effect::Quotas { .. } => None,
            })
            .map(|line| serde_json::from_str(&line).expect("a line of JSON"))
            .collect()
    }

    fn kinds(lines: &[Value]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                let kind = line.get("type").and_then(Value::as_str).unwrap_or_default();
                match line.pointer("/event/type").and_then(Value::as_str) {
                    Some(event) => format!("{kind}:{event}"),
                    None => kind.to_owned(),
                }
            })
            .collect()
    }

    /// The id of the last `session/prompt` among `effects`.
    fn prompt_id(effects: &[Effect]) -> u64 {
        agent_lines(effects)
            .into_iter()
            .rev()
            .find(|line| line.get("method") == Some(&json!("session/prompt")))
            .and_then(|line| line.get("id").and_then(Value::as_u64))
            .expect("a prompt")
    }

    /// Panics unless the window reads `line` as a line it knows, down to its blocks and events:
    /// a line it doesn't know would parse as an `Other` and be dropped without a word.
    fn read_by_the_window(line: &Value) {
        use crate::external::wire::{
            ContentBlock, Delta, IncomingRequest, Message, StreamEvent, System, UserContent,
        };
        if line.get("type") == Some(&json!("tool_call")) {
            let call = line.get("call").cloned().expect("a call");
            serde_json::from_value::<AcpCall>(call)
                .unwrap_or_else(|error| panic!("the window can't read {line}: {error}"));
            return;
        }
        if line.get("type") == Some(&json!("permission_asked")) {
            let call = line.get("call").cloned().expect("a call");
            serde_json::from_value::<AcpCall>(call)
                .unwrap_or_else(|error| panic!("the window can't read {line}: {error}"));
            let options = line.get("options").cloned().expect("options");
            serde_json::from_value::<Vec<AcpOption>>(options)
                .unwrap_or_else(|error| panic!("the window can't read {line}: {error}"));
            return;
        }
        let message: Message = serde_json::from_value(line.clone())
            .unwrap_or_else(|error| panic!("the window can't read {line}: {error}"));
        let blocks_known = |blocks: &[ContentBlock]| {
            assert!(
                !blocks
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Other)),
                "a block the window doesn't know in {line}"
            );
        };
        match &message {
            Message::Assistant { message, .. } => blocks_known(&message.content),
            Message::User(user) => {
                if let UserContent::Blocks(blocks) = &user.message.content {
                    blocks_known(blocks);
                }
            }
            Message::StreamEvent { event, .. } => {
                assert!(
                    !matches!(
                        event,
                        StreamEvent::Other
                            | StreamEvent::ContentBlockDelta {
                                delta: Delta::Other,
                                ..
                            }
                    ),
                    "an event the window doesn't know: {line}"
                );
                if let StreamEvent::ContentBlockStart { content_block, .. } = event {
                    blocks_known(std::slice::from_ref(content_block));
                }
            }
            Message::System(system) => {
                assert!(
                    !matches!(system, System::Other),
                    "a system line the window doesn't know: {line}"
                );
            }
            Message::ControlRequest { request, .. } => {
                assert!(
                    !matches!(request, IncomingRequest::Other),
                    "a request the window doesn't know: {line}"
                );
            }
            Message::Result(_)
            | Message::ControlResponse { .. }
            | Message::RateLimitEvent { .. }
            | Message::ControlCancelRequest { .. } => {}
            Message::Other => panic!("a line the window doesn't know: {line}"),
        }
    }

    /// Every line the adapter writes for the window reads as a line of Claude Code's, or as a
    /// tool call as the agent gave it: through a turn with streamed thinking and text, a tool
    /// call asked about and answered, a mode and a title, another session's message, a `!`
    /// command, a failed turn and one that went well.
    #[test]
    fn the_window_reads_every_line() {
        let mut translator = translator_for(Agent::Agy, Start::New);
        let mut effects = translator.start();
        effects.extend(translator.window_said(&asks(
            "w1",
            ControlRequest::Initialize {
                per_task_stop_affordance: true,
            },
        )));
        effects.extend(
            translator.agent_said(r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#),
        );
        effects.extend(translator.agent_said(
            r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s","models":{"availableModels":[{"modelId":"m1","name":"Model one"}],"currentModelId":"m1"},"modes":{"availableModes":[{"id":"fast","name":"Fast"}],"currentModeId":"fast"}}}"#,
        ));
        effects.extend(translator.window_said(&prompt("go")));
        let first = prompt_id(&effects);
        for update in [
            r#"{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"Hmm"}}"#,
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Listing."}}"#,
            r#"{"sessionUpdate":"tool_call","toolCallId":"c1","name":"run_command","kind":"execute","status":"pending","title":"ls","rawInput":{"CommandLine":"ls"}}"#,
            r#"{"sessionUpdate":"current_mode_update","currentModeId":"fast"}"#,
            r#"{"sessionUpdate":"session_info_update","title":"Listing files"}"#,
        ] {
            effects.extend(translator.agent_said(&format!(
                r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s","update":{update}}}}}"#
            )));
        }
        effects.extend(translator.agent_said(
            r#"{"jsonrpc":"2.0","id":"p1","method":"session/request_permission","params":{"sessionId":"s","toolCall":{"toolCallId":"c1","title":"ls"},"options":[{"optionId":"a","name":"Allow","kind":"allow_once"},{"optionId":"r","name":"Reject","kind":"reject_once"}]}}"#,
        ));
        effects.extend(translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"c1","status":"completed","content":[{"type":"content","content":{"type":"text","text":"a.txt"}}]}}}"#,
        ));
        effects.extend(translator.peer(
            "<cross-session-message from=\"calm-heron\">hello</cross-session-message>".to_owned(),
            PeerMessages::LetAllIn,
        ));
        effects.extend(translator.window_said(&said(&Command::Shell {
            command: "ls".to_owned(),
        })));
        effects.extend(translator.agent_said(&format!(
            r#"{{"jsonrpc":"2.0","id":{first},"error":{{"code":-32603,"message":"overloaded"}}}}"#
        )));
        let second = prompt_id(&effects);
        effects.extend(translator.agent_said(&format!(
            r#"{{"jsonrpc":"2.0","id":{second},"result":{{"stopReason":"end_turn"}}}}"#
        )));
        let mut window = std::collections::BTreeSet::new();
        for line in window_lines(&effects) {
            read_by_the_window(&line);
            window.extend(kinds(&[line]));
        }
        let expected = |names: &[&str]| -> std::collections::BTreeSet<String> {
            names.iter().map(|name| (*name).to_owned()).collect()
        };
        assert_eq!(
            window,
            expected(&[
                "assistant",
                "control_response",
                "permission_asked",
                "result",
                "stream_event:content_block_delta",
                "stream_event:content_block_start",
                "stream_event:content_block_stop",
                "stream_event:message_start",
                "system",
                "tool_call",
                "user",
            ])
        );
    }

    /// A translator past `initialize` and `session/new`, its session `s`.
    fn opened() -> Translator {
        opened_with(Agent::Agy)
    }

    fn opened_with(agent: Agent) -> Translator {
        let mut translator = translator_for(agent, Start::New);
        translator.start();
        translator.agent_said(r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#);
        translator.agent_said(
            r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s","models":{"availableModels":[{"modelId":"gemini-3.8-flash-high","name":"Gemini 3.8 Flash (High)"}],"currentModelId":"gemini-3.8-flash-high"}}}"#,
        );
        translator
    }

    /// The window's `initialize`, sent before the session is open, is answered once it is,
    /// with agy's models; from then on the session goes by agy's id for it.
    #[test]
    fn initialize_waits_for_the_session() {
        let mut translator = translator(Start::New);
        translator.start();
        let early = translator.window_said(&asks(
            "r1",
            ControlRequest::Initialize {
                per_task_stop_affordance: true,
            },
        ));
        assert!(early.is_empty());
        let initialized =
            translator.agent_said(r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#);
        let asked = agent_lines(&initialized);
        assert_eq!(
            asked.first().and_then(|line| line.get("method")),
            Some(&json!("session/new"))
        );
        assert_eq!(
            asked
                .first()
                .and_then(|line| line.pointer("/params/mcpServers/0/args/0")),
            Some(&json!(super::super::TOOL_FLAG))
        );
        let opened = translator.agent_said(
            r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s","models":{"availableModels":[{"modelId":"m1","name":"Model one"}],"currentModelId":"m1"}}}"#,
        );
        let answer = window_lines(&opened)
            .into_iter()
            .find(|line| line.get("type") == Some(&json!("control_response")))
            .expect("initialize answered");
        assert_eq!(
            answer.pointer("/response/response/models/0/value"),
            Some(&json!("m1"))
        );
        // From now on the session is known by agy's id for it, which agy lists and loads.
        let agys = SessionId::for_external(crate::provider::Backend::Agy, "s");
        assert!(
            opened
                .iter()
                .any(|effect| matches!(effect, Effect::Named(id) if *id == agys))
        );
        let turn = window_lines(&translator.window_said(&prompt("go")));
        let init = turn
            .iter()
            .find(|line| line.get("subtype") == Some(&json!("init")))
            .expect("the turn tells the window the session");
        assert_eq!(init.get("session_id"), Some(&json!(agys.as_str())));
    }

    /// The permission request among the window's lines: its id, the call it is about, the
    /// agent's choices.
    fn permission_of(window: &[Value]) -> (RequestId, Value, Vec<AcpOption>) {
        let asked = window
            .iter()
            .find(|line| line.get("type") == Some(&json!("permission_asked")))
            .expect("the window asked");
        let request = asked
            .get("request_id")
            .and_then(Value::as_str)
            .map(|id| RequestId::from(id.to_owned()))
            .expect("an id");
        let options = asked.get("options").cloned().expect("choices");
        (
            request,
            asked.clone(),
            serde_json::from_value(options).expect("choices the window reads"),
        )
    }

    /// The window's choice, as it sends it.
    fn chose(request: RequestId, option: &str) -> String {
        said(&Command::Choose {
            request_id: request,
            option: OptionId::from(option.to_owned()),
        })
    }

    /// The call among the window's lines, as the agent last gave it.
    fn call_of(window: &[Value]) -> AcpCall {
        let call = window
            .iter()
            .rfind(|line| line.get("type") == Some(&json!("tool_call")))
            .and_then(|line| line.get("call"))
            .cloned()
            .expect("a tool call");
        serde_json::from_value(call).expect("a call the window reads")
    }

    /// A turn reads as Claude Code's: init, streamed text, the assistant message, the tool
    /// call as the agent gave it and again with its result, then the result of the turn.
    #[test]
    fn a_turn_reads_as_claude_codes() {
        let mut translator = opened();
        let prompted = translator.window_said(&prompt("Count the lines"));
        assert_eq!(kinds(&window_lines(&prompted)), ["system"]);
        let prompt = agent_lines(&prompted)
            .into_iter()
            .find(|line| line.get("method") == Some(&json!("session/prompt")))
            .expect("agy prompted");
        let prompt_id = prompt.get("id").cloned().expect("an id");
        let mut lines = Vec::new();
        for update in [
            r#"{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"Reading."}}"#,
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Let me count."}}"#,
            r#"{"sessionUpdate":"tool_call","toolCallId":"c1","name":"run_command","kind":"execute","status":"pending","title":"wc -l notes.txt","rawInput":{"CommandLine":"wc -l notes.txt","toolSummary":"Count"}}"#,
            r#"{"sessionUpdate":"tool_call_update","toolCallId":"c1","status":"completed","content":[{"type":"content","content":{"type":"text","text":"3 notes.txt"}}]}"#,
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Three."}}"#,
        ] {
            let line = format!(
                r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s","update":{update}}}}}"#
            );
            lines.extend(translator.agent_said(&line));
        }
        lines.extend(translator.agent_said(&format!(
            r#"{{"jsonrpc":"2.0","id":{prompt_id},"result":{{"stopReason":"end_turn","usage":{{"inputTokens":10,"outputTokens":2}}}}}}"#
        )));
        let window = window_lines(&lines);
        assert_eq!(
            kinds(&window),
            [
                "stream_event:message_start",
                "stream_event:content_block_start",
                "stream_event:content_block_delta",
                "stream_event:content_block_stop",
                "stream_event:content_block_start",
                "stream_event:content_block_delta",
                "stream_event:content_block_stop",
                "assistant",
                "tool_call",
                "tool_call",
                "stream_event:message_start",
                "stream_event:content_block_start",
                "stream_event:content_block_delta",
                "stream_event:content_block_stop",
                "assistant",
                "result",
            ]
        );
        let call = call_of(&window);
        assert_eq!(call.title, "wc -l notes.txt");
        assert_eq!(call.kind, Some(AcpKind::Execute));
        assert_eq!(call.status, AcpStatus::Completed);
        assert_eq!(call.output.as_deref(), Some("3 notes.txt"));
        let result = window.last().expect("the result");
        assert_eq!(result.get("result"), Some(&json!("Let me count.Three.")));
        assert_eq!(result.get("is_error"), Some(&json!(false)));
    }

    /// agy sends a call with its arguments: it is shown at once, before it completes.
    #[test]
    fn agys_call_shows_at_once() {
        let mut translator = opened();
        let announced = translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call","toolCallId":"c1","name":"run_command","kind":"execute","status":"pending","title":"ls","rawInput":{"CommandLine":"ls"}}}}"#,
        );
        let window = window_lines(&announced);
        assert_eq!(kinds(&window), ["tool_call"]);
        let call = call_of(&window);
        assert_eq!(call.title, "ls");
        assert_eq!(call.input, Some(json!({"CommandLine": "ls"})));
    }

    /// opencode names a tool before it sends its arguments: the call isn't shown until they
    /// come, and then it is shown once, as opencode gives it.
    #[test]
    fn opencodes_call_shows_once_its_arguments_come() {
        let mut translator = opened_with(Agent::Opencode);
        let announced = translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call","toolCallId":"c1","name":"shell","title":"shell","kind":"execute","status":"pending","locations":[{"path":"/w"}],"rawInput":{"cwd":"/w"}}}}"#,
        );
        assert!(
            window_lines(&announced).is_empty(),
            "nothing before the arguments"
        );
        let updated = translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"c1","status":"in_progress","kind":"execute","title":"ls","locations":[{"path":"/w"}],"rawInput":{"command":"ls","cwd":"/w"}}}}"#,
        );
        let window = window_lines(&updated);
        assert_eq!(kinds(&window), ["tool_call"]);
        let call = call_of(&window);
        assert_eq!(call.title, "ls");
        assert_eq!(call.locations, [PathBuf::from("/w")]);
        assert_eq!(call.input, Some(json!({"command": "ls", "cwd": "/w"})));
    }

    /// A permission request for a call kept without its arguments shows the call, with the
    /// arguments the request has.
    #[test]
    fn a_permission_request_shows_a_kept_call() {
        let mut translator = opened_with(Agent::Opencode);
        let kept = translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call","toolCallId":"c1","name":"shell","title":"shell","kind":"execute","status":"pending","locations":[{"path":"/w"}],"rawInput":{"cwd":"/w"}}}}"#,
        );
        assert!(window_lines(&kept).is_empty());
        let asked = translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/request_permission","params":{"sessionId":"s","toolCall":{"toolCallId":"c1","name":"shell","title":"ls","kind":"execute","status":"pending","rawInput":{"command":"ls","cwd":"/w"}},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"},{"optionId":"deny","name":"Deny","kind":"reject_once"}]},"id":"7"}"#,
        );
        let window = window_lines(&asked);
        let call = call_of(&window);
        assert_eq!(call.input, Some(json!({"command": "ls", "cwd": "/w"})));
        let (_, asked, _) = permission_of(&window);
        assert_eq!(asked.get("tool_use_id"), Some(&json!("c1")));
        assert_eq!(asked.pointer("/call/title"), Some(&json!("ls")));
        assert_eq!(asked.pointer("/call/input/command"), Some(&json!("ls")));
    }

    /// An edit shows as the agent gives it: its own name and title, its file, and the change
    /// it makes.
    #[test]
    fn an_edit_shows_the_agents_change() {
        let call = ToolCall {
            tool_call_id: "c4".to_owned(),
            name: Some("edit".to_owned()),
            title: Some("notes.txt".to_owned()),
            kind: Some(acp::ToolKind::Edit),
            locations: Some(vec![Location {
                path: PathBuf::from("/w/notes.txt"),
            }]),
            content: Some(vec![ToolContent::Diff {
                path: PathBuf::from("/w/notes.txt"),
                old_text: Some("two".to_owned()),
                new_text: "TWO".to_owned(),
            }]),
            raw_input: Some(json!({"path": "notes.txt"})),
            ..ToolCall::default()
        };
        assert_eq!(
            drawn(Agent::Opencode, &call),
            Call::Acp(AcpCall {
                title: "notes.txt".to_owned(),
                kind: Some(AcpKind::Edit),
                locations: vec![PathBuf::from("/w/notes.txt")],
                diffs: vec![Diff {
                    path: PathBuf::from("/w/notes.txt"),
                    old_text: Some("two".to_owned()),
                    new_text: "TWO".to_owned(),
                }],
                input: Some(json!({"path": "notes.txt"})),
                status: AcpStatus::Pending,
                output: None,
            })
        );
    }

    /// agy asks before edualc's own messaging tools too: the adapter allows them, and the
    /// window isn't asked.
    #[test]
    fn edualcs_own_tools_go_without_asking() {
        let mut translator = opened();
        translator.window_said(&prompt("go"));
        let asked = translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/request_permission","params":{"sessionId":"s","toolCall":{"toolCallId":"c3","name":"mcp_edualc_list_sessions","kind":"other","status":"pending","title":"list_sessions","rawInput":{}},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"},{"optionId":"deny","name":"Deny","kind":"reject_once"}]},"id":"8"}"#,
        );
        assert!(
            window_lines(&asked)
                .iter()
                .all(|line| line.get("type") != Some(&json!("control_request"))),
            "the window wasn't asked"
        );
        assert_eq!(
            agent_lines(&asked)
                .first()
                .and_then(|line| line.pointer("/result/outcome/optionId")),
            Some(&json!("allow"))
        );
    }

    /// agy's permission request is asked in the window with agy's choices, in agy's words; the
    /// answer is the choice made, and the denied call ends as agy ends it (the window knows it
    /// denied it).
    #[test]
    fn permissions_go_through_the_window() {
        let mut translator = opened();
        translator.window_said(&prompt("go"));
        let asked = translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/request_permission","params":{"sessionId":"s","toolCall":{"toolCallId":"c2","name":"run_command","kind":"execute","status":"pending","title":"touch /tmp/x","rawInput":{"CommandLine":"touch /tmp/x"}},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"},{"optionId":"deny","name":"Deny","kind":"reject_once"}]},"id":"7"}"#,
        );
        let (request, asked, options) = permission_of(&window_lines(&asked));
        assert_eq!(asked.pointer("/call/title"), Some(&json!("touch /tmp/x")));
        assert_eq!(
            options,
            [
                AcpOption {
                    id: OptionId::from("allow".to_owned()),
                    name: "Allow".to_owned(),
                    kind: AcpOptionKind::AllowOnce,
                },
                AcpOption {
                    id: OptionId::from("deny".to_owned()),
                    name: "Deny".to_owned(),
                    kind: AcpOptionKind::RejectOnce,
                },
            ]
        );
        let denied = translator.window_said(&chose(request, "deny"));
        assert_eq!(
            agent_lines(&denied)
                .first()
                .and_then(|line| line.pointer("/result/outcome/optionId")),
            Some(&json!("deny"))
        );
        let failed = translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"c2","status":"failed","content":[]}}}"#,
        );
        assert_eq!(call_of(&window_lines(&failed)).status, AcpStatus::Failed);
    }

    /// The prompt goes to the agent as ACP names its parts: text as `text`, an image with its
    /// `mimeType` and its base64 `data`, under the request's `method` and `params`.
    #[test]
    fn a_prompt_is_sent_as_acp_names_it() {
        let mut translator = opened();
        let image = wire::ImageBlock::base64(MediaType::Png, "QUJD".to_owned());
        let sent = translator.window_said(&said(&Command::Prompt {
            shell: Vec::new(),
            images: vec![image],
            text: "look".to_owned(),
        }));
        let id = prompt_id(&sent);
        assert_eq!(
            agent_lines(&sent),
            vec![json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "session/prompt",
                "params": {
                    "sessionId": "s",
                    "prompt": [
                        {"type": "image", "mimeType": "image/png", "data": "QUJD"},
                        {"type": "text", "text": "look"}
                    ]
                }
            })]
        );
    }

    /// A permission answer goes back under the id the request came with, its outcome the
    /// option the user chose, as `selected` with that option's `optionId`.
    #[test]
    fn a_permission_answer_is_acps() {
        let mut translator = opened();
        translator.window_said(&prompt("go"));
        let asked = translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/request_permission","params":{"sessionId":"s","toolCall":{"toolCallId":"c2","name":"run_command","kind":"execute","status":"pending","title":"touch /tmp/x","rawInput":{"CommandLine":"touch /tmp/x"}},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"},{"optionId":"deny","name":"Deny","kind":"reject_once"}]},"id":"7"}"#,
        );
        let (request, _, _) = permission_of(&window_lines(&asked));
        let answered = translator.window_said(&chose(request, "allow"));
        assert_eq!(
            agent_lines(&answered),
            vec![json!({
                "jsonrpc": "2.0",
                "id": "7",
                "result": {"outcome": {"outcome": "selected", "optionId": "allow"}}
            })]
        );
    }

    /// A choice the agent didn't offer isn't passed on as one: the request is cancelled.
    #[test]
    fn a_choice_the_agent_didnt_offer_cancels() {
        let mut translator = opened();
        translator.window_said(&prompt("go"));
        let asked = translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/request_permission","params":{"sessionId":"s","toolCall":{"toolCallId":"c2","name":"run_command","kind":"execute","status":"pending","title":"touch /tmp/x","rawInput":{"CommandLine":"touch /tmp/x"}},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"}]},"id":"7"}"#,
        );
        let (request, _, _) = permission_of(&window_lines(&asked));
        let answered = translator.window_said(&chose(request, "always"));
        assert_eq!(
            agent_lines(&answered)
                .first()
                .and_then(|line| line.pointer("/result/outcome/outcome")),
            Some(&json!("cancelled"))
        );
    }

    /// A turn the agent ends by refusing, or that an answer edualc can't read ends, ends as
    /// failed with the reason in the conversation; one it ends as done ends as a success.
    #[test]
    fn a_turn_cut_short_ends_as_failed_saying_why() {
        let ended_with = |answer: &str| {
            let mut translator = opened();
            let prompted = translator.window_said(&prompt("go"));
            let rpc = prompt_id(&prompted);
            let ended = translator.agent_said(&format!(
                r#"{{"jsonrpc":"2.0","id":{rpc},"result":{answer}}}"#
            ));
            let lines = window_lines(&ended);
            let failed = lines
                .iter()
                .find(|line| line.get("type") == Some(&json!("result")))
                .and_then(|line| line.get("is_error"))
                .and_then(Value::as_bool)
                .expect("a result");
            let said = lines
                .iter()
                .filter_map(|line| line.pointer("/message/content/0/text"))
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ");
            (failed, said)
        };
        let (failed, said) = ended_with(r#"{"stopReason":"refusal"}"#);
        assert!(failed);
        assert!(said.contains("refused to go on"), "{said}");
        let (failed, said) = ended_with(r#"{"stopReason":"max_tokens"}"#);
        assert!(failed);
        assert!(said.contains("the most it may write"), "{said}");
        let (failed, said) = ended_with(r#"{"no":"stop reason"}"#);
        assert!(failed);
        assert!(said.contains("can't be read"), "{said}");
        let (failed, _) = ended_with(r#"{"stopReason":"end_turn"}"#);
        assert!(!failed);
    }

    /// A turn that fails because Google's API refused agy's sign-in ends with a 401 in its
    /// result, for the window to offer signing in and trying again, and agy's words aren't shown
    /// as a message. Any other failure is shown in agy's words.
    #[test]
    fn a_refused_sign_in_ends_the_turn_as_a_401() {
        let failed_with = |error: &str| {
            let mut translator = opened();
            let prompted = translator.window_said(&prompt("go"));
            let prompt_id = agent_lines(&prompted)
                .into_iter()
                .find(|line| line.get("method") == Some(&json!("session/prompt")))
                .and_then(|line| line.get("id").cloned())
                .expect("prompted");
            let ended = translator.agent_said(&format!(
                r#"{{"jsonrpc":"2.0","id":{prompt_id},"error":{error}}}"#
            ));
            window_lines(&ended)
        };
        let result = |lines: &[Value]| {
            lines
                .iter()
                .find(|line| line.get("type") == Some(&json!("result")))
                .cloned()
                .expect("a result")
        };
        let said = |lines: &[Value]| {
            lines
                .iter()
                .any(|line| line.get("type") == Some(&json!("assistant")))
        };

        let unauthenticated = failed_with(
            r#"{"code":-32603,"message":"UNAUTHENTICATED (code 401): Request had invalid authentication credentials."}"#,
        );
        assert_eq!(
            result(&unauthenticated).get("api_error_status"),
            Some(&json!(401))
        );
        assert!(!said(&unauthenticated), "agy's words aren't shown");

        let auth_required = failed_with(r#"{"code":-32000,"message":"Authentication required"}"#);
        assert_eq!(
            result(&auth_required).get("api_error_status"),
            Some(&json!(401))
        );

        let other = failed_with(r#"{"code":-32603,"message":"quota exhausted"}"#);
        assert_eq!(result(&other).get("api_error_status"), Some(&Value::Null));
        assert!(said(&other), "agy's words are shown");
    }

    /// A message from another session is shown as one, and prompts agy once its turn ends.
    #[test]
    fn a_peer_message_waits_for_its_turn() {
        let mut translator = opened();
        let first = translator.window_said(&prompt("hello"));
        let prompt_id = agent_lines(&first)
            .into_iter()
            .find(|line| line.get("method") == Some(&json!("session/prompt")))
            .and_then(|line| line.get("id").cloned())
            .expect("prompted");
        let envelope = "<cross-session-message from-name=\"tidy-otter-work-12\" from-mode=\"prompting\">\nHi there\n</cross-session-message>";
        let peer = translator.peer(envelope.to_owned(), PeerMessages::LetAllIn);
        let shown = window_lines(&peer);
        assert_eq!(
            shown.first().and_then(|line| line.pointer("/origin/name")),
            Some(&json!("tidy-otter-work-12"))
        );
        assert_eq!(
            shown.first().and_then(|line| line.pointer("/origin/body")),
            Some(&json!("Hi there"))
        );
        assert!(agent_lines(&peer).is_empty(), "agy is busy");
        let ended = translator.agent_said(&format!(
            r#"{{"jsonrpc":"2.0","id":{prompt_id},"result":{{"stopReason":"end_turn"}}}}"#
        ));
        let next = agent_lines(&ended)
            .into_iter()
            .find(|line| line.get("method") == Some(&json!("session/prompt")))
            .expect("the peer's message prompted");
        assert_eq!(
            next.pointer("/params/prompt/0/text"),
            Some(&json!(envelope))
        );
    }

    /// A message from a sender the user hasn't let in waits while the window asks, offering
    /// to let the sender in. Turned down, the agent never sees it; let in with its sender, it
    /// comes in, and so does the sender's next message, unasked. Another sender is asked about.
    #[test]
    fn a_new_senders_message_waits_to_be_let_in() {
        let mut translator = opened();
        let from = |name: &str, text: &str| {
            format!(
                "<cross-session-message from-name=\"{name}\" from-mode=\"prompting\">\n{text}\n</cross-session-message>"
            )
        };
        let asked = window_lines(
            &translator.peer(from("tidy-otter", "First"), PeerMessages::AskEachSender),
        );
        let [card] = asked.as_slice() else {
            panic!("one question: {asked:?}");
        };
        assert_eq!(
            card.pointer("/request/tool_name"),
            Some(&json!("edualc_incoming_message"))
        );
        assert_eq!(
            card.pointer("/request/input"),
            Some(&json!({"from": "tidy-otter", "message": "First"}))
        );
        let offered = card
            .pointer("/request/permission_suggestions/0")
            .cloned()
            .expect("letting the sender in, offered");
        let answer = |request: &Value, response: Value| {
            serde_json::to_string(&json!({
                "ControlResponse": {
                    "response": {"subtype": "success", "request_id": request, "response": response},
                }
            }))
            .expect("a line")
        };
        let request = card.get("request_id").cloned().expect("an id");
        let turned_down = translator.window_said(&answer(
            &request,
            json!({"behavior": "deny", "message": "No."}),
        ));
        assert!(turned_down.is_empty(), "{turned_down:?}");

        let asked = window_lines(
            &translator.peer(from("tidy-otter", "Second"), PeerMessages::AskEachSender),
        );
        let request = asked
            .first()
            .and_then(|card| card.get("request_id"))
            .cloned()
            .expect("asked again");
        let let_in = translator.window_said(&answer(
            &request,
            json!({"behavior": "allow", "updatedInput": {}, "updatedPermissions": [offered]}),
        ));
        assert_eq!(
            agent_lines(&let_in)
                .iter()
                .find_map(|line| line.pointer("/params/prompt/0/text"))
                .cloned(),
            Some(json!(from("tidy-otter", "Second")))
        );

        let unasked = translator.peer(from("tidy-otter", "Third"), PeerMessages::AskEachSender);
        assert!(
            window_lines(&unasked)
                .iter()
                .all(|line| line.get("type") != Some(&json!("control_request"))),
            "the sender was let in"
        );
        let other =
            window_lines(&translator.peer(from("calm-heron", "Hi"), PeerMessages::AskEachSender));
        assert_eq!(
            other.first().and_then(|line| line.get("type")),
            Some(&json!("control_request"))
        );
    }

    /// agy's form for its `ask_question` tool, as agy 1.5.9 sent it (descriptions shortened):
    /// a question taking one option, one taking several, each with a field for words beside.
    const FORM: &str = r#"{"jsonrpc":"2.0","id":"1","method":"elicitation/create","params":{"sessionId":"s","toolCallId":"call_1","mode":"form","message":"The agent has 2 questions.","requestedSchema":{"type":"object","properties":{"q1":{"type":"string","title":"Tabs or spaces?","oneOf":[{"const":"1","title":"(Recommended) Spaces"},{"const":"2","title":"Tabs"}]},"q1_other":{"type":"string","title":"Other","description":"Optional free-form answer to: Tabs or spaces?"},"q2":{"type":"array","title":"Which colours?","items":{"anyOf":[{"const":"1","title":"(Recommended) Red"},{"const":"2","title":"Green"},{"const":"3","title":"Blue"}]}},"q2_other":{"type":"string","title":"Other","description":"Optional free-form answer to: Which colours?"}}}}}"#;

    /// agy's form is asked as Claude Code's questions are, on its tool call, in its order; the
    /// labels picked go back as the options' values, typed words in the field for words beside,
    /// and turning the questions down declines the form.
    #[test]
    fn agys_form_is_asked_as_questions() {
        let answer = |request: &Value, response: Value| {
            serde_json::to_string(&json!({
                "ControlResponse": {
                    "response": {"subtype": "success", "request_id": request, "response": response},
                }
            }))
            .expect("a line")
        };
        let mut translator = opened();
        let asked = window_lines(&translator.agent_said(FORM));
        let [card] = asked.as_slice() else {
            panic!("one question: {asked:?}");
        };
        assert_eq!(
            card.pointer("/request/tool_name"),
            Some(&json!("AskUserQuestion"))
        );
        assert_eq!(card.pointer("/request/tool_use_id"), Some(&json!("call_1")));
        let questions = card
            .pointer("/request/input/questions")
            .and_then(Value::as_array)
            .expect("questions");
        let asked_as: Vec<(Option<&Value>, Option<&Value>, usize)> = questions
            .iter()
            .map(|question| {
                (
                    question.get("question"),
                    question.get("multiSelect"),
                    question
                        .get("options")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len),
                )
            })
            .collect();
        assert_eq!(
            asked_as,
            [
                (Some(&json!("Tabs or spaces?")), Some(&json!(false)), 2),
                (Some(&json!("Which colours?")), Some(&json!(true)), 3),
            ]
        );
        let request = card.get("request_id").cloned().expect("an id");
        let answered = agent_lines(&translator.window_said(&answer(
            &request,
            json!({"behavior": "allow", "updatedInput": {
                "questions": [],
                "answers": {"Tabs or spaces?": "Tabs", "Which colours?": "(Recommended) Red, Blue"},
            }}),
        )));
        assert_eq!(
            answered.first(),
            Some(&json!({"jsonrpc": "2.0", "id": "1", "result": {
                "action": "accept", "content": {"q1": "2", "q2": ["1", "3"]},
            }}))
        );

        let asked = window_lines(&translator.agent_said(FORM));
        let request = asked
            .first()
            .and_then(|card| card.get("request_id"))
            .cloned()
            .expect("asked again");
        let typed = agent_lines(&translator.window_said(&answer(
            &request,
            json!({"behavior": "allow", "updatedInput": {
                "questions": [],
                "answers": {"Tabs or spaces?": "Whatever the file uses"},
            }}),
        )));
        assert_eq!(
            typed
                .first()
                .and_then(|line| line.pointer("/result/content")),
            Some(&json!({"q1_other": "Whatever the file uses"}))
        );

        let asked = window_lines(&translator.agent_said(FORM));
        let request = asked
            .first()
            .and_then(|card| card.get("request_id"))
            .cloned()
            .expect("asked again");
        let declined = agent_lines(&translator.window_said(&answer(
            &request,
            json!({"behavior": "deny", "message": "The user chose not to answer."}),
        )));
        assert_eq!(
            declined.first().and_then(|line| line.get("result")),
            Some(&json!({"action": "decline"}))
        );
    }

    /// agy's messaging tool sends at once: nothing is asked in the window.
    #[test]
    fn messages_go_without_asking() {
        let mut translator = opened();
        let sent = translator.tool_called(CallId(4), "calm-heron".to_owned(), "hi".to_owned());
        assert!(
            matches!(
                sent.as_slice(),
                [Effect::Send { call: CallId(4), to, message }] if to == "calm-heron" && message == "hi"
            ),
            "one message sent, and nothing else: {sent:?}"
        );
    }

    /// agy says nothing of what an MCP tool gave: the call's result is what edualc's tool
    /// answered it.
    #[test]
    fn the_tools_answer_is_the_calls_result() {
        let mut translator = opened();
        translator.window_said(&prompt("go"));
        translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call","toolCallId":"c3","name":"mcp_edualc_send_message","kind":"other","status":"in_progress","rawInput":{"to":"calm-heron","message":"hi"}}}}"#,
        );
        translator.tool_called(CallId(9), "calm-heron".to_owned(), "hi".to_owned());
        translator.tool_told(
            CallId(9),
            &super::super::tool::Answer {
                text: "Delivered to calm-heron.".to_owned(),
                error: false,
            },
        );
        let ended = translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"c3","status":"completed"}}}"#,
        );
        assert_eq!(
            window_lines(&ended)
                .first()
                .and_then(|line| line.pointer("/message/content/0/content")),
            Some(&json!("Delivered to calm-heron."))
        );
    }

    /// A file or command tool keeps the agent's name and arguments, named or not: an append of
    /// agy's (1.5.6's arguments, as logged), a command by ACP's kind and its title, a read of
    /// its location.
    #[test]
    fn file_and_command_tools_show_as_the_agent_gives_them() {
        let appended = ToolCall {
            tool_call_id: "c".to_owned(),
            name: Some("write_file".to_owned()),
            title: Some("Append to f.txt".to_owned()),
            raw_input: Some(json!({"Append": true, "Content": "three\n", "Path": "/w/f.txt"})),
            ..ToolCall::default()
        };
        let Call::Acp(call) = drawn(Agent::Agy, &appended) else {
            panic!("agy's own call");
        };
        assert_eq!(call.title, "Append to f.txt");
        assert_eq!(call.input, appended.raw_input);
        let command = ToolCall {
            tool_call_id: "c".to_owned(),
            title: Some("ls -la".to_owned()),
            kind: Some(acp::ToolKind::Execute),
            ..ToolCall::default()
        };
        let Call::Acp(call) = drawn(Agent::Agy, &command) else {
            panic!("agy's own call");
        };
        assert_eq!(call.title, "ls -la");
        assert_eq!(call.kind, Some(AcpKind::Execute));
        let read = ToolCall {
            tool_call_id: "c".to_owned(),
            title: Some("Read f.txt".to_owned()),
            kind: Some(acp::ToolKind::Read),
            locations: Some(vec![Location {
                path: PathBuf::from("/w/f.txt"),
            }]),
            ..ToolCall::default()
        };
        let Call::Acp(call) = drawn(Agent::Opencode, &read) else {
            panic!("opencode's own call");
        };
        assert_eq!(call.locations, [PathBuf::from("/w/f.txt")]);
    }

    /// agy's swarm tools that edualc acts on go by the names the window knows, with arguments
    /// as agy 1.5.9 gives them: a post to the board as a message, a member joining as an agent
    /// with its brief, its questions as AskUserQuestion's; what else `swarm` does is agy's own.
    #[test]
    fn agys_swarm_tools_read_as_claude_codes() {
        let shown = |name: &str, args: Value| {
            named(
                Agent::Agy,
                &ToolCall {
                    tool_call_id: "c".to_owned(),
                    name: Some(name.to_owned()),
                    raw_input: Some(args),
                    ..ToolCall::default()
                },
            )
        };
        assert_eq!(
            shown("post_message", json!({ "text": "@verifier Ready." })),
            Some((
                ToolName::EdualcSendMessage,
                json!({ "to": "the board", "message": "@verifier Ready." })
            ))
        );
        let (name, input) = shown(
            "swarm",
            json!({ "action": "add", "name": "monitor", "brief": "Watch for stalls." }),
        )
        .expect("a member joining");
        assert_eq!(name, ToolName::Agent);
        assert_eq!(input.get("subagent_type"), Some(&json!("monitor")));
        assert_eq!(input.get("prompt"), Some(&json!("Watch for stalls.")));
        assert_eq!(shown("swarm", json!({ "action": "status" })), None);
        let (name, input) = shown(
            "ask_question",
            json!({ "questions": [{ "question": "Which?", "options": ["A", "B"], "is_multi_select": true }] }),
        )
        .expect("a question");
        assert_eq!(name, ToolName::AskUserQuestion);
        assert_eq!(
            input,
            json!({ "questions": [{
                "question": "Which?",
                "options": [{ "label": "A" }, { "label": "B" }],
                "multiSelect": true,
            }] })
        );
    }

    /// agy's subagents are looked at as a turn ends, under agy's id for the conversation;
    /// opencode keeps no records of them, and its are never looked for.
    #[test]
    fn subagents_are_looked_at_as_a_turn_ends() {
        let looked = |agent| {
            let mut translator = opened_with(agent);
            let prompted = translator.window_said(&prompt("go"));
            let rpc = prompt_id(&prompted);
            translator
                .agent_said(&format!(
                    r#"{{"jsonrpc":"2.0","id":{rpc},"result":{{"stopReason":"end_turn"}}}}"#
                ))
                .into_iter()
                .filter_map(|effect| match effect {
                    Effect::Subagents { conversation } => Some(conversation),
                    Effect::ToAgent(_)
                    | Effect::ToWindow(_)
                    | Effect::Named(_)
                    | Effect::Send { .. }
                    | Effect::List(_)
                    | Effect::Quotas { .. } => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(looked(Agent::Agy), ["s"]);
        assert!(looked(Agent::Opencode).is_empty());
    }

    /// What a command printed, as agy gives it in the call's raw output, is the call's result;
    /// a raw output of another shape isn't.
    #[test]
    fn a_commands_printed_output_is_its_result() {
        let ended = |raw_output| {
            result_text(
                &ToolCall {
                    tool_call_id: "c".to_owned(),
                    raw_output: Some(raw_output),
                    ..ToolCall::default()
                },
                false,
            )
        };
        assert_eq!(ended(json!({ "output": "3 notes.txt\n" })), "3 notes.txt\n");
        assert_eq!(ended(json!({ "files": ["a"] })), "Done.");
    }

    /// A `!` command is told as not run, in the rows the window reads a command's run from,
    /// and nothing reaches agy.
    #[test]
    fn shell_commands_are_refused() {
        let mut translator = opened();
        let refused = translator.window_said(&said(&Command::Shell {
            command: "ls <dir>".to_owned(),
        }));
        assert!(agent_lines(&refused).is_empty());
        let window = window_lines(&refused);
        let rows: Vec<&str> = window
            .iter()
            .filter_map(|line| line.pointer("/message/content").and_then(Value::as_str))
            .collect();
        assert_eq!(
            rows.first(),
            Some(&"<bash-input>ls &lt;dir&gt;</bash-input>")
        );
        assert!(rows.get(1).is_some_and(|row| row.contains("Not run")));
    }

    /// A request the window writes that the adapter doesn't do is answered with a failure, so
    /// the window isn't left waiting on it.
    #[test]
    fn a_request_the_adapter_doesnt_do_is_failed() {
        let mut translator = opened_with(Agent::Opencode);
        let answered = translator.window_said(&asks("u1", ControlRequest::GetUsage));
        assert!(agent_lines(&answered).is_empty());
        let answers = window_lines(&answered);
        assert_eq!(answers.len(), 1, "one answer: {answers:?}");
        let answer = answers.first().expect("the answer");
        assert_eq!(answer.pointer("/response/subtype"), Some(&json!("error")));
        assert_eq!(answer.pointer("/response/request_id"), Some(&json!("u1")));
        assert_eq!(
            answer.pointer("/response/error"),
            Some(&json!("opencode doesn't do this"))
        );
    }

    /// How full the context is, as the agent tells it, reaches the window at once, answers
    /// `get_context_usage`, and gives the turn's result the window's size.
    #[test]
    fn the_contexts_use_reaches_the_window() {
        let mut translator = opened_with(Agent::Opencode);
        let unknown =
            window_lines(&translator.window_said(&asks("c0", ControlRequest::GetContextUsage)));
        assert_eq!(
            unknown
                .first()
                .and_then(|line| line.pointer("/response/subtype")),
            Some(&json!("error"))
        );
        let told = window_lines(&translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"usage_update","used":42000,"size":1048576,"cost":{"amount":0,"currency":"USD"}}}}"#,
        ));
        assert_eq!(
            told.first()
                .map(|line| (line.get("subtype"), line.get("used"), line.get("size"))),
            Some((
                Some(&json!("context_used")),
                Some(&json!(42_000)),
                Some(&json!(1_048_576))
            ))
        );
        let answered =
            window_lines(&translator.window_said(&asks("c1", ControlRequest::GetContextUsage)));
        let report = answered
            .first()
            .and_then(|line| line.pointer("/response/response"))
            .expect("a report");
        assert_eq!(report.get("totalTokens"), Some(&json!(42_000)));
        assert_eq!(report.get("maxTokens"), Some(&json!(1_048_576)));
        assert_eq!(
            report.pointer("/categories/1/tokens"),
            Some(&json!(1_048_576 - 42_000))
        );
        let prompted = translator.window_said(&prompt("Hi"));
        let rpc = agent_lines(&prompted)
            .iter()
            .find(|line| line.get("method") == Some(&json!("session/prompt")))
            .and_then(|line| line.get("id").cloned())
            .expect("prompted");
        let ended = window_lines(&translator.agent_said(&format!(
            r#"{{"jsonrpc":"2.0","id":{rpc},"result":{{"stopReason":"end_turn"}}}}"#
        )));
        let result = ended
            .iter()
            .find(|line| line.get("type") == Some(&json!("result")))
            .expect("a result");
        assert_eq!(
            result.pointer("/modelUsage/gemini-3.8-flash-high/contextWindow"),
            Some(&json!(1_048_576))
        );
    }

    /// agy's quotas as `agy -p /usage` says them (shortened).
    fn quotas() -> agy::Quotas {
        serde_json::from_str::<agy::UsageAnswer>(
            r#"{"command":{"data":{"groups":[{"name":"Gemini Models","description":"Models within this group: Gemini 4 Hydrogen, Gemini 3.8 Flash","buckets":[{"window":"weekly","remaining_fraction":0.75,"reset_time":"2026-11-22T20:18:23Z"},{"window":"5h","remaining_fraction":0.5,"reset_time":"2026-11-16T13:26:14Z"}]},{"name":"Gemini 4 Argon","description":"Argon quota is separate.","buckets":[{"window":"weekly","remaining_fraction":0,"reset_time":"2026-11-22T20:14:06Z"}]}]}}}"#,
        )
        .expect("quotas")
        .command
        .data
    }

    /// agy is asked for its quotas for the window's `get_usage` and as each turn ends; the
    /// sidebar's bars are the quotas of the group the session's model draws on, and the Usage
    /// panel lists every group's, each named.
    #[test]
    fn agys_quotas_are_the_plans_limits() {
        let mut translator = opened();
        let asked = translator.window_said(&asks("u1", ControlRequest::GetUsage));
        assert!(
            matches!(asked.as_slice(), [Effect::Quotas { request: Some(request) }] if request.to_string() == "u1"),
            "{asked:?}"
        );
        let prompted = translator.window_said(&prompt("Hi"));
        let rpc = agent_lines(&prompted)
            .iter()
            .find(|line| line.get("method") == Some(&json!("session/prompt")))
            .and_then(|line| line.get("id").cloned())
            .expect("prompted");
        let ended = translator.agent_said(&format!(
            r#"{{"jsonrpc":"2.0","id":{rpc},"result":{{"stopReason":"end_turn"}}}}"#
        ));
        assert!(
            ended
                .iter()
                .any(|effect| matches!(effect, Effect::Quotas { request: None }))
        );

        let bars = window_lines(&translator.quotas_told(None, Ok(quotas())));
        let windows = bars
            .first()
            .and_then(|line| line.pointer("/rate_limit_info/unifiedWindows"))
            .expect("the bars");
        assert_eq!(windows.pointer("/five_hour/utilization"), Some(&json!(0.5)));
        assert_eq!(
            windows.pointer("/seven_day/utilization"),
            Some(&json!(0.25))
        );

        let report = window_lines(
            &translator.quotas_told(Some(RequestId::from("u1".to_owned())), Ok(quotas())),
        );
        let limits = report
            .first()
            .and_then(|line| line.pointer("/response/response/rate_limits/limits"))
            .and_then(Value::as_array)
            .expect("the limits");
        let named: Vec<(Option<&Value>, Option<&Value>)> = limits
            .iter()
            .map(|limit| (limit.get("kind"), limit.get("label")))
            .collect();
        assert_eq!(
            named,
            [
                (
                    Some(&json!("weekly_all")),
                    Some(&json!("Gemini Models, this week"))
                ),
                (
                    Some(&json!("session")),
                    Some(&json!("Gemini Models, 5 hours"))
                ),
                (
                    Some(&json!("other")),
                    Some(&json!("Gemini 4 Argon, this week"))
                ),
            ]
        );
    }

    /// Quotas agy couldn't say fail the window's `get_usage` with why, and leave the bars.
    #[test]
    fn quotas_that_cant_be_read_fail_the_request() {
        let mut translator = opened();
        assert!(
            translator
                .quotas_told(None, Err(super::super::QuotaError::TimedOut))
                .is_empty()
        );
        let failed = window_lines(&translator.quotas_told(
            Some(RequestId::from("u1".to_owned())),
            Err(super::super::QuotaError::TimedOut),
        ));
        assert_eq!(
            failed
                .first()
                .and_then(|line| line.pointer("/response/error")),
            Some(&json!(
                "agy couldn't say its plan's quotas: it didn't answer in time"
            ))
        );
    }

    /// A line the window wrote that edualc can't read, but that is a control request, is
    /// answered with a failure under its request id, saying edualc can't read it.
    #[test]
    fn a_control_request_edualc_cant_read_is_failed_under_its_id() {
        let mut translator = opened();
        let unreadable = translator.window_said(
            r#"{"ControlRequest":{"request_id":"x1","request":{"subtype":"from_a_newer_edualc"}}}"#,
        );
        assert!(agent_lines(&unreadable).is_empty());
        let answers = window_lines(&unreadable);
        assert_eq!(answers.len(), 1, "one answer: {answers:?}");
        let answer = answers.first().expect("the answer");
        assert_eq!(answer.pointer("/response/subtype"), Some(&json!("error")));
        assert_eq!(answer.pointer("/response/request_id"), Some(&json!("x1")));
        assert!(
            answer
                .pointer("/response/error")
                .and_then(Value::as_str)
                .is_some_and(|error| error.starts_with("edualc can't read this request:")),
            "the reason is said: {answer}"
        );
    }

    /// A loaded session's replayed history isn't shown again: the window read it from agy's
    /// records.
    #[test]
    fn a_loaded_sessions_history_isnt_repeated() {
        let mut translator = translator(Start::Resume("s".to_owned()));
        translator.start();
        let loading =
            translator.agent_said(r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#);
        assert_eq!(
            agent_lines(&loading)
                .first()
                .and_then(|line| line.get("method")),
            Some(&json!("session/load"))
        );
        let replayed = translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"old"}}}}"#,
        );
        assert!(replayed.is_empty());
    }

    /// The window is told agy's own modes and the one agy is in, picks one by agy's id, and
    /// hears when agy changes mode; Claude Code's modes are refused.
    #[test]
    fn agys_modes_reach_the_window() {
        let mut translator = translator(Start::New);
        translator.start();
        translator.window_said(&asks(
            "r1",
            ControlRequest::Initialize {
                per_task_stop_affordance: true,
            },
        ));
        translator.agent_said(r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#);
        let opened = translator.agent_said(
            r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s","modes":{"availableModes":[{"id":"default","name":"Default","description":"Ask before acting"},{"id":"accept-edits","name":"Accept edits"}],"currentModeId":"default"}}}"#,
        );
        let answer = window_lines(&opened)
            .into_iter()
            .find(|line| line.get("type") == Some(&json!("control_response")))
            .expect("initialize answered");
        assert_eq!(
            answer.pointer("/response/response/programModes"),
            Some(&json!({
                "offered": [
                    {"id": "default", "name": "Default", "description": "Ask before acting"},
                    {"id": "accept-edits", "name": "Accept edits", "description": ""}
                ],
                "current": "default"
            }))
        );

        let asked = translator.window_said(&asks(
            "r2",
            ControlRequest::SetProgramMode {
                mode: ModeId::from("accept-edits".to_owned()),
            },
        ));
        let set_mode = agent_lines(&asked)
            .into_iter()
            .find(|line| line.get("method") == Some(&json!("session/set_mode")))
            .expect("agy asked to switch");
        assert_eq!(
            set_mode.get("params"),
            Some(&json!({"sessionId": "s", "modeId": "accept-edits"}))
        );
        let rpc = set_mode.get("id").and_then(Value::as_u64).expect("an id");
        let switched =
            translator.agent_said(&format!(r#"{{"jsonrpc":"2.0","id":{rpc},"result":{{}}}}"#));
        assert_eq!(
            window_lines(&switched)
                .first()
                .and_then(|line| line.pointer("/response/subtype")),
            Some(&json!("success"))
        );
        let again = translator.window_said(&asks(
            "r3",
            ControlRequest::Initialize {
                per_task_stop_affordance: true,
            },
        ));
        assert_eq!(
            window_lines(&again)
                .first()
                .and_then(|line| line.pointer("/response/response/programModes/current")),
            Some(&json!("accept-edits"))
        );

        let claudes = translator.window_said(&asks(
            "r4",
            ControlRequest::SetPermissionMode {
                mode: PermissionMode::Plan,
            },
        ));
        assert!(agent_lines(&claudes).is_empty());
        assert_eq!(
            window_lines(&claudes)
                .first()
                .and_then(|line| line.pointer("/response/subtype")),
            Some(&json!("error"))
        );

        let changed = translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"current_mode_update","currentModeId":"plan"}}}"#,
        );
        let told = window_lines(&changed);
        assert_eq!(
            told.first()
                .map(|line| (line.get("subtype"), line.get("mode"))),
            Some((Some(&json!("program_mode")), Some(&json!("plan"))))
        );
    }

    /// Authentication failures while opening a session emit `authentication_failed`, and a failed
    /// model switch leaves the previous model intact.
    #[test]
    fn open_auth_failure_and_failed_set_model_preserve_state() {
        let mut first = translator(Start::New);
        first.start();
        first.window_said(&asks(
            "r1",
            ControlRequest::Initialize {
                per_task_stop_affordance: true,
            },
        ));
        first.agent_said(r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#);
        let auth_failed = first.agent_said(
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32000,"message":"Authentication required"}}"#,
        );
        let err_msg = window_lines(&auth_failed)
            .into_iter()
            .find(|line| line.get("type") == Some(&json!("assistant")))
            .expect("assistant error emitted");
        assert_eq!(err_msg.get("error"), Some(&json!("authentication_failed")));

        let mut second = translator(Start::New);
        second.start();
        second.window_said(&asks(
            "r1",
            ControlRequest::Initialize {
                per_task_stop_affordance: true,
            },
        ));
        second.agent_said(r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#);
        second.agent_said(
            r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s","models":{"availableModels":[{"modelId":"m1","name":"Model 1"},{"modelId":"m2","name":"Model 2"}],"currentModelId":"m1"}}}"#,
        );
        let ask_model = second.window_said(&asks(
            "r2",
            ControlRequest::SetModel {
                model: "m2".to_owned(),
            },
        ));
        let rpc = agent_lines(&ask_model)
            .into_iter()
            .find(|line| line.get("method") == Some(&json!("session/set_model")))
            .and_then(|line| line.get("id").and_then(Value::as_u64))
            .expect("set_model rpc id");
        let failed_set = second.agent_said(&format!(
            r#"{{"jsonrpc":"2.0","id":{rpc},"error":{{"code":-32603,"message":"denied"}}}}"#
        ));
        assert_eq!(
            window_lines(&failed_set)
                .first()
                .and_then(|line| line.pointer("/response/subtype")),
            Some(&json!("error"))
        );
        assert_eq!(second.model.as_deref(), Some("gemini-3.8-flash-high"));
    }

    #[test]
    fn available_commands_and_config_options_reach_the_window() {
        let mut translator = translator_for(Agent::Opencode, Start::Resume("ses_1".to_owned()));
        translator.start();
        translator.window_said(&asks(
            "r1",
            ControlRequest::Initialize {
                per_task_stop_affordance: true,
            },
        ));
        translator.agent_said(r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#);
        let cmd_during_load = translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"ses_1","update":{"sessionUpdate":"available_commands_update","availableCommands":[{"name":"review","description":"review changes","input":{"hint":"[branch]"}}]}}}"#,
        );
        assert!(cmd_during_load.is_empty());

        let opened = translator.agent_said(
            r#"{"jsonrpc":"2.0","id":2,"result":{"configOptions":[{"id":"model","name":"Model","category":"model","type":"select","currentValue":"opencode/big-pickle","options":[{"value":"opencode/big-pickle","name":"Big Pickle"},{"value":"opencode/beam-free","name":"Beam Free"}]},{"id":"mode","name":"Session Mode","category":"mode","type":"select","currentValue":"build","options":[{"value":"build","name":"Build","description":"Default"},{"value":"plan","name":"Plan","description":"Read-only"}]}]}}"#,
        );
        let init_answer = window_lines(&opened)
            .into_iter()
            .find(|line| line.get("type") == Some(&json!("control_response")))
            .expect("initialize answered");
        assert_eq!(
            init_answer.pointer("/response/response/commands"),
            Some(
                &json!([{"name":"review","description":"review changes","argumentHint":"[branch]"}])
            )
        );
        assert_eq!(
            init_answer.pointer("/response/response/models/1/value"),
            Some(&json!("opencode/beam-free"))
        );
        assert_eq!(
            init_answer.pointer("/response/response/programModes/current"),
            Some(&json!("build"))
        );

        let ask_model = translator.window_said(&asks(
            "r2",
            ControlRequest::SetModel {
                model: "opencode/beam-free".to_owned(),
            },
        ));
        let set_opt = agent_lines(&ask_model)
            .into_iter()
            .find(|line| line.get("method") == Some(&json!("session/set_config_option")))
            .expect("set_config_option called for model");
        assert_eq!(
            set_opt.get("params"),
            Some(&json!({"sessionId":"ses_1","configId":"model","value":"opencode/beam-free"}))
        );

        let config_update = translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"ses_1","update":{"sessionUpdate":"config_option_update","configOptions":[{"id":"mode","category":"mode","currentValue":"plan","options":[]}]}}}"#,
        );
        assert_eq!(
            window_lines(&config_update)
                .first()
                .map(|line| (line.get("subtype"), line.get("mode"))),
            Some((Some(&json!("program_mode")), Some(&json!("plan"))))
        );
    }

    #[test]
    fn plan_updates_become_todowrite_calls() {
        let mut translator = opened_with(Agent::Opencode);
        let planned = translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"plan","entries":[{"content":"Read code","status":"completed"},{"content":"Fix bug","status":"in_progress"}]}}}"#,
        );
        let window = window_lines(&planned);
        assert_eq!(kinds(&window), ["assistant", "user"]);
        assert_eq!(
            window
                .first()
                .and_then(|line| line.pointer("/message/content/0/name")),
            Some(&json!("TodoWrite"))
        );
        assert_eq!(
            window
                .first()
                .and_then(|line| line.pointer("/message/content/0/input/todos/1/status")),
            Some(&json!("in_progress"))
        );
        assert_eq!(
            window
                .get(1)
                .and_then(|line| line.pointer("/message/content/0/type")),
            Some(&json!("tool_result"))
        );
    }

    /// What an update adds to a call shown reaches the window as the call, changed, its end
    /// included.
    #[test]
    fn a_calls_update_shows_what_it_adds() {
        let mut translator = opened_with(Agent::Opencode);
        let shown = translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call","toolCallId":"c1","name":"edit","title":"edit","kind":"edit","status":"in_progress","rawInput":{"filePath":"a.rs","oldString":"x","newString":"y"}}}}"#,
        );
        let window = window_lines(&shown);
        assert_eq!(kinds(&window), ["tool_call"]);
        assert!(call_of(&window).locations.is_empty());
        let updated = translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"c1","status":"completed","locations":[{"path":"/w/a.rs"}]}}}"#,
        );
        let window = window_lines(&updated);
        assert_eq!(kinds(&window), ["tool_call"]);
        let call = call_of(&window);
        assert_eq!(call.locations, [PathBuf::from("/w/a.rs")]);
        assert_eq!((call.status, call.output), (AcpStatus::Completed, None));
    }

    /// The agent's "always" is offered as its own choice, and chosen by its id.
    #[test]
    fn the_agents_always_is_a_choice_of_its_own() {
        let mut translator = opened();
        translator.window_said(&prompt("go"));
        let asked = translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/request_permission","params":{"sessionId":"s","toolCall":{"toolCallId":"c9","name":"run_command","kind":"execute","status":"pending","title":"ls","rawInput":{"CommandLine":"ls"}},"options":[{"optionId":"once","name":"Allow","kind":"allow_once"},{"optionId":"always","name":"Always","kind":"allow_always"}]},"id":"9"}"#,
        );
        let (request, _, options) = permission_of(&window_lines(&asked));
        assert_eq!(
            options
                .iter()
                .map(|option| (option.name.as_str(), option.kind))
                .collect::<Vec<_>>(),
            [
                ("Allow", AcpOptionKind::AllowOnce),
                ("Always", AcpOptionKind::AllowAlways)
            ]
        );
        let answered = translator.window_said(&chose(request, "always"));
        assert_eq!(
            agent_lines(&answered)
                .first()
                .and_then(|line| line.pointer("/result/outcome/optionId")),
            Some(&json!("always"))
        );
    }

    #[test]
    fn fork_and_resume_replay_and_ready_commands_emit_expected_effects() {
        // Fork sends session/fork, and the session goes by the id it answers with.
        let mut forking = translator_for(Agent::Opencode, Start::Fork("ses_old".to_owned()));
        forking.start();
        let ask_fork =
            forking.agent_said(r#"{"jsonrpc":"2.0","result":{"protocolVersion":1},"id":1}"#);
        assert_eq!(
            agent_lines(&ask_fork)
                .first()
                .and_then(|line| line.pointer("/method")),
            Some(&json!("session/fork"))
        );
        let forked =
            forking.agent_said(r#"{"jsonrpc":"2.0","result":{"sessionId":"ses_new"},"id":2}"#);
        let new = SessionId::for_external(crate::provider::Backend::Opencode, "ses_new");
        assert!(
            forked
                .iter()
                .any(|effect| matches!(effect, Effect::Named(id) if *id == new))
        );

        // Ready available_commands_update emits System::Commands to the window.
        let ready_cmds = forking.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"ses_new","update":{"sessionUpdate":"available_commands_update","availableCommands":[{"name":"review","description":"Review changes"}]}}}"#,
        );
        assert!(ready_cmds.iter().any(|effect| matches!(
            effect,
            Effect::ToWindow(Event::System(System::Commands { commands }))
                if commands.iter().any(|c| c.name == "review")
        )));

        // ResumeReplay shows the window the user's and the agent's turns session/load replays.
        let mut replaying =
            translator_for(Agent::Opencode, Start::ResumeReplay("ses_ext".to_owned()));
        replaying.start();
        replaying.agent_said(r#"{"jsonrpc":"2.0","result":{"protocolVersion":1},"id":1}"#);
        let u = replaying.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"ses_ext","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"Hi"}}}}"#,
        );
        assert!(u.is_empty());
        let a = replaying.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"ses_ext","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Hello!"}}}}"#,
        );
        assert!(
            a.iter()
                .any(|effect| matches!(effect, Effect::ToWindow(Event::User(_))))
        );
        let loaded = replaying.agent_said(r#"{"jsonrpc":"2.0","result":{},"id":2}"#);
        assert!(
            loaded
                .iter()
                .any(|effect| matches!(effect, Effect::ToWindow(Event::Assistant { .. })))
        );
    }

    #[test]
    fn model_switch_reprojects_cached_quotas_and_mid_turn_tools_refresh_quotas() {
        let mut translator = opened();
        translator.models.push(acp::Model {
            model_id: "orchestrator-argon".to_owned(),
            name: "Orchestrator (Argon)".to_owned(),
        });
        // Prime cached quotas while on Gemini Flash (both 5h and 7d).
        drop(translator.quotas_told(None, Ok(quotas())));

        // Switch to Orchestrator (Argon): weekly-only bucket (100% used in fixture) clears 5h.
        let ask = translator.window_said(&asks(
            "m1",
            ControlRequest::SetModel {
                model: "orchestrator-argon".to_owned(),
            },
        ));
        let rpc_id = agent_lines(&ask)
            .first()
            .and_then(|line| line.get("id").and_then(Value::as_u64))
            .expect("set_model rpc id");
        let switched = translator.agent_said(&format!(
            r#"{{"jsonrpc":"2.0","id":{rpc_id},"result":{{}}}}"#
        ));
        let rate_limits = window_lines(&switched)
            .into_iter()
            .find(|line| line.get("type") == Some(&json!("rate_limit_event")))
            .expect("reprojected rate limits");
        assert_eq!(
            rate_limits.pointer("/rate_limit_info/unifiedWindows/five_hour"),
            Some(&Value::Null)
        );
        assert_eq!(
            rate_limits.pointer("/rate_limit_info/unifiedWindows/seven_day/utilization"),
            Some(&json!(1.0))
        );

        // Completing 5 tool calls mid-turn triggers a quota refresh.
        drop(translator.window_said(&prompt("research")));
        let mut refreshed = false;
        for i in 1..=5 {
            let start = format!(
                r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s","update":{{"sessionUpdate":"tool_call","toolCallId":"t{i}","name":"read_file","kind":"read","status":"in_progress","rawInput":{{"Path":"/work/a.rs"}}}}}}}}"#
            );
            drop(translator.agent_said(&start));
            let done = format!(
                r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s","update":{{"sessionUpdate":"tool_call_update","toolCallId":"t{i}","status":"completed","content":[]}}}}}}"#
            );
            let effects = translator.agent_said(&done);
            if effects
                .iter()
                .any(|effect| matches!(effect, Effect::Quotas { request: None }))
            {
                refreshed = true;
            }
        }
        assert!(
            refreshed,
            "5 completed tools should trigger mid-turn quota refresh"
        );
    }
}
