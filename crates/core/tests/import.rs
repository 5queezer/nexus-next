//! Importer behaviour against the synthetic export in `tests/fixtures/export`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use nexus_core::Db;
use nexus_core::blob::sha256_hex;
use nexus_core::db::db_path;
use nexus_core::import::{self, ImportError, ImportOptions, ImportSummary, TableCount};
use nexus_core::repo::{applications, cv, cv_facts, documents};
use serde_json::{Value, json};
use sqlx::Row as _;
use tempfile::TempDir;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/export");

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// A scratch copy of the fixture export plus an empty data directory.
struct Env {
    _root: TempDir,
    export: PathBuf,
    data: PathBuf,
}

impl Env {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let export = root.path().join("export");
        copy_dir(Path::new(FIXTURE), &export);
        let data = root.path().join("data");
        Self {
            _root: root,
            export,
            data,
        }
    }

    fn options(&self) -> ImportOptions {
        ImportOptions {
            export_dir: self.export.clone(),
            data_dir: self.data.clone(),
            approve_recovered: None,
            force: false,
        }
    }

    async fn import(&self) -> Result<ImportSummary, ImportError> {
        import::run(&self.options()).await
    }

    async fn import_approving(&self, approvals: &str) -> Result<ImportSummary, ImportError> {
        let path = self.export.parent().unwrap().join("approve.txt");
        fs::write(&path, approvals).unwrap();
        import::run(&ImportOptions {
            approve_recovered: Some(path),
            ..self.options()
        })
        .await
    }

    /// Recomputes MANIFEST.json after a deliberate edit, so that the edit itself is what fails.
    fn reseal(&self) {
        fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(root, &path, out);
                } else if path.file_name().unwrap() != "MANIFEST.json" {
                    let relative = path
                        .strip_prefix(root)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_owned();
                    out.insert(relative, sha256_hex(&fs::read(&path).unwrap()));
                }
            }
        }
        let mut files = BTreeMap::new();
        walk(&self.export, &self.export, &mut files);
        let path = self.export.join("MANIFEST.json");
        let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        manifest["files"] = json!(files);
        fs::write(path, manifest.to_string()).unwrap();
    }

    fn edit_rows(&self, file: &str, edit: impl FnOnce(&mut Vec<Value>)) {
        let path = self.export.join("jsonl").join(format!("{file}.jsonl"));
        let text = fs::read_to_string(&path).unwrap();
        let mut rows: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        edit(&mut rows);
        let lines: Vec<String> = rows.iter().map(|row| format!("{row}\n")).collect();
        fs::write(path, lines.concat()).unwrap();
        let count = rows.len();
        self.edit_json("row-counts.json", |counts| counts[file] = json!(count));
    }

    fn edit_json(&self, relative: &str, edit: impl FnOnce(&mut Value)) {
        let path = self.export.join(relative);
        let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        edit(&mut value);
        fs::write(path, value.to_string()).unwrap();
        self.reseal();
    }

    async fn db(&self) -> Db {
        Db::open(&self.data).await.unwrap()
    }

    /// Every row of every table, as text, so two imports can be compared in full.
    async fn dump(&self) -> Vec<String> {
        let db = self.db().await;
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name != '_sqlx_migrations'
             ORDER BY name",
        )
        .fetch_all(db.pool())
        .await
        .unwrap();
        let mut out = Vec::new();
        for table in tables {
            let columns: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info(?)")
                .bind(&table)
                .fetch_all(db.pool())
                .await
                .unwrap();
            let expr = columns
                .iter()
                .map(|c| format!("quote(\"{c}\")"))
                .collect::<Vec<_>>()
                .join(" || '|' || ");
            let rows: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT {expr} FROM \"{table}\" ORDER BY 1"
            )))
            .fetch_all(db.pool())
            .await
            .unwrap();
            out.push(format!("{table}: {rows:?}"));
        }
        db.close().await;
        out
    }
}

fn count(summary: &ImportSummary, table: &str) -> (usize, usize) {
    let TableCount {
        imported, skipped, ..
    } = summary.tables.iter().find(|t| t.table == table).unwrap();
    (*imported, *skipped)
}

fn assert_invalid<T: std::fmt::Debug>(result: Result<T, ImportError>, expected: &str) {
    match result {
        Err(ImportError::Invalid(message)) => {
            assert!(message.contains(expected), "{message:?} lacks {expected:?}")
        }
        other => panic!("expected Invalid({expected:?}), got {other:?}"),
    }
}

#[tokio::test]
async fn happy_path_imports_owner_rows_and_skips_the_rest() {
    let env = Env::new();
    let summary = env.import().await.unwrap();
    assert_eq!(count(&summary, "applications"), (3, 1));
    assert_eq!(count(&summary, "contacts"), (2, 1));
    assert_eq!(count(&summary, "application_submissions"), (1, 1));
    assert_eq!(count(&summary, "documents"), (8, 2));
    assert_eq!(count(&summary, "application_documents"), (3, 2));
    assert_eq!(count(&summary, "application_events"), (3, 2));
    assert_eq!(count(&summary, "cv_profile"), (1, 1));
    assert_eq!(count(&summary, "cv_patches"), (1, 1));
    assert_eq!(summary.documents_with_blob, 4);
    assert_eq!(summary.documents_missing_blob, 4);
    assert_eq!(summary.recovered_documents, 0);
    assert_eq!(summary.hashes_computed, 1);
    assert_eq!(summary.blob_files, 3);
    assert!(!env.data.join("nexus.db.importing").exists());

    let db = env.db().await;
    let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(integrity, "ok");
    db.close().await;
}

#[tokio::test]
async fn ids_are_preserved_and_new_ids_continue_after_the_max() {
    let env = Env::new();
    env.import().await.unwrap();
    let db = env.db().await;
    let mut conn = db.conn().await.unwrap();

    let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM applications ORDER BY id")
        .fetch_all(&mut *conn)
        .await
        .unwrap();
    assert_eq!(ids, [3, 7, 12]);
    let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM documents ORDER BY id")
        .fetch_all(&mut *conn)
        .await
        .unwrap();
    assert_eq!(ids, [5, 6, 8, 15, 21, 22, 23, 24]);

    let created = applications::create(&mut conn, "New Co", "Engineer", Default::default())
        .await
        .unwrap();
    assert_eq!(created.id, 15);
    let event: i64 = sqlx::query_scalar(
        "INSERT INTO application_events (application_id, type, occurred_at)
         VALUES (3, 'note', '2026-05-01T00:00:00.000Z') RETURNING id",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(event, 13);

    let profile_id: i64 = sqlx::query_scalar("SELECT id FROM cv_profile")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(profile_id, 2);
    drop(conn);
    db.close().await;
}

#[tokio::test]
async fn a_real_profile_row_splits_as_upsert_would() {
    let env = Env::new();
    let experience = json!([{"id": "brightpath", "company": "Brightpath Logistics",
        "title": "Software Engineer", "date": "2019 – 2022", "location": "Hamburg", "tier": 1,
        "bullets": ["Built the routing service."]}]);
    let education = json!([{"institution": "Nordsee University", "degree": "BSc Computer Science",
        "date": "2015 – 2018", "location": "Kiel", "details": "Thesis on routing."}]);
    env.edit_rows("CvProfile", |rows| {
        rows[0]["experience"] = experience.clone();
        rows[0]["education"] = education.clone();
    });
    env.reseal();
    env.import().await.unwrap();

    let db = env.db().await;
    let mut conn = db.conn().await.unwrap();
    let profile = cv::get_profile(&mut conn).await.unwrap().unwrap();
    assert_eq!(profile.id, 2);
    assert_eq!(profile.name, "Invented Person");
    assert_eq!(profile.contact.0, json!({"email": "me@example.test"}));
    assert_eq!(profile.experience.0, experience);
    assert_eq!(profile.education.0, education);
    let (kept,): (String,) = sqlx::query_as("SELECT experience FROM cv_profile")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert!(
        !kept.contains("Brightpath"),
        "cv_profile keeps only wording"
    );
    let state = cv_facts::get(&mut conn).await.unwrap();
    assert_eq!(state.facts.employment[0].company, "Brightpath Logistics");
    assert_eq!(state.facts.education[0].id, "edu-1");
    assert!(!state.is_locked(), "the owner locks after reading them");
    drop(conn);
    db.close().await;
}

#[tokio::test]
async fn a_profile_row_whose_facts_could_not_be_read_back_is_refused() {
    let env = Env::new();
    env.edit_rows("CvProfile", |rows| {
        rows[0]["experience"] = json!([{"id": "x", "company": 7}]);
    });
    env.reseal();
    assert_invalid(env.import().await, "experience");
}

#[tokio::test]
async fn next_ids_follow_the_postgres_sequence_or_the_max_whichever_is_higher() {
    let env = Env::new();
    env.import().await.unwrap();
    let db = env.db().await;
    let mut conn = db.conn().await.unwrap();

    // Contact's largest row is 5 but its sequence is 6, so ID 6 was issued and must not be reused.
    let contact: i64 = sqlx::query_scalar(
        "INSERT INTO contacts (name, application_id) VALUES ('Synthetic Contact', 3) RETURNING id",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(contact, 7);

    // Document's sequence is 2, below its largest row of 24, so the max wins.
    let document: i64 = sqlx::query_scalar(
        "INSERT INTO documents (filename, original_name, size, mime_type)
         VALUES ('synthetic.txt', 'synthetic.txt', 1, 'text/plain') RETURNING id",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(document, 25);

    let issued: String = sqlx::query_scalar(
        "SELECT group_concat(name || '=' || seq, ' ') FROM (SELECT name, seq FROM sqlite_sequence ORDER BY name)",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(
        issued,
        "application_events=12 application_submissions=9 applications=14 contacts=7 \
         cv_patches=6 cv_profile=2 cv_variants=1 documents=25"
    );
    drop(conn);
    db.close().await;
}

#[tokio::test]
async fn a_truncated_or_miscounted_jsonl_fails_the_row_count() {
    let env = Env::new();
    let path = env.export.join("jsonl/Application.jsonl");
    let text = fs::read_to_string(&path).unwrap();
    let kept: Vec<&str> = text.lines().take(3).collect();
    fs::write(&path, kept.join("\n") + "\n").unwrap();
    env.reseal();
    assert_invalid(
        env.import().await,
        "Application: 3 rows in its jsonl file, row-counts.json says 4",
    );
    assert!(!db_path(&env.data).exists());

    let env = Env::new();
    env.edit_json("row-counts.json", |counts| counts["Contact"] = json!(4));
    assert_invalid(
        env.import().await,
        "Contact: 3 rows in its jsonl file, row-counts.json says 4",
    );
    assert!(!db_path(&env.data).exists());

    let env = Env::new();
    env.edit_json("row-counts.json", |counts| {
        counts.as_object_mut().unwrap().shift_remove("Document");
    });
    assert_invalid(
        env.import().await,
        "row-counts.json has no count for Document",
    );
}

#[tokio::test]
async fn sequences_and_row_counts_are_required() {
    for relative in ["sequences.json", "row-counts.json"] {
        let env = Env::new();
        fs::remove_file(env.export.join(relative)).unwrap();
        assert_invalid(env.import().await, &format!("cannot read {relative}"));
        assert!(!db_path(&env.data).exists());

        let env = Env::new();
        let path = env.export.join("MANIFEST.json");
        let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        manifest["files"]
            .as_object_mut()
            .unwrap()
            .shift_remove(relative);
        fs::write(&path, manifest.to_string()).unwrap();
        assert_invalid(env.import().await, &format!("{relative} is not listed"));
        assert!(!db_path(&env.data).exists());
    }

    let env = Env::new();
    env.edit_json("sequences.json", |sequences| {
        sequences.as_object_mut().unwrap().shift_remove("Contact");
    });
    assert_invalid(
        env.import().await,
        "sequences.json has no value for Contact",
    );
}

#[tokio::test]
async fn timestamps_are_canonical_utc_everywhere() {
    let env = Env::new();
    env.import().await.unwrap();
    let db = env.db().await;

    let row = sqlx::query(
        "SELECT applied_at, last_contact, job_captured_at, created_at, updated_at
         FROM applications WHERE id = 3",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    let values: Vec<String> = (0..5).map(|i| row.get(i)).collect();
    assert_eq!(
        values,
        [
            "2026-03-05T09:00:00.000Z",
            "2026-03-06T09:00:00.500Z",
            "2026-03-01T08:00:00.125Z",
            "2026-03-01T10:00:00.100Z",
            "2026-03-02T11:30:15.250Z",
        ]
    );
    db.close().await;
}

#[tokio::test]
async fn values_round_trip_through_the_repository() {
    let env = Env::new();
    env.import().await.unwrap();
    let db = env.db().await;
    let mut conn = db.conn().await.unwrap();

    let application = applications::get(&mut conn, 3).await.unwrap().unwrap();
    assert_eq!(application.company, "Acme Robotics");
    assert_eq!(application.status, "interview");
    assert_eq!(application.event_version, 2);
    assert!(application.remote);
    assert_eq!(application.visa_sponsorship, Some(false));
    assert_eq!(application.eligible_countries.0, [json!("DE"), json!("AT")]);
    let application = applications::get(&mut conn, 7).await.unwrap().unwrap();
    assert!(application.eligible_countries.0.is_empty());

    let metadata: String =
        sqlx::query_scalar("SELECT metadata FROM application_events WHERE id = 10")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(
        metadata,
        r#"{"zeta":1,"alpha":{"b":1,"a":2},"requestHash":"legacy"}"#
    );
    let nulls: (Option<String>, Option<String>) =
        sqlx::query_as("SELECT metadata, idempotency_key FROM application_events WHERE id = 12")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(nulls, (None, None));
    let contact_id: String =
        sqlx::query_scalar("SELECT contact_id FROM application_events WHERE id = 11")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(contact_id, "5");
    drop(conn);
    db.close().await;
}

#[tokio::test]
async fn blob_states_follow_the_export_and_files_are_stored() {
    let env = Env::new();
    env.import().await.unwrap();
    let db = env.db().await;
    let mut conn = db.conn().await.unwrap();

    let shared = documents::get(&mut conn, 5).await.unwrap().unwrap();
    assert_eq!(shared.blob_state, documents::BlobState::Present);
    assert_eq!(shared.blob_source.as_deref(), Some("gcs"));
    assert!(!shared.content_hash_computed);
    let address = shared.blob_sha256.clone().unwrap();
    assert_eq!(shared.content_hash.as_deref(), Some(address.as_str()));
    assert_eq!(
        db.blobs().get(&address).unwrap(),
        b"Synthetic CV, version A.\n"
    );
    let twin = documents::get(&mut conn, 6).await.unwrap().unwrap();
    assert_eq!(twin.blob_sha256.as_deref(), Some(address.as_str()));

    let computed = documents::get(&mut conn, 8).await.unwrap().unwrap();
    assert!(computed.content_hash_computed);
    assert_eq!(computed.content_hash, computed.blob_sha256);
    assert!(computed.content_hash.is_some());

    let submitted = documents::get(&mut conn, 15).await.unwrap().unwrap();
    assert_eq!(submitted.submission_id, Some(4));
    assert!(submitted.is_submission_document());

    for id in [21, 22, 23, 24] {
        let missing = documents::get(&mut conn, id).await.unwrap().unwrap();
        assert_eq!(missing.blob_state, documents::BlobState::Missing, "{id}");
        assert_eq!(missing.blob_sha256, None);
        assert_eq!(missing.blob_source, None);
        assert_eq!(missing.content_hash, None);
    }
    assert_eq!(db.blobs().list().unwrap().len(), 3);
    drop(conn);
    db.close().await;
}

#[tokio::test]
async fn data_files_are_private_to_the_owner() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    env.import().await.unwrap();
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&env.data), 0o700);
    assert_eq!(mode(&db_path(&env.data)), 0o600);
    let blob = fs::read_dir(env.data.join("blobs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(mode(&blob), 0o600);
}

#[tokio::test]
async fn approved_candidates_import_with_the_right_file() {
    let env = Env::new();
    let summary = env
        .import_approving("# confirmed by a human\n21\n\n22 # size only\n")
        .await
        .unwrap();
    assert_eq!(summary.documents_with_blob, 6);
    assert_eq!(summary.documents_missing_blob, 2);
    assert_eq!(summary.recovered_documents, 2);
    assert_eq!(summary.hashes_computed, 3);
    assert_eq!(summary.blob_files, 5);

    let db = env.db().await;
    let mut conn = db.conn().await.unwrap();
    let name_matched = documents::get(&mut conn, 21).await.unwrap().unwrap();
    assert_eq!(name_matched.blob_state, documents::BlobState::Present);
    assert_eq!(
        name_matched.blob_source.as_deref(),
        Some("recovered-hermes")
    );
    assert!(name_matched.content_hash_computed);
    assert_eq!(name_matched.content_hash, name_matched.blob_sha256);
    let address = name_matched.blob_sha256.unwrap();
    assert_eq!(db.blobs().get(&address).unwrap(), b"recovered-X-right\n");
    let size_only = documents::get(&mut conn, 22).await.unwrap().unwrap();
    assert_eq!(
        db.blobs().get(&size_only.blob_sha256.unwrap()).unwrap(),
        b"recovered-Z\n"
    );
    drop(conn);
    db.close().await;
}

#[tokio::test]
async fn approval_failures_are_loud_and_change_nothing() {
    let env = Env::new();
    env.import().await.unwrap();
    let before = env.dump().await;

    assert_invalid(env.import_approving("23\n").await, "ambiguous");
    assert_invalid(env.import_approving("24\n").await, "no recovered candidate");
    assert_invalid(env.import_approving("5\n").await, "not awaiting recovery");
    assert_invalid(env.import_approving("30\n").await, "not awaiting recovery");
    assert_invalid(
        env.import_approving("9999\n").await,
        "not awaiting recovery",
    );
    assert_invalid(
        env.import_approving("twenty-one\n").await,
        "not a document id",
    );
    let missing_file = ImportOptions {
        approve_recovered: Some(env.export.join("no-such-file")),
        ..env.options()
    };
    assert_invalid(
        import::run(&missing_file).await,
        "cannot read approval file",
    );

    assert_eq!(env.dump().await, before);
}

#[tokio::test]
async fn a_candidate_failing_its_checks_is_rejected() {
    let env = Env::new();
    env.edit_rows("Document", |rows| {
        let row = rows.iter_mut().find(|row| row["id"] == 22).unwrap();
        row["size"] = json!(1);
    });
    assert_invalid(env.import_approving("22\n").await, "document 22: size");

    let env = Env::new();
    env.edit_rows("Document", |rows| {
        let row = rows.iter_mut().find(|row| row["id"] == 22).unwrap();
        row["contentHash"] = json!("a".repeat(64));
    });
    assert_invalid(
        env.import_approving("22\n").await,
        "document 22: stored contentHash",
    );
}

#[tokio::test]
async fn a_second_name_match_is_ambiguous() {
    let env = Env::new();
    env.edit_json("recovered-candidates/MATCHES.json", |matches| {
        let entry = &mut matches["documents"][0];
        entry["name_matched"] = entry["candidates"].clone();
    });
    assert_invalid(env.import_approving("21\n").await, "ambiguous");
}

#[tokio::test]
async fn reimport_gives_the_same_database() {
    let env = Env::new();
    let first = env.import().await.unwrap();
    let first_dump = env.dump().await;
    let first_blobs = fs::read_dir(env.data.join("blobs")).unwrap().count();

    let second = env.import().await.unwrap();
    assert_eq!(first, second);
    assert_eq!(env.dump().await, first_dump);
    assert_eq!(
        fs::read_dir(env.data.join("blobs")).unwrap().count(),
        first_blobs
    );

    // Opened and read by a server, an import still counts as untouched.
    env.db().await.close().await;
    env.import().await.unwrap();
    assert_eq!(env.dump().await, first_dump);
}

#[tokio::test]
async fn withdrawing_an_approval_removes_the_candidate_file() {
    let env = Env::new();
    env.import_approving("21\n").await.unwrap();
    assert_eq!(fs::read_dir(env.data.join("blobs")).unwrap().count(), 4);
    let summary = env.import().await.unwrap();
    assert_eq!(summary.orphan_blobs_removed, 1);
    assert_eq!(fs::read_dir(env.data.join("blobs")).unwrap().count(), 3);
}

#[tokio::test]
async fn a_written_database_is_refused_unless_forced() {
    let env = Env::new();
    env.import().await.unwrap();
    let db = env.db().await;
    let mut conn = db.conn().await.unwrap();
    applications::create(&mut conn, "Live Row", "Engineer", Default::default())
        .await
        .unwrap();
    drop(conn);
    db.close().await;
    let live = env.dump().await;

    assert!(matches!(env.import().await, Err(ImportError::Refused(_))));
    assert_eq!(env.dump().await, live);

    let forced = ImportOptions {
        force: true,
        ..env.options()
    };
    import::run(&forced).await.unwrap();
    let db = env.db().await;
    let mut conn = db.conn().await.unwrap();
    assert!(applications::get(&mut conn, 13).await.unwrap().is_none());
    drop(conn);
    db.close().await;
}

#[tokio::test]
async fn a_failed_forced_import_keeps_the_old_database() {
    let env = Env::new();
    env.import().await.unwrap();
    let db = env.db().await;
    let mut conn = db.conn().await.unwrap();
    applications::create(&mut conn, "Live Row", "Engineer", Default::default())
        .await
        .unwrap();
    drop(conn);
    db.close().await;
    let live = env.dump().await;

    fs::write(env.export.join("jsonl/Contact.jsonl"), "tampered\n").unwrap();
    let forced = ImportOptions {
        force: true,
        ..env.options()
    };
    assert_invalid(
        import::run(&forced).await,
        "jsonl/Contact.jsonl does not match",
    );
    assert_eq!(env.dump().await, live);
}

#[tokio::test]
async fn an_empty_database_is_replaced_but_a_foreign_file_is_not() {
    let env = Env::new();
    env.db().await.close().await;
    env.import().await.unwrap();

    let env = Env::new();
    fs::create_dir_all(&env.data).unwrap();
    fs::write(db_path(&env.data), "not a database").unwrap();
    assert!(matches!(env.import().await, Err(ImportError::Refused(_))));
    assert_eq!(fs::read(db_path(&env.data)).unwrap(), b"not a database");
}

#[tokio::test]
async fn an_open_database_is_refused() {
    let env = Env::new();
    env.import().await.unwrap();
    let db = env.db().await;
    assert!(env.data.join("nexus.db-wal").exists());
    assert!(matches!(env.import().await, Err(ImportError::Refused(_))));
    db.close().await;
    env.import().await.unwrap();
}

/// Leaves the data directory as a crash would: a row committed only to a WAL nobody holds open.
async fn crash_with_a_row_in_the_wal(env: &Env) {
    env.import().await.unwrap();
    let db = env.db().await;
    let mut conn = db.conn().await.unwrap();
    applications::create(&mut conn, "Live Row", "Engineer", Default::default())
        .await
        .unwrap();
    drop(conn);
    let names = ["nexus.db", "nexus.db-wal", "nexus.db-shm"];
    let image: Vec<Vec<u8>> = names
        .iter()
        .map(|name| fs::read(env.data.join(name)).unwrap())
        .collect();
    db.close().await;
    for (name, bytes) in names.iter().zip(image) {
        fs::write(env.data.join(name), bytes).unwrap();
    }
}

fn forced(env: &Env) -> ImportOptions {
    ImportOptions {
        force: true,
        ..env.options()
    }
}

#[tokio::test]
async fn an_open_database_is_refused_even_when_forced() {
    let env = Env::new();
    env.import().await.unwrap();
    let db = env.db().await;
    let mut conn = db.conn().await.unwrap();
    let live = applications::create(&mut conn, "Live Row", "Engineer", Default::default())
        .await
        .unwrap();

    let Err(ImportError::Refused(message)) = import::run(&forced(&env)).await else {
        panic!("a forced import replaced a database in use");
    };
    assert!(message.contains("Stop the server"), "{message}");
    assert!(
        applications::get(&mut conn, live.id)
            .await
            .unwrap()
            .is_some()
    );
    assert!(env.data.join("nexus.db-wal").exists());

    drop(conn);
    db.close().await;
    import::run(&forced(&env)).await.unwrap();
}

#[tokio::test]
async fn a_crashed_wal_does_not_block_a_forced_import() {
    let env = Env::new();
    crash_with_a_row_in_the_wal(&env).await;
    assert!(env.data.join("nexus.db-wal").exists());

    import::run(&forced(&env)).await.unwrap();

    assert!(!env.data.join("nexus.db-wal").exists());
    let db = env.db().await;
    let mut conn = db.conn().await.unwrap();
    assert!(applications::get(&mut conn, 13).await.unwrap().is_none());
    drop(conn);
    db.close().await;
}

#[tokio::test]
async fn a_row_only_in_a_crashed_wal_still_counts_as_live_data() {
    let env = Env::new();
    crash_with_a_row_in_the_wal(&env).await;
    let Err(ImportError::Refused(message)) = env.import().await else {
        panic!("an import without --force replaced a row from the WAL");
    };
    assert!(message.contains("--force"), "{message}");
}

#[tokio::test]
async fn a_wal_that_cannot_be_removed_names_the_way_out() {
    use std::os::unix::fs::PermissionsExt;

    let env = Env::new();
    crash_with_a_row_in_the_wal(&env).await;
    let set_mode = |mode| {
        fs::set_permissions(&env.data, fs::Permissions::from_mode(mode)).unwrap();
    };
    set_mode(0o500);
    if fs::write(env.data.join("probe"), "").is_ok() {
        // Running as root, which ignores the mode.
        set_mode(0o700);
        return;
    }
    let refused = import::run(&forced(&env)).await;
    set_mode(0o700);
    let Err(ImportError::Refused(message)) = refused else {
        panic!("a forced import went past a WAL it could not remove: {refused:?}");
    };
    assert!(
        message.contains("can write to the data directory"),
        "{message}"
    );

    import::run(&forced(&env)).await.unwrap();
}

#[tokio::test]
async fn tampering_fails_the_manifest_check() {
    for relative in [
        "jsonl/Application.jsonl",
        "blob-index.json",
        "MISSING_DOCUMENTS.json",
    ] {
        let env = Env::new();
        let path = env.export.join(relative);
        let mut bytes = fs::read(&path).unwrap();
        bytes.push(b'\n');
        fs::write(&path, bytes).unwrap();
        assert_invalid(env.import().await, &format!("{relative} does not match"));
        assert!(!db_path(&env.data).exists());
    }
}

#[tokio::test]
async fn tampered_blob_fails_the_manifest_check() {
    let env = Env::new();
    let blob = fs::read_dir(env.export.join("blobs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::write(&blob, "tampered").unwrap();
    assert_invalid(env.import().await, "does not match");
    assert!(!db_path(&env.data).exists());
}

#[tokio::test]
async fn files_missing_from_the_manifest_or_disk_fail() {
    let env = Env::new();
    let path = env.export.join("MANIFEST.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    manifest["files"]
        .as_object_mut()
        .unwrap()
        .shift_remove("jsonl/Contact.jsonl");
    fs::write(&path, manifest.to_string()).unwrap();
    assert_invalid(env.import().await, "jsonl/Contact.jsonl is not listed");

    let env = Env::new();
    fs::remove_file(env.export.join("jsonl/Contact.jsonl")).unwrap();
    assert_invalid(env.import().await, "cannot read jsonl/Contact.jsonl");

    let env = Env::new();
    fs::remove_file(env.export.join("MANIFEST.json")).unwrap();
    assert_invalid(env.import().await, "cannot read MANIFEST.json");
}

#[tokio::test]
async fn document_and_blob_mismatches_fail() {
    let env = Env::new();
    env.edit_rows("Document", |rows| rows[0]["size"] = json!(1));
    assert_invalid(env.import().await, "document 5: size differs");

    let env = Env::new();
    env.edit_rows("Document", |rows| {
        rows[0]["contentHash"] = json!("a".repeat(64))
    });
    assert_invalid(env.import().await, "document 5: stored contentHash differs");

    let env = Env::new();
    env.edit_json("blob-index.json", |index| {
        index["file-5.pdf"]["size"] = json!(1);
    });
    assert_invalid(env.import().await, "size for document 5 is wrong");

    let env = Env::new();
    env.edit_json("MANIFEST.json", |manifest| {
        manifest["missing_documents"] = json!(3)
    });
    assert_invalid(env.import().await, "counts 3 missing documents");

    let env = Env::new();
    env.edit_json("MISSING_DOCUMENTS.json", |list| {
        list["documents"][0]["id"] = json!(4242);
    });
    assert_invalid(env.import().await, "unknown document 4242");
}

#[tokio::test]
async fn the_blob_index_decides_blob_state_over_the_missing_list() {
    let env = Env::new();
    env.edit_json("blob-index.json", |index| {
        index.as_object_mut().unwrap().shift_remove("file-5.pdf");
    });
    let summary = env.import().await.unwrap();
    assert_eq!(summary.missing_list_disagreements, 1);
    assert_eq!(summary.documents_missing_blob, 5);
    let db = env.db().await;
    let mut conn = db.conn().await.unwrap();
    let absent = documents::get(&mut conn, 5).await.unwrap().unwrap();
    assert_eq!(absent.blob_state, documents::BlobState::Missing);
    assert_eq!(absent.blob_sha256, None);
    drop(conn);
    db.close().await;

    let env = Env::new();
    env.edit_json("MISSING_DOCUMENTS.json", |list| {
        list["documents"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id": 5}));
    });
    env.edit_json("MANIFEST.json", |manifest| {
        manifest["missing_documents"] = json!(5)
    });
    let summary = env.import().await.unwrap();
    assert_eq!(summary.missing_list_disagreements, 1);
    let db = env.db().await;
    let mut conn = db.conn().await.unwrap();
    let indexed = documents::get(&mut conn, 5).await.unwrap().unwrap();
    assert_eq!(indexed.blob_state, documents::BlobState::Present);
    drop(conn);
    db.close().await;

    let env = Env::new();
    assert_eq!(env.import().await.unwrap().missing_list_disagreements, 0);
}

#[tokio::test]
async fn foreign_key_violations_fail_and_name_the_row() {
    let env = Env::new();
    env.edit_rows("Contact", |rows| rows[0]["applicationId"] = json!(4242));
    assert_invalid(
        env.import().await,
        "contacts row 2 references missing applications",
    );

    let env = Env::new();
    env.edit_rows("ApplicationEvent", |rows| {
        rows[0]["applicationId"] = json!(4242)
    });
    assert_invalid(env.import().await, "application_events row 10");

    let env = Env::new();
    env.edit_rows("Document", |rows| rows[0]["submissionId"] = json!(4242));
    assert_invalid(
        env.import().await,
        "documents row 5 references missing application_submissions",
    );

    let env = Env::new();
    env.edit_rows("ApplicationToDocument", |rows| rows[0]["B"] = json!(4242));
    assert_invalid(env.import().await, "foreign key violations");
    assert!(!env.data.join("nexus.db.importing").exists());
    assert!(!db_path(&env.data).exists());
}

#[tokio::test]
async fn row_level_problems_fail_with_their_line() {
    let env = Env::new();
    env.edit_rows("Application", |rows| {
        let duplicate = rows[0].clone();
        rows.push(duplicate);
    });
    assert_invalid(env.import().await, "Application line 5: ");

    let env = Env::new();
    env.edit_rows("Application", |rows| {
        rows[1]["appliedAt"] = json!("2026-03-05")
    });
    assert_invalid(
        env.import().await,
        "Application line 2: appliedAt is not a UTC timestamp",
    );

    let env = Env::new();
    env.edit_rows("Application", |rows| {
        rows[1]["appliedAt"] = json!("2026-03-05T09:00:00Z")
    });
    assert_invalid(env.import().await, "Application line 2: appliedAt");

    let env = Env::new();
    env.edit_rows("Application", |rows| rows[1]["rating"] = json!("five"));
    assert_invalid(
        env.import().await,
        "Application line 2: rating is not an integer",
    );

    let env = Env::new();
    env.edit_rows("Application", |rows| rows[1]["remote"] = json!(1));
    assert_invalid(
        env.import().await,
        "Application line 2: remote is not a boolean",
    );

    let env = Env::new();
    env.edit_rows("Application", |rows| rows[1]["company"] = Value::Null);
    assert_invalid(env.import().await, "Application line 2: ");

    let env = Env::new();
    let path = env.export.join("jsonl/Contact.jsonl");
    let mut text = fs::read_to_string(&path).unwrap();
    text.push_str("{broken\n");
    fs::write(&path, text).unwrap();
    env.reseal();
    assert_invalid(env.import().await, "jsonl/Contact.jsonl line 4");
}

#[tokio::test]
async fn export_schema_changes_fail() {
    let env = Env::new();
    env.edit_rows("Application", |rows| rows[1]["brandNewColumn"] = json!(1));
    assert_invalid(env.import().await, "unknown column brandNewColumn");

    let env = Env::new();
    env.edit_rows("Application", |rows| {
        rows[1].as_object_mut().unwrap().shift_remove("rating");
    });
    assert_invalid(env.import().await, "missing column rating");

    let env = Env::new();
    env.edit_rows("ApplicationToDocument", |rows| {
        rows[0].as_object_mut().unwrap().shift_remove("A");
    });
    assert_invalid(env.import().await, "missing column applicationId");
}

#[tokio::test]
async fn the_owner_is_recorded_for_mcp_responses() {
    let env = Env::new();
    env.import().await.unwrap();
    let db = nexus_core::Db::open(&env.data).await.unwrap();
    let owner = nexus_core::import::imported_owner(&mut db.conn().await.unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        owner.1.as_deref(),
        Some("agent"),
        "the most common MCP actor"
    );
    assert!(!owner.0.is_empty());
}

#[tokio::test]
async fn owner_must_be_unique() {
    let env = Env::new();
    env.edit_rows("Application", |rows| {
        rows[1]["userId"] = json!("user_second")
    });
    assert_invalid(env.import().await, "2 different userId values");

    let env = Env::new();
    env.edit_rows("Application", |rows| rows.clear());
    assert_invalid(env.import().await, "owner is unknown");
}

#[tokio::test]
async fn scanned_email_rows_fail() {
    let env = Env::new();
    env.edit_rows("ScannedEmail", |rows| rows.push(json!({"id": 1})));
    assert_invalid(env.import().await, "ScannedEmail has 1 rows");
}
