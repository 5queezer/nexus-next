use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};

use argon2::password_hash::{Ident, PasswordHash};
use argon2::{ARGON2ID_IDENT, Params};
use axum::http::HeaderName;
use url::Url;

const PASSPHRASE_FILE: &str = "owner-passphrase.argon2";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("NEXUS_PUBLIC_BASE_URL is not set")]
    MissingBaseUrl,
    #[error("public base URL must be https (or http on localhost) with no path, query or userinfo")]
    InvalidBaseUrl,
    #[error(
        "no owner passphrase hash: set NEXUS_OWNER_PASSPHRASE_HASH or create {0} with the set-passphrase command"
    )]
    MissingPassphraseHash(PathBuf),
    #[error("owner passphrase hash must be an argon2id PHC string")]
    InvalidPassphraseHash,
    #[error("NEXUS_TRUSTED_IP_HEADER is not a valid header name")]
    InvalidTrustedHeader,
    #[error("NEXUS_TRUSTED_PROXY_PEERS must be a comma-separated list of IP addresses")]
    InvalidTrustedPeers,
    #[error("reading {0}: {1}")]
    Io(PathBuf, #[source] std::io::Error),
}

/// A proxy allowed to name the client address. Rate limiting reads `header` only on connections
/// from `peers`, so a client reaching the server directly cannot rotate its own limiter key.
#[derive(Debug, Clone)]
pub struct TrustedProxy {
    /// Must be stripped and set by the proxy, e.g. `cf-connecting-ip`. With a list such as
    /// `x-forwarded-for`, the last entry is used: it is the one the proxy itself appended.
    pub header: HeaderName,
    pub peers: Vec<IpAddr>,
}

#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// The issuer. Never taken from request headers, so a spoofed Host cannot change it.
    pub public_base_url: String,
    /// argon2id PHC string.
    pub owner_passphrase_hash: String,
    /// `None` (the default) means rate limits key on the socket address, ignoring forwarded headers.
    pub trusted_proxy: Option<TrustedProxy>,
}

pub fn default_passphrase_file(data_dir: &Path) -> PathBuf {
    data_dir.join(PASSPHRASE_FILE)
}

impl AuthConfig {
    /// Reads `NEXUS_PUBLIC_BASE_URL`; `NEXUS_OWNER_PASSPHRASE_HASH`, else the file named by
    /// `NEXUS_OWNER_PASSPHRASE_HASH_FILE`, else `<data_dir>/owner-passphrase.argon2`; and optionally
    /// `NEXUS_TRUSTED_IP_HEADER` with `NEXUS_TRUSTED_PROXY_PEERS` (default: loopback only).
    pub fn from_env(data_dir: &Path) -> Result<Self, ConfigError> {
        Self::from_vars(|name| std::env::var(name).ok(), data_dir)
    }

    fn from_vars(
        var: impl Fn(&str) -> Option<String>,
        data_dir: &Path,
    ) -> Result<Self, ConfigError> {
        let public_base_url = var("NEXUS_PUBLIC_BASE_URL").ok_or(ConfigError::MissingBaseUrl)?;
        let owner_passphrase_hash = match var("NEXUS_OWNER_PASSPHRASE_HASH") {
            Some(hash) => hash,
            None => {
                let path = var("NEXUS_OWNER_PASSPHRASE_HASH_FILE")
                    .map_or_else(|| default_passphrase_file(data_dir), PathBuf::from);
                match std::fs::read_to_string(&path) {
                    Ok(hash) => hash.trim().to_owned(),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        return Err(ConfigError::MissingPassphraseHash(path));
                    }
                    Err(e) => return Err(ConfigError::Io(path, e)),
                }
            }
        };
        let trusted_proxy = var("NEXUS_TRUSTED_IP_HEADER")
            .map(|header| {
                let peers = match var("NEXUS_TRUSTED_PROXY_PEERS") {
                    Some(list) => list
                        .split(',')
                        .map(|peer| peer.trim().parse())
                        .collect::<Result<_, _>>()
                        .map_err(|_| ConfigError::InvalidTrustedPeers)?,
                    None => vec![Ipv4Addr::LOCALHOST.into(), Ipv6Addr::LOCALHOST.into()],
                };
                let header = header
                    .trim()
                    .parse()
                    .map_err(|_| ConfigError::InvalidTrustedHeader)?;
                Ok(TrustedProxy { header, peers })
            })
            .transpose()?;
        Ok(Self {
            public_base_url,
            owner_passphrase_hash,
            trusted_proxy,
        })
    }
}

/// The canonical origin of a configured base URL. Cookies use `Path=/` and routes are absolute, so
/// a path prefix is refused rather than half-supported.
pub(crate) fn normalize_base_url(raw: &str) -> Result<String, ConfigError> {
    let url = Url::parse(raw).map_err(|_| ConfigError::InvalidBaseUrl)?;
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1"));
    if (url.scheme() != "https" && !(url.scheme() == "http" && local))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ConfigError::InvalidBaseUrl);
    }
    Ok(url.origin().ascii_serialization())
}

pub(crate) fn validate_passphrase_hash(hash: &str) -> Result<(), ConfigError> {
    let parsed = PasswordHash::new(hash).map_err(|_| ConfigError::InvalidPassphraseHash)?;
    let is_argon2id = parsed.algorithm == Ident::new_unwrap(ARGON2ID_IDENT.as_str());
    if !is_argon2id || parsed.hash.is_none() || Params::try_from(&parsed).is_err() {
        return Err(ConfigError::InvalidPassphraseHash);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    const HASH: &str = "$argon2id$v=19$m=8,t=1,p=1$c2FsdHNhbHQ$AAAAAAAAAAAAAAAAAAAAAA";

    fn config(vars: &[(&str, &str)], dir: &Path) -> Result<AuthConfig, ConfigError> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        AuthConfig::from_vars(|name| vars.get(name).cloned(), dir)
    }

    const BASE: (&str, &str) = ("NEXUS_PUBLIC_BASE_URL", "https://crm.example.test");

    #[test]
    fn missing_base_url_and_missing_hash_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            config(&[], dir.path()),
            Err(ConfigError::MissingBaseUrl)
        ));
        assert!(matches!(
            config(&[BASE], dir.path()),
            Err(ConfigError::MissingPassphraseHash(_))
        ));
    }

    #[test]
    fn hash_comes_from_env_then_named_file_then_default_file() {
        let dir = tempfile::tempdir().unwrap();
        let env = ("NEXUS_OWNER_PASSPHRASE_HASH", HASH);
        assert_eq!(
            config(&[BASE, env], dir.path())
                .unwrap()
                .owner_passphrase_hash,
            HASH
        );

        std::fs::write(default_passphrase_file(dir.path()), format!("{HASH}\n")).unwrap();
        assert_eq!(
            config(&[BASE], dir.path()).unwrap().owner_passphrase_hash,
            HASH
        );

        let named = dir.path().join("elsewhere");
        std::fs::write(&named, "$argon2id$other").unwrap();
        let file = ("NEXUS_OWNER_PASSPHRASE_HASH_FILE", named.to_str().unwrap());
        assert_eq!(
            config(&[BASE, file], dir.path())
                .unwrap()
                .owner_passphrase_hash,
            "$argon2id$other"
        );
    }

    #[test]
    fn forwarded_headers_are_ignored_unless_configured() {
        let dir = tempfile::tempdir().unwrap();
        let env = ("NEXUS_OWNER_PASSPHRASE_HASH", HASH);
        assert!(
            config(&[BASE, env], dir.path())
                .unwrap()
                .trusted_proxy
                .is_none()
        );

        let header = ("NEXUS_TRUSTED_IP_HEADER", "CF-Connecting-IP");
        let proxy = config(&[BASE, env, header], dir.path())
            .unwrap()
            .trusted_proxy
            .unwrap();
        assert_eq!(proxy.header, "cf-connecting-ip");
        assert_eq!(
            proxy.peers,
            [
                IpAddr::from(Ipv4Addr::LOCALHOST),
                IpAddr::from(Ipv6Addr::LOCALHOST)
            ]
        );

        let peers = ("NEXUS_TRUSTED_PROXY_PEERS", "10.0.0.1, 10.0.0.2");
        let proxy = config(&[BASE, env, header, peers], dir.path())
            .unwrap()
            .trusted_proxy
            .unwrap();
        assert_eq!(proxy.peers.len(), 2);

        let bad = ("NEXUS_TRUSTED_PROXY_PEERS", "not-an-ip");
        assert!(matches!(
            config(&[BASE, env, header, bad], dir.path()),
            Err(ConfigError::InvalidTrustedPeers)
        ));
    }

    #[test]
    fn base_url_must_be_a_bare_https_origin() {
        for good in [
            "https://crm.example.test",
            "https://crm.example.test/",
            "https://crm.example.test:8443",
            "http://localhost:3000",
            "http://127.0.0.1:3000/",
        ] {
            assert!(normalize_base_url(good).is_ok(), "{good}");
        }
        assert_eq!(
            normalize_base_url("https://crm.example.test/").unwrap(),
            "https://crm.example.test"
        );
        for bad in [
            "",
            "crm.example.test",
            "http://crm.example.test",
            "ftp://crm.example.test",
            "https://crm.example.test/prefix",
            "https://crm.example.test/?a=1",
            "https://crm.example.test/#a",
            "https://user:secret@crm.example.test",
        ] {
            assert!(normalize_base_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn only_argon2id_phc_hashes_are_accepted() {
        assert!(validate_passphrase_hash(HASH).is_ok());
        for bad in [
            "",
            "hunter2hunter2",
            "$argon2i$v=19$m=8,t=1,p=1$c2FsdHNhbHQ$AAAAAAAAAAAAAAAAAAAAAA",
            "$argon2d$v=19$m=8,t=1,p=1$c2FsdHNhbHQ$AAAAAAAAAAAAAAAAAAAAAA",
            "$argon2id$v=19$m=8,t=1,p=1$c2FsdHNhbHQ",
            "$2b$12$abcdefghijklmnopqrstuv",
        ] {
            assert!(validate_passphrase_hash(bad).is_err(), "{bad}");
        }
    }
}
