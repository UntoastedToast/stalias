//! Searching, filtering and sorting the aliases of one mailbox. Stalwart returns a mailbox's
//! aliases in one piece, so this works on the list in memory.

use std::collections::HashMap;

use super::AliasEntry;

/// Which aliases to show by their status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Status {
    #[default]
    All,
    Active,
    Disabled,
}

impl Status {
    /// Reads a form value. Anything unknown means no filter.
    pub fn parse(value: &str) -> Self {
        match value {
            "active" => Self::Active,
            "disabled" => Self::Disabled,
            _ => Self::All,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Active => "active",
            Self::Disabled => "disabled",
        }
    }
}

/// How recently an alias must have been created. Only aliases whose creation stalias saw have
/// a date, so any window other than `Any` leaves out the others.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Created {
    #[default]
    Any,
    Day,
    Week,
    Month,
}

impl Created {
    /// Reads a form value. Anything unknown means no filter.
    pub fn parse(value: &str) -> Self {
        match value {
            "day" => Self::Day,
            "week" => Self::Week,
            "month" => Self::Month,
            _ => Self::Any,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
        }
    }

    fn window_secs(self) -> Option<i64> {
        match self {
            Self::Any => None,
            Self::Day => Some(24 * 3600),
            Self::Week => Some(7 * 24 * 3600),
            Self::Month => Some(30 * 24 * 3600),
        }
    }
}

/// The order of the result.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Sort {
    /// Stalwart numbers aliases in the order they were added, so this is the reverse of it.
    #[default]
    Newest,
    Oldest,
    Address,
}

impl Sort {
    /// Reads a form value. Anything unknown means the default order.
    pub fn parse(value: &str) -> Self {
        match value {
            "oldest" => Self::Oldest,
            "address" => Self::Address,
            _ => Self::Newest,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Newest => "newest",
            Self::Oldest => "oldest",
            Self::Address => "address",
        }
    }
}

/// What the admin typed into the search and filter bar.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AliasFilter {
    /// Matched case-insensitively against the address and the description.
    pub query: String,
    pub status: Status,
    /// Only aliases on this domain.
    pub domain: Option<String>,
    pub created: Created,
    pub sort: Sort,
}

impl AliasFilter {
    /// True when something narrows the list down, as opposed to just ordering it.
    pub fn is_active(&self) -> bool {
        !self.query.trim().is_empty()
            || self.status != Status::All
            || self.domain.is_some()
            || self.created != Created::Any
    }

    /// Keeps the matching aliases and sorts them. `entries` must be in Stalwart's order,
    /// oldest first. `created` maps lower-case addresses to Unix seconds, `now` is the current
    /// time in the same unit.
    pub fn apply(
        &self,
        entries: Vec<AliasEntry>,
        created: &HashMap<String, i64>,
        now: i64,
    ) -> Vec<AliasEntry> {
        let query = self.query.trim().to_lowercase();
        let window = self.created.window_secs();
        let mut kept: Vec<AliasEntry> = entries
            .into_iter()
            .filter(|e| {
                let address = e.address().to_lowercase();
                let text_ok = query.is_empty()
                    || address.contains(&query)
                    || e.description
                        .as_deref()
                        .is_some_and(|d| d.to_lowercase().contains(&query));
                let status_ok = match self.status {
                    Status::All => true,
                    Status::Active => e.enabled,
                    Status::Disabled => !e.enabled,
                };
                let domain_ok = self
                    .domain
                    .as_deref()
                    .is_none_or(|d| e.domain.eq_ignore_ascii_case(d));
                let created_ok = window.is_none_or(|secs| {
                    created
                        .get(&address)
                        .is_some_and(|&at| now.saturating_sub(at) <= secs)
                });
                text_ok && status_ok && domain_ok && created_ok
            })
            .collect();
        match self.sort {
            Sort::Newest => kept.reverse(),
            Sort::Oldest => {}
            Sort::Address => kept.sort_by_key(AliasEntry::address),
        }
        kept
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(local: &str, domain: &str, enabled: bool, description: Option<&str>) -> AliasEntry {
        AliasEntry {
            local_part: local.into(),
            domain: domain.into(),
            enabled,
            description: description.map(Into::into),
        }
    }

    fn sample() -> Vec<AliasEntry> {
        vec![
            entry("github", "example.org", true, Some("Website: github.com")),
            entry("news", "example.org", false, Some("Newsletters")),
            entry("shop", "example.net", true, None),
        ]
    }

    fn names(entries: &[AliasEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.local_part.as_str()).collect()
    }

    #[test]
    fn default_shows_everything_newest_first() {
        let out = AliasFilter::default().apply(sample(), &HashMap::new(), 0);
        assert_eq!(names(&out), ["shop", "news", "github"]);
        assert!(!AliasFilter::default().is_active());
    }

    #[test]
    fn query_matches_address_and_description_case_insensitively() {
        let filter = |q: &str| AliasFilter {
            query: q.into(),
            sort: Sort::Oldest,
            ..Default::default()
        };
        let find = |q: &str| names(&filter(q).apply(sample(), &HashMap::new(), 0)).join(",");
        assert_eq!(find("GITHUB"), "github");
        assert_eq!(find("letters"), "news");
        assert_eq!(find("@example.net"), "shop");
        assert_eq!(find("  shop  "), "shop");
        assert_eq!(find("nothing"), "");
        assert_eq!(find(""), "github,news,shop");
    }

    #[test]
    fn status_and_domain_narrow_the_list() {
        let out = AliasFilter {
            status: Status::Active,
            domain: Some("EXAMPLE.org".into()),
            ..Default::default()
        }
        .apply(sample(), &HashMap::new(), 0);
        assert_eq!(names(&out), ["github"]);
        let off = AliasFilter {
            status: Status::Disabled,
            ..Default::default()
        }
        .apply(sample(), &HashMap::new(), 0);
        assert_eq!(names(&off), ["news"]);
    }

    #[test]
    fn creation_window_leaves_out_unknown_and_old_aliases() {
        let day = 24 * 3600;
        let created = HashMap::from([
            ("github@example.org".to_owned(), 1_000_000 - 2 * day),
            ("news@example.org".to_owned(), 1_000_000 - 3600),
        ]);
        let in_window = |created_filter| {
            let out = AliasFilter {
                created: created_filter,
                ..Default::default()
            }
            .apply(sample(), &created, 1_000_000);
            names(&out).join(",")
        };
        assert_eq!(in_window(Created::Day), "news");
        assert_eq!(in_window(Created::Week), "news,github");
        assert_eq!(in_window(Created::Any), "shop,news,github");
    }

    #[test]
    fn sorts_by_address() {
        let out = AliasFilter {
            sort: Sort::Address,
            ..Default::default()
        }
        .apply(sample(), &HashMap::new(), 0);
        assert_eq!(names(&out), ["github", "news", "shop"]);
    }

    #[test]
    fn unknown_form_values_fall_back_to_the_defaults() {
        assert_eq!(Status::parse("bogus"), Status::All);
        assert_eq!(Created::parse(""), Created::Any);
        assert_eq!(Sort::parse("x"), Sort::Newest);
        for s in [Status::Active, Status::Disabled] {
            assert_eq!(Status::parse(s.as_str()), s);
        }
        for c in [Created::Day, Created::Week, Created::Month] {
            assert_eq!(Created::parse(c.as_str()), c);
        }
        for s in [Sort::Oldest, Sort::Address] {
            assert_eq!(Sort::parse(s.as_str()), s);
        }
    }
}
