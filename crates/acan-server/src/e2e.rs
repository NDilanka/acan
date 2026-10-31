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
    HumanCmd, LEASE_TTL_MS, LaneVerdict, MachineKey, OrderId, Origin, ScopeBinding, Stage, Stamped,
    TaskId, TaskState, World,
};
use acan_intent::{ItemClass, Scope, ScopeConfirmation};
use acan_llm::{
    CallRecord, ChatRequest, ChatResponse, LlmError, ModelClient, Role, Usage, UsageLedger,
};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout};

use crate::model::{self, Ask};
use crate::offline::Offline;
use crate::{DEFAULT_WITNESS_CONCURRENCY, Running, ServerConfig, start};

const OWNER_TOKEN: &str = "acan_owner_e2e";
const AGENT_TOKEN: &str = "acan_agent_e2e";
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
#[ignore = "product gap: a protected head that moves after confirmation refuses CandidatePrepared as stale evidence (lane.rs stale check, jobs.rs contrast)"]
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
#[ignore = "product gap: a candidate that conflicts with the protected head records no refusal (jobs.rs prepare_candidate)"]
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
    // The branch is still the one the gate passed, not a leftover the loop should remove.
    settle(
        &running,
        "the queued branch to stay at its generation",
        |_, _| {
            let stays = worktree_dir(root, task).is_dir() && branch_exists(root, "acan/t-0");
            let recorded = git_config(root, "branch.acan/t-0.acanGeneration");
            (stays && recorded.trim() == generation.to_string()).then_some(())
        },
    )
    .await;
    assert_eq!(
        git(&worktree_dir(root, task), &["rev-parse", "HEAD"]).trim(),
        built.to_string(),
        "the queued branch still holds the gated commit"
    );

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
#[ignore = "product gap: a head moved by the predecessor's land refuses CandidatePrepared as stale evidence (lane.rs stale check, jobs.rs contrast)"]
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
}

impl ModelClient for Drafting {
    async fn chat(&self, role: Role, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        if model::ask_of(&request) != Some(Ask::Scope) {
            return self.inner.chat(role, request).await;
        }
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

/// A draft that never checks is recorded as a failure the owner can see. Nothing stands in for it,
/// so the lane stays closed.
#[tokio::test]
async fn a_scope_draft_that_never_checks_is_recorded_as_a_failure() {
    let repo = demo_repo();
    let root = repo.path();
    let foreman = Drafting {
        inner: TwoTasks::default(),
        draft: json!({ "may": [], "must_keep": [], "open": [] }),
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
    let (_, origin) = snapshot(&running).await;
    assert!(origin.world().lane(order).is_none(), "no scope is opened");
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
fn write_tokens(root: &Path) {
    let state = root.join(".acan");
    fs::create_dir_all(&state).expect("state directory");
    fs::write(
        state.join("tokens.json"),
        format!(
            r#"[
              {{"token": "{OWNER_TOKEN}", "principal": {{"kind": "member", "id": "owner"}}}},
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
    };
    start(config, client, ledger)
        .await
        .expect("the server starts on the test repository")
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
    let response = client()
        .post(url(running, "/api/commands"))
        .bearer_auth(OWNER_TOKEN)
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
    member(
        running,
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
/// or was claimed again. The test's own steps check that outcome, so the rejection is ignored.
fn keep_lease(running: &Running, fence: Fence) {
    let writer = running.writer.clone();
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

async fn agent_command(running: &Running, cmd: AgentCmd) {
    running
        .writer
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
    let log: Vec<Stamped> = client()
        .get(url(running, "/api/events"))
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
    let result = timeout(WAIT, async {
        loop {
            let (log, origin) = snapshot(running).await;
            if let Some(found) = done(&log, origin.world()) {
                return found;
            }
            sleep(POLL).await;
        }
    })
    .await;
    let Ok(found) = result else {
        let (log, _) = snapshot(running).await;
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
