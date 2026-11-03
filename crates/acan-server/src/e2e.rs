//! The loop end to end on a real git repository, on the scope-first lane. An order is posted over
//! HTTP, the owner revises and confirms its scope, the agent reads the scope back, claims, builds
//! in its task worktree, and the final gate judges the build against the acan-demo app in Docker
//! before anything lands. The offline split stands in for the foreman. A scripted agent drives
//! the worker side.
//!
//! The agent sends its commands through the writer, because agents cannot send commands over
//! HTTP yet. Everything else goes through the public surface: member commands over HTTP, the
//! agent's worktree route over HTTP, and the stored log. The gate runs the demo app in Docker, so
//! the tests need a checkout of acan-demo at its pinned commit in `ACAN_DEMO_DIR`. They only read it.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::Duration;

use acan_core::{
    AgentCmd, AgentId, Command, CommitId, DeclarationInput, Event, Fence, FilePath, Generation,
    HumanCmd, LEASE_TTL_MS, LaneVerdict, MachineKey, MemberId, OrderId, Origin, ScopeBinding,
    Stage, Stamped, TaskId, TaskState, World,
};
use acan_intent::{
    ContrastExample, EpisodeId, ItemClass, LedgerEvent, RulingStatus, Scope, ScopeConfirmation,
    Supersedes,
};
use acan_llm::{
    CallRecord, ChatRequest, ChatResponse, LlmError, ModelClient, Role, Usage, UsageLedger,
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;

use crate::model::{self, Ask};
use crate::offline::Offline;
use crate::projects::{ProjectSet, ProjectSpec};
use crate::{
    DEFAULT_WITNESS_CONCURRENCY, Running, ServeConfig, ServerConfig, start, start_projects,
};

const OWNER_TOKEN: &str = "acan_owner_e2e";
const AGENT_TOKEN: &str = "acan_agent_e2e";
/// A member who posts an order and then rules in it without being an owner.
const REVIEWER_TOKEN: &str = "acan_reviewer_e2e";
const REVIEWER: &str = "reviewer";
/// A member who neither owns nor posted the orders the tests build.
const STRANGER_TOKEN: &str = "acan_stranger_e2e";
const AGENT: &str = "agent-1";
/// The machine the order is pinned on. The demo's signup rule reaches the auth and store machines.
const MACHINE: &str = "auth";
const LANDED: &str = "landed.txt";
const AGENT_CONTENT: &str = "written by the agent\n";
/// The environment variable that names the acan-demo checkout. The tests only read it.
const DEMO_DIR_VAR: &str = "ACAN_DEMO_DIR";
/// The acan-demo commit the protected branch is pinned to.
const DEMO_COMMIT: &str = "f91962156841332d5753d869554839f7c4cdea79";
const PROTECTED_BRANCH: &str = "main";
/// The must-keep rule of the scope: the signup answer keeps its body.
const SIGNUP_RULE: &str = "k-signup-body";
const SIGNUP_OBLIGATION: &str = "obl-signup-body";
/// The signup handler's answer, as the pinned demo has it.
const SIGNUP_RESPONSE: &str = "return { id: user.id, username: user.username };";
/// The seeded regression: the answer upper-cases the username, so the answer differs from base.
const REGRESSED_SIGNUP_RESPONSE: &str =
    "return { id: user.id, username: user.username.toUpperCase() };";
/// A gate run builds and runs the demo app in Docker, about a minute per candidate, and a chain
/// may judge two candidates. The budget is generous, and it stays finite so a real stall fails.
const WAIT: Duration = Duration::from_secs(300);
const POLL: Duration = Duration::from_millis(20);
/// A sixth of the lease, so the test agent renews well before a lapse and can miss a few.
const LEASE_RENEWAL: Duration = Duration::from_millis(LEASE_TTL_MS / 6);
/// Serializes the tests that run the demo app in Docker, so their containers do not compete for
/// the same CPU budget and their gates stay inside their budgets.
static DOCKER: Mutex<()> = Mutex::const_new(());

#[tokio::test]
async fn an_order_is_gated_in_its_worktree_and_lands_on_the_protected_branch() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let root = repo.path();
    let running = start_plant(root).await;
    let status_before = git(root, &["status", "--porcelain"]);
    let task = TaskId::from_str("t-0").expect("valid task id");

    post_order(&running).await;
    let scope = scope_first(&running, root, task).await;

    let before_claim = worktree_route(&running, AGENT_TOKEN, task).await;
    assert_eq!(
        before_claim.status(),
        reqwest::StatusCode::FORBIDDEN,
        "an agent without the lease is not told where the worktree is"
    );
    let fence = claim(&running, task).await;
    let worktree = worktree_dir(root, task);
    wait_for_worktree(root, task, fence.generation).await;

    let response = worktree_route(&running, AGENT_TOKEN, task).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("the route answers JSON");
    let told = PathBuf::from(body["path"].as_str().expect("the route names the path"));
    assert_eq!(
        canonical(&told),
        canonical(&worktree),
        "the lease holder is told the worktree the loop created"
    );
    assert_eq!(body["branch"], "acan/t-0");
    let owner_view = worktree_route(&running, OWNER_TOKEN, task).await;
    assert_eq!(
        owner_view.status(),
        reqwest::StatusCode::FORBIDDEN,
        "a member token does not read a worktree"
    );
    assert_eq!(
        scope.rev.0, 1,
        "the agent read the confirmed first revision"
    );

    let built = build(&running, root, task, fence, LANDED, AGENT_CONTENT).await;
    settle(&running, "the task to land", |_, world| {
        in_state(world, task, |state| {
            matches!(state, TaskState::Landed { .. })
        })
        .then_some(())
    })
    .await;

    let (log, _) = snapshot(&running).await;
    assert!(
        log.iter().any(|stamped| matches!(
            &stamped.event,
            Event::ReceiptRecorded { record, .. } if record.verdict == LaneVerdict::Passed
        )),
        "the final gate passed the built commit"
    );
    assert_eq!(
        git(root, &["show", "main:landed.txt"]),
        AGENT_CONTENT,
        "the protected branch carries the agent's file"
    );
    assert_eq!(
        git(root, &["rev-parse", "main"]).trim(),
        built.to_string(),
        "the task branch fast-forwarded the protected branch"
    );
    assert_eq!(
        git(root, &["status", "--porcelain"]),
        status_before,
        "the user's checkout status is unchanged by the land"
    );
    assert!(
        !root.join(LANDED).exists(),
        "the land never writes the user's working tree"
    );
    running.stop().await.expect("the server stops");
}

/// A land in one project moves that project's branch and nothing else. The second project is a
/// plain repository on the same server, and its branch, working tree, and worktrees stay as they
/// were. The land runs the same flow as the test above, through the default project.
#[tokio::test]
async fn a_land_in_one_project_moves_only_its_branch() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let root = repo.path();
    let plain = plain_repo();
    let plain_root = plain.path();
    let plain_head = git(plain_root, &["rev-parse", PROTECTED_BRANCH]);
    let plain_status = git(plain_root, &["status", "--porcelain"]);
    let running = start_pair(root, plain_root).await;
    let task = TaskId::from_str("t-0").expect("valid task id");

    post_order(&running).await;
    let _scope = scope_first(&running, root, task).await;
    let fence = claim(&running, task).await;
    let worktree = worktree_dir(root, task);
    wait_for_worktree(root, task, fence.generation).await;
    assert!(
        worktree.is_dir(),
        "the claim's worktree is in the demo project's state folder"
    );
    let built = build(&running, root, task, fence, LANDED, AGENT_CONTENT).await;
    settle(&running, "the task to land", |_, world| {
        in_state(world, task, |state| {
            matches!(state, TaskState::Landed { .. })
        })
        .then_some(())
    })
    .await;
    assert_eq!(
        git(root, &["rev-parse", PROTECTED_BRANCH]).trim(),
        built.to_string(),
        "the demo project's branch moved to the built commit"
    );

    assert_eq!(
        git(plain_root, &["rev-parse", PROTECTED_BRANCH]),
        plain_head,
        "the plain project's branch did not move"
    );
    assert_eq!(
        git(plain_root, &["status", "--porcelain"]),
        plain_status,
        "the plain project's working tree did not change"
    );
    assert!(
        !plain_root.join(".acan").join("worktrees").exists(),
        "the plain project has no worktree folder"
    );
    let plain_worktrees = git(plain_root, &["worktree", "list", "--porcelain"]);
    assert!(
        !plain_worktrees.contains(&root.display().to_string()),
        "the plain project's git lists no worktree of the demo project: {plain_worktrees}"
    );
    running.stop().await.expect("the server stops");
}

/// Two projects whose worlds both hold `t-0`, from one order each. An agent claims `t-0` through
/// the first project's MCP endpoint, and only that project changes. The same token gets 401 on the
/// second project's MCP endpoint, since the second project holds no agent token.
#[tokio::test]
async fn same_ids_do_not_collide() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let root = repo.path();
    let other = demo_repo();
    let other_root = other.path();
    write_owner_tokens(other_root);
    let running = start_pair(root, other_root).await;
    let task = TaskId::from_str("t-0").expect("valid task id");

    member_at(
        &running,
        "/api/p/plain/commands",
        &HumanCmd::PostOrder {
            machine: machine(),
            sentence: "land the agent's file".to_owned(),
            witnesses: Vec::new(),
        },
    )
    .await;
    settle_at(
        &running,
        "/api/p/plain/events",
        "the offline split in plain",
        |_, world| world.task(task).map(|found| found.order),
    )
    .await;
    post_order(&running).await;
    scope_first(&running, root, task).await;
    let (plain_before, _) = snapshot_at(&running, "/api/p/plain/events").await;

    let (status, body) = rpc_tool(
        &running,
        "/api/p/demo/mcp",
        AGENT_TOKEN,
        "claim_task",
        json!({ "task": "t-0" }),
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "demo's agent claims t-0: {body}"
    );
    settle(&running, "the claim in demo", |_, world| {
        in_state(world, task, |state| {
            matches!(state, TaskState::Claimed { .. })
        })
        .then_some(())
    })
    .await;

    let (plain_after, plain_origin) = snapshot_at(&running, "/api/p/plain/events").await;
    assert_eq!(
        serde_json::to_value(&plain_after).expect("the log serializes"),
        serde_json::to_value(&plain_before).expect("the log serializes"),
        "plain's log did not change"
    );
    assert!(
        !in_state(plain_origin.world(), task, |state| {
            matches!(state, TaskState::Claimed { .. })
        }),
        "plain's t-0 is not claimed"
    );

    let (status, _) = rpc_tool(
        &running,
        "/api/p/plain/mcp",
        AGENT_TOKEN,
        "claim_task",
        json!({ "task": "t-0" }),
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::UNAUTHORIZED,
        "demo's agent token gets 401 on plain's MCP endpoint"
    );
    running.stop().await.expect("the server stops");
}

/// The snapshot a worktree names holds the protected head's `.acan` files, prepared the way the
/// gate prepares its base. A candidate's own edit to `.acan` in its worktree never reaches it, over
/// HTTP or over MCP, and a move of the protected head names a new snapshot.
#[tokio::test]
async fn a_worktree_names_a_snapshot_of_the_protected_head_that_a_candidate_cannot_change() {
    const PROTECTED_APP: &str = ".acan/snapshot-check/app.json";
    const PROTECTED_CORPUS: &str = ".acan/snapshot-check/core.json";
    const APP_BASE: &str = "{\"app\": \"base\"}\n";
    const CORPUS_BASE: &str = "{\"probes\": [\"base\"]}\n";
    const APP_CANDIDATE: &str = "{\"app\": \"candidate\"}\n";
    const APP_MOVED: &str = "{\"app\": \"moved\"}\n";
    let repo = demo_repo();
    let root = repo.path();
    commit_on_protected(root, PROTECTED_APP, APP_BASE);
    commit_on_protected(root, PROTECTED_CORPUS, CORPUS_BASE);
    let running = start_plant(root).await;
    let task = TaskId::from_str("t-0").expect("valid task id");

    post_order(&running).await;
    scope_first(&running, root, task).await;
    let fence = claim(&running, task).await;
    wait_for_worktree(root, task, fence.generation).await;
    let worktree = worktree_dir(root, task);

    let first = worktree_view(&running, task).await;
    let snapshot = PathBuf::from(
        first["snapshot"]
            .as_str()
            .expect("the view names a snapshot"),
    );
    assert_eq!(
        read_text(&snapshot.join(PROTECTED_APP)),
        APP_BASE,
        "the snapshot holds the protected head's app contract"
    );
    assert_eq!(
        read_text(&snapshot.join(PROTECTED_CORPUS)),
        CORPUS_BASE,
        "the snapshot holds the protected head's probe corpus"
    );
    assert!(
        !snapshot.join(MACHINE).exists(),
        "the snapshot holds protected paths only"
    );

    commit_file(&worktree, PROTECTED_APP, APP_CANDIDATE);
    assert_eq!(
        read_text(&worktree.join(PROTECTED_APP)),
        APP_CANDIDATE,
        "the candidate edited .acan in its worktree"
    );
    let again = worktree_view(&running, task).await;
    assert_eq!(
        again["snapshot"], first["snapshot"],
        "the same protected head names the same snapshot"
    );
    assert_eq!(
        read_text(&snapshot.join(PROTECTED_APP)),
        APP_BASE,
        "the candidate's edit is not in the snapshot"
    );
    let over_mcp = mcp_worktree(&running, task).await;
    assert_eq!(
        over_mcp["snapshot"], first["snapshot"],
        "MCP names the same snapshot as HTTP"
    );

    commit_on_protected(root, PROTECTED_APP, APP_MOVED);
    let moved = worktree_view(&running, task).await;
    let moved_snapshot = PathBuf::from(
        moved["snapshot"]
            .as_str()
            .expect("the view names a snapshot"),
    );
    assert_ne!(
        moved_snapshot, snapshot,
        "a moved protected head names a new snapshot"
    );
    assert_eq!(
        read_text(&moved_snapshot.join(PROTECTED_APP)),
        APP_MOVED,
        "the new snapshot holds the moved protected head"
    );
    assert_eq!(
        read_text(&snapshot.join(PROTECTED_APP)),
        APP_BASE,
        "the earlier snapshot stays as it was"
    );
    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_failing_gate_sends_the_task_back_to_building_and_lands_nothing() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let root = repo.path();
    let running = start_plant(root).await;
    let task = TaskId::from_str("t-0").expect("valid task id");
    post_order(&running).await;
    scope_first(&running, root, task).await;
    let fence = claim(&running, task).await;
    wait_for_worktree(root, task, fence.generation).await;
    let protected_before = git(root, &["rev-parse", "main"]);

    build_with(
        &running,
        root,
        task,
        fence,
        &["src/auth.ts"],
        seed_regression,
    )
    .await;
    let record = settle(&running, "the gate to block the regression", |log, _| {
        log.iter().find_map(|stamped| match &stamped.event {
            Event::ReceiptRecorded { record, .. }
                if matches!(record.verdict, LaneVerdict::Blocked { .. }) =>
            {
                Some(record.clone())
            }
            _ => None,
        })
    })
    .await;
    let LaneVerdict::Blocked { blockers } = &record.verdict else {
        panic!("the regression is blocked: {:?}", record.verdict);
    };
    assert!(
        serde_json::to_string(blockers)
            .expect("the blockers serialize")
            .contains(SIGNUP_RULE),
        "the receipt names the signup rule the regression breaks: {blockers:?}"
    );
    assert!(
        record.receipt.items.iter().any(|item| {
            item.item_id.as_str() == SIGNUP_RULE && item.class == ItemClass::ObligationFailed
        }),
        "the receipt marks the signup rule's obligation as failed: {:?}",
        record.receipt.items
    );
    settle(&running, "the task to return to building", |_, world| {
        in_state(world, task, |state| {
            matches!(
                state,
                TaskState::Claimed {
                    stage: Stage::Declared { .. } | Stage::Building { .. },
                    ..
                }
            )
        })
        .then_some(())
    })
    .await;

    let (log, _) = snapshot(&running).await;
    assert!(
        log.iter().any(|stamped| matches!(
            &stamped.event,
            Event::GateClosed {
                verdict: acan_core::GateVerdict::Failed { .. },
                ..
            }
        )),
        "the gate closed on a failure"
    );
    assert!(
        !log.iter().any(|stamped| matches!(
            &stamped.event,
            Event::LandFailed { .. } | Event::Landed { .. }
        )),
        "nothing reached the land queue"
    );
    assert_eq!(
        git(root, &["rev-parse", "main"]),
        protected_before,
        "the protected branch did not move"
    );
    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_conflicting_land_reports_the_files_and_reopens_the_task() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let root = repo.path();
    let running = start_plant(root).await;
    let task = TaskId::from_str("t-0").expect("valid task id");
    post_order(&running).await;
    scope_first(&running, root, task).await;
    let fence = claim(&running, task).await;
    wait_for_worktree(root, task, fence.generation).await;

    // The user lands a different version of the same file while the agent works.
    let user_commit = commit_on_protected(root, LANDED, "written by the user\n");
    build(&running, root, task, fence, LANDED, AGENT_CONTENT).await;

    let reason = settle(&running, "the land to fail", |log, _| {
        log.iter().find_map(|stamped| match &stamped.event {
            Event::LandFailed {
                task: failed,
                reason,
            } if *failed == task => Some(reason.clone()),
            _ => None,
        })
    })
    .await;
    assert!(
        reason.contains(LANDED),
        "the conflict names the file: {reason}"
    );
    settle(&running, "the task to reopen", |_, world| {
        in_state(world, task, |state| matches!(state, TaskState::Open)).then_some(())
    })
    .await;
    assert_eq!(
        git(root, &["rev-parse", "main"]).trim(),
        user_commit.to_string(),
        "a refused land leaves the protected branch at the user's commit"
    );
    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_land_onto_a_protected_branch_that_moved_merges_only_the_gated_commit() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let root = repo.path();
    let running = start_plant(root).await;
    let task = TaskId::from_str("t-0").expect("valid task id");
    post_order(&running).await;
    scope_first(&running, root, task).await;
    let fence = claim(&running, task).await;
    wait_for_worktree(root, task, fence.generation).await;

    // The user lands a different file while the agent works, so the land is a real merge.
    let user_commit = commit_on_protected(root, "user.txt", "written by the user\n");
    let built = build(&running, root, task, fence, LANDED, AGENT_CONTENT).await;
    settle(&running, "the task to land", |_, world| {
        in_state(world, task, |state| {
            matches!(state, TaskState::Landed { .. })
        })
        .then_some(())
    })
    .await;

    assert_eq!(
        git(root, &["rev-parse", "main^1"]).trim(),
        user_commit.to_string(),
        "the merge's first parent is the protected head it merged onto"
    );
    assert_eq!(
        git(root, &["rev-parse", "main^2"]).trim(),
        built.to_string(),
        "the merge's second parent is the gated commit"
    );
    assert_eq!(git(root, &["show", "main:landed.txt"]), AGENT_CONTENT);
    assert_eq!(
        git(root, &["show", "main:user.txt"]),
        "written by the user\n",
        "the user's file survives the land"
    );
    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_reclaim_after_a_refused_land_starts_from_the_protected_head_the_user_moved_to() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let root = repo.path();
    let running = start_plant(root).await;
    let task = TaskId::from_str("t-0").expect("valid task id");
    post_order(&running).await;
    scope_first(&running, root, task).await;
    let fence = claim(&running, task).await;
    wait_for_worktree(root, task, fence.generation).await;

    let user_commit = commit_on_protected(root, LANDED, "written by the user\n");
    build(&running, root, task, fence, LANDED, AGENT_CONTENT).await;
    settle(&running, "the land to fail", |log, _| {
        log.iter().find_map(|stamped| match &stamped.event {
            Event::LandFailed {
                task: failed,
                reason,
            } if *failed == task => Some(reason.clone()),
            _ => None,
        })
    })
    .await;
    settle(&running, "the task to reopen", |_, world| {
        in_state(world, task, |state| matches!(state, TaskState::Open)).then_some(())
    })
    .await;
    // The directory goes first and the branch after it, so both are awaited.
    settle(
        &running,
        "the reopened worktree and branch to be removed",
        |_, _| {
            (!worktree_dir(root, task).exists() && !branch_exists(root, "acan/t-0")).then_some(())
        },
    )
    .await;

    let fence = claim(&running, task).await;
    wait_for_worktree(root, task, fence.generation).await;
    assert_eq!(
        git(&worktree_dir(root, task), &["rev-parse", "HEAD"]).trim(),
        user_commit.to_string(),
        "the reclaimed worktree starts at the protected head the user moved to"
    );
    let built = build(&running, root, task, fence, LANDED, AGENT_CONTENT).await;
    settle(&running, "the reclaimed task to land", |_, world| {
        in_state(world, task, |state| {
            matches!(state, TaskState::Landed { .. })
        })
        .then_some(())
    })
    .await;
    assert_eq!(
        git(root, &["rev-parse", "main"]).trim(),
        built.to_string(),
        "the reclaimed task fast-forwards the protected branch"
    );
    assert_eq!(git(root, &["show", "main:landed.txt"]), AGENT_CONTENT);
    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_reclaim_without_waiting_for_cleanup_starts_at_the_protected_head() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let root = repo.path();
    let running = start_plant(root).await;
    let task = TaskId::from_str("t-0").expect("valid task id");
    post_order(&running).await;
    scope_first(&running, root, task).await;
    let fence = claim(&running, task).await;
    let worktree = worktree_dir(root, task);
    wait_for_worktree(root, task, fence.generation).await;
    let old = commit_file(&worktree, "old.txt", "from the old claim\n");
    let protected = git(root, &["rev-parse", "main"]);

    agent_command(&running, AgentCmd::Release { fence }).await;
    settle(&running, "the release to reopen the task", |_, world| {
        in_state(world, task, |state| matches!(state, TaskState::Open)).then_some(())
    })
    .await;
    // The old worktree must be gone before the route creates the new one, or the test's own git
    // and the loop's removal share the directory. The route then creates the new generation
    // without waiting for the loop, which is the cleanup the reclaim does not wait for.
    settle(&running, "the released worktree to be removed", |_, _| {
        (!worktree.exists() && !branch_exists(root, "acan/t-0")).then_some(())
    })
    .await;
    let fence = claim(&running, task).await;
    let response = worktree_route(&running, AGENT_TOKEN, task).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let head = git(&worktree, &["rev-parse", "HEAD"]);
    assert_eq!(
        head.trim(),
        protected.trim(),
        "the reclaimed worktree starts at the protected head"
    );
    assert_ne!(
        head.trim(),
        old.to_string(),
        "the old claim's commit is gone"
    );
    assert!(
        !worktree.join("old.txt").exists(),
        "the old claim's file is gone"
    );
    let history = git(&worktree, &["log", "--format=%H"]);
    assert!(
        !history.contains(&old.to_string()),
        "none of the old claim's commits remain"
    );

    let built = build(&running, root, task, fence, LANDED, AGENT_CONTENT).await;
    settle(&running, "the reclaimed task to land", |_, world| {
        in_state(world, task, |state| {
            matches!(state, TaskState::Landed { .. })
        })
        .then_some(())
    })
    .await;
    assert_eq!(
        git(root, &["rev-parse", "main"]).trim(),
        built.to_string(),
        "the reclaimed task fast-forwards the protected branch"
    );
    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_queued_task_keeps_its_branch_until_it_lands() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let root = repo.path();
    let hold = hold_protected_updates(root);
    let running = start_plant(root).await;
    let task = TaskId::from_str("t-0").expect("valid task id");
    post_order(&running).await;
    scope_first(&running, root, task).await;
    let fence = claim(&running, task).await;
    wait_for_worktree(root, task, fence.generation).await;
    let built = build(&running, root, task, fence, LANDED, AGENT_CONTENT).await;

    let generation = settle(&running, "the task to queue", |_, world| {
        match &world.task(task)?.state {
            TaskState::Queued { run, .. } => world.gate(*run).map(|gate| gate.generation.get()),
            _ => None,
        }
    })
    .await;
    // The land is held, so the task stays queued while its branch is checked. The branch is
    // still the one the gate passed, not a leftover the loop should remove.
    assert!(
        worktree_dir(root, task).is_dir() && branch_exists(root, "acan/t-0"),
        "the queued task keeps its worktree and branch"
    );
    assert_eq!(
        git_config(root, "branch.acan/t-0.acanGeneration").trim(),
        generation.to_string(),
        "the queued branch stays at its generation"
    );
    assert_eq!(
        git(&worktree_dir(root, task), &["rev-parse", "HEAD"]).trim(),
        built.to_string(),
        "the queued branch still holds the gated commit"
    );

    fs::remove_file(&hold).expect("the hold file is removed");
    settle(&running, "the queued task to land", |_, world| {
        in_state(world, task, |state| {
            matches!(state, TaskState::Landed { .. })
        })
        .then_some(())
    })
    .await;
    settle(&running, "the landed branch to be removed", |_, _| {
        (!worktree_dir(root, task).exists() && !branch_exists(root, "acan/t-0")).then_some(())
    })
    .await;
    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_serialized_waiter_builds_on_its_landed_predecessor_and_lands_cleanly() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let root = repo.path();
    let running = start_with(root, TwoTasks::default(), UsageLedger::default()).await;
    let first = TaskId::from_str("t-0").expect("valid task id");
    let second = TaskId::from_str("t-1").expect("valid task id");
    post_order(&running).await;
    let (_, scope) = order_ready(&running, root, first).await;
    read_back(&running, first, &scope).await;
    read_back(&running, second, &scope).await;
    let first_fence = claim(&running, first).await;
    let second_fence = claim(&running, second).await;
    wait_for_worktree(root, first, first_fence.generation).await;
    wait_for_worktree(root, second, second_fence.generation).await;
    let cut_at = git(root, &["rev-parse", "main"]);

    declare(&running, first_fence, &[LANDED]).await;
    declare(&running, second_fence, &[LANDED]).await;
    let clash = settle(&running, "the two declarations to clash", |_, world| {
        world.clashes().find(|c| c.blocking()).map(|c| c.id)
    })
    .await;
    let response = client()
        .post(url(&running, "/api/commands"))
        .bearer_auth(OWNER_TOKEN)
        .json(&HumanCmd::SettleClash {
            clash,
            choice: acan_core::Settlement::Serialize {
                first,
                then: second,
            },
        })
        .send()
        .await
        .expect("the settle request is sent");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::OK,
        "the clash settles"
    );

    wait_building(&running, first).await;
    let first_content = "written by the first task\n";
    let first_head = commit_file(&worktree_dir(root, first), LANDED, first_content);
    agent_command(
        &running,
        AgentCmd::ReportBuilt {
            fence: first_fence,
            head: first_head,
        },
    )
    .await;
    wait_building(&running, second).await;

    let landed_head = git(root, &["rev-parse", "main"]);
    assert_ne!(landed_head, cut_at, "the first task landed");
    let waiter = worktree_dir(root, second);
    assert_eq!(
        git(&waiter, &["rev-parse", "HEAD"]),
        landed_head,
        "the waiter's worktree starts from the protected head its predecessor landed on"
    );
    assert_eq!(
        // A checkout with core.autocrlf writes CRLF on Windows.
        fs::read_to_string(waiter.join(LANDED))
            .expect("the landed file is in the worktree")
            .replace('\r', ""),
        first_content,
        "the waiter sees its predecessor's change before it writes"
    );

    let second_content = "written by the first task\nand extended by the second\n";
    let second_head = commit_file(&waiter, LANDED, second_content);
    agent_command(
        &running,
        AgentCmd::ReportBuilt {
            fence: second_fence,
            head: second_head.clone(),
        },
    )
    .await;
    settle(&running, "the waiter to land", |_, world| {
        in_state(world, second, |state| {
            matches!(state, TaskState::Landed { .. })
        })
        .then_some(())
    })
    .await;

    let (log, _) = snapshot(&running).await;
    assert!(
        !log.iter()
            .any(|stamped| matches!(&stamped.event, Event::LandFailed { .. })),
        "neither land conflicted"
    );
    let passes = log
        .iter()
        .filter(|stamped| {
            matches!(
                &stamped.event,
                Event::ReceiptRecorded { record, .. } if record.verdict == LaneVerdict::Passed
            )
        })
        .count();
    assert_eq!(passes, 2, "each task's gate passed on its built commit");
    assert_eq!(
        git(root, &["rev-parse", "main"]).trim(),
        second_head.to_string(),
        "the waiter fast-forwarded the protected branch"
    );
    assert_eq!(git(root, &["show", "main:landed.txt"]), second_content);
    running.stop().await.expect("the server stops");
}

/// The file the touch test claims: one status type, and the two moves the code makes on it.
const STATUS_FILE: &str = "src/order_status.ts";
const STATUS_SOURCE: &str = "export type OrderStatus = 'open' | 'paid' | 'shipped';\n\nexport function settle(order: { status: OrderStatus }) {\n  switch (order.status) {\n    case 'open':\n      order.status = 'paid';\n      break;\n    case 'paid':\n      order.status = 'shipped';\n      break;\n  }\n}\n";

/// A claim's footprint reaches the live stream as one `transition_touched` frame per transition it
/// covers. A touch is not an event: it has no seq, and the stored log holds nothing for it.
#[tokio::test]
async fn a_claim_sends_a_live_touch_for_each_transition_it_covers() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let root = repo.path();
    // The scan reads the working tree, so the file must be in the checkout before the server starts.
    write_file(root, STATUS_FILE, STATUS_SOURCE);
    let running = start_plant(root).await;
    let task = TaskId::from_str("t-0").expect("valid task id");
    post_order(&running).await;
    scope_first(&running, root, task).await;

    // A member added after the stream connects reaches the stream. Once it does, the stream has
    // subscribed to touches, so the claim's touches cannot go out unseen.
    let mut frames = live_frames(&running, OWNER_TOKEN);
    member(
        &running,
        &HumanCmd::AddMember {
            member: MemberId::from_str("watcher").expect("valid member id"),
            role: acan_core::Role::Member,
        },
    )
    .await;
    frame_where(&mut frames, |frame| {
        frame["event"]["type"] == "member_added" && frame["event"]["member"]["id"] == "watcher"
    })
    .await;

    let fence = claim(&running, task).await;
    declare(&running, fence, &[STATUS_FILE]).await;

    // Two transitions: the move to `paid` on line 6 and the move to `shipped` on line 9.
    let mut touches = Vec::new();
    while touches.len() < 2 {
        touches.push(frame_where(&mut frames, |frame| frame["type"] == "transition_touched").await);
    }
    let lines: Vec<(u64, u64)> = touches
        .iter()
        .map(|touch| {
            (
                touch["line"].as_u64().expect("a line"),
                touch["end_line"].as_u64().expect("an end line"),
            )
        })
        .collect();
    assert_eq!(lines, vec![(6, 6), (9, 9)], "the touches: {touches:?}");
    for touch in &touches {
        assert!(
            touch.get("seq").is_none(),
            "a touch is not an event: {touch}"
        );
        assert_eq!(touch["machine"], "src/order_status.ts#OrderStatus");
        assert_eq!(touch["file"], STATUS_FILE);
        assert_eq!(touch["by"], "claim");
        assert_eq!(touch["stage"], "declared");
        assert_eq!(touch["task"], 0);
        assert!(touch["commit"].is_null(), "a claim has no commit: {touch}");
    }
    running.stop().await.expect("the server stops");
}

/// Streams the frames `token` reads from the live stream, from seq 0, in a task of its own. The
/// receiver yields each text frame as JSON, until the socket closes.
fn live_frames(running: &Running, token: &str) -> tokio::sync::mpsc::UnboundedReceiver<Value> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut request = format!("ws://{}/api/stream?from=0", running.local_addr)
        .into_client_request()
        .expect("the stream URL");
    let header = HeaderValue::from_str(&format!("Bearer {token}")).expect("the token header");
    request.headers_mut().insert(AUTHORIZATION, header);
    tokio::spawn(async move {
        let (mut socket, _response) = tokio_tungstenite::connect_async(request)
            .await
            .expect("the stream connects");
        while let Some(Ok(message)) = socket.next().await {
            if let WsMessage::Text(text) = message {
                let frame: Value = serde_json::from_str(&text).expect("a stream frame is JSON");
                if tx.send(frame).is_err() {
                    return;
                }
            }
        }
    });
    rx
}

/// The next frame of `frames` that `done` accepts, within [`WAIT`]. Frames it refuses are skipped.
async fn frame_where(
    frames: &mut tokio::sync::mpsc::UnboundedReceiver<Value>,
    done: impl Fn(&Value) -> bool,
) -> Value {
    timeout(WAIT, async {
        loop {
            let frame = frames.recv().await.expect("the stream stays open");
            if done(&frame) {
                return frame;
            }
        }
    })
    .await
    .expect("the stream sends the frame in time")
}

/// The scope the scripted foreman drafts for the signup order: one may-change item on the signup
/// route, in the shape the live foreman returns.
fn drafted_scope_body() -> Value {
    json!({
        "may": [{
            "id": "i-signup",
            "selector": {
                "surface": "http",
                "method": "POST",
                "route_template": "/api/signup",
                "step_context": { "kind": "step", "step_id": "s1" },
                "principal": { "kind": "anonymous" },
                "channel": "body"
            },
            "change": "add",
            "acceptance": [{
                "obligation_id": "obl-signup-may",
                "predicate": {
                    "predicate": "equals_base",
                    "step_id": "s1",
                    "path": "body",
                    "anchor": { "kind": "fresh" }
                },
                "text": "The signup answer's body matches the base run"
            }],
            "provenance": "model",
            "example": {
                "kind": "target",
                "step_id": "s1",
                "path": "status",
                "after": { "state": "present", "value": 201 }
            },
            "machine_ids": ["auth"]
        }],
        "must_keep": [],
        "open": []
    })
}

/// The offline models, except that a split makes two tasks and a scope request is answered with
/// `draft`. A stand-in for the live foreman in this test only: the job checks the reply the same
/// way it checks a live one.
struct Drafting {
    inner: TwoTasks,
    draft: Value,
    /// The user message of every scope request, as the foreman received it.
    asked: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl ModelClient for Drafting {
    async fn chat(&self, role: Role, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        if model::ask_of(&request) != Some(Ask::Scope) {
            return self.inner.chat(role, request).await;
        }
        self.asked
            .lock()
            .expect("the asked lock is not poisoned")
            .extend(request.messages.iter().filter_map(|message| match message {
                acan_llm::Message::User { content } => Some(content.clone()),
                _ => None,
            }));
        Ok(ChatResponse {
            content: Some(self.draft.to_string()),
            tool_calls: Vec::new(),
            finish_reason: None,
            call: CallRecord {
                role,
                provider: "scripted".to_owned(),
                model: "scripted-foreman".to_owned(),
                usage: Usage::default(),
                latency: Duration::ZERO,
            },
        })
    }
}

/// A foreman whose first scope reply is `refused` and whose later replies are `then`. Each
/// scope request's user message is recorded in `asked`, in the order the foreman received them.
struct RefusedFirst {
    inner: TwoTasks,
    refused: Value,
    then: Value,
    asked: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl ModelClient for RefusedFirst {
    async fn chat(&self, role: Role, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        if model::ask_of(&request) != Some(Ask::Scope) {
            return self.inner.chat(role, request).await;
        }
        let draft = {
            let mut asked = self.asked.lock().expect("the asked lock is not poisoned");
            asked.extend(request.messages.iter().filter_map(|message| match message {
                acan_llm::Message::User { content } => Some(content.clone()),
                _ => None,
            }));
            if asked.len() == 1 {
                &self.refused
            } else {
                &self.then
            }
            .clone()
        };
        Ok(ChatResponse {
            content: Some(draft.to_string()),
            tool_calls: Vec::new(),
            finish_reason: None,
            call: CallRecord {
                role,
                provider: "scripted".to_owned(),
                model: "scripted-foreman".to_owned(),
                usage: Usage::default(),
                latency: Duration::ZERO,
            },
        })
    }
}

/// Polls the latest scope draft of `order` until the draft job stores its outcome, and returns
/// the view. A failed draft is a stored outcome too, so the caller checks the state.
async fn scope_draft_of(running: &Running, order: OrderId) -> Value {
    timeout(WAIT, async {
        loop {
            let response = client()
                .get(url(running, &format!("/api/orders/{order}/scope-draft")))
                .bearer_auth(OWNER_TOKEN)
                .send()
                .await
                .expect("the draft request is sent");
            if response.status() == reqwest::StatusCode::OK {
                return response.json::<Value>().await.expect("the draft is JSON");
            }
            sleep(POLL).await;
        }
    })
    .await
    .expect("the scope draft is stored in time")
}

/// The scope the owner adopts from a stored draft: the proposal the model wrote, at the draft's
/// revision and base commit.
fn adopted_scope(draft: &Value) -> Scope {
    serde_json::from_value(json!({
        "version": "acan-scope/1",
        "order": draft["order"],
        "rev": draft["rev"],
        "base_commit": draft["base_commit"],
        "body": draft["draft"]["proposal"],
    }))
    .expect("the adopted scope decodes")
}

/// A scope draft is stored without authority. The owner sees it, and a confirmation before
/// adoption is refused. Adopting it (revising and confirming the scope) opens the lane, and only
/// then does the agent's readback and claim rest on it.
#[tokio::test]
async fn a_scope_draft_is_stored_without_authority_until_the_owner_adopts_it() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let root = repo.path();
    let foreman = Drafting {
        inner: TwoTasks::default(),
        draft: drafted_scope_body(),
        asked: std::sync::Arc::default(),
    };
    let running = start_with(root, foreman, UsageLedger::default()).await;
    let task = TaskId::from_str("t-0").expect("valid task id");
    post_order(&running).await;
    let order = settle(&running, "the order", |_, world| {
        world.orders().next().map(|found| found.id)
    })
    .await;
    let view = scope_draft_of(&running, order).await;
    assert_eq!(view["state"], "drafted", "the draft is stored: {view}");
    assert_eq!(
        view["rev"], 0,
        "the draft is for the order's first revision"
    );
    let draft = &view["draft"];
    assert_eq!(draft["draft"]["model"], "scripted-foreman");
    assert_eq!(draft["draft"]["provenance"], "model_proposal");
    let (_, origin) = snapshot(&running).await;
    assert!(
        origin.world().lane(order).is_none(),
        "a draft opens no lane"
    );
    let scope = adopted_scope(draft);
    let refused = client()
        .post(url(&running, "/api/commands"))
        .bearer_auth(OWNER_TOKEN)
        .json(&HumanCmd::ConfirmScope {
            order,
            confirmation: Box::new(confirmation_for(&scope)),
        })
        .send()
        .await
        .expect("the confirmation is sent");
    assert_eq!(
        refused.status(),
        reqwest::StatusCode::CONFLICT,
        "a draft cannot be confirmed before the owner adopts it"
    );
    settle(&running, "the split", |_, world| world.task(task).map(drop)).await;
    member(&running, &HumanCmd::RegisterAgent { agent: agent() }).await;
    member(
        &running,
        &HumanCmd::ReviseScope {
            order,
            scope: Box::new(scope.clone()),
        },
    )
    .await;
    member(
        &running,
        &HumanCmd::ConfirmScope {
            order,
            confirmation: Box::new(confirmation_for(&scope)),
        },
    )
    .await;
    read_back(&running, task, &scope).await;
    let _fence = claim(&running, task).await;
    let (log, _) = snapshot(&running).await;
    let drafted_at = log
        .iter()
        .position(|stamped| matches!(stamped.event, Event::ScopeDrafted { .. }));
    let revised_at = log
        .iter()
        .position(|stamped| matches!(stamped.event, Event::ScopeRevised { .. }));
    assert!(
        drafted_at.is_some() && drafted_at < revised_at,
        "the draft is stored before the owner adopts it"
    );
    running.stop().await.expect("the server stops");
}

/// With no model keys, the offline foreman's built-in draft carries an order through. The draft is
/// stored under the offline model and labelled built in, the owner adopts it, and the agent's build
/// is gated and lands. No scripted foreman stands in for the draft.
#[tokio::test]
async fn an_offline_scope_draft_carries_an_order_through_to_a_landed_build() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let root = repo.path();
    let running = start_plant(root).await;
    let task = TaskId::from_str("t-0").expect("valid task id");
    post_order(&running).await;
    let order = settle(&running, "the order", |_, world| {
        world.orders().next().map(|found| found.id)
    })
    .await;
    let view = scope_draft_of(&running, order).await;
    assert_eq!(
        view["state"], "drafted",
        "the built-in draft is stored: {view}"
    );
    let draft = &view["draft"];
    assert_eq!(draft["draft"]["model"], "offline");
    let text = &draft["draft"]["proposal"]["may"][0]["acceptance"][0]["text"];
    assert!(
        text.as_str()
            .is_some_and(|text| text.starts_with("Built-in offline draft")),
        "the draft says it is built in: {draft}"
    );
    let scope = adopted_scope(draft);
    settle(&running, "the split", |_, world| world.task(task).map(drop)).await;
    member(&running, &HumanCmd::RegisterAgent { agent: agent() }).await;
    member(
        &running,
        &HumanCmd::ReviseScope {
            order,
            scope: Box::new(scope.clone()),
        },
    )
    .await;
    member(
        &running,
        &HumanCmd::ConfirmScope {
            order,
            confirmation: Box::new(confirmation_for(&scope)),
        },
    )
    .await;
    read_back(&running, task, &scope).await;
    let fence = claim(&running, task).await;
    wait_for_worktree(root, task, fence.generation).await;
    let built = build(&running, root, task, fence, LANDED, AGENT_CONTENT).await;
    settle(&running, "the task to land", |_, world| {
        in_state(world, task, |state| {
            matches!(state, TaskState::Landed { .. })
        })
        .then_some(())
    })
    .await;

    let (log, _) = snapshot(&running).await;
    assert!(
        log.iter().any(|stamped| matches!(
            &stamped.event,
            Event::ReceiptRecorded { record, .. } if record.verdict == LaneVerdict::Passed
        )),
        "the final gate passed the built commit"
    );
    assert_eq!(
        git(root, &["rev-parse", "main"]).trim(),
        built.to_string(),
        "the task branch fast-forwarded the protected branch"
    );
    assert_eq!(git(root, &["show", "main:landed.txt"]), AGENT_CONTENT);
    running.stop().await.expect("the server stops");
}

/// A draft that never checks is recorded as a failure the owner can see. Nothing stands in for it,
/// so the lane stays closed.
#[tokio::test]
async fn a_scope_draft_that_never_checks_is_recorded_as_a_failure() {
    let repo = demo_repo();
    let root = repo.path();
    let asked = std::sync::Arc::<std::sync::Mutex<Vec<String>>>::default();
    let foreman = Drafting {
        inner: TwoTasks::default(),
        draft: json!({ "may": [], "must_keep": [], "open": [] }),
        asked: std::sync::Arc::clone(&asked),
    };
    let running = start_with(root, foreman, UsageLedger::default()).await;
    post_order(&running).await;
    let order = settle(&running, "the order", |_, world| {
        world.orders().next().map(|found| found.id)
    })
    .await;
    let view = scope_draft_of(&running, order).await;
    assert_eq!(view["state"], "failed", "the failure is stored: {view}");
    assert!(
        view["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("no may-change items")),
        "the reason names the check: {view}"
    );
    // The request the job sent carries the order's sentence, which is what the human asked for.
    // The order ref is only the draft's key, so it is not what the foreman drafts from.
    let sent = asked
        .lock()
        .expect("the asked lock is not poisoned")
        .clone();
    assert!(!sent.is_empty(), "the foreman was asked for a scope");
    for message in &sent {
        let context: Value =
            serde_json::from_str(acan_llm::outside::unfence(message).unwrap_or(message))
                .expect("the request is JSON");
        assert_eq!(
            context["order"], "land the agent's file",
            "the scope request names the order's sentence: {message}"
        );
    }
    let (_, origin) = snapshot(&running).await;
    assert!(origin.world().lane(order).is_none(), "no scope is opened");
    running.stop().await.expect("the server stops");
}

/// A draft refused for a side-effect selector without a field path is asked again, and the retry
/// carries the refusal. The second reply checks, so the draft is stored.
#[tokio::test]
async fn a_scope_draft_refused_for_a_side_effect_path_is_asked_again_with_the_refusal() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let root = repo.path();
    let mut refused = drafted_scope_body();
    refused["may"][0]["selector"]["channel"] = json!("side_effect");
    let asked = std::sync::Arc::<std::sync::Mutex<Vec<String>>>::default();
    let foreman = RefusedFirst {
        inner: TwoTasks::default(),
        refused,
        then: drafted_scope_body(),
        asked: std::sync::Arc::clone(&asked),
    };
    let running = start_with(root, foreman, UsageLedger::default()).await;
    post_order(&running).await;
    let order = settle(&running, "the order", |_, world| {
        world.orders().next().map(|found| found.id)
    })
    .await;
    let view = scope_draft_of(&running, order).await;
    assert_eq!(view["state"], "drafted", "the retry is stored: {view}");
    let sent = asked
        .lock()
        .expect("the asked lock is not poisoned")
        .clone();
    assert!(sent.len() >= 2, "the foreman was asked again: {sent:?}");
    let retry: Value =
        serde_json::from_str(acan_llm::outside::unfence(&sent[1]).unwrap_or(&sent[1]))
            .expect("the request is JSON");
    assert!(
        retry["previous_error"]
            .as_str()
            .is_some_and(|error| error.contains("selector is malformed")),
        "the retry carries the refusal: {retry}"
    );
    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_scope_draft_naming_an_undefined_step_is_asked_again_with_the_refusal() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let root = repo.path();
    let mut refused = drafted_scope_body();
    refused["may"][0]["selector"]["step_context"] =
        json!({ "kind": "step", "step_id": "login-step" });
    let asked = std::sync::Arc::<std::sync::Mutex<Vec<String>>>::default();
    let foreman = RefusedFirst {
        inner: TwoTasks::default(),
        refused,
        then: drafted_scope_body(),
        asked: std::sync::Arc::clone(&asked),
    };
    let running = start_with(root, foreman, UsageLedger::default()).await;
    post_order(&running).await;
    let order = settle(&running, "the order", |_, world| {
        world.orders().next().map(|found| found.id)
    })
    .await;
    let view = scope_draft_of(&running, order).await;
    assert_eq!(view["state"], "drafted", "the retry is stored: {view}");
    let sent = asked
        .lock()
        .expect("the asked lock is not poisoned")
        .clone();
    assert!(sent.len() >= 2, "the foreman was asked again: {sent:?}");
    let retry: Value =
        serde_json::from_str(acan_llm::outside::unfence(&sent[1]).unwrap_or(&sent[1]))
            .expect("the request is JSON");
    assert!(
        retry["previous_error"]
            .as_str()
            .is_some_and(|error| error.contains("login-step")),
        "the retry names the undefined step: {retry}"
    );
    running.stop().await.expect("the server stops");
}

/// Text only the stored scope draft carries. It is the obligation id of the draft's one check, so
/// the agent's views never contain it.
const DRAFT_MARKER: &str = "obl-signup-may";

/// The events route as `token` reads it from `from`: the status, and the body as text.
async fn events_as(running: &Running, token: &str, from: u64) -> (reqwest::StatusCode, String) {
    let response = client()
        .get(url(running, &format!("/api/events?from={from}")))
        .bearer_auth(token)
        .send()
        .await
        .expect("the events request is sent");
    let status = response.status();
    (
        status,
        response.text().await.expect("the events body is text"),
    )
}

/// The stream as `token` reads it from `from`, until an event of type `kind` arrives. Returns each
/// frame as text, in the order the stream sent them.
async fn stream_until(running: &Running, token: &str, from: u64, kind: &str) -> Vec<String> {
    let mut request = format!("ws://{}/api/stream?from={from}", running.local_addr)
        .into_client_request()
        .expect("the stream URL");
    let header = HeaderValue::from_str(&format!("Bearer {token}")).expect("the token header");
    request.headers_mut().insert(AUTHORIZATION, header);
    let (mut socket, _response) = tokio_tungstenite::connect_async(request)
        .await
        .expect("the stream connects");
    let mut frames = Vec::new();
    timeout(WAIT, async {
        loop {
            let message = socket
                .next()
                .await
                .expect("the stream stays open")
                .expect("the stream reads");
            let WsMessage::Text(text) = message else {
                continue;
            };
            let frame: &str = &text;
            let event: Value = serde_json::from_str(frame).expect("a stream event");
            let found = event["event"]["type"] == kind;
            frames.push(frame.to_owned());
            if found {
                return;
            }
        }
    })
    .await
    .expect("the stream sends the event in time");
    frames
}

/// The event of type `kind` in a view of the log, if the view has one.
fn event_of_type<'a>(log: &'a [Value], kind: &str) -> Option<&'a Value> {
    log.iter().find(|stamped| stamped["event"]["type"] == kind)
}

/// The agent's view of each event is the member's view with a scope draft's payload set to null.
/// Every other field, the seq, time and actor included, is the same, so a cursor holds.
fn assert_redacted(agent: &[Value], member: &[Value]) {
    assert_eq!(agent.len(), member.len(), "the agent reads every event");
    for (seen, stored) in agent.iter().zip(member) {
        let mut expected = stored.clone();
        if stored["event"]["type"] == "scope_drafted" {
            expected["event"]["draft"] = Value::Null;
        }
        assert_eq!(seen, &expected, "the agent's view of an event");
    }
}

/// An agent token never reads a scope draft. The events route and the stream both carry the
/// member's events with the draft set to null, so the agent sees the same log and the same cursor.
/// The member reads the draft through both routes.
#[tokio::test]
async fn an_agent_never_reads_a_scope_draft_through_the_events_or_the_stream() {
    let repo = demo_repo();
    let root = repo.path();
    let foreman = Drafting {
        inner: TwoTasks::default(),
        draft: drafted_scope_body(),
        asked: std::sync::Arc::default(),
    };
    let running = start_with(root, foreman, UsageLedger::default()).await;
    let task = TaskId::from_str("t-0").expect("valid task id");
    post_order(&running).await;
    let order = settle(&running, "the order", |_, world| {
        world.orders().next().map(|found| found.id)
    })
    .await;
    let view = scope_draft_of(&running, order).await;
    assert_eq!(view["state"], "drafted", "the draft is stored: {view}");
    // The split adds events after the draft, so wait for it. Once the task exists the log is
    // quiet until a human adopts the draft, and the views below are taken from the same log.
    settle(&running, "the split", |_, world| world.task(task).map(drop)).await;

    let (status, body) = events_as(&running, OWNER_TOKEN, 0).await;
    assert_eq!(status, reqwest::StatusCode::OK, "the owner reads the log");
    assert!(body.contains(DRAFT_MARKER), "the member reads the draft");
    let member_log: Vec<Value> = serde_json::from_str(&body).expect("the log is JSON");
    let member_drafted = event_of_type(&member_log, "scope_drafted").expect("the draft is logged");
    assert_eq!(
        member_drafted["event"]["draft"], view["draft"],
        "the member's event holds the draft the route serves"
    );

    let (status, body) = events_as(&running, AGENT_TOKEN, 0).await;
    assert_eq!(status, reqwest::StatusCode::OK, "an agent reads the log");
    assert!(
        !body.contains(DRAFT_MARKER),
        "the agent reads no draft: {body}"
    );
    let agent_log: Vec<Value> = serde_json::from_str(&body).expect("the log is JSON");
    assert_redacted(&agent_log, &member_log);

    // A cursor taken at the draft holds for the agent too: the tail is the member's tail, redacted.
    let index = member_log
        .iter()
        .position(|stamped| stamped["event"]["type"] == "scope_drafted")
        .expect("the draft is logged");
    let from = member_log[index]["seq"]
        .as_u64()
        .expect("the seq is a number");
    let (_, body) = events_as(&running, AGENT_TOKEN, from).await;
    let agent_tail: Vec<Value> = serde_json::from_str(&body).expect("the tail is JSON");
    assert_eq!(
        agent_tail[0]["seq"], from,
        "the agent's tail starts at the cursor"
    );
    let (_, body) = events_as(&running, OWNER_TOKEN, from).await;
    let member_tail: Vec<Value> = serde_json::from_str(&body).expect("the tail is JSON");
    assert_redacted(&agent_tail, &member_tail);

    // The stream sends the same frames the events route does.
    let frames = stream_until(&running, AGENT_TOKEN, 0, "scope_drafted").await;
    assert!(
        frames.iter().all(|frame| !frame.contains(DRAFT_MARKER)),
        "the agent's stream carries no draft: {frames:?}"
    );
    let streamed: Vec<Value> = frames
        .iter()
        .map(|frame| serde_json::from_str(frame).expect("a stream event"))
        .collect();
    assert_eq!(
        streamed.as_slice(),
        &agent_log[..streamed.len()],
        "the agent's stream matches the agent's events route"
    );
    let frames = stream_until(&running, OWNER_TOKEN, 0, "scope_drafted").await;
    let last = frames.last().expect("the member's stream sent the draft");
    let streamed: Value = serde_json::from_str(last).expect("a stream event");
    assert_eq!(
        streamed["event"]["draft"], view["draft"],
        "the member's stream carries the draft"
    );
    running.stop().await.expect("the server stops");
}

/// The confirmation body is the one a member sends as `ConfirmScope`. It is absent until the order
/// has a scope, an agent token never reads it, and posting the body as the owner confirms the
/// revision it was read for.
#[tokio::test]
async fn a_member_reads_the_confirmation_body_and_sending_it_confirms_the_scope() {
    let repo = demo_repo();
    let root = repo.path();
    let running = start_with(root, TwoTasks::default(), UsageLedger::default()).await;
    let task = TaskId::from_str("t-0").expect("valid task id");
    post_order(&running).await;
    let order = settle(&running, "the order", |_, world| {
        world.orders().next().map(|found| found.id)
    })
    .await;
    settle(&running, "the split", |_, world| world.task(task).map(drop)).await;
    let path = format!("/api/orders/{order}/scope/confirmation");
    let unscoped = client()
        .get(url(&running, &path))
        .bearer_auth(OWNER_TOKEN)
        .send()
        .await
        .expect("the body request is sent");
    assert_eq!(
        unscoped.status(),
        reqwest::StatusCode::NOT_FOUND,
        "an order with no scope has no body"
    );
    let base = git(root, &["rev-parse", PROTECTED_BRANCH])
        .trim()
        .to_owned();
    let scope = scope_for(order, &base);
    member(
        &running,
        &HumanCmd::ReviseScope {
            order,
            scope: Box::new(scope.clone()),
        },
    )
    .await;
    let refused = client()
        .get(url(&running, &path))
        .bearer_auth(AGENT_TOKEN)
        .send()
        .await
        .expect("the agent's body request is sent");
    assert_eq!(
        refused.status(),
        reqwest::StatusCode::FORBIDDEN,
        "an agent token never reads the body"
    );
    let response = client()
        .get(url(&running, &path))
        .bearer_auth(OWNER_TOKEN)
        .send()
        .await
        .expect("the body request is sent");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("the body is JSON");
    let binding = ScopeBinding::of(&scope).expect("the scope binds");
    assert_eq!(body["scope_rev"], 1, "the body is for the current revision");
    assert_eq!(body["by"], "owner", "the body is signed by the reader");
    assert_eq!(body["selector_hashes"], json!(binding.selector_hashes));
    assert_eq!(body["example_hashes"], json!(binding.example_hashes));
    let confirmation: ScopeConfirmation =
        serde_json::from_value(body).expect("the body is a confirmation");
    member(
        &running,
        &HumanCmd::ConfirmScope {
            order,
            confirmation: Box::new(confirmation),
        },
    )
    .await;
    let (_, origin) = snapshot(&running).await;
    let lane = origin.world().lane(order).expect("the lane is open");
    assert!(
        lane.confirmation.is_some(),
        "posting the body confirms the scope"
    );
    running.stop().await.expect("the server stops");
}

/// A failing gate opens an episode, and the observed runs it was judged on are kept for it. A
/// member reads them through the ruling draft, and a ruling built from the draft is recorded. An
/// agent token never reads the draft.
#[tokio::test]
async fn a_member_rules_on_a_kept_example_from_the_ruling_draft() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let root = repo.path();
    let (running, order, episode) = blocked_signup_episode(root).await;

    let path = format!("/api/orders/{order}/episodes/{episode}/ruling-draft");
    let refused = client()
        .get(url(&running, &path))
        .bearer_auth(AGENT_TOKEN)
        .send()
        .await
        .expect("the agent's draft request is sent");
    assert_eq!(
        refused.status(),
        reqwest::StatusCode::FORBIDDEN,
        "an agent token never reads a ruling draft"
    );
    let response = client()
        .get(url(&running, &path))
        .bearer_auth(OWNER_TOKEN)
        .send()
        .await
        .expect("the draft request is sent");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::OK,
        "the member reads the draft"
    );
    let draft: Value = response.json().await.expect("the draft is JSON");
    let ruled_on = draft["ruled_on"]
        .as_str()
        .expect("the draft names the example it is on")
        .to_owned();
    let kept = draft["examples"]
        .as_array()
        .expect("the draft lists the kept examples");
    assert!(
        kept.iter()
            .any(|example| example["hash"] == json!(ruled_on)),
        "the ruled-on example is one of the kept examples: {draft}"
    );
    for kept_example in kept {
        let example: ContrastExample = serde_json::from_value(kept_example["example"].clone())
            .expect("a kept example decodes");
        let hash = example.contrast_example_hash().expect("the example hashes");
        assert_eq!(
            kept_example["hash"],
            json!(hash.to_string()),
            "a kept example hashes to its name"
        );
    }

    // The decision is built from the draft: the ledger head it chains to, the scope in force, the
    // episode, and the example it is on. RecordRuling checks the chain and the actor.
    let head = &draft["ledger_head"];
    let mut decision = json!({
        "event": "decision_confirmed",
        "schema_version": "acan-ledger/1",
        "event_seq": head["event_seq"].as_u64().expect("the head has a sequence") + 1,
        "actor": "owner",
        "at": 1_800_000_000_000u64,
        "ruling_id": "r-signup-body",
        "order": order.to_string(),
        "scope_hash": draft["scope_hash"],
        "episode_id": draft["episode"],
        "evidence_hashes": [],
        "selector": draft["selector"],
        "predicate_ast": {
            "predicate": "equals_base",
            "step_id": "s1",
            "path": "body",
            "anchor": { "kind": "fresh" }
        },
        "accepts": [ruled_on],
        "rejects": [],
        "ruled_on": ruled_on,
        "normalization_version": draft["normalization_version"],
        "normalization_hash": draft["normalization_hash"],
        "applicability": "exact_observation_approval",
        "label": "the signup body",
        "machine_ids": [MACHINE]
    });
    if !head["hash"].is_null() {
        decision["prev_hash"] = head["hash"].clone();
    }
    let event: LedgerEvent = serde_json::from_value(decision).expect("the decision decodes");
    member(
        &running,
        &HumanCmd::RecordRuling {
            event: Box::new(event),
        },
    )
    .await;
    let (_, origin) = snapshot(&running).await;
    assert_eq!(
        origin.world().ledger().head().event_seq,
        1,
        "the ruling built from the draft is the ledger's first event"
    );
    running.stop().await.expect("the server stops");
}

/// Builds the seeded signup regression on the scope-first lane until the gate blocks it. Returns
/// the running server with the order and the episode the regression opened.
async fn blocked_signup_episode(root: &Path) -> (Running, OrderId, EpisodeId) {
    let (running, order, episode, _, _) = blocked_signup_task(root).await;
    (running, order, episode)
}

/// As [`blocked_signup_episode`], and also returns the task and the fence it was built under, so a
/// test can build the regression again on the same task.
async fn blocked_signup_task(root: &Path) -> (Running, OrderId, EpisodeId, TaskId, Fence) {
    blocked_signup_posted_by(root, OWNER_TOKEN).await
}

/// As [`blocked_signup_task`], with the order posted by the member `poster` holds. Only a member of
/// the world may post, so the owner adds the reviewer first.
async fn blocked_signup_posted_by(
    root: &Path,
    poster: &str,
) -> (Running, OrderId, EpisodeId, TaskId, Fence) {
    let running = start_plant(root).await;
    let task = TaskId::from_str("t-0").expect("valid task id");
    if poster == REVIEWER_TOKEN {
        member(
            &running,
            &HumanCmd::AddMember {
                member: MemberId::from_str(REVIEWER).expect("valid member id"),
                role: acan_core::Role::Member,
            },
        )
        .await;
    }
    post_order_as(&running, poster).await;
    scope_first(&running, root, task).await;
    let fence = claim(&running, task).await;
    wait_for_worktree(root, task, fence.generation).await;
    build_with(
        &running,
        root,
        task,
        fence,
        &["src/auth.ts"],
        seed_regression,
    )
    .await;
    settle(&running, "the gate to block the regression", |log, _| {
        log.iter().find_map(|stamped| match &stamped.event {
            Event::ReceiptRecorded { record, .. }
                if matches!(record.verdict, LaneVerdict::Blocked { .. }) =>
            {
                Some(())
            }
            _ => None,
        })
    })
    .await;
    let order = settle(&running, "the order", |_, world| {
        world.orders().next().map(|found| found.id)
    })
    .await;
    let episode = settle(&running, "the episode the regression opened", |_, world| {
        world
            .lane(order)
            .and_then(|lane| lane.episodes.keys().next().copied())
    })
    .await;
    (running, order, episode, task, fence)
}

/// The agent builds the regression again on the same task, adding a file, so the gate runs a second
/// time on the same episode.
async fn rebuild_regressed(running: &Running, root: &Path, task: TaskId, fence: Fence) {
    build_with(
        running,
        root,
        task,
        fence,
        &["src/auth.ts", "second.txt"],
        |worktree| write_file(worktree, "second.txt", "the second build\n"),
    )
    .await;
}

/// The predicate the signup must-keep rule names: the signup answer's body equals base. The
/// regressed answer differs from base, so this predicate fails on the example.
fn same_as_base() -> Value {
    json!({
        "predicate": "equals_base",
        "step_id": "s1",
        "path": "body",
        "anchor": { "kind": "fresh" }
    })
}

/// A predicate the regressed answer holds: the signup answers 201 Created.
fn created() -> Value {
    json!({ "predicate": "status_in", "step_id": "s1", "statuses": [201] })
}

/// The body of a ruling made from `draft`: the member's decision and predicate, with the head and
/// the example the draft named.
fn ruling_body(draft: &Value, decision: &str, predicate: Value) -> Value {
    json!({
        "decision": decision,
        "draft_head": draft["ledger_head"],
        "ruled_on": draft["ruled_on"],
        "scope_hash": draft["scope_hash"],
        "evidence_run": draft["evidence_run"],
        "evidence_revision": draft["evidence_revision"],
        "predicate": predicate,
        "applicability": "standing_predicate_approval",
        "label": "the signup body",
    })
}

/// The ruling draft of `episode`, read as the owner. Fails the test unless it is served.
async fn draft_of(running: &Running, order: OrderId, episode: EpisodeId) -> Value {
    let response = client()
        .get(url(
            running,
            &format!("/api/orders/{order}/episodes/{episode}/ruling-draft"),
        ))
        .bearer_auth(OWNER_TOKEN)
        .send()
        .await
        .expect("the draft request is sent");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::OK,
        "the owner reads the draft"
    );
    response.json().await.expect("the draft is JSON")
}

/// Posts a ruling on `episode` as `token`. Returns the status and the response body.
async fn post_ruling(
    running: &Running,
    token: &str,
    order: OrderId,
    episode: EpisodeId,
    body: &Value,
) -> (reqwest::StatusCode, String) {
    let response = client()
        .post(url(
            running,
            &format!("/api/orders/{order}/episodes/{episode}/ruling"),
        ))
        .bearer_auth(token)
        .json(body)
        .send()
        .await
        .expect("the ruling is sent");
    let status = response.status();
    (status, response.text().await.unwrap_or_default())
}

/// A member allows the kept example through the ruling route. The predicate holds on the regressed
/// answer, so the server builds the decision from the draft and the origin records it.
#[tokio::test]
async fn a_member_allows_an_episode_through_the_ruling_route() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let (running, order, episode) = blocked_signup_episode(repo.path()).await;
    let draft = draft_of(&running, order, episode).await;
    let ruled_on = draft["ruled_on"]
        .as_str()
        .expect("the draft names its example");
    let (status, body) = post_ruling(
        &running,
        OWNER_TOKEN,
        order,
        episode,
        &ruling_body(&draft, "allow", created()),
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "the allow is recorded: {body}"
    );
    let (_, origin) = snapshot(&running).await;
    match origin.world().ledger().events() {
        [LedgerEvent::DecisionConfirmed(decision)] => {
            let accepts: Vec<String> = decision.accepts.iter().map(ToString::to_string).collect();
            assert_eq!(
                accepts,
                [ruled_on],
                "the allow accepts the example it is on"
            );
            assert!(decision.rejects.is_empty(), "an allow rejects nothing");
            assert_eq!(decision.ruled_on.to_string(), ruled_on);
            assert_eq!(decision.episode_id, Some(episode));
            assert_eq!(
                decision.actor.as_str(),
                "owner",
                "the member signs the ruling"
            );
        }
        other => panic!("one decision is recorded, got {other:?}"),
    }
    running.stop().await.expect("the server stops");
}

/// A member forbids the kept example through the ruling route. The predicate fails on the
/// regressed answer, so the example is rejected and the origin records it so.
#[tokio::test]
async fn a_member_forbids_an_episode_through_the_ruling_route() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let (running, order, episode) = blocked_signup_episode(repo.path()).await;
    let draft = draft_of(&running, order, episode).await;
    let ruled_on = draft["ruled_on"]
        .as_str()
        .expect("the draft names its example");
    let (status, body) = post_ruling(
        &running,
        OWNER_TOKEN,
        order,
        episode,
        &ruling_body(&draft, "forbid", same_as_base()),
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "the forbid is recorded: {body}"
    );
    let (_, origin) = snapshot(&running).await;
    match origin.world().ledger().events() {
        [LedgerEvent::DecisionConfirmed(decision)] => {
            assert!(decision.accepts.is_empty(), "a forbid accepts nothing");
            let rejects: Vec<String> = decision.rejects.iter().map(ToString::to_string).collect();
            assert_eq!(
                rejects,
                [ruled_on],
                "the forbid rejects the example it is on"
            );
        }
        other => panic!("one decision is recorded, got {other:?}"),
    }
    running.stop().await.expect("the server stops");
}

/// The server checks the predicate on the example before it records anything. Allowing with a
/// predicate the regressed answer fails is refused, and so is forbidding with one it holds on. The
/// log does not move.
#[tokio::test]
async fn a_predicate_that_does_not_give_the_decisions_answer_is_refused() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let (running, order, episode) = blocked_signup_episode(repo.path()).await;
    let draft = draft_of(&running, order, episode).await;
    let (allow, body) = post_ruling(
        &running,
        OWNER_TOKEN,
        order,
        episode,
        &ruling_body(&draft, "allow", same_as_base()),
    )
    .await;
    assert_eq!(
        allow,
        reqwest::StatusCode::BAD_REQUEST,
        "an allow with a predicate the example fails is refused: {body}"
    );
    let (forbid, body) = post_ruling(
        &running,
        OWNER_TOKEN,
        order,
        episode,
        &ruling_body(&draft, "forbid", created()),
    )
    .await;
    assert_eq!(
        forbid,
        reqwest::StatusCode::BAD_REQUEST,
        "a forbid with a predicate the example holds is refused: {body}"
    );
    let (_, origin) = snapshot(&running).await;
    assert_eq!(
        origin.world().ledger().head().event_seq,
        0,
        "a refused ruling is not recorded"
    );
    running.stop().await.expect("the server stops");
}

/// A draft is stale once the ruling log moves. A ruling sent from a stale draft is refused with
/// 409 and records nothing. A ruling made from the fresh draft supersedes the first one.
#[tokio::test]
async fn a_stale_draft_is_refused_and_a_fresh_draft_supersedes_the_ruling() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let (running, order, episode) = blocked_signup_episode(repo.path()).await;
    let first = draft_of(&running, order, episode).await;
    let (status, body) = post_ruling(
        &running,
        OWNER_TOKEN,
        order,
        episode,
        &ruling_body(&first, "allow", created()),
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "the first ruling is recorded: {body}"
    );

    let (status, body) = post_ruling(
        &running,
        OWNER_TOKEN,
        order,
        episode,
        &ruling_body(&first, "forbid", same_as_base()),
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::CONFLICT,
        "a ruling from the draft read before the first ruling is stale: {body}"
    );
    let (_, origin) = snapshot(&running).await;
    assert_eq!(
        origin.world().ledger().head().event_seq,
        1,
        "the stale ruling is not recorded"
    );

    let second = draft_of(&running, order, episode).await;
    let (status, body) = post_ruling(
        &running,
        OWNER_TOKEN,
        order,
        episode,
        &ruling_body(&second, "forbid", same_as_base()),
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "the fresh ruling is recorded: {body}"
    );
    let (_, origin) = snapshot(&running).await;
    match origin.world().ledger().events() {
        [
            LedgerEvent::DecisionConfirmed(_),
            LedgerEvent::DecisionConfirmed(decision),
        ] => {
            assert_eq!(decision.event_seq, 2);
            assert_eq!(
                decision.supersedes,
                Some(Supersedes { event_seq: 1 }),
                "the fresh ruling supersedes the first in the same lineage"
            );
        }
        other => panic!("two decisions are recorded, got {other:?}"),
    }
    running.stop().await.expect("the server stops");
}

/// Only a member rules. An agent's token gets 403 and no token gets 401, and neither reaches the
/// log, even with a ruling the draft would accept.
#[tokio::test]
async fn only_a_member_rules_through_the_ruling_route() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let (running, order, episode) = blocked_signup_episode(repo.path()).await;
    let draft = draft_of(&running, order, episode).await;
    let body = ruling_body(&draft, "allow", created());
    let (agent, text) = post_ruling(&running, AGENT_TOKEN, order, episode, &body).await;
    assert_eq!(
        agent,
        reqwest::StatusCode::FORBIDDEN,
        "an agent never rules: {text}"
    );
    let anonymous = client()
        .post(url(
            &running,
            &format!("/api/orders/{order}/episodes/{episode}/ruling"),
        ))
        .json(&body)
        .send()
        .await
        .expect("the anonymous ruling is sent");
    assert_eq!(
        anonymous.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "a ruling without a token is unauthorized"
    );
    let (_, origin) = snapshot(&running).await;
    assert_eq!(
        origin.world().ledger().head().event_seq,
        0,
        "no refused ruling is recorded"
    );
    running.stop().await.expect("the server stops");
}

/// A receipt that lands while a ruling waits for the writer refuses the ruling. The draft was read
/// at one evidence revision; the agent's second build adds one before the writer decides the
/// ruling, so the ruling is refused with 409 and nothing is recorded. A ruling from a fresh draft
/// is then recorded.
#[tokio::test]
async fn a_receipt_that_lands_while_a_ruling_waits_refuses_it() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let root = repo.path();
    let (running, order, episode, task, fence) = blocked_signup_task(root).await;
    let draft = draft_of(&running, order, episode).await;
    assert_eq!(
        draft["evidence_revision"],
        json!(1),
        "the draft reads the evidence of one gate run"
    );
    let stale = ruling_body(&draft, "allow", created());

    running.ruling_hold.hold();
    let ruling = post_ruling(&running, OWNER_TOKEN, order, episode, &stale);
    let receipt = async {
        running.ruling_hold.parked(1).await;
        rebuild_regressed(&running, root, task, fence).await;
        settle(&running, "the second receipt on the episode", |_, world| {
            let found = world.lane(order)?.episodes.get(&episode)?;
            (found.evidence.len() == 2).then_some(())
        })
        .await;
        running.ruling_hold.release();
    };
    let ((status, body), ()) = tokio::join!(ruling, receipt);
    assert_eq!(
        status,
        reqwest::StatusCode::CONFLICT,
        "a ruling on the first evidence is refused: {body}"
    );
    assert!(
        body.contains("stale_evidence"),
        "the refusal names the evidence: {body}"
    );
    let (_, origin) = snapshot(&running).await;
    assert_eq!(
        origin.world().ledger().head().event_seq,
        0,
        "the refused ruling is not recorded"
    );

    let fresh = draft_of(&running, order, episode).await;
    assert_eq!(
        fresh["evidence_revision"],
        json!(2),
        "the fresh draft reads both gate runs"
    );
    let (status, body) = post_ruling(
        &running,
        OWNER_TOKEN,
        order,
        episode,
        &ruling_body(&fresh, "allow", created()),
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "a ruling from the fresh draft is recorded: {body}"
    );
    running.stop().await.expect("the server stops");
}

/// A scope revision that lands while a ruling waits for the writer refuses the ruling with 409 and
/// the current scope. The draft named the first revision; the revision moves the lane past it.
#[tokio::test]
async fn a_scope_revision_that_lands_while_a_ruling_waits_refuses_it() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let root = repo.path();
    let (running, order, episode) = blocked_signup_episode(root).await;
    let draft = draft_of(&running, order, episode).await;
    let base = git(root, &["rev-parse", PROTECTED_BRANCH])
        .trim()
        .to_owned();
    let mut revised = serde_json::to_value(scope_for(order, &base)).expect("the scope serializes");
    revised["rev"] = json!(2);
    let revised: Scope = serde_json::from_value(revised).expect("the revised scope decodes");

    let body = ruling_body(&draft, "allow", created());
    running.ruling_hold.hold();
    let ruling = post_ruling(&running, OWNER_TOKEN, order, episode, &body);
    let revision = async {
        running.ruling_hold.parked(1).await;
        member(
            &running,
            &HumanCmd::ReviseScope {
                order,
                scope: Box::new(revised),
            },
        )
        .await;
        running.ruling_hold.release();
    };
    let ((status, body), ()) = tokio::join!(ruling, revision);
    assert_eq!(
        status,
        reqwest::StatusCode::CONFLICT,
        "a ruling under the first scope is refused: {body}"
    );
    assert!(
        body.contains("stale_scope"),
        "the refusal names the scope: {body}"
    );
    let (_, origin) = snapshot(&running).await;
    assert_eq!(
        origin.world().ledger().head().event_seq,
        0,
        "the refused ruling is not recorded"
    );
    running.stop().await.expect("the server stops");
}

/// Two rulings from the same draft, sent at once, record one decision. The writer decides them in
/// turn, so the second finds the log past the head it was built on and is refused with 409.
#[tokio::test]
async fn two_rulings_from_the_same_draft_record_one_decision() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let (running, order, episode) = blocked_signup_episode(repo.path()).await;
    let draft = draft_of(&running, order, episode).await;
    let body = ruling_body(&draft, "allow", created());

    running.ruling_hold.hold();
    let first = post_ruling(&running, OWNER_TOKEN, order, episode, &body);
    let second = post_ruling(&running, OWNER_TOKEN, order, episode, &body);
    let release = async {
        running.ruling_hold.parked(2).await;
        running.ruling_hold.release();
    };
    let ((first, first_body), (second, second_body), ()) = tokio::join!(first, second, release);
    let statuses = [first, second];
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == reqwest::StatusCode::OK)
            .count(),
        1,
        "one of the two rulings is recorded: {first} {first_body} / {second} {second_body}"
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == reqwest::StatusCode::CONFLICT)
            .count(),
        1,
        "the other is refused as stale: {first} {first_body} / {second} {second_body}"
    );
    let (_, origin) = snapshot(&running).await;
    assert_eq!(
        origin.world().ledger().head().event_seq,
        1,
        "exactly one decision is recorded"
    );
    running.stop().await.expect("the server stops");
}

/// Only the order's poster or an owner rules in it. A member who neither posted the order nor
/// owns the plant is refused with 403, and nothing is recorded.
#[tokio::test]
async fn a_member_who_did_not_post_the_order_cannot_rule_in_it() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let (running, order, episode) = blocked_signup_episode(repo.path()).await;
    let draft = draft_of(&running, order, episode).await;
    let (status, body) = post_ruling(
        &running,
        STRANGER_TOKEN,
        order,
        episode,
        &ruling_body(&draft, "allow", created()),
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::FORBIDDEN,
        "an unrelated member is refused: {body}"
    );
    let (_, origin) = snapshot(&running).await;
    assert_eq!(
        origin.world().ledger().head().event_seq,
        0,
        "no refused ruling is recorded"
    );
    running.stop().await.expect("the server stops");
}

/// The member who posted the order rules in it without being an owner. The decision is signed by
/// that member.
#[tokio::test]
async fn the_poster_of_an_order_rules_in_it_without_being_an_owner() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let (running, order, episode, _, _) =
        blocked_signup_posted_by(repo.path(), REVIEWER_TOKEN).await;
    let draft = draft_of(&running, order, episode).await;
    let (status, body) = post_ruling(
        &running,
        REVIEWER_TOKEN,
        order,
        episode,
        &ruling_body(&draft, "allow", created()),
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "the poster's ruling is recorded: {body}"
    );
    let (_, origin) = snapshot(&running).await;
    match origin.world().ledger().events() {
        [LedgerEvent::DecisionConfirmed(decision)] => assert_eq!(
            decision.actor.as_str(),
            "reviewer",
            "the poster signs the ruling"
        ),
        other => panic!("one decision is recorded, got {other:?}"),
    }
    running.stop().await.expect("the server stops");
}

/// An episode belongs to the order that opened it. A draft or a ruling that names it under another
/// order is not found, and nothing is recorded.
#[tokio::test]
async fn an_episode_is_not_found_under_another_order() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let (running, order, episode) = blocked_signup_episode(repo.path()).await;
    let draft = draft_of(&running, order, episode).await;
    member(
        &running,
        &HumanCmd::PostOrder {
            machine: machine(),
            sentence: "a second order".to_owned(),
            witnesses: Vec::new(),
        },
    )
    .await;
    let other = settle(&running, "the second order", |_, world| {
        world.orders().map(|found| found.id).find(|id| *id != order)
    })
    .await;

    let response = client()
        .get(url(
            &running,
            &format!("/api/orders/{other}/episodes/{episode}/ruling-draft"),
        ))
        .bearer_auth(OWNER_TOKEN)
        .send()
        .await
        .expect("the draft request is sent");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::NOT_FOUND,
        "the draft of another order's episode is not found"
    );
    let (status, body) = post_ruling(
        &running,
        OWNER_TOKEN,
        other,
        episode,
        &ruling_body(&draft, "allow", created()),
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::NOT_FOUND,
        "a ruling on another order's episode is not found: {body}"
    );
    let (_, origin) = snapshot(&running).await;
    assert_eq!(
        origin.world().ledger().head().event_seq,
        0,
        "no ruling is recorded"
    );
    running.stop().await.expect("the server stops");
}

/// A ruling names the example it is on. One naming another example is refused with 409, and
/// nothing is recorded.
#[tokio::test]
async fn a_ruling_on_a_moved_example_is_refused() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let (running, order, episode) = blocked_signup_episode(repo.path()).await;
    let draft = draft_of(&running, order, episode).await;
    let mut body = ruling_body(&draft, "allow", created());
    body["ruled_on"] = json!(format!("sha256:{}", "0".repeat(64)));
    let (status, text) = post_ruling(&running, OWNER_TOKEN, order, episode, &body).await;
    assert_eq!(
        status,
        reqwest::StatusCode::CONFLICT,
        "a ruling on another example is refused: {text}"
    );
    let (_, origin) = snapshot(&running).await;
    assert_eq!(
        origin.world().ledger().head().event_seq,
        0,
        "no refused ruling is recorded"
    );
    running.stop().await.expect("the server stops");
}

/// A body that is not JSON, has an unknown field, lacks a decision, names an unknown decision, or
/// has a blank label is refused with 400 before anything is checked or recorded.
#[tokio::test]
async fn a_malformed_or_unlabelled_ruling_is_refused() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let (running, order, episode) = blocked_signup_episode(repo.path()).await;
    let draft = draft_of(&running, order, episode).await;
    let valid = ruling_body(&draft, "allow", created());
    let mut blank = valid.clone();
    blank["label"] = json!("   ");
    let mut unknown = valid.clone();
    unknown["extra"] = json!(true);
    let mut missing = valid.clone();
    missing
        .as_object_mut()
        .expect("the body is an object")
        .remove("decision");
    let mut unknown_decision = valid.clone();
    unknown_decision["decision"] = json!("maybe");
    for (what, body) in [
        ("a blank label", blank),
        ("an unknown field", unknown),
        ("no decision", missing),
        ("an unknown decision", unknown_decision),
    ] {
        let (status, text) = post_ruling(&running, OWNER_TOKEN, order, episode, &body).await;
        assert_eq!(
            status,
            reqwest::StatusCode::BAD_REQUEST,
            "{what} is refused: {text}"
        );
    }
    let not_json = client()
        .post(url(
            &running,
            &format!("/api/orders/{order}/episodes/{episode}/ruling"),
        ))
        .bearer_auth(OWNER_TOKEN)
        .body("not json")
        .send()
        .await
        .expect("the ruling is sent");
    assert_eq!(
        not_json.status(),
        reqwest::StatusCode::BAD_REQUEST,
        "a body that is not JSON is refused"
    );
    let (_, origin) = snapshot(&running).await;
    assert_eq!(
        origin.world().ledger().head().event_seq,
        0,
        "no malformed ruling is recorded"
    );
    running.stop().await.expect("the server stops");
}

/// A fixed base anchor names a saved example, and the gate builds its receipts with none. A ruling
/// with one would block the next gate run, so the server refuses it with 400 and records nothing.
/// The forbid here names the example it is on, which the save check alone would have accepted.
#[tokio::test]
async fn a_fixed_anchor_is_refused_and_nothing_is_recorded() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let (running, order, episode) = blocked_signup_episode(repo.path()).await;
    let draft = draft_of(&running, order, episode).await;
    let fixed = json!({
        "predicate": "equals_base",
        "step_id": "s1",
        "path": "body",
        "anchor": { "kind": "fixed", "example": draft["ruled_on"] }
    });
    let (status, body) = post_ruling(
        &running,
        OWNER_TOKEN,
        order,
        episode,
        &ruling_body(&draft, "forbid", fixed),
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::BAD_REQUEST,
        "a fixed anchor is refused: {body}"
    );
    let (_, origin) = snapshot(&running).await;
    assert_eq!(
        origin.world().ledger().head().event_seq,
        0,
        "no ruling with a fixed anchor is recorded"
    );
    running.stop().await.expect("the server stops");
}

/// A ruling on a fresh base anchor is judged at the gate run that follows it. The agent builds the
/// regression again after the ruling, and the receipt of that run lists the ruling as held or
/// violated, never unknown. A fixed anchor would read unknown there, since the gate loads no saved
/// examples. The verdict depends on which probes the predicate covers, so the test pins only that
/// the ruling is judged.
#[tokio::test]
async fn a_posted_ruling_is_judged_at_the_gate_run_that_follows_it() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let root = repo.path();
    let (running, order, episode, task, fence) = blocked_signup_task(root).await;
    let draft = draft_of(&running, order, episode).await;
    let (status, body) = post_ruling(
        &running,
        OWNER_TOKEN,
        order,
        episode,
        &ruling_body(&draft, "forbid", same_as_base()),
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "the ruling is recorded: {body}"
    );

    rebuild_regressed(&running, root, task, fence).await;
    let ruling_id = format!("episode:{episode}");
    let (verdict, rulings) = settle(&running, "the gate run after the ruling", |log, _| {
        let ruled_at = log
            .iter()
            .position(|stamped| matches!(stamped.event, Event::RulingRecorded { .. }))?;
        log[ruled_at..]
            .iter()
            .rev()
            .find_map(|stamped| match &stamped.event {
                Event::ReceiptRecorded { record, .. } => {
                    Some((record.verdict.clone(), record.receipt.rulings.clone()))
                }
                _ => None,
            })
    })
    .await;
    let judged = rulings
        .iter()
        .find(|check| check.ruling_id.to_string() == ruling_id)
        .map(|check| {
            matches!(
                check.status,
                RulingStatus::Held {} | RulingStatus::Violated { .. }
            )
        });
    assert_eq!(
        judged,
        Some(true),
        "the ruling is judged at the gate run that follows it; verdict {verdict:?}, rulings {rulings:?}"
    );
    running.stop().await.expect("the server stops");
}

/// A must-keep obligation that names a saved example as its base is refused when a member revises
/// the scope. The refusal is a 400 that names the anchor, and the scope does not move. A fixed
/// anchor nested under `all` is refused too. The same rule with a fresh anchor is accepted.
#[tokio::test]
async fn a_fixed_anchor_in_a_must_keep_rule_is_refused_and_the_scope_does_not_move() {
    let _docker = DOCKER.lock().await;
    let repo = demo_repo();
    let root = repo.path();
    let running = start_plant(root).await;
    post_order(&running).await;
    let order = settle(&running, "the order", |_, world| {
        world.orders().next().map(|found| found.id)
    })
    .await;
    let base = git(root, &["rev-parse", PROTECTED_BRANCH])
        .trim()
        .to_owned();
    let fixed = json!({ "kind": "fixed", "example": format!("sha256:{}", "a".repeat(64)) });

    let mut direct = revise_command(order, &base);
    direct["scope"]["body"]["must_keep"][0]["obligations"][0]["predicate"]["anchor"] =
        fixed.clone();
    let (status, body) = send_command(&running, &direct).await;
    assert_eq!(
        status,
        reqwest::StatusCode::BAD_REQUEST,
        "a fixed anchor in a must-keep rule is refused: {body}"
    );
    assert!(
        body.contains("fixed anchors are not supported yet"),
        "the refusal names the anchor: {body}"
    );

    let mut nested = revise_command(order, &base);
    nested["scope"]["body"]["must_keep"][0]["obligations"][0]["predicate"] = json!({
        "predicate": "all",
        "of": [
            same_as_base(),
            { "predicate": "equals_base", "step_id": "s1", "path": "body", "anchor": fixed }
        ]
    });
    let (status, body) = send_command(&running, &nested).await;
    assert_eq!(
        status,
        reqwest::StatusCode::BAD_REQUEST,
        "a fixed anchor nested under all is refused: {body}"
    );
    assert!(
        body.contains("fixed anchors are not supported yet"),
        "the nested refusal names the anchor: {body}"
    );
    let (_, origin) = snapshot(&running).await;
    assert!(
        origin.world().lane(order).is_none(),
        "a refused scope revision moves nothing"
    );

    member(
        &running,
        &HumanCmd::ReviseScope {
            order,
            scope: Box::new(scope_for(order, &base)),
        },
    )
    .await;
    let (_, origin) = snapshot(&running).await;
    assert!(
        origin.world().lane(order).is_some(),
        "the same rule with a fresh anchor is accepted"
    );
    running.stop().await.expect("the server stops");
}

/// A `ReviseScope` command as the HTTP body the owner sends, for the scope `scope_for` builds.
fn revise_command(order: OrderId, base: &str) -> Value {
    serde_json::to_value(HumanCmd::ReviseScope {
        order,
        scope: Box::new(scope_for(order, base)),
    })
    .expect("the command serializes")
}

/// Posts one command body to `/api/commands` as the owner. Returns the status and the body.
async fn send_command(running: &Running, cmd: &Value) -> (reqwest::StatusCode, String) {
    let response = client()
        .post(url(running, "/api/commands"))
        .bearer_auth(OWNER_TOKEN)
        .json(cmd)
        .send()
        .await
        .expect("the command is sent");
    let status = response.status();
    (status, response.text().await.unwrap_or_default())
}

/// The plant as the owner reads it over HTTP.
async fn plant_of(running: &Running) -> Value {
    let response = client()
        .get(url(running, "/api/plant"))
        .bearer_auth(OWNER_TOKEN)
        .send()
        .await
        .expect("the plant request is sent");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    response.json().await.expect("the plant is JSON")
}

/// The contract flag reads the protected app contract at the protected head. The demo carries one,
/// so the flag is present and names the contract's version. The plant carries the same flag, and
/// the server runs no vision checks, so the plant's vision flag is off.
#[tokio::test]
async fn the_contract_flag_reads_the_protected_app_contract() {
    let repo = demo_repo();
    let running = start_plant(repo.path()).await;
    let response = client()
        .get(url(&running, "/api/contract"))
        .bearer_auth(OWNER_TOKEN)
        .send()
        .await
        .expect("the contract request is sent");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let contract: Value = response.json().await.expect("the contract is JSON");
    assert_eq!(
        contract,
        json!({ "present": true, "version": "acan-app/1" })
    );
    let plant = plant_of(&running).await;
    assert_eq!(
        plant["contract"],
        json!({ "present": true, "version": "acan-app/1" }),
        "the plant carries the same contract flag"
    );
    assert_eq!(
        plant["vision"],
        json!({ "on": false, "reason": "the server runs no vision checks" }),
        "the test server runs no vision checks"
    );
    running.stop().await.expect("the server stops");
}

/// A protected head with no app contract reports none, with no version. The contract is removed on
/// the protected branch before the server starts, so the flag reads the head as it is.
#[tokio::test]
async fn a_protected_head_without_a_contract_reports_none() {
    let repo = demo_repo();
    let root = repo.path();
    git(root, &["checkout", "-q", PROTECTED_BRANCH]);
    git(root, &["rm", "-q", ".acan/app.json"]);
    git(root, &["commit", "-q", "-m", "drop the app contract"]);
    git(root, &["switch", "-q", "user-work"]);
    let running = start_plant(root).await;
    let response = client()
        .get(url(&running, "/api/contract"))
        .bearer_auth(OWNER_TOKEN)
        .send()
        .await
        .expect("the contract request is sent");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let contract: Value = response.json().await.expect("the contract is JSON");
    assert_eq!(contract, json!({ "present": false, "version": null }));
    assert_eq!(
        plant_of(&running).await["contract"],
        json!({ "present": false, "version": null }),
        "the plant carries the same contract flag"
    );
    running.stop().await.expect("the server stops");
}

/// The offline models, except that a split makes two tasks on the order's machine.
#[derive(Default)]
struct TwoTasks {
    offline: Offline,
}

impl ModelClient for TwoTasks {
    async fn chat(&self, role: Role, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        let split = model::ask_of(&request) == Some(Ask::Split);
        let mut response = self.offline.chat(role, request).await?;
        if split {
            let content = response.content.as_deref().unwrap_or_default();
            let mut reply: Value =
                serde_json::from_str(content).expect("the offline split is JSON");
            let tasks = reply["tasks"]
                .as_array_mut()
                .expect("the split lists tasks");
            let mut again = tasks[0].clone();
            again["title"] = Value::from("the same file again");
            tasks.push(again);
            response.content = Some(reply.to_string());
        }
        Ok(response)
    }
}

/// A clone of acan-demo at its pinned commit, with `main` as the protected branch and the user's
/// checkout on `user-work`. The server must never touch that checkout. The clone is the only
/// thing the tests write.
struct DemoRepo {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl DemoRepo {
    fn path(&self) -> &Path {
        &self.root
    }
}

fn demo_repo() -> DemoRepo {
    let source = std::env::var_os(DEMO_DIR_VAR).unwrap_or_else(|| {
        panic!("set {DEMO_DIR_VAR} to a checkout of acan-demo at {DEMO_COMMIT}")
    });
    let dir = tempfile::tempdir().expect("temporary repository");
    let root = dir.path().join("demo");
    git(
        dir.path(),
        &[
            "clone",
            "-q",
            "--no-hardlinks",
            &source.to_string_lossy(),
            &root.to_string_lossy(),
        ],
    );
    git(
        &root,
        &["checkout", "-q", "-B", PROTECTED_BRANCH, DEMO_COMMIT],
    );
    git(&root, &["config", "user.name", "Test User"]);
    git(&root, &["config", "user.email", "test@example.test"]);
    git(&root, &["switch", "-q", "-c", "user-work"]);
    DemoRepo { _dir: dir, root }
}

/// Runs git in `dir` and returns its stdout. Panics with the exit status, stdout and stderr when
/// the command fails. Git reports some failures only on stdout, such as `commit` with nothing
/// staged under `-q`, so stderr alone can be empty.
///
/// A lock held by the server's own git is contention, not a failure of the command, so it is
/// retried. The server and these tests share the repository's index.
fn git(dir: &Path, args: &[&str]) -> String {
    for attempt in 1..=5 {
        let output = Process::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git runs");
        if output.status.success() {
            return String::from_utf8(output.stdout).expect("git prints UTF-8");
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let locked = stderr.contains("index.lock");
        if !locked || attempt == 5 {
            panic!(
                "git {args:?} in {} failed with {}\nstdout: {stdout}\nstderr: {stderr}",
                dir.display(),
                output.status
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    unreachable!("the loop returns or panics")
}

/// A repository-local config value, empty when unset. `git config --get` exits 1 for an unset key.
fn git_config(dir: &Path, key: &str) -> String {
    let output = Process::new("git")
        .arg("-C")
        .arg(dir)
        .args(["config", "--get", key])
        .output()
        .expect("git runs");
    if output.status.success() {
        String::from_utf8(output.stdout).expect("git prints UTF-8")
    } else {
        String::new()
    }
}

/// Whether `branch` exists in the repository that `dir` belongs to.
fn branch_exists(dir: &Path, branch: &str) -> bool {
    Process::new("git")
        .arg("-C")
        .arg(dir)
        .args(["show-ref", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .status()
        .expect("git runs")
        .success()
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().expect("the path exists")
}

fn worktree_dir(root: &Path, task: TaskId) -> PathBuf {
    root.join(".acan").join("worktrees").join(task.to_string())
}

/// Writes the token file before the first start, so the test holds an owner and an agent token.
/// Writes a tokens file that holds only the owner, so the project has no agent token.
fn write_owner_tokens(root: &Path) {
    let state = root.join(".acan");
    fs::create_dir_all(&state).expect("state directory");
    fs::write(
        state.join("tokens.json"),
        format!(
            r#"[{{"token": "{OWNER_TOKEN}", "principal": {{"kind": "member", "id": "owner"}}}}]"#
        ),
    )
    .expect("tokens file");
}

fn write_tokens(root: &Path) {
    let state = root.join(".acan");
    fs::create_dir_all(&state).expect("state directory");
    fs::write(
        state.join("tokens.json"),
        format!(
            r#"[
              {{"token": "{OWNER_TOKEN}", "principal": {{"kind": "member", "id": "owner"}}}},
              {{"token": "{REVIEWER_TOKEN}", "principal": {{"kind": "member", "id": "{REVIEWER}"}}}},
              {{"token": "{STRANGER_TOKEN}", "principal": {{"kind": "member", "id": "stranger"}}}},
              {{"token": "{AGENT_TOKEN}", "principal": {{"kind": "agent", "id": "{AGENT}"}}}}
            ]"#
        ),
    )
    .expect("tokens file");
}

async fn start_plant(root: &Path) -> Running {
    let offline = Offline::default();
    let ledger = offline.ledger().clone();
    start_with(root, offline, ledger).await
}

async fn start_with<C: ModelClient + 'static>(
    root: &Path,
    client: C,
    ledger: UsageLedger,
) -> Running {
    write_tokens(root);
    let config = ServerConfig {
        repo: root.to_path_buf(),
        addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        team: false,
        web: None,
        protected_branch: Some("main".to_owned()),
        witness_http_base_url: None,
        witness_concurrency: DEFAULT_WITNESS_CONCURRENCY,
        vision: None,
    };
    start(config, client, ledger)
        .await
        .expect("the server starts on the test repository")
}

/// Starts one server over two projects. `root` comes first, so the unprefixed routes reach it,
/// and `other` is the second project. Both protect `main`.
async fn start_pair(root: &Path, other: &Path) -> Running {
    write_tokens(root);
    let project = |id: &str, name: &str, repo: &Path| ProjectSpec {
        id: id.to_owned(),
        name: name.to_owned(),
        repo: repo.to_path_buf(),
        protected_branch: Some(PROTECTED_BRANCH.to_owned()),
        llm: None,
        witness_http_base_url: None,
    };
    let config = ServeConfig {
        projects: ProjectSet {
            default: "demo".to_owned(),
            projects: vec![
                project("demo", "Demo", root),
                project("plain", "Plain", other),
            ],
        },
        addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        team: false,
        web: None,
        witness_concurrency: DEFAULT_WITNESS_CONCURRENCY,
        vision: None,
        local_workers: 0,
    };
    let first = Offline::default();
    let first_ledger = first.ledger().clone();
    let second = Offline::default();
    let second_ledger = second.ledger().clone();
    start_projects(config, vec![(first, first_ledger), (second, second_ledger)])
        .await
        .expect("the server starts on both projects")
}

/// A plain repository with one commit on `main` and a file of its own.
fn plain_repo() -> DemoRepo {
    let dir = tempfile::tempdir().expect("temporary repository");
    let root = dir.path().join("plain");
    fs::create_dir_all(&root).expect("the repository folder");
    git(&root, &["init", "-q", "-b", PROTECTED_BRANCH]);
    git(&root, &["config", "user.name", "Test User"]);
    git(&root, &["config", "user.email", "test@example.test"]);
    fs::write(root.join("notes.txt"), "the plain project\n").expect("a file");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "initial"]);
    DemoRepo { _dir: dir, root }
}

fn url(running: &Running, path: &str) -> String {
    format!("http://{}{path}", running.local_addr)
}

/// One client for every request, so each poll reuses its connection instead of building a client.
fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

fn agent() -> AgentId {
    AgentId::from_str(AGENT).expect("valid agent id")
}

fn machine() -> MachineKey {
    MachineKey::from_str(MACHINE).expect("valid machine key")
}

/// Sends one member command over HTTP as the owner, and fails the test unless it is accepted.
async fn member(running: &Running, cmd: &HumanCmd) {
    member_as(running, OWNER_TOKEN, cmd).await;
}

/// Sends one member command over HTTP as the member `token` holds, and fails the test unless it
/// is accepted.
async fn member_as(running: &Running, token: &str, cmd: &HumanCmd) {
    send_member(running, "/api/commands", token, cmd).await;
}

/// Sends one member command to the commands route at `path` as the owner, and fails the test
/// unless it is accepted.
async fn member_at(running: &Running, path: &str, cmd: &HumanCmd) {
    send_member(running, path, OWNER_TOKEN, cmd).await;
}

/// Sends one member command to the commands route at `path` as the member `token` holds, and
/// fails the test unless it is accepted.
async fn send_member(running: &Running, path: &str, token: &str, cmd: &HumanCmd) {
    let response = client()
        .post(url(running, path))
        .bearer_auth(token)
        .json(cmd)
        .send()
        .await
        .expect("the member command is sent");
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "the member command is accepted: {body}"
    );
}

async fn post_order(running: &Running) {
    post_order_as(running, OWNER_TOKEN).await;
}

/// Posts the test order as the member `token` holds.
async fn post_order_as(running: &Running, token: &str) {
    member_as(
        running,
        token,
        &HumanCmd::PostOrder {
            machine: machine(),
            sentence: "land the agent's file".to_owned(),
            witnesses: Vec::new(),
        },
    )
    .await;
}

/// The scope of `order` at revision 1, with one must-keep rule over the signup answer's body. The
/// rule reaches the signup probes at their first step and holds when the answer matches base.
fn scope_for(order: OrderId, base: &str) -> Scope {
    serde_json::from_value(json!({
        "version": "acan-scope/1",
        "order": order.to_string(),
        "rev": 1,
        "base_commit": base,
        "body": {
            "may": [],
            "must_keep": [{
                "id": SIGNUP_RULE,
                "selector": {
                    "surface": "http",
                    "method": "POST",
                    "route_template": "/api/signup",
                    "step_context": { "kind": "step", "step_id": "s1" },
                    "principal": { "kind": "anonymous" },
                    "channel": "body"
                },
                "obligations": [{
                    "obligation_id": SIGNUP_OBLIGATION,
                    "predicate": {
                        "predicate": "equals_base",
                        "step_id": "s1",
                        "path": "body",
                        "anchor": { "kind": "fresh" }
                    },
                    "text": "The signup answer's body matches the base run"
                }],
                "text": "Signup answers with the same body as before the change",
                "machine_ids": ["auth", "store"]
            }],
            "open": []
        }
    }))
    .expect("the demo scope decodes")
}

/// The human confirmation of `scope`, binding its hashes, selectors and examples.
fn confirmation_for(scope: &Scope) -> ScopeConfirmation {
    let binding = ScopeBinding::of(scope).expect("the scope binds");
    let order = serde_json::to_value(scope).expect("the scope serializes")["order"].clone();
    serde_json::from_value(json!({
        "version": "acan-scope-confirmation/1",
        "order": order,
        "scope_rev": scope.rev,
        "scope_hash": binding.scope_hash,
        "semantic_scope_hash": scope.semantic_scope_hash().expect("the scope hashes"),
        "base_commit": scope.base_commit,
        "ruling_versions": [],
        "selector_hashes": binding.selector_hashes,
        "example_hashes": binding.example_hashes,
        "answers": [],
        "broad_grants": [],
        "budget": {
            "repair_rounds": 2,
            "wall_time_ms": 600_000,
            "model_cost_micros": 0,
            "max_episodes": 4
        },
        "by": "owner",
        "at": 1_700_000_000_000_u64
    }))
    .expect("the demo confirmation decodes")
}

/// Waits for the offline split, registers the agent, and has the owner revise and confirm the
/// order's scope against the protected head. Returns the order and its confirmed scope.
async fn order_ready(running: &Running, root: &Path, task: TaskId) -> (OrderId, Scope) {
    let order = settle(running, "the offline split", |_, world| {
        world.task(task).map(|found| found.order)
    })
    .await;
    member(running, &HumanCmd::RegisterAgent { agent: agent() }).await;
    let base = git(root, &["rev-parse", PROTECTED_BRANCH])
        .trim()
        .to_owned();
    let scope = scope_for(order, &base);
    member(
        running,
        &HumanCmd::ReviseScope {
            order,
            scope: Box::new(scope.clone()),
        },
    )
    .await;
    member(
        running,
        &HumanCmd::ConfirmScope {
            order,
            confirmation: Box::new(confirmation_for(&scope)),
        },
    )
    .await;
    (order, scope)
}

/// The agent reads `task` back at the confirmed revision and hash, and waits until the readback is
/// stored. The claim needs this readback.
async fn read_back(running: &Running, task: TaskId, scope: &Scope) {
    let scope_hash = scope.scope_hash().expect("the scope hashes");
    agent_command(
        running,
        AgentCmd::PostScopedReadback {
            task,
            scope_rev: scope.rev,
            scope_hash,
            summary: "write the file".to_owned(),
            predictions: Vec::new(),
        },
    )
    .await;
    settle(running, "the scoped readback", |_, world| {
        world.scoped_readbacks_of(task).next().map(drop)
    })
    .await;
}

/// Confirms the scope of the task's order and reads the task back. Returns the confirmed scope.
async fn scope_first(running: &Running, root: &Path, task: TaskId) -> Scope {
    let (_, scope) = order_ready(running, root, task).await;
    read_back(running, task, &scope).await;
    scope
}

/// Claims the task and returns the fence the agent must present. The lease grant creates the
/// worktree on the job loop; the caller waits for it.
async fn claim(running: &Running, task: TaskId) -> Fence {
    agent_command(running, AgentCmd::Claim { task }).await;
    let fence = settle(running, "the claim", |_, world| {
        match &world.task(task)?.state {
            TaskState::Claimed { claim, .. } => Some(claim.fence(task)),
            _ => None,
        }
    })
    .await;
    keep_lease(running, fence);
    fence
}

/// Renews `fence`'s lease on a timer, as a worker does, until the test's runtime ends. A
/// lease lapses 60 seconds after its last renewal, and a chain under load can run longer.
///
/// A renewal the origin rejects means the claim is over, because the task landed, was released,
/// was claimed again, or its lane was stopped. The test's own steps check that outcome, so the
/// rejection is ignored.
fn keep_lease(running: &Running, fence: Fence) {
    let writer = running.writer().clone();
    tokio::spawn(async move {
        loop {
            sleep(LEASE_RENEWAL).await;
            let _ = writer
                .submit(Command::Agent {
                    agent: agent(),
                    cmd: AgentCmd::Renew { fence },
                })
                .await;
        }
    });
}

/// The scripted agent's build: declare the file, wait for the build to start, write and commit
/// the file in its worktree, and report the commit.
async fn build(
    running: &Running,
    root: &Path,
    task: TaskId,
    fence: Fence,
    file: &str,
    content: &str,
) -> CommitId {
    build_with(running, root, task, fence, &[file], |worktree| {
        write_file(worktree, file, content);
    })
    .await
}

/// The scripted agent's build with an arbitrary edit: declare `files`, wait for the build to
/// start, apply `edit` in the task worktree, commit everything, and report the commit.
async fn build_with(
    running: &Running,
    root: &Path,
    task: TaskId,
    fence: Fence,
    files: &[&str],
    edit: impl FnOnce(&Path),
) -> CommitId {
    declare(running, fence, files).await;
    wait_building(running, task).await;
    let dir = worktree_dir(root, task);
    edit(&dir);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "the agent's build"]);
    let head: CommitId = git(&dir, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("git prints a commit id");
    agent_command(
        running,
        AgentCmd::ReportBuilt {
            fence,
            head: head.clone(),
        },
    )
    .await;
    head
}

/// Declares a whole-file write of each of `files`.
async fn declare(running: &Running, fence: Fence, files: &[&str]) {
    agent_command(
        running,
        AgentCmd::Declare {
            fence,
            footprint: DeclarationInput {
                files: files
                    .iter()
                    .map(|file| FilePath::from_str(file).expect("valid file path"))
                    .collect(),
                ..DeclarationInput::default()
            },
        },
    )
    .await;
}

async fn wait_building(running: &Running, task: TaskId) {
    settle(running, "the build to start", |_, world| {
        in_state(world, task, |state| {
            matches!(
                state,
                TaskState::Claimed {
                    stage: Stage::Building { .. },
                    ..
                }
            )
        })
        .then_some(())
    })
    .await;
}

/// One `tools/call` to the MCP endpoint at `path` as `token`, and the status and JSON body it
/// answers with.
async fn rpc_tool(
    running: &Running,
    path: &str,
    token: &str,
    name: &str,
    arguments: Value,
) -> (reqwest::StatusCode, Value) {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    });
    let response = client()
        .post(url(running, path))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .expect("the MCP call is sent");
    let status = response.status();
    (
        status,
        response.json::<Value>().await.unwrap_or(Value::Null),
    )
}

async fn agent_command(running: &Running, cmd: AgentCmd) {
    running
        .writer()
        .submit(Command::Agent {
            agent: agent(),
            cmd,
        })
        .await
        .expect("the origin accepts the agent's command");
}

/// Writes `content` to `file` in `dir`, creating its directory.
fn write_file(dir: &Path, file: &str, content: &str) {
    let path = dir.join(file);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("the file's directory exists");
    }
    fs::write(&path, content).expect("the file is written");
}

/// Writes `file` in `dir`, commits it, and returns the new commit.
fn commit_file(dir: &Path, file: &str, content: &str) -> CommitId {
    write_file(dir, file, content);
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", &format!("write {file}")]);
    git(dir, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("git prints a commit id")
}

/// Commits `content` to `file` on the protected branch, as the user would. A scratch checkout
/// does the commit, so the user's own checkout stays put.
fn commit_on_protected(root: &Path, file: &str, content: &str) -> CommitId {
    let scratch = tempfile::tempdir().expect("scratch directory");
    let checkout = scratch.path().join("main");
    let checkout_arg = checkout.to_str().expect("UTF-8 path");
    git(root, &["worktree", "add", "-q", checkout_arg, "main"]);
    let commit = commit_file(&checkout, file, content);
    git(root, &["worktree", "remove", "--force", checkout_arg]);
    commit
}

/// Seeds the signup regression in the demo's auth handler: the answer upper-cases the username.
fn seed_regression(worktree: &Path) {
    let file = worktree.join("src").join("auth.ts");
    let source = read_text(&file);
    assert_eq!(
        source.matches(SIGNUP_RESPONSE).count(),
        1,
        "the signup response appears once in the pinned demo"
    );
    fs::write(
        &file,
        source.replace(SIGNUP_RESPONSE, REGRESSED_SIGNUP_RESPONSE),
    )
    .expect("the regression is written");
}

/// How many polls the hook waits for the hold file to go, at 50 ms each. It matches [`WAIT`], so
/// a test that never releases the hold fails at its own timeout.
const HOLD_POLLS: u32 = 6000;

/// Holds every update of the protected branch while the returned file exists.
///
/// Git runs the `reference-transaction` hook in the prepared state, before a ref moves, so a
/// land that reaches the protected branch waits there, and the task stays queued. Only updates
/// of `main` wait. Removing the file releases the land.
fn hold_protected_updates(root: &Path) -> PathBuf {
    let hold = root.join(".git").join("e2e-hold-protected");
    fs::write(&hold, "").expect("the hold file is written");
    let hooks = root.join(".git").join("hooks");
    fs::create_dir_all(&hooks).expect("the hooks directory exists");
    let hook = hooks.join("reference-transaction");
    // Git runs hooks with its own shell, which reads forward slashes on every platform.
    let hold_path = hold.to_string_lossy().replace('\\', "/");
    let script = format!(
        r#"#!/bin/sh
# Written by the e2e test. Waits while the hold file exists.
[ "$1" = prepared ] || exit 0
held=
while read -r _old _new name; do
  [ "$name" = refs/heads/main ] && held=1
done
[ -n "$held" ] || exit 0
polls=0
while [ -e '{hold_path}' ]; do
  [ "$polls" -lt {HOLD_POLLS} ] || exit 1
  sleep 0.05
  polls=$((polls + 1))
done
exit 0
"#
    );
    fs::write(&hook, script).expect("the hook is written");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755))
            .expect("the hook is executable");
    }
    hold
}

async fn worktree_route(running: &Running, token: &str, task: TaskId) -> reqwest::Response {
    client()
        .get(url(running, &format!("/api/tasks/{task}/worktree")))
        .bearer_auth(token)
        .send()
        .await
        .expect("the worktree request is sent")
}

/// The lease holder's worktree view over HTTP, which must answer.
async fn worktree_view(running: &Running, task: TaskId) -> Value {
    let response = worktree_route(running, AGENT_TOKEN, task).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    response.json().await.expect("the route answers JSON")
}

/// The lease holder's worktree view over MCP: the `get_worktree` tool's answer, parsed from its
/// text. The tool must not fail.
async fn mcp_worktree(running: &Running, task: TaskId) -> Value {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "get_worktree", "arguments": { "task": task.to_string() } },
    });
    let reply: Value = client()
        .post(url(running, "/mcp"))
        .bearer_auth(AGENT_TOKEN)
        .json(&body)
        .send()
        .await
        .expect("the MCP call is sent")
        .json()
        .await
        .expect("the reply is JSON-RPC");
    let result = &reply["result"];
    assert_eq!(result["isError"], false, "the tool answers: {reply}");
    let text = result["content"][0]["text"]
        .as_str()
        .expect("the result has its text");
    serde_json::from_str(text).expect("the text is JSON")
}

/// A file's text, which the test expects to exist.
fn read_text(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

/// Waits until the loop has finished creating `task`'s worktree at `generation`.
///
/// The directory appears before `git worktree add` has written the worktree, so a directory check
/// lets a commit run against the enclosing repository. The generation is recorded last, once the
/// worktree is complete.
async fn wait_for_worktree(root: &Path, task: TaskId, generation: Generation) {
    let key = format!("branch.acan/{task}.acanGeneration");
    let expected = generation.get().to_string();
    timeout(WAIT, async {
        while git_config(root, &key).trim() != expected {
            sleep(POLL).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!("the worktree of {task} at generation {expected} was never recorded")
    });
}

fn in_state(world: &World, task: TaskId, state: impl Fn(&TaskState) -> bool) -> bool {
    world.task(task).is_some_and(|t| state(&t.state))
}

/// The stored log and the world it replays to, read through the owner's events route.
async fn snapshot(running: &Running) -> (Vec<Stamped>, Origin) {
    snapshot_at(running, "/api/events").await
}

/// The stored log at `path`, and the world it replays to.
async fn snapshot_at(running: &Running, path: &str) -> (Vec<Stamped>, Origin) {
    let log: Vec<Stamped> = client()
        .get(url(running, path))
        .bearer_auth(OWNER_TOKEN)
        .send()
        .await
        .expect("the events request is sent")
        .error_for_status()
        .expect("the owner may read the log")
        .json()
        .await
        .expect("the log is JSON");
    let origin = Origin::replay(log.iter().cloned()).expect("the stored log replays");
    (log, origin)
}

/// Polls the stored log until `done` accepts it, and returns what it found. A timeout names the
/// newest events, so a stalled step shows where the chain stopped.
async fn settle<T>(
    running: &Running,
    what: &str,
    done: impl Fn(&[Stamped], &World) -> Option<T>,
) -> T {
    settle_at(running, "/api/events", what, done).await
}

/// Waits until `done` accepts the log at `path`, the way [`settle`] does for the default project.
async fn settle_at<T>(
    running: &Running,
    path: &str,
    what: &str,
    done: impl Fn(&[Stamped], &World) -> Option<T>,
) -> T {
    let result = timeout(WAIT, async {
        loop {
            let (log, origin) = snapshot_at(running, path).await;
            if let Some(found) = done(&log, origin.world()) {
                return found;
            }
            sleep(POLL).await;
        }
    })
    .await;
    let Ok(found) = result else {
        let (log, _) = snapshot_at(running, path).await;
        // Lease renewals arrive every few seconds and would hide the steps that stalled.
        let newest: Vec<&Event> = log
            .iter()
            .rev()
            .filter(|stamped| !matches!(stamped.event, Event::LeaseRenewed { .. }))
            .take(6)
            .map(|stamped| &stamped.event)
            .collect();
        panic!("timed out after {WAIT:?} waiting for {what}; newest events first: {newest:?}");
    };
    found
}
