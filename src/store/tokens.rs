//! API tokens for the Addy.io-compatible API. Each token belongs to one mailbox.

use rand::distr::{Alphanumeric, SampleString};
use rusqlite::{Connection, OptionalExtension, params};

use super::{Result, Store, hash, now};

pub(super) const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS tokens (
    id INTEGER PRIMARY KEY,
    hash TEXT NOT NULL UNIQUE,
    label TEXT NOT NULL,
    account_id TEXT NOT NULL,
    account_name TEXT NOT NULL,
    created_at INTEGER NOT NULL
);";

const TOKEN_PREFIX: &str = "stalias_";

/// An API token. Only its hash is stored, so this holds metadata only.
#[derive(Debug, Clone)]
pub struct Token {
    pub id: i64,
    pub label: String,
    pub account_id: String,
    pub account_name: String,
    /// Unix timestamp in seconds.
    pub created_at: i64,
}

impl Store {
    /// Returns the plaintext token. Only its hash is stored, so this is the one chance to show it.
    pub async fn create_token(
        &self,
        label: String,
        account_id: String,
        account_name: String,
    ) -> Result<String> {
        let plain = format!(
            "{TOKEN_PREFIX}{}",
            Alphanumeric.sample_string(&mut rand::rng(), 40)
        );
        let hashed = hash(&plain);
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO tokens (hash, label, account_id, account_name, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![hashed, label, account_id, account_name, now()],
            )?;
            Ok(())
        })
        .await?;
        Ok(plain)
    }

    /// Looks up a token by its plaintext value.
    pub async fn find_token(&self, plain: &str) -> Result<Option<Token>> {
        let hashed = hash(plain);
        self.run(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT id, label, account_id, account_name, created_at FROM tokens WHERE hash = ?1",
                    [hashed],
                    token_from_row,
                )
                .optional()?)
        })
        .await
    }

    /// All tokens, oldest first.
    pub async fn tokens(&self) -> Result<Vec<Token>> {
        self.run(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, label, account_id, account_name, created_at FROM tokens ORDER BY id",
            )?;
            let rows = stmt.query_map([], token_from_row)?;
            Ok(rows.collect::<std::result::Result<_, _>>()?)
        })
        .await
    }

    /// Revokes a token.
    pub async fn delete_token(&self, id: i64) -> Result<()> {
        self.run(move |conn| {
            conn.execute("DELETE FROM tokens WHERE id = ?1", [id])?;
            Ok(())
        })
        .await
    }
}

/// Revokes every token. Used when the Stalwart server changes, since mailbox ids differ there.
pub(super) fn clear(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM tokens", [])?;
    Ok(())
}

fn token_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Token> {
    Ok(Token {
        id: r.get(0)?,
        label: r.get(1)?,
        account_id: r.get(2)?,
        account_name: r.get(3)?,
        created_at: r.get(4)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tokens_are_stored_hashed_and_revocable() {
        let s = Store::in_memory().unwrap();
        let plain = s
            .create_token("laptop".into(), "c".into(), "alice".into())
            .await
            .unwrap();
        assert!(plain.starts_with(TOKEN_PREFIX));
        let found = s.find_token(&plain).await.unwrap().unwrap();
        assert_eq!(
            (found.account_id.as_str(), found.label.as_str()),
            ("c", "laptop")
        );
        assert!(s.find_token("stalias_wrong").await.unwrap().is_none());
        s.delete_token(found.id).await.unwrap();
        assert!(s.find_token(&plain).await.unwrap().is_none());
    }
}
