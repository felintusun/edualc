//! A folder's trust for Claude Code, as its record in `~/.claude.json` keeps it.

use std::path::{Path, PathBuf};

use crate::external::claude_files::{ClaudeJsonWhole, ProjectWhole};
use crate::sessions::{self, ClaudeHome};

use super::write_atomically;

/// Why a folder's trust wasn't recorded for Claude Code.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ClaudeTrustError {
    #[error("couldn't read {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{0} is too large to be Claude Code's settings")]
    TooLarge(PathBuf),
    #[error("{path} doesn't read as Claude Code's settings")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("couldn't write Claude Code's settings")]
    Encode(#[source] serde_json::Error),
    #[error("couldn't write {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Claude Code kept {0} locked")]
    Busy(PathBuf),
    #[error("couldn't lock {path}")]
    Lock {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// How long we wait for Claude Code's lock on `~/.claude.json`, which it holds for moments.
const CLAUDE_LOCK_WAITS: [std::time::Duration; 4] = [
    std::time::Duration::from_millis(100),
    std::time::Duration::from_millis(200),
    std::time::Duration::from_millis(400),
    std::time::Duration::from_millis(800),
];
/// How old a lock is when Claude Code takes it over as stale: proper-lockfile's default, which
/// Claude Code 2.1.323 keeps, refreshing a lock it holds every 5 s.
const CLAUDE_LOCK_STALE: std::time::Duration = std::time::Duration::from_secs(10);

/// What recording a folder's trust for Claude Code came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaudeTrust {
    /// Its record says it is trusted now.
    Recorded,
    /// Its record said so already, so nothing was written.
    AlreadyTrusted,
}

/// Returns whether Claude Code applies `folder`'s own settings, which it does once the
/// folder's record says it is trusted.
pub(crate) fn trusted_for_claude_code(
    home: &ClaudeHome,
    folder: &Path,
) -> Result<bool, ClaudeTrustError> {
    let config = home.global_config();
    let path = std::fs::canonicalize(config).unwrap_or_else(|_| config.to_owned());
    Ok(read_claude_json(&path)?
        .projects
        .get(&claude_project_key(folder))
        .is_some_and(|project| project.accepted == Some(true)))
}

/// Records `folder` as trusted for Claude Code, as its trust dialog would
/// (`hasTrustDialogAccepted` in `~/.claude.json`). Claude Code applies a folder's own permission
/// rules only then.
///
/// We write the file as Claude Code does: under its lock, read afresh, with that one record
/// changed and the rest kept. Nothing is written if the record says so already.
pub(crate) fn trust_for_claude_code(
    home: &ClaudeHome,
    folder: &Path,
) -> Result<ClaudeTrust, ClaudeTrustError> {
    trust_in(
        home.global_config(),
        &claude_project_key(folder),
        &CLAUDE_LOCK_WAITS,
    )
}

/// Reads `~/.claude.json` at `path`, following a link; an empty one where there is none.
fn read_claude_json(path: &Path) -> Result<ClaudeJsonWhole, ClaudeTrustError> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.len() > sessions::GLOBAL_CONFIG_BYTES => {
            Err(ClaudeTrustError::TooLarge(path.to_owned()))
        }
        Ok(_) => {
            let text = std::fs::read_to_string(path).map_err(|source| ClaudeTrustError::Read {
                path: path.to_owned(),
                source,
            })?;
            serde_json::from_str(&text).map_err(|source| ClaudeTrustError::Parse {
                path: path.to_owned(),
                source,
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(ClaudeJsonWhole::default())
        }
        Err(source) => Err(ClaudeTrustError::Read {
            path: path.to_owned(),
            source,
        }),
    }
}

/// Returns the key Claude Code 2.1.323 keeps a folder's record under: the real path of the
/// git repository it is in, or of the folder itself outside one.
fn claude_project_key(folder: &Path) -> PathBuf {
    let real = std::fs::canonicalize(folder).unwrap_or_else(|_| folder.to_owned());
    real.ancestors()
        .find(|place| place.join(".git").exists())
        .map_or_else(|| real.clone(), Path::to_path_buf)
}

/// Claude Code's lock on its config, as it takes it: a folder beside the file while held.
struct ClaudeLock(PathBuf);

impl ClaudeLock {
    /// Takes the lock, waiting `waits` in turn while Claude Code holds it. A lock left longer
    /// than [`CLAUDE_LOCK_STALE`] is taken over, as Claude Code does.
    fn take(config: &Path, waits: &[std::time::Duration]) -> Result<Self, ClaudeTrustError> {
        let mut path = config.as_os_str().to_owned();
        path.push(".lock");
        let path = PathBuf::from(path);
        let mut waits = waits.iter();
        loop {
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&path)
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age > CLAUDE_LOCK_STALE);
                    if stale {
                        tracing::info!(lock = %path.display(), "a stale lock of Claude Code's taken over");
                        if let Err(error) = std::fs::remove_dir(&path)
                            && error.kind() != std::io::ErrorKind::NotFound
                        {
                            return Err(ClaudeTrustError::Lock {
                                path,
                                source: error,
                            });
                        }
                        continue;
                    }
                    let Some(wait) = waits.next() else {
                        return Err(ClaudeTrustError::Busy(path));
                    };
                    std::thread::sleep(*wait);
                }
                Err(source) => return Err(ClaudeTrustError::Lock { path, source }),
            }
        }
    }
}

impl Drop for ClaudeLock {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir(&self.0) {
            tracing::warn!(%error, lock = %self.0.display(), "Claude Code's lock wasn't let go");
        }
    }
}

fn trust_in(
    config: &Path,
    key: &Path,
    waits: &[std::time::Duration],
) -> Result<ClaudeTrust, ClaudeTrustError> {
    let _lock = ClaudeLock::take(config, waits)?;
    // A link (a dotfiles repository's, say) is followed, and the file it names written.
    let path = std::fs::canonicalize(config).unwrap_or_else(|_| config.to_owned());
    let mut whole = read_claude_json(&path)?;
    // Claude Code reads a folder's settings by its own record alone: trust on a folder above it
    // doesn't count.
    match whole.projects.get_mut(key) {
        Some(project) if project.accepted == Some(true) => return Ok(ClaudeTrust::AlreadyTrusted),
        Some(project) => project.accepted = Some(true),
        None => {
            let project = ProjectWhole::trusted().map_err(ClaudeTrustError::Encode)?;
            whole.projects.insert(key.to_owned(), project);
        }
    }
    let text = serde_json::to_vec_pretty(&whole).map_err(ClaudeTrustError::Encode)?;
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(ClaudeTrustError::Write {
            path,
            source: std::io::Error::from(std::io::ErrorKind::InvalidInput),
        });
    };
    write_atomically(parent, name, &text).map_err(|source| ClaudeTrustError::Write {
        path: path.clone(),
        source,
    })?;
    Ok(ClaudeTrust::Recorded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_is_recorded_for_claude_code_keeping_the_rest() {
        let dir = std::env::temp_dir().join(format!("edualc-claude-json-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch folder");
        let config = dir.join(".claude.json");
        let before = serde_json::json!({
            "numStartups": 7,
            "mcpServers": {"tool": {"command": "x", "env": {"TOKEN": "secret"}}},
            "projects": {
                "/code/other": {"allowedTools": [], "lastCost": 0.5},
                "/code/trusted": {"hasTrustDialogAccepted": true}
            }
        });
        std::fs::write(&config, before.to_string()).expect("a config");

        assert!(matches!(
            trust_in(&config, Path::new("/code/app"), &[]),
            Ok(ClaudeTrust::Recorded)
        ));
        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).expect("read")).expect("json");
        let project = |json: &serde_json::Value, folder: &str| {
            json.get("projects")
                .and_then(|projects| projects.get(folder))
                .cloned()
        };
        let app = project(&after, "/code/app").expect("the record");
        assert_eq!(
            app.get("hasTrustDialogAccepted"),
            Some(&serde_json::Value::Bool(true))
        );
        assert_eq!(app.get("allowedTools"), Some(&serde_json::json!([])));
        assert_eq!(
            project(&after, "/code/other"),
            project(&before, "/code/other")
        );
        assert_eq!(after.get("mcpServers"), before.get("mcpServers"));
        assert_eq!(after.get("numStartups"), before.get("numStartups"));

        let written = std::fs::read(&config).expect("read");
        assert!(matches!(
            trust_in(&config, Path::new("/code/app"), &[]),
            Ok(ClaudeTrust::AlreadyTrusted)
        ));
        assert_eq!(
            std::fs::read(&config).expect("read"),
            written,
            "not written again"
        );
        assert!(matches!(
            trust_in(&config, Path::new("/code/trusted/sub"), &[]),
            Ok(ClaudeTrust::Recorded)
        ));

        std::fs::write(&config, "{ not json").expect("broken");
        assert!(matches!(
            trust_in(&config, Path::new("/code/app"), &[]),
            Err(ClaudeTrustError::Parse { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(&config).expect("read"),
            "{ not json"
        );
        std::fs::remove_dir_all(&dir).expect("the scratch folder goes");
    }

    #[test]
    fn trust_is_recorded_under_claude_codes_lock() {
        let dir = std::env::temp_dir().join(format!("edualc-claude-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch folder");
        let config = dir.join(".claude.json");
        std::fs::write(&config, "{}").expect("a config");
        let lock = dir.join(".claude.json.lock");

        std::fs::create_dir(&lock).expect("Claude Code's lock");
        assert!(matches!(
            trust_in(&config, Path::new("/code/app"), &[]),
            Err(ClaudeTrustError::Busy(_))
        ));
        assert_eq!(std::fs::read_to_string(&config).expect("read"), "{}");
        assert!(lock.exists(), "Claude Code's lock is left as it is");

        let long_ago = std::time::SystemTime::now() - CLAUDE_LOCK_STALE * 2;
        std::fs::File::open(&lock)
            .and_then(|held| held.set_modified(long_ago))
            .expect("the lock made old");
        assert!(matches!(
            trust_in(&config, Path::new("/code/app"), &[]),
            Ok(ClaudeTrust::Recorded)
        ));
        assert!(!lock.exists(), "the lock is let go");
        std::fs::remove_dir_all(&dir).expect("the scratch folder goes");
    }

    #[test]
    fn a_folder_is_keyed_as_claude_code_keys_it() {
        let dir = std::env::temp_dir().join(format!("edualc-claude-key-{}", std::process::id()));
        let repository = dir.join("repo");
        let inside = repository.join("crates/app");
        let outside = dir.join("loose");
        for folder in [&inside, &repository.join(".git"), &outside] {
            std::fs::create_dir_all(folder).expect("a scratch folder");
        }
        let real = |path: &Path| std::fs::canonicalize(path).expect("real");
        assert_eq!(claude_project_key(&inside), real(&repository));
        assert_eq!(claude_project_key(&outside), real(&outside));
        std::fs::remove_dir_all(&dir).expect("the scratch folder goes");
    }
}
