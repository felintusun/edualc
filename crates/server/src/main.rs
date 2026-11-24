//! `edualcd`: edualc's daemon, with `--listen` also the server of the page; built with the
//! `mobile` feature, the Android app.

#[cfg(feature = "server")]
mod serve;

#[cfg(feature = "mobile")]
mod android;
mod line;
mod web;

#[cfg(feature = "server")]
fn main() -> std::process::ExitCode {
    serve::main()
}

#[cfg(not(any(feature = "server", feature = "mobile")))]
fn main() {
    dioxus::launch(web::App);
}

/// Starts the Android app: the same window, with a native line (`android`).
#[cfg(all(feature = "mobile", not(feature = "server")))]
fn main() {
    dioxus::launch(android::App);
}
