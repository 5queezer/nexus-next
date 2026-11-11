//! Operator actions, for the server binary to expose as subcommands.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use argon2::password_hash::{PasswordHasher, SaltString};
use argon2::{Argon2, PasswordHash, PasswordVerifier};
use nexus_core::db::Db;
use nexus_core::repo::{api_tokens, oauth};
use url::Url;

use crate::crypto::{generate_token, random_bytes, sha256_hex};
use crate::oauth::{grant_label, printable_name};

pub(crate) const MIN_PASSPHRASE_CHARS: usize = 16;
/// Repeating a unit this short adds nothing a guesser would not try first.
const MAX_TRIVIAL_CYCLE: usize = 8;
/// `grants list` prints 12 characters. A shorter ID is more likely a slip than a choice.
const MIN_GRANT_PREFIX: usize = 8;
pub(crate) const MAX_PASSPHRASE_BYTES: usize = 1024;

#[derive(Debug, thiserror::Error)]
pub enum PassphraseError {
    #[error("passphrase must be at least {MIN_PASSPHRASE_CHARS} characters")]
    TooShort,
    #[error("passphrase must not be one short pattern repeated")]
    Repetitive,
    #[error("passphrase must be at most {MAX_PASSPHRASE_BYTES} bytes")]
    TooLong,
    #[error("hashing failed: {0}")]
    Hash(argon2::password_hash::Error),
    #[error("writing {0}: {1}")]
    Io(PathBuf, #[source] std::io::Error),
    #[error("hashing task failed")]
    Task,
    #[error("revoking credentials failed: {0}")]
    Storage(#[from] nexus_core::error::Error),
}

/// Issues a `jt_` API token, storing only its hash, and returns the raw value, which cannot be
/// recovered later. As in old Nexus there is one token at a time, so this revokes any earlier one.
pub async fn issue_api_token(db: &Db, name: &str) -> nexus_core::error::Result<String> {
    let raw = generate_token("jt");
    let mut tx = db.begin_write().await?;
    api_tokens::replace(&mut tx, &sha256_hex(&raw), name).await?;
    tx.commit().await?;
    tracing::info!(
        event = "api_token_issued",
        "issued an API token, replacing any earlier one"
    );
    Ok(raw)
}

/// An argon2id PHC string with the crate's default parameters (19 MiB, 2 passes).
pub async fn hash_passphrase(passphrase: &str) -> Result<String, PassphraseError> {
    if passphrase.chars().count() < MIN_PASSPHRASE_CHARS {
        return Err(PassphraseError::TooShort);
    }
    if passphrase.len() > MAX_PASSPHRASE_BYTES {
        return Err(PassphraseError::TooLong);
    }
    if is_short_cycle(passphrase) {
        return Err(PassphraseError::Repetitive);
    }
    let passphrase = passphrase.to_owned();
    tokio::task::spawn_blocking(move || {
        let salt = SaltString::encode_b64(&random_bytes()[..16]).map_err(PassphraseError::Hash)?;
        Argon2::default()
            .hash_password(passphrase.as_bytes(), &salt)
            .map(|hash| hash.to_string())
            .map_err(PassphraseError::Hash)
    })
    .await
    .map_err(|_| PassphraseError::Task)?
}

fn is_short_cycle(passphrase: &str) -> bool {
    let chars: Vec<char> = passphrase.chars().collect();
    (1..=MAX_TRIVIAL_CYCLE).any(|cycle| chars[cycle..].iter().zip(&chars).all(|(a, b)| a == b))
}

/// One authorization that still holds live tokens, ready to print.
pub struct GrantLine {
    /// The same short form as `grant=` in the log, and what `revoke_grant` takes.
    pub id: String,
    pub client_id: String,
    pub client_name: String,
    pub redirect_origin: String,
    pub scopes: String,
    pub issued_at: String,
    pub expires_at: String,
}

pub async fn list_grants(db: &Db) -> nexus_core::error::Result<Vec<GrantLine>> {
    let grants = oauth::list_grants(&mut *db.conn().await?).await?;
    Ok(grants
        .into_iter()
        .map(|grant| GrantLine {
            id: grant_label(&grant.grant_id).to_owned(),
            client_id: grant.client_id,
            client_name: printable_name(grant.client_name.as_deref().unwrap_or_default()),
            redirect_origin: grant
                .redirect_uri
                .and_then(|uri| Url::parse(&uri).ok())
                .map_or_else(
                    || "unknown".to_owned(),
                    |u| u.origin().ascii_serialization(),
                ),
            scopes: grant.scopes.0.join(" "),
            issued_at: grant.issued_at.to_string(),
            expires_at: grant.expires_at.to_string(),
        })
        .collect())
}

#[derive(Debug, thiserror::Error)]
pub enum RevokeError {
    #[error("no grant matches that ID: see `nexus grants list`")]
    NotFound,
    #[error("that ID matches more than one grant: give more of it")]
    Ambiguous,
    #[error(transparent)]
    Storage(#[from] nexus_core::error::Error),
}

/// Revokes one grant's tokens, by a prefix of its ID. Takes effect at once on a running server,
/// which reads tokens from the database. The client stays registered and can authorize again.
pub async fn revoke_grant(db: &Db, id: &str) -> Result<oauth::Revoked, RevokeError> {
    if id.len() < MIN_GRANT_PREFIX {
        return Err(RevokeError::NotFound);
    }
    let mut tx = db.begin_write().await?;
    let matches = oauth::grants_with_prefix(&mut tx, id).await?;
    let [grant_id] = matches.as_slice() else {
        return Err(if matches.is_empty() {
            RevokeError::NotFound
        } else {
            RevokeError::Ambiguous
        });
    };
    let revoked = oauth::revoke_grant(&mut tx, grant_id).await?;
    tx.commit().await.map_err(nexus_core::error::Error::from)?;
    tracing::warn!(
        event = "grants_revoked",
        reason = "operator",
        grant = grant_label(grant_id),
        access_tokens = revoked.access_tokens,
        refresh_tokens = revoked.refresh_tokens,
        "revoked the tokens of one grant"
    );
    Ok(revoked)
}

/// Changes the owner passphrase: revokes every OAuth code and token and every API token, then
/// writes the hash to `path`, mode 600, replacing the file atomically. Credentials go first, so a
/// failed write can't leave a suspected leak still working. Client registrations stay; each client
/// authorizes again. Running servers read the hash at startup, so restart them for the new
/// passphrase to apply, which also ends their sessions. Returns the hash, which the server's own
/// change form takes up at once instead.
pub async fn set_owner_passphrase(
    db: &Db,
    path: &Path,
    passphrase: &str,
) -> Result<String, PassphraseError> {
    let hash = hash_passphrase(passphrase).await?;

    let mut tx = db.begin_write().await?;
    let revoked = oauth::revoke_all(&mut tx).await?;
    api_tokens::delete_all(&mut tx).await?;
    tx.commit().await.map_err(nexus_core::error::Error::from)?;
    tracing::warn!(
        event = "grants_revoked",
        reason = "passphrase_changed",
        access_tokens = revoked.access_tokens,
        refresh_tokens = revoked.refresh_tokens,
        "owner passphrase is changing: revoked all OAuth grants and API tokens"
    );

    let path = path.to_owned();
    tokio::task::spawn_blocking(move || match write_private(&path, &hash) {
        Ok(()) => Ok(hash),
        Err(e) => Err(PassphraseError::Io(path, e)),
    })
    .await
    .map_err(|_| PassphraseError::Task)?
}

/// Writes `contents` and a newline to `path`, mode 600, replacing the file atomically.
pub fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    let mut temp = path.as_os_str().to_owned();
    temp.push(format!(".tmp-{}", hex::encode(&random_bytes()[..8])));
    let temp = PathBuf::from(temp);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    let written = file
        .write_all(contents.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all())
        .and_then(|()| fs::rename(&temp, path));
    if written.is_err() {
        let _ = fs::remove_file(&temp);
        return written;
    }
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

/// Runs off the async workers; `hash` was validated at startup, so a parse failure is a mismatch.
pub(crate) fn verify_passphrase(hash: &str, passphrase: &str) -> bool {
    if passphrase.len() > MAX_PASSPHRASE_BYTES {
        return false;
    }
    PasswordHash::new(hash).is_ok_and(|parsed| {
        Argon2::default()
            .verify_password(passphrase.as_bytes(), &parsed)
            .is_ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repetitive_passphrases_are_short_cycles() {
        for weak in [
            "aaaaaaaaaaaaaaaa",
            "abababababababab",
            "abcabcabcabcabcab",
            "abcdefghabcdefgh",
            "äöäöäöäöäöäöäöäö",
        ] {
            assert!(is_short_cycle(weak), "{weak}");
        }
        for fine in [
            "correct horse battery staple",
            "aaaaaaaaaaaaaaab",
            "abcdefghiabcdefghi",
        ] {
            assert!(!is_short_cycle(fine), "{fine}");
        }
    }
}
