//! The bars over the composer that ask something of the session (`ui/app.slint`): a session
//! another process runs, watched; a folder not trusted yet; signing in to Claude Code.

use dioxus::prelude::*;
use edualc_core::app::Action;

use crate::web::View;
use crate::web::widgets::{Button, Icon, IconButton, Lucide, take_focus};

/// What signs in to Claude Code, run in a terminal.
const SIGN_IN_COMMAND: &str = "claude auth login";

/// In place of the composer while a session another process runs is shown: keep watching it or
/// continue here as a copy.
#[component]
pub fn WatchBar() -> Element {
    let view = use_context::<View>();
    let session = view.session.read();
    rsx! {
        div { class: "dock-bar",
            div { class: "dock-bar-heading",
                Icon { icon: Lucide::Eye }
                div { class: "dock-bar-text secondary", "{session.watch_text}" }
            }
            div { class: "dock-bar-actions",
                if session.can_fork {
                    Button {
                        primary: true,
                        icon: Lucide::Copy,
                        onclick: move |_| view.act(Action::ContinueHere),
                        "Continue here as a copy"
                    }
                }
            }
        }
    }
}

/// Before Claude Code starts in a folder not trusted yet: what it would run there.
#[component]
pub fn TrustBar() -> Element {
    let view = use_context::<View>();
    let dialogs = use_context::<super::pages::DialogOpen>();
    let session = view.session.read();
    let runs = !session.trust_runs.is_empty();
    rsx! {
        div { class: "dock-bar warned",
            div { class: "dock-bar-heading",
                Icon { icon: Lucide::ShieldExclamationPoint, class: "warning" }
                div { class: "dock-bar-text",
                    "Do you trust the files in {session.project}? Claude Code runs its hooks and settings on start."
                }
            }
            // What it would run, as read from its settings and .mcp.json (plain text, as
            // written there); a long list scrolls.
            div { class: "dock-bar-note",
                if runs {
                    "Its settings run or set:"
                } else {
                    "Its settings run nothing and allow nothing more."
                }
            }
            if runs {
                div { class: "dock-bar-runs",
                    for (index, line) in session.trust_runs.iter().enumerate() {
                        div { key: "{index}", "{line}" }
                    }
                }
            }
            // As the terminal's dialog lets one go on without them; asked again at the next start.
            div { class: "dock-bar-note",
                "Without its settings, no MCP servers or claude.ai connectors run either, yours included. Trusting it also trusts it for Claude Code in the terminal."
            }
            div { class: "dock-bar-actions",
                // The dialog of folders, over the question, which stays if none is chosen.
                Button { onclick: move |_| dialogs.open_folder(view), "Open another folder" }
                // The focus comes here as the question shows, never to "Trust this folder": a
                // stray Enter starts Claude Code with none of the folder's settings.
                button {
                    class: "button",
                    onmounted: take_focus,
                    onclick: move |_| view.act(Action::StartWithoutFolderSettings),
                    "Start without its settings"
                }
                Button {
                    primary: true,
                    onclick: move |_| view.act(Action::TrustFolder),
                    "Trust this folder"
                }
            }
        }
    }
}

/// How to sign in: Claude Code behind a pipe can't, though it tells the user to run `/login`. It
/// reads the sign-in as it starts, so trying again starts it again.
#[component]
pub fn SignInBar() -> Element {
    let view = use_context::<View>();
    rsx! {
        div { class: "dock-bar warned",
            div { class: "dock-bar-heading",
                Icon { icon: Lucide::LogIn, class: "warning" }
                div { class: "dock-bar-text strong", "Sign in to Claude Code" }
            }
            div { class: "dock-bar-note",
                "Run this in a terminal, then try again."
            }
            div { class: "dock-bar-command",
                code { "{SIGN_IN_COMMAND}" }
                IconButton {
                    icon: Lucide::Copy,
                    label: "Copy the command".to_owned(),
                    onclick: move |_| view.act(Action::Copy(SIGN_IN_COMMAND.to_owned())),
                }
            }
            div { class: "dock-bar-actions",
                Button {
                    onclick: move |_| view.act(Action::DismissSignIn),
                    "Not now"
                }
                Button {
                    primary: true,
                    onclick: move |_| view.act(Action::TrySignInAgain),
                    "Try again"
                }
            }
        }
    }
}
