//! Bearer tokens. A tokens file in the repo's `.acan/` directory maps each token to a principal.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock, RwLockReadGuard};

use acan_core::{AgentId, MemberId};
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use uuid::Uuid;

pub(crate) const TOKENS_FILE: &str = "tokens.json";

/// Each token the server stopped accepting, sent once its replacement is written. A connection
/// that holds one of them closes itself. See [`subscribe_revocations`].
static REVOCATIONS: OnceLock<broadcast::Sender<String>> = OnceLock::new();

/// How many revocations may wait for a connection before it lags. A lagging connection checks its
/// own token rather than trusting the channel to have told it everything.
const REVOCATION_CAPACITY: usize = 64;

/// The sender of the revocation channel, made on first use.
fn revocations() -> &'static broadcast::Sender<String> {
    REVOCATIONS.get_or_init(|| broadcast::channel(REVOCATION_CAPACITY).0)
}

/// A receiver of every token revoked from now on. A connection subscribes when it is accepted,
/// before it checks its token, so a revocation that lands after the check still reaches it.
#[must_use]
pub(crate) fn subscribe_revocations() -> broadcast::Receiver<String> {
    revocations().subscribe()
}

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

/// Every token the server accepts. Clones share one table, so a token rotated through one clone
/// stops working in every clone.
#[derive(Clone)]
pub struct Tokens {
    by_token: Arc<RwLock<BTreeMap<String, Principal>>>,
    /// Held for the whole of a change, so two changes never interleave their file writes.
    gate: Arc<Mutex<()>>,
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
        let token = Token(Self::mint_token());
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
                let token = Token(Self::mint_token());
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
    pub fn principal(&self, token: &str) -> Option<Principal> {
        self.table().get(token).cloned()
    }

    /// The token text held for `principal`, or `None` when it holds none. When it holds several,
    /// the first in token order.
    #[must_use]
    pub fn token_of(&self, principal: &Principal) -> Option<String> {
        self.table()
            .iter()
            .find(|(_, held)| *held == principal)
            .map(|(token, _)| token.clone())
    }

    /// Every agent the tokens act for, each once, in id order.
    #[must_use]
    pub fn agents(&self) -> Vec<AgentId> {
        self.table()
            .values()
            .filter_map(|principal| match principal {
                Principal::Agent(agent) => Some(agent.clone()),
                Principal::Member(_) => None,
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// How many distinct agents the tokens act for. An agent with two tokens counts once.
    #[must_use]
    pub fn agent_count(&self) -> usize {
        self.agents().len()
    }

    /// The token of agent `agent`. A token the file does not hold yet is minted and written to
    /// `<dir>/tokens.json` before it is returned, so an agent keeps its token across starts.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be written. The new token is not kept then.
    pub fn agent_token(&mut self, dir: &Path, agent: &AgentId) -> anyhow::Result<Token> {
        let principal = Principal::Agent(agent.clone());
        if let Some(text) = self.token_of(&principal) {
            return Ok(Token(text));
        }
        let token = Token(Self::mint_token());
        let text = token.0.clone();
        self.commit(dir, |table| {
            table.insert(text, principal);
            Ok(())
        })?;
        Ok(token)
    }

    /// Gives `principal` the token `token` in place of every token it held, and writes the file.
    /// The old tokens stop working, and each one removed is announced on the revocation channel
    /// once the write succeeds. A change that cannot be written leaves the tokens as they were,
    /// and announces nothing.
    ///
    /// # Errors
    ///
    /// Returns an error when `token` is empty or already held by another principal, or when the
    /// file cannot be written.
    pub fn replace_token(
        &self,
        dir: &Path,
        principal: &Principal,
        token: &str,
    ) -> anyhow::Result<()> {
        if token.is_empty() {
            bail!("a token cannot be empty");
        }
        let removed = self.commit(dir, |table| {
            if table.get(token).is_some_and(|held| held != principal) {
                bail!("the token is held by another principal");
            }
            // A token the principal keeps is not removed, so it is not announced.
            let removed: Vec<String> = table
                .iter()
                .filter(|(text, held)| *held == principal && text.as_str() != token)
                .map(|(text, _)| text.clone())
                .collect();
            table.retain(|_, held| held != principal);
            table.insert(token.to_owned(), principal.clone());
            Ok(removed)
        })?;
        for old in removed {
            // Nothing may be listening, which is not an error: there is then nothing to close.
            let _ = revocations().send(old);
        }
        Ok(())
    }

    /// A new bearer token. Every token is minted here, so the format lives in one place.
    #[must_use]
    pub fn mint_token() -> String {
        format!("acan_{}", Uuid::new_v4().simple())
    }

    /// Applies `change` to a copy of the table, writes the copy to `<dir>/tokens.json`, and then
    /// makes the copy the table. The table lock is not held during the write.
    fn commit<T>(
        &self,
        dir: &Path,
        change: impl FnOnce(&mut BTreeMap<String, Principal>) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let _gate = self.gate.lock().unwrap_or_else(PoisonError::into_inner);
        let mut next = self.table().clone();
        let outcome = change(&mut next)?;
        Self::write(&next, dir)?;
        *self
            .by_token
            .write()
            .unwrap_or_else(PoisonError::into_inner) = next;
        Ok(outcome)
    }

    /// The table, read under its lock. A poisoned lock is read anyway, so a panic elsewhere
    /// cannot stop requests.
    fn table(&self) -> RwLockReadGuard<'_, BTreeMap<String, Principal>> {
        self.by_token.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Writes `table` to `<dir>/tokens.json` through a temporary file, so a crash mid-write
    /// leaves the old file in place.
    fn write(table: &BTreeMap<String, Principal>, dir: &Path) -> anyhow::Result<()> {
        let entries: Vec<Entry> = table
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
        Ok(Self {
            by_token: Arc::new(RwLock::new(by_token)),
            gate: Arc::new(Mutex::new(())),
        })
    }
}

impl fmt::Debug for Tokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tokens")
            .field("count", &self.table().len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    fn owner_id() -> MemberId {
        MemberId::from_str("owner").expect("valid member id")
    }

    fn agent_id(name: &str) -> AgentId {
        AgentId::from_str(name).expect("valid agent id")
    }

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
            Some(Principal::Agent(local))
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

    #[test]
    fn a_rotated_token_stops_working_and_survives_a_reload() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let (mut tokens, _) = Tokens::load_or_create(dir.path(), &owner_id()).expect("tokens load");
        let principal = Principal::Agent(agent_id("local-1"));
        let old = tokens
            .agent_token(dir.path(), &agent_id("local-1"))
            .expect("the agent gets a token");
        let new = Tokens::mint_token();

        tokens
            .replace_token(dir.path(), &principal, &new)
            .expect("the token rotates");
        assert_eq!(
            tokens.principal(old.as_str()),
            None,
            "the old token stops working"
        );
        assert_eq!(
            tokens.principal(&new),
            Some(principal.clone()),
            "the new token works"
        );

        let text = fs::read_to_string(dir.path().join(TOKENS_FILE)).expect("tokens file");
        assert!(
            !text.contains(old.as_str()),
            "the old token leaves the file"
        );
        assert!(text.contains(&new), "the new token is written to the file");

        let (reloaded, _) = Tokens::load_or_create(dir.path(), &owner_id()).expect("reload");
        assert_eq!(
            reloaded.principal(&new),
            Some(principal),
            "the new token survives a reload"
        );
        assert_eq!(reloaded.principal(old.as_str()), None);
    }

    #[test]
    fn token_of_finds_the_token_an_agent_was_given() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let (mut tokens, owner_token) =
            Tokens::load_or_create(dir.path(), &owner_id()).expect("tokens load");
        let owner_token = owner_token.expect("the first load mints the owner token");
        let minted = tokens
            .agent_token(dir.path(), &agent_id("local-1"))
            .expect("the agent gets a token");

        assert_eq!(
            tokens.token_of(&Principal::Agent(agent_id("local-1"))),
            Some(minted.as_str().to_owned())
        );
        assert_eq!(
            tokens.token_of(&Principal::Agent(agent_id("local-2"))),
            None,
            "an agent with no token has none"
        );
        assert_eq!(
            tokens.token_of(&Principal::Member(owner_id())),
            Some(owner_token.as_str().to_owned()),
            "the owner's token is found the same way"
        );
    }

    #[test]
    fn agents_lists_each_agent_once_in_id_order() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let (mut tokens, _) = Tokens::load_or_create(dir.path(), &owner_id()).expect("tokens load");
        tokens
            .agent_token(dir.path(), &agent_id("local-2"))
            .expect("local-2 gets a token");
        tokens
            .agent_token(dir.path(), &agent_id("local-1"))
            .expect("local-1 gets a token");
        tokens
            .replace_token(
                dir.path(),
                &Principal::Agent(agent_id("local-1")),
                &Tokens::mint_token(),
            )
            .expect("local-1 rotates");

        assert_eq!(
            tokens.agents(),
            vec![agent_id("local-1"), agent_id("local-2")]
        );
        assert_eq!(tokens.agent_count(), 2);
    }

    #[test]
    fn a_token_held_by_another_principal_is_refused() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let (mut tokens, owner_token) =
            Tokens::load_or_create(dir.path(), &owner_id()).expect("tokens load");
        let owner_token = owner_token.expect("the first load mints the owner token");
        let worker = tokens
            .agent_token(dir.path(), &agent_id("local-1"))
            .expect("the agent gets a token");

        let err = tokens
            .replace_token(
                dir.path(),
                &Principal::Agent(agent_id("local-2")),
                worker.as_str(),
            )
            .expect_err("another principal's token is refused");
        assert!(
            err.to_string().contains("held by another principal"),
            "the error names the conflict: {err}"
        );
        assert_eq!(
            tokens.principal(worker.as_str()),
            Some(Principal::Agent(agent_id("local-1"))),
            "the holder keeps its token"
        );
        assert_eq!(
            tokens.principal(owner_token.as_str()),
            Some(Principal::Member(owner_id())),
            "the owner keeps its token"
        );

        tokens
            .replace_token(
                dir.path(),
                &Principal::Member(owner_id()),
                owner_token.as_str(),
            )
            .expect("a principal may keep its own token");
        assert!(
            tokens
                .replace_token(dir.path(), &Principal::Member(owner_id()), "")
                .is_err(),
            "an empty token is refused"
        );
        assert_eq!(tokens.principal(""), None);
    }

    #[test]
    fn a_rotation_that_cannot_be_written_changes_nothing() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let (mut tokens, _) = Tokens::load_or_create(dir.path(), &owner_id()).expect("tokens load");
        let principal = Principal::Agent(agent_id("local-1"));
        let old = tokens
            .agent_token(dir.path(), &agent_id("local-1"))
            .expect("the agent gets a token");
        let missing = dir.path().join("missing");

        tokens
            .replace_token(&missing, &principal, &Tokens::mint_token())
            .expect_err("the directory does not exist, so the write fails");
        assert_eq!(
            tokens.principal(old.as_str()),
            Some(principal),
            "the old token still works"
        );
    }

    #[test]
    fn debug_output_hides_rotated_and_agent_tokens() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let (mut tokens, _) = Tokens::load_or_create(dir.path(), &owner_id()).expect("tokens load");
        let principal = Principal::Agent(agent_id("local-1"));
        let old = tokens
            .agent_token(dir.path(), &agent_id("local-1"))
            .expect("the agent gets a token");
        let new = Tokens::mint_token();
        tokens
            .replace_token(dir.path(), &principal, &new)
            .expect("the token rotates");

        let rendered = format!("{old:?} {tokens:?}");
        assert!(
            !rendered.contains(old.as_str()) && !rendered.contains(&new),
            "debug output must not contain any token: {rendered}"
        );
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    /// Every token announced since `receiver` last read, in the order announced.
    fn announcements(receiver: &mut broadcast::Receiver<String>) -> Vec<String> {
        let mut seen = Vec::new();
        loop {
            match receiver.try_recv() {
                Ok(text) => seen.push(text),
                Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(_) => return seen,
            }
        }
    }

    #[test]
    fn a_removed_token_is_announced_once_its_write_succeeds() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let (mut tokens, _) = Tokens::load_or_create(dir.path(), &owner_id()).expect("tokens load");
        let principal = Principal::Agent(agent_id("local-1"));
        let old = tokens
            .agent_token(dir.path(), &agent_id("local-1"))
            .expect("the agent gets a token");
        let new = Tokens::mint_token();
        let mut announced = subscribe_revocations();

        tokens
            .replace_token(dir.path(), &principal, &new)
            .expect("the token rotates");
        let seen = announcements(&mut announced);
        assert!(
            seen.iter().any(|text| text == old.as_str()),
            "the removed token is announced: {seen:?}"
        );
        assert!(!seen.contains(&new), "the new token is not announced");

        tokens
            .replace_token(dir.path(), &principal, &new)
            .expect("the same token is kept");
        assert!(
            !announcements(&mut announced).contains(&new),
            "a token the principal keeps is not announced"
        );

        let missing = dir.path().join("missing");
        tokens
            .replace_token(&missing, &principal, &Tokens::mint_token())
            .expect_err("the directory does not exist, so the write fails");
        assert!(
            !announcements(&mut announced).contains(&new),
            "a write that failed announces nothing, and the held token is still held"
        );
        assert_eq!(tokens.principal(&new), Some(principal));
    }
}
