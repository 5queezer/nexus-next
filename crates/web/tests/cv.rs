//! The CV workspace against a real SQLite file and real compiles, with invented data.

use std::time::{Duration, Instant};

use axum::Extension;
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use nexus_core::Db;
use nexus_core::auth::{FormToken, SCOPE_TOOLS};
use nexus_core::repo::cv_facts::{self, OwnerFacts};
use nexus_mcp::McpState;
use nexus_web::{OPENAI_BASE_URL, VoiceConfig, VoiceKey};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

const TOKEN: &str = "test-form-token";

struct Fixture {
    dir: TempDir,
    db: Db,
    state: McpState,
    app: Router,
}

fn voice() -> VoiceConfig {
    VoiceConfig {
        key: VoiceKey::new("sk-synthetic-never-sent-0123456789".to_owned()),
        model: "model-x".to_owned(),
        voice: "marin".to_owned(),
        base_url: OPENAI_BASE_URL.to_owned(),
        lifetime: Duration::from_secs(900),
    }
}

fn profile() -> Value {
    json!({
        "name": "Mara Example",
        "contact": {"email": "mara@example.test", "location": "Hamburg"},
        "profile": "Backend engineer building freight software.",
        "skills": [{"category": "Languages", "items": ["Rust", "Go"]}],
        "experience": [
            {"id": "brightpath", "company": "Brightpath Logistics", "title": "Software Engineer",
             "date": "03/2019 – 12/2022", "location": "Hamburg", "tier": 1,
             "bullets": ["Cut invoice errors by 40% across 3 depots.", "Built the routing service."]},
        ],
        "education": [{"institution": "Nordsee University", "degree": "BSc Computer Science",
            "date": "2015 – 2018", "location": "Kiel"}],
    })
}

fn build(state: &McpState, dir: &TempDir, voice: Option<VoiceConfig>) -> Router {
    nexus_web::cv_router(
        state.clone(),
        "Europe/Madrid".parse().unwrap(),
        nexus_web::Voice::new(voice),
        dir.path(),
    )
    .layer(Extension(FormToken(TOKEN.to_owned())))
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
    let result = nexus_mcp::call_tool(
        std::sync::Arc::new(state.clone()),
        &[SCOPE_TOOLS],
        "test",
        "upsert_cv_profile",
        profile(),
    )
    .await;
    assert_ne!(result.is_error, Some(true));
    let app = build(&state, &dir, None);
    Fixture {
        dir,
        db,
        state,
        app,
    }
}

async fn lock(db: &Db) {
    let mut tx = db.begin_write().await.unwrap();
    cv_facts::lock(&mut tx, "test").await.unwrap();
    tx.commit().await.unwrap();
}

async fn locked() -> Fixture {
    let f = fixture().await;
    lock(&f.db).await;
    f
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let response = app.clone().oneshot(request).await.unwrap();
    let (status, headers) = (response.status(), response.headers().clone());
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, body.to_vec())
}

async fn get(app: &Router, uri: &str) -> (StatusCode, HeaderMap, String) {
    let request = Request::get(uri).body(Body::empty()).unwrap();
    let (status, headers, body) = send(app, request).await;
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

fn encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

async fn post(app: &Router, uri: &str, fields: &[(&str, &str)]) -> (StatusCode, HeaderMap, String) {
    let body = fields
        .iter()
        .map(|(key, value)| format!("{key}={}", encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    let request = Request::post(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let (status, headers, bytes) = send(app, request).await;
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// What a browser would hold in the source editor: the parser eats one leading newline.
fn editor_text(page: &str) -> String {
    let at = page.find("id=\"cv-source\"").expect("the editor");
    let start = at + page[at..].find('>').unwrap() + 1;
    let end = start + page[start..].find("</textarea>").unwrap();
    let raw = &page[start..end];
    unescape(raw.strip_prefix('\n').unwrap_or(raw))
}

fn hidden(page: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\" ");
    let at = page.find(&marker).unwrap_or_else(|| panic!("{name}"));
    let tag_end = at + page[at..].find('>').unwrap();
    let tag = &page[at..tag_end];
    tag.split("value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_owned()
}

const VARIANT: &str = "/cv/variants/default";
/// The same page in the view that holds the editor, and in the one that holds the revisions.
const EDIT: &str = "/cv/variants/default?view=edit";
const HISTORY: &str = "/cv/variants/default?view=history";

/// The draft or approved badge at the top of a variant page.
fn badge(page: &str) -> &str {
    let after = page.split("id=\"cv-state\"").nth(1).expect("the badge");
    let start = after.find('>').unwrap() + 1;
    let end = start + after[start..].find('<').unwrap();
    &after[start..end]
}

async fn head_of(f: &Fixture) -> (i64, String) {
    let (status, _, page) = get(&f.app, EDIT).await;
    assert_eq!(status, StatusCode::OK);
    (
        hidden(&page, "expected").parse().unwrap(),
        editor_text(&page),
    )
}

async fn save(f: &Fixture, expected: i64, source: &str) -> (StatusCode, HeaderMap, String) {
    post(
        &f.app,
        &format!("{VARIANT}/save"),
        &[
            ("csrf", TOKEN),
            ("expected", &expected.to_string()),
            ("fromBase", "1"),
            ("source", source),
            ("summary", "A human edit"),
        ],
    )
    .await
}

async fn revision_rows(db: &Db) -> Vec<(i64, String, String, String)> {
    sqlx::query_as("SELECT number, source, origin, summary FROM cv_revisions ORDER BY number")
        .fetch_all(db.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn the_list_names_the_variants_and_the_facts_state() {
    let f = fixture().await;
    let (status, _, page) = get(&f.app, "/cv").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("href=\"/cv/variants/default\""), "{page}");
    assert!(page.contains("Nothing stored yet"));
    assert!(page.contains("aren&#x27;t locked") || page.contains("aren't locked"));
    assert!(page.contains("href=\"/cv/facts\""));
    assert!(page.contains("aria-current=\"page\""));
}

#[tokio::test]
async fn a_save_stores_only_what_changed_as_a_revision_from_the_dashboard() {
    let f = locked().await;
    let (expected, source) = head_of(&f).await;
    assert_eq!(expected, 0);
    let changed = source.replace("Built the routing service.", "Built the routing engine.");
    assert_ne!(changed, source);
    // As a browser sends a textarea.
    let (status, headers, _) = save(&f, 0, &changed.replace('\n', "\r\n")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        headers["location"],
        "/cv/variants/default?done=saved#cv-workspace"
    );

    let rows = revision_rows(&f.db).await;
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].0, rows[0].1.as_str()), (1, changed.as_str()));
    assert_eq!(
        (rows[0].2.as_str(), rows[0].3.as_str()),
        ("dashboard", "A human edit")
    );

    let (_, _, page) = get(&f.app, "/cv/variants/default?done=saved").await;
    assert!(page.contains("Saved as a draft"), "{page}");
    assert_eq!(badge(&page), "Draft, not approved");
    assert!(page.contains("class=\"da\""), "added lines are marked");
    assert_eq!(
        head_of(&f).await,
        (1, changed.clone()),
        "the editor holds the head"
    );

    // Only the one line moves in the second revision's diff.
    let next = changed.replace("Built the routing engine.", "Built the routing core.");
    assert_eq!(
        save(&f, 1, &next.replace('\n', "\r\n")).await.0,
        StatusCode::SEE_OTHER
    );
    let (_, _, page) = get(&f.app, HISTORY).await;
    let listed = page.split("id=\"cv-revisions\"").nth(1).unwrap();
    let diff_of_head = listed.split("Revision 1").next().unwrap();
    assert_eq!(
        diff_of_head.matches("class=\"da\"").count(),
        1,
        "one added line"
    );
    assert_eq!(
        diff_of_head.matches("class=\"dd\"").count(),
        1,
        "one removed line"
    );
}

#[tokio::test]
async fn a_failed_compile_keeps_the_text_and_names_the_line() {
    let f = locked().await;
    let (_, source) = head_of(&f).await;
    let broken = format!("{source}\n#nonexistent-function()\n");
    let (status, _, page) = save(&f, 0, &broken).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(editor_text(&page), broken);
    let line = broken.lines().count();
    assert!(page.contains(&format!("Line {line}, column")), "{page}");
    assert!(page.contains("doesn&#x27;t compile") || page.contains("doesn't compile"));
    assert!(!page.contains("cv_compile_failed"), "no raw codes");
    assert!(revision_rows(&f.db).await.is_empty(), "nothing stored");
    assert_eq!(hidden(&page, "expected"), "0");
}

#[tokio::test]
async fn the_hard_rules_apply_to_a_person_and_show_the_token_marked() {
    let f = locked().await;
    let (_, source) = head_of(&f).await;
    let hidden_text = source.replace(
        "Built the routing service.",
        "Built it. #text(fill: white)[Zanzibar]",
    );
    let (status, _, page) = save(&f, 0, &hidden_text).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(editor_text(&page), hidden_text);
    assert!(page.contains("<mark>Zanzibar</mark>"), "{page}");
    assert!(
        page.contains("can&#x27;t see") || page.contains("can't see"),
        "{page}"
    );
    assert!(!page.contains("cv_fact_violation"));
    assert!(revision_rows(&f.db).await.is_empty());
}

/// An agent's draft, as the owner reads it.
async fn agent_patch(f: &Fixture, expected: i64, old: &str, new: &str) -> Value {
    let result = nexus_mcp::call_tool(
        std::sync::Arc::new(f.state.clone()),
        &[SCOPE_TOOLS],
        "mcp",
        "patch_cv",
        json!({"expectedRevision": expected, "summary": "An agent's edit", "origin": "telegram",
            "edits": [{"oldText": old, "newText": new}]}),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    result.structured_content.unwrap()
}

async fn approvals(db: &Db) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM cv_approvals")
        .fetch_one(db.pool())
        .await
        .unwrap()
}

async fn tap_approve(f: &Fixture, page: &str) -> (StatusCode, HeaderMap, String) {
    post(
        &f.app,
        &format!("{VARIANT}/approve"),
        &[
            ("csrf", TOKEN),
            ("revision", &hidden(page, "revision")),
            ("sourceSha256", &hidden(page, "sourceSha256")),
            ("factsSha256", &hidden(page, "factsSha256")),
        ],
    )
    .await
}

#[tokio::test]
async fn an_agents_edit_waits_for_the_owner_who_reads_it_and_approves_it() {
    let f = locked().await;
    let draft = agent_patch(
        &f,
        0,
        "Built the routing service.",
        "Built 77 routing services at Zanzibar.",
    )
    .await;
    assert_eq!(draft["state"], "draft");

    let (status, _, page) = get(&f.app, VARIANT).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(badge(&page), "Draft, not approved");
    // By name: another variant can stand at revision 1 too.
    assert!(page.contains(">Approve revision 1 of default<"), "{page}");
    assert!(page.contains("id=\"cv-review-items\""), "{page}");
    // Marked where it stands, in what a reader sees: nothing is approved yet, so that is all of it.
    let rendered = page.split("id=\"cv-rendered-diff\"").nth(1).unwrap();
    assert!(rendered.contains("<mark>77</mark>"), "{rendered}");
    assert!(rendered.contains("class=\"da\""), "{rendered}");
    assert!(page.contains("Download draft PDF, stamped"), "{page}");
    assert!(page.contains("Source changes"), "{page}");
    assert!(page.contains("class=\"count\""), "the nav counts it");

    let (_, _, list) = get(&f.app, "/cv").await;
    assert!(list.contains("Waiting for your approval"), "{list}");
    assert!(list.contains("/cv/variants/default#cv-approval"), "{list}");
    assert!(list.contains("class=\"count\""), "{list}");

    let (status, headers, _) = tap_approve(&f, &page).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let landed = "/cv/variants/default?view=review&done=approved#cv-approval";
    assert_eq!(headers["location"], landed);
    let by: String = sqlx::query_scalar("SELECT approved_by FROM cv_approvals")
        .fetch_one(f.db.pool())
        .await
        .unwrap();
    assert_eq!(by, "dashboard");
    let (_, _, page) = get(&f.app, landed).await;
    assert_eq!(badge(&page), "Approved");
    assert!(
        page.contains("Revision 1 of default is approved. Its PDF now has no stamp."),
        "{page}"
    );
    assert!(page.contains("id=\"cv-approval\""), "{page}");
    assert!(page.contains("You approved this text"), "{page}");
    assert!(!page.contains("Approve revision"), "{page}");
    assert!(page.contains(">Download PDF<"), "{page}");
    assert!(!page.contains("class=\"count\""), "nothing waits: {page}");
    let (_, _, list) = get(&f.app, "/cv").await;
    assert!(!list.contains("Waiting for your approval"), "{list}");
}

/// Three small drafts can't hide what changed: the owner reads them against what was last approved.
#[tokio::test]
async fn the_owner_reads_a_chain_of_drafts_against_the_last_approved_text() {
    let f = locked().await;
    agent_patch(
        &f,
        0,
        "Built the routing service.",
        "Built the routing engine.",
    )
    .await;
    let (_, _, page) = get(&f.app, VARIANT).await;
    tap_approve(&f, &page).await;
    agent_patch(&f, 1, "routing engine.", "routing engine for 11 ports.").await;
    agent_patch(&f, 2, "11 ports.", "11 ports and 22 depots.").await;
    agent_patch(&f, 3, "22 depots.", "22 depots across 33 towns.").await;
    let (_, _, page) = get(&f.app, VARIANT).await;
    assert!(page.contains("Compared with revision 1"), "{page}");
    let rendered = page.split("id=\"cv-rendered-diff\"").nth(1).unwrap();
    let rendered = rendered.split("</pre>").next().unwrap();
    for number in ["11", "22", "33"] {
        assert!(
            rendered.contains(&format!("<mark>{number}</mark>")),
            "{number}: {rendered}"
        );
    }
    assert!(
        rendered.contains("class=\"dd\""),
        "what was approved shows removed: {rendered}"
    );
    assert!(rendered.contains("Built the routing engine."), "{rendered}");
}

/// The form names what the owner read. A revision landing in between can't be approved by accident.
#[tokio::test]
async fn approving_what_has_moved_is_refused_and_shows_what_is_there() {
    let f = locked().await;
    agent_patch(
        &f,
        0,
        "Built the routing service.",
        "Built the routing engine.",
    )
    .await;
    let (_, _, read) = get(&f.app, VARIANT).await;

    agent_patch(&f, 1, "routing engine.", "routing engine at Zanzibar.").await;
    let (status, _, page) = tap_approve(&f, &read).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(page.contains("nothing was approved"), "{page}");
    assert!(
        page.contains("Approve revision 2"),
        "the new one to read: {page}"
    );
    assert!(page.contains("<mark>Zanzibar</mark>"), "{page}");
    assert_eq!(approvals(&f.db).await, 0);

    // A wrong or missing hash of the right revision.
    for (source, facts) in [
        ("wrong", hidden(&page, "factsSha256")),
        (
            hidden(&page, "sourceSha256").leak() as &str,
            "wrong".to_owned(),
        ),
        ("", String::new()),
    ] {
        let (status, _, _) = post(
            &f.app,
            &format!("{VARIANT}/approve"),
            &[
                ("csrf", TOKEN),
                ("revision", "2"),
                ("sourceSha256", source),
                ("factsSha256", &facts),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{source} {facts}");
    }
    let (status, _, _) = post(
        &f.app,
        &format!("{VARIANT}/approve"),
        &[
            ("revision", "2"),
            ("sourceSha256", &hidden(&page, "sourceSha256")),
            ("factsSha256", &hidden(&page, "factsSha256")),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "no token, no approval");
    assert_eq!(approvals(&f.db).await, 0);
}

#[tokio::test]
async fn a_new_word_in_a_draft_is_shown_to_check() {
    let f = locked().await;
    agent_patch(
        &f,
        0,
        "Built the routing service.",
        "Built the Zanzibar service.",
    )
    .await;
    let (_, _, page) = get(&f.app, VARIANT).await;
    assert!(page.contains("<mark>Zanzibar</mark>"), "{page}");
    assert!(page.contains("Is it true?"), "{page}");
}

/// The owner's edit needs no second look: they are looking at it. An agent's text in the same field
/// wouldn't be: `origin` plays no part, only that this is the owner's form.
#[tokio::test]
async fn the_owners_save_approves_and_a_draft_pdf_is_stamped_and_labelled() {
    let f = locked().await;
    let (_, source) = head_of(&f).await;
    let draft = agent_patch(
        &f,
        0,
        "Built the routing service.",
        "Built the routing engine.",
    )
    .await;
    assert_eq!(draft["state"], "draft");
    let request = |query: &str| {
        Request::get(format!("{VARIANT}/cv.pdf?{query}"))
            .body(Body::empty())
            .unwrap()
    };
    let (status, headers, stamped) = send(&f.app, request("revision=1")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers["content-disposition"],
        "attachment; filename=\"cv-default-1-DRAFT-NOT-APPROVED.pdf\""
    );
    assert!(stamped.starts_with(b"%PDF-"));
    let (draft_status, _, draft_png) = image(&f, "revision=1&page=1").await;
    assert_eq!(draft_status, StatusCode::OK);

    let (_, _, page) = get(&f.app, VARIANT).await;
    assert_eq!(tap_approve(&f, &page).await.0, StatusCode::SEE_OTHER);
    let (_, headers, clean) = send(&f.app, request("revision=1")).await;
    assert_eq!(
        headers["content-disposition"],
        "attachment; filename=\"cv-default-1.pdf\""
    );
    assert_ne!(clean, stamped);
    let (_, _, clean_png) = image(&f, "revision=1&page=1").await;
    assert_ne!(
        clean_png, draft_png,
        "the stamp isn't served from the cache"
    );

    let edited = source.replace("Built the routing service.", "Built the routing core.");
    assert_eq!(save(&f, 1, &edited).await.0, StatusCode::SEE_OTHER);
    let (_, _, page) = get(&f.app, VARIANT).await;
    assert_eq!(badge(&page), "Approved");
    assert_eq!(approvals(&f.db).await, 2);
}

#[tokio::test]
async fn a_stale_base_is_a_conflict_that_keeps_the_text_and_then_lets_the_person_decide() {
    let f = locked().await;
    let (_, source) = head_of(&f).await;
    let first = source.replace("Built the routing service.", "Built the routing engine.");
    assert_eq!(save(&f, 0, &first).await.0, StatusCode::SEE_OTHER);

    let mine = source.replace("Built the routing service.", "Built the routing core.");
    let (status, _, page) = save(&f, 0, &mine).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(editor_text(&page), mine);
    assert!(page.contains("Revision 1 landed"), "{page}");
    assert_eq!(revision_rows(&f.db).await.len(), 1);
    assert_eq!(
        hidden(&page, "expected"),
        "1",
        "the page names the new head"
    );
    assert_eq!(save(&f, 1, &mine).await.0, StatusCode::SEE_OTHER);
    assert_eq!(revision_rows(&f.db).await[1].1, mine);
}

#[tokio::test]
async fn saving_the_same_text_or_with_unlocked_facts_says_why() {
    let f = fixture().await;
    let (_, source) = head_of(&f).await;
    let edited = format!("{source}\n");
    let (status, _, page) = save(&f, 0, &edited).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(page.contains("Facts page"), "{page}");
    assert_eq!(editor_text(&page), edited);
    assert!(revision_rows(&f.db).await.is_empty());
    lock(&f.db).await;
    // A variant's starting text can be stored as it stands: that makes it the owner's.
    assert_eq!(save(&f, 0, &source).await.0, StatusCode::SEE_OTHER);
    assert_eq!(revision_rows(&f.db).await.len(), 1);
    let (status, _, page) = save(&f, 1, &source).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(page.contains("nothing to save"), "{page}");
    assert_eq!(revision_rows(&f.db).await.len(), 1);
}

#[tokio::test]
async fn a_form_without_the_token_changes_nothing_on_any_route() {
    let f = locked().await;
    let (_, source) = head_of(&f).await;
    let changed = source.replace("Built", "Made");
    for csrf in ["", "wrong"] {
        let fields = [
            ("csrf", csrf),
            ("expected", "0"),
            ("source", changed.as_str()),
            ("to", "1"),
            ("confirm", "1"),
            ("languages", "Latin: fluent"),
        ];
        for route in [
            "/cv/variants/default/save",
            "/cv/variants/default/revert",
            "/cv/variants/default/approve",
            "/cv/facts",
            "/cv/facts/lock",
            "/cv/facts/unlock",
        ] {
            let (status, _, _) = post(&f.app, route, &fields).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{route}");
        }
    }
    assert!(revision_rows(&f.db).await.is_empty());
    let state = cv_facts::get(&mut f.db.conn().await.unwrap())
        .await
        .unwrap();
    assert!(state.is_locked());
    assert!(state.facts.languages.is_empty());
}

#[tokio::test]
async fn text_with_markup_stays_text_and_a_leading_newline_survives_the_editor() {
    let f = locked().await;
    let (_, source) = head_of(&f).await;
    let tricky = format!("\n\n{source}\n// </textarea><script>alert(1)</script> & \"quotes\"\n");
    assert_eq!(save(&f, 0, &tricky).await.0, StatusCode::SEE_OTHER);
    let (_, _, page) = get(&f.app, EDIT).await;
    assert!(!page.contains("<script>alert(1)"), "escaped everywhere");
    assert_eq!(editor_text(&page), tricky);
}

#[tokio::test]
async fn markup_in_the_facts_stays_text_in_their_editors() {
    let f = locked().await;
    let state = facts_row(&f.db).await;
    let version = state.version.to_string();
    let evil = "</textarea><script>alert(1)</script>";
    let (status, _, page) = post(
        &f.app,
        "/cv/facts",
        &[
            ("csrf", TOKEN),
            ("expected", version.as_str()),
            ("vocabulary", evil),
            ("bannedPhrases", evil),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!page.contains("<script>alert(1)"), "{page}");
    assert_eq!(facts_row(&f.db).await.facts.vocabulary, [evil]);
}

#[tokio::test]
async fn revert_adds_a_revision_with_the_old_text_and_refuses_a_stale_head() {
    let f = locked().await;
    let (_, source) = head_of(&f).await;
    let first = source.replace("Built the routing service.", "Built the routing engine.");
    let second = source.replace("Built the routing service.", "Built the routing core.");
    assert_eq!(save(&f, 0, &first).await.0, StatusCode::SEE_OTHER);
    assert_eq!(save(&f, 1, &second).await.0, StatusCode::SEE_OTHER);

    let (_, _, page) = get(&f.app, HISTORY).await;
    assert!(page.contains("Revert to 1"), "{page}");
    assert!(!page.contains("Revert to 2"), "not to the head itself");

    let revert = "/cv/variants/default/revert";
    let (status, _, page) = post(
        &f.app,
        revert,
        &[("csrf", TOKEN), ("expected", "1"), ("to", "1")],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{page}");
    assert!(
        page.contains("data-view=\"history\""),
        "back where the revisions are: {page}"
    );
    assert_eq!(revision_rows(&f.db).await.len(), 2);

    let (status, headers, _) = post(
        &f.app,
        revert,
        &[("csrf", TOKEN), ("expected", "2"), ("to", "1")],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers["location"], "/cv/variants/default?done=reverted");
    let rows = revision_rows(&f.db).await;
    assert_eq!(rows.len(), 3);
    assert_eq!(
        (rows[2].1.as_str(), rows[2].2.as_str()),
        (first.as_str(), "revert")
    );
}

fn is_png(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
}

async fn image(f: &Fixture, query: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
    let request = Request::get(format!("{VARIANT}/preview.png?{query}"))
        .body(Body::empty())
        .unwrap();
    send(&f.app, request).await
}

fn cache_entries(f: &Fixture) -> Vec<String> {
    std::fs::read_dir(f.dir.path().join("cache/renders"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_preview_waiting_behind_another_compile_gives_up_in_words() {
    use std::os::unix::fs::PermissionsExt;
    let f = locked().await;
    let worker = f.dir.path().join("hanging-worker");
    std::fs::write(&worker, "#!/bin/sh\nexec /usr/bin/sleep 60\n").unwrap();
    std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o755)).unwrap();
    let hanging = McpState::new(
        f.db.clone(),
        "owner-1".to_owned(),
        "owner@example.com".to_owned(),
        nexus_mcp::Renderer::Subprocess {
            exe: worker,
            rss_limit_bytes: 512 * 1024 * 1024,
            timeout: Duration::from_secs(20),
        },
    );
    let app = build(&hanging, &f.dir, None);
    let uri = format!("{VARIANT}/preview.json?revision=0");
    let compiling = tokio::spawn({
        let (app, uri) = (app.clone(), uri.clone());
        async move { get(&app, &uri).await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let started = Instant::now();
    let (status, _, json) = get(&app, &uri).await;
    let waited = started.elapsed();
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{json}");
    assert!(json.contains("Other CVs are rendering"), "{json}");
    assert!(waited < Duration::from_secs(8), "{waited:?}");
    assert!(!compiling.is_finished(), "the compile itself goes on");
    let request = Request::get(format!("{VARIANT}/preview.png?revision=0"))
        .body(Body::empty())
        .unwrap();
    let (status, _, text) = send(&app, request).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(String::from_utf8_lossy(&text).contains("Other CVs are rendering"));
    compiling.abort();
}

#[tokio::test]
async fn previews_come_from_the_cache_until_the_source_or_the_facts_change() {
    let f = locked().await;
    let (status, headers, png) = image(&f, "revision=0&page=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/png");
    assert_eq!(headers["cache-control"], "no-store");
    assert!(is_png(&png));
    let entries = cache_entries(&f);
    assert_eq!(entries.len(), 1);

    // Served from the entry even if it is altered, which only a cache hit can do.
    let entry = f.dir.path().join("cache/renders").join(&entries[0]);
    let altered = [&5u32.to_le_bytes()[..], b"fake!"].concat();
    std::fs::write(&entry, altered).unwrap();
    let (status, _, again) = image(&f, "revision=0&page=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again, b"fake!");
    std::fs::remove_file(&entry).unwrap();
    assert_eq!(
        image(&f, "revision=0&page=1").await.2,
        png,
        "anything may delete it"
    );

    let (status, _, _) = image(&f, "revision=0&page=9").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, _, json) = get(&f.app, &format!("{VARIANT}/preview.json?revision=0")).await;
    assert_eq!(serde_json::from_str::<Value>(&json).unwrap()["pages"], 1);

    let (expected, source) = head_of(&f).await;
    let edited = source.replace("Built the routing service.", "Built the routing engine.");
    assert_eq!(save(&f, expected, &edited).await.0, StatusCode::SEE_OTHER);
    let (status, _, next) = image(&f, "revision=1&page=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(next, png, "another source, another image");
    assert_eq!(cache_entries(&f).len(), 2);

    // A changed facts version renders again rather than trusting the old check.
    let mut tx = f.db.begin_write().await.unwrap();
    cv_facts::set_owner_facts(&mut tx, &Default::default(), "test")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(image(&f, "revision=1&page=1").await.0, StatusCode::OK);
    assert_eq!(cache_entries(&f).len(), 3);
}

#[tokio::test]
async fn a_revision_that_stops_passing_shows_no_preview_and_no_pdf() {
    let f = locked().await;
    let (_, source) = head_of(&f).await;
    let extra = source.replace("Built the routing service.", "Built frobnicators.");
    assert_eq!(save(&f, 0, &extra).await.0, StatusCode::SEE_OTHER);
    assert_eq!(image(&f, "revision=1&page=1").await.0, StatusCode::OK);

    let mut tx = f.db.begin_write().await.unwrap();
    let banned = cv_facts::OwnerFacts {
        banned_phrases: Some(vec!["frobnicators".to_owned()]),
        ..Default::default()
    };
    cv_facts::set_owner_facts(&mut tx, &banned, "test")
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let (status, _, _) = image(&f, "revision=1&page=1").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _, json) = get(&f.app, &format!("{VARIANT}/preview.json?revision=1")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(body["tokens"][0], "frobnicators");
    let (status, _, page) = get(&f.app, &format!("{VARIANT}/cv.pdf?revision=1")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(page.contains("No PDF for this revision"), "{page}");
}

#[tokio::test]
async fn an_absurd_page_number_is_only_an_empty_page() {
    let f = locked().await;
    let (status, _, _) = get(&f.app, "/cv/variants/default?page=9223372036854775807").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn the_pdf_downloads_as_an_attachment() {
    let f = locked().await;
    let request = Request::get(format!("{VARIANT}/cv.pdf?revision=0"))
        .body(Body::empty())
        .unwrap();
    let (status, headers, bytes) = send(&f.app, request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/pdf");
    assert_eq!(
        headers["content-disposition"],
        "attachment; filename=\"cv-default-0-DRAFT-NOT-APPROVED.pdf\""
    );
    assert!(bytes.starts_with(b"%PDF-"));
    assert_eq!(
        get(&f.app, "/cv/variants/nope/cv.pdf").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&f.app, "/cv/variants/nope").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&f.app, "/api/cv/nope/head").await.0,
        StatusCode::NOT_FOUND
    );
    let (status, _, head) = get(&f.app, "/api/cv/default/head").await;
    assert_eq!(status, StatusCode::OK);
    let head = serde_json::from_str::<Value>(&head).unwrap();
    assert_eq!(head["revision"], 0);
    assert_eq!(head["state"], "draft");
}

#[tokio::test]
async fn every_writer_announces_its_revision_after_commit() {
    let f = locked().await;
    let mut events = f.state.cv_events.subscribe();
    let (_, source) = head_of(&f).await;
    let edited = source.replace("Built the routing service.", "Built the routing engine.");
    assert_eq!(save(&f, 0, &edited).await.0, StatusCode::SEE_OTHER);
    let event = events.recv().await.unwrap();
    assert_eq!((event.variant.as_str(), event.revision), ("default", 1));
    assert_eq!(event.origin, "dashboard");

    // At the moment of the announcement the revision can be read back.
    assert_eq!(revision_rows(&f.db).await.len(), 1);
    let revert = "/cv/variants/default/revert";
    let refused = save(&f, 0, &source).await;
    assert_eq!(refused.0, StatusCode::CONFLICT);
    assert!(events.try_recv().is_err(), "a refused write is silent");
    assert_eq!(
        post(
            &f.app,
            revert,
            &[("csrf", TOKEN), ("expected", "1"), ("to", "1")]
        )
        .await
        .0,
        StatusCode::SEE_OTHER
    );
    assert_eq!(events.recv().await.unwrap().origin, "revert");
}

#[tokio::test]
async fn the_microphone_and_openai_are_allowed_only_on_a_variant_page_with_voice_on() {
    let f = locked().await;
    let on = build(&f.state, &f.dir, Some(voice()));

    let (_, headers, page) = get(&on, VARIANT).await;
    let csp = headers["content-security-policy"].to_str().unwrap();
    assert!(
        csp.contains("connect-src 'self' https://api.openai.com"),
        "{csp}"
    );
    assert!(csp.contains("media-src 'self'"), "{csp}");
    assert!(
        csp.contains("script-src 'self'") && !csp.contains("unsafe"),
        "{csp}"
    );
    assert_eq!(headers["permissions-policy"], "microphone=(self)");
    assert!(page.contains("/assets/voice.js"));
    assert!(page.contains("/assets/cv.js"));
    assert!(!page.contains("sk-synthetic"), "the key isn't in the page");

    let (_, headers, page) = get(&on, "/assistant").await;
    let csp = headers["content-security-policy"].to_str().unwrap();
    assert!(
        csp.contains("connect-src 'self' https://api.openai.com"),
        "{csp}"
    );
    assert_eq!(headers["permissions-policy"], "microphone=(self)");
    assert!(page.contains("/assets/voice.js"));
    assert!(
        page.contains("aria-current=\"page\">Assistant</a>"),
        "{page}"
    );
    assert!(page.contains("Nexus workspace"), "{page}");
    assert!(!page.contains("Working with"));
    assert!(!page.contains("No page context"));

    for path in ["/cv", "/cv/facts"] {
        let (_, headers, page) = get(&on, path).await;
        let csp = headers["content-security-policy"].to_str().unwrap();
        assert!(!csp.contains("connect-src"), "{path}: {csp}");
        assert_eq!(headers["permissions-policy"], "microphone=()", "{path}");
        assert!(!page.contains("voice.js"), "{path}");
    }

    let (_, headers, page) = get(&f.app, VARIANT).await;
    let csp = headers["content-security-policy"].to_str().unwrap();
    assert!(csp.contains("connect-src 'self';"), "{csp}");
    assert!(
        !csp.contains("openai") && !csp.contains("media-src"),
        "{csp}"
    );
    assert_eq!(headers["permissions-policy"], "microphone=()");
    assert!(page.contains("id=\"voice-off\""), "{page}");
    assert!(!page.contains("voice.js"), "voice isn't loaded when off");
    assert!(page.contains("/assets/cv.js"));
}

#[tokio::test]
async fn voice_controls_and_history_are_separate_and_states_are_accessible() {
    let f = locked().await;
    let on = build(&f.state, &f.dir, Some(voice()));
    for path in ["/assistant", VARIANT] {
        let (_, _, page) = get(&on, path).await;
        for id in [
            "voice-phase",
            "voice-mic-state",
            "voice-hold-label",
            "voice-interrupt",
            "voice-transcript-empty",
            "voice-tools-empty",
        ] {
            assert!(page.contains(&format!("id=\"{id}\"")), "{path}: {id}");
        }
        assert!(page.contains("data-phase=\"idle\""));
        assert!(page.contains("Microphone off"));
        assert!(page.contains("End conversation"));
        assert!(page.contains("aria-relevant=\"additions\""));
        assert!(page.contains("class=\"voice-history\""));
        let history = page.split("class=\"voice-history\"").nth(1).unwrap();
        assert!(history.contains("id=\"voice-transcript\""));
        assert!(history.contains("id=\"voice-tools\""));
        assert!(!history.contains("id=\"voice-live\""));
    }
}

#[tokio::test]
async fn no_page_has_inline_script_or_style() {
    let f = locked().await;
    let on = build(&f.state, &f.dir, Some(voice()));
    for path in ["/cv", VARIANT, "/cv/facts", "/assistant"] {
        let (_, _, page) = get(&on, path).await;
        assert!(!page.contains(" style="), "{path}");
        assert!(!page.contains(" onclick"), "{path}");
        for script in page.split("<script").skip(1) {
            assert!(script.trim_start().starts_with("src="), "{path}: {script}");
        }
    }
}

#[tokio::test]
async fn assistant_validates_and_pins_only_record_ids_as_page_context() {
    let f = locked().await;
    let created = nexus_mcp::call_tool(
        std::sync::Arc::new(f.state.clone()),
        &[SCOPE_TOOLS],
        "test",
        "create_application",
        json!({
            "company": "Example & Partners",
            "role": "Platform <Engineer>",
            "status": "inbound"
        }),
    )
    .await;
    assert_ne!(created.is_error, Some(true));
    let created = created.structured_content.unwrap();
    let id = created["id"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| created["id"].as_i64().map(|id| id.to_string()))
        .unwrap();
    let on = build(&f.state, &f.dir, Some(voice()));

    let (_, _, application) = get(&on, &format!("/assistant?applicationId={id}")).await;
    assert!(
        application.contains(&format!("data-application-id=\"{id}\"")),
        "{application}"
    );
    assert!(application.contains("Application: Example &amp; Partners"));
    assert!(application.contains("Platform &lt;Engineer&gt;"));
    assert!(!application.contains("data-variant=\"default\""));

    let (_, _, variant) = get(&on, "/assistant?variant=default").await;
    assert!(variant.contains("CV: default"));
    assert!(variant.contains("data-variant=\"default\""));
    assert!(!variant.contains("data-application-id"));

    for uri in [
        "/assistant?applicationId=999999".to_owned(),
        format!("/assistant?applicationId={id}&variant=default"),
    ] {
        let (_, _, page) = get(&on, &uri).await;
        assert!(page.contains("Nexus workspace"), "{uri}: {page}");
        assert!(!page.contains("data-application-id"), "{uri}: {page}");
        assert!(!page.contains("data-variant=\"default\""), "{uri}: {page}");
    }
}

async fn facts_row(db: &Db) -> cv_facts::FactsState {
    cv_facts::get(&mut db.conn().await.unwrap()).await.unwrap()
}

async fn history(db: &Db) -> Vec<(String, String)> {
    sqlx::query_as("SELECT change, changed_by FROM cv_facts_history ORDER BY rowid")
        .fetch_all(db.pool())
        .await
        .unwrap()
}

async fn lock_on_page(f: &Fixture, page: &str) -> (StatusCode, HeaderMap, String) {
    post(
        &f.app,
        "/cv/facts/lock",
        &[("csrf", TOKEN), ("hash", &hidden(page, "hash"))],
    )
    .await
}

#[tokio::test]
async fn locking_on_the_page_shows_every_change_and_records_who() {
    let f = fixture().await;
    let (_, _, page) = get(&f.app, "/cv/facts").await;
    assert!(page.contains("Mara Example") && page.contains("Brightpath Logistics"));
    assert!(page.contains("Lock the facts"));
    assert!(page.contains("Facts changed"), "{page}");
    assert!(page.contains("Wording changed"), "{page}");
    assert!(page.contains("Cut invoice errors by 40%"), "{page}");
    let (status, _, locked) = lock_on_page(&f, &page).await;
    assert_eq!(status, StatusCode::OK);
    assert!(locked.contains("Unlock"), "{locked}");
    assert!(facts_row(&f.db).await.is_locked());
    assert_eq!(
        history(&f.db).await,
        [
            ("upsert_cv_profile".to_owned(), "mcp".to_owned()),
            ("lock".to_owned(), "dashboard".to_owned())
        ]
    );

    // A second tap, with the page as it was.
    let (status, _, again) = lock_on_page(&f, &page).await;
    assert_eq!(status, StatusCode::OK);
    assert!(again.contains("already locked"), "{again}");
}

/// The hole this closes: an agent's unlocked writes confirmed by one tap on a stale page.
#[tokio::test]
async fn a_lock_from_a_stale_page_is_refused_and_shows_what_was_written() {
    let f = fixture().await;
    let (_, _, stale) = get(&f.app, "/cv/facts").await;

    let mut rewritten = profile();
    rewritten["experience"][0]["title"] = "Principal Engineer".into();
    let result = nexus_mcp::call_tool(
        std::sync::Arc::new(f.state.clone()),
        &[SCOPE_TOOLS],
        "mcp",
        "upsert_cv_profile",
        rewritten,
    )
    .await;
    assert_ne!(result.is_error, Some(true));

    let (status, _, page) = lock_on_page(&f, &stale).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(!facts_row(&f.db).await.is_locked(), "nothing locked");
    assert!(page.contains("nothing was locked"), "{page}");
    assert!(
        page.contains("Principal Engineer"),
        "the new change shows: {page}"
    );

    let (status, _, _) = post(&f.app, "/cv/facts/lock", &[("csrf", TOKEN)]).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a form naming nothing locks nothing"
    );
    assert!(!facts_row(&f.db).await.is_locked());

    let (status, _, _) = lock_on_page(&f, &page).await;
    assert_eq!(status, StatusCode::OK);
    assert!(facts_row(&f.db).await.is_locked());
}

#[tokio::test]
async fn locking_with_no_name_is_refused_in_words() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).await.unwrap();
    let state = McpState::new(
        db.clone(),
        "o".to_owned(),
        "o@example.test".to_owned(),
        nexus_mcp::Renderer::InProcess,
    );
    let app = build(&state, &dir, None);
    let (_, _, page) = get(&app, "/cv/facts").await;
    assert!(page.contains("nothing to lock"), "{page}");
    assert!(!page.contains("Lock the facts"), "{page}");
    let (_, _, page) = post(&app, "/cv/facts/lock", &[("csrf", TOKEN)]).await;
    assert!(page.contains("nothing to lock"), "{page}");
    assert!(!facts_row(&db).await.is_locked());
}

#[tokio::test]
async fn unlocking_asks_first() {
    let f = locked().await;
    let (status, _, page) = post(&f.app, "/cv/facts/unlock", &[("csrf", TOKEN)]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Unlock the facts?"), "{page}");
    assert!(page.contains("name=\"confirm\""));
    assert!(facts_row(&f.db).await.is_locked(), "not yet");

    let (status, _, page) = post(
        &f.app,
        "/cv/facts/unlock",
        &[("csrf", TOKEN), ("confirm", "1")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("are unlocked"), "{page}");
    assert!(!facts_row(&f.db).await.is_locked());
    assert_eq!(history(&f.db).await.last().unwrap().0, "unlock");
}

#[tokio::test]
async fn editing_sets_only_what_nexus_cv_facts_set_can_and_keeps_typing_on_a_refusal() {
    let f = locked().await;
    let before = facts_row(&f.db).await;
    let version = before.version.to_string();
    let good = [
        ("csrf", TOKEN),
        ("expected", version.as_str()),
        ("languages", "German: C1\r\nEnglish: native\r\n"),
        ("metrics", "invoices: Cut invoice errors: by 40%"),
        ("allowedTokens", "40%\n3\n2015\n"),
        ("vocabulary", "Kubernetes"),
        ("bannedPhrases", "team player"),
        // Not fields of this form: a crafted post must not reach them.
        ("name", "Someone Else"),
        ("employment", "[]"),
        ("lockedAt", ""),
    ];
    let (status, _, page) = post(&f.app, "/cv/facts", &good).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Facts saved."), "{page}");

    let after = facts_row(&f.db).await;
    assert_eq!(after.version, before.version + 1);
    assert_eq!(after.facts.name, "Mara Example");
    assert_eq!(after.facts.employment, before.facts.employment);
    assert!(after.is_locked());
    assert_eq!(after.facts.languages.len(), 2);
    assert_eq!(after.facts.languages[1].name, "English");
    assert_eq!(after.facts.metrics[0].key, "invoices");
    assert_eq!(after.facts.metrics[0].text, "Cut invoice errors: by 40%");
    assert_eq!(after.facts.allowed_tokens, ["40%", "3", "2015"]);
    assert_eq!(after.facts.banned_phrases, ["team player"]);
    assert_eq!(history(&f.db).await.last().unwrap().1, "dashboard");

    let (status, _, page) = post(&f.app, "/cv/facts", &good).await;
    assert_eq!(status, StatusCode::CONFLICT, "the old version");
    assert!(page.contains("changed since you opened"), "{page}");
    assert_eq!(facts_row(&f.db).await.version, after.version);

    let current = after.version.to_string();
    let bad = [
        ("csrf", TOKEN),
        ("expected", current.as_str()),
        ("languages", "German without a level"),
        ("vocabulary", "Keep me"),
    ];
    let (status, _, page) = post(&f.app, "/cv/facts", &bad).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(page.contains("German without a level"));
    assert!(page.contains("Keep me"), "what was typed stays");
    assert_eq!(facts_row(&f.db).await.version, after.version);
}

/// Saves the starting text, then approves it: an approved revision 1 to work from.
async fn approved_start(f: &Fixture) -> String {
    let (_, source) = head_of(f).await;
    assert_eq!(save(f, 0, &source).await.0, StatusCode::SEE_OTHER);
    let (_, _, page) = get(&f.app, VARIANT).await;
    let (status, _, _) = post(
        &f.app,
        &format!("{VARIANT}/approve"),
        &[
            ("csrf", TOKEN),
            ("revision", &hidden(&page, "revision")),
            ("sourceSha256", &hidden(&page, "sourceSha256")),
            ("factsSha256", &hidden(&page, "factsSha256")),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    source
}

async fn agent_call(f: &Fixture, tool: &str, args: Value) -> nexus_mcp::CallToolResult {
    nexus_mcp::call_tool(
        std::sync::Arc::new(f.state.clone()),
        &[SCOPE_TOOLS],
        "test",
        tool,
        args,
    )
    .await
}

/// Finding 6, as the owner sees it: the button says what a save does, and it does that.
#[tokio::test]
async fn the_save_button_says_whether_it_approves_and_a_draft_save_stays_a_draft() {
    let f = locked().await;
    let (_, _, page) = get(&f.app, VARIANT).await;
    assert!(page.contains(">Save draft<"), "nothing is approved yet");
    assert!(!page.contains("Save and approve"), "{page}");

    let (_, source) = head_of(&f).await;
    let (status, headers, _) = save(&f, 0, &source).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_, _, page) = get(&f.app, headers["location"].to_str().unwrap()).await;
    assert_eq!(badge(&page), "Draft, not approved");
    assert!(page.contains("Saved as a draft"), "{page}");
    assert!(!page.contains("Saved and approved"), "{page}");
    assert_eq!(approvals(&f.db).await, 0);

    let (_, _, page) = get(&f.app, VARIANT).await;
    let (status, _, _) = post(
        &f.app,
        &format!("{VARIANT}/approve"),
        &[
            ("csrf", TOKEN),
            ("revision", &hidden(&page, "revision")),
            ("sourceSha256", &hidden(&page, "sourceSha256")),
            ("factsSha256", &hidden(&page, "factsSha256")),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_, _, page) = get(&f.app, VARIANT).await;
    assert!(
        page.contains(">Save and approve as a new revision<"),
        "{page}"
    );

    let edited = source.replace("Built the routing service.", "Built the routing engine.");
    let (status, headers, _) = save(&f, 1, &edited).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_, _, page) = get(&f.app, headers["location"].to_str().unwrap()).await;
    assert_eq!(badge(&page), "Approved");
    assert!(page.contains("Saved and approved"), "{page}");
}

/// An agent's draft under an owner's old page: the stale editor's save is the conflict it always was.
#[tokio::test]
async fn a_stale_editor_gets_the_conflict_and_never_approves_what_landed() {
    let f = locked().await;
    let source = approved_start(&f).await;
    let draft = source.replace(
        "Built the routing service.",
        "Holds a Doctorate from Stanford.",
    );
    let landed = agent_call(
        &f,
        "patch_cv",
        json!({"expectedRevision": 1, "summary": "small fix",
            "edits": [{"oldText": source, "newText": draft}]}),
    )
    .await;
    assert_ne!(landed.is_error, Some(true));
    let edited = source.replace("Backend engineer", "Backend developer");
    let (status, _, page) = save(&f, 1, &edited).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(page.contains("landed while you were working"), "{page}");
    assert_eq!(hidden(&page, "expected"), "2");
    assert!(
        page.contains(">Save draft<"),
        "the base is now a draft: {page}"
    );
    assert_eq!(approvals(&f.db).await, 1);
    assert_eq!(revision_rows(&f.db).await.len(), 2);
}

/// Finding 9: a change only in how text looks never shows Approve under an empty panel.
#[tokio::test]
async fn a_change_in_looks_says_so_above_the_button() {
    let f = locked().await;
    let source = approved_start(&f).await;
    let (_, _, page) = get(&f.app, VARIANT).await;
    assert!(!page.contains("cv-looks-only"), "approved: nothing to say");
    let changed = source.replace("#education(\"edu-1\")", "#strike[#education(\"edu-1\")]");
    assert_ne!(changed, source);
    let landed = agent_call(
        &f,
        "patch_cv",
        json!({"expectedRevision": 1, "summary": "tidy",
            "edits": [{"oldText": source, "newText": changed}]}),
    )
    .await;
    assert_ne!(landed.is_error, Some(true));
    let (_, _, page) = get(&f.app, VARIANT).await;
    assert!(page.contains("id=\"cv-looks-only\""), "{page}");
    assert!(page.contains("The wording is unchanged"), "{page}");
    let line = page.find("id=\"cv-looks-only\"").unwrap();
    assert!(
        line < page.find("class=\"approve-form\"").unwrap(),
        "above the button"
    );
    assert!(page.contains("#strike"), "the source diff is on the page");

    // With a real wording change, nothing is claimed about looks.
    let (_, now) = head_of(&f).await;
    let worded = now.replace("Built the routing service.", "Built the routing engine.");
    let landed = agent_call(
        &f,
        "patch_cv",
        json!({"expectedRevision": 2, "summary": "tidy",
            "edits": [{"oldText": now, "newText": worded}]}),
    )
    .await;
    assert_ne!(landed.is_error, Some(true));
    let (_, _, page) = get(&f.app, VARIANT).await;
    assert!(!page.contains("cv-looks-only"), "{page}");
}

/// Finding 16: a link an agent sends can't make the page claim what didn't happen.
#[tokio::test]
async fn a_done_banner_shows_only_when_the_state_agrees() {
    let f = locked().await;
    let source = approved_start(&f).await;
    let draft = source.replace("Built the routing service.", "Built the routing engine.");
    let landed = agent_call(
        &f,
        "patch_cv",
        json!({"expectedRevision": 1, "summary": "Edited by hand",
            "edits": [{"oldText": source, "newText": draft}]}),
    )
    .await;
    assert_ne!(landed.is_error, Some(true));
    for done in ["approved", "saved", "reverted"] {
        let (_, _, page) = get(&f.app, &format!("{VARIANT}?done={done}")).await;
        assert_eq!(badge(&page), "Draft, not approved", "{done}");
        for claim in [
            "is approved.",
            "Saved and approved",
            "Saved as a draft",
            "Reverted:",
        ] {
            assert!(!page.contains(claim), "{done}: {claim}");
        }
    }
    // On approved text an agent reached by a revert, there is still no saving to speak of.
    let undone = agent_call(
        &f,
        "revert_cv",
        json!({"toRevision": 1, "expectedRevision": 2}),
    )
    .await;
    assert_ne!(undone.is_error, Some(true));
    let (_, _, page) = get(&f.app, &format!("{VARIANT}?done=saved")).await;
    assert_eq!(badge(&page), "Approved");
    assert!(!page.contains("Saved and approved"), "{page}");
    let (_, _, page) = get(&f.app, &format!("{VARIANT}?done=reverted")).await;
    assert!(page.contains("Reverted:"), "true here: {page}");
}

/// The dashboard's revert to replaced text is a draft too, and its banner says what to do next.
#[tokio::test]
async fn reverting_to_replaced_text_on_the_dashboard_asks_for_approval() {
    let f = locked().await;
    let source = approved_start(&f).await;
    let edited = source.replace("Built the routing service.", "Built the routing engine.");
    assert_eq!(save(&f, 1, &edited).await.0, StatusCode::SEE_OTHER);
    let (status, headers, _) = post(
        &f.app,
        &format!("{VARIANT}/revert"),
        &[("csrf", TOKEN), ("expected", "2"), ("to", "1")],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_, _, page) = get(&f.app, headers["location"].to_str().unwrap()).await;
    assert_eq!(badge(&page), "Draft, not approved");
    assert!(page.contains("Reverted:"), "{page}");
    assert!(page.contains("approve it"), "{page}");
    assert!(page.contains("Compared with revision 2"), "{page}");
}

/// A revision's state can now flip with no change of facts, so its clean picture must not outlive it.
#[tokio::test]
async fn a_cached_clean_preview_is_not_served_for_text_since_replaced() {
    let f = locked().await;
    let source = approved_start(&f).await;
    let png = format!("{VARIANT}/preview.png?revision=1&page=1");
    let picture = async |uri: &str| {
        let request = Request::get(uri).body(Body::empty()).unwrap();
        send(&f.app, request).await.2
    };
    let clean = picture(&png).await;
    let edited = source.replace("Built the routing service.", "Built the routing engine.");
    assert_eq!(save(&f, 1, &edited).await.0, StatusCode::SEE_OTHER);
    assert_ne!(picture(&png).await, clean, "revision 1 has a stamp again");
}

/// Fields of the page's own editor form, as a browser sends them.
fn editor_fields(page: &str) -> Vec<(String, String)> {
    let form = &page[page.find("id=\"cv-editor-form\"").unwrap()..];
    let form = &form[..form.find("</form>").unwrap()];
    let mut fields: Vec<(String, String)> = form
        .split("<input type=\"hidden\"")
        .skip(1)
        .map(|input| {
            let attribute = |name: &str| {
                let from = input.find(&format!("{name}=\"")).unwrap() + name.len() + 2;
                unescape(input[from..].split('"').next().unwrap())
            };
            (attribute("name"), attribute("value"))
        })
        .collect();
    fields.push(("source".to_owned(), editor_text(page)));
    fields
}

async fn send_form(f: &Fixture, fields: &[(String, String)]) -> (StatusCode, HeaderMap, String) {
    let fields: Vec<(&str, &str)> = fields
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    post(&f.app, &format!("{VARIANT}/save"), &fields).await
}

/// A conflict moves the form to the new head and keeps the typed text, so the form's base no longer
/// says where that text came from. It stores a draft, even if the new head is approved.
#[tokio::test]
async fn a_form_sent_after_a_conflict_stores_a_draft_on_any_base() {
    let f = locked().await;
    approved_start(&f).await;
    agent_patch(
        &f,
        1,
        "Built the routing service.",
        "Holds a Doctorate from Stanford.",
    )
    .await;
    let (_, _, page) = get(&f.app, EDIT).await;
    assert!(page.contains(">Save draft<"), "{page}");
    let mut fields = editor_fields(&page);
    let typed = editor_text(&page).replace("Backend engineer", "Backend developer");
    fields.last_mut().unwrap().1 = typed.clone();

    let reverted = agent_call(
        &f,
        "revert_cv",
        json!({"toRevision": 1, "expectedRevision": 2}),
    )
    .await;
    assert_eq!(reverted.structured_content.unwrap()["state"], "approved");
    let approvals_before = approvals(&f.db).await;

    let (status, _, page) = send_form(&f, &fields).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(editor_text(&page), typed);
    assert_eq!(hidden(&page, "expected"), "3");
    assert!(page.contains(">Save draft<"), "{page}");
    assert!(!page.contains("Save and approve"), "{page}");

    // The form sent again and refused for its text stays what it was.
    let mut broken = editor_fields(&page);
    broken.last_mut().unwrap().1 = format!("{typed}\n#nonsense(");
    let (status, _, refused) = send_form(&f, &broken).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(refused.contains(">Save draft<"), "{refused}");

    let (status, headers, _) = send_form(&f, &editor_fields(&page)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_, _, page) = get(&f.app, headers["location"].to_str().unwrap()).await;
    assert_eq!(badge(&page), "Draft, not approved");
    assert!(page.contains("Saved as a draft"), "{page}");
    assert!(
        !page.contains("it started from text that isn't approved"),
        "its base was approved: {page}"
    );
    assert_eq!(approvals(&f.db).await, approvals_before);
}

/// A form that doesn't say its text came from its base stores a draft: a lost mark can approve nothing.
#[tokio::test]
async fn a_form_without_the_mark_of_its_base_stores_a_draft() {
    let f = locked().await;
    let source = approved_start(&f).await;
    let edited = source.replace("Built the routing service.", "Built the routing engine.");
    let approvals_before = approvals(&f.db).await;
    let (status, headers, _) = post(
        &f.app,
        &format!("{VARIANT}/save"),
        &[
            ("csrf", TOKEN),
            ("expected", "1"),
            ("source", &edited),
            ("summary", "x"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_, _, page) = get(&f.app, headers["location"].to_str().unwrap()).await;
    assert_eq!(badge(&page), "Draft, not approved");
    assert_eq!(approvals(&f.db).await, approvals_before);
}

/// A browser may give a reloaded or revisited page the text typed before it, under the new page's base.
#[tokio::test]
async fn the_editor_form_keeps_no_typing_across_a_reload() {
    let f = locked().await;
    approved_start(&f).await;
    let (_, _, page) = get(&f.app, VARIANT).await;
    let opening = page[..page.find("id=\"cv-editor-form\"").unwrap()]
        .rfind("<form")
        .unwrap();
    let tag = page[opening..].split('>').next().unwrap();
    assert!(tag.contains("autocomplete=\"off\""), "{tag}");
}

/// Finding 5: one changed word elsewhere must not fold what shows a change in looks.
#[tokio::test]
async fn a_draft_says_above_the_button_when_its_source_goes_beyond_its_words() {
    let f = locked().await;
    approved_start(&f).await;
    agent_patch(
        &f,
        1,
        "Built the routing service.",
        "Built the #strike[routing] service well.",
    )
    .await;
    let (_, _, page) = get(&f.app, VARIANT).await;
    assert!(!page.contains("cv-looks-only"), "words changed too: {page}");
    // Words changed, and so did something they don't show: a line above the button says so.
    let line = page
        .find("id=\"cv-beyond-text\"")
        .unwrap_or_else(|| panic!("{page}"));
    assert!(
        line < page.find("class=\"approve-form\"").unwrap(),
        "{page}"
    );
    assert!(page.contains("#strike[routing]"), "{page}");
}

/// Finding 11: a one-letter word marks itself, and not every word that holds its letter.
#[tokio::test]
async fn a_word_is_marked_where_it_stands_whole() {
    let f = locked().await;
    approved_start(&f).await;
    agent_patch(
        &f,
        1,
        "Built the routing service.",
        "I am certified as a logistics auditor in Italy.",
    )
    .await;
    let (_, _, page) = get(&f.app, VARIANT).await;
    assert!(page.contains(">I</mark>"), "{page}");
    assert!(
        !page.contains(">i</mark>"),
        "marked inside logistics: {page}"
    );
}

/// The approval screen: the picture before the button, and text that says what it is.
#[tokio::test]
async fn the_approval_screen_shows_the_picture_before_the_button() {
    let f = locked().await;
    approved_start(&f).await;
    agent_patch(
        &f,
        1,
        "Built the routing service.",
        "I am #strike[not] certified.",
    )
    .await;
    let (_, _, page) = get(&f.app, VARIANT).await;
    assert!(!page.contains("What a reader sees"), "{page}");
    assert!(page.contains("What the text says"), "{page}");
    // In the page's own order, which is what a screen reader and a page without scripts follow: the
    // picture, what to check, then the button.
    let picture = page
        .find("preview.png?revision=2&amp;page=1")
        .unwrap_or_else(|| panic!("no picture: {page}"));
    let read = page.find("Read the picture").unwrap();
    // It is part of the review view itself, on any screen: no other view or tab holds it.
    let view = page.find("id=\"cv-workspace\"");
    assert_eq!(view_of(&page), "review");
    assert!(view.is_some_and(|view| view < read), "{page}");
    assert!(
        page[read..].contains("id=\"cv-approval\""),
        "the panel follows it: {page}"
    );
    let items = page.find("id=\"cv-review-items\"").unwrap();
    let button = page.find("class=\"approve-form\"").unwrap();
    assert!(
        read < picture && picture < items && items < button,
        "{page}"
    );
    assert!(
        page[..picture]
            .rfind("<a ")
            .is_some_and(|tap| page[tap..picture].contains("href=")),
        "a tap opens the page at full size: {page}"
    );
    // Once, at a size to read: not small and large, and not again as a preview.
    assert_eq!(page.matches("<img").count(), 1, "{page}");
    assert!(!page.contains("id=\"cv-preview-pages\""), "{page}");
    let panel = &page[page.find("id=\"cv-approval\"").unwrap()..];
    let button = panel.find("class=\"approve-form\"").unwrap();
    // The text and the source are collapsed, below the button: they no longer push it down.
    let text = panel.find("id=\"cv-text-details\"").unwrap();
    let source = panel.find("id=\"cv-source-details\"").unwrap();
    assert!(button < text && button < source, "{panel}");
    for collapsed in ["cv-text-details", "cv-source-details"] {
        assert!(
            !panel.contains(&format!("id=\"{collapsed}\" open")),
            "{panel}"
        );
    }
}

/// A long first approval keeps its button close: only the most serious items stand open.
#[tokio::test]
async fn a_long_list_of_items_does_not_push_the_button_down() {
    let f = locked().await;
    approved_start(&f).await;
    let many: String = (0..80)
        .map(|i| format!("Handled batch number {i}0 of 7{i}.\\\n"))
        .collect();
    agent_patch(&f, 1, "Built the routing service.", &many).await;
    let (_, _, page) = get(&f.app, VARIANT).await;
    let panel = &page[page.find("id=\"cv-approval\"").unwrap()..];
    let (open, closed) = (
        panel
            .find("id=\"cv-review-items\"")
            .unwrap_or_else(|| panic!("{panel}")),
        panel
            .find("id=\"cv-more-items\"")
            .unwrap_or_else(|| panic!("{panel}")),
    );
    let button = panel.find("class=\"approve-form\"").unwrap();
    assert!(open < closed && closed < button, "{panel}");
    // Whole lines stand open, up to the first that brings the words to 30.
    let words = panel[open..closed].matches("aria-describedby").count();
    assert!((30..40).contains(&words), "{words}: {panel}");
    assert!(panel[open..closed].matches("<li").count() < 30, "{panel}");
    assert!(
        !panel[closed..button].contains("<details id=\"cv-more-items\" open"),
        "{panel}"
    );
}

/// Page text about what it reads against says only what is true of this variant.
#[tokio::test]
async fn the_page_says_truly_what_a_draft_is_read_against() {
    let f = locked().await;
    approved_start(&f).await;
    let (_, _, page) = get(&f.app, VARIANT).await;
    assert!(!page.contains("the last you approved"), "approved: {page}");

    let page_of = async |variant: &str| get(&f.app, &format!("/cv/variants/{variant}")).await.2;
    let call = async |args: Value| {
        let reply = agent_call(&f, "patch_cv", args).await;
        assert_ne!(reply.is_error, Some(true), "{reply:?}");
    };
    call(
        json!({"variant": "copy", "createFrom": "default", "expectedRevision": 1,
        "summary": "New", "edits": []}),
    )
    .await;
    call(json!({"variant": "copy", "expectedRevision": 1, "summary": "Edit",
        "edits": [{"oldText": "Built the routing service.", "newText": "Built the routing engine."}]}))
    .await;
    let page = page_of("copy").await;
    assert!(page.contains("in another variant"), "{page}");
    assert!(!page.contains("the last you approved"), "{page}");

    agent_patch(
        &f,
        1,
        "Built the routing service.",
        "Built the routing core.",
    )
    .await;
    let page = page_of("default").await;
    assert!(
        page.contains("the last you approved in this variant"),
        "{page}"
    );

    // The same text, under a fact changed since: "isn't the text you last approved" would be false.
    agent_call(
        &f,
        "revert_cv",
        json!({"toRevision": 1, "expectedRevision": 2}),
    )
    .await;
    let fields: OwnerFacts =
        serde_json::from_value(json!({"languages": [{"name": "German", "level": "B2"}]})).unwrap();
    let mut tx = f.db.begin_write().await.unwrap();
    cv_facts::set_owner_facts(&mut tx, &fields, "test")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let page = page_of("default").await;
    assert_eq!(badge(&page), "Draft, not approved");
    assert!(!page.contains("isn't the text you last approved"), "{page}");
    assert!(!page.contains("Nothing is approved yet"), "{page}");
    assert!(page.contains("no approved text to compare with"), "{page}");
    assert!(page.contains("id=\"cv-whole-cv\""), "{page}");
    assert!(
        page.contains("The facts changed since you last approved"),
        "{page}"
    );
}

async fn tap(f: &Fixture, url: &str) {
    let (_, _, page) = get(&f.app, url).await;
    let (status, _, _) = post(
        &f.app,
        &format!("{url}/approve"),
        &[
            ("csrf", TOKEN),
            ("revision", &hidden(&page, "revision")),
            ("sourceSha256", &hidden(&page, "sourceSha256")),
            ("factsSha256", &hidden(&page, "factsSha256")),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
}

fn above_the_button(page: &str, id: &str) -> bool {
    let line = page.find(&format!("id=\"{id}\"")).unwrap_or(usize::MAX);
    line < page.find("class=\"approve-form\"").unwrap()
}

/// The owner's decision on corrections: a text replaced elsewhere reads as a diff again, and the line that
/// says what that costs stands above the button.
#[tokio::test]
async fn a_replaced_text_reads_as_a_diff_and_says_so_above_the_button() {
    let f = locked().await;
    let source = approved_start(&f).await;
    let make = async |name: &str, edits: Value| {
        agent_call(
            &f,
            "patch_cv",
            json!({"variant": name, "createFrom": "default", "expectedRevision": 1,
                "summary": "new", "edits": edits}),
        )
        .await
    };
    // Nothing replaced: taken at once, and a whole-CV line nowhere.
    make("copied", json!([])).await;
    let (_, _, page) = get(&f.app, "/cv/variants/copied").await;
    assert!(!page.contains("cv-replaced-elsewhere"), "{page}");

    // A change in the call that makes a variant reads against what it started from.
    let changed = source.replace("Built the routing service.", "Built the routing core.");
    assert_ne!(changed, source);
    make("tailored", json!([{"oldText": source, "newText": changed}])).await;
    let (_, _, page) = get(&f.app, "/cv/variants/tailored").await;
    assert!(page.contains("started from"), "{page}");
    assert!(!page.contains("cv-whole-cv"), "{page}");
    tap(&f, "/cv/variants/tailored").await;

    // That tap replaced the starting text. The next copy holds it, with nothing to mark, and says why.
    make("copy-after", json!([])).await;
    let (_, _, page) = get(&f.app, "/cv/variants/copy-after").await;
    assert!(page.contains("class=\"approve-form\""), "{page}");
    assert!(above_the_button(&page, "cv-replaced-elsewhere"), "{page}");
    assert!(page.contains("a line you corrected there won"), "{page}");
    assert!(!page.contains("cv-whole-cv"), "{page}");
    assert!(
        !page.contains("cv-looks-only"),
        "the source is the same: {page}"
    );

    // A draft on `default`'s own approval of that replaced text keeps its wording, with the line.
    let worded = source.replace("Built the routing service.", "Built the routing engine.");
    let landed = agent_call(
        &f,
        "patch_cv",
        json!({"expectedRevision": 1, "summary": "tidy",
            "edits": [{"oldText": source, "newText": worded}]}),
    )
    .await;
    assert_ne!(landed.is_error, Some(true));
    let (_, _, page) = get(&f.app, VARIANT).await;
    assert!(above_the_button(&page, "cv-replaced-elsewhere"), "{page}");
    assert!(
        page.contains("the last you approved in this variant"),
        "{page}"
    );

    // On the text the tap approved, in the variant where it was changed, there is nothing to say.
    let (_, _, page) = get(&f.app, "/cv/variants/tailored").await;
    assert!(!page.contains("cv-replaced-elsewhere"), "{page}");
}

/// The owner's decision on marks: said above the button, naming where to look, and refusing nothing.
#[tokio::test]
async fn a_mark_drawn_on_a_number_is_said_above_the_button() {
    let f = locked().await;
    let source = approved_start(&f).await;
    let (_, _, page) = get(&f.app, VARIANT).await;
    assert!(!page.contains("cv-drawn-marks"), "{page}");
    let marked = source.replace(
        "Built the routing service.",
        "Grade #text(size: 11pt, kerning: false)[F]#box(width: 0pt, height: 0pt, place(top + left, dx: -5pt, dy: -0.6pt, rect(width: 5pt, height: 0.6pt, fill: black))) here.",
    );
    assert_ne!(marked, source);
    let landed = agent_call(
        &f,
        "patch_cv",
        json!({"expectedRevision": 1, "summary": "tidy",
            "edits": [{"oldText": source, "newText": marked}]}),
    )
    .await;
    assert_ne!(landed.is_error, Some(true));
    let (_, _, page) = get(&f.app, VARIANT).await;
    assert!(above_the_button(&page, "cv-drawn-marks"), "{page}");
    assert!(page.contains("Read the numbers in the picture"), "{page}");
}

/// The fifth gate review's finding 7: a sentence nobody has seen stays open ahead of a flood of numbers.
#[tokio::test]
async fn a_new_word_stands_ahead_of_the_numbers() {
    let f = locked().await;
    let source = approved_start(&f).await;
    let lines: String = (0..40)
        .map(|i| format!("- Handled {} invoices.\n", 100 + i))
        .collect();
    let flood = source.replace(
        "- Built the routing service.\n",
        &format!("{lines}- Won the zorbitane award.\n"),
    );
    assert_ne!(flood, source);
    let landed = agent_call(
        &f,
        "patch_cv",
        json!({"expectedRevision": 1, "summary": "tidy",
            "edits": [{"oldText": source, "newText": flood}]}),
    )
    .await;
    assert_ne!(landed.is_error, Some(true));
    let (_, _, page) = get(&f.app, VARIANT).await;
    let open = &page[page.find("id=\"cv-review-items\"").unwrap()..];
    let open = &open[..open.find("</ul>").unwrap()];
    assert!(open.contains("<mark>zorbitane</mark>"), "{open}");
}

/// A stored revision that can't be drawn: no form, a refused tap, and no claim that nothing is stored.
#[tokio::test(flavor = "multi_thread")]
async fn a_revision_too_heavy_to_draw_says_so_and_cant_be_approved() {
    use std::os::unix::fs::PermissionsExt;
    let f = locked().await;
    approved_start(&f).await;
    agent_patch(
        &f,
        1,
        "Built the routing service.",
        "Built the routing engine.",
    )
    .await;
    let (_, _, page) = get(&f.app, VARIANT).await;
    let shown = [
        ("csrf", TOKEN.to_owned()),
        ("revision", hidden(&page, "revision")),
        ("sourceSha256", hidden(&page, "sourceSha256")),
        ("factsSha256", hidden(&page, "factsSha256")),
    ];
    let worker = f.dir.path().join("heavy-worker");
    std::fs::write(&worker, "#!/bin/sh\nexec /usr/bin/sleep 60\n").unwrap();
    std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o755)).unwrap();
    let heavy = McpState::new(
        f.db.clone(),
        "owner-1".to_owned(),
        "owner@example.com".to_owned(),
        nexus_mcp::Renderer::Subprocess {
            exe: worker,
            rss_limit_bytes: 1,
            timeout: Duration::from_secs(20),
        },
    );
    let app = build(&heavy, &f.dir, None);
    let (_, _, page) = get(&app, VARIANT).await;
    assert!(!page.contains("class=\"approve-form\""), "{page}");
    assert!(page.contains("too much memory"), "{page}");
    assert!(!page.contains("Nothing was stored"), "it is stored: {page}");
    let (status, _, json) = get(&app, &format!("{VARIANT}/preview.json?revision=2")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(json.contains("too much memory"), "{json}");
    assert!(!json.contains("Nothing was stored"), "it is stored: {json}");

    let approvals_before = approvals(&f.db).await;
    let fields: Vec<(&str, &str)> = shown.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let (status, _, page) = post(&app, &format!("{VARIANT}/approve"), &fields).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!page.contains("Nothing was stored"), "{page}");
    assert!(page.contains("Nothing was approved"), "{page}");
    assert_eq!(approvals(&f.db).await, approvals_before);
}

/// Which view a page opens in, read off the workspace.
fn view_of(page: &str) -> &str {
    let after = page.split("data-view=\"").last().expect("the view");
    after.split('"').next().unwrap()
}

/// A draft opens on what there is to approve, anything else on the editor, and a link picks.
#[tokio::test]
async fn the_view_follows_the_head_unless_a_link_names_one() {
    let f = locked().await;
    let (_, _, page) = get(&f.app, VARIANT).await;
    assert_eq!(view_of(&page), "edit", "nothing stored: {page}");
    assert!(page.contains("id=\"cv-source\""), "{page}");
    assert!(!page.contains("id=\"cv-approval\""), "{page}");

    approved_start(&f).await;
    let (_, _, page) = get(&f.app, VARIANT).await;
    assert_eq!(view_of(&page), "edit", "approved: {page}");
    // Asked for, the review of approved text says so, with the picture beside it.
    let (_, _, page) = get(&f.app, &format!("{VARIANT}?view=review")).await;
    assert_eq!(view_of(&page), "review");
    assert!(page.contains("You approved this text"), "{page}");
    assert!(page.contains("id=\"cv-preview-pages\""), "{page}");
    assert!(!page.contains("id=\"cv-source\""), "{page}");

    agent_patch(
        &f,
        1,
        "Built the routing service.",
        "Built the routing core.",
    )
    .await;
    let (_, _, page) = get(&f.app, VARIANT).await;
    assert_eq!(view_of(&page), "review", "a draft: {page}");
    assert!(page.contains("class=\"approve-form\""), "{page}");
    assert!(!page.contains("id=\"cv-source\""), "{page}");
    assert!(!page.contains("id=\"cv-revisions\""), "{page}");
    assert!(
        page.contains("href=\"/cv/variants/default?view=edit\""),
        "{page}"
    );

    let (_, _, page) = get(&f.app, EDIT).await;
    assert_eq!(view_of(&page), "edit");
    assert!(!page.contains("class=\"approve-form\""), "{page}");
    let (_, _, page) = get(&f.app, HISTORY).await;
    assert_eq!(view_of(&page), "history");
    assert!(page.contains("id=\"cv-revisions\""), "{page}");
    assert!(!page.contains("id=\"cv-source\""), "{page}");
    // The picture has no view of its own: it is part of the review.
    let (_, _, page) = get(&f.app, &format!("{VARIANT}?view=picture")).await;
    assert_eq!(view_of(&page), "review");
    assert!(!page.contains("view=picture"), "{page}");
    // A link from before the views, and a view nobody has, land somewhere sensible.
    let (_, _, page) = get(&f.app, &format!("{VARIANT}?page=1")).await;
    assert_eq!(view_of(&page), "history");
    let (_, _, page) = get(&f.app, &format!("{VARIANT}?view=nonsense")).await;
    assert_eq!(view_of(&page), "review");

    // A refused save comes back in the editor, with the text, whatever the head is.
    let (_, source) = head_of(&f).await;
    let typed = format!("{source}\n#nonsense(");
    let (status, _, page) = save(&f, 2, &typed).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(view_of(&page), "edit");
    assert_eq!(editor_text(&page), typed);
}

/// Every view draws the CV once: a draft's own picture where it is read for approval, and the
/// preview elsewhere.
#[tokio::test]
async fn each_view_draws_the_picture_once() {
    let f = locked().await;
    approved_start(&f).await;
    agent_patch(
        &f,
        1,
        "Built the routing service.",
        "Built the routing core.",
    )
    .await;
    for view in ["review", "edit", "history"] {
        let (_, _, page) = get(&f.app, &format!("{VARIANT}?view={view}")).await;
        assert_eq!(page.matches("<img").count(), 1, "{view}: {page}");
        let reading = view == "review";
        assert_eq!(page.contains("id=\"cv-picture-pages\""), reading, "{view}");
        assert_eq!(page.contains("id=\"cv-preview-pages\""), !reading, "{view}");
    }
}

/// A word on a line of the picture says where it stands there, for `cv.js` to mark it. One that is on
/// no line says nothing.
#[tokio::test]
async fn a_word_to_check_carries_its_place_in_the_picture() {
    let f = locked().await;
    let source = approved_start(&f).await;
    let education = source.lines().find(|line| line.starts_with("#education("));
    agent_patch(&f, 1, education.unwrap(), "Zanzibar School 77").await;
    let (_, _, page) = get(&f.app, VARIANT).await;
    let list = page.split("id=\"cv-review-items\"").nth(1).unwrap();
    let chip = |token: &str| {
        // The line's own text marks the word too, without a rule.
        let end = list.find(&format!("\">{token}</mark>"));
        let end = end.unwrap_or_else(|| panic!("{token}: {list}"));
        &list[list[..end].rfind("<mark").unwrap()..end]
    };
    let boxes = chip("77").split("data-boxes=\"").nth(1).expect("its place");
    let boxes = boxes.split('"').next().unwrap();
    let (page_number, shares) = boxes.split_once(':').unwrap();
    assert_eq!(page_number, "1", "{boxes}");
    let shares: Vec<f32> = shares.split(',').map(|s| s.parse().unwrap()).collect();
    assert_eq!(shares.len(), 4, "{boxes}");
    assert!(shares.iter().all(|s| (0.0..1.0).contains(s)), "{boxes}");
    // One word of the line, not the line.
    assert!(shares[2] > 0.0 && shares[2] < 0.1, "{boxes}");
    assert_ne!(chip("Zanzibar"), chip("77"));
    assert!(!chip("Nordsee University").contains("data-boxes"), "{list}");
}

/// What a rule asks is said once, with its count. The words stand by the line they are on.
#[tokio::test]
async fn a_rule_is_said_once_and_its_words_stand_by_their_line() {
    let f = locked().await;
    approved_start(&f).await;
    agent_patch(
        &f,
        1,
        "Built the routing service.",
        "Built 77 Zanzibar services in 2019 and 12 in 2020.",
    )
    .await;
    let (_, _, page) = get(&f.app, VARIANT).await;
    let numbers = "A number or date. Is it right, and in the right place?";
    assert_eq!(page.matches(numbers).count(), 1, "{page}");
    assert_eq!(page.matches("A new word. Is it true?").count(), 1, "{page}");
    let legend = page.split("id=\"cv-legend\"").nth(1).unwrap();
    let legend = legend.split("</ul>").next().unwrap();
    assert!(legend.contains("id=\"cv-rule-numbers\""), "{legend}");
    assert!(
        legend.contains(&format!("{numbers}<small> \u{b7} 4</small>")),
        "{legend}"
    );

    // One line, read once, with all its words, each pointing at its rule.
    let list = page.split("id=\"cv-review-items\"").nth(1).unwrap();
    let list = list.split("</ul>").next().unwrap();
    assert_eq!(list.matches("<li").count(), 1, "{list}");
    assert_eq!(list.matches("class=\"context\"").count(), 1, "{list}");
    assert_eq!(
        list.matches("aria-describedby=\"cv-rule-numbers\"").count(),
        4,
        "{list}"
    );
    let zanzibar = list.split("\">Zanzibar</mark>").next().unwrap();
    let zanzibar = zanzibar.rsplit("<mark").next().unwrap();
    assert!(
        zanzibar.starts_with(" aria-describedby=\"cv-rule-newWords\" ")
            && zanzibar.ends_with(" class=\"new"),
        "{list}"
    );
    assert!(
        page.contains("Review<small>\u{b7} 1</small>"),
        "the tab counts lines: {page}"
    );
}

async fn save_template(f: &Fixture, expected: i64, template: &str) -> StatusCode {
    let fields = [
        ("csrf", TOKEN),
        ("expected", &expected.to_string()),
        ("template", template),
    ];
    post(&f.app, &format!("{VARIANT}/save"), &fields).await.0
}

#[tokio::test]
async fn another_template_is_a_draft_with_only_the_show_line_changed() {
    let f = locked().await;
    let source = approved_start(&f).await;
    let (_, _, page) = get(&f.app, EDIT).await;
    assert!(page.contains("value=\"classic\" selected"), "{page}");

    assert_eq!(save_template(&f, 1, "modern").await, StatusCode::SEE_OTHER);
    let rows = revision_rows(&f.db).await;
    assert_eq!(
        rows[1].1,
        source.replace(
            "cv.with(lang: \"en\")",
            "cv.with(lang: \"en\", template: \"modern\")"
        )
    );
    assert_ne!(rows[1].1, source);
    assert_eq!(rows[1].3, "Template: modern");
    assert_eq!(
        approvals(&f.db).await,
        1,
        "the owner's save approves, this doesn't"
    );
    let (_, _, page) = get(&f.app, EDIT).await;
    assert_eq!(badge(&page), "Draft, not approved");
    assert!(page.contains("value=\"modern\" selected"), "{page}");

    assert_eq!(save_template(&f, 2, "fancy").await, StatusCode::BAD_REQUEST);
    let fields = [
        ("csrf", TOKEN),
        ("expected", "2"),
        ("template", "modern"),
        ("pages", "3"),
    ];
    assert_eq!(
        post(&f.app, &format!("{VARIANT}/save"), &fields).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(save_template(&f, 1, "banner").await, StatusCode::CONFLICT);
    assert_eq!(revision_rows(&f.db).await.len(), 2);
}

#[tokio::test]
async fn a_show_line_written_by_hand_turns_the_template_control_off() {
    let f = locked().await;
    let (_, source) = head_of(&f).await;
    let own = source.replace("cv.with(lang: \"en\")", "cv.with(lang: \"en\") // mine");
    assert_ne!(own, source);
    assert_eq!(save(&f, 0, &own).await.0, StatusCode::SEE_OTHER);
    let (_, _, page) = get(&f.app, EDIT).await;
    assert!(page.contains("id=\"cv-template\" disabled"), "{page}");
    assert!(page.contains("by hand"), "{page}");
    assert_eq!(
        save_template(&f, 1, "modern").await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(revision_rows(&f.db).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn unsaved_text_is_drawn_for_the_preview_and_stored_nowhere() {
    let f = locked().await;
    let (_, source) = head_of(&f).await;
    let modern = source.replace(
        "cv.with(lang: \"en\")",
        "cv.with(lang: \"en\", template: \"modern\")",
    );
    let uri = format!("{VARIANT}/try");
    let (status, _, _) = post(&f.app, &uri, &[("source", &modern)]).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _, body) = post(&f.app, &uri, &[("csrf", TOKEN), ("source", &modern)]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let told: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(told["pages"], 1);
    let picture = |key: String, page: u32| {
        let uri = format!(
            "{VARIANT}/try.png?key={key}&version={}&page={page}",
            told["version"]
        );
        send(&f.app, Request::get(uri).body(Body::empty()).unwrap())
    };
    let key = told["key"].as_str().unwrap().to_owned();
    let (status, _, png) = picture(key.clone(), 1).await;
    assert_eq!(status, StatusCode::OK);
    assert!(png.starts_with(b"\x89PNG"));
    assert_eq!(picture(key, 2).await.0, StatusCode::NOT_FOUND);
    assert_eq!(picture("../x".to_owned(), 1).await.0, StatusCode::NOT_FOUND);
    assert!(revision_rows(&f.db).await.is_empty());

    let broken = format!("{source}\n#nothing()\n");
    let (status, _, body) = post(&f.app, &uri, &[("csrf", TOKEN), ("source", &broken)]).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.contains("message"), "{body}");
    // The editor marks the line, so the answer names it.
    let told: Value = serde_json::from_str(&body).unwrap();
    let problem = &told["diagnostics"][0];
    assert_eq!(problem["line"], broken.lines().count());
    assert_eq!(problem["column"], 2);
    assert_eq!(problem["file"], "/main.typ");
    assert!(
        problem["message"].as_str().unwrap().contains("nothing"),
        "{body}"
    );
    assert!(revision_rows(&f.db).await.is_empty());
}

#[tokio::test]
async fn the_editor_has_the_template_select_with_its_fallback_button() {
    let f = locked().await;
    let (_, _, page) = get(&f.app, EDIT).await;
    for part in [
        "id=\"cv-template\"",
        "id=\"cv-template-save\"",
        "id=\"cv-preview-unsaved\"",
        "id=\"cv-template-unreadable\" hidden",
    ] {
        assert!(page.contains(part), "{part}\n{page}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_draft_lists_its_layout_notes_and_each_marks_its_line() {
    let f = locked().await;
    let source = approved_start(&f).await;
    let stray = format!("{source}\n#pagebreak()\nBuilt the routing service.\n");
    // Without the mark of its base, a save is a draft.
    let fields = [
        ("csrf", TOKEN),
        ("expected", "1"),
        ("source", stray.as_str()),
    ];
    let (status, _, _) = post(&f.app, &format!("{VARIANT}/save"), &fields).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_, _, page) = get(&f.app, "/cv/variants/default?view=review").await;
    let layout = page
        .split("id=\"cv-layout-fill\"")
        .nth(1)
        .expect("the layout list");
    assert!(layout.contains("2 pages, the last"), "{layout}");
    assert!(layout.contains("Page 2 holds only 1 line"), "{layout}");
    assert!(layout.contains("data-boxes=\"2:"), "{layout}");
    assert!(!layout.contains("breaks across pages"), "{layout}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_target_is_chosen_beside_the_template_and_the_preview_says_how_far_off() {
    let f = locked().await;
    let source = approved_start(&f).await;
    let (_, _, page) = get(&f.app, EDIT).await;
    assert!(page.contains("id=\"cv-pages\""), "{page}");
    assert!(
        page.contains("<option value=\"\" selected>no target</option>"),
        "{page}"
    );

    let aimed = source.replace("cv.with(lang: \"en\")", "cv.with(lang: \"en\", pages: 2)");
    let fields = [("csrf", TOKEN), ("source", aimed.as_str())];
    let (status, _, body) = post(&f.app, &format!("{VARIANT}/try"), &fields).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let told: Value = serde_json::from_str(&body).unwrap();
    let layout: Vec<&str> = told["layout"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(layout[0].starts_with("1 page, the last "), "{layout:?}");
    assert!(
        layout[1].starts_with("Aiming for 2 full pages: the CV ends on page 1"),
        "{layout:?}"
    );

    // Without the script, the form stores both choices as a draft.
    let fields = [
        ("csrf", TOKEN),
        ("expected", "1"),
        ("template", "classic"),
        ("pages", "2"),
    ];
    assert_eq!(
        post(&f.app, &format!("{VARIANT}/save"), &fields).await.0,
        StatusCode::SEE_OTHER
    );
    assert_eq!(revision_rows(&f.db).await[1].1, aimed);
    let (_, _, page) = get(&f.app, EDIT).await;
    assert!(
        page.contains("<option value=\"2\" selected>2 full pages</option>"),
        "{page}"
    );
    let (_, _, page) = get(&f.app, "/cv/variants/default?view=review").await;
    assert!(page.contains("Aiming for 2 full pages"), "{page}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fit_gives_back_the_text_at_the_density_closest_to_its_pages() {
    let f = locked().await;
    let (_, source) = head_of(&f).await;
    let uri = format!("{VARIANT}/fit");
    let (status, _, _) = post(&f.app, &uri, &[("source", &source)]).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, body) = post(&f.app, &uri, &[("csrf", TOKEN), ("source", &source)]).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.contains("Choose how many pages"), "{body}");

    // One page with room to spare: the loosest fills it most.
    let aimed = source.replace("cv.with(lang: \"en\")", "cv.with(lang: \"en\", pages: 1)");
    let (status, _, body) = post(&f.app, &uri, &[("csrf", TOKEN), ("source", &aimed)]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let told: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(told["density"], -2, "{told}");
    assert_eq!(
        told["source"],
        aimed.replace("pages: 1", "pages: 1, density: -2")
    );
    let message = told["message"].as_str().unwrap();
    assert!(
        message.starts_with("Density -2. Aiming for 1 full page: page 1 is"),
        "{message}"
    );

    // Two pages are out of reach, which it says with what is missing.
    let far = aimed.replace("pages: 1", "pages: 2");
    let (_, _, body) = post(&f.app, &uri, &[("csrf", TOKEN), ("source", &far)]).await;
    let told: Value = serde_json::from_str(&body).unwrap();
    let message = told["message"].as_str().unwrap();
    assert!(message.starts_with("No density reaches it. The closest is -2. Aiming for 2 full pages: the CV ends on page 1"), "{message}");
    assert!(
        message.contains("lines more fill them") || message.contains("more fill them"),
        "{message}"
    );
    assert!(revision_rows(&f.db).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rule_and_its_words_name_each_other_and_each_line_leads_to_the_source() {
    let f = locked().await;
    approved_start(&f).await;
    agent_patch(
        &f,
        1,
        "Built the routing service.",
        "Built the routing service for 12 depots.",
    )
    .await;
    let (_, _, page) = get(&f.app, "/cv/variants/default?view=review").await;
    let legend = page.split("id=\"cv-legend\"").nth(1).unwrap();
    let row = legend.split("<li").nth(1).unwrap();
    let rule = row
        .split("data-rule=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    assert!(row.contains(&format!("id=\"cv-rule-{rule}\"")), "{row}");
    assert!(page.contains("id=\"cv-legend-hint\" hidden"), "{page}");
    let chips = page.split("id=\"cv-review-items\"").nth(1).unwrap();
    assert!(chips.contains(&format!("data-rule=\"{rule}\"")), "{chips}");
    let link = chips
        .split("class=\"to-source\"")
        .next()
        .unwrap()
        .rsplit("<a ")
        .next()
        .unwrap();
    assert!(
        link.contains("href=\"/cv/variants/default?view=edit&amp;find="),
        "{link}"
    );

    // The layout note's word goes the same way, and the editor has the line for a word it lacks.
    let layout = page.split("id=\"cv-layout\"").nth(1).unwrap_or_default();
    assert_eq!(
        layout.matches("class=\"to-source\"").count(),
        layout.matches("class=\"layout\"").count()
    );
    let (status, _, page) = get(&f.app, "/cv/variants/default?view=edit&find=12%20depots").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("id=\"cv-find-miss\""), "{page}");
    assert!(page.contains("href=\"/cv/facts\""), "{page}");
}

/// The editor's script moves the textarea, so it runs before the script that selects a word in it,
/// and only the view with the textarea loads it.
#[tokio::test]
async fn the_editor_script_comes_with_the_edit_view_and_before_the_page_script() {
    let f = locked().await;
    let (_, _, page) = get(&f.app, EDIT).await;
    let editor = page.find("/assets/editor.js").expect("the editor's script");
    assert!(editor < page.find("/assets/cv.js").unwrap(), "{page}");
    // What only the scripts can work stays out of sight without them.
    assert!(page.contains("id=\"cv-compile-row\" hidden"), "{page}");
    assert!(
        page.contains("id=\"cv-compile-state\" role=\"status\""),
        "{page}"
    );
    assert!(page.contains("id=\"cv-error\" hidden"), "{page}");
    assert!(page.contains("id=\"cv-keys\" hidden"), "{page}");
    assert!(page.contains("Press Escape and then Tab to leave the editor."), "{page}");
    let (_, _, page) = get(&f.app, &format!("{VARIANT}?view=history")).await;
    assert!(!page.contains("/assets/editor.js"), "{page}");
    assert!(page.contains("/assets/cv.js"), "{page}");
}

/// What the editor completes with comes in an attribute: the helpers, the templates and the ids in
/// the facts, each with a line that says whose it is. No value in the facts can end the attribute.
#[tokio::test]
async fn the_editor_gets_the_ids_in_the_facts_and_no_value_breaks_out() {
    let f = fixture().await;
    let evil = "</script><script>alert(1)</script> \"quoted\" 'single' & <b>";
    let version = facts_row(&f.db).await.version.to_string();
    let metrics = format!("invoice-errors: 40%\nevil: {evil}");
    let fields = [
        ("csrf", TOKEN),
        ("expected", version.as_str()),
        ("metrics", metrics.as_str()),
    ];
    assert_eq!(post(&f.app, "/cv/facts", &fields).await.0, StatusCode::OK);

    let (_, _, page) = get(&f.app, EDIT).await;
    assert!(!page.contains("<script>alert(1)"), "{page}");
    let at = page.find("data-complete=\"").expect("the attribute") + "data-complete=\"".len();
    let raw = &page[at..at + page[at..].find('"').unwrap()];
    assert!(!raw.contains(['<', '>']), "{raw}");
    let told: Value = serde_json::from_str(&unescape(raw)).expect("JSON to the closing quote");
    assert_eq!(told["metric"][1], json!({"id": "evil", "label": evil}));
    assert_eq!(
        told["metric"][0],
        json!({"id": "invoice-errors", "label": "40%"})
    );
    assert_eq!(
        told["employment"],
        json!([{"id": "brightpath", "label": "Brightpath Logistics · Software Engineer · 03/2019 – 12/2022"}])
    );
    let school = &told["education"][0];
    assert_eq!(
        school["label"],
        "Nordsee University · BSc Computer Science · 2015 – 2018"
    );
    assert!(
        school["id"].as_str().is_some_and(|id| !id.is_empty()),
        "{school}"
    );
    assert_eq!(told["templates"], json!(nexus_tailor::TEMPLATES));
    let names: Vec<_> = told["helpers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|helper| helper["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"employment") && names.contains(&"cv"),
        "{names:?}"
    );
    assert_eq!(told["helpers"][3]["snippet"], "employment(\"|\")[\n  - \n]");
    // The other views have no editor to complete in.
    let (_, _, page) = get(&f.app, &format!("{VARIANT}?view=history")).await;
    assert!(!page.contains("data-complete"), "{page}");
}
