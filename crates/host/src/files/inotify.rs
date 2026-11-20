//! What notices a change on Linux: inotify, read on a thread of its own, which ends as the
//! notifier is dropped (the pipe it waits on beside inotify closes).

use std::collections::HashSet;
use std::ffi::OsStr;
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use rustix::fs::inotify::{self, CreateFlags, WatchFlags};
use rustix::io::Errno;
use tokio::sync::mpsc;

use super::{Left, Noticed};

pub(super) type Error = Errno;
/// What the thread reading inotify sends at a time: all it had to tell.
pub(super) type Batch = HashSet<Noticed>;

/// One inotify instance, and the pipe whose closing ends the thread reading it.
#[derive(Debug)]
pub(super) struct Notifier {
    inotify: OwnedFd,
    _held: OwnedFd,
}

impl Notifier {
    /// Starts noticing: what is noticed goes to `noticing`, until the notifier is dropped.
    pub(super) fn start(noticing: mpsc::Sender<Batch>) -> Result<Self, Error> {
        // Not blocking, so the thread reads all there is each time `poll` wakes it.
        let inotify = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK)?;
        let reading = rustix::io::fcntl_dupfd_cloexec(&inotify, 0)?;
        let (ending, held) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?;
        std::thread::spawn(move || read(&reading, &ending, &noticing));
        Ok(Self {
            inotify,
            _held: held,
        })
    }

    /// Watches `folder`; the descriptor its changes come with. A folder watched already for
    /// another of the window's watches has the same descriptor.
    pub(super) fn add(&mut self, folder: &Path) -> Result<i32, Error> {
        inotify::add_watch(&self.inotify, folder, FOLDER_FLAGS | WatchFlags::MASK_ADD)
    }

    pub(super) fn remove(&mut self, descriptor: i32) -> Result<(), Error> {
        inotify::remove_watch(&self.inotify, descriptor)
    }

    /// What a batch says was noticed: on Linux, what inotify said. macOS's notifier reads a
    /// batch by the folders it watches.
    pub(super) fn noticed(&self, batch: Batch) -> HashSet<Noticed> {
        batch
    }
}

/// What a folder watched is watched for: anything written, made, removed or moved in it, and
/// its own moving away.
const FOLDER_FLAGS: WatchFlags = WatchFlags::CREATE
    .union(WatchFlags::MODIFY)
    .union(WatchFlags::DELETE)
    .union(WatchFlags::CLOSE_WRITE)
    .union(WatchFlags::MOVED_TO)
    .union(WatchFlags::MOVED_FROM)
    .union(WatchFlags::MOVE_SELF);

/// Sends on what inotify notices, all it has to tell at a time, until the actor ends: its end
/// of `ending` closes.
fn read(inotify: &OwnedFd, ending: &OwnedFd, noticing: &mpsc::Sender<Batch>) {
    use rustix::event::{PollFd, PollFlags, poll};
    let mut buffer = [MaybeUninit::<u8>::uninit(); 4096];
    loop {
        let mut ready = [
            PollFd::new(inotify, PollFlags::IN),
            PollFd::new(ending, PollFlags::IN),
        ];
        match poll(&mut ready, None) {
            Ok(_) | Err(Errno::INTR) => {}
            Err(error) => {
                tracing::warn!(%error, "stopped noticing changes for a window");
                return;
            }
        }
        let [noticed, ended] = ready;
        if !ended.revents().is_empty() {
            return;
        }
        if noticed.revents().is_empty() {
            continue;
        }
        let mut reader = inotify::Reader::new(inotify, &mut buffer);
        let mut batch = HashSet::new();
        loop {
            match reader.next() {
                Ok(event) => {
                    let flags = event.events();
                    let left = if flags.contains(inotify::ReadFlags::IGNORED) {
                        Some(Left::Gone)
                    } else if flags.contains(inotify::ReadFlags::MOVE_SELF) {
                        Some(Left::Moved)
                    } else {
                        None
                    };
                    batch.insert(Noticed {
                        descriptor: event.wd(),
                        name: event
                            .file_name()
                            .map(|name| OsStr::from_bytes(name.to_bytes()).to_owned()),
                        left,
                    });
                }
                Err(Errno::INTR) => {}
                // All read, for now.
                Err(Errno::WOULDBLOCK) => {
                    if !batch.is_empty() && noticing.blocking_send(batch).is_err() {
                        return;
                    }
                    break;
                }
                Err(error) => {
                    tracing::warn!(%error, "stopped noticing changes for a window");
                    return;
                }
            }
        }
    }
}
