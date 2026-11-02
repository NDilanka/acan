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
use acan_intent::{AppContract, ContainerPolicy, ContainerUser, ContentHash, NPM_REGISTRY};
use acan_witness::{InstallError, RunnerConfig, install_dependencies};
use serde::Serialize;
use serde_json::{Map, Value};
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
/// The uid and gid an install runs as when the host is root, or is in group 0. No container may
/// run as root, and a host user in group 0 is treated the same way.
const FALLBACK_ID: u32 = 1000;

/// The user a check runs as: the uid and gid of a file it creates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Identity {
    uid: u32,
    gid: u32,
}

impl Identity {
    /// The owners a cache entry, or anything under one, may have: this user, and the user an
    /// install runs as, which is the fallback when this user is root or in group 0.
    fn trusted_owners(self) -> [u32; 2] {
        [self.uid, install_ids(self.uid, self.gid).0]
    }
}

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
    /// The lockfile names a package source the install may not fetch from. Nothing is installed.
    #[error("the {} lockfile cannot be installed: {detail}", role.name())]
    Lockfile {
        /// The role.
        role: Role,
        /// What the lockfile names, without any URL, so no credential reaches the message.
        detail: String,
    },
    /// A `package.json` names a package source the install may not fetch from. Nothing is
    /// installed.
    #[error("the {} tree cannot be installed: {detail}", role.name())]
    Manifest {
        /// The role.
        role: Role,
        /// What the manifest names, without any URL, so no credential reaches the message.
        detail: String,
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
    /// A path in the cache is a link, or it is open to other users, so nothing in it is trusted.
    /// Remove the entry, or make the cache root private to this user, and run again.
    #[error("the cache path {} cannot be used: {detail}", path.display())]
    Tampered {
        /// The path.
        path: PathBuf,
        /// What is wrong with it.
        detail: String,
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
        if app.install.is_none() {
            return Ok(None);
        }
        self.install_as(preparation, role, app, process_identity()?)
            .await
    }

    /// As [`Self::install`], for the user `me`. The cache root is checked for `me` before any
    /// entry under it is looked up, so a root another user can write is never read, not even for
    /// a hit.
    async fn install_as(
        &self,
        preparation: &Preparation,
        role: Role,
        app: &AppContract,
        me: Identity,
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
        check_lockfile(role, &lock)?;
        let manifests = manifests(&tree).map_err(|source| io_error(&tree, source))?;
        check_manifests(role, &manifests)?;
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
        prepare_root(self.cache)?;
        check_root(self.cache, me)?;
        if read_entry(&entry, me)? == Entry::Complete {
            return Ok(Some(Installed {
                modules: entry.join(MODULES),
                record: record(Cache::Hit),
            }));
        }

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
        // The install left the tree open to its user. Every run reads the entry, so it is sealed
        // before it is published.
        seal(&work).map_err(|source| io_error(&work, source))?;
        match fs::rename(&work, &entry) {
            Ok(()) => {}
            // Another install of the same key stored the entry first. Its entry is the same tree.
            Err(_) if matches!(read_entry(&entry, me), Ok(Entry::Complete)) => {}
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

/// Refuses a lockfile that names a package source the install may not fetch from. The install has
/// network egress, so each fetch is a request the host makes. Only the public registry is allowed.
/// A git, file, plain-HTTP or local-address source is refused, a link outside the tree is refused,
/// and so is a version that names a source where a semver version belongs. Every dependency map
/// and every override is checked too, so a node with no `resolved` cannot name a source either.
///
/// Residual risk: npm follows the redirects the registry sends, and the host resolves the
/// registry's name. This check sees neither.
fn check_lockfile(role: Role, lock: &[u8]) -> Result<(), DepsError> {
    check_lock(lock).map_err(|detail| DepsError::Lockfile { role, detail })
}

/// The reason a lockfile is refused, or `Ok` when every source in it is admitted.
fn check_lock(lock: &[u8]) -> Result<(), String> {
    let value: Value = serde_json::from_slice(lock).map_err(|_| "it is not JSON".to_owned())?;
    let root = value
        .as_object()
        .ok_or_else(|| "it is not a JSON object".to_owned())?;
    check_node("the lockfile root", root)?;
    if let Some(packages) = root.get("packages") {
        let packages = packages
            .as_object()
            .ok_or_else(|| "its packages are not an object".to_owned())?;
        for (path, node) in packages {
            let node = node
                .as_object()
                .ok_or_else(|| format!("{path} is not an object"))?;
            check_node(path, node)?;
        }
    }
    Ok(())
}

/// Checks one lockfile node: its own source and version, its dependency maps and its overrides. A
/// version 1 lockfile nests a node's dependencies as nodes, and those are checked as well. `name`
/// is the key the node sits under, which a refusal names.
fn check_node(name: &str, node: &Map<String, Value>) -> Result<(), String> {
    check_entry(name, node)?;
    for (key, child) in node {
        if is_spec_map(key, true) {
            check_lock_specs(name, key, child)?;
        } else if is_redirect_map(key) {
            check_redirects(name, child)?;
        }
    }
    Ok(())
}

/// Checks a lockfile node's map of dependencies. A string is a spec, and an object is a nested
/// node of a version 1 lockfile.
fn check_lock_specs(owner: &str, key: &str, value: &Value) -> Result<(), String> {
    let map = value
        .as_object()
        .ok_or_else(|| format!("{owner} has a {key} that is not an object"))?;
    for (dep, child) in map {
        check_dep_name(owner, dep)?;
        match child {
            Value::Object(nested) => check_node(dep, nested)?,
            spec => {
                if let Some(refusal) = refused_spec(owner, dep, spec) {
                    return Err(refusal);
                }
            }
        }
    }
    Ok(())
}

/// Refuses a manifest that names a package source the install may not fetch from. npm builds the
/// tree from every manifest before it checks the lockfile against them, so a spec in a manifest is
/// fetched even when the lockfile does not list it.
fn check_manifests(role: Role, manifests: &[(String, Vec<u8>)]) -> Result<(), DepsError> {
    for (path, bytes) in manifests {
        check_manifest(path, bytes).map_err(|detail| DepsError::Manifest { role, detail })?;
    }
    Ok(())
}

/// The reason a manifest is refused, or `Ok` when every spec in it is admitted.
fn check_manifest(path: &str, bytes: &[u8]) -> Result<(), String> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| format!("{path} is not JSON"))?;
    let object = value
        .as_object()
        .ok_or_else(|| format!("{path} is not a JSON object"))?;
    for (key, child) in object {
        if is_spec_map(key, false) {
            check_manifest_specs(path, key, child)?;
        } else if is_redirect_map(key) {
            check_redirects(path, child)?;
        }
    }
    Ok(())
}

/// Checks a manifest's map of dependencies: every name is a package name, and every value is a
/// spec the install may fetch.
fn check_manifest_specs(owner: &str, key: &str, value: &Value) -> Result<(), String> {
    let map = value
        .as_object()
        .ok_or_else(|| format!("{owner} has a {key} that is not an object"))?;
    for (dep, spec) in map {
        check_dep_name(owner, dep)?;
        if let Some(refusal) = refused_spec(owner, dep, spec) {
            return Err(refusal);
        }
    }
    Ok(())
}

/// Refuses a dependency whose name is not a package name. Such a name is never echoed.
fn check_dep_name(owner: &str, dep: &str) -> Result<(), String> {
    if is_package_name(dep) {
        return Ok(());
    }
    let owner = if owner.is_empty() {
        "the root package"
    } else {
        owner
    };
    Err(format!(
        "{owner} declares a dependency whose name is not a package name"
    ))
}

/// Checks overrides or resolutions. A string redirects the package it is keyed by, and an object
/// nests more redirects. A key may be a selector rather than a package name, so a refusal names it
/// only when it is a package name.
fn check_redirects(owner: &str, value: &Value) -> Result<(), String> {
    let map = value
        .as_object()
        .ok_or_else(|| format!("{owner} has redirects that are not an object"))?;
    for (key, redirect) in map {
        match redirect {
            Value::Object(_) => check_redirects(owner, redirect)?,
            spec => {
                if let Some(refusal) = refused_spec(owner, key, spec) {
                    return Err(refusal);
                }
            }
        }
    }
    Ok(())
}

/// Whether `key` names a map of package specs: a manifest's or a lockfile node's dependencies. A
/// version 1 lockfile lists a node's dependencies under `requires` as well, so only a lockfile
/// reads that key.
fn is_spec_map(key: &str, lock: bool) -> bool {
    matches!(
        key,
        "dependencies" | "devDependencies" | "optionalDependencies" | "peerDependencies"
    ) || (lock && key == "requires")
}

/// Whether `key` names a map of redirects, which npm reads as dependencies to fetch instead.
fn is_redirect_map(key: &str) -> bool {
    matches!(key, "overrides" | "resolutions")
}

/// The reason a dependency's spec is refused, or `None` when npm may fetch it. The reason names
/// the owner, and the dependency only when its name is a package name. It never names the spec,
/// which may carry a URL or a credential.
fn refused_spec(owner: &str, dep: &str, spec: &Value) -> Option<String> {
    if spec.as_str().is_some_and(admitted_spec) {
        return None;
    }
    let owner = if owner.is_empty() {
        "the root package"
    } else {
        owner
    };
    let dep = if is_package_name(dep) {
        dep
    } else {
        "a dependency"
    };
    Some(format!(
        "{owner} declares {dep} with a spec the install may not fetch; only a version range, a \
         tag or an npm alias is admitted"
    ))
}

/// Whether npm may fetch a spec from the registry: a version range or a tag, or an npm alias of
/// one. A URL, a git or hosted shorthand, a path, a tarball and a `file:` spec name another source.
fn admitted_spec(spec: &str) -> bool {
    let spec = spec.trim();
    match spec.strip_prefix("npm:") {
        Some(alias) => match alias.rfind('@') {
            Some(at) if at > 0 => is_package_name(&alias[..at]) && admitted_range(&alias[at + 1..]),
            _ => is_package_name(alias),
        },
        None => admitted_range(spec),
    }
}

/// Whether a version range or a tag holds only the characters of a semver range. A path, a
/// relative name, and a tarball name fall outside that grammar.
fn admitted_range(range: &str) -> bool {
    let range = range.trim();
    let lower = range.to_ascii_lowercase();
    !range.starts_with('.')
        && !lower.ends_with(".tgz")
        && !lower.ends_with(".tar")
        && !lower.ends_with(".tar.gz")
        && range
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || " .-+*<>=|~^".contains(c))
}

/// Whether `name` is a package name: an optional scope and a name, each of the URL-safe characters
/// npm allows in a name, and neither starting with a dot. A name with any other character, such
/// as a colon, a slash or a credential, is not a package name.
fn is_package_name(name: &str) -> bool {
    let (scope, bare) = match name.strip_prefix('@') {
        Some(scoped) => match scoped.split_once('/') {
            Some((scope, bare)) => (Some(scope), bare),
            None => return false,
        },
        None => (None, name),
    };
    let segment = |part: &str| {
        !part.is_empty()
            && !part.starts_with('.')
            && part.chars().all(|c| {
                c.is_ascii_alphanumeric()
                    || matches!(c, '-' | '.' | '_' | '~' | '!' | '*' | '\'' | '(' | ')')
            })
    };
    scope.is_none_or(segment) && segment(bare)
}

/// Checks one lockfile node's `resolved` source and its `version`. `name` is the node's key.
fn check_entry(name: &str, entry: &Map<String, Value>) -> Result<(), String> {
    if let Some(resolved) = entry.get("resolved").and_then(Value::as_str) {
        if entry.get("link") == Some(&Value::Bool(true)) {
            if !is_tree_path(resolved) {
                return Err(format!("{name} links to a path outside the tree"));
            }
        } else if !resolved.starts_with(NPM_REGISTRY) {
            return Err(format!(
                "{name} names a source other than {NPM_REGISTRY}, and the install fetches only \
                 from that registry"
            ));
        }
    }
    if let Some(version) = entry.get("version").and_then(Value::as_str)
        && version.contains([':', '/', '#'])
    {
        return Err(format!("{name} names a source where a version belongs"));
    }
    Ok(())
}

/// Whether a link's target is a relative path that stays inside the tree.
fn is_tree_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains([':', '\\'])
        && path.split('/').all(|part| part != "..")
}

/// What the cache holds under one key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entry {
    /// Nothing is stored under the key, or only a partial entry is.
    Absent,
    /// A complete entry: a real directory with a real `node_modules` and the complete marker, and
    /// nothing in it that another user can write.
    Complete,
}

/// Reads what the cache holds under `entry`, for the user `me`. A link where the entry, its
/// `node_modules` or its marker should be is refused. So is an entry, or anything under it, that
/// another user owns or the group or others can write: a run would mount or trust whatever it
/// reaches.
fn read_entry(entry: &Path, me: Identity) -> Result<Entry, DepsError> {
    let marker = entry.join(COMPLETE);
    match fs::symlink_metadata(&marker) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Err(tampered(&marker, "the complete marker is not a plain file")),
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Entry::Absent),
        Err(source) => return Err(io_error(&marker, source)),
    }
    let meta = fs::symlink_metadata(entry).map_err(|source| io_error(entry, source))?;
    if !meta.is_dir() {
        return Err(tampered(entry, "the entry is not a directory"));
    }
    check_tree(entry, me)?;
    let modules = entry.join(MODULES);
    let modules_meta =
        fs::symlink_metadata(&modules).map_err(|source| io_error(&modules, source))?;
    if !modules_meta.is_dir() {
        return Err(tampered(&modules, "node_modules is not a plain directory"));
    }
    Ok(Entry::Complete)
}

/// Refuses `path`, and everything under it, when another user owns it or the group or others can
/// write it. A link is not followed, and nothing is changed.
fn check_tree(path: &Path, me: Identity) -> Result<(), DepsError> {
    let meta = fs::symlink_metadata(path).map_err(|source| io_error(path, source))?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    if !me.trusted_owners().contains(&owner(&meta)) {
        return Err(tampered(
            path,
            "another user owns it; remove the entry and install again",
        ));
    }
    if writable_by_others(&meta) {
        return Err(tampered(
            path,
            "the group or others can write it; remove the entry and install again",
        ));
    }
    if meta.is_dir() {
        for child in fs::read_dir(path).map_err(|source| io_error(path, source))? {
            let child = child.map_err(|source| io_error(path, source))?;
            check_tree(&child.path(), me)?;
        }
    }
    Ok(())
}

/// Creates the cache root when it is missing, private to its owner. Its parents are created as
/// usual.
fn prepare_root(cache: &Path) -> Result<(), DepsError> {
    if let Some(parent) = cache.parent() {
        fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
    }
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(cache) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(source) => Err(io_error(cache, source)),
    }
}

/// Checks the cache root before anything is looked up or stored in it: a real directory, not open
/// to its group or others, and owned by the user `me`.
fn check_root(cache: &Path, me: Identity) -> Result<(), DepsError> {
    let root = fs::symlink_metadata(cache).map_err(|source| io_error(cache, source))?;
    if !root.is_dir() {
        return Err(tampered(cache, "the cache root is not a plain directory"));
    }
    if writable_by_others(&root) {
        return Err(tampered(
            cache,
            "the cache root is writable by its group or others; make it private to this user",
        ));
    }
    if owner(&root) != me.uid {
        return Err(tampered(cache, "the cache root belongs to another user"));
    }
    Ok(())
}

/// Who this process runs as. A file it creates in the system temporary directory is owned by it,
/// so the cache is never written to learn the answer.
#[cfg(unix)]
fn process_identity() -> Result<Identity, DepsError> {
    use std::os::unix::fs::MetadataExt;
    let dir = std::env::temp_dir();
    let probe = tempfile::NamedTempFile::new_in(&dir).map_err(|source| io_error(&dir, source))?;
    let meta = probe
        .as_file()
        .metadata()
        .map_err(|source| io_error(probe.path(), source))?;
    Ok(Identity {
        uid: meta.uid(),
        gid: meta.gid(),
    })
}

#[cfg(not(unix))]
fn process_identity() -> Result<Identity, DepsError> {
    Ok(Identity { uid: 0, gid: 0 })
}

fn tampered(path: &Path, detail: &str) -> DepsError {
    DepsError::Tampered {
        path: path.to_path_buf(),
        detail: detail.to_owned(),
    }
}

/// Whether the group or others can write `meta`.
#[cfg(unix)]
fn writable_by_others(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o022 != 0
}

#[cfg(not(unix))]
fn writable_by_others(_meta: &fs::Metadata) -> bool {
    false
}

/// The uid that owns `meta`. Windows has no such owner, so every path has the same one.
#[cfg(unix)]
fn owner(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    meta.uid()
}

#[cfg(not(unix))]
fn owner(_meta: &fs::Metadata) -> u32 {
    0
}

/// The uid and gid an install runs as, for a tree its host user owns. A root host, or a host in
/// group 0, runs it as the fallback, which no container may run as root.
fn install_ids(uid: u32, gid: u32) -> (u32, u32) {
    if uid == 0 || gid == 0 {
        (FALLBACK_ID, FALLBACK_ID)
    } else {
        (uid, gid)
    }
}

/// Clears the group and other write bits of every directory and file under `root`, and of `root`
/// itself. Links are left alone: changing a link's mode would change its target.
#[cfg(unix)]
fn seal(root: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fn walk(path: &Path) -> io::Result<()> {
        let meta = fs::symlink_metadata(path)?;
        if meta.file_type().is_symlink() {
            return Ok(());
        }
        if meta.is_dir() {
            for entry in fs::read_dir(path)? {
                walk(&entry?.path())?;
            }
        }
        let mode = meta.permissions().mode() & 0o7777;
        fs::set_permissions(path, fs::Permissions::from_mode(mode & !0o022))
    }
    walk(root)
}

#[cfg(not(unix))]
fn seal(_root: &Path) -> io::Result<()> {
    Ok(())
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
    let (uid, gid) = install_ids(meta.uid(), meta.gid());
    container_user(uid, gid)
}

#[cfg(not(unix))]
fn install_user(_work: &Path) -> io::Result<ContainerUser> {
    let (uid, gid) = install_ids(0, 0);
    container_user(uid, gid)
}

fn container_user(uid: u32, gid: u32) -> io::Result<ContainerUser> {
    format!("{uid}:{gid}")
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
    use std::str::FromStr;

    use acan_git::{PrepareOutcome, ProtectedPaths};
    use acan_intent::CommitSha;

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

    /// The refusal a lockfile gets, as its text, or `None` when it is admitted.
    fn refusal(lock: &str) -> Option<String> {
        check_lockfile(Role::Base, lock.as_bytes())
            .err()
            .map(|err| err.to_string())
    }

    #[test]
    fn a_lockfile_that_fetches_only_from_the_registry_is_admitted() {
        let lock = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "app", "version": "1.0.0" },
                "node_modules/left-pad": {
                    "version": "1.3.0",
                    "resolved": "https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz",
                    "integrity": "sha512-x"
                },
                "node_modules/shared": { "resolved": "packages/shared", "link": true },
                "packages/shared": { "name": "shared", "version": "0.1.0" }
            }
        }"#;

        assert_eq!(refusal(lock), None);
    }

    #[test]
    fn a_source_other_than_the_registry_is_refused_and_no_url_is_named() {
        let sources = [
            "http://169.254.169.254/latest/meta-data.tgz",
            "https://10.0.0.5/pkg.tgz",
            "git+ssh://git@github.com/x/y.git#abc",
            "file:../../outside",
            "https://registry.npmjs.org.example.test/pkg.tgz",
            "https://user:secret@registry.npmjs.org/pkg.tgz",
        ];
        for source in sources {
            let lock = format!(
                r#"{{"packages": {{"node_modules/pkg": {{"version": "1.0.0", "resolved": "{source}"}}}}}}"#
            );

            let text = refusal(&lock).expect("the source is refused");

            assert!(text.contains("node_modules/pkg"), "{text}");
            assert!(!text.contains(source), "the URL is not echoed: {text}");
            assert!(!text.contains("secret"), "no credential is echoed: {text}");
        }
    }

    #[test]
    fn a_link_outside_the_tree_and_a_version_that_names_a_source_are_refused() {
        for link in ["/etc", "../../outside", "C:/outside"] {
            let lock = format!(
                r#"{{"packages": {{"node_modules/x": {{"resolved": "{link}", "link": true}}}}}}"#
            );
            assert!(refusal(&lock).is_some(), "{link} is refused");
        }
        let lock =
            r#"{"packages": {"node_modules/x": {"version": "git+ssh://git@github.com/x/y.git"}}}"#;
        assert!(refusal(lock).expect("refused").contains("node_modules/x"));
    }

    #[test]
    fn a_lockfile_that_is_not_json_is_refused() {
        assert!(refusal("{not json").expect("refused").contains("not JSON"));
    }

    /// Specs that name a source other than the registry, in the forms a manifest or a lockfile can
    /// hold them.
    const OTHER_SOURCES: [&str; 9] = [
        "https://evil.example.test/left-pad.tgz",
        "http://169.254.169.254/latest/meta-data.tgz",
        "git+ssh://git@github.com/x/y.git#abc",
        "github:x/y",
        "x/y",
        "file:../../outside",
        "./left-pad",
        "left-pad.tgz",
        "npm:left-pad@https://evil.example.test/left-pad.tgz",
    ];

    /// The refusal a manifest gets, as its text, or `None` when it is admitted.
    fn manifest_refusal(manifest: &str) -> Option<String> {
        check_manifest("package.json", manifest.as_bytes()).err()
    }

    #[test]
    fn a_manifest_spec_that_names_another_source_is_refused_and_no_url_is_named() {
        for source in OTHER_SOURCES {
            for key in [
                "dependencies",
                "devDependencies",
                "optionalDependencies",
                "peerDependencies",
            ] {
                let manifest = format!(r#"{{"name": "app", "{key}": {{"left-pad": "{source}"}}}}"#);
                let text = manifest_refusal(&manifest).expect("the spec is refused");
                assert!(
                    text.contains("package.json") && text.contains("left-pad"),
                    "{text}"
                );
                assert!(!text.contains(source), "the spec is not echoed: {text}");
            }
            for redirect in [
                format!(r#"{{"overrides": {{"left-pad": "{source}"}}}}"#),
                format!(r#"{{"overrides": {{"left-pad": {{"right-pad": "{source}"}}}}}}"#),
                format!(r#"{{"resolutions": {{"left-pad": "{source}"}}}}"#),
            ] {
                let text = manifest_refusal(&redirect).expect("the redirect is refused");
                assert!(!text.contains(source), "the redirect is not echoed: {text}");
            }
        }
    }

    #[test]
    fn a_manifest_spec_that_is_a_range_a_tag_or_an_alias_is_admitted() {
        for spec in [
            "",
            "*",
            "latest",
            "^1.2.3",
            "~1.0",
            "1.x",
            ">=1.0.0 <2",
            "1.0.0 || 2.0.0",
            "1.0.0-rc.1+build.5",
            "npm:left-pad@^1.3.0",
            "npm:@scope/left-pad@1.3.0",
            "npm:left-pad",
        ] {
            let manifest = format!(r#"{{"dependencies": {{"left-pad": "{spec}"}}}}"#);
            assert_eq!(manifest_refusal(&manifest), None, "{spec:?} is admitted");
        }
    }

    #[test]
    fn a_dependency_name_that_is_not_a_package_name_is_refused_and_not_echoed() {
        let manifest = r#"{"dependencies": {"https://user:secret@evil.example.test/x": "1.0.0"}}"#;
        let text = manifest_refusal(manifest).expect("the name is refused");
        assert!(!text.contains("secret") && !text.contains("evil"), "{text}");
    }

    #[test]
    fn a_manifest_that_is_not_json_or_not_an_object_is_refused() {
        assert!(
            manifest_refusal("{not json")
                .expect("refused")
                .contains("not JSON")
        );
        assert!(
            manifest_refusal("[]")
                .expect("refused")
                .contains("not a JSON object")
        );
    }

    #[test]
    fn a_lockfile_spec_that_names_another_source_is_refused_even_when_the_node_has_no_resolved() {
        // An incomplete lockfile: the node names no `resolved`, so only its maps name a source.
        for source in OTHER_SOURCES {
            for lock in [
                format!(
                    r#"{{"lockfileVersion": 3, "packages": {{"": {{"dependencies": {{"left-pad": "{source}"}}}}}}}}"#
                ),
                format!(
                    r#"{{"lockfileVersion": 3, "packages": {{"node_modules/a": {{"version": "1.0.0", "optionalDependencies": {{"left-pad": "{source}"}}}}}}}}"#
                ),
                format!(
                    r#"{{"lockfileVersion": 3, "packages": {{"node_modules/a": {{"version": "1.0.0", "peerDependencies": {{"left-pad": "{source}"}}}}}}}}"#
                ),
                format!(
                    r#"{{"lockfileVersion": 3, "packages": {{"": {{"overrides": {{"left-pad": "{source}"}}}}}}}}"#
                ),
                format!(
                    r#"{{"lockfileVersion": 1, "dependencies": {{"a": {{"version": "1.0.0", "requires": {{"left-pad": "{source}"}}}}}}}}"#
                ),
            ] {
                let text = refusal(&lock).expect("the spec is refused");
                assert!(!text.contains(source), "the spec is not echoed: {text}");
            }
        }
    }

    #[test]
    fn a_lockfile_with_a_registry_spec_in_its_maps_is_admitted() {
        let lock = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": {"dependencies": {"left-pad": "^1.3.0", "alias": "npm:right-pad@1.0.0"}},
                "node_modules/left-pad": {"version": "1.3.0", "dependencies": {"x": "~1"}}
            }
        }"#;
        assert_eq!(refusal(lock), None);
    }

    #[tokio::test]
    async fn a_manifest_that_names_another_source_is_refused_before_any_install_or_cache_entry() {
        let manifest = r#"{"name": "app", "version": "1.0.0", "dependencies": {"left-pad": "https://evil.example.test/left-pad.tgz"}}"#;
        let (clone, head) = clone_with(&[("package.json", manifest), ("package-lock.json", LOCK)]);
        let repo = Repo::open(clone.path()).await.expect("the clone opens");
        let PrepareOutcome::Prepared(prepared) = repo
            .prepare(&head, &ProtectedPaths::default())
            .await
            .expect("the commit prepares")
        else {
            panic!("the commit merges without conflict");
        };
        let app: AppContract = serde_json::from_str(NODE_APP).expect("the app contract decodes");
        let cache_parent = tempfile::tempdir().expect("a cache parent");
        let cache = cache_parent.path().join("deps");
        let scratch = tempfile::tempdir().expect("a scratch directory");
        // No docker runs here, so an install that got past the check would fail differently.
        let config = RunnerConfig::new("acan-no-such-docker", 3000);
        let slots = Semaphore::new(1);
        let installer = Installer {
            repo: &repo,
            config: &config,
            slots: &slots,
            cache: &cache,
            scratch: scratch.path(),
        };
        let me = process_identity().expect("this process's identity");

        let refused = installer
            .install_as(&prepared, Role::Base, &app, me)
            .await
            .expect_err("the manifest names a source the install may not fetch");

        assert!(
            matches!(&refused, DepsError::Manifest { detail, .. } if detail.contains("package.json")),
            "{refused}"
        );
        assert!(
            !refused.to_string().contains("evil.example.test"),
            "{refused}"
        );
        assert!(!cache.exists(), "nothing was stored in the cache");
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

    #[cfg(unix)]
    #[test]
    fn a_sealed_tree_is_private_to_its_owner_and_leaves_links_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("temporary directory");
        let outside = tempfile::tempdir().expect("a target outside the tree");
        let target = outside.path().join("target");
        fs::write(&target, "x").expect("the target");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).expect("its mode");
        let root = dir.path().join("work");
        let modules = root.join(MODULES);
        fs::create_dir_all(&modules).expect("the tree");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o777)).expect("open root");
        fs::set_permissions(&modules, fs::Permissions::from_mode(0o777)).expect("open dir");
        let file = modules.join("file.js");
        fs::write(&file, "y").expect("a file");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o666)).expect("open file");
        std::os::unix::fs::symlink(&target, root.join("link")).expect("a link");

        seal(&root).expect("the tree is sealed");

        let mode_of = |path: &Path| fs::metadata(path).expect("metadata").permissions().mode();
        assert_eq!(
            mode_of(&root) & 0o777,
            0o755,
            "the root is readable, not open"
        );
        assert_eq!(mode_of(&modules) & 0o022, 0, "node_modules is not open");
        assert_eq!(mode_of(&file) & 0o022, 0, "a file is not open");
        assert_eq!(
            mode_of(&target) & 0o777,
            0o644,
            "a link's target is not changed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_cache_root_others_can_write_is_refused_and_a_private_one_is_not() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("temporary directory");
        let cache = dir.path().join("deps");
        prepare_root(&cache).expect("a new root is created");
        assert_eq!(
            fs::metadata(&cache).expect("metadata").permissions().mode() & 0o777,
            0o700,
            "a new root is private"
        );
        let me = process_identity().expect("this process's identity");
        check_root(&cache, me).expect("a private root of this user is accepted");

        fs::set_permissions(&cache, fs::Permissions::from_mode(0o777)).expect("open root");
        let refused = check_root(&cache, me).expect_err("an open root is refused");
        assert!(matches!(refused, DepsError::Tampered { .. }), "{refused}");
    }

    #[cfg(unix)]
    #[test]
    fn a_root_another_user_owns_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("temporary directory");
        let cache = dir.path().join("deps");
        prepare_root(&cache).expect("a new root is created");
        fs::set_permissions(&cache, fs::Permissions::from_mode(0o700)).expect("private root");
        let me = process_identity().expect("this process's identity");
        let foreign = Identity {
            uid: me.uid + 1,
            ..me
        };

        let refused = check_root(&cache, foreign).expect_err("a root of another user is refused");

        assert!(
            matches!(&refused, DepsError::Tampered { detail, .. } if detail.contains("another user")),
            "{refused}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_link_or_an_open_entry_is_refused_and_a_complete_one_is_a_hit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("temporary directory");
        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir_all(elsewhere.join(MODULES)).expect("a target tree");
        fs::write(elsewhere.join(COMPLETE), "").expect("a target marker");

        let good = dir.path().join("good");
        fs::create_dir_all(good.join(MODULES)).expect("a complete entry");
        fs::write(good.join(COMPLETE), "").expect("its marker");
        fs::set_permissions(&good, fs::Permissions::from_mode(0o755)).expect("its mode");
        let me = process_identity().expect("this process's identity");
        assert_eq!(read_entry(&good, me).expect("a read"), Entry::Complete);
        assert_eq!(
            read_entry(&dir.path().join("missing"), me).expect("a read"),
            Entry::Absent
        );

        let linked = dir.path().join("linked");
        std::os::unix::fs::symlink(&elsewhere, &linked).expect("an entry link");
        let refused = read_entry(&linked, me).expect_err("a linked entry is refused");
        assert!(matches!(refused, DepsError::Tampered { .. }), "{refused}");

        let linked_modules = dir.path().join("linked-modules");
        fs::create_dir_all(&linked_modules).expect("an entry");
        fs::write(linked_modules.join(COMPLETE), "").expect("its marker");
        fs::set_permissions(&linked_modules, fs::Permissions::from_mode(0o755)).expect("mode");
        std::os::unix::fs::symlink(elsewhere.join(MODULES), linked_modules.join(MODULES))
            .expect("a modules link");
        let refused =
            read_entry(&linked_modules, me).expect_err("a linked node_modules is refused");
        assert!(matches!(refused, DepsError::Tampered { .. }), "{refused}");

        let open = dir.path().join("open");
        fs::create_dir_all(open.join(MODULES)).expect("an entry");
        fs::write(open.join(COMPLETE), "").expect("its marker");
        fs::set_permissions(&open, fs::Permissions::from_mode(0o777)).expect("open entry");
        let refused = read_entry(&open, me).expect_err("an open entry is refused");
        assert!(matches!(refused, DepsError::Tampered { .. }), "{refused}");
    }

    /// A complete entry at `entry` with one file in its `node_modules`, every directory private.
    #[cfg(unix)]
    fn complete_entry(entry: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let modules = entry.join(MODULES);
        fs::create_dir_all(&modules).expect("an entry");
        let file = modules.join("index.js");
        fs::write(&file, "module.exports = 1;").expect("a file under the entry");
        fs::write(entry.join(COMPLETE), "").expect("its marker");
        for path in [entry, modules.as_path(), file.as_path()] {
            let mode = if path == file { 0o644 } else { 0o755 };
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("a private mode");
        }
        file
    }

    #[cfg(unix)]
    #[test]
    fn an_entry_a_writable_file_or_a_foreign_owner_under_it_is_refused_on_a_hit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("temporary directory");
        let entry = dir.path().join("entry");
        let file = complete_entry(&entry);
        let me = process_identity().expect("this process's identity");
        assert_eq!(read_entry(&entry, me).expect("a read"), Entry::Complete);

        fs::set_permissions(&file, fs::Permissions::from_mode(0o666)).expect("open file");
        let refused = read_entry(&entry, me).expect_err("a file others can write is refused");
        assert!(
            matches!(&refused, DepsError::Tampered { detail, .. } if detail.contains("group or others")),
            "{refused}"
        );
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).expect("private file");

        let foreign = Identity {
            uid: me.uid + 1,
            ..me
        };
        let refused = read_entry(&entry, foreign).expect_err("an entry of another user is refused");
        assert!(
            matches!(&refused, DepsError::Tampered { detail, .. } if detail.contains("another user")),
            "{refused}"
        );
    }

    const PACKAGE: &str =
        r#"{"name": "app", "version": "1.0.0", "dependencies": {"left-pad": "1.3.0"}}"#;
    const LOCK: &str = r#"{
        "name": "app",
        "lockfileVersion": 3,
        "packages": {
            "": {"name": "app", "version": "1.0.0", "dependencies": {"left-pad": "1.3.0"}},
            "node_modules/left-pad": {
                "version": "1.3.0",
                "resolved": "https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz",
                "integrity": "sha512-x"
            }
        }
    }"#;
    const NODE_APP: &str = r#"{
        "version": "acan-app/2",
        "runner_contract": "acan-runner/1",
        "runtime": "node-24",
        "install": ["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"],
        "boot": ["npm", "run", "start"],
        "port_env": "PORT",
        "ready": {"path": "/health", "status": 200, "timeout_ms": 10000},
        "store": "fresh_per_sequence",
        "clock": {"kind": "none"},
        "sinks": [],
        "env": {},
        "limits": {"max_requests": 64, "max_output_bytes": 65536, "sequence_timeout_ms": 30000}
    }"#;

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=Tester",
                "-c",
                "user.email=tester@localhost",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// A clone on `main` whose only commit holds `files`, and that commit.
    fn clone_with(files: &[(&str, &str)]) -> (tempfile::TempDir, CommitSha) {
        let dir = tempfile::tempdir().expect("temporary clone");
        git(dir.path(), &["init", "-q", "-b", "main"]);
        for (path, text) in files {
            let full = dir.path().join(path);
            fs::create_dir_all(full.parent().expect("a file has a parent")).expect("a directory");
            fs::write(&full, text).expect("a file");
        }
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-q", "-m", "base"]);
        let head = CommitSha::from_str(&git(dir.path(), &["rev-parse", "HEAD"])).expect("a commit");
        (dir, head)
    }

    /// The cache key of the node fixture's install, as `install` computes it.
    fn fixture_key(app: &AppContract) -> String {
        let image = ContainerPolicy::node_install()
            .image
            .digest
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default();
        let argv = app.install.as_ref().expect("an install").as_slice();
        let manifests = manifests_of(&[("package.json", PACKAGE)]);
        key_of(&image, argv, LOCK.as_bytes(), &manifests)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_hit_is_refused_under_an_open_root_or_a_root_another_user_owns() {
        use std::os::unix::fs::PermissionsExt;
        let (clone, head) = clone_with(&[("package.json", PACKAGE), ("package-lock.json", LOCK)]);
        let repo = Repo::open(clone.path()).await.expect("the clone opens");
        let PrepareOutcome::Prepared(prepared) = repo
            .prepare(&head, &ProtectedPaths::default())
            .await
            .expect("the commit prepares")
        else {
            panic!("the commit merges without conflict");
        };
        let app: AppContract = serde_json::from_str(NODE_APP).expect("the app contract decodes");
        let cache_parent = tempfile::tempdir().expect("a cache parent");
        let cache = cache_parent.path().join("deps");
        prepare_root(&cache).expect("the cache root");
        let entry = cache.join(fixture_key(&app).replace(':', "-"));
        complete_entry(&entry);
        let scratch = tempfile::tempdir().expect("a scratch directory");
        let config = RunnerConfig::new("acan-no-such-docker", 3000);
        let slots = Semaphore::new(1);
        let installer = Installer {
            repo: &repo,
            config: &config,
            slots: &slots,
            cache: &cache,
            scratch: scratch.path(),
        };
        let me = process_identity().expect("this process's identity");

        let hit = installer
            .install_as(&prepared, Role::Base, &app, me)
            .await
            .expect("a private root of this user hits")
            .expect("the app has an install");
        assert_eq!(hit.record.cache, Cache::Hit);

        fs::set_permissions(&cache, fs::Permissions::from_mode(0o777)).expect("an open root");
        let refused = installer
            .install_as(&prepared, Role::Base, &app, me)
            .await
            .expect_err("an open root is refused before its entry is read");
        assert!(matches!(refused, DepsError::Tampered { .. }), "{refused}");

        fs::set_permissions(&cache, fs::Permissions::from_mode(0o700)).expect("a private root");
        let foreign = Identity {
            uid: me.uid + 1,
            ..me
        };
        let refused = installer
            .install_as(&prepared, Role::Base, &app, foreign)
            .await
            .expect_err("a root another user owns is refused before its entry is read");
        assert!(
            matches!(&refused, DepsError::Tampered { detail, .. } if detail.contains("another user")),
            "{refused}"
        );
    }

    #[test]
    fn an_explicit_cache_dir_is_named_absolute() {
        let dir = cache_dir(Some(Path::new("relative/cache"))).expect("a directory is named");
        assert!(dir.is_absolute(), "{}", dir.display());
        assert!(dir.ends_with("relative/cache"), "{}", dir.display());
    }
}
