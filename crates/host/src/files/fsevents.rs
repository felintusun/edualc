//! What notices a change on macOS: FSEvents, through `notify`. Its callback runs on a thread of
//! FSEvents', and only passes on what it is told; which watch a change is about is read here,
//! by the notifier, which alone knows the folders watched.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use notify::event::{EventKind, ModifyKind};
use notify::{RecursiveMode, Watcher as _};
use tokio::sync::mpsc;

use super::{Left, Noticed, OVERFLOW};

pub(super) type Error = notify::Error;
/// What FSEvents tells at a time: one change, to one or more paths.
pub(super) type Batch = notify::Result<notify::Event>;

/// The folders watched, each with the descriptor its changes come with, by the path FSEvents
/// tells (links resolved, `/var` as `/private/var`).
#[derive(Debug)]
pub(super) struct Notifier {
    watcher: notify::FsEventWatcher,
    folders: HashMap<i32, PathBuf>,
    next: i32,
}

impl Notifier {
    /// Starts noticing: what is noticed goes to `noticing`, until the notifier is dropped.
    pub(super) fn start(noticing: mpsc::Sender<Batch>) -> Result<Self, Error> {
        let watcher = notify::FsEventWatcher::new(
            move |batch: Batch| {
                if noticing.blocking_send(batch).is_err() {
                    tracing::debug!("a change noticed after its window's watches ended");
                }
            },
            notify::Config::default(),
        )?;
        Ok(Self {
            watcher,
            folders: HashMap::new(),
            next: 1,
        })
    }

    /// Watches `folder`; the descriptor its changes come with. A folder watched already for
    /// another of the window's watches has the same descriptor, as inotify gives it.
    pub(super) fn add(&mut self, folder: &Path) -> Result<i32, Error> {
        let folder = folder.canonicalize().map_err(notify::Error::io)?;
        if let Some(descriptor) = self.descriptor(&folder) {
            return Ok(descriptor);
        }
        self.watcher.watch(&folder, RecursiveMode::NonRecursive)?;
        let descriptor = self.next;
        self.next += 1;
        self.folders.insert(descriptor, folder);
        Ok(descriptor)
    }

    pub(super) fn remove(&mut self, descriptor: i32) -> Result<(), Error> {
        match self.folders.remove(&descriptor) {
            Some(folder) => self.watcher.unwatch(&folder),
            None => Err(notify::Error::watch_not_found()),
        }
    }

    /// What a change FSEvents told is about: the folder watched itself (`name` none), or a file
    /// or folder in it. A change FSEvents couldn't tell in full (it dropped events, or failed)
    /// is taken as inotify's overflow: every watch is placed afresh, and told.
    pub(super) fn noticed(&self, batch: Batch) -> HashSet<Noticed> {
        let overflow = || {
            HashSet::from([Noticed {
                descriptor: OVERFLOW,
                name: None,
                left: None,
            }])
        };
        let event = match batch {
            Ok(event) if !event.need_rescan() => event,
            Ok(_) => return overflow(),
            Err(error) => {
                tracing::debug!(%error, "FSEvents failed to tell a change");
                return overflow();
            }
        };
        // FSEvents watches a path, not a folder: a folder removed keeps its watch, which serves
        // it again should it come back.
        let left = match event.kind {
            EventKind::Remove(_) => Some(Left::Gone),
            EventKind::Modify(ModifyKind::Name(_)) => Some(Left::Moved),
            EventKind::Any
            | EventKind::Access(_)
            | EventKind::Create(_)
            | EventKind::Modify(_)
            | EventKind::Other => None,
        };
        event
            .paths
            .iter()
            .filter_map(|path| match self.descriptor(path) {
                Some(descriptor) => Some(Noticed {
                    descriptor,
                    name: None,
                    left,
                }),
                None => Some(Noticed {
                    descriptor: self.descriptor(path.parent()?)?,
                    name: path.file_name().map(ToOwned::to_owned),
                    left: None,
                }),
            })
            .collect()
    }

    fn descriptor(&self, folder: &Path) -> Option<i32> {
        self.folders
            .iter()
            .find(|(_, known)| *known == folder)
            .map(|(&descriptor, _)| descriptor)
    }
}
