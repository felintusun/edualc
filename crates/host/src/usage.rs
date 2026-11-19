//! On-demand usage queries for every coding agent backend (`claude`, `agy`, `opencode`) so the
//! Usage popover works from any page or session, even when a provider has no active session in
//! this window.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::Command;

use crate::claude::{self, Backend};
use crate::external::wire::{
    AssistantError, ControlRequest, ControlResponse, Input, Message, RateLimitReport, RequestId,
    UsageReport,
};
use crate::ipc::ProviderUsage;

/// How long a standalone `claude` usage probe gets to answer.
const CLAUDE_USAGE_WAIT: Duration = Duration::from_secs(15);

/// Which coding agent binaries exist on PATH (or are named by `CLAUDE_BIN`).
pub(crate) fn available_backends() -> Vec<Backend> {
    Backend::ALL
        .into_iter()
        .filter(|&backend| is_available(backend))
        .collect()
}

fn is_available(backend: Backend) -> bool {
    match backend {
        Backend::Claude => find_executable(&claude::program()).is_some(),
        Backend::Agy => find_executable(OsStr::new("agy")).is_some(),
        Backend::Opencode => find_executable(OsStr::new("opencode")).is_some(),
    }
}

fn find_executable(program: &OsStr) -> Option<PathBuf> {
    let as_path = Path::new(program);
    if as_path.components().count() > 1 {
        return as_path.is_file().then(|| as_path.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

fn working_dir(cwd: Option<PathBuf>) -> PathBuf {
    cwd.filter(|dir| dir.is_dir())
        .or_else(dirs::home_dir)
        .filter(|dir| dir.is_dir())
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// Queries `backend` for its current usage / plan limits.
pub(crate) async fn query(backend: Backend, cwd: Option<PathBuf>) -> ProviderUsage {
    if !is_available(backend) {
        return ProviderUsage::Unavailable;
    }
    let dir = working_dir(cwd);
    match backend {
        Backend::Claude => query_claude(dir).await,
        Backend::Agy => query_agy(dir).await,
        Backend::Opencode => ProviderUsage::noted("opencode doesn't report usage or plan limits."),
    }
}

async fn query_agy(cwd: PathBuf) -> ProviderUsage {
    match crate::acp::agy_quotas(cwd).await {
        Ok(quotas) => {
            let limits = crate::acp::translate::plan_limits(&quotas, None);
            ProviderUsage::reported(UsageReport {
                rate_limits: RateLimitReport { limits },
                ..UsageReport::default()
            })
        }
        Err(error) => ProviderUsage::noted(format!("agy couldn't say its plan's quotas: {error}")),
    }
}

async fn query_claude(cwd: PathBuf) -> ProviderUsage {
    let Some(program) = find_executable(&claude::program()) else {
        return ProviderUsage::Unavailable;
    };
    let mut command = Command::new(program);
    command.args([
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--setting-sources=user",
        "--strict-mcp-config",
    ]);
    for name in crate::spawn::INHERITED_SESSION {
        command.env_remove(name);
    }
    command
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return ProviderUsage::noted(format!("Couldn't start Claude Code: {error}"));
        }
    };
    let Some(mut stdin) = child.stdin.take() else {
        return ProviderUsage::noted("Claude Code's input wasn't piped.");
    };
    let Some(stdout) = child.stdout.take() else {
        return ProviderUsage::noted("Claude Code's output wasn't piped.");
    };

    let init = Input::ControlRequest {
        request_id: RequestId::from("u_init".to_owned()),
        request: ControlRequest::Initialize {
            per_task_stop_affordance: false,
        },
    };
    let usage_id = RequestId::from("u_usage".to_owned());
    let usage_req = Input::ControlRequest {
        request_id: usage_id.clone(),
        request: ControlRequest::GetUsage,
    };
    let payload = match (
        serde_json::to_string(&init),
        serde_json::to_string(&usage_req),
    ) {
        (Ok(init_line), Ok(usage_line)) => format!("{init_line}\n{usage_line}\n"),
        (Err(error), _) | (_, Err(error)) => {
            return ProviderUsage::noted(format!("Couldn't encode usage request: {error}"));
        }
    };
    if let Err(error) = stdin.write_all(payload.as_bytes()).await {
        return ProviderUsage::noted(format!("Couldn't ask Claude Code: {error}"));
    }
    if let Err(error) = stdin.flush().await {
        tracing::debug!(%error, "flushing claude usage probe failed");
    }

    let outcome = tokio::time::timeout(CLAUDE_USAGE_WAIT, async {
        let mut lines = BufReader::new(stdout).lines();
        let mut auth_failed = false;
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(message) = serde_json::from_str::<Message>(&line) else {
                continue;
            };
            match message {
                Message::Assistant {
                    error: Some(AssistantError::AuthenticationFailed),
                    ..
                } => auth_failed = true,
                Message::ControlResponse {
                    response:
                        ControlResponse::Success {
                            request_id,
                            response,
                        },
                } if request_id == usage_id => {
                    return match serde_json::from_value::<UsageReport>(response) {
                        Ok(report) => ProviderUsage::reported(report),
                        Err(error) => ProviderUsage::noted(format!(
                            "Couldn't read Claude Code's usage: {error}"
                        )),
                    };
                }
                Message::ControlResponse {
                    response: ControlResponse::Error { request_id, error },
                } if request_id == usage_id => {
                    return ProviderUsage::noted(error);
                }
                Message::StreamEvent { .. }
                | Message::Assistant { .. }
                | Message::User(_)
                | Message::System(_)
                | Message::Result(_)
                | Message::RateLimitEvent { .. }
                | Message::ControlRequest { .. }
                | Message::ControlResponse { .. }
                | Message::ControlCancelRequest { .. }
                | Message::Other => {}
            }
        }
        let note = if auth_failed {
            "Claude Code isn't signed in. Run `claude auth login` in a terminal."
        } else {
            "Claude Code ended before reporting usage."
        };
        ProviderUsage::noted(note)
    })
    .await;

    drop(stdin);
    if let Err(error) = child.start_kill() {
        tracing::debug!(%error, "claude usage probe already ended");
    }
    if let Err(error) = child.wait().await {
        tracing::debug!(%error, "waiting for claude usage probe failed");
    }

    outcome
        .unwrap_or_else(|_elapsed| ProviderUsage::noted("Claude Code didn't report usage in time."))
}
