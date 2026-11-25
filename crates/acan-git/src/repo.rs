//! The repository handle: worktree management and landing.

use std::path::{Path, PathBuf};

use acan_core::{CommitId, FilePath, Generation, TaskId};

use crate::git;
use crate::{Error, TaskWorktree};

/// A git repository Acan works in: task worktrees plus the protected branch.
///
/// All git access shells out to the `git` CLI, so worktrees and merges behave
/// exactly like the user's own git. Methods are async and each git call has a
/// timeout.
#[derive(Debug)]
pub struct Repo {
    root: PathBuf,
    protected: String,
    author_name: String,
    author_email: String,
}

impl Repo {
    /// Open the repository containing `path`, protecting the checked-out branch.
    ///
    /// # Errors
    /// Fails when `path` is not in a git repository, or `HEAD` is detached.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let root = repo_root(path.as_ref()).await?;
        // `symbolic-ref` names the branch even before its first commit; it
        // fails only when HEAD is detached.
        let args = ["symbolic-ref", "--short", "-q", "HEAD"];
        let out = git::run(&root, &args, &[]).await?;
        if !out.status.success() {
            return Err(Error::DetachedHead(root));
        }
        Self::finish_open(root, out.stdout.trim().to_owned()).await
    }

    /// Open the repository containing `path`, protecting `protected` instead of
    /// the checked-out branch.
    ///
    /// # Errors
    /// Fails when `path` is not in a git repository.
    pub async fn open_with_protected(
        path: impl AsRef<Path>,
        protected: impl Into<String>,
    ) -> Result<Self, Error> {
        let root = repo_root(path.as_ref()).await?;
        Self::finish_open(root, protected.into()).await
    }

    async fn finish_open(root: PathBuf, protected: String) -> Result<Self, Error> {
        let author_name = config(&root, "user.name")
            .await?
            .unwrap_or_else(|| "Acan".to_owned());
        let author_email = config(&root, "user.email")
            .await?
            .unwrap_or_else(|| "acan@localhost".to_owned());
        Ok(Self {
            root,
            protected,
            author_name,
            author_email,
        })
    }

    /// The repository's top-level directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The branch finished work lands on.
    #[must_use]
    pub fn protected_branch(&self) -> &str {
        &self.protected
    }

    /// The current head of the protected branch.
    ///
    /// # Errors
    /// Fails if the protected branch cannot be read.
    pub async fn protected_head(&self) -> Result<CommitId, Error> {
        let reference = format!("refs/heads/{}", self.protected);
        let out = git::ok(&self.root, &["rev-parse", &reference], &[]).await?;
        Ok(out.trim().parse()?)
    }

    /// The task worktrees of this repository.
    #[must_use]
    pub fn worktrees(&self) -> Worktrees<'_> {
        Worktrees { repo: self }
    }

    /// Merge `gated`, the commit a task's gate tested, into the protected branch.
    ///
    /// `gated` is merged as given, not the task branch's current head, so commits
    /// the agent makes after the gate never land untested. `expected_head` is the
    /// protected head the caller decided the land at. The merge is computed with
    /// `git merge-tree` and committed without a checkout, then installed with
    /// `git update-ref` guarded by the old value, so a concurrent move of the
    /// protected branch is detected and reported as [`LandOutcome::HeadMoved`],
    /// never overwritten.
    ///
    /// A checkout that has the protected branch checked out, the user's main one
    /// included, moves with the branch. A clean checkout gets the new files through
    /// a two-tree read, without running hooks. A checkout with uncommitted changes stops the
    /// land with [`LandOutcome::CheckoutDirty`]. An untracked or ignored file the land
    /// would overwrite stops it with [`LandOutcome::CheckoutBlocked`]. A checkout that
    /// switched branches between the decision and the merge is
    /// [`LandOutcome::CheckoutMoved`]. Nothing moves in any of those cases.
    ///
    /// Idempotent: when the protected branch already contains `gated`, the commit
    /// that landed it is reported again without merging twice.
    ///
    /// # Errors
    /// Fails when git itself fails, including when `gated` is not a commit of this
    /// repository. Conflicts, stale heads, a dirty or blocked checkout, and a
    /// checkout that switched branches are outcomes.
    pub async fn land(
        &self,
        task: TaskId,
        gated: &CommitId,
        expected_head: &CommitId,
    ) -> Result<LandOutcome, Error> {
        let current = self.protected_head().await?;

        // A retried land after a crash finds the task already merged. Report the
        // commit that landed it instead of merging again.
        if self.is_ancestor(gated, &current).await? {
            let commit = self.landing_commit(gated, &current).await?;
            return Ok(LandOutcome::Landed { commit });
        }
        if current != *expected_head {
            return Ok(LandOutcome::HeadMoved { current });
        }
        if self.is_ancestor(&current, gated).await? {
            return self
                .advance(gated, expected_head)
                .await
                .map(LandOutcome::from);
        }

        let out = git::run(
            &self.root,
            &[
                "merge-tree",
                "--write-tree",
                "--name-only",
                "-z",
                current.as_str(),
                gated.as_str(),
            ],
            &[],
        )
        .await?;
        let mut tokens = out.stdout.split('\0');
        let tree = tokens
            .next()
            .filter(|token| !token.is_empty())
            .ok_or_else(|| Error::BadOutput {
                args: "merge-tree --write-tree".to_owned(),
                detail: "missing tree id".to_owned(),
            })?;
        if !out.status.success() {
            // After the tree id, `-z --name-only` prints one conflicting file
            // per NUL-terminated token up to the empty token that ends the
            // section.
            let files = tokens
                .take_while(|token| !token.is_empty())
                .map(|token| Ok(token.parse::<FilePath>()?))
                .collect::<Result<Vec<FilePath>, Error>>()?;
            return Ok(LandOutcome::Conflict { files });
        }

        let message = format!("acan: land task {task}");
        let envs = self.author_env();
        let hooks = git::EmptyHooks::new()?;
        let args = hooks.args(&[
            "-c",
            "commit.gpgsign=false",
            "commit-tree",
            tree,
            "-p",
            current.as_str(),
            "-p",
            gated.as_str(),
            "-m",
            &message,
        ]);
        let merged: CommitId = git::ok(&self.root, &args, &envs).await?.trim().parse()?;
        self.advance(&merged, expected_head)
            .await
            .map(LandOutcome::from)
    }

    /// Point the protected branch at `commit`, which descends from `expected_head`.
    pub(crate) async fn advance(
        &self,
        commit: &CommitId,
        expected_head: &CommitId,
    ) -> Result<Advance, Error> {
        match self.checkout_of_protected().await? {
            Some(checkout) => {
                self.advance_checkout(&checkout, commit, expected_head)
                    .await
            }
            None => self.advance_ref(commit, expected_head).await,
        }
    }

    /// Move the protected ref only if it still sits at `expected_head`. Used when no
    /// checkout has the protected branch checked out.
    async fn advance_ref(
        &self,
        commit: &CommitId,
        expected_head: &CommitId,
    ) -> Result<Advance, Error> {
        let protected_ref = format!("refs/heads/{}", self.protected);
        let hooks = git::EmptyHooks::new()?;
        let args = hooks.args(&[
            "update-ref",
            protected_ref.as_str(),
            commit.as_str(),
            expected_head.as_str(),
        ]);
        let out = git::run(&self.root, &args, &[]).await?;
        if out.status.success() {
            return Ok(Advance::Landed {
                commit: commit.clone(),
            });
        }
        let current = self.protected_head().await?;
        if current != *expected_head {
            return Ok(Advance::HeadMoved { current });
        }
        // The ref did not move, yet the update failed: a real git error.
        Err(git::ensure_ok(out, &args).expect_err("status was not success"))
    }

    /// Move the protected branch through the checkout that has it checked out.
    ///
    /// The checks come first: a dirty checkout, a checkout that left the protected branch,
    /// and a protected head that is not `expected_head` are refused. The move follows and
    /// names both heads in its ref update, so a head that moved after the checks is refused,
    /// never overwritten. A refusal to overwrite local files is a land failure, not a git
    /// error.
    async fn advance_checkout(
        &self,
        checkout: &Path,
        commit: &CommitId,
        expected_head: &CommitId,
    ) -> Result<Advance, Error> {
        match self.checkout_refusal(checkout, expected_head).await? {
            Some(refusal) => Ok(refusal),
            None => self.move_checkout(checkout, commit, expected_head).await,
        }
    }

    /// Why the checkout cannot take the move now, when it cannot: its tracked files are
    /// dirty, it no longer has the protected branch checked out, or the protected head is
    /// not `expected_head`.
    async fn checkout_refusal(
        &self,
        checkout: &Path,
        expected_head: &CommitId,
    ) -> Result<Option<Advance>, Error> {
        let hooks = git::EmptyHooks::new()?;
        let args = hooks.args(&[
            "-c",
            "core.fsmonitor=false",
            "status",
            "--porcelain",
            "--untracked-files=no",
        ]);
        let env = [("GIT_OPTIONAL_LOCKS".to_owned(), "0".to_owned())];
        let dirty = git::ok(checkout, &args, &env).await?;
        if !dirty.trim().is_empty() {
            return Ok(Some(Advance::CheckoutDirty {
                path: checkout.to_path_buf(),
            }));
        }
        match self.checkout_branch(checkout).await? {
            Some(branch) if branch == self.protected => {}
            _ => {
                return Ok(Some(Advance::CheckoutMoved {
                    path: checkout.to_path_buf(),
                }));
            }
        }
        let current = self.protected_head().await?;
        if current != *expected_head {
            return Ok(Some(Advance::HeadMoved { current }));
        }
        Ok(None)
    }

    /// Whether the checkout already holds a local file, ignored or not, at a path that `commit`
    /// adds over `expected_head`. Such a file is one the land would overwrite.
    async fn holds_added_path(
        &self,
        checkout: &Path,
        commit: &CommitId,
        expected_head: &CommitId,
    ) -> Result<bool, Error> {
        let args = [
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            "--diff-filter=A",
            expected_head.as_str(),
            commit.as_str(),
        ];
        let added = git::ok(&self.root, &args, &[]).await?;
        Ok(added
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(Path::new)
            .any(|path| {
                checkout.join(path).symlink_metadata().is_ok()
                    || path
                        .ancestors()
                        .skip(1)
                        .any(|ancestor| checkout.join(ancestor).is_file())
            }))
    }

    /// Points the protected ref from `expected_head` to `commit`, then brings the checkout's
    /// files onto `commit`.
    ///
    /// A local file at a path the commit adds is refused first, before the ref moves: the
    /// two-tree read below overwrites ignored local files without asking. The ref update names
    /// both values, so git refuses it when the branch is no longer at `expected_head`, whatever
    /// the checks saw. The checkout's files then follow with a two-tree read, where hooks cannot
    /// run. A local file the read still refuses sends the ref back to `expected_head`, so the
    /// branch never names a commit its checkout does not hold.
    async fn move_checkout(
        &self,
        checkout: &Path,
        commit: &CommitId,
        expected_head: &CommitId,
    ) -> Result<Advance, Error> {
        if self
            .holds_added_path(checkout, commit, expected_head)
            .await?
        {
            return Ok(Advance::CheckoutBlocked {
                path: checkout.to_path_buf(),
            });
        }
        let protected_ref = format!("refs/heads/{}", self.protected);
        let hooks = git::EmptyHooks::new()?;
        let ref_args = hooks.args(&[
            "update-ref",
            protected_ref.as_str(),
            commit.as_str(),
            expected_head.as_str(),
        ]);
        let out = git::run(&self.root, &ref_args, &[]).await?;
        if !out.status.success() {
            let current = self.protected_head().await?;
            if current != *expected_head {
                return Ok(Advance::HeadMoved { current });
            }
            return Err(git::ensure_ok(out, &ref_args).expect_err("status was not success"));
        }
        let tree_args = hooks.args(&[
            "-c",
            "core.fsmonitor=false",
            "read-tree",
            "-m",
            "-u",
            expected_head.as_str(),
            commit.as_str(),
        ]);
        let out = git::run(checkout, &tree_args, &[]).await?;
        if out.status.success() {
            // Only the postcondition says the land happened: the ref names `commit`.
            if self.protected_head().await? == *commit {
                return Ok(Advance::Landed {
                    commit: commit.clone(),
                });
            }
            return Err(Error::BadOutput {
                args: "read-tree".to_owned(),
                detail: format!("the protected branch is not at {commit}"),
            });
        }
        // The checkout did not move, so the branch goes back to the head it had.
        let revert_args = hooks.args(&[
            "update-ref",
            protected_ref.as_str(),
            expected_head.as_str(),
            commit.as_str(),
        ]);
        let revert = git::run(&self.root, &revert_args, &[]).await?;
        if !revert.status.success() {
            return Err(git::ensure_ok(revert, &revert_args).expect_err("status was not success"));
        }
        if out.stderr.contains("would be overwritten by merge") {
            return Ok(Advance::CheckoutBlocked {
                path: checkout.to_path_buf(),
            });
        }
        Err(git::ensure_ok(out, &tree_args).expect_err("status was not success"))
    }

    /// The branch `checkout` has checked out, or `None` when its head is detached.
    async fn checkout_branch(&self, checkout: &Path) -> Result<Option<String>, Error> {
        let out = git::run(checkout, &["symbolic-ref", "--short", "-q", "HEAD"], &[]).await?;
        if !out.status.success() {
            return Ok(None);
        }
        Ok(Some(out.stdout.trim().to_owned()))
    }

    /// The repository's common git directory, shared by every worktree.
    pub(crate) async fn common_dir(&self) -> Result<PathBuf, Error> {
        let out = git::ok(&self.root, &["rev-parse", "--git-common-dir"], &[]).await?;
        let common = PathBuf::from(out.trim());
        Ok(if common.is_absolute() {
            common
        } else {
            self.root.join(common)
        })
    }

    /// The checkout that has the protected branch checked out, the user's main
    /// checkout included.
    async fn checkout_of_protected(&self) -> Result<Option<PathBuf>, Error> {
        let out = git::ok(&self.root, &["worktree", "list", "--porcelain"], &[]).await?;
        let wanted = format!("branch refs/heads/{}", self.protected);
        let mut path: Option<PathBuf> = None;
        for line in out.lines() {
            if let Some(found) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(found));
            } else if line == wanted {
                return Ok(path);
            }
        }
        Ok(None)
    }

    /// The commit that landed `gated` on the way to `head`: `gated` itself after a
    /// fast-forward, else the merge commit whose second parent it is. Falls back to
    /// `head` when neither sits on the first-parent history, as when the user merged
    /// the task by hand.
    async fn landing_commit(&self, gated: &CommitId, head: &CommitId) -> Result<CommitId, Error> {
        if gated == head {
            return Ok(head.clone());
        }
        // Only the commits after the gated one can be the merge that landed it.
        let range = format!("{gated}..{head}");
        let args = ["log", "--first-parent", "--format=%H %P", &range];
        let out = git::ok(&self.root, &args, &[]).await?;
        for line in out.lines() {
            let mut ids = line.split_whitespace();
            let Some(commit) = ids.next() else {
                continue;
            };
            // Parents after the first are the ones a landing merge adds.
            let landed =
                commit == gated.as_str() || ids.skip(1).any(|parent| parent == gated.as_str());
            if landed {
                return Ok(commit.parse()?);
            }
        }
        Ok(head.clone())
    }

    pub(crate) async fn is_ancestor(
        &self,
        older: &CommitId,
        newer: &CommitId,
    ) -> Result<bool, Error> {
        let args = [
            "merge-base",
            "--is-ancestor",
            older.as_str(),
            newer.as_str(),
        ];
        let out = git::run(&self.root, &args, &[]).await?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => git::ensure_ok(out, &args).map(|_| false),
        }
    }

    pub(crate) fn author_env(&self) -> [(String, String); 4] {
        [
            ("GIT_AUTHOR_NAME".to_owned(), self.author_name.clone()),
            ("GIT_AUTHOR_EMAIL".to_owned(), self.author_email.clone()),
            ("GIT_COMMITTER_NAME".to_owned(), self.author_name.clone()),
            ("GIT_COMMITTER_EMAIL".to_owned(), self.author_email.clone()),
        ]
    }
}

/// What [`Repo::land`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
#[non_exhaustive]
pub enum LandOutcome {
    /// The task landed; the protected branch now points at this commit.
    Landed {
        /// The new protected head: the gated commit on a fast-forward, else the
        /// merge commit. On a retry, the commit that landed the task.
        commit: CommitId,
    },
    /// The merge conflicts. The protected branch did not move.
    Conflict {
        /// The conflicting repo-relative paths.
        files: Vec<FilePath>,
    },
    /// The protected branch moved since `expected_head` was read. Nothing was
    /// merged; retry against `current`.
    HeadMoved {
        /// The protected head now.
        current: CommitId,
    },
    /// The checkout of the protected branch has uncommitted changes to tracked
    /// files, so landing would overwrite them. Nothing moved; commit or stash there
    /// and land again.
    CheckoutDirty {
        /// The checkout that has the protected branch.
        path: PathBuf,
    },
    /// The checkout holds an untracked or ignored file the land would overwrite.
    /// Nothing moved, and the file is kept; move it and land again.
    CheckoutBlocked {
        /// The checkout that has the protected branch.
        path: PathBuf,
    },
    /// The checkout that had the protected branch switched away from it between the
    /// decision and the merge. Nothing moved.
    CheckoutMoved {
        /// The checkout that switched.
        path: PathBuf,
    },
}

impl From<LandOutcome> for Result<CommitId, String> {
    /// The shape [`acan_core::SystemCmd::LandResult`] carries.
    fn from(outcome: LandOutcome) -> Self {
        match outcome {
            LandOutcome::Landed { commit } => Ok(commit),
            LandOutcome::Conflict { files } => {
                let names: Vec<String> = files.iter().map(ToString::to_string).collect();
                Err(format!("merge conflict in {}", names.join(", ")))
            }
            LandOutcome::HeadMoved { current } => {
                Err(format!("protected branch moved to {current}"))
            }
            LandOutcome::CheckoutDirty { path } => Err(format!(
                "{} has uncommitted changes; commit or stash to land",
                path.display()
            )),
            LandOutcome::CheckoutBlocked { path } => Err(format!(
                "{} has untracked files the land would overwrite; move them to land",
                path.display()
            )),
            LandOutcome::CheckoutMoved { path } => Err(format!(
                "{} left the protected branch; land again",
                path.display()
            )),
        }
    }
}

/// How moving the protected branch to a commit that descends from the expected head ended.
pub(crate) enum Advance {
    /// The protected branch now points at `commit`.
    Landed { commit: CommitId },
    /// The protected branch is no longer at the expected head.
    HeadMoved { current: CommitId },
    /// The checkout of the protected branch has uncommitted changes to tracked files.
    CheckoutDirty { path: PathBuf },
    /// The checkout holds an untracked or ignored file the move would overwrite.
    CheckoutBlocked { path: PathBuf },
    /// The checkout switched away from the protected branch.
    CheckoutMoved { path: PathBuf },
}

impl From<Advance> for LandOutcome {
    fn from(advance: Advance) -> Self {
        match advance {
            Advance::Landed { commit } => Self::Landed { commit },
            Advance::HeadMoved { current } => Self::HeadMoved { current },
            Advance::CheckoutDirty { path } => Self::CheckoutDirty { path },
            Advance::CheckoutBlocked { path } => Self::CheckoutBlocked { path },
            Advance::CheckoutMoved { path } => Self::CheckoutMoved { path },
        }
    }
}

/// The task worktrees of a [`Repo`], borrowed from it.
#[derive(Debug)]
pub struct Worktrees<'r> {
    repo: &'r Repo,
}

impl Worktrees<'_> {
    /// Create the worktree for `task` at `generation`, on branch `acan/<task>`,
    /// starting at the protected head.
    ///
    /// A worktree already recorded at `generation` is returned unchanged. A worktree
    /// recorded at another generation is removed first, branch included, so a reclaim
    /// starts at the current protected head and keeps none of the earlier claim's
    /// commits. The generation is stored on the branch, so a restart can tell a live
    /// claim's worktree from a stale one.
    ///
    /// # Errors
    /// Fails when the slot holds a foreign directory, or git cannot create the
    /// worktree.
    pub async fn create(
        &self,
        task: TaskId,
        generation: Generation,
    ) -> Result<TaskWorktree, Error> {
        let dir = self.dir_for(task);
        let branch = format!("acan/{task}");
        if dir.exists() {
            let current = git::ok(&dir, &["rev-parse", "--abbrev-ref", "HEAD"], &[]).await?;
            if current.trim() != branch {
                return Err(Error::SlotTaken(dir));
            }
            if self.recorded_generation(&branch).await? == Some(generation) {
                return Ok(TaskWorktree::new(dir, task));
            }
            // The directory goes now so the new worktree can take its path. The branch stays
            // until the new one exists, so a failed create leaves the old claim recoverable.
            self.remove_worktree(task).await?;
        }
        let stale = self.branch_exists(&branch).await?
            && self.recorded_generation(&branch).await? != Some(generation);
        let stale_tip = if stale {
            self.branch_tip(&branch).await?
        } else {
            None
        };
        self.ensure_excluded().await?;
        let dir_arg = path_str(&dir)?;
        if let Some(tip) = &stale_tip {
            // The old branch goes only once the new worktree can take its name. A failure
            // here leaves the branch, so the old claim stays recoverable.
            self.delete_branch_at(&branch, tip).await?;
        }
        if self.branch_exists(&branch).await? {
            git::ok(&self.repo.root, &["worktree", "add", dir_arg, &branch], &[]).await?;
        } else {
            let base = format!("refs/heads/{}", self.repo.protected);
            git::ok(
                &self.repo.root,
                &["worktree", "add", dir_arg, "-b", &branch, &base],
                &[],
            )
            .await?;
        }
        self.record_generation(&branch, generation).await?;
        Ok(TaskWorktree::new(dir, task))
    }

    /// Remove the worktree and branch of `task`, but only when the recorded generation
    /// is still `expected`.
    ///
    /// A newer generation recorded since the caller decided to remove is left in place,
    /// so a late removal never deletes a newer claim's worktree. A task with no recorded
    /// generation is removed, which clears leftovers from before generations were stored.
    ///
    /// # Errors
    /// Fails if git or the filesystem cannot remove the leftovers.
    pub async fn reset(&self, task: TaskId, expected: Generation) -> Result<(), Error> {
        let branch = format!("acan/{task}");
        if self
            .recorded_generation(&branch)
            .await?
            .is_some_and(|recorded| recorded != expected)
        {
            return Ok(());
        }
        // The generation can change between the check above and the removal, so the branch
        // delete itself is guarded by the tip read here. A newer claim moves that tip.
        let tip = self.branch_tip(&branch).await?;
        self.remove_worktree(task).await?;
        if let Some(tip) = tip {
            self.delete_branch_at(&branch, &tip).await?;
        }
        Ok(())
    }

    /// The tip of `branch`, when the branch exists.
    async fn branch_tip(&self, branch: &str) -> Result<Option<CommitId>, Error> {
        if !self.branch_exists(branch).await? {
            return Ok(None);
        }
        let reference = format!("refs/heads/{branch}");
        let out = git::ok(&self.repo.root, &["rev-parse", &reference], &[]).await?;
        Ok(Some(out.trim().parse()?))
    }

    /// Delete `branch` only if it still points at `tip`. A branch that moved is a newer claim's.
    ///
    /// A failed delete is ignored only when the observed state changed: the branch is gone, its
    /// tip moved, or its recorded generation changed. Any other failure surfaces, because the
    /// caller must not stamp a new generation over a branch it failed to delete.
    async fn delete_branch_at(&self, branch: &str, tip: &CommitId) -> Result<(), Error> {
        let reference = format!("refs/heads/{branch}");
        let observed = self.recorded_generation(branch).await?;
        let args = ["update-ref", "-d", reference.as_str(), tip.as_str()];
        let out = git::run(&self.repo.root, &args, &[]).await?;
        if out.status.success() {
            return Ok(());
        }
        // A failed delete of a branch that moved, or that a newer generation recreated at the
        // same commit, leaves that generation's branch in place.
        if !self.branch_exists(branch).await?
            || self.branch_tip(branch).await? != Some(tip.clone())
            || self.recorded_generation(branch).await? != observed
        {
            return Ok(());
        }
        git::ensure_ok(out, &args).map(drop)
    }

    /// The generation recorded on `task`'s branch, when the branch exists and records one.
    ///
    /// # Errors
    /// Fails if git cannot read the branch config.
    pub async fn generation(&self, task: TaskId) -> Result<Option<Generation>, Error> {
        let branch = format!("acan/{task}");
        self.recorded_generation(&branch).await
    }

    /// Remove the worktree and branch of `task`, including uncommitted work.
    ///
    /// Idempotent: removing a task with no worktree or branch is a no-op.
    ///
    /// # Errors
    /// Fails if git or the filesystem cannot remove the leftovers.
    pub async fn remove(&self, task: TaskId) -> Result<(), Error> {
        let branch = format!("acan/{task}");
        self.remove_worktree(task).await?;
        if self.branch_exists(&branch).await? {
            git::ok(&self.repo.root, &["branch", "-D", &branch], &[]).await?;
        }
        Ok(())
    }

    /// Remove the worktree directory of `task`, leaving its branch for the caller to delete.
    async fn remove_worktree(&self, task: TaskId) -> Result<(), Error> {
        let dir = self.dir_for(task);
        if dir.exists() {
            let dir_arg = path_str(&dir)?;
            let out = git::run(
                &self.repo.root,
                &["worktree", "remove", "--force", dir_arg],
                &[],
            )
            .await?;
            if !out.status.success() {
                // Git lost track of it; drop the files directly.
                git::ok(&self.repo.root, &["worktree", "prune"], &[]).await?;
                if dir.exists() {
                    tokio::fs::remove_dir_all(&dir).await?;
                }
            }
        }
        Ok(())
    }

    /// Every live task worktree, sorted by task id.
    ///
    /// # Errors
    /// Fails if the worktree directory cannot be read.
    pub async fn list(&self) -> Result<Vec<TaskWorktree>, Error> {
        let base = self.repo.root.join(".acan").join("worktrees");
        let mut entries = match tokio::fs::read_dir(&base).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Error::Io(e)),
        };
        let mut found = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            if !entry.file_type().await?.is_dir() {
                continue;
            }
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            // A directory that is not a task id is not ours; leave it alone.
            if let Ok(task) = name.parse::<TaskId>() {
                found.push(TaskWorktree::new(entry.path(), task));
            }
        }
        found.sort_by(|a, b| a.task().cmp(b.task()));
        Ok(found)
    }

    fn dir_for(&self, task: TaskId) -> PathBuf {
        self.repo
            .root
            .join(".acan")
            .join("worktrees")
            .join(task.to_string())
    }

    /// `branch.acan/<task>.acanGeneration`, set when the worktree is created.
    async fn recorded_generation(&self, branch: &str) -> Result<Option<Generation>, Error> {
        let key = format!("branch.{branch}.acanGeneration");
        let Some(raw) = config(&self.repo.root, &key).await? else {
            return Ok(None);
        };
        Ok(raw.parse::<u32>().ok().map(Generation::new))
    }

    async fn record_generation(&self, branch: &str, generation: Generation) -> Result<(), Error> {
        let key = format!("branch.{branch}.acanGeneration");
        let value = generation.get().to_string();
        git::ok(&self.repo.root, &["config", &key, &value], &[]).await?;
        Ok(())
    }

    async fn branch_exists(&self, branch: &str) -> Result<bool, Error> {
        let reference = format!("refs/heads/{branch}");
        let args = ["show-ref", "--verify", "--quiet", &reference];
        let out = git::run(&self.repo.root, &args, &[]).await?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => git::ensure_ok(out, &args).map(|_| false),
        }
    }

    /// Worktrees live inside the repository, so keep them out of `git status`.
    async fn ensure_excluded(&self) -> Result<(), Error> {
        let out = git::ok(&self.repo.root, &["rev-parse", "--git-common-dir"], &[]).await?;
        let common = PathBuf::from(out.trim());
        let common = if common.is_absolute() {
            common
        } else {
            self.repo.root.join(common)
        };
        let exclude = common.join("info").join("exclude");
        let existing = tokio::fs::read_to_string(&exclude)
            .await
            .unwrap_or_default();
        if existing.lines().any(|line| line.trim() == "/.acan/") {
            return Ok(());
        }
        let mut content = existing;
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str("/.acan/\n");
        tokio::fs::write(&exclude, content).await?;
        Ok(())
    }
}

async fn repo_root(path: &Path) -> Result<PathBuf, Error> {
    let args = ["rev-parse", "--show-toplevel"];
    let out = git::run(path, &args, &[]).await.map_err(|e| match e {
        e @ (Error::Timeout(_) | Error::Spawn { .. }) => e,
        _ => Error::NotARepo(path.to_path_buf()),
    })?;
    if !out.status.success() {
        return Err(Error::NotARepo(path.to_path_buf()));
    }
    Ok(PathBuf::from(out.stdout.trim()))
}

/// A repository-local config value, `None` when unset.
async fn config(root: &Path, key: &str) -> Result<Option<String>, Error> {
    let out = git::run(root, &["config", key], &[]).await?;
    if !out.status.success() {
        return Ok(None);
    }
    let value = out.stdout.trim();
    Ok((!value.is_empty()).then(|| value.to_owned()))
}

pub(crate) fn path_str(path: &Path) -> Result<&str, Error> {
    path.to_str()
        .ok_or_else(|| Error::NonUtf8Path(path.to_path_buf()))
}

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Repo>();
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs git in `dir`, failing the test on a non-zero exit.
    fn run_git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// Writes `file` with `content`, commits it on the current branch, and returns the head.
    fn commit_file(root: &Path, file: &str, content: &str, message: &str) -> CommitId {
        std::fs::write(root.join(file), content).expect("write file");
        run_git(root, &["add", "-A"]);
        run_git(root, &["commit", "-q", "-m", message]);
        run_git(root, &["rev-parse", "HEAD"])
            .parse()
            .expect("the head is a commit")
    }

    /// The repository the tests move. `main` holds `parent` and then `base`, the protected
    /// head. `candidate` is a commit on `base` on another branch. `main` is checked out and
    /// clean.
    struct Chain {
        dir: tempfile::TempDir,
        repo: Repo,
        parent: CommitId,
        base: CommitId,
        candidate: CommitId,
    }

    async fn chain() -> Chain {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        run_git(&root, &["init", "-q", "-b", "main"]);
        run_git(&root, &["config", "user.name", "Tester"]);
        run_git(&root, &["config", "user.email", "tester@example.com"]);
        let parent = commit_file(&root, "shared.txt", "parent\n", "parent");
        let base = commit_file(&root, "base.txt", "base\n", "base");
        run_git(&root, &["checkout", "-q", "-b", "cand"]);
        let candidate = commit_file(&root, "candidate.txt", "candidate\n", "candidate");
        run_git(&root, &["checkout", "-q", "main"]);
        let repo = Repo::open(&root).await.expect("open repo");
        Chain {
            dir,
            repo,
            parent,
            base,
            candidate,
        }
    }

    /// A head that moves to an ancestor of the candidate after the checks is refused. The
    /// protected branch keeps the outside writer's commit, and the candidate's files never
    /// reach the checkout. Before the fix, `merge --ff-only` fast-forwarded the branch onto the
    /// candidate from the ancestor.
    #[tokio::test]
    async fn a_head_moved_to_an_ancestor_after_the_checks_installs_nothing() {
        let c = chain().await;
        let root = c.dir.path();
        let checks = c
            .repo
            .checkout_refusal(root, &c.base)
            .await
            .expect("the checks run");
        assert!(
            checks.is_none(),
            "the checks pass while main sits at the base"
        );

        // An outside writer moves main to the parent, an ancestor of the candidate, after the
        // checks and before the move.
        run_git(root, &["reset", "--quiet", "--hard", c.parent.as_str()]);
        let outcome = c
            .repo
            .move_checkout(root, &c.candidate, &c.base)
            .await
            .expect("the move runs");
        assert!(
            matches!(&outcome, Advance::HeadMoved { current } if *current == c.parent),
            "the move names the head it found, got another outcome"
        );
        assert_eq!(
            c.repo.protected_head().await.expect("protected head"),
            c.parent,
            "the outside writer's commit stays on the protected branch"
        );
        assert!(
            !root.join("candidate.txt").exists(),
            "the candidate's files never reach the checkout"
        );
    }

    /// A local file where the candidate adds one blocks the checkout's update. The branch goes
    /// back to the base, so it never names a commit its checkout does not hold.
    #[tokio::test]
    async fn a_checkout_blocked_by_a_local_file_puts_the_branch_back() {
        let c = chain().await;
        let root = c.dir.path();
        std::fs::write(root.join("candidate.txt"), "local\n").expect("write local file");

        let outcome = c
            .repo
            .move_checkout(root, &c.candidate, &c.base)
            .await
            .expect("the move runs");
        assert!(
            matches!(outcome, Advance::CheckoutBlocked { .. }),
            "the local file blocks the checkout"
        );
        assert_eq!(
            c.repo.protected_head().await.expect("protected head"),
            c.base,
            "the protected branch is back on the base"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("candidate.txt")).expect("local file"),
            "local\n",
            "the local file is untouched"
        );
    }

    /// From the expected head, the move lands the commit and brings its files onto the
    /// checkout, which stays clean.
    #[tokio::test]
    async fn a_move_from_the_expected_head_lands_the_commit_and_its_files() {
        let c = chain().await;
        let root = c.dir.path();

        let outcome = c
            .repo
            .move_checkout(root, &c.candidate, &c.base)
            .await
            .expect("the move runs");
        assert!(
            matches!(&outcome, Advance::Landed { commit } if *commit == c.candidate),
            "the move lands the candidate"
        );
        assert_eq!(
            c.repo.protected_head().await.expect("protected head"),
            c.candidate
        );
        // A checkout with `core.autocrlf` writes CRLF, so the text is compared with LF endings.
        let landed = std::fs::read_to_string(root.join("candidate.txt")).expect("landed file");
        assert_eq!(landed.replace("\r\n", "\n"), "candidate\n");
        assert_eq!(run_git(root, &["status", "--porcelain"]), "");
    }
}
