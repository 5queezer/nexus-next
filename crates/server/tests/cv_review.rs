//! The real `nexus serve` and `nexus cv-facts`: what an owner and a lost or crowding client see. All
//! people and companies are invented.

mod support;

use std::path::Path;
use std::process::Output;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::{Agent, Node, lock_confirmed, nexus};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

const START: &str = "#import \"/nexus.typ\": *\n#show: cv\n";
/// Takes seconds in a debug build, in constant memory, and then passes the check.
const SLOW_BUT_FINE: &str = "#for a in range(3000) { for b in range(3000) { } }\nWork.\n";
/// Runs until the worker's timeout.
const FOR_DAYS: &str =
    "#for a in range(9999) { for b in range(9999) { for c in range(9999) { } } }\n";

fn profile() -> Value {
    json!({
        "name": "Mara Example",
        "contact": {"email": "mara@example.test", "location": "Hamburg"},
        "profile": "Backend engineer.",
        "skills": [],
        "experience": [
            {"id": "brightpath", "company": "Brightpath Logistics", "title": "Software Engineer",
             "date": "03/2019 – 12/2022", "location": "Hamburg", "tier": 1,
             "bullets": ["Cut invoice errors by 40% across 3 depots."]},
        ],
        "education": [{"institution": "Nordsee University", "degree": "BSc Computer Science",
            "date": "2015 – 2018", "location": "Kiel"}],
    })
}

fn cv_facts(dir: &Path, args: &[&str]) -> Output {
    nexus()
        .arg("cv-facts")
        .args(args)
        .arg("--data-dir")
        .arg(dir)
        .output()
        .unwrap()
}

/// The children of the server that run a compile.
fn compiles_of(node: &Node) -> Vec<String> {
    let mut found = Vec::new();
    for task in std::fs::read_dir(format!("/proc/{}/task", node.pid()))
        .unwrap()
        .flatten()
    {
        let children = std::fs::read_to_string(task.path().join("children")).unwrap_or_default();
        for child in children.split_whitespace() {
            let command = std::fs::read(format!("/proc/{child}/cmdline")).unwrap_or_default();
            if String::from_utf8_lossy(&command).contains("__render") {
                found.push(child.to_owned());
            }
        }
    }
    found
}

async fn wait_for(what: &str, within: Duration, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < within, "{what} after {within:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Finding 13, as the review ran it: an agent rewrites the facts in the unlocked window, and the owner
/// sees it all at the lock.
#[tokio::test(flavor = "multi_thread")]
async fn a_lock_shows_what_an_agent_wrote_and_needs_a_confirmation() {
    let root = tempfile::tempdir().unwrap();
    let (node, token) = Node::create(root.path(), &[]).await;
    let agent = Agent::new(node.addr, &token);
    agent.ok("upsert_cv_profile", profile()).await;

    let refused = cv_facts(&node.dir, &["lock"]);
    assert!(
        !refused.status.success(),
        "a first lock confirms everything"
    );
    let printed = String::from_utf8(refused.stdout).unwrap();
    assert!(
        printed.contains("changed name: (none) -> \"Mara Example\""),
        "{printed}"
    );
    assert!(
        printed.contains("wording changed profile: (none) -> [\"Backend engineer.\"]"),
        "{printed}"
    );
    let locked: i64 = node
        .count("SELECT count(*) FROM cv_facts WHERE locked_at IS NOT NULL")
        .await;
    assert_eq!(locked, 0, "nothing written");

    lock_confirmed(&node.dir);
    assert!(cv_facts(&node.dir, &["unlock"]).status.success());
    let unchanged = cv_facts(&node.dir, &["lock"]);
    assert!(
        unchanged.status.success(),
        "nothing to confirm: {unchanged:?}"
    );
    assert!(cv_facts(&node.dir, &["unlock"]).status.success());

    let mut rewritten = profile();
    rewritten["experience"][0]["title"] = json!("Principal Engineer");
    rewritten["experience"][0]["date"] = json!("03/2015 – 12/2022");
    rewritten["education"][0]["degree"] = json!("PhD Computer Science");
    agent.ok("upsert_cv_profile", rewritten.clone()).await;
    rewritten["experience"][0]["title"] = json!("Distinguished Principal");
    agent.ok("upsert_cv_profile", rewritten.clone()).await;
    rewritten["experience"][0]["title"] = json!("Principal Engineer");
    agent.ok("upsert_cv_profile", rewritten).await;

    let refused = cv_facts(&node.dir, &["lock"]);
    assert!(!refused.status.success());
    let printed = String::from_utf8(refused.stdout).unwrap();
    for line in [
        "changed employment[0:brightpath].title: \"Software Engineer\" -> \"Principal Engineer\"",
        "changed employment[0:brightpath].date: \"03/2019 – 12/2022\" -> \"03/2015 – 12/2022\"",
        "changed education[0:edu-1].qualification: \"BSc Computer Science\" -> \"PhD Computer Science\"",
    ] {
        assert!(printed.lines().any(|l| l == line), "{line} in {printed}");
    }
    assert!(
        String::from_utf8(refused.stderr)
            .unwrap()
            .contains("--confirm")
    );
    let open: i64 = node
        .count("SELECT count(*) FROM cv_facts WHERE locked_at IS NULL")
        .await;
    assert_eq!(open, 1, "still unlocked");

    let confirmed = lock_confirmed(&node.dir);
    assert!(confirmed.contains("Principal Engineer"), "{confirmed}");

    let mut conn = node.audit_connection().await;
    let writes: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT changed_by, change, state ->> '$.employment[0].title' FROM cv_facts_history
         WHERE changed_by = 'mcp' ORDER BY id",
    )
    .fetch_all(&mut conn)
    .await
    .unwrap();
    let titles: Vec<&str> = writes.iter().map(|w| w.2.as_str()).collect();
    assert_eq!(
        titles,
        [
            "",
            "Software Engineer",
            "Principal Engineer",
            "Distinguished Principal"
        ],
        "{writes:?}"
    );
}

/// The race the plain `--confirm` left open: an agent writing between the printout and the command.
#[tokio::test(flavor = "multi_thread")]
async fn a_lock_confirmed_with_an_old_hash_is_refused_and_shows_what_was_written() {
    let root = tempfile::tempdir().unwrap();
    let (node, token) = Node::create(root.path(), &[]).await;
    let agent = Agent::new(node.addr, &token);
    agent.ok("upsert_cv_profile", profile()).await;

    let shown = cv_facts(&node.dir, &["lock"]);
    let said = String::from_utf8(shown.stderr).unwrap();
    let old_hash = said
        .split("--confirm ")
        .nth(1)
        .unwrap()
        .split('`')
        .next()
        .unwrap()
        .to_owned();

    let mut rewritten = profile();
    rewritten["experience"][0]["title"] = json!("Principal Engineer");
    agent.ok("upsert_cv_profile", rewritten).await;

    let refused = cv_facts(&node.dir, &["lock", "--confirm", &old_hash]);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        String::from_utf8(refused.stdout)
            .unwrap()
            .contains("Principal Engineer"),
        "the new change is printed"
    );
    let said = String::from_utf8(refused.stderr).unwrap();
    assert!(said.contains("something changed"), "{said}");
    let open: i64 = node
        .count("SELECT count(*) FROM cv_facts WHERE locked_at IS NULL")
        .await;
    assert_eq!(open, 1, "still unlocked");
    let plain = nexus()
        .args(["cv-facts", "lock", "--confirm", "--data-dir"])
        .arg(&node.dir)
        .output()
        .unwrap();
    assert!(!plain.status.success(), "--confirm names a hash");

    assert!(lock_confirmed(&node.dir).contains("Principal Engineer"));
    let locked: i64 = node
        .count("SELECT count(*) FROM cv_facts WHERE locked_at IS NOT NULL")
        .await;
    assert_eq!(locked, 1);
}

/// Finding 20: rmcp runs a tool in a task of its own, so a lost connection reaches it only through
/// the token of the call. The compile ends with the client, and nothing is stored.
#[tokio::test(flavor = "multi_thread")]
async fn a_patch_whose_client_has_gone_stops_and_stores_nothing() {
    let root = tempfile::tempdir().unwrap();
    let (node, token) = Node::create(root.path(), &[]).await;
    let agent = Agent::new(node.addr, &token);
    agent.ok("upsert_cv_profile", profile()).await;
    lock_confirmed(&node.dir);
    let source = agent.ok("get_cv_source", json!({})).await["source"]
        .as_str()
        .unwrap()
        .to_owned();
    let new = format!(
        "{START}#header()\n#employment(\"brightpath\")\n#education(\"edu-1\")\n{SLOW_BUT_FINE}"
    );
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "patch_cv", "arguments": {
            "expectedRevision": 0, "summary": "slow",
            "edits": [{"oldText": source, "newText": new}],
        }},
    })
    .to_string();
    let mut client = TcpStream::connect(node.addr).await.unwrap();
    let request = format!(
        "POST /api/mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {token}\r\n\
         Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{body}",
        node.addr,
        body.len()
    );
    client.write_all(request.as_bytes()).await.unwrap();
    wait_for("no compile started", Duration::from_secs(5), || {
        !compiles_of(&node).is_empty()
    })
    .await;
    drop(client);
    wait_for("the compile still runs", Duration::from_secs(2), || {
        compiles_of(&node).is_empty()
    })
    .await;

    // Long enough for a compile that kept going to have stored its revision.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(agent.ok("get_cv_source", json!({})).await["revision"], 0);
    let count = node.count("SELECT count(*) FROM cv_revisions").await;
    assert_eq!(count, 0);

    // The control: with its client there, the same source does store, and it does take this long.
    let started = Instant::now();
    let stored = agent
        .ok(
            "patch_cv",
            json!({"expectedRevision": 0, "summary": "slow, with a client",
                "edits": [{"oldText": source, "newText": new}]}),
        )
        .await;
    assert_eq!(stored["revision"], 1);
    assert!(
        started.elapsed() > Duration::from_secs(1),
        "too fast to prove anything: {:?}",
        started.elapsed()
    );
}

/// Finding 19: past a compile and two waiting, a call is refused at once and holds no slot of the
/// pool every other tool needs.
#[tokio::test(flavor = "multi_thread")]
async fn a_crowd_of_compiles_is_refused_at_once_and_other_tools_keep_working() {
    let root = tempfile::tempdir().unwrap();
    let (node, token) = Node::create(root.path(), &[]).await;
    let agent = Agent::new(node.addr, &token);
    agent.ok("upsert_cv_profile", profile()).await;
    lock_confirmed(&node.dir);
    let source = agent.ok("get_cv_source", json!({})).await["source"]
        .as_str()
        .unwrap()
        .to_owned();
    let args = json!({"expectedRevision": 0, "summary": "slow",
        "edits": [{"oldText": source, "newText": format!("{START}{FOR_DAYS}")}]});
    let started = Instant::now();
    let calls: Vec<_> = (0..8)
        .map(|_| {
            let (agent, args) = (agent.clone(), args.clone());
            tokio::spawn(async move {
                let reply = agent.call("patch_cv", args).await.unwrap();
                (reply.code(), started.elapsed())
            })
        })
        .collect();
    let mut refused = 0;
    let mut waiting = Vec::new();
    tokio::time::sleep(Duration::from_secs(3)).await;
    for call in calls {
        if call.is_finished() {
            let (code, took) = call.await.unwrap();
            assert_eq!(code, "cv_render_busy");
            assert!(took < Duration::from_secs(3), "{took:?}");
            refused += 1;
        } else {
            waiting.push(call);
        }
    }
    assert_eq!(refused, 5, "one compiling and two waiting stay");
    let before = Instant::now();
    agent.ok("list_applications", json!({})).await;
    assert!(
        before.elapsed() < Duration::from_secs(2),
        "{:?}",
        before.elapsed()
    );
    for call in &waiting {
        call.abort();
    }
    wait_for(
        "a compile outlived its clients",
        Duration::from_secs(5),
        || compiles_of(&node).is_empty(),
    )
    .await;
}

/// Finding 7: nothing an agent wrote reaches the owner's terminal raw, in anything these print or ask.
#[tokio::test(flavor = "multi_thread")]
async fn nothing_an_agent_wrote_reaches_the_owners_terminal_raw() {
    const ESC: &str = "\u{1b}[2K\r\u{1b}[?7l\u{202e}";
    let root = tempfile::tempdir().unwrap();
    let (node, token) = Node::create(root.path(), &[]).await;
    let agent = Agent::new(node.addr, &token);
    let raw = |output: &Output| {
        let bytes = [output.stdout.as_slice(), output.stderr.as_slice()].concat();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        assert!(
            !text.contains(['\u{1b}', '\r', '\u{7}', '\u{202e}']),
            "raw control characters in {text:?}"
        );
        text
    };
    let mut hostile = profile();
    hostile["experience"][0]["id"] = format!("job{ESC}").into();
    hostile["profile"] =
        format!("Backend engineer{ESC}, see https://tool.example.test/{ESC}x").into();
    hostile["skills"] =
        json!([{"category": format!("Tools{ESC}"), "items": [format!("Rust{ESC}")]}]);
    hostile["contact"]["location"] = format!("Hamburg{ESC}").into();
    agent.ok("upsert_cv_profile", hostile).await;

    let refused = cv_facts(&node.dir, &["lock"]);
    assert!(!refused.status.success());
    let shown = raw(&refused);
    assert!(
        shown.contains("job\\u{1b}[2K"),
        "still says what is there: {shown}"
    );
    assert!(shown.contains("added to allowed tokens"), "{shown}");
    let hash = shown
        .split("--confirm ")
        .nth(1)
        .unwrap()
        .split('`')
        .next()
        .unwrap();
    let locked = cv_facts(&node.dir, &["lock", "--confirm", hash]);
    assert!(locked.status.success(), "{locked:?}");

    // That profile's bidi override can't render, so the rest starts from a clean one.
    assert!(cv_facts(&node.dir, &["unlock"]).status.success());
    agent.ok("upsert_cv_profile", profile()).await;
    raw(&cv_facts(&node.dir, &["lock"]));
    lock_confirmed(&node.dir);

    let source = agent.ok("get_cv_source", json!({})).await;
    let source = source["source"].as_str().unwrap().to_owned();
    // The bidi override is a hard refusal in a CV's text, so this carries only the terminal's controls.
    let drafted = format!("{source}\n#\"One\u{1b}[2K\u{7}Two\"\n");
    let patched = agent
        .ok(
            "patch_cv",
            json!({"expectedRevision": 0, "summary": format!("Typo{ESC}"),
                "edits": [{"oldText": source, "newText": drafted}]}),
        )
        .await;
    assert_eq!(patched["state"], "draft", "{patched}");
    assert!(patched["reviewItems"].is_array());
    let read = cv_facts(
        &node.dir,
        &["approve", "--variant", "default", "--revision", "1"],
    );
    assert!(!read.status.success());
    raw(&read);
    let done = cv_facts(
        &node.dir,
        &[
            "approve",
            "--variant",
            "default",
            "--revision",
            "1",
            "--confirm",
        ],
    );
    assert!(done.status.success(), "{done:?}");
    raw(&done);

    // A change only in looks has only the source to show, and this prints that too.
    let (from, to) = ("#header()", "#strong[#header()]");
    let looks = agent
        .ok(
            "patch_cv",
            json!({"expectedRevision": 1, "summary": "tidy",
                "edits": [{"oldText": from, "newText": to}]}),
        )
        .await;
    assert_eq!(looks["state"], "draft", "{looks}");
    let read = cv_facts(
        &node.dir,
        &["approve", "--variant", "default", "--revision", "2"],
    );
    let shown = raw(&read);
    assert!(shown.contains("The wording is unchanged"), "{shown}");
    assert!(shown.contains("+#strong[#header()]"), "{shown}");
}

/// The owner's decisions on corrections and on marks reach the terminal too, in the same words.
#[tokio::test(flavor = "multi_thread")]
async fn the_terminal_says_why_a_cv_is_whole_replaced_or_marked() {
    let root = tempfile::tempdir().unwrap();
    let (node, token) = Node::create(root.path(), &[]).await;
    let agent = Agent::new(node.addr, &token);
    agent.ok("upsert_cv_profile", profile()).await;
    lock_confirmed(&node.dir);
    let read = |variant: &str, revision: &str, confirm: bool| {
        let mut args = vec!["approve", "--variant", variant, "--revision", revision];
        if confirm {
            args.push("--confirm");
        }
        let output = cv_facts(&node.dir, &args);
        let bytes = [output.stdout.as_slice(), output.stderr.as_slice()].concat();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    let new = |name: &str, edits: Value| {
        agent.ok(
            "patch_cv",
            json!({"variant": name, "createFrom": "default", "expectedRevision": 1,
                "summary": "new", "edits": edits}),
        )
    };
    let drafted = agent
        .ok(
            "patch_cv",
            json!({"expectedRevision": 0, "summary": "start",
                "edits": [{"oldText": "#header()", "newText": "#header()\n"}]}),
        )
        .await;
    assert_eq!(drafted["state"], "draft", "{drafted}");
    let shown = read("default", "1", false);
    assert!(shown.contains("Nothing this variant holds"), "{shown}");
    read("default", "1", true);

    let (old, fixed) = ("3 depots", "2 depots");
    let tailored = new("tailored", json!([{"oldText": old, "newText": fixed}])).await;
    assert_eq!(tailored["state"], "draft", "{tailored}");
    let shown = read("tailored", "1", false);
    assert!(shown.contains("this variant started from"), "{shown}");
    assert!(!shown.contains("A text you approved"), "{shown}");
    read("tailored", "1", true);

    let copied = new("copied", json!([])).await;
    assert_eq!(copied["state"], "draft", "{copied}");
    let shown = read("copied", "1", false);
    assert!(
        shown.contains("A text you approved has since been changed"),
        "{shown}"
    );
    assert!(!shown.contains("The wording is unchanged"), "{shown}");

    let marked = agent
        .ok(
            "patch_cv",
            json!({"expectedRevision": 1, "summary": "tidy",
                "edits": [{"oldText": "#header()", "newText": "#header()\nGrade #text(size: 11pt, kerning: false)[F]#box(width: 0pt, height: 0pt, place(top + left, dx: -5pt, dy: -0.6pt, rect(width: 5pt, height: 0.6pt, fill: black))) here.\n"}]}),
        )
        .await;
    assert_eq!(marked["state"], "draft", "{marked}");
    let shown = read("default", "2", false);
    assert!(
        shown.contains("The draft draws marks on its text"),
        "{shown}"
    );
}

/// What a terminal of this size still shows at the end of `output`.
fn last_screen(output: &Output, rows: usize, columns: usize) -> String {
    let bytes = [output.stdout.as_slice(), output.stderr.as_slice()].concat();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let mut screen: Vec<String> = Vec::new();
    for line in text.split('\n') {
        let chars: Vec<char> = line.chars().collect();
        if chars.is_empty() {
            screen.push(String::new());
        }
        for row in chars.chunks(columns) {
            screen.push(row.iter().collect());
        }
    }
    screen[screen.len().saturating_sub(rows)..].join("\n")
}

/// Line breaks and long values in what an agent wrote can't scroll the facts off the owner's screen,
/// and a value too long to print says so.
#[tokio::test(flavor = "multi_thread")]
async fn nothing_an_agent_wrote_scrolls_the_lock_list_off_the_terminal() {
    let breaks = "\n".repeat(70);
    let long = "y".repeat(100_000);
    for (name, id, company) in [
        ("line breaks", format!("{breaks}y"), "Second".to_owned()),
        (
            "carriage returns",
            format!("{}y", "\r".repeat(70)),
            "Second".to_owned(),
        ),
        (
            "other separators",
            format!("{}y", "\u{2028}\u{2029}\u{85}\u{b}\u{c}".repeat(14)),
            "Second".to_owned(),
        ),
        ("a long id", long.clone(), "Second".to_owned()),
        ("a long value", "second".to_owned(), long.clone()),
    ] {
        let root = tempfile::tempdir().unwrap();
        let (node, token) = Node::create(root.path(), &[]).await;
        let agent = Agent::new(node.addr, &token);
        let mut hostile = profile();
        hostile["experience"] = json!([
            {"id": "fake", "company": "Fake Inc", "title": "CEO", "date": "2020", "location": "Bonn",
             "tier": 1, "bullets": []},
            {"id": id, "company": company, "title": "Clerk", "date": "2021", "location": "Kiel",
             "tier": 1, "bullets": []},
        ]);
        agent.ok("upsert_cv_profile", hostile).await;
        let refused = cv_facts(&node.dir, &["lock"]);
        assert!(!refused.status.success(), "{name}");
        let screen = last_screen(&refused, 50, 200);
        assert!(screen.contains("Fake Inc"), "{name}: {screen}");
        assert!(screen.contains("CEO"), "{name}: {screen}");
        if name.starts_with("a long") {
            assert!(screen.contains("more characters"), "{name}: {screen}");
            assert!(screen.contains("facts page"), "{name}: {screen}");
        } else {
            assert!(screen.contains("--confirm"), "{name}: {screen}");
        }
    }
}

/// Finding 12 of the fourth gate review: what the terminal cut short can't be confirmed from it.
#[tokio::test]
async fn a_lock_with_a_cut_value_cannot_be_confirmed_from_the_terminal() {
    let root = tempfile::tempdir().unwrap();
    let (node, token) = Node::create(root.path(), &[]).await;
    let agent = Agent::new(node.addr, &token);
    let mut hostile = profile();
    let tail = " and I was its CEO";
    hostile["experience"][0]["company"] = json!(format!("Brightpath {}{tail}", "x".repeat(1200)));
    agent.ok("upsert_cv_profile", hostile).await;
    let listed = cv_facts(&node.dir, &["lock"]);
    assert!(!listed.status.success());
    let told = format!(
        "{}{}",
        String::from_utf8_lossy(&listed.stdout),
        String::from_utf8_lossy(&listed.stderr)
    );
    assert!(!told.contains("its CEO"), "{told}");
    assert!(told.contains("more characters"), "{told}");
    assert!(told.contains("facts page"), "{told}");
    assert!(!told.contains("--confirm"), "no hash to run: {told}");
    // Nor does any hash get past it.
    let hash_of = cv_facts(&node.dir, &["lock", "--confirm", "0000"]);
    assert!(!hash_of.status.success());
    let state = cv_facts(&node.dir, &["show"]);
    let shown = String::from_utf8_lossy(&state.stdout);
    assert!(shown.contains("\"lockedAt\": null"), "{shown}");
}

/// An id long enough to cut where it is named, with every value short.
#[tokio::test]
async fn a_lock_naming_a_cut_id_cannot_be_confirmed_from_the_terminal() {
    let root = tempfile::tempdir().unwrap();
    let (node, token) = Node::create(root.path(), &[]).await;
    let agent = Agent::new(node.addr, &token);
    let mut hostile = profile();
    hostile["experience"][0]["id"] = json!("y".repeat(500));
    agent.ok("upsert_cv_profile", hostile).await;
    let listed = cv_facts(&node.dir, &["lock"]);
    assert!(!listed.status.success());
    let told = String::from_utf8_lossy(&listed.stderr);
    assert!(told.contains("facts page"), "{told}");
    assert!(!told.contains("--confirm"), "{told}");
}

/// The same for a draft: a cut line can't be approved from the terminal.
#[tokio::test]
async fn an_approval_with_a_cut_line_cannot_be_confirmed_from_the_terminal() {
    let root = tempfile::tempdir().unwrap();
    let (node, token) = Node::create(root.path(), &[]).await;
    let agent = Agent::new(node.addr, &token);
    agent.ok("upsert_cv_profile", profile()).await;
    lock_confirmed(&node.dir);
    let long = "word ".repeat(300);
    let source = format!("{START}#header()\n{long}and I was its CEO.\n");
    let head = agent
        .ok("get_cv_source", json!({"variant": "default"}))
        .await;
    let reply = agent
        .ok(
            "patch_cv",
            json!({"expectedRevision": head["revision"], "summary": "Long",
                "edits": [{"oldText": head["source"], "newText": source}]}),
        )
        .await;
    let revision = reply["revision"].to_string();
    let tried = cv_facts(
        &node.dir,
        &[
            "approve",
            "--variant",
            "default",
            "--revision",
            &revision,
            "--confirm",
        ],
    );
    assert!(!tried.status.success());
    let told = String::from_utf8_lossy(&tried.stderr);
    assert!(told.contains("dashboard"), "{told}");
    let after = agent
        .ok("get_cv_source", json!({"variant": "default"}))
        .await;
    assert_eq!(after["state"], "draft");
}

/// A worker that does what the real one does, except that it can't draw: what a source does when it
/// fits in memory until its pages are drawn. Whatever asks it to draw is told so.
fn worker_that_cannot_draw(root: &Path) -> nexus_mcp::Renderer {
    use std::os::unix::fs::PermissionsExt;
    let real = env!("CARGO_BIN_EXE_nexus");
    let worker = root.join("worker");
    std::fs::write(
        &worker,
        format!(
            "#!/bin/sh\ninput=$(/usr/bin/cat)\ncase \"$input\" in\n*'\"previewPages\":0,'*) \
             printf '%s' \"$input\" | exec {real} __render ;;\n*) exit 9 ;;\nesac\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o755)).unwrap();
    nexus_mcp::Renderer::Subprocess {
        exe: worker,
        rss_limit_bytes: 512 * 1024 * 1024,
        timeout: Duration::from_secs(30),
    }
}

/// The owner's two steps draw what the preview draws: a source that can be stored and can't be drawn
/// has nothing to read, and so nothing to approve.
#[tokio::test(flavor = "multi_thread")]
async fn a_source_that_cannot_be_drawn_cannot_be_reviewed_or_approved() {
    let root = tempfile::tempdir().unwrap();
    let (node, token) = Node::create(root.path(), &[]).await;
    let agent = Agent::new(node.addr, &token);
    agent.ok("upsert_cv_profile", profile()).await;
    lock_confirmed(&node.dir);
    let source = agent.ok("get_cv_source", json!({})).await;
    let source = source["source"].as_str().unwrap().to_owned();
    let drafted = source.replace("Cut invoice errors", "Cut most invoice errors");
    let patched = agent
        .ok(
            "patch_cv",
            json!({"expectedRevision": 0, "summary": "Edit",
                "edits": [{"oldText": source, "newText": drafted}]}),
        )
        .await;
    assert_eq!(patched["state"], "draft", "{patched}");

    let db = nexus_core::Db::open(&node.dir).await.unwrap();
    let state = std::sync::Arc::new(nexus_mcp::McpState::new(
        db.clone(),
        "owner".to_owned(),
        "owner@example.test".to_owned(),
        nexus_mcp::Renderer::Subprocess {
            exe: env!("CARGO_BIN_EXE_nexus").into(),
            rss_limit_bytes: 512 * 1024 * 1024,
            timeout: Duration::from_secs(30),
        },
    ));
    let review = nexus_mcp::owner::review(state, "default").await.unwrap();

    let limited = nexus_mcp::McpState::new(
        db,
        "owner".to_owned(),
        "owner@example.test".to_owned(),
        worker_that_cannot_draw(root.path()),
    );
    let limited = std::sync::Arc::new(limited);
    let refused = nexus_mcp::owner::review(limited.clone(), "default").await;
    assert!(refused.is_err(), "a review with no picture: {refused:?}");
    let tap = nexus_mcp::owner::approve(
        limited,
        "default",
        review.revision,
        &review.source_sha256,
        &review.facts_sha256,
        "dashboard",
    )
    .await;
    assert_eq!(tap.is_error, Some(true), "approved unseen: {tap:?}");
    assert_eq!(node.count("SELECT count(*) FROM cv_approvals").await, 0);
}
