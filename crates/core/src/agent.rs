//! A running agent session's events and commands, the same for every program.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};

use crate::call::{AcpCall, AcpOption, OptionId};
use crate::decode::Picture;
use crate::external::wire::{
    ApiMessage, AssistantError, ControlRequest, ControlResponse, ImageBlock, IncomingRequest,
    PermissionResult, RateLimits, Reply, StreamEvent, System, TurnResult, UserLine,
};
use crate::ids::{RequestId, SessionId, ToolUseId};
use crate::ipc::{Generation, HostedId, StartFailure, ToDaemon};
use crate::link::{self, Link};
use crate::provider::Launch;

/// What a frontend sends to the running session via the daemon.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub enum Command {
    Prompt {
        shell: Vec<String>,
        images: Vec<ImageBlock>,
        text: String,
    },
    Shell {
        command: String,
    },
    ControlRequest {
        request_id: RequestId,
        request: ControlRequest,
    },
    ControlResponse {
        response: Reply,
    },
    /// The user's choice among those an ACP agent offered (see [`Event::PermissionAsked`]).
    Choose {
        request_id: RequestId,
        option: OptionId,
    },
}

impl Command {
    /// Serializes a command into JSON for chunked transport over IPC or pipes.
    pub fn encode(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// The request id answered if this command replies to a session prompt/permission request.
    pub fn answered_request_id(&self) -> Option<&RequestId> {
        match self {
            Self::ControlResponse {
                response: Reply::Success { request_id, .. } | Reply::Error { request_id, .. },
            }
            | Self::Choose { request_id, .. } => Some(request_id),
            Self::Prompt { .. } | Self::Shell { .. } | Self::ControlRequest { .. } => None,
        }
    }
}

/// A typed, provider-neutral event emitted by any running session driver (`ClaudeDriver`,
/// `AcpDriver`, or the demo driver).
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub enum Event {
    Stream {
        event: StreamEvent,
        parent_tool_use_id: Option<ToolUseId>,
    },
    Assistant {
        message: ApiMessage,
        parent_tool_use_id: Option<ToolUseId>,
        error: Option<AssistantError>,
    },
    /// An ACP agent's tool call, new or changed, as the agent gives it, its status and output
    /// included.
    ToolCall {
        id: ToolUseId,
        call: AcpCall,
        parent_tool_use_id: Option<ToolUseId>,
    },
    /// An ACP agent asks whether its call may go ahead, offering these choices; the window
    /// answers with [`Command::Choose`].
    PermissionAsked {
        request_id: RequestId,
        tool_use_id: ToolUseId,
        call: AcpCall,
        options: Vec<AcpOption>,
    },
    User(UserLine),
    System(System),
    Result(TurnResult),
    RateLimits(RateLimits),
    ControlRequest {
        request_id: RequestId,
        request: IncomingRequest,
    },
    ControlAnswered(ControlResponse),
    RequestCancelled {
        request_id: RequestId,
    },
}

/// What the session process says, or the daemon about it.
#[derive(Debug)]
pub enum Output {
    /// The daemon started it, or it runs already as the window attaches.
    Started {
        pid: u32,
        /// Turns under way, as the daemon counted them.
        turns: usize,
    },
    /// The daemon couldn't start it, or refused its input.
    Failed(StartFailure),
    Event(Event),
    /// A turn's result or a permission request came through broken and couldn't be read.
    Unreadable {
        lost: Lost,
        error: String,
    },
    /// A diagnostic line on stderr, or output that didn't parse as an event.
    Log(String),
    /// A turn ended, having changed these files in its folder.
    TurnChanged(crate::changes::Summary),
    /// The process ended, with its exit code if it exited on its own.
    Ended(Option<i32>),
}

impl Output {
    /// Decodes a serialized [`Event`] emitted by `edualcd` or the demo driver.
    pub fn of_line(line: String) -> Self {
        match serde_json::from_str::<Event>(&line) {
            Ok(event) => Self::Event(event),
            Err(error) => {
                tracing::debug!(%error, "a session line that isn't an event");
                Self::Log(line)
            }
        }
    }
}

/// What an unreadable line from the session represented, when it would leave a turn or
/// permission prompt waiting forever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lost {
    Result,
    Request(RequestId),
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("couldn't encode a command for the session: {0}")]
    Encode(#[source] serde_json::Error),
    #[error("lost the edualc daemon")]
    Lost,
}

/// The window's handle on a running session in the daemon. Dropped, it ends the session unless
/// [`Agent::detach`] let it run on.
#[derive(Debug)]
pub struct Agent {
    key: Generation,
    link: Link,
    next_request: u64,
    pid: Option<u32>,
    leaving: Leaving,
    use_: Use,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Use {
    Opened,
    Used,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Leaving {
    End,
    LetGo,
}

impl Agent {
    /// Asks the daemon to start the session; [`Output::Started`] or [`Output::Failed`] follows.
    pub fn start(link: &Link, key: Generation, launch: Launch) -> Result<Self, Error> {
        link::send(link, ToDaemon::Start { key, launch }).map_err(|_lost| Error::Lost)?;
        Ok(Self::new(link, key, Use::Opened))
    }

    /// Attaches to an already-running hosted session under `key`.
    pub fn attach(link: &Link, key: Generation, hosted: HostedId) -> Result<Self, Error> {
        link::send(link, ToDaemon::Attach { key, hosted }).map_err(|_lost| Error::Lost)?;
        Ok(Self::new(link, key, Use::Used))
    }

    fn new(link: &Link, key: Generation, use_: Use) -> Self {
        Self {
            key,
            link: link.clone(),
            next_request: 0,
            pid: None,
            leaving: Leaving::End,
            use_,
        }
    }

    pub fn used(&self) -> bool {
        self.use_ == Use::Used
    }

    pub fn started(&mut self, pid: u32) {
        self.pid = Some(pid);
    }

    pub fn identify(&self, session: &SessionId) -> Result<(), Error> {
        let identify = ToDaemon::Identify {
            key: self.key,
            session: session.clone(),
        };
        link::send(&self.link, identify).map_err(|_lost| Error::Lost)
    }

    pub fn detach(mut self) {
        self.leaving = Leaving::LetGo;
    }

    fn send(&self, command: &Command) -> Result<(), Error> {
        let line = command.encode().map_err(Error::Encode)?;
        link::send_command(&self.link, self.key, &line).map_err(|_lost| Error::Lost)
    }

    pub fn prompt(
        &mut self,
        shell: &[String],
        images: &[Picture],
        text: &str,
    ) -> Result<(), Error> {
        let blocks = images
            .iter()
            .map(|picture| ImageBlock::base64(picture.media_type, BASE64.encode(&picture.bytes)))
            .collect();
        self.use_ = Use::Used;
        self.send(&Command::Prompt {
            shell: shell.to_vec(),
            images: blocks,
            text: text.to_owned(),
        })
    }

    pub fn run_shell(&mut self, command: &str) -> Result<(), Error> {
        self.use_ = Use::Used;
        self.send(&Command::Shell {
            command: command.to_owned(),
        })
    }

    pub fn request(&mut self, request: ControlRequest) -> Result<RequestId, Error> {
        self.next_request += 1;
        let id = RequestId::from(format!("gui_{}", self.next_request));
        self.send(&Command::ControlRequest {
            request_id: id.clone(),
            request,
        })?;
        Ok(id)
    }

    pub fn answer(&self, request: &RequestId, response: PermissionResult) -> Result<(), Error> {
        self.send(&Command::ControlResponse {
            response: Reply::Success {
                request_id: request.clone(),
                response,
            },
        })
    }

    pub fn choose(&self, request: &RequestId, option: &OptionId) -> Result<(), Error> {
        self.send(&Command::Choose {
            request_id: request.clone(),
            option: option.clone(),
        })
    }

    pub fn refuse(&self, request: &RequestId, error: &str) -> Result<(), Error> {
        self.send(&Command::ControlResponse {
            response: Reply::Error {
                request_id: request.clone(),
                error: error.to_owned(),
            },
        })
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let leave = match self.leaving {
            Leaving::End => {
                if let Err(error) = self.request(ControlRequest::Interrupt) {
                    tracing::debug!(%error, "couldn't interrupt session before ending it");
                }
                ToDaemon::End { key: self.key }
            }
            Leaving::LetGo => ToDaemon::Detach { key: self.key },
        };
        if link::send(&self.link, leave).is_err() {
            tracing::debug!("the daemon was gone as a window let go of a session");
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::provider::{Backend, Conversation, FolderConfiguration, SignInWith};

    #[test]
    fn a_session_is_used_once_prompted_or_given_a_shell_command() {
        let (link, _daemon) = async_channel::bounded(link::QUEUE);
        let launch = || Launch {
            backend: Backend::Claude,
            cwd: PathBuf::from("/work"),
            conversation: Conversation::New(SessionId::new_random()),
            model: None,
            folder_configuration: FolderConfiguration::Read,
            sign_in: SignInWith::Account,
        };

        let mut prompted = Agent::start(&link, Generation::default(), launch()).expect("sent");
        assert!(!prompted.used());
        prompted.prompt(&[], &[], "go on").expect("sent");
        assert!(prompted.used());

        let mut commanded = Agent::start(&link, Generation::default(), launch()).expect("sent");
        commanded.run_shell("ls").expect("sent");
        assert!(commanded.used());

        let attached = Agent::attach(&link, Generation::default(), HostedId::default());
        assert!(attached.expect("sent").used());
    }
}
