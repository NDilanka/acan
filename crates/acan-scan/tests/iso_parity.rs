//! Parity with acan-iso, the crate the kind rules were copied from.
//!
//! This test, and the `acan-iso` dev-dependency it needs, go away when acan-iso is removed
//! (V1 step 6). Until then it checks that the kind names, the footprints and the kind rules here
//! equal acan-iso's, so the copy is proven and not trusted.

use acan_scan::{Footprint, Kind, classify, footprint};

/// One word from each row of the kind rules, in table order (the assembler row last). Each word
/// reaches its row through the name pass, the directory pass and the file-name pass.
const ROW_WORDS: [&str; 16] = [
    "auth",
    "ratelimit",
    "middleware",
    "server",
    "routes",
    "components",
    "store",
    "schemas",
    "workers",
    "webhooks",
    "config",
    "deploy",
    "scripts",
    "clients",
    "util",
    "service",
];

/// One package from each import-rule row that has packages, so each is reached by the package pass.
const ROW_PACKAGES: [&str; 13] = [
    "jsonwebtoken",
    "express-rate-limit",
    "http-proxy",
    "express",
    "react",
    "prisma",
    "zod",
    "bull",
    "socket.io",
    "dotenv",
    "dockerode",
    "commander",
    "axios",
];

/// The reasons the rules give, as prefixes. Each must be reached by at least one case.
const FAMILIES: [&str; 9] = [
    "no files",
    "only tests",
    "generated code",
    "name word",
    "directory word",
    "most file names",
    "imports",
    "most files are tsx or jsx",
    "no rule matched",
];

/// The wire name of a kind, such as `switchboard`.
fn wire<T: serde::Serialize>(kind: T) -> String {
    serde_json::to_value(kind)
        .expect("a kind serializes")
        .as_str()
        .expect("a kind serializes to a string")
        .to_owned()
}

/// The acan-iso kind with the same wire name as `kind`.
fn iso_kind(kind: Kind) -> acan_iso::Kind {
    let name = wire(kind);
    acan_iso::Kind::ALL
        .into_iter()
        .find(|iso| wire(*iso) == name)
        .unwrap_or_else(|| panic!("acan-iso has no kind named {name}"))
}

#[test]
fn kind_names_match_acan_iso_in_order() {
    let ours: Vec<String> = Kind::ALL.into_iter().map(wire).collect();
    let theirs: Vec<String> = acan_iso::Kind::ALL.into_iter().map(wire).collect();
    assert_eq!(ours, theirs);
}

#[test]
fn footprints_match_acan_iso() {
    for kind in Kind::ALL {
        let theirs = acan_iso::footprint(iso_kind(kind));
        let expected = Footprint {
            w: theirs.w,
            d: theirs.d,
            top: theirs.top,
        };
        assert_eq!(footprint(kind), expected, "footprint of {}", wire(kind));
    }
}

/// One input to `classify`, with owned strings so a case can be built from a format string.
struct Case {
    name: String,
    dir: String,
    paths: Vec<String>,
    packages: Vec<&'static str>,
}

fn case(name: &str, dir: &str, paths: &[&str], packages: &[&'static str]) -> Case {
    Case {
        name: name.to_owned(),
        dir: dir.to_owned(),
        paths: paths.iter().map(|path| (*path).to_owned()).collect(),
        packages: packages.to_vec(),
    }
}

/// The inputs: every rule family, the edges of each rule, and the fallback.
fn cases() -> Vec<Case> {
    let mut out = vec![
        // Crate reasons come before every other rule.
        case("auth", "src/auth", &[], &[]),
        case(
            "auth",
            "src/auth",
            &["src/auth/login.test.ts", "tests/api.ts"],
            &[],
        ),
        case(
            "store",
            "src/gen",
            &["src/generated/api.ts", "dist/out.js"],
            &[],
        ),
        case("store", "", &["src/api.pb.ts"], &[]),
        case("store", "", &["src/model_pb2.py"], &[]),
        case("store", "", &["src/schema.g.dart"], &[]),
        case("store", "", &["src/store.test.ts", "src/store.ts"], &[]),
        case("plain", "", &["src/a_test.go", "src/a.go"], &[]),
        // The name pass: the innermost segment, any separator, any case.
        case("crates/acan-server/src/settings", "", &["src/x.rs"], &[]),
        case("auth-service", "", &["src/x.ts"], &[]),
        case("user_store", "", &["src/x.ts"], &[]),
        case("Crates\\Acan-Server\\Routes", "", &["src/x.rs"], &[]),
        case("app/store", "src", &["src/x.ts"], &[]),
        // The directory pass.
        case(
            "plain",
            "crates/acan-server/src/settings",
            &["src/x.rs"],
            &[],
        ),
        // File names need more than half the files; exactly half is not enough.
        case("plain", "src", &["src/auth.ts", "src/b.ts"], &[]),
        case(
            "plain",
            "src",
            &["src/auth.ts", "src/jwt.ts", "src/b.ts"],
            &[],
        ),
        case(
            "plain",
            "src",
            &["src/service.ts", "src/service-one.ts"],
            &[],
        ),
        // Packages: the first row with one of them, in table order.
        case(
            "plain",
            "src",
            &["src/a.ts", "src/b.ts"],
            &["axios", "express"],
        ),
        case("plain", "src", &["src/a.ts", "src/b.ts"], &["lodash"]),
        // Frontend: at least 60 percent of the files are .tsx or .jsx.
        case(
            "plain",
            "src",
            &[
                "src/Button.tsx",
                "src/Card.tsx",
                "src/Nav.tsx",
                "src/util.ts",
            ],
            &[],
        ),
        case(
            "plain",
            "src",
            &[
                "src/a.tsx",
                "src/b.jsx",
                "src/c.tsx",
                "src/d.ts",
                "src/e.ts",
            ],
            &[],
        ),
        case(
            "plain",
            "src",
            &["src/a.tsx", "src/b.jsx", "src/c.ts", "src/d.ts", "src/e.ts"],
            &[],
        ),
        // The fallback, and backslash paths.
        case("plain", "src", &["src/x.ts"], &[]),
        case("", "", &["src/x.rs", "src/y.rs"], &[]),
        case("plain", "", &["src\\lib\\x.rs", "src\\lib\\y.rs"], &[]),
    ];
    for word in ROW_WORDS {
        let one = format!("src/{word}.ts");
        let two = format!("src/{word}-one.ts");
        let dir = format!("src/{word}");
        out.push(case(word, "", &["src/a.ts"], &[]));
        out.push(case("plain", &dir, &["src/a.ts"], &[]));
        out.push(case(
            "plain",
            "src",
            &[one.as_str(), two.as_str(), "src/zz.ts"],
            &[],
        ));
        out.push(case(
            "plain",
            "src",
            &[one.as_str(), "src/zz.ts", "src/yy.ts"],
            &[],
        ));
    }
    for package in ROW_PACKAGES {
        out.push(case("plain", "src", &["src/a.ts", "src/b.ts"], &[package]));
    }
    out
}

#[test]
fn classify_matches_acan_iso() {
    let cases = cases();
    assert!(cases.len() >= 40, "only {} cases", cases.len());
    let mut reasons = Vec::new();
    for case in &cases {
        let paths: Vec<&str> = case.paths.iter().map(String::as_str).collect();
        let ours = classify(&case.name, &case.dir, &paths, &case.packages);
        let theirs = acan_iso::classify(&case.name, &case.dir, &paths, &case.packages);
        assert_eq!(
            (wire(ours.kind), ours.guessed, ours.reason.clone()),
            (wire(theirs.kind), theirs.guessed, theirs.reason.clone()),
            "name {:?}, dir {:?}, paths {:?}, packages {:?}",
            case.name,
            case.dir,
            paths,
            case.packages,
        );
        reasons.push(ours.reason);
    }
    let missing: Vec<&str> = FAMILIES
        .into_iter()
        .filter(|family| !reasons.iter().any(|reason| reason.starts_with(*family)))
        .collect();
    assert!(missing.is_empty(), "no case reaches: {missing:?}");
}
