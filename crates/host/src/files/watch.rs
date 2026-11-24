//! A window's watches on the files it shows, with inotify.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rustix::fs::inotify::{self, CreateFlags, WatchFlags};
use rustix::io::Errno;
use tokio::sync::mpsc;

use crate::ipc::{FromDaemon, Target, Watched};
use crate::sessions::ClaudeHome;

use super::Outbox;
use super::read::resolve;

/// What a window's watches are to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Order {
    Watch(Watched, Target),
    Unwatch(Watched),
}

/// Keeps a window's watches in one inotify instance, named as the window named them, until the
/// window's orders end. A thread of its own reads what inotify notices; it ends with the actor,
/// when the pipe it waits on beside inotify closes.
pub async fn watch(home: ClaudeHome, mut orders: mpsc::Receiver<Order>, outbox: Outbox) {
    // Not blocking, so the thread reads all there is each time `poll` wakes it.
    let started = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK).and_then(|inotify| {
        let reading = rustix::io::fcntl_dupfd_cloexec(&inotify, 0)?;
        let (waits, _held) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?;
        Ok((inotify, reading, waits, _held))
    });
    // `_held` is the pipe's other end, closed as this returns: the thread wakes, and ends.
    let (inotify, reading, ending, _held) = match started {
        Ok(started) => started,
        Err(error) => {
            tracing::warn!(%error, "a window's watches go unnoticed");
            while orders.recv().await.is_some() {}
            return;
        }
    };
    let (noticing, mut noticed) = mpsc::channel(QUEUE);
    std::thread::spawn(move || read(&reading, &ending, &noticing));
    let mut watches = Watches::new(Inotify(inotify));
    let mut retrying = tokio::time::interval(RETRY);
    retrying.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let misplaced = watches.misplaced();
        let changed: HashSet<Watched> = tokio::select! {
            order = orders.recv() => match order {
                Some(Order::Watch(watched, target)) => {
                    watches.watch(watched, start(&home, &target));
                    HashSet::new()
                }
                Some(Order::Unwatch(watched)) => {
                    watches.unwatch(watched);
                    HashSet::new()
                }
                None => return,
            },
            batch = noticed.recv() => {
                let Some(batch) = batch else {
                    tracing::warn!("stopped noticing changes for a window");
                    return;
                };
                watches.noticed(&batch)
            },
            _ = retrying.tick(), if misplaced => watches.retried(),
        };
        for watched in changed {
            if outbox.send(FromDaemon::Changed(watched)).await.is_err() {
                return;
            }
        }
    }
}

/// Batches of watches noticed and waiting for the actor.
const QUEUE: usize = 64;
/// The watch descriptor inotify tells its queue's overflow on (`IN_Q_OVERFLOW`).
const OVERFLOW: i32 = -1;
/// How far above its goal a watch may be. Any further and it might watch the home folder or
/// the root, whose many writes would each wake it.
const MOST_ABOVE: usize = 2;
/// How often a watch that couldn't be placed is tried again.
const RETRY: Duration = Duration::from_secs(5);

/// What a folder watched is watched for: anything written, made, removed or moved in it, and
/// its own moving away.
const FOLDER_FLAGS: WatchFlags = WatchFlags::CREATE
    .union(WatchFlags::MODIFY)
    .union(WatchFlags::DELETE)
    .union(WatchFlags::CLOSE_WRITE)
    .union(WatchFlags::MOVED_TO)
    .union(WatchFlags::MOVED_FROM)
    .union(WatchFlags::MOVE_SELF);

/// What the watches need of the kernel: its watches on folders, and which folders are there.
trait Folders {
    /// Watches `folder`, or adds to what is watched there already; returns its descriptor,
    /// the same for each watch on one folder.
    fn add(&mut self, folder: &Path) -> Result<i32, Errno>;
    /// Lets go of the watch on `descriptor`.
    fn remove(&mut self, descriptor: i32);
    fn is_folder(&self, path: &Path) -> bool;
}

/// The folders as an inotify instance watches them, and as they are on disk.
#[derive(Debug)]
struct Inotify(OwnedFd);

impl Folders for Inotify {
    fn add(&mut self, folder: &Path) -> Result<i32, Errno> {
        // Added to what is watched there already for another of the window's watches.
        inotify::add_watch(&self.0, folder, FOLDER_FLAGS | WatchFlags::MASK_ADD)
    }

    fn remove(&mut self, descriptor: i32) {
        if let Err(error) = inotify::remove_watch(&self.0, descriptor) {
            tracing::debug!(%error, "a watch that was gone already");
        }
    }

    fn is_folder(&self, path: &Path) -> bool {
        path.is_dir()
    }
}

/// A window's watches, by the names the window gave them, on the folders `Kernel` watches.
#[derive(Debug)]
struct Watches<Kernel> {
    kernel: Kernel,
    watches: HashMap<Watched, Watch>,
}

impl<Kernel: Folders> Watches<Kernel> {
    fn new(kernel: Kernel) -> Self {
        Self {
            kernel,
            watches: HashMap::new(),
        }
    }

    /// Returns whether a watch couldn't be placed, and is to be tried again.
    fn misplaced(&self) -> bool {
        self.watches.values().any(|watch| watch.misplaced)
    }

    /// Places `watch` as `watched`, in place of what had that name.
    fn watch(&mut self, watched: Watched, mut watch: Watch) {
        let mut left = watch.place(&mut self.kernel, &mut false);
        left.extend(
            self.watches
                .insert(watched, watch)
                .and_then(|old| old.descriptor()),
        );
        self.forget_unused(left);
    }

    fn unwatch(&mut self, watched: Watched) {
        let left = self
            .watches
            .remove(&watched)
            .and_then(|old| old.descriptor());
        self.forget_unused(left);
    }

    /// Returns the watches that a batch of what inotify noticed concerns.
    fn noticed(&mut self, batch: &HashSet<Noticed>) -> HashSet<Watched> {
        if batch.iter().any(|noticed| noticed.descriptor == OVERFLOW) {
            self.overflowed()
        } else {
            batch
                .iter()
                .flat_map(|noticed| self.touched(noticed))
                .collect()
        }
    }

    /// Tries again to place the watches that couldn't be, and returns those that moved.
    fn retried(&mut self) -> HashSet<Watched> {
        let mut changed = HashSet::new();
        let mut left = Vec::new();
        for (watched, watch) in self.watches.iter_mut().filter(|(_, watch)| watch.misplaced) {
            let mut moved = false;
            left.extend(watch.place(&mut self.kernel, &mut moved));
            if moved {
                changed.insert(*watched);
            }
        }
        self.forget_unused(left);
        changed
    }

    /// Returns the watches that `noticed` concerns. One whose folder went is placed again, and
    /// one above its goal moves down as folders on the way come.
    fn touched(&mut self, noticed: &Noticed) -> Vec<Watched> {
        let named: Vec<Watched> = self
            .watches
            .iter()
            .filter(|(_, watch)| {
                watch.descriptor() == Some(noticed.descriptor)
                    && (noticed.left.is_some() || watch.concerns(noticed.name.as_deref()))
            })
            .map(|(watched, _)| *watched)
            .collect();
        let mut left = Vec::new();
        if noticed.left == Some(Left::Moved) {
            left.push(noticed.descriptor);
        }
        for watched in &named {
            let Some(watch) = self.watches.get_mut(watched) else {
                continue;
            };
            if noticed.left.is_some() {
                // Gone, or watching a folder elsewhere now: placed afresh.
                watch.at = None;
            } else if !watch.above_its_goal() {
                continue;
            }
            left.extend(watch.place(&mut self.kernel, &mut false));
        }
        self.forget_unused(left);
        named
    }

    /// Places every watch afresh and returns them all, after inotify's queue overflowed: what
    /// was lost may have concerned any of them, a folder made or gone among it.
    fn overflowed(&mut self) -> HashSet<Watched> {
        let mut left = Vec::new();
        for watch in self.watches.values_mut() {
            left.extend(watch.at.take().map(|(_, descriptor)| descriptor));
            left.extend(watch.place(&mut self.kernel, &mut false));
        }
        self.forget_unused(left);
        self.watches.keys().copied().collect()
    }

    /// Lets go of each of `descriptors` that none of the window's watches is on.
    fn forget_unused(&mut self, descriptors: impl IntoIterator<Item = i32>) {
        let mut forgotten = HashSet::new();
        for descriptor in descriptors {
            if forgotten.insert(descriptor)
                && self
                    .watches
                    .values()
                    .all(|watch| watch.descriptor() != Some(descriptor))
            {
                self.kernel.remove(descriptor);
            }
        }
    }
}

/// A window's watch on a folder with inotify; for a transcript, the folder it is in.
///
/// We watch a transcript's folder, not the file: a watch follows its file, so a file written
/// anew by a rename over it would go unseen. Until the folder is there, the nearest one above
/// it is watched, at most [`MOST_ABOVE`] up.
#[derive(Debug)]
struct Watch {
    /// The folder it watches once that is there.
    goal: PathBuf,
    /// The transcript in it; `None` for the records' folder, where any change is told.
    file: Option<PathBuf>,
    /// The folder watched now, the goal or one above it, and its descriptor.
    at: Option<(PathBuf, i32)>,
    /// Set when it couldn't be placed; it is tried again every [`RETRY`].
    misplaced: bool,
}

impl Watch {
    fn new(goal: PathBuf, file: Option<PathBuf>) -> Self {
        Self {
            goal,
            file,
            at: None,
            misplaced: true,
        }
    }

    fn descriptor(&self) -> Option<i32> {
        self.at.as_ref().map(|&(_, descriptor)| descriptor)
    }

    /// Returns whether what inotify noticed at `name` in the folder watched, or in the folder
    /// itself, concerns this watch: its file, anything in the records' folder, or a folder on
    /// the way to its goal.
    fn concerns(&self, name: Option<&OsStr>) -> bool {
        let Some((folder, _)) = &self.at else {
            return false;
        };
        let Some(name) = name else {
            return true;
        };
        let named = folder.join(name);
        if *folder == self.goal {
            self.file.as_ref().is_none_or(|file| *file == named)
        } else {
            self.goal.starts_with(&named)
        }
    }

    /// Returns whether it watches a folder above its goal, so a nearer one may have come.
    fn above_its_goal(&self) -> bool {
        self.at
            .as_ref()
            .is_some_and(|(folder, _)| *folder != self.goal)
    }

    /// Watches the nearest folder to its goal that is there, including one made meanwhile.
    /// Returns the descriptors it let go of, and sets `moved` if it moved.
    fn place(&mut self, kernel: &mut impl Folders, moved: &mut bool) -> Vec<i32> {
        let mut left = Vec::new();
        self.misplaced = false;
        // Each step down nearer: as many as there are folders it may be above its goal.
        for _ in 0..=MOST_ABOVE {
            let Some(nearest) = nearest_folder(kernel, &self.goal) else {
                left.extend(self.at.take().map(|(_, descriptor)| descriptor));
                self.misplaced = true;
                break;
            };
            if self
                .at
                .as_ref()
                .is_some_and(|(folder, _)| *folder == nearest)
            {
                break;
            }
            match kernel.add(&nearest) {
                Ok(descriptor) => {
                    if let Some((_, old)) = self.at.replace((nearest, descriptor))
                        && old != descriptor
                    {
                        left.push(old);
                    }
                    *moved = true;
                }
                Err(error) => {
                    tracing::debug!(%error, folder = ?nearest, "a watch stays where it was");
                    self.misplaced = true;
                    break;
                }
            }
        }
        left
    }
}

/// Returns the nearest folder to `goal` that is there: itself, or one at most [`MOST_ABOVE`]
/// above it.
fn nearest_folder(kernel: &impl Folders, goal: &Path) -> Option<PathBuf> {
    goal.ancestors()
        .take(MOST_ABOVE + 1)
        .find(|folder| kernel.is_folder(folder))
        .map(Path::to_path_buf)
}

/// Returns the watch for `target`, not placed yet.
fn start(home: &ClaudeHome, target: &Target) -> Watch {
    match target {
        Target::Records => Watch::new(home.records(), None),
        Target::Transcript(place) => {
            let file = resolve(home, place).path().to_owned();
            let goal = file.parent().map_or_default(Path::to_path_buf);
            Watch::new(goal, Some(file))
        }
    }
}

/// What inotify noticed, once read off its queue.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Noticed {
    descriptor: i32,
    /// The name in the folder watched; `None` for the folder itself.
    name: Option<OsString>,
    /// The folder watched itself went.
    left: Option<Left>,
}

/// How a folder watched went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Left {
    /// Removed or unmounted, so inotify let go of its watch.
    Gone,
    /// Moved, taking its watch along, which we must let go of.
    Moved,
}

/// Sends on what inotify notices, a batch at a time, until the actor closes its end of
/// `ending`.
fn read(inotify: &OwnedFd, ending: &OwnedFd, noticing: &mpsc::Sender<HashSet<Noticed>>) {
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
        match queued(inotify, &mut buffer) {
            Ok(batch) => {
                if !batch.is_empty() && noticing.blocking_send(batch).is_err() {
                    return;
                }
            }
            Err(error) => {
                tracing::warn!(%error, "stopped noticing changes for a window");
                return;
            }
        }
    }
}

/// Returns all that a non-blocking `inotify` has queued for now.
fn queued(inotify: &OwnedFd, buffer: &mut [MaybeUninit<u8>]) -> Result<HashSet<Noticed>, Errno> {
    let mut reader = inotify::Reader::new(inotify, buffer);
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
            Err(Errno::WOULDBLOCK) => return Ok(batch),
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// Folders and the kernel's watches on them, as inotify keeps them: one descriptor for each
    /// folder watched.
    #[derive(Debug, Default)]
    struct Kernel {
        folders: HashSet<PathBuf>,
        watched: HashMap<PathBuf, i32>,
        last: i32,
    }

    impl Folders for Kernel {
        fn add(&mut self, folder: &Path) -> Result<i32, Errno> {
            if !self.folders.contains(folder) {
                return Err(Errno::NOENT);
            }
            if let Some(&descriptor) = self.watched.get(folder) {
                return Ok(descriptor);
            }
            self.last += 1;
            self.watched.insert(folder.to_owned(), self.last);
            Ok(self.last)
        }

        fn remove(&mut self, descriptor: i32) {
            self.watched.retain(|_, watched| *watched != descriptor);
        }

        fn is_folder(&self, path: &Path) -> bool {
            self.folders.contains(path)
        }
    }

    impl Kernel {
        fn with(folders: &[&str]) -> Self {
            Self {
                folders: folders.iter().map(PathBuf::from).collect(),
                ..Self::default()
            }
        }

        fn make(&mut self, folder: &str) {
            self.folders.insert(PathBuf::from(folder));
        }

        fn take_away(&mut self, folder: &str) {
            self.folders.remove(Path::new(folder));
        }

        fn on(&self, folder: &str) -> i32 {
            self.watched
                .get(Path::new(folder))
                .copied()
                .expect("a folder watched")
        }

        fn watching(&self) -> BTreeSet<&str> {
            self.watched
                .keys()
                .map(|folder| folder.to_str().expect("a test's folder"))
                .collect()
        }
    }

    fn transcript(goal: &str, file: &str) -> Watch {
        Watch::new(PathBuf::from(goal), Some(Path::new(goal).join(file)))
    }

    fn noticed(descriptor: i32, name: Option<&str>, left: Option<Left>) -> HashSet<Noticed> {
        HashSet::from([Noticed {
            descriptor,
            name: name.map(OsString::from),
            left,
        }])
    }

    #[test]
    fn a_transcript_is_told_of_its_own_file_alone() {
        let mut watches = Watches::new(Kernel::with(&["/h", "/h/p"]));
        let watched = Watched::new();
        watches.watch(watched, transcript("/h/p", "s.jsonl"));
        let at = watches.kernel.on("/h/p");
        assert_eq!(
            watches.noticed(&noticed(at, Some("s.jsonl"), None)),
            HashSet::from([watched])
        );
        assert_eq!(
            watches.noticed(&noticed(at, Some("t.jsonl"), None)),
            HashSet::new()
        );
        watches.unwatch(watched);
        assert_eq!(watches.kernel.watching(), BTreeSet::new());
    }

    #[test]
    fn a_watch_moves_down_as_the_folders_on_its_way_come() {
        let mut watches = Watches::new(Kernel::with(&["/h"]));
        let watched = Watched::new();
        watches.watch(watched, transcript("/h/p/s", "a.jsonl"));
        assert_eq!(watches.kernel.watching(), BTreeSet::from(["/h"]));
        let above = watches.kernel.on("/h");
        watches.kernel.make("/h/p");
        watches.kernel.make("/h/p/s");
        assert_eq!(
            watches.noticed(&noticed(above, Some("p"), None)),
            HashSet::from([watched])
        );
        assert_eq!(watches.kernel.watching(), BTreeSet::from(["/h/p/s"]));
    }

    #[test]
    fn a_folder_moved_away_is_let_go_of_and_watched_again_when_it_comes_back() {
        let mut watches = Watches::new(Kernel::with(&["/h", "/h/p"]));
        let watched = Watched::new();
        watches.watch(watched, transcript("/h/p", "s.jsonl"));
        let at = watches.kernel.on("/h/p");
        watches.kernel.take_away("/h/p");
        assert_eq!(
            watches.noticed(&noticed(at, None, Some(Left::Moved))),
            HashSet::from([watched])
        );
        assert_eq!(watches.kernel.watching(), BTreeSet::from(["/h"]));
        watches.kernel.make("/h/p");
        let above = watches.kernel.on("/h");
        assert_eq!(
            watches.noticed(&noticed(above, Some("p"), None)),
            HashSet::from([watched])
        );
        assert_eq!(watches.kernel.watching(), BTreeSet::from(["/h/p"]));
    }

    #[test]
    fn a_watch_with_nowhere_to_be_is_placed_when_tried_again() {
        let mut watches = Watches::new(Kernel::with(&["/h"]));
        let watched = Watched::new();
        watches.watch(watched, transcript("/h/a/b/c", "s.jsonl"));
        assert!(watches.misplaced());
        assert_eq!(watches.retried(), HashSet::new());
        assert!(watches.misplaced());
        for folder in ["/h/a", "/h/a/b", "/h/a/b/c"] {
            watches.kernel.make(folder);
        }
        assert_eq!(watches.retried(), HashSet::from([watched]));
        assert!(!watches.misplaced());
        assert_eq!(watches.kernel.watching(), BTreeSet::from(["/h/a/b/c"]));
    }

    #[test]
    fn an_overflow_tells_every_watch() {
        let mut watches = Watches::new(Kernel::with(&["/h", "/h/p", "/h/q"]));
        let (first, second) = (Watched::new(), Watched::new());
        watches.watch(first, transcript("/h/p", "s.jsonl"));
        watches.watch(second, transcript("/h/q", "s.jsonl"));
        assert_eq!(
            watches.noticed(&noticed(OVERFLOW, None, None)),
            HashSet::from([first, second])
        );
        assert_eq!(watches.kernel.watching(), BTreeSet::from(["/h/p", "/h/q"]));
    }

    #[test]
    fn watches_on_one_folder_keep_it_until_the_last_goes() {
        let mut watches = Watches::new(Kernel::with(&["/h", "/h/p"]));
        let (first, second) = (Watched::new(), Watched::new());
        watches.watch(first, transcript("/h/p", "s.jsonl"));
        watches.watch(second, transcript("/h/p", "t.jsonl"));
        let at = watches.kernel.on("/h/p");
        watches.unwatch(first);
        assert_eq!(watches.kernel.watching(), BTreeSet::from(["/h/p"]));
        assert_eq!(
            watches.noticed(&noticed(at, Some("t.jsonl"), None)),
            HashSet::from([second])
        );
        watches.unwatch(second);
        assert_eq!(watches.kernel.watching(), BTreeSet::new());
    }

    #[test]
    fn what_is_noticed_concerns_a_transcript_on_its_way() {
        let file = PathBuf::from("/h/projects/p/s.jsonl");
        let mut watch = Watch::new(PathBuf::from("/h/projects/p"), Some(file));
        assert!(!watch.concerns(None), "nowhere yet");
        watch.at = Some((PathBuf::from("/h/projects"), 1));
        assert!(watch.concerns(Some(OsStr::new("p"))));
        assert!(!watch.concerns(Some(OsStr::new("q"))));
        assert!(!watch.concerns(Some(OsStr::new("p2"))));
        assert!(watch.concerns(None));
        assert!(watch.above_its_goal());
        watch.at = Some((PathBuf::from("/h/projects/p"), 2));
        assert!(watch.concerns(Some(OsStr::new("s.jsonl"))));
        assert!(!watch.concerns(Some(OsStr::new("t.jsonl"))));
        assert!(!watch.above_its_goal());
        let mut records = Watch::new(PathBuf::from("/h/sessions"), None);
        records.at = Some((PathBuf::from("/h/sessions"), 3));
        assert!(records.concerns(Some(OsStr::new("anything"))));
        records.at = Some((PathBuf::from("/h"), 4));
        assert!(records.concerns(Some(OsStr::new("sessions"))));
        assert!(!records.concerns(Some(OsStr::new("projects"))));
    }

    #[tokio::test]
    async fn a_window_is_told_of_its_transcript_written() {
        let dir = std::env::temp_dir().join(format!("edualc-told-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("project")).expect("a scratch folder");
        let (orders, received) = mpsc::channel(1);
        let (outbox, mut told) = mpsc::channel(8);
        let home = ClaudeHome::moved_to(dir.join("claude"));
        let actor = tokio::spawn(watch(home, received, outbox));
        let watched = Watched::new();
        let path = dir.join("project/s.jsonl");
        let target = Target::Transcript(crate::ipc::Place::Path(path.clone()));
        orders
            .send(Order::Watch(watched, target))
            .await
            .expect("ordered");
        // With room for one order, the last of these is sent once the actor took the one
        // before, so it has carried out the watch.
        for _ in 0..2 {
            orders
                .send(Order::Unwatch(Watched::new()))
                .await
                .expect("ordered");
        }
        std::fs::write(&path, "{}\n").expect("written");
        assert!(matches!(told.recv().await, Some(FromDaemon::Changed(it)) if it == watched));
        drop((orders, told));
        actor.await.expect("the actor ended");
        std::fs::remove_dir_all(&dir).expect("the scratch folder goes");
    }

    // The kernel queues what inotify notices during the call that does it, so each read here
    // follows the change it is about.
    #[test]
    fn inotify_tells_a_file_made_renamed_over_and_its_folder_moved_or_removed() {
        let dir = std::env::temp_dir().join(format!("edualc-inotify-{}", std::process::id()));
        let project = dir.join("project");
        std::fs::create_dir_all(&project).expect("a scratch folder");
        let flags = CreateFlags::CLOEXEC | CreateFlags::NONBLOCK;
        let mut kernel = Inotify(inotify::init(flags).expect("an inotify instance"));
        let at = kernel.add(&project).expect("the folder watched");
        let mut buffer = [MaybeUninit::<u8>::uninit(); 4096];
        let mut queued = || queued(&kernel.0, &mut buffer).expect("read");
        let in_it = |name: &str| noticed(at, Some(name), None);
        assert_eq!(queued(), HashSet::new(), "nothing yet");

        std::fs::write(project.join("s.jsonl"), "{}\n").expect("written");
        assert_eq!(queued(), in_it("s.jsonl"), "made and written");
        let staged = dir.join("staged.jsonl");
        std::fs::write(&staged, "{}\n{}\n").expect("staged");
        std::fs::rename(&staged, project.join("s.jsonl")).expect("renamed over it");
        assert_eq!(queued(), in_it("s.jsonl"), "renamed over");
        std::fs::rename(&project, dir.join("project.old")).expect("moved away");
        assert_eq!(queued(), noticed(at, None, Some(Left::Moved)), "moved away");

        let other = dir.join("other");
        std::fs::create_dir(&other).expect("another folder");
        let other_at = kernel.add(&other).expect("it watched");
        let mut queued = || self::queued(&kernel.0, &mut buffer).expect("read");
        std::fs::remove_dir(&other).expect("removed");
        assert_eq!(
            queued(),
            noticed(other_at, None, Some(Left::Gone)),
            "removed"
        );
        std::fs::remove_dir_all(&dir).expect("the scratch folder goes");
    }
}
