//! OAuth 2.1 authorization server for MCP clients and the single-owner login that guards it.
//!
//! `router` serves discovery, registration, authorize, token, `/login` and `/logout`.
//! `require_mcp_auth` and `require_owner_session` are middleware for the routes they protect, used
//! with `axum::middleware::from_fn_with_state`. Serve with
//! `into_make_service_with_connect_info::<SocketAddr>()`: without a peer address, every client
//! shares one rate-limit bucket.

mod admin;
mod config;
mod crypto;
mod limit;
mod oauth;
mod session;
mod state;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};

pub use admin::{PassphraseError, hash_passphrase, issue_api_token, set_owner_passphrase};
pub use config::{AuthConfig, ConfigError, TrustedProxy, default_passphrase_file};
pub use oauth::{SENSITIVE_CONSENT_VERSION, effective_mcp_scopes, require_mcp_auth};
pub use session::require_owner_session;
pub use state::AuthState;

/// Larger than any real login, consent or token request, and enough for a long `state`.
const BODY_LIMIT: usize = 64 * 1024;
/// Registration is unauthenticated and every byte accepted is stored.
const REGISTER_BODY_LIMIT: usize = 16 * 1024;

pub fn router(state: AuthState) -> Router {
    Router::new()
        .route(
            "/.well-known/oauth-authorization-server",
            get(oauth::authorization_server_metadata),
        )
        .route(
            "/.well-known/oauth-protected-resource",
            get(oauth::protected_resource_metadata),
        )
        .route(
            "/.well-known/oauth-protected-resource/{*path}",
            get(oauth::protected_resource_metadata),
        )
        .route(
            "/api/mcp/register",
            post(oauth::register).layer(DefaultBodyLimit::max(REGISTER_BODY_LIMIT)),
        )
        .route(
            "/api/mcp/authorize",
            get(oauth::authorize).post(oauth::approve),
        )
        .route("/api/mcp/token", post(oauth::token))
        .route("/login", get(session::login_page).post(session::login))
        .route("/logout", get(session::logout_page).post(session::logout))
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .with_state(state)
}
