use std::time::Duration;

use acan_llm::{
    ApiKey, ApiKeys, ChatRequest, ContentPart, FunctionCall, JsonSchemaFormat, LlmConfig, LlmError,
    Message, ModelClient, OpenAiCompatible, RequestId, RequestIdSource, RetryPolicy, Role,
    ToolCall, ToolKind, ToolSpec,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const META_KEY: &str = "meta-test-key";
const BASETEN_KEY: &str = "baseten-test-key";

#[derive(Debug, PartialEq, Deserialize, JsonSchema)]
struct Verdict {
    clash: bool,
    files: Vec<String>,
}

fn text_reply(content: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": {"role": "assistant", "content": content}
        }],
        "usage": {"prompt_tokens": 120, "completion_tokens": 30, "total_tokens": 150}
    }))
}

fn client(meta: &MockServer, baseten: &MockServer, worker_provider: &str) -> OpenAiCompatible {
    client_for(test_config(meta, baseten, worker_provider))
}

fn test_config(meta: &MockServer, baseten: &MockServer, worker_provider: &str) -> LlmConfig {
    let meta_url = meta.uri();
    let baseten_url = baseten.uri();
    let toml = format!(
        r#"
[[providers]]
name = "meta"
base_url = "{meta_url}/v1"
api_key_env = "META_API_KEY"
default_model = "muse-spark-1.3"
structured_output = "json_schema"
timeout_secs = 2

[[providers]]
name = "baseten"
base_url = "{baseten_url}/v1"
api_key_env = "BASETEN_API_KEY"
default_model = "meta-models/muse-spark-open-1026"
structured_output = "prompt"
timeout_secs = 2

[routing.foreman]
provider = "meta"

[routing.worker]
provider = "{worker_provider}"

[routing.fast]
provider = "baseten"
"#
    );
    LlmConfig::from_toml_str(&toml).expect("test config is valid")
}

fn client_for(config: LlmConfig) -> OpenAiCompatible {
    let mut keys = ApiKeys::default();
    keys.insert("meta", ApiKey::new(META_KEY));
    keys.insert("baseten", ApiKey::new(BASETEN_KEY));
    OpenAiCompatible::new(config, keys)
        .expect("client builds")
        .with_retry_policy(RetryPolicy {
            max_retries: 3,
            base_delay: Duration::from_millis(1),
        })
}

async fn body_of(server: &MockServer, index: usize) -> Value {
    let requests = server.received_requests().await.unwrap_or_default();
    serde_json::from_slice(&requests[index].body).expect("request body is JSON")
}

#[tokio::test]
async fn request_matches_openai_chat_completions_and_sends_bearer_key() {
    let meta = MockServer::start().await;
    let baseten = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer meta-test-key"))
        .and(body_partial_json(json!({"model": "muse-spark-1.3"})))
        .respond_with(text_reply("OK"))
        .expect(1)
        .mount(&meta)
        .await;

    let request = ChatRequest {
        messages: vec![
            Message::system("You plan work."),
            Message::user("Plan the login fix."),
        ],
        tools: vec![ToolSpec {
            name: "read_file".to_owned(),
            description: "Read a file".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        }],
        temperature: Some(0.5),
        max_tokens: Some(256),
        response_format: None,
    };
    let response = client(&meta, &baseten, "baseten")
        .chat(Role::Foreman, request)
        .await
        .expect("chat succeeds");

    assert_eq!(response.content.as_deref(), Some("OK"));
    assert_eq!(
        body_of(&meta, 0).await,
        json!({
            "model": "muse-spark-1.3",
            "messages": [
                {"role": "system", "content": "You plan work."},
                {"role": "user", "content": "Plan the login fix."}
            ],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "Read a file",
                    "parameters": {
                        "type": "object",
                        "properties": {"path": {"type": "string"}},
                        "required": ["path"]
                    }
                }
            }],
            "temperature": 0.5,
            "max_tokens": 256
        })
    );
}

fn capped_client(meta: &MockServer, token_limit_field: &str) -> OpenAiCompatible {
    let meta_url = meta.uri();
    let toml = format!(
        r#"
[[providers]]
name = "meta"
base_url = "{meta_url}/v1"
api_key_env = "META_API_KEY"
default_model = "muse-spark-1.3"
token_limit_field = "{token_limit_field}"

[routing.foreman]
provider = "meta"

[routing.worker]
provider = "meta"

[routing.fast]
provider = "meta"
"#
    );
    client_for(LlmConfig::from_toml_str(&toml).expect("test config is valid"))
}

async fn body_for_capped_call(token_limit_field: &str, max_tokens: Option<u32>) -> Value {
    let meta = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(text_reply("OK"))
        .expect(1)
        .mount(&meta)
        .await;

    let request = ChatRequest {
        messages: vec![Message::user("Reply with the single word OK.")],
        max_tokens,
        ..Default::default()
    };
    capped_client(&meta, token_limit_field)
        .chat(Role::Foreman, request)
        .await
        .expect("chat succeeds");
    body_of(&meta, 0).await
}

#[tokio::test]
async fn token_cap_is_sent_under_the_configured_field_name() {
    let legacy = body_for_capped_call("max_tokens", Some(64)).await;
    assert_eq!(legacy["max_tokens"], json!(64), "cap under max_tokens");
    assert!(
        legacy.get("max_completion_tokens").is_none(),
        "max_completion_tokens is not sent: {legacy}"
    );

    let canonical = body_for_capped_call("max_completion_tokens", Some(64)).await;
    assert_eq!(
        canonical["max_completion_tokens"],
        json!(64),
        "cap under max_completion_tokens"
    );
    assert!(
        canonical.get("max_tokens").is_none(),
        "max_tokens is not sent: {canonical}"
    );

    let uncapped = body_for_capped_call("max_completion_tokens", None).await;
    assert!(
        uncapped.get("max_tokens").is_none() && uncapped.get("max_completion_tokens").is_none(),
        "no cap was set, so no cap field is sent: {uncapped}"
    );
}

#[tokio::test]
async fn tool_history_and_tool_calls_use_openai_shapes() {
    let meta = MockServer::start().await;
    let baseten = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_2",
                        "type": "function",
                        "function": {"name": "read_file", "arguments": "{\"path\":\"b.rs\"}"}
                    }]
                }
            }],
            "usage": {"prompt_tokens": 50, "completion_tokens": 9}
        })))
        .expect(1)
        .mount(&meta)
        .await;

    let history = vec![
        Message::user("Read a.rs"),
        Message::Assistant {
            content: None,
            tool_calls: vec![ToolCall {
                id: "call_1".to_owned(),
                kind: ToolKind::Function,
                function: FunctionCall {
                    name: "read_file".to_owned(),
                    arguments: "{\"path\":\"a.rs\"}".to_owned(),
                },
            }],
        },
        Message::Tool {
            tool_call_id: "call_1".to_owned(),
            content: "fn main() {}".to_owned(),
        },
    ];
    let response = client(&meta, &baseten, "baseten")
        .chat(
            Role::Foreman,
            ChatRequest {
                messages: history,
                ..Default::default()
            },
        )
        .await
        .expect("chat succeeds");

    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].id, "call_2");
    assert_eq!(response.tool_calls[0].function.name, "read_file");
    assert_eq!(
        response.tool_calls[0].function.arguments,
        "{\"path\":\"b.rs\"}"
    );
    assert_eq!(
        body_of(&meta, 0).await["messages"],
        json!([
            {"role": "user", "content": "Read a.rs"},
            {"role": "assistant", "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "read_file", "arguments": "{\"path\":\"a.rs\"}"}
            }]},
            {"role": "tool", "tool_call_id": "call_1", "content": "fn main() {}"}
        ])
    );
}

#[tokio::test]
async fn json_schema_reply_parses_into_typed_struct() {
    let meta = MockServer::start().await;
    let baseten = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(text_reply(r#"{"clash":true,"files":["src/lib.rs"]}"#))
        .expect(1)
        .mount(&meta)
        .await;

    let out = client(&meta, &baseten, "baseten")
        .chat_json::<Verdict>(
            Role::Foreman,
            ChatRequest {
                messages: vec![Message::user("Do agents clash?")],
                ..Default::default()
            },
        )
        .await
        .expect("reply matches the schema");

    assert_eq!(
        out.value,
        Verdict {
            clash: true,
            files: vec!["src/lib.rs".to_owned()],
        }
    );
    assert_eq!(out.call.provider, "meta");
    assert_eq!(out.call.usage.prompt_tokens, 120);

    let body = body_of(&meta, 0).await;
    assert_eq!(body["response_format"]["type"], "json_schema");
    assert_eq!(body["response_format"]["json_schema"]["name"], "Verdict");
    assert!(body["response_format"]["json_schema"]["schema"]["properties"]["clash"].is_object());
}

#[tokio::test]
async fn an_unrouted_director_answers_on_the_foreman_route() {
    let meta = MockServer::start().await;
    let baseten = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(text_reply(r#"{"clash":false,"files":[]}"#))
        .expect(1)
        .mount(&meta)
        .await;

    let out = client(&meta, &baseten, "baseten")
        .chat_json::<Verdict>(
            Role::Director,
            ChatRequest {
                messages: vec![Message::user("Order the tour.")],
                ..Default::default()
            },
        )
        .await
        .expect("the director answers on the foreman's route");

    assert_eq!(out.call.role, Role::Director);
    assert_eq!(out.call.provider, "meta");
}

#[tokio::test]
async fn a_routed_director_answers_on_its_own_provider() {
    let meta = MockServer::start().await;
    let baseten = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(text_reply(r#"{"clash":false,"files":[]}"#))
        .expect(1)
        .mount(&baseten)
        .await;

    let mut config = test_config(&meta, &baseten, "baseten");
    config
        .route_to(Role::Director, "baseten")
        .expect("baseten is defined");
    let out = client_for(config)
        .chat_json::<Verdict>(
            Role::Director,
            ChatRequest {
                messages: vec![Message::user("Order the tour.")],
                ..Default::default()
            },
        )
        .await
        .expect("the director answers on its own route");

    assert_eq!(out.call.provider, "baseten");
    assert!(
        meta.received_requests()
            .await
            .unwrap_or_default()
            .is_empty()
    );
}

#[tokio::test]
async fn schema_invalid_reply_is_a_validation_error_and_still_metered() {
    let meta = MockServer::start().await;
    let baseten = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(text_reply(r#"{"clash":"maybe"}"#))
        .expect(1)
        .mount(&meta)
        .await;

    let client = client(&meta, &baseten, "baseten");
    let err = client
        .chat_json::<Verdict>(
            Role::Foreman,
            ChatRequest {
                messages: vec![Message::user("Do agents clash?")],
                ..Default::default()
            },
        )
        .await
        .expect_err("reply must not parse");

    let trace = err
        .reply_trace()
        .expect("the unusable reply keeps its trace");
    assert_eq!(trace.call.provider, "meta");
    match err {
        LlmError::SchemaValidation { raw, .. } => assert!(raw.contains("maybe")),
        other => panic!("expected SchemaValidation, got {other:?}"),
    }
    assert_eq!(client.ledger().records().len(), 1, "tokens were billed");
}

#[tokio::test]
async fn structured_reply_cut_at_the_output_cap_is_refused_and_names_the_cap() {
    let meta = MockServer::start().await;
    let baseten = MockServer::start().await;
    // A cut reply can still parse as a prefix of valid JSON, so the finish reason must decide.
    let cut = ResponseTemplate::new(200)
        .insert_header("x-request-id", "req-cap-1")
        .set_body_json(json!({
            "choices": [{
                "index": 0,
                "finish_reason": "length",
                "message": {"role": "assistant", "content": r#"{"clash":true,"files":[]}"#}
            }],
            "usage": {"prompt_tokens": 120, "completion_tokens": 8000, "total_tokens": 8120}
        }));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({"max_tokens": 8000})))
        .respond_with(cut)
        .expect(1)
        .mount(&meta)
        .await;

    let client = client(&meta, &baseten, "baseten");
    let err = client
        .chat_json::<Verdict>(
            Role::Foreman,
            ChatRequest {
                messages: vec![Message::user("Do agents clash?")],
                max_tokens: Some(8_000),
                ..Default::default()
            },
        )
        .await
        .expect_err("a reply cut at the cap must not parse");

    match &err {
        LlmError::OutputCapped {
            provider,
            max_tokens,
            ..
        } => {
            assert_eq!(provider, "meta");
            assert_eq!(*max_tokens, Some(8_000));
        }
        other => panic!("expected OutputCapped, got {other:?}"),
    }
    // The cut call's tokens, latency and request id survive the refusal.
    let trace = err.reply_trace().expect("the cut reply keeps its trace");
    assert_eq!(trace.request_id.as_deref(), Some("req-cap-1"));
    assert_eq!(trace.call.provider, "meta");
    assert_eq!(trace.call.usage.prompt_tokens, 120);
    assert_eq!(trace.call.usage.completion_tokens, 8_000);
    assert!(
        trace.call.latency > std::time::Duration::ZERO,
        "the call's latency is kept"
    );
    assert_eq!(
        err.to_string(),
        "meta stopped the reply at the 8000-token output cap before it finished"
    );
    assert_eq!(client.ledger().records().len(), 1, "tokens were billed");
}

#[tokio::test]
async fn structured_reply_cut_without_a_request_cap_names_the_provider_default() {
    let meta = MockServer::start().await;
    let baseten = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "index": 0,
                "finish_reason": "length",
                "message": {"role": "assistant", "content": "{"}
            }],
            "usage": {"prompt_tokens": 120, "completion_tokens": 4096, "total_tokens": 4216}
        })))
        .expect(1)
        .mount(&meta)
        .await;

    let err = client(&meta, &baseten, "baseten")
        .chat_json::<Verdict>(
            Role::Foreman,
            ChatRequest {
                messages: vec![Message::user("Do agents clash?")],
                ..Default::default()
            },
        )
        .await
        .expect_err("a reply cut at the cap must not parse");

    assert!(
        matches!(
            err,
            LlmError::OutputCapped {
                max_tokens: None,
                ..
            }
        ),
        "expected OutputCapped without a cap, got {err:?}"
    );
    assert_eq!(
        err.to_string(),
        "meta stopped the reply at the provider's default output cap before it finished"
    );
}

#[tokio::test]
async fn prompt_fallback_puts_schema_in_prompt_and_parses_fenced_json() {
    let meta = MockServer::start().await;
    let baseten = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer baseten-test-key"))
        .respond_with(text_reply("```json\n{\"clash\":false,\"files\":[]}\n```"))
        .expect(1)
        .mount(&baseten)
        .await;

    let out = client(&meta, &baseten, "baseten")
        .chat_json::<Verdict>(
            Role::Worker,
            ChatRequest {
                messages: vec![Message::user("Do agents clash?")],
                ..Default::default()
            },
        )
        .await
        .expect("fenced JSON parses");

    assert_eq!(
        out.value,
        Verdict {
            clash: false,
            files: vec![],
        }
    );
    assert_eq!(out.call.provider, "baseten");
    let body = body_of(&baseten, 0).await;
    assert!(
        body.get("response_format").is_none(),
        "no response_format in prompt mode"
    );
    assert_eq!(body["messages"][0]["role"], "system");
    let schema_prompt = body["messages"][0]["content"].as_str().unwrap_or_default();
    assert!(schema_prompt.contains("JSON Schema"));
    assert!(schema_prompt.contains("clash"));
}

#[tokio::test]
async fn rate_limit_retries_then_succeeds() {
    let meta = MockServer::start().await;
    let baseten = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(429))
        .up_to_n_times(2)
        .mount(&meta)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(text_reply("OK"))
        .expect(1)
        .mount(&meta)
        .await;

    let response = client(&meta, &baseten, "baseten")
        .chat(
            Role::Foreman,
            ChatRequest {
                messages: vec![Message::user("ping")],
                ..Default::default()
            },
        )
        .await
        .expect("third attempt succeeds");

    assert_eq!(response.content.as_deref(), Some("OK"));
    assert_eq!(meta.received_requests().await.unwrap_or_default().len(), 3);
}

#[tokio::test]
async fn payment_required_fails_fast_with_its_own_variant() {
    let meta = MockServer::start().await;
    let baseten = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(402).set_body_string("credits exhausted"))
        .expect(1)
        .mount(&meta)
        .await;

    let err = client(&meta, &baseten, "baseten")
        .chat(
            Role::Foreman,
            ChatRequest {
                messages: vec![Message::user("ping")],
                ..Default::default()
            },
        )
        .await
        .expect_err("402 must fail");

    match err {
        LlmError::PaymentRequired { provider, body } => {
            assert_eq!(provider, "meta");
            assert_eq!(body, "credits exhausted");
        }
        other => panic!("expected PaymentRequired, got {other:?}"),
    }
}

#[tokio::test]
async fn server_errors_retry_up_to_the_policy_then_report_attempts() {
    let meta = MockServer::start().await;
    let baseten = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503))
        .expect(4)
        .mount(&meta)
        .await;

    let err = client(&meta, &baseten, "baseten")
        .chat(
            Role::Foreman,
            ChatRequest {
                messages: vec![Message::user("ping")],
                ..Default::default()
            },
        )
        .await
        .expect_err("503 on every attempt must fail");

    match err {
        LlmError::ServerError {
            status, attempts, ..
        } => {
            assert_eq!(status, 503);
            assert_eq!(attempts, 4, "one attempt plus three retries");
        }
        other => panic!("expected ServerError, got {other:?}"),
    }
}

#[tokio::test]
async fn other_client_errors_are_not_retried() {
    let meta = MockServer::start().await;
    let baseten = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad model"))
        .expect(1)
        .mount(&meta)
        .await;

    let err = client(&meta, &baseten, "baseten")
        .chat(
            Role::Foreman,
            ChatRequest {
                messages: vec![Message::user("ping")],
                ..Default::default()
            },
        )
        .await
        .expect_err("400 must fail");

    assert!(
        matches!(err, LlmError::Status { status: 400, .. }),
        "got {err:?}"
    );
}

#[tokio::test]
async fn slow_reply_times_out() {
    let meta = MockServer::start().await;
    let baseten = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(text_reply("late").set_delay(Duration::from_secs(3)))
        .mount(&meta)
        .await;

    let err = client(&meta, &baseten, "baseten")
        .chat(
            Role::Foreman,
            ChatRequest {
                messages: vec![Message::user("ping")],
                ..Default::default()
            },
        )
        .await
        .expect_err("reply arrives after the 2 s timeout");

    assert!(matches!(err, LlmError::Timeout { .. }), "got {err:?}");
}

#[tokio::test]
async fn usage_lands_in_the_ledger_per_role_and_model() {
    let meta = MockServer::start().await;
    let baseten = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({"model": "muse-spark-1.3"})))
        .respond_with(text_reply("OK"))
        .expect(2)
        .mount(&meta)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(
            json!({"model": "meta-models/muse-spark-open-1026"}),
        ))
        .respond_with(text_reply("OK"))
        .expect(1)
        .mount(&baseten)
        .await;

    let client = client(&meta, &baseten, "baseten");
    for _ in 0..2 {
        client
            .chat(
                Role::Foreman,
                ChatRequest {
                    messages: vec![Message::user("plan")],
                    ..Default::default()
                },
            )
            .await
            .expect("foreman call succeeds");
    }
    client
        .chat(
            Role::Worker,
            ChatRequest {
                messages: vec![Message::user("edit")],
                ..Default::default()
            },
        )
        .await
        .expect("worker call succeeds");

    let totals = client.ledger().snapshot();
    assert_eq!(totals.len(), 2);
    assert_eq!(totals[0].role, Role::Foreman);
    assert_eq!(totals[0].provider, "meta");
    assert_eq!(totals[0].model, "muse-spark-1.3");
    assert_eq!(totals[0].calls, 2);
    assert_eq!(totals[0].prompt_tokens, 240);
    assert_eq!(totals[0].completion_tokens, 60);
    assert_eq!(totals[1].role, Role::Worker);
    assert_eq!(totals[1].provider, "baseten");
    assert_eq!(totals[1].model, "meta-models/muse-spark-open-1026");
    assert_eq!(totals[1].calls, 1);
}

#[tokio::test]
async fn routing_line_moves_a_role_to_another_provider() {
    let meta = MockServer::start().await;
    let baseten = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(text_reply("OK"))
        .expect(1)
        .mount(&meta)
        .await;

    client(&meta, &baseten, "meta")
        .chat(
            Role::Worker,
            ChatRequest {
                messages: vec![Message::user("edit")],
                ..Default::default()
            },
        )
        .await
        .expect("worker call routed to meta succeeds");

    assert_eq!(
        baseten.received_requests().await.unwrap_or_default().len(),
        0
    );
    assert_eq!(body_of(&meta, 0).await["model"], "muse-spark-1.3");
}

#[tokio::test]
async fn missing_key_fails_before_any_request() {
    let meta = MockServer::start().await;
    let baseten = MockServer::start().await;
    let meta_url = meta.uri();
    let toml = format!(
        r#"
[[providers]]
name = "meta"
base_url = "{meta_url}/v1"
api_key_env = "META_API_KEY"
default_model = "muse-spark-1.3"

[routing.foreman]
provider = "meta"

[routing.worker]
provider = "meta"

[routing.fast]
provider = "meta"
"#
    );
    let config = LlmConfig::from_toml_str(&toml).expect("test config is valid");
    let client = OpenAiCompatible::new(config, ApiKeys::default()).expect("client builds");

    let err = client
        .chat(
            Role::Foreman,
            ChatRequest {
                messages: vec![Message::user("ping")],
                ..Default::default()
            },
        )
        .await
        .expect_err("no key must fail");

    match &err {
        LlmError::MissingKey { provider, env_var } => {
            assert_eq!(provider, "meta");
            assert_eq!(env_var, "META_API_KEY");
        }
        other => panic!("expected MissingKey, got {other:?}"),
    }
    assert!(err.to_string().contains("no key for provider meta"));
    assert_eq!(meta.received_requests().await.unwrap_or_default().len(), 0);
    assert_eq!(
        baseten.received_requests().await.unwrap_or_default().len(),
        0
    );
}

/// Asserts that the `Debug` and `Display` text of `err` both omit `secret`.
fn assert_secret_absent(err: &LlmError, secret: &str) {
    assert!(
        !format!("{err:?}").contains(secret),
        "Debug output contains a configured API key"
    );
    assert!(
        !err.to_string().contains(secret),
        "Display output contains a configured API key"
    );
}

/// Makes one meta call answered by `reply` and returns the error it fails with.
async fn meta_failure(reply: ResponseTemplate) -> LlmError {
    let meta = MockServer::start().await;
    let baseten = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(reply)
        .mount(&meta)
        .await;

    client(&meta, &baseten, "baseten")
        .chat(
            Role::Foreman,
            ChatRequest {
                messages: vec![Message::user("ping")],
                ..Default::default()
            },
        )
        .await
        .expect_err("reply must fail")
}

#[tokio::test]
async fn reflected_key_is_redacted_from_401_and_402_bodies() {
    let unauthorized = meta_failure(
        ResponseTemplate::new(401).set_body_string(format!("invalid api key {META_KEY}")),
    )
    .await;
    match &unauthorized {
        LlmError::Status {
            status: 401, body, ..
        } => assert!(body.contains("<redacted>"), "401 body was not captured"),
        _ => panic!("expected Status with HTTP 401"),
    }
    assert_secret_absent(&unauthorized, META_KEY);

    let unpaid = meta_failure(
        ResponseTemplate::new(402).set_body_string(format!("no credits for {META_KEY}")),
    )
    .await;
    match &unpaid {
        LlmError::PaymentRequired { body, .. } => {
            assert!(body.contains("<redacted>"), "402 body was not captured");
        }
        _ => panic!("expected PaymentRequired"),
    }
    assert_secret_absent(&unpaid, META_KEY);
}

#[tokio::test]
async fn reflected_key_is_redacted_from_malformed_reply_errors() {
    let err = meta_failure(
        ResponseTemplate::new(200).set_body_string(format!(r#"{{"choices":"{META_KEY}"}}"#)),
    )
    .await;

    match &err {
        LlmError::MalformedResponse { detail, .. } => {
            assert!(
                detail.contains("<redacted>"),
                "parser detail was not captured"
            );
        }
        _ => panic!("expected MalformedResponse"),
    }
    assert_secret_absent(&err, META_KEY);
}

#[tokio::test]
async fn reflected_key_is_redacted_from_schema_errors() {
    let meta = MockServer::start().await;
    let baseten = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(text_reply(&format!(
            r#"{{"clash":"{META_KEY}","files":[]}}"#
        )))
        .expect(1)
        .mount(&meta)
        .await;

    let err = client(&meta, &baseten, "baseten")
        .chat_json::<Verdict>(
            Role::Foreman,
            ChatRequest {
                messages: vec![Message::user("Do agents clash?")],
                ..Default::default()
            },
        )
        .await
        .expect_err("reply must not parse");

    match &err {
        LlmError::SchemaValidation { raw, .. } => {
            assert!(raw.contains("<redacted>"), "reply text was not captured");
        }
        _ => panic!("expected SchemaValidation"),
    }
    assert_secret_absent(&err, META_KEY);
}

#[tokio::test]
async fn key_across_the_error_body_budget_leaves_no_fragment() {
    // The key starts 5 bytes before the 512-byte budget ends, so the cut would split it.
    let body = format!("{}{META_KEY}", "a".repeat(507));
    let err = meta_failure(ResponseTemplate::new(401).set_body_string(body)).await;

    match &err {
        LlmError::Status { body, .. } => {
            assert!(body.len() <= 512, "error body exceeds the byte budget");
            assert!(
                !body.contains(&META_KEY[..4]),
                "a fragment of the key survived the budget cut"
            );
        }
        _ => panic!("expected Status with HTTP 401"),
    }
    assert_secret_absent(&err, META_KEY);
}

#[tokio::test]
async fn oversized_error_body_is_cut_to_the_byte_budget() {
    // "é" is two bytes, so the 512-byte cut falls inside a character.
    let body = format!("a{}", "é".repeat(1000));
    let err = meta_failure(ResponseTemplate::new(400).set_body_string(body)).await;

    match &err {
        LlmError::Status {
            status: 400, body, ..
        } => {
            assert!(body.len() <= 512, "error body exceeds the byte budget");
            assert!(body.starts_with('a'), "body lost its leading bytes");
        }
        _ => panic!("expected Status with HTTP 400"),
    }
}

/// A reply that carries the provider's body `id` and echoed `model`.
fn traced_reply(content: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "chatcmpl-body-1",
        "model": "muse-spark-1.3-served",
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": {"role": "assistant", "content": content}
        }],
        "usage": {"prompt_tokens": 12, "completion_tokens": 3}
    }))
}

fn ping() -> ChatRequest {
    ChatRequest {
        messages: vec![Message::user("ping")],
        ..Default::default()
    }
}

#[tokio::test]
async fn request_id_header_is_preferred_over_the_body_id() {
    let meta = MockServer::start().await;
    let baseten = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(traced_reply("OK").insert_header("x-request-id", "req-header-1"))
        .expect(1)
        .mount(&meta)
        .await;

    let traced = client(&meta, &baseten, "baseten")
        .chat_traced(Role::Foreman, ping())
        .await
        .expect("chat succeeds");

    assert_eq!(
        traced.request_id,
        Some(RequestId {
            value: "req-header-1".to_owned(),
            source: RequestIdSource::Header("x-request-id".to_owned()),
        })
    );
    assert_eq!(traced.reply_model.as_deref(), Some("muse-spark-1.3-served"));
}

#[tokio::test]
async fn baseten_request_id_header_is_read() {
    let meta = MockServer::start().await;
    let baseten = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(traced_reply("OK").insert_header("x-baseten-request-id", "bt-req-1"))
        .expect(1)
        .mount(&meta)
        .await;

    let traced = client(&meta, &baseten, "meta")
        .chat_traced(Role::Foreman, ping())
        .await
        .expect("chat succeeds");

    assert_eq!(
        traced.request_id,
        Some(RequestId {
            value: "bt-req-1".to_owned(),
            source: RequestIdSource::Header("x-baseten-request-id".to_owned()),
        })
    );
}

#[tokio::test]
async fn request_id_falls_back_to_the_body_id_when_the_header_is_blank() {
    let meta = MockServer::start().await;
    let baseten = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(traced_reply("OK").insert_header("x-request-id", "   "))
        .expect(1)
        .mount(&meta)
        .await;

    let traced = client(&meta, &baseten, "baseten")
        .chat_traced(Role::Foreman, ping())
        .await
        .expect("chat succeeds");

    assert_eq!(
        traced.request_id,
        Some(RequestId {
            value: "chatcmpl-body-1".to_owned(),
            source: RequestIdSource::Body,
        })
    );
}

#[tokio::test]
async fn reply_with_neither_header_nor_body_id_has_no_request_id() {
    let meta = MockServer::start().await;
    let baseten = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(text_reply("OK"))
        .expect(1)
        .mount(&meta)
        .await;

    let traced = client(&meta, &baseten, "baseten")
        .chat_traced(Role::Foreman, ping())
        .await
        .expect("chat succeeds");

    assert_eq!(traced.request_id, None);
    assert_eq!(traced.reply_model, None);
}

#[tokio::test]
async fn json_reply_keeps_its_request_id() {
    let meta = MockServer::start().await;
    let baseten = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            traced_reply(r#"{"clash":true,"files":["src/lib.rs"]}"#)
                .insert_header("x-request-id", "req-json-1"),
        )
        .expect(1)
        .mount(&meta)
        .await;

    let traced = client(&meta, &baseten, "baseten")
        .chat_json_traced::<Verdict>(Role::Foreman, ping())
        .await
        .expect("reply matches the schema");

    assert!(traced.value.value.clash);
    assert_eq!(
        traced.request_id.map(|id| id.value),
        Some("req-json-1".to_owned())
    );
}

#[derive(Debug, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RouteArgs {
    route: String,
}

#[tokio::test]
async fn tool_call_arguments_parse_against_the_declared_schema() {
    let meta = MockServer::start().await;
    let baseten = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "index": 0,
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_7",
                        "type": "function",
                        "function": {
                            "name": "get_route_status",
                            "arguments": "{\"route\":\"worker-7\"}"
                        }
                    }]
                }
            }],
            "usage": {"prompt_tokens": 40, "completion_tokens": 8}
        })))
        .expect(1)
        .mount(&meta)
        .await;

    let schema = JsonSchemaFormat::for_type::<RouteArgs>();
    let response = client(&meta, &baseten, "baseten")
        .chat(
            Role::Foreman,
            ChatRequest {
                messages: vec![Message::user("Check route worker-7.")],
                tools: vec![ToolSpec {
                    name: "get_route_status".to_owned(),
                    description: "Return the status of one route.".to_owned(),
                    parameters: schema.schema,
                }],
                ..Default::default()
            },
        )
        .await
        .expect("chat succeeds");

    assert_eq!(response.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(response.content, None);
    assert_eq!(response.tool_calls.len(), 1);
    let call = &response.tool_calls[0];
    assert_eq!(call.function.name, "get_route_status");
    let args: RouteArgs =
        serde_json::from_str(&call.function.arguments).expect("arguments match the schema");
    assert_eq!(
        args,
        RouteArgs {
            route: "worker-7".to_owned()
        }
    );

    let body = body_of(&meta, 0).await;
    assert_eq!(body["tools"][0]["function"]["name"], "get_route_status");
    assert_eq!(
        body["tools"][0]["function"]["parameters"]["required"],
        json!(["route"])
    );
}

#[tokio::test]
async fn route_to_moves_a_role_to_another_provider() {
    let meta = MockServer::start().await;
    let baseten = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(text_reply("OK"))
        .expect(1)
        .mount(&meta)
        .await;

    let mut config = test_config(&meta, &baseten, "baseten");
    config
        .route_to(Role::Worker, "meta")
        .expect("meta is a defined provider");
    client_for(config)
        .chat(
            Role::Worker,
            ChatRequest {
                messages: vec![Message::user("edit")],
                ..Default::default()
            },
        )
        .await
        .expect("worker call routed to meta succeeds");

    assert_eq!(
        baseten.received_requests().await.unwrap_or_default().len(),
        0
    );
    assert_eq!(body_of(&meta, 0).await["model"], "muse-spark-1.3");
}

#[tokio::test]
async fn a_turn_with_images_reaches_the_wire_as_an_array_of_parts() {
    let meta = MockServer::start().await;
    let baseten = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(traced_reply("{}"))
        .expect(1)
        .mount(&meta)
        .await;

    let request = ChatRequest {
        messages: vec![
            Message::system("Compare the pages."),
            Message::user_parts(vec![
                ContentPart::text("Does the summary line still hold?"),
                ContentPart::text("BASELINE"),
                ContentPart::png(b"base"),
                ContentPart::text("CANDIDATE"),
                ContentPart::png(b"cand"),
            ]),
        ],
        max_tokens: Some(800),
        ..Default::default()
    };
    let traced =
        ModelClient::chat_traced(&client(&meta, &baseten, "baseten"), Role::Foreman, request)
            .await
            .expect("chat succeeds");

    assert_eq!(traced.reply_model.as_deref(), Some("muse-spark-1.3-served"));
    assert_eq!(
        body_of(&meta, 0).await["messages"],
        json!([
            {"role": "system", "content": "Compare the pages."},
            {"role": "user", "content": [
                {"type": "text", "text": "Does the summary line still hold?"},
                {"type": "text", "text": "BASELINE"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,YmFzZQ=="}},
                {"type": "text", "text": "CANDIDATE"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,Y2FuZA=="}}
            ]}
        ])
    );
}
