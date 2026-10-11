//! The subset of the Addy.io REST API that password managers and scripts use to
//! create and manage aliases. <https://app.addy.io/docs>

use std::sync::Arc;

use axum::extract::{FromRequestParts, Path};
use axum::http::{StatusCode, header, request::Parts};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::aliases::{self, AliasEntry, NewAlias};
use crate::naming::NameFormat;
use crate::stalwart;
use crate::state::{AppState, Backend};
use crate::store::{Store, Token};

/// Routes of the Addy.io-compatible API under `/api/v1`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/aliases", post(create).get(list))
        .route(
            "/api/v1/aliases/{id}",
            axum::routing::get(show).patch(update).delete(remove),
        )
        .route("/api/v1/active-aliases", post(activate))
        .route("/api/v1/active-aliases/{id}", delete(deactivate))
}

/// An error response: a status code and a JSON body in Addy.io's shape.
#[derive(Debug)]
pub struct ApiError(StatusCode, Value);

impl ApiError {
    fn message(status: StatusCode, msg: &str) -> Self {
        Self(status, json!({ "message": msg }))
    }

    fn field(field: &str, msg: &str) -> Self {
        Self(
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({ "message": msg, "errors": { field: [msg] } }),
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(self.1)).into_response()
    }
}

impl From<aliases::Error> for ApiError {
    fn from(e: aliases::Error) -> Self {
        use aliases::Error::*;
        match e {
            UnknownDomain(_) => Self::field("domain", "The selected domain is not available."),
            InvalidLocalPart => Self::field("local_part", "The local part is not valid."),
            InvalidDescription => Self::field("description", "The description is too long."),
            Conflict => Self::field("local_part", "This alias is already in use."),
            AliasNotFound => Self::message(StatusCode::NOT_FOUND, "Alias not found."),
            AccountNotFound => Self::message(
                StatusCode::NOT_FOUND,
                "The mailbox for this token no longer exists.",
            ),
            Stalwart(err) => {
                tracing::error!("stalwart request failed: {err}");
                let msg = match err {
                    stalwart::Error::Http(_) => "The mail server could not be reached.",
                    _ => "The mail server returned an error.",
                };
                Self::message(StatusCode::BAD_GATEWAY, msg)
            }
        }
    }
}

/// An authenticated request: the token plus the backend it will act on.
pub struct Authed {
    token: Token,
    backend: Arc<Backend>,
    store: Store,
    domains: Vec<String>,
    alias_format: NameFormat,
    _connection: tokio::sync::OwnedRwLockReadGuard<()>,
}

impl FromRequestParts<AppState> for Authed {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let unauthenticated = || ApiError::message(StatusCode::UNAUTHORIZED, "Unauthenticated.");
        let plain = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(bearer_token)
            .ok_or_else(unauthenticated)?;
        let connection = state.connection_read().await;
        let internal = |e: crate::store::Error| {
            tracing::error!("{e}");
            ApiError::message(StatusCode::INTERNAL_SERVER_ERROR, "Internal error.")
        };
        let token = state
            .store
            .find_token(plain)
            .await
            .map_err(internal)?
            .ok_or_else(unauthenticated)?;
        let settings = state.store.settings().await.map_err(internal)?;
        let backend = state
            .backend()
            .filter(|_| !settings.domains.is_empty())
            .ok_or_else(|| {
                ApiError::message(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "stalias is not set up yet.",
                )
            })?;
        Ok(Self {
            token,
            backend,
            store: state.store.clone(),
            domains: settings.domains,
            alias_format: settings.alias_format,
            _connection: connection,
        })
    }
}

/// The token of an `Authorization: Bearer <token>` header. The scheme is case-insensitive.
fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

#[derive(Deserialize, Default)]
struct CreateBody {
    domain: Option<String>,
    description: Option<String>,
    format: Option<String>,
    local_part: Option<String>,
}

async fn create(
    auth: Authed,
    Json(body): Json<CreateBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    // Clients may send "random" or nothing and let the server choose.
    let domain = match body.domain.as_deref() {
        None | Some("") | Some("random") => auth.domains[0].clone(),
        Some(d) => auth
            .domains
            .iter()
            .find(|allowed| allowed.eq_ignore_ascii_case(d))
            .cloned()
            .ok_or_else(|| ApiError::field("domain", "The selected domain is not available."))?,
    };
    // Without a `format` the server decides, like Addy.io does with its account setting.
    let (local_part, format) = match body.format.as_deref() {
        Some("custom") => (
            Some(body.local_part.ok_or_else(|| {
                ApiError::field(
                    "local_part",
                    "The local part is required for the custom format.",
                )
            })?),
            auth.alias_format,
        ),
        requested => (
            None,
            requested
                .and_then(NameFormat::parse)
                .unwrap_or(auth.alias_format),
        ),
    };
    let entry = auth
        .backend
        .aliases
        .create(
            &auth.token.account_id,
            &domain,
            NewAlias {
                local_part,
                description: body.description.filter(|d| !d.is_empty()),
                format,
            },
        )
        .await?;
    // Stalwart records no creation date, so stalias keeps it for the search filters. The alias
    // exists either way, so a failure here is only logged.
    if let Err(e) = auth
        .store
        .record_alias_created(&auth.token.account_id, &entry.address())
        .await
    {
        tracing::warn!("could not save alias metadata: {e}");
    }
    Ok((
        StatusCode::CREATED,
        Json(json!({ "data": alias_json(&entry, &auth.token) })),
    ))
}

async fn list(auth: Authed) -> Result<Json<Value>, ApiError> {
    let entries = auth.backend.aliases.list(&auth.token.account_id).await?;
    let data: Vec<Value> = entries.iter().map(|e| alias_json(e, &auth.token)).collect();
    Ok(Json(json!({ "data": data })))
}

async fn show(auth: Authed, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({ "data": find(&auth, &id).await? })))
}

#[derive(Deserialize)]
struct UpdateBody {
    /// Outer `None`: the field was not sent and must stay as it is. `Some(None)`: `null`, which clears it.
    #[serde(default, deserialize_with = "present")]
    description: Option<Option<String>>,
}

fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}

async fn update(
    auth: Authed,
    Path(id): Path<String>,
    Json(body): Json<UpdateBody>,
) -> Result<Json<Value>, ApiError> {
    if let Some(description) = body.description {
        let description = description.filter(|d| !d.is_empty());
        auth.backend
            .aliases
            .set_description(&auth.token.account_id, &id, description.as_deref())
            .await?;
    }
    Ok(Json(json!({ "data": find(&auth, &id).await? })))
}

async fn remove(auth: Authed, Path(id): Path<String>) -> Result<StatusCode, ApiError> {
    auth.backend
        .aliases
        .delete(&auth.token.account_id, &id)
        .await?;
    if let Err(e) = auth.store.forget_alias(&auth.token.account_id, &id).await {
        tracing::warn!("could not save alias metadata: {e}");
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ActivateBody {
    id: String,
}

async fn activate(auth: Authed, Json(body): Json<ActivateBody>) -> Result<Json<Value>, ApiError> {
    auth.backend
        .aliases
        .set_enabled(&auth.token.account_id, &body.id, true)
        .await?;
    Ok(Json(json!({ "data": find(&auth, &body.id).await? })))
}

async fn deactivate(auth: Authed, Path(id): Path<String>) -> Result<StatusCode, ApiError> {
    auth.backend
        .aliases
        .set_enabled(&auth.token.account_id, &id, false)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn find(auth: &Authed, address: &str) -> Result<Value, ApiError> {
    let entry = auth
        .backend
        .aliases
        .get(&auth.token.account_id, address)
        .await?;
    Ok(alias_json(&entry, &auth.token))
}

/// Addy.io identifies aliases by UUID. Stalwart has no stable alias id, so the
/// address itself serves as the id. Fields stalias cannot know are left empty.
fn alias_json(e: &AliasEntry, token: &Token) -> Value {
    json!({
        "id": e.address(),
        "user_id": token.account_id,
        "aliasable_id": null,
        "aliasable_type": null,
        "local_part": e.local_part,
        "extension": null,
        "domain": e.domain,
        "email": e.address(),
        "active": e.enabled,
        "description": e.description,
        "from_name": null,
        "attached_recipients_only": false,
        "emails_forwarded": 0,
        "emails_blocked": 0,
        "emails_replied": 0,
        "emails_sent": 0,
        "recipients": [],
        "last_forwarded": null,
        "last_blocked": null,
        "last_replied": null,
        "last_sent": null,
        "created_at": null,
        "updated_at": null,
        "deleted_at": null,
    })
}
