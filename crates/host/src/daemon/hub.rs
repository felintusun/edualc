//! The hub: the sessions the daemon runs, and which window shows each.

use std::collections::BTreeMap;
use std::path::PathBuf;

use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;

use crate::external::wire::SessionId;
use crate::ipc::{FromDaemon, Generation, Hosted, HostedId, HostedState, StartFailure};
use crate::provider::{Launch, SignInWith};
use crate::snapshots;

use super::connection::{Ask, WindowId};
use super::session::{Command, Reporting, host};
use super::{QUEUE, tell};

/// What the hub is told.
#[derive(Debug)]
pub(super) enum ToHub {
    /// A window connected: it hears the list of sessions from now on.
    Joined {
        window: WindowId,
        outbox: mpsc::Sender<FromDaemon>,
    },
    Asked {
        window: WindowId,
        ask: Ask,
        outbox: mpsc::Sender<FromDaemon>,
    },
    /// A window hung up: its sessions run on, shown nowhere.
    Left { window: WindowId },
}

/// A session as the hub keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Kept {
    pub(super) cwd: PathBuf,
    pub(super) session: SessionId,
    pub(super) backend: crate::provider::Backend,
    /// What it was started signed in with: a window attaching to it is told.
    pub(super) sign_in: SignInWith,
    /// The window and key it is shown under.
    pub(super) shown: Option<(WindowId, Generation)>,
    pub(super) state: HostedState,
}

/// The sessions the daemon runs, and which window shows each.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Hub {
    pub(super) sessions: BTreeMap<HostedId, Kept>,
    pub(super) last: HostedId,
}

/// What the hub's own decisions come to.
#[derive(Debug, PartialEq)]
pub(super) enum Step {
    /// A new session, as `id`, shown under the window's key.
    Start { id: HostedId, launch: Launch },
    /// To a session.
    Tell { id: HostedId, command: Order },
    /// Something the window is told.
    Answer(FromDaemon),
    /// The list changed: every window hears it.
    Changed,
    /// Every window is told, and the hub ends, its sessions with it.
    ShutDown,
}

/// What the hub tells a session (`Attach` carries the outbox, added by the loop).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Order {
    Line(String),
    Attach { key: Generation },
    Detach,
    End,
}

impl Hub {
    pub(super) async fn event_loop(
        mut self,
        mut requests: mpsc::Receiver<ToHub>,
        running: watch::Sender<usize>,
        runtime: tokio::runtime::Handle,
        keeper: mpsc::Sender<snapshots::Job>,
    ) -> Self {
        let mut windows: BTreeMap<WindowId, mpsc::Sender<FromDaemon>> = BTreeMap::new();
        let mut orders: BTreeMap<HostedId, mpsc::Sender<Command>> = BTreeMap::new();
        let (states_tx, mut state_updates) = mpsc::unbounded_channel::<(HostedId, HostedState)>();
        let mut tasks = JoinSet::new();
        // Which session each task runs: one that panics gives back no `Session` to say.
        let mut hosting: BTreeMap<tokio::task::Id, HostedId> = BTreeMap::new();
        // A window asked to shut down: the loop ends, and below, every session with it.
        let mut shutting_down = false;
        loop {
            let steps = tokio::select! {
                request = requests.recv() => match request {
                    Some(ToHub::Joined { window, outbox }) => {
                        let list = FromDaemon::Hosted(self.list());
                        tell(&outbox, list).await;
                        windows.insert(window, outbox);
                        Vec::new()
                    }
                    Some(ToHub::Asked { window, ask, outbox }) => {
                        let steps = self.handle(window, ask);
                        let mut rest = Vec::new();
                        for step in steps {
                            match step {
                                Step::Start { id, launch } => {
                                    let (order, commands) = mpsc::channel(QUEUE);
                                    orders.insert(id, order);
                                    let key = self.key_of(id);
                                    let shown = key.map(|key| (key, outbox.clone()));
                                    let task = tasks.spawn(host(
                                        id,
                                        launch,
                                        runtime.clone(),
                                        commands,
                                        shown,
                                        Reporting {
                                            states: Some(states_tx.clone()),
                                            keeper: keeper.clone(),
                                        },
                                    ));
                                    hosting.insert(task.id(), id);
                                }
                                Step::Tell { id, command: Order::Attach { key } } => {
                                    send(&orders, id, Command::Attach { key, outbox: outbox.clone() }).await;
                                }
                                Step::Answer(answer) => tell(&outbox, answer).await,
                                other @ (Step::Tell { .. } | Step::Changed | Step::ShutDown) => rest.push(other),
                            }
                        }
                        rest
                    }
                    Some(ToHub::Left { window }) => {
                        windows.remove(&window);
                        self.left(window)
                    }
                    // The daemon ends: the sessions' orders close, and each ends.
                    None => break,
                },
                Some((id, state)) = state_updates.recv() => self.set_state(id, state),
                Some(ended) = tasks.join_next_with_id() => {
                    let task = match &ended {
                        Ok((task, _)) => *task,
                        Err(error) => error.id(),
                    };
                    match (hosting.remove(&task), ended) {
                        (Some(_) | None, Ok((_, session))) => {
                            orders.remove(&session.id);
                            self.ended(session.id)
                        }
                        // Its process is killed as the task drops it; it goes from the list, and
                        // the window showing it is told it ended, rather than waiting on it.
                        (Some(id), Err(error)) => {
                            tracing::error!(%error, ?id, "a session failed");
                            orders.remove(&id);
                            let shown = self.sessions.get(&id).and_then(|kept| kept.shown);
                            if let Some((window, key)) = shown
                                && let Some(outbox) = windows.get(&window)
                            {
                                tell(outbox, FromDaemon::Ended { key, code: None }).await;
                            }
                            self.ended(id)
                        }
                        (None, Err(error)) => {
                            tracing::error!(%error, "a session failed, which ran nothing known");
                            Vec::new()
                        }
                    }
                },
            };
            for step in steps {
                match step {
                    Step::Tell { id, command } => {
                        let command = match command {
                            Order::Line(line) => Command::Line(line),
                            Order::Detach => Command::Detach,
                            Order::End => {
                                // Its orders closing ends it.
                                orders.remove(&id);
                                continue;
                            }
                            Order::Attach { .. } => continue,
                        };
                        send(&orders, id, command).await;
                    }
                    Step::Changed => {
                        let list = self.list();
                        for outbox in windows.values() {
                            tell(outbox, FromDaemon::Hosted(list.clone())).await;
                        }
                    }
                    Step::ShutDown => {
                        tracing::info!(
                            sessions = self.sessions.len(),
                            "shutting down, as a window asked"
                        );
                        for outbox in windows.values() {
                            tell(outbox, FromDaemon::ShuttingDown).await;
                        }
                        shutting_down = true;
                    }
                    Step::Start { .. } | Step::Answer(_) => {}
                }
            }
            running.send_replace(self.sessions.len());
            if shutting_down {
                break;
            }
        }
        orders.clear();
        while tasks.join_next().await.is_some() {}
        self
    }

    pub(super) fn key_of(&self, id: HostedId) -> Option<Generation> {
        self.sessions
            .get(&id)
            .and_then(|kept| kept.shown)
            .map(|(_, key)| key)
    }

    /// The session a window shows under `key`.
    pub(super) fn shown_as(&self, window: WindowId, key: Generation) -> Option<HostedId> {
        self.sessions
            .iter()
            .find(|(_, kept)| kept.shown == Some((window, key)))
            .map(|(id, _)| *id)
    }

    /// What a window's ask comes to (sans-io).
    pub(super) fn handle(&mut self, window: WindowId, ask: Ask) -> Vec<Step> {
        match ask {
            Ask::Start { key, launch } => {
                self.last = self.last.next();
                let id = self.last;
                self.sessions.insert(
                    id,
                    Kept {
                        cwd: launch.cwd.clone(),
                        session: launch.conversation.id().clone(),
                        backend: launch.backend,
                        sign_in: launch.sign_in,
                        shown: Some((window, key)),
                        state: HostedState::Idle,
                    },
                );
                vec![Step::Start { id, launch }, Step::Changed]
            }
            Ask::Line { key, line } => match self.shown_as(window, key) {
                Some(id) => vec![Step::Tell {
                    id,
                    command: Order::Line(line),
                }],
                None => Vec::new(),
            },
            Ask::End { key } => match self.shown_as(window, key) {
                Some(id) => vec![Step::Tell {
                    id,
                    command: Order::End,
                }],
                None => Vec::new(),
            },
            Ask::Detach { key } => match self.shown_as(window, key) {
                Some(id) => {
                    if let Some(kept) = self.sessions.get_mut(&id) {
                        kept.shown = None;
                    }
                    vec![
                        Step::Tell {
                            id,
                            command: Order::Detach,
                        },
                        Step::Changed,
                    ]
                }
                None => Vec::new(),
            },
            Ask::Attach { key, hosted } => match self.sessions.get_mut(&hosted) {
                Some(kept) if kept.shown.is_none() => {
                    kept.shown = Some((window, key));
                    vec![
                        Step::Tell {
                            id: hosted,
                            command: Order::Attach { key },
                        },
                        Step::Changed,
                    ]
                }
                Some(_) | None => vec![Step::Answer(FromDaemon::Failed {
                    key,
                    failure: StartFailure::NotHosted,
                })],
            },
            Ask::ShutDown => vec![Step::ShutDown],
            Ask::Identify { key, session } => match self.shown_as(window, key) {
                Some(id) => {
                    if let Some(kept) = self.sessions.get_mut(&id) {
                        kept.session = session;
                    }
                    vec![Step::Changed]
                }
                None => Vec::new(),
            },
        }
    }

    /// A window hung up: what it showed is shown nowhere, and runs on.
    pub(super) fn left(&mut self, window: WindowId) -> Vec<Step> {
        let mut steps = Vec::new();
        for (id, kept) in &mut self.sessions {
            if kept.shown.is_some_and(|(shown_in, _)| shown_in == window) {
                kept.shown = None;
                steps.push(Step::Tell {
                    id: *id,
                    command: Order::Detach,
                });
            }
        }
        if !steps.is_empty() {
            steps.push(Step::Changed);
        }
        steps
    }

    pub(super) fn ended(&mut self, id: HostedId) -> Vec<Step> {
        if self.sessions.remove(&id).is_some() {
            vec![Step::Changed]
        } else {
            Vec::new()
        }
    }

    pub(super) fn set_state(&mut self, id: HostedId, state: HostedState) -> Vec<Step> {
        let Some(kept) = self.sessions.get_mut(&id) else {
            return Vec::new();
        };
        if kept.state == state {
            return Vec::new();
        }
        kept.state = state;
        vec![Step::Changed]
    }

    pub(super) fn list(&self) -> Vec<Hosted> {
        self.sessions
            .iter()
            .map(|(id, kept)| Hosted {
                id: *id,
                session: kept.session.clone(),
                cwd: kept.cwd.clone(),
                backend: kept.backend,
                sign_in: kept.sign_in,
                attached: kept.shown.is_some(),
                state: kept.state.clone(),
            })
            .collect()
    }
}

pub(super) async fn send(
    orders: &BTreeMap<HostedId, mpsc::Sender<Command>>,
    id: HostedId,
    command: Command,
) {
    if let Some(order) = orders.get(&id)
        && order.send(command).await.is_err()
    {
        tracing::debug!(?id, "an order for a session that ended");
    }
}
