//! Runs Acan's split job once against the configured provider, in an in-memory world, and prints
//! what it returned.
//!
//! Run with `cargo run -p acan-server --example live_jobs`. The routing file defaults to
//! `crates/acan-llm/llm.toml`; `--config <path>` picks another. Keys come from the environment
//! or `crates/acan-llm/.env`, and are never printed.
//!
//! The job prints the provider and model that answered, the latency, the tokens, and the parsed
//! reply. The reply is then applied to the origin, so a reply the origin would refuse stops the
//! run with the origin's reason. A job a failed live call answered offline shows `provider
//! offline`, with a warning on stderr.
//!
//! The review and explain jobs are not run here. Review is retired. Explain needs a clash, and a
//! legacy claim needs a readback verdict that only the retired review could give.

#[path = "../tests/support/mod.rs"]
#[expect(
    dead_code,
    reason = "the example opens a demo and posts one order; the rest of the support module is for the tests"
)]
mod support;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use acan_core::{Command, OrderId, SystemCmd};
use acan_llm::{ApiKeys, CallRecord, LlmConfig, ModelClient, Role};
use acan_server::Models;
use acan_server::model::{self, SplitReply};
use support::Demo;

const ORDER: &str = "make the greeting configurable from the settings file";

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::WARN)
        .init();
    dotenvy::from_path(Path::new(env!("CARGO_MANIFEST_DIR")).join("../acan-llm/.env")).ok();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn config_path() -> Result<PathBuf, String> {
    let mut args = std::env::args().skip(1);
    let mut path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../acan-llm/llm.toml");
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--config" => {
                path = PathBuf::from(args.next().ok_or("--config needs a path")?);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(path)
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

fn show(job: &str, call: &CallRecord) {
    println!("{job}: provider {} model {}", call.provider, call.model);
    println!(
        "  latency {} ms, prompt tokens {}, completion tokens {}",
        call.latency.as_millis(),
        call.usage.prompt_tokens,
        call.usage.completion_tokens
    );
    if call.provider == "offline" {
        println!("  the live call failed, so this is the offline reply");
    }
}

fn apply(demo: &mut Demo, cmd: SystemCmd) -> Result<(), String> {
    demo.try_apply(Command::System(cmd))
        .map(|_| ())
        .map_err(|rejection| format!("the origin refused the reply: {rejection}"))
}

async fn run() -> Result<(), String> {
    let path = config_path()?;
    let config = LlmConfig::load(&path)
        .map_err(|err| format!("{err}, copy llm.example.toml to llm.toml or pass --config"))?;
    let keys = ApiKeys::from_env(&config);
    let models = Models::live(config, keys).map_err(|err| err.to_string())?;

    let mut demo = Demo::open();
    let order = demo.post_order(ORDER);
    split(&models, &mut demo, order).await?;
    println!("the split job parsed, and the origin accepted its reply");
    Ok(())
}

async fn split(models: &Models, demo: &mut Demo, order: OrderId) -> Result<(), String> {
    let machines = model::plant_machines(demo.index());
    let request = model::split_request(demo.world(), order, &machines)
        .ok_or("the order is missing from the world")?;
    let reply = models
        .chat_json::<SplitReply>(Role::Foreman, request)
        .await
        .map_err(|err| describe(&err))?;
    show("split", &reply.call);
    println!("  parsed: {:#?}", reply.value);
    let rev = demo
        .world()
        .order(order)
        .ok_or("the order is missing from the world")?
        .rev;
    let cmd =
        model::split_result(order, rev, &machines, reply.value).map_err(|err| err.to_string())?;
    apply(demo, cmd)?;
    println!("  the origin accepted the split");
    Ok(())
}
