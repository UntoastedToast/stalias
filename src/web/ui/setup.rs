use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect};
use axum_extra::extract::cookie::CookieJar;
use serde::Deserialize;

use super::{UiResult, render};
use crate::auth::{ClientIp, constant_time_eq, hash_password, is_https, session_cookie};
use crate::state::AppState;

const MIN_PASSWORD_LEN: usize = 8;

#[derive(Template)]
#[template(path = "pages/setup.html")]
struct SetupPage {
    error: Option<String>,
}

/// `GET /setup`: the first-run form (step 1 of the wizard).
pub async fn page(State(state): State<AppState>) -> UiResult {
    if state.store.settings().await?.admin_hash.is_some() {
        return Ok(Redirect::to("/").into_response());
    }
    Ok(render(SetupPage { error: None })?.into_response())
}

/// Fields of `POST /setup`.
#[derive(Deserialize)]
pub struct SetupForm {
    code: String,
    password: String,
    confirm: String,
}

/// `POST /setup`: creates the admin account if the setup code from the log is right.
pub async fn submit(
    State(state): State<AppState>,
    ClientIp(client): ClientIp,
    headers: HeaderMap,
    jar: CookieJar,
    Form(form): Form<SetupForm>,
) -> UiResult {
    if state.store.settings().await?.admin_hash.is_some() {
        return Ok(Redirect::to("/").into_response());
    }
    let reject = |status: StatusCode, msg: &str| -> UiResult {
        let page = render(SetupPage {
            error: Some(msg.into()),
        })?;
        Ok((status, page).into_response())
    };
    let Some(mut attempt) = state.login_limiter.try_attempt(client) else {
        return reject(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many failed attempts. Try again in a minute.",
        );
    };
    let code_ok = state
        .setup_code
        .as_deref()
        .is_some_and(|code| constant_time_eq(form.code.trim(), code));
    if !code_ok {
        return reject(
            StatusCode::UNAUTHORIZED,
            "The setup code is wrong. Look for it in the container log.",
        );
    }
    attempt.success();
    if form.password.chars().count() < MIN_PASSWORD_LEN {
        return reject(
            StatusCode::UNPROCESSABLE_ENTITY,
            "The password must have at least 8 characters.",
        );
    }
    if form.password != form.confirm {
        return reject(
            StatusCode::UNPROCESSABLE_ENTITY,
            "The passwords do not match.",
        );
    }
    if !state
        .store
        .create_admin(hash_password(form.password).await)
        .await?
    {
        return Ok(Redirect::to("/login").into_response());
    }
    let session = state.store.create_session().await?;
    Ok((
        jar.add(session_cookie(session, is_https(&headers))),
        Redirect::to("/"),
    )
        .into_response())
}
