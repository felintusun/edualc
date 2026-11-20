//! The helper's confinement on Linux, and on Android, whose kernel it shares: the helper
//! confines itself as it starts, with Landlock and seccomp.

use std::collections::BTreeMap;

use landlock::{
    ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, Ruleset, RulesetAttr, Scope,
};
use rustix::process::{DumpableBehavior, Resource};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule,
};

use super::{ConfineError, Job, bounded, limit, only_standard_streams};

/// The newest Landlock this helper knows.
const ABI_KNOWN: ABI = ABI::V9;
/// The helper's address space: decoding's allocations and the program around them.
const MAX_ADDRESS_SPACE_BYTES: u64 = 1024 * 1024 * 1024;

/// Where the helper lists the descriptors it holds.
pub(super) const OPEN_DESCRIPTORS: &str = "/proc/self/fd";

/// The helper: this program, as it runs, asked to do `job`.
pub(super) fn command(job: Job) -> std::io::Result<tokio::process::Command> {
    let mut command = tokio::process::Command::new("/proc/self/exe");
    command.arg(job.as_ref());
    Ok(command)
}

/// Confines the helper, in an order where nothing set up later needs what was taken earlier:
/// closed to other processes, sure to hold nothing but its standard streams, then no file,
/// socket or signal (Landlock), bounded (rlimits), and last the system calls decoding makes,
/// with no memory made executable (seccomp).
pub(super) fn confine() -> Result<(), ConfineError> {
    rustix::process::set_dumpable_behavior(DumpableBehavior::NotDumpable)?;
    only_standard_streams()?;
    Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(ABI_KNOWN))?
        .handle_access(AccessNet::BindTcp | AccessNet::ConnectTcp)?
        .scope(Scope::AbstractUnixSocket | Scope::Signal)?
        .create()?
        .restrict_self()?;
    limit(Resource::As, MAX_ADDRESS_SPACE_BYTES)?;
    bounded()?;
    seccompiler::apply_filter(&filter()?)?;
    Ok(())
}

/// The system calls the helper makes once confined: reading its input, writing its answer,
/// memory (never executable), and ending. Any other kills it.
fn filter() -> Result<BpfProgram, ConfineError> {
    let on_fd = |fd: u64| -> Result<SeccompRule, ConfineError> {
        Ok(SeccompRule::new(vec![SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Eq,
            fd,
        )?])?)
    };
    let not_executable = || -> Result<SeccompRule, ConfineError> {
        let exec = u64::from(libc::PROT_EXEC.unsigned_abs());
        Ok(SeccompRule::new(vec![SeccompCondition::new(
            2,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::MaskedEq(exec),
            0,
        )?])?)
    };
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    rules.insert(libc::SYS_read, vec![on_fd(0)?]);
    rules.insert(libc::SYS_write, vec![on_fd(1)?, on_fd(2)?]);
    rules.insert(libc::SYS_mmap, vec![not_executable()?]);
    rules.insert(libc::SYS_mprotect, vec![not_executable()?]);
    for always in [
        libc::SYS_munmap,
        libc::SYS_mremap,
        libc::SYS_brk,
        libc::SYS_madvise,
        libc::SYS_futex,
        libc::SYS_rt_sigreturn,
        libc::SYS_rt_sigprocmask,
        libc::SYS_sigaltstack,
        libc::SYS_getrandom,
        libc::SYS_exit,
        libc::SYS_exit_group,
    ] {
        rules.insert(always, Vec::new());
    }
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::KillProcess,
        SeccompAction::Allow,
        std::env::consts::ARCH.try_into()?,
    )?;
    Ok(filter.try_into()?)
}

#[cfg(test)]
mod tests {
    use std::process::Stdio;

    use super::*;

    /// The confinement holds: confined as the helper is, a process reaching for a file is killed
    /// (the real filter, whose answer to anything not allowed is death). The test starts itself
    /// again as that process.
    #[test]
    fn a_confined_helper_is_killed_reaching_for_a_file() {
        use std::os::unix::process::ExitStatusExt;
        const CHILD: &str = "EDUALC_CONFINED_CHILD";
        if std::env::var_os(CHILD).is_some() {
            confine().expect("confined");
            let opened = std::fs::File::open("/etc/hostname");
            // Still here: the parent sees an exit, not the kill it waits for.
            std::process::exit(if opened.is_ok() { 3 } else { 4 });
        }
        let status = std::process::Command::new(std::env::current_exe().expect("this test"))
            .args([
                "--exact",
                "decode::helper::linux::tests::a_confined_helper_is_killed_reaching_for_a_file",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("ran");
        assert_eq!(status.signal(), Some(libc::SIGSYS), "{status}");
    }
}
