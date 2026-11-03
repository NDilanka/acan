//! Calls the provider configured for one role and prints the reply, usage, and request id.
//!
//! Run with `cargo run -p acan-llm --example smoke -- --role foreman`. The modes are:
//! - plain: free text, the default.
//! - `--json`: `chat_json` with a small schema, the path the server's jobs take.
//! - `--tool`: one function tool with a required argument. It passes only when the reply holds
//!   a `tool_calls` entry whose arguments parse against the tool's schema.
//!
//! `--provider <name>` routes the role to that provider for this run. `--max-tokens <n>` caps the
//! reply in plain mode. Keys come from the environment or `crates/acan-llm/.env`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use acan_llm::{
    ApiKeys, CallRecord, ChatRequest, ChatResponse, JsonSchemaFormat, LlmConfig, Message,
    OpenAiCompatible, RequestId, Role, Structured, TokenLimitField, ToolSpec, Traced, UnknownRole,
};
use schemars::JsonSchema;
use serde::Deserialize;

const DEFAULT_PROMPT: &str = "Reply with the single word OK.";
const JSON_PROMPT: &str = "Answer with the single word yes as answer, and the number 5 as count.";
const TOOL_NAME: &str = "get_route_status";
const TOOL_PROMPT: &str = "Use the get_route_status tool to check the status of route worker-7.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Plain,
    Json,
    Tool,
}

struct Args {
    role: Role,
    provider: Option<String>,
    config: PathBuf,
    prompt: Option<String>,
    mode: Mode,
    max_tokens: Option<u32>,
}

/// The typed reply `--json` asks for.
#[derive(Debug, Deserialize, JsonSchema)]
struct Probe {
    /// The answer as one lowercase word.
    answer: String,
    /// The number the answer stands for.
    count: u32,
}

/// The arguments `get_route_status` takes. Unknown fields fail the schema.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RouteStatusArgs {
    /// The route to look up, for example worker-7.
    route: String,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut parsed = Args {
        role: Role::Foreman,
        provider: None,
        config: Path::new(env!("CARGO_MANIFEST_DIR")).join("llm.toml"),
        prompt: None,
        mode: Mode::Plain,
        max_tokens: None,
    };
    while let Some(flag) = args.next() {
        if let Some(mode) = mode_flag(&flag) {
            if parsed.mode != Mode::Plain {
                return Err("pass --json or --tool, not both".to_owned());
            }
            parsed.mode = mode;
            continue;
        }
        let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--role" => {
                parsed.role = value.parse().map_err(|err: UnknownRole| err.to_string())?;
            }
            "--provider" => parsed.provider = Some(value),
            "--config" => parsed.config = PathBuf::from(value),
            "--prompt" => parsed.prompt = Some(value),
            "--max-tokens" => {
                parsed.max_tokens = Some(
                    value
                        .parse()
                        .map_err(|_| format!("--max-tokens needs a whole number, got {value}"))?,
                );
            }
            other => return Err(format!("unknown flag {other}")),
        }
    }
    if parsed.max_tokens.is_some() && parsed.mode != Mode::Plain {
        return Err("--max-tokens works with plain mode only".to_owned());
    }
    Ok(parsed)
}

fn mode_flag(flag: &str) -> Option<Mode> {
    match flag {
        "--json" => Some(Mode::Json),
        "--tool" => Some(Mode::Tool),
        _ => None,
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    dotenvy::from_path(Path::new(env!("CARGO_MANIFEST_DIR")).join(".env")).ok();
    dotenvy::dotenv().ok();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn wire_name(field: TokenLimitField) -> &'static str {
    match field {
        TokenLimitField::MaxTokens => "max_tokens",
        TokenLimitField::MaxCompletionTokens => "max_completion_tokens",
    }
}

/// Adds the source chain, so a DNS or TLS cause shows up next to the variant name.
fn describe(err: &dyn std::error::Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        text = format!("{text}, caused by {cause}");
        source = cause.source();
    }
    text
}

fn print_call(call: &CallRecord, request_id: Option<&RequestId>, reply_model: Option<&str>) {
    println!(
        "role {} provider {} model {}",
        call.role, call.provider, call.model
    );
    println!("reply model {}", reply_model.unwrap_or("none"));
    match request_id {
        Some(id) => println!("request id {} source {}", id.value, id.source),
        None => println!("request id none"),
    }
    println!(
        "latency {} ms, prompt tokens {}, completion tokens {}",
        call.latency.as_millis(),
        call.usage.prompt_tokens,
        call.usage.completion_tokens
    );
}

async fn run() -> Result<(), String> {
    let args = parse_args(std::env::args().skip(1))?;
    let mut config = LlmConfig::load(&args.config)
        .map_err(|err| format!("{err}, copy llm.example.toml to llm.toml or pass --config"))?;
    if let Some(provider) = &args.provider {
        config
            .route_to(args.role, provider)
            .map_err(|err| describe(&err))?;
    }
    let keys = ApiKeys::from_env(&config);
    let client = OpenAiCompatible::new(config.clone(), keys).map_err(|err| err.to_string())?;

    match args.mode {
        Mode::Plain => plain(&client, &config, args).await,
        Mode::Json => json(&client, args.role, args.prompt).await,
        Mode::Tool => tool(&client, args.role, args.prompt).await,
    }
}

async fn plain(client: &OpenAiCompatible, config: &LlmConfig, args: Args) -> Result<(), String> {
    let request = ChatRequest {
        messages: vec![Message::user(
            args.prompt.unwrap_or_else(|| DEFAULT_PROMPT.to_owned()),
        )],
        max_tokens: args.max_tokens,
        ..Default::default()
    };
    let traced = client
        .chat_traced(args.role, request)
        .await
        .map_err(|err| describe(&err))?;
    let Traced {
        value: response,
        request_id,
        reply_model,
    } = traced;
    print_call(&response.call, request_id.as_ref(), reply_model.as_deref());
    let field = config
        .providers()
        .iter()
        .find(|provider| provider.name == response.call.provider)
        .map_or("unknown", |provider| wire_name(provider.token_limit_field));
    let cap = args
        .max_tokens
        .map_or_else(|| "none".to_owned(), |cap| cap.to_string());
    println!("token cap {cap} field {field}");
    println!("reply:");
    println!("{}", response.content.unwrap_or_default());
    Ok(())
}

async fn json(client: &OpenAiCompatible, role: Role, prompt: Option<String>) -> Result<(), String> {
    let request = ChatRequest {
        messages: vec![Message::user(
            prompt.unwrap_or_else(|| JSON_PROMPT.to_owned()),
        )],
        ..Default::default()
    };
    let traced = client
        .chat_json_traced::<Probe>(role, request)
        .await
        .map_err(|err| describe(&err))?;
    let Traced {
        value: Structured {
            value: probe, call, ..
        },
        request_id,
        reply_model,
    } = traced;
    print_call(&call, request_id.as_ref(), reply_model.as_deref());
    println!("result answer {} count {}", probe.answer, probe.count);
    println!("pass");
    Ok(())
}

async fn tool(client: &OpenAiCompatible, role: Role, prompt: Option<String>) -> Result<(), String> {
    let schema = JsonSchemaFormat::for_type::<RouteStatusArgs>();
    let request = ChatRequest {
        messages: vec![Message::user(
            prompt.unwrap_or_else(|| TOOL_PROMPT.to_owned()),
        )],
        tools: vec![ToolSpec {
            name: TOOL_NAME.to_owned(),
            description: "Return the status of one route.".to_owned(),
            parameters: schema.schema,
        }],
        ..Default::default()
    };
    let traced = client
        .chat_traced(role, request)
        .await
        .map_err(|err| describe(&err))?;
    let Traced {
        value: response,
        request_id,
        reply_model,
    } = traced;
    print_call(&response.call, request_id.as_ref(), reply_model.as_deref());
    let (raw, args) = route_status_call(&response)?;
    println!(
        "result tool call {TOOL_NAME} route {} arguments {raw}",
        args.route
    );
    println!("pass");
    Ok(())
}

/// Finds the `get_route_status` call and parses its arguments against the tool schema.
fn route_status_call(response: &ChatResponse) -> Result<(String, RouteStatusArgs), String> {
    let call = response
        .tool_calls
        .iter()
        .find(|call| call.function.name == TOOL_NAME)
        .ok_or_else(|| {
            format!(
                "reply has no {TOOL_NAME} tool call, finish reason {:?}",
                response.finish_reason
            )
        })?;
    let args = serde_json::from_str::<RouteStatusArgs>(&call.function.arguments)
        .map_err(|err| format!("{TOOL_NAME} arguments do not match the schema: {err}"))?;
    Ok((call.function.arguments.clone(), args))
}
