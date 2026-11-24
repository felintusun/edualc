//! How the window starts: its command line read, its daemon reached (or started), and the
//! window opened.

use std::path::PathBuf;

use slint::ComponentHandle;

use crate::folder::{Folder, FolderError};
use crate::ids::SessionId;
use crate::provider::Backend;
use crate::sessions::ClaudeHome;
use crate::{app, connect, decode, desktop, generated, sessions, ui};

/// A native desktop window for coding agents: Claude Code, agy and opencode.
///
/// Its arguments work like claude's own: the folder to work in, the model, a session to
/// continue, and a first prompt. It needs the program it runs on the PATH: claude (Claude Code)
/// by default, agy or opencode with --backend.
#[derive(Debug, clap::Parser)]
#[command(name = "edualc", version, about)]
struct Args {
    /// The program to work with: claude (Claude Code), agy (Google's Antigravity CLI), or
    /// opencode.
    #[arg(long, value_name = "PROGRAM", default_value = "claude", value_parser = backends())]
    backend: Backend,
    /// The model: for claude an alias such as sonnet, or a full name; for agy a name that
    /// `agy models` lists; for opencode one it offers once it runs.
    #[arg(short, long)]
    model: Option<String>,
    /// The permission mode to start in.
    #[arg(long = "permission-mode", value_name = "MODE", value_parser = permission_modes())]
    mode: Option<app::PermissionMode>,
    /// A session to continue, by its id.
    #[arg(short, long, value_name = "SESSION", value_parser = session_id)]
    resume: Option<SessionId>,
    /// The folder to work in. Without one or `--resume`, opens the start page.
    #[arg(value_parser = folder)]
    dir: Option<Folder>,
    /// A first prompt.
    prompt: Option<String>,
}

/// Parses the permission modes by Claude Code's names for them, each with what it lets
/// through, so that help lists them and a wrong one is told the right ones.
fn permission_modes() -> impl clap::builder::TypedValueParser<Value = app::PermissionMode> {
    use clap::builder::{PossibleValue, PossibleValuesParser, TypedValueParser as _};
    PossibleValuesParser::new(
        <app::PermissionMode as strum::IntoEnumIterator>::iter()
            .map(|mode| PossibleValue::new(<&'static str>::from(mode)).help(mode.description())),
    )
    .try_map(|name| name.parse::<app::PermissionMode>())
}

/// Parses the programs by their names, so that help lists them and a wrong one is told the
/// right ones.
fn backends() -> impl clap::builder::TypedValueParser<Value = Backend> {
    use clap::builder::{PossibleValuesParser, TypedValueParser as _};
    PossibleValuesParser::new(
        <Backend as strum::IntoEnumIterator>::iter().map(<&'static str>::from),
    )
    .try_map(|name| name.parse::<Backend>())
}

/// Why the folder named on the command line isn't worked in.
#[derive(Debug, thiserror::Error)]
enum DirError {
    #[error(transparent)]
    Folder(#[from] FolderError),
    /// No folder by that name, nor a path: `claude` takes its prompt first, and this may be one.
    #[error("{source}. A first prompt comes after the folder, as in: edualc . \"{prompt}\"")]
    Prompt {
        prompt: String,
        #[source]
        source: FolderError,
    },
}

/// Reads the folder a command line names, relative to where edualc was started.
fn folder(text: &str) -> Result<Folder, DirError> {
    Folder::at(PathBuf::from(text)).map_err(|error| match error {
        FolderError::Missing(_) if !text.contains('/') => DirError::Prompt {
            prompt: text.to_owned(),
            source: error,
        },
        FolderError::Missing(_) | FolderError::NotAFolder(_) | FolderError::Unreadable { .. } => {
            DirError::Folder(error)
        }
    })
}

/// Returns where edualc was started, or the home folder if that is gone.
fn here() -> Result<Folder, FolderError> {
    Folder::at(
        std::env::current_dir()
            .ok()
            .or_else(dirs::home_dir)
            .unwrap_or_default(),
    )
}

/// Why edualc doesn't start where its command line points.
#[derive(Debug, thiserror::Error)]
enum WhereError {
    #[error(transparent)]
    Folder(#[from] FolderError),
    #[error("There is no session {0}")]
    NoSession(SessionId),
    #[error(
        "There is no folder of Claude Code's to find a session in: set $CLAUDE_CONFIG_DIR or $HOME"
    )]
    NoClaudeHome,
}

impl Args {
    /// Returns the folder to work in: the one named; else, continuing a session, the one it was
    /// started in, or where edualc was started if its transcript names none; else none, which
    /// opens the start page. A first prompt always comes after a folder.
    ///
    /// `claude --resume` goes on in whatever folder it is started from, and its tools work
    /// there, so the session's own folder matters.
    ///
    /// # Errors
    ///
    /// Refuses a session to continue that no transcript in `claude` has, or that can't be
    /// looked for without a `claude`.
    fn working_folder(&self, claude: Option<&ClaudeHome>) -> Result<Option<Folder>, WhereError> {
        let Some(id) = &self.resume else {
            return Ok(self.dir.clone());
        };
        let claude = claude.ok_or(WhereError::NoClaudeHome)?;
        match sessions::saved(claude, id) {
            sessions::Saved::Nowhere => Err(WhereError::NoSession(id.clone())),
            sessions::Saved::StartedIn(_) | sessions::Saved::Unplaced if self.dir.is_some() => {
                Ok(self.dir.clone())
            }
            sessions::Saved::StartedIn(started) => Ok(Some(Folder::at(started)?)),
            sessions::Saved::Unplaced => Ok(Some(here()?)),
        }
    }
}

/// Reads a session's id as Claude Code writes one, refusing anything else before it reaches
/// claude's command line.
fn session_id(id: &str) -> Result<SessionId, String> {
    SessionId::try_from(id.to_owned()).map_err(|error| error.to_string())
}

pub(crate) fn main() -> Result<std::process::ExitCode, slint::PlatformError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "edualc=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    // The image decoder the daemon and the window start, confined before it reads (see
    // `decode`).
    if let Some(job) = std::env::args()
        .nth(1)
        .and_then(|argument| argument.parse::<decode::Job>().ok())
    {
        return Ok(decode::run(job));
    }

    // Help and the version are printed here, and wrong arguments said, before anything starts.
    let args: Args = clap::Parser::parse();
    let claude = ClaudeHome::of_user();
    let cwd = match args.working_folder(claude.as_ref()) {
        Ok(folder) => folder.map(Folder::into_path),
        Err(error) => {
            eprintln!("edualc: {error}");
            return Ok(std::process::ExitCode::from(2));
        }
    };
    let Args {
        backend,
        model,
        mode,
        resume,
        prompt,
        ..
    } = args;

    let connected = match connect::connect() {
        Ok(connected) => connected,
        Err(error) => {
            eprintln!("edualc: {error}");
            return Ok(std::process::ExitCode::FAILURE);
        }
    };
    let connect::Connected { line, greeting } = connected;
    // We ask the desktop's text size on a thread of its own while the fonts are found here:
    // Slint's font collection belongs to the thread that draws.
    let (fonts, text_size) = std::thread::scope(|scope| {
        let text_size = scope.spawn(crate::appearance::desktop_text_size);
        let fonts = crate::appearance::fonts();
        let text_size = text_size
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        (fonts, text_size)
    });
    let desktop = app::Desktop::of(&greeting, text_size, crate::seat::finds_windows(), claude);
    let (events, received) = async_channel::unbounded();
    let link = match connect::run(line, events.clone()) {
        Ok(link) => link,
        Err(error) => {
            eprintln!("edualc: {error}");
            return Ok(std::process::ExitCode::FAILURE);
        }
    };
    let window = generated::AppWindow::new()?;
    // Before `show`: the compositor is told the id once, as the window is made.
    slint::set_xdg_app_id(desktop::APP_ID)?;
    ui::wire(&window, &events);
    window
        .global::<generated::Nav>()
        .set_show_window_controls(show_window_controls());
    let frontend = ui::SlintFrontend::new(window.clone_strong(), fonts, events.clone());
    let mut app = app::App::new(
        frontend,
        events,
        link,
        desktop,
        greeting.settings,
        app::Start {
            cwd,
            backend,
            model,
        },
    );
    app.begin(
        greeting.hosted,
        greeting.live,
        resume,
        mode,
        prompt.as_deref(),
    );
    slint::spawn_local(app.run(received)).expect("the event loop is set up");
    window.show()?;
    slint::run_event_loop_until_quit()?;
    Ok(std::process::ExitCode::SUCCESS)
}

/// Custom window controls (minimize/maximize/close) are hidden under tiling Wayland
/// compositors such as `niri` that manage windows without client-side titlebar buttons.
fn show_window_controls() -> bool {
    show_window_controls_with(
        std::env::var_os("NIRI_SOCKET").as_deref(),
        std::env::var_os("XDG_CURRENT_DESKTOP").as_deref(),
    )
}

fn show_window_controls_with(
    niri_socket: Option<&std::ffi::OsStr>,
    xdg_current_desktop: Option<&std::ffi::OsStr>,
) -> bool {
    if niri_socket.is_some() {
        return false;
    }
    !xdg_current_desktop.is_some_and(|desktop| {
        desktop
            .to_string_lossy()
            .split(':')
            .any(|part| part.eq_ignore_ascii_case("niri"))
    })
}

#[cfg(test)]
mod manual;

#[cfg(test)]
mod tests {
    use clap::{CommandFactory as _, Parser as _};

    use super::*;

    fn args(line: &str) -> Result<Args, clap::Error> {
        Args::try_parse_from(std::iter::once("edualc").chain(line.split_whitespace()))
    }

    #[test]
    fn the_command_line_holds_together() {
        Args::command().debug_assert();
    }

    #[test]
    fn help_and_version_are_asked_for() {
        for line in ["--help", "-h", "--model haiku --help"] {
            let shown = args(line).map(|_| ()).map_err(|error| error.kind());
            assert_eq!(shown, Err(clap::error::ErrorKind::DisplayHelp), "{line}");
        }
        for line in ["--version", "-V", "/tmp --version"] {
            let shown = args(line).map(|_| ()).map_err(|error| error.kind());
            assert_eq!(shown, Err(clap::error::ErrorKind::DisplayVersion), "{line}");
        }
    }

    #[test]
    fn arguments_read_like_claudes() {
        let id = "ee79c043-bfa7-44c0-82b6-509839a05c22";
        let parsed = args(&format!(
            "--model haiku --permission-mode plan -r {id} /tmp hello"
        ))
        .expect("valid arguments");
        assert_eq!(parsed.mode, Some(app::PermissionMode::Plan));
        assert_eq!(parsed.model.as_deref(), Some("haiku"));
        assert_eq!(parsed.resume, SessionId::try_from(id.to_owned()).ok());
        assert_eq!(
            parsed.dir.map(Folder::into_path),
            PathBuf::from("/tmp").canonicalize().ok()
        );
        assert_eq!(parsed.prompt.as_deref(), Some("hello"));
        assert!(args("--resume").is_err());
        // Ids end up on claude's command line: one that reads as an option is refused.
        assert!(args("--resume --dangerously-skip-permissions").is_err());
        assert!(args("--resume ../../etc/x").is_err());
        assert!(args("--bogus").is_err());
        assert!(args("--permission-mode sometimes").is_err());
        assert!(args("/tmp hello extra").is_err());
        let joined = args("--model=opus --permission-mode=acceptEdits").expect("valid arguments");
        assert_eq!(joined.model.as_deref(), Some("opus"));
        assert_eq!(joined.mode, Some(app::PermissionMode::AcceptEdits));
    }

    #[test]
    fn a_wrong_folder_is_said_before_anything_starts() {
        let said = |argument: &str| {
            Args::try_parse_from(["edualc", argument])
                .map(|_| ())
                .map_err(|error| error.to_string())
                .expect_err(argument)
        };
        let prompt = said("explain this repository");
        assert!(
            prompt.contains("There is no folder explain this repository")
                && prompt.contains("edualc . \"explain this repository\""),
            "{prompt}"
        );
        let missing = said("/no/such/edualc/folder");
        assert!(
            missing.contains("There is no folder /no/such/edualc/folder")
                && !missing.contains("prompt"),
            "{missing}"
        );
        let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
        assert!(
            said(manifest).contains(&format!("{manifest} is a file, not a folder")),
            "{manifest}"
        );
    }

    #[test]
    fn help_lists_the_permission_modes() {
        let help = Args::command().render_long_help().to_string();
        for mode in <app::PermissionMode as strum::IntoEnumIterator>::iter() {
            let name: &str = mode.into();
            let listed = help.lines().any(|line| {
                line.trim_start()
                    .strip_prefix(&format!("- {name}:"))
                    .is_some_and(|rest| rest.trim() == mode.description())
            });
            assert!(listed, "{name} in\n{help}");
        }
    }

    fn folder_for(line: &str, claude: Option<&ClaudeHome>) -> Result<Option<PathBuf>, WhereError> {
        args(line)
            .expect("a command line")
            .working_folder(claude)
            .map(|folder| folder.map(Folder::into_path))
    }

    #[test]
    fn a_folder_is_worked_in_as_named_and_none_opens_the_start_page() {
        assert_eq!(folder_for("", None).expect("the start page"), None);
        assert_eq!(
            folder_for("/tmp", None).expect("a folder"),
            PathBuf::from("/tmp").canonicalize().ok()
        );
    }

    #[test]
    fn a_session_goes_on_in_the_folder_it_was_started_in_unless_one_is_named() {
        let scratch = std::env::temp_dir().join(format!("edualc-resume-{}", std::process::id()));
        let started = scratch.join("work");
        std::fs::create_dir_all(&started).expect("the session's folder");
        let claude = ClaudeHome::moved_to(scratch.join("claude"));
        let id = "5d6e7f80-91a2-4b3c-8d4e-5f60718293a4";
        let transcript = scratch
            .join("claude/projects/-work")
            .join(format!("{id}.jsonl"));
        std::fs::create_dir_all(transcript.parent().expect("a folder")).expect("a project");
        let line = serde_json::json!({ "type": "user", "cwd": started, "sessionId": id });
        std::fs::write(&transcript, format!("{line}\n")).expect("a transcript");

        let resume = format!("--resume {id}");
        assert_eq!(
            folder_for(&resume, Some(&claude)).expect("a session"),
            started.canonicalize().ok()
        );
        assert_eq!(
            folder_for(&format!("{resume} /tmp"), Some(&claude)).expect("a session elsewhere"),
            PathBuf::from("/tmp").canonicalize().ok()
        );
        assert!(matches!(
            folder_for(
                "--resume 00000000-1111-4222-8333-444444444444",
                Some(&claude)
            ),
            Err(WhereError::NoSession(_))
        ));
        assert!(matches!(
            folder_for(&resume, None),
            Err(WhereError::NoClaudeHome)
        ));
        std::fs::remove_dir_all(&scratch).expect("the scratch folder goes");
    }

    #[test]
    fn window_controls_hidden_under_niri() {
        use std::ffi::OsStr;
        assert!(show_window_controls_with(None, None));
        assert!(show_window_controls_with(None, Some(OsStr::new("GNOME"))));
        assert!(!show_window_controls_with(
            Some(OsStr::new("/run/user/1000/niri.sock")),
            None
        ));
        assert!(!show_window_controls_with(
            None,
            Some(OsStr::new("niri:wlroots"))
        ));
        assert!(!show_window_controls_with(None, Some(OsStr::new("Niri"))));
    }
}
