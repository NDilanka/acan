//! Model keys a tester saves from the setup page: the status the page shows, the file the key is
//! kept in, and the provider check that runs before a key is written.
//!
//! A key is sent only to the `base_url` of its provider. Nothing here prints, logs or returns the
//! key. A save reports its outcome, never the key. The file is private to its user on Unix: mode
//! 0600 in a folder that only the user can enter. On Windows it sits in the user's profile folder,
//! which only that user can read by default.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{ConfigError, LlmConfig};
use crate::key::ApiKey;
use crate::preset::{KeyCheck, check_key};

/// The file the saved key is kept in, inside the acan folder of the user's config directory.
pub const KEYS_FILE: &str = "keys.toml";

/// The longest key a save accepts. Provider keys are far shorter.
const MAX_KEY_LEN: usize = 512;

/// The header written above a saved key, so a tester who opens the file knows what it is.
const HEADER: &str = "# The model key saved from the acan setup page. Keep this file private.\n";

/// Where the live model client gets its keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeySource {
    /// The repository's routing file names the providers. Their keys come from the environment
    /// or the repository's `.env`.
    RoutingFile,
    /// A key saved from the setup page, for the provider it was saved for.
    SavedKey,
}

/// What the setup page shows: whether model calls have a key, and which provider it belongs to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct KeyStatus {
    /// Whether a live model client with a key is in use. `false` means calls get built-in replies.
    pub configured: bool,
    /// The provider a saved key belongs to. `None` when a routing file chose the providers.
    pub provider: Option<String>,
    /// Where the calls get their providers and keys: a routing file, or a saved key. `None` when
    /// neither is set, so calls get built-in replies.
    pub source: Option<KeySource>,
    /// Whether the setup page can save a key here. A routing file decides the providers itself,
    /// so it cannot.
    pub can_save: bool,
}

/// What a key check and save found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyOutcome {
    /// The key is saved. `checked` is `false` when the provider answered without showing whether
    /// the key works, so it was saved unverified.
    Stored { checked: bool },
    /// The provider refused the key. Nothing was saved.
    Rejected,
    /// The provider could not be reached. Nothing was saved.
    Unreachable,
}

/// Why a key could not be saved at all.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SetupError {
    #[error("this server does not take keys from the setup page")]
    Unavailable,

    #[error("the repository's routing file chooses the providers; set their keys in its .env")]
    RoutingFile,

    #[error("the provider is not one the setup page offers")]
    UnknownProvider,

    #[error("the key is empty, too long, or not a single line of text")]
    InvalidKey,

    #[error("cannot write the keys file")]
    Write(#[source] io::Error),

    #[error(transparent)]
    Client(#[from] ConfigError),
}

/// A key read from the keys file: the provider it belongs to, and the key.
#[derive(Debug)]
pub struct SavedKey {
    pub provider: String,
    pub key: ApiKey,
}

/// The per-user file that holds the saved key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyFile {
    path: PathBuf,
}

impl KeyFile {
    /// A keys file at `path`. Nothing is read or created until it is used.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The keys file of the current user: `keys.toml` in the acan folder of the user's config
    /// directory. `None` when that directory cannot be found.
    #[must_use]
    pub fn for_user() -> Option<Self> {
        config_dir().map(|dir| Self::new(dir.join("acan").join(KEYS_FILE)))
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads the saved key. `Ok(None)` when there is no file.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read, or does not hold one provider and one key.
    /// The error names no part of the file, since a parser error can quote the text it failed on.
    pub fn load(&self) -> io::Result<Option<SavedKey>> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Stored {
            provider: String,
            key: String,
        }

        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        let stored: Stored = toml::from_str(&text).map_err(|_| not_a_key())?;
        if stored.key.trim().is_empty() || stored.provider.trim().is_empty() {
            return Err(not_a_key());
        }
        Ok(Some(SavedKey {
            provider: stored.provider,
            key: ApiKey::new(stored.key),
        }))
    }

    /// Replaces the saved key with `key` for `provider`. The file is written to a temporary
    /// name and renamed over the old one, so a crash leaves the old key or the new one, never a
    /// half-written file.
    ///
    /// # Errors
    ///
    /// Returns the I/O error of the write. A failed write leaves no temporary file behind.
    pub fn save(&self, provider: &str, key: &str) -> io::Result<()> {
        #[derive(Serialize)]
        struct Stored<'a> {
            provider: &'a str,
            key: &'a str,
        }

        let body = toml::to_string(&Stored { provider, key })
            .map_err(|_| io::Error::other("cannot encode the keys file"))?;
        let dir = self
            .path
            .parent()
            .ok_or_else(|| io::Error::other("the keys file has no folder"))?;
        create_private_dir(dir)?;

        let temp = dir.join(format!(".{KEYS_FILE}.tmp"));
        let _ = fs::remove_file(&temp);
        let written = write_private(&temp, &format!("{HEADER}{body}"))
            .and_then(|()| fs::rename(&temp, &self.path));
        if written.is_err() {
            let _ = fs::remove_file(&temp);
        }
        written
    }
}

/// Checks `key` with `provider` and saves it unless the provider refuses it. `config` names the
/// provider and its base URL, and the key is sent only to that URL. A refused or unreachable key
/// is not saved.
///
/// # Errors
///
/// Returns [`SetupError::UnknownProvider`] when `config` has no such provider, and
/// [`SetupError::InvalidKey`] when the key is empty, too long, or has whitespace or control
/// characters. Both are checked before any request is sent. Returns [`SetupError::Write`] when the
/// key is accepted but the file cannot be written.
pub async fn save_checked(
    file: &KeyFile,
    config: &LlmConfig,
    provider: &str,
    key: &str,
) -> Result<KeyOutcome, SetupError> {
    let known = config
        .providers()
        .iter()
        .find(|known| known.name == provider)
        .ok_or(SetupError::UnknownProvider)?;
    if !valid_key(key) {
        return Err(SetupError::InvalidKey);
    }
    let checked = match check_key(&known.base_url, key).await {
        KeyCheck::Refused => return Ok(KeyOutcome::Rejected),
        KeyCheck::Unreachable => return Ok(KeyOutcome::Unreachable),
        KeyCheck::Accepted => true,
        KeyCheck::Unchecked => false,
    };
    file.save(provider, key).map_err(SetupError::Write)?;
    Ok(KeyOutcome::Stored { checked })
}

fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_KEY_LEN
        && !key.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn not_a_key() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "the keys file does not hold one provider and one key",
    )
}

/// The user's config directory: `%APPDATA%` on Windows, `~/Library/Application Support` on macOS,
/// and `$XDG_CONFIG_HOME` or `~/.config` elsewhere.
fn config_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        return std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute());
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())?;
    if cfg!(target_os = "macos") {
        return Some(home.join("Library").join("Application Support"));
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| Some(home.join(".config")))
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// Creates `path` as a new file only this user can read, and writes `text` to it.
fn write_private(path: &Path, text: &str) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()
}
