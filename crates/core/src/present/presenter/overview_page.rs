//! The start page and the projects page.

use crate::external::wire;
use crate::ids::SessionId;
use crate::overview;
use crate::present::model::{
    CardSessionItem, CommandItem, ProjectCard, ProjectItem, ProjectSort, SwitcherItem, WaitingItem,
};
use crate::present::{Command, Frontend};
use crate::sessions::{self, ProjectInfo};
use crate::settings;
use crate::tools::shortened_path;

use super::{Presenter, index_for_slint, live_state};

/// How many recent project cards stay visible on the start page when `show_all` is off.
pub(super) const COMPACT_PROJECTS: usize = 9;

impl<F: Frontend> Presenter<F> {
    pub fn render_overview(
        &mut self,
        built: &overview::Built,
        shown: Option<&SessionId>,
        view: OverviewView<'_>,
    ) {
        self.nav.needs_you = index_for_slint(built.waiting.len());
        self.nav.waiting = built
            .waiting
            .iter()
            .map(|item| {
                let waited = Some(sessions::age(item.since))
                    .filter(|age| !age.is_empty())
                    .map_or_else(|| "waiting".to_owned(), |age| format!("waiting {age}"));
                WaitingItem {
                    project_name: item.project_name.clone(),
                    project_folder: item.project_path.to_string_lossy().into_owned(),
                    session_id: item.opens.session().as_str().to_owned(),
                    hosted_id: item
                        .opens
                        .hosted()
                        .map_or_else(String::new, |id| id.to_string()),
                    session_title: item.session_title.clone(),
                    question: item.question.clone(),
                    waited,
                }
            })
            .collect();

        let hidden_count = built.cards.iter().filter(|card| card.hidden).count();
        let eligible: Vec<&overview::Card> = built
            .cards
            .iter()
            .filter(|card| view.show_hidden || !card.hidden)
            .collect();
        let total_projects = eligible.len();
        let needle = view.filter.trim().to_lowercase();

        let mut switcher = Vec::new();
        for card in &eligible {
            let project_folder = card.path.to_string_lossy().into_owned();
            for session in &card.sessions {
                let age = sessions::age(session.modified);
                let id = session.opens.session().as_str().to_owned();
                let hosted_id = session
                    .opens
                    .hosted()
                    .map_or_else(String::new, |id| id.to_string());
                let detail = match session.worktree.as_deref() {
                    Some(worktree) if !worktree.is_empty() => {
                        format!("{worktree} · {age}")
                    }
                    _ => age,
                };
                switcher.push(SwitcherItem {
                    project_name: card.name.clone(),
                    project_folder: project_folder.clone(),
                    id,
                    hosted_id,
                    title: session.title.clone(),
                    detail,
                    state: live_state(session.state),
                    selected: shown.is_some_and(|current| session.opens.session() == current),
                });
            }
        }

        let filtered: Vec<&overview::Card> = if needle.is_empty() {
            eligible
        } else {
            eligible
                .into_iter()
                .filter(|card| {
                    card.name.to_lowercase().contains(&needle)
                        || card.path.to_string_lossy().to_lowercase().contains(&needle)
                        || card
                            .last_said
                            .as_deref()
                            .is_some_and(|said| said.to_lowercase().contains(&needle))
                })
                .collect()
        };

        let visible: Vec<&overview::Card> = if view.show_all || !needle.is_empty() {
            filtered.clone()
        } else {
            let mut remaining_unpinned = COMPACT_PROJECTS.saturating_sub(
                filtered
                    .iter()
                    .filter(|card| card.pinned || card.needs_you > 0 || card.running > 0)
                    .count(),
            );
            filtered
                .iter()
                .copied()
                .filter(|card| {
                    if card.pinned || card.needs_you > 0 || card.running > 0 {
                        true
                    } else if remaining_unpinned > 0 {
                        remaining_unpinned -= 1;
                        true
                    } else {
                        false
                    }
                })
                .collect()
        };
        let has_more = !view.show_all && needle.is_empty() && visible.len() < filtered.len();

        self.nav.cards = visible
            .into_iter()
            .map(|card| {
                let project_folder = card.path.to_string_lossy().into_owned();
                let sessions = card
                    .sessions
                    .iter()
                    .map(|session| {
                        let age = sessions::age(session.modified);
                        let id = session.opens.session().as_str().to_owned();
                        let hosted_id = session
                            .opens
                            .hosted()
                            .map_or_else(String::new, |id| id.to_string());
                        CardSessionItem {
                            id,
                            hosted_id,
                            title: session.title.clone(),
                            state: live_state(session.state),
                            age,
                        }
                    })
                    .collect::<Vec<_>>();
                ProjectCard {
                    path: project_folder,
                    shown: shortened_path(&card.path, 56),
                    name: card.name.clone(),
                    age: sessions::age(card.modified),
                    needs_you: index_for_slint(card.needs_you),
                    running: index_for_slint(card.running),
                    last_said: card.last_said.clone().unwrap_or_default(),
                    has_sessions: !sessions.is_empty(),
                    sessions,
                    pinned: card.pinned,
                    hidden: card.hidden,
                }
            })
            .collect();
        self.nav.all_sessions = switcher;

        self.nav.project_sort = match view.sort {
            settings::ProjectSort::PinnedFirst => ProjectSort::PinnedFirst,
            settings::ProjectSort::Recent => ProjectSort::Recent,
        };
        self.nav.show_hidden_projects = view.show_hidden;
        self.nav.show_all_projects = view.show_all;
        self.nav.total_projects = index_for_slint(total_projects);
        self.nav.hidden_projects_count = index_for_slint(hidden_count);
        self.nav.has_more_projects = has_more;
    }

    pub fn set_new_projects_in(&mut self, folder: &str) {
        self.preferences.new_projects_in = folder.into();
    }

    pub fn set_new_project_parent(&mut self, folder: &str) {
        self.nav.new_project_parent = folder.into();
    }

    pub fn set_new_project_error(&mut self, error: &str) {
        self.nav.new_project_error = error.into();
    }

    pub fn new_project_created(&mut self) {
        self.nav.new_project_error.clear();
        self.command(Command::NewProjectCreated);
    }

    pub fn focus_page(&mut self) {
        self.command(Command::FocusPage);
    }

    pub fn clear_selection(&mut self) {
        self.command(Command::ClearSelection);
    }

    pub fn toggle_switcher(&mut self) {
        self.command(Command::ToggleSwitcher);
    }

    pub fn open_new_project(&mut self) {
        self.nav.new_project_error.clear();
        self.command(Command::OpenNewProject);
    }

    /// Why the folder typed in the list of folders didn't open; empty for nothing to say.
    pub fn set_project_problem(&mut self, problem: &str) {
        self.sessions.problem = problem.into();
    }

    /// Closes the folder list, as the folder typed in it opened.
    pub fn projects_opened(&mut self) {
        self.sessions.problem.clear();
        self.command(Command::ProjectsOpened);
    }

    pub fn render_projects(&mut self, projects: &[ProjectInfo]) {
        let items: Vec<ProjectItem> = projects
            .iter()
            .map(|project| ProjectItem {
                path: project.path.to_string_lossy().as_ref().into(),
                shown: shortened_path(&project.path, 56),
                name: sessions::folder_name(&project.path),
                age: sessions::age(project.modified),
            })
            .collect();
        self.sessions.projects = items;
    }

    pub fn render_commands(&mut self, commands: &[&wire::CommandInfo]) {
        let items: Vec<CommandItem> = commands
            .iter()
            .map(|command| CommandItem {
                name: command.name.as_str().into(),
                hint: command.argument_hint.as_str().into(),
                description: command.description.as_str().into(),
            })
            .collect();
        self.composer.commands = items;
    }
}

/// Start-page view preferences and filter applied when presenting project cards.
#[derive(Debug, Default, Clone, Copy)]
pub struct OverviewView<'a> {
    pub sort: settings::ProjectSort,
    pub show_hidden: bool,
    pub show_all: bool,
    pub filter: &'a str,
}
