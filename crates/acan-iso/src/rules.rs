//! The kind of each machine: the drawing the renderer uses for it. The rules follow section 8 of
//! `designs/kinds-expansion.md`.
//!
//! A machine gets the first kind that matches, in this order:
//!
//! 1. Crate, exactly: no files, only test files, or only files under a generated directory.
//!    The generated file-name suffixes `.pb.`, `_pb2` and `.g.` also count.
//! 2. The name pass. The name is split into segments on `/` and `\`, and the last segment is
//!    tried first. Each segment is split on every character that is not a letter or digit, and
//!    each word is matched whole and case-insensitively against [`RULES`]. The first segment with
//!    a matching word decides, and in it the first row with a matching word wins, so
//!    `auth-service` is a Gate, `ci` does not match `decide`, and `crates/acan-server/src/settings`
//!    is a Switchbox, not a server.
//! 3. The path pass, only when the name matched no row. The machine's directory is matched the
//!    same way, innermost segment first.
//! 4. The file-name pass. The first row, Assembler aside, with a word in the stems of at least
//!    half the files wins, so a folder of `routes.ts` files is a Switchboard.
//! 5. The import pass. The first row of [`IMPORT_RULES`] with a package that at least half the
//!    files import wins, so a folder that imports `react` is a Kiosk.
//! 6. The language pass, only when no pass matched: a Kiosk when at least 60 percent of the
//!    files are `.tsx` or `.jsx`.
//! 7. The assembler, the fallback. Its words are a row of [`RULES`] too, so a service name is
//!    named as one in the tooltip.
//!
//! The scanner reads only `.ts`, `.tsx`, `.js`, `.jsx`, `.mjs`, `.cjs` and `.rs`, so no `.json`,
//! `.yaml`, `.html` or `.css` file ever reaches a machine. The rules therefore use names, paths,
//! imports and the tsx/jsx share only. Switchbox and Gantry match on words and imports alone,
//! since the language rules for their config and infra files cannot fire.
//!
//! The scanner passes each machine's packages. The renderer passes none when it classifies a
//! plant without kinds, so there a machine that only its imports would name is an assembler.
//!
//! A keyword match is a guess, and the renderer breaks the plinth groove for it. A crate and the
//! assembler are exact, and so is a kind the user sets in `.acan/kinds.json`.

use crate::models::Kind;

/// The token table, in priority order: the first row with a matching word wins. Each kind's
/// words are whole words, matched case-insensitively.
const RULES: [(Kind, &[&str]); 16] = [
    (
        Kind::Gate,
        &[
            "auth",
            "auths",
            "session",
            "sessions",
            "login",
            "logins",
            "permission",
            "permissions",
            "oauth",
            "jwt",
            "guard",
            "guards",
            "acl",
            "policy",
            "policies",
            "identity",
            "password",
            "passwords",
            "credential",
            "credentials",
        ],
    ),
    (
        Kind::Valve,
        &[
            "ratelimit",
            "ratelimits",
            "limit",
            "limits",
            "throttle",
            "throttles",
            "quota",
            "quotas",
            "backoff",
            "budget",
            "budgets",
        ],
    ),
    (
        Kind::Splitter,
        &[
            "middleware",
            "middlewares",
            "proxy",
            "proxies",
            "gateway",
            "gateways",
        ],
    ),
    (
        Kind::Intake,
        &[
            "server",
            "servers",
            "main",
            "index",
            "app",
            "bootstrap",
            "entry",
            "startup",
        ],
    ),
    (
        Kind::Switchboard,
        &[
            "route",
            "routes",
            "router",
            "routers",
            "controller",
            "controllers",
            "handler",
            "handlers",
            "endpoint",
            "endpoints",
            "api",
            "apis",
            "views",
            "resolvers",
            "http",
            "rpc",
            "mcp",
            "graphql",
            "rest",
            "socket",
            "sockets",
        ],
    ),
    (
        Kind::Kiosk,
        &[
            "component",
            "components",
            "ui",
            "pages",
            "page",
            "screens",
            "screen",
            "layout",
            "layouts",
            "widgets",
            "styles",
            "theme",
            "web",
            "www",
            "frontend",
            "render",
            "renderer",
            "canvas",
            "dom",
            "public",
            "static",
        ],
    ),
    (
        Kind::Tank,
        &[
            "store",
            "stores",
            "db",
            "repo",
            "repos",
            "repository",
            "repositories",
            "dao",
            "persistence",
            "storage",
            "cache",
            "migration",
            "migrations",
            "sql",
            "sqlite",
            "database",
            "databases",
            "ledger",
            "ledgers",
            "snapshot",
            "snapshots",
            "archive",
            "archives",
        ],
    ),
    (
        Kind::Mould,
        &[
            "model",
            "models",
            "schema",
            "schemas",
            "entity",
            "entities",
            "types",
            "typings",
            "interfaces",
            "dto",
            "dtos",
            "proto",
            "protos",
            "contracts",
        ],
    ),
    (
        Kind::Conveyor,
        &[
            "job",
            "jobs",
            "queue",
            "queues",
            "worker",
            "workers",
            "task",
            "tasks",
            "scheduler",
            "schedule",
            "cron",
            "consumer",
            "consumers",
            "batch",
            "agent",
            "agents",
            "runner",
            "runners",
        ],
    ),
    (
        Kind::Horn,
        &[
            "event",
            "events",
            "bus",
            "pubsub",
            "broker",
            "topic",
            "topics",
            "emitter",
            "webhook",
            "webhooks",
            "signal",
            "signals",
            "notify",
            "notifications",
            "stream",
            "streams",
            "feed",
            "feeds",
            "broadcast",
        ],
    ),
    (
        Kind::Switchbox,
        &[
            "config",
            "configs",
            "configuration",
            "settings",
            "env",
            "environment",
            "options",
            "constants",
            "flags",
        ],
    ),
    (
        Kind::Gantry,
        &[
            "infra",
            "infrastructure",
            "deploy",
            "deployment",
            "terraform",
            "k8s",
            "kubernetes",
            "helm",
            "docker",
            "compose",
            "ansible",
            "pulumi",
            "ci",
            "github",
            "gitlab",
            "workflows",
            "pipeline",
            "pipelines",
            "release",
            "releases",
            "container",
            "containers",
            "sandbox",
            "volume",
            "volumes",
        ],
    ),
    (
        Kind::Console,
        &[
            "cli", "cmd", "command", "commands", "console", "scripts", "script", "tools", "bin",
            "examples", "example", "demo", "demos", "smoke", "harness",
        ],
    ),
    (
        Kind::Antenna,
        &[
            "client",
            "clients",
            "sdk",
            "integration",
            "integrations",
            "adapter",
            "adapters",
            "connector",
            "connectors",
            "external",
            "provider",
            "providers",
            "llm",
            "llms",
            "git",
            "vision",
        ],
    ),
    (
        Kind::Toolrack,
        &[
            "util", "utils", "helper", "helpers", "common", "shared", "lib", "libs", "pkg",
            "internal", "core",
        ],
    ),
    // The assembler's service words. A match gives the fallback kind, and the tooltip says "service".
    (
        Kind::Assembler,
        &[
            "service", "services", "domain", "usecase", "usecases", "logic",
        ],
    ),
];

/// The package table, in the row order of [`RULES`]: the first row with a package the machine
/// imports wins. The names are as the scanner normalizes them: the first segment of a Rust path,
/// and the npm package name with no `node:` prefix and no subpath.
const IMPORT_RULES: [(Kind, &[&str]); 16] = [
    (
        Kind::Gate,
        &[
            "jsonwebtoken",
            "jose",
            "passport",
            "bcrypt",
            "bcryptjs",
            "argon2",
            "next-auth",
            "express-session",
            "oauth2",
        ],
    ),
    (
        Kind::Valve,
        &[
            "express-rate-limit",
            "bottleneck",
            "p-limit",
            "governor",
            "tower_governor",
        ],
    ),
    (
        Kind::Splitter,
        &["http-proxy", "http-proxy-middleware", "tower_http"],
    ),
    (Kind::Intake, &[]),
    (
        Kind::Switchboard,
        &[
            "express",
            "koa",
            "fastify",
            "hono",
            "@trpc/server",
            "axum",
            "actix_web",
            "warp",
            "rocket",
        ],
    ),
    (
        Kind::Kiosk,
        &[
            "react",
            "react-dom",
            "vue",
            "svelte",
            "preact",
            "solid-js",
            "@angular/core",
            "wasm_bindgen",
            "web_sys",
            "js_sys",
        ],
    ),
    (
        Kind::Tank,
        &[
            "prisma",
            "@prisma/client",
            "sequelize",
            "typeorm",
            "knex",
            "mongoose",
            "pg",
            "mysql2",
            "better-sqlite3",
            "sqlx",
            "diesel",
            "rusqlite",
            "sea_orm",
            "tokio_postgres",
            "redis",
            "ioredis",
        ],
    ),
    (
        Kind::Mould,
        &["zod", "yup", "joi", "ajv", "schemars", "prost"],
    ),
    (
        Kind::Conveyor,
        &[
            "bull",
            "bullmq",
            "agenda",
            "node-cron",
            "cron",
            "apalis",
            "tokio_cron_scheduler",
        ],
    ),
    (
        Kind::Horn,
        &["eventemitter3", "mitt", "socket.io", "lapin", "rdkafka"],
    ),
    (
        Kind::Switchbox,
        &["dotenv", "dotenvy", "convict", "figment"],
    ),
    (
        Kind::Gantry,
        &["dockerode", "bollard", "@kubernetes/client-node", "kube"],
    ),
    (
        Kind::Console,
        &[
            "commander",
            "yargs",
            "inquirer",
            "clap",
            "structopt",
            "argh",
        ],
    ),
    (
        Kind::Antenna,
        &[
            "axios",
            "node-fetch",
            "got",
            "openai",
            "@anthropic-ai/sdk",
            "reqwest",
            "ureq",
            "octocrab",
            "git2",
            "gix",
        ],
    ),
    (Kind::Toolrack, &[]),
    (Kind::Assembler, &[]),
];

/// Directory names that hold generated or vendored output, not code for an agent to edit.
const GENERATED_DIRS: [&str; 7] = [
    "generated",
    "__generated__",
    "gen",
    "dist",
    "build",
    "vendor",
    "node_modules",
];

/// File-name suffixes that mark generated code.
const GENERATED_NAMES: [&str; 3] = [".pb.", "_pb2", ".g."];

/// Why a machine has its kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub kind: Kind,
    /// True when a token or the language share guessed the kind. The renderer breaks the groove.
    pub guessed: bool,
    /// The rule that matched, in words for the tooltip.
    pub reason: String,
}

/// The kind of a machine from its name, its directory, the repo-relative paths of its files, and
/// the external packages that at least half of its files import. `packages` is empty when unknown.
pub fn classify(name: &str, dir: &str, paths: &[&str], packages: &[&str]) -> Decision {
    if let Some(reason) = crate_reason(paths) {
        return Decision {
            kind: Kind::Crate,
            guessed: false,
            reason: reason.to_owned(),
        };
    }
    if let Some((kind, token)) = by_segment(name) {
        return token_decision(kind, format!("name word \"{token}\""));
    }
    if let Some((kind, token)) = by_segment(dir) {
        return token_decision(kind, format!("directory word \"{token}\""));
    }
    if let Some((kind, token, count)) = by_file_names(paths) {
        return Decision {
            kind,
            guessed: true,
            reason: format!(
                "most file names: \"{token}\" ({count} of {} files)",
                paths.len()
            ),
        };
    }
    if let Some((kind, package)) = by_package(packages) {
        return Decision {
            kind,
            guessed: true,
            reason: format!("imports \"{package}\""),
        };
    }
    if mostly_frontend(paths) {
        return Decision {
            kind: Kind::Kiosk,
            guessed: true,
            reason: "most files are tsx or jsx".to_owned(),
        };
    }
    Decision {
        kind: Kind::Assembler,
        guessed: false,
        reason: "no rule matched; default".to_owned(),
    }
}

/// The reason a machine is a crate, or None when it is not one.
fn crate_reason(paths: &[&str]) -> Option<&'static str> {
    if paths.is_empty() {
        Some("no files")
    } else if paths.iter().all(|path| is_test(path)) {
        Some("only tests")
    } else if paths.iter().all(|path| is_generated(path)) {
        Some("generated code")
    } else {
        None
    }
}

/// The first match of [`by_token`] over the `/` or `\` segments of `text`, innermost first.
/// In a large repo a machine's name is its whole folder path, and its innermost folder says the
/// most about it: `crates/acan-server/src/settings` holds settings, not a server.
fn by_segment(text: &str) -> Option<(Kind, String)> {
    text.rsplit(['/', '\\'])
        .find_map(|segment| by_token(&words(segment)))
}

/// The first row of [`RULES`], the assembler aside, with a word in the stems of at least half of
/// `paths`, the first such word, and the number of files with one.
fn by_file_names(paths: &[&str]) -> Option<(Kind, String, usize)> {
    let stems: Vec<Vec<String>> = paths.iter().map(|path| words(stem(path))).collect();
    for (kind, keywords) in RULES {
        if kind == Kind::Assembler {
            continue;
        }
        let matched: Vec<&String> = stems
            .iter()
            .filter_map(|stem_words| {
                stem_words
                    .iter()
                    .find(|word| keywords.contains(&word.as_str()))
            })
            .collect();
        if let Some(word) = matched.first()
            && matched.len() * 2 >= paths.len()
        {
            return Some((kind, (*word).clone(), matched.len()));
        }
    }
    None
}

/// The first row of [`IMPORT_RULES`] with a package among `packages`, and that package.
fn by_package(packages: &[&str]) -> Option<(Kind, &'static str)> {
    IMPORT_RULES.iter().find_map(|(kind, names)| {
        names
            .iter()
            .find(|name| packages.contains(name))
            .map(|name| (*kind, *name))
    })
}

/// The file name of `path` without its last extension. A leading dot is part of the name.
fn stem(path: &str) -> &str {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match name.rfind('.') {
        Some(dot) if dot > 0 => &name[..dot],
        _ => name,
    }
}

/// The first row of [`RULES`] with a word among `words`, and that word. `words` is in order, so
/// the first matching word in the text is the one named.
fn by_token(words: &[String]) -> Option<(Kind, String)> {
    for (kind, keywords) in RULES {
        if let Some(word) = words.iter().find(|word| keywords.contains(&word.as_str())) {
            return Some((kind, word.clone()));
        }
    }
    None
}

fn token_decision(kind: Kind, reason: String) -> Decision {
    // The assembler is the fallback, so a service word gives an exact kind, not a guess.
    Decision {
        kind,
        guessed: kind != Kind::Assembler,
        reason,
    }
}

/// True when at least 60 percent of the files are `.tsx` or `.jsx`.
fn mostly_frontend(paths: &[&str]) -> bool {
    let frontend = paths
        .iter()
        .filter(|path| {
            let lower = path.to_ascii_lowercase();
            lower.ends_with(".tsx") || lower.ends_with(".jsx")
        })
        .count();
    !paths.is_empty() && frontend * 5 >= paths.len() * 3
}

/// The lowercase words of `text`: runs of letters and digits, in order. Every other character
/// separates words, so `auth-service` and `user_store` each have two.
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn is_test(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let in_test_dir = lower
        .split(['/', '\\'])
        .any(|part| matches!(part, "test" | "tests" | "__tests__" | "spec"));
    let test_file =
        lower.contains(".test.") || lower.contains(".spec.") || lower.contains("_test.");
    in_test_dir || test_file
}

/// True when the file is under a generated directory, or its name carries a generated suffix.
fn is_generated(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let (directories, file_name) = match lower.rsplit_once(['/', '\\']) {
        Some((directories, file_name)) => (directories, file_name),
        None => ("", lower.as_str()),
    };
    directories
        .split(['/', '\\'])
        .any(|part| GENERATED_DIRS.contains(&part))
        || GENERATED_NAMES
            .iter()
            .any(|suffix| file_name.contains(suffix))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind_of(name: &str, dir: &str, paths: &[&str]) -> Kind {
        classify(name, dir, paths, &[]).kind
    }

    const SRC: [&str; 1] = ["src/x.ts"];

    #[test]
    fn each_row_picks_its_kind() {
        let cases: [(&str, Kind); 16] = [
            ("auth", Kind::Gate),
            ("ratelimit", Kind::Valve),
            ("middleware", Kind::Splitter),
            ("server", Kind::Intake),
            ("routes", Kind::Switchboard),
            ("components", Kind::Kiosk),
            ("store", Kind::Tank),
            ("schemas", Kind::Mould),
            ("workers", Kind::Conveyor),
            ("webhooks", Kind::Horn),
            ("config", Kind::Switchbox),
            ("deploy", Kind::Gantry),
            ("scripts", Kind::Console),
            ("clients", Kind::Antenna),
            ("utils", Kind::Toolrack),
            ("service", Kind::Assembler),
        ];
        for (name, kind) in cases {
            assert_eq!(kind_of(name, "src", &SRC), kind, "{name}");
        }
    }

    #[test]
    fn hooks_is_not_a_horn_word() {
        // Section 8 dropped "hooks": a React hooks folder is not a broadcast.
        assert_eq!(kind_of("hooks", "src", &SRC), Kind::Assembler);
    }

    #[test]
    fn words_are_whole_and_case_insensitive() {
        // No substring matches: "ci" is not in "decide", "main" is not in "domain".
        assert_eq!(kind_of("decide", "src", &SRC), Kind::Assembler);
        assert_eq!(kind_of("domain", "src", &SRC), Kind::Assembler);
        assert_eq!(kind_of("maintain", "src", &SRC), Kind::Assembler);
        assert_eq!(kind_of("build", "src", &SRC), Kind::Assembler);
        assert_eq!(kind_of("CI", "src", &SRC), Kind::Gantry);
        assert_eq!(kind_of("Auth", "src", &SRC), Kind::Gate);
    }

    #[test]
    fn names_split_on_every_character_that_is_not_a_letter_or_digit() {
        assert_eq!(kind_of("auth-service", "src", &SRC), Kind::Gate);
        assert_eq!(kind_of("user_routes", "src", &SRC), Kind::Switchboard);
        assert_eq!(kind_of("api.v2", "src", &SRC), Kind::Switchboard);
        assert_eq!(kind_of(".github", "src", &["src/a.yml"]), Kind::Gantry);
        // Camel case is not split: the word is "authservice", which matches no row.
        assert_eq!(kind_of("authService", "src", &SRC), Kind::Assembler);
    }

    #[test]
    fn the_table_order_decides_overlapping_words() {
        // Gate before Switchboard: an auth route is auth.
        assert_eq!(kind_of("auth-routes", "src", &SRC), Kind::Gate);
        // Gate before Intake: an auth server is auth.
        assert_eq!(kind_of("auth-server", "src", &SRC), Kind::Gate);
        // Valve before Splitter: a rate-limit middleware is a valve.
        assert_eq!(kind_of("ratelimit-middleware", "src", &SRC), Kind::Valve);
        // Splitter before Intake: a middleware in the app folder is a splitter.
        assert_eq!(kind_of("app-middleware", "src", &SRC), Kind::Splitter);
        // Switchboard before Kiosk: a views module is a switchboard even when its files are tsx.
        assert_eq!(
            kind_of("views", "src", &["src/views.tsx"]),
            Kind::Switchboard
        );
        // Tank before Mould: a store of models is a store.
        assert_eq!(kind_of("store-models", "src", &SRC), Kind::Tank);
    }

    #[test]
    fn the_path_pass_decides_only_when_the_name_matches_nothing() {
        // "errors" matches no name word, so its auth folder decides.
        assert_eq!(kind_of("errors", "auth", &["auth/x.ts"]), Kind::Gate);
        assert_eq!(kind_of("errors", "store", &["store/x.ts"]), Kind::Tank);
        assert_eq!(kind_of("src", "auth", &["auth/x.ts"]), Kind::Gate);
        // A name word that matches a row wins over the folder, even a different row.
        assert_eq!(kind_of("store", "auth", &["auth/x.ts"]), Kind::Tank);
    }

    #[test]
    fn a_name_that_matches_nothing_is_an_assembler() {
        let decision = classify("errors", "src", &SRC, &[]);
        assert_eq!(decision.kind, Kind::Assembler);
        assert!(!decision.guessed);
        assert_eq!(decision.reason, "no rule matched; default");
    }

    #[test]
    fn a_service_word_names_the_assembler_as_a_service() {
        let decision = classify("billing-service", "src", &SRC, &[]);
        assert_eq!(decision.kind, Kind::Assembler);
        assert!(!decision.guessed);
        assert_eq!(decision.reason, "name word \"service\"");
    }

    #[test]
    fn token_matches_are_guesses_with_their_rule_named() {
        let decision = classify("auth", "src", &SRC, &[]);
        assert_eq!(decision.kind, Kind::Gate);
        assert!(decision.guessed);
        assert_eq!(decision.reason, "name word \"auth\"");
        let by_dir = classify("errors", "routes", &["routes/x.ts"], &[]);
        assert_eq!(by_dir.kind, Kind::Switchboard);
        assert!(by_dir.guessed);
        assert_eq!(by_dir.reason, "directory word \"routes\"");
    }

    #[test]
    fn the_path_pass_names_the_innermost_matching_word() {
        let decision = classify("errors", "app/routes/api", &["app/routes/api/x.ts"], &[]);
        assert_eq!(
            decision.kind,
            Kind::Switchboard,
            "api is the innermost folder, so it beats app"
        );
        assert_eq!(decision.reason, "directory word \"api\"");
    }

    #[test]
    fn the_innermost_segment_of_a_name_wins() {
        let cases: [(&str, Kind); 4] = [
            ("crates/acan-server/src/settings", Kind::Switchbox),
            ("crates/acan-server/src", Kind::Intake),
            ("crates/acan-llm/examples", Kind::Console),
            ("crates/acan-witness/src/harness", Kind::Console),
        ];
        for (name, kind) in cases {
            assert_eq!(kind_of(name, name, &SRC), kind, "{name}");
        }
        assert_eq!(
            classify("crates\\acan-server\\src\\settings", "src", &SRC, &[]).reason,
            "name word \"settings\""
        );
    }

    #[test]
    fn most_file_names_decide_when_no_folder_matches() {
        let majority = [
            "src/a/routes.ts",
            "src/b/routes.ts",
            "src/c/routes.ts",
            "src/a/x.ts",
            "src/b/y.ts",
        ];
        let decision = classify("feature", "src", &majority, &[]);
        assert_eq!(decision.kind, Kind::Switchboard);
        assert!(decision.guessed);
        assert_eq!(
            decision.reason,
            "most file names: \"routes\" (3 of 5 files)"
        );
        let minority = [
            "src/a/routes.ts",
            "src/b/routes.ts",
            "src/a/x.ts",
            "src/b/y.ts",
            "src/c/z.ts",
        ];
        assert_eq!(kind_of("feature", "src", &minority), Kind::Assembler);
    }

    #[test]
    fn the_name_beats_the_file_names() {
        let files = ["src/routes.ts", "src/api.ts"];
        assert_eq!(kind_of("store", "src", &files), Kind::Tank);
    }

    #[test]
    fn the_import_rows_follow_the_token_rows() {
        for ((token_kind, _), (import_kind, _)) in RULES.iter().zip(IMPORT_RULES.iter()) {
            assert_eq!(token_kind, import_kind);
        }
    }

    #[test]
    fn a_shared_package_decides_when_no_word_matches() {
        let decision = classify("panel", "src", &SRC, &["react"]);
        assert_eq!(decision.kind, Kind::Kiosk);
        assert!(decision.guessed);
        assert_eq!(decision.reason, "imports \"react\"");
        let unknown = classify("panel", "src", &SRC, &[]);
        assert_eq!(unknown.kind, Kind::Assembler);
        assert_eq!(unknown.reason, "no rule matched; default");
        assert_eq!(classify("panel", "src", &SRC, &["left-pad"]), unknown);
        // Gate is above Kiosk in the table, so jsonwebtoken wins over react.
        assert_eq!(
            classify("panel", "src", &SRC, &["react", "jsonwebtoken"]).kind,
            Kind::Gate
        );
        // A word in the name comes before the imports.
        assert_eq!(classify("store", "src", &SRC, &["react"]).kind, Kind::Tank);
    }

    #[test]
    fn each_added_word_group_picks_its_kind() {
        let cases: [(&str, Kind); 10] = [
            ("credentials", Kind::Gate),
            ("budget", Kind::Valve),
            ("mcp", Kind::Switchboard),
            ("www", Kind::Kiosk),
            ("ledger", Kind::Tank),
            ("agents", Kind::Conveyor),
            ("stream", Kind::Horn),
            ("sandbox", Kind::Gantry),
            ("examples", Kind::Console),
            ("llm", Kind::Antenna),
        ];
        for (name, kind) in cases {
            assert_eq!(kind_of(name, "src", &SRC), kind, "{name}");
        }
    }

    #[test]
    fn mostly_tsx_or_jsx_is_a_kiosk_when_no_word_matches() {
        let files = [
            "src/panel/a.tsx",
            "src/panel/b.tsx",
            "src/panel/c.jsx",
            "src/panel/d.ts",
        ];
        let decision = classify("panel", "src/panel", &files, &[]);
        assert_eq!(decision.kind, Kind::Kiosk);
        assert!(decision.guessed);
        assert_eq!(decision.reason, "most files are tsx or jsx");
        let fewer = [
            "src/panel/a.tsx",
            "src/panel/b.ts",
            "src/panel/c.ts",
            "src/panel/d.ts",
        ];
        assert_eq!(kind_of("panel", "src/panel", &fewer), Kind::Assembler);
    }

    #[test]
    fn a_word_beats_the_language_share() {
        let files = ["src/utils/a.tsx", "src/utils/b.tsx"];
        assert_eq!(kind_of("utils", "src/utils", &files), Kind::Toolrack);
    }

    #[test]
    fn test_only_empty_and_generated_machines_are_crates() {
        let test_only = classify("test", "test", &["test/a.ts", "test/b.ts"], &[]);
        assert_eq!(test_only.kind, Kind::Crate);
        assert_eq!(test_only.reason, "only tests");
        assert_eq!(kind_of("auth", "src", &["src/auth.test.ts"]), Kind::Crate);
        let empty = classify("store", "src", &[], &[]);
        assert_eq!(empty.kind, Kind::Crate);
        assert_eq!(empty.reason, "no files");
        let generated = classify(
            "api",
            "src/gen",
            &["src/gen/api.ts", "src/gen/types.ts"],
            &[],
        );
        assert_eq!(generated.kind, Kind::Crate);
        assert_eq!(generated.reason, "generated code");
        assert!(!generated.guessed);
        assert_eq!(kind_of("client", "src", &["src/client.pb.ts"]), Kind::Crate);
        assert_eq!(
            kind_of("client", "src", &["src/client_pb2.ts"]),
            Kind::Crate
        );
        assert_eq!(kind_of("client", "src", &["src/api.g.ts"]), Kind::Crate);
    }

    #[test]
    fn a_generated_word_in_a_file_name_is_not_a_generated_directory() {
        // "gen" as a file name is not a directory, so the machine is not a crate.
        assert_eq!(kind_of("api", "src", &["src/gen.ts"]), Kind::Switchboard);
    }

    #[test]
    fn a_mix_with_one_source_file_is_not_a_crate() {
        assert_eq!(
            kind_of("auth", "src", &["src/auth.ts", "src/auth.test.ts"]),
            Kind::Gate
        );
        // Only all-generated machines are crates; one hand-written file keeps the machine.
        assert_eq!(
            kind_of("api", "src", &["src/gen/api.ts", "src/api.ts"]),
            Kind::Switchboard
        );
    }
}
