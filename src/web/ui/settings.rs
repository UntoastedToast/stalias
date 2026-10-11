use askama::Template;
use axum::Form;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Redirect};
use serde::Deserialize;

use super::{SelectOption, UiError, UiResult, backend, choices, in_wizard, render, selected_value};
use crate::auth::Admin;
use crate::naming::NameFormat;
use crate::stalwart::Client;
use crate::state::AppState;

/// The tabs of the settings page. The server picks the open one, so a redirect after saving
/// returns to the tab the change was made on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Tab {
    #[default]
    Connection,
    Domains,
    Names,
}

impl Tab {
    fn parse(value: &str) -> Self {
        match value {
            "domains" => Self::Domains,
            "names" => Self::Names,
            _ => Self::Connection,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Connection => "connection",
            Self::Domains => "domains",
            Self::Names => "names",
        }
    }

    /// Where to send the browser to open this tab.
    fn location(self) -> String {
        format!("/settings?tab={}", self.as_str())
    }
}

/// What the domain picker shows, in the wizard and in the settings tab alike.
#[derive(Default)]
struct DomainsView {
    rows: Vec<DomainRow>,
    /// Stalwart answered, but has no domain yet.
    no_domains: bool,
    error: Option<String>,
}

struct DomainRow {
    name: String,
    selected: bool,
}

#[derive(Template)]
#[template(path = "pages/settings.html")]
struct SettingsPage {
    wizard: bool,
    tab: &'static str,
    url: String,
    key_saved: bool,
    alias_formats: Vec<SelectOption>,
    alias_format_value: String,
    /// How many names each style can produce, for the hint under the select.
    alias_format_hint: String,
    domains_view: DomainsView,
    /// An error of the connection form.
    error: Option<String>,
}

/// Messages and input to show again after a form was refused.
#[derive(Default)]
struct Feedback {
    connection_error: Option<String>,
    /// The URL the admin typed, so it does not have to be typed again.
    typed_url: Option<String>,
    domains_error: Option<String>,
}

/// Query string of `GET /settings`.
#[derive(Deserialize)]
pub struct SettingsQuery {
    tab: Option<String>,
}

/// `GET /settings`: the Stalwart connection form (step 2 of the wizard). Afterwards the page has
/// three tabs: the connection, the alias domains and the style of alias names.
pub async fn page(
    _: Admin,
    State(state): State<AppState>,
    Query(query): Query<SettingsQuery>,
) -> UiResult {
    let tab = Tab::parse(query.tab.as_deref().unwrap_or_default());
    render_settings(&state, tab, Feedback::default()).await
}

async fn render_settings(state: &AppState, tab: Tab, feedback: Feedback) -> UiResult {
    let settings = state.store.settings().await?;
    let wizard = in_wizard(state).await?;
    let alias_formats = choices(
        &[
            (
                NameFormat::Words.as_str(),
                "Three words (possibly-cosmic-cougar)",
            ),
            (
                NameFormat::Characters.as_str(),
                "Random characters (4gh9nzibm3)",
            ),
        ],
        settings.alias_format.as_str(),
    );
    // The wizard step has no domain list, and the connection may not exist yet.
    let domains_view = if wizard {
        DomainsView::default()
    } else {
        load_domains(state, feedback.domains_error).await?
    };
    let page = SettingsPage {
        wizard,
        tab: tab.as_str(),
        url: feedback
            .typed_url
            .or(settings.stalwart_url)
            .unwrap_or_default(),
        key_saved: settings.stalwart_key.is_some(),
        alias_format_value: selected_value(&alias_formats),
        alias_formats,
        alias_format_hint: format!(
            "Possible names: three words about {}, random characters about {}. \
             Both are far more than needed, and a name that is already taken is replaced by a new one.",
            approximately(NameFormat::Words.combinations()),
            approximately(NameFormat::Characters.combinations()),
        ),
        domains_view,
        error: feedback.connection_error,
    };
    Ok(render(page)?.into_response())
}

/// Fields of `POST /settings`.
#[derive(Deserialize)]
pub struct ConnectionForm {
    url: String,
    key: String,
}

/// `POST /settings`: tests the connection and saves it only if it works.
pub async fn save(
    _: Admin,
    State(state): State<AppState>,
    Form(form): Form<ConnectionForm>,
) -> UiResult {
    let url = form.url.trim().trim_end_matches('/').to_owned();
    let key = if form.key.trim().is_empty() {
        state.store.settings().await?.stalwart_key
    } else {
        Some(form.key.trim().to_owned())
    };
    let Some(key) = key else {
        let feedback = Feedback {
            connection_error: Some("Please enter the API key.".into()),
            typed_url: Some(url),
            ..Feedback::default()
        };
        return render_settings(&state, Tab::Connection, feedback).await;
    };
    // Validate before saving, so a typo cannot lock the instance out of Stalwart.
    let check = match Client::new(&url, &key) {
        Ok(client) => client.check().await,
        Err(e) => Err(e),
    };
    if let Err(e) = check {
        let feedback = Feedback {
            connection_error: Some(e.to_string()),
            typed_url: Some(url),
            ..Feedback::default()
        };
        return render_settings(&state, Tab::Connection, feedback).await;
    }
    state.set_connection(url, key).await?;
    Ok(Redirect::to("/").into_response())
}

/// A large count in words, like `2.1 billion`.
fn approximately(count: u128) -> String {
    const UNITS: [&str; 5] = ["thousand", "million", "billion", "trillion", "quadrillion"];
    let mut value = count as f64;
    let mut unit = None;
    for name in UNITS {
        if value < 1000.0 {
            break;
        }
        value /= 1000.0;
        unit = Some(name);
    }
    match unit {
        Some(unit) => format!("{value:.1} {unit}"),
        None => count.to_string(),
    }
}

/// Fields of `POST /settings/alias-format`.
#[derive(Deserialize)]
pub struct AliasFormatForm {
    alias_format: String,
}

/// `POST /settings/alias-format`: saves the style of generated alias names. An unknown value
/// is ignored, so the setting never ends up in a state the form cannot show.
pub async fn save_alias_format(
    _: Admin,
    State(state): State<AppState>,
    Form(form): Form<AliasFormatForm>,
) -> UiResult {
    if let Some(format) = NameFormat::parse(&form.alias_format) {
        state.store.set_alias_format(format).await?;
    }
    Ok(Redirect::to(&Tab::Names.location()).into_response())
}

#[derive(Template)]
#[template(path = "pages/domains.html")]
struct DomainsPage {
    wizard: bool,
    domains_view: DomainsView,
}

/// `GET /settings/domains`: the domain picker (step 3 of the wizard). Afterwards the picker is a
/// tab of the settings page, so this opens it there.
pub async fn domains_page(_: Admin, State(state): State<AppState>) -> UiResult {
    if !in_wizard(&state).await? {
        return Ok(Redirect::to(&Tab::Domains.location()).into_response());
    }
    render_domains_step(&state, None).await
}

/// The domain picker as its own page, for the wizard.
async fn render_domains_step(state: &AppState, error: Option<String>) -> UiResult {
    backend(state)?;
    let page = DomainsPage {
        wizard: true,
        domains_view: load_domains(state, error).await?,
    };
    Ok(render(page)?.into_response())
}

/// Reads the domains from Stalwart and marks the chosen ones. A failure becomes a message in
/// the picker, so the rest of the page stays usable, for example to fix the connection.
async fn load_domains(state: &AppState, error: Option<String>) -> Result<DomainsView, UiError> {
    let Some(backend) = state.backend() else {
        return Ok(DomainsView {
            error: Some("Connect to Stalwart first.".into()),
            ..DomainsView::default()
        });
    };
    let selected = state.store.settings().await?.domains;
    // Nothing chosen yet (first run): offer every domain, the user only has to confirm.
    let first_run = selected.is_empty();
    Ok(match backend.client.domains().await {
        Ok(list) => {
            let rows: Vec<DomainRow> = list
                .into_iter()
                .map(|d| DomainRow {
                    selected: first_run || selected.contains(&d.name),
                    name: d.name,
                })
                .collect();
            DomainsView {
                no_domains: rows.is_empty() && error.is_none(),
                rows,
                error,
            }
        }
        Err(e) => DomainsView {
            error: Some(e.to_string()),
            ..DomainsView::default()
        },
    })
}

/// Fields of `POST /settings/domains`: one `domain` entry per ticked box.
#[derive(Deserialize)]
pub struct DomainsForm {
    #[serde(default)]
    domain: Vec<String>,
}

/// `POST /settings/domains`: saves the chosen domains.
pub async fn save_domains(
    _: Admin,
    State(state): State<AppState>,
    axum_extra::extract::Form(form): axum_extra::extract::Form<DomainsForm>,
) -> UiResult {
    let wizard = in_wizard(&state).await?;
    let refused = |message: String| async {
        if wizard {
            render_domains_step(&state, Some(message)).await
        } else {
            let feedback = Feedback {
                domains_error: Some(message),
                ..Feedback::default()
            };
            render_settings(&state, Tab::Domains, feedback).await
        }
    };
    let backend = backend(&state)?;
    // Only accept names Stalwart actually knows, whatever the form claims.
    let known = match backend.client.domains().await {
        Ok(list) => list,
        Err(e) => return refused(e.to_string()).await,
    };
    let chosen: Vec<String> = known
        .into_iter()
        .map(|d| d.name)
        .filter(|name| form.domain.contains(name))
        .collect();
    if chosen.is_empty() {
        return refused("Select at least one domain.".into()).await;
    }
    state.store.set_domains(&chosen).await?;
    let to = if wizard {
        "/".to_owned()
    } else {
        Tab::Domains.location()
    };
    Ok(Redirect::to(&to).into_response())
}

#[cfg(test)]
mod tests {
    use super::{Tab, approximately};

    #[test]
    fn large_counts_are_written_in_words() {
        assert_eq!(approximately(999), "999");
        assert_eq!(approximately(1_000), "1.0 thousand");
        assert_eq!(approximately(2_054_000_000), "2.1 billion");
        assert_eq!(approximately(52_617_861), "52.6 million");
        assert_eq!(approximately(3_656_158_440_062_976), "3.7 quadrillion");
    }

    #[test]
    fn unknown_tabs_open_the_connection() {
        assert_eq!(Tab::parse("names"), Tab::Names);
        assert_eq!(Tab::parse("domains"), Tab::Domains);
        assert_eq!(Tab::parse("nonsense"), Tab::Connection);
        assert_eq!(Tab::parse(Tab::Names.as_str()), Tab::Names);
        assert_eq!(Tab::Domains.location(), "/settings?tab=domains");
    }
}
