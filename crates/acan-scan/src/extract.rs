use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::time::Instant;

use tree_sitter::{Node, ParseOptions, Parser, Query, QueryCursor, StreamingIterator};

use crate::ScanError;
use crate::lang::Lang;
use crate::model::{LineRange, Symbol, SymbolKind};

/// An import as written in the source, before it is resolved to a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawImport {
    pub(crate) line: u32,
    pub(crate) reference: RawRef,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RawRef {
    /// A TypeScript or JavaScript module specifier, as written.
    Module(String),
    /// A Rust `mod name;` declaration.
    RustMod(String),
    /// A Rust `use` path with groups expanded.
    RustUse(String),
}

pub(crate) struct Extracted {
    pub(crate) symbols: Vec<Symbol>,
    pub(crate) imports: Vec<RawImport>,
}

pub(crate) struct Extractor {
    parser: Parser,
    queries: BTreeMap<Lang, Queries>,
}

struct Queries {
    symbols: Query,
    imports: Query,
}

impl Extractor {
    pub(crate) fn new() -> Result<Self, ScanError> {
        let queries = Lang::ALL
            .into_iter()
            .map(|lang| Queries::compile(lang).map(|compiled| (lang, compiled)))
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        Ok(Self {
            parser: Parser::new(),
            queries,
        })
    }

    pub(crate) fn extract_until(
        &mut self,
        lang: Lang,
        path: &str,
        source: &str,
        deadline: Instant,
    ) -> Result<Extracted, ScanError> {
        self.parser
            .set_language(&lang.language())
            .map_err(|error| ScanError::Language {
                lang,
                source: error,
            })?;
        self.parser.reset();
        let mut progress = |_: &tree_sitter::ParseState| {
            if Instant::now() >= deadline {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        };
        let bytes = source.as_bytes();
        let tree = if Instant::now() >= deadline {
            None
        } else {
            self.parser.parse_with_options(
                &mut |offset, _| &bytes[offset..],
                None,
                Some(ParseOptions::new().progress_callback(&mut progress)),
            )
        }
        .ok_or_else(|| ScanError::ParseCancelled {
            path: path.to_string(),
        })?;
        let queries = self
            .queries
            .get(&lang)
            .expect("queries are compiled for every language in new");
        let root = tree.root_node();
        let bytes = source.as_bytes();
        let (symbols, imports) = match lang {
            Lang::Rust => (
                rust_symbols(&queries.symbols, root, bytes),
                rust_imports(&queries.imports, root, bytes),
            ),
            Lang::TypeScript | Lang::Tsx | Lang::JavaScript => (
                js_symbols(&queries.symbols, root, bytes),
                js_imports(&queries.imports, root, bytes),
            ),
        };
        Ok(Extracted { symbols, imports })
    }
}

impl Queries {
    fn compile(lang: Lang) -> Result<Self, ScanError> {
        let (symbols, imports) = match lang {
            Lang::TypeScript | Lang::Tsx => (
                include_str!("queries/ts_symbols.scm"),
                include_str!("queries/js_imports.scm"),
            ),
            Lang::JavaScript => (
                include_str!("queries/js_symbols.scm"),
                include_str!("queries/js_imports.scm"),
            ),
            Lang::Rust => (
                include_str!("queries/rust_symbols.scm"),
                include_str!("queries/rust_imports.scm"),
            ),
        };
        let language = lang.language();
        let compile = |text: &str| {
            Query::new(&language, text).map_err(|source| ScanError::Query { lang, source })
        };
        Ok(Self {
            symbols: compile(symbols)?,
            imports: compile(imports)?,
        })
    }
}

/// One query match as (capture name, node) pairs.
struct Hit<'q, 't> {
    captures: Vec<(&'q str, Node<'t>)>,
}

impl<'t> Hit<'_, 't> {
    fn node(&self, name: &str) -> Option<Node<'t>> {
        self.captures
            .iter()
            .find(|(capture, _)| *capture == name)
            .map(|&(_, node)| node)
    }

    /// The `@def.*` capture that names what the match found.
    fn def(&self) -> Option<(&str, Node<'t>)> {
        self.captures
            .iter()
            .find(|(capture, _)| capture.starts_with("def."))
            .copied()
    }
}

fn hits<'q, 't>(query: &'q Query, root: Node<'t>, source: &[u8]) -> Vec<Hit<'q, 't>> {
    let names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, root, source);
    let mut out = Vec::new();
    while let Some(found) = matches.next() {
        let captures = found
            .captures()
            .iter()
            .map(|capture| (names[capture.index as usize], capture.node))
            .collect();
        out.push(Hit { captures });
    }
    out
}

fn js_symbols(query: &Query, root: Node, source: &[u8]) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    let mut clauses = Vec::new();
    let mut default_locals = Vec::new();
    // Each method records its class name. Its export status is set once every clause is applied.
    let mut methods: Vec<(usize, String)> = Vec::new();
    for hit in hits(query, root, source) {
        let Some((capture, node)) = hit.def() else {
            continue;
        };
        match capture {
            "def.specifier" => clauses.extend(export_clause(node, source)),
            "def.default" => {
                default_locals.extend(default_local(node, source));
                symbols.extend(js_symbol(capture, node, source));
            }
            "def.method" => {
                if let Some((owner, method)) = js_method(node, source) {
                    methods.push((symbols.len(), owner));
                    symbols.push(method);
                }
            }
            _ => symbols.extend(js_symbol(capture, node, source)),
        }
    }
    for clause in clauses {
        let declared = !clause.reexport && mark_local_export(&mut symbols, &clause.local);
        if !declared || clause.local != clause.exported {
            symbols.push(clause.symbol);
        }
    }
    for name in default_locals {
        mark_local_export(&mut symbols, &name);
    }
    for (index, owner) in methods {
        let class_exported = symbols
            .iter()
            .any(|s| s.kind == SymbolKind::Class && s.exported && s.name == owner);
        symbols[index].exported = class_exported;
    }
    symbols.sort_by_key(|s| s.byte_range.start);
    symbols
}

/// Marks the top-level declaration named `name` as exported. Returns whether one was found.
fn mark_local_export(symbols: &mut [Symbol], name: &str) -> bool {
    let declaration = symbols
        .iter_mut()
        .find(|s| s.name == name && !matches!(s.kind, SymbolKind::Method | SymbolKind::Export));
    match declaration {
        Some(symbol) => {
            symbol.exported = true;
            true
        }
        None => false,
    }
}

/// The local binding in `export default name;`. Expression defaults have none.
fn default_local(statement: Node, source: &[u8]) -> Option<String> {
    let value = statement.child_by_field_name("value")?;
    (value.kind() == "identifier").then(|| text(value, source).to_string())
}

/// A method of a module-level class, with the class name. The export status is set later.
fn js_method(node: Node, source: &[u8]) -> Option<(String, Symbol)> {
    let class = node.parent()?.parent()?;
    declaration_scope(class)?;
    let owner = name_of(class, source)?.to_string();
    let method = symbol(name_of(node, source)?, SymbolKind::Method, node, false);
    Some((owner, method))
}

fn js_symbol(capture: &str, node: Node, source: &[u8]) -> Option<Symbol> {
    match capture {
        "def.default" => {
            is_top_level(node).then(|| symbol("default", SymbolKind::Export, node, true))
        }
        "def.namespace_export" => {
            let name = text(node.named_child(0)?, source);
            is_top_level(node.parent()?).then(|| symbol(name, SymbolKind::Export, node, true))
        }
        "def.const" | "def.variable" => {
            let exported = declaration_scope(node.parent()?)?;
            let kind = if is_function_like(node.child_by_field_name("value")) {
                SymbolKind::Function
            } else if capture == "def.const" {
                SymbolKind::Const
            } else {
                SymbolKind::Variable
            };
            Some(symbol(name_of(node, source)?, kind, node, exported))
        }
        _ => {
            let kind = match capture {
                "def.function" => SymbolKind::Function,
                "def.class" => SymbolKind::Class,
                "def.interface" => SymbolKind::Interface,
                "def.type" => SymbolKind::Type,
                "def.enum" => SymbolKind::Enum,
                _ => return None,
            };
            let exported = declaration_scope(node)?;
            Some(symbol(name_of(node, source)?, kind, node, exported))
        }
    }
}

struct ExportClause {
    local: String,
    exported: String,
    reexport: bool,
    symbol: Symbol,
}

fn export_clause(specifier: Node, source: &[u8]) -> Option<ExportClause> {
    let statement = specifier.parent()?.parent()?;
    if !is_top_level(statement) {
        return None;
    }
    let local = text(specifier.child_by_field_name("name")?, source).to_string();
    let exported = specifier
        .child_by_field_name("alias")
        .map_or_else(|| local.clone(), |alias| text(alias, source).to_string());
    let reexport = statement.child_by_field_name("source").is_some();
    Some(ExportClause {
        symbol: symbol(&exported, SymbolKind::Export, specifier, true),
        local,
        exported,
        reexport,
    })
}

/// `Some(exported)` when a declaration sits at module level, directly or under a top-level `export`.
fn declaration_scope(node: Node) -> Option<bool> {
    let parent = node.parent()?;
    if parent.kind() == "export_statement" {
        is_top_level(parent).then_some(true)
    } else {
        is_top_level(node).then_some(false)
    }
}

fn is_top_level(node: Node) -> bool {
    node.parent()
        .is_some_and(|parent| parent.kind() == "program")
}

fn is_function_like(value: Option<Node>) -> bool {
    matches!(
        value.map(|node| node.kind()),
        Some("arrow_function" | "function_expression" | "function" | "generator_function")
    )
}

fn js_imports(query: &Query, root: Node, source: &[u8]) -> Vec<RawImport> {
    hits(query, root, source)
        .iter()
        .filter_map(|hit| {
            let node = hit.node("source")?;
            Some(RawImport {
                line: line_number(node.start_position().row),
                reference: RawRef::Module(string_literal(node, source)),
            })
        })
        .collect()
}

fn rust_symbols(query: &Query, root: Node, source: &[u8]) -> Vec<Symbol> {
    let mut symbols: Vec<Symbol> = hits(query, root, source)
        .iter()
        .filter_map(|hit| {
            let (capture, node) = hit.def()?;
            let (kind, exported) = rust_item(capture, node, source)?;
            Some(symbol(name_of(node, source)?, kind, node, exported))
        })
        .collect();
    symbols.sort_by_key(|s| s.byte_range.start);
    symbols
}

fn rust_item(capture: &str, node: Node, source: &[u8]) -> Option<(SymbolKind, bool)> {
    if capture == "def.function" {
        let owner = node
            .parent()
            .and_then(|list| list.parent())
            .filter(|owner| matches!(owner.kind(), "impl_item" | "trait_item"));
        if let Some(owner) = owner {
            let owner_is_pub = rust_scope(owner, source)?;
            let exported = if owner.kind() == "trait_item" {
                owner_is_pub
            } else {
                has_pub(node, source)
            };
            return Some((SymbolKind::Method, exported));
        }
    }
    let kind = match capture {
        "def.function" => SymbolKind::Function,
        "def.struct" => SymbolKind::Struct,
        "def.enum" => SymbolKind::Enum,
        "def.trait" => SymbolKind::Trait,
        "def.type" => SymbolKind::Type,
        "def.const" => SymbolKind::Const,
        _ => return None,
    };
    Some((kind, rust_scope(node, source)?))
}

/// `Some(is_pub)` when a Rust item sits at module level, either directly in a file or in an inline module.
fn rust_scope(node: Node, source: &[u8]) -> Option<bool> {
    let parent = node.parent()?;
    let module_level = match parent.kind() {
        "source_file" => true,
        "declaration_list" => parent
            .parent()
            .is_some_and(|owner| owner.kind() == "mod_item"),
        _ => false,
    };
    module_level.then(|| has_pub(node, source))
}

fn has_pub(node: Node, source: &[u8]) -> bool {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .any(|child| child.kind() == "visibility_modifier" && text(child, source) == "pub")
}

fn rust_imports(query: &Query, root: Node, source: &[u8]) -> Vec<RawImport> {
    let mut imports = Vec::new();
    for hit in hits(query, root, source) {
        let Some((capture, node)) = hit.def() else {
            continue;
        };
        let line = line_number(node.start_position().row);
        match capture {
            "def.mod" => {
                if let Some(name) = name_of(node, source) {
                    imports.push(RawImport {
                        line,
                        reference: RawRef::RustMod(name.to_string()),
                    });
                }
            }
            "def.use" => {
                if let Some(argument) = node.child_by_field_name("argument") {
                    imports.extend(expand_use(text(argument, source)).into_iter().map(|path| {
                        RawImport {
                            line,
                            reference: RawRef::RustUse(path),
                        }
                    }));
                }
            }
            _ => {}
        }
    }
    imports
}

/// Expands a Rust use tree into full paths. `crate::{a::B, c::{self, D as E}}` gives three paths.
pub(crate) fn expand_use(tree: &str) -> Vec<String> {
    let normalized = tree.split_whitespace().collect::<Vec<_>>().join(" ");
    expand_tree(&normalized, "")
}

fn expand_tree(tree: &str, prefix: &str) -> Vec<String> {
    let tree = tree.trim();
    let Some(open) = tree.find('{') else {
        let path = tree.split(" as ").next().unwrap_or_default().trim();
        let full = format!("{prefix}{path}");
        return vec![full.strip_suffix("::self").unwrap_or(&full).to_string()];
    };
    let base = format!("{prefix}{}", tree[..open].trim());
    let close = matching_brace(tree, open);
    split_top_level(&tree[open + 1..close])
        .into_iter()
        .flat_map(|part| expand_tree(part, &base))
        .collect()
}

fn matching_brace(tree: &str, open: usize) -> usize {
    let mut depth = 0usize;
    for (offset, c) in tree[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return open + offset;
                }
            }
            _ => {}
        }
    }
    tree.len()
}

fn split_top_level(list: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (index, c) in list.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&list[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&list[start..]);
    parts
        .into_iter()
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect()
}

fn symbol(name: &str, kind: SymbolKind, node: Node, exported: bool) -> Symbol {
    Symbol {
        name: name.to_string(),
        kind,
        byte_range: node.start_byte()..node.end_byte(),
        line_range: LineRange {
            start: line_number(node.start_position().row),
            end: line_number(node.end_position().row),
        },
        exported,
    }
}

fn text<'a>(node: Node, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or_default()
}

fn name_of<'a>(node: Node, source: &'a [u8]) -> Option<&'a str> {
    node.child_by_field_name("name")
        .map(|name| text(name, source))
}

fn string_literal(node: Node, source: &[u8]) -> String {
    text(node, source)
        .trim_matches(|c| matches!(c, '"' | '\'' | '`'))
        .to_string()
}

fn line_number(row: usize) -> u32 {
    (row + 1) as u32
}

#[cfg(test)]
mod tests {
    use super::expand_use;

    #[test]
    fn expands_nested_groups_and_non_ascii_prefixes() {
        assert_eq!(
            expand_use("crate::{a::{b, c::{self, D as E}}, f::*}"),
            vec![
                "crate::a::b",
                "crate::a::c",
                "crate::a::c::D",
                "crate::f::*"
            ],
        );
        assert_eq!(
            expand_use("crate::é::{a, b}"),
            vec!["crate::é::a", "crate::é::b"],
        );
    }
}
