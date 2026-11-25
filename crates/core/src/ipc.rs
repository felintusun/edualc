//! The window and its daemon over a Unix socket in a folder of the user's alone, each peer
//! checked as it connects; messages are COBS-framed postcard of at most [`MAX_MESSAGE`].

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use web_time::SystemTime;

mod frame;
pub use frame::{Deframer, Error, MAX_FRAME, deframe, frame};
#[cfg(not(target_arch = "wasm32"))]
mod socket;
#[cfg(not(target_arch = "wasm32"))]
pub use socket::{AsyncChannel, Channel, Folder, Listener, Writing, recv, send};

pub mod pairing;
#[cfg(not(target_arch = "wasm32"))]
pub mod servers;
#[cfg(not(target_arch = "wasm32"))]
pub mod tls;
mod token;
#[cfg(not(target_arch = "wasm32"))]
pub mod ws;
pub use token::{Auth, Token};

use crate::decode::{Picture, Refusal};
use crate::external::wire::TaskId;
use crate::ids::SessionId;
use crate::provider::Launch;
use crate::sessions::{LiveSession, ProjectInfo, Saved, SessionInfo, Worktree};
use crate::settings::Settings;

/// The largest message either side sends; a larger one means a broken or hostile peer. Text is
/// cut to fit (see [`chunks`]), but an answer to a read is one message, and a thousand sessions
/// fill one well past 64 KiB.
pub const MAX_MESSAGE: usize = 1024 * 1024;
/// The most text one message carries, leaving room for the rest of the message. A longer line
/// goes in several.
pub const MAX_CHUNK: usize = MAX_MESSAGE - 1024;
/// The longest line kept of a process's output or a window's input.
pub const MAX_LINE: usize = 32 * 1024 * 1024;

/// Splits a byte stream, such as a process's output, into lines as it is read. A line longer
/// than [`MAX_LINE`] is replaced by a note that it was dropped.
#[derive(Debug, Default)]
pub struct LineSplitter {
    buffer: Vec<u8>,
    /// Set while passing over a line too long to keep, up to its end.
    skipping: bool,
}

impl LineSplitter {
    /// Returns the whole lines that `bytes` complete.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        for piece in bytes.split_inclusive(|&byte| byte == b'\n') {
            let ends = piece.last() == Some(&b'\n');
            if !self.skipping {
                self.buffer.extend_from_slice(piece);
                if self.buffer.len() > MAX_LINE {
                    self.buffer.clear();
                    self.skipping = true;
                }
            }
            if ends {
                if self.skipping {
                    self.skipping = false;
                    lines.push(format!("[a line over {MAX_LINE} bytes, left out]"));
                } else {
                    let line = std::mem::take(&mut self.buffer);
                    lines.push(
                        String::from_utf8_lossy(&line)
                            .trim_end_matches('\n')
                            .to_owned(),
                    );
                }
            }
        }
        lines
    }

    /// Returns what is left at the end of the stream, as a last line.
    pub fn finish(&mut self) -> Option<String> {
        let rest = std::mem::take(&mut self.buffer);
        (!rest.is_empty()).then(|| String::from_utf8_lossy(&rest).into_owned())
    }
}

/// Joins lines again from the pieces they were sent in, up to [`MAX_LINE`].
#[derive(Debug)]
pub struct Assembler<K: Eq + std::hash::Hash> {
    partial: std::collections::HashMap<K, String>,
}

impl<K: Eq + std::hash::Hash> Default for Assembler<K> {
    fn default() -> Self {
        Self {
            partial: std::collections::HashMap::new(),
        }
    }
}

impl<K: Eq + std::hash::Hash> Assembler<K> {
    /// Adds a piece, and returns the whole line once its last piece came.
    ///
    /// # Errors
    ///
    /// [`Error::Oversized`] for a line longer than [`MAX_LINE`]; its pieces are dropped.
    pub fn push(&mut self, key: K, piece: &str, last: bool) -> Result<Option<String>, Error> {
        let line = self.partial.entry(key).or_default();
        if line.len() + piece.len() > MAX_LINE {
            line.clear();
            return Err(Error::Oversized);
        }
        line.push_str(piece);
        Ok(last.then(|| std::mem::take(line)))
    }

    /// Forgets the pieces of a session that ended.
    pub fn forget(&mut self, key: &K) {
        self.partial.remove(key);
    }
}

/// Cuts text into pieces that each fit a message, on character boundaries.
pub fn chunks(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text;
    let mut first = true;
    std::iter::from_fn(move || {
        if rest.is_empty() && !std::mem::take(&mut first) {
            return None;
        }
        first = false;
        let mut end = rest.len().min(MAX_CHUNK);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (piece, after) = rest.split_at(end);
        rest = after;
        Some(piece)
    })
}

/// Bumped with any change to the messages below, or to their framing: postcard isn't
/// self-describing, so both ends must agree on every type.
pub const PROTOCOL: u32 = 40;

/// A window's key for a process it shows, new for each one, so that what an earlier process
/// still says is dropped.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct Generation(u64);

impl Generation {
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// The daemon's id for a session it runs, which a window attaches to.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct HostedId(u64);

impl HostedId {
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl std::fmt::Display for HostedId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for HostedId {
    type Err = std::num::ParseIntError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let id = text.parse::<u64>()?;
        Ok(Self(id))
    }
}

/// A window's id for an [`Act`] it asked for, which the daemon's answer carries.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct ActId(u64);

impl ActId {
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// What the window asks of the daemon. Every field is checked where it is taken.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum ToDaemon {
    /// The first message, sent once.
    Hello {
        protocol: u32,
    },
    /// Starts a session's program, known by `key` from then on.
    Start {
        key: Generation,
        launch: Launch,
    },
    /// A piece of a serialized [`crate::agent::Command`] for the session; the last piece ends it.
    Command {
        key: Generation,
        text: String,
        last: bool,
    },
    /// Ends the session's process: its input closes, and it is killed if it doesn't end.
    End {
        key: Generation,
    },
    /// Shows a session the daemon runs in this window, known by `key` from then on. What it
    /// asked and wasn't answered comes again, then what it says.
    Attach {
        key: Generation,
        hosted: HostedId,
    },
    /// Stops showing the session, which runs on, as when the window closes.
    Detach {
        key: Generation,
    },
    /// Tells the program's id for the session, as its process told the window, for the list.
    Identify {
        key: Generation,
        session: SessionId,
    },
    /// Asks the daemon to act on its machine; answered with the same `request`.
    Act {
        request: ActId,
        act: Act,
    },
    /// Ends every session the daemon runs, in every window, and then the daemon, so that the
    /// next window starts the edualc installed now.
    ShutDown,
    /// Reads the programs' or edualc's files, which a page can't read itself; answered by
    /// `Answer` with the same `request`, after a transcript's `Lines`.
    Read {
        request: ActId,
        read: Read,
    },
    /// Makes one change to the config (`~/.config/edualc/config.toml`) or state
    /// (`~/.local/state/edualc/state.json`) as they are on disk now; answered by
    /// `SettingsNotKept` only if it couldn't be kept.
    ChangeSettings(crate::settings::Change),
    /// Tells the window, as `Changed`, when what `target` names changes, until `Unwatch`.
    Watch {
        watched: Watched,
        target: Target,
    },
    Unwatch(Watched),
}

/// A watch, named by the window that asked for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Watched(uuid::Uuid);

impl Watched {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl Default for Watched {
    fn default() -> Self {
        Self::new()
    }
}

/// What a window watches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Target {
    /// The records of the sessions running (`~/.claude/sessions`).
    Records,
    /// A transcript being written, or the folders it will be in while it isn't there yet.
    Transcript(Place),
}

/// Where a transcript is, as a window knows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Place {
    /// Found already.
    Path(PathBuf),
    /// A session's, wherever it is, as a worktree the session entered may have moved it.
    Session { cwd: PathBuf, id: SessionId },
    /// A subagent's, beside its session's.
    Subagent {
        cwd: PathBuf,
        id: SessionId,
        task: TaskId,
    },
}

/// What a window reads through the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Read {
    /// The sessions running now: `Answer::Live`.
    Live,
    /// The folders Claude Code worked in: `Answer::Projects`.
    Projects,
    /// Every project with its recent sessions, for the start page: `Answer::Overview`.
    Overview,
    /// A folder's sessions, newest first: `Answer::Sessions`. Those `known` are summed up
    /// again only if their transcript changed.
    Sessions {
        cwd: PathBuf,
        limit: usize,
        known: Vec<SessionInfo>,
    },
    /// A transcript's whole lines from `from` on: `Lines`, then `Answer::Transcript`.
    Transcript { place: Place, from: u64 },
    /// The worktree a session's transcript says it entered: `Answer::Worktree`.
    WorktreeOf { cwd: PathBuf, id: SessionId },
    /// `cwd` as a git worktree, if it is a linked one: `Answer::Worktree`.
    LinkedWorktree { cwd: PathBuf },
    /// The folder a typed path names, resolved, with a leading `~` as the home folder:
    /// `Answer::Folder`.
    TypedFolder(String),
    /// The folder at a path, such as the picker's or a worktree's, resolved: `Answer::Folder`.
    Folder(PathBuf),
    /// What a session changed in its folder, in `scope`: `Answer::Changes`.
    Changes {
        session: SessionId,
        scope: crate::changes::Scope,
    },
    /// The diff of a file a session changed, in `scope`: `Answer::Diff`.
    Diff {
        session: SessionId,
        scope: crate::changes::Scope,
        root: PathBuf,
        path: PathBuf,
    },
    /// A program's usage and plan limits: `Answer::ProviderUsage`.
    ProviderUsage {
        backend: crate::provider::Backend,
        cwd: Option<PathBuf>,
    },
    /// Where a session's transcript is kept, and the folder it was started in: `Answer::Saved`.
    /// The Android app asks this to open a session before it knows the folder.
    Saved(SessionId),
}

/// Why a session's changes weren't read: the daemon's words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{0}")]
pub struct Unkept(pub String);

/// A project with its recent sessions, for the start page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectOverview {
    pub path: PathBuf,
    pub modified: Option<SystemTime>,
    pub sessions: Vec<SessionInfo>,
}

/// A program's usage, as the usage panel asked for it. `report` holds at most one report; a
/// `Vec` keeps `Answer` and `FromDaemon` small without a `Box`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum ProviderUsage {
    Available {
        report: Vec<crate::external::wire::UsageReport>,
        note: Option<String>,
    },
    Unavailable,
}

impl ProviderUsage {
    pub fn reported(report: crate::external::wire::UsageReport) -> Self {
        Self::Available {
            report: vec![report],
            note: None,
        }
    }

    pub fn noted(note: impl Into<String>) -> Self {
        Self::Available {
            report: Vec::new(),
            note: Some(note.into()),
        }
    }
}

/// What the daemon read for a window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Answer {
    Live(Vec<LiveSession>),
    Projects(Vec<ProjectInfo>),
    Overview(Vec<ProjectOverview>),
    Sessions(Vec<SessionInfo>),
    /// Ends a transcript's lines: where it was found, and the offset after its last whole
    /// line or why it couldn't be read.
    Transcript {
        path: PathBuf,
        read: Result<u64, Unread>,
    },
    Worktree(Option<Worktree>),
    Folder(Result<PathBuf, String>),
    Changes(Result<crate::changes::ChangeSet, Unkept>),
    Diff(Result<crate::changes::Diff, Unkept>),
    ProviderUsage(ProviderUsage),
    Saved(Saved),
}

/// What the daemon tells the window.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum FromDaemon {
    /// The first message, with the settings and the sessions running now. `Hosted` follows
    /// before anything else.
    Hello {
        protocol: u32,
        settings: Result<Settings, String>,
        live: Vec<LiveSession>,
        /// The programs on PATH.
        backends: Vec<crate::provider::Backend>,
        /// The daemon's user's home folder; `None` where `$HOME` isn't set.
        home: Option<PathBuf>,
    },
    /// The process runs, started for the window or already running as it attaches.
    Started {
        key: Generation,
        pid: u32,
        /// The turns under way: prompts written and not yet ended by a result.
        turns: usize,
    },
    Failed {
        key: Generation,
        failure: StartFailure,
    },
    /// A piece of a line the process wrote; the last piece ends the line.
    Output {
        key: Generation,
        stream: Stream,
        text: String,
        last: bool,
    },
    /// The process ended, with its exit code if it exited on its own.
    Ended { key: Generation, code: Option<i32> },
    Done {
        request: ActId,
        result: Result<(), ActFailure>,
    },
    /// Whether the terminal's trust dialog was accepted for the folder asked about.
    Trusted { request: ActId, trust: FolderTrust },
    /// Whether Claude Code counts the folder as trusted, or why that wasn't read; answers
    /// `Act::CheckClaudeTrust`.
    ClaudeTrust {
        request: ActId,
        trusted: Result<bool, ActFailure>,
    },
    /// The sessions the daemon runs, sent again whenever they change.
    Hosted(Vec<Hosted>),
    /// A window asked the daemon to shut down; its sessions end, and then the daemon.
    ShuttingDown,
    /// What was read; answers `Read`.
    Answer { request: ActId, answer: Answer },
    /// Part of a large `Overview` answer, sent before `Answer::Overview`.
    OverviewChunk {
        request: ActId,
        projects: Vec<ProjectOverview>,
    },
    /// A piece of a transcript's text, for `Read::Transcript`. Pieces fit a message and don't
    /// end at lines; the `Answer` follows the last.
    Lines { request: ActId, text: String },
    /// What a watch names changed.
    Changed(Watched),
    /// Why a `ChangeSettings` couldn't be kept, for you to put right.
    SettingsNotKept(String),
    /// Whether the keyring holds an Anthropic API key, or why that couldn't be told.
    ApiKeyStored {
        request: ActId,
        stored: Result<bool, ActFailure>,
    },
    /// The new project's folder, or why it wasn't made; answers `Act::CreateProject`.
    Created {
        request: ActId,
        result: Result<PathBuf, CreateProjectError>,
    },
    /// What the turn that just ended changed in the folder of the session shown under `key`.
    TurnChanged {
        key: Generation,
        summary: crate::changes::Summary,
    },
}

/// The clipboard's image, decoded, or why there is none.
pub type Pasted = Result<Picture, PasteFailure>;

/// Whether a folder may run Claude Code with its own configuration, as the daemon tells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum FolderTrust {
    /// The terminal's trust dialog was accepted for it.
    Accepted,
    /// Not trusted yet, with what its configuration would have Claude Code run there.
    Untrusted(Vec<Runs>),
}

/// Something a folder's own configuration (`.claude/settings.json`, `settings.local.json`,
/// `.mcp.json`) would have Claude Code run or set there, read before you trust the folder. The
/// text is cut short and has no control characters (see [`crate::trust`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Runs {
    /// What a hook does on one of Claude Code's events, for the tools `matcher` names, or all
    /// if it is empty.
    Hook {
        event: String,
        matcher: String,
        action: HookAction,
    },
    /// An MCP server, with the command line it starts or the URL it reaches.
    McpServer { name: String, runs: String },
    /// A command whose output is used as the API key.
    ApiKeyHelper(String),
    /// A command run to draw the status line.
    StatusLine(String),
    /// A plugin turned on, which brings hooks and MCP servers of its own.
    Plugin(String),
    /// Environment variables set, by name; their values aren't shown.
    Environment(Vec<String>),
    /// Permission rules that let tools run without asking.
    Allows(Vec<String>),
    /// Folders beyond this one that the session may read and write.
    Directories(Vec<String>),
    /// The permission mode sessions start in.
    DefaultMode(String),
    /// Plugin marketplaces added, by name.
    Marketplaces(Vec<String>),
    /// Settings the window doesn't read, by key; only that they're set is shown.
    AlsoSets(Vec<String>),
    /// How many more entries there were than are listed.
    More(usize),
    /// A file that couldn't be read, so what it would run is unknown.
    Unreadable { file: String, reason: String },
}

/// What a hook does when its event comes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum HookAction {
    /// Runs a command line.
    Command(String),
    /// Sends the event to a URL.
    Url(String),
    /// Calls an MCP server's tool.
    McpTool { server: String, tool: String },
    /// Asks a model or an agent with a prompt, running no command of its own.
    Prompt,
}

/// Why the clipboard gave no image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum PasteFailure {
    /// The clipboard holds something else, such as text; not worth telling.
    #[error("the clipboard holds no PNG, JPEG, GIF or WebP image")]
    NoImage,
    #[error("couldn't read the clipboard: {0}")]
    Clipboard(String),
    #[error("the image wasn't taken: {0}")]
    Refused(Refusal),
    #[error("{0}")]
    Decoder(String),
}

/// What a session the daemon runs is doing now.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum HostedState {
    #[default]
    Idle,
    Busy,
    Waiting {
        question: String,
        since: SystemTime,
    },
}

/// A session the daemon runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hosted {
    pub id: HostedId,
    /// The program's id for the session: chosen at the start (`--session-id`), or the one it
    /// continues, and updated when the program moves it to another. Its transcript may not be
    /// written yet.
    pub session: SessionId,
    pub cwd: PathBuf,
    pub backend: crate::provider::Backend,
    /// What it was started signed in with.
    pub sign_in: crate::provider::SignInWith,
    /// Set while a window shows it.
    pub attached: bool,
    pub state: HostedState,
}

/// Why a transcript wasn't read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(deny_unknown_fields)]
pub enum Unread {
    /// No file there yet; a session's first turn writes it.
    #[error("it isn't written yet")]
    NotWritten,
    /// The system's words.
    #[error("{0}")]
    Failed(String),
}

impl From<std::io::Error> for Unread {
    fn from(error: std::io::Error) -> Self {
        if error.kind() == std::io::ErrorKind::NotFound {
            Self::NotWritten
        } else {
            Self::Failed(error.to_string())
        }
    }
}

/// The name of a new project's folder: one folder, not a path, trimmed of spaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, derive_more::Display)]
#[serde(try_from = "String")]
pub struct ProjectName(String);

/// Why a name can't be a new project's folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NotAProjectName {
    #[error("Enter a folder name.")]
    Empty,
    #[error("Use a folder name without slashes.")]
    Slash,
    #[error("“.” and “..” name folders that are there already. Choose another name.")]
    Dots,
}

impl TryFrom<String> for ProjectName {
    type Error = NotAProjectName;

    fn try_from(typed: String) -> Result<Self, Self::Error> {
        let name = typed.trim();
        if name.is_empty() {
            Err(NotAProjectName::Empty)
        } else if name.contains('/') {
            Err(NotAProjectName::Slash)
        } else if name == "." || name == ".." {
            Err(NotAProjectName::Dots)
        } else {
            Ok(Self(name.to_owned()))
        }
    }
}

impl ProjectName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why the daemon didn't make a new project's folder, in the system's words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(deny_unknown_fields)]
pub enum CreateProjectError {
    #[error("{0} isn't an existing folder.")]
    NoParent(PathBuf),
    #[error("There is already a folder named {0}.")]
    AlreadyExists(PathBuf),
    #[error("Couldn't make {folder}: {reason}")]
    Create { folder: PathBuf, reason: String },
    #[error("The daemon failed making the folder.")]
    Crashed,
    #[error("The browser demo makes no folders.")]
    InBrowser,
}

/// Why a session's process didn't start, or its input was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum StartFailure {
    #[error("{0} isn't a folder to work in")]
    NoFolder(PathBuf),
    #[error("{0:?} isn't a model's name")]
    NoModel(String),
    #[error(
        "couldn't start claude: {0}. Is Claude Code installed and `claude` on PATH? CLAUDE_BIN can name it."
    )]
    Start(String),
    #[error("a session with that key runs already")]
    KeyInUse,
    #[error("a line too long for claude's input")]
    LineTooLong,
    #[error("that session isn't run here, or is shown in another window")]
    NotHosted,
    #[error("the daemon failed starting it")]
    Crashed,
    #[error("{0}")]
    Acp(String),
    #[error("{0}")]
    ApiKey(String),
}

/// Why the daemon didn't do what it was asked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(deny_unknown_fields)]
pub enum ActFailure {
    #[error("the daemon failed doing it")]
    Crashed,
    #[error("the keyring: {0}")]
    Keyring(String),
    #[error("Claude Code was saving its settings the whole while; try again in a moment")]
    ClaudeCodeBusy,
    /// Claude Code's settings couldn't be read or written, in the daemon's words.
    #[error("couldn't record the folder as trusted for Claude Code: {0}")]
    ClaudeCodeTrust(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Stream {
    Out,
    Err,
}

/// An Anthropic API key, on its way to the keyring or to the process that signs in with it.
/// It is cleared from memory when dropped, and `Debug` leaves it out.
#[derive(PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiKey(zeroize::Zeroizing<String>);

impl ApiKey {
    /// Reads a pasted key, trimmed of spaces and line breaks; `None` if nothing is left or
    /// spaces or control characters are inside it.
    pub fn parse(text: &str) -> Option<Self> {
        let key = text.trim();
        let fits = !key.is_empty()
            && key.len() <= 512
            && !key
                .chars()
                .any(|character| character.is_whitespace() || character.is_control());
        fits.then(|| Self(zeroize::Zeroizing::new(key.to_owned())))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ApiKey(…)")
    }
}

/// What the window asks the daemon to do on its machine.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Act {
    /// Asks whether the terminal's trust dialog was accepted for the folder or one it is in
    /// (see [`crate::trust`]); answered by [`FromDaemon::Trusted`].
    CheckTrust {
        folder: PathBuf,
    },
    /// Asks whether Claude Code applies the folder's own permission rules, which it does if
    /// `~/.claude.json` records the folder as trusted; answered by [`FromDaemon::ClaudeTrust`].
    CheckClaudeTrust {
        folder: PathBuf,
    },
    /// Records the folder as trusted for Claude Code too, in `~/.claude.json`, as its trust
    /// dialog would; answered by `Done`.
    TrustForClaudeCode {
        folder: PathBuf,
    },
    /// Keeps the key in the desktop's keyring, in place of one kept before.
    StoreApiKey(ApiKey),
    RemoveApiKey,
    /// Asks whether the keyring holds a key; answered by `ApiKeyStored`.
    CheckApiKey,
    /// Creates a new project folder inside `parent`; answered by `FromDaemon::Created`.
    CreateProject {
        parent: PathBuf,
        name: ProjectName,
    },
    /// Records that you looked at a session's changes, so "since you last looked" starts now.
    /// Not answered.
    ChangesLooked {
        session: SessionId,
    },
    /// Marks a file a session changed as reviewed with what it holds `now`, `None` if it was
    /// deleted. Not answered.
    MarkReviewed {
        session: SessionId,
        path: PathBuf,
        now: Option<crate::changes::ContentId>,
    },
    /// Takes the reviewed mark off a file a session changed. Not answered.
    UnmarkReviewed {
        session: SessionId,
        path: PathBuf,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_project_name_is_one_folder() {
        let name = |typed: &str| ProjectName::try_from(typed.to_owned());
        assert_eq!(
            name("  my app ").map(|name| name.to_string()),
            Ok("my app".to_owned())
        );
        assert_eq!(name(" \t"), Err(NotAProjectName::Empty));
        assert_eq!(name("a/b"), Err(NotAProjectName::Slash));
        assert_eq!(name("/"), Err(NotAProjectName::Slash));
        assert_eq!(name(".."), Err(NotAProjectName::Dots));
        assert_eq!(name("."), Err(NotAProjectName::Dots));
        assert_eq!(name("..."), Ok(ProjectName("...".to_owned())));
        let arrived: Result<ProjectName, _> = serde_json::from_str("\"..\"");
        assert!(arrived.is_err());
    }

    #[test]
    fn an_api_key_is_taken_clean_and_never_written_out() {
        let key = ApiKey::parse("  sk-ant-api03-abc_DEF-123\n").expect("a key");
        assert_eq!(key.as_str(), "sk-ant-api03-abc_DEF-123");
        assert!(!format!("{key:?}").contains("abc_DEF"));
        assert!(!format!("{:?}", Act::StoreApiKey(key)).contains("abc_DEF"));
        assert!(ApiKey::parse("sk-ant api03").is_none());
        assert!(ApiKey::parse(" \n").is_none());
    }

    #[test]
    fn long_text_goes_in_pieces_that_fit() {
        let text = "é".repeat(MAX_CHUNK);
        let pieces: Vec<&str> = chunks(&text).collect();
        assert!(pieces.iter().all(|piece| piece.len() <= MAX_CHUNK));
        assert_eq!(pieces.concat(), text);
        assert_eq!(chunks("").collect::<Vec<_>>(), [""]);
    }

    #[test]
    fn a_usage_report_reaches_the_window_as_claude_code_gave_it() {
        let report: crate::external::wire::UsageReport = serde_json::from_str(
            r#"{
                "rate_limits": { "limits": [
                    { "kind": "session", "percent": 12.0, "resets_at": "2026-11-25T03:00:00Z", "scope": null },
                    { "kind": "weekly_scoped", "percent": 40.0, "resets_at": null,
                      "scope": { "model": { "display_name": "Opus" } }, "label": "Opus, week" }
                ] },
                "behaviors": { "day": { "request_count": 3, "session_count": 1 }, "week": null }
            }"#,
        )
        .expect("Claude Code's answer");
        let sent = FromDaemon::Answer {
            request: ActId(7),
            answer: Answer::ProviderUsage(ProviderUsage::reported(report)),
        };
        let received: FromDaemon = deframe(&frame(&sent).expect("framed")).expect("deframed");
        assert_eq!(received, sent);
    }
}
