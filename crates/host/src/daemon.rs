//! The daemon (`edualcd`): runs the sessions for the windows, which outlive the window that
//! started them, and does what windows ask of the desktop.

mod acts;
mod connection;
mod hub;
mod pending;
mod pipes;
mod session;

use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::ipc::{self, FromDaemon, ToDaemon};
use crate::sessions::ClaudeHome;
use crate::{files, snapshots};

use connection::{Shared, WindowId, window, window_over};
use hub::Hub;

/// How long the daemon waits, with no window and no session, before it ends.
const IDLE: Duration = Duration::from_secs(5);
/// How long a window has to say hello.
const HELLO: Duration = Duration::from_secs(2);
/// How long a process whose input closed gets to end before it is killed.
const GRACE: Duration = Duration::from_secs(3);
/// How long a session no window shows may stay quiet before it is ended.
const QUIET: Duration = Duration::from_secs(60 * 60);
/// Messages queued for a window, requests from it, commands for a session.
const QUEUE: usize = 1024;
/// Permission requests kept for a window to come: more unanswered than this is a runaway.
const MAX_PENDING: usize = 64;

/// A window's line, whatever carries it: what it asks (its hello first) and where what the
/// daemon says to it goes. Whatever carries the line makes the messages and takes them.
#[derive(Debug)]
pub struct Line {
    pub requests: mpsc::Receiver<ToDaemon>,
    pub outbox: mpsc::Sender<FromDaemon>,
}

impl Line {
    /// Makes a line and its other end: what a window asks goes in at the sender, and what the
    /// daemon says comes out at the receiver.
    #[must_use]
    pub fn pair() -> (Self, mpsc::Sender<ToDaemon>, mpsc::Receiver<FromDaemon>) {
        let (asks, requests) = mpsc::channel(QUEUE);
        let (outbox, says) = mpsc::channel(QUEUE);
        (Self { requests, outbox }, asks, says)
    }
}

/// Where the daemon takes windows from besides its socket.
#[derive(Debug, Default)]
pub struct Options {
    /// The network side, if there is one: lines that show the token are windows too. The daemon
    /// does not end while it serves them, as a service doesn't.
    pub network: Option<crate::net::Config>,
}

/// Runs the daemon; it returns as it ends.
pub fn run(options: Options) -> std::process::ExitCode {
    // Closed to the user's other processes: its memory, and the descriptors it holds (the
    // claude processes' and the image decoder's pipes). Windows check it by user, not program.
    if let Err(error) =
        rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
    {
        tracing::warn!(%error, "the daemon stays open to the user's other processes");
    }
    let Some(claude) = ClaudeHome::of_user() else {
        tracing::error!(
            "no folder of Claude Code's: neither $CLAUDE_CONFIG_DIR nor a home folder is set"
        );
        return std::process::ExitCode::FAILURE;
    };
    let folder = match ipc::Folder::open() {
        Ok(folder) => folder,
        Err(error) => {
            tracing::error!(%error, "no private place for the daemon");
            return std::process::ExitCode::FAILURE;
        }
    };
    let listener = match ipc::Listener::bind(&folder) {
        Ok(Some(listener)) => listener,
        Ok(None) => {
            tracing::info!("another daemon runs");
            return std::process::ExitCode::SUCCESS;
        }
        Err(error) => {
            tracing::error!(%error, "couldn't listen");
            return std::process::ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, "no runtime");
            return std::process::ExitCode::FAILURE;
        }
    };
    match runtime.block_on(serve(listener, options, claude)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "the daemon failed");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Takes windows as they connect, until no window and no session is left for [`IDLE`].
async fn serve(
    listener: ipc::Listener,
    options: Options,
    claude: ClaudeHome,
) -> Result<(), ipc::Error> {
    let lingers = options.network.is_some();
    let mut network = options.network.map(|config| {
        let (lines, taken) = mpsc::channel(QUEUE);
        let access = crate::net::Access::new(config.token, lines);
        let line = crate::net::router(access);
        let app = match config.page {
            Some(page) => page.merge(line),
            None => line,
        };
        tokio::spawn(async move {
            if let Err(error) = crate::net::serve(config.listen, config.tls, app).await {
                tracing::error!(%error, "the network side failed");
            }
        });
        taken
    });
    let (listener, _lock) = listener.into_tokio()?;
    let (hub, requests) = mpsc::channel(QUEUE);
    let (running, mut sessions) = watch::channel(0usize);
    // With no state folder, nothing keeps the changes, and what is sent for them is dropped.
    let (keeper, jobs) = mpsc::channel(QUEUE);
    let keeper_task =
        snapshots::home().map(|home| tokio::spawn(snapshots::keep(home, claude.clone(), jobs)));
    let hub_task = tokio::spawn(Hub::default().event_loop(
        requests,
        running,
        tokio::runtime::Handle::current(),
        keeper.clone(),
    ));
    let (settings_keeper, settings_changes) = mpsc::channel(QUEUE);
    tokio::spawn(files::keep_settings(settings_changes));
    let (catalog, catalog_queries) = mpsc::channel(QUEUE);
    tokio::spawn(files::keep_catalog(catalog_queries));
    let shared = Shared {
        hub,
        keeper,
        settings_keeper,
        catalog,
        claude,
    };
    let mut windows = JoinSet::new();
    let mut last_window = WindowId::default();
    let mut quiet_since = Some(Instant::now());
    loop {
        let idle = async {
            match quiet_since {
                Some(since) => tokio::time::sleep_until(since + IDLE).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            accepted = ipc::AsyncChannel::accept(&listener) => match accepted {
                Ok(channel) => {
                    last_window = last_window.next();
                    windows.spawn(window(channel, last_window, shared.clone()));
                    quiet_since = None;
                }
                Err(error) => tracing::warn!(%error, "refused a connection"),
            },
            line = async {
                match network.as_mut() {
                    Some(lines) => lines.recv().await,
                    None => std::future::pending().await,
                }
            } => match line {
                Some(line) => {
                    last_window = last_window.next();
                    windows.spawn(window_over(line, last_window, shared.clone()));
                    quiet_since = None;
                }
                None => {
                    tracing::error!("the network side ended: ending");
                    break;
                }
            },
            Some(ended) = windows.join_next() => {
                if let Err(error) = ended {
                    tracing::error!(%error, "a window's connection failed");
                }
            },
            changed = sessions.changed() => {
                // As a window asked it to shut down, its sessions ended.
                if changed.is_err() {
                    tracing::info!("the hub ended: ending");
                    break;
                }
            },
            () = idle => {
                tracing::info!("no window and no session for a while: ending");
                break;
            },
        }
        let quiet = !lingers && windows.is_empty() && *sessions.borrow() == 0;
        quiet_since = match (quiet, quiet_since) {
            (true, None) => Some(Instant::now()),
            (true, since @ Some(_)) => since,
            (false, _) => None,
        };
    }
    // The hub's requests close; it ends its sessions, and is awaited; then the keeper, once
    // they have sent it their last turns.
    let Shared { hub, keeper, .. } = shared;
    drop(hub);
    if let Err(error) = hub_task.await {
        tracing::error!(%error, "the hub failed");
    }
    drop(keeper);
    if let Some(Err(error)) = match keeper_task {
        Some(task) => Some(task.await),
        None => None,
    } {
        tracing::error!(%error, "the changes' keeper failed");
    }
    Ok(())
}

/// Hands the changes' keeper a job; with no keeper (no state folder), it is dropped.
async fn keep(keeper: &mpsc::Sender<snapshots::Job>, job: snapshots::Job) {
    if keeper.send(job).await.is_err() {
        tracing::debug!("nothing keeps the changes");
    }
}

/// Tells a window something; a window gone isn't told.
async fn tell(outbox: &mpsc::Sender<FromDaemon>, message: FromDaemon) {
    if outbox.send(message).await.is_err() {
        tracing::debug!("a window gone");
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    use super::acts::create_project;
    use super::connection::{Ask, Connection, Effect};
    use super::hub::{Order, Step, ToHub};
    use super::pending::Pending;
    use super::pipes::{Input, QUEUED_BYTES, Unqueued};
    use super::session::{Command, Doing, Place, Reporting, Session};
    use super::*;
    use crate::agent;
    use crate::driver::{Decoded, Driver};
    use crate::external::wire;
    use crate::ids::SessionId;
    use crate::ipc::{
        CreateProjectError, Generation, HostedId, HostedState, ProjectName, StartFailure, Stream,
    };
    use crate::provider::{Conversation, FolderConfiguration, Launch, SignInWith};
    use crate::spawn;

    #[tokio::test]
    async fn a_line_over_the_network_is_greeted() {
        let (line, asks, mut says) = Line::pair();
        let (hub, _hub_requests) = mpsc::channel(8);
        let (settings_keeper, _settings_changes) = mpsc::channel(8);
        let (catalog, _catalog_queries) = mpsc::channel(8);
        let shared = Shared {
            hub,
            keeper: no_keeper(),
            settings_keeper,
            catalog,
            claude: ClaudeHome::moved_to(PathBuf::from("/nowhere")),
        };
        let window = tokio::spawn(window_over(line, WindowId(1), shared));
        asks.send(ToDaemon::Hello {
            protocol: ipc::PROTOCOL,
        })
        .await
        .expect("sent");
        assert!(matches!(
            says.recv().await,
            Some(FromDaemon::Hello { protocol, .. }) if protocol == ipc::PROTOCOL
        ));
        drop(asks);
        window
            .await
            .expect("the window's connection ends with its line");
    }

    fn launch() -> Launch {
        Launch {
            backend: crate::provider::Backend::Claude,
            cwd: PathBuf::from("/tmp"),
            conversation: Conversation::New(
                SessionId::try_from("11111111-2222-4333-8444-555555555555".to_owned())
                    .expect("a session id"),
            ),
            model: None,
            folder_configuration: FolderConfiguration::Read,
            sign_in: crate::provider::SignInWith::Account,
        }
    }

    /// A keeper nobody runs: what is sent to it is dropped.
    fn no_keeper() -> mpsc::Sender<snapshots::Job> {
        mpsc::channel(8).0
    }

    fn place() -> Place {
        Place {
            cwd: PathBuf::new(),
            session: launch().conversation.id().clone(),
        }
    }

    fn reporting() -> Reporting {
        Reporting {
            states: None,
            keeper: no_keeper(),
        }
    }

    #[test]
    fn a_session_tells_the_keeper_its_turns() {
        let (changed, _summaries) = mpsc::channel(1);
        let place = place();
        let mut doing = Doing::default();
        let decode_event = |line: &str| match Driver::Claude.decode_output(line.to_owned()) {
            Decoded::Event { event, .. } => event,
            other @ (Decoded::UnreadableRequest { .. } | Decoded::Log(_)) => {
                panic!("expected event, got {other:?}")
            }
        };
        let early = decode_event(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t0","name":"Write","input":{"file_path":"/p/early.rs","content":"x"}}]},"parent_tool_use_id":null}"#,
        );
        doing.saw(&early);
        assert!(doing.follow(false, &place, &changed).is_none());
        assert!(matches!(
            doing.follow(true, &place, &changed),
            Some(snapshots::Job::TurnStarted { .. })
        ));
        assert!(doing.follow(true, &place, &changed).is_none());
        let edit = decode_event(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"/p/a.rs","old_string":"a","new_string":"b"}}]},"parent_tool_use_id":null}"#,
        );
        doing.saw(&edit);
        // An ACP agent's edit, as opencode gives one.
        doing.saw(&agent::Event::ToolCall {
            id: crate::ids::ToolUseId::from("c1".to_owned()),
            call: crate::call::AcpCall {
                title: "edit".to_owned(),
                kind: Some(crate::call::AcpKind::Edit),
                locations: vec![PathBuf::from("/p/b.rs")],
                diffs: Vec::new(),
                input: None,
                status: crate::call::AcpStatus::Completed,
                output: None,
            },
            parent_tool_use_id: None,
        });
        let Some(snapshots::Job::TurnEnded { touched, .. }) = doing.follow(false, &place, &changed)
        else {
            panic!("the turn ends");
        };
        assert_eq!(
            touched,
            BTreeSet::from([PathBuf::from("/p/a.rs"), PathBuf::from("/p/b.rs")])
        );
        assert_eq!(doing, Doing::BetweenTurns);
    }

    fn piece(key: Generation, text: &str, last: bool) -> ToDaemon {
        ToDaemon::Command {
            key,
            text: text.to_owned(),
            last,
        }
    }

    #[test]
    fn a_connection_works_out_what_requests_come_to() {
        let key = Generation::default().next();
        let unused_key = key.next();
        let mut connection = Connection::new(WindowId(1));
        assert_eq!(
            connection.handle(ToDaemon::Start {
                key,
                launch: launch()
            }),
            vec![Effect::Hub(Ask::Start {
                key,
                launch: launch()
            })]
        );
        assert_eq!(
            connection.handle(ToDaemon::Attach {
                key,
                hosted: HostedId::default().next()
            }),
            vec![Effect::Refuse {
                key,
                failure: StartFailure::KeyInUse
            }]
        );
        assert!(connection.handle(piece(key, "{\"a\":", false)).is_empty());
        assert_eq!(
            connection.handle(piece(key, "1}", true)),
            vec![Effect::Hub(Ask::Line {
                key,
                line: "{\"a\":1}".to_owned()
            })]
        );
        assert!(
            connection.handle(piece(unused_key, "x", true)).is_empty(),
            "no key, no line"
        );
        assert_eq!(
            connection.handle(ToDaemon::Detach { key }),
            vec![Effect::Hub(Ask::Detach { key })]
        );
        assert!(
            connection.handle(ToDaemon::End { key }).is_empty(),
            "let go once"
        );
    }

    #[test]
    fn the_hub_keeps_sessions_past_their_window() {
        let (first_window, second_window) = (WindowId(1), WindowId(2));
        let first_key = Generation::default().next();
        let second_key = first_key.next();
        let mut hub = Hub::default();
        let started = hub.handle(
            first_window,
            Ask::Start {
                key: first_key,
                launch: launch(),
            },
        );
        let Some(&Step::Start { id: hosted, .. }) = started.first() else {
            panic!("no session started: {started:?}");
        };
        // Listed under the id it was started with, before it has a transcript, so the record
        // Claude Code writes for the same process is known as this one.
        assert_eq!(
            hub.list().first().map(|listed| &listed.session),
            Some(launch().conversation.id())
        );
        assert_eq!(hub.shown_as(first_window, first_key), Some(hosted));
        assert_eq!(
            hub.handle(
                second_window,
                Ask::Attach {
                    key: second_key,
                    hosted
                }
            ),
            vec![Step::Answer(FromDaemon::Failed {
                key: second_key,
                failure: StartFailure::NotHosted
            })],
            "shown in the first window"
        );
        assert_eq!(
            hub.left(first_window),
            vec![
                Step::Tell {
                    id: hosted,
                    command: Order::Detach
                },
                Step::Changed
            ]
        );
        assert!(hub.list().iter().all(|listed| !listed.attached));
        assert_eq!(
            hub.handle(
                second_window,
                Ask::Attach {
                    key: second_key,
                    hosted
                }
            ),
            vec![
                Step::Tell {
                    id: hosted,
                    command: Order::Attach { key: second_key }
                },
                Step::Changed
            ]
        );
        let session = SessionId::try_from("ee79c043-bfa7-44c0-82b6-509839a05c22".to_owned())
            .expect("a session id");
        assert_eq!(
            hub.handle(
                second_window,
                Ask::Identify {
                    key: second_key,
                    session: session.clone()
                }
            ),
            vec![Step::Changed]
        );
        assert_eq!(
            hub.list().first().map(|listed| listed.session.clone()),
            Some(session)
        );
        assert_eq!(
            hub.handle(second_window, Ask::End { key: second_key }),
            vec![Step::Tell {
                id: hosted,
                command: Order::End
            }]
        );
        assert_eq!(hub.ended(hosted), vec![Step::Changed]);
        assert!(hub.list().is_empty());
    }

    #[test]
    fn the_hub_lists_each_sessions_sign_in() {
        let window = WindowId(1);
        let first_key = Generation::default().next();
        let second_key = first_key.next();
        let mut hub = Hub::default();
        hub.handle(
            window,
            Ask::Start {
                key: first_key,
                launch: launch(),
            },
        );
        hub.handle(
            window,
            Ask::Start {
                key: second_key,
                launch: Launch {
                    sign_in: SignInWith::ApiKey,
                    ..launch()
                },
            },
        );
        let sign_ins: Vec<SignInWith> = hub.list().iter().map(|listed| listed.sign_in).collect();
        assert_eq!(sign_ins, [SignInWith::Account, SignInWith::ApiKey]);
    }

    fn claude_saw(pending: &mut Pending, line: &str) -> String {
        claude_saw_in(pending, line, Path::new("/code/beep"))
    }

    fn claude_saw_in(pending: &mut Pending, line: &str, cwd: &Path) -> String {
        match Driver::Claude.decode_output(line.to_owned()) {
            Decoded::Event { event, wire_line } => {
                pending.saw(&event, &wire_line, cwd);
                wire_line
            }
            Decoded::Log(raw) => raw,
            Decoded::UnreadableRequest { .. } => panic!("unexpected unreadable request"),
        }
    }

    fn prompt_command(text: &str) -> agent::Command {
        agent::Command::Prompt {
            shell: Vec::new(),
            images: Vec::new(),
            text: text.to_owned(),
        }
    }

    #[test]
    fn the_latest_init_is_told_first() {
        let mut pending = Pending::default();
        let first = r#"{"type":"system","subtype":"init","session_id":"11111111-2222-4333-8444-555555555555","model":"m1","permissionMode":"default","cwd":"/code"}"#;
        let later = r#"{"type":"system","subtype":"init","session_id":"11111111-2222-4333-8444-555555555555","model":"m2","permissionMode":"default","cwd":"/code"}"#;
        let asked = r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{}}}"#;
        claude_saw(&mut pending, first);
        let asked_wire = claude_saw(&mut pending, asked);
        let later_wire = claude_saw(&mut pending, later);
        claude_saw(
            &mut pending,
            r#"{"type":"system","subtype":"status","status":"requesting"}"#,
        );
        assert_eq!(
            pending.lines().cloned().collect::<Vec<_>>(),
            vec![later_wire, asked_wire]
        );
    }

    #[test]
    fn the_latest_background_tasks_are_told_after_the_init() {
        let mut pending = Pending::default();
        let init = r#"{"type":"system","subtype":"init","session_id":"11111111-2222-4333-8444-555555555555","model":"m","permissionMode":"default","cwd":"/code"}"#;
        let two = r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"t1","task_type":"local_agent","description":"a"},{"task_id":"t2","task_type":"local_bash","description":"b"}]}"#;
        let one = r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"t2","task_type":"local_bash","description":"b"}]}"#;
        let none = r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#;
        let asked = r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{}}}"#;
        claude_saw(&mut pending, two);
        let init_wire = claude_saw(&mut pending, init);
        let asked_wire = claude_saw(&mut pending, asked);
        let one_wire = claude_saw(&mut pending, one);
        assert_eq!(
            pending.lines().cloned().collect::<Vec<_>>(),
            vec![init_wire.clone(), one_wire, asked_wire.clone()]
        );
        let none_wire = claude_saw(&mut pending, none);
        assert_eq!(
            pending.lines().cloned().collect::<Vec<_>>(),
            vec![init_wire, none_wire, asked_wire]
        );
    }

    #[test]
    fn open_tasks_are_told_until_they_end() {
        let mut pending = Pending::default();
        let init = r#"{"type":"system","subtype":"init","session_id":"11111111-2222-4333-8444-555555555555","model":"m","permissionMode":"default","cwd":"/code"}"#;
        let list = r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#;
        let started = |id: &str| {
            format!(
                r#"{{"type":"system","subtype":"task_started","task_id":"{id}","is_backgrounded":false,"task_type":"local_agent","description":"d"}}"#
            )
        };
        let (agent, other) = (started("a1"), started("a2"));
        let moved = r#"{"type":"system","subtype":"task_updated","task_id":"a1","patch":{"is_backgrounded":true}}"#;
        let ended = r#"{"type":"system","subtype":"task_notification","task_id":"a2","status":"completed"}"#;
        let asked = r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{}}}"#;
        let init_wire = claude_saw(&mut pending, init);
        let list_wire = claude_saw(&mut pending, list);
        let agent_wire = claude_saw(&mut pending, &agent);
        let other_wire = claude_saw(&mut pending, &other);
        let asked_wire = claude_saw(&mut pending, asked);
        let moved_wire = claude_saw(&mut pending, moved);
        assert_eq!(
            pending.lines().cloned().collect::<Vec<_>>(),
            vec![
                init_wire.clone(),
                list_wire.clone(),
                agent_wire.clone(),
                moved_wire.clone(),
                other_wire,
                asked_wire.clone(),
            ]
        );
        claude_saw(&mut pending, ended);
        assert_eq!(
            pending.lines().cloned().collect::<Vec<_>>(),
            vec![
                init_wire.clone(),
                list_wire.clone(),
                agent_wire,
                moved_wire,
                asked_wire.clone(),
            ]
        );
        claude_saw(
            &mut pending,
            r#"{"type":"result","subtype":"success","is_error":false}"#,
        );
        assert_eq!(
            pending.lines().cloned().collect::<Vec<_>>(),
            vec![init_wire, list_wire, asked_wire]
        );
    }

    #[test]
    fn a_tasks_latest_progress_is_told_until_it_ends() {
        let mut pending = Pending::default();
        let progress = |tokens: u32| {
            format!(
                r#"{{"type":"system","subtype":"task_progress","task_id":"a1","description":"d","usage":{{"total_tokens":{tokens},"tool_uses":1,"duration_ms":5}}}}"#
            )
        };
        let (early, late) = (progress(10), progress(20));
        claude_saw(&mut pending, &early);
        let late_wire = claude_saw(&mut pending, &late);
        claude_saw(
            &mut pending,
            r#"{"type":"result","subtype":"success","is_error":false}"#,
        );
        assert_eq!(
            pending.lines().cloned().collect::<Vec<_>>(),
            vec![late_wire]
        );
        claude_saw(
            &mut pending,
            r#"{"type":"system","subtype":"task_notification","task_id":"a1","status":"completed"}"#,
        );
        assert_eq!(pending.lines().count(), 0);
    }

    #[test]
    fn turns_under_way_are_counted_for_a_window_attaching() {
        let mut pending = Pending::default();
        let prompt = prompt_command("Hi");
        let result = r#"{"type":"result","subtype":"success","is_error":false}"#;
        pending.sent(&prompt);
        pending.sent(&prompt);
        pending.sent(&agent::Command::Shell {
            command: "ls".to_owned(),
        });
        pending.sent(&agent::Command::ControlRequest {
            request_id: crate::ids::RequestId::from("gui_1".to_owned()),
            request: wire::ControlRequest::Interrupt,
        });
        claude_saw(
            &mut pending,
            r#"{"type":"user","message":{"role":"user","content":"<bash-input>ls</bash-input>"},"isReplay":true}"#,
        );
        assert_eq!(pending.turns, 2);
        claude_saw(&mut pending, result);
        assert_eq!(pending.turns, 1);
        claude_saw(&mut pending, result);
        claude_saw(&mut pending, result);
        assert_eq!(pending.turns, 0, "never below none");
    }

    #[test]
    fn what_was_asked_and_not_answered_is_kept() {
        let mut pending = Pending::default();
        let asked = r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{}}}"#;
        let asked_wire = claude_saw(&mut pending, asked);
        claude_saw(
            &mut pending,
            r#"{"type":"control_request","request_id":"r2","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{}}}"#,
        );
        claude_saw(
            &mut pending,
            r#"{"type":"control_cancel_request","request_id":"r2"}"#,
        );
        claude_saw(&mut pending, "not json");
        assert_eq!(
            pending.lines().cloned().collect::<Vec<_>>(),
            vec![asked_wire]
        );
        pending.sent(&agent::Command::ControlResponse {
            response: wire::Reply::Success {
                request_id: crate::ids::RequestId::from("r1".to_owned()),
                response: wire::PermissionResult::Deny {
                    message: String::new(),
                    interrupt: false,
                },
            },
        });
        assert_eq!(pending.lines().count(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_window_joining_hears_the_list_first() {
        let (hub, requests) = mpsc::channel(8);
        let (running, _sessions) = watch::channel(0usize);
        let hub_task = tokio::spawn(Hub::default().event_loop(
            requests,
            running,
            tokio::runtime::Handle::current(),
            no_keeper(),
        ));
        let (outbox, mut told) = mpsc::channel(8);
        hub.send(ToHub::Joined {
            window: WindowId(1),
            outbox,
        })
        .await
        .expect("the hub listens");
        assert_eq!(told.recv().await, Some(FromDaemon::Hosted(Vec::new())));
        drop(hub);
        hub_task.await.expect("the hub ends");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_shutdown_tells_every_window_and_ends_the_hub() {
        let (hub, requests) = mpsc::channel(8);
        let (running, mut sessions) = watch::channel(0usize);
        let hub_task = tokio::spawn(Hub::default().event_loop(
            requests,
            running,
            tokio::runtime::Handle::current(),
            no_keeper(),
        ));
        let mut heard = Vec::new();
        for window in [WindowId(1), WindowId(2)] {
            let (outbox, mut told) = mpsc::channel(8);
            hub.send(ToHub::Joined {
                window,
                outbox: outbox.clone(),
            })
            .await
            .expect("the hub listens");
            assert_eq!(told.recv().await, Some(FromDaemon::Hosted(Vec::new())));
            heard.push((outbox, told));
        }
        let asking = heard
            .first()
            .map(|(outbox, _)| outbox.clone())
            .expect("a window");
        hub.send(ToHub::Asked {
            window: WindowId(1),
            ask: Ask::ShutDown,
            outbox: asking,
        })
        .await
        .expect("the hub listens");
        for (_, told) in &mut heard {
            assert_eq!(told.recv().await, Some(FromDaemon::ShuttingDown));
        }
        hub_task.await.expect("the hub ends of itself");
        // The last count it sent comes first, then its end, which `serve` stops on.
        while sessions.changed().await.is_ok() {}
        drop(hub);
    }

    #[test]
    fn a_window_asks_the_hub_to_shut_down() {
        let mut connection = Connection::new(WindowId(1));
        assert_eq!(
            connection.handle(ToDaemon::ShutDown),
            [Effect::Hub(Ask::ShutDown)]
        );
    }

    #[test]
    fn a_session_ends_a_grace_after_its_input_closed_or_long_alone() {
        let mut session = Session::new(HostedId::default().next());
        assert_eq!(session.kill_at(), None);
        assert_eq!(
            session.quiet_end_at(),
            None,
            "shown, it is never ended for quiet"
        );
        let now = Instant::now();
        session.alone_since = Some(now);
        session.active = now;
        assert_eq!(session.quiet_end_at(), Some(now + QUIET));
        session.input_closed(now);
        assert_eq!(session.kill_at(), Some(now + GRACE));
        session.input_closed(now + GRACE);
        assert_eq!(
            session.kill_at(),
            Some(now + GRACE),
            "the first close counts"
        );
    }

    fn running(program: &str, args: &[&str]) -> spawn::Spawned {
        let mut command = tokio::process::Command::new(program);
        command
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        spawn::Spawned {
            child: command.spawn().expect("cat runs"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_session_asks_again_what_a_window_missed() {
        let (orders, commands) = mpsc::channel(8);
        let (outbox, mut answers) = mpsc::channel(32);
        let asked = r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{}}}"#;
        let expected = match Driver::Claude.decode_output(asked.to_owned()) {
            Decoded::Event { wire_line, .. } => wire_line,
            other @ (Decoded::UnreadableRequest { .. } | Decoded::Log(_)) => {
                panic!("expected event, got {other:?}")
            }
        };
        let key = Generation::default().next();
        let bye = agent::Command::Shell {
            command: "bye".to_owned(),
        }
        .encode()
        .expect("encodes");
        let process = running(
            "sh",
            &[
                "-c",
                "printf '%s\\n' \"$1\"; head -n1 >/dev/null",
                "_",
                asked,
            ],
        );
        let attach = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            orders
                .send(Command::Attach { key, outbox })
                .await
                .expect("queued");
            orders.send(Command::Line(bye)).await.expect("queued");
        });
        let session = Session::new(HostedId::default().next())
            .event_loop(&place(), process, commands, None, reporting())
            .await;
        attach.await.expect("the attaching task");
        assert_eq!(session.lines_in, 1);
        let mut told = Vec::new();
        while let Some(answer) = answers.recv().await {
            told.push(answer);
        }
        assert!(
            matches!(told.first(), Some(FromDaemon::Started { key: started, .. }) if *started == key)
        );
        assert!(told.iter().any(|answer| matches!(
            answer,
            FromDaemon::Output { key: shown, stream: Stream::Out, text, last: true }
                if *shown == key && text == &expected
        )));
        assert!(matches!(
            told.last(),
            Some(FromDaemon::Ended { key: ended, code: Some(0) }) if *ended == key
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_session_not_reading_its_input_goes_on() {
        let (orders, commands) = mpsc::channel(8);
        let (outbox, mut answers) = mpsc::channel(QUEUE);
        let key = Generation::default().next();
        let place = place();
        let session = tokio::spawn(async move {
            Session::new(HostedId::default().next())
                .event_loop(
                    &place,
                    running("sleep", &["30"]),
                    commands,
                    None,
                    reporting(),
                )
                .await
        });
        // More than a pipe holds: its writing waits on the process for good.
        let unread = agent::Command::Shell {
            command: "x".repeat(4 * 1024 * 1024),
        }
        .encode()
        .expect("encodes");
        orders.send(Command::Line(unread)).await.expect("queued");
        orders
            .send(Command::Attach { key, outbox })
            .await
            .expect("queued");
        let started = tokio::time::timeout(Duration::from_secs(5), answers.recv()).await;
        assert!(
            matches!(started, Ok(Some(FromDaemon::Started { key: shown, .. })) if shown == key)
        );
        drop(orders);
        let ended = tokio::time::timeout(GRACE * 3, session).await;
        assert!(matches!(ended, Ok(Ok(Session { lines_in: 1, .. }))));
        let mut told = Vec::new();
        while let Some(answer) = answers.recv().await {
            told.push(answer);
        }
        assert!(matches!(
            told.last(),
            Some(FromDaemon::Ended { key: ended, code: None }) if *ended == key
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_process_input_unread_is_bounded_by_its_bytes() {
        let mut process = running("sleep", &["30"]).child;
        let mut input = Input::start(process.stdin.take().expect("its input"));
        let longest = "x".repeat(crate::ipc::MAX_LINE);
        for _ in 0..QUEUED_BYTES / crate::ipc::MAX_LINE {
            assert_eq!(input.queue(longest.clone()), Ok(()));
        }
        tokio::task::yield_now().await;
        assert_eq!(input.queue("one more".to_owned()), Err(Unqueued::Full));
    }

    #[test]
    fn pending_state_follows_turns_and_questions() {
        let mut pending = Pending::default();
        let here = Path::new("/code/beep");
        assert_eq!(pending.state(), HostedState::Idle);
        let prompt = prompt_command("Hi");
        let result = r#"{"type":"result","subtype":"success","is_error":false}"#;
        let ask = r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[{"question":"Which colour?","header":"Pick","options":[{"label":"Red","description":""}],"multiSelect":false}]}}}"#;
        let answer = agent::Command::ControlResponse {
            response: wire::Reply::Success {
                request_id: crate::ids::RequestId::from("r1".to_owned()),
                response: wire::PermissionResult::Deny {
                    message: String::new(),
                    interrupt: false,
                },
            },
        };

        pending.sent(&prompt);
        assert_eq!(pending.state(), HostedState::Busy);

        claude_saw_in(&mut pending, ask, here);
        assert!(matches!(
            pending.state(),
            HostedState::Waiting { ref question, .. } if question == "Which colour?"
        ));

        pending.sent(&answer);
        assert_eq!(pending.state(), HostedState::Busy);

        claude_saw_in(&mut pending, result, here);
        assert_eq!(pending.state(), HostedState::Idle);
    }

    #[test]
    fn a_turn_the_session_starts_itself_is_a_turn() {
        let mut pending = Pending::default();
        let streamed =
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"m1"}}}"#;
        let said = r#"{"type":"assistant","message":{"content":[]},"parent_tool_use_id":null}"#;
        let result = r#"{"type":"result","subtype":"success","is_error":false}"#;
        let prompt = prompt_command("Hi");

        claude_saw(&mut pending, streamed);
        assert!(pending.in_turn());
        claude_saw(&mut pending, said);
        claude_saw(&mut pending, result);
        assert!(!pending.in_turn());

        pending.sent(&prompt);
        claude_saw(&mut pending, said);
        claude_saw(&mut pending, result);
        assert!(!pending.in_turn(), "a prompted turn ends with its result");
    }

    #[test]
    fn create_project_makes_a_new_folder_inside_an_existing_one() {
        let root = std::env::temp_dir().join(format!("edualc-create-proj-{}", std::process::id()));
        if root.exists() {
            std::fs::remove_dir_all(&root).expect("clean stale temp dir");
        }
        std::fs::create_dir_all(&root).expect("temp root");
        let name = |typed: &str| ProjectName::try_from(typed.to_owned()).expect("a project name");

        let missing = root.join("missing-parent");
        assert_eq!(
            create_project(&missing, &name("ok")),
            Err(CreateProjectError::NoParent(missing))
        );

        let plain = create_project(&root, &name("plain-proj")).expect("creates folder");
        assert!(plain.is_dir());
        assert!(!plain.join(".git").exists());
        assert_eq!(
            create_project(&root, &name("plain-proj")),
            Err(CreateProjectError::AlreadyExists(plain))
        );

        std::fs::remove_dir_all(&root).expect("clean temp dir");
    }
}
