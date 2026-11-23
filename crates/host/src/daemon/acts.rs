//! What a window asks of the desktop (opening a folder or a file, pasting, a new project), done
//! for it and told back.

use std::path::{Path, PathBuf};

use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;

use crate::decode::{self, Refusal};
use crate::desktop;
use crate::ipc::{
    self, Act, ActFailure, ActId, CreateProjectError, FromDaemon, PasteFailure, ProjectName,
};
use crate::{files, trust};

use super::{CLIPBOARD, MAX_CLIPBOARD_TYPES_BYTES, Reach, tell};

/// Does what the desktop is asked, on a thread of its own (it may take a while), and tells the
/// window.
pub(super) async fn act_for(
    request: ActId,
    act: Act,
    reach: Reach,
    outbox: mpsc::Sender<FromDaemon>,
) {
    // The desktop here is the user's of this machine; a line over the network has its own.
    if reach == Reach::Network && act.needs_its_desktop() {
        match act {
            Act::PickFolder => {
                tell(
                    &outbox,
                    FromDaemon::Picked {
                        request,
                        folder: None,
                    },
                )
                .await
            }
            Act::PasteImage => tell_pasted(&outbox, request, &Err(PasteFailure::NoImage)).await,
            Act::OpenUrl(_)
            | Act::Notify { .. }
            | Act::FocusWindowOf { .. }
            | Act::CheckTrust { .. }
            | Act::CheckClaudeTrust { .. }
            | Act::TrustForClaudeCode { .. }
            | Act::StoreApiKey(_)
            | Act::RemoveApiKey
            | Act::CheckApiKey
            | Act::CreateProject { .. }
            | Act::ChangesLooked { .. }
            | Act::MarkReviewed { .. }
            | Act::UnmarkReviewed { .. } => {
                tell(
                    &outbox,
                    FromDaemon::Done {
                        request,
                        result: Err(ActFailure::NotHere),
                    },
                )
                .await;
            }
        }
        return;
    }
    let keyring_failure = |error: crate::keyring::Error| ActFailure::Keyring(error.to_string());
    let act = match act {
        Act::StoreApiKey(key) => {
            let result = crate::keyring::store(&key).await.map_err(keyring_failure);
            tell(&outbox, FromDaemon::Done { request, result }).await;
            return;
        }
        Act::RemoveApiKey => {
            let result = crate::keyring::remove().await.map_err(keyring_failure);
            tell(&outbox, FromDaemon::Done { request, result }).await;
            return;
        }
        Act::CheckApiKey => {
            let stored = crate::keyring::stored().await.map_err(keyring_failure);
            tell(&outbox, FromDaemon::ApiKeyStored { request, stored }).await;
            return;
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
            return;
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
            return;
        }
        act @ (Act::OpenUrl(_)
        | Act::PickFolder
        | Act::PasteImage
        | Act::CheckTrust { .. }
        | Act::CreateProject { .. }
        | Act::Notify { .. }
        | Act::FocusWindowOf { .. }) => act,
        // For the changes' keeper, which a window's connection hands them to.
        Act::ChangesLooked { .. } | Act::MarkReviewed { .. } | Act::UnmarkReviewed { .. } => {
            return;
        }
    };
    if let Act::CreateProject { parent, name } = act {
        let created = tokio::task::spawn_blocking(move || create_project(&parent, &name)).await;
        let result = created.unwrap_or_else(|error| {
            tracing::error!(%error, "making the project folder failed");
            Err(CreateProjectError::Crashed)
        });
        tell(&outbox, FromDaemon::Created { request, result }).await;
        return;
    }
    if act == Act::PickFolder {
        let picked = tokio::task::spawn_blocking(move || {
            rfd::FileDialog::new()
                .set_title("Open a folder")
                .pick_folder()
        })
        .await;
        let folder = picked.unwrap_or_else(|error| {
            tracing::error!(%error, "the folder picker failed");
            None
        });
        tell(&outbox, FromDaemon::Picked { request, folder }).await;
        return;
    }
    if act == Act::PasteImage {
        let pasted = paste_image().await;
        tell_pasted(&outbox, request, &pasted).await;
        return;
    }
    if let Act::CheckTrust { folder } = act {
        // What the folder would run is read only for one that isn't trusted: it is asked about.
        let read = tokio::task::spawn_blocking(move || {
            if trust::accepted_in_terminal(&folder) {
                ipc::FolderTrust::Accepted
            } else {
                ipc::FolderTrust::Untrusted(trust::what_runs(&folder))
            }
        })
        .await;
        // A failed read is said as such: an empty list would read as a folder that runs nothing.
        let trust = read.unwrap_or_else(|error| {
            tracing::error!(%error, "reading the folder's trust failed");
            ipc::FolderTrust::Untrusted(vec![ipc::Runs::Unreadable {
                file: "its settings".to_owned(),
                reason: format!("the edualc daemon failed reading them: {error}"),
            }])
        });
        tell(&outbox, FromDaemon::Trusted { request, trust }).await;
        return;
    }
    let done = tokio::task::spawn_blocking(move || perform(act)).await;
    let result = done.unwrap_or_else(|error| {
        tracing::error!(%error, "doing what the desktop was asked failed");
        Err(ActFailure::Crashed)
    });
    tell(&outbox, FromDaemon::Done { request, result }).await;
}

/// The clipboard's image, read with `wl-paste` and decoded in the confined helper (see
/// `decode`).
pub(super) async fn paste_image() -> ipc::Pasted {
    let listed = clipboard(&["--list-types"], MAX_CLIPBOARD_TYPES_BYTES).await?;
    let format = String::from_utf8_lossy(&listed)
        .lines()
        .find_map(|line| line.trim().parse::<decode::Format>().ok())
        .ok_or(PasteFailure::NoImage)?;
    let limit = decode::MAX_INPUT_BYTES + 1;
    let bytes = clipboard(&["--no-newline", "--type", format.as_ref()], limit).await?;
    if bytes.len() > decode::MAX_INPUT_BYTES {
        return Err(PasteFailure::Refused(Refusal::TooManyBytes));
    }
    decode::decode(&bytes).await.map_err(|error| match error {
        decode::Error::Refused(refusal) => PasteFailure::Refused(refusal),
        error @ (decode::Error::Start(_)
        | decode::Error::NoPipes
        | decode::Error::Hand(_)
        | decode::Error::Read(_)
        | decode::Error::Slow
        | decode::Error::Failed(_)
        | decode::Error::TooMuch
        | decode::Error::Unreadable(_)
        | decode::Error::Malformed) => PasteFailure::Decoder(error.to_string()),
    })
}

/// What `wl-paste` gives with `args`, up to `limit` bytes (reading stops there).
pub(super) async fn clipboard(args: &[&str], limit: usize) -> Result<Vec<u8>, PasteFailure> {
    let failed = |error: std::io::Error| PasteFailure::Clipboard(error.to_string());
    let mut child = tokio::process::Command::new("wl-paste")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(failed)?;
    let Some(output) = child.stdout.take() else {
        return Err(PasteFailure::Clipboard("wl-paste has no output".to_owned()));
    };
    let reading = async {
        let mut bytes = Vec::new();
        let cap = u64::try_from(limit).unwrap_or(u64::MAX);
        output
            .take(cap)
            .read_to_end(&mut bytes)
            .await
            .map_err(failed)?;
        if bytes.len() >= limit {
            // The caller refuses it; what is left isn't waited for (the child is killed as it
            // is dropped).
            return Ok(bytes);
        }
        let status = child.wait().await.map_err(failed)?;
        // It fails when the clipboard is empty, or holds nothing of the type asked.
        if status.success() {
            Ok(bytes)
        } else {
            Err(PasteFailure::NoImage)
        }
    };
    tokio::time::timeout(CLIPBOARD, reading)
        .await
        .map_err(|_elapsed| {
            PasteFailure::Clipboard("the clipboard's owner didn't hand it over in time".to_owned())
        })?
}

/// The pasted image to a window, in pieces that fit messages.
pub(super) async fn tell_pasted(
    outbox: &mpsc::Sender<FromDaemon>,
    request: ActId,
    pasted: &ipc::Pasted,
) {
    let encoded = match postcard::to_stdvec(pasted) {
        Ok(encoded) => encoded,
        Err(error) => {
            tracing::error!(%error, "couldn't encode a pasted image");
            return;
        }
    };
    let mut pieces = encoded.chunks(ipc::MAX_CHUNK).peekable();
    while let Some(piece) = pieces.next() {
        let last = pieces.peek().is_none();
        let piece = piece.to_vec();
        tell(
            outbox,
            FromDaemon::Pasted {
                request,
                piece,
                last,
            },
        )
        .await;
    }
}

/// Does what the desktop is asked (it may take a while: a session stopping elsewhere).
pub(super) fn perform(act: Act) -> Result<(), ActFailure> {
    match act {
        Act::Notify { summary, body } => desktop::notify(&summary, &body)?,
        Act::OpenUrl(link) => {
            // Claude writes the links: a file or another app's scheme could run what it wrote.
            if !desktop::is_web_link(&link) {
                return Err(ActFailure::NotAWebLink(link));
            }
            open::that_detached(&link).map_err(|error| ActFailure::Open {
                reason: error.to_string(),
                link,
            })?;
        }
        Act::FocusWindowOf { pid } => desktop::focus_window_of(pid)?,
        // Answered in `act_for`: as `Picked`, `Pasted`, `Trusted`, `Created` and `ApiKeyStored`,
        // or done with the keyring there.
        Act::PickFolder
        | Act::PasteImage
        | Act::CheckTrust { .. }
        | Act::CreateProject { .. }
        | Act::StoreApiKey(_)
        | Act::RemoveApiKey
        | Act::CheckApiKey
        | Act::ChangesLooked { .. }
        | Act::MarkReviewed { .. }
        | Act::UnmarkReviewed { .. }
        | Act::CheckClaudeTrust { .. }
        | Act::TrustForClaudeCode { .. } => {}
    }
    Ok(())
}

/// Why Claude Code's trust wasn't read or recorded, as the window is told: its lock held
/// throughout, or the error with what caused it.
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
