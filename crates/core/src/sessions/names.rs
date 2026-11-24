//! The names a session goes by, picked by its id.

use std::path::Path;

use crate::ids::SessionId;

/// A session's two words, an adjective and a noun picked by its id, so they stay the same each
/// time it runs. edualcd names a session's process by them, and Claude Code keeps that name as
/// the session's custom title too, which is then a name and not a title.
#[derive(Debug, Clone, PartialEq, Eq, derive_more::Display)]
pub struct PetName(String);

impl PetName {
    pub fn of(id: &SessionId) -> Self {
        fn word<'list>(list: &[&'list str], number: usize) -> &'list str {
            list.get(number % list.len().max(1))
                .copied()
                .unwrap_or("session")
        }
        let words = petname::Petnames::small();
        Self(format!(
            "{}-{}",
            word(&words.adjectives, id_number(id, 0)),
            word(&words.nouns, id_number(id, 8))
        ))
    }

    /// Returns whether `text` holds the two words, as every name edualc gave the session does.
    pub(super) fn is_in(&self, text: &str) -> bool {
        text.contains(self.0.as_str())
    }
}

/// The name other sessions message a session's process by (`--name`). It stays the same each
/// time the session runs; Claude Code's own name comes from the process id and changes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, derive_more::Display)]
pub struct MessagingName(String);

impl MessagingName {
    /// Returns the name of session `id` in `cwd`: its [`PetName`], the folder's name, and two
    /// digits picked by the id.
    pub fn of(id: &SessionId, cwd: &Path) -> Self {
        Self(format!(
            "{}-{}-{:02}",
            PetName::of(id),
            project_part(cwd),
            id_number(id, 16) % 100
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The most of the folder's name a session's name takes.
const PROJECT_PART_CHARS: usize = 20;

/// Returns the folder's name as Claude Code writes it in names (`claude_but_sane` as
/// `claude-but-sane`): lowercase letters and digits, the rest a dash, at most
/// [`PROJECT_PART_CHARS`]. A folder with no such letters gives "project".
fn project_part(cwd: &Path) -> String {
    let folder = cwd
        .file_name()
        .map_or_default(|name| name.to_string_lossy().to_lowercase());
    let mut part = String::new();
    for character in folder.chars() {
        if character.is_ascii_alphanumeric() {
            part.push(character);
        } else if !part.is_empty() && !part.ends_with('-') {
            part.push('-');
        }
    }
    let part: String = part.chars().take(PROJECT_PART_CHARS).collect();
    match part.trim_end_matches('-') {
        "" => "project".to_owned(),
        trimmed => trimmed.to_owned(),
    }
}

/// Returns a number from a session's id: eight of its hex digits, from the `at`th on.
pub fn id_number(id: &SessionId, at: usize) -> usize {
    let digits: String = id
        .as_str()
        .chars()
        .filter(char::is_ascii_hexdigit)
        .collect();
    digits
        .get(at..at + 8)
        .and_then(|hex| u32::from_str_radix(hex, 16).ok())
        .and_then(|number| usize::try_from(number).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_is_named_by_its_project_and_id() {
        let id = |text: &str| SessionId::try_from(text.to_owned()).expect("a session id");
        let project = Path::new("/home/someone/code/claude_but_sane");
        let session = "d1ba2743-c47a-4446-aefc-d4594dbc045d";
        let name = MessagingName::of(&id(session), project);
        assert_eq!(
            name,
            MessagingName::of(&id(session), project),
            "started again"
        );
        let mut parts = name.0.splitn(3, '-');
        let (Some(adjective), Some(noun), Some(rest)) = (parts.next(), parts.next(), parts.next())
        else {
            panic!("two words first in {name}");
        };
        let words = petname::Petnames::small();
        assert!(words.adjectives.contains(&adjective), "{name}");
        assert!(words.nouns.contains(&noun), "{name}");
        let number = rest
            .strip_prefix("claude-but-sane-")
            .expect("named after its project");
        assert!(
            number.len() == 2 && number.chars().all(|digit| digit.is_ascii_digit()),
            "{name}"
        );
        let names: std::collections::HashSet<MessagingName> = (0..200)
            .map(|_| MessagingName::of(&SessionId::new_random(), project))
            .collect();
        assert!(names.len() > 195, "only {} names of 200", names.len());
    }

    #[test]
    fn a_project_is_named_after_its_folder() {
        assert_eq!(
            project_part(Path::new("/code/claude_but_sane")),
            "claude-but-sane"
        );
        assert_eq!(
            project_part(Path::new("/code/My Project.v2")),
            "my-project-v2"
        );
        assert_eq!(
            project_part(Path::new("/code/a-very-long-project-name-indeed")),
            "a-very-long-project"
        );
        assert_eq!(project_part(Path::new("/home/someone/文档")), "project");
        assert_eq!(project_part(Path::new("/")), "project");
    }
}
