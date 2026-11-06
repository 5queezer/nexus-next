//! Nexus's human surfaces: the read-only dashboard, the JSON API and the calendar feed. Each
//! router carries no authentication; the server mounts them behind its auth layers.

mod api;
mod calendar;
mod dashboard;
mod format;
mod queue;
mod views;

use axum::http::{HeaderName, HeaderValue, header};
use axum::response::Response;

pub use api::api_router;
pub use calendar::calendar_router;
pub use dashboard::dashboard_router;

/// Default response headers: no inline script or foreign asset can load, nothing sniffs types, and
/// nothing personal is cached by shared caches. A handler's own value wins.
async fn secure(mut response: Response) -> Response {
    let headers = response.headers_mut();
    for (name, value) in [
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; style-src 'self'; img-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
        ),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::CACHE_CONTROL, "no-store"),
        (HeaderName::from_static("x-frame-options"), "DENY"),
    ] {
        headers
            .entry(name)
            .or_insert(HeaderValue::from_static(value));
    }
    response
}
