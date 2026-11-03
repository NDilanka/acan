//! State machines the scanner can read from the source: a status-like type (`OrderState`,
//! `TaskStatus`, ...), its states, and the moves between them that the code performs.
//!
//! Each file yields facts while its tree is parsed: the status-like declarations and the sites
//! that assign or match a state. [`resolve`] joins them across files. Nothing here guesses. A site
//! that names no declared type, or a target that is not one of the machine's states, is dropped.
//! A source state the code does not show is kept as `*`, which lowers the machine's confidence.
//! Attachment never crosses a language family: Rust declarations take Rust sites only, and
//! TypeScript declarations take TypeScript, TSX, and JavaScript sites.

use std::collections::BTreeMap;

use tree_sitter::Node;

use crate::extract::{line_number, text};
use crate::lang::Lang;
use crate::model::{Confidence, State, StateMachine, Transition};

/// Name suffixes that make a declared type status-like.
const STATUS_SUFFIXES: [&str; 6] = ["State", "Status", "Phase", "Stage", "Mode", "Verdict"];
/// The source state of a transition the code does not show.
const UNKNOWN_FROM: &str = "*";
/// The fewest states a declaration needs to count as a state machine.
const MIN_STATES: usize = 2;

/// What one file contributes. Built while the file's tree is parsed.
#[derive(Debug, Default)]
pub(crate) struct FileStates {
    pub(crate) declarations: Vec<Declaration>,
    pub(crate) sites: Vec<Site>,
}

/// The language family of a file. A declaration and a site attach only within one family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Family {
    Rust,
    /// TypeScript, TSX, and JavaScript.
    Ts,
}

/// A status-like type declared in a file, with its states in declaration order.
#[derive(Debug)]
pub(crate) struct Declaration {
    pub(crate) name: String,
    pub(crate) file: String,
    pub(crate) family: Family,
    /// The line of the type's name.
    pub(crate) line: u32,
    /// Each state's name and line.
    pub(crate) states: Vec<(String, u32)>,
    /// For a string-valued enum, each member's string value mapped to the member's name. A value
    /// that two members share maps to `None`, so no literal names either of them. Empty for every
    /// other declaration, where a string literal is the state's own name.
    pub(crate) values: BTreeMap<String, Option<String>>,
}

/// A state a site names, before it is checked against a declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StateRef {
    /// A state by its name: `OrderState::Paid`, or `OrderStatus.Paid`.
    Named(String),
    /// A string the code writes, such as `'paid'`. A string-valued enum maps it to a member.
    Literal(String),
}

/// One place the code moves a value to a state: an assignment, or a match arm.
#[derive(Debug)]
pub(crate) struct Site {
    /// The language family of the file the site is in.
    pub(crate) family: Family,
    /// The type the target state belongs to, when the code names it (`OrderState::Split`).
    pub(crate) enum_name: Option<String>,
    /// The property assigned, for TypeScript member assignments (`order.status = 'paid'`).
    pub(crate) property: Option<String>,
    /// The target state.
    pub(crate) to: StateRef,
    /// The source state, when the code shows it. `None` becomes `*`.
    pub(crate) from: Option<StateRef>,
    pub(crate) file: String,
    /// 1-based first line of the assignment or match arm.
    pub(crate) line: u32,
    /// 1-based last line, inclusive.
    pub(crate) end_line: u32,
    /// The enclosing function's name.
    pub(crate) via: Option<String>,
}

/// Collects the facts one parsed file holds. Rust files give enum declarations, assignment sites,
/// and match-arm sites. TypeScript and TSX files also give union and enum declarations. JavaScript
/// files give sites only.
pub(crate) fn extract(lang: Lang, file: &str, root: Node, source: &[u8]) -> FileStates {
    let mut out = FileStates::default();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        match lang {
            Lang::Rust => rust_node(node, file, source, &mut out),
            Lang::TypeScript | Lang::Tsx => ts_node(node, file, source, true, &mut out),
            Lang::JavaScript => ts_node(node, file, source, false, &mut out),
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    out
}

/// Joins the facts of every file into state machines, sorted by file, line, and name. The module of
/// each machine is left empty for the caller to fill after grouping.
///
/// A declaration is dropped when another declaration in its file has the same name, because the two
/// share the id `{file}#{name}` and cannot be told apart. Both are dropped, so no site attaches to
/// either.
pub(crate) fn resolve(files: Vec<FileStates>) -> Vec<StateMachine> {
    let mut declarations = Vec::new();
    let mut sites = Vec::new();
    for file in files {
        declarations.extend(file.declarations);
        sites.extend(file.sites);
    }
    drop_repeated_ids(&mut declarations);

    let mut by_name: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    let mut by_literal: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (index, declaration) in declarations.iter().enumerate() {
        by_name
            .entry(declaration.name.as_str())
            .or_default()
            .push(index);
        for literal in declaration.literal_keys() {
            by_literal.entry(literal).or_default().push(index);
        }
    }

    let mut found: Vec<Vec<Transition>> = vec![Vec::new(); declarations.len()];
    for site in &sites {
        let Some(index) = attach(site, &declarations, &by_name, &by_literal) else {
            continue;
        };
        let declaration = &declarations[index];
        let Some(to) = declaration.state_of(&site.to) else {
            continue;
        };
        let from = site
            .from
            .as_ref()
            .and_then(|from| declaration.state_of(from))
            .unwrap_or_else(|| UNKNOWN_FROM.to_string());
        if from == to {
            continue;
        }
        found[index].push(Transition {
            from,
            to,
            file: site.file.clone(),
            line: site.line,
            end_line: site.end_line,
            via: site.via.clone(),
        });
    }

    let mut machines: Vec<StateMachine> = declarations
        .into_iter()
        .zip(found)
        .filter_map(|(declaration, mut transitions)| {
            if transitions.is_empty() {
                return None;
            }
            transitions.sort_by(|a, b| {
                (&a.file, a.line, a.end_line, &a.from, &a.to, &a.via)
                    .cmp(&(&b.file, b.line, b.end_line, &b.from, &b.to, &b.via))
            });
            transitions.dedup();
            let confidence = if transitions.iter().all(|t| t.from != UNKNOWN_FROM) {
                Confidence::High
            } else {
                Confidence::Low
            };
            Some(StateMachine {
                id: format!("{}#{}", declaration.file, declaration.name),
                name: declaration.name,
                file: declaration.file,
                line: declaration.line,
                module: String::new(),
                states: declaration
                    .states
                    .into_iter()
                    .map(|(name, line)| State { name, line })
                    .collect(),
                transitions,
                confidence,
            })
        })
        .collect();
    machines.sort_by(|a, b| (&a.file, a.line, &a.name).cmp(&(&b.file, b.line, &b.name)));
    machines
}

/// Drops every declaration whose file and name another declaration of the same file shares.
fn drop_repeated_ids(declarations: &mut Vec<Declaration>) {
    let mut counts: BTreeMap<(String, String), usize> = BTreeMap::new();
    for declaration in declarations.iter() {
        *counts
            .entry((declaration.file.clone(), declaration.name.clone()))
            .or_insert(0) += 1;
    }
    declarations
        .retain(|declaration| counts[&(declaration.file.clone(), declaration.name.clone())] == 1);
}

impl Declaration {
    fn has_state(&self, state: &str) -> bool {
        self.states.iter().any(|(name, _)| name == state)
    }

    /// The strings that can name a state here: the string values of a string-valued enum, or else
    /// the state names.
    fn literal_keys(&self) -> Vec<&str> {
        if self.values.is_empty() {
            self.states.iter().map(|(name, _)| name.as_str()).collect()
        } else {
            self.values.keys().map(String::as_str).collect()
        }
    }

    /// The state a value names here, when it is one of this declaration's states. In a
    /// string-valued enum a literal names the member with that string value, and a literal that is
    /// no value names nothing. In any other declaration a literal is the state's own name.
    fn state_of(&self, value: &StateRef) -> Option<String> {
        let name = match value {
            StateRef::Named(name) => name.as_str(),
            StateRef::Literal(literal) if self.values.is_empty() => literal.as_str(),
            StateRef::Literal(literal) => self.values.get(literal)?.as_deref()?,
        };
        self.has_state(name).then(|| name.to_string())
    }
}

/// The declaration a site's target belongs to. Only declarations of the site's family count. A site
/// with a type name attaches by name: a single declaration of that name wins, and when several
/// files declare it, the one in the site's own file wins. A site without a type name (a TypeScript
/// literal) attaches only when exactly one declaration of its family has the literal as a state
/// or string value and a name containing the assigned property.
fn attach(
    site: &Site,
    declarations: &[Declaration],
    by_name: &BTreeMap<&str, Vec<usize>>,
    by_literal: &BTreeMap<&str, Vec<usize>>,
) -> Option<usize> {
    match &site.enum_name {
        Some(name) => {
            let candidates: Vec<usize> = by_name
                .get(name.as_str())?
                .iter()
                .copied()
                .filter(|&index| declarations[index].family == site.family)
                .collect();
            if let [only] = candidates.as_slice() {
                return Some(*only);
            }
            let own: Vec<usize> = candidates
                .into_iter()
                .filter(|&index| declarations[index].file == site.file)
                .collect();
            match own.as_slice() {
                [only] => Some(*only),
                _ => None,
            }
        }
        None => {
            let StateRef::Literal(literal) = &site.to else {
                return None;
            };
            let property = site.property.as_deref()?.to_lowercase();
            let mut candidates = by_literal.get(literal.as_str())?.clone();
            candidates.retain(|&index| {
                let declaration = &declarations[index];
                declaration.family == site.family
                    && declaration.name.to_lowercase().contains(property.as_str())
                    && declaration.state_of(&site.to).is_some()
            });
            candidates.sort_unstable();
            candidates.dedup();
            match candidates.as_slice() {
                [only] => Some(*only),
                _ => None,
            }
        }
    }
}

// Rust

fn rust_node(node: Node, file: &str, source: &[u8], out: &mut FileStates) {
    match node.kind() {
        "enum_item" => out
            .declarations
            .extend(rust_declaration(node, file, source)),
        "assignment_expression" => out.sites.extend(rust_assignment(node, file, source)),
        "match_arm" => out.sites.extend(rust_arm(node, file, source)),
        _ => {}
    }
}

/// A status-like enum with at least two variants. Each variant is a state.
fn rust_declaration(node: Node, file: &str, source: &[u8]) -> Option<Declaration> {
    let name = node.child_by_field_name("name")?;
    let declared = rust_name(name, source);
    if !is_status_name(declared) {
        return None;
    }
    let body = node.child_by_field_name("body")?;
    let mut states = Vec::new();
    let mut cursor = body.walk();
    for variant in body.named_children(&mut cursor) {
        if variant.kind() != "enum_variant" {
            continue;
        }
        let Some(variant_name) = variant.child_by_field_name("name") else {
            continue;
        };
        states.push((
            rust_name(variant_name, source).to_string(),
            line_number(variant_name.start_position().row),
        ));
    }
    declaration(Family::Rust, declared, file, name, states, BTreeMap::new())
}

/// `target = Enum::Variant`, where the target is any place. The source state comes from the match
/// arm or the `if` that tests the same place, when the code shows one.
fn rust_assignment(node: Node, file: &str, source: &[u8]) -> Option<Site> {
    let (enum_name, to) = rust_variant(node.child_by_field_name("right")?, source)?;
    let subject = place_of(Family::Rust, node.child_by_field_name("left")?, source);
    let from = rust_from(node, &subject, &enum_name, source);
    Some(Site {
        family: Family::Rust,
        enum_name: Some(enum_name),
        property: None,
        to: StateRef::Named(to),
        from,
        file: file.to_string(),
        line: line_number(node.start_position().row),
        end_line: line_number(node.end_position().row),
        via: rust_via(node, source),
    })
}

/// A match arm whose pattern and value both name variants of one enum: `Enum::A => Enum::B`.
fn rust_arm(node: Node, file: &str, source: &[u8]) -> Option<Site> {
    let (from_enum, from) = rust_variant(pattern_of(node)?, source)?;
    let (to_enum, to) = rust_variant(node.child_by_field_name("value")?, source)?;
    if from_enum != to_enum {
        return None;
    }
    Some(Site {
        family: Family::Rust,
        enum_name: Some(to_enum),
        property: None,
        to: StateRef::Named(to),
        from: Some(StateRef::Named(from)),
        file: file.to_string(),
        line: line_number(node.start_position().row),
        end_line: line_number(node.end_position().row),
        via: rust_via(node, source),
    })
}

/// The `(enum, variant)` an expression or pattern names: `OrderState::Split`, `Self::Split`,
/// `Stage::Gating { .. }`, `Stage::Declared(x)`. Anything else gives `None`.
fn rust_variant(node: Node, source: &[u8]) -> Option<(String, String)> {
    match node.kind() {
        "scoped_identifier" | "scoped_type_identifier" => rust_qualified(node, source),
        "struct_expression" => rust_qualified(node.child_by_field_name("name")?, source),
        "struct_pattern" | "tuple_struct_pattern" => {
            rust_qualified(node.child_by_field_name("type")?, source)
        }
        "call_expression" => rust_variant(node.child_by_field_name("function")?, source),
        _ => None,
    }
}

/// A qualified path `Owner::Name`. `Self` as the owner is the type of the enclosing `impl`.
fn rust_qualified(node: Node, source: &[u8]) -> Option<(String, String)> {
    if !matches!(node.kind(), "scoped_identifier" | "scoped_type_identifier") {
        return None;
    }
    let mut owner = path_tail(node.child_by_field_name("path")?, source)?;
    if owner == "Self" {
        owner = path_tail(enclosing_impl_type(node)?, source)?;
    }
    let variant = ident(node.child_by_field_name("name")?, source)?;
    Some((owner, variant))
}

/// The last name of a path: `OrderState` for `crate::order::OrderState`.
fn path_tail(node: Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "scoped_identifier" | "scoped_type_identifier" => {
            path_tail(node.child_by_field_name("name")?, source)
        }
        "generic_type" => path_tail(node.child_by_field_name("type")?, source),
        _ => ident(node, source),
    }
}

/// The name a Rust identifier spells. A raw identifier's `r#` prefix is not part of the name, so
/// `r#Match` is `Match`.
fn rust_name<'a>(node: Node, source: &'a [u8]) -> &'a str {
    let name = text(node, source);
    name.strip_prefix("r#").unwrap_or(name)
}

/// The text of a node that is a plain name, without a raw identifier's `r#` prefix.
fn ident(node: Node, source: &[u8]) -> Option<String> {
    let name = rust_name(node, source);
    (!name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_'))
        .then(|| name.to_string())
}

/// The type of the nearest enclosing `impl` block.
fn enclosing_impl_type(node: Node) -> Option<Node> {
    let mut current = node.parent();
    while let Some(ancestor) = current {
        if ancestor.kind() == "impl_item" {
            return ancestor.child_by_field_name("type");
        }
        current = ancestor.parent();
    }
    None
}

/// The pattern of a match arm, without its guard.
fn pattern_of(arm: Node) -> Option<Node> {
    arm.child_by_field_name("pattern")?.named_child(0)
}

/// The variant of `enum_name` a Rust expression or pattern names.
fn rust_state_of(node: Node, enum_name: &str, source: &[u8]) -> Option<String> {
    rust_variant(node, source)
        .filter(|(owner, _)| owner == enum_name)
        .map(|(_, state)| state)
}

/// The source state of a Rust assignment: the nearest enclosing match arm or `if` that pins the
/// assigned place to a variant of the same enum. The search stops at the enclosing function or
/// closure, and gives nothing when an earlier assignment on the same path changes the place.
fn rust_from(site: Node, subject: &str, enum_name: &str, source: &[u8]) -> Option<StateRef> {
    let mut below = site;
    let mut current = site.parent();
    while let Some(node) = current {
        let found = match node.kind() {
            "function_item" | "closure_expression" => return None,
            "match_arm" => rust_arm_from(node, subject, enum_name, source),
            "if_expression" if is_consequence(node, below) => {
                node.child_by_field_name("condition").and_then(|condition| {
                    pinned_state(
                        condition,
                        subject,
                        Family::Rust,
                        source,
                        &["=="],
                        &|value| rust_state_of(value, enum_name, source).map(StateRef::Named),
                    )
                })
            }
            _ => None,
        };
        if let Some(state) = found {
            return (!assigned_before(node, site, subject, Family::Rust, source)).then_some(state);
        }
        below = node;
        current = node.parent();
    }
    None
}

/// The variant a match arm pins its subject to, when the arm matches the subject.
fn rust_arm_from(arm: Node, subject: &str, enum_name: &str, source: &[u8]) -> Option<StateRef> {
    let matched = arm.parent()?.parent()?;
    if matched.kind() != "match_expression"
        || place_of(Family::Rust, matched.child_by_field_name("value")?, source) != subject
    {
        return None;
    }
    rust_state_of(pattern_of(arm)?, enum_name, source).map(StateRef::Named)
}

/// The enclosing function's name.
fn rust_via(node: Node, source: &[u8]) -> Option<String> {
    let mut current = node.parent();
    while let Some(ancestor) = current {
        if ancestor.kind() == "function_item" {
            return ancestor
                .child_by_field_name("name")
                .map(|name| text(name, source).to_string());
        }
        current = ancestor.parent();
    }
    None
}

// TypeScript, TSX, and JavaScript

fn ts_node(node: Node, file: &str, source: &[u8], declares: bool, out: &mut FileStates) {
    match node.kind() {
        "type_alias_declaration" if declares => {
            out.declarations.extend(ts_alias(node, file, source));
        }
        "enum_declaration" if declares => out.declarations.extend(ts_enum(node, file, source)),
        "assignment_expression" => out.sites.extend(ts_assignment(node, file, source)),
        _ => {}
    }
}

/// `type Name = 'a' | 'b' | ...`, where every member is a string literal.
fn ts_alias(node: Node, file: &str, source: &[u8]) -> Option<Declaration> {
    let name = node.child_by_field_name("name")?;
    let declared = text(name, source);
    if !is_status_name_ts(declared) {
        return None;
    }
    let mut states = Vec::new();
    if !union_literals(node.child_by_field_name("value")?, source, &mut states) {
        return None;
    }
    declaration(Family::Ts, declared, file, name, states, BTreeMap::new())
}

/// Collects the string members of a union of string literals, in source order. False when a member
/// is not one. A union nests to the left, one level per member, so the members are walked with an
/// explicit stack rather than by recursion.
fn union_literals(node: Node, source: &[u8], states: &mut Vec<(String, u32)>) -> bool {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        match current.kind() {
            "union_type" => {
                let mut cursor = current.walk();
                let members: Vec<Node> = current.named_children(&mut cursor).collect();
                // Pushed in reverse, so that the leftmost member is popped first.
                stack.extend(members.into_iter().rev());
            }
            "literal_type" => match current.named_child(0) {
                Some(literal) if literal.kind() == "string" => {
                    states.push((
                        quoted_value(literal, source),
                        line_number(literal.start_position().row),
                    ));
                }
                _ => return false,
            },
            _ => return false,
        }
    }
    true
}

/// `enum Name { A, B = 'b' }`. Each member is a state. A member with a string value also maps that
/// value to the member, so that a literal such as `'b'` names the member `B`.
fn ts_enum(node: Node, file: &str, source: &[u8]) -> Option<Declaration> {
    let name = node.child_by_field_name("name")?;
    let declared = text(name, source);
    if !is_status_name_ts(declared) {
        return None;
    }
    let body = node.child_by_field_name("body")?;
    let mut states = Vec::new();
    let mut values = BTreeMap::new();
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        let (member_name, value) = match member.kind() {
            "property_identifier" | "string" => (member, None),
            "enum_assignment" => {
                let Some(member_name) = member.child_by_field_name("name") else {
                    continue;
                };
                (member_name, member.child_by_field_name("value"))
            }
            _ => continue,
        };
        let state = if member_name.kind() == "string" {
            quoted_value(member_name, source)
        } else {
            text(member_name, source).to_string()
        };
        if let Some(value) = value.filter(|value| value.kind() == "string") {
            values
                .entry(quoted_value(value, source))
                .and_modify(|member: &mut Option<String>| *member = None)
                .or_insert_with(|| Some(state.clone()));
        }
        states.push((state, line_number(member_name.start_position().row)));
    }
    declaration(Family::Ts, declared, file, name, states, values)
}

/// `obj.prop = 'literal'`, or `obj.prop = Enum.Member`. The property is any member of any object,
/// so `order.status`, `this.order.status`, and `state.order.status` are all read.
fn ts_assignment(node: Node, file: &str, source: &[u8]) -> Option<Site> {
    let left = strip_parens(node.child_by_field_name("left")?);
    if left.kind() != "member_expression" {
        return None;
    }
    let property = text(left.child_by_field_name("property")?, source).to_string();
    let right = strip_parens(node.child_by_field_name("right")?);
    let (enum_name, to) = match right.kind() {
        "string" | "template_string" => (None, StateRef::Literal(quoted_value(right, source))),
        "member_expression" => {
            let object = right.child_by_field_name("object")?;
            if object.kind() != "identifier" {
                return None;
            }
            let member = right.child_by_field_name("property")?;
            (
                Some(text(object, source).to_string()),
                StateRef::Named(text(member, source).to_string()),
            )
        }
        _ => return None,
    };
    let subject = place_of(Family::Ts, left, source);
    let from = ts_from(node, &subject, source);
    Some(Site {
        family: Family::Ts,
        enum_name,
        property: Some(property),
        to,
        from,
        file: file.to_string(),
        line: line_number(node.start_position().row),
        end_line: line_number(node.end_position().row),
        via: ts_via(node, source),
    })
}

/// The source state of a TypeScript assignment: the nearest enclosing `switch` case or `if` branch
/// that pins the assigned place. The search stops at the enclosing function, and gives nothing when
/// an earlier assignment on the same path changes the place.
fn ts_from(site: Node, subject: &str, source: &[u8]) -> Option<StateRef> {
    let mut below = site;
    let mut current = site.parent();
    while let Some(node) = current {
        if is_function_kind(node.kind()) {
            return None;
        }
        let found = match node.kind() {
            "switch_case" => ts_case_from(node, subject, source),
            "if_statement" if is_consequence(node, below) => {
                node.child_by_field_name("condition").and_then(|condition| {
                    pinned_state(
                        condition,
                        subject,
                        Family::Ts,
                        source,
                        &["===", "=="],
                        &|value| ts_value(value, source),
                    )
                })
            }
            _ => None,
        };
        if let Some(state) = found {
            return (!assigned_before(node, site, subject, Family::Ts, source)).then_some(state);
        }
        below = node;
        current = node.parent();
    }
    None
}

/// The value of a `case` clause, when the clause is only entered by jumping to it and the switch
/// tests the same place.
fn ts_case_from(case: Node, subject: &str, source: &[u8]) -> Option<StateRef> {
    let body = case.parent()?;
    let switch = body.parent()?;
    if switch.kind() != "switch_statement"
        || place_of(Family::Ts, switch.child_by_field_name("value")?, source) != subject
    {
        return None;
    }
    if reached_by_fallthrough(case, body) {
        return None;
    }
    ts_value(case.child_by_field_name("value")?, source)
}

/// Whether the clause before `case` runs into it: that clause has no statements, or its statements
/// do not end by leaving the switch. Scans the clauses of the switch body for each site, so a very
/// large single function is quadratic in its number of sites. That is accepted for now.
fn reached_by_fallthrough(case: Node, body: Node) -> bool {
    let mut previous = None;
    let mut cursor = body.walk();
    for clause in body.named_children(&mut cursor) {
        if clause.id() == case.id() {
            break;
        }
        if matches!(clause.kind(), "switch_case" | "switch_default") {
            previous = Some(clause);
        }
    }
    let Some(previous) = previous else {
        return false;
    };
    !ends_switch(&statements_of(previous))
}

/// Whether a statement list ends by leaving the switch. A block ends the way its own last statement
/// does, so `{ x; break; }` leaves the switch. An empty list does not.
fn ends_switch(statements: &[Node]) -> bool {
    match statements.last() {
        None => false,
        Some(last) => match last.kind() {
            "break_statement" | "return_statement" | "throw_statement" | "continue_statement" => {
                true
            }
            "statement_block" => ends_switch(&statements_of(*last)),
            _ => false,
        },
    }
}

/// The statements of a switch clause or a block: its named children without comments. A clause's
/// case value is left out.
fn statements_of(node: Node) -> Vec<Node> {
    let value = node.child_by_field_name("value").map(|value| value.id());
    let mut cursor = node.walk();
    let statements: Vec<Node> = node
        .named_children(&mut cursor)
        .filter(|child| Some(child.id()) != value && !child.kind().contains("comment"))
        .collect();
    statements
}

/// The state a TypeScript value names: a string literal, or the member of an enum (`Status.Paid`
/// gives `Paid`).
fn ts_value(node: Node, source: &[u8]) -> Option<StateRef> {
    let node = strip_parens(node);
    match node.kind() {
        "string" | "template_string" => Some(StateRef::Literal(quoted_value(node, source))),
        "member_expression" => {
            let object = node.child_by_field_name("object")?;
            let property = node.child_by_field_name("property")?;
            (object.kind() == "identifier")
                .then(|| StateRef::Named(text(property, source).to_string()))
        }
        _ => None,
    }
}

/// The text of a string literal between one matching pair of outer quotes. Quote characters inside
/// the value stay, so `'"'` is `"`. `string_literal` in extract.rs trims every quote at both ends,
/// which would empty that value, so this helper removes exactly one pair.
fn quoted_value(node: Node, source: &[u8]) -> String {
    let raw = text(node, source);
    for quote in ['"', '\'', '`'] {
        if let Some(inner) = raw
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner.to_string();
        }
    }
    raw.to_string()
}

/// The name of the nearest enclosing named function: its own name, or the variable it initializes.
fn ts_via(node: Node, source: &[u8]) -> Option<String> {
    let mut current = node.parent();
    while let Some(ancestor) = current {
        if let Some(name) = ts_function_name(ancestor, source) {
            return Some(name);
        }
        current = ancestor.parent();
    }
    None
}

fn ts_function_name(node: Node, source: &[u8]) -> Option<String> {
    if !is_function_kind(node.kind()) {
        return None;
    }
    if let Some(name) = node.child_by_field_name("name") {
        return Some(text(name, source).to_string());
    }
    let parent = node.parent()?;
    if parent.kind() != "variable_declarator"
        || parent.child_by_field_name("value")?.id() != node.id()
    {
        return None;
    }
    let name = parent.child_by_field_name("name")?;
    (name.kind() == "identifier").then(|| text(name, source).to_string())
}

fn is_function_kind(kind: &str) -> bool {
    matches!(
        kind,
        "function_declaration"
            | "generator_function_declaration"
            | "function_expression"
            | "function"
            | "generator_function"
            | "arrow_function"
            | "method_definition"
            | "function_item"
            | "closure_expression"
    )
}

// Shared

/// A declaration with at least `MIN_STATES` states. `name_node` gives its line.
fn declaration(
    family: Family,
    name: &str,
    file: &str,
    name_node: Node,
    states: Vec<(String, u32)>,
    values: BTreeMap<String, Option<String>>,
) -> Option<Declaration> {
    (states.len() >= MIN_STATES).then(|| Declaration {
        name: name.to_string(),
        file: file.to_string(),
        family,
        line: line_number(name_node.start_position().row),
        states,
        values,
    })
}

/// Rust names are matched case-sensitively.
fn is_status_name(name: &str) -> bool {
    STATUS_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
}

/// TypeScript names are matched case-insensitively.
fn is_status_name_ts(name: &str) -> bool {
    let lower = name.to_lowercase();
    STATUS_SUFFIXES
        .iter()
        .any(|suffix| lower.ends_with(&suffix.to_lowercase()))
}

/// The expression inside any parentheses around `node`.
fn strip_parens(mut node: Node) -> Node {
    while node.kind() == "parenthesized_expression" {
        match node.named_child(0) {
            Some(inner) => node = inner,
            None => break,
        }
    }
    node
}

/// The text a place is compared by, so that spellings of one place match. Rust drops any `&`,
/// `&mut`, `*`, and parentheses around the place, so `*state`, `&mut state`, and `(state)` all
/// compare as `state`. TypeScript drops only the parentheses.
fn place_of(family: Family, node: Node, source: &[u8]) -> String {
    match family {
        Family::Rust => {
            let mut node = node;
            while let Some(inner) = rust_place_inside(node) {
                node = inner;
            }
            text(node, source).trim().to_string()
        }
        Family::Ts => text(strip_parens(node), source).trim().to_string(),
    }
}

/// The expression a Rust place wraps: the operand of `&`, `&mut`, or `*`, or the expression inside
/// parentheses.
fn rust_place_inside(node: Node) -> Option<Node> {
    match node.kind() {
        "parenthesized_expression" => node.named_child(0),
        "reference_expression" => node.child_by_field_name("value"),
        "unary_expression" if node.child(0).is_some_and(|first| first.kind() == "*") => {
            node.named_child(0)
        }
        _ => None,
    }
}

/// Whether `node` is the `consequence` of the `if` node.
fn is_consequence(node: Node, below: Node) -> bool {
    node.child_by_field_name("consequence")
        .is_some_and(|block| block.id() == below.id())
}

/// The state a condition pins `subject` to: `subject == value`, possibly joined by `&&` or a let
/// chain with other tests. An `||` or any other operator pins nothing.
fn pinned_state(
    condition: Node,
    subject: &str,
    family: Family,
    source: &[u8],
    equal: &[&str],
    value_of: &impl Fn(Node) -> Option<StateRef>,
) -> Option<StateRef> {
    let condition = strip_parens(condition);
    match condition.kind() {
        // `if let P = x && cond` is a let chain. Each part is tried in turn, as the sides of `&&` are.
        "let_chain" => {
            let mut cursor = condition.walk();
            let tests: Vec<Node> = condition.named_children(&mut cursor).collect();
            tests
                .into_iter()
                .find_map(|test| pinned_state(test, subject, family, source, equal, value_of))
        }
        "binary_expression" => {
            let operator = text(condition.child_by_field_name("operator")?, source);
            let left = condition.child_by_field_name("left")?;
            let right = condition.child_by_field_name("right")?;
            if operator == "&&" {
                return pinned_state(left, subject, family, source, equal, value_of)
                    .or_else(|| pinned_state(right, subject, family, source, equal, value_of));
            }
            if !equal.contains(&operator) {
                return None;
            }
            if place_of(family, left, source) == subject {
                value_of(right)
            } else if place_of(family, right, source) == subject {
                value_of(left)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Whether an assignment to `subject` runs before `site` on the path from `container` down to it.
/// Walks up from the site to the container and checks the earlier siblings at each level.
///
/// This scans the subtrees of those siblings on every level, so a very large single function is
/// quadratic in its number of sites. That is accepted for now.
fn assigned_before(
    container: Node,
    site: Node,
    subject: &str,
    family: Family,
    source: &[u8],
) -> bool {
    let mut below = site;
    while let Some(ancestor) = below.parent() {
        if earlier_assignment(ancestor, below, subject, family, source) {
            return true;
        }
        if ancestor.id() == container.id() {
            return false;
        }
        below = ancestor;
    }
    false
}

/// Whether a child of `ancestor` that comes before `on` assigns `subject`. The other arms of a
/// `match` and the other cases of a `switch` are alternatives to `on`, so they never count. Neither
/// does the branch of an `if` that `on` is not part of.
fn earlier_assignment(
    ancestor: Node,
    on: Node,
    subject: &str,
    family: Family,
    source: &[u8],
) -> bool {
    if matches!(ancestor.kind(), "match_block" | "switch_body") {
        return false;
    }
    let is_if = matches!(ancestor.kind(), "if_expression" | "if_statement");
    let mut cursor = ancestor.walk();
    for child in ancestor.children(&mut cursor) {
        if child.id() == on.id() {
            break;
        }
        if is_if && is_branch(ancestor, child) {
            continue;
        }
        if contains_assignment(child, subject, family, source) {
            return true;
        }
    }
    false
}

/// Whether `child` is the consequence or the alternative of the `if` node.
fn is_branch(if_node: Node, child: Node) -> bool {
    ["consequence", "alternative"].iter().any(|field| {
        if_node
            .child_by_field_name(field)
            .is_some_and(|branch| branch.id() == child.id())
    })
}

/// Whether `node`, or a node under it, assigns `subject`. Only called on siblings that come before a
/// site, so every assignment found there ends before the site starts.
fn contains_assignment(node: Node, subject: &str, family: Family, source: &[u8]) -> bool {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind() == "assignment_expression"
            && current
                .child_by_field_name("left")
                .is_some_and(|left| place_of(family, left, source) == subject)
        {
            return true;
        }
        let mut cursor = current.walk();
        stack.extend(current.children(&mut cursor));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(lang: Lang, file: &str, source: &str) -> FileStates {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&lang.language())
            .expect("the grammar loads");
        let tree = parser.parse(source, None).expect("the source parses");
        extract(lang, file, tree.root_node(), source.as_bytes())
    }

    /// (from, to, file, line, end_line, via) for each transition of the machine.
    fn edges(machine: &StateMachine) -> Vec<(String, String, String, u32, u32, Option<String>)> {
        machine
            .transitions
            .iter()
            .map(|t| {
                (
                    t.from.clone(),
                    t.to.clone(),
                    t.file.clone(),
                    t.line,
                    t.end_line,
                    t.via.clone(),
                )
            })
            .collect()
    }

    fn edge(
        from: &str,
        to: &str,
        file: &str,
        line: u32,
        via: &str,
    ) -> (String, String, String, u32, u32, Option<String>) {
        (
            from.to_string(),
            to.to_string(),
            file.to_string(),
            line,
            line,
            Some(via.to_string()),
        )
    }

    fn state_names(machine: &StateMachine) -> Vec<&str> {
        machine.states.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn rust_match_arm_and_guarded_assignment() {
        let source = "\
pub enum OrderState {
    Open,
    Paid,
    Shipped,
}

pub fn pay(order: &mut Order) {
    match order.state {
        OrderState::Open => order.state = OrderState::Paid,
        _ => {}
    }
}

pub fn ship(order: &mut Order) {
    if order.state == OrderState::Paid {
        order.state = OrderState::Shipped;
    }
}

pub fn reopen(state: OrderState) -> OrderState {
    match state {
        OrderState::Shipped => OrderState::Open,
        other => other,
    }
}
";
        let machines = resolve(vec![facts(Lang::Rust, "order.rs", source)]);
        assert_eq!(machines.len(), 1);
        let machine = &machines[0];
        assert_eq!(machine.id, "order.rs#OrderState");
        assert_eq!(machine.line, 1);
        assert_eq!(state_names(machine), ["Open", "Paid", "Shipped"]);
        assert_eq!(
            machine.states.iter().map(|s| s.line).collect::<Vec<_>>(),
            [2, 3, 4]
        );
        assert_eq!(
            edges(machine),
            vec![
                edge("Open", "Paid", "order.rs", 9, "pay"),
                edge("Paid", "Shipped", "order.rs", 16, "ship"),
                edge("Shipped", "Open", "order.rs", 22, "reopen"),
            ]
        );
        assert_eq!(machine.confidence, Confidence::High);
    }

    #[test]
    fn rust_self_path_inside_impl_resolves_to_the_impl_type() {
        let source = "\
pub enum TaskPhase {
    Queued,
    Running,
    Stopped,
}

impl TaskPhase {
    pub fn next(self) -> Self {
        match self {
            Self::Queued => Self::Running,
            Self::Running => Self::Stopped,
            Self::Stopped => Self::Stopped,
        }
    }

    pub fn halt(&mut self) {
        if *self == Self::Running {
            *self = Self::Stopped;
        }
    }
}
";
        let machines = resolve(vec![facts(Lang::Rust, "phase.rs", source)]);
        assert_eq!(machines.len(), 1);
        assert_eq!(machines[0].id, "phase.rs#TaskPhase");
        assert_eq!(
            edges(&machines[0]),
            vec![
                edge("Queued", "Running", "phase.rs", 10, "next"),
                edge("Running", "Stopped", "phase.rs", 11, "next"),
                edge("Running", "Stopped", "phase.rs", 18, "halt"),
            ]
        );
        assert_eq!(machines[0].confidence, Confidence::High);
    }

    #[test]
    fn rust_or_pattern_arms_and_cross_enum_values_are_not_read() {
        let source = "\
pub enum Stage { Declaring, Declared, Building }
pub enum OrderPhase { Splitting, Building }

pub fn clear(stage: Stage) -> Stage {
    match stage {
        Stage::Declared | Stage::Building => Stage::Declaring,
        other => other,
    }
}

pub fn phase(state: Stage) -> OrderPhase {
    match state {
        Stage::Declaring => OrderPhase::Splitting,
        _ => OrderPhase::Building,
    }
}

pub fn build(stage: &mut Stage) {
    if *stage == Stage::Declared {
        *stage = Stage::Building;
    }
}
";
        let machines = resolve(vec![facts(Lang::Rust, "stage.rs", source)]);
        assert_eq!(machines.len(), 1, "OrderPhase has no transition of its own");
        assert_eq!(machines[0].name, "Stage");
        assert_eq!(
            edges(&machines[0]),
            vec![edge("Declared", "Building", "stage.rs", 20, "build")]
        );
    }

    #[test]
    fn ts_union_with_switch_assignment() {
        let source = "\
export type OrderStatus = 'open' | 'paid' | 'shipped';

export function settle(order: Order) {
  switch (order.status) {
    case 'open':
      order.status = 'paid';
      break;
    case 'paid':
      order.status = 'shipped';
      break;
  }
}
";
        let machines = resolve(vec![facts(Lang::TypeScript, "order.ts", source)]);
        assert_eq!(machines.len(), 1);
        let machine = &machines[0];
        assert_eq!(machine.id, "order.ts#OrderStatus");
        assert_eq!(state_names(machine), ["open", "paid", "shipped"]);
        assert_eq!(
            edges(machine),
            vec![
                edge("open", "paid", "order.ts", 6, "settle"),
                edge("paid", "shipped", "order.ts", 9, "settle"),
            ]
        );
        assert_eq!(machine.confidence, Confidence::High);
    }

    #[test]
    fn ts_enum_member_assignment_reads_the_if_branch() {
        let source = "\
export enum OrderStatus {
  Open,
  Paid,
}

export function markPaid(order: Order) {
  if (order.status === OrderStatus.Open) {
    order.status = OrderStatus.Paid;
  }
}
";
        let machines = resolve(vec![facts(Lang::Tsx, "order.tsx", source)]);
        assert_eq!(machines.len(), 1);
        assert_eq!(state_names(&machines[0]), ["Open", "Paid"]);
        assert_eq!(
            edges(&machines[0]),
            vec![edge("Open", "Paid", "order.tsx", 8, "markPaid")]
        );
    }

    #[test]
    fn unknown_source_lowers_confidence() {
        let source = "\
export enum TicketStatus { New, Open, Closed }

export function open(t: Ticket) {
  t.status = TicketStatus.Open;
}

export function close(t: Ticket) {
  if (t.status === TicketStatus.Open || t.urgent) {
    t.status = TicketStatus.Closed;
  }
}
";
        let machines = resolve(vec![facts(Lang::TypeScript, "ticket.ts", source)]);
        assert_eq!(machines.len(), 1);
        let machine = &machines[0];
        assert_eq!(
            edges(machine),
            vec![
                edge("*", "Open", "ticket.ts", 4, "open"),
                edge("*", "Closed", "ticket.ts", 9, "close"),
            ]
        );
        assert_eq!(machine.confidence, Confidence::Low);
    }

    #[test]
    fn self_transitions_are_dropped() {
        let source = "\
pub enum LaneState { Idle, Busy }

pub fn tick(lane: &mut Lane) {
    match lane.state {
        LaneState::Idle => lane.state = LaneState::Busy,
        LaneState::Busy => lane.state = LaneState::Busy,
    }
}
";
        let machines = resolve(vec![facts(Lang::Rust, "lane.rs", source)]);
        assert_eq!(machines.len(), 1);
        assert_eq!(
            edges(&machines[0]),
            vec![edge("Idle", "Busy", "lane.rs", 5, "tick")]
        );
    }

    #[test]
    fn target_outside_the_states_is_dropped() {
        let source = "\
pub enum LaneState { Idle, Busy }

pub fn stop(lane: &mut Lane) {
    lane.state = LaneState::Stopped;
}

pub fn start(lane: &mut Lane) {
    lane.state = LaneState::Busy;
}
";
        let machines = resolve(vec![facts(Lang::Rust, "lane.rs", source)]);
        assert_eq!(machines.len(), 1);
        assert_eq!(
            edges(&machines[0]),
            vec![edge("*", "Busy", "lane.rs", 8, "start")]
        );
        assert_eq!(machines[0].confidence, Confidence::Low);
    }

    #[test]
    fn grouped_case_labels_leave_the_source_unknown() {
        let source = "\
export type Mode = 'a' | 'b' | 'c';

export function run(x: Thing) {
  switch (x.mode) {
    case 'a':
    case 'b':
      x.mode = 'c';
      break;
  }
}
";
        let machines = resolve(vec![facts(Lang::TypeScript, "mode.ts", source)]);
        assert_eq!(machines.len(), 1);
        assert_eq!(
            edges(&machines[0]),
            vec![edge("*", "c", "mode.ts", 7, "run")]
        );
    }

    #[test]
    fn same_name_in_two_files_attaches_to_the_own_file() {
        let a = facts(Lang::Rust, "a.rs", "pub enum JobState { X, Y }\n");
        let b = facts(
            Lang::Rust,
            "b.rs",
            "pub enum JobState { X, Y }\npub fn go(j: &mut J) { j.state = JobState::Y; }\n",
        );
        let c = facts(
            Lang::Rust,
            "c.rs",
            "pub fn stray(j: &mut J) { j.state = JobState::Y; }\n",
        );
        let machines = resolve(vec![a, b, c]);
        assert_eq!(machines.len(), 1);
        assert_eq!(machines[0].file, "b.rs");
        assert_eq!(edges(&machines[0]), vec![edge("*", "Y", "b.rs", 2, "go")]);
    }

    #[test]
    fn literal_assignment_attaches_only_to_one_matching_declaration() {
        let a = facts(
            Lang::TypeScript,
            "a.ts",
            "export type OrderStatus = 'paid' | 'open';\n",
        );
        let b = facts(
            Lang::TypeScript,
            "b.ts",
            "export type InvoiceStatus = 'paid' | 'due';\n",
        );
        let js = facts(
            Lang::JavaScript,
            "legacy.js",
            "order.status = 'paid';\nx.state = 'open';\n",
        );
        let machines = resolve(vec![a, b, js]);
        assert!(
            machines.is_empty(),
            "two declarations hold 'paid' under a status name, and no name holds 'open' with 'state'"
        );

        let a = facts(
            Lang::TypeScript,
            "a.ts",
            "export type OrderStatus = 'paid' | 'open';\n",
        );
        let js = facts(Lang::JavaScript, "legacy.js", "order.status = 'paid';\n");
        let machines = resolve(vec![a, js]);
        assert_eq!(machines.len(), 1);
        assert_eq!(
            edges(&machines[0]),
            vec![(
                "*".to_string(),
                "paid".to_string(),
                "legacy.js".to_string(),
                1,
                1,
                None
            )]
        );
    }

    #[test]
    fn rust_earlier_arm_of_a_nested_match_does_not_hide_the_source() {
        let source = "\
pub enum JobState {
    Queued,
    Running,
    Done,
}

impl Job {
    pub fn step(&mut self, event: Event) {
        match self.state {
            JobState::Queued => match event {
                Event::Start => self.state = JobState::Running,
                Event::Cancel => self.state = JobState::Done,
            },
            _ => {}
        }
    }
}
";
        let machines = resolve(vec![facts(Lang::Rust, "job.rs", source)]);
        assert_eq!(machines.len(), 1);
        assert_eq!(
            edges(&machines[0]),
            vec![
                edge("Queued", "Running", "job.rs", 11, "step"),
                edge("Queued", "Done", "job.rs", 12, "step"),
            ]
        );
    }

    #[test]
    fn rust_if_alternative_does_not_hide_the_source() {
        let source = "\
pub enum JobState { Queued, Running, Done }
pub fn start(job: &mut Job, fast: bool) {
    if job.state == JobState::Queued {
        if fast {
            job.state = JobState::Running;
        } else {
            job.state = JobState::Done;
        }
    }
}
";
        let machines = resolve(vec![facts(Lang::Rust, "job.rs", source)]);
        assert_eq!(machines.len(), 1);
        assert_eq!(
            edges(&machines[0]),
            vec![
                edge("Queued", "Running", "job.rs", 5, "start"),
                edge("Queued", "Done", "job.rs", 7, "start"),
            ]
        );
    }

    #[test]
    fn ts_if_alternative_does_not_hide_the_source() {
        let source = "\
export type OrderStatus = 'open' | 'paid' | 'void';
export function close(o: Order, x: boolean) {
  if (o.status === 'open') {
    if (x) o.status = 'paid';
    else o.status = 'void';
  }
}
";
        let machines = resolve(vec![facts(Lang::TypeScript, "close.ts", source)]);
        assert_eq!(machines.len(), 1);
        assert_eq!(
            edges(&machines[0]),
            vec![
                edge("open", "paid", "close.ts", 4, "close"),
                edge("open", "void", "close.ts", 5, "close"),
            ]
        );
    }

    #[test]
    fn rust_let_chain_guard_pins_the_source() {
        let source = "\
pub enum OrderState {
    Splitting,
    Split,
}

impl Desk {
    pub fn split(&mut self, order: u32) {
        if let Some(o) = self.orders.get_mut(order) && o.state == OrderState::Splitting {
            o.state = OrderState::Split;
        }
    }
}
";
        let machines = resolve(vec![facts(Lang::Rust, "desk.rs", source)]);
        assert_eq!(machines.len(), 1);
        assert_eq!(
            edges(&machines[0]),
            vec![edge("Splitting", "Split", "desk.rs", 9, "split")]
        );
    }

    #[test]
    fn rust_deref_and_reference_places_match_the_arm() {
        let source = "\
pub enum JobState { Queued, Running }
pub fn pick(state: &mut JobState) {
    match state {
        JobState::Queued => *state = JobState::Running,
        _ => {}
    }
}
pub struct Job { pub state: JobState }
impl Job {
    pub fn begin(&mut self) {
        match &self.state {
            JobState::Queued => self.state = JobState::Running,
            _ => {}
        }
    }
    pub fn retry(&mut self) {
        match &mut self.state {
            JobState::Queued => self.state = JobState::Running,
            _ => {}
        }
    }
}
";
        let machines = resolve(vec![facts(Lang::Rust, "job.rs", source)]);
        assert_eq!(machines.len(), 1);
        assert_eq!(
            edges(&machines[0]),
            vec![
                edge("Queued", "Running", "job.rs", 4, "pick"),
                edge("Queued", "Running", "job.rs", 12, "begin"),
                edge("Queued", "Running", "job.rs", 18, "retry"),
            ]
        );
    }

    #[test]
    fn ts_member_assignment_on_a_nested_object_is_read() {
        let source = "\
export type OrderStatus = 'open' | 'paid';
export class Checkout {
  settle() {
    switch (this.order.status) {
      case 'open':
        this.order.status = 'paid';
        break;
    }
  }
}
export function pay(state: State) {
  state.order.status = 'paid';
}
";
        let machines = resolve(vec![facts(Lang::TypeScript, "checkout.ts", source)]);
        assert_eq!(machines.len(), 1);
        assert_eq!(
            edges(&machines[0]),
            vec![
                edge("open", "paid", "checkout.ts", 6, "settle"),
                edge("*", "paid", "checkout.ts", 12, "pay"),
            ]
        );
    }

    #[test]
    fn ts_braced_case_that_breaks_is_not_fallthrough() {
        let source = "\
export type OrderStatus = 'a' | 'b' | 'c';
export function run(o: Order) {
  switch (o.status) {
    case 'a': {
      log();
      break;
    }
    case 'b':
      o.status = 'c';
      break;
  }
}
";
        let machines = resolve(vec![facts(Lang::TypeScript, "run.ts", source)]);
        assert_eq!(machines.len(), 1);
        assert_eq!(
            edges(&machines[0]),
            vec![edge("b", "c", "run.ts", 9, "run")]
        );
        assert_eq!(machines[0].confidence, Confidence::High);
    }

    #[test]
    fn javascript_literal_does_not_attach_to_a_rust_enum() {
        let rust = facts(
            Lang::Rust,
            "job.rs",
            "pub enum JobState { Running, Done }\npub fn finish(job: &mut Job) {\n    job.state = JobState::Done;\n}\n",
        );
        let js = facts(Lang::JavaScript, "legacy.js", "job.state = 'Running';\n");
        let machines = resolve(vec![rust, js]);
        assert_eq!(machines.len(), 1);
        assert_eq!(
            edges(&machines[0]),
            vec![edge("*", "Done", "job.rs", 3, "finish")]
        );
    }

    #[test]
    fn rust_path_does_not_attach_to_a_typescript_alias() {
        let ts = facts(
            Lang::TypeScript,
            "mode.ts",
            "export type Mode = 'Fast' | 'Slow';\nexport function slow(x: X) {\n  x.mode = 'Slow';\n}\n",
        );
        let rust = facts(
            Lang::Rust,
            "stray.rs",
            "pub fn fast(x: &mut X) {\n    x.mode = Mode::Fast;\n}\n",
        );
        let machines = resolve(vec![ts, rust]);
        assert_eq!(machines.len(), 1);
        assert_eq!(
            edges(&machines[0]),
            vec![edge("*", "Slow", "mode.ts", 3, "slow")]
        );
    }

    #[test]
    fn ts_string_enum_values_map_to_member_names() {
        let source = "\
export enum OrderStatus {
  Paid = 'paid',
  Void = 'void',
}
export function reopen(order: Order) {
  if (order.status === 'void') {
    order.status = 'paid';
  }
}
export function pay(order: Order) {
  order.status = 'paid';
  order.status = 'Void';
}
";
        let machines = resolve(vec![facts(Lang::TypeScript, "order.ts", source)]);
        assert_eq!(machines.len(), 1);
        let machine = &machines[0];
        assert_eq!(state_names(machine), ["Paid", "Void"]);
        assert_eq!(
            edges(machine),
            vec![
                edge("Void", "Paid", "order.ts", 7, "reopen"),
                edge("*", "Paid", "order.ts", 11, "pay"),
            ]
        );
        assert_eq!(machine.confidence, Confidence::Low);
    }

    #[test]
    fn large_union_gives_every_state_in_source_order() {
        let members: Vec<String> = (0..5000).map(|i| format!("'m{i}'")).collect();
        let source = format!(
            "export type BigStatus = {};\nexport function f(o: O) {{\n  o.status = 'm1';\n}}\n",
            members.join(" | ")
        );
        let machines = resolve(vec![facts(Lang::TypeScript, "big.ts", &source)]);
        assert_eq!(machines.len(), 1);
        let names = state_names(&machines[0]);
        assert_eq!(names.len(), 5000);
        assert_eq!(names[0], "m0");
        assert_eq!(names[4999], "m4999");
        assert_eq!(edges(&machines[0]), vec![edge("*", "m1", "big.ts", 3, "f")]);
    }

    #[test]
    fn quoted_values_keep_inner_quote_characters() {
        let source = "\
export type QuoteMode = '\"' | \"'\";
export function flip(q: Quote) {
  switch (q.mode) {
    case '\"':
      q.mode = \"'\";
      break;
  }
}
";
        let machines = resolve(vec![facts(Lang::TypeScript, "quote.ts", source)]);
        assert_eq!(machines.len(), 1);
        assert_eq!(state_names(&machines[0]), ["\"", "'"]);
        assert_eq!(
            edges(&machines[0]),
            vec![edge("\"", "'", "quote.ts", 5, "flip")]
        );
    }

    #[test]
    fn repeated_ids_in_one_file_are_dropped() {
        let source = "\
namespace A {
  export type OrderStatus = 'a' | 'b';
}
namespace B {
  export type OrderStatus = 'c' | 'd';
}
export type TicketStatus = 'x' | 'y';
export function f(t: T) {
  t.status = 'x';
}
";
        let machines = resolve(vec![facts(Lang::TypeScript, "ids.ts", source)]);
        assert_eq!(
            machines.len(),
            1,
            "both OrderStatus declarations share an id"
        );
        assert_eq!(machines[0].id, "ids.ts#TicketStatus");
        assert_eq!(edges(&machines[0]), vec![edge("*", "x", "ids.ts", 9, "f")]);
    }

    #[test]
    fn raw_identifiers_drop_the_prefix() {
        let source = "\
pub enum JobState {
    r#Match,
    Other,
}
pub fn settle(job: &mut Job) {
    if job.state == JobState::Other {
        job.state = JobState::r#Match;
    }
}
";
        let machines = resolve(vec![facts(Lang::Rust, "job.rs", source)]);
        assert_eq!(machines.len(), 1);
        assert_eq!(state_names(&machines[0]), ["Match", "Other"]);
        assert_eq!(
            edges(&machines[0]),
            vec![edge("Other", "Match", "job.rs", 7, "settle")]
        );
    }
}
