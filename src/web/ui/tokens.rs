use askama::Template;
use axum::Form;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect};
use serde::Deserialize;

use super::{
    SelectOption, UiResult, account_options, backend, base_url, in_wizard, render, selected_value,
    toast, wants_fragment,
};
use crate::auth::Admin;
use crate::state::AppState;
use crate::store::Token;

struct NewToken {
    value: String,
    account_name: String,
}

#[derive(Template)]
#[template(path = "pages/tokens.html")]
struct TokensPage {
    wizard: bool,
    accounts: Vec<SelectOption>,
    account_value: String,
    tokens: Vec<Token>,
    new_token: Option<NewToken>,
    /// Stalwart answered, but has no mailbox yet.
    no_accounts: bool,
    default_domain: String,
    base_url: String,
    error: Option<String>,
}

async fn render_tokens(
    state: &AppState,
    headers: &HeaderMap,
    new_token: Option<NewToken>,
    mut error: Option<String>,
) -> UiResult {
    let backend = backend(state)?;
    let accounts = match backend.client.accounts().await {
        Ok(list) => account_options(&list, list.first().map(|a| a.id.as_str())),
        Err(e) => {
            error.get_or_insert(e.to_string());
            Vec::new()
        }
    };
    let no_accounts = accounts.is_empty() && error.is_none();
    let page = TokensPage {
        wizard: in_wizard(state).await?,
        account_value: selected_value(&accounts),
        accounts,
        tokens: state.store.tokens().await?,
        new_token,
        no_accounts,
        default_domain: state
            .store
            .settings()
            .await?
            .domains
            .into_iter()
            .next()
            .unwrap_or_default(),
        base_url: base_url(headers),
        error,
    };
    Ok(render(page)?.into_response())
}

/// `GET /tokens`: the API tokens (step 4 of the wizard).
pub async fn page(_: Admin, State(state): State<AppState>, headers: HeaderMap) -> UiResult {
    render_tokens(&state, &headers, None, None).await
}

/// Fields of `POST /tokens`.
#[derive(Deserialize)]
pub struct CreateForm {
    account_id: String,
    label: String,
}

/// `POST /tokens`: creates a token for a mailbox and shows it once.
pub async fn create(
    _: Admin,
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<CreateForm>,
) -> UiResult {
    let _connection = state.connection_read().await;
    let backend = backend(&state)?;
    let account = match backend.client.account(&form.account_id).await {
        Ok(Some(account)) => account,
        Ok(None) => {
            return render_tokens(
                &state,
                &headers,
                None,
                Some("That mailbox no longer exists.".into()),
            )
            .await;
        }
        Err(e) => return render_tokens(&state, &headers, None, Some(e.to_string())).await,
    };
    let label = match form.label.trim() {
        "" => account.name.as_str(),
        label => label,
    };
    let value = state
        .store
        .create_token(label.to_owned(), account.id.clone(), account.name.clone())
        .await?;
    let new_token = NewToken {
        value,
        account_name: account.name,
    };
    render_tokens(&state, &headers, Some(new_token), None).await
}

/// `POST /tokens/{id}/delete`: revokes a token.
pub async fn delete(
    _: Admin,
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> UiResult {
    state.store.delete_token(id).await?;
    if wants_fragment(&headers) {
        // Apart from the toast the body is empty, which removes the row from the table.
        return Ok(axum::response::Html(toast("success", "Token revoked", None)?).into_response());
    }
    Ok(Redirect::to("/tokens").into_response())
}
