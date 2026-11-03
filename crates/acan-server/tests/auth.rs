use std::str::FromStr;

use acan_core::{AgentId, MemberId};
use acan_server::{Principal, Tokens};

fn owner() -> MemberId {
    MemberId::from_str("owner").expect("valid member id")
}

#[test]
fn owner_token_is_minted_once_and_then_reloaded() {
    let dir = tempfile::tempdir().expect("temp dir");

    let (tokens, minted) = Tokens::load_or_create(dir.path(), &owner()).expect("first load");
    let token = minted.expect("the first start mints an owner token");
    assert_eq!(
        tokens.principal(token.as_str()),
        Some(Principal::Member(owner())),
        "the minted token acts as the owner"
    );
    assert_eq!(
        tokens.principal("acan_not-a-token"),
        None,
        "an unknown token has no principal"
    );

    let (reloaded, minted_again) =
        Tokens::load_or_create(dir.path(), &owner()).expect("second load");
    assert!(minted_again.is_none(), "a later start mints nothing");
    assert_eq!(
        reloaded.principal(token.as_str()),
        Some(Principal::Member(owner())),
        "the same token still works after a restart"
    );
}

#[test]
fn token_debug_output_never_shows_the_secret() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (tokens, minted) = Tokens::load_or_create(dir.path(), &owner()).expect("load");
    let token = minted.expect("owner token");

    let rendered = format!("{token:?} {tokens:?}");
    assert!(
        !rendered.contains(token.as_str()),
        "debug output must not contain the token: {rendered}"
    );
}

#[test]
fn principals_keep_their_kind() {
    let agent = Principal::Agent(AgentId::from_str("agent-1").expect("valid agent id"));
    let encoded = serde_json::to_string(&agent).expect("encode principal");
    assert_eq!(encoded, r#"{"kind":"agent","id":"agent-1"}"#);
}
