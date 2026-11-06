//! Hostile values through MCP and back out of every human surface of the real binary: no panics, no
//! 5xx, nothing executable in HTML, nothing injected into the calendar. Extreme dates included.

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;

use serde_json::{Value, json};
use support::{Agent, Node};

const CALENDAR_TOKEN: &str = "cal-7f3a91c2e8d4b6051a2f7c9e3d8b4a60";

const VALUES: [&str; 12] = [
    "👩‍👩‍👧‍👦🦀🇪🇸",
    "\u{202E}evil\u{202C} שלום مرحبا",
    "e\u{301}\u{301}\u{301}\u{301}",
    "before\0after",
    "\u{FEFF}bom\u{200B}\u{200D}",
    "<script>alert(1)</script>\"><img src=x onerror=alert(2)>'",
    "javascript:alert(document.cookie)",
    "x\r\nX-Injected: 1\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nSUMMARY:injected\r\nEND:VEVENT",
    "a\nb\tc\u{b}d\u{c}e\u{1b}f\u{7f}",
    "'; DROP TABLE applications; -- \" OR 1=1",
    "%_\\%{{7*7}}${x}%s%n",
    "../../../etc/passwd",
];

const DATES: [&str; 11] = [
    "9999-12-31T23:59:59.999Z",
    "0000-01-01T00:00:00.000Z",
    "0001-01-01T00:00:00.000Z",
    "1969-12-31T23:59:59.000Z",
    "2028-02-29T00:00:00.000Z",
    "2028-02-29T12:00:00.000Z",
    // Madrid's clock jumps at 01:00Z in March and falls back at 01:00Z in October.
    "2026-03-29T00:59:59.000Z",
    "2026-03-29T01:30:00.000Z",
    "2026-10-25T00:30:00.000Z",
    "2026-10-25T01:30:00.000Z",
    "2026-10-25T00:00:00.000Z",
];

async fn seed(agent: &Agent) -> Vec<String> {
    let mut ids = Vec::new();
    for value in VALUES {
        let short = |max: usize| value.chars().take(max).collect::<String>();
        let created = agent
            .ok(
                "create_application",
                json!({
                    "company": value, "role": value, "notes": value, "jobDescription": value,
                    "source": value, "resumeId": value, "triageReason": value, "jobSummary": value,
                    "timezoneOverlap": value, "atsName": value, "requisitionId": value,
                    "nextAction": value, "currentStage": value, "primaryLocations": [value],
                    "followUpAt": "2031-02-03T10:00:00.000Z",
                }),
            )
            .await;
        let id = created["id"].as_str().unwrap().to_owned();
        let metadata = [
            (
                "interview_scheduled",
                json!({"interviewType": short(100), "scheduledAt": "2031-02-03T10:00:00.000Z",
                       "outcome": value, "nextAction": value, "toStage": value}),
            ),
            (
                "recruiter_contacted",
                json!({"channel": value, "outcome": value}),
            ),
            ("note_added", json!({"note": value})),
        ];
        for (event_type, metadata) in metadata {
            let reply = agent
                .call(
                    "record_application_event",
                    json!({"applicationId": id, "type": event_type, "metadata": metadata}),
                )
                .await
                .expect("a clean answer");
            // A rejection is fine and comes from validation, never from a failed statement.
            assert!(!reply.text.contains("database"), "{}", reply.text);
        }
        agent
            .ok(
                "create_contact",
                json!({"applicationId": id, "name": value, "email": value, "role": value,
                       "phone": value, "linkedIn": value}),
            )
            .await;
        agent
            .ok(
                "upload_document_content",
                json!({
                    "filename": value, "mimeType": "application/pdf",
                    "contentBase64": "JVBERi0xLjcgc3ludGhldGlj", "applicationIds": [id],
                }),
            )
            .await;
        ids.push(id);
    }
    for date in DATES {
        for field in ["appliedAt", "lastContact", "followUpAt"] {
            let created = agent
                .ok(
                    "create_application",
                    json!({"company": format!("{field} {date}"), "role": "R", field: date}),
                )
                .await;
            ids.push(created["id"].as_str().unwrap().to_owned());
        }
        let created = agent
            .ok(
                "create_application",
                json!({"company": format!("Scheduled {date}"), "role": "R"}),
            )
            .await;
        let id = created["id"].as_str().unwrap().to_owned();
        agent
            .ok(
                "record_application_event",
                json!({"applicationId": id, "type": "interview_scheduled", "occurredAt": date,
                       "metadata": {"interviewType": "screen", "scheduledAt": date, "durationMinutes": 60}}),
            )
            .await;
        ids.push(id);
    }
    ids
}

async fn get(http: &reqwest::Client, url: &str) -> (u16, String) {
    let reply = http.get(url).send().await.unwrap_or_else(|e| {
        panic!("{url}: the connection died, likely a panic: {e}");
    });
    let status = reply.status().as_u16();
    assert!(status < 500, "{url}: {status}");
    (status, reply.text().await.unwrap())
}

fn assert_inert(url: &str, html: &str) {
    let lower = html.to_lowercase();
    for marker in ["<script", "<img", "href=\"javascript:", "href='javascript:"] {
        assert!(!lower.contains(marker), "{url} has raw {marker}");
    }
}

fn assert_valid_feed(feed: &str) {
    for byte in feed.bytes() {
        assert!(
            byte >= 0x20 || matches!(byte, b'\r' | b'\n' | b'\t'),
            "control byte {byte:#x} in the feed"
        );
    }
    let bare_newline = feed.replace("\r\n", "");
    assert!(!bare_newline.contains(['\r', '\n']), "bare CR or LF");
    for line in feed.split("\r\n") {
        assert!(line.len() <= 75, "unfolded line: {line:?}");
    }
    // The hostile text may appear escaped inside a value, so only whole lines count.
    let unfolded = feed.replace("\r\n ", "");
    let lines: Vec<&str> = unfolded.split("\r\n").collect();
    let begins = lines.iter().filter(|line| **line == "BEGIN:VEVENT").count();
    assert!(begins > 0);
    assert!(
        !lines
            .iter()
            .any(|line| *line == "SUMMARY:injected" || line.starts_with("X-Injected")),
        "a value became a line"
    );
    assert_eq!(begins, lines.iter().filter(|l| **l == "END:VEVENT").count());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hostile_rows_and_extreme_dates_survive_every_surface() {
    let root = tempfile::tempdir().unwrap();
    let token_file = root.path().join("calendar-token");
    fs::write(&token_file, CALENDAR_TOKEN).unwrap();
    fs::set_permissions(&token_file, fs::Permissions::from_mode(0o600)).unwrap();
    let (mut node, token) = Node::create(
        root.path(),
        &[
            "--insecure-local-only",
            "--calendar-token-file",
            token_file.to_str().unwrap(),
        ],
    )
    .await;
    let agent = Agent::new(node.addr, &token);
    let ids = seed(&agent).await;

    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let base = format!("http://{}", node.addr);
    for path in [
        "/",
        "/queue",
        "/?status=<script>",
        "/?q=%3Cscript%3E",
        "/?q=%25_%5C",
    ] {
        let url = format!("{base}{path}");
        let (status, body) = get(&http, &url).await;
        assert_eq!(status, 200, "{url}");
        assert_inert(&url, &body);
    }
    for id in &ids {
        let url = format!("{base}/applications/{id}");
        let (status, body) = get(&http, &url).await;
        assert_eq!(status, 200, "{url}");
        assert_inert(&url, &body);
        for tail in ["", "/events", "/contacts", "/documents", "/submissions"] {
            let url = format!("{base}/api/applications/{id}{tail}");
            let (status, body) = get(&http, &url).await;
            assert_eq!(status, 200, "{url}");
            serde_json::from_str::<Value>(&body).expect(&url);
        }
    }
    let documents = agent.ok("list_documents", json!({"pageSize": 200})).await;
    for document in documents.as_array().unwrap() {
        let url = format!(
            "{base}/api/documents/{}/download",
            document["id"].as_str().unwrap()
        );
        let reply = http.get(&url).send().await.unwrap();
        assert_eq!(reply.status().as_u16(), 200, "{url}");
        assert!(reply.headers().get("x-injected").is_none());
        assert_eq!(
            reply.headers().get("content-type").unwrap(),
            "application/pdf"
        );
    }
    for path in [
        "/applications/abc",
        "/applications/-1",
        "/applications/9223372036854775808",
        "/api/applications/9223372036854775808/events",
        "/api/documents/1e3/download",
        "/api/applications?limit=-1&cursor=%F0%9F%A6%80",
    ] {
        get(&http, &format!("{base}{path}")).await;
    }

    let (status, feed) = get(
        &http,
        &format!("{base}/calendar.ics?token={CALENDAR_TOKEN}"),
    )
    .await;
    assert_eq!(status, 200);
    assert_valid_feed(&feed);
    let unfolded = feed.replace("\r\n ", "");
    for instant in [
        "DTSTART:20280229T000000Z",
        "DTSTART:20260329T013000Z",
        "DTSTART:20261025T003000Z",
        "DTSTART:20261025T013000Z",
    ] {
        assert!(unfolded.contains(instant), "{instant}");
    }

    let healthcheck = agent.call("pipeline_healthcheck", json!({})).await.unwrap();
    assert!(!healthcheck.is_error, "{}", healthcheck.text);

    node.assert_database_sound().await;
    node.terminate();
    let logs = node.logs().to_lowercase();
    assert!(!logs.contains("panic"), "{logs}");
}
