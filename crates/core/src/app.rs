//! The one owner of everything the window shows: [`Event`]s reach it on one channel, and it
//! draws what changed once per batch.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_channel::{Receiver, Sender};

use crate::agent::{Agent, Output};
use crate::chat::{Change, Chat, ImageKey};
use crate::decode::{Picture, Pixels};
use crate::external::wire::{self, ControlRequest, ToolName, TranscriptLine};
use crate::folder::Folder;
use crate::ids::{RequestId, SessionId};
use crate::ipc::{
    self, Act, ActFailure, ActId, CreateProjectError, Generation, Hosted, PasteFailure, Pasted,
    ProjectOverview, Read, Target, ToDaemon, Watched,
};
use crate::link::{self, Link};
use crate::overview;
use crate::present::model::PlaceView;
use crate::present::{Command, Frontend, OverviewView, Presenter, Seat};
use crate::provider;
use crate::repeat::Repeat;
use crate::sessions::{
    self, LiveSession, LiveStatus, MessagingName, ProjectInfo, SessionInfo, Worktree,
};
use crate::settings::{self, Settings, Zoom};
use crate::sidebar;

mod api_key;
mod asks;
mod changes;
mod composer;
mod folder_trust;
#[cfg_attr(target_arch = "wasm32", path = "app/web_images.rs")]
mod images;
mod limits;
mod modes;
mod peek;
mod places;
mod process;
mod protocol;
mod session;
mod side;
mod tasks;
mod usage;
mod worktree;

pub use asks::{AnswerError, Ask, AskKind, answer_as};
pub use changes::{ChangesView, LineComment, Listed, Picked, comments_on, rows as diff_rows};
pub use modes::{ChosenMode, Mode, Modes, PermissionMode};
pub use peek::{CommandOutput, Conversation, Followed, Peek, PeekView, Step, StepKind};
pub use process::SignIn;
pub use side::{Answer, SideChat, SideView};
pub use tasks::{Place, Running, TaskProgress, Tasks};

use composer::Recall;
use folder_trust::{ClaudeTrustAsk, TrustGate};

#[derive(Debug)]
pub enum Event {
    Action(Action),
    Agent {
        generation: Generation,
        output: Output,
    },
    SessionsListed {
        cwd: PathBuf,
        sessions: Vec<SessionInfo>,
    },
    ProjectsListed(Vec<ProjectInfo>),
    /// A saved session read from its start, with where its transcript is and how far it was
    /// read.
    SessionLoaded {
        id: SessionId,
        path: PathBuf,
        result: Result<(Chat, u64), ipc::Unread>,
    },
    /// What a watched session's transcript says after what is shown.
    TranscriptGrew {
        id: SessionId,
        result: Result<(Vec<TranscriptLine>, u64), ipc::Unread>,
    },
    /// What the transcript of the subagent peeked into says after what is shown.
    PeekGrew {
        task: wire::TaskId,
        result: Result<(Vec<TranscriptLine>, u64), ipc::Unread>,
    },
    /// What the daemon read for `App::read`, with a transcript's lines parsed.
    Answered {
        request: ActId,
        answer: ipc::Answer,
        lines: Vec<TranscriptLine>,
    },
    /// Whether `cwd` is a linked git worktree. `replace` is set when the process said it works
    /// there, which overrides what was known.
    LinkedWorktreeRead {
        cwd: PathBuf,
        worktree: Option<Worktree>,
        replace: bool,
    },
    /// The folder to open as the daemon resolved it, or why it isn't one. `typed` is set when it
    /// was typed into the folder list, which then shows the problem.
    FolderRead {
        typed: bool,
        result: Result<PathBuf, String>,
    },
    LiveListed(Vec<LiveSession>),
    /// The daemon's answer to an `Act`.
    Acted {
        request: ActId,
        result: Result<(), ActFailure>,
    },
    /// The folder picked in the desktop's folder picker, if one was.
    Picked {
        request: ActId,
        folder: Option<PathBuf>,
    },
    /// Whether the terminal's trust dialog was accepted for the folder asked about.
    Trusted {
        request: ActId,
        trust: crate::ipc::FolderTrust,
    },
    /// Whether Claude Code trusts the folder, as asked by `Act::CheckClaudeTrust`.
    ClaudeTrustRead {
        request: ActId,
        trusted: Result<bool, ActFailure>,
    },
    /// The clipboard's image, decoded in the confined helper, or why there is none.
    Pasted {
        request: ActId,
        pasted: Pasted,
    },
    /// An image of the conversation, decoded in the confined helper to show, or why it isn't.
    ImageShown {
        key: ImageKey,
        shown: Result<Pixels, images::ShowError>,
    },
    /// Code was coloured on its own thread and is ready to draw (`Command::DrawColouredCode`).
    CodeColoured,
    /// Why the frontend couldn't do something on its desktop (see `present::Seat`).
    SeatFailed(String),
    /// The daemon is gone, and with it the sessions it ran for this window.
    DaemonLost,
    /// A window asked the daemon to shut down; its sessions end, then the daemon.
    DaemonShuttingDown,
    /// The sessions the daemon runs, sent again whenever they change.
    Hosted(Vec<Hosted>),
    /// The worktree a session's transcript says it works in.
    WorktreeRead {
        id: SessionId,
        worktree: Option<Worktree>,
    },
    /// What a session changed in `scope`, or why it wasn't read.
    ChangesRead {
        session: SessionId,
        scope: crate::changes::Scope,
        result: Result<crate::changes::ChangeSet, String>,
    },
    /// A changed file's diff, or why it wasn't read.
    DiffRead {
        session: SessionId,
        root: PathBuf,
        path: PathBuf,
        result: Result<crate::changes::Diff, String>,
    },
    /// Something watched changed: the running sessions' records, or a followed transcript.
    Changed(Watched),
    /// Sent now and then, to let go of sessions whose process ended without saying so.
    Tick,
    /// Sent during a turn, to check whether Claude Code has gone quiet.
    Listen,
    /// Sent every second while a command is followed, to read what it printed since.
    PollOutput,
    /// A usage limit of the plan started over (see [`limits`]).
    LimitReset,
    /// Whether the keyring keeps an Anthropic API key, or why that couldn't be told.
    ApiKeyStored(Result<bool, ActFailure>),
    /// Known projects and their recent sessions, for the start page and switcher.
    OverviewListed(Vec<ProjectOverview>),
    /// The daemon's answer to `Act::CreateProject`.
    ProjectCreated {
        request: ActId,
        result: Result<PathBuf, CreateProjectError>,
    },
    /// A change to the settings wasn't kept, and why.
    SettingsNotKept(String),
    /// A program's usage, as the usage panel asked for it.
    ProviderUsageRead {
        backend: provider::Backend,
        usage: ipc::ProviderUsage,
    },
    /// The network line to edualcd was lost and is being reached again. What was asked on it
    /// goes unanswered; the sessions it ran for this window run on there.
    LineLost,
    /// The line to edualcd is back, with the sessions it runs and those running now.
    Reconnected {
        hosted: Vec<Hosted>,
        live: Vec<LiveSession>,
    },
}

/// A prompt as sent, to send again once the sign-in works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentPrompt {
    text: String,
    images: Vec<Picture>,
}

/// What the window knows of its daemon's life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Daemon {
    Running,
    /// A window asked it to shut down, so its end isn't reported as a loss.
    ShuttingDown,
}

/// Where the window is: the start page, a project's session list, or a session.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Location {
    Start,
    Project(PathBuf),
    Session(PathBuf),
}

impl Location {
    fn cwd(&self) -> Option<&Path> {
        match self {
            Self::Start => None,
            Self::Project(path) | Self::Session(path) => Some(path.as_path()),
        }
    }

    fn view(&self) -> PlaceView {
        match self {
            Self::Start => PlaceView::Start,
            Self::Project(_) => PlaceView::Project,
            Self::Session(_) => PlaceView::Session,
        }
    }
}

/// Which field a folder picker answer updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickingTarget {
    OpenFolder,
    NewProjectParent,
    DefaultProjectsDir,
}

/// A session to open from the start page or quick switcher, with its project's folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTarget {
    pub folder: PathBuf,
    pub opens: overview::SessionRef,
}

/// What a request to the daemon was for.
#[derive(Debug)]
enum Acting {
    /// Something the user asked of the daemon; only its failure is told.
    Desktop,
    /// The folder picker, for opening a folder or choosing a projects directory.
    Picking(PickingTarget),
    CreatingProject,
    /// Storing or removing the Anthropic API key in the keyring.
    ApiKey,
}

/// An API key as the user pasted it, not yet checked: cleared from memory once dropped, and
/// left out of what `Debug` writes, as [`crate::ipc::ApiKey`] is.
pub struct PastedKey(zeroize::Zeroizing<String>);

impl PastedKey {
    pub fn new(text: &str) -> Self {
        Self(zeroize::Zeroizing::new(text.to_owned()))
    }
}

impl std::fmt::Debug for PastedKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PastedKey(…)")
    }
}

/// What the user did in the window.
#[derive(Debug)]
pub enum Action {
    Send(String),
    Interrupt,
    /// Starts a new session of this program, signed in this way if it is Claude Code.
    NewSession(provider::Backend, provider::SignInWith),
    /// Starts a new session of this program from the project page.
    NewSessionInProject(provider::Backend, provider::SignInWith),
    /// Starts a Claude Code session with the API key, with this model (an alias Claude Code
    /// takes) or the model menu's.
    NewApiKeySession(Option<String>),
    /// Continues the session waiting for it, signed in this way; `None` doesn't continue it.
    ContinueWith(Option<provider::SignInWith>),
    /// Keeps an Anthropic API key, as pasted, in the keyring.
    StoreApiKey(PastedKey),
    RemoveApiKey,
    OpenSession(SessionId),
    /// Goes to the start page.
    OpenStart,
    /// Opens a project's session list without starting a session.
    EnterProject(PathBuf),
    /// Opens or attaches to a session in any project.
    JumpTo(SessionTarget),
    /// Goes to the next session waiting for you.
    NextWaiting,
    /// Goes back one level: from a session to its project, from a project to the start page.
    Back,
    /// Picks the New project dialog's parent folder in the desktop's picker.
    BrowseNewProjectParent,
    /// Makes a project folder, trusts it, and opens a session in it.
    CreateProject {
        parent: String,
        name: String,
    },
    ClearNewProjectError,
    /// Opens a folder typed into the folder list.
    OpenProject(String),
    BrowseProject,
    ListProjects,
    ChooseModel(usize),
    ChooseMode(usize),
    DraftEdited(String),
    /// Puts a command from the list matching the draft into the draft.
    PickCommand(usize),
    /// Shows the prompt before the one shown, or the newest (Up on the first line).
    RecallOlder,
    /// Shows the next prompt, or the draft again (Down on the last line).
    RecallNewer,
    /// Sends a command from the list matching the draft as it is.
    RunCommand(usize),
    Toggle(usize),
    CopyEntry(usize),
    Copy(String),
    OpenUrl(String),
    Allow(RequestId),
    AllowAlways(RequestId),
    Deny {
        request: RequestId,
        feedback: String,
    },
    ToggleOption {
        question: usize,
        option: usize,
    },
    SetOther {
        question: usize,
        text: String,
    },
    SetNotes {
        question: usize,
        text: String,
    },
    AnswerQuestions(RequestId),
    ApprovePlan {
        request: RequestId,
        mode: PermissionMode,
        overall: String,
    },
    EditComment(usize),
    SetComment {
        section: usize,
        text: String,
    },
    CancelComment,
    ContinueHere,
    GoToWindow,
    AskSide(String),
    ToggleSide,
    DismissSide,
    CopySide(usize),
    ShowChanges,
    CloseChanges,
    ChangesScope(crate::changes::Scope),
    SelectChangedFile(usize),
    ToggleReviewed,
    EditLineComment(usize),
    SetLineComment(usize, String),
    CancelLineComment,
    SendLineComments,
    /// Follows a subagent's conversation in the side panel.
    OpenPeek(wire::TaskId),
    ClosePeek,
    StopTask(wire::TaskId),
    StopAllTasks,
    SideToConversation,
    BranchSide,
    /// Opens the session's worktree in this window.
    OpenWorktree,
    Zoom(Zoom),
    /// Switches between the Gruvbox and the plain look.
    SwitchLook,
    /// Colours the code again, after the look or the desktop's dark or light changed.
    RecolourCode,
    /// Sets the key that sends the draft.
    SendWith(settings::SendKey),
    /// Sets how messages from other sessions come in.
    PeerMessages(settings::PeerMessages),
    /// Sets the language of text in Han characters alone.
    HanLanguage(crate::cjk::CjkLanguage),
    /// Sets the composer's height in logical pixels; `None` fits it to its text.
    ComposerHeight(Option<u16>),
    /// Sets the folder new projects are made in.
    SetNewProjectsIn(String),
    BrowseNewProjectsIn,
    /// Attaches the clipboard's image, if it holds one, to the next message (Ctrl+V).
    PasteImage,
    /// Takes the image at this place off the next message.
    RemoveImage(usize),
    /// Turns the draft into a shell command, or back into a message (the `>_` button).
    ToggleShell,
    FilterSessions(String),
    /// Opens or folds the run of tool calls whose first call is at this place.
    ToggleRun(usize),
    ShowUsage,
    RefreshUsage,
    SelectUsageTab(usize),
    HideUsage,
    TogglePinProject(PathBuf),
    ToggleHideProject(PathBuf),
    RemoveProject(PathBuf),
    SetProjectSort(settings::ProjectSort),
    ToggleShowHiddenProjects,
    ToggleShowAllProjects,
    FilterStartProjects(String),
    /// Starts Claude Code again after you signed in elsewhere, and sends again what failed.
    TrySignInAgain,
    /// Hides the sign-in help.
    DismissSignIn,
    /// Trusts the folder shown, here and for Claude Code, and starts Claude Code in it.
    TrustFolder,
    /// Trusts the folder shown, already trusted here, for Claude Code too.
    TrustForClaudeCode,
    NotForClaudeCode,
    /// Starts Claude Code in the folder shown this once, without its configuration.
    StartWithoutFolderSettings,
    /// Ends every session the daemon runs, in every window, and then the daemon.
    ShutDown,
    Quit,
}

/// A model picked for one program: the other program doesn't know its name.
#[derive(Debug)]
pub struct ModelChoice {
    backend: provider::Backend,
    model: String,
}

/// What the header, the status bar and the menus show about the process and its session.
#[derive(Debug, Default)]
pub struct Status {
    pub session_id: Option<SessionId>,
    pub title: Option<String>,
    /// The program the session runs: the one picked for a new session, or the transcript's for
    /// one continued.
    pub backend: provider::Backend,
    /// What a Claude Code session signs in with, as picked when it was started or continued.
    pub sign_in_with: provider::SignInWith,
    /// The model in use, as the CLI resolved it.
    pub model: String,
    pub models: Vec<wire::ModelInfo>,
    /// The model picked in the menu or on the command line, kept for later processes of the
    /// same program.
    pub model_choice: Option<ModelChoice>,
    pub modes: Modes,
    /// The mode picked for this session, applied again if its process restarts.
    pub mode_choice: Option<ChosenMode>,
    pub commands: Vec<wire::CommandInfo>,
    pub account: wire::Account,
    /// The Claude plan's name as last told ("max", "pro"), kept across pages.
    pub claude_subscription: String,
    pub sign_in: SignIn,
    /// Claude Code's version, as it told at `initialize`.
    pub version: String,
    /// Prompts sent and not answered yet.
    pub turns: usize,
    /// Set when you stopped the turn, with Stop or by denying a call without saying why. The
    /// turn then ends as failed, but isn't shown as a failure.
    pub stop_asked: bool,
    pub doing: Doing,
    /// What runs besides the turn's own steps: subagents, commands, workflows.
    pub tasks: Tasks,
    /// Set when you stopped a turn and its tasks ran on; their list says so until the next
    /// prompt.
    pub tasks_left_running: bool,
    pub context_tokens: u64,
    pub context_window: u64,
    pub cost_usd: f64,
    pub limits: PlanLimits,
    pub limit_wait: LimitWait,
    pub worktree: Option<Worktree>,
    /// What the last turn changed in the folder, from the daemon's snapshots.
    pub turn_changed: Option<crate::changes::Summary>,
}

impl Status {
    /// Returns whether a turn is under way, prompted here or started by the session itself
    /// (for a message from another session, which only its output shows).
    pub(crate) fn in_turn(&self) -> bool {
        self.turns > 0 || self.doing != Doing::Nothing
    }

    /// Returns what the session's program supports with its sign-in.
    pub(crate) fn capabilities(&self) -> crate::provider::Capabilities {
        self.backend.capabilities(self.sign_in_with)
    }

    /// Returns the limits of the plan paying for the session; `None` for an API key or a
    /// program that reports none.
    pub(crate) fn plan_limits(&self) -> Option<Limits> {
        if !self.capabilities().plan_limits {
            return None;
        }
        match self.backend {
            provider::Backend::Claude => Some(self.limits.claude),
            provider::Backend::Agy => Some(self.limits.agy),
            provider::Backend::Opencode => None,
        }
    }

    /// Returns where the limits the session's program reports are kept; `None` for a program
    /// that reports none.
    pub(crate) fn limits_mut(&mut self) -> Option<&mut Limits> {
        if !self
            .backend
            .capabilities(provider::SignInWith::Account)
            .plan_limits
        {
            return None;
        }
        match self.backend {
            provider::Backend::Claude => Some(&mut self.limits.claude),
            provider::Backend::Agy => Some(&mut self.limits.agy),
            provider::Backend::Opencode => None,
        }
    }

    /// Returns the model picked for the session's program; `None` if it was picked for another.
    pub(crate) fn chosen_model(&self) -> Option<&str> {
        self.model_choice
            .as_ref()
            .filter(|choice| choice.backend == self.backend)
            .map(|choice| choice.model.as_str())
    }
}

/// What Claude is doing, for the status bar. That it waits for the user is told by what it
/// asks.
#[derive(Debug, Clone, Default, PartialEq, Eq, derive_more::Display)]
pub enum Doing {
    #[default]
    #[display("")]
    Nothing,
    #[display("Starting")]
    Starting,
    #[display("Thinking")]
    Thinking,
    #[display("Thinking  ~{}", crate::present::short_count(*_0))]
    ThinkingTokens(u64),
    #[display("Writing")]
    Writing,
    #[display("Preparing {_0}")]
    Preparing(ToolName),
    #[display("Running {_0}")]
    Running(ToolName),
    /// An ACP agent's call runs, named as the agent names it.
    #[display("{_0}")]
    Calling(String),
    /// Going on after the user answered.
    #[display("Working")]
    Working,
    #[display("Compacting the conversation")]
    Compacting,
    /// A request to the API failed, and is tried again.
    #[display("{_0}")]
    Retrying(Retry),
    /// Waiting on the API, and nothing heard from Claude Code for this long.
    #[display("No answer for {}", crate::present::short_duration(*_0))]
    Silent(Duration),
}

/// Claude Code tries a request to the API again: which time, of how many, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retry {
    pub attempt: u32,
    pub most: u32,
    pub status: Option<u16>,
    pub cause: wire::RetryCause,
}

impl std::fmt::Display for Retry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "Retrying {}/{} · ", self.attempt, self.most)?;
        match (self.cause, self.status) {
            (wire::RetryCause::Overloaded, _) => formatter.write_str("the API is overloaded"),
            (wire::RetryCause::RateLimit, _) => formatter.write_str("rate limited"),
            (wire::RetryCause::AuthenticationFailed, _) => formatter.write_str("sign-in refused"),
            (wire::RetryCause::CloudCredentialError, _) => {
                formatter.write_str("the cloud's credentials failed")
            }
            (wire::RetryCause::ServerError | wire::RetryCause::Unknown, Some(status)) => {
                write!(formatter, "the API answered {status}")
            }
            (wire::RetryCause::ServerError | wire::RetryCause::Unknown, None) => {
                formatter.write_str("no connection to the API")
            }
        }
    }
}

/// A turn the plan's usage limit stopped, continued once the limit resets. Claude Code's
/// terminal does this; behind a pipe it doesn't.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum LimitWait {
    #[default]
    Idle,
    /// Asked Claude Code when the limit resets.
    Asking,
    /// The turn goes on at this time.
    Until(jiff::Timestamp),
}

/// Whether the desktop's keyring keeps an Anthropic API key for Claude Code sessions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ApiKeyKept {
    /// The daemon wasn't asked yet, or hasn't answered.
    #[default]
    Unknown,
    Kept,
    Absent,
}

/// Why Claude Code refused the turn under way, said before its result ends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// Nobody is signed in, or the API refused the sign-in.
    SignIn,
    /// The plan's usage limit is reached.
    UsageLimit,
}

/// Each plan's limits, kept apart: a Claude Code session's aren't an agy session's.
#[derive(Debug, Default, Clone, Copy)]
pub struct PlanLimits {
    claude: Limits,
    agy: Limits,
}

impl PlanLimits {
    pub(crate) fn for_backend(&self, backend: provider::Backend) -> Option<Limits> {
        match backend {
            provider::Backend::Claude => Some(self.claude),
            provider::Backend::Agy => Some(self.agy),
            provider::Backend::Opencode => None,
        }
    }

    pub(crate) fn mut_for_backend(&mut self, backend: provider::Backend) -> Option<&mut Limits> {
        match backend {
            provider::Backend::Claude => Some(&mut self.claude),
            provider::Backend::Agy => Some(&mut self.agy),
            provider::Backend::Opencode => None,
        }
    }
}

/// How much of the plan's usage limits is used, as far as the CLI said.
#[derive(Debug, Default, Clone, Copy)]
pub struct Limits {
    pub five_hour: Option<LimitUse>,
    pub seven_day: Option<LimitUse>,
}

#[derive(Debug, Clone, Copy)]
pub struct LimitUse {
    /// The share used, from 0 to 1.
    pub used: f32,
    pub resets_at: Option<jiff::Timestamp>,
}

impl From<&wire::PlanLimit> for LimitUse {
    fn from(limit: &wire::PlanLimit) -> Self {
        Self {
            used: (limit.percent / 100.0).clamp(0.0, 1.0),
            resets_at: limit.resets_at,
        }
    }
}

/// One program's tab in the usage panel.
#[derive(Debug, Default)]
pub struct ProviderUsageState {
    pub report: Option<wire::UsageReport>,
    pub context: Option<wire::ContextReport>,
    /// Answers still owed.
    pub waiting: usize,
    pub error: Option<String>,
    pub available: bool,
}

/// The usage panel: each program's `/usage`, and `/context` for the session shown.
#[derive(Debug, Default)]
pub struct UsagePanel {
    pub open: bool,
    pub active_backend: provider::Backend,
    pub claude: ProviderUsageState,
    pub agy: ProviderUsageState,
    pub opencode: ProviderUsageState,
}

impl UsagePanel {
    pub fn for_backend(&self, backend: provider::Backend) -> &ProviderUsageState {
        match backend {
            provider::Backend::Claude => &self.claude,
            provider::Backend::Agy => &self.agy,
            provider::Backend::Opencode => &self.opencode,
        }
    }

    pub fn for_backend_mut(&mut self, backend: provider::Backend) -> &mut ProviderUsageState {
        match backend {
            provider::Backend::Claude => &mut self.claude,
            provider::Backend::Agy => &mut self.agy,
            provider::Backend::Opencode => &mut self.opencode,
        }
    }

    pub fn clear_waiting(&mut self) {
        self.claude.waiting = 0;
        self.agy.waiting = 0;
        self.opencode.waiting = 0;
    }
}

/// What our control requests were for, to act on their answers.
#[derive(Debug)]
pub enum Request {
    Initialize,
    Model(String),
    Mode(ChosenMode),
    Interrupt,
    StopTask(wire::TaskId),
    /// What the command followed in the side panel printed.
    TaskOutput(wire::TaskId),
    /// A side question, by its place in the side chat.
    Side(usize),
    Usage(provider::Backend),
    ContextUsage(provider::Backend),
}

/// A session running elsewhere, shown as it goes but not driven from here.
#[derive(Debug)]
pub struct Watch {
    pub session: LiveSession,
    /// Where its transcript is; found by the first read, which looks for it.
    place: ipc::Place,
    /// How far the transcript has been read.
    offset: u64,
    reading: Refresh,
    /// The watch on its transcript, once it is found.
    watched: Option<Watched>,
}

/// Rereads what other processes write: one read at a time, and one more after it if the file
/// changed meanwhile.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Refresh {
    #[default]
    Idle,
    Reading,
    /// Reading, and changed since the read began.
    ReadingStale,
}

impl Refresh {
    /// Asks for another read; returns whether one starts now.
    fn wanted(&mut self) -> bool {
        let start = *self == Self::Idle;
        *self = if start {
            Self::Reading
        } else {
            Self::ReadingStale
        };
        start
    }

    /// Ends a read; returns whether the file changed meanwhile and must be read again.
    fn done(&mut self) -> bool {
        let stale = *self == Self::ReadingStale;
        *self = Self::Idle;
        stale
    }
}

/// What to draw besides the transcript and the status bar, which are drawn every time.
#[derive(Debug, Default, Clone, Copy)]
struct Redraw {
    /// A run of tool calls was opened or folded.
    runs: bool,
    menus: bool,
    prompt: bool,
    /// Both lists: this folder's sessions mark those running elsewhere.
    sessions: bool,
    /// The start page's cards and waiting list.
    overview: bool,
    side: bool,
    usage: bool,
    tasks: bool,
    peek: bool,
    /// The Changes view, or what the last turn changed.
    changes: bool,
}

impl Redraw {
    /// Returns everything to draw, after switching sessions.
    fn everything() -> Self {
        Self {
            runs: true,
            menus: true,
            prompt: true,
            sessions: true,
            overview: true,
            side: true,
            usage: true,
            tasks: true,
            peek: true,
            changes: true,
        }
    }
}

/// How this window's own session is doing, as the sidebar last listed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OwnMark {
    state: LiveStatus,
    /// Its subagents running now.
    agents: usize,
}

/// Lines of stderr kept, to say why the process ended.
const LOG_LINES: usize = 20;
/// What the window asks a process first. We stop tasks one by one, so stopping a turn leaves
/// background agents, commands and monitors running, and lists them for you to stop or leave.
const INITIALIZE: ControlRequest = ControlRequest::Initialize {
    per_task_stop_affordance: true,
};
/// How often the running sessions are looked at without being told of a change: a process that
/// crashed leaves its record behind.
const TICK: Duration = Duration::from_secs(60);
/// The least time between two notifications about the same session.
const NOTIFY_AGAIN: Duration = Duration::from_secs(60);
/// How often a turn waiting on the API is looked at (`Event::Listen`).
const LISTEN: Duration = Duration::from_secs(10);
/// How long Claude Code may say nothing before the window says so.
const QUIET: Duration = Duration::from_secs(30);

/// What a read is for, and so what becomes of the daemon's answer to it (`App::read`).
#[derive(Debug)]
enum Reading {
    /// The folders Claude Code worked in, listed.
    Projects,
    /// Known projects and their recent sessions, for the start page and switcher.
    Overview,
    /// A folder typed or picked, to resolve.
    Folder { typed: bool },
    /// Whether the working folder is a linked git worktree.
    LinkedWorktree { cwd: PathBuf, replace: bool },
    /// A saved session's transcript, to open it.
    Session { id: SessionId },
    /// The sessions running now.
    Live,
    /// What the followed session's transcript says after what is shown.
    Followed { id: SessionId },
    /// What a subagent's transcript says after what the peek shows.
    Peeked { task: wire::TaskId },
    /// A folder's sessions.
    Sessions { cwd: PathBuf },
    /// The worktree a session entered.
    Worktree { id: SessionId },
    /// What a session changed, for the Changes view.
    Changes {
        session: SessionId,
        scope: crate::changes::Scope,
    },
    /// A changed file's diff, for the Changes view.
    Diff {
        session: SessionId,
        root: PathBuf,
        path: PathBuf,
    },
    /// A program's usage, for the usage panel.
    ProviderUsage { backend: provider::Backend },
}

impl Reading {
    /// Returns the event `answer` makes; an answer of another kind is dropped.
    fn answered(self, answer: ipc::Answer, lines: Vec<TranscriptLine>) -> Option<Event> {
        match self {
            Self::Projects => {
                let ipc::Answer::Projects(projects) = answer else {
                    return None;
                };
                Some(Event::ProjectsListed(projects))
            }
            Self::Overview => {
                let ipc::Answer::Overview(projects) = answer else {
                    return None;
                };
                Some(Event::OverviewListed(projects))
            }
            Self::Folder { typed } => {
                let ipc::Answer::Folder(result) = answer else {
                    return None;
                };
                Some(Event::FolderRead { typed, result })
            }
            Self::LinkedWorktree { cwd, replace } => {
                let ipc::Answer::Worktree(worktree) = answer else {
                    return None;
                };
                Some(Event::LinkedWorktreeRead {
                    cwd,
                    worktree,
                    replace,
                })
            }
            Self::Session { id } => {
                let ipc::Answer::Transcript { path, read } = answer else {
                    return None;
                };
                let result = read.map(|offset| (Chat::from_transcript(&lines), offset));
                Some(Event::SessionLoaded { id, path, result })
            }
            Self::Live => {
                let ipc::Answer::Live(live) = answer else {
                    return None;
                };
                Some(Event::LiveListed(live))
            }
            Self::Followed { id } => {
                let ipc::Answer::Transcript { read, .. } = answer else {
                    return None;
                };
                let result = read.map(|offset| (lines, offset));
                Some(Event::TranscriptGrew { id, result })
            }
            Self::Peeked { task } => {
                let ipc::Answer::Transcript { read, .. } = answer else {
                    return None;
                };
                let result = read.map(|offset| (lines, offset));
                Some(Event::PeekGrew { task, result })
            }
            Self::Sessions { cwd } => {
                let ipc::Answer::Sessions(sessions) = answer else {
                    return None;
                };
                Some(Event::SessionsListed { cwd, sessions })
            }
            Self::Worktree { id } => {
                let ipc::Answer::Worktree(worktree) = answer else {
                    return None;
                };
                Some(Event::WorktreeRead { id, worktree })
            }
            Self::Changes { session, scope } => {
                let ipc::Answer::Changes(result) = answer else {
                    return None;
                };
                Some(Event::ChangesRead {
                    session,
                    scope,
                    result: result.map_err(|unkept| unkept.to_string()),
                })
            }
            Self::Diff {
                session,
                root,
                path,
            } => {
                let ipc::Answer::Diff(result) = answer else {
                    return None;
                };
                Some(Event::DiffRead {
                    session,
                    root,
                    path,
                    result: result.map_err(|unkept| unkept.to_string()),
                })
            }
            Self::ProviderUsage { backend } => {
                let ipc::Answer::ProviderUsage(usage) = answer else {
                    return None;
                };
                Some(Event::ProviderUsageRead { backend, usage })
            }
        }
    }
}

pub struct App<F: Frontend> {
    view: Presenter<F>,
    events: Sender<Event>,
    chat: Chat,
    status: Status,
    asks: VecDeque<Ask>,
    requests: HashMap<RequestId, Request>,
    process: Option<Agent>,
    generation: Generation,
    location: Location,
    sessions: Vec<SessionInfo>,
    overview: Vec<ProjectOverview>,
    built_overview: overview::Built,
    /// The slash commands the draft could become, by their place in `status.commands`.
    command_matches: Vec<usize>,
    recall: Recall,
    log: VecDeque<String>,
    live: Vec<LiveSession>,
    /// The sessions the daemon runs, which outlive their windows.
    hosted: Vec<Hosted>,
    /// Images pasted into the composer, to go with the next message.
    attached: Vec<Picture>,
    /// The rows Claude Code replayed for the `!` commands run since the last prompt. It runs
    /// them beside the conversation, so we send them ahead of the next prompt, as the terminal
    /// does.
    shell_rows: Vec<String>,
    /// The conversation's images, decoded for showing.
    images: images::Images,
    listing_live: Refresh,
    listing_overview: Refresh,
    /// When each session running elsewhere was last notified about.
    notified: HashMap<SessionId, web_time::Instant>,
    watch: Option<Watch>,
    /// The subagent whose conversation the side panel follows.
    peek: Option<Peek>,
    /// The Changes view, while open.
    changes: Option<ChangesView>,
    /// Comments on lines of the session's changes, not sent yet.
    line_comments: Vec<LineComment>,
    /// What each read's answer is for, by request.
    answers: HashMap<ActId, Reading>,
    /// The newest `Read::Sessions` request sent, so a slower earlier one doesn't overwrite it.
    sessions_request: Option<ActId>,
    /// Why the settings couldn't be read, to say once the window is up.
    settings_problem: Option<String>,
    /// The watch on the running sessions' records.
    records: Option<Watched>,
    side: SideChat,
    /// The session the frontend was last told it shows (`Command::SessionShown`).
    told_shown: Option<SessionId>,
    settings: Settings,
    /// The desktop's text size, in logical pixels, which the zoom scales.
    desktop_text_size: f32,
    /// The program named on the command line, restored on the project and start pages.
    default_backend: provider::Backend,
    /// The programs on PATH when the window connected.
    available_backends: Vec<provider::Backend>,
    /// What the start-page project filter holds.
    start_filter: String,
    /// What the list of sessions is narrowed to.
    session_filter: String,
    /// How our session was last listed in the sidebar; `None` while no process runs it here.
    own_listed: Option<OwnMark>,
    /// The runs of tool calls opened, by their first call; the others are folded.
    open_runs: HashSet<usize>,
    usage: UsagePanel,
    /// A start held back until the folder is trusted.
    trust_gate: TrustGate,
    claude_trust_ask: ClaudeTrustAsk,
    /// Folders the daemon found trusted in the terminal; starting there again asks nothing.
    accepted_in_terminal: HashSet<PathBuf>,
    /// The command line's prompt for a continued session. We send it once the transcript is
    /// shown, because showing it replaces the conversation, the prompt's bubble with it.
    held_prompt: Option<String>,
    daemon: Daemon,
    /// Set when you signed in elsewhere while a restart would interrupt something (a question
    /// waiting for its answers, say); the process restarts once nothing is under way.
    restart_waits: bool,
    /// The last prompt sent, for `SignIn::Refused` to keep if its turn fails for that.
    last_prompt: Option<SentPrompt>,
    /// Why Claude Code refused the turn under way, said before its result ends it.
    turn_refusal: Option<Refusal>,
    /// Whether the keyring keeps an Anthropic API key, as the daemon last said.
    api_key: ApiKeyKept,
    /// What the keyring last said when the key was stored, removed or looked for, for the
    /// settings.
    api_key_note: String,
    /// A Claude Code session to continue once the user picks what it signs in with.
    choosing_sign_in: Option<SessionId>,
    /// The command line's session to continue, held until the keyring was asked for a key.
    resume_after_key_check: Option<SessionId>,
    link: Link,
    /// What each request to the daemon was for.
    acting: HashMap<ActId, Acting>,
    last_act: ActId,
    tick: Repeat,
    /// Runs while a turn is under way (see [`Event::Listen`]).
    listen: Repeat,
    /// Runs while a command's output is followed (see [`Event::PollOutput`]).
    polling: Repeat,
    /// Set for the next time a usage limit resets (see [`Event::LimitReset`]); stops as it
    /// fires.
    limit_reset: Repeat,
    /// When the process last said something.
    heard: web_time::Instant,
    redraw: Redraw,
}

impl<F: Frontend> App<F> {
    pub fn new(
        frontend: F,
        events: Sender<Event>,
        link: Link,
        desktop_found: Desktop,
        settings: Result<Settings, String>,
        start: Start,
    ) -> Self {
        let Start {
            cwd,
            backend,
            model,
        } = start;
        let Desktop {
            finds_windows,
            text_size: desktop_text_size,
            backends: available_backends,
        } = desktop_found;
        let mut view = Presenter::new(frontend, finds_windows);
        let (settings, settings_problem) = match settings {
            Ok(settings) => (settings, None),
            Err(problem) => {
                tracing::warn!(%problem, "starting with the default settings");
                (Settings::default(), Some(problem))
            }
        };
        let projects_dir = settings
            .effective_projects_dir()
            .map_or_default(|dir| dir.to_string_lossy().into_owned());
        view.set_text_size(desktop_text_size * settings.scale());
        view.set_composer_height(settings.composer_height_px);
        view.set_new_projects_in(&projects_dir);
        view.set_new_project_parent(&projects_dir);
        view.set_look(settings.look);
        view.set_send_key(settings.send_with);
        view.set_peer_messages(settings.peer_messages);
        view.set_han_language(settings.han_language());
        let location = match cwd {
            Some(cwd) => Location::Session(cwd),
            None => Location::Start,
        };
        let mut usage = UsagePanel {
            active_backend: backend,
            ..UsagePanel::default()
        };
        for candidate in provider::Backend::ALL {
            usage.for_backend_mut(candidate).available = available_backends.contains(&candidate);
        }
        Self {
            view,
            images: images::Images::start(events.clone()),
            events,
            chat: Chat::default(),
            status: Status {
                backend,
                model_choice: model.map(|model| ModelChoice { backend, model }),
                ..Status::default()
            },
            asks: VecDeque::new(),
            requests: HashMap::new(),
            process: None,
            generation: Generation::default(),
            location,
            sessions: Vec::new(),
            overview: Vec::new(),
            built_overview: overview::Built {
                cards: Vec::new(),
                waiting: Vec::new(),
            },
            command_matches: Vec::new(),
            recall: Recall::Writing,
            log: VecDeque::new(),
            live: Vec::new(),
            hosted: Vec::new(),
            attached: Vec::new(),
            shell_rows: Vec::new(),
            listing_live: Refresh::Idle,
            listing_overview: Refresh::Idle,
            notified: HashMap::new(),
            watch: None,
            peek: None,
            changes: None,
            line_comments: Vec::new(),
            answers: HashMap::new(),
            sessions_request: None,
            settings_problem,
            records: None,
            side: SideChat::default(),
            told_shown: None,
            settings,
            desktop_text_size,
            default_backend: backend,
            available_backends,
            start_filter: String::new(),
            session_filter: String::new(),
            own_listed: None,
            open_runs: HashSet::new(),
            usage,
            trust_gate: TrustGate::Open,
            claude_trust_ask: ClaudeTrustAsk::Not,
            accepted_in_terminal: HashSet::new(),
            held_prompt: None,
            daemon: Daemon::Running,
            restart_waits: false,
            last_prompt: None,
            turn_refusal: None,
            api_key: ApiKeyKept::Unknown,
            api_key_note: String::new(),
            choosing_sign_in: None,
            resume_after_key_check: None,
            link,
            acting: HashMap::new(),
            last_act: ActId::default(),
            tick: Repeat::default(),
            listen: Repeat::default(),
            polling: Repeat::default(),
            limit_reset: Repeat::default(),
            heard: web_time::Instant::now(),
            redraw: Redraw::default(),
        }
    }

    /// Opens the first session when given a folder or `--resume`, or starts on the start page.
    pub fn begin(
        &mut self,
        hosted: Vec<Hosted>,
        live: Vec<LiveSession>,
        resume: Option<SessionId>,
        mode: Option<PermissionMode>,
        prompt: Option<&str>,
    ) {
        let starting_in_session = matches!(&self.location, Location::Session(_));
        if starting_in_session {
            self.load_sessions();
        }
        // Known before resuming: a session the daemon runs is attached to, one running
        // elsewhere watched, neither resumed by a second process writing the same transcript.
        self.view.set_running_sessions(hosted.len());
        self.hosted = hosted;
        let resuming = resume.is_some();
        if resuming || !starting_in_session {
            self.live = live;
        }
        // A Claude Code session to continue asks what it signs in with when an API key is
        // kept: it waits for the keyring's answer.
        match resume {
            Some(id) => self.resume_after_key_check = Some(id),
            None if starting_in_session => {
                self.new_session(self.status.backend, self.status.sign_in_with);
            }
            None => {}
        }
        self.check_api_key();
        // Said in the window, not only the log: what is changed now isn't kept.
        if let Some(error) = self.settings_problem.take() {
            self.chat.error(format!(
                "{error}\nThe window runs with the default settings, and keeps no change until the \
                 file is put right."
            ));
        }
        if let Some(mode) = mode {
            self.set_mode(ChosenMode::Claude(mode));
        }
        match prompt {
            // Sent once the transcript is shown (see `session_loaded`).
            Some(prompt) if resuming => self.held_prompt = Some(prompt.to_owned()),
            Some(prompt) if starting_in_session => self.send(prompt),
            Some(_) | None => {}
        }
        self.refresh_overview();
        self.records = self.watch(Target::Records);
        self.tick.start(TICK, self.events.clone(), || Event::Tick);
        self.tick();
        self.render();
    }

    /// Takes events until the window quits, drawing once per batch.
    pub async fn run(mut self, events: Receiver<Event>) {
        while let Ok(event) = events.recv().await {
            let mut flow = self.handle(event);
            while flow.is_continue()
                && let Ok(event) = events.try_recv()
            {
                flow = self.handle(event);
            }
            if flow.is_break() {
                break;
            }
            if self.restart_waits && !self.under_way() {
                self.restart();
            }
            self.render();
        }
    }

    fn handle(&mut self, event: Event) -> ControlFlow<()> {
        match event {
            Event::Action(action) => return self.act(action),
            Event::Agent { generation, output } => self.take(generation, output),
            Event::SessionsListed { cwd, sessions } => self.sessions_listed(&cwd, sessions),
            Event::ProjectsListed(projects) => {
                let shown: Vec<_> = projects
                    .into_iter()
                    .filter(|project| !self.settings.is_removed(&project.path))
                    .collect();
                self.view.render_projects(&shown);
            }
            Event::SessionLoaded { id, path, result } => self.session_loaded(&id, path, result),
            Event::TranscriptGrew { id, result } => self.transcript_grew(&id, result),
            Event::PeekGrew { task, result } => self.peek_grew(&task, result),
            Event::LiveListed(live) => self.live_listed(live),
            Event::WorktreeRead { id, worktree } => self.worktree_read(&id, worktree),
            Event::ChangesRead {
                session,
                scope,
                result,
            } => self.changes_read(&session, scope, result),
            Event::DiffRead {
                session,
                root,
                path,
                result,
            } => self.diff_read(&session, &root, &path, result),
            Event::Acted { request, result } => self.acted(request, result),
            Event::Answered {
                request,
                answer,
                lines,
            } => {
                if let Some(reading) = self.answers.remove(&request) {
                    if matches!(reading, Reading::Sessions { .. })
                        && self
                            .sessions_request
                            .is_some_and(|latest| latest != request)
                    {
                        return ControlFlow::Continue(());
                    }
                    if let Some(event) = reading.answered(answer, lines) {
                        return self.handle(event);
                    }
                }
            }
            Event::LinkedWorktreeRead {
                cwd,
                worktree,
                replace,
            } => {
                if worktree.is_some()
                    && (replace
                        || (self.status.worktree.is_none()
                            && self.location.cwd() == Some(cwd.as_path())))
                {
                    self.status.worktree = worktree;
                }
            }
            Event::FolderRead { typed, result } => match result {
                Ok(path) => {
                    if typed {
                        self.view.projects_opened();
                    }
                    self.enter_project(Folder::checked(path).into_path());
                }
                Err(problem) if typed => self.view.set_project_problem(&problem),
                Err(problem) => self.chat.error(problem),
            },
            Event::Picked { request, folder } => {
                // Nothing picked, or for a request since forgotten: nothing updates.
                if let (Some(Acting::Picking(target)), Some(folder)) =
                    (self.acting.remove(&request), folder)
                {
                    match target {
                        PickingTarget::OpenFolder => {
                            self.read_folder(Read::Folder(folder), false);
                        }
                        PickingTarget::NewProjectParent => {
                            self.view.set_new_project_parent(&folder.to_string_lossy());
                        }
                        PickingTarget::DefaultProjectsDir => self.set_new_projects_in(Some(folder)),
                    }
                }
            }
            Event::Trusted { request, trust } => self.trust_read(request, trust),
            Event::ClaudeTrustRead { request, trusted } => self.claude_trust_read(request, trusted),
            Event::CodeColoured => self.view.command(Command::DrawColouredCode),
            Event::SeatFailed(failure) => {
                tracing::warn!(%failure, "the frontend's desktop didn't do it");
                self.chat.error(failure);
            }
            Event::ImageShown { key, shown } => {
                if self.images.wanted(key) {
                    self.view
                        .show_image(key, shown.as_ref().map_err(ToString::to_string));
                    self.chat.image_shown(key);
                }
            }
            Event::Pasted { request, pasted } => {
                self.acting.remove(&request);
                match pasted {
                    Ok(picture) => {
                        self.attached.push(picture);
                        self.view.render_attached(&self.attached);
                    }
                    // Text was pasted, most likely; the composer took it.
                    Err(PasteFailure::NoImage) => {}
                    Err(failure) => self.chat.error(failure.to_string()),
                }
            }
            Event::DaemonLost => {
                self.process = None;
                self.forget_process();
                match self.daemon {
                    Daemon::Running => {
                        self.chat
                            .error("Lost the edualc daemon. Open edualc again to go on.");
                    }
                    // Said as it began.
                    Daemon::ShuttingDown => {}
                }
            }
            Event::LineLost => {
                self.process = None;
                self.forget_process();
                // Asked on the lost line: no answer comes.
                self.answers.clear();
                self.acting.clear();
                self.listing_live = Refresh::Idle;
                self.chat
                    .error("Lost the line to edualcd; reaching it again.");
            }
            Event::Reconnected { hosted, live } => self.reconnected(hosted, live),
            Event::DaemonShuttingDown => {
                self.daemon = Daemon::ShuttingDown;
                self.chat.info(
                    "edualc is shutting down, as asked in a window: every session ends. Open \
                     edualc again to go on and continue this session from its project.",
                );
            }
            Event::Hosted(hosted) => {
                self.view.set_running_sessions(hosted.len());
                self.hosted = hosted;
                self.refresh_overview();
                self.redraw.sessions = true;
            }
            Event::Changed(watched) => self.changed(watched),
            Event::Tick => self.tick(),
            Event::Listen => self.listen(),
            Event::PollOutput => self.poll_output(),
            Event::LimitReset => self.limit_reset(),
            Event::ApiKeyStored(stored) => self.api_key_stored(stored),
            Event::OverviewListed(projects) => {
                if self.listing_overview.done() {
                    self.load_overview();
                }
                self.overview = projects;
                self.refresh_overview();
            }
            Event::ProjectCreated { request, result } => self.project_created(request, result),
            Event::SettingsNotKept(why) => {
                self.chat
                    .error(format!("A change to your settings wasn't kept: {why}"));
            }
            Event::ProviderUsageRead { backend, usage } => {
                self.provider_usage_read(backend, usage);
            }
        }
        ControlFlow::Continue(())
    }

    fn act(&mut self, action: Action) -> ControlFlow<()> {
        match action {
            Action::Send(text) => self.send(&text),
            Action::Interrupt => self.interrupt(),
            Action::NewSession(backend, sign_in)
            | Action::NewSessionInProject(backend, sign_in) => {
                self.new_session(backend, sign_in);
            }
            Action::NewApiKeySession(model) => {
                if let Some(model) = model {
                    self.status.model_choice = Some(ModelChoice {
                        backend: provider::Backend::Claude,
                        model,
                    });
                }
                self.new_session(provider::Backend::Claude, provider::SignInWith::ApiKey);
            }
            Action::ContinueWith(sign_in) => self.continue_with(sign_in),
            Action::StoreApiKey(pasted) => self.store_api_key(&pasted.0),
            Action::RemoveApiKey => self.ask_daemon(Act::RemoveApiKey, Acting::ApiKey),
            Action::OpenSession(id) => self.open_session(id),
            Action::OpenStart => self.open_start(),
            Action::EnterProject(folder) => self.enter_project(folder),
            Action::JumpTo(target) => self.jump_to(target),
            Action::NextWaiting => self.next_waiting(),
            Action::Back => self.back(),
            Action::BrowseNewProjectParent => self.pick_folder(PickingTarget::NewProjectParent),
            Action::CreateProject { parent, name } => self.create_project(&parent, name),
            Action::ClearNewProjectError => self.view.set_new_project_error(""),
            Action::OpenProject(typed) => self.read_folder(Read::TypedFolder(typed), true),
            Action::BrowseProject => self.pick_folder(PickingTarget::OpenFolder),
            Action::ListProjects => {
                self.view.set_project_problem("");
                self.read(Read::Projects, Reading::Projects);
            }
            Action::ChooseModel(index) => self.choose_model(index),
            Action::ChooseMode(index) => self.choose_mode(index),
            Action::DraftEdited(draft) => self.draft_edited(&draft),
            Action::PickCommand(index) => self.pick_command(index),
            Action::RecallOlder => self.recall_older(),
            Action::RecallNewer => self.recall_newer(),
            Action::RunCommand(index) => self.run_command(index),
            Action::Toggle(index) => self.chat.toggle(index),
            Action::CopyEntry(index) => {
                if let Some(entry) = self.chat.entries.get(index) {
                    self.view.copy(entry.copy_text());
                }
            }
            Action::Copy(text) => self.view.copy(&text),
            Action::OpenUrl(url) => self.view.seat(Seat::OpenUrl(url)),
            Action::Allow(request) => self.allow(&request, false),
            Action::AllowAlways(request) => self.allow(&request, true),
            Action::Deny { request, feedback } => self.deny(&request, &feedback),
            Action::ToggleOption { question, option } => self.toggle_option(question, option),
            Action::SetOther { question, text } => self.set_other(question, text),
            Action::SetNotes { question, text } => self.set_notes(question, text),
            Action::AnswerQuestions(request) => self.answer_questions(&request),
            Action::ApprovePlan {
                request,
                mode,
                overall,
            } => self.approve_plan(&request, mode, &overall),
            Action::EditComment(section) => self.edit_comment(Some(section)),
            Action::CancelComment => self.edit_comment(None),
            Action::SetComment { section, text } => self.set_comment(section, &text),
            Action::ContinueHere => self.continue_here(),
            Action::GoToWindow => self.go_to_window(),
            Action::AskSide(question) => self.ask_side(&question),
            Action::ToggleSide => self.toggle_side(),
            Action::DismissSide => self.dismiss_side(),
            Action::CopySide(index) => self.copy_side(index),
            Action::SideToConversation => self.side_to_conversation(),
            Action::BranchSide => self.branch_side(),
            Action::ShowChanges => self.show_changes(),
            Action::CloseChanges => self.close_changes(),
            Action::ChangesScope(scope) => self.choose_changes_scope(scope),
            Action::SelectChangedFile(index) => self.select_changed_file(index),
            Action::ToggleReviewed => self.toggle_reviewed(),
            Action::EditLineComment(row) => self.edit_line_comment(row),
            Action::SetLineComment(row, text) => self.set_line_comment(row, &text),
            Action::CancelLineComment => self.cancel_line_comment(),
            Action::SendLineComments => self.send_line_comments(),
            Action::OpenPeek(task) => self.open_peek(task),
            Action::ClosePeek => self.close_peek(),
            Action::StopTask(task) => self.stop_task(task),
            Action::StopAllTasks => self.stop_all_tasks(),
            Action::OpenWorktree => self.open_worktree(),
            Action::ShowUsage => self.show_usage(),
            Action::RefreshUsage => self.refresh_usage(),
            Action::SelectUsageTab(index) => self.select_usage_tab(index),
            Action::HideUsage => {
                self.usage.open = false;
                self.redraw.usage = true;
            }
            Action::TogglePinProject(path) => {
                self.change_settings(settings::Change::TogglePinProject(path));
                self.refresh_overview();
            }
            Action::ToggleHideProject(path) => {
                self.change_settings(settings::Change::ToggleHideProject(path));
                self.refresh_overview();
            }
            Action::RemoveProject(path) => {
                self.change_settings(settings::Change::RemoveProject(path));
                self.refresh_overview();
            }
            Action::SetProjectSort(sort) => {
                self.change_settings(settings::Change::ProjectSort(sort));
                self.refresh_overview();
            }
            Action::ToggleShowHiddenProjects => {
                let show = !self.settings.show_hidden_projects;
                self.change_settings(settings::Change::ShowHiddenProjects(show));
                self.redraw.overview = true;
            }
            Action::ToggleShowAllProjects => {
                let show = !self.settings.show_all_projects;
                self.change_settings(settings::Change::ShowAllProjects(show));
                self.redraw.overview = true;
            }
            Action::FilterStartProjects(filter) => {
                self.start_filter = filter;
                self.redraw.overview = true;
            }
            Action::TrySignInAgain => self.try_sign_in_again(),
            Action::DismissSignIn => {
                if self.status.sign_in.waits_for_user() {
                    self.status.sign_in = SignIn::Unknown;
                }
            }
            Action::TrustFolder => self.trust_folder(),
            Action::TrustForClaudeCode => self.trust_for_claude_code(),
            Action::NotForClaudeCode => self.not_for_claude_code(),
            Action::StartWithoutFolderSettings => self.start_without_folder_settings(),
            Action::ToggleRun(index) => {
                if !self.open_runs.remove(&index) {
                    self.open_runs.insert(index);
                }
                self.redraw.runs = true;
            }
            Action::FilterSessions(filter) => {
                self.session_filter = filter;
                self.redraw.sessions = true;
            }
            Action::Zoom(zoom) => {
                self.settings.zoom(zoom);
                self.view
                    .set_text_size(self.desktop_text_size * self.settings.scale());
                self.change_settings(settings::Change::ZoomPercent(self.settings.zoom_percent));
            }
            Action::SwitchLook => {
                let look = self.settings.look.other();
                self.view.set_look(look);
                self.change_settings(settings::Change::Look(look));
            }
            Action::RecolourCode => {
                self.view.command(Command::RecolourCode);
                // Messages already drawn, and the side panel's answers, are drawn again in them.
                self.chat.touch_all();
                self.redraw.side = true;
            }
            Action::SendWith(key) => {
                self.view.set_send_key(key);
                self.change_settings(settings::Change::SendWith(key));
            }
            Action::PeerMessages(peer_messages) => {
                self.view.set_peer_messages(peer_messages);
                self.change_settings(settings::Change::PeerMessages(peer_messages));
            }
            Action::HanLanguage(language) => {
                self.view.set_han_language(language);
                self.change_settings(settings::Change::HanText(language));
                // Messages already drawn are drawn again in it.
                self.chat.touch_all();
            }
            Action::ComposerHeight(height_px) => {
                self.change_settings(settings::Change::ComposerHeight(height_px));
            }
            Action::SetNewProjectsIn(dir) => {
                let trimmed = dir.trim();
                let next = (!trimmed.is_empty()).then(|| PathBuf::from(trimmed));
                self.set_new_projects_in(next);
            }
            Action::BrowseNewProjectsIn => self.pick_folder(PickingTarget::DefaultProjectsDir),
            Action::PasteImage => self.view.seat(Seat::PasteImage),
            Action::RemoveImage(index) => {
                if index < self.attached.len() {
                    self.attached.remove(index);
                    self.view.render_attached(&self.attached);
                }
            }
            Action::ToggleShell => self.toggle_shell(),
            Action::ShutDown => {
                if link::send(&self.link, ToDaemon::ShutDown).is_err() {
                    self.chat.error("Lost the edualc daemon.");
                }
            }
            Action::Quit => {
                self.quit();
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    }

    fn render(&mut self) {
        let redraw = std::mem::take(&mut self.redraw);
        let cwd_buf = self.location.cwd().map_or_default(Path::to_path_buf);
        let cwd = cwd_buf.as_path();
        let (project_name, project_folder, session_title, can_back) = match &self.location {
            Location::Start => (String::new(), String::new(), String::new(), false),
            Location::Project(folder) => (
                sessions::folder_name(folder),
                folder.to_string_lossy().into_owned(),
                String::new(),
                true,
            ),
            Location::Session(folder) => (
                sessions::folder_name(folder),
                folder.to_string_lossy().into_owned(),
                self.status
                    .title
                    .as_deref()
                    .unwrap_or("New session")
                    .to_owned(),
                true,
            ),
        };
        self.view.render_nav(
            self.location.view(),
            &project_name,
            &project_folder,
            &session_title,
            can_back,
        );
        if self.status.session_id != self.told_shown {
            self.told_shown.clone_from(&self.status.session_id);
            self.view
                .command(Command::SessionShown(self.told_shown.clone()));
        }
        let changes = self.chat.take_changes();
        if changes.contains(&Change::Reset) {
            let shown = self.chat.image_keys();
            self.images.keep(&shown);
            self.view.keep_images(&shown);
        }
        for (key, bytes) in self.chat.take_images() {
            if !self.images.ask(key, bytes) {
                self.view
                    .show_image(key, Err("the window's image decoder is gone".to_owned()));
            }
        }
        // Streamed text adds no message and folds no run.
        let restructured = redraw.runs
            || changes
                .iter()
                .any(|change| !matches!(change, Change::Grew(_)));
        self.view
            .render_transcript(&self.chat, changes, cwd, &self.open_runs, redraw.runs);
        if restructured {
            self.view.render_pinned(&self.chat);
        }
        self.view.render_activity(
            &self.status,
            !self.asks.is_empty(),
            self.chat.shell_running(),
        );
        let asking = match &self.trust_gate {
            TrustGate::Asking { runs, .. } => Some(runs.as_slice()),
            TrustGate::Open | TrustGate::Checking { .. } => None,
        };
        let worktree = self.current_worktree().cloned();
        self.view.render_status(
            &self.status,
            asking,
            matches!(self.claude_trust_ask, ClaudeTrustAsk::Asking(_)),
            cwd,
            self.watch.as_ref(),
            worktree.as_ref(),
        );
        let choosing = self.choosing_sign_in.as_ref().map(|id| {
            self.sessions
                .iter()
                .find(|session| &session.id == id)
                .map_or_else(|| id.as_str().to_owned(), |session| session.title.clone())
        });
        self.view
            .render_api_key(self.api_key, &self.api_key_note, choosing.as_deref());
        if redraw.menus {
            self.view.render_menus(&self.status);
        }
        if redraw.prompt {
            self.view
                .render_prompt(&self.asks, cwd, self.status.backend);
        }
        if redraw.usage {
            self.view.render_usage(&self.usage, &self.status);
        }
        if redraw.tasks {
            self.view
                .render_tasks(&self.status.tasks, self.status.tasks_left_running);
        }
        if redraw.peek || redraw.tasks {
            self.view
                .render_peek(self.peek.as_ref(), &self.status.tasks);
        }
        if redraw.changes {
            self.view.render_changes(
                self.changes.as_ref(),
                &self.line_comments,
                self.status.turn_changed,
            );
        }
        if redraw.side {
            let can_ask = self.watch.is_none() && self.status.capabilities().side_questions;
            let can_branch = can_ask
                && self.status.turns == 0
                && self.status.session_id.is_some()
                && self.side.has_answers()
                && !self.side.pending();
            self.view.render_side(&self.side, can_ask, can_branch);
        }
        if redraw.overview || redraw.sessions {
            let shown = self
                .watch
                .as_ref()
                .map(|watch| &watch.session.id)
                .or(self.status.session_id.as_ref());
            self.view.render_overview(
                &self.built_overview,
                shown,
                OverviewView {
                    sort: self.settings.project_sort,
                    show_hidden: self.settings.show_hidden_projects,
                    show_all: self.settings.show_all_projects,
                    filter: &self.start_filter,
                },
            );
        }
        // The own session's row changes with its turns, questions and agents, which no list
        // update says.
        let own = self.own_mark();
        if redraw.sessions || own != self.own_listed {
            self.own_listed = own;
            if let Some(folder) = self.location.cwd() {
                let shown = self
                    .watch
                    .as_ref()
                    .map(|watch| &watch.session.id)
                    .or(self.status.session_id.as_ref());
                let rows = sidebar::rows(&sidebar::Listing {
                    own: own.and_then(|mark| self.own_session(mark)),
                    live: &self.live,
                    hosted: &self.hosted,
                    saved: &self.sessions,
                    shown,
                    folder,
                    filter: &self.session_filter,
                });
                self.view
                    .render_sidebar(&rows, !self.session_filter.trim().is_empty());
            } else {
                self.view.render_sidebar(&[], false);
            }
        }
        self.view.flush();
    }

    /// Returns how our own session is doing, while its process runs here.
    fn own_mark(&self) -> Option<OwnMark> {
        if self.watch.is_some() || self.process.is_none() || self.status.session_id.is_none() {
            return None;
        }
        let state = if !self.asks.is_empty() {
            LiveStatus::Waiting
        } else if self.status.in_turn() {
            LiveStatus::Busy
        } else {
            LiveStatus::Idle
        };
        let agents = self
            .status
            .tasks
            .running()
            .filter(|running| running.task.task_type.is_agent() && !running.task.ambient)
            .count();
        Some(OwnMark { state, agents })
    }

    /// Returns our own session as the session list shows it.
    fn own_session(&self, mark: OwnMark) -> Option<sidebar::Own<'_>> {
        let cwd = self.location.cwd()?;
        let id = self.status.session_id.as_ref()?;
        let title = self
            .status
            .title
            .as_deref()
            .or_else(|| {
                self.sessions
                    .iter()
                    .find(|saved| &saved.id == id)
                    .map(|saved| saved.title.as_str())
            })
            .unwrap_or("New session");
        Some(sidebar::Own {
            id,
            title,
            name: MessagingName::of(id, cwd).to_string(),
            state: mark.state,
            worktree: self
                .status
                .worktree
                .as_ref()
                .map(|worktree| worktree.name.as_str()),
            agents: mark.agents,
            program: provider::Program::of(self.status.backend, self.status.sign_in_with),
        })
    }

    /// Asks the daemon to read something; `reading` says what its answer becomes.
    fn read(&mut self, read: Read, reading: Reading) {
        self.last_act = self.last_act.next();
        let request = self.last_act;
        if link::send(&self.link, ToDaemon::Read { request, read }).is_ok() {
            self.answers.insert(request, reading);
        } else {
            tracing::warn!("the daemon is gone: nothing is read");
        }
    }

    /// Asks the daemon to resolve the folder to open.
    fn read_folder(&mut self, read: Read, typed: bool) {
        self.read(read, Reading::Folder { typed });
    }

    /// Reads whether the working folder is a linked git worktree.
    fn read_linked_worktree(&mut self, cwd: PathBuf, replace: bool) {
        let read = Read::LinkedWorktree { cwd: cwd.clone() };
        self.read(read, Reading::LinkedWorktree { cwd, replace });
    }

    /// Asks the daemon to tell when what `target` names changes (`Event::Changed`).
    fn watch(&self, target: Target) -> Option<Watched> {
        let watched = Watched::new();
        match link::send(&self.link, ToDaemon::Watch { watched, target }) {
            Ok(()) => Some(watched),
            Err(link::Lost) => {
                tracing::warn!("the daemon is gone: changes go unnoticed");
                None
            }
        }
    }

    fn unwatch(&self, watched: Watched) {
        if link::send(&self.link, ToDaemon::Unwatch(watched)).is_err() {
            tracing::debug!("the daemon is gone with its watches");
        }
    }

    /// Applies `change` to the settings and has the daemon keep it. We send the change alone,
    /// so what another window changed meanwhile is kept too.
    fn change_settings(&mut self, change: settings::Change) {
        self.settings.apply(change.clone());
        if link::send(&self.link, ToDaemon::ChangeSettings(change)).is_err() {
            self.chat
                .error("A change to your settings wasn't kept: the edualc daemon is gone.");
        }
    }

    /// Asks the daemon to do something on its machine; its answer comes as `Event::Acted`.
    fn ask_daemon(&mut self, act: Act, acting: Acting) {
        self.last_act = self.last_act.next();
        let request = self.last_act;
        if link::send(&self.link, ToDaemon::Act { request, act }).is_err() {
            self.chat.error("Lost the edualc daemon.");
            return;
        }
        self.acting.insert(request, acting);
    }

    /// Asks the frontend's desktop for a folder, for `target`; it comes as `Event::Picked`.
    fn pick_folder(&mut self, target: PickingTarget) {
        self.last_act = self.last_act.next();
        self.acting.insert(self.last_act, Acting::Picking(target));
        self.view.seat(Seat::PickFolder(self.last_act));
    }

    fn acted(&mut self, request: ActId, result: Result<(), ActFailure>) {
        match (self.acting.remove(&request), result) {
            (Some(Acting::ApiKey), result) => self.api_key_changed(result),
            (Some(Acting::Desktop), Err(failure)) => {
                tracing::warn!(%failure, "the daemon didn't do it");
                self.chat.error(failure.to_string());
            }
            // A folder picked is answered as `Picked`, a project as `Created`.
            (Some(Acting::Desktop | Acting::Picking(_) | Acting::CreatingProject), Ok(()))
            | (None, _) => {}
            (Some(Acting::Picking(_)), Err(failure)) => {
                self.chat.error(failure.to_string());
            }
            (Some(Acting::CreatingProject), Err(failure)) => {
                self.view.set_new_project_error(&failure.to_string());
            }
        }
    }

    fn quit(&mut self) {
        self.tick.stop();
        self.generation = self.generation.next();
        self.let_go();
        self.view.command(crate::present::Command::Quit);
    }

    fn tick(&mut self) {
        self.list_live();
        self.load_overview();
        self.read_on();
    }

    /// Starts checking whether Claude Code has gone quiet (see [`Self::listen`]), counting
    /// from now.
    fn start_listening(&mut self) {
        self.heard = web_time::Instant::now();
        if self.listen.running() {
            return;
        }
        self.listen
            .start(LISTEN, self.events.clone(), || Event::Listen);
    }

    /// Says how long Claude Code has been quiet while a turn waits on the API, so that a
    /// blocked network doesn't look like "Starting" for ever. Stops once no turn is under way.
    fn listen(&mut self) {
        if self.status.turns == 0 {
            self.listen.stop();
            return;
        }
        if let Some(quiet) = gone_quiet(
            &self.status.doing,
            !self.asks.is_empty(),
            self.heard.elapsed(),
        ) {
            self.status.doing = Doing::Silent(quiet);
        }
    }

    fn changed(&mut self, watched: Watched) {
        if self.records == Some(watched) {
            self.list_live();
            self.load_overview();
        } else if self
            .watch
            .as_ref()
            .is_some_and(|watch| watch.watched == Some(watched))
        {
            self.read_on();
        } else if self.peeks_at(watched) {
            self.read_peek();
        } else {
            // A transcript no longer followed.
            self.unwatch(watched);
        }
    }

    /// Lists the running sessions again, once whatever listing is under way is done.
    fn list_live(&mut self) {
        if self.listing_live.wanted() {
            self.read(Read::Live, Reading::Live);
        }
    }

    /// Reads what the followed session's transcript says after what is shown.
    fn read_on(&mut self) {
        let Some(watch) = self.watch.as_mut() else {
            return;
        };
        if watch.reading.wanted() {
            let (id, place, from) = (watch.session.id.clone(), watch.place.clone(), watch.offset);
            self.read(Read::Transcript { place, from }, Reading::Followed { id });
        }
    }

    /// Stops watching the followed session's transcript.
    fn forget_watch(&self) {
        if let Some(watched) = self.watch.as_ref().and_then(|watch| watch.watched) {
            self.unwatch(watched);
        }
    }

    /// Takes the running sessions, ours left out, and notifies when one starts waiting for its
    /// user.
    fn live_listed(&mut self, mut live: Vec<LiveSession>) {
        if self.listing_live.done() {
            self.list_live();
        }
        let own = self.process.as_ref().and_then(Agent::pid);
        live.retain(|session| Some(session.pid) != own);
        for session in &live {
            let was = self
                .live
                .iter()
                .find(|known| known.id == session.id)
                .map(|known| known.status);
            let started_waiting = session.status == LiveStatus::Waiting
                && was.is_some_and(|was| was != LiveStatus::Waiting);
            // A session flipping between waiting and working doesn't make a notification each
            // time.
            let now = web_time::Instant::now();
            let recently = self
                .notified
                .get(&session.id)
                .is_some_and(|at| now.duration_since(*at) < NOTIFY_AGAIN);
            if started_waiting && !recently {
                self.notified.insert(session.id.clone(), now);
                let summary = format!("{} needs you", session.name);
                let body = session.cwd.display().to_string();
                self.view.seat(Seat::Notify { summary, body });
            }
        }
        if let Some(watch) = self.watch.as_mut()
            && let Some(session) = live.iter().find(|session| session.id == watch.session.id)
        {
            watch.session.clone_from(session);
        }
        if live != self.live {
            self.live = live;
            self.refresh_overview();
            self.redraw.sessions = true;
        }
    }
}

/// Returns how long Claude Code has been `quiet`, in whole seconds, once that is [`QUIET`] or
/// longer while it works on an answer and nothing is `asking` you. A tool, compacting or a
/// retry say what they do themselves.
fn gone_quiet(doing: &Doing, asking: bool, quiet: Duration) -> Option<Duration> {
    let on_an_answer = matches!(
        doing,
        Doing::Starting
            | Doing::Thinking
            | Doing::ThinkingTokens(_)
            | Doing::Writing
            | Doing::Preparing(_)
            | Doing::Working
            | Doing::Silent(_)
    );
    (on_an_answer && !asking && quiet >= QUIET).then(|| Duration::from_secs(quiet.as_secs()))
}

/// What the window was started with: a folder, the program, and a model (else the program's
/// default).
#[derive(Debug)]
pub struct Start {
    pub cwd: Option<PathBuf>,
    pub backend: provider::Backend,
    pub model: Option<String>,
}

/// What the daemon told the window, and what the window's own desktop offers.
#[derive(Debug)]
pub struct Desktop {
    /// Other programs' windows can be found from this window.
    finds_windows: bool,
    /// In logical pixels.
    text_size: f32,
    /// The programs on PATH when the window connected.
    backends: Vec<provider::Backend>,
}

impl Desktop {
    /// Takes what the daemon told, the desktop's text size in logical pixels, and whether other
    /// programs' windows can be found here.
    pub fn of(greeting: &link::Greeting, text_size: f32, finds_windows: bool) -> Self {
        Self {
            finds_windows,
            text_size,
            backends: greeting.backends.clone(),
        }
    }

    /// Returns the browser demo's, whose text has the size most desktops have.
    #[cfg(target_arch = "wasm32")]
    pub fn in_the_browser(text_size: f32) -> Self {
        Self {
            finds_windows: false,
            text_size,
            backends: vec![provider::Backend::Claude],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::asks::answered;
    use super::composer::{Recalled, argument_hint, normalized, provisional_title, shell_command};
    use super::folder_trust::TrustAnswer;
    use super::*;

    use crate::agent;
    use crate::call::{self, AcpCall, AcpOption, AcpOptionKind};
    use crate::ids::ToolUseId;
    use crate::ipc::Runs;
    use crate::present;

    /// A frontend that draws nothing and sends the test what it was shown.
    struct Recorder {
        sessions: std::sync::mpsc::Sender<present::model::Session>,
        commands: std::sync::mpsc::Sender<present::Command>,
    }

    impl Frontend for Recorder {
        fn nav(&mut self, _old: Option<&present::model::Nav>, _nav: &present::model::Nav) {}
        fn changes(
            &mut self,
            _old: Option<&present::model::Changes>,
            _changes: &present::model::Changes,
        ) {
        }
        fn session(
            &mut self,
            _old: Option<&present::model::Session>,
            session: &present::model::Session,
        ) {
            if self.sessions.send(session.clone()).is_err() {
                tracing::debug!("the test stopped listening");
            }
        }
        fn composer(
            &mut self,
            _old: Option<&present::model::Composer>,
            _composer: &present::model::Composer,
        ) {
        }
        fn sessions(
            &mut self,
            _old: Option<&present::model::Sessions>,
            _sessions: &present::model::Sessions,
        ) {
        }
        fn entries(&mut self, _change: present::EntriesChange) {}
        fn prompt(
            &mut self,
            _old: Option<&present::model::Prompt>,
            _prompt: &present::model::Prompt,
        ) {
        }
        fn peek(&mut self, _old: Option<&present::model::Peek>, _peek: &present::model::Peek) {}
        fn side(&mut self, _old: Option<&present::model::Side>, _side: &present::model::Side) {}
        fn preferences(
            &mut self,
            _old: Option<&present::model::Preferences>,
            _preferences: &present::model::Preferences,
        ) {
        }
        fn usage(&mut self, _old: Option<&present::model::Usage>, _usage: &present::model::Usage) {}
        fn transcript(
            &mut self,
            _old: Option<&present::model::Transcript>,
            _transcript: &present::model::Transcript,
        ) {
        }
        fn show_image(&mut self, _key: ImageKey, _shown: Result<&crate::decode::Pixels, String>) {}
        fn keep_images(&mut self, _shown: &HashSet<ImageKey>) {}
        fn command(&mut self, command: present::Command) {
            if self.commands.send(command).is_err() {
                tracing::debug!("the test stopped listening");
            }
        }
        fn draft(&self) -> String {
            String::new()
        }
    }

    /// An app as a window has it, in a folder trusted nowhere, and what it shows and sends the
    /// daemon, as the test reads them.
    struct Driven {
        app: App<Recorder>,
        sessions: std::sync::mpsc::Receiver<present::model::Session>,
        commands: std::sync::mpsc::Receiver<present::Command>,
        sent: Receiver<ToDaemon>,
        cwd: PathBuf,
    }

    impl Driven {
        fn new() -> Self {
            let (shown_sessions, sessions) = std::sync::mpsc::channel();
            let (shown_commands, commands) = std::sync::mpsc::channel();
            let (link, sent) = async_channel::bounded(link::QUEUE);
            let (events, _unread) = async_channel::unbounded();
            let cwd = PathBuf::from("/nowhere/edualc-app-test");
            let app = App::new(
                Recorder {
                    sessions: shown_sessions,
                    commands: shown_commands,
                },
                events,
                link,
                Desktop {
                    finds_windows: false,
                    text_size: 14.0,
                    backends: vec![provider::Backend::Claude],
                },
                Ok(Settings::default()),
                Start {
                    cwd: Some(cwd.clone()),
                    backend: provider::Backend::Claude,
                    model: None,
                },
            );
            Self {
                app,
                sessions,
                commands,
                sent,
                cwd,
            }
        }

        /// Takes `event` as the event loop does, and draws.
        fn take(&mut self, event: Event) {
            let flow = self.app.handle(event);
            assert!(flow.is_continue(), "the app quit");
            self.app.render();
        }

        /// What the app sent the daemon since last asked.
        fn sent(&self) -> Vec<ToDaemon> {
            std::iter::from_fn(|| self.sent.try_recv().ok()).collect()
        }

        /// The session as it was last shown.
        fn session(&self) -> present::model::Session {
            self.sessions
                .try_iter()
                .last()
                .expect("the session was shown")
        }

        /// What the window was told to do since last asked.
        fn commands(&self) -> Vec<present::Command> {
            self.commands.try_iter().collect()
        }
    }

    #[test]
    fn a_folder_is_trusted_before_its_session_starts() {
        let mut driven = Driven::new();
        driven.app.begin(Vec::new(), Vec::new(), None, None, None);
        let sent = driven.sent();
        let check = sent
            .iter()
            .find_map(|message| {
                if let ToDaemon::Act {
                    request,
                    act: Act::CheckTrust { folder },
                } = message
                    && *folder == driven.cwd
                {
                    Some(*request)
                } else {
                    None
                }
            })
            .expect("the daemon is asked about the folder");
        assert!(
            !sent
                .iter()
                .any(|message| matches!(message, ToDaemon::Start { .. })),
            "nothing starts before the folder is trusted"
        );
        driven.take(Event::Trusted {
            request: check,
            trust: crate::ipc::FolderTrust::Untrusted(vec![Runs::Hook {
                event: "SessionStart".to_owned(),
                matcher: String::new(),
                action: crate::ipc::HookAction::Command("./setup.sh".to_owned()),
            }]),
        });
        let asking = driven.session();
        assert!(asking.needs_trust);
        assert_eq!(asking.trust_runs, ["Hook on SessionStart: ./setup.sh"]);
        assert!(driven.sent().is_empty(), "still nothing starts");
        driven.take(Event::Action(Action::TrustFolder));
        let sent = driven.sent();
        assert!(sent.iter().any(|message| matches!(
            message,
            ToDaemon::ChangeSettings(settings::Change::Trust(folder)) if *folder == driven.cwd
        )));
        // As the card says, for Claude Code too.
        assert!(sent.iter().any(|message| matches!(
            message,
            ToDaemon::Act { act: Act::TrustForClaudeCode { folder }, .. } if *folder == driven.cwd
        )));
        assert!(
            sent.iter()
                .any(|message| matches!(message, ToDaemon::Start { .. }))
        );
        assert!(!driven.session().needs_trust);
        assert!(driven.commands().contains(&present::Command::FocusComposer));
    }

    #[test]
    fn a_folder_trusted_before_is_asked_about_once_for_claude_code() {
        let mut driven = Driven::new();
        driven
            .app
            .settings
            .apply(settings::Change::Trust(driven.cwd.clone()));
        driven.app.begin(Vec::new(), Vec::new(), None, None, None);
        let sent = driven.sent();
        let check = sent
            .iter()
            .find_map(|message| {
                if let ToDaemon::Act {
                    request,
                    act: Act::CheckClaudeTrust { folder },
                } = message
                    && *folder == driven.cwd
                {
                    Some(*request)
                } else {
                    None
                }
            })
            .expect("the daemon is asked whether Claude Code trusts it");
        assert!(
            !sent.iter().any(|message| matches!(
                message,
                ToDaemon::Act {
                    act: Act::TrustForClaudeCode { .. },
                    ..
                }
            )),
            "nothing is recorded unasked"
        );
        driven.take(Event::ClaudeTrustRead {
            request: check,
            trusted: Ok(false),
        });
        assert!(driven.session().asks_claude_trust);

        driven.take(Event::Action(Action::NotForClaudeCode));
        assert!(!driven.session().asks_claude_trust);
        assert!(driven.sent().iter().any(|message| matches!(
            message,
            ToDaemon::ChangeSettings(settings::Change::NotForClaudeCode(folder))
                if *folder == driven.cwd
        )));
        assert!(
            driven
                .app
                .settings
                .not_for_claude_code
                .contains(&driven.cwd)
        );
    }

    /// A session started in a folder trusted here, with the generation its process runs as.
    fn started() -> (Driven, Generation) {
        let mut driven = Driven::new();
        driven
            .app
            .settings
            .apply(settings::Change::Trust(driven.cwd.clone()));
        driven.app.begin(Vec::new(), Vec::new(), None, None, None);
        let generation = driven
            .sent()
            .iter()
            .find_map(|message| {
                if let ToDaemon::Start { key, .. } = message {
                    Some(*key)
                } else {
                    None
                }
            })
            .expect("the session starts");
        driven.take(Event::Agent {
            generation,
            output: Output::Started { pid: 7, turns: 0 },
        });
        (driven, generation)
    }

    #[test]
    fn a_turn_whose_result_cant_be_read_ends() {
        let (mut driven, generation) = started();
        driven.take(Event::Action(Action::Send("Count the lines".to_owned())));
        assert!(driven.session().busy, "the turn is under way");
        let error = serde_json::from_str::<wire::TurnResult>("{")
            .expect_err("a line cut short doesn't parse");
        driven.take(Event::Agent {
            generation,
            output: Output::Unreadable {
                lost: agent::Lost::Result,
                error: error.to_string(),
            },
        });
        assert!(!driven.session().busy, "the turn has ended");
        assert!(
            driven
                .app
                .chat
                .entries
                .iter()
                .any(|entry| entry.copy_text().contains("can't read")),
            "the conversation says why"
        );
    }

    #[test]
    fn an_agents_request_is_answered_with_its_choices() {
        let (mut driven, generation) = started();
        driven.take(Event::Action(Action::Send("Count the lines".to_owned())));
        driven.sent();
        let option = |id: &str, kind| AcpOption {
            id: call::OptionId::from(id.to_owned()),
            name: id.to_owned(),
            kind,
        };
        let mut ask = |request: &str| {
            driven.take(Event::Agent {
                generation,
                output: Output::Event(agent::Event::PermissionAsked {
                    request_id: RequestId::from(request.to_owned()),
                    tool_use_id: ToolUseId::from(format!("call-{request}")),
                    call: AcpCall {
                        title: "wc -l notes.txt".to_owned(),
                        kind: Some(call::AcpKind::Execute),
                        locations: Vec::new(),
                        diffs: Vec::new(),
                        input: None,
                        status: call::AcpStatus::Pending,
                        output: None,
                    },
                    options: vec![
                        option("once", AcpOptionKind::AllowOnce),
                        option("always", AcpOptionKind::AllowAlways),
                        option("reject", AcpOptionKind::RejectOnce),
                    ],
                }),
            });
        };
        ask("p1");
        ask("p2");
        ask("p3");
        let mut answer = |action| {
            driven.take(Event::Action(action));
            driven
                .sent()
                .into_iter()
                .filter_map(|message| {
                    if let ToDaemon::Command { text, .. } = message {
                        Some(serde_json::from_str::<agent::Command>(&text).expect("a command"))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        };
        let chosen = |commands: &[agent::Command]| match commands.first() {
            Some(agent::Command::Choose { option, .. }) => option.as_str().to_owned(),
            other => panic!("a choice, not {other:?}"),
        };
        assert_eq!(
            chosen(&answer(Action::Allow(RequestId::from("p1".to_owned())))),
            "once"
        );
        assert_eq!(
            chosen(&answer(Action::AllowAlways(RequestId::from(
                "p2".to_owned()
            )))),
            "always"
        );
        let denied = answer(Action::Deny {
            request: RequestId::from("p3".to_owned()),
            feedback: String::new(),
        });
        assert_eq!(chosen(&denied), "reject");
        assert_eq!(denied.len(), 1, "the reject alone: {denied:?}");
        assert!(!driven.app.status.stop_asked);
    }

    #[test]
    fn no_rule_is_offered_where_claude_code_offers_none() {
        let offered = |suppress: bool| {
            let (mut driven, generation) = started();
            let line = serde_json::json!({
                "type": "control_request",
                "request_id": "p1",
                "request": {
                    "subtype": "can_use_tool",
                    "tool_name": "Bash",
                    "input": {"command": "npm test"},
                    "permission_suggestions": [{
                        "type": "addRules", "destination": "localSettings",
                        "rules": [{"toolName": "Bash", "ruleContent": "npm test:*"}],
                    }],
                    "suppress_always_allow_rule": suppress,
                },
            });
            driven.take(Event::Agent {
                generation,
                output: crate::claude::output_of_line(line.to_string()),
            });
            match driven.app.asks.front().map(|ask| &ask.kind) {
                Some(AskKind::Tool { suggestions, .. }) => suggestions.len(),
                other => panic!("a tool's request, not {other:?}"),
            }
        };
        assert_eq!(offered(false), 1);
        assert_eq!(offered(true), 0);
    }

    #[test]
    fn a_message_turned_down_stops_nothing() {
        let (mut driven, generation) = started();
        driven.take(Event::Action(Action::Send("Count the lines".to_owned())));
        driven.sent();
        driven.take(Event::Agent {
            generation,
            output: crate::claude::output_of_line(
                r#"{"type":"control_request","request_id":"m1","request":{"subtype":"can_use_tool","tool_name":"edualc_incoming_message","input":{"from":"tidy-otter","message":"Hi"}}}"#
                    .to_owned(),
            ),
        });
        driven.take(Event::Action(Action::Deny {
            request: RequestId::from("m1".to_owned()),
            feedback: String::new(),
        }));
        let answered: String = driven
            .sent()
            .into_iter()
            .filter_map(|message| {
                if let ToDaemon::Command { text, .. } = message {
                    Some(text)
                } else {
                    None
                }
            })
            .collect();
        let command: agent::Command = serde_json::from_str(&answered).expect("a command");
        assert!(matches!(
            command,
            agent::Command::ControlResponse {
                response: wire::Reply::Success {
                    response: wire::PermissionResult::Deny {
                        interrupt: false,
                        ..
                    },
                    ..
                }
            }
        ));
        assert!(!driven.app.status.stop_asked);
        assert!(driven.session().busy, "the turn goes on");
    }

    #[test]
    fn a_held_message_is_told() {
        let (mut driven, generation) = started();
        let said = |driven: &Driven| -> Vec<String> {
            driven
                .app
                .chat
                .entries
                .iter()
                .map(|entry| entry.copy_text().to_owned())
                .collect()
        };
        for line in [
            r#"{"type":"system","subtype":"peer_message_hold","state":"held","message_uuid":"m","lane":"socket","from":"uds:/run/user/1000/cc-socks/9.sock","from_name":"calm-heron","cause":"mode-mismatch","uuid":"u1","session_id":"s"}"#,
            r#"{"type":"system","subtype":"peer_message_hold","state":"dropped","message_uuid":"m","lane":"socket","from":"uds:/run/user/1000/cc-socks/9.sock","from_name":"calm-heron","outcome":"expired","uuid":"u2","session_id":"s"}"#,
        ] {
            driven.take(Event::Agent {
                generation,
                output: crate::claude::output_of_line(line.to_owned()),
            });
        }
        let said = said(&driven);
        assert!(
            said.iter()
                .any(|text| text.contains("holding a message from calm-heron")
                    && text.contains("bypasses permissions")),
            "{said:?}"
        );
        assert!(
            said.iter()
                .any(|text| text.contains("from calm-heron expired")),
            "{said:?}"
        );
    }

    #[test]
    fn a_pasted_key_isnt_written_out() {
        let action = Action::StoreApiKey(PastedKey::new("sk-ant-abc_DEF"));
        assert!(!format!("{action:?}").contains("abc_DEF"));
    }

    #[test]
    fn a_turn_a_message_started_counts_as_under_way() {
        let mut status = Status::default();
        assert!(!status.in_turn());
        status.doing = Doing::Thinking;
        assert!(
            status.in_turn(),
            "working on a turn the window didn't prompt"
        );
        status.doing = Doing::Nothing;
        status.turns = 1;
        assert!(status.in_turn(), "a turn the window prompted");
    }

    #[test]
    fn each_session_shows_its_own_plans_limits() {
        let used = LimitUse {
            used: 0.9,
            resets_at: None,
        };
        let five_hours = |status: &Status| status.plan_limits().and_then(|limits| limits.five_hour);
        let mut status = Status::default();
        if let Some(limits) = status.limits_mut() {
            limits.five_hour = Some(used);
        }
        assert!(five_hours(&status).is_some());
        status.sign_in_with = provider::SignInWith::ApiKey;
        assert!(status.plan_limits().is_none());
        status.backend = provider::Backend::Agy;
        status.sign_in_with = provider::SignInWith::Account;
        assert!(
            five_hours(&status).is_none(),
            "Claude Code's limits aren't agy's"
        );
        if let Some(limits) = status.limits_mut() {
            limits.five_hour = Some(used);
        }
        assert!(five_hours(&status).is_some());
        status.backend = provider::Backend::Opencode;
        assert!(status.plan_limits().is_none());
        assert!(status.limits_mut().is_none());
    }

    #[test]
    fn a_model_is_kept_for_the_program_it_was_picked_for() {
        let mut status = Status {
            backend: provider::Backend::Claude,
            model_choice: Some(ModelChoice {
                backend: provider::Backend::Claude,
                model: "sonnet".to_owned(),
            }),
            ..Status::default()
        };
        assert_eq!(status.chosen_model(), Some("sonnet"));
        status.backend = provider::Backend::Agy;
        assert_eq!(status.chosen_model(), None);
        status.backend = provider::Backend::Claude;
        assert_eq!(status.chosen_model(), Some("sonnet"));
    }

    #[test]
    fn a_mode_is_asked_for_in_its_programs_terms() {
        let initialized: wire::Initialized = serde_json::from_value(serde_json::json!({
            "models": [],
            "programModes": {
                "offered": [
                    {"id": "default", "name": "Default", "description": "Ask before acting"},
                    {"id": "accept-edits", "name": "Accept edits", "description": ""}
                ],
                "current": "default"
            }
        }))
        .expect("an agy adapter's answer");
        let program = initialized.program_modes.expect("agy's modes");
        let accept_edits = program
            .offered
            .get(1)
            .map(|mode| mode.id.clone())
            .expect("accept-edits offered");
        let mut modes = Modes::Program {
            offered: program.offered,
            current: program.current,
        };

        let agys = ChosenMode::Program(accept_edits.clone());
        assert_eq!(
            serde_json::to_value(agys.request()).expect("a request"),
            serde_json::json!({"subtype": "set_program_mode", "mode": "accept-edits"})
        );
        assert_eq!(modes.label(&agys), "Accept edits");
        modes.chose(&agys);
        modes.chose(&ChosenMode::Claude(PermissionMode::Plan));
        modes.claude_in(Mode::Known(PermissionMode::Plan));
        assert!(
            matches!(&modes, Modes::Program { current: Some(current), .. } if *current == accept_edits),
            "Claude Code's modes leave agy's as they are"
        );

        let claudes = ChosenMode::Claude(PermissionMode::AcceptEdits);
        assert_eq!(
            serde_json::to_value(claudes.request()).expect("a request"),
            serde_json::json!({"subtype": "set_permission_mode", "mode": "acceptEdits"})
        );
        let mut modes = Modes::of(provider::Backend::Claude);
        modes.chose(&agys);
        assert_eq!(
            modes,
            Modes::Claude(None),
            "agy's modes leave Claude Code's as they are"
        );
        modes.chose(&claudes);
        assert_eq!(
            modes,
            Modes::Claude(Some(Mode::Known(PermissionMode::AcceptEdits)))
        );

        let changed: wire::Message = serde_json::from_value(serde_json::json!({
            "type": "system",
            "subtype": "program_mode",
            "mode": "plan",
            "session_id": "11111111-2222-4333-8444-555555555555"
        }))
        .expect("an agy adapter's line");
        assert!(matches!(
            changed,
            wire::Message::System(wire::System::ProgramMode { mode }) if mode.to_string() == "plan"
        ));
    }

    #[test]
    fn up_and_down_step_through_the_prompts() {
        let mut recall = Recall::Writing;
        assert!(!recall.has_older(0));
        assert_eq!(recall.older(0, "half a thought"), None);
        assert_eq!(recall, Recall::Writing);
        assert_eq!(recall.newer(3), None);

        assert!(recall.has_older(3));
        assert_eq!(recall.older(3, "half a thought"), Some(2));
        assert_eq!(recall.older(3, "the newest, shown"), Some(1));
        assert_eq!(recall.older(3, "the middle, shown"), Some(0));
        assert!(!recall.has_older(3));
        assert_eq!(recall.older(3, "the oldest, shown"), None);
        assert_eq!(
            recall,
            Recall::Recalled {
                index: 0,
                draft: "half a thought".to_owned()
            },
            "Up at the oldest stays there, the draft kept"
        );

        assert_eq!(recall.newer(3), Some(Recalled::Prompt(1)));
        assert_eq!(recall.newer(3), Some(Recalled::Prompt(2)));
        assert_eq!(
            recall.newer(3),
            Some(Recalled::Draft("half a thought".to_owned()))
        );
        assert_eq!(recall, Recall::Writing);
    }

    #[test]
    fn a_refused_sign_in_asks_the_user() {
        let refused = SignIn::Refused {
            failed: Some(SentPrompt {
                text: "edit the notes".to_owned(),
                images: Vec::new(),
            }),
        };
        assert!(refused.waits_for_user());
        assert!(SignIn::Asked.waits_for_user());
        assert!(!SignIn::SignedIn.waits_for_user());
        assert!(!SignIn::Unknown.waits_for_user());
    }

    #[test]
    fn a_prompt_sent_while_trust_is_read_goes_once_trusted() {
        let first = ActId::default();
        let checking = || TrustGate::Checking {
            request: first,
            resume: None,
            held: None,
        };

        let mut gate = checking();
        gate.hold("edit the notes");
        assert_eq!(
            gate.answered(first, crate::ipc::FolderTrust::Accepted),
            TrustAnswer::Trusted {
                resume: None,
                held: Some("edit the notes".to_owned())
            }
        );
        assert!(matches!(gate, TrustGate::Open));

        let mut gate = checking();
        assert_eq!(
            gate.answered(first, crate::ipc::FolderTrust::Accepted),
            TrustAnswer::Trusted {
                resume: None,
                held: None
            }
        );

        let mut gate = checking();
        gate.hold("edit the notes");
        assert_eq!(
            gate.answered(first, crate::ipc::FolderTrust::Untrusted(Vec::new())),
            TrustAnswer::Asked
        );
        gate.hold("sent again before deciding");
        assert!(matches!(gate, TrustGate::Asking { .. }));

        let mut gate = checking();
        gate.hold("edit the notes");
        assert_eq!(
            gate.answered(first.next(), crate::ipc::FolderTrust::Accepted),
            TrustAnswer::Stale
        );
        assert!(
            matches!(&gate, TrustGate::Checking { held: Some(held), .. } if held == "edit the notes")
        );
    }

    #[test]
    fn a_draft_is_read_by_how_it_starts() {
        let shell = |draft: &str| shell_command(&normalized(draft)).map(str::to_owned);
        assert_eq!(shell("!ls -a"), Some("ls -a".to_owned()));
        assert_eq!(shell("！ls"), Some("ls".to_owned()));
        assert_eq!(shell("![screenshot](shot.png) why is this blank?"), None);
        assert_eq!(shell("hi! there"), None);
        assert_eq!(normalized("／clear"), "/clear");
        assert_eq!(normalized("今日は！"), "今日は！");
        assert!(matches!(
            normalized("plain"),
            std::borrow::Cow::Borrowed("plain")
        ));
    }

    #[test]
    fn silence_is_told_while_an_answer_is_awaited() {
        let half_a_minute_on = Duration::from_millis(31_400);
        assert_eq!(
            gone_quiet(&Doing::Starting, false, half_a_minute_on),
            Some(Duration::from_secs(31))
        );
        assert_eq!(
            gone_quiet(
                &Doing::Silent(Duration::from_secs(31)),
                false,
                Duration::from_secs(45)
            ),
            Some(Duration::from_secs(45))
        );
        assert_eq!(
            gone_quiet(&Doing::Starting, false, Duration::from_secs(29)),
            None
        );
        assert_eq!(gone_quiet(&Doing::Starting, true, half_a_minute_on), None);
        assert_eq!(
            gone_quiet(&Doing::Compacting, false, half_a_minute_on),
            None
        );
        assert_eq!(gone_quiet(&Doing::Nothing, false, half_a_minute_on), None);
        let retry = Retry {
            attempt: 2,
            most: 10,
            status: None,
            cause: wire::RetryCause::Unknown,
        };
        assert_eq!(
            gone_quiet(&Doing::Retrying(retry), false, half_a_minute_on),
            None
        );
    }

    #[test]
    fn a_retry_says_why() {
        let retry = |status, cause| Retry {
            attempt: 3,
            most: 10,
            status,
            cause,
        };
        assert_eq!(
            retry(None, wire::RetryCause::Unknown).to_string(),
            "Retrying 3/10 · no connection to the API"
        );
        assert_eq!(
            retry(Some(529), wire::RetryCause::Overloaded).to_string(),
            "Retrying 3/10 · the API is overloaded"
        );
        assert_eq!(
            retry(Some(502), wire::RetryCause::ServerError).to_string(),
            "Retrying 3/10 · the API answered 502"
        );
    }

    #[test]
    fn a_command_hints_what_it_takes_until_something_follows() {
        let commands: Vec<wire::CommandInfo> = serde_json::from_value(serde_json::json!([
            { "name": "compact", "argumentHint": "[instructions]" },
            { "name": "cost" },
        ]))
        .expect("commands");
        assert_eq!(argument_hint("/compact ", &commands), "[instructions]");
        assert_eq!(argument_hint("/compact", &commands), "");
        assert_eq!(argument_hint("/compact k", &commands), "");
        assert_eq!(argument_hint("/compact  ", &commands), "");
        assert_eq!(argument_hint("/cost ", &commands), "");
        assert_eq!(argument_hint("/unknown ", &commands), "");
        assert_eq!(argument_hint("compact ", &commands), "");
    }

    #[test]
    fn the_first_prompt_names_a_session() {
        assert_eq!(
            provisional_title("\n  Fix the flaky test \nin CI"),
            Some("Fix the flaky test")
        );
        assert_eq!(
            provisional_title("このテストを直して"),
            Some("このテストを直して")
        );
        assert_eq!(provisional_title("/compact keep the plan"), None);
        assert_eq!(provisional_title("／clear"), None);
        assert_eq!(provisional_title(" \n "), None);
    }

    #[test]
    fn answers_carry_notes_and_the_picked_preview() {
        let asked: wire::Questions = serde_json::from_value(serde_json::json!({
            "questions": [
                { "question": "Layout?", "options": [
                    { "label": "Grid", "preview": "▦" }, { "label": "List", "preview": "☰" } ] },
                { "question": "Name?", "options": [ { "label": "A", "preview": "a" } ] },
                { "question": "Which?", "multiSelect": true, "options": [
                    { "label": "X", "preview": "x" }, { "label": "Y", "preview": "y" } ] },
                { "question": "Plain?", "options": [ { "label": "Yes" } ] },
            ]
        }))
        .expect("questions");
        let picked = [vec![false, true], vec![true], vec![true, true], vec![true]];
        let other = [
            String::new(),
            "B instead".into(),
            String::new(),
            String::new(),
        ];
        let notes = [
            "  denser  ".into(),
            String::new(),
            "both".into(),
            String::new(),
        ];
        let given = answered(&asked, &picked, &other, &notes);
        assert_eq!(
            given.answers.get("Layout?").map(String::as_str),
            Some("List")
        );
        assert_eq!(
            given.answers.get("Name?").map(String::as_str),
            Some("B instead")
        );
        assert_eq!(
            given.answers.get("Which?").map(String::as_str),
            Some("X, Y")
        );
        assert_eq!(
            given.annotations.get("Layout?"),
            Some(&wire::Annotation {
                notes: Some("denser".into()),
                preview: Some("☰".into()),
            })
        );
        assert_eq!(given.annotations.get("Name?"), None, "typed: no preview");
        assert_eq!(
            given.annotations.get("Which?"),
            Some(&wire::Annotation {
                notes: Some("both".into()),
                preview: None,
            }),
            "two picked: no preview"
        );
        assert_eq!(given.annotations.get("Plain?"), None);
    }

    #[test]
    fn changes_during_a_read_are_read_after_it() {
        let mut refresh = Refresh::default();
        assert!(refresh.wanted(), "the first change starts a read");
        assert!(!refresh.wanted(), "one while reading waits");
        assert!(!refresh.wanted());
        assert!(refresh.done(), "the read came back stale: ask again");
        assert!(refresh.wanted(), "and asking again starts a read");
        assert!(!refresh.done(), "this one wasn't stale");
        assert!(refresh.wanted(), "a later change starts a read again");
    }

    #[cfg(not(target_arch = "wasm32"))]
    struct RecordingFrontend {
        navs: std::sync::mpsc::Sender<crate::present::model::Nav>,
        commands: std::sync::mpsc::Sender<Command>,
    }

    #[cfg(not(target_arch = "wasm32"))]
    impl Frontend for RecordingFrontend {
        fn nav(
            &mut self,
            _old: Option<&crate::present::model::Nav>,
            nav: &crate::present::model::Nav,
        ) {
            if self.navs.send(nav.clone()).is_err() {
                // Tests stop reading updates when the harness drops.
            }
        }
        fn session(
            &mut self,
            _old: Option<&crate::present::model::Session>,
            _session: &crate::present::model::Session,
        ) {
        }
        fn composer(
            &mut self,
            _old: Option<&crate::present::model::Composer>,
            _composer: &crate::present::model::Composer,
        ) {
        }
        fn sessions(
            &mut self,
            _old: Option<&crate::present::model::Sessions>,
            _sessions: &crate::present::model::Sessions,
        ) {
        }
        fn entries(&mut self, _change: crate::present::EntriesChange) {}
        fn prompt(
            &mut self,
            _old: Option<&crate::present::model::Prompt>,
            _prompt: &crate::present::model::Prompt,
        ) {
        }
        fn peek(
            &mut self,
            _old: Option<&crate::present::model::Peek>,
            _peek: &crate::present::model::Peek,
        ) {
        }
        fn changes(
            &mut self,
            _old: Option<&crate::present::model::Changes>,
            _changes: &crate::present::model::Changes,
        ) {
        }
        fn side(
            &mut self,
            _old: Option<&crate::present::model::Side>,
            _side: &crate::present::model::Side,
        ) {
        }
        fn preferences(
            &mut self,
            _old: Option<&crate::present::model::Preferences>,
            _preferences: &crate::present::model::Preferences,
        ) {
        }
        fn usage(
            &mut self,
            _old: Option<&crate::present::model::Usage>,
            _usage: &crate::present::model::Usage,
        ) {
        }
        fn transcript(
            &mut self,
            _old: Option<&crate::present::model::Transcript>,
            _transcript: &crate::present::model::Transcript,
        ) {
        }
        fn show_image(&mut self, _key: ImageKey, _shown: Result<&Pixels, String>) {}
        fn keep_images(&mut self, _shown: &HashSet<ImageKey>) {}
        fn command(&mut self, command: Command) {
            if self.commands.send(command).is_err() {
                // Tests stop reading commands when the harness drops.
            }
        }
        fn draft(&self) -> String {
            String::new()
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    struct TestApp {
        app: App<RecordingFrontend>,
        to_daemon: Receiver<ToDaemon>,
        navs: std::sync::mpsc::Receiver<crate::present::model::Nav>,
        last_nav: crate::present::model::Nav,
        commands: std::sync::mpsc::Receiver<Command>,
        _events_rx: Receiver<Event>,
    }

    #[cfg(not(target_arch = "wasm32"))]
    impl TestApp {
        fn start(cwd: Option<PathBuf>, settings: Settings) -> Self {
            let (events_tx, events_rx) = async_channel::bounded(64);
            let (link_tx, link_rx) = async_channel::bounded(link::QUEUE);
            let (nav_tx, nav_rx) = std::sync::mpsc::channel();
            let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
            let app = App::new(
                RecordingFrontend {
                    navs: nav_tx,
                    commands: cmd_tx,
                },
                events_tx,
                link_tx,
                Desktop {
                    finds_windows: false,
                    text_size: 14.0,
                    backends: vec![provider::Backend::Claude],
                },
                Ok(settings),
                Start {
                    cwd,
                    backend: provider::Backend::Claude,
                    model: None,
                },
            );
            Self {
                app,
                to_daemon: link_rx,
                navs: nav_rx,
                last_nav: crate::present::model::Nav::default(),
                commands: cmd_rx,
                _events_rx: events_rx,
            }
        }

        fn send(&mut self, event: Event) {
            let flow = self.app.handle(event);
            assert!(flow.is_continue());
            self.app.render();
        }

        fn drain_daemon(&self) -> Vec<ToDaemon> {
            std::iter::from_fn(|| self.to_daemon.try_recv().ok()).collect()
        }

        fn drain_commands(&self) -> Vec<Command> {
            self.commands.try_iter().collect()
        }

        fn latest_nav(&mut self) -> &crate::present::model::Nav {
            for next in self.navs.try_iter() {
                self.last_nav = next;
            }
            &self.last_nav
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn starts_on_start_page_when_no_folder_is_given() {
        let mut test = TestApp::start(None, Settings::default());
        test.app.begin(Vec::new(), Vec::new(), None, None, None);

        assert!(matches!(&test.app.location, Location::Start));
        let nav = test.latest_nav();
        assert_eq!(nav.view, PlaceView::Start);
        assert!(!nav.can_back);

        let sent = test.drain_daemon();
        assert!(sent.iter().any(|msg| matches!(
            msg,
            ToDaemon::Read {
                read: Read::Overview,
                ..
            }
        )));
        assert!(!sent.iter().any(|msg| matches!(
            msg,
            ToDaemon::Start { .. }
                | ToDaemon::Act {
                    act: Act::CheckTrust { .. },
                    ..
                }
        )));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn entering_a_project_loads_its_sessions_before_checking_trust() {
        let mut test = TestApp::start(None, Settings::default());
        test.app.begin(Vec::new(), Vec::new(), None, None, None);
        drop(test.drain_daemon());
        drop(test.drain_commands());

        let proj = PathBuf::from("/tmp/proj-a");
        test.send(Event::Action(Action::EnterProject(proj.clone())));
        assert!(matches!(&test.app.location, Location::Project(path) if path == &proj));
        let nav = test.latest_nav();
        assert_eq!(nav.view, PlaceView::Project);
        assert_eq!(nav.project_name, "proj-a");
        assert_eq!(nav.project_folder, "/tmp/proj-a");
        assert!(nav.can_back);
        assert!(
            test.drain_commands()
                .iter()
                .any(|cmd| matches!(cmd, Command::FocusPage))
        );

        let sent = test.drain_daemon();
        assert!(sent.iter().any(|msg| matches!(
            msg,
            ToDaemon::Read {
                read: Read::Sessions { cwd, .. },
                ..
            } if cwd == &proj
        )));
        assert!(!sent.iter().any(|msg| matches!(
            msg,
            ToDaemon::Start { .. }
                | ToDaemon::Act {
                    act: Act::CheckTrust { .. },
                    ..
                }
        )));

        test.send(Event::Action(Action::NewSession(
            provider::Backend::Claude,
            provider::SignInWith::Account,
        )));
        assert!(matches!(&test.app.location, Location::Session(path) if path == &proj));
        assert_eq!(test.latest_nav().view, PlaceView::Session);
        assert!(test.drain_daemon().iter().any(|msg| matches!(
            msg,
            ToDaemon::Act {
                act: Act::CheckTrust { folder },
                ..
            } if folder == &proj
        )));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_line_reached_again_keeps_a_projects_page() {
        let proj = PathBuf::from("/tmp/proj-reached-again");
        let settings = Settings {
            trusted_folders: vec![proj.clone()],
            ..Settings::default()
        };
        let mut test = TestApp::start(None, settings);
        test.app.begin(Vec::new(), Vec::new(), None, None, None);
        test.send(Event::Action(Action::EnterProject(proj.clone())));
        drop(test.drain_daemon());

        test.send(Event::LineLost);
        test.send(Event::Reconnected {
            hosted: Vec::new(),
            live: Vec::new(),
        });
        assert!(matches!(&test.app.location, Location::Project(path) if path == &proj));
        assert_eq!(test.latest_nav().view, PlaceView::Project);
        assert!(
            !test
                .drain_daemon()
                .iter()
                .any(|msg| matches!(msg, ToDaemon::Start { .. }))
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn back_steps_from_session_to_project_to_start() {
        let proj = PathBuf::from("/tmp/proj-b");
        let settings = Settings {
            trusted_folders: vec![proj.clone()],
            ..Settings::default()
        };
        let mut test = TestApp::start(Some(proj.clone()), settings);
        test.app.begin(Vec::new(), Vec::new(), None, None, None);

        assert!(matches!(&test.app.location, Location::Session(path) if path == &proj));
        assert_eq!(test.latest_nav().view, PlaceView::Session);
        assert!(test.latest_nav().can_back);

        test.send(Event::Action(Action::Back));
        assert!(matches!(&test.app.location, Location::Project(path) if path == &proj));
        assert_eq!(test.latest_nav().view, PlaceView::Project);
        assert!(test.latest_nav().can_back);

        test.send(Event::Action(Action::Back));
        assert!(matches!(&test.app.location, Location::Start));
        assert_eq!(test.latest_nav().view, PlaceView::Start);
        assert!(!test.latest_nav().can_back);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn creating_a_project_trusts_it_and_starts_a_session() {
        let mut test = TestApp::start(None, Settings::default());
        test.app.begin(Vec::new(), Vec::new(), None, None, None);
        drop(test.drain_daemon());
        drop(test.drain_commands());

        // A name that isn't one folder is refused here, under the field, without the daemon.
        test.send(Event::Action(Action::CreateProject {
            parent: "/tmp".to_owned(),
            name: " .. ".to_owned(),
        }));
        assert!(
            !test
                .drain_daemon()
                .iter()
                .any(|msg| matches!(msg, ToDaemon::Act { .. }))
        );
        assert_eq!(
            test.latest_nav().new_project_error,
            crate::ipc::NotAProjectName::Dots.to_string()
        );

        test.send(Event::Action(Action::CreateProject {
            parent: "/tmp".to_owned(),
            name: " fresh-app ".to_owned(),
        }));
        assert_eq!(test.latest_nav().new_project_error, "");
        let first_request = test
            .drain_daemon()
            .into_iter()
            .find_map(|msg| {
                if let ToDaemon::Act {
                    request,
                    act: Act::CreateProject { parent, name },
                } = msg
                {
                    assert_eq!(parent, Path::new("/tmp"));
                    assert_eq!(name.as_str(), "fresh-app");
                    Some(request)
                } else {
                    None
                }
            })
            .expect("Act::CreateProject");

        let created_path = PathBuf::from("/tmp/fresh-app");
        test.send(Event::ProjectCreated {
            request: first_request,
            result: Err(CreateProjectError::AlreadyExists(created_path.clone())),
        });
        assert!(matches!(&test.app.location, Location::Start));
        assert!(!test.latest_nav().new_project_error.is_empty());

        test.send(Event::Action(Action::CreateProject {
            parent: "/tmp".to_owned(),
            name: "fresh-app".to_owned(),
        }));
        let second_request = test
            .drain_daemon()
            .into_iter()
            .find_map(|msg| {
                if let ToDaemon::Act {
                    request,
                    act: Act::CreateProject { .. },
                } = msg
                {
                    Some(request)
                } else {
                    None
                }
            })
            .expect("second Act::CreateProject");

        test.send(Event::ProjectCreated {
            request: second_request,
            result: Ok(created_path.clone()),
        });
        assert!(test.app.settings.trusted_folders.contains(&created_path));
        assert!(matches!(&test.app.location, Location::Session(path) if path == &created_path));
        assert_eq!(test.latest_nav().view, PlaceView::Session);
        assert!(
            test.drain_commands()
                .iter()
                .any(|cmd| matches!(cmd, Command::NewProjectCreated))
        );
        assert!(
            test.drain_daemon()
                .iter()
                .any(|msg| matches!(msg, ToDaemon::Start { .. }))
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn next_waiting_jumps_to_the_waiting_session() {
        let mut test = TestApp::start(None, Settings::default());
        test.app.begin(Vec::new(), Vec::new(), None, None, None);
        drop(test.drain_daemon());

        let wait_id = SessionId::new_random();
        let proj = PathBuf::from("/tmp/proj-wait");
        test.send(Event::OverviewListed(vec![ProjectOverview {
            path: proj.clone(),
            modified: std::time::SystemTime::UNIX_EPOCH,
            sessions: vec![SessionInfo {
                backend: provider::Backend::Claude,
                id: wait_id.clone(),
                title: "Fix parser".to_owned(),
                modified: std::time::SystemTime::UNIX_EPOCH,
                worktree: None,
                last_said: Some("Ready for review.".to_owned()),
            }],
        }]));
        test.send(Event::LiveListed(vec![LiveSession {
            pid: 42_001,
            started: "100".to_owned(),
            id: wait_id.clone(),
            cwd: proj.clone(),
            name: "brave-owl-proj-wait-12".to_owned(),
            title: Some("Fix parser".to_owned()),
            kind: sessions::LiveKind::Interactive,
            status: LiveStatus::Waiting,
            worktree: None,
            agents: 0,
            backend: provider::Backend::Claude,
        }]));
        assert_eq!(test.latest_nav().needs_you, 1);

        test.send(Event::Action(Action::NextWaiting));
        assert!(matches!(&test.app.location, Location::Session(path) if path == &proj));
        assert_eq!(test.latest_nav().view, PlaceView::Session);
        assert_eq!(
            test.app.watch.as_ref().map(|watch| &watch.session.id),
            Some(&wait_id)
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_new_session_the_daemon_runs_is_attached_without_an_error() {
        let mut test = TestApp::start(None, Settings::default());
        test.app.begin(Vec::new(), Vec::new(), None, None, None);
        let id = SessionId::try_from("8e1c72c8-83ea-4639-a986-1ad80b839271".to_owned())
            .expect("a session id");
        let hosted = Hosted {
            id: ipc::HostedId::default().next(),
            session: id.clone(),
            cwd: PathBuf::from("/code/beep"),
            backend: provider::Backend::Claude,
            sign_in: provider::SignInWith::Account,
            attached: false,
            state: ipc::HostedState::Idle,
        };
        test.send(Event::Hosted(vec![hosted.clone()]));
        drop(test.drain_daemon());

        test.send(Event::Action(Action::OpenSession(id.clone())));
        assert!(test.drain_daemon().iter().any(|msg| matches!(
            msg,
            ToDaemon::Attach { hosted: attached, .. } if *attached == hosted.id
        )));
        assert!(test.app.watch.is_none());

        let path = PathBuf::from("/nowhere/8e1c72c8-83ea-4639-a986-1ad80b839271.jsonl");
        test.send(Event::SessionLoaded {
            id: id.clone(),
            path: path.clone(),
            result: Err(ipc::Unread::NotWritten),
        });
        assert!(test.app.chat.entries.is_empty());
        test.send(Event::SessionLoaded {
            id,
            path,
            result: Err(ipc::Unread::Failed("Permission denied".to_owned())),
        });
        assert_eq!(test.app.chat.entries.len(), 1);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn the_changes_view_reads_shows_and_sends_comments() {
        use crate::changes::{
            ChangeSet, Diff, DiffLine, FileChange, Hunk, Kind, LineKind, Review, Scope, Who,
        };
        let mut test = TestApp::start(None, Settings::default());
        test.app.begin(Vec::new(), Vec::new(), None, None, None);
        let session = SessionId::try_from("8e1c72c8-83ea-4639-a986-1ad80b839271".to_owned())
            .expect("a session id");
        test.app.status.session_id = Some(session.clone());
        drop(test.drain_daemon());

        test.send(Event::Action(Action::ShowChanges));
        assert!(test.drain_daemon().iter().any(|msg| matches!(
            msg,
            ToDaemon::Read {
                read: Read::Changes {
                    scope: Scope::Turn,
                    ..
                },
                ..
            }
        )));
        let root = PathBuf::from("/code/beep");
        test.send(Event::ChangesRead {
            session: session.clone(),
            scope: Scope::Turn,
            result: Ok(ChangeSet {
                files: vec![FileChange {
                    root: root.clone(),
                    path: PathBuf::from("calc.py"),
                    kind: Kind::Changed,
                    who: Who::Agent,
                    added: 1,
                    removed: 0,
                    review: Review::NotReviewed,
                    now: None,
                }],
                left_out: crate::changes::LeftOut::default(),
            }),
        });
        assert!(test.drain_daemon().iter().any(|msg| matches!(
            msg,
            ToDaemon::Read { read: Read::Diff { path, .. }, .. } if path == Path::new("calc.py")
        )));
        test.send(Event::DiffRead {
            session: session.clone(),
            root,
            path: PathBuf::from("calc.py"),
            result: Ok(Diff {
                hunks: vec![Hunk {
                    lines: vec![DiffLine {
                        kind: LineKind::Added,
                        old: None,
                        new: Some(3),
                        text: "    return x * x".to_owned(),
                    }],
                }],
                cut: false,
            }),
        });
        test.send(Event::Action(Action::SetLineComment(
            0,
            "Use a docstring.".to_owned(),
        )));
        assert_eq!(test.app.line_comments.len(), 1);
        // The row of the gap between hunks, or past the end, takes no comment.
        test.send(Event::Action(Action::SetLineComment(5, "Lost.".to_owned())));
        assert_eq!(test.app.line_comments.len(), 1);

        test.send(Event::Action(Action::SendLineComments));
        assert!(test.app.line_comments.is_empty());
        assert!(test.app.changes.is_none());
        assert!(test.drain_daemon().iter().any(|msg| matches!(
            msg,
            ToDaemon::Act { act: Act::ChangesLooked { session: looked }, .. } if *looked == session
        )));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn open_session_with_api_key_choice_enters_session_view() {
        let mut test = TestApp::start(None, Settings::default());
        test.app.begin(Vec::new(), Vec::new(), None, None, None);
        test.send(Event::ApiKeyStored(Ok(true)));
        let proj = PathBuf::from("/tmp/proj-signin");
        let id = SessionId::new_random();
        test.send(Event::Action(Action::EnterProject(proj.clone())));
        test.send(Event::SessionsListed {
            cwd: proj.clone(),
            sessions: vec![sessions::SessionInfo {
                backend: provider::Backend::Claude,
                id: id.clone(),
                title: "Chat".to_owned(),
                modified: std::time::SystemTime::UNIX_EPOCH,
                worktree: None,
                last_said: None,
            }],
        });
        drop(test.drain_daemon());

        test.send(Event::Action(Action::OpenSession(id.clone())));
        assert_eq!(test.app.choosing_sign_in.as_ref(), Some(&id));
        assert!(matches!(&test.app.location, Location::Session(path) if path == &proj));
        assert_eq!(test.latest_nav().view, PlaceView::Session);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn let_go_clears_attached_so_reopening_hosted_session_attaches() {
        let mut test = TestApp::start(None, Settings::default());
        let proj = PathBuf::from("/tmp/proj-reattach");
        test.app.begin(Vec::new(), Vec::new(), None, None, None);
        let id = SessionId::new_random();
        let hosted = Hosted {
            id: ipc::HostedId::default().next(),
            session: id.clone(),
            cwd: proj.clone(),
            backend: provider::Backend::Claude,
            sign_in: provider::SignInWith::Account,
            attached: false,
            state: ipc::HostedState::Idle,
        };
        test.send(Event::Hosted(vec![hosted.clone()]));
        drop(test.drain_daemon());

        test.send(Event::Action(Action::OpenSession(id.clone())));
        let attach_key = test
            .drain_daemon()
            .into_iter()
            .find_map(|msg| {
                if let ToDaemon::Attach { key, .. } = msg {
                    Some(key)
                } else {
                    None
                }
            })
            .expect("attach sent");
        test.send(Event::Hosted(vec![Hosted {
            attached: true,
            ..hosted.clone()
        }]));
        test.send(Event::Agent {
            generation: attach_key,
            output: agent::Output::Started { pid: 123, turns: 0 },
        });
        drop(test.drain_daemon());

        test.send(Event::Action(Action::EnterProject(proj)));
        assert!(
            test.app
                .hosted
                .iter()
                .all(|item| item.id != hosted.id || !item.attached)
        );
        drop(test.drain_daemon());

        test.send(Event::Action(Action::OpenSession(id)));
        assert!(test.app.watch.is_none());
        assert!(test.drain_daemon().iter().any(|msg| matches!(
            msg,
            ToDaemon::Attach { hosted: attached, .. } if *attached == hosted.id
        )));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn entering_project_resets_backend_and_new_session_in_project_starts_cleanly() {
        let mut test = TestApp::start(None, Settings::default());
        test.app.begin(Vec::new(), Vec::new(), None, None, None);
        test.app.status.backend = provider::Backend::Agy;
        test.app.status.sign_in_with = provider::SignInWith::ApiKey;
        test.app.choosing_sign_in = Some(SessionId::new_random());

        let proj = PathBuf::from("/tmp/proj-reset");
        test.send(Event::Action(Action::EnterProject(proj.clone())));
        assert_eq!(test.app.status.backend, provider::Backend::Claude);
        assert_eq!(test.app.status.sign_in_with, provider::SignInWith::Account);
        assert!(test.app.choosing_sign_in.is_none());
        drop(test.drain_daemon());

        test.send(Event::Action(Action::NewSessionInProject(
            provider::Backend::Claude,
            provider::SignInWith::Account,
        )));
        assert!(matches!(&test.app.location, Location::Session(path) if path == &proj));
        assert!(test.app.choosing_sign_in.is_none());
        assert!(test.drain_daemon().iter().any(|msg| matches!(
            msg,
            ToDaemon::Act { act: Act::CheckTrust { folder }, .. } if folder == &proj
        )));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn watch_live_preserves_backend_and_changes_keeps_comments_when_watching() {
        use crate::changes::{
            ChangeSet, Diff, DiffLine, FileChange, Hunk, Kind, LineKind, Review, Scope, Who,
        };
        let mut test = TestApp::start(None, Settings::default());
        test.app.begin(Vec::new(), Vec::new(), None, None, None);
        let proj = PathBuf::from("/tmp/proj-watch-agy");
        let id = SessionId::new_random();
        let live = LiveSession {
            pid: 9001,
            started: "100".to_owned(),
            id: id.clone(),
            cwd: proj.clone(),
            name: "agy-live".to_owned(),
            title: Some("Agy chat".to_owned()),
            kind: sessions::LiveKind::Interactive,
            status: LiveStatus::Waiting,
            worktree: None,
            agents: 0,
            backend: provider::Backend::Agy,
        };
        test.send(Event::LiveListed(vec![live]));
        test.send(Event::Action(Action::EnterProject(proj.clone())));
        test.send(Event::Action(Action::OpenSession(id.clone())));
        assert_eq!(test.app.status.backend, provider::Backend::Agy);
        assert!(test.app.watch.is_some());

        test.send(Event::Action(Action::ShowChanges));
        test.send(Event::ChangesRead {
            session: id.clone(),
            scope: Scope::Turn,
            result: Ok(ChangeSet {
                files: vec![FileChange {
                    root: proj.clone(),
                    path: PathBuf::from("lib.rs"),
                    kind: Kind::Changed,
                    who: Who::Agent,
                    added: 1,
                    removed: 0,
                    review: Review::NotReviewed,
                    now: None,
                }],
                left_out: crate::changes::LeftOut::default(),
            }),
        });
        test.send(Event::DiffRead {
            session: id,
            root: proj,
            path: PathBuf::from("lib.rs"),
            result: Ok(Diff {
                hunks: vec![Hunk {
                    lines: vec![DiffLine {
                        kind: LineKind::Added,
                        old: None,
                        new: Some(1),
                        text: "fn f() {}".to_owned(),
                    }],
                }],
                cut: false,
            }),
        });
        test.send(Event::Action(Action::SetLineComment(
            0,
            "Keep this note.".to_owned(),
        )));
        assert_eq!(test.app.line_comments.len(), 1);
        test.send(Event::Action(Action::SendLineComments));
        assert_eq!(test.app.line_comments.len(), 1);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn send_keeps_draft_while_ask_is_pending() {
        let mut test = TestApp::start(None, Settings::default());
        let proj = PathBuf::from("/tmp/proj-ask-guard");
        test.app.begin(Vec::new(), Vec::new(), None, None, None);
        test.app.location = Location::Session(proj);
        test.app.asks.push_back(Ask {
            request: RequestId::from("req-1".to_owned()),
            tool_use: None,
            kind: AskKind::Tool {
                tool: wire::ToolName::from("Bash"),
                input: serde_json::json!({}),
                suggestions: Vec::new(),
                description: "Run ls".to_owned(),
            },
        });
        drop(test.drain_daemon());
        drop(test.drain_commands());

        test.send(Event::Action(Action::Send("queued thought".to_owned())));
        assert!(test.drain_daemon().is_empty());
        assert!(
            !test
                .drain_commands()
                .iter()
                .any(|cmd| matches!(cmd, Command::SetDraft(text) if text.is_empty()))
        );
        assert!(!test.app.chat.entries.is_empty());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn provider_usage_read_populates_tabs_from_any_page() {
        let mut test = TestApp::start(None, Settings::default());
        test.app.available_backends = vec![provider::Backend::Claude, provider::Backend::Agy];
        test.app.begin(Vec::new(), Vec::new(), None, None, None);
        drop(test.drain_daemon());

        test.send(Event::Action(Action::ShowUsage));
        let daemon_msgs = test.drain_daemon();
        assert!(daemon_msgs.iter().any(|msg| matches!(
            msg,
            ToDaemon::Read {
                read: Read::ProviderUsage {
                    backend: provider::Backend::Claude,
                    ..
                },
                ..
            }
        )));
        assert!(daemon_msgs.iter().any(|msg| matches!(
            msg,
            ToDaemon::Read {
                read: Read::ProviderUsage {
                    backend: provider::Backend::Agy,
                    ..
                },
                ..
            }
        )));

        let report = wire::UsageReport {
            rate_limits: wire::RateLimitReport {
                limits: vec![wire::PlanLimit {
                    kind: wire::LimitKind::Session,
                    percent: 25.0,
                    resets_at: None,
                    scope: None,
                    label: Some("5h".to_owned()),
                }],
            },
            ..wire::UsageReport::default()
        };
        test.send(Event::ProviderUsageRead {
            backend: provider::Backend::Agy,
            usage: ipc::ProviderUsage::reported(report.clone()),
        });
        assert_eq!(test.app.usage.agy.report, Some(report));
        assert!(test.app.usage.agy.error.is_none());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn back_and_enter_project_keep_earlier_sessions_visible() {
        let mut test = TestApp::start(None, Settings::default());
        test.app.begin(Vec::new(), Vec::new(), None, None, None);
        let proj = PathBuf::from("/tmp/proj-earlier-stable");
        let s1 = SessionId::new_random();
        let s2 = SessionId::new_random();
        let info = |id: SessionId, title: &str| sessions::SessionInfo {
            backend: provider::Backend::Opencode,
            id,
            title: title.to_owned(),
            modified: std::time::SystemTime::UNIX_EPOCH,
            worktree: None,
            last_said: None,
        };
        test.send(Event::OverviewListed(vec![ProjectOverview {
            path: proj.clone(),
            modified: std::time::SystemTime::UNIX_EPOCH,
            sessions: vec![info(s1.clone(), "From overview")],
        }]));
        test.send(Event::Action(Action::EnterProject(proj.clone())));
        assert_eq!(
            test.app
                .sessions
                .first()
                .map(|session| session.title.as_str()),
            Some("From overview")
        );

        test.send(Event::SessionsListed {
            cwd: proj.clone(),
            sessions: vec![info(s1.clone(), "From overview"), info(s2, "Second")],
        });
        assert_eq!(test.app.sessions.len(), 2);

        // Open one session, then click Back into the same project: Earlier must not clear.
        test.send(Event::Action(Action::OpenSession(s1)));
        assert!(matches!(&test.app.location, Location::Session(p) if p == &proj));
        test.send(Event::Action(Action::Back));
        assert!(matches!(&test.app.location, Location::Project(p) if p == &proj));
        assert_eq!(test.app.sessions.len(), 2);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn opencode_can_fork_watched_session_and_takes_system_commands() {
        let mut test = TestApp::start(None, Settings::default());
        test.app.begin(Vec::new(), Vec::new(), None, None, None);
        let proj = PathBuf::from("/tmp/proj-opencode-fork");
        let id = SessionId::new_random();
        let live = LiveSession {
            pid: 0,
            started: String::new(),
            id: id.clone(),
            cwd: proj.clone(),
            name: "oc-live".to_owned(),
            title: Some("Opencode chat".to_owned()),
            kind: sessions::LiveKind::Interactive,
            status: LiveStatus::Busy,
            worktree: None,
            agents: 0,
            backend: provider::Backend::Opencode,
        };
        test.send(Event::LiveListed(vec![live]));
        test.send(Event::Action(Action::EnterProject(proj)));
        test.send(Event::Action(Action::OpenSession(id)));
        assert!(test.app.watch.is_some());
        drop(test.drain_daemon());

        // Go to window with pid 0 is a no-op.
        test.send(Event::Action(Action::GoToWindow));
        assert!(test.drain_daemon().is_empty());

        // Forking watched opencode session emits Start with Fork.
        test.send(Event::Action(Action::ContinueHere));
        assert!(test.app.watch.is_none());
        assert!(test.drain_daemon().iter().any(|msg| matches!(
            msg,
            ToDaemon::Start {
                launch: provider::Launch {
                    backend: provider::Backend::Opencode,
                    conversation: provider::Conversation::Fork { .. },
                    ..
                },
                ..
            }
        )));

        let generation = test.app.generation;
        test.send(Event::Agent {
            generation,
            output: agent::Output::Event(agent::Event::System(wire::System::Commands {
                commands: vec![
                    wire::CommandInfo {
                        name: "review".to_owned(),
                        description: "Review".to_owned(),
                        argument_hint: String::new(),
                    },
                    wire::CommandInfo {
                        name: "init".to_owned(),
                        description: "Init".to_owned(),
                        argument_hint: String::new(),
                    },
                ],
            })),
        });
        let names: Vec<&str> = test
            .app
            .status
            .commands
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, ["init", "review"]);
    }
}
