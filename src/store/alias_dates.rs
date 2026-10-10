//! Creation dates of aliases. Stalwart records none, so stalias remembers the ones it saw
//! being created, keyed by mailbox and lower-case address.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};

use super::{Result, Store, now};

pub(super) const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS alias_meta (
    account_id TEXT NOT NULL,
    address TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (account_id, address)
);";

impl Store {
    /// Remembers that an alias was created now. Addresses are stored in lower case.
    pub async fn record_alias_created(&self, account_id: &str, address: &str) -> Result<()> {
        let (account_id, address) = (account_id.to_owned(), address.to_ascii_lowercase());
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO alias_meta (account_id, address, created_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(account_id, address) DO UPDATE SET created_at = excluded.created_at",
                params![account_id, address, now()],
            )?;
            Ok(())
        })
        .await
    }

    /// Drops what is known about a deleted alias.
    pub async fn forget_alias(&self, account_id: &str, address: &str) -> Result<()> {
        let (account_id, address) = (account_id.to_owned(), address.to_ascii_lowercase());
        self.run(move |conn| {
            conn.execute(
                "DELETE FROM alias_meta WHERE account_id = ?1 AND address = ?2",
                params![account_id, address],
            )?;
            Ok(())
        })
        .await
    }

    /// When an alias was created, if stalias saw it happen. Address in lower case.
    pub async fn alias_created_at(&self, account_id: &str, address: &str) -> Result<Option<i64>> {
        let (account_id, address) = (account_id.to_owned(), address.to_ascii_lowercase());
        self.run(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT created_at FROM alias_meta WHERE account_id = ?1 AND address = ?2",
                    params![account_id, address],
                    |r| r.get(0),
                )
                .optional()?)
        })
        .await
    }

    /// Creation dates (Unix seconds) of all known aliases of a mailbox, by lower-case address.
    pub async fn alias_created_dates(&self, account_id: &str) -> Result<HashMap<String, i64>> {
        let account_id = account_id.to_owned();
        self.run(move |conn| {
            let mut stmt =
                conn.prepare("SELECT address, created_at FROM alias_meta WHERE account_id = ?1")?;
            let rows = stmt.query_map([account_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<std::result::Result<_, _>>()?)
        })
        .await
    }
}

/// Forgets every date. Used when the Stalwart server changes, since mailbox ids differ there.
pub(super) fn clear(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM alias_meta", [])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn creation_dates_are_kept_per_mailbox() {
        let s = Store::in_memory().unwrap();
        s.record_alias_created("a", "Shop@Example.org")
            .await
            .unwrap();
        s.record_alias_created("b", "other@example.org")
            .await
            .unwrap();
        assert!(
            s.alias_created_at("a", "shop@example.org")
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            s.alias_created_at("a", "other@example.org")
                .await
                .unwrap()
                .is_none()
        );
        let dates = s.alias_created_dates("a").await.unwrap();
        assert_eq!(dates.keys().collect::<Vec<_>>(), ["shop@example.org"]);

        s.forget_alias("a", "SHOP@example.org").await.unwrap();
        assert!(s.alias_created_dates("a").await.unwrap().is_empty());
        assert_eq!(s.alias_created_dates("b").await.unwrap().len(), 1);
    }
}
