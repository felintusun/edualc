//! Permission modes and models: what each program offers, and the one chosen.

use crate::external::wire::{self, ControlRequest};
use crate::present::Frontend;
use crate::provider;

use super::{App, ModelChoice, Request};

/// The permission modes the menu offers, as the CLI names them.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    strum::EnumIter,
    strum::EnumString,
    strum::IntoStaticStr,
)]
#[strum(serialize_all = "camelCase")]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    Default,
    AcceptEdits,
    Plan,
    Auto,
    BypassPermissions,
}

impl PermissionMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "Ask before edits",
            Self::AcceptEdits => "Accept edits",
            Self::Plan => "Plan mode",
            Self::Auto => "Auto",
            Self::BypassPermissions => "Bypass permissions",
        }
    }

    /// Returns the mode's name as Claude Code 2.1.329's permission dialog gives it in "Yes, and
    /// switch to …".
    pub fn claude_code_words(self) -> &'static str {
        match self {
            Self::Default => "default (ask each time)",
            Self::AcceptEdits => "accept edits (auto-approve file edits and common file commands)",
            Self::Plan => "plan mode (research and propose changes without making them)",
            Self::Auto => "auto (no routine prompts; a reviewer model screens actions)",
            Self::BypassPermissions => "BYPASS PERMISSIONS (no further prompts)",
        }
    }

    /// Returns what the mode lets through, for its menu.
    pub fn description(self) -> &'static str {
        match self {
            Self::Default => "Asks before edits and commands",
            Self::AcceptEdits => "Edits without asking",
            Self::Plan => "Reads and plans, edits nothing",
            Self::Auto => "A classifier approves safe actions",
            Self::BypassPermissions => "Never asks. For throwaway sandboxes",
        }
    }
}

/// The mode the CLI reports: one the menu knows, or one named as the CLI names it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum Mode {
    Known(PermissionMode),
    Unknown(String),
}

/// The modes a session's menu offers, and the one the session is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Modes {
    /// Claude Code's, [`PermissionMode`]'s; and the one its process reports.
    Claude(Option<Mode>),
    /// An agent program's own, as its adapter reports them: none before it has.
    Program {
        offered: Vec<wire::ProgramMode>,
        current: Option<wire::ModeId>,
    },
}

impl Default for Modes {
    fn default() -> Self {
        Self::of(provider::Backend::default())
    }
}

impl Modes {
    /// Returns the modes a session of `backend` offers before its process says more.
    pub fn of(backend: provider::Backend) -> Self {
        if backend
            .capabilities(provider::SignInWith::Account)
            .fixed_modes
        {
            Self::Claude(None)
        } else {
            Self::Program {
                offered: Vec::new(),
                current: None,
            }
        }
    }

    /// Records that Claude Code's process is in `mode`; another program's session has none of
    /// Claude Code's modes.
    pub(super) fn claude_in(&mut self, mode: Mode) {
        if let Self::Claude(current) = self {
            *current = Some(mode);
        }
    }

    /// Records that the session is in `chosen`, if `chosen` is one of its kind.
    pub(super) fn chose(&mut self, chosen: &ChosenMode) {
        match (self, chosen) {
            (Self::Claude(current), ChosenMode::Claude(mode)) => {
                *current = Some(Mode::Known(*mode))
            }
            (Self::Program { current, .. }, ChosenMode::Program(id)) => {
                *current = Some(id.clone());
            }
            (Self::Claude(_), ChosenMode::Program(_))
            | (Self::Program { .. }, ChosenMode::Claude(_)) => {}
        }
    }

    /// Returns `chosen`'s name in the menu.
    pub(super) fn label(&self, chosen: &ChosenMode) -> String {
        match (self, chosen) {
            (_, ChosenMode::Claude(mode)) => mode.label().to_owned(),
            (Self::Program { offered, .. }, ChosenMode::Program(id)) => offered
                .iter()
                .find(|mode| &mode.id == id)
                .map_or_else(|| id.to_string(), |mode| mode.name.clone()),
            (Self::Claude(_), ChosenMode::Program(id)) => id.to_string(),
        }
    }
}

/// A mode picked in the menu: one of Claude Code's, or one of an agent program's own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChosenMode {
    Claude(PermissionMode),
    Program(wire::ModeId),
}

impl ChosenMode {
    pub(super) fn request(&self) -> ControlRequest {
        match self {
            Self::Claude(mode) => ControlRequest::SetPermissionMode { mode: *mode },
            Self::Program(mode) => ControlRequest::SetProgramMode { mode: mode.clone() },
        }
    }
}

impl<F: Frontend> App<F> {
    pub(super) fn choose_model(&mut self, index: usize) {
        if self.watch.is_some() {
            self.redraw.menus = true;
            return;
        }
        let Some(value) = self
            .status
            .models
            .get(index)
            .map(|model| model.value.clone())
        else {
            self.redraw.menus = true;
            return;
        };
        // Without a process the choice waits for the next: it is started with it.
        if self.process.is_none() {
            self.status.model_choice = Some(ModelChoice {
                backend: self.status.backend,
                model: value,
            });
            self.redraw.menus = true;
            return;
        }
        if !self.request(
            ControlRequest::SetModel {
                model: value.clone(),
            },
            Request::Model(value),
        ) {
            self.redraw.menus = true;
        }
    }

    pub(super) fn choose_mode(&mut self, index: usize) {
        use strum::IntoEnumIterator;
        if self.watch.is_some() {
            self.redraw.menus = true;
            return;
        }
        let chosen = match &self.status.modes {
            Modes::Claude(_) => PermissionMode::iter().nth(index).map(ChosenMode::Claude),
            Modes::Program { offered, .. } => offered
                .get(index)
                .map(|mode| ChosenMode::Program(mode.id.clone())),
        };
        match chosen {
            Some(mode) => self.set_mode(mode),
            None => self.redraw.menus = true,
        }
    }

    pub(super) fn set_mode(&mut self, mode: ChosenMode) {
        // Without a process the choice waits for the next, which `start` gives it.
        if self.process.is_none() {
            self.status.modes.chose(&mode);
            self.status.mode_choice = Some(mode);
            self.redraw.menus = true;
            return;
        }
        if !self.request(mode.request(), Request::Mode(mode.clone())) {
            self.redraw.menus = true;
            return;
        }
        // An ACP agent takes a mode as a turn starts: the turn under way keeps the one it began
        // with.
        if self.status.capabilities().deferred_mode_switch && self.status.turns.under_way() {
            self.chat.info(format!(
                "{} switches to “{}” after this turn.",
                self.status.backend.agent(),
                self.status.modes.label(&mode)
            ));
            self.view.scroll_to_end();
        }
    }
}
