//! `acan init` in a clone. It reads the committed files of a base branch, shows the plan, and
//! writes the four protected files to a new branch, or to the working tree with `--working-tree`.
//!
//! The clone's checkout, index and HEAD are not touched in the default mode. The branch is written
//! with git plumbing: each file becomes a blob, a tree is built in a temporary index, one commit is
//! made, and one ref is created. No worktree is checked out. Every git call runs with hooks
//! disabled and fsmonitor off, so the clone's configuration cannot run code on the host.

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
/// Where git looks for hooks. Pointing it at the null device means no hook runs.
#[cfg(windows)]
const NO_HOOKS: &str = "NUL";
#[cfg(not(windows))]
const NO_HOOKS: &str = "/dev/null";

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

/// Writes the four files to a new branch made from `base_commit`. Each file becomes a blob, a tree
/// is built from the base's tree in a temporary index, and one commit is made on it. The branch is
/// created last, and only if it does not exist, so a failure leaves no branch behind. Returns the
/// new commit.
fn write_branch(repo: &Path, base_commit: &str, branch: &str, plan: &Plan) -> Result<String> {
    let reference = format!("refs/heads/{branch}");
    git(repo, &["check-ref-format", &reference])
        .map_err(|_| anyhow!("{branch} is not a valid branch name"))?;
    if !git(repo, &["rev-parse", "--verify", "--quiet", &reference])
        .unwrap_or_default()
        .is_empty()
    {
        bail!("the branch {branch} already exists; pass --branch <new name>");
    }
    require_identity(repo)?;
    let scratch = tempfile::tempdir().context("a temporary directory for the index")?;
    let index = scratch.path().join("index").to_string_lossy().into_owned();
    let env = [("GIT_INDEX_FILE", index.as_str())];
    git_with(repo, &env, &["read-tree", base_commit], None)?;
    for file in &plan.files {
        let blob = text(&git_with(
            repo,
            &[],
            &["hash-object", "-w", "--no-filters", "--stdin"],
            Some(file.contents.as_bytes()),
        )?);
        let cacheinfo = format!("100644,{blob},{}", file.path);
        git_with(
            repo,
            &env,
            &["update-index", "--add", "--cacheinfo", &cacheinfo],
            None,
        )?;
    }
    let tree = text(&git_with(repo, &env, &["write-tree"], None)?);
    let commit = text(&git_with(
        repo,
        &[],
        &[
            "commit-tree",
            &tree,
            "-p",
            base_commit,
            "-m",
            COMMIT_MESSAGE,
        ],
        None,
    )?);
    // An empty old value makes the update fail if the branch exists, so nothing is replaced.
    git_with(repo, &[], &["update-ref", &reference, &commit, ""], None)?;
    Ok(commit)
}

/// Writes the four files into the clone's working tree. Refuses when any of them exists, when
/// `.acan/` has changes, or when a directory on the way is a link or junction. Every path is
/// checked before anything is written, and each file is created without following a link.
fn write_working_tree(repo: &Path, plan: &Plan) -> Result<()> {
    for file in FILES {
        if fs::symlink_metadata(repo.join(file)).is_ok() {
            bail!("{file} already exists in the working tree; remove it, then run again");
        }
    }
    for file in &plan.files {
        let parts: Vec<&str> = file.path.split('/').collect();
        let mut current = repo.to_path_buf();
        for part in &parts[..parts.len() - 1] {
            current.push(part);
            match fs::symlink_metadata(&current) {
                Ok(meta) if is_link(&meta) => bail!(
                    "{} is a link or junction; refusing to write through it",
                    current.display()
                ),
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => bail!("{} is not a directory", current.display()),
                Err(source) if source.kind() == io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(source).with_context(|| format!("reading {}", current.display()));
                }
            }
        }
    }
    if !git(repo, &["status", "--porcelain", "--", ".acan"])?.is_empty() {
        bail!("the working tree has changes under .acan/; commit or stash them first");
    }
    for file in &plan.files {
        let parts: Vec<&str> = file.path.split('/').collect();
        let (name, dirs) = parts.split_last().context("a file has a name")?;
        let mut current = repo.to_path_buf();
        for part in dirs {
            current.push(part);
            // Checked again here, because the tree may change between the two passes.
            match fs::symlink_metadata(&current) {
                Ok(meta) if is_link(&meta) => bail!(
                    "{} is a link or junction; refusing to write through it",
                    current.display()
                ),
                Ok(_) => {}
                Err(source) if source.kind() == io::ErrorKind::NotFound => {
                    fs::create_dir(&current)
                        .with_context(|| format!("creating {}", current.display()))?;
                }
                Err(source) => {
                    return Err(source).with_context(|| format!("reading {}", current.display()));
                }
            }
        }
        let path = current.join(name);
        let mut handle = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("creating {}", file.path))?;
        handle
            .write_all(file.contents.as_bytes())
            .with_context(|| format!("writing {}", file.path))?;
    }
    Ok(())
}

/// Whether `meta` is a symbolic link, or a junction or other reparse point on Windows.
fn is_link(meta: &fs::Metadata) -> bool {
    meta.file_type().is_symlink() || is_reparse_point(meta)
}

#[cfg(windows)]
fn is_reparse_point(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const REPARSE_POINT: u32 = 0x400;
    meta.file_attributes() & REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_meta: &fs::Metadata) -> bool {
    false
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

/// Runs git in `repo` and returns its standard output. Hooks are disabled and fsmonitor is off, so
/// the clone's configuration cannot run code here. A failure carries git's message.
fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    git_with(repo, &[], args, None)
}

/// As [`git`], with extra environment variables, and with `input` written to git's standard input
/// when it is given.
fn git_with(
    repo: &Path,
    env: &[(&str, &str)],
    args: &[&str],
    input: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args(["-c", &format!("core.hooksPath={NO_HOOKS}")])
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .envs(env.iter().copied())
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().context("running git")?;
    if let Some(bytes) = input
        && let Some(mut stdin) = child.stdin.take()
    {
        // A git that exits early closes its input. Its exit status reports that failure.
        let _ = stdin.write_all(bytes);
    }
    let output = child.wait_with_output().context("running git")?;
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
