//! The repository's real files, and the check a plan must pass before its paths reach the origin.
//!
//! A model names files from memory. A declared path the repository does not hold is one the clash
//! check cannot compare with anyone else's, so the footprint would look clear when it is not. The
//! worker therefore plans against the repository's tracked files, and every path goes through
//! [`RepoFiles::check`]: an existing file must be tracked, and a new file must sit in a directory
//! the repository already has.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::tools::{git, normalize_path};

/// The most tracked paths a planning message lists. A larger repository is cut and marked.
const MAX_LISTED_FILES: usize = 2000;
/// The most bytes of scope file text a planning message carries in all.
const MAX_SCOPE_BYTES: usize = 64 * 1024;
/// A scope file larger than this is named in the message, but its text is left out.
const MAX_SCOPE_FILE_BYTES: usize = 16 * 1024;

/// The tracked files of one repository, and every directory that holds one of them.
#[derive(Debug, Clone)]
pub struct RepoFiles {
    root: PathBuf,
    files: BTreeSet<String>,
    dirs: BTreeSet<String>,
}

/// A plan that passed [`RepoFiles::check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    /// The footprint in the origin's form: each path once, in path order.
    pub footprint: Vec<String>,
    /// One line for each path the check changed: a name it resolved, or a new file that was
    /// already tracked. The lines follow the plan's order.
    pub fixes: Vec<String>,
}

impl RepoFiles {
    /// Lists the tracked files of the repository at `root`.
    ///
    /// # Errors
    ///
    /// Returns the reason when git cannot list the repository.
    pub async fn load(root: &Path) -> anyhow::Result<Self> {
        let listed = git(root, &["-c", "core.quotepath=off", "ls-files"])
            .await
            .map_err(|err| anyhow::anyhow!("listing the tracked files failed: {err}"))?;
        Ok(Self::from_paths(root, listed.lines().map(str::to_owned)))
    }

    /// The files at `paths` in the repository at `root`. Each path is repository-relative and
    /// `/`-separated.
    pub fn from_paths(root: impl Into<PathBuf>, paths: impl IntoIterator<Item = String>) -> Self {
        let mut files = BTreeSet::new();
        let mut dirs = BTreeSet::new();
        for path in paths {
            let mut dir = path.as_str();
            while let Some((parent, _)) = dir.rsplit_once('/') {
                dirs.insert(parent.to_owned());
                dir = parent;
            }
            files.insert(path);
        }
        Self {
            root: root.into(),
            files,
            dirs,
        }
    }

    /// Whether `path` is a tracked file.
    #[must_use]
    pub fn is_tracked(&self, path: &str) -> bool {
        self.files.contains(path)
    }

    /// Whether `dir` is the repository root, or holds a tracked file at some depth.
    #[must_use]
    pub fn has_dir(&self, dir: &str) -> bool {
        dir.is_empty() || self.dirs.contains(dir)
    }

    /// The tracked paths, one `- path` line each, cut at [`MAX_LISTED_FILES`].
    #[must_use]
    pub fn listing(&self) -> String {
        let mut lines: Vec<String> = self
            .files
            .iter()
            .take(MAX_LISTED_FILES)
            .map(|file| format!("- {file}"))
            .collect();
        if self.files.len() > MAX_LISTED_FILES {
            lines.push(format!(
                "... {} more files are not listed",
                self.files.len() - MAX_LISTED_FILES
            ));
        }
        lines.join("\n")
    }

    /// The module file of each machine: `src/ratelimit.ts` is machine `ratelimit`'s file. A
    /// machine that names a directory has no one module file, so it contributes nothing. In
    /// path order.
    #[must_use]
    pub fn routed(&self, machines: &[String]) -> Vec<&str> {
        self.files
            .iter()
            .map(String::as_str)
            .filter(|file| {
                let name = file.rsplit('/').next().unwrap_or(file);
                let stem = name.split('.').next().unwrap_or(name);
                machines.iter().any(|machine| machine.as_str() == stem)
            })
            .collect()
    }

    /// The tracked files that belong to the scope machines, in path order.
    #[must_use]
    pub fn in_scope(&self, scope: &[String]) -> Vec<&str> {
        self.files
            .iter()
            .map(String::as_str)
            .filter(|file| in_machine(file, scope))
            .collect()
    }

    /// The full text of the scope files, each under a `=== path ===` heading. A file that is too
    /// large, is not UTF-8, cannot be read, or lies past the byte budget is named without its text.
    pub async fn scope_text(&self, scope: &[String]) -> String {
        let mut text = String::new();
        let mut used = 0;
        for file in self.in_scope(scope) {
            text.push_str(&format!("=== {file} ===\n"));
            let path = self.root.join(file);
            // The size is checked before the read, so a large file is never loaded whole.
            let size = match tokio::fs::metadata(&path).await {
                Ok(meta) => meta.len(),
                Err(err) => {
                    text.push_str(&format!("(not sent: cannot read it: {})\n", err.kind()));
                    continue;
                }
            };
            if size > MAX_SCOPE_FILE_BYTES as u64 {
                text.push_str(&format!(
                    "(not sent: {size} bytes is over the per-file limit)\n"
                ));
                continue;
            }
            let bytes = match tokio::fs::read(&path).await {
                Ok(bytes) => bytes,
                Err(err) => {
                    text.push_str(&format!("(not sent: cannot read it: {})\n", err.kind()));
                    continue;
                }
            };
            let Ok(body) = String::from_utf8(bytes) else {
                text.push_str("(not sent: not UTF-8 text)\n");
                continue;
            };
            if used + body.len() > MAX_SCOPE_BYTES {
                text.push_str("(not sent: the scope text is over its budget)\n");
                continue;
            }
            used += body.len();
            text.push_str(&body);
            if !body.ends_with('\n') {
                text.push('\n');
            }
        }
        text
    }

    /// Checks a plan's paths against the repository.
    ///
    /// Each path in `files` must name a tracked file: exactly, or as the end of one tracked file
    /// that no other tracked file ends the same way. So `server.ts` names `src/server.ts` when that
    /// is the only `server.ts`. Each path in `new_files` must not be tracked, and its directory must
    /// already hold tracked files; a tracked path in `new_files` is taken as an existing file.
    ///
    /// # Errors
    ///
    /// Returns every reason the plan was refused, one per path. A plan that names no file is refused
    /// with one reason.
    pub fn check(&self, files: &[String], new_files: &[String]) -> Result<Checked, Vec<String>> {
        let mut footprint = BTreeSet::new();
        let mut fixes = Vec::new();
        let mut refusals = Vec::new();
        for raw in files {
            let path = match normalize_path(raw) {
                Ok(path) => path,
                Err(reason) => {
                    refusals.push(reason);
                    continue;
                }
            };
            match self.existing(&path) {
                Ok(tracked) => {
                    if tracked != path {
                        fixes.push(format!("resolved {path} to {tracked}"));
                    }
                    footprint.insert(tracked);
                }
                Err(reason) => refusals.push(reason),
            }
        }
        for raw in new_files {
            let path = match normalize_path(raw) {
                Ok(path) => path,
                Err(reason) => {
                    refusals.push(reason);
                    continue;
                }
            };
            if self.is_tracked(&path) {
                fixes.push(format!(
                    "{path} is already tracked; treated as an existing file"
                ));
                footprint.insert(path);
                continue;
            }
            let parent = parent_of(&path);
            if self.has_dir(parent) {
                footprint.insert(path);
            } else {
                refusals.push(format!(
                    "`{path}` is a new file, but its directory `{parent}` is not in the repository; a new file needs a directory the repository already has"
                ));
            }
        }
        if !refusals.is_empty() {
            return Err(refusals);
        }
        if footprint.is_empty() {
            return Err(vec!["the plan names no file".to_owned()]);
        }
        Ok(Checked {
            footprint: footprint.into_iter().collect(),
            fixes,
        })
    }

    /// The tracked file that an existing-file path names. An exact tracked path names itself.
    fn existing(&self, path: &str) -> Result<String, String> {
        if self.is_tracked(path) {
            return Ok(path.to_owned());
        }
        let suffix = format!("/{path}");
        let ends: Vec<&str> = self
            .files
            .iter()
            .map(String::as_str)
            .filter(|file| file.ends_with(&suffix))
            .collect();
        match ends.as_slice() {
            [only] => Ok((*only).to_owned()),
            [] => Err(format!(
                "`{path}` is not a tracked file, and no tracked file ends with that path; name a file from the repository file list, or list a new file under new_files"
            )),
            many => Err(format!(
                "`{path}` matches {} tracked files ({}); name the one you mean in full",
                many.len(),
                many.join(", ")
            )),
        }
    }
}

/// The directory part of a repository-relative path, empty for a file at the root.
fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// Whether a file belongs to one of the scope machines. A machine owns a file when it is one of the
/// file's directories, or the name the file has before its first dot (`src/store.ts` is in `store`,
/// and `test/login.test.ts` is in `test`).
fn in_machine(file: &str, scope: &[String]) -> bool {
    let (dirs, name) = file.rsplit_once('/').unwrap_or(("", file));
    let stem = name.split('.').next().unwrap_or(name);
    scope.iter().any(|machine| {
        machine.as_str() == stem || dirs.split('/').any(|dir| dir == machine.as_str())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(list: &[&str]) -> Vec<String> {
        list.iter().map(|path| (*path).to_owned()).collect()
    }

    fn demo() -> RepoFiles {
        RepoFiles::from_paths(
            "unused",
            paths(&[
                "src/middleware.ts",
                "src/server.ts",
                "src/store.ts",
                "src/ratelimit.ts",
                "src/dashboard.ts",
                "test/login.test.ts",
                "test/helpers.ts",
                "tsconfig.json",
            ]),
        )
    }

    #[test]
    fn an_exact_tracked_path_passes_and_aliases_merge() {
        let checked = demo()
            .check(
                &paths(&["./src/server.ts", "src/server.ts", "src/store.ts"]),
                &[],
            )
            .expect("the paths name tracked files");
        assert_eq!(checked.footprint, paths(&["src/server.ts", "src/store.ts"]));
        assert!(checked.fixes.is_empty());
    }

    #[test]
    fn a_unique_basename_resolves_and_the_fix_is_logged() {
        let checked = demo()
            .check(&paths(&["server.ts", "middleware.ts"]), &[])
            .expect("each basename names one tracked file");
        assert_eq!(
            checked.footprint,
            paths(&["src/middleware.ts", "src/server.ts"])
        );
        assert_eq!(
            checked.fixes,
            paths(&[
                "resolved server.ts to src/server.ts",
                "resolved middleware.ts to src/middleware.ts",
            ])
        );
    }

    #[test]
    fn a_missing_prefix_resolves_when_one_tracked_file_ends_with_it() {
        let repo = RepoFiles::from_paths(
            "unused",
            paths(&["src/middleware/ratelimit.ts", "src/server.ts"]),
        );
        let checked = repo
            .check(&paths(&["middleware/ratelimit.ts"]), &[])
            .expect("one tracked file ends with that path");
        assert_eq!(checked.footprint, paths(&["src/middleware/ratelimit.ts"]));
        assert_eq!(checked.fixes.len(), 1);
    }

    #[test]
    fn an_ambiguous_name_is_refused_with_its_candidates() {
        let repo = RepoFiles::from_paths("unused", paths(&["src/store.ts", "test/store.ts"]));
        let reasons = repo
            .check(&paths(&["store.ts"]), &[])
            .expect_err("two tracked files end with store.ts");
        assert_eq!(reasons.len(), 1);
        assert!(
            reasons[0].contains("src/store.ts") && reasons[0].contains("test/store.ts"),
            "{reasons:?}"
        );
    }

    #[test]
    fn a_wrong_extension_and_an_invented_directory_are_refused() {
        let reasons = demo()
            .check(
                &paths(&["src/middleware.js", "src/ratelimit/index.ts"]),
                &[],
            )
            .expect_err("neither path is tracked");
        assert_eq!(reasons.len(), 2, "{reasons:?}");
        assert!(reasons[0].contains("src/middleware.js"));
        assert!(reasons[1].contains("src/ratelimit/index.ts"));
    }

    #[test]
    fn a_new_file_needs_a_directory_the_repository_already_has() {
        let checked = demo()
            .check(&[], &paths(&["src/rate-limit.ts"]))
            .expect("src is in the repository");
        assert_eq!(checked.footprint, paths(&["src/rate-limit.ts"]));
        let reasons = demo()
            .check(&[], &paths(&["src/ratelimit/index.ts"]))
            .expect_err("src/ratelimit is not a directory in the repository");
        assert!(reasons[0].contains("src/ratelimit"), "{reasons:?}");
    }

    #[test]
    fn a_new_file_already_tracked_is_taken_as_an_existing_file() {
        let checked = demo()
            .check(&[], &paths(&["src/server.ts"]))
            .expect("the path is tracked");
        assert_eq!(checked.footprint, paths(&["src/server.ts"]));
        assert_eq!(checked.fixes.len(), 1);
    }

    #[test]
    fn a_plan_that_names_nothing_or_climbs_out_is_refused() {
        assert_eq!(
            demo().check(&[], &[]),
            Err(vec!["the plan names no file".to_owned()])
        );
        assert!(demo().check(&paths(&["../escape.ts"]), &[]).is_err());
        assert!(demo().check(&paths(&["/etc/passwd"]), &[]).is_err());
    }

    #[test]
    fn the_scope_machines_pick_their_files_by_name_and_directory() {
        let repo = demo();
        assert_eq!(
            repo.in_scope(&paths(&["middleware", "ratelimit", "server"])),
            vec!["src/middleware.ts", "src/ratelimit.ts", "src/server.ts"]
        );
        assert_eq!(
            repo.in_scope(&paths(&["test"])),
            vec!["test/helpers.ts", "test/login.test.ts"]
        );
    }

    #[test]
    fn a_machine_routes_through_its_own_module_file_and_a_directory_machine_through_none() {
        let repo = demo();
        assert_eq!(
            repo.routed(&paths(&["middleware", "server", "test"])),
            vec!["src/middleware.ts", "src/server.ts"]
        );
    }
}
