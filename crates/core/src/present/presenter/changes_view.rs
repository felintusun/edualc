//! The Changes view, and the line under each turn that says what it changed.

use std::collections::BTreeSet;
use std::path::Path;

use crate::app::{ChangesView, LineComment, Listed, comments_on, diff_rows};
use crate::changes;
use crate::chat;
use crate::present::Frontend;
use crate::present::model;
use crate::sessions;

use super::{Presenter, index_for_slint};

/// "Turn 2 · 18 s · 24.1k tokens in, 312 out".
pub(super) fn turn_facts(turn: &chat::TurnEnd) -> String {
    let tokens = |count: u64| {
        if count >= 1000 {
            let tenths = count.saturating_add(50) / 100;
            format!("{}.{}k", tenths / 10, tenths % 10)
        } else {
            count.to_string()
        }
    };
    let took = turn.duration_ms.map(|ms| {
        let seconds = ms.saturating_add(500) / 1000;
        if seconds < 60 {
            format!("{seconds} s")
        } else {
            format!("{} min {} s", seconds / 60, seconds % 60)
        }
    });
    [
        Some(format!("Turn {}", turn.number)),
        took,
        Some(format!(
            "{} tokens in, {} out",
            tokens(turn.tokens_in),
            tokens(turn.tokens_out)
        )),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ")
}

/// "3 files · +12 −4".
pub(super) fn summary_words(summary: changes::Summary) -> String {
    let files = if summary.files == 1 {
        "1 file".to_owned()
    } else {
        format!("{} files", summary.files)
    };
    format!("{files} · +{} −{}", summary.added, summary.removed)
}

pub(super) fn change_review(review: changes::Review) -> model::ChangeReview {
    match review {
        changes::Review::NotReviewed => model::ChangeReview::NotReviewed,
        changes::Review::Reviewed => model::ChangeReview::Reviewed,
        changes::Review::ChangedSinceReviewed => model::ChangeReview::ChangedSince,
    }
}

/// Says once what the snapshots left out; empty if nothing was.
pub(super) fn left_out_words(left_out: changes::LeftOut) -> String {
    let plural = |count: u32, one: &str, many: &str| {
        if count == 1 {
            format!("1 {one}")
        } else {
            format!("{count} {many}")
        }
    };
    let mut parts = Vec::new();
    if left_out.large > 0 {
        parts.push(plural(
            left_out.large,
            "file over 1 MiB",
            "files over 1 MiB",
        ));
    }
    if left_out.binary > 0 {
        parts.push(plural(left_out.binary, "binary file", "binary files"));
    }
    if left_out.beyond_limit {
        parts.push(format!("the files after the first {}", changes::MOST_FILES));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("Not looked at: {}.", parts.join(", "))
    }
}

impl<F: Frontend> Presenter<F> {
    /// Shows the Changes view if it is open, with your comments not sent yet, and what the last
    /// turn changed, for the top bar.
    pub fn render_changes(
        &mut self,
        view: Option<&ChangesView>,
        comments: &[LineComment],
        last_turn: Option<changes::Summary>,
    ) {
        let global = &mut self.changes;
        global.last_turn = last_turn
            .filter(|summary| summary.files > 0)
            .map_or_else(String::new, summary_words);
        global.comments = index_for_slint(comments.len());
        let Some(view) = view else {
            *global = model::Changes {
                last_turn: std::mem::take(&mut global.last_turn),
                comments: global.comments,
                can_send: true,
                ..model::Changes::default()
            };
            return;
        };
        global.open = true;
        global.can_send = !view.elsewhere;
        global.editing_comment = view.editing.is_some();
        global.scope = match view.scope {
            changes::Scope::Turn => model::ChangeScope::Turn,
            changes::Scope::SinceLooked => model::ChangeScope::SinceLooked,
            changes::Scope::Session => model::ChangeScope::Session,
        };
        let picked = view.picked.as_ref();
        let read = match &view.listed {
            Listed::Read(read) => Some(read),
            Listed::Reading | Listed::Failed(_) => None,
        };
        let files = read.map_or(&[][..], |read| read.files.as_slice());
        let roots: BTreeSet<&Path> = files.iter().map(|file| file.root.as_path()).collect();
        let shown_path = |file: &changes::FileChange| {
            if roots.len() > 1 {
                Path::new(&sessions::folder_name(&file.root))
                    .join(&file.path)
                    .display()
                    .to_string()
            } else {
                file.path.display().to_string()
            }
        };
        global.files = files
            .iter()
            .map(|file| model::ChangedFile {
                path: shown_path(file),
                kind: match file.kind {
                    changes::Kind::Added => model::ChangeKind::Added,
                    changes::Kind::Changed => model::ChangeKind::Changed,
                    changes::Kind::Deleted => model::ChangeKind::Deleted,
                },
                who: match file.who {
                    changes::Who::Agent => model::ChangeWho::Agent,
                    changes::Who::Command => model::ChangeWho::Command,
                    changes::Who::Both => model::ChangeWho::Both,
                    changes::Who::BetweenTurns => model::ChangeWho::BetweenTurns,
                },
                added: i32::try_from(file.added).unwrap_or(i32::MAX),
                removed: i32::try_from(file.removed).unwrap_or(i32::MAX),
                review: change_review(file.review),
                comments: index_for_slint(comments_on(comments, file)),
                selected: picked
                    .is_some_and(|picked| picked.root == file.root && picked.path == file.path),
            })
            .collect();
        (global.summary, global.note) = match &view.listed {
            Listed::Reading => (String::new(), "Reading the snapshots…".to_owned()),
            Listed::Failed(error) => (String::new(), format!("Couldn't read the changes: {error}")),
            Listed::Read(read) if read.files.is_empty() => (
                match view.scope {
                    changes::Scope::Turn => "The last turn changed no files.",
                    changes::Scope::SinceLooked => "Nothing changed since you last looked.",
                    changes::Scope::Session => "The session changed no files.",
                }
                .to_owned(),
                if view.send_blocked {
                    "Continue this session here to send comments.".to_owned()
                } else if view.elsewhere {
                    "edualc keeps changes only for sessions it runs.".to_owned()
                } else {
                    left_out_words(read.left_out)
                },
            ),
            Listed::Read(read) => (
                summary_words(changes::Summary::from(read)),
                if view.send_blocked {
                    "Continue this session here to send comments.".to_owned()
                } else {
                    left_out_words(read.left_out)
                },
            ),
        };
        let picked_file = picked.and_then(|picked| {
            files
                .iter()
                .find(|file| file.root == picked.root && file.path == picked.path)
        });
        global.file_path = picked_file.map_or_else(String::new, shown_path);
        global.file_review = picked_file.map_or(model::ChangeReview::NotReviewed, |file| {
            change_review(file.review)
        });
        (global.diff, global.diff_note) = match picked.map(|picked| (picked, &picked.diff)) {
            None => (Vec::new(), String::new()),
            Some((_, Listed::Reading)) => (Vec::new(), "Reading the diff…".to_owned()),
            Some((_, Listed::Failed(error))) => {
                (Vec::new(), format!("Couldn't read the diff: {error}"))
            }
            Some((picked, Listed::Read(diff))) => (
                diff_rows(diff)
                    .into_iter()
                    .enumerate()
                    .map(|(row, line)| match line {
                        None => model::DiffRow {
                            kind: model::DiffRowKind::Gap,
                            ..model::DiffRow::default()
                        },
                        Some(line) => model::DiffRow {
                            kind: match line.kind {
                                changes::LineKind::Same => model::DiffRowKind::Same,
                                changes::LineKind::Removed => model::DiffRowKind::Removed,
                                changes::LineKind::Added => model::DiffRowKind::Added,
                            },
                            old: line
                                .old
                                .map_or_else(String::new, |number| number.to_string()),
                            new: line
                                .new
                                .map_or_else(String::new, |number| number.to_string()),
                            text: line.text.clone(),
                            comment: comments
                                .iter()
                                .find(|comment| {
                                    comment.root == picked.root
                                        && comment.path == picked.path
                                        && comment.line.old == line.old
                                        && comment.line.new == line.new
                                })
                                .map_or_else(String::new, |comment| comment.text.clone()),
                            editing: view.editing == Some(row),
                        },
                    })
                    .collect(),
                if diff.cut {
                    "Long diff: cut short.".to_owned()
                } else {
                    String::new()
                },
            ),
        };
    }
}
