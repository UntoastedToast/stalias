use askama::Template;
use axum::Form;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::{Html, IntoResponse, Redirect};
use serde::Deserialize;

use super::{
    SelectOption, UiError, UiResult, account_options, backend, next_step, no_swap, render,
    selected_value, toast, wants_fragment,
};
use crate::aliases::{AliasEntry, NewAlias};
use crate::auth::Admin;
use crate::state::AppState;

struct AliasRow {
    address: String,
    description: String,
    enabled: bool,
}

impl From<AliasEntry> for AliasRow {
    fn from(entry: AliasEntry) -> Self {
        Self {
            address: entry.address(),
            description: entry.description.unwrap_or_default(),
            enabled: entry.enabled,
        }
    }
}

/// A single table row, returned to htmx after creating or toggling an alias.
#[derive(Template)]
#[template(path = "components/alias_row.html")]
struct AliasRowFragment<'a> {
    a: &'a AliasRow,
    account_id: &'a str,
}

#[derive(Template)]
#[template(path = "pages/dashboard.html")]
struct DashboardPage {
    /// The dashboard only opens once the wizard is done, so this is always false.
    wizard: bool,
    mailboxes: Vec<SelectOption>,
    mailbox_value: String,
    account_id: String,
    domains: Vec<SelectOption>,
    domain_value: String,
    aliases: Vec<AliasRow>,
    error: Option<String>,
}

/// Query string of the dashboard: the mailbox to show.
#[derive(Deserialize)]
pub struct DashboardQuery {
    account: Option<String>,
}

/// `GET /`: the aliases of a mailbox. Redirects to the next unfinished setup step.
pub async fn page(
    _: Admin,
    State(state): State<AppState>,
    Query(query): Query<DashboardQuery>,
) -> UiResult {
    render_dashboard(&state, query.account, None).await
}

/// Error messages are rendered directly and never travel through the URL, so nobody can
/// link to the dashboard with text of their choosing on it.
async fn render_dashboard(
    state: &AppState,
    account: Option<String>,
    mut error: Option<String>,
) -> UiResult {
    let settings = state.store.settings().await?;
    let tokens = state.store.tokens().await?;
    if let Some(step) = next_step(&settings, tokens.len()) {
        return Err(UiError::Redirect(step));
    }
    if !settings.setup_done {
        state.store.finish_setup().await?;
    }
    let backend = backend(state)?;

    let accounts = match backend.client.accounts().await {
        Ok(list) => list,
        Err(e) => {
            error.get_or_insert(e.to_string());
            Vec::new()
        }
    };
    // Default to the mailbox of the first token, since that is where Bitwarden writes.
    let wanted = account.or_else(|| tokens.first().map(|t| t.account_id.clone()));
    let selected = accounts
        .iter()
        .find(|a| Some(&a.id) == wanted.as_ref())
        .or(accounts.first())
        .map(|a| a.id.clone());

    let aliases = match &selected {
        Some(id) => match backend.aliases.list(id).await {
            Ok(list) => list.into_iter().map(AliasRow::from).collect(),
            Err(e) => {
                error.get_or_insert(e.to_string());
                Vec::new()
            }
        },
        None => Vec::new(),
    };

    let mailboxes = account_options(&accounts, selected.as_deref());
    let domains: Vec<SelectOption> = settings
        .domains
        .into_iter()
        .enumerate()
        .map(|(i, d)| SelectOption {
            value: d.clone(),
            label: d,
            selected: i == 0,
        })
        .collect();
    let page = DashboardPage {
        wizard: false,
        mailbox_value: selected_value(&mailboxes),
        mailboxes,
        account_id: selected.unwrap_or_default(),
        domain_value: selected_value(&domains),
        domains,
        aliases,
        error,
    };
    Ok(render(page)?.into_response())
}

/// Where a form without htmx goes next: back to the alias list of the mailbox, or, when the
/// action failed, to that same list with the error shown.
async fn back(state: &AppState, account: String, error: Option<String>) -> UiResult {
    if error.is_some() {
        return render_dashboard(state, Some(account), error).await;
    }
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query.append_pair("account", &account);
    Ok(Redirect::to(&format!("/?{}", query.finish())).into_response())
}

fn failed_fragment(message: &str) -> UiResult {
    let toast = toast("error", "That didn't work", Some(message))?;
    Ok(([no_swap()], Html(toast)).into_response())
}

fn row_fragment(row: &AliasRow, account_id: &str) -> Result<String, UiError> {
    Ok(AliasRowFragment { a: row, account_id }.render()?)
}

/// Fields of `POST /aliases`.
#[derive(Deserialize)]
pub struct CreateForm {
    account: String,
    domain: String,
    local_part: String,
    description: String,
}

/// `POST /aliases`: creates an alias. htmx gets a table row and a toast, other clients a redirect.
pub async fn create(
    _: Admin,
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<CreateForm>,
) -> UiResult {
    let backend = backend(&state)?;
    let non_empty = |s: String| Some(s.trim().to_owned()).filter(|s| !s.is_empty());
    let new = NewAlias {
        local_part: non_empty(form.local_part),
        description: non_empty(form.description),
    };
    let allowed = state.store.settings().await?.domains;
    let result = if allowed.iter().any(|d| d.eq_ignore_ascii_case(&form.domain)) {
        backend
            .aliases
            .create(&form.account, &form.domain, new)
            .await
    } else {
        Err(crate::aliases::Error::UnknownDomain(form.domain.clone()))
    };
    if !wants_fragment(&headers) {
        return back(&state, form.account, result.err().map(|e| e.to_string())).await;
    }
    match result {
        Ok(entry) => {
            let row = AliasRow::from(entry);
            let toast = toast("success", "Alias created", Some(&row.address))?;
            let html = row_fragment(&row, &form.account)? + &toast;
            Ok(([("hx-trigger-after-swap", "aliasCreated")], Html(html)).into_response())
        }
        Err(e) => failed_fragment(&e.to_string()),
    }
}

/// Fields of `POST /aliases/toggle`.
#[derive(Deserialize)]
pub struct ToggleForm {
    account: String,
    address: String,
    enabled: bool,
}

/// `POST /aliases/toggle`: enables or disables an alias.
pub async fn toggle(
    _: Admin,
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ToggleForm>,
) -> UiResult {
    let backend = backend(&state)?;
    let result = backend
        .aliases
        .set_enabled(&form.account, &form.address, form.enabled)
        .await;
    if !wants_fragment(&headers) {
        return back(&state, form.account, result.err().map(|e| e.to_string())).await;
    }
    if let Err(e) = result {
        return failed_fragment(&e.to_string());
    }
    let entry = match backend.aliases.get(&form.account, &form.address).await {
        Ok(entry) => entry,
        Err(e) => return failed_fragment(&e.to_string()),
    };
    let row = AliasRow::from(entry);
    let title = if row.enabled {
        "Alias enabled"
    } else {
        "Alias disabled"
    };
    let toast = toast("success", title, Some(&row.address))?;
    let html = row_fragment(&row, &form.account)? + &toast;
    Ok(Html(html).into_response())
}

/// Fields of `POST /aliases/delete`.
#[derive(Deserialize)]
pub struct DeleteForm {
    account: String,
    address: String,
}

/// `POST /aliases/delete`: removes an alias.
pub async fn delete(
    _: Admin,
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<DeleteForm>,
) -> UiResult {
    let result = backend(&state)?
        .aliases
        .delete(&form.account, &form.address)
        .await;
    if !wants_fragment(&headers) {
        return back(&state, form.account, result.err().map(|e| e.to_string())).await;
    }
    match result {
        // Apart from the toast the body is empty, which removes the row from the table.
        Ok(()) => Ok(Html(toast("success", "Alias deleted", Some(&form.address))?).into_response()),
        Err(e) => failed_fragment(&e.to_string()),
    }
}
