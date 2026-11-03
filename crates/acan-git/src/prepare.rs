//! The gate side of git: prepare the merged commit a candidate is tested as, materialize it
//! for a run, and install exactly that commit.
//!
//! Prepare merges the candidate into the protected head with `git merge-tree`, so neither
//! the protected branch nor any checkout moves, then replaces every protected path of the
//! merged tree with the protected head's own content. A candidate that deletes, renames or
//! forges a protected input therefore changes nothing the gate reads; the paths it touched
//! are reported as discarded. Install moves the protected branch to the prepared commit
//! only while the branch still sits at the head the commit was prepared on.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use acan_core::CommitId;
use acan_intent::{
    ArtifactHash, CommitSha, ContentHash, GateContext, OverlayHash, OverlayManifest, RepoPath,
};

use crate::repo::{Advance, path_str};
use crate::{Error, ProtectedPaths, Repo, checkout, git};

/// The git mode of a regular file.
const REGULAR: &str = "100644";
/// The git mode of an executable file.
const EXECUTABLE: &str = "100755";

/// One regular file of a tree: where it goes and the blob that holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BlobFile {
    pub(crate) path: RepoPath,
    oid: String,
    pub(crate) executable: bool,
}

/// A candidate prepared for the gate: the exact merged commit to test and to install.
///
/// Only [`Repo::prepare`] makes one, from git. It carries no candidate claim: the protected
/// head, the merged commit and the overlay of protected inputs are all read from the
/// repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preparation {
    protected_head: CommitSha,
    candidate: CommitSha,
    merged_commit: CommitSha,
    protected: ProtectedPaths,
    overlay_files: Vec<BlobFile>,
    overlay: OverlayManifest,
    overlay_hash: OverlayHash,
    discarded: Vec<RepoPath>,
}

impl Preparation {
    /// The protected head the candidate was merged into. Install refuses once the protected
    /// branch has moved off it.
    #[must_use]
    pub fn protected_head(&self) -> &CommitSha {
        &self.protected_head
    }

    /// The candidate commit as given.
    #[must_use]
    pub fn candidate(&self) -> &CommitSha {
        &self.candidate
    }

    /// The prepared merged commit: the one every run tests and the only one install moves
    /// the protected branch to. It is the candidate itself when the candidate already
    /// contains the protected head and touches no protected path.
    #[must_use]
    pub fn merged_commit(&self) -> &CommitSha {
        &self.merged_commit
    }

    /// The paths that were protected.
    #[must_use]
    pub fn protected(&self) -> &ProtectedPaths {
        &self.protected
    }

    /// Every protected file of the protected head, with its content hash.
    #[must_use]
    pub fn overlay(&self) -> &OverlayManifest {
        &self.overlay
    }

    /// The identity of [`Self::overlay`].
    #[must_use]
    pub fn overlay_hash(&self) -> OverlayHash {
        self.overlay_hash
    }

    /// Paths where the merge of the candidate differed from the prepared tree: protected
    /// files it edited, deleted, renamed or added, and files it put where a protected
    /// path's directory must be. Empty for a candidate that left protected inputs alone.
    #[must_use]
    pub fn discarded(&self) -> &[RepoPath] {
        &self.discarded
    }

    /// `context` with the fields git owns (protected head, merged commit, overlay hash)
    /// replaced by this preparation's, whatever the caller or the candidate put there.
    #[must_use]
    pub fn bind(&self, mut context: GateContext) -> GateContext {
        context.protected_head = self.protected_head.clone();
        context.merged_commit = self.merged_commit.clone();
        context.overlay_hash = self.overlay_hash;
        context
    }

    fn names(&self, context: &GateContext) -> bool {
        context.protected_head == self.protected_head
            && context.merged_commit == self.merged_commit
            && context.overlay_hash == self.overlay_hash
    }
}

/// The protected paths of one commit, read from that commit's own tree.
///
/// Unlike a [`Preparation`] it merges nothing and tests nothing. It is the overlay a gate pass over
/// its commit restores, so a receipt can be checked against the inputs it ran on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtectedExport {
    commit: CommitSha,
    protected: ProtectedPaths,
    files: Vec<BlobFile>,
    overlay: OverlayManifest,
    overlay_hash: OverlayHash,
}

impl ProtectedExport {
    /// The commit the protected paths were read from.
    #[must_use]
    pub fn commit(&self) -> &CommitSha {
        &self.commit
    }

    /// Every protected file of the commit, with its content hash.
    #[must_use]
    pub fn overlay(&self) -> &OverlayManifest {
        &self.overlay
    }

    /// The identity of [`Self::overlay`], which a receipt records as its overlay hash.
    #[must_use]
    pub fn overlay_hash(&self) -> OverlayHash {
        self.overlay_hash
    }
}

/// What [`Repo::prepare`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use]
#[non_exhaustive]
pub enum PrepareOutcome {
    /// The candidate merges cleanly into the protected head.
    Prepared(Preparation),
    /// The candidate conflicts with the protected head. Nothing was written.
    Conflict {
        /// The protected head the merge was attempted on.
        protected_head: CommitSha,
        /// The conflicting paths.
        files: Vec<RepoPath>,
    },
}

/// What [`Repo::install`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use]
#[non_exhaustive]
pub enum InstallOutcome {
    /// The protected branch points at the prepared commit, now or already.
    Installed {
        /// The prepared merged commit.
        commit: CommitSha,
    },
    /// The protected branch is no longer at the head the commit was prepared on. Nothing
    /// moved; prepare and run again.
    HeadMoved {
        /// The protected head now.
        current: CommitSha,
    },
    /// The tested context names another protected head, merged commit or overlay than this
    /// preparation. Nothing moved.
    ContextMismatch,
    /// The checkout of the protected branch has uncommitted changes to tracked files.
    /// Nothing moved.
    CheckoutDirty {
        /// The checkout that has the protected branch.
        path: PathBuf,
    },
    /// The checkout holds an untracked or ignored file the install would overwrite.
    /// Nothing moved.
    CheckoutBlocked {
        /// The checkout that has the protected branch.
        path: PathBuf,
    },
    /// The checkout switched away from the protected branch. Nothing moved.
    CheckoutMoved {
        /// The checkout that switched.
        path: PathBuf,
    },
}

/// One record of `git ls-tree -r -z`, borrowed from its output.
struct TreeEntry<'a> {
    mode: &'a str,
    oid: &'a str,
    path: &'a str,
    record: &'a str,
}

impl Repo {
    /// Prepare `candidate` for the gate: merge it into the current protected head and
    /// restore every protected path from that head, without moving any branch or checkout.
    ///
    /// The result is a commit that has not been tested yet. Test it through
    /// [`Self::materialize`] and land it through [`Self::install`]. A candidate that merges
    /// cleanly on its own base but conflicts with work landed since is a
    /// [`PrepareOutcome::Conflict`]; one that merges but breaks with that work fails on the
    /// prepared commit, because that is what every run tests.
    ///
    /// # Errors
    /// Fails when `candidate` is not a commit of this repository, when a protected path of
    /// the protected head is a symlink or a submodule, when a path git reports is not a
    /// portable [`RepoPath`], or when git fails.
    pub async fn prepare(
        &self,
        candidate: &CommitSha,
        protected: &ProtectedPaths,
    ) -> Result<PrepareOutcome, Error> {
        let head: CommitSha = self.protected_head().await?.as_str().parse()?;
        let commit = format!("{candidate}^{{commit}}");
        git::ok(
            self.root(),
            &["rev-parse", "--verify", "--quiet", &commit],
            &[],
        )
        .await?;

        let args = [
            "merge-tree",
            "--write-tree",
            "--name-only",
            "-z",
            head.as_str(),
            candidate.as_str(),
        ];
        let out = git::run_raw(self.root(), &args, &[], None).await?;
        let clean = match out.status.code() {
            Some(0) => true,
            Some(1) => false,
            _ => return Err(out.into_error(&args)),
        };
        let stdout = utf8(out.stdout, "merge-tree --write-tree")?;
        let mut tokens = stdout.split('\0');
        let merged_tree = tokens
            .next()
            .filter(|token| !token.is_empty())
            .ok_or_else(|| Error::BadOutput {
                args: "merge-tree --write-tree".to_owned(),
                detail: "missing tree id".to_owned(),
            })?;
        if !clean {
            // After the tree id, `-z --name-only` prints one conflicting file per
            // NUL-terminated token up to the empty token that ends the section.
            let files = tokens
                .take_while(|token| !token.is_empty())
                .map(str::parse)
                .collect::<Result<Vec<RepoPath>, _>>()?;
            return Ok(PrepareOutcome::Conflict {
                protected_head: head,
                files,
            });
        }

        let mut records = String::new();
        let head_listing = self.list_tree(head.as_str()).await?;
        let overlay_files = protected_files(&head_listing, protected)?;
        for entry in tree_entries(&head_listing)? {
            if protected.covers(entry.path) {
                push_record(&mut records, entry.record);
            }
        }
        let merged_listing = self.list_tree(merged_tree).await?;
        for entry in tree_entries(&merged_listing)? {
            if !protected.covers(entry.path) && !protected.encloses(entry.path) {
                push_record(&mut records, entry.record);
            }
        }
        let tree = self.write_tree(&records).await?;
        let discarded = self.changed_paths(merged_tree, &tree).await?;
        let merged_commit = self.merged_commit(&head, candidate, &tree).await?;

        let blobs = self
            .read_blobs(overlay_files.iter().map(|file| file.oid.as_str()))
            .await?;
        let (overlay, overlay_hash) = overlay_of(&overlay_files, &blobs)?;
        Ok(PrepareOutcome::Prepared(Preparation {
            protected_head: head,
            candidate: candidate.clone(),
            merged_commit,
            protected: protected.clone(),
            overlay_files,
            overlay,
            overlay_hash,
            discarded,
        }))
    }

    /// Install the prepared commit: move the protected branch to it by compare-and-swap
    /// against the protected head it was prepared on.
    ///
    /// `tested` is the context the verdict was reached under. It must name this
    /// preparation's protected head, merged commit and overlay, or nothing moves. A
    /// protected branch that moved since the preparation, even to a commit that contains
    /// it, is [`InstallOutcome::HeadMoved`]: what sits at the head was not tested. A
    /// checkout of the protected branch moves with it, as in [`Self::land`].
    ///
    /// Idempotent: when the protected branch already points at the prepared commit, it is
    /// reported installed again.
    ///
    /// # Errors
    /// Fails when git itself fails.
    pub async fn install(
        &self,
        preparation: &Preparation,
        tested: &GateContext,
    ) -> Result<InstallOutcome, Error> {
        if !preparation.names(tested) {
            return Ok(InstallOutcome::ContextMismatch);
        }
        let merged: CommitId = preparation.merged_commit.as_str().parse()?;
        let expected: CommitId = preparation.protected_head.as_str().parse()?;
        let current = self.protected_head().await?;
        if current == merged {
            return Ok(InstallOutcome::Installed {
                commit: preparation.merged_commit.clone(),
            });
        }
        if current != expected {
            return Ok(InstallOutcome::HeadMoved {
                current: current.as_str().parse()?,
            });
        }
        Ok(match self.advance(&merged, &expected).await? {
            Advance::Landed { commit } => InstallOutcome::Installed {
                commit: commit.as_str().parse()?,
            },
            Advance::HeadMoved { current } => InstallOutcome::HeadMoved {
                current: current.as_str().parse()?,
            },
            Advance::CheckoutDirty { path } => InstallOutcome::CheckoutDirty { path },
            Advance::CheckoutBlocked { path } => InstallOutcome::CheckoutBlocked { path },
            Advance::CheckoutMoved { path } => InstallOutcome::CheckoutMoved { path },
        })
    }

    /// Write the prepared commit's tree into `dir`, which must be missing or empty, with
    /// every protected path restored from the protected head. Returns the overlay hash of
    /// what the protected paths hold on disk, which equals the preparation's.
    ///
    /// The directory holds files only, with no `.git`, written byte for byte from the
    /// blobs with no line-ending or attribute conversion.
    ///
    /// # Errors
    /// Fails when `dir` is not empty, when the tree holds a symlink or a submodule, when
    /// two of its paths differ only in case, when a path is not portable, or on I/O.
    pub async fn materialize(
        &self,
        preparation: &Preparation,
        dir: &Path,
    ) -> Result<OverlayHash, Error> {
        let listing = self.list_tree(preparation.merged_commit.as_str()).await?;
        let entries = tree_entries(&listing)?;
        checkout::refuse_case_collisions(entries.iter().map(|entry| entry.path))?;
        let mut files = Vec::new();
        for entry in &entries {
            if preparation.protected.covers(entry.path) {
                continue;
            }
            let executable = match entry.mode {
                REGULAR => false,
                EXECUTABLE => true,
                mode => {
                    return Err(Error::UnsupportedEntry {
                        path: entry.path.to_owned(),
                        mode: mode.to_owned(),
                    });
                }
            };
            files.push(BlobFile {
                path: entry.path.parse()?,
                oid: entry.oid.to_owned(),
                executable,
            });
        }
        let blobs = self
            .read_blobs(files.iter().map(|file| file.oid.as_str()))
            .await?;
        let files: Vec<(BlobFile, Vec<u8>)> = files.into_iter().zip(blobs).collect();
        let root = dir.to_path_buf();
        blocking(move || checkout::write_files(&root, &files)).await?;
        self.restore_protected(preparation, dir).await
    }

    /// Restore every protected path of `dir` from the protected head: remove whatever is
    /// there, case variants and links included, and write the protected head's files.
    /// Call it before every run. Returns the overlay hash read back from disk.
    ///
    /// Files outside the protected paths are left as they are.
    ///
    /// # Errors
    /// Fails on I/O, or with [`Error::OverlayMismatch`] when the protected paths on disk do
    /// not read back as the preparation's overlay.
    pub async fn restore_protected(
        &self,
        preparation: &Preparation,
        dir: &Path,
    ) -> Result<OverlayHash, Error> {
        self.restore_overlay(
            &preparation.protected,
            &preparation.overlay_files,
            &preparation.overlay,
            dir,
        )
        .await
    }

    /// The protected paths of `commit`, read from that commit's own tree. Nothing is merged,
    /// and no branch or checkout moves. A receipt names the commit it ran on, so this reads
    /// back the protected inputs of that commit after the protected head has moved on.
    ///
    /// # Errors
    /// Fails when `commit` is not a commit of this repository, when a protected path of it is
    /// not a regular file, or when git fails.
    pub async fn export_protected(
        &self,
        commit: &CommitSha,
        protected: &ProtectedPaths,
    ) -> Result<ProtectedExport, Error> {
        let spec = format!("{commit}^{{commit}}");
        git::ok(
            self.root(),
            &["rev-parse", "--verify", "--quiet", &spec],
            &[],
        )
        .await?;
        let listing = self.list_tree(commit.as_str()).await?;
        let files = protected_files(&listing, protected)?;
        let blobs = self
            .read_blobs(files.iter().map(|file| file.oid.as_str()))
            .await?;
        let (overlay, overlay_hash) = overlay_of(&files, &blobs)?;
        Ok(ProtectedExport {
            commit: commit.clone(),
            protected: protected.clone(),
            files,
            overlay,
            overlay_hash,
        })
    }

    /// Write the protected paths of `export` into `dir`, which is created when missing, and
    /// read them back. Returns the overlay hash read back from disk, which equals the
    /// export's. Files outside the protected paths are left as they are.
    ///
    /// # Errors
    /// Fails on I/O, or with [`Error::OverlayMismatch`] when the protected paths on disk do
    /// not read back as the export's overlay.
    pub async fn write_protected(
        &self,
        export: &ProtectedExport,
        dir: &Path,
    ) -> Result<OverlayHash, Error> {
        self.restore_overlay(&export.protected, &export.files, &export.overlay, dir)
            .await
    }

    /// Restore `files`, the protected files a manifest names, into `dir`, and check that the
    /// protected paths on disk read back as `expected`.
    async fn restore_overlay(
        &self,
        protected: &ProtectedPaths,
        files: &[BlobFile],
        expected: &OverlayManifest,
        dir: &Path,
    ) -> Result<OverlayHash, Error> {
        let blobs = self
            .read_blobs(files.iter().map(|file| file.oid.as_str()))
            .await?;
        let files: Vec<(BlobFile, Vec<u8>)> = files.iter().cloned().zip(blobs).collect();
        let protected = protected.clone();
        let root = dir.to_path_buf();
        let on_disk = blocking(move || checkout::restore(&root, &protected, &files)).await?;
        if on_disk != *expected {
            return Err(Error::OverlayMismatch(dir.to_path_buf()));
        }
        Ok(on_disk.overlay_hash()?)
    }

    /// The commit to test: the candidate itself when the overlaid tree is its tree and it
    /// contains the head, the head itself when the candidate adds nothing, and otherwise a
    /// new merge commit of the two. The merge commit depends only on its inputs: the tree,
    /// the two parents, the candidate's identity and dates, and the message.
    async fn merged_commit(
        &self,
        head: &CommitSha,
        candidate: &CommitSha,
        tree: &str,
    ) -> Result<CommitSha, Error> {
        let head_id: CommitId = head.as_str().parse()?;
        let candidate_id: CommitId = candidate.as_str().parse()?;
        if self.tree_of(candidate).await? == tree
            && self.is_ancestor(&head_id, &candidate_id).await?
        {
            return Ok(candidate.clone());
        }
        if self.tree_of(head).await? == tree && self.is_ancestor(&candidate_id, &head_id).await? {
            return Ok(head.clone());
        }
        let message = format!("acan: prepare {candidate} on {head}");
        let envs = self.candidate_identity(candidate).await?;
        let args = [
            "-c",
            "commit.gpgsign=false",
            "commit-tree",
            tree,
            "-p",
            head.as_str(),
            "-p",
            candidate.as_str(),
            "-m",
            &message,
        ];
        Ok(git::ok(self.root(), &args, &envs).await?.trim().parse()?)
    }

    /// The author and committer of a merge commit: the candidate's own names, emails and dates,
    /// as git records them. The clock and the server's configuration play no part, so a
    /// candidate prepared again after a restart gets the commit id the origin recorded.
    async fn candidate_identity(
        &self,
        candidate: &CommitSha,
    ) -> Result<[(String, String); 6], Error> {
        const FORMAT: &str = "--format=%an%x1f%ae%x1f%ad%x1f%cn%x1f%ce%x1f%cd";
        let args = ["log", "-1", "--date=raw", FORMAT, candidate.as_str()];
        let out = git::ok(self.root(), &args, &[]).await?;
        let found: Vec<&str> = out.trim_end().split('\u{1f}').collect();
        let [
            author_name,
            author_email,
            author_date,
            committer_name,
            committer_email,
            committer_date,
        ] = <[&str; 6]>::try_from(found).map_err(|found| Error::BadOutput {
            args: "log -1".to_owned(),
            detail: format!("expected six identity fields, got {}", found.len()),
        })?;
        Ok([
            ("GIT_AUTHOR_NAME".to_owned(), author_name.to_owned()),
            ("GIT_AUTHOR_EMAIL".to_owned(), author_email.to_owned()),
            ("GIT_AUTHOR_DATE".to_owned(), author_date.to_owned()),
            ("GIT_COMMITTER_NAME".to_owned(), committer_name.to_owned()),
            ("GIT_COMMITTER_EMAIL".to_owned(), committer_email.to_owned()),
            ("GIT_COMMITTER_DATE".to_owned(), committer_date.to_owned()),
        ])
    }

    async fn tree_of(&self, commit: &CommitSha) -> Result<String, Error> {
        let spec = format!("{commit}^{{tree}}");
        let out = git::ok(self.root(), &["rev-parse", "--verify", &spec], &[]).await?;
        Ok(out.trim().to_owned())
    }

    async fn list_tree(&self, tree_ish: &str) -> Result<String, Error> {
        git::ok_utf8(self.root(), &["ls-tree", "-r", "-z", tree_ish], &[]).await
    }

    /// Write a tree from `ls-tree` records through a scratch index, leaving every checkout's
    /// own index alone.
    async fn write_tree(&self, records: &str) -> Result<String, Error> {
        let index = ScratchIndex::new(self).await?;
        let envs = [("GIT_INDEX_FILE".to_owned(), path_str(&index.0)?.to_owned())];
        git::ok_raw(
            self.root(),
            &["update-index", "-z", "--index-info"],
            &envs,
            Some(records.as_bytes()),
        )
        .await?;
        let tree = git::ok(self.root(), &["write-tree"], &envs).await?;
        Ok(tree.trim().to_owned())
    }

    /// The paths whose entries differ between two trees, renames shown as a deletion and an
    /// addition.
    async fn changed_paths(&self, from: &str, to: &str) -> Result<Vec<RepoPath>, Error> {
        let args = [
            "diff-tree",
            "-r",
            "-z",
            "--no-renames",
            "--name-only",
            from,
            to,
        ];
        let out = git::ok_utf8(self.root(), &args, &[]).await?;
        Ok(out
            .split('\0')
            .filter(|token| !token.is_empty())
            .map(str::parse)
            .collect::<Result<_, _>>()?)
    }

    /// The contents of `oids`, in order, through one `git cat-file --batch`.
    async fn read_blobs(&self, oids: impl Iterator<Item = &str>) -> Result<Vec<Vec<u8>>, Error> {
        let oids: Vec<&str> = oids.collect();
        if oids.is_empty() {
            return Ok(Vec::new());
        }
        let input: String = oids.iter().map(|oid| format!("{oid}\n")).collect();
        let out = git::ok_raw(
            self.root(),
            &["cat-file", "--batch"],
            &[],
            Some(input.as_bytes()),
        )
        .await?;
        parse_batch(&out, &oids)
    }
}

fn push_record(records: &mut String, record: &str) {
    records.push_str(record);
    records.push('\0');
}

/// The protected files of a tree listing, in listing order. A protected path that is not a
/// regular file is refused, because the gate restores files only.
fn protected_files(listing: &str, protected: &ProtectedPaths) -> Result<Vec<BlobFile>, Error> {
    tree_entries(listing)?
        .into_iter()
        .filter(|entry| protected.covers(entry.path))
        .map(|entry| {
            let executable = match entry.mode {
                REGULAR => false,
                EXECUTABLE => true,
                mode => {
                    return Err(Error::ProtectedNotAFile {
                        path: entry.path.to_owned(),
                        mode: mode.to_owned(),
                    });
                }
            };
            Ok(BlobFile {
                path: entry.path.parse()?,
                oid: entry.oid.to_owned(),
                executable,
            })
        })
        .collect()
}

/// The manifest of `files`, whose contents are `blobs` in the same order, and its identity.
fn overlay_of(
    files: &[BlobFile],
    blobs: &[Vec<u8>],
) -> Result<(OverlayManifest, OverlayHash), Error> {
    let overlay = OverlayManifest {
        files: files
            .iter()
            .zip(blobs)
            .map(|(file, bytes)| {
                let hash: ArtifactHash = ContentHash::of_bytes(bytes).into();
                (file.path.clone(), hash)
            })
            .collect(),
    };
    let overlay_hash = overlay.overlay_hash()?;
    Ok((overlay, overlay_hash))
}

fn tree_entries(listing: &str) -> Result<Vec<TreeEntry<'_>>, Error> {
    listing
        .split('\0')
        .filter(|record| !record.is_empty())
        .map(|record| {
            let bad = || Error::BadOutput {
                args: "ls-tree -r -z".to_owned(),
                detail: format!("unexpected record {record:?}"),
            };
            let (meta, path) = record.split_once('\t').ok_or_else(bad)?;
            let mut fields = meta.split(' ');
            let (Some(mode), Some(_kind), Some(oid), None) =
                (fields.next(), fields.next(), fields.next(), fields.next())
            else {
                return Err(bad());
            };
            Ok(TreeEntry {
                mode,
                oid,
                path,
                record,
            })
        })
        .collect()
}

/// Split `git cat-file --batch` output into one body per requested object.
fn parse_batch(mut out: &[u8], oids: &[&str]) -> Result<Vec<Vec<u8>>, Error> {
    let bad = |detail: String| Error::BadOutput {
        args: "cat-file --batch".to_owned(),
        detail,
    };
    let mut blobs = Vec::with_capacity(oids.len());
    for oid in oids {
        let newline = out
            .iter()
            .position(|&byte| byte == b'\n')
            .ok_or_else(|| bad(format!("no header for {oid}")))?;
        let header = std::str::from_utf8(&out[..newline])
            .map_err(|_| bad(format!("header for {oid} is not UTF-8")))?;
        let mut fields = header.split(' ');
        let (Some(found), Some("blob"), Some(size), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err(bad(format!("unexpected header {header:?} for {oid}")));
        };
        let size: usize = size
            .parse()
            .map_err(|_| bad(format!("unexpected size in {header:?}")))?;
        if found != *oid {
            return Err(bad(format!("asked for {oid}, got {found}")));
        }
        let start = newline + 1;
        let end = start
            .checked_add(size)
            .ok_or_else(|| bad(format!("unexpected size in {header:?}")))?;
        let body = out
            .get(start..end)
            .ok_or_else(|| bad(format!("{oid} is truncated")))?;
        blobs.push(body.to_vec());
        out = out
            .get(end + 1..)
            .ok_or_else(|| bad(format!("{oid} is truncated")))?;
    }
    Ok(blobs)
}

fn utf8(bytes: Vec<u8>, args: &str) -> Result<String, Error> {
    String::from_utf8(bytes).map_err(|_| Error::BadOutput {
        args: args.to_owned(),
        detail: "output is not UTF-8".to_owned(),
    })
}

/// Run filesystem work off the async threads. A panic in it resumes here.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, Error> + Send + 'static,
) -> Result<T, Error> {
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => Err(Error::Io(std::io::Error::other(e))),
    }
}

/// Distinguishes scratch indexes of concurrent preparations in one process.
static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

/// A scratch index file in the common git directory, removed on drop.
struct ScratchIndex(PathBuf);

impl ScratchIndex {
    async fn new(repo: &Repo) -> Result<Self, Error> {
        let seq = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
        let name = format!("acan-prepare-{}-{seq}.index", std::process::id());
        let path = repo.common_dir().await?.join(name);
        // A file a crashed process left under the same name would seed the index.
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(Self(path))
    }
}

impl Drop for ScratchIndex {
    fn drop(&mut self) {
        // Best effort: a leftover is removed by the next preparation that draws its name.
        let _ = std::fs::remove_file(&self.0);
    }
}

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Preparation>();
    assert_send_sync::<ProtectedExport>();
};
