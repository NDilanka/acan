use std::collections::BTreeMap;

use acan_llm::{ChatRequest, LlmError, Message, ModelClient, Reply, Role, Stub, Usage};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
struct Verdict {
    clash: bool,
    files: Vec<String>,
}

fn clash_question(text: &str) -> ChatRequest {
    ChatRequest {
        messages: vec![Message::user(text)],
        ..Default::default()
    }
}

#[tokio::test]
async fn scripted_foreman_reply_round_trips_through_chat_json() {
    let scripted = Verdict {
        clash: true,
        files: vec!["src/lib.rs".to_owned()],
    };
    let stub = Stub::default().when(
        Role::Foreman,
        |request| {
            request
                .last_user_text()
                .is_some_and(|text| text.contains("clash"))
        },
        Reply::json(&scripted).with_usage(Usage {
            prompt_tokens: 40,
            completion_tokens: 12,
        }),
    );

    let out = stub
        .chat_json::<Verdict>(Role::Foreman, clash_question("Is there a clash?"))
        .await
        .expect("scripted reply parses");

    assert_eq!(out.value, scripted);
    assert_eq!(out.call.role, Role::Foreman);
    assert_eq!(out.call.provider, "stub");
    assert_eq!(out.call.usage.prompt_tokens, 40);
    assert_eq!(out.call.usage.completion_tokens, 12);
}

#[test]
fn reply_json_returns_an_error_for_values_json_cannot_represent() {
    // JSON object keys must be strings, so a map keyed by tuples cannot serialize.
    let tuple_keyed = BTreeMap::from([((1_u8, 2_u8), "x")]);

    assert!(Reply::try_json(&tuple_keyed).is_err());
}

#[tokio::test]
async fn unscripted_role_fails_with_no_scripted_reply() {
    let stub = Stub::default().when(Role::Foreman, |_| true, Reply::text("plan"));

    let err = stub
        .chat(Role::Fast, clash_question("triage"))
        .await
        .expect_err("fast has no script");

    assert!(
        matches!(err, LlmError::NoScriptedReply { role: Role::Fast }),
        "got {err:?}"
    );
}

#[tokio::test]
async fn stub_schema_mismatch_is_a_validation_error() {
    let stub = Stub::default().when(
        Role::Foreman,
        |_| true,
        Reply::text(r#"{"clash":"yes","files":[]}"#),
    );

    let err = stub
        .chat_json::<Verdict>(Role::Foreman, clash_question("Is there a clash?"))
        .await
        .expect_err("string where bool belongs must fail");

    assert!(
        matches!(err, LlmError::SchemaValidation { .. }),
        "got {err:?}"
    );
}

#[tokio::test]
async fn snapshot_totals_group_calls_by_role_and_model() {
    let usage = Usage {
        prompt_tokens: 20,
        completion_tokens: 5,
    };
    let stub = Stub::default()
        .when(
            Role::Foreman,
            |_| true,
            Reply::text("plan").with_usage(usage),
        )
        .when(
            Role::Worker,
            |_| true,
            Reply::text("edit").with_usage(usage),
        );

    stub.chat(Role::Foreman, clash_question("one"))
        .await
        .expect("call");
    stub.chat(Role::Foreman, clash_question("two"))
        .await
        .expect("call");
    stub.chat(Role::Worker, clash_question("three"))
        .await
        .expect("call");

    let totals = stub.ledger().snapshot();
    assert_eq!(totals.len(), 2);
    assert_eq!(totals[0].role, Role::Foreman);
    assert_eq!(totals[0].provider, "stub");
    assert_eq!(totals[0].calls, 2);
    assert_eq!(totals[0].prompt_tokens, 40);
    assert_eq!(totals[0].completion_tokens, 10);
    assert_eq!(totals[1].role, Role::Worker);
    assert_eq!(totals[1].calls, 1);
}

#[tokio::test]
async fn a_stub_reply_has_no_trace_fields() {
    let stub = Stub::default().when(Role::Foreman, |_| true, Reply::text("ok"));
    let traced = stub
        .chat_traced(Role::Foreman, clash_question("ping"))
        .await
        .expect("scripted reply");
    assert_eq!(traced.value.content.as_deref(), Some("ok"));
    assert_eq!(traced.request_id, None);
    assert_eq!(traced.reply_model, None);
}
