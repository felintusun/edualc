//! A plan's quotas, as an agent tells them outside a session.

use std::path::PathBuf;
use std::process::Stdio;

use tokio::process::Command;

use crate::external::agy;

use super::agents::{Agent, on_path};

/// How long the agent gets to say its plan's quotas.
const QUOTAS_WAIT: std::time::Duration = std::time::Duration::from_secs(15);

/// Why the plan's quotas couldn't be read.
#[derive(Debug, thiserror::Error)]
pub(crate) enum QuotaError {
    #[error("{0} has no quotas to say")]
    NoQuotas(Agent),
    #[error("{0} isn't on PATH")]
    NotOnPath(Agent),
    #[error("couldn't run it: {0}")]
    Run(#[source] std::io::Error),
    #[error("it didn't answer in time")]
    TimedOut,
    #[error("it failed ({0})")]
    Failed(std::process::ExitStatus),
    #[error("its answer can't be read: {0}")]
    Read(#[source] serde_json::Error),
}

/// Asks agy for its plan's quotas outside a session.
pub(crate) async fn agy_quotas(cwd: PathBuf) -> Result<agy::Quotas, QuotaError> {
    quotas(Agent::Agy, cwd).await
}

/// Asks `agent` for its plan's quotas in `cwd`, where agy reads its settings.
pub(super) async fn quotas(agent: Agent, cwd: PathBuf) -> Result<agy::Quotas, QuotaError> {
    let arguments = agent
        .quotas_arguments()
        .ok_or(QuotaError::NoQuotas(agent))?;
    let program = on_path(agent.program()).ok_or(QuotaError::NotOnPath(agent))?;
    let run = Command::new(program)
        .args(arguments)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(QUOTAS_WAIT, run)
        .await
        .map_err(|_late| QuotaError::TimedOut)?
        .map_err(QuotaError::Run)?;
    if !output.status.success() {
        return Err(QuotaError::Failed(output.status));
    }
    serde_json::from_slice::<agy::UsageAnswer>(&output.stdout)
        .map(|answer| answer.command.data)
        .map_err(QuotaError::Read)
}
