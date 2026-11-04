use std::collections::HashMap;
use std::path::Path;

use toml::{Table, Value};

use crate::resolve::{join, normalize};

/// The library crates of the scanned repo's own Cargo workspace, by the names `use` paths give them.
/// A repo without a readable root `Cargo.toml` has none.
#[derive(Default)]
pub(crate) struct RustCrates {
    /// Crate name as written in a `use` path, to its library root file.
    roots: HashMap<String, String>,
    /// Member directory, to the dependency names its manifest renames and their library root files.
    renames: Vec<(String, HashMap<String, String>)>,
}

/// A workspace member whose manifest parsed.
struct Member {
    dir: String,
    manifest: Table,
}

impl RustCrates {
    /// Reads the root manifest under `root` and the manifests of its workspace members, once per scan.
    pub(crate) fn discover(root: &Path) -> Self {
        let Some(manifest) = read_manifest(root, "") else {
            return Self::default();
        };
        let workspace = manifest.get("workspace").and_then(Value::as_table);
        let mut members = Vec::new();
        if let Some(workspace) = workspace {
            let excluded = strings(workspace.get("exclude"));
            let mut dirs: Vec<String> = strings(workspace.get("members"))
                .flat_map(|pattern| expand_members(root, pattern))
                .filter(|dir| !dir.is_empty())
                .filter(|dir| !excluded.clone().any(|pattern| matches_path(pattern, dir)))
                .collect();
            dirs.sort();
            dirs.dedup();
            members.extend(dirs.into_iter().filter_map(|dir| {
                read_manifest(root, &dir).map(|manifest| Member { dir, manifest })
            }));
        }
        let workspace_deps = workspace
            .and_then(|workspace| workspace.get("dependencies"))
            .and_then(Value::as_table);
        if manifest.contains_key("package") {
            members.push(Member {
                dir: String::new(),
                manifest: manifest.clone(),
            });
        }

        let mut roots = HashMap::new();
        let mut packages = HashMap::new();
        for member in &members {
            if let Some((package, name, lib_root)) = library(root, member) {
                packages.insert(package, lib_root.clone());
                roots.entry(name).or_insert(lib_root);
            }
        }
        let renames = members
            .iter()
            .map(|member| {
                let renamed = renamed_dependencies(&member.manifest, workspace_deps)
                    .filter_map(|(alias, package)| {
                        let lib_root = packages.get(package)?;
                        Some((alias, lib_root.clone()))
                    })
                    .collect::<HashMap<_, _>>();
                (member.dir.clone(), renamed)
            })
            .filter(|(_, renamed)| !renamed.is_empty())
            .collect();
        Self { roots, renames }
    }

    /// The library root file that `name` at the start of a `use` path in `from` refers to. A rename in
    /// the manifest of the member holding `from` wins over a crate name.
    pub(crate) fn lib_root(&self, from: &str, name: &str) -> Option<&str> {
        let renamed = self
            .renames
            .iter()
            .filter(|(dir, _)| {
                dir.is_empty()
                    || from
                        .strip_prefix(dir.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            })
            .max_by_key(|(dir, _)| dir.len())
            .and_then(|(_, renamed)| renamed.get(name));
        renamed.or_else(|| self.roots.get(name)).map(String::as_str)
    }
}

fn read_manifest(root: &Path, dir: &str) -> Option<Table> {
    let text = std::fs::read_to_string(root.join(join(dir, "Cargo.toml"))).ok()?;
    text.parse().ok()
}

fn strings(value: Option<&Value>) -> impl Iterator<Item = &str> + Clone {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
}

/// The package name, the name `use` paths give the library, and the library root file. The root is
/// `[lib] path`, else `src/lib.rs`, else `src/main.rs` so tests of a binary crate still resolve.
fn library(root: &Path, member: &Member) -> Option<(String, String, String)> {
    let package = member
        .manifest
        .get("package")?
        .as_table()?
        .get("name")?
        .as_str()?;
    let lib = member.manifest.get("lib").and_then(Value::as_table);
    let name = lib
        .and_then(|lib| lib.get("name"))
        .and_then(Value::as_str)
        .unwrap_or(package)
        .replace('-', "_");
    let lib_root = match lib.and_then(|lib| lib.get("path")).and_then(Value::as_str) {
        Some(path) => normalize(&join(&member.dir, path))?,
        None => ["src/lib.rs", "src/main.rs"]
            .iter()
            .map(|file| join(&member.dir, file))
            .find(|file| root.join(file).is_file())?,
    };
    Some((package.to_string(), name, lib_root))
}

/// Dependencies declared as `alias = { package = "real-name", ... }`, directly or through
/// `alias.workspace = true` and a renamed entry in `[workspace.dependencies]`.
fn renamed_dependencies<'a>(
    manifest: &'a Table,
    workspace_deps: Option<&'a Table>,
) -> impl Iterator<Item = (String, &'a str)> {
    ["dependencies", "dev-dependencies", "build-dependencies"]
        .into_iter()
        .filter_map(|section| manifest.get(section)?.as_table())
        .flatten()
        .filter_map(move |(alias, entry)| {
            let entry = entry.as_table()?;
            let inherited = entry.get("workspace").and_then(Value::as_bool) == Some(true);
            let source = if inherited {
                workspace_deps?.get(alias)?.as_table()?
            } else {
                entry
            };
            let package = source.get("package")?.as_str()?;
            Some((alias.replace('-', "_"), package))
        })
}

/// The directories a `members` entry names. Each `*` or `?` segment matches the subdirectories
/// that are there; other segments are taken as written.
fn expand_members(root: &Path, pattern: &str) -> Vec<String> {
    let mut dirs = vec![String::new()];
    for segment in pattern.split('/') {
        if !segment.contains(['*', '?']) {
            dirs = dirs
                .iter()
                .filter_map(|dir| normalize(&join(dir, segment)))
                .collect();
            continue;
        }
        dirs = dirs
            .iter()
            .flat_map(|dir| {
                let names: Vec<String> = std::fs::read_dir(root.join(dir))
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                    .filter_map(|entry| entry.file_name().into_string().ok())
                    .filter(|name| wildcard(segment.as_bytes(), name.as_bytes()))
                    .collect();
                names.into_iter().map(move |name| join(dir, &name))
            })
            .collect();
    }
    dirs
}

fn matches_path(pattern: &str, path: &str) -> bool {
    let Some(pattern) = normalize(pattern) else {
        return false;
    };
    let patterns: Vec<&str> = pattern.split('/').collect();
    let parts: Vec<&str> = path.split('/').collect();
    patterns.len() == parts.len()
        && patterns
            .iter()
            .zip(&parts)
            .all(|(pattern, part)| wildcard(pattern.as_bytes(), part.as_bytes()))
}

/// Matches one path segment against a pattern where `*` is any run of characters and `?` is one.
fn wildcard(pattern: &[u8], name: &[u8]) -> bool {
    match pattern.split_first() {
        None => name.is_empty(),
        Some((b'*', rest)) => (0..=name.len()).any(|skip| wildcard(rest, &name[skip..])),
        Some((b'?', rest)) => !name.is_empty() && wildcard(rest, &name[1..]),
        Some((byte, rest)) => name.first() == Some(byte) && wildcard(rest, &name[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn workspace_members_by_glob_and_lib_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\", \"tools/gen\"]\nexclude = [\"crates/old\"]\n",
        );
        write(
            root,
            "crates/alpha-core/Cargo.toml",
            "[package]\nname = \"alpha-core\"\n",
        );
        write(root, "crates/alpha-core/src/lib.rs", "");
        write(
            root,
            "crates/beta/Cargo.toml",
            "[package]\nname = \"beta\"\n[lib]\nname = \"beta_lib\"\npath = \"lib/beta.rs\"\n",
        );
        write(root, "crates/beta/lib/beta.rs", "");
        write(root, "crates/old/Cargo.toml", "[package]\nname = \"old\"\n");
        write(root, "crates/old/src/lib.rs", "");
        write(root, "tools/gen/Cargo.toml", "[package]\nname = \"gen\"\n");
        write(root, "tools/gen/src/main.rs", "");

        let crates = RustCrates::discover(root);
        assert_eq!(
            crates.lib_root("x.rs", "alpha_core"),
            Some("crates/alpha-core/src/lib.rs")
        );
        assert_eq!(
            crates.lib_root("x.rs", "beta_lib"),
            Some("crates/beta/lib/beta.rs")
        );
        assert_eq!(crates.lib_root("x.rs", "beta"), None);
        assert_eq!(crates.lib_root("x.rs", "old"), None);
        assert_eq!(
            crates.lib_root("x.rs", "gen"),
            Some("tools/gen/src/main.rs")
        );
        assert_eq!(crates.lib_root("x.rs", "alpha-core"), None);
    }

    #[test]
    fn dependency_renames_apply_to_their_member() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "Cargo.toml",
            "[workspace]\nmembers = [\"a\", \"b\"]\n\
             [workspace.dependencies]\nshared = { package = \"a\", path = \"a\" }\n",
        );
        write(root, "a/Cargo.toml", "[package]\nname = \"a\"\n");
        write(root, "a/src/lib.rs", "");
        write(
            root,
            "b/Cargo.toml",
            "[package]\nname = \"b\"\n[dependencies]\nfirst = { package = \"a\", path = \"../a\" }\n\
             [dev-dependencies]\nshared.workspace = true\n",
        );
        write(root, "b/src/lib.rs", "");

        let crates = RustCrates::discover(root);
        assert_eq!(
            crates.lib_root("b/src/lib.rs", "first"),
            Some("a/src/lib.rs")
        );
        assert_eq!(
            crates.lib_root("b/tests/it.rs", "shared"),
            Some("a/src/lib.rs")
        );
        assert_eq!(crates.lib_root("a/src/lib.rs", "first"), None);
        assert_eq!(crates.lib_root("bb/src/lib.rs", "first"), None);
    }

    #[test]
    fn single_package_without_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "Cargo.toml", "[package]\nname = \"solo-tool\"\n");
        write(root, "src/lib.rs", "");
        let crates = RustCrates::discover(root);
        assert_eq!(
            crates.lib_root("tests/it.rs", "solo_tool"),
            Some("src/lib.rs")
        );
    }

    #[test]
    fn missing_or_malformed_manifests_give_no_crates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(RustCrates::discover(root).roots.is_empty());

        write(root, "Cargo.toml", "[workspace\nmembers = [\"a\"");
        assert!(RustCrates::discover(root).roots.is_empty());

        write(
            root,
            "Cargo.toml",
            "[workspace]\nmembers = [\"a\", \"b\", 7]\n",
        );
        write(root, "a/Cargo.toml", "[package]\nname = \"a\"\n");
        write(root, "a/src/lib.rs", "");
        write(root, "b/Cargo.toml", "[package\nname = ");
        let crates = RustCrates::discover(root);
        assert_eq!(crates.lib_root("x.rs", "a"), Some("a/src/lib.rs"));
        assert_eq!(crates.roots.len(), 1);
    }
}
