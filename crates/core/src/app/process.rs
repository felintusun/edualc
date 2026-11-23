//! The program's process: starting it, signing in, stopping and restarting it, and what is
//! written to it.

use crate::agent::{self, Agent};
use crate::external::wire::ControlRequest;
use crate::ipc::{Act, Hosted, Runs, Target, ToDaemon};
use crate::link;
use crate::present::Frontend;
use crate::provider::{FolderConfiguration, Launch, Resume};
use crate::sessions::LiveSession;
use crate::trust;

use super::{App, Doing, INITIALIZE, LimitWait, Location, Refresh, Request, SentPrompt, TrustGate};

/// What is known of Claude Code's sign-in. Behind a pipe it offers no `/login`, and a session
/// signed in with a token handed over names no account, so this comes from how turns end.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SignIn {
    /// Nothing has told yet.
    #[default]
    Unknown,
    /// It named an account, or a model answered.
    SignedIn,
    /// A turn failed for want of a sign-in; the prompt it failed on goes again with "Try again".
    Refused { failed: Option<SentPrompt> },
    /// The user asked with `/login`, and is shown how, whatever the state before.
    Asked,
}

impl SignIn {
    /// Whether the user is being shown how to sign in.
    pub fn waits_for_user(&self) -> bool {
        matches!(self, Self::Refused { .. } | Self::Asked)
    }
}

impl<F: Frontend> App<F> {
    /// Starts a process in the working directory once it is trusted: here, or in the terminal,
    /// which the daemon is asked about.
    pub(super) fn start(&mut self, resume: Option<Resume>) {
        let Location::Session(cwd) = &self.location else {
            return;
        };
        let cwd = cwd.clone();
        // What a folder configures is for providers that read folder trust (Claude Code); others
        // read none of it.
        let checks_folder_trust = self.status.capabilities().folder_trust;
        if !checks_folder_trust
            || trust::trusted_in_edualc(&cwd, &self.settings)
            || self.accepted_in_terminal.contains(&cwd)
        {
            // Trusted here: is it for Claude Code too, unless you said not to?
            if checks_folder_trust
                && trust::trusted_in_edualc(&cwd, &self.settings)
                && !self.settings.not_for_claude_code.contains(&cwd)
            {
                self.check_claude_trust(cwd);
            }
            self.launch(resume, FolderConfiguration::Read);
            return;
        }
        self.last_act = self.last_act.next();
        let request = self.last_act;
        let check = ToDaemon::Act {
            request,
            act: Act::CheckTrust { folder: cwd },
        };
        self.trust_gate = if link::send(&self.link, check).is_ok() {
            TrustGate::Checking {
                request,
                resume,
                held: None,
            }
        } else {
            TrustGate::Asking {
                resume,
                runs: vec![Runs::Unreadable {
                    file: "its settings".to_owned(),
                    reason: "the edualc daemon couldn't be asked".to_owned(),
                }],
            }
        };
    }

    /// Starts a process in the working directory: trusted, or without the folder's configuration.
    pub(super) fn launch(
        &mut self,
        resume: Option<Resume>,
        folder_configuration: FolderConfiguration,
    ) {
        let Location::Session(cwd) = &self.location else {
            return;
        };
        let cwd = cwd.clone();
        self.trust_gate = TrustGate::Open;
        self.generation = self.generation.next();
        self.log.clear();
        if self.status.worktree.is_none() {
            self.read_linked_worktree(cwd.clone(), false);
        }
        let launch = Launch {
            backend: self.status.backend,
            cwd,
            conversation: Resume::conversation(resume),
            model: self.status.chosen_model().map(str::to_owned),
            folder_configuration,
            sign_in: self.status.sign_in_with,
        };
        match Agent::start(&self.link, self.generation, launch) {
            Ok(process) => self.process = Some(process),
            Err(error) => {
                self.chat.error(error.to_string());
                return;
            }
        }
        self.request(INITIALIZE, Request::Initialize);
        if let Some(mode) = self.status.mode_choice.clone() {
            self.request(mode.request(), Request::Mode(mode));
        }
    }

    /// Ends the process, if any, and drops a start held back for trust: it was the session's
    /// being left, perhaps in another folder.
    pub(super) fn stop(&mut self) {
        self.process = None;
        self.trust_gate = TrustGate::Open;
        self.restart_waits = false;
        self.forget_process();
    }

    /// "Try again": Claude Code reads the sign-in as it starts, so it starts again, and what
    /// failed goes again, leaving anything written in the composer since. With something under
    /// way that a start would cut short, it waits for that, and what failed goes back to the
    /// user to send.
    pub(super) fn try_sign_in_again(&mut self) {
        let failed = match std::mem::take(&mut self.status.sign_in) {
            SignIn::Refused { failed } => failed,
            SignIn::Asked => None,
            other @ (SignIn::Unknown | SignIn::SignedIn) => {
                self.status.sign_in = other;
                return;
            }
        };
        if self.under_way() {
            self.restart_waits = true;
            if let Some(failed) = failed {
                self.attached = failed.images;
                self.view.render_attached(&self.attached);
                self.put_draft(&failed.text);
            }
            return;
        }
        self.stop();
        let Some(failed) = failed else {
            self.ensure_process();
            return;
        };
        let written = self.view.draft();
        self.attached = failed.images;
        self.view.render_attached(&self.attached);
        self.send(&failed.text);
        if !written.trim().is_empty() {
            self.put_draft(&written);
        }
        self.view.focus_composer();
    }

    /// Starts the process again under the settings as they are now, continuing the session.
    pub(super) fn restart(&mut self) {
        let resume = self.status.session_id.clone().map(Resume::Continue);
        self.stop();
        self.start(resume);
    }

    /// Drops what the process had under way, as it is gone: nothing it still says is taken,
    /// and nothing waits for its answers.
    pub(super) fn forget_process(&mut self) {
        self.generation = self.generation.next();
        self.asks.clear();
        self.requests.clear();
        self.status.turns = 0;
        self.status.stop_asked = false;
        self.status.doing = Doing::Nothing;
        // They ran in the process.
        self.status.tasks.clear();
        self.status.tasks_left_running = false;
        self.redraw.tasks = true;
        self.chat.stop_shells();
        self.usage.clear_waiting();
        self.side_lost();
        self.redraw.prompt = true;
        self.redraw.usage = true;
    }

    /// Starts a process for the session shown, unless one runs, the session runs elsewhere or
    /// a start waits for the folder's trust. Whether one runs now.
    pub(super) fn ensure_process(&mut self) -> bool {
        if self.process.is_none()
            && self.watch.is_none()
            && matches!(self.trust_gate, TrustGate::Open)
        {
            let resume = self.status.session_id.clone().map(Resume::Continue);
            self.start(resume);
        }
        self.process.is_some()
    }

    /// Whether the session shown has something under way that ending its process would cut
    /// short: a turn (from its prompt being written on), a `!` command, a request waiting for
    /// the user, or a task running in the background (a subagent, a shell, a monitor), which a
    /// stopped turn leaves running.
    pub(super) fn under_way(&self) -> bool {
        self.status.in_turn()
            || self.chat.shell_running()
            || !self.asks.is_empty()
            || self
                .status
                .tasks
                .running()
                .any(|running| !running.task.ambient)
    }

    /// Writes to the process; says so in the transcript if that fails.
    pub(super) fn write(&mut self, action: impl FnOnce(&mut Agent) -> Result<(), agent::Error>) {
        let Some(process) = self.process.as_mut() else {
            return;
        };
        if let Err(error) = action(process) {
            tracing::warn!(%error, backend = %self.status.backend.program(), "lost session process");
            self.chat
                .error(format!("Lost {}: {error}", self.status.backend.program()));
        }
    }

    /// Sends a control request, remembering what it was for. Whether it went out.
    pub(super) fn request(&mut self, request: ControlRequest, purpose: Request) -> bool {
        let Some(process) = self.process.as_mut() else {
            return false;
        };
        match process.request(request) {
            Ok(id) => {
                self.requests.insert(id, purpose);
                true
            }
            Err(error) => {
                tracing::warn!(%error, "couldn't send a control request");
                false
            }
        }
    }

    /// Stops Claude's turn, and the `!` command running: an interrupt stops both.
    pub(super) fn interrupt(&mut self) {
        if self.status.limit_wait != LimitWait::Idle && self.status.turns == 0 {
            self.cancel_limit_wait();
            return;
        }
        if self.status.turns > 0 || self.chat.shell_running() {
            self.status.stop_asked = self.status.turns > 0;
            self.request(ControlRequest::Interrupt, Request::Interrupt);
        }
    }

    /// The line to edualcd is back: what was watched is watched again, and the session shown
    /// is opened again, from its transcript, which has what was said meanwhile. One this window
    /// ran was let go of as the window came again (see the daemon's hub), and is attached to.
    pub(super) fn reconnected(&mut self, hosted: Vec<Hosted>, live: Vec<LiveSession>) {
        self.view.set_running_sessions(hosted.len());
        self.hosted = hosted;
        self.live = live;
        // A listing asked on the lost line, or while it was lost (dropped unsent), is answered
        // never: waiting on it, the running sessions would be listed no more.
        self.listing_live = Refresh::Idle;
        self.redraw.sessions = true;
        self.records = self.watch(Target::Records);
        match self.status.session_id.take() {
            Some(id) => {
                self.watch = None;
                self.open_session(id);
            }
            // A new session not started yet starts again; a page (the projects, a project's
            // sessions) stays the page, not a session begun in its folder.
            None if matches!(self.location, Location::Session(_)) => {
                self.new_session(self.status.backend, self.status.sign_in_with);
            }
            None => {}
        }
        self.load_sessions();
        self.render();
    }
}
