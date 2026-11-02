//! The worker's local tools. Every path stays inside the task's worktree, a write is allowed
//! only to a file the task declared, and no tool writes or commits a protected path, declared
//! or not. The worker decides when a write may run (see `worker`).
//!
//! The path checks bind these tools and nothing else; they are not a sandbox. Code the test
//! command runs is held back only by the container [`Worktree::run_tests`] starts, and when no
//! container runtime answers, the test command does not run at all. The container's limits are
//! read from a [`ContainerPolicy`]; this module only renders that policy as `docker run`
//! arguments. A protected path is mounted from the protected-head snapshot, never from the
//! worktree, so a protected file the task edited is not what the container sees.

use std::collections::BTreeSet;
use std::fmt::Display;
use std::num::NonZeroU32;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use acan_intent::{
    ContainerImage, ContainerNetwork, ContainerPolicy, MissingProtected, Privileges, RepoPath,
    RootFilesystem,
};
use acan_llm::ToolSpec;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

/// The most bytes one read returns. A longer file comes in pages: each cut page is marked with
/// the offset of the next one, and a read from that offset returns it.
const MAX_READ_BYTES: usize = 32 * 1024;
/// The most entries a directory listing returns.
const MAX_LISTING: usize = 200;

/// Paths no tool writes or commits, whatever the task declares or the policy lists. A floor:
/// a policy can add protected paths but cannot drop these.
const ALWAYS_PROTECTED: &[&str] = &[".acan"];

/// How every refusal for a missing or failed container runtime begins.
const ISOLATION_UNAVAILABLE: &str = "isolation unavailable";
const DEFAULT_DOCKER: &str = "docker";
/// The writable temporary directory, which is also `HOME`. Its size comes from the policy.
const TMP_DIR: &str = "/tmp";
const DOCKER_ANSWER_TIMEOUT: Duration = Duration::from_secs(15);
/// Bytes kept from the end of the test command's stdout and of its stderr.
const OUTPUT_TAIL_BYTES: usize = 4 * 1024;

/// One task's worktree, with the files it declared, the paths it may never change, the test
/// command it may run, and the container policy that command runs under.
#[derive(Debug)]
pub struct Worktree {
    root: PathBuf,
    declared: Vec<String>,
    /// The protected paths the caller added with [`Worktree::with_protected`].
    extra_protected: Vec<String>,
    test_command: Option<String>,
    container: Container,
    policy: ContainerPolicy,
    /// The protected-head snapshot the container's protected paths are mounted from. Unset, the
    /// test command does not run, because there is nothing safe to mount them from.
    snapshot: Option<PathBuf>,
}

/// Where a covered path's mount comes from.
#[derive(Clone, Copy)]
enum Origin {
    /// The worktree itself, read-only: `.git`, which the policy keeps read-only.
    Worktree,
    /// The protected-head snapshot: every protected path.
    Snapshot,
}

/// The docker CLI the test command runs through.
#[derive(Debug)]
struct Container {
    docker: PathBuf,
}

/// The `docker run` arguments of one test run, and what the run puts on the host.
struct Plan {
    args: Vec<String>,
    /// The empty directory bound read-only over each masked path. Created before the run.
    mask_source: Option<PathBuf>,
    /// Host directories that were missing when the plan was made and that docker creates as the
    /// mount points of masked paths, deepest first.
    mount_points: Vec<PathBuf>,
}

impl Plan {
    /// Removes what the run left on the host. A directory is removed only when it is empty, and
    /// a file only when it is empty and docker made it as a mount point, so a file the candidate
    /// wrote into the worktree is never deleted.
    fn clean_up(&self) {
        for path in &self.mount_points {
            let empty_file =
                std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.len() == 0);
            if empty_file {
                let _ = std::fs::remove_file(path);
            } else {
                let _ = std::fs::remove_dir(path);
            }
        }
        if let Some(source) = &self.mask_source {
            let _ = std::fs::remove_dir(source);
        }
    }
}

/// A path inside the worktree: as joined from the request, and as it really is on disk.
struct Resolved {
    path: PathBuf,
    /// The `/`-separated path from the worktree root after every link, case difference and
    /// short name is resolved.
    real: String,
}

impl Worktree {
    /// The worktree at `root` for a task that declared `declared`. The declared paths are
    /// normalized the way [`normalize_path`] does; a path that cannot be normalized is dropped,
    /// so it can never be written. The worker only passes paths it has already normalized.
    ///
    /// `.acan/` is protected from the start. The test command runs with `docker` from `PATH`
    /// under [`ContainerPolicy::default_run`], and it runs only once a protected-head snapshot
    /// is set with [`Worktree::with_snapshot`].
    #[must_use]
    pub fn new(root: PathBuf, declared: &[String], test_command: Option<String>) -> Self {
        let declared = declared
            .iter()
            .filter_map(|path| normalize_path(path).ok())
            .collect();
        Self {
            root,
            declared,
            extra_protected: Vec::new(),
            test_command,
            container: Container {
                docker: PathBuf::from(DEFAULT_DOCKER),
            },
            policy: ContainerPolicy::default_run(),
            snapshot: None,
        }
    }

    /// Protects `paths` as well as `.acan/` and the policy's protected paths. A protected path
    /// covers itself and everything under it. No tool writes, restores or commits it, even when
    /// the task declares it, and the test command's container mounts it read-only.
    #[must_use]
    pub fn with_protected(mut self, paths: &[RepoPath]) -> Self {
        self.extra_protected
            .extend(paths.iter().map(|path| protected_form(path.as_str())));
        self
    }

    /// Runs the test command's container through the docker CLI at `docker`.
    #[must_use]
    pub fn with_docker(mut self, docker: impl Into<PathBuf>) -> Self {
        self.container.docker = docker.into();
        self
    }

    /// Runs the test command under `policy` instead of the default one.
    #[must_use]
    pub fn with_policy(mut self, policy: ContainerPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Mounts the protected paths from the protected-head snapshot at `snapshot`, a directory
    /// holding the protected paths as they are at the task's base. The worktree is never the
    /// source of a protected mount.
    #[must_use]
    pub fn with_snapshot(mut self, snapshot: impl Into<PathBuf>) -> Self {
        self.snapshot = Some(snapshot.into());
        self
    }

    /// The container policy the test command runs under. Its hash is the sandbox policy hash
    /// of the run.
    #[must_use]
    pub fn container_policy(&self) -> &ContainerPolicy {
        &self.policy
    }

    /// The local tools, as the model sees them.
    #[must_use]
    pub fn specs() -> Vec<ToolSpec> {
        vec![
            spec(
                "read_file",
                &format!(
                    "Read a file in the worktree. A file longer than {} KB comes in pages: a cut page names the offset of the next one, and you read on by passing that offset.",
                    MAX_READ_BYTES / 1024
                ),
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "offset": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "The byte offset to start from. Omit it to start at the beginning of the file.",
                        },
                    },
                    "required": ["path"],
                }),
            ),
            spec(
                "list_files",
                "List one directory in the worktree. An empty path lists the root.",
                json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                }),
            ),
            spec(
                "write_file",
                "Write a file you declared. Protected paths such as .acan/ are never written. The write waits until the origin clears the task to build.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" },
                    },
                    "required": ["path", "content"],
                }),
            ),
            spec(
                "run_tests",
                "Run the test command the worker was started with, in a container over the worktree with no network and protected paths read-only.",
                json!({ "type": "object", "properties": {} }),
            ),
            spec(
                "commit",
                "Commit every change in the worktree. Only declared files may change, and protected paths may not change at all.",
                json!({
                    "type": "object",
                    "properties": { "message": { "type": "string" } },
                    "required": ["message"],
                }),
            ),
        ]
    }

    /// Reads one page of a file, from the byte `offset`. A page is at most [`MAX_READ_BYTES`]
    /// long; when more of the file follows, the text ends with a marker naming the offset of
    /// the next page. Reading from offset zero gives the start of the file.
    ///
    /// # Errors
    ///
    /// Returns the reason when the path is not inside the worktree, cannot be read, or the
    /// offset is past the end of the file.
    pub async fn read_file(&self, path: &str, offset: u64) -> Result<String, String> {
        let relative = normalize_path(path)?;
        let bytes = tokio::fs::read(self.inside(&relative)?)
            .await
            .map_err(|err| format!("cannot read {relative}: {err}"))?;
        let from = usize::try_from(offset).unwrap_or(usize::MAX);
        if from > bytes.len() {
            return Err(format!(
                "offset {offset} is past the end of {relative}, which is {} bytes long",
                bytes.len()
            ));
        }
        let (start, end) = page_bounds(&bytes, from);
        let mut text = String::from_utf8_lossy(&bytes[start..end]).into_owned();
        if end < bytes.len() {
            text.push_str(&format!(
                "\n[truncated: bytes {start} to {end} of {} shown. Call read_file with offset {end} for the rest.]",
                bytes.len()
            ));
        }
        Ok(text)
    }

    /// Lists one directory, directories first marked with a trailing slash. `.git` is hidden.
    ///
    /// # Errors
    ///
    /// Returns the reason when the path is not inside the worktree or cannot be listed.
    pub fn list_files(&self, path: &str) -> Result<String, String> {
        let dir = if path.trim().is_empty() || path == "." {
            self.root.clone()
        } else {
            self.inside(&normalize_path(path)?)?
        };
        let entries = std::fs::read_dir(&dir)
            .map_err(|err| format!("cannot list {}: {err}", dir.display()))?;
        let mut names: Vec<String> = entries
            .filter_map(Result::ok)
            .map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                if entry.path().is_dir() {
                    format!("{name}/")
                } else {
                    name
                }
            })
            .filter(|name| name != ".git/" && name != ".git")
            .collect();
        names.sort();
        names.truncate(MAX_LISTING);
        Ok(names.join("\n"))
    }

    /// Writes a declared file, creating its directories. The write does not go through a link,
    /// so it lands on the declared path and nowhere else, and never on a protected path.
    ///
    /// # Errors
    ///
    /// Returns the reason when the path is protected under any spelling, the file is not
    /// declared, its path runs through a link, or it cannot be written.
    pub fn write_file(&self, path: &str, content: &str) -> Result<String, String> {
        let relative = normalize_path(path)?;
        self.refuse_protected(&relative)?;
        if !self.declared.contains(&relative) {
            return Err(format!(
                "{relative} is not declared; declared files are: {}",
                self.declared.join(", ")
            ));
        }
        self.no_links(&relative)?;
        let resolved = self.resolve(&relative)?;
        // A Windows short name such as `ACAN~1` names a protected directory the plain spelling
        // check cannot see; the resolved path shows it.
        self.refuse_protected(&resolved.real)?;
        if let Some(parent) = resolved.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("cannot create the directory for {relative}: {err}"))?;
        }
        std::fs::write(&resolved.path, content)
            .map_err(|err| format!("cannot write {relative}: {err}"))?;
        Ok(format!("wrote {} bytes to {relative}", content.len()))
    }

    /// Runs the configured test command in a fresh container over the worktree and reports
    /// its outcome. The command never runs on the host: when the container runtime is missing
    /// or does not answer, or the policy cannot be applied, the reason begins with
    /// `isolation unavailable`.
    ///
    /// # Errors
    ///
    /// Returns the reason when no test command is configured, the container cannot run, or
    /// the command timed out.
    pub async fn run_tests(&self) -> Result<String, String> {
        let Some(command) = &self.test_command else {
            return Err(no_test_command());
        };
        let name = container_name();
        let plan = self.container_plan(&name, command).map_err(unavailable)?;
        self.container.answers().await?;
        if let Some(source) = &plan.mask_source {
            std::fs::create_dir(source)
                .map_err(|err| unavailable(format!("cannot create {}: {err}", source.display())))?;
        }
        let timeout = Duration::from_millis(self.policy.timeout_ms.get());
        let outcome = self.container.run(&name, &plan.args, timeout).await;
        plan.clean_up();
        outcome
    }

    /// The `docker` arguments that run the test command in a container named `name`. Nothing is
    /// created on the host, so the arguments can be checked without a container runtime.
    ///
    /// # Errors
    ///
    /// Returns the reason when no test command is configured, the policy names a network this
    /// runner cannot apply, a protected path is missing and the policy refuses that, or a path
    /// runs through a link or cannot be mounted.
    pub fn test_container_args(&self, name: &str) -> Result<Vec<String>, String> {
        let command = self.test_command.as_deref().ok_or_else(no_test_command)?;
        self.container_plan(name, command).map(|plan| plan.args)
    }

    /// The `docker` arguments and host bookkeeping for `command` in a container named `name`.
    /// Every limit, mount, network and environment rule comes from the policy, except the fixed
    /// rules this function states: a fresh container removed after the run, and every protected
    /// path mounted read-only on top of the worktree from the snapshot. A protected path the
    /// snapshot lacks is covered as the policy's `missing_protected` says, never from the
    /// worktree. `.git` is mounted read-only from the worktree only when `readonly_git` says so.
    fn container_plan(&self, name: &str, command: &str) -> Result<Plan, String> {
        let policy = &self.policy;
        policy
            .check_environment()
            .map_err(|err| format!("the container policy is refused: {err}"))?;
        let root = std::path::absolute(&self.root)
            .map_err(|err| format!("the worktree cannot be resolved: {err}"))?;
        let snapshot = self.snapshot_root()?;
        let workdir = policy.mounts.workdir.as_str();
        let mut args: Vec<String> = ["run", "--rm", "--name", name].map(str::to_owned).to_vec();
        args.extend(network_args(policy.network));
        args.extend(root_args(policy.root));
        args.extend(privilege_args(policy.privileges));
        args.extend([
            "--user".to_owned(),
            policy.user.to_string(),
            "--cpus".to_owned(),
            cpus(policy.cpus_milli),
            "--memory".to_owned(),
            policy.memory_bytes.to_string(),
            // Equal to `--memory`, so the container gets no swap beyond its memory cap.
            "--memory-swap".to_owned(),
            policy.memory_bytes.to_string(),
            "--pids-limit".to_owned(),
            policy.pids.to_string(),
            "--tmpfs".to_owned(),
            format!("{TMP_DIR}:rw,nosuid,nodev,size={}", policy.tmp_bytes),
        ]);
        // Only the variables the policy names reach the container, and no other host variable.
        for (key, value) in &policy.environment {
            args.extend(["--env".to_owned(), format!("{key}={value}")]);
        }
        args.extend(["--mount".to_owned(), bind(&root, workdir)?]);

        let protected = self.protected_paths();
        // The `.git` guard is mounted from the worktree, so a protected path under `.git` would
        // be the worktree's copy. While the guard is on, that path is refused instead.
        if policy.mounts.readonly_git
            && let Some(path) = protected.iter().find(|path| is_git_dir_or_below(path))
        {
            return Err(format!(
                "{path} is protected and lies under .git, which readonly_git mounts from the worktree; set readonly_git to false to mount it from the snapshot"
            ));
        }
        // Hooks written under `.git` would run on the host at the next commit, so the policy
        // keeps it read-only unless `readonly_git` turns that off.
        let guarded = policy
            .mounts
            .readonly_git
            .then_some((".git", Origin::Worktree));
        let sources = guarded.into_iter().chain(
            protected
                .iter()
                .map(|path| (path.as_str(), Origin::Snapshot)),
        );
        let mut covered: Vec<String> = Vec::new();
        let mut mount_points: Vec<PathBuf> = Vec::new();
        let mut mask_source: Option<PathBuf> = None;
        for (relative, origin) in sources {
            if covered.iter().any(|covering| covers(covering, relative)) {
                continue;
            }
            let source = match origin {
                Origin::Worktree => self
                    .existing_real(relative)?
                    .map(|real| host_path(&root, &real)),
                Origin::Snapshot => {
                    self.no_links(relative)?;
                    snapshot_source(&snapshot, relative)?
                }
            };
            let target = format!("{workdir}/{relative}");
            match source {
                Some(source) => {
                    // Docker makes a mount point the worktree lacks. The run removes what it
                    // made, and nothing when the worktree already has the path.
                    mount_points.extend(missing_chain(&root, relative).into_iter().rev());
                    args.extend([
                        "--mount".to_owned(),
                        format!("{},readonly", bind(&source, &target)?),
                    ]);
                }
                None => match policy.mounts.missing_protected {
                    MissingProtected::Refused => {
                        let from = match origin {
                            Origin::Worktree => "the worktree",
                            Origin::Snapshot => "the protected-head snapshot",
                        };
                        return Err(format!(
                            "{relative} is protected and missing from {from}, and the policy refuses a run without it"
                        ));
                    }
                    MissingProtected::Masked => {
                        // Masking binds an empty read-only directory over the path, so a copy the
                        // task added in the worktree is covered too.
                        self.no_links(relative)?;
                        mount_points.extend(missing_chain(&root, relative).into_iter().rev());
                        let source = mask_source.get_or_insert_with(|| mask_dir(name));
                        args.extend([
                            "--mount".to_owned(),
                            format!("{},readonly", bind(source.as_path(), &target)?),
                        ]);
                    }
                },
            }
            covered.push(relative.to_owned());
        }

        args.extend([
            "--workdir".to_owned(),
            workdir.to_owned(),
            "--entrypoint".to_owned(),
            "sh".to_owned(),
        ]);
        args.push(image_reference(&policy.image));
        args.extend(["-c".to_owned(), command.to_owned()]);
        Ok(Plan {
            args,
            mask_source,
            mount_points,
        })
    }

    /// Stages every change, refuses it when a protected path or a file outside the declaration
    /// changed, and commits. Returns the new commit id.
    ///
    /// # Errors
    ///
    /// Returns the reason when git fails, when nothing changed, or when a protected path or a
    /// file outside the declaration changed, deleted or renamed included.
    pub async fn commit(&self, message: &str) -> Result<String, String> {
        git(&self.root, &["add", "-A"]).await?;
        // Without `--no-renames`, a protected file renamed to a declared path lists only the
        // declared side, and the deletion of the protected file would go unseen.
        let staged = git(
            &self.root,
            &["diff", "--cached", "--name-only", "--no-renames", "-z"],
        )
        .await?;
        let changed: Vec<&str> = staged.split('\0').filter(|file| !file.is_empty()).collect();
        let protected: Vec<&str> = changed
            .iter()
            .copied()
            .filter(|file| self.is_protected(file))
            .collect();
        let outside: Vec<&str> = changed
            .iter()
            .copied()
            .filter(|file| !self.is_protected(file) && !self.declared.iter().any(|d| d == file))
            .collect();
        let mut reasons = Vec::new();
        if !protected.is_empty() {
            reasons.push(format!("{} is protected", protected.join(", ")));
        }
        if !outside.is_empty() {
            reasons.push(format!(
                "{} changed outside the declared files",
                outside.join(", ")
            ));
        }
        if !reasons.is_empty() {
            // The refused change stays in the worktree, unstaged, for the worker to see.
            let _ = git(&self.root, &["reset", "-q"]).await;
            return Err(format!("commit refused: {}", reasons.join("; ")));
        }
        git(&self.root, &["commit", "-q", "-m", message]).await?;
        Ok(git(&self.root, &["rev-parse", "HEAD"])
            .await?
            .trim()
            .to_owned())
    }

    /// Sets the files the task declares now. Writes to any other file are refused from here on.
    pub fn declare(&mut self, declared: &[String]) {
        self.declared = declared
            .iter()
            .filter_map(|path| normalize_path(path).ok())
            .collect();
    }

    /// Restores files a narrowing took from the declaration. A tracked file goes back to its
    /// committed version. A file the task never committed is removed, so a later commit cannot
    /// carry the change. A protected path is left alone: no tool wrote it, so there is nothing
    /// to undo, and removing it would delete a protected input.
    ///
    /// # Errors
    ///
    /// Returns the reason when git cannot restore a file or a file cannot be removed.
    pub async fn restore(&self, files: &[String]) -> Result<(), String> {
        for relative in files {
            if self.is_protected(relative) {
                continue;
            }
            let path = self.inside(relative)?;
            // `ls-files` prints the path when git tracks it and nothing when it does not. A git
            // failure is returned as an error, so a file is never removed on a guess.
            let listed = git(
                &self.root,
                &["--literal-pathspecs", "ls-files", "--", relative],
            )
            .await?;
            if listed.trim().is_empty() {
                if path.exists() {
                    std::fs::remove_file(&path)
                        .map_err(|err| format!("cannot remove {relative}: {err}"))?;
                }
            } else {
                git(
                    &self.root,
                    &["--literal-pathspecs", "checkout", "-q", "--", relative],
                )
                .await?;
            }
        }
        Ok(())
    }

    /// Every protected path: `.acan/`, the policy's and the caller's. Sorted, so a path comes
    /// after the path that covers it.
    fn protected_paths(&self) -> BTreeSet<String> {
        ALWAYS_PROTECTED
            .iter()
            .map(|&path| path.to_owned())
            .chain(
                self.policy
                    .mounts
                    .protected
                    .iter()
                    .map(|path| protected_form(path.as_str())),
            )
            .chain(self.extra_protected.iter().cloned())
            .collect()
    }

    /// Whether `relative` is a protected path or lies under one. Case is ignored, because
    /// Windows and macOS name one file by every case of its name.
    fn is_protected(&self, relative: &str) -> bool {
        let path = relative.to_lowercase();
        self.protected_paths().iter().any(|protected| {
            let protected = protected.to_lowercase();
            path.strip_prefix(&protected)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        })
    }

    fn refuse_protected(&self, relative: &str) -> Result<(), String> {
        if self.is_protected(relative) {
            return Err(format!(
                "{relative} is protected; no tool writes or commits it, even when declared"
            ));
        }
        Ok(())
    }

    /// The path inside the worktree. Every existing link on the way is resolved, so a symlink
    /// that points outside the worktree is refused, and so is a link that cannot be resolved.
    fn inside(&self, relative: &str) -> Result<PathBuf, String> {
        self.resolve(relative).map(|resolved| resolved.path)
    }

    fn resolve(&self, relative: &str) -> Result<Resolved, String> {
        let joined = self.root.join(relative);
        let root = std::fs::canonicalize(&self.root)
            .map_err(|err| format!("the worktree cannot be resolved: {err}"))?;
        // `symlink_metadata` sees a dangling link too, so the link is resolved and refused.
        let mut probe = joined.as_path();
        while probe.symlink_metadata().is_err() {
            probe = probe
                .parent()
                .ok_or_else(|| format!("{relative} is outside the worktree"))?;
        }
        let real = std::fs::canonicalize(probe)
            .map_err(|err| format!("cannot resolve {relative}: {err}"))?;
        let below_root = real
            .strip_prefix(&root)
            .map_err(|_| format!("{relative} resolves outside the worktree"))?;
        let missing = joined
            .strip_prefix(probe)
            .map_err(|_| format!("cannot resolve {relative}"))?;
        let mut parts = Vec::new();
        for component in below_root.components().chain(missing.components()) {
            match component {
                Component::Normal(part) => {
                    parts.push(part.to_str().ok_or_else(|| {
                        format!("{relative} resolves to a name that is not UTF-8")
                    })?);
                }
                _ => return Err(format!("cannot resolve {relative}")),
            }
        }
        Ok(Resolved {
            real: parts.join("/"),
            path: joined,
        })
    }

    /// Refuses a write whose path runs through a link. The declaration names one file, and a link
    /// can lead to any other file, so no write follows one.
    fn no_links(&self, relative: &str) -> Result<(), String> {
        let mut current = self.root.clone();
        for part in relative.split('/') {
            current.push(part);
            match std::fs::symlink_metadata(&current) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(format!(
                        "{relative} runs through a link, and writes do not follow links"
                    ));
                }
                Ok(_) => {}
                // Nothing exists past a missing component, so the rest is created as plain files
                // and directories.
                Err(_) => break,
            }
        }
        Ok(())
    }

    /// The resolved form of `relative` when it exists, refusing one that runs through a link,
    /// because a read-only mount of a link would expose whatever it points to.
    fn existing_real(&self, relative: &str) -> Result<Option<String>, String> {
        match self.root.join(relative).symlink_metadata() {
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(format!("cannot inspect {relative}: {err}")),
        }
        self.no_links(relative)?;
        self.resolve(relative).map(|resolved| Some(resolved.real))
    }

    /// The absolute snapshot directory. Without one, no protected path can be mounted from a
    /// source the task did not write, so the run is refused rather than mounted from the worktree.
    fn snapshot_root(&self) -> Result<PathBuf, String> {
        let snapshot = self.snapshot.as_deref().ok_or_else(|| {
            "no protected-head snapshot is set, so protected paths have nothing to be mounted from"
                .to_owned()
        })?;
        std::path::absolute(snapshot)
            .map_err(|err| format!("the protected-head snapshot cannot be resolved: {err}"))
    }
}

impl Container {
    /// Checks that the docker CLI starts and its daemon answers.
    async fn answers(&self) -> Result<(), String> {
        let mut version = Command::new(&self.docker);
        version
            .args(["version", "--format", "{{.Server.Version}}"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let output = match tokio::time::timeout(DOCKER_ANSWER_TIMEOUT, version.output()).await {
            Err(_) => return Err(unavailable("the docker daemon did not answer")),
            Ok(Err(err)) => {
                return Err(unavailable(format!(
                    "{} did not start: {err}",
                    self.docker.display()
                )));
            }
            Ok(Ok(output)) => output,
        };
        if output.status.success() {
            return Ok(());
        }
        let said = String::from_utf8_lossy(&output.stderr);
        Err(unavailable(format!(
            "the docker daemon is not reachable: {}",
            said.trim()
        )))
    }

    async fn run(&self, name: &str, args: &[String], timeout: Duration) -> Result<String, String> {
        let mut child = Command::new(&self.docker)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| {
                unavailable(format!("{} did not start: {err}", self.docker.display()))
            })?;
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            return Err("the docker output is not piped".to_owned());
        };
        let stdout = tokio::spawn(read_tail(stdout));
        let stderr = tokio::spawn(read_tail(stderr));
        let status = match tokio::time::timeout(timeout, child.wait()).await {
            Err(_) => {
                // Killing the docker CLI leaves its container running, so the container is
                // killed by name.
                self.kill(name).await;
                let _ = child.kill().await;
                return Err(format!(
                    "the test command timed out after {timeout:?}; its container was killed"
                ));
            }
            Ok(Err(err)) => return Err(format!("could not wait on docker: {err}")),
            Ok(Ok(status)) => status,
        };
        let output = shown(
            &stdout.await.unwrap_or_default(),
            &stderr.await.unwrap_or_default(),
        );
        match status.code() {
            Some(0) => Ok("pass".to_owned()),
            // 125 is docker's own failure: the container never ran the command.
            Some(125) => Err(unavailable(format!(
                "docker could not start the container{output}"
            ))),
            Some(code) => Ok(format!("fail: exit code {code}{output}")),
            None => Ok(format!("fail: killed by a signal{output}")),
        }
    }

    async fn kill(&self, name: &str) {
        let mut kill = Command::new(&self.docker);
        kill.args(["kill", name])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let _ = tokio::time::timeout(DOCKER_ANSWER_TIMEOUT, kill.status()).await;
    }
}

/// The `--network` arguments of the policy. The policy can name no other network: a container
/// that publishes no port has no route to the host or to the rest of the network.
fn network_args(network: ContainerNetwork) -> Vec<String> {
    match network {
        ContainerNetwork::None {} => vec!["--network".to_owned(), "none".to_owned()],
    }
}

fn root_args(root: RootFilesystem) -> Vec<String> {
    match root {
        RootFilesystem::ReadOnly => vec!["--read-only".to_owned()],
    }
}

fn privilege_args(privileges: Privileges) -> Vec<String> {
    match privileges {
        Privileges::Dropped => ["--cap-drop", "ALL", "--security-opt", "no-new-privileges"]
            .map(str::to_owned)
            .to_vec(),
    }
}

/// The `--cpus` value for a cap in thousandths of a core, exact to the thousandth.
fn cpus(milli: NonZeroU32) -> String {
    let (whole, frac) = (milli.get() / 1000, milli.get() % 1000);
    if frac == 0 {
        return whole.to_string();
    }
    format!("{whole}.{frac:03}")
        .trim_end_matches('0')
        .to_owned()
}

/// The image reference docker runs: the name, and `@digest` when the policy pins one.
fn image_reference(image: &ContainerImage) -> String {
    match &image.digest {
        Some(digest) => format!("{}@{digest}", image.name),
        None => image.name.to_string(),
    }
}

/// The host directory that stands in for a masked path. It is named after the container, so
/// two runs never share one.
fn mask_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("{name}-mask"))
}

/// The directories `relative` names under `root` that do not exist yet, top first. Nothing
/// exists past the first missing one, so the chain runs to the end of `relative`.
fn missing_chain(root: &Path, relative: &str) -> Vec<PathBuf> {
    let mut path = root.to_path_buf();
    let mut chain = Vec::new();
    for part in relative.split('/') {
        path.push(part);
        let missing = matches!(
            path.symlink_metadata(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound
        );
        if !chain.is_empty() || missing {
            chain.push(path.clone());
        }
    }
    chain
}

/// Whether a mount of `covering` already covers `path`: they are the same path, or `path` lies
/// under `covering`.
fn covers(covering: &str, path: &str) -> bool {
    path == covering
        || path
            .strip_prefix(covering)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Whether `path` is `.git` or lies under it. Case is ignored, because Windows and macOS name one
/// directory by every case of its name.
fn is_git_dir_or_below(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower == ".git" || lower.starts_with(".git/")
}

/// The host path of a `/`-separated path under the worktree.
fn host_path(root: &Path, relative: &str) -> PathBuf {
    relative
        .split('/')
        .fold(root.to_path_buf(), |path, part| path.join(part))
}

/// The host path of `relative` in the protected-head snapshot, or nothing when the snapshot does
/// not hold it. A link on the way is refused, because a mount of a link exposes its target.
fn snapshot_source(snapshot: &Path, relative: &str) -> Result<Option<PathBuf>, String> {
    let mut path = snapshot.to_path_buf();
    for part in relative.split('/') {
        path.push(part);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!(
                    "the snapshot's {relative} runs through a link, and a mount does not follow links"
                ));
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(format!("cannot inspect the snapshot's {relative}: {err}")),
        }
    }
    Ok(Some(path))
}

/// The form a protected path is kept in: normalized when it can be, otherwise as given.
fn protected_form(path: &str) -> String {
    normalize_path(path).unwrap_or_else(|_| path.to_owned())
}

/// A `--mount` value binding `source` on the host to `target` in the container.
fn bind(source: &Path, target: &str) -> Result<String, String> {
    let source = source
        .to_str()
        .ok_or_else(|| format!("{} is not UTF-8", source.display()))?;
    // docker reads `--mount` as comma-separated fields, so a comma or quote would end a field.
    for path in [source, target] {
        if path.contains([',', '"', '\n', '\r']) {
            return Err(format!("{path:?} cannot be mounted"));
        }
    }
    Ok(format!("type=bind,source={source},target={target}"))
}

/// A container name no other run of this or another worker shares.
fn container_name() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    format!(
        "acan-test-{}-{nanos}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

fn no_test_command() -> String {
    "no test command is configured for this worker".to_owned()
}

fn unavailable(detail: impl Display) -> String {
    format!("{ISOLATION_UNAVAILABLE}: {detail}")
}

/// The ends of the command's output, each on its own lines, or nothing when it printed none.
fn shown(stdout: &[u8], stderr: &[u8]) -> String {
    [("stdout", stdout), ("stderr", stderr)]
        .into_iter()
        .filter(|(_, bytes)| !bytes.is_empty())
        .map(|(label, bytes)| {
            format!(
                "\n{label} (last {} bytes):\n{}",
                bytes.len(),
                String::from_utf8_lossy(bytes)
            )
        })
        .collect()
}

/// The last [`OUTPUT_TAIL_BYTES`] bytes `reader` yields.
async fn read_tail<R: AsyncRead + Unpin>(mut reader: R) -> Vec<u8> {
    let mut tail = Vec::new();
    let mut buf = [0u8; 8192];
    while let Ok(read @ 1..) = reader.read(&mut buf).await {
        tail.extend_from_slice(&buf[..read]);
        if tail.len() > 2 * OUTPUT_TAIL_BYTES {
            tail.drain(..tail.len() - OUTPUT_TAIL_BYTES);
        }
    }
    let excess = tail.len().saturating_sub(OUTPUT_TAIL_BYTES);
    tail.drain(..excess);
    tail
}

fn spec(name: &str, description: &str, parameters: Value) -> ToolSpec {
    ToolSpec {
        name: name.to_owned(),
        description: description.to_owned(),
        parameters,
    }
}

/// The byte range one read page covers, from `from` forward to [`MAX_READ_BYTES`]. Both ends
/// move to a character boundary, so a multi-byte character is never split between two pages:
/// the start moves forward past continuation bytes, and the end moves back until it sits on a
/// lead byte or the end of the file. `from` must not be past the end of `bytes`.
fn page_bounds(bytes: &[u8], from: usize) -> (usize, usize) {
    let continues = |index: usize| bytes.get(index).is_some_and(|byte| byte & 0xC0 == 0x80);
    let mut start = from;
    while continues(start) {
        start += 1;
    }
    let mut end = bytes.len().min(start + MAX_READ_BYTES);
    while end < bytes.len() && continues(end) {
        end -= 1;
    }
    (start, end)
}

/// The repository-relative form of `path` that the origin keeps: `/`-separated normal components.
/// A `.` is dropped, so `./model/store.ts` and `model/store.ts` are one path to the origin and to
/// the worktree alike.
///
/// # Errors
///
/// Returns the reason when the path is absolute, climbs out with `..`, names nothing, is inside
/// `.git`, or has a name the origin does not keep as a path.
pub fn normalize_path(path: &str) -> Result<String, String> {
    let mut parts = Vec::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => {
                let part = part
                    .to_str()
                    .ok_or_else(|| format!("{path:?} is not UTF-8"))?;
                if part.eq_ignore_ascii_case(".git") {
                    return Err(format!("{path} is inside .git"));
                }
                // The origin keeps neither `\` nor `:`, and Windows drops a trailing dot or space,
                // so such a name would name a different file on one side than the other.
                if part.contains(['\\', ':']) || part.ends_with(['.', ' ']) {
                    return Err(format!(
                        "{path:?} has a name the origin does not keep as a path"
                    ));
                }
                parts.push(part);
            }
            Component::CurDir => {}
            _ => {
                return Err(format!(
                    "{path} must be relative and stay inside the worktree"
                ));
            }
        }
    }
    if parts.is_empty() {
        return Err(format!("{path:?} names no file"));
    }
    Ok(parts.join("/"))
}

/// Runs git in `dir`. A failure returns git's own words, from stderr or else stdout.
pub(crate) async fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .await
        .map_err(|err| format!("git did not start: {err}"))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if !stderr.is_empty() {
        return Err(stderr);
    }
    Err(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
