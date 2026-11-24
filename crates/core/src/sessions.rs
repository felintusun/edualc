//! Claude Code's sessions on disk: saved ones in `~/.claude/projects`, running ones in
//! `~/.claude/sessions`.

#[cfg(not(target_arch = "wasm32"))]
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use web_time::SystemTime;

use crate::chat::UserText;
#[cfg(not(target_arch = "wasm32"))]
use crate::external::claude_files::{AgentRecord, LiveRecord};
use crate::external::wire::{self, MessageOrigin, TranscriptCwd, TranscriptLine, UserContent};
use crate::ids::SessionId;
#[cfg(not(target_arch = "wasm32"))]
use crate::process;

/// A session's two words, an adjective and a noun picked by its id, so they stay the same each
/// time it runs. edualcd names a session's process by them, and Claude Code keeps that name as
/// the session's custom title too, which is then a name and not a title.
#[derive(Debug, Clone, PartialEq, Eq, derive_more::Display)]
pub struct PetName(String);

impl PetName {
    pub fn of(id: &SessionId) -> Self {
        fn word<'list>(list: &[&'list str], number: usize) -> &'list str {
            list.get(number % list.len().max(1))
                .copied()
                .unwrap_or("session")
        }
        let words = petname::Petnames::small();
        Self(format!(
            "{}-{}",
            word(&words.adjectives, id_number(id, 0)),
            word(&words.nouns, id_number(id, 8))
        ))
    }

    /// Returns whether `text` holds the two words, as every name edualc gave the session does.
    fn is_in(&self, text: &str) -> bool {
        text.contains(self.0.as_str())
    }
}

/// The name other sessions message a session's process by (`--name`). It stays the same each
/// time the session runs; Claude Code's own name comes from the process id and changes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, derive_more::Display)]
pub struct MessagingName(String);

impl MessagingName {
    /// Returns the name of session `id` in `cwd`: its [`PetName`], the folder's name, and two
    /// digits picked by the id.
    pub fn of(id: &SessionId, cwd: &Path) -> Self {
        Self(format!(
            "{}-{}-{:02}",
            PetName::of(id),
            project_part(cwd),
            id_number(id, 16) % 100
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The most of the folder's name a session's name takes.
const PROJECT_PART_CHARS: usize = 20;

/// Returns the folder's name as Claude Code writes it in names (`claude_but_sane` as
/// `claude-but-sane`): lowercase letters and digits, the rest a dash, at most
/// [`PROJECT_PART_CHARS`]. A folder with no such letters gives "project".
fn project_part(cwd: &Path) -> String {
    let folder = cwd
        .file_name()
        .map_or_default(|name| name.to_string_lossy().to_lowercase());
    let mut part = String::new();
    for character in folder.chars() {
        if character.is_ascii_alphanumeric() {
            part.push(character);
        } else if !part.is_empty() && !part.ends_with('-') {
            part.push('-');
        }
    }
    let part: String = part.chars().take(PROJECT_PART_CHARS).collect();
    match part.trim_end_matches('-') {
        "" => "project".to_owned(),
        trimmed => trimmed.to_owned(),
    }
}

/// Returns a number from a session's id: eight of its hex digits, from the `at`th on.
pub fn id_number(id: &SessionId, at: usize) -> usize {
    let digits: String = id
        .as_str()
        .chars()
        .filter(char::is_ascii_hexdigit)
        .collect();
    digits
        .get(at..at + 8)
        .and_then(|hex| u32::from_str_radix(hex, 16).ok())
        .and_then(|number| usize::try_from(number).ok())
        .unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionInfo {
    pub backend: crate::provider::Backend,
    pub id: SessionId,
    pub title: String,
    /// When it last changed; `None` where the program doesn't say.
    pub modified: Option<SystemTime>,
    /// The worktree it last worked in, if that still exists.
    pub worktree: Option<Worktree>,
    /// The first sentence of the last assistant reply, if one is in the tail.
    #[serde(default)]
    pub last_said: Option<String>,
}

/// A git worktree a session works in (`claude --worktree`, the EnterWorktree tool).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Worktree {
    pub path: PathBuf,
    pub name: String,
    pub branch: String,
}

impl Worktree {
    fn existing(session: wire::WorktreeSession) -> Option<Self> {
        session.path.is_dir().then(|| Self {
            name: if session.name.is_empty() {
                folder_name(&session.path)
            } else {
                session.name
            },
            path: session.path,
            branch: session.branch,
        })
    }
}

/// Returns `path` as a worktree if it is a linked one, whose `.git` is a file naming the real
/// one.
pub fn linked_worktree(path: &Path) -> Option<Worktree> {
    let pointer = read_small(&path.join(".git"), GIT_POINTER_BYTES)?;
    let gitdir = PathBuf::from(pointer.strip_prefix("gitdir:")?.trim());
    let gitdir = if gitdir.is_absolute() {
        gitdir
    } else {
        path.join(gitdir)
    };
    let head = read_small(&gitdir.join("HEAD"), GIT_POINTER_BYTES).unwrap_or_default();
    let branch = head
        .trim()
        .strip_prefix("ref: refs/heads/")
        .unwrap_or("(detached)")
        .to_owned();
    Some(Worktree {
        path: path.to_owned(),
        name: folder_name(path),
        branch,
    })
}

/// Returns the last part of a path, for a title; the whole path for `/`.
pub fn folder_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectInfo {
    pub path: PathBuf,
    /// When a session in it last changed; `None` where no program says.
    pub modified: Option<SystemTime>,
}

/// A session a Claude Code process runs now, in a terminal, an editor or the background, as
/// the process reports itself.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct LiveSession {
    pub pid: u32,
    /// When its process started, to tell it from a later one with the same pid.
    pub started: String,
    pub id: SessionId,
    pub cwd: PathBuf,
    /// The name other sessions message it by.
    pub name: String,
    /// The title its transcript gives, once it has one.
    pub title: Option<String>,
    pub kind: LiveKind,
    pub status: LiveStatus,
    pub worktree: Option<Worktree>,
    /// Its subagents running now, as their records tell (see [`running_agents`]).
    pub agents: usize,
    /// The program it runs, told by where its transcript is; Claude Code before it has one.
    pub backend: crate::provider::Backend,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveKind {
    Interactive,
    /// Started with `--bg`.
    Bg,
    #[default]
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveStatus {
    Idle,
    Busy,
    /// Waiting for its user: a permission request or a question.
    Waiting,
    #[default]
    #[serde(other)]
    Other,
}

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

/// Returns whether `transcript` is in agy's brain folder.
#[cfg(not(target_arch = "wasm32"))]
fn is_agy_brain_path(transcript: &Path) -> bool {
    crate::external::agy::brain().is_some_and(|brain| transcript.starts_with(&brain))
}

#[cfg(target_arch = "wasm32")]
fn is_agy_brain_path(_transcript: &Path) -> bool {
    false
}

/// Returns whether agy keeps the steps of a session `id` in its brain folder.
#[cfg(not(target_arch = "wasm32"))]
pub fn is_agy_brain_session(id: &SessionId) -> bool {
    crate::external::agy::brain()
        .is_some_and(|brain| crate::external::agy::steps(&brain, id).is_file())
}

#[cfg(target_arch = "wasm32")]
pub fn is_agy_brain_session(_id: &SessionId) -> bool {
    false
}

/// Returns the program a session saved at `transcript` runs, told by where the file is.
pub fn backend_of(transcript: &Path) -> crate::provider::Backend {
    if is_agy_brain_path(transcript) {
        crate::provider::Backend::Agy
    } else {
        crate::provider::Backend::Claude
    }
}

/// Returns whether `session_cwd` belongs to the project `project_cwd`: it is the project, one
/// of its `.claude/worktrees/`, or a linked git worktree whose gitdir is in its `.git`.
#[cfg(not(target_arch = "wasm32"))]
pub fn belongs_to_project(session_cwd: &Path, project_cwd: &Path) -> bool {
    if session_cwd == project_cwd
        || session_cwd.starts_with(project_cwd.join(".claude").join("worktrees"))
    {
        return true;
    }
    let Some(pointer) = read_small(&session_cwd.join(".git"), GIT_POINTER_BYTES) else {
        return false;
    };
    let Some(gitdir) = pointer.strip_prefix("gitdir:").map(str::trim) else {
        return false;
    };
    let gitdir = if Path::new(gitdir).is_absolute() {
        PathBuf::from(gitdir)
    } else {
        session_cwd.join(gitdir)
    };
    gitdir.starts_with(project_cwd.join(".git"))
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
pub fn running_agents(transcript: &Path, now: SystemTime) -> usize {
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

/// A `.git` file or a `HEAD` names a path or a branch.
const GIT_POINTER_BYTES: u64 = 4096;
/// A session record is a few hundred bytes; more is not one.
const RECORD_BYTES: u64 = 64 * 1024;
/// What is read of a transcript at once: whole sessions run to tens of megabytes.
const TRANSCRIPT_BYTES: u64 = 512 * 1024 * 1024;

/// Reads a small regular file's text. A FIFO would block, and a device or a huge file could be
/// put where a record or `.git` is expected by anything running as you.
fn read_small(path: &Path, limit: u64) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > limit {
        return None;
    }
    let mut text = String::new();
    File::open(path)
        .ok()?
        .take(limit)
        .read_to_string(&mut text)
        .ok()?;
    Some(text)
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

/// Returns where Claude Code keeps the transcript of subagent `task` of the session at
/// `session`.
pub fn transcript_of(session: &Path, task: &crate::external::wire::TaskId) -> PathBuf {
    session
        .with_extension("")
        .join("subagents")
        .join(format!("agent-{task}.jsonl"))
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

/// Titles are appended as the session goes on; prompts are at the start. Neither needs the
/// whole file, which can be many megabytes.
const PEEK_BYTES: u64 = 128 * 1024;
/// How much of a transcript's start is read for its first prompt and `cwd`, past a snapshot or
/// an attachment longer than [`PEEK_BYTES`].
const HEAD_SCAN_BYTES: u64 = 4 * 1024 * 1024;

struct Summary {
    title: String,
    worktree: Option<Worktree>,
    last_said: Option<String>,
}

/// Reads a session's title, the worktree it last worked in, and the start of its last reply.
fn summary(path: &Path) -> Option<Summary> {
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let tail = read_range(&mut file, length.saturating_sub(PEEK_BYTES), length)?;
    let (mut custom, mut generated, mut summary, mut worktree, mut last_said) =
        (None, None, None, None, None);
    let own_name = path
        .file_stem()
        .and_then(|stem| SessionId::try_from(stem.to_string_lossy().into_owned()).ok())
        .map(|id| PetName::of(&id));
    for line in tail.lines().rev() {
        let want_title = custom.is_none() && line.contains("itle\"")
            || generated.is_none() && line.contains("aiTitle")
            || summary.is_none() && line.contains("\"summary\"")
            || worktree.is_none() && line.contains("worktree-state");
        let want_said =
            last_said.is_none() && line.contains("\"assistant\"") && line.contains("\"text\"");
        if !want_title && !want_said {
            continue;
        }
        match serde_json::from_str(line) {
            Ok(TranscriptLine::CustomTitle { custom_title }) if custom.is_none() => {
                if !own_name
                    .as_ref()
                    .is_some_and(|name| name.is_in(&custom_title))
                {
                    custom = Some(custom_title);
                }
            }
            Ok(TranscriptLine::AiTitle { ai_title }) if generated.is_none() => {
                generated = Some(ai_title);
            }
            Ok(TranscriptLine::Summary { summary: text }) if summary.is_none() => {
                summary = Some(text);
            }
            Ok(TranscriptLine::WorktreeState { worktree: state }) if worktree.is_none() => {
                worktree = Some(state);
            }
            Ok(TranscriptLine::Assistant {
                message,
                is_sidechain: false,
            }) if last_said.is_none() => {
                if let Some(said) = assistant_sentence(&message.content) {
                    last_said = Some(said);
                }
            }
            // The first line of the tail may be cut; other lines aren't titles.
            Ok(_) | Err(_) => {}
        }
        if custom.is_some() && worktree.is_some() && last_said.is_some() {
            break;
        }
    }
    let worktree = worktree.flatten().and_then(Worktree::existing);
    if let Some(title) = custom.or(generated).or(summary) {
        return Some(Summary {
            title: title_line(&title).to_owned(),
            worktree,
            last_said,
        });
    }
    // The first prompt; or, for a session another one started by writing to it, that message,
    // whose turn can fill more than the head before the user writes anything.
    file.seek(SeekFrom::Start(0)).ok()?;
    head_lines(file)
        .filter(|line| line.contains("\"user\""))
        .find_map(|line| match serde_json::from_str(&line) {
            Ok(TranscriptLine::User {
                is_sidechain: false,
                origin: Some(MessageOrigin::Peer { name, body }),
                ..
            }) => Some(peer_title(name.as_deref(), body.as_deref())),
            Ok(TranscriptLine::User {
                message,
                is_meta: false,
                is_sidechain: false,
                ..
            }) => prompt_text(&message.content),
            Ok(_) | Err(_) => None,
        })
        .map(|prompt| Summary {
            title: crate::tools::truncate_chars(title_line(&prompt), 80),
            worktree,
            last_said,
        })
}

/// Returns the first readable sentence an assistant turn wrote, for cards and rows.
pub fn assistant_sentence(blocks: &[crate::external::wire::ContentBlock]) -> Option<String> {
    let combined = blocks
        .iter()
        .filter_map(|block| match block {
            crate::external::wire::ContentBlock::Text { text } => Some(text.as_str()),
            crate::external::wire::ContentBlock::Thinking { .. }
            | crate::external::wire::ContentBlock::ToolUse { .. }
            | crate::external::wire::ContentBlock::ServerToolUse { .. }
            | crate::external::wire::ContentBlock::ToolResult { .. }
            | crate::external::wire::ContentBlock::Image { .. }
            | crate::external::wire::ContentBlock::Other => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    first_sentence(&combined)
}

pub fn first_sentence(text: &str) -> Option<String> {
    let mut line = String::new();
    let mut heading_fallback: Option<&str> = None;
    for raw in text.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with("```") {
            if line.is_empty() {
                continue;
            }
            break;
        }
        if trimmed.starts_with('#') {
            if line.is_empty() && heading_fallback.is_none() {
                let heading = trimmed.trim_start_matches('#').trim_start();
                if !heading.is_empty() {
                    heading_fallback = Some(heading);
                }
            }
            continue;
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(trimmed);
        if ends_sentence(&line) {
            break;
        }
    }
    let chosen = if line.is_empty() {
        heading_fallback?
    } else {
        split_first_sentence(&line)
    };
    Some(crate::tools::truncate_chars(chosen.trim(), 140))
}

fn ends_sentence(text: &str) -> bool {
    text.ends_with(['.', '!', '?', '。', '！', '？'])
}

fn split_first_sentence(text: &str) -> &str {
    let mut chars = text.char_indices().peekable();
    while let Some((at, ch)) = chars.next() {
        if matches!(ch, '。' | '！' | '？') {
            return text.get(..at + ch.len_utf8()).unwrap_or(text);
        }
        if matches!(ch, '.' | '!' | '?') {
            match chars.peek() {
                None => return text,
                Some(&(_, next)) if next.is_whitespace() => {
                    return text.get(..at + ch.len_utf8()).unwrap_or(text);
                }
                Some(_) => {}
            }
        }
    }
    text
}

/// Returns the first line with text in it, as a title is shown on one line.
fn title_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
}

/// Makes a title from another session's message: who sent it and what it says, as far as
/// known.
fn peer_title(name: Option<&str>, body: Option<&str>) -> String {
    match (name, body) {
        (Some(name), Some(body)) => format!("{name}: {}", body.trim()),
        (None, Some(body)) => body.to_owned(),
        (Some(name), None) => format!("Message from {name}"),
        (None, None) => "Message from another session".to_owned(),
    }
}

/// Returns the worktree a session works in now, as its transcript last said.
pub fn worktree_of(transcript: &Path) -> Option<Worktree> {
    summary(transcript).and_then(|found| found.worktree)
}

fn recorded_cwd(path: &Path) -> Option<PathBuf> {
    head_lines(File::open(path).ok()?)
        .filter(|line| line.contains("\"cwd\""))
        .find_map(|line| serde_json::from_str::<TranscriptCwd>(&line).ok())
        .map(|record| record.cwd)
}

/// Returns the prompt a user message holds, if any; a prompt sent after `!` commands follows
/// their rows.
fn prompt_text(content: &UserContent) -> Option<String> {
    let (whole, blocks) = match content {
        UserContent::Text(text) => (Some(text.as_str()), [].as_slice()),
        UserContent::Blocks(blocks) => (None, blocks.as_slice()),
    };
    let block_texts = blocks.iter().filter_map(|block| match block {
        wire::ContentBlock::Text { text } => Some(text.as_str()),
        wire::ContentBlock::Thinking { .. }
        | wire::ContentBlock::ToolUse { .. }
        | wire::ContentBlock::ServerToolUse { .. }
        | wire::ContentBlock::ToolResult { .. }
        | wire::ContentBlock::Image { .. }
        | wire::ContentBlock::Other => None,
    });
    whole
        .into_iter()
        .chain(block_texts)
        .find_map(|text| match UserText::of(text) {
            UserText::Prompt(text) => Some(text.to_owned()),
            UserText::Command { .. }
            | UserText::Shell(_)
            | UserText::ShellOutput { .. }
            | UserText::Stdout(_)
            | UserText::Stderr(_)
            | UserText::Interrupted
            | UserText::ForModel
            | UserText::Empty => None,
        })
}

/// Reads the lines at the start of a transcript, only as far as they are wanted.
fn head_lines(file: File) -> impl Iterator<Item = String> {
    BufReader::new(file.take(HEAD_SCAN_BYTES))
        .lines()
        .map_while(Result::ok)
}

fn read_range(file: &mut File, start: u64, end: u64) -> Option<String> {
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(end - start).read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Says how long ago, briefly: "now", "5m", "3h", "2d", "6w"; nothing for no time.
pub fn age(modified: Option<SystemTime>) -> String {
    let Some(modified) = modified else {
        return String::new();
    };
    let seconds = SystemTime::now()
        .duration_since(modified)
        .map_or(0, |elapsed| elapsed.as_secs());
    match seconds {
        0..60 => "now".into(),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h", seconds / 3600),
        86400..604_800 => format!("{}d", seconds / 86400),
        _ => format!("{}w", seconds / 604_800),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_session_is_named_by_its_project_and_id() {
        let id = |text: &str| SessionId::try_from(text.to_owned()).expect("a session id");
        let project = Path::new("/home/someone/code/claude_but_sane");
        let session = "d1ba2743-c47a-4446-aefc-d4594dbc045d";
        let name = MessagingName::of(&id(session), project);
        assert_eq!(
            name,
            MessagingName::of(&id(session), project),
            "started again"
        );
        let mut parts = name.0.splitn(3, '-');
        let (Some(adjective), Some(noun), Some(rest)) = (parts.next(), parts.next(), parts.next())
        else {
            panic!("two words first in {name}");
        };
        let words = petname::Petnames::small();
        assert!(words.adjectives.contains(&adjective), "{name}");
        assert!(words.nouns.contains(&noun), "{name}");
        let number = rest
            .strip_prefix("claude-but-sane-")
            .expect("named after its project");
        assert!(
            number.len() == 2 && number.chars().all(|digit| digit.is_ascii_digit()),
            "{name}"
        );
        let names: std::collections::HashSet<MessagingName> = (0..200)
            .map(|_| MessagingName::of(&SessionId::new_random(), project))
            .collect();
        assert!(names.len() > 195, "only {} names of 200", names.len());
    }

    #[test]
    fn a_project_is_named_after_its_folder() {
        assert_eq!(
            project_part(Path::new("/code/claude_but_sane")),
            "claude-but-sane"
        );
        assert_eq!(
            project_part(Path::new("/code/My Project.v2")),
            "my-project-v2"
        );
        assert_eq!(
            project_part(Path::new("/code/a-very-long-project-name-indeed")),
            "a-very-long-project"
        );
        assert_eq!(project_part(Path::new("/home/someone/文档")), "project");
        assert_eq!(project_part(Path::new("/")), "project");
    }

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
    fn a_linked_worktree_names_its_branch() {
        let dir = std::env::temp_dir().join(format!("edualc-worktree-{}", std::process::id()));
        let gitdir = dir.join("repo-git-worktrees-w");
        let worktree = dir.join("w");
        std::fs::create_dir_all(&gitdir).expect("a scratch git dir");
        std::fs::create_dir_all(&worktree).expect("a scratch worktree");
        std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/feature\n").expect("a HEAD");
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .expect("a .git file");
        let found = linked_worktree(&worktree).expect("a linked worktree");
        assert_eq!(
            (found.name.as_str(), found.branch.as_str()),
            ("w", "feature")
        );
        assert_eq!(linked_worktree(&dir), None);
        std::fs::remove_dir_all(&dir).expect("the scratch directory goes");
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

    #[test]
    fn edualcs_name_for_a_session_is_no_title() {
        let id = "291365c8-6d45-4dba-ba19-41a5f0eccd6b";
        let pet = PetName::of(&SessionId::try_from(id.to_owned()).expect("an id"));
        let dir = std::env::temp_dir().join(format!("edualc-titles-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let path = dir.join(format!("{id}.jsonl"));
        let title_after = |custom: &str| {
            let lines = [
                json!({ "type": "ai-title", "aiTitle": "Floating NPU screen describer" }),
                json!({ "type": "custom-title", "customTitle": custom }),
            ];
            let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
            std::fs::write(&path, text).expect("a transcript");
            summary(&path).map(|found| found.title)
        };
        for name in [
            format!("edualc-{pet}"),
            format!("edualc-local-npu-screen-con-{pet}-81"),
            format!("{pet}-local-npu-screen-con-81"),
        ] {
            assert_eq!(
                title_after(&name).as_deref(),
                Some("Floating NPU screen describer"),
                "{name}"
            );
        }
        assert_eq!(
            title_after("NPU describer, native").as_deref(),
            Some("NPU describer, native")
        );
        std::fs::remove_dir_all(&dir).expect("the scratch directory goes");
    }

    #[test]
    fn a_title_is_one_line() {
        let dir = std::env::temp_dir().join(format!("edualc-one-line-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let path = dir.join("80bdeda4-8427-4be3-b815-ea82e7490483.jsonl");
        let title_of = |line: serde_json::Value| {
            std::fs::write(&path, format!("{line}\n")).expect("a transcript");
            summary(&path).map(|found| found.title)
        };
        let prompt = json!({
            "type": "user",
            "message": { "role": "user", "content": "\n  Check the kitchen\nThen move on to the hall" },
        });
        assert_eq!(title_of(prompt).as_deref(), Some("Check the kitchen"));
        let titled = json!({ "type": "custom-title", "customTitle": "Two\nlines" });
        assert_eq!(title_of(titled).as_deref(), Some("Two"));
        std::fs::remove_dir_all(&dir).expect("cleaned up");
    }

    #[test]
    fn a_session_another_started_is_titled_by_its_message() {
        let dir = std::env::temp_dir().join(format!("edualc-peer-titles-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let path = dir.join("80bdeda4-8427-4be3-b815-ea82e7490482.jsonl");
        let title_of = |lines: &[serde_json::Value]| {
            let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
            std::fs::write(&path, text).expect("a transcript");
            summary(&path).map(|found| found.title)
        };
        let peer = json!({
            "type": "user",
            "isMeta": true,
            "message": { "role": "user", "content": "Another Claude session sent a message: …" },
            "origin": { "kind": "peer", "from": "unknown", "name": "sterling-gibbon-94",
                        "body": "Please reply with one sentence." },
        });
        let filler = json!({ "type": "attachment", "attachment": {
            "type": "skill_listing", "content": "x".repeat(usize::try_from(PEEK_BYTES).expect("small")) } });
        let prompt = json!({ "type": "user", "message": { "role": "user", "content": "Send a second note" } });
        assert_eq!(
            title_of(&[peer.clone(), filler, prompt.clone()]).as_deref(),
            Some("sterling-gibbon-94: Please reply with one sentence.")
        );
        assert_eq!(
            title_of(&[prompt, peer]).as_deref(),
            Some("Send a second note")
        );
        std::fs::remove_dir_all(&dir).expect("the scratch directory goes");
    }

    #[test]
    fn last_said_reads_the_first_sentence_of_the_last_assistant_reply() {
        let dir = std::env::temp_dir().join(format!("edualc-last-said-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let path = dir.join("80bdeda4-8427-4be3-b815-ea82e7490489.jsonl");
        let lines = [
            json!({ "type": "user", "message": { "role": "user", "content": "Check tests" } }),
            json!({
                "type": "assistant",
                "message": { "role": "assistant", "content": [{ "type": "text", "text": "Looking at that." }] }
            }),
            json!({
                "type": "assistant",
                "isSidechain": true,
                "message": { "role": "assistant", "content": [{ "type": "text", "text": "Sidechain thought." }] }
            }),
            json!({
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [{ "type": "text", "text": "## Summary\nAll three tests pass now. The cache stays untouched." }]
                }
            }),
        ];
        let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
        std::fs::write(&path, text).expect("a transcript");
        let found = summary(&path).expect("a summary");
        assert_eq!(found.title, "Check tests");
        assert_eq!(
            found.last_said.as_deref(),
            Some("All three tests pass now.")
        );
        assert_eq!(
            first_sentence("All three tests pass now. The cache stays untouched.").as_deref(),
            Some("All three tests pass now.")
        );
        assert_eq!(
            first_sentence("日本語の文です。次の文。").as_deref(),
            Some("日本語の文です。")
        );
        std::fs::remove_dir_all(&dir).expect("the scratch directory goes");
    }

    #[test]
    fn summary_survives_oversized_head_lines_on_large_transcripts() {
        let dir = std::env::temp_dir().join(format!("edualc-oversized-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let path = dir.join("80bdeda4-8427-4be3-b815-ea82e7490490.jsonl");
        let huge_snapshot = json!({
            "type": "file-history-snapshot",
            "payload": "x".repeat(256 * 1024),
        });
        let prompt = json!({
            "type": "user",
            "message": { "role": "user", "content": "Recover after huge snapshot" },
        });
        let reply = json!({
            "type": "assistant",
            "message": { "role": "assistant", "content": [{ "type": "text", "text": "Recovered cleanly." }] },
        });
        std::fs::write(&path, format!("{huge_snapshot}\n{prompt}\n{reply}\n"))
            .expect("a transcript");
        let found = summary(&path).expect("summary past >128KB head line");
        assert_eq!(found.title, "Recover after huge snapshot");
        assert_eq!(found.last_said.as_deref(), Some("Recovered cleanly."));
        std::fs::remove_dir_all(&dir).expect("the scratch directory goes");
    }

    #[test]
    fn external_ids_map_deterministically_and_agys_records_tell_its_sessions() {
        let uuid = "e4cd185b-4679-4fa9-8245-69f15bdbbbe1";
        assert_eq!(
            SessionId::for_external(crate::provider::Backend::Agy, uuid).as_str(),
            uuid
        );
        let ses_a = SessionId::for_external(crate::provider::Backend::Opencode, "ses_abc123");
        let ses_b = SessionId::for_external(crate::provider::Backend::Opencode, "ses_abc123");
        let ses_c = SessionId::for_external(crate::provider::Backend::Opencode, "ses_xyz789");
        assert_eq!(ses_a, ses_b);
        assert_ne!(ses_a, ses_c);
        // The UUIDv5 of "opencode:ses_abc123" in edualc's namespace, as Python's uuid5 makes it:
        // a session's id must not change between releases, or its Changes snapshots are lost.
        assert_eq!(ses_a.as_str(), "5752e0e1-4cd0-5d87-930c-4104ab8263aa");
        if let Some(brain) = crate::external::agy::brain() {
            assert_eq!(
                backend_of(
                    &brain
                        .join(uuid)
                        .join(".system_generated/logs/transcript.jsonl")
                ),
                crate::provider::Backend::Agy
            );
        }
    }
}
