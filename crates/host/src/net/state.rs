//! What edualcd keeps between starts in a folder of the user's alone: its certificate, its key,
//! and the token, read from there only so that no program edualcd runs inherits it.

use std::fs::{DirBuilder, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{
    DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::path::{Path, PathBuf};

use crate::ipc::Token;
use crate::ipc::pairing::Fingerprint;

const FOLDER: &str = "edualcd";
const CERTIFICATE: &str = "certificate.der";
const KEY: &str = "key.der";
const TOKEN: &str = "token";
const LISTENING: &str = "listening.json";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("there is no home folder to keep edualcd's certificate and token in")]
    NoHome,
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{0} isn't the user's alone (it must be a folder they own, closed to others)")]
    NotPrivate(PathBuf),
    #[error("couldn't make edualcd's certificate: {0}")]
    Certificate(#[from] rcgen::Error),
    #[error("{0} is empty: remove it, and a new token is made")]
    EmptyToken(PathBuf),
    #[error("{path} doesn't read: {source}")]
    Unreadable {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// Where edualcd serving the network listens, and the name it is reached by: kept as it starts,
/// so that `edualcd --pair` prints the address of the edualcd that runs (a service's, with an
/// address of its own), not that of a default.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Listening {
    pub address: std::net::SocketAddr,
    #[serde(default)]
    pub name: Option<String>,
}

impl Listening {
    /// Reads where an edualcd serving the network listened last; `None` if none has yet.
    pub fn kept() -> Result<Option<Self>, Error> {
        let path = folder()?.join(LISTENING);
        read(&path)?
            .map(|bytes| {
                serde_json::from_slice(&bytes).map_err(|source| Error::Unreadable { path, source })
            })
            .transpose()
    }

    /// Kept, for `edualcd --pair`.
    pub fn keep(&self) -> Result<(), Error> {
        let path = folder()?.join(LISTENING);
        let bytes = serde_json::to_vec(self).map_err(|source| Error::Unreadable {
            path: path.clone(),
            source,
        })?;
        write(&path, &bytes)
    }
}

/// What edualcd keeps.
pub struct State {
    /// DER.
    pub certificate: Vec<u8>,
    /// PKCS #8 DER.
    pub key: Vec<u8>,
    pub token: Token,
}

/// The token is left out.
impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("State")
            .field("fingerprint", &self.fingerprint())
            .finish_non_exhaustive()
    }
}

impl State {
    /// Reads what was kept, or makes it where nothing was: the certificate for `names`, the
    /// names and addresses windows may reach edualcd by. A pinned certificate is trusted
    /// whatever it names, but a browser shows them.
    pub fn load(names: &[String]) -> Result<Self, Error> {
        let folder = folder()?;
        let certificate_path = folder.join(CERTIFICATE);
        let key_path = folder.join(KEY);
        let (certificate, key) = match (read(&certificate_path)?, read(&key_path)?) {
            (Some(certificate), Some(key)) => (certificate, key),
            // Neither, or one without the other: both made anew.
            (Some(_) | None, Some(_) | None) => {
                let made = rcgen::generate_simple_self_signed(names.to_vec())?;
                let certificate = made.cert.der().to_vec();
                let key = made.signing_key.serialize_der();
                write(&key_path, &key)?;
                write(&certificate_path, &certificate)?;
                tracing::info!(folder = %folder.display(), "made edualcd's certificate");
                (certificate, key)
            }
        };
        let token_path = folder.join(TOKEN);
        let token = match read(&token_path)? {
            Some(token) => {
                let token = Token::new(String::from_utf8_lossy(&token).trim().to_owned());
                if token.is_empty() {
                    return Err(Error::EmptyToken(token_path));
                }
                token
            }
            None => {
                let token = Token::new(format!(
                    "{}{}",
                    uuid::Uuid::new_v4().simple(),
                    uuid::Uuid::new_v4().simple()
                ));
                write(&token_path, token.as_str().as_bytes())?;
                token
            }
        };
        Ok(Self {
            certificate,
            key,
            token,
        })
    }

    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint::of(&self.certificate)
    }
}

/// Returns the folder, made if it isn't there and checked to be yours alone. One of yours with
/// looser permissions is closed to others first.
fn folder() -> Result<PathBuf, Error> {
    let state = dirs::state_dir().ok_or(Error::NoHome)?;
    let folder = state.join(FOLDER);
    let io = |source| Error::Io {
        path: folder.clone(),
        source,
    };
    std::fs::create_dir_all(&state).map_err(|source| Error::Io {
        path: state.clone(),
        source,
    })?;
    match DirBuilder::new().mode(0o700).create(&folder) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(io(error)),
    }
    let metadata = std::fs::symlink_metadata(&folder).map_err(io)?;
    if !metadata.is_dir() || metadata.uid() != rustix::process::getuid().as_raw() {
        return Err(Error::NotPrivate(folder));
    }
    if metadata.mode() & 0o077 != 0 {
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700)).map_err(io)?;
    }
    Ok(folder)
}

/// Reads a file's bytes without following a link; `None` if it isn't there.
fn read(path: &Path) -> Result<Option<Vec<u8>>, Error> {
    let opened = OpenOptions::new()
        .read(true)
        .custom_flags(nofollow())
        .open(path);
    match opened {
        Ok(mut file) => {
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut file, &mut bytes).map_err(|source| Error::Io {
                path: path.to_owned(),
                source,
            })?;
            Ok(Some(bytes))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

/// Writes a file the user alone reads, in place of whatever was there.
fn write(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let io = |source| Error::Io {
        path: path.to_owned(),
        source,
    };
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io(error)),
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(io)?;
    file.write_all(bytes).map_err(io)?;
    file.sync_all().map_err(io)
}

/// Returns `O_NOFOLLOW` as `OpenOptions` takes it.
fn nofollow() -> i32 {
    i32::try_from(rustix::fs::OFlags::NOFOLLOW.bits()).unwrap_or_default()
}
