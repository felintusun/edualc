//! edualcd's network side: windows on other machines reach the daemon over TLS at `/api/line`,
//! and their asks of this machine's desktop are refused.

mod line;
mod listener;
pub mod state;

pub use line::{Access, router};
pub use listener::{serve, serve_plain};

/// What the daemon's network side is told: where to listen, with what TLS, and the token a line
/// must show.
pub struct Config {
    pub listen: std::net::SocketAddr,
    pub tls: rustls::ServerConfig,
    pub token: crate::ipc::Token,
    /// What else is served over the same connections: a page and its files.
    pub page: Option<axum::Router>,
}

/// Where it listens; neither the TLS nor the token.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("listen", &self.listen)
            .finish_non_exhaustive()
    }
}
