//! The helper's confinement on macOS. A process confines itself there only through
//! `sandbox_init`, which takes `unsafe`, so the helper is started inside a sandbox instead, by
//! `sandbox-exec`: its profile denies everything but starting the helper, reading what the
//! dynamic linker loads it with, and the descriptors it was handed. The helper then checks it
//! is confined before it reads a byte, as one started by hand isn't.

use std::path::Path;

use super::{ConfineError, Job, bounded, only_standard_streams};

/// Where the helper lists the descriptors it holds.
pub(super) const OPEN_DESCRIPTORS: &str = "/dev/fd";

/// What starts a program in a sandbox: part of macOS, at this place on every Mac.
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// A file every Mac has, which the sandbox keeps out of reach: the helper won't go on if it can
/// read it.
const OUT_OF_REACH: &str = "/etc/hosts";

/// The helper: this program, asked to do `job`, in a sandbox.
pub(super) fn command(job: Job) -> std::io::Result<tokio::process::Command> {
    // The profile names the program by the path the kernel sees, links resolved.
    let mut command = sandboxed(&std::env::current_exe()?.canonicalize()?)?;
    command.arg(job.as_ref());
    Ok(tokio::process::Command::from(command))
}

/// `program` started in the sandbox, its own arguments still to come.
fn sandboxed(program: &Path) -> std::io::Result<std::process::Command> {
    let mut command = std::process::Command::new(SANDBOX_EXEC);
    command.arg("-p").arg(profile(program)?).arg(program);
    Ok(command)
}

/// The sandbox's profile: everything denied but starting `program` and what it takes to run
/// it. The dynamic linker reads the root folder, the system's libraries and its shared cache,
/// and Rust's runtime the page size, to guard its stack;
/// a file's metadata it may read anywhere, which tells the helper no more than that it is
/// there, with nowhere to tell it but its answer.
fn profile(program: &Path) -> std::io::Result<String> {
    let program = program
        .to_str()
        .filter(|path| !path.contains(['"', '\\']))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{} can't be named in a sandbox profile", program.display()),
            )
        })?;
    Ok(format!(
        r#"(version 1)
(deny default)
(allow process-exec (literal "{program}"))
(allow file-read-metadata)
(allow file-read*
    (literal "/")
    (literal "{program}")
    (subpath "/usr/lib")
    (subpath "/System")
    (subpath "/private/var/db/dyld")
    (subpath "{OPEN_DESCRIPTORS}"))
(allow sysctl-read (sysctl-name "hw.pagesize" "hw.pagesize_compat"))"#
    ))
}

/// Checks the helper is in its sandbox and holds nothing but its standard streams, then bounds
/// it. macOS refuses a bound on memory (`RLIMIT_AS`, `RLIMIT_DATA`), so what bounds it there is
/// the decoder's own limit on what it allocates, and the time it is given before it is killed.
pub(super) fn confine() -> Result<(), ConfineError> {
    if std::fs::File::open(OUT_OF_REACH).is_ok() {
        return Err(ConfineError::Unconfined);
    }
    only_standard_streams()?;
    Ok(bounded()?)
}

#[cfg(test)]
mod tests {
    use std::process::Stdio;

    use super::*;

    /// The sandbox holds: started as the helper is, a process can't read a file, nor go on
    /// unconfined when started without it. The test starts itself again as that process.
    #[test]
    fn a_sandboxed_helper_reaches_no_file() {
        const CHILD: &str = "EDUALC_CONFINED_CHILD";
        if std::env::var_os(CHILD).is_some() {
            // Why, for whoever runs this by hand: the parent sees only the exit.
            let confined = confine().inspect_err(|error| eprintln!("{error}")).is_ok();
            let home = std::env::var_os("HOME").unwrap_or_default();
            let opened =
                std::fs::read_dir(home).is_ok() || std::fs::File::open("/etc/passwd").is_ok();
            std::process::exit(match (confined, opened) {
                (true, false) => 0,
                (false, _) => 3,
                (true, true) => 4,
            });
        }
        let test = std::env::current_exe().expect("this test");
        let args = [
            "--exact",
            "decode::helper::macos::tests::a_sandboxed_helper_reaches_no_file",
            "--test-threads=1",
        ];
        let run = |in_sandbox: bool| {
            let mut command = if in_sandbox {
                sandboxed(&test.canonicalize().expect("a path")).expect("a profile")
            } else {
                std::process::Command::new(&test)
            };
            command
                .args(args)
                .env(CHILD, "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .expect("ran")
                .code()
        };
        assert_eq!(run(true), Some(0), "sandboxed");
        assert_eq!(run(false), Some(3), "started without the sandbox");
    }
}
