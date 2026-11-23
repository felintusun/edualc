//! edualc's daemon (`edualcd`): runs the agents' sessions for the windows and reads their files.

// The window's modules, under the names this crate's modules know them by.
use edualc_core::*;

mod acp;
mod daemon;
pub mod demo;
mod driver;
mod files;
mod keyring;
pub mod net;
mod snapshots;
mod spawn;
mod usage;

// What `edualcd` starts as: the daemon (with its network side, [`net`], if it is given one), or
// an ACP session's adapter or its messaging tool. The image decoder's helper is the window's
// crate's (`edualc_core::decode`).
pub use acp::{ADAPTER_FLAG, TOOL_FLAG, run_adapter, run_tool};
pub use daemon::{Line, Options, run};
