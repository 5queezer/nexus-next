//! The CV tools over real MCP, with invented people and an in-process renderer.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use common::*;
use nexus_core::repo::cv_facts::{self, OwnerFacts};
use nexus_mcp::{Renderer, owner};
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

/// The owner reads the head and approves it, through what no tool reaches.
async fn owner_approve(server: &Server, variant: &str) {
    let review = owner::review(server.state.clone(), variant).await.unwrap();
    let approved = owner::approve(
        server.state.clone(),
        variant,
        review.revision,
        &review.source_sha256,
        &review.facts_sha256,
        "cli",
    )
    .await;
    assert_ne!(approved.is_error, Some(true), "{approved:?}");
}

async fn approvals(server: &Server) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM cv_approvals")
        .fetch_one(server.db.pool())
        .await
        .unwrap()
}

fn item_tokens(reply: &Value) -> Vec<String> {
    reply["reviewItems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["token"].as_str().unwrap().to_owned())
        .collect()
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
    assert_eq!(first["state"], "draft", "{first}");
    assert_eq!(first["approvalPath"], "/cv/variants/default#cv-approval");
    assert!(first["reviewItems"].is_array(), "{first}");
    assert_eq!(first["truncated"], 0);

    let stale = patch(&server, 0, "Freight engineer", "Engineer").await;
    assert_eq!(stale.code(), "cv_revision_conflict");
    assert_eq!(stale.json()["error"]["currentRevision"], 1);
    assert_eq!(stale.json()["error"]["expectedRevision"], 0);

    let flagged = patch(&server, 1, "Freight engineer", "Kubernetes engineer")
        .await
        .ok();
    assert_eq!(flagged["revision"], 2);
    assert!(
        item_tokens(&flagged).contains(&"Kubernetes".to_owned()),
        "{flagged}"
    );
    assert!(flagged.get("reviewTokens").is_none());
    let stored: String = sqlx::query_scalar("SELECT warnings FROM cv_revisions WHERE number = 2")
        .fetch_one(server.db.pool())
        .await
        .unwrap();
    assert!(stored.contains("Kubernetes"), "{stored}");
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
async fn hard_rules_refuse_and_store_nothing() {
    let server = locked_server().await;
    let before = state(&server).await;
    for (new, rule, token) in [
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
        ("Backend engineer, by lOO%", "digitLookalike", "lOO"),
        (
            "Backend engineer at evil.example",
            "contactDetails",
            "evil.example",
        ),
        ("Backend #rotate(90deg)[engineer]", "transform", "engineer"),
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
    assert_eq!(state(&server).await, before);
}

/// The review's case: what no list can judge stores, and waits for the owner.
#[tokio::test]
async fn a_false_claim_stores_as_a_draft_and_is_listed() {
    let server = locked_server().await;
    let mut old = "Backend engineer".to_owned();
    for (expected, (new, rule, token)) in [
        ("Backend engineer with 12 years", "numbers", "12"),
        ("Eight years as a backend engineer", "numberWords", "Eight"),
        ("Senior backend engineer", "claimWords", "Senior"),
        (
            "Brightpath Logistics Senior Architect 2010",
            "entityBinding",
            "2010",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let reply = patch(&server, expected as i64, &old, new).await.ok();
        old = new.to_owned();
        assert_eq!(reply["state"], "draft", "{new}");
        assert!(
            reply["reviewItems"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["rule"] == rule && i["token"] == token),
            "{new}: {reply}"
        );
    }
    assert_eq!(approvals(&server).await, 0);
}

#[tokio::test]
async fn no_argument_skips_a_hard_rule_or_approves() {
    let server = locked_server().await;
    let before = state(&server).await;
    for extra in [
        "force",
        "skipCheck",
        "override",
        "allowViolations",
        "unsafe",
        "approve",
        "approved",
        "state",
    ] {
        let mut args = json!({
            "expectedRevision": 0,
            "edits": [{"oldText": "Backend engineer", "newText": "A team player"}],
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
    let one = patch(
        &server,
        0,
        "Backend engineer",
        "Backend engineer of frobnicators",
    )
    .await
    .ok();
    assert_eq!(one["revision"], 1);
    patch(&server, 1, " of frobnicators", "").await.ok();
    assert!(!render(&server, json!({"revision": 1})).await.is_error);

    owner_set(&server, json!({"bannedPhrases": ["frobnicators"]})).await;
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
async fn a_draft_renders_stamped_with_no_pdf_and_cannot_be_saved() {
    let server = locked_server().await;
    patch(&server, 0, "Backend engineer", "Freight engineer")
        .await
        .ok();
    let done = render(&server, json!({"previewPages": 1})).await;
    assert!(!done.is_error, "{:?}", done.content);
    assert_eq!(done.content.len(), 2, "no resource: {:?}", done.content);
    let summary = done.text_json();
    assert_eq!(summary["state"], "draft");
    assert_eq!(summary["approvalPath"], "/cv/variants/default#cv-approval");
    assert!(summary["reviewItems"].is_array());
    assert!(summary.get("pdfBytes").is_none());
    assert_eq!(done.content[1]["type"], "image");

    let app = server.application("Acme").await;
    let saved = render(&server, json!({"saveToApplicationId": app})).await;
    assert!(saved.is_error);
    let error = &saved.text_json()["error"];
    assert_eq!(error["code"], "cv_approval_required");
    assert_eq!(error["revision"], 1);
    assert_eq!(error["variant"], "default");
    assert_eq!(error["approvalPath"], "/cv/variants/default#cv-approval");
    assert_eq!(
        server
            .call("list_documents", json!({"applicationId": app}))
            .await
            .ok(),
        json!([]),
    );
}

#[tokio::test]
async fn an_approved_render_returns_a_resource_and_images_and_can_save_a_document() {
    let server = locked_server().await;
    patch(&server, 0, "Backend engineer", "Freight engineer")
        .await
        .ok();
    let stamped = render(&server, json!({"previewPages": 1})).await;
    owner_approve(&server, "default").await;
    let done = render(&server, json!({"previewPages": 1})).await;
    assert!(!done.is_error, "{:?}", done.content);
    assert_eq!(done.content.len(), 3);
    let summary = done.text_json();
    assert_eq!(summary["revision"], 1);
    assert_eq!(summary["pageCount"], 1);
    assert_eq!(summary["state"], "approved");
    assert_eq!(summary["reviewItems"], json!([]));
    assert_eq!(summary["approvalPath"], Value::Null);
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
    assert_ne!(
        done.content[2]["data"], stamped.content[1]["data"],
        "the stamp is gone"
    );

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
    let many: String = (0..200)
        .map(|n| format!("#text(fill: white)[w{n}] "))
        .collect();
    let new = format!("Backend engineer. {many}");
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

#[tokio::test]
async fn a_long_list_of_review_items_is_cut_and_says_so() {
    let server = locked_server().await;
    let numbers: String = (1000..1200).map(|n| format!("{n} ")).collect();
    let new = format!("Backend engineer. {numbers}");
    let reply = patch(&server, 0, "Backend engineer", &new).await.ok();
    assert_eq!(reply["reviewItems"].as_array().unwrap().len(), 50);
    assert!(reply["truncated"].as_u64().unwrap() >= 150, "{reply}");
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

    // No lock needed, as old Nexus had none.
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
    assert_eq!(linked_to(&b).await, std::slice::from_ref(&shared));
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
async fn generate_refuses_what_a_hard_rule_refuses() {
    let server = locked_server().await;
    let app = server.application("Acme").await;
    let before = state(&server).await;
    let args = json!({"applicationId": app, "experienceIds": ["brightpath"],
        "skillCategories": [], "profileOverride": "A team player"});
    assert_eq!(
        server.call("generate_tailored_cv", args).await.code(),
        "cv_fact_violation",
    );
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

/// Finding 12's laundering: an agent's wording can't reach a document through the profile or an
/// override. The owner's own wording, unchanged since the lock, goes through at once.
#[tokio::test]
async fn generate_stores_a_document_only_for_what_the_owner_approved() {
    let server = locked_server().await;
    let app = server.application("Acme").await;
    let variant = format!("application-{app}");
    let args =
        json!({"applicationId": app, "experienceIds": ["brightpath"], "skillCategories": []});
    let documents = async |server: &Server| {
        let listed = server
            .call("list_documents", json!({"applicationId": app}))
            .await
            .ok();
        listed.as_array().unwrap().len()
    };

    // The owner's wording as locked: the lock approved it.
    let first = server.call("generate_tailored_cv", args.clone()).await.ok();
    assert_eq!(first["message"], "CV generated successfully");
    assert_eq!(documents(&server).await, 1);
    let by: Vec<String> = sqlx::query_scalar("SELECT approved_by FROM cv_approvals")
        .fetch_all(server.db.pool())
        .await
        .unwrap();
    assert_eq!(by, ["lock"]);

    // An override is an agent's wording.
    let mut with_override = args.clone();
    with_override["profileOverride"] = "Stanford alumnus with 12 years at Google".into();
    let held = server
        .call("generate_tailored_cv", with_override.clone())
        .await;
    assert_eq!(held.code(), "cv_approval_required");
    let error = &held.json()["error"];
    assert_eq!(error["variant"], variant);
    assert_eq!(error["revision"], 2);
    assert_eq!(
        error["approvalPath"],
        format!("/cv/variants/{variant}#cv-approval")
    );
    assert_eq!(
        documents(&server).await,
        1,
        "no document, and the old one stays"
    );
    // Trying again adds nothing to approve.
    let again = server
        .call("generate_tailored_cv", with_override.clone())
        .await;
    assert_eq!(again.json()["error"]["revision"], 2);

    // So is a locked upsert: this is the laundering of finding 12.
    let mut laundered = profile();
    laundered["profile"] = "Harvard alumnus and Google veteran.".into();
    server.call("upsert_cv_profile", laundered).await.ok();
    let held = server.call("generate_tailored_cv", args.clone()).await;
    assert_eq!(held.code(), "cv_approval_required");
    assert_eq!(held.json()["error"]["revision"], 3);
    assert_eq!(documents(&server).await, 1);

    // The owner approves it, and the same call now stores, with no re-approval loop.
    owner_approve(&server, &variant).await;
    let stored = server.call("generate_tailored_cv", args.clone()).await.ok();
    assert_eq!(stored["message"], "CV generated successfully");
    assert_eq!(documents(&server).await, 1);
    let listed = server
        .call("list_cv_revisions", json!({"variant": variant}))
        .await
        .ok();
    assert_eq!(listed["total"], 3, "the same text adds no revision");
    assert_eq!(listed["revisions"][0]["state"], "approved");
    assert_eq!(listed["revisions"][1]["state"], "draft");
    assert_eq!(
        listed["revisions"][2]["state"], "draft",
        "what the lock approved here, the owner has now replaced"
    );
}

/// Text identical to the newest approved text is approved: no re-approval loop, on any path.
#[tokio::test]
async fn identical_text_is_approved_however_it_arrives() {
    let server = locked_server().await;
    patch(&server, 0, "Backend engineer", "Freight engineer")
        .await
        .ok();
    owner_approve(&server, "default").await;
    assert_eq!(
        server.call("get_cv_source", json!({})).await.ok()["state"],
        "approved"
    );
    let draft = patch(&server, 1, "Freight engineer", "Routing engineer")
        .await
        .ok();
    assert_eq!(draft["state"], "draft");

    let reverted = server
        .call("revert_cv", json!({"toRevision": 1, "expectedRevision": 2}))
        .await
        .ok();
    assert_eq!(reverted["state"], "approved");
    assert_eq!(reverted["reviewItems"], json!([]));

    let copied = server
        .call(
            "patch_cv",
            json!({"variant": "copy", "createFrom": "default", "expectedRevision": 3,
                "edits": [], "summary": "s"}),
        )
        .await
        .ok();
    assert_eq!(copied["state"], "approved");
    let listed = server.call("list_cv_revisions", json!({})).await.ok();
    let states: Vec<&Value> = listed["revisions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| &r["state"])
        .collect();
    assert_eq!(states, ["approved", "draft", "approved"]);
    assert_eq!(approvals(&server).await, 1);
}

/// The review compares with the text last approved, however many drafts came between.
#[tokio::test]
async fn items_and_the_diff_compare_with_the_last_approved_text_not_the_parent() {
    let server = locked_server().await;
    patch(&server, 0, "Backend engineer", "Freight engineer")
        .await
        .ok();
    owner_approve(&server, "default").await;
    let one = patch(
        &server,
        1,
        "Freight engineer",
        "Freight engineer with 11 cats",
    )
    .await
    .ok();
    let two = patch(&server, 2, "with 11 cats", "with 11 cats and 22 dogs")
        .await
        .ok();
    let three = patch(&server, 3, "and 22 dogs", "and 22 dogs and 33 owls")
        .await
        .ok();
    let tokens = item_tokens(&three);
    for token in ["11", "22", "33", "cats", "dogs", "owls"] {
        assert!(tokens.iter().any(|t| t == token), "{token} in {tokens:?}");
    }
    assert!(item_tokens(&one).contains(&"11".to_owned()));
    assert!(item_tokens(&two).contains(&"11".to_owned()));
    let review = owner::review(server.state.clone(), "default")
        .await
        .unwrap();
    assert_eq!(review.baseline_revision, Some(1));
    assert!(
        review
            .baseline
            .as_deref()
            .unwrap()
            .contains("Freight engineer")
    );
    assert!(!review.baseline.as_deref().unwrap().contains("cats"));
    assert!(review.rendered_text.contains("33 owls"));
    assert!(review.source_diff.contains("-") && review.source_diff.contains("33 owls"));

    // Approved again, that is what the next draft reads against.
    owner_approve(&server, "default").await;
    let next = patch(&server, 4, "and 33 owls", "and 33 owls and 44 bats")
        .await
        .ok();
    let tokens = item_tokens(&next);
    assert!(tokens.iter().any(|t| t == "44"), "{tokens:?}");
    assert!(
        !tokens.iter().any(|t| t == "owls"),
        "approved already: {tokens:?}"
    );
    let review = owner::review(server.state.clone(), "default")
        .await
        .unwrap();
    assert_eq!(review.baseline_revision, Some(4));
}

#[tokio::test]
async fn approval_is_bound_to_the_exact_revision_and_facts() {
    let server = locked_server().await;
    patch(&server, 0, "Backend engineer", "Freight engineer")
        .await
        .ok();
    let review = owner::review(server.state.clone(), "default")
        .await
        .unwrap();
    let approve = |revision: i64, source: String, facts: String| {
        let state = server.state.clone();
        async move { owner::approve(state, "default", revision, &source, &facts, "dashboard").await }
    };
    let code = |result: &nexus_mcp::CallToolResult| {
        result
            .structured_content
            .as_ref()
            .and_then(|value| value["error"]["code"].as_str().map(str::to_owned))
            .unwrap_or_default()
    };
    let (source, facts) = (review.source_sha256.clone(), review.facts_sha256.clone());
    for (revision, source_hash, facts_hash) in [
        (1, "wrong".to_owned(), facts.clone()),
        (1, source.clone(), "wrong".to_owned()),
    ] {
        let refused = approve(revision, source_hash, facts_hash).await;
        assert_eq!(code(&refused), "cv_approval_stale");
    }
    assert_eq!(
        code(&approve(9, source.clone(), facts.clone()).await),
        "cv_revision_not_found"
    );
    assert_eq!(approvals(&server).await, 0);

    // A patch landing after the page loaded: the page's revision is no longer the one to approve.
    patch(&server, 1, "Freight engineer", "Routing engineer")
        .await
        .ok();
    let refused = approve(1, source, facts).await;
    assert_eq!(code(&refused), "cv_approval_stale");
    assert_eq!(
        refused.structured_content.unwrap()["error"]["currentRevision"],
        2
    );
    assert_eq!(approvals(&server).await, 0);
    assert_eq!(
        server.call("get_cv_source", json!({})).await.ok()["state"],
        "draft"
    );
}

/// Whichever wins, the patch that landed is a draft, and only the revision read was approved.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_patch_landing_during_approval_is_never_approved_by_accident() {
    let server = locked_server().await;
    patch(&server, 0, "Backend engineer", "Freight engineer")
        .await
        .ok();
    for round in 0..6_i64 {
        let number = round + 1;
        let review = owner::review(server.state.clone(), "default")
            .await
            .unwrap();
        assert_eq!(review.revision, number);
        let new = format!("Freight engineer {}", "x".repeat(round as usize + 1));
        let old = if round == 0 {
            "Freight engineer".to_owned()
        } else {
            format!("Freight engineer {}", "x".repeat(round as usize))
        };
        let (approved, patched) = tokio::join!(
            owner::approve(
                server.state.clone(),
                "default",
                review.revision,
                &review.source_sha256,
                &review.facts_sha256,
                "dashboard",
            ),
            patch(&server, number, &old, &new),
        );
        let patched = patched.ok();
        assert_eq!(patched["state"], "draft", "round {round}");
        assert_eq!(
            server.call("get_cv_source", json!({})).await.ok()["state"],
            "draft",
            "round {round}: the head is the patch, which no one approved"
        );
        let approved_row: i64 =
            sqlx::query_scalar("SELECT count(*) FROM cv_approvals WHERE revision_number = ?")
                .bind(number)
                .fetch_one(server.db.pool())
                .await
                .unwrap();
        assert_eq!(
            approved.is_error != Some(true),
            approved_row == 1,
            "round {round}: {approved:?}"
        );
    }
}

/// The window a look at the head can't close: an approval that read the head while a patch was still
/// compiling, and whose own compile, one at a time, ends after that patch has stored. The approval can
/// also win the commit, which is fine, so this tries until it has seen the window.
/// Each try has its own text, or Typst's memoizing would make the patch finish before the approval.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_approval_refuses_a_revision_that_stopped_being_the_head_during_its_compile() {
    for round in 0..12 {
        let slow = format!(
            "Freight engineer {round} #for a in range(1500) {{ for b in range(1500) {{ }} }}"
        );
        let server = locked_server().await;
        patch(&server, 0, "Backend engineer", "Freight engineer")
            .await
            .ok();
        let review = owner::review(server.state.clone(), "default")
            .await
            .unwrap();
        let (patched, approved) =
            tokio::join!(patch(&server, 1, "Freight engineer", &slow), async {
                tokio::time::sleep(Duration::from_millis(500)).await;
                owner::approve(
                    server.state.clone(),
                    "default",
                    review.revision,
                    &review.source_sha256,
                    &review.facts_sha256,
                    "dashboard",
                )
                .await
            });
        assert_eq!(patched.ok()["state"], "draft");
        if approved.is_error != Some(true) {
            assert_eq!(approvals(&server).await, 1, "it reached the compile first");
            continue;
        }
        let error = approved.structured_content.unwrap();
        // On a busy machine the patch's compile can outlast the wait for the slot: refused, unrecorded.
        if error["error"]["code"] == "cv_render_busy" {
            assert_eq!(approvals(&server).await, 0, "{error}");
            continue;
        }
        assert_eq!(error["error"]["code"], "cv_approval_stale", "{error}");
        assert_eq!(error["error"]["currentRevision"], 2);
        assert_eq!(approvals(&server).await, 0);
        return;
    }
    panic!("the approval never met a patch in its compile");
}

/// Any changed fact invalidates what was built on it. An unlock and relock with no change doesn't.
#[tokio::test]
async fn approval_is_bound_to_the_facts_it_was_given_under() {
    let server = locked_server().await;
    patch(&server, 0, "Backend engineer", "Freight engineer")
        .await
        .ok();
    owner_approve(&server, "default").await;
    let is_approved = async |server: &Server| {
        server.call("get_cv_source", json!({})).await.ok()["state"] == "approved"
    };
    assert!(is_approved(&server).await);

    // Vocabulary and the like can't render, so they invalidate nothing.
    owner_set(&server, json!({"vocabulary": ["Kubernetes"]})).await;
    assert!(is_approved(&server).await);

    // An unlock is a draft for as long as it lasts, and a relock with no change restores it.
    let mut tx = server.db.begin_write().await.unwrap();
    cv_facts::unlock(&mut tx, "test").await.unwrap();
    tx.commit().await.unwrap();
    assert!(
        !is_approved(&server).await,
        "no approval without locked facts"
    );
    let unlocked = render(&server, json!({})).await;
    assert_eq!(unlocked.text_json()["state"], "draft");
    owner_lock(&server).await;
    assert!(is_approved(&server).await);
    assert_eq!(render(&server, json!({})).await.content.len(), 3);

    // A language is a fact.
    owner_set(
        &server,
        json!({"languages": [{"name": "German", "level": "C2"}]}),
    )
    .await;
    assert!(!is_approved(&server).await);
    let draft = render(&server, json!({})).await;
    assert_eq!(draft.text_json()["state"], "draft");
    assert_eq!(draft.content.len(), 2, "no PDF");
    let app = server.application("Acme").await;
    let saved = render(&server, json!({"saveToApplicationId": app})).await;
    assert_eq!(saved.text_json()["error"]["code"], "cv_approval_required");
    assert_eq!(approvals(&server).await, 1);
}

/// `origin` and the caller's `source` are text: neither grants anything. Only the owner's path does.
#[tokio::test]
async fn nothing_a_caller_says_about_itself_approves() {
    let server = locked_server().await;
    let as_dashboard = nexus_mcp::call_tool(
        server.state.clone(),
        &["mcp:tools"],
        "dashboard",
        "patch_cv",
        json!({"expectedRevision": 0, "summary": "s", "origin": "from the dashboard",
            "edits": [{"oldText": "Backend engineer", "newText": "Freight engineer"}]}),
    )
    .await;
    let reply = as_dashboard.structured_content.unwrap();
    assert_eq!(reply["state"], "draft", "{reply}");
    assert_eq!(approvals(&server).await, 0);
}

#[tokio::test]
async fn the_owners_save_on_approved_text_is_stored_and_approved_in_one_step() {
    let server = locked_server().await;
    let (number, source) = head(&server, "default").await;
    assert_eq!(number, 0);
    let unchanged = owner::save(
        server.state.clone(),
        &json!({"variant": "default", "expectedRevision": 0, "summary": "Edited by hand",
            "origin": "dashboard", "edits": [{"oldText": source, "newText": source}]}),
    )
    .await;
    let reply = unchanged.structured_content.unwrap();
    assert_eq!(reply["state"], "draft", "{reply}");
    assert_eq!(
        reply["revision"], 1,
        "the starting text can be stored as it stands"
    );
    owner_approve(&server, "default").await;
    let edited = owner::save(
        server.state.clone(),
        &json!({"variant": "default", "expectedRevision": 1, "summary": "Edited by hand",
            "origin": "dashboard", "edits": [{"oldText": "Backend engineer", "newText": "Freight engineer"}]}),
    )
    .await;
    let reply = edited.structured_content.unwrap();
    assert_eq!(
        (&reply["state"], &reply["revision"]),
        (&json!("approved"), &json!(2)),
        "{reply}"
    );
    let by: String =
        sqlx::query_scalar("SELECT approved_by FROM cv_approvals ORDER BY id DESC LIMIT 1")
            .fetch_one(server.db.pool())
            .await
            .unwrap();
    assert_eq!(by, "dashboard");

    let hard = owner::save(
        server.state.clone(),
        &json!({"variant": "default", "expectedRevision": 2, "summary": "s", "origin": "dashboard",
            "edits": [{"oldText": "Freight engineer", "newText": "A team player"}]}),
    )
    .await;
    let refused = hard.structured_content.unwrap();
    assert_eq!(
        refused["error"]["code"], "cv_fact_violation",
        "an approval overrides no hard rule"
    );
    assert_eq!(approvals(&server).await, 2);
}

/// Hard rules are read again at approval: the owner's approval overrides none of them.
#[tokio::test]
async fn approving_rechecks_the_hard_rules() {
    let server = locked_server().await;
    patch(&server, 0, "Backend engineer", "Frobnicating engineer")
        .await
        .ok();
    let review = owner::review(server.state.clone(), "default")
        .await
        .unwrap();
    owner_set(&server, json!({"bannedPhrases": ["Frobnicating"]})).await;
    let refused = owner::approve(
        server.state.clone(),
        "default",
        review.revision,
        &review.source_sha256,
        &review.facts_sha256,
        "cli",
    )
    .await;
    assert_eq!(
        refused.structured_content.unwrap()["error"]["code"],
        "cv_fact_violation"
    );
    assert_eq!(approvals(&server).await, 0);
}

/// What can change under an approved revision can't be read by its source.
#[tokio::test]
async fn a_source_cannot_print_what_the_facts_hash_does_not_cover() {
    let server = locked_server().await;
    owner_set(
        &server,
        json!({"vocabulary": ["SecretWord"], "allowedTokens": ["secret.example.org"]}),
    )
    .await;
    let new = "Engineer #json(\"/facts.json\").vocabulary.len() #json(\"/facts.json\").allowedTokens.len()";
    patch(&server, 0, "Backend engineer", new).await.ok();
    let review = owner::review(server.state.clone(), "default")
        .await
        .unwrap();
    assert!(
        review.rendered_text.contains("Engineer 0 0"),
        "{}",
        review.rendered_text
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
        ])
    );
    assert!(refused.json()["error"].get("tokens").is_none());
    assert_eq!(facts_and_history(state(&server).await), before);

    let mut wording = profile();
    wording["profile"] = "Worked 15 years on S3.".into();
    server.call("upsert_cv_profile", wording).await.ok();
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
    let (_, source) = head(&server, "default").await;
    let saved = owner::save(
        server.state.clone(),
        &json!({"variant": "default", "expectedRevision": 0, "summary": "s", "origin": "dashboard",
            "edits": [{"oldText": source, "newText": source}]}),
    )
    .await;
    assert_ne!(saved.is_error, Some(true), "{saved:?}");
    owner_approve(&server, "default").await;
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

// The second gate review, and the rule that a replaced approval stops counting.

fn generate_all(app: &str) -> Value {
    json!({"applicationId": app, "experienceIds": ["brightpath", "harborline"],
        "skillCategories": ["Languages"], "includeProjects": true})
}

async fn owner_unlock(server: &Server) {
    let mut tx = server.db.begin_write().await.unwrap();
    cv_facts::unlock(&mut tx, "test").await.unwrap();
    tx.commit().await.unwrap();
}

async fn lock_preview(server: &Server) -> cv_facts::LockPreview {
    let mut conn = server.db.conn().await.unwrap();
    cv_facts::lock_preview(&mut conn).await.unwrap()
}

async fn document_count(server: &Server) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM documents")
        .fetch_one(server.db.pool())
        .await
        .unwrap()
}

type ProfileChange = fn(&mut Value);

/// Finding 1: which string plays which part is wording too, on every path that compares it.
#[tokio::test]
async fn a_regroup_a_move_or_a_tier_is_a_change_of_wording_everywhere() {
    let cases: [(&str, ProfileChange, &str); 8] = [
        (
            "a skill moved into another group",
            |p| {
                p["skills"] = json!([
                    {"category": "Languages", "items": ["Rust", "Go", "Tools", "Docker"]},
                ]);
            },
            "skills[0].items[2]",
        ),
        (
            "two skills swapped",
            |p| p["skills"][0]["items"] = json!(["Go", "Rust"]),
            "skills[0].items[0]",
        ),
        (
            "a category renamed to another's name",
            |p| p["skills"][0]["category"] = "Tools".into(),
            "skills[0].category",
        ),
        (
            "two bullets swapped",
            |p| {
                let bullets = p["experience"][0]["bullets"].as_array_mut().unwrap();
                bullets.swap(0, 1);
            },
            "experience[0:brightpath].bullets[0]",
        ),
        (
            "a bullet moved to another job",
            |p| {
                let moved = p["experience"][0]["bullets"].as_array_mut().unwrap().pop();
                p["experience"][1]["bullets"] = json!([moved]);
            },
            "experience[1:harborline].bullets[0]",
        ),
        (
            "a tier",
            |p| p["experience"][0]["tier"] = 3.into(),
            "experience[0:brightpath].tier",
        ),
        (
            "a job left out with no words of its own",
            |p| {
                p["experience"].as_array_mut().unwrap().pop();
            },
            "experience[1:harborline]",
        ),
        (
            "a project's parts swapped",
            |p| {
                p["projects"][0]["name"] = "Rust".into();
                p["projects"][0]["stack"] = "tidy".into();
            },
            "projects[0].name",
        ),
    ];
    for (name, change, field) in cases {
        let server = locked_server().await;
        let mut changed = profile();
        change(&mut changed);
        let upserted = server.call("upsert_cv_profile", changed).await;
        assert!(!upserted.is_error, "{name}: {}", upserted.json());
        let app = server.application("Acme").await;
        let generated = server
            .call("generate_tailored_cv", generate_all(&app))
            .await;
        assert_eq!(generated.code(), "cv_approval_required", "{name}");
        assert_eq!(document_count(&server).await, 0, "{name}");
        owner_unlock(&server).await;
        let preview = lock_preview(&server).await;
        assert!(
            preview.wording.iter().any(|change| change.field == field),
            "{name}: {:?}",
            preview.wording
        );
        assert!(preview.needs_confirmation(), "{name}");
    }
}

/// The hash names the list, so a regroup after a printout is seen, not confirmed.
#[tokio::test]
async fn a_regroup_moves_the_locks_hash() {
    let server = locked_server().await;
    owner_unlock(&server).await;
    let mut once = profile();
    once["profile"] = "Backend engineer for freight.".into();
    server.call("upsert_cv_profile", once.clone()).await.ok();
    let shown = lock_preview(&server).await.hash();
    once["skills"] = json!([{"category": "Languages", "items": ["Rust", "Go", "Tools", "Docker"]}]);
    server.call("upsert_cv_profile", once).await.ok();
    assert_ne!(lock_preview(&server).await.hash(), shown);
}

/// A row locked before wording had structure shows as changed, and nothing counts as approved by it.
#[tokio::test]
async fn wording_locked_as_a_flat_list_shows_as_changed() {
    let server = locked_server().await;
    sqlx::query(
        r#"UPDATE cv_facts SET locked_wording = '[["profile","Backend engineer building freight software."],["skills","Languages"]]'"#,
    )
    .execute(server.db.pool())
    .await
    .unwrap();
    let app = server.application("Acme").await;
    let generated = server
        .call("generate_tailored_cv", generate_all(&app))
        .await;
    assert_eq!(generated.code(), "cv_approval_required");
    owner_unlock(&server).await;
    assert!(lock_preview(&server).await.needs_confirmation());
}

/// A lock approves wording, not arrangement.
#[tokio::test]
async fn a_lock_approves_a_plain_selection_and_no_other_arrangement() {
    let server = locked_server().await;
    for (name, jobs, categories) in [
        (
            "a job twice",
            json!(["brightpath", "brightpath"]),
            json!([]),
        ),
        ("no jobs", json!([]), json!(["Languages"])),
        ("only unknown jobs", json!(["ghost"]), json!(["Languages"])),
        (
            "a category twice",
            json!(["brightpath"]),
            json!(["Tools", "Tools"]),
        ),
    ] {
        let app = server.application(name).await;
        let reply = server
            .call(
                "generate_tailored_cv",
                json!({"applicationId": app, "experienceIds": jobs, "skillCategories": categories}),
            )
            .await;
        assert_eq!(reply.code(), "cv_approval_required", "{name}");
    }
    assert_eq!(document_count(&server).await, 0);
    for (name, jobs) in [
        ("as locked", json!(["brightpath", "harborline"])),
        ("in another order", json!(["harborline", "brightpath"])),
        ("some of them", json!(["harborline"])),
    ] {
        let app = server.application(name).await;
        let reply = server
            .call(
                "generate_tailored_cv",
                json!({"applicationId": app, "experienceIds": jobs, "skillCategories": ["Tools"]}),
            )
            .await;
        assert_eq!(reply.ok()["message"], "CV generated successfully", "{name}");
    }
    assert_eq!(document_count(&server).await, 3);
}

/// Text A approved, corrected to B and approved: revisions 1 and 2.
async fn replaced_once(server: &Server) {
    patch(
        server,
        0,
        "Built the routing service.",
        "Led a team of 20 engineers.",
    )
    .await
    .ok();
    owner_approve(server, "default").await;
    patch(server, 1, "a team of 20", "a team of 12").await.ok();
    owner_approve(server, "default").await;
}

async fn revert(server: &Server, to: i64, expected: i64) -> Value {
    server
        .call(
            "revert_cv",
            json!({"variant": "default", "toRevision": to, "expectedRevision": expected}),
        )
        .await
        .ok()
}

/// The owner's correction holds: a revert to what they replaced is a draft to read, with the
/// difference in view, and no path gives a real PDF of the old text by naming its revision.
#[tokio::test]
async fn an_approval_the_owner_replaced_stops_counting_in_that_variant() {
    let server = locked_server().await;
    replaced_once(&server).await;
    let app = server.application("Acme").await;

    let reverted = revert(&server, 1, 2).await;
    assert_eq!(reverted["state"], "draft", "{reverted}");
    assert_eq!(reverted["approvalPath"], "/cv/variants/default#cv-approval");
    assert!(
        item_tokens(&reverted).contains(&"20".to_owned()),
        "{reverted}"
    );

    let review = owner::review(server.state.clone(), "default")
        .await
        .unwrap();
    assert!(!review.approved);
    assert_eq!(review.baseline_revision, Some(2));
    assert!(review.baseline.as_deref().unwrap().contains("a team of 12"));
    assert!(review.rendered_text.contains("a team of 20"));

    let by_number = render(&server, json!({"revision": 1, "previewPages": 1})).await;
    assert_eq!(by_number.text_json()["state"], "draft");
    assert!(
        by_number
            .content
            .iter()
            .all(|block| block["type"] != "resource"),
        "no real PDF of replaced text"
    );
    let head = render(&server, json!({"previewPages": 1})).await;
    assert!(head.content.iter().all(|block| block["type"] != "resource"));
    let saved = render(&server, json!({"revision": 1, "saveToApplicationId": app})).await;
    assert_eq!(saved.text_json()["error"]["code"], "cv_approval_required");
    assert_eq!(document_count(&server).await, 0);

    let listed = server
        .call("list_cv_revisions", json!({"variant": "default"}))
        .await
        .ok();
    let states: Vec<&str> = listed["revisions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|revision| revision["state"].as_str().unwrap())
        .collect();
    assert_eq!(states, ["draft", "approved", "draft"], "newest first");

    // The newest approved text keeps working by its number.
    let newest = render(&server, json!({"revision": 2, "saveToApplicationId": app})).await;
    assert_eq!(newest.text_json()["state"], "approved");
    assert_eq!(document_count(&server).await, 1);

    // Undoing back to the newest approved text needs no tap, which dictation's undo relies on.
    let undone = revert(&server, 2, 3).await;
    assert_eq!(undone["state"], "approved", "{undone}");
    let before = approvals(&server).await;
    assert_eq!(before, 2, "no approval was written by reverting");

    // The owner can choose the old text again. Only then does it count, and the other stops.
    assert_eq!(revert(&server, 1, 4).await["state"], "draft");
    owner_approve(&server, "default").await;
    assert_eq!(approvals(&server).await, 3);
    assert_eq!(head_state(&server, "default").await, "approved");
    let old_again = render(&server, json!({"revision": 1, "previewPages": 0})).await;
    assert_eq!(old_again.text_json()["state"], "approved");
    let now_replaced = render(&server, json!({"revision": 2, "previewPages": 0})).await;
    assert_eq!(now_replaced.text_json()["state"], "draft");
}

async fn head_state(server: &Server, variant: &str) -> String {
    let got = server
        .call("get_cv_source", json!({"variant": variant}))
        .await
        .ok();
    got["state"].as_str().unwrap().to_owned()
}

/// A variant with no approval of its own takes approved text from elsewhere, unless that text has
/// been replaced there too: otherwise a new variant would bring back any text ever approved.
#[tokio::test]
async fn a_new_variant_takes_approved_text_but_not_replaced_text() {
    let server = locked_server().await;
    replaced_once(&server).await;
    let copied = server
        .call(
            "patch_cv",
            json!({"variant": "platform", "createFrom": "default", "expectedRevision": 2,
                "edits": [], "summary": "New variant"}),
        )
        .await
        .ok();
    assert_eq!(copied["state"], "approved", "{copied}");

    let replaced = server
        .call(
            "patch_cv",
            json!({"variant": "older", "createFrom": "default", "expectedRevision": 2,
                "edits": [{"oldText": "a team of 12", "newText": "a team of 20"}],
                "summary": "The old text, in a new variant"}),
        )
        .await
        .ok();
    assert_eq!(replaced["state"], "draft", "{replaced}");
    let app = server.application("Acme").await;
    let saved = render(
        &server,
        json!({"variant": "older", "saveToApplicationId": app}),
    )
    .await;
    assert_eq!(saved.text_json()["error"]["code"], "cv_approval_required");

    // Once a variant has an approval of its own, only that counts there.
    owner_approve(&server, "older").await;
    assert_eq!(head_state(&server, "older").await, "approved");
    assert_eq!(
        head_state(&server, "default").await,
        "approved",
        "each its own"
    );
}

/// An approval under facts the owner has since changed is no baseline: the line it holds may be one
/// the owner corrected, and would read as unchanged. The whole CV shows instead.
#[tokio::test]
async fn an_approval_under_old_facts_is_no_baseline_and_no_approval() {
    let server = locked_server().await;
    patch(
        &server,
        0,
        "Built the routing service.",
        "Built the routing engine.",
    )
    .await
    .ok();
    owner_approve(&server, "default").await;
    owner_set(
        &server,
        json!({"languages": [{"name": "German", "level": "B2"}]}),
    )
    .await;
    assert_eq!(head_state(&server, "default").await, "draft");
    let review = owner::review(server.state.clone(), "default")
        .await
        .unwrap();
    assert_eq!(review.baseline, None);
    assert_eq!(review.baseline_revision, None);
    patch(
        &server,
        1,
        "Built the routing engine.",
        "Built the routing core.",
    )
    .await
    .ok();
    let review = owner::review(server.state.clone(), "default")
        .await
        .unwrap();
    assert_eq!(review.baseline, None);
    let reverted = revert(&server, 1, 2).await;
    assert_eq!(reverted["state"], "draft");
}

/// Generating follows the same rule: wording the owner has replaced doesn't come back by generating
/// from it, in this application or a new one.
#[tokio::test]
async fn generating_from_replaced_wording_is_a_draft() {
    let server = locked_server().await;
    let app = server.application("Acme").await;
    let args = generate_all(&app);
    server.call("generate_tailored_cv", args.clone()).await.ok();

    let old_bullet = "Built the routing service.";
    let new_bullet = "Built the routing engine.";
    owner_unlock(&server).await;
    let mut corrected = profile();
    corrected["experience"][0]["bullets"][1] = new_bullet.into();
    server.call("upsert_cv_profile", corrected).await.ok();
    owner_lock(&server).await;
    let second = server.call("generate_tailored_cv", args.clone()).await.ok();
    assert_eq!(second["message"], "CV generated successfully");
    let documents = document_count(&server).await;

    // The old wording again, while locked: wording is free, so this is allowed, and it is a draft.
    let mut back = profile();
    back["experience"][0]["bullets"][1] = old_bullet.into();
    server.call("upsert_cv_profile", back).await.ok();
    let again = server.call("generate_tailored_cv", args).await;
    assert_eq!(again.code(), "cv_approval_required");
    let other = server.application("Beta").await;
    let elsewhere = server
        .call("generate_tailored_cv", generate_all(&other))
        .await;
    assert_eq!(elsewhere.code(), "cv_approval_required");
    assert_eq!(document_count(&server).await, documents);
}

/// The lock speaks for the wording, not over the owner's own later approval in that variant.
#[tokio::test]
async fn a_lock_does_not_approve_over_the_owners_own_approval_in_a_variant() {
    let server = locked_server().await;
    let app = server.application("Acme").await;
    let variant = format!("application-{app}");
    let plain = generate_all(&app);
    server
        .call("generate_tailored_cv", plain.clone())
        .await
        .ok();

    let mut edited = plain.clone();
    edited["profileOverride"] = "Backend engineer, corrected by hand.".into();
    let held = server.call("generate_tailored_cv", edited.clone()).await;
    assert_eq!(held.code(), "cv_approval_required");
    owner_approve(&server, &variant).await;
    let approved = server.call("generate_tailored_cv", edited).await.ok();
    assert_eq!(approved["message"], "CV generated successfully");

    let documents = document_count(&server).await;
    let back = server.call("generate_tailored_cv", plain.clone()).await;
    assert_eq!(back.code(), "cv_approval_required");
    let mut reordered = plain;
    reordered["experienceIds"] = json!(["harborline", "brightpath"]);
    let other = server.call("generate_tailored_cv", reordered).await;
    assert_eq!(other.code(), "cv_approval_required");
    assert_eq!(document_count(&server).await, documents);
}

/// Selections of locked wording alternate freely: the lock approved them all.
#[tokio::test]
async fn a_lock_keeps_approving_its_own_selections_in_turn() {
    let server = locked_server().await;
    let app = server.application("Acme").await;
    let first = generate_all(&app);
    let mut second = first.clone();
    second["experienceIds"] = json!(["harborline", "brightpath"]);
    for args in [&first, &second, &first] {
        let reply = server.call("generate_tailored_cv", args.clone()).await;
        assert_eq!(reply.ok()["message"], "CV generated successfully");
    }
}

/// Finding 6: a save approves only where the diff is the owner's own.
#[tokio::test]
async fn a_save_approves_only_on_an_approved_base() {
    let server = locked_server().await;
    let (_, source) = head(&server, "default").await;
    let save = async |expected: i64, old: &str, new: &str| {
        owner::save(
            server.state.clone(),
            &json!({"variant": "default", "expectedRevision": expected, "summary": "By hand",
                "origin": "dashboard", "edits": [{"oldText": old, "newText": new}]}),
        )
        .await
        .structured_content
        .unwrap()
    };

    // The starting text isn't approved, whoever wrote the profile.
    let first = save(0, &source, &source).await;
    assert_eq!(
        (&first["state"], &first["revision"]),
        (&json!("draft"), &json!(1)),
        "{first}"
    );
    assert_eq!(approvals(&server).await, 0);

    // An agent's draft: one word by hand doesn't approve the rest.
    patch(
        &server,
        1,
        "Built the routing service.",
        "Holds a Doctorate from Stanford.",
    )
    .await
    .ok();
    let on_a_draft = save(2, "freight software", "freight systems").await;
    assert_eq!(on_a_draft["state"], "draft", "{on_a_draft}");
    assert_eq!(approvals(&server).await, 0);

    // Approved base: the change is the owner's own.
    owner_approve(&server, "default").await;
    let on_approved = save(3, "freight systems", "freight platforms").await;
    assert_eq!(on_approved["state"], "approved", "{on_approved}");
    let by: Vec<String> = sqlx::query_scalar("SELECT approved_by FROM cv_approvals ORDER BY id")
        .fetch_all(server.db.pool())
        .await
        .unwrap();
    assert_eq!(by, ["cli", "dashboard"]);
}

/// Finding 15.
#[tokio::test]
async fn a_variant_name_of_only_dots_is_refused() {
    let server = locked_server().await;
    for name in [".", "..", "...", "...."] {
        let reply = server
            .call(
                "patch_cv",
                json!({"variant": name, "createFrom": "default", "expectedRevision": 0,
                    "edits": [], "summary": "s"}),
            )
            .await;
        assert_eq!(reply.code(), "cv_variant_name_invalid", "{name}");
    }
    let dotted = server
        .call(
            "patch_cv",
            json!({"variant": "v1.2", "createFrom": "default", "expectedRevision": 0,
                "edits": [], "summary": "s"}),
        )
        .await;
    assert!(!dotted.is_error, "{}", dotted.json());
}

/// Finding 16: these say the owner acted, or that the tool did, so no caller can send them.
#[tokio::test]
async fn origins_that_claim_the_owner_are_refused_from_a_tool_call() {
    let server = locked_server().await;
    for origin in [
        "dashboard",
        "Dashboard",
        " dashboard ",
        "revert",
        "REVERT",
        "cli",
        "lock",
    ] {
        let reply = server
            .call(
                "patch_cv",
                json!({"expectedRevision": 0, "summary": "Edited by hand", "origin": origin,
                    "edits": [{"oldText": "Backend engineer", "newText": "Backend developer"}]}),
            )
            .await;
        assert_eq!(reply.code(), "cv_origin_reserved", "{origin}");
    }
    assert_eq!(head(&server, "default").await.0, 0, "nothing stored");
    let free = patch(&server, 0, "Backend engineer", "Backend developer").await;
    assert_eq!(free.ok()["state"], "draft");
}

async fn patch_in(server: &Server, variant: &str, expected: i64, old: &str, new: &str) -> Value {
    server
        .call(
            "patch_cv",
            json!({"variant": variant, "expectedRevision": expected, "summary": "An edit",
                "origin": "test", "edits": [{"oldText": old, "newText": new}]}),
        )
        .await
        .ok()
}

/// A new variant brought text back: what it is read against must not be that same replaced text.
#[tokio::test]
async fn a_new_variant_reads_against_no_text_the_owner_has_replaced() {
    let server = locked_server().await;
    let old = "Built the routing service.";
    let (twenty, twelve) = ("Led a team of 20 engineers.", "Led a team of 12 engineers.");
    patch(&server, 0, old, twenty).await.ok();
    owner_approve(&server, "default").await;
    patch(&server, 1, twenty, twelve).await.ok();
    owner_approve(&server, "default").await;

    let brought_back = server
        .call(
            "patch_cv",
            json!({"variant": "laundry", "createFrom": "default", "expectedRevision": 2,
                "summary": "New", "origin": "test",
                "edits": [{"oldText": twelve, "newText": twenty}]}),
        )
        .await
        .ok();
    assert_eq!(brought_back["state"], "draft");
    let review = owner::review(server.state.clone(), "laundry")
        .await
        .unwrap();
    assert_eq!(review.baseline, None, "the whole CV shows");
    assert_eq!(review.baseline_revision, None);

    // Text still standing where it was approved is a baseline, as long as it stands.
    let kept = server
        .call(
            "patch_cv",
            json!({"variant": "kept", "createFrom": "default", "expectedRevision": 2,
                "summary": "New", "origin": "test", "edits": []}),
        )
        .await
        .ok();
    assert_eq!(kept["state"], "approved", "{kept}");
    patch_in(&server, "kept", 1, twelve, twenty).await;
    let review = owner::review(server.state.clone(), "kept").await.unwrap();
    assert!(review.baseline.unwrap().contains(twelve));
    assert_eq!(review.baseline_revision, Some(1));

    patch(&server, 2, twelve, "Led a team of 15 engineers.")
        .await
        .ok();
    owner_approve(&server, "default").await;
    let review = owner::review(server.state.clone(), "kept").await.unwrap();
    assert_eq!(review.baseline, None, "replaced where it stood");
}

/// An approval under facts that came back doesn't outrank what the owner approved since.
#[tokio::test]
async fn a_fact_changed_and_changed_back_brings_back_nothing_the_owner_corrected() {
    let server = locked_server().await;
    let before = json!({"languages": [{"name": "German", "level": "C1"}]});
    let during = json!({"languages": [{"name": "German", "level": "B2"}]});
    let (superseded, corrected) = ("Holds a superseded claim.", "Holds the corrected claim.");
    patch(&server, 0, "Built the routing service.", superseded)
        .await
        .ok();
    owner_approve(&server, "default").await;
    owner_set(&server, during.clone()).await;
    patch(&server, 1, superseded, corrected).await.ok();
    owner_approve(&server, "default").await;
    owner_set(&server, before.clone()).await;

    assert_eq!(
        head_state(&server, "default").await,
        "draft",
        "an approval under other facts is no approval"
    );
    let reverted = revert(&server, 1, 2).await;
    assert_eq!(reverted["state"], "draft", "{reverted}");
    let rendered = server
        .call("render_cv_pdf", json!({"previewPages": 0}))
        .await
        .ok();
    assert_eq!(rendered["state"], "draft", "{rendered}");
    let review = owner::review(server.state.clone(), "default")
        .await
        .unwrap();
    assert_eq!(review.baseline, None, "nothing that stands to read against");

    let copied = server
        .call(
            "patch_cv",
            json!({"variant": "copy", "createFrom": "default", "expectedRevision": 3,
                "summary": "New", "origin": "test", "edits": []}),
        )
        .await
        .ok();
    assert_eq!(copied["state"], "draft", "{copied}");

    // The owner can stand by it again under these facts, and that then is what counts.
    owner_approve(&server, "default").await;
    assert_eq!(head_state(&server, "default").await, "approved");
    let undone = revert(&server, 2, 3).await;
    assert_eq!(undone["state"], "draft", "{undone}");
    let review = owner::review(server.state.clone(), "default")
        .await
        .unwrap();
    assert!(review.baseline.unwrap().contains(superseded));

    // With nothing approved in between, a fact changed and changed back took nothing away.
    owner_approve(&server, "default").await;
    owner_set(&server, during).await;
    assert_eq!(head_state(&server, "default").await, "draft");
    owner_set(&server, before).await;
    assert_eq!(head_state(&server, "default").await, "approved");
}

/// The lock path of the same case: wording the lock approved doesn't come back over the owner's own
/// later approval, under whichever facts that was made.
#[tokio::test]
async fn a_lock_does_not_speak_over_what_the_owner_approved_under_other_facts() {
    let server = locked_server().await;
    let before = json!({"languages": [{"name": "German", "level": "C1"}]});
    let during = json!({"languages": [{"name": "German", "level": "B2"}]});
    let app = server.application("Acme").await;
    let plain = generate_all(&app);
    let first = server.call("generate_tailored_cv", plain.clone()).await;
    assert_eq!(first.ok()["message"], "CV generated successfully");

    // A lock's own approval under old facts doesn't stand in the way of the lock under new ones.
    owner_set(&server, during.clone()).await;
    let relocked = server.call("generate_tailored_cv", plain.clone()).await;
    assert_eq!(relocked.ok()["message"], "CV generated successfully");

    let mut edited = plain.clone();
    edited["profileOverride"] = "Backend engineer, corrected by hand.".into();
    let held = server.call("generate_tailored_cv", edited).await;
    assert_eq!(held.code(), "cv_approval_required");
    owner_approve(&server, &format!("application-{app}")).await;

    owner_set(&server, before).await;
    let documents = document_count(&server).await;
    let back = server.call("generate_tailored_cv", plain.clone()).await;
    assert_eq!(back.code(), "cv_approval_required");
    assert_eq!(document_count(&server).await, documents);
}

// The owner's decision: a correction counts everywhere.

const CLAIM_20: (&str, &str) = ("Built the routing service.", "Led a team of 20 engineers.");

async fn new_variant(server: &Server, name: &str, from: &str) -> Value {
    let (at, _) = head(server, from).await;
    server
        .call(
            "patch_cv",
            json!({"variant": name, "createFrom": from, "expectedRevision": at,
                "summary": "New", "edits": []}),
        )
        .await
        .ok()
}

/// `beta` holds the owner's claim of 20, and `alpha` takes it with no approval of its own. The owner then
/// corrects it to 12 in `alpha`. The fourth gate review's `d2.py`.
async fn corrected_in_one_variant(server: &Server) {
    patch(
        server,
        0,
        "Built the routing service.",
        "Built the routing engine.",
    )
    .await
    .ok();
    owner_approve(server, "default").await;
    new_variant(server, "beta", "default").await;
    let (at, _) = head(server, "beta").await;
    server
        .call(
            "patch_cv",
            json!({"variant": "beta", "expectedRevision": at, "summary": "Claim",
                "edits": [{"oldText": "Built the routing engine.", "newText": CLAIM_20.1}]}),
        )
        .await
        .ok();
    owner_approve(server, "beta").await;
    new_variant(server, "alpha", "beta").await;
    assert_eq!(
        head_state(server, "alpha").await,
        "approved",
        "taken at once"
    );
    let (at, _) = head(server, "alpha").await;
    server
        .call(
            "patch_cv",
            json!({"variant": "alpha", "expectedRevision": at, "summary": "Correct",
                "edits": [{"oldText": "a team of 20", "newText": "a team of 12"}]}),
        )
        .await
        .ok();
    owner_approve(server, "alpha").await;
}

#[tokio::test]
async fn a_correction_in_one_variant_stops_a_new_variant_taking_the_old_claim() {
    let server = locked_server().await;
    corrected_in_one_variant(&server).await;
    // `beta`'s own approval is untouched.
    assert_eq!(head_state(&server, "beta").await, "approved");
    // A new variant from it holds the corrected claim, and has to be read.
    let gamma = new_variant(&server, "gamma", "beta").await;
    assert_eq!(gamma["state"], "draft", "{gamma}");
    assert_eq!(head_state(&server, "gamma").await, "draft");
    let (_, source) = head(&server, "gamma").await;
    assert!(source.contains("a team of 20"), "{source}");
    let rendered = server
        .call(
            "render_cv_pdf",
            json!({"variant": "gamma", "previewPages": 0}),
        )
        .await
        .ok();
    assert_eq!(rendered["state"], "draft", "stamped: {rendered}");
    // It reads as all new, so the claim isn't hidden as unchanged.
    let review = owner::review(server.state.clone(), "gamma").await.unwrap();
    assert_eq!(review.baseline, None, "{review:?}");
    assert_eq!(review.baseline_revision, None);
    // And a tap makes it stand there, as any draft.
    owner_approve(&server, "gamma").await;
    assert_eq!(head_state(&server, "gamma").await, "approved");
}

/// The owner approving the old text again takes it back: it stands, and a new variant may take it.
#[tokio::test]
async fn approving_the_old_text_again_takes_the_correction_back() {
    let server = locked_server().await;
    corrected_in_one_variant(&server).await;
    let (at, _) = head(&server, "alpha").await;
    server
        .call(
            "patch_cv",
            json!({"variant": "alpha", "expectedRevision": at, "summary": "Back",
                "edits": [{"oldText": "a team of 12", "newText": "a team of 20"}]}),
        )
        .await
        .ok();
    owner_approve(&server, "alpha").await;
    let again = new_variant(&server, "again", "beta").await;
    assert_eq!(again["state"], "approved", "{again}");
}

/// The honest order: a text nobody has replaced is taken at once, and reads against itself.
#[tokio::test]
async fn a_text_nobody_replaced_is_taken_at_once() {
    let server = locked_server().await;
    patch(&server, 0, CLAIM_20.0, "Built the routing engine.")
        .await
        .ok();
    owner_approve(&server, "default").await;
    for name in ["one", "two"] {
        let made = new_variant(&server, name, "default").await;
        assert_eq!(made["state"], "approved", "{made}");
        let rendered = server
            .call("render_cv_pdf", json!({"variant": name, "previewPages": 0}))
            .await
            .ok();
        assert_eq!(rendered["state"], "approved", "{rendered}");
    }
    let (_, source) = head(&server, "two").await;
    let (at, _) = head(&server, "two").await;
    let edited = server
        .call(
            "patch_cv",
            json!({"variant": "two", "expectedRevision": at, "summary": "Edit",
                "edits": [{"oldText": "routing engine", "newText": "routing core"}]}),
        )
        .await
        .ok();
    assert_eq!(edited["state"], "draft", "{source}");
    let review = owner::review(server.state.clone(), "two").await.unwrap();
    assert!(review.baseline.is_some(), "reads against what it took");
    assert!(!review.baseline_is_own);
}

/// Facts that change and change back don't bring a replaced text back either, in any variant.
#[tokio::test]
async fn a_correction_counts_after_the_facts_change_and_change_back() {
    let server = locked_server().await;
    corrected_in_one_variant(&server).await;
    let german = |level: &str| json!({"languages": [{"name": "German", "level": level}], "bannedPhrases": ["team player"]});
    for level in ["C2", "C1"] {
        let mut tx = server.db.begin_write().await.unwrap();
        cv_facts::unlock(&mut tx, "test").await.unwrap();
        tx.commit().await.unwrap();
        owner_set(&server, german(level)).await;
        owner_lock(&server).await;
    }
    assert_eq!(
        head_state(&server, "beta").await,
        "approved",
        "its own stands"
    );
    let gamma = new_variant(&server, "gamma", "beta").await;
    assert_eq!(gamma["state"], "draft", "{gamma}");
}

/// A lock's own approval isn't the owner's tap, and the page says which it is.
#[tokio::test]
async fn a_review_says_when_its_baseline_is_a_locks() {
    let server = locked_server().await;
    let app = server.application("Acme").await;
    let args =
        json!({"applicationId": app, "experienceIds": ["brightpath"], "skillCategories": []});
    let generated = server.call("generate_tailored_cv", args).await;
    let name: Option<String> = sqlx::query_scalar(
        "SELECT v.name FROM cv_approvals a JOIN cv_variants v ON v.id = a.variant_id
         WHERE a.approved_by = 'lock' ORDER BY a.id DESC LIMIT 1",
    )
    .fetch_optional(server.db.pool())
    .await
    .unwrap();
    let Some(name) = name else {
        panic!("generate wrote no lock approval: {generated:?}");
    };
    let (at, source) = head(&server, &name).await;
    let line = source
        .lines()
        .find(|line| line.trim_start().starts_with("- "))
        .unwrap()
        .to_owned();
    server
        .call(
            "patch_cv",
            json!({"variant": name, "expectedRevision": at, "summary": "Edit",
                "edits": [{"oldText": line, "newText": "  - Built an entirely different engine."}]}),
        )
        .await
        .ok();
    let review = owner::review(server.state.clone(), &name).await.unwrap();
    assert!(
        review.baseline_is_own && review.baseline_is_lock,
        "{review:?}"
    );
    assert!(
        !review.source_diff.contains("nothing approved yet"),
        "{}",
        review.source_diff
    );
}
