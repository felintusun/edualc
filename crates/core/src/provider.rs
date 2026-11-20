//! Coding agent providers (`Backend`) and what each supports (`Capabilities`).
//!
//! `edualc-core` relies on these capabilities rather than assuming Claude Code's CLI
//! features or ACP's baseline alone: standard capabilities (like continuation, fork, models,
//! and modes) sit alongside provider-specific ones (such as side questions, inline shell,
//! plan limits, and pre-start folder trust).

use std::path::PathBuf;

use crate::external::wire::SessionId;

/// What a coding agent provider supports beyond a basic prompt-and-reply turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Persists and lists past sessions so you can read and continue them.
    pub transcripts: bool,
    /// Supports cloning a session into an independent copy.
    pub fork: bool,
    /// Supports ephemeral side questions (`/btw`) beside the main conversation.
    pub side_questions: bool,
    /// Supports `!` inline shell execution beside the composer.
    pub inline_shell: bool,
    /// Reports five-hour and weekly plan quota limits for the signed-in account.
    pub plan_limits: bool,
    /// Reads folder-level settings/hooks/MCP configuration and needs a pre-start trust gate.
    pub folder_trust: bool,
    /// Offers an optional stored Anthropic API key alongside terminal sign-in.
    pub api_key_sign_in: bool,
    /// Supplies built-in client commands (`/btw`, `/login`) and subscription tier metadata.
    pub claude_extras: bool,
    /// Supports interactive block-level plan comments returned alongside approval.
    pub plan_comments: bool,
    /// Uses Claude Code's fixed permission-mode menu rather than dynamic agent modes.
    pub claude_modes: bool,
    /// Applies a mid-turn mode switch only from the next prompt onward.
    pub deferred_mode_switch: bool,
}

/// The program a session talks to: Claude Code, Google's Antigravity CLI, or opencode.
/// edualcd's ACP adapter speaks to the ACP backends and translates for the window.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::EnumIter,
)]
#[strum(serialize_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    #[default]
    Claude,
    Agy,
    Opencode,
}

impl Backend {
    /// Every coding agent backend edualc supports, in tab order.
    pub const ALL: [Self; 3] = [Self::Claude, Self::Agy, Self::Opencode];

    /// Who answers and asks, as the window names it.
    pub fn agent(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Agy => "agy",
            Self::Opencode => "opencode",
        }
    }

    /// The program the session runs as.
    pub fn program(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Agy => "agy",
            Self::Opencode => "opencode",
        }
    }

    /// Whether edualcd's ACP adapter runs it.
    pub fn is_acp(self) -> bool {
        match self {
            Self::Claude => false,
            Self::Agy | Self::Opencode => true,
        }
    }

    /// What this provider supports, refined by how the session signs in.
    pub fn capabilities(self, sign_in: SignInWith) -> Capabilities {
        match self {
            Self::Claude => Capabilities {
                transcripts: true,
                fork: true,
                side_questions: true,
                inline_shell: true,
                plan_limits: sign_in == SignInWith::Account,
                folder_trust: true,
                api_key_sign_in: true,
                claude_extras: true,
                plan_comments: true,
                claude_modes: true,
                deferred_mode_switch: false,
            },
            Self::Agy => Capabilities {
                transcripts: true,
                fork: false,
                side_questions: false,
                inline_shell: false,
                plan_limits: true,
                folder_trust: false,
                api_key_sign_in: false,
                claude_extras: false,
                plan_comments: false,
                claude_modes: false,
                deferred_mode_switch: true,
            },
            Self::Opencode => Capabilities {
                transcripts: true,
                fork: true,
                side_questions: false,
                inline_shell: false,
                plan_limits: false,
                folder_trust: false,
                api_key_sign_in: false,
                claude_extras: false,
                plan_comments: false,
                claude_modes: false,
                deferred_mode_switch: true,
            },
        }
    }

    /// Whether a session of this backend can be forked into a copy (`--fork-session` / ACP
    /// `session/fork`).
    pub fn can_fork(self) -> bool {
        self.capabilities(SignInWith::Account).fork
    }

    /// What the window shows when sign-in is refused: instructions and the terminal command.
    pub fn sign_in_help(self) -> (&'static str, &'static str) {
        match self {
            Self::Claude => (
                "Run this in a terminal and follow its steps, then try again. Claude Code starts \
                 again with your sign-in, and the message that failed is sent again.",
                "claude auth login",
            ),
            Self::Agy => (
                "agy renews its sign-in by itself, so trying again often works. If it keeps \
                 failing, run agy in a terminal and sign in when it asks, then try again. The \
                 message that failed is sent again.",
                "agy",
            ),
            Self::Opencode => (
                "opencode keeps its own sign-in, so trying again often works. If it keeps \
                 failing, run opencode in a terminal and sign in when it asks, then try again. \
                 The message that failed is sent again.",
                "opencode",
            ),
        }
    }

    /// Short explanation where usage limits aren't reported (for the usage bar and popover).
    pub fn limits_note(self, sign_in: SignInWith) -> &'static str {
        match (self, sign_in) {
            (Self::Opencode, _) => "opencode reports no usage limits",
            (Self::Claude, SignInWith::ApiKey) => "API key · billed by the token",
            (Self::Claude, SignInWith::Account) | (Self::Agy, _) => "Usage",
        }
    }
}

/// What a session runs, as the sidebar and the New session dialog tell them apart.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Program {
    #[default]
    Claude,
    ClaudeApiKey,
    Agy,
    Opencode,
}

impl Program {
    /// agy and opencode sign in themselves, so their sign-in doesn't matter.
    pub fn of(backend: Backend, sign_in: SignInWith) -> Self {
        match (backend, sign_in) {
            (Backend::Claude, SignInWith::Account) => Self::Claude,
            (Backend::Claude, SignInWith::ApiKey) => Self::ClaudeApiKey,
            (Backend::Agy, _) => Self::Agy,
            (Backend::Opencode, _) => Self::Opencode,
        }
    }
}

/// What a session signs in with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SignInWith {
    /// The user's own sign-in, as in their terminal.
    #[default]
    Account,
    /// The Anthropic API key in the desktop's keyring, billed by the token.
    ApiKey,
}

/// Whether the folder's own configuration (`.claude/settings*.json`, `.mcp.json`) is read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FolderConfiguration {
    /// As for a trusted folder: its hooks, environment, helpers and MCP servers apply.
    #[default]
    Read,
    /// Not trusted, and started anyway without it: only the user's own settings apply, and no
    /// MCP servers from configuration at all.
    Skipped,
}

/// What a session's process is started with; the daemon starts it.
#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Launch {
    pub backend: Backend,
    pub cwd: PathBuf,
    pub conversation: Conversation,
    /// A model or alias, as the backend takes it; the backend's default if none.
    pub model: Option<String>,
    /// Whether the folder's own configuration is read.
    pub folder_configuration: FolderConfiguration,
    /// How Claude Code signs in; ACP backends sign in themselves.
    pub sign_in: SignInWith,
}

/// A saved session for a process to go on with.
#[derive(Debug, PartialEq, Eq)]
pub enum Resume {
    /// Continues the session itself.
    Continue(SessionId),
    /// Continues a copy under a new id, as the session runs elsewhere too.
    Fork(SessionId),
}

impl Resume {
    /// The conversation a process runs for `resume` (none: a new one), the ids of new
    /// sessions chosen here.
    pub fn conversation(resume: Option<Self>) -> Conversation {
        match resume {
            None => Conversation::New(SessionId::new_random()),
            Some(Self::Continue(id)) => Conversation::Continue(id),
            Some(Self::Fork(of)) => Conversation::Fork {
                of,
                id: SessionId::new_random(),
            },
        }
    }
}

/// The conversation a process runs, and the id it runs it under.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Conversation {
    New(SessionId),
    Continue(SessionId),
    /// A copy of `of`, under `id`.
    Fork {
        of: SessionId,
        id: SessionId,
    },
}

impl Conversation {
    pub fn id(&self) -> &SessionId {
        match self {
            Self::New(id) | Self::Continue(id) | Self::Fork { id, .. } => id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_capabilities_reflect_each_backend_and_sign_in() {
        let claude_account = Backend::Claude.capabilities(SignInWith::Account);
        assert!(claude_account.fork);
        assert!(claude_account.side_questions);
        assert!(claude_account.inline_shell);
        assert!(claude_account.plan_limits);
        assert!(claude_account.folder_trust);

        let claude_key = Backend::Claude.capabilities(SignInWith::ApiKey);
        assert!(!claude_key.plan_limits);
        assert!(claude_key.fork);

        let agy = Backend::Agy.capabilities(SignInWith::Account);
        assert!(!agy.fork);
        assert!(!agy.side_questions);
        assert!(!agy.inline_shell);
        assert!(agy.plan_limits);
        assert!(!agy.folder_trust);

        let opencode = Backend::Opencode.capabilities(SignInWith::Account);
        assert!(opencode.fork);
        assert!(!opencode.plan_limits);
        assert!(!opencode.side_questions);
    }
}
