//! The desktop the native window is on: links, notifications, the clipboard's image, the folder
//! picker and other programs' windows.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use tokio::io::AsyncReadExt;

use crate::app::Event;
use crate::decode::{self, Refusal};
use crate::desktop;
use crate::ipc::{ActId, PasteFailure, Pasted};
use crate::present::Seat;

/// How long the clipboard's owner gets to hand over what it holds.
const CLIPBOARD: Duration = Duration::from_secs(5);
/// The most of the clipboard's list of types we read: a few dozen names.
const MAX_CLIPBOARD_TYPES_BYTES: usize = 64 * 1024;

/// Returns whether other programs' windows can be found from here: only niri is asked.
pub(crate) fn finds_windows() -> bool {
    std::env::var_os("NIRI_SOCKET").is_some()
}

/// Does what the window asks of its desktop, one thing at a time, on a thread of its own.
#[derive(Debug)]
pub(crate) struct Desk {
    orders: mpsc::Sender<Seat>,
}

impl Desk {
    /// Starts the thread; its answers come to the app as `events`.
    pub(crate) fn start(events: async_channel::Sender<Event>) -> Self {
        let (orders, taken) = mpsc::channel();
        std::thread::spawn(move || serve(&taken, &events));
        Self { orders }
    }

    pub(crate) fn ask(&self, seat: Seat) {
        if self.orders.send(seat).is_err() {
            tracing::warn!("the window's desktop thread is gone");
        }
    }
}

fn serve(taken: &mpsc::Receiver<Seat>, events: &async_channel::Sender<Event>) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::warn!(%error, "the window does nothing on its desktop: no runtime");
            return;
        }
    };
    while let Ok(seat) = taken.recv() {
        if let Some(event) = runtime.block_on(done(seat))
            && events.send_blocking(event).is_err()
        {
            return;
        }
    }
}

/// Does `seat`; returns what the app is told of it.
async fn done(seat: Seat) -> Option<Event> {
    match seat {
        Seat::OpenUrl(link) => open_url(&link).await.err().map(Event::SeatFailed),
        Seat::Notify { summary, body } => {
            // Not told in the conversation: a desktop without a notification service would
            // put an error there for each.
            if let Err(error) = notify(&summary, &body).await {
                tracing::warn!(%error, "couldn't notify");
            }
            None
        }
        Seat::PasteImage => Some(Event::Pasted {
            request: ActId::default(),
            pasted: paste_image().await,
        }),
        Seat::PickFolder(request) => Some(Event::Picked {
            request,
            folder: pick_folder().await,
        }),
        Seat::FocusWindowOf { pid } => desktop::focus_window_of(pid)
            .err()
            .map(|error| Event::SeatFailed(error.to_string())),
    }
}

/// Opens a web page or mail through the desktop's portal.
///
/// # Errors
/// What to tell: the link isn't a web page's or mail, or the portal refused.
async fn open_url(link: &str) -> Result<(), String> {
    // Claude writes the links: a file or another app's scheme could run what it wrote.
    if !desktop::is_web_link(link) {
        return Err(format!(
            "not opened: {link}\nOnly web links open from here; copy it to open it yourself."
        ));
    }
    let uri = ashpd::Uri::parse(link).map_err(|error| format!("couldn't open {link}: {error}"))?;
    ashpd::desktop::open_uri::OpenFileRequest::default()
        .send_uri(&uri)
        .await
        .map(drop)
        .map_err(|error| format!("couldn't open {link}: {error}"))
}

/// Shows a notification through the desktop's notification service.
async fn notify(summary: &str, body: &str) -> zbus::Result<()> {
    // The body may hold markup for servers that take it; we show other sessions' words as text.
    let mut escaped = String::new();
    pulldown_cmark_escape::escape_html(&mut escaped, body)
        .map_err(|_error| zbus::Error::Failure("the body couldn't be escaped".to_owned()))?;
    let hints = HashMap::from([("desktop-entry", zbus::Variant::from(desktop::APP_ID))]);
    let actions: &[&str] = &[];
    // org.freedesktop.Notifications.Notify: app name, no notification to replace, icon,
    // summary, body, no actions, hints, the server's own timeout.
    let arguments = (
        desktop::APP_NAME,
        0_u32,
        desktop::APP_ID,
        summary,
        escaped.as_str(),
        actions,
        hints,
        -1_i32,
    );
    zbus::Connection::session()
        .await?
        .call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &arguments,
        )
        .await
        .map(drop)
}

/// Returns the folder picked in the desktop's folder picker, if one was.
async fn pick_folder() -> Option<PathBuf> {
    rfd::AsyncFileDialog::new()
        .set_title("Open a folder")
        .pick_folder()
        .await
        .map(|picked| picked.path().to_path_buf())
}

/// Reads the clipboard's image with `wl-paste` and has the confined helper decode it (see
/// `decode`).
async fn paste_image() -> Pasted {
    let listed = clipboard(&["--list-types"], MAX_CLIPBOARD_TYPES_BYTES).await?;
    let format = String::from_utf8_lossy(&listed)
        .lines()
        .find_map(|line| line.trim().parse::<decode::Format>().ok())
        .ok_or(PasteFailure::NoImage)?;
    let limit = decode::MAX_INPUT_BYTES + 1;
    let bytes = clipboard(&["--no-newline", "--type", format.as_ref()], limit).await?;
    if bytes.len() > decode::MAX_INPUT_BYTES {
        return Err(PasteFailure::Refused(Refusal::TooManyBytes));
    }
    decode::decode(&bytes).await.map_err(|error| match error {
        decode::Error::Refused(refusal) => PasteFailure::Refused(refusal),
        error @ (decode::Error::Start(_)
        | decode::Error::NoPipes
        | decode::Error::Hand(_)
        | decode::Error::Read(_)
        | decode::Error::Slow
        | decode::Error::Failed(_)
        | decode::Error::TooMuch
        | decode::Error::Unreadable(_)
        | decode::Error::Malformed) => PasteFailure::Decoder(error.to_string()),
    })
}

/// Runs `wl-paste` with `args` and reads what it gives, up to `limit` bytes.
async fn clipboard(args: &[&str], limit: usize) -> Result<Vec<u8>, PasteFailure> {
    let failed = |error: std::io::Error| PasteFailure::Clipboard(error.to_string());
    let mut child = tokio::process::Command::new("wl-paste")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(failed)?;
    let Some(output) = child.stdout.take() else {
        return Err(PasteFailure::Clipboard("wl-paste has no output".to_owned()));
    };
    let reading = async {
        let mut bytes = Vec::new();
        let cap = u64::try_from(limit).unwrap_or(u64::MAX);
        output
            .take(cap)
            .read_to_end(&mut bytes)
            .await
            .map_err(failed)?;
        if bytes.len() >= limit {
            // The caller refuses it; we don't wait for the rest (the child is killed as it is
            // dropped).
            return Ok(bytes);
        }
        let status = child.wait().await.map_err(failed)?;
        // It fails when the clipboard is empty, or holds nothing of the type asked.
        if status.success() {
            Ok(bytes)
        } else {
            Err(PasteFailure::NoImage)
        }
    };
    tokio::time::timeout(CLIPBOARD, reading)
        .await
        .map_err(|_elapsed| {
            PasteFailure::Clipboard("the clipboard's owner didn't hand it over in time".to_owned())
        })?
}
