//! Claude Code's folder: its transcripts in `projects/`, its running sessions' records in
//! `sessions/`.

#[cfg(not(target_arch = "wasm32"))]
use std::collections::HashMap;
#[cfg(not(target_arch = "wasm32"))]
use std::fs::File;
#[cfg(not(target_arch = "wasm32"))]
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[cfg(not(target_arch = "wasm32"))]
use web_time::SystemTime;

#[cfg(not(target_arch = "wasm32"))]
use crate::external::claude_files::{AgentRecord, LiveRecord};
use crate::external::wire::TranscriptLine;
use crate::ids::SessionId;
#[cfg(not(target_arch = "wasm32"))]
use crate::process;

use super::titles::recorded_cwd;
#[cfg(not(target_arch = "wasm32"))]
use super::titles::summary;
#[cfg(not(target_arch = "wasm32"))]
use super::{
    LiveKind, LiveSession, LiveStatus, ProjectInfo, SessionInfo, backend_of, linked_worktree,
    read_small,
};

/// Claude Code's own folder, where it keeps its settings, transcripts and the records of the
/// sessions running: `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeHome {
    dir: PathBuf,
    /// `.claude.json`: beside the folder, or in it when `$CLAUDE_CONFIG_DIR` moves it.
    global_config: PathBuf,
}

impl ClaudeHome {
    /// Returns yours; `None` where neither `$CLAUDE_CONFIG_DIR` nor a home folder is set.
    pub fn of_user() -> Option<Self> {
        match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|dir| !dir.is_empty()) {
            Some(dir) => Some(Self::moved_to(PathBuf::from(dir))),
            None => dirs::home_dir().map(|home| Self {
                dir: home.join(".claude"),
                global_config: home.join(".claude.json"),
            }),
        }
    }

    /// Returns a home moved to `dir`, as `$CLAUDE_CONFIG_DIR` moves it, with `.claude.json` in
    /// it.
    pub fn moved_to(dir: PathBuf) -> Self {
        Self {
            global_config: dir.join(".claude.json"),
            dir,
        }
    }

    pub fn global_config(&self) -> &Path {
        &self.global_config
    }

    /// Returns the folder of the records of the sessions running.
    pub fn records(&self) -> PathBuf {
        self.dir.join("sessions")
    }

    fn projects(&self) -> PathBuf {
        self.dir.join("projects")
    }
}

/// `~/.claude.json` runs to hundreds of kilobytes; anything far larger isn't that file.
pub const GLOBAL_CONFIG_BYTES: u64 = 64 * 1024 * 1024;

/// A session record is a few hundred bytes; more is not one.
const RECORD_BYTES: u64 = 64 * 1024;
/// What is read of a transcript at once: whole sessions run to tens of megabytes.
const TRANSCRIPT_BYTES: u64 = 512 * 1024 * 1024;

/// Returns a project's folder name as Claude Code writes it: its path with each character but
/// a letter or a digit made a dash.
pub fn project_name(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect()
}

fn project_dir(projects: &Path, cwd: &Path) -> PathBuf {
    projects.join(project_name(cwd))
}

/// Returns the folders in `projects` a project's sessions are in: its own and those of its
/// worktrees. Claude Code keeps worktrees in `.claude/worktrees/`, and moves the transcript of
/// a session that enters one to the worktree's folder.
fn session_dirs(projects: &Path, cwd: &Path) -> Vec<PathBuf> {
    let name = project_name(cwd);
    let worktrees = format!("{name}--claude-worktrees-");
    let in_worktrees = std::fs::read_dir(projects)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(&worktrees))
        .map(|entry| entry.path());
    std::iter::once(projects.join(&name))
        .chain(in_worktrees)
        .collect()
}

/// Returns where a Claude Code session in `cwd` keeps its transcript.
pub fn transcript_path(home: &ClaudeHome, cwd: &Path, id: &SessionId) -> PathBuf {
    project_dir(&home.projects(), cwd).join(format!("{}.jsonl", id.as_str()))
}

/// Finds a session's transcript: under its folder's project, or another project's if it moved
/// there by entering a worktree. The newest wins if more than one has it.
pub fn find_transcript(home: &ClaudeHome, cwd: &Path, id: &SessionId) -> Option<PathBuf> {
    Some(transcript_path(home, cwd, id))
        .filter(|path| path.is_file())
        .or_else(|| transcript_in_any(&home.projects(), id))
}

/// Finds session `id`'s transcript in any project in `projects`; the newest if more than one
/// has it.
fn transcript_in_any(projects: &Path, id: &SessionId) -> Option<PathBuf> {
    let name = format!("{}.jsonl", id.as_str());
    newest(
        std::fs::read_dir(projects)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path().join(&name)),
    )
}

/// What the transcripts say of a session to continue.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Saved {
    /// No transcript of it is kept.
    Nowhere,
    /// The folder it was started in, where it goes on as the sidebar would open it.
    StartedIn(PathBuf),
    /// Its transcript names no folder in its first lines.
    Unplaced,
}

/// Returns what session `id`'s transcript says of it, whichever project keeps it.
pub fn saved(home: &ClaudeHome, id: &SessionId) -> Saved {
    match transcript_in_any(&home.projects(), id) {
        None => Saved::Nowhere,
        Some(transcript) => recorded_cwd(&transcript).map_or(Saved::Unplaced, Saved::StartedIn),
    }
}

/// Returns where session `id` working in `cwd` writes its transcript, or will (see
/// [`transcript_in`]).
pub fn written_transcript(home: &ClaudeHome, cwd: &Path, id: &SessionId) -> PathBuf {
    transcript_in(&home.projects(), cwd, id)
}

/// Returns where session `id` working in `cwd` has its transcript in `projects`: under its
/// folder's project, or another's if it moved there by entering a worktree. A transcript not
/// written yet goes under its folder's project.
fn transcript_in(projects: &Path, cwd: &Path, id: &SessionId) -> PathBuf {
    let name = format!("{}.jsonl", id.as_str());
    let own = projects.join(project_name(cwd)).join(&name);
    if own.is_file() {
        return own;
    }
    newest(
        std::fs::read_dir(projects)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path().join(&name)),
    )
    .unwrap_or(own)
}

/// Returns the most recently changed of the files that exist.
fn newest(paths: impl Iterator<Item = PathBuf>) -> Option<PathBuf> {
    paths
        .filter_map(|path| {
            let metadata = path.metadata().ok().filter(std::fs::Metadata::is_file)?;
            Some((metadata.modified().ok()?, path))
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path)
}

/// Lists the transcripts in a folder, newest first.
#[cfg(not(target_arch = "wasm32"))]
fn transcripts(dir: &Path) -> Vec<(PathBuf, SystemTime)> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::debug!(%error, dir = %dir.display(), "no transcripts");
            return Vec::new();
        }
    };
    let mut files: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .filter_map(|path| {
            let modified = path.metadata().and_then(|metadata| metadata.modified());
            match modified {
                Ok(modified) => Some((path, modified)),
                Err(error) => {
                    tracing::debug!(%error, path = %path.display(), "a transcript without a time");
                    None
                }
            }
        })
        .collect();
    files.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));
    files
}

/// Lists the sessions of the project in `cwd` and its worktrees, newest first. Sessions
/// without a prompt are left out, and those in `known` whose transcript hasn't changed aren't
/// read again.
#[cfg(not(target_arch = "wasm32"))]
pub fn list(
    home: &ClaudeHome,
    cwd: &Path,
    limit: usize,
    known: Vec<SessionInfo>,
) -> Vec<SessionInfo> {
    let mut known: HashMap<SessionId, SessionInfo> = known
        .into_iter()
        .map(|session| (session.id.clone(), session))
        .collect();
    let mut files: Vec<(PathBuf, SystemTime)> = session_dirs(&home.projects(), cwd)
        .iter()
        .flat_map(|dir| transcripts(dir))
        .collect();
    files.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));
    // A session in the project and in one of its worktrees is listed once, as its newer
    // transcript has it.
    let mut seen = std::collections::HashSet::new();
    files.retain(|(path, _)| seen.insert(path.file_name().map(std::ffi::OsStr::to_owned)));
    files
        .into_iter()
        .filter_map(|(path, modified)| {
            let id = SessionId::try_from(path.file_stem()?.to_string_lossy().into_owned()).ok()?;
            if let Some(same) = known
                .remove(&id)
                .filter(|session| session.modified == Some(modified))
            {
                return Some(same);
            }
            let found = summary(&path)?;
            Some(SessionInfo {
                backend: backend_of(&path),
                id,
                title: found.title,
                modified: Some(modified),
                worktree: found.worktree,
                last_said: found.last_said,
            })
        })
        .take(limit)
        .collect()
}

/// Lists every folder Claude Code was used in that still exists, most recent first.
#[cfg(not(target_arch = "wasm32"))]
pub fn projects(home: &ClaudeHome) -> Vec<ProjectInfo> {
    let folder = home.projects();
    let entries = match std::fs::read_dir(&folder) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::debug!(%error, dir = %folder.display(), "no projects");
            return Vec::new();
        }
    };
    let mut projects: Vec<_> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let (newest, modified) = transcripts(&entry.path()).into_iter().next()?;
            let path = recorded_cwd(&newest)?;
            path.is_dir().then_some(ProjectInfo {
                path,
                modified: Some(modified),
            })
        })
        .collect();
    projects.sort_by_key(|project| std::cmp::Reverse(project.modified));
    // A project whose sessions are kept in more than one folder, as a worktree's are, is
    // listed once.
    let mut seen = std::collections::HashSet::new();
    projects.retain(|project| seen.insert(project.path.clone()));
    projects
}

/// Lists the sessions Claude Code processes run now, but not our own (`claude -p`).
#[cfg(not(target_arch = "wasm32"))]
pub fn live(home: &ClaudeHome) -> Vec<LiveSession> {
    let dir = home.records();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::debug!(%error, "no running sessions");
            return Vec::new();
        }
    };
    let mut sessions: Vec<LiveSession> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "json"))
        .filter_map(|path| {
            let text = read_small(&path, RECORD_BYTES)?;
            let record: LiveRecord = match serde_json::from_str(&text) {
                Ok(record) => record,
                Err(error) => {
                    tracing::debug!(%error, path = %path.display(), "a session record that doesn't parse");
                    return None;
                }
            };
            // A record that can't tell its process from a later one with the same pid isn't
            // taken; nor this window's own.
            let started = record.proc_start?;
            let shown = matches!(record.kind, LiveKind::Interactive | LiveKind::Bg)
                && record.pid != std::process::id();
            (shown && process::is_running(record.pid, &started)).then(|| {
                let transcript = find_transcript(home, &record.cwd, &record.session_id);
                LiveSession {
                    pid: record.pid,
                    started,
                    worktree: linked_worktree(&record.cwd),
                    title: transcript
                        .as_deref()
                        .and_then(summary)
                        .map(|found| found.title),
                    agents: transcript
                        .as_deref()
                        .map_or(0, |transcript| running_agents(transcript, SystemTime::now())),
                    backend: transcript
                        .as_deref()
                        .map_or(crate::provider::Backend::Claude, backend_of),
                    id: record.session_id,
                    cwd: record.cwd,
                    name: record.name,
                    kind: record.kind,
                    status: record.status,
                }
            })
        })
        .collect();
    // Those waiting for their user first, then those at work, then the idle.
    sessions.sort_by_key(|session| {
        let urgency = match session.status {
            LiveStatus::Waiting => 0,
            LiveStatus::Busy => 1,
            LiveStatus::Idle => 2,
            LiveStatus::Other => 3,
        };
        (urgency, session.name.clone())
    });
    sessions
}

/// How long a subagent's transcript may go unwritten and the agent still count as running.
/// Claude Code marks an agent finished as it ends, but not when its session is killed.
const AGENT_QUIET: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Counts the subagents of the session at `transcript` that run at `now`: those whose record
/// (`<session>/subagents/agent-<id>.meta.json`) isn't marked finished, and whose transcript
/// was written within [`AGENT_QUIET`].
#[cfg(not(target_arch = "wasm32"))]
fn running_agents(transcript: &Path, now: SystemTime) -> usize {
    let Ok(entries) = std::fs::read_dir(transcript.with_extension("").join("subagents")) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry
                .file_name()
                .to_str()?
                .strip_suffix(".meta.json")?
                .to_owned();
            Some((
                entry.path(),
                entry.path().with_file_name(format!("{name}.jsonl")),
            ))
        })
        .filter(|(record, agent_transcript)| {
            let finished = read_small(record, RECORD_BYTES)
                .and_then(|text| serde_json::from_str::<AgentRecord>(&text).ok())
                .is_some_and(|record| record.finished);
            // Written after `now` (a clock set back, or `now` taken first): just written.
            let quiet_for = std::fs::metadata(agent_transcript)
                .and_then(|metadata| metadata.modified())
                .ok()
                .map(|written| now.duration_since(written).unwrap_or_default());
            !finished && quiet_for.is_some_and(|quiet| quiet < AGENT_QUIET)
        })
        .count()
}

/// Reads the whole lines of a transcript from `offset` on, and returns them with the offset
/// after the last.
#[cfg(not(target_arch = "wasm32"))]
pub fn read_lines_from(path: &Path, offset: u64) -> std::io::Result<(Vec<String>, u64)> {
    if !std::fs::metadata(path)?.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.take(TRANSCRIPT_BYTES).read_to_end(&mut bytes)?;
    // A line still being written is left for the next read.
    let whole = bytes
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map_or(0, |at| at + 1);
    let text = String::from_utf8_lossy(bytes.get(..whole).unwrap_or_default());
    let lines = text.lines().map(str::to_owned).collect();
    Ok((lines, offset + whole as u64))
}

/// Parses a transcript's lines, passing over those that don't parse.
pub fn parse_lines(lines: &[String]) -> Vec<TranscriptLine> {
    lines.iter().filter_map(|line| parse_line(line)).collect()
}

fn parse_line(line: &str) -> Option<TranscriptLine> {
    match serde_json::from_str(line) {
        Ok(parsed) => Some(parsed),
        Err(error) => {
            tracing::debug!(%error, "a transcript line that doesn't parse");
            None
        }
    }
}

/// Returns where Claude Code keeps the transcript of subagent `task` of the session at
/// `session`.
pub fn transcript_of(session: &Path, task: &crate::external::wire::TaskId) -> PathBuf {
    session
        .with_extension("")
        .join("subagents")
        .join(format!("agent-{task}.jsonl"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_browser_demos_transcripts_read_as_transcripts() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/demo");
        let transcripts: Vec<PathBuf> = std::fs::read_dir(&dir)
            .expect("the demo's folder")
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "jsonl")
            })
            .collect();
        assert_eq!(transcripts.len(), 11, "{transcripts:?}");
        for path in transcripts {
            let text = std::fs::read_to_string(&path).expect("a transcript");
            let lines: Vec<TranscriptLine> = text.lines().filter_map(parse_line).collect();
            assert_eq!(lines.len(), text.lines().count(), "{}", path.display());
            assert!(
                lines
                    .iter()
                    .any(|line| matches!(line, TranscriptLine::AiTitle { .. })),
                "{}",
                path.display()
            );
        }
    }

    #[test]
    fn project_dirs_are_named_like_claude_code_names_them() {
        let dir = project_dir(
            Path::new("/projects"),
            Path::new("/home/someone/code/my_project.v2"),
        );
        assert_eq!(dir, Path::new("/projects/-home-someone-code-my-project-v2"));
    }

    #[test]
    fn a_projects_sessions_include_its_worktrees() {
        let home = std::env::temp_dir().join(format!("edualc-homes-{}", std::process::id()));
        let cwd = Path::new("/code/beep");
        let own = home.join("-code-beep");
        let in_worktree = home.join("-code-beep--claude-worktrees-demo");
        let other = home.join("-code-beep-old");
        for dir in [&own, &in_worktree, &other] {
            std::fs::create_dir_all(dir).expect("a scratch transcripts folder");
        }
        let mut found = session_dirs(&home, cwd);
        found.sort();
        assert_eq!(found, [own, in_worktree]);
        std::fs::remove_dir_all(&home).expect("the scratch directory goes");
    }

    #[test]
    fn running_subagents_are_counted_from_their_records() {
        let dir = std::env::temp_dir().join(format!("edualc-agents-{}", std::process::id()));
        let subagents = dir.join("s1").join("subagents");
        std::fs::create_dir_all(&subagents).expect("a scratch directory");
        let agent = |id: &str, record: &str, transcript: bool| {
            std::fs::write(subagents.join(format!("agent-{id}.meta.json")), record)
                .expect("a record");
            if transcript {
                std::fs::write(subagents.join(format!("agent-{id}.jsonl")), "{}\n")
                    .expect("a transcript");
            }
        };
        agent("a1", r#"{"agentType":"Explore","description":"d"}"#, true);
        agent("a2", r#"{"agentType":"Explore","finished":true}"#, true);
        agent("a3", r#"{"agentType":"Explore","finished":false}"#, true);
        agent("a4", r#"{"agentType":"Explore"}"#, false);
        let session = dir.join("s1.jsonl");
        let written = std::fs::metadata(subagents.join("agent-a1.jsonl"))
            .and_then(|metadata| metadata.modified())
            .expect("a time it was written");
        assert_eq!(running_agents(&session, written), 2, "a1 and a3");
        // Written after `now` reads as just written, whatever order the files were made in.
        let before = written - std::time::Duration::from_secs(1);
        assert_eq!(running_agents(&session, before), 2, "a1 and a3");
        let later = written + AGENT_QUIET + std::time::Duration::from_secs(1);
        assert_eq!(running_agents(&session, later), 0, "quiet too long");
        assert_eq!(running_agents(&dir.join("none.jsonl"), written), 0);
        std::fs::remove_dir_all(&dir).expect("the scratch directory goes");
    }

    #[test]
    fn a_transcript_in_a_home_is_found_where_it_went() {
        let projects = std::env::temp_dir().join(format!("edualc-home-{}", std::process::id()));
        let cwd = Path::new("/w/app");
        let id =
            SessionId::try_from("0b8f1c2d-3e4f-4a5b-8c6d-7e8f9a0b1c2d".to_owned()).expect("an id");
        let own = projects.join("-w-app").join(format!("{id}.jsonl"));
        assert_eq!(transcript_in(&projects, cwd, &id), own, "not written yet");
        let moved = projects.join("-w-app-wt").join(format!("{id}.jsonl"));
        std::fs::create_dir_all(moved.parent().expect("a folder")).expect("a project folder");
        std::fs::write(&moved, "").expect("a transcript");
        assert_eq!(
            transcript_in(&projects, cwd, &id),
            moved,
            "moved to a worktree"
        );
        std::fs::create_dir_all(own.parent().expect("a folder")).expect("a project folder");
        std::fs::write(&own, "").expect("a transcript");
        let later = SystemTime::now() + std::time::Duration::from_secs(60);
        File::options()
            .write(true)
            .open(&moved)
            .and_then(|file| file.set_modified(later))
            .expect("the moved one newer");
        assert_eq!(
            transcript_in(&projects, cwd, &id),
            own,
            "its folder's first, if newer elsewhere"
        );
        std::fs::remove_dir_all(&projects).expect("the scratch folder goes");
    }

    #[test]
    fn a_session_goes_on_where_it_was_started() {
        let scratch = std::env::temp_dir().join(format!("edualc-started-{}", std::process::id()));
        let home = ClaudeHome::moved_to(scratch.clone());
        let projects = scratch.join("projects");
        let id =
            SessionId::try_from("5d6e7f80-91a2-4b3c-8d4e-5f60718293a4".to_owned()).expect("an id");
        let transcript = projects.join("-w-app").join(format!("{id}.jsonl"));
        std::fs::create_dir_all(transcript.parent().expect("a folder")).expect("a project");
        let line = |cwd: &str| json!({ "type": "user", "cwd": cwd, "sessionId": id.as_str() });
        std::fs::write(
            &transcript,
            format!("{}\n{}\n", line("/w/app"), line("/somewhere/else")),
        )
        .expect("a transcript");
        assert_eq!(saved(&home, &id), Saved::StartedIn(PathBuf::from("/w/app")));
        let unknown =
            SessionId::try_from("00000000-1111-4222-8333-444444444444".to_owned()).expect("an id");
        assert_eq!(saved(&home, &unknown), Saved::Nowhere);
        let unplaced = projects.join("-w-lib").join(format!("{unknown}.jsonl"));
        std::fs::create_dir_all(unplaced.parent().expect("a folder")).expect("a project");
        std::fs::write(&unplaced, format!("{}\n", json!({ "type": "summary" })))
            .expect("a transcript");
        assert_eq!(saved(&home, &unknown), Saved::Unplaced);
        std::fs::remove_dir_all(&scratch).expect("the scratch folder goes");
    }

    #[test]
    fn a_line_still_being_written_waits() {
        let dir = std::env::temp_dir().join(format!("edualc-transcript-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let path = dir.join("t.jsonl");
        let whole = format!("{}\n", json!({ "type": "summary", "summary": "a" }));
        let next = json!({ "type": "summary", "summary": "b" }).to_string();
        let unfinished: String = next.chars().take(next.len() / 2).collect();
        std::fs::write(&path, format!("{whole}{unfinished}")).expect("a transcript");
        let (lines, offset) = read_lines_from(&path, 0).expect("readable");
        assert_eq!(lines.len(), 1);
        assert_eq!(offset, whole.len() as u64);
        std::fs::remove_dir_all(&dir).expect("the scratch directory goes");
    }
}
