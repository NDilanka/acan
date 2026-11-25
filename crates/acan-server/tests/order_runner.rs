//! Worktree order attempts against a scripted Droid executable, without credentials or a model.

#![cfg(any(unix, windows))]

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use acan_droid::{CleanEnv, RequestId, SessionInit, SessionSettings, SpawnConfig};
use acan_intent::{Block, GenUiPlace, parse_and_validate_proposal};
use acan_server::order_runner::{
    Card, CardDecision, CardKind, DeveloperAuthority, MAX_CONCURRENT_ORDERS, MAX_ONE_LINE_CHARS,
    OrderRunSpec, OrderRunState, OrderRunnerError, OrderRunnerManager, PersistedOrderState,
    reconcile_on_startup,
};
use acan_server::{Principal, Tokens};
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(10);
const PROMPT: &str = "Apply the order";
const ENDPOINT: &str = "http://127.0.0.1:1234/hooks";
const HANDSHAKE: [&str; 2] = ["droid.initialize_session", "droid.update_session_settings"];
const CLOSE: Step = Step::Expect(r#""method":"droid.close_session""#);

fn developer() -> DeveloperAuthority {
    DeveloperAuthority::from_principal(&Principal::Member("owner".parse().expect("member id")))
        .expect("a member has approval authority")
}

fn select(option_id: &str) -> CardDecision {
    CardDecision::SelectOption {
        option_id: option_id.to_owned(),
    }
}

fn other(note: impl Into<String>) -> CardDecision {
    CardDecision::Other { note: note.into() }
}

fn frame(fields: Value) -> Step {
    let mut fields = fields.as_object().expect("frame fields").clone();
    fields.insert("jsonrpc".into(), json!("2.0"));
    fields.insert("factoryApiVersion".into(), json!("1.0.0"));
    Step::Emit(Value::Object(fields))
}

fn response(id: u32, result: Value) -> Step {
    frame(json!({"type":"response","id":id,"result":result}))
}

fn notification(method: &str, params: Value) -> Step {
    frame(json!({"type":"notification","method":method,"params":params}))
}

fn complete() -> Step {
    notification("droid.turn_completed", json!({"reason":"completed"}))
}

fn permission() -> Step {
    frame(json!({
        "type":"request","id":"permission-1","method":"droid.request_permission","params":{
            "toolUses":[{
                "toolUse":{"id":"tool-1","name":"Script"},
                "details":{"calls":[
                    {"toolName":"docs___preview","confirmation":{"type":"mcp_tool"}},
                    {"toolName":"Execute","toolInput":{"command":"echo preview"},"confirmation":{"type":"exec"}}
                ]}
            }],
            "options":[{"label":"Allow once","value":"proceed_once"},{"label":"Cancel","value":"cancel"}]
        }
    }))
}

fn questions(questions: Value) -> Step {
    frame(json!({
        "type":"request","id":101,"method":"droid.ask_user",
        "params":{"toolCallId":"ask-1","questions":questions}
    }))
}

fn handshake() -> [Step; 4] {
    [
        Step::Expect(HANDSHAKE[0]),
        response(1, json!({"sessionId":"fixture-session"})),
        Step::Expect(HANDSHAKE[1]),
        response(2, json!({"modelId":"fixture-model","autonomyLevel":"low"})),
    ]
}

fn turn(prompt: &'static str, steps: impl IntoIterator<Item = Step>) -> Vec<Step> {
    handshake()
        .into_iter()
        .chain([Step::UserMessage(prompt)])
        .chain(steps)
        .collect()
}

enum Step {
    Expect(&'static str),
    UserMessage(&'static str),
    Emit(Value),
    WaitForRelease,
}

struct FakeDroid {
    dir: tempfile::TempDir,
    executable: PathBuf,
}

impl FakeDroid {
    fn new(steps: &[Step], retry: Option<&[Step]>, hook_order: Option<&str>) -> Self {
        let dir = tempfile::tempdir().expect("fake Droid directory");
        let executable = write_fake(dir.path(), steps, retry, hook_order);
        Self { dir, executable }
    }

    fn spec(&self, order_id: &str) -> OrderRunSpec<'_> {
        let mut env = CleanEnv::new();
        env.insert("ACAN_TEST_KEEP", "yes");
        OrderRunSpec {
            order_id: order_id.to_owned(),
            prompt: PROMPT.to_owned(),
            spawn_config: SpawnConfig::new(&self.executable)
                .with_working_dir(self.dir.path())
                .with_env(env),
            init: SessionInit {
                machine_id: "test-machine",
                cwd: self.dir.path(),
            },
            settings: SessionSettings {
                model_id: "fixture-model".to_owned(),
                reasoning_effort: "low".to_owned(),
                autonomy_level: "low".to_owned(),
            },
            startup_timeout: WAIT,
            hook_endpoint: None,
        }
    }

    fn started(&self) -> bool {
        self.dir.path().join("started").exists()
    }

    fn retried(&self) -> bool {
        self.dir.path().join("retried").exists()
    }
}

fn script_steps(script: &mut String, steps: &[Step]) {
    for step in steps {
        match step {
            Step::Expect(fragment) => writeln!(script, "expect {}", quoted(fragment)),
            Step::UserMessage(prompt) => {
                let fragment = format!(r#""method":"droid.add_user_message","params":{{"text":{}}}"#, json!(prompt));
                writeln!(script, "expect {}", quoted(&fragment))
            }
            Step::Emit(frame) => writeln!(script, "emit {}", quoted(&frame.to_string())),
            Step::WaitForRelease => {
                #[cfg(unix)]
                let line = "while [ ! -f release ]; do sleep 0.02; done";
                #[cfg(windows)]
                let line = "while (-not (Test-Path -LiteralPath 'release')) { Start-Sleep -Milliseconds 20 }";
                writeln!(script, "{line}")
            }
        }
        .expect("write fake exchange");
    }
}

#[cfg(unix)]
fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[cfg(unix)]
fn write_fake(
    dir: &Path,
    steps: &[Step],
    retry: Option<&[Step]>,
    hook_order: Option<&str>,
) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let mut script = String::from(
        r#"#!/bin/sh
if [ "$1" = --version ]; then
    printf '%s\n' '1.15.0'
    exit 0
fi
[ "$*" = 'exec --input-format stream-jsonrpc --output-format stream-jsonrpc' ] || exit 90
[ "$FACTORY_DROID_AUTO_UPDATE_ENABLED" = false ] || exit 91
[ "$ACAN_TEST_KEEP" = yes ] || exit 92
expect() {
    IFS= read -r frame || exit 93
    case "$frame" in *"$1"*) ;; *) exit 94 ;; esac
}
emit() { printf '%s\n' "$1"; }
"#,
    );
    if let Some(id) = hook_order {
        writeln!(
            script,
            r#"[ "$ACAN_ORDER_ID" = {} ] || exit 95"#,
            quoted(id)
        )
        .unwrap();
        writeln!(
            script,
            r#"[ "$ACAN_HOOK_ENDPOINT" = {} ] || exit 96"#,
            quoted(ENDPOINT)
        )
        .unwrap();
    }
    if let Some(retry) = retry {
        script.push_str("if [ -f started ]; then\n: > retried\n");
        script_steps(&mut script, retry);
        script.push_str("exit 0\nfi\n");
    }
    script.push_str(": > started\n");
    script_steps(&mut script, steps);
    let executable = dir.join("fake droid");
    fs::write(&executable, script).expect("write fake Unix Droid");
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
        .expect("make the fake Droid executable");
    executable
}

#[cfg(windows)]
fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

#[cfg(windows)]
fn write_fake(
    dir: &Path,
    steps: &[Step],
    retry: Option<&[Step]>,
    hook_order: Option<&str>,
) -> PathBuf {
    let mut script = String::from(
        r#"$ErrorActionPreference = 'Stop'
if ($env:FACTORY_DROID_AUTO_UPDATE_ENABLED -cne 'false') { exit 91 }
if ($env:ACAN_TEST_KEEP -cne 'yes') { exit 92 }
function expect([string]$fragment) {
    $frame = [Console]::ReadLine()
    if ($null -eq $frame) { exit 93 }
    if (-not $frame.Contains($fragment)) { exit 94 }
}
function emit([string]$frame) { [Console]::WriteLine($frame) }
"#,
    );
    if let Some(id) = hook_order {
        writeln!(
            script,
            "if ($env:ACAN_ORDER_ID -cne {}) {{ exit 95 }}",
            quoted(id)
        )
        .unwrap();
        writeln!(
            script,
            "if ($env:ACAN_HOOK_ENDPOINT -cne {}) {{ exit 96 }}",
            quoted(ENDPOINT)
        )
        .unwrap();
    }
    if let Some(retry) = retry {
        script.push_str(
            "if (Test-Path -LiteralPath 'started') {\n[IO.File]::WriteAllText('retried', '')\n",
        );
        script_steps(&mut script, retry);
        script.push_str("exit 0\n}\n");
    }
    script.push_str("[IO.File]::WriteAllText('started', '')\n");
    script_steps(&mut script, steps);
    fs::write(dir.join("fake-droid.ps1"), script).expect("write the fake session script");

    let executable = dir.join("fake droid.cmd");
    fs::write(
        &executable,
        r#"@echo off
if "%~1"=="--version" (
    echo 1.15.0
    exit /b 0
)
if not "%*"=="exec --input-format stream-jsonrpc --output-format stream-jsonrpc" exit /b 90
powershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "%~dp0fake-droid.ps1"
exit /b %errorlevel%
"#
        .replace('\n', "\r\n"),
    )
    .expect("write fake Windows shim");
    executable
}

async fn wait_for_state(
    runner: &OrderRunnerManager,
    id: &str,
    ready: impl Fn(&OrderRunState) -> bool,
) -> OrderRunState {
    tokio::time::timeout(WAIT, async {
        loop {
            let state = runner.state(id).expect("the order is registered");
            if ready(&state) {
                return state;
            }
            if let OrderRunState::Failed { reason } = state {
                panic!("{id} failed before reaching its expected state: {reason}");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "{id} did not reach its expected state: {:?}",
            runner.state(id)
        )
    })
}

async fn wait_for_card(runner: &OrderRunnerManager, id: &str, kind: CardKind) -> Card {
    wait_for_state(runner, id, |state| match kind {
        CardKind::RateLimit => matches!(state, OrderRunState::PausedLimit { .. }),
        _ => matches!(state, OrderRunState::WaitingCard { .. }),
    })
    .await;
    let card = runner
        .pending_card(id)
        .expect("the waiting order has a Card");
    assert_eq!(card.kind, kind);
    assert_eq!(card.order_id, id);
    if matches!(kind, CardKind::Permission | CardKind::Question) {
        assert_eq!(
            runner.state(id),
            Some(OrderRunState::WaitingCard {
                card_id: card.id.clone()
            })
        );
    }
    card
}

async fn completed(runner: &OrderRunnerManager, id: &str) {
    wait_for_state(runner, id, |state| *state == OrderRunState::Completed).await;
    assert!(runner.pending_card(id).is_none());
}

#[test]
fn only_an_authenticated_member_can_get_approval_authority() {
    let dir = tempfile::tempdir().expect("token directory");
    let owner = "owner".parse().expect("member id");
    let agent = "worker".parse().expect("agent id");
    let (mut tokens, owner_token) = Tokens::load_or_create(dir.path(), &owner).expect("tokens");
    let agent_token = tokens.agent_token(dir.path(), &agent).expect("agent token");
    let principal = tokens
        .principal(agent_token.as_str())
        .expect("authenticated agent");
    assert_eq!(principal, Principal::Agent(agent));
    assert_eq!(
        DeveloperAuthority::from_principal(&principal).unwrap_err(),
        OrderRunnerError::AgentApprovalForbidden
    );
    let owner = tokens
        .principal(owner_token.expect("owner token").as_str())
        .expect("authenticated member");
    assert!(DeveloperAuthority::from_principal(&owner).is_ok());
}

#[tokio::test]
async fn permission_and_batched_questions_round_trip_through_the_child() {
    let fake = FakeDroid::new(
        &turn(
            PROMPT,
            [
                permission(),
                questions(json!([
                    {"index":4,"topic":"Preview","question":"Which color?","options":["Blue","Red"],
                     "optionDescriptions":["Ocean","Apple"]},
                    {"index":9,"question":"Any preferences?","options":null}
                ])),
                Step::Expect(r#""id":"permission-1","result":{"selectedOption":"proceed_once"}"#),
                Step::Expect(
                    r#""id":101,"result":{"answers":[{"index":4,"question":"Which color?","answer":"Blue"},{"index":9,"question":"Any preferences?","answer":"Preserve the existing layout"}]}"#,
                ),
                complete(),
                CLOSE,
            ],
        ),
        None,
        Some("hooks"),
    );
    let runner = OrderRunnerManager::new();
    let authority = developer();
    let mut spec = fake.spec("hooks");
    spec.hook_endpoint = Some(ENDPOINT.to_owned());
    runner.start_order(spec).expect("start order");
    let permission = wait_for_card(&runner, "hooks", CardKind::Permission).await;
    assert_eq!(
        permission.request_id,
        Some(RequestId::Text("permission-1".into()))
    );
    assert_eq!(
        permission.summary,
        "Script; docs___preview; Execute: echo preview"
    );
    assert_eq!(permission.options[0].id, "proceed_once");
    assert_eq!(permission.options[0].label, "Allow once");
    assert!(!permission.allow_other);
    assert!(permission.evidence.is_empty());

    let evidence = parse_and_validate_proposal(
        GenUiPlace::CardEvidence,
        r#"{"summary":"Suggestion","blocks":[{"kind":"text","text":"One preview"}]}"#,
    )
    .expect("validated evidence");
    runner
        .attach_card_evidence(&permission.id, evidence)
        .expect("attach evidence");
    let pending = runner.pending_card("hooks").expect("pending permission");
    assert_eq!(
        pending.summary, permission.summary,
        "evidence cannot rewrite the decision"
    );
    assert_eq!(
        pending.evidence,
        [Block::Text {
            text: "One preview".into()
        }]
    );
    let report = parse_and_validate_proposal(
        GenUiPlace::Report,
        r#"{"title":"Report","blocks":[{"kind":"text","text":"Not Card evidence"}]}"#,
    )
    .expect("validated report");
    assert_eq!(
        runner.attach_card_evidence(&permission.id, report),
        Err(OrderRunnerError::InvalidEvidencePlace)
    );
    assert_eq!(
        runner.answer_card(&authority, "not-minted", select("proceed_once")),
        Err(OrderRunnerError::CardNotFound)
    );
    assert_eq!(
        runner.answer_card(&authority, &permission.id, select("allow")),
        Err(OrderRunnerError::InvalidOption)
    );
    assert_eq!(
        runner.answer_card(&authority, &permission.id, other("Allow it")),
        Err(OrderRunnerError::OtherNotAllowed)
    );
    runner
        .answer_card(&authority, &permission.id, select("proceed_once"))
        .expect("answer permission");
    assert_eq!(
        runner.answer_card(&authority, &permission.id, select("cancel")),
        Err(OrderRunnerError::CardAlreadyResolved)
    );

    let first = wait_for_card(&runner, "hooks", CardKind::Question).await;
    assert_eq!(first.request_id, Some(RequestId::Number(101)));
    assert_eq!(first.title, "Preview");
    assert_eq!(first.summary, "Which color?");
    assert_eq!(first.options[0].id, "Blue");
    assert_eq!(first.options[0].label, "Blue");
    assert_eq!(first.options[0].description.as_deref(), Some("Ocean"));
    runner
        .answer_card(&authority, &first.id, select("Blue"))
        .expect("answer first question");

    let second = wait_for_card(&runner, "hooks", CardKind::Question).await;
    assert_ne!(second.id, first.id, "server IDs are unique within a batch");
    assert_eq!(second.request_id, first.request_id);
    assert!(second.allow_other);
    assert!(second.options.is_empty());
    for note in [
        "line1\nline2",
        "line1\rline2",
        "a\tb",
        "a\u{0000}b",
        "a\u{202e}b",
        "a\u{2069}b",
        "a\u{2028}b",
        "a\u{2029}b",
        "",
        "   ",
    ] {
        assert_eq!(
            runner.answer_card(&authority, &second.id, other(note)),
            Err(OrderRunnerError::InvalidNote),
            "refuse {note:?}"
        );
    }
    assert_eq!(
        runner.answer_card(
            &authority,
            &second.id,
            other("é".repeat(MAX_ONE_LINE_CHARS + 1))
        ),
        Err(OrderRunnerError::InvalidNote)
    );
    runner
        .answer_card(
            &authority,
            &second.id,
            other("Preserve the existing layout"),
        )
        .expect("answer second question with one line");
    completed(&runner, "hooks").await;
}

#[tokio::test]
async fn cancelling_a_live_card_echoes_only_an_explicit_droid_decline() {
    let fake = FakeDroid::new(
        &turn(
            PROMPT,
            [
                permission(),
                Step::Expect(r#""id":"permission-1","result":{"selectedOption":"cancel"}"#),
                questions(json!([{"index":1,"question":"Any preferences?"}])),
                Step::Expect(r#""id":101,"result":{"cancelled":true,"answers":[]}"#),
                notification("droid.turn_completed", json!({"reason":"cancelled"})),
            ],
        ),
        None,
        None,
    );
    let runner = OrderRunnerManager::new();
    let authority = developer();
    runner
        .start_order(fake.spec("decline"))
        .expect("start order");
    for kind in [CardKind::Permission, CardKind::Question] {
        let card = wait_for_card(&runner, "decline", kind).await;
        runner
            .answer_card(&authority, &card.id, CardDecision::Cancel)
            .expect("decline request");
    }
    wait_for_state(&runner, "decline", |state| {
        *state == OrderRunState::Cancelled
    })
    .await;
}

#[tokio::test]
async fn rate_limit_releases_a_slot_and_waits_for_explicit_resume() {
    let live = FakeDroid::new(
        &turn(
            PROMPT,
            [
                permission(),
                Step::Expect(r#""selectedOption":"proceed_once""#),
                complete(),
                CLOSE,
            ],
        ),
        None,
        None,
    );
    let limited = FakeDroid::new(
        &turn(
            PROMPT,
            [notification(
                "droid.rate_limited",
                json!({"message":"No headroom","retryAfterMs":10,"headroom":{"remaining":0,"limit":5}}),
            )],
        ),
        Some(&turn(
            "Apply the order (Developer resume note: Keep the tests)",
            [complete(), CLOSE],
        )),
        None,
    );
    let runner = OrderRunnerManager::new();
    let authority = developer();
    runner.start_order(live.spec("one")).expect("first slot");
    runner
        .start_order(limited.spec("limited"))
        .expect("second slot");
    let card = wait_for_card(&runner, "limited", CardKind::RateLimit).await;
    assert_eq!(
        runner.state("limited"),
        Some(OrderRunState::PausedLimit {
            reason: "No headroom".to_owned(),
            retry_after_ms: Some(10),
        })
    );
    assert_eq!(card.summary, "No headroom (0 of 5 remaining)");
    assert_eq!(card.request_id, None);
    assert_eq!(
        card.options
            .iter()
            .map(|option| option.id.as_str())
            .collect::<Vec<_>>(),
        ["resume", "cancel"]
    );
    assert_eq!(
        runner.start_order(limited.spec("limited")),
        Err(OrderRunnerError::ResumeRequired)
    );
    assert_eq!(
        runner.resume_order(&authority, "limited", Some("two\rlines".into())),
        Err(OrderRunnerError::InvalidNote)
    );
    runner
        .start_order(live.spec("two"))
        .expect("the paused slot is available");
    assert_eq!(
        runner.answer_card(&authority, &card.id, select("resume")),
        Err(OrderRunnerError::ConcurrencyLimitReached { limit: 2 })
    );
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(runner.pending_card("limited"), Some(card.clone()));
    assert!(!limited.retried(), "retryAfterMs cannot restart a session");

    runner
        .cancel_order(&authority, "one")
        .await
        .expect("cancel and free slot");
    runner
        .resume_order(&authority, "limited", Some("Keep the tests".into()))
        .expect("resume explicitly");
    completed(&runner, "limited").await;
    assert!(limited.retried());
    assert_eq!(
        runner.answer_card(&authority, &card.id, CardDecision::Cancel),
        Err(OrderRunnerError::CardAlreadyResolved)
    );
    runner
        .cancel_order(&authority, "two")
        .await
        .expect("cancel remaining run");
}

#[tokio::test]
async fn rpc_rate_limits_pause_during_startup_and_during_a_turn() {
    let startup = FakeDroid::new(
        &[
            Step::Expect(HANDSHAKE[0]),
            frame(json!({"type":"response","id":1,"error":{
                "code":-32000,"message":"Quota exhausted","data":{
                    "code":"rate_limit_exceeded","retryAfterMs":2000,"headroom":{"remaining":0,"limit":20}
                }
            }})),
        ],
        None,
        None,
    );
    let live = FakeDroid::new(
        &turn(
            PROMPT,
            [frame(json!({"type":"response","id":3,"error":{
                "code":429,"message":"Too many requests","data":{"retryAfter":3}
            }}))],
        ),
        None,
        None,
    );
    let runner = OrderRunnerManager::new();
    let authority = developer();
    runner
        .start_order(startup.spec("startup"))
        .expect("start order");
    runner.start_order(live.spec("live")).expect("start order");
    let startup_card = wait_for_card(&runner, "startup", CardKind::RateLimit).await;
    assert_eq!(startup_card.summary, "Quota exhausted (0 of 20 remaining)");
    assert_eq!(
        runner.state("startup"),
        Some(OrderRunState::PausedLimit {
            reason: "Quota exhausted".into(),
            retry_after_ms: Some(2000),
        })
    );
    let live_card = wait_for_card(&runner, "live", CardKind::RateLimit).await;
    assert_eq!(live_card.summary, "Too many requests");
    assert_eq!(
        runner.state("live"),
        Some(OrderRunState::PausedLimit {
            reason: "Too many requests".into(),
            retry_after_ms: Some(3000),
        })
    );
    runner
        .answer_card(&authority, &startup_card.id, select("cancel"))
        .expect("cancel via Card");
    runner
        .cancel_order(&authority, "live")
        .await
        .expect("cancel paused order");
    for id in ["startup", "live"] {
        assert_eq!(runner.state(id), Some(OrderRunState::Cancelled));
        assert!(runner.pending_card(id).is_none());
    }
}

#[tokio::test]
async fn startup_restores_interrupted_cards_without_starting_a_process() {
    use OrderRunState::*;

    let fake = FakeDroid::new(
        &turn(
            "Apply the order (Developer resume note: Use the saved worktree)",
            [complete(), CLOSE],
        ),
        None,
        None,
    );
    let records: Vec<_> = [
        ("running", Running),
        (
            "waiting",
            WaitingCard {
                card_id: "old-card".into(),
            },
        ),
        (
            "limited",
            PausedLimit {
                reason: "quota".into(),
                retry_after_ms: None,
            },
        ),
        ("done", Completed),
        (
            "failed",
            Failed {
                reason: "failure".into(),
            },
        ),
        ("cancelled", Cancelled),
        (
            "interrupted",
            Interrupted {
                reason: "existing reason".into(),
            },
        ),
    ]
    .into_iter()
    .map(|(id, state)| PersistedOrderState {
        order_id: id.into(),
        state,
    })
    .collect();

    let expected = reconcile_on_startup(records.clone());
    let runner = OrderRunnerManager::new();
    let restored = runner
        .load_persisted_orders(records.clone(), [fake.spec("running")])
        .expect("load persisted orders");
    assert_eq!(restored.len(), records.len());
    for (index, (order, record)) in restored.iter().zip(&records).enumerate() {
        assert_eq!(order.order_id, record.order_id);
        assert_eq!(order.state, expected[index].state);
        assert_eq!(runner.state(&order.order_id), Some(order.state.clone()));
        if index < 3 {
            assert_eq!(
                order.state,
                Interrupted {
                    reason: "daemon restarted before order finished".into()
                }
            );
        } else {
            assert_eq!(order.state, record.state);
        }
        if let Interrupted { reason } = &order.state {
            let card = order.card.as_ref().expect("interrupted Card");
            assert_ne!(card.id, "old-card");
            assert_eq!(card.kind, CardKind::Interrupted);
            assert_eq!(card.summary, *reason);
            assert_eq!(card.request_id, None);
            assert_eq!(runner.pending_card(&order.order_id).as_ref(), Some(card));
        } else {
            assert!(order.card.is_none());
        }
    }
    let snapshots = runner.persisted_states();
    let encoded = serde_json::to_string(&snapshots).expect("encode durable state");
    assert_eq!(
        serde_json::from_str::<Vec<PersistedOrderState>>(&encoded).expect("decode durable state"),
        snapshots
    );
    let authority = developer();
    assert_eq!(
        runner.start_order(fake.spec("running")),
        Err(OrderRunnerError::ResumeRequired)
    );
    assert_eq!(
        runner.answer_card(&authority, "old-card", CardDecision::Cancel),
        Err(OrderRunnerError::CardNotFound)
    );
    assert_eq!(
        runner.resume_order(&authority, "waiting", None),
        Err(OrderRunnerError::ResumeSpecMissing)
    );
    assert!(
        !fake.started(),
        "startup and rejected actions must not spawn Droid"
    );

    let waiting = runner.pending_card("waiting").expect("waiting Card");
    runner
        .answer_card(&authority, &waiting.id, CardDecision::Cancel)
        .expect("cancel interrupted order");
    assert_eq!(runner.state("waiting"), Some(Cancelled));
    let running = runner.pending_card("running").expect("interrupted Card");
    assert_eq!(
        runner.answer_card(&authority, &running.id, other("line1\nline2")),
        Err(OrderRunnerError::InvalidNote)
    );
    runner
        .answer_card(&authority, &running.id, other("Use the saved worktree"))
        .expect("resume after startup");
    completed(&runner, "running").await;
    assert!(fake.started());
}

#[tokio::test]
async fn two_active_orders_refuse_a_third_until_a_slot_is_released() {
    assert_eq!(MAX_CONCURRENT_ORDERS, 2);
    let fake = FakeDroid::new(
        &turn(
            PROMPT,
            [
                permission(),
                Step::Expect(r#""selectedOption":"proceed_once""#),
                complete(),
                CLOSE,
            ],
        ),
        None,
        None,
    );
    let runner = OrderRunnerManager::new();
    let shared = runner.clone();
    let authority = developer();
    let mut invalid = fake.spec("invalid");
    invalid.prompt = "one\ntwo".into();
    assert_eq!(
        runner.start_order(invalid),
        Err(OrderRunnerError::InvalidPrompt)
    );
    runner.start_order(fake.spec("one")).expect("first slot");
    shared.start_order(fake.spec("two")).expect("second slot");
    assert_eq!(runner.state("one"), Some(OrderRunState::Running));
    assert_eq!(
        shared.start_order(fake.spec("one")),
        Err(OrderRunnerError::OrderAlreadyActive)
    );
    assert_eq!(
        runner.start_order(fake.spec("three")),
        Err(OrderRunnerError::ConcurrencyLimitReached { limit: 2 })
    );
    assert_eq!(runner.state("three"), None, "no queued order");

    let first = wait_for_card(&runner, "one", CardKind::Permission).await;
    let second = wait_for_card(&runner, "two", CardKind::Permission).await;
    assert_eq!(
        runner.start_order(fake.spec("three")),
        Err(OrderRunnerError::ConcurrencyLimitReached { limit: 2 })
    );
    runner
        .cancel_order(&authority, "one")
        .await
        .expect("cancel and reap");
    assert_eq!(runner.state("one"), Some(OrderRunState::Cancelled));
    assert_eq!(
        runner.answer_card(&authority, &first.id, select("proceed_once")),
        Err(OrderRunnerError::CardAlreadyResolved)
    );
    runner
        .start_order(fake.spec("three"))
        .expect("reuse cancelled slot");
    runner
        .answer_card(&authority, &second.id, select("proceed_once"))
        .expect("complete second order");
    completed(&runner, "two").await;
    runner
        .start_order(fake.spec("four"))
        .expect("reuse completed slot");
    runner
        .cancel_order(&authority, "three")
        .await
        .expect("cancel third");
    runner
        .cancel_order(&authority, "four")
        .await
        .expect("cancel fourth");
}

#[tokio::test]
async fn an_exit_while_waiting_closes_the_card_and_frees_the_slot() {
    let fake = FakeDroid::new(
        &turn(PROMPT, [permission(), Step::WaitForRelease]),
        None,
        None,
    );
    let runner = OrderRunnerManager::new();
    let authority = developer();
    runner.start_order(fake.spec("exit")).expect("start order");
    let card = wait_for_card(&runner, "exit", CardKind::Permission).await;
    fs::write(fake.dir.path().join("release"), "").expect("allow clean mid-turn exit");
    let state = wait_for_state(&runner, "exit", |state| {
        matches!(state, OrderRunState::Failed { .. })
    })
    .await;
    assert!(matches!(state, OrderRunState::Failed { reason } if reason.contains("exited before")));
    assert!(runner.pending_card("exit").is_none());
    assert_eq!(
        runner.answer_card(&authority, &card.id, select("proceed_once")),
        Err(OrderRunnerError::CardAlreadyResolved)
    );
    runner
        .start_order(fake.spec("next"))
        .expect("slot after failure");
    runner
        .cancel_order(&authority, "next")
        .await
        .expect("cancel next order");
}
