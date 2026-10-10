//! HTTP layer: the Addy.io-compatible API and the browser UI.

use axum::Router;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use crate::state::AppState;

/// The version of stalias, shown in the sidebar.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod addy;
pub mod assets;
mod ui;

/// Builds the whole application: API, UI, static files and health check, wrapped in the
/// security middleware.
pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(addy::routes())
        .merge(ui::routes())
        .route("/static/{file}", get(assets::serve))
        .route(
            "/favicon.ico",
            get(|| async { axum::response::Redirect::permanent("/static/logo.svg") }),
        )
        .route("/healthz", get(|| async { "ok" }))
        .layer(middleware::from_fn(reject_cross_site))
        .layer(middleware::map_response(security_headers))
        .with_state(state)
}

/// Browser forms authenticate with a cookie, so a page on another site must not be able to
/// submit them. `SameSite=Lax` already stops that; this also refuses it explicitly, using
/// `Sec-Fetch-Site` (or `Origin` in older browsers). The token API is exempt: it uses a
/// bearer token that a foreign page cannot attach.
async fn reject_cross_site(req: Request, next: Next) -> Response {
    let changes_state = !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if changes_state && !req.uri().path().starts_with("/api/") && is_cross_site(req.headers()) {
        return (
            StatusCode::FORBIDDEN,
            "Cross-site request refused. Behind a reverse proxy, forward the original Host header \
             (or set X-Forwarded-Host).",
        )
            .into_response();
    }
    next.run(req).await
}

fn is_cross_site(headers: &HeaderMap) -> bool {
    let value = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    if let Some(site) = value("sec-fetch-site") {
        return site != "same-origin" && site != "none";
    }
    let Some(origin) = value("origin") else {
        return false;
    };
    let host = value("x-forwarded-host").or(value("host"));
    origin.split_once("://").map(|(_, authority)| authority) != host
}

async fn security_headers(mut res: Response) -> Response {
    let headers = res.headers_mut();
    // Pages show one-time secrets (new API tokens), so browsers and proxies must not keep
    // them. Static assets set their own caching and are left alone.
    if !headers.contains_key(header::CACHE_CONTROL) {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    headers.insert(
        header::HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; img-src 'self' data:; frame-ancestors 'none'; form-action 'self'",
        ),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    res
}
