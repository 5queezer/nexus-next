use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::HeaderMap;
use axum::http::request::Parts;
use nexus_core::db::Db;
use nexus_core::repo::oauth as repo;
use nexus_core::time::Timestamp;
use tokio::sync::Semaphore;

use crate::config::{
    AuthConfig, ConfigError, TrustedProxy, normalize_base_url, validate_passphrase_hash,
};
use crate::crypto::random_bytes;
use crate::limit::{Backoff, BackoffPolicy, FixedWindow, address_key};

const PRUNE_EVERY: Duration = Duration::from_secs(600);

/// Shared state of the auth routes and middleware. The only way to build it is `new`, which fails
/// without a valid passphrase hash, so auth routes cannot start without one.
#[derive(Clone)]
pub struct AuthState(pub(crate) Arc<Inner>);

pub(crate) struct Inner {
    pub(crate) db: Db,
    pub(crate) base_url: String,
    pub(crate) passphrase_hash: String,
    trusted_proxy: Option<TrustedProxy>,
    /// Signs consent and CSRF tokens. These live for minutes, so a key per process is enough, and
    /// no secret has to be stored or configured.
    pub(crate) key: [u8; 32],
    /// SHA-256 of each session id, with its expiry. In memory because a restart costing the owner
    /// one login is cheaper than another stored credential; logout really does revoke.
    pub(crate) sessions: Mutex<HashMap<String, Instant>>,
    pub(crate) login_limit: Backoff,
    pub(crate) register_ip_limit: FixedWindow,
    pub(crate) register_global_limit: FixedWindow,
    pub(crate) token_limit: FixedWindow,
    /// Each argon2 verification takes about 19 MiB; on the board they run one at a time.
    pub(crate) hash_slot: Semaphore,
    last_prune: Mutex<Option<Instant>>,
}

impl AuthState {
    pub fn new(db: Db, config: AuthConfig) -> Result<Self, ConfigError> {
        validate_passphrase_hash(&config.owner_passphrase_hash)?;
        let hour = Duration::from_secs(3600);
        Ok(Self(Arc::new(Inner {
            db,
            base_url: normalize_base_url(&config.public_base_url)?,
            passphrase_hash: config.owner_passphrase_hash,
            trusted_proxy: config.trusted_proxy,
            key: random_bytes(),
            sessions: Mutex::default(),
            login_limit: Backoff::new(
                BackoffPolicy {
                    free: 5,
                    base: Duration::from_secs(2),
                    max: hour,
                    forget_after: hour * 2,
                },
                BackoffPolicy {
                    free: 20,
                    base: Duration::from_secs(1),
                    max: Duration::from_secs(300),
                    forget_after: hour,
                },
            ),
            register_ip_limit: FixedWindow::new(10, hour),
            register_global_limit: FixedWindow::new(30, hour),
            // The `oauth` group of old Nexus's lib/rate-limit.ts.
            token_limit: FixedWindow::new(20, Duration::from_secs(60)),
            hash_slot: Semaphore::new(1),
            last_prune: Mutex::default(),
        })))
    }

    fn client_key(&self, headers: &HeaderMap, peer: Option<IpAddr>) -> String {
        let Some(peer) = peer.map(|ip| ip.to_canonical()) else {
            return "unknown".to_owned();
        };
        let forwarded = self
            .0
            .trusted_proxy
            .as_ref()
            .filter(|proxy| proxy.peers.iter().any(|p| p.to_canonical() == peer))
            .and_then(|proxy| headers.get(&proxy.header)?.to_str().ok())
            .and_then(|value| value.rsplit(',').next()?.trim().parse().ok());
        address_key(forwarded.unwrap_or(peer))
    }

    /// Deletes expired codes and tokens at most once per `PRUNE_EVERY`. Called from the routes that
    /// add rows, so an idle server does no work and a busy one cannot accumulate them.
    pub(crate) async fn prune_if_due(&self) -> nexus_core::error::Result<()> {
        {
            let now = Instant::now();
            let mut last = self.0.last_prune.lock().unwrap();
            if last.is_some_and(|at| now.duration_since(at) < PRUNE_EVERY) {
                return Ok(());
            }
            *last = Some(now);
        }
        let mut conn = self.0.db.conn().await?;
        repo::prune_expired(&mut conn, &Timestamp::now()).await
    }
}

/// The rate-limit key of the caller; see `TrustedProxy`.
pub(crate) struct ClientKey(pub(crate) String);

impl FromRequestParts<AuthState> for ClientKey {
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AuthState,
    ) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|info| info.0.ip());
        Ok(Self(state.client_key(&parts.headers, peer)))
    }
}
