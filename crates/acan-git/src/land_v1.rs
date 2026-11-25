//! Stage only approved task changes and scan the exact commit that can be landed.

use std::collections::BTreeSet;
use std::io;
use std::path::Path;

use acan_core::CommitId;
use acan_intent::{PathCoverage, PathScope, RepoPath, RepoRules};
use serde::{Deserialize, Serialize};

use crate::{Author, Error, TaskWorktree, git};

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
    #[error(transparent)]
    Git(#[from] Error),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// One changed path in the staged commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    pub status: ChangeStatus,
}

/// The change relative to the original task base. Copies are additions; they leave their
/// source unchanged, but both endpoints still have to pass the scope check.
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

/// A commit Acan made from an approved task worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedCommit {
    pub commit: CommitId,
    pub base: CommitId,
    pub changed: Vec<ChangedFile>,
    pub ignored: IgnoredAccounting,
}

/// Stages and commits the worktree's changes, with hooks and signing disabled. The index is
/// scanned after staging and the exact commit is scanned again before updating the task ref.
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
    stage(
        worktree_root,
        None,
        expected_base,
        scope,
        rules,
        author,
        message,
    )
    .await
}

impl TaskWorktree {
    /// Like [`stage_and_commit_worktree`], but pins the ref update to the registered task
    /// branch. A checkout that switched branches cannot commit onto a different branch.
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
    if root.symlink_metadata()?.is_symlink()
        || root.canonicalize()? != Path::new(top.trim_end_matches(['\r', '\n'])).canonicalize()?
    {
        return Err(LandV1Error::EscapeRefused {
            path: ".".to_owned(),
        });
    }
    let head_ref = git::run(root, &["symbolic-ref", "-q", "HEAD"], &[]).await?;
    let reference = match head_ref.status.code() {
        Some(0) => head_ref.stdout.trim(),
        Some(1) => "HEAD",
        _ => {
            return Err(git::ensure_ok(head_ref, &["symbolic-ref", "-q", "HEAD"])
                .expect_err("symbolic-ref did not succeed")
                .into());
        }
    };
    if task_ref.is_some_and(|task_ref| task_ref != reference) {
        return Err(bad_output(
            "symbolic-ref HEAD",
            "the checkout left the task branch",
        ));
    }

    let status = scan.status().await?;
    if !status.changed {
        return Err(LandV1Error::NothingToCommit);
    }
    if let Some(refusal) = status.refusals.into_error() {
        // Porcelain cannot pair an unstaged delete with an untracked destination. Diagnose
        // that case in a scratch index, never staging the unapproved changes in the real one.
        let refusal = if status.unpaired && matches!(refusal, LandV1Error::OutOfScope { .. }) {
            scan.diagnose_rename(base, refusal).await
        } else {
            Ok(refusal)
        };
        scan.unstage().await?;
        return Err(refusal?);
    }

    let commit = match scan.commit(base, author, message).await {
        Ok(commit) => commit,
        Err(error) => {
            scan.unstage().await?;
            return Err(error);
        }
    };
    // Check HEAD again and update the pinned ref with a compare-and-swap. Never follow a ref
    // that an agent may have switched during the scan.
    scan.check_head(base).await?;
    git::ok(
        root,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "update-ref",
            "--no-deref",
            reference,
            commit.0.as_str(),
            base.as_str(),
        ],
        &[],
    )
    .await?;
    scan.check_head(&commit.0).await?;
    Ok(StagedCommit {
        commit: commit.0,
        base: base.clone(),
        changed: commit.1,
        ignored: status.ignored,
    })
}

const STATUS: &[&str] = &[
    "-c",
    "core.quotepath=false",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.hooksPath=/dev/null",
    "status",
    "--porcelain=v2",
    "-z",
    "--untracked-files=all",
    "--ignored=matching",
    "--ignore-submodules=none",
    "--find-renames",
];
const ADD: &[&str] = &[
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
    "add",
    "-A",
];

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

    // Reset only the index, not a ref or any file in the checkout.
    async fn unstage(&self) -> Result<(), LandV1Error> {
        let args = [
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "reset",
            "--quiet",
            "HEAD",
            "--",
            ":/",
        ];
        git::ok(self.root, &args, &[]).await?;
        Ok(())
    }

    async fn status(&self) -> Result<WorktreeStatus, LandV1Error> {
        let raw = git::ok_raw(self.root, STATUS, &[], None).await?;
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
                    let on_disk = self.check_disk(&path).await?;
                    if on_disk.is_some_and(|m| m.is_dir())
                        && metadata(&self.root.join(path.as_str()).join(".git"))
                            .await?
                            .is_some()
                    {
                        return Err(LandV1Error::SubmoduleRefused {
                            path: path.to_string(),
                        });
                    }
                    status.changed = true;
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
                    status.changed = true;
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
        let Some(meta) = self.check_disk(&start).await? else {
            return Ok(());
        };
        // `--ignored=matching` collapses a directly ignored directory. Enumerate its
        // contents, not just its name, without walking into any symlink.
        let mut pending = vec![(start, meta.file_type(), false)];
        while let Some((path, kind, protected_parent)) = pending.pop() {
            if kind.is_symlink() {
                return Err(LandV1Error::SymlinkRefused {
                    path: path.to_string(),
                });
            }
            let protected = protected_parent || self.is_protected(&path);
            // Git's own root metadata is not reported by status. A nested .git entry
            // discovered on disk is not ours; refuse it without opening its contents.
            if git_metadata(&path) {
                refused.insert(path.to_string());
            } else if kind.is_dir() {
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

    async fn diagnose_rename(
        &self,
        base: &CommitId,
        refusal: LandV1Error,
    ) -> Result<LandV1Error, LandV1Error> {
        let scratch = tempfile::tempdir()?;
        let index = scratch.path().join("index");
        let env = [(
            "GIT_INDEX_FILE".to_owned(),
            crate::repo::path_str(&index)?.to_owned(),
        )];
        let args = ["-c", "core.hooksPath=/dev/null", "read-tree", base.as_str()];
        git::ok(self.root, &args, &env).await?;
        git::ok(self.root, ADD, &env).await?;
        let (_changed, refused) = self.diff(&["--cached", base.as_str()], &env).await?;
        // Keep the porcelain refusal if the scratch index did not explain it as a pair.
        Ok(match refused.into_error() {
            Some(error @ LandV1Error::RenameOutsideScope { .. }) => error,
            _ => refusal,
        })
    }

    async fn commit(
        &self,
        base: &CommitId,
        author: &Author,
        message: &str,
    ) -> Result<(CommitId, Vec<ChangedFile>), LandV1Error> {
        git::ok(self.root, ADD, &[]).await?;
        let (changed, refused) = self.diff(&["--cached", base.as_str()], &[]).await?;
        if let Some(error) = refused.into_error() {
            return Err(error);
        }
        if changed.is_empty() {
            return Err(LandV1Error::NothingToCommit);
        }
        let args = [
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "write-tree",
        ];
        let tree = git::ok(self.root, &args, &[]).await?;
        let env = [
            ("GIT_AUTHOR_NAME".to_owned(), author.name.clone()),
            ("GIT_AUTHOR_EMAIL".to_owned(), author.email.clone()),
            ("GIT_COMMITTER_NAME".to_owned(), author.name.clone()),
            ("GIT_COMMITTER_EMAIL".to_owned(), author.email.clone()),
        ];
        // Commit the written tree with exactly one parent. A pending merge or a repo hook
        // cannot introduce unapproved history or edit the index during the commit.
        let args = [
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "commit-tree",
            tree.trim(),
            "-p",
            base.as_str(),
            "-m",
            message,
        ];
        let out = git::ok(self.root, &args, &env).await?;
        let commit: CommitId = out.trim().parse().map_err(Error::from)?;
        let (changed, refused) = self.diff(&[base.as_str(), commit.as_str()], &[]).await?;
        if let Some(error) = refused.into_error() {
            return Err(error);
        }
        if changed.is_empty() {
            return Err(LandV1Error::NothingToCommit);
        }
        Ok((commit, changed))
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
            self.check_disk(&path).await?;
            if let Some(from) = &from {
                self.check_disk(from).await?;
            }
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
    changed: bool,
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
