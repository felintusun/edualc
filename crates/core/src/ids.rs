//! The ids edualc gives every program's sessions, calls, messages and requests in the same terms,
//! whichever program they come from.

use serde::{Deserialize, Serialize};

/// A session, as Claude Code names its transcript (`<id>.jsonl`) and takes it in `--resume`: a
/// UUID. Ids come from file names and records other processes write, and end up on `claude`'s
/// command line and in paths, so anything else (a file named `--settings=….jsonl`, a `../`) is
/// refused where it enters.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize, derive_more::Display,
)]
#[serde(try_from = "String")]
pub struct SessionId(String);

#[derive(Debug, thiserror::Error)]
#[error("{0:?} isn't a session id")]
pub struct NotASessionId(String);

impl TryFrom<String> for SessionId {
    type Error = NotASessionId;

    fn try_from(id: String) -> Result<Self, Self::Error> {
        // Of the forms uuid parses, only the hyphenated one is 36 characters long.
        if id.len() == 36 && uuid::Uuid::try_parse(&id).is_ok() {
            Ok(Self(id))
        } else {
            Err(NotASessionId(id))
        }
    }
}

impl From<uuid::Uuid> for SessionId {
    fn from(id: uuid::Uuid) -> Self {
        Self(id.hyphenated().to_string())
    }
}

/// The namespace of the name-based ids edualc gives sessions whose program's own id isn't a
/// UUID. Changing it changes those sessions' ids, and their Changes snapshots are lost.
const EXTERNAL_SESSIONS: uuid::Uuid = uuid::uuid!("cc254ad2-efe7-47f0-a854-d03ab1b39064");

impl SessionId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Makes a fresh id for a session started or forked here. Claude Code takes it with
    /// `--session-id`, so that it is known before the session's first prompt.
    pub fn new_random() -> Self {
        Self::from(uuid::Uuid::new_v4())
    }

    /// Returns the id edualc keeps for a session `backend` names `raw_id`: the id itself where
    /// it is a UUID (Claude Code's, agy's), else a name-based UUID made from it (opencode's).
    pub fn for_external(backend: crate::provider::Backend, raw_id: &str) -> Self {
        if let Ok(uuid) = Self::try_from(raw_id.to_owned()) {
            return uuid;
        }
        let name = format!("{}:{raw_id}", <&'static str>::from(backend));
        Self::from(uuid::Uuid::new_v5(&EXTERNAL_SESSIONS, name.as_bytes()))
    }
}

/// A tool call, which permission requests and results refer back to.
#[derive(
    Debug,
    Clone,
    Default,
    PartialEq,
    Eq,
    Hash,
    Deserialize,
    Serialize,
    derive_more::From,
    derive_more::Display,
)]
#[serde(transparent)]
pub struct ToolUseId(String);

/// An API message, whose stream events and complete copy arrive separately.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Deserialize, Serialize, derive_more::From)]
#[serde(transparent)]
pub struct MessageId(String);

/// A control request, either way: its answer names it.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Hash,
    Deserialize,
    Serialize,
    derive_more::From,
    derive_more::Display,
)]
#[serde(transparent)]
pub struct RequestId(String);
