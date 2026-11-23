//! The composer: sending a draft or a shell command, slash commands, and stepping through
//! earlier prompts.

use crate::chat::Body;
use crate::decode::Picture;
use crate::external::wire;
use crate::present::Frontend;

use super::{App, Doing, LimitWait, SentPrompt, SignIn, TrustGate};

/// Where the composer is among the session's prompts, as Up and Down step through them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) enum Recall {
    /// The draft is the user's own.
    #[default]
    Writing,
    /// The draft is prompt `index` of the session's, oldest first; `draft` is what was
    /// written before, given back once Down steps past the newest.
    Recalled { index: usize, draft: String },
}

/// What Down puts in the draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Recalled {
    /// The session's prompt at this index.
    Prompt(usize),
    /// What was being written before Up.
    Draft(String),
}

impl Recall {
    /// Up, among `count` prompts, with `draft` in the composer: the prompt to show, if there
    /// is an older one.
    pub(super) fn older(&mut self, count: usize, draft: &str) -> Option<usize> {
        let (index, kept) = match self {
            Self::Writing => (count.checked_sub(1)?, draft.to_owned()),
            Self::Recalled { index, draft } => (index.checked_sub(1)?, draft.clone()),
        };
        *self = Self::Recalled { index, draft: kept };
        Some(index)
    }

    /// Down, among `count` prompts: the next prompt, or past the newest the draft from before,
    /// the user writing again; nothing unless a prompt is recalled.
    pub(super) fn newer(&mut self, count: usize) -> Option<Recalled> {
        let Self::Recalled { index, draft } = std::mem::take(self) else {
            return None;
        };
        let next = index.saturating_add(1);
        if next < count {
            *self = Self::Recalled { index: next, draft };
            Some(Recalled::Prompt(next))
        } else {
            Some(Recalled::Draft(draft))
        }
    }

    /// Whether Up has an older prompt to bring, among `count`.
    pub(super) fn has_older(&self, count: usize) -> bool {
        match self {
            Self::Writing => count > 0,
            Self::Recalled { index, .. } => *index > 0,
        }
    }
}

impl<F: Frontend> App<F> {
    pub(super) fn send(&mut self, text: &str) {
        let text = normalized(text.trim());
        let text = text.as_ref();
        if text.is_empty() && self.attached.is_empty() {
            return;
        }
        // Nothing runs before the folder is trusted: the prompt waits in the composer.
        if !matches!(self.trust_gate, TrustGate::Open) {
            self.wait_for_trust(text);
            return;
        }
        // In the terminal these start over; the CLI behind a pipe doesn't.
        if matches!(text, "/clear" | "/new") {
            self.put_draft("");
            self.new_session(self.status.backend, self.status.sign_in_with);
            return;
        }
        // Claude Code asks for this, but only its terminal has it.
        if text == "/login" {
            self.put_draft("");
            self.status.sign_in = SignIn::Asked;
            return;
        }
        // The terminal's `/btw` takes over the input; here it asks beside the conversation.
        if let Some(question) = text.strip_prefix("/btw")
            && (question.is_empty() || question.starts_with(char::is_whitespace))
        {
            self.put_draft("");
            self.ask_side(question);
            return;
        }
        if self.watch.is_some() {
            return;
        }
        if !self.asks.is_empty() {
            self.chat
                .info("Answer what is asked above before sending a new message.");
            self.view.scroll_to_end();
            return;
        }
        if let Some(command) = shell_command(text) {
            if !self.status.capabilities().inline_shell {
                self.chat.info(format!(
                    "{} sessions don't run ! commands. Ask {} to run it, or run it in a terminal.",
                    self.status.backend.agent(),
                    self.status.backend.agent()
                ));
                self.view.scroll_to_end();
                return;
            }
            self.put_draft("");
            self.run_shell(text, command.trim());
            return;
        }
        self.put_draft("");
        // Sent past the card on how to sign in: the process running can't take up a new sign-in.
        if self.status.sign_in.waits_for_user() && !self.under_way() {
            self.status.sign_in = SignIn::Unknown;
            self.stop();
        }
        // A process starts if none runs; while one can't yet (its folder's trust is still read,
        // or asked about), the prompt waits in the composer.
        if !self.ensure_process() {
            self.wait_for_trust(text);
            return;
        }
        let images = std::mem::take(&mut self.attached);
        self.view.render_attached(&self.attached);
        self.chat.user_images(&images);
        if !text.is_empty() {
            self.chat.user(text);
            self.show_recall();
        }
        self.last_prompt = Some(SentPrompt {
            text: text.to_owned(),
            images: images.clone(),
        });
        // Until Claude Code names the session, its first prompt does.
        if self.status.title.is_none()
            && let Some(title) = provisional_title(text)
        {
            self.status.title = Some(title.to_owned());
        }
        self.start_turn(&images, text);
    }

    /// Hands a prompt to the running process, which starts a turn with it.
    pub(super) fn start_turn(&mut self, images: &[Picture], text: &str) {
        // The user goes on themselves, or the wait is over.
        self.status.limit_wait = LimitWait::Idle;
        self.status.turns += 1;
        self.status.doing = Doing::Starting;
        if std::mem::take(&mut self.status.tasks_left_running) {
            self.redraw.tasks = true;
        }
        self.start_listening();
        let shell_rows = std::mem::take(&mut self.shell_rows);
        self.write(|process| process.prompt(&shell_rows, images, text));
        self.view.scroll_to_end();
    }

    /// The terminal's `!`: Claude Code runs the command beside the conversation, and replays
    /// its echo and then its output, which the conversation shows when they come. Images
    /// attached stay for the next prompt.
    pub(super) fn run_shell(&mut self, written: &str, command: &str) {
        if command.is_empty() {
            return;
        }
        // Claude Code runs one at a time and would refuse this; it waits in the composer.
        if self.chat.shell_running() {
            self.put_draft(written);
            self.chat
                .info("A `!` command still runs: wait for it, or stop it (Stop), then send this.");
            self.view.scroll_to_end();
            return;
        }
        if !self.ensure_process() {
            self.put_draft(written);
            return;
        }
        self.write(|process| process.run_shell(command));
        self.view.scroll_to_end();
    }

    /// Lists the slash commands the draft could become.
    pub(super) fn draft_edited(&mut self, draft: &str) {
        // An edit makes the draft the user's own, a recalled prompt changed too.
        self.recall = Recall::Writing;
        self.show_recall();
        let draft = normalized(draft);
        self.view.set_shell_mode(shell_command(&draft).is_some());
        self.view
            .set_argument_hint(argument_hint(&draft, &self.status.commands));
        let matches: Vec<usize> = match draft.strip_prefix('/') {
            Some(typed) if !typed.contains(char::is_whitespace) => self
                .status
                .commands
                .iter()
                .enumerate()
                .filter(|(_, command)| command.name.starts_with(typed) && command.name != typed)
                .take(8)
                .map(|(index, _)| index)
                .collect(),
            _ => Vec::new(),
        };
        if self.command_matches == matches {
            return;
        }
        let shown: Vec<&wire::CommandInfo> = matches
            .iter()
            .filter_map(|index| self.status.commands.get(*index))
            .collect();
        self.view.render_commands(&shown);
        self.command_matches = matches;
    }

    /// The command at `index` in the list matching the draft.
    pub(super) fn matching_command(&self, index: usize) -> Option<&wire::CommandInfo> {
        self.command_matches
            .get(index)
            .and_then(|at| self.status.commands.get(*at))
    }

    /// Puts the command in the draft: with a space after it, for what follows, if it takes
    /// an argument.
    pub(super) fn pick_command(&mut self, index: usize) {
        let Some(command) = self.matching_command(index) else {
            return;
        };
        let picked = if command.argument_hint.is_empty() {
            format!("/{}", command.name)
        } else {
            format!("/{} ", command.name)
        };
        self.put_draft(&picked);
        self.view.focus_composer();
    }

    pub(super) fn run_command(&mut self, index: usize) {
        let Some(command) = self.matching_command(index) else {
            return;
        };
        let sent = format!("/{}", command.name);
        self.send(&sent);
    }

    /// Puts `text` in the composer, and reads it as an edit is read (the command list, the
    /// shell mode).
    pub(super) fn put_draft(&mut self, text: &str) {
        self.view.set_draft(text);
        self.draft_edited(text);
    }

    /// The session's prompts as the conversation shows them, oldest first: those of a
    /// resumed session's transcript too, whichever window or terminal sent them.
    pub(super) fn prompts(&self) -> Vec<String> {
        self.chat
            .entries
            .iter()
            .filter_map(|entry| {
                if let Body::User(text) = &entry.body {
                    Some(text.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Tells the composer whether Up has an older prompt to bring, and whether one is shown.
    pub(super) fn show_recall(&mut self) {
        let count = self
            .chat
            .entries
            .iter()
            .filter(|entry| matches!(entry.body, Body::User(_)))
            .count();
        let recalling = matches!(self.recall, Recall::Recalled { .. });
        self.view
            .set_recall(self.recall.has_older(count), recalling);
    }

    /// Up: the prompt before the one shown, or the newest, the draft kept for coming back.
    pub(super) fn recall_older(&mut self) {
        let prompts = self.prompts();
        let mut recall = self.recall.clone();
        let Some(text) = recall
            .older(prompts.len(), &self.view.draft())
            .and_then(|index| prompts.get(index))
        else {
            return;
        };
        // Put in the draft as an edit is, which makes it the user's own: the step is kept after.
        self.put_draft(text);
        self.recall = recall;
        self.show_recall();
        self.view.focus_composer();
    }

    /// Down: the prompt after the one shown, or, past the newest, the draft as it was.
    pub(super) fn recall_newer(&mut self) {
        let prompts = self.prompts();
        let mut recall = self.recall.clone();
        let text = match recall.newer(prompts.len()) {
            Some(Recalled::Prompt(index)) => prompts.get(index).cloned().unwrap_or_default(),
            Some(Recalled::Draft(draft)) => draft,
            None => return,
        };
        self.put_draft(&text);
        self.recall = recall;
        self.show_recall();
        self.view.focus_composer();
    }

    /// The `>_` button: `!` put in front of the draft, or taken away.
    pub(super) fn toggle_shell(&mut self) {
        let draft = self.view.draft();
        let toggled = match shell_command(&normalized(&draft)) {
            Some(command) => command.trim_start().to_owned(),
            None => format!("! {}", draft.trim_start()),
        };
        self.put_draft(&toggled);
        self.view.focus_composer();
    }
}

/// A draft as its start is read: the full-width `！` and `／` that input methods type in
/// Japanese or Chinese mode count as `!` and `/`.
pub(super) fn normalized(draft: &str) -> std::borrow::Cow<'_, str> {
    if let Some(rest) = draft.strip_prefix('！') {
        format!("!{rest}").into()
    } else if let Some(rest) = draft.strip_prefix('／') {
        format!("/{rest}").into()
    } else {
        draft.into()
    }
}

/// The command a (normalized) draft runs with `!`, if it is one: not a Markdown image
/// (`![…](…)`), as a pasted text may start with.
pub(super) fn shell_command(draft: &str) -> Option<&str> {
    draft
        .strip_prefix('!')
        .filter(|command| !command.starts_with('['))
}

/// What the command in a (normalized) draft takes, to hint after it: only while the draft is
/// the command and a space, before anything follows; empty otherwise.
pub(super) fn argument_hint<'a>(draft: &str, commands: &'a [wire::CommandInfo]) -> &'a str {
    draft
        .strip_prefix('/')
        .and_then(|typed| typed.strip_suffix(' '))
        .filter(|name| !name.contains(char::is_whitespace))
        .and_then(|name| commands.iter().find(|command| command.name == name))
        .map_or("", |command| command.argument_hint.as_str())
}

/// What names a session until Claude Code does: the prompt's first line with text. A slash
/// command names nothing.
pub(super) fn provisional_title(prompt: &str) -> Option<&str> {
    prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .filter(|line| !normalized(line).starts_with('/'))
}
