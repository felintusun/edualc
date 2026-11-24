//! The sessions an agent keeps, listed and found again with `session/list`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

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
const MAX_LIST_PAGES: usize = 10;

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

async fn send_and_wait_response(
    stdin: &mut ChildStdin,
    lines: &mut tokio::io::Lines<BufReader<ChildStdout>>,
    request_line: &str,
    expected_id: u64,
) -> Option<Result<serde_json::Value, RpcError>> {
    if write_line(stdin, &request_line).await.is_err() {
        return None;
    }
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(Incoming::Response { id, result, error }) = serde_json::from_str(&line) else {
            continue;
        };
        if id.as_u64() != Some(expected_id) {
            continue;
        }
        if let Some(error) = error {
            return Some(Err(error));
        }
        return Some(Ok(result.unwrap_or(serde_json::Value::Null)));
    }
    None
}

async fn start_initialized_acp(
    agent: Agent,
    cwd: &Path,
) -> Option<(Child, ChildStdin, tokio::io::Lines<BufReader<ChildStdout>>)> {
    on_path(agent.program())?;
    let mut process = agent.start(cwd).ok()?;
    let mut stdin = process.stdin.take()?;
    let stdout = process.stdout.take()?;
    let mut lines = BufReader::new(stdout).lines();
    let init = AcpRequest {
        jsonrpc: JsonRpc::TwoPointZero,
        id: 1,
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
    };
    let init_line = serde_json::to_string(&init).ok()?;
    match send_and_wait_response(&mut stdin, &mut lines, &init_line, 1).await {
        Some(Ok(_)) => Some((process, stdin, lines)),
        Some(Err(error)) => {
            tracing::debug!(%agent, %error.message, "acp initialize refused");
            None
        }
        None => None,
    }
}

/// Lists the sessions `agent` keeps, with `session/list`.
pub(crate) async fn list_sessions(agent: Agent) -> Vec<ExternalSession> {
    // The agent is started in the home folder to list its sessions; without one we don't guess.
    let Some(cwd) = dirs::home_dir() else {
        tracing::warn!(%agent, "no home folder ($HOME) to list the sessions from");
        return Vec::new();
    };
    let result = tokio::time::timeout(PROBE_WAIT, list_sessions_inner(agent, &cwd)).await;
    match result {
        Ok(sessions) => sessions,
        Err(_late) => {
            tracing::debug!(%agent, "acp session/list timed out");
            Vec::new()
        }
    }
}

async fn list_sessions_inner(agent: Agent, cwd: &Path) -> Vec<ExternalSession> {
    let Some((mut process, mut stdin, mut lines)) = start_initialized_acp(agent, cwd).await else {
        return Vec::new();
    };
    let mut collected = Vec::new();
    let mut seen_raw = HashSet::new();
    let mut cursor: Option<String> = None;
    let backend = agent.backend();
    for page in 0..MAX_LIST_PAGES {
        let req_id = 2 + page as u64;
        let request = AcpRequest {
            jsonrpc: JsonRpc::TwoPointZero,
            id: req_id,
            call: ClientCall::ListSessions(ListSessionsParams {
                cwd: None,
                cursor: cursor.clone(),
            }),
        };
        let Ok(line) = serde_json::to_string(&request) else {
            break;
        };
        let Some(Ok(value)) = send_and_wait_response(&mut stdin, &mut lines, &line, req_id).await
        else {
            break;
        };
        let Ok(list) = serde_json::from_value::<ListSessionsResult>(value) else {
            break;
        };
        if list.sessions.is_empty() {
            break;
        }
        let mut added = 0_usize;
        for session in list.sessions {
            if !seen_raw.insert(session.session_id.clone()) {
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
    if let Err(error) = process.start_kill() {
        tracing::debug!(%error, %agent, "couldn't stop acp session/list child");
    }
    if let Err(error) = process.wait().await {
        tracing::debug!(%error, %agent, "acp session/list child wait failed");
    }
    collected
}

/// Returns the agent's id for session `id`'s conversation. agy's is the same id, which its
/// records are kept under; opencode's is the one `session/list` gives for that window id.
pub(super) async fn resolve_conversation(agent: Agent, id: &SessionId) -> Option<String> {
    match agent {
        Agent::Agy => agy::brain()?
            .join(id.as_str())
            .is_dir()
            .then(|| id.as_str().to_owned()),
        Agent::Opencode => list_sessions(agent)
            .await
            .into_iter()
            .find(|session| &session.id == id)
            .map(|session| session.raw_id),
    }
}
