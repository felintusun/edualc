//! The app's state made into what the window shows, handed to the frontend once per batch, each
//! part only when it changed.

mod changes_view;
mod menus;
mod overview_page;
mod prompt;
mod tasks;
mod transcript;
mod usage;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::model::{
    self, Activity, Cjk, CjkChars, Entry, HanLanguage, ImageChip, ImageState, LiveState, MdBlock,
    PeerMessages, PlaceView, SendKey, SessionRow, SessionRowKind, ShownImage, SideExchange,
    SideState, SignInState, StatRow,
};
use super::{Command, Frontend, Image, Look};
use crate::app::{
    Answer, ApiKeyKept, Doing, LimitUse, LimitWait, SideChat, SideView, SignIn, Status, Watch,
};
use crate::chat::{Chat, ImageKey};
use crate::cjk::{CjkLanguage, CjkText, Standing};
use crate::decode::{Picture, Pixels};
use crate::grouping::Grouping;
use crate::ids::RequestId;
use crate::ipc::Runs;
use crate::markdown;
use crate::sessions::{self, LiveStatus, Worktree};
use crate::settings;
use crate::sidebar;
use crate::tools::shortened_path;

pub use overview_page::OverviewView;
pub use tasks::short_count;
use transcript::{failed_image, md_block};
pub(crate) use usage::clock;
pub use usage::short_duration;
use usage::{fraction_of, provider_name, reset_text};

/// Long outputs are cut to this many lines in the window; copying takes all of it.
const OUTPUT_LINES: usize = 80;
const OUTPUT_CHARS: usize = 20_000;

/// The state of each of the window's parts, as shown last or to be shown.
#[derive(Debug, Clone, Default, PartialEq)]
struct Parts {
    nav: model::Nav,
    session: model::Session,
    composer: model::Composer,
    sessions: model::Sessions,
    prompt: model::Prompt,
    peek: model::Peek,
    changes: model::Changes,
    side: model::Side,
    preferences: model::Preferences,
    usage: model::Usage,
    transcript: model::Transcript,
}

pub struct Presenter<F: Frontend> {
    frontend: F,
    nav: model::Nav,
    session: model::Session,
    composer: model::Composer,
    sessions: model::Sessions,
    prompt: model::Prompt,
    peek: model::Peek,
    changes: model::Changes,
    side: model::Side,
    preferences: model::Preferences,
    usage: model::Usage,
    transcript: model::Transcript,
    /// What the frontend was told last, for telling it only what changed.
    shown: Option<Parts>,
    /// The rows of the transcript as the frontend has them.
    entries: Vec<Entry>,
    /// How each entry was shown last with respect to runs of tool calls.
    groups: Vec<Grouping>,
    /// The request the bar shows.
    prompt_shown: Option<RequestId>,
    /// What the menus hold, set again only when it changes (setting closes an open menu).
    models: Vec<(String, String)>,
    modes: Vec<(String, String)>,
    /// Which model and mode the menus have chosen, told with the next flush.
    menu_choice: Option<(i32, i32)>,
    /// Whether the plan is reviewed, told with the next flush (after the plan itself).
    reviewing: Option<bool>,
    /// The conversation's images, as far as they are decoded; one not here is still decoded.
    images: HashMap<ImageKey, ShownImage>,
    /// The language text with Han characters alone is set in (see [`crate::cjk`]).
    han: CjkLanguage,
}

impl<F: Frontend> Presenter<F> {
    pub fn new(frontend: F, finds_windows: bool) -> Self {
        let session = model::Session {
            can_go_to_window: finds_windows,
            limit_five_hour: -1.0,
            limit_seven_day: -1.0,
            title: "New session".to_owned(),
            agent: "Claude".to_owned(),
            agent_name: "Claude".to_owned(),
            program: "Claude Code".to_owned(),
            shell_commands: true,
            can_shell: true,
            can_btw: true,
            can_fork: true,
            ..model::Session::default()
        };
        Self {
            frontend,
            nav: model::Nav::default(),
            session,
            composer: model::Composer::default(),
            sessions: model::Sessions::default(),
            prompt: model::Prompt::default(),
            peek: model::Peek::default(),
            changes: model::Changes {
                can_send: true,
                ..model::Changes::default()
            },
            side: model::Side::default(),
            preferences: model::Preferences::default(),
            usage: model::Usage::default(),
            // Nothing is pinned before the first prompt.
            transcript: model::Transcript {
                pinned_index: -1,
                ..model::Transcript::default()
            },
            shown: None,
            entries: Vec::new(),
            groups: Vec::new(),
            prompt_shown: None,
            models: Vec::new(),
            modes: Vec::new(),
            menu_choice: None,
            reviewing: None,
            images: HashMap::new(),
            han: CjkLanguage::Japanese,
        }
    }

    /// Tells the frontend what changed since it was told last.
    pub fn flush(&mut self) {
        let first = self.shown.is_none();
        let shown = self.shown.get_or_insert_with(Parts::default);
        // Each part told and kept again only when it differs from what was shown.
        macro_rules! tell {
            ($($part:ident),*) => {$(
                if first || shown.$part != self.$part {
                    let old = (!first).then_some(&shown.$part);
                    self.frontend.$part(old, &self.$part);
                    shown.$part.clone_from(&self.$part);
                }
            )*};
        }
        tell!(
            nav,
            session,
            composer,
            sessions,
            prompt,
            peek,
            changes,
            side,
            preferences,
            usage,
            transcript
        );
        if let Some((model, mode)) = self.menu_choice.take() {
            self.frontend.command(Command::ModelIndex(model));
            self.frontend.command(Command::ModeIndex(mode));
        }
        if let Some(reviewing) = self.reviewing.take() {
            self.frontend.command(Command::Reviewing(reviewing));
        }
    }

    pub fn command(&mut self, command: Command) {
        self.frontend.command(command);
    }

    /// Sets the language of text with Han characters alone, from the next row drawn on, and
    /// the settings page's choice.
    pub fn set_han_language(&mut self, han: CjkLanguage) {
        self.han = han;
        self.preferences.han_language = match han {
            CjkLanguage::Japanese => HanLanguage::Japanese,
            CjkLanguage::SimplifiedChinese => HanLanguage::SimplifiedChinese,
            CjkLanguage::TraditionalChinese => HanLanguage::TraditionalChinese,
            CjkLanguage::Korean => HanLanguage::Korean,
        };
    }

    /// Shows image `key`'s pixels, or why it can't be shown.
    pub fn show_image(&mut self, key: ImageKey, shown: Result<&Pixels, String>) {
        let row = match &shown {
            Ok(pixels) if pixels.is_whole() => ShownImage {
                state: ImageState::Shown,
                image: Image::Key(key),
                width_px: i32::try_from(pixels.width_px).unwrap_or(i32::MAX),
                height_px: i32::try_from(pixels.height_px).unwrap_or(i32::MAX),
                note: String::new(),
            },
            Ok(_) => failed_image("its pixels don't add up".to_owned()),
            Err(why) => failed_image(why.clone()),
        };
        self.images.insert(key, row);
        self.frontend.show_image(key, shown);
    }

    /// Keeps only the images in `shown`, after the conversation was replaced.
    pub fn keep_images(&mut self, shown: &HashSet<ImageKey>) {
        self.images.retain(|key, _| shown.contains(key));
        self.frontend.keep_images(shown);
    }

    fn shown_images(&self, keys: &[ImageKey]) -> Vec<ShownImage> {
        keys.iter()
            .map(|key| self.images.get(key).cloned().unwrap_or_default())
            .collect()
    }

    /// Sets the base text size in logical pixels, which everything else is sized from.
    pub fn set_text_size(&mut self, pixels: f32) {
        self.command(Command::TextSize(pixels));
    }

    pub fn set_send_key(&mut self, key: settings::SendKey) {
        self.preferences.send_key = match key {
            settings::SendKey::Enter => SendKey::Enter,
            settings::SendKey::CtrlEnter => SendKey::CtrlEnter,
        };
    }

    pub fn set_peer_messages(&mut self, peer_messages: settings::PeerMessages) {
        self.preferences.peer_messages = match peer_messages {
            settings::PeerMessages::AskEachSender => PeerMessages::AskEachSender,
            settings::PeerMessages::LetAllIn => PeerMessages::LetAllIn,
        };
    }

    /// Sets how many sessions a shutdown would end.
    pub fn set_running_sessions(&mut self, count: usize) {
        self.preferences.running_sessions = i32::try_from(count).unwrap_or(i32::MAX);
    }

    pub fn set_look(&mut self, look: Look) {
        self.command(Command::Look(look));
    }

    /// Shows the images going with the next message as chips in the composer. Their
    /// thumbnails are the decoder's pixels, not decoded again here.
    pub fn render_attached(&mut self, attached: &[Picture]) {
        self.composer.images = attached
            .iter()
            .filter(|picture| picture.thumbnail.is_whole())
            .map(|picture| ImageChip {
                thumbnail: Image::Pixels(picture.thumbnail.clone()),
                caption: format!("{}×{}", picture.width_px, picture.height_px),
            })
            .collect();
    }

    /// Sets the composer's height as dragged; `None` fits it to its text.
    pub fn set_composer_height(&mut self, height_px: Option<u16>) {
        self.command(Command::ComposerHeight(height_px));
    }

    pub fn copy(&mut self, text: &str) {
        self.command(Command::Copy(text.to_owned()));
    }

    /// Has the frontend act on its desktop at once.
    pub fn seat(&mut self, seat: super::Seat) {
        self.frontend.seat(seat);
    }

    pub fn set_draft(&mut self, text: &str) {
        self.command(Command::SetDraft(text.to_owned()));
    }

    pub fn draft(&self) -> String {
        self.frontend.draft()
    }

    /// Sets whether Up can bring an older prompt back, and whether one is in the draft now.
    pub fn set_recall(&mut self, can_recall_older: bool, recalling: bool) {
        self.composer.can_recall_older = can_recall_older;
        self.composer.recalling = recalling;
    }

    /// Sets whether the composer shows the draft as a shell command.
    pub fn set_shell_mode(&mut self, shell: bool) {
        self.composer.shell = shell;
    }

    /// Sets the hint after the command in the draft for what it takes; empty for none.
    pub fn set_argument_hint(&mut self, hint: &str) {
        hint.clone_into(&mut self.composer.argument_hint);
    }

    /// Adds `text` to the end of the draft.
    pub fn append_draft(&mut self, text: &str) {
        self.command(Command::AppendDraft(text.to_owned()));
    }

    pub fn focus_side(&mut self) {
        self.command(Command::FocusSide);
    }

    pub fn render_side(&mut self, chat: &SideChat, can_ask: bool, can_branch: bool) {
        let han = self.han;
        let side = &mut self.side;
        side.view = match chat.view {
            SideView::Hidden => model::SideView::Hidden,
            SideView::Shown => model::SideView::Shown,
            SideView::Folded { .. } => model::SideView::Folded,
        };
        side.exchanges = chat
            .exchanges
            .iter()
            .map(|exchange| {
                let (state, answer, error) = match &exchange.answer {
                    Answer::Pending => (SideState::Pending, Vec::new(), ""),
                    Answer::Retrying => (SideState::Retrying, Vec::new(), ""),
                    Answer::Answered(text) => {
                        let blocks: Vec<MdBlock> = markdown::blocks(text)
                            .iter()
                            .map(|block| md_block(block, han))
                            .collect();
                        (SideState::Answered, blocks, "")
                    }
                    Answer::Failed(error) => (SideState::Failed, Vec::new(), error.as_str()),
                };
                SideExchange {
                    question: exchange.question.clone(),
                    state,
                    answer,
                    error: error.to_owned(),
                }
            })
            .collect();
        side.unseen = chat.view == SideView::Folded { unseen: true };
        side.can_ask = can_ask;
        side.can_branch = can_branch;
        side.has_answers = chat.has_answers();
        side.pending = chat.pending();
    }

    pub fn focus_composer(&mut self) {
        self.command(Command::FocusComposer);
    }

    pub fn scroll_to_end(&mut self) {
        self.command(Command::ScrollToEnd);
    }

    /// Shows whether the keyring keeps an API key, what it last said, and the title of a
    /// session waiting for you to pick what it signs in with.
    pub fn render_api_key(&mut self, kept: ApiKeyKept, note: &str, choosing: Option<&str>) {
        let kept = kept == ApiKeyKept::Kept;
        self.preferences.api_key_kept = kept;
        note.clone_into(&mut self.preferences.api_key_note);
        self.sessions.api_key_kept = kept;
        choosing
            .unwrap_or_default()
            .clone_into(&mut self.sessions.choosing_sign_in);
    }

    /// Pins your newest message above the conversation, on one line.
    pub fn render_pinned(&mut self, chat: &Chat) {
        /// More than a wide window shows on the pinned line.
        const PINNED_CHARS: usize = 400;
        let transcript = &mut self.transcript;
        let Some((index, text)) = chat.last_prompt() else {
            transcript.pinned_index = -1;
            transcript.pinned_text.clear();
            return;
        };
        let line: String = text
            .split_whitespace()
            .flat_map(|word| [" ", word])
            .skip(1)
            .flat_map(str::chars)
            .take(PINNED_CHARS)
            .collect();
        // Its place in the list, which leaves out the later calls of folded runs.
        let row = self
            .entries
            .iter()
            .take(index)
            .filter(|entry| !entry.hidden)
            .count();
        transcript.pinned_index = index_for_slint(index);
        transcript.pinned_row = index_for_slint(row);
        transcript.pinned_text = line;
    }

    /// Shows a loaded transcript at its end, once the list has measured its way there.
    pub fn open_at_end(&mut self) {
        self.command(Command::OpenAtEnd);
    }

    /// Shows what the session is doing: whether something runs that Stop stops (a turn, or a
    /// `!` command, which an interrupt stops too), the progress line, and the status bar's
    /// word for it.
    pub fn render_activity(&mut self, status: &Status, waiting: bool, shell_running: bool) {
        let session = &mut self.session;
        // Stop and Esc cancel a wait for the usage limit too.
        let limit_wait = match status.limit_wait {
            LimitWait::Idle => None,
            LimitWait::Asking => Some("Usage limit reached".to_owned()),
            LimitWait::Until(at) => {
                Some(format!("Usage limit reached · continuing at {}", clock(at)))
            }
        };
        session.busy = status.turns.under_way() || shell_running || limit_wait.is_some();
        session.turn = status.turns.under_way();
        session.activity = match status.doing {
            Doing::Nothing if shell_running => Activity::Tool,
            Doing::Nothing => Activity::Idle,
            Doing::Starting
            | Doing::Thinking
            | Doing::ThinkingTokens(_)
            | Doing::Writing
            | Doing::Working
            | Doing::Compacting
            | Doing::Retrying(_)
            | Doing::Silent(_) => Activity::Model,
            Doing::Preparing(_) | Doing::Running(_) | Doing::Calling(_) => Activity::Tool,
        };
        let doing = if waiting {
            "Waiting for you".to_owned()
        } else if let (Doing::Nothing, Some(limit_wait)) = (&status.doing, limit_wait) {
            limit_wait
        } else if matches!(status.doing, Doing::Nothing) && shell_running {
            "Running your command".to_owned()
        } else {
            status.doing.to_string()
        };
        session.status = doing;
    }

    /// Shows the session's status. `asking` holds what the folder would run while you are asked
    /// to trust it; `asks_claude_trust` asks whether to trust a folder trusted here for Claude
    /// Code too.
    pub fn render_status(
        &mut self,
        status: &Status,
        asking: Option<&[Runs]>,
        asks_claude_trust: bool,
        cwd: &Path,
        watch: Option<&Watch>,
        worktree: Option<&Worktree>,
    ) {
        let session = &mut self.session;
        session.cwd = shortened_path(cwd, 36);
        let project = sessions::folder_name(cwd);
        session.project = project;
        session.title = status.title.as_deref().unwrap_or("New session").into();
        session.sign_in = match status.sign_in {
            SignIn::SignedIn => SignInState::SignedIn,
            SignIn::Refused { .. } => SignInState::SignedOut,
            SignIn::Unknown | SignIn::Asked => SignInState::Unknown,
        };
        session.shows_sign_in = status.sign_in.waits_for_user();
        let (help, command) = status.backend.sign_in_help();
        help.clone_into(&mut session.sign_in_help);
        command.clone_into(&mut session.sign_in_command);
        session.needs_trust = asking.is_some();
        session.asks_claude_trust = asks_claude_trust;
        session.trust_runs = asking.unwrap_or_default().iter().map(run_line).collect();
        status.backend.agent().clone_into(&mut session.agent);
        status.backend.agent().clone_into(&mut session.agent_name);
        status.backend.program().clone_into(&mut session.program);
        let caps = status.capabilities();
        session.shell_commands = caps.inline_shell;
        session.can_shell = caps.inline_shell;
        session.can_btw = caps.side_questions;
        session.can_fork = caps.fork;
        self.sessions.program = program_shown(crate::provider::Program::of(
            status.backend,
            status.sign_in_with,
        ));
        let (context, fraction) = match (status.context_tokens, status.context_window) {
            (0, _) => (String::new(), 0.0),
            (used, 0) => (short_count(used), 0.0),
            (used, window) => (
                format!("{} / {}", short_count(used), short_count(window)),
                fraction_of(used, window),
            ),
        };
        session.context = context;
        session.context_fraction = fraction;
        let cost = if status.cost_usd > 0.0 {
            format!("${:.2}", status.cost_usd)
        } else {
            String::new()
        };
        session.cost = cost;
        // Unknown shows as nothing (less than 0), not as an empty bar.
        let used = |limit: Option<LimitUse>| limit.map_or(-1.0, |limit| limit.used);
        let resets = |limit: Option<LimitUse>| {
            limit
                .and_then(|limit| limit.resets_at)
                .map_or_else(String::new, |at| reset_text(at, false))
        };
        let limits = status.plan_limits().unwrap_or_default();
        session.limit_five_hour = used(limits.five_hour);
        session.limit_five_hour_resets = resets(limits.five_hour);
        session.limit_seven_day = used(limits.seven_day);
        // What stands where the bars would, without any.
        status
            .backend
            .limits_note(status.sign_in_with)
            .clone_into(&mut session.limits_note);
        let account = &status.account;
        session.email = account.email.as_str().into();
        session.organization = account.organization.as_str().into();
        session.provider = provider_name(&account.api_provider).into();
        session.version = status.version.as_str().into();
        session.worktree_name = worktree
            .map_or("", |worktree| worktree.name.as_str())
            .into();
        session.worktree_branch = worktree
            .map_or("", |worktree| worktree.branch.as_str())
            .into();
        session.watching = watch.is_some();
        if let Some(watch) = watch {
            let doing = match watch.session.status {
                LiveStatus::Busy => "working",
                LiveStatus::Waiting => "waiting for its user",
                LiveStatus::Idle => "idle",
                LiveStatus::Other => "running",
            };
            let text = format!(
                "{} runs in another {} window ({doing}). This view follows it.",
                watch.session.name,
                watch.session.backend.program()
            );
            session.watch_text = text;
        }
    }

    /// Shows the session list (see [`crate::sidebar`]); `filtered` says why it may be empty.
    pub fn render_sidebar(&mut self, rows: &[sidebar::Row], filtered: bool) {
        let items: Vec<SessionRow> = rows.iter().map(session_row).collect();
        let empty = match (items.is_empty(), filtered) {
            (false, _) => "",
            (true, true) => "No matches.",
            (true, false) => "No sessions yet.",
        };
        let sessions = &mut self.sessions;
        sessions.empty = empty.into();
        sessions.rows = items;
    }

    pub fn render_nav(
        &mut self,
        view: PlaceView,
        project_name: &str,
        project_folder: &str,
        session_title: &str,
        can_back: bool,
    ) {
        self.nav.view = view;
        self.nav.project_name = project_name.into();
        self.nav.project_folder = project_folder.into();
        self.nav.session_title = session_title.into();
        self.nav.can_back = can_back;
    }
}

/// Returns a text's CJK characters (see [`crate::cjk`]), for the window to set the text by.
fn cjk(text: &str, han: CjkLanguage) -> CjkChars {
    CjkText::of(text, han).map_or_else(CjkChars::default, |cjk| CjkChars {
        language: match cjk.language {
            CjkLanguage::Japanese => Cjk::Japanese,
            CjkLanguage::SimplifiedChinese => Cjk::SimplifiedChinese,
            CjkLanguage::TraditionalChinese => Cjk::TraditionalChinese,
            CjkLanguage::Korean => Cjk::Korean,
        },
        main: match cjk.standing {
            Standing::Main => true,
            Standing::Guest => false,
        },
    })
}

fn program_shown(program: crate::provider::Program) -> model::Program {
    match program {
        crate::provider::Program::Claude => model::Program::Claude,
        crate::provider::Program::ClaudeApiKey => model::Program::ClaudeApiKey,
        crate::provider::Program::Agy => model::Program::Agy,
        crate::provider::Program::Opencode => model::Program::Opencode,
    }
}

fn live_state(state: Option<LiveStatus>) -> LiveState {
    match state {
        Some(LiveStatus::Idle) => LiveState::Idle,
        Some(LiveStatus::Busy) => LiveState::Busy,
        Some(LiveStatus::Waiting) => LiveState::Waiting,
        Some(LiveStatus::Other) | None => LiveState::Other,
    }
}

/// Returns a row of the session list as the sidebar shows it.
fn session_row(row: &sidebar::Row) -> SessionRow {
    match row {
        sidebar::Row::RunningHeading { running, waiting } => SessionRow {
            kind: SessionRowKind::Heading,
            title: format!("Running · {running}"),
            detail: match waiting {
                0 => String::new(),
                1 => "1 needs you".into(),
                waiting => format!("{waiting} need you"),
            },
            ..SessionRow::default()
        },
        sidebar::Row::EarlierHeading { folder } => SessionRow {
            kind: SessionRowKind::Heading,
            title: format!("Earlier in {folder}"),
            ..SessionRow::default()
        },
        sidebar::Row::Running(running) => SessionRow {
            kind: SessionRowKind::Running,
            id: running.id.as_str().into(),
            title: running.title.as_str().into(),
            detail: running_detail(running),
            state: live_state(Some(running.state)),
            selected: running.shown,
            worktree: running.worktree.as_deref().unwrap_or_default().into(),
            agents: index_for_slint(running.agents),
            program: program_shown(running.program),
            ..SessionRow::default()
        },
        sidebar::Row::Earlier(earlier) => SessionRow {
            kind: SessionRowKind::Earlier,
            id: earlier.id.as_str().into(),
            title: earlier.title.as_str().into(),
            age: sessions::age(earlier.modified),
            selected: earlier.shown,
            worktree: earlier.worktree.as_deref().unwrap_or_default().into(),
            program: program_shown(crate::provider::Program::of(
                earlier.backend,
                crate::provider::SignInWith::Account,
            )),
            ..SessionRow::default()
        },
    }
}

/// Returns a running session's second line: what it does, the name to message it by unless
/// its title is that name, and its worktree.
fn running_detail(running: &sidebar::Running) -> String {
    let doing = match running.state {
        LiveStatus::Waiting => "Needs you",
        LiveStatus::Busy => "Working",
        LiveStatus::Idle => "Idle",
        LiveStatus::Other => "Running",
    };
    let worktree = running
        .worktree
        .as_ref()
        .map(|worktree| format!("⎇ {worktree}"));
    [
        Some(doing),
        running.name_beside_title(),
        worktree.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<&str>>()
    .join(" · ")
}

fn stat(label: &str, value: String, detail: String, fraction: f32) -> StatRow {
    StatRow {
        label: label.into(),
        value,
        detail,
        fraction,
    }
}

/// Returns a line of what a folder would run, as the trust question lists it.
fn run_line(runs: &Runs) -> String {
    match runs {
        Runs::Hook {
            event,
            matcher,
            action,
        } => {
            let does = match action {
                crate::ipc::HookAction::Command(command) => command.clone(),
                crate::ipc::HookAction::Url(url) => format!("sends it to {url}"),
                crate::ipc::HookAction::McpTool { server, tool } => {
                    format!("calls the MCP tool {tool} of {server}")
                }
                crate::ipc::HookAction::Prompt => "asks a model".to_owned(),
            };
            if matcher.is_empty() {
                format!("Hook on {event}: {does}")
            } else {
                format!("Hook on {event} ({matcher}): {does}")
            }
        }
        Runs::McpServer { name, runs } => format!("MCP server {name}: {runs}"),
        Runs::ApiKeyHelper(command) => format!("API key helper: {command}"),
        Runs::StatusLine(command) => format!("Status line: {command}"),
        Runs::Plugin(name) => format!("Plugin {name}"),
        Runs::Environment(names) => format!("Environment variables: {}", names.join(", ")),
        Runs::Allows(rules) => format!("Allowed without asking: {}", rules.join(", ")),
        Runs::Directories(folders) => format!("Also works in: {}", folders.join(", ")),
        Runs::DefaultMode(mode) => format!("Starts in the permission mode {mode}"),
        Runs::Marketplaces(names) => format!("Plugin marketplaces: {}", names.join(", ")),
        Runs::AlsoSets(keys) => format!("Also sets: {}", keys.join(", ")),
        Runs::More(count) => format!("… and {count} more"),
        Runs::Unreadable { file, reason } => format!("Couldn't read {file}: {reason}"),
    }
}

/// Slint counts in `i32`; no list here comes near its end.
fn index_for_slint(index: usize) -> i32 {
    i32::try_from(index).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use strum::IntoEnumIterator;

    use super::changes_view::turn_facts;
    use super::menus::mode_menu;
    use super::prompt::{acp_prompt, always_label, tool_prompt};
    use super::tasks::{task_item, task_used, tasks_summary};
    use super::transcript::answered;
    use super::usage::duration_text;
    use super::*;
    use crate::app::{Ask, AskKind, Mode, Modes, PermissionMode, Place, Running};
    use crate::call::{AcpCall, AcpOptionKind};
    use crate::chat;
    use crate::external::wire::{self, ToolName};
    use crate::ids::SessionId;
    use crate::overview;
    use crate::present::EntriesChange;
    use crate::present::model::{PermissionKind, TaskKind};
    use crate::provider::Backend;
    use crate::tools;

    #[test]
    fn a_turns_facts_read_short() {
        let turn = |duration_ms, tokens_in, tokens_out| chat::TurnEnd {
            number: 3,
            duration_ms,
            tokens_in,
            tokens_out,
            changed: None,
        };
        assert_eq!(
            turn_facts(&turn(Some(18_400), 24_149, 312)),
            "Turn 3 · 18 s · 24.1k tokens in, 312 out"
        );
        assert_eq!(
            turn_facts(&turn(Some(125_000), 999, 1_050)),
            "Turn 3 · 2 min 5 s · 999 tokens in, 1.1k out"
        );
        assert_eq!(turn_facts(&turn(None, 0, 0)), "Turn 3 · 0 tokens in, 0 out");
    }

    #[test]
    fn the_mode_menu_offers_the_programs_own_modes() {
        use crate::provider::Backend;
        let titles = |items: &[(String, String)]| {
            items
                .iter()
                .map(|(title, _)| title.clone())
                .collect::<Vec<_>>()
        };

        let (items, ticked) = mode_menu(
            &Modes::Claude(Some(Mode::Known(PermissionMode::Plan))),
            Backend::Claude,
        );
        assert_eq!(
            titles(&items),
            PermissionMode::iter()
                .map(|mode| mode.label().to_owned())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            items.get(ticked).map(|(title, _)| title.as_str()),
            Some("Plan mode")
        );

        let offered: Vec<wire::ProgramMode> = serde_json::from_value(serde_json::json!([
            {"id": "default", "name": "Default", "description": "Ask before acting"},
            {"id": "accept-edits", "name": "Accept edits", "description": ""}
        ]))
        .expect("agy's modes");
        let current = serde_json::from_value(serde_json::json!("accept-edits")).ok();
        let (items, ticked) = mode_menu(&Modes::Program { offered, current }, Backend::Agy);
        assert_eq!(titles(&items), ["Default", "Accept edits"]);
        assert_eq!(
            items.first().map(|(_, description)| description.as_str()),
            Some("Ask before acting")
        );
        assert_eq!(ticked, 1);

        let (items, ticked) = mode_menu(&Modes::of(Backend::Agy), Backend::Agy);
        assert_eq!(titles(&items), ["Default mode"]);
        assert!(
            items
                .first()
                .is_some_and(|(_, description)| description.contains("once agy runs"))
        );
        assert_eq!(ticked, 0);
    }

    #[test]
    fn a_question_is_answered_by_a_pick_or_by_writing() {
        assert!(!answered(&[false, false], ""));
        assert!(answered(&[false, true], ""));
        assert!(answered(&[false, false], "neither, build it in CI"));
        assert!(!answered(&[false, false], "  \n"));
        assert!(!answered(&[], ""));
    }

    #[test]
    fn the_bar_sets_a_message_as_prose_and_a_command_as_code() {
        let asked = |tool: ToolName, input: serde_json::Value| {
            tool_prompt(
                &tool,
                &input,
                Path::new("/work"),
                crate::provider::Backend::Claude,
            )
        };
        let message = asked(
            ToolName::SendMessage,
            serde_json::json!({"to": "peer-07", "message": "Second note."}),
        );
        assert_eq!(message.summary, "Second note.");
        assert_eq!(message.summary_reads_as, tools::Writing::Prose);
        let command = asked(ToolName::Bash, serde_json::json!({"command": "ls -l"}));
        assert_eq!(command.summary, "ls -l");
        assert_eq!(command.summary_reads_as, tools::Writing::Code);
    }

    #[test]
    fn a_rule_to_remember_reads_as_claude_codes_dialog_says_it() {
        let cwd = Path::new("/work/app");
        let label = |suggestion: serde_json::Value| {
            let suggestion: wire::Suggestion =
                serde_json::from_value(suggestion).expect("a suggestion");
            always_label(&suggestion, cwd)
        };
        let bash = |content: &str| serde_json::json!({"toolName": "Bash", "ruleContent": content});
        assert_eq!(
            label(serde_json::json!({
                "type": "addRules", "destination": "localSettings",
                "rules": [bash("npm test:*")],
            })),
            "Yes, and don't ask again for npm test commands in /work/app"
        );
        assert_eq!(
            label(serde_json::json!({
                "type": "addRules", "destination": "session",
                "rules": [bash("cargo build:*"), bash("cargo test:*"), bash("ls")],
            })),
            "Yes, and don't ask again for cargo build, cargo test, and ls commands in /work/app"
        );
        assert_eq!(
            label(serde_json::json!({
                "type": "addRules", "destination": "userSettings",
                "rules": [{"toolName": "WebFetch", "ruleContent": "domain:docs.rs"}],
            })),
            "Yes, and don't ask again for docs.rs in any project (user settings)"
        );
        assert_eq!(
            label(serde_json::json!({
                "type": "setMode", "mode": "acceptEdits", "destination": "session",
            })),
            "Yes, and switch to accept edits (auto-approve file edits and common file commands) \
             for this session"
        );
        assert_eq!(
            label(serde_json::json!({
                "type": "addDirectories", "destination": "session",
                "directories": ["/srv/a", "/srv/b"],
            })),
            "Yes, and always allow access to /srv/a and /srv/b from this project"
        );
    }

    #[test]
    fn claude_codes_request_is_answered_in_its_dialogs_words() {
        let suggestion = serde_json::json!({
            "type": "addRules", "destination": "localSettings",
            "rules": [{"toolName": "Bash", "ruleContent": "npm test:*"}],
        });
        let mut asks = VecDeque::new();
        asks.push_back(Ask {
            request: crate::ids::RequestId::from("r1".to_owned()),
            tool_use: None,
            kind: AskKind::Tool {
                tool: ToolName::Bash,
                input: serde_json::json!({"command": "npm test"}),
                suggestions: vec![wire::Offered::read(suggestion)],
                description: String::new(),
            },
        });
        let mut presenter = Presenter::new(NavSpy::default(), false);
        presenter.render_prompt(&asks, Path::new("/work/app"), Backend::Claude);
        let prompt = &presenter.prompt;
        assert_eq!(
            (
                prompt.allow_label.as_str(),
                prompt.always_label.as_str(),
                prompt.deny_label.as_str()
            ),
            (
                "Yes",
                "Yes, and don't ask again for npm test commands in /work/app",
                "No"
            )
        );
        assert!(prompt.takes_feedback);
    }

    #[test]
    fn the_bar_asks_about_an_acp_call_in_the_agents_words() {
        let call = |title: &str, kind, location: &str| AcpCall {
            title: title.to_owned(),
            kind: Some(kind),
            locations: vec![std::path::PathBuf::from(location)],
            diffs: Vec::new(),
            input: None,
            status: crate::call::AcpStatus::Pending,
            output: None,
        };
        let cwd = Path::new("/work");
        let command = acp_prompt(
            &call("wc -l notes.txt", crate::call::AcpKind::Execute, "/work"),
            cwd,
            Backend::Opencode,
        );
        assert_eq!(command.title, "Allow opencode to run this command?");
        assert_eq!(command.summary, "wc -l notes.txt");
        assert_eq!(command.summary_reads_as, tools::Writing::Code);
        let edit = acp_prompt(
            &call("edit", crate::call::AcpKind::Edit, "/work/notes.txt"),
            cwd,
            Backend::Agy,
        );
        assert_eq!(edit.title, "Allow agy to edit notes.txt?");
        assert_eq!(edit.summary, "edit  notes.txt");

        // Its buttons are the agent's choices, named as the agent names them.
        let option = |id: &str, name: &str, kind| crate::call::AcpOption {
            id: crate::call::OptionId::from(id.to_owned()),
            name: name.to_owned(),
            kind,
        };
        let mut asks = VecDeque::new();
        asks.push_back(Ask {
            request: crate::ids::RequestId::from("p1".to_owned()),
            tool_use: None,
            kind: AskKind::Acp {
                call: call("wc -l notes.txt", crate::call::AcpKind::Execute, "/work"),
                options: vec![
                    option("once", "Allow once", AcpOptionKind::AllowOnce),
                    option("always", "Always allow", AcpOptionKind::AllowAlways),
                    option("reject", "Reject", AcpOptionKind::RejectOnce),
                ],
            },
        });
        let mut presenter = Presenter::new(NavSpy::default(), false);
        presenter.render_prompt(&asks, cwd, Backend::Opencode);
        let prompt = &presenter.prompt;
        assert_eq!(
            (
                prompt.allow_label.as_str(),
                prompt.always_label.as_str(),
                prompt.deny_label.as_str()
            ),
            ("Allow once", "Always allow", "Reject")
        );
        // ACP's answer has no place for what to do instead, so it isn't asked for.
        assert!(!prompt.takes_feedback);
    }

    #[test]
    fn background_tasks_are_counted_by_kind() {
        assert_eq!(tasks_summary(&[]), "");
        assert_eq!(tasks_summary(&[wire::TaskKind::LocalBash]), "1 command");
        assert_eq!(
            tasks_summary(&[
                wire::TaskKind::LocalBash,
                wire::TaskKind::LocalAgent,
                wire::TaskKind::Other,
                wire::TaskKind::RemoteAgent,
            ]),
            "2 agents, 1 command, 1 task"
        );
    }

    fn task(kind: &str, agent: Option<&str>, description: &str) -> wire::BackgroundTask {
        serde_json::from_value(serde_json::json!({
            "task_id": "t",
            "task_type": kind,
            "subagent_type": agent,
            "description": description,
        }))
        .expect("a task")
    }

    #[test]
    fn a_task_says_what_it_does_and_where() {
        let running = |task, place| Running {
            task,
            place,
            progress: None,
        };
        let explore = task("local_agent", Some("Explore"), "Find the tests");
        let agent = task_item(running(&explore, Place::Background));
        assert_eq!(agent.kind, TaskKind::Agent);
        assert_eq!(agent.title, "Find the tests");
        assert_eq!(agent.detail, "Agent · Explore");
        assert!(agent.peekable && agent.used.is_empty() && agent.now.is_empty());
        assert_eq!(
            task_item(running(&explore, Place::Foreground)).detail,
            "Agent · Explore · in the foreground"
        );
        let command = task("local_bash", None, "");
        let unnamed = task_item(running(&command, Place::Background));
        assert_eq!(unnamed.title, "Shell command");
        assert_eq!(unnamed.detail, "Shell command");
        assert!(unnamed.peekable, "what it prints is followed");
        let monitor = task("monitor_mcp", None, "Watch the build");
        assert!(
            !task_item(running(&monitor, Place::Background)).peekable,
            "neither a conversation here nor output to ask for"
        );
    }

    #[test]
    fn a_tasks_progress_reads_briefly() {
        let usage = |tool_uses, total_tokens, duration_ms| wire::TaskUsage {
            total_tokens,
            tool_uses,
            duration_ms,
        };
        assert_eq!(
            task_used(usage(14, 31_200, 160_000)),
            "14 tool uses · 31k tokens · 2m 40s"
        );
        assert_eq!(task_used(usage(1, 0, 0)), "1 tool use");
        assert_eq!(task_used(usage(0, 0, 0)), "");
        let explore = task("local_agent", Some("Explore"), "Find the tests");
        let with = |summary: Option<&str>| {
            let progress = crate::app::TaskProgress {
                usage: usage(2, 900, 3_000),
                last_tool: Some(wire::ToolName::Grep),
                summary: summary.map(str::to_owned),
            };
            let item = task_item(Running {
                task: &explore,
                place: Place::Background,
                progress: Some(&progress),
            });
            (item.used.to_string(), item.now)
        };
        assert_eq!(
            with(None),
            (
                "2 tool uses · 900 tokens · 3s".to_owned(),
                "Last used Grep".to_owned()
            )
        );
        assert_eq!(with(Some("Reading the parser")).1, "Reading the parser");
    }

    #[test]
    fn durations_read_briefly() {
        assert_eq!(duration_text(45_000), "45s");
        assert_eq!(duration_text(192_000), "3m 12s");
        assert_eq!(duration_text(3_840_000), "1h 4m");
    }

    #[test]
    fn shares_fit_a_bar() {
        assert!((fraction_of(25_000, 1_000_000) - 0.02).abs() < 1e-6);
        assert!((fraction_of(5, 0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn counts_are_short() {
        assert_eq!(short_count(950), "950");
        assert_eq!(short_count(1_000), "1k");
        assert_eq!(short_count(4_250), "4.2k");
        assert_eq!(short_count(45_300), "45k");
        assert_eq!(short_count(1_000_000), "1M");
        assert_eq!(short_count(1_250_000), "1.2M");
    }

    #[derive(Default)]
    struct NavSpy {
        nav: Option<model::Nav>,
        nav_pushes: usize,
        commands: Vec<Command>,
    }

    impl Frontend for NavSpy {
        fn nav(&mut self, _old: Option<&model::Nav>, nav: &model::Nav) {
            self.nav = Some(nav.clone());
            self.nav_pushes += 1;
        }
        fn session(&mut self, _old: Option<&model::Session>, _: &model::Session) {}
        fn composer(&mut self, _old: Option<&model::Composer>, _: &model::Composer) {}
        fn sessions(&mut self, _old: Option<&model::Sessions>, _: &model::Sessions) {}
        fn entries(&mut self, _: EntriesChange) {}
        fn prompt(&mut self, _old: Option<&model::Prompt>, _: &model::Prompt) {}
        fn peek(&mut self, _old: Option<&model::Peek>, _: &model::Peek) {}
        fn changes(&mut self, _old: Option<&model::Changes>, _: &model::Changes) {}
        fn side(&mut self, _old: Option<&model::Side>, _: &model::Side) {}
        fn preferences(&mut self, _old: Option<&model::Preferences>, _: &model::Preferences) {}
        fn usage(&mut self, _old: Option<&model::Usage>, _: &model::Usage) {}
        fn transcript(&mut self, _old: Option<&model::Transcript>, _: &model::Transcript) {}
        fn show_image(&mut self, _: ImageKey, _: Result<&Pixels, String>) {}
        fn keep_images(&mut self, _: &HashSet<ImageKey>) {}
        fn command(&mut self, command: Command) {
            self.commands.push(command);
        }
        fn draft(&self) -> String {
            String::new()
        }
    }

    #[test]
    fn nav_and_overview_push_only_when_changed() {
        let mut presenter = Presenter::new(NavSpy::default(), false);
        let id = SessionId::try_from("ee79c043-bfa7-44c0-82b6-509839a05c22".to_owned())
            .expect("session id");
        let built = overview::Built {
            cards: vec![overview::Card {
                path: std::path::PathBuf::from("/code/edualc"),
                name: "edualc".to_owned(),
                modified: None,
                needs_you: 1,
                running: 1,
                last_said: Some("Checked the tests.".to_owned()),
                sessions: vec![overview::CardSession {
                    opens: overview::SessionRef::Session(id.clone()),
                    title: "Fix shell".to_owned(),
                    state: Some(LiveStatus::Waiting),
                    modified: None,
                    last_said: Some("Checked the tests.".to_owned()),
                    program: crate::provider::Program::Claude,
                    worktree: Some("rework".to_owned()),
                }],
                pinned: false,
                hidden: false,
            }],
            waiting: vec![overview::WaitingItem {
                opens: overview::SessionRef::Session(id.clone()),
                project_path: std::path::PathBuf::from("/code/edualc"),
                project_name: "edualc".to_owned(),
                session_title: "Fix shell".to_owned(),
                question: "Run tests?".to_owned(),
                since: None,
                program: crate::provider::Program::Claude,
            }],
        };

        presenter.render_nav(PlaceView::Project, "edualc", "/code/edualc", "", true);
        presenter.render_overview(&built, Some(&id), OverviewView::default());
        presenter.flush();
        presenter.flush();

        assert_eq!(presenter.frontend.nav_pushes, 1);
        let nav = presenter.frontend.nav.as_ref().expect("nav sent");
        assert_eq!(nav.view, PlaceView::Project);
        assert_eq!(nav.needs_you, 1);
        assert_eq!(nav.waiting.len(), 1);
        assert_eq!(nav.waiting.first().expect("item").waited, "waiting");
        assert_eq!(nav.cards.len(), 1);
        assert_eq!(nav.all_sessions.len(), 1);
        assert!(nav.all_sessions.first().expect("item").selected);
    }

    #[test]
    fn acp_plan_mode_and_plan_prompt_name_the_agent() {
        let mut presenter = Presenter::new(NavSpy::default(), false);
        let offered: Vec<wire::ProgramMode> = serde_json::from_value(serde_json::json!([
            {"id": "build", "name": "Build", "description": ""},
            {"id": "plan", "name": "Plan", "description": ""}
        ]))
        .expect("modes");
        let status = Status {
            backend: Backend::Opencode,
            modes: Modes::Program {
                offered,
                current: Some(wire::ModeId::from("plan".to_owned())),
            },
            ..Status::default()
        };
        presenter.render_menus(&status);
        assert_eq!(presenter.session.mode, PermissionKind::Plan);

        let mut asks = VecDeque::new();
        asks.push_back(Ask {
            request: crate::ids::RequestId::from("p1".to_owned()),
            tool_use: None,
            kind: AskKind::Plan {
                input: wire::PlanInput {
                    plan: "1. Do it".to_owned(),
                    rest: serde_json::Map::new(),
                },
                parts: Vec::new(),
                editing: None,
            },
        });
        presenter.render_prompt(&asks, Path::new("/work"), Backend::Opencode);
        assert!(
            presenter
                .prompt
                .title
                .starts_with("opencode's plan is ready.")
        );
    }
}
