//! Bearer tokens. A tokens file in the repo's `.acan/` directory maps each token to a principal.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::Path;

use acan_core::{AgentId, MemberId};
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub(crate) const TOKENS_FILE: &str = "tokens.json";

/// Who a token acts for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Principal {
    /// A human member.
    Member(MemberId),
    /// An agent acting for a member.
    Agent(AgentId),
}

/// A bearer token. `Debug` is redacted, so a token cannot reach a log through `{:?}`.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// The token text. Only the code that shows or sends the token should call this.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

#[derive(Serialize, Deserialize)]
struct Entry {
    token: String,
    principal: Principal,
}

/// Every token the server accepts, loaded once at startup.
#[derive(Clone)]
pub struct Tokens {
    by_token: BTreeMap<String, Principal>,
}

impl Tokens {
    /// Loads `<dir>/tokens.json`. When the file does not exist, it mints an owner token for
    /// `founder`, writes the file, and returns the token so the caller can show it once.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read, written or parsed.
    pub fn load_or_create(dir: &Path, founder: &MemberId) -> anyhow::Result<(Self, Option<Token>)> {
        let path = dir.join(TOKENS_FILE);
        if path.exists() {
            let text =
                fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            let entries: Vec<Entry> = serde_json::from_str(&text)
                .with_context(|| format!("parsing {}", path.display()))?;
            return Ok((Self::from_entries(entries)?, None));
        }
        let token = Token(format!("acan_{}", Uuid::new_v4().simple()));
        let entries = vec![Entry {
            token: token.0.clone(),
            principal: Principal::Member(founder.clone()),
        }];
        fs::write(&path, serde_json::to_string_pretty(&entries)?)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok((Self::from_entries(entries)?, Some(token)))
    }

    /// Loads the tokens of every project one process serves, and gives them one human token.
    ///
    /// The human token is the first owner entry found across the files, in order, or a new token
    /// when no file has one. It is written into every file that lacks it, so one browser token
    /// opens every project. Agent tokens are never shared: each stays in its project's file. A
    /// file that does not exist yet is created with the human token alone.
    ///
    /// Returns the tokens of each directory, in the order given, and the token minted now, if
    /// one was minted.
    ///
    /// # Errors
    ///
    /// Returns an error when a file cannot be read, written or parsed, or when a file already
    /// lists the human token for some other principal.
    pub fn load_shared(
        dirs: &[&Path],
        founder: &MemberId,
    ) -> anyhow::Result<(Vec<Self>, Option<Token>)> {
        let owner = Principal::Member(founder.clone());
        let mut files = Vec::with_capacity(dirs.len());
        for dir in dirs {
            let path = dir.join(TOKENS_FILE);
            let entries = if path.exists() {
                let text = fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                serde_json::from_str::<Vec<Entry>>(&text)
                    .with_context(|| format!("parsing {}", path.display()))?
            } else {
                Vec::new()
            };
            files.push((path, entries));
        }
        let existing = files
            .iter()
            .flat_map(|(_, entries)| entries)
            .find(|entry| entry.principal == owner)
            .map(|entry| entry.token.clone());
        let (human, minted) = match existing {
            Some(token) => (token, None),
            None => {
                let token = Token(format!("acan_{}", Uuid::new_v4().simple()));
                (token.0.clone(), Some(token))
            }
        };
        let mut tokens = Vec::with_capacity(files.len());
        for (path, mut entries) in files {
            let held = entries
                .iter()
                .find(|entry| entry.token == human)
                .map(|entry| entry.principal == owner);
            match held {
                Some(true) => {}
                Some(false) => bail!(
                    "the tokens file {} lists the human token for another principal",
                    path.display()
                ),
                None => {
                    entries.push(Entry {
                        token: human.clone(),
                        principal: owner.clone(),
                    });
                    fs::write(&path, serde_json::to_string_pretty(&entries)?)
                        .with_context(|| format!("writing {}", path.display()))?;
                }
            }
            tokens.push(Self::from_entries(entries)?);
        }
        Ok((tokens, minted))
    }

    /// The principal a token acts for, or `None` for an unknown token.
    #[must_use]
    pub fn principal(&self, token: &str) -> Option<&Principal> {
        self.by_token.get(token)
    }

    /// How many distinct agents the tokens act for. An agent with two tokens counts once.
    #[must_use]
    pub fn agent_count(&self) -> usize {
        self.by_token
            .values()
            .filter_map(|principal| match principal {
                Principal::Agent(agent) => Some(agent),
                Principal::Member(_) => None,
            })
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    }

    /// The token of agent `agent`. A token the file does not hold yet is minted and written to
    /// `<dir>/tokens.json` before it is returned, so an agent keeps its token across starts.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be written. The new token is not kept then.
    pub fn agent_token(&mut self, dir: &Path, agent: &AgentId) -> anyhow::Result<Token> {
        let principal = Principal::Agent(agent.clone());
        if let Some((text, _)) = self.by_token.iter().find(|(_, held)| **held == principal) {
            return Ok(Token(text.clone()));
        }
        let token = Token(format!("acan_{}", Uuid::new_v4().simple()));
        self.by_token.insert(token.0.clone(), principal);
        if let Err(err) = self.write(dir) {
            self.by_token.remove(&token.0);
            return Err(err);
        }
        Ok(token)
    }

    /// Writes every entry to `<dir>/tokens.json` through a temporary file, so a crash mid-write
    /// leaves the old file in place.
    fn write(&self, dir: &Path) -> anyhow::Result<()> {
        let entries: Vec<Entry> = self
            .by_token
            .iter()
            .map(|(token, principal)| Entry {
                token: token.clone(),
                principal: principal.clone(),
            })
            .collect();
        let path = dir.join(TOKENS_FILE);
        let temporary = dir.join(format!("{TOKENS_FILE}.tmp"));
        fs::write(&temporary, serde_json::to_string_pretty(&entries)?)
            .with_context(|| format!("writing {}", temporary.display()))?;
        fs::rename(&temporary, &path).with_context(|| format!("writing {}", path.display()))
    }

    fn from_entries(entries: Vec<Entry>) -> anyhow::Result<Self> {
        let mut by_token = BTreeMap::new();
        for entry in entries {
            if by_token.insert(entry.token, entry.principal).is_some() {
                bail!("the tokens file lists one token more than once");
            }
        }
        Ok(Self { by_token })
    }
}

impl fmt::Debug for Tokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tokens")
            .field("count", &self.by_token.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn an_agent_keeps_its_token_across_loads_and_each_agent_has_its_own() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let owner = MemberId::from_str("owner").expect("valid member id");
        let local = AgentId::from_str("local-1").expect("valid agent id");
        let (mut tokens, owner_token) =
            Tokens::load_or_create(dir.path(), &owner).expect("tokens load");
        assert!(
            owner_token.is_some(),
            "the first load mints the owner token"
        );

        let first = tokens
            .agent_token(dir.path(), &local)
            .expect("the agent gets a token");
        let again = tokens
            .agent_token(dir.path(), &local)
            .expect("the same agent gets the same token");
        assert_eq!(first, again);

        let (mut reloaded, owner_again) =
            Tokens::load_or_create(dir.path(), &owner).expect("tokens reload");
        assert!(owner_again.is_none(), "a second load mints nothing");
        assert_eq!(
            reloaded
                .agent_token(dir.path(), &local)
                .expect("the token survives"),
            first
        );
        assert_eq!(
            reloaded.principal(first.as_str()),
            Some(&Principal::Agent(local))
        );

        let other = AgentId::from_str("local-2").expect("valid agent id");
        let second = reloaded
            .agent_token(dir.path(), &other)
            .expect("a second agent gets a token");
        assert_ne!(second, first);
        assert!(
            !dir.path().join("tokens.json.tmp").exists(),
            "the write leaves no temporary file"
        );
    }
}
