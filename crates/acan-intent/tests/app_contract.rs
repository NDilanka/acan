//! The protected app contract: the install phase and the runtime agree with the version.

use acan_intent::{AppContract, MAX_SEQUENCE_TIMEOUT_MS, RunLimits};
use serde_json::{Value, json};

/// A valid `acan-app/2` node contract with the install phase `npm ci` names.
fn node_contract() -> Value {
    json!({
        "version": "acan-app/2",
        "runner_contract": "acan-runner/1",
        "runtime": "node-24",
        "boot": ["npm", "run", "start"],
        "install": ["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"],
        "port_env": "PORT",
        "ready": { "path": "/health", "status": 200, "timeout_ms": 10000 },
        "store": "fresh_per_sequence",
        "clock": { "kind": "http_endpoint", "path": "/__acan/clock" },
        "sinks": ["store"],
        "env": { "NODE_ENV": "test" },
        "limits": {
            "max_requests": 64,
            "max_output_bytes": 65536,
            "sequence_timeout_ms": 30000
        }
    })
}

fn decode(value: Value) -> AppContract {
    serde_json::from_value(value).expect("the contract decodes")
}

#[test]
fn a_contract_sets_the_sequence_cap_up_to_the_hard_maximum() {
    // The policy's 30 s default is the cap only when the contract sets none.
    let limits = |sequence_timeout_ms| RunLimits {
        max_requests: 64,
        max_output_bytes: 65_536,
        sequence_timeout_ms,
    };
    let mut acquit_like = node_contract();
    acquit_like["limits"]["sequence_timeout_ms"] = json!(120_000);
    assert_eq!(
        decode(acquit_like).limits.sequence_cap_ms(30_000),
        120_000,
        "a contract above the policy default is honored"
    );
    assert_eq!(limits(10_000).sequence_cap_ms(30_000), 10_000);
    assert_eq!(
        limits(600_000).sequence_cap_ms(30_000),
        MAX_SEQUENCE_TIMEOUT_MS,
        "a contract above the maximum is clamped"
    );
    assert_eq!(
        limits(0).sequence_cap_ms(30_000),
        30_000,
        "a contract that sets none keeps the policy's timeout"
    );
    assert_eq!(
        limits(0).sequence_cap_ms(900_000),
        MAX_SEQUENCE_TIMEOUT_MS,
        "the policy's timeout is clamped the same way"
    );
}

#[test]
fn a_node_install_is_npm_ci_with_ignore_scripts_and_the_allowed_flags() {
    decode(node_contract())
        .check()
        .expect("the install is admitted");
    let mut bare = node_contract();
    bare["install"] = json!(["npm", "ci", "--ignore-scripts"]);
    decode(bare).check().expect("the bare install is admitted");
}

#[test]
fn an_install_argv_that_is_not_npm_ci_with_ignore_scripts_is_refused() {
    for argv in [
        json!(["npm", "install", "--ignore-scripts"]),
        json!(["npm", "ci"]),
        json!(["npm", "ci", "--no-audit"]),
        json!(["npm", "ci", "--ignore-scripts", "--force"]),
        json!(["npm", "ci", "--ignore-scripts", "--prefix", "/"]),
        json!(["sh", "-c", "npm ci --ignore-scripts"]),
    ] {
        let mut value = node_contract();
        value["install"] = argv.clone();
        decode(value)
            .check()
            .expect_err(&format!("the argv {argv} is refused"));
    }
}

#[test]
fn an_empty_install_argv_does_not_decode() {
    let mut value = node_contract();
    value["install"] = json!([]);
    serde_json::from_value::<AppContract>(value).expect_err("an empty install is refused");
}

#[test]
fn an_acan_app_1_contract_has_no_install_phase_and_no_node_runtime() {
    let mut with_install = node_contract();
    with_install["version"] = json!("acan-app/1");
    with_install["runtime"] = json!("bun-1.4.4");
    decode(with_install)
        .check()
        .expect_err("a v1 contract refuses an install");
    let mut node = node_contract();
    node["version"] = json!("acan-app/1");
    node["install"] = Value::Null;
    decode(node)
        .check()
        .expect_err("a v1 contract refuses the node runtime");
}

#[test]
fn a_bun_contract_refuses_an_install_phase() {
    let mut value = node_contract();
    value["runtime"] = json!("bun-1.4.4");
    decode(value)
        .check()
        .expect_err("only node-24 has an install phase");
}

#[test]
fn the_bun_fixture_is_an_admitted_acan_app_1_contract() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/app-contract.json"))
        .expect("the fixture is JSON");
    decode(fixture)
        .check()
        .expect("the bun fixture is admitted as it is");
}

#[test]
fn a_node_contract_without_an_install_is_admitted() {
    let mut value = node_contract();
    value.as_object_mut().expect("an object").remove("install");
    decode(value)
        .check()
        .expect("a node app with no install phase is admitted");
}
