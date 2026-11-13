//! The Anthropic API key Claude Code sessions may sign in with: stored in and removed from the
//! desktop's keyring through the daemon, which alone reads it back (see `keyring`). The window
//! only knows whether one is kept.

use super::{Acting, ApiKeyKept, App};
use crate::ipc::{Act, ActFailure, ApiKey, ToDaemon};
use crate::link;

impl App {
    /// Asks the daemon whether the keyring keeps a key; `api_key_stored` takes the answer.
    pub(super) fn check_api_key(&mut self) {
        self.last_act = self.last_act.next();
        let check = ToDaemon::Act {
            request: self.last_act,
            act: Act::CheckApiKey,
        };
        if link::send(&self.link, check).is_err() {
            self.api_key_stored(Ok(false));
        }
    }

    pub(super) fn api_key_stored(&mut self, stored: Result<bool, ActFailure>) {
        self.api_key = match stored {
            Ok(true) => ApiKeyKept::Kept,
            Ok(false) => ApiKeyKept::Absent,
            Err(failure) => {
                tracing::warn!(%failure, "couldn't tell whether an API key is kept");
                self.api_key_note = failure.to_string();
                ApiKeyKept::Absent
            }
        };
        if let Some(id) = self.resume_after_key_check.take() {
            self.open_session(id);
        }
    }

    /// Keeps the pasted key in the keyring, through the daemon.
    pub(super) fn store_api_key(&mut self, text: &str) {
        let Some(key) = ApiKey::parse(text) else {
            self.api_key_note =
                "That isn't an API key: paste the key alone, without spaces in it.".to_owned();
            return;
        };
        self.ask_desktop(Act::StoreApiKey(key), Acting::ApiKey);
    }

    /// The keyring stored or removed the key, or failed to: the settings say so.
    pub(super) fn api_key_changed(&mut self, result: Result<(), ActFailure>) {
        self.api_key_note = match result {
            Ok(()) => String::new(),
            Err(failure) => failure.to_string(),
        };
        self.check_api_key();
    }
}
