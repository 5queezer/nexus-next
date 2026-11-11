//! Nexus's human surfaces: the dashboard, the JSON API, the calendar feed and the public static
//! files. Each router carries no authentication; the server mounts them behind its auth layers.

mod api;
mod assets;
mod calendar;
mod cv;
mod cv_events;
mod dashboard;
mod format;
mod forms;
mod openai_key;
mod queue;
mod render_cache;
mod views;
mod voice;

use axum::http::{HeaderName, HeaderValue, header};
use axum::response::Response;

pub use api::api_router;
pub use assets::static_router;
pub use calendar::calendar_router;
pub use chrono_tz::Tz;
pub use cv::cv_router;
pub use cv_events::{
    CutConnection, HEARTBEAT, MAX_STREAMS, STREAM_LIFE, StreamLife, cv_events_router,
};
pub use dashboard::dashboard_router;
pub use openai_key::openai_key_router;
pub use voice::{
    CONFIRMED_VOICE_TOOLS, IDLE_SECONDS, MAX_MODEL_OUTPUT_BYTES, MINTS_PER_HOUR, OPENAI_BASE_URL,
    OPENAI_KEY_FILE, VOICE_TOOLS, Voice, VoiceConfig, VoiceKey, tool_definitions,
    voice_status_router,
};

/// Default response headers: no inline script or foreign asset can load, nothing sniffs types, and
/// nothing personal is cached by shared caches. A handler's own value wins, which is how a route
/// gets a policy of its own (`sw.js` does).
async fn secure(mut response: Response) -> Response {
    let headers = response.headers_mut();
    for (name, value) in [
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; manifest-src 'self'; worker-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
        ),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::CACHE_CONTROL, "no-store"),
        (HeaderName::from_static("x-frame-options"), "DENY"),
        (
            HeaderName::from_static("permissions-policy"),
            "microphone=()",
        ),
    ] {
        headers
            .entry(name)
            .or_insert(HeaderValue::from_static(value));
    }
    response
}
