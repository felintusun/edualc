//! What the daemon reads and watches for the windows: the programs' files and edualc's
//! settings.

mod claude_trust;
mod read;
mod watch;

use std::ffi::OsStr;
use std::path::Path;

use tokio::sync::mpsc;

pub(crate) use claude_trust::{ClaudeTrustError, trust_for_claude_code, trusted_for_claude_code};
pub use read::{Catalog, answer, keep_catalog};
pub use watch::{Order, watch};

use crate::ipc::FromDaemon;
use crate::settings;

/// Where what is told to a window goes.
type Outbox = mpsc::Sender<FromDaemon>;

/// Writes `name` in `folder` whole or not at all, readable by this user alone, as Claude Code
/// writes its own.
pub(crate) fn write_atomically(folder: &Path, name: &OsStr, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let staged = folder.join(format!(".edualc-{}.tmp", uuid::Uuid::new_v4()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staged)?;
    let written = file
        .write_all(bytes)
        .and_then(|()| std::fs::rename(&staged, folder.join(name)));
    if written.is_err() {
        remove(&staged);
    }
    written
}

/// Removes a file edualc wrote, if it is there; one that stays is logged.
pub(crate) fn remove(path: &Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::debug!(%error, path = %path.display(), "a file of edualc's stays");
    }
}

/// A window's change to the settings, and where to say if it wasn't kept.
#[derive(Debug)]
pub struct SettingsChange {
    pub change: settings::Change,
    pub outbox: Outbox,
}

/// Owns the settings files: applies each window's change in turn to what the files hold, and
/// tells a window when its change couldn't be kept.
pub async fn keep_settings(mut changes: mpsc::Receiver<SettingsChange>) {
    while let Some(SettingsChange { change, outbox }) = changes.recv().await {
        let changed = tokio::task::spawn_blocking(move || settings::change(change)).await;
        let why = match changed {
            Ok(Ok(_)) => continue,
            Ok(Err(error)) => {
                tracing::warn!(%error, "couldn't keep the settings");
                error.to_string()
            }
            Err(error) => {
                tracing::error!(%error, "keeping the settings failed");
                error.to_string()
            }
        };
        if outbox.send(FromDaemon::SettingsNotKept(why)).await.is_err() {
            tracing::debug!("the window that changed the settings is gone");
        }
    }
}
