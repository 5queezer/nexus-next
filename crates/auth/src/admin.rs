//! Operator actions, for the server binary to expose as subcommands.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use argon2::password_hash::{PasswordHasher, SaltString};
use argon2::{Argon2, PasswordHash, PasswordVerifier};
use nexus_core::db::Db;
use nexus_core::repo::{api_tokens, oauth};

use crate::crypto::{generate_token, random_bytes, sha256_hex};

const MIN_PASSPHRASE_CHARS: usize = 12;
pub(crate) const MAX_PASSPHRASE_BYTES: usize = 1024;

#[derive(Debug, thiserror::Error)]
pub enum PassphraseError {
    #[error("passphrase must be at least {MIN_PASSPHRASE_CHARS} characters")]
    TooShort,
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

/// Changes the owner passphrase: revokes every OAuth code and token and every API token, then
/// writes the hash to `path`, mode 600, replacing the file atomically. Credentials go first, so a
/// failed write can't leave a suspected leak still working. Client registrations stay; each client
/// authorizes again. Running servers read the hash at startup, so restart them for the new
/// passphrase to apply, which also ends their sessions.
pub async fn set_owner_passphrase(
    db: &Db,
    path: &Path,
    passphrase: &str,
) -> Result<(), PassphraseError> {
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
    tokio::task::spawn_blocking(move || {
        write_private(&path, &hash).map_err(|e| PassphraseError::Io(path, e))
    })
    .await
    .map_err(|_| PassphraseError::Task)?
}

fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
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
