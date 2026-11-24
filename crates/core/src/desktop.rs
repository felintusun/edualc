//! The desktop around the window: notifications, links, and other programs' windows.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::external::niri;

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    strum::Display,
    strum::EnumIter,
    serde::Serialize,
    serde::Deserialize,
)]
#[strum(serialize_all = "kebab-case")]
pub enum Program {
    FcList,
    FcMatch,
    Gsettings,
}

/// Whether a link Claude wrote may open: a web page's, or mail. A file or another app's scheme
/// could run what it wrote.
#[must_use]
pub fn is_web_link(link: &str) -> bool {
    ["https://", "http://", "mailto:"].iter().any(|scheme| {
        link.get(..scheme.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(scheme))
    })
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("couldn't run {program}: {source}")]
    Run {
        program: Program,
        #[source]
        source: std::io::Error,
    },
    #[error("{program} didn't end within {}s", RUN_LIMIT.as_secs())]
    TimedOut { program: Program },
    #[error("{program} failed: {stderr}")]
    Failed { program: Program, stderr: String },
    #[error(transparent)]
    Niri(#[from] niri::Error),
    #[error(
        "no window holds process {0}; it may run in the background or in a terminal multiplexer"
    )]
    NoWindow(u32),
}

/// How long a program edualc runs gets to end. Each is quick; one that takes longer has hung,
/// and we kill it and what it started rather than hold its thread forever.
pub(crate) const RUN_LIMIT: Duration = Duration::from_secs(30);

/// How long what a program printed is waited for once it ended: a process it started may hold
/// its output open, and what came by then is what it printed.
const OUTPUT_GRACE: Duration = Duration::from_millis(500);

/// Runs a program to its end, for at most [`RUN_LIMIT`]; what it printed, if it succeeded.
pub fn run(program: Program, args: &[&str]) -> Result<Vec<u8>, Error> {
    run_within(program, program.to_string().as_ref(), args, RUN_LIMIT)
}

/// Runs `name`, as `program`, for at most `limit`.
fn run_within(
    program: Program,
    name: &std::ffi::OsStr,
    args: &[&str],
    limit: Duration,
) -> Result<Vec<u8>, Error> {
    let mut command = Command::new(name);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // A group of its own, so that what it starts is killed with it.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command
        .spawn()
        .map_err(|source| Error::Run { program, source })?;
    let pid = child.id();
    // Read beside it, so one printing much isn't stopped by a full pipe; and waited for beside
    // it, so that one the kernel can't kill at once (stuck on a file system) holds up no one.
    let stdout = child.stdout.take().map(read_beside);
    let stderr = child.stderr.take().map(read_beside);
    let (ended, ending) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        if ended.send(child.wait()).is_err() {
            // Given up on: it is reaped as it ends.
        }
    });
    let status = match ending.recv_timeout(limit) {
        Ok(Ok(status)) => status,
        Ok(Err(source)) => return Err(Error::Run { program, source }),
        Err(_) => {
            kill_group(pid);
            return Err(Error::TimedOut { program });
        }
    };
    let grace = Instant::now() + OUTPUT_GRACE;
    let stdout = printed(stdout, grace);
    if status.success() {
        Ok(stdout)
    } else {
        let stderr = printed(stderr, grace);
        Err(Error::Failed {
            program,
            stderr: String::from_utf8_lossy(&stderr).trim().to_owned(),
        })
    }
}

/// Kills the process group `pid` leads.
fn kill_group(pid: u32) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        use rustix::process::{Pid, Signal, kill_process_group};
        let Some(group) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
            return;
        };
        if let Err(error) = kill_process_group(group, Signal::KILL) {
            tracing::debug!(%error, pid, "couldn't kill a program run");
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        // No program runs in a browser.
        let _: u32 = pid;
    }
}

/// What a program printed to one of its pipes by its end, or by `grace` if a process it started
/// holds the pipe open.
fn printed(reading: Option<mpsc::Receiver<Vec<u8>>>, grace: Instant) -> Vec<u8> {
    let mut bytes = Vec::new();
    let Some(reading) = reading else {
        return bytes;
    };
    loop {
        let left = grace.saturating_duration_since(Instant::now());
        match reading.recv_timeout(left) {
            Ok(read) => bytes.extend(read),
            Err(mpsc::RecvTimeoutError::Disconnected | mpsc::RecvTimeoutError::Timeout) => {
                return bytes;
            }
        }
    }
}

/// Reads what a program prints to one of its pipes, on a thread of its own, piece by piece as
/// it comes, until the pipe closes.
fn read_beside(mut pipe: impl Read + Send + 'static) -> mpsc::Receiver<Vec<u8>> {
    let (read, reading) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = vec![0; 64 * 1024];
        loop {
            match pipe.read(&mut bytes) {
                Ok(0) => return,
                Ok(count) => {
                    let piece = bytes.get(..count).unwrap_or_default().to_vec();
                    if read.send(piece).is_err() {
                        // Nobody waits for it any more.
                        return;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => {
                    tracing::debug!(%error, "a program's output wasn't read to its end");
                    return;
                }
            }
        }
    });
    reading
}

/// What the desktop knows edualc by: the window's Wayland app id, the name of its desktop entry
/// (`assets/io.github.felintusun.edualc.desktop`) and of its icon, which the entry's `Icon`
/// names, and its AppStream id.
pub const APP_ID: &str = "io.github.felintusun.edualc";
/// What people see edualc called, in a notification's header.
pub const APP_NAME: &str = "edualc";

/// Focuses the window that process `pid` runs in: the one whose process is `pid` or one of
/// its ancestors (a terminal, an editor). Only niri is asked.
#[cfg(not(target_arch = "wasm32"))]
pub fn focus_window_of(pid: u32) -> Result<(), Error> {
    let windows = match niri::ask(&niri::Request::Windows)? {
        niri::Response::Windows(windows) => windows,
        niri::Response::Handled => Vec::new(),
    };
    let window = ancestors(pid)
        .into_iter()
        .find_map(|ancestor| {
            windows
                .iter()
                .find(|window| window.pid.and_then(|pid| u32::try_from(pid).ok()) == Some(ancestor))
        })
        .ok_or(Error::NoWindow(pid))?;
    let focus = niri::Request::Action(niri::Action::FocusWindow { id: window.id });
    niri::ask(&focus).map(drop).map_err(Error::Niri)
}

/// `pid` and its parents, nearest first, up to (not including) init.
fn ancestors(pid: u32) -> Vec<u32> {
    let mut chain = vec![pid];
    let mut current = pid;
    // Deep enough for any real chain; a cycle in a racing /proc can't loop forever.
    for _ in 0..64 {
        let Some(parent) = parent_of(current).filter(|&parent| parent > 1) else {
            break;
        };
        chain.push(parent);
        current = parent;
    }
    chain
}

fn parent_of(pid: u32) -> Option<u32> {
    crate::process::stat(pid).map(|stat| stat.parent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_has_ancestors() {
        let chain = ancestors(std::process::id());
        assert_eq!(chain.first(), Some(&std::process::id()));
        assert!(chain.len() > 1, "the test runner has a parent");
    }

    #[test]
    fn a_program_run_gives_what_it_printed() {
        let said = run_within(Program::FcList, "echo".as_ref(), &["hello"], RUN_LIMIT);
        assert_eq!(said.ok(), Some(b"hello\n".to_vec()));
        let failed = run_within(
            Program::FcList,
            "sh".as_ref(),
            &["-c", "echo no >&2; exit 3"],
            RUN_LIMIT,
        );
        assert!(matches!(failed, Err(Error::Failed { stderr, .. }) if stderr == "no"));
    }

    #[test]
    fn a_program_that_hangs_is_given_up() {
        let limit = Duration::from_millis(300);
        let started = Instant::now();
        let hung = run_within(
            Program::FcList,
            "sh".as_ref(),
            &["-c", "sleep 30 & wait"],
            limit,
        );
        assert!(matches!(
            hung,
            Err(Error::TimedOut {
                program: Program::FcList
            })
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
        let started = Instant::now();
        let left_open = run_within(
            Program::FcList,
            "sh".as_ref(),
            &["-c", "echo done; sleep 30 &"],
            limit,
        );
        assert_eq!(left_open.ok(), Some(b"done\n".to_vec()));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
