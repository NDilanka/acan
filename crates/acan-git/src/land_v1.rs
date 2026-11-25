//! Stage only approved task changes and scan the exact commit that can be landed.

use std::collections::BTreeSet;
use std::io;
use std::path::Path;

use acan_core::CommitId;
use acan_intent::{PathCoverage, PathScope, RepoPath, RepoRules, detect_case_collisions};
use serde::{Deserialize, Serialize};

use crate::{Author, Error, LandOutcome, Repo, TaskWorktree, checkout, git};

/// Why a task's worktree could not be committed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LandV1Error {
    #[error("the task committed directly: expected {expected_base}, found {actual_head}")]
    AgentCommittedDirectly {
        expected_base: CommitId,
        actual_head: CommitId,
    },
    #[error("cannot land a symlink at {path:?}")]
    SymlinkRefused { path: String },
    #[error("cannot land a submodule at {path:?}")]
    SubmoduleRefused { path: String },
    #[error("path is not portable or is outside the worktree: {path:?}")]
    EscapeRefused { path: String },
    #[error("rename or copy from {from:?} to {to:?} crosses the approved scope")]
    RenameOutsideScope { from: String, to: String },
    #[error("paths outside the approved scope: {paths:?}")]
    OutOfScope { paths: Vec<String> },
    #[error("cannot land changes to protected paths: {paths:?}")]
    ProtectedPath { paths: Vec<String> },
    #[error("ignored paths violate repo protection: {paths:?}")]
    ProtectedIgnoredFiles { paths: Vec<String> },
    #[error("the task has no changes to commit")]
    NothingToCommit,
    #[error("the task conflicts with protected head {protected_head}")]
    MergeConflict { protected_head: CommitId },
    #[error(transparent)]
    Git(#[from] Error),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// One changed path relative to the head the commit is applied to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    pub status: ChangeStatus,
}

/// Copies are additions. For Git-detected rename and copy pairs (`-M -C`), both endpoints
/// have to pass the scope check; arbitrary content provenance is not inferred.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeStatus {
    Added,
    Modified,
    Deleted,
    Renamed { from: String },
}

/// The sorted paths of ignored files present at staging, none of which were committed.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IgnoredAccounting {
    pub paths: Vec<String>,
}

/// A detached commit Acan made from an approved task worktree. When prepared for landing,
/// `commit` and `changed` describe the exact candidate against the expected protected head;
/// `base` always records the original task base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedCommit {
    pub commit: CommitId,
    pub base: CommitId,
    pub changed: Vec<ChangedFile>,
    pub ignored: IgnoredAccounting,
}

/// The candidate scanned against the head it can replace. Materialization and installation
/// take the same value so neither can accidentally operate on the unmerged task commit.
#[derive(Debug)]
pub struct V1Candidate {
    staged: StagedCommit,
    expected_head: CommitId,
}

impl V1Candidate {
    /// The exact commit and change accounting to record in the landing audit.
    #[must_use]
    pub fn staged(&self) -> &StagedCommit {
        &self.staged
    }
}

/// Stages the worktree's changes in a temporary index and creates a detached commit, with
/// hooks and signing disabled. Neither the task branch nor its index is modified.
///
/// # Errors
///
/// Refuses a moved base, unsafe or unapproved paths, or an empty change. May also fail on git or
/// filesystem errors. `worktree_root` must name the checkout root, not a directory within it.
pub async fn stage_and_commit_worktree(
    worktree_root: &Path,
    expected_base: &CommitId,
    scope: &PathScope,
    rules: &RepoRules,
    author: &Author,
    message: &str,
) -> Result<StagedCommit, LandV1Error> {
    let staged = stage(
        worktree_root,
        None,
        expected_base,
        scope,
        rules,
        author,
        message,
    )
    .await?;
    validate_tree(worktree_root, &staged.commit).await?;
    Ok(staged)
}

impl TaskWorktree {
    /// Stage a detached task commit before preparing it for its expected protected head.
    ///
    /// # Errors
    ///
    /// Also refuses if this checkout is no longer on its task branch.
    pub async fn stage_and_commit_v1(
        &self,
        expected_base: &CommitId,
        scope: &PathScope,
        rules: &RepoRules,
        author: &Author,
        message: &str,
    ) -> Result<StagedCommit, LandV1Error> {
        let branch = format!("refs/heads/{}", self.branch());
        stage(
            self.path(),
            Some(&branch),
            expected_base,
            scope,
            rules,
            author,
            message,
        )
        .await
    }
}

impl Repo {
    /// Prepare and scan the exact commit that may be installed, without moving either branch.
    ///
    /// # Errors
    ///
    /// Refuses merge conflicts, unsafe or unapproved changes against the protected head, and
    /// nonportable or case-colliding paths anywhere in the final tree.
    pub async fn prepare_v1_candidate(
        &self,
        mut staged: StagedCommit,
        expected_head: &CommitId,
        scope: &PathScope,
        rules: &RepoRules,
        author: &Author,
    ) -> Result<V1Candidate, LandV1Error> {
        let root = self.root();
        if *expected_head != staged.base {
            let hooks = git::EmptyHooks::new()?;
            let args = hooks.args(&[
                "merge-tree",
                "--write-tree",
                "--merge-base",
                staged.base.as_str(),
                expected_head.as_str(),
                staged.commit.as_str(),
            ]);
            let out = git::run(root, &args, &[]).await?;
            if out.status.code() == Some(1) {
                return Err(LandV1Error::MergeConflict {
                    protected_head: expected_head.clone(),
                });
            }
            let tree = git::ensure_ok(out, &args)?;
            staged.commit = commit_tree(
                root,
                &hooks,
                tree.trim(),
                &[expected_head, &staged.commit],
                author,
                "acan: merge task edits for landing",
            )
            .await?;
        }
        let scan = Scan { root, scope, rules };
        let (changed, refused) = scan
            .diff(&[expected_head.as_str(), staged.commit.as_str()], &[])
            .await?;
        if let Some(error) = refused.into_error() {
            return Err(error);
        }
        validate_tree(root, &staged.commit).await?;
        staged.changed = changed;
        Ok(V1Candidate {
            staged,
            expected_head: expected_head.clone(),
        })
    }

    /// Materialize a prepared candidate byte for byte, with no checkout metadata or local files.
    ///
    /// # Errors
    ///
    /// Fails on unsafe paths, non-regular files, or Git or filesystem errors.
    pub async fn materialize_v1_candidate(
        &self,
        candidate: &V1Candidate,
    ) -> Result<tempfile::TempDir, LandV1Error> {
        let dir = tempfile::tempdir()?;
        self.materialize_tree(candidate.staged.commit.as_str(), None, dir.path())
            .await?;
        Ok(dir)
    }

    /// Install the already prepared and audited candidate verbatim, only if the protected
    /// branch still points at the expected head. No new merge is performed.
    ///
    /// # Errors
    ///
    /// Fails when Git cannot update the protected branch or its checkout.
    pub async fn install_v1_candidate(
        &self,
        candidate: &V1Candidate,
    ) -> Result<LandOutcome, Error> {
        self.advance(&candidate.staged.commit, &candidate.expected_head)
            .await
            .map(LandOutcome::from)
    }
}

async fn stage(
    root: &Path,
    task_ref: Option<&str>,
    base: &CommitId,
    scope: &PathScope,
    rules: &RepoRules,
    author: &Author,
    message: &str,
) -> Result<StagedCommit, LandV1Error> {
    let scan = Scan { root, scope, rules };
    scan.check_head(base).await?;
    let top = git::ok(root, &["rev-parse", "--show-toplevel"], &[]).await?;
    let canonical_root = root.canonicalize()?;
    if root.symlink_metadata()?.is_symlink()
        || canonical_root != Path::new(top.trim_end_matches(['\r', '\n'])).canonicalize()?
    {
        return Err(LandV1Error::EscapeRefused {
            path: ".".to_owned(),
        });
    }
    scan.check_branch(task_ref).await?;

    scan.walk_worktree(&canonical_root).await?;
    let hooks = git::EmptyHooks::new()?;
    let status = scan.status(&hooks).await?;
    if let Some(refusal) = status.refusals.into_error() {
        // Porcelain cannot pair an unstaged delete with an untracked destination. Diagnose
        // that case in a scratch index. Keep the porcelain refusal if Git found no pair.
        let refusal = if status.unpaired && matches!(refusal, LandV1Error::OutOfScope { .. }) {
            let index = scan.stage_index(base, &hooks).await?;
            let (_, refused) = scan.diff(&["--cached", base.as_str()], &index.env).await?;
            match refused.into_error() {
                Some(error @ LandV1Error::RenameOutsideScope { .. }) => error,
                _ => refusal,
            }
        } else {
            refusal
        };
        return Err(refusal);
    }

    let index = scan.stage_index(base, &hooks).await?;
    let (changed, refused) = scan.diff(&["--cached", base.as_str()], &index.env).await?;
    if let Some(error) = refused.into_error() {
        return Err(error);
    }
    if changed.is_empty() {
        return Err(LandV1Error::NothingToCommit);
    }
    let tree = git::ok(
        root,
        &hooks.args(&["-c", "core.fsmonitor=false", "write-tree"]),
        &index.env,
    )
    .await?;
    let commit = commit_tree(root, &hooks, tree.trim(), &[base], author, message).await?;
    scan.check_head(base).await?;
    scan.check_branch(task_ref).await?;
    Ok(StagedCommit {
        commit,
        base: base.clone(),
        changed,
        ignored: status.ignored,
    })
}

async fn commit_tree(
    root: &Path,
    hooks: &git::EmptyHooks,
    tree: &str,
    parents: &[&CommitId],
    author: &Author,
    message: &str,
) -> Result<CommitId, LandV1Error> {
    let env = [
        ("GIT_AUTHOR_NAME".to_owned(), author.name.clone()),
        ("GIT_AUTHOR_EMAIL".to_owned(), author.email.clone()),
        ("GIT_COMMITTER_NAME".to_owned(), author.name.clone()),
        ("GIT_COMMITTER_EMAIL".to_owned(), author.email.clone()),
    ];
    let mut args = hooks.args(&["-c", "commit.gpgsign=false", "commit-tree", tree]);
    for parent in parents {
        args.extend(["-p", parent.as_str()]);
    }
    args.extend(["-m", message]);
    let out = git::ok(root, &args, &env).await?;
    Ok(out.trim().parse().map_err(Error::from)?)
}

async fn validate_tree(root: &Path, commit: &CommitId) -> Result<(), LandV1Error> {
    let args = ["ls-tree", "-r", "-z", "--name-only", commit.as_str()];
    let raw = git::ok_raw(root, &args, &[], None).await?;
    let paths = nul_fields(&raw, "ls-tree")?
        .map(|path| repo_path(path, false))
        .collect::<Result<Vec<_>, _>>()?;
    let protected: Vec<_> = paths
        .iter()
        .filter(|path| git_metadata(path))
        .map(ToString::to_string)
        .collect();
    if !protected.is_empty() {
        return Err(LandV1Error::ProtectedPath { paths: protected });
    }
    if let Some((_, path)) = detect_case_collisions(&paths).into_iter().next() {
        return Err(Error::CaseCollision(path.to_string()).into());
    }
    // Also check parent segments: `Src/a` and `src/b` have distinct leaf names, but
    // their directories would be merged on a case-insensitive filesystem.
    checkout::refuse_case_collisions(paths.iter().map(RepoPath::as_str))?;
    Ok(())
}

const STATUS: &[&str] = &[
    "-c",
    "core.quotepath=false",
    "-c",
    "core.fsmonitor=false",
    "status",
    "--porcelain=v2",
    "-z",
    "--untracked-files=all",
    "--ignored=matching",
    "--ignore-submodules=none",
    "--find-renames",
];
struct ScratchIndex {
    _dir: tempfile::TempDir,
    env: [(String, String); 1],
}

struct Scan<'a> {
    root: &'a Path,
    scope: &'a PathScope,
    rules: &'a RepoRules,
}

impl Scan<'_> {
    async fn head(&self) -> Result<CommitId, LandV1Error> {
        let out = git::ok(self.root, &["rev-parse", "HEAD"], &[]).await?;
        Ok(out.trim().parse().map_err(Error::from)?)
    }

    async fn check_head(&self, expected: &CommitId) -> Result<(), LandV1Error> {
        let actual_head = self.head().await?;
        if actual_head != *expected {
            return Err(LandV1Error::AgentCommittedDirectly {
                expected_base: expected.clone(),
                actual_head,
            });
        }
        Ok(())
    }

    async fn check_branch(&self, task_ref: Option<&str>) -> Result<(), LandV1Error> {
        if let Some(task_ref) = task_ref {
            let args = ["symbolic-ref", "-q", "HEAD"];
            let out = git::run(self.root, &args, &[]).await?;
            if out.status.code() == Some(1) || git::ensure_ok(out, &args)?.trim() != task_ref {
                return Err(bad_output(
                    "symbolic-ref HEAD",
                    "the checkout left the task branch",
                ));
            }
        }
        Ok(())
    }

    // Git can hide nested metadata entirely, particularly in tracked and ignored
    // directories. Walk the filesystem without following links or relying on status.
    async fn walk_worktree(&self, canonical_root: &Path) -> Result<(), LandV1Error> {
        let mut pending = vec![(self.root.to_path_buf(), String::new())];
        let mut protected = BTreeSet::new();
        let mut links = Vec::new();
        while let Some((dir, prefix)) = pending.pop() {
            let mut entries = tokio::fs::read_dir(dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                let name = entry.file_name();
                if prefix.is_empty() && name == ".git" {
                    continue;
                }
                let path = if prefix.is_empty() {
                    name.to_string_lossy().into_owned()
                } else {
                    format!("{prefix}/{}", name.to_string_lossy())
                };
                if name.eq_ignore_ascii_case(".git") {
                    protected.insert(path);
                    continue;
                }
                let kind = entry.file_type().await?;
                if kind.is_symlink() {
                    links.push((entry.path(), path));
                } else if kind.is_dir() {
                    pending.push((entry.path(), path));
                }
            }
        }
        if !protected.is_empty() {
            return Err(LandV1Error::ProtectedPath {
                paths: protected.into_iter().collect(),
            });
        }
        for (entry, path) in links {
            match tokio::fs::canonicalize(entry).await {
                Ok(target) if target.starts_with(canonical_root) => {}
                Ok(_) => return Err(LandV1Error::EscapeRefused { path }),
                // A dangling or cyclic link cannot be proven to stay inside the worktree.
                Err(_) => return Err(LandV1Error::SymlinkRefused { path }),
            }
            let link = repo_path(path.as_bytes(), false)?;
            let args = [
                "-c",
                "core.fsmonitor=false",
                "check-ignore",
                "-q",
                "--",
                link.as_str(),
            ];
            let out = git::run(self.root, &args, &[]).await?;
            match out.status.code() {
                Some(0) => {}
                Some(1) => return Err(LandV1Error::SymlinkRefused { path }),
                _ => {
                    git::ensure_ok(out, &args)?;
                }
            }
        }
        Ok(())
    }

    async fn status(&self, hooks: &git::EmptyHooks) -> Result<WorktreeStatus, LandV1Error> {
        // Inspect pre-staged entries for protection and modes without refreshing the real index.
        let env = [("GIT_OPTIONAL_LOCKS".to_owned(), "0".to_owned())];
        let raw = git::ok_raw(self.root, &hooks.args(STATUS), &env, None).await?;
        let mut fields = nul_fields(&raw, "status")?;
        let mut status = WorktreeStatus::default();
        let mut ignored_refusals = BTreeSet::new();
        while let Some(record) = fields.next() {
            match record.first() {
                Some(b'!') => {
                    let path = repo_path(short_status_path(record)?, true)?;
                    self.walk_ignored(path, &mut status.ignored.paths, &mut ignored_refusals)
                        .await?;
                }
                Some(b'?') => {
                    let path = repo_path(short_status_path(record)?, true)?;
                    self.check_disk(&path).await?;
                    status.unpaired = true;
                    status.refusals.add(self.classify(&path), &path, None);
                }
                Some(b'1' | b'2' | b'u') => {
                    let (count, mode_end) = match record[0] {
                        b'1' => (9, 6),
                        b'2' => (10, 6),
                        _ => (11, 7),
                    };
                    let columns: Vec<_> = record.splitn(count, |b| *b == b' ').collect();
                    if columns.len() != count {
                        return Err(bad_output("status", "incomplete tracked entry"));
                    }
                    let path = repo_path(columns[count - 1], false)?;
                    self.check_disk(&path).await?;
                    for mode in &columns[3..mode_end] {
                        check_mode(mode, &path)?;
                    }
                    let from = if record[0] == b'2' {
                        let from = repo_path(next_field(&mut fields, "status")?, false)?;
                        self.check_disk(&from).await?;
                        // The HEAD mode describes the source of a rename or copy.
                        check_mode(columns[3], &from)?;
                        Some(from)
                    } else {
                        status.unpaired |= columns[1].contains(&b'A');
                        None
                    };
                    self.check_coverage(&path, from.as_ref(), &mut status.refusals);
                }
                _ => return Err(bad_output("status", "unknown porcelain entry")),
            }
        }
        if !ignored_refusals.is_empty() {
            return Err(LandV1Error::ProtectedIgnoredFiles {
                paths: ignored_refusals.into_iter().collect(),
            });
        }
        status.ignored.paths.sort_unstable();
        status.ignored.paths.dedup();
        Ok(status)
    }

    async fn walk_ignored(
        &self,
        start: RepoPath,
        ignored: &mut Vec<String>,
        refused: &mut BTreeSet<String>,
    ) -> Result<(), LandV1Error> {
        let Some(meta) = metadata(&self.root.join(start.as_str())).await? else {
            return Ok(());
        };
        // `--ignored=matching` collapses a directly ignored directory. Enumerate its
        // contents, not just its name, without walking into any symlink.
        let mut pending = vec![(start, meta.file_type(), false)];
        while let Some((path, kind, protected_parent)) = pending.pop() {
            let protected = protected_parent || self.is_protected(&path);
            if kind.is_dir() {
                let mut dir = tokio::fs::read_dir(self.root.join(path.as_str())).await?;
                let mut any = false;
                while let Some(child) = dir.next_entry().await? {
                    any = true;
                    let name = child.file_name();
                    let raw = format!("{path}/{}", name.to_string_lossy());
                    if name.to_str().is_none() {
                        return Err(LandV1Error::EscapeRefused { path: raw });
                    }
                    pending.push((
                        repo_path(raw.as_bytes(), false)?,
                        child.file_type().await?,
                        protected,
                    ));
                }
                if protected && !any {
                    refused.insert(path.to_string());
                }
            } else if protected {
                refused.insert(path.to_string());
            } else {
                ignored.push(path.to_string());
            }
        }
        Ok(())
    }

    async fn check_disk(&self, path: &RepoPath) -> Result<Option<std::fs::Metadata>, LandV1Error> {
        let mut prefix = self.root.to_path_buf();
        let mut found = None;
        for component in path.as_str().split('/') {
            prefix.push(component);
            found = metadata(&prefix).await?;
            if found.as_ref().is_some_and(std::fs::Metadata::is_symlink) {
                return Err(LandV1Error::SymlinkRefused {
                    path: path.to_string(),
                });
            }
        }
        Ok(found)
    }

    fn is_protected(&self, path: &RepoPath) -> bool {
        git_metadata(path)
            || path.as_str().split('/').next().is_some_and(|first| {
                [".acan", ".factory"]
                    .iter()
                    .any(|name| first.eq_ignore_ascii_case(name))
            })
            || matches!(
                self.scope.coverage(path, self.rules),
                PathCoverage::Protected { .. } | PathCoverage::CheckFile
            )
    }

    fn classify(&self, path: &RepoPath) -> Decision {
        if self.is_protected(path) {
            return Decision::Protected;
        }
        match self.scope.coverage(path, self.rules) {
            PathCoverage::Covered => Decision::Allowed,
            PathCoverage::SharedFile => Decision::Protected,
            _ => Decision::Outside,
        }
    }

    fn check_coverage(&self, path: &RepoPath, from: Option<&RepoPath>, refused: &mut Refusals) {
        if let Some(from) = from {
            refused.add(self.classify(from), from, Some((from, path)));
        }
        refused.add(self.classify(path), path, from.map(|from| (from, path)));
    }

    async fn stage_index(
        &self,
        base: &CommitId,
        hooks: &git::EmptyHooks,
    ) -> Result<ScratchIndex, LandV1Error> {
        let dir = tempfile::tempdir()?;
        let index = dir.path().join("index");
        let env = [(
            "GIT_INDEX_FILE".to_owned(),
            crate::repo::path_str(&index)?.to_owned(),
        )];
        let args = hooks.args(&["-c", "core.fsmonitor=false", "read-tree", base.as_str()]);
        git::ok(self.root, &args, &env).await?;
        let args = hooks.args(&["-c", "core.fsmonitor=false", "add", "-A"]);
        git::ok(self.root, &args, &env).await?;
        Ok(ScratchIndex { _dir: dir, env })
    }

    async fn diff(
        &self,
        revisions: &[&str],
        env: &[(String, String)],
    ) -> Result<(Vec<ChangedFile>, Refusals), LandV1Error> {
        let mut args = vec![
            "-c",
            "core.quotepath=false",
            "-c",
            "core.fsmonitor=false",
            "diff",
            "--raw",
            "-z",
            "-M",
            "-C",
            "--find-copies-harder",
            // Do not let a configured rename limit drop inexact pairs from the scan.
            "-l0",
            "--ignore-submodules=none",
            "--no-ext-diff",
            "--no-textconv",
        ];
        args.extend_from_slice(revisions);
        args.push("--");
        let raw = git::ok_raw(self.root, &args, env, None).await?;
        let mut fields = nul_fields(&raw, "diff --raw")?;
        let mut changed = Vec::new();
        let mut refused = Refusals::default();
        while let Some(header) = fields.next() {
            let columns: Vec<_> = header.split(|b| *b == b' ').collect();
            let [old_mode, new_mode, _, _, kind] = columns.as_slice() else {
                return Err(bad_output("diff --raw", "incomplete change header"));
            };
            let old_mode = old_mode
                .strip_prefix(b":")
                .ok_or_else(|| bad_output("diff --raw", "missing old mode"))?;
            let first = repo_path(next_field(&mut fields, "diff --raw")?, false)?;
            let (path, from) = if matches!(kind.first(), Some(b'R' | b'C')) {
                (
                    repo_path(next_field(&mut fields, "diff --raw")?, false)?,
                    Some(first),
                )
            } else {
                (first, None)
            };
            check_mode(old_mode, from.as_ref().unwrap_or(&path))?;
            check_mode(new_mode, &path)?;
            self.check_coverage(&path, from.as_ref(), &mut refused);
            let status = match kind.first() {
                Some(b'A' | b'C') => ChangeStatus::Added,
                Some(b'M' | b'T') => ChangeStatus::Modified,
                Some(b'D') => ChangeStatus::Deleted,
                Some(b'R') => ChangeStatus::Renamed {
                    from: from.expect("a rename has two paths").to_string(),
                },
                _ => return Err(bad_output("diff --raw", "unexpected change status")),
            };
            changed.push(ChangedFile {
                path: path.to_string(),
                status,
            });
        }
        changed.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        Ok((changed, refused))
    }
}

#[derive(Default)]
struct WorktreeStatus {
    unpaired: bool,
    refusals: Refusals,
    ignored: IgnoredAccounting,
}

enum Decision {
    Allowed,
    Protected,
    Outside,
}

#[derive(Default)]
struct Refusals {
    protected: BTreeSet<String>,
    outside: BTreeSet<String>,
    pair: Option<(String, String)>,
}

impl Refusals {
    fn add(&mut self, decision: Decision, path: &RepoPath, pair: Option<(&RepoPath, &RepoPath)>) {
        match decision {
            Decision::Allowed => {}
            Decision::Protected => {
                self.protected.insert(path.to_string());
            }
            Decision::Outside => {
                if let Some((from, to)) = pair {
                    if self.pair.is_none() {
                        self.pair = Some((from.to_string(), to.to_string()));
                    }
                } else {
                    self.outside.insert(path.to_string());
                }
            }
        }
    }

    fn into_error(self) -> Option<LandV1Error> {
        if !self.protected.is_empty() {
            Some(LandV1Error::ProtectedPath {
                paths: self.protected.into_iter().collect(),
            })
        } else if let Some((from, to)) = self.pair {
            Some(LandV1Error::RenameOutsideScope { from, to })
        } else if !self.outside.is_empty() {
            Some(LandV1Error::OutOfScope {
                paths: self.outside.into_iter().collect(),
            })
        } else {
            None
        }
    }
}

async fn metadata(path: &Path) -> io::Result<Option<std::fs::Metadata>> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(meta) => Ok(Some(meta)),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

fn git_metadata(path: &RepoPath) -> bool {
    path.as_str()
        .split('/')
        .any(|part| part.eq_ignore_ascii_case(".git"))
}

fn repo_path(raw: &[u8], directory_marker: bool) -> Result<RepoPath, LandV1Error> {
    let refuse = || LandV1Error::EscapeRefused {
        path: String::from_utf8_lossy(raw).into_owned(),
    };
    let path = std::str::from_utf8(raw).map_err(|_| refuse())?;
    let path = if directory_marker {
        path.strip_suffix('/').unwrap_or(path)
    } else {
        path
    };
    path.parse().map_err(|_| refuse())
}

fn check_mode(mode: &[u8], path: &RepoPath) -> Result<(), LandV1Error> {
    match mode {
        b"120000" => Err(LandV1Error::SymlinkRefused {
            path: path.to_string(),
        }),
        b"160000" => Err(LandV1Error::SubmoduleRefused {
            path: path.to_string(),
        }),
        _ => Ok(()),
    }
}

fn short_status_path(record: &[u8]) -> Result<&[u8], LandV1Error> {
    if record.get(1) != Some(&b' ') {
        return Err(bad_output("status", "missing path separator"));
    }
    Ok(&record[2..])
}

fn nul_fields<'a>(
    raw: &'a [u8],
    command: &str,
) -> Result<impl Iterator<Item = &'a [u8]>, LandV1Error> {
    if !raw.is_empty() && !raw.ends_with(b"\0") {
        return Err(bad_output(command, "missing NUL terminator"));
    }
    Ok(raw
        .split_inclusive(|b| *b == 0)
        .map(|field| &field[..field.len() - 1]))
}

fn next_field<'a>(
    fields: &mut impl Iterator<Item = &'a [u8]>,
    command: &str,
) -> Result<&'a [u8], LandV1Error> {
    fields
        .next()
        .ok_or_else(|| bad_output(command, "missing path"))
}

fn bad_output(args: &str, detail: &str) -> LandV1Error {
    Error::BadOutput {
        args: args.to_owned(),
        detail: detail.to_owned(),
    }
    .into()
}
