//! The edualcds the Android app is paired with, in a file of the user's alone: a token lets
//! whoever holds it run the agents as the user on that machine.

use std::collections::BTreeMap;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};

use super::pairing::Server;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} doesn't read: {source}")]
    Unreadable {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "no edualcd is paired yet. Start one (systemctl --user enable --now edualcd), then pair \
         with the address `edualcd --pair` prints"
    )]
    Unpaired,
    #[error("no edualcd is paired as {0:?}; paired: {1}")]
    Unknown(String, String),
    #[error(
        "{0:?} is the name of another edualcd already: choose another, or forget that one first"
    )]
    Taken(String),
}

/// What the file holds.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Servers {
    /// The one reached unless another is named; the last paired.
    #[serde(default)]
    default: Option<String>,
    #[serde(default)]
    servers: BTreeMap<String, Server>,
}

impl Servers {
    /// Reads what is kept at `path`; empty if there is no file yet.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let path = path.to_owned();
        match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|source| Error::Unreadable { path, source })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(Error::Io { path, source }),
        }
    }

    /// Checks that `server` may be paired as `name`. A name belongs to one edualcd, and pairing
    /// over it would silently lose that one's token; what is reached the same way may take its
    /// own name again, with a new token.
    ///
    /// # Errors
    ///
    /// Fails if another edualcd is paired as `name`.
    pub fn free(&self, name: &str, server: &Server) -> Result<(), Error> {
        match self.servers.get(name) {
            Some(paired) if !paired.same_place(server) => Err(Error::Taken(name.to_owned())),
            Some(_) | None => Ok(()),
        }
    }

    /// Pairs with `server` as `name`, which becomes the one reached unless another is named.
    pub fn pair(&mut self, name: String, server: Server) -> Result<(), Error> {
        self.free(&name, &server)?;
        self.servers.insert(name.clone(), server);
        self.default = Some(name);
        Ok(())
    }

    /// Makes the edualcd paired as `name` the one reached unless another is named.
    pub fn choose(&mut self, name: &str) {
        if self.servers.contains_key(name) {
            self.default = Some(name.to_owned());
        }
    }

    /// Returns the name to pair `server` as when none is given: its host's, unless another is
    /// paired by that name already (another port there, or a demo where an edualcd listened).
    /// Then `host:port` tells them apart, and `host:port demo` a demo from an edualcd there.
    ///
    /// Pairing again with what is reached the same way takes its name: a new token replaces the
    /// old, and a demo never replaces an edualcd's token.
    pub fn name_for(&self, server: &Server) -> String {
        let authority = server.authority();
        let names = [
            server.host().to_owned(),
            authority.clone(),
            format!("{authority} demo"),
            format!("{authority} paired"),
        ];
        let free = names.iter().find(|name| {
            self.servers
                .get(name.as_str())
                .is_none_or(|paired| paired.same_place(server))
        });
        free.cloned().unwrap_or(authority)
    }

    /// Forgets the edualcd paired as `name`; if it was the default, another becomes it.
    pub fn forget(&mut self, name: &str) {
        self.servers.remove(name);
        if self.default.as_deref() == Some(name) {
            self.default = self.servers.keys().next().cloned();
        }
    }

    /// Lists those paired, by name.
    pub fn paired(&self) -> impl Iterator<Item = (&String, &Server)> {
        self.servers.iter()
    }

    /// Returns the name of the one reached unless another is named.
    pub fn default_name(&self) -> Option<&str> {
        self.default.as_deref()
    }

    /// Returns the edualcd named, or the default one.
    pub fn chosen(&self, name: Option<&str>) -> Result<(String, Server), Error> {
        let name = name
            .map(str::to_owned)
            .or_else(|| self.default.clone())
            .or_else(|| self.servers.keys().next().cloned())
            .ok_or(Error::Unpaired)?;
        match self.servers.get(&name) {
            Some(pairing) => Ok((name, pairing.clone())),
            None if self.servers.is_empty() => Err(Error::Unpaired),
            None => Err(Error::Unknown(
                name,
                self.servers.keys().cloned().collect::<Vec<_>>().join(", "),
            )),
        }
    }

    /// Writes the file at `path` anew, readable by you alone.
    pub fn save(&self, path: &Path) -> Result<(), Error> {
        let path = path.to_owned();
        let io = |source| Error::Io {
            path: path.clone(),
            source,
        };
        if let Some(folder) = path.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(folder)
                .map_err(io)?;
        }
        let text = serde_json::to_vec_pretty(self).map_err(|source| Error::Unreadable {
            path: path.clone(),
            source,
        })?;
        // Only the user reads it: it holds tokens.
        crate::replace::replace(&path, &text, 0o600).map_err(io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::pairing::{Demo, Fingerprint, Pairing};

    fn pairing(host: &str) -> Server {
        Server::Paired(Pairing {
            host: host.to_owned(),
            port: 7878,
            fingerprint: Fingerprint::of(host.as_bytes()),
            token: crate::ipc::Token::new("t".to_owned()),
        })
    }

    #[test]
    fn the_last_paired_is_the_default() {
        let mut servers = Servers::default();
        assert!(matches!(servers.chosen(None), Err(Error::Unpaired)));
        servers
            .pair("here".to_owned(), pairing("127.0.0.1"))
            .expect("a free name");
        servers
            .pair("box".to_owned(), pairing("box"))
            .expect("a free name");
        assert_eq!(
            servers.chosen(None).map(|(name, _)| name).ok(),
            Some("box".to_owned())
        );
        assert_eq!(
            servers
                .chosen(Some("here"))
                .map(|(_, server)| server.host().to_owned())
                .ok(),
            Some("127.0.0.1".to_owned())
        );
        assert!(matches!(
            servers.chosen(Some("elsewhere")),
            Err(Error::Unknown(name, paired)) if name == "elsewhere" && paired == "box, here"
        ));
    }

    #[test]
    fn a_name_taken_by_another_address_is_told_apart() {
        let mut servers = Servers::default();
        let demo = Server::Demo(Demo {
            host: "box".to_owned(),
            port: 7979,
        });
        assert_eq!(servers.name_for(&demo), "box");
        servers
            .pair(servers.name_for(&demo), demo.clone())
            .expect("a free name");
        assert_eq!(servers.name_for(&pairing("box")), "box:7878");
        assert_eq!(servers.name_for(&demo), "box");
        // A demo where an edualcd was paired doesn't take its name, nor its token's place.
        let mut servers = Servers::default();
        servers
            .pair("box".to_owned(), pairing("box"))
            .expect("a free name");
        let there = Server::Demo(Demo {
            host: "box".to_owned(),
            port: 7878,
        });
        assert_eq!(servers.name_for(&there), "box:7878");
        servers
            .pair(servers.name_for(&there), there.clone())
            .expect("a free name");
        assert_eq!(servers.name_for(&pairing("box")), "box");
        assert_eq!(servers.name_for(&there), "box:7878");
    }

    #[test]
    fn forgetting_the_default_moves_it() {
        let mut servers = Servers::default();
        servers
            .pair("here".to_owned(), pairing("127.0.0.1"))
            .expect("a free name");
        servers
            .pair("box".to_owned(), pairing("box"))
            .expect("a free name");
        servers.forget("box");
        assert_eq!(servers.default_name(), Some("here"));
        servers.forget("here");
        assert_eq!(servers.default_name(), None);
        assert!(matches!(servers.chosen(None), Err(Error::Unpaired)));
    }

    #[test]
    fn a_name_paired_already_is_refused() {
        let mut servers = Servers::default();
        servers
            .pair("home".to_owned(), pairing("a"))
            .expect("a free name");
        assert!(matches!(
            servers.pair("home".to_owned(), pairing("b")),
            Err(Error::Taken(name)) if name == "home"
        ));
        assert_eq!(
            servers
                .chosen(Some("home"))
                .map(|(_, server)| server.host().to_owned())
                .ok(),
            Some("a".to_owned())
        );
        servers
            .pair("home".to_owned(), pairing("a"))
            .expect("its own name");
    }

    #[test]
    fn the_file_reads_back_and_refuses_unknown_keys() {
        let mut servers = Servers::default();
        servers
            .pair("box".to_owned(), pairing("box"))
            .expect("a free name");
        servers
            .pair(
                "demo".to_owned(),
                Server::Demo(Demo {
                    host: "10.0.2.2".to_owned(),
                    port: 7878,
                }),
            )
            .expect("a free name");
        let text = serde_json::to_string(&servers).expect("written");
        let back: Servers = serde_json::from_str(&text).expect("read back");
        assert_eq!(back.chosen(None).ok(), servers.chosen(None).ok());
        assert!(serde_json::from_str::<Servers>(r#"{"servers": {}, "extra": 1}"#).is_err());
    }
}
