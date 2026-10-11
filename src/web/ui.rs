//! The browser UI: server-rendered pages (askama) enhanced with htmx.

use std::net::IpAddr;
use std::sync::Arc;

use askama::Template;
use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};

use crate::auth::{LoginAttempt, LoginLimiter, RateLimit};
use crate::password::{PasswordError, PasswordProcessor, PasswordWork, ProcessingError};
use crate::stalwart::Account;
use crate::state::{AppState, Backend};
use crate::store::Settings;

mod dashboard;
mod login;
mod password;
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
        .route(
            "/login",
            get(login::page)
                .post(login::submit)
                .layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route("/logout", post(login::logout))
        .route(
            "/setup",
            get(setup::page)
                .post(setup::submit)
                .layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route("/settings", get(settings::page).post(settings::save))
        .route(
            "/settings/password",
            get(password::page)
                .post(password::submit)
                .layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route("/settings/alias-format", post(settings::save_alias_format))
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

/// A refused password form: the status, the message for the page and an optional `Retry-After`.
struct Denied {
    status: StatusCode,
    message: String,
    retry: Option<u64>,
}

impl Denied {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            retry: None,
        }
    }

    /// The page to show again, with this status and `Retry-After` header.
    fn respond(self, page: impl IntoResponse) -> Response {
        let mut response = (self.status, page).into_response();
        if let Some(seconds) = self.retry {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
        }
        response
    }
}

/// Maps only categorical errors into the UI; no secret-dependent worker error is logged.
impl From<ProcessingError> for Denied {
    fn from(error: ProcessingError) -> Self {
        let (status, retry) = match &error {
            ProcessingError::Invalid(_) => (StatusCode::UNPROCESSABLE_ENTITY, None),
            ProcessingError::Busy => (StatusCode::SERVICE_UNAVAILABLE, Some(1)),
            ProcessingError::Breach(_) => (StatusCode::SERVICE_UNAVAILABLE, None),
            ProcessingError::Worker | ProcessingError::Hash => {
                (StatusCode::INTERNAL_SERVER_ERROR, None)
            }
        };
        Self {
            status,
            message: error.to_string(),
            retry,
        }
    }
}

impl From<PasswordError> for Denied {
    fn from(error: PasswordError) -> Self {
        ProcessingError::from(error).into()
    }
}

impl From<RateLimit> for Denied {
    fn from(limit: RateLimit) -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: "Too many failed attempts. Please try again later.".into(),
            retry: Some(limit.retry_after),
        }
    }
}

/// Reserves computing capacity first, so overload consumes no authentication attempt, then one
/// attempt of the limiter. The slot is released again when the limiter refuses.
fn admit(
    passwords: &PasswordProcessor,
    limiter: &Arc<LoginLimiter>,
    client: IpAddr,
) -> Result<(PasswordWork, LoginAttempt), Denied> {
    let work = passwords.reserve()?;
    let attempt = limiter.try_attempt(client)?;
    Ok((work, attempt))
}

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

/// Select options from `(value, label)` pairs, with the one equal to `current` selected.
fn choices(items: &[(&str, &str)], current: &str) -> Vec<SelectOption> {
    items
        .iter()
        .map(|(value, label)| SelectOption {
            value: (*value).to_owned(),
            label: (*label).to_owned(),
            selected: *value == current,
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
