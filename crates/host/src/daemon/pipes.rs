//! A session's process's pipes: its input, written on a task of its own, and its output, read
//! line by line and passed on to the window.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::ChildStdin;
use tokio::sync::mpsc;

use crate::ipc::{self, FromDaemon, Generation, LineSplitter, Stream};

use super::QUEUE;

/// The most bytes queued for a process's input and not written yet: a few of the longest
/// lines a window may send.
pub(super) const QUEUED_BYTES: usize = 4 * crate::ipc::MAX_LINE;

/// A process's input, written on a task of its own, line by line as they are queued. Dropped,
/// the input closes once the lines queued are written; one the process doesn't read holds it
/// open only until the process is killed, a grace after its input closed.
pub(super) struct Input {
    pub(super) lines: mpsc::UnboundedSender<String>,
    /// The length of each line written, as it is.
    pub(super) written: mpsc::UnboundedReceiver<usize>,
    pub(super) queued_lines: usize,
    pub(super) queued_bytes: usize,
}

/// Why a line wasn't queued for a process's input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Unqueued {
    Closed,
    /// So much is queued already that the process can't be reading.
    Full,
}

impl Input {
    pub(super) fn start(mut stdin: ChildStdin) -> Self {
        let (lines, mut writing) = mpsc::unbounded_channel::<String>();
        let (wrote, written) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(line) = writing.recv().await {
                if let Err(error) = write_line(&mut stdin, &line).await {
                    tracing::debug!(%error, "claude's input closed");
                    return;
                }
                if wrote.send(line.len()).is_err() {
                    // The session let go of its input: what is queued is still written.
                }
            }
        });
        Self {
            lines,
            written,
            queued_lines: 0,
            queued_bytes: 0,
        }
    }

    /// Queues `line`, unless the process has left too much unread.
    pub(super) fn queue(&mut self, line: String) -> Result<(), Unqueued> {
        while let Ok(bytes) = self.written.try_recv() {
            self.queued_lines = self.queued_lines.saturating_sub(1);
            self.queued_bytes = self.queued_bytes.saturating_sub(bytes);
        }
        let bytes = line.len();
        if self.queued_lines >= QUEUE || self.queued_bytes.saturating_add(bytes) > QUEUED_BYTES {
            return Err(Unqueued::Full);
        }
        if self.lines.send(line).is_err() {
            return Err(Unqueued::Closed);
        }
        self.queued_lines += 1;
        self.queued_bytes += bytes;
        Ok(())
    }
}

pub(super) async fn write_line(stdin: &mut ChildStdin, line: &str) -> std::io::Result<()> {
    stdin.write_all(line.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await
}

/// A process's output, line by line (split sans-io), to its session.
pub(super) async fn pump(
    mut output: impl tokio::io::AsyncRead + Unpin,
    stream: Stream,
    lines: mpsc::Sender<(Stream, String)>,
) {
    let mut splitter = LineSplitter::default();
    let mut bytes = vec![0u8; 64 * 1024];
    loop {
        let read = match output.read(&mut bytes).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let Some(read) = bytes.get(..read) else {
            break;
        };
        for line in splitter.push(read) {
            if lines.send((stream, line)).await.is_err() {
                return;
            }
        }
    }
    if let Some(line) = splitter.finish()
        && lines.send((stream, line)).await.is_err()
    {
        tracing::debug!("a session gone");
    }
}

/// A line to a window, in pieces that fit messages.
pub(super) async fn send_line(
    outbox: &mpsc::Sender<FromDaemon>,
    key: Generation,
    stream: Stream,
    line: &str,
) {
    let mut pieces = ipc::chunks(line).peekable();
    while let Some(text) = pieces.next() {
        let last = pieces.peek().is_none();
        let piece = FromDaemon::Output {
            key,
            stream,
            text: text.to_owned(),
            last,
        };
        if outbox.send(piece).await.is_err() {
            return;
        }
    }
}
