//! What a transcript says of its session: its title, its last reply, its prompts.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::chat::UserText;
use crate::external::wire::{self, MessageOrigin, TranscriptCwd, TranscriptLine, UserContent};
use crate::ids::SessionId;

use super::{PetName, Worktree};

/// Titles are appended as the session goes on; prompts are at the start. Neither needs the
/// whole file, which can be many megabytes.
const PEEK_BYTES: u64 = 128 * 1024;
/// How much of a transcript's start is read for its first prompt and `cwd`, past a snapshot or
/// an attachment longer than [`PEEK_BYTES`].
const HEAD_SCAN_BYTES: u64 = 4 * 1024 * 1024;

pub(super) struct Summary {
    pub(super) title: String,
    pub(super) worktree: Option<Worktree>,
    pub(super) last_said: Option<String>,
}

/// Reads a session's title, the worktree it last worked in, and the start of its last reply.
pub(super) fn summary(path: &Path) -> Option<Summary> {
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let tail = read_range(&mut file, length.saturating_sub(PEEK_BYTES), length)?;
    let (mut custom, mut generated, mut summary, mut worktree, mut last_said) =
        (None, None, None, None, None);
    let own_name = path
        .file_stem()
        .and_then(|stem| SessionId::try_from(stem.to_string_lossy().into_owned()).ok())
        .map(|id| PetName::of(&id));
    for line in tail.lines().rev() {
        let want_title = custom.is_none() && line.contains("itle\"")
            || generated.is_none() && line.contains("aiTitle")
            || summary.is_none() && line.contains("\"summary\"")
            || worktree.is_none() && line.contains("worktree-state");
        let want_said =
            last_said.is_none() && line.contains("\"assistant\"") && line.contains("\"text\"");
        if !want_title && !want_said {
            continue;
        }
        match serde_json::from_str(line) {
            Ok(TranscriptLine::CustomTitle { custom_title }) if custom.is_none() => {
                if !own_name
                    .as_ref()
                    .is_some_and(|name| name.is_in(&custom_title))
                {
                    custom = Some(custom_title);
                }
            }
            Ok(TranscriptLine::AiTitle { ai_title }) if generated.is_none() => {
                generated = Some(ai_title);
            }
            Ok(TranscriptLine::Summary { summary: text }) if summary.is_none() => {
                summary = Some(text);
            }
            Ok(TranscriptLine::WorktreeState { worktree: state }) if worktree.is_none() => {
                worktree = Some(state);
            }
            Ok(TranscriptLine::Assistant {
                message,
                is_sidechain: false,
            }) if last_said.is_none() => {
                if let Some(said) = assistant_sentence(&message.content) {
                    last_said = Some(said);
                }
            }
            // The first line of the tail may be cut; other lines aren't titles.
            Ok(_) | Err(_) => {}
        }
        if custom.is_some() && worktree.is_some() && last_said.is_some() {
            break;
        }
    }
    let worktree = worktree.flatten().and_then(Worktree::existing);
    if let Some(title) = custom.or(generated).or(summary) {
        return Some(Summary {
            title: title_line(&title).to_owned(),
            worktree,
            last_said,
        });
    }
    // The first prompt; or, for a session another one started by writing to it, that message,
    // whose turn can fill more than the head before the user writes anything.
    file.seek(SeekFrom::Start(0)).ok()?;
    head_lines(file)
        .filter(|line| line.contains("\"user\""))
        .find_map(|line| match serde_json::from_str(&line) {
            Ok(TranscriptLine::User {
                is_sidechain: false,
                origin: Some(MessageOrigin::Peer { name, body }),
                ..
            }) => Some(peer_title(name.as_deref(), body.as_deref())),
            Ok(TranscriptLine::User {
                message,
                is_meta: false,
                is_compact_summary: false,
                is_sidechain: false,
                ..
            }) => prompt_text(&message.content),
            Ok(_) | Err(_) => None,
        })
        .map(|prompt| Summary {
            title: crate::tools::truncate_chars(title_line(&prompt), 80),
            worktree,
            last_said,
        })
}

/// Returns the first readable sentence an assistant turn wrote, for cards and rows.
pub fn assistant_sentence(blocks: &[crate::external::wire::ContentBlock]) -> Option<String> {
    let combined = blocks
        .iter()
        .filter_map(|block| match block {
            crate::external::wire::ContentBlock::Text { text } => Some(text.as_str()),
            crate::external::wire::ContentBlock::Thinking { .. }
            | crate::external::wire::ContentBlock::ToolUse { .. }
            | crate::external::wire::ContentBlock::ServerToolUse { .. }
            | crate::external::wire::ContentBlock::ToolResult { .. }
            | crate::external::wire::ContentBlock::Image { .. }
            | crate::external::wire::ContentBlock::Other => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    first_sentence(&combined)
}

pub fn first_sentence(text: &str) -> Option<String> {
    let mut line = String::new();
    let mut heading_fallback: Option<&str> = None;
    for raw in text.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with("```") {
            if line.is_empty() {
                continue;
            }
            break;
        }
        if trimmed.starts_with('#') {
            if line.is_empty() && heading_fallback.is_none() {
                let heading = trimmed.trim_start_matches('#').trim_start();
                if !heading.is_empty() {
                    heading_fallback = Some(heading);
                }
            }
            continue;
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(trimmed);
        if ends_sentence(&line) {
            break;
        }
    }
    let chosen = if line.is_empty() {
        heading_fallback?
    } else {
        split_first_sentence(&line)
    };
    Some(crate::tools::truncate_chars(chosen.trim(), 140))
}

fn ends_sentence(text: &str) -> bool {
    text.ends_with(['.', '!', '?', '。', '！', '？'])
}

fn split_first_sentence(text: &str) -> &str {
    let mut chars = text.char_indices().peekable();
    while let Some((at, ch)) = chars.next() {
        if matches!(ch, '。' | '！' | '？') {
            return text.get(..at + ch.len_utf8()).unwrap_or(text);
        }
        if matches!(ch, '.' | '!' | '?') {
            match chars.peek() {
                None => return text,
                Some(&(_, next)) if next.is_whitespace() => {
                    return text.get(..at + ch.len_utf8()).unwrap_or(text);
                }
                Some(_) => {}
            }
        }
    }
    text
}

/// Returns the first line with text in it, as a title is shown on one line.
fn title_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
}

/// Makes a title from another session's message: who sent it and what it says, as far as
/// known.
fn peer_title(name: Option<&str>, body: Option<&str>) -> String {
    match (name, body) {
        (Some(name), Some(body)) => format!("{name}: {}", body.trim()),
        (None, Some(body)) => body.to_owned(),
        (Some(name), None) => format!("Message from {name}"),
        (None, None) => "Message from another session".to_owned(),
    }
}

/// Returns the worktree a session works in now, as its transcript last said.
pub fn worktree_of(transcript: &Path) -> Option<Worktree> {
    summary(transcript).and_then(|found| found.worktree)
}

pub(super) fn recorded_cwd(path: &Path) -> Option<PathBuf> {
    head_lines(File::open(path).ok()?)
        .filter(|line| line.contains("\"cwd\""))
        .find_map(|line| serde_json::from_str::<TranscriptCwd>(&line).ok())
        .map(|record| record.cwd)
}

/// Returns the prompt a user message holds, if any; a prompt sent after `!` commands follows
/// their rows.
fn prompt_text(content: &UserContent) -> Option<String> {
    let (whole, blocks) = match content {
        UserContent::Text(text) => (Some(text.as_str()), [].as_slice()),
        UserContent::Blocks(blocks) => (None, blocks.as_slice()),
    };
    let block_texts = blocks.iter().filter_map(|block| match block {
        wire::ContentBlock::Text { text } => Some(text.as_str()),
        wire::ContentBlock::Thinking { .. }
        | wire::ContentBlock::ToolUse { .. }
        | wire::ContentBlock::ServerToolUse { .. }
        | wire::ContentBlock::ToolResult { .. }
        | wire::ContentBlock::Image { .. }
        | wire::ContentBlock::Other => None,
    });
    whole
        .into_iter()
        .chain(block_texts)
        .find_map(|text| match UserText::of(text) {
            UserText::Prompt(text) => Some(text.to_owned()),
            UserText::Command { .. }
            | UserText::Shell(_)
            | UserText::ShellOutput { .. }
            | UserText::Stdout(_)
            | UserText::Stderr(_)
            | UserText::Interrupted
            | UserText::ForModel
            | UserText::Empty => None,
        })
}

/// Reads the lines at the start of a transcript, only as far as they are wanted.
fn head_lines(file: File) -> impl Iterator<Item = String> {
    BufReader::new(file.take(HEAD_SCAN_BYTES))
        .lines()
        .map_while(Result::ok)
}

fn read_range(file: &mut File, start: u64, end: u64) -> Option<String> {
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(end - start).read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn edualcs_name_for_a_session_is_no_title() {
        let id = "291365c8-6d45-4dba-ba19-41a5f0eccd6b";
        let pet = PetName::of(&SessionId::try_from(id.to_owned()).expect("an id"));
        let dir = std::env::temp_dir().join(format!("edualc-titles-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let path = dir.join(format!("{id}.jsonl"));
        let title_after = |custom: &str| {
            let lines = [
                json!({ "type": "ai-title", "aiTitle": "Floating NPU screen describer" }),
                json!({ "type": "custom-title", "customTitle": custom }),
            ];
            let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
            std::fs::write(&path, text).expect("a transcript");
            summary(&path).map(|found| found.title)
        };
        for name in [
            format!("edualc-{pet}"),
            format!("edualc-local-npu-screen-con-{pet}-81"),
            format!("{pet}-local-npu-screen-con-81"),
        ] {
            assert_eq!(
                title_after(&name).as_deref(),
                Some("Floating NPU screen describer"),
                "{name}"
            );
        }
        assert_eq!(
            title_after("NPU describer, native").as_deref(),
            Some("NPU describer, native")
        );
        std::fs::remove_dir_all(&dir).expect("the scratch directory goes");
    }

    #[test]
    fn a_title_is_one_line() {
        let dir = std::env::temp_dir().join(format!("edualc-one-line-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let path = dir.join("80bdeda4-8427-4be3-b815-ea82e7490483.jsonl");
        let title_of = |line: serde_json::Value| {
            std::fs::write(&path, format!("{line}\n")).expect("a transcript");
            summary(&path).map(|found| found.title)
        };
        let prompt = json!({
            "type": "user",
            "message": { "role": "user", "content": "\n  Check the kitchen\nThen move on to the hall" },
        });
        assert_eq!(title_of(prompt).as_deref(), Some("Check the kitchen"));
        let titled = json!({ "type": "custom-title", "customTitle": "Two\nlines" });
        assert_eq!(title_of(titled).as_deref(), Some("Two"));
        std::fs::remove_dir_all(&dir).expect("cleaned up");
    }

    #[test]
    fn a_session_another_started_is_titled_by_its_message() {
        let dir = std::env::temp_dir().join(format!("edualc-peer-titles-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let path = dir.join("80bdeda4-8427-4be3-b815-ea82e7490482.jsonl");
        let title_of = |lines: &[serde_json::Value]| {
            let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
            std::fs::write(&path, text).expect("a transcript");
            summary(&path).map(|found| found.title)
        };
        let peer = json!({
            "type": "user",
            "isMeta": true,
            "message": { "role": "user", "content": "Another Claude session sent a message: …" },
            "origin": { "kind": "peer", "from": "unknown", "name": "sterling-gibbon-94",
                        "body": "Please reply with one sentence." },
        });
        let filler = json!({ "type": "attachment", "attachment": {
            "type": "skill_listing", "content": "x".repeat(usize::try_from(PEEK_BYTES).expect("small")) } });
        let prompt = json!({ "type": "user", "message": { "role": "user", "content": "Send a second note" } });
        assert_eq!(
            title_of(&[peer.clone(), filler, prompt.clone()]).as_deref(),
            Some("sterling-gibbon-94: Please reply with one sentence.")
        );
        assert_eq!(
            title_of(&[prompt, peer]).as_deref(),
            Some("Send a second note")
        );
        std::fs::remove_dir_all(&dir).expect("the scratch directory goes");
    }

    #[test]
    fn last_said_reads_the_first_sentence_of_the_last_assistant_reply() {
        let dir = std::env::temp_dir().join(format!("edualc-last-said-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let path = dir.join("80bdeda4-8427-4be3-b815-ea82e7490489.jsonl");
        let lines = [
            json!({ "type": "user", "message": { "role": "user", "content": "Check tests" } }),
            json!({
                "type": "assistant",
                "message": { "role": "assistant", "content": [{ "type": "text", "text": "Looking at that." }] }
            }),
            json!({
                "type": "assistant",
                "isSidechain": true,
                "message": { "role": "assistant", "content": [{ "type": "text", "text": "Sidechain thought." }] }
            }),
            json!({
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [{ "type": "text", "text": "## Summary\nAll three tests pass now. The cache stays untouched." }]
                }
            }),
        ];
        let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
        std::fs::write(&path, text).expect("a transcript");
        let found = summary(&path).expect("a summary");
        assert_eq!(found.title, "Check tests");
        assert_eq!(
            found.last_said.as_deref(),
            Some("All three tests pass now.")
        );
        assert_eq!(
            first_sentence("All three tests pass now. The cache stays untouched.").as_deref(),
            Some("All three tests pass now.")
        );
        assert_eq!(
            first_sentence("日本語の文です。次の文。").as_deref(),
            Some("日本語の文です。")
        );
        std::fs::remove_dir_all(&dir).expect("the scratch directory goes");
    }

    #[test]
    fn summary_survives_oversized_head_lines_on_large_transcripts() {
        let dir = std::env::temp_dir().join(format!("edualc-oversized-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let path = dir.join("80bdeda4-8427-4be3-b815-ea82e7490490.jsonl");
        let huge_snapshot = json!({
            "type": "file-history-snapshot",
            "payload": "x".repeat(256 * 1024),
        });
        let prompt = json!({
            "type": "user",
            "message": { "role": "user", "content": "Recover after huge snapshot" },
        });
        let reply = json!({
            "type": "assistant",
            "message": { "role": "assistant", "content": [{ "type": "text", "text": "Recovered cleanly." }] },
        });
        std::fs::write(&path, format!("{huge_snapshot}\n{prompt}\n{reply}\n"))
            .expect("a transcript");
        let found = summary(&path).expect("summary past >128KB head line");
        assert_eq!(found.title, "Recover after huge snapshot");
        assert_eq!(found.last_said.as_deref(), Some("Recovered cleanly."));
        std::fs::remove_dir_all(&dir).expect("the scratch directory goes");
    }
}
