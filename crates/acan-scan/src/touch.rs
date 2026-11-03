//! Which state-machine transitions a change touches.
//!
//! A claim's footprint and a commit's diff both name lines of source. [`touched`] maps either one
//! to the transitions of the plant's state machines that it covers. This is the only copy of the
//! mapping: the server's live touch frames use it, and a UI port should use it too.

use crate::model::{LineRange, SourceFile, StateMachine, Transition};

/// Every line of a file, as a range. Each transition of the file overlaps it.
const WHOLE_FILE: LineRange = LineRange {
    start: 0,
    end: u32::MAX,
};

/// What changed, in the terms its caller has.
#[derive(Debug, Clone, Copy)]
pub enum Change<'a> {
    /// A claim's footprint. Interfaces are passed as `symbols`, and inserts as `files`.
    Claim {
        /// Repo-relative files the claim may rewrite.
        files: &'a [&'a str],
        /// `(file, name)` pairs of the symbols the claim may change.
        symbols: &'a [(&'a str, &'a str)],
    },
    /// A commit's unified diff against its first parent, produced with `--unified=0 --no-renames`.
    Commit {
        /// The diff text, as git prints it.
        diff: &'a str,
    },
}

/// One transition that a change touches, with the machine that owns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Touched<'a> {
    /// The state machine the transition belongs to.
    pub machine: &'a StateMachine,
    /// The transition the change touches.
    pub transition: &'a Transition,
}

/// Returns each transition of each state machine in `machines` that `change` touches. The result
/// follows the order of `machines`, then of each machine's `transitions`.
///
/// `files` is the plant's file list. It resolves a claimed symbol to its line range.
///
/// The rules:
///
/// - A claimed file covers every line of that file.
/// - A claimed symbol covers the line range of the plant symbol with that file and name. When the
///   file holds several symbols with that name, each covers its own range. A symbol that the plant
///   does not hold covers nothing.
/// - A claimed insert is a claimed file, so it covers every line of that file, because the agent
///   may edit near existing transitions. An interface is a claimed symbol.
/// - A commit covers the lines that [`hunks`] returns for its diff.
/// - A transition is touched when its `file` equals a covered file and its `line..=end_line`
///   overlaps a covered range. The overlap is inclusive at both ends, so a range that ends on the
///   transition's first line touches it.
/// - Every machine in `machines` is considered, and machines are not filtered by module. A
///   transition's `from` state does not matter: `"*"` matches the same way as a known state.
#[must_use]
pub fn touched<'a>(
    machines: &'a [StateMachine],
    files: &[SourceFile],
    change: Change<'_>,
) -> Vec<Touched<'a>> {
    let covered = match change {
        Change::Claim {
            files: claimed,
            symbols,
        } => claimed_ranges(files, claimed, symbols),
        Change::Commit { diff } => hunks(diff),
    };
    let mut out = Vec::new();
    for machine in machines {
        for transition in &machine.transitions {
            let hit = covered
                .iter()
                .any(|(file, range)| *file == transition.file && overlaps(*range, transition));
            if hit {
                out.push(Touched {
                    machine,
                    transition,
                });
            }
        }
    }
    out
}

/// The lines of each file that a unified diff changes, as `(file, lines)` pairs in diff order.
///
/// The file of a hunk is the path in its `+++ b/<path>` header. For a file the diff deletes, which
/// has `+++ /dev/null`, the file is the path in `--- a/<path>`. Each hunk `@@ -a,b +c,d @@` covers
/// lines `c` through `c + d - 1` of the new side. A pure deletion (`d = 0`) at position `c` covers
/// lines `c` and `c + 1`, conservatively. A deleted file covers every line, because each of its
/// transitions is gone.
///
/// A hunk body is read by the counts in its header, so a removed line such as `-- old` is not taken
/// for a file header.
#[must_use]
pub fn hunks(diff: &str) -> Vec<(String, LineRange)> {
    let mut out = Vec::new();
    let mut old_path: Option<String> = None;
    let mut file: Option<String> = None;
    let mut deleted = false;
    // The lines the current hunk still owes on each side.
    let mut old_left = 0_u64;
    let mut new_left = 0_u64;
    for line in diff.lines() {
        if old_left > 0 || new_left > 0 {
            if line.starts_with('-') && old_left > 0 {
                old_left -= 1;
                continue;
            }
            if line.starts_with('+') && new_left > 0 {
                new_left -= 1;
                continue;
            }
            if line.starts_with('\\') {
                continue;
            }
            // The hunk ended early, so the line is read as a header below.
            old_left = 0;
            new_left = 0;
        }
        if line.starts_with("diff ") {
            old_path = None;
            file = None;
            deleted = false;
        } else if let Some(rest) = line.strip_prefix("--- ") {
            old_path = header_path(rest, "a/");
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            if let Some(path) = header_path(rest, "b/") {
                file = Some(path);
                deleted = false;
            } else {
                file = old_path.clone();
                deleted = true;
            }
        } else if let Some(rest) = line.strip_prefix("@@ ") {
            let Some(((_, old_count), (new_start, new_count))) = hunk_sides(rest) else {
                continue;
            };
            old_left = old_count;
            new_left = new_count;
            if let Some(path) = &file {
                let range = if deleted {
                    WHOLE_FILE
                } else {
                    new_lines(new_start, new_count)
                };
                out.push((path.clone(), range));
            }
        }
    }
    out
}

/// The ranges a claim covers. A claimed file covers every line, and a claimed symbol covers the
/// range of each plant symbol with its file and name.
fn claimed_ranges(
    plant_files: &[SourceFile],
    files: &[&str],
    symbols: &[(&str, &str)],
) -> Vec<(String, LineRange)> {
    let mut out: Vec<(String, LineRange)> = files
        .iter()
        .map(|file| ((*file).to_owned(), WHOLE_FILE))
        .collect();
    for (file, name) in symbols {
        for source in plant_files.iter().filter(|source| source.path == *file) {
            for symbol in source.symbols.iter().filter(|symbol| symbol.name == *name) {
                out.push(((*file).to_owned(), symbol.line_range));
            }
        }
    }
    out
}

/// Whether the transition's lines overlap `range`. Both ends are inclusive.
fn overlaps(range: LineRange, transition: &Transition) -> bool {
    let last = transition.end_line.max(transition.line);
    transition.line <= range.end && range.start <= last
}

/// The new-side lines a hunk covers: `count` lines from `start`, or the two lines around a pure
/// deletion when `count` is zero.
fn new_lines(start: u64, count: u64) -> LineRange {
    let end = if count == 0 {
        start.saturating_add(1)
    } else {
        start.saturating_add(count - 1)
    };
    LineRange {
        start: line_number(start),
        end: line_number(end),
    }
}

fn line_number(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The path a `---` or `+++` header names, without its `a/` or `b/` prefix. `None` for `/dev/null`.
fn header_path(raw: &str, prefix: &str) -> Option<String> {
    // Git puts a tab after a name that contains a space.
    let raw = raw.strip_suffix('\t').unwrap_or(raw);
    if raw == "/dev/null" {
        return None;
    }
    let path = if raw.starts_with('"') {
        unquote(raw)?
    } else {
        raw.to_owned()
    };
    let stripped = path.strip_prefix(prefix).map(str::to_owned);
    Some(stripped.unwrap_or(path))
}

/// Decodes a C-quoted path, which git prints for a name with a quote, a backslash or a control
/// character.
fn unquote(raw: &str) -> Option<String> {
    let inner = raw.strip_prefix('"')?.strip_suffix('"')?;
    let bytes = inner.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        let escaped = *bytes.get(i + 1)?;
        i += 2;
        let byte = match escaped {
            b'a' => 0x07,
            b'b' => 0x08,
            b'f' => 0x0c,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'v' => 0x0b,
            b'"' => b'"',
            b'\\' => b'\\',
            b'0'..=b'7' => {
                // An octal escape has up to three digits.
                let mut value = u32::from(escaped - b'0');
                for _ in 0..2 {
                    match bytes.get(i).copied() {
                        Some(digit @ b'0'..=b'7') => {
                            value = value * 8 + u32::from(digit - b'0');
                            i += 1;
                        }
                        _ => break,
                    }
                }
                u8::try_from(value).ok()?
            }
            _ => return None,
        };
        out.push(byte);
    }
    String::from_utf8(out).ok()
}

/// The old and new `(start, count)` of a hunk header, given the text after its `@@ `.
fn hunk_sides(header: &str) -> Option<((u64, u64), (u64, u64))> {
    let mut parts = header.split_whitespace();
    let old = side_range(parts.next()?.strip_prefix('-')?)?;
    let new = side_range(parts.next()?.strip_prefix('+')?)?;
    Some((old, new))
}

/// `start,count` or `start`, where a missing count is one line.
fn side_range(spec: &str) -> Option<(u64, u64)> {
    match spec.split_once(',') {
        Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
        None => Some((spec.parse().ok()?, 1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::ContentHash;
    use crate::lang::Lang;
    use crate::model::{Confidence, FileId, Source, Symbol, SymbolKind};

    fn transition(file: &str, line: u32, end_line: u32) -> Transition {
        Transition {
            from: "open".to_owned(),
            to: "paid".to_owned(),
            file: file.to_owned(),
            line,
            end_line,
            via: None,
            source: Source::Scan,
            label: None,
        }
    }

    fn machine(transitions: Vec<Transition>) -> StateMachine {
        StateMachine {
            id: "a.ts#Status".to_owned(),
            name: "Status".to_owned(),
            file: "a.ts".to_owned(),
            line: 1,
            module: "m".to_owned(),
            states: Vec::new(),
            transitions,
            confidence: Confidence::High,
            derived: false,
        }
    }

    /// A plant file with the named symbols, each as `(name, start, end)`.
    fn source(path: &str, symbols: &[(&str, u32, u32)]) -> SourceFile {
        SourceFile {
            id: FileId(0),
            path: path.to_owned(),
            lang: Lang::TypeScript,
            content_hash: ContentHash::of(b""),
            symbols: symbols
                .iter()
                .map(|&(name, start, end)| Symbol {
                    name: name.to_owned(),
                    kind: SymbolKind::Function,
                    byte_range: 0..0,
                    line_range: LineRange { start, end },
                    exported: true,
                })
                .collect(),
            imports: Vec::new(),
        }
    }

    /// The `(line, end_line)` of each touched transition, in order.
    fn hits(touches: &[Touched<'_>]) -> Vec<(u32, u32)> {
        touches
            .iter()
            .map(|touch| (touch.transition.line, touch.transition.end_line))
            .collect()
    }

    fn claim<'a>(files: &'a [&'a str], symbols: &'a [(&'a str, &'a str)]) -> Change<'a> {
        Change::Claim { files, symbols }
    }

    fn commit(diff: &str) -> Change<'_> {
        Change::Commit { diff }
    }

    #[test]
    fn a_claimed_file_touches_every_transition_in_it() {
        let machines = [machine(vec![
            transition("a.ts", 3, 3),
            transition("a.ts", 40, 41),
            transition("b.ts", 3, 3),
        ])];
        let files = [source("a.ts", &[])];

        let touches = touched(&machines, &files, claim(&["a.ts"], &[]));
        assert_eq!(hits(&touches), vec![(3, 3), (40, 41)]);

        let elsewhere = touched(&machines, &files, claim(&["c.ts"], &[]));
        assert!(elsewhere.is_empty(), "no transition is in c.ts");
    }

    #[test]
    fn a_claimed_symbol_touches_only_the_transitions_in_its_range() {
        let machines = [machine(vec![
            transition("a.ts", 3, 3),
            transition("a.ts", 25, 26),
        ])];
        let files = [source("a.ts", &[("load", 1, 5), ("save", 20, 30)])];

        let touches = touched(&machines, &files, claim(&[], &[("a.ts", "load")]));

        assert_eq!(
            hits(&touches),
            vec![(3, 3)],
            "save's transition is outside load"
        );
    }

    #[test]
    fn a_claimed_symbol_covers_the_range_of_each_symbol_with_its_name() {
        let machines = [machine(vec![
            transition("a.ts", 3, 3),
            transition("a.ts", 42, 42),
            transition("a.ts", 20, 20),
        ])];
        let files = [source(
            "a.ts",
            &[("load", 1, 5), ("load", 40, 44), ("save", 20, 20)],
        )];

        let touches = touched(&machines, &files, claim(&[], &[("a.ts", "load")]));

        assert_eq!(hits(&touches), vec![(3, 3), (42, 42)]);
    }

    #[test]
    fn a_symbol_the_plant_lacks_touches_nothing() {
        let machines = [machine(vec![transition("a.ts", 3, 3)])];
        let files = [source("a.ts", &[("load", 1, 5)])];

        let missing_name = touched(&machines, &files, claim(&[], &[("a.ts", "gone")]));
        let missing_file = touched(&machines, &files, claim(&[], &[("b.ts", "load")]));

        assert!(missing_name.is_empty());
        assert!(missing_file.is_empty());
    }

    #[test]
    fn an_interface_is_a_symbol_and_an_insert_is_a_whole_file() {
        let machines = [machine(vec![
            transition("a.ts", 3, 3),
            transition("routes.ts", 500, 500),
        ])];
        let files = [source("a.ts", &[("load", 1, 5)]), source("routes.ts", &[])];

        // An insert is passed as a file: the whole of routes.ts is covered.
        let insert = touched(&machines, &files, claim(&["routes.ts"], &[]));
        assert_eq!(hits(&insert), vec![(500, 500)]);

        // An interface is passed as a symbol: only load's range is covered.
        let interface = touched(&machines, &files, claim(&[], &[("a.ts", "load")]));
        assert_eq!(hits(&interface), vec![(3, 3)]);
    }

    /// A diff with the header git prints for `a.ts`, and the given hunks.
    fn diff_of(path: &str, hunks: &str) -> String {
        format!(
            "diff --git a/{path} b/{path}\nindex 1111111..2222222 100644\n--- a/{path}\n+++ b/{path}\n{hunks}"
        )
    }

    #[test]
    fn hunks_read_added_modified_and_pure_deletion_hunks() {
        let diff = diff_of(
            "a.ts",
            "@@ -3,0 +4,2 @@\n+x\n+y\n@@ -10 +12 @@\n-old\n+new\n@@ -20,2 +21,0 @@\n-gone\n-gone too\n",
        );

        assert_eq!(
            hunks(&diff),
            vec![
                ("a.ts".to_owned(), LineRange { start: 4, end: 5 }),
                ("a.ts".to_owned(), LineRange { start: 12, end: 12 }),
                ("a.ts".to_owned(), LineRange { start: 21, end: 22 }),
            ]
        );
    }

    #[test]
    fn hunks_of_a_new_file_and_a_deleted_file() {
        let diff = "diff --git a/new.ts b/new.ts\nnew file mode 100644\n--- /dev/null\n+++ b/new.ts\n@@ -0,0 +1,3 @@\n+1\n+2\n+3\ndiff --git a/gone.ts b/gone.ts\ndeleted file mode 100644\n--- a/gone.ts\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-a\n-b\n";

        assert_eq!(
            hunks(diff),
            vec![
                ("new.ts".to_owned(), LineRange { start: 1, end: 3 }),
                ("gone.ts".to_owned(), WHOLE_FILE),
            ]
        );
    }

    #[test]
    fn a_removed_line_that_looks_like_a_header_is_content() {
        // The removed line `-- old` prints as `--- old`, and the added `++ new` as `+++ new`.
        let diff = diff_of("a.ts", "@@ -1,2 +1,1 @@\n--- old\n-x\n+++ new\n");

        assert_eq!(
            hunks(&diff),
            vec![("a.ts".to_owned(), LineRange { start: 1, end: 1 })]
        );
    }

    #[test]
    fn a_quoted_path_is_unquoted() {
        let diff = "--- \"a/we\\\"ird.ts\"\n+++ \"b/we\\\"ird.ts\"\n@@ -1 +1 @@\n-a\n+b\n";

        assert_eq!(
            hunks(diff),
            vec![("we\"ird.ts".to_owned(), LineRange { start: 1, end: 1 })]
        );
    }

    #[test]
    fn a_hunk_outside_every_transition_misses() {
        let machines = [machine(vec![transition("a.ts", 10, 12)])];

        let touches = touched(
            &machines,
            &[],
            commit(&diff_of("a.ts", "@@ -50 +50 @@\n-a\n+b\n")),
        );

        assert!(touches.is_empty());
    }

    #[test]
    fn a_transition_in_another_file_misses() {
        let machines = [machine(vec![transition("a.ts", 10, 12)])];

        let touches = touched(
            &machines,
            &[],
            commit(&diff_of("b.ts", "@@ -10 +10 @@\n-a\n+b\n")),
        );

        assert!(touches.is_empty());
    }

    #[test]
    fn a_hunk_that_ends_on_the_first_line_touches_the_transition() {
        let machines = [machine(vec![transition("a.ts", 10, 12)])];

        // Covers 9..=10: ends on the transition's first line.
        let ends_on_first = touched(
            &machines,
            &[],
            commit(&diff_of("a.ts", "@@ -8,0 +9,2 @@\n+x\n+y\n")),
        );
        assert_eq!(hits(&ends_on_first), vec![(10, 12)]);

        // Covers 6..=8: ends one line before the transition.
        let ends_before = touched(
            &machines,
            &[],
            commit(&diff_of("a.ts", "@@ -5,0 +6,3 @@\n+x\n+y\n+z\n")),
        );
        assert!(ends_before.is_empty());

        // A modified line 12 is the transition's last line.
        let last = touched(
            &machines,
            &[],
            commit(&diff_of("a.ts", "@@ -11 +12 @@\n-a\n+b\n")),
        );
        assert_eq!(hits(&last), vec![(10, 12)]);
    }

    #[test]
    fn a_pure_deletion_covers_the_lines_around_its_position() {
        let machines = [machine(vec![transition("a.ts", 7, 9)])];

        // `+6,0` deletes between lines 6 and 7, so it covers 6 and 7.
        let touches = touched(
            &machines,
            &[],
            commit(&diff_of("a.ts", "@@ -7,2 +6,0 @@\n-a\n-b\n")),
        );

        assert_eq!(hits(&touches), vec![(7, 9)]);
    }

    #[test]
    fn touches_follow_the_order_of_machines_then_transitions() {
        let mut second = machine(vec![transition("a.ts", 2, 2)]);
        second.id = "a.ts#Other".to_owned();
        let machines = [
            machine(vec![transition("a.ts", 9, 9), transition("a.ts", 4, 4)]),
            second,
        ];

        let touches = touched(&machines, &[], claim(&["a.ts"], &[]));

        let order: Vec<(&str, u32)> = touches
            .iter()
            .map(|touch| (touch.machine.id.as_str(), touch.transition.line))
            .collect();
        assert_eq!(
            order,
            vec![("a.ts#Status", 9), ("a.ts#Status", 4), ("a.ts#Other", 2)]
        );
    }

    #[test]
    fn a_scanned_temp_repo_maps_a_claim_and_a_commit_to_transitions() {
        let dir = tempfile::tempdir().expect("temp dir");
        let source = "export type OrderStatus = 'open' | 'paid' | 'shipped';\n\nexport function settle(order: { status: OrderStatus }) {\n  switch (order.status) {\n    case 'open':\n      order.status = 'paid';\n      break;\n    case 'paid':\n      order.status = 'shipped';\n      break;\n  }\n}\n";
        std::fs::write(dir.path().join("order.ts"), source).expect("write order.ts");
        let plant = crate::scan(dir.path()).expect("scan the temp repo");

        let settle = plant
            .files
            .iter()
            .find(|file| file.path == "order.ts")
            .and_then(|file| file.symbols.iter().find(|symbol| symbol.name == "settle"))
            .expect("settle is a symbol")
            .line_range;
        let expected: Vec<(u32, u32)> = plant
            .state_machines
            .iter()
            .flat_map(|machine| &machine.transitions)
            .filter(|transition| {
                transition.file == "order.ts"
                    && transition.line >= settle.start
                    && transition.end_line <= settle.end
            })
            .map(|transition| (transition.line, transition.end_line))
            .collect();
        assert!(!expected.is_empty(), "settle holds transitions");

        let by_claim = touched(
            &plant.state_machines,
            &plant.files,
            claim(&[], &[("order.ts", "settle")]),
        );
        assert_eq!(hits(&by_claim), expected);

        // A hunk on the first transition line touches it and nothing else.
        let first = expected[0].0;
        let diff = diff_of("order.ts", &format!("@@ -{first} +{first} @@\n-a\n+b\n"));
        let by_commit = touched(&plant.state_machines, &plant.files, commit(&diff));
        assert_eq!(hits(&by_commit), vec![expected[0]]);
    }
}
