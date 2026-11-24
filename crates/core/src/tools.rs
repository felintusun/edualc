//! How a tool call reads in the chat: its name, a one-line summary (the command, the file),
//! the input worth showing when expanded, and for edits a diff.

mod cards;

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::call::AcpCall;
use crate::edualc_tool::EdualcTool;
use crate::external::tool_input::{
    BashInput, EditInput, MultiEditInput, NotebookEditInput, ReadInput, WriteInput,
};
use crate::external::wire::{ContentBlock, ToolName, UserContent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    Context,
    Added,
    Removed,
    Hunk,
}

/// How a call's summary reads, and so how it is set: as code (a command, a path, a pattern, a
/// URL, the input), or as words (a question, what an agent is to do).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Writing {
    #[default]
    Code,
    Prose,
}

#[derive(Debug, Default)]
pub struct ToolView {
    pub title: String,
    pub summary: String,
    pub summary_reads_as: Writing,
    /// Shown when expanded, as plain monospace text; empty if the summary says it all.
    pub detail: String,
    pub diff: Vec<(DiffKind, String)>,
    /// Markdown shown when expanded (ExitPlanMode's plan).
    pub markdown: String,
}

/// How much of a call to describe: its line alone (title and summary), or also what an
/// expanded card shows (a diff, a file, the input).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    Headline,
    Full,
}

/// Diffs longer than this are cut; the full change is in the file.
const MAX_DIFF_LINES: usize = 400;

pub fn describe(name: &ToolName, input: &Value, cwd: &Path, depth: Depth) -> ToolView {
    let call = cards::Describing {
        name,
        input,
        cwd,
        full: depth == Depth::Full,
    };
    let mut view = ToolView {
        title: name.to_string(),
        ..ToolView::default()
    };
    let card: fn(&cards::Describing, &mut ToolView) = match name {
        ToolName::Bash | ToolName::Monitor => cards::command,
        ToolName::Read => cards::read_file,
        ToolName::Write => cards::write,
        ToolName::Edit => cards::edit,
        ToolName::MultiEdit => cards::multi_edit,
        ToolName::NotebookEdit => cards::notebook_edit,
        ToolName::Glob => cards::glob,
        ToolName::Grep => cards::grep,
        ToolName::WebFetch => cards::web_fetch,
        ToolName::WebSearch => cards::web_search,
        ToolName::Task | ToolName::Agent => cards::agent,
        ToolName::Skill => cards::skill,
        ToolName::TodoWrite => cards::todo_list,
        ToolName::AskUserQuestion => cards::question,
        ToolName::ExitPlanMode => cards::plan,
        ToolName::SendMessage => cards::message,
        ToolName::Edualc(EdualcTool::SendMessage) => cards::edualc_message,
        ToolName::Edualc(EdualcTool::IncomingMessage) => cards::incoming_message,
        ToolName::Edualc(EdualcTool::ListSessions) => cards::list_sessions,
        ToolName::Other(tool) if tool.starts_with("mcp__") => cards::mcp,
        ToolName::BashOutput
        | ToolName::KillShell
        | ToolName::NotebookRead
        | ToolName::Ls
        | ToolName::Other(_) => cards::input,
    };
    card(&call, &mut view);
    cut_long_diff(&mut view);
    view
}

/// Describes an ACP agent's call as ACP clients show one: its title, the files it works on that
/// the title doesn't name, and, opened, the changes it makes or else its arguments. A
/// command's working folder goes unsaid when it is the session's.
pub fn describe_acp(call: &AcpCall, cwd: &Path, depth: Depth) -> ToolView {
    let mut view = ToolView {
        title: call.title.clone(),
        ..ToolView::default()
    };
    view.summary = call
        .locations
        .iter()
        .filter(|place| place.as_path() != cwd)
        .map(|place| short_path(place, cwd))
        .filter(|place| !call.title.contains(place.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    if depth == Depth::Full {
        for change in &call.diffs {
            match &change.old_text {
                Some(old_text) => view.diff.extend(diff(old_text, &change.new_text)),
                None => view.detail.push_str(&change.new_text),
            }
        }
        if let Some(input) = call.input.as_ref().filter(|_| call.diffs.is_empty())
            && !input.as_object().is_some_and(serde_json::Map::is_empty)
        {
            view.detail = pretty(input);
        }
    }
    cut_long_diff(&mut view);
    view
}

fn cut_long_diff(view: &mut ToolView) {
    if view.diff.len() > MAX_DIFF_LINES {
        let cut = view.diff.len() - MAX_DIFF_LINES;
        view.diff.truncate(MAX_DIFF_LINES);
        view.diff
            .push((DiffKind::Hunk, format!("… {cut} more lines")));
    }
}

/// Returns the file a call reads, writes or edits.
pub fn file_of(name: &ToolName, input: &Value) -> Option<PathBuf> {
    match name {
        ToolName::Read => read::<ReadInput>(name, input).map(|read| read.file_path),
        ToolName::Write => read::<WriteInput>(name, input).map(|write| write.file_path),
        ToolName::Edit => read::<EditInput>(name, input).map(|edit| edit.file_path),
        ToolName::MultiEdit => read::<MultiEditInput>(name, input).map(|edits| edits.file_path),
        ToolName::NotebookEdit => {
            read::<NotebookEditInput>(name, input).map(|edit| edit.notebook_path)
        }
        ToolName::Bash
        | ToolName::BashOutput
        | ToolName::KillShell
        | ToolName::Monitor
        | ToolName::NotebookRead
        | ToolName::Glob
        | ToolName::Grep
        | ToolName::Ls
        | ToolName::WebFetch
        | ToolName::WebSearch
        | ToolName::Task
        | ToolName::Agent
        | ToolName::Skill
        | ToolName::TodoWrite
        | ToolName::AskUserQuestion
        | ToolName::ExitPlanMode
        | ToolName::SendMessage
        | ToolName::Edualc(_)
        | ToolName::Other(_) => None,
    }
}

/// Describes a `!` command you ran, as a Bash call's card.
pub fn describe_command(command: &BashInput) -> ToolView {
    let mut view = ToolView {
        title: ToolName::Bash.to_string(),
        ..ToolView::default()
    };
    command_view(&mut view, command);
    view
}

/// Fills in a command's first line, the whole of a long one, and its description.
fn command_view(view: &mut ToolView, bash: &BashInput) {
    let command = &bash.command;
    view.summary = first_line(command);
    if command.contains('\n') || command.chars().count() > 90 {
        view.detail.clone_from(command);
    }
    if let Some(description) = bash.description.as_deref().filter(|text| !text.is_empty()) {
        view.detail = if view.detail.is_empty() {
            format!("# {description}")
        } else {
            format!("# {description}\n{}", view.detail)
        };
    }
}

/// Reads a tool's input as its type; `None` for one that isn't, such as the empty input of a
/// call still streaming in.
pub(crate) fn read<Input: DeserializeOwned>(name: &ToolName, input: &Value) -> Option<Input> {
    match Input::deserialize(input) {
        Ok(read) => Some(read),
        Err(error) => {
            if !input.as_object().is_some_and(serde_json::Map::is_empty) {
                tracing::debug!(%error, tool = %name, "a tool's input that isn't its shape");
            }
            None
        }
    }
}

/// Returns a unified line diff with three lines of context around each change.
fn diff(old: &str, new: &str) -> Vec<(DiffKind, String)> {
    let text_diff = similar::TextDiff::from_lines(old, new);
    let mut lines = Vec::new();
    for (i, group) in text_diff.grouped_ops(3).iter().enumerate() {
        if i > 0 {
            lines.push((DiffKind::Hunk, "⋯".to_owned()));
        }
        for op in group {
            for change in text_diff.iter_changes(op) {
                let kind = match change.tag() {
                    similar::ChangeTag::Equal => DiffKind::Context,
                    similar::ChangeTag::Delete => DiffKind::Removed,
                    similar::ChangeTag::Insert => DiffKind::Added,
                };
                let text = change.value();
                lines.push((kind, text.strip_suffix('\n').unwrap_or(text).to_owned()));
            }
        }
    }
    lines
}

/// `path` relative to the working directory when inside it.
/// The most a path outside the working directory takes on a card before its middle goes.
const CARD_PATH_CHARS: usize = 60;

/// Returns a path as a card shows it: relative to the working folder inside it, else as
/// [`shortened_path`] gives it, so that a long one keeps its file's name in view.
pub fn short_path(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd)
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty())
        .map_or_else(
            || shortened_path(path, CARD_PATH_CHARS),
            |relative| relative.display().to_string(),
        )
}

/// Returns `path` relative to home, its middle left out if it is longer than `max_chars`: the
/// start says where it is, the end which it is.
pub fn shortened_path(path: &Path, max_chars: usize) -> String {
    let full = home_relative(path);
    if full.chars().count() <= max_chars {
        return full;
    }
    let parts: Vec<&str> = full.split('/').collect();
    match (
        parts.first(),
        parts.len().checked_sub(2).and_then(|at| parts.get(at..)),
    ) {
        (Some(first), Some(last)) if parts.len() > 3 => format!("{first}/…/{}", last.join("/")),
        _ => full,
    }
}

/// Returns `path` from `~` when it is in the home folder.
pub fn home_relative(path: &Path) -> String {
    let home = dirs::home_dir();
    match home
        .as_deref()
        .and_then(|home| path.strip_prefix(home).ok())
    {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

fn with_place(what: &str, place: &str) -> String {
    if place.is_empty() {
        what.to_owned()
    } else {
        format!("{what}  in {place}")
    }
}

fn first_line(text: &str) -> String {
    let mut lines = text.lines();
    let first = lines.next().unwrap_or("").to_owned();
    if lines.next().is_some() {
        format!("{first} …")
    } else {
        first
    }
}

fn compact(input: &Value, max: usize) -> String {
    let text = input.to_string();
    if text == "{}" {
        return String::new();
    }
    truncate_chars(&text, max)
}

fn pretty(input: &Value) -> String {
    serde_json::to_string_pretty(input).unwrap_or_default()
}

pub fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text.to_owned(),
    }
}

/// Returns a tool result as text: its text blocks joined, its pictures named, and Claude
/// Code's error wrapper taken off.
pub fn result_text(content: Option<&UserContent>) -> String {
    let text = match content {
        Some(UserContent::Text(text)) => text.clone(),
        Some(UserContent::Blocks(blocks)) => blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } if !text.is_empty() => Some(text.as_str()),
                // Shown in the card as images.
                ContentBlock::Image { .. }
                | ContentBlock::Text { .. }
                | ContentBlock::Thinking { .. }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::ServerToolUse { .. }
                | ContentBlock::ToolResult { .. }
                | ContentBlock::Other => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        None => String::new(),
    };
    let trimmed = text.trim();
    trimmed
        .strip_prefix("<tool_use_error>")
        .and_then(|inner| inner.strip_suffix("</tool_use_error>"))
        .unwrap_or(trimmed)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn long_paths_lose_their_middle() {
        assert_eq!(
            shortened_path(Path::new("/var/tmp/claude-1000/scratchpad/review"), 24),
            "/…/scratchpad/review"
        );
        assert_eq!(shortened_path(Path::new("/srv/app"), 24), "/srv/app");
    }

    #[test]
    fn a_card_keeps_the_file_name_of_a_long_path() {
        let cwd = Path::new("/srv/project");
        let short = |path: &str| short_path(Path::new(path), cwd);
        assert_eq!(short("/srv/project/src/main.rs"), "src/main.rs");
        assert_eq!(
            short(
                "/var/tmp/claude-1000/-home-someone-code-a-project/0a1b2c3d/scratchpad/shell.png"
            ),
            "/…/scratchpad/shell.png"
        );
        assert_eq!(short("/etc/hosts"), "/etc/hosts");
    }

    #[test]
    fn edit_shows_a_diff_with_context() {
        let input = json!({
            "file_path": "/p/src/main.rs",
            "old_string": "a\nb\nc\n",
            "new_string": "a\nB\nc\n",
        });
        let view = describe(&ToolName::Edit, &input, Path::new("/p"), Depth::Full);
        assert_eq!(view.summary, "src/main.rs");
        assert_eq!(
            view.diff,
            vec![
                (DiffKind::Context, "a".into()),
                (DiffKind::Removed, "b".into()),
                (DiffKind::Added, "B".into()),
                (DiffKind::Context, "c".into()),
            ]
        );
    }

    #[test]
    fn cards_read_claude_codes_inputs() {
        let cwd = Path::new("/p");
        let line = |name: ToolName, input: Value| {
            let view = describe(&name, &input, cwd, Depth::Full);
            (view.title, view.summary, view.detail)
        };
        assert_eq!(
            line(
                ToolName::Read,
                json!({ "file_path": "/p/a.rs", "offset": 10, "limit": 5 })
            ),
            ("Read".into(), "a.rs  lines 10–15".into(), String::new())
        );
        assert_eq!(
            line(
                ToolName::Grep,
                json!({ "pattern": "fn main", "path": "/p/src", "glob": "*.rs" })
            ),
            (
                "Grep".into(),
                "fn main  (*.rs)  in src".into(),
                String::new()
            )
        );
        assert_eq!(
            line(
                ToolName::Agent,
                json!({ "description": "Look around", "prompt": "List the files.", "subagent_type": "Explore" })
            ),
            (
                "Agent · Explore".into(),
                "Look around".into(),
                "List the files.".into()
            )
        );
        assert_eq!(
            line(
                ToolName::WebFetch,
                json!({ "url": "https://example.com", "prompt": "The title" })
            ),
            (
                "WebFetch".into(),
                "https://example.com".into(),
                "The title".into()
            )
        );
        assert_eq!(
            line(
                ToolName::TodoWrite,
                json!({ "todos": [
                    { "content": "Read", "status": "completed", "activeForm": "Reading" },
                    { "content": "Write", "status": "in_progress", "activeForm": "Writing" },
                ] })
            ),
            (
                "TodoWrite".into(),
                "2 items".into(),
                "☑ Read\n◐ Write".into()
            )
        );
        assert_eq!(
            line(
                ToolName::AskUserQuestion,
                json!({ "questions": [{ "question": "Which?", "header": "Pick", "options": [], "multiSelect": false }] })
            ),
            ("Question".into(), "Which?".into(), String::new())
        );
        assert_eq!(
            line(ToolName::Grep, json!({})),
            ("Grep".into(), String::new(), String::new())
        );
    }

    #[test]
    fn bash_summary_is_first_line() {
        let input = json!({ "command": "cd x\nmake", "description": "Build" });
        let view = describe(&ToolName::Bash, &input, Path::new("/"), Depth::Full);
        assert_eq!(view.summary, "cd x …");
        assert_eq!(view.detail, "# Build\ncd x\nmake");
    }

    #[test]
    fn a_monitor_reads_as_its_command() {
        let input = json!({
            "command": "for i in $(seq 1 60); do echo beat $i; sleep 2; done",
            "description": "Heartbeat lines every two seconds",
            "run_in_background": true,
        });
        let view = describe(&ToolName::Monitor, &input, Path::new("/"), Depth::Full);
        assert_eq!(view.title, "Monitor");
        assert_eq!(
            view.summary,
            "for i in $(seq 1 60); do echo beat $i; sleep 2; done"
        );
        assert_eq!(view.detail, "# Heartbeat lines every two seconds");
    }

    #[test]
    fn mcp_tools_name_their_server() {
        let view = describe(
            &ToolName::from("mcp__github__create_issue".to_owned()),
            &json!({ "title": "t" }),
            Path::new("/"),
            Depth::Full,
        );
        assert_eq!(view.title, "create_issue");
        assert!(view.summary.starts_with("github  "));
    }

    #[test]
    fn messages_to_sessions_read_as_messages() {
        let card = |to: &str| {
            let input = json!({ "to": to, "summary": "Ping", "message": "Ping 2 from A.\nMore." });
            describe(
                &ToolName::from("SendMessage".to_owned()),
                &input,
                Path::new("/"),
                Depth::Full,
            )
        };
        let by_name = card("decent-perch-55");
        assert_eq!(by_name.title, "Message to decent-perch-55");
        assert_eq!(by_name.summary, "Ping 2 from A. …");
        assert_eq!(by_name.detail, "Ping 2 from A.\nMore.");
        let by_address = card("uds:/run/user/1000/cc-socks/2164.sock");
        assert_eq!(by_address.title, "Message to another session");
        assert_eq!(
            by_address.detail,
            "Ping 2 from A.\nMore.\n\nTo uds:/run/user/1000/cc-socks/2164.sock"
        );
        assert_eq!(by_address.summary_reads_as, Writing::Prose);
    }

    #[test]
    fn errors_lose_their_wrapper() {
        let content = UserContent::Text("<tool_use_error>File not found</tool_use_error>".into());
        assert_eq!(result_text(Some(&content)), "File not found");
    }

    #[test]
    fn an_acp_call_reads_as_the_agent_gives_it() {
        use crate::call::{AcpCall, AcpKind, AcpStatus, Diff};
        let cwd = Path::new("/work");
        let call = |title: &str, kind, location: &str, diffs| AcpCall {
            title: title.to_owned(),
            kind: Some(kind),
            locations: vec![PathBuf::from(location)],
            diffs,
            input: Some(json!({ "path": "notes.txt" })),
            status: AcpStatus::Completed,
            output: None,
        };
        let read = describe_acp(
            &call("read", AcpKind::Read, "/work/notes.txt", Vec::new()),
            cwd,
            Depth::Full,
        );
        assert_eq!(
            (read.title.as_str(), read.summary.as_str()),
            ("read", "notes.txt")
        );
        let command = describe_acp(
            &call("wc -l notes.txt", AcpKind::Execute, "/work", Vec::new()),
            cwd,
            Depth::Headline,
        );
        assert_eq!(command.title, "wc -l notes.txt");
        assert_eq!(command.summary, "");
        let changed = vec![Diff {
            path: PathBuf::from("/work/notes.txt"),
            old_text: Some("one\ntwo\n".to_owned()),
            new_text: "one\nTWO\n".to_owned(),
        }];
        let edit = describe_acp(
            &call("edit", AcpKind::Edit, "/work/notes.txt", changed),
            cwd,
            Depth::Full,
        );
        assert_eq!(edit.summary, "notes.txt");
        assert!(edit.diff.contains(&(DiffKind::Removed, "two".to_owned())));
        assert!(edit.diff.contains(&(DiffKind::Added, "TWO".to_owned())));
        assert_eq!(edit.detail, "", "the change, not the arguments");
    }
}
