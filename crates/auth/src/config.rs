use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use argon2::password_hash::{Ident, PasswordHash};
use argon2::{ARGON2ID_IDENT, Params};
use axum::http::HeaderName;
use url::Url;

const PASSPHRASE_FILE: &str = "owner-passphrase.argon2";
/// Where a client may be sent with an authorization code, besides loopback. Claude.ai's real
/// callback host is unverified until the M1 gate: see `event=redirect_host_rejected` in the log.
const DEFAULT_REDIRECT_HOSTS: [&str; 2] = ["claude.ai", "claude.com"];

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("public base URL must be https (or http on localhost) with no path, query or userinfo")]
    InvalidBaseUrl,
    #[error(
        "no owner passphrase is configured: run `nexus set-passphrase` to create {0}, or set NEXUS_OWNER_PASSPHRASE_HASH"
    )]
    MissingPassphraseHash(PathBuf),
    #[error("owner passphrase hash must be an argon2id PHC string")]
    InvalidPassphraseHash,
    #[error("{0} has mode {1:o}, which lets other users read the hash: use 0400 or 0600")]
    LoosePassphraseFile(PathBuf, u32),
    #[error("NEXUS_TRUSTED_IP_HEADER is not a valid header name")]
    InvalidTrustedHeader,
    #[error("NEXUS_TRUSTED_PROXY_PEERS must be a comma-separated list of IP addresses")]
    InvalidTrustedPeers,
    #[error(
        "NEXUS_TRUSTED_IP_HEADER is required with an https base URL: name the header your ingress \
         sets to the client address, or set it to `none` if clients reach this server directly"
    )]
    MissingTrustedHeader,
    #[error(
        "NEXUS_OAUTH_REDIRECT_HOSTS must be comma-separated host names, such as claude.ai: letters, \
         digits, hyphens and dots only, no scheme, port or wildcard"
    )]
    InvalidRedirectHosts,
    #[error("reading {0}: {1}")]
    Io(PathBuf, #[source] std::io::Error),
}

/// A proxy allowed to name the client address. Rate limiting reads `header` only on connections
/// from `peers`, so a client reaching the server directly cannot rotate its own limiter key.
#[derive(Debug, Clone)]
pub struct TrustedProxy {
    /// Must be stripped and set by the proxy, e.g. `cf-connecting-ip`. With a list such as
    /// `x-forwarded-for`, the last entry of the last header line is used: it is the one the proxy
    /// itself appended.
    pub header: HeaderName,
    pub peers: Vec<IpAddr>,
}

#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// The issuer. Never taken from request headers, so a spoofed Host cannot change it.
    pub public_base_url: String,
    /// argon2id PHC string.
    pub owner_passphrase_hash: String,
    /// The file the hash was read from, which a change in the browser rewrites. `None` when the
    /// hash came from `NEXUS_OWNER_PASSPHRASE_HASH`, which the server can't change.
    pub owner_passphrase_file: Option<PathBuf>,
    /// `None` (the default) means rate limits key on the socket address, ignoring forwarded headers.
    pub trusted_proxy: Option<TrustedProxy>,
    /// Hosts an https redirect URI may name, matched exactly. Loopback is always allowed.
    pub redirect_hosts: Vec<String>,
}

/// Where the owner passphrase's hash lives: `NEXUS_OWNER_PASSPHRASE_HASH_FILE`, else the data dir.
pub fn owner_passphrase_file(data_dir: &Path) -> PathBuf {
    passphrase_file(&|name: &str| std::env::var(name).ok(), data_dir)
}

fn passphrase_file(var: &impl Fn(&str) -> Option<String>, data_dir: &Path) -> PathBuf {
    var("NEXUS_OWNER_PASSPHRASE_HASH_FILE")
        .map_or_else(|| data_dir.join(PASSPHRASE_FILE), PathBuf::from)
}

fn read_hash_file(path: &Path) -> Result<String, ConfigError> {
    let not_read = |e: std::io::Error| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ConfigError::MissingPassphraseHash(path.to_owned())
        } else {
            ConfigError::Io(path.to_owned(), e)
        }
    };
    let mode = std::fs::metadata(path)
        .map_err(not_read)?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(ConfigError::LoosePassphraseFile(
            path.to_owned(),
            mode & 0o777,
        ));
    }
    Ok(std::fs::read_to_string(path)
        .map_err(not_read)?
        .trim()
        .to_owned())
}

impl AuthConfig {
    /// Takes the public origin from the caller (the server's `--base-url`), and reads the rest:
    /// `NEXUS_OWNER_PASSPHRASE_HASH`, else the file named by `NEXUS_OWNER_PASSPHRASE_HASH_FILE`, else
    /// `<data_dir>/owner-passphrase.argon2`; and optionally `NEXUS_TRUSTED_IP_HEADER` with
    /// `NEXUS_TRUSTED_PROXY_PEERS` (default: loopback only), required with an https base URL unless
    /// it says `none`; and `NEXUS_OAUTH_REDIRECT_HOSTS`. Everything is checked here.
    pub fn from_env(public_base_url: &str, data_dir: &Path) -> Result<Self, ConfigError> {
        Self::from_vars(|name| std::env::var(name).ok(), public_base_url, data_dir)
    }

    fn from_vars(
        var: impl Fn(&str) -> Option<String>,
        public_base_url: &str,
        data_dir: &Path,
    ) -> Result<Self, ConfigError> {
        let public_base_url = normalize_base_url(public_base_url)?;
        let (owner_passphrase_hash, owner_passphrase_file) =
            match var("NEXUS_OWNER_PASSPHRASE_HASH") {
                Some(hash) => (hash, None),
                None => {
                    let path = passphrase_file(&var, data_dir);
                    (read_hash_file(&path)?, Some(path))
                }
            };
        validate_passphrase_hash(&owner_passphrase_hash)?;
        let trusted_header = var("NEXUS_TRUSTED_IP_HEADER");
        if trusted_header.is_none() && public_base_url.starts_with("https://") {
            return Err(ConfigError::MissingTrustedHeader);
        }
        let trusted_proxy = trusted_header
            .filter(|header| !header.trim().eq_ignore_ascii_case("none"))
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
        let redirect_hosts = match var("NEXUS_OAUTH_REDIRECT_HOSTS") {
            Some(list) => parse_redirect_hosts(&list)?,
            None => DEFAULT_REDIRECT_HOSTS.map(str::to_owned).to_vec(),
        };
        Ok(Self {
            public_base_url,
            owner_passphrase_hash,
            owner_passphrase_file,
            trusted_proxy,
            redirect_hosts,
        })
    }
}

fn parse_redirect_hosts(list: &str) -> Result<Vec<String>, ConfigError> {
    list.split(',')
        .map(|host| host.trim().to_ascii_lowercase())
        .filter(|host| !host.is_empty())
        .map(|host| {
            if host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
            {
                Ok(host)
            } else {
                Err(ConfigError::InvalidRedirectHosts)
            }
        })
        .collect()
}

/// The canonical origin of a configured base URL. Cookies use `Path=/` and routes are absolute, so
/// a path prefix is refused rather than half-supported.
pub fn normalize_base_url(raw: &str) -> Result<String, ConfigError> {
    let url = Url::parse(raw).map_err(|_| ConfigError::InvalidBaseUrl)?;
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
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

    const BASE: &str = "https://crm.example.test";
    const ENV_HASH: (&str, &str) = ("NEXUS_OWNER_PASSPHRASE_HASH", HASH);
    const NO_PROXY: (&str, &str) = ("NEXUS_TRUSTED_IP_HEADER", "none");

    fn config(vars: &[(&str, &str)], dir: &Path) -> Result<AuthConfig, ConfigError> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        AuthConfig::from_vars(
            |name| {
                vars.get(name)
                    .cloned()
                    .or_else(|| (name == NO_PROXY.0).then(|| NO_PROXY.1.to_owned()))
            },
            BASE,
            dir,
        )
    }

    fn write_private(path: &Path, contents: &str) {
        std::fs::write(path, contents).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn a_missing_hash_is_an_error_naming_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let error = config(&[], dir.path()).unwrap_err();
        assert!(matches!(error, ConfigError::MissingPassphraseHash(_)));
        assert!(error.to_string().contains("nexus set-passphrase"));
    }

    #[test]
    fn hash_comes_from_env_then_named_file_then_default_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            config(&[ENV_HASH], dir.path())
                .unwrap()
                .owner_passphrase_hash,
            HASH
        );

        write_private(&dir.path().join(PASSPHRASE_FILE), &format!("{HASH}\n"));
        assert_eq!(config(&[], dir.path()).unwrap().owner_passphrase_hash, HASH);

        let named = dir.path().join("elsewhere");
        let other = "$argon2id$v=19$m=8,t=1,p=1$b3RoZXJzYWx0$AAAAAAAAAAAAAAAAAAAAAA";
        write_private(&named, other);
        let file = ("NEXUS_OWNER_PASSPHRASE_HASH_FILE", named.to_str().unwrap());
        assert_eq!(
            config(&[file], dir.path()).unwrap().owner_passphrase_hash,
            other
        );
    }

    #[test]
    fn a_hash_file_others_can_read_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PASSPHRASE_FILE);
        write_private(&path, HASH);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(matches!(
            config(&[], dir.path()),
            Err(ConfigError::LoosePassphraseFile(_, 0o640))
        ));
    }

    #[test]
    fn forwarded_headers_are_ignored_unless_configured() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            config(&[ENV_HASH], dir.path())
                .unwrap()
                .trusted_proxy
                .is_none()
        );

        let header = ("NEXUS_TRUSTED_IP_HEADER", "CF-Connecting-IP");
        let proxy = config(&[ENV_HASH, header], dir.path())
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
        let proxy = config(&[ENV_HASH, header, peers], dir.path())
            .unwrap()
            .trusted_proxy
            .unwrap();
        assert_eq!(proxy.peers.len(), 2);

        let bad = ("NEXUS_TRUSTED_PROXY_PEERS", "not-an-ip");
        assert!(matches!(
            config(&[ENV_HASH, header, bad], dir.path()),
            Err(ConfigError::InvalidTrustedPeers)
        ));
    }

    #[test]
    fn an_https_base_url_needs_the_trusted_header_or_an_explicit_none() {
        let dir = tempfile::tempdir().unwrap();
        let var = |base: &'static str, header: Option<&'static str>| {
            AuthConfig::from_vars(
                move |name| match name {
                    "NEXUS_OWNER_PASSPHRASE_HASH" => Some(HASH.to_owned()),
                    "NEXUS_TRUSTED_IP_HEADER" => header.map(str::to_owned),
                    _ => None,
                },
                base,
                dir.path(),
            )
        };
        assert!(matches!(
            var(BASE, None),
            Err(ConfigError::MissingTrustedHeader)
        ));
        for none in ["none", "NONE", " none "] {
            assert!(var(BASE, Some(none)).unwrap().trusted_proxy.is_none());
        }
        assert!(
            var(BASE, Some("CF-Connecting-IP"))
                .unwrap()
                .trusted_proxy
                .is_some()
        );
        assert!(var("http://127.0.0.1:3000", None).is_ok());
    }

    #[test]
    fn redirect_hosts_default_to_claude_and_the_setting_replaces_them() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            config(&[ENV_HASH], dir.path()).unwrap().redirect_hosts,
            ["claude.ai", "claude.com"]
        );
        let set = (
            "NEXUS_OAUTH_REDIRECT_HOSTS",
            " Hermes.Example.net, ,a-b.example.net ",
        );
        assert_eq!(
            config(&[ENV_HASH, set], dir.path()).unwrap().redirect_hosts,
            ["hermes.example.net", "a-b.example.net"]
        );
        let empty = ("NEXUS_OAUTH_REDIRECT_HOSTS", "");
        assert!(
            config(&[ENV_HASH, empty], dir.path())
                .unwrap()
                .redirect_hosts
                .is_empty()
        );
        for bad in ["https://claude.ai", "*.claude.ai", "claude.ai:443", "a b"] {
            assert!(matches!(
                config(&[ENV_HASH, ("NEXUS_OAUTH_REDIRECT_HOSTS", bad)], dir.path()),
                Err(ConfigError::InvalidRedirectHosts)
            ));
        }
    }

    #[test]
    fn base_url_must_be_a_bare_https_origin() {
        for good in [
            "https://crm.example.test",
            "https://crm.example.test/",
            "https://crm.example.test:8443",
            "http://localhost:3000",
            "http://127.0.0.1:3000/",
            "http://[::1]:3000",
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
