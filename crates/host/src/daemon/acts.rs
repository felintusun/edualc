//! What a window asks of the daemon's machine: the keyring, a folder's trust, a new project.

use std::path::{Path, PathBuf};

use tokio::sync::mpsc;

use crate::ipc::{self, Act, ActFailure, ActId, CreateProjectError, FromDaemon, ProjectName};
use crate::{files, trust};

use super::tell;

/// Does what the window asked, on a thread of its own where it may take a while, and tells the
/// window.
pub(super) async fn act_for(request: ActId, act: Act, outbox: mpsc::Sender<FromDaemon>) {
    let keyring_failure = |error: crate::keyring::Error| ActFailure::Keyring(error.to_string());
    match act {
        Act::StoreApiKey(key) => {
            let result = crate::keyring::store(&key).await.map_err(keyring_failure);
            tell(&outbox, FromDaemon::Done { request, result }).await;
        }
        Act::RemoveApiKey => {
            let result = crate::keyring::remove().await.map_err(keyring_failure);
            tell(&outbox, FromDaemon::Done { request, result }).await;
        }
        Act::CheckApiKey => {
            let stored = crate::keyring::stored().await.map_err(keyring_failure);
            tell(&outbox, FromDaemon::ApiKeyStored { request, stored }).await;
        }
        Act::CheckClaudeTrust { folder } => {
            let read =
                tokio::task::spawn_blocking(move || files::trusted_for_claude_code(&folder)).await;
            let trusted = match read {
                Ok(read) => read.map_err(|error| claude_trust_failure(&error)),
                Err(error) => {
                    tracing::error!(%error, "reading Claude Code's trust failed");
                    Err(ActFailure::Crashed)
                }
            };
            tell(&outbox, FromDaemon::ClaudeTrust { request, trusted }).await;
        }
        // It may wait a moment for Claude Code's lock (see `files::trust_for_claude_code`).
        Act::TrustForClaudeCode { folder } => {
            let recorded =
                tokio::task::spawn_blocking(move || files::trust_for_claude_code(&folder)).await;
            let result = match recorded {
                Ok(recorded) => recorded
                    .map(|_| ())
                    .map_err(|error| claude_trust_failure(&error)),
                Err(error) => {
                    tracing::error!(%error, "recording Claude Code's trust failed");
                    Err(ActFailure::Crashed)
                }
            };
            tell(&outbox, FromDaemon::Done { request, result }).await;
        }
        Act::CreateProject { parent, name } => {
            let created = tokio::task::spawn_blocking(move || create_project(&parent, &name)).await;
            let result = created.unwrap_or_else(|error| {
                tracing::error!(%error, "making the project folder failed");
                Err(CreateProjectError::Crashed)
            });
            tell(&outbox, FromDaemon::Created { request, result }).await;
        }
        Act::CheckTrust { folder } => {
            // We read what the folder would run only for one that isn't trusted: it is asked
            // about.
            let read = tokio::task::spawn_blocking(move || {
                if trust::accepted_in_terminal(&folder) {
                    ipc::FolderTrust::Accepted
                } else {
                    ipc::FolderTrust::Untrusted(trust::what_runs(&folder))
                }
            })
            .await;
            // A failed read is said as such: an empty list would read as a folder that runs
            // nothing.
            let trust = read.unwrap_or_else(|error| {
                tracing::error!(%error, "reading the folder's trust failed");
                ipc::FolderTrust::Untrusted(vec![ipc::Runs::Unreadable {
                    file: "its settings".to_owned(),
                    reason: format!("the edualc daemon failed reading them: {error}"),
                }])
            });
            tell(&outbox, FromDaemon::Trusted { request, trust }).await;
        }
        // For the changes' keeper, which a window's connection hands them to.
        Act::ChangesLooked { .. } | Act::MarkReviewed { .. } | Act::UnmarkReviewed { .. } => {}
    }
}

/// Says why Claude Code's trust wasn't read or recorded: its lock held throughout, or the
/// error with what caused it.
pub(super) fn claude_trust_failure(error: &files::ClaudeTrustError) -> ActFailure {
    if let files::ClaudeTrustError::Busy(_) = error {
        return ActFailure::ClaudeCodeBusy;
    }
    let mut words = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        words.push_str(": ");
        words.push_str(&cause.to_string());
        source = cause.source();
    }
    ActFailure::ClaudeCodeTrust(words)
}

/// Makes a new project folder inside `parent`.
pub(super) fn create_project(
    parent: &Path,
    name: &ProjectName,
) -> Result<PathBuf, CreateProjectError> {
    if !parent.is_dir() {
        return Err(CreateProjectError::NoParent(parent.to_path_buf()));
    }
    let target = parent.join(name.as_str());
    std::fs::create_dir(&target).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            CreateProjectError::AlreadyExists(target.clone())
        } else {
            CreateProjectError::Create {
                folder: target.clone(),
                reason: error.to_string(),
            }
        }
    })?;
    Ok(target)
}
