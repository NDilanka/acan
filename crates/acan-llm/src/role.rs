use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::UnknownRole;

/// The job a model call does. Routing maps each role to a provider and model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Muse Spark. Plans and reviews the work.
    Foreman,
    /// Muse Spark Open. Does the worker calls.
    Worker,
    /// Quick, cheap calls such as clash triage.
    Fast,
    /// Image input. Compares page screenshots in the advisory vision witness.
    Vision,
}

impl Role {
    /// The name used in config files, CLI flags, and reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Foreman => "foreman",
            Self::Worker => "worker",
            Self::Fast => "fast",
            Self::Vision => "vision",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = UnknownRole;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "foreman" => Ok(Self::Foreman),
            "worker" => Ok(Self::Worker),
            "fast" => Ok(Self::Fast),
            "vision" => Ok(Self::Vision),
            other => Err(UnknownRole(other.to_owned())),
        }
    }
}
