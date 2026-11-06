//! The public static surface: stylesheet, script, service worker, manifest, icons and the offline
//! page. None of it holds personal data, so it sits outside the login: a browser fetches the
//! manifest and the worker script without the session cookie, and a logged-out one must still
//! get the same bytes rather than a redirect.

use std::sync::LazyLock;

use axum::Router;
use axum::extract::{Path, Query};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;

use crate::views::{self, Nav};

const CSS: &str = include_str!("style.css");
const JS: &str = include_str!("../assets/app.js");
const CV_JS: &str = include_str!("../assets/cv.js");
const VOICE_JS: &str = include_str!("../assets/voice.js");
const SW: &str = include_str!("../assets/sw.js");
const MANIFEST: &str = r##"{
  "name": "Nexus",
  "short_name": "Nexus",
  "description": "Job pipeline",
  "id": "/",
  "start_url": "/",
  "scope": "/",
  "display": "standalone",
  "background_color": "#14161a",
  "theme_color": "#1f5f4a",
  "icons": [
    {"src": "/icons/icon-192.png", "sizes": "192x192", "type": "image/png"},
    {"src": "/icons/icon-512.png", "sizes": "512x512", "type": "image/png"},
    {"src": "/icons/icon-maskable-512.png", "sizes": "512x512", "type": "image/png", "purpose": "maskable"}
  ]
}"##;
const ICONS: [(&str, &[u8]); 4] = [
    ("icon-192.png", include_bytes!("../assets/icon-192.png")),
    ("icon-512.png", include_bytes!("../assets/icon-512.png")),
    (
        "icon-maskable-512.png",
        include_bytes!("../assets/icon-maskable-512.png"),
    ),
    (
        "apple-touch-icon.png",
        include_bytes!("../assets/apple-touch-icon.png"),
    ),
];

/// Names a build of the static files: pages link to `?v=` URLs and the worker caches by it, so a
/// changed file replaces the old cache.
pub(crate) static VERSION: LazyLock<String> = LazyLock::new(|| {
    let hash = [CSS, JS, CV_JS, VOICE_JS, SW, MANIFEST]
        .iter()
        .flat_map(|part| part.bytes())
        .chain(ICONS.iter().flat_map(|(_, bytes)| bytes.iter().copied()))
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        });
    format!("{hash:016x}")
});

/// Mounts the static files and `/offline`, with no login.
pub fn static_router() -> Router {
    Router::new()
        .route("/assets/app.css", get(stylesheet))
        .route("/assets/app.js", get(script))
        .route("/assets/cv.js", get(cv_script))
        .route("/assets/voice.js", get(voice_script))
        .route("/sw.js", get(service_worker))
        .route("/manifest.webmanifest", get(manifest))
        .route("/icons/{name}", get(icon))
        .route("/offline", get(offline))
        .layer(middleware::map_response(crate::secure))
}

#[derive(Deserialize)]
struct Versioned {
    v: Option<String>,
}

async fn stylesheet(query: Query<Versioned>) -> Response {
    file(query, "text/css; charset=utf-8", CSS)
}

async fn script(query: Query<Versioned>) -> Response {
    file(query, "text/javascript; charset=utf-8", JS)
}

async fn cv_script(query: Query<Versioned>) -> Response {
    file(query, "text/javascript; charset=utf-8", CV_JS)
}

async fn voice_script(query: Query<Versioned>) -> Response {
    file(query, "text/javascript; charset=utf-8", VOICE_JS)
}

/// A `?v=` URL naming this build never changes; any other URL must be revalidated.
fn file(Query(query): Query<Versioned>, kind: &'static str, body: &'static str) -> Response {
    let cache = if query.v.as_deref() == Some(VERSION.as_str()) {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    (
        [(header::CONTENT_TYPE, kind), (header::CACHE_CONTROL, cache)],
        body,
    )
        .into_response()
}

/// The worker's own `connect-src` governs the fetches it makes, which the page policy does not.
async fn service_worker() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; connect-src 'self'",
            ),
        ],
        SW.replace("__VERSION__", &VERSION),
    )
        .into_response()
}

async fn manifest() -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/manifest+json"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        MANIFEST,
    )
        .into_response()
}

async fn icon(Path(name): Path<String>) -> Response {
    match ICONS.iter().find(|(known, _)| *known == name) {
        Some((_, bytes)) => (
            [
                (header::CONTENT_TYPE, HeaderValue::from_static("image/png")),
                (
                    header::CACHE_CONTROL,
                    HeaderValue::from_static("public, max-age=86400"),
                ),
            ],
            *bytes,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn offline() -> Html<String> {
    Html(views::page(
        "Offline",
        Nav::None,
        0,
        views::message(
            "You are offline",
            "Nexus shows your applications only while connected. Reconnect and reload.",
        ),
    ))
}
