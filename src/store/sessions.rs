//! Login sessions of the admin. The browser holds a random id; only its hash is stored.

use rand::distr::{Alphanumeric, SampleString};
use rusqlite::{OptionalExtension, params};

use super::{Result, Store, hash, now};

pub(super) const SCHEMA: &str =
    "CREATE TABLE IF NOT EXISTS sessions (hash TEXT PRIMARY KEY, expires_at INTEGER NOT NULL);";

const SESSION_TTL_SECS: i64 = 7 * 24 * 3600;

impl Store {
    /// Returns the cookie value for a new login session.
    pub async fn create_session(&self) -> Result<String> {
        let plain = Alphanumeric.sample_string(&mut rand::rng(), 48);
        let hashed = hash(&plain);
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO sessions (hash, expires_at) VALUES (?1, ?2)",
                params![hashed, now() + SESSION_TTL_SECS],
            )?;
            Ok(())
        })
        .await?;
        Ok(plain)
    }

    /// Whether the cookie value belongs to a session that has not expired.
    pub async fn session_valid(&self, plain: &str) -> Result<bool> {
        let hashed = hash(plain);
        self.run(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT 1 FROM sessions WHERE hash = ?1 AND expires_at >= ?2",
                    params![hashed, now()],
                    |_| Ok(()),
                )
                .optional()?
                .is_some())
        })
        .await
    }

    /// Removes sessions that can no longer be used. Validity checks ignore them anyway,
    /// so this only keeps the table small.
    pub async fn purge_expired_sessions(&self) -> Result<()> {
        self.run(|conn| {
            conn.execute("DELETE FROM sessions WHERE expires_at < ?1", [now()])?;
            Ok(())
        })
        .await
    }

    /// Ends a session (logout).
    pub async fn delete_session(&self, plain: &str) -> Result<()> {
        let hashed = hash(plain);
        self.run(move |conn| {
            conn.execute("DELETE FROM sessions WHERE hash = ?1", [hashed])?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sessions_expire_and_can_be_deleted() {
        let s = Store::in_memory().unwrap();
        let id = s.create_session().await.unwrap();
        assert!(s.session_valid(&id).await.unwrap());
        s.delete_session(&id).await.unwrap();
        assert!(!s.session_valid(&id).await.unwrap());
        assert!(!s.session_valid("never-issued").await.unwrap());
    }
}
