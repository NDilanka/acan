//! The once-per-start check for a newer build on GitHub Releases.
//!
//! The check runs in the background, so startup never waits for it. A failed check is a status,
//! not an error: an offline machine shows `offline` and is checked again on the next start. The
//! list is read from the releases endpoint and not `releases/latest`, because `latest` skips
//! pre-releases, and every alpha is one.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use semver::Version;
use serde::{Deserialize, Serialize};

use super::{set_update_status, version};

const RELEASES_API: &str = "https://api.github.com/repos/NDilanka/acan/releases?per_page=10";
const RELEASES_PAGE: &str = "https://github.com/NDilanka/acan/releases";
const RELEASE_URL_PREFIX: &str = "https://github.com/NDilanka/acan/releases/tag/";

/// How long the whole check may take before it counts as offline.
const TIMEOUT: Duration = Duration::from_secs(8);

/// Where the check stands. `state` is one of `off`, `checking`, `current`, `available` or
/// `offline`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateStatus {
    pub state: UpdateState,
    pub current: &'static str,
    /// The newest release found, when the check reached GitHub.
    pub latest: Option<String>,
    /// The page of the newest release, when one was found.
    pub url: Option<String>,
    /// When the check finished, in seconds since the Unix epoch.
    pub checked_at: Option<u64>,
}

impl UpdateStatus {
    /// The status before any check runs. Usable in a static.
    #[must_use]
    pub const fn off() -> Self {
        Self {
            state: UpdateState::Off,
            current: "",
            latest: None,
            url: None,
            checked_at: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateState {
    /// The check is turned off, by `--no-update-check` or `ACAN_UPDATE_CHECK=0`.
    Off,
    /// The check is running.
    Checking,
    /// This build is the newest release found.
    Current,
    /// A newer release exists.
    Available,
    /// GitHub could not be reached, or it did not answer usefully.
    Offline,
}

/// The fields of a release the check reads.
#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    draft: bool,
}

/// Starts the check in the background and returns at once. Call it once per start, after the
/// Tokio runtime is running. Nothing else starts a check.
pub fn spawn_update_check() {
    let current = version();
    set_update_status(UpdateStatus {
        state: UpdateState::Checking,
        current,
        latest: None,
        url: None,
        checked_at: None,
    });
    tokio::spawn(async move {
        let status = check(current).await;
        tracing::info!(state = ?status.state, latest = ?status.latest, "update check finished");
        set_update_status(status);
    });
}

async fn check(current: &'static str) -> UpdateStatus {
    let releases = fetch_releases(current).await;
    let checked_at = unix_now();
    let Some(releases) = releases else {
        return UpdateStatus {
            state: UpdateState::Offline,
            current,
            latest: None,
            url: None,
            checked_at: Some(checked_at),
        };
    };
    let newest = newest_release(&releases);
    let newer = match (Version::parse(current), &newest) {
        (Ok(running), Some((latest, _))) => *latest > running,
        _ => false,
    };
    let (latest, url) = match newest {
        Some((version, release)) => (
            Some(version.to_string()),
            Some(release_page(&release.html_url)),
        ),
        None => (None, None),
    };
    UpdateStatus {
        state: if newer {
            UpdateState::Available
        } else {
            UpdateState::Current
        },
        current,
        latest,
        url,
        checked_at: Some(checked_at),
    }
}

/// The release list, or `None` when GitHub cannot be reached or answers with an error. The
/// reason goes to the log.
async fn fetch_releases(current: &str) -> Option<Vec<Release>> {
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .user_agent(format!("acan/{current}"))
        .build()
        .ok()?;
    let response = match client
        .get(RELEASES_API)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
    {
        Ok(response) => response,
        Err(err) => {
            tracing::info!(error = %err, "update check could not reach GitHub");
            return None;
        }
    };
    if !response.status().is_success() {
        tracing::info!(status = %response.status(), "update check got an error from GitHub");
        return None;
    }
    match response.json::<Vec<Release>>().await {
        Ok(releases) => Some(releases),
        Err(err) => {
            tracing::info!(error = %err, "update check could not read the release list");
            None
        }
    }
}

/// The published release with the highest version. Drafts and tags that are not versions are
/// skipped, and a leading `v` is allowed.
fn newest_release(releases: &[Release]) -> Option<(Version, &Release)> {
    releases
        .iter()
        .filter(|release| !release.draft)
        .filter_map(|release| {
            let version = Version::parse(release.tag_name.trim_start_matches('v')).ok()?;
            Some((version, release))
        })
        .max_by(|(a, _), (b, _)| a.cmp(b))
}

/// The release page to show, which must be on this project's GitHub. Anything else falls back to
/// the releases list.
fn release_page(html_url: &str) -> String {
    if html_url.starts_with(RELEASE_URL_PREFIX) {
        html_url.to_owned()
    } else {
        RELEASES_PAGE.to_owned()
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, draft: bool) -> Release {
        Release {
            tag_name: tag.to_owned(),
            html_url: format!("{RELEASE_URL_PREFIX}{tag}"),
            draft,
        }
    }

    #[test]
    fn the_newest_published_version_wins_whatever_its_list_position() {
        let releases = vec![
            release("v0.1.0-alpha.2", false),
            release("v0.2.0-alpha.1", true),
            release("v0.1.0", false),
            release("v0.1.0-alpha.10", false),
            release("nightly", false),
        ];
        let (version, found) = newest_release(&releases).expect("a version is found");
        assert_eq!(version, Version::parse("0.1.0").expect("valid"));
        assert_eq!(found.tag_name, "v0.1.0");
    }

    #[test]
    fn a_prerelease_is_older_than_its_release() {
        let alpha = Version::parse("0.1.0-alpha.2").expect("valid");
        let release = Version::parse("0.1.0").expect("valid");
        assert!(alpha < release);
    }

    #[test]
    fn only_this_projects_release_pages_are_linked() {
        let own = format!("{RELEASE_URL_PREFIX}v0.1.0-alpha.2");
        assert_eq!(release_page(&own), own);
        assert_eq!(release_page("https://example.com/evil"), RELEASES_PAGE);
    }

    #[test]
    fn the_off_status_has_no_result() {
        let status = UpdateStatus::off();
        assert_eq!(status.state, UpdateState::Off);
        assert!(status.latest.is_none());
        assert!(status.checked_at.is_none());
    }
}
