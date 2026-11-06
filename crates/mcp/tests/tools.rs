//! Each tool over HTTP with synthetic data, covering the runtime rules in SPEC section 4.

mod common;

use common::*;
use serde_json::{Value, json};

#[tokio::test]
async fn create_and_get_application() {
    let server = Server::start().await;
    let created = server
        .call(
            "create_application",
            json!({
                "company": "Acme Robotics",
                "role": "Engineer",
                "jobUrl": "https://Jobs.Example.com/a/?utm_source=x&b=1#frag",
                "salaryMin": 50000.5,
                "salaryCurrency": "eur",
                "eligibleCountries": ["de", "DE", "fr"],
                "primaryLocations": [" Berlin ", "Berlin"],
                "jobContentHash": "A".repeat(64),
            }),
        )
        .await
        .ok();
    assert_eq!(created["status"], "inbound");
    assert_eq!(created["remote"], false, "omitted remote reads back false");
    assert_eq!(created["appliedAt"], Value::Null);
    assert_eq!(created["userId"], OWNER_ID);
    assert_eq!(created["isDemo"], false);
    assert_eq!(created["demoKey"], Value::Null);
    assert_eq!(created["demoWorkspaceId"], Value::Null);
    assert_eq!(created["contacts"], json!([]));
    assert_eq!(
        created["salaryMin"], 50000,
        "Prisma truncates fractions on Int columns"
    );
    assert_eq!(created["salaryCurrency"], "EUR");
    assert_eq!(created["canonicalJobUrl"], "https://jobs.example.com/a?b=1");
    assert_eq!(created["eligibleCountries"], json!(["DE", "DE", "FR"]));
    assert_eq!(created["primaryLocations"], json!(["Berlin"]));
    assert_eq!(created["jobContentHash"], "a".repeat(64));
    assert_eq!(created["eventVersion"], 0);

    let id = created["id"].as_str().unwrap();
    let got = server.call("get_application", json!({"id": id})).await;
    assert_eq!(
        got.structured.as_ref(),
        Some(&got.json()),
        "structuredContent mirrors the text"
    );
    assert_eq!(got.ok()["company"], "Acme Robotics");
    for missing in ["999999", "abc", "0", "007", "99999999999"] {
        assert_eq!(
            server
                .call("get_application", json!({"id": missing}))
                .await
                .err_text(),
            "Application not found"
        );
    }
}

#[tokio::test]
async fn create_defaults_and_rejects_like_old_nexus() {
    let server = Server::start().await;
    let interview = server
        .call(
            "create_application",
            json!({"company": "A", "role": "R", "status": "interview"}),
        )
        .await
        .ok();
    assert!(
        interview["appliedAt"].is_string(),
        "applied-or-later statuses default appliedAt"
    );
    let explicit = server
        .call(
            "create_application",
            json!({"company": "A", "role": "R", "appliedAt": "2026-01-02"}),
        )
        .await
        .ok();
    assert_eq!(explicit["appliedAt"], "2026-01-02T00:00:00.000Z");
    let long = server
        .call(
            "create_application",
            json!({"company": "x".repeat(300), "role": "R", "source": "s".repeat(150)}),
        )
        .await
        .ok();
    assert_eq!(long["company"].as_str().unwrap().len(), 255);
    assert_eq!(
        long["source"].as_str().unwrap().len(),
        100,
        "create truncates source silently"
    );

    for (args, text) in [
        (json!({"appliedAt": "soon"}), "appliedAt_invalid"),
        (
            json!({"jobContentHash": "nothex"}),
            "jobContentHash_invalid",
        ),
        (json!({"jobUrl": "not a url"}), "Invalid URL"),
        (
            json!({"jobUrl": "ftp://example.com/x"}),
            "Job URL must use http or https",
        ),
        (
            json!({"jobUrl": "https://u:p@example.com/x"}),
            "Job URL must not contain credentials",
        ),
    ] {
        let mut full = json!({"company": "A", "role": "R"});
        full.as_object_mut()
            .unwrap()
            .extend(args.as_object().unwrap().clone());
        assert_eq!(
            server.call("create_application", full).await.err_text(),
            text
        );
    }
    server
        .call(
            "create_application",
            json!({"company": "A", "role": "R", "jobUrl": "https://example.com/dup"}),
        )
        .await
        .ok();
    let duplicate = server
        .call(
            "create_application",
            json!({"company": "B", "role": "R", "jobUrl": "https://example.com/dup/"}),
        )
        .await;
    assert_eq!(duplicate.err_text(), "canonical_job_url_conflict");
}

#[tokio::test]
async fn validation_errors_carry_zod_issues() {
    let server = Server::start().await;
    let reply = server.call("get_application", json!({})).await;
    assert_eq!(
        reply.err_text(),
        "MCP error -32602: Input validation error: Invalid arguments for tool get_application: [\n  {\n    \"code\": \"invalid_type\",\n    \"expected\": \"string\",\n    \"received\": \"undefined\",\n    \"path\": [\n      \"id\"\n    ],\n    \"message\": \"Required\"\n  }\n]"
    );
    assert_eq!(
        server.call("no_such_tool", json!({})).await.err_text(),
        "MCP error -32602: Tool no_such_tool not found"
    );
    // Unknown keys are stripped, not rejected, though the schema says additionalProperties: false.
    let id = server.application("Acme").await;
    server
        .call("get_application", json!({"id": id, "surprise": true}))
        .await
        .ok();
    // Offsets are accepted only where the schema was declared with them.
    let offset = "2026-01-02T03:04:05+02:00";
    assert!(
        server
            .call(
                "update_application",
                json!({"id": id, "expectedUpdatedAt": offset})
            )
            .await
            .err_text()
            .contains("Invalid datetime")
    );
    let archived = server
        .call(
            "archive_application",
            json!({"id": id, "archivedAt": offset}),
        )
        .await
        .ok();
    assert_eq!(
        archived["application"]["archivedAt"],
        "2026-01-02T01:04:05.000Z"
    );
    // Document source has a schema limit; application upsert too, but not create.
    let long_source = "s".repeat(101);
    assert!(server.call("upsert_application_by_job_url", json!({"company": "A", "role": "R", "jobUrl": "https://example.com/v", "source": long_source})).await.err_text().contains("at most 100"));
}

#[tokio::test]
async fn update_application_rules() {
    let server = Server::start().await;
    let id = server.application("Acme").await;

    // Mixed metadata and lifecycle fails atomically.
    let mixed = server
        .call(
            "update_application",
            json!({"id": id, "notes": "x", "status": "applied", "currentStage": null}),
        )
        .await;
    assert_eq!(
        mixed.json(),
        json!({"error": {"code": "lifecycle_event_required", "fields": ["status", "currentStage"]}})
    );
    assert!(mixed.is_error);
    let unchanged = server.call("get_application", json!({"id": id})).await.ok();
    assert_eq!(unchanged["notes"], Value::Null);
    assert_eq!(unchanged["eventVersion"], 0);

    // dryRun previews without writing, showing the raw value.
    let preview = server
        .call(
            "update_application",
            json!({"id": id, "notes": "n", "rating": 2.5, "dryRun": true}),
        )
        .await
        .ok();
    assert_eq!(preview["dryRun"], true);
    assert_eq!(preview["application"]["notes"], "n");
    assert_eq!(preview["application"]["rating"], 2.5);
    assert_eq!(
        server.call("get_application", json!({"id": id})).await.ok()["notes"],
        Value::Null
    );

    let updated = server
        .call(
            "update_application",
            json!({"id": id, "notes": "n", "rating": 2.5}),
        )
        .await
        .ok();
    assert_eq!(updated["rating"], 2, "rating truncates on write");
    assert_eq!(updated["eventVersion"], 1);

    // Optimistic concurrency, on writes and on dry runs.
    let stale = "2020-01-01T00:00:00.000Z";
    for dry_run in [true, false] {
        let conflict = server
            .call(
                "update_application",
                json!({"id": id, "notes": "z", "expectedUpdatedAt": stale, "dryRun": dry_run}),
            )
            .await;
        assert_eq!(conflict.code(), "conflict");
    }
    let fresh = updated["updatedAt"].as_str().unwrap();
    server
        .call(
            "update_application",
            json!({"id": id, "notes": "z", "expectedUpdatedAt": fresh}),
        )
        .await
        .ok();

    assert_eq!(
        server
            .call("update_application", json!({"id": "424242", "notes": "z"}))
            .await
            .code(),
        "not_found"
    );
    assert_eq!(
        server
            .call(
                "update_application",
                json!({"id": id, "jobUrl": "ftp://x.example"})
            )
            .await
            .code(),
        "application_update_failed"
    );
    assert_eq!(
        server
            .call(
                "update_application",
                json!({"id": id, "jobContentHash": "bad"})
            )
            .await
            .code(),
        "application_update_failed"
    );
    assert_eq!(
        server
            .call(
                "update_application",
                json!({"id": id, "salaryMin": 3_000_000_000_i64})
            )
            .await
            .code(),
        "application_update_failed"
    );
}

#[tokio::test]
async fn archive_and_unarchive_are_idempotent() {
    let server = Server::start().await;
    let id = server.application("Acme").await;
    let at = "2026-03-01T10:00:00.000Z";

    let preview = server
        .call(
            "archive_application",
            json!({"id": id, "archivedAt": at, "dryRun": true}),
        )
        .await
        .ok();
    assert_eq!(
        (&preview["dryRun"], &preview["changed"]),
        (&json!(true), &json!(true))
    );
    assert_eq!(preview["application"]["archivedAt"], at);
    assert_eq!(
        server.call("get_application", json!({"id": id})).await.ok()["archivedAt"],
        Value::Null
    );

    let first = server
        .call("archive_application", json!({"id": id, "archivedAt": at}))
        .await
        .ok();
    assert_eq!(first["changed"], true);
    let again = server
        .call("archive_application", json!({"id": id}))
        .await
        .ok();
    assert_eq!(again["changed"], false);
    assert_eq!(
        again["application"]["archivedAt"], at,
        "the first timestamp is preserved"
    );

    let stale = "2020-01-01T00:00:00.000Z";
    assert_eq!(
        server
            .call(
                "archive_application",
                json!({"id": id, "expectedUpdatedAt": stale})
            )
            .await
            .code(),
        "conflict"
    );
    let exact_retry = server
        .call(
            "archive_application",
            json!({"id": id, "archivedAt": at, "expectedUpdatedAt": stale}),
        )
        .await
        .ok();
    assert_eq!(exact_retry["changed"], false);

    let restored = server
        .call("unarchive_application", json!({"id": id}))
        .await
        .ok();
    assert_eq!(
        (&restored["changed"], &restored["application"]["archivedAt"]),
        (&json!(true), &Value::Null)
    );
    assert_eq!(
        server
            .call("unarchive_application", json!({"id": id}))
            .await
            .ok()["changed"],
        false
    );
    assert_eq!(
        server
            .call("archive_application", json!({"id": "9999"}))
            .await
            .code(),
        "not_found"
    );
}

#[tokio::test]
async fn list_applications_and_filters() {
    let server = Server::start().await;
    let a = server.application("München GmbH").await;
    let b = server.application("Beta 50% Ltd").await;
    let c = server.application("Gamma").await;
    server
        .call(
            "update_application",
            json!({"id": a, "rating": 5, "remote": true}),
        )
        .await
        .ok();

    let page = server
        .call("list_applications", json!({"limit": 2}))
        .await
        .ok();
    assert_eq!(page.as_array().unwrap().len(), 2);
    assert_eq!(page[0]["id"], c.as_str(), "newest first");
    let keys: Vec<_> = page[0].as_object().unwrap().keys().cloned().collect();
    for key in [
        "id",
        "company",
        "role",
        "status",
        "currentStage",
        "nextAction",
        "rating",
        "updatedAt",
    ] {
        assert!(keys.iter().any(|k| k == key), "{key}");
    }
    assert_eq!(keys.len(), 8, "the compact index has no heavy fields");
    let next = server
        .call("list_applications", json!({"limit": 2, "cursor": b}))
        .await
        .ok();
    assert_eq!(next[0]["id"], a.as_str());
    assert_eq!(
        server
            .call("list_applications", json!({"cursor": "abc"}))
            .await
            .code(),
        "application_cursor_invalid"
    );
    assert_eq!(
        server
            .call("list_applications", json!({"cursor": "424242"}))
            .await
            .code(),
        "application_cursor_invalid"
    );

    let found = server
        .call("list_applications", json!({"q": "MÜNCHEN"}))
        .await
        .ok();
    assert_eq!(
        found.as_array().unwrap().len(),
        1,
        "search folds non-ASCII case, as ILIKE does"
    );
    let wildcard = server
        .call("list_applications", json!({"q": "50%"}))
        .await
        .ok();
    assert_eq!(wildcard[0]["id"], b.as_str());
    assert_eq!(
        server
            .call("list_applications", json!({"q": "MÜNCHEN", "cursor": b}))
            .await
            .code(),
        "application_cursor_invalid"
    );
    let picked = server
        .call(
            "list_applications",
            json!({"fields": ["company", "nope"], "limit": 1}),
        )
        .await
        .ok();
    assert_eq!(
        picked[0].as_object().unwrap().len(),
        2,
        "id always, unknown fields dropped"
    );

    let filtered = server
        .call(
            "list_applications_filtered",
            json!({"rating_gte": 4.5, "fields": ["rating"]}),
        )
        .await
        .ok();
    assert_eq!(filtered.as_array().unwrap().len(), 1);
    let all = server
        .call("list_applications_filtered", json!({}))
        .await
        .ok();
    assert!(
        all[0].get("jobDescription").is_some(),
        "defaults to all fields"
    );
    assert_eq!(all[0]["contacts"], json!([]), "no contacts unless asked");
    let sorted = server
        .call(
            "list_applications_filtered",
            json!({"sort": "-rating", "limit": 1.5}),
        )
        .await
        .ok();
    assert_eq!(
        sorted.as_array().unwrap().len(),
        1,
        "a fractional limit truncates"
    );
    server
        .call("create_contact", json!({"applicationId": a, "name": "Ann"}))
        .await
        .ok();
    let with_contacts = server
        .call(
            "list_applications_filtered",
            json!({"remote": true, "include_contacts": true, "status": ["inbound"]}),
        )
        .await
        .ok();
    assert_eq!(with_contacts[0]["contacts"][0]["name"], "Ann");
}

#[tokio::test]
async fn delete_application_cascades() {
    let server = Server::start().await;
    let id = server.application("Acme").await;
    server
        .call(
            "create_contact",
            json!({"applicationId": id, "name": "Ann"}),
        )
        .await
        .ok();
    let linked = server.pdf(json!([id])).await;
    let claimed = server.pdf(json!([id])).await;
    server
        .call("record_application_submission", json!({
            "applicationId": id, "submittedAt": "2026-01-02T03:04:05Z", "idempotencyKey": "submit-key-1",
            "answers": one_answer(), "policy": good_policy(), "documentIds": [claimed["id"]],
        }))
        .await
        .ok();
    assert_eq!(
        server
            .call("delete_application", json!({"id": id}))
            .await
            .text,
        "Application deleted"
    );
    assert_eq!(
        server
            .call("get_application", json!({"id": id}))
            .await
            .err_text(),
        "Application not found"
    );
    let activity = server
        .call("list_application_activity", json!({}))
        .await
        .ok();
    assert_eq!(activity["items"], json!([]), "events cascade");
    let survivor = server
        .call("get_document", json!({"id": linked["id"]}))
        .await
        .ok();
    assert_eq!(
        survivor["applications"],
        json!([]),
        "linked documents survive, unlinked"
    );
    let historical = server
        .call("get_document", json!({"id": claimed["id"]}))
        .await
        .ok();
    assert_eq!(
        (&historical["state"], &historical["submissionId"]),
        (&json!("historical"), &Value::Null)
    );
    assert_eq!(
        server
            .call("delete_application", json!({"id": id}))
            .await
            .err_text(),
        "Application not found or access denied"
    );
}

#[tokio::test]
async fn find_and_upsert_by_job_url() {
    let server = Server::start().await;
    let url = "https://Example.com/job/1/?utm_campaign=x";
    let canonical = "https://example.com/job/1";
    let args = json!({"company": "Acme", "role": "Eng", "jobUrl": url});

    let miss = server
        .call("find_application_by_job_url", json!({"jobUrl": url}))
        .await
        .ok();
    assert_eq!(
        miss,
        json!({"canonicalJobUrl": canonical, "application": null})
    );
    let dry = server
        .call(
            "upsert_application_by_job_url",
            json!({"company": "Acme", "role": "Eng", "jobUrl": url, "dryRun": true}),
        )
        .await
        .ok();
    assert_eq!(
        dry,
        json!({"dryRun": true, "operation": "create", "canonicalJobUrl": canonical, "existingId": null})
    );
    assert!(
        server
            .call("find_application_by_job_url", json!({"jobUrl": url}))
            .await
            .ok()["application"]
            .is_null()
    );

    let created = server
        .call("upsert_application_by_job_url", args.clone())
        .await
        .ok();
    assert_eq!(created["operation"], "created");
    let id = created["application"]["id"].as_str().unwrap();
    let hit = server
        .call("find_application_by_job_url", json!({"jobUrl": canonical}))
        .await
        .ok();
    assert_eq!(hit["application"]["id"], id);

    let updated = server
        .call(
            "upsert_application_by_job_url",
            json!({"company": "Acme Inc", "role": "Eng", "jobUrl": url, "notes": "n"}),
        )
        .await
        .ok();
    assert_eq!(updated["operation"], "updated");
    assert_eq!(updated["application"]["company"], "Acme Inc");
    let dry_update = server
        .call(
            "upsert_application_by_job_url",
            json!({"company": "Acme", "role": "Eng", "jobUrl": url, "dryRun": true}),
        )
        .await
        .ok();
    assert_eq!(
        (&dry_update["operation"], &dry_update["existingId"]),
        (&json!("update"), &json!(id))
    );

    let lifecycle = server
        .call(
            "upsert_application_by_job_url",
            json!({"company": "Acme", "role": "Eng", "jobUrl": url, "status": "offer"}),
        )
        .await;
    assert_eq!(lifecycle.code(), "lifecycle_event_required");
    let same_status = server
        .call(
            "upsert_application_by_job_url",
            json!({"company": "Acme", "role": "Eng", "jobUrl": url, "status": "inbound"}),
        )
        .await;
    assert!(!same_status.is_error);
    let stage = server
        .call(
            "upsert_application_by_job_url",
            json!({"company": "Acme", "role": "Eng", "jobUrl": url, "currentStage": "screen"}),
        )
        .await;
    assert_eq!(stage.code(), "lifecycle_event_required");
    let range = server.call("upsert_application_by_job_url", json!({"company": "Acme", "role": "Eng", "jobUrl": url, "salaryMin": 5, "salaryMax": 1})).await;
    assert_eq!(range.code(), "salary_range_invalid");
    let scheme = server
        .call(
            "upsert_application_by_job_url",
            json!({"company": "A", "role": "E", "jobUrl": "ftp://example.com/x"}),
        )
        .await;
    assert_eq!(scheme.code(), "Job URL must use http or https");
    let hash = server.call("upsert_application_by_job_url", json!({"company": "A", "role": "E", "jobUrl": "https://example.com/h", "jobContentHash": "bad"})).await;
    assert_eq!(hash.code(), "jobContentHash_invalid");
}

#[tokio::test]
async fn batch_upsert_and_delete_report_partial_failure() {
    let server = Server::start().await;
    let id = server.application("Acme").await;
    let result = server
        .call(
            "batch_upsert_applications",
            json!({"items": [
                {"company": "New", "role": "R", "notes": ""},
                {"id": id, "notes": "edited"},
                {"id": id, "status": "offer"},
                {"company": "No role"},
                {"company": "Bad date", "role": "R", "appliedAt": "soon"},
            ]}),
        )
        .await
        .ok();
    assert_eq!(
        (&result["total"], &result["succeeded"], &result["failed"]),
        (&json!(5), &json!(2), &json!(3))
    );
    let results = &result["results"];
    assert_eq!(results[0]["operation"], "created");
    assert_eq!(
        results[1],
        json!({"index": 1, "id": id, "operation": "updated"})
    );
    assert_eq!(results[2]["error"], "lifecycle_event_required");
    assert_eq!(
        results[3]["error"],
        "company and role are required for new applications"
    );
    assert_eq!(
        results[4],
        json!({"index": 4, "id": "", "operation": "created", "error": "appliedAt_invalid"})
    );
    let new_id = results[0]["id"].as_str().unwrap();
    assert_eq!(
        server
            .call("get_application", json!({"id": new_id}))
            .await
            .ok()["notes"],
        Value::Null
    );
    assert_eq!(
        server.call("get_application", json!({"id": id})).await.ok()["notes"],
        "edited"
    );

    assert_eq!(
        server
            .call(
                "batch_upsert_applications",
                json!({"items": [{"id": "424242"}]})
            )
            .await
            .err_text(),
        "Batch upsert failed"
    );
    assert_eq!(
        server
            .call(
                "batch_upsert_applications",
                json!({"items": [{"company": "A", "role": "R", "jobUrl": "nope"}]})
            )
            .await
            .err_text(),
        "Batch upsert failed"
    );

    let deleted = server
        .call(
            "batch_delete_applications",
            json!({"ids": [new_id, new_id]}),
        )
        .await
        .ok();
    assert_eq!(deleted["succeeded"], 1);
    assert_eq!(
        deleted["results"][1],
        json!({"id": new_id, "deleted": false, "error": "not_found"})
    );
    assert_eq!(
        server
            .call("batch_delete_applications", json!({"ids": ["424242"]}))
            .await
            .err_text(),
        "Batch delete failed"
    );
}

#[tokio::test]
async fn contacts_lifecycle() {
    let server = Server::start().await;
    let id = server.application("Acme").await;
    let contact = server
        .call("create_contact", json!({"applicationId": id, "name": "Ann", "email": "ann@example.test", "phone": "1".repeat(60), "linkedIn": "https://example.test/ann"}))
        .await
        .ok();
    assert_eq!(contact["applicationId"], id.as_str());
    assert_eq!(contact["phone"].as_str().unwrap().len(), 50);
    assert_eq!(contact["role"], Value::Null);
    let contact_id = contact["id"].as_str().unwrap();

    let updated = server.call("update_contact", json!({"contactId": contact_id, "applicationId": id, "email": null, "role": "Recruiter"})).await.ok();
    assert_eq!(
        (&updated["email"], &updated["role"], &updated["name"]),
        (&Value::Null, &json!("Recruiter"), &json!("Ann"))
    );
    assert_eq!(
        server.call("get_application", json!({"id": id})).await.ok()["contacts"][0]["role"],
        "Recruiter"
    );

    for (contact_ref, application) in [
        ("424242", id.as_str()),
        ("abc", id.as_str()),
        (contact_id, "424242"),
    ] {
        let args = json!({"contactId": contact_ref, "applicationId": application});
        assert_eq!(
            server.call("update_contact", args.clone()).await.err_text(),
            "Contact not found or access denied"
        );
        assert_eq!(
            server.call("delete_contact", args).await.err_text(),
            "Contact not found or access denied"
        );
    }
    assert_eq!(
        server
            .call(
                "delete_contact",
                json!({"contactId": contact_id, "applicationId": id})
            )
            .await
            .text,
        "Contact deleted"
    );

    let missing = json!({"applicationId": "424242", "name": "Ann"});
    assert_eq!(
        server.call("create_contact", missing).await.err_text(),
        "Application not found or access denied"
    );

    let batch = server.call("batch_create_contacts", json!({"applicationId": id, "contacts": [{"name": "A"}, {"name": "B", "role": "r".repeat(120)}]})).await.ok();
    assert_eq!(
        (&batch["total"], &batch["succeeded"], &batch["failed"]),
        (&json!(2), &json!(2), &json!(0))
    );
    assert_eq!(batch["results"][1]["operation"], "created");
    let missing = json!({"applicationId": "424242", "contacts": [{"name": "A"}]});
    assert_eq!(
        server
            .call("batch_create_contacts", missing)
            .await
            .err_text(),
        "Application not found or access denied"
    );
}

fn at(day: u32) -> String {
    format!("2026-02-{day:02}T10:00:00.000Z")
}

#[tokio::test]
async fn events_project_onto_the_application() {
    let server = Server::start().await;
    let id = server.application("Acme").await;
    let record = |kind: &str, metadata: Value| json!({"applicationId": id, "type": kind, "occurredAt": at(1), "metadata": metadata});

    let stage = server
        .call(
            "record_application_event",
            record(
                "stage_changed",
                json!({"toStage": "screen", "toStatus": "applied"}),
            ),
        )
        .await
        .ok();
    assert_eq!(stage["replayed"], false);
    assert_eq!(stage["application"]["currentStage"], "screen");
    assert_eq!(stage["application"]["status"], "applied");
    assert_eq!(stage["application"]["eventVersion"], 1);
    let metadata = &stage["event"]["metadata"];
    assert_eq!(
        (&metadata["fromStage"], &metadata["fromStatus"]),
        (&Value::Null, &json!("inbound"))
    );
    assert_eq!(stage["event"]["source"], "mcp");
    assert_eq!(stage["event"]["actor"], OWNER_EMAIL);
    assert_eq!(stage["event"]["userId"], OWNER_ID);
    assert_eq!(stage["event"]["isDemo"], false);

    let scheduled = server
        .call("record_application_event", record("interview_scheduled", json!({"interviewType": "tech", "scheduledAt": at(9), "nextAction": " prepare ", "durationMinutes": 45})))
        .await
        .ok();
    let application = &scheduled["application"];
    assert_eq!(application["status"], "interview");
    assert_eq!(application["currentStage"], "interview_scheduled");
    assert_eq!(application["followUpAt"], at(9));
    assert_eq!(application["lastContact"], at(1));
    assert_eq!(application["nextAction"], "prepare");
    assert_eq!(scheduled["event"]["metadata"]["fromStage"], "screen");

    let rejected = server
        .call(
            "record_application_event",
            record("application_rejected", json!({"reason": "no"})),
        )
        .await
        .ok();
    assert_eq!(rejected["application"]["status"], "rejected");
    assert_eq!(rejected["application"]["followUpAt"], Value::Null);

    let other = server.application("Beta").await;
    let outbound = server
        .call(
            "record_application_event",
            json!({"applicationId": other, "type": "outbound_contact_recorded"}),
        )
        .await
        .ok();
    assert_eq!(
        outbound["application"]["status"], "applied",
        "outbound contact moves inbound to applied"
    );
    let offer = server.call("record_application_event", json!({"applicationId": other, "type": "offer_received", "metadata": {"followUpAt": at(20)}})).await.ok();
    assert_eq!(
        (
            &offer["application"]["currentStage"],
            &offer["application"]["followUpAt"]
        ),
        (&json!("offer_received"), &json!(at(20)))
    );
}

#[tokio::test]
async fn event_validation_and_idempotency() {
    let server = Server::start().await;
    let id = server.application("Acme").await;
    let event = |extra: Value| {
        let mut args =
            json!({"applicationId": id, "type": "note_added", "metadata": {"note": "hi"}});
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        args
    };

    let no_time = server
        .call(
            "record_application_event",
            event(json!({"idempotencyKey": "event-key-1"})),
        )
        .await;
    assert_eq!(no_time.code(), "occurred_at_required_for_idempotency");
    let blank_key = server
        .call(
            "record_application_event",
            event(json!({"idempotencyKey": "        ", "occurredAt": at(1)})),
        )
        .await;
    assert_eq!(blank_key.code(), "idempotency_key_invalid");
    let submitted = server
        .call(
            "record_application_event",
            json!({"applicationId": id, "type": "application_submitted"}),
        )
        .await;
    assert_eq!(
        submitted.code(),
        "submission_event_requires_submission_workflow"
    );
    assert_eq!(
        server
            .call(
                "record_application_event",
                json!({"applicationId": "424242", "type": "note_added"})
            )
            .await
            .code(),
        "not_found"
    );
    assert_eq!(
        server
            .call(
                "record_application_event",
                event(json!({"expectedUpdatedAt": at(1)}))
            )
            .await
            .code(),
        "conflict"
    );

    let invalid = server.call("record_application_event", json!({"applicationId": id, "type": "recruiter_contacted", "metadata": {"outcome": "x".repeat(256)}})).await;
    assert_eq!(
        invalid.json(),
        json!({"error": {
            "code": "event_metadata_invalid",
            "path": "metadata.outcome",
            "expected": "string with 1 to 255 characters",
            "reason": "outcome must contain 1 to 255 characters after trimming",
        }})
    );
    let unknown = server.call("record_application_event", json!({"applicationId": id, "type": "note_added", "metadata": {"note": "n", "extra": 1}})).await;
    assert_eq!(
        unknown.json()["error"]["reason"],
        "extra is not accepted for note_added"
    );
    assert_eq!(unknown.json()["error"]["expected"], "one of: note");
    let required = server
        .call(
            "record_application_event",
            json!({"applicationId": id, "type": "stage_changed"}),
        )
        .await;
    assert_eq!(required.json()["error"]["path"], "metadata.toStage");
    let duration = server.call("record_application_event", json!({"applicationId": id, "type": "interview_invited", "metadata": {"durationMinutes": 1441}})).await;
    assert_eq!(
        duration.json()["error"]["expected"],
        "integer from 1 to 1440"
    );
    let date = server.call("record_application_event", json!({"applicationId": id, "type": "follow_up_scheduled", "metadata": {"followUpAt": "soon"}})).await;
    assert_eq!(
        date.json()["error"]["reason"],
        "followUpAt must be a valid ISO 8601 date-time string"
    );
    let contact = server.call("record_application_event", json!({"applicationId": id, "type": "recruiter_contacted", "metadata": {"contactId": "424242"}})).await;
    assert_eq!(contact.code(), "contact_not_found");
    let document = server.call("record_application_event", json!({"applicationId": id, "type": "document_attached", "metadata": {"documentId": "abc"}})).await;
    assert_eq!(document.code(), "document_not_found");
    let linked = server.pdf(json!([id])).await;
    let attached = server.call("record_application_event", json!({"applicationId": id, "type": "document_attached", "metadata": {"documentId": linked["id"]}})).await;
    assert!(!attached.is_error);

    // Replay returns the original, and a changed request conflicts.
    let first = server
        .call(
            "record_application_event",
            event(json!({"idempotencyKey": "event-key-1", "occurredAt": at(1)})),
        )
        .await
        .ok();
    let replay = server
        .call(
            "record_application_event",
            event(json!({"idempotencyKey": "event-key-1", "occurredAt": at(1)})),
        )
        .await
        .ok();
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["event"]["id"], first["event"]["id"]);
    assert_eq!(
        replay["application"]["eventVersion"],
        first["application"]["eventVersion"]
    );
    let changed = server
        .call(
            "record_application_event",
            event(json!({"idempotencyKey": "event-key-1", "occurredAt": at(2)})),
        )
        .await;
    assert_eq!(changed.code(), "idempotency_conflict");

    // Writing the same idempotent event concurrently still produces one event.
    let other = server.application("Beta").await;
    let args = json!({"applicationId": other, "type": "note_added", "metadata": {"note": "n"}, "idempotencyKey": "race-key-1", "occurredAt": at(3)});
    let (a, b, c) = tokio::join!(
        server.call("record_application_event", args.clone()),
        server.call("record_application_event", args.clone()),
        server.call("record_application_event", args.clone()),
    );
    let replays = [a.ok(), b.ok(), c.ok()]
        .iter()
        .filter(|r| r["replayed"] == true)
        .count();
    assert_eq!(replays, 2);
}

#[tokio::test]
async fn notes_and_timelines() {
    let server = Server::start().await;
    let id = server.application("Acme Corp").await;
    let args = json!({"applicationId": id, "note": "  spoke to Ann  ", "occurredAt": at(1), "idempotencyKey": "note-key-1"});
    let note = server
        .call("append_application_note", args.clone())
        .await
        .ok();
    assert_eq!(note["event"]["type"], "note_added");
    assert_eq!(
        note["event"]["metadata"]["note"], "spoke to Ann",
        "zod trims the note"
    );
    assert_eq!(
        note["application"]["notes"],
        Value::Null,
        "the compact summary is untouched"
    );
    assert_eq!(
        server.call("append_application_note", args).await.ok()["replayed"],
        true
    );
    assert!(server.call("append_application_note", json!({"applicationId": id, "note": "   ", "occurredAt": at(1), "idempotencyKey": "note-key-2"})).await.err_text().contains("too_small"));
    assert_eq!(server.call("append_application_note", json!({"applicationId": "424242", "note": "x", "occurredAt": at(1), "idempotencyKey": "note-key-2"})).await.code(), "not_found");

    for day in 2..=4 {
        server.call("record_application_event", json!({"applicationId": id, "type": "note_added", "occurredAt": at(day), "metadata": {"note": "n"}})).await.ok();
    }
    let page = server
        .call(
            "list_application_events",
            json!({"applicationId": id, "limit": 2}),
        )
        .await
        .ok();
    assert_eq!(page["items"][0]["occurredAt"], at(4));
    assert_eq!(page["items"][0]["application"]["company"], "Acme Corp");
    let cursor = page["nextCursor"].as_str().unwrap();
    let next = server
        .call(
            "list_application_events",
            json!({"applicationId": id, "limit": 2, "cursor": cursor}),
        )
        .await
        .ok();
    assert_eq!(next["items"][1]["occurredAt"], at(1));
    assert_eq!(next["nextCursor"], Value::Null);
    let oldest = server
        .call(
            "list_application_events",
            json!({"applicationId": id, "order": "oldest", "limit": 1}),
        )
        .await
        .ok();
    assert_eq!(oldest["items"][0]["occurredAt"], at(1));
    assert_eq!(
        server
            .call(
                "list_application_events",
                json!({"applicationId": id, "cursor": "!!!"})
            )
            .await
            .code(),
        "event_query_invalid"
    );
    assert_eq!(
        server
            .call(
                "list_application_events",
                json!({"applicationId": "424242"})
            )
            .await
            .code(),
        "not_found"
    );

    let activity = server.call("list_application_activity", json!({"company": "ACME", "types": ["note_added"], "occurredAfter": at(3), "actor": OWNER_EMAIL})).await.ok();
    assert_eq!(activity["items"].as_array().unwrap().len(), 2);
    assert_eq!(
        server
            .call("list_application_activity", json!({"company": "nobody"}))
            .await
            .ok()["items"],
        json!([])
    );
    assert_eq!(
        server
            .call("list_application_activity", json!({"types": []}))
            .await
            .code(),
        "event_query_invalid"
    );
    assert_eq!(
        server
            .call(
                "list_application_activity",
                json!({"occurredAfter": at(4), "occurredBefore": at(1)})
            )
            .await
            .code(),
        "event_query_invalid"
    );
    assert_eq!(
        server
            .call("list_application_activity", json!({"applicationId": "abc"}))
            .await
            .code(),
        "event_query_failed"
    );
    assert_eq!(
        server
            .call("list_application_activity", json!({"source": "   "}))
            .await
            .code(),
        "event_query_invalid"
    );
}

fn submission(id: &str, key: &str, documents: Value) -> Value {
    json!({
        "applicationId": id,
        "submittedAt": "2026-03-01T09:00:00.000Z",
        "idempotencyKey": key,
        "answers": one_answer(),
        "policy": good_policy(),
        "documentIds": documents,
    })
}

#[tokio::test]
async fn submissions_write_atomically_and_replay() {
    let server = Server::start().await;
    let id = server.application("Acme").await;
    let document = server.pdf(json!([id])).await;
    let args = submission(&id, "submit-key-1", json!([document["id"]]));

    let mut dry = args.clone();
    dry["dryRun"] = true.into();
    let preview = server.call("record_application_submission", dry).await.ok();
    assert_eq!(
        (&preview["dryRun"], &preview["event"]),
        (&json!(true), &Value::Null)
    );
    assert_eq!(preview["submission"]["id"], "dry-run");
    assert_eq!(preview["application"]["status"], "applied");
    assert_eq!(preview["documents"][0]["state"], "submitted");
    assert_eq!(
        server.call("get_application", json!({"id": id})).await.ok()["status"],
        "inbound",
        "a dry run writes nothing"
    );

    let stored = server
        .call("record_application_submission", args.clone())
        .await
        .ok();
    assert_eq!(
        (&stored["replayed"], &stored["verified"]),
        (&json!(false), &json!(true))
    );
    assert_eq!(stored["application"]["status"], "applied");
    assert_eq!(
        stored["application"]["appliedAt"],
        "2026-03-01T09:00:00.000Z"
    );
    assert_eq!(stored["event"]["type"], "application_submitted");
    assert_eq!(stored["event"]["idempotencyKey"], "submission:submit-key-1");
    assert_eq!(stored["event"]["metadata"]["answerCount"], 1);
    assert_eq!(stored["submission"]["policy"]["confirmedNoAnswers"], false);
    assert_eq!(stored["submission"]["userId"], OWNER_ID);
    assert_eq!(
        stored["documents"][0]["submissionId"],
        stored["submission"]["id"]
    );
    // Only the top-level documents were sanitized.
    assert!(stored["documents"][0].get("applicationIds").is_none());
    assert_eq!(
        stored["submission"]["documents"][0]["applicationIds"],
        json!([id])
    );
    assert_eq!(
        stored["submission"]["documents"][0]["demoProvenance"],
        false
    );

    let replay = server
        .call("record_application_submission", args.clone())
        .await
        .ok();
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["submission"]["id"], stored["submission"]["id"]);
    assert_eq!(replay["event"]["id"], stored["event"]["id"]);

    let mut changed = args.clone();
    changed["submittedAt"] = "2026-03-02T09:00:00.000Z".into();
    assert_eq!(
        server
            .call("record_application_submission", changed)
            .await
            .code(),
        "idempotency_conflict"
    );

    // Documents stay immutable once claimed.
    let claimed = json!({"id": document["id"]});
    assert_eq!(
        server
            .call("delete_document", claimed.clone())
            .await
            .err_text(),
        "Document not found or access denied"
    );
    assert_eq!(
        server
            .call(
                "update_document_links",
                json!({"id": document["id"], "applicationIds": []})
            )
            .await
            .err_text(),
        "Document not found or access denied"
    );
    assert_eq!(
        server
            .call(
                "update_document_metadata",
                json!({"id": document["id"], "source": "x"})
            )
            .await
            .err_text(),
        "Document not found or access denied"
    );
    let moved = server
        .call(
            "update_document_metadata",
            json!({"id": document["id"], "state": "historical"}),
        )
        .await
        .ok();
    assert_eq!(moved["state"], "historical");

    let listed = server
        .call("list_application_submissions", json!({"applicationId": id}))
        .await
        .ok();
    assert_eq!(listed[0]["answers"], json!([]), "summaries exclude answers");
    assert_eq!(
        server
            .call(
                "list_application_submissions",
                json!({"applicationId": "abc"})
            )
            .await
            .err_text(),
        "Application not found or access denied"
    );
    let submission_id = stored["submission"]["id"].as_str().unwrap();
    let full = server
        .call("get_application_submission", json!({"id": submission_id}))
        .await
        .ok();
    assert_eq!(full["answers"][0]["question"], "Why here?");
    assert!(
        full["documents"][0].get("applicationIds").is_none(),
        "this path sanitizes"
    );
    assert_eq!(
        server
            .call("get_application_submission", json!({"id": "424242"}))
            .await
            .err_text(),
        "Submission not found"
    );
    assert_eq!(
        server
            .call("get_application_submission", json!({"id": "abc"}))
            .await
            .err_text(),
        "Invalid numeric id: abc"
    );

    let recall = server
        .call("get_interview_recall_package", json!({"applicationId": id}))
        .await
        .ok();
    assert_eq!(recall["submissions"][0]["answers"][0]["answer"], "Because.");
    assert_eq!(recall["events"][0]["type"], "application_submitted");
    assert_eq!(recall["documents"].as_array().unwrap().len(), 1);
    assert_eq!(
        server
            .call(
                "get_interview_recall_package",
                json!({"applicationId": "abc"})
            )
            .await
            .err_text(),
        "Application not found"
    );
}

#[tokio::test]
async fn submission_policy_and_conflict_rules() {
    let server = Server::start().await;
    let id = server.application("Acme Corp").await;
    let doc = server.pdf(json!([])).await;
    let docs = json!([doc["id"]]);
    let with = |patch: Value| {
        let mut args = submission(&id, "submit-key-9", docs.clone());
        args.as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        args
    };
    let policy_with = |key: &str, value: Value| {
        let mut policy = good_policy();
        policy[key] = value;
        json!({"policy": policy})
    };

    for (patch, code) in [
        (json!({"policy": null}), "human_review_required"),
        (json!({"policy": "yes"}), "human_review_required"),
        (
            policy_with("humanReviewed", json!(false)),
            "human_review_required",
        ),
        (
            policy_with("identityConsistent", json!(false)),
            "identity_consistency_required",
        ),
        (
            policy_with("factsVerified", json!(false)),
            "fact_verification_required",
        ),
        (
            policy_with("profileConsistencyStatus", json!("maybe")),
            "profile_consistency_review_required",
        ),
        (json!({"documentIds": []}), "submission_materials_required"),
        (json!({"answers": []}), "submission_answers_required"),
        (
            policy_with("confirmedNoAnswers", json!(true)),
            "submission_answers_conflict",
        ),
        (
            policy_with("resubmissionReason", json!(5)),
            "submission_policy_reason_invalid",
        ),
        (
            policy_with("sameCompanyOverrideReason", json!("x".repeat(1001))),
            "submission_policy_reason_too_long",
        ),
        (
            json!({"documentIds": [doc["id"], doc["id"]]}),
            "submission_documents_invalid",
        ),
        (json!({"documentIds": ["424242"]}), "invalid_documents"),
        (json!({"documentIds": ["abc"]}), "submission_failed"),
        (
            json!({"candidateSalaryMin": 5, "candidateSalaryMax": 1}),
            "salary_range_invalid",
        ),
        (
            json!({"expectedUpdatedAt": "2020-01-01T00:00:00Z"}),
            "conflict",
        ),
        (json!({"applicationId": "424242"}), "not_found"),
        (
            json!({"answers": [{"question": "   ", "answer": "a"}]}),
            "submission_failed",
        ),
    ] {
        let reply = server
            .call("record_application_submission", with(patch.clone()))
            .await;
        assert_eq!(reply.code(), code, "{patch}");
    }
    let none = json!({"answers": [], "policy": {"humanReviewed": true, "identityConsistent": true, "factsVerified": true, "profileConsistencyStatus": "unavailable_reviewed", "confirmedNoAnswers": true}, "dryRun": true});
    assert!(
        !server
            .call("record_application_submission", with(none))
            .await
            .is_error
    );

    // A failed write applies nothing.
    assert_eq!(
        server.call("get_application", json!({"id": id})).await.ok()["status"],
        "inbound"
    );
    assert_eq!(
        server
            .call("get_document", json!({"id": doc["id"]}))
            .await
            .ok()["state"],
        "current"
    );

    // Lowercase currency normalizes; the same company blocks a second active submission.
    let stored = server.call("record_application_submission", with(json!({"candidateSalaryCurrency": "eur", "requisitionId": "REQ-1", "atsName": "Lever"}))).await.ok();
    assert_eq!(stored["submission"]["candidateSalaryCurrency"], "EUR");
    assert_eq!(stored["application"]["requisitionId"], "REQ-1");

    let repeat_doc = server.pdf(json!([])).await;
    let mut again = submission(&id, "submit-key-a", json!([repeat_doc["id"]]));
    assert_eq!(
        server
            .call("record_application_submission", again.clone())
            .await
            .code(),
        "application_already_submitted"
    );
    again["policy"]["resubmissionReason"] = "new role".into();
    assert!(
        !server
            .call("record_application_submission", again)
            .await
            .is_error
    );

    let twin = server.application("  ACME   corp ").await;
    let twin_doc = server.pdf(json!([])).await;
    let mut other = submission(&twin, "submit-key-b", json!([twin_doc["id"]]));
    other["requisitionId"] = "req-1".into();
    assert_eq!(
        server
            .call("record_application_submission", other.clone())
            .await
            .code(),
        "duplicate_requisition"
    );
    other.as_object_mut().unwrap().remove("requisitionId");
    assert_eq!(
        server
            .call("record_application_submission", other.clone())
            .await
            .code(),
        "same_company_active_application"
    );
    other["policy"]["sameCompanyOverrideReason"] = "different team".into();
    let stored = server
        .call("record_application_submission", other)
        .await
        .ok();
    assert_eq!(
        stored["submission"]["policy"]["sameCompanyOverrideReason"],
        "different team"
    );

    // A document can only be claimed once.
    let third = server.application("Gamma").await;
    let used = submission(&third, "submit-key-c", json!([twin_doc["id"]]));
    assert_eq!(
        server
            .call("record_application_submission", used)
            .await
            .code(),
        "document_already_submitted"
    );

    // Concurrent replays of one submission store one package.
    let fourth = server.application("Delta").await;
    let fresh = server.pdf(json!([])).await;
    let args = submission(&fourth, "race-submit-1", json!([fresh["id"]]));
    let (a, b) = tokio::join!(
        server.call("record_application_submission", args.clone()),
        server.call("record_application_submission", args.clone()),
    );
    let (a, b) = (a.ok(), b.ok());
    assert_eq!(a["submission"]["id"], b["submission"]["id"]);
    assert_ne!(a["replayed"], b["replayed"]);
}

#[tokio::test]
async fn submission_scope_is_enforced_per_tool() {
    let server = Server::start().await;
    let id = server.application("Acme").await;
    let open = server.pdf(json!([id])).await;
    let claimed = server.pdf(json!([id])).await;
    let stored = server
        .call(
            "record_application_submission",
            submission(&id, "submit-key-1", json!([claimed["id"]])),
        )
        .await
        .ok();
    let denied = json!({"error": {"code": "insufficient_scope", "required": "mcp:submissions"}});

    for (tool, args) in [
        ("delete_application", json!({"id": id})),
        (
            "record_application_submission",
            submission(&id, "submit-key-2", json!([open["id"]])),
        ),
        ("list_application_submissions", json!({"applicationId": id})),
        (
            "get_application_submission",
            json!({"id": stored["submission"]["id"]}),
        ),
        ("get_interview_recall_package", json!({"applicationId": id})),
        ("pipeline_healthcheck", json!({})),
        ("batch_delete_applications", json!({"ids": [id]})),
        ("get_document", json!({"id": claimed["id"]})),
        ("download_document_content", json!({"id": claimed["id"]})),
        (
            "update_document_links",
            json!({"id": claimed["id"], "applicationIds": []}),
        ),
        (
            "update_document_metadata",
            json!({"id": claimed["id"], "state": "historical"}),
        ),
        (
            "update_document_metadata",
            json!({"id": open["id"], "state": "historical"}),
        ),
        (
            "update_document_metadata",
            json!({"id": open["id"], "state": "submitted"}),
        ),
    ] {
        let reply = server.call_as(TOOLS_ONLY, tool, args).await;
        assert!(reply.is_error, "{tool}");
        assert_eq!(reply.json(), denied, "{tool}");
    }

    // Not gated, as in old Nexus: timelines, notes, and the unclaimed side of documents.
    assert!(
        !server
            .call_as(
                TOOLS_ONLY,
                "list_application_events",
                json!({"applicationId": id})
            )
            .await
            .is_error
    );
    assert!(
        !server
            .call_as(TOOLS_ONLY, "get_document", json!({"id": open["id"]}))
            .await
            .is_error
    );
    let visible = server
        .call_as(TOOLS_ONLY, "list_documents", json!({}))
        .await
        .ok();
    assert_eq!(
        visible.as_array().unwrap().len(),
        1,
        "submission artifacts are hidden from the listing"
    );
    let hidden = server
        .call_as(
            TOOLS_ONLY,
            "list_documents",
            json!({"submissionId": stored["submission"]["id"]}),
        )
        .await
        .ok();
    assert_eq!(
        hidden[0]["id"], open["id"],
        "the scope filter overrides state and submissionId"
    );
    // delete_document has no scope check: immutability alone protects a claimed document.
    assert_eq!(
        server
            .call_as(TOOLS_ONLY, "delete_document", json!({"id": claimed["id"]}))
            .await
            .err_text(),
        "Document not found or access denied"
    );
    assert_eq!(
        server
            .call_as(TOOLS_ONLY, "delete_document", json!({"id": open["id"]}))
            .await
            .text,
        "Document deleted"
    );

    // API tokens always carry both scopes.
    assert!(
        !server
            .call_as(API_TOKEN, "pipeline_healthcheck", json!({}))
            .await
            .is_error
    );
}

#[tokio::test]
async fn documents_upload_list_get_download_update_delete() {
    let server = Server::start().await;
    let id = server.application("Acme").await;
    let document = server.pdf(json!([id, id])).await;
    assert_eq!(document["originalName"], "cv.pdf");
    assert_eq!(document["documentType"], "other");
    assert_eq!(document["state"], "current");
    assert_eq!(document["source"], "upload");
    assert_eq!(document["version"], 1);
    assert_eq!(document["size"], 18);
    assert_eq!(document["contentHash"].as_str().unwrap().len(), 64);
    assert!(document["filename"].as_str().unwrap().ends_with(".pdf"));
    assert_eq!(
        document["applications"],
        json!([{"id": id, "company": "Acme", "role": "Engineer"}])
    );
    assert!(document.get("applicationIds").is_none());
    assert!(document.get("demoProvenance").is_none());
    let doc_id = document["id"].as_str().unwrap();

    let upload = |content: &str, mime: &str, ids: Value| json!({"filename": "x", "mimeType": mime, "contentBase64": content, "applicationIds": ids});
    assert_eq!(
        server
            .call(
                "upload_document_content",
                upload("!!!", "application/pdf", json!([]))
            )
            .await
            .err_text(),
        "Invalid base64 content"
    );
    assert_eq!(
        server
            .call(
                "upload_document_content",
                upload("aGVsbG8=", "application/pdf", json!([]))
            )
            .await
            .err_text(),
        "File bytes do not match the declared MIME type"
    );
    assert_eq!(
        server
            .call(
                "upload_document_content",
                upload("JVBERi0=", "application/pdf", json!(["424242"]))
            )
            .await
            .err_text(),
        "Application not found or access denied"
    );
    assert!(
        server
            .call(
                "upload_document_content",
                upload("JVBERi0=", "text/plain", json!([]))
            )
            .await
            .err_text()
            .contains("invalid_enum_value")
    );

    let listed = server
        .call(
            "list_documents",
            json!({"applicationId": id, "fields": ["originalName"]}),
        )
        .await
        .ok();
    assert_eq!(listed, json!([{"id": doc_id, "originalName": "cv.pdf"}]));
    assert_eq!(
        server
            .call("list_documents", json!({"orphaned": true}))
            .await
            .ok(),
        json!([])
    );
    assert_eq!(
        server
            .call("list_documents", json!({"state": "draft"}))
            .await
            .ok(),
        json!([])
    );
    assert_eq!(
        server
            .call("list_documents", json!({"documentType": "cv"}))
            .await
            .ok(),
        json!([])
    );
    assert_eq!(
        server.call("list_documents", json!({"page": 2})).await.ok(),
        json!([])
    );
    assert_eq!(
        server
            .call("list_documents", json!({"applicationId": "abc"}))
            .await
            .err_text(),
        "Invalid numeric id: abc"
    );

    assert_eq!(
        server
            .call("get_document", json!({"id": doc_id}))
            .await
            .ok()["id"],
        doc_id
    );
    assert_eq!(
        server
            .call("get_document", json!({"id": "424242"}))
            .await
            .err_text(),
        "Document not found"
    );
    assert_eq!(
        server
            .call("get_document", json!({"id": "abc"}))
            .await
            .err_text(),
        "Invalid numeric id: abc"
    );

    let downloaded = server
        .call("download_document_content", json!({"id": doc_id}))
        .await;
    assert!(downloaded.structured.is_some());
    assert_eq!(
        downloaded.ok(),
        json!({"id": doc_id, "filename": "cv.pdf", "mimeType": "application/pdf", "size": 18, "contentBase64": "JVBERi0xLjcgc3ludGhldGlj"})
    );
    assert_eq!(
        server
            .call("download_document_content", json!({"id": "424242"}))
            .await
            .err_text(),
        "Document not found"
    );

    let meta = server
        .call("update_document_metadata", json!({
            "id": doc_id, "documentType": "cv", "version": 2, "contentHash": "A".repeat(64), "source": "agent", "generatedAt": "2026-01-02T03:04:05Z",
        }))
        .await
        .ok();
    assert_eq!(
        (&meta["documentType"], &meta["version"]),
        (&json!("cv"), &json!(2))
    );
    assert_eq!(
        meta["contentHash"],
        "A".repeat(64),
        "any case is stored as given"
    );
    assert_eq!(meta["generatedAt"], "2026-01-02T03:04:05.000Z");
    let source = server
        .call(
            "update_document_metadata",
            json!({"id": doc_id, "source": "s".repeat(101)}),
        )
        .await;
    assert!(
        source.err_text().contains("at most 100"),
        "a document source over 100 chars fails"
    );
    assert!(
        server
            .call(
                "update_document_metadata",
                json!({"id": doc_id, "contentHash": "xyz"})
            )
            .await
            .err_text()
            .contains("regex")
    );
    assert_eq!(
        server
            .call(
                "update_document_metadata",
                json!({"id": doc_id, "state": "submitted"})
            )
            .await
            .err_text(),
        "Document not found or access denied"
    );
    // The file stays reachable after a client rewrites contentHash.
    assert_eq!(
        server
            .call("download_document_content", json!({"id": doc_id}))
            .await
            .ok()["size"],
        18
    );

    let other = server.application("Beta").await;
    let relinked = server
        .call(
            "update_document_links",
            json!({"id": doc_id, "applicationIds": [other]}),
        )
        .await
        .ok();
    assert_eq!(relinked["applications"][0]["company"], "Beta");
    assert_eq!(
        server
            .call(
                "update_document_links",
                json!({"id": doc_id, "applicationIds": ["424242"]})
            )
            .await
            .err_text(),
        "Document not found or access denied"
    );
    assert_eq!(
        server
            .call(
                "update_document_links",
                json!({"id": "abc", "applicationIds": []})
            )
            .await
            .err_text(),
        "Document not found or access denied"
    );

    // Identical content shares a blob, and deleting one document leaves the other's file.
    let twin = server.pdf(json!([])).await;
    assert_eq!(
        server
            .call("delete_document", json!({"id": doc_id}))
            .await
            .text,
        "Document deleted"
    );
    assert_eq!(
        server
            .call("download_document_content", json!({"id": twin["id"]}))
            .await
            .ok()["size"],
        18
    );
    assert_eq!(
        server
            .call("delete_document", json!({"id": doc_id}))
            .await
            .err_text(),
        "Document not found or access denied"
    );
    assert!(
        !server
            .call("delete_document", json!({"id": twin["id"]}))
            .await
            .is_error
    );
    assert_eq!(
        server.db.blobs().list().unwrap(),
        Vec::<String>::new(),
        "the last reference removes the blob"
    );
}

#[tokio::test]
async fn a_document_without_its_file_reads_but_does_not_download() {
    let server = Server::start().await;
    let document = server.pdf(json!([])).await;
    let id: i64 = document["id"].as_str().unwrap().parse().unwrap();
    sqlx::query("UPDATE documents SET blob_state = 'missing', blob_sha256 = NULL, content_hash = NULL WHERE id = ?")
        .bind(id)
        .execute(server.db.pool())
        .await
        .unwrap();
    let got = server
        .call("get_document", json!({"id": document["id"]}))
        .await
        .ok();
    assert_eq!(got["originalName"], "cv.pdf");
    let download = server
        .call("download_document_content", json!({"id": document["id"]}))
        .await;
    assert_eq!(download.err_text(), "Document file is missing from storage");
    assert!(download.structured.is_none());
    assert_eq!(
        server
            .call("delete_document", json!({"id": document["id"]}))
            .await
            .text,
        "Document deleted"
    );
}

#[tokio::test]
async fn a_computed_content_hash_stays_hidden() {
    let server = Server::start().await;
    let document = server.pdf(json!([])).await;
    let id: i64 = document["id"].as_str().unwrap().parse().unwrap();
    sqlx::query("UPDATE documents SET content_hash_computed = 1 WHERE id = ?")
        .bind(id)
        .execute(server.db.pool())
        .await
        .unwrap();
    assert_eq!(
        server
            .call("get_document", json!({"id": document["id"]}))
            .await
            .ok()["contentHash"],
        Value::Null
    );
    let written = server
        .call(
            "update_document_metadata",
            json!({"id": document["id"], "contentHash": "b".repeat(64)}),
        )
        .await
        .ok();
    assert_eq!(written["contentHash"], "b".repeat(64));
}

#[tokio::test]
async fn pipeline_healthcheck_reports_findings() {
    let server = Server::start().await;
    let clean = server.call("pipeline_healthcheck", json!({})).await.ok();
    assert_eq!(
        clean,
        json!({"healthy": true, "findingCount": 0, "findings": []})
    );

    let applied = server.call("create_application", json!({"company": "A", "role": "R", "status": "applied", "followUpAt": "2020-01-01T00:00:00Z"})).await.ok();
    let lead = server
        .call(
            "create_application",
            json!({"company": "B", "role": "R", "appliedAt": "2026-01-01"}),
        )
        .await
        .ok();
    let orphan = server.pdf(json!([])).await;
    let report = server.call("pipeline_healthcheck", json!({})).await.ok();
    assert_eq!(report["healthy"], false);
    let has = |code: &str, key: &str, id: &Value| {
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["code"] == code && f[key] == *id)
    };
    assert!(has(
        "applied_without_submission",
        "applicationId",
        &applied["id"]
    ));
    assert!(has("overdue_follow_up", "applicationId", &applied["id"]));
    assert!(has(
        "date_without_applied_status",
        "applicationId",
        &lead["id"]
    ));
    assert!(has("orphan_document", "documentId", &orphan["id"]));
    assert_eq!(report["findingCount"], 4);
}

#[tokio::test]
async fn cv_profile_and_generation_gap() {
    let server = Server::start().await;
    let id = server.application("Acme").await;
    let none = server.call("get_cv_profile", json!({})).await;
    assert!(!none.is_error);
    assert_eq!(
        none.text,
        "No CV profile found. Use upsert_cv_profile to create one."
    );
    let generate = json!({"applicationId": id, "experienceIds": [], "skillCategories": []});
    assert_eq!(
        server
            .call("generate_tailored_cv", generate.clone())
            .await
            .err_text(),
        "No CV profile found. Use upsert_cv_profile first."
    );
    let bad_app = json!({"applicationId": "424242", "experienceIds": [], "skillCategories": []});
    assert_eq!(
        server
            .call("generate_tailored_cv", bad_app)
            .await
            .err_text(),
        "Application not found or access denied"
    );

    let profile = json!({
        "name": "Sam Example",
        "contact": {"email": "sam@example.test", "ignored": "stripped"},
        "profile": "Engineer",
        "skills": [{"category": "Rust", "items": ["axum"]}],
        "experience": [{"id": "acme-2024", "company": "Acme", "title": "Eng", "date": "2024", "location": "Berlin", "tier": 1, "bullets": ["Built"]}],
    });
    let stored = server.call("upsert_cv_profile", profile.clone()).await.ok();
    assert_eq!(stored["userId"], OWNER_ID);
    assert_eq!(
        stored["contact"],
        json!({"email": "sam@example.test"}),
        "zod strips unknown keys"
    );
    assert_eq!(
        (&stored["projects"], &stored["education"]),
        (&json!([]), &json!([]))
    );
    assert_eq!(
        server.call("get_cv_profile", json!({})).await.ok()["name"],
        "Sam Example"
    );
    let mut again = profile.clone();
    again["name"] = "Sam E.".into();
    assert_eq!(
        server.call("upsert_cv_profile", again).await.ok()["id"],
        stored["id"],
        "one profile"
    );
    let mut tier = profile;
    tier["experience"][0]["tier"] = 4.into();
    assert!(
        server
            .call("upsert_cv_profile", tier)
            .await
            .err_text()
            .contains("too_big")
    );

    let gap = server.call("generate_tailored_cv", generate).await;
    assert!(gap.err_text().contains("M2"));
    assert_eq!(
        server.call("list_documents", json!({})).await.ok(),
        json!([]),
        "generation writes nothing"
    );
    let patches: i64 = sqlx::query_scalar("SELECT count(*) FROM cv_patches")
        .fetch_one(server.db.pool())
        .await
        .unwrap();
    assert_eq!(patches, 0);
}
