//! The sessions on disk: Claude Code's in its folder, agy's in its brain folder, and what the
//! lists show of them.

mod agy;
mod claude;
mod names;
mod titles;

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use web_time::SystemTime;

pub use agy::{backend_of, is_agy_brain_session};
pub use claude::{
    ClaudeHome, GLOBAL_CONFIG_BYTES, Saved, find_transcript, parse_lines, saved, transcript_of,
    transcript_path, written_transcript,
};
#[cfg(not(target_arch = "wasm32"))]
pub use claude::{list, live, projects, read_lines_from};
pub use names::{MessagingName, PetName};
pub use titles::{assistant_sentence, first_sentence, worktree_of};

use crate::external::wire;
use crate::ids::SessionId;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionInfo {
    pub backend: crate::provider::Backend,
    pub id: SessionId,
    pub title: String,
    /// When it last changed; `None` where the program doesn't say.
    pub modified: Option<SystemTime>,
    /// The worktree it last worked in, if that still exists.
    pub worktree: Option<Worktree>,
    /// The first sentence of the last assistant reply, if one is in the tail.
    #[serde(default)]
    pub last_said: Option<String>,
}

/// A git worktree a session works in (`claude --worktree`, the EnterWorktree tool).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Worktree {
    pub path: PathBuf,
    pub name: String,
    pub branch: String,
}

impl Worktree {
    fn existing(session: wire::WorktreeSession) -> Option<Self> {
        session.path.is_dir().then(|| Self {
            name: if session.name.is_empty() {
                folder_name(&session.path)
            } else {
                session.name
            },
            path: session.path,
            branch: session.branch,
        })
    }
}

/// Returns `path` as a worktree if it is a linked one, whose `.git` is a file naming the real
/// one.
pub fn linked_worktree(path: &Path) -> Option<Worktree> {
    let pointer = read_small(&path.join(".git"), GIT_POINTER_BYTES)?;
    let gitdir = PathBuf::from(pointer.strip_prefix("gitdir:")?.trim());
    let gitdir = if gitdir.is_absolute() {
        gitdir
    } else {
        path.join(gitdir)
    };
    let head = read_small(&gitdir.join("HEAD"), GIT_POINTER_BYTES).unwrap_or_default();
    let branch = head
        .trim()
        .strip_prefix("ref: refs/heads/")
        .unwrap_or("(detached)")
        .to_owned();
    Some(Worktree {
        path: path.to_owned(),
        name: folder_name(path),
        branch,
    })
}

/// Returns the last part of a path, for a title; the whole path for `/`.
pub fn folder_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectInfo {
    pub path: PathBuf,
    /// When a session in it last changed; `None` where no program says.
    pub modified: Option<SystemTime>,
}

/// A session a Claude Code process runs now, in a terminal, an editor or the background, as
/// the process reports itself.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct LiveSession {
    pub pid: u32,
    /// When its process started, to tell it from a later one with the same pid.
    pub started: String,
    pub id: SessionId,
    pub cwd: PathBuf,
    /// The name other sessions message it by.
    pub name: String,
    /// The title its transcript gives, once it has one.
    pub title: Option<String>,
    pub kind: LiveKind,
    pub status: LiveStatus,
    pub worktree: Option<Worktree>,
    /// Its subagents running now, as their records beside its transcript tell.
    pub agents: usize,
    /// The program it runs, told by where its transcript is; Claude Code before it has one.
    pub backend: crate::provider::Backend,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveKind {
    Interactive,
    /// Started with `--bg`.
    Bg,
    #[default]
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveStatus {
    Idle,
    Busy,
    /// Waiting for its user: a permission request or a question.
    Waiting,
    #[default]
    #[serde(other)]
    Other,
}

/// Returns whether `session_cwd` belongs to the project `project_cwd`: it is the project, one
/// of its `.claude/worktrees/`, or a linked git worktree whose gitdir is in its `.git`.
#[cfg(not(target_arch = "wasm32"))]
pub fn belongs_to_project(session_cwd: &Path, project_cwd: &Path) -> bool {
    if session_cwd == project_cwd
        || session_cwd.starts_with(project_cwd.join(".claude").join("worktrees"))
    {
        return true;
    }
    let Some(pointer) = read_small(&session_cwd.join(".git"), GIT_POINTER_BYTES) else {
        return false;
    };
    let Some(gitdir) = pointer.strip_prefix("gitdir:").map(str::trim) else {
        return false;
    };
    let gitdir = if Path::new(gitdir).is_absolute() {
        PathBuf::from(gitdir)
    } else {
        session_cwd.join(gitdir)
    };
    gitdir.starts_with(project_cwd.join(".git"))
}

/// A `.git` file or a `HEAD` names a path or a branch.
const GIT_POINTER_BYTES: u64 = 4096;
/// Reads a small regular file's text. A FIFO would block, and a device or a huge file could be
/// put where a record or `.git` is expected by anything running as you.
fn read_small(path: &Path, limit: u64) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > limit {
        return None;
    }
    let mut text = String::new();
    File::open(path)
        .ok()?
        .take(limit)
        .read_to_string(&mut text)
        .ok()?;
    Some(text)
}

/// Says how long ago, briefly: "now", "5m", "3h", "2d", "6w"; nothing for no time.
pub fn age(modified: Option<SystemTime>) -> String {
    let Some(modified) = modified else {
        return String::new();
    };
    let seconds = SystemTime::now()
        .duration_since(modified)
        .map_or(0, |elapsed| elapsed.as_secs());
    match seconds {
        0..60 => "now".into(),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h", seconds / 3600),
        86400..604_800 => format!("{}d", seconds / 86400),
        _ => format!("{}w", seconds / 604_800),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_linked_worktree_names_its_branch() {
        let dir = std::env::temp_dir().join(format!("edualc-worktree-{}", std::process::id()));
        let gitdir = dir.join("repo-git-worktrees-w");
        let worktree = dir.join("w");
        std::fs::create_dir_all(&gitdir).expect("a scratch git dir");
        std::fs::create_dir_all(&worktree).expect("a scratch worktree");
        std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/feature\n").expect("a HEAD");
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .expect("a .git file");
        let found = linked_worktree(&worktree).expect("a linked worktree");
        assert_eq!(
            (found.name.as_str(), found.branch.as_str()),
            ("w", "feature")
        );
        assert_eq!(linked_worktree(&dir), None);
        std::fs::remove_dir_all(&dir).expect("the scratch directory goes");
    }

    #[test]
    fn external_ids_map_deterministically_and_agys_records_tell_its_sessions() {
        let uuid = "e4cd185b-4679-4fa9-8245-69f15bdbbbe1";
        assert_eq!(
            SessionId::for_external(crate::provider::Backend::Agy, uuid).as_str(),
            uuid
        );
        let ses_a = SessionId::for_external(crate::provider::Backend::Opencode, "ses_abc123");
        let ses_b = SessionId::for_external(crate::provider::Backend::Opencode, "ses_abc123");
        let ses_c = SessionId::for_external(crate::provider::Backend::Opencode, "ses_xyz789");
        assert_eq!(ses_a, ses_b);
        assert_ne!(ses_a, ses_c);
        // The UUIDv5 of "opencode:ses_abc123" in edualc's namespace, as Python's uuid5 makes it:
        // a session's id must not change between releases, or its Changes snapshots are lost.
        assert_eq!(ses_a.as_str(), "5752e0e1-4cd0-5d87-930c-4104ab8263aa");
        if let Some(brain) = crate::external::agy::brain() {
            assert_eq!(
                backend_of(
                    &brain
                        .join(uuid)
                        .join(".system_generated/logs/transcript.jsonl")
                ),
                crate::provider::Backend::Agy
            );
        }
    }
}
