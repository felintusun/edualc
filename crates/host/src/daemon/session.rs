//! One session's process, as the daemon runs it, and what it tells the hub and the changes'
//! keeper.

use std::collections::BTreeSet;
use std::path::PathBuf;

use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::agent;
use crate::driver::{Decoded, Driver};
use crate::ids::SessionId;
use crate::ipc::{FromDaemon, Generation, HostedId, HostedState, StartFailure, Stream};
use crate::provider::Launch;
use crate::{snapshots, spawn};

use super::pending::Pending;
use super::pipes::{Input, Unqueued, pump, send_line};
use super::{GRACE, QUEUE, QUIET, keep, tell};

/// A session's turn, as its lines tell it, for the changes' keeper (sans-io).
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) enum Doing {
    #[default]
    BetweenTurns,
    /// With the files the agent's editing tools named so far.
    Turn { touched: BTreeSet<PathBuf> },
}

impl Doing {
    /// Takes an event the process wrote, keeping the files it names while a turn is under way.
    pub(super) fn saw(&mut self, event: &agent::Event) {
        let Self::Turn { touched } = self else {
            return;
        };
        match event {
            agent::Event::Assistant { message, .. } => {
                touched.extend(snapshots::edited_in(&message.content));
            }
            agent::Event::ToolCall { call, .. } => touched.extend(snapshots::edited_by(call)),
            agent::Event::Stream { .. }
            | agent::Event::User(_)
            | agent::Event::System(_)
            | agent::Event::Result(_)
            | agent::Event::RateLimits(_)
            | agent::Event::ControlRequest { .. }
            | agent::Event::PermissionAsked { .. }
            | agent::Event::ControlAnswered(_)
            | agent::Event::RequestCancelled { .. } => {}
        }
    }

    /// Returns what to tell the keeper as the session becomes `busy` or not: that a turn
    /// starts, or that it ends, its changes in short to go to `changed`.
    pub(super) fn follow(
        &mut self,
        busy: bool,
        place: &Place,
        changed: &mpsc::Sender<crate::changes::Summary>,
    ) -> Option<snapshots::Job> {
        match (std::mem::take(self), busy) {
            (Self::BetweenTurns, true) => {
                *self = Self::Turn {
                    touched: BTreeSet::new(),
                };
                Some(snapshots::Job::TurnStarted {
                    session: place.session.clone(),
                    cwd: place.cwd.clone(),
                })
            }
            (Self::Turn { touched }, false) => Some(snapshots::Job::TurnEnded {
                session: place.session.clone(),
                touched,
                done: changed.clone(),
            }),
            (doing, _) => {
                *self = doing;
                None
            }
        }
    }
}

/// Whom a session tells about itself: the hub, what it does; the changes' keeper, its turns.
#[derive(Debug)]
pub(super) struct Reporting {
    pub(super) states: Option<mpsc::UnboundedSender<(HostedId, HostedState)>>,
    pub(super) keeper: mpsc::Sender<snapshots::Job>,
}

/// Where a session works, and its changes' name.
#[derive(Debug)]
pub(super) struct Place {
    pub(super) cwd: PathBuf,
    /// The id it started under: its changes are kept under it.
    pub(super) session: SessionId,
}

/// Starts a session's process and runs it, telling the window it is shown in.
pub(super) async fn host(
    id: HostedId,
    launch: Launch,
    runtime: tokio::runtime::Handle,
    commands: mpsc::Receiver<Command>,
    shown: Option<(Generation, mpsc::Sender<FromDaemon>)>,
    reporting: Reporting,
) -> Session {
    let session = Session::with_driver(id, Driver::of(launch.backend));
    let place = Place {
        cwd: launch.cwd.clone(),
        session: launch.conversation.id().clone(),
    };
    let spawned = tokio::task::spawn_blocking(move || spawn::spawn(&launch, &runtime)).await;
    let failure = match spawned {
        Ok(Ok(spawned)) => {
            return session
                .event_loop(&place, spawned, commands, shown, reporting)
                .await;
        }
        Ok(Err(error)) => StartFailure::from(error),
        Err(error) => {
            tracing::error!(%error, "starting a session failed");
            StartFailure::Crashed
        }
    };
    if let Some((key, outbox)) = shown {
        tell(&outbox, FromDaemon::Failed { key, failure }).await;
    }
    session
}

/// What a session's loop is told.
#[derive(Debug)]
pub(super) enum Command {
    Line(String),
    /// Shown in a window from now on, under its key.
    Attach {
        key: Generation,
        outbox: mpsc::Sender<FromDaemon>,
    },
    Detach,
}

/// Where a session's process is in its life (sans-io: the event loop says what happened and
/// when, and is told what to do).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    Running,
    /// Its input closed at this time; it is killed if it hasn't ended a while after.
    Closing(Instant),
    Killed,
}

/// A session's process, as the daemon runs it.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Session {
    pub(super) id: HostedId,
    pub(super) driver: Driver,
    pub(super) phase: Phase,
    /// What it asked the user and wasn't answered: asked again of a window that attaches.
    pub(super) pending: Pending,
    pub(super) lines_in: u64,
    /// Since when no window shows it.
    pub(super) alone_since: Option<Instant>,
    /// When it last said or was told something.
    pub(super) active: Instant,
}

impl Session {
    #[cfg(test)]
    pub(super) fn new(id: HostedId) -> Self {
        Self::with_driver(id, Driver::Claude)
    }

    pub(super) fn with_driver(id: HostedId, driver: Driver) -> Self {
        Self {
            id,
            driver,
            phase: Phase::Running,
            pending: Pending::default(),
            lines_in: 0,
            alone_since: None,
            active: Instant::now(),
        }
    }

    /// Marks the input closed at `now`; a process that doesn't end within [`GRACE`] is killed.
    pub(super) fn input_closed(&mut self, now: Instant) {
        if self.phase == Phase::Running {
            self.phase = Phase::Closing(now);
        }
    }

    /// Returns when the process is to be killed, if it is to be.
    pub(super) fn kill_at(&self) -> Option<Instant> {
        match self.phase {
            Phase::Closing(since) => Some(since + GRACE),
            Phase::Running | Phase::Killed => None,
        }
    }

    /// When, alone and quiet, it is to be ended.
    pub(super) fn quiet_end_at(&self) -> Option<Instant> {
        let alone = self.alone_since?;
        (self.phase == Phase::Running).then(|| alone.max(self.active) + QUIET)
    }

    /// Queues a line for the process's input without waiting, so that a process that isn't
    /// reading holds up nothing else. One that has left [`QUEUE`] lines or
    /// [`QUEUED_BYTES`](super::pipes::QUEUED_BYTES) unread has hung, and is ended as a closed
    /// input ends it.
    pub(super) fn write(&mut self, stdin: &mut Option<Input>, line: String) {
        let Some(input) = stdin.as_mut() else {
            return;
        };
        match input.queue(line) {
            Ok(()) => {}
            Err(Unqueued::Closed) => {
                tracing::debug!(id = ?self.id, "claude's input closed");
            }
            Err(Unqueued::Full) => {
                tracing::warn!(id = ?self.id, "claude isn't reading its input: ending it");
                *stdin = None;
                self.input_closed(Instant::now());
            }
        }
    }

    /// Runs the session until the process ends: lines to its input, its output to the window
    /// that shows it, and its end.
    pub(super) async fn event_loop(
        mut self,
        place: &Place,
        spawned: spawn::Spawned,
        mut commands: mpsc::Receiver<Command>,
        shown: Option<(Generation, mpsc::Sender<FromDaemon>)>,
        reporting: Reporting,
    ) -> Self {
        let cwd = place.cwd.as_path();
        let Reporting { states, keeper } = reporting;
        let mut doing = Doing::default();
        // What the keeper says a turn changed; this loop keeps a sender, so it stays open.
        let (changed, mut turns_changed) = mpsc::channel(QUEUE);
        let spawn::Spawned { mut child } = spawned;
        let Some(pid) = child.id() else {
            return self;
        };
        let mut shown = shown;
        let mut last_state = HostedState::Idle;
        if shown.is_none() {
            self.alone_since = Some(Instant::now());
        }
        if let Some((key, outbox)) = &shown {
            tell(
                outbox,
                FromDaemon::Started {
                    key: *key,
                    pid,
                    turns: 0,
                },
            )
            .await;
        }
        let mut stdin = child.stdin.take().map(Input::start);
        let (lines, mut output) = mpsc::channel(QUEUE);
        let pumps = [
            child
                .stdout
                .take()
                .map(|out| tokio::spawn(pump(out, Stream::Out, lines.clone()))),
            child
                .stderr
                .take()
                .map(|err| tokio::spawn(pump(err, Stream::Err, lines.clone()))),
        ];
        drop(lines);
        let mut commands_open = true;
        let mut output_open = true;
        let code = loop {
            let deadline = self
                .kill_at()
                .or_else(|| self.quiet_end_at())
                .map(tokio::time::sleep_until);
            let deadline = async {
                match deadline {
                    Some(sleep) => sleep.await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                command = commands.recv(), if commands_open && self.phase == Phase::Running => match command {
                    Some(Command::Line(line)) => {
                        self.lines_in += 1;
                        self.active = Instant::now();
                        match serde_json::from_str::<agent::Command>(&line) {
                            Ok(command) => {
                                self.pending.sent(&command);
                                self.publish_state(&mut last_state, states.as_ref());
                                if let Some(job) = doing.follow(self.pending.in_turn(), place, &changed) {
                                    keep(&keeper, job).await;
                                }
                                match self.driver.encode_command(command) {
                                    Ok(Some(encoded)) => self.write(&mut stdin, encoded),
                                    Ok(None) => {
                                        tracing::warn!(id = ?self.id, "a command the program has no input for");
                                    }
                                    Err(error) => {
                                        tracing::warn!(%error, id = ?self.id, "couldn't encode command for driver");
                                    }
                                }
                            }
                            Err(error) => {
                                tracing::warn!(%error, id = ?self.id, "dropping unreadable command from window");
                            }
                        }
                    }
                    Some(Command::Attach { key, outbox }) => {
                        self.alone_since = None;
                        let turns = self.pending.turns;
                        tell(&outbox, FromDaemon::Started { key, pid, turns }).await;
                        // What it asked meanwhile, asked again.
                        for line in self.pending.lines() {
                            send_line(&outbox, key, Stream::Out, line).await;
                        }
                        shown = Some((key, outbox));
                    }
                    Some(Command::Detach) => {
                        shown = None;
                        self.alone_since = Some(Instant::now());
                    }
                    None => {
                        commands_open = false;
                        stdin = None;
                        self.input_closed(Instant::now());
                    }
                },
                line = output.recv(), if output_open => match line {
                    Some((Stream::Out, line)) => {
                        self.active = Instant::now();
                        match self.driver.decode_output(line) {
                            Decoded::Event { event, wire_line } => {
                                doing.saw(&event);
                                self.pending.saw(&event, &wire_line, cwd);
                                self.publish_state(&mut last_state, states.as_ref());
                                if let Some(job) = doing.follow(self.pending.in_turn(), place, &changed) {
                                    keep(&keeper, job).await;
                                }
                                if let Some((key, outbox)) = &shown {
                                    send_line(outbox, *key, Stream::Out, &wire_line).await;
                                }
                            }
                            Decoded::UnreadableRequest {
                                reply_to_process,
                                notice,
                            } => {
                                if !reply_to_process.is_empty() {
                                    self.write(&mut stdin, reply_to_process);
                                }
                                if let Some((key, outbox)) = &shown {
                                    send_line(outbox, *key, Stream::Err, &notice).await;
                                }
                            }
                            Decoded::Log(line) => {
                                if !line.is_empty()
                                    && let Some((key, outbox)) = &shown
                                {
                                    send_line(outbox, *key, Stream::Err, &line).await;
                                }
                            }
                        }
                    }
                    Some((Stream::Err, line)) => {
                        self.active = Instant::now();
                        if let Some((key, outbox)) = &shown {
                            send_line(outbox, *key, Stream::Err, &line).await;
                        }
                    }
                    None => output_open = false,
                },
                Some(summary) = turns_changed.recv() => {
                    if let Some((key, outbox)) = &shown {
                        tell(outbox, FromDaemon::TurnChanged { key: *key, summary }).await;
                    }
                },
                ended = child.wait() => break ended.ok().and_then(|status| status.code()),
                () = deadline => {
                    if self.phase == Phase::Running {
                        // Alone and quiet for long: ended as a closed input ends it.
                        stdin = None;
                        self.input_closed(Instant::now());
                    } else {
                        self.phase = Phase::Killed;
                        if let Err(error) = child.start_kill() {
                            tracing::warn!(%error, id = ?self.id, "couldn't kill claude");
                        }
                    }
                }
            }
        };
        // What it wrote goes out before its end is told.
        for pump in pumps.into_iter().flatten() {
            if let Err(error) = pump.await {
                tracing::debug!(%error, "an output pump failed");
            }
        }
        while let Some((stream, line)) = output.recv().await {
            if stream == Stream::Out {
                match self.driver.decode_output(line) {
                    Decoded::Event { event, wire_line } => {
                        doing.saw(&event);
                        if let Some((key, outbox)) = &shown {
                            send_line(outbox, *key, Stream::Out, &wire_line).await;
                        }
                    }
                    Decoded::UnreadableRequest { notice, .. } => {
                        if let Some((key, outbox)) = &shown {
                            send_line(outbox, *key, Stream::Err, &notice).await;
                        }
                    }
                    Decoded::Log(line) => {
                        if !line.is_empty()
                            && let Some((key, outbox)) = &shown
                        {
                            send_line(outbox, *key, Stream::Err, &line).await;
                        }
                    }
                }
            } else if let Some((key, outbox)) = &shown {
                send_line(outbox, *key, Stream::Err, &line).await;
            }
        }
        // A turn the process ended in ends with it.
        if let Some(job) = doing.follow(false, place, &changed) {
            keep(&keeper, job).await;
        }
        if let Some((key, outbox)) = &shown {
            tell(outbox, FromDaemon::Ended { key: *key, code }).await;
        }
        self
    }

    pub(super) fn publish_state(
        &self,
        last: &mut HostedState,
        states: Option<&mpsc::UnboundedSender<(HostedId, HostedState)>>,
    ) {
        let next = self.pending.state();
        if next == *last {
            return;
        }
        *last = next.clone();
        if let Some(tx) = states
            && tx.send((self.id, next)).is_err()
        {
            tracing::debug!(id = ?self.id, "hub ended before state update");
        }
    }
}
