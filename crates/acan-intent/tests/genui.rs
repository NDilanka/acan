use acan_intent::{
    Block, GenUiError, GenUiPlace, MAX_BLOCKS, MAX_CALLS_PER_VIEW_PER_TURN, MAX_CHECKS,
    MAX_LABEL_CHARS, MAX_LIST_ITEMS, MAX_PROSE_CHARS, MAX_SCOPE_PATHS, MAX_SNIPPET_CHARS,
    MAX_TABLE_COLUMNS, MAX_TABLE_ROWS, MAX_TARGET_CHARS, MAX_TOTAL_BYTES, Proposal,
    ReceiptNotesProposal, ReportProposal, Tone, TurnBudget, ValidatedProposal, mcp_tool_spec,
    mcp_tool_specs, parse_and_validate_proposal, validate_proposal,
};
use serde_json::{Value, json};

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
fn refs_and_scope_paths_share_the_portable_repo_grammar() {
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
    ] {
        assert_eq!(
            parse_block(json!({"kind":"file_ref","path":path})),
            Err(GenUiError::InvalidPath { field: "path" }),
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
fn links_allow_web_hosts_or_safe_paths_but_not_other_schemes() {
    let link = |target| parse_block(json!({"kind":"link","label":"Docs","target":target}));
    for target in [
        "https://example.com",
        "http://localhost:8080/docs?query=1#top",
        "HTTPS://[::1]/",
        "src/lib.rs",
    ] {
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
        "https://[]",
        "https://example.com:bad",
        "https://\\example.com",
        "//example.com",
        "../src/lib.rs",
        "src/a b",
        "src/a\u{00a0}b",
        "https://example.com/a\nb",
        "https://example.com/a\0b",
    ] {
        assert_eq!(link(target), Err(GenUiError::InvalidLink), "{target:?}");
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
