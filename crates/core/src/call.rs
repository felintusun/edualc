//! A tool call in the terms of the program that made it: Claude Code's by its tool's name and
//! input, an ACP agent's (agy's, opencode's) as the agent describes it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::external::wire::{ToolKind, ToolName};
use crate::tools::{self, Depth, ToolView};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// A tool the window knows by its name: Claude Code's, edualc's own, and those of an ACP
    /// agent's that edualc acts on (a question, a to-do list, a subagent).
    Named {
        name: ToolName,
        input: Value,
    },
    Acp(AcpCall),
}

/// An ACP agent's tool call, as the agent gives it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AcpCall {
    /// The agent's own name for the tool, if it says (`run_command`).
    pub name: Option<String>,
    /// What the agent says the call does.
    pub title: String,
    pub kind: Option<AcpKind>,
    /// The files it works on.
    pub locations: Vec<PathBuf>,
    /// The files it changes.
    pub diffs: Vec<Diff>,
    /// Its arguments, as the agent gave them.
    pub input: Option<Value>,
    pub status: AcpStatus,
    /// What it printed or said, if it has.
    pub output: Option<String>,
}

/// Where an ACP agent's call is, by ACP's statuses: a call the agent hasn't said one for is
/// pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

/// What an ACP agent's call does, by ACP's kinds of tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    Other,
}

/// A file a call changes: a new file has no old text.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Diff {
    pub path: PathBuf,
    pub old_text: Option<String>,
    pub new_text: String,
}

impl Call {
    pub fn describe(&self, cwd: &Path, depth: Depth) -> ToolView {
        match self {
            Self::Named { name, input } => tools::describe(name, input, cwd, depth),
            Self::Acp(call) => tools::describe_acp(call, cwd, depth),
        }
    }

    pub fn kind(&self) -> ToolKind {
        match self {
            Self::Named { name, .. } => name.kind(),
            Self::Acp(call) => call.kind(),
        }
    }

    /// The file it reads, writes or edits.
    pub fn file(&self) -> Option<PathBuf> {
        match self {
            Self::Named { name, input } => tools::file_of(name, input),
            Self::Acp(call) => call.file().map(Path::to_path_buf),
        }
    }

    /// Whether its card opens as it comes: one that changes files shows the change.
    pub fn opens_expanded(&self) -> bool {
        match self {
            Self::Named { name, .. } => name.opens_expanded(),
            Self::Acp(call) => !call.diffs.is_empty(),
        }
    }
}

impl AcpCall {
    /// The file it changes, or else the first it works on.
    pub fn file(&self) -> Option<&Path> {
        self.diffs
            .first()
            .map(|diff| diff.path.as_path())
            .or_else(|| self.locations.first().map(PathBuf::as_path))
    }

    /// ACP's kinds, as the window sums calls up; a change that only makes new files writes.
    pub fn kind(&self) -> ToolKind {
        let changes = || {
            if !self.diffs.is_empty() && self.diffs.iter().all(|diff| diff.old_text.is_none()) {
                ToolKind::Write
            } else {
                ToolKind::Edit
            }
        };
        match self.kind {
            Some(AcpKind::Read) => ToolKind::Read,
            Some(AcpKind::Edit | AcpKind::Delete | AcpKind::Move) => changes(),
            Some(AcpKind::Search) => ToolKind::Search,
            Some(AcpKind::Execute) => ToolKind::Command,
            Some(AcpKind::Fetch) => ToolKind::Web,
            Some(AcpKind::Think | AcpKind::Other) | None if !self.diffs.is_empty() => changes(),
            Some(AcpKind::Think | AcpKind::Other) | None => ToolKind::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(kind: Option<AcpKind>, diffs: Vec<Diff>) -> AcpCall {
        AcpCall {
            name: None,
            title: "a call".to_owned(),
            kind,
            locations: Vec::new(),
            diffs,
            input: None,
            status: AcpStatus::InProgress,
            output: None,
        }
    }

    fn diff(old_text: Option<&str>) -> Diff {
        Diff {
            path: PathBuf::from("/work/notes.txt"),
            old_text: old_text.map(str::to_owned),
            new_text: "new".to_owned(),
        }
    }

    #[test]
    fn a_call_is_summed_up_by_its_kind_and_what_it_changes() {
        assert_eq!(
            call(Some(AcpKind::Execute), Vec::new()).kind(),
            ToolKind::Command
        );
        assert_eq!(
            call(Some(AcpKind::Edit), vec![diff(Some("old"))]).kind(),
            ToolKind::Edit
        );
        assert_eq!(
            call(Some(AcpKind::Edit), vec![diff(None)]).kind(),
            ToolKind::Write
        );
        assert_eq!(call(None, vec![diff(None)]).kind(), ToolKind::Write);
        assert_eq!(
            call(Some(AcpKind::Think), Vec::new()).kind(),
            ToolKind::Other
        );
    }
}
