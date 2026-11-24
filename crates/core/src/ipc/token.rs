//! The token a line shows to be let in: cleared from memory as it goes, never shown by `Debug`
//! or `Display`, and compared in constant time.

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

/// A token.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct Token(Zeroizing<String>);

impl Token {
    #[must_use]
    pub fn new(token: String) -> Self {
        Self(Zeroizing::new(token))
    }

    /// Returns the token as text, to put in an address or a file.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns whether `other` is this token. We compare the SHA-256 of each in constant time,
    /// so that how long this takes tells neither the length nor where they differ.
    #[must_use]
    pub fn admits(&self, other: &Self) -> bool {
        let ours = Sha256::digest(self.0.as_bytes());
        let theirs = Sha256::digest(other.0.as_bytes());
        ours.ct_eq(&theirs).into()
    }
}

/// Equal as `admits` has it: in constant time.
impl PartialEq for Token {
    fn eq(&self, other: &Self) -> bool {
        self.admits(other)
    }
}

impl Eq for Token {}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(..)")
    }
}

/// What a line says first, and nothing else until edualcd has let it in: the token, in a frame
/// of its own, so that it is in no address (addresses are logged by proxies and servers).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Auth {
    pub token: Token,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_same_token_is_admitted() {
        let token = Token::new("secret".to_owned());
        assert!(token.admits(&Token::new("secret".to_owned())));
        assert!(!token.admits(&Token::new("secreT".to_owned())));
        assert!(!token.admits(&Token::new("secret ".to_owned())));
        assert!(!token.admits(&Token::new(String::new())));
    }

    #[test]
    fn a_token_is_never_shown() {
        let token = Token::new("to ken".to_owned());
        assert!(!format!("{token:?}").contains("to ken"));
        let auth = Auth { token };
        assert!(!format!("{auth:?}").contains("to ken"));
    }
}
