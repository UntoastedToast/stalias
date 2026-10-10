//! The browser UI: server-rendered pages (askama) enhanced with htmx.

use askama::Template;
use axum::Router;
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};

use crate::stalwart::Account;
use crate::state::{AppState, Backend};
use crate::store::Settings;

mod dashboard;
mod login;
mod settings;
mod setup;
mod tokens;

/// Routes of the browser UI.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(dashboard::page))
        .route("/aliases", post(dashboard::create))
        .route("/aliases/list", get(dashboard::list))
        .route("/aliases/toggle", post(dashboard::toggle))
        .route("/aliases/delete", post(dashboard::delete))
        .route("/login", get(login::page).post(login::submit))
        .route("/logout", post(login::logout))
        .route("/setup", get(setup::page).post(setup::submit))
        .route("/settings", get(settings::page).post(settings::save))
        .route(
            "/settings/domains",
            get(settings::domains_page).post(settings::save_domains),
        )
        .route("/tokens", get(tokens::page).post(tokens::create))
        .route("/tokens/{id}/delete", post(tokens::delete))
}

/// Anything a handler can bail out with: a redirect (e.g. to the next setup step) or a 500.
pub enum UiError {
    Redirect(&'static str),
    Internal(String),
}

impl IntoResponse for UiError {
    fn into_response(self) -> Response {
        match self {
            UiError::Redirect(to) => Redirect::to(to).into_response(),
            UiError::Internal(msg) => {
                tracing::error!("{msg}");
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal error",
                )
                    .into_response()
            }
        }
    }
}

/// Lets handlers use `?` on store, template and state errors alike.
impl<E: std::error::Error> From<E> for UiError {
    fn from(e: E) -> Self {
        UiError::Internal(e.to_string())
    }
}

type UiResult = Result<Response, UiError>;

fn render<T: Template>(page: T) -> Result<Html<String>, UiError> {
    Ok(Html(page.render()?))
}

/// The backend, or a redirect to the settings page while Stalwart is not connected.
fn backend(state: &AppState) -> Result<std::sync::Arc<Backend>, UiError> {
    state.backend().ok_or(UiError::Redirect("/settings"))
}

/// First setup step that still needs doing, if any. Tokens are only required to finish the
/// wizard; revoking them later must not lock the admin out of the dashboard.
fn next_step(settings: &Settings, token_count: usize) -> Option<&'static str> {
    if settings.admin_hash.is_none() {
        Some("/setup")
    } else if settings.stalwart_url.is_none() || settings.stalwart_key.is_none() {
        Some("/settings")
    } else if settings.domains.is_empty() {
        Some("/settings/domains")
    } else if token_count == 0 && !settings.setup_done {
        Some("/tokens")
    } else {
        None
    }
}

/// True until the dashboard has been reached once with everything set up. It stays true
/// after the last step's data exists, so the wizard does not end before its last page was seen.
async fn in_wizard(state: &AppState) -> Result<bool, UiError> {
    Ok(!state.store.settings().await?.setup_done)
}

/// Public address of this instance as the browser sees it, for the setup instructions.
fn base_url(headers: &HeaderMap) -> String {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let scheme = header("x-forwarded-proto").unwrap_or("http");
    let host = header("x-forwarded-host")
        .or(header("host"))
        .unwrap_or("localhost");
    format!("{scheme}://{host}")
}

/// One entry of a select box.
pub struct SelectOption {
    pub value: String,
    pub label: String,
    pub selected: bool,
}

fn account_options(accounts: &[Account], selected: Option<&str>) -> Vec<SelectOption> {
    accounts
        .iter()
        .map(|a| SelectOption {
            value: a.id.clone(),
            label: match &a.email {
                Some(email) => format!("{} ({email})", a.name),
                None => a.name.clone(),
            },
            selected: selected == Some(a.id.as_str()),
        })
        .collect()
}

/// The value to submit for a select: the chosen option, or the first one.
fn selected_value(options: &[SelectOption]) -> String {
    options
        .iter()
        .find(|o| o.selected)
        .or(options.first())
        .map(|o| o.value.clone())
        .unwrap_or_default()
}

/// True for requests sent by htmx on a form with its own `hx-post`, which want a
/// fragment back. Boosted navigation and plain browser requests get the usual redirect.
fn wants_fragment(headers: &HeaderMap) -> bool {
    headers.contains_key("hx-request") && !headers.contains_key("hx-boosted")
}

#[derive(Template)]
#[template(path = "components/toast.html")]
struct ToastFragment<'a> {
    category: &'a str,
    title: &'a str,
    description: Option<&'a str>,
}

/// A toast for htmx to append to the page's toaster (see `templates/components/toast.html`).
fn toast(category: &str, title: &str, description: Option<&str>) -> Result<String, UiError> {
    Ok(ToastFragment {
        category,
        title,
        description,
    }
    .render()?)
}

/// Tells htmx to leave the page as it is (used with an error toast).
fn no_swap() -> (HeaderName, HeaderValue) {
    (
        HeaderName::from_static("hx-reswap"),
        HeaderValue::from_static("none"),
    )
}
