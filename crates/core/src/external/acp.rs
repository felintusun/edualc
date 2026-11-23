//! The Agent Client Protocol (agentclientprotocol.com) as edualc's ACP adapter reads it:
//! JSON-RPC 2.0, one message per line on stdio. Only what the adapter reads and writes: the
//! messages it reads are [`Incoming`], the ones it writes are [`ToAgent`]. Agents add keys of
//! their own (a tool call's `name` and its arguments by the agent's names), read where they say
//! more than the protocol's.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::call::AcpKind;
use crate::external::wire::MediaType;

/// A line from the agent: its answer to a request of ours, a request of its own, or news.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Incoming {
    Request {
        id: Value,
        method: AgentMethod,
        #[serde(default)]
        params: Value,
    },
    Notification {
        method: AgentMethod,
        #[serde(default)]
        params: Value,
    },
    Response {
        id: Value,
        #[serde(default)]
        result: Option<Value>,
        #[serde(default)]
        error: Option<RpcError>,
    },
}

/// What the agent asks or tells the client.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub enum AgentMethod {
    #[serde(rename = "session/update")]
    SessionUpdate,
    #[serde(rename = "session/request_permission")]
    RequestPermission,
    /// A form for the user to fill in: agy asks its questions so (see [`Elicitation`]).
    #[serde(rename = "elicitation/create")]
    ElicitationCreate,
    #[serde(untagged)]
    Other(String),
}

/// An `elicitation/create` request's parameters, as agy 1.5.9 sends them for its
/// `ask_question` tool: a form of one field per question (`q1`, `q2`, …), its options as
/// `const` values with their labels as `title`, and a free-text field beside each
/// (`q1_other`).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Elicitation {
    /// The tool call it asks for.
    #[serde(default)]
    pub tool_call_id: Option<String>,
    pub mode: ElicitationMode,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub requested_schema: FormSchema,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ElicitationMode {
    Form,
    #[serde(other)]
    Other,
}

/// The form's fields, in the order the agent gave them.
#[derive(Debug, Default, Deserialize)]
pub struct FormSchema {
    #[serde(default)]
    pub properties: indexmap::IndexMap<String, FormField>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormField {
    #[serde(rename = "type")]
    pub kind: FieldKind,
    #[serde(default)]
    pub title: Option<String>,
    /// The options of a field that takes one.
    #[serde(default)]
    pub one_of: Vec<FormOption>,
    /// The options of a field that takes several.
    #[serde(default)]
    pub items: Option<FormItems>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldKind {
    String,
    Array,
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormItems {
    #[serde(default)]
    pub any_of: Vec<FormOption>,
}

/// An option: the value the form takes, and the label shown.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FormOption {
    #[serde(rename = "const")]
    pub value: String,
    pub title: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RpcError {
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub message: String,
}

impl RpcError {
    /// ACP's error for a request the agent can't serve before the client authenticates.
    const AUTH_REQUIRED: i64 = -32000;

    /// Whether the agent refused for want of a valid sign-in, as ACP's code says.
    pub fn sign_in_refused(&self) -> bool {
        self.code == Self::AUTH_REQUIRED
    }
}

/// `session/update`'s parameters.
#[derive(Debug, Deserialize)]
pub struct Notified {
    pub update: Change,
}

/// What changed in a session.
#[derive(Debug, Deserialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub enum Change {
    AgentMessageChunk {
        content: Chunk,
    },
    AgentThoughtChunk {
        content: Chunk,
    },
    /// The user's prompt, as the agent took it; a session loaded replays them.
    UserMessageChunk {
        #[serde(default)]
        content: Option<Chunk>,
    },
    ToolCall(ToolCall),
    ToolCallUpdate(ToolCall),
    SessionInfoUpdate {
        #[serde(default)]
        title: Option<String>,
    },
    CurrentModeUpdate {
        #[serde(rename = "currentModeId")]
        current_mode_id: String,
    },
    AvailableCommandsUpdate {
        #[serde(rename = "availableCommands", default)]
        available_commands: Vec<AvailableCommand>,
    },
    ConfigOptionUpdate {
        #[serde(rename = "configOptions", default)]
        config_options: Vec<ConfigOption>,
    },
    Plan {
        #[serde(default)]
        entries: Vec<PlanEntry>,
    },
    /// How much of the model's context window the session uses, and the window's size, in
    /// tokens.
    UsageUpdate {
        used: u64,
        size: u64,
    },
    #[serde(other)]
    Other,
}

/// A slash command offered by an ACP agent.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableCommand {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub input: Option<AvailableCommandInput>,
}

/// Hint for a slash command's arguments.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableCommandInput {
    #[serde(default)]
    pub hint: String,
}

/// One step of an agent's plan, sent in a `plan` session update.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanEntry {
    pub content: String,
    #[serde(default)]
    pub status: PlanStatus,
}

/// Whether a plan step waits, runs, or finished.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
    #[serde(other)]
    Other,
}

/// A piece of a message.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Chunk {
    Text {
        text: String,
    },
    #[serde(other)]
    Other,
}

/// A tool call, as first told or as it changed: an update carries only what changed.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub tool_call_id: String,
    /// agy's own name for the tool (`run_command`, `edit_file`).
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub kind: Option<ToolKind>,
    #[serde(default)]
    pub status: Option<ToolStatus>,
    #[serde(default)]
    pub raw_input: Option<Value>,
    #[serde(default)]
    pub raw_output: Option<Value>,
    #[serde(default)]
    pub content: Option<Vec<ToolContent>>,
    #[serde(default)]
    pub locations: Option<Vec<Location>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    #[serde(other)]
    Other,
}

impl From<ToolKind> for AcpKind {
    fn from(kind: ToolKind) -> Self {
        match kind {
            ToolKind::Read => Self::Read,
            ToolKind::Edit => Self::Edit,
            ToolKind::Delete => Self::Delete,
            ToolKind::Move => Self::Move,
            ToolKind::Search => Self::Search,
            ToolKind::Execute => Self::Execute,
            ToolKind::Think => Self::Think,
            ToolKind::Fetch => Self::Fetch,
            ToolKind::Other => Self::Other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

/// What a tool call produced.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolContent {
    Content {
        content: Chunk,
    },
    /// A file changed: a new file has no old text.
    #[serde(rename_all = "camelCase")]
    Diff {
        path: PathBuf,
        #[serde(default)]
        old_text: Option<String>,
        new_text: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Location {
    pub path: PathBuf,
}

/// A tool call's `rawOutput` when it is what a command printed, as agy gives it; other tools'
/// outputs have shapes of their own.
#[derive(Debug, Deserialize)]
pub struct PrintedOutput {
    pub output: String,
}

/// A tool call's arguments (`rawInput`) as agy 1.5.6+ names them, each as far as agy has
/// given it.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AgyArguments {
    pub path: Option<PathBuf>,
    pub absolute_path: Option<PathBuf>,
    pub target_file: Option<PathBuf>,
    pub search_path: Option<PathBuf>,
    pub search_directory: Option<PathBuf>,
    pub directory_path: Option<PathBuf>,
    pub command_line: Option<String>,
    #[serde(rename = "toolSummary")]
    pub tool_summary: Option<String>,
    pub content: Option<String>,
    pub code_content: Option<String>,
    pub append: Option<bool>,
    pub target: Option<String>,
    pub replacement: Option<String>,
    pub target_content: Option<String>,
    pub replacement_content: Option<String>,
    #[serde(default)]
    pub replacement_chunks: Vec<AgyReplacementChunk>,
    pub start_line: Option<u64>,
    pub end_line: Option<u64>,
    #[serde(alias = "query")]
    pub query: Option<String>,
    pub pattern: Option<String>,
    #[serde(default)]
    pub includes: Vec<String>,
    pub url: Option<String>,
    /// edualc's messaging tool's, by its own names.
    #[serde(rename = "to")]
    pub to: Option<String>,
    #[serde(rename = "message")]
    pub message: Option<String>,
    /// `post_message`'s, to a swarm's board.
    #[serde(rename = "text")]
    pub text: Option<String>,
    /// `swarm`'s: what it does to the swarm, and with `add` the member's name and brief.
    #[serde(rename = "action")]
    pub action: Option<SwarmAction>,
    #[serde(rename = "name")]
    pub name: Option<String>,
    #[serde(rename = "brief")]
    pub brief: Option<String>,
    /// `invoke_subagent`'s.
    #[serde(rename = "subagent_name")]
    pub subagent_name: Option<String>,
    #[serde(rename = "prompt")]
    pub prompt: Option<String>,
    /// `ask_question`'s.
    #[serde(rename = "questions", default)]
    pub questions: Vec<AgyQuestion>,
}

/// One replacement chunk of agy's `multi_replace_file_content`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AgyReplacementChunk {
    #[serde(default)]
    pub target_content: String,
    #[serde(default)]
    pub replacement_content: String,
}

/// What agy's `swarm` tool does, as far as edualc shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmAction {
    /// A member joins, with a brief.
    Add,
    #[serde(other)]
    Other,
}

/// A question agy's `ask_question` asks.
#[derive(Debug, Deserialize)]
pub struct AgyQuestion {
    pub question: String,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub is_multi_select: bool,
}

/// A tool call's arguments (`rawInput`) as opencode names them, each as far as opencode has
/// given it.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpencodeArguments {
    pub path: Option<PathBuf>,
    #[serde(alias = "file_path")]
    pub file_path: Option<PathBuf>,
    pub command: Option<String>,
    pub description: Option<String>,
    pub content: Option<String>,
    #[serde(alias = "old_string")]
    pub old_string: Option<String>,
    #[serde(alias = "new_string")]
    pub new_string: Option<String>,
    #[serde(alias = "replace_all")]
    pub replace_all: Option<bool>,
    pub offset: Option<u64>,
    pub limit: Option<u64>,
    pub pattern: Option<String>,
    pub include: Option<String>,
    pub url: Option<String>,
    pub query: Option<String>,
    pub prompt: Option<String>,
    #[serde(alias = "subagent_type")]
    pub subagent_type: Option<String>,
    pub name: Option<String>,
    #[serde(default)]
    pub todos: Vec<PlanEntry>,
    #[serde(default)]
    pub questions: Vec<OpencodeQuestion>,
}

/// A question opencode's `question` tool asks.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpencodeQuestion {
    pub question: String,
    #[serde(default)]
    pub header: String,
    #[serde(default)]
    pub options: Vec<OpencodeQuestionOption>,
    #[serde(default, alias = "is_multi_select")]
    pub multiple: bool,
}

/// An option of opencode's `question` tool (an object with `label`, or a plain string).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum OpencodeQuestionOption {
    Detailed {
        label: String,
        #[serde(default)]
        description: String,
    },
    Plain(String),
}

/// Fallback tool call arguments read across ACP naming conventions when an unknown tool is
/// classified by `ToolKind` or normalized name.
#[derive(Debug, Default, Deserialize)]
pub struct GenericArguments {
    #[serde(alias = "CommandLine", alias = "cmd")]
    pub command: Option<String>,
    #[serde(alias = "toolSummary")]
    pub description: Option<String>,
    #[serde(
        alias = "file_path",
        alias = "filePath",
        alias = "Path",
        alias = "AbsolutePath",
        alias = "TargetFile",
        alias = "DirectoryPath",
        alias = "SearchPath",
        alias = "SearchDirectory"
    )]
    pub path: Option<PathBuf>,
    #[serde(alias = "query", alias = "Query", alias = "Pattern")]
    pub pattern: Option<String>,
    #[serde(alias = "include")]
    pub glob: Option<String>,
    #[serde(alias = "Url")]
    pub url: Option<String>,
    #[serde(alias = "Content", alias = "CodeContent")]
    pub content: Option<String>,
    #[serde(alias = "oldString", alias = "Target", alias = "TargetContent")]
    pub old_string: Option<String>,
    #[serde(
        alias = "newString",
        alias = "Replacement",
        alias = "ReplacementContent"
    )]
    pub new_string: Option<String>,
    #[serde(alias = "StartLine")]
    pub offset: Option<u64>,
    pub limit: Option<u64>,
}

/// `session/request_permission`'s parameters.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionAsked {
    pub tool_call: ToolCall,
    pub options: Vec<PermissionOption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    pub option_id: String,
    pub kind: OptionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

/// `session/new`'s and `session/load`'s answer, as far as it is read.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Opened {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub models: Option<Models>,
    #[serde(default)]
    pub modes: Option<SessionModes>,
    #[serde(default)]
    pub config_options: Vec<ConfigOption>,
}

/// An ACP session configuration option (model, mode, thought level) returned by `session/new`,
/// `session/load`, `session/set_config_option`, or `config_option_update`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOption {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub current_value: Option<String>,
    #[serde(default)]
    pub options: Vec<ConfigOptionChoice>,
}

impl ConfigOption {
    /// All selectable choices in order, flattening option groups if present.
    pub fn choices(&self) -> Vec<&ConfigSelectOption> {
        let mut flat = Vec::new();
        for choice in &self.options {
            match choice {
                ConfigOptionChoice::Item(option) => flat.push(option),
                ConfigOptionChoice::Group(group) => flat.extend(group.options.iter()),
            }
        }
        flat
    }
}

/// Either a single selectable option or a named group of options.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ConfigOptionChoice {
    Item(ConfigSelectOption),
    Group(ConfigOptionGroup),
}

/// One choice inside an ACP `ConfigOption`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSelectOption {
    pub value: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// A group of choices inside an ACP `ConfigOption`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOptionGroup {
    #[serde(default)]
    pub options: Vec<ConfigSelectOption>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Models {
    #[serde(default)]
    pub available_models: Vec<Model>,
    #[serde(default)]
    pub current_model_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub model_id: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionModes {
    #[serde(default)]
    pub available_modes: Vec<SessionMode>,
    #[serde(default)]
    pub current_mode_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMode {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// `session/list`'s answer.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListSessionsResult {
    #[serde(default)]
    pub sessions: Vec<ListedSession>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// One session returned by `session/list`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListedSession {
    pub session_id: String,
    pub cwd: PathBuf,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// `session/prompt`'s answer: why the turn ended, and what it used.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Prompted {
    pub stop_reason: StopReason,
    #[serde(default)]
    pub usage: Option<PromptUsage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cached_read_tokens: u64,
}

/// The `jsonrpc` field of every message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum JsonRpc {
    #[serde(rename = "2.0")]
    TwoPointZero,
}

/// A request edualc makes of the agent, by its method and parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "method", content = "params")]
pub enum ClientCall {
    #[serde(rename = "initialize")]
    Initialize(InitializeParams),
    #[serde(rename = "session/new")]
    NewSession(NewSessionParams),
    #[serde(rename = "session/load")]
    LoadSession(LoadSessionParams),
    #[serde(rename = "session/fork")]
    ForkSession(ForkSessionParams),
    #[serde(rename = "session/list")]
    ListSessions(ListSessionsParams),
    #[serde(rename = "session/set_model")]
    SetModel(SetModelParams),
    #[serde(rename = "session/set_mode")]
    SetMode(SetModeParams),
    #[serde(rename = "session/set_config_option")]
    SetConfigOption(SetConfigOptionParams),
    #[serde(rename = "session/prompt")]
    Prompt(PromptParams),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub protocol_version: u32,
    pub client_capabilities: ClientCapabilities,
    pub client_info: ClientInfo,
}

/// What edualc lets the agent ask of it: forms, and none of the file and terminal requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    pub fs: FileCapabilities,
    pub terminal: bool,
    pub elicitation: ElicitationCapabilities,
}

/// The kinds of elicitation edualc answers: forms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ElicitationCapabilities {
    pub form: FormCapabilities,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FormCapabilities {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileCapabilities {
    pub read_text_file: bool,
    pub write_text_file: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionParams {
    pub cwd: PathBuf,
    pub mcp_servers: Vec<McpServer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadSessionParams {
    pub session_id: String,
    pub cwd: PathBuf,
    pub mcp_servers: Vec<McpServer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkSessionParams {
    pub session_id: String,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListSessionsParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetModelParams {
    pub session_id: String,
    pub model_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetModeParams {
    pub session_id: String,
    pub mode_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetConfigOptionParams {
    pub session_id: String,
    pub config_id: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptParams {
    pub session_id: String,
    pub prompt: Vec<PromptBlock>,
}

/// An MCP server the agent starts for the session: a program with its arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct McpServer {
    pub name: String,
    pub command: PathBuf,
    pub args: Vec<PathBuf>,
    pub env: Vec<EnvVariable>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EnvVariable {
    pub name: String,
    pub value: String,
}

/// A part of a prompt the agent is given.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum PromptBlock {
    Text { text: String },
    Image { mime_type: MediaType, data: String },
}

/// A message to the agent that asks for no answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "method", content = "params")]
pub enum ClientNotice {
    #[serde(rename = "session/cancel")]
    Cancel(CancelParams),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelParams {
    pub session_id: String,
}

/// The answer to a permission request: the option the user chose, or none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "outcome",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum Outcome {
    Selected { option_id: String },
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionAnswer {
    pub outcome: Outcome,
}

/// The answer to an elicitation: the form filled in, or turned down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum ElicitationAnswer {
    Accept {
        content: FormContent,
    },
    /// The user said no.
    Decline,
    /// Nobody answered.
    Cancel,
}

/// A form filled in: each field's value, by the field's name.
pub type FormContent = indexmap::IndexMap<String, FieldValue>;

/// What a field was filled in with: text, or the options picked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum FieldValue {
    Text(String),
    Picked(Vec<String>),
}

/// The `result` or the `error` of an answer to a request of the agent's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Reply<T> {
    Result(T),
    Error(RpcErrorBody),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RpcErrorBody {
    pub code: i64,
    pub message: String,
}

/// A request edualc makes; the agent answers with the same `id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Request {
    pub jsonrpc: JsonRpc,
    pub id: u64,
    #[serde(flatten)]
    pub call: ClientCall,
}

/// A message edualc sends that asks for no answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Notice {
    pub jsonrpc: JsonRpc,
    #[serde(flatten)]
    pub notice: ClientNotice,
}

/// An answer to a request of the agent's, under the `id` it came with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Answer<T> {
    pub jsonrpc: JsonRpc,
    pub id: Value,
    #[serde(flatten)]
    pub reply: Reply<T>,
}

/// What edualc writes to the agent, one line each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToAgent {
    Request(Request),
    Notice(Notice),
    Answer(Answer<PermissionAnswer>),
    Elicited(Answer<ElicitationAnswer>),
}

impl ToAgent {
    /// The message as one line of JSON, without its line break. Fails only when a path in it
    /// isn't UTF-8.
    pub fn to_line(&self) -> Result<String, serde_json::Error> {
        match self {
            Self::Request(request) => serde_json::to_string(request),
            Self::Notice(notice) => serde_json::to_string(notice),
            Self::Answer(answer) => serde_json::to_string(answer),
            Self::Elicited(answer) => serde_json::to_string(answer),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lines as agy 1.5.6 wrote them (paths shortened), read as what they are.
    #[test]
    fn agys_lines_read() {
        let update = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"call_1","name":"edit_file","args":{},"status":"completed","kind":"edit","title":"File notes.txt","rawInput":{"Path":"/w/notes.txt","Replacement":"b","Target":"a"},"rawOutput":{},"content":[{"type":"diff","path":"/w/notes.txt","newText":"b\n","oldText":"a\n"}],"locations":[{"path":"/w/notes.txt"}]}}}"#;
        let Ok(Incoming::Notification {
            method: AgentMethod::SessionUpdate,
            params,
        }) = serde_json::from_str(update)
        else {
            panic!("a session update");
        };
        let notified: Notified = serde_json::from_value(params).expect("an update");
        let Change::ToolCallUpdate(call) = notified.update else {
            panic!("a tool call's update");
        };
        assert_eq!(call.name.as_deref(), Some("edit_file"));
        assert_eq!(call.status, Some(ToolStatus::Completed));
        assert!(matches!(
            call.content.as_deref(),
            Some([ToolContent::Diff { old_text, new_text, .. }])
                if old_text.as_deref() == Some("a\n") && new_text == "b\n"
        ));

        let asked = r#"{"jsonrpc":"2.0","method":"session/request_permission","params":{"sessionId":"s","toolCall":{"sessionUpdate":"tool_call","toolCallId":"call_2","name":"run_command","status":"pending","kind":"execute","title":"wc -l notes.txt","rawInput":{"CommandLine":"wc -l notes.txt"}},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"},{"optionId":"deny","name":"Deny","kind":"reject_once"}]},"id":"1"}"#;
        let Ok(Incoming::Request {
            method: AgentMethod::RequestPermission,
            params,
            ..
        }) = serde_json::from_str(asked)
        else {
            panic!("a permission request");
        };
        let asked: PermissionAsked = serde_json::from_value(params).expect("its parameters");
        assert_eq!(asked.options.len(), 2);

        let answered = r#"{"jsonrpc":"2.0","result":{"stopReason":"end_turn","usage":{"inputTokens":18742,"outputTokens":885,"cachedReadTokens":0}},"id":5}"#;
        let Ok(Incoming::Response {
            result: Some(result),
            ..
        }) = serde_json::from_str(answered)
        else {
            panic!("an answer");
        };
        let prompted: Prompted = serde_json::from_value(result).expect("a prompt's answer");
        assert_eq!(prompted.stop_reason, StopReason::EndTurn);

        let opened_json = r#"{"sessionId":"s","modes":{"availableModes":[{"id":"default","name":"Default","description":"Ask"},{"id":"accept-edits","name":"Accept edits"}],"currentModeId":"default"}}"#;
        let opened: Opened = serde_json::from_str(opened_json).expect("opened session with modes");
        let modes = opened.modes.expect("modes present");
        assert_eq!(modes.available_modes.len(), 2);
        assert_eq!(
            modes.available_modes.get(1).map(|mode| mode.id.as_str()),
            Some("accept-edits")
        );
        assert_eq!(modes.current_mode_id.as_deref(), Some("default"));

        let mode_update = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"current_mode_update","currentModeId":"accept-edits"}}}"#;
        let Ok(Incoming::Notification {
            method: AgentMethod::SessionUpdate,
            params,
        }) = serde_json::from_str(mode_update)
        else {
            panic!("a session update");
        };
        let notified: Notified = serde_json::from_value(params).expect("an update");
        let Change::CurrentModeUpdate { current_mode_id } = notified.update else {
            panic!("a current mode update");
        };
        assert_eq!(current_mode_id, "accept-edits");

        let commands_update = r#"{"sessionUpdate":"available_commands_update","availableCommands":[{"name":"review","description":"review changes","input":{"hint":"[branch]"}}]}"#;
        let Change::AvailableCommandsUpdate { available_commands } =
            serde_json::from_str(commands_update).expect("available commands update")
        else {
            panic!("available commands");
        };
        assert_eq!(available_commands.len(), 1);
        assert_eq!(
            available_commands.first().map(|cmd| cmd.name.as_str()),
            Some("review")
        );
        assert_eq!(
            available_commands
                .first()
                .and_then(|cmd| cmd.input.as_ref())
                .map(|input| input.hint.as_str()),
            Some("[branch]")
        );

        let plan_update = r#"{"sessionUpdate":"plan","entries":[{"content":"Inspect files","priority":"high","status":"completed"},{"content":"Write tests","status":"in_progress"}]}"#;
        let Change::Plan { entries } = serde_json::from_str(plan_update).expect("plan update")
        else {
            panic!("plan");
        };
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries.get(1).map(|entry| entry.status),
            Some(PlanStatus::InProgress)
        );

        let opencode_opened = r#"{"sessionId":"ses_1","configOptions":[{"id":"model","name":"Model","category":"model","type":"select","currentValue":"opencode/big-pickle","options":[{"value":"opencode/big-pickle","name":"Big Pickle"}]},{"id":"mode","name":"Session Mode","category":"mode","type":"select","currentValue":"build","options":[{"value":"build","name":"Build","description":"Default"}]}]}"#;
        let opened: Opened = serde_json::from_str(opencode_opened).expect("opencode opened");
        assert_eq!(opened.config_options.len(), 2);
        assert_eq!(
            opened
                .config_options
                .first()
                .and_then(|opt| opt.current_value.as_deref()),
            Some("opencode/big-pickle")
        );

        let listed_json = r#"{"sessions":[{"sessionId":"ses_1","cwd":"/w","title":"Work","updatedAt":"2026-11-19T21:00:00Z"}],"nextCursor":"c2"}"#;
        let listed: ListSessionsResult =
            serde_json::from_str(listed_json).expect("list sessions result");
        assert_eq!(listed.sessions.len(), 1);
        assert_eq!(listed.next_cursor.as_deref(), Some("c2"));
    }
}
