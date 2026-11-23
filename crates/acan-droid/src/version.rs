use std::fmt;
use std::str::FromStr;

use semver::{Version, VersionReq};
use serde::Deserialize;

use crate::DroidError;

/// The semantic version reported by `droid --version`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DroidVersion(Version);

impl FromStr for DroidVersion {
    type Err = DroidError;

    fn from_str(output: &str) -> Result<Self, Self::Err> {
        let mut version = output.trim();
        if let Some(rest) = version
            .strip_prefix("droid ")
            .or_else(|| version.strip_prefix("Droid "))
        {
            version = rest.trim_start();
            version = version.strip_prefix("CLI ").unwrap_or(version);
            version = version.strip_prefix("version ").unwrap_or(version);
        }
        Ok(Self(Version::parse(
            version.strip_prefix('v').unwrap_or(version),
        )?))
    }
}

impl fmt::Display for DroidVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The CLI versions a caller allows the client to start.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VersionPolicy(VersionReq);

impl Default for VersionPolicy {
    fn default() -> Self {
        ">=1.10.1, <1.16.0"
            .parse()
            .expect("the default Droid range is a valid version requirement")
    }
}

impl FromStr for VersionPolicy {
    type Err = semver::Error;

    fn from_str(requirement: &str) -> Result<Self, Self::Err> {
        requirement.parse().map(Self)
    }
}

impl fmt::Display for VersionPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl VersionPolicy {
    /// Checks the reported version before starting a session.
    ///
    /// # Errors
    ///
    /// Returns the detected version and allowed range if the version is not supported.
    pub fn check(&self, version: &DroidVersion) -> Result<(), DroidError> {
        if self.0.matches(&version.0) {
            Ok(())
        } else {
            Err(DroidError::UnsupportedVersion {
                version: version.clone(),
                policy: self.clone(),
            })
        }
    }
}

/// The result of one authentication check, without its potentially identifying details.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[non_exhaustive]
pub struct AuthCheck {
    /// Droid's stable check identifier, such as `auth.verify`.
    pub id: String,
    /// The wire status, such as `pass`, `fail`, or `skip`.
    pub status: String,
}

/// Authentication status reported by Droid's doctor command.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct AuthStatus {
    /// Whether the doctor report passed and did not report a failed auth check.
    pub authenticated: bool,
    /// Just the authentication checks, excluding environment details.
    pub checks: Vec<AuthCheck>,
}

/// Parses `droid doctor --checks auth --json` without retaining details or credential data.
///
/// # Errors
///
/// Returns an error for invalid JSON or a report that contains no authentication checks.
pub fn parse_doctor_auth(json: &str) -> Result<AuthStatus, DroidError> {
    #[derive(Deserialize)]
    struct Report {
        ok: bool,
        results: Vec<AuthCheck>,
    }

    let report: Report = serde_json::from_str(json)?;
    let checks: Vec<_> = report
        .results
        .into_iter()
        .filter(|check| check.id.starts_with("auth."))
        .collect();
    if checks.is_empty() {
        return Err(DroidError::Protocol("doctor reported no auth checks"));
    }
    Ok(AuthStatus {
        authenticated: report.ok
            && checks.iter().any(|check| check.status == "pass")
            && checks
                .iter()
                .all(|check| matches!(check.status.as_str(), "pass" | "skip")),
        checks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tested_version_range() {
        let policy = VersionPolicy::default();
        for version in ["1.10.1", "1.15.0", "1.15.99", "Droid CLI v1.13.0+build"] {
            let version: DroidVersion = version.parse().expect("parse supported version");
            assert!(policy.check(&version).is_ok(), "refused {version}");
        }
        for version in ["0.244.0", "1.10.0", "1.15.0-rc.1", "1.16.0", "2.0.0"] {
            let version: DroidVersion = version.parse().expect("parse unsupported version");
            let error = policy.check(&version).expect_err("refuse untested version");
            assert!(error.to_string().contains(&version.to_string()));
        }
        assert!(
            policy
                .check(&"1.10.1".parse().expect("parse version"))
                .is_ok()
        );
        assert!("not a version".parse::<DroidVersion>().is_err());
        let override_policy: VersionPolicy = "=0.244.0".parse().expect("parse override");
        assert!(
            override_policy
                .check(&"0.244.0".parse().expect("parse override version"))
                .is_ok()
        );
    }

    #[test]
    fn doctor_reports_only_auth_check_statuses() {
        let report = r#"{"ok":true,"results":[
            {"id":"env.info","status":"pass","data":{"home":"not retained"}},
            {"id":"auth.credentials","status":"pass","detail":"not retained"},
            {"id":"auth.verify","status":"pass","durationMs":19}
        ]}"#;
        let status = parse_doctor_auth(report).expect("parse auth status");
        assert!(status.authenticated);
        assert_eq!(status.checks.len(), 2);
        assert_eq!(status.checks[0].id, "auth.credentials");
        assert!(!format!("{status:?}").contains("not retained"));

        let failed = r#"{"ok":false,"results":[{"id":"auth.verify","status":"fail"}]}"#;
        assert!(
            !parse_doctor_auth(failed)
                .expect("parse failed status")
                .authenticated
        );
        assert!(parse_doctor_auth(r#"{"ok":true,"results":[]}"#).is_err());
        assert!(parse_doctor_auth("not JSON").is_err());
    }
}
