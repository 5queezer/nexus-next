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
use nexus_core::repo::{cv, cv_approvals, cv_facts, cv_revisions};
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
async fn cv_facts_and_revisions_survive_a_backup_and_a_restore() {
    let keys = new_keys();
    let recipients = read_recipients(&keys.recipient_file).unwrap();
    let source = tempfile::tempdir().unwrap();
    let db = Db::open(source.path()).await.unwrap();
    add_application_with_document(&db, 0).await;
    let mut tx = db.begin_write().await.unwrap();
    let input = cv::CvProfileInput {
        name: "Mara Example".to_owned(),
        contact: serde_json::json!({"email": "mara@example.test"}),
        profile: "Summary with 7 words.".to_owned(),
        skills: serde_json::json!([]),
        experience: serde_json::json!([{"id": "a", "company": "Acme", "title": "Engineer",
            "date": "2020", "location": "Kiel", "tier": 1, "bullets": []}]),
        projects: None,
        education: None,
    };
    cv::upsert_profile(&mut tx, &input).await.unwrap();
    cv_facts::lock(&mut tx, "test").await.unwrap();
    let default = cv_revisions::get_variant(&mut tx, "default")
        .await
        .unwrap()
        .unwrap();
    cv_revisions::append(
        &mut tx,
        default.id,
        &cv_revisions::NewRevision {
            source: "= CV",
            origin: "test",
            summary: "first",
            facts_version: 3,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let facts = cv_facts::get(&mut tx).await.unwrap().facts;
    let facts_hash = cv_approvals::facts_sha256(&facts);
    cv_approvals::record(
        &mut tx,
        &cv_approvals::NewApproval {
            source_sha256: &nexus_core::blob::sha256_hex(b"= CV"),
            facts_sha256: &facts_hash,
            rendered_text: "CV",
            variant_id: default.id,
            revision_number: 1,
            approved_by: "cli",
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let out = source.path().join("backup.age");
    backup::backup(source.path(), &out, &recipients)
        .await
        .unwrap();
    db.close().await;

    let target = tempfile::tempdir().unwrap();
    backup::restore(&out, &keys.identity_file, target.path(), false, false)
        .await
        .unwrap();
    let restored = Db::open(target.path()).await.unwrap();
    let mut conn = restored.conn().await.unwrap();
    let state = cv_facts::get(&mut conn).await.unwrap();
    assert!(state.is_locked());
    let wording = cv_facts::locked_wording(&mut conn).await.unwrap();
    assert!(
        wording.iter().any(|(_, text)| text.contains("7 words")),
        "{wording:?}"
    );
    let profile = cv::get_profile(&mut conn).await.unwrap().unwrap();
    assert_eq!(profile.experience.0[0]["company"], "Acme");
    let variant = cv_revisions::get_variant(&mut conn, "default")
        .await
        .unwrap()
        .unwrap();
    let head = cv_revisions::head(&mut conn, variant.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((head.number, head.source.as_str()), (1, "= CV"));
    let history: i64 = sqlx::query_scalar("SELECT count(*) FROM cv_facts_history")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(history, 2, "the first profile write, then the lock");
    let source_hash = nexus_core::blob::sha256_hex(b"= CV");
    let still_approved = cv_approvals::is_approved(
        &mut conn,
        &source_hash,
        &cv_approvals::facts_sha256(&state.facts),
    )
    .await
    .unwrap();
    assert!(still_approved, "an approval survives a restore");
    let (text, at) = cv_approvals::last_approved(&mut conn, &variant)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((text.as_str(), at), ("CV", 1));
    let changed = sqlx::query("UPDATE cv_approvals SET rendered_text = 'x'")
        .execute(&mut *conn)
        .await
        .unwrap_err();
    assert!(changed.to_string().contains("append-only"), "{changed}");
    let update = sqlx::query("UPDATE cv_revisions SET source = 'x'")
        .execute(&mut *conn)
        .await;
    assert!(update.is_err(), "the triggers came along");
    drop(conn);
    restored.close().await;
}

#[tokio::test]
async fn the_render_cache_stays_out_of_a_backup() {
    let keys = new_keys();
    let recipients = read_recipients(&keys.recipient_file).unwrap();
    let source = tempfile::tempdir().unwrap();
    let db = Db::open(source.path()).await.unwrap();
    let cache = source.path().join("cache/renders");
    fs::create_dir_all(&cache).unwrap();
    fs::write(cache.join("abc-1"), b"a synthetic preview").unwrap();
    let out_dir = tempfile::tempdir().unwrap();
    let out = out_dir.path().join("backup.tar.age");
    backup::backup(source.path(), &out, &recipients)
        .await
        .unwrap();
    db.close().await;

    let target = tempfile::tempdir().unwrap();
    backup::restore(&out, &keys.identity_file, target.path(), false, false)
        .await
        .unwrap();
    assert!(target.path().join("nexus.db").exists());
    assert!(!target.path().join("cache").exists());
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
    let restored = backup::restore(&out, &keys.identity_file, target.path(), false, false)
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

/// A backup of one application, and a data directory as a crash leaves it: two more applications
/// committed only to a WAL that nothing holds open, with files the umask left readable.
async fn backup_and_crashed_data_dir(keys: &Keys) -> (TempDir, PathBuf, TempDir) {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;

    let recipients = read_recipients(&keys.recipient_file).unwrap();
    let source = tempfile::tempdir().unwrap();
    let db = Db::open(source.path()).await.unwrap();
    add_application_with_document(&db, 1).await;
    let out = source.path().join("b.age");
    backup::backup(source.path(), &out, &recipients)
        .await
        .unwrap();

    let live = tempfile::tempdir().unwrap();
    let other = Db::open(live.path()).await.unwrap();
    add_application_with_document(&other, 7).await;
    add_application_with_document(&other, 8).await;
    let target = tempfile::tempdir().unwrap();
    for name in ["nexus.db", "nexus.db-wal", "nexus.db-shm"] {
        fs::copy(live.path().join(name), target.path().join(name)).unwrap();
        fs::set_permissions(target.path().join(name), Permissions::from_mode(0o644)).unwrap();
    }
    other.close().await;
    (source, out, target)
}

const COUNT_APPLICATIONS: &str = "SELECT count(*) FROM applications";

#[tokio::test]
async fn the_safety_copy_opens_with_transactions_only_its_wal_held() {
    let keys = new_keys();
    let (_source, out, target) = backup_and_crashed_data_dir(&keys).await;

    backup::restore(&out, &keys.identity_file, target.path(), true, false)
        .await
        .unwrap();

    let replaced = target.path().join("nexus.db.before-restore");
    assert_eq!(scalar(&replaced, COUNT_APPLICATIONS).await, 2);
    assert_eq!(
        scalar(&target.path().join("nexus.db"), COUNT_APPLICATIONS).await,
        1
    );
}

#[tokio::test]
async fn the_safety_copy_is_private_and_no_stale_wal_can_replay_onto_it() {
    use std::os::unix::fs::PermissionsExt;

    let keys = new_keys();
    let (_source, out, target) = backup_and_crashed_data_dir(&keys).await;

    backup::restore(&out, &keys.identity_file, target.path(), true, false)
        .await
        .unwrap();
    let replaced = target.path().join("nexus.db.before-restore");
    let mode = fs::metadata(&replaced).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    // The in-use check already replayed the crash's WAL into the copy.
    assert!(!target.path().join("nexus.db.before-restore-wal").exists());
    assert!(!target.path().join("nexus.db-wal").exists());
    assert!(!target.path().join("nexus.db-shm").exists());

    // A second restore replaces the safety copy. An old WAL must not replay onto the new one.
    fs::write(target.path().join("nexus.db.before-restore-wal"), b"stale").unwrap();
    backup::restore(&out, &keys.identity_file, target.path(), true, false)
        .await
        .unwrap();
    assert!(!target.path().join("nexus.db.before-restore-wal").exists());
    assert_eq!(scalar(&replaced, COUNT_APPLICATIONS).await, 1);
}

#[tokio::test]
async fn a_forced_restore_refuses_a_database_in_use() {
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
    let live = Db::open(target.path()).await.unwrap();
    add_application_with_document(&live, 7).await;
    add_application_with_document(&live, 8).await;
    let refused = backup::restore(&out, &keys.identity_file, target.path(), true, false)
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("Stop the server"), "{refused}");
    assert!(!target.path().join("nexus.db.before-restore").exists());
    let count: i64 = sqlx::query_scalar(COUNT_APPLICATIONS)
        .fetch_one(live.pool())
        .await
        .unwrap();
    assert_eq!(count, 2);
    live.close().await;
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

    let error = backup::restore(&out, &keys.identity_file, target.path(), false, false)
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

    backup::restore(&out, &keys.identity_file, target.path(), true, false)
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
        let result = backup::restore(&file, &identity, target.path(), false, false).await;
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
    let error = backup::restore(&file, &keys.identity_file, target.path(), false, false)
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
        backup::restore(&out, &keys.identity_file, target.path(), false, false)
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

fn nexus_backup(data: &Path, out: &Path, keys: &Keys) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_nexus"));
    command
        .args(["backup", "--data-dir"])
        .arg(data)
        .arg("--out")
        .arg(out)
        .arg("--recipient-file")
        .arg(&keys.recipient_file);
    command
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[tokio::test(flavor = "multi_thread")]
async fn a_killed_backup_leaves_no_plaintext_where_backups_go_and_the_next_one_cleans_up() {
    use std::os::unix::fs::PermissionsExt;

    let keys = new_keys();
    let data = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let db = Db::open(data.path()).await.unwrap();
    add_application_with_document(&db, 1).await;
    // Big enough that encrypting it outlasts the checks below.
    let sha = db.blobs().put(&vec![7u8; 96 * 1024 * 1024]).unwrap();
    let mut tx = db.begin_write().await.unwrap();
    documents::create(
        &mut tx,
        &NewDocument {
            filename: "big.pdf".into(),
            original_name: "Big.pdf".into(),
            size: 96 * 1024 * 1024,
            mime_type: "application/pdf".into(),
            blob_sha256: sha,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    db.close().await;

    let out = store.path().join("nexus-a.tar.age");
    let mut child = nexus_backup(data.path(), &out, &keys).spawn().unwrap();
    let stage = loop {
        let staged = names_in(data.path())
            .into_iter()
            .find(|n| n.starts_with(".backup-"));
        let encrypting = names_in(store.path())
            .iter()
            .any(|n| n.ends_with(".partial"));
        if let (Some(stage), true) = (staged, encrypting) {
            break data.path().join(stage);
        }
        assert!(child.try_wait().unwrap().is_none(), "backup ended early");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    };
    assert_eq!(
        fs::metadata(&stage).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(
        stage.join("nexus.db").exists(),
        "the snapshot is staged here"
    );

    // A second run must not delete the first one's files from under it.
    let second = nexus_backup(data.path(), &store.path().join("nexus-b.tar.age"), &keys)
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("another backup"));
    assert!(stage.exists());

    // SIGKILL, which no handler could ever catch.
    child.kill().unwrap();
    child.wait().unwrap();
    let left = names_in(store.path());
    assert!(
        left.iter().all(|n| n.ends_with(".partial")),
        "only ciphertext may be left beside the backups: {left:?}"
    );
    assert!(stage.exists());

    // What older builds left beside the backups goes too.
    let old_stage = store.path().join(".backup-old");
    fs::create_dir(&old_stage).unwrap();
    fs::write(old_stage.join("nexus.db"), "plaintext").unwrap();
    let next = nexus_backup(data.path(), &store.path().join("nexus-c.tar.age"), &keys)
        .output()
        .unwrap();
    assert!(
        next.status.success(),
        "{}",
        String::from_utf8_lossy(&next.stderr)
    );
    assert_eq!(names_in(store.path()), ["nexus-c.tar.age"]);
    assert!(
        names_in(data.path())
            .iter()
            .all(|n| !n.starts_with(".backup-")),
        "{:?}",
        names_in(data.path())
    );
}

/// A backup as someone holding only the public key could build it.
fn forged_backup(path: &Path, recipient_file: &Path, staged: &Path) {
    let recipients = read_recipients(recipient_file).unwrap();
    let encryptor =
        age::Encryptor::with_recipients(recipients.iter().map(|r| r as &dyn age::Recipient))
            .unwrap();
    let mut writer = encryptor
        .wrap_output(fs::File::create(path).unwrap())
        .unwrap();
    {
        let mut tar = tar::Builder::new(&mut writer);
        tar.append_path_with_name(staged.join("nexus.db"), "nexus.db")
            .unwrap();
        let junk = b"no document references this";
        let mut header = tar::Header::new_gnu();
        header.set_size(junk.len() as u64);
        header.set_mode(0o600);
        let name = format!("blobs/{}", nexus_core::blob::sha256_hex(junk));
        tar.append_data(&mut header, name, &junk[..]).unwrap();
        tar.finish().unwrap();
    }
    writer.finish().unwrap();
}

#[tokio::test]
async fn restore_clears_credentials_and_skips_unreferenced_blobs_unless_told() {
    use std::os::unix::fs::PermissionsExt;

    let keys = new_keys();
    let source = tempfile::tempdir().unwrap();
    let db = Db::open(source.path()).await.unwrap();
    nexus_auth::issue_api_token(&db, "forged").await.unwrap();
    sqlx::raw_sql(
        r#"INSERT INTO oauth_clients (client_id, redirect_uris, grant_types, response_types)
               VALUES ('forged', '[]', '[]', '[]');
           INSERT INTO oauth_access_tokens (token_hash, client_id, scopes, expires_at)
               VALUES ('a', 'forged', '[]', '2999-01-01T00:00:00.000Z');
           INSERT INTO oauth_refresh_tokens (token_hash, client_id, scopes, expires_at)
               VALUES ('r', 'forged', '[]', '2999-01-01T00:00:00.000Z');
           INSERT INTO oauth_auth_codes (code, client_id, redirect_uri, code_challenge, scopes, expires_at)
               VALUES ('c', 'forged', 'x', 'y', '[]', '2999-01-01T00:00:00.000Z');"#,
    )
    .execute(db.pool())
    .await
    .unwrap();
    db.close().await;
    let file = source.path().join("forged.age");
    forged_backup(&file, &keys.recipient_file, source.path());

    for (keep, expected) in [(false, 0), (true, 1)] {
        let target = tempfile::tempdir().unwrap();
        let summary = backup::restore(&file, &keys.identity_file, target.path(), false, keep)
            .await
            .unwrap();
        assert_eq!(summary.blobs, 0);
        assert!(
            blob_names(target.path()).is_empty(),
            "unreferenced blob installed"
        );
        let restored = target.path().join("nexus.db");
        assert_eq!(
            fs::metadata(&restored).unwrap().permissions().mode() & 0o777,
            0o600
        );
        for sql in [
            "SELECT count(*) FROM api_tokens",
            "SELECT count(*) FROM oauth_clients",
            "SELECT count(*) FROM oauth_access_tokens",
            "SELECT count(*) FROM oauth_refresh_tokens",
            "SELECT count(*) FROM oauth_auth_codes",
        ] {
            assert_eq!(scalar(&restored, sql).await, expected, "{sql}");
        }
    }
}

#[tokio::test]
async fn a_replaced_database_stays_private() {
    use std::os::unix::fs::PermissionsExt;

    let keys = new_keys();
    let source = tempfile::tempdir().unwrap();
    let db = Db::open(source.path()).await.unwrap();
    db.close().await;
    let out = source.path().join("b.age");
    let recipients = read_recipients(&keys.recipient_file).unwrap();
    backup::backup(source.path(), &out, &recipients)
        .await
        .unwrap();

    let target = tempfile::tempdir().unwrap();
    fs::write(target.path().join("nexus.db"), "old").unwrap();
    fs::set_permissions(
        target.path().join("nexus.db"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    backup::restore(&out, &keys.identity_file, target.path(), true, false)
        .await
        .unwrap();
    assert_eq!(
        fs::metadata(target.path().join("nexus.db.before-restore"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}
