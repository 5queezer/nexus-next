//! The layout notes: advice read from where the words stand.

mod common;

use common::{content_en, facts_en, render};
use nexus_tailor::CvContent;
use nexus_tailor::{Layout, LayoutKind, TEMPLATES, source_from_profile, with_template};

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
