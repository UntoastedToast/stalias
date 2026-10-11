use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect};
use axum_extra::extract::cookie::{Cookie, CookieJar};
use serde::Deserialize;

use super::{Denied, UiResult, admit, in_wizard, render, settings};
use crate::auth::{Admin, ClientIp, SESSION_COOKIE};
use crate::password::bounded_normalize;
use crate::state::AppState;

#[derive(Template)]
#[template(path = "pages/password.html")]
struct PasswordPage {
    wizard: bool,
    password_error: Option<String>,
    password_autofocus: bool,
}

pub async fn page(_: Admin, State(state): State<AppState>) -> UiResult {
    if !in_wizard(&state).await? {
        return settings::render_security(&state, None).await;
    }
    Ok(render(PasswordPage {
        wizard: true,
        password_error: None,
        password_autofocus: true,
    })?
    .into_response())
}

#[derive(Deserialize)]
pub struct PasswordForm {
    current_password: String,
    password: String,
    confirm: String,
}

pub async fn submit(
    _: Admin,
    State(state): State<AppState>,
    ClientIp(client): ClientIp,
    jar: CookieJar,
    Form(form): Form<PasswordForm>,
) -> UiResult {
    let wizard = in_wizard(&state).await?;
    let reject = async |d: Denied| -> UiResult {
        let page = if wizard {
            render(PasswordPage {
                wizard,
                password_error: Some(d.message.clone()),
                password_autofocus: true,
            })?
            .into_response()
        } else {
            settings::render_security(&state, Some(d.message.clone())).await?
        };
        Ok(d.respond(page))
    };
    let Some(session) = jar.get(SESSION_COOKIE).map(|c| c.value().to_owned()) else {
        return Ok(Redirect::to("/login").into_response());
    };
    let Some(expected_hash) = state.store.settings().await?.admin_hash else {
        return Ok(Redirect::to("/setup").into_response());
    };
    // All raw password fields are bounded before any NFC or expensive processing.
    let current = match bounded_normalize(&form.current_password) {
        Ok(value) => value,
        Err(e) => return reject(e.into()).await,
    };
    for value in [&form.password, &form.confirm] {
        if let Err(e) = bounded_normalize(value) {
            return reject(e.into()).await;
        }
    }
    let (work, attempt) = match admit(&state.passwords, &state.login_limiter, client) {
        Ok(admitted) => admitted,
        Err(d) => return reject(d).await,
    };
    match work.verify(current, expected_hash.clone()).await {
        Ok(true) => attempt.success(),
        Ok(false) => {
            return reject(Denied::new(
                StatusCode::UNAUTHORIZED,
                "Wrong current password.",
            ))
            .await;
        }
        Err(e) => return reject(e.into()).await,
    }
    let new_hash = match work
        .new_hash(form.password, form.confirm, state.breach_check.as_deref())
        .await
    {
        Ok(h) => h,
        Err(e) => return reject(e.into()).await,
    };
    if !state
        .store
        .change_admin_password(expected_hash, new_hash, Some(session))
        .await?
    {
        return reject(Denied::new(
            StatusCode::CONFLICT,
            "The password or session changed while this request was running. Please log in again.",
        ))
        .await;
    }
    Ok((
        jar.remove(Cookie::build(SESSION_COOKIE).path("/")),
        Redirect::to("/login?password_changed=1"),
    )
        .into_response())
}
