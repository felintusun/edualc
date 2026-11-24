//! The sessions an agent keeps, listed and found again with `session/list`.

use std::collections::HashSet;
use std::path::PathBuf;

use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

use crate::external::acp::{
    ClientCall, ClientCapabilities, ClientInfo, ElicitationCapabilities, FileCapabilities,
    FormCapabilities, Incoming, InitializeParams, JsonRpc, ListSessionsParams, ListSessionsResult,
    Request as AcpRequest, RpcError,
};
use crate::external::agy;
use crate::ids::SessionId;
use crate::sessions;

use super::agents::{Agent, on_path};
use super::write_line;

/// How long a `session/list` or `session/fork` asked of a program outside a session may take.
const PROBE_WAIT: std::time::Duration = std::time::Duration::from_secs(5);
/// The most pages of `session/list` read.
const MOST_PAGES: u64 = 10;

/// A session started outside edualc, as `session/list` gives it.
#[derive(Debug, Clone)]
pub(crate) struct ExternalSession {
    pub backend: crate::provider::Backend,
    pub raw_id: String,
    pub id: SessionId,
    pub cwd: PathBuf,
    pub title: String,
    /// When it last changed; `None` where `session/list` gives no `updatedAt` that reads as a
    /// time.
    pub modified: Option<std::time::SystemTime>,
}

/// Why an agent's sessions couldn't be listed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ListingError {
    #[error("no home folder ($HOME) to start it in")]
    NoHome,
    #[error("couldn't start it: {0}")]
    Start(#[source] std::io::Error),
    #[error("it was started without its pipes")]
    Pipes,
    #[error("it didn't answer in time")]
    TimedOut,
    #[error("couldn't write to it: {0}")]
    Write(#[source] std::io::Error),
    #[error("couldn't read from it: {0}")]
    Read(#[source] std::io::Error),
    #[error("it ended before it answered")]
    Ended,
    #[error("it refused ({}): {}", .0.code, .0.message)]
    Refused(RpcError),
    #[error("it answered with neither a result nor an error")]
    NoResult,
    #[error("couldn't write the request: {0}")]
    Encode(#[source] serde_json::Error),
    #[error("its list can't be read: {0}")]
    Unreadable(#[source] serde_json::Error),
}

/// Lists the sessions `agent` keeps, with `session/list`. An agent that isn't on PATH keeps
/// none that edualc can open.
///
/// # Errors
///
/// When the agent couldn't be started, didn't answer within [`PROBE_WAIT`], refused, or
/// answered with a list that can't be read.
pub(crate) async fn list_sessions(agent: Agent) -> Result<Vec<ExternalSession>, ListingError> {
    // The agent is started in the home folder to list its sessions; without one we don't guess.
    let cwd = dirs::home_dir().ok_or(ListingError::NoHome)?;
    let Some(program) = on_path(agent.program()) else {
        return Ok(Vec::new());
    };
    let mut process = agent.start(&program, &cwd).map_err(ListingError::Start)?;
    let listed = tokio::time::timeout(PROBE_WAIT, listed(agent, &mut process)).await;
    if let Err(error) = process.start_kill() {
        tracing::debug!(%error, %agent, "couldn't stop the agent asked for its sessions");
    }
    if let Err(error) = process.wait().await {
        tracing::debug!(%error, %agent, "the agent asked for its sessions didn't end well");
    }
    listed.map_err(|_late| ListingError::TimedOut)?
}

async fn listed(agent: Agent, process: &mut Child) -> Result<Vec<ExternalSession>, ListingError> {
    let mut stdin = process.stdin.take().ok_or(ListingError::Pipes)?;
    let stdout = process.stdout.take().ok_or(ListingError::Pipes)?;
    let mut lines = BufReader::new(stdout).lines();
    asked(&mut stdin, &mut lines, &initialize(1)).await?;
    let backend = agent.backend();
    let mut collected = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor: Option<String> = None;
    for id in 2..2 + MOST_PAGES {
        let request = AcpRequest {
            jsonrpc: JsonRpc::TwoPointZero,
            id,
            call: ClientCall::ListSessions(ListSessionsParams {
                cwd: None,
                cursor: cursor.clone(),
            }),
        };
        let answer = asked(&mut stdin, &mut lines, &request).await?;
        let list: ListSessionsResult =
            serde_json::from_value(answer).map_err(ListingError::Unreadable)?;
        let mut added = 0_usize;
        for session in list.sessions {
            if !seen.insert(session.session_id.clone()) {
                continue;
            }
            added += 1;
            let id = SessionId::for_external(backend, &session.session_id);
            let title = session
                .title
                .as_deref()
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .map_or_else(|| sessions::PetName::of(&id).to_string(), str::to_owned);
            collected.push(ExternalSession {
                backend,
                raw_id: session.session_id,
                id,
                cwd: session.cwd,
                title,
                modified: parse_rfc3339(session.updated_at.as_deref()),
            });
        }
        if added == 0 || list.next_cursor.is_none() || list.next_cursor == cursor {
            break;
        }
        cursor = list.next_cursor;
    }
    Ok(collected)
}

fn initialize(id: u64) -> AcpRequest {
    AcpRequest {
        jsonrpc: JsonRpc::TwoPointZero,
        id,
        call: ClientCall::Initialize(InitializeParams {
            protocol_version: 1,
            client_capabilities: ClientCapabilities {
                fs: FileCapabilities {
                    read_text_file: false,
                    write_text_file: false,
                },
                terminal: false,
                elicitation: ElicitationCapabilities {
                    form: FormCapabilities {},
                },
            },
            client_info: ClientInfo {
                name: "edualc".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
            },
        }),
    }
}

/// Sends `request` and returns the result of the response with its id; other lines are passed
/// over.
async fn asked(
    stdin: &mut ChildStdin,
    lines: &mut tokio::io::Lines<BufReader<ChildStdout>>,
    request: &AcpRequest,
) -> Result<serde_json::Value, ListingError> {
    let line = serde_json::to_string(request).map_err(ListingError::Encode)?;
    write_line(stdin, &line)
        .await
        .map_err(ListingError::Write)?;
    loop {
        let line = lines
            .next_line()
            .await
            .map_err(ListingError::Read)?
            .ok_or(ListingError::Ended)?;
        let Ok(Incoming::Response { id, result, error }) = serde_json::from_str(&line) else {
            continue;
        };
        if id.as_u64() != Some(request.id) {
            continue;
        }
        if let Some(error) = error {
            return Err(ListingError::Refused(error));
        }
        return result.ok_or(ListingError::NoResult);
    }
}

fn parse_rfc3339(text: Option<&str>) -> Option<std::time::SystemTime> {
    let text = text?;
    match text.parse::<jiff::Timestamp>() {
        Ok(timestamp) => Some(timestamp.into()),
        Err(error) => {
            tracing::debug!(%error, text, "a session's updatedAt that isn't a time");
            None
        }
    }
}

/// Returns the agent's id for session `id`'s conversation, if it keeps one. agy's is the same
/// id, which its records are kept under; opencode's is the one `session/list` gives for that
/// window id.
///
/// # Errors
///
/// When opencode's sessions couldn't be listed (see [`list_sessions`]).
pub(super) async fn resolve_conversation(
    agent: Agent,
    id: &SessionId,
) -> Result<Option<String>, ListingError> {
    match agent {
        Agent::Agy => Ok(agy::brain().and_then(|brain| {
            brain
                .join(id.as_str())
                .is_dir()
                .then(|| id.as_str().to_owned())
        })),
        Agent::Opencode => Ok(list_sessions(agent)
            .await?
            .into_iter()
            .find(|session| &session.id == id)
            .map(|session| session.raw_id)),
    }
}
