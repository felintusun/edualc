//! Whether a folder is trusted before a session starts in it, and Claude Code's own trust of
//! it.

use std::path::{Path, PathBuf};

use crate::ipc::{Act, ActFailure, ActId, Runs, ToDaemon};
use crate::link;
use crate::present::Frontend;
use crate::provider::{FolderConfiguration, Resume};
use crate::settings;

use super::{Acting, App};

/// Where a start stands on its folder's trust.
#[derive(Debug, Default)]
pub(super) enum TrustGate {
    /// Nothing is held back.
    #[default]
    Open,
    /// The daemon reads whether the terminal's dialog trusted the folder (the window can't,
    /// see `trust`); the start, with what it was to resume, waits for its answer, and so does
    /// a prompt sent meanwhile (the command line's first, or an early Enter).
    Checking {
        request: ActId,
        resume: Option<Resume>,
        held: Option<String>,
    },
    /// The user is asked to trust the folder, shown what its configuration would run; the
    /// start waits for them.
    Asking {
        resume: Option<Resume>,
        runs: Vec<Runs>,
    },
}

/// Whether a folder trusted here is trusted for Claude Code too, which applies the folder's own
/// permission rules only then (see `Act::TrustForClaudeCode`). Asked once, of a folder trusted
/// before trusting it here also trusted it for Claude Code.
#[derive(Debug, Default)]
pub(super) enum ClaudeTrustAsk {
    #[default]
    Not,
    /// The daemon reads whether Claude Code trusts the folder.
    Checking { request: ActId, folder: PathBuf },
    /// It doesn't: a bar asks.
    Asking(PathBuf),
}

/// Where the daemon's answer on the folder's trust leaves a start held back.
#[derive(Debug, PartialEq)]
pub(super) enum TrustAnswer {
    /// Trusted in the terminal: the start goes ahead, and then the prompt held for it.
    Trusted {
        resume: Option<Resume>,
        held: Option<String>,
    },
    /// The user is asked now; a prompt held waits in the composer for them to send again.
    Asked,
    /// An answer for a start since dropped or replaced: nothing changes.
    Stale,
}

impl TrustGate {
    /// A prompt sent before the start: held to go once the folder is found trusted, if its
    /// trust is still being read; the user, once asked, sends it themselves.
    pub(super) fn hold(&mut self, prompt: &str) {
        if let Self::Checking { held, .. } = self {
            *held = Some(prompt.to_owned());
        }
    }

    /// The daemon's answer to `request`: the gate opens, asks the user, or stays as it is.
    pub(super) fn answered(
        &mut self,
        request: ActId,
        trust: crate::ipc::FolderTrust,
    ) -> TrustAnswer {
        match std::mem::take(self) {
            Self::Checking {
                request: asked,
                resume,
                held,
            } if asked == request => match trust {
                crate::ipc::FolderTrust::Accepted => TrustAnswer::Trusted { resume, held },
                crate::ipc::FolderTrust::Untrusted(runs) => {
                    *self = Self::Asking { resume, runs };
                    TrustAnswer::Asked
                }
            },
            kept @ (Self::Open | Self::Checking { .. } | Self::Asking { .. }) => {
                *self = kept;
                TrustAnswer::Stale
            }
        }
    }
}

impl<F: Frontend> App<F> {
    /// The daemon's answer on the folder's trust: the start held back goes ahead, with a prompt
    /// sent meanwhile, or the user is asked, shown what the folder would run. An answer for a
    /// start since dropped or replaced is no longer waited for.
    pub(super) fn trust_read(&mut self, request: ActId, trust: crate::ipc::FolderTrust) {
        match self.trust_gate.answered(request, trust) {
            TrustAnswer::Trusted { resume, held } => {
                if let Some(cwd) = self.location.cwd() {
                    self.accepted_in_terminal.insert(cwd.to_path_buf());
                }
                self.launch(resume, FolderConfiguration::Read);
                if let Some(prompt) = held {
                    self.send(&prompt);
                }
            }
            TrustAnswer::Asked | TrustAnswer::Stale => {}
        }
    }

    /// Trusts the folder shown and starts what waited for it.
    pub(super) fn trust_folder(&mut self) {
        let Some(cwd) = self.location.cwd().map(Path::to_path_buf) else {
            return;
        };
        let resume = match std::mem::take(&mut self.trust_gate) {
            TrustGate::Asking { resume, .. } => resume,
            kept @ (TrustGate::Open | TrustGate::Checking { .. }) => {
                self.trust_gate = kept;
                return;
            }
        };
        // As the card says: trusted for Claude Code too, so its permission rules apply.
        self.ask_daemon(
            Act::TrustForClaudeCode {
                folder: cwd.clone(),
            },
            Acting::Desktop,
        );
        self.change_settings(settings::Change::Trust(cwd));
        self.launch(resume, FolderConfiguration::Read);
        self.view.focus_composer();
    }

    /// Asks the daemon whether Claude Code trusts `folder`, to ask you if it doesn't.
    pub(super) fn check_claude_trust(&mut self, folder: PathBuf) {
        self.last_act = self.last_act.next();
        let request = self.last_act;
        let check = ToDaemon::Act {
            request,
            act: Act::CheckClaudeTrust {
                folder: folder.clone(),
            },
        };
        self.claude_trust_ask = if link::send(&self.link, check).is_ok() {
            ClaudeTrustAsk::Checking { request, folder }
        } else {
            ClaudeTrustAsk::Not
        };
    }

    /// The daemon's answer: a folder Claude Code doesn't trust is asked about, if it is still
    /// the one shown. One whose trust couldn't be read isn't: the bar would ask on every start.
    pub(super) fn claude_trust_read(&mut self, request: ActId, trusted: Result<bool, ActFailure>) {
        let ClaudeTrustAsk::Checking {
            request: asked,
            folder,
        } = std::mem::take(&mut self.claude_trust_ask)
        else {
            return;
        };
        if asked != request {
            return;
        }
        self.claude_trust_ask = match trusted {
            Ok(false) if self.location.cwd() == Some(folder.as_path()) => {
                ClaudeTrustAsk::Asking(folder)
            }
            Ok(_) => ClaudeTrustAsk::Not,
            Err(failure) => {
                tracing::warn!(%failure, "couldn't tell whether Claude Code trusts the folder");
                ClaudeTrustAsk::Not
            }
        };
    }

    pub(super) fn trust_for_claude_code(&mut self) {
        if let ClaudeTrustAsk::Asking(folder) = std::mem::take(&mut self.claude_trust_ask) {
            self.ask_daemon(Act::TrustForClaudeCode { folder }, Acting::Desktop);
        }
    }

    pub(super) fn not_for_claude_code(&mut self) {
        if let ClaudeTrustAsk::Asking(folder) = std::mem::take(&mut self.claude_trust_ask) {
            self.change_settings(settings::Change::NotForClaudeCode(folder));
        }
    }

    /// Starts what waited for the folder's trust without its configuration, this once: the
    /// next start asks again.
    pub(super) fn start_without_folder_settings(&mut self) {
        let resume = match std::mem::take(&mut self.trust_gate) {
            TrustGate::Asking { resume, .. } => resume,
            kept @ (TrustGate::Open | TrustGate::Checking { .. }) => {
                self.trust_gate = kept;
                return;
            }
        };
        self.chat.info(
            "Started without this folder's settings: its hooks, permissions, environment and \
             MCP servers don't apply, and neither do your own MCP servers or claude.ai \
             connectors.",
        );
        self.launch(resume, FolderConfiguration::Skipped);
        self.view.focus_composer();
    }

    /// A prompt sent before the process could start: shown in the composer, and sent once the
    /// folder is found trusted in the terminal (see [`TrustGate::hold`]).
    pub(super) fn wait_for_trust(&mut self, text: &str) {
        self.put_draft(text);
        self.trust_gate.hold(text);
    }
}
