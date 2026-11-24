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
    fn place(&mut self, inotify: &OwnedFd, moved: &mut bool) -> Vec<i32> {
        let mut left = Vec::new();
        self.misplaced = false;
        // Each step down nearer: as many as there are folders it may be above its goal.
        for _ in 0..=MOST_ABOVE {
            let Some(nearest) = nearest_folder(&self.goal) else {
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
            match add(inotify, &nearest, FOLDER_FLAGS) {
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
    let mut watches: HashMap<Watched, Watch> = HashMap::new();
    let mut retrying = tokio::time::interval(RETRY);
    retrying.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let misplaced = watches.values().any(|watch| watch.misplaced);
        let changed: HashSet<Watched> = tokio::select! {
            order = orders.recv() => match order {
                Some(Order::Watch(watched, target)) => {
                    let mut watch = start(&home, &target);
                    let mut left = watch.place(&inotify, &mut false);
                    left.extend(watches.insert(watched, watch).and_then(|old| old.descriptor()));
                    forget_unused(&inotify, &watches, left);
                    HashSet::new()
                }
                Some(Order::Unwatch(watched)) => {
                    let left = watches.remove(&watched).and_then(|old| old.descriptor());
                    forget_unused(&inotify, &watches, left);
                    HashSet::new()
                }
                None => return,
            },
            batch = noticed.recv() => {
                let Some(batch) = batch else {
                    tracing::warn!("stopped noticing changes for a window");
                    return;
                };
                if batch.iter().any(|noticed| noticed.descriptor == OVERFLOW) {
                    overflowed(&inotify, &mut watches)
                } else {
                    batch
                        .iter()
                        .flat_map(|noticed| touched(&inotify, &mut watches, noticed))
                        .collect()
                }
            },
            _ = retrying.tick(), if misplaced => retried(&inotify, &mut watches),
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

/// Returns the window's watches that `noticed` concerns. One whose folder went is placed
/// again, and one above its goal moves down as folders on the way come.
fn touched(
    inotify: &OwnedFd,
    watches: &mut HashMap<Watched, Watch>,
    noticed: &Noticed,
) -> Vec<Watched> {
    let named: Vec<Watched> = watches
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
        let Some(watch) = watches.get_mut(watched) else {
            continue;
        };
        if noticed.left.is_some() {
            // Gone, or watching a folder elsewhere now: placed afresh.
            watch.at = None;
        } else if !watch.above_its_goal() {
            continue;
        }
        left.extend(watch.place(inotify, &mut false));
    }
    forget_unused(inotify, watches, left);
    named
}

/// Places every watch afresh and tells each, after inotify's queue overflowed: what was lost
/// may have concerned any of them, a folder made or gone among it.
fn overflowed(inotify: &OwnedFd, watches: &mut HashMap<Watched, Watch>) -> HashSet<Watched> {
    let mut left = Vec::new();
    for watch in watches.values_mut() {
        left.extend(watch.at.take().map(|(_, descriptor)| descriptor));
        left.extend(watch.place(inotify, &mut false));
    }
    forget_unused(inotify, watches, left);
    watches.keys().copied().collect()
}

/// Tries again to place the watches that couldn't be, and tells those that moved.
fn retried(inotify: &OwnedFd, watches: &mut HashMap<Watched, Watch>) -> HashSet<Watched> {
    let mut changed = HashSet::new();
    let mut left = Vec::new();
    for (watched, watch) in watches.iter_mut().filter(|(_, watch)| watch.misplaced) {
        let mut moved = false;
        left.extend(watch.place(inotify, &mut moved));
        if moved {
            changed.insert(*watched);
        }
    }
    forget_unused(inotify, watches, left);
    changed
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

/// Returns the nearest folder to `goal` that is there: itself, or one at most [`MOST_ABOVE`]
/// above it.
fn nearest_folder(goal: &Path) -> Option<PathBuf> {
    goal.ancestors()
        .take(MOST_ABOVE + 1)
        .find(|folder| folder.is_dir())
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

fn add(inotify: &OwnedFd, path: &Path, flags: WatchFlags) -> Result<i32, Errno> {
    // Added to what is watched there already for another of the window's watches.
    inotify::add_watch(inotify, path, flags | WatchFlags::MASK_ADD)
}

/// Lets go of each of `descriptors` that none of the window's watches is on.
fn forget_unused(
    inotify: &OwnedFd,
    watches: &HashMap<Watched, Watch>,
    descriptors: impl IntoIterator<Item = i32>,
) {
    let mut forgotten = HashSet::new();
    for descriptor in descriptors {
        if forgotten.insert(descriptor)
            && watches
                .values()
                .all(|watch| watch.descriptor() != Some(descriptor))
            && let Err(error) = inotify::remove_watch(inotify, descriptor)
        {
            tracing::debug!(%error, "a watch that was gone already");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::Place;

    #[tokio::test]
    async fn a_transcript_is_watched_before_and_after_it_is_written() {
        let mut scene = Scene::watching("watch", "session.jsonl").await;
        let path = scene.dir.join("session.jsonl");
        std::fs::write(&path, "{}\n").expect("written");
        assert_eq!(changed(&mut scene.told).await, Some(scene.watched), "made");
        settle(&mut scene.told).await;
        std::fs::write(&path, "{}\n{}\n").expect("written again");
        assert_eq!(
            changed(&mut scene.told).await,
            Some(scene.watched),
            "written to"
        );
        settle(&mut scene.told).await;
        let watched = scene.watched;
        scene
            .orders
            .send(Order::Unwatch(watched))
            .await
            .expect("ordered");
        tokio::time::sleep(Duration::from_millis(50)).await;
        std::fs::write(&path, "{}\n{}\n{}\n").expect("written after");
        assert!(quiet(&mut scene.told).await, "unwatched");
        scene.end().await;
    }

    #[tokio::test]
    async fn a_transcript_is_followed_through_its_folder() {
        let mut scene = Scene::watching("follow", "project/session.jsonl").await;
        let project = scene.dir.join("project");
        let path = project.join("session.jsonl");
        std::fs::create_dir(&project).expect("its folder made");
        assert_eq!(
            changed(&mut scene.told).await,
            Some(scene.watched),
            "its folder came"
        );
        settle(&mut scene.told).await;
        std::fs::write(&path, "{}\n").expect("written");
        assert_eq!(
            changed(&mut scene.told).await,
            Some(scene.watched),
            "made in its folder"
        );
        settle(&mut scene.told).await;
        std::fs::write(project.join("another.jsonl"), "{}\n").expect("another written");
        assert!(quiet(&mut scene.told).await, "another file isn't it");
        let staged = scene.dir.join("staged.jsonl");
        std::fs::write(&staged, "{}\n{}\n").expect("staged");
        std::fs::rename(&staged, &path).expect("renamed over it");
        assert_eq!(
            changed(&mut scene.told).await,
            Some(scene.watched),
            "renamed over"
        );
        settle(&mut scene.told).await;
        std::fs::write(&path, "{}\n{}\n{}\n").expect("written to after");
        assert_eq!(
            changed(&mut scene.told).await,
            Some(scene.watched),
            "written to after"
        );
        scene.end().await;
    }

    #[tokio::test]
    async fn a_watch_moves_down_through_folders_made_at_once() {
        let mut scene = Scene::watching("deep", "p/s/subagents/agent.jsonl").await;
        let folder = scene.dir.join("p/s/subagents");
        std::fs::create_dir_all(&folder).expect("its folders made");
        assert_eq!(
            changed(&mut scene.told).await,
            Some(scene.watched),
            "its folders came"
        );
        settle(&mut scene.told).await;
        std::fs::write(folder.join("agent.jsonl"), "{}\n").expect("written");
        assert_eq!(
            changed(&mut scene.told).await,
            Some(scene.watched),
            "made in its folder"
        );
        scene.end().await;
    }

    #[tokio::test]
    async fn a_folder_moved_away_is_let_go_of() {
        let mut scene = Scene::watching("moved", "project/session.jsonl").await;
        let project = scene.dir.join("project");
        std::fs::create_dir(&project).expect("its folder made");
        assert_eq!(
            changed(&mut scene.told).await,
            Some(scene.watched),
            "its folder came"
        );
        settle(&mut scene.told).await;
        let old = scene.dir.join("project.old");
        std::fs::rename(&project, &old).expect("moved away");
        assert_eq!(
            changed(&mut scene.told).await,
            Some(scene.watched),
            "moved away"
        );
        settle(&mut scene.told).await;
        std::fs::write(old.join("session.jsonl"), "{}\n").expect("written where it went");
        assert!(
            quiet(&mut scene.told).await,
            "the folder moved away isn't it"
        );
        std::fs::create_dir(&project).expect("made again");
        assert_eq!(
            changed(&mut scene.told).await,
            Some(scene.watched),
            "made again"
        );
        settle(&mut scene.told).await;
        std::fs::write(project.join("session.jsonl"), "{}\n").expect("written");
        assert_eq!(
            changed(&mut scene.told).await,
            Some(scene.watched),
            "written there"
        );
        scene.end().await;
    }

    #[tokio::test]
    async fn a_watch_with_nowhere_to_be_is_tried_again() {
        let mut scene = Scene::watching("retry", "a/b/c/d/session.jsonl").await;
        std::fs::create_dir_all(scene.dir.join("a/b/c/d")).expect("its folders made");
        assert_eq!(
            changed(&mut scene.told).await,
            Some(scene.watched),
            "placed at last"
        );
        settle(&mut scene.told).await;
        std::fs::write(scene.dir.join("a/b/c/d/session.jsonl"), "{}\n").expect("written");
        assert_eq!(
            changed(&mut scene.told).await,
            Some(scene.watched),
            "made in its folder"
        );
        scene.end().await;
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

    /// The next watch told of as changed, if one is within a while.
    async fn changed(outbox: &mut mpsc::Receiver<FromDaemon>) -> Option<Watched> {
        match tokio::time::timeout(Duration::from_secs(10), outbox.recv()).await {
            Ok(Some(FromDaemon::Changed(watched))) => Some(watched),
            Ok(_) | Err(_) => None,
        }
    }

    /// Lets what is still being told of the last change be told.
    async fn settle(outbox: &mut mpsc::Receiver<FromDaemon>) {
        while let Ok(Some(_)) =
            tokio::time::timeout(Duration::from_millis(200), outbox.recv()).await
        {}
    }

    /// Nothing is told within a while.
    async fn quiet(outbox: &mut mpsc::Receiver<FromDaemon>) -> bool {
        tokio::time::timeout(Duration::from_millis(300), outbox.recv())
            .await
            .is_err()
    }

    /// A scratch folder for a test, and a window's watches of a transcript in it.
    struct Scene {
        dir: PathBuf,
        orders: mpsc::Sender<Order>,
        told: mpsc::Receiver<FromDaemon>,
        actor: tokio::task::JoinHandle<()>,
        watched: Watched,
    }

    impl Scene {
        async fn watching(name: &str, transcript: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("edualc-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("a scratch folder");
            let (orders, received) = mpsc::channel(8);
            let (outbox, told) = mpsc::channel(8);
            let claude = ClaudeHome::moved_to(dir.join("claude"));
            let actor = tokio::spawn(watch(claude, received, outbox));
            let watched = Watched::new();
            let target = Target::Transcript(Place::Path(dir.join(transcript)));
            orders
                .send(Order::Watch(watched, target))
                .await
                .expect("ordered");
            // The order is taken before the test goes on.
            tokio::time::sleep(Duration::from_millis(50)).await;
            Self {
                dir,
                orders,
                told,
                actor,
                watched,
            }
        }

        async fn end(self) {
            drop(self.orders);
            self.actor.await.expect("the actor ended");
            std::fs::remove_dir_all(&self.dir).expect("the scratch folder goes");
        }
    }
}
