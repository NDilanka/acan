//! Exchanges with the daemon through the same planner, transport and decoder as the hook.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use acan_hook::input::{self, Plan};
use acan_hook::protocol::{Decision, HookResponse, encode_request, parse_reply};
use acan_hook::transport::{self, Endpoint};
use acan_intent::{PathScope, RepoRules, ScopeEntry};
use acan_server::hook_gate::{
    ActiveOrderPolicy, FirstRunHookProbe, HookProbeError, HookRegistry, HookServer,
    MAX_REQUEST_BYTES,
};
use acan_server::plugin::{PluginConfig, PluginError, install_plugin_bundle, verify_plugin_bundle};
use serde_json::{Value, json};
use tempfile::TempDir;

const ORDER: &str = "order-1";
const WAIT: Duration = Duration::from_secs(5);

fn register(registry: &HookRegistry, order_id: &str, root: &Path, scope: &str, allow_shell: bool) {
    registry
        .register_order(ActiveOrderPolicy {
            order_id: order_id.to_owned(),
            worktree_root: root.to_owned(),
            scope: PathScope::new([ScopeEntry::from_input(scope).expect("scope entry")])
                .expect("scope"),
            rules: RepoRules {
                check_files: vec!["src/checks.yml".parse().expect("check file")],
                ..RepoRules::default()
            },
            allow_shell,
            allowed_mcp_tools: ["draft_order".to_owned()].into_iter().collect(),
        })
        .expect("register order");
}

async fn exchange(server: &HookServer, line: impl Into<Vec<u8>>) -> HookResponse {
    let endpoint = Endpoint::parse(server.endpoint()).expect("server gave a hook endpoint");
    let line = line.into();
    tokio::task::spawn_blocking(move || {
        let text = transport::exchange(&endpoint, &line).expect("hook transport succeeds");
        assert_eq!(text.matches('\n').count(), 1);
        let reply = parse_reply(&text).expect("daemon replies with v1");
        if reply.decision == Decision::Allow {
            assert_eq!(text, "{\"version\":1,\"decision\":\"allow\"}\n");
        }
        reply
    })
    .await
    .expect("hook exchange task")
}

async fn ask(server: &HookServer, order: &str, input: Value) -> HookResponse {
    let plan = input::plan(
        input::parse(&input.to_string()).expect("Droid input"),
        order,
    )
    .expect("hook plan");
    let Plan::Ask(request) = plan else {
        panic!("expected a daemon exchange");
    };
    exchange(server, encode_request(&request).expect("wire request")).await
}

async fn tool(server: &HookServer, cwd: &Path, name: &str, input: Value) -> HookResponse {
    ask(
        server,
        ORDER,
        json!({
            "hook_event_name": "PreToolUse", "tool_name": name, "tool_input": input, "cwd": cwd,
        }),
    )
    .await
}

fn allowed(reply: HookResponse) {
    assert_eq!(reply.decision, Decision::Allow, "{:?}", reply.reason);
    assert_eq!(reply.reason, None);
}

fn denied(reply: HookResponse, why: &str) {
    assert_eq!(reply.decision, Decision::Deny, "expected {why:?}");
    let reason = reply.reason.expect("deny reason");
    assert!(!reason.trim().is_empty());
    assert!(!reason.contains(['\n', '\r', '\u{2028}', '\u{2029}']));
    assert!(reason.contains(why), "{reason:?} should contain {why:?}");
}

async fn heartbeats(server: &HookServer, registry: &HookRegistry, root: &Path) {
    let input = json!({
        "hook_event_name": "SessionStart", "session_id": "droid-session", "cwd": root,
    });
    assert!(registry.last_heartbeat(ORDER).is_none());
    let (seen, reply) = tokio::join!(
        registry.wait_for_heartbeat(ORDER, WAIT),
        ask(server, ORDER, input.clone()),
    );
    allowed(reply);
    let seen = seen.expect("heartbeat arrives");
    assert_eq!(seen.order_id, ORDER);
    assert_eq!(seen.session_id.as_deref(), Some("droid-session"));
    assert_eq!(seen.cwd.as_deref(), root.to_str());
    assert_eq!(registry.last_heartbeat(ORDER), Some(seen));
    denied(ask(server, "inactive", input).await, "unknown order");
    assert!(registry.last_heartbeat("inactive").is_none());
    registry.unregister_order(ORDER);
    assert!(registry.last_heartbeat(ORDER).is_none());
}

#[tokio::test]
async fn loopback_session_starts_and_records_heartbeat() {
    let dir = TempDir::new().expect("worktree");
    let registry = HookRegistry::new();
    register(&registry, ORDER, dir.path(), "src/", false);
    let server = HookServer::bind_loopback(0, registry.clone())
        .await
        .expect("listen");
    assert!(server.endpoint().starts_with("tcp://127.0.0.1:"));
    heartbeats(&server, &registry, dir.path()).await;
    server.shutdown().await.expect("shutdown");
}

#[cfg(unix)]
#[tokio::test]
async fn unix_socket_is_private_and_removed_on_shutdown_or_drop() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().expect("worktree");
    let registry = HookRegistry::new();
    register(&registry, ORDER, dir.path(), "src/", false);
    let parent = dir.path().join("daemon");
    let path = parent.join("hook.sock");
    let server = HookServer::bind_unix(&path, registry.clone())
        .await
        .expect("listen");
    assert_eq!(server.endpoint(), path.to_str().expect("UTF-8"));
    for (path, mode) in [(&parent, 0o700), (&path, 0o600)] {
        assert_eq!(
            fs::metadata(path).expect("metadata").permissions().mode() & 0o777,
            mode
        );
    }
    assert!(
        HookServer::bind_unix(&path, registry.clone())
            .await
            .is_err()
    );
    heartbeats(&server, &registry, dir.path()).await;
    server.shutdown().await.expect("shutdown");
    assert!(!path.exists());

    drop(std::os::unix::net::UnixListener::bind(&path).expect("stale socket fixture"));
    let server = HookServer::bind_unix(&path, registry.clone())
        .await
        .expect("replace stale socket");
    assert!(path.exists());
    drop(server);
    assert!(!path.exists());

    fs::write(&path, "not a socket").expect("regular file");
    assert!(
        HookServer::bind_unix(&path, registry.clone())
            .await
            .is_err()
    );
    assert_eq!(
        fs::read_to_string(&path).expect("the non-socket remains"),
        "not a socket"
    );
    let link = parent.join("hook-link.sock");
    std::os::unix::fs::symlink(&path, &link).expect("link fixture");
    assert!(HookServer::bind_unix(&link, registry).await.is_err());
    assert!(
        fs::symlink_metadata(&link)
            .expect("symlink remains")
            .is_symlink()
    );
}

#[cfg(windows)]
#[tokio::test]
async fn windows_named_pipe_accepts_concurrent_exchange() {
    let dir = TempDir::new().expect("worktree");
    let registry = HookRegistry::new();
    register(&registry, ORDER, dir.path(), "src/", false);
    let server = HookServer::bind_default_named_pipe(registry.clone()).expect("listen");
    let id = server
        .endpoint()
        .strip_prefix(r"\\.\pipe\acan-hook-")
        .expect("default pipe prefix");
    assert_eq!(
        uuid::Uuid::parse_str(id)
            .expect("pipe UUID")
            .get_version_num(),
        4
    );
    assert!(HookServer::bind_named_pipe(server.endpoint(), registry.clone()).is_err());
    let replies = futures_util::future::join_all(
        (0..8).map(|_| tool(&server, dir.path(), "Read", json!({}))),
    )
    .await;
    for reply in replies {
        allowed(reply);
    }
    heartbeats(&server, &registry, dir.path()).await;
    server.shutdown().await.expect("shutdown");
}

#[cfg(unix)]
#[tokio::test]
async fn unix_socket_refuses_non_private_or_symlink_parents() {
    use std::io::ErrorKind;
    use std::os::unix::fs::{PermissionsExt, symlink};

    let dir = TempDir::new().expect("fixture");
    let parent = dir.path().join("run");
    fs::create_dir(&parent).expect("socket parent");
    let path = parent.join("hook.sock");
    let registry = HookRegistry::new();
    for mode in [0o755, 0o777] {
        fs::set_permissions(&parent, fs::Permissions::from_mode(mode)).expect("parent mode");
        let err = HookServer::bind_unix(&path, registry.clone())
            .await
            .expect_err("must refuse a non-private parent");
        assert_eq!(err.kind(), ErrorKind::PermissionDenied);
        assert!(!path.exists());
    }

    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).expect("private parent");
    let alias = dir.path().join("alias");
    symlink(&parent, &alias).expect("parent symlink");
    let err = HookServer::bind_unix(alias.join("./hook.sock"), registry.clone())
        .await
        .expect_err("must refuse an alias to a private parent");
    assert_eq!(err.kind(), ErrorKind::PermissionDenied);
    assert!(!path.exists());
    let uncanonical = parent.join(".").join("hook.sock");
    let server = HookServer::bind_unix(&uncanonical, registry)
        .await
        .expect("private parent works");
    let canonical = parent
        .canonicalize()
        .expect("canonical parent")
        .join("hook.sock");
    assert_eq!(server.endpoint(), canonical.to_string_lossy());
    server.shutdown().await.expect("shutdown");
}

#[cfg(unix)]
#[tokio::test]
async fn unix_socket_cleanup_does_not_follow_a_replaced_parent() {
    use std::os::unix::fs::symlink;

    let dir = TempDir::new().expect("fixture");
    let parent = dir.path().join("run");
    let moved = dir.path().join("moved");
    let path = parent.join("hook.sock");
    let server = HookServer::bind_unix(&path, HookRegistry::new())
        .await
        .expect("listen");
    fs::rename(&parent, &moved).expect("move private parent");
    let other = dir.path().join("other");
    fs::create_dir(&other).expect("other parent");
    let foreign = other.join("hook.sock");
    let _other_listener = std::os::unix::net::UnixListener::bind(&foreign).expect("other socket");
    symlink(&other, &parent).expect("replace old parent with symlink");

    server.shutdown().await.expect("shutdown");
    assert!(!moved.join("hook.sock").exists());
    assert!(foreign.exists());
    assert!(
        fs::symlink_metadata(&parent)
            .expect("alias remains")
            .is_symlink()
    );
}

#[tokio::test]
async fn reads_and_writes_obey_their_distinct_path_policies() {
    let dir = TempDir::new().expect("worktree");
    let outside = TempDir::new().expect("outside worktree");
    let root = dir.path();
    fs::create_dir_all(root.join("src/nested")).expect("source directory");
    let registry = HookRegistry::new();
    register(&registry, ORDER, root, "src/", false);
    let server = HookServer::bind_loopback(0, registry)
        .await
        .expect("listen");

    allowed(tool(&server, root, "Edit", json!({"file_path": "src/app.rs"})).await);
    allowed(
        tool(
            &server,
            root,
            "Create",
            json!({"path": root.join("src/nested/new.rs")}),
        )
        .await,
    );
    allowed(
        ask(
            &server,
            ORDER,
            json!({
                "hook_event_name": "PreToolUse", "tool_name": "Create", "cwd": root.join("src"),
                "tool_input": {"file_path": "nested/new.rs"},
            }),
        )
        .await,
    );
    allowed(
        tool(
            &server,
            root,
            "MultiEdit",
            json!({
                "file_path": "src/app.rs", "paths": ["src/a.rs", "src/b.rs"],
                "edits": [{"target_file": "src/c.rs"}],
            }),
        )
        .await,
    );
    denied(
        tool(&server, root, "Edit", json!({"file_path": "notes/a.md"})).await,
        "outside the scope",
    );
    denied(
        tool(
            &server,
            root,
            "Edit",
            json!({"file_path": "src/checks.yml"}),
        )
        .await,
        "check file",
    );
    denied(
        tool(&server, root, "Write", json!({"path": "src/Cargo.lock"})).await,
        "shared file",
    );
    denied(
        tool(&server, root, "Delete", json!({"path": "src/nested"})).await,
        "not a directory",
    );
    denied(
        tool(
            &server,
            root,
            "Edit",
            json!({"file_path": "src/../src/app.rs"}),
        )
        .await,
        ".. segment",
    );
    denied(
        tool(
            &server,
            root,
            "Write",
            json!({"path": outside.path().join("new.rs")}),
        )
        .await,
        "outside the worktree",
    );
    denied(
        ask(
            &server,
            ORDER,
            json!({
                "hook_event_name": "PreToolUse", "tool_name": "Create", "cwd": outside.path(),
                "tool_input": {"file_path": "src/nested/new.rs"},
            }),
        )
        .await,
        "invalid cwd",
    );
    for path in [".acan/world.db", ".git/HEAD", ".factory/hooks.json"] {
        denied(
            tool(&server, root, "Edit", json!({"file_path": path})).await,
            "protected",
        );
    }
    for args in [
        json!({"file_path": "src/a.rs", "paths": ["src/b.rs", "notes/b.md"]}),
        json!({"edits": [{"file_path": "src/a.rs"}, {"target_file": "notes/b.md"}]}),
    ] {
        denied(
            tool(&server, root, "MultiEdit", args).await,
            "outside the scope",
        );
    }
    denied(
        tool(
            &server,
            root,
            "Move",
            json!({"old_path": "src/a.rs", "new_path": "notes/b.md"}),
        )
        .await,
        "outside the scope",
    );
    denied(
        tool(
            &server,
            root,
            "MultiEdit",
            json!({"file_path": "src/a.rs", "paths": [true]}),
        )
        .await,
        "non-string",
    );
    denied(
        tool(&server, root, "Edit", json!({})).await,
        "no write target",
    );
    denied(
        tool(
            &server,
            root,
            "ApplyPatch",
            json!({"file_path": "src/a.rs", "input": "opaque patch"}),
        )
        .await,
        "free-form patch",
    );

    allowed(tool(&server, root, "Read", json!({"file_path": "notes/a.md"})).await);
    allowed(tool(&server, root, "Grep", json!({"pattern": "needle"})).await);
    allowed(
        tool(
            &server,
            root,
            "FetchUrl",
            json!({"url": "https://example.com"}),
        )
        .await,
    );
    denied(
        tool(
            &server,
            root,
            "Read",
            json!({"file_path": ".ACAN/TOKENS.JSON"}),
        )
        .await,
        "reading .acan/tokens.json",
    );
    denied(
        tool(
            &server,
            root,
            "Read",
            json!({"file_path": "src/bad\u{0}name"}),
        )
        .await,
        "control character",
    );
    denied(
        tool(&server, root, "Glob", json!({"folder": outside.path()})).await,
        "outside the worktree",
    );
    denied(
        ask(
            &server,
            "inactive",
            json!({
                "hook_event_name": "PreToolUse", "tool_name": "Read", "tool_input": {},
            }),
        )
        .await,
        "unknown order",
    );
    server.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn mutating_tools_require_an_explicit_absolute_worktree_cwd() {
    let dir = TempDir::new().expect("worktree");
    let outside = TempDir::new().expect("outside worktree");
    let registry = HookRegistry::new();
    register(&registry, ORDER, dir.path(), "src/", true);
    let server = HookServer::bind_loopback(0, registry)
        .await
        .expect("listen");
    for (name, args) in [
        ("Edit", json!({"file_path": dir.path().join("src/app.rs")})),
        (
            "Execute",
            json!({"command": "git --no-optional-locks status"}),
        ),
    ] {
        let mut input = json!({
            "hook_event_name": "PreToolUse", "tool_name": name, "tool_input": args,
        });
        denied(ask(&server, ORDER, input.clone()).await, "missing cwd");
        input["cwd"] = json!(".");
        denied(ask(&server, ORDER, input.clone()).await, "absolute cwd");
        input["cwd"] = json!(outside.path());
        denied(
            ask(&server, ORDER, input.clone()).await,
            "outside the worktree",
        );
        input["cwd"] = json!(dir.path());
        allowed(ask(&server, ORDER, input).await);
    }
    server.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn mutating_tools_reject_unrecognized_and_malformed_input_keys() {
    let dir = TempDir::new().expect("worktree");
    let registry = HookRegistry::new();
    register(&registry, ORDER, dir.path(), "src/", false);
    let server = HookServer::bind_loopback(0, registry)
        .await
        .expect("listen");
    for (name, input) in [
        (
            "Edit",
            json!({"file_path": "src/a.rs", "old_str": "x", "new_str": "y", "change_all": true}),
        ),
        ("Create", json!({"file_path": "src/a.rs", "content": "x"})),
        ("Write", json!({"path": "src/a.rs", "content": "x"})),
        (
            "MultiEdit",
            json!({"file_path": "src/a.rs", "edits": [{"old_string": "x", "new_string": "y"}]}),
        ),
        ("Delete", json!({"path": "src/a.rs"})),
        ("Patch", json!({"target_file": "src/a.rs"})),
        ("ApplyPatch", json!({"paths": ["src/a.rs", "src/b.rs"]})),
        (
            "NotebookEdit",
            json!({"notebook_path": "src/a.ipynb", "new_source": "x"}),
        ),
        (
            "Rename",
            json!({"old_path": "src/a.rs", "new_path": "src/b.rs"}),
        ),
        (
            "Move",
            json!({"old_path": "src/a.rs", "new_path": "src/b.rs"}),
        ),
    ] {
        allowed(tool(&server, dir.path(), name, input.clone()).await);
        let mut input = input;
        input["destination"] = json!("notes/a.rs");
        denied(
            tool(&server, dir.path(), name, input).await,
            "unrecognized input key \"destination\"",
        );
    }
    for (name, input, reason) in [
        (
            "Edit",
            json!({"file_path": "src/a.rs", "new_path": "src/b.rs"}),
            "unrecognized input key \"new_path\"",
        ),
        (
            "Write",
            json!({"file_path": "src/a.rs", "content": {"path": "notes/a.rs"}}),
            "content is not a string",
        ),
        (
            "MultiEdit",
            json!({"file_path": "src/a.rs", "edits": [{"destination": "notes/a.rs"}]}),
            "unrecognized input key \"destination\"",
        ),
    ] {
        denied(tool(&server, dir.path(), name, input).await, reason);
    }
    server.shutdown().await.expect("shutdown");

    let shell_registry = HookRegistry::new();
    register(&shell_registry, ORDER, dir.path(), "src/", true);
    let shell_server = HookServer::bind_loopback(0, shell_registry)
        .await
        .expect("listen");
    for name in ["Execute", "Bash", "Shell", "Terminal", "RunCommand"] {
        allowed(
            tool(
                &shell_server,
                dir.path(),
                name,
                json!({"command": "cargo test", "description": "run unit tests", "timeout": 60}),
            )
            .await,
        );
        denied(
            tool(
                &shell_server,
                dir.path(),
                name,
                json!({"command": "cargo test", "workdir": "/tmp"}),
            )
            .await,
            "unrecognized input key \"workdir\"",
        );
    }
    shell_server.shutdown().await.expect("shutdown");
}

#[cfg(unix)]
#[tokio::test]
async fn symlinks_cannot_escape_or_hide_protected_or_unscoped_targets() {
    use std::os::unix::fs::symlink;

    let dir = TempDir::new().expect("worktree");
    let root = dir.path();
    let outside = TempDir::new().expect("outside worktree");
    for name in ["src", "notes", ".acan"] {
        fs::create_dir(root.join(name)).expect("fixture directory");
    }
    fs::write(root.join(".acan/tokens.json"), "{}").expect("fixture token file");
    symlink(outside.path(), root.join("src/escape")).expect("external link");
    symlink(outside.path().join("missing"), root.join("src/dangling")).expect("dangling link");
    symlink(root.join("notes"), root.join("src/alias")).expect("unscoped alias");
    symlink(root.join(".acan/tokens.json"), root.join("src/info")).expect("token alias");
    let registry = HookRegistry::new();
    register(&registry, ORDER, root, "src/", false);
    let server = HookServer::bind_loopback(0, registry)
        .await
        .expect("listen");
    denied(
        tool(
            &server,
            root,
            "Create",
            json!({"path": "src/escape/new/a.rs"}),
        )
        .await,
        "outside the worktree",
    );
    denied(
        tool(
            &server,
            root,
            "Create",
            json!({"path": "src/dangling/a.rs"}),
        )
        .await,
        "cannot resolve",
    );
    denied(
        tool(&server, root, "Create", json!({"path": "src/alias/a.rs"})).await,
        "outside the scope",
    );
    denied(
        tool(&server, root, "Read", json!({"file_path": "src/info"})).await,
        "reading .acan/tokens.json",
    );
    denied(
        tool(&server, root, "Edit", json!({"file_path": "src/info"})).await,
        "protected",
    );
    server.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn git_commands_allow_only_read_only_subcommands_without_unsafe_options() {
    let dir = TempDir::new().expect("worktree");
    let registry = HookRegistry::new();
    register(&registry, ORDER, dir.path(), "src/", true);
    let server = HookServer::bind_loopback(0, registry)
        .await
        .expect("listen");
    for subcommand in [
        "status",
        "diff --no-ext-diff --no-textconv",
        "log --no-ext-diff --no-textconv",
        "show --no-ext-diff --no-textconv",
        "rev-parse",
        "ls-files",
        "grep",
        "describe",
        "shortlog",
        "blame --no-ext-diff --no-textconv",
        "cat-file",
        "ls-tree",
        "merge-base",
        "rev-list",
        "--version",
    ] {
        allowed(
            tool(
                &server,
                dir.path(),
                "Execute",
                json!({"command": format!("git --no-pager --no-optional-locks {subcommand}")}),
            )
            .await,
        );
    }
    allowed(
        tool(
            &server,
            dir.path(),
            "Execute",
            json!({"command": "GIT_OPTIONAL_LOCKS=0 git status"}),
        )
        .await,
    );
    for (arguments, reason) in [
        ("status", "requires --no-optional-locks"),
        ("diff", "requires --no-optional-locks"),
        (
            "diff --no-ext-diff --no-textconv",
            "requires --no-optional-locks",
        ),
        (
            "--no-optional-locks diff",
            "requires leading --no-ext-diff and --no-textconv",
        ),
        (
            "--no-optional-locks diff --no-ext-diff",
            "requires leading --no-ext-diff and --no-textconv",
        ),
        (
            "--no-optional-locks diff --no-textconv",
            "requires leading --no-ext-diff and --no-textconv",
        ),
        (
            "--no-optional-locks diff -S --no-ext-diff --no-textconv",
            "requires leading --no-ext-diff and --no-textconv",
        ),
        ("log", "requires leading --no-ext-diff and --no-textconv"),
        (
            "show HEAD",
            "requires leading --no-ext-diff and --no-textconv",
        ),
        (
            "blame src/lib.rs",
            "requires leading --no-ext-diff and --no-textconv",
        ),
        (
            "--no-optional-locks diff -- --no-ext-diff --no-textconv",
            "requires leading --no-ext-diff and --no-textconv",
        ),
        ("show-ref", "only read-only git commands"),
        ("diff --output result.txt", "git argument"),
        ("show --output=result.txt", "git argument"),
        ("log -o result.txt", "git argument"),
        ("diff -poresult.txt", "git argument"),
        ("-c core.hooksPath=elsewhere status", "git argument"),
        ("-ccore.hooksPath=elsewhere status", "git argument"),
        ("--config-env core.hooksPath=HOOKS status", "git argument"),
        ("--config-env=core.hooksPath=HOOKS status", "git argument"),
        ("--git-dir repo status", "git argument"),
        ("--git-dir=repo status", "git argument"),
        ("--work-tree other status", "git argument"),
        ("--work-tree=other status", "git argument"),
        ("-C other status", "git argument"),
        ("--exec-path status", "git argument"),
        ("--exec-path=tools status", "git argument"),
        ("diff --ext-diff", "git argument"),
        ("diff --textconv", "git argument"),
        ("diff --out=result.txt", "git argument"),
        ("cat-file --filters HEAD:path", "git argument"),
        ("grep -Ocustom needle", "git argument"),
        ("diff > result.txt", "redirection or grouping"),
        (
            "diff < input --output=result.txt",
            "redirection or grouping",
        ),
    ] {
        denied(
            tool(
                &server,
                dir.path(),
                "Execute",
                json!({"command": format!("git {arguments}")}),
            )
            .await,
            reason,
        );
    }
    denied(
        tool(
            &server,
            dir.path(),
            "Execute",
            json!({"command": "env GIT_EXTERNAL_DIFF=other git diff"}),
        )
        .await,
        "git environment overrides",
    );
    for (command, reason) in [
        (
            "echo GIT_OPTIONAL_LOCKS=0 && git status",
            "requires --no-optional-locks",
        ),
        (
            "git status GIT_OPTIONAL_LOCKS=0",
            "requires --no-optional-locks",
        ),
        #[cfg(unix)]
        (
            "git_optional_locks=0 git status",
            "requires --no-optional-locks",
        ),
        (
            "GIT_OPTIONAL_LOCKS=0\u{00a0}git status",
            "unsupported shell syntax",
        ),
        (
            "GIT_OPTIONAL_LOCKS=0\rgit status",
            "unsupported shell syntax",
        ),
    ] {
        denied(
            tool(&server, dir.path(), "Execute", json!({"command": command})).await,
            reason,
        );
    }
    server.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn interlock_applies_to_the_target_and_releases_on_unregister() {
    let root_a = TempDir::new().expect("order-a worktree");
    let root_b = TempDir::new().expect("order-b worktree");
    let registry = HookRegistry::new();
    register(&registry, "order-a", root_a.path(), "src/", false);
    registry
        .register_order(ActiveOrderPolicy {
            order_id: ORDER.to_owned(),
            worktree_root: root_b.path().to_owned(),
            scope: PathScope::new([ScopeEntry::RepoRoot { tests: true }]).expect("scope"),
            rules: RepoRules::default(),
            allow_shell: false,
            allowed_mcp_tools: Default::default(),
        })
        .expect("register order-b");
    let server = HookServer::bind_loopback(0, registry.clone())
        .await
        .expect("listen");
    denied(
        tool(
            &server,
            root_b.path(),
            "Edit",
            json!({"path": "SRC/main.rs"}),
        )
        .await,
        "active order order-a",
    );
    // The other scope does not grant test writes, but the overlap is conservative.
    denied(
        tool(
            &server,
            root_b.path(),
            "Edit",
            json!({"path": "src/main_test.rs"}),
        )
        .await,
        "active order order-a",
    );
    allowed(
        tool(
            &server,
            root_b.path(),
            "Create",
            json!({"path": "docs/a.md"}),
        )
        .await,
    );
    registry.unregister_order("order-a");
    allowed(
        tool(
            &server,
            root_b.path(),
            "Edit",
            json!({"path": "src/main.rs"}),
        )
        .await,
    );
    server.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn shell_and_mcp_tools_require_their_own_allowances() {
    let dir = TempDir::new().expect("worktree");
    let registry = HookRegistry::new();
    register(&registry, ORDER, dir.path(), "src/", true);
    let server = HookServer::bind_loopback(0, registry.clone())
        .await
        .expect("listen");
    for command in [
        "cargo test",
        "git --no-optional-locks status && cargo test",
        "echo .github/workflows/ci.yml",
    ] {
        allowed(tool(&server, dir.path(), "Execute", json!({"command": command})).await);
    }
    for command in [
        "git commit -m x",
        "git push",
        "git add src/a.rs",
        "cat x && git reset --hard",
        "git switch other",
        "git checkout other",
        "git rebase main",
        "git merge main",
        "git cherry-pick HEAD",
        "git tag t",
        "git branch x",
        "git update-ref x y",
        "git config core.hooksPath x",
        "echo x > .acan/pwn",
        "echo x > '.factory/hooks.json'",
        "cat .git/HEAD",
        "unset ACAN_ORDER_ID ACAN_HOOK_ENDPOINT",
        r#"echo ok; "git" 'commit'"#,
        "test -f x || git push",
        "printf '' | git -C . add src/a.rs",
        "echo `git push`",
        "echo $(git push)",
    ] {
        denied(
            tool(&server, dir.path(), "Bash", json!({"command": command})).await,
            "",
        );
    }
    denied(
        tool(&server, dir.path(), "Execute", json!({"command": "  "})).await,
        "missing command",
    );
    denied(
        tool(&server, dir.path(), "Execute", json!({})).await,
        "missing command",
    );
    for name in [
        "draft_order",
        "mcp__acan__draft_order",
        "acan___draft_order",
    ] {
        allowed(tool(&server, dir.path(), name, json!({})).await);
    }
    for name in [
        "mcp__acan__approve_order",
        "mcp__other__draft_order",
        "UnknownTool",
    ] {
        denied(
            tool(&server, dir.path(), name, json!({})).await,
            "not allowed",
        );
    }
    registry.unregister_order(ORDER);
    register(&registry, ORDER, dir.path(), "src/", false);
    denied(
        tool(
            &server,
            dir.path(),
            "Execute",
            json!({"command": "cargo test"}),
        )
        .await,
        "shell tools are not allowed",
    );
    server.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn invalid_wire_lines_are_denied_and_do_not_stop_the_listener() {
    let dir = TempDir::new().expect("worktree");
    let registry = HookRegistry::new();
    register(&registry, ORDER, dir.path(), "src/", false);
    let server = HookServer::bind_loopback(0, registry)
        .await
        .expect("listen");
    for (line, reason) in [
        ("{not json}\n", "bad hook input"),
        (
            "[1,\"session_start\",\"order-1\"]\n",
            "expected a JSON object",
        ),
        (
            "{\"version\":2,\"kind\":\"session_start\",\"order\":\"order-1\"}\n",
            "protocol version 2",
        ),
        (
            "{\"kind\":\"session_start\",\"order\":\"order-1\"}\n",
            "missing field `version`",
        ),
        (
            "{\"version\":1,\"kind\":\"unknown\",\"order\":\"order-1\"}\n",
            "unknown variant",
        ),
        (
            "{\"version\":1,\"kind\":\"session_start\",\"order\":\"order-1\",\"extra\":null}\n",
            "unknown field `extra`",
        ),
        (
            "{\"version\":1,\"kind\":\"pre_tool_use\",\"order\":\"order-1\",\"tool_name\":\"Read\"}\n",
            "raw_tool_input must be an object",
        ),
        (
            "{\"version\":1,\"kind\":\"pre_tool_use\",\"order\":\"order-1\",\"tool_name\":\"Read\",\"raw_tool_input\":null}\n",
            "raw_tool_input must be an object",
        ),
        (
            "{\"version\":1,\"kind\":\"pre_tool_use\",\"order\":\"order-1\",\"tool_name\":\"Read\",\"raw_tool_input\":[]}\n",
            "raw_tool_input must be an object",
        ),
        (
            "{\"version\":1,\"version\":1,\"kind\":\"session_start\",\"order\":\"order-1\"}\n",
            "duplicate key",
        ),
        (
            concat!(
                "{\"version\":1,\"kind\":\"pre_tool_use\",\"order\":\"order-1\",\"tool_name\":\"Read\",",
                "\"raw_tool_input\":{\"extra\":[{\"path\":\"a\",\"path\":\"b\"}]}}\n"
            ),
            "duplicate key",
        ),
        (
            "{\"version\":1,\"kind\":\"pre_tool_use\",\"order\":\"order-1\",\"tool_name\":\" \"}\n",
            "missing tool_name",
        ),
    ] {
        denied(exchange(&server, line).await, reason);
    }
    for size in [MAX_REQUEST_BYTES, MAX_REQUEST_BYTES * 4] {
        let mut large = vec![b' '; size];
        large.push(b'\n');
        denied(exchange(&server, large).await, "request line is too large");
    }
    allowed(tool(&server, dir.path(), "Read", json!({})).await);
    server.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn wait_reports_timeout_and_unregister() {
    let dir = TempDir::new().expect("worktree");
    let registry = HookRegistry::new();
    register(&registry, ORDER, dir.path(), "src/", false);
    assert!(matches!(
        registry.wait_for_heartbeat(ORDER, Duration::ZERO).await,
        Err(HookProbeError::Timeout)
    ));
    let (result, ()) = tokio::join!(biased;
        registry.wait_for_heartbeat(ORDER, WAIT),
        async { registry.unregister_order(ORDER) },
    );
    assert!(matches!(result, Err(HookProbeError::UnknownOrder)));
}

/// Cargo exposes the executable path for binaries of this package only. Ask Cargo for the hook
/// artifact so this test also works by itself on a clean build and with a custom target directory.
async fn hook_binary() -> PathBuf {
    let result = tokio::process::Command::new(env!("CARGO"))
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args([
            "build",
            "-p",
            "acan-hook",
            "--bin",
            "acan-hook",
            "--locked",
            "--message-format=json",
        ])
        .output()
        .await
        .expect("build hook executable");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8_lossy(&result.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find_map(|message| {
            (message["reason"] == "compiler-artifact" && message["target"]["name"] == "acan-hook")
                .then(|| message["executable"].as_str().map(PathBuf::from))
                .flatten()
        })
        .expect("Cargo reports the hook executable")
}

#[tokio::test]
async fn first_run_probe_proves_both_decisions_with_the_real_hook() {
    let hook = hook_binary().await;
    let dir = TempDir::new().expect("worktree");
    let active_dir = TempDir::new().expect("unrelated real order's worktree");
    let registry = HookRegistry::new();
    register(&registry, ORDER, active_dir.path(), "src/", false);
    let server = HookServer::bind_loopback(0, registry.clone())
        .await
        .expect("listen");
    let probe = FirstRunHookProbe::new(registry.clone(), dir.path()).expect("register probe");
    let order_id = probe.order_id().to_owned();
    allowed(
        tool(
            &server,
            active_dir.path(),
            "Edit",
            json!({"file_path": "src/a.rs"}),
        )
        .await,
    );
    denied(
        ask(
            &server,
            &order_id,
            json!({
                "hook_event_name": "PreToolUse", "tool_name": "Edit", "cwd": dir.path(),
                "tool_input": {"file_path": "src/a.rs"},
            }),
        )
        .await,
        "probe orders cannot write",
    );
    let report = probe
        .run(&hook, server.endpoint(), WAIT)
        .await
        .expect("both checks pass");
    assert_eq!(report.heartbeat.order_id, order_id);
    assert_eq!(report.session_start_exit_code, 0);
    assert_eq!(report.pre_tool_use_exit_code, acan_hook::BLOCK_EXIT_CODE);
    denied(
        ask(
            &server,
            &order_id,
            json!({"hook_event_name": "SessionStart"}),
        )
        .await,
        "unknown order",
    );

    let probe = FirstRunHookProbe::new(registry.clone(), dir.path()).expect("register probe");
    let order_id = probe.order_id().to_owned();
    assert!(
        probe
            .run(&dir.path().join("missing-hook"), server.endpoint(), WAIT)
            .await
            .is_err()
    );
    denied(
        ask(
            &server,
            &order_id,
            json!({"hook_event_name": "SessionStart"}),
        )
        .await,
        "unknown order",
    );

    // Forward SessionStart to the real registry, but have a fake daemon return a different
    // denial at PreToolUse. An actual hook exit 2 with the wrong reason must not pass.
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    let proxy = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("proxy");
    let endpoint = format!("tcp://{}", proxy.local_addr().expect("proxy address"));
    let probe = FirstRunHookProbe::new(registry.clone(), dir.path()).expect("register probe");
    let answer = async {
        let (stream, _) = proxy.accept().await.expect("SessionStart connection");
        let mut stream = BufReader::new(stream);
        let mut line = String::new();
        stream
            .read_line(&mut line)
            .await
            .expect("read heartbeat request");
        allowed(exchange(&server, line).await);
        stream
            .write_all(b"{\"version\":1,\"decision\":\"allow\"}\n")
            .await
            .expect("allow heartbeat");
        drop(stream);

        let (stream, _) = proxy.accept().await.expect("PreToolUse connection");
        let mut stream = BufReader::new(stream);
        let mut line = String::new();
        stream
            .read_line(&mut line)
            .await
            .expect("read PreToolUse request");
        stream.write_all(b"{\"version\":1,\"decision\":\"deny\",\"reason\":\".acan/world.db: protected path (other policy)\"}\n").await.expect("send wrong denial");
    };
    let (result, ()) = tokio::time::timeout(WAIT, async {
        tokio::join!(probe.run(&hook, &endpoint, WAIT), answer)
    })
    .await
    .expect("proxy and probe completed");
    assert!(
        matches!(result, Err(HookProbeError::MissingDenial)),
        "{result:?}"
    );
    server.shutdown().await.expect("shutdown");
}

#[test]
fn plugin_install_round_trips_and_verify_rejects_tampering() {
    let dir = TempDir::new().expect("fixture");
    let root = dir.path().join("acan-plugin");
    let hook_bin = dir
        .path()
        .join("path with spaces")
        .join(format!("acan-hook{}", std::env::consts::EXE_SUFFIX));
    let config = PluginConfig {
        hook_bin: hook_bin.clone(),
        mcp_url: Some("http://127.0.0.1:4777/mcp".to_owned()),
        version: "1.0.0".to_owned(),
    };
    let bundle = install_plugin_bundle(&root, &config).expect("install plugin");
    assert_eq!(bundle.manifest, root.join(".factory-plugin/plugin.json"));
    assert_eq!(bundle.hooks, root.join("hooks/hooks.json"));
    let mcp = bundle.mcp.as_ref().expect("MCP file");
    assert_eq!(*mcp, root.join(".mcp.json"));
    let load = |path: &Path| -> Value {
        serde_json::from_slice(&fs::read(path).expect("read plugin file")).expect("valid JSON")
    };
    assert_eq!(load(&bundle.manifest)["name"], "acan");
    assert_eq!(load(&bundle.manifest)["version"], "1.0.0");
    assert_eq!(load(mcp)["mcpServers"]["acan"]["type"], "http");
    assert_eq!(
        load(mcp)["mcpServers"]["acan"]["url"],
        "http://127.0.0.1:4777/mcp"
    );
    let mut hooks = load(&bundle.hooks);
    let quote = if cfg!(windows) { '"' } else { '\'' };
    let command = format!("{quote}{}{quote}", hook_bin.display());
    for event in ["SessionStart", "PreToolUse"] {
        assert_eq!(hooks["hooks"][event][0]["matcher"], ".*");
        assert_eq!(hooks["hooks"][event][0]["hooks"][0]["command"], command);
    }
    assert_eq!(
        hooks["hooks"]["PreToolUse"][0]["hooks"][0]["onFailure"],
        "block"
    );
    verify_plugin_bundle(&root, &hook_bin).expect("verify plugin");
    assert_eq!(
        install_plugin_bundle(&root, &config).expect("reinstall"),
        bundle
    );
    assert!(verify_plugin_bundle(&root, &dir.path().join("another-hook")).is_err());

    hooks["hooks"]["PreToolUse"][0]["matcher"] = json!("Read");
    fs::write(&bundle.hooks, hooks.to_string()).expect("tamper with matcher");
    assert!(verify_plugin_bundle(&root, &hook_bin).is_err());
    fs::write(&bundle.hooks, r#"{"hooks":{},"hooks":{}}"#).expect("duplicate key");
    assert!(verify_plugin_bundle(&root, &hook_bin).is_err());

    let without_mcp = PluginConfig {
        mcp_url: None,
        ..config.clone()
    };
    let new_bundle = install_plugin_bundle(&root, &without_mcp).expect("reinstall without MCP");
    assert_eq!(new_bundle.mcp, None);
    assert!(!mcp.exists());
    verify_plugin_bundle(&root, &hook_bin).expect("verify bundle without MCP");
    for hook_bin in [
        PathBuf::from("relative-hook"),
        dir.path().join("hook;bad"),
        dir.path().join("hook\nbad"),
    ] {
        assert!(matches!(
            install_plugin_bundle(
                &root,
                &PluginConfig {
                    hook_bin,
                    ..config.clone()
                }
            ),
            Err(PluginError::BadHookPath)
        ));
    }
    verify_plugin_bundle(&root, &hook_bin).expect("bad install did not modify plugin files");
}

#[cfg(unix)]
#[test]
fn plugin_keeps_private_modes_and_refuses_symlink_directories() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let dir = TempDir::new().expect("fixture");
    let root = dir.path().join("plugin");
    let config = PluginConfig {
        hook_bin: dir.path().join("acan-hook"),
        mcp_url: Some("http://localhost:4777/mcp".to_owned()),
        version: "1.0.0".to_owned(),
    };
    let bundle = install_plugin_bundle(&root, &config).expect("install");
    for path in [&root, &root.join(".factory-plugin"), &root.join("hooks")] {
        assert_eq!(
            fs::metadata(path).expect("directory").permissions().mode() & 0o777,
            0o700
        );
    }
    for path in [
        &bundle.manifest,
        &bundle.hooks,
        bundle.mcp.as_ref().expect("MCP file"),
    ] {
        assert_eq!(
            fs::metadata(path).expect("file").permissions().mode() & 0o777,
            0o600
        );
    }
    let linked = dir.path().join("linked");
    fs::create_dir(&linked).expect("second plugin root");
    symlink(root.join("hooks"), linked.join("hooks")).expect("symlinked hooks dir");
    assert!(install_plugin_bundle(&linked, &config).is_err());
}

#[cfg(unix)]
#[test]
fn plugin_verify_refuses_unsafe_permissions_and_symlinks() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let dir = TempDir::new().expect("fixture");
    let root = dir.path().join("plugin");
    let config = PluginConfig {
        hook_bin: dir.path().join("acan-hook"),
        mcp_url: Some("http://localhost:4777/mcp".to_owned()),
        version: "1.0.0".to_owned(),
    };
    let bundle = install_plugin_bundle(&root, &config).expect("install");
    let backup = dir.path().join("backup");
    for path in [
        &root,
        &root.join(".factory-plugin"),
        &root.join("hooks"),
        &bundle.manifest,
        &bundle.hooks,
        bundle.mcp.as_ref().expect("MCP file"),
    ] {
        let readable = if path.is_dir() { 0o755 } else { 0o644 };
        for writable in [0o020, 0o002] {
            fs::set_permissions(path, fs::Permissions::from_mode(readable | writable))
                .expect("writable fixture");
            let err = verify_plugin_bundle(&root, &config.hook_bin).expect_err("unsafe mode");
            assert!(err.to_string().contains("group/world-writable"), "{err}");
        }
        fs::set_permissions(path, fs::Permissions::from_mode(readable)).expect("safe mode");
        verify_plugin_bundle(&root, &config.hook_bin).expect("readable mode is safe");

        fs::rename(path, &backup).expect("move actual bundle entry");
        symlink(&backup, path).expect("replace bundle entry with symlink");
        assert!(verify_plugin_bundle(&root, &config.hook_bin).is_err());
        if path == &root {
            assert!(verify_plugin_bundle(&root.join("."), &config.hook_bin).is_err());
        }
        fs::remove_file(path).expect("remove symlink");
        fs::rename(&backup, path).expect("restore actual entry");
        verify_plugin_bundle(&root, &config.hook_bin).expect("restored bundle verifies");
    }
}

#[cfg(windows)]
#[test]
fn plugin_accepts_only_a_drive_letter_verbatim_prefix() {
    let dir = TempDir::new().expect("fixture");
    let root = dir.path().join("plugin");
    let config = PluginConfig {
        hook_bin: PathBuf::from(r"\\?\C:\Program Files\Acan\acan-hook.exe"),
        mcp_url: None,
        version: "1.0.0".to_owned(),
    };
    let bundle = install_plugin_bundle(&root, &config).expect("install a verbatim drive path");
    verify_plugin_bundle(&root, &config.hook_bin).expect("verify a verbatim drive path");
    let hooks: Value =
        serde_json::from_slice(&fs::read(bundle.hooks).expect("read hooks")).expect("parse hooks");
    assert_eq!(
        hooks
            .pointer("/hooks/PreToolUse/0/hooks/0/command")
            .and_then(Value::as_str),
        Some(r#""\\?\C:\Program Files\Acan\acan-hook.exe""#)
    );
    for invalid in [
        r"\\?\UNC\server\share\acan-hook.exe",
        r"\\?\C:\bad?\acan-hook.exe",
        r"C:\bad?\acan-hook.exe",
        r"\\?\C:acan-hook.exe",
    ] {
        assert!(
            matches!(
                verify_plugin_bundle(&root, Path::new(invalid)),
                Err(PluginError::BadHookPath)
            ),
            "{invalid:?}"
        );
    }
}
