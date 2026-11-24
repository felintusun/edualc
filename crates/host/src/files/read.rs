//! What the daemon reads for a window: the sessions, the projects, a transcript.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use tokio::sync::mpsc;

use crate::acp::{ExternalSession, brain};
use crate::external::agy::{self, AgyTask};
use crate::external::wire::TranscriptLine;
use crate::folder::Folder;
use crate::ids::SessionId;
use crate::ipc::{self, ActId, Answer, FromDaemon, Place, ProjectOverview, Read};
use crate::provider::Backend;
use crate::sessions::{
    ClaudeHome, LiveKind, LiveSession, LiveStatus, MessagingName, ProjectInfo, SessionInfo,
};
use crate::{sessions, snapshots};

use super::Outbox;
use super::catalog::{Catalog, query_catalog};

/// The most sessions listed at once.
const MOST_SESSIONS: usize = 1000;

/// A transcript, by where it is and how it is written.
#[derive(Debug)]
pub enum Source {
    /// In Claude Code's format, which the ACP adapter writes too.
    Transcript(PathBuf),
    /// An agy conversation's steps, read as its transcript (see [`brain::step_line`]).
    AgySteps(PathBuf),
    /// An agy swarm's board, read as what its members said (see [`brain::post_line`]).
    AgyBoard(PathBuf),
}

impl Source {
    pub fn path(&self) -> &Path {
        match self {
            Self::Transcript(path) | Self::AgySteps(path) | Self::AgyBoard(path) => path,
        }
    }

    /// Reads the transcript's whole lines after byte `from`, in Claude Code's format, and
    /// returns them with where the next read starts.
    fn read_from(&self, from: u64) -> std::io::Result<(Vec<String>, u64)> {
        let (lines, offset) = sessions::read_lines_from(self.path(), from)?;
        let lines = match self {
            Self::Transcript(_) => lines,
            Self::AgySteps(_) => encode_lines(&brain::step_lines(&lines)),
            Self::AgyBoard(_) => encode_lines(
                &lines
                    .iter()
                    .filter_map(|written| brain::post_line(written))
                    .collect::<Vec<_>>(),
            ),
        };
        Ok((lines, offset))
    }
}

/// Serializes translated transcript lines; a line that can't be written is left out.
fn encode_lines(lines: &[TranscriptLine]) -> Vec<String> {
    lines
        .iter()
        .filter_map(|transcribed| {
            serde_json::to_string(transcribed)
                .inspect_err(|error| tracing::warn!(%error, "a transcript line not written"))
                .ok()
        })
        .collect()
}

/// Returns where a transcript is. An agy session's subagents and board are in agy's records,
/// by the conversation the task names; an agy session started elsewhere is read from its steps.
pub fn resolve(home: &ClaudeHome, place: &Place) -> Source {
    match place {
        Place::Path(path) => {
            if brain::is_steps_file(path) {
                Source::AgySteps(path.clone())
            } else if brain::is_board(path) {
                Source::AgyBoard(path.clone())
            } else {
                Source::Transcript(path.clone())
            }
        }
        Place::Session { cwd, id } => {
            if let Some(found) = sessions::find_transcript(home, cwd, id) {
                Source::Transcript(found)
            } else if let Some(brain_log) = brain::steps_file(id) {
                Source::AgySteps(brain_log)
            } else {
                Source::Transcript(sessions::transcript_path(home, cwd, id))
            }
        }
        Place::Subagent { cwd, id, task } => match (AgyTask::of(task), agy::brain()) {
            (Some(AgyTask::Conversation(conversation)), Some(brain)) => {
                Source::AgySteps(agy::steps(&brain, &conversation))
            }
            (Some(AgyTask::Board(conversation)), Some(brain)) => {
                Source::AgyBoard(agy::generated(&brain, &conversation).join("swarm/board.jsonl"))
            }
            (Some(_), None) | (None, _) => Source::Transcript(sessions::transcript_of(
                &sessions::written_transcript(home, cwd, id),
                task,
            )),
        },
    }
}

/// Reads what a window asked for, off the runtime's thread, and tells it.
pub async fn answer(
    home: ClaudeHome,
    request: ActId,
    read: Read,
    outbox: Outbox,
    catalog: Catalog,
) {
    if let Read::ProviderUsage { backend, cwd } = read {
        let usage = crate::usage::query(backend, cwd).await;
        if outbox
            .send(FromDaemon::Answer {
                request,
                answer: Answer::ProviderUsage(usage),
            })
            .await
            .is_err()
        {
            tracing::debug!("a window gone before its provider usage answer");
        }
        return;
    }
    let external = match &read {
        Read::Live => query_catalog(&catalog, true).await,
        Read::Projects | Read::Overview | Read::Sessions { .. } | Read::Saved(_) => {
            query_catalog(&catalog, false).await
        }
        Read::Transcript { .. }
        | Read::WorktreeOf { .. }
        | Read::LinkedWorktree { .. }
        | Read::TypedFolder(_)
        | Read::Folder(_)
        | Read::Changes { .. }
        | Read::Diff { .. }
        | Read::ProviderUsage { .. } => Vec::new(),
    };
    let pieces = outbox.clone();
    let answered =
        tokio::task::spawn_blocking(move || read_for(&home, request, read, &pieces, &external))
            .await;
    let answer = match answered {
        Ok(answer) => answer,
        Err(error) => {
            tracing::error!(%error, "reading for a window failed");
            return;
        }
    };
    if outbox
        .send(FromDaemon::Answer { request, answer })
        .await
        .is_err()
    {
        tracing::debug!("a window gone before its answer");
    }
}

/// The most sessions read per project for the start page.
const OVERVIEW_SESSIONS: usize = 5;

/// Returns what `read` comes to. A transcript's lines go to `pieces` first, as they are read.
fn read_for(
    home: &ClaudeHome,
    request: ActId,
    read: Read,
    pieces: &Outbox,
    external: &[ExternalSession],
) -> Answer {
    match read {
        Read::Live => Answer::Live(merge_live(sessions::live(home), external)),
        Read::Projects => Answer::Projects(merge_projects(sessions::projects(home), external)),
        Read::Overview => {
            let overviews: Vec<ProjectOverview> =
                merge_projects(sessions::projects(home), external)
                    .into_iter()
                    .map(|project| {
                        let local =
                            sessions::list(home, &project.path, OVERVIEW_SESSIONS, Vec::new());
                        let sessions =
                            merge_sessions(local, external, &project.path, OVERVIEW_SESSIONS);
                        ProjectOverview {
                            path: project.path,
                            modified: project.modified,
                            sessions,
                        }
                    })
                    .collect();
            Answer::Overview(chunk_overview(request, overviews, pieces))
        }
        Read::Sessions { cwd, limit, known } => {
            let limit = limit.min(MOST_SESSIONS);
            let local = sessions::list(home, &cwd, limit, known);
            Answer::Sessions(merge_sessions(local, external, &cwd, limit))
        }
        Read::Transcript { place, from } => {
            let source = resolve(home, &place);
            // Only a transcript is read, by no link (`sessions::open_transcript`): a place a
            // window names could otherwise lead the daemon to any file of the user's.
            let read = match source.read_from(from) {
                Ok((lines, offset)) => {
                    send_lines(request, &lines, pieces);
                    Ok(offset)
                }
                Err(error) => {
                    // A window shows nothing for a session whose transcript isn't there: where
                    // it was looked for says why.
                    tracing::info!(%error, path = %source.path().display(), "a transcript a window asked for wasn't read");
                    Err(ipc::Unread::from(error))
                }
            };
            Answer::Transcript {
                path: source.path().to_owned(),
                read,
            }
        }
        Read::WorktreeOf { cwd, id } => Answer::Worktree(
            sessions::find_transcript(home, &cwd, &id)
                .and_then(|transcript| sessions::worktree_of(&transcript)),
        ),
        Read::LinkedWorktree { cwd } => Answer::Worktree(sessions::linked_worktree(&cwd)),
        Read::TypedFolder(typed) => Answer::Folder(
            Folder::typed(&typed, dirs::home_dir().as_deref())
                .map(Folder::into_path)
                .map_err(|problem| problem.to_string()),
        ),
        Read::Folder(path) => Answer::Folder(
            Folder::at(path)
                .map(Folder::into_path)
                .map_err(|problem| problem.to_string()),
        ),
        Read::Changes { session, scope } => Answer::Changes(
            kept_changes(&session)
                .and_then(|store| store.changes(scope).map_err(|error| unkept(&error))),
        ),
        Read::Diff {
            session,
            scope,
            root,
            path,
        } => Answer::Diff(kept_changes(&session).and_then(|store| {
            store
                .hunks(scope, &root, &path)
                .map(crate::changes::Diff::fitted)
                .map_err(|error| unkept(&error))
        })),
        Read::ProviderUsage { .. } => {
            unreachable!("ProviderUsage is handled asynchronously in files::answer")
        }
        Read::Saved(id) => {
            let saved = sessions::saved(home, &id);
            let saved = if saved == sessions::Saved::Nowhere {
                external
                    .iter()
                    .find(|session| session.id == id && session.cwd.is_dir())
                    .map_or(saved, |session| {
                        sessions::Saved::StartedIn(session.cwd.clone())
                    })
            } else {
                saved
            };
            Answer::Saved(saved)
        }
    }
}

fn merge_live(mut live: Vec<LiveSession>, external: &[ExternalSession]) -> Vec<LiveSession> {
    let now = SystemTime::now();
    for ext in external {
        if ext.backend != Backend::Agy
            || !ext.cwd.is_dir()
            || live.iter().any(|session| session.id == ext.id)
        {
            continue;
        }
        if brain::is_running(&ext.id, now) {
            live.push(LiveSession {
                pid: 0,
                started: String::new(),
                id: ext.id.clone(),
                cwd: ext.cwd.clone(),
                name: MessagingName::of(&ext.id, &ext.cwd).to_string(),
                title: Some(ext.title.clone()),
                kind: LiveKind::Interactive,
                status: LiveStatus::Busy,
                worktree: sessions::linked_worktree(&ext.cwd),
                agents: 0,
                backend: Backend::Agy,
            });
        }
    }
    live.sort_by_key(|session| {
        let urgency = match session.status {
            LiveStatus::Waiting => 0,
            LiveStatus::Busy => 1,
            LiveStatus::Idle => 2,
            LiveStatus::Other => 3,
        };
        (urgency, session.name.clone())
    });
    live
}

fn merge_projects(local: Vec<ProjectInfo>, external: &[ExternalSession]) -> Vec<ProjectInfo> {
    // A time is newer than none, so `max` keeps the newest time there is.
    let mut by_path: HashMap<PathBuf, Option<SystemTime>> = local
        .into_iter()
        .map(|project| (project.path, project.modified))
        .collect();
    for ext in external {
        if !ext.cwd.is_dir() {
            continue;
        }
        let modified = if ext.backend == Backend::Agy {
            brain::steps_file(&ext.id)
                .and_then(|path| path.metadata().ok()?.modified().ok())
                .max(ext.modified)
        } else {
            ext.modified
        };
        by_path
            .entry(ext.cwd.clone())
            .and_modify(|existing| *existing = (*existing).max(modified))
            .or_insert(modified);
    }
    let mut projects: Vec<ProjectInfo> = by_path
        .into_iter()
        .map(|(path, modified)| ProjectInfo { path, modified })
        .collect();
    projects.sort_by_key(|project| std::cmp::Reverse(project.modified));
    projects
}

fn merge_sessions(
    local: Vec<SessionInfo>,
    external: &[ExternalSession],
    cwd: &Path,
    limit: usize,
) -> Vec<SessionInfo> {
    let mut seen_ids: HashSet<SessionId> = local.iter().map(|session| session.id.clone()).collect();
    let mut merged = local;
    for ext in external {
        if !sessions::belongs_to_project(&ext.cwd, cwd) || seen_ids.contains(&ext.id) {
            continue;
        }
        seen_ids.insert(ext.id.clone());
        let (brain_mod, last_said) = if ext.backend == Backend::Agy {
            match brain::last_said(&ext.id) {
                Some((brain_mod, said)) => (Some(brain_mod), Some(said)),
                None => (
                    brain::steps_file(&ext.id)
                        .and_then(|path| path.metadata().ok()?.modified().ok()),
                    None,
                ),
            }
        } else {
            (None, None)
        };
        let modified = brain_mod.max(ext.modified);
        merged.push(SessionInfo {
            backend: ext.backend,
            id: ext.id.clone(),
            title: ext.title.clone(),
            modified,
            worktree: sessions::linked_worktree(&ext.cwd),
            last_said,
        });
    }
    merged.sort_by_key(|session| std::cmp::Reverse(session.modified));
    merged.truncate(limit);
    merged
}

/// Opens a session's changes as the keeper wrote them.
fn kept_changes(session: &crate::ids::SessionId) -> Result<snapshots::Store, ipc::Unkept> {
    let home =
        snapshots::home().ok_or_else(|| ipc::Unkept("there is no state folder".to_owned()))?;
    snapshots::Store::open(&home, session).map_err(|error| unkept(&error))
}

/// Returns a store's error in words, with what caused it, for the window.
fn unkept(error: &snapshots::Error) -> ipc::Unkept {
    let mut words = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        words.push_str(": ");
        words.push_str(&cause.to_string());
        source = cause.source();
    }
    ipc::Unkept(words)
}

/// Sends a transcript's lines to the window in pieces that fit messages, stopping if the
/// window is gone.
fn send_lines(request: ActId, lines: &[String], pieces: &mpsc::Sender<FromDaemon>) {
    let mut text = String::with_capacity(lines.iter().map(|line| line.len() + 1).sum());
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    for piece in ipc::chunks(&text).filter(|piece| !piece.is_empty()) {
        let lines = FromDaemon::Lines {
            request,
            text: piece.to_owned(),
        };
        if pieces.blocking_send(lines).is_err() {
            tracing::debug!("a window gone as its transcript was read");
            return;
        }
    }
}

/// Sends a large overview as `FromDaemon::OverviewChunk`s that each fit `ipc::MAX_MESSAGE`,
/// and returns the last projects, for `Answer::Overview`.
fn chunk_overview(
    request: ActId,
    overviews: Vec<ProjectOverview>,
    pieces: &Outbox,
) -> Vec<ProjectOverview> {
    let mut batch: Vec<ProjectOverview> = Vec::new();
    for project in overviews {
        batch.push(project);
        let fits = ipc::frame(&FromDaemon::OverviewChunk {
            request,
            projects: batch.clone(),
        })
        .is_ok();
        if !fits && batch.len() > 1 {
            let overflowed = batch.pop();
            let chunk = FromDaemon::OverviewChunk {
                request,
                projects: std::mem::take(&mut batch),
            };
            if pieces.blocking_send(chunk).is_err() {
                tracing::debug!("a window gone as its overview was read");
                return Vec::new();
            }
            if let Some(overflowed) = overflowed {
                batch.push(overflowed);
            }
        }
    }
    batch
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn external_sessions_merge_dedup_and_sort_by_modified() {
        let dir = std::env::temp_dir().join(format!("edualc-merge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch dir");
        let now = SystemTime::now();
        let (now, earlier) = (Some(now), Some(now - Duration::from_secs(60)));
        let local_id = SessionId::new_random();
        let new_ext_id = SessionId::for_external(Backend::Opencode, "ses_new1");
        let other_dir = dir.join("other");
        std::fs::create_dir_all(&other_dir).expect("other dir");

        let local = vec![SessionInfo {
            backend: Backend::Agy,
            id: local_id.clone(),
            title: "Local agy".to_owned(),
            modified: earlier,
            worktree: None,
            last_said: None,
        }];
        let external = vec![
            // The same session as the local one, listed by agy too: listed once.
            ExternalSession {
                backend: Backend::Agy,
                raw_id: local_id.as_str().to_owned(),
                id: local_id.clone(),
                cwd: dir.clone(),
                title: "Duplicate".to_owned(),
                modified: now,
            },
            ExternalSession {
                backend: Backend::Opencode,
                raw_id: "ses_new1".to_owned(),
                id: new_ext_id.clone(),
                cwd: dir.clone(),
                title: "External opencode".to_owned(),
                modified: now,
            },
            ExternalSession {
                backend: Backend::Opencode,
                raw_id: "ses_other".to_owned(),
                id: SessionId::for_external(Backend::Opencode, "ses_other"),
                cwd: other_dir,
                title: "Other project".to_owned(),
                modified: now,
            },
        ];
        let merged = merge_sessions(local, &external, &dir, 10);
        let ids: Vec<(SessionId, Backend)> = merged
            .iter()
            .map(|session| (session.id.clone(), session.backend))
            .collect();
        assert_eq!(
            ids,
            [(new_ext_id, Backend::Opencode), (local_id, Backend::Agy),]
        );
        std::fs::remove_dir_all(&dir).expect("scratch cleaned");
    }

    #[test]
    fn a_session_listed_without_a_time_comes_last_and_undated() {
        let dir = std::env::temp_dir();
        let dated = SessionInfo {
            backend: Backend::Claude,
            id: SessionId::new_random(),
            title: "Dated".to_owned(),
            modified: Some(SystemTime::UNIX_EPOCH),
            worktree: None,
            last_said: None,
        };
        let undated = ExternalSession {
            backend: Backend::Opencode,
            raw_id: "ses_undated".to_owned(),
            id: SessionId::for_external(Backend::Opencode, "ses_undated"),
            cwd: dir.clone(),
            title: "Undated".to_owned(),
            modified: None,
        };
        let merged = merge_sessions(
            vec![dated.clone()],
            std::slice::from_ref(&undated),
            &dir,
            10,
        );
        let listed: Vec<_> = merged
            .iter()
            .map(|session| (&session.id, session.modified))
            .collect();
        assert_eq!(listed, [(&dated.id, dated.modified), (&undated.id, None)]);
        let projects = merge_projects(Vec::new(), &[undated]);
        assert_eq!(
            projects
                .iter()
                .map(|project| project.modified)
                .collect::<Vec<_>>(),
            [None]
        );
    }

    #[test]
    fn resolve_maps_agy_brain_logs_to_agy_steps() {
        if let Some(brain) = agy::brain() {
            let log = brain
                .join("8287c7b4-6f01-4d71-95bf-8379dbf2fefa")
                .join(".system_generated/logs/transcript.jsonl");
            assert!(matches!(
                resolve(&ClaudeHome::moved_to(PathBuf::from("/nowhere")), &Place::Path(log.clone())),
                Source::AgySteps(path) if path == log
            ));
        }
    }

    #[test]
    fn a_transcripts_lines_come_in_pieces_that_fit_messages() {
        let lines = vec![
            "short".to_owned(),
            "é".repeat(3 * ipc::MAX_CHUNK),
            "also short".to_owned(),
        ];
        let (pieces, mut received) = mpsc::channel(64);
        send_lines(ActId::default(), &lines, &pieces);
        drop(pieces);
        let mut text = String::new();
        while let Ok(FromDaemon::Lines { text: piece, .. }) = received.try_recv() {
            let sent = crate::ipc::frame(&FromDaemon::Lines {
                request: ActId::default(),
                text: piece.clone(),
            });
            assert!(sent.is_ok(), "a piece of {} bytes didn't fit", piece.len());
            text.push_str(&piece);
        }
        let mut splitter = ipc::LineSplitter::default();
        assert_eq!(splitter.push(text.as_bytes()), lines);
    }

    #[test]
    fn a_thousand_sessions_fit_a_message() {
        let session = |at: usize| sessions::SessionInfo {
            backend: crate::provider::Backend::Claude,
            id: crate::ids::SessionId::from(uuid::Uuid::from_u128(at as u128)),
            title: "x".repeat(300),
            modified: Some(std::time::SystemTime::now()),
            worktree: None,
            last_said: None,
        };
        let answer = FromDaemon::Answer {
            request: ActId::default(),
            answer: Answer::Sessions((0..MOST_SESSIONS).map(session).collect()),
        };
        assert!(crate::ipc::frame(&answer).is_ok());
    }

    #[test]
    fn large_overviews_are_chunked_so_each_message_fits() {
        let now = Some(std::time::SystemTime::now());
        let make_project = |idx: usize| ProjectOverview {
            path: PathBuf::from(format!("/code/project-{idx}-{}", "x".repeat(400))),
            modified: now,
            sessions: (0..OVERVIEW_SESSIONS)
                .map(|sub| sessions::SessionInfo {
                    backend: crate::provider::Backend::Claude,
                    id: crate::ids::SessionId::from(uuid::Uuid::from_u128(
                        u128::try_from(idx * 10 + sub).unwrap_or(0),
                    )),
                    title: "title ".repeat(200),
                    modified: now,
                    worktree: None,
                    last_said: Some("sentence ".repeat(200)),
                })
                .collect(),
        };
        let total = 150;
        let all: Vec<ProjectOverview> = (0..total).map(make_project).collect();
        let (pieces, mut received) = mpsc::channel(64);
        let tail = chunk_overview(ActId::default(), all, &pieces);
        drop(pieces);
        let mut collected = Vec::new();
        while let Ok(FromDaemon::OverviewChunk { projects, .. }) = received.try_recv() {
            let framed = FromDaemon::OverviewChunk {
                request: ActId::default(),
                projects: projects.clone(),
            };
            assert!(ipc::frame(&framed).is_ok());
            collected.extend(projects);
        }
        assert!(!collected.is_empty(), "expected chunking across messages");
        let answer = FromDaemon::Answer {
            request: ActId::default(),
            answer: Answer::Overview(tail.clone()),
        };
        assert!(ipc::frame(&answer).is_ok());
        collected.extend(tail);
        assert_eq!(collected.len(), total);
    }
}
