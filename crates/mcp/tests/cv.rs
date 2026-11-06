//! The CV tools over real MCP, with invented people and an in-process renderer.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use common::*;
use nexus_core::repo::cv_facts::{self, OwnerFacts};
use nexus_mcp::Renderer;
use serde_json::{Value, json};

fn profile() -> Value {
    json!({
        "name": "Mara Example",
        "contact": {"email": "mara@example.test", "location": "Hamburg"},
        "profile": "Backend engineer building freight software.",
        "skills": [
            {"category": "Languages", "items": ["Rust", "Go"]},
            {"category": "Tools", "items": ["Docker"]},
        ],
        "experience": [
            {"id": "brightpath", "company": "Brightpath Logistics", "title": "Software Engineer",
             "date": "03/2019 – 12/2022", "location": "Hamburg", "tier": 1,
             "bullets": ["Cut invoice errors by 40% across 3 depots.", "Built the routing service."]},
            {"id": "harborline", "company": "Harborline Systems", "title": "Platform Engineer",
             "date": "01/2023 – 06/2025", "location": "Bremen", "tier": 3, "bullets": []},
        ],
        "projects": [{"name": "tidy", "url": "tidy.example.test", "stack": "Rust",
            "description": "A linter for route files."}],
        "education": [{"institution": "Nordsee University", "degree": "BSc Computer Science",
            "date": "2015 – 2018", "location": "Kiel"}],
    })
}

async fn owner_set(server: &Server, fields: Value) {
    let fields: OwnerFacts = serde_json::from_value(fields).unwrap();
    let mut tx = server.db.begin_write().await.unwrap();
    cv_facts::set_owner_facts(&mut tx, &fields, "test")
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

async fn owner_lock(server: &Server) {
    let mut tx = server.db.begin_write().await.unwrap();
    cv_facts::lock(&mut tx, "test").await.unwrap();
    tx.commit().await.unwrap();
}

/// A server with a profile and locked facts.
async fn locked_server() -> Server {
    let server = Server::start().await;
    server.call("upsert_cv_profile", profile()).await.ok();
    owner_set(
        &server,
        json!({"languages": [{"name": "German", "level": "C1"}], "bannedPhrases": ["team player"]}),
    )
    .await;
    owner_lock(&server).await;
    server
}

async fn head(server: &Server, variant: &str) -> (i64, String) {
    let got = server
        .call("get_cv_source", json!({"variant": variant}))
        .await
        .ok();
    (
        got["revision"].as_i64().unwrap(),
        got["source"].as_str().unwrap().to_owned(),
    )
}

async fn patch(server: &Server, expected: i64, old: &str, new: &str) -> ToolReply {
    server
        .call(
            "patch_cv",
            json!({
                "expectedRevision": expected,
                "edits": [{"oldText": old, "newText": new}],
                "summary": "An edit",
                "origin": "test",
            }),
        )
        .await
}

/// Everything no CV tool may touch on a refusal: facts, history and every revision.
async fn state(server: &Server) -> Vec<String> {
    let mut out = Vec::new();
    for sql in [
        "SELECT json_group_array(json_object('n', name, 'c', contact, 'e', employment, 'd', education,
             'l', languages, 'm', metrics, 'a', allowed_tokens, 'b', banned_phrases, 'w', vocabulary,
             'v', version, 't', locked_at)) FROM cv_facts",
        "SELECT json_group_array(state) FROM cv_facts_history",
        "SELECT json_group_array(json_object('n', number, 's', source_sha256)) FROM cv_revisions",
        "SELECT json_group_array(json_object('n', name, 'h', head_revision_id)) FROM cv_variants",
    ] {
        out.push(
            sqlx::query_scalar::<_, String>(sql)
                .fetch_one(server.db.pool())
                .await
                .unwrap(),
        );
    }
    out
}

#[tokio::test]
async fn patching_needs_locked_facts_and_the_right_head() {
    let server = Server::start().await;
    server.call("upsert_cv_profile", profile()).await.ok();
    let unlocked = patch(&server, 0, "Backend engineer", "Engineer").await;
    assert_eq!(unlocked.code(), "cv_facts_not_locked");
    owner_lock(&server).await;

    let stale = patch(&server, 4, "Backend engineer", "Engineer").await;
    assert_eq!(stale.code(), "cv_revision_conflict");
    assert_eq!(stale.json()["error"]["currentRevision"], 0);

    let first = patch(&server, 0, "Backend engineer", "Freight engineer")
        .await
        .ok();
    assert_eq!(first["revision"], 1);
    assert_eq!(first["variant"], "default");
    assert!(
        first["diff"]
            .as_str()
            .unwrap()
            .contains("+Freight engineer"),
        "{first}"
    );
    assert!(
        first["diff"]
            .as_str()
            .unwrap()
            .contains("-Backend engineer"),
        "{first}"
    );
    assert_eq!(first["reviewTokens"], json!([]), "{first}");

    let stale = patch(&server, 0, "Freight engineer", "Engineer").await;
    assert_eq!(stale.code(), "cv_revision_conflict");
    assert_eq!(stale.json()["error"]["currentRevision"], 1);
    assert_eq!(stale.json()["error"]["expectedRevision"], 0);

    let flagged = patch(&server, 1, "Freight engineer", "Kubernetes engineer")
        .await
        .ok();
    assert_eq!(flagged["revision"], 2);
    assert_eq!(flagged["reviewTokens"], json!(["Kubernetes"]));
    let stored: String = sqlx::query_scalar("SELECT warnings FROM cv_revisions WHERE number = 2")
        .fetch_one(server.db.pool())
        .await
        .unwrap();
    assert_eq!(stored, r#"["Kubernetes"]"#);
    let (origin,): (String,) = sqlx::query_as("SELECT origin FROM cv_revisions WHERE number = 2")
        .fetch_one(server.db.pool())
        .await
        .unwrap();
    assert_eq!(origin, "test");
}

#[tokio::test]
async fn origin_is_free_text_limited_in_length_and_stored_as_data() {
    let server = locked_server().await;
    let args = |origin: &str, summary: &str| {
        json!({
            "expectedRevision": 0,
            "edits": [{"oldText": "Backend engineer", "newText": "Freight engineer"}],
            "summary": summary,
            "origin": origin,
        })
    };
    let long = "x".repeat(65);
    assert!(
        server
            .call("patch_cv", args(&long, "s"))
            .await
            .err_text()
            .contains("too_big")
    );
    let long = "x".repeat(501);
    assert!(
        server
            .call("patch_cv", args("o", &long))
            .await
            .err_text()
            .contains("too_big")
    );
    let origin = "'); DROP TABLE cv_facts; --";
    server.call("patch_cv", args(origin, "s")).await.ok();
    let listed = server.call("list_cv_revisions", json!({})).await.ok();
    assert_eq!(listed["revisions"][0]["origin"], origin);
    assert!(
        server.call("get_cv_source", json!({})).await.ok()["factsLocked"] == true,
        "the facts are still there"
    );
}

#[tokio::test]
async fn edits_are_atomic_and_a_bad_one_is_named() {
    let server = locked_server().await;
    let before = state(&server).await;
    let reply = server
        .call(
            "patch_cv",
            json!({
                "expectedRevision": 0,
                "edits": [
                    {"oldText": "Backend engineer", "newText": "Engineer"},
                    {"oldText": "no such text", "newText": "x"},
                ],
                "summary": "s",
            }),
        )
        .await;
    assert_eq!(reply.code(), "cv_edit_no_match");
    assert_eq!(reply.json()["error"]["edit"], 1);

    let reply = patch(&server, 0, "Rust", "Zig").await;
    assert_eq!(reply.code(), "cv_edit_ambiguous");
    assert_eq!(reply.json()["error"]["matches"], 2);

    assert_eq!(
        patch(&server, 0, "Backend engineer", "Backend engineer")
            .await
            .code(),
        "cv_no_change"
    );
    assert_eq!(state(&server).await, before);
}

#[tokio::test]
async fn a_compile_error_returns_diagnostics_and_stores_nothing() {
    let server = locked_server().await;
    let before = state(&server).await;
    let reply = patch(&server, 0, "Backend engineer", "#nosuchfunction()").await;
    assert_eq!(reply.code(), "cv_compile_failed");
    let diagnostic = &reply.json()["error"]["diagnostics"][0];
    assert_eq!(diagnostic["severity"], "error");
    assert!(diagnostic["line"].as_u64().unwrap() > 1, "{diagnostic}");
    assert_eq!(state(&server).await, before);
}

#[tokio::test]
async fn the_gate_refuses_invented_facts_on_every_path_and_stores_nothing() {
    let server = locked_server().await;
    let before = state(&server).await;
    for (new, rule, token) in [
        ("Backend engineer with 12 years", "numbers", "12"),
        (
            "Eight years as a backend engineer",
            "numberWords",
            "eight years",
        ),
        ("Senior backend engineer", "claimWords", "Senior"),
        (
            "Backend engineer, a team player",
            "bannedPhrases",
            "team player",
        ),
        (
            "Backend engineer #text(fill: white)[Harborline]",
            "hiddenText",
            "Harborline",
        ),
        (
            "Backend engineer #image(bytes(\"<svg xmlns='http://www.w3.org/2000/svg' width='9' height='9'/>\"), format: \"svg\")",
            "images",
            "1",
        ),
    ] {
        let reply = patch(&server, 0, "Backend engineer", new).await;
        assert_eq!(reply.code(), "cv_fact_violation", "{new}");
        let violations = reply.json()["error"]["violations"].clone();
        assert!(
            violations
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["rule"] == rule && v["token"].as_str().unwrap().contains(token)),
            "{new}: {violations}"
        );
    }
    let reply = patch(
        &server,
        0,
        "#employment(\"brightpath\")[",
        "Brightpath Logistics Senior Architect 2010\n#employment(\"brightpath\")[",
    )
    .await;
    assert_eq!(reply.code(), "cv_fact_violation");
    assert_eq!(state(&server).await, before);
}

#[tokio::test]
async fn no_argument_skips_the_check() {
    let server = locked_server().await;
    let before = state(&server).await;
    for extra in [
        "force",
        "skipCheck",
        "override",
        "allowViolations",
        "unsafe",
    ] {
        let mut args = json!({
            "expectedRevision": 0,
            "edits": [{"oldText": "Backend engineer", "newText": "Senior engineer"}],
            "summary": "s",
        });
        args[extra] = true.into();
        assert_eq!(
            server.call("patch_cv", args).await.code(),
            "cv_fact_violation",
            "{extra}"
        );
    }
    assert_eq!(state(&server).await, before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_patches_of_one_head_cannot_both_win() {
    let server = locked_server().await;
    let (a, b) = tokio::join!(
        patch(&server, 0, "Backend engineer", "Freight engineer"),
        patch(&server, 0, "Backend engineer", "Routing engineer"),
    );
    let (won, lost) = if a.is_error { (b, a) } else { (a, b) };
    assert_eq!(won.ok()["revision"], 1);
    assert_eq!(lost.code(), "cv_revision_conflict");
    assert_eq!(lost.json()["error"]["currentRevision"], 1);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM cv_revisions")
        .fetch_one(server.db.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn a_new_variant_starts_from_another_and_keeps_its_own_history() {
    let server = locked_server().await;
    let one = patch(&server, 0, "Backend engineer", "Freight engineer")
        .await
        .ok();
    assert_eq!(one["revision"], 1);

    let created = server
        .call(
            "patch_cv",
            json!({
                "variant": "research",
                "createFrom": "default",
                "expectedRevision": 1,
                "edits": [{"oldText": "Freight engineer", "newText": "Research engineer"}],
                "summary": "For research roles",
            }),
        )
        .await
        .ok();
    assert_eq!(created["revision"], 1);
    assert_eq!(created["variant"], "research");
    assert!(
        head(&server, "research")
            .await
            .1
            .contains("Research engineer")
    );
    assert!(
        head(&server, "default")
            .await
            .1
            .contains("Freight engineer")
    );

    let again = json!({"variant": "research", "createFrom": "default", "expectedRevision": 1,
        "edits": [], "summary": "s"});
    assert_eq!(
        server.call("patch_cv", again).await.code(),
        "cv_variant_exists"
    );
    for name in ["application-9", "a b", ""] {
        let args = json!({"variant": name, "createFrom": "default", "expectedRevision": 1,
            "edits": [], "summary": "s"});
        assert_eq!(
            server.call("patch_cv", args).await.code(),
            "cv_variant_name_invalid",
            "{name}"
        );
    }
    let unknown = json!({"variant": "nope", "expectedRevision": 0, "edits": [], "summary": "s"});
    assert_eq!(
        server.call("patch_cv", unknown).await.code(),
        "cv_variant_not_found"
    );
    assert_eq!(
        server
            .call("get_cv_source", json!({"variant": "nope"}))
            .await
            .code(),
        "cv_variant_not_found"
    );
}

#[tokio::test]
async fn get_cv_source_gives_facts_and_help_but_not_the_banned_phrases() {
    let server = locked_server().await;
    let got = server.call("get_cv_source", json!({})).await.ok();
    assert_eq!(got["revision"], 0);
    assert_eq!(got["variant"], "default");
    assert!(
        got["source"]
            .as_str()
            .unwrap()
            .contains("#employment(\"brightpath\")")
    );
    assert_eq!(
        got["facts"]["employment"][0]["company"],
        "Brightpath Logistics"
    );
    assert_eq!(got["factsLocked"], true);
    assert!(
        got["templateHelp"]
            .as_str()
            .unwrap()
            .contains("#employment")
    );
    assert!(got["facts"].get("bannedPhrases").is_none());
    assert!(!got.to_string().contains("team player"));

    // Not from the compile's own copy of the facts either.
    let printed = patch(
        &server,
        0,
        "Backend engineer",
        "#json(\"/facts.json\").bannedPhrases.join(\" \") engineer",
    )
    .await
    .ok();
    assert_eq!(printed["revision"], 1);

    patch(&server, 1, "engineer building", "engineer, building")
        .await
        .ok();
    let old = server
        .call("get_cv_source", json!({"revision": 1}))
        .await
        .ok();
    assert_eq!(old["revision"], 1);
    assert_eq!(
        server
            .call("get_cv_source", json!({"revision": 9}))
            .await
            .code(),
        "cv_revision_not_found"
    );
}

#[tokio::test]
async fn revisions_list_newest_first_with_diffs_and_a_revert_adds_one() {
    let server = locked_server().await;
    patch(&server, 0, "Backend engineer", "Freight engineer")
        .await
        .ok();
    patch(&server, 1, "Freight engineer", "Routing engineer")
        .await
        .ok();

    let page = server
        .call(
            "list_cv_revisions",
            json!({"includeDiff": true, "pageSize": 1}),
        )
        .await
        .ok();
    assert_eq!(page["total"], 2);
    assert_eq!(page["revisions"].as_array().unwrap().len(), 1);
    let newest = &page["revisions"][0];
    assert_eq!(newest["revision"], 2);
    assert_eq!(newest["origin"], "test");
    assert!(
        newest["diff"]
            .as_str()
            .unwrap()
            .contains("+Routing engineer")
    );
    let plain = server
        .call("list_cv_revisions", json!({"page": 2, "pageSize": 1}))
        .await
        .ok();
    assert_eq!(plain["revisions"][0]["revision"], 1);
    assert!(plain["revisions"][0].get("diff").is_none());

    let before = state(&server).await;
    let args = json!({"toRevision": 1, "expectedRevision": 1});
    assert_eq!(
        server.call("revert_cv", args).await.code(),
        "cv_revision_conflict"
    );
    let args = json!({"toRevision": 9, "expectedRevision": 2});
    assert_eq!(
        server.call("revert_cv", args).await.code(),
        "cv_revision_not_found"
    );
    assert_eq!(state(&server).await, before);

    let reverted = server
        .call("revert_cv", json!({"toRevision": 1, "expectedRevision": 2}))
        .await
        .ok();
    assert_eq!(reverted["revision"], 3);
    assert!(
        reverted["diff"]
            .as_str()
            .unwrap()
            .contains("+Freight engineer")
    );
    let (number, source) = head(&server, "default").await;
    assert_eq!(number, 3);
    assert!(source.contains("Freight engineer"));
    let listed = server.call("list_cv_revisions", json!({})).await.ok();
    assert_eq!(listed["total"], 3, "history never shrinks");
    assert_eq!(listed["revisions"][0]["origin"], "revert");
}

#[tokio::test]
async fn a_revert_and_a_render_are_judged_by_todays_facts() {
    let server = locked_server().await;
    owner_set(&server, json!({"allowedTokens": ["40%", "3", "7"]})).await;
    let one = patch(
        &server,
        0,
        "Backend engineer",
        "Backend engineer of 7 teams",
    )
    .await
    .ok();
    assert_eq!(one["revision"], 1);
    patch(&server, 1, " of 7 teams", "").await.ok();
    assert!(!render(&server, json!({"revision": 1})).await.is_error);

    owner_set(&server, json!({"allowedTokens": ["40%", "3"]})).await;
    let before = state(&server).await;
    let reverted = server
        .call("revert_cv", json!({"toRevision": 1, "expectedRevision": 2}))
        .await;
    assert_eq!(reverted.code(), "cv_fact_violation");
    assert_eq!(state(&server).await, before);

    let stale = render(&server, json!({"revision": 1})).await;
    assert!(stale.is_error);
    assert_eq!(stale.text_json()["error"]["code"], "cv_fact_violation");
    assert!(stale.content.len() == 1, "no PDF or image comes back");
    assert!(
        !render(&server, json!({})).await.is_error,
        "the head still passes"
    );
}

struct Rendered {
    is_error: bool,
    content: Vec<Value>,
}

impl Rendered {
    fn text_json(&self) -> Value {
        serde_json::from_str(self.content[0]["text"].as_str().unwrap()).unwrap()
    }
}

async fn render(server: &Server, args: Value) -> Rendered {
    let (status, body) = server
        .post(
            FULL,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "render_cv_pdf", "arguments": args}}),
        )
        .await;
    assert_eq!(status, 200);
    Rendered {
        is_error: body["result"]["isError"] == true,
        content: body["result"]["content"].as_array().unwrap().clone(),
    }
}

#[tokio::test]
async fn a_render_returns_a_resource_and_images_and_can_save_a_document() {
    let server = locked_server().await;
    patch(&server, 0, "Backend engineer", "Freight engineer")
        .await
        .ok();
    let done = render(&server, json!({"previewPages": 1})).await;
    assert!(!done.is_error, "{:?}", done.content);
    assert_eq!(done.content.len(), 3);
    let summary = done.text_json();
    assert_eq!(summary["revision"], 1);
    assert_eq!(summary["pageCount"], 1);
    assert_eq!(done.content[1]["type"], "resource");
    assert_eq!(done.content[1]["resource"]["mimeType"], "application/pdf");
    assert_eq!(
        done.content[1]["resource"]["uri"],
        "nexus://cv/default/1.pdf"
    );
    let pdf = STANDARD
        .decode(done.content[1]["resource"]["blob"].as_str().unwrap())
        .unwrap();
    assert!(pdf.starts_with(b"%PDF-"));
    assert_eq!(summary["pdfBytes"], pdf.len());
    assert_eq!(done.content[2]["type"], "image");
    assert_eq!(done.content[2]["mimeType"], "image/png");
    let png = STANDARD
        .decode(done.content[2]["data"].as_str().unwrap())
        .unwrap();
    assert!(png.starts_with(b"\x89PNG"));

    let none = render(&server, json!({"previewPages": 0})).await;
    assert_eq!(none.content.len(), 2);

    let app = server.application("Acme").await;
    let saved = render(&server, json!({"saveToApplicationId": app})).await;
    let id = saved.text_json()["savedDocumentId"]
        .as_str()
        .unwrap()
        .to_owned();
    let document = server.call("get_document", json!({"id": id})).await.ok();
    assert_eq!(document["originalName"], "CV - Acme - Engineer.pdf");
    assert_eq!(document["mimeType"], "application/pdf");
    assert_eq!(document["applications"][0]["id"], app);
    let content = server
        .call("download_document_content", json!({"id": document["id"]}))
        .await
        .ok();
    let stored = STANDARD
        .decode(content["contentBase64"].as_str().unwrap())
        .unwrap();
    assert_eq!(
        document["contentHash"],
        nexus_core::blob::sha256_hex(&stored),
        "hashes work as for an upload"
    );

    let missing = render(&server, json!({"saveToApplicationId": "424242"})).await;
    assert!(missing.is_error);
    assert_eq!(
        missing.content[0]["text"],
        "Application not found or access denied"
    );
}

/// Finding 13: a stored CV is built on facts the owner confirmed.
#[tokio::test]
async fn saving_a_render_needs_locked_facts_and_a_plain_render_does_not() {
    let server = Server::start().await;
    server.call("upsert_cv_profile", profile()).await.ok();
    owner_set(&server, json!({"allowedTokens": ["40%", "3"]})).await;
    let app = server.application("Acme").await;
    let saved = render(&server, json!({"saveToApplicationId": app})).await;
    assert!(saved.is_error);
    assert_eq!(saved.text_json()["error"]["code"], "cv_facts_not_locked");
    assert_eq!(
        server
            .call("list_documents", json!({"applicationId": app}))
            .await
            .ok(),
        json!([]),
    );
    assert!(!render(&server, json!({})).await.is_error);
}

/// Finding 17: a refused source says what is wrong without costing the server its memory.
#[tokio::test]
async fn a_long_list_of_violations_is_cut_and_says_so() {
    let server = locked_server().await;
    let numbers: String = (1000..1200).map(|n| format!("{n} ")).collect();
    let long_run = format!("#text(fill: white)[{}]", "Word ".repeat(2000));
    let new = format!("Backend engineer. {numbers} {long_run}");
    let refused = patch(&server, 0, "Backend engineer", &new).await;
    assert_eq!(refused.code(), "cv_fact_violation");
    let error = &refused.json()["error"];
    let violations = error["violations"].as_array().unwrap();
    assert_eq!(violations.len(), 50);
    assert!(error["truncated"].as_u64().unwrap() >= 150, "{error}");
    let hidden = "Word ".repeat(2000);
    let tiny = format!("Backend engineer. #text(fill: white)[{hidden}]");
    let refused = patch(&server, 0, "Backend engineer", &tiny).await;
    let error = &refused.json()["error"];
    assert_eq!(error["truncated"], 0);
    for violation in error["violations"].as_array().unwrap() {
        assert!(violation["token"].as_str().unwrap().chars().count() <= 200);
        assert!(violation["context"].as_str().unwrap().chars().count() <= 200);
    }
}

/// Finding 17: a page of diffs has a budget. The newest ones show, and the rest say they were left out.
#[tokio::test]
async fn a_page_of_revisions_carries_a_bounded_amount_of_diff() {
    let server = locked_server().await;
    let filler = |c: char| format!("// {}\n", c.to_string().repeat(80)).repeat(3000);
    let mut old = "#show: cv".to_owned();
    let (mut number, _) = head(&server, "default").await;
    for c in ['a', 'b', 'c', 'd', 'e'] {
        let new = format!("{}#show: cv", filler(c));
        patch(&server, number, &old, &new).await.ok();
        old = new;
        number += 1;
    }
    let page = server
        .call("list_cv_revisions", json!({"includeDiff": true}))
        .await
        .ok();
    let revisions = page["revisions"].as_array().unwrap();
    assert_eq!(revisions.len(), 5);
    let total: usize = revisions
        .iter()
        .filter_map(|r| r["diff"].as_str())
        .map(str::len)
        .sum();
    assert!(total <= 1024 * 1024, "{total}");
    assert!(revisions[0]["diff"].is_string());
    assert_eq!(revisions[4]["diffOmitted"], true);
    assert!(revisions[4].get("diff").is_none());
}

fn generate_args(app: &str) -> Value {
    json!({
        "applicationId": app,
        "experienceIds": ["brightpath", "ghost"],
        "skillCategories": ["Languages", "Nope"],
        "includeProjects": true,
    })
}

#[tokio::test]
async fn generate_keeps_the_frozen_response_and_replaces_only_its_own_unsubmitted_document() {
    let server = Server::start().await;
    server.call("upsert_cv_profile", profile()).await.ok();
    let app = server.application("Acme").await;

    // No lock needed, as old Nexus had none. 40% and 3 aren't known until a lock confirms them.
    let refused = server
        .call("generate_tailored_cv", generate_args(&app))
        .await;
    assert_eq!(refused.code(), "cv_fact_violation");
    assert_eq!(
        server
            .call("list_documents", json!({"applicationId": app}))
            .await
            .ok(),
        json!([]),
    );
    owner_set(&server, json!({"allowedTokens": ["40%", "3"]})).await;

    let first = server
        .call("generate_tailored_cv", generate_args(&app))
        .await
        .ok();
    let keys: Vec<&String> = first.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "message",
            "documentId",
            "originalName",
            "size",
            "applicationId",
            "company",
            "role",
            "warnings"
        ]
    );
    assert_eq!(first["message"], "CV generated successfully");
    assert_eq!(first["originalName"], "CV - Acme - Engineer.pdf");
    assert_eq!(first["applicationId"], app.as_str());
    assert_eq!(first["company"], "Acme");
    assert_eq!(first["role"], "Engineer");
    assert_eq!(
        first["warnings"],
        json!([
            "Unmatched experience IDs ignored: ghost",
            "Unmatched skill categories ignored: Nope",
        ])
    );

    let revisions = server
        .call(
            "list_cv_revisions",
            json!({"variant": format!("application-{app}")}),
        )
        .await
        .ok();
    assert_eq!(revisions["total"], 1);
    assert_eq!(revisions["revisions"][0]["origin"], "generate");

    let args =
        json!({"applicationId": app, "experienceIds": ["brightpath"], "skillCategories": []});
    let second = server.call("generate_tailored_cv", args.clone()).await.ok();
    assert!(second.get("warnings").is_none());
    assert_ne!(second["documentId"], first["documentId"]);
    let listed = server
        .call("list_documents", json!({"applicationId": app}))
        .await
        .ok();
    assert_eq!(
        listed.as_array().unwrap().len(),
        1,
        "the previous one is gone"
    );
    assert_eq!(listed[0]["id"], second["documentId"]);

    let recorded = server
        .call(
            "record_application_submission",
            json!({
                "applicationId": app,
                "submittedAt": "2026-03-01T10:00:00.000Z",
                "idempotencyKey": "cv-submit-1",
                "documentIds": [second["documentId"]],
                "answers": one_answer(),
                "policy": good_policy(),
            }),
        )
        .await;
    assert!(!recorded.is_error, "{}", recorded.text);
    let third = server.call("generate_tailored_cv", args).await.ok();
    let listed = server
        .call("list_documents", json!({"applicationId": app}))
        .await
        .ok();
    let ids: Vec<&Value> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|d| &d["id"])
        .collect();
    assert!(
        ids.contains(&&second["documentId"]),
        "a submitted one stays: {ids:?}"
    );
    assert!(ids.contains(&&third["documentId"]));
}

/// Finding 16, a deliberate difference from old Nexus: a shared document stays for the other application.
#[tokio::test]
async fn generate_keeps_a_document_another_application_still_links() {
    let server = locked_server().await;
    let (a, b) = (
        server.application("Acme").await,
        server.application("Birch").await,
    );
    let first = server
        .call("generate_tailored_cv", generate_args(&a))
        .await
        .ok();
    let shared = first["documentId"].as_str().unwrap().to_owned();
    server
        .call(
            "update_document_links",
            json!({"id": shared, "applicationIds": [a, b]}),
        )
        .await
        .ok();
    let second = server
        .call("generate_tailored_cv", generate_args(&a))
        .await
        .ok();
    assert_ne!(second["documentId"].as_str().unwrap(), shared);

    let linked_to = |app: &str| {
        let app = app.to_owned();
        let server = &server;
        async move {
            let listed = server
                .call("list_documents", json!({"applicationId": app}))
                .await
                .ok();
            let ids: Vec<String> = listed
                .as_array()
                .unwrap()
                .iter()
                .map(|d| d["id"].as_str().unwrap().to_owned())
                .collect();
            ids
        }
    };
    assert_eq!(linked_to(&b).await, [shared.clone()]);
    assert_eq!(
        linked_to(&a).await,
        [second["documentId"].as_str().unwrap().to_owned()]
    );
    let kept = server
        .call("get_document", json!({"id": shared}))
        .await
        .ok();
    assert_eq!(kept["id"].as_str().unwrap(), shared);
    let content = server
        .call("download_document_content", json!({"id": shared}))
        .await;
    assert!(!content.is_error, "{}", content.text);
}

#[tokio::test]
async fn generate_refuses_what_the_check_refuses() {
    let server = locked_server().await;
    let app = server.application("Acme").await;
    let before = state(&server).await;
    for summary in [
        "Twelve years of work",
        "Senior engineer with 12 years",
        "A team player",
    ] {
        let args = json!({"applicationId": app, "experienceIds": ["brightpath"],
            "skillCategories": [], "profileOverride": summary});
        assert_eq!(
            server.call("generate_tailored_cv", args).await.code(),
            "cv_fact_violation",
            "{summary}"
        );
    }
    assert_eq!(state(&server).await, before);
    let patches: i64 = sqlx::query_scalar("SELECT count(*) FROM cv_patches")
        .fetch_one(server.db.pool())
        .await
        .unwrap();
    assert_eq!(patches, 0);
    assert_eq!(
        server
            .call("list_documents", json!({"applicationId": app}))
            .await
            .ok(),
        json!([])
    );
}

/// Arguments that satisfy a tool's schema, built from the schema itself, with the given overrides.
fn valid_args(schema: &Value, known_id: &str) -> Value {
    let properties = schema["properties"].as_object().unwrap();
    let required = schema["required"].as_array().cloned().unwrap_or_default();
    let mut args = serde_json::Map::new();
    for name in required {
        let name = name.as_str().unwrap();
        let property = &properties[name];
        let value = match (property["type"].as_str(), property.get("enum")) {
            (_, Some(options)) => options[0].clone(),
            (Some("string"), _) if name.to_lowercase().contains("id") => known_id.into(),
            (Some("string"), _) => {
                if property["format"] == "date-time" {
                    "2026-03-01T10:00:00.000Z".into()
                } else {
                    "synthetic".into()
                }
            }
            (Some("integer" | "number"), _) => property["minimum"].as_i64().unwrap_or(1).into(),
            (Some("boolean"), _) => true.into(),
            (Some("array"), _) => json!([]),
            (Some("object"), _) => valid_args(property, known_id),
            _ => "synthetic".into(),
        };
        args.insert(name.to_owned(), value);
    }
    Value::Object(args)
}

#[tokio::test]
async fn no_tool_can_write_locked_facts_or_lock_or_unlock() {
    let server = locked_server().await;
    let app = server.application("Acme").await;
    server
        .call(
            "generate_tailored_cv",
            json!({"applicationId": app, "experienceIds": ["brightpath"], "skillCategories": []}),
        )
        .await
        .ok();

    let facts_and_history = |state: Vec<String>| (state[0].clone(), state[1].clone());
    let before = facts_and_history(state(&server).await);

    let (_, listed) = server
        .post(
            FULL,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        )
        .await;
    let tools = listed["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 40);
    let attack = json!({
        "name": "Evil", "lockedAt": null, "locked_at": null, "locked": false, "unlock": true,
        "allowedTokens": ["999"], "allowed_tokens": ["999"], "vocabulary": ["Senior"],
        "bannedPhrases": [], "version": 99,
    });
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        // Deleting the application would only end this test's other calls early.
        if name.contains("delete") {
            continue;
        }
        for token in [FULL, API_TOKEN] {
            let mut args = valid_args(&tool["inputSchema"], &app);
            server.call_as(token, name, args.clone()).await;
            for (key, value) in attack.as_object().unwrap() {
                args[key] = value.clone();
            }
            server.call_as(token, name, args).await;
            assert_eq!(
                facts_and_history(state(&server).await),
                before,
                "{name} moved the facts"
            );
        }
    }
    for name in [
        "lock_cv_facts",
        "unlock_cv_facts",
        "set_cv_facts",
        "cv_facts",
        "__render",
    ] {
        let reply = server.call(name, json!({})).await;
        assert!(reply.err_text().contains("not found"), "{name}");
    }

    let mut changed = profile();
    changed["name"] = "Mara Prime".into();
    changed["experience"][0]["title"] = "Principal Engineer".into();
    changed["experience"][1]["date"] = "2001 – 2025".into();
    changed["profile"] = "Worked 15 years.".into();
    let refused = server.call("upsert_cv_profile", changed).await;
    assert_eq!(refused.code(), "cv_facts_locked");
    assert_eq!(
        refused.json()["error"]["fields"],
        json!([
            "name",
            "experience.brightpath.title",
            "experience.harborline.date",
            "profile"
        ])
    );
    assert_eq!(refused.json()["error"]["tokens"], json!(["15"]));
    assert_eq!(facts_and_history(state(&server).await), before);
}

#[tokio::test]
async fn get_cv_profile_is_unchanged_by_the_split_and_the_lock() {
    let server = Server::start().await;
    let written = server.call("upsert_cv_profile", profile()).await.ok();
    let read = server.call("get_cv_profile", json!({})).await;
    assert_eq!(read.text, serde_json::to_string_pretty(&written).unwrap());
    let keys: Vec<&String> = written.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "id",
            "name",
            "contact",
            "profile",
            "skills",
            "experience",
            "projects",
            "education",
            "createdAt",
            "updatedAt",
            "userId"
        ]
    );
    assert_eq!(written["experience"], profile()["experience"]);
    assert_eq!(written["education"], profile()["education"]);
    owner_lock(&server).await;
    assert_eq!(
        server.call("get_cv_profile", json!({})).await.text,
        read.text,
        "the lock itself changes nothing a client sees"
    );
}

#[tokio::test]
async fn deleting_an_application_takes_its_variant_but_not_others() {
    let server = locked_server().await;
    let app = server.application("Acme").await;
    let args =
        json!({"applicationId": app, "experienceIds": ["brightpath"], "skillCategories": []});
    server.call("generate_tailored_cv", args).await.ok();
    let name = format!("application-{app}");
    let derived = server
        .call(
            "patch_cv",
            json!({"variant": "derived", "createFrom": name, "expectedRevision": 1,
                "edits": [], "summary": "s"}),
        )
        .await
        .ok();
    assert_eq!(derived["revision"], 1);
    let deleted = server.call("delete_application", json!({"id": app})).await;
    assert!(!deleted.is_error, "{}", deleted.text);
    assert_eq!(
        server
            .call("get_cv_source", json!({"variant": name}))
            .await
            .code(),
        "cv_variant_not_found"
    );
    let listed = server
        .call(
            "list_cv_revisions",
            json!({"variant": "derived", "includeDiff": true}),
        )
        .await
        .ok();
    assert_eq!(listed["total"], 1, "its parent is gone, the variant isn't");
    assert!(
        listed["revisions"][0]["diff"]
            .as_str()
            .unwrap()
            .contains("--- empty")
    );
}

/// A fake worker: `exe __render` writes finished frames, as a real one would for a PDF at its limit.
fn fake_worker(dir: &std::path::Path, pdf_bytes: usize) -> (PathBuf, Vec<u8>) {
    let mut pdf = b"%PDF-1.7 ".to_vec();
    pdf.resize(pdf_bytes, b'x');
    let preview = b"\x89PNG\r\n\x1a\nsynthetic".to_vec();
    let head = json!({"Ok": {
        "hasPdf": true, "previewCount": 1, "pageCount": 1, "warnings": [],
        "text": {"lines": ["Mara Example"], "hidden": [], "transformed": [], "reversed": [], "links": [],
            "metadata": [], "imageCount": 0},
    }});
    let mut frames = Vec::new();
    for frame in [head.to_string().as_bytes(), &pdf, &preview] {
        frames.extend_from_slice(&(frame.len() as u64).to_le_bytes());
        frames.extend_from_slice(frame);
    }
    let output = dir.join("frames");
    std::fs::write(&output, frames).unwrap();
    let script = dir.join("worker");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nexec /bin/cat '{}'\n", output.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    (script, pdf)
}

#[tokio::test]
async fn the_transport_carries_a_pdf_at_its_limit() {
    let dir = tempfile::tempdir().unwrap();
    let (exe, pdf) = fake_worker(dir.path(), nexus_tailor::MAX_PDF_BYTES);
    let server = Server::start_with(Renderer::Subprocess {
        exe,
        rss_limit_bytes: nexus_tailor::DEFAULT_RSS_LIMIT_BYTES,
        timeout: Duration::from_secs(20),
    })
    .await;
    server.call("upsert_cv_profile", profile()).await.ok();
    owner_lock(&server).await;
    let done = render(&server, json!({})).await;
    assert!(!done.is_error, "{:?}", done.content.first());
    let returned = STANDARD
        .decode(done.content[1]["resource"]["blob"].as_str().unwrap())
        .unwrap();
    assert_eq!(returned, pdf);
    assert_eq!(done.text_json()["pdfBytes"], nexus_tailor::MAX_PDF_BYTES);

    let saved = render(
        &server,
        json!({"saveToApplicationId": server.application("Acme").await}),
    )
    .await;
    assert!(!saved.is_error);
}
