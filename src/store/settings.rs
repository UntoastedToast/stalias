//! Instance settings: the admin password hash, the Stalwart connection, the chosen alias
//! domains and the first-run flag. They are plain key-value pairs.

use std::collections::HashMap;

use rusqlite::{OptionalExtension, params};

use crate::naming::NameFormat;

use super::{Result, Store, alias_dates, hash, now, tokens};

pub(super) const SCHEMA: &str =
    "CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);";

const UPSERT: &str = "INSERT INTO settings (key, value) VALUES (?1, ?2)
     ON CONFLICT(key) DO UPDATE SET value = excluded.value";

/// Instance settings saved in the database.
#[derive(Debug, Default, Clone)]
pub struct Settings {
    pub admin_hash: Option<String>,
    pub stalwart_url: Option<String>,
    pub stalwart_key: Option<String>,
    /// Domain names that may be used for new aliases.
    pub domains: Vec<String>,
    /// Set once the first-run wizard was completed, which switches back to the normal layout.
    pub setup_done: bool,
    /// The style of aliases that get a generated name.
    pub alias_format: NameFormat,
}

impl Store {
    /// Reads all settings.
    pub async fn settings(&self) -> Result<Settings> {
        self.run(|conn| {
            let mut stmt = conn.prepare("SELECT key, value FROM settings")?;
            let mut all: HashMap<String, String> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            Ok(Settings {
                admin_hash: all.remove("admin_hash"),
                stalwart_url: all.remove("stalwart_url"),
                stalwart_key: all.remove("stalwart_key"),
                domains: all
                    .remove("domains")
                    .and_then(|v| serde_json::from_str(&v).ok())
                    .unwrap_or_default(),
                setup_done: all.contains_key("setup_done"),
                alias_format: all
                    .remove("alias_format")
                    .and_then(|v| NameFormat::parse(&v))
                    .unwrap_or_default(),
            })
        })
        .await
    }

    /// Stores the admin password hash unless an admin already exists.
    /// Returns false in that case, so two simultaneous signups cannot both win.
    pub async fn create_admin(&self, hash: String) -> Result<bool> {
        self.run(move |conn| {
            let changed = conn.execute(
                "INSERT INTO settings (key, value) VALUES ('admin_hash', ?1) ON CONFLICT(key) DO NOTHING",
                [hash],
            )?;
            Ok(changed == 1)
        })
        .await
    }

    /// Compare the password version and (for the web UI) session inside the write transaction.
    /// A successful change revokes every session, preserving tokens and other settings.
    pub async fn change_admin_password(
        &self,
        expected_hash: String,
        new_hash: String,
        session: Option<String>,
    ) -> Result<bool> {
        let session = session.map(|s| hash(&s));
        self.run(move |conn| {
            let tx = rusqlite::Transaction::new_unchecked(
                conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            if let Some(session) = session {
                let valid = tx
                    .query_row(
                        "SELECT 1 FROM sessions WHERE hash = ?1 AND expires_at >= ?2",
                        params![session, now()],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if !valid {
                    return Ok(false);
                }
            }
            let changed = tx.execute(
                "UPDATE settings SET value = ?1 WHERE key = 'admin_hash' AND value = ?2",
                params![new_hash, expected_hash],
            )?;
            if changed != 1 {
                return Ok(false);
            }
            tx.execute("DELETE FROM sessions", [])?;
            tx.commit()?;
            Ok(true)
        })
        .await
    }

    /// Saves the Stalwart URL and key.
    ///
    /// A different server revokes all tokens and clears the chosen domains, the finished-setup
    /// flag and the known creation dates, because their ids belong to the old server. Key
    /// rotation keeps them.
    pub async fn set_stalwart(&self, url: String, key: String) -> Result<()> {
        self.run(move |conn| {
            let tx = conn.unchecked_transaction()?;
            let previous: Option<String> = tx
                .query_row(
                    "SELECT value FROM settings WHERE key = 'stalwart_url'",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            if previous
                .as_deref()
                .is_some_and(|old| !same_server_url(old, &url))
            {
                tokens::clear(&tx)?;
                alias_dates::clear(&tx)?;
                tx.execute(
                    "DELETE FROM settings WHERE key IN ('domains', 'setup_done')",
                    [],
                )?;
            }
            for (name, value) in [("stalwart_url", url), ("stalwart_key", key)] {
                tx.execute(UPSERT, params![name, value])?;
            }
            Ok(tx.commit()?)
        })
        .await
    }

    /// Saves the domains that may be used for new aliases.
    pub async fn set_domains(&self, domains: &[String]) -> Result<()> {
        let json = serde_json::to_string(domains).expect("strings serialize");
        self.set("domains", json).await
    }

    /// Saves the style of generated alias names.
    pub async fn set_alias_format(&self, format: NameFormat) -> Result<()> {
        self.set("alias_format", format.as_str().into()).await
    }

    /// Marks the first-run wizard as completed.
    pub async fn finish_setup(&self) -> Result<()> {
        self.set("setup_done", "1".into()).await
    }

    async fn set(&self, key: &'static str, value: String) -> Result<()> {
        self.run(move |conn| {
            conn.execute(UPSERT, params![key, value])?;
            Ok(())
        })
        .await
    }
}

/// Whether two URLs name the same server, so that `http://mail` and `http://mail:80/` do not
/// count as a change.
fn same_server_url(a: &str, b: &str) -> bool {
    match (url::Url::parse(a), url::Url::parse(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn settings_roundtrip() {
        let s = Store::in_memory().unwrap();
        assert!(s.settings().await.unwrap().stalwart_url.is_none());
        s.set_stalwart("http://x".into(), "key".into())
            .await
            .unwrap();
        s.set_domains(&["a.org".into(), "b.org".into()])
            .await
            .unwrap();
        s.set_stalwart("http://y".into(), "key2".into())
            .await
            .unwrap();
        let c = s.settings().await.unwrap();
        assert_eq!(c.stalwart_url.as_deref(), Some("http://y"));
        assert!(c.domains.is_empty());
    }

    #[tokio::test]
    async fn alias_format_defaults_to_words_and_survives_a_server_change() {
        let s = Store::in_memory().unwrap();
        assert_eq!(s.settings().await.unwrap().alias_format, NameFormat::Words);
        s.set_alias_format(NameFormat::Characters).await.unwrap();
        assert_eq!(
            s.settings().await.unwrap().alias_format,
            NameFormat::Characters
        );
        s.set_stalwart("http://a".into(), "key".into())
            .await
            .unwrap();
        s.set_stalwart("http://b".into(), "key".into())
            .await
            .unwrap();
        assert_eq!(
            s.settings().await.unwrap().alias_format,
            NameFormat::Characters
        );
        // A value nobody understands falls back to the default.
        s.set("alias_format", "uuid".into()).await.unwrap();
        assert_eq!(s.settings().await.unwrap().alias_format, NameFormat::Words);
    }

    #[tokio::test]
    async fn only_the_first_admin_is_created() {
        let s = Store::in_memory().unwrap();
        assert!(s.create_admin("hash-1".into()).await.unwrap());
        assert!(!s.create_admin("hash-2".into()).await.unwrap());
        assert_eq!(
            s.settings().await.unwrap().admin_hash.as_deref(),
            Some("hash-1")
        );
    }

    #[tokio::test]
    async fn changing_the_server_revokes_tokens_and_resets_setup() {
        let s = Store::in_memory().unwrap();
        s.create_admin("admin-hash".into()).await.unwrap();
        s.set_stalwart("http://mail-a".into(), "key".into())
            .await
            .unwrap();
        s.set_domains(&["example.org".into()]).await.unwrap();
        s.finish_setup().await.unwrap();
        let token = s
            .create_token("test".into(), "a".into(), "alice".into())
            .await
            .unwrap();
        s.record_alias_created("a", "shop@example.org")
            .await
            .unwrap();
        // Equivalent URLs and key rotation must preserve mailbox bindings.
        s.set_stalwart("http://mail-a:80/".into(), "rotated-key".into())
            .await
            .unwrap();
        assert!(s.find_token(&token).await.unwrap().is_some());
        assert_eq!(s.settings().await.unwrap().domains, ["example.org"]);
        assert!(s.settings().await.unwrap().setup_done);
        assert_eq!(s.alias_created_dates("a").await.unwrap().len(), 1);
        s.set_stalwart("http://mail-b".into(), "new-key".into())
            .await
            .unwrap();
        assert!(s.find_token(&token).await.unwrap().is_none());
        assert!(s.alias_created_dates("a").await.unwrap().is_empty());
        let settings = s.settings().await.unwrap();
        assert!(settings.domains.is_empty());
        assert!(!settings.setup_done);
        assert_eq!(settings.admin_hash.as_deref(), Some("admin-hash"));
        // Returning to the original URL must not resurrect a revoked token.
        s.set_stalwart("http://mail-a".into(), "key".into())
            .await
            .unwrap();
        assert!(s.find_token(&token).await.unwrap().is_none());
    }
}
