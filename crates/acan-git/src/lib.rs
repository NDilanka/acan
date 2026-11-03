//! Per-task git worktrees, queue-ordered landing, and the gate side of git.
//!
//! Each agent task works in its own worktree under `.acan/worktrees/<task>` on
//! branch `acan/<task>`. When a task passes its gate, [`Repo::land`] merges the
//! gated commit into the protected branch and reports a plain [`LandOutcome`] the
//! server turns into `acan_core::SystemCmd::LandResult`.
//!
//! The behavior gate goes through [`Repo::prepare`], [`Repo::materialize`] and
//! [`Repo::install`]: the candidate is merged into the protected head without moving it,
//! every protected path is restored from the protected head, each run tests that exact
//! commit, and only that commit is installed, by compare-and-swap on the head.
//!
//! All git access shells out to the `git` CLI, so behavior matches the user's
//! own git, and every call has a timeout with stderr captured into [`Error`].

mod checkout;
mod error;
mod git;
pub mod git_env;
mod prepare;
mod protected;
mod repo;
mod worktree;

pub use acan_core;

pub use error::Error;
pub use prepare::{InstallOutcome, Preparation, PrepareOutcome, ProtectedExport};
pub use protected::ProtectedPaths;
pub use repo::{LandOutcome, Repo, Worktrees};
pub use worktree::{Author, TaskWorktree};
