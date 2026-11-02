//! `acan init` in a clone. It reads the committed files of a base branch, shows the plan, and
//! writes the four protected files to a new branch, or to the working tree with `--working-tree`.
//!
//! The clone's checkout, index and HEAD are not touched in the default mode. The new branch is
//! created in a temporary worktree, which is removed before the command returns.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;

use crate::init::{self, FILES, Options, Plan, Snapshot};

/// The branch the contract is committed to, unless `--branch` names another.
pub const DEFAULT_BRANCH: &str = "acan/init";
/// The commit message of the contract commit.
const COMMIT_MESSAGE: &str = "Add the acan app contract (acan init)";
/// The answer that confirms a write.
const CONFIRMATION: &str = "yes";

/// The arguments of `acan init`.
#[derive(Debug, Clone, Args)]
pub struct InitArgs {
    /// The clone to read, and to write the branch into.
    #[arg(long)]
    pub repo: PathBuf,
    /// The local branch to read. Defaults to the clone's current branch.
    #[arg(long)]
    pub base: Option<String>,
    /// The new branch the contract is committed to.
    #[arg(long, default_value = DEFAULT_BRANCH)]
    pub branch: String,
    /// Writes the four files into the clone's working tree, with no commit. Receipts read
    /// committed files, so commit them to the base before a receipt runs.
    #[arg(long)]
    pub working_tree: bool,
    /// Prints the detected plan and writes nothing.
    #[arg(long)]
    pub print: bool,
    /// The workspace package to boot, by its directory, when several have a start script.
    #[arg(long)]
    pub package: Option<String>,
    /// The ready route, when the sources name none of the usual ones. Must answer 200 on GET.
    #[arg(long)]
    pub ready_path: Option<String>,
}

/// Runs `acan init` with the human's confirmation at the terminal, and returns the text to print.
///
/// # Errors
///
/// Returns an error, and writes nothing, when the base is not a local branch, when the base
/// already has `.acan/`, when detection refuses, when the human does not confirm, or when git
/// fails.
pub fn run(args: &InitArgs) -> Result<String> {
    run_with(args, &mut confirmed)
}

/// Runs `acan init`. `confirm` receives the question and answers whether to write; it is asked
/// only when something would be written.
///
/// # Errors
///
/// As [`run`].
pub fn run_with(args: &InitArgs, confirm: &mut dyn FnMut(&str) -> bool) -> Result<String> {
    let repo = args
        .repo
        .canonicalize()
        .with_context(|| format!("the clone {} does not exist", args.repo.display()))?;
    let base = match &args.base {
        Some(base) => base.clone(),
        None => current_branch(&repo)?,
    };
    let commit = local_branch_commit(&repo, &base)?;
    if !git(&repo, &["ls-tree", "-z", &commit, "--", ".acan"])?.is_empty() {
        bail!("init refused: {base} already has .acan/, so there is nothing to write");
    }
    let snapshot = GitSnapshot::open(&repo, &commit)?;
    let options = Options {
        package: args.package.clone(),
        ready_path: args.ready_path.clone(),
    };
    let plan = init::detect(&snapshot, &options).map_err(|err| anyhow!("{err}"))?;
    if args.print {
        return Ok(init::render(&plan));
    }
    let question = if args.working_tree {
        format!("Write the four files into the working tree of {base}?")
    } else {
        format!(
            "Commit the four files to a new branch {} from {base}?",
            args.branch
        )
    };
    eprintln!("{}", init::render(&plan));
    if !confirm(&question) {
        bail!("init cancelled: nothing was written");
    }
    if args.working_tree {
        write_working_tree(&repo, &plan)?;
        return Ok(format!(
            "Wrote {} files under .acan/ in {}. Nothing is committed. Commit them to {base} \
             before a receipt runs, because receipts read committed files.",
            FILES.len(),
            repo.display()
        ));
    }
    let contract = write_branch(&repo, &commit, &args.branch, &plan)?;
    let written = GitSnapshot::open(&repo, &contract)?;
    for file in FILES {
        if written.text(file)? != plan.file(file).unwrap_or_default() {
            bail!("{file} in the new commit differs from the plan; nothing is reported as written");
        }
    }
    let app = written.text(init::APP_PATH)?;
    let corpus = written.text(init::CORPUS_PATH)?;
    init::check_files(&app, &corpus).map_err(|err| anyhow!("{err}"))?;
    Ok(format!(
        "Committed the acan app contract to {branch} at {short}.\n\
         Next: push {branch} yourself, open a pull request, and merge it to {base}.\n\
         Then run: acan receipt --repo {repo} --base {base} --head {branch}",
        branch = args.branch,
        short = &contract[..contract.len().min(12)],
        repo = repo.display(),
    ))
}

/// Asks the human at the terminal to confirm the write. Anything but `yes` cancels it. A read
/// error counts as no.
fn confirmed(question: &str) -> bool {
    eprint!("{question} Type yes to write: ");
    if io::stderr().flush().is_err() {
        return false;
    }
    let mut answer = String::new();
    match io::stdin().lock().read_line(&mut answer) {
        Ok(_) => answer.trim() == CONFIRMATION,
        Err(_) => false,
    }
}

/// The branch the clone's HEAD names.
fn current_branch(repo: &Path) -> Result<String> {
    // `symbolic-ref -q` exits non-zero on a detached HEAD, which is the case this reports.
    let name = text(&git(repo, &["symbolic-ref", "--short", "-q", "HEAD"]).unwrap_or_default());
    if name.is_empty() {
        bail!("the clone is on a detached HEAD; pass --base <branch>");
    }
    Ok(name)
}

/// The commit a local branch names. A commit or a remote branch is refused.
fn local_branch_commit(repo: &Path, base: &str) -> Result<String> {
    let reference = format!("refs/heads/{base}");
    let output = git(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{reference}^{{commit}}"),
        ],
    );
    match output {
        Ok(bytes) if !bytes.is_empty() => Ok(text(&bytes)),
        _ => bail!("{base} is not a local branch of the clone; pass --base <local branch>"),
    }
}

/// Writes the four files to a new branch made from `base_commit`, in a temporary worktree.
/// Returns the new commit. On failure the branch is deleted, if this call created it.
fn write_branch(repo: &Path, base_commit: &str, branch: &str, plan: &Plan) -> Result<String> {
    git(repo, &["check-ref-format", &format!("refs/heads/{branch}")])
        .map_err(|_| anyhow!("{branch} is not a valid branch name"))?;
    if !git(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .unwrap_or_default()
    .is_empty()
    {
        bail!("the branch {branch} already exists; pass --branch <new name>");
    }
    require_identity(repo)?;
    let dir = tempfile::tempdir().context("a temporary directory for the worktree")?;
    let worktree = dir.path().join("init");
    let worktree_arg = worktree.to_string_lossy().into_owned();
    git(
        repo,
        &["worktree", "add", "-b", branch, &worktree_arg, base_commit],
    )?;
    let result = commit_in(&worktree, plan);
    let removed = git(repo, &["worktree", "remove", "--force", &worktree_arg]);
    match (result, removed) {
        (Ok(commit), Ok(_)) => Ok(commit),
        (Ok(_), Err(err)) => {
            let _ = git(repo, &["branch", "-D", branch]);
            Err(err.context("removing the temporary worktree"))
        }
        (Err(err), _) => {
            let _ = git(repo, &["worktree", "remove", "--force", &worktree_arg]);
            let _ = git(repo, &["branch", "-D", branch]);
            Err(err)
        }
    }
}

/// Writes the files into a worktree, commits them, and returns the commit.
fn commit_in(worktree: &Path, plan: &Plan) -> Result<String> {
    write_files(worktree, plan)?;
    git(worktree, &["add", "--", ".acan"])?;
    git(worktree, &["commit", "-q", "-m", COMMIT_MESSAGE])?;
    Ok(text(&git(worktree, &["rev-parse", "HEAD"])?))
}

/// Writes the four files into the clone's working tree. Refuses when any of them exists or when
/// `.acan/` has changes.
fn write_working_tree(repo: &Path, plan: &Plan) -> Result<()> {
    for file in FILES {
        if repo.join(file).exists() {
            bail!("{file} already exists in the working tree; remove it, then run again");
        }
    }
    if !git(repo, &["status", "--porcelain", "--", ".acan"])?.is_empty() {
        bail!("the working tree has changes under .acan/; commit or stash them first");
    }
    write_files(repo, plan)
}

/// Writes every generated file under `root`, creating directories as needed.
fn write_files(root: &Path, plan: &Plan) -> Result<()> {
    for file in &plan.files {
        let path = root.join(file.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        fs::write(&path, &file.contents).with_context(|| format!("writing {}", file.path))?;
    }
    Ok(())
}

/// Refuses to commit without a configured identity. acan never invents one.
fn require_identity(repo: &Path) -> Result<()> {
    for key in ["user.name", "user.email"] {
        let value = git(repo, &["config", "--get", key]).unwrap_or_default();
        if text(&value).is_empty() {
            bail!("git {key} is not set; set it, then run again. acan does not invent an identity");
        }
    }
    Ok(())
}

/// The committed files of one commit: paths and blob ids, read through git.
struct GitSnapshot {
    repo: PathBuf,
    blobs: BTreeMap<String, String>,
}

impl GitSnapshot {
    fn open(repo: &Path, commit: &str) -> Result<Self> {
        let listing = git(repo, &["ls-tree", "-r", "-z", "--full-tree", commit])?;
        let mut blobs = BTreeMap::new();
        for entry in listing
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
        {
            let entry = String::from_utf8_lossy(entry);
            let Some((meta, path)) = entry.split_once('\t') else {
                continue;
            };
            let mut fields = meta.split(' ');
            let (Some(mode), Some(kind), Some(oid)) = (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            if kind == "blob" && mode != "120000" {
                blobs.insert(path.to_owned(), oid.to_owned());
            }
        }
        Ok(Self {
            repo: repo.to_path_buf(),
            blobs,
        })
    }

    /// The text of one committed file.
    fn text(&self, path: &str) -> Result<String> {
        let bytes = self
            .read(path)
            .ok_or_else(|| anyhow!("{path} is not in the commit"))?;
        String::from_utf8(bytes).map_err(|_| anyhow!("{path} is not UTF-8"))
    }
}

impl Snapshot for GitSnapshot {
    fn paths(&self) -> Vec<String> {
        self.blobs.keys().cloned().collect()
    }

    fn read(&self, path: &str) -> Option<Vec<u8>> {
        let oid = self.blobs.get(path)?;
        git(&self.repo, &["cat-file", "blob", oid]).ok()
    }
}

/// Runs git in `repo` and returns its standard output. A failure carries git's message.
fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("running git")?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

/// Trimmed text of git output.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_owned()
}
