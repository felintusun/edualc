//! The Anthropic API key in the desktop's keyring (the Secret Service), for Claude Code
//! sessions that sign in with it. Only the daemon reaches the keyring: it stores the key the
//! window was given, and reads it back as it starts such a session.

use crate::ipc::ApiKey;

/// What the keyring shows the key as.
const LABEL: &str = "edualc: Anthropic API key";
/// How edualc finds its key among the keyring's items.
const ATTRIBUTES: [(&str, &str); 2] = [("application", "edualc"), ("secret", "anthropic-api-key")];

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("couldn't reach the keyring: {0}")]
    Keyring(#[from] oo7::Error),
    #[error("the API key in the keyring isn't text: {0}")]
    NotText(#[source] std::str::Utf8Error),
    #[error("the API key in the keyring isn't one")]
    NotAKey,
}

/// The keyring, unlocked: the desktop asks for its password if it is locked.
async fn unlocked() -> Result<oo7::Keyring, Error> {
    let keyring = oo7::Keyring::new().await?;
    keyring.unlock().await?;
    Ok(keyring)
}

/// Keeps `key`, in place of one kept before.
pub(crate) async fn store(key: &ApiKey) -> Result<(), Error> {
    unlocked()
        .await?
        .create_item(LABEL, &ATTRIBUTES, oo7::Secret::text(key.as_str()), true)
        .await?;
    Ok(())
}

pub(crate) async fn remove() -> Result<(), Error> {
    unlocked().await?.delete(&ATTRIBUTES).await?;
    Ok(())
}

/// Whether a key is kept, told without unlocking the keyring.
pub(crate) async fn stored() -> Result<bool, Error> {
    let keyring = oo7::Keyring::new().await?;
    Ok(!keyring.search_items(&ATTRIBUTES).await?.is_empty())
}

/// The key kept, if one is.
pub(crate) async fn load() -> Result<Option<ApiKey>, Error> {
    let keyring = unlocked().await?;
    let Some(item) = keyring.search_items(&ATTRIBUTES).await?.into_iter().next() else {
        return Ok(None);
    };
    item.unlock().await?;
    let secret = item.secret().await?;
    let text = std::str::from_utf8(secret.as_bytes()).map_err(Error::NotText)?;
    ApiKey::parse(text).map(Some).ok_or(Error::NotAKey)
}
