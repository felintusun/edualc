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
        let dashes_at = [8, 13, 18, 23];
        let uuid = id.len() == 36
            && id.char_indices().all(|(at, character)| {
                if dashes_at.contains(&at) {
                    character == '-'
                } else {
                    character.is_ascii_hexdigit()
                }
            });
        if uuid {
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

impl SessionId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A fresh id, for a session started (or forked) here: Claude Code takes it with
    /// `--session-id`, so it is known before the session's first prompt.
    pub fn new_random() -> Self {
        Self::from(uuid::Uuid::new_v4())
    }

    /// The stable window id for an external session whose own backend id is `raw_id`: kept as
    /// itself when already a UUID (Claude, agy), or mapped deterministically to a UUID (opencode).
    pub fn for_external(backend: crate::provider::Backend, raw_id: &str) -> Self {
        use sha2::Digest as _;
        if let Ok(uuid) = Self::try_from(raw_id.to_owned()) {
            return uuid;
        }
        let mut hasher = sha2::Sha256::new();
        hasher.update(<&'static str>::from(backend).as_bytes());
        hasher.update(b":");
        hasher.update(raw_id.as_bytes());
        let digest = hasher.finalize();
        let mut bytes = [0_u8; 16];
        bytes.copy_from_slice(digest.get(..16).unwrap_or(&[]));
        // Set RFC 4122 version (5) and variant bits so the identifier parses as a standard UUID.
        bytes[6] = (bytes[6] & 0x0f) | 0x50;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Self::from(uuid::Uuid::from_bytes(bytes))
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
