//! Edits a person really dictates must pass: a false positive makes dictation useless.

mod common;

use common::{content_de, content_en, facts_de, facts_en, render, render_and_check};
use nexus_tailor::{
    CvContent, Facts, MAX_PAGES, RenderError, RenderRequest, RenderedText, Renderer, Rule, check,
    default_source, source_from_profile,
};

const START: &str = "#import \"/nexus.typ\": *\n#show: cv\n#header()\n";

fn clean(report: &nexus_tailor::Report, source: &str) {
    assert_eq!(report.violations, [], "{source}");
}

#[tokio::test]
async fn generated_cvs_pass_in_both_languages() {
    for (facts, content) in [(facts_en(), content_en()), (facts_de(), content_de())] {
        let source = source_from_profile(&content);
        let report = render_and_check(&source, &facts).await;
        clean(&report, &source);
    }
}

#[tokio::test]
async fn default_source_compiles_and_passes() {
    for facts in [facts_en(), facts_de(), Facts::default()] {
        let source = default_source(&facts);
        let report = render_and_check(&source, &facts).await;
        clean(&report, &source);
    }
}

#[tokio::test]
async fn a_generated_cv_is_one_page_with_a_pdf_and_a_png() {
    let facts = facts_en();
    let rendered = Renderer::InProcess
        .render(RenderRequest {
            source: source_from_profile(&content_en()),
            facts: facts.clone(),
            pdf: true,
            preview_pages: 1,
        })
        .await
        .unwrap();
    assert_eq!(rendered.page_count, 1);
    assert!(rendered.pdf.unwrap().starts_with(b"%PDF-"));
    assert!(rendered.previews[0].starts_with(b"\x89PNG"));
    assert_eq!(rendered.warnings, []);
}

#[tokio::test]
async fn rendering_is_deterministic() {
    let request = RenderRequest {
        source: source_from_profile(&content_de()),
        facts: facts_de(),
        pdf: true,
        preview_pages: 0,
    };
    let first = Renderer::InProcess.render(request.clone()).await.unwrap();
    let second = Renderer::InProcess.render(request).await.unwrap();
    assert_eq!(first.pdf, second.pdf);
}

#[tokio::test]
async fn markup_in_profile_text_renders_literally() {
    let mut content = CvContent {
        summary: "C# & *Rust_* <tags> [x] $5 `code` @me #hash -- \\ // /* 1. = + http://a.b"
            .to_owned(),
        ..CvContent::default()
    };
    content.summary = content.summary.replace("$5", "$");
    let source = source_from_profile(&content);
    let rendered = render(&source, &Facts::default()).await.unwrap();
    let text = rendered.text.lines.join(" ");
    assert!(
        text.contains("C# & *Rust_* <tags> [x] $ `code` @me #hash -- \\ // /* 1. = + http://a.b"),
        "{text}"
    );
}

#[tokio::test]
async fn valid_edits_pass() {
    let bodies = [
        // Changed wording, in both directions of dictation.
        "#employment(\"brightpath\")[- Rebuilt the dispatch service around plain events.]",
        // Reordered bullets and entries.
        "#employment(\"quillon\")[- Second.\n- First.]\n#employment(\"brightpath\")[- Only.]",
        // Changed emphasis.
        "#employment(\"brightpath\")[- *Rebuilt* the _dispatch_ service.]",
        "#show strong: upper\n#employment(\"brightpath\")[- Work.]",
        "#upper[#employment(\"brightpath\")[- Work.]]",
        "#text(fill: blue)[#employment(\"brightpath\")[- Work.]]",
        // A metric through #metric, and its tokens by hand.
        "#employment(\"brightpath\")[- #metric(\"latency\").]",
        "#employment(\"brightpath\")[- We cut p95 latency by 40%, over 1.200 services.]",
        // A year from a fact, in prose.
        "#employment(\"brightpath\")[- Shipped the rewrite in 2022.]",
        "Moved to Hamburg in March 2019.",
        "Available from 03/2019 as stated.",
        // An allowed token.
        "#employment(\"brightpath\")[- On call 24/7.]",
        // Compact entries in any order.
        "#employment(\"tessel\")\n#employment(\"brightpath\")\n#employment(\"quillon\")",
        // Everyday words near the rule lists.
        "Led a small team, managed the roadmap and mastered the master data migration.",
        "A cloud-native, third-degree view; the data lead the way.",
        "Worked on two products with three colleagues over many years.",
        "Graduated work into production; a mid-level summary of native ideas is not a claim here.",
        // Facts' own words.
        "Bachelor thesis on route planning. German is native, English C1.",
        // Vocabulary, whole.
        "Worked as Scrum Master on AWS S3 and OAuth2.",
        // Punctuation that is not a number.
        "Quality, speed & care: “tested” – always… (really!) «ja» 'quoted'.",
        // Accented names and other scripts.
        "Worked with José, Zoë, Søren, François, 李雷 and محمد on Überführung.",
    ];
    let facts = facts_en();
    for body in bodies {
        let source = format!("{START}{body}\n");
        let report = render_and_check(&source, &facts).await;
        clean(&report, &source);
    }
}

#[tokio::test]
async fn valid_german_edits_pass() {
    let bodies = [
        "#employment(\"rheinwerk\")[- Die Wartungsplanung für Großkunden übernommen.]",
        "#employment(\"rheinwerk\")[- #metric(\"kosten\").]",
        "#employment(\"rheinwerk\")[- Wartungskosten um 15 Prozent gesenkt.]",
        "#employment(\"rheinwerk\")[- Seit März 2018 verantwortlich für den Betrieb; ab 2023 beratend.]",
        "Über eine Woche hinweg in einem Jahr: Größe, Maß, Straße, Äpfel, Übung, Öl.",
        "Die Lehrer und Leitern im Halbleiter-Bereich begleiten; Studierende betreuen.",
        "Muttersprache Deutsch, Englisch verhandlungssicher, Diplom-Informatiker (FH).",
    ];
    let facts = facts_de();
    for body in bodies {
        let source =
            format!("#import \"/nexus.typ\": *\n#show: cv.with(lang: \"de\")\n#header()\n{body}\n");
        let report = render_and_check(&source, &facts).await;
        clean(&report, &source);
    }
}

#[tokio::test]
async fn a_long_word_hyphenated_at_a_line_end_is_read_whole() {
    let source = format!(
        "{START}#set text(hyphenate: true)\n#box(width: 1.4cm)[Rock#\"\"star Internationalization]\n"
    );
    let rendered = render(&source, &Facts::default()).await.unwrap();
    assert!(rendered.text.lines.len() > 2, "{:?}", rendered.text.lines);
    let facts = Facts {
        banned_phrases: vec!["internationalization".to_owned()],
        ..Facts::default()
    };
    let report = check(&rendered.text, &facts, None);
    assert_eq!(report.violations.len(), 1, "{:?}", rendered.text.lines);
    assert_eq!(report.violations[0].rule, Rule::BannedPhrases);
}

fn text(lines: &[&str]) -> RenderedText {
    RenderedText {
        lines: lines.iter().map(|l| (*l).to_owned()).collect(),
        ..RenderedText::default()
    }
}

#[test]
fn review_tokens_are_new_capitalized_words() {
    let facts = facts_en();
    let previous = text(&["Worked at Globex on Kubernetes."]);
    let current = text(&["Worked at Initech and Globex on Kubernetes, Hamburg. Initech again."]);
    let report = check(&current, &facts, Some(&previous));
    assert_eq!(report.violations, []);
    assert_eq!(report.review_tokens, ["Initech"]);
    let report = check(&current, &facts, None);
    assert!(report.review_tokens.contains(&"Globex".to_owned()));
    assert!(!report.review_tokens.contains(&"Kubernetes".to_owned()));
    assert!(!report.review_tokens.contains(&"Hamburg".to_owned()));
}

#[test]
fn any_script_is_safe_and_the_check_is_fast() {
    let facts = facts_en();
    let scripts = [
        "Zoë Müller-Lüdenscheidt",
        "日本語のテキスト 二〇一五",
        "مرحبا بالعالم ٢٠١٥",
        "שלום עולם",
        "한국어",
        "🦀 👩‍💻",
        "\u{301}\u{ad}\u{200b}",
        "ß",
        "ﬁ",
        "-",
        ".",
    ];
    for script in scripts {
        check(&text(&[script]), &facts, None);
    }
    let line =
        "Zoë built 日本語 and مرحبا, the dispatch service for 200 Ärzte in Köln. ".repeat(40);
    let lines: Vec<&str> = std::iter::repeat_n(line.as_str(), 400).collect();
    let started = std::time::Instant::now();
    let report = check(&text(&lines), &facts, None);
    assert_eq!(report.violations.len(), 1);
    assert!(started.elapsed().as_secs() < 30, "{:?}", started.elapsed());
}

#[tokio::test]
async fn limits_without_a_child() {
    let huge = "x".repeat(nexus_tailor::MAX_SOURCE_BYTES + 1);
    assert_eq!(
        render(&huge, &Facts::default()).await,
        Err(RenderError::SourceTooLarge)
    );
    let pages = "#pagebreak()\n".repeat(MAX_PAGES as usize);
    assert_eq!(
        render(&pages, &Facts::default()).await,
        Err(RenderError::TooManyPages)
    );
}
