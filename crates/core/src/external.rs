//! Formats other programs write, as types. They grow with each release, so unlike edualc's own
//! formats they take keys they don't know.

#[cfg(not(target_arch = "wasm32"))]
pub mod acp;
#[cfg(not(target_arch = "wasm32"))]
pub mod agy;
pub mod claude_files;
#[cfg(not(target_arch = "wasm32"))]
pub mod mcp;
pub mod niri;
pub mod tool_input;
pub mod wire;
