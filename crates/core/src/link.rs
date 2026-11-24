//! A window's line to its daemon, whatever carries it: asks go out on a channel, and what the
//! daemon says comes back as the app's events.

use std::collections::HashMap;

use crate::agent::Output;
use crate::app::Event;
use crate::ipc::{self, Generation, ToDaemon};

use crate::ipc::{ActId, Assembler, FromDaemon, Hosted, LineSplitter, ProjectOverview, Stream};
use crate::sessions::{self, LiveSession};
use crate::settings::Settings;

/// What the window asks of the daemon goes this way, in order, to whatever writes it (a thread
/// of the window's, a task of its line's, the browser demo's script). Bounded: a daemon that
/// stopped taking messages loses the line rather than the window its memory.
pub type Link = async_channel::Sender<ToDaemon>;

/// Messages waiting to be written to the daemon.
pub const QUEUE: usize = 1024;

/// The line was lost: the daemon ended, or stopped taking messages.
#[derive(Debug)]
pub struct Lost;

/// Sends a message the window doesn't wait on.
pub fn send(link: &Link, message: ToDaemon) -> Result<(), Lost> {
    use async_channel::TrySendError;
    match link.try_send(message) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_) | TrySendError::Closed(_)) => Err(Lost),
    }
}

/// Sends a command for a session in pieces that each fit a message.
pub fn send_command(link: &Link, key: Generation, line: &str) -> Result<(), Lost> {
    let mut pieces = ipc::chunks(line).peekable();
    while let Some(text) = pieces.next() {
        let last = pieces.peek().is_none();
        send(
            link,
            ToDaemon::Command {
                key,
                text: text.to_owned(),
                last,
            },
        )?;
    }
    Ok(())
}

/// What the daemon tells a window as it connects.
#[derive(Debug)]
pub struct Greeting {
    /// The sessions the daemon runs: known before the window opens one, so that one it runs
    /// is attached to rather than started a second time.
    pub hosted: Vec<Hosted>,
    /// The sessions running now (in terminals, say): one of them resumed is watched instead.
    pub live: Vec<LiveSession>,
    /// The settings, or why they can't be read.
    pub settings: Result<Settings, String>,
    /// The daemon's user's home folder, if it has one.
    pub home: Option<std::path::PathBuf>,
    /// The programs on PATH when the window connected.
    pub backends: Vec<crate::provider::Backend>,
}

/// Why what the daemon said as a line opened isn't a greeting.
#[derive(Debug, thiserror::Error)]
pub enum NotGreeted {
    #[error(
        "the edualc daemon is another version; update the older of the two (a daemon started by \
         a window ends once its windows and sessions do)"
    )]
    Version,
    #[error("the edualc daemon said something out of turn as the window connected")]
    OutOfTurn,
}

/// Reads the daemon's greeting from its first two messages: its hello, then the sessions it
/// runs.
///
/// # Errors
///
/// Fails if the first is another version's hello or not one, or the second isn't the sessions.
pub fn greeted(hello: FromDaemon, hosted: FromDaemon) -> Result<Greeting, NotGreeted> {
    let FromDaemon::Hello {
        protocol,
        settings,
        live,
        home,
        backends,
    } = hello
    else {
        return Err(NotGreeted::Version);
    };
    if protocol != crate::ipc::PROTOCOL {
        return Err(NotGreeted::Version);
    }
    let FromDaemon::Hosted(hosted) = hosted else {
        return Err(NotGreeted::OutOfTurn);
    };
    let greeting = Greeting {
        hosted,
        live,
        settings,
        home,
        backends,
    };
    Ok(greeting)
}

/// Turns what the daemon says into the app's events (sans-io), joining the pieces a line or a
/// transcript comes in.
#[derive(Debug, Default)]
pub struct Inbox {
    lines: Assembler<(Generation, Stream)>,
    /// A transcript's text, made lines as it comes, kept for the answer it belongs to.
    transcripts: HashMap<ActId, (LineSplitter, Vec<String>)>,
    /// Pieces of an `Overview` answer before its final chunk arrives.
    overviews: HashMap<ActId, Vec<ProjectOverview>>,
}

impl Inbox {
    /// Takes a message and returns the app's event it makes, if any; a piece of a line makes
    /// none.
    pub fn take(&mut self, message: FromDaemon) -> Option<Event> {
        match message {
            FromDaemon::Started { key, pid, turns } => {
                Some(agent(key, Output::Started { pid, turns }))
            }
            FromDaemon::Failed { key, failure } => Some(agent(key, Output::Failed(failure))),
            FromDaemon::Output {
                key,
                stream,
                text,
                last,
            } => match self.lines.push((key, stream), &text, last) {
                Ok(Some(line)) => Some(agent(
                    key,
                    match stream {
                        Stream::Out => Output::of_line(line),
                        Stream::Err => Output::Log(line),
                    },
                )),
                Ok(None) => None,
                Err(error) => {
                    tracing::warn!(%error, "a line from session too long to keep");
                    None
                }
            },
            FromDaemon::Ended { key, code } => {
                self.lines.forget(&(key, Stream::Out));
                self.lines.forget(&(key, Stream::Err));
                Some(agent(key, Output::Ended(code)))
            }
            FromDaemon::Done { request, result } => Some(Event::Acted { request, result }),
            FromDaemon::Trusted { request, trust } => Some(Event::Trusted { request, trust }),
            FromDaemon::Hosted(sessions) => Some(Event::Hosted(sessions)),
            FromDaemon::ClaudeTrust { request, trusted } => {
                Some(Event::ClaudeTrustRead { request, trusted })
            }
            FromDaemon::TurnChanged { key, summary } => {
                Some(agent(key, Output::TurnChanged(summary)))
            }
            FromDaemon::ShuttingDown => Some(Event::DaemonShuttingDown),
            FromDaemon::ApiKeyStored { stored, .. } => Some(Event::ApiKeyStored(stored)),
            FromDaemon::Created { request, result } => {
                Some(Event::ProjectCreated { request, result })
            }
            FromDaemon::OverviewChunk { request, projects } => {
                self.overviews.entry(request).or_default().extend(projects);
                None
            }
            FromDaemon::Lines { request, text } => {
                let (splitter, lines) = self.transcripts.entry(request).or_default();
                lines.extend(splitter.push(text.as_bytes()));
                None
            }
            FromDaemon::Answer { request, answer } => {
                let (_unfinished, lines) = self.transcripts.remove(&request).unwrap_or_default();
                let answer = match answer {
                    ipc::Answer::Overview(tail) => {
                        let mut projects = self.overviews.remove(&request).unwrap_or_default();
                        projects.extend(tail);
                        ipc::Answer::Overview(projects)
                    }
                    other @ (ipc::Answer::Live(_)
                    | ipc::Answer::Projects(_)
                    | ipc::Answer::Sessions(_)
                    | ipc::Answer::Transcript { .. }
                    | ipc::Answer::Worktree(_)
                    | ipc::Answer::Folder(_)
                    | ipc::Answer::Changes(_)
                    | ipc::Answer::Diff(_)
                    | ipc::Answer::ProviderUsage(_)
                    | ipc::Answer::Saved(_)) => other,
                };
                Some(Event::Answered {
                    request,
                    answer,
                    lines: sessions::parse_lines(&lines),
                })
            }
            FromDaemon::Changed(watched) => Some(Event::Changed(watched)),
            FromDaemon::SettingsNotKept(why) => Some(Event::SettingsNotKept(why)),
            FromDaemon::Hello { .. } => None,
        }
    }
}

fn agent(generation: Generation, output: Output) -> Event {
    Event::Agent { generation, output }
}
