//! The dashboard's forms and the PWA files, against a real SQLite file with synthetic data.

use axum::Extension;
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use nexus_core::Db;
use nexus_core::auth::{FormToken, SCOPE_TOOLS};
use nexus_core::repo::applications::{self, ApplicationFields};
use nexus_core::repo::{contacts, events};
use nexus_mcp::McpState;
use tempfile::TempDir;
use tower::ServiceExt;

const EVIL: &str = "<script>alert(1)</script>";

struct Fixture {
    _dir: TempDir,
    db: Db,
    state: McpState,
    app: Router,
}

async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).await.unwrap();
    let state = McpState::new(
        db.clone(),
        "owner-1".to_owned(),
        "owner@example.com".to_owned(),
        nexus_mcp::Renderer::InProcess,
    );
    let app = nexus_web::dashboard_router(state.clone(), "Europe/Madrid".parse().unwrap())
        .merge(nexus_web::static_router());
    Fixture {
        _dir: dir,
        db,
        state,
        app,
    }
}

async fn add_app(db: &Db, company: &str, status: &str) -> i64 {
    let fields = ApplicationFields {
        status: Some(status.to_owned()),
        ..Default::default()
    };
    applications::create(&mut db.conn().await.unwrap(), company, "Engineer", fields)
        .await
        .unwrap()
        .id
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, String) {
    let response = app.clone().oneshot(request).await.unwrap();
    let (status, headers) = (response.status(), response.headers().clone());
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

async fn get(app: &Router, uri: &str) -> (StatusCode, HeaderMap, String) {
    send(app, Request::get(uri).body(Body::empty()).unwrap()).await
}

fn encode(value: &str) -> String {
    value.bytes().map(|b| format!("%{b:02X}")).collect()
}

async fn post(app: &Router, uri: &str, fields: &[(&str, &str)]) -> (StatusCode, HeaderMap, String) {
    let body: Vec<String> = fields
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect();
    let request = Request::post(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body.join("&")))
        .unwrap();
    send(app, request).await
}

/// The value of the first hidden input called `name` on the page.
fn hidden(page: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\" value=\"");
    let start = page.find(&marker).unwrap_or_else(|| panic!("no {name}")) + marker.len();
    page[start..start + page[start..].find('"').unwrap()].to_owned()
}

/// Posts a form as the page offers it: the page's CSRF token, `expected`, `at` and `key`, plus the
/// caller's own fields.
async fn submit(
    f: &Fixture,
    id: i64,
    kind: &str,
    own: &[(&str, &str)],
) -> (StatusCode, HeaderMap, String) {
    let (_, _, page) = get(&f.app, &format!("/applications/{id}")).await;
    let (csrf, expected, at, key) = (
        hidden(&page, "csrf"),
        hidden(&page, "expected"),
        hidden(&page, "at"),
        hidden(&page, "key"),
    );
    let mut fields = vec![
        ("csrf", csrf.as_str()),
        ("expected", expected.as_str()),
        ("at", at.as_str()),
        ("key", key.as_str()),
    ];
    fields.extend_from_slice(own);
    post(&f.app, &format!("/applications/{id}/{kind}"), &fields).await
}

async fn application(db: &Db, id: i64) -> applications::Application {
    applications::get(&mut db.conn().await.unwrap(), id)
        .await
        .unwrap()
        .unwrap()
}

async fn event_types(db: &Db, id: i64) -> Vec<String> {
    events::list_for_application(&mut db.conn().await.unwrap(), id, Some(500))
        .await
        .unwrap()
        .into_iter()
        .map(|event| event.r#type)
        .collect()
}

fn location(headers: &HeaderMap) -> &str {
    headers["location"].to_str().unwrap()
}

async fn sources(db: &Db, id: i64) -> Vec<String> {
    events::list_for_application(&mut db.conn().await.unwrap(), id, Some(500))
        .await
        .unwrap()
        .into_iter()
        .map(|event| event.source.unwrap_or_default())
        .collect()
}

#[tokio::test]
async fn events_from_forms_say_dashboard_and_from_the_tool_route_say_mcp() {
    let f = fixture().await;
    let id = add_app(&f.db, "Acme", "inbound").await;
    submit(&f, id, "note", &[("note", "n")]).await;
    let own = [
        ("what", "interview"),
        ("detail", "Phone"),
        ("when", "2030-05-06T10:30"),
    ];
    submit(&f, id, "event", &own).await;
    submit(&f, id, "event", &[("what", "applied"), ("confirm", "1")]).await;
    submit(&f, id, "event", &[("what", "offer"), ("confirm", "1")]).await;
    submit(&f, id, "event", &[("what", "rejected"), ("confirm", "1")]).await;
    submit(
        &f,
        id,
        "event",
        &[("what", "follow_up"), ("when", "2030-06-01T09:00")],
    )
    .await;
    // These forms write no event, so there is no source to get wrong.
    submit(&f, id, "triage", &[("rating", "3")]).await;
    submit(&f, id, "contact", &[("name", "Pat")]).await;
    submit(&f, id, "archive", &[("confirm", "1")]).await;
    submit(&f, id, "unarchive", &[]).await;
    let stored = sources(&f.db, id).await;
    assert_eq!(stored.len(), 6, "{stored:?}");
    assert!(stored.iter().all(|s| s == "dashboard"), "{stored:?}");

    let args = serde_json::json!({"applicationId": id.to_string(), "note": "from an agent", "occurredAt": "2030-01-01T00:00:00.000Z", "idempotencyKey": "agent-key-1"});
    let state = std::sync::Arc::new(f.state.clone());
    let done = nexus_mcp::call_tool(
        state,
        &[SCOPE_TOOLS],
        "mcp",
        "append_application_note",
        args,
    )
    .await;
    assert_ne!(serde_json::to_value(&done).unwrap()["isError"], true);
    let stored = sources(&f.db, id).await;
    assert_eq!(
        stored.iter().filter(|s| *s == "mcp").count(),
        1,
        "{stored:?}"
    );
}

#[tokio::test]
async fn a_note_is_appended_once_and_shown() {
    let f = fixture().await;
    let id = add_app(&f.db, "Acme", "inbound").await;
    let (status, headers, _) = submit(&f, id, "note", &[("note", "Called the recruiter")]).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        location(&headers),
        format!("/applications/{id}?done=note#actions")
    );
    let (_, _, page) = get(&f.app, location(&headers)).await;
    assert!(page.contains("Note added."));
    assert!(page.contains("Called the recruiter"));
    assert_eq!(event_types(&f.db, id).await, ["note_added"]);
}

#[tokio::test]
async fn sending_a_form_twice_is_one_change() {
    let f = fixture().await;
    let id = add_app(&f.db, "Acme", "inbound").await;
    let (_, _, page) = get(&f.app, &format!("/applications/{id}")).await;
    let fields = [
        ("csrf", hidden(&page, "csrf")),
        ("at", hidden(&page, "at")),
        ("key", hidden(&page, "key")),
        ("expected", hidden(&page, "expected")),
        ("note", "Once".to_owned()),
    ];
    let fields: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
    for _ in 0..2 {
        let (status, ..) = post(&f.app, &format!("/applications/{id}/note"), &fields).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
    }
    assert_eq!(event_types(&f.db, id).await, ["note_added"]);
}

#[tokio::test]
async fn an_empty_note_is_refused_and_everything_typed_is_kept() {
    let f = fixture().await;
    let id = add_app(&f.db, "Acme", "inbound").await;
    let (status, _, page) = submit(&f, id, "note", &[("note", "  ")]).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(page.contains("Write a note first."));
    let (status, _, page) = submit(
        &f,
        id,
        "contact",
        &[("name", ""), ("email", "kept@example.com"), ("role", EVIL)],
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(page.contains("A contact needs a name."));
    assert!(page.contains("kept@example.com"));
    assert!(page.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(!page.contains("<script>"));
    assert!(event_types(&f.db, id).await.is_empty());
}

#[tokio::test]
async fn too_long_a_note_shows_a_plain_message_not_the_tool_error() {
    let f = fixture().await;
    let id = add_app(&f.db, "Acme", "inbound").await;
    let long = "x".repeat(5001);
    let (status, _, page) = submit(&f, id, "note", &[("note", &long)]).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(page.contains("A value was not accepted"));
    for internal in ["MCP error", "Invalid arguments", "isError", "validation"] {
        assert!(!page.contains(internal), "{internal}");
    }
    assert!(event_types(&f.db, id).await.is_empty());
}

#[tokio::test]
async fn triage_and_rating_are_saved_and_cleared() {
    let f = fixture().await;
    let id = add_app(&f.db, "Acme", "inbound").await;
    let own = [
        ("rating", "4"),
        ("triageQuality", "2"),
        ("triageReason", "Remote, fair pay"),
    ];
    let (status, ..) = submit(&f, id, "triage", &own).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let a = application(&f.db, id).await;
    assert_eq!((a.rating, a.triage_quality), (Some(4), Some(2)));
    assert_eq!(a.triage_reason.as_deref(), Some("Remote, fair pay"));
    assert_eq!(a.status, "inbound");

    let (status, ..) = submit(
        &f,
        id,
        "triage",
        &[("rating", ""), ("triageQuality", ""), ("triageReason", "")],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let a = application(&f.db, id).await;
    assert_eq!(
        (a.rating, a.triage_quality, a.triage_reason),
        (None, None, None)
    );

    let (status, _, page) = submit(
        &f,
        id,
        "triage",
        &[("rating", "9"), ("triageReason", "kept")],
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(page.contains("Rating must be a whole number from 1 to 5."));
    assert!(page.contains("kept"));
}

#[tokio::test]
async fn lifecycle_events_change_status_and_the_closing_ones_ask_first() {
    let f = fixture().await;
    let id = add_app(&f.db, "Acme", "inbound").await;

    let (status, _, page) = submit(&f, id, "event", &[("what", "applied")]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Please confirm") && page.contains("Mark Acme as applied?"));
    assert_eq!(application(&f.db, id).await.status, "inbound");
    assert!(event_types(&f.db, id).await.is_empty());

    let (status, ..) = submit(&f, id, "event", &[("what", "applied"), ("confirm", "1")]).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(application(&f.db, id).await.status, "applied");

    let own = [
        ("what", "interview"),
        ("detail", "Phone screen"),
        ("when", "2030-05-06T10:30"),
    ];
    let (status, ..) = submit(&f, id, "event", &own).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let a = application(&f.db, id).await;
    assert_eq!(a.status, "interview");

    let own = [("what", "follow_up"), ("when", "2030-06-01T09:00")];
    submit(&f, id, "event", &own).await;
    assert_eq!(
        application(&f.db, id).await.follow_up_at.unwrap().as_str(),
        "2030-06-01T07:00:00.000Z"
    );

    let (_, _, page) = submit(
        &f,
        id,
        "event",
        &[("what", "rejected"), ("detail", "No fit")],
    )
    .await;
    assert!(page.contains("Mark Acme as rejected?") && page.contains("No fit"));
    assert_eq!(application(&f.db, id).await.status, "interview");
    let own = [("what", "rejected"), ("detail", "No fit"), ("confirm", "1")];
    submit(&f, id, "event", &own).await;
    assert_eq!(application(&f.db, id).await.status, "rejected");

    let (_, _, page) = submit(&f, id, "event", &[("what", "offer")]).await;
    assert!(page.contains("Record an offer from Acme?"));
}

#[tokio::test]
async fn an_event_with_missing_details_is_refused_in_words() {
    let f = fixture().await;
    let id = add_app(&f.db, "Acme", "applied").await;
    for (own, message) in [
        (vec![("what", "")], "Choose what happened."),
        (
            vec![("what", "interview"), ("when", "2030-05-06T10:30")],
            "An interview needs a type and a date and time.",
        ),
        (
            vec![("what", "follow_up")],
            "A follow-up needs a date and time.",
        ),
        (
            vec![("what", "follow_up"), ("when", "not a date")],
            "The date and time were not understood.",
        ),
    ] {
        let (status, _, page) = submit(&f, id, "event", &own).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(page.contains(message), "{message}");
    }
    assert!(event_types(&f.db, id).await.is_empty());
}

#[tokio::test]
async fn a_contact_is_added_and_escaped() {
    let f = fixture().await;
    let id = add_app(&f.db, "Acme", "applied").await;
    let own = [
        ("name", EVIL),
        ("role", "Recruiter"),
        ("email", "r@example.com"),
        ("phone", ""),
        ("linkedIn", ""),
    ];
    let (status, headers, _) = submit(&f, id, "contact", &own).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_, _, page) = get(&f.app, location(&headers)).await;
    assert!(page.contains("Contact added."));
    assert!(page.contains("&lt;script&gt;alert(1)&lt;/script&gt;") && !page.contains("<script>"));
    let all = contacts::list_for_application(&mut f.db.conn().await.unwrap(), id)
        .await
        .unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].email.as_deref(), Some("r@example.com"));
}

#[tokio::test]
async fn archiving_asks_first_and_can_be_undone() {
    let f = fixture().await;
    let id = add_app(&f.db, "Acme", "applied").await;
    let (status, _, page) = submit(&f, id, "archive", &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Archive Acme?"));
    assert!(application(&f.db, id).await.archived_at.is_none());

    let (status, ..) = submit(&f, id, "archive", &[("confirm", "1")]).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(application(&f.db, id).await.archived_at.is_some());
    let (_, _, page) = get(&f.app, &format!("/applications/{id}")).await;
    assert!(
        page.contains("Restore to the pipeline") && !page.contains("Takes it out of the pipeline")
    );

    let (status, ..) = submit(&f, id, "unarchive", &[]).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(application(&f.db, id).await.archived_at.is_none());
}

#[tokio::test]
async fn a_stale_page_gets_a_conflict_message_and_keeps_the_typed_text() {
    let f = fixture().await;
    let id = add_app(&f.db, "Acme", "applied").await;
    let (_, _, page) = get(&f.app, &format!("/applications/{id}")).await;
    let stale = hidden(&page, "expected");
    submit(&f, id, "triage", &[("rating", "3")]).await;
    let fields = [
        ("csrf", hidden(&page, "csrf")),
        ("at", hidden(&page, "at")),
        ("key", hidden(&page, "key")),
        ("expected", stale),
        ("note", "My draft note".to_owned()),
    ];
    let fields: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let (status, _, page) = post(&f.app, &format!("/applications/{id}/note"), &fields).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(page.contains("This application changed since you opened the page."));
    assert!(page.contains("My draft note"));
    assert!(event_types(&f.db, id).await.is_empty());
    assert!(!page.contains("\"conflict\""));
}

#[tokio::test]
async fn a_post_without_the_right_token_changes_nothing() {
    let f = fixture().await;
    let id = add_app(&f.db, "Acme", "applied").await;
    let uri = format!("/applications/{id}/note");
    let (status, _, page) = post(
        &f.app,
        &uri,
        &[
            ("note", "x"),
            ("at", "2030-01-01T00:00:00Z"),
            ("key", "abcdefgh1"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(page.contains("This form has expired"));
    let (status, ..) = post(
        &f.app,
        &uri,
        &[
            ("csrf", "wrong"),
            ("note", "x"),
            ("at", "2030-01-01T00:00:00Z"),
            ("key", "abcdefgh1"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (_, _, page) = get(&f.app, &format!("/applications/{id}")).await;
    let own_token = hidden(&page, "csrf");
    let expected = hidden(&page, "expected");
    let other_session = f
        .app
        .clone()
        .layer(Extension(FormToken("another-sessions-token".to_owned())));
    let (status, ..) = post(
        &other_session,
        &uri,
        &[
            ("csrf", &own_token),
            ("note", "x"),
            ("at", "2030-01-01T00:00:00Z"),
            ("key", "abcdefgh1"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, ..) = post(
        &other_session,
        &uri,
        &[
            ("csrf", "another-sessions-token"),
            ("note", "x"),
            ("expected", &expected),
            ("at", "2030-01-01T00:00:00Z"),
            ("key", "abcdefgh1"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    for kind in ["archive", "unarchive", "triage", "event", "contact"] {
        let (status, ..) = post(
            &f.app,
            &format!("/applications/{id}/{kind}"),
            &[("confirm", "1"), ("what", "applied")],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{kind}");
    }
    assert_eq!(event_types(&f.db, id).await, ["note_added"]);
    assert!(application(&f.db, id).await.archived_at.is_none());
}

#[tokio::test]
async fn reading_never_changes_anything() {
    let f = fixture().await;
    let id = add_app(&f.db, "Acme", "applied").await;
    let before = application(&f.db, id).await.updated_at;
    for uri in [
        format!("/applications/{id}?done=archive"),
        format!("/applications/{id}?note=x&archive=1&confirm=1"),
    ] {
        assert_eq!(get(&f.app, &uri).await.0, StatusCode::OK);
    }
    for kind in ["note", "archive", "event", "triage", "contact", "unarchive"] {
        let (status, ..) = get(&f.app, &format!("/applications/{id}/{kind}")).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{kind}");
    }
    assert_eq!(application(&f.db, id).await.updated_at, before);
    assert!(event_types(&f.db, id).await.is_empty());
}

#[tokio::test]
async fn unknown_forms_and_applications_are_not_found_and_big_bodies_are_cut() {
    let f = fixture().await;
    let id = add_app(&f.db, "Acme", "applied").await;
    let (_, _, page) = get(&f.app, &format!("/applications/{id}")).await;
    let csrf = hidden(&page, "csrf");
    assert_eq!(
        post(
            &f.app,
            &format!("/applications/{id}/delete"),
            &[("csrf", &csrf)]
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        post(
            &f.app,
            "/applications/9999/note",
            &[("csrf", &csrf), ("note", "x")]
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let huge = "a".repeat(200 * 1024);
    let (status, ..) = post(
        &f.app,
        &format!("/applications/{id}/note"),
        &[("csrf", &csrf), ("note", &huge)],
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn the_tool_entry_point_still_refuses_mixed_lifecycle_calls_whole() {
    let f = fixture().await;
    let id = add_app(&f.db, "Acme", "inbound").await;
    let args = serde_json::json!({"id": id.to_string(), "status": "applied", "rating": 5});
    let result = nexus_mcp::call_tool(
        std::sync::Arc::new(f.state.clone()),
        &[SCOPE_TOOLS],
        "mcp",
        "update_application",
        args,
    )
    .await;
    let value = serde_json::to_value(&result).unwrap();
    assert_eq!(value["isError"], true);
    assert_eq!(
        value["structuredContent"]["error"]["code"],
        "lifecycle_event_required"
    );
    let a = application(&f.db, id).await;
    assert_eq!((a.status.as_str(), a.rating), ("inbound", None));

    let none = nexus_mcp::call_tool(
        std::sync::Arc::new(f.state.clone()),
        &[],
        "mcp",
        "get_application",
        serde_json::json!({"id": "1"}),
    )
    .await;
    assert_eq!(
        serde_json::to_value(&none).unwrap()["structuredContent"]["error"]["code"],
        "insufficient_scope"
    );
}

#[tokio::test]
async fn hostile_stored_values_are_escaped_in_every_new_place() {
    let f = fixture().await;
    let id = add_app(&f.db, EVIL, "applied").await;
    submit(&f, id, "triage", &[("rating", "3"), ("triageReason", EVIL)]).await;
    let (_, _, page) = get(&f.app, &format!("/applications/{id}")).await;
    assert!(!page.contains("<script>"));
    let (_, _, confirm) = submit(&f, id, "archive", &[("note", EVIL)]).await;
    assert!(confirm.contains("Archive &lt;script&gt;"));
    assert!(!confirm.contains("<script>") && !confirm.contains("alert(1)</"));
    let quote = "\"><img src=x onerror=alert(1)>";
    let (_, _, page) = submit(&f, id, "contact", &[("name", ""), ("email", quote)]).await;
    assert!(!page.contains("<img"));
}

#[tokio::test]
async fn a_note_sent_back_to_its_editor_cannot_close_the_textarea() {
    let f = fixture().await;
    let id = add_app(&f.db, "Careful Co", "applied").await;
    let note = format!("</textarea>{EVIL}{}", "x".repeat(5001));
    let (status, _, page) = submit(&f, id, "note", &[("note", &note)]).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!page.contains("<script>"), "{page}");
    assert!(
        page.contains("&lt;/textarea&gt;") || page.contains("&lt;/textarea>"),
        "{page}"
    );
}

#[tokio::test]
async fn the_manifest_icons_and_offline_page_are_public_and_personal_data_free() {
    let f = fixture().await;
    add_app(&f.db, "Private Corp", "applied").await;
    let (status, headers, body) = get(&f.app, "/manifest.webmanifest").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/manifest+json");
    let manifest: serde_json::Value = serde_json::from_str(&body).unwrap();
    for key in [
        "name",
        "short_name",
        "start_url",
        "scope",
        "display",
        "theme_color",
        "background_color",
    ] {
        assert!(manifest[key].is_string(), "{key}");
    }
    assert_eq!(manifest["display"], "standalone");
    let icons = manifest["icons"].as_array().unwrap();
    assert!(icons.iter().any(|i| i["sizes"] == "192x192"));
    assert!(
        icons
            .iter()
            .any(|i| i["sizes"] == "512x512" && i["purpose"] != "maskable")
    );
    assert!(icons.iter().any(|i| i["purpose"] == "maskable"));
    for icon in icons {
        let src = icon["src"].as_str().unwrap();
        let response = f
            .app
            .clone()
            .oneshot(Request::get(src).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{src}");
        assert_eq!(response.headers()["content-type"], "image/png");
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&bytes[..4], b"\x89PNG");
    }
    assert_eq!(
        get(&f.app, "/icons/missing.png").await.0,
        StatusCode::NOT_FOUND
    );

    let (status, _, offline) = get(&f.app, "/offline").await;
    assert_eq!(status, StatusCode::OK);
    assert!(offline.contains("You are offline") && !offline.contains("Private Corp"));
}

#[tokio::test]
async fn the_service_worker_is_versioned_scoped_and_has_its_own_policy() {
    let f = fixture().await;
    let (status, headers, body) = get(&f.app, "/sw.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/javascript")
    );
    assert_eq!(headers["cache-control"], "no-cache");
    assert_eq!(
        headers["content-security-policy"],
        "default-src 'none'; connect-src 'self'"
    );
    assert!(!body.contains("__VERSION__"));
    let (_, _, again) = get(&f.app, "/sw.js").await;
    assert_eq!(body, again);

    let (_, _, page) = get(&f.app, "/offline").await;
    let version = page
        .split("app.js?v=")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_owned();
    assert!(
        body.contains(&version),
        "the worker's cache name follows the build"
    );
    let (_, headers, _) = get(&f.app, &format!("/assets/app.js?v={version}")).await;
    assert!(
        headers["cache-control"]
            .to_str()
            .unwrap()
            .contains("immutable")
    );
    let (_, headers, _) = get(&f.app, "/assets/app.js").await;
    assert_eq!(headers["cache-control"], "no-cache");
}

#[tokio::test]
async fn pages_declare_the_pwa_and_a_strict_policy() {
    let f = fixture().await;
    let id = add_app(&f.db, "Acme", "applied").await;
    let (_, headers, page) = get(&f.app, &format!("/applications/{id}")).await;
    for needle in [
        "rel=\"manifest\"",
        "name=\"theme-color\"",
        "rel=\"apple-touch-icon\"",
        "app.js?v=",
    ] {
        assert!(page.contains(needle), "{needle}");
    }
    assert!(!page.contains(" style=") && !page.contains("<script>"));
    let csp = headers["content-security-policy"].to_str().unwrap();
    for directive in [
        "default-src 'none'",
        "script-src 'self'",
        "manifest-src 'self'",
        "worker-src 'self'",
        "form-action 'self'",
    ] {
        assert!(csp.contains(directive), "{directive}");
    }
    assert!(!csp.contains("unsafe-"));
    assert_eq!(headers["cache-control"], "no-store");
}
