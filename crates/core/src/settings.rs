//! What the user set and wants kept between runs: the text zoom, the look, the folders trusted
//! to run Claude Code in, the projects pinned or removed.
//!
//! What you might set by hand is in `$XDG_CONFIG_HOME/edualc/config.toml`, changed a key at a
//! time so your comments and order stay. What edualc keeps for itself (folders opened, pinned,
//! hidden, removed or trusted, the composer's height) is in `$XDG_STATE_HOME/edualc/state.json`.
//! A `settings.json` of an earlier version, kept where `config.toml` is now, is read while
//! neither is there, and moved into both by the first change.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cjk::CjkLanguage;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// The text size relative to the desktop's, in percent.
    pub zoom_percent: u16,
    /// Trusted here (see [`crate::trust`]), with what is under them.
    pub trusted_folders: Vec<PathBuf>,
    /// The composer's height as dragged, in logical pixels; fitted to its text if none.
    pub composer_height_px: Option<u16>,
    /// Where new projects are made; `~/code` if it exists, else `~`, if none.
    pub new_projects_in: Option<PathBuf>,
    /// Folders trusted here that you chose not to trust for Claude Code too: not asked again.
    pub not_for_claude_code: Vec<PathBuf>,
    /// The window's colours (dark or light as the desktop asks).
    pub look: Look,
    /// The key that sends what is written in the composer.
    pub send_with: SendKey,
    /// The language of text with Han characters alone (see [`crate::cjk`]); none for the desktop's,
    /// or Japanese if that isn't a CJK one.
    pub han_text: Option<CjkLanguage>,
    /// Whether a message from another session comes in at once, or only once you let its
    /// sender in. Messages edualc doesn't carry follow the receiving program's own rules.
    pub peer_messages: PeerMessages,
    /// Project folders pinned, hidden or removed on the start page.
    pub project_marks: BTreeMap<PathBuf, ProjectMark>,
    /// How project cards on the start page are sorted.
    pub project_sort: ProjectSort,
    /// Whether hidden projects are shown on the start page.
    pub show_hidden_projects: bool,
    /// Whether all recent projects are shown instead of the compact start-page list.
    pub show_all_projects: bool,
    /// Project folders created or opened here, newest first, so they stay on the start page
    /// even before a first turn writes a transcript.
    pub recent_projects: Vec<RecentProject>,
}

/// How projects on the start page are ordered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectSort {
    #[default]
    PinnedFirst,
    Recent,
}

/// What you made of a project folder on the start page; a folder has one of these at most.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectMark {
    /// At the top of the list.
    Pinned,
    /// Left out of the list unless hidden projects are shown.
    Hidden,
    /// Left out of edualc, its files and transcripts kept, until you open it again.
    Removed,
}

/// A project folder explicitly opened or created, with when it was last opened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecentProject {
    pub path: PathBuf,
    pub opened_secs: u64,
}

/// How messages from other sessions come into a session edualc runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerMessages {
    /// You're asked before the first message from each sender, in each session.
    AskEachSender,
    /// Every message comes in without asking, as between Claude Code's own sessions.
    #[default]
    LetAllIn,
}

/// What sends the draft: Enter (Shift+Enter or Ctrl+Enter start a new line), or Ctrl+Enter
/// (Enter starts a new line, so a stray Enter never sends), as many chat apps let one choose.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendKey {
    #[default]
    Enter,
    CtrlEnter,
}

/// The looks to choose from: Gruvbox, or plain greys with a blue accent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Look {
    #[default]
    Gruvbox,
    Plain,
}

impl Look {
    /// The look the palette button switches to.
    pub fn other(self) -> Self {
        match self {
            Self::Gruvbox => Self::Plain,
            Self::Plain => Self::Gruvbox,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            zoom_percent: Self::full_size(),
            trusted_folders: Vec::new(),
            composer_height_px: None,
            new_projects_in: None,
            not_for_claude_code: Vec::new(),
            look: Look::default(),
            send_with: SendKey::default(),
            han_text: None,
            peer_messages: PeerMessages::default(),
            project_marks: BTreeMap::new(),
            project_sort: ProjectSort::default(),
            show_hidden_projects: false,
            show_all_projects: false,
            recent_projects: Vec::new(),
        }
    }
}

/// How far the text zooms, in percent, and in what steps.
const ZOOM_RANGE: std::ops::RangeInclusive<u16> = 70..=200;
const ZOOM_STEP: u16 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zoom {
    In,
    Out,
    Reset,
}

impl Settings {
    fn full_size() -> u16 {
        100
    }

    pub fn zoom(&mut self, zoom: Zoom) {
        let wanted = match zoom {
            Zoom::In => self.zoom_percent.saturating_add(ZOOM_STEP),
            Zoom::Out => self.zoom_percent.saturating_sub(ZOOM_STEP),
            Zoom::Reset => Self::full_size(),
        };
        self.zoom_percent = wanted.clamp(*ZOOM_RANGE.start(), *ZOOM_RANGE.end());
    }

    pub fn scale(&self) -> f32 {
        f32::from(self.zoom_percent) / 100.0
    }

    /// The default folder for new projects: what you set, else `~/code` if it exists, else `~`.
    pub fn effective_projects_dir(&self) -> Option<PathBuf> {
        if let Some(dir) = &self.new_projects_in {
            return Some(dir.clone());
        }
        let home = dirs::home_dir()?;
        let code = home.join("code");
        Some(if code.is_dir() { code } else { home })
    }

    /// The language text with Han characters alone is taken to be in: the one chosen, else
    /// the desktop's if CJK, else Japanese.
    pub fn han_language(&self) -> CjkLanguage {
        self.han_text
            .or_else(CjkLanguage::of_desktop)
            .unwrap_or(CjkLanguage::Japanese)
    }

    pub fn project_mark(&self, folder: &Path) -> Option<ProjectMark> {
        self.project_marks.get(folder).copied()
    }

    pub fn is_removed(&self, folder: &Path) -> bool {
        self.project_mark(folder) == Some(ProjectMark::Removed)
    }

    /// Marks `folder` so, or takes the mark off if it has it already.
    fn toggle_mark(&mut self, folder: PathBuf, mark: ProjectMark) {
        if self.project_marks.get(&folder) == Some(&mark) {
            self.project_marks.remove(&folder);
        } else {
            self.project_marks.insert(folder, mark);
        }
    }
}

/// One thing the user changed. A window sends the change, not its whole copy of the settings,
/// so what another window changed meanwhile is kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Change {
    ZoomPercent(u16),
    Look(Look),
    SendWith(SendKey),
    HanText(CjkLanguage),
    ComposerHeight(Option<u16>),
    /// The folder New project makes projects in; the default if none.
    NewProjectsIn(Option<PathBuf>),
    /// A folder trusted here, with what is under it.
    Trust(PathBuf),
    /// A folder trusted here that isn't to be trusted for Claude Code too, nor asked about.
    NotForClaudeCode(PathBuf),
    PeerMessages(PeerMessages),
    TogglePinProject(PathBuf),
    ToggleHideProject(PathBuf),
    /// Takes a folder off the start page, its files and transcripts kept, until it is opened
    /// again.
    RemoveProject(PathBuf),
    ProjectSort(ProjectSort),
    ShowHiddenProjects(bool),
    ShowAllProjects(bool),
    TouchProject {
        path: PathBuf,
        opened_secs: u64,
    },
}

/// How many folders opened or created are kept; the oldest go first.
const MAX_RECENT_PROJECTS: usize = 50;

impl Settings {
    pub fn apply(&mut self, change: Change) {
        match change {
            Change::ZoomPercent(percent) => {
                self.zoom_percent = percent.clamp(*ZOOM_RANGE.start(), *ZOOM_RANGE.end());
            }
            Change::Look(look) => self.look = look,
            Change::SendWith(key) => self.send_with = key,
            Change::HanText(language) => self.han_text = Some(language),
            Change::ComposerHeight(height) => self.composer_height_px = height,
            Change::NewProjectsIn(folder) => self.new_projects_in = folder,
            Change::Trust(folder) => {
                if !self.trusted_folders.contains(&folder) {
                    self.trusted_folders.push(folder);
                }
            }
            Change::NotForClaudeCode(folder) => {
                if !self.not_for_claude_code.contains(&folder) {
                    self.not_for_claude_code.push(folder);
                }
            }
            Change::PeerMessages(peer_messages) => self.peer_messages = peer_messages,
            Change::TogglePinProject(folder) => self.toggle_mark(folder, ProjectMark::Pinned),
            Change::ToggleHideProject(folder) => self.toggle_mark(folder, ProjectMark::Hidden),
            Change::RemoveProject(folder) => {
                self.recent_projects.retain(|recent| recent.path != folder);
                self.project_marks.insert(folder, ProjectMark::Removed);
            }
            Change::ProjectSort(sort) => self.project_sort = sort,
            Change::ShowHiddenProjects(show) => self.show_hidden_projects = show,
            Change::ShowAllProjects(show) => self.show_all_projects = show,
            Change::TouchProject { path, opened_secs } => {
                if self.is_removed(&path) {
                    self.project_marks.remove(&path);
                }
                self.recent_projects.retain(|recent| recent.path != path);
                self.recent_projects
                    .insert(0, RecentProject { path, opened_secs });
                self.recent_projects.truncate(MAX_RECENT_PROJECTS);
            }
        }
    }
}

/// What `config.toml` holds: what you might set by hand.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    zoom_percent: u16,
    look: Look,
    send_with: SendKey,
    han_text: Option<CjkLanguage>,
    new_projects_in: Option<PathBuf>,
    peer_messages: PeerMessages,
    project_sort: ProjectSort,
}

#[cfg(not(target_arch = "wasm32"))]
impl Default for Config {
    fn default() -> Self {
        Self::from(&Settings::default())
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl From<&Settings> for Config {
    fn from(settings: &Settings) -> Self {
        Self {
            zoom_percent: settings.zoom_percent,
            look: settings.look,
            send_with: settings.send_with,
            han_text: settings.han_text,
            new_projects_in: settings.new_projects_in.clone(),
            peer_messages: settings.peer_messages,
            project_sort: settings.project_sort,
        }
    }
}

/// A key of `config.toml`, named as [`Config`]'s field is.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
enum ConfigKey {
    ZoomPercent,
    Look,
    SendWith,
    HanText,
    NewProjectsIn,
    PeerMessages,
    ProjectSort,
}

/// What `state.json` holds: what edualc keeps for itself.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct State {
    trusted_folders: Vec<PathBuf>,
    composer_height_px: Option<u16>,
    not_for_claude_code: Vec<PathBuf>,
    project_marks: BTreeMap<PathBuf, ProjectMark>,
    show_hidden_projects: bool,
    show_all_projects: bool,
    recent_projects: Vec<RecentProject>,
}

#[cfg(not(target_arch = "wasm32"))]
impl From<&Settings> for State {
    fn from(settings: &Settings) -> Self {
        Self {
            trusted_folders: settings.trusted_folders.clone(),
            composer_height_px: settings.composer_height_px,
            not_for_claude_code: settings.not_for_claude_code.clone(),
            project_marks: settings.project_marks.clone(),
            show_hidden_projects: settings.show_hidden_projects,
            show_all_projects: settings.show_all_projects,
            recent_projects: settings.recent_projects.clone(),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Settings {
    fn from_parts(config: Config, state: State) -> Self {
        Self {
            zoom_percent: config.zoom_percent,
            trusted_folders: state.trusted_folders,
            composer_height_px: state.composer_height_px,
            new_projects_in: config.new_projects_in,
            not_for_claude_code: state.not_for_claude_code,
            look: config.look,
            send_with: config.send_with,
            han_text: config.han_text,
            peer_messages: config.peer_messages,
            project_marks: state.project_marks,
            project_sort: config.project_sort,
            show_hidden_projects: state.show_hidden_projects,
            show_all_projects: state.show_all_projects,
            recent_projects: state.recent_projects,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Change {
    /// The key of `config.toml` this changes; none for what `state.json` keeps.
    fn config_key(&self) -> Option<ConfigKey> {
        match self {
            Self::ZoomPercent(_) => Some(ConfigKey::ZoomPercent),
            Self::Look(_) => Some(ConfigKey::Look),
            Self::SendWith(_) => Some(ConfigKey::SendWith),
            Self::HanText(_) => Some(ConfigKey::HanText),
            Self::NewProjectsIn(_) => Some(ConfigKey::NewProjectsIn),
            Self::PeerMessages(_) => Some(ConfigKey::PeerMessages),
            Self::ProjectSort(_) => Some(ConfigKey::ProjectSort),
            Self::ComposerHeight(_)
            | Self::Trust(_)
            | Self::NotForClaudeCode(_)
            | Self::TogglePinProject(_)
            | Self::ToggleHideProject(_)
            | Self::RemoveProject(_)
            | Self::ShowHiddenProjects(_)
            | Self::ShowAllProjects(_)
            | Self::TouchProject { .. } => None,
        }
    }
}

/// The `settings.json` of earlier versions, read only to move what it holds. The keys it has no
/// place for any more (the sandbox's, gone on 2026-11-13; the sidebar's width) are read and
/// passed over.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Legacy {
    #[serde(rename = "sidebar_width_px")]
    _sidebar_width_px: serde::de::IgnoredAny,
    #[serde(rename = "sandbox")]
    _sandbox: serde::de::IgnoredAny,
    #[serde(rename = "sandbox_desktop")]
    _sandbox_desktop: serde::de::IgnoredAny,
    #[serde(rename = "sandbox_hardware")]
    _sandbox_hardware: serde::de::IgnoredAny,
    #[serde(rename = "sandbox_messages_unasked")]
    _sandbox_messages_unasked: serde::de::IgnoredAny,
    #[serde(rename = "sandbox_read")]
    _sandbox_read: serde::de::IgnoredAny,
    #[serde(rename = "sandbox_write")]
    _sandbox_write: serde::de::IgnoredAny,
    #[serde(default = "Settings::full_size")]
    zoom_percent: u16,
    trusted_folders: Vec<PathBuf>,
    composer_height_px: Option<u16>,
    new_projects_in: Option<PathBuf>,
    not_for_claude_code: Vec<PathBuf>,
    look: Look,
    send_with: SendKey,
    han_text: Option<CjkLanguage>,
    peer_messages: PeerMessages,
    pinned_projects: Vec<PathBuf>,
    hidden_projects: Vec<PathBuf>,
    project_sort: ProjectSort,
    show_hidden_projects: bool,
    show_all_projects: bool,
    recent_projects: Vec<RecentProject>,
}

#[cfg(not(target_arch = "wasm32"))]
impl From<Legacy> for Settings {
    fn from(legacy: Legacy) -> Self {
        // Pinning a folder unhid it, and hiding one unpinned it: a folder was in one list at most.
        let project_marks = legacy
            .hidden_projects
            .into_iter()
            .map(|folder| (folder, ProjectMark::Hidden))
            .chain(
                legacy
                    .pinned_projects
                    .into_iter()
                    .map(|folder| (folder, ProjectMark::Pinned)),
            )
            .collect();
        Self {
            zoom_percent: legacy.zoom_percent,
            trusted_folders: legacy.trusted_folders,
            composer_height_px: legacy.composer_height_px,
            new_projects_in: legacy.new_projects_in,
            not_for_claude_code: legacy.not_for_claude_code,
            look: legacy.look,
            send_with: legacy.send_with,
            han_text: legacy.han_text,
            peer_messages: legacy.peer_messages,
            project_marks,
            project_sort: legacy.project_sort,
            show_hidden_projects: legacy.show_hidden_projects,
            show_all_projects: legacy.show_all_projects,
            recent_projects: legacy.recent_projects,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no configuration folder to keep settings in")]
    NoConfigDir,
    #[error("no state folder to keep edualc's state in")]
    NoStateDir,
    #[error("couldn't read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} isn't TOML: {source}")]
    NotToml {
        path: PathBuf,
        #[source]
        source: toml_edit::TomlError,
    },
    #[error("{path} isn't configuration this version reads: {source}")]
    Config {
        path: PathBuf,
        #[source]
        source: toml_edit::de::Error,
    },
    #[error("{path} isn't what this version reads: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("couldn't put the configuration into TOML: {0}")]
    EncodeToml(#[source] toml_edit::ser::Error),
    #[error("couldn't put the state into JSON: {0}")]
    EncodeJson(#[source] serde_json::Error),
    #[error("couldn't write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Where the settings are kept.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone)]
struct Paths {
    config: PathBuf,
    state: PathBuf,
    legacy: PathBuf,
}

#[cfg(not(target_arch = "wasm32"))]
impl Paths {
    fn of_user() -> Result<Self, Error> {
        let config = dirs::config_dir().ok_or(Error::NoConfigDir)?.join("edualc");
        // macOS has no state folder: its application support folder, as for data.
        let state = dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .ok_or(Error::NoStateDir)?
            .join("edualc");
        Ok(Self {
            config: config.join("config.toml"),
            state: state.join("state.json"),
            legacy: config.join("settings.json"),
        })
    }
}

/// The settings as kept, with what `config.toml` holds as written, to change a key of.
#[cfg(not(target_arch = "wasm32"))]
enum Kept {
    /// In `config.toml` and `state.json`, or in neither.
    Split {
        settings: Settings,
        config: Option<toml_edit::DocumentMut>,
    },
    /// In the `settings.json` of an earlier version.
    Legacy(Settings),
}

#[cfg(not(target_arch = "wasm32"))]
impl Kept {
    fn into_settings(self) -> Settings {
        match self {
            Self::Split { settings, .. } | Self::Legacy(settings) => settings,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn read_optional(path: &Path) -> Result<Option<String>, Error> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn load_at(paths: &Paths) -> Result<Kept, Error> {
    let config_text = read_optional(&paths.config)?;
    let state_text = read_optional(&paths.state)?;
    if config_text.is_none()
        && state_text.is_none()
        && let Some(legacy) = read_optional(&paths.legacy)?
    {
        let legacy: Legacy = serde_json::from_str(&legacy).map_err(|source| Error::Json {
            path: paths.legacy.clone(),
            source,
        })?;
        return Ok(Kept::Legacy(legacy.into()));
    }
    let config_doc = config_text
        .map(|text| {
            text.parse::<toml_edit::DocumentMut>()
                .map_err(|source| Error::NotToml {
                    path: paths.config.clone(),
                    source,
                })
        })
        .transpose()?;
    let config = match &config_doc {
        Some(doc) => toml_edit::de::from_document(doc.clone()).map_err(|source| Error::Config {
            path: paths.config.clone(),
            source,
        })?,
        None => Config::default(),
    };
    let state = match state_text {
        Some(text) => serde_json::from_str(&text).map_err(|source| Error::Json {
            path: paths.state.clone(),
            source,
        })?,
        None => State::default(),
    };
    Ok(Kept::Split {
        settings: Settings::from_parts(config, state),
        config: config_doc,
    })
}

/// The kept settings; the defaults if none were kept yet.
#[cfg(not(target_arch = "wasm32"))]
pub fn load() -> Result<Settings, Error> {
    Ok(load_at(&Paths::of_user()?)?.into_settings())
}

/// Makes `change` to the kept settings and writes back the file it is in, unless a file
/// doesn't parse: what it holds (a hand edit with a typo, a later version's keys) would be
/// lost, and it stays until it is put right. Changes are made one at a time, by one owner
/// (edualcd).
#[cfg(not(target_arch = "wasm32"))]
pub fn change(change: Change) -> Result<Settings, Error> {
    change_at(&Paths::of_user()?, change)
}

#[cfg(not(target_arch = "wasm32"))]
fn change_at(paths: &Paths, change: Change) -> Result<Settings, Error> {
    let key = change.config_key();
    match load_at(paths)? {
        Kept::Legacy(mut settings) => {
            settings.apply(change);
            // Both, so neither the configuration nor the state is lost once one is there.
            let config =
                toml_edit::ser::to_document(&Config::from(&settings)).map_err(Error::EncodeToml)?;
            write_atomically(&paths.config, config.to_string().as_bytes())?;
            write_state(paths, &settings)?;
            Ok(settings)
        }
        Kept::Split {
            mut settings,
            config,
        } => {
            settings.apply(change);
            match key {
                Some(key) => {
                    let mut doc = config.unwrap_or_default();
                    set_key(&mut doc, key, &Config::from(&settings))?;
                    write_atomically(&paths.config, doc.to_string().as_bytes())?;
                }
                None => write_state(paths, &settings)?,
            }
            Ok(settings)
        }
    }
}

/// Puts `key` in `doc` as `config` has it, or takes it out if `config` has none, keeping the
/// comments around it.
#[cfg(not(target_arch = "wasm32"))]
fn set_key(doc: &mut toml_edit::DocumentMut, key: ConfigKey, config: &Config) -> Result<(), Error> {
    let name: &'static str = key.into();
    let mut fresh = toml_edit::ser::to_document(config).map_err(Error::EncodeToml)?;
    match fresh.remove(name).and_then(|item| item.into_value().ok()) {
        Some(mut value) => match doc.get_mut(name).and_then(toml_edit::Item::as_value_mut) {
            Some(written) => {
                *value.decor_mut() = written.decor().clone();
                *written = value;
            }
            None => {
                doc.insert(name, toml_edit::Item::Value(value));
            }
        },
        None => {
            doc.remove(name);
        }
    }
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
fn write_state(paths: &Paths, settings: &Settings) -> Result<(), Error> {
    let text = serde_json::to_string_pretty(&State::from(settings)).map_err(Error::EncodeJson)?;
    write_atomically(&paths.state, text.as_bytes())
}

#[cfg(not(target_arch = "wasm32"))]
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    if let Some(dir) = path.parent() {
        private_folder(dir).map_err(|source| Error::Write {
            path: dir.to_owned(),
            source,
        })?;
    }
    // Written beside the file and renamed over it, so a crash leaves the old file or the new
    // one, never half of one that no longer parses.
    let mut fresh_name = path.as_os_str().to_os_string();
    fresh_name.push(".new");
    let fresh = PathBuf::from(fresh_name);
    private_file(&fresh, bytes).map_err(|source| Error::Write {
        path: fresh.clone(),
        source,
    })?;
    std::fs::rename(&fresh, path).map_err(|source| Error::Write {
        path: path.to_path_buf(),
        source,
    })
}

/// `dir` and the folders above it that aren't there yet, made readable by the user alone.
#[cfg(not(target_arch = "wasm32"))]
fn private_folder(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

/// `path` written anew with `bytes`, readable by the user alone.
#[cfg(not(target_arch = "wasm32"))]
fn private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    std::io::Write::write_all(&mut options.open(path)?, bytes)
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    /// A folder of the test's own, with where the settings would be kept in it.
    fn scratch(label: &str) -> (PathBuf, Paths) {
        let dir = std::env::temp_dir().join(format!("edualc-{label}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch folder");
        let paths = Paths {
            config: dir.join("config").join("config.toml"),
            state: dir.join("state").join("state.json"),
            legacy: dir.join("config").join("settings.json"),
        };
        (dir, paths)
    }

    fn mode(path: &Path) -> u32 {
        let metadata = std::fs::metadata(path).expect("the file is there");
        std::os::unix::fs::PermissionsExt::mode(&metadata.permissions()) & 0o777
    }

    #[test]
    fn zoom_steps_within_its_range() {
        let mut settings = Settings::default();
        settings.zoom(Zoom::In);
        assert_eq!(settings.zoom_percent, 110);
        for _ in 0..20 {
            settings.zoom(Zoom::In);
        }
        assert_eq!(settings.zoom_percent, 200);
        settings.zoom(Zoom::Reset);
        for _ in 0..20 {
            settings.zoom(Zoom::Out);
        }
        assert_eq!(settings.zoom_percent, 70);
    }

    #[test]
    fn nothing_kept_reads_as_the_defaults() {
        let (dir, paths) = scratch("nothing-kept");
        let kept = load_at(&paths).expect("loads").into_settings();
        assert_eq!(kept, Settings::default());
        let config: Config = toml_edit::de::from_str("").expect("an empty file parses");
        assert_eq!(config, Config::default());
        std::fs::remove_dir_all(&dir).expect("cleaned up");
    }

    #[test]
    fn an_earlier_settings_json_moves_into_both_files() {
        let (dir, paths) = scratch("earlier-settings");
        let earlier = r#"{
            "zoom_percent": 120,
            "trusted_folders": ["/work/app"],
            "sidebar_width_px": 300,
            "sandbox": false,
            "sandbox_write": ["/srv/shared"],
            "look": "plain",
            "pinned_projects": ["/work/app"],
            "hidden_projects": ["/work/old"]
        }"#;
        std::fs::create_dir_all(paths.legacy.parent().expect("a parent")).expect("made");
        std::fs::write(&paths.legacy, earlier).expect("written");

        let read = load_at(&paths).expect("loads").into_settings();
        assert_eq!(read.zoom_percent, 120);
        assert_eq!(read.look, Look::Plain);
        assert_eq!(read.trusted_folders, [PathBuf::from("/work/app")]);
        assert_eq!(
            read.project_mark(Path::new("/work/app")),
            Some(ProjectMark::Pinned)
        );
        assert_eq!(
            read.project_mark(Path::new("/work/old")),
            Some(ProjectMark::Hidden)
        );

        let changed = change_at(&paths, Change::ShowAllProjects(true)).expect("moved");
        assert!(changed.show_all_projects);
        let config = std::fs::read_to_string(&paths.config).expect("config.toml written");
        assert!(config.contains("zoom_percent = 120"), "{config}");
        assert!(config.contains("look = \"plain\""), "{config}");
        assert!(!config.contains("sandbox"), "{config}");
        assert!(
            std::fs::read_to_string(&paths.state).is_ok_and(|state| !state.contains("sandbox"))
        );
        assert_eq!(load_at(&paths).expect("reloads").into_settings(), changed);
        std::fs::remove_dir_all(&dir).expect("cleaned up");
    }

    #[test]
    fn config_toml_keeps_its_comments() {
        let (dir, paths) = scratch("toml-comments");
        std::fs::create_dir_all(paths.config.parent().expect("a parent")).expect("made");
        let written = "\
# Mine
look = \"gruvbox\" # warm

# Enter for new lines
send_with = \"ctrl_enter\"
";
        std::fs::write(&paths.config, written).expect("written");

        change_at(&paths, Change::Trust(PathBuf::from("/work/app"))).expect("trusted");
        assert_eq!(
            std::fs::read_to_string(&paths.config).expect("read"),
            written
        );

        change_at(&paths, Change::Look(Look::Plain)).expect("changed");
        change_at(&paths, Change::NewProjectsIn(Some(PathBuf::from("/work")))).expect("set");
        assert_eq!(
            std::fs::read_to_string(&paths.config).expect("read"),
            "\
# Mine
look = \"plain\" # warm

# Enter for new lines
send_with = \"ctrl_enter\"
new_projects_in = \"/work\"
"
        );
        change_at(&paths, Change::NewProjectsIn(None)).expect("unset");
        assert!(
            std::fs::read_to_string(&paths.config).is_ok_and(|config| !config.contains("/work"))
        );
        std::fs::remove_dir_all(&dir).expect("cleaned up");
    }

    #[test]
    fn every_config_key_reads_back() {
        let (dir, paths) = scratch("config-keys");
        let changes = [
            Change::ZoomPercent(130),
            Change::Look(Look::Plain),
            Change::SendWith(SendKey::CtrlEnter),
            Change::HanText(CjkLanguage::Korean),
            Change::NewProjectsIn(Some(PathBuf::from("/work"))),
            Change::PeerMessages(PeerMessages::AskEachSender),
            Change::ProjectSort(ProjectSort::Recent),
        ];
        let mut expected = Settings::default();
        for change in changes {
            assert!(change.config_key().is_some(), "{change:?}");
            expected.apply(change.clone());
            change_at(&paths, change).expect("changed");
        }
        assert!(!paths.state.exists(), "only config.toml is written");
        assert_eq!(load_at(&paths).expect("reloads").into_settings(), expected);
        std::fs::remove_dir_all(&dir).expect("cleaned up");
    }

    #[test]
    fn a_file_that_doesnt_parse_isnt_written_over() {
        let (dir, paths) = scratch("bad-toml");
        std::fs::create_dir_all(paths.config.parent().expect("a parent")).expect("made");
        let typo = "look = \"plain\"\nzooom_percent = 120\n";
        std::fs::write(&paths.config, typo).expect("written");
        assert!(matches!(
            change_at(&paths, Change::Look(Look::Gruvbox)),
            Err(Error::Config { .. })
        ));
        let broken = "look = \"plain\n";
        std::fs::write(&paths.config, broken).expect("written");
        assert!(matches!(
            change_at(&paths, Change::Look(Look::Gruvbox)),
            Err(Error::NotToml { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(&paths.config).ok().as_deref(),
            Some(broken)
        );
        std::fs::remove_file(&paths.config).expect("removed");
        change_at(&paths, Change::Look(Look::Plain)).expect("a missing file is written");
        std::fs::remove_dir_all(&dir).expect("cleaned up");
    }

    #[test]
    fn changes_from_two_windows_are_both_kept() {
        let (dir, paths) = scratch("two-windows");
        change_at(&paths, Change::Trust(PathBuf::from("/work/app"))).expect("trusted");
        change_at(&paths, Change::ZoomPercent(120)).expect("zoomed");
        change_at(&paths, Change::Trust(PathBuf::from("/work/app"))).expect("again");
        let kept = load_at(&paths).expect("reloads").into_settings();
        assert_eq!(kept.trusted_folders, [PathBuf::from("/work/app")]);
        assert_eq!(kept.zoom_percent, 120);
        assert!(!paths.config.with_extension("toml.new").exists());
        assert!(!paths.state.with_extension("json.new").exists());
        assert_eq!(mode(&paths.config), 0o600);
        assert_eq!(mode(&paths.state), 0o600);
        std::fs::remove_dir_all(&dir).expect("cleaned up");
    }

    #[test]
    fn a_misspelled_key_is_refused() {
        assert!(toml_edit::de::from_str::<Config>("send_wit = \"enter\"").is_err());
        assert!(toml_edit::de::from_str::<Config>("send_with = \"ctrl_enter\"").is_ok());
        assert!(serde_json::from_str::<State>(r#"{"trusted_foldres": ["/tmp"]}"#).is_err());
        assert!(serde_json::from_str::<State>(r#"{"trusted_folders": ["/tmp"]}"#).is_ok());
    }

    #[test]
    fn a_folder_has_one_mark_at_most() {
        let mut settings = Settings::default();
        let alpha = PathBuf::from("/work/alpha");
        settings.apply(Change::TogglePinProject(alpha.clone()));
        assert_eq!(settings.project_mark(&alpha), Some(ProjectMark::Pinned));
        settings.apply(Change::ToggleHideProject(alpha.clone()));
        assert_eq!(settings.project_mark(&alpha), Some(ProjectMark::Hidden));
        settings.apply(Change::ToggleHideProject(alpha.clone()));
        assert_eq!(settings.project_mark(&alpha), None);
        settings.apply(Change::TogglePinProject(alpha.clone()));
        settings.apply(Change::RemoveProject(alpha.clone()));
        assert_eq!(settings.project_mark(&alpha), Some(ProjectMark::Removed));
        settings.apply(Change::TogglePinProject(alpha.clone()));
        assert_eq!(settings.project_mark(&alpha), Some(ProjectMark::Pinned));
    }

    #[test]
    fn a_removed_folder_comes_back_when_opened() {
        let mut settings = Settings::default();
        let alpha = PathBuf::from("/work/alpha");
        let beta = PathBuf::from("/work/beta");
        for (path, opened_secs) in [(&alpha, 10), (&beta, 20), (&alpha, 30)] {
            settings.apply(Change::TouchProject {
                path: path.clone(),
                opened_secs,
            });
        }
        let opened = |settings: &Settings| -> Vec<(PathBuf, u64)> {
            settings
                .recent_projects
                .iter()
                .map(|recent| (recent.path.clone(), recent.opened_secs))
                .collect()
        };
        assert_eq!(opened(&settings), [(alpha.clone(), 30), (beta.clone(), 20)]);

        settings.apply(Change::RemoveProject(beta.clone()));
        assert!(settings.is_removed(&beta));
        assert_eq!(opened(&settings), [(alpha.clone(), 30)]);

        settings.apply(Change::TouchProject {
            path: beta.clone(),
            opened_secs: 40,
        });
        assert_eq!(settings.project_mark(&beta), None);
        assert_eq!(opened(&settings), [(beta, 40), (alpha, 30)]);
    }

    #[test]
    fn opening_a_pinned_folder_keeps_it_pinned() {
        let mut settings = Settings::default();
        let alpha = PathBuf::from("/work/alpha");
        settings.apply(Change::TogglePinProject(alpha.clone()));
        settings.apply(Change::TouchProject {
            path: alpha.clone(),
            opened_secs: 10,
        });
        assert_eq!(settings.project_mark(&alpha), Some(ProjectMark::Pinned));
    }
}
