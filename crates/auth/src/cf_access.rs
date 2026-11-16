//! Cloudflare Access in front of the owner's browser paths. Access signs every request it lets
//! through with a JWT in `Cf-Access-Jwt-Assertion`; a valid one for an allowed email is a login, so
//! the owner never types the passphrase on the public host.
//!
//! Only a verified signature counts: the tailnet and loopback reach the server without Access, and
//! anyone there can send the header. Verification is RS256 only, against the team's keys in a file
//! a timer refreshes (the service itself has no network), then `aud`, `iss`, `exp`, `nbf` and the
//! email. Anything missing or wrong falls back to the passphrase.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents};
use serde::Deserialize;
use serde_json::Value;

pub const HEADER: &str = "cf-access-jwt-assertion";
/// Clock difference allowed between Cloudflare and this board, in seconds.
const LEEWAY_SECS: i64 = 60;
/// Longer than any real assertion; a bigger header is not looked at.
const MAX_TOKEN_BYTES: usize = 16 * 1024;

/// `NEXUS_CF_ACCESS_*`: all four or none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CfAccess {
    /// The Access application's audience tag.
    pub aud: String,
    /// The team name: the issuer is `https://<team>.cloudflareaccess.com`.
    pub team: String,
    /// Owner emails allowed in, compared without case.
    pub emails: Vec<String>,
    /// The team's keys as Cloudflare serves them at `/cdn-cgi/access/certs`.
    pub certs_file: PathBuf,
}

/// Why an assertion was not taken, for the log. Never the token itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Malformed,
    Algorithm,
    NoKeys,
    UnknownKey,
    Signature,
    Audience,
    Issuer,
    Expired,
    NotYetValid,
    Email,
}

impl Refusal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::Algorithm => "algorithm",
            Self::NoKeys => "no_keys",
            Self::UnknownKey => "unknown_key",
            Self::Signature => "signature",
            Self::Audience => "audience",
            Self::Issuer => "issuer",
            Self::Expired => "expired",
            Self::NotYetValid => "not_yet_valid",
            Self::Email => "email",
        }
    }
}

struct Key {
    kid: String,
    n: Vec<u8>,
    e: Vec<u8>,
}

#[derive(Deserialize)]
struct Jwk {
    kid: Option<String>,
    kty: String,
    n: Option<String>,
    e: Option<String>,
}

#[derive(Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

/// The RSA keys in a JWKS document. Keys of other types, or without an id, are skipped.
pub fn parse_keys(json: &str) -> Result<usize, String> {
    keys_of(json).map(|keys| keys.len())
}

fn keys_of(json: &str) -> Result<Vec<Key>, String> {
    let jwks: Jwks = serde_json::from_str(json).map_err(|e| format!("not a key set: {e}"))?;
    let keys: Vec<Key> = jwks
        .keys
        .into_iter()
        .filter(|key| key.kty == "RSA")
        .filter_map(|key| {
            Some(Key {
                kid: key.kid?,
                n: URL_SAFE_NO_PAD.decode(key.n?).ok()?,
                e: URL_SAFE_NO_PAD.decode(key.e?).ok()?,
            })
        })
        .collect();
    if keys.is_empty() {
        return Err("no RSA key with an id".to_owned());
    }
    Ok(keys)
}

pub(crate) struct Verifier {
    config: CfAccess,
    issuer: String,
    /// The keys and the modification time of the file they came from.
    keys: Mutex<Option<(SystemTime, Vec<Key>)>>,
}

#[derive(Deserialize)]
struct Header {
    alg: String,
    kid: Option<String>,
}

impl Verifier {
    pub(crate) fn new(config: CfAccess) -> Self {
        let issuer = format!("https://{}.cloudflareaccess.com", config.team);
        Self {
            config,
            issuer,
            keys: Mutex::default(),
        }
    }

    /// Re-reads the file when it changed. An unreadable or unusable file leaves no keys, so every
    /// assertion is refused: fail closed, to the passphrase.
    fn with_key<T>(&self, kid: &str, use_key: impl FnOnce(&Key) -> T) -> Result<T, Refusal> {
        let mut cached = self.keys.lock().unwrap();
        let modified = std::fs::metadata(&self.config.certs_file)
            .and_then(|meta| meta.modified())
            .ok();
        match modified {
            None => *cached = None,
            Some(at) if cached.as_ref().is_none_or(|(seen, _)| *seen != at) => {
                *cached = std::fs::read_to_string(&self.config.certs_file)
                    .ok()
                    .and_then(|json| keys_of(&json).ok())
                    .map(|keys| (at, keys));
                if cached.is_none() {
                    tracing::error!(event = "cf_access_keys_unusable", file = %self.config.certs_file.display(), "the Access key file can't be used: the passphrase login stays the only one");
                }
            }
            Some(_) => {}
        }
        let (_, keys) = cached.as_ref().ok_or(Refusal::NoKeys)?;
        keys.iter()
            .find(|key| key.kid == kid)
            .map(use_key)
            .ok_or(Refusal::UnknownKey)
    }

    /// The allowed owner email the assertion names, if it is valid at `now` (Unix seconds).
    pub(crate) fn owner_email(&self, token: &str, now: i64) -> Result<String, Refusal> {
        if token.len() > MAX_TOKEN_BYTES {
            return Err(Refusal::Malformed);
        }
        let mut parts = token.split('.');
        let (Some(header), Some(payload), Some(signature), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(Refusal::Malformed);
        };
        let decode = |part: &str| URL_SAFE_NO_PAD.decode(part).map_err(|_| Refusal::Malformed);
        let parsed: Header =
            serde_json::from_slice(&decode(header)?).map_err(|_| Refusal::Malformed)?;
        // Fixed, so a token can't choose `none` or an HMAC keyed by a public key.
        if parsed.alg != "RS256" {
            return Err(Refusal::Algorithm);
        }
        let kid = parsed.kid.ok_or(Refusal::UnknownKey)?;
        let signature = decode(signature)?;
        let signed = &token[..header.len() + 1 + payload.len()];
        self.with_key(&kid, |key| {
            RsaPublicKeyComponents {
                n: &key.n,
                e: &key.e,
            }
            .verify(&RSA_PKCS1_2048_8192_SHA256, signed.as_bytes(), &signature)
        })?
        .map_err(|_| Refusal::Signature)?;

        let claims: Value =
            serde_json::from_slice(&decode(payload)?).map_err(|_| Refusal::Malformed)?;
        // Cloudflare sends `aud` as a list; the standard also allows one string.
        let audience_ok = match &claims["aud"] {
            Value::String(aud) => *aud == self.config.aud,
            Value::Array(auds) => auds.iter().any(|aud| *aud == *self.config.aud),
            _ => false,
        };
        if !audience_ok {
            return Err(Refusal::Audience);
        }
        if claims["iss"].as_str() != Some(self.issuer.as_str()) {
            return Err(Refusal::Issuer);
        }
        match claims["exp"].as_i64() {
            Some(exp) if now < exp + LEEWAY_SECS => {}
            _ => return Err(Refusal::Expired),
        }
        if claims["nbf"]
            .as_i64()
            .is_some_and(|nbf| now + LEEWAY_SECS < nbf)
        {
            return Err(Refusal::NotYetValid);
        }
        let email = claims["email"].as_str().unwrap_or_default();
        self.config
            .emails
            .iter()
            .find(|allowed| allowed.eq_ignore_ascii_case(email))
            .cloned()
            .ok_or(Refusal::Email)
    }
}

/// The settings, if `NEXUS_CF_ACCESS_AUD`, `_TEAM`, `_EMAILS` and `_CERTS_FILE` are all set. Only
/// some of them is an error, so a typo can't silently turn the feature off.
pub(crate) fn from_vars(
    var: &impl Fn(&str) -> Option<String>,
) -> Result<Option<CfAccess>, &'static str> {
    let names = [
        "NEXUS_CF_ACCESS_AUD",
        "NEXUS_CF_ACCESS_TEAM",
        "NEXUS_CF_ACCESS_EMAILS",
        "NEXUS_CF_ACCESS_CERTS_FILE",
    ];
    let values: Vec<Option<String>> = names
        .iter()
        .map(|name| {
            var(name)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        })
        .collect();
    if values.iter().all(Option::is_none) {
        return Ok(None);
    }
    let [Some(aud), Some(team), Some(emails), Some(certs_file)] =
        <[Option<String>; 4]>::try_from(values).expect("four names")
    else {
        return Err("set all of NEXUS_CF_ACCESS_AUD, _TEAM, _EMAILS and _CERTS_FILE, or none");
    };
    let team = team
        .strip_suffix(".cloudflareaccess.com")
        .unwrap_or(&team)
        .to_ascii_lowercase();
    if team.is_empty() || !team.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err(
            "NEXUS_CF_ACCESS_TEAM must be the team name, as in <team>.cloudflareaccess.com",
        );
    }
    let emails: Vec<String> = emails
        .split(',')
        .map(|email| email.trim().to_owned())
        .filter(|email| email.contains('@'))
        .collect();
    if emails.is_empty() {
        return Err("NEXUS_CF_ACCESS_EMAILS must list at least one email address");
    }
    Ok(Some(CfAccess {
        aud,
        team,
        emails,
        certs_file: Path::new(&certs_file).to_owned(),
    }))
}
