//! Encrypted online backup and restore of a data directory.
//!
//! A backup is one `age` file holding a tar of `nexus.db` (a `VACUUM INTO` snapshot, consistent
//! while the server runs) and `blobs/<sha256>` for every blob that snapshot references. The board
//! only holds public recipient keys, so a stolen backup or board cannot decrypt older backups.

use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io::{self, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

use age::x25519;
use anyhow::{Context, bail, ensure};
use nexus_core::BlobStore;
use nexus_core::db::{DB_FILE, db_path};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, SqliteConnection};

const BLOBS_DIR: &str = "blobs";
const REPLACED_DB: &str = "nexus.db.before-restore";
const STAGE_PREFIX: &str = ".backup-";
const BACKUP_LOCK: &str = ".backup.lock";

#[derive(Debug)]
pub struct Summary {
    pub blobs: usize,
    pub bytes: u64,
}

impl std::fmt::Display for Summary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} blobs, {} bytes", self.blobs, self.bytes)
    }
}

/// Reads age recipients (`age1...`), one per line. Blank lines and `#` comments are skipped.
pub fn read_recipients(path: &Path) -> anyhow::Result<Vec<x25519::Recipient>> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("cannot read recipient file {}", path.display()))?;
    let recipients = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            l.parse::<x25519::Recipient>()
                .map_err(|e| anyhow::anyhow!("invalid recipient in {}: {e}", path.display()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    ensure!(
        !recipients.is_empty(),
        "no recipients in {}",
        path.display()
    );
    Ok(recipients)
}

fn is_sha256(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn private_dir(path: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(path)
}

async fn open_existing(path: &Path) -> anyhow::Result<SqliteConnection> {
    SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .busy_timeout(Duration::from_secs(10))
        .connect()
        .await
        .with_context(|| format!("cannot open database {}", path.display()))
}

/// Checks a database file and returns the blobs its documents reference.
async fn verify_database(path: &Path) -> anyhow::Result<Vec<String>> {
    let mut conn = open_existing(path).await?;
    let integrity: Vec<String> = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_all(&mut conn)
        .await?;
    ensure!(integrity == ["ok"], "integrity_check failed: {integrity:?}");
    let violations = sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(&mut conn)
        .await?;
    ensure!(
        violations.is_empty(),
        "foreign_key_check found {} violations",
        violations.len()
    );
    let refs = sqlx::query_scalar(
        "SELECT DISTINCT blob_sha256 FROM documents WHERE blob_state = 'present' ORDER BY 1",
    )
    .fetch_all(&mut conn)
    .await?;
    sqlx::Connection::close(conn).await?;
    Ok(refs)
}

/// Held for a whole backup, and released by the kernel however the process dies, so whatever
/// `remove_stale` finds really is left over.
fn lock_backups(data_dir: &Path) -> anyhow::Result<File> {
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(data_dir.join(BACKUP_LOCK))?;
    lock.try_lock()
        .context("another backup of this data directory is running")?;
    Ok(lock)
}

/// A kill skips `TempDir`'s cleanup. This also clears the plaintext stage directories older builds
/// left next to the backups.
fn remove_stale(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(STAGE_PREFIX)
        {
            continue;
        }
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// Writes an encrypted, verified backup of `data_dir` to `out`. The file appears under its final
/// name only once complete.
///
/// The plaintext snapshot is staged under `data_dir`, which already holds the same data and is never
/// synced anywhere, so a kill can't leave it where the backups go. The encrypted file is written
/// beside `out`: in the systemd unit the two directories are separate mounts, and `rename` can't
/// cross them.
pub async fn backup(
    data_dir: &Path,
    out: &Path,
    recipients: &[x25519::Recipient],
) -> anyhow::Result<Summary> {
    let source = db_path(data_dir);
    ensure!(source.is_file(), "no database at {}", source.display());
    let parent = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    private_dir(parent)?;
    let _lock = lock_backups(data_dir)?;
    remove_stale(data_dir)?;
    remove_stale(parent)?;
    let stage = tempfile::Builder::new()
        .prefix(STAGE_PREFIX)
        .permissions(Permissions::from_mode(0o700))
        .tempdir_in(data_dir)?;
    let copy = stage.path().join(DB_FILE);

    let mut conn = open_existing(&source).await?;
    sqlx::query("VACUUM INTO ?")
        .bind(copy.to_str().context("backup path is not UTF-8")?)
        .execute(&mut conn)
        .await
        .context("snapshot failed")?;
    sqlx::Connection::close(conn).await?;
    let refs = verify_database(&copy).await?;

    let partial = tempfile::Builder::new()
        .prefix(STAGE_PREFIX)
        .suffix(".partial")
        .tempfile_in(parent)?;
    let summary = write_archive(
        partial.as_file().try_clone()?,
        &copy,
        &BlobStore::new(data_dir),
        &refs,
        recipients,
    )?;
    partial
        .persist(out)
        .with_context(|| format!("cannot write {}", out.display()))?;
    File::open(parent)?.sync_all()?;
    Ok(summary)
}

fn write_archive(
    file: File,
    db_copy: &Path,
    store: &BlobStore,
    refs: &[String],
    recipients: &[x25519::Recipient],
) -> anyhow::Result<Summary> {
    let encryptor =
        age::Encryptor::with_recipients(recipients.iter().map(|r| r as &dyn age::Recipient))?;
    let mut tar = tar::Builder::new(encryptor.wrap_output(file)?);
    let mut bytes = fs::metadata(db_copy)?.len();
    tar.append_path_with_name(db_copy, DB_FILE)?;
    for sha in refs {
        let content = store
            .get(sha)
            .with_context(|| format!("document blob {sha} is missing or corrupt"))?;
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o600);
        tar.append_data(&mut header, format!("{BLOBS_DIR}/{sha}"), &content[..])?;
        bytes += content.len() as u64;
    }
    let file = tar.into_inner()?.finish()?;
    file.sync_all()?;
    Ok(Summary {
        blobs: refs.len(),
        bytes,
    })
}

/// Anyone holding the public key can build a complete backup: age proves confidentiality, not who
/// wrote the file. So unless asked, a restored database brings no way in with it.
async fn clear_credentials(path: &Path) -> anyhow::Result<()> {
    let mut conn = open_existing(path).await?;
    sqlx::raw_sql(
        "BEGIN IMMEDIATE;
         DELETE FROM api_tokens;
         DELETE FROM oauth_access_tokens;
         DELETE FROM oauth_refresh_tokens;
         DELETE FROM oauth_auth_codes;
         DELETE FROM oauth_clients;
         COMMIT;",
    )
    .execute(&mut conn)
    .await
    .context("cannot clear credentials from the backup")?;
    sqlx::Connection::close(conn).await?;
    Ok(())
}

/// Decrypts `from`, verifies it and installs it into `data_dir`. Stop the server first. An
/// existing database is only replaced with `force`, and is kept as `nexus.db.before-restore`. API
/// tokens and OAuth clients and grants are cleared unless `keep_credentials`, and blobs no document
/// references are not installed.
pub async fn restore(
    from: &Path,
    identity_file: &Path,
    data_dir: &Path,
    force: bool,
    keep_credentials: bool,
) -> anyhow::Result<Summary> {
    let target = db_path(data_dir);
    ensure!(
        force || !target.exists(),
        "{} already exists: stop the server and pass --force to replace it",
        target.display()
    );
    let identities = age::IdentityFile::from_file(identity_file.to_string_lossy().into_owned())
        .and_then(|f| f.into_identities().map_err(io::Error::other))
        .with_context(|| format!("cannot read identity file {}", identity_file.display()))?;
    private_dir(data_dir)?;
    let stage = tempfile::Builder::new()
        .prefix(".restore-")
        .tempdir_in(data_dir)?;

    extract(from, &identities, stage.path())?;

    let staged_db = stage.path().join(DB_FILE);
    ensure!(staged_db.is_file(), "the backup holds no {DB_FILE}");
    let refs = verify_database(&staged_db).await?;
    let staged_blobs = BlobStore::new(stage.path());
    let present = staged_blobs.list()?;
    for sha in &present {
        staged_blobs
            .get(sha)
            .with_context(|| format!("blob {sha} in the backup is corrupt"))?;
    }
    for sha in &refs {
        ensure!(
            present.binary_search(sha).is_ok(),
            "the backup lacks blob {sha} that a document references"
        );
    }
    if !keep_credentials {
        clear_credentials(&staged_db).await?;
    }

    let blobs_dir = data_dir.join(BLOBS_DIR);
    private_dir(&blobs_dir)?;
    for sha in &refs {
        fs::rename(stage.path().join(BLOBS_DIR).join(sha), blobs_dir.join(sha))?;
    }
    File::open(&blobs_dir)?.sync_all()?;
    if target.exists() {
        let replaced = data_dir.join(REPLACED_DB);
        fs::rename(&target, &replaced)?;
        fs::set_permissions(&replaced, Permissions::from_mode(0o600))?;
    }
    for suffix in ["-wal", "-shm"] {
        let _ = fs::remove_file(data_dir.join(format!("{DB_FILE}{suffix}")));
    }
    fs::rename(&staged_db, &target)?;
    File::open(data_dir)?.sync_all()?;
    Ok(Summary {
        blobs: refs.len(),
        bytes: fs::metadata(&target)?.len(),
    })
}

fn extract(
    from: &Path,
    identities: &[Box<dyn age::Identity + Send + Sync>],
    stage: &Path,
) -> anyhow::Result<()> {
    let input = BufReader::new(
        File::open(from).with_context(|| format!("cannot open {}", from.display()))?,
    );
    let reader = age::Decryptor::new(input)
        .context("not an age file")?
        .decrypt(identities.iter().map(|i| i.as_ref() as &dyn age::Identity))
        .context("cannot decrypt with this identity")?;
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries()? {
        let mut entry = entry?;
        ensure!(
            entry.header().entry_type().is_file(),
            "unexpected entry type"
        );
        let path = entry.path()?.into_owned();
        let name = path.to_str().unwrap_or_default();
        let dest = if name == DB_FILE {
            stage.join(DB_FILE)
        } else if let Some(sha) = name.strip_prefix("blobs/").filter(|s| is_sha256(s)) {
            private_dir(&stage.join(BLOBS_DIR))?;
            stage.join(BLOBS_DIR).join(sha)
        } else {
            bail!("unexpected entry in backup: {name:?}");
        };
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&dest)
            .with_context(|| format!("duplicate entry in backup: {name:?}"))?;
        io::copy(&mut entry, &mut file)?;
        file.flush()?;
        file.sync_all()?;
    }
    // Reading to the end makes age authenticate its final chunk, which catches truncation.
    io::copy(&mut archive.into_inner(), &mut io::sink())
        .context("backup is truncated or corrupt")?;
    Ok(())
}
