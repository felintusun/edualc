//! The demo's daemon: answers a window as the daemon does, from the script and the demo project.

use std::collections::HashMap;
use std::path::Path;

use crate::daemon::Line;
use crate::ipc::{
    Act, ActFailure, ActId, Answer, FromDaemon, Generation, PROTOCOL, Place, Read, StartFailure,
    ToDaemon,
};
use crate::settings::Settings;
use edualc_core::demo::script::{Session, Sink};
use edualc_core::demo::world as demo;
use tokio::sync::mpsc::Sender;

/// Answers a window until it hangs up.
pub async fn run(line: Line) {
    let Line {
        mut requests,
        outbox,
    } = line;
    let mut sessions: HashMap<Generation, Session> = HashMap::new();
    while let Some(request) = requests.recv().await {
        match request {
            ToDaemon::Hello { .. } => greet(&outbox),
            ToDaemon::Start { key, launch } => {
                let session = Session::start(key, Sink::Line(outbox.clone()), &launch);
                sessions.insert(key, session);
            }
            ToDaemon::Command { key, text, last } => {
                if let Some(session) = sessions.get_mut(&key)
                    && let Some(task) = session.take(&text, last)
                {
                    tokio::spawn(task.run());
                }
            }
            ToDaemon::End { key } => {
                if sessions.remove(&key).is_some() {
                    Sink::Line(outbox.clone()).ended(key);
                }
            }
            ToDaemon::Attach { key, .. } => tell(
                &outbox,
                FromDaemon::Failed {
                    key,
                    failure: StartFailure::NotHosted,
                },
            ),
            ToDaemon::Act { request, act } => acted(&outbox, request, act),
            ToDaemon::Read { request, read } => read_for(&outbox, request, read),
            // Nothing is kept, and nothing changes on its own; the demo ends with edualcd.
            ToDaemon::ShutDown
            | ToDaemon::ChangeSettings(_)
            | ToDaemon::Watch { .. }
            | ToDaemon::Unwatch(_)
            | ToDaemon::Detach { .. }
            | ToDaemon::Identify { .. } => {}
        }
    }
}

/// Sends the hello and the sessions the demo runs: none, but four "run in other terminals".
fn greet(outbox: &Sender<FromDaemon>) {
    let settings = Settings {
        trusted_folders: vec![demo::Folder::Trailhead.path().to_path_buf()],
        ..Settings::default()
    };
    tell(
        outbox,
        FromDaemon::Hello {
            protocol: PROTOCOL,
            settings: Ok(settings),
            live: demo::running(),
            home: Some(demo::Folder::Trailhead.path().to_path_buf()),
            backends: vec![crate::provider::Backend::Claude],
        },
    );
    tell(outbox, FromDaemon::Hosted(Vec::new()));
}

/// Answers what a window reads from the demo project.
fn read_for(outbox: &Sender<FromDaemon>, request: ActId, read: Read) {
    let answer = match read {
        Read::Live => Answer::Live(demo::running()),
        Read::Projects => Answer::Projects(demo::projects()),
        Read::Overview => Answer::Overview(demo::overview()),
        Read::Sessions { cwd, limit, .. } => {
            let mut sessions = demo::saved(&cwd);
            sessions.truncate(limit);
            Answer::Sessions(sessions)
        }
        Read::Transcript { place, from } => {
            let path = match place {
                Place::Path(path) => path,
                Place::Session { cwd, id } => cwd.join(format!("{}.jsonl", id.as_str())),
                // Beside its session's, as Claude Code keeps it: not its session's own.
                Place::Subagent { cwd, id, task, .. } => crate::sessions::transcript_of(
                    &cwd.join(format!("{}.jsonl", id.as_str())),
                    &task,
                ),
            };
            let read = match demo::transcript(&path) {
                Some(text) => {
                    let rest = usize::try_from(from)
                        .ok()
                        .and_then(|at| text.get(at..))
                        .unwrap_or_default();
                    for piece in crate::ipc::chunks(rest).filter(|piece| !piece.is_empty()) {
                        tell(
                            outbox,
                            FromDaemon::Lines {
                                request,
                                text: piece.to_owned(),
                            },
                        );
                    }
                    Ok(u64::try_from(text.len()).unwrap_or(u64::MAX))
                }
                None => Err(crate::ipc::Unread::NotWritten),
            };
            Answer::Transcript { path, read }
        }
        Read::WorktreeOf { .. } | Read::LinkedWorktree { .. } => Answer::Worktree(None),
        Read::TypedFolder(typed) => Answer::Folder(demo_folder(Path::new(typed.trim()))),
        Read::Folder(path) => Answer::Folder(demo_folder(&path)),
        // The demo's sessions change no folder.
        Read::Changes { .. } => Answer::Changes(Ok(crate::changes::ChangeSet::default())),
        Read::Diff { .. } => Answer::Diff(Ok(crate::changes::Diff::default())),
        // The demo keeps nothing between its runs: a session shown before isn't there.
        Read::Saved(_) => Answer::Saved(crate::sessions::Saved::Nowhere),
        Read::ProviderUsage { backend, .. } => Answer::ProviderUsage(match backend {
            crate::provider::Backend::Claude => {
                crate::ipc::ProviderUsage::noted("The browser demo shows no live usage.")
            }
            crate::provider::Backend::Agy | crate::provider::Backend::Opencode => {
                crate::ipc::ProviderUsage::Unavailable
            }
        }),
    };
    tell(outbox, FromDaemon::Answer { request, answer });
}

/// Returns one of the demo's folders, or why a path isn't one.
fn demo_folder(path: &Path) -> Result<std::path::PathBuf, String> {
    demo::projects()
        .into_iter()
        .map(|project| project.path)
        .find(|project| project == path)
        .ok_or_else(|| format!("{} isn't one of the demo's folders", path.display()))
}

fn tell(outbox: &Sender<FromDaemon>, message: FromDaemon) {
    if outbox.try_send(message).is_err() {
        tracing::debug!("the window is gone");
    }
}

/// Answers what the daemon is asked to do, as far as the demo can.
fn acted(outbox: &Sender<FromDaemon>, request: ActId, act: Act) {
    let message = match act {
        Act::CheckTrust { folder } => FromDaemon::Trusted {
            request,
            trust: match demo::untrusted(&folder) {
                Some(runs) => crate::ipc::FolderTrust::Untrusted(runs),
                None => crate::ipc::FolderTrust::Accepted,
            },
        },
        // The demo has no Claude Code to trust anything: it never asks.
        Act::CheckClaudeTrust { .. } => FromDaemon::ClaudeTrust {
            request,
            trusted: Ok(true),
        },
        Act::TrustForClaudeCode { .. } => FromDaemon::Done {
            request,
            result: Ok(()),
        },
        Act::CreateProject { .. } => FromDaemon::Created {
            request,
            result: Err(crate::ipc::CreateProjectError::InBrowser),
        },
        // Not answered.
        Act::ChangesLooked { .. } | Act::MarkReviewed { .. } | Act::UnmarkReviewed { .. } => {
            return;
        }
        // The demo has no keyring to keep a key in.
        Act::CheckApiKey => FromDaemon::ApiKeyStored {
            request,
            stored: Ok(false),
        },
        Act::StoreApiKey(_) | Act::RemoveApiKey => FromDaemon::Done {
            request,
            result: Err(ActFailure::Keyring("the demo keeps no API key".to_owned())),
        },
    };
    tell(outbox, message);
}
