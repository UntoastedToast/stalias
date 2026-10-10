//! Frontend files compiled into the binary, so the container needs no extra files.

use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

/// Changes with the content of the asset files; templates append it to asset URLs.
pub const VERSION: &str = env!("ASSET_VERSION");

const BASECOAT_JS: &[u8] = include_bytes!("static/basecoat.min.js");
const APP_CSS: &[u8] = include_bytes!("static/app.css");
const APP_JS: &[u8] = include_bytes!("static/app.js");
const LOGO: &[u8] = include_bytes!("static/logo.svg");
const GEIST: &[u8] = include_bytes!("static/geist-1.7.2.woff2");
const HTMX_JS: &[u8] = include_bytes!("static/htmx.min.js");

/// Serves a compiled-in static file by name. Unknown names give 404.
pub async fn serve(Path(file): Path<String>) -> Response {
    let (content_type, body) = match file.as_str() {
        "basecoat.min.js" => ("text/javascript; charset=utf-8", BASECOAT_JS),
        "app.css" => ("text/css; charset=utf-8", APP_CSS),
        "app.js" => ("text/javascript; charset=utf-8", APP_JS),
        "logo.svg" => ("image/svg+xml", LOGO),
        "geist-1.7.2.woff2" => ("font/woff2", GEIST),
        "htmx.min.js" => ("text/javascript; charset=utf-8", HTMX_JS),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        body,
    )
        .into_response()
}
