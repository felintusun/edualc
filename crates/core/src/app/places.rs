//! Where the window is: the start page, the projects and their sessions, and jumping to a
//! session that needs you.

use std::path::{Path, PathBuf};

use crate::ipc::{Act, ActId, CreateProjectError, ProjectName, Read};
use crate::overview;
use crate::present::Frontend;
use crate::provider;
use crate::sessions::SessionInfo;
use crate::settings;
use crate::trust;

use super::{Acting, App, Location, Reading, Redraw, SessionTarget};

impl<F: Frontend> App<F> {
    pub(super) fn refresh_overview(&mut self) {
        let home = dirs::home_dir();
        let options = overview::Options {
            marks: &self.settings.project_marks,
            recent: &self.settings.recent_projects,
            sort: self.settings.project_sort,
        };
        self.built_overview = overview::build_with(
            &self.overview,
            &self.live,
            &self.hosted,
            home.as_deref(),
            &options,
        );
        self.redraw.overview = true;
    }

    pub(super) fn touch_recent_project(&mut self, path: &Path) {
        if dirs::home_dir().as_deref() == Some(path) {
            return;
        }
        let now_secs = web_time::SystemTime::now()
            .duration_since(web_time::SystemTime::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs());
        let recently_recorded = self.settings.recent_projects.first().is_some_and(|recent| {
            recent.path == path && now_secs.saturating_sub(recent.opened_secs) < 60
        });
        if recently_recorded {
            return;
        }
        self.change_settings(settings::Change::TouchProject {
            path: path.to_path_buf(),
            opened_secs: now_secs,
        });
        self.refresh_overview();
    }

    pub(super) fn load_overview(&mut self) {
        if self.listing_overview.wanted() {
            self.read(Read::Overview, Reading::Overview);
        }
    }

    pub(super) fn open_start(&mut self) {
        if matches!(&self.location, Location::Session(_)) {
            self.leave();
            self.watch = None;
            self.chat.clear();
            self.reset_session(None, None, self.default_backend);
        }
        self.view.clear_selection();
        self.location = Location::Start;
        self.session_filter.clear();
        self.sessions.clear();
        self.load_overview();
        self.list_live();
        self.redraw = Redraw::everything();
        self.view.focus_page();
    }

    pub(super) fn enter_project(&mut self, path: PathBuf) {
        if matches!(&self.location, Location::Session(_)) {
            self.leave();
            self.watch = None;
            self.chat.clear();
        }
        self.view.clear_selection();
        // Reset backend/sign-in back to the default so "New session here" doesn't inherit
        // `claude-api-key` or another backend from an earlier session.
        self.reset_session(None, None, self.default_backend);
        self.touch_recent_project(&path);
        self.switch_project_sessions(&path);
        self.location = Location::Project(path);
        self.session_filter.clear();
        self.load_sessions();
        self.list_live();
        self.redraw = Redraw::everything();
        self.view.focus_page();
    }

    pub(super) fn back(&mut self) {
        match &self.location {
            Location::Start => {}
            Location::Project(_) => self.open_start(),
            Location::Session(cwd) => self.enter_project(cwd.clone()),
        }
    }

    pub(super) fn jump_to(&mut self, SessionTarget { folder, opens }: SessionTarget) {
        let session = match opens {
            overview::SessionRef::Hosted {
                hosted: hosted_id,
                session,
            } => {
                if let Some(hosted) = self
                    .hosted
                    .iter()
                    .find(|hosted| hosted.id == hosted_id)
                    .cloned()
                {
                    if !hosted.attached {
                        self.attach(&hosted);
                        return;
                    }
                    if self.status.session_id.as_ref() == Some(&hosted.session)
                        && self.watch.is_none()
                    {
                        self.location = Location::Session(hosted.cwd);
                        self.redraw.overview = true;
                        self.view.focus_composer();
                        return;
                    }
                }
                // Ended since, or shown in another window.
                session
            }
            overview::SessionRef::Session(session) => session,
        };
        if self.location.cwd() != Some(folder.as_path()) {
            self.switch_project_sessions(&folder);
            self.location = Location::Session(folder);
            self.load_sessions();
        } else {
            self.location = Location::Session(folder);
        }
        self.open_session(session);
    }

    pub(super) fn next_waiting(&mut self) {
        let Some(first) = self.built_overview.waiting.first() else {
            return;
        };
        let current = self.status.session_id.as_ref();
        let chosen = self
            .built_overview
            .waiting
            .iter()
            .find(|item| Some(item.opens.session()) != current)
            .unwrap_or(first)
            .clone();
        self.jump_to(SessionTarget {
            folder: chosen.project_path,
            opens: chosen.opens,
        });
    }

    pub(super) fn create_project(&mut self, parent: &str, typed: String) {
        let name = match ProjectName::try_from(typed) {
            Ok(name) => name,
            Err(refused) => {
                self.view.set_new_project_error(&refused.to_string());
                return;
            }
        };
        self.view.set_new_project_error("");
        let parent_path = if parent.trim().is_empty() {
            self.settings.effective_projects_dir().unwrap_or_default()
        } else {
            PathBuf::from(parent.trim())
        };
        self.ask_daemon(
            Act::CreateProject {
                parent: parent_path,
                name,
            },
            Acting::CreatingProject,
        );
    }

    pub(super) fn project_created(
        &mut self,
        request: ActId,
        result: Result<PathBuf, CreateProjectError>,
    ) {
        if !matches!(self.acting.remove(&request), Some(Acting::CreatingProject)) {
            return;
        }
        match result {
            Ok(path) => {
                if !trust::trusted_in_edualc(&path, &self.settings) {
                    self.change_settings(settings::Change::Trust(path.clone()));
                }
                self.view.new_project_created();
                self.touch_recent_project(&path);
                self.location = Location::Session(path);
                self.sessions.clear();
                self.load_sessions();
                self.load_overview();
                self.new_session(self.default_backend, provider::SignInWith::Account);
            }
            Err(error) => self.view.set_new_project_error(&error.to_string()),
        }
    }

    pub(super) fn set_new_projects_in(&mut self, folder: Option<PathBuf>) {
        self.change_settings(settings::Change::NewProjectsIn(folder));
        let shown = self
            .settings
            .effective_projects_dir()
            .map_or_default(|dir| dir.to_string_lossy().into_owned());
        self.view.set_new_projects_in(&shown);
        self.view.set_new_project_parent(&shown);
    }

    /// Keeps `self.sessions` when staying in `cwd`; otherwise seeds it from `self.overview` so
    /// `Earlier` never blanks out while `Read::Sessions` runs.
    pub(super) fn switch_project_sessions(&mut self, cwd: &Path) {
        if self.location.cwd() == Some(cwd) {
            return;
        }
        self.sessions = self
            .overview
            .iter()
            .find(|project| project.path == cwd)
            .map_or_else(Vec::new, |project| project.sessions.clone());
    }

    /// Lists the working directory's sessions anew, e.g. once a new one has its title.
    pub(super) fn load_sessions(&mut self) {
        let Some(cwd) = self.location.cwd().map(Path::to_path_buf) else {
            return;
        };
        // Those listed already are summed up again only if their transcript changed.
        let read = Read::Sessions {
            cwd: cwd.clone(),
            limit: 200,
            known: self.sessions.clone(),
        };
        self.read(read, Reading::Sessions { cwd });
        if self.answers.contains_key(&self.last_act) {
            self.sessions_request = Some(self.last_act);
        }
    }

    pub(super) fn sessions_listed(&mut self, cwd: &Path, sessions: Vec<SessionInfo>) {
        if self.location.cwd() != Some(cwd) {
            return;
        }
        if let Some(id) = &self.status.session_id
            && let Some(session) = sessions.iter().find(|session| &session.id == id)
            && self.watch.is_none()
        {
            self.status.title = Some(session.title.clone());
        }
        self.sessions = sessions;
        self.redraw.sessions = true;
    }
}
