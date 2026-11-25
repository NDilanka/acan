use acan_intent::{
    Block, GenUiError, GenUiPlace, LinkTarget, MAX_BLOCKS, MAX_CALLS_PER_VIEW_PER_TURN, MAX_CHECKS,
    MAX_LABEL_CHARS, MAX_LIST_ITEMS, MAX_PROSE_CHARS, MAX_SCOPE_PATHS, MAX_SNIPPET_CHARS,
    MAX_TABLE_COLUMNS, MAX_TABLE_ROWS, MAX_TARGET_CHARS, MAX_TOTAL_BYTES,
    MAX_TRACKED_VIEWS_PER_TURN, MAX_VIEW_KEY_LEN, Proposal, ReceiptNotesProposal, ReportProposal,
    Tone, TurnBudget, ValidatedProposal, classify_link_target, mcp_tool_spec, mcp_tool_specs,
    parse_and_validate_proposal, validate_proposal,
};
use serde_json::{Value, json};

const BIDI_CONTROLS: [char; 12] = [
    '\u{061c}', '\u{200e}', '\u{200f}', '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}', '\u{202e}',
    '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
];

const LINE_SEPARATORS: [char; 2] = ['\u{2028}', '\u{2029}'];

fn parse(place: GenUiPlace, payload: Value) -> Result<ValidatedProposal, GenUiError> {
    parse_and_validate_proposal(place, &payload.to_string())
}

fn report_with_blocks(blocks: Vec<Block>) -> Proposal {
    Proposal::Report(ReportProposal {
        title: "Changes".into(),
        summary: None,
        blocks,
    })
}

fn parse_block(block: Value) -> Result<ValidatedProposal, GenUiError> {
    parse(
        GenUiPlace::Report,
        json!({"title": "Changes", "blocks": [block]}),
    )
}

fn invalid_field(result: Result<ValidatedProposal, GenUiError>, expected_field: &str) {
    assert!(
        matches!(&result, Err(GenUiError::InvalidField { field, .. }) if *field == expected_field),
        "expected an invalid {expected_field} error, got {result:?}"
    );
}

#[test]
fn all_block_kinds_round_trip_and_validate() {
    let specimens = [
        json!({"kind":"heading","level":3,"text":"Details"}),
        json!({"kind":"text","text":"First line.\n\tSecond line."}),
        json!({"kind":"callout","tone":"warning","title":"Note","text":"Review this.\n\tThanks."}),
        json!({"kind":"key_value","items":[{"key":"Check","value":"Build"}]}),
        json!({"kind":"bullet_list","items":["Build","Test"]}),
        json!({"kind":"check_list","items":[{"label":"Build","passed":true,"detail":"Completed"}]}),
        json!({"kind":"status_badge","tone":"success","label":"Ready"}),
        json!({"kind":"metric","label":"Time","value":"40 ms","delta":"-2 ms"}),
        json!({"kind":"file_ref","path":"src/lib.rs","line":1,"note":"Definitions"}),
        json!({"kind":"diff_snippet","path":"src/lib.rs","before":"\ta\n","after":"\tb\n","note":"Edit"}),
        json!({"kind":"code_snippet","language":"rust","code":"fn main() {\n\tlog();\n}","caption":"Entry"}),
        json!({"kind":"command_output","command":"cargo test","exit_code":-1,"stdout":"\tout\n","stderr":"\terr\n"}),
        json!({"kind":"table","columns":["Key","Value"],"rows":[["Branch","main"],["Note",""]]}),
        json!({"kind":"progress","label":"Files","current":2,"total":2}),
        json!({"kind":"link","label":"Documentation","target":"https://example.com"}),
        json!({"kind":"divider"}),
    ];
    let blocks = specimens
        .iter()
        .map(|specimen| {
            let block: Block = serde_json::from_value(specimen.clone()).expect("block decodes");
            assert_eq!(serde_json::to_value(&block).unwrap(), *specimen);
            block
        })
        .collect::<Vec<_>>();
    let proposal = report_with_blocks(blocks);
    let decoded = parse(
        GenUiPlace::Report,
        json!({"title":"Changes","blocks":specimens}),
    )
    .expect("wire blocks validate");
    assert_eq!(decoded.proposal(), &proposal);
    assert_eq!(validate_proposal(proposal).unwrap(), decoded);
    for (tone, name) in [
        (Tone::Info, "info"),
        (Tone::Success, "success"),
        (Tone::Warning, "warning"),
        (Tone::Danger, "danger"),
    ] {
        assert_eq!(serde_json::to_value(tone).unwrap(), json!(name));
        assert_eq!(serde_json::from_value::<Tone>(json!(name)).unwrap(), tone);
    }
}

#[test]
fn the_tool_name_selects_one_strict_envelope() {
    for (name, place, payload) in [
        (
            "draft_order",
            GenUiPlace::DraftOrder,
            json!({"intent":"Fix the build","scope_paths":["src/lib.rs"],"checks":["cargo test"],"blocks":[{"kind":"divider"}]}),
        ),
        (
            "card_evidence",
            GenUiPlace::CardEvidence,
            json!({"summary":"Build log","blocks":[{"kind":"divider"}]}),
        ),
        (
            "receipt_notes",
            GenUiPlace::ReceiptNotes,
            json!({"blocks":[{"kind":"divider"}]}),
        ),
        (
            "report",
            GenUiPlace::Report,
            json!({"title":"Changes","summary":"One edit","blocks":[{"kind":"divider"}]}),
        ),
    ] {
        assert_eq!(place.tool_name(), name);
        assert_eq!(GenUiPlace::from_tool_name(name), Some(place));
        assert_eq!(serde_json::to_value(place).unwrap(), name);
        assert_eq!(
            serde_json::from_value::<GenUiPlace>(json!(name)).unwrap(),
            place
        );

        let proposal = parse(place, payload.clone()).expect("place payload validates");
        assert_eq!(proposal.place(), place);
        assert_eq!(proposal.proposal().place(), place);
        assert_eq!(proposal.blocks(), &[Block::Divider {}]);
        assert_eq!(proposal.clone().into_inner().blocks(), proposal.blocks());

        let mut unknown_field = payload;
        unknown_field["extra"] = json!(false);
        assert!(matches!(
            parse(place, unknown_field),
            Err(GenUiError::InvalidJson { .. })
        ));
    }
    assert_eq!(GenUiPlace::from_tool_name("Report"), None);
    assert!(matches!(
        parse(
            GenUiPlace::DraftOrder,
            json!({"title":"Changes","blocks":[]})
        ),
        Err(GenUiError::InvalidJson { .. })
    ));
    assert_eq!(
        parse(
            GenUiPlace::ReceiptNotes,
            json!({"summary":"A note","blocks":[]})
        )
        .unwrap()
        .into_inner(),
        Proposal::ReceiptNotes(ReceiptNotesProposal {
            summary: Some("A note".into()),
            blocks: vec![],
        })
    );
    invalid_field(
        parse(GenUiPlace::ReceiptNotes, json!({"blocks":[]})),
        "summary",
    );
    invalid_field(
        parse(GenUiPlace::ReceiptNotes, json!({"summary":"","blocks":[]})),
        "summary",
    );
}

#[test]
fn wire_enums_accept_only_exact_string_spellings() {
    for raw in [
        r#"{"warning":null}"#,
        r#"["warning"]"#,
        r#""Warning""#,
        "false",
        "0",
        "null",
    ] {
        assert!(serde_json::from_str::<Tone>(raw).is_err(), "{raw}");
        let tone: Value = serde_json::from_str(raw).unwrap();
        assert!(
            serde_json::from_value::<Tone>(tone.clone()).is_err(),
            "{raw}"
        );
        assert!(
            matches!(
                parse_block(json!({"kind":"status_badge","tone":tone,"label":"Check"})),
                Err(GenUiError::InvalidJson { .. })
            ),
            "{raw}"
        );
    }
    for raw in [
        r#"{"report":null}"#,
        r#"["report"]"#,
        r#""Report""#,
        "false",
        "0",
        "null",
    ] {
        assert!(serde_json::from_str::<GenUiPlace>(raw).is_err(), "{raw}");
        let place: Value = serde_json::from_str(raw).unwrap();
        assert!(
            serde_json::from_value::<GenUiPlace>(place).is_err(),
            "{raw}"
        );
    }
    assert_eq!(
        serde_json::from_str::<Tone>(r#""warn\u0069ng""#).unwrap(),
        Tone::Warning
    );
    assert_eq!(
        serde_json::from_str::<GenUiPlace>(r#""rep\u006frt""#).unwrap(),
        GenUiPlace::Report
    );
}

#[test]
fn optional_fields_are_omitted_and_reject_explicit_nulls() {
    for (block, parent, fields) in [
        (
            json!({"kind":"callout","tone":"info","text":"Note"}),
            "",
            &["title"][..],
        ),
        (
            json!({"kind":"check_list","items":[{"label":"Build","passed":true}]}),
            "/items/0",
            &["detail"],
        ),
        (
            json!({"kind":"metric","label":"Time","value":"1 ms"}),
            "",
            &["delta"],
        ),
        (
            json!({"kind":"file_ref","path":"src/lib.rs"}),
            "",
            &["line", "note"],
        ),
        (
            json!({"kind":"diff_snippet","path":"src/lib.rs","before":"","after":""}),
            "",
            &["note"],
        ),
        (
            json!({"kind":"code_snippet","code":"fn main() {}"}),
            "",
            &["language", "caption"],
        ),
        (
            json!({"kind":"command_output","command":"cargo test","exit_code":0,"stdout":""}),
            "",
            &["stderr"],
        ),
    ] {
        let validated = parse_block(block.clone()).unwrap();
        assert_eq!(serde_json::to_value(&validated.blocks()[0]).unwrap(), block);
        for field in fields {
            let mut with_null = block.clone();
            with_null.pointer_mut(parent).unwrap()[*field] = Value::Null;
            assert!(
                serde_json::from_value::<Block>(with_null.clone()).is_err(),
                "{with_null}"
            );
            assert!(
                matches!(
                    parse_block(with_null.clone()),
                    Err(GenUiError::InvalidJson { .. })
                ),
                "{with_null}"
            );
        }
    }
    for (place, mut payload) in [
        (
            GenUiPlace::DraftOrder,
            json!({"intent":"Build","scope_paths":[],"checks":[],"blocks":[]}),
        ),
        (
            GenUiPlace::ReceiptNotes,
            json!({"blocks":[{"kind":"divider"}]}),
        ),
        (GenUiPlace::Report, json!({"title":"Changes","blocks":[]})),
    ] {
        let serialized = match parse(place, payload.clone()).unwrap().into_inner() {
            Proposal::DraftOrder(draft) => serde_json::to_value(draft),
            Proposal::ReceiptNotes(notes) => serde_json::to_value(notes),
            Proposal::Report(report) => serde_json::to_value(report),
            other => panic!("unexpected proposal: {other:?}"),
        };
        assert_eq!(serialized.unwrap(), payload);
        payload["summary"] = Value::Null;
        assert!(
            matches!(parse(place, payload), Err(GenUiError::InvalidJson { .. })),
            "{place:?}"
        );
    }
}

#[test]
fn block_shapes_refuse_extra_fields_actions_children_and_positionals() {
    for block in [
        json!({"kind":"text","text":"hello","extra":true}),
        json!({"kind":"text","text":"hello","id":"t"}),
        json!({"kind":"divider","extra":true}),
        json!({"kind":"divider","id":"d"}),
        json!({"kind":"divider","children":[{"kind":"text","text":"nested"}]}),
        json!({"kind":"link","label":"Click","target":"README.md","callback":"save"}),
        json!({"kind":"table","columns":["Block"],"rows":[[{"kind":"divider"}]]}),
        json!({"kind":"button","label":"Click"}),
        json!({"kind":"key_value","items":[{"key":"k","value":"v","id":"extra"}]}),
        json!({"kind":"check_list","items":[{"label":"k","passed":true,"extra":false}]}),
        json!({"kind":"key_value","items":[["k","v"]]}),
        json!({"kind":"check_list","items":[["k",true,null]]}),
        json!(["text", "hello"]),
    ] {
        assert!(
            matches!(
                parse_block(block.clone()),
                Err(GenUiError::InvalidJson { .. })
            ),
            "{block}"
        );
    }
    assert!(serde_json::from_str::<Block>(r#"{"kind":"divider","id":"d"}"#).is_err());
    assert!(matches!(
        parse(GenUiPlace::Report, json!(["Changes", null, []])),
        Err(GenUiError::InvalidJson { .. })
    ));
}

#[test]
fn duplicate_keys_are_refused_before_typed_decoding() {
    for (raw, key) in [
        (r#"{"title":"A","title":"B","blocks":[]}"#, "title"),
        (r#"{"title":"A","ti\u0074le":"B","blocks":[]}"#, "title"),
        (
            r#"{"title":"A","blocks":[{"kind":"divider","kind":"text","text":"A"}]}"#,
            "kind",
        ),
        (
            r#"{"title":"A","blocks":[{"kind":"key_value","items":[{"key":"a","value":"1","value":"2"}]}]}"#,
            "value",
        ),
        // The checker walks even fields that typed decoding would refuse.
        (
            r#"{"unknown":[[{"deep":{"x":true,"x":false}}]],"blocks":[]}"#,
            "x",
        ),
    ] {
        assert_eq!(
            parse_and_validate_proposal(GenUiPlace::Report, raw),
            Err(GenUiError::DuplicateKey { key: key.into() })
        );
    }
    for raw in [
        r#"{"title":"A","blocks":[}"#,
        r#"{"title":"A","blocks":[]} []"#,
        r#"{"title":42,"blocks":[]}"#,
        r#"{"title":"A"}"#,
    ] {
        assert!(matches!(
            parse_and_validate_proposal(GenUiPlace::Report, raw),
            Err(GenUiError::InvalidJson { .. })
        ));
    }
}

#[test]
fn required_text_numeric_ranges_and_control_rules_are_enforced() {
    for level in [0, 4] {
        invalid_field(
            parse_block(json!({"kind":"heading","level":level,"text":"Heading"})),
            "level",
        );
    }
    for (block, field) in [
        (json!({"kind":"heading","level":1,"text":""}), "text"),
        (json!({"kind":"text","text":" "}), "text"),
        (json!({"kind":"callout","tone":"danger","text":""}), "text"),
        (
            json!({"kind":"status_badge","tone":"info","label":""}),
            "label",
        ),
        (json!({"kind":"metric","label":"","value":"1"}), "label"),
        (json!({"kind":"metric","label":"Time","value":""}), "value"),
        (json!({"kind":"code_snippet","code":""}), "code"),
        (
            json!({"kind":"command_output","command":"","exit_code":0,"stdout":""}),
            "command",
        ),
        (
            json!({"kind":"progress","label":"","current":0,"total":1}),
            "label",
        ),
        (
            json!({"kind":"link","label":"","target":"README.md"}),
            "label",
        ),
        (json!({"kind":"bullet_list","items":[""]}), "items"),
        (json!({"kind":"heading","level":1,"text":"a\nb"}), "text"),
        (
            json!({"kind":"file_ref","path":"README.md","line":0}),
            "line",
        ),
        (
            json!({"kind":"file_ref","path":"README.md","note":"a\tb"}),
            "note",
        ),
        (json!({"kind":"text","text":"a\0b"}), "text"),
        (json!({"kind":"text","text":"a\u{0085}b"}), "text"),
        (json!({"kind":"code_snippet","code":"a\rb"}), "code"),
        (
            json!({"kind":"command_output","command":"cargo test","exit_code":0,"stdout":"","stderr":"\u{007f}"}),
            "stderr",
        ),
    ] {
        invalid_field(parse_block(block), field);
    }
    for (current, total) in [(0, 0), (2, 1)] {
        assert_eq!(
            parse_block(json!({"kind":"progress","label":"Count","current":current,"total":total})),
            Err(GenUiError::InvalidProgress { current, total })
        );
    }
    invalid_field(
        parse(GenUiPlace::Report, json!({"title":"","blocks":[]})),
        "title",
    );
    invalid_field(
        parse(GenUiPlace::CardEvidence, json!({"summary":"","blocks":[]})),
        "summary",
    );
    for (intent, checks, field) in [
        ("", vec![], "intent"),
        ("Build", vec![""], "checks"),
        ("Build", vec!["cargo test\ncargo check"], "checks"),
    ] {
        invalid_field(
            parse(
                GenUiPlace::DraftOrder,
                json!({"intent":intent,"scope_paths":[],"checks":checks,"blocks":[]}),
            ),
            field,
        );
    }
}

#[test]
fn text_rejects_bidi_controls_and_single_line_separators() {
    for c in BIDI_CONTROLS.into_iter().chain(LINE_SEPARATORS) {
        let text = format!("a{c}b");
        invalid_field(
            parse_block(json!({"kind":"heading","level":1,"text":text})),
            "text",
        );
        invalid_field(
            parse_block(json!({"kind":"table","columns":["A"],"rows":[[text]]})),
            "rows",
        );
        invalid_field(
            parse_block(json!({"kind":"file_ref","path":"src/lib.rs","note":text})),
            "note",
        );
        invalid_field(
            parse(
                GenUiPlace::Report,
                json!({"title":"Changes","summary":text,"blocks":[]}),
            ),
            "summary",
        );
    }
    for c in BIDI_CONTROLS {
        let text = format!("a{c}b");
        for (block, field) in [
            (json!({"kind":"text","text":text}), "text"),
            (json!({"kind":"callout","tone":"info","text":text}), "text"),
            (json!({"kind":"code_snippet","code":text}), "code"),
            (
                json!({"kind":"diff_snippet","path":"src/lib.rs","before":text,"after":""}),
                "before",
            ),
            (
                json!({"kind":"diff_snippet","path":"src/lib.rs","before":"","after":text}),
                "after",
            ),
            (
                json!({"kind":"command_output","command":"cargo test","exit_code":0,"stdout":text}),
                "stdout",
            ),
            (
                json!({"kind":"command_output","command":"cargo test","exit_code":0,"stdout":"","stderr":text}),
                "stderr",
            ),
        ] {
            invalid_field(parse_block(block), field);
        }
    }
    assert!(parse_block(json!({"kind":"heading","level":1,"text":"עברית العربية"})).is_ok());
    assert!(parse_block(json!({"kind":"text","text":"a\u{2028}b\u{2029}c\n\td"})).is_ok());
}

#[test]
fn refs_scope_paths_and_repo_links_share_the_portable_repo_grammar() {
    for path in [
        "",
        "../x",
        "/abs",
        "C:/x",
        "C:x",
        "a\\b",
        "\\rooted",
        "a//b",
        "./x",
        "a/./b",
        "x/",
        "src/a\0b",
        "src/a\tb",
        "file.txt:stream",
        "COM¹",
        "src/cOm².txt",
        "CoM³/nested/file.txt",
        "LPT¹.log",
        "src/LpT²/file.txt",
        "lpt³",
    ]
    .into_iter()
    .map(str::to_owned)
    .chain(
        BIDI_CONTROLS
            .into_iter()
            .chain(LINE_SEPARATORS)
            .map(|c| format!("src/a{c}b")),
    ) {
        assert_eq!(
            parse_block(json!({"kind":"file_ref","path":path})),
            Err(GenUiError::InvalidPath { field: "path" }),
            "{path:?}"
        );
        assert_eq!(
            parse_block(json!({"kind":"link","label":"Docs","target":path})),
            Err(GenUiError::InvalidLink),
            "{path:?}"
        );
        assert_eq!(
            parse_block(json!({"kind":"diff_snippet","path":path,"before":"","after":""})),
            Err(GenUiError::InvalidPath { field: "path" }),
            "{path:?}"
        );
        assert_eq!(
            parse(
                GenUiPlace::DraftOrder,
                json!({"intent":"Build","scope_paths":[path],"checks":[],"blocks":[]})
            ),
            Err(GenUiError::InvalidPath {
                field: "scope_paths"
            }),
            "{path:?}"
        );
    }
    assert!(parse_block(json!({"kind":"file_ref","path":"docs/API reference.md"})).is_ok());
    assert!(
        parse_block(
            json!({"kind":"diff_snippet","path":".gitignore","before":"","after":"target/\n"})
        )
        .is_ok()
    );
}

#[test]
fn links_classify_strict_web_urls_and_literal_repo_paths() {
    let link = |target: &str| parse_block(json!({"kind":"link","label":"Docs","target":target}));
    for target in [
        "https://example.com",
        "http://localhost:8080/docs?query=1#top",
        "https://Sub-Domain.Example.com:1/docs%20and%2Fmore?a=b&x=@y#top",
        "https://192.0.2.1:65535/",
        "HTTPS://[::1]/",
        "https://[::ffff:192.0.2.2]:443/docs",
    ] {
        assert_eq!(classify_link_target(target), Ok(LinkTarget::WebUrl(target)));
        assert!(link(target).is_ok(), "{target}");
    }
    for target in [
        "src/lib.rs",
        "docs/COM⁴.txt",
        "src/COM¹notes.md",
        "docs/café.md",
        "docs/1%25.txt",
    ] {
        assert_eq!(
            classify_link_target(target),
            Ok(LinkTarget::RepoPath(target))
        );
        assert!(link(target).is_ok(), "{target}");
    }
    for target in [
        "javascript:alert(1)",
        "JavaScript:alert(1)",
        "data:text/html,hi",
        "vbscript:msgbox(1)",
        "file:///etc/passwd",
        "ftp://example.com",
        "http:example.com",
        "https://",
        "https:///docs",
        "http://?query=1",
        "https://:443",
        "https://user@",
        "https://user:pass@example.com",
        "https://user@[::1]",
        "https://%65xample.com",
        "https://exa_mple.com",
        "https://exämple.com",
        "https://-example.com",
        "https://example-.com",
        "https://.example.com",
        "https://example..com",
        "https://example.com.",
        "https://256.1.2.3",
        "https://127.1",
        "https://0x7f000001",
        "https://0177.0.0.1",
        "https://::1/",
        "https://[]",
        "https://[example.com]",
        "https://[::1]extra",
        "https://[fe80::1%25eth0]",
        "https://[::1]:",
        "https://example.com:",
        "https://example.com:0",
        "https://example.com:65536",
        "https://example.com:+443",
        "https://example.com:bad",
        "https://\\example.com",
        "//example.com",
        "../src/lib.rs",
        "src/a b",
        "src/a\u{00a0}b",
        "src/%2e%2E/file",
        "src/a%2fb",
        "src/a%2Fb",
        "src/a%5cb",
        "src/a%5Cb",
        "https://example.com/a b",
        "https://example.com/a\nb",
        "https://example.com/a\0b",
        "https://example.com/café",
        "https://example.com/a%zz",
        "https://example.com/?q=%2",
        "https://example.com/?q=\"",
        "https://example.com/#<tag>",
        "https://example.com/#a#b",
    ] {
        assert_eq!(
            classify_link_target(target),
            Err(GenUiError::InvalidLink),
            "{target:?}"
        );
        assert_eq!(link(target), Err(GenUiError::InvalidLink), "{target:?}");
    }
    for c in BIDI_CONTROLS.into_iter().chain(LINE_SEPARATORS) {
        let target = format!("https://example.com/docs?query=a{c}b#top");
        assert_eq!(link(&target), Err(GenUiError::InvalidLink), "{target:?}");
    }
}

#[test]
fn text_and_target_lengths_count_characters() {
    let heading = |len| parse_block(json!({"kind":"heading","level":1,"text":"é".repeat(len)}));
    assert!(heading(MAX_LABEL_CHARS).is_ok());
    invalid_field(heading(MAX_LABEL_CHARS + 1), "text");
    let text = |len| parse_block(json!({"kind":"text","text":"é".repeat(len)}));
    assert!(text(MAX_PROSE_CHARS).is_ok());
    invalid_field(text(MAX_PROSE_CHARS + 1), "text");

    let code = |len| parse_block(json!({"kind":"code_snippet","code":"é".repeat(len)}));
    assert!(code(MAX_SNIPPET_CHARS).is_ok());
    invalid_field(code(MAX_SNIPPET_CHARS + 1), "code");
    for (mut block, field) in [
        (
            json!({"kind":"diff_snippet","path":"src/lib.rs","before":"","after":""}),
            "before",
        ),
        (
            json!({"kind":"diff_snippet","path":"src/lib.rs","before":"","after":""}),
            "after",
        ),
        (
            json!({"kind":"command_output","command":"cargo test","exit_code":0,"stdout":""}),
            "stdout",
        ),
        (
            json!({"kind":"command_output","command":"cargo test","exit_code":0,"stdout":""}),
            "stderr",
        ),
    ] {
        block[field] = json!("x".repeat(MAX_SNIPPET_CHARS + 1));
        invalid_field(parse_block(block), field);
    }
    invalid_field(
        parse_block(json!({"kind":"file_ref","path":"a","note":"n".repeat(MAX_PROSE_CHARS + 1)})),
        "note",
    );
    assert_eq!(
        parse_block(json!({"kind":"file_ref","path":"a".repeat(MAX_TARGET_CHARS + 1)})),
        Err(GenUiError::InvalidPath { field: "path" })
    );
    assert_eq!(
        parse_block(
            json!({"kind":"link","label":"Docs","target":"a".repeat(MAX_TARGET_CHARS + 1)})
        ),
        Err(GenUiError::InvalidLink)
    );
}

#[test]
fn list_sizes_are_bounded_for_blocks_and_for_draft_fields() {
    for (kind, item) in [
        ("key_value", json!({"key":"Name","value":"Build"})),
        ("bullet_list", json!("Build")),
        ("check_list", json!({"label":"Build","passed":false})),
    ] {
        let list = |len| parse_block(json!({"kind":kind,"items":vec![item.clone(); len]}));
        invalid_field(list(0), "items");
        assert!(list(MAX_LIST_ITEMS).is_ok(), "{kind}");
        invalid_field(list(MAX_LIST_ITEMS + 1), "items");
    }
    let draft = |paths, checks| {
        parse(
            GenUiPlace::DraftOrder,
            json!({
                "intent":"Build","scope_paths":vec!["src/lib.rs"; paths],
                "checks":vec!["cargo test"; checks],"blocks":[]
            }),
        )
    };
    assert!(draft(MAX_SCOPE_PATHS, MAX_CHECKS).is_ok());
    invalid_field(draft(MAX_SCOPE_PATHS + 1, 0), "scope_paths");
    invalid_field(draft(0, MAX_CHECKS + 1), "checks");
    for field in ["intent", "checks", "summary"] {
        let mut payload = json!({"intent":"Build","scope_paths":[],"checks":[],"blocks":[]});
        let long = "x".repeat(MAX_PROSE_CHARS + 1);
        payload[field] = if field == "checks" {
            json!([long])
        } else {
            json!(long)
        };
        invalid_field(parse(GenUiPlace::DraftOrder, payload), field);
    }
}

#[test]
fn tables_bound_dimensions_and_require_rectangular_rows() {
    let table = |cols, rows| parse_block(json!({"kind":"table","columns":cols,"rows":rows}));
    invalid_field(table(json!([]), json!([])), "columns");
    invalid_field(table(json!([""]), json!([])), "columns");
    invalid_field(
        table(json!(vec!["Column"; MAX_TABLE_COLUMNS + 1]), json!([])),
        "columns",
    );
    assert!(table(json!(["Column"]), json!([])).is_ok());
    assert!(
        table(
            json!(vec!["Column"; MAX_TABLE_COLUMNS]),
            json!(vec![vec!["Cell"; MAX_TABLE_COLUMNS]; MAX_TABLE_ROWS]),
        )
        .is_ok()
    );
    invalid_field(
        table(
            json!(["Column"]),
            json!(vec![vec!["Cell"]; MAX_TABLE_ROWS + 1]),
        ),
        "rows",
    );
    assert_eq!(
        table(json!(["A", "B"]), json!([["1", "2"], ["3"]])),
        Err(GenUiError::TableShapeMismatch {
            row: 1,
            expected: 2,
            actual: 1
        })
    );
}

#[test]
fn block_and_byte_caps_apply_to_raw_and_typed_proposals() {
    let raw = |len| {
        parse(
            GenUiPlace::Report,
            json!({"title":"Empty","blocks":vec![json!({"kind":"divider"}); len]}),
        )
    };
    let typed = |len| validate_proposal(report_with_blocks(vec![Block::Divider {}; len]));
    let too_many = Err(GenUiError::TooManyBlocks { limit: MAX_BLOCKS });
    assert!(raw(MAX_BLOCKS).is_ok());
    assert!(typed(MAX_BLOCKS).is_ok());
    assert_eq!(raw(MAX_BLOCKS + 1), too_many);
    assert_eq!(typed(MAX_BLOCKS + 1), too_many);

    let mut padded = r#"{"title":"Changes","blocks":[]}"#.to_owned();
    padded.push_str(&" ".repeat(MAX_TOTAL_BYTES - padded.len()));
    let too_large = Err(GenUiError::PayloadTooLarge {
        limit: MAX_TOTAL_BYTES,
    });
    assert!(parse_and_validate_proposal(GenUiPlace::Report, &padded).is_ok());
    padded.push(' ');
    assert_eq!(
        parse_and_validate_proposal(GenUiPlace::Report, &padded),
        too_large
    );

    // These fields fit the character limit individually, but their JSON takes two bytes per é.
    let block = Block::CodeSnippet {
        language: None,
        code: "é".repeat(MAX_SNIPPET_CHARS),
        caption: None,
    };
    assert!(validate_proposal(report_with_blocks(vec![block.clone()])).is_ok());
    assert_eq!(
        validate_proposal(report_with_blocks(vec![block; 2])),
        too_large
    );
}

#[test]
fn compact_proposals_pass_both_byte_caps_at_the_limit() {
    let shape = |first: &str, second: &str| {
        json!({"title":"Changes","blocks":[
            {"kind":"code_snippet","code":first},
            {"kind":"code_snippet","code":second}
        ]})
        .to_string()
    };
    let room = MAX_TOTAL_BYTES - shape("", "").len();
    let code = "é".repeat(room / 4);
    let raw = shape(&code, &(code.clone() + &"x".repeat(room % 4)));
    assert_eq!(raw.len(), MAX_TOTAL_BYTES);
    let validated = parse_and_validate_proposal(GenUiPlace::Report, &raw).unwrap();
    assert_eq!(
        validate_proposal(validated.clone().into_inner()),
        Ok(validated)
    );
}

#[test]
fn oversized_in_memory_tables_hit_the_payload_cap() {
    // Each table has a valid shape and each cell fits its field limit. Together they would
    // serialize to over 18 MiB, but validation stops counting at the first excess write.
    let table = Block::Table {
        columns: vec!["Column".into(); MAX_TABLE_COLUMNS],
        rows: vec![vec!["x".repeat(MAX_PROSE_CHARS); MAX_TABLE_COLUMNS]; MAX_TABLE_ROWS],
    };
    assert_eq!(
        validate_proposal(report_with_blocks(vec![table; MAX_BLOCKS])),
        Err(GenUiError::PayloadTooLarge {
            limit: MAX_TOTAL_BYTES
        })
    );
}

#[test]
fn budget_is_per_place_and_view_and_resets_after_each_turn() {
    let mut budget = TurnBudget::new();
    let report = GenUiPlace::Report;
    let card = GenUiPlace::CardEvidence;
    assert_eq!(
        budget.remaining(report, "view"),
        MAX_CALLS_PER_VIEW_PER_TURN
    );
    for remaining in [2, 1, 0] {
        assert_eq!(budget.record_call(report, "view"), Ok(remaining));
        assert_eq!(budget.remaining(report, "view"), remaining);
    }
    for _ in 0..2 {
        assert_eq!(
            budget.record_call(report, "view"),
            Err(GenUiError::BudgetExceeded {
                place: report,
                limit: MAX_CALLS_PER_VIEW_PER_TURN
            })
        );
    }
    assert_eq!(budget.remaining(report, "view"), 0);
    assert_eq!(budget.record_call(report, "other"), Ok(2));
    assert_eq!(budget.record_call(card, "view"), Ok(2));
    budget.reset_turn();
    for (place, view) in [(report, "view"), (report, "other"), (card, "view")] {
        assert_eq!(budget.remaining(place, view), MAX_CALLS_PER_VIEW_PER_TURN);
    }
    assert_eq!(budget.record_call(report, "view"), Ok(2));
}

#[test]
fn budget_rejects_invalid_keys_and_bounds_distinct_pairs_across_places() {
    let mut budget = TurnBudget::new();
    let report = GenUiPlace::Report;
    let card = GenUiPlace::CardEvidence;
    let notes = GenUiPlace::ReceiptNotes;
    let too_long = "é".repeat(MAX_VIEW_KEY_LEN + 1);
    for key in [
        "",
        " a",
        "a ",
        "a b",
        "a\tb",
        "a\u{00a0}b",
        "a\0b",
        "a\u{007f}b",
        &too_long,
    ] {
        assert!(
            matches!(
                budget.record_call(report, key),
                Err(GenUiError::InvalidField {
                    field: "view_key",
                    ..
                })
            ),
            "{key:?}"
        );
        assert_eq!(budget.remaining(report, key), 0, "{key:?}");
    }
    for c in BIDI_CONTROLS.into_iter().chain(LINE_SEPARATORS) {
        let key = format!("a{c}b");
        assert!(
            matches!(
                budget.record_call(report, &key),
                Err(GenUiError::InvalidField {
                    field: "view_key",
                    ..
                })
            ),
            "{key:?}"
        );
        assert_eq!(budget.remaining(report, &key), 0, "{key:?}");
    }

    let longest = "é".repeat(MAX_VIEW_KEY_LEN);
    assert_eq!(budget.record_call(report, &longest), Ok(2));
    assert_eq!(budget.record_call(card, &longest), Ok(2));
    for i in 1..MAX_TRACKED_VIEWS_PER_TURN / 2 {
        let key = format!("view-{i}");
        assert_eq!(budget.record_call(report, &key), Ok(2));
        assert_eq!(budget.record_call(card, &key), Ok(2));
    }
    assert_eq!(budget.remaining(notes, &longest), 0);
    assert_eq!(
        budget.record_call(notes, &longest),
        Err(GenUiError::TurnBudgetFull {
            limit: MAX_TRACKED_VIEWS_PER_TURN
        })
    );
    assert_eq!(budget.remaining(report, &longest), 2);
    assert_eq!(budget.record_call(report, &longest), Ok(1));
    assert_eq!(budget.record_call(report, &longest), Ok(0));
    budget.reset_turn();
    assert_eq!(
        budget.remaining(notes, &longest),
        MAX_CALLS_PER_VIEW_PER_TURN
    );
    assert_eq!(budget.record_call(notes, &longest), Ok(2));
}

#[test]
fn tool_specs_are_flat_and_model_facing() {
    fn assert_simple(schema: &Value) {
        match schema {
            Value::Object(fields) => {
                for (key, value) in fields {
                    assert!(!matches!(
                        key.as_str(),
                        "oneOf" | "anyOf" | "allOf" | "$ref" | "$defs" | "pattern"
                    ));
                    assert_simple(value);
                }
            }
            Value::Array(items) => items.iter().for_each(assert_simple),
            _ => {}
        }
    }

    let specs = mcp_tool_specs();
    assert_eq!(
        specs.each_ref().map(|spec| spec.name),
        ["draft_order", "card_evidence", "receipt_notes", "report",]
    );
    for (spec, required) in specs.into_iter().zip([
        json!(["intent", "scope_paths", "checks", "blocks"]),
        json!(["summary", "blocks"]),
        json!(["blocks"]),
        json!(["title", "blocks"]),
    ]) {
        assert_eq!(GenUiPlace::from_tool_name(spec.name), Some(spec.place));
        assert_eq!(mcp_tool_spec(spec.place), spec);
        assert!(spec.description.starts_with("Propose "));
        let schema = spec.input_schema;
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"], required);
        assert_eq!(
            schema["properties"]["blocks"],
            json!({"type":"array","items":{"type":"object"}})
        );
        assert_simple(&schema);
    }
}
