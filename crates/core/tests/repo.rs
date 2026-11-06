//! Schema constraints and repository semantics against a real SQLite file.

use chrono::Duration;
use nexus_core::repo::applications::{
    self, ApplicationFields, ApplicationFilter, Guard, SearchField,
};
use nexus_core::repo::contacts::{self, ContactPatch, NewContact};
use nexus_core::repo::documents::{self, DocumentFilter, DocumentPatch, NewDocument};
use nexus_core::repo::events::{self, EventFilter, NewEvent};
use nexus_core::repo::submissions::{self, NewSubmission};
use nexus_core::repo::{api_tokens, cv, oauth};
use nexus_core::{Db, Error, Timestamp};
use serde_json::json;
use sqlx::SqliteConnection;
use tempfile::TempDir;

async fn open() -> (TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).await.unwrap();
    (dir, db)
}

fn ts(value: &str) -> Timestamp {
    Timestamp::parse(value).unwrap()
}

async fn new_app(conn: &mut SqliteConnection, company: &str) -> i64 {
    applications::create(conn, company, "Engineer", ApplicationFields::default())
        .await
        .unwrap()
        .id
}

async fn new_doc(conn: &mut SqliteConnection, name: &str, apps: Vec<i64>) -> i64 {
    let new = NewDocument {
        filename: format!("{name}.pdf"),
        original_name: name.to_owned(),
        size: 3,
        mime_type: "application/pdf".to_owned(),
        application_ids: apps,
        blob_sha256: "a".repeat(64),
        ..NewDocument::default()
    };
    documents::create(conn, &new).await.unwrap().id
}

fn new_submission(application_id: i64, key: &str) -> NewSubmission {
    NewSubmission {
        application_id,
        idempotency_key: key.to_owned(),
        request_hash: "b".repeat(64),
        submitted_at: ts("2026-04-01T10:00:00Z"),
        application_url: None,
        ats_name: None,
        requisition_id: None,
        language: None,
        answers: json!([{"question": "q", "answer": "a"}]),
        policy: json!({"humanReviewed": true}),
        candidate_salary_min: None,
        candidate_salary_max: None,
        candidate_salary_currency: None,
        candidate_salary_period: None,
        candidate_salary_type: None,
        candidate_salary_flexible: false,
        document_ids: vec![],
    }
}

async fn scalar(conn: &mut SqliteConnection, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(conn).await.unwrap()
}

#[tokio::test]
async fn connections_use_wal_and_foreign_keys() {
    let (_dir, db) = open().await;
    let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(mode, "wal");
    let keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(keys, 1);
    let mut conn = db.conn().await.unwrap();
    let orphan = sqlx::query("INSERT INTO contacts (name, application_id) VALUES ('x', 999)")
        .execute(&mut *conn)
        .await;
    assert!(orphan.is_err());
}

#[tokio::test]
async fn deleting_an_application_cascades_as_prisma_did() {
    let (_dir, db) = open().await;
    let mut tx = db.begin_write().await.unwrap();
    let app = new_app(&mut tx, "Acme").await;
    let other = new_app(&mut tx, "Globex").await;
    contacts::create(
        &mut tx,
        app,
        &NewContact {
            name: "Pat".into(),
            ..NewContact::default()
        },
    )
    .await
    .unwrap();
    events::insert(
        &mut tx,
        &NewEvent {
            application_id: app,
            r#type: "note".into(),
            ..NewEvent::default()
        },
    )
    .await
    .unwrap();
    let keeper = events::insert(
        &mut tx,
        &NewEvent {
            application_id: other,
            r#type: "note".into(),
            ..NewEvent::default()
        },
    )
    .await
    .unwrap();
    cv::upsert_patch(&mut tx, app, &cv::CvPatchInput::default())
        .await
        .unwrap();
    let linked = new_doc(&mut tx, "linked", vec![app]).await;
    let claimed = new_doc(&mut tx, "claimed", vec![app]).await;
    let submission = submissions::insert(&mut tx, &new_submission(app, "key"))
        .await
        .unwrap();
    documents::claim_for_submission(&mut tx, &[claimed], submission.id, &submission.submitted_at)
        .await
        .unwrap();

    applications::delete(&mut tx, app).await.unwrap();

    assert!(applications::get(&mut tx, app).await.unwrap().is_none());
    assert_eq!(scalar(&mut tx, "SELECT count(*) FROM contacts").await, 0);
    assert_eq!(
        scalar(&mut tx, "SELECT count(*) FROM application_submissions").await,
        0
    );
    assert_eq!(scalar(&mut tx, "SELECT count(*) FROM cv_patches").await, 0);
    assert_eq!(
        scalar(&mut tx, "SELECT count(*) FROM application_documents").await,
        0
    );
    let remaining: Vec<i64> = sqlx::query_scalar("SELECT id FROM application_events")
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    assert_eq!(remaining, [keeper.id]);

    let linked = documents::get(&mut tx, linked).await.unwrap().unwrap();
    assert!(linked.applications.is_empty());
    assert_eq!(linked.state, "current");
    let claimed = documents::get(&mut tx, claimed).await.unwrap().unwrap();
    assert_eq!(claimed.state, "historical");
    assert_eq!(claimed.submission_id, None);

    assert!(matches!(
        applications::delete(&mut tx, app).await,
        Err(Error::NotFound)
    ));
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn deleting_a_submission_sets_the_document_link_null() {
    let (_dir, db) = open().await;
    let mut conn = db.conn().await.unwrap();
    let app = new_app(&mut conn, "Acme").await;
    let doc = new_doc(&mut conn, "cv", vec![]).await;
    let submission = submissions::insert(&mut conn, &new_submission(app, "key"))
        .await
        .unwrap();
    documents::claim_for_submission(&mut conn, &[doc], submission.id, &submission.submitted_at)
        .await
        .unwrap();
    sqlx::query("DELETE FROM application_submissions")
        .execute(&mut *conn)
        .await
        .unwrap();
    assert_eq!(
        documents::get(&mut conn, doc)
            .await
            .unwrap()
            .unwrap()
            .submission_id,
        None
    );
}

#[tokio::test]
async fn deleting_a_document_removes_only_its_links() {
    let (_dir, db) = open().await;
    let mut conn = db.conn().await.unwrap();
    let app = new_app(&mut conn, "Acme").await;
    let doc = new_doc(&mut conn, "cv", vec![app]).await;
    documents::delete(&mut conn, doc).await.unwrap().unwrap();
    assert_eq!(
        scalar(&mut conn, "SELECT count(*) FROM application_documents").await,
        0
    );
    assert!(applications::exists(&mut conn, app).await.unwrap());
}

#[tokio::test]
async fn events_cannot_be_updated_but_still_cascade() {
    let (_dir, db) = open().await;
    let mut conn = db.conn().await.unwrap();
    let app = new_app(&mut conn, "Acme").await;
    let event = events::insert(
        &mut conn,
        &NewEvent {
            application_id: app,
            r#type: "note".into(),
            ..NewEvent::default()
        },
    )
    .await
    .unwrap();

    let error = sqlx::query("UPDATE application_events SET outcome = 'changed' WHERE id = ?")
        .bind(event.id)
        .execute(&mut *conn)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("append-only"), "{error}");

    sqlx::query("DELETE FROM application_events WHERE id = ?")
        .bind(event.id)
        .execute(&mut *conn)
        .await
        .unwrap();
    events::insert(
        &mut conn,
        &NewEvent {
            application_id: app,
            r#type: "note".into(),
            ..NewEvent::default()
        },
    )
    .await
    .unwrap();
    applications::delete(&mut conn, app).await.unwrap();
    assert_eq!(
        scalar(&mut conn, "SELECT count(*) FROM application_events").await,
        0
    );
}

#[tokio::test]
async fn the_schema_rejects_bad_json_timestamps_and_blob_states() {
    let (_dir, db) = open().await;
    let mut conn = db.conn().await.unwrap();
    let now = "2026-01-01T00:00:00.000Z";
    for sql in [
        "INSERT INTO applications (company, role, updated_at) VALUES ('a', 'b', '2026-01-01T00:00:00')",
        "INSERT INTO applications (company, role, updated_at, applied_at) VALUES ('a', 'b', ?1, '2026-01-01')",
        "INSERT INTO applications (company, role, updated_at, eligible_countries) VALUES ('a', 'b', ?1, 'not json')",
        "INSERT INTO applications (company, role, updated_at, remote) VALUES ('a', 'b', ?1, 2)",
        "INSERT INTO applications (company, role, updated_at, rating) VALUES ('a', 'b', ?1, 'high')",
        "INSERT INTO documents (filename, original_name, size, mime_type, blob_state) VALUES ('f', 'n', 1, 'm', 'present')",
        "INSERT INTO documents (filename, original_name, size, mime_type, blob_sha256) VALUES ('f', 'n', 1, 'm', '../../etc/passwd')",
        "INSERT INTO documents (filename, original_name, size, mime_type, blob_source) VALUES ('f', 'n', 1, 'm', 'gcs')",
        "INSERT INTO documents (filename, original_name, size, mime_type, blob_state) VALUES ('f', 'n', 1, 'm', 'lost')",
    ] {
        let result = sqlx::query(sql).bind(now).execute(&mut *conn).await;
        assert!(result.is_err(), "accepted: {sql}");
    }
    let accepted =
        sqlx::query("INSERT INTO applications (company, role, updated_at) VALUES ('a', 'b', ?)")
            .bind(now)
            .execute(&mut *conn)
            .await;
    assert!(accepted.is_ok());
}

#[tokio::test]
async fn unique_columns_hold_without_a_user_id() {
    let (_dir, db) = open().await;
    let mut conn = db.conn().await.unwrap();
    let fields = ApplicationFields {
        canonical_job_url: Some(Some("https://jobs.example.test/1".into())),
        ..ApplicationFields::default()
    };
    let first = applications::create(&mut conn, "A", "R", fields.clone())
        .await
        .unwrap();
    assert!(matches!(
        applications::create(&mut conn, "B", "R", fields.clone()).await,
        Err(Error::CanonicalJobUrlConflict)
    ));
    let second = new_app(&mut conn, "C").await;
    let third = new_app(&mut conn, "D").await;
    assert_ne!(second, third);
    assert!(matches!(
        applications::update(&mut conn, second, fields, Guard::None).await,
        Err(Error::CanonicalJobUrlConflict)
    ));
    let found = applications::find_by_canonical_job_url(&mut conn, "https://jobs.example.test/1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.id, first.id);

    let event = NewEvent {
        application_id: first.id,
        r#type: "note".into(),
        idempotency_key: Some("key".into()),
        request_hash: Some("hash".into()),
        ..NewEvent::default()
    };
    events::insert(&mut conn, &event).await.unwrap();
    let error = events::insert(&mut conn, &event).await.unwrap_err();
    assert!(error.is_unique_violation());
    let replay = events::find_by_idempotency_key(&mut conn, "key")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(replay.persisted_request_hash(), Some("hash"));

    submissions::insert(&mut conn, &new_submission(first.id, "sub"))
        .await
        .unwrap();
    let error = submissions::insert(&mut conn, &new_submission(first.id, "sub"))
        .await
        .unwrap_err();
    assert!(error.is_unique_violation());
}

#[tokio::test]
async fn create_applies_old_nexus_defaults() {
    let (_dir, db) = open().await;
    let mut conn = db.conn().await.unwrap();
    let lead = new_app(&mut conn, "Lead").await;
    let lead = applications::get(&mut conn, lead).await.unwrap().unwrap();
    assert_eq!(lead.status, "inbound");
    assert!(lead.applied_at.is_none());
    assert!(!lead.remote);
    assert_eq!(lead.event_version, 0);
    assert_eq!(lead.created_at, lead.updated_at);
    assert!(lead.eligible_countries.0.is_empty());

    let sent = applications::create(
        &mut conn,
        "Sent",
        "Engineer",
        ApplicationFields {
            status: Some("interview".into()),
            ..ApplicationFields::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(sent.status, "interview");
    assert!(sent.applied_at.is_some());

    // Old Nexus tests the raw status for the default, so a differently cased one gets no date.
    let cased = applications::create(
        &mut conn,
        "Cased",
        "Engineer",
        ApplicationFields {
            status: Some("Interview".into()),
            ..ApplicationFields::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(cased.status, "interview");
    assert!(cased.applied_at.is_none());

    let too_long = ApplicationFields {
        notes: Some(Some("x".repeat(10_001))),
        ..ApplicationFields::default()
    };
    assert!(matches!(
        applications::create(&mut conn, "A", "R", too_long).await,
        Err(Error::NotesTooLong)
    ));
}

#[tokio::test]
async fn update_bumps_versions_and_honours_guards() {
    let (_dir, db) = open().await;
    let mut conn = db.conn().await.unwrap();
    let id = new_app(&mut conn, "Acme").await;
    let original = applications::get(&mut conn, id).await.unwrap().unwrap();

    let set_company = |name: &str| ApplicationFields {
        company: Some(name.to_owned()),
        ..ApplicationFields::default()
    };
    let updated = applications::update(&mut conn, id, set_company("Acme 2"), Guard::None)
        .await
        .unwrap();
    assert_eq!(updated.company, "Acme 2");
    assert_eq!(updated.event_version, 1);
    assert!(updated.updated_at >= original.updated_at);

    let cleared = ApplicationFields {
        source: Some(Some("x".into())),
        ..ApplicationFields::default()
    };
    let set = applications::update(&mut conn, id, cleared, Guard::None)
        .await
        .unwrap();
    assert_eq!(set.source.as_deref(), Some("x"));
    let cleared = ApplicationFields {
        source: Some(None),
        ..ApplicationFields::default()
    };
    let cleared = applications::update(&mut conn, id, cleared, Guard::None)
        .await
        .unwrap();
    assert_eq!(cleared.source, None);
    assert_eq!(cleared.company, "Acme 2");

    let stale = Guard::UpdatedAt(ts("2000-01-01T00:00:00Z"));
    assert!(matches!(
        applications::update(&mut conn, id, set_company("no"), stale).await,
        Err(Error::Conflict)
    ));
    assert!(matches!(
        applications::update(&mut conn, id, set_company("no"), Guard::EventVersion(99)).await,
        Err(Error::Conflict)
    ));
    let current = applications::get(&mut conn, id).await.unwrap().unwrap();
    assert_eq!(current.company, "Acme 2");
    let good = Guard::EventVersion(current.event_version);
    applications::update(&mut conn, id, set_company("yes"), good)
        .await
        .unwrap();
    let good = Guard::UpdatedAt(
        applications::get(&mut conn, id)
            .await
            .unwrap()
            .unwrap()
            .updated_at,
    );
    applications::update(&mut conn, id, set_company("yes"), good)
        .await
        .unwrap();

    assert!(matches!(
        applications::update(&mut conn, 999, set_company("x"), Guard::None).await,
        Err(Error::NotFound)
    ));
    assert!(matches!(
        applications::update(&mut conn, 999, set_company("x"), Guard::EventVersion(0)).await,
        Err(Error::NotFound)
    ));
}

#[tokio::test]
async fn unchanged_notes_skip_validation_and_append_note_joins_with_a_blank_line() {
    let (_dir, db) = open().await;
    let mut conn = db.conn().await.unwrap();
    let id = new_app(&mut conn, "Acme").await;

    let first = applications::append_note(&mut conn, id, "first", None)
        .await
        .unwrap();
    assert_eq!(first.notes.as_deref(), Some("first"));
    assert_eq!(first.event_version, 1);
    let second = applications::append_note(&mut conn, id, "second", None)
        .await
        .unwrap();
    assert_eq!(second.notes.as_deref(), Some("first\n\nsecond"));

    assert!(matches!(
        applications::append_note(&mut conn, id, "x", Some(&ts("2000-01-01T00:00:00Z"))).await,
        Err(Error::Conflict)
    ));
    assert!(matches!(
        applications::append_note(&mut conn, id, &"x".repeat(10_000), None).await,
        Err(Error::NotesTooLong)
    ));
    assert!(matches!(
        applications::append_note(&mut conn, 999, "x", None).await,
        Err(Error::NotFound)
    ));
    let guarded = applications::append_note(&mut conn, id, "third", Some(&second.updated_at)).await;
    assert!(guarded.is_ok());

    // An over-long note already stored stays writable as long as it is not changed.
    let long = "y".repeat(10_001);
    sqlx::query("UPDATE applications SET notes = ? WHERE id = ?")
        .bind(&long)
        .bind(id)
        .execute(&mut *conn)
        .await
        .unwrap();
    let same = ApplicationFields {
        notes: Some(Some(long)),
        ..ApplicationFields::default()
    };
    assert!(
        applications::update(&mut conn, id, same, Guard::None)
            .await
            .is_ok()
    );
    let changed = ApplicationFields {
        notes: Some(Some("z".repeat(10_001))),
        ..ApplicationFields::default()
    };
    assert!(matches!(
        applications::update(&mut conn, id, changed, Guard::None).await,
        Err(Error::NotesTooLong)
    ));
}

#[tokio::test]
async fn stored_status_reads_normalized() {
    let (_dir, db) = open().await;
    let mut conn = db.conn().await.unwrap();
    let id = new_app(&mut conn, "Acme").await;
    sqlx::query("UPDATE applications SET status = 'ghost' WHERE id = ?")
        .bind(id)
        .execute(&mut *conn)
        .await
        .unwrap();
    assert_eq!(
        applications::get(&mut conn, id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "rejected"
    );
    assert_eq!(
        applications::list(&mut conn).await.unwrap()[0].status,
        "rejected"
    );
    assert_eq!(
        applications::list_identities(&mut conn).await.unwrap()[0].status,
        "rejected"
    );
}

#[tokio::test]
async fn filtered_lists_sort_nulls_like_postgres_and_page_by_cursor() {
    let (_dir, db) = open().await;
    let mut conn = db.conn().await.unwrap();
    let mut ids = Vec::new();
    for (company, rating) in [("A", Some(5)), ("B", None), ("C", Some(1)), ("D", None)] {
        let fields = ApplicationFields {
            rating: Some(rating),
            ..ApplicationFields::default()
        };
        ids.push(
            applications::create(&mut conn, company, "R", fields)
                .await
                .unwrap()
                .id,
        );
    }
    let order = |rows: Vec<applications::Application>| {
        rows.into_iter()
            .map(|row| row.company)
            .collect::<Vec<_>>()
            .concat()
    };
    let by = |sort: &str| ApplicationFilter {
        sort: Some(sort.to_owned()),
        ..ApplicationFilter::default()
    };

    assert_eq!(
        order(
            applications::list_filtered(&mut conn, &by("-rating"))
                .await
                .unwrap()
        ),
        "DBAC"
    );
    assert_eq!(
        order(
            applications::list_filtered(&mut conn, &by("rating"))
                .await
                .unwrap()
        ),
        "CABD"
    );
    assert_eq!(
        order(
            applications::list_filtered(&mut conn, &by("nonsense"))
                .await
                .unwrap()
        )
        .len(),
        4
    );

    let page = ApplicationFilter {
        sort: Some("rating".into()),
        limit: Some(2),
        ..ApplicationFilter::default()
    };
    assert_eq!(
        order(applications::list_filtered(&mut conn, &page).await.unwrap()),
        "CA"
    );
    let next = ApplicationFilter {
        cursor: Some(ids[0]),
        ..page.clone()
    };
    assert_eq!(
        order(applications::list_filtered(&mut conn, &next).await.unwrap()),
        "BD"
    );
    let bad = ApplicationFilter {
        cursor: Some(999),
        ..page
    };
    assert!(matches!(
        applications::list_filtered(&mut conn, &bad).await,
        Err(Error::ApplicationCursorInvalid)
    ));

    let filtered = ApplicationFilter {
        rating_gte: Some(2),
        ..ApplicationFilter::default()
    };
    assert_eq!(
        order(
            applications::list_filtered(&mut conn, &filtered)
                .await
                .unwrap()
        ),
        "A"
    );
    let outside_filter = ApplicationFilter {
        cursor: Some(ids[2]),
        ..filtered
    };
    assert!(matches!(
        applications::list_filtered(&mut conn, &outside_filter).await,
        Err(Error::ApplicationCursorInvalid)
    ));
}

#[tokio::test]
async fn search_is_case_insensitive_and_literal() {
    let (_dir, db) = open().await;
    let mut conn = db.conn().await.unwrap();
    new_app(&mut conn, "ACME Robotics").await;
    let notes = ApplicationFields {
        notes: Some(Some("100%_ remote".into())),
        status: Some("offer".into()),
        ..ApplicationFields::default()
    };
    applications::create(&mut conn, "Globex", "R", notes)
        .await
        .unwrap();

    let search = |term: &str, fields: Option<Vec<SearchField>>| ApplicationFilter {
        search: Some(term.to_owned()),
        search_fields: fields,
        ..ApplicationFilter::default()
    };
    let found = applications::list_filtered(&mut conn, &search("acme", None))
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    let found = applications::list_filtered(&mut conn, &search("%_", None))
        .await
        .unwrap();
    assert_eq!(found[0].company, "Globex");
    let found = applications::list_filtered(&mut conn, &search("e%", None))
        .await
        .unwrap();
    assert!(found.is_empty());
    let found =
        applications::list_filtered(&mut conn, &search("100", Some(vec![SearchField::Company])))
            .await
            .unwrap();
    assert!(found.is_empty());
    let by_status = ApplicationFilter {
        status: vec!["offer".into()],
        ..ApplicationFilter::default()
    };
    assert_eq!(
        applications::list_filtered(&mut conn, &by_status)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn pages_count_totals() {
    let (_dir, db) = open().await;
    let mut conn = db.conn().await.unwrap();
    for company in ["A", "B", "C"] {
        new_app(&mut conn, company).await;
    }
    let page = applications::list_page(&mut conn, Some(2), Some(2))
        .await
        .unwrap();
    assert_eq!(
        (page.total, page.page, page.page_size, page.total_pages),
        (3, 2, 2, 2)
    );
    assert_eq!(page.data.len(), 1);
    let page = applications::list_page(&mut conn, None, Some(1000))
        .await
        .unwrap();
    assert_eq!((page.page, page.page_size), (1, 100));
}

#[tokio::test]
async fn contacts_require_their_application() {
    let (_dir, db) = open().await;
    let mut conn = db.conn().await.unwrap();
    let app = new_app(&mut conn, "Acme").await;
    let other = new_app(&mut conn, "Globex").await;
    let new = NewContact {
        name: "Pat".into(),
        email: Some("p@example.test".into()),
        ..NewContact::default()
    };
    assert!(matches!(
        contacts::create(&mut conn, 999, &new).await,
        Err(Error::NotFound)
    ));
    let contact = contacts::create(&mut conn, app, &new).await.unwrap();

    let patch = ContactPatch {
        email: Some(None),
        phone: Some(Some("1".into())),
        ..ContactPatch::default()
    };
    assert!(matches!(
        contacts::update(&mut conn, contact.id, other, &patch).await,
        Err(Error::NotFound)
    ));
    let updated = contacts::update(&mut conn, contact.id, app, &patch)
        .await
        .unwrap();
    assert_eq!((updated.email, updated.phone.as_deref()), (None, Some("1")));

    assert_eq!(
        contacts::list_for_application(&mut conn, app)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        contacts::list_for_applications(&mut conn, &[app, other])
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        contacts::list_for_applications(&mut conn, &[])
            .await
            .unwrap()
            .is_empty()
    );

    assert!(matches!(
        contacts::delete(&mut conn, contact.id, other).await,
        Err(Error::NotFound)
    ));
    contacts::delete(&mut conn, contact.id, app).await.unwrap();
    assert!(matches!(
        contacts::delete(&mut conn, contact.id, app).await,
        Err(Error::NotFound)
    ));
}

#[tokio::test]
async fn event_listing_filters_and_pages_both_ways() {
    let (_dir, db) = open().await;
    let mut conn = db.conn().await.unwrap();
    let acme = new_app(&mut conn, "Acme").await;
    let globex = new_app(&mut conn, "Globex").await;
    let mut insert = async |application_id, kind: &str, occurred: &str, outcome: Option<&str>| {
        events::insert(
            &mut conn,
            &NewEvent {
                application_id,
                r#type: kind.to_owned(),
                occurred_at: Some(ts(occurred)),
                outcome: outcome.map(str::to_owned),
                ..NewEvent::default()
            },
        )
        .await
        .unwrap()
        .id
    };
    let first = insert(acme, "note", "2026-01-01T00:00:00Z", None).await;
    let tie_a = insert(acme, "interview", "2026-01-02T00:00:00Z", Some("passed")).await;
    let tie_b = insert(globex, "note", "2026-01-02T00:00:00Z", None).await;
    let last = insert(globex, "note", "2026-01-03T00:00:00Z", None).await;

    let page = |filter: EventFilter| EventFilter { limit: 2, ..filter };
    let newest = events::list_filtered(&mut conn, &page(EventFilter::default()))
        .await
        .unwrap();
    let ids: Vec<i64> = newest.items.iter().map(|event| event.id).collect();
    assert_eq!(ids, [last, tie_b]);
    assert!(newest.has_more);
    assert_eq!(
        newest.items[0].application.as_ref().unwrap().company,
        "Globex"
    );

    let tail = newest.items.last().unwrap();
    let next = EventFilter {
        cursor: Some((tail.occurred_at.clone(), tail.id)),
        ..EventFilter::default()
    };
    let older = events::list_filtered(&mut conn, &page(next)).await.unwrap();
    let ids: Vec<i64> = older.items.iter().map(|event| event.id).collect();
    assert_eq!(ids, [tie_a, first]);
    assert!(!older.has_more);

    let oldest = EventFilter {
        oldest_first: true,
        ..EventFilter::default()
    };
    let up = events::list_filtered(&mut conn, &page(oldest))
        .await
        .unwrap();
    assert_eq!(up.items[0].id, first);

    for (filter, expected) in [
        (
            EventFilter {
                company: Some("acm".into()),
                ..EventFilter::default()
            },
            2,
        ),
        (
            EventFilter {
                application_id: Some(globex),
                ..EventFilter::default()
            },
            2,
        ),
        (
            EventFilter {
                types: vec!["interview".into()],
                ..EventFilter::default()
            },
            1,
        ),
        (
            EventFilter {
                outcome: Some("passed".into()),
                ..EventFilter::default()
            },
            1,
        ),
        (
            EventFilter {
                occurred_after: Some(ts("2026-01-03T00:00:00Z")),
                ..EventFilter::default()
            },
            1,
        ),
        (
            EventFilter {
                occurred_before: Some(ts("2026-01-01T00:00:00Z")),
                ..EventFilter::default()
            },
            1,
        ),
    ] {
        let found = events::list_filtered(
            &mut conn,
            &EventFilter {
                limit: 50,
                ..filter
            },
        )
        .await
        .unwrap();
        assert_eq!(found.items.len(), expected);
    }

    let by_application = events::list_for_application(&mut conn, acme, None)
        .await
        .unwrap();
    assert_eq!(
        by_application
            .iter()
            .map(|event| event.id)
            .collect::<Vec<_>>(),
        [tie_a, first]
    );
}

#[tokio::test]
async fn submission_documents_are_immutable() {
    let (_dir, db) = open().await;
    let mut conn = db.conn().await.unwrap();
    let app = new_app(&mut conn, "Acme").await;
    let doc = new_doc(&mut conn, "cv", vec![app]).await;
    let submission = submissions::insert(&mut conn, &new_submission(app, "key"))
        .await
        .unwrap();
    let at = submission.submitted_at.clone();

    documents::claim_for_submission(&mut conn, &[doc], submission.id, &at)
        .await
        .unwrap();
    assert!(matches!(
        documents::claim_for_submission(&mut conn, &[doc], submission.id, &at).await,
        Err(Error::DocumentAlreadySubmitted)
    ));
    let claimed = documents::get(&mut conn, doc).await.unwrap().unwrap();
    assert_eq!(
        (claimed.state.as_str(), claimed.submitted_at.as_ref()),
        ("submitted", Some(&at))
    );
    assert_eq!(
        documents::list_for_submission(&mut conn, submission.id)
            .await
            .unwrap()
            .len(),
        1
    );

    let set_version = DocumentPatch {
        version: Some(9),
        ..DocumentPatch::default()
    };
    let set_state = |state: &str| DocumentPatch {
        state: Some(state.into()),
        ..DocumentPatch::default()
    };
    let combined = DocumentPatch {
        version: Some(9),
        ..set_state("submitted")
    };
    for patch in [
        &set_version,
        &DocumentPatch::default(),
        &set_state("current"),
        &combined,
    ] {
        assert!(matches!(
            documents::update_metadata(&mut conn, doc, patch).await,
            Err(Error::SubmittedDocumentImmutable)
        ));
    }
    assert!(
        documents::update_metadata(&mut conn, doc, &set_state("submitted"))
            .await
            .is_ok()
    );
    assert!(matches!(
        documents::set_links(&mut conn, doc, &[]).await,
        Err(Error::SubmittedDocumentImmutable)
    ));
    assert!(matches!(
        documents::rename(&mut conn, doc, "new").await,
        Err(Error::SubmittedDocumentImmutable)
    ));
    assert!(matches!(
        documents::delete(&mut conn, doc).await,
        Err(Error::SubmittedDocumentImmutable)
    ));
    let historical = documents::update_metadata(&mut conn, doc, &set_state("historical"))
        .await
        .unwrap();
    assert_eq!(historical.state, "historical");
}

#[tokio::test]
async fn ordinary_documents_are_mutable_with_guards() {
    let (_dir, db) = open().await;
    let mut conn = db.conn().await.unwrap();
    let app = new_app(&mut conn, "Acme").await;
    let doc = new_doc(&mut conn, "cv", vec![app, app]).await;

    let fresh = documents::get(&mut conn, doc).await.unwrap().unwrap();
    assert_eq!(fresh.applications.len(), 1);
    assert_eq!(fresh.applications[0].company, "Acme");
    assert_eq!(
        (
            fresh.document_type.as_str(),
            fresh.state.as_str(),
            fresh.version
        ),
        ("other", "current", 1)
    );
    assert_eq!(fresh.blob_state, documents::BlobState::Present);
    assert_eq!(fresh.blob_source, None);

    let reserved = DocumentPatch {
        state: Some("submitted".into()),
        ..DocumentPatch::default()
    };
    assert!(matches!(
        documents::update_metadata(&mut conn, doc, &reserved).await,
        Err(Error::SubmittedStateReserved)
    ));
    assert!(matches!(
        documents::update_metadata(&mut conn, 999, &DocumentPatch::default()).await,
        Err(Error::NotFound)
    ));
    let cleared = DocumentPatch {
        content_hash: Some(None),
        source: Some(Some("x".into())),
        ..DocumentPatch::default()
    };
    let updated = documents::update_metadata(&mut conn, doc, &cleared)
        .await
        .unwrap();
    assert_eq!(updated.content_hash, None);
    assert_eq!(updated.blob_sha256, fresh.blob_sha256);

    assert!(matches!(
        documents::set_links(&mut conn, doc, &[999]).await,
        Err(Error::InvalidApplications)
    ));
    let unlinked = documents::set_links(&mut conn, doc, &[]).await.unwrap();
    assert!(unlinked.applications.is_empty());

    let mut bad = NewDocument {
        state: Some("submitted".into()),
        ..NewDocument::default()
    };
    assert!(matches!(
        documents::create(&mut conn, &bad).await,
        Err(Error::SubmittedStateReserved)
    ));
    bad.state = None;
    bad.application_ids = vec![999];
    assert!(matches!(
        documents::create(&mut conn, &bad).await,
        Err(Error::InvalidApplications)
    ));

    assert_eq!(
        documents::rename(&mut conn, doc, "new")
            .await
            .unwrap()
            .unwrap()
            .original_name,
        "new"
    );
    assert!(
        documents::rename(&mut conn, 999, "new")
            .await
            .unwrap()
            .is_none()
    );

    let address = "a".repeat(64);
    let twin = new_doc(&mut conn, "twin", vec![]).await;
    documents::delete(&mut conn, doc).await.unwrap().unwrap();
    assert!(
        documents::blob_is_referenced(&mut conn, &address)
            .await
            .unwrap()
    );
    documents::delete(&mut conn, twin).await.unwrap().unwrap();
    assert!(
        !documents::blob_is_referenced(&mut conn, &address)
            .await
            .unwrap()
    );
    assert!(documents::delete(&mut conn, doc).await.unwrap().is_none());
}

#[tokio::test]
async fn document_listing_filters_and_pages() {
    let (_dir, db) = open().await;
    let mut conn = db.conn().await.unwrap();
    let app = new_app(&mut conn, "Acme").await;
    let linked = new_doc(&mut conn, "linked", vec![app]).await;
    let orphan = new_doc(&mut conn, "orphan", vec![]).await;
    let claimed = new_doc(&mut conn, "claimed", vec![app]).await;
    let submission = submissions::insert(&mut conn, &new_submission(app, "key"))
        .await
        .unwrap();
    documents::claim_for_submission(
        &mut conn,
        &[claimed],
        submission.id,
        &submission.submitted_at,
    )
    .await
    .unwrap();

    for (filter, expected) in [
        (
            DocumentFilter {
                application_id: Some(app),
                ..DocumentFilter::default()
            },
            vec![claimed, linked],
        ),
        (
            DocumentFilter {
                orphaned: Some(true),
                ..DocumentFilter::default()
            },
            vec![orphan],
        ),
        (
            DocumentFilter {
                orphaned: Some(false),
                ..DocumentFilter::default()
            },
            vec![claimed, linked],
        ),
        (
            DocumentFilter {
                state: Some("submitted".into()),
                ..DocumentFilter::default()
            },
            vec![claimed],
        ),
        (
            DocumentFilter {
                submission_id: Some(submission.id),
                ..DocumentFilter::default()
            },
            vec![claimed],
        ),
        (
            DocumentFilter {
                exclude_submission_artifacts: true,
                state: Some("submitted".into()),
                ..DocumentFilter::default()
            },
            vec![orphan, linked],
        ),
        (
            DocumentFilter {
                page_size: Some(1),
                page: Some(2),
                ..DocumentFilter::default()
            },
            vec![orphan],
        ),
        (
            DocumentFilter {
                limit: Some(1),
                ..DocumentFilter::default()
            },
            vec![claimed],
        ),
    ] {
        let mut found: Vec<i64> = documents::list_filtered(&mut conn, &filter)
            .await
            .unwrap()
            .into_iter()
            .map(|document| document.id)
            .collect();
        found.sort_unstable();
        let mut expected = expected;
        expected.sort_unstable();
        assert_eq!(found, expected, "{filter:?}");
    }
    assert_eq!(documents::list(&mut conn).await.unwrap().len(), 3);
    assert_eq!(
        documents::list_for_application(&mut conn, app)
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn submissions_hide_answers_unless_asked() {
    let (_dir, db) = open().await;
    let mut conn = db.conn().await.unwrap();
    let app = new_app(&mut conn, "Acme").await;
    let submission = submissions::insert(&mut conn, &new_submission(app, "key"))
        .await
        .unwrap();
    assert_eq!(
        submission.answers.0,
        json!([{"question": "q", "answer": "a"}])
    );

    let hidden = submissions::list_for_application(&mut conn, app, false)
        .await
        .unwrap();
    assert_eq!(hidden[0].answers.0, json!([]));
    let shown = submissions::list_for_application(&mut conn, app, true)
        .await
        .unwrap();
    assert_eq!(shown[0].answers, submission.answers);
    assert!(matches!(
        submissions::list_for_application(&mut conn, 999, false).await,
        Err(Error::NotFound)
    ));
    assert_eq!(
        submissions::count_for_application(&mut conn, app)
            .await
            .unwrap(),
        1
    );
    assert_eq!(submissions::list(&mut conn).await.unwrap().len(), 1);
    assert!(
        submissions::get(&mut conn, submission.id)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        submissions::find_by_idempotency_key(&mut conn, "key")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn cv_profile_is_a_single_row() {
    let (_dir, db) = open().await;
    let mut conn = db.conn().await.unwrap();
    assert!(cv::get_profile(&mut conn).await.unwrap().is_none());
    let input = cv::CvProfileInput {
        name: "First".into(),
        contact: json!({"email": "a@example.test"}),
        profile: "p".into(),
        skills: json!([]),
        experience: json!([]),
        projects: Some(json!([{"name": "x"}])),
        education: None,
    };
    let first = cv::upsert_profile(&mut conn, &input).await.unwrap();
    let second = cv::upsert_profile(
        &mut conn,
        &cv::CvProfileInput {
            name: "Second".into(),
            projects: None,
            ..input
        },
    )
    .await
    .unwrap();
    assert_eq!(first.id, second.id);
    assert_eq!(second.name, "Second");
    assert_eq!(second.projects.0, json!([]));
    assert_eq!(second.created_at, first.created_at);
    assert_eq!(
        scalar(&mut conn, "SELECT count(*) FROM cv_profile").await,
        1
    );
    let extra = sqlx::query(
        "INSERT INTO cv_profile (singleton, name, contact, profile, skills, experience, updated_at)
         VALUES (2, 'x', '{}', 'p', '[]', '[]', '2026-01-01T00:00:00.000Z')",
    )
    .execute(&mut *conn)
    .await;
    assert!(extra.is_err());
}

#[tokio::test]
async fn cv_patches_upsert_per_application() {
    let (_dir, db) = open().await;
    let mut conn = db.conn().await.unwrap();
    let app = new_app(&mut conn, "Acme").await;
    let doc = new_doc(&mut conn, "cv", vec![]).await;
    assert!(cv::get_patch(&mut conn, app).await.unwrap().is_none());
    assert!(matches!(
        cv::upsert_patch(&mut conn, 999, &cv::CvPatchInput::default()).await,
        Err(Error::NotFound)
    ));
    let input = cv::CvPatchInput {
        experience_ids: vec!["a".into()],
        ..cv::CvPatchInput::default()
    };
    let first = cv::upsert_patch(&mut conn, app, &input).await.unwrap();
    assert!(!first.include_projects);
    assert!(first.include_education);
    let again = cv::CvPatchInput {
        include_education: Some(false),
        ..input
    };
    let second = cv::upsert_patch(&mut conn, app, &again).await.unwrap();
    assert_eq!((first.id, second.include_education), (second.id, false));

    cv::set_patch_document_id(&mut conn, first.id, Some(doc))
        .await
        .unwrap();
    assert_eq!(
        cv::get_patch(&mut conn, app)
            .await
            .unwrap()
            .unwrap()
            .document_id,
        Some(doc)
    );
    assert!(matches!(
        cv::set_patch_document_id(&mut conn, first.id, Some(999)).await,
        Err(Error::NotFound)
    ));
    assert!(matches!(
        cv::set_patch_document_id(&mut conn, 999, None).await,
        Err(Error::NotFound)
    ));
}

#[tokio::test]
async fn api_tokens_are_one_at_a_time() {
    let (_dir, db) = open().await;
    let mut conn = db.conn().await.unwrap();
    assert!(api_tokens::current(&mut conn).await.unwrap().is_none());
    let first = api_tokens::replace(&mut conn, "hash-1", "default")
        .await
        .unwrap();
    let second = api_tokens::replace(&mut conn, "hash-2", "cli")
        .await
        .unwrap();
    assert_ne!(first.id, second.id);
    assert!(
        api_tokens::find_by_hash(&mut conn, "hash-1")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        api_tokens::current(&mut conn).await.unwrap().unwrap().name,
        "cli"
    );
    api_tokens::touch_last_used(&mut conn, second.id)
        .await
        .unwrap();
    assert!(
        api_tokens::find_by_hash(&mut conn, "hash-2")
            .await
            .unwrap()
            .unwrap()
            .last_used_at
            .is_some()
    );
    api_tokens::delete_all(&mut conn).await.unwrap();
    assert!(api_tokens::current(&mut conn).await.unwrap().is_none());
}

#[tokio::test]
async fn oauth_codes_are_single_use_and_tokens_store_json_scopes() {
    let (_dir, db) = open().await;
    let mut conn = db.conn().await.unwrap();
    let client = oauth::insert_client(
        &mut conn,
        &oauth::NewOAuthClient {
            client_id: "mcp_1".into(),
            client_secret_hash: None,
            client_name: Some("Claude".into()),
            redirect_uris: vec!["https://client.example.test/cb".into()],
            grant_types: vec!["authorization_code".into()],
            response_types: vec!["code".into()],
            token_endpoint_auth: "none".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(client.redirect_uris.0, ["https://client.example.test/cb"]);
    assert!(
        oauth::find_client(&mut conn, "mcp_1")
            .await
            .unwrap()
            .is_some()
    );

    let now = Timestamp::now();
    let code = oauth::NewAuthCode {
        code: "code-1".into(),
        client_id: "mcp_1".into(),
        redirect_uri: "https://client.example.test/cb".into(),
        code_challenge: "challenge".into(),
        scopes: vec!["mcp:tools".into()],
        sensitive_consent_version: 1,
        expires_at: now.plus(Duration::minutes(10)),
    };
    oauth::insert_auth_code(&mut conn, &code).await.unwrap();
    let uri = &code.redirect_uri;
    assert!(
        oauth::consume_auth_code(&mut conn, "code-1", "other", uri, &now)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        oauth::consume_auth_code(
            &mut conn,
            "code-1",
            "mcp_1",
            "https://evil.example.test",
            &now
        )
        .await
        .unwrap()
        .is_none()
    );
    let late = now.plus(Duration::minutes(11));
    assert!(
        oauth::consume_auth_code(&mut conn, "code-1", "mcp_1", uri, &late)
            .await
            .unwrap()
            .is_none()
    );
    let used = oauth::consume_auth_code(&mut conn, "code-1", "mcp_1", uri, &now)
        .await
        .unwrap()
        .unwrap();
    assert!(used.used);
    assert_eq!(used.scopes.0, ["mcp:tools"]);
    assert!(
        oauth::consume_auth_code(&mut conn, "code-1", "mcp_1", uri, &now)
            .await
            .unwrap()
            .is_none()
    );

    let token = oauth::NewOAuthToken {
        token_hash: "hash".into(),
        client_id: "mcp_1".into(),
        scopes: vec!["mcp:tools".into(), "mcp:submissions".into()],
        sensitive_consent_version: 1,
        expires_at: now.plus(Duration::days(1)),
    };
    oauth::insert_access_token(&mut conn, &token).await.unwrap();
    oauth::insert_refresh_token(&mut conn, &token)
        .await
        .unwrap();
    let access = oauth::find_access_token(&mut conn, "hash")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(access.scopes.0.len(), 2);
    let refresh = oauth::find_refresh_token(&mut conn, "hash")
        .await
        .unwrap()
        .unwrap();
    assert!(
        oauth::delete_refresh_token(&mut conn, refresh.id)
            .await
            .unwrap()
    );
    assert!(
        !oauth::delete_refresh_token(&mut conn, refresh.id)
            .await
            .unwrap()
    );
    assert!(
        oauth::find_refresh_token(&mut conn, "hash")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn records_serialize_in_old_nexus_json_shape() {
    let (_dir, db) = open().await;
    let mut conn = db.conn().await.unwrap();
    let fields = ApplicationFields {
        job_url: Some(Some("https://jobs.example.test/1".into())),
        eligible_countries: Some(sqlx::types::Json(vec!["DE".into()])),
        ..ApplicationFields::default()
    };
    let app = applications::create(&mut conn, "Acme", "Engineer", fields)
        .await
        .unwrap();
    let json = serde_json::to_value(&app).unwrap();
    assert_eq!(json["id"], json!(app.id.to_string()));
    assert_eq!(json["jobUrl"], json!("https://jobs.example.test/1"));
    assert_eq!(json["eligibleCountries"], json!(["DE"]));
    assert_eq!(json["primaryLocations"], json!([]));
    assert_eq!(json["createdAt"].as_str().unwrap().len(), 24);

    let contact = contacts::create(
        &mut conn,
        app.id,
        &NewContact {
            name: "Pat".into(),
            linked_in: Some("l".into()),
            ..NewContact::default()
        },
    )
    .await
    .unwrap();
    let json = serde_json::to_value(&contact).unwrap();
    assert_eq!(json["linkedIn"], json!("l"));
    assert_eq!(json["applicationId"], json!(app.id.to_string()));

    let event = events::insert(
        &mut conn,
        &NewEvent {
            application_id: app.id,
            r#type: "note".into(),
            metadata: Some(json!({"zeta": 1, "requestHash": "legacy", "alpha": 2})),
            ..NewEvent::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(event.persisted_request_hash(), Some("legacy"));
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["type"], json!("note"));
    assert_eq!(
        serde_json::to_string(&json["metadata"]).unwrap(),
        r#"{"zeta":1,"alpha":2}"#
    );
    assert!(json.get("requestHash").is_none());

    let doc = new_doc(&mut conn, "cv", vec![app.id]).await;
    let json =
        serde_json::to_value(documents::get(&mut conn, doc).await.unwrap().unwrap()).unwrap();
    assert_eq!(json["submissionId"], json!(null));
    assert_eq!(json["applications"][0]["id"], json!(app.id.to_string()));
    assert!(json.get("blobSha256").is_none());
}
