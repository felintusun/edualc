//! The ACP adapter (`edualcd --agy-adapter`): runs agy or opencode over the Agent Client
//! Protocol for the daemon, in place of `claude`, with the session's inbox for other sessions.

mod agents;
pub(crate) mod brain;
mod inbox;
mod listing;
mod outbox;
mod quotas;
mod tool;
pub(crate) mod translate;

pub(crate) use agents::Agent;
pub(crate) use listing::{ExternalSession, list_sessions};
pub(crate) use quotas::{QuotaError, agy_quotas};

use std::collections::HashMap;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::PathBuf;

use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::UnixListener;
use tokio::net::unix::OwnedWriteHalf;
use tokio::process::ChildStdin;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::mpsc;

use crate::agent;
use crate::external::agy;
use crate::external::wire::{BackgroundTask, System};
use crate::ids::{RequestId, SessionId};
use crate::sessions::{ClaudeHome, MessagingName};
use crate::settings::{self, PeerMessages};
use listing::resolve_conversation;
use outbox::{Job, Outcome, Places, SendArguments};
use quotas::quotas;
use translate::{CallId, Effect, Start, ToolServer, Translator};

/// The first argument that makes this program an ACP session's adapter.
pub const ADAPTER_FLAG: &str = "--agy-adapter";
/// The first argument that makes it the messaging tool an agent starts.
pub const TOOL_FLAG: &str = "--agy-tool";
/// What a message line from a peer or the tool may hold, at most.
const QUEUE: usize = 256;
/// How long the agent gets to end once its input closed, before it is killed.
const GRACE: std::time::Duration = std::time::Duration::from_secs(3);
/// The adapter's command line, as `spawn` writes it.
#[derive(Debug, clap::Parser)]
#[command(name = "edualcd --agy-adapter", disable_help_flag = true)]
struct Args {
    /// The agent this adapter serves.
    #[arg(long, value_enum, default_value_t = Agent::Agy)]
    agent: Agent,
    #[arg(long)]
    cwd: PathBuf,
    /// A new session, under this id (or the new id of a forked session).
    #[arg(long = "session-id", value_parser = session_id, conflicts_with = "resume")]
    session_id: Option<SessionId>,
    /// A session to continue.
    #[arg(long, value_parser = session_id, conflicts_with = "fork")]
    resume: Option<SessionId>,
    /// A session to fork into `--session-id`.
    #[arg(long, value_parser = session_id)]
    fork: Option<SessionId>,
    #[arg(long)]
    model: Option<String>,
}

fn session_id(id: &str) -> Result<SessionId, String> {
    SessionId::try_from(id.to_owned()).map_err(|error| error.to_string())
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("{0}")]
    Arguments(#[from] clap::Error),
    #[error("the adapter needs --session-id or --resume")]
    NoSession,
    #[error("{0} has no session {1}")]
    NoConversation(Agent, SessionId),
    #[error("continuing {0} sessions isn't supported yet")]
    ResumeNotSupported(Agent),
    #[error("forking {0} sessions isn't supported yet")]
    ForkNotSupported(Agent),
    #[error("no runtime folder (XDG_RUNTIME_DIR)")]
    NoRuntime,
    #[error(
        "no folder of Claude Code's to publish the session's record in ($CLAUDE_CONFIG_DIR or a home folder)"
    )]
    NoClaudeHome,
    #[error("couldn't make the session's folder: {0}")]
    Folder(#[source] std::io::Error),
    #[error("{} isn't on PATH; {}", .0, .0.install_hint())]
    NotOnPath(Agent),
    #[error("couldn't start {0}: {1}")]
    Start(Agent, #[source] std::io::Error),
    #[error("couldn't find this program: {0}")]
    Program(#[source] std::io::Error),
    #[error(transparent)]
    Inbox(#[from] inbox::Error),
    #[error("couldn't run: {0}")]
    Runtime(#[source] std::io::Error),
}

/// Runs the adapter until the window's side closes or the agent ends.
pub fn run_adapter() -> std::process::ExitCode {
    match adapter() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            // The window shows what the adapter says on stderr.
            eprintln!("edualc: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Runs the messaging tool the agent starts (see [`tool`]).
pub fn run_tool() -> std::process::ExitCode {
    tool::run()
}

fn adapter() -> Result<(), Error> {
    let args = <Args as clap::Parser>::try_parse_from(std::env::args_os().skip(1))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(Error::Runtime)?;
    runtime.block_on(adapt(args))
}

/// The session's folder in the runtime folder, for the messaging tool's socket, removed when
/// dropped.
#[derive(Debug)]
struct Own {
    folder: PathBuf,
}

impl Own {
    fn make(agent: Agent) -> Result<Self, Error> {
        let runtime = dirs::runtime_dir().ok_or(Error::NoRuntime)?;
        let folder = runtime.join(format!("edualc-{agent}-{}", std::process::id()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&folder)
            .map_err(Error::Folder)?;
        Ok(Self { folder })
    }

    fn tool(&self) -> PathBuf {
        self.folder.join("tool")
    }
}

impl Drop for Own {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.folder) {
            tracing::debug!(%error, "the session's folder stays");
        }
    }
}

/// Returns how you want other sessions' messages let in, as the settings say now. Settings
/// that can't be read ask about each new sender, so that nobody comes in unasked.
fn peer_messages() -> PeerMessages {
    match settings::load() {
        Ok(settings) => settings.peer_messages,
        Err(error) => {
            tracing::warn!(%error, "the settings can't be read: asking about each new sender");
            PeerMessages::AskEachSender
        }
    }
}

/// What the loop is told.
#[derive(Debug)]
enum Heard {
    Window(Option<String>),
    Agent(Option<String>),
    AgentLog(String),
    /// Another session's message, and whether you want each new sender asked about.
    Peer(String, PeerMessages),
    Tool(CallId, tool::Request, OwnedWriteHalf),
    Quotas(Option<RequestId>, Result<agy::Quotas, QuotaError>),
}

async fn adapt(args: Args) -> Result<(), Error> {
    let Args {
        agent,
        cwd,
        session_id,
        resume,
        fork,
        model,
    } = args;
    let (id, start) = match (session_id, resume, fork) {
        (Some(id), None, None) => (id, Start::New),
        (Some(id), None, Some(of)) => {
            if !agent.can_fork() {
                return Err(Error::ForkNotSupported(agent));
            }
            let conversation = resolve_conversation(agent, &of)
                .await
                .ok_or(Error::NoConversation(agent, of))?;
            (id, Start::Fork(conversation))
        }
        (None, Some(id), None) => {
            if !agent.loads_sessions() {
                return Err(Error::ResumeNotSupported(agent));
            }
            let conversation = resolve_conversation(agent, &id)
                .await
                .ok_or_else(|| Error::NoConversation(agent, id.clone()))?;
            let start = match agent {
                Agent::Agy => Start::Resume(conversation),
                Agent::Opencode => Start::ResumeReplay(conversation),
            };
            (id, start)
        }
        _ => return Err(Error::NoSession),
    };
    // The name the daemon knows the session by, as it names Claude Code's (see `spawn`).
    let name = MessagingName::of(&id, &cwd);
    let own = Own::make(agent)?;
    let tools = UnixListener::bind(own.tool()).map_err(Error::Folder)?;
    let claude = ClaudeHome::of_user().ok_or(Error::NoClaudeHome)?;
    let inbox = inbox::Inbox::open(&claude, &id, &cwd, &name)?;
    tracing::info!(inbox = %inbox.socket().display(), %name, "the session's inbox is open");
    let mut process = agent.start(&cwd)?;
    let program = std::env::current_exe().map_err(Error::Program)?;
    let mut translator = Translator::new(
        agent,
        id.clone(),
        cwd.clone(),
        start,
        model,
        ToolServer {
            program,
            socket: own.tool(),
        },
    );

    let (heard, mut hearing) = mpsc::channel::<Heard>(QUEUE);
    let mut agent_in = process.stdin.take();
    if let Some(out) = process.stdout.take() {
        let heard = heard.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(out).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if heard.send(Heard::Agent(Some(line))).await.is_err() {
                    return;
                }
            }
            if heard.send(Heard::Agent(None)).await.is_err() {
                tracing::debug!("the adapter's loop ended before the agent did");
            }
        });
    }
    if let Some(err) = process.stderr.take() {
        let heard = heard.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(err).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if heard.send(Heard::AgentLog(line)).await.is_err() {
                    return;
                }
            }
        });
    }
    {
        let heard = heard.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(tokio::io::stdin()).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if heard.send(Heard::Window(Some(line))).await.is_err() {
                    return;
                }
            }
            if heard.send(Heard::Window(None)).await.is_err() {
                tracing::debug!("the adapter's loop ended before the window's side closed");
            }
        });
    }
    let (posted, mut posts) = mpsc::channel::<String>(QUEUE);
    {
        let heard = heard.clone();
        tokio::spawn(async move {
            while let Some(post) = posts.recv().await {
                let peer_messages = tokio::task::spawn_blocking(peer_messages)
                    .await
                    .unwrap_or(PeerMessages::AskEachSender);
                if heard.send(Heard::Peer(post, peer_messages)).await.is_err() {
                    return;
                }
            }
        });
    }
    {
        let heard = heard.clone();
        tokio::spawn(async move {
            let mut next = 0;
            loop {
                let Ok((stream, _)) = tools.accept().await else {
                    continue;
                };
                next += 1;
                let call = CallId(next);
                let heard = heard.clone();
                tokio::spawn(async move {
                    let (read, write) = stream.into_split();
                    let mut line = String::new();
                    let read = BufReader::new(read).read_line(&mut line).await;
                    match read.ok().and_then(|_| serde_json::from_str(&line).ok()) {
                        Some(request) => {
                            if heard.send(Heard::Tool(call, request, write)).await.is_err() {
                                tracing::debug!("a tool call after the adapter's loop ended");
                            }
                        }
                        None => tracing::debug!("a tool call that isn't one"),
                    }
                });
            }
        });
    }
    let mut out = Out {
        window: tokio::io::stdout(),
        agent: agent_in.take(),
        program: agent,
        inbox,
        places: Places::of_user(&claude),
        cwd,
        calls: HashMap::new(),
        name,
        tasks: Vec::new(),
        heard,
    };
    let started = translator.start();
    out.carry_out(&mut translator, started).await;
    // Told to end, it ends as when its input closes, and takes its files away.
    let mut ending = Ending::listen().map_err(Error::Runtime)?;
    loop {
        let effects = tokio::select! {
            () = ending.told() => break,
            () = out.inbox.take(&posted) => continue,
            said = hearing.recv() => match said {
                Some(Heard::Window(Some(line))) => translator.window_said(&line),
                Some(Heard::Window(None)) => {
                    // The daemon closed the session's input: the agent's goes too, and it ends.
                    out.agent = None;
                    continue;
                }
                Some(Heard::Agent(Some(line))) => translator.agent_said(&line),
                Some(Heard::Agent(None)) | None => break,
                Some(Heard::AgentLog(line)) => {
                    eprintln!("{agent}: {line}");
                    continue;
                }
                Some(Heard::Peer(post, peer_messages)) => translator.peer(post, peer_messages),
                Some(Heard::Tool(call, request, write)) => {
                    out.calls.insert(call, write);
                    match request {
                        tool::Request::Send { to, message } => {
                            translator.tool_called(call, to, message)
                        }
                        tool::Request::List => translator.listing_asked(call),
                    }
                }
                Some(Heard::Quotas(request, told)) => translator.quotas_told(request, told),
            },
        };
        out.carry_out(&mut translator, effects).await;
    }
    // Its input closed, the agent ends; one that doesn't within a while is killed.
    drop(out);
    let ended = match tokio::time::timeout(GRACE, process.wait()).await {
        Ok(ended) => ended,
        Err(_late) => {
            if let Err(error) = process.start_kill() {
                tracing::warn!(%error, %agent, "couldn't kill the agent");
            }
            process.wait().await
        }
    };
    tracing::info!(?ended, %agent, "the agent ended");
    drop(own);
    Ok(())
}

/// The signals that end the adapter: the daemon's end, a terminal's.
#[derive(Debug)]
struct Ending {
    terminate: Signal,
    interrupt: Signal,
    hangup: Signal,
}

impl Ending {
    fn listen() -> std::io::Result<Self> {
        Ok(Self {
            terminate: signal(SignalKind::terminate())?,
            interrupt: signal(SignalKind::interrupt())?,
            hangup: signal(SignalKind::hangup())?,
        })
    }

    async fn told(&mut self) {
        tokio::select! {
            _ = self.terminate.recv() => {}
            _ = self.interrupt.recv() => {}
            _ = self.hangup.recv() => {}
        }
    }
}

/// Where the translator's effects go.
#[derive(Debug)]
struct Out {
    window: tokio::io::Stdout,
    agent: Option<ChildStdin>,
    /// Which agent it is.
    program: Agent,
    /// Where other sessions post to this one, under the session's id and name.
    inbox: inbox::Inbox,
    /// Where the running sessions' records and inboxes are, for the messaging tool.
    places: Places,
    /// The folder the session works in.
    cwd: PathBuf,
    /// The tool's calls waiting for their answer.
    calls: HashMap<CallId, OwnedWriteHalf>,
    /// The session's name for messaging, which the messages it sends come from.
    name: MessagingName,
    /// The subagents last told to the window.
    tasks: Vec<BackgroundTask>,
    /// Where what is asked in the background comes back to the loop.
    heard: mpsc::Sender<Heard>,
}

impl Out {
    /// Does what the translator said. What the tool is answered goes back to the translator,
    /// as the agent tells nothing of an MCP tool's result.
    async fn carry_out(&mut self, translator: &mut Translator, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::ToAgent(message) => match message.to_line() {
                    Ok(line) => {
                        if let Some(agent) = self.agent.as_mut()
                            && write_line(agent, &line).await.is_err()
                        {
                            tracing::debug!("the agent's input closed");
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "a line for the agent that can't be written: not sent");
                    }
                },
                Effect::ToWindow(event) => self.tell(&event).await,
                Effect::Named(id) => self.named(&id),
                Effect::Send { call, to, message } => {
                    let told = self.carried(Job::Send(SendArguments { to, message })).await;
                    translator.tool_told(call, &told);
                    self.answer(call, told).await;
                }
                Effect::List(call) => {
                    let told = self.carried(Job::List).await;
                    translator.tool_told(call, &told);
                    self.answer(call, told).await;
                }
                Effect::Subagents { conversation } => self.subagents(conversation).await,
                Effect::Quotas { request } => self.ask_quotas(request),
            }
        }
    }

    /// Asks the agent for its plan's quotas in the background; the answer comes back to the
    /// loop.
    fn ask_quotas(&self, request: Option<RequestId>) {
        let agent = self.program;
        let cwd = self.cwd.clone();
        let heard = self.heard.clone();
        tokio::spawn(async move {
            let told = quotas(agent, cwd).await;
            if heard.send(Heard::Quotas(request, told)).await.is_err() {
                tracing::debug!("the plan's quotas came after the adapter's loop ended");
            }
        });
    }

    /// Sends an event to the window, through the daemon.
    async fn tell(&mut self, event: &agent::Event) {
        match serde_json::to_string(event) {
            Ok(line) => {
                if write_line(&mut self.window, &line).await.is_err() {
                    tracing::debug!("the window's side closed");
                }
            }
            Err(error) => {
                tracing::warn!(%error, "an event for the window that can't be written: not sent");
            }
        }
    }

    /// Tells the window the subagents the agent keeps for `conversation`, if they changed.
    async fn subagents(&mut self, conversation: String) {
        let Ok(conversation) = SessionId::try_from(conversation) else {
            tracing::debug!("a conversation id that isn't agy's: no subagents read");
            return;
        };
        let tasks = brain::tasks(&conversation);
        if tasks != self.tasks {
            self.tasks.clone_from(&tasks);
            self.tell(&agent::Event::System(System::BackgroundTasksChanged {
                tasks,
            }))
            .await;
        }
    }

    /// Carries out a call of edualc's tool (see [`outbox::carry_out`]).
    async fn carried(&self, job: Job) -> tool::Answer {
        let sender = self.name.clone();
        let places = self.places.clone();
        let carried =
            tokio::task::spawn_blocking(move || outbox::carry_out(&places, &sender, &job))
                .await
                .unwrap_or(Outcome::Refused(outbox::Refusal::Failed));
        let (text, error) = carried.told();
        tool::Answer { text, error }
    }

    async fn answer(&mut self, call: CallId, answer: tool::Answer) {
        let Some(mut write) = self.calls.remove(&call) else {
            return;
        };
        let line = serde_json::to_string(&answer).unwrap_or_default();
        let written = write.write_all(format!("{line}\n").as_bytes()).await;
        if written.is_err() {
            tracing::debug!("the tool's call went away");
        }
    }

    /// Renames the session to the agent's id: its record says so, with the name that id gives
    /// it, for other sessions and the running list.
    fn named(&mut self, id: &SessionId) {
        self.name = MessagingName::of(id, &self.cwd);
        if let Err(error) = self.inbox.rename(id, &self.name) {
            tracing::warn!(%error, "couldn't publish the session under the agent's id");
        }
    }
}

async fn write_line(
    to: &mut (impl tokio::io::AsyncWrite + Unpin),
    line: &impl std::fmt::Display,
) -> std::io::Result<()> {
    to.write_all(format!("{line}\n").as_bytes()).await?;
    to.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_agents_load_their_sessions_and_opencode_forks() {
        assert!(Agent::Opencode.loads_sessions());
        assert!(Agent::Opencode.can_fork());
        assert!(Agent::Agy.loads_sessions());
        assert!(!Agent::Agy.can_fork());
    }
}
