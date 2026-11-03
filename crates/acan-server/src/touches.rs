//! Live transition touches: the state-machine transitions that an accepted write touches.
//!
//! When the writer persists a batch, each claim (`Declared`), built commit (`BuildReported`) and
//! landed commit (`Landed`) maps to the transitions its lines cover, and each one goes out as a
//! `transition_touched` frame. A frame is live only. It is not stored, has no seq, and a resuming
//! stream never replays it, so a client that missed one reads the plant again.
//!
//! The mapping is [`acan_scan::touched`]. This module adds the server's parts: the diff of a commit
//! against its first parent, and the frame's shape.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use acan_core::{Declaration, Event, Stamped, TaskId};
use acan_scan::{Change, Touched, touched};
use anyhow::Context as _;
use serde::Serialize;
use tokio::sync::broadcast;

use crate::index::Scan;

/// Where the writer sends its touch frames, and the repository its built and landed commits are read
/// from.
pub(crate) struct TouchSink {
    pub(crate) sender: broadcast::Sender<TransitionTouch>,
    pub(crate) repo: PathBuf,
}

/// The frame's type tag. It has one variant, so every touch frame carries the same tag.
#[derive(Clone, Copy, Debug, Serialize)]
enum Frame {
    #[serde(rename = "transition_touched")]
    TransitionTouched,
}

/// Which kind of change touched the transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum By {
    /// A task's claim, which declares its footprint.
    Claim,
    /// A commit: a task's build or its landing.
    Commit,
}

/// The point in a task's life that touched the transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Stage {
    /// The task declared its footprint.
    Declared,
    /// The task reported its build.
    Built,
    /// The task landed.
    Landed,
}

/// A state-machine transition that an accepted write touches. It is sent live as
/// `{"type":"transition_touched", ...}` and is never stored.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct TransitionTouch {
    #[serde(rename = "type")]
    kind: Frame,
    /// The state machine's id, `{file}#{name}`.
    machine: String,
    from: String,
    to: String,
    /// Repo-relative file of the transition site.
    file: String,
    /// 1-based first line of the transition.
    line: u32,
    /// 1-based last line of the transition, inclusive.
    end_line: u32,
    by: By,
    stage: Stage,
    task: TaskId,
    /// The commit of a commit stage. `None` for a claim.
    commit: Option<String>,
}

impl TransitionTouch {
    fn of(touch: &Touched<'_>, by: By, stage: Stage, task: TaskId, commit: Option<&str>) -> Self {
        Self {
            kind: Frame::TransitionTouched,
            machine: touch.machine.id.clone(),
            from: touch.transition.from.clone(),
            to: touch.transition.to.clone(),
            file: touch.transition.file.clone(),
            line: touch.transition.line,
            end_line: touch.transition.end_line,
            by,
            stage,
            task,
            commit: commit.map(str::to_owned),
        }
    }
}

/// The frames for the transitions a claim's footprint touches. The footprint's files and inserts
/// are whole files, and its symbols and interfaces are symbols.
pub(crate) fn frames_for_claim(scan: &Scan, decl: &Declaration) -> Vec<TransitionTouch> {
    let footprint = &decl.footprint;
    let files: Vec<&str> = footprint
        .files
        .iter()
        .chain(&footprint.inserts)
        .map(|file| file.as_str())
        .collect();
    let symbols: Vec<(&str, &str)> = footprint
        .symbols
        .iter()
        .chain(&footprint.interfaces)
        .map(|symbol| (symbol.file.as_str(), symbol.name.as_str()))
        .collect();
    let change = Change::Claim {
        files: &files,
        symbols: &symbols,
    };
    touched(&scan.plant.state_machines, &scan.plant.files, change)
        .iter()
        .map(|touch| TransitionTouch::of(touch, By::Claim, Stage::Declared, decl.task, None))
        .collect()
}

/// The frames for the transitions that a commit's `diff` touches. `stage` names the commit's stage,
/// and `commit` is the commit the diff is of.
pub(crate) fn frames_for_diff(
    scan: &Scan,
    diff: &str,
    task: TaskId,
    stage: Stage,
    commit: &str,
) -> Vec<TransitionTouch> {
    touched(
        &scan.plant.state_machines,
        &scan.plant.files,
        Change::Commit { diff },
    )
    .iter()
    .map(|touch| TransitionTouch::of(touch, By::Commit, stage, task, Some(commit)))
    .collect()
}

/// Sends the touch frames of a persisted batch. The writer calls this after the batch is stored and
/// published, so a claim's frames come from the scan the batch was decided against. A commit's diff
/// is read in its own task, so git never delays the writer. Nothing here fails the write.
pub(crate) fn publish_batch(batch: &[Stamped], scan: &Arc<Scan>, sink: &TouchSink) {
    // Frames are live only, so with no subscriber there is nothing to derive.
    if sink.sender.receiver_count() == 0 {
        return;
    }
    for stamped in batch {
        match &stamped.event {
            Event::Declared { decl } => send_all(&sink.sender, frames_for_claim(scan, decl)),
            Event::BuildReported { task, head, .. } => {
                spawn_commit_frames(Arc::clone(scan), sink, *task, Stage::Built, head.as_str())
            }
            Event::Landed { task, commit } => spawn_commit_frames(
                Arc::clone(scan),
                sink,
                *task,
                Stage::Landed,
                commit.as_str(),
            ),
            _ => {}
        }
    }
}

fn send_all(sender: &broadcast::Sender<TransitionTouch>, frames: Vec<TransitionTouch>) {
    for frame in frames {
        // A send fails only when nobody is subscribed, which is fine.
        let _ = sender.send(frame);
    }
}

/// Reads the diff of `commit` in a task of its own, then sends its frames. A failure logs and sends
/// nothing.
fn spawn_commit_frames(
    scan: Arc<Scan>,
    sink: &TouchSink,
    task: TaskId,
    stage: Stage,
    commit: &str,
) {
    let sender = sink.sender.clone();
    let repo = sink.repo.clone();
    let commit = commit.to_owned();
    tokio::spawn(async move {
        match commit_diff(repo, commit.clone()).await {
            Ok(Some(diff)) => {
                let frames = frames_for_diff(&scan, &diff, task, stage, &commit);
                send_all(&sender, frames);
            }
            // A root commit has no parent to diff against, so it touches nothing.
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(error = ?err, %commit, "reading a commit's diff for transition touches failed; no touches sent");
            }
        }
    });
}

/// The diff of `commit` against its first parent, read in a blocking task. `None` for a root commit.
async fn commit_diff(repo: PathBuf, commit: String) -> anyhow::Result<Option<String>> {
    tokio::task::spawn_blocking(move || diff_of_commit(&repo, &commit))
        .await
        .context("the commit diff task did not finish")?
}

fn diff_of_commit(repo: &Path, commit: &str) -> anyhow::Result<Option<String>> {
    // The commit and its parents. A root commit lists no parent.
    let listed = git(repo, &["rev-list", "--parents", "-n", "1", commit])?;
    if listed.split_whitespace().count() < 2 {
        return Ok(None);
    }
    let parent = format!("{commit}^");
    let diff = git(
        repo,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "--unified=0",
            "--no-renames",
            "--no-color",
            "--no-ext-diff",
            parent.as_str(),
            commit,
        ],
    )?;
    Ok(Some(diff))
}

/// Runs git in `repo` and returns its standard output. A non-zero exit is an error that carries
/// git's message. The environment is prepared as the other git calls prepare theirs.
fn git(repo: &Path, args: &[&str]) -> anyhow::Result<String> {
    let mut command = std::process::Command::new("git");
    acan_git::git_env::prepare(&mut command);
    let output = command
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    anyhow::ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use acan_core::{CodeRev, DeclarationInput, Generation, SymbolRef};
    use serde_json::{Value, json};

    use super::*;
    use crate::index;

    /// The order file of the scanner's states fixture, which holds one machine with transitions.
    const ORDER: &str = "export type OrderStatus = 'open' | 'paid' | 'shipped';\n\nexport function settle(order: { status: OrderStatus }) {\n  switch (order.status) {\n    case 'open':\n      order.status = 'paid';\n      break;\n    case 'paid':\n      order.status = 'shipped';\n      break;\n  }\n}\n";

    fn task(n: u32) -> TaskId {
        TaskId::from_str(&format!("t-{n}")).expect("a task id")
    }

    /// Runs git in `dir`, failing the test on a non-zero exit.
    fn git_in(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// A temp repo with `order.ts` in it, scanned into a [`Scan`].
    fn scanned_order(dir: &Path) -> Scan {
        std::fs::write(dir.join("order.ts"), ORDER).expect("write order.ts");
        index::scan(dir).expect("scan the temp repo")
    }

    #[test]
    fn a_claim_frame_has_exactly_the_stream_shape() {
        let frame = TransitionTouch {
            kind: Frame::TransitionTouched,
            machine: "ts/order.ts#OrderStatus".to_owned(),
            from: "open".to_owned(),
            to: "paid".to_owned(),
            file: "ts/order.ts".to_owned(),
            line: 5,
            end_line: 5,
            by: By::Claim,
            stage: Stage::Declared,
            task: task(3),
            commit: None,
        };

        assert_eq!(
            serde_json::to_value(&frame).expect("the frame encodes"),
            json!({
                "type": "transition_touched",
                "machine": "ts/order.ts#OrderStatus",
                "from": "open",
                "to": "paid",
                "file": "ts/order.ts",
                "line": 5,
                "end_line": 5,
                "by": "claim",
                "stage": "declared",
                "task": 3,
                "commit": null,
            })
        );
    }

    #[test]
    fn a_commit_frame_carries_its_commit() {
        let frame = TransitionTouch {
            kind: Frame::TransitionTouched,
            machine: "order.ts#OrderStatus".to_owned(),
            from: "*".to_owned(),
            to: "shipped".to_owned(),
            file: "order.ts".to_owned(),
            line: 9,
            end_line: 10,
            by: By::Commit,
            stage: Stage::Landed,
            task: task(7),
            commit: Some("abc123".to_owned()),
        };

        let value: Value = serde_json::to_value(&frame).expect("the frame encodes");
        assert_eq!(value["by"], "commit");
        assert_eq!(value["stage"], "landed");
        assert_eq!(value["commit"], "abc123");
        assert_eq!(value["task"], 7);
    }

    #[test]
    fn a_claim_touches_the_transitions_of_its_symbols_files_and_inserts() {
        let dir = tempfile::tempdir().expect("temp dir");
        let scan = scanned_order(dir.path());
        let in_order: Vec<(u32, u32)> = scan
            .plant
            .state_machines
            .iter()
            .flat_map(|machine| &machine.transitions)
            .filter(|transition| transition.file == "order.ts")
            .map(|transition| (transition.line, transition.end_line))
            .collect();
        assert!(!in_order.is_empty(), "order.ts holds transitions");
        let decl = |footprint: DeclarationInput| Declaration {
            task: task(1),
            generation: Generation::new(1),
            index: CodeRev::from_str("rev-test").expect("a revision"),
            footprint,
        };

        // A symbol whose body changes: only the transitions in its range.
        let by_symbol = frames_for_claim(
            &scan,
            &decl(DeclarationInput {
                symbols: vec![SymbolRef {
                    file: acan_core::FilePath::from_str("order.ts").expect("a path"),
                    name: "settle".to_owned(),
                }],
                ..DeclarationInput::default()
            }),
        );
        let settle_lines = scan
            .plant
            .files
            .iter()
            .find(|file| file.path == "order.ts")
            .and_then(|file| file.symbols.iter().find(|symbol| symbol.name == "settle"))
            .expect("settle is a symbol")
            .line_range;
        assert!(!by_symbol.is_empty());
        assert!(by_symbol.iter().all(|frame| frame.line >= settle_lines.start
            && frame.end_line <= settle_lines.end));

        // An insert is a whole file: every transition in it.
        let by_insert = frames_for_claim(
            &scan,
            &decl(DeclarationInput {
                inserts: vec![acan_core::FilePath::from_str("order.ts").expect("a path")],
                ..DeclarationInput::default()
            }),
        );
        let lines: Vec<(u32, u32)> = by_insert
            .iter()
            .map(|frame| (frame.line, frame.end_line))
            .collect();
        assert_eq!(lines, in_order);
        assert!(by_insert.iter().all(|frame| frame.by == By::Claim
            && frame.stage == Stage::Declared
            && frame.commit.is_none()
            && frame.task == task(1)));

        // A file the plant does not hold, and no symbol: nothing.
        let elsewhere = frames_for_claim(
            &scan,
            &decl(DeclarationInput {
                files: vec![acan_core::FilePath::from_str("other.ts").expect("a path")],
                ..DeclarationInput::default()
            }),
        );
        assert!(elsewhere.is_empty());
    }

    #[test]
    fn a_commit_hunk_on_a_transition_line_yields_built_frames_with_the_commit() {
        let dir = tempfile::tempdir().expect("temp dir");
        let scan = scanned_order(dir.path());
        let first = scan
            .plant
            .state_machines
            .iter()
            .flat_map(|machine| &machine.transitions)
            .find(|transition| transition.file == "order.ts")
            .expect("order.ts holds a transition")
            .line;
        let diff = format!(
            "diff --git a/order.ts b/order.ts\n--- a/order.ts\n+++ b/order.ts\n@@ -{first} +{first} @@\n-a\n+b\n"
        );

        let frames = frames_for_diff(&scan, &diff, task(4), Stage::Built, "deadbeef");

        assert!(!frames.is_empty());
        assert!(frames.iter().all(|frame| frame.by == By::Commit
            && frame.stage == Stage::Built
            && frame.task == task(4)
            && frame.commit.as_deref() == Some("deadbeef")));
        assert!(frames.iter().all(|frame| frame.line == first));
    }

    #[test]
    fn a_root_commit_has_no_diff_and_a_child_commit_has_one() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();
        git_in(root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("order.ts"), ORDER).expect("write order.ts");
        git_in(root, &["add", "-A"]);
        git_in(root, &["commit", "-q", "-m", "the root"]);
        let root_commit = git_in(root, &["rev-parse", "HEAD"]);
        std::fs::write(root.join("order.ts"), ORDER.replace("'paid'", "'paid' ")).expect("edit");
        git_in(root, &["add", "-A"]);
        git_in(root, &["commit", "-q", "-m", "the child"]);
        let child_commit = git_in(root, &["rev-parse", "HEAD"]);

        assert_eq!(
            diff_of_commit(root, &root_commit).expect("the root lists"),
            None,
            "a root commit has no parent to diff against"
        );
        let diff = diff_of_commit(root, &child_commit)
            .expect("the child diffs")
            .expect("a child has a parent");
        assert!(diff.contains("+++ b/order.ts"), "{diff}");
        assert!(diff.contains("@@ -"), "{diff}");
    }
}
