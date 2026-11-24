//! What the window shows, apart from how it is drawn, handed to a [`Frontend`] that draws it.

use std::collections::HashSet;

use crate::chat::ImageKey;
use crate::decode::Pixels;

pub mod model;
mod presenter;

pub(crate) use presenter::clock;
pub use presenter::{OverviewView, Presenter, short_count, short_duration};

/// Text the window sets: plain, or with inline Markdown (emphasis, code, links) for the frontend
/// to style.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Styled {
    #[default]
    Empty,
    Plain(String),
    Markdown(String),
}

impl Styled {
    pub fn plain(text: &str) -> Self {
        Self::Plain(text.to_owned())
    }

    pub fn markdown(text: &str) -> Self {
        Self::Markdown(text.to_owned())
    }
}

/// An image the window shows: one of the conversation's by its key, whose pixels come later
/// through [`Frontend::show_image`], or pixels at hand such as a thumbnail.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Image {
    #[default]
    None,
    Key(ImageKey),
    Pixels(Pixels),
}

/// The window's look, as the settings keep it.
pub use crate::settings::Look;

/// A change to the transcript's rows. They change one by one as the conversation goes, so they
/// aren't part of a global's state.
#[derive(Debug, Clone, PartialEq)]
pub enum EntriesChange {
    /// Replaces every row, as for another session.
    Reset(Vec<model::Entry>),
    Push(model::Entry),
    /// Replaces row `index`; a frontend keeps what of it stays, such as its blocks' selection.
    Set(usize, model::Entry),
}

/// What the window does on its own desktop, which is yours; edualcd may run where nobody sits.
/// A failure comes back as `Event::SeatFailed`, except a notification's: a desktop without a
/// notification service would show an error for each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seat {
    /// Opens a web page or mail in the desktop's browser; anything else is refused (see
    /// `desktop::is_web_link`).
    OpenUrl(String),
    /// Shows a notification. Its texts come from other sessions' records: show them as text.
    Notify { summary: String, body: String },
    /// Reads the clipboard's image; answered by `Event::Pasted`.
    PasteImage,
    /// Asks the desktop's folder picker for a folder; answered by `Event::Picked`.
    PickFolder(crate::ipc::ActId),
    /// Brings forward the window of the program that runs process `pid`.
    FocusWindowOf { pid: u32 },
}

/// What the window is told to do rather than show: state the app sets now and then (the
/// draft, a menu's choice), focus, and moving about.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Copy(String),
    SetDraft(String),
    /// Adds text to the end of the draft.
    AppendDraft(String),
    FocusComposer,
    FocusSide,
    /// Clears the transcript's text selection, on moving elsewhere.
    ClearSelection,
    ScrollToEnd,
    /// Shows a loaded transcript at its end, once the list has measured its way there.
    OpenAtEnd,
    /// Opens the list of what runs.
    ShowTasks,
    /// Closes the folder list, as the folder typed in it opened.
    ProjectsOpened,
    /// Closes the New project dialog, as the folder was made.
    NewProjectCreated,
    /// Focuses the start page or the project page shown.
    FocusPage,
    /// Opens or closes the session switcher inside a session.
    ToggleSwitcher,
    /// Opens the New project dialog from the start page.
    OpenNewProject,
    /// Sets the model the menu has chosen.
    ModelIndex(i32),
    /// Sets the mode the menu has chosen.
    ModeIndex(i32),
    /// Sets whether the plan shown is being reviewed.
    Reviewing(bool),
    /// Sets the composer's height in logical pixels; `None` fits it to its text.
    ComposerHeight(Option<u16>),
    /// Sets the base text size in logical pixels, which everything else is sized from.
    TextSize(f32),
    Look(Look),
    /// Colours the code shown again, after the look or the desktop's dark or light changed.
    /// Only a window that colours code acts on it.
    RecolourCode,
    /// Draws the code the window's own thread coloured (`Event::CodeColoured`). A window that
    /// doesn't colour code is never told.
    DrawColouredCode,
    Quit,
    /// Tells which session the window shows; `None` for a new one not started yet. A frontend
    /// that its system ends and starts again, such as an app, opens it again.
    SessionShown(Option<crate::ids::SessionId>),
}

/// Draws the window. It is told the state of each part as it changes, with what it was told
/// before, and what to do.
pub trait Frontend {
    fn nav(&mut self, old: Option<&model::Nav>, nav: &model::Nav);
    fn session(&mut self, old: Option<&model::Session>, session: &model::Session);
    fn composer(&mut self, old: Option<&model::Composer>, composer: &model::Composer);
    fn sessions(&mut self, old: Option<&model::Sessions>, sessions: &model::Sessions);
    fn entries(&mut self, change: EntriesChange);
    fn prompt(&mut self, old: Option<&model::Prompt>, prompt: &model::Prompt);
    fn peek(&mut self, old: Option<&model::Peek>, peek: &model::Peek);
    fn changes(&mut self, old: Option<&model::Changes>, changes: &model::Changes);
    fn side(&mut self, old: Option<&model::Side>, side: &model::Side);
    fn preferences(&mut self, old: Option<&model::Preferences>, preferences: &model::Preferences);
    fn usage(&mut self, old: Option<&model::Usage>, usage: &model::Usage);
    /// Shows the newest prompt, pinned above the conversation.
    fn transcript(&mut self, old: Option<&model::Transcript>, transcript: &model::Transcript);
    /// Shows image `key`'s pixels, or why it can't be shown.
    fn show_image(&mut self, key: ImageKey, shown: Result<&Pixels, String>);
    /// Lets go of every image but those in `shown`, after the conversation was replaced.
    fn keep_images(&mut self, shown: &HashSet<ImageKey>);
    fn command(&mut self, command: Command);
    /// Does something on the desktop the frontend is on (see [`Seat`]).
    fn seat(&mut self, _seat: Seat) {}
    /// Returns what the composer holds now.
    fn draft(&self) -> String;
}
