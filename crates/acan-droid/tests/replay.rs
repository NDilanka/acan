use std::path::{Path, PathBuf};
use std::time::Duration;

use acan_droid::{
    AskAnswer, AskUserReply, CleanEnv, DroidError, DroidEvent, DroidSession, Headroom,
    PermissionReply, RequestId, SessionInit, SessionSettings, SpawnConfig,
};
use serde_json::{Value, json};
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, DuplexStream, Lines,
    ReadHalf, WriteHalf, duplex, split,
};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(5);
type Replay = DroidSession<ReadHalf<DuplexStream>, WriteHalf<DuplexStream>>;

struct Peer {
    rx: Lines<BufReader<ReadHalf<DuplexStream>>>,
    tx: WriteHalf<DuplexStream>,
}

fn replay() -> (Replay, Peer) {
    let (session, peer) = duplex(4096);
    let (rx, tx) = split(session);
    let (peer_rx, peer_tx) = split(peer);
    (
        DroidSession::from_streams(rx, tx),
        Peer {
            rx: BufReader::new(peer_rx).lines(),
            tx: peer_tx,
        },
    )
}

impl Peer {
    async fn read_client_frame(&mut self) -> Option<Value> {
        timeout(WAIT, self.rx.next_line())
            .await
            .expect("client should send a frame or close the stream")
            .expect("read client frame")
            .map(|line| serde_json::from_str(&line).expect("valid client JSON"))
    }

    async fn client_request(&mut self, method: &str) -> Value {
        let frame = self.read_client_frame().await.expect("client request");
        assert_eq!(frame["jsonrpc"], "2.0");
        assert_eq!(frame["factoryApiVersion"], "1.0.0");
        assert_eq!(frame["type"], "request");
        assert_eq!(frame["method"], method);
        frame
    }

    async fn write(&mut self, mut frame: Value) {
        frame["jsonrpc"] = json!("2.0");
        frame["factoryApiVersion"] = json!("1.0.0");
        frame["factoryProtocolVersion"] = json!("1.357.0");
        self.tx
            .write_all(format!("{frame}\n").as_bytes())
            .await
            .expect("send scripted server frame");
    }

    async fn response(&mut self, id: &Value, result: Value) {
        self.write(json!({"type":"response", "id":id, "result":result}))
            .await;
    }

    async fn notify(&mut self, notification: Value) {
        self.write(json!({
            "type":"notification",
            "method":"droid.session_notification",
            "params":{"sessionId":"session-fixture","notification":notification}
        }))
        .await;
    }

    async fn start(&mut self) -> Value {
        let init = self.client_request("droid.initialize_session").await;
        assert_eq!(
            init["params"],
            json!({"machineId":"test-host", "cwd":"fixture-worktree"})
        );
        // A settings notification for the initial defaults must not be mistaken
        // for confirmation of update_session_settings.
        self.notify(json!({
            "type":"settings_updated",
            "settings":{"modelId":"default-model","autonomyLevel":"high"}
        }))
        .await;
        self.response(&init["id"], json!({"sessionId":"session-fixture"}))
            .await;
        let update = self.client_request("droid.update_session_settings").await;
        assert_eq!(
            update["params"],
            json!({"modelId":"fixture-model","reasoningEffort":"low","autonomyLevel":"low"})
        );
        update["id"].clone()
    }

    async fn confirm(&mut self) {
        let id = self.start().await;
        self.notify(json!({
            "type":"settings_updated",
            "settings":{"modelId":"default-model","autonomyLevel":"high"}
        }))
        .await;
        self.notify(json!({
            "type":"settings_updated",
            "requestId":id,
            "settings":{"modelId":"fixture-model","autonomyLevel":"low","newField":"ignored"}
        }))
        .await;
        self.response(&id, json!({})).await;
    }
}

fn settings() -> SessionSettings {
    SessionSettings {
        model_id: "fixture-model".into(),
        reasoning_effort: "low".into(),
        autonomy_level: "low".into(),
    }
}

fn init() -> SessionInit<'static> {
    SessionInit {
        machine_id: "test-host",
        cwd: Path::new("fixture-worktree"),
    }
}

async fn event<R, W>(session: &mut DroidSession<R, W>) -> DroidEvent
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    timeout(WAIT, async {
        loop {
            let event = session
                .next_event()
                .await
                .expect("read event")
                .expect("event before stream closes");
            if !matches!(event, DroidEvent::SettingsUpdated { .. }) {
                break event;
            }
        }
    })
    .await
    .expect("receive replay event")
}

#[tokio::test]
async fn handshake_then_message_and_turn_completed() {
    let (mut session, mut peer) = replay();
    assert!(matches!(
        session.send_user_message("too early").await,
        Err(DroidError::NotConfirmed)
    ));
    let server = tokio::spawn(async move {
        peer.confirm().await;
        let message = peer.client_request("droid.add_user_message").await;
        assert_eq!(message["params"], json!({"text":"work from the order"}));
        peer.response(&message["id"], Value::Null).await;
        peer.notify(json!({"type":"agent_turn_completed","turnId":"turn-1","reason":"completed"}))
            .await;
        let close = peer.client_request("droid.close_session").await;
        peer.response(&close["id"], json!({})).await;
    });
    session
        .initialize(init(), &settings(), WAIT)
        .await
        .expect("confirm requested settings");
    assert_eq!(session.session_id(), Some("session-fixture"));
    session
        .send_user_message("work from the order")
        .await
        .expect("send confirmed message");
    assert!(!session.finished(), "sending does not finish the turn");
    assert!(matches!(
        session.send_user_message("overlapping turn").await,
        Err(DroidError::TurnInProgress)
    ));
    assert_eq!(
        event(&mut session).await,
        DroidEvent::TurnCompleted {
            turn_id: Some("turn-1".into()),
            reason: Some("completed".into())
        }
    );
    assert!(session.finished());
    session.shutdown(WAIT).await.expect("graceful shutdown");
    assert_eq!(
        event(&mut session).await,
        DroidEvent::Exited { status: None }
    );
    assert!(session.next_event().await.expect("end of stream").is_none());
    server.await.expect("server replay passed");
}

#[tokio::test]
async fn update_response_can_confirm_both_settings() {
    let (mut session, mut peer) = replay();
    let server = tokio::spawn(async move {
        let id = peer.start().await;
        peer.response(
            &id,
            json!({"settings":{"modelId":"fixture-model","autonomyLevel":"low"}}),
        )
        .await;
    });
    session
        .initialize(init(), &settings(), WAIT)
        .await
        .expect("update result confirmed the settings");
    server.await.expect("server replay passed");
}

#[tokio::test]
async fn matching_broadcast_after_ack_can_confirm_settings() {
    let (mut session, mut peer) = replay();
    let server = tokio::spawn(async move {
        let id = peer.start().await;
        peer.response(&id, json!({})).await;
        peer.notify(json!({
            "type":"settings_updated",
            "settings":{"modelId":"fixture-model","autonomyLevel":"low"}
        }))
        .await;
    });
    session
        .initialize(init(), &settings(), WAIT)
        .await
        .expect("untagged settings snapshot confirms the acknowledged update");
    server.await.expect("server replay passed");
}

#[tokio::test]
async fn mismatched_model_or_autonomy_aborts_before_sending() {
    for (model, level, setting) in [
        ("different-model", "low", "model"),
        ("fixture-model", "high", "autonomy"),
    ] {
        let (mut session, mut peer) = replay();
        let server = tokio::spawn(async move {
            let id = peer.start().await;
            peer.notify(json!({
                "type":"settings_updated","requestId":id,
                "settings":{"modelId":model,"autonomyLevel":level}
            }))
            .await;
            assert!(
                peer.read_client_frame().await.is_none(),
                "client sent another frame"
            );
        });
        assert!(matches!(
            session.initialize(init(), &settings(), WAIT).await,
            Err(DroidError::SettingsMismatch(observed)) if observed == setting
        ));
        assert!(matches!(
            session.send_user_message("never sent").await,
            Err(DroidError::NotConfirmed)
        ));
        assert!(matches!(
            session.initialize(init(), &settings(), WAIT).await,
            Err(DroidError::AlreadyInitialized)
        ));
        drop(session);
        server.await.expect("server replay passed");
    }
}

#[tokio::test]
async fn an_ack_or_settings_for_another_request_is_not_confirmation() {
    let (mut session, mut peer) = replay();
    let (ack_written, ack_seen) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let id = peer.start().await;
        peer.notify(json!({
            "type":"settings_updated", "requestId":1,
            "settings":{"modelId":"fixture-model","autonomyLevel":"low"}
        }))
        .await;
        peer.response(&id, json!({"settings":{"modelId":"fixture-model"}}))
            .await;
        ack_written.send(()).expect("handshake should still wait");
        assert!(
            peer.read_client_frame().await.is_none(),
            "client sent another frame"
        );
    });
    // Poll until the server has written everything; the stream stays open without confirmation.
    let desired = settings();
    let mut handshake = Box::pin(session.initialize(init(), &desired, Duration::from_millis(250)));
    tokio::select! {
        result = &mut handshake => panic!("returned before receiving the update reply: {result:?}"),
        ready = ack_seen => ready.expect("server wrote the acknowledgement"),
    }
    assert!(matches!(
        handshake.await,
        Err(DroidError::Timeout("settings confirmation"))
    ));
    assert!(matches!(
        session.send_user_message("never sent").await,
        Err(DroidError::NotConfirmed)
    ));
    drop(session);
    server.await.expect("server replay passed");
}

#[tokio::test]
async fn script_permission_extracts_mcp_and_exec_and_echoes_choice() {
    let (mut session, mut peer) = replay();
    let server = tokio::spawn(async move {
        peer.confirm().await;
        peer.write(json!({
            "type":"request","id":"permission-1","method":"droid.request_permission","params":{
                "toolUses":[{
                    "toolUse":{"type":"tool_call","id":"tool-1","name":"Script","input":{}},
                    "confirmationType":"script",
                    "details":{"type":"script","impactLevel":"high","calls":[
                        {"toolName":"example___preview","confirmation":{"type":"mcp_tool","serverName":"example"}},
                        {"toolName":"Execute","toolInput":{"command":"echo preview"},"confirmation":{"type":"exec"}}
                    ]}
                }],
                "options":[{"label":"Allow once","value":"proceed_once","newField":false},
                           {"label":"Cancel","value":"cancel"}]
            }
        })).await;
        assert_eq!(
            peer.read_client_frame().await.expect("permission response"),
            json!({"jsonrpc":"2.0","factoryApiVersion":"1.0.0","type":"response",
                   "id":"permission-1","result":{"selectedOption":"proceed_once"}})
        );
    });
    session
        .initialize(init(), &settings(), WAIT)
        .await
        .expect("initialize");
    let DroidEvent::PermissionRequested { id, request } = event(&mut session).await else {
        panic!("expected a Script permission request");
    };
    assert_eq!(id, RequestId::Text("permission-1".into()));
    assert_eq!(
        request.tool_uses[0].confirmation_type.as_deref(),
        Some("script")
    );
    assert_eq!(
        request
            .script_calls()
            .map(|call| (call.tool_name.as_str(), call.confirmation.kind.as_str()))
            .collect::<Vec<_>>(),
        [("example___preview", "mcp_tool"), ("Execute", "exec")]
    );
    session
        .reply_permission(
            &id,
            &PermissionReply {
                selected_option: request.options[0].value.clone(),
            },
        )
        .await
        .expect("reply to Script permission");
    server.await.expect("server replay passed");
}

#[tokio::test]
async fn answers_one_or_many_questions_and_can_decline() {
    let (mut session, mut peer) = replay();
    let server = tokio::spawn(async move {
        peer.confirm().await;
        for (id, count) in [(101, 1), (102, 2), (103, 2)] {
            let questions: Vec<_> = (1..=count)
                .map(|index| {
                    json!({
                        "index":index,"topic":"Preview","question":format!("Choose {index}"),
                        "options":["Blue","Red"],"optionDescriptions":["Ocean","Apple"]
                    })
                })
                .collect();
            peer.write(json!({"type":"request","id":id,"method":"droid.ask_user",
                              "params":{"toolCallId":"ask-1","questions":questions}}))
                .await;
            let frame = peer.read_client_frame().await.expect("ask-user reply");
            assert_eq!(frame["id"], id);
            let expected = if id == 103 {
                json!({"cancelled":true,"answers":[]})
            } else {
                json!({"answers":(1..=count).map(|index| json!({
                    "index":index,"question":format!("Choose {index}"),"answer":"Blue"
                })).collect::<Vec<_>>()})
            };
            assert_eq!(frame["result"], expected);
        }
    });
    session
        .initialize(init(), &settings(), WAIT)
        .await
        .expect("initialize");
    for (count, decline) in [(1, false), (2, false), (2, true)] {
        let DroidEvent::AskUserRequested { id, request } = event(&mut session).await else {
            panic!("expected questions from Droid");
        };
        assert_eq!(request.tool_call_id, "ask-1");
        assert_eq!(request.questions.len(), count);
        assert_eq!(request.questions[0].options, ["Blue", "Red"]);
        let reply = if decline {
            AskUserReply::Declined
        } else {
            AskUserReply::Answers(
                request
                    .questions
                    .into_iter()
                    .map(|item| AskAnswer {
                        index: item.index,
                        question: item.question,
                        answer: "Blue".into(),
                    })
                    .collect(),
            )
        };
        session
            .reply_ask_user(&id, &reply)
            .await
            .expect("reply to questions");
    }
    server.await.expect("server replay passed");
}

#[tokio::test]
async fn unknown_notification_and_rate_limit_without_or_with_headroom() {
    let (mut session, mut peer) = replay();
    let server = tokio::spawn(async move {
        peer.confirm().await;
        peer.notify(json!({"type":"future_activity","payload":{"extra":[1,2]}}))
            .await;
        peer.notify(json!({"type":"agent_error","error":{"statusCode":429,"message":"Slow down"}}))
            .await;
        peer.write(json!({"type":"response","id":96,"error":{
            "code":429,"message":"Slow down",
            "data":{"retryAfterMs":1200,"headroom":{"remaining":0,"limit":50}}
        }}))
        .await;
    });
    session
        .initialize(init(), &settings(), WAIT)
        .await
        .expect("initialize");
    assert_eq!(
        event(&mut session).await,
        DroidEvent::Unknown {
            method: "future_activity".into(),
            raw: json!({"type":"future_activity","payload":{"extra":[1,2]}})
        }
    );
    assert_eq!(
        event(&mut session).await,
        DroidEvent::RateLimited {
            message: "Slow down".into(),
            retry_after: None,
            headroom: Headroom::Unknown
        }
    );
    assert_eq!(
        event(&mut session).await,
        DroidEvent::RateLimited {
            message: "Slow down".into(),
            retry_after: Some(Duration::from_millis(1200)),
            headroom: Headroom::Known {
                remaining: 0,
                limit: 50
            }
        }
    );
    server.await.expect("server replay passed");
}

#[tokio::test]
async fn interrupt_and_cancelled_completion_do_not_finish_a_run() {
    let (mut session, mut peer) = replay();
    let server = tokio::spawn(async move {
        peer.confirm().await;
        let message = peer.client_request("droid.add_user_message").await;
        peer.response(&message["id"], json!({})).await;
        let interrupt = peer.client_request("droid.interrupt_session").await;
        peer.response(&interrupt["id"], json!({})).await;
        peer.notify(json!({"type":"agent_turn_completed","reason":"cancelled"}))
            .await;
    });
    session
        .initialize(init(), &settings(), WAIT)
        .await
        .expect("initialize");
    session
        .send_user_message("work")
        .await
        .expect("send message");
    session.interrupt().await.expect("interrupt turn");
    assert_eq!(
        event(&mut session).await,
        DroidEvent::TurnCompleted {
            turn_id: None,
            reason: Some("cancelled".into())
        }
    );
    assert!(!session.finished());
    server.await.expect("server replay passed");
}

#[cfg(windows)]
fn fake_droid(dir: &Path, version: &str) -> PathBuf {
    let file = dir.join("fake droid.cmd");
    std::fs::write(
        &file,
        format!(
            r#"@echo off
if not "%FACTORY_DROID_AUTO_UPDATE_ENABLED%"=="false" exit /b 91
if not "%ACAN_TEST_FAKE%"=="yes" exit /b 92
if "%~1"=="--version" (
    echo {version}
    exit /b 0
)
if not "%*"=="exec --input-format stream-jsonrpc --output-format stream-jsonrpc" exit /b 93
set /p unused=
echo {{"jsonrpc":"2.0","factoryApiVersion":"1.0.0","type":"response","id":1,"result":{{}}}}
set /p unused=
echo {{"jsonrpc":"2.0","factoryApiVersion":"1.0.0","type":"response","id":2,"result":{{"modelId":"fixture-model","autonomyLevel":"low"}}}}
set /p unused=
exit /b 0
"#
        )
        .replace('\n', "\r\n"),
    )
    .expect("write fake Windows shim");
    file
}

#[cfg(unix)]
fn fake_droid(dir: &Path, version: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let file = dir.join("fake droid");
    std::fs::write(
        &file,
        format!(
            r#"#!/bin/sh
[ "$FACTORY_DROID_AUTO_UPDATE_ENABLED" = false ] || exit 91
[ "$ACAN_TEST_FAKE" = yes ] || exit 92
if [ "$1" = --version ]; then
    printf '%s\n' '{version}'
    exit 0
fi
[ "$*" = 'exec --input-format stream-jsonrpc --output-format stream-jsonrpc' ] || exit 93
read -r unused || exit 94
printf '%s\n' '{{"jsonrpc":"2.0","factoryApiVersion":"1.0.0","type":"response","id":1,"result":{{}}}}'
read -r unused || exit 95
printf '%s\n' '{{"jsonrpc":"2.0","factoryApiVersion":"1.0.0","type":"response","id":2,"result":{{"modelId":"fixture-model","autonomyLevel":"low"}}}}'
read -r unused || exit 96
exit 0
"#
        ),
    )
    .expect("write fake Unix Droid");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o700))
        .expect("make the fake Droid executable");
    file
}

fn fake_config(path: &Path) -> SpawnConfig {
    let mut env = CleanEnv::new();
    env.insert("ACAN_TEST_FAKE", "yes");
    SpawnConfig::new(path).with_env(env)
}

#[tokio::test]
async fn successful_child_exit_mid_turn_is_unfinished() {
    let dir = tempfile::tempdir().expect("make fake Droid directory");
    let fake = fake_droid(dir.path(), "1.15.0");
    let mut session = DroidSession::spawn(&fake_config(&fake))
        .await
        .expect("start the fake executable");
    assert_eq!(
        session.version().expect("record version").to_string(),
        "1.15.0"
    );
    session
        .initialize(init(), &settings(), WAIT)
        .await
        .expect("initialize fake child");
    session
        .send_user_message("work")
        .await
        .expect("send message to fake child");
    let DroidEvent::Exited {
        status: Some(status),
    } = event(&mut session).await
    else {
        panic!("expected a real child exit status");
    };
    assert!(status.success(), "scripted child exited with {status}");
    assert!(!session.finished(), "exit zero is not turn completion");
    assert!(session.next_event().await.expect("end of stream").is_none());
}

#[tokio::test]
async fn spawn_rejects_an_untested_version_before_exec() {
    let dir = tempfile::tempdir().expect("make fake Droid directory");
    let fake = fake_droid(dir.path(), "2.0.0");
    #[cfg(windows)]
    let fake = fake.with_extension("");
    let error = DroidSession::spawn(&fake_config(&fake))
        .await
        .expect_err("untested version should not be spawned");
    assert!(matches!(error, DroidError::UnsupportedVersion { .. }));
    assert!(error.to_string().contains("2.0.0"));
}

#[test]
fn spawn_config_rejects_unsafe_arguments() {
    for arg in [
        "--skip-permissions-unsafe",
        "--SKIP-PERMISSIONS-UNSAFE=true",
    ] {
        assert!(matches!(
            SpawnConfig::new("never-executed").with_args([arg]),
            Err(DroidError::ForbiddenArgument("--skip-permissions-unsafe"))
        ));
    }
}
