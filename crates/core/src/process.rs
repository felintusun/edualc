//! What the system says about a process: who started it, when, and what it runs. Linux tells
//! it in `/proc`; macOS through `libproc`.

use std::path::{Path, PathBuf};

/// From `/proc/<pid>/stat`, or macOS's `proc_bsdinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stat {
    pub parent: u32,
    /// When it started, as Claude Code writes it in its session records: with the pid, it names
    /// one process, as pids are reused.
    pub started: String,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn stat(pid: u32) -> Option<Stat> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command (field 2) may hold spaces and ends in `)`; field 4 is the parent, field 22
    // the start time, in ticks after boot.
    let (_, fields) = stat.rsplit_once(')')?;
    let mut fields = fields.split_whitespace();
    let parent = fields.nth(1)?.parse().ok()?;
    let started = fields.nth(17)?.to_owned();
    Some(Stat { parent, started })
}

#[cfg(target_os = "macos")]
pub fn stat(pid: u32) -> Option<Stat> {
    use libproc::bsd_info::BSDInfo;
    let info = libproc::proc_pid::pidinfo::<BSDInfo>(i32::try_from(pid).ok()?, 0).ok()?;
    let started = jiff::Timestamp::from_second(i64::try_from(info.pbi_start_tvsec).ok()?).ok()?;
    Some(Stat {
        parent: info.pbi_ppid,
        started: lstart(started),
    })
}

/// A start time as Claude Code writes it on macOS: as `ps -o lstart` shows it, in UTC.
#[cfg(target_os = "macos")]
fn lstart(started: jiff::Timestamp) -> String {
    started.strftime("%a %b %e %H:%M:%S %Y").to_string()
}

/// Which machine and pid namespace this process is in, as Claude Code writes it:
/// `linux:<boot id>:pid:[<namespace>]`.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn pid_domain() -> Option<String> {
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let namespace = std::fs::read_link("/proc/self/ns/pid").ok()?;
    Some(format!(
        "linux:{}:{}",
        boot.trim().replace('-', ""),
        namespace.to_string_lossy()
    ))
}

/// Which system this process is in, as Claude Code writes it on macOS: `darwin`, no more.
#[cfg(target_os = "macos")]
pub fn pid_domain() -> Option<String> {
    Some("darwin".to_owned())
}

/// Whether process `pid` runs, and is the one that started at `started`.
pub fn is_running(pid: u32, started: &str) -> bool {
    stat(pid).is_some_and(|stat| stat.started == started)
}

/// The program process `pid` runs.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn program(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

/// The program process `pid` runs.
#[cfg(target_os = "macos")]
pub fn program(pid: u32) -> Option<PathBuf> {
    libproc::proc_pid::pidpath(i32::try_from(pid).ok()?)
        .ok()
        .map(PathBuf::from)
}

/// Whether process `pid` runs Claude Code: the native `claude`, or Node running the npm
/// package.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn runs_claude(pid: u32) -> bool {
    let named = std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .is_ok_and(|command| command.trim_end() == "claude");
    named
        || std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmdline| {
            cmdline
                .split(|&byte| byte == 0)
                .take(2)
                .map(|argument| Path::new(std::str::from_utf8(argument).unwrap_or("")))
                .any(is_claude)
        })
}

/// Whether process `pid` runs Claude Code: the native `claude`, by its program. Node running
/// the npm package isn't told apart here, as macOS gives a process's arguments only in a raw
/// form no safe interface reads.
#[cfg(target_os = "macos")]
pub fn runs_claude(pid: u32) -> bool {
    program(pid).is_some_and(|program| is_claude(&program))
}

/// Whether a program, or the script a program runs, is Claude Code's.
fn is_claude(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with("claude"))
        || path
            .components()
            .any(|part| part.as_os_str() == "claude-code" || part.as_os_str() == "claude")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_reads_back() {
        let own = stat(std::process::id()).expect("our own stat");
        assert!(is_running(std::process::id(), &own.started));
        assert!(!is_running(std::process::id(), "0"));
        assert!(!runs_claude(std::process::id()));
        assert_eq!(
            program(std::process::id()).and_then(|path| path.canonicalize().ok()),
            std::env::current_exe()
                .and_then(|path| path.canonicalize())
                .ok()
        );
    }

    /// Claude Code's `procStart` on macOS: `ps -o lstart` in UTC, the day padded with a space.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_start_reads_as_claude_code_writes_it() {
        let started = jiff::Timestamp::from_second(1_000_000_000).expect("a time");
        assert_eq!(lstart(started), "Sun Sep  9 01:46:40 2001");
    }
}
