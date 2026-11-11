//! Every template draws the same source, and each drawing must pass the check a CV passes.

mod common;

use common::{content_de, content_en, facts_de, facts_en, render};
use nexus_tailor::{
    CvContent, Facts, RenderError, RenderRequest, Renderer, TEMPLATES, check, pages_of,
    source_from_profile, template_of, with_pages, with_template,
};

/// A CV long enough for a second page.
fn long_en() -> CvContent {
    let mut content = content_en();
    for _ in 0..4 {
        let bullets = content.employment[0].bullets.clone();
        content.employment[0].bullets.extend(bullets);
    }
    content
}

fn cases() -> [(&'static str, Facts, CvContent); 3] {
    [
        ("en", facts_en(), content_en()),
        ("de", facts_de(), content_de()),
        ("long", facts_en(), long_en()),
    ]
}

/// With `NEXUS_TEMPLATE_SHOTS` set, the pictures land in `target/template-shots` to be looked at.
#[tokio::test]
async fn every_template_passes_in_both_languages_and_over_two_pages() {
    let shots = std::env::var_os("NEXUS_TEMPLATE_SHOTS").map(|_| {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/template-shots")
    });
    for template in TEMPLATES {
        for (case, facts, content) in cases() {
            let source = with_template(&source_from_profile(&content), template).unwrap();
            let rendered = Renderer::InProcess
                .render(RenderRequest {
                    source: source.clone(),
                    facts: facts.clone(),
                    pdf: false,
                    preview_pages: if shots.is_some() { 2 } else { 0 },
                    stamp: false,
                })
                .await
                .unwrap_or_else(|e| panic!("{template} {case} does not compile: {e:?}"));
            assert_eq!(rendered.warnings, [], "{template} {case}");
            assert_eq!(rendered.page_count > 1, case == "long", "{template} {case}");
            let report = check(&rendered.text, &facts, None, "");
            assert_eq!(
                report.violations,
                [],
                "{template} {case}\n{:#?}",
                rendered.text
            );
            if let Some(dir) = &shots {
                std::fs::create_dir_all(dir).unwrap();
                std::fs::write(
                    dir.join(format!("{template}-{case}.txt")),
                    rendered.text.lines.join("\n"),
                )
                .unwrap();
                for (page, png) in rendered.previews.iter().enumerate() {
                    std::fs::write(dir.join(format!("{template}-{case}-{}.png", page + 1)), png)
                        .unwrap();
                }
            }
        }
    }
}

/// A rule at the top of a source wraps the rest of it, which `sidebar` then can't take apart.
#[tokio::test]
async fn every_helper_draws_in_every_template_also_under_a_rule() {
    let body = "#header()\n#contact()\n= Skills\n#skills(\"Languages\")[Rust, Go]\n\
                #show strong: emph\n= Experience\n#employment(\"brightpath\")[- #metric(\"latency\").]\n\
                #employment(\"tessel\")\n#project(\"Pallet\", stack: \"Rust\", url: \"example.org/pallet\")[A tracker.]\n\
                = Education\n#education(\"hochschule\")\n= Languages\n#languages()\n";
    let facts = facts_en();
    for template in TEMPLATES {
        let source =
            format!("#import \"/nexus.typ\": *\n#show: cv.with(template: \"{template}\")\n{body}");
        let rendered = render(&source, &facts).await.unwrap();
        let report = check(&rendered.text, &facts, None, "");
        assert_eq!(report.violations, [], "{template}\n{:#?}", rendered.text);
        let text = rendered.text.lines.join("\n");
        for said in [
            "Example",
            "Rust, Go",
            "Brightpath Logistics",
            "Tessel Works",
            "Pallet",
            "Hochschule Nordkap",
            "English (C1)",
        ] {
            assert!(text.contains(said), "{template} lacks {said}\n{text}");
        }
        assert_eq!(
            text.matches("mara@example.org").count(),
            2,
            "{template}\n{text}"
        );
    }
}

#[tokio::test]
async fn a_source_naming_no_template_is_the_classic_one() {
    for (_, facts, content) in cases() {
        let plain = source_from_profile(&content);
        assert_eq!(template_of(&plain), Some("classic"));
        assert_eq!(with_template(&plain, "classic").unwrap(), plain);
        let named = plain.replacen("lang:", "template: \"classic\", lang:", 1);
        assert_ne!(named, plain);
        let pdf = |source: String| {
            let facts = facts.clone();
            async move {
                Renderer::InProcess
                    .render(RenderRequest {
                        source,
                        facts,
                        pdf: true,
                        preview_pages: 0,
                        stamp: false,
                    })
                    .await
                    .unwrap()
            }
        };
        let (plain, named) = (pdf(plain).await, pdf(named).await);
        assert_eq!(plain.text, named.text);
        assert_eq!(plain.pdf, named.pdf);
    }
}

#[tokio::test]
async fn an_unknown_template_is_a_compile_error_naming_the_known_ones() {
    let source = "#import \"/nexus.typ\": *\n#show: cv.with(template: \"fancy\")\n#header()\n";
    let Err(RenderError::Diagnostics(diagnostics)) = render(source, &facts_en()).await else {
        panic!("compiled");
    };
    let message = &diagnostics[0].message;
    assert!(message.contains("no template \"fancy\""), "{message}");
    for template in TEMPLATES {
        assert!(message.contains(template), "{message}");
    }
}

#[test]
fn only_the_show_line_changes_with_the_template() {
    let source = "#import \"/nexus.typ\": *\n#show: cv.with(lang: \"de\")\n\n#header()\n";
    let modern = with_template(source, "modern").unwrap();
    assert_eq!(
        modern,
        "#import \"/nexus.typ\": *\n#show: cv.with(lang: \"de\", template: \"modern\")\n\n#header()\n"
    );
    assert_eq!(template_of(&modern), Some("modern"));
    assert_eq!(with_template(&modern, "classic").unwrap(), source);
    assert_eq!(
        with_template("#show: cv\r\n#header()", "compact").unwrap(),
        "#show: cv.with(template: \"compact\")\r\n#header()"
    );
    assert_eq!(with_template(source, "fancy"), None);
}

#[test]
fn the_page_target_is_kept_and_set_beside_the_template() {
    let source = "#import \"/nexus.typ\": *\n#show: cv.with(lang: \"de\")\n#header()\n";
    assert_eq!(pages_of(source), None);
    let two = with_pages(source, Some(2)).unwrap();
    assert_eq!(
        two,
        source.replace("lang: \"de\"", "lang: \"de\", pages: 2")
    );
    assert_eq!(
        (pages_of(&two), template_of(&two)),
        (Some(2), Some("classic"))
    );
    let modern = with_template(&two, "modern").unwrap();
    assert_eq!(
        modern,
        source.replace(
            "lang: \"de\"",
            "lang: \"de\", template: \"modern\", pages: 2"
        )
    );
    assert_eq!(
        with_pages(&modern, Some(1)).unwrap(),
        modern.replace("pages: 2", "pages: 1")
    );
    assert_eq!(
        with_pages(&with_template(&modern, "classic").unwrap(), None).unwrap(),
        source
    );
    assert_eq!(with_pages(source, Some(3)), None);
    for other in [
        "pages: 3",
        "pages: \"2\"",
        "pages: 1, pages: 2",
        "pages: auto",
    ] {
        let source = format!("#show: cv.with({other})\n");
        assert_eq!(
            (pages_of(&source), template_of(&source)),
            (None, None),
            "{other}"
        );
    }
}

#[test]
fn a_show_line_written_another_way_is_left_alone() {
    for source in [
        "#header()\n",
        "#show: cv\n#show: cv.with(lang: \"de\")\n",
        "#show: cv.with(lang: \"de\", paper: \"a5\")\n",
        "#show: cv.with(lang: language)\n",
        "#show: cv.with(lang: \"de\") // German\n",
        "#show: cvs\n",
        "  #show: cv\n",
    ] {
        assert_eq!(template_of(source), None, "{source}");
        assert_eq!(with_template(source, "modern"), None, "{source}");
    }
}

/// The editor completes from `HELPERS`, so a helper added to `/nexus.typ` has to be added there too.
#[test]
fn the_editors_helper_list_names_every_public_helper() {
    let mut defined: Vec<&str> = include_str!("../src/nexus.typ")
        .lines()
        .filter_map(|line| line.strip_prefix("#let "))
        .filter(|rest| !rest.starts_with('_'))
        .filter_map(|rest| rest.split(['(', ' ']).next())
        .collect();
    let mut listed: Vec<&str> = nexus_tailor::HELPERS
        .iter()
        .map(|helper| helper.0)
        .collect();
    defined.sort_unstable();
    listed.sort_unstable();
    assert_eq!(listed, defined);
}
