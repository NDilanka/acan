//! The projects one `acan serve` process serves.
//!
//! A project is one repository with its own world, state directory, tokens, gate and protected
//! branch. Its id is the URL segment under `/api/p/`, so it is a short slug that stays the same
//! while the server runs. A projects file pins the ids. Without one, each `--repo` takes an id
//! from its folder name.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail, ensure};
use serde::Deserialize;

use crate::STATE_DIR;

/// The longest id a projects file may give. Long enough for any folder name a slug is made from,
/// with room for a `-2` suffix.
const MAX_ID_LEN: usize = 40;
/// The longest slug made from a folder name. Shorter than [`MAX_ID_LEN`], so a suffix still fits.
const MAX_SLUG_LEN: usize = 32;
/// The id a folder name becomes when it has no letters or digits.
const FALLBACK_ID: &str = "project";

/// One project as the server is told about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSpec {
    /// The URL segment and the key every API and log line uses for this project.
    pub id: String,
    /// The name the UI shows.
    pub name: String,
    /// The repository whose world this project serves.
    pub repo: PathBuf,
    /// The branch finished work lands on. `None` lands on the branch checked out in the repository.
    pub protected_branch: Option<String>,
    /// The model routing file. `None` means `<repo>/.acan/llm.toml`. The command line reads it;
    /// the server does not.
    pub llm: Option<PathBuf>,
    /// The base URL HTTP witness checks target for this project.
    pub witness_http_base_url: Option<String>,
}

/// The projects of one server, and the one the unprefixed routes serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSet {
    /// The id the unprefixed routes map to.
    pub default: String,
    /// Every project, in the order the UI lists them.
    pub projects: Vec<ProjectSpec>,
}

/// The projects file: which project is the default, and the projects.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    default: Option<String>,
    #[serde(default)]
    project: Vec<FileProject>,
}

/// One `[[project]]` table of the projects file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileProject {
    id: String,
    name: Option<String>,
    repo: PathBuf,
    protected_branch: Option<String>,
    llm: Option<PathBuf>,
    witness_http_base_url: Option<String>,
}

impl ProjectSet {
    /// One project per repository, with ids from folder names. The first repository is the
    /// default. A repeated folder name gets `-2`, `-3`, and so on.
    #[must_use]
    pub fn from_repos(repos: &[PathBuf]) -> Self {
        let mut taken = BTreeSet::new();
        let projects: Vec<ProjectSpec> = repos
            .iter()
            .map(|repo| {
                let name = folder_name(repo);
                let id = unique(slug(&name), &mut taken);
                ProjectSpec {
                    id,
                    name,
                    repo: repo.clone(),
                    protected_branch: None,
                    llm: None,
                    witness_http_base_url: None,
                }
            })
            .collect();
        let default = projects
            .first()
            .map(|project| project.id.clone())
            .unwrap_or_default();
        Self { default, projects }
    }

    /// Reads a projects file. Relative paths in it are relative to the file's folder. Without a
    /// `default` key, the first project is the default.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read or parsed, or has a key it does not know.
    pub fn from_file(path: &Path) -> anyhow::Result<Self> {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let file: File =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let base = path.parent().unwrap_or_else(|| Path::new(""));
        let projects: Vec<ProjectSpec> = file
            .project
            .into_iter()
            .map(|entry| ProjectSpec {
                name: entry.name.unwrap_or_else(|| entry.id.clone()),
                id: entry.id,
                repo: base.join(entry.repo),
                protected_branch: entry.protected_branch,
                llm: entry.llm.map(|llm| base.join(llm)),
                witness_http_base_url: entry.witness_http_base_url,
            })
            .collect();
        let default = file
            .default
            .or_else(|| projects.first().map(|project| project.id.clone()))
            .unwrap_or_default();
        Ok(Self { default, projects })
    }

    /// Checks the set before anything opens. Ids must be valid and unique, and the default must
    /// be one of them. Every repository must exist, must not be another project's repository, and
    /// must not sit inside another project's state directory.
    ///
    /// # Errors
    ///
    /// Returns an error naming the first problem found.
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            !self.projects.is_empty(),
            "no project to serve: give --repo or --projects"
        );
        let mut ids = BTreeSet::new();
        for project in &self.projects {
            ensure!(
                valid_id(&project.id),
                "project id {:?} must be 1 to {MAX_ID_LEN} lowercase letters, digits or dashes, and must not start or end with a dash",
                project.id
            );
            ensure!(
                ids.insert(project.id.as_str()),
                "project id {:?} is listed more than once",
                project.id
            );
            if let Some(branch) = &project.protected_branch {
                ensure!(
                    !branch.trim().is_empty(),
                    "project {:?} has an empty protected branch",
                    project.id
                );
            }
        }
        ensure!(
            ids.contains(self.default.as_str()),
            "the default project {:?} is not one of the projects",
            self.default
        );

        let mut repos: Vec<(PathBuf, &str)> = Vec::with_capacity(self.projects.len());
        for project in &self.projects {
            let repo = project.repo.canonicalize().with_context(|| {
                format!(
                    "opening the repository of project {:?}: {}",
                    project.id,
                    project.repo.display()
                )
            })?;
            ensure!(
                repo.is_dir(),
                "the repository of project {:?} is not a folder: {}",
                project.id,
                project.repo.display()
            );
            if let Some((_, other)) = repos.iter().find(|(path, _)| *path == repo) {
                bail!(
                    "projects {other:?} and {:?} name the same repository",
                    project.id
                );
            }
            repos.push((repo, project.id.as_str()));
        }
        for (repo, id) in &repos {
            for (other, other_id) in &repos {
                ensure!(
                    !repo.starts_with(other.join(STATE_DIR)) || repo == other,
                    "project {id:?} sits inside the state directory of project {other_id:?}"
                );
            }
        }
        Ok(())
    }
}

/// Whether `id` may be a project id: 1 to [`MAX_ID_LEN`] lowercase letters, digits or dashes,
/// with no dash at either end.
#[must_use]
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && !id.starts_with('-')
        && !id.ends_with('-')
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// The id a name becomes: its ASCII letters and digits in lowercase, each run of anything else
/// as one dash. A name with none becomes `project`.
#[must_use]
pub fn slug(name: &str) -> String {
    let mut id = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            id.push(c.to_ascii_lowercase());
        } else if !id.is_empty() && !id.ends_with('-') {
            id.push('-');
        }
    }
    id.truncate(MAX_SLUG_LEN);
    while id.ends_with('-') {
        id.pop();
    }
    if id.is_empty() {
        FALLBACK_ID.to_owned()
    } else {
        id
    }
}

/// `base`, or `base-2`, `base-3` and so on, whichever is not taken yet. Records the id as taken.
fn unique(base: String, taken: &mut BTreeSet<String>) -> String {
    let mut id = base.clone();
    let mut n = 2;
    while !taken.insert(id.clone()) {
        id = format!("{base}-{n}");
        n += 1;
    }
    id
}

/// The last folder of `repo`, made absolute first so `.` names its folder.
fn folder_name(repo: &Path) -> String {
    let absolute = std::path::absolute(repo).unwrap_or_else(|_| repo.to_path_buf());
    absolute
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::{ProjectSet, slug, valid_id};

    fn repo(root: &std::path::Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).expect("the repository folder");
        dir
    }

    #[test]
    fn a_folder_name_becomes_a_slug() {
        assert_eq!(slug("My Repo_2"), "my-repo-2");
        assert_eq!(slug("Acan"), "acan");
        assert_eq!(slug("--a..b--"), "a-b");
        assert_eq!(slug("---"), "project");
        assert_eq!(slug(&"x".repeat(60)).len(), 32);
    }

    #[test]
    fn only_lowercase_slugs_are_valid_ids() {
        assert!(valid_id("acan-2"));
        assert!(!valid_id(""));
        assert!(!valid_id("Acan"));
        assert!(!valid_id("-acan"));
        assert!(!valid_id("acan-"));
        assert!(!valid_id("a/b"));
        assert!(!valid_id(&"a".repeat(41)));
    }

    #[test]
    fn repeated_folder_names_get_a_suffix() {
        let root = tempfile::tempdir().expect("temp dir");
        let repos = [repo(root.path(), "a/app"), repo(root.path(), "b/app")];
        let set = ProjectSet::from_repos(&repos);
        let ids: Vec<&str> = set.projects.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["app", "app-2"]);
        assert_eq!(set.default, "app");
        assert_eq!(set.projects[0].name, "app");
        set.validate().expect("two distinct repositories are valid");
    }

    #[test]
    fn a_projects_file_resolves_paths_from_its_own_folder() {
        let root = tempfile::tempdir().expect("temp dir");
        repo(root.path(), "shop");
        fs::write(
            root.path().join("acan-projects.toml"),
            "default = \"shop\"\n\n[[project]]\nid = \"acan\"\nrepo = \"acan\"\n\n[[project]]\nid = \"shop\"\nname = \"Shop\"\nrepo = \"shop\"\nprotected_branch = \"main\"\n",
        )
        .expect("the projects file");
        let set = ProjectSet::from_file(&root.path().join("acan-projects.toml")).expect("parses");
        assert_eq!(set.default, "shop");
        assert_eq!(set.projects[0].name, "acan", "name defaults to the id");
        assert_eq!(set.projects[1].name, "Shop");
        assert_eq!(set.projects[1].repo, root.path().join("shop"));
        assert_eq!(set.projects[1].protected_branch.as_deref(), Some("main"));
    }

    #[test]
    fn a_projects_file_with_an_unknown_key_is_refused() {
        let root = tempfile::tempdir().expect("temp dir");
        let path = root.path().join("acan-projects.toml");
        fs::write(
            &path,
            "[[project]]\nid = \"acan\"\nrepo = \".\"\nprotected_brach = \"main\"\n",
        )
        .expect("the projects file");
        let err = ProjectSet::from_file(&path).expect_err("a misspelled key is an error");
        assert!(format!("{err:#}").contains("protected_brach"), "{err:#}");
    }

    #[test]
    fn validation_refuses_bad_or_duplicate_ids() {
        let root = tempfile::tempdir().expect("temp dir");
        let a = repo(root.path(), "a");
        let b = repo(root.path(), "b");
        let mut set = ProjectSet::from_repos(&[a.clone(), b.clone()]);
        set.projects[0].id = "Acan".to_owned();
        set.default = "Acan".to_owned();
        assert!(set.validate().is_err(), "an uppercase id is refused");

        let mut set = ProjectSet::from_repos(&[a.clone(), b]);
        set.projects[1].id = set.projects[0].id.clone();
        let err = set.validate().expect_err("a duplicate id is refused");
        assert!(err.to_string().contains("more than once"), "{err}");
    }

    #[test]
    fn validation_refuses_an_unknown_default() {
        let root = tempfile::tempdir().expect("temp dir");
        let mut set = ProjectSet::from_repos(&[repo(root.path(), "a")]);
        set.default = "nope".to_owned();
        assert!(set.validate().is_err());
    }

    #[test]
    fn validation_refuses_the_same_repository_twice() {
        let root = tempfile::tempdir().expect("temp dir");
        let a = repo(root.path(), "a");
        let again = root.path().join(".").join("a");
        let set = ProjectSet::from_repos(&[a, again]);
        let err = set.validate().expect_err("one repository, two projects");
        assert!(err.to_string().contains("same repository"), "{err}");
    }

    #[test]
    fn validation_refuses_a_repository_inside_another_state_directory() {
        let root = tempfile::tempdir().expect("temp dir");
        let a = repo(root.path(), "a");
        let inner = a.join(".acan").join("inner");
        fs::create_dir_all(&inner).expect("the nested folder");
        let set = ProjectSet::from_repos(&[a, inner]);
        let err = set.validate().expect_err("inside .acan is refused");
        assert!(err.to_string().contains("state directory"), "{err}");
    }

    #[test]
    fn validation_refuses_a_missing_repository() {
        let root = tempfile::tempdir().expect("temp dir");
        let set = ProjectSet::from_repos(&[root.path().join("missing")]);
        assert!(set.validate().is_err());
    }

    #[test]
    fn validation_refuses_an_empty_set() {
        let set = ProjectSet::from_repos(&[]);
        assert!(set.validate().is_err());
    }
}
