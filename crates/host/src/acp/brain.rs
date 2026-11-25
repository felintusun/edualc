//! agy's own records of a conversation (see [`crate::external::agy`]), read for the window.

use std::borrow::Cow;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::de::DeserializeOwned;

use super::Agent;
use super::translate::named;
use crate::external::acp::ToolCall;
use crate::external::agy::{
    self, AgyTask, BoardPost, MemberRole, Step, StepKind, StepStatus, Subagent, SubagentState,
    Swarm,
};
use crate::external::wire::{
    ApiMessage, BackgroundTask, ContentBlock, TaskKind, ToolName, TranscriptLine, Usage,
    UserContent, UserMessage,
};
use crate::ids::{MessageId, SessionId, ToolUseId};
use crate::sessions;

/// Lists the running subagents and swarm members conversation `own` started, and its swarm's
/// board.
pub(super) fn tasks(own: &SessionId) -> Vec<BackgroundTask> {
    let Some(brain) = agy::brain() else {
        return Vec::new();
    };
    let generated = agy::generated(&brain, own);
    let swarm: Option<Swarm> = read(&generated.join("swarm/members.json"));
    let subagents = std::fs::read_dir(generated.join("subagents"))
        .into_iter()
        .flatten()
        .filter_map(|entry| read::<Subagent>(&entry.ok()?.path()))
        .collect();
    background_tasks(own, swarm, subagents)
}

/// Lists the tasks a session's records tell of: its swarm's board and running workers, then
/// the subagents outside the swarm that are alive, each by its conversation.
fn background_tasks(
    own: &SessionId,
    swarm: Option<Swarm>,
    subagents: Vec<Subagent>,
) -> Vec<BackgroundTask> {
    let task = |id: AgyTask, kind: String, description: String| BackgroundTask {
        task_id: id.id(),
        task_type: TaskKind::LocalAgent,
        subagent_type: Some(kind),
        description,
        ambient: false,
    };
    let mut tasks = Vec::new();
    let mut members = Vec::new();
    if let Some(swarm) = swarm {
        tasks.push(task(
            AgyTask::Board(own.clone()),
            "Board".to_owned(),
            swarm.title,
        ));
        for (name, member) in swarm.members {
            members.push(member.conversation_id.clone());
            match member.role {
                MemberRole::Worker | MemberRole::Other if !member.killed => tasks.push(task(
                    AgyTask::Conversation(member.conversation_id),
                    name,
                    member.model.unwrap_or_default(),
                )),
                MemberRole::Worker | MemberRole::Other | MemberRole::Coordinator => {}
            }
        }
    }
    for subagent in subagents {
        match subagent.state {
            SubagentState::Alive if !members.contains(&subagent.conversation_id) => {
                tasks.push(task(
                    AgyTask::Conversation(subagent.conversation_id),
                    subagent.subagent_descriptor.type_name,
                    subagent.subagent_descriptor.role,
                ));
            }
            SubagentState::Alive | SubagentState::Other => {}
        }
    }
    tasks
}

/// Reads a JSON file agy wrote, if it is there; one that can't be read is logged.
fn read<Record: DeserializeOwned>(path: &Path) -> Option<Record> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "an agy record edualc can't read");
            return None;
        }
    };
    serde_json::from_str(&text)
        .inspect_err(|error| {
            tracing::warn!(%error, path = %path.display(), "an agy record edualc can't read");
        })
        .ok()
}

/// Translates a batch of agy conversation steps into transcript lines, pairing each
/// `Generic` tool result with its preceding `PlannerResponse` tool call.
pub(crate) fn step_lines(lines: &[String]) -> Vec<TranscriptLine> {
    let mut pending_tools = std::collections::VecDeque::new();
    let mut out = Vec::new();
    for raw in lines {
        let Ok(step) = serde_json::from_str::<Step>(raw) else {
            continue;
        };
        match step.kind {
            StepKind::Generic if !pending_tools.is_empty() => {
                if let Some(tool_use_id) = pending_tools.pop_front() {
                    let is_error = step.status == StepStatus::Error;
                    let text = step.error.or(step.content).unwrap_or_default();
                    let content = (!text.trim().is_empty()).then_some(UserContent::Text(text));
                    out.push(TranscriptLine::User {
                        message: UserMessage {
                            content: UserContent::Blocks(vec![ContentBlock::ToolResult {
                                tool_use_id,
                                content,
                                is_error,
                            }]),
                        },
                        is_meta: false,
                        is_sidechain: false,
                        is_compact_summary: false,
                        origin: None,
                    });
                }
            }
            StepKind::SystemMessage
            | StepKind::UserInput
            | StepKind::PlannerResponse
            | StepKind::Generic
            | StepKind::Checkpoint
            | StepKind::Other => {
                if step.kind == StepKind::UserInput {
                    pending_tools.clear();
                } else if step.kind == StepKind::PlannerResponse {
                    for n in 0..step.tool_calls.len() {
                        pending_tools
                            .push_back(ToolUseId::from(format!("agy-{}-{n}", step.step_index)));
                    }
                }
                if let Some(line) = step_line(raw) {
                    out.push(line);
                }
            }
        }
    }
    out
}

/// Reads a line of a conversation's steps as a transcript line: a message to the model as a
/// prompt, the model's answer with its tool calls by agy's names (those edualc acts on by the
/// window's), or an error step.
pub(crate) fn step_line(line: &str) -> Option<TranscriptLine> {
    let step: Step = serde_json::from_str(line)
        .inspect_err(|error| tracing::debug!(%error, "an agy step edualc can't read"))
        .ok()?;
    match step.kind {
        StepKind::SystemMessage | StepKind::UserInput => {
            let text = step.content?;
            let text = unwrapped(&text);
            if text.is_empty() {
                return None;
            }
            Some(TranscriptLine::User {
                message: UserMessage {
                    content: UserContent::Text(text),
                },
                is_meta: false,
                is_sidechain: false,
                is_compact_summary: false,
                origin: None,
            })
        }
        StepKind::PlannerResponse => {
            let mut content = Vec::new();
            content.extend(
                step.thinking
                    .filter(|thinking| !thinking.trim().is_empty())
                    .map(|thinking| ContentBlock::Thinking { thinking }),
            );
            content.extend(
                step.content
                    .filter(|text| !text.trim().is_empty())
                    .map(|text| ContentBlock::Text { text }),
            );
            if step.status == StepStatus::Error
                && let Some(error) = step.error.filter(|error| !error.trim().is_empty())
            {
                content.push(ContentBlock::Text { text: error });
            }
            content.extend(step.tool_calls.into_iter().zip(0_u32..).map(|(call, n)| {
                let id = format!("agy-{}-{n}", step.step_index);
                let called = ToolCall {
                    tool_call_id: id.clone(),
                    name: Some(call.name.clone()),
                    raw_input: Some(call.args.clone()),
                    ..ToolCall::default()
                };
                // Read as a transcript, a call keeps agy's name and arguments.
                let (name, input) =
                    named(Agent::Agy, &called).unwrap_or((ToolName::Other(call.name), call.args));
                ContentBlock::ToolUse {
                    id: ToolUseId::from(id),
                    name,
                    input,
                }
            }));
            if content.is_empty() {
                return None;
            }
            Some(TranscriptLine::Assistant {
                message: ApiMessage {
                    id: MessageId::from(format!("agy-step-{}", step.step_index)),
                    content,
                    usage: Usage::default(),
                },
                is_sidechain: false,
            })
        }
        StepKind::Generic if step.status == StepStatus::Error => {
            let text = step
                .error
                .or(step.content)
                .filter(|text| !text.trim().is_empty())?;
            Some(TranscriptLine::Assistant {
                message: ApiMessage {
                    id: MessageId::from(format!("agy-step-{}", step.step_index)),
                    content: vec![ContentBlock::Text { text }],
                    usage: Usage::default(),
                },
                is_sidechain: false,
            })
        }
        StepKind::Generic | StepKind::Checkpoint | StepKind::Other => None,
    }
}

/// Removes every `<open>...</close>` block from `text`, leaving the text around them intact.
fn strip_tag_blocks<'a>(text: &'a str, open: &str, close: &str) -> Cow<'a, str> {
    if !text.contains(open) {
        return Cow::Borrowed(text);
    }
    let mut kept = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(open) {
        kept.push_str(&rest[..start]);
        let after_open = &rest[start + open.len()..];
        let Some(end) = after_open.find(close) else {
            // Unclosed tag: keep the rest rather than dropping real text.
            kept.push_str(&rest[start..]);
            return Cow::Owned(kept);
        };
        rest = &after_open[end + close.len()..];
    }
    kept.push_str(rest);
    Cow::Owned(kept)
}

/// Takes the wrappers off a message agy puts to the model (`<SYSTEM_MESSAGE>`,
/// `<USER_REQUEST>`, `<ADDITIONAL_METADATA>`, `<USER_SETTINGS_CHANGE>`), and the fields before
/// another conversation's message (`[Message] timestamp=… sender=… priority=… content=`).
fn unwrapped(text: &str) -> String {
    let first = strip_tag_blocks(text, "<ADDITIONAL_METADATA>", "</ADDITIONAL_METADATA>");
    let second = strip_tag_blocks(&first, "<USER_SETTINGS_CHANGE>", "</USER_SETTINGS_CHANGE>");
    let inner = second.as_ref();
    let inner = inner
        .split_once("<USER_REQUEST>")
        .and_then(|(_, after)| after.rsplit_once("</USER_REQUEST>"))
        .map_or(inner, |(inside, _)| inside.trim());
    let inner = inner
        .rsplit_once("<SYSTEM_MESSAGE>")
        .and_then(|(_, after)| after.split_once("</SYSTEM_MESSAGE>"))
        .map_or(inner, |(inside, _)| inside.trim());
    inner
        .strip_prefix("[Message] ")
        .and_then(|fields| fields.split_once(" content="))
        .map_or(inner, |(_, message)| message)
        .trim()
        .to_owned()
}

/// Reads a post on a swarm's board as what its member said.
pub(crate) fn post_line(line: &str) -> Option<TranscriptLine> {
    let post: BoardPost = serde_json::from_str(line)
        .inspect_err(|error| tracing::debug!(%error, "a board post edualc can't read"))
        .ok()?;
    Some(TranscriptLine::Assistant {
        message: ApiMessage {
            id: MessageId::from(format!("agy-post-{}", post.seq)),
            content: vec![ContentBlock::Text {
                text: format!("{} (#{}): {}", post.agent, post.seq, post.text),
            }],
            usage: Usage::default(),
        },
        is_sidechain: false,
    })
}

/// How recently an agy session started elsewhere must have written a `RUNNING` step to show
/// as running.
const AGY_LIVE_WINDOW: Duration = Duration::from_secs(120);
/// How much of the end of an agy session's steps is read, to tell whether it runs and what it
/// said last.
const BRAIN_TAIL_BYTES: u64 = 32 * 1024;

/// Returns the steps file of conversation `id`, if agy keeps one.
pub(crate) fn steps_file(id: &SessionId) -> Option<PathBuf> {
    let log = agy::steps(&agy::brain()?, id);
    log.is_file().then_some(log)
}

/// Returns whether `path` is a conversation's steps file in agy's brain folder.
pub(crate) fn is_steps_file(path: &Path) -> bool {
    agy::brain().is_some_and(|brain| agy::is_steps(&brain, path))
}

/// Returns whether `path` is a swarm's board in agy's brain folder.
pub(crate) fn is_board(path: &Path) -> bool {
    agy::brain().is_some_and(|brain| path.starts_with(&brain))
        && path.ends_with("swarm/board.jsonl")
}

/// Returns the first sentence of agy's last reply in conversation `id`, and when its steps were
/// last written.
pub(crate) fn last_said(id: &SessionId) -> Option<(SystemTime, String)> {
    let path = steps_file(id)?;
    let (modified, steps) = tail(&path)?;
    let last = steps.into_iter().rev().find_map(|step| {
        (step.kind == StepKind::PlannerResponse)
            .then_some(step.content)
            .flatten()
            .and_then(|text| sessions::first_sentence(&text))
    })?;
    Some((modified, last))
}

/// Returns whether conversation `id` runs at `now`: its last step runs, and was written within
/// [`AGY_LIVE_WINDOW`].
pub(crate) fn is_running(id: &SessionId, now: SystemTime) -> bool {
    let Some(path) = steps_file(id) else {
        return false;
    };
    let Some((modified, steps)) = tail(&path) else {
        return false;
    };
    let fresh = now.duration_since(modified).unwrap_or_default() < AGY_LIVE_WINDOW;
    fresh
        && steps
            .last()
            .is_some_and(|step| step.status == StepStatus::Running)
}

/// Reads the last steps of a conversation's steps file, and when it was last written.
fn tail(path: &Path) -> Option<(SystemTime, Vec<Step>)> {
    let mut file = std::fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    let modified = metadata.modified().ok()?;
    let len = metadata.len();
    let start = len.saturating_sub(BRAIN_TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(BRAIN_TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let steps = text
        .lines()
        .skip(usize::from(start > 0))
        .filter_map(|line| serde_json::from_str::<Step>(line).ok())
        .collect();
    Some((modified, steps))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external::agy::Member;
    use crate::external::agy::SubagentDescriptor;
    use crate::external::wire::ToolName;

    fn id(text: &str) -> SessionId {
        SessionId::try_from(text.to_owned()).expect("an id")
    }

    const OWN: &str = "6361b5fc-2b57-415e-b512-bdd977278077";
    const WORKER: &str = "1e16b130-ab24-45b6-8ebf-b573ab82eb7f";
    const ENDED: &str = "a539fe4a-54a1-469c-bc13-066bcacfba57";
    const HELPER: &str = "55282ae3-ca5a-4b50-a583-7ca7ffa6c63b";

    #[test]
    fn a_swarm_and_its_subagents_are_tasks() {
        let member = |conversation: &str, role, killed| Member {
            conversation_id: id(conversation),
            role,
            model: Some("gemini-4".to_owned()),
            killed,
        };
        let swarm = Swarm {
            title: "Two tracks".to_owned(),
            members: [
                (
                    "coordinator".to_owned(),
                    member(OWN, MemberRole::Coordinator, false),
                ),
                (
                    "acp-worker".to_owned(),
                    member(WORKER, MemberRole::Worker, false),
                ),
                (
                    "verifier".to_owned(),
                    member(ENDED, MemberRole::Worker, true),
                ),
            ]
            .into(),
        };
        let subagent = |conversation: &str| Subagent {
            conversation_id: id(conversation),
            subagent_descriptor: SubagentDescriptor {
                type_name: "worker".to_owned(),
                role: "Helper".to_owned(),
            },
            state: SubagentState::Alive,
        };
        let tasks = background_tasks(
            &id(OWN),
            Some(swarm),
            vec![subagent(WORKER), subagent(HELPER)],
        );
        let shown: Vec<(Option<AgyTask>, Option<&str>)> = tasks
            .iter()
            .map(|task| (AgyTask::of(&task.task_id), task.subagent_type.as_deref()))
            .collect();
        assert_eq!(
            shown,
            [
                (Some(AgyTask::Board(id(OWN))), Some("Board")),
                (Some(AgyTask::Conversation(id(WORKER))), Some("acp-worker")),
                (Some(AgyTask::Conversation(id(HELPER))), Some("worker")),
            ]
        );
        assert!(
            tasks
                .iter()
                .all(|task| task.task_type == TaskKind::LocalAgent)
        );
    }

    #[test]
    fn steps_read_as_a_transcript() {
        let told = step_line(
            r#"{"step_index":1,"source":"SYSTEM","type":"SYSTEM_MESSAGE","status":"DONE","content":"The following is a <SYSTEM_MESSAGE> not actually sent by the user.\n\n<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-15T21:28:47Z sender=6361b5fc-2b57-415e-b512-bdd977278077 priority=MESSAGE_PRIORITY_HIGH content=Start with the brief.\n</SYSTEM_MESSAGE>"}"#,
        );
        let Some(TranscriptLine::User { message, .. }) = told else {
            panic!("a prompt, not {told:?}");
        };
        assert!(
            matches!(&message.content, UserContent::Text(text) if text == "Start with the brief.")
        );
        let answered = step_line(
            r#"{"step_index":2,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","thinking":"First the files.","tool_calls":[{"name":"read_file","args":{"Path":"/w/notes.txt"}}]}"#,
        );
        let Some(TranscriptLine::Assistant { message, .. }) = answered else {
            panic!("an answer, not {answered:?}");
        };
        let called = message.content.iter().find_map(|block| match block {
            ContentBlock::ToolUse { name, input, .. } => Some((name.clone(), input.clone())),
            ContentBlock::Text { .. }
            | ContentBlock::Thinking { .. }
            | ContentBlock::ServerToolUse { .. }
            | ContentBlock::ToolResult { .. }
            | ContentBlock::Image { .. }
            | ContentBlock::Other => None,
        });
        assert_eq!(
            called,
            Some((
                ToolName::Other("read_file".to_owned()),
                serde_json::json!({ "Path": "/w/notes.txt" })
            ))
        );
        assert!(
            step_line(
                r#"{"step_index":3,"source":"MODEL","type":"GENERIC","status":"DONE","content":"notes"}"#
            )
            .is_none()
        );
        let failed = step_line(
            r#"{"step_index":4,"source":"MODEL","type":"GENERIC","status":"ERROR","error":"permission check failed"}"#,
        );
        assert!(matches!(
            failed,
            Some(TranscriptLine::Assistant { message, .. })
                if matches!(message.content.first(), Some(ContentBlock::Text { text }) if text == "permission check failed")
        ));
    }

    #[test]
    fn user_requests_unwrap_and_drop_metadata() {
        let asked = step_line(
            r#"{"step_index":1,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","content":"<USER_SETTINGS_CHANGE>\nmodel: x\n</USER_SETTINGS_CHANGE>\n<USER_REQUEST>\nFix the parser.\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\n{\"cwd\":\"/w\"}\n</ADDITIONAL_METADATA>"}"#,
        );
        assert!(matches!(
            asked,
            Some(TranscriptLine::User { message, .. })
                if matches!(&message.content, UserContent::Text(text) if text == "Fix the parser.")
        ));
        assert!(
            step_line(
                r#"{"step_index":2,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","content":"<USER_SETTINGS_CHANGE>\nmodel: x\n</USER_SETTINGS_CHANGE>"}"#,
            )
            .is_none()
        );
    }

    #[test]
    fn a_post_reads_as_its_members() {
        let said = post_line(
            r#"{"seq":4,"agent":"acp-worker","text":"Ready for review.","time":"2026-11-15 23:00:00","kind":"post"}"#,
        );
        let Some(TranscriptLine::Assistant { message, .. }) = said else {
            panic!("a post, not {said:?}");
        };
        assert!(matches!(
            message.content.first(),
            Some(ContentBlock::Text { text }) if text == "acp-worker (#4): Ready for review."
        ));
    }

    #[test]
    fn step_lines_pairs_generic_tool_results_with_planner_tool_calls() {
        let lines = vec![
            r#"{"step_index":2,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"read_file","args":{"Path":"/w/a.rs"}}]}"#.to_owned(),
            r#"{"step_index":3,"source":"MODEL","type":"GENERIC","status":"DONE","content":"fn main() {}"}"#.to_owned(),
        ];
        let parsed = step_lines(&lines);
        assert_eq!(parsed.len(), 2);
        assert!(matches!(
            parsed.get(1),
            Some(TranscriptLine::User { message, .. })
                if matches!(
                    &message.content,
                    UserContent::Blocks(blocks)
                        if matches!(
                            blocks.first(),
                            Some(ContentBlock::ToolResult { tool_use_id, is_error: false, .. })
                                if tool_use_id.to_string() == "agy-2-0"
                        )
                )
        ));
    }
}
