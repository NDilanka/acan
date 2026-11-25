use acan_core::{Actor, Event, Generation, MemberId, Rejection, Seq, Stamped, Timestamp, WorldRef};

#[test]
fn a_stamped_event_round_trips_through_the_stored_json() {
    let founder: MemberId = "owner".parse().expect("valid member");
    let stamped = Stamped {
        seq: Seq::new(0),
        at: Timestamp::from_millis(1_000),
        actor: Actor::Member(founder.clone()),
        shareable: false,
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
fn only_an_unrecognized_string_tag_can_use_the_unknown_event_fallback() {
    for json in [
        r#"{"type":"order_posted"}"#,
        r#"{"type":"world_opened","world":{"id":"w","repo":"demo"},"founder":false}"#,
        r#"{"type":"world_opened","world":{"id":"w","repo":"demo","repo":"twice"},"founder":"owner"}"#,
        r#"{"type":"order_posted","type":"future_event"}"#,
        r#"{"type":null}"#,
        r#"{"type":17}"#,
        r#"{"type":0,"world":{"id":"w","repo":"demo"},"founder":"owner"}"#,
        r#"{"payload":{"type":"future_event"}}"#,
        r#"["world_opened",{"id":"w","repo":"demo"},"owner"]"#,
        r#"["world_opened",[{"id":"w","repo":"demo"},"owner"]]"#,
        r#"[]"#,
        r#"null"#,
        r#""future_event""#,
    ] {
        assert!(
            serde_json::from_str::<Event>(json).is_err(),
            "decoded {json}"
        );
    }
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
