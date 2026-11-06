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
use nexus_core::db::{DB_FILE, db_path, in_use_reason};
use nexus_core::{BlobStore, Db};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, SqliteConnection};

const BLOBS_DIR: &str = "blobs";
const REPLACED_DB: &str = "nexus.db.before-restore";
const STAGE_PREFIX: &str = ".backup-";
const BACKUP_LOCK: &str = ".backup.lock";
const SIDE_FILES: [&str; 2] = ["-wal", "-shm"];

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

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
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
    backup_with_hook(data_dir, out, recipients, async || {}).await
}

/// `after_snapshot` lets a test delete a document at the moment a real delete would hurt.
async fn backup_with_hook(
    data_dir: &Path,
    out: &Path,
    recipients: &[x25519::Recipient],
    mut after_snapshot: impl AsyncFnMut(),
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
    let store = BlobStore::new(data_dir);

    // A document deleted after the snapshot takes its blob with it, but a new snapshot no longer
    // references that blob. So only a blob missing twice is a real failure.
    let mut retried = false;
    loop {
        remove_if_present(&copy)?;
        let mut conn = open_existing(&source).await?;
        sqlx::query("VACUUM INTO ?")
            .bind(copy.to_str().context("backup path is not UTF-8")?)
            .execute(&mut conn)
            .await
            .context("snapshot failed")?;
        sqlx::Connection::close(conn).await?;
        let refs = verify_database(&copy).await?;
        after_snapshot().await;

        let partial = tempfile::Builder::new()
            .prefix(STAGE_PREFIX)
            .suffix(".partial")
            .tempfile_in(parent)?;
        let (summary, unreadable) = write_archive(
            partial.as_file().try_clone()?,
            &copy,
            &store,
            &refs,
            recipients,
        )?;
        if unreadable.is_empty() {
            partial
                .persist(out)
                .with_context(|| format!("cannot write {}", out.display()))?;
            File::open(parent)?.sync_all()?;
            return Ok(summary);
        }
        if retried {
            bail!(describe_unreadable(&copy, &unreadable).await?);
        }
        retried = true;
    }
}

async fn describe_unreadable(copy: &Path, unreadable: &[String]) -> anyhow::Result<String> {
    let mut conn = open_existing(copy).await?;
    let mut text = format!(
        "{} document blobs are missing or corrupt, so no backup was written:",
        unreadable.len()
    );
    for sha in unreadable {
        let ids: Vec<i64> =
            sqlx::query_scalar("SELECT id FROM documents WHERE blob_sha256 = ? ORDER BY id")
                .bind(sha)
                .fetch_all(&mut conn)
                .await?;
        text.push_str(&format!("\n  blob {sha}: documents {ids:?}"));
    }
    text.push_str(
        "\nIf a file is truly gone, record it with `nexus mark-blob-missing` with the server \
         stopped. See deploy/README.md, section 6.",
    );
    sqlx::Connection::close(conn).await?;
    Ok(text)
}

/// Also returns the blobs it could not read. Any of those means the archive is incomplete.
fn write_archive(
    file: File,
    db_copy: &Path,
    store: &BlobStore,
    refs: &[String],
    recipients: &[x25519::Recipient],
) -> anyhow::Result<(Summary, Vec<String>)> {
    let encryptor =
        age::Encryptor::with_recipients(recipients.iter().map(|r| r as &dyn age::Recipient))?;
    let mut tar = tar::Builder::new(encryptor.wrap_output(file)?);
    let mut bytes = fs::metadata(db_copy)?.len();
    let mut unreadable = Vec::new();
    tar.append_path_with_name(db_copy, DB_FILE)?;
    for sha in refs {
        let Ok(content) = store.get(sha) else {
            unreadable.push(sha.clone());
            continue;
        };
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o600);
        tar.append_data(&mut header, format!("{BLOBS_DIR}/{sha}"), &content[..])?;
        bytes += content.len() as u64;
    }
    let file = tar.into_inner()?.finish()?;
    file.sync_all()?;
    let summary = Summary {
        blobs: refs.len(),
        bytes,
    };
    Ok((summary, unreadable))
}

/// Records that these documents' files are truly gone, so backups work again and downloads say so.
/// Changes nothing unless every row qualifies. Returns the rows it changed: a row already marked is
/// left out. Refuses while the server runs, whose reads and writes assume the row is right.
pub async fn mark_blob_missing(data_dir: &Path, ids: &[i64]) -> anyhow::Result<Vec<i64>> {
    let target = db_path(data_dir);
    ensure!(target.is_file(), "no database at {}", target.display());
    if let Some(reason) = in_use_reason(&target).await? {
        bail!(reason);
    }
    let db = Db::open(data_dir).await?;
    let result = mark_rows(&db, ids).await;
    db.close().await;
    result
}

async fn mark_rows(db: &Db, ids: &[i64]) -> anyhow::Result<Vec<i64>> {
    let mut tx = db.begin_write().await?;
    let mut refused = Vec::new();
    for &id in ids {
        let blob: Option<Option<String>> =
            sqlx::query_scalar("SELECT blob_sha256 FROM documents WHERE id = ?")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        match blob {
            None => refused.push(format!("document {id}: no such row")),
            Some(Some(sha)) if db.blobs().exists(&sha)? => {
                refused.push(format!("document {id}: its file {sha} exists"));
            }
            Some(_) => {}
        }
    }
    ensure!(
        refused.is_empty(),
        "nothing was changed:\n  {}",
        refused.join("\n  ")
    );
    let mut changed = Vec::new();
    for &id in ids {
        let updated = sqlx::query(
            "UPDATE documents SET blob_state = 'missing', blob_sha256 = NULL, blob_source = NULL
             WHERE id = ? AND blob_state = 'present'",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated > 0 {
            changed.push(id);
        }
    }
    tx.commit().await?;
    Ok(changed)
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

/// Decrypts `from`, verifies it and installs it into `data_dir`. An existing database is only
/// replaced with `force` and with the server stopped, and is kept as `nexus.db.before-restore`,
/// with its WAL and SHM if it still has them. API
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
    if target.exists()
        && let Some(reason) = in_use_reason(&target).await?
    {
        bail!(reason);
    }
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
    let keep_old = target.exists();
    if keep_old {
        let replaced = data_dir.join(REPLACED_DB);
        // An earlier safety copy's WAL would be replayed onto the one replacing it.
        for suffix in SIDE_FILES {
            remove_if_present(&data_dir.join(format!("{REPLACED_DB}{suffix}")))?;
        }
        fs::rename(&target, &replaced)?;
        fs::set_permissions(&replaced, Permissions::from_mode(0o600))?;
    }
    for suffix in SIDE_FILES {
        let side = data_dir.join(format!("{DB_FILE}{suffix}"));
        if !keep_old {
            remove_if_present(&side)?;
        } else if side.exists() {
            // After a crash the WAL holds committed transactions, and SQLite replays it when the
            // safety copy is opened under this name.
            let kept = data_dir.join(format!("{REPLACED_DB}{suffix}"));
            fs::rename(&side, &kept)?;
            fs::set_permissions(&kept, Permissions::from_mode(0o600))?;
        }
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

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    async fn add_document(db: &Db, label: &str) -> (i64, String) {
        let sha = db.blobs().put(label.as_bytes()).unwrap();
        let id = sqlx::query_scalar(
            "INSERT INTO documents (filename, original_name, size, mime_type, uploaded_at,
                                    blob_sha256, blob_state)
             VALUES (?, ?, 1, 'application/pdf', '2026-01-01T00:00:00.000Z', ?, 'present')
             RETURNING id",
        )
        .bind(label)
        .bind(label)
        .bind(&sha)
        .fetch_one(db.pool())
        .await
        .unwrap();
        (id, sha)
    }

    fn recipient() -> Vec<x25519::Recipient> {
        vec![x25519::Identity::generate().to_public()]
    }

    #[tokio::test]
    async fn a_document_deleted_during_the_backup_does_not_fail_it() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).await.unwrap();
        let (id, sha) = add_document(&db, "deleted during the backup").await;
        add_document(&db, "stays").await;
        let out = dir.path().join("out").join("b.age");
        let snapshots = Cell::new(0);

        let summary = backup_with_hook(dir.path(), &out, &recipient(), async || {
            snapshots.set(snapshots.get() + 1);
            if snapshots.get() == 1 {
                sqlx::query("DELETE FROM documents WHERE id = ?")
                    .bind(id)
                    .execute(db.pool())
                    .await
                    .unwrap();
                db.blobs().delete(&sha).unwrap();
            }
        })
        .await
        .unwrap();

        assert_eq!((snapshots.get(), summary.blobs), (2, 1));
        assert!(out.is_file());
        db.close().await;
    }

    #[tokio::test]
    async fn every_persistently_missing_blob_is_named_with_its_documents() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).await.unwrap();
        let (first, first_sha) = add_document(&db, "first").await;
        let (second, second_sha) = add_document(&db, "second").await;
        add_document(&db, "stays").await;
        db.blobs().delete(&first_sha).unwrap();
        db.blobs().delete(&second_sha).unwrap();
        let out = dir.path().join("out").join("b.age");
        let snapshots = Cell::new(0);

        let message = backup_with_hook(dir.path(), &out, &recipient(), async || {
            snapshots.set(snapshots.get() + 1);
        })
        .await
        .unwrap_err()
        .to_string();

        assert_eq!(snapshots.get(), 2);
        assert!(
            message.contains(&format!("{first_sha}: documents [{first}]")),
            "{message}"
        );
        assert!(
            message.contains(&format!("{second_sha}: documents [{second}]")),
            "{message}"
        );
        assert!(message.contains("mark-blob-missing"), "{message}");
        assert!(!out.exists());
        db.close().await;
    }

    #[tokio::test]
    async fn mark_blob_missing_lets_backups_work_again_and_refuses_what_it_must() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).await.unwrap();
        let (gone, gone_sha) = add_document(&db, "gone").await;
        let (kept, kept_sha) = add_document(&db, "kept").await;
        db.blobs().delete(&gone_sha).unwrap();

        let in_use = mark_blob_missing(dir.path(), &[gone]).await.unwrap_err();
        assert!(in_use.to_string().contains("Stop the server"), "{in_use}");
        db.close().await;

        let refused = mark_blob_missing(dir.path(), &[gone, kept, 999])
            .await
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains(&format!("document {kept}: its file {kept_sha}")),
            "{refused}"
        );
        assert!(refused.contains("document 999: no such row"), "{refused}");
        let out = dir.path().join("out").join("b.age");
        backup(dir.path(), &out, &recipient()).await.unwrap_err();

        assert_eq!(
            mark_blob_missing(dir.path(), &[gone]).await.unwrap(),
            [gone]
        );
        assert!(
            mark_blob_missing(dir.path(), &[gone])
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            backup(dir.path(), &out, &recipient()).await.unwrap().blobs,
            1
        );

        let db = Db::open(dir.path()).await.unwrap();
        let row: (String, Option<String>) =
            sqlx::query_as("SELECT blob_state, blob_sha256 FROM documents WHERE id = ?")
                .bind(gone)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(row, ("missing".to_owned(), None));
        db.close().await;
    }
}
