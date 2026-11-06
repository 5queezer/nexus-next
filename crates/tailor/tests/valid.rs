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
async fn valid_cross_line_layouts_pass() {
    let bodies = [
        // The date a little below, as long as it comes before the next company.
        "Brightpath Logistics · Software Engineer\n\n03/2019 – 12/2022",
        "Brightpath Logistics · Software Engineer\n\nHamburg\n\n03/2019 – 12/2022",
        // Another company named in prose carries no date of its own there.
        "#employment(\"quillon\")[- Moved customers over to Brightpath Logistics.]\n#employment(\"brightpath\")[- Work.]",
        "= Summary\nFormerly at Brightpath Logistics and Quillon Systems.\n= Experience\n#employment(\"brightpath\")[- Work.]\n#employment(\"quillon\")[- Work.]",
        // A bare year of another entry in a bullet stays allowed.
        "#employment(\"brightpath\")[- Kept the 2016 tooling running into 2022.\n- Replaced what was left from 2014.]",
        // Sections after the last entry.
        "#employment(\"brightpath\")[- Work.]\n= Projects\n#project(\"Pallet\", stack: \"Rust\")[A tracker from 2016.]\n= Languages\n#languages()",
    ];
    let facts = facts_en();
    for body in bodies {
        let source = format!("{START}{body}\n");
        let report = render_and_check(&source, &facts).await;
        clean(&report, &source);
    }
}

#[tokio::test]
async fn two_roles_at_one_company_and_a_one_year_entry_pass() {
    let mut facts = facts_en();
    facts.employment[1].company = "Brightpath Logistics".to_owned();
    facts.employment[2].date = "2015".to_owned();
    let source = format!(
        "{START}#employment(\"brightpath\")[- Shipped in 2015.]\n#employment(\"quillon\")[- Work.]\n#employment(\"tessel\")\n"
    );
    let report = render_and_check(&source, &facts).await;
    clean(&report, &source);
}

#[tokio::test]
async fn honest_scripts_initials_and_roman_numerals_pass() {
    let bodies = [
        // Umlauts and other accented Latin.
        "Überführung für Müller, Straße, Ölpreis, Äpfel, Zoë, François, Søren, Łukasz.",
        // Wholly Cyrillic and Greek words, also where they could be Latin letters.
        "Мария Иванова из Москвы, сто человек, ООО, Ελλάδα, Αθήνα, ΑΒΓ.",
        "A Москва-Berlin route, an α-Test and a β-Version.",
        // Greek letters that are no lookalikes, inside technical words.
        "Latency in μs, 5 kΩ never mind, the λ-calculus.",
        // Initials and short letter runs.
        "With J. R. R. Tolkien and A. B. Example, an A B test and a C-level pitch.",
        "Grid x y z, plans A B, u. a. in Köln, z. B. hier.",
        // ASCII Roman numerals in words are ordinary text.
        "CI and CD, Phase IV, Louis XIV, Type V, Mix, DC, Section III: I said so.",
        "IV drips, a CI job and a V model over many days.",
    ];
    let facts = Facts {
        allowed_tokens: vec!["5".to_owned()],
        ..facts_en()
    };
    for body in bodies {
        let source = format!("{START}{body}\n");
        let report = render_and_check(&source, &facts).await;
        clean(&report, &source);
    }
}

#[tokio::test]
async fn owner_lists_free_other_numerals_and_a_fact_year_in_cjk() {
    let facts = Facts {
        allowed_tokens: vec!["Ⅷ".to_owned(), "VIII years".to_owned()],
        ..facts_en()
    };
    for body in ["Since 二〇一五.", "Ⅷ of them.", "VIII years of it."] {
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

#[tokio::test]
async fn owner_lists_free_exactly_what_they_name() {
    let mut facts = facts_en();
    let source = format!("{START}With eight years of Rust as Staff Engineer on EC2 for €900k.\n");
    let rules = |report: &nexus_tailor::Report| {
        let mut rules: Vec<Rule> = report.violations.iter().map(|v| v.rule).collect();
        rules.dedup();
        rules
    };
    let report = render_and_check(&source, &facts).await;
    assert_eq!(
        rules(&report),
        [Rule::Numbers, Rule::NumberWords, Rule::ClaimWords]
    );
    facts
        .allowed_tokens
        .extend(["eight years", "€900k"].map(str::to_owned));
    facts
        .vocabulary
        .extend(["EC2", "Staff Engineer"].map(str::to_owned));
    let report = render_and_check(&source, &facts).await;
    assert_eq!(report.violations, []);
    // The whole token, not its parts.
    let source = format!("{START}900 years in EC2. A Staff of two.\n");
    let report = render_and_check(&source, &facts).await;
    assert_eq!(rules(&report), [Rule::Numbers]);
}

#[tokio::test]
async fn injected_markup_in_profile_text_stays_text() {
    let content = CvContent {
        summary: "#hide[x] #image(\"/etc/passwd\") ] #let a = 1 [".to_owned(),
        employment: vec![nexus_tailor::CvEmployment {
            id: "no\"pe".to_owned(),
            tier: 1,
            bullets: vec!["x".to_owned()],
        }],
        ..CvContent::default()
    };
    let source = source_from_profile(&content);
    let Err(RenderError::Diagnostics(diagnostics)) = render(&source, &Facts::default()).await
    else {
        panic!("an id not in the facts must not compile");
    };
    assert!(
        diagnostics[0].message.contains("no employment with id"),
        "{diagnostics:?}"
    );
    let without_entry = CvContent {
        employment: Vec::new(),
        ..content
    };
    let rendered = render(&source_from_profile(&without_entry), &Facts::default())
        .await
        .unwrap();
    assert!(
        rendered.text.lines.join(" ").contains("#hide[x] #image"),
        "{:?}",
        rendered.text
    );
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

/// Right-to-left text needs no control characters: the Unicode algorithm orders it.
#[tokio::test]
async fn natural_right_to_left_text_passes() {
    let facts = facts_en();
    for body in [
        "Spricht auch שלום עולם.",
        "#set text(lang: \"he\")\nשלום עולם, Kubernetes.",
        "#text(dir: rtl)[שלום עולם]",
        "#set text(dir: rtl)\nשלום עולם",
        "שלום 2019 עולם, 2019.",
        "#set text(lang: \"ar\")\nمرحبا 2019 بالعالم.",
        "مرحبا ٢٠١٩",
    ] {
        let source = format!("{START}{body}\n");
        let report = render_and_check(&source, &facts).await;
        clean(&report, &source);
    }
}

#[tokio::test]
async fn translation_and_a_uniform_scale_stay_readable() {
    let facts = facts_en();
    for body in [
        "#scale(150%)[Work]",
        "#scale(50%, reflow: true)[#text(size: 21pt)[Work]]",
        "#rotate(0deg)[Work]",
        "#rotate(360deg)[Work]",
        "#move(dx: 1cm, dy: 1pt)[Work]",
        "#scale(x: 100%, y: 100%)[Work]",
        "#block(clip: true)[Work]",
        "#box(clip: true, height: 1.5em)[Gap]",
        "#place(bottom + right)[Work]",
        "#super[Work] and #sub[work]",
        "#set par(justify: true)\n#box(width: 8cm)[Mentored two colleagues through their first on-call rotation, and they stayed on the team for it.]",
        "*Bold* and _italic_ and #text(fill: blue)[blue] and #text(size: 12pt)[large] words.",
        "#text(tracking: 0.1em)[Spaced out] and #text(tracking: -0.02em)[slightly tight].",
        "Water#h(-0.1em)fall, x#super[a]#sub[b], and #underline[underlined] text.",
        "Left #h(1fr) Right",
        "#grid(columns: (1fr, 1fr), [Left cell], [Right cell])",
    ] {
        let source = format!("{START}{body}\n");
        let report = render_and_check(&source, &facts).await;
        clean(&report, &source);
    }
}

/// Finding 11: what a CV says about its owner's contact details and projects must keep working.
#[tokio::test]
async fn honest_links_and_skills_pass() {
    let mut facts = facts_en();
    facts
        .allowed_tokens
        .push("docs.example.org/guide".to_owned());
    facts.vocabulary.push("status.example.com".to_owned());
    for body in [
        "#contact()",
        "#project(\"Pallet\", stack: \"Rust\", url: \"example.org/pallet\")[A tracker.]",
        "#project(\"Pallet\", url: \"https://github.com/someone/pallet\")[A tracker.]",
        "#link(\"https://example.org/pallet\")[Pallet]",
        "#link(\"https://github.com/maraexample\")[github.com/maraexample]",
        "#link(\"https://www.github.com/maraexample/\")[github.com/maraexample]",
        "#link(\"https://github.com/maraexample/pallet\")[github.com/maraexample/pallet]",
        "#link(\"mailto:MARA@example.org\")[mara\\@example.org]",
        "#link(\"tel:+491705550137\")[+49 170 5550137]",
        "Code at github.com/maraexample/pallet and https://github.com/maraexample.",
        "See docs.example.org/guide and https://status.example.com.",
        "#skills(\"Web\")[Node.js, Next.js/React, ASP.NET, ADO.NET/EF, socket.io, Vue.js, net/http]",
        "#skills(\"Other\")[e.g. this, i.e. that, C\\#/.NET, CI/CD, I/O, a.m., z.B. so]",
        "Rust \\@ home, on call via pager.",
    ] {
        let source = format!("{START}{body}\n");
        let report = render_and_check(&source, &facts).await;
        clean(&report, &source);
    }
}

#[tokio::test]
async fn words_like_digits_pass_when_they_are_words() {
    let mut facts = facts_en();
    facts.vocabulary.push("Ill".to_owned());
    for body in [
        "Tuned IO on the routing nodes.",
        "Improved I/O by 40%.",
        "Improved IO latency.",
        "Ran CI for days.",
        "IO, 40% of it.",
        "Oil prices.",
        "I worked for days.",
    ] {
        let source = format!("{START}{body}\n");
        let report = render_and_check(&source, &facts).await;
        clean(&report, &source);
    }
}

/// Finding 15: facts that are valid must render, from the generated source, without an edit.
#[tokio::test]
async fn valid_facts_with_odd_shapes_render() {
    let mut no_title = facts_en();
    no_title.employment[0].title = String::new();
    let mut no_degree = facts_en();
    no_degree.education[0].qualification = String::new();
    let mut long_title = facts_en();
    long_title.employment[0].title =
        "Software Engineer, Cloud Infrastructure and Platform Reliability".to_owned();
    let mut longer_title = facts_en();
    longer_title.employment[0].title =
        "Software Engineer, Cloud Infrastructure and Platform Reliability, Developer Experience and Internal Tooling for the Whole Company Across All Regions".to_owned();
    for facts in [no_title, no_degree, long_title, longer_title] {
        let source = default_source(&facts);
        let report = render_and_check(&source, &facts).await;
        clean(&report, &source);
        let full = source_from_profile(&content_en());
        let report = render_and_check(&full, &facts).await;
        clean(&report, &full);
    }
    let mut no_title = facts_en();
    no_title.employment[0].title = String::new();
    let rendered = render(&default_source(&no_title), &no_title).await.unwrap();
    assert!(
        rendered
            .text
            .lines
            .iter()
            .any(|line| line.starts_with("Brightpath Logistics · Hamburg 03/2019")),
        "no dangling separator: {:?}",
        rendered.text.lines
    );
}
