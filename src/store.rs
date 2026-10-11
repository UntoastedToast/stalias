//! SQLite storage for stalias' own state: settings, API tokens, login sessions and the
//! creation dates of aliases. Aliases themselves live in Stalwart and are never copied here;
//! Stalwart records no creation date, so stalias keeps the ones it knows about.
//!
//! This module owns the connection. Each submodule adds the queries for one kind of data to
//! [`Store`] and brings its own table definition.

mod alias_dates;
mod sessions;
mod settings;
mod tokens;

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};

pub use settings::Settings;
pub use tokens::Token;

/// A failed database operation.
#[derive(Debug, thiserror::Error)]
#[error("database error: {0}")]
pub struct Error(#[from] rusqlite::Error);

type Result<T> = std::result::Result<T, Error>;

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

    /// Local recovery must never create a database or initialize an empty instance.
    ///
    /// # Errors
    ///
    /// Fails when the file is missing or is not a stalias database.
    pub fn open_existing(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // A foreign SQLite file would otherwise fail later with a raw "no such table".
        conn.query_row("SELECT 1 FROM settings LIMIT 1", [], |_| Ok(()))
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(()),
                e => Err(e),
            })?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA journal_mode = WAL;")?;
        for schema in [
            settings::SCHEMA,
            tokens::SCHEMA,
            sessions::SCHEMA,
            alias_dates::SCHEMA,
        ] {
            conn.execute_batch(schema)?;
        }
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Runs a query on the blocking pool.
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
}

/// SHA-256 of a secret as lower-case hex. Tokens and session ids are random and long, so a
/// plain hash is enough to keep them unreadable in the database.
fn hash(plain: &str) -> String {
    Sha256::digest(plain.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The current time in Unix seconds.
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970")
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn hashes_are_stable_hex() {
        assert_eq!(hash("a"), hash("a"));
        assert_ne!(hash("a"), hash("b"));
        assert_eq!(hash("a").len(), 64);
    }
}
