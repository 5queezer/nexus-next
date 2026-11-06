//! The real `nexus __render` child, under attack. The parent must survive everything unchanged.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nexus_tailor::{
    Contact, Employment, Facts, RenderError, RenderRequest, Renderer, check, default_source,
};

/// `children` sees this whole process, so the tests here take turns.
static ONE_TEST: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const RSS_LIMIT: u64 = nexus_tailor::DEFAULT_RSS_LIMIT_BYTES;
const TIMEOUT: Duration = Duration::from_secs(10);
/// Runs for days in constant memory.
const SLOW: &str = "#for a in range(9999) { for b in range(9999) { for c in range(9999) { } } }\n";

fn renderer() -> Renderer {
    Renderer::Subprocess {
        exe: PathBuf::from(env!("CARGO_BIN_EXE_nexus")),
        rss_limit_bytes: RSS_LIMIT,
        timeout: TIMEOUT,
    }
}

fn facts() -> Facts {
    Facts {
        name: "Mara Example".to_owned(),
        contact: Contact {
            email: "mara@example.org".to_owned(),
            ..Contact::default()
        },
        employment: vec![Employment {
            id: "brightpath".to_owned(),
            company: "Brightpath Logistics".to_owned(),
            title: "Software Engineer".to_owned(),
            date: "03/2019 – 12/2022".to_owned(),
            location: "Hamburg".to_owned(),
        }],
        ..Facts::default()
    }
}

fn request(source: String) -> RenderRequest {
    RenderRequest {
        source,
        facts: facts(),
        pdf: true,
        preview_pages: 1,
        stamp: false,
    }
}

fn own_rss_kib() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = status
        .lines()
        .find(|line| line.starts_with("VmRSS:"))
        .unwrap();
    line.split_ascii_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap()
}

/// Every child of this process, by thread. Zombies stay listed until they are reaped.
fn children() -> Vec<String> {
    let mut found = Vec::new();
    for task in std::fs::read_dir("/proc/self/task").unwrap().flatten() {
        if let Ok(text) = std::fs::read_to_string(Path::new(&task.path()).join("children")) {
            found.extend(text.split_ascii_whitespace().map(str::to_owned));
        }
    }
    found
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cv_renders_in_the_child() {
    let _turn = ONE_TEST.lock().await;
    let facts = facts();
    let rendered = renderer()
        .render(request(default_source(&facts)))
        .await
        .unwrap();
    assert_eq!(rendered.page_count, 1);
    assert!(rendered.pdf.unwrap().starts_with(b"%PDF-"));
    assert!(rendered.previews[0].starts_with(b"\x89PNG"));
    assert_eq!(check(&rendered.text, &facts, None).violations, []);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_compile_error_is_data() {
    let _turn = ONE_TEST.lock().await;
    let result = renderer().render(request("#nope()\n".to_owned())).await;
    let Err(RenderError::Diagnostics(diagnostics)) = result else {
        panic!("{result:?}");
    };
    assert_eq!(diagnostics[0].line, Some(1));
    assert_eq!(diagnostics[0].file.as_deref(), Some("/main.typ"));
}

/// Each ends with one of the listed errors, within the limits, with nothing left behind.
#[tokio::test(flavor = "multi_thread")]
async fn resource_attacks_end_in_the_child() {
    let _turn = ONE_TEST.lock().await;
    let nesting_bomb = format!("{}{}", "#[".repeat(40_000), "]".repeat(40_000));
    let deep_content = "#let x = [a]\n#for i in range(200000) { x = box(x) }\n#x\n".to_owned();
    let cases: Vec<(&str, String)> = vec![
        ("recursion", "#let f(x) = f(x + 1)\n#f(1)\n".to_owned()),
        ("mutual recursion", "#let f(g, x) = g(g, x) + g(g, x)\n#f(f, 1)\n".to_owned()),
        ("while", "#let i = 0\n#while i >= 0 { i += 1 }\n".to_owned()),
        ("for over a huge range", "#for i in range(9000000000) { }\n".to_owned()),
        ("huge range kept", "#let a = range(9000000000)\n#a.len()\n".to_owned()),
        ("string doubling", "#let s = \"x\"\n#for i in range(62) { s = s + s }\n#s.len()\n".to_owned()),
        ("array doubling", "#let a = (1,)\n#for i in range(62) { a = a + a }\n#a.len()\n".to_owned()),
        ("many pages", "#for i in range(100000) { pagebreak() }\n".to_owned()),
        ("nested loops", SLOW.to_owned()),
        ("show rule loop", "#show heading: it => heading(it.body)\n= a\n".to_owned()),
        ("huge page", "#set page(width: 100000cm, height: 100000cm)\n#rect(width: 100%, height: 100%, fill: gradient.linear(red, blue))\n".to_owned()),
        ("nesting bomb", nesting_bomb),
        ("deep content", deep_content),
    ];
    for (name, source) in cases {
        let started = Instant::now();
        let before = own_rss_kib();
        let result = renderer().render(request(source)).await;
        assert!(
            started.elapsed() < TIMEOUT + Duration::from_secs(5),
            "{name}: {:?}",
            started.elapsed()
        );
        // The huge page has no limit to hit: its preview is capped instead.
        if name != "huge page" {
            let Err(error) = result else {
                panic!("{name} rendered");
            };
            println!("{name}: {:?} in {:?}", short(&error), started.elapsed());
            assert!(
                matches!(
                    error,
                    RenderError::Diagnostics(_)
                        | RenderError::Timeout
                        | RenderError::MemoryExceeded
                        | RenderError::TooManyPages
                        | RenderError::Worker(_)
                ),
                "{name}: {error:?}"
            );
        }
        let grown = own_rss_kib().saturating_sub(before);
        assert!(grown < 32 * 1024, "{name}: the parent grew by {grown} KiB");
        assert_eq!(
            children(),
            Vec::<String>::new(),
            "{name}: a child was left behind"
        );
    }
}

fn short(error: &RenderError) -> String {
    match error {
        RenderError::Diagnostics(d) => format!("diagnostics: {}", d[0].message),
        other => other.to_string(),
    }
}

/// The parent still works after a killed child, and the one-at-a-time lock is free.
#[tokio::test(flavor = "multi_thread")]
async fn a_good_render_follows_a_killed_one() {
    let _turn = ONE_TEST.lock().await;
    let limited = Renderer::Subprocess {
        exe: PathBuf::from(env!("CARGO_BIN_EXE_nexus")),
        rss_limit_bytes: RSS_LIMIT,
        timeout: Duration::from_millis(500),
    };
    let result = limited.render(request(SLOW.to_owned())).await;
    assert_eq!(result, Err(RenderError::Timeout));
    let facts = facts();
    assert!(
        renderer()
            .render(request(default_source(&facts)))
            .await
            .is_ok()
    );
}
