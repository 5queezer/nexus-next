//! Operator actions, for the server binary to expose as subcommands.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use argon2::password_hash::{PasswordHasher, SaltString};
use argon2::{Argon2, PasswordHash, PasswordVerifier};
use nexus_core::db::Db;
use nexus_core::repo::api_tokens;

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
}

/// Issues a `jt_` API token, storing only its hash, and returns the raw value, which cannot be
/// recovered later. As in old Nexus there is one token at a time, so this revokes any earlier one.
pub async fn issue_api_token(db: &Db, name: &str) -> nexus_core::error::Result<String> {
    let raw = generate_token("jt");
    let mut tx = db.begin_write().await?;
    api_tokens::replace(&mut tx, &sha256_hex(&raw), name).await?;
    tx.commit().await?;
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

/// Writes the owner passphrase's hash to `path`, mode 600, replacing the file atomically. Running
/// servers read the hash at startup, so restart them for the change to apply.
pub async fn set_owner_passphrase(path: &Path, passphrase: &str) -> Result<(), PassphraseError> {
    let hash = hash_passphrase(passphrase).await?;
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
