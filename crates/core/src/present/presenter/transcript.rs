//! The conversation's rows: messages, tool cards and their Markdown.

use std::collections::{BTreeSet, HashSet};
use std::path::Path;

use crate::chat::{self, Body, Change, Chat};
use crate::cjk::CjkLanguage;
use crate::external::tool_input::BashInput;
use crate::grouping::{self, Grouping, RunState};
use crate::markdown::{self, Block};
use crate::present::model::{
    BlockKind, DiffKind, DiffLine, Entry, EntryKind, GroupRole, ImageState, MdBlock, ShownImage,
    TableRow, ToolState,
};
use crate::present::{EntriesChange, Frontend, Styled};
use crate::tools;

use super::changes_view::{summary_words, turn_facts};
use super::{OUTPUT_CHARS, OUTPUT_LINES, Presenter, cjk};

/// Returns a tool card's row as `view` describes it; `output` only for an open card.
pub(super) fn card(
    row: &mut Entry,
    view: tools::ToolView,
    state: ToolState,
    output: &str,
    expanded: bool,
    han: CjkLanguage,
) {
    row.kind = EntryKind::Tool;
    row.expanded = expanded;
    row.tool_title = view.title;
    row.tool_summary = view.summary;
    row.tool_summary_prose = view.summary_reads_as == tools::Writing::Prose;
    row.tool_state = state;
    row.has_detail = !view.detail.is_empty();
    row.detail = Styled::plain(&view.detail);
    if !view.markdown.is_empty() {
        row.blocks = blocks(&view.markdown, han);
    }
    if card_depth(expanded, row.hidden) == tools::Depth::Full {
        let (output, note) = clip(output);
        row.has_output = !output.is_empty();
        row.output = Styled::plain(&output);
        row.output_note = note;
    }
    row.diff = view
        .diff
        .into_iter()
        .map(|(kind, text)| DiffLine {
            kind: diff_kind(kind),
            text,
        })
        .collect();
}

pub(super) fn blocks(text: &str, han: CjkLanguage) -> Vec<MdBlock> {
    markdown::blocks(text)
        .iter()
        .map(|block| md_block(block, han))
        .collect()
}

/// Makes a Markdown block for the window, its prose set in the family its script takes, as a
/// paragraph's script sets its lines (`han` for Han characters alone).
pub(super) fn md_block(block: &Block, han: CjkLanguage) -> MdBlock {
    match block {
        Block::Text(source) => MdBlock {
            kind: BlockKind::Text,
            body: Styled::markdown(source),
            cjk: cjk(source, han),
            ..MdBlock::default()
        },
        Block::Heading(level, source) => MdBlock {
            kind: BlockKind::Heading,
            body: Styled::markdown(&format!("**{source}**")),
            level: i32::from(*level),
            cjk: cjk(source, han),
            ..MdBlock::default()
        },
        Block::Code { lang, code } => MdBlock {
            kind: BlockKind::Code,
            body: Styled::plain(code),
            lang: lang.as_str().into(),
            raw: code.as_str().into(),
            ..MdBlock::default()
        },
        Block::Quote(source) => MdBlock {
            kind: BlockKind::Quote,
            body: Styled::markdown(source),
            cjk: cjk(source, han),
            ..MdBlock::default()
        },
        Block::Table(table) => {
            let rows: Vec<TableRow> = table
                .rows
                .iter()
                .enumerate()
                .map(|(position, row)| {
                    let cells: Vec<Styled> = row
                        .iter()
                        .map(|cell| match position {
                            0 if !cell.trim().is_empty() => {
                                Styled::markdown(&format!("**{cell}**"))
                            }
                            _ => Styled::markdown(cell),
                        })
                        .collect();
                    TableRow { cells }
                })
                .collect();
            let column_stretch: Vec<f32> = table
                .longest_cell_chars
                .iter()
                .map(|&chars| f32::from(u16::try_from(chars).unwrap_or(u16::MAX)))
                .collect();
            MdBlock {
                kind: BlockKind::Table,
                cjk: cjk(&table.source, han),
                raw: table.source.as_str().into(),
                rows,
                column_stretch,
                ..MdBlock::default()
            }
        }
        Block::Rule => MdBlock {
            kind: BlockKind::Rule,
            ..MdBlock::default()
        },
    }
}

/// Cuts a long output, returning its start and a note of what was left out.
pub(super) fn clip(output: &str) -> (String, String) {
    let lines = output.lines().count();
    let mut clipped = output
        .lines()
        .take(OUTPUT_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    if clipped.len() > OUTPUT_CHARS {
        clipped = tools::truncate_chars(&clipped, OUTPUT_CHARS);
    }
    let note = if lines > OUTPUT_LINES {
        format!("{} more lines", lines - OUTPUT_LINES)
    } else if clipped.len() < output.len() {
        "Cut short".to_owned()
    } else {
        String::new()
    };
    (clipped, note)
}

/// Returns whether a question of Claude's is answered: an option picked, or something else
/// written.
pub(super) fn answered(picks: &[bool], other: &str) -> bool {
    picks.contains(&true) || !other.trim().is_empty()
}

pub(super) fn failed_image(why: String) -> ShownImage {
    ShownImage {
        state: ImageState::Failed,
        note: why,
        ..ShownImage::default()
    }
}

/// Returns how much of a card to describe; what only an open card shows is made only for one.
pub(super) fn card_depth(expanded: bool, hidden: bool) -> tools::Depth {
    if expanded && !hidden {
        tools::Depth::Full
    } else {
        tools::Depth::Headline
    }
}

pub(super) fn shell_state(state: chat::ShellState) -> ToolState {
    match state {
        chat::ShellState::Running => ToolState::Running,
        chat::ShellState::Done => ToolState::Done,
        chat::ShellState::Failed => ToolState::Failed,
        chat::ShellState::Stopped => ToolState::Cancelled,
    }
}

pub(super) fn tool_state(state: chat::ToolState) -> ToolState {
    match state {
        chat::ToolState::Streaming => ToolState::Streaming,
        chat::ToolState::Running => ToolState::Running,
        chat::ToolState::Awaiting => ToolState::Awaiting,
        chat::ToolState::Done => ToolState::Done,
        chat::ToolState::Failed => ToolState::Failed,
        chat::ToolState::Denied => ToolState::Denied,
        chat::ToolState::Cancelled => ToolState::Cancelled,
    }
}

pub(super) fn diff_kind(kind: tools::DiffKind) -> DiffKind {
    match kind {
        tools::DiffKind::Context => DiffKind::Context,
        tools::DiffKind::Added => DiffKind::Added,
        tools::DiffKind::Removed => DiffKind::Removed,
        tools::DiffKind::Hunk => DiffKind::Hunk,
    }
}

impl<F: Frontend> Presenter<F> {
    /// Brings the transcript in line with `chat`: the rows `changes` name, and those whose place
    /// in a run of tool calls changed (`open_runs` holds the runs opened, by their first call).
    /// Each changed row is built once. Runs are worked out again only when they can have
    /// changed, which text growing never does.
    pub fn render_transcript(
        &mut self,
        chat: &Chat,
        changes: Vec<Change>,
        cwd: &Path,
        open_runs: &HashSet<usize>,
        runs_toggled: bool,
    ) {
        if changes.is_empty() && !runs_toggled {
            return;
        }
        let mut regrouped = BTreeSet::new();
        let structural = changes
            .iter()
            .any(|change| !matches!(change, Change::Grew(_)));
        if structural || runs_toggled {
            let groups = grouping::group(&chat.entries, open_runs, cwd);
            regrouped = (0..groups.len())
                .filter(|&index| self.groups.get(index) != groups.get(index))
                .collect();
            self.groups = groups;
        }
        let mut rebuilt = BTreeSet::new();
        let mut reset = false;
        for change in changes {
            match change {
                Change::Reset => reset = true,
                Change::Pushed(index) | Change::Updated(index) | Change::Grew(index) => {
                    rebuilt.insert(index);
                }
            }
        }
        if reset {
            self.entries = chat
                .entries
                .iter()
                .enumerate()
                .map(|(index, entry)| self.row(index, entry, cwd))
                .collect();
            self.frontend
                .entries(EntriesChange::Reset(self.entries.clone()));
            return;
        }
        // New entries, in order; then the changed ones and those their run changed for.
        while let Some(entry) = chat.entries.get(self.entries.len()) {
            let at = self.entries.len();
            let row = self.row(at, entry, cwd);
            self.entries.push(row.clone());
            self.frontend.entries(EntriesChange::Push(row));
            rebuilt.remove(&at);
            regrouped.remove(&at);
        }
        for index in rebuilt.into_iter().chain(regrouped) {
            let Some(entry) = chat.entries.get(index) else {
                continue;
            };
            let row = self.row(index, entry, cwd);
            if let Some(shown) = self.entries.get_mut(index)
                && *shown != row
            {
                shown.clone_from(&row);
                self.frontend.entries(EntriesChange::Set(index, row));
            }
        }
    }

    fn row(&self, index: usize, entry: &chat::Entry, cwd: &Path) -> Entry {
        let han = self.han;
        let mut row = Entry {
            index: i32::try_from(index).unwrap_or(i32::MAX),
            nested: entry.nested,
            ..Entry::default()
        };
        match self.groups.get(index) {
            Some(Grouping::Head(line)) => {
                row.group = GroupRole::Head;
                row.group_summary.clone_from(&line.summary);
                row.group_state = match line.state {
                    RunState::Working => ToolState::Running,
                    RunState::Done => ToolState::Done,
                    RunState::Failed => ToolState::Failed,
                };
                row.group_open = line.open;
            }
            Some(Grouping::Member { open }) => {
                row.group = GroupRole::Member;
                row.group_open = *open;
                row.hidden = !*open;
            }
            Some(Grouping::Alone) | None => {}
        }
        match &entry.body {
            Body::User(text) => {
                row.kind = EntryKind::User;
                row.body = Styled::plain(text);
                row.cjk = cjk(text, han);
            }
            Body::Assistant(text) => {
                row.kind = EntryKind::Assistant;
                row.cjk = cjk(text, han);
                row.blocks = blocks(text, han);
            }
            Body::Thinking { text, expanded } => {
                row.kind = EntryKind::Thinking;
                if *expanded {
                    row.body = Styled::plain(text);
                }
                row.expanded = *expanded;
                row.cjk = cjk(text, han);
            }
            // Sessions write Markdown to each other, as they do to the user.
            Body::Peer { from, text } => {
                row.kind = EntryKind::Peer;
                row.body = Styled::markdown(text);
                row.sender = from.as_deref().unwrap_or("another session").to_owned();
                row.cjk = cjk(text, han);
            }
            // Notes come from slash commands, which use Markdown (`sonnet`).
            Body::Info(text) => {
                row.kind = EntryKind::Info;
                row.body = Styled::markdown(text);
                row.cjk = cjk(text, han);
            }
            Body::Compacted { summary, expanded } => {
                row.kind = EntryKind::Compacted;
                row.has_detail = !summary.is_empty();
                if *expanded {
                    row.blocks = blocks(summary, han);
                }
                row.expanded = *expanded;
                row.cjk = cjk(summary, han);
            }
            Body::TurnEnd(turn) => {
                row.kind = EntryKind::TurnEnd;
                row.body = Styled::plain(&turn_facts(turn));
                row.changed = turn
                    .changed
                    .filter(|summary| summary.files > 0)
                    .map_or_else(String::new, summary_words);
            }
            Body::Error(text) => {
                row.kind = EntryKind::Error;
                row.body = Styled::plain(text);
                row.cjk = cjk(text, han);
            }
            Body::Tool { tool, expanded } => {
                let depth = card_depth(*expanded, row.hidden);
                let view = tool.call.describe(cwd, depth);
                let state = tool_state(tool.state);
                card(&mut row, view, state, &tool.output, *expanded, han);
                if depth == tools::Depth::Full {
                    row.images = self.shown_images(&tool.images);
                }
            }
            Body::Image(key) => {
                row.kind = EntryKind::Image;
                row.images = self.shown_images(std::slice::from_ref(key));
            }
            // A card as Claude's own commands get, titled as the user's.
            Body::Shell { shell, expanded } => {
                let mut view =
                    tools::describe_command(&BashInput::command(shell.command.clone(), None));
                view.title = "You ran".to_owned();
                let state = shell_state(shell.state);
                card(&mut row, view, state, &shell.output, *expanded, han);
            }
        }
        row
    }
}
