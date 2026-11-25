//! The agent programs (`Backend`) and what each supports (`Capabilities`).

use std::path::PathBuf;

use crate::ids::SessionId;

/// What a program does beyond a turn of prompt and reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Keeps and lists past sessions, so you can read and continue them.
    pub transcripts: bool,
    /// Forks a session into a copy of its own.
    pub fork: bool,
    /// Answers side questions (`/btw`) beside the conversation.
    pub side_questions: bool,
    /// Runs `!` commands beside the conversation.
    pub inline_shell: bool,
    /// Reports the plan's five-hour and weekly limits.
    pub plan_limits: bool,
    /// Reads the folder's own settings, hooks and MCP servers, so the folder is trusted first.
    pub folder_trust: bool,
    /// Signs in with an Anthropic API key from the keyring, as well as in the terminal.
    pub api_key_sign_in: bool,
    /// Signs in by a command you run in a terminal, which `/login` shows you. Behind a pipe the
    /// program offers no sign-in of its own.
    pub terminal_sign_in: bool,
    /// Takes comments on a plan's blocks along with its approval.
    pub plan_comments: bool,
    /// Has a fixed set of permission modes for the mode menu, rather than modes the program
    /// offers as it starts.
    pub fixed_modes: bool,
    /// Applies a mode picked during a turn from the next prompt on.
    pub deferred_mode_switch: bool,
}

/// The program a session runs: Claude Code, Google's Antigravity CLI, or opencode. edualcd's
/// ACP adapter runs the last two.
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
    /// Every program edualc runs, in tab order.
    pub const ALL: [Self; 3] = [Self::Claude, Self::Agy, Self::Opencode];

    /// Returns who answers and asks, as the window names it.
    pub fn agent(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Agy => "agy",
            Self::Opencode => "opencode",
        }
    }

    /// Returns the program's name.
    pub fn program(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Agy => "agy",
            Self::Opencode => "opencode",
        }
    }

    /// Returns whether edualcd's ACP adapter runs it.
    pub fn is_acp(self) -> bool {
        match self {
            Self::Claude => false,
            Self::Agy | Self::Opencode => true,
        }
    }

    /// Returns what the program does, signed in with `sign_in`.
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
                terminal_sign_in: true,
                plan_comments: true,
                fixed_modes: true,
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
                terminal_sign_in: false,
                plan_comments: false,
                fixed_modes: false,
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
                terminal_sign_in: false,
                plan_comments: false,
                fixed_modes: false,
                deferred_mode_switch: true,
            },
        }
    }

    /// Returns whether its sessions can be forked (`--fork-session`, `session/fork`).
    pub fn can_fork(self) -> bool {
        self.capabilities(SignInWith::Account).fork
    }

    /// Returns what the window shows when sign-in is refused: what to do, and the command to
    /// run in a terminal.
    pub fn sign_in_help(self) -> (&'static str, &'static str) {
        match self {
            Self::Claude => (
                "Run this in a terminal, then try again.",
                "claude auth login",
            ),
            Self::Agy => (
                "Try again. If it keeps failing, sign in with this in a terminal.",
                "agy",
            ),
            Self::Opencode => (
                "Try again. If it keeps failing, sign in with this in a terminal.",
                "opencode",
            ),
        }
    }

    /// Returns the usage bar's and panel's words where usage limits aren't reported.
    pub fn limits_note(self, sign_in: SignInWith) -> &'static str {
        match (self, sign_in) {
            (Self::Opencode, _) => "No usage limits",
            (Self::Claude, SignInWith::ApiKey) => "API key",
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
    /// Your own sign-in, as in your terminal.
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
    /// Not trusted, and started without it: only your own settings apply, and no MCP servers
    /// from configuration at all.
    Skipped,
}

/// What a session's process is started with; the daemon starts it.
#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Launch {
    pub backend: Backend,
    pub cwd: PathBuf,
    pub conversation: Conversation,
    /// A model or alias, as the program takes it; `None` for the program's default.
    pub model: Option<String>,
    pub folder_configuration: FolderConfiguration,
    /// How Claude Code signs in; the others sign in themselves.
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
    /// Returns the conversation a process runs for `resume`, or a new one for `None`. New
    /// sessions get their ids here.
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
