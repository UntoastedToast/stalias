//! Alias operations on top of the Stalwart client.
//!
//! Stalwart numbers aliases by position and renumbers them on removal, and it has
//! no optimistic locking. Every change therefore re-reads the account and locates
//! the alias by name while holding a lock.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use rand::seq::IndexedRandom;
use serde_json::{Map, Value, json};

mod filter;

pub use filter::{AliasFilter, Created, Sort, Status};

use crate::stalwart::{self, Account, Alias, Client, Domain};

const RANDOM_LEN: usize = 10;
const RANDOM_ATTEMPTS: usize = 5;
const LOCAL_PART_MAX: usize = 64;
const DESCRIPTION_MAX: usize = 255;
/// Domains rarely change, so listing and editing aliases reuse one lookup for a while.
const DOMAINS_TTL: Duration = Duration::from_secs(60);

/// Why an alias operation failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Stalwart(#[from] stalwart::Error),
    #[error("mailbox not found")]
    AccountNotFound,
    #[error("alias not found")]
    AliasNotFound,
    #[error("unknown domain: {0}")]
    UnknownDomain(String),
    #[error("invalid local part")]
    InvalidLocalPart,
    #[error("description is too long")]
    InvalidDescription,
    #[error("address is already in use")]
    Conflict,
}

/// An alias with its domain resolved to a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasEntry {
    pub local_part: String,
    pub domain: String,
    pub enabled: bool,
    pub description: Option<String>,
}

impl AliasEntry {
    /// The full address, `local@domain`. The API uses it as the alias id.
    pub fn address(&self) -> String {
        format!("{}@{}", self.local_part, self.domain)
    }
}

/// Input for [`AliasService::create`].
#[derive(Debug, Default)]
pub struct NewAlias {
    /// Chosen by the caller; a random one is generated when absent.
    pub local_part: Option<String>,
    pub description: Option<String>,
}

/// Reads and changes the aliases of Stalwart mailboxes. Writes are serialised by one lock.
pub struct AliasService {
    client: Arc<Client>,
    write_lock: Arc<tokio::sync::Mutex<()>>,
    domains: Mutex<Option<(Instant, Arc<Vec<Domain>>)>>,
}

impl AliasService {
    /// Creates a service with its own write lock. The app shares one lock through `AppState`.
    pub fn new(client: Arc<Client>) -> Self {
        Self::with_write_lock(client, Arc::default())
    }

    /// Backends for the same instance share this lock, including during a reload.
    pub(crate) fn with_write_lock(
        client: Arc<Client>,
        write_lock: Arc<tokio::sync::Mutex<()>>,
    ) -> Self {
        Self {
            client,
            write_lock,
            domains: Mutex::default(),
        }
    }

    /// The domain list, from the cache while it is fresh.
    async fn domains(&self) -> Result<Arc<Vec<Domain>>, Error> {
        let cached = self
            .domains
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .filter(|(at, _)| at.elapsed() < DOMAINS_TTL)
            .map(|(_, list)| list.clone());
        match cached {
            Some(list) => Ok(list),
            None => self.fresh_domains().await,
        }
    }

    async fn fresh_domains(&self) -> Result<Arc<Vec<Domain>>, Error> {
        let list = Arc::new(self.client.domains().await?);
        *self.domains.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((Instant::now(), list.clone()));
        Ok(list)
    }

    /// All aliases of a mailbox whose domain Stalwart knows.
    pub async fn list(&self, account_id: &str) -> Result<Vec<AliasEntry>, Error> {
        let account = self.account(account_id).await?;
        let domains = self.domains().await?;
        Ok(account
            .aliases
            .iter()
            .filter_map(|a| entry(a, &domains))
            .collect())
    }

    /// One alias by its address (case-insensitive).
    pub async fn get(&self, account_id: &str, address: &str) -> Result<AliasEntry, Error> {
        self.list(account_id)
            .await?
            .into_iter()
            .find(|e| e.address().eq_ignore_ascii_case(address))
            .ok_or(Error::AliasNotFound)
    }

    /// Creates an alias on `domain`, with a random local part unless `new` names one.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Conflict`] when the address is taken, and with a validation error
    /// when the domain, local part or description is not acceptable.
    pub async fn create(
        &self,
        account_id: &str,
        domain: &str,
        new: NewAlias,
    ) -> Result<AliasEntry, Error> {
        // Addresses are case-insensitive; Stalwart stores them in lower case.
        let new = NewAlias {
            local_part: new.local_part.map(|l| l.to_ascii_lowercase()),
            ..new
        };
        if let Some(local) = &new.local_part {
            validate_local_part(local)?;
        }
        validate_description(new.description.as_deref())?;
        let _guard = self.write_lock.lock().await;
        // A new alias is rare and must not be refused over a stale domain list.
        let domains = self.fresh_domains().await?;
        let found = domains
            .iter()
            .find(|d| d.name.eq_ignore_ascii_case(domain))
            .ok_or_else(|| Error::UnknownDomain(domain.to_owned()))?;
        let domain_id = found.id.clone();
        let domain_name = found.name.clone();

        let attempts = if new.local_part.is_some() {
            1
        } else {
            RANDOM_ATTEMPTS
        };
        for _ in 0..attempts {
            let local = new.local_part.clone().unwrap_or_else(random_local_part);
            let account = self.account(account_id).await?;
            if account
                .aliases
                .iter()
                .any(|a| a.domain_id == domain_id && a.name == local)
            {
                continue;
            }
            // Stalwart accepts the mailbox's own address as an alias, but disabling or deleting
            // it would not stop mail, which the UI would wrongly suggest.
            if account
                .email
                .as_deref()
                .is_some_and(|own| own.eq_ignore_ascii_case(&format!("{local}@{domain_name}")))
            {
                continue;
            }
            // Positions are renumbered on removal, so max + 1 never overwrites an entry.
            let next = account
                .aliases
                .iter()
                .filter_map(|a| a.index.parse::<u32>().ok())
                .max()
                .map_or(0, |m| m + 1);
            let mut alias = json!({ "name": local, "domainId": domain_id });
            if let Some(desc) = &new.description {
                alias["description"] = json!(desc);
            }
            match self
                .client
                .patch_account(account_id, patch(format!("aliases/{next}"), alias))
                .await
            {
                Ok(()) => {
                    return Ok(AliasEntry {
                        local_part: local,
                        domain: domain.to_ascii_lowercase(),
                        enabled: true,
                        description: new.description,
                    });
                }
                // Another mailbox owns this address; for random names just try again.
                Err(stalwart::Error::Conflict) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(Error::Conflict)
    }

    /// Enables or disables an alias. A disabled alias rejects incoming mail.
    pub async fn set_enabled(
        &self,
        account_id: &str,
        address: &str,
        enabled: bool,
    ) -> Result<(), Error> {
        self.modify(account_id, address, |index| {
            patch(format!("aliases/{index}/enabled"), json!(enabled))
        })
        .await
    }

    /// Sets the description of an alias. `None` removes it.
    pub async fn set_description(
        &self,
        account_id: &str,
        address: &str,
        description: Option<&str>,
    ) -> Result<(), Error> {
        validate_description(description)?;
        self.modify(account_id, address, |index| {
            patch(format!("aliases/{index}/description"), json!(description))
        })
        .await
    }

    /// Removes an alias for good.
    pub async fn delete(&self, account_id: &str, address: &str) -> Result<(), Error> {
        self.modify(account_id, address, |index| {
            patch(format!("aliases/{index}"), Value::Null)
        })
        .await
    }

    async fn modify(
        &self,
        account_id: &str,
        address: &str,
        build: impl FnOnce(&str) -> Map<String, Value>,
    ) -> Result<(), Error> {
        let _guard = self.write_lock.lock().await;
        let account = self.account(account_id).await?;
        let domains = self.domains().await?;
        let alias = account
            .aliases
            .iter()
            .find(|a| entry(a, &domains).is_some_and(|e| e.address().eq_ignore_ascii_case(address)))
            .ok_or(Error::AliasNotFound)?;
        self.client
            .patch_account(account_id, build(&alias.index))
            .await?;
        Ok(())
    }

    async fn account(&self, id: &str) -> Result<Account, Error> {
        self.client.account(id).await?.ok_or(Error::AccountNotFound)
    }
}

fn entry(alias: &Alias, domains: &[stalwart::Domain]) -> Option<AliasEntry> {
    let domain = domains.iter().find(|d| d.id == alias.domain_id)?;
    Some(AliasEntry {
        local_part: alias.name.clone(),
        domain: domain.name.clone(),
        enabled: alias.enabled,
        description: alias.description.clone(),
    })
}

fn patch(key: String, value: Value) -> Map<String, Value> {
    Map::from_iter([(key, value)])
}

fn random_local_part() -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    (0..RANDOM_LEN)
        .map(|_| *CHARS.choose(&mut rng).expect("non-empty") as char)
        .collect()
}

fn validate_local_part(local: &str) -> Result<(), Error> {
    let ok = !local.is_empty()
        && local.len() <= LOCAL_PART_MAX
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
        && local.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        });
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidLocalPart)
    }
}

fn validate_description(description: Option<&str>) -> Result<(), Error> {
    match description {
        Some(d) if d.chars().count() > DESCRIPTION_MAX => Err(Error::InvalidDescription),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_names_are_valid_and_distinct() {
        let a = random_local_part();
        let b = random_local_part();
        assert_eq!(a.len(), RANDOM_LEN);
        assert!(validate_local_part(&a).is_ok());
        assert_ne!(a, b);
    }

    #[test]
    fn local_part_validation() {
        for ok in ["shop", "a.b", "news-2024", "x_y"] {
            assert!(validate_local_part(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            ".a",
            "a.",
            "a..b",
            "Upper",
            "sp ace",
            "a@b",
            &"x".repeat(65),
        ] {
            assert!(validate_local_part(bad).is_err(), "{bad}");
        }
    }
}
