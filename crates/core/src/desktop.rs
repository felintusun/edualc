//! The desktop around the window: which links may open, and other programs' windows.

use crate::external::niri;

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
    #[error(transparent)]
    Niri(#[from] niri::Error),
    #[error(
        "no window holds process {0}; it may run in the background or in a terminal multiplexer"
    )]
    NoWindow(u32),
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
}
