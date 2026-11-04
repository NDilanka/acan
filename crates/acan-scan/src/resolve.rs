use std::collections::HashMap;

use crate::extract::{RawImport, RawRef};
use crate::manifest::RustCrates;
use crate::model::{FileId, ImportRef, ImportTarget};

/// Repo-relative path to file id, for the files that were scanned.
pub(crate) struct PathIndex(HashMap<String, FileId>);

impl PathIndex {
    pub(crate) fn from_paths<'a>(paths: impl IntoIterator<Item = &'a str>) -> Self {
        Self(
            paths
                .into_iter()
                .enumerate()
                .map(|(index, path)| (path.to_string(), FileId(index as u32)))
                .collect(),
        )
    }

    fn get(&self, path: &str) -> Option<FileId> {
        self.0.get(path).copied()
    }
}

pub(crate) fn resolve_import(
    from: &str,
    raw: RawImport,
    index: &PathIndex,
    crates: &RustCrates,
) -> ImportRef {
    let (specifier, target) = match raw.reference {
        RawRef::Module(specifier) => {
            let target = resolve_module(from, &specifier, index);
            (specifier, target)
        }
        RawRef::RustMod(name) => {
            let target = resolve_rust_mod(from, &name, index);
            (name, target)
        }
        RawRef::RustUse(path) => {
            let target = resolve_rust_use(from, &path, index, crates);
            (path, target)
        }
    };
    ImportRef {
        specifier,
        line: raw.line,
        target,
    }
}

const JS_EXTENSIONS: [&str; 9] = [
    ".ts", ".tsx", ".d.ts", ".js", ".jsx", ".mts", ".cts", ".mjs", ".cjs",
];
const TS_FOR_JS_EXTENSIONS: [&str; 5] = [".ts", ".tsx", ".mts", ".cts", ".d.ts"];
const JS_SOURCE_SUFFIXES: [&str; 4] = [".js", ".jsx", ".mjs", ".cjs"];

/// Bare specifiers are packages, so they are external. Relative specifiers must resolve inside the repo.
fn resolve_module(from: &str, specifier: &str, index: &PathIndex) -> ImportTarget {
    if !is_relative(specifier) {
        return ImportTarget::External;
    }
    let Some(base) = normalize(&join(parent_dir(from), specifier)) else {
        return ImportTarget::Unresolved;
    };
    find_file(index, js_candidates(&base))
}

fn is_relative(specifier: &str) -> bool {
    matches!(specifier, "." | "..") || specifier.starts_with("./") || specifier.starts_with("../")
}

/// Order follows TypeScript. It tries the path as written, then extensions, then `index` files, then a `.js` specifier mapped to its `.ts` source.
fn js_candidates(base: &str) -> Vec<String> {
    let mut candidates = vec![base.to_string()];
    candidates.extend(JS_EXTENSIONS.iter().map(|ext| format!("{base}{ext}")));
    candidates.extend(
        JS_EXTENSIONS
            .iter()
            .map(|ext| join(base, &format!("index{ext}"))),
    );
    if let Some(stem) = JS_SOURCE_SUFFIXES
        .iter()
        .find_map(|suffix| base.strip_suffix(suffix))
    {
        candidates.extend(
            TS_FOR_JS_EXTENSIONS
                .iter()
                .map(|ext| format!("{stem}{ext}")),
        );
    }
    candidates
}

/// `mod name;` lives in the module's own directory. `a/b.rs` declares into `a/b/`, while `mod.rs`, `lib.rs`, and `main.rs` declare into their own directory.
fn resolve_rust_mod(from: &str, name: &str, index: &PathIndex) -> ImportTarget {
    let base = join(&rust_module_dir(from), name);
    find_file(index, [format!("{base}.rs"), format!("{base}/mod.rs")])
}

fn rust_module_dir(from: &str) -> String {
    let dir = parent_dir(from);
    let file_name = from.rsplit('/').next().unwrap_or(from);
    match file_name {
        "mod.rs" | "lib.rs" | "main.rs" => dir.to_string(),
        _ => join(dir, file_name.strip_suffix(".rs").unwrap_or(file_name)),
    }
}

/// Resolves `crate::a::b::Item`, and `name::a::b::Item` for a crate of the repo's own workspace, by the longest module prefix that is a scanned file. `self::` and `super::` are left unresolved, and other roots are external crates.
fn resolve_rust_use(
    from: &str,
    path: &str,
    index: &PathIndex,
    crates: &RustCrates,
) -> ImportTarget {
    let mut segments = path.trim_start_matches("::").split("::");
    let root_files = match segments.next() {
        Some("crate") => {
            let Some(src_dir) = crate_src_dir(from) else {
                return ImportTarget::Unresolved;
            };
            vec![join(src_dir, "lib.rs"), join(src_dir, "main.rs")]
        }
        Some("self" | "super") => return ImportTarget::Unresolved,
        Some(name) => match crates.lib_root(from, name) {
            Some(lib_root) => vec![lib_root.to_string()],
            None => return ImportTarget::External,
        },
        None => return ImportTarget::External,
    };
    // A crate root declares its modules into its own directory, whatever the file is called.
    let src_dir = parent_dir(&root_files[0]);
    let modules: Vec<&str> = segments
        .filter(|segment| !segment.is_empty() && *segment != "*")
        .collect();
    for depth in (0..=modules.len()).rev() {
        let candidates = if depth == 0 {
            root_files.clone()
        } else {
            let base = modules[..depth]
                .iter()
                .fold(src_dir.to_string(), |acc, module| join(&acc, module));
            vec![format!("{base}.rs"), format!("{base}/mod.rs")]
        };
        let target = find_file(index, candidates);
        if !matches!(target, ImportTarget::Unresolved) {
            return target;
        }
    }
    ImportTarget::Unresolved
}

/// The nearest ancestor directory named `src`, which is the crate root for `crate::` paths.
fn crate_src_dir(from: &str) -> Option<&str> {
    let mut dir = parent_dir(from);
    loop {
        if dir.rsplit('/').next() == Some("src") {
            return Some(dir);
        }
        if dir.is_empty() {
            return None;
        }
        dir = parent_dir(dir);
    }
}

fn find_file(index: &PathIndex, candidates: impl IntoIterator<Item = String>) -> ImportTarget {
    candidates
        .into_iter()
        .find_map(|candidate| index.get(&candidate))
        .map_or(ImportTarget::Unresolved, |file| ImportTarget::File { file })
}

fn parent_dir(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

pub(crate) fn join(dir: &str, rest: &str) -> String {
    if dir.is_empty() {
        rest.to_string()
    } else {
        format!("{dir}/{rest}")
    }
}

/// Removes `.` and resolves `..`. Returns `None` when the path climbs above the repo root.
pub(crate) fn normalize(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::model::{MachineId, Plant};

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\n",
        );
        write(
            root,
            "crates/alpha-core/Cargo.toml",
            "[package]\nname = \"alpha-core\"\n",
        );
        write(root, "crates/alpha-core/src/lib.rs", "pub mod model;\n");
        write(
            root,
            "crates/alpha-core/src/model.rs",
            "pub struct Thing;\n",
        );
        write(
            root,
            "crates/beta/Cargo.toml",
            "[package]\nname = \"beta\"\n[dependencies]\nalpha-core = { path = \"../alpha-core\" }\n",
        );
        write(
            root,
            "crates/beta/src/lib.rs",
            "use alpha_core::model::Thing;\nuse serde::Serialize;\npub fn make() -> Thing { Thing }\n",
        );
        write(
            root,
            "crates/beta/tests/it.rs",
            "use beta::make;\nuse alpha_core::model::{self, Thing};\nuse alpha_core::Missing;\n",
        );
        dir
    }

    fn targets(plant: &Plant, path: &str) -> Vec<(String, Option<String>)> {
        let file = plant.files.iter().find(|file| file.path == path).unwrap();
        file.imports
            .iter()
            .map(|import| {
                let target = match import.target {
                    ImportTarget::File { file } => Some(plant.files[file.0 as usize].path.clone()),
                    _ => None,
                };
                (import.specifier.clone(), target)
            })
            .collect()
    }

    fn machine_of(plant: &Plant, path: &str) -> MachineId {
        let file = plant
            .files
            .iter()
            .find(|file| file.path == path)
            .unwrap()
            .id;
        plant
            .machines
            .iter()
            .find(|machine| machine.files.contains(&file))
            .unwrap()
            .id
    }

    #[test]
    fn workspace_crate_uses_resolve_to_their_files() {
        let dir = workspace();
        let plant = crate::scan(dir.path()).unwrap();
        let model = Some("crates/alpha-core/src/model.rs".to_string());
        assert_eq!(
            targets(&plant, "crates/beta/src/lib.rs"),
            [
                ("alpha_core::model::Thing".to_string(), model.clone()),
                ("serde::Serialize".to_string(), None),
            ]
        );
        assert_eq!(
            targets(&plant, "crates/beta/tests/it.rs"),
            [
                (
                    "beta::make".to_string(),
                    Some("crates/beta/src/lib.rs".to_string())
                ),
                ("alpha_core::model".to_string(), model.clone()),
                ("alpha_core::model::Thing".to_string(), model),
                (
                    "alpha_core::Missing".to_string(),
                    Some("crates/alpha-core/src/lib.rs".to_string())
                ),
            ]
        );
        let serde = &plant
            .files
            .iter()
            .find(|file| file.path == "crates/beta/src/lib.rs")
            .unwrap()
            .imports[1];
        assert_eq!(serde.target, ImportTarget::External);

        let from = machine_of(&plant, "crates/beta/src/lib.rs");
        let to = machine_of(&plant, "crates/alpha-core/src/model.rs");
        assert_ne!(from, to);
        assert!(
            plant
                .belts
                .iter()
                .any(|belt| belt.from == from && belt.to == to)
        );
    }

    #[test]
    fn single_crate_tests_import_it_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "Cargo.toml", "[package]\nname = \"solo-tool\"\n");
        write(root, "src/lib.rs", "pub mod parse;\n");
        write(root, "src/parse/mod.rs", "pub fn run() {}\n");
        write(root, "tests/it.rs", "use solo_tool::parse::run;\n");
        write(root, "examples/demo.rs", "use ::solo_tool::parse;\n");
        let plant = crate::scan(root).unwrap();
        let parse = Some("src/parse/mod.rs".to_string());
        assert_eq!(
            targets(&plant, "tests/it.rs"),
            [("solo_tool::parse::run".to_string(), parse.clone())]
        );
        assert_eq!(
            targets(&plant, "examples/demo.rs"),
            [("::solo_tool::parse".to_string(), parse)]
        );
    }

    #[test]
    fn lib_name_override_and_malformed_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "Cargo.toml",
            "[workspace]\nmembers = [\"core\", \"app\"]\n",
        );
        write(
            root,
            "core/Cargo.toml",
            "[package]\nname = \"core-pkg\"\n[lib]\nname = \"engine\"\n",
        );
        write(root, "core/src/lib.rs", "pub struct Engine;\n");
        write(root, "app/Cargo.toml", "[package\nname = \"app\"");
        write(
            root,
            "app/src/main.rs",
            "use engine::Engine;\nuse core_pkg::Engine;\n",
        );
        let plant = crate::scan(root).unwrap();
        assert_eq!(
            targets(&plant, "app/src/main.rs"),
            [
                (
                    "engine::Engine".to_string(),
                    Some("core/src/lib.rs".to_string())
                ),
                ("core_pkg::Engine".to_string(), None),
            ]
        );

        write(root, "Cargo.toml", "[workspace\nmembers = ");
        let plant = crate::scan(root).unwrap();
        assert_eq!(
            targets(&plant, "app/src/main.rs"),
            [
                ("engine::Engine".to_string(), None),
                ("core_pkg::Engine".to_string(), None),
            ]
        );
    }
}
