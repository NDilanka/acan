//! The daemon's local hook listener, live order policies, and first-run check.
//!
//! The hook gives fast pre-tool feedback. It does not sandbox an allowed shell, and paths can
//! change between this check and tool execution. The landing gate's post-hook scan of the
//! resulting commit is the non-racy, commit-time enforcement boundary.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use acan_hook::protocol::{Decision, HookRequest, HookResponse, PROTOCOL_VERSION, RequestKind};
use acan_hook::transport::Endpoint;
use acan_hook::{BLOCK_EXIT_CODE, ENDPOINT_ENV, MAX_TIMEOUT_MS, ORDER_ENV, TIMEOUT_ENV};
use acan_intent::{PathCoverage, PathScope, RepoPath, RepoRules, ScopeEntry};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::{Semaphore, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::timeout;

/// Largest request line, including its newline.
pub const MAX_REQUEST_BYTES: usize = 256 * 1024;
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_CONNECTIONS: usize = 64;
const PROTECTED_DIRS: [&str; 3] = [".git", ".acan", ".factory"];

/// The places and tools an active order may use.
#[derive(Debug, Clone)]
pub struct ActiveOrderPolicy {
    pub order_id: String,
    pub worktree_root: PathBuf,
    pub scope: PathScope,
    pub rules: RepoRules,
    pub allow_shell: bool,
    pub allowed_mcp_tools: BTreeSet<String>,
}

/// The last session start reported for a particular registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heartbeat {
    pub order_id: String,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub at: SystemTime,
}

/// Shared by the listener and the owner of the order lifecycle.
#[derive(Debug, Clone, Default)]
pub struct HookRegistry {
    orders: Arc<RwLock<BTreeMap<String, Registration>>>,
}

#[derive(Debug)]
struct Registration {
    policy: Arc<RegisteredPolicy>,
    heartbeat: watch::Sender<Option<Heartbeat>>,
}

#[derive(Debug)]
struct RegisteredPolicy {
    policy: ActiveOrderPolicy,
    is_probe: bool,
    // Keep the given spelling too: Droid may report an absolute path using it, e.g. on macOS
    // where a worktree under /var has a canonical path under /private/var.
    given_root: PathBuf,
}

impl HookRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Activates one order, with a canonical worktree root and no heartbeat yet.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid or already active id, or a root that is not a directory.
    pub fn register_order(&self, policy: ActiveOrderPolicy) -> io::Result<()> {
        self.register(policy, false)
    }

    fn register(&self, mut policy: ActiveOrderPolicy, is_probe: bool) -> io::Result<()> {
        let id = &policy.order_id;
        if id.is_empty() || id.trim() != id || id.chars().any(char::is_control) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid order id",
            ));
        }
        let given_root = std::path::absolute(&policy.worktree_root)?;
        policy.worktree_root = fs::canonicalize(&given_root)?;
        if !fs::metadata(&policy.worktree_root)?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "worktree is not a directory",
            ));
        }
        let mut orders = self.orders.write().expect("hook registry lock");
        if orders.contains_key(&policy.order_id) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "order is already active",
            ));
        }
        orders.insert(
            policy.order_id.clone(),
            Registration {
                policy: Arc::new(RegisteredPolicy {
                    policy,
                    is_probe,
                    given_root,
                }),
                heartbeat: watch::Sender::new(None),
            },
        );
        Ok(())
    }

    /// Removes the policy and its heartbeat, and wakes anyone still waiting for one.
    pub fn unregister_order(&self, order_id: &str) {
        self.orders
            .write()
            .expect("hook registry lock")
            .remove(order_id);
    }

    #[must_use]
    pub fn last_heartbeat(&self, order_id: &str) -> Option<Heartbeat> {
        self.orders
            .read()
            .expect("hook registry lock")
            .get(order_id)?
            .heartbeat
            .borrow()
            .clone()
    }

    /// Waits for the first heartbeat of the current registration, or returns one already seen.
    ///
    /// # Errors
    ///
    /// Returns an error when the order is not registered, is removed, or does not start in time.
    pub async fn wait_for_heartbeat(
        &self,
        order_id: &str,
        limit: Duration,
    ) -> Result<Heartbeat, HookProbeError> {
        let mut rx = self
            .orders
            .read()
            .expect("hook registry lock")
            .get(order_id)
            .ok_or(HookProbeError::UnknownOrder)?
            .heartbeat
            .subscribe();
        let seen = timeout(limit, rx.wait_for(Option::is_some))
            .await
            .map_err(|_| HookProbeError::Timeout)?
            .map_err(|_| HookProbeError::UnknownOrder)?;
        Ok(seen.clone().expect("heartbeat seen"))
    }

    fn decide(&self, request: HookRequest) -> Result<(), String> {
        let policy = {
            let orders = self.orders.read().expect("hook registry lock");
            let registration = orders.get(&request.order).ok_or("unknown order")?;
            if request.kind == RequestKind::SessionStart {
                if request.tool_name.is_some()
                    || request.file_path.is_some()
                    || request.command.is_some()
                    || request.raw_tool_input.is_some()
                {
                    return Err("SessionStart carries tool fields".to_owned());
                }
                registration.heartbeat.send_replace(Some(Heartbeat {
                    order_id: request.order,
                    session_id: request.session_id,
                    cwd: request.cwd,
                    at: SystemTime::now(),
                }));
                return Ok(());
            }
            Arc::clone(&registration.policy)
        };

        let writes = policy.check_tool(&request)?;
        // Filesystem checks need no registry lock. Before allowing, recheck that this is still
        // the same registration and consult the other orders as they stand now.
        let orders = self.orders.read().expect("hook registry lock");
        if !orders
            .get(&request.order)
            .is_some_and(|r| Arc::ptr_eq(&policy, &r.policy))
        {
            return Err("unknown order".to_owned());
        }
        // Probe registrations verify the hook path, not a real order's claims on the repo.
        // The probe should never authorize a write, even though it has a broad synthetic scope.
        if policy.is_probe {
            return if writes.is_empty() {
                Ok(())
            } else {
                Err("probe orders cannot write".to_owned())
            };
        }
        for path in writes {
            let target = PathScope::new([ScopeEntry::File {
                path: path.clone(),
                new: false,
            }])
            .expect("one valid path makes a scope");
            for (id, other) in &*orders {
                if id == &request.order || other.policy.is_probe {
                    continue;
                }
                let other = &other.policy.policy;
                if other.scope.coverage(&path, &other.rules) == PathCoverage::Covered
                    || !other.scope.overlaps(&target).is_empty()
                {
                    return Err(format!("{path} overlaps active order {id}"));
                }
            }
        }
        Ok(())
    }
}

impl RegisteredPolicy {
    /// Checks one call and returns the repo-relative names of all its write targets.
    fn check_tool(&self, request: &HookRequest) -> Result<BTreeSet<RepoPath>, String> {
        let name = request
            .tool_name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .ok_or("missing tool_name")?;
        let input = request
            .raw_tool_input
            .as_ref()
            .and_then(Value::as_object)
            .ok_or("raw_tool_input must be an object")?;
        let mut writes = BTreeSet::new();
        match name {
            "Read" | "Grep" | "Glob" | "LS" | "View" => {
                let mut paths = request
                    .file_path
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>();
                for key in ["file_path", "path", "directory_path", "folder"] {
                    if let Some(value) = input.get(key).filter(|v| !v.is_null()) {
                        paths.push(value.as_str().ok_or("invalid read path")?);
                    }
                }
                if !paths.is_empty() {
                    let cwd = self.cwd(request.cwd.as_deref())?;
                    for path in paths {
                        let (lexical, resolved) = self.resolve(&cwd, path)?;
                        for path in [&lexical, &resolved] {
                            let relative = path
                                .strip_prefix(&self.policy.worktree_root)
                                .expect("resolved inside worktree");
                            if is_token_file(relative) {
                                return Err("reading .acan/tokens.json is not allowed".to_owned());
                            }
                        }
                    }
                }
            }
            "WebSearch" | "FetchUrl" => {}
            "Edit" | "Create" | "Write" | "MultiEdit" | "Delete" | "Patch" | "ApplyPatch"
            | "NotebookEdit" | "Rename" | "Move" => {
                let cwd = self.mutating_cwd(request.cwd.as_deref())?;
                let paths = write_paths(name, input)?;
                for path in paths.into_iter().chain(request.file_path.as_deref()) {
                    let (lexical, resolved) = self.resolve(&cwd, path)?;
                    if resolved.is_dir() {
                        return Err("a write target must be a file, not a directory".to_owned());
                    }
                    for path in [&lexical, &resolved] {
                        let path = self.repo_file(path)?;
                        if path.as_str().split('/').any(is_protected_dir) {
                            return Err(format!("{path}: protected path"));
                        }
                        let coverage = self.policy.scope.coverage(&path, &self.policy.rules);
                        if coverage != PathCoverage::Covered {
                            return Err(format!("{path}: {coverage}"));
                        }
                        writes.insert(path);
                    }
                }
            }
            "Execute" | "Bash" | "Shell" | "Terminal" | "RunCommand" => {
                if !self.policy.allow_shell {
                    return Err("shell tools are not allowed for this order".to_owned());
                }
                const ALLOWED_SHELL_KEYS: &[&str] = &[
                    "command",
                    "description",
                    "timeout",
                    "summary",
                    "riskLevel",
                    "risk_level",
                ];
                for key in input.keys() {
                    if !ALLOWED_SHELL_KEYS.contains(&key.as_str()) {
                        return Err(format!("{name}: unrecognized input key {key:?}"));
                    }
                }
                self.mutating_cwd(request.cwd.as_deref())?;
                let raw_command = input
                    .get("command")
                    .map(|v| v.as_str().ok_or("invalid command"))
                    .transpose()?;
                if matches!((request.command.as_deref(), raw_command), (Some(a), Some(b)) if a != b)
                {
                    return Err("conflicting command fields".to_owned());
                }
                let command = request
                    .command
                    .as_deref()
                    .or(raw_command)
                    .filter(|value| !value.trim().is_empty())
                    .ok_or("missing command")?;
                check_shell(command)?;
            }
            name if self.policy.allowed_mcp_tools.contains(name)
                || name
                    .strip_prefix("mcp__acan__")
                    .or_else(|| name.strip_prefix("acan___"))
                    .is_some_and(|name| self.policy.allowed_mcp_tools.contains(name)) => {}
            _ => return Err(format!("tool {name:?} is not allowed")),
        }
        Ok(writes)
    }

    fn mutating_cwd(&self, cwd: Option<&str>) -> Result<PathBuf, String> {
        let cwd = cwd.ok_or("missing cwd for a mutating tool")?;
        if !Path::new(cwd).is_absolute() {
            return Err("invalid cwd: a mutating tool needs an absolute cwd".to_owned());
        }
        self.cwd(Some(cwd))
    }

    fn cwd(&self, cwd: Option<&str>) -> Result<PathBuf, String> {
        let root = &self.policy.worktree_root;
        if !fs::canonicalize(root).is_ok_and(|resolved| resolved == *root && resolved.is_dir()) {
            return Err(
                "worktree root is no longer an existing directory at its registered path"
                    .to_owned(),
            );
        }
        let Some(cwd) = cwd else {
            return Ok(PathBuf::new());
        };
        let (lexical, resolved) = self
            .resolve(Path::new(""), cwd)
            .map_err(|err| format!("invalid cwd: {err}"))?;
        if !resolved.is_dir() {
            return Err("invalid cwd: not an existing directory".to_owned());
        }
        Ok(lexical
            .strip_prefix(&self.policy.worktree_root)
            .expect("cwd inside worktree")
            .to_owned())
    }

    /// Names a target before and after following links, so neither spelling may hide a
    /// protected path or change the scope that applies.
    fn resolve(&self, cwd: &Path, raw: &str) -> Result<(PathBuf, PathBuf), String> {
        use std::path::Component;

        let root = &self.policy.worktree_root;
        if raw.trim().is_empty() || raw.chars().any(char::is_control) {
            return Err("path is blank or contains a control character".to_owned());
        }
        if raw.split(['/', '\\']).any(|part| part == "..") {
            return Err("path contains a .. segment".to_owned());
        }
        if cfg!(unix) && raw.contains('\\') {
            return Err("path contains a non-portable separator".to_owned());
        }
        let path = Path::new(raw);
        let absolute = path.is_absolute();
        let relative = if absolute {
            if let Some(relative) = strip_worktree(path, root) {
                relative
            } else if let Some(relative) = strip_worktree(path, &self.given_root)
                && fs::canonicalize(&self.given_root).is_ok_and(|given| given == *root)
            {
                relative
            } else {
                return Err("absolute path is outside the worktree".to_owned());
            }
        } else {
            path
        };
        // Validate the supplied segments before handing them to the OS (notably Windows
        // can silently trim trailing dots and spaces or treat a colon as a drive).
        let mut checked = if absolute {
            PathBuf::new()
        } else {
            cwd.to_owned()
        };
        for part in relative.components() {
            match part {
                Component::Normal(part) => {
                    if cfg!(windows) {
                        part.to_str()
                            .ok_or("path is not UTF-8")?
                            .parse::<RepoPath>()
                            .map_err(|err| err.to_string())?;
                    }
                    checked.push(part);
                }
                Component::CurDir => {}
                _ => return Err("path has an ambiguous root".to_owned()),
            }
        }
        let relative = checked;
        let lexical = root.join(&relative);
        let mut resolved = root.clone();
        let mut missing = false;
        for part in relative.components() {
            resolved.push(part.as_os_str());
            if missing {
                continue;
            }
            match fs::symlink_metadata(&resolved) {
                Ok(_) => {
                    resolved =
                        fs::canonicalize(&resolved).map_err(|_| "cannot resolve a path or link")?;
                    if !resolved.starts_with(root) {
                        return Err("path resolves outside the worktree".to_owned());
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => missing = true,
                Err(_) => return Err("cannot inspect path".to_owned()),
            }
        }
        Ok((lexical, resolved))
    }

    fn repo_file(&self, path: &Path) -> Result<RepoPath, String> {
        path.strip_prefix(&self.policy.worktree_root)
            .expect("path inside worktree")
            .to_str()
            .ok_or("path is not UTF-8")?
            .replace(std::path::MAIN_SEPARATOR, "/")
            .parse::<RepoPath>()
            .map_err(|err| err.to_string())
    }
}

fn strip_worktree<'a>(path: &'a Path, root: &Path) -> Option<&'a Path> {
    #[cfg(not(windows))]
    return path.strip_prefix(root).ok();

    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};

        let mut path = path.components();
        for part in root.components() {
            match (path.next()?, part) {
                (Component::Prefix(a), Component::Prefix(b)) => match (a.kind(), b.kind()) {
                    (
                        Prefix::Disk(a) | Prefix::VerbatimDisk(a),
                        Prefix::Disk(b) | Prefix::VerbatimDisk(b),
                    ) if a.eq_ignore_ascii_case(&b) => {}
                    (
                        Prefix::UNC(a, b) | Prefix::VerbatimUNC(a, b),
                        Prefix::UNC(c, d) | Prefix::VerbatimUNC(c, d),
                    ) if a.eq_ignore_ascii_case(c) && b.eq_ignore_ascii_case(d) => {}
                    _ => return None,
                },
                (a, b) if a == b => {}
                _ => return None,
            }
        }
        Some(path.as_path())
    }
}

fn is_token_file(path: &Path) -> bool {
    use std::ffi::OsStr;

    let mut parts = path.iter().peekable();
    while let Some(part) = parts.next() {
        if part.eq_ignore_ascii_case(OsStr::new(".acan"))
            && parts
                .peek()
                .is_some_and(|name| name.eq_ignore_ascii_case(OsStr::new("tokens.json")))
        {
            return true;
        }
    }
    false
}

fn is_protected_dir(part: &str) -> bool {
    PROTECTED_DIRS
        .iter()
        .any(|dir| part.eq_ignore_ascii_case(dir))
}

fn write_paths<'a>(tool: &str, input: &'a Map<String, Value>) -> Result<Vec<&'a str>, String> {
    let allowed: &[&str] = match tool {
        "Edit" => &[
            "file_path",
            "path",
            "old_str",
            "new_str",
            "change_all",
            "old_string",
            "new_string",
            "replace_all",
        ],
        "Create" | "Write" => &["file_path", "path", "content"],
        "MultiEdit" => &["file_path", "path", "paths", "edits"],
        "Delete" => &["file_path", "path"],
        // A free-form patch can name additional paths. Only structured targets can be checked.
        "Patch" | "ApplyPatch" => &["file_path", "path", "target_file", "paths"],
        "NotebookEdit" => &[
            "file_path",
            "path",
            "notebook_path",
            "cell_id",
            "new_source",
            "cell_type",
            "edit_mode",
        ],
        "Rename" | "Move" => &["file_path", "path", "old_path", "new_path"],
        _ => unreachable!("write tool without an input schema"),
    };
    let mut paths = Vec::new();
    collect_write_paths(input, allowed, &mut paths).map_err(|err| {
        if matches!(tool, "Patch" | "ApplyPatch") {
            format!("cannot verify the targets of a free-form patch: {err}")
        } else {
            format!("{tool}: {err}")
        }
    })?;
    if paths.is_empty() {
        return Err("no write target path".to_owned());
    }
    Ok(paths)
}

fn collect_write_paths<'a>(
    input: &'a Map<String, Value>,
    allowed: &[&str],
    paths: &mut Vec<&'a str>,
) -> Result<(), String> {
    for (key, value) in input {
        if !allowed.contains(&key.as_str()) {
            return Err(format!("unrecognized input key {key:?}"));
        }
        match key.as_str() {
            "file_path" | "path" | "old_path" | "new_path" | "target_file" | "notebook_path" => {
                paths.push(
                    value
                        .as_str()
                        .ok_or_else(|| format!("{key} is not a path string"))?,
                );
            }
            "paths" => {
                for value in value.as_array().ok_or("paths is not an array")? {
                    paths.push(value.as_str().ok_or("paths contains a non-string target")?);
                }
            }
            "edits" => {
                for edit in value.as_array().ok_or("edits is not an array")? {
                    collect_write_paths(
                        edit.as_object().ok_or("edits contains a non-object")?,
                        &[
                            "file_path",
                            "path",
                            "target_file",
                            "old_str",
                            "new_str",
                            "change_all",
                            "old_string",
                            "new_string",
                            "replace_all",
                        ],
                        paths,
                    )?;
                }
            }
            "change_all" | "replace_all" if !value.is_boolean() => {
                return Err(format!("{key} is not a boolean"));
            }
            "change_all" | "replace_all" => {}
            _ if !value.is_string() => return Err(format!("{key} is not a string")),
            _ => {}
        }
    }
    Ok(())
}

/// Checks literal shell words without executing them. Separators expose chained commands;
/// dynamic substitutions and other syntax that would hide words are refused.
fn check_shell(command: &str) -> Result<(), String> {
    let (words, has_redirection_or_grouping) = shell_words(command)?;
    for (index, word) in words.iter().enumerate() {
        if word
            .split(|ch: char| {
                ch.is_whitespace() || matches!(ch, '/' | '\\' | '=' | ':' | '"' | '\'')
            })
            .any(|part| {
                is_protected_dir(part)
                    || part.eq_ignore_ascii_case(ORDER_ENV)
                    || part.eq_ignore_ascii_case(ENDPOINT_ENV)
            })
        {
            return Err("command references protected state or hook environment".to_owned());
        }
        let bin = word
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(word)
            .trim_start_matches('@');
        let lower = bin.to_ascii_lowercase();
        let bin = lower
            .strip_suffix(".exe")
            .or_else(|| lower.strip_suffix(".cmd"))
            .or_else(|| lower.strip_suffix(".bat"))
            .unwrap_or(&lower);
        if matches!(
            bin,
            "sh" | "bash"
                | "dash"
                | "zsh"
                | "fish"
                | "ksh"
                | "cmd"
                | "pwsh"
                | "powershell"
                | "eval"
                | "source"
                | "."
        ) {
            return Err("nested shell commands cannot be checked".to_owned());
        }
        if bin == "git" {
            // Redirections may appear between argv tokens, or apply to an entire group.
            // Do not let them hide later Git options or direct Git's output to a file.
            if has_redirection_or_grouping {
                return Err("git commands cannot use redirection or grouping".to_owned());
            }
            if words.iter().any(|word| {
                word.split_once('=').is_some_and(|(name, value)| {
                    let upper = name.trim().to_ascii_uppercase();
                    upper.starts_with("GIT_") && !(upper == "GIT_OPTIONAL_LOCKS" && value == "0")
                })
            }) {
                return Err("git environment overrides are not allowed".to_owned());
            }
            let cmd_start = words[..index]
                .iter()
                .rposition(String::is_empty)
                .map_or(0, |sep| sep + 1);
            let prefix = &words[cmd_start..index];
            let assignments = match prefix.first().map(String::as_str) {
                Some("env") => &prefix[1..],
                _ => prefix,
            };
            let env_no_optional_locks = !assignments.is_empty()
                && assignments.iter().all(|word| {
                    word.split_once('=')
                        .is_some_and(|(name, _)| !name.is_empty() && !name.starts_with('-'))
                })
                && assignments.iter().any(|word| {
                    word.split_once('=').is_some_and(|(name, value)| {
                        let matches_name = if cfg!(windows) {
                            name.eq_ignore_ascii_case("GIT_OPTIONAL_LOCKS")
                        } else {
                            name == "GIT_OPTIONAL_LOCKS"
                        };
                        matches_name && value == "0"
                    })
                });
            check_git(&words[index + 1..], env_no_optional_locks)?;
        }
    }
    Ok(())
}

fn check_git(words: &[String], env_no_optional_locks: bool) -> Result<(), String> {
    const DENIED_LONG: &[&str] = &[
        "--output",
        "--config-env",
        "--git-dir",
        "--work-tree",
        "--exec-path",
        "--ext-diff",
        "--textconv",
        "--filters",
        "--open-files-in-pager",
        "--help",
    ];
    let args: Vec<&str> = words
        .iter()
        .map(String::as_str)
        .take_while(|arg| !arg.is_empty())
        .collect();
    let mut subcommand_idx = None;
    let mut has_no_optional_locks = env_no_optional_locks;
    for (idx, &arg) in args.iter().enumerate() {
        if arg == "--no-optional-locks" {
            has_no_optional_locks = true;
            continue;
        }
        if arg == "--no-pager" {
            continue;
        }
        subcommand_idx = Some((idx, arg));
        break;
    }
    let subcommand = subcommand_idx.map(|(_, cmd)| cmd);
    let sub_args = subcommand_idx.map_or(&[][..], |(idx, _)| &args[idx + 1..]);
    for &arg in &args {
        let flag = arg.split_once('=').map_or(arg, |(flag, _)| flag);
        // Git's parse-options accepts unambiguous long prefixes and bundled short options.
        let denied = if flag.starts_with("--") {
            flag.len() > 2 && DENIED_LONG.iter().any(|name| name.starts_with(flag))
        } else {
            flag.strip_prefix('-').is_some_and(|options| {
                options.contains(['o', 'c', 'C'])
                    || (subcommand == Some("grep") && options.contains('O'))
            })
        };
        if denied {
            return Err(format!("git argument {arg:?} is not allowed"));
        }
    }
    if !matches!(
        subcommand,
        Some(
            "status"
                | "diff"
                | "log"
                | "show"
                | "rev-parse"
                | "ls-files"
                | "grep"
                | "describe"
                | "shortlog"
                | "blame"
                | "cat-file"
                | "ls-tree"
                | "merge-base"
                | "rev-list"
                | "--version"
        )
    ) {
        return Err("only read-only git commands are allowed".to_owned());
    }
    if matches!(subcommand, Some("status" | "diff")) && !has_no_optional_locks {
        return Err(format!(
            "git {} requires --no-optional-locks or GIT_OPTIONAL_LOCKS=0 to stay read-only",
            subcommand.unwrap_or_default()
        ));
    }
    if matches!(subcommand, Some("diff" | "log" | "show" | "blame")) {
        let has_leading_readonly_flags = matches!(
            sub_args.get(..2),
            Some(["--no-ext-diff", "--no-textconv"] | ["--no-textconv", "--no-ext-diff"])
        );
        if !has_leading_readonly_flags {
            return Err(format!(
                "git {} requires leading --no-ext-diff and --no-textconv to stay read-only",
                subcommand.unwrap_or_default()
            ));
        }
    }
    Ok(())
}

fn shell_words(command: &str) -> Result<(Vec<String>, bool), String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut has_redirection_or_grouping = false;
    let mut chars = command.chars().peekable();
    while let Some(ch) = chars.next() {
        let control = ch.is_control() && !matches!(ch, '\n' | '\t');
        let unsupported_space = ch.is_whitespace() && !matches!(ch, ' ' | '\t' | '\n');
        if control || unsupported_space || (cfg!(windows) && matches!(ch, '%' | '!' | '^')) {
            return Err("command contains unsupported shell syntax".to_owned());
        }
        if quote == Some(ch) {
            quote = None;
        } else if quote == Some('\'') {
            word.push(ch);
        } else if cfg!(unix)
            && ch == '\\'
            && (quote.is_none()
                || chars
                    .peek()
                    .is_some_and(|c| matches!(c, '\\' | '"' | '$' | '`' | '\n')))
        {
            let escaped = chars.next().ok_or("unfinished shell escape")?;
            if escaped != '\n' {
                if escaped.is_control() {
                    return Err("command contains a control character".to_owned());
                }
                word.push(escaped);
            }
        } else if matches!(ch, '$' | '`') {
            return Err("shell expansions and substitutions cannot be checked".to_owned());
        } else if quote.is_some() {
            word.push(ch);
        } else if ch == '"' || (cfg!(unix) && ch == '\'') {
            quote = Some(ch);
        } else if matches!(
            ch,
            ' ' | '\t' | '\n' | ';' | '&' | '|' | '<' | '>' | '(' | ')'
        ) {
            has_redirection_or_grouping |= matches!(ch, '<' | '>' | '(' | ')');
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
            if matches!(ch, '\n' | ';' | '&' | '|' | '<' | '>' | '(' | ')') {
                words.push(String::new());
            }
        } else if matches!(ch, '*' | '?' | '[' | ']' | '{' | '}' | '~' | '#') {
            return Err("command contains unsupported shell syntax".to_owned());
        } else {
            word.push(ch);
        }
    }
    if quote.is_some() {
        return Err("unfinished shell quote".to_owned());
    }
    if !word.is_empty() {
        words.push(word);
    }
    if words.iter().all(String::is_empty) {
        return Err("missing command".to_owned());
    }
    Ok((words, has_redirection_or_grouping))
}

/// A bound hook listener. Dropping it stops the listener; [`Self::shutdown`] drains it first.
#[derive(Debug)]
pub struct HookServer {
    endpoint: String,
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<io::Result<()>>>,
    #[cfg(unix)]
    socket: Option<SocketFile>,
}

impl HookServer {
    /// Listens on a diagnostic TCP endpoint, only on IPv4 loopback. Port 0 picks a free port.
    pub async fn bind_loopback(port: u16, registry: HookRegistry) -> io::Result<Self> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
        let endpoint = format!("tcp://{}", listener.local_addr()?);
        Ok(Self::start(endpoint, Listener::Tcp(listener), registry))
    }

    /// Listens at an absolute Unix socket path. Never replaces a regular file, a symlink or
    /// a live listener. The parent directory must be private to the current user.
    #[cfg(unix)]
    pub async fn bind_unix(path: impl AsRef<Path>, registry: HookRegistry) -> io::Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        use tokio::net::{UnixListener, UnixStream};

        let path = path.as_ref();
        let _ = local_endpoint(path)?;
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "socket needs a file name",
            ));
        };
        let (parent_path, parent) = private_socket_parent(parent)?;
        let path = parent_path.join(name);
        let endpoint = local_endpoint(&path)?;
        let parent = Arc::new(parent);
        match SocketFile::inspect(Arc::clone(&parent), name) {
            Ok(existing) => {
                verify_socket_parent(&parent_path, &parent)?;
                match timeout(CONNECTION_TIMEOUT, UnixStream::connect(&path)).await {
                    Ok(Err(err)) if err.kind() == io::ErrorKind::ConnectionRefused => {
                        existing.remove_if_same()?;
                    }
                    Ok(Err(err)) => return Err(err),
                    _ => return Err(io::Error::new(io::ErrorKind::AddrInUse, "socket is in use")),
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        verify_socket_parent(&parent_path, &parent)?;
        let listener = UnixListener::bind(&path)?;
        let socket = SocketFile::inspect(Arc::clone(&parent), name)?;
        let secured = verify_socket_parent(&parent_path, &parent)
            .and_then(|()| fs::set_permissions(&path, fs::Permissions::from_mode(0o600)));
        if let Err(err) = secured {
            let _ = socket.remove_if_same();
            return Err(err);
        }
        let mut server = Self::start(endpoint, Listener::Unix(listener), registry);
        server.socket = Some(socket);
        Ok(server)
    }

    /// Listens on a fresh, randomly named local Windows pipe. Prefer this over a fixed name:
    /// the first instance rejects preexisting servers, and all instances reject remote clients,
    /// but the name itself is not authentication for local clients.
    #[cfg(windows)]
    pub fn bind_default_named_pipe(registry: HookRegistry) -> io::Result<Self> {
        let path = format!(r"\\.\pipe\acan-hook-{}", uuid::Uuid::new_v4());
        Self::bind_named_pipe(path, registry)
    }

    /// Listens on an explicitly named local Windows pipe. Fails if that name is already in use
    /// and rejects remote clients.
    #[cfg(windows)]
    pub fn bind_named_pipe(path: impl AsRef<Path>, registry: HookRegistry) -> io::Result<Self> {
        let endpoint = local_endpoint(path.as_ref())?;
        let pending = [
            pipe_instance(&endpoint, true)?,
            pipe_instance(&endpoint, false)?,
        ];
        Ok(Self::start(
            endpoint.clone(),
            Listener::Pipe { endpoint, pending },
            registry,
        ))
    }

    fn start(endpoint: String, listener: Listener, registry: HookRegistry) -> Self {
        let (stop, stopped) = watch::channel(false);
        Self {
            endpoint,
            stop,
            task: Some(tokio::spawn(serve(listener, registry, stopped))),
            #[cfg(unix)]
            socket: None,
        }
    }

    /// The exact value to set in `ACAN_HOOK_ENDPOINT`.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Stops accepting connections, waits for accepted exchanges, and removes the socket.
    pub async fn shutdown(mut self) -> io::Result<()> {
        self.stop.send_replace(true);
        let served = self
            .task
            .as_mut()
            .expect("hook listener task")
            .await
            .map_err(io::Error::other)?;
        self.task = None;
        #[cfg(unix)]
        if let Some(socket) = &self.socket {
            socket.remove_if_same()?;
            self.socket = None;
        }
        served
    }
}

impl Drop for HookServer {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
        #[cfg(unix)]
        if let Some(socket) = &self.socket {
            let _ = socket.remove_if_same();
        }
    }
}

#[cfg(any(unix, windows))]
fn local_endpoint(path: &Path) -> io::Result<String> {
    let endpoint = path
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "endpoint is not UTF-8"))?;
    if endpoint.starts_with("tcp://") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a local path",
        ));
    }
    Endpoint::parse(endpoint).map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
    Ok(endpoint.to_owned())
}

#[cfg(unix)]
fn private_socket_parent(path: &Path) -> io::Result<(PathBuf, fs::File)> {
    use rustix::fs::{Mode, OFlags};
    use std::os::unix::fs::DirBuilderExt;

    // Remove trailing separators and `.` so symlink_metadata will not dereference a final link.
    let path: PathBuf = path.components().collect();
    if let Err(err) = fs::symlink_metadata(&path) {
        if err.kind() != io::ErrorKind::NotFound {
            return Err(err);
        }
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&path)?;
    }
    // Inspect before opening (including after create), and compare to the opened directory.
    socket_parent_metadata(&path)?;
    let dir = fs::File::from(rustix::fs::open(
        &path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    verify_socket_parent(&path, &dir)?;
    let canonical = fs::canonicalize(&path)?;
    verify_socket_parent(&canonical, &dir)?;
    Ok((canonical, dir))
}

#[cfg(unix)]
fn socket_parent_metadata(path: &Path) -> io::Result<fs::Metadata> {
    use std::os::unix::fs::MetadataExt;

    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir()
        || meta.mode() & 0o077 != 0
        || meta.uid() != rustix::process::geteuid().as_raw()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "socket parent must be a real directory owned by the current user and private to them",
        ));
    }
    Ok(meta)
}

#[cfg(unix)]
fn verify_socket_parent(path: &Path, dir: &fs::File) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let meta = socket_parent_metadata(path)?;
    let opened = dir.metadata()?;
    if (meta.dev(), meta.ino()) != (opened.dev(), opened.ino()) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "socket parent directory changed",
        ));
    }
    Ok(())
}

#[cfg(unix)]
#[derive(Debug)]
struct SocketFile {
    parent: Arc<fs::File>,
    name: std::ffi::OsString,
    stat: rustix::fs::Stat,
}

#[cfg(unix)]
impl SocketFile {
    fn inspect(parent: Arc<fs::File>, name: &std::ffi::OsStr) -> io::Result<Self> {
        use rustix::fs::{AtFlags, FileType, statat};

        let stat = statat(&*parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::Socket {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "path is not a socket",
            ));
        }
        Ok(Self {
            parent,
            name: name.to_owned(),
            stat,
        })
    }

    fn remove_if_same(&self) -> io::Result<()> {
        use rustix::fs::{AtFlags, FileType, statat, unlinkat};

        // Use the opened parent, not a pathname that could be redirected after bind.
        match statat(&*self.parent, &self.name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat)
                if FileType::from_raw_mode(stat.st_mode) == FileType::Socket
                    && (stat.st_dev, stat.st_ino) == (self.stat.st_dev, self.stat.st_ino) =>
            {
                unlinkat(&*self.parent, &self.name, AtFlags::empty()).map_err(io::Error::from)
            }
            Ok(_) => Ok(()),
            Err(rustix::io::Errno::NOENT) => Ok(()),
            Err(err) => Err(err.into()),
        }
    }
}

#[cfg(windows)]
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

#[cfg(windows)]
fn pipe_instance(endpoint: &str, first: bool) -> io::Result<NamedPipeServer> {
    ServerOptions::new()
        .first_pipe_instance(first)
        .reject_remote_clients(true)
        .create(endpoint)
}

enum Listener {
    Tcp(TcpListener),
    #[cfg(unix)]
    Unix(tokio::net::UnixListener),
    #[cfg(windows)]
    Pipe {
        endpoint: String,
        pending: [NamedPipeServer; 2],
    },
}

trait Connection: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Connection for T {}

impl Listener {
    async fn accept(&mut self) -> io::Result<Box<dyn Connection>> {
        match self {
            Self::Tcp(listener) => Ok(Box::new(listener.accept().await?.0)),
            #[cfg(unix)]
            Self::Unix(listener) => Ok(Box::new(listener.accept().await?.0)),
            #[cfg(windows)]
            Self::Pipe { endpoint, pending } => {
                let [first, spare] = pending;
                let index = tokio::select! {
                    result = first.connect() => { result?; 0 }
                    result = spare.connect() => { result?; 1 }
                };
                // Keep an instance available while the accepted client is being served.
                Ok(Box::new(std::mem::replace(
                    &mut pending[index],
                    pipe_instance(endpoint, false)?,
                )))
            }
        }
    }
}

async fn serve(
    mut listener: Listener,
    registry: HookRegistry,
    mut stopped: watch::Receiver<bool>,
) -> io::Result<()> {
    let mut tasks = JoinSet::new();
    let policy_slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let result = loop {
        tokio::select! {
            biased;
            _ = stopped.wait_for(|stop| *stop) => break Ok(()),
            _ = tasks.join_next(), if !tasks.is_empty() => {}
            stream = listener.accept(), if tasks.len() < MAX_CONNECTIONS => match stream {
                Ok(stream) => {
                    tasks.spawn(exchange(stream, registry.clone(), Arc::clone(&policy_slots)));
                }
                Err(err) => break Err(err),
            },
        }
    };
    drop(listener);
    while tasks.join_next().await.is_some() {}
    result
}

async fn check_policy(
    policy_slots: Arc<Semaphore>,
    limit: Duration,
    decide: impl FnOnce() -> Result<(), String> + Send + 'static,
) -> Result<(), String> {
    let permit = policy_slots
        .try_acquire_owned()
        .map_err(|_| "too many outstanding hook policy checks".to_owned())?;
    timeout(
        limit,
        tokio::task::spawn_blocking(move || {
            // A timed-out caller cannot cancel running filesystem I/O. Keep its slot
            // reserved until the blocking check actually exits.
            let _permit = permit;
            decide()
        }),
    )
    .await
    .map_err(|_| "hook policy check timed out".to_owned())?
    .map_err(|_| "hook policy check failed".to_owned())?
}

async fn exchange(
    mut stream: Box<dyn Connection>,
    registry: HookRegistry,
    policy_slots: Arc<Semaphore>,
) {
    let mut line = Vec::new();
    let read = timeout(CONNECTION_TIMEOUT, async {
        BufReader::new((&mut stream).take(MAX_REQUEST_BYTES as u64 + 1))
            .read_until(b'\n', &mut line)
            .await
    })
    .await;
    let oversized = line.len() > MAX_REQUEST_BYTES;
    let unread_tail = oversized && !line.ends_with(b"\n");
    let request = match read {
        Ok(Ok(_)) if oversized => Err("request line is too large".to_owned()),
        Ok(Ok(_)) => std::str::from_utf8(&line)
            .map_err(|_| "request is not UTF-8".to_owned())
            .and_then(|line| {
                acan_hook::protocol::parse_request(line).map_err(|err| err.to_string())
            }),
        Ok(Err(_)) => Err("cannot read request".to_owned()),
        Err(_) => Err("request read timed out".to_owned()),
    };
    let verdict = match request {
        Ok(request) => {
            check_policy(policy_slots, CONNECTION_TIMEOUT, move || {
                registry.decide(request)
            })
            .await
        }
        Err(reason) => Err(reason),
    };
    let reply = match verdict {
        Ok(()) => HookResponse {
            version: PROTOCOL_VERSION,
            decision: Decision::Allow,
            reason: None,
        },
        Err(reason) => HookResponse {
            version: PROTOCOL_VERSION,
            decision: Decision::Deny,
            reason: Some(
                reason
                    .chars()
                    .take(1024)
                    .map(|ch| {
                        if ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}') {
                            ' '
                        } else {
                            ch
                        }
                    })
                    .collect(),
            ),
        },
    };
    let mut line = serde_json::to_vec(&reply).expect("hook reply is serializable");
    line.push(b'\n');
    let _ = timeout(CONNECTION_TIMEOUT, async {
        stream.write_all(&line).await?;
        // A client that writes the whole line before reading needs to finish that write.
        // On TCP, closing while its data is still queued could also reset the reply. Discard
        // the tail without buffering it or waiting past the connection deadline.
        if unread_tail {
            let mut buf = [0; 8192];
            loop {
                let read = stream.read(&mut buf).await?;
                if read == 0 || buf[..read].contains(&b'\n') {
                    break;
                }
            }
        }
        stream.shutdown().await
    })
    .await;
}

/// Proof that the installed hook can report a session and block a forbidden write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookProbeReport {
    pub heartbeat: Heartbeat,
    pub session_start_exit_code: i32,
    pub pre_tool_use_exit_code: i32,
}

/// Why first-run verification did not prove the hook is working.
#[derive(Debug, thiserror::Error)]
pub enum HookProbeError {
    #[error("unknown order")]
    UnknownOrder,
    #[error("hook probe timed out")]
    Timeout,
    #[error("cannot register hook probe: {0}")]
    Registration(#[source] io::Error),
    #[error("cannot run hook probe for {event}: {source}")]
    Process {
        event: &'static str,
        source: io::Error,
    },
    #[error("hook probe for {event} exited with {actual:?}, expected {expected}")]
    Exit {
        event: &'static str,
        expected: i32,
        actual: Option<i32>,
    },
    #[error("hook probe received a different SessionStart heartbeat")]
    BadHeartbeat,
    #[error("hook probe did not receive the expected daemon denial")]
    MissingDenial,
}

/// A temporary order that checks the same path from executable to daemon as a Droid hook.
#[derive(Debug)]
pub struct FirstRunHookProbe {
    registry: HookRegistry,
    order_id: String,
    worktree: PathBuf,
}

impl FirstRunHookProbe {
    /// Registers a fresh probe order. Dropping the probe removes it, including on failure.
    pub fn new(registry: HookRegistry, worktree: impl AsRef<Path>) -> Result<Self, HookProbeError> {
        let order_id = format!("hook-probe-{}", uuid::Uuid::new_v4());
        let worktree = std::path::absolute(worktree).map_err(HookProbeError::Registration)?;
        let scope = PathScope::new([ScopeEntry::RepoRoot { tests: false }])
            .expect("a repo root makes a scope");
        registry
            .register(
                ActiveOrderPolicy {
                    order_id: order_id.clone(),
                    worktree_root: worktree.clone(),
                    scope,
                    rules: RepoRules::default(),
                    allow_shell: false,
                    allowed_mcp_tools: BTreeSet::new(),
                },
                true,
            )
            .map_err(HookProbeError::Registration)?;
        Ok(Self {
            registry,
            order_id,
            worktree,
        })
    }

    #[must_use]
    pub fn order_id(&self) -> &str {
        &self.order_id
    }

    /// Runs the executable for `SessionStart` and one forbidden `PreToolUse` within `limit`.
    pub async fn run(
        self,
        hook_bin: &Path,
        endpoint: &str,
        limit: Duration,
    ) -> Result<HookProbeReport, HookProbeError> {
        timeout(limit, self.check(hook_bin, endpoint, limit))
            .await
            .map_err(|_| HookProbeError::Timeout)?
    }

    async fn check(
        &self,
        hook_bin: &Path,
        endpoint: &str,
        limit: Duration,
    ) -> Result<HookProbeReport, HookProbeError> {
        let cwd = self.worktree.to_string_lossy();
        self.invoke(
            hook_bin,
            endpoint,
            "SessionStart",
            0,
            json!({
                "hook_event_name": "SessionStart", "session_id": self.order_id, "cwd": cwd,
            }),
        )
        .await?;
        let heartbeat = self
            .registry
            .wait_for_heartbeat(&self.order_id, limit)
            .await?;
        if heartbeat.session_id.as_deref() != Some(&self.order_id)
            || heartbeat.cwd.as_deref() != Some(&cwd)
        {
            return Err(HookProbeError::BadHeartbeat);
        }
        let stderr = self
            .invoke(
                hook_bin,
                endpoint,
                "PreToolUse",
                BLOCK_EXIT_CODE,
                json!({
                    "hook_event_name": "PreToolUse", "session_id": self.order_id, "cwd": cwd,
                    "tool_name": "Edit", "tool_input": { "file_path": ".acan/world.db" },
                }),
            )
            .await?;
        // The hook also exits 2 for an unreachable daemon or a malformed reply. Confirm that
        // it received our policy decision, not merely that it failed closed.
        if !String::from_utf8_lossy(&stderr)
            .lines()
            .any(|line| line == "acan hook: blocked: .acan/world.db: protected path")
        {
            return Err(HookProbeError::MissingDenial);
        }
        Ok(HookProbeReport {
            heartbeat,
            session_start_exit_code: 0,
            pre_tool_use_exit_code: BLOCK_EXIT_CODE,
        })
    }

    async fn invoke(
        &self,
        hook_bin: &Path,
        endpoint: &str,
        event: &'static str,
        expected: i32,
        input: Value,
    ) -> Result<Vec<u8>, HookProbeError> {
        let failed = |source| HookProbeError::Process { event, source };
        let mut child = Command::new(hook_bin)
            .current_dir(&self.worktree)
            .env(ORDER_ENV, &self.order_id)
            .env(ENDPOINT_ENV, endpoint)
            .env(TIMEOUT_ENV, MAX_TIMEOUT_MS.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(failed)?;
        let mut stdin = child.stdin.take().expect("probe stdin");
        stdin
            .write_all(input.to_string().as_bytes())
            .await
            .map_err(failed)?;
        drop(stdin);
        let mut stderr_pipe = child.stderr.take().expect("probe stderr").take(4096);
        let mut stderr = Vec::new();
        stderr_pipe.read_to_end(&mut stderr).await.map_err(failed)?;
        drop(stderr_pipe);
        let actual = child.wait().await.map_err(failed)?.code();
        if actual != Some(expected) {
            return Err(HookProbeError::Exit {
                event,
                expected,
                actual,
            });
        }
        Ok(stderr)
    }
}

impl Drop for FirstRunHookProbe {
    fn drop(&mut self) {
        self.registry.unregister_order(&self.order_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn timed_out_policy_check_occupies_slot_until_it_exits() {
        let slots = Arc::new(Semaphore::new(1));
        let (release, blocking) = std::sync::mpsc::channel();
        let verdict = check_policy(Arc::clone(&slots), Duration::ZERO, move || {
            let _ = blocking.recv();
            Ok(())
        })
        .await;
        assert_eq!(verdict.unwrap_err(), "hook policy check timed out");
        assert_eq!(
            check_policy(Arc::clone(&slots), Duration::ZERO, || unreachable!())
                .await
                .unwrap_err(),
            "too many outstanding hook policy checks"
        );
        release.send(()).expect("unblock policy check");
        let _permit = timeout(Duration::from_secs(2), slots.acquire())
            .await
            .expect("blocking check releases its slot")
            .expect("semaphore is open");
    }
}
