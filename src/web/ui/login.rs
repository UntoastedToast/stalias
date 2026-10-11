use askama::Template;
use axum::Form;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect};
use axum_extra::extract::cookie::{Cookie, CookieJar};
use serde::Deserialize;

use super::{Denied, UiError, UiResult, admit, render};
use crate::auth::{ClientIp, SESSION_COOKIE, is_https, session_cookie};
use crate::password::bounded_normalize;
use crate::state::AppState;

#[derive(Template)]
#[template(path = "pages/login.html")]
struct LoginPage {
    error: Option<String>,
    password_changed: bool,
}

#[derive(Deserialize)]
pub struct LoginQuery {
    password_changed: Option<String>,
}

/// `GET /login`.
pub async fn page(State(state): State<AppState>, Query(query): Query<LoginQuery>) -> UiResult {
    if state.store.settings().await?.admin_hash.is_none() {
        return Err(UiError::Redirect("/setup"));
    }
    Ok(render(LoginPage {
        error: None,
        password_changed: query.password_changed.as_deref() == Some("1"),
    })?
    .into_response())
}

/// Fields of `POST /login`.
#[derive(Deserialize)]
pub struct LoginForm {
    password: String,
}

/// `POST /login`: checks the admin password. Failed attempts are rate-limited per client and
/// per account, and password checks are limited to a few at a time.
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
    let deny = |d: Denied| -> UiResult {
        let page = render(LoginPage {
            error: Some(d.message.clone()),
            password_changed: false,
        })?;
        Ok(d.respond(page))
    };
    let password = match bounded_normalize(&form.password) {
        Ok(p) => p,
        Err(e) => return deny(e.into()),
    };
    let (work, attempt) = match admit(&state.passwords, &state.login_limiter, client) {
        Ok(admitted) => admitted,
        Err(d) => return deny(d),
    };
    match work.verify(password, hash.clone()).await {
        Ok(true) => attempt.success(),
        Ok(false) => {
            return deny(Denied::new(StatusCode::UNAUTHORIZED, "Wrong password."));
        }
        Err(e) => return deny(e.into()),
    }
    let Some(session) = state.store.create_session(hash).await? else {
        return deny(Denied::new(
            StatusCode::CONFLICT,
            "The password changed while you were logging in. Please try again.",
        ));
    };
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
