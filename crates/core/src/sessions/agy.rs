//! agy's sessions, as its brain folder keeps them.

use std::path::Path;

use crate::ids::SessionId;

/// Returns whether `transcript` is in agy's brain folder.
#[cfg(not(target_arch = "wasm32"))]
fn is_agy_brain_path(transcript: &Path) -> bool {
    crate::external::agy::brain().is_some_and(|brain| transcript.starts_with(&brain))
}

#[cfg(target_arch = "wasm32")]
fn is_agy_brain_path(_transcript: &Path) -> bool {
    false
}

/// Returns whether agy keeps the steps of a session `id` in its brain folder.
#[cfg(not(target_arch = "wasm32"))]
pub fn is_agy_brain_session(id: &SessionId) -> bool {
    crate::external::agy::brain()
        .is_some_and(|brain| crate::external::agy::steps(&brain, id).is_file())
}

#[cfg(target_arch = "wasm32")]
pub fn is_agy_brain_session(_id: &SessionId) -> bool {
    false
}

/// Returns the program a session saved at `transcript` runs, told by where the file is.
pub fn backend_of(transcript: &Path) -> crate::provider::Backend {
    if is_agy_brain_path(transcript) {
        crate::provider::Backend::Agy
    } else {
        crate::provider::Backend::Claude
    }
}
