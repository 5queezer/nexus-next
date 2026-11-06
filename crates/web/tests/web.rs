//! The three routers against a real SQLite file with synthetic data.

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use nexus_core::repo::applications::{self, ApplicationFields};
use nexus_core::repo::contacts::{self, NewContact};
use nexus_core::repo::documents::{self, NewDocument};
use nexus_core::repo::events::{self, NewEvent};
use nexus_core::repo::submissions::{self, NewSubmission};
use nexus_core::{Db, Timestamp};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

fn percent(value: &str) -> String {
    value.bytes().map(|b| format!("%{b:02X}")).collect()
}

const TOKEN: &str = "s3cret-token-value";
const EVIL: &str = "<script>alert(1)</script>";

struct Fixture {
    _dir: TempDir,
    db: Db,
    app: Router,
}

async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).await.unwrap();
    let app = nexus_web::dashboard_router(db.clone(), "Europe/Madrid".parse().unwrap())
        .merge(nexus_web::api_router(db.clone()))
        .merge(nexus_web::calendar_router(db.clone(), TOKEN.to_owned()));
    Fixture { _dir: dir, db, app }
}

fn ts(value: &str) -> Timestamp {
    Timestamp::parse(value).unwrap()
}

async fn add_app(
    db: &Db,
    company: &str,
    status: &str,
    edit: impl FnOnce(&mut ApplicationFields),
) -> i64 {
    let mut fields = ApplicationFields {
        status: Some(status.to_owned()),
        ..Default::default()
    };
    edit(&mut fields);
    applications::create(&mut db.conn().await.unwrap(), company, "Engineer", fields)
        .await
        .unwrap()
        .id
}

async fn add_event(db: &Db, id: i64, kind: &str, occurred: &str, metadata: Value) -> i64 {
    let event = NewEvent {
        application_id: id,
        r#type: kind.to_owned(),
        occurred_at: Some(ts(occurred)),
        actor: Some("owner".to_owned()),
        source: Some("mcp".to_owned()),
        metadata: Some(metadata),
        ..Default::default()
    };
    events::insert(&mut db.conn().await.unwrap(), &event)
        .await
        .unwrap()
        .id
}

async fn fetch(app: &Router, uri: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
    let response = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes().to_vec();
    (parts.status, parts.headers, bytes)
}

async fn html(app: &Router, uri: &str) -> String {
    let (status, _, body) = fetch(app, uri).await;
    assert_eq!(status, StatusCode::OK, "{uri}");
    String::from_utf8(body).unwrap()
}

async fn json_of(app: &Router, uri: &str) -> Value {
    let (status, _, body) = fetch(app, uri).await;
    assert_eq!(status, StatusCode::OK, "{uri}");
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn pipeline_escapes_text_and_sets_security_headers() {
    let f = fixture().await;
    let id = add_app(&f.db, EVIL, "applied", |_| {}).await;
    let (status, headers, body) = fetch(&f.app, "/").await;
    let body = String::from_utf8(body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("<script"), "{body}");
    assert!(!body.contains(" style="), "the CSP blocks inline styles");
    assert!(body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(body.contains(&format!("#{id}")));
    assert_eq!(headers["x-content-type-options"], "nosniff");
    let csp = headers["content-security-policy"].to_str().unwrap();
    assert!(csp.contains("default-src 'none'") && !csp.contains("unsafe-inline"));
    assert!(
        headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    let css = fetch(&f.app, "/assets/app.css").await;
    assert_eq!(css.0, StatusCode::OK);
    assert!(
        css.1["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/css")
    );
}

#[tokio::test]
async fn pipeline_filters_by_status_archive_and_search() {
    let f = fixture().await;
    add_app(&f.db, "Alpha Labs", "applied", |_| {}).await;
    add_app(&f.db, "Beta Works", "interview", |_| {}).await;
    add_app(&f.db, "Gamma Archived", "applied", |fields| {
        fields.archived_at = Some(Some(ts("2026-01-01T00:00:00Z")));
    })
    .await;

    let all = html(&f.app, "/").await;
    assert!(all.contains("Alpha Labs") && all.contains("Beta Works"));
    assert!(!all.contains("Gamma Archived"));
    let interview = html(&f.app, "/?status=interview").await;
    assert!(interview.contains("Beta Works") && !interview.contains("Alpha Labs"));
    let archived = html(&f.app, "/?archived=only").await;
    assert!(archived.contains("Gamma Archived") && !archived.contains("Alpha Labs"));
    assert!(archived.contains("(archived)"));
    assert!(
        html(&f.app, "/?archived=include")
            .await
            .contains("Gamma Archived")
    );
    let found = html(&f.app, "/?q=beta").await;
    assert!(found.contains("Beta Works") && !found.contains("Alpha Labs"));
}

#[tokio::test]
async fn search_folds_case_beyond_ascii_in_dashboard_and_api() {
    let f = fixture().await;
    add_app(&f.db, "M\u{fc}nchen Werke", "applied", |_| {}).await;
    add_app(&f.db, "Se\u{f1}or Pesca", "applied", |fields| {
        fields.notes = Some(Some("K\u{f6}ln office".to_owned()));
    })
    .await;
    for (term, hit, miss) in [
        ("m\u{dc}nchen", "M\u{fc}nchen Werke", "Pesca"),
        ("SE\u{d1}OR", "Se\u{f1}or Pesca", "Werke"),
        ("k\u{d6}ln", "Pesca", "Werke"),
    ] {
        let encoded = percent(term);
        let page = html(&f.app, &format!("/?q={encoded}")).await;
        assert!(page.contains(hit) && !page.contains(miss), "{term}");
        let api = json_of(&f.app, &format!("/api/applications?search={encoded}")).await;
        assert_eq!(api["items"].as_array().unwrap().len(), 1, "{term}");
        assert!(api.to_string().contains(hit), "{term}");
    }
}

#[tokio::test]
async fn overdue_follow_ups_stand_out_in_pipeline_and_queue() {
    let f = fixture().await;
    add_app(&f.db, "Late Co", "applied", |fields| {
        fields.follow_up_at = Some(Some(ts("2020-01-01T00:00:00Z")));
    })
    .await;
    add_app(&f.db, "Future Co", "applied", |fields| {
        fields.follow_up_at = Some(Some(ts("2099-01-01T00:00:00Z")));
    })
    .await;
    add_app(&f.db, "Held Co", "applied", |fields| {
        fields.follow_up_at = Some(Some(ts("2020-01-01T00:00:00Z")));
        fields.next_action = Some(Some("Do not follow up until they reply".to_owned()));
    })
    .await;
    add_app(&f.db, "Lead Co", "inbound", |_| {}).await;
    add_app(&f.db, "Done Co", "rejected", |_| {}).await;

    let pipeline = html(&f.app, "/").await;
    assert_eq!(pipeline.matches("row overdue").count(), 1);
    assert_eq!(pipeline.matches("Overdue follow-up").count(), 1);
    let queue = html(&f.app, "/queue").await;
    let position = |text: &str| queue.find(text).unwrap_or_else(|| panic!("{text} missing"));
    assert!(position("Overdue (1)") < position("Late Co"));
    assert!(position("Late Co") < position("Due soon (1)"));
    assert!(position("Future Co") < position("Waiting (1)"));
    assert!(position("Held Co") < position("New leads (1)"));
    assert!(position("Lead Co") < position("Completed (1)"));
}

#[tokio::test]
async fn detail_shows_everything_and_marks_missing_files() {
    let f = fixture().await;
    let id = add_app(&f.db, EVIL, "interview", |fields| {
        fields.notes = Some(Some("Note with <b>markup</b>".to_owned()));
        fields.job_description = Some(Some("Build things".to_owned()));
        fields.job_url = Some(Some("javascript:alert(1)".to_owned()));
        fields.rating = Some(Some(4));
    })
    .await;
    let mut conn = f.db.conn().await.unwrap();
    let contact = contacts::create(
        &mut conn,
        id,
        &NewContact {
            name: "Rita Recruiter".to_owned(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    drop(conn);
    let event = NewEvent {
        application_id: id,
        r#type: "interview_scheduled".to_owned(),
        occurred_at: Some(ts("2026-03-01T09:00:00Z")),
        contact_id: Some(contact.id.to_string()),
        metadata: Some(
            json!({"interviewType": "technical", "scheduledAt": "2026-03-05T14:30:00.000Z"}),
        ),
        ..Default::default()
    };
    events::insert(&mut f.db.conn().await.unwrap(), &event)
        .await
        .unwrap();

    let sha = f.db.blobs().put(b"%PDF-1.4 fake").unwrap();
    let mut tx = f.db.begin_write().await.unwrap();
    let present = documents::create(
        &mut tx,
        &NewDocument {
            filename: "cv.pdf".into(),
            original_name: "My CV.pdf".into(),
            size: 13,
            mime_type: "application/pdf".into(),
            application_ids: vec![id],
            blob_sha256: sha,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let lost = documents::create(
        &mut tx,
        &NewDocument {
            filename: "lost.pdf".into(),
            original_name: "Lost letter.pdf".into(),
            size: 5,
            mime_type: "application/pdf".into(),
            application_ids: vec![id],
            blob_sha256: "c".repeat(64),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    sqlx::query("UPDATE documents SET blob_state = 'missing', blob_sha256 = NULL WHERE id = ?")
        .bind(lost.id)
        .execute(f.db.pool())
        .await
        .unwrap();
    submissions::insert(
        &mut f.db.conn().await.unwrap(),
        &NewSubmission {
            application_id: id,
            idempotency_key: "key-1".into(),
            request_hash: "b".repeat(64),
            submitted_at: ts("2026-03-02T10:00:00Z"),
            application_url: None,
            ats_name: Some("Greenhouse".into()),
            requisition_id: None,
            language: None,
            answers: json!([{"question": "Why us?", "answer": "Because <i>reasons</i>", "sensitive": true}]),
            policy: json!({"humanReviewed": true}),
            candidate_salary_min: Some(90000),
            candidate_salary_max: Some(100000),
            candidate_salary_currency: Some("EUR".into()),
            candidate_salary_period: Some("year".into()),
            candidate_salary_type: None,
            candidate_salary_flexible: true,
            document_ids: vec![],
        },
    )
    .await
    .unwrap();

    let page = html(&f.app, &format!("/applications/{id}")).await;
    assert!(
        !page.contains("<script") && !page.contains("<b>markup") && !page.contains("<i>reasons")
    );
    assert!(page.contains(&format!("#{id}")));
    assert!(
        page.contains("Interview scheduled")
            && page.contains("Scheduled for: Mar 05, 2026, 15:30 CET")
    );
    assert!(page.contains("Contact: Rita Recruiter"));
    assert!(page.contains("Why us?") && page.contains("90,000 to 100,000 EUR / year, flexible"));
    assert!(page.contains(&format!("/api/documents/{}/download", present.id)));
    assert!(page.contains("file missing"));
    assert!(!page.contains(&format!("/api/documents/{}/download", lost.id)));
    assert!(!page.contains("href=\"javascript:"));

    assert_eq!(
        fetch(&f.app, "/applications/999").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        fetch(&f.app, "/applications/abc").await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn api_reads_and_downloads() {
    let f = fixture().await;
    let id = add_app(&f.db, "Api Co", "applied", |_| {}).await;
    add_event(
        &f.db,
        id,
        "note_added",
        "2026-03-01T09:00:00Z",
        json!({"note": "hi"}),
    )
    .await;
    let sha = f.db.blobs().put(b"hello").unwrap();
    let mut tx = f.db.begin_write().await.unwrap();
    let doc = documents::create(
        &mut tx,
        &NewDocument {
            filename: "a.txt".into(),
            original_name: "na\"me\u{e9}.txt".into(),
            size: 5,
            mime_type: "text/plain".into(),
            application_ids: vec![id],
            blob_sha256: sha,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let list = json_of(&f.app, "/api/applications").await;
    assert_eq!(list["items"][0]["company"], "Api Co");
    assert_eq!(list["items"][0]["id"], id.to_string());
    assert_eq!(
        json_of(&f.app, &format!("/api/applications/{id}")).await["company"],
        "Api Co"
    );
    let events = json_of(&f.app, &format!("/api/applications/{id}/events")).await;
    assert_eq!(events["items"][0]["type"], "note_added");
    assert_eq!(
        json_of(&f.app, &format!("/api/applications/{id}/contacts")).await["items"],
        json!([])
    );
    let docs = json_of(&f.app, &format!("/api/applications/{id}/documents")).await;
    assert_eq!(docs["items"][0]["blobState"], "present");
    assert!(docs["items"][0].get("blobSha256").is_none());
    assert_eq!(
        json_of(&f.app, &format!("/api/documents/{}", doc.id)).await["originalName"],
        "na\"me\u{e9}.txt"
    );

    let (status, headers, body) =
        fetch(&f.app, &format!("/api/documents/{}/download", doc.id)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"hello");
    assert_eq!(headers["content-type"], "text/plain");
    let disposition = headers["content-disposition"].to_str().unwrap();
    assert!(
        disposition
            .starts_with("attachment; filename=\"na_me_.txt\"; filename*=UTF-8''na%22me%C3%A9.txt")
    );
    assert!(
        headers["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("sandbox")
    );

    sqlx::query("UPDATE documents SET blob_state = 'missing', blob_sha256 = NULL WHERE id = ?")
        .bind(doc.id)
        .execute(f.db.pool())
        .await
        .unwrap();
    let (status, _, body) = fetch(&f.app, &format!("/api/documents/{}/download", doc.id)).await;
    assert_eq!(status, StatusCode::GONE);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["error"],
        "file_missing"
    );

    for uri in [
        "/api/applications/999",
        "/api/applications/999/events",
        "/api/documents/999",
        "/api/applications/x",
    ] {
        assert_eq!(fetch(&f.app, uri).await.0, StatusCode::NOT_FOUND, "{uri}");
    }
}

fn parse_calendar(body: &str) -> Vec<ical::parser::ical::component::IcalEvent> {
    let mut parser = ical::IcalParser::new(body.as_bytes());
    let calendar = parser.next().expect("one calendar").expect("parses");
    assert!(parser.next().is_none());
    calendar.events
}

fn property(event: &ical::parser::ical::component::IcalEvent, name: &str) -> Option<String> {
    event
        .properties
        .iter()
        .find(|p| p.name == name)
        .and_then(|p| p.value.clone())
}

async fn calendar(app: &Router) -> String {
    html(app, &format!("/calendar.ics?token={TOKEN}")).await
}

#[tokio::test]
async fn calendar_rejects_bad_tokens_without_a_body() {
    let f = fixture().await;
    add_app(&f.db, "Secret Co", "applied", |fields| {
        fields.follow_up_at = Some(Some(ts("2099-01-01T00:00:00Z")));
    })
    .await;
    for uri in [
        "/calendar.ics",
        "/calendar.ics?token=",
        "/calendar.ics?token=wrong",
        &format!("/calendar.ics?token={}x", TOKEN),
        &format!("/calendar.ics?token={}", &TOKEN[..5]),
    ] {
        let (status, _, body) = fetch(&f.app, uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert!(body.is_empty(), "{uri}");
    }
    let disabled = nexus_web::calendar_router(f.db.clone(), String::new());
    assert_eq!(
        fetch(&disabled, "/calendar.ics?token=").await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn calendar_lists_interviews_and_follow_ups_per_rfc_5545() {
    let f = fixture().await;
    let long_company = format!(
        "Fold, Inc; {} {}",
        "long name".repeat(12),
        "\u{e9}".repeat(40)
    );
    let id = add_app(&f.db, &long_company, "interview", |fields| {
        fields.follow_up_at = Some(Some(ts("2099-02-03T00:00:00Z")));
        fields.next_action = Some(Some("Call back\nsecond line".to_owned()));
    })
    .await;
    add_event(
        &f.db,
        id,
        "interview_scheduled",
        "2099-01-01T09:00:00Z",
        json!({"interviewType": "technical", "scheduledAt": "2099-01-10T13:00:00.000Z", "durationMinutes": 45}),
    )
    .await;
    let timed = add_app(&f.db, "Timed Co", "applied", |fields| {
        fields.follow_up_at = Some(Some(ts("2099-03-04T08:30:00Z")));
    })
    .await;
    add_app(&f.db, "Archived Co", "applied", |fields| {
        fields.follow_up_at = Some(Some(ts("2099-03-04T00:00:00Z")));
        fields.archived_at = Some(Some(ts("2026-01-01T00:00:00Z")));
    })
    .await;
    let rejected = add_app(&f.db, "Rejected Co", "rejected", |fields| {
        fields.follow_up_at = Some(Some(ts("2099-03-04T00:00:00Z")));
    })
    .await;
    add_event(
        &f.db,
        rejected,
        "interview_scheduled",
        "2099-01-01T09:00:00Z",
        json!({"interviewType": "final", "scheduledAt": "2099-01-12T13:00:00.000Z"}),
    )
    .await;
    add_app(&f.db, "Held Co", "applied", |fields| {
        fields.follow_up_at = Some(Some(ts("2099-03-04T00:00:00Z")));
        fields.next_action = Some(Some("Wait for recruiter feedback".to_owned()));
    })
    .await;

    let (status, headers, bytes) = fetch(&f.app, &format!("/calendar.ics?token={TOKEN}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "text/calendar; charset=utf-8");
    assert!(
        headers["cache-control"]
            .to_str()
            .unwrap()
            .starts_with("private")
    );
    let body = String::from_utf8(bytes).unwrap();

    for line in body.split("\r\n") {
        assert!(line.len() <= 75, "line over 75 octets: {line}");
    }
    assert!(body.ends_with("END:VCALENDAR\r\n"));
    assert!(!body.replace("\r\n", "").contains('\n') && !body.replace("\r\n", "").contains('\r'));

    let events = parse_calendar(&body);
    assert_eq!(
        events.len(),
        3,
        "interview, dated follow-up, timed follow-up:\n{body}"
    );
    let interview = events
        .iter()
        .find(|e| property(e, "UID").unwrap().starts_with("interview-"))
        .unwrap();
    assert_eq!(property(interview, "DTSTART").unwrap(), "20990110T130000Z");
    assert_eq!(property(interview, "DTEND").unwrap(), "20990110T134500Z");
    assert!(property(interview, "DTSTAMP").unwrap().ends_with('Z'));
    let summary = property(interview, "SUMMARY").unwrap();
    assert!(
        summary.starts_with("Interview (technical): Fold\\, Inc\\; "),
        "{summary}"
    );
    assert!(
        summary.ends_with(&"\u{e9}".repeat(40)),
        "folding must not split characters"
    );
    assert!(
        property(interview, "DESCRIPTION")
            .unwrap()
            .contains("Engineer (#")
    );

    let dated = events
        .iter()
        .find(|e| property(e, "UID").unwrap() == format!("followup-{id}@nexus"))
        .unwrap();
    let start = dated
        .properties
        .iter()
        .find(|p| p.name == "DTSTART")
        .unwrap();
    assert_eq!(start.value.as_deref(), Some("20990203"));
    assert_eq!(start.params.as_ref().unwrap()[0].0, "VALUE");
    let timed_event = events
        .iter()
        .find(|e| property(e, "UID").unwrap() == format!("followup-{timed}@nexus"))
        .unwrap();
    assert_eq!(
        property(timed_event, "DTSTART").unwrap(),
        "20990304T083000Z"
    );

    assert_eq!(
        calendar(&f.app).await,
        body,
        "same data gives the same feed"
    );
}

#[tokio::test]
async fn rescheduled_interview_keeps_its_uid_and_later_rounds_get_new_ones() {
    let f = fixture().await;
    let id = add_app(&f.db, "Round Co", "interview", |_| {}).await;
    let first = add_event(
        &f.db,
        id,
        "interview_scheduled",
        "2099-01-01T09:00:00Z",
        json!({"interviewType": "technical", "scheduledAt": "2099-01-10T13:00:00.000Z"}),
    )
    .await;
    let before = parse_calendar(&calendar(&f.app).await);
    assert_eq!(before.len(), 1);

    add_event(
        &f.db,
        id,
        "interview_scheduled",
        "2099-01-05T09:00:00Z",
        json!({"interviewType": "technical", "scheduledAt": "2099-01-12T15:00:00.000Z"}),
    )
    .await;
    let after = parse_calendar(&calendar(&f.app).await);
    assert_eq!(after.len(), 1, "the reschedule replaces the original");
    assert_eq!(
        property(&after[0], "UID"),
        Some(format!("interview-{id}-{first}@nexus"))
    );
    assert_eq!(property(&before[0], "UID"), property(&after[0], "UID"));
    assert_eq!(property(&after[0], "DTSTART").unwrap(), "20990112T150000Z");
    assert_eq!(property(&after[0], "SEQUENCE").unwrap(), "1");

    let round_two = add_event(
        &f.db,
        id,
        "interview_scheduled",
        "2099-01-13T09:00:00Z",
        json!({"interviewType": "technical", "scheduledAt": "2099-01-20T09:00:00.000Z"}),
    )
    .await;
    let rounds = parse_calendar(&calendar(&f.app).await);
    assert_eq!(rounds.len(), 2, "the first interview had already happened");
    assert!(
        rounds
            .iter()
            .any(|e| property(e, "UID") == Some(format!("interview-{id}-{round_two}@nexus")))
    );
}

// ── Strange stored data ──────────────────────────────────────────────────────

async fn raw(db: &Db, sql: &str) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_owned()))
        .execute(db.pool())
        .await
        .unwrap();
}

/// Every value here passes the columns' checks, which only look at a timestamp's shape and whether
/// JSON parses. Agents can't store most of them, but the pages must not depend on that.
const NOT_A_DATE: &str = "2026-99-99T99:99:99.999Z";

#[tokio::test]
async fn poisoned_rows_cost_at_most_themselves_on_every_page() {
    let f = fixture().await;
    let good = add_app(&f.db, "Good Co", "applied", |fields| {
        fields.follow_up_at = Some(Some(ts("2030-01-02T10:00:00Z")));
    })
    .await;
    add_event(
        &f.db,
        good,
        "interview_scheduled",
        "2026-01-01T10:00:00Z",
        json!({"scheduledAt": "2030-02-01T10:00:00Z", "interviewType": "good"}),
    )
    .await;

    raw(
        &f.db,
        &format!(
            r#"INSERT INTO applications (id, company, role, status, current_stage, applied_at,
                   last_contact, follow_up_at, archived_at, job_posted_at, created_at, updated_at,
                   salary_min, salary_max, rating, triage_quality, event_version,
                   eligible_countries, primary_locations, job_url)
               VALUES
                 (900, 'Bad Dates', 'r', 'interview', 'x', '{d}', '{d}', '{d}', NULL, '{d}', '{d}', '{d}',
                  -9223372036854775808, 9223372036854775807, -9223372036854775808,
                  -9223372036854775808, 9223372036854775807, '{{"a":1}}', '5', 'javascript:x'),
                 (901, 'Min Quality', 'r', 'not-a-status', '', NULL, NULL, '2030-01-02T10:00:00.000Z',
                  NULL, NULL, '0000-01-01T00:00:00.000Z', '9999-12-31T23:59:59.999Z',
                  NULL, NULL, NULL, -9223372036854775808, 0, '[[[]]]', 'null', ''),
                 (902, 'Max Quality', 'r', '', '', NULL, NULL, '2030-01-02T10:00:00.000Z',
                  NULL, NULL, '9999-12-31T23:59:59.999Z', '0000-01-01T00:00:00.000Z',
                  NULL, NULL, NULL, 9223372036854775807, 0, NULL, NULL, NULL);"#,
            d = NOT_A_DATE
        ),
    )
    .await;
    let metadata = [
        r#"{"scheduledAt":"2030-03-01T10:00:00Z","interviewType":"huge","durationMinutes":1000000000000}"#,
        r#"{"scheduledAt":"2030-03-02T10:00:00Z","interviewType":"max","durationMinutes":9223372036854775807}"#,
        r#"{"scheduledAt":"2030-03-03T10:00:00Z","interviewType":"min","durationMinutes":-9223372036854775808}"#,
        r#"{"scheduledAt":"2030-03-04T10:00:00Z","interviewType":"float","durationMinutes":1.5e300}"#,
        r#"{"scheduledAt":"2030-03-05T10:00:00Z","interviewType":"text","durationMinutes":"long"}"#,
        r#"{"scheduledAt":"9999-12-31T23:59:59.000Z","interviewType":"last","durationMinutes":1440}"#,
        r#"{"scheduledAt":"0000-01-01T00:00:00.000Z","interviewType":"first"}"#,
        r#"{"scheduledAt":"2026-99-99T99:99:99.999Z","interviewType":"bad"}"#,
        r#"{"scheduledAt":12345,"interviewType":{"a":1},"note":[1,2],"followUpAt":null}"#,
        "[1,2,3]",
        "5",
        "null",
        r#"{"overflow":1e999}"#,
    ];
    for (i, metadata) in metadata.iter().enumerate() {
        raw(
            &f.db,
            &format!(
                "INSERT INTO application_events (application_id, type, occurred_at, created_at, metadata)
                 VALUES (900, 'interview_scheduled', '{d}', '{d}', '{m}');
                 INSERT INTO application_events (application_id, type, occurred_at, metadata)
                 VALUES (900, 'x{i}', '2026-01-01T10:00:00.000Z', '{m}');",
                d = NOT_A_DATE,
                m = metadata.replace('\'', "''"),
            ),
        )
        .await;
    }
    let deep = format!("{}1{}", "[".repeat(300), "]".repeat(300));
    raw(
        &f.db,
        &format!(
            r#"INSERT INTO application_events (application_id, type, occurred_at, metadata)
                 VALUES (900, 'deep', '2026-01-01T10:00:00.000Z', '{deep}');
               INSERT INTO contacts (application_id, name, created_at)
                 VALUES (900, 'Contact', '{d}');
               INSERT INTO documents (filename, original_name, size, mime_type, uploaded_at, version)
                 VALUES ('f', 'f', -9223372036854775808, '', '{d}', -1);
               INSERT INTO application_documents (application_id, document_id)
                 VALUES (900, last_insert_rowid());
               INSERT INTO application_submissions (application_id, idempotency_key, request_hash,
                     submitted_at, answers, policy, document_ids)
                 VALUES (900, 'k1', 'h', '{d}', '5', '[]', '"x"'),
                        (900, 'k2', 'h', '2026-01-01T10:00:00.000Z', '{{"a":1e999}}', '{{}}', '[]'),
                        (900, 'k3', 'h', '2026-01-01T10:00:00.000Z', '[{{"question":5,"answer":null}}, 7]', 'null', '{{}}');"#,
            d = NOT_A_DATE,
        ),
    )
    .await;

    for path in [
        "/",
        "/?archived=all",
        "/queue",
        "/applications/900",
        "/applications/901",
        "/applications/902",
        "/api/applications",
        "/api/applications/900",
        "/api/applications/900/events",
        "/api/applications/900/contacts",
        "/api/applications/900/documents",
        "/api/applications/900/submissions",
    ] {
        let (status, _, _) = fetch(&f.app, path).await;
        assert_eq!(status, StatusCode::OK, "{path}");
    }

    let pipeline = html(&f.app, "/?archived=all").await;
    assert!(pipeline.contains("Good Co") && pipeline.contains("Bad Dates"));
    let queue = html(&f.app, "/queue").await;
    assert!(queue.contains("Good Co") && queue.contains("Min Quality"));
    assert!(
        queue.find("Max Quality") < queue.find("Min Quality"),
        "higher triage quality comes first, even at the limits"
    );

    let (status, _, bytes) = fetch(&f.app, &format!("/calendar.ics?token={TOKEN}")).await;
    assert_eq!(status, StatusCode::OK);
    let body = String::from_utf8(bytes).unwrap();
    let events = parse_calendar(&body);
    let summaries: Vec<String> = events
        .iter()
        .map(|event| {
            event
                .properties
                .iter()
                .find(|p| p.name == "SUMMARY")
                .and_then(|p| p.value.clone())
                .unwrap()
        })
        .collect();
    assert!(
        summaries.iter().any(|s| s.contains("good")),
        "{summaries:?}"
    );
    assert!(
        summaries.iter().any(|s| s.contains("huge")),
        "{summaries:?}"
    );
    for line in body.lines().filter(|l| l.starts_with("DTEND")) {
        assert_eq!(line.trim().len(), "DTEND:20300301T110000Z".len(), "{line}");
    }
    assert!(body.contains("DTSTART:20300301T100000Z\r\nDTEND:20300301T110000Z"));
    assert!(body.contains("DTSTART:20300302T100000Z\r\nDTEND:20300302T110000Z"));
    assert!(body.contains("DTSTART:20300303T100000Z\r\nDTEND:20300303T110000Z"));
    assert!(body.contains("DTSTART:99991231T235959Z\r\nDTEND:99991231T235959Z"));
}

#[tokio::test]
async fn the_longest_allowed_interview_is_kept_and_longer_ones_get_the_default() {
    let f = fixture().await;
    let id = add_app(&f.db, "Long Co", "interview", |_| {}).await;
    for (day, minutes) in [(1, 1440), (2, 1441), (3, 0)] {
        add_event(
            &f.db,
            id,
            "interview_scheduled",
            "2026-01-01T10:00:00Z",
            json!({
                "scheduledAt": format!("2030-03-0{day}T10:00:00Z"),
                "interviewType": format!("round {day}"),
                "durationMinutes": minutes,
            }),
        )
        .await;
    }
    let body = calendar(&f.app).await;
    assert!(body.contains("DTSTART:20300301T100000Z\r\nDTEND:20300302T100000Z"));
    assert!(body.contains("DTSTART:20300302T100000Z\r\nDTEND:20300302T110000Z"));
    assert!(body.contains("DTSTART:20300303T100000Z\r\nDTEND:20300303T110000Z"));
}

#[tokio::test]
async fn the_feed_leaves_out_free_text_next_action() {
    let f = fixture().await;
    add_app(&f.db, "Quiet Co", "applied", |fields| {
        fields.follow_up_at = Some(Some(ts("2030-01-02T10:00:00Z")));
        fields.next_action = Some(Some("PRIVATE-NOTE call the recruiter".to_owned()));
    })
    .await;
    let body = calendar(&f.app).await;
    assert!(body.contains("Follow up: Quiet Co"), "{body}");
    assert!(
        !body.contains("PRIVATE-NOTE") && !body.contains("Next action"),
        "{body}"
    );
}
