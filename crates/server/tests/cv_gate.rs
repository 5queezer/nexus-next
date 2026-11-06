//! The gate through the real doors: a client that authorizes with real OAuth, the real `nexus serve`,
//! the real `nexus __render` child and the real `nexus cv-facts approve`. Each false fact from the
//! independent review must stay a draft: listed for the owner, no PDF, a stamp on every picture, no
//! document, until the owner approves exactly that text. All people and companies are invented.

mod support;

use std::process::Output;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use support::{Agent, Node, lock_confirmed, nexus, oauth_token};

include!("../../tailor/tests/common/normal_bullets.rs");

const H: &str = "#import \"/nexus.typ\": *\n#show: cv\n#header()\n";

fn profile() -> Value {
    json!({
        "name": "Mara Example",
        "contact": {"email": "mara@example.test", "phone": "+49 170 5550123"},
        "profile": "Backend engineer in freight.",
        "skills": [{"category": "Languages", "items": ["Rust", "Go"]}],
        "experience": [
            {"id": "brightpath", "company": "Brightpath Logistics GmbH", "title": "Software Engineer",
             "date": "03/2019 – 12/2022", "location": "Hamburg", "tier": 1,
             "bullets": ["Cut invoice errors by 40% across 3 depots.", "Built the routing service."]},
            {"id": "harborline", "company": "Harborline Systems", "title": "Platform Engineer",
             "date": "01/2023 – 06/2025", "location": "Bremen", "tier": 1,
             "bullets": ["Moved 12 services to one deploy pipeline."]},
        ],
        "education": [{"institution": "Nordsee University", "degree": "BSc Computer Science",
            "date": "2015 – 2018", "location": "Kiel"}],
    })
}

struct Gate {
    _root: tempfile::TempDir,
    node: Node,
    agent: Agent,
    application: String,
}

fn cv_facts(node: &Node, args: &[&str]) -> Output {
    nexus()
        .arg("cv-facts")
        .args(args)
        .arg("--data-dir")
        .arg(&node.dir)
        .output()
        .unwrap()
}

async fn gate() -> Gate {
    let root = tempfile::tempdir().unwrap();
    let (node, _) = Node::create(root.path(), &[]).await;
    let agent = Agent::new(
        node.addr,
        &oauth_token(&format!("http://{}", node.addr)).await,
    );
    agent.ok("upsert_cv_profile", profile()).await;
    let file = root.path().join("facts.json");
    std::fs::write(
        &file,
        json!({
            "languages": [{"name": "German", "level": "C2"}, {"name": "English", "level": "C1"}],
            "metrics": [{"key": "invoices", "text": "40% fewer invoice errors"}],
            "bannedPhrases": ["rockstar ninja"],
        })
        .to_string(),
    )
    .unwrap();
    let set = cv_facts(&node, &["set", "--file", file.to_str().unwrap()]);
    assert!(set.status.success(), "{set:?}");
    lock_confirmed(&node.dir);
    let application = agent
        .ok(
            "create_application",
            json!({"company": "Acme", "role": "Engineer", "status": "inbound"}),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    Gate {
        _root: root,
        node,
        agent,
        application,
    }
}

impl Gate {
    /// A hash of every stored document, so that "no document" can't mean "a different one".
    async fn documents(&self) -> String {
        let mut conn = self.node.audit_connection().await;
        sqlx::query_scalar(
            "SELECT coalesce(json_group_array(json_object('i', id, 'b', blob_sha256)), '[]') FROM documents",
        )
        .fetch_one(&mut conn)
        .await
        .unwrap()
    }

    async fn approvals(&self) -> i64 {
        self.node.count("SELECT count(*) FROM cv_approvals").await
    }

    /// A new variant holding just this source.
    async fn variant(&self, name: &str, source: &str) -> Value {
        let start = self.agent.ok("get_cv_source", json!({})).await;
        self.agent
            .ok(
                "patch_cv",
                json!({
                    "variant": name,
                    "createFrom": "default",
                    "expectedRevision": start["revision"],
                    "edits": [{"oldText": start["source"], "newText": source}],
                    "summary": "An agent's edit",
                    "origin": "telegram",
                }),
            )
            .await
    }

    fn approve(&self, name: &str, revision: i64) -> Output {
        cv_facts(
            self.node_ref(),
            &[
                "approve",
                "--variant",
                name,
                "--revision",
                &revision.to_string(),
                "--confirm",
            ],
        )
    }

    fn node_ref(&self) -> &Node {
        &self.node
    }
}

/// A render's JSON summary, its PDF if any, and its first PNG.
fn parts(result: &Value) -> (Value, Option<Vec<u8>>, Option<Vec<u8>>) {
    let blocks = result["content"].as_array().unwrap();
    let summary = serde_json::from_str(blocks[0]["text"].as_str().unwrap()).unwrap();
    let decode = |text: &Value| STANDARD.decode(text.as_str().unwrap()).unwrap();
    let pdf = blocks
        .iter()
        .find(|block| block["type"] == "resource")
        .map(|block| decode(&block["resource"]["blob"]));
    let png = blocks
        .iter()
        .find(|block| block["type"] == "image")
        .map(|block| decode(&block["data"]));
    (summary, pdf, png)
}

/// How many pixels of the stamp's red each quarter of an image holds.
fn red_by_quarter(png_bytes: &[u8]) -> [usize; 4] {
    let mut reader = png::Decoder::new(std::io::Cursor::new(png_bytes))
        .read_info()
        .unwrap();
    let mut buffer = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buffer).unwrap();
    let channels = info.color_type.samples();
    let mut quarters = [0; 4];
    for y in 0..info.height as usize {
        for x in 0..info.width as usize {
            let pixel = &buffer[y * info.line_size + x * channels..];
            let (r, g, b) = (
                i32::from(pixel[0]),
                i32::from(pixel[1]),
                i32::from(pixel[2]),
            );
            if r - g > 60 && r - b > 60 {
                let right = usize::from(x * 2 >= info.width as usize);
                let lower = usize::from(y * 2 >= info.height as usize);
                quarters[right + 2 * lower] += 1;
            }
        }
    }
    quarters
}

/// The review's proofs, each with the tokens the owner must be shown. All of the review's fixture.
const FALSE_FACTS: &[(&str, &str, &[&str])] = &[
    (
        "finding 1: a short name and false years",
        "*Brightpath* · Software Engineer #h(1fr) 2015 – 2025\n\n#employment(\"harborline\")\n#education(\"edu-1\")",
        &["Brightpath", "2015", "2025"],
    ),
    (
        "finding 1: another job's dates and title",
        "*Brightpath Logistics* · Platform Engineer #h(1fr) 01/2023 – 06/2025",
        &["Brightpath Logistics", "platform engineer"],
    ),
    (
        "finding 2: a count",
        "- Cut invoice errors by 40% across 12 depots.",
        &["40%", "12"],
    ),
    ("finding 2: tenure", "with 12 years in freight", &["12"]),
    (
        "finding 2: a smaller count",
        "- Moved 6 services to one deploy pipeline.",
        &["6"],
    ),
    (
        "finding 2: a metric moved",
        "- Grew revenue by 40%.",
        &["40%"],
    ),
    ("finding 2: patents", "6 patents granted.", &["6"]),
    (
        "finding 3: titles swapped",
        "*Brightpath Logistics GmbH* · Platform Engineer #h(1fr) 03/2019 – 12/2022\n\n*Harborline Systems* · Software Engineer #h(1fr) 01/2023 – 06/2025\n\n#education(\"edu-1\")",
        &["platform engineer", "software engineer"],
    ),
    (
        "finding 3: levels swapped",
        "German (C1) · English (C2)",
        &["german (c1", "english (c2"],
    ),
    (
        "finding 3: a false place",
        "*Brightpath Logistics GmbH* · Software Engineer · Bremen #h(1fr) 03/2019 – 12/2022",
        &["bremen"],
    ),
    (
        "finding 4: a month in no pattern",
        "- Joined in Nov, 2019 and left in Feb, 2025.",
        &["Nov, 2019", "Feb, 2025"],
    ),
    ("finding 4: of", "Started March of 2019", &["March of 2019"]),
    ("finding 4: reversed", "2019 March start", &["2019 March"]),
    (
        "finding 5: degree in lower case",
        "master of science in computer science",
        &["master"],
    ),
    (
        "finding 5: small caps",
        "#smallcaps[master of science]",
        &["master"],
    ),
    (
        "finding 5: honors",
        "graduated summa cum laude",
        &["graduated", "cum laude"],
    ),
    (
        "finding 5: seniority",
        "worked as lead engineer and engineering manager",
        &["lead", "manager"],
    ),
    (
        "finding 5: German",
        "habilitierte sich, dann Teamleitung, fließendes Französisch",
        &["habilitierte", "Teamleitung", "fließendes"],
    ),
    (
        "finding 10: a word between",
        "with eight consecutive years in freight",
        &["eight"],
    ),
    (
        "finding 10: German",
        "mit zweieinhalb Jahren Erfahrung",
        &["zweieinhalb"],
    ),
    (
        "finding 10: a scale",
        "12 trillion events.",
        &["12", "trillion"],
    ),
    (
        "finding 12: lower case",
        "worked for google and mckinsey at stanford",
        &["google", "mckinsey", "stanford"],
    ),
    (
        "finding 12: small caps",
        "#smallcaps[worked for google at stanford university]",
        &["google", "stanford"],
    ),
];

#[tokio::test(flavor = "multi_thread")]
async fn each_false_fact_stays_a_draft_until_the_owner_approves_exactly_it() {
    let gate = gate().await;
    let agent = &gate.agent;
    let documents = gate.documents().await;
    for (index, (name, body, tokens)) in FALSE_FACTS.iter().enumerate() {
        let variant = format!("gate-{index}");
        let source = format!("{H}{body}\n");
        let stored = gate.variant(&variant, &source).await;
        assert_eq!(stored["state"], "draft", "{name}");
        assert_eq!(
            stored["approvalPath"],
            format!("/cv/variants/{variant}#cv-approval"),
            "{name}"
        );

        // The first approval diffs against nothing: every token shows, against no allowed set.
        let listed: Vec<String> = stored["reviewItems"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["token"].as_str().unwrap().to_lowercase())
            .collect();
        for token in *tokens {
            assert!(
                listed.iter().any(|t| *t == token.to_lowercase()),
                "{name}: no {token} in {listed:?}"
            );
        }

        // A draft makes a picture with a stamp, and no PDF.
        let (summary, pdf, png) = parts(
            &agent
                .raw(
                    "render_cv_pdf",
                    json!({"variant": variant, "previewPages": 1}),
                )
                .await,
        );
        assert_eq!(summary["state"], "draft", "{name}");
        assert!(pdf.is_none(), "{name}");
        assert!(summary.get("pdfBytes").is_none(), "{name}");
        let stamped = png.unwrap();
        let quarters = red_by_quarter(&stamped);
        assert!(quarters.iter().all(|red| *red > 40), "{name}: {quarters:?}");

        // Nothing can store it.
        let saving = agent
            .call(
                "render_cv_pdf",
                json!({"variant": variant, "saveToApplicationId": gate.application}),
            )
            .await
            .unwrap();
        assert_eq!(saving.code(), "cv_approval_required", "{name}");
        assert_eq!(saving.json()["error"]["revision"], 1, "{name}");
        assert_eq!(gate.documents().await, documents, "{name}");

        // The owner reads it on the board. Without --confirm, nothing happens.
        let read = cv_facts(
            &gate.node,
            &["approve", "--variant", &variant, "--revision", "1"],
        );
        assert!(!read.status.success(), "{name}");
        let approvals = gate.approvals().await;
        assert_eq!(approvals, index as i64, "{name}: the read approved nothing");
        let approved = gate.approve(&variant, 1);
        assert!(approved.status.success(), "{name}: {approved:?}");
        assert_eq!(gate.approvals().await, index as i64 + 1, "{name}");

        // Now there is a PDF, and the same picture with no stamp.
        let (summary, pdf, png) = parts(
            &agent
                .raw(
                    "render_cv_pdf",
                    json!({"variant": variant, "previewPages": 1}),
                )
                .await,
        );
        assert_eq!(summary["state"], "approved", "{name}");
        assert!(pdf.unwrap().starts_with(b"%PDF-"), "{name}");
        let clean = png.unwrap();
        assert_ne!(clean, stamped, "{name}");
        assert_eq!(red_by_quarter(&clean), [0; 4], "{name}: the stamp is gone");
    }
}

/// `generate_tailored_cv` with its frozen schema: the two ways an agent's wording reaches it.
#[tokio::test(flavor = "multi_thread")]
async fn generate_stores_no_document_for_wording_the_owner_has_not_seen() {
    let gate = gate().await;
    let agent = &gate.agent;
    let variant = format!("application-{}", gate.application);
    let args = json!({"applicationId": gate.application, "experienceIds": ["brightpath"],
        "skillCategories": ["Languages"]});

    // Finding 2.
    let documents = gate.documents().await;
    let mut with_override = args.clone();
    with_override["profileOverride"] = "Over 12 years in freight, 6 at Stanford.".into();
    let held = agent
        .call("generate_tailored_cv", with_override)
        .await
        .unwrap();
    assert_eq!(held.code(), "cv_approval_required", "{}", held.text);
    let error = &held.json()["error"];
    assert_eq!(error["variant"], variant);
    assert_eq!(error["revision"], 1);
    assert_eq!(
        error["approvalPath"],
        format!("/cv/variants/{variant}#cv-approval")
    );
    assert_eq!(gate.documents().await, documents);
    let stored: String = {
        let mut conn = gate.node.audit_connection().await;
        sqlx::query_scalar("SELECT warnings FROM cv_revisions ORDER BY id DESC LIMIT 1")
            .fetch_one(&mut conn)
            .await
            .unwrap()
    };
    assert!(
        stored.contains("Stanford") && stored.contains("\"12\""),
        "{stored}"
    );

    // The owner's own wording, unchanged since the lock: the lock approved it.
    let first = agent.ok("generate_tailored_cv", args.clone()).await;
    assert_eq!(first["message"], "CV generated successfully");
    assert_ne!(gate.documents().await, documents);
    let (summary, pdf, _) = parts(
        &agent
            .raw(
                "render_cv_pdf",
                json!({"variant": variant, "previewPages": 0}),
            )
            .await,
    );
    assert_eq!(summary["state"], "approved");
    assert!(pdf.is_some());
    let by: Vec<String> = {
        let mut conn = gate.node.audit_connection().await;
        sqlx::query_scalar("SELECT approved_by FROM cv_approvals")
            .fetch_all(&mut conn)
            .await
            .unwrap()
    };
    assert_eq!(by, ["lock"]);

    // Finding 12, laundering through the profile: a locked upsert stops counting as the lock's.
    let documents = gate.documents().await;
    let mut laundered = profile();
    laundered["profile"] =
        "Harvard alumnus and Google veteran. Worked with McKinsey partners.".into();
    agent.ok("upsert_cv_profile", laundered).await;
    let held = agent
        .call("generate_tailored_cv", args.clone())
        .await
        .unwrap();
    assert_eq!(held.code(), "cv_approval_required", "{}", held.text);
    let revision = held.json()["error"]["revision"].as_i64().unwrap();
    assert_eq!(gate.documents().await, documents);
    let copied = agent
        .call(
            "patch_cv",
            json!({"variant": "copied", "createFrom": "default", "expectedRevision": 0,
                "edits": [], "summary": "s"}),
        )
        .await
        .unwrap();
    assert_eq!(copied.json()["state"], "draft", "{}", copied.text);
    let items = copied.json()["reviewItems"].to_string();
    assert!(
        items.contains("Harvard") && items.contains("McKinsey"),
        "{items}"
    );

    // After the owner approves it, the same call goes through, with no loop.
    assert!(gate.approve(&variant, revision).status.success());
    let stored = agent.ok("generate_tailored_cv", args).await;
    assert_eq!(stored["message"], "CV generated successfully");
    assert_ne!(gate.documents().await, documents);
}

/// Finding 14: what the word rules refused nearly every time stores, in both languages.
#[tokio::test(flavor = "multi_thread")]
async fn fifty_normal_bullets_all_store_as_drafts() {
    let gate = gate().await;
    let agent = &gate.agent;
    let marker = "- Built the routing service.";
    let source = format!("{H}#employment(\"brightpath\")[\n{marker}\n]\n");
    gate.variant("bullets", &source).await;
    let bullets = ENGLISH_BULLETS.iter().chain(&GERMAN_BULLETS);
    for (at, bullet) in bullets.enumerate() {
        let reply = agent
            .ok(
                "patch_cv",
                json!({
                    "variant": "bullets",
                    "expectedRevision": at + 1,
                    "summary": "A normal bullet",
                    "edits": [{"oldText": marker, "newText": format!("- {bullet}\n{marker}")}],
                }),
            )
            .await;
        assert_eq!(reply["state"], "draft", "{bullet}");
    }
    assert_eq!(gate.approvals().await, 0);
    assert_eq!(
        agent
            .ok(
                "list_cv_revisions",
                json!({"variant": "bullets", "pageSize": 1})
            )
            .await["total"],
        51
    );
}

/// Three small patches can't hide what changed: the owner reads them against what was last approved.
#[tokio::test(flavor = "multi_thread")]
async fn a_chain_of_drafts_shows_whole_against_the_last_approved_text() {
    let gate = gate().await;
    let agent = &gate.agent;
    let source = format!("{H}Moved freight for a living.\n");
    gate.variant("chain", &source).await;
    assert!(gate.approve("chain", 1).status.success());

    let mut old = "for a living.".to_owned();
    for (at, new) in [
        "for a living across 11 ports.",
        "for a living across 11 ports with 22 depots.",
        "for a living across 11 ports with 22 depots in 33 towns.",
    ]
    .iter()
    .enumerate()
    {
        let patched = agent
            .ok(
                "patch_cv",
                json!({"variant": "chain", "expectedRevision": at + 1, "summary": "small",
                    "edits": [{"oldText": old, "newText": new}]}),
            )
            .await;
        assert_eq!(patched["state"], "draft");
        old = (*new).to_owned();
    }
    let read = cv_facts(
        &gate.node,
        &["approve", "--variant", "chain", "--revision", "4"],
    );
    let printed = String::from_utf8(read.stdout).unwrap();
    assert!(printed.contains("compared with revision 1"), "{printed}");
    assert!(
        printed.contains("- Moved freight for a living."),
        "{printed}"
    );
    assert!(
        printed
            .contains("+ Moved freight for a living across 11 ports with 22 depots in 33 towns."),
        "{printed}"
    );
    for number in ["11", "22", "33"] {
        assert!(
            printed.contains(&format!(": {number}\n")),
            "{number} in {printed}"
        );
    }
}

/// Arguments that satisfy a tool's schema, from the schema itself.
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

/// No tool can approve: every listed tool, with what an attacker would try, changes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn no_listed_tool_can_approve_whatever_it_is_passed() {
    let gate = gate().await;
    let agent = &gate.agent;
    let source = format!("{H}Moved freight for 12 years.\n");
    gate.variant("target", &source).await;
    let head_state = async || {
        agent
            .ok("get_cv_source", json!({"variant": "target"}))
            .await["state"]
            .clone()
    };
    assert_eq!(head_state().await, "draft");
    // So that a generate has no wording the lock could stand for, which is the owner's approval.
    let mut changed = profile();
    changed["profile"] = "Backend engineer, in freight.".into();
    agent.ok("upsert_cv_profile", changed).await;

    let listed = reqwest::Client::new()
        .post(format!("http://{}/api/mcp", gate.node.addr))
        .bearer_auth(&oauth_token(&format!("http://{}", gate.node.addr)).await)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .body(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string())
        .send()
        .await
        .unwrap();
    let listed: Value = serde_json::from_str(&listed.text().await.unwrap()).unwrap();
    let tools = listed["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 40);
    let attack = json!({
        "approve": true, "approved": true, "approvedBy": "owner", "approved_by": "cli",
        "state": "approved", "confirm": true, "force": true, "owner": true, "source": "dashboard",
        "origin": "dashboard", "actor": "cli", "variant": "target", "revision": 1,
    });
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        // Deleting the application would only end this test's other calls early.
        if name.contains("delete") {
            continue;
        }
        let mut args = valid_args(&tool["inputSchema"], &gate.application);
        for (key, value) in attack.as_object().unwrap() {
            if tool["inputSchema"]["properties"].get(key).is_none()
                || matches!(key.as_str(), "origin" | "variant" | "revision")
            {
                args[key] = value.clone();
            }
        }
        let _ = agent.call(name, args).await;
        assert_eq!(gate.approvals().await, 0, "{name}");
        assert_eq!(head_state().await, "draft", "{name}");
    }
    for name in [
        "approve_cv",
        "cv_approve",
        "approve_cv_revision",
        "approve",
        "cv_facts_approve",
    ] {
        let reply = agent
            .call(name, json!({"variant": "target", "revision": 1}))
            .await
            .unwrap();
        assert!(reply.is_error, "{name}");
        assert!(reply.text.contains("not found"), "{name}: {}", reply.text);
    }
    // `origin` is text a caller sends. A caller's word for itself grants nothing.
    let mut wording = "Moved freight".to_owned();
    for origin in ["dashboard", "cli", "lock", "owner", "voice"] {
        let start = agent
            .ok("get_cv_source", json!({"variant": "target"}))
            .await;
        let new = format!("{wording}, {origin}");
        let patched = agent
            .ok(
                "patch_cv",
                json!({
                    "variant": "target",
                    "expectedRevision": start["revision"],
                    "edits": [{"oldText": wording, "newText": new}],
                    "summary": "s",
                    "origin": origin,
                }),
            )
            .await;
        wording = new;
        assert_eq!(patched["state"], "draft", "{origin}");
    }
    assert_eq!(gate.approvals().await, 0);
}

/// Any changed fact invalidates what is built on it, and nothing else does.
#[tokio::test(flavor = "multi_thread")]
async fn an_approval_holds_for_its_facts_and_its_exact_revision() {
    let gate = gate().await;
    let agent = &gate.agent;
    let source = format!("{H}#employment(\"brightpath\")\n");
    gate.variant("bound", &source).await;

    // The wrong revision can't be approved, on the board either.
    let wrong = gate.approve("bound", 9);
    assert!(!wrong.status.success());
    assert!(gate.approve("bound", 1).status.success());
    assert_eq!(gate.approvals().await, 1);
    let approved =
        || async { agent.ok("get_cv_source", json!({"variant": "bound"})).await["state"].clone() };
    assert_eq!(approved().await, "approved");

    // A vocabulary edit changes nothing a CV can render.
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("vocabulary.json");
    std::fs::write(&file, json!({"vocabulary": ["Kubernetes"]}).to_string()).unwrap();
    let set = cv_facts(&gate.node, &["set", "--file", file.to_str().unwrap()]);
    assert!(set.status.success());
    assert_eq!(approved().await, "approved");

    // Unlocked: a draft for as long as it lasts. Relocked with no change: approved again.
    assert!(cv_facts(&gate.node, &["unlock"]).status.success());
    assert_eq!(approved().await, "draft");
    let (summary, pdf, _) = parts(
        &agent
            .raw(
                "render_cv_pdf",
                json!({"variant": "bound", "previewPages": 0}),
            )
            .await,
    );
    assert_eq!(summary["state"], "draft");
    assert!(pdf.is_none());
    let relocked = cv_facts(&gate.node, &["lock"]);
    assert!(relocked.status.success(), "nothing changed: {relocked:?}");
    assert_eq!(approved().await, "approved");

    // A changed fact: draft, and no PDF.
    std::fs::write(&file, json!({"metrics": []}).to_string()).unwrap();
    let set = cv_facts(&gate.node, &["set", "--file", file.to_str().unwrap()]);
    assert!(set.status.success());
    assert_eq!(approved().await, "draft");
    let (summary, pdf, png) = parts(
        &agent
            .raw(
                "render_cv_pdf",
                json!({"variant": "bound", "previewPages": 1}),
            )
            .await,
    );
    assert_eq!(summary["state"], "draft");
    assert!(pdf.is_none());
    assert!(red_by_quarter(&png.unwrap()).iter().all(|red| *red > 40));
    let saving = agent
        .call(
            "render_cv_pdf",
            json!({"variant": "bound", "saveToApplicationId": gate.application}),
        )
        .await
        .unwrap();
    assert_eq!(saving.code(), "cv_approval_required");

    // An older revision isn't what the owner is reading.
    let start = agent.ok("get_cv_source", json!({"variant": "bound"})).await;
    agent
        .ok(
            "patch_cv",
            json!({"variant": "bound", "expectedRevision": start["revision"], "summary": "more",
                "edits": [{"oldText": "#employment(\"brightpath\")", "newText": "#employment(\"harborline\")"}]}),
        )
        .await;
    let stale = gate.approve("bound", 1);
    assert!(!stale.status.success());
    assert!(
        String::from_utf8(stale.stderr)
            .unwrap()
            .contains("isn't the newest"),
        "names the newest"
    );
    assert_eq!(gate.approvals().await, 1);

    // Hard rules are read again at approval: no approval overrides one.
    std::fs::write(&file, json!({"bannedPhrases": ["Harborline"]}).to_string()).unwrap();
    assert!(
        cv_facts(&gate.node, &["set", "--file", file.to_str().unwrap()])
            .status
            .success()
    );
    assert!(!gate.approve("bound", 2).status.success());
    assert_eq!(gate.approvals().await, 1);
}

/// Unlocked, it stays as old Nexus so parity holds: the same response, with bytes that can't be taken
/// for final.
#[tokio::test(flavor = "multi_thread")]
async fn unlocked_generate_answers_as_old_nexus_with_a_stamped_pdf() {
    let root = tempfile::tempdir().unwrap();
    let (node, _) = Node::create(root.path(), &[]).await;
    let token = oauth_token(&format!("http://{}", node.addr)).await;
    let agent = Agent::new(node.addr, &token);
    agent.ok("upsert_cv_profile", profile()).await;
    let application = agent
        .ok(
            "create_application",
            json!({"company": "Acme", "role": "Engineer", "status": "inbound"}),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let generated = agent
        .ok(
            "generate_tailored_cv",
            json!({"applicationId": application, "experienceIds": ["brightpath"], "skillCategories": []}),
        )
        .await;
    assert_eq!(generated["message"], "CV generated successfully");
    let unlocked_bytes = agent
        .ok(
            "download_document_content",
            json!({"id": generated["documentId"]}),
        )
        .await["contentBase64"]
        .as_str()
        .unwrap()
        .to_owned();

    lock_confirmed(&node.dir);
    let generated = agent
        .ok(
            "generate_tailored_cv",
            json!({"applicationId": application, "experienceIds": ["brightpath"], "skillCategories": []}),
        )
        .await;
    let locked_bytes = agent
        .ok(
            "download_document_content",
            json!({"id": generated["documentId"]}),
        )
        .await["contentBase64"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(unlocked_bytes, locked_bytes, "the stamp is in the PDF");
    let (unlocked, locked) = (
        STANDARD.decode(unlocked_bytes).unwrap(),
        STANDARD.decode(locked_bytes).unwrap(),
    );
    assert!(unlocked.len() > locked.len(), "the stamp adds to it");
}
