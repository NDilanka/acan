//! Model keys and role picks a tester saves from the setup page: the keys file, the provider check
//! that runs before a key is written, and the status the page shows.
//!
//! A tester keys any number of providers, one key each, and may pick the provider a role uses. The
//! keys file holds both. A key is sent only to the `base_url` of its own provider. Nothing here
//! prints, logs or returns a key. A save reports its outcome, never the key. The file is private to
//! its user on Unix: mode 0600 in a folder that only the user can enter. On Windows it sits in the
//! user's profile folder, which only that user can read by default.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use crate::config::{ConfigError, LlmConfig};
use crate::key::ApiKey;
use crate::plan::{ProviderStatus, RoleStatus};
use crate::providers::{KeyCheck, check_key, is_provider};
use crate::role::Role;

/// The file the saved keys and picks are kept in, inside the acan folder of the user's config
/// directory.
pub const KEYS_FILE: &str = "keys.toml";

/// The longest key a save accepts. Provider keys are far shorter.
const MAX_KEY_LEN: usize = 512;

/// The header written above the saved setup, so a tester who opens the file knows what it is.
const HEADER: &str =
    "# The model keys and role picks saved from the acan setup page. Keep this file private.\n";

/// Serializes the read-modify-write of the keys file within this process, so two saves at once
/// keep both.
static WRITE: Mutex<()> = Mutex::new(());

/// Where the live model client gets its providers and keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeySource {
    /// The repository's routing file names the providers. Their keys come from the environment
    /// or the repository's `.env`.
    RoutingFile,
    /// The setup page's saved keys and role picks, with any keys the environment sets.
    Setup,
}

/// What the setup page shows: whether model calls go to live models, which providers have keys,
/// and which provider serves each role.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct KeyStatus {
    /// Whether every role has a provider, so calls go to live models. `false` means calls get
    /// built-in replies.
    pub configured: bool,
    /// Where the providers and keys come from. `None` when no key is set.
    pub source: Option<KeySource>,
    /// Whether the setup page can save keys and picks here. A routing file decides them itself,
    /// so it cannot.
    pub can_save: bool,
    /// One entry per provider the page offers. A routing file lists the providers it names.
    pub providers: Vec<ProviderStatus>,
    /// One entry per role. A role that is off says why.
    pub roles: Vec<RoleStatus>,
}

impl KeyStatus {
    /// The status of `role`, or `None` when this status does not list it.
    #[must_use]
    pub fn role(&self, role: Role) -> Option<&RoleStatus> {
        self.roles.iter().find(|status| status.role == role)
    }
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

/// Why a key or pick could not be saved at all.
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

    #[error("that role needs a Meta key, and no other provider can serve it")]
    NeedsMeta,

    #[error("no key is set for that provider")]
    NoKey,

    #[error("cannot write the keys file")]
    Write(#[source] io::Error),

    #[error(transparent)]
    Client(#[from] ConfigError),
}

/// The keys and role picks a tester saved.
#[derive(Debug, Default)]
pub struct SavedSetup {
    /// One key per provider, by provider name.
    pub keys: BTreeMap<String, ApiKey>,
    /// The provider a role is picked to use. A role with no entry takes the default.
    pub roles: BTreeMap<Role, String>,
}

/// The per-user file that holds the saved keys and picks.
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

    /// Reads the saved keys and picks. A missing file reads as an empty setup. An entry that names
    /// a provider or role the setup page does not offer is skipped, with a warning.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read or does not parse. The error names no part of
    /// the file, since a parser error can quote the text it failed on.
    pub fn load(&self) -> io::Result<SavedSetup> {
        #[derive(Deserialize, Default)]
        #[serde(deny_unknown_fields, default)]
        struct Stored {
            keys: BTreeMap<String, String>,
            roles: BTreeMap<String, String>,
        }

        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok(SavedSetup::default());
            }
            Err(err) => return Err(err),
        };
        let stored: Stored = toml::from_str(&text).map_err(|_| not_a_key())?;

        let mut saved = SavedSetup::default();
        for (provider, key) in stored.keys {
            let key = key.trim();
            if !is_provider(&provider) || key.is_empty() {
                tracing::warn!(
                    "the keys file has an entry the setup page does not use; it is skipped"
                );
                continue;
            }
            saved.keys.insert(provider, ApiKey::new(key));
        }
        for (role, provider) in stored.roles {
            match role.parse::<Role>() {
                Ok(role) if is_provider(&provider) => {
                    saved.roles.insert(role, provider);
                }
                _ => tracing::warn!(
                    "the keys file has a role pick the setup page does not use; it is skipped"
                ),
            }
        }
        Ok(saved)
    }

    /// Saves `key` for `provider`, replacing the key it had. Role picks are kept.
    ///
    /// # Errors
    ///
    /// Returns [`SetupError::UnknownProvider`] or [`SetupError::InvalidKey`] before the file is
    /// touched, and [`SetupError::Write`] when the file cannot be read or written.
    pub fn set_key(&self, provider: &str, key: &str) -> Result<(), SetupError> {
        if !is_provider(provider) {
            return Err(SetupError::UnknownProvider);
        }
        if !valid_key(key) {
            return Err(SetupError::InvalidKey);
        }
        self.update(|saved| {
            saved.keys.insert(provider.to_owned(), ApiKey::new(key));
        })
    }

    /// Removes the key for `provider`, and any role pick that named it. Returns `false` when the
    /// provider had no key saved.
    ///
    /// # Errors
    ///
    /// Returns [`SetupError::Write`] when the file cannot be read or written.
    pub fn remove_key(&self, provider: &str) -> Result<bool, SetupError> {
        let mut removed = false;
        self.update(|saved| {
            removed = saved.keys.remove(provider).is_some();
            saved.roles.retain(|_, pick| pick != provider);
        })?;
        Ok(removed)
    }

    /// Picks `provider` for `role`, or clears the pick when `provider` is `None`. Whether the
    /// provider has a key is for the caller to check, since an environment key counts too.
    ///
    /// # Errors
    ///
    /// Returns [`SetupError::UnknownProvider`] for a provider the page does not offer,
    /// [`SetupError::NeedsMeta`] when a role that needs Meta is picked for another provider, and
    /// [`SetupError::Write`] when the file cannot be read or written.
    pub fn set_role(&self, role: Role, provider: Option<&str>) -> Result<(), SetupError> {
        if let Some(name) = provider {
            if !is_provider(name) {
                return Err(SetupError::UnknownProvider);
            }
            if role.needs_meta() && name != "meta" {
                return Err(SetupError::NeedsMeta);
            }
        }
        self.update(|saved| match provider {
            Some(name) => {
                saved.roles.insert(role, name.to_owned());
            }
            None => {
                saved.roles.remove(&role);
            }
        })
    }

    /// Reads the file, applies `change`, and writes it back. Two changes at once are serialized, so
    /// each keeps the other.
    fn update(&self, change: impl FnOnce(&mut SavedSetup)) -> Result<(), SetupError> {
        let _write = WRITE.lock().unwrap_or_else(PoisonError::into_inner);
        let mut saved = self.load().map_err(SetupError::Write)?;
        change(&mut saved);
        self.store(&saved).map_err(SetupError::Write)
    }

    /// Replaces the file with `saved`. The file is written to a temporary name and renamed over the
    /// old one, so a crash leaves the old setup or the new one, never a half-written file.
    fn store(&self, saved: &SavedSetup) -> io::Result<()> {
        #[derive(Serialize)]
        struct Stored<'a> {
            #[serde(skip_serializing_if = "BTreeMap::is_empty")]
            keys: BTreeMap<&'a str, &'a str>,
            #[serde(skip_serializing_if = "BTreeMap::is_empty")]
            roles: BTreeMap<&'static str, &'a str>,
        }

        let stored = Stored {
            keys: saved
                .keys
                .iter()
                .map(|(provider, key)| (provider.as_str(), key.expose()))
                .collect(),
            roles: saved
                .roles
                .iter()
                .map(|(role, provider)| (role.as_str(), provider.as_str()))
                .collect(),
        };
        let body = toml::to_string(&stored)
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
    file.set_key(provider, key)?;
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
        "the keys file does not parse as a keys and roles file",
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
