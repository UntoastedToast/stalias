use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect};
use axum_extra::extract::cookie::{Cookie, CookieJar};
use serde::Deserialize;

use super::{UiError, UiResult, render};
use crate::auth::{ClientIp, SESSION_COOKIE, is_https, session_cookie, verify_password};
use crate::state::AppState;

#[derive(Template)]
#[template(path = "pages/login.html")]
struct LoginPage {
    error: Option<String>,
}

/// `GET /login`.
pub async fn page(State(state): State<AppState>) -> UiResult {
    if state.store.settings().await?.admin_hash.is_none() {
        return Err(UiError::Redirect("/setup"));
    }
    Ok(render(LoginPage { error: None })?.into_response())
}

/// Fields of `POST /login`.
#[derive(Deserialize)]
pub struct LoginForm {
    password: String,
}

/// `POST /login`: checks the admin password. Failed attempts are rate-limited per client.
pub async fn submit(
    State(state): State<AppState>,
    ClientIp(client): ClientIp,
    headers: HeaderMap,
    jar: CookieJar,
    Form(form): Form<LoginForm>,
) -> UiResult {
    let Some(hash) = state.store.settings().await?.admin_hash else {
        return Err(UiError::Redirect("/setup"));
    };
    let deny = |status: StatusCode, msg: &str| -> UiResult {
        let page = render(LoginPage {
            error: Some(msg.into()),
        })?;
        Ok((status, page).into_response())
    };
    let Some(mut attempt) = state.login_limiter.try_attempt(client) else {
        return deny(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many failed attempts. Try again in a minute.",
        );
    };
    if !verify_password(form.password, hash).await {
        return deny(StatusCode::UNAUTHORIZED, "Wrong password.");
    }
    attempt.success();
    let session = state.store.create_session().await?;
    Ok((
        jar.add(session_cookie(session, is_https(&headers))),
        Redirect::to("/"),
    )
        .into_response())
}

/// `POST /logout`: ends the session.
pub async fn logout(State(state): State<AppState>, jar: CookieJar) -> UiResult {
    if let Some(cookie) = jar.get(SESSION_COOKIE) {
        state.store.delete_session(cookie.value()).await?;
    }
    let jar = jar.remove(Cookie::build(SESSION_COOKIE).path("/"));
    Ok((jar, Redirect::to("/login")).into_response())
}
