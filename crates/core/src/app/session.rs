//! A session: opening it, attaching to it, continuing it, and letting it go.

use std::path::{Path, PathBuf};

use crate::agent::Agent;
use crate::chat::Chat;
use crate::external::wire::TranscriptLine;
use crate::ids::SessionId;
use crate::ipc::{self, Act, Hosted, Read, Target};
use crate::present::Frontend;
use crate::provider::{self, Resume};
use crate::sessions;

use super::{
    Acting, ApiKeyKept, App, ClaudeTrustAsk, INITIALIZE, LimitWait, Location, Modes, Reading,
    Recall, Redraw, Refresh, Request, SideChat, SignIn, Watch,
};

impl<F: Frontend> App<F> {
    /// Lets go of the process. One the user used, or with something under way, runs on in the
    /// daemon, for this window or another to come back to (it is listed as running). One only
    /// opened to read ends, as its transcript resumes it as it was.
    pub(super) fn let_go(&mut self) {
        let under_way = self.under_way();
        if let Some(process) = self.process.take() {
            if under_way || process.used() {
                if let Some(id) = &self.status.session_id {
                    for item in &mut self.hosted {
                        if &item.session == id {
                            item.attached = false;
                        }
                    }
                }
                process.detach();
            } else {
                let pid = process.pid();
                if let Some(id) = &self.status.session_id {
                    self.hosted.retain(|item| &item.session != id);
                }
                if let Some(pid) = pid {
                    self.live.retain(|running| running.pid != pid);
                }
            }
        }
    }

    /// Leaves the session shown for another, letting go of its process.
    pub(super) fn leave(&mut self) {
        self.view.clear_selection();
        self.close_peek();
        self.let_go();
        self.stop();
        // A process let go of runs on, listed by its record, which the list read while it was
        // this window's left out; nothing else would read it again for up to a minute.
        self.list_live();
    }

    pub(super) fn new_session(
        &mut self,
        backend: provider::Backend,
        sign_in: provider::SignInWith,
    ) {
        let Some(cwd) = self.location.cwd().map(Path::to_path_buf) else {
            return;
        };
        self.leave();
        self.watch = None;
        self.chat.clear();
        self.location = Location::Session(cwd);
        self.reset_session(None, None, backend);
        // agy and opencode sign in themselves.
        self.status.sign_in_with = if !backend.capabilities(sign_in).api_key_sign_in {
            provider::SignInWith::Account
        } else {
            sign_in
        };
        self.start(None);
        self.redraw = Redraw::everything();
        self.view.focus_composer();
    }

    pub(super) fn reset_session(
        &mut self,
        id: Option<SessionId>,
        title: Option<String>,
        backend: provider::Backend,
    ) {
        self.status.session_id = id;
        self.status.title = title;
        self.status.backend = backend;
        // The menu offers the program's own modes at once, not only once its process says.
        self.status.modes = Modes::of(backend);
        self.status.mode_choice = None;
        // A wait for the limit belongs to the session whose turn it stopped.
        self.status.limit_wait = LimitWait::Idle;
        // So does its sign-in: one running elsewhere is taken to be on the user's own.
        self.status.sign_in_with = provider::SignInWith::Account;
        // A session that was to be continued once its sign-in was picked isn't, once another is
        // shown.
        self.choosing_sign_in = None;
        self.status.context_tokens = 0;
        self.status.context_window = 0;
        self.status.cost_usd = 0.0;
        self.status.worktree = None;
        // What it changed, and comments on that, belong to the session shown before; so does a
        // question about its folder's trust.
        self.status.turn_changed = None;
        self.changes = None;
        self.claude_trust_ask = ClaudeTrustAsk::Not;
        self.line_comments.clear();
        self.open_runs.clear();
        self.shell_rows.clear();
        // What failed belongs to the session it was sent in.
        self.last_prompt = None;
        if let SignIn::Refused { failed } = &mut self.status.sign_in {
            *failed = None;
        }
        // Side questions belong to the session they were asked in.
        self.side = SideChat::default();
        self.redraw.side = true;
        self.recall = Recall::Writing;
        self.show_recall();
    }

    /// Shows a saved session and continues it; one running elsewhere is only watched.
    pub(super) fn open_session(&mut self, id: SessionId) {
        if matches!(&self.location, Location::Session(_))
            && self.status.session_id.as_ref() == Some(&id)
            && self.watch.is_none()
        {
            return;
        }
        if let Some(hosted) = self
            .hosted
            .iter()
            .find(|hosted| hosted.session == id && !hosted.attached)
            .cloned()
        {
            self.attach(&hosted);
            return;
        }
        if self.live.iter().any(|session| session.id == id) {
            self.watch_live(&id);
            return;
        }
        let backend = self.backend_of(&id);
        // With a key kept, you pick what a provider supporting API keys continues with; switch to
        // the session view right away so `SignInChoiceBar` is shown immediately instead of staying
        // hidden on the project page.
        if backend
            .capabilities(provider::SignInWith::Account)
            .api_key_sign_in
            && self.api_key == ApiKeyKept::Kept
        {
            let Some(cwd) = self.location.cwd().map(Path::to_path_buf) else {
                return;
            };
            self.leave();
            self.watch = None;
            self.chat.clear();
            self.location = Location::Session(cwd.clone());
            let known = self.sessions.iter().find(|session| session.id == id);
            let title = known.map(|session| session.title.clone());
            let worktree = known.and_then(|session| session.worktree.clone());
            self.reset_session(Some(id.clone()), title, backend);
            self.status.worktree = worktree;
            self.choosing_sign_in = Some(id.clone());
            self.load_session(id, cwd);
            self.redraw = Redraw::everything();
            return;
        }
        self.continue_session(id, backend, provider::SignInWith::Account);
    }

    /// The program session `id` ran, as the list, running records, or its transcript says.
    pub(super) fn backend_of(&self, id: &SessionId) -> provider::Backend {
        self.sessions
            .iter()
            .find(|session| &session.id == id)
            .map(|session| session.backend)
            .or_else(|| {
                self.live
                    .iter()
                    .find(|session| &session.id == id)
                    .map(|session| session.backend)
            })
            .or_else(|| {
                self.hosted
                    .iter()
                    .find(|hosted| &hosted.session == id)
                    .map(|hosted| hosted.backend)
            })
            .or_else(|| {
                self.location
                    .cwd()
                    .and_then(|cwd| sessions::find_transcript(cwd, id))
                    .map(|path| sessions::backend_of(&path))
            })
            .or_else(|| sessions::is_agy_brain_session(id).then_some(provider::Backend::Agy))
            .unwrap_or_else(|| {
                self.location.cwd().map_or(self.status.backend, |cwd| {
                    sessions::backend_of(&sessions::locate(cwd, id))
                })
            })
    }

    /// The session waiting for the user's pick goes on signed in so; or isn't continued.
    pub(super) fn continue_with(&mut self, sign_in: Option<provider::SignInWith>) {
        let Some(id) = self.choosing_sign_in.take() else {
            return;
        };
        if let Some(sign_in) = sign_in {
            let backend = self.backend_of(&id);
            self.continue_session(id, backend, sign_in);
        }
    }

    /// Continues session `id`, which no process runs, in this window.
    pub(super) fn continue_session(
        &mut self,
        id: SessionId,
        backend: provider::Backend,
        sign_in: provider::SignInWith,
    ) {
        let Some(cwd) = self.location.cwd().map(Path::to_path_buf) else {
            return;
        };
        self.leave();
        self.watch = None;
        self.chat.clear();
        self.location = Location::Session(cwd.clone());
        let known = self.sessions.iter().find(|session| session.id == id);
        let title = known.map(|session| session.title.clone());
        let worktree = known.and_then(|session| session.worktree.clone());
        self.reset_session(Some(id.clone()), title, backend);
        self.status.worktree = worktree;
        self.status.sign_in_with = sign_in;
        self.load_session(id.clone(), cwd);
        self.start(Some(Resume::Continue(id)));
        self.redraw = Redraw::everything();
        self.view.focus_composer();
    }

    /// Shows a session the daemon runs, which no window shows: its transcript as saved, if it
    /// has one yet, then what it asked meanwhile and what it says.
    pub(super) fn attach(&mut self, hosted: &Hosted) {
        let id = &hosted.session;
        for item in &mut self.hosted {
            if item.id == hosted.id {
                item.attached = true;
            }
        }
        self.leave();
        self.watch = None;
        self.chat.clear();
        if self.location.cwd() != Some(hosted.cwd.as_path()) {
            self.switch_project_sessions(&hosted.cwd);
            self.location = Location::Session(hosted.cwd.clone());
            self.load_sessions();
        } else {
            self.location = Location::Session(hosted.cwd.clone());
        }
        let title = self
            .sessions
            .iter()
            .find(|session| &session.id == id)
            .map(|session| session.title.clone());
        let backend = hosted.backend;
        self.reset_session(Some(id.clone()), title, backend);
        self.status.sign_in_with = hosted.sign_in;
        self.load_session(id.clone(), hosted.cwd.clone());
        self.generation = self.generation.next();
        match Agent::attach(&self.link, self.generation, hosted.id) {
            Ok(process) => self.process = Some(process),
            Err(error) => {
                self.chat.error(error.to_string());
                return;
            }
        }
        // What the process offers, again: the window that started it asked before.
        self.request(INITIALIZE, Request::Initialize);
        self.redraw = Redraw::everything();
        self.view.focus_composer();
    }

    /// Loads a session's transcript in the background, looking for it: a worktree the session
    /// entered may have moved it.
    pub(super) fn load_session(&mut self, id: SessionId, cwd: PathBuf) {
        let place = ipc::Place::Session {
            cwd,
            id: id.clone(),
        };
        self.read(Read::Transcript { place, from: 0 }, Reading::Session { id });
    }

    pub(super) fn session_loaded(
        &mut self,
        id: &SessionId,
        path: PathBuf,
        result: Result<(Chat, u64), ipc::Unread>,
    ) {
        // Another session may have been opened meanwhile.
        if self.status.session_id.as_ref() != Some(id) {
            return;
        }
        // Watched, or until it is written, the folder it will be in.
        let follows = self
            .watch
            .as_ref()
            .is_some_and(|watch| &watch.session.id == id);
        let watched = if follows {
            self.watch(Target::Transcript(ipc::Place::Path(path.clone())))
        } else {
            None
        };
        let watching = self.watch.as_mut().filter(|watch| &watch.session.id == id);
        match result {
            Ok((chat, offset)) => {
                self.chat = chat;
                self.view.open_at_end();
                if let Some(watch) = watching {
                    watch.watched = watched;
                    watch.place = ipc::Place::Path(path);
                    watch.offset = offset;
                    if watch.reading.done() {
                        self.read_on();
                    }
                }
            }
            Err(unread) => {
                // One not written yet is watched for, and read once it is.
                if let Some(watch) = watching {
                    watch.watched = watched;
                    watch.place = ipc::Place::Path(path);
                    watch.reading = Refresh::Idle;
                }
                match unread {
                    ipc::Unread::NotWritten => {}
                    ipc::Unread::Failed(error) => {
                        self.chat
                            .error(format!("Couldn't read the session: {error}"));
                    }
                }
            }
        }
        self.show_recall();
        if let Some(prompt) = self.held_prompt.take() {
            self.send(&prompt);
        }
    }

    pub(super) fn transcript_grew(
        &mut self,
        id: &SessionId,
        result: Result<(Vec<TranscriptLine>, u64), ipc::Unread>,
    ) {
        let Some(watch) = self.watch.as_mut().filter(|watch| &watch.session.id == id) else {
            return;
        };
        let again = watch.reading.done();
        match result {
            Ok((lines, offset)) => {
                watch.offset = offset;
                self.chat.replay(&lines);
            }
            Err(error) => tracing::warn!(%error, "couldn't read on in a watched session"),
        }
        if again {
            self.read_on();
        }
    }

    /// Shows a session another Claude Code process runs, following it as it goes.
    pub(super) fn watch_live(&mut self, id: &SessionId) {
        let Some(session) = self.live.iter().find(|session| &session.id == id).cloned() else {
            return;
        };
        self.leave();
        self.chat.clear();
        if self.location.cwd() != Some(session.cwd.as_path()) {
            self.switch_project_sessions(&session.cwd);
            self.location = Location::Session(session.cwd.clone());
            self.load_sessions();
        } else {
            self.location = Location::Session(session.cwd.clone());
        }
        // Its title, as the sidebar lists it; its name to message it by until it has one.
        let title = session
            .title
            .clone()
            .unwrap_or_else(|| session.name.clone());
        // Preserve the watched session's own backend (`Claude`, `Agy`, `Opencode`).
        self.reset_session(Some(session.id.clone()), Some(title), session.backend);
        self.status.sign_in_with = provider::SignInWith::Account;
        self.load_session(session.id.clone(), session.cwd.clone());
        self.forget_watch();
        self.watch = Some(Watch {
            place: ipc::Place::Session {
                cwd: session.cwd.clone(),
                id: session.id.clone(),
            },
            session,
            offset: 0,
            reading: Refresh::Reading,
            watched: None,
        });
        self.redraw = Redraw::everything();
    }

    /// Continues the watched session here, as a copy: the original runs on where it runs.
    pub(super) fn continue_here(&mut self) {
        let Some(watch) = self.watch.take() else {
            return;
        };
        if !watch
            .session
            .backend
            .capabilities(provider::SignInWith::Account)
            .fork
        {
            let agent = watch.session.backend.agent();
            self.watch = Some(watch);
            self.chat
                .info(format!("{agent} sessions can't be copied yet."));
            return;
        }
        self.status.session_id = None;
        self.status.title = Some(format!("{} (copy)", watch.session.name));
        self.start(Some(Resume::Fork(watch.session.id)));
        self.redraw.sessions = true;
        self.view.focus_composer();
    }

    pub(super) fn go_to_window(&mut self) {
        let Some(watch) = &self.watch else {
            return;
        };
        let pid = watch.session.pid;
        if pid == 0 {
            return;
        }
        self.ask_desktop(Act::FocusWindowOf { pid }, Acting::Desktop);
    }
}
