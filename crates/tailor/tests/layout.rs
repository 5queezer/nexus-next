//! The layout notes: advice read from where the words stand.

mod common;

use common::{content_en, facts_en, render};
use nexus_tailor::CvContent;
use nexus_tailor::{
    Layout, LayoutKind, TEMPLATES, check, density_of, pages_of, source_from_profile, with_density,
    with_pages, with_template,
};

const START: &str = "#import \"/nexus.typ\": *\n#show: cv\n#header()\n";

async fn layout_of(source: &str) -> Layout {
    render(source, &facts_en()).await.unwrap().text.layout
}

fn kinds(layout: &Layout) -> Vec<LayoutKind> {
    layout.notes.iter().map(|note| note.kind).collect()
}

fn long_en() -> CvContent {
    let mut content = content_en();
    for _ in 0..4 {
        let bullets = content.employment[0].bullets.clone();
        content.employment[0].bullets.extend(bullets);
    }
    content
}

/// A paragraph grows word by word, and is noted while one word stands on its last line.
#[tokio::test]
async fn a_last_line_of_one_word_is_a_stub_and_says_which() {
    let mut stubs = 0;
    for words in 12..40 {
        let source = format!("{START}{}end.\n", "word ".repeat(words));
        let rendered = render(&source, &facts_en()).await.unwrap();
        let alone = rendered.text.lines.last().unwrap() == "end.";
        let layout = rendered.text.layout;
        let full = rendered.text.lines.last().unwrap().split(' ').count() > 3;
        assert!(
            !alone || kinds(&layout) == [LayoutKind::Stub],
            "{words}: {layout:#?}"
        );
        assert!(!full || layout.notes.is_empty(), "{words}: {layout:#?}");
        if alone {
            let note = &layout.notes[0];
            assert_eq!((note.page, note.line.as_str()), (1, "end."));
            assert!(
                note.message.contains("\"end.\" is the last of"),
                "{}",
                note.message
            );
            let (line, first, count) = note.at.unwrap();
            assert_eq!(
                (rendered.text.lines[line].as_str(), first, count),
                ("end.", 0, 1)
            );
            stubs += 1;
        }
    }
    assert!(stubs > 0);
}

#[tokio::test]
async fn lines_that_only_stand_under_each_other_are_no_paragraph() {
    let source = format!(
        "{START}#skills(\"Languages\")[Rust, Go]\n#skills(\"Platform\")[AWS]\n= Languages\n#languages()\n"
    );
    assert_eq!(layout_of(&source).await.notes, []);
}

#[tokio::test]
async fn contact_details_that_need_two_lines_leave_none_alone() {
    let mut facts = facts_en();
    facts.contact.linkedin = "linkedin.com/in/mara-example-backend-engineer".to_owned();
    facts.contact.github = "github.com/mara-example-open-source".to_owned();
    for template in TEMPLATES {
        let source = format!(
            "#import \"/nexus.typ\": *\n#show: cv.with(template: \"{template}\")\n#header()\n"
        );
        let rendered = nexus_tailor::Renderer::InProcess
            .render(nexus_tailor::RenderRequest {
                source: source.clone(),
                facts: facts.clone(),
                pdf: false,
                preview_pages: 1,
                stamp: false,
            })
            .await
            .unwrap();
        if std::env::var_os("NEXUS_TEMPLATE_SHOTS").is_some() {
            std::fs::write(
                format!(
                    "{}/../../target/template-shots/contact-{template}.png",
                    env!("CARGO_MANIFEST_DIR")
                ),
                &rendered.previews[0],
            )
            .unwrap();
        }
        let text = rendered.text.lines.join("\n");
        assert!(
            !text.contains("\ngithub.com/mara-example-open-source"),
            "{template}\n{text}"
        );
        if template != "sidebar" {
            assert_eq!(rendered.text.layout.notes, [], "{template}\n{text}");
        }
    }
}

/// The templates keep a heading with what follows it. A source that undoes that can strand one: pushed
/// down the page until nothing fits under it, its section starts on the next.
#[tokio::test]
async fn a_heading_left_at_the_bottom_of_a_page_is_noted() {
    let mut alone = 0;
    for space in 230..262 {
        let source = format!(
            "{START}#show heading: set block(sticky: false)\n#v({space}mm)\n= Experience\n#block(breakable: false)[One\\ two\\ three]\n"
        );
        let layout = layout_of(&source).await;
        if let Some(note) = layout
            .notes
            .iter()
            .find(|note| note.kind == LayoutKind::HeadingAlone)
        {
            assert_eq!(
                (note.page, note.line.as_str(), layout.pages),
                (1, "Experience", 2)
            );
            assert!(
                note.message
                    .contains("\"Experience\" is the last line of page 1"),
                "{}",
                note.message
            );
            alone += 1;
        }
    }
    assert!(alone > 0);
    assert_eq!(
        layout_of(&source_from_profile(&long_en()))
            .await
            .notes
            .iter()
            .filter(|note| note.kind == LayoutKind::HeadingAlone)
            .count(),
        0
    );
}

#[tokio::test]
async fn an_entry_a_page_break_cuts_is_noted_in_every_template() {
    for template in TEMPLATES {
        let one = with_template(&source_from_profile(&content_en()), template).unwrap();
        assert!(
            !kinds(&layout_of(&one).await).contains(&LayoutKind::EntrySplit),
            "{template}"
        );
        let two = with_template(&source_from_profile(&long_en()), template).unwrap();
        let layout = layout_of(&two).await;
        let split: Vec<_> = layout
            .notes
            .iter()
            .filter(|note| note.kind == LayoutKind::EntrySplit)
            .collect();
        assert_eq!(split.len(), 1, "{template}: {layout:#?}");
        assert_eq!(
            (split[0].page, split[0].line.as_str()),
            (1, "Brightpath Logistics · Software Engineer")
        );
        assert!(
            split[0].message.contains("on page 1 and"),
            "{}",
            split[0].message
        );
    }
}

#[tokio::test]
async fn the_last_page_says_how_full_it_is_and_when_that_is_next_to_nothing() {
    let one = layout_of(&source_from_profile(&content_en())).await;
    assert_eq!(one.pages, 1);
    assert!((0.5..0.65).contains(&one.last_page_fill), "{one:#?}");
    assert!((18..30).contains(&one.room_lines), "{one:#?}");

    let stray = layout_of(&format!(
        "{START}#v(25cm)\nOne line.\n\nAnother.\n\nA third.\n"
    ))
    .await;
    assert_eq!(stray.pages, 2, "{stray:#?}");
    let note = stray
        .notes
        .iter()
        .find(|note| note.kind == LayoutKind::ThinLastPage)
        .unwrap();
    assert_eq!(note.page, 2);
    assert!(
        note.message.contains("brings the CV to 1 page."),
        "{}",
        note.message
    );
    assert!(stray.last_page_fill < 0.1);
}

/// What the owner aims for is in the source, and the notes say how far off the CV is, in lines.
#[tokio::test]
async fn a_page_target_says_how_far_off_the_cv_is() {
    let target = |layout: &Layout| {
        let note = &layout.notes[0];
        assert_eq!(note.kind, LayoutKind::PageTarget, "{layout:#?}");
        note.message.clone()
    };
    let one = source_from_profile(&content_en());
    let plain = layout_of(&one).await;
    assert_eq!(plain.target_pages, None);
    assert!(!kinds(&plain).contains(&LayoutKind::PageTarget));

    let roomy = layout_of(&with_pages(&one, Some(1)).unwrap()).await;
    assert_eq!((roomy.target_pages, roomy.pages), (Some(1), 1));
    let said = target(&roomy);
    assert!(
        said.starts_with("Aiming for 1 full page: page 1 is 5"),
        "{said}"
    );
    assert!(
        said.contains(&format!("room for about {} more lines", roomy.room_lines)),
        "{said}"
    );
    // Only the note differs from the source that names no target.
    assert_eq!(roomy.notes[1..], plain.notes[..]);

    let short = target(&layout_of(&with_pages(&one, Some(2)).unwrap()).await);
    assert!(
        short.starts_with("Aiming for 2 full pages: the CV ends on page 1"),
        "{short}"
    );
    let lines: u32 = short
        .split("About ")
        .nth(1)
        .unwrap()
        .split(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!((60..110).contains(&lines), "{short}");

    let long = with_pages(&source_from_profile(&long_en()), Some(1)).unwrap();
    let over = layout_of(&long).await;
    let said = target(&over);
    assert_eq!(
        said,
        format!(
            "Aiming for 1 full page: about {} lines over, on page 2. Cut that much.",
            over.last_page_lines
        )
    );
    assert!(over.last_page_lines > 10);

    let fits = target(&layout_of(&with_pages(&long, Some(2)).unwrap()).await);
    assert!(
        fits.starts_with("Aiming for 2 full pages: page 2 is 5"),
        "{fits}"
    );
}

#[tokio::test]
async fn a_page_target_draws_nothing() {
    let plain = source_from_profile(&content_en());
    let aimed = with_pages(&plain, Some(2)).unwrap();
    assert_eq!(pages_of(&aimed), Some(2));
    let draw = |source: String| async move {
        nexus_tailor::Renderer::InProcess
            .render(nexus_tailor::RenderRequest {
                source,
                facts: facts_en(),
                pdf: true,
                preview_pages: 1,
                stamp: false,
            })
            .await
            .unwrap()
    };
    let (plain, aimed) = (draw(plain).await, draw(aimed).await);
    assert_eq!(plain.pdf, aimed.pdf);
    assert_eq!(plain.previews, aimed.previews);
    assert_eq!(plain.text.lines, aimed.text.lines);
}

/// The same text tighter or looser, to close a small gap to a page target.
#[tokio::test]
async fn density_draws_the_same_text_tighter_or_looser_in_every_template() {
    let facts = facts_en();
    for template in TEMPLATES {
        let plain = with_template(&source_from_profile(&long_en()), template).unwrap();
        assert_eq!(density_of(&plain), Some(0));
        assert_eq!(with_density(&plain, 0).unwrap(), plain);
        let mut used = Vec::new();
        for density in -2..=2 {
            let source = with_density(&plain, density).unwrap();
            assert_eq!(density_of(&source), Some(density));
            let shots = std::env::var_os("NEXUS_TEMPLATE_SHOTS").is_some();
            let rendered = nexus_tailor::Renderer::InProcess
                .render(nexus_tailor::RenderRequest {
                    source: source.clone(),
                    facts: facts.clone(),
                    pdf: false,
                    preview_pages: u32::from(shots),
                    stamp: false,
                })
                .await
                .unwrap();
            if shots {
                let name = format!("density-{template}-{density}.png");
                let dir = format!("{}/../../target/template-shots", env!("CARGO_MANIFEST_DIR"));
                std::fs::write(format!("{dir}/{name}"), &rendered.previews[0]).unwrap();
            }
            let report = check(&rendered.text, &facts, None, "");
            assert_eq!(report.violations, [], "{template} {density}");
            let layout = &rendered.text.layout;
            used.push(f64::from(layout.pages) + f64::from(layout.last_page_fill));
            // The commonest word is body text, which stays at 9 pt or more: a box is 1.05 em high.
            let mut heights: Vec<f32> = rendered
                .text
                .places
                .iter()
                .flat_map(|place| place.words.iter().map(|word| word[3] * 841.89 / 1.05))
                .collect();
            heights.sort_by(f32::total_cmp);
            assert!(
                heights[heights.len() / 2] > 8.95,
                "{template} {density}: {}",
                heights[heights.len() / 2]
            );
        }
        assert!(
            used.windows(2).all(|pair| pair[0] > pair[1]),
            "{template}: {used:?}"
        );
        assert!(used[0] - used[4] > 0.15, "{template}: {used:?}");
    }
    let source = "#import \"/nexus.typ\": *\n#show: cv.with(density: 3)\n#header()\n";
    let Err(nexus_tailor::RenderError::Diagnostics(told)) = render(source, &facts).await else {
        panic!("compiled");
    };
    assert!(
        told[0].message.contains("no density 3"),
        "{}",
        told[0].message
    );
    assert_eq!(
        (density_of(source), with_density("#show: cv\n", 3)),
        (None, None)
    );
    assert_eq!(
        with_density("#show: cv.with(pages: 1)\n", 2).unwrap(),
        "#show: cv.with(pages: 1, density: 2)\n"
    );
}

/// A company and title too long for the line beside the date: the word left over is noted, with how
/// much shorter the header must be.
#[tokio::test]
async fn an_entry_header_that_leaves_one_word_beside_its_date_is_a_stub() {
    let mut stubs = 0;
    for template in ["classic", "modern"] {
        for words in 2..14 {
            let mut facts = facts_en();
            facts.employment[0].company =
                "Brightpath Logistics (client: a consumer electronics company)".to_owned();
            facts.employment[0].title = format!("{}Engineer", "QA ".repeat(words));
            let source = format!(
                "#import \"/nexus.typ\": *\n#show: cv.with(template: \"{template}\")\n#header()\n= Experience\n#employment(\"brightpath\")[- Work.]\n"
            );
            let rendered = render(&source, &facts).await.unwrap();
            let alone = rendered
                .text
                .lines
                .iter()
                .filter(|line| line.starts_with("Engineer "))
                .count();
            let layout = rendered.text.layout;
            let noted: Vec<_> = layout
                .notes
                .iter()
                .filter(|note| note.kind == LayoutKind::Stub)
                .collect();
            assert!(noted.len() <= 1, "{template} {words}: {layout:#?}");
            for note in &noted {
                assert!(
                    note.message
                        .starts_with("The header of the entry \"Brightpath Logistics (client:"),
                    "{}",
                    note.message
                );
            }
            if alone == 1 {
                assert_eq!(
                    noted.len(),
                    1,
                    "{template} {words}: {:#?}",
                    rendered.text.lines
                );
                assert_eq!(noted[0].line, "Engineer");
                assert!(
                    noted[0]
                        .message
                        .contains("\"Engineer\" stands alone on its last line. It is about "),
                    "{}",
                    noted[0].message
                );
                stubs += 1;
            }
        }
    }
    assert!(stubs > 1);
}
