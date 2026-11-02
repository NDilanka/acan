//! `acan init`: the protected app contract of a repository, detected from its committed files.
//!
//! Detection reads text and JSON only. It never runs the app, bun, node, npm or an install, and
//! anything it cannot read is a refusal, never a guess that gets written. Each generated file is
//! decoded by the acan-intent types, and the decoded value must encode back to the same bytes, so
//! what is written is what a receipt reads.

use std::collections::{BTreeMap, BTreeSet};

use acan_intent::{AppContract, PLACEHOLDER_PREFIX, ProbeCorpus, is_credential_name};
use serde_json::{Map, Value, json};
use thiserror::Error;

/// The protected files `acan init` writes, relative to the repository root.
pub const APP_PATH: &str = ".acan/app.json";
/// The starter probe corpus.
pub const CORPUS_PATH: &str = ".acan/probes/core.json";
/// Ignore rules for runtime state next to the protected files.
pub const GITIGNORE_PATH: &str = ".acan/.gitignore";
/// The review note: every generated value, where it came from, and what a human must check.
pub const REVIEW_PATH: &str = ".acan/REVIEW.md";

/// The four files, in the order they are written.
pub const FILES: [&str; 4] = [APP_PATH, CORPUS_PATH, GITIGNORE_PATH, REVIEW_PATH];

/// The Bun version the pinned bun image runs.
const BUN_VERSION: Version = (1, 4, 4);
/// The Node version in the pinned `node:24.22.2-bookworm-slim` image.
const NODE_VERSION: Version = (24, 22, 2);
/// A semantic version as (major, minor, patch).
type Version = (u64, u64, u64);

/// The install phase of a Node app with a lockfile. Install scripts never run.
const NODE_INSTALL: [&str; 5] = ["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"];
/// Routes that name the ready check, first match wins.
const READY_ROUTES: [&str; 4] = ["/health", "/healthz", "/api/health", "/status"];
/// Starter probes beyond the ready check.
const EXTRA_PROBES: usize = 3;
/// How long the runner waits for the ready check.
const READY_TIMEOUT_MS: u64 = 10_000;
/// The deadline of each starter request.
const DEADLINE_MS: u64 = 2_000;
/// The client name every starter request uses.
const CLIENT: &str = "client-a";
/// Source files larger than this are not read.
const MAX_SOURCE_BYTES: usize = 512 * 1024;
/// Source bytes read in total for one detection.
const MAX_SOURCE_TOTAL: usize = 16 * 1024 * 1024;
/// Names the policy sets itself. An app cannot set them, so detection never suggests them.
const POLICY_NAMES: [&str; 3] = ["HOME", "PATH", "PORT"];
/// Fragments of names that carry a host, a database or a certificate. A suggestion never sets one.
const SENSITIVE_NAME_PARTS: [&str; 6] = ["URL", "DSN", "HOST", "DATABASE", "CERT", "PRIVATE"];
/// Directories whose files are never read.
const SKIPPED_DIRS: [&str; 8] = [
    "node_modules",
    ".git",
    ".acan",
    "dist",
    "build",
    "out",
    "coverage",
    "target",
];
/// Directories and file names that hold tests, which detection does not read.
const TEST_DIRS: [&str; 3] = ["test", "tests", "__tests__"];

/// Why detection stopped, or why the generated files failed their own checks.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum InitError {
    /// Detection cannot pick one value, or the repository is not one acan runs. Nothing is written.
    #[error("init refused: {0}")]
    Refused(String),
    /// A generated file failed its own checks. This is a bug in acan, and nothing is written.
    #[error("init could not generate a valid contract: {0}")]
    Internal(String),
}

/// Read access to the files of one committed tree. Paths use `/` and are relative to its root.
pub trait Snapshot {
    /// Every file path in the tree, sorted.
    fn paths(&self) -> Vec<String>;
    /// The bytes of one file, or `None` when it is absent.
    fn read(&self, path: &str) -> Option<Vec<u8>>;
}

/// The answers a human can give when detection cannot pick a value alone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// The workspace package to boot, by its directory, when several packages have a start script.
    pub package: Option<String>,
    /// The ready route, when no known route is found. Must start with `/`.
    pub ready_path: Option<String>,
}

/// One value the plan sets, with the file it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The contract field, or the environment variable, the value belongs to.
    pub field: String,
    /// The value as written.
    pub value: String,
    /// The repository file the value was read from, or the rule that chose it.
    pub source: String,
    /// True when a human must check the value before the contract is merged.
    pub review: bool,
}

/// One generated file: its repository path and its bytes as they will be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedFile {
    /// The path, relative to the repository root.
    pub path: &'static str,
    /// The file's bytes as text.
    pub contents: String,
}

/// What `acan init` will write. Building it touches nothing in the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The four generated files, in [`FILES`] order.
    pub files: Vec<GeneratedFile>,
    /// Every detected value, in the order the review note lists them.
    pub findings: Vec<Finding>,
    /// Facts a reviewer must know that are not values.
    pub notes: Vec<String>,
}

impl Plan {
    /// The bytes of one generated file.
    #[must_use]
    pub fn file(&self, path: &str) -> Option<&str> {
        self.files
            .iter()
            .find(|file| file.path == path)
            .map(|file| file.contents.as_str())
    }
}

/// Detects the contract of the repository behind `snapshot` and generates its four files.
///
/// # Errors
///
/// Returns [`InitError::Refused`] when a value cannot be picked alone, when the repository uses
/// a runtime or a package manager acan does not run, or when no start script, PORT read or ready
/// route is found. Returns [`InitError::Internal`] when a generated file fails its own checks.
pub fn detect(snapshot: &dyn Snapshot, options: &Options) -> Result<Plan, InitError> {
    let root = package(snapshot, "")?;
    let runtime = runtime_of(snapshot, &root)?;
    let members = members(snapshot, &root)?;
    let app = select_app(&root, &members, options)?;
    let mut sheet = Sheet::default();

    let npm_lock = snapshot.read("package-lock.json").is_some();
    let all: Vec<&Package> = std::iter::once(&root).chain(members.iter()).collect();
    let dependencies: Vec<String> = all
        .iter()
        .filter(|package| has_dependencies(&package.json))
        .map(|package| package.label())
        .collect();
    match runtime {
        Runtime::Bun if !dependencies.is_empty() => {
            return Err(refused(format!(
                "runtime dependencies in {}: bun apps have no install phase, so only a Node app \
                 with a package-lock.json can have them",
                dependencies.join(", ")
            )));
        }
        Runtime::Node if !dependencies.is_empty() && !npm_lock => {
            return Err(refused(format!(
                "runtime dependencies in {} but no package-lock.json for npm ci to install from",
                dependencies.join(", ")
            )));
        }
        _ => {}
    }

    let app_label = app.label();
    let runtime_id = engine_check(&mut sheet, &root, runtime)?;
    sheet.found(
        "runtime",
        runtime_id,
        "package.json engines and lockfiles",
        false,
    );
    sheet.found(
        "package_manager",
        match runtime {
            Runtime::Bun => "bun",
            Runtime::Node => "npm",
        },
        "lockfile",
        false,
    );

    let install = match (runtime, npm_lock) {
        (Runtime::Node, true) => {
            let argv: Vec<String> = NODE_INSTALL.iter().map(|arg| (*arg).to_owned()).collect();
            sheet.found(
                "install",
                NODE_INSTALL.join(" "),
                "package-lock.json",
                false,
            );
            Some(argv)
        }
        _ => None,
    };
    let boot = boot_argv(runtime, &app);
    sheet.found(
        "boot",
        boot.join(" "),
        &format!("{app_label} scripts.start"),
        false,
    );

    let sources = app_sources(snapshot, &app.dir);
    let reads = env_reads(&sources);
    let port_env = port_env(&reads, &app)?;
    sheet.found("port_env", &port_env, reads[&port_env].as_str(), false);

    let routes = scan_routes(&sources);
    let ready = ready_path(&mut sheet, &routes, options)?;
    sheet.found(
        "ready",
        format!("GET {ready} expects 200"),
        "route in app source",
        true,
    );
    sheet.found("store", "fresh_per_sequence", "fixed", false);
    sheet.found("clock", "none", "fixed in v1", false);
    sheet.found("sinks", "[]", "fixed in v1", false);

    let env = starter_env(snapshot, &root, &app, &reads, &port_env, &mut sheet);
    let mut routes = routes;
    routes.insert(("GET".to_owned(), ready.clone()));
    let probes = starter_probes(&routes, &ready);
    sheet.found(
        "routes",
        format!(
            "{} detected, {} with a starter probe",
            routes.len(),
            probes.len()
        ),
        "route literals in app source",
        true,
    );
    install_script_notes(&mut sheet, &all, runtime);
    sheet.notes.push(
        "Clock and randomness are not controlled in v1. Base runs that print times, ids or \
         random values will disagree until a human adds maskable paths or a seeded harness."
            .to_owned(),
    );
    sheet.notes.push(
        "Routes and probes are static guesses. Coverage means what the starter corpus \
         exercises, not what the app does."
            .to_owned(),
    );
    sheet.notes.push(
        "Detection never runs the app, so the ready path and the port are checked only when a \
         receipt runs."
            .to_owned(),
    );

    let app_value = app_value(runtime, install.as_deref(), &boot, &port_env, &ready, &env);
    let corpus_value = corpus_value(&routes, &probes);
    let app_file = encode_app(app_value)?;
    let corpus_file = encode_corpus(corpus_value)?;
    let gitignore = GITIGNORE.to_owned();
    let mut plan = Plan {
        files: vec![
            GeneratedFile {
                path: APP_PATH,
                contents: app_file,
            },
            GeneratedFile {
                path: CORPUS_PATH,
                contents: corpus_file,
            },
            GeneratedFile {
                path: GITIGNORE_PATH,
                contents: gitignore,
            },
        ],
        findings: sheet.findings,
        notes: sheet.notes,
    };
    let review = review_text(&plan.findings, &plan.notes);
    plan.files.push(GeneratedFile {
        path: REVIEW_PATH,
        contents: review,
    });
    verify(&plan)?;
    Ok(plan)
}

/// The runtime a repository's markers name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Runtime {
    Bun,
    Node,
}

/// A package.json, with the directory it lives in. The root package has an empty directory.
#[derive(Debug, Clone)]
struct Package {
    dir: String,
    json: Map<String, Value>,
}

impl Package {
    /// The directory, or `the repository root` for the root package.
    fn label(&self) -> String {
        if self.dir.is_empty() {
            "the root package".to_owned()
        } else {
            self.dir.clone()
        }
    }

    fn name(&self) -> Option<&str> {
        self.json.get("name").and_then(Value::as_str)
    }

    fn script(&self, name: &str) -> Option<&str> {
        self.json
            .get("scripts")
            .and_then(Value::as_object)
            .and_then(|scripts| scripts.get(name))
            .and_then(Value::as_str)
    }
}

/// Values and notes collected while detection runs.
#[derive(Debug, Default)]
struct Sheet {
    findings: Vec<Finding>,
    notes: Vec<String>,
}

impl Sheet {
    fn found(
        &mut self,
        field: impl Into<String>,
        value: impl Into<String>,
        source: &str,
        review: bool,
    ) {
        self.findings.push(Finding {
            field: field.into(),
            value: value.into(),
            source: source.to_owned(),
            review,
        });
    }
}

/// The starter `.acan/.gitignore`: the demo's runtime-state rules.
const GITIGNORE: &str = "# Runtime state: stores, run output and tokens. The protected inputs next to this file stay tracked.\n*.db\n*.db-shm\n*.db-wal\ntokens.json\nstores/\nruns/\nobservations/\n";

fn refused(message: impl Into<String>) -> InitError {
    InitError::Refused(message.into())
}

/// Reads and parses the package.json of a directory. A missing root file is a refusal; a missing
/// member file is handled by the caller.
fn package(snapshot: &dyn Snapshot, dir: &str) -> Result<Package, InitError> {
    let path = join(dir, "package.json");
    let bytes = snapshot
        .read(&path)
        .ok_or_else(|| refused(format!("no {path} in the repository")))?;
    let json: Value = serde_json::from_slice(&bytes)
        .map_err(|err| refused(format!("{path} is not JSON: {err}")))?;
    match json {
        Value::Object(json) => Ok(Package {
            dir: dir.to_owned(),
            json,
        }),
        _ => Err(refused(format!("{path} is not a JSON object"))),
    }
}

fn join(dir: &str, file: &str) -> String {
    if dir.is_empty() {
        file.to_owned()
    } else {
        format!("{dir}/{file}")
    }
}

/// The runtime the markers name, or a refusal when they name none or both.
fn runtime_of(snapshot: &dyn Snapshot, root: &Package) -> Result<Runtime, InitError> {
    let bun_lock = snapshot.read("bun.lock").is_some() || snapshot.read("bun.lockb").is_some();
    let npm_lock = snapshot.read("package-lock.json").is_some();
    for (file, manager) in [("pnpm-lock.yaml", "pnpm"), ("yarn.lock", "yarn")] {
        if snapshot.read(file).is_some() {
            return Err(refused(format!(
                "{file} names {manager}, which acan does not run; use npm (package-lock.json) or bun"
            )));
        }
    }
    let bun_marker = bun_lock || engines(root, "bun").is_some();
    let node_marker = npm_lock || engines(root, "node").is_some();
    match (bun_marker, node_marker) {
        (true, true) => Err(refused(
            "both bun and Node markers are present (a bun lockfile, engines.bun, a package-lock.json \
             or engines.node); keep one runtime",
        )),
        (true, false) => Ok(Runtime::Bun),
        (false, true) => Ok(Runtime::Node),
        (false, false) => Err(refused(
            "no runtime marker: commit package-lock.json for Node, or bun.lock for bun",
        )),
    }
}

/// The engines range for a runtime, as written.
fn engines(package: &Package, runtime: &str) -> Option<String> {
    package
        .json
        .get("engines")
        .and_then(Value::as_object)
        .and_then(|engines| engines.get(runtime))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// The workspace packages named by the root's `workspaces`, empty when it names none.
fn members(snapshot: &dyn Snapshot, root: &Package) -> Result<Vec<Package>, InitError> {
    let patterns = match root.json.get("workspaces") {
        None => return Ok(Vec::new()),
        Some(Value::Array(items)) => string_items(items)?,
        Some(Value::Object(object)) => match object.get("packages") {
            Some(Value::Array(items)) => string_items(items)?,
            _ => return Err(refused("workspaces has no packages list")),
        },
        Some(_) => return Err(refused("workspaces is neither a list nor an object")),
    };
    let paths = snapshot.paths();
    let mut dirs = BTreeSet::new();
    for pattern in &patterns {
        let pattern = pattern
            .strip_prefix("./")
            .unwrap_or(pattern)
            .trim_end_matches('/');
        if let Some(parent) = pattern.strip_suffix("/*") {
            if parent.contains('*') || parent.is_empty() {
                return Err(refused(format!(
                    "workspace pattern {pattern:?} is not supported"
                )));
            }
            let prefix = format!("{parent}/");
            for path in &paths {
                let Some(rest) = path.strip_prefix(&prefix) else {
                    continue;
                };
                let child = rest.strip_suffix("/package.json");
                if let Some(child) = child.filter(|child| !child.contains('/') && !child.is_empty())
                {
                    dirs.insert(format!("{parent}/{child}"));
                }
            }
        } else if pattern.contains('*') || pattern.is_empty() || pattern.starts_with('!') {
            return Err(refused(format!(
                "workspace pattern {pattern:?} is not supported"
            )));
        } else if snapshot.read(&format!("{pattern}/package.json")).is_some() {
            dirs.insert(pattern.to_owned());
        }
    }
    if dirs.is_empty() {
        return Err(refused("workspaces match no package.json"));
    }
    dirs.iter().map(|dir| package(snapshot, dir)).collect()
}

fn string_items(items: &[Value]) -> Result<Vec<String>, InitError> {
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| refused("workspaces lists something that is not a string"))
        })
        .collect()
}

/// The package the app boots from: the root, or the one workspace package with a start script.
fn select_app(
    root: &Package,
    members: &[Package],
    options: &Options,
) -> Result<Package, InitError> {
    if members.is_empty() {
        if options.package.is_some() {
            return Err(refused(
                "--package names a workspace, but the repository has none",
            ));
        }
        if root.script("start").is_none() {
            return Err(refused(
                "the root package.json has no scripts.start to boot",
            ));
        }
        return Ok(root.clone());
    }
    let starts: Vec<&Package> = members
        .iter()
        .filter(|package| package.script("start").is_some())
        .collect();
    if let Some(dir) = &options.package {
        let dir = dir.trim_end_matches('/');
        return starts
            .into_iter()
            .find(|package| package.dir == dir)
            .cloned()
            .ok_or_else(|| {
                refused(format!(
                    "--package {dir} is not a workspace package with a start script"
                ))
            });
    }
    match starts.as_slice() {
        [] => Err(refused("no workspace package has a scripts.start to boot")),
        [only] => Ok((*only).clone()),
        several => Err(refused(format!(
            "several workspace packages have a start script ({}); pass --package <dir> to choose",
            several
                .iter()
                .map(|package| package.dir.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

fn has_dependencies(json: &Map<String, Value>) -> bool {
    json.get("dependencies")
        .and_then(Value::as_object)
        .is_some_and(|deps| !deps.is_empty())
}

/// Checks the engines range of the runtime against the pinned version. Returns the runtime id.
fn engine_check(
    sheet: &mut Sheet,
    root: &Package,
    runtime: Runtime,
) -> Result<&'static str, InitError> {
    let (name, id, pinned) = match runtime {
        Runtime::Bun => ("bun", acan_intent::BUN_RUNTIME, BUN_VERSION),
        Runtime::Node => ("node", acan_intent::NODE_RUNTIME, NODE_VERSION),
    };
    match engines(root, name) {
        None => sheet.notes.push(format!(
            "engines.{name} is not set; the pinned runtime {id} runs the app"
        )),
        Some(range) => match admits(&range, pinned) {
            Some(true) => {}
            Some(false) => {
                return Err(refused(format!(
                    "engines.{name} is {range:?}, which does not admit the pinned {id} version"
                )));
            }
            None => {
                return Err(refused(format!(
                    "engines.{name} is {range:?}, which acan cannot read; set a plain range"
                )));
            }
        },
    }
    Ok(id)
}

/// The boot argv for the runtime and the app package.
fn boot_argv(runtime: Runtime, app: &Package) -> Vec<String> {
    let start = |argv: &[&str]| {
        argv.iter()
            .map(|arg| (*arg).to_owned())
            .collect::<Vec<String>>()
    };
    match (runtime, app.dir.is_empty()) {
        (Runtime::Bun, true) => start(&["bun", "run", "start"]),
        (Runtime::Bun, false) => {
            let mut argv = start(&["bun", "run", "--cwd"]);
            argv.push(app.dir.clone());
            argv.push("start".to_owned());
            argv
        }
        (Runtime::Node, true) => start(&["npm", "run", "start"]),
        (Runtime::Node, false) => {
            let mut argv = start(&["npm", "run", "--workspace"]);
            argv.push(app.name().unwrap_or(&app.dir).to_owned());
            argv.push("start".to_owned());
            argv
        }
    }
}

/// The source files of the app package, skipping dependencies, build output and tests.
fn app_sources(snapshot: &dyn Snapshot, dir: &str) -> Vec<(String, String)> {
    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    };
    let mut sources = Vec::new();
    let mut total = 0;
    for path in snapshot.paths() {
        if !path.starts_with(&prefix) || skipped(&path) || !is_source(&path) {
            continue;
        }
        let Some(bytes) = snapshot.read(&path) else {
            continue;
        };
        if bytes.len() > MAX_SOURCE_BYTES || total + bytes.len() > MAX_SOURCE_TOTAL {
            continue;
        }
        total += bytes.len();
        sources.push((path, String::from_utf8_lossy(&bytes).into_owned()));
    }
    sources
}

fn skipped(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    let file = parts.last().copied().unwrap_or_default();
    parts
        .iter()
        .any(|part| SKIPPED_DIRS.contains(part) || TEST_DIRS.contains(part))
        || file.contains(".test.")
        || file.contains(".spec.")
}

fn is_source(path: &str) -> bool {
    [".js", ".mjs", ".cjs", ".ts", ".mts", ".cts", ".jsx", ".tsx"]
        .iter()
        .any(|ext| path.ends_with(ext))
}

/// Environment names the sources read, each with the first file that reads it.
fn env_reads(sources: &[(String, String)]) -> BTreeMap<String, String> {
    let mut reads = BTreeMap::new();
    for (path, text) in sources {
        for (at, _) in text.match_indices("process.env") {
            let rest = &text[at + "process.env".len()..];
            let name = if let Some(after) = rest.strip_prefix('.') {
                identifier(after)
            } else {
                rest.trim_start()
                    .strip_prefix('[')
                    .map(str::trim_start)
                    .and_then(|after| after.strip_prefix(['"', '\'']))
                    .and_then(identifier)
            };
            if let Some(name) = name {
                reads.entry(name.to_owned()).or_insert_with(|| path.clone());
            }
        }
    }
    reads
}

/// The leading identifier of `text`, made of letters, digits and underscores.
fn identifier(text: &str) -> Option<&str> {
    let end = text
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(text.len());
    let name = &text[..end];
    (!name.is_empty() && !name.starts_with(|c: char| c.is_ascii_digit())).then_some(name)
}

/// The variable that carries the port: `PORT`, which the sources must read.
fn port_env(reads: &BTreeMap<String, String>, app: &Package) -> Result<String, InitError> {
    if reads.contains_key("PORT") {
        return Ok("PORT".to_owned());
    }
    Err(refused(format!(
        "no process.env.PORT read in {}; acan sets the port through PORT, so the app must read it",
        app.label()
    )))
}

/// Literal routes the sources name, as (method, path). Parameters stay as `:name` templates.
fn scan_routes(sources: &[(String, String)]) -> BTreeSet<(String, String)> {
    let mut routes = BTreeSet::new();
    for (_, text) in sources {
        for method in ["get", "post", "put", "patch", "delete"] {
            let needle = format!(".{method}(");
            for (at, _) in text.match_indices(&needle) {
                if let Some(path) = quoted_route(&text[at + needle.len()..]) {
                    routes.insert((method.to_ascii_uppercase(), path));
                }
            }
        }
        for (at, _) in text.match_indices("pathname") {
            let rest = text[at + "pathname".len()..].trim_start();
            let rest = rest
                .strip_prefix("===")
                .or_else(|| rest.strip_prefix("=="))
                .and_then(quoted_route);
            if let Some(path) = rest {
                routes.insert(("GET".to_owned(), path));
            }
        }
    }
    routes
}

/// A quoted route literal at the start of `text`: a path with no query, template or space.
fn quoted_route(text: &str) -> Option<String> {
    let text = text.trim_start();
    let quote = text.chars().next()?;
    if !matches!(quote, '\'' | '"' | '`') {
        return None;
    }
    let body = &text[1..];
    let path = &body[..body.find(quote)?];
    let plain = path
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"/-_.:".contains(&byte));
    (plain && path.starts_with('/') && path.len() <= 200 && !path.contains("//"))
        .then(|| path.to_owned())
}

/// The ready route: the first known one the app names, or the one the human gave.
fn ready_path(
    sheet: &mut Sheet,
    routes: &BTreeSet<(String, String)>,
    options: &Options,
) -> Result<String, InitError> {
    if let Some(path) = &options.ready_path {
        if !path.starts_with('/') {
            return Err(refused("--ready-path must start with /"));
        }
        return Ok(path.clone());
    }
    for candidate in READY_ROUTES {
        if routes.contains(&("GET".to_owned(), candidate.to_owned())) {
            sheet.notes.push(format!(
                "ready check {candidate} is a GET route the sources name"
            ));
            return Ok(candidate.to_owned());
        }
    }
    Err(refused(format!(
        "no GET ready route found ({}); pass --ready-path <route> for a GET route that answers 200",
        READY_ROUTES.join(", ")
    )))
}

/// The starter environment. Every entry is flagged for review.
///
/// - A credential name the sources read, or `.env.example` names, gets a placeholder. The value
///   in `.env.example` is never copied.
/// - A name `.env.example` lists with no value gets a placeholder too, since the app expects one.
/// - An assignment in the README or a package script is a suggestion, with its value, when the
///   value is a short plain token and the name carries no host, database or certificate.
fn starter_env(
    snapshot: &dyn Snapshot,
    root: &Package,
    app: &Package,
    reads: &BTreeMap<String, String>,
    port_env: &str,
    sheet: &mut Sheet,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for (name, source) in reads {
        if valid_name(name) && is_credential_name(name) {
            env.insert(name.clone(), placeholder(name));
            sheet.found(
                name,
                placeholder(name),
                &format!("placeholder; {source} reads it"),
                true,
            );
        }
    }
    for file in [".env.example".to_owned(), join(&app.dir, ".env.example")] {
        let Some(bytes) = snapshot.read(&file) else {
            continue;
        };
        for line in String::from_utf8_lossy(&bytes).lines() {
            let Some((name, value)) = line.split_once('=') else {
                continue;
            };
            let name = name.trim();
            if !valid_name(name) || env.contains_key(name) || port_env == name {
                continue;
            }
            if is_credential_name(name) || value.trim().is_empty() {
                env.insert(name.to_owned(), placeholder(name));
                sheet.found(
                    name,
                    placeholder(name),
                    &format!("placeholder; {file} lists the name without a value"),
                    true,
                );
            }
        }
    }
    let mut evidence: Vec<(String, String, String)> = Vec::new();
    for file in ["README.md", "readme.md"] {
        if let Some(bytes) = snapshot.read(file) {
            for (name, value) in assignments(&String::from_utf8_lossy(&bytes)) {
                evidence.push((name, value, format!("{file}; suggested, check it")));
            }
            break;
        }
    }
    for package in [root, app] {
        for (script, text) in package
            .json
            .get("scripts")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            for (name, value) in text.as_str().map(assignments).unwrap_or_default() {
                evidence.push((
                    name,
                    value,
                    format!("{} scripts.{script}; suggested, check it", package.label()),
                ));
            }
        }
    }
    for (name, value, source) in evidence {
        if !valid_name(&name)
            || POLICY_NAMES.contains(&name.as_str())
            || name == port_env
            || is_credential_name(&name)
            || SENSITIVE_NAME_PARTS.iter().any(|part| name.contains(part))
            || env.contains_key(&name)
        {
            continue;
        }
        sheet.found(name.clone(), value.clone(), &source, true);
        env.insert(name, value);
    }
    env
}

/// A placeholder for a credential name: the prefix, then the name in lower case with hyphens.
fn placeholder(name: &str) -> String {
    let tail: String = name
        .to_ascii_lowercase()
        .replace('_', "-")
        .chars()
        .take(48)
        .collect();
    format!("{PLACEHOLDER_PREFIX}{}", tail.trim_end_matches('-'))
}

/// An environment name acan accepts: uppercase letters, digits and underscores, from a letter.
fn valid_name(name: &str) -> bool {
    name.len() <= 64
        && name.starts_with(|c: char| c.is_ascii_uppercase())
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// `NAME=value` assignments in a piece of text, where the value is a short plain token.
fn assignments(text: &str) -> Vec<(String, String)> {
    text.split(|c: char| c.is_whitespace() || "\"'`;&|()".contains(c))
        .filter_map(|token| {
            let (name, value) = token.split_once('=')?;
            let plain = !value.is_empty()
                && value.len() <= 32
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte));
            (plain && valid_name(name)).then(|| (name.to_owned(), value.to_owned()))
        })
        .collect()
}

/// Notes for install scripts. They never run, so a package that needs one fails at run time.
fn install_script_notes(sheet: &mut Sheet, all: &[&Package], runtime: Runtime) {
    if runtime != Runtime::Node {
        return;
    }
    for package in all {
        for script in ["preinstall", "install", "postinstall"] {
            if package.script(script).is_some() {
                sheet.notes.push(format!(
                    "{} has scripts.{script}; install scripts are not run (--ignore-scripts), so \
                     any step it performs is skipped",
                    package.label()
                ));
            }
        }
    }
}

fn app_value(
    runtime: Runtime,
    install: Option<&[String]>,
    boot: &[String],
    port_env: &str,
    ready: &str,
    env: &BTreeMap<String, String>,
) -> Value {
    let (version, id) = match runtime {
        Runtime::Bun => ("acan-app/1", acan_intent::BUN_RUNTIME),
        Runtime::Node => ("acan-app/2", acan_intent::NODE_RUNTIME),
    };
    let mut value = json!({
        "version": version,
        "runner_contract": "acan-runner/1",
        "runtime": id,
        "boot": boot,
        "port_env": port_env,
        "ready": { "path": ready, "status": 200, "timeout_ms": READY_TIMEOUT_MS },
        "store": "fresh_per_sequence",
        "clock": { "kind": "none" },
        "sinks": [],
        "env": env,
        "limits": {
            "max_requests": 64,
            "max_output_bytes": 65536,
            "sequence_timeout_ms": 30000
        }
    });
    if let Some(install) = install {
        value["install"] = json!(install);
    }
    value
}

/// A starter probe: one GET of a route, against the empty fixture.
fn probe(label: &str, path: &str) -> Value {
    json!({
        "label": label,
        "spec": {
            "surface": "http",
            "fixture": "f-empty",
            "steps": [{
                "step": "http",
                "id": "s1",
                "client": CLIENT,
                "request": { "method": "GET", "path": path },
                "redirect": "manual",
                "deadline_ms": DEADLINE_MS
            }]
        },
        "origin": "human",
        "route_path": { "machines": ["server"], "source": "static" },
        "normalization": {
            "version": "norm/1",
            "typed_captures": [],
            "projections": [],
            "relations": [],
            "maskable_paths": []
        }
    })
}

/// The starter probes: the ready check, then up to [`EXTRA_PROBES`] literal GET routes that take no
/// parameter. Each label says it needs review.
fn starter_probes(routes: &BTreeSet<(String, String)>, ready: &str) -> Vec<(String, String)> {
    let mut probes = vec![("health (needs review)".to_owned(), ready.to_owned())];
    for (method, path) in routes {
        if probes.len() > EXTRA_PROBES {
            break;
        }
        if method == "GET" && !path.contains(':') && path != ready {
            probes.push((format!("GET {path} (needs review)"), path.clone()));
        }
    }
    probes
}

fn corpus_value(routes: &BTreeSet<(String, String)>, probes: &[(String, String)]) -> Value {
    let probed: BTreeSet<(&str, &str)> = probes
        .iter()
        .map(|(_, path)| ("GET", path.as_str()))
        .collect();
    let route_entries: Vec<Value> = routes
        .iter()
        .map(|(method, path)| {
            let status = if probed.contains(&(method.as_str(), path.as_str())) {
                json!({ "status": "covered" })
            } else {
                json!({ "status": "uncovered", "reason": "not in the starter corpus" })
            };
            json!({
                "method": method,
                "route_template": path,
                "route_path": { "machines": ["server"], "source": "static" },
                "status": status
            })
        })
        .collect();
    json!({
        "version": "acan-probes/1",
        "fixtures": [{
            "id": "f-empty",
            "recipe": [],
            "isolation": "fresh_instance",
            "runner_contract": "acan-runner/1"
        }],
        "probes": probes.iter().map(|(label, path)| probe(label, path)).collect::<Vec<_>>(),
        "routes": route_entries
    })
}

/// Decodes the app contract and encodes it, the bytes written.
fn encode_app(value: Value) -> Result<String, InitError> {
    let app: AppContract =
        serde_json::from_value(value).map_err(|err| internal(err.to_string()))?;
    app.check().map_err(|err| internal(err.to_string()))?;
    encode(&app)
}

/// Decodes the probe corpus and encodes it, the bytes written.
fn encode_corpus(value: Value) -> Result<String, InitError> {
    let corpus: ProbeCorpus =
        serde_json::from_value(value).map_err(|err| internal(err.to_string()))?;
    encode(&corpus)
}

fn encode<T: serde::Serialize>(value: &T) -> Result<String, InitError> {
    let mut text = serde_json::to_string_pretty(value).map_err(|err| internal(err.to_string()))?;
    text.push('\n');
    Ok(text)
}

fn internal(message: String) -> InitError {
    InitError::Internal(message)
}

fn verify(plan: &Plan) -> Result<(), InitError> {
    let app = plan
        .file(APP_PATH)
        .ok_or_else(|| internal("no app.json".to_owned()))?;
    let corpus = plan
        .file(CORPUS_PATH)
        .ok_or_else(|| internal("no probes file".to_owned()))?;
    check_files(app, corpus)
}

/// Checks the two protected files as a receipt reads them. Each decodes with the acan-intent
/// types, encodes back to the same bytes, and every probe names a fixture the corpus has. Call it
/// on the bytes a commit holds, to prove what was written.
///
/// # Errors
///
/// Returns [`InitError::Internal`] naming the first check that fails.
pub fn check_files(app_bytes: &str, corpus_bytes: &str) -> Result<(), InitError> {
    let app: AppContract =
        serde_json::from_str(app_bytes).map_err(|err| internal(err.to_string()))?;
    app.check().map_err(|err| internal(err.to_string()))?;
    if encode(&app)? != app_bytes {
        return Err(internal(
            "app.json does not re-encode to the same bytes".to_owned(),
        ));
    }
    let corpus: ProbeCorpus =
        serde_json::from_str(corpus_bytes).map_err(|err| internal(err.to_string()))?;
    if encode(&corpus)? != corpus_bytes {
        return Err(internal(
            "probes do not re-encode to the same bytes".to_owned(),
        ));
    }
    let fixtures: BTreeSet<&str> = corpus
        .fixtures
        .iter()
        .map(|fixture| fixture.id.as_str())
        .collect();
    for probe in &corpus.probes {
        if !fixtures.contains(probe.spec.fixture.as_str()) {
            return Err(internal(format!(
                "probe {:?} names a fixture the corpus does not have",
                probe.label
            )));
        }
    }
    Ok(())
}

/// Renders a plan for a human: the review note, then the app contract as written.
#[must_use]
pub fn render(plan: &Plan) -> String {
    let review = plan.file(REVIEW_PATH).unwrap_or_default();
    let app = plan.file(APP_PATH).unwrap_or_default();
    format!("{review}\n## {APP_PATH}\n\n{app}")
}

/// The review note. It names every value, its source, and what a human must check. It holds no
/// secret: a placeholder is printed as the placeholder.
fn review_text(findings: &[Finding], notes: &[String]) -> String {
    let mut text = String::from(
        "# Acan app contract: review before you merge\n\n\
         acan init wrote the files in this directory from the repository's committed files. \
         It ran no app, runtime, package manager or install. Check every item marked REVIEW.\n\n\
         ## Values\n\n",
    );
    for finding in findings {
        let mark = if finding.review { " REVIEW" } else { "" };
        text.push_str(&format!(
            "- {}: {} (from {}){mark}\n",
            finding.field, finding.value, finding.source
        ));
    }
    text.push_str("\n## Notes\n\n");
    for note in notes {
        text.push_str(&format!("- {note}\n"));
    }
    text
}

/// Whether a semver-style engines range admits `version`. `None` when the range uses syntax this
/// check does not read, such as hyphen ranges or tags.
#[must_use]
pub fn admits(range: &str, version: Version) -> Option<bool> {
    let mut any = false;
    for alternative in range.split("||") {
        let comparators: Vec<&str> = alternative.split_whitespace().collect();
        if comparators.is_empty() || alternative.contains(" - ") {
            return None;
        }
        let mut all = true;
        for comparator in comparators {
            all &= comparator_admits(comparator, version)?;
        }
        any |= all;
    }
    Some(any)
}

/// One comparator: `>=24`, `<25`, `^22.1`, `~24.22.2`, `24.x` or `*`.
fn comparator_admits(text: &str, version: Version) -> Option<bool> {
    if matches!(text, "*" | "x" | "X") {
        return Some(true);
    }
    let (op, rest) = ["<=", ">=", "<", ">", "=", "^", "~"]
        .iter()
        .find_map(|op| text.strip_prefix(op).map(|rest| (*op, rest)))
        .unwrap_or(("", text));
    let parts: Vec<&str> = rest.trim_start_matches('v').split('.').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let mut given = [None; 3];
    for (slot, part) in given.iter_mut().zip(&parts) {
        *slot = match *part {
            "x" | "X" | "*" => None,
            number => Some(number.parse::<u64>().ok()?),
        };
    }
    // The lower bound, and the first version past the range a partial version names.
    let lower = (given[0]?, given[1].unwrap_or(0), given[2].unwrap_or(0));
    let full = given.iter().all(Option::is_some);
    let past = match (given[0], given[1]) {
        (Some(major), None) => (major + 1, 0, 0),
        (Some(major), Some(minor)) if given[2].is_none() => (major, minor + 1, 0),
        _ => (lower.0, lower.1, lower.2 + 1),
    };
    Some(match op {
        ">=" => version >= lower,
        ">" => {
            if full {
                version > lower
            } else {
                version >= past
            }
        }
        "<" => version < lower,
        "<=" => {
            if full {
                version <= lower
            } else {
                version < past
            }
        }
        "^" => {
            let ceiling = if lower.0 == 0 && given[1].is_some() {
                (0, lower.1 + 1, 0)
            } else {
                (lower.0 + 1, 0, 0)
            };
            version >= lower && version < ceiling
        }
        "~" => {
            let ceiling = if given[1].is_some() {
                (lower.0, lower.1 + 1, 0)
            } else {
                (lower.0 + 1, 0, 0)
            };
            version >= lower && version < ceiling
        }
        _ => {
            if full {
                version == lower
            } else {
                version >= lower && version < past
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tree held in memory, for tests.
    struct Memory(BTreeMap<String, Vec<u8>>);

    impl Memory {
        fn new(files: &[(&str, &str)]) -> Self {
            Self(
                files
                    .iter()
                    .map(|(path, text)| ((*path).to_owned(), text.as_bytes().to_vec()))
                    .collect(),
            )
        }
    }

    impl Snapshot for Memory {
        fn paths(&self) -> Vec<String> {
            self.0.keys().cloned().collect()
        }

        fn read(&self, path: &str) -> Option<Vec<u8>> {
            self.0.get(path).cloned()
        }
    }

    const NODE_ROOT: &str = r#"{
        "name": "shop", "private": true, "type": "module",
        "engines": { "node": ">=24" },
        "workspaces": ["packages/*", "apps/*"],
        "scripts": { "test": "node --test" }
    }"#;

    const API: &str = r#"{
        "name": "@shop/api", "type": "module",
        "dependencies": { "express": "^5.0.0" },
        "scripts": { "start": "ACQUIT_DEV=1 node src/server.ts" }
    }"#;

    const API_SERVER: &str = "import express from 'express';\n\
        const config = { port: Number(process.env.PORT), key: process.env.PAYPAL_CLIENT_SECRET };\n\
        const app = express();\n\
        app.get('/health', (req, res) => res.send('ok'));\n\
        app.get('/api/items', (req, res) => res.json([]));\n\
        app.post('/api/items', (req, res) => res.json({}));\n\
        app.get('/api/items/:id', (req, res) => res.json({}));\n\
        app.listen(config.port);\n";

    fn node_workspace() -> Memory {
        Memory::new(&[
            ("package.json", NODE_ROOT),
            ("package-lock.json", "{}\n"),
            ("packages/core/package.json", r#"{"name":"@shop/core"}"#),
            ("packages/core/index.js", "export const x = 1;\n"),
            ("apps/api/package.json", API),
            ("apps/api/src/server.ts", API_SERVER),
            (
                ".env.example",
                "PAYPAL_CLIENT_ID=\nPAYPAL_CLIENT_SECRET=\nLOG_LEVEL=info\n",
            ),
            (
                "README.md",
                "Run `ACQUIT_DEV=1 npm run start` to use fixtures.\n",
            ),
        ])
    }

    fn file_json(plan: &Plan, path: &str) -> Value {
        serde_json::from_str(plan.file(path).expect("the file is generated")).expect("json")
    }

    #[test]
    fn a_node_workspace_gets_the_node_contract_with_install_and_boot() {
        let plan =
            detect(&node_workspace(), &Options::default()).expect("the workspace is detected");
        let app = file_json(&plan, APP_PATH);
        assert_eq!(app["version"], "acan-app/2");
        assert_eq!(app["runtime"], "node-24");
        assert_eq!(
            app["boot"],
            json!(["npm", "run", "--workspace", "@shop/api", "start"])
        );
        assert_eq!(app["install"], json!(NODE_INSTALL));
        assert_eq!(app["port_env"], "PORT");
        assert_eq!(app["ready"]["path"], "/health");
    }

    #[test]
    fn credential_names_get_placeholders_and_suggestions_are_marked_for_review() {
        let plan = detect(&node_workspace(), &Options::default()).expect("detected");
        let app = file_json(&plan, APP_PATH);
        assert_eq!(
            app["env"]["PAYPAL_CLIENT_SECRET"],
            "acan-placeholder-paypal-client-secret"
        );
        assert_eq!(
            app["env"]["PAYPAL_CLIENT_ID"],
            "acan-placeholder-paypal-client-id"
        );
        assert_eq!(app["env"]["ACQUIT_DEV"], "1");
        assert!(
            app["env"].get("LOG_LEVEL").is_none(),
            "an unsourced name is not suggested"
        );
        let review = plan.file(REVIEW_PATH).expect("the review note");
        assert!(review.contains("PAYPAL_CLIENT_SECRET: acan-placeholder-paypal-client-secret"));
        assert!(review.contains("ACQUIT_DEV: 1 (from README.md; suggested, check it) REVIEW"));
    }

    #[test]
    fn literal_routes_become_probes_and_the_corpus_decodes() {
        let plan = detect(&node_workspace(), &Options::default()).expect("detected");
        let corpus = file_json(&plan, CORPUS_PATH);
        let labels: Vec<&str> = corpus["probes"]
            .as_array()
            .expect("probes")
            .iter()
            .map(|probe| probe["label"].as_str().expect("label"))
            .collect();
        assert_eq!(
            labels,
            ["health (needs review)", "GET /api/items (needs review)"]
        );
        let routes = corpus["routes"].as_array().expect("routes");
        let uncovered = routes
            .iter()
            .filter(|route| route["status"]["status"] == "uncovered")
            .count();
        assert_eq!(routes.len(), 4, "health, get, post and the parameter route");
        assert_eq!(uncovered, 2);
    }

    #[test]
    fn the_bytes_written_decode_and_encode_back_unchanged() {
        let plan = detect(&node_workspace(), &Options::default()).expect("detected");
        for path in [APP_PATH, CORPUS_PATH] {
            assert!(plan.file(path).expect("written").ends_with("}\n"));
        }
        verify(&plan).expect("the plan verifies");
        let again = detect(&node_workspace(), &Options::default()).expect("detected");
        assert_eq!(plan, again, "the same input gives the same bytes");
    }

    #[test]
    fn a_bun_app_without_dependencies_keeps_the_v1_contract() {
        let tree = Memory::new(&[
            (
                "package.json",
                r#"{"name":"demo","scripts":{"start":"bun run server.ts"}}"#,
            ),
            ("bun.lock", "{}"),
            (
                "server.ts",
                "app.get('/health', h); const p = process.env.PORT;\n",
            ),
        ]);
        let plan = detect(&tree, &Options::default()).expect("detected");
        let app = file_json(&plan, APP_PATH);
        assert_eq!(app["version"], "acan-app/1");
        assert_eq!(app["runtime"], "bun-1.4.4");
        assert_eq!(app["boot"], json!(["bun", "run", "start"]));
        assert!(app.get("install").is_none());
    }

    #[test]
    fn a_bun_app_with_runtime_dependencies_is_refused() {
        let tree = Memory::new(&[
            (
                "package.json",
                r#"{"scripts":{"start":"x"},"dependencies":{"hono":"4.0.0"}}"#,
            ),
            ("bun.lock", "{}"),
            ("server.ts", "process.env.PORT; app.get('/health', h);"),
        ]);
        let err = detect(&tree, &Options::default()).expect_err("refused");
        assert!(matches!(err, InitError::Refused(message) if message.contains("bun apps")));
    }

    #[test]
    fn a_node_app_with_a_bun_lockfile_is_refused() {
        let tree = Memory::new(&[
            (
                "package.json",
                r#"{"engines":{"node":">=24"},"scripts":{"start":"x"}}"#,
            ),
            ("bun.lock", "{}"),
            ("package-lock.json", "{}"),
            ("server.js", "process.env.PORT; app.get('/health', h);"),
        ]);
        let err = detect(&tree, &Options::default()).expect_err("refused");
        assert!(matches!(err, InitError::Refused(message) if message.contains("both")));
    }

    #[test]
    fn several_workspace_start_scripts_need_a_package_choice() {
        let tree = Memory::new(&[
            ("package.json", r#"{"workspaces":["apps/*"]}"#),
            ("package-lock.json", "{}"),
            (
                "apps/a/package.json",
                r#"{"name":"a","scripts":{"start":"x"}}"#,
            ),
            (
                "apps/a/server.js",
                "process.env.PORT; app.get('/health', h);",
            ),
            (
                "apps/b/package.json",
                r#"{"name":"b","scripts":{"start":"y"}}"#,
            ),
        ]);
        let err = detect(&tree, &Options::default()).expect_err("ambiguous");
        assert!(matches!(&err, InitError::Refused(m) if m.contains("--package")));
        let chosen = Options {
            package: Some("apps/a".to_owned()),
            ..Options::default()
        };
        let plan = detect(&tree, &chosen).expect("the chosen package is detected");
        let app = file_json(&plan, APP_PATH);
        assert_eq!(
            app["boot"],
            json!(["npm", "run", "--workspace", "a", "start"])
        );
    }

    #[test]
    fn a_missing_port_read_or_ready_route_refuses_without_a_guess() {
        let no_port = Memory::new(&[
            ("package.json", r#"{"scripts":{"start":"x"}}"#),
            ("package-lock.json", "{}"),
            ("server.js", "app.get('/health', h);"),
        ]);
        assert!(matches!(
            detect(&no_port, &Options::default()),
            Err(InitError::Refused(m)) if m.contains("PORT")
        ));
        let no_ready = Memory::new(&[
            ("package.json", r#"{"scripts":{"start":"x"}}"#),
            ("package-lock.json", "{}"),
            ("server.js", "process.env.PORT; app.get('/api/x', h);"),
        ]);
        assert!(matches!(
            detect(&no_ready, &Options::default()),
            Err(InitError::Refused(m)) if m.contains("ready")
        ));
        let given = Options {
            ready_path: Some("/api/x".to_owned()),
            ..Options::default()
        };
        assert!(detect(&no_ready, &given).is_ok());
    }

    #[test]
    fn engines_must_admit_the_pinned_node() {
        let tree = |range: &str| {
            let package =
                format!(r#"{{"engines":{{"node":"{range}"}},"scripts":{{"start":"x"}}}}"#);
            Memory::new(&[
                ("package.json", package.as_str()),
                ("package-lock.json", "{}"),
                ("server.js", "process.env.PORT; app.get('/health', h);"),
            ])
        };
        assert!(detect(&tree(">=24"), &Options::default()).is_ok());
        assert!(detect(&tree(">=20 <24"), &Options::default()).is_err());
        assert!(detect(&tree("lts/*"), &Options::default()).is_err());
    }

    #[test]
    fn a_lockfile_for_another_manager_is_refused() {
        let tree = Memory::new(&[
            ("package.json", r#"{"scripts":{"start":"x"}}"#),
            ("pnpm-lock.yaml", "lockfileVersion: 9\n"),
        ]);
        assert!(matches!(
            detect(&tree, &Options::default()),
            Err(InitError::Refused(m)) if m.contains("pnpm")
        ));
    }

    #[test]
    fn admits_reads_plain_ranges_and_refuses_the_rest() {
        let pinned = NODE_VERSION;
        assert_eq!(admits(">=24", pinned), Some(true));
        assert_eq!(admits(">=20 <24", pinned), Some(false));
        assert_eq!(admits("^24.0.0", pinned), Some(true));
        assert_eq!(admits("^22", pinned), Some(false));
        assert_eq!(admits("24.x", pinned), Some(true));
        assert_eq!(admits("~24.22.1", pinned), Some(true));
        assert_eq!(admits("~24.21", pinned), Some(false));
        assert_eq!(admits("<24 || >=24.22.0", pinned), Some(true));
        assert_eq!(admits("*", pinned), Some(true));
        assert_eq!(admits("lts/*", pinned), None);
        assert_eq!(admits("18 - 24", pinned), None);
        assert_eq!(admits("", pinned), None);
    }

    #[test]
    fn placeholders_follow_the_placeholder_grammar() {
        let value = placeholder("PAYPAL_CLIENT_SECRET");
        assert_eq!(value, "acan-placeholder-paypal-client-secret");
        let long = placeholder(&format!("TOKEN_{}", "X".repeat(80)));
        assert!(long.len() <= PLACEHOLDER_PREFIX.len() + 48);
        assert!(!long.ends_with('-'));
        assert!(
            long.strip_prefix(PLACEHOLDER_PREFIX)
                .is_some_and(|tail| tail.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
        );
    }
}
