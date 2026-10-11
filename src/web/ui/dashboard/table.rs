//! What the alias table shows: reading one mailbox's aliases, applying the search and filters,
//! cutting out a page, and the pieces the templates need for the filter bar and the pager.

use askama::Template;
use serde::Deserialize;
use time::OffsetDateTime;

use crate::aliases::{AliasEntry, AliasFilter, Created, Sort, Status};
use crate::state::AppState;
use crate::web::ui::{SelectOption, UiError, backend, choices, selected_value};

/// Aliases shown per page.
pub(super) const PAGE_SIZE: usize = 15;

pub(super) struct AliasRow {
    pub(super) address: String,
    pub(super) description: String,
    /// The creation day, or empty when stalias never saw the alias being created.
    pub(super) created: String,
    pub(super) enabled: bool,
}

impl AliasRow {
    pub(super) fn new(entry: AliasEntry, created_at: Option<i64>) -> Self {
        Self {
            address: entry.address(),
            description: entry.description.unwrap_or_default(),
            created: created_at.map(day).unwrap_or_default(),
            enabled: entry.enabled,
        }
    }
}

/// The calendar day (UTC) of a Unix timestamp, as `2026-10-10`.
pub(super) fn day(timestamp: i64) -> String {
    OffsetDateTime::from_unix_timestamp(timestamp)
        .map(|t| t.date().to_string())
        .unwrap_or_default()
}

pub(super) fn now() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

/// A single table row, returned to htmx after creating or toggling an alias.
#[derive(Template)]
#[template(path = "components/alias_row.html")]
pub(super) struct AliasRowFragment<'a> {
    pub(super) a: &'a AliasRow,
    account_id: &'a str,
}

/// Title and text of the empty state that stands in for a table without rows.
pub(super) struct Empty {
    pub(super) title: &'static str,
    pub(super) text: &'static str,
}

/// One entry of the pager: a page link or a gap between distant pages.
pub(super) struct PageItem {
    pub(super) number: usize,
    /// Query string that opens this page.
    pub(super) query: String,
    pub(super) current: bool,
    pub(super) gap: bool,
}

/// The filtered, paged alias table with its summary line and pager.
pub(super) struct Results {
    pub(super) rows: Vec<AliasRow>,
    pub(super) summary: String,
    pub(super) empty: Option<Empty>,
    pub(super) error: Option<String>,
    /// Empty when everything fits on one page.
    pub(super) pages: Vec<PageItem>,
    /// Query strings for the neighbouring pages.
    pub(super) prev: Option<String>,
    pub(super) next: Option<String>,
}

impl Results {
    pub(super) fn nothing() -> Self {
        Self {
            rows: Vec::new(),
            summary: String::new(),
            empty: Some(NO_ALIASES),
            error: None,
            pages: Vec::new(),
            prev: None,
            next: None,
        }
    }

    pub(super) fn failed(error: String) -> Self {
        Self {
            empty: None,
            error: Some(error),
            ..Self::nothing()
        }
    }
}

pub(super) const NO_ALIASES: Empty = Empty {
    title: "No aliases yet",
    text: "Create one above, or let Bitwarden create it.",
};

pub(super) const NO_MATCHES: Empty = Empty {
    title: "No matching aliases",
    text: "Try another search or reset the filters.",
};

/// The pages to link: the first, the last and the neighbourhood of the current one, with a
/// gap wherever pages are left out.
pub(super) fn page_items(
    query: &ListQuery,
    account_id: &str,
    current: usize,
    pages: usize,
) -> Vec<PageItem> {
    let mut items = Vec::new();
    let mut previous = 0;
    for number in 1..=pages {
        if number != 1 && number != pages && number.abs_diff(current) > 1 {
            continue;
        }
        if number > previous + 1 {
            items.push(PageItem {
                number: 0,
                query: String::new(),
                current: false,
                gap: true,
            });
        }
        items.push(PageItem {
            number,
            query: query.link(account_id, number),
            current: number == current,
            gap: false,
        });
        previous = number;
    }
    items
}

/// The table on its own, returned to htmx when the search or filters change.
#[derive(Template)]
#[template(path = "components/alias_results.html")]
pub(super) struct ResultsFragment<'a> {
    pub(super) account_id: &'a str,
    pub(super) results: &'a Results,
}

/// Values of the search and filter bar, with the choices for each select.
pub(super) struct FilterBar {
    pub(super) query: String,
    pub(super) status: Vec<SelectOption>,
    pub(super) status_value: String,
    pub(super) created: Vec<SelectOption>,
    pub(super) created_value: String,
    pub(super) sort: Vec<SelectOption>,
    pub(super) sort_value: String,
    /// Only offered when more than one domain is in use.
    pub(super) domains: Vec<SelectOption>,
    pub(super) domain_value: String,
    pub(super) multiple_domains: bool,
}

/// Query string of the dashboard and of the alias table: the mailbox, the search and filters,
/// and the page. Every value is optional and anything unknown means "no filter".
#[derive(Deserialize, Default)]
pub struct ListQuery {
    pub(super) account: Option<String>,
    pub(super) q: Option<String>,
    pub(super) status: Option<String>,
    pub(super) domain: Option<String>,
    pub(super) created: Option<String>,
    pub(super) sort: Option<String>,
    pub(super) page: Option<String>,
}

impl ListQuery {
    pub(super) fn text(value: &Option<String>) -> &str {
        value.as_deref().unwrap_or_default()
    }

    pub(super) fn filter(&self) -> AliasFilter {
        AliasFilter {
            query: Self::text(&self.q).to_owned(),
            status: Status::parse(Self::text(&self.status)),
            domain: Some(Self::text(&self.domain).to_owned()).filter(|d| !d.is_empty()),
            created: Created::parse(Self::text(&self.created)),
            sort: Sort::parse(Self::text(&self.sort)),
        }
    }

    pub(super) fn page(&self) -> usize {
        Self::text(&self.page).parse().unwrap_or(1).max(1)
    }

    /// The query string for another page of the same search.
    pub(super) fn link(&self, account_id: &str, page: usize) -> String {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query.append_pair("account", account_id);
        for (name, value) in [
            ("q", &self.q),
            ("status", &self.status),
            ("domain", &self.domain),
            ("created", &self.created),
            ("sort", &self.sort),
        ] {
            if !Self::text(value).is_empty() {
                query.append_pair(name, Self::text(value));
            }
        }
        query.append_pair("page", &page.to_string());
        query.finish()
    }
}

pub(super) fn filter_bar(query: &ListQuery, filter: &AliasFilter, domains: &[String]) -> FilterBar {
    let status = choices(
        &[
            ("all", "All"),
            ("active", "Active"),
            ("disabled", "Disabled"),
        ],
        filter.status.as_str(),
    );
    let created = choices(
        &[
            ("any", "Any time"),
            ("day", "Last 24 hours"),
            ("week", "Last 7 days"),
            ("month", "Last 30 days"),
        ],
        filter.created.as_str(),
    );
    let sort = choices(
        &[
            ("newest", "Newest first"),
            ("oldest", "Oldest first"),
            ("address", "Address A to Z"),
        ],
        filter.sort.as_str(),
    );
    let mut domain_choices = vec![SelectOption {
        value: String::new(),
        label: "All domains".into(),
        selected: filter.domain.is_none(),
    }];
    domain_choices.extend(domains.iter().map(|d| {
        SelectOption {
            value: d.clone(),
            label: d.clone(),
            selected: filter
                .domain
                .as_deref()
                .is_some_and(|f| f.eq_ignore_ascii_case(d)),
        }
    }));
    FilterBar {
        query: ListQuery::text(&query.q).to_owned(),
        status_value: selected_value(&status),
        status,
        created_value: selected_value(&created),
        created,
        sort_value: selected_value(&sort),
        sort,
        domain_value: selected_value(&domain_choices),
        domains: domain_choices,
        multiple_domains: domains.len() > 1,
    }
}

/// Reads one mailbox's aliases, applies the search and filters and cuts out one page.
pub(super) async fn load_results(
    state: &AppState,
    account_id: &str,
    query: &ListQuery,
) -> Result<Results, UiError> {
    let backend = backend(state)?;
    let entries = match backend.aliases.list(account_id).await {
        Ok(list) => list,
        Err(e) => return Ok(Results::failed(e.to_string())),
    };
    let total = entries.len();
    let created = state.store.alias_created_dates(account_id).await?;
    let filter = query.filter();
    let matching = filter.apply(entries, &created, now());

    let pages = matching.len().div_ceil(PAGE_SIZE).max(1);
    let page = query.page().min(pages);
    let first = (page - 1) * PAGE_SIZE;
    let shown: Vec<AliasRow> = matching
        .iter()
        .skip(first)
        .take(PAGE_SIZE)
        .map(|e| AliasRow::new(e.clone(), created.get(&e.address().to_lowercase()).copied()))
        .collect();

    let plural = |n: usize| if n == 1 { "alias" } else { "aliases" };
    let summary = if total == 0 {
        String::new()
    } else if filter.is_active() && pages == 1 {
        format!("{} of {total} {} match", matching.len(), plural(total))
    } else if pages == 1 {
        format!("{total} {}", plural(total))
    } else {
        let what = if filter.is_active() { "matching " } else { "" };
        format!(
            "Showing {}\u{2013}{} of {} {what}aliases",
            first + 1,
            first + shown.len(),
            matching.len()
        )
    };
    Ok(Results {
        rows: shown,
        summary,
        empty: Some(if total > 0 && matching.is_empty() {
            NO_MATCHES
        } else {
            NO_ALIASES
        }),
        error: None,
        pages: if pages > 1 {
            page_items(query, account_id, page, pages)
        } else {
            Vec::new()
        },
        prev: (page > 1).then(|| query.link(account_id, page - 1)),
        next: (page < pages).then(|| query.link(account_id, page + 1)),
    })
}

pub(super) fn row_fragment(row: &AliasRow, account_id: &str) -> Result<String, UiError> {
    Ok(AliasRowFragment { a: row, account_id }.render()?)
}
