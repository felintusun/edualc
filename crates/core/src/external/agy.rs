//! What agy (Google's Antigravity CLI, 1.5.9) keeps of a conversation in its brain folder,
//! `~/.gemini/antigravity-cli/brain/<conversation>/.system_generated/`, in the fields edualc reads:
//! the conversation's steps (`logs/transcript.jsonl`, one a line, appended; see [`STEP_LOGS`]), the subagents it
//! started (`subagents/<conversation>.json`), and a swarm's members (`swarm/members.json`) and
//! board (`swarm/board.jsonl`, a post a line). A subagent, or a swarm's member, is a
//! conversation of its own, with a brain folder of its own. Also what `agy -p /usage` says of
//! the plan's quotas.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;

use crate::external::wire::TaskId;
use crate::ids::SessionId;

/// Where agy keeps its conversations.
pub fn brain() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".gemini/antigravity-cli/brain"))
}

/// Conversation `id`'s folder of what agy wrote for it.
pub fn generated(brain: &std::path::Path, id: &SessionId) -> PathBuf {
    brain.join(id.as_str()).join(".system_generated")
}

/// The names agy gives a conversation's steps, in `logs/`: the first as it is read, the second
/// with the same steps in full. While agy works on a conversation the first can be missing for
/// a moment (seen as a session was loaded), and the second stands in.
pub const STEP_LOGS: [&str; 2] = ["transcript.jsonl", "transcript_full.jsonl"];

/// Conversation `id`'s steps: the first of [`STEP_LOGS`] that is there, else the first's path.
pub fn steps(brain: &std::path::Path, id: &SessionId) -> PathBuf {
    let logs = generated(brain, id).join("logs");
    STEP_LOGS
        .iter()
        .map(|name| logs.join(name))
        .find(|path| path.is_file())
        .unwrap_or_else(|| logs.join(STEP_LOGS[0]))
}

/// Whether `path` names a conversation's steps under `brain`.
pub fn is_steps(brain: &std::path::Path, path: &std::path::Path) -> bool {
    path.starts_with(brain)
        && path.parent().is_some_and(|logs| logs.ends_with("logs"))
        && path
            .file_name()
            .is_some_and(|name| STEP_LOGS.iter().any(|log| name == *log))
}

/// One step of a conversation: the model's answer (its thinking, text and tool calls), a tool
/// call's result, or a message to the model.
#[derive(Debug, Deserialize)]
pub struct Step {
    pub step_index: u64,
    #[serde(rename = "type")]
    pub kind: StepKind,
    pub status: StepStatus,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub thinking: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<StepCall>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StepKind {
    /// The model's answer; each of its tool calls has a result step after it, in turn.
    PlannerResponse,
    /// A tool call's result.
    Generic,
    /// A message to the model from agy: an assignment, a board's news.
    SystemMessage,
    UserInput,
    Checkpoint,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StepStatus {
    Done,
    /// Still under way, or a command that went on in the background.
    Running,
    Error,
    #[serde(other)]
    Other,
}

/// A tool call, by agy's name for the tool and its arguments.
#[derive(Debug, Deserialize)]
pub struct StepCall {
    pub name: String,
    #[serde(default)]
    pub args: Value,
}

/// A subagent a conversation started.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Subagent {
    pub conversation_id: SessionId,
    pub subagent_descriptor: SubagentDescriptor,
    pub state: SubagentState,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentDescriptor {
    /// Its kind ("worker").
    pub type_name: String,
    /// What it does, in words ("Swarm Worker (verifier)").
    #[serde(default)]
    pub role: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum SubagentState {
    #[serde(rename = "SUBAGENT_STATE_ALIVE")]
    Alive,
    #[serde(other)]
    Other,
}

/// A swarm's members, by name.
#[derive(Debug, Deserialize)]
pub struct Swarm {
    #[serde(default)]
    pub title: String,
    pub members: BTreeMap<String, Member>,
}

#[derive(Debug, Deserialize)]
pub struct Member {
    pub conversation_id: SessionId,
    pub role: MemberRole,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub killed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberRole {
    /// The conversation that started the swarm.
    Coordinator,
    Worker,
    #[serde(other)]
    Other,
}

/// A post on a swarm's board.
#[derive(Debug, Deserialize)]
pub struct BoardPost {
    pub seq: u64,
    /// The member's name.
    pub agent: String,
    pub text: String,
}

/// A background task of an agy session, as edualc names it to the window: one of its swarm's
/// members (or another subagent), or the swarm's board. The conversation's id is in the name,
/// so the daemon finds what to read from the name alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgyTask {
    Conversation(SessionId),
    Board(SessionId),
}

const CONVERSATION: &str = "agy-";
const BOARD: &str = "agy-board-";

impl AgyTask {
    /// The task this is, if it is one of an agy session's.
    pub fn of(task: &TaskId) -> Option<Self> {
        let name = task.to_string();
        if let Some(id) = name.strip_prefix(BOARD) {
            return SessionId::try_from(id.to_owned()).ok().map(Self::Board);
        }
        name.strip_prefix(CONVERSATION)
            .and_then(|id| SessionId::try_from(id.to_owned()).ok())
            .map(Self::Conversation)
    }

    pub fn id(&self) -> TaskId {
        match self {
            Self::Conversation(id) => TaskId::from(format!("{CONVERSATION}{}", id.as_str())),
            Self::Board(id) => TaskId::from(format!("{BOARD}{}", id.as_str())),
        }
    }
}

/// What `agy -p /usage --output-format json` prints (agy 1.5.9), in the fields edualc reads:
/// the plan's quotas.
#[derive(Debug, Deserialize)]
pub struct UsageAnswer {
    pub command: UsageCommand,
}

#[derive(Debug, Deserialize)]
pub struct UsageCommand {
    pub data: Quotas,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Quotas {
    pub groups: Vec<QuotaGroup>,
}

/// Models that share their quotas: a five hours' and a week's, or only a week's.
#[derive(Debug, Clone, Deserialize)]
pub struct QuotaGroup {
    pub name: String,
    /// Which models are in the group, in words: "Models within this group: Gemini 4 Hydrogen,
    /// Gemini 3.8 Flash".
    #[serde(default)]
    pub description: String,
    /// The limits that apply; those agy has switched off are left out.
    #[serde(rename = "buckets", deserialize_with = "applying")]
    pub quotas: Vec<Quota>,
}

/// A limit as agy says it. One it has switched off (`"disabled": true`, seen for the five
/// hours of "Claude and GPT models") says no reset time.
#[derive(Debug, Deserialize)]
struct Bucket {
    window: QuotaWindow,
    remaining_fraction: f32,
    #[serde(default)]
    disabled: bool,
    reset_time: Option<jiff::Timestamp>,
}

/// The buckets that limit anything, as quotas.
fn applying<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<Quota>, D::Error> {
    Vec::<Bucket>::deserialize(deserializer)?
        .into_iter()
        .filter(|bucket| !bucket.disabled)
        .map(|bucket| match bucket.reset_time {
            Some(reset_time) => Ok(Quota {
                window: bucket.window,
                remaining_fraction: bucket.remaining_fraction,
                reset_time,
            }),
            None => Err(serde::de::Error::missing_field("reset_time")),
        })
        .collect()
}

/// How the group's models are listed in its description.
const LISTED: &str = "Models within this group: ";

impl QuotaGroup {
    /// Whether the model agy names `model` ("Gemini 3.8 Flash (High)", its effort in
    /// brackets, or "Orchestrator (Argon)") draws on this group's quotas: the group is named
    /// after it, or its description lists it.
    pub fn covers(&self, model: &str) -> bool {
        if let Some(inner) = model
            .strip_prefix("Orchestrator (")
            .and_then(|rest| rest.strip_suffix(')'))
        {
            return self.matches_token(inner);
        }
        let model = model
            .rsplit_once(" (")
            .map_or(model, |(model, _effort)| model);
        self.matches_exact(model)
    }

    fn matches_exact(&self, model: &str) -> bool {
        self.name == model || self.listed_models().any(|listed| listed == model)
    }

    fn matches_token(&self, token: &str) -> bool {
        let suffix = format!(" {token}");
        let matches = |candidate: &str| candidate == token || candidate.ends_with(&suffix);
        matches(&self.name) || self.listed_models().any(matches)
    }

    fn listed_models(&self) -> impl Iterator<Item = &str> {
        self.description
            .strip_prefix(LISTED)
            .into_iter()
            .flat_map(|listed| listed.trim_end_matches('.').split(", "))
    }
}

#[derive(Debug, Clone)]
pub struct Quota {
    pub window: QuotaWindow,
    /// What is left, from 0 to 1.
    pub remaining_fraction: f32,
    pub reset_time: jiff::Timestamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum QuotaWindow {
    #[serde(rename = "5h")]
    FiveHours,
    #[serde(rename = "weekly")]
    Week,
    #[serde(other)]
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORKER: &str = "1e16b130-ab24-45b6-8ebf-b573ab82eb7f";

    #[test]
    fn steps_are_read_from_either_log() {
        let brain = std::env::temp_dir().join(format!("edualc-agy-steps-{}", std::process::id()));
        let id = SessionId::try_from(WORKER.to_owned()).expect("an id");
        let logs = generated(&brain, &id).join("logs");
        std::fs::create_dir_all(&logs).expect("a scratch brain");
        let [plain, full] = STEP_LOGS.map(|name| logs.join(name));

        assert_eq!(steps(&brain, &id), plain, "neither yet: the first's path");
        std::fs::write(&full, "").expect("written");
        assert_eq!(steps(&brain, &id), full);
        std::fs::write(&plain, "").expect("written");
        assert_eq!(steps(&brain, &id), plain);

        assert!(is_steps(&brain, &plain) && is_steps(&brain, &full));
        assert!(!is_steps(
            &brain,
            &generated(&brain, &id).join("swarm/board.jsonl")
        ));
        assert!(!is_steps(
            &brain,
            std::path::Path::new("/elsewhere/logs/transcript.jsonl")
        ));
        std::fs::remove_dir_all(&brain).expect("cleaned up");
    }

    #[test]
    fn a_switched_off_limit_is_left_out() {
        let groups = r#"{"groups":[{"name":"Gemini Models","buckets":[{"id":"gemini-weekly","window":"weekly","remaining_fraction":0.99,"reset_time":"2026-11-29T23:41:19Z"}]},{"name":"Claude and GPT models","buckets":[{"id":"3p-weekly","window":"weekly","remaining_fraction":0,"reset_time":"2026-11-24T23:50:29Z"},{"id":"3p-5h","window":"5h","disabled":true,"remaining_fraction":1}]}]}"#;
        let quotas: Quotas = serde_json::from_str(groups).expect("quotas");
        let windows: Vec<Vec<QuotaWindow>> = quotas
            .groups
            .iter()
            .map(|group| group.quotas.iter().map(|quota| quota.window).collect())
            .collect();
        assert_eq!(windows, [vec![QuotaWindow::Week], vec![QuotaWindow::Week]]);

        let unsaid = groups.replace(r#""disabled":true,"#, "");
        assert!(serde_json::from_str::<Quotas>(&unsaid).is_err());
    }

    #[test]
    fn quotas_read() {
        let answer: UsageAnswer = serde_json::from_str(
            r#"{"conversation_id":"","status":"SUCCESS","response":"…","command":{"name":"usage","data":{"description":"Within each group, models share a weekly limit and a 5-hour limit.","groups":[{"name":"Gemini Models","description":"Models within this group: Gemini 4 Hydrogen, Gemini 3.8 Flash","buckets":[{"id":"gemini-weekly","name":"Weekly Limit Remaining","window":"weekly","remaining_fraction":0.84,"reset_time":"2026-11-22T20:18:23Z"},{"id":"gemini-5h","name":"Five Hour Limit Remaining","window":"5h","remaining_fraction":1,"reset_time":"2026-11-16T13:26:14Z"}]},{"name":"Gemini 4 Argon","description":"For paid Pro plans, Argon quota is separate.","buckets":[{"id":"gemini-argon-wtu-weekly","name":"Gemini 4 Argon Weekly Limit","window":"weekly","remaining_fraction":0,"reset_time":"2026-11-22T20:14:06Z"}]}]}}}"#,
        )
        .expect("quotas");
        let groups = &answer.command.data.groups;
        let [gemini, argon] = groups.as_slice() else {
            panic!("two groups");
        };
        assert_eq!(
            gemini
                .quotas
                .iter()
                .map(|quota| quota.window)
                .collect::<Vec<_>>(),
            [QuotaWindow::Week, QuotaWindow::FiveHours]
        );
        assert!(gemini.covers("Gemini 3.8 Flash (High)"));
        assert!(gemini.covers("Gemini 4 Hydrogen (XHigh)"));
        assert!(!gemini.covers("Gemini 4 Argon (High)"));
        assert!(argon.covers("Gemini 4 Argon (High)"));
        assert!(argon.covers("Orchestrator (Argon)"));
        assert!(!gemini.covers("Orchestrator (Argon)"));
        assert!(gemini.covers("Orchestrator (Hydrogen)"));
    }

    #[test]
    fn steps_read() {
        let answer: Step = serde_json::from_str(
            r#"{"step_index":2,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-15T21:28:47Z","model":"m","input_tokens":1,"cache_read_tokens":0,"output_tokens":1,"thinking":"First, the brief.","tool_calls":[{"name":"run_command","args":{"CommandLine":"ls","toolSummary":"List"}}]}"#,
        )
        .expect("an answer");
        assert_eq!(answer.kind, StepKind::PlannerResponse);
        assert_eq!(answer.thinking.as_deref(), Some("First, the brief."));
        assert_eq!(
            answer.tool_calls.first().map(|call| call.name.as_str()),
            Some("run_command")
        );
        let failed: Step = serde_json::from_str(
            r#"{"step_index":4,"source":"MODEL","type":"GENERIC","status":"ERROR","created_at":"2026-11-15T21:29:00Z","error":"permission check failed"}"#,
        )
        .expect("a result");
        assert_eq!(
            (failed.kind, failed.status),
            (StepKind::Generic, StepStatus::Error)
        );
    }

    #[test]
    fn a_swarm_reads() {
        let swarm: Swarm = serde_json::from_str(&format!(
            r#"{{"title":"Two tracks","members":{{"acp-worker":{{"conversation_id":"{WORKER}","role":"worker","desired_state":"RUNNING","phase":"RUNNING","wake_mode":"mentions","killed":false,"model":"gemini-4-hydrogen-xhigh"}}}}}}"#
        ))
        .expect("members");
        let worker = swarm.members.get("acp-worker").expect("the worker");
        assert_eq!(worker.role, MemberRole::Worker);
        assert_eq!(worker.conversation_id.as_str(), WORKER);
        let subagent: Subagent = serde_json::from_str(&format!(
            r#"{{"conversationId":"{WORKER}","subagentDescriptor":{{"typeName":"worker","role":"Swarm Worker (acp-worker)"}},"state":"SUBAGENT_STATE_ALIVE","spawnStepIndex":366,"workspaceUris":[]}}"#
        ))
        .expect("a subagent");
        assert_eq!(subagent.state, SubagentState::Alive);
        let post: BoardPost = serde_json::from_str(
            r#"{"seq":2,"agent":"coordinator","text":"@acp-worker Start.","time":"2026-11-15 22:28:45","kind":"brief"}"#,
        )
        .expect("a post");
        assert_eq!((post.seq, post.agent.as_str()), (2, "coordinator"));
    }

    #[test]
    fn task_names_round_trip() {
        let id = SessionId::try_from(WORKER.to_owned()).expect("an id");
        for task in [AgyTask::Conversation(id.clone()), AgyTask::Board(id)] {
            assert_eq!(AgyTask::of(&task.id()), Some(task));
        }
        assert_eq!(AgyTask::of(&TaskId::from("a1b2c3d4".to_owned())), None);
        assert_eq!(AgyTask::of(&TaskId::from("agy-../../etc".to_owned())), None);
    }
}
