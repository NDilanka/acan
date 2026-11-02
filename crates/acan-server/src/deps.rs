//! The dependency cache of a node app.
//!
//! An app whose contract names an install (`acan-app/2`, runtime `node-24`) gets its dependencies
//! from one install per lockfile. The install runs `npm ci --ignore-scripts` in a container with
//! network egress, in a scratch directory that holds only the lockfile and the `package.json`
//! files of the tree. Its `node_modules` becomes a cache entry, and every run of that lockfile
//! mounts the entry read-only, with the network closed.
//!
//! The entry's key is the content of everything the install reads: the pinned image digest, the
//! install argv, the lockfile's bytes, and every `package.json` by path. A hit is never a stale
//! tree. An entry is built in a temporary directory, marked complete, and renamed to its key only
//! then, so a run never reads a half-built entry.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use acan_git::{Preparation, Repo};
use acan_intent::{AppContract, ContainerPolicy, ContainerUser, ContentHash};
use acan_witness::{InstallError, RunnerConfig, install_dependencies};
use serde::Serialize;
use tokio::sync::Semaphore;

/// The lockfile a node app is installed from, relative to the tree's root.
pub const LOCKFILE: &str = "package-lock.json";
/// The manifest every package and workspace has. Every one in the tree is part of the key.
const MANIFEST: &str = "package.json";
/// Directories the key never reads: git's own, and a `node_modules` already in the tree.
const SKIPPED_DIRS: [&str; 2] = [".git", "node_modules"];
/// The directory an install produces. Runs mount it at `/work/node_modules`.
const MODULES: &str = "node_modules";
/// Marks a cache entry complete. Only a complete entry is ever read.
const COMPLETE: &str = ".complete";
/// The version of the key's text. Changing what the key covers changes this.
const KEY_VERSION: &str = "acan-deps/1";
/// The network the install runs with, as the envelope names it.
const NETWORK: &str = "egress";
/// Install scripts never run, so the envelope says so.
const INSTALL_SCRIPTS: &str = "ignored";
/// The user an install runs as when the host is root, which no container may be.
const FALLBACK_USER: &str = "1000:1000";

/// Which side of the contrast an install serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The protected head the contrast starts from.
    Base,
    /// The candidate's merged commit.
    Candidate,
}

impl Role {
    /// The role's name, as the envelope and the scratch directory name it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::Candidate => "candidate",
        }
    }
}

/// Whether an install reused a cache entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cache {
    /// The entry for the key existed, so nothing was installed.
    Hit,
    /// The entry was built by this install.
    Miss,
}

/// One install, as the receipt envelope records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstallRecord {
    /// Which side of the contrast the install serves.
    pub role: Role,
    /// The commit the tree was written from.
    pub commit: String,
    /// The cache key.
    pub key: String,
    /// The hash of the lockfile.
    pub lockfile_sha256: String,
    /// Whether the entry was reused.
    pub cache: Cache,
    /// The digest of the pinned image the install ran in.
    pub image_digest: String,
    /// The install argv.
    pub argv: Vec<String>,
    /// Always `ignored`: no install script runs.
    pub install_scripts: &'static str,
    /// The network the install ran with.
    pub network: &'static str,
    /// How long the role's dependencies took, from the tree to the entry.
    pub duration_ms: u64,
}

/// A role's installed dependencies: the entry's `node_modules`, and the record of the install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    /// The directory every run of the role mounts read-only.
    pub modules: PathBuf,
    /// The install's record for the envelope.
    pub record: InstallRecord,
}

/// Why a node app's dependencies could not be installed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DepsError {
    /// A file or directory could not be written or read.
    #[error("cannot access {}: {source}", path.display())]
    Io {
        /// The path.
        path: PathBuf,
        /// The cause.
        #[source]
        source: io::Error,
    },
    /// The tree could not be written from its commit.
    #[error("cannot write the {} tree: {detail}", role.name())]
    Tree {
        /// The role.
        role: Role,
        /// What went wrong.
        detail: String,
    },
    /// The tree has no lockfile, so there is nothing to install.
    #[error("the {} tree has no {LOCKFILE}", role.name())]
    MissingLockfile {
        /// The role.
        role: Role,
    },
    /// The install did not finish.
    #[error("the {} install failed: {source}", role.name())]
    Install {
        /// The role.
        role: Role,
        /// Why the install failed.
        #[source]
        source: InstallError,
    },
    /// The install finished without a `node_modules`.
    #[error("the {} install produced no {MODULES}", role.name())]
    NoModules {
        /// The role.
        role: Role,
    },
    /// The entry could not be stored under its key.
    #[error("cannot store the cache entry {}: {source}", path.display())]
    Store {
        /// The entry's path.
        path: PathBuf,
        /// The cause.
        #[source]
        source: io::Error,
    },
}

/// The host side of the install phase: the repository the trees are written from, the directory
/// the cache entries live in, the scratch directory, and the container permits an install takes.
pub struct Installer<'a> {
    /// The repository the trees are written from.
    pub repo: &'a Repo,
    /// How the host reaches docker.
    pub config: &'a RunnerConfig,
    /// The host's container permits.
    pub slots: &'a Semaphore,
    /// The directory the cache entries are stored under. Created when missing.
    pub cache: &'a Path,
    /// The directory each role's tree is written under. Created when missing.
    pub scratch: &'a Path,
}

impl Installer<'_> {
    /// Installs `app`'s dependencies for `preparation`, or reuses the cache entry for them.
    /// Returns `None` when the app has no install.
    ///
    /// # Errors
    ///
    /// Returns [`DepsError`] when the tree cannot be written or has no lockfile, the install fails
    /// or produces no `node_modules`, or the entry cannot be stored.
    pub async fn install(
        &self,
        preparation: &Preparation,
        role: Role,
        app: &AppContract,
    ) -> Result<Option<Installed>, DepsError> {
        let Some(argv) = app.install.as_ref() else {
            return Ok(None);
        };
        let argv = argv.as_slice();
        let started = Instant::now();
        let tree = self.scratch.join(role.name());
        remove_if_present(&tree)?;
        fs::create_dir_all(&tree).map_err(|source| io_error(&tree, source))?;
        self.repo
            .materialize(preparation, &tree)
            .await
            .map_err(|err| DepsError::Tree {
                role,
                detail: err.to_string(),
            })?;

        let lock_path = tree.join(LOCKFILE);
        let lock = match fs::read(&lock_path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Err(DepsError::MissingLockfile { role });
            }
            Err(source) => return Err(io_error(&lock_path, source)),
        };
        let manifests = manifests(&tree).map_err(|source| io_error(&tree, source))?;
        let mut policy = ContainerPolicy::node_install();
        let image = policy
            .image
            .digest
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default();
        let key = key(&image, argv, &lock, &manifests);
        let entry = self.cache.join(key.to_string().replace(':', "-"));
        let record = |cache: Cache| InstallRecord {
            role,
            commit: preparation.merged_commit().to_string(),
            key: key.to_string(),
            lockfile_sha256: ContentHash::of_bytes(&lock).to_string(),
            cache,
            image_digest: image.clone(),
            argv: argv.to_vec(),
            install_scripts: INSTALL_SCRIPTS,
            network: NETWORK,
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        };
        if entry.join(COMPLETE).is_file() {
            return Ok(Some(Installed {
                modules: entry.join(MODULES),
                record: record(Cache::Hit),
            }));
        }

        fs::create_dir_all(self.cache).map_err(|source| io_error(self.cache, source))?;
        // The partial directory is removed when it drops, unless the entry was stored from it.
        let partial = tempfile::Builder::new()
            .prefix("partial-")
            .tempdir_in(self.cache)
            .map_err(|source| io_error(self.cache, source))?;
        let work = partial.path().join("work");
        fs::create_dir(&work).map_err(|source| io_error(&work, source))?;
        open_to_install(&work).map_err(|source| io_error(&work, source))?;
        stage(&work, &lock, &manifests).map_err(|source| io_error(&work, source))?;
        policy.user = install_user(&work).map_err(|source| io_error(&work, source))?;
        {
            let _permit = self
                .slots
                .acquire()
                .await
                .expect("the container permits are never closed");
            install_dependencies(self.config, &policy, &work, argv)
                .await
                .map_err(|source| DepsError::Install { role, source })?;
        }
        if !work.join(MODULES).is_dir() {
            return Err(DepsError::NoModules { role });
        }
        fs::write(work.join(COMPLETE), b"").map_err(|source| io_error(&work, source))?;
        match fs::rename(&work, &entry) {
            Ok(()) => {}
            // Another install of the same key stored the entry first. Its entry is the same tree.
            Err(_) if entry.join(COMPLETE).is_file() => {}
            Err(source) => {
                return Err(DepsError::Store {
                    path: entry,
                    source,
                });
            }
        }
        Ok(Some(Installed {
            modules: entry.join(MODULES),
            record: record(Cache::Miss),
        }))
    }
}

/// The directory cache entries are stored under: `explicit` when given, else
/// `$XDG_CACHE_HOME/acan/deps`, else `$HOME/.cache/acan/deps`.
///
/// # Errors
///
/// Returns a message when no directory can be named.
pub fn cache_dir(explicit: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(dir) = explicit {
        return std::path::absolute(dir)
            .map_err(|err| format!("cannot resolve {}: {err}", dir.display()));
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .ok_or_else(|| "neither XDG_CACHE_HOME nor HOME is set; pass --cache-dir".to_owned())?;
    Ok(base.join("acan").join("deps"))
}

/// The cache key: the text of everything the install reads, hashed. The manifests are sorted by
/// path, so the key does not depend on the order the tree is walked in.
fn key(image: &str, argv: &[String], lock: &[u8], manifests: &[(String, Vec<u8>)]) -> ContentHash {
    let argv = serde_json::to_string(argv).unwrap_or_default();
    let mut text = format!(
        "{KEY_VERSION}\nimage {image}\nargv {argv}\nlockfile {}\n",
        ContentHash::of_bytes(lock)
    );
    for (path, bytes) in manifests {
        let _ = std::fmt::Write::write_fmt(
            &mut text,
            format_args!("manifest {path} {}\n", ContentHash::of_bytes(bytes)),
        );
    }
    ContentHash::of_bytes(text.as_bytes())
}

/// Every `package.json` under `root`, by its path from `root`, sorted by path, with its bytes.
/// Git's directory and any `node_modules` are skipped, and so are symbolic links.
fn manifests(root: &Path) -> io::Result<Vec<(String, Vec<u8>)>> {
    let mut found = Vec::new();
    walk(root, root, &mut found)?;
    found.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(found)
}

fn walk(root: &Path, dir: &Path, found: &mut Vec<(String, Vec<u8>)>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if file_type.is_dir() {
            if !SKIPPED_DIRS.iter().any(|skip| name == *skip) {
                walk(root, &path, found)?;
            }
        } else if file_type.is_file() && name == MANIFEST {
            let relative = path
                .strip_prefix(root)
                .map_err(|err| io::Error::other(err.to_string()))?;
            let text = relative.to_string_lossy().replace('\\', "/");
            found.push((text, fs::read(&path)?));
        }
    }
    Ok(())
}

/// Writes the lockfile and the manifests into `work`, at the paths they have in the tree.
fn stage(work: &Path, lock: &[u8], manifests: &[(String, Vec<u8>)]) -> io::Result<()> {
    fs::write(work.join(LOCKFILE), lock)?;
    for (path, bytes) in manifests {
        let target = work.join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&target, bytes)?;
    }
    Ok(())
}

/// The install's user: the owner of its directory, which is the host user, so the files it writes
/// belong to the host and the host can remove them. A root host runs the install as 1000.
#[cfg(unix)]
fn install_user(work: &Path) -> io::Result<ContainerUser> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(work)?;
    let text = if meta.uid() == 0 || meta.gid() == 0 {
        FALLBACK_USER.to_owned()
    } else {
        format!("{}:{}", meta.uid(), meta.gid())
    };
    text.parse()
        .map_err(|err: acan_intent::InvalidValue| io::Error::other(err.to_string()))
}

#[cfg(not(unix))]
fn install_user(_work: &Path) -> io::Result<ContainerUser> {
    FALLBACK_USER
        .parse()
        .map_err(|err: acan_intent::InvalidValue| io::Error::other(err.to_string()))
}

/// Lets the install's user write into `work`, whichever user the host is. The directory is
/// private to the host through its parent, which is created `0700`.
#[cfg(unix)]
fn open_to_install(work: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(work, fs::Permissions::from_mode(0o777))
}

#[cfg(not(unix))]
fn open_to_install(_work: &Path) -> io::Result<()> {
    Ok(())
}

/// Removes `path` when it exists, file or directory.
fn remove_if_present(path: &Path) -> Result<(), DepsError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error(path, source)),
    }
}

fn io_error(path: &Path, source: io::Error) -> DepsError {
    DepsError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IMAGE: &str = "node:24@sha256:a";

    fn argv() -> Vec<String> {
        ["npm", "ci", "--ignore-scripts"]
            .map(str::to_owned)
            .to_vec()
    }

    fn manifests_of(pairs: &[(&str, &str)]) -> Vec<(String, Vec<u8>)> {
        pairs
            .iter()
            .map(|(path, text)| ((*path).to_owned(), text.as_bytes().to_vec()))
            .collect()
    }

    fn key_of(
        image: &str,
        argv: &[String],
        lock: &[u8],
        manifests: &[(String, Vec<u8>)],
    ) -> String {
        key(image, argv, lock, manifests).to_string()
    }

    #[test]
    fn the_key_follows_every_input_the_install_reads() {
        let manifests = manifests_of(&[("package.json", "{}")]);
        let base = key_of(IMAGE, &argv(), b"lock", &manifests);
        assert_eq!(
            base,
            key_of(IMAGE, &argv(), b"lock", &manifests),
            "the same inputs give the same key"
        );
        let other_argv = ["npm", "ci", "--ignore-scripts", "--no-fund"]
            .map(str::to_owned)
            .to_vec();
        let changed = [
            key_of("node:24@sha256:b", &argv(), b"lock", &manifests),
            key_of(IMAGE, &other_argv, b"lock", &manifests),
            key_of(IMAGE, &argv(), b"lock2", &manifests),
            key_of(
                IMAGE,
                &argv(),
                b"lock",
                &manifests_of(&[("package.json", "{ }")]),
            ),
            key_of(
                IMAGE,
                &argv(),
                b"lock",
                &manifests_of(&[("package.json", "{}"), ("packages/a/package.json", "{}")]),
            ),
        ];
        for other in changed {
            assert_ne!(base, other, "an input the install reads is in the key");
        }
    }

    #[test]
    fn manifests_are_every_package_json_by_sorted_path_and_nothing_else() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let root = dir.path();
        let files = [
            ("package.json", "{}"),
            ("packages/b/package.json", "{\"b\":1}"),
            ("packages/a/package.json", "{\"a\":1}"),
            ("node_modules/x/package.json", "{}"),
            (".git/package.json", "{}"),
            ("README.md", "text"),
        ];
        for (path, text) in files {
            let file = root.join(path);
            fs::create_dir_all(file.parent().expect("a parent")).expect("a directory");
            fs::write(&file, text).expect("a file");
        }
        let found = manifests(root).expect("the tree is read");
        let paths: Vec<&str> = found.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "package.json",
                "packages/a/package.json",
                "packages/b/package.json"
            ]
        );
    }

    #[test]
    fn stage_writes_the_lockfile_and_the_manifests_where_the_tree_has_them() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let manifests = manifests_of(&[
            ("package.json", "{}"),
            ("packages/a/package.json", "{\"a\":1}"),
        ]);
        stage(dir.path(), b"lock", &manifests).expect("the tree is staged");
        assert_eq!(
            fs::read(dir.path().join(LOCKFILE)).expect("the lockfile"),
            b"lock"
        );
        assert_eq!(
            fs::read(dir.path().join("packages/a/package.json")).expect("the manifest"),
            b"{\"a\":1}"
        );
    }

    #[test]
    fn an_explicit_cache_dir_is_named_absolute() {
        let dir = cache_dir(Some(Path::new("relative/cache"))).expect("a directory is named");
        assert!(dir.is_absolute(), "{}", dir.display());
        assert!(dir.ends_with("relative/cache"), "{}", dir.display());
    }
}
