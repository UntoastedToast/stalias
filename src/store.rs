//! SQLite storage for stalias' own state: settings, API tokens and login sessions.
//! Aliases themselves live in Stalwart and are never copied here.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use rand::distr::{Alphanumeric, SampleString};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

const SESSION_TTL_SECS: i64 = 7 * 24 * 3600;
const TOKEN_PREFIX: &str = "stalias_";
const UPSERT_SETTING: &str = "INSERT INTO settings (key, value) VALUES (?1, ?2)
     ON CONFLICT(key) DO UPDATE SET value = excluded.value";

/// A failed database operation.
#[derive(Debug, thiserror::Error)]
#[error("database error: {0}")]
pub struct Error(#[from] rusqlite::Error);

type Result<T> = std::result::Result<T, Error>;

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
}

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

/// Cheap to clone. Every query runs on tokio's blocking pool, so a slow disk never
/// stalls the async workers.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    /// The file holds the Stalwart API key, so it is created readable by the owner only.
    /// SQLite gives the WAL and SHM files the same mode.
    pub fn open(path: &Path) -> Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // Errors here resurface when SQLite opens the file below.
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(path);
        }
        Self::init(Connection::open(path)?)
    }

    /// An empty database in memory, for tests.
    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS tokens (
                 id INTEGER PRIMARY KEY,
                 hash TEXT NOT NULL UNIQUE,
                 label TEXT NOT NULL,
                 account_id TEXT NOT NULL,
                 account_name TEXT NOT NULL,
                 created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS sessions (hash TEXT PRIMARY KEY, expires_at INTEGER NOT NULL);",
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    async fn run<T: Send + 'static>(
        &self,
        query: impl FnOnce(&Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            // A panic in another query must not take the whole store down with it.
            query(&conn.lock().unwrap_or_else(PoisonError::into_inner))
        })
        .await
        .expect("database task panicked")
    }

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

    /// Saves the Stalwart URL and key.
    ///
    /// A different server revokes all tokens and clears the chosen domains and the finished-setup
    /// flag, because their ids belong to the old server. Key rotation keeps them.
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
                // Account ids and domains are meaningful only on their original server.
                tx.execute("DELETE FROM tokens", [])?;
                tx.execute(
                    "DELETE FROM settings WHERE key IN ('domains', 'setup_done')",
                    [],
                )?;
            }
            for (name, value) in [("stalwart_url", url), ("stalwart_key", key)] {
                tx.execute(UPSERT_SETTING, params![name, value])?;
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

    /// Marks the first-run wizard as completed.
    pub async fn finish_setup(&self) -> Result<()> {
        self.set("setup_done", "1".into()).await
    }

    async fn set(&self, key: &'static str, value: String) -> Result<()> {
        self.run(move |conn| {
            conn.execute(UPSERT_SETTING, params![key, value])?;
            Ok(())
        })
        .await
    }

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

fn same_server_url(a: &str, b: &str) -> bool {
    match (url::Url::parse(a), url::Url::parse(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
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

fn hash(plain: &str) -> String {
    Sha256::digest(plain.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970")
        .as_secs() as i64
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
        // Equivalent URLs and key rotation must preserve mailbox bindings.
        s.set_stalwart("http://mail-a:80/".into(), "rotated-key".into())
            .await
            .unwrap();
        assert!(s.find_token(&token).await.unwrap().is_some());
        assert_eq!(s.settings().await.unwrap().domains, ["example.org"]);
        assert!(s.settings().await.unwrap().setup_done);
        s.set_stalwart("http://mail-b".into(), "new-key".into())
            .await
            .unwrap();
        assert!(s.find_token(&token).await.unwrap().is_none());
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

    #[cfg(unix)]
    #[test]
    fn database_file_is_private_to_the_owner() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("stalias-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("stalias.db");
        Store::open(&file).unwrap();
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(mode & 0o077, 0, "mode is {mode:o}");
    }

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
