//! The world's event log: one `events` table per world database, written in transactions.

use std::path::Path;

use acan_core::{Seq, Stamped};
use anyhow::Context;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use crate::land_v1::LandingAuditRecord;

/// The stored log of one world.
#[derive(Clone)]
pub struct EventStore {
    pool: SqlitePool,
}

impl EventStore {
    /// Opens or creates the database at `path` and applies its migrations.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be opened or a migration fails.
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
        Ok(Self { pool })
    }

    /// Stores `events` in one transaction. Either every event is stored or none is.
    ///
    /// # Errors
    ///
    /// Returns an error when any insert fails. The transaction is then rolled back.
    pub async fn append(&self, events: &[Stamped]) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        for stamped in events {
            let json = serde_json::to_string(stamped)?;
            sqlx::query("INSERT INTO events (seq, json) VALUES (?, ?)")
                .bind(i64::try_from(stamped.seq.get())?)
                .bind(json)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Store one V1 landing authorization before the protected ref is allowed to move.
    /// These events do not consume positions in the origin's sequence.
    pub(crate) async fn append_v1_landing(
        &self,
        record: &LandingAuditRecord,
    ) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO v1_landing_events (json) VALUES (?)")
            .bind(serde_json::to_string(record)?)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Returns every stored event with `seq >= from`, in seq order.
    ///
    /// # Errors
    ///
    /// Returns an error when the query fails or a stored row does not decode.
    pub async fn load_from(&self, from: Seq) -> anyhow::Result<Vec<Stamped>> {
        let rows = sqlx::query("SELECT json FROM events WHERE seq >= ? ORDER BY seq")
            .bind(i64::try_from(from.get())?)
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|row| {
                let json: String = row.try_get("json")?;
                Ok(serde_json::from_str(&json)?)
            })
            .collect()
    }
}
