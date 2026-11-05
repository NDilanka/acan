//! The world's event log: one `events` table per world database, written in transactions.

use std::fs;
use std::io::{Read, Write};
use std::path::Path;

use acan_core::{Seq, Stamped};
use acan_intent::canonical_json;
use anyhow::{Context, ensure};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

const ZERO_MAC: [u8; 32] = [0; 32];

/// The stored log of one world.
#[derive(Clone)]
pub struct EventStore {
    pool: SqlitePool,
    key: [u8; 32],
}

impl EventStore {
    /// Opens or creates the database at `path` and applies its migrations.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be opened, a migration fails, or integrity
    /// verification fails. Legacy logs are sealed once; see `SECURITY.md`.
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new()
            .connect_with(options)
            .await
            .with_context(|| format!("opening {}", path.display()))?;
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .context("applying the event log migrations")?;
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        let sealed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM event_integrity)")
            .fetch_one(&mut *tx)
            .await?;
        let key_path = path.with_extension("key");
        let key = if sealed {
            read_key(&key_path)?
        } else {
            ensure!(
                !key_path.try_exists()?,
                "unsealed event log already has a key; restore a trusted database/key backup"
            );
            let rows = sqlx::query("SELECT seq, json, mac FROM events ORDER BY seq")
                .fetch_all(&mut *tx)
                .await?;
            let mut key = [0; 32];
            getrandom::fill(&mut key).context("generating the event log key")?;
            let mut previous = ZERO_MAC;
            for (expected, row) in rows.iter().enumerate() {
                let seq: i64 = row.try_get("seq")?;
                let json: String = row.try_get("json")?;
                let mac: Option<Vec<u8>> = row.try_get("mac")?;
                ensure!(mac.is_none(), "an unsealed log contains authenticated rows");
                ensure!(seq == i64::try_from(expected)?, "event log sequence gap");
                let event: Stamped = serde_json::from_str(&json)?;
                ensure!(
                    event.seq.get() == u64::try_from(seq)?,
                    "event row sequence mismatch"
                );
                previous = event_mac(&key, &previous, seq, &json)?
                    .finalize()
                    .into_bytes()
                    .into();
                sqlx::query("UPDATE events SET mac = ? WHERE seq = ?")
                    .bind(previous.as_slice())
                    .bind(seq)
                    .execute(&mut *tx)
                    .await?;
            }
            // Publish the key before committing the seal. A crash in between fails closed,
            // rather than silently authenticating the database again on the next start.
            let mut file = tempfile::NamedTempFile::new_in(
                key_path
                    .parent()
                    .context("event log key needs a directory")?,
            )?;
            file.write_all(&key)?;
            file.as_file().sync_all()?;
            file.persist_noclobber(&key_path)
                .map_err(|error| error.error)
                .context("saving the event log key")?;
            sqlx::query(
                "INSERT INTO event_integrity (id, version, next_seq, mac) VALUES (1, 1, ?, ?)",
            )
            .bind(i64::try_from(rows.len())?)
            .bind(previous.as_slice())
            .execute(&mut *tx)
            .await?;
            key
        };
        tx.commit().await?;
        let store = Self { pool, key };
        store.load_from(Seq::new(0)).await?;
        Ok(store)
    }

    /// Stores `events` in one transaction. Either every event is stored or none is.
    ///
    /// # Errors
    ///
    /// Returns an error when any insert fails. The transaction is then rolled back.
    pub async fn append(&self, events: &[Stamped]) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let head = sqlx::query("SELECT version, next_seq, mac FROM event_integrity WHERE id = 1")
            .fetch_one(&mut *tx)
            .await?;
        ensure!(
            head.try_get::<i64, _>("version")? == 1,
            "unknown event integrity version"
        );
        let mut next: i64 = head.try_get("next_seq")?;
        let mut previous: Vec<u8> = head.try_get("mac")?;
        ensure!(previous.len() == 32, "invalid event log head");
        for stamped in events {
            ensure!(
                i64::try_from(stamped.seq.get())? == next,
                "event log sequence gap"
            );
            let json = serde_json::to_string(stamped)?;
            previous = event_mac(&self.key, &previous, next, &json)?
                .finalize()
                .into_bytes()
                .to_vec();
            sqlx::query("INSERT INTO events (seq, json, mac) VALUES (?, ?, ?)")
                .bind(next)
                .bind(json)
                .bind(&previous)
                .execute(&mut *tx)
                .await?;
            next = next.checked_add(1).context("event sequence exhausted")?;
        }
        sqlx::query("UPDATE event_integrity SET next_seq = ?, mac = ? WHERE id = 1")
            .bind(next)
            .bind(previous)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Returns every stored event with `seq >= from`, in seq order.
    ///
    /// Verifies the entire chain, including the prefix before `from`, in one read snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the query fails, a row does not decode, or integrity fails.
    pub async fn load_from(&self, from: Seq) -> anyhow::Result<Vec<Stamped>> {
        let mut tx = self.pool.begin().await?;
        let head = sqlx::query("SELECT version, next_seq, mac FROM event_integrity WHERE id = 1")
            .fetch_one(&mut *tx)
            .await?;
        ensure!(
            head.try_get::<i64, _>("version")? == 1,
            "unknown event integrity version"
        );
        let rows = sqlx::query("SELECT seq, json, mac FROM events ORDER BY seq")
            .fetch_all(&mut *tx)
            .await?;
        let mut previous = ZERO_MAC.to_vec();
        let mut events = Vec::new();
        for (expected, row) in rows.iter().enumerate() {
            let seq: i64 = row.try_get("seq")?;
            let json: String = row.try_get("json")?;
            let mac: Vec<u8> = row.try_get("mac")?;
            ensure!(seq == i64::try_from(expected)?, "event log sequence gap");
            event_mac(&self.key, &previous, seq, &json)?
                .verify_slice(&mac)
                .with_context(|| format!("event log integrity mismatch at seq {seq}"))?;
            let event: Stamped = serde_json::from_str(&json)?;
            ensure!(
                event.seq.get() == u64::try_from(seq)?,
                "event row sequence mismatch"
            );
            if event.seq >= from {
                events.push(event);
            }
            previous = mac;
        }
        ensure!(
            head.try_get::<i64, _>("next_seq")? == i64::try_from(rows.len())?
                && head.try_get::<Vec<u8>, _>("mac")? == previous,
            "event log head mismatch"
        );
        tx.commit().await?;
        Ok(events)
    }
}

fn event_mac(
    key: &[u8; 32],
    previous: &[u8],
    seq: i64,
    json: &str,
) -> anyhow::Result<Hmac<Sha256>> {
    let value: serde_json::Value = serde_json::from_str(json)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(key)?;
    mac.update(b"acan-event-log-v1\0");
    mac.update(previous);
    mac.update(&seq.to_be_bytes());
    mac.update(&canonical_json(&value)?);
    Ok(mac)
}

fn read_key(path: &Path) -> anyhow::Result<[u8; 32]> {
    let metadata = fs::symlink_metadata(path).context("reading the event log key metadata")?;
    ensure!(
        metadata.is_file(),
        "event log key must be a regular file, not a symlink"
    );
    let file = fs::File::open(path).context("opening the event log key")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            file.metadata()?.permissions().mode() & 0o077 == 0,
            "event log key must not be readable by other users (use mode 0600)"
        );
    }
    let mut bytes = Vec::new();
    file.take(33).read_to_end(&mut bytes)?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid event log key length"))
}
