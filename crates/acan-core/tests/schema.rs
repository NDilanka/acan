use std::collections::BTreeMap;

use acan_core::{
    Actor, CallFailure, Event, Generation, MemberId, ModelUsage, Rejection, Replica, Seq, Stamped,
    Step, Timestamp, TokenCount, WorldRef,
};

#[test]
fn a_stamped_event_round_trips_through_the_stored_json() {
    let founder: MemberId = "owner".parse().expect("valid member");
    let stamped = Stamped {
        seq: Seq::new(0),
        at: Timestamp::from_millis(1_000),
        actor: Actor::Member(founder.clone()),
        event: Event::WorldOpened {
            world: WorldRef {
                id: "w".parse().expect("valid world"),
                repo: "demo".to_owned(),
            },
            founder,
        },
    };
    let json = serde_json::to_string(&stamped).expect("serializes");
    assert!(
        json.contains(r#""type":"world_opened""#),
        "event tag in {json}"
    );
    assert_eq!(
        serde_json::from_str::<Stamped>(&json).expect("deserializes"),
        stamped
    );
}

#[test]
fn model_usage_serializes_to_json() {
    let usage = ModelUsage {
        totals: BTreeMap::from([(
            "muse-spark".to_owned(),
            BTreeMap::from([(
                Step::Split,
                TokenCount {
                    input: 900,
                    output: 120,
                    calls: 1,
                },
            )]),
        )]),
    };
    let json = serde_json::to_string(&usage).expect("serializes");
    assert_eq!(
        serde_json::from_str::<ModelUsage>(&json).expect("deserializes"),
        usage
    );
}

#[test]
fn model_used_without_detail_still_decodes() {
    let old = r#"{"type":"model_used","model":"muse-spark","step":"split","input_tokens":900,"output_tokens":120}"#;
    let decoded: Event = serde_json::from_str(old).expect("old shape decodes");
    assert_eq!(
        decoded,
        Event::ModelUsed {
            model: "muse-spark".to_owned(),
            provider: None,
            step: Step::Split,
            input_tokens: Some(900),
            output_tokens: Some(120),
            failure: None,
            detail: None,
        }
    );
    assert_eq!(
        serde_json::to_string(&decoded).expect("serializes"),
        old,
        "no provider, failure or detail means no new key"
    );
    let noted = Event::ModelUsed {
        model: "muse-spark".to_owned(),
        provider: None,
        step: Step::Scope,
        input_tokens: Some(900),
        output_tokens: Some(120),
        failure: None,
        detail: Some("scope draft for o-1 rev 2".to_owned()),
    };
    let json = serde_json::to_string(&noted).expect("serializes");
    assert!(json.contains(r#""step":"scope""#), "{json}");
    assert_eq!(
        serde_json::from_str::<Event>(&json).expect("deserializes"),
        noted
    );
}

#[test]
fn a_failed_model_used_has_no_tokens_and_no_totals() {
    let failed = r#"{"type":"model_used","model":"muse-spark","provider":"meta","step":"director","failure":"timeout","detail":"tour for receipt 1a2b3c4d: timeout"}"#;
    let decoded: Event = serde_json::from_str(failed).expect("a failed entry decodes");
    assert_eq!(
        decoded,
        Event::ModelUsed {
            model: "muse-spark".to_owned(),
            provider: Some("meta".to_owned()),
            step: Step::Director,
            input_tokens: None,
            output_tokens: None,
            failure: Some(CallFailure::Timeout),
            detail: Some("tour for receipt 1a2b3c4d: timeout".to_owned()),
        }
    );
    assert_eq!(
        serde_json::to_string(&decoded).expect("serializes"),
        failed,
        "a failed entry writes no token keys"
    );
    let opened = Stamped {
        seq: Seq::new(0),
        at: Timestamp::from_millis(1_000),
        actor: Actor::System,
        event: Event::WorldOpened {
            world: WorldRef {
                id: "w".parse().expect("valid world"),
                repo: "demo".to_owned(),
            },
            founder: "owner".parse().expect("valid member"),
        },
    };
    let entry = |event| Stamped {
        seq: Seq::new(1),
        at: Timestamp::from_millis(2_000),
        actor: Actor::System,
        event,
    };
    let before = Replica::from_log(vec![opened.clone()]).expect("opening folds");
    let after = Replica::from_log(vec![opened.clone(), entry(decoded)]).expect("the entry folds");
    assert_eq!(
        after.world().usage(),
        before.world().usage(),
        "a failed call touches no total"
    );
    assert!(after.world().usage().totals.is_empty());

    // The same call with its tokens counts once, so the check above is not vacuous.
    let answered = Event::ModelUsed {
        model: "muse-spark".to_owned(),
        provider: Some("meta".to_owned()),
        step: Step::Director,
        input_tokens: Some(900),
        output_tokens: Some(120),
        failure: None,
        detail: None,
    };
    let counted = Replica::from_log(vec![opened, entry(answered)]).expect("the reply folds");
    let count = counted.world().usage().totals["muse-spark"][&Step::Director];
    assert_eq!(
        (count.input, count.output, count.calls),
        (900, 120, 1),
        "a reply is counted"
    );
}

#[test]
fn a_tampered_path_in_stored_json_is_refused() {
    let declared = |file: &str| {
        format!(
            r#"{{"seq":0,"at":0,"actor":{{"kind":"system"}},"event":{{"type":"declared","decl":{{"task":0,"generation":1,"index":"r","footprint":{{"files":["{file}"],"symbols":[],"interfaces":[],"inserts":[]}}}}}}}}"#
        )
    };
    assert!(
        serde_json::from_str::<Stamped>(&declared("src/api/mod.rs")).is_ok(),
        "control parses"
    );
    assert!(serde_json::from_str::<Stamped>(&declared("../etc/passwd")).is_err());
}

#[test]
fn a_rejection_round_trips_for_a_joined_desktop() {
    let rejections = [
        Rejection::WrongState {
            what: "task".into(),
            found: "declaring".into(),
        },
        Rejection::Unknown {
            what: "order".into(),
            id: "o-7".to_owned(),
        },
        Rejection::StaleGeneration {
            current: Some(Generation::new(3)),
        },
        Rejection::LeaseExpired,
    ];
    for rejection in rejections {
        let json = serde_json::to_string(&rejection).expect("serializes");
        assert_eq!(
            serde_json::from_str::<Rejection>(&json).expect("deserializes"),
            rejection,
            "json {json}"
        );
    }
}
