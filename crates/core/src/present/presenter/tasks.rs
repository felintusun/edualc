//! What runs in the background, and the subagent or command followed in the side panel.

use crate::app::{CommandOutput, Conversation, Followed, Peek, PeekView, Place, Running, Tasks};
use crate::external::wire;
use crate::present::model::{PeekStep, StepKind, TaskItem, TaskKind};
use crate::present::{Command, Frontend};

use super::Presenter;
use super::usage::duration_text;

impl<F: Frontend> Presenter<F> {
    /// What runs in the background: each task, and how much of each kind.
    /// What runs besides the turn, and whether a stopped turn left it running.
    pub fn render_tasks(&mut self, tasks: &Tasks, left_running: bool) {
        let session = &mut self.session;
        let kinds: Vec<wire::TaskKind> = tasks
            .running()
            .map(|running| running.task.task_type)
            .collect();
        session.tasks_summary = tasks_summary(&kinds);
        let items: Vec<TaskItem> = tasks.running().map(task_item).collect();
        session.tasks = items;
        session.tasks_left_running = left_running;
    }

    /// Opens the list of what runs.
    pub fn show_tasks(&mut self) {
        self.command(Command::ShowTasks);
    }

    /// The subagent followed in the side panel, if one is, and whether it still runs.
    pub fn render_peek(&mut self, peek: Option<&Peek>, tasks: &Tasks) {
        let global = &mut self.peek;
        let Some(peek) = peek else {
            global.open = false;
            global.steps = Vec::new();
            global.output = Vec::new();
            return;
        };
        let running = tasks
            .running()
            .any(|running| running.task.task_id == peek.task);
        global.open = true;
        global.kind = task_kind(peek.kind);
        global.title = peek.title.as_str().into();
        global.detail = peek_detail(peek);
        global.running = running;
        let (steps, lines, note): (Vec<PeekStep>, Vec<String>, String) = match &peek.view {
            PeekView::Conversation(conversation) => (
                conversation.steps.iter().map(peek_step).collect(),
                Vec::new(),
                String::new(),
            ),
            PeekView::Output(output) => (
                Vec::new(),
                output.text.lines().map(String::from).collect(),
                output_note(output),
            ),
        };
        global.steps = steps;
        global.output = lines;
        global.note = note;
    }
}

/// The kind of a task as the window tells them apart.
pub(super) fn task_kind(kind: wire::TaskKind) -> TaskKind {
    match kind {
        wire::TaskKind::LocalAgent
        | wire::TaskKind::RemoteAgent
        | wire::TaskKind::InProcessTeammate => TaskKind::Agent,
        wire::TaskKind::LocalBash => TaskKind::Command,
        wire::TaskKind::LocalWorkflow => TaskKind::Workflow,
        wire::TaskKind::MonitorMcp | wire::TaskKind::MonitorWs => TaskKind::Monitor,
        wire::TaskKind::Other => TaskKind::Other,
    }
}

pub(super) fn task_item(running: Running<'_>) -> TaskItem {
    let Running {
        task,
        place,
        progress,
    } = running;
    let what = match task.task_type {
        wire::TaskKind::LocalAgent => "Agent",
        wire::TaskKind::RemoteAgent => "Agent in the cloud",
        wire::TaskKind::InProcessTeammate => "Teammate",
        wire::TaskKind::LocalBash => "Shell command",
        wire::TaskKind::LocalWorkflow => "Workflow",
        wire::TaskKind::MonitorMcp | wire::TaskKind::MonitorWs => "Monitor",
        wire::TaskKind::Other => "Task",
    };
    let mut detail = match &task.subagent_type {
        Some(agent) => format!("{what} · {agent}"),
        None => what.to_owned(),
    };
    if place == Place::Foreground {
        detail.push_str(" · in the foreground");
    }
    let used = progress.map(|progress| task_used(progress.usage));
    let now = progress.and_then(|progress| match (&progress.summary, &progress.last_tool) {
        (Some(summary), _) => Some(summary.clone()),
        (None, Some(tool)) => Some(format!("Last used {tool}")),
        (None, None) => None,
    });
    TaskItem {
        id: task.task_id.to_string(),
        kind: task_kind(task.task_type),
        title: if task.description.is_empty() {
            what.into()
        } else {
            task.description.as_str().into()
        },
        detail,
        used: used.unwrap_or_default(),
        now: now.unwrap_or_default(),
        peekable: Followed::of(task.task_type).is_some(),
    }
}

/// "Agent · Explore", as the chip's list names a subagent; "Shell command".
pub(super) fn peek_detail(peek: &Peek) -> String {
    match &peek.view {
        PeekView::Conversation(Conversation {
            agent: Some(agent), ..
        }) => format!("Agent · {agent}"),
        PeekView::Conversation(_) => "Agent".to_owned(),
        PeekView::Output(_) => "Shell command".to_owned(),
    }
}

/// What the side panel says of a command's output beside it: why there is none, or that it is
/// only the end.
pub(super) fn output_note(output: &CommandOutput) -> String {
    match output.refusal() {
        Some(refusal) => format!("Claude Code doesn't show what it printed: {refusal}"),
        None if output.truncated => format!(
            "Only the end of the {} it printed",
            bytes_text(output.total_bytes)
        ),
        None => String::new(),
    }
}

/// "9 KB", "1.2 MB": the size of a command's output, which is shown whole below 8 KB.
pub(super) fn bytes_text(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * KB;
    match bytes {
        ..MB => format!("{} KB", bytes.div_ceil(KB)),
        MB.. => format!("{}.{} MB", bytes / MB, (bytes % MB) * 10 / MB),
    }
}

pub(super) fn peek_step(step: &crate::app::Step) -> PeekStep {
    PeekStep {
        kind: match step.kind {
            crate::app::StepKind::Asked => StepKind::Asked,
            crate::app::StepKind::Said => StepKind::Said,
            crate::app::StepKind::Called => StepKind::Called,
        },
        title: step.title.as_str().into(),
        text: step.text.as_str().into(),
    }
}

/// "14 tool uses · 31k tokens · 2m 40s"; what is still none is left out.
pub(super) fn task_used(usage: wire::TaskUsage) -> String {
    let tools = match usage.tool_uses {
        0 => None,
        1 => Some("1 tool use".to_owned()),
        uses => Some(format!("{uses} tool uses")),
    };
    let tokens =
        (usage.total_tokens > 0).then(|| format!("{} tokens", short_count(usage.total_tokens)));
    let time = (usage.duration_ms > 0).then(|| duration_text(usage.duration_ms));
    [tools, tokens, time]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ")
}

/// How much runs of each kind, the most common kinds of work first: "2 agents, 1 command".
pub(super) fn tasks_summary(kinds: &[wire::TaskKind]) -> String {
    [
        TaskKind::Agent,
        TaskKind::Command,
        TaskKind::Workflow,
        TaskKind::Monitor,
        TaskKind::Other,
    ]
    .into_iter()
    .filter_map(|kind| {
        let count = kinds
            .iter()
            .filter(|task_type| task_kind(**task_type) == kind)
            .count();
        let (one, many) = match kind {
            TaskKind::Agent => ("agent", "agents"),
            TaskKind::Command => ("command", "commands"),
            TaskKind::Workflow => ("workflow", "workflows"),
            TaskKind::Monitor => ("monitor", "monitors"),
            TaskKind::Other => ("task", "tasks"),
        };
        match count {
            0 => None,
            1 => Some(format!("1 {one}")),
            _ => Some(format!("{count} {many}")),
        }
    })
    .collect::<Vec<_>>()
    .join(", ")
}

/// 950, 4.2k, 45k, 1.2M.
pub fn short_count(count: u64) -> String {
    let scaled = |divisor: u32, unit: &str| {
        let hundreds = f64::from(u32::try_from(count / 100).unwrap_or(u32::MAX));
        let value = hundreds * 100.0 / f64::from(divisor);
        format!("{value:.1}{unit}").replace(".0", "")
    };
    match count {
        0..1_000 => count.to_string(),
        1_000..10_000 => scaled(1_000, "k"),
        10_000..1_000_000 => format!("{}k", count / 1_000),
        _ => scaled(1_000_000, "M"),
    }
}
