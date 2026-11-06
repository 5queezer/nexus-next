//! Backup and restore against synthetic data.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use age::secrecy::ExposeSecret;
use age::x25519;
use nexus_core::Db;
use nexus_core::repo::applications::{self, ApplicationFields};
use nexus_core::repo::documents::{self, NewDocument};
use nexus_server::backup::{self, read_recipients};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection};
use tempfile::TempDir;

struct Keys {
    _dir: TempDir,
    recipient_file: PathBuf,
    identity_file: PathBuf,
}

fn new_keys() -> Keys {
    let dir = tempfile::tempdir().unwrap();
    let identity = x25519::Identity::generate();
    let recipient_file = dir.path().join("recipients.txt");
    let identity_file = dir.path().join("identity.txt");
    fs::write(
        &recipient_file,
        format!("# backup key\n\n{}\n", identity.to_public()),
    )
    .unwrap();
    fs::write(
        &identity_file,
        format!("{}\n", identity.to_string().expose_secret()),
    )
    .unwrap();
    Keys {
        _dir: dir,
        recipient_file,
        identity_file,
    }
}

async fn add_application_with_document(db: &Db, n: usize) {
    let sha = db
        .blobs()
        .put(format!("synthetic document {n}").as_bytes())
        .unwrap();
    let mut tx = db.begin_write().await.unwrap();
    let app = applications::create(
        &mut tx,
        &format!("Company {n}"),
        "Engineer",
        ApplicationFields::default(),
    )
    .await
    .unwrap();
    documents::create(
        &mut tx,
        &NewDocument {
            filename: format!("doc{n}.pdf"),
            original_name: format!("Doc {n}.pdf"),
            size: 10,
            mime_type: "application/pdf".into(),
            application_ids: vec![app.id],
            blob_sha256: sha,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

async fn scalar(db_file: &Path, sql: &'static str) -> i64 {
    let mut conn = SqliteConnectOptions::new()
        .filename(db_file)
        .connect()
        .await
        .unwrap();
    let value = sqlx::query_scalar(sql).fetch_one(&mut conn).await.unwrap();
    conn.close().await.unwrap();
    value
}

fn blob_names(data_dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(data_dir.join("blobs"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn backup_then_restore_reproduces_database_and_blobs() {
    let keys = new_keys();
    let recipients = read_recipients(&keys.recipient_file).unwrap();
    let source = tempfile::tempdir().unwrap();
    let db = Db::open(source.path()).await.unwrap();
    for n in 0..5 {
        add_application_with_document(&db, n).await;
    }
    // A document whose file is known to be missing is not an error.
    sqlx::query("INSERT INTO documents (filename, original_name, size, mime_type, uploaded_at) VALUES ('m.pdf', 'm.pdf', 1, 'application/pdf', '2026-01-01T00:00:00.000Z')")
        .execute(db.pool())
        .await
        .unwrap();

    let out_dir = tempfile::tempdir().unwrap();
    let out = out_dir.path().join("backup.tar.age");
    let summary = backup::backup(source.path(), &out, &recipients)
        .await
        .unwrap();
    assert_eq!(summary.blobs, 5);

    let raw = fs::read(&out).unwrap();
    assert!(raw.starts_with(b"age-encryption.org/v1"));
    assert!(!raw.windows(15).any(|w| w == b"SQLite format 3"));
    assert!(!raw.windows(18).any(|w| w == b"synthetic document"));
    assert_eq!(
        fs::read_dir(out_dir.path()).unwrap().count(),
        1,
        "no staging files are left next to the backup"
    );

    let target = tempfile::tempdir().unwrap();
    let restored = backup::restore(&out, &keys.identity_file, target.path(), false)
        .await
        .unwrap();
    assert_eq!(restored.blobs, 5);
    let db_file = target.path().join("nexus.db");
    assert_eq!(
        scalar(&db_file, "SELECT count(*) FROM applications").await,
        5
    );
    assert_eq!(scalar(&db_file, "SELECT count(*) FROM documents").await, 6);
    assert_eq!(blob_names(target.path()), blob_names(source.path()));
    let reopened = Db::open(target.path()).await.unwrap();
    for sha in reopened.blobs().list().unwrap() {
        assert_eq!(
            reopened.blobs().get(&sha).unwrap(),
            db.blobs().get(&sha).unwrap()
        );
    }
    assert!(
        fs::read_dir(target.path()).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".restore-")),
        "no staging directory is left behind"
    );
}

#[tokio::test]
async fn restore_refuses_to_overwrite_without_force_and_keeps_the_old_database_with_it() {
    let keys = new_keys();
    let recipients = read_recipients(&keys.recipient_file).unwrap();
    let source = tempfile::tempdir().unwrap();
    let db = Db::open(source.path()).await.unwrap();
    add_application_with_document(&db, 1).await;
    let out = source.path().join("b.age");
    backup::backup(source.path(), &out, &recipients)
        .await
        .unwrap();

    let target = tempfile::tempdir().unwrap();
    let other = Db::open(target.path()).await.unwrap();
    add_application_with_document(&other, 7).await;
    add_application_with_document(&other, 8).await;
    other.close().await;

    let error = backup::restore(&out, &keys.identity_file, target.path(), false)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("--force"), "{error}");
    assert_eq!(
        scalar(
            &target.path().join("nexus.db"),
            "SELECT count(*) FROM applications"
        )
        .await,
        2
    );

    backup::restore(&out, &keys.identity_file, target.path(), true)
        .await
        .unwrap();
    assert_eq!(
        scalar(
            &target.path().join("nexus.db"),
            "SELECT count(*) FROM applications"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(
            &target.path().join("nexus.db.before-restore"),
            "SELECT count(*) FROM applications"
        )
        .await,
        2
    );
}

#[tokio::test]
async fn wrong_key_truncation_and_tampering_install_nothing() {
    let keys = new_keys();
    let recipients = read_recipients(&keys.recipient_file).unwrap();
    let source = tempfile::tempdir().unwrap();
    let db = Db::open(source.path()).await.unwrap();
    for n in 0..3 {
        add_application_with_document(&db, n).await;
    }
    let out = source.path().join("b.age");
    backup::backup(source.path(), &out, &recipients)
        .await
        .unwrap();
    let good = fs::read(&out).unwrap();

    let stranger = new_keys();
    let mut truncated = good.clone();
    truncated.truncate(good.len() - 40);
    let mut tampered = good.clone();
    let middle = tampered.len() / 2;
    tampered[middle] ^= 0xff;

    let cases = [
        ("wrong key", good.clone(), stranger.identity_file.clone()),
        ("truncated", truncated, keys.identity_file.clone()),
        ("tampered", tampered, keys.identity_file.clone()),
    ];
    for (label, bytes, identity) in cases {
        let file = source.path().join("case.age");
        fs::write(&file, bytes).unwrap();
        let target = tempfile::tempdir().unwrap();
        let result = backup::restore(&file, &identity, target.path(), false).await;
        assert!(result.is_err(), "{label} must fail");
        assert!(!target.path().join("nexus.db").exists(), "{label}");
        assert!(
            !target.path().join("blobs").exists()
                || fs::read_dir(target.path().join("blobs")).unwrap().count() == 0,
            "{label}"
        );
    }
}

#[tokio::test]
async fn archives_with_unexpected_entries_are_refused() {
    let keys = new_keys();
    let recipients = read_recipients(&keys.recipient_file).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("evil.age");
    {
        let encryptor =
            age::Encryptor::with_recipients(recipients.iter().map(|r| r as &dyn age::Recipient))
                .unwrap();
        let mut tar = tar::Builder::new(
            encryptor
                .wrap_output(fs::File::create(&file).unwrap())
                .unwrap(),
        );
        let mut header = tar::Header::new_gnu();
        header.set_size(4);
        header.set_mode(0o600);
        tar.append_data(&mut header, "blobs/not-a-hash", &b"evil"[..])
            .unwrap();
        tar.into_inner().unwrap().finish().unwrap().flush().unwrap();
    }
    let target = tempfile::tempdir().unwrap();
    let error = backup::restore(&file, &keys.identity_file, target.path(), false)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unexpected entry"), "{error}");
    assert!(!target.path().join("nexus.db").exists());
}

#[tokio::test]
async fn backup_fails_when_a_referenced_blob_is_missing() {
    let keys = new_keys();
    let recipients = read_recipients(&keys.recipient_file).unwrap();
    let source = tempfile::tempdir().unwrap();
    let db = Db::open(source.path()).await.unwrap();
    add_application_with_document(&db, 1).await;
    let sha = db.blobs().list().unwrap().remove(0);
    db.blobs().delete(&sha).unwrap();

    let out = source.path().join("b.age");
    let error = backup::backup(source.path(), &out, &recipients)
        .await
        .unwrap_err();
    assert!(error.to_string().contains(&sha), "{error:#}");
    assert!(!out.exists());
    let leftovers: Vec<_> = fs::read_dir(source.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with(".backup-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backup_taken_during_concurrent_writes_is_consistent() {
    let keys = new_keys();
    let recipients = read_recipients(&keys.recipient_file).unwrap();
    let source = tempfile::tempdir().unwrap();
    let db = Db::open(source.path()).await.unwrap();
    let written = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (db, written, stop) = (db.clone(), written.clone(), stop.clone());
        tokio::spawn(async move {
            let mut n = 0;
            while !stop.load(Ordering::Relaxed) {
                add_application_with_document(&db, n).await;
                n += 1;
                written.store(n, Ordering::Relaxed);
            }
        })
    };
    while written.load(Ordering::Relaxed) < 20 {
        tokio::task::yield_now().await;
    }

    let out_dir = tempfile::tempdir().unwrap();
    let mut backups = Vec::new();
    for i in 0..3 {
        let floor = written.load(Ordering::Relaxed);
        let out = out_dir.path().join(format!("b{i}.age"));
        backup::backup(source.path(), &out, &recipients)
            .await
            .unwrap();
        backups.push((out, floor));
    }
    stop.store(true, Ordering::Relaxed);
    writer.await.unwrap();
    let total = written.load(Ordering::Relaxed);

    for (out, floor) in backups {
        let target = tempfile::tempdir().unwrap();
        backup::restore(&out, &keys.identity_file, target.path(), false)
            .await
            .unwrap();
        let db_file = target.path().join("nexus.db");
        let apps = scalar(&db_file, "SELECT count(*) FROM applications").await as usize;
        assert!(
            floor <= apps && apps <= total,
            "{floor} <= {apps} <= {total}"
        );
        let docs = scalar(&db_file, "SELECT count(*) FROM documents").await as usize;
        assert_eq!(docs, apps, "a snapshot never holds half a transaction");
        assert_eq!(blob_names(target.path()).len(), apps);
        assert_eq!(
            scalar(&db_file, "SELECT count(*) FROM pragma_foreign_key_check").await,
            0
        );
    }
}

#[test]
fn recipient_files_are_validated() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("r");
    fs::write(&file, "# nothing\n\n").unwrap();
    assert!(
        read_recipients(&file)
            .unwrap_err()
            .to_string()
            .contains("no recipients")
    );
    fs::write(&file, "age1notakey\n").unwrap();
    assert!(
        read_recipients(&file)
            .unwrap_err()
            .to_string()
            .contains("invalid recipient")
    );
    assert!(read_recipients(&dir.path().join("missing")).is_err());
}
