//! The in-app updater. The owner asks for the newest pre-release on GitHub, and the install takes
//! its asset from the `FLAVOR` file beside the program: `app` or `demo`. The download is checked
//! against the release's `SHA256SUMS` before anything runs. On Windows the verified setup then
//! runs detached and silently. On Linux the verified archive is kept, and its path is reported.

use std::path::{Path, PathBuf};
use std::time::Duration;

use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The GitHub API root. Tests point a [`Github`] at a mock server instead.
const GITHUB_API: &str = "https://api.github.com";
/// The repository whose releases are checked.
const REPO: &str = "NDilanka/acan";
/// The checksum file every release ships.
const SUMS_NAME: &str = "SHA256SUMS";
/// The file the packaging script writes beside the program.
const FLAVOR_FILE: &str = "FLAVOR";
/// The folder under the system temp folder where a verified download is kept.
const DOWNLOAD_DIR: &str = "acan-update";
/// How long one request may take. A setup is tens of megabytes, so this is generous.
const TIMEOUT: Duration = Duration::from_secs(300);
/// The arguments of the silent Windows setup.
#[cfg(any(windows, test))]
const SETUP_ARGS: [&str; 3] = ["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"];

/// Which build this install is: the app, or the demo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Flavor {
    App,
    Demo,
}

impl Flavor {
    fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "app" => Some(Self::App),
            "demo" => Some(Self::Demo),
            _ => None,
        }
    }

    /// The product name the asset file names start with.
    fn product(self) -> &'static str {
        match self {
            Self::App => "acan",
            Self::Demo => "acan-demo",
        }
    }
}

/// Why an update could not be found, downloaded or checked. Each names its cause and never a
/// path or a token.
#[derive(Debug)]
pub(crate) enum UpdateError {
    /// The `FLAVOR` file is missing, or names neither app nor demo.
    NoFlavor,
    /// GitHub could not be reached, or did not answer usefully.
    Offline,
    /// No published pre-release exists.
    NoPrerelease,
    /// The pre-release has no asset for this install. The name goes to the log.
    NoAsset,
    /// The download does not match `SHA256SUMS`, or the sums do not list it. Nothing runs.
    Checksum,
    /// The verified download could not be written.
    Storage(std::io::Error),
}

/// The check's result. `state` is `current`, `available`, `offline` or `no_prerelease`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Check {
    pub(crate) state: &'static str,
    pub(crate) current: &'static str,
    pub(crate) latest: Option<String>,
    pub(crate) url: Option<String>,
}

/// A verified download: where it was saved, and the version it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Download {
    pub(crate) path: PathBuf,
    pub(crate) version: String,
}

/// The fields of a release the updater reads.
#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

/// The GitHub client. It talks to one API root, which is GitHub in production.
pub(crate) struct Github {
    api: String,
    client: reqwest::Client,
}

impl Github {
    /// The client for GitHub itself.
    pub(crate) fn production() -> Self {
        Self::at(GITHUB_API)
    }

    /// The client for the API root `api`, such as a mock server in tests.
    pub(crate) fn at(api: &str) -> Self {
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(format!("acan/{}", crate::release::version()))
            .build()
            .unwrap_or_default();
        Self {
            api: api.trim_end_matches('/').to_owned(),
            client,
        }
    }

    /// The newest releases of the repository, in the order GitHub lists them.
    async fn releases(&self) -> Result<Vec<Release>, UpdateError> {
        let url = format!("{}/repos/{REPO}/releases?per_page=10", self.api);
        let response = self
            .client
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|err| {
                tracing::info!(error = %err, "the update check could not reach GitHub");
                UpdateError::Offline
            })?;
        if !response.status().is_success() {
            tracing::info!(status = %response.status(), "the update check got an error from GitHub");
            return Err(UpdateError::Offline);
        }
        response.json::<Vec<Release>>().await.map_err(|err| {
            tracing::info!(error = %err, "the update check could not read the release list");
            UpdateError::Offline
        })
    }

    /// The bytes at `url`, which must answer with success.
    async fn bytes(&self, url: &str) -> Result<Vec<u8>, UpdateError> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|err| {
                tracing::info!(error = %err, "an update download failed");
                UpdateError::Offline
            })?;
        let bytes = response.bytes().await.map_err(|err| {
            tracing::info!(error = %err, "an update download could not be read");
            UpdateError::Offline
        })?;
        Ok(bytes.to_vec())
    }
}

/// The version of the running build, and the newest published pre-release on GitHub. It downloads
/// nothing. GitHub being unreachable is a state, not an error.
pub(crate) async fn check(github: &Github) -> Check {
    let current = crate::release::version();
    let releases = match github.releases().await {
        Ok(releases) => releases,
        Err(_) => return check_without_release(current, "offline"),
    };
    let Some((latest, release)) = newest_prerelease(&releases) else {
        return check_without_release(current, "no_prerelease");
    };
    let newer = Version::parse(current).is_ok_and(|running| latest > running);
    Check {
        state: if newer { "available" } else { "current" },
        current,
        latest: Some(latest.to_string()),
        url: Some(release_page(&release.tag_name)),
    }
}

fn check_without_release(current: &'static str, state: &'static str) -> Check {
    Check {
        state,
        current,
        latest: None,
        url: None,
    }
}

/// The published pre-release with the highest version. Drafts, stable releases and tags that are
/// not versions are skipped, and a leading `v` is allowed.
fn newest_prerelease(releases: &[Release]) -> Option<(Version, &Release)> {
    releases
        .iter()
        .filter(|release| release.prerelease && !release.draft)
        .filter_map(|release| {
            let version = Version::parse(release.tag_name.trim_start_matches('v')).ok()?;
            Some((version, release))
        })
        .max_by(|(a, _), (b, _)| a.cmp(b))
}

fn release_page(tag: &str) -> String {
    format!("https://github.com/{REPO}/releases/tag/{tag}")
}

/// The folder the `FLAVOR` file is read from: the folder of the running program.
pub(crate) fn install_dir() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(exe.parent().map(Path::to_path_buf).unwrap_or_default())
}

/// The folder a verified download is kept in.
pub(crate) fn download_dir() -> PathBuf {
    std::env::temp_dir().join(DOWNLOAD_DIR)
}

/// The flavor the install in `dir` is, from its `FLAVOR` file.
pub(crate) fn flavor_in(dir: &Path) -> Result<Flavor, UpdateError> {
    let text = std::fs::read_to_string(dir.join(FLAVOR_FILE)).map_err(|_| UpdateError::NoFlavor)?;
    Flavor::parse(&text).ok_or(UpdateError::NoFlavor)
}

/// The asset file name of `flavor` at `version`, for Windows or Linux. These are the names the
/// packaging scripts and the Inno Setup scripts produce.
fn asset_name(flavor: Flavor, version: &str, windows: bool) -> String {
    if windows {
        format!("{}-setup-{version}-windows-x64.exe", flavor.product())
    } else {
        format!("{}-{version}-linux-x64.tar.gz", flavor.product())
    }
}

/// The hash `SHA256SUMS` lists for the file `name`, lower case. A line names its file after the
/// hash, optionally with a `*` before the name.
fn expected_sum(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let file = parts.next()?.trim_start_matches('*');
        (file == name).then(|| hash.to_ascii_lowercase())
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn asset_url(release: &Release, name: &str) -> Result<String, UpdateError> {
    release
        .assets
        .iter()
        .find(|asset| asset.name == name)
        .map(|asset| asset.browser_download_url.clone())
        .ok_or_else(|| {
            tracing::info!(asset = name, "the release has no asset for this install");
            UpdateError::NoAsset
        })
}

/// Downloads the newest pre-release's asset for `flavor` into `dir`, and keeps it only when its
/// SHA256 matches the release's `SHA256SUMS`. A mismatch, or a sums file that does not list the
/// asset, is refused and writes nothing.
pub(crate) async fn download_latest(
    github: &Github,
    flavor: Flavor,
    dir: &Path,
    windows: bool,
) -> Result<Download, UpdateError> {
    let releases = github.releases().await?;
    let Some((latest, release)) = newest_prerelease(&releases) else {
        return Err(UpdateError::NoPrerelease);
    };
    let version = latest.to_string();
    let name = asset_name(flavor, &version, windows);
    let asset = asset_url(release, &name)?;
    let sums = github.bytes(&asset_url(release, SUMS_NAME)?).await?;
    let sums = String::from_utf8_lossy(&sums);
    let expected = expected_sum(&sums, &name).ok_or(UpdateError::Checksum)?;
    let bytes = github.bytes(&asset).await?;
    if sha256_hex(&bytes) != expected {
        return Err(UpdateError::Checksum);
    }
    std::fs::create_dir_all(dir).map_err(UpdateError::Storage)?;
    let path = dir.join(&name);
    std::fs::write(&path, &bytes).map_err(UpdateError::Storage)?;
    Ok(Download { path, version })
}

/// Starts the verified Windows setup with the silent flags, detached from this process. The child
/// is not waited for, so the server answers at once and the setup keeps running after it returns.
#[cfg(windows)]
pub(crate) fn run_setup_detached(setup: &Path) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    Command::new(setup)
        .args(SETUP_ARGS)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .map(drop)
}

/// The Windows setup runs only on Windows.
#[cfg(not(windows))]
pub(crate) fn run_setup_detached(_setup: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "the setup runs on Windows only",
    ))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn releases_of(value: Value) -> Vec<Release> {
        serde_json::from_value(value).expect("the releases parse")
    }

    fn release_json(tag: &str, prerelease: bool, draft: bool, assets: Value) -> Value {
        json!({"tag_name": tag, "prerelease": prerelease, "draft": draft, "assets": assets})
    }

    #[test]
    fn the_newest_prerelease_wins_and_stable_or_draft_releases_are_skipped() {
        let releases = releases_of(json!([
            release_json("v0.1.0-pre.2", true, false, json!([])),
            release_json("v9.0.0", false, false, json!([])),
            release_json("v9.1.0-pre.1", true, true, json!([])),
            release_json("v0.1.0-pre.10", true, false, json!([])),
            release_json("nightly", true, false, json!([])),
        ]));

        let (version, release) = newest_prerelease(&releases).expect("a pre-release");

        assert_eq!(version.to_string(), "0.1.0-pre.10");
        assert_eq!(release.tag_name, "v0.1.0-pre.10");
    }

    #[test]
    fn flavor_is_app_or_demo_and_anything_else_is_refused() {
        assert_eq!(Flavor::parse("app\n"), Some(Flavor::App));
        assert_eq!(Flavor::parse(" demo "), Some(Flavor::Demo));
        assert_eq!(Flavor::parse("other"), None);
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(matches!(flavor_in(dir.path()), Err(UpdateError::NoFlavor)));
        std::fs::write(dir.path().join(FLAVOR_FILE), "demo").expect("the flavor is written");
        assert_eq!(flavor_in(dir.path()).ok(), Some(Flavor::Demo));
    }

    #[test]
    fn asset_names_follow_the_packaging_names() {
        assert_eq!(
            asset_name(Flavor::App, "0.1.0-pre.1", true),
            "acan-setup-0.1.0-pre.1-windows-x64.exe"
        );
        assert_eq!(
            asset_name(Flavor::Demo, "0.1.0-pre.1", true),
            "acan-demo-setup-0.1.0-pre.1-windows-x64.exe"
        );
        assert_eq!(
            asset_name(Flavor::App, "0.1.0-pre.1", false),
            "acan-0.1.0-pre.1-linux-x64.tar.gz"
        );
        assert_eq!(
            asset_name(Flavor::Demo, "0.1.0-pre.1", false),
            "acan-demo-0.1.0-pre.1-linux-x64.tar.gz"
        );
    }

    /// The release workflow names every file it publishes. The updater asks for the same names, so
    /// an update never finds no asset after a rename. The workflow is read when the tests build.
    #[test]
    fn the_asset_names_are_the_ones_the_release_workflow_publishes() {
        let workflow = include_str!("../../../../.github/workflows/release.yml");
        let version = "0.1.0-pre.1";
        for flavor in [Flavor::App, Flavor::Demo] {
            for windows in [true, false] {
                let template = asset_name(flavor, version, windows).replace(version, "$VERSION");
                assert!(
                    workflow.contains(&template),
                    "the release workflow does not publish {template}"
                );
            }
        }
        assert!(
            workflow.contains(&format!("archives/{SUMS_NAME}")),
            "the release workflow does not upload {SUMS_NAME}"
        );
    }

    #[test]
    fn the_sums_line_is_found_by_its_exact_name() {
        let sums = "ABCDEF  acan-setup-1.exe\n0123 *acan-setup-1.exe.sig\n";

        assert_eq!(
            expected_sum(sums, "acan-setup-1.exe").as_deref(),
            Some("abcdef")
        );
        assert_eq!(expected_sum(sums, "acan-setup-1.ex"), None);
    }

    #[test]
    fn the_silent_setup_takes_the_three_flags() {
        assert_eq!(
            SETUP_ARGS,
            ["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"]
        );
    }

    async fn mount_releases(server: &MockServer, body: Value) {
        Mock::given(method("GET"))
            .and(path(format!("/repos/{REPO}/releases")))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn a_verified_download_is_kept_and_a_mismatch_is_refused() {
        let server = MockServer::start().await;
        let setup = b"the setup bytes";
        let name = "acan-setup-0.1.0-pre.1-windows-x64.exe";
        let good = sha256_hex(setup);
        mount_releases(
            &server,
            json!([release_json(
                "v0.1.0-pre.1",
                true,
                false,
                json!([
                    {"name": name, "browser_download_url": format!("{}/dl/setup", server.uri())},
                    {"name": SUMS_NAME, "browser_download_url": format!("{}/dl/sums", server.uri())},
                ]),
            )]),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/dl/setup"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(setup.to_vec()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/dl/sums"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!("{good}  {name}\n")))
            .mount(&server)
            .await;
        let github = Github::at(&server.uri());
        let dir = tempfile::tempdir().expect("temp dir");

        let download = download_latest(&github, Flavor::App, dir.path(), true)
            .await
            .expect("the verified setup is kept");

        assert_eq!(download.version, "0.1.0-pre.1");
        assert_eq!(std::fs::read(&download.path).expect("the file"), setup);

        let other = tempfile::tempdir().expect("temp dir");
        server.reset().await;
        mount_releases(
            &server,
            json!([release_json(
                "v0.1.0-pre.1",
                true,
                false,
                json!([
                    {"name": name, "browser_download_url": format!("{}/dl/setup", server.uri())},
                    {"name": SUMS_NAME, "browser_download_url": format!("{}/dl/sums", server.uri())},
                ]),
            )]),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/dl/setup"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"tampered".to_vec()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/dl/sums"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!("{good}  {name}\n")))
            .mount(&server)
            .await;

        let refused = download_latest(&github, Flavor::App, other.path(), true).await;

        assert!(matches!(refused, Err(UpdateError::Checksum)));
        assert!(!other.path().join(name).exists(), "nothing is written");
    }

    #[tokio::test]
    async fn the_check_reports_a_newer_prerelease_and_offline_when_github_fails() {
        let server = MockServer::start().await;
        mount_releases(
            &server,
            json!([release_json("v99.0.0-pre.1", true, false, json!([]))]),
        )
        .await;
        let check = super::check(&Github::at(&server.uri())).await;
        assert_eq!(check.state, "available");
        assert_eq!(check.latest.as_deref(), Some("99.0.0-pre.1"));
        assert_eq!(
            check.url.as_deref(),
            Some("https://github.com/NDilanka/acan/releases/tag/v99.0.0-pre.1")
        );

        let down = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&down)
            .await;
        let offline = super::check(&Github::at(&down.uri())).await;
        assert_eq!(offline.state, "offline");
        assert_eq!(offline.latest, None);
    }
}
