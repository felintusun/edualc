//! A window's connection: its hello, its requests to the hub and the daemon's actors, and what
//! is said back to it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::ids::SessionId;
use crate::ipc::{
    self, Act, ActId, Deframer, FromDaemon, Generation, HostedId, StartFailure, ToDaemon,
};
use crate::provider::Launch;
use crate::sessions::ClaudeHome;
use crate::{files, sessions, settings, snapshots};

use super::acts::act_for;
use super::hub::ToHub;

/// What every window's connection reaches: the daemon's actors, and Claude Code's folder.
#[derive(Debug, Clone)]
pub(super) struct Shared {
    pub(super) hub: mpsc::Sender<ToHub>,
    pub(super) keeper: mpsc::Sender<snapshots::Job>,
    pub(super) settings_keeper: mpsc::Sender<files::SettingsChange>,
    pub(super) catalog: files::Catalog,
    pub(super) claude: ClaudeHome,
}
use super::{HELLO, Line, QUEUE, keep, tell};

/// Serves a window's connection by the socket, pumping its two directions into a [`Line`].
pub(super) async fn window(channel: ipc::AsyncChannel, id: WindowId, shared: Shared) {
    let (mut reading, writing) = channel.split();
    let (line, requests, answers) = Line::pair();
    let mut deframer = Deframer::default();
    // The socket's two directions: pumps, not actors.
    let reader = tokio::spawn(async move {
        loop {
            match ipc::recv::<ToDaemon>(&mut reading, &mut deframer).await {
                Ok(request) => {
                    if requests.send(request).await.is_err() {
                        return;
                    }
                }
                Err(error) => {
                    tracing::info!(%error, "a window hung up");
                    return;
                }
            }
        }
    });
    let writer = tokio::spawn(write(writing, answers));
    window_over(line, id, shared).await;
    reader.abort();
    if let Err(error) = writer.await {
        tracing::debug!(%error, "the writer ended");
    }
}

/// Serves a window over whatever carries its line: the hello, then its requests to its
/// [`Connection`] and what the hub, the sessions and the daemon's acts answer, until it hangs
/// up.
pub(super) async fn window_over(line: Line, id: WindowId, shared: Shared) {
    let Line {
        mut requests,
        outbox,
    } = line;
    match tokio::time::timeout(HELLO, requests.recv()).await {
        Ok(Some(ToDaemon::Hello { protocol })) => {
            let claude = shared.claude.clone();
            let (settings, live, backends) = tokio::task::spawn_blocking(move || {
                (
                    settings::load().map_err(|error| error.to_string()),
                    sessions::live(&claude),
                    crate::usage::available_backends(),
                )
            })
            .await
            .unwrap_or_else(|error| (Err(error.to_string()), Vec::new(), Vec::new()));
            let answer = FromDaemon::Hello {
                protocol: ipc::PROTOCOL,
                settings,
                live,
                home: dirs::home_dir(),
                backends,
            };
            if outbox.send(answer).await.is_err() {
                tracing::warn!("a window left as it came");
                return;
            }
            if protocol != ipc::PROTOCOL {
                tracing::warn!(protocol, "a window of another version");
                return;
            }
        }
        Ok(Some(_) | None) | Err(_) => {
            tracing::warn!("a connection that didn't say hello");
            return;
        }
    }
    let connection = Connection::new(id)
        .event_loop(requests, outbox, shared)
        .await;
    tracing::info!(window = ?connection.id, "a window's connection ended");
}

/// Writes what is queued for a window, until everything that could queue more is gone.
pub(super) async fn write(mut writing: ipc::Writing, mut answers: mpsc::Receiver<FromDaemon>) {
    while let Some(answer) = answers.recv().await {
        if let Err(error) = ipc::send(&mut writing, &answer).await {
            tracing::info!(%error, "a window stopped listening");
            return;
        }
    }
}

/// What a window's requests come to.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Effect {
    /// For the hub.
    Hub(Ask),
    /// For the daemon's acts, answered to the window.
    Act { request: ActId, act: Act },
    /// What was asked can't be done; the window is told.
    Refuse {
        key: Generation,
        failure: StartFailure,
    },
    /// For the files, answered to the window.
    Read { request: ActId, read: ipc::Read },
    /// For the settings' keeper.
    ChangeSettings(crate::settings::Change),
    /// For the window's watches.
    Watch(files::Order),
    /// For the changes' keeper.
    Looked { session: SessionId },
    Reviewed {
        session: SessionId,
        path: PathBuf,
        mark: snapshots::Mark,
    },
}

/// What a window asks of the hub, by its keys.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Ask {
    Start {
        key: Generation,
        launch: Launch,
    },
    Line {
        key: Generation,
        line: String,
    },
    End {
        key: Generation,
    },
    Attach {
        key: Generation,
        hosted: HostedId,
    },
    Detach {
        key: Generation,
    },
    Identify {
        key: Generation,
        session: SessionId,
    },
    /// Every session ends, and then the daemon.
    ShutDown,
}

/// The daemon's id for a window's connection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct WindowId(pub(super) u64);

impl WindowId {
    pub(super) fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// A window: the keys it shows sessions under, and the line it is sending each in pieces.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Connection {
    pub(super) id: WindowId,
    pub(super) keys: BTreeMap<Generation, String>,
}

impl Connection {
    pub(super) fn new(id: WindowId) -> Self {
        Self {
            id,
            keys: BTreeMap::new(),
        }
    }

    pub(super) async fn event_loop(
        mut self,
        mut requests: mpsc::Receiver<ToDaemon>,
        outbox: mpsc::Sender<FromDaemon>,
        shared: Shared,
    ) -> Self {
        let Shared {
            hub,
            keeper,
            settings_keeper,
            catalog,
            claude,
        } = shared;
        let window = self.id;
        if hub
            .send(ToHub::Joined {
                window,
                outbox: outbox.clone(),
            })
            .await
            .is_err()
        {
            return self;
        }
        let (watching, watch_orders) = mpsc::channel(QUEUE);
        tokio::spawn(files::watch(claude.clone(), watch_orders, outbox.clone()));
        while let Some(request) = requests.recv().await {
            for effect in self.handle(request) {
                match effect {
                    Effect::Read { request, read } => {
                        tokio::spawn(files::answer(
                            claude.clone(),
                            request,
                            read,
                            outbox.clone(),
                            catalog.clone(),
                        ));
                    }
                    Effect::ChangeSettings(change) => {
                        let change = files::SettingsChange {
                            change,
                            outbox: outbox.clone(),
                        };
                        if settings_keeper.send(change).await.is_err() {
                            tell(
                                &outbox,
                                FromDaemon::SettingsNotKept(
                                    "the daemon's keeper of the settings has stopped".to_owned(),
                                ),
                            )
                            .await;
                        }
                    }
                    Effect::Watch(order) => {
                        if watching.send(order).await.is_err() {
                            tracing::debug!("a window's watches ended");
                        }
                    }
                    Effect::Hub(ask) => {
                        let outbox = outbox.clone();
                        if hub
                            .send(ToHub::Asked {
                                window,
                                ask,
                                outbox,
                            })
                            .await
                            .is_err()
                        {
                            return self;
                        }
                    }
                    Effect::Act { request, act } => {
                        let outbox = outbox.clone();
                        tokio::spawn(act_for(claude.clone(), request, act, outbox));
                    }
                    Effect::Refuse { key, failure } => {
                        tell(&outbox, FromDaemon::Failed { key, failure }).await;
                    }
                    Effect::Looked { session } => {
                        keep(&keeper, snapshots::Job::Looked { session }).await;
                    }
                    Effect::Reviewed {
                        session,
                        path,
                        mark,
                    } => {
                        keep(
                            &keeper,
                            snapshots::Job::Reviewed {
                                session,
                                path,
                                mark,
                            },
                        )
                        .await;
                    }
                }
            }
        }
        // The window hung up: its sessions run on, shown nowhere.
        if hub.send(ToHub::Left { window }).await.is_err() {
            tracing::debug!("the hub was gone as a window left");
        }
        self
    }

    /// Returns what a request comes to.
    pub(super) fn handle(&mut self, request: ToDaemon) -> Vec<Effect> {
        match request {
            ToDaemon::Start { key, .. } | ToDaemon::Attach { key, .. }
                if self.keys.contains_key(&key) =>
            {
                vec![Effect::Refuse {
                    key,
                    failure: StartFailure::KeyInUse,
                }]
            }
            ToDaemon::Start { key, launch } => {
                self.keys.insert(key, String::new());
                vec![Effect::Hub(Ask::Start { key, launch })]
            }
            ToDaemon::Attach { key, hosted } => {
                self.keys.insert(key, String::new());
                vec![Effect::Hub(Ask::Attach { key, hosted })]
            }
            ToDaemon::Command { key, text, last } => {
                let Some(line) = self.keys.get_mut(&key) else {
                    return Vec::new();
                };
                if line.len() + text.len() > ipc::MAX_LINE {
                    line.clear();
                    return vec![Effect::Refuse {
                        key,
                        failure: StartFailure::LineTooLong,
                    }];
                }
                line.push_str(&text);
                if last {
                    let line = std::mem::take(line);
                    vec![Effect::Hub(Ask::Line { key, line })]
                } else {
                    Vec::new()
                }
            }
            ToDaemon::End { key } => match self.keys.remove(&key) {
                Some(_) => vec![Effect::Hub(Ask::End { key })],
                None => Vec::new(),
            },
            ToDaemon::Detach { key } => match self.keys.remove(&key) {
                Some(_) => vec![Effect::Hub(Ask::Detach { key })],
                None => Vec::new(),
            },
            ToDaemon::Identify { key, session } if self.keys.contains_key(&key) => {
                vec![Effect::Hub(Ask::Identify { key, session })]
            }
            // A second hello, or a key not in use.
            ToDaemon::Hello { .. } | ToDaemon::Identify { .. } => Vec::new(),
            ToDaemon::Act {
                act: Act::ChangesLooked { session },
                ..
            } => vec![Effect::Looked { session }],
            ToDaemon::Act {
                act: Act::MarkReviewed { session, path, now },
                ..
            } => vec![Effect::Reviewed {
                session,
                path,
                mark: snapshots::Mark::On(now),
            }],
            ToDaemon::Act {
                act: Act::UnmarkReviewed { session, path },
                ..
            } => vec![Effect::Reviewed {
                session,
                path,
                mark: snapshots::Mark::Off,
            }],
            ToDaemon::Act { request, act } => vec![Effect::Act { request, act }],
            ToDaemon::ShutDown => vec![Effect::Hub(Ask::ShutDown)],
            ToDaemon::Read { request, read } => vec![Effect::Read { request, read }],
            ToDaemon::ChangeSettings(change) => vec![Effect::ChangeSettings(change)],
            ToDaemon::Watch { watched, target } => {
                vec![Effect::Watch(files::Order::Watch(watched, target))]
            }
            ToDaemon::Unwatch(watched) => vec![Effect::Watch(files::Order::Unwatch(watched))],
        }
    }
}
