//! The settings file: read when the server starts, and replaced whole after each saved change.
//!
//! The file is written to a temporary name in the same folder and renamed over the old one, so a
//! crash leaves the old settings or the new ones, never a half-written file. On Unix the file is
//! mode 0600 and its folder is created 0700, as the keys file is.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use anyhow::bail;

use super::{FILE, Stored};

/// Reads the settings at `path`. A missing file reads as the defaults. An unreadable file, or one
/// that does not parse, is an error: the server does not replace it with the defaults.
///
/// # Errors
///
/// Returns an error when the file exists but cannot be read or does not parse. The error names no
/// part of the file, since a parser error can quote the text it failed on.
pub(crate) fn load(path: &Path) -> anyhow::Result<Stored> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Stored::default()),
        Err(err) => bail!("the {FILE} file cannot be read: {:?}", err.kind()),
    };
    serde_json::from_str(&text)
        .map_err(|_| anyhow::anyhow!("the {FILE} file does not parse as settings"))
}

/// Replaces the settings file at `path` with `stored`.
///
/// # Errors
///
/// Returns an error when the folder cannot be created, or the file cannot be written or renamed.
/// The old file is left in place then.
pub(crate) fn save(path: &Path, stored: &Stored) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("the settings file has no folder"))?;
    create_private_dir(dir)?;
    let mut body = serde_json::to_string_pretty(stored).map_err(io::Error::other)?;
    body.push('\n');

    let temp = dir.join(format!(".{FILE}.tmp"));
    let _ = fs::remove_file(&temp);
    let written = write_private(&temp, &body).and_then(|()| fs::rename(&temp, path));
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written
}

/// Creates the folder, and on Unix makes it private to its user when it is new.
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

/// Creates `path` as a new file only its user can read, and writes `text` to it.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{Budgets, Look, Theme};

    fn write(path: &Path, text: &str) {
        fs::write(path, text).expect("the test file is written");
    }

    #[test]
    fn a_missing_file_reads_as_the_defaults() {
        let dir = tempfile::tempdir().expect("temp dir");
        let stored = load(&dir.path().join("settings.json")).expect("a missing file loads");
        assert_eq!(stored, Stored::default());
    }

    #[test]
    fn saved_settings_load_back_as_they_were_saved() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("settings.json");
        let mut stored = Stored::default();
        stored.budgets.repair_rounds = 5;
        stored.look.theme = Theme::Dark;
        save(&path, &stored).expect("the settings are saved");
        assert_eq!(load(&path).expect("the settings load"), stored);
    }

    #[test]
    fn a_partial_file_keeps_the_defaults_for_what_it_leaves_out() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("settings.json");
        write(&path, r#"{"look": {"theme": "dark"}}"#);
        let stored = load(&path).expect("a partial file loads");
        assert_eq!(stored.look.theme, Theme::Dark);
        assert_eq!(stored.look.motion, Look::default().motion);
        assert_eq!(stored.budgets, Budgets::default());
    }

    #[test]
    fn an_unparseable_file_is_an_error_and_is_not_reset() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("settings.json");
        write(&path, "{ not json");
        assert!(load(&path).is_err());
        assert_eq!(
            fs::read_to_string(&path).expect("the file is still there"),
            "{ not json",
            "the file is left as it was"
        );
    }

    #[test]
    fn an_unknown_key_in_the_file_is_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("settings.json");
        write(&path, r#"{"budgets": {"repair_roundz": 3}}"#);
        assert!(load(&path).is_err());
    }

    #[test]
    fn the_error_for_a_bad_file_names_no_part_of_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("settings.json");
        write(&path, r#"{"look": "secret-text-in-the-file""#);
        let message = load(&path)
            .expect_err("the file does not parse")
            .to_string();
        assert!(!message.contains("secret-text"), "message: {message}");
    }

    #[test]
    fn a_save_leaves_no_temporary_file_behind() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("settings.json");
        save(&path, &Stored::default()).expect("the settings are saved");
        assert!(path.exists());
        assert!(!dir.path().join(format!(".{FILE}.tmp")).exists());
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_private_to_its_user() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("settings.json");
        save(&path, &Stored::default()).expect("the settings are saved");
        let mode = fs::metadata(&path)
            .expect("the file exists")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
