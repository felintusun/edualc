//! Each tool's card: what Claude Code's tools, edualc's own and MCP tools show of a call.

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::external::tool_input::{
    AgentInput, BashInput, EditInput, GlobInput, GrepInput, IncomingMessageInput, MultiEditInput,
    NotebookEditInput, ReadInput, SendMessageInput, SkillInput, TodoStatus, TodoWriteInput,
    WebFetchInput, WebSearchInput, WriteInput,
};
use crate::external::wire::{PlanInput, Questions, ToolName};

use super::{
    ToolView, Writing, command_view, compact, diff, first_line, pretty, read, short_path,
    with_place,
};

/// A call being described: the tool, what it was given, the session's folder, and whether its
/// card is open.
pub(super) struct Describing<'call> {
    pub(super) name: &'call ToolName,
    pub(super) input: &'call Value,
    pub(super) cwd: &'call Path,
    pub(super) full: bool,
}

impl Describing<'_> {
    fn input<Input: DeserializeOwned>(&self) -> Option<Input> {
        read(self.name, self.input)
    }

    fn path(&self, path: &Path) -> String {
        short_path(path, self.cwd)
    }

    fn place(&self, place: Option<&PathBuf>) -> String {
        place.map_or_default(|place| self.path(place))
    }
}

/// A Monitor runs a command too, in the background, and says what it watches for.
pub(super) fn command(call: &Describing, view: &mut ToolView) {
    if let Some(bash) = call.input::<BashInput>() {
        command_view(view, &bash);
        if *call.name == ToolName::Bash && bash.run_in_background == Some(true) {
            view.title = "Bash (background)".into();
        }
    }
}

pub(super) fn read_file(call: &Describing, view: &mut ToolView) {
    if let Some(read) = call.input::<ReadInput>() {
        view.summary = call.path(&read.file_path);
        if let Some(offset) = read.offset {
            let range = match read.limit.filter(|&limit| limit > 0) {
                Some(limit) => format!("lines {offset}–{}", offset.saturating_add(limit)),
                None => format!("from line {offset}"),
            };
            view.summary = format!("{}  {range}", view.summary);
        }
    }
}

/// Shows the whole file as it will be: a wall of `+` lines would say the same, louder.
pub(super) fn write(call: &Describing, view: &mut ToolView) {
    if let Some(write) = call.input::<WriteInput>() {
        view.summary = call.path(&write.file_path);
        if call.full {
            view.detail = write.content;
        }
    }
}

pub(super) fn edit(call: &Describing, view: &mut ToolView) {
    if let Some(edit) = call.input::<EditInput>() {
        view.summary = call.path(&edit.file_path);
        if edit.replace_all == Some(true) {
            view.summary += "  (all occurrences)";
        }
        if call.full {
            view.diff = diff(&edit.old_string, &edit.new_string);
        }
    }
}

pub(super) fn multi_edit(call: &Describing, view: &mut ToolView) {
    if let Some(edits) = call.input::<MultiEditInput>() {
        view.summary = call.path(&edits.file_path);
        for edit in edits.edits.iter().filter(|_| call.full) {
            view.diff.extend(diff(&edit.old_string, &edit.new_string));
        }
    }
}

/// Shows the cell's new source whole, as it will be.
pub(super) fn notebook_edit(call: &Describing, view: &mut ToolView) {
    if let Some(edit) = call.input::<NotebookEditInput>() {
        view.summary = call.path(&edit.notebook_path);
        if call.full {
            view.detail = edit.new_source;
        }
    }
}

pub(super) fn glob(call: &Describing, view: &mut ToolView) {
    if let Some(glob) = call.input::<GlobInput>() {
        view.summary = with_place(&glob.pattern, &call.place(glob.path.as_ref()));
    }
}

pub(super) fn grep(call: &Describing, view: &mut ToolView) {
    if let Some(grep) = call.input::<GrepInput>() {
        let what = match grep.glob.filter(|glob| !glob.is_empty()) {
            Some(glob) => format!("{}  ({glob})", grep.pattern),
            None => grep.pattern,
        };
        view.summary = with_place(&what, &call.place(grep.path.as_ref()));
    }
}

pub(super) fn web_fetch(call: &Describing, view: &mut ToolView) {
    if let Some(fetch) = call.input::<WebFetchInput>() {
        view.summary = fetch.url;
        view.detail = fetch.prompt.unwrap_or_default();
    }
}

pub(super) fn web_search(call: &Describing, view: &mut ToolView) {
    view.summary_reads_as = Writing::Prose;
    if let Some(search) = call.input::<WebSearchInput>() {
        view.summary = search.query;
    }
}

pub(super) fn agent(call: &Describing, view: &mut ToolView) {
    view.summary_reads_as = Writing::Prose;
    view.title = "Agent".into();
    if let Some(agent) = call.input::<AgentInput>() {
        if let Some(kind) = agent.subagent_type.filter(|kind| !kind.is_empty()) {
            view.title = format!("Agent · {kind}");
        }
        view.summary = agent.description;
        if call.full {
            view.detail = agent.prompt;
        }
    }
}

pub(super) fn skill(call: &Describing, view: &mut ToolView) {
    if let Some(skill) = call.input::<SkillInput>() {
        view.summary = [skill.skill, skill.command, skill.name]
            .into_iter()
            .flatten()
            .find(|named| !named.is_empty())
            .unwrap_or_default();
    }
}

pub(super) fn todo_list(call: &Describing, view: &mut ToolView) {
    view.summary_reads_as = Writing::Prose;
    if let Some(list) = call.input::<TodoWriteInput>() {
        view.summary = format!("{} items", list.todos.len());
        view.detail = list
            .todos
            .iter()
            .filter(|_| call.full)
            .map(|todo| {
                let mark = match todo.status {
                    TodoStatus::Completed => "☑",
                    TodoStatus::InProgress => "◐",
                    TodoStatus::Pending | TodoStatus::Other => "☐",
                };
                format!("{mark} {}", todo.content)
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
}

pub(super) fn question(call: &Describing, view: &mut ToolView) {
    view.summary_reads_as = Writing::Prose;
    view.title = "Question".into();
    if let Some(asked) = call.input::<Questions>() {
        view.summary = asked
            .questions
            .into_iter()
            .next()
            .map_or_default(|question| question.question);
    }
}

pub(super) fn plan(call: &Describing, view: &mut ToolView) {
    view.summary_reads_as = Writing::Prose;
    view.title = "Plan".into();
    view.summary = "Ready for review".into();
    if call.full
        && let Some(plan) = call.input::<PlanInput>()
    {
        view.markdown = plan.plan;
    }
}

/// Claude Code's own message to another session.
pub(super) fn message(call: &Describing, view: &mut ToolView) {
    view.summary_reads_as = Writing::Prose;
    if let Some(SendMessageInput { to, message }) = call.input() {
        // An inbox's address (`uds:…`, as Claude Code tells a session to answer by) says
        // nothing to you; it stays in the detail.
        let addressed = ["uds:", "bridge:", "did:"]
            .iter()
            .any(|scheme| to.starts_with(scheme));
        view.title = if addressed {
            "Message to another session".to_owned()
        } else {
            format!("Message to {to}")
        };
        view.summary = first_line(&message);
        if call.full {
            view.detail = if addressed {
                format!("{message}\n\nTo {to}")
            } else {
                message
            };
        }
    }
}

/// A message sent with edualc's messaging tool.
pub(super) fn edualc_message(call: &Describing, view: &mut ToolView) {
    view.summary_reads_as = Writing::Prose;
    if let Some(SendMessageInput { to, message }) = call.input() {
        view.title = format!("Message to {to}");
        view.summary = first_line(&message);
        if call.full {
            view.detail = message;
        }
    }
}

/// Another session's message that waits to be let in.
pub(super) fn incoming_message(call: &Describing, view: &mut ToolView) {
    view.summary_reads_as = Writing::Prose;
    if let Some(IncomingMessageInput { from, message }) = call.input() {
        view.title = format!("Message from {from}");
        view.summary = first_line(&message);
        if call.full {
            view.detail = message;
        }
    }
}

/// An MCP tool (`mcp__<server>__<tool>`), titled by its tool and summed up by its server and
/// input.
pub(super) fn mcp(call: &Describing, view: &mut ToolView) {
    let name = call.name.to_string();
    let mut parts = name.trim_start_matches("mcp__").splitn(2, "__");
    let server = parts.next().unwrap_or("");
    view.title = parts.next().unwrap_or(&name).to_owned();
    view.summary = format!("{server}  {}", compact(call.input, 120));
    if call.full {
        view.detail = pretty(call.input);
    }
}

/// edualc's listing of the sessions, shown as the MCP tool it is, as any other server's.
pub(super) fn list_sessions(call: &Describing, view: &mut ToolView) {
    view.title = "list_sessions".to_owned();
    view.summary = format!("edualc  {}", compact(call.input, 120));
    if call.full {
        view.detail = pretty(call.input);
    }
}

/// Any other call, shown by its input.
pub(super) fn input(call: &Describing, view: &mut ToolView) {
    view.summary = compact(call.input, 120);
    if call.full {
        view.detail = pretty(call.input);
    }
}
