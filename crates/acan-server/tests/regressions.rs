//! Contiguous logs across cancellation and shutdown, atomic rescans, and a static root that
//! cannot expose private state.

use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use acan_core::{
    Command, HumanCmd, MemberId, Origin, Role as MemberRole, Seq, SystemCmd, Timestamp, WorldId,
    WorldRef,
};
use acan_server::{
    DEFAULT_PROBE_CONCURRENCY, DEFAULT_WITNESS_CONCURRENCY, EventStore, Host, HostParts, Running,
    ServerConfig, Tokens, scan, start,
};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Connection, SqliteConnection};
use tokio::time::{sleep, timeout};

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temporary repository");
    fs::create_dir_all(dir.path().join("model")).expect("model directory");
    fs::write(
        dir.path().join("model/store.ts"),
        "export function load() { return 1; }\n",
    )
    .expect("fixture");
    dir
}

fn owner() -> MemberId {
    MemberId::from_str("owner").expect("valid member id")
}

fn add_member(name: &str) -> Command {
    Command::Human {
        member: owner(),
        cmd: HumanCmd::AddMember {
            member: MemberId::from_str(name).expect("valid member id"),
            role: MemberRole::Member,
        },
    }
}

fn config(dir: &tempfile::TempDir) -> ServerConfig {
    ServerConfig {
        repo: dir.path().to_path_buf(),
        addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        team: false,
        web: None,
        protected_branch: None,
        witness_http_base_url: None,
        witness_concurrency: DEFAULT_WITNESS_CONCURRENCY,
    }
}

fn token_of(running: &Running) -> String {
    running
        .owner_token
        .as_ref()
        .expect("the first start mints the owner token")
        .as_str()
        .to_owned()
}

async fn host_for(dir: &tempfile::TempDir) -> (Host, EventStore) {
    let state = dir.path().join(".acan");
    fs::create_dir_all(&state).expect("state dir");
    let path = state.join("world.db");
    let store = EventStore::open(&path).await.expect("event store");
    let (origin, opened) = Origin::open(
        WorldRef {
            id: WorldId::from_str("test").expect("valid world id"),
            repo: dir.path().display().to_string(),
        },
        owner(),
        Timestamp::from_millis(1),
    );
    store
        .append(&[opened])
        .await
        .expect("the open event stores");
    let (tokens, _) = Tokens::load_or_create(&state, &owner()).expect("tokens");
    let (_shutdown_tx, shutdown) = tokio::sync::watch::channel(false);
    let host = Host::new(HostParts {
        origin,
        store: store.clone(),
        tokens,
        scan: scan(dir.path()).expect("scan"),
        repo: dir.path().to_path_buf(),
        shutdown,
        git: None,
        witness_http_base_url: None,
        witness_concurrency: DEFAULT_WITNESS_CONCURRENCY,
        probe_concurrency: DEFAULT_PROBE_CONCURRENCY,
    });
    (host, store)
}

async fn lock_writes(path: &std::path::Path) -> SqliteConnection {
    let mut connection =
        SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path))
            .await
            .expect("database connection");
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut connection)
        .await
        .expect("the write lock is taken");
    connection
}

async fn unlock_writes(connection: &mut SqliteConnection) {
    sqlx::query("ROLLBACK")
        .execute(connection)
        .await
        .expect("the write lock is released");
}

#[tokio::test]
async fn a_cancelled_submit_still_commits_and_the_log_stays_contiguous() {
    let dir = repo();
    let path = dir.path().join(".acan/world.db");
    let (host, store) = host_for(&dir).await;
    let mut blocker = lock_writes(&path).await;

    let writer_host = host.clone();
    let task = tokio::spawn(async move { writer_host.submit(add_member("kept")).await });
    sleep(Duration::from_millis(100)).await;

    // Reads come from the published snapshot, so the blocked write holds nothing up.
    assert_eq!(
        host.read(acan_core::World::next_seq),
        Seq::new(1),
        "reads never wait on a database write"
    );

    // Dropping the caller only drops the reply; the accepted write still commits.
    task.abort();
    let _ = task.await;
    unlock_writes(&mut blocker).await;

    let events = host
        .submit(add_member("next"))
        .await
        .expect("the next command is accepted");
    assert_eq!(
        events
            .iter()
            .map(|event| event.seq.get())
            .collect::<Vec<_>>(),
        [2],
        "the cancelled write landed first, as seq 1"
    );
    let kept = MemberId::from_str("kept").expect("valid member id");
    assert!(
        host.read(|world| world.member(&kept).is_some()),
        "the cancelled caller's member is in the world"
    );

    let log = store.load_from(Seq::new(0)).await.expect("log");
    let seqs: Vec<u64> = log.iter().map(|event| event.seq.get()).collect();
    assert_eq!(seqs, [0, 1, 2], "no gap from the cancelled caller");
    Origin::replay(log).expect("the stored log replays");
    host.shutdown().await.expect("the writer stops");
}

#[tokio::test]
async fn stop_waits_for_an_in_flight_write_and_the_log_replays_afterwards() {
    let dir = repo();
    let running = start(config(&dir)).await.expect("start");
    let token = token_of(&running);
    let addr = running.local_addr;
    let path = dir.path().join(".acan/world.db");
    let mut blocker = lock_writes(&path).await;

    let request = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("http://{addr}/api/commands"))
            .bearer_auth(token)
            .json(&HumanCmd::AddMember {
                member: MemberId::from_str("in-flight").expect("valid member id"),
                role: MemberRole::Member,
            })
            .send()
            .await
    });
    sleep(Duration::from_millis(100)).await;

    // The CLI cancels `wait` on Ctrl+C before it calls `stop`; that must not skip the barrier.
    let mut running = running;
    assert!(
        timeout(Duration::from_millis(20), running.wait())
            .await
            .is_err(),
        "the listener is still serving"
    );
    let stopper = tokio::spawn(running.stop());
    sleep(Duration::from_millis(80)).await;
    unlock_writes(&mut blocker).await;

    let response = timeout(Duration::from_secs(3), request)
        .await
        .expect("the in-flight request finishes")
        .expect("the request task")
        .expect("the request");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::OK,
        "a write accepted before stop still commits"
    );
    stopper
        .await
        .expect("the stop task")
        .expect("stop completes");

    let refused = reqwest::Client::new()
        .get(format!("http://{addr}/health"))
        .send()
        .await;
    assert!(refused.is_err(), "the listener is closed after stop");

    let store = EventStore::open(&path).await.expect("reopen");
    let log = store.load_from(Seq::new(0)).await.expect("log");
    let seqs: Vec<u64> = log.iter().map(|event| event.seq.get()).collect();
    assert_eq!(seqs, [0, 1, 2], "no write outlived the stop barrier");
    Origin::replay(log).expect("the stored log replays after a restart-worthy stop");
}

#[tokio::test]
async fn a_failed_rescan_keeps_the_committed_scan_and_the_writer_recovers() {
    let dir = repo();
    let path = dir.path().join(".acan/world.db");
    let (host, store) = host_for(&dir).await;
    host.submit(Command::System(SystemCmd::IndexUpdated))
        .await
        .expect("the initial index is recorded");
    let first = host.scan().index.rev.clone();

    fs::write(
        dir.path().join("model/store.ts"),
        "export function load() { return 2; }\n",
    )
    .expect("edit the source");
    let mut connection =
        SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
            .await
            .expect("database connection");
    sqlx::query("CREATE TRIGGER reject_insert BEFORE INSERT ON events BEGIN SELECT RAISE(FAIL, 'injected failure'); END")
        .execute(&mut connection)
        .await
        .expect("inject the append failure");

    let err = host.rescan().await.expect_err("the rescan cannot persist");

    assert!(
        err.to_string().contains("injected failure"),
        "the write failure surfaces: {err}"
    );
    assert_eq!(
        host.scan().index.rev,
        first,
        "the published scan still matches the committed world"
    );
    assert_eq!(
        host.read(|world| world.index_rev().cloned()),
        Some(first.clone()),
        "the world kept its index revision"
    );
    let log = store.load_from(Seq::new(0)).await.expect("log");
    assert_eq!(log.len(), 2, "nothing was stored for the failed rescan");

    sqlx::query("DROP TRIGGER reject_insert")
        .execute(&mut connection)
        .await
        .expect("remove the injected failure");
    let events = host
        .rescan()
        .await
        .expect("the log still ends at the committed world, so the writer keeps serving");
    assert_eq!(
        events
            .iter()
            .map(|event| event.seq.get())
            .collect::<Vec<_>>(),
        [2],
        "the retried rescan takes the next seq"
    );
    assert_ne!(
        host.scan().index.rev,
        first,
        "the new scan is published with its batch"
    );
    assert_eq!(
        host.read(|world| world.index_rev().cloned()),
        Some(host.scan().index.rev.clone()),
        "world and scan move together"
    );
    let log = store.load_from(Seq::new(0)).await.expect("log");
    Origin::replay(log).expect("the stored log replays");
    host.shutdown().await.expect("the writer stops");
}

#[tokio::test]
async fn a_web_root_that_holds_private_state_is_refused() {
    let dir = repo();
    for web in [dir.path().to_path_buf(), dir.path().join(".acan")] {
        fs::create_dir_all(&web).expect("web directory");
        let config = ServerConfig {
            web: Some(web.clone()),
            ..config(&dir)
        };
        let err = start(config)
            .await
            .expect_err("a root holding private state is refused");
        assert!(
            err.to_string().contains("web directory"),
            "the refusal names the web directory for {}: {err}",
            web.display()
        );
    }
}

/// Links `link` to the directory `target` the way an unprivileged user can.
fn link_dir(target: &Path, link: &Path) {
    #[cfg(windows)]
    {
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .status()
            .expect("mklink runs");
        assert!(status.success(), "the junction is created");
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).expect("the symlink is created");
}

#[tokio::test]
async fn public_files_are_served_but_private_state_is_not_reachable_without_a_token() {
    let dir = repo();
    let public = dir.path().join("public");
    fs::create_dir_all(&public).expect("public directory");
    fs::write(public.join("index.html"), "<p>acan</p>").expect("index page");
    let running = start(ServerConfig {
        web: Some(public.clone()),
        ..config(&dir)
    })
    .await
    .expect("a dedicated public directory is accepted");
    link_dir(&dir.path().join(".acan"), &public.join("leak"));
    let http = reqwest::Client::new();
    let get = |path: &str| {
        let url = format!("http://{}{path}", running.local_addr);
        let http = http.clone();
        async move { http.get(url).send().await.expect("the request is sent") }
    };

    let page = get("/").await;
    assert_eq!(
        page.status(),
        reqwest::StatusCode::OK,
        "the index page is public"
    );
    assert_eq!(page.text().await.expect("page body"), "<p>acan</p>");

    for path in [
        "/.acan/tokens.json",
        "/leak/tokens.json",
        "/%2e%2e/.acan/tokens.json",
        "/..%2f.acan%2ftokens.json",
        "/..%5c.acan%5ctokens.json",
        "/model/store.ts",
    ] {
        let response = get(path).await;
        assert_eq!(
            response.status(),
            reqwest::StatusCode::NOT_FOUND,
            "GET {path} without a token"
        );
    }
    running.stop().await.expect("the server stops");
}
