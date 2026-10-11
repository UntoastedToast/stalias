use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect};
use axum_extra::extract::cookie::CookieJar;
use serde::Deserialize;

use super::{Denied, UiResult, admit, render};
use crate::auth::{ClientIp, constant_time_eq, is_https, session_cookie};
use crate::password;
use crate::state::AppState;

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
    let reject = |d: Denied| -> UiResult {
        let page = render(SetupPage {
            error: Some(d.message.clone()),
        })?;
        Ok(d.respond(page))
    };
    for value in [&form.password, &form.confirm] {
        if let Err(e) = password::bounded_normalize(value) {
            return reject(e.into());
        }
    }
    let (work, attempt) = match admit(&state.passwords, &state.setup_limiter, client) {
        Ok(admitted) => admitted,
        Err(d) => return reject(d),
    };
    let code_ok = state
        .setup_code
        .as_deref()
        .is_some_and(|code| constant_time_eq(form.code.trim(), code));
    if !code_ok {
        return reject(Denied::new(
            StatusCode::UNAUTHORIZED,
            "The setup code is wrong. Look for it in the container log.",
        ));
    }
    attempt.success();
    let hash = match work
        .new_hash(form.password, form.confirm, state.breach_check.as_deref())
        .await
    {
        Ok(h) => h,
        Err(e) => return reject(e.into()),
    };
    if !state.store.create_admin(hash.clone()).await? {
        return Ok(Redirect::to("/login").into_response());
    }
    let Some(session) = state.store.create_session(hash).await? else {
        return Ok(Redirect::to("/login").into_response());
    };
    Ok((
        jar.add(session_cookie(session, is_https(&headers))),
        Redirect::to("/"),
    )
        .into_response())
}
