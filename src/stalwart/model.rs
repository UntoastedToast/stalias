use std::collections::BTreeMap;

use serde::Deserialize;

/// A mail domain in Stalwart.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Domain {
    pub id: String,
    pub name: String,
}

/// An alias as Stalwart stores it on an account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alias {
    /// Position key in Stalwart's alias map. Not stable across removals.
    pub index: String,
    pub name: String,
    pub domain_id: String,
    pub enabled: bool,
    pub description: Option<String>,
}

/// A user account (mailbox) with its aliases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    pub name: String,
    pub email: Option<String>,
    pub aliases: Vec<Alias>,
}

#[derive(Deserialize)]
pub(super) struct RawAlias {
    name: String,
    #[serde(rename = "domainId")]
    domain_id: String,
    #[serde(default = "default_true")]
    enabled: bool,
    description: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct RawAccount {
    id: String,
    name: String,
    #[serde(rename = "@type")]
    pub(super) kind: Option<String>,
    #[serde(rename = "emailAddress")]
    email: Option<String>,
    #[serde(default)]
    aliases: BTreeMap<String, RawAlias>,
}

fn default_true() -> bool {
    true
}

impl From<RawAccount> for Account {
    fn from(raw: RawAccount) -> Self {
        let mut aliases: Vec<Alias> = raw
            .aliases
            .into_iter()
            .map(|(index, a)| Alias {
                index,
                name: a.name,
                domain_id: a.domain_id,
                enabled: a.enabled,
                description: a.description,
            })
            .collect();
        // Keys are numeric strings, so a plain string sort would put "10" before "2".
        aliases.sort_by_key(|a| a.index.parse::<u32>().unwrap_or(u32::MAX));
        Account {
            id: raw.id,
            name: raw.name,
            email: raw.email,
            aliases,
        }
    }
}
