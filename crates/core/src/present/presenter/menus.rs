//! The model and mode menus.

use strum::IntoEnumIterator;

use crate::app::{Mode, Modes, PermissionMode, Status};
use crate::present::Frontend;
use crate::present::model::{MenuItem, PermissionKind};

use super::{Presenter, index_for_slint};

/// The mode menu's items, with their descriptions, and the one ticked.
pub(super) fn mode_menu(
    modes: &Modes,
    backend: crate::provider::Backend,
) -> (Vec<(String, String)>, usize) {
    match modes {
        Modes::Claude(current) => {
            let mut items: Vec<(String, String)> = PermissionMode::iter()
                .map(|mode| (mode.label().to_owned(), mode.description().to_owned()))
                .collect();
            let ticked = match current {
                Some(Mode::Known(mode)) => PermissionMode::iter()
                    .position(|known| known == *mode)
                    .unwrap_or_default(),
                Some(Mode::Unknown(name)) => {
                    items.push((name.clone(), "As the CLI names it".to_owned()));
                    items.len() - 1
                }
                None => 0,
            };
            (items, ticked)
        }
        // Before the program has said which modes there are: a name, not an empty menu.
        Modes::Program { offered, .. } if offered.is_empty() => (
            vec![(
                "Default mode".to_owned(),
                format!("{} says which modes it has once it runs", backend.program()),
            )],
            0,
        ),
        Modes::Program { offered, current } => {
            let items = offered
                .iter()
                .map(|mode| (mode.name.clone(), mode.description.clone()))
                .collect();
            let ticked = current
                .as_ref()
                .and_then(|current| offered.iter().position(|mode| &mode.id == current))
                .unwrap_or_default();
            (items, ticked)
        }
    }
}

pub(super) fn menu(items: &[(String, String)]) -> Vec<MenuItem> {
    let items: Vec<MenuItem> = items
        .iter()
        .map(|(title, description)| MenuItem {
            title: title.as_str().into(),
            description: description.as_str().into(),
        })
        .collect();
    items
}

impl<F: Frontend> Presenter<F> {
    /// The model and mode menus, and which of each is in use.
    pub fn render_menus(&mut self, status: &Status) {
        let mut models: Vec<(String, String)> = status
            .models
            .iter()
            .map(|model| (model.display_name.clone(), model.description.clone()))
            .collect();
        let picked = status
            .chosen_model()
            .and_then(|choice| status.models.iter().position(|model| model.value == choice))
            .or_else(|| {
                status.models.iter().position(|model| {
                    model.value == "default" && model.resolved_model == status.model
                })
            })
            .or_else(|| {
                status.models.iter().position(|model| {
                    model.resolved_model == status.model || model.value == status.model
                })
            });
        let model_index = match picked {
            Some(index) => index,
            None if !status.model.is_empty() => {
                models.push((status.model.clone(), String::new()));
                models.len() - 1
            }
            // Before the program has said which models there are: a name, not a bare icon.
            None if models.is_empty() => {
                let name = status.chosen_model().unwrap_or("Default model").to_owned();
                models.push((
                    name,
                    format!(
                        "{} says which model this is once it runs",
                        status.backend.program()
                    ),
                ));
                0
            }
            None => 0,
        };
        let (modes, mode_index) = mode_menu(&status.modes, status.backend);
        let session = &mut self.session;
        if self.models != models {
            session.models = menu(&models);
            self.models = models;
        }
        if self.modes != modes {
            session.modes = menu(&modes);
            self.modes = modes;
        }
        self.menu_choice = Some((index_for_slint(model_index), index_for_slint(mode_index)));
        self.session.mode = match &status.modes {
            Modes::Claude(Some(Mode::Known(PermissionMode::Default)) | None) => {
                PermissionKind::Default
            }
            Modes::Claude(Some(Mode::Known(PermissionMode::AcceptEdits))) => {
                PermissionKind::AcceptEdits
            }
            Modes::Claude(Some(Mode::Known(PermissionMode::Plan))) => PermissionKind::Plan,
            Modes::Claude(Some(Mode::Known(PermissionMode::Auto))) => PermissionKind::Auto,
            Modes::Claude(Some(Mode::Known(PermissionMode::BypassPermissions))) => {
                PermissionKind::Bypass
            }
            Modes::Program {
                current: Some(mode),
                ..
            } if mode.to_string().eq_ignore_ascii_case("plan") => PermissionKind::Plan,
            // What a program's own mode lets through is the program's to say.
            Modes::Claude(Some(Mode::Unknown(_))) | Modes::Program { .. } => {
                PermissionKind::Unknown
            }
        };
    }
}
