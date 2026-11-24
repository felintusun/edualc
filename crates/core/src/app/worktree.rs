//! The worktree a session works in.

use std::path::Path;

use super::App;
use crate::present::Frontend;
use crate::sessions::Worktree;

impl<F: Frontend> App<F> {
    /// Returns the worktree of the session shown: the watched one's as its process reports it,
    /// or ours.
    pub(super) fn current_worktree(&self) -> Option<&Worktree> {
        match &self.watch {
            Some(watch) => watch
                .session
                .worktree
                .as_ref()
                .or(self.status.worktree.as_ref()),
            None => self.status.worktree.as_ref(),
        }
    }

    /// Opens the current worktree in this window.
    pub(super) fn open_worktree(&mut self) {
        let Some(worktree) = self.current_worktree().cloned() else {
            return;
        };
        // We read the folder first: the worktree may be gone since it was read.
        self.read_folder(crate::ipc::Read::Folder(worktree.path), false);
    }

    /// Looks for the worktree the session entered, as its transcript says after a turn.
    pub(super) fn read_worktree(&mut self) {
        let Some(id) = self.status.session_id.clone() else {
            return;
        };
        let Some(cwd) = self.location.cwd().map(Path::to_path_buf) else {
            return;
        };
        let read = crate::ipc::Read::WorktreeOf {
            cwd,
            id: id.clone(),
        };
        self.read(read, super::Reading::Worktree { id });
    }

    pub(super) fn worktree_read(&mut self, id: &crate::ids::SessionId, worktree: Option<Worktree>) {
        if self.status.session_id.as_ref() == Some(id) && worktree.is_some() {
            self.status.worktree = worktree;
        }
    }
}
