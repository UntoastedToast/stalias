use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::response::{IntoResponse, Redirect};
use serde::Deserialize;

use super::{UiResult, backend, in_wizard, render};
use crate::auth::Admin;
use crate::stalwart::Client;
use crate::state::AppState;

#[derive(Template)]
#[template(path = "pages/settings.html")]
struct SettingsPage {
    wizard: bool,
    url: String,
    key_saved: bool,
    error: Option<String>,
}

/// `GET /settings`: the Stalwart connection form (step 2 of the wizard).
pub async fn page(_: Admin, State(state): State<AppState>) -> UiResult {
    render_settings(&state, None, None).await
}

/// `typed_url` is what the admin submitted before an error, so it does not have to be typed again.
async fn render_settings(
    state: &AppState,
    error: Option<String>,
    typed_url: Option<String>,
) -> UiResult {
    let settings = state.store.settings().await?;
    let page = SettingsPage {
        wizard: in_wizard(state).await?,
        url: typed_url.or(settings.stalwart_url).unwrap_or_default(),
        key_saved: settings.stalwart_key.is_some(),
        error,
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
        return render_settings(&state, Some("Please enter the API key.".into()), Some(url)).await;
    };
    // Validate before saving, so a typo cannot lock the instance out of Stalwart.
    let check = match Client::new(&url, &key) {
        Ok(client) => client.check().await,
        Err(e) => Err(e),
    };
    if let Err(e) = check {
        return render_settings(&state, Some(e.to_string()), Some(url)).await;
    }
    state.set_connection(url, key).await?;
    Ok(Redirect::to("/").into_response())
}

struct DomainRow {
    name: String,
    selected: bool,
}

#[derive(Template)]
#[template(path = "pages/domains.html")]
struct DomainsPage {
    wizard: bool,
    domains: Vec<DomainRow>,
    /// Stalwart answered, but has no domain yet.
    no_domains: bool,
    error: Option<String>,
}

/// `GET /settings/domains`: the domain picker (step 3 of the wizard).
pub async fn domains_page(_: Admin, State(state): State<AppState>) -> UiResult {
    render_domains(&state, None).await
}

async fn render_domains(state: &AppState, error: Option<String>) -> UiResult {
    let backend = backend(state)?;
    let selected = state.store.settings().await?.domains;
    // Nothing chosen yet (first run): offer every domain, the user only has to confirm.
    let first_run = selected.is_empty();
    let (domains, error) = match backend.client.domains().await {
        Ok(list) => (
            list.into_iter()
                .map(|d| DomainRow {
                    selected: first_run || selected.contains(&d.name),
                    name: d.name,
                })
                .collect(),
            error,
        ),
        Err(e) => (Vec::new(), Some(e.to_string())),
    };
    let no_domains = domains.is_empty() && error.is_none();
    let page = DomainsPage {
        wizard: in_wizard(state).await?,
        domains,
        no_domains,
        error,
    };
    Ok(render(page)?.into_response())
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
    let backend = backend(&state)?;
    // Only accept names Stalwart actually knows, whatever the form claims.
    let known = match backend.client.domains().await {
        Ok(list) => list,
        Err(e) => return render_domains(&state, Some(e.to_string())).await,
    };
    let chosen: Vec<String> = known
        .into_iter()
        .map(|d| d.name)
        .filter(|name| form.domain.contains(name))
        .collect();
    if chosen.is_empty() {
        return render_domains(&state, Some("Select at least one domain.".into())).await;
    }
    state.store.set_domains(&chosen).await?;
    Ok(Redirect::to("/").into_response())
}
