//! Sources that try to get past the gate, with real compiles. Each must be refused or flagged.

mod common;

use common::{facts_en, render, render_and_check};
use nexus_tailor::{RenderError, Rule, Severity};

const START: &str = "#import \"/nexus.typ\": *\n#show: cv\n#header()\n";
/// The smallest valid PNG, one pixel.
const PNG: &str = "137,80,78,71,13,10,26,10,0,0,0,13,73,72,68,82,0,0,0,1,0,0,0,1,8,6,0,0,0,31,21,196,137,0,0,0,13,73,68,65,84,120,156,99,248,255,255,255,127,0,9,251,3,253,42,134,227,138,0,0,0,0,73,69,78,68,174,66,96,130";

#[tokio::test]
async fn flagged_sources() {
    let image = format!("#image(bytes(({PNG},)), width: 1cm)");
    let cases: Vec<(&str, Rule, &str)> = vec![
        // Numbers.
        ("literal year", Rule::Numbers, "Started in 2018."),
        (
            "show rewrite",
            Rule::Numbers,
            "#show \"2019\": \"2018\"\n#employment(\"brightpath\")[- Work.]",
        ),
        (
            "show regex",
            Rule::Numbers,
            "#show regex(\"20\\\\d\\\\d\"): \"2031\"\n#employment(\"brightpath\")[- Work.]",
        ),
        ("computed", Rule::Numbers, "#(\"20\" + \"18\")"),
        ("computed number", Rule::Numbers, "#(1000 + 1018)"),
        ("box split", Rule::Numbers, "#box[20]#box[18]"),
        (
            "box in a word",
            Rule::Numbers,
            "Since 20#box[18] I work here.",
        ),
        ("kerned apart", Rule::Numbers, "20#h(0.05em)18"),
        (
            "reversed placement",
            Rule::Numbers,
            "#place(dx: 1cm)[18]#place(dx: 0cm)[20]",
        ),
        ("raised digits", Rule::Numbers, "20#super[18]"),
        ("Arabic-Indic digits", Rule::Numbers, "Seit ٢٠١٨ hier."),
        ("Extended Arabic digits", Rule::Numbers, "۲۰۱۸"),
        ("Devanagari digits", Rule::Numbers, "२०१८"),
        ("fullwidth digits", Rule::Numbers, "２０１８"),
        ("mathematical digits", Rule::Numbers, "𝟚𝟘𝟙𝟠"),
        ("superscript digits", Rule::Numbers, "²⁰¹⁸"),
        ("zero width in a number", Rule::Numbers, "20\u{200b}18"),
        ("soft hyphen in a number", Rule::Numbers, "20\u{ad}18"),
        ("combining mark in a number", Rule::Numbers, "2\u{301}018"),
        (
            "thousand separator",
            Rule::Numbers,
            "Served 1,300 customers.",
        ),
        ("German thousand separator", Rule::Numbers, "1.300 Kunden."),
        ("decimal", Rule::Numbers, "Rated 4.9 stars."),
        ("percent", Rule::Numbers, "Grew by 41%."),
        ("percent word", Rule::Numbers, "Um 41 Prozent gesteigert."),
        ("currency", Rule::Numbers, "Saved €900k."),
        ("currency after", Rule::Numbers, "Spart 900 €."),
        ("date with slash", Rule::Numbers, "Until 04/2019."),
        ("ISO date", Rule::Numbers, "Until 2019-04."),
        ("German month", Rule::Numbers, "Seit April 2019."),
        ("English month", Rule::Numbers, "Since Jun 2023."),
        ("full date", Rule::Numbers, "Am 15.03.2019 begonnen."),
        ("number in a word", Rule::Numbers, "Version x2018 shipped."),
        (
            "small number near a phone number",
            Rule::Numbers,
            "Led 170 people.",
        ),
        ("postcode digits alone", Rule::Numbers, "10115 customers."),
        (
            "metric number with a new percent",
            Rule::Numbers,
            "Cut latency by 1,200%.",
        ),
        (
            "in a list number",
            Rule::Numbers,
            "+ one\n+ two\n#enum.item(2018)[x]",
        ),
        ("str of a number", Rule::Numbers, "#str(2018)"),
        ("raw", Rule::Numbers, "`2018`"),
        ("heading", Rule::Numbers, "= Experience since 2018"),
        ("footnote", Rule::Numbers, "Work.#footnote[Since 2018.]"),
        (
            "header on the page",
            Rule::Numbers,
            "#set page(header: [2018])\nWork.",
        ),
        (
            "page number",
            Rule::Numbers,
            "#set page(numbering: \"1\")\n#counter(page).update(2018)\nWork.",
        ),
        (
            "page label only in the PDF",
            Rule::Numbers,
            "#set page(numbering: \"2018-1\", footer: none)\nWork.",
        ),
        // Text that no reader sees.
        ("white text", Rule::Numbers, "#text(fill: white)[2018]"),
        (
            "transparent text",
            Rule::Numbers,
            "#text(fill: rgb(0, 0, 0, 0))[2018]",
        ),
        ("1pt text", Rule::Numbers, "#text(size: 1pt)[2018]"),
        ("0pt text", Rule::Numbers, "#text(size: 0pt)[2018]"),
        ("off page", Rule::Numbers, "#place(dx: -100cm)[2018]"),
        (
            "far below the page",
            Rule::Numbers,
            "#place(dy: 100cm)[2018]",
        ),
        (
            "clipped away",
            Rule::Numbers,
            "#box(width: 0pt, height: 0pt, clip: true)[2018]",
        ),
        (
            "scaled to nothing",
            Rule::Numbers,
            "#scale(x: 1%, y: 1%)[2018]",
        ),
        ("rotated", Rule::Numbers, "#rotate(90deg)[2018]"),
        (
            "page background",
            Rule::Numbers,
            "#set page(background: text(fill: white)[2018])\nWork.",
        ),
        (
            "tiling on a shape",
            Rule::Numbers,
            "#rect(width: 1cm, height: 1cm, fill: tiling(size: (20pt, 20pt))[2018])",
        ),
        (
            "tiling on text",
            Rule::Numbers,
            "#text(fill: tiling(size: (20pt, 20pt))[2018])[Work.]",
        ),
        (
            "tiling on a stroke",
            Rule::Numbers,
            "#rect(width: 1cm, stroke: 2pt + tiling(size: (20pt, 20pt))[2018])",
        ),
        (
            "tiling on the page",
            Rule::Numbers,
            "#set page(fill: tiling(size: (20pt, 20pt))[2018])\nWork.",
        ),
        (
            "tiling in a tiling",
            Rule::Numbers,
            "#rect(width: 1cm, fill: tiling(size: (20pt, 20pt), rect(width: 1cm, fill: tiling(size: (9pt, 9pt))[2018])))",
        ),
        // Metadata.
        (
            "document title",
            Rule::Numbers,
            "#set document(title: \"Engineer since 2018\")",
        ),
        (
            "document author",
            Rule::Numbers,
            "#set document(author: \"Mara 2018\")",
        ),
        (
            "document keywords",
            Rule::Numbers,
            "#set document(keywords: (\"2018\",))",
        ),
        (
            "document description",
            Rule::Numbers,
            "#set document(description: \"Since 2018\")",
        ),
        (
            "document date",
            Rule::Numbers,
            "#set document(date: datetime(year: 2018, month: 1, day: 1))",
        ),
        (
            "link target",
            Rule::Numbers,
            "#link(\"https://example.org/2018\")[Work]",
        ),
        (
            "claim in metadata",
            Rule::ClaimWords,
            "#set document(keywords: (\"PhD\",))",
        ),
        (
            "banned in metadata",
            Rule::BannedPhrases,
            "#set document(description: \"A passionate team player\")",
        ),
        (
            "heading kept off the page",
            Rule::Numbers,
            "#show heading: none\n= Since 2018\nWork.",
        ),
        (
            "claim in a hidden heading",
            Rule::ClaimWords,
            "#show heading: none\n= Doctorate\nWork.",
        ),
        // Text no reader sees, with wording that would pass by itself.
        (
            "true title in white",
            Rule::HiddenText,
            "Brightpath Logistics · Sales Engineer\n#text(fill: white)[Software Engineer 03/2019 – 12/2022]",
        ),
        (
            "almost white",
            Rule::HiddenText,
            "#text(fill: luma(240))[Work]",
        ),
        (
            "transparent",
            Rule::HiddenText,
            "#text(fill: rgb(0, 0, 0, 20))[Work]",
        ),
        ("tiny", Rule::HiddenText, "#text(size: 1pt)[Work]"),
        ("zero size", Rule::HiddenText, "#text(size: 0pt)[Work]"),
        (
            "scaled down",
            Rule::HiddenText,
            "#scale(x: 5%, y: 5%)[Work]",
        ),
        ("off the left", Rule::HiddenText, "#place(dx: -100cm)[Work]"),
        (
            "off the bottom",
            Rule::HiddenText,
            "#place(dy: 100cm)[Work]",
        ),
        (
            "clipped to nothing",
            Rule::HiddenText,
            "#box(width: 0pt, height: 0pt, clip: true)[Work]",
        ),
        (
            "moved out of a clip",
            Rule::HiddenText,
            "#box(width: 1cm, height: 1cm, clip: true)[#move(dx: 5cm)[Work]]",
        ),
        (
            "in a tiling",
            Rule::HiddenText,
            "#rect(width: 1cm, fill: tiling(size: (30pt, 30pt))[Work])",
        ),
        // Banned phrases.
        ("banned", Rule::BannedPhrases, "A passionate team player."),
        (
            "banned over a line",
            Rule::BannedPhrases,
            "#box(width: 2.2cm)[A passionate team player.]",
        ),
        (
            "banned over markup lines",
            Rule::BannedPhrases,
            "A passionate\nteam\nplayer.",
        ),
        (
            "banned over a parbreak space",
            Rule::BannedPhrases,
            "A passionate    team \u{a0} player.",
        ),
        (
            "banned zero width",
            Rule::BannedPhrases,
            "A passion\u{200b}ate team player.",
        ),
        (
            "banned word joiner",
            Rule::BannedPhrases,
            "A passionate te\u{2060}am player.",
        ),
        (
            "banned soft hyphen",
            Rule::BannedPhrases,
            "A pas\u{ad}sionate team player.",
        ),
        (
            "banned case",
            Rule::BannedPhrases,
            "A PaSsIoNaTe TeAm PlAyEr.",
        ),
        (
            "banned upper",
            Rule::BannedPhrases,
            "#upper[a passionate team player]",
        ),
        (
            "banned compatibility letters",
            Rule::BannedPhrases,
            "A ｐａｓｓｉｏｎａｔｅ team player.",
        ),
        (
            "banned box split",
            Rule::BannedPhrases,
            "A pass#box[ionate te]am player.",
        ),
        (
            "banned hyphenated",
            Rule::BannedPhrases,
            "#set text(hyphenate: true)\n#box(width: 0.9cm)[Rockstar Rockstar Rockstar]",
        ),
        (
            "banned computed",
            Rule::BannedPhrases,
            "#(\"Rock\" + \"star\")",
        ),
        // Images.
        ("raster image", Rule::Images, image.as_str()),
        (
            "SVG image",
            Rule::Images,
            "#image(bytes(\"<svg xmlns='http://www.w3.org/2000/svg' width='9' height='9'><text>Doctorate</text></svg>\"), format: \"svg\", width: 1cm)",
        ),
        // Binding.
        (
            "shadowed facts",
            Rule::EntityBinding,
            "#let facts = (employment: ((company: \"Brightpath Logistics\", date: \"Jan 2016\"),))\n#facts.employment.at(0).company #facts.employment.at(0).date",
        ),
        (
            "fake employment",
            Rule::EntityBinding,
            "#let employment(id, body) = [Brightpath Logistics · Software Engineer · 2016 #body]\n#employment(\"brightpath\")[- Work.]",
        ),
        (
            "another entry's date",
            Rule::EntityBinding,
            "Brightpath Logistics · Software Engineer · 2016\n\nIt ran 03/2019 – 12/2022.",
        ),
        (
            "another entry's month",
            Rule::EntityBinding,
            "Quillon Systems · Backend Developer 03/2019\n\nJan 2016 – Feb 2019",
        ),
        (
            "upper case company with another date",
            Rule::EntityBinding,
            "BRIGHTPATH LOGISTICS 2016\n\nSoftware Engineer 03/2019 – 12/2022",
        ),
        (
            "lower case company with another date",
            Rule::EntityBinding,
            "#lower[Brightpath Logistics] 2016",
        ),
        (
            "wrong title",
            Rule::EntityBinding,
            "Brightpath Logistics · Hardware Engineer · 03/2019 – 12/2022",
        ),
        (
            "no date",
            Rule::EntityBinding,
            "Brightpath Logistics · Software Engineer",
        ),
        (
            "wrong title, swapped by a show rule",
            Rule::EntityBinding,
            "#show \"Software Engineer\": \"Hardware Engineer\"\n#employment(\"brightpath\")[- Work.]",
        ),
        (
            "wrong date, swapped by a show rule",
            Rule::EntityBinding,
            "#show \"03/2019 – 12/2022\": \"Jan 2016 – Feb 2019\"\n#employment(\"brightpath\")[- Work.]",
        ),
        (
            "wrong institution date",
            Rule::EntityBinding,
            "Hochschule Nordkap 2016 Bachelor of Science, Computer Science",
        ),
        // Number words.
        (
            "eight years",
            Rule::NumberWords,
            "With eight years of experience.",
        ),
        (
            "Eight Years capitalized",
            Rule::NumberWords,
            "Eight Years of experience.",
        ),
        (
            "ten plus years",
            Rule::NumberWords,
            "Over ten+ years of work.",
        ),
        ("two decades", Rule::NumberWords, "For two decades."),
        ("hyphenated", Rule::NumberWords, "A ten-year career."),
        ("compound tens", Rule::NumberWords, "For twenty-two years."),
        (
            "split over a line",
            Rule::NumberWords,
            "#box(width: 1.5cm)[With eight years of it.]",
        ),
        (
            "zehn Jahre",
            Rule::NumberWords,
            "Über zehn Jahre Erfahrung.",
        ),
        (
            "seit zwölf langen Jahren",
            Rule::NumberWords,
            "Seit zwölf langen Jahren dabei.",
        ),
        (
            "zehnjährig",
            Rule::NumberWords,
            "Eine zehnjährige Laufbahn.",
        ),
        (
            "zweiundzwanzig",
            Rule::NumberWords,
            "Nach zweiundzwanzig Jahren.",
        ),
        // Claim words.
        (
            "a new doctorate",
            Rule::ClaimWords,
            "Holds a doctorate in physics.",
        ),
        ("PhD", Rule::ClaimWords, "Ph.D. in physics."),
        ("MBA", Rule::ClaimWords, "An MBA from a good school."),
        ("Master", Rule::ClaimWords, "Master of Arts in History."),
        (
            "Masterstudium",
            Rule::ClaimWords,
            "Abgeschlossenes Masterstudium.",
        ),
        ("Studium", Rule::ClaimWords, "Studium der Physik."),
        (
            "Informatikstudium",
            Rule::ClaimWords,
            "Ein Informatikstudium.",
        ),
        ("Promotion", Rule::ClaimWords, "Promotion in Chemie."),
        ("Dipl.-Ing.", Rule::ClaimWords, "Dipl.-Ing. Maschinenbau."),
        ("Senior", Rule::ClaimWords, "As Senior Engineer."),
        (
            "senior in lower case",
            Rule::ClaimWords,
            "As a senior engineer.",
        ),
        ("Lead", Rule::ClaimWords, "Team Lead for the platform."),
        ("Head of", Rule::ClaimWords, "head of engineering."),
        ("CTO", Rule::ClaimWords, "Acting CTO."),
        ("Teamleiter", Rule::ClaimWords, "Als Teamleiter tätig."),
        (
            "Geschäftsführerin",
            Rule::ClaimWords,
            "Als Geschäftsführerin tätig.",
        ),
        ("fluent", Rule::ClaimWords, "Fluent in French."),
        ("C2", Rule::ClaimWords, "French (C2)."),
        ("fließend", Rule::ClaimWords, "Französisch fließend."),
        (
            "Muttersprachlerin",
            Rule::ClaimWords,
            "Französisch als Muttersprachlerin.",
        ),
        (
            "verhandlungssicher",
            Rule::ClaimWords,
            "Verhandlungssicher in Französisch.",
        ),
    ];
    let facts = facts_en();
    let mut missed = Vec::new();
    for (name, rule, body) in cases {
        let source = format!("{START}{body}\n");
        match render(&source, &facts).await {
            Ok(rendered) => {
                let report = nexus_tailor::check(&rendered.text, &facts, None);
                if !report.violations.iter().any(|v| v.rule == rule) {
                    missed.push(format!(
                        "{name}: wanted {rule:?}, got {:?}\n  lines: {:?}\n  meta: {:?}",
                        report.violations, rendered.text.lines, rendered.text.metadata
                    ));
                }
            }
            Err(e) => missed.push(format!("{name}: does not compile: {e:?}")),
        }
    }
    assert!(missed.is_empty(), "{}", missed.join("\n"));
}

/// Things a compile refuses by itself, each with a diagnostic naming the problem.
#[tokio::test]
async fn refused_sources() {
    let cases = [
        ("#hide[2018]", "hide is not allowed"),
        ("#std.hide[2018]", "hide is not allowed"),
        ("#let h = hide\n#h[Rockstar]", "hide is not allowed"),
        (
            "#pdf.attach(\"x.txt\", bytes(\"2018\"))",
            "pdf.attach is not allowed",
        ),
        ("#import \"@preview/x:1.0.0\"", "does not exist"),
        ("#read(\"/etc/passwd\")", "does not exist"),
        ("#read(\"../../x\")", "would escape the project root"),
        ("#read(\"/proc/self/environ\")", "does not exist"),
        ("#image(\"/etc/hostname\")", "does not exist"),
        ("#include \"/other.typ\"", "does not exist"),
        ("#datetime.today()", "current date"),
        (
            "#employment(\"nope\")[- Work.]",
            "no employment with id \"nope\"",
        ),
        ("#education(\"nope\")", "no education with id \"nope\""),
        ("#metric(\"nope\")", "no metric with key \"nope\""),
        ("#unknown-function()", "unknown variable"),
    ];
    let facts = facts_en();
    for (body, wanted) in cases {
        let source = format!("{START}{body}\n");
        let Err(RenderError::Diagnostics(diagnostics)) = render(&source, &facts).await else {
            panic!("{body} should not compile");
        };
        let first = &diagnostics[0];
        assert_eq!(first.severity, Severity::Error, "{body}");
        assert!(
            diagnostics.iter().any(|d| d.message.contains(wanted)),
            "{body}: {diagnostics:?}"
        );
        assert_eq!(
            first.file.as_deref(),
            Some("/main.typ"),
            "{body}: {first:?}"
        );
        let last_line = 3 + body.lines().count() as u32;
        assert_eq!(first.line, Some(last_line), "{body}: {first:?}");
    }
}

#[tokio::test]
async fn reading_the_facts_is_fine() {
    let source = format!("{START}#let data = json(\"/facts.json\")\n#data.name\n");
    let report = render_and_check(&source, &facts_en()).await;
    assert_eq!(report.violations, []);
}

#[tokio::test]
async fn box_split_digits_join_in_the_lines() {
    let rendered = render("#box[20]#box[15]\n", &facts_en()).await.unwrap();
    assert_eq!(rendered.text.lines, ["2015"]);
}

#[tokio::test]
async fn zero_size_text_stays_in_the_lines() {
    let rendered = render("Shown #text(size: 0pt)[tiny]\n", &facts_en())
        .await
        .unwrap();
    assert_eq!(rendered.text.lines, ["Shown tiny"]);
    assert_eq!(rendered.text.hidden, ["tiny"]);
}

/// With no math font, Typst refuses math. If that ever changes, digits in it must still be seen.
#[tokio::test]
async fn math_is_refused_or_read() {
    let facts = facts_en();
    match render("$2018$\n", &facts).await {
        Err(RenderError::Diagnostics(_)) => {}
        Ok(rendered) => {
            let report = nexus_tailor::check(&rendered.text, &facts, None);
            assert!(
                report.violations.iter().any(|v| v.rule == Rule::Numbers),
                "{:?}",
                rendered.text
            );
        }
        Err(other) => panic!("{other:?}"),
    }
}
