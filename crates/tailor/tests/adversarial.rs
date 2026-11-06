//! Sources that try to get past the gate, with real compiles. Each must be refused, or listed for the
//! owner's review.

mod common;

use common::{facts_en, render, render_and_check};
use nexus_tailor::{Facts, RenderError, RenderRequest, Renderer, Rule, Severity, check};

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
            Rule::EntityBinding,
            "#show \"2019\": \"2018\"\n#employment(\"brightpath\")[- Work.]",
        ),
        (
            "show regex",
            Rule::EntityBinding,
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
    assert_flagged(&cases).await;
}

/// A hard rule's case must be refused. A review rule's must be listed, whatever else its source does.
fn is_hard(rule: Rule) -> bool {
    !matches!(
        rule,
        Rule::MissingFact
            | Rule::EntityBinding
            | Rule::LanguageLevel
            | Rule::ClaimWords
            | Rule::Numbers
            | Rule::NumberWords
            | Rule::NewWords
    )
}

async fn assert_flagged(cases: &[(&str, Rule, &str)]) {
    assert_flagged_with(&facts_en(), cases).await;
}

async fn assert_flagged_with(facts: &Facts, cases: &[(&str, Rule, &str)]) {
    let mut missed = Vec::new();
    for (name, rule, body) in cases {
        let source = format!("{START}{body}\n");
        match render(&source, facts).await {
            Ok(rendered) => {
                let report = nexus_tailor::check(&rendered.text, facts, None, "");
                let found = if is_hard(*rule) {
                    report.violations.iter().any(|v| v.rule == *rule)
                } else {
                    report.review_items.iter().any(|i| i.rule == *rule)
                };
                if !found {
                    missed.push(format!(
                        "{name}: wanted {rule:?}, got {:?}\n  items: {:?}\n  lines: {:?}",
                        report.violations, report.review_items, rendered.text.lines
                    ));
                }
            }
            Err(e) => missed.push(format!("{name}: does not compile: {e:?}")),
        }
    }
    assert!(missed.is_empty(), "{}", missed.join("\n"));
}

#[tokio::test]
async fn dates_swapped_across_lines() {
    assert_flagged(&[
        ("swapped between two companies", Rule::EntityBinding, "Brightpath Logistics · Software Engineer\n\nJan 2016 – Feb 2019\n\nQuillon Systems · Backend Developer\n\n03/2019 – 12/2022"),
        ("another entry's date after its own", Rule::EntityBinding, "Brightpath Logistics · Software Engineer\n\n03/2019 – 12/2022\n\nJan 2016 – Feb 2019"),
        ("another entry's date several lines on", Rule::EntityBinding, "Brightpath Logistics · Software Engineer 03/2019 – 12/2022\n\nOne.\n\nTwo.\n\nThree.\n\nFour.\n\nJan 2016 – Feb 2019"),
        ("own date too far away", Rule::EntityBinding, "Brightpath Logistics · Software Engineer\n\nOne.\n\nTwo.\n\nThree.\n\n03/2019 – 12/2022"),
        ("own date only after the next company", Rule::EntityBinding, "Brightpath Logistics · Software Engineer\n\nQuillon Systems · Backend Developer Jan 2016 – Feb 2019\n\n03/2019 – 12/2022"),
        ("swapped with an institution", Rule::EntityBinding, "Hochschule Nordkap\n\nBachelor of Science, Computer Science\n\n2014 – 2015\n\nOne.\n\nTwo.\n\n2012 – 2015"),
        ("in lower case", Rule::EntityBinding, "#lower[Brightpath Logistics]\n\nJan 2016 – Feb 2019"),
    ])
    .await;
}

#[tokio::test]
async fn homoglyphs() {
    assert_flagged(&[
        (
            "Latin with a Cyrillic e",
            Rule::MixedScript,
            "Long exреrience here.",
        ),
        (
            "Latin with a Greek alpha",
            Rule::MixedScript,
            "A pαssion for it.",
        ),
        (
            "Latin with a Greek capital",
            Rule::MixedScript,
            "Εxpert work.",
        ),
        (
            "mixed part of a hyphenated word",
            Rule::MixedScript,
            "Well-rоunded work.",
        ),
        (
            "mixed claim, as a claim",
            Rule::ClaimWords,
            "As Sеnior Engineer.",
        ),
        ("mixed capitalized claim", Rule::ClaimWords, "Team Lеad."),
        ("wholly Cyrillic claim", Rule::ClaimWords, "Acting СЕО."),
        ("wholly Cyrillic degree", Rule::ClaimWords, "An МВА."),
        ("wholly Greek degree", Rule::ClaimWords, "An ΜΒΑ."),
        (
            "banned, mixed",
            Rule::BannedPhrases,
            "A раssionate team player.",
        ),
        ("banned word, mixed", Rule::BannedPhrases, "Rосkstar."),
    ])
    .await;
}

#[tokio::test]
async fn spaced_letters() {
    assert_flagged(&[
        ("claim", Rule::ClaimWords, "As S e n i o r Engineer."),
        (
            "upper case claim",
            Rule::ClaimWords,
            "As S E N I O R Engineer.",
        ),
        ("three letters", Rule::ClaimWords, "With a P h D."),
        ("an acronym", Rule::ClaimWords, "An M B A."),
        (
            "starting with a letter that is also a word",
            Rule::ClaimWords,
            "A b i t u r in Kiel.",
        ),
        (
            "spaced by layout",
            Rule::ClaimWords,
            "S#h(0.5em)e#h(0.5em)n#h(0.5em)i#h(0.5em)o#h(0.5em)r",
        ),
        ("banned word", Rule::BannedPhrases, "R o c k s t a r."),
        (
            "banned phrase, one word spaced",
            Rule::BannedPhrases,
            "A p a s s i o n a t e team player.",
        ),
        (
            "Cyrillic and spaced",
            Rule::ClaimWords,
            "As S е n i o r Engineer.",
        ),
    ])
    .await;
}

#[tokio::test]
async fn other_numerals() {
    assert_flagged(&[
        ("CJK positional year", Rule::Numbers, "Since 二〇一八."),
        ("CJK with a unit", Rule::Numbers, "十年 of work."),
        ("CJK unit alone", Rule::Numbers, "百 customers."),
        ("Roman numeral sign", Rule::Numbers, "Ⅷ Jahre Erfahrung."),
        (
            "small Roman numeral sign",
            Rule::Numbers,
            "ⅷ years of work.",
        ),
        (
            "two Roman numeral signs",
            Rule::Numbers,
            "ⅩⅧ years of work.",
        ),
        ("Suzhou numerals", Rule::Numbers, "〡〢 customers."),
        (
            "ASCII Roman with a time unit",
            Rule::NumberWords,
            "VIII years of work.",
        ),
        (
            "German, ASCII Roman with a time unit",
            Rule::NumberWords,
            "X Jahre Erfahrung.",
        ),
        (
            "hyphenated ASCII Roman",
            Rule::NumberWords,
            "A VIII-year career.",
        ),
        (
            "ASCII Roman with a filler",
            Rule::NumberWords,
            "XII long years.",
        ),
    ])
    .await;
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
        (
            "#show pdf.attach: none\n#pdf.attach(\"x.txt\", bytes(\"2018\"))",
            "pdf.attach is not allowed",
        ),
        (
            "#show pdf.attach: it => []\n#pdf.attach(\"x.txt\", bytes(\"2018\"))",
            "pdf.attach is not allowed",
        ),
        (
            "#show pdf.attach: none\n#context pdf.attach(\"x.txt\", bytes(\"2018\"))",
            "pdf.attach is not allowed",
        ),
        ("#show hide: it => it\n#hide[2018]", "hide is not allowed"),
        (
            "#show hide: set text(fill: red)\n#hide[2018]",
            "hide is not allowed",
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
            let report = nexus_tailor::check(&rendered.text, &facts, None, "");
            assert!(
                report.review_items.iter().any(|i| i.rule == Rule::Numbers),
                "{:?}",
                rendered.text
            );
        }
        Err(other) => panic!("{other:?}"),
    }
}

/// The exporter asks the document's introspector for attachments, which has never seen a tiling's
/// content. So one there reaches no PDF, as the check's query says.
#[tokio::test]
async fn an_attachment_in_a_tiling_reaches_no_pdf() {
    let source = "#show pdf.attach: none\n#rect(fill: tiling(size: (9pt, 9pt))[#pdf.attach(\"x.txt\", bytes(\"2018\"))])\n";
    let rendered = Renderer::InProcess
        .render(RenderRequest {
            source: source.to_owned(),
            facts: facts_en(),
            pdf: true,
            preview_pages: 0,
            stamp: false,
        })
        .await
        .unwrap();
    let pdf = String::from_utf8_lossy(rendered.pdf.as_deref().unwrap()).into_owned();
    assert!(!pdf.contains("EmbeddedFile"));
}

/// A show rule that takes over `hide` can only draw its body, which the check reads, or drop it.
#[tokio::test]
async fn a_show_rule_on_hide_cannot_hide_text_from_the_check() {
    assert_flagged(&[(
        "drawn after all",
        Rule::Numbers,
        "#show hide: it => it.body\n#hide[2018]",
    )])
    .await;
    let rendered = render("#show hide: none\n#hide[2018]\nWork.\n", &facts_en())
        .await
        .unwrap();
    assert_eq!(rendered.text.lines, ["Work."]);
}

/// The review's proofs of finding 6, and every control character in a run.
#[tokio::test]
async fn bidi_controls_are_refused_wherever_they_stand() {
    let mut facts = facts_en();
    facts.allowed_tokens.push("12".to_owned());
    facts.banned_phrases.push("rockstar ninja".to_owned());
    assert_flagged_with(
        &facts,
        &[
            (
                "a reversed claim",
                Rule::BidiControl,
                "#\"\\u{202E}roineS\\u{202C}\" engineer",
            ),
            (
                "a reversed number",
                Rule::BidiControl,
                "Moved #\"\\u{202E}12\\u{202C}\" services.",
            ),
            (
                "reversed claims",
                Rule::BidiControl,
                "#\"\\u{202E}rotceriD roineS ,DhP\\u{202C}\"",
            ),
            (
                "a reversed banned phrase",
                Rule::BidiControl,
                "#\"\\u{202E}ajnin ratskcor\\u{202C}\"",
            ),
            (
                "in the metadata",
                Rule::BidiControl,
                "#set document(description: \"\\u{202E}roineS\")",
            ),
            (
                "digits with the control trimmed from the line's start",
                Rule::BidiControl,
                "#\"\\u{202E}12\"",
            ),
            (
                "Arabic digits reversed",
                Rule::BidiControl,
                "#\"\\u{202E}١٢\"",
            ),
            (
                "an override as an element of its own",
                Rule::BidiControl,
                "Moved #\"\\u{202E}\"12 services.",
            ),
            (
                "an isolate around an override",
                Rule::BidiControl,
                "#\"\\u{2067}\\u{202E}12\\u{202C}\\u{2069}\"",
            ),
        ],
    )
    .await;
    for control in [
        0x200E, 0x202A, 0x202B, 0x202C, 0x202D, 0x202E, 0x2066, 0x2067, 0x2068, 0x2069,
    ] {
        let body = format!("#\"Wo\\u{{{control:X}}}rk done.\"");
        let name = format!("U+{control:X}");
        assert_flagged_with(&facts, &[(&name, Rule::BidiControl, &body)]).await;
    }
}

/// A right-to-left mark forms a run of its own that holds no glyph, so no run shows it. Whatever order
/// it causes between runs is the order the lines read, since they join runs by where they draw.
#[tokio::test]
async fn a_mark_that_reorders_runs_reads_as_drawn() {
    let rendered = render("#\"\\u{200F}12 34\"\n", &facts_en()).await.unwrap();
    assert_eq!(rendered.text.lines, ["34 12"]);
}

/// The review's proofs of finding 7: the check read `12 6` and a 6, the page shows 126 and a 9.
#[tokio::test]
async fn transforms_cannot_draw_numbers_the_check_reads_otherwise() {
    let mut facts = facts_en();
    facts.allowed_tokens.extend(["12", "6"].map(str::to_owned));
    assert_flagged_with(
        &facts,
        &[
            (
                "scaled digits drawn together",
                Rule::Numbers,
                "Served #box(scale(1000%, reflow: true, text(size: 1.05pt)[12]))#h(.35pt)#box(scale(1000%, reflow: true, text(size: 1.05pt)[6])) clients.",
            ),
            (
                "a 6 turned into a 9",
                Rule::Transform,
                "#box(rotate(180deg)[6]) years in freight.",
            ),
            ("rotated", Rule::Transform, "#rotate(90deg)[Work]"),
            ("barely rotated", Rule::Transform, "#rotate(1deg)[Work]"),
            ("mirrored", Rule::Transform, "#scale(x: -100%)[Work]"),
            (
                "mirrored both ways",
                Rule::Transform,
                "#scale(x: -100%, y: -100%)[Work]",
            ),
            ("skewed", Rule::Transform, "#skew(ax: 30deg)[Work]"),
            ("stretched", Rule::Transform, "#scale(x: 150%, y: 100%)[Work]"),
        ],
    )
    .await;
}

/// Found while fixing finding 7: glyphs and spaces can be squeezed as far as transforms can move them.
#[tokio::test]
async fn squeezed_text_cannot_draw_numbers_the_check_reads_otherwise() {
    let mut facts = facts_en();
    facts
        .allowed_tokens
        .extend(["1", "2", "12"].map(str::to_owned));
    assert_flagged_with(
        &facts,
        &[
            (
                "negative tracking draws 12 as 21",
                Rule::Transform,
                "Served #text(tracking: -1.3em)[12] clients.",
            ),
            (
                "no word space draws 1 2 as 12",
                Rule::Transform,
                "Served #text(spacing: 0%)[1 2] clients.",
            ),
            (
                "runs drawn over one another, which a PDF reader joins",
                Rule::Transform,
                "Served #box[12]#h(-1.4em)#box[6] clients.",
            ),
            (
                "a thin word space draws 1 2 almost as 12",
                Rule::Transform,
                "Served #text(spacing: 30%)[1 2] clients.",
            ),
            (
                "negative word space",
                Rule::Transform,
                "Served #text(spacing: -50%)[1 2] clients.",
            ),
            (
                "a space tracked away",
                Rule::Transform,
                "Served #text(tracking: -0.22em)[1 2] clients.",
            ),
        ],
    )
    .await;
    let mut no_twelve = facts_en();
    no_twelve
        .allowed_tokens
        .extend(["1", "2"].map(str::to_owned));
    assert_flagged_with(
        &no_twelve,
        &[
            (
                "a tiny space between digits",
                Rule::Numbers,
                "Served 1#text(size: 1pt)[ ]2 clients.",
            ),
            (
                "a tiny space between boxes",
                Rule::Numbers,
                "Served #box[1]#text(size: 0.5pt)[ ]#box[2] clients.",
            ),
        ],
    )
    .await;
    // It reads as what it draws.
    let source = format!("{START}Served 1#text(size: 1pt)[ ]2 clients.\n");
    let report = render_and_check(&source, &no_twelve).await;
    assert!(
        report.review_items.iter().any(|i| i.token == "12"),
        "{:?}",
        report.review_items
    );
}

/// The review's proof of finding 9: the bookmark says what the page doesn't.
#[tokio::test]
async fn heading_numbering_reaches_the_check_as_the_bookmark_has_it() {
    assert_flagged(&[
        (
            "numbering function on a hidden heading",
            Rule::Numbers,
            "#set heading(numbering: (..n) => \"CEO since 1999, PhD\")\n#show heading: none\n= Experience",
        ),
        (
            "a claim in the numbering",
            Rule::ClaimWords,
            "#set heading(numbering: (..n) => \"CEO since 1999, PhD\")\n#show heading: none\n= Experience",
        ),
        (
            "numbering on one heading",
            Rule::Numbers,
            "#show heading: none\n#heading(numbering: (..n) => \"PhD 2029\")[Experience]",
        ),
    ])
    .await;
    let source = "#set heading(numbering: (..n) => \"PhD\")\n#show heading: none\n= Experience\n";
    let rendered = render(source, &facts_en()).await.unwrap();
    assert!(
        rendered.text.metadata.iter().any(|m| m == "PhD Experience"),
        "{:?}",
        rendered.text.metadata
    );
}

/// The review's proofs of finding 18: a baseline can stand inside what shows while its ink doesn't.
#[tokio::test]
async fn glyphs_not_baselines_must_stand_in_what_shows() {
    assert_flagged(&[
        (
            "above the page with its baseline on it",
            Rule::HiddenText,
            "#place(top + left, dy: -1.7cm - 0.2pt)[#text(top-edge: \"baseline\", bottom-edge: \"baseline\")[Software Engineer]]",
        ),
        (
            "a sliver of a clip",
            Rule::HiddenText,
            "#box(width: 8cm, height: 0.5pt, clip: true)[#text(top-edge: \"baseline\", bottom-edge: \"descender\")[Hidden Extra Words]]",
        ),
        (
            "tall but squeezed to nothing",
            Rule::HiddenText,
            "#scale(x: 1%, y: 20%, origin: top + left, reflow: true)[#text(size: 100pt)[Hidden Extra Words]]",
        ),
    ])
    .await;
}

/// The review's proofs of finding 11.
#[tokio::test]
async fn contact_details_only_from_the_facts() {
    assert_flagged(&[
        (
            "a false address behind the true one",
            Rule::ContactDetails,
            "#link(\"mailto:hr-dropbox@evil.example\")[mara\\@example.org]",
        ),
        (
            "a false address as text",
            Rule::ContactDetails,
            "Write to hr-dropbox\\@evil.example.",
        ),
        (
            "an obfuscated address",
            Rule::ContactDetails,
            "Write to hr-dropbox(at)evil.example.",
        ),
        (
            "an address in the true address",
            Rule::ContactDetails,
            "mara\\@example.org.evil.example",
        ),
        (
            "an address in the metadata",
            Rule::ContactDetails,
            "#set document(description: \"hr@evil.example\")",
        ),
        (
            "a false address on a neutral link",
            Rule::ContactDetails,
            "#link(\"mailto:hr@evil.example\")[Write to me]",
        ),
        (
            "a copy to someone else",
            Rule::ContactDetails,
            "#link(\"mailto:mara@example.org?bcc=hr@evil.example\")[mara\\@example.org]",
        ),
        (
            "a false phone target",
            Rule::ContactDetails,
            "#link(\"tel:+4917000\")[Call me]",
        ),
        (
            "a false profile link",
            Rule::ContactDetails,
            "#link(\"https://evil.example/phish\")[github.com/maraexample]",
        ),
        (
            "an http link on the true address",
            Rule::ContactDetails,
            "#link(\"https://evil.example\")[mara\\@example.org]",
        ),
        (
            "a link on the true phone number",
            Rule::ContactDetails,
            "#link(\"https://evil.example\")[+49 170 5550137]",
        ),
        (
            "a mail target on the true profile",
            Rule::ContactDetails,
            "#link(\"mailto:mara@example.org\")[github.com/maraexample]",
        ),
        (
            "a URL with a scheme",
            Rule::ContactDetails,
            "See https://evil.example/apply.",
        ),
        (
            "a www address",
            Rule::ContactDetails,
            "See www.evil.example.",
        ),
        (
            "a host and a path",
            Rule::ContactDetails,
            "See evil.example/apply.",
        ),
        ("a bare host", Rule::ContactDetails, "See apply.evil.com."),
        (
            "in capitals",
            Rule::ContactDetails,
            "#upper[See evil.com/apply.]",
        ),
        (
            "a false profile in text",
            Rule::ContactDetails,
            "See github.com/someone-else.",
        ),
        (
            "a profile with a longer name",
            Rule::ContactDetails,
            "See github.com/maraexample-evil.",
        ),
        (
            "another scheme",
            Rule::ContactDetails,
            "#link(\"sms:555\")[Text me]",
        ),
    ])
    .await;
}

/// Finding 22: a word only of digit lookalikes before `%` or a time unit reads as a number, so it is refused.
#[tokio::test]
async fn digit_lookalikes_before_a_unit_are_refused() {
    assert_flagged(&[
        ("Latin", Rule::DigitLookalike, "Cut errors by lOO%."),
        ("Cyrillic", Rule::DigitLookalike, "Cut errors by ЗО%."),
        ("with a space", Rule::DigitLookalike, "Cut errors by lO %."),
        (
            "the word",
            Rule::DigitLookalike,
            "Cut errors by lOO percent.",
        ),
        ("years", Rule::DigitLookalike, "With lO years in freight."),
        ("German", Rule::DigitLookalike, "Seit lO Jahren dabei."),
    ])
    .await;
}

/// Finding 17: what a source can make the worker send back stays small.
#[tokio::test]
async fn diagnostics_and_text_stay_bounded() {
    let facts = facts_en();
    let Err(RenderError::Diagnostics(diagnostics)) =
        render("#panic(\"x\" * 1000000)\n", &facts).await
    else {
        panic!("should not compile");
    };
    assert!(diagnostics[0].message.chars().count() <= 1001);
    assert!(diagnostics[0].message.len() < 5000);

    let many: String = (0..500).map(|i| format!("#unknown{i}\n")).collect();
    let Err(RenderError::Diagnostics(diagnostics)) = render(&many, &facts).await else {
        panic!("should not compile");
    };
    assert!(diagnostics.len() <= 20, "{}", diagnostics.len());

    let hints = "#assert(false, message: \"x\" * 1000000)\n";
    let Err(RenderError::Diagnostics(diagnostics)) = render(hints, &facts).await else {
        panic!("should not compile");
    };
    assert!(diagnostics.iter().all(|d| d.message.len() < 5000));

    let warnings: String = (0..500)
        .map(|i| format!("#text(font: \"none{i}\")[x]\n"))
        .collect();
    let rendered = render(&warnings, &facts).await.unwrap();
    assert!(rendered.warnings.len() <= 20, "{}", rendered.warnings.len());

    let title = "#set document(title: \"x\" * 2000000)\nWork.\n";
    assert_eq!(
        render(title, &facts).await.unwrap_err(),
        RenderError::TooManyPages
    );
}

/// Previews are limited with the PDF, by the child, so that they never reach the parent.
#[tokio::test]
async fn a_result_over_its_limit_is_refused_in_the_worker() {
    // Printable noise repeated past deflate's window, so that nothing compresses it much.
    let noise = "#let noise = range(65536).map(i => str.from-unicode(33 + calc.rem(i * i * 31 + i * 7 + calc.quo(i * i, 257), 90))).join()\n#let pixels = bytes(noise * 94).slice(0, 6120000)\n#let picture = image(pixels, format: (encoding: \"rgb8\", width: 1200, height: 1700), width: 100%, scaling: \"pixelated\")\n#set page(margin: 0pt)\n#picture\n#pagebreak()\n#picture\n#pagebreak()\n#picture\n#pagebreak()\n#picture\n#pagebreak()\n#picture\n";
    let pages = noise.to_owned();
    let result = Renderer::InProcess
        .render(RenderRequest {
            source: pages,
            facts: Facts::default(),
            pdf: true,
            preview_pages: 5,
            stamp: false,
        })
        .await;
    assert_eq!(result.unwrap_err(), RenderError::ResultTooLarge);
}

/// The check reads the same on the page as `check` does for links it never saw.
#[tokio::test]
async fn links_are_read_with_the_text_they_cover() {
    let facts = facts_en();
    let source = format!("{START}Before #link(\"https://example.org/x\")[the link] after.\n");
    let rendered = render(&source, &facts).await.unwrap();
    let link = rendered
        .text
        .links
        .iter()
        .find(|link| link.target == "https://example.org/x")
        .unwrap();
    assert_eq!(link.text, "the link");
    assert_eq!(check(&rendered.text, &facts, None, "").violations, []);
}

async fn previews_of(source: &str, stamp: bool) -> Vec<Vec<u8>> {
    Renderer::InProcess
        .render(RenderRequest {
            source: source.to_owned(),
            facts: facts_en(),
            pdf: true,
            preview_pages: 3,
            stamp,
        })
        .await
        .unwrap_or_else(|e| panic!("{source}: {e:?}"))
        .previews
}

fn pixels(png_bytes: &[u8]) -> (usize, usize, usize, Vec<u8>) {
    let mut reader = png::Decoder::new(std::io::Cursor::new(png_bytes))
        .read_info()
        .unwrap();
    let mut buffer = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buffer).unwrap();
    (
        info.width as usize,
        info.height as usize,
        info.color_type.samples(),
        buffer,
    )
}

/// How many pixels of each quarter of the page the stamp visibly changes. Compared with the same page
/// unstamped, and not by the stamp's colour, since a source chooses what lies under it.
fn seen_by_quarter(plain: &[u8], stamped: &[u8]) -> [usize; 4] {
    /// Levels of 255 a person can tell apart at a glance.
    const VISIBLE: i32 = 40;
    let (width, height, channels, before) = pixels(plain);
    let (_, _, _, after) = pixels(stamped);
    let mut quarters = [0; 4];
    for y in 0..height {
        for x in 0..width {
            let at = (y * width + x) * channels;
            let moved = (0..3)
                .map(|i| (i32::from(before[at + i]) - i32::from(after[at + i])).abs())
                .max()
                .unwrap();
            if moved > VISIBLE {
                let right = usize::from(x * 2 >= width);
                let lower = usize::from(y * 2 >= height);
                quarters[right + 2 * lower] += 1;
            }
        }
    }
    quarters
}

/// No source can remove or cover the stamp, on any page, because it is drawn after everything the
/// source can reach, from a document of its own. Judged by looking at pixels.
#[tokio::test]
async fn nothing_in_a_source_removes_or_covers_the_stamp() {
    let cover =
        "#place(top + left, dx: -3cm, dy: -3cm, rect(width: 40cm, height: 40cm, fill: white))";
    let attacks = [
        "Work.".to_owned(),
        "#set page(background: none, foreground: none)\nWork.".to_owned(),
        format!("#set page(foreground: {})\nWork.", &cover[1..]),
        format!("#set page(background: {})\nWork.", &cover[1..]),
        format!("Work.\n{cover}"),
        "#set page(fill: white)\nWork.".to_owned(),
        "#set page(fill: red)\nWork.".to_owned(),
        // The stamp's own colour, and what lies either side of it. One colour alone would vanish here.
        "#set page(fill: rgb(200, 16, 16))\nWork.".to_owned(),
        "#set page(fill: rgb(180, 30, 30))\nWork.".to_owned(),
        "#set page(fill: rgb(226, 120, 120))\nWork.".to_owned(),
        "#set page(fill: rgb(120, 120, 120))\nWork.".to_owned(),
        "#set page(fill: black)\nWork.".to_owned(),
        "#show \"DRAFT\": none\n#show \"DRAFT, NOT APPROVED\": none\nWork.".to_owned(),
        "#show regex(\".+\"): none\nWork.".to_owned(),
        "#show text: none\nWork.".to_owned(),
        "#set text(fill: white)\n#show: it => none\n".to_owned(),
        "Page one.\n#pagebreak()\nPage two.\n#pagebreak()\nPage three.".to_owned(),
    ];
    for body in attacks {
        let source = format!("#import \"/nexus.typ\": *\n#show: cv\n{body}\n");
        let plain = previews_of(&source, false).await;
        let stamped = previews_of(&source, true).await;
        assert_eq!(plain.len(), stamped.len(), "{body}");
        for (plain, stamped) in plain.iter().zip(&stamped) {
            let quarters = seen_by_quarter(plain, stamped);
            assert!(
                quarters.iter().all(|seen| *seen > 5),
                "{body}: {quarters:?}"
            );
        }
    }
}

/// The stamp isn't CV text: the check reads what the source says, stamped or not.
#[tokio::test]
async fn the_stamp_is_not_read_as_cv_text() {
    let source = format!("{START}Work.\n");
    let facts = facts_en();
    let read = |stamp| {
        Renderer::InProcess.render(RenderRequest {
            source: source.clone(),
            facts: facts.clone(),
            pdf: true,
            preview_pages: 1,
            stamp,
        })
    };
    let (plain, stamped) = (read(false).await.unwrap(), read(true).await.unwrap());
    assert_eq!(plain.text, stamped.text);
    assert!(!stamped.text.lines.join(" ").contains("DRAFT"));
    assert_ne!(plain.pdf, stamped.pdf);
}

/// What a source can print from `/facts.json` must be what an approval's hash covers.
#[test]
fn a_compile_sees_only_what_a_cv_can_render() {
    let facts = facts_en().renderable();
    assert!(facts.allowed_tokens.is_empty());
    assert!(facts.banned_phrases.is_empty());
    assert!(facts.vocabulary.is_empty());
    assert_eq!(facts.name, facts_en().name);
    assert_eq!(facts.employment, facts_en().employment);
}

/// The review's proofs: each must be listed where the owner reads, and none refused.
#[tokio::test]
async fn the_reviews_false_facts_are_listed_for_the_owner() {
    let mut facts = facts_en();
    facts.employment[0].company = "Brightpath Logistics GmbH".to_owned();
    let cases: &[(&str, &str, &[&str])] = &[
        (
            "finding 1: a short name and false years",
            "*Brightpath* · Software Engineer #h(1fr) 2015 – 2025",
            &["Brightpath", "2015", "2025"],
        ),
        (
            "finding 1: another job's dates and title",
            "*Brightpath Logistics* · Backend Developer #h(1fr) Jan 2016 – Feb 2019",
            &[
                "Brightpath Logistics",
                "backend developer",
                "jan 2016 - feb 2019",
            ],
        ),
        (
            "finding 2: an allowed number in the wrong place",
            "Cut invoice errors by 40% across 12 depots.",
            &["40%", "12"],
        ),
        ("finding 2: tenure", "with 12 years in freight", &["12"]),
        ("finding 2: patents", "6 patents granted.", &["6"]),
        (
            "finding 3: titles swapped",
            "*Brightpath Logistics GmbH* · Backend Developer #h(1fr) 03/2019 – 12/2022\n\n*Quillon Systems* · Software Engineer #h(1fr) Jan 2016 – Feb 2019",
            &["backend developer", "software engineer"],
        ),
        (
            "finding 3: a false place",
            "*Brightpath Logistics GmbH* · Software Engineer · Bremen #h(1fr) 03/2019 – 12/2022",
            &["bremen"],
        ),
        (
            "finding 3: language levels swapped",
            "German (C1) · English (native)",
            &["german (c1", "english (native"],
        ),
        (
            "finding 4: a month without its pattern",
            "Joined in Nov, 2019 and left in Feb, 2025.",
            &["Nov, 2019", "Feb, 2025"],
        ),
        ("finding 4: of", "Started March of 2019", &["March of 2019"]),
        ("finding 4: reversed", "2019 March start", &["2019 March"]),
        (
            "finding 5: lower-case degree",
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
            "finding 5: abbreviation",
            "holds a b.a. in philosophy",
            &["b.a"],
        ),
        (
            "finding 5: German degree",
            "habilitierte sich",
            &["habilitierte"],
        ),
        ("finding 5: seniority", "worked as lead engineer", &["lead"]),
        ("finding 5: manager", "engineering manager", &["manager"]),
        (
            "finding 5: German title",
            "Teamleitung und Projektleitung",
            &["Teamleitung", "Projektleitung"],
        ),
        (
            "finding 5: language",
            "fließendes Französisch",
            &["fließendes"],
        ),
        (
            "finding 5: German language",
            "Spanischkenntnisse",
            &["Spanischkenntnisse"],
        ),
        (
            "finding 10: filler",
            "with eight consecutive years in freight",
            &["eight"],
        ),
        (
            "finding 10: and a half",
            "with two and a half years in freight",
            &["two"],
        ),
        (
            "finding 10: plus",
            "with eight-plus years in freight",
            &["eight-plus"],
        ),
        (
            "finding 10: einhalb",
            "mit zweieinhalb Jahren Erfahrung",
            &["zweieinhalb"],
        ),
        (
            "finding 10: scale",
            "12 Tausend Nutzer.",
            &["12", "Tausend"],
        ),
        ("finding 10: bn", "12 bn EUR revenue.", &["12"]),
        (
            "finding 12: names",
            "worked for Google at Stanford",
            &["Google", "Stanford"],
        ),
    ];
    let mut missed = Vec::new();
    for (name, body, tokens) in cases {
        let source = format!("{START}{body}\n");
        let report = render_and_check(&source, &facts).await;
        if !report.violations.is_empty() {
            missed.push(format!("{name}: refused: {:?}", report.violations));
        }
        let listed: Vec<String> = report
            .review_items
            .iter()
            .map(|item| item.token.to_lowercase())
            .collect();
        for token in *tokens {
            if !listed.iter().any(|t| t == &token.to_lowercase()) {
                missed.push(format!("{name}: no {token}: {listed:?}"));
            }
        }
    }
    assert!(missed.is_empty(), "{}", missed.join("\n"));
}

/// A false phone number would otherwise only be a number in a list: a reply goes to whoever it names.
#[tokio::test]
async fn a_false_phone_number_is_refused_and_the_true_one_is_not() {
    let facts = facts_en();
    for body in [
        "Call +49 170 5550199.",
        "Call 0049 170 5550199.",
        "Call +1 (555) 010-9999.",
    ] {
        let report = render_and_check(&format!("{START}{body}\n"), &facts).await;
        assert!(
            report
                .violations
                .iter()
                .any(|v| v.rule == Rule::ContactDetails),
            "{body}: {:?}",
            report.violations
        );
    }
    for body in [
        "Call +49 170 5550137.",
        "Call 0049 170 5550137.",
        "#contact()",
    ] {
        let report = render_and_check(&format!("{START}{body}\n"), &facts).await;
        assert_eq!(report.violations, [], "{body}");
    }
}

/// A source sets the page's size, so these go out as compile errors: no CV needs them, and a page far
/// longer than wide once left the stamp's tiles nothing to show in.
#[tokio::test]
async fn a_page_no_cv_needs_is_a_compile_error() {
    let long = "Worked on the routing service.\n\n".repeat(120);
    let wide = "Worked on the routing service. ".repeat(60);
    for (name, body) in [
        (
            "tall strip",
            "#set page(width: 150pt, height: 14400pt, margin: 5pt)\nWork.".to_owned(),
        ),
        (
            "wide strip",
            "#set page(width: 14400pt, height: 60pt, margin: 5pt)\nWork.".to_owned(),
        ),
        (
            "A4 wide, very tall",
            "#set page(width: 595pt, height: 60000pt, margin: 40pt)\nWork.".to_owned(),
        ),
        (
            "A3 wide, very tall",
            "#set page(width: 842pt, height: 80000pt, margin: 40pt)\nWork.".to_owned(),
        ),
        (
            "A4 wide, long",
            "#set page(width: 595pt, height: 14400pt, margin: 20pt)\nWork.".to_owned(),
        ),
        (
            "just over the limit",
            "#set page(width: 595pt, height: 1201pt)\nWork.".to_owned(),
        ),
        (
            "just over, sideways",
            "#set page(width: 1201pt, height: 595pt)\nWork.".to_owned(),
        ),
        (
            "large square",
            "#set page(width: 6000pt, height: 6000pt)\nWork.".to_owned(),
        ),
        (
            "tiny",
            "#set page(width: 40pt, height: 40pt, margin: 1pt)\nx".to_owned(),
        ),
        (
            "just under the least",
            "#set page(width: 199pt, height: 595pt, margin: 5pt)\nWork.".to_owned(),
        ),
        (
            "just under, sideways",
            "#set page(width: 595pt, height: 199pt, margin: 5pt)\nWork.".to_owned(),
        ),
        (
            "auto height, run long",
            format!("#set page(height: auto)\n{long}"),
        ),
        (
            "auto height, one line",
            "#set page(height: auto, margin: 1pt)\nWork.".to_owned(),
        ),
        (
            "auto width, run wide",
            format!("#set page(width: auto, height: 400pt)\n{wide}"),
        ),
        (
            "only the last page",
            "Work.\n#pagebreak()\n#set page(height: 3000pt)\nMore.".to_owned(),
        ),
    ] {
        let source = format!("{START}{body}\n");
        match render(&source, &facts_en()).await {
            Err(RenderError::Diagnostics(errors)) => assert!(
                errors.iter().any(|e| e.message.contains("pt on each side")),
                "{name}: {errors:?}"
            ),
            other => panic!(
                "{name}: wanted a compile error, got {:?}",
                other.map(|r| r.text.lines)
            ),
        }
    }
}

/// What remains is stamped on every page, wherever you look. Judged by pixels against the same page
/// unstamped, and each quarter must show a real part of the stamp, not a stray corner of it.
#[tokio::test]
async fn the_stamp_shows_in_every_quarter_at_every_size_left() {
    let text = "Worked on the routing service.\n\n".repeat(40);
    let mut sizes: Vec<String> = [
        (595, 842),   // A4
        (612, 792),   // Letter
        (612, 1008),  // Legal
        (420, 595),   // A5
        (842, 595),   // A4, sideways
        (842, 1191),  // A3
        (1191, 842),  // A3, sideways
        (200, 200),   // the least
        (200, 1200),  // the longest strip left
        (1200, 200),  // and sideways
        (1200, 1200), // the most
        (595, 1200),
        (1200, 595),
        (320, 180 + 20),
        (333, 777),
        (777, 333),
    ]
    .iter()
    .map(|(width, height)| {
        format!("#set page(width: {width}pt, height: {height}pt, margin: 10pt)\nWork.")
    })
    .collect();
    sizes.push(format!("#set page(height: auto)\n{text}"));
    sizes.push(format!(
        "#set page(width: 300pt, height: auto, margin: 10pt)\n{text}"
    ));
    for body in sizes {
        let source = format!("#import \"/nexus.typ\": *\n#show: cv\n{body}\n");
        let plain = previews_of(&source, false).await;
        let stamped = previews_of(&source, true).await;
        assert_eq!(plain.len(), stamped.len(), "{body}");
        for (plain, stamped) in plain.iter().zip(&stamped) {
            let (width, height, _, _) = pixels(plain);
            // A thousandth of a quarter's area: a word of the stamp, not a pixel or two.
            let least = width * height / 4 / 1000;
            let quarters = seen_by_quarter(plain, stamped);
            assert!(
                quarters.iter().all(|seen| *seen > least.max(5)),
                "{body}: {quarters:?}, wanted over {least}"
            );
        }
    }
}

/// Finding 4 of the second gate review: a stroke paints over the glyph it outlines.
#[tokio::test]
async fn a_stroke_cannot_hide_the_text_it_outlines() {
    assert_flagged(&[
        (
            "a white stroke",
            Rule::HiddenText,
            "I am #text(stroke: 2pt + white)[not] certified.",
        ),
        (
            "a thin white stroke",
            Rule::HiddenText,
            "I am #text(stroke: 0.4pt + white)[not] certified.",
        ),
        (
            "near-white",
            Rule::HiddenText,
            "I am #text(stroke: 2pt + luma(95%))[not] certified.",
        ),
        (
            "a stroke of no colour",
            Rule::HiddenText,
            "I am #text(stroke: 2pt + rgb(0, 0, 0, 10))[not] certified.",
        ),
        (
            "a gradient",
            Rule::HiddenText,
            "I am #text(stroke: 2pt + gradient.linear(white, luma(95%)))[not] certified.",
        ),
        (
            "thick and black, a blot",
            Rule::HiddenText,
            "Staff of #text(stroke: 3pt + black)[38] in Kiel.",
        ),
        (
            "thick against a small size",
            Rule::HiddenText,
            "Staff of #text(size: 5pt, stroke: 1pt + black)[38] in Kiel.",
        ),
    ])
    .await;
    for body in [
        "Faux bold #text(stroke: 0.3pt + black)[is] fine.",
        "A coloured outline #text(stroke: 0.5pt + blue)[reads] too.",
    ] {
        let source = format!("{START}{body}\n");
        let report = render_and_check(&source, &facts_en()).await;
        assert_eq!(report.violations, [], "{body}");
    }
}

/// Decorations are shapes drawn after the text, like the white rectangle SPEC lists as a limit. This
/// pins what that limit says: the check still reads the word, and only the stamped picture shows
/// the blank. Close the limit and this should change.
#[tokio::test]
async fn a_shape_over_text_stays_the_limit_spec_lists() {
    for body in [
        "I am #strike(stroke: 14pt + white)[not] certified.",
        "I am #underline(stroke: 14pt + white, offset: -4pt)[not] certified.",
        "I am #overline(stroke: 14pt + white, offset: -8pt)[not] certified.",
        "I am #highlight(fill: black)[not] certified.",
        "I am #place(dx: 1.2em, rect(width: 2em, height: 1em, fill: white))not certified.",
    ] {
        let source = format!("{START}{body}\n");
        let rendered = render(&source, &facts_en()).await.unwrap();
        let report = check(&rendered.text, &facts_en(), None, "");
        assert_eq!(report.violations, [], "{body}");
        assert!(
            rendered
                .text
                .lines
                .iter()
                .any(|l| l.contains("not certified")),
            "{body}"
        );
    }
}

/// Finding 5: tracking short of negative still stacks digits. `1000` draws as `10`, and well before
/// that as a blot a reader takes for other digits. Spread wide, it reads otherwise than it draws too.
#[tokio::test]
async fn tracking_cannot_draw_other_digits_in_either_direction() {
    let mut facts = facts_en();
    facts
        .allowed_tokens
        .extend(["1000", "111", "10"].map(str::to_owned));
    assert_flagged_with(
        &facts,
        &[
            (
                "111 as 1",
                Rule::Transform,
                "Staff of #text(tracking: -0.46em)[111] in Kiel.",
            ),
            (
                "1000 as 10",
                Rule::Transform,
                "Headcount #text(tracking: -0.46em)[1000] people.",
            ),
            (
                "almost stacked",
                Rule::Transform,
                "Headcount #text(tracking: -0.38em)[1000] people.",
            ),
            (
                "1000 as a blot",
                Rule::Transform,
                "Headcount #text(tracking: -0.30em)[1000] people.",
            ),
            (
                "overlapping",
                Rule::Transform,
                "Headcount #text(tracking: -0.25em)[1000] people.",
            ),
            (
                "one digit between runs",
                Rule::Transform,
                "Headcount 1#text(tracking: -0.46em)[00]0 people.",
            ),
            (
                "letters too",
                Rule::Transform,
                "I am #text(tracking: -0.40em)[not] certified.",
            ),
            (
                "through the set rule",
                Rule::Transform,
                "#set text(tracking: -0.46em)\nHeadcount 1000 people.",
            ),
            (
                "spread apart",
                Rule::Transform,
                "Headcount #text(tracking: 2em)[1000] people.",
            ),
            (
                "spread and interleaved",
                Rule::Transform,
                "#text(tracking: 1em)[135]#h(-3em)#text(tracking: 1em)[246]",
            ),
        ],
    )
    .await;
    // Runs and moves that stack digits, which only the overlap between runs can see.
    assert_flagged_with(
        &facts,
        &[
            (
                "a run back over the last",
                Rule::Transform,
                "Headcount #box[10]#h(-0.75em)#box[00] people.",
            ),
            (
                "a move back over the last",
                Rule::Transform,
                "Headcount #box[10]#box(move(dx: -0.75em)[00]) people.",
            ),
        ],
    )
    .await;
}

/// The other things that move glyphs. These read as they draw: one line of each, or what SPEC lists.
#[tokio::test]
async fn honest_tracking_kerning_and_features_pass() {
    for body in [
        "Tight #text(tracking: -0.05em)[1000 people] and #text(tracking: -0.12em)[1000].",
        "Loose #text(tracking: 0.1em)[1000 people] and #text(tracking: 0.5em)[HEADER].",
        "#text(kerning: false)[AVATAR To. 1000]",
        "#text(ligatures: false)[office fly] and office fly",
        "#text(features: (\"onum\",))[0123456789], #text(features: (\"zero\",))[0123456789]",
        "#box[10]#h(-0.1em)#box[00] people",
    ] {
        let source = format!("{START}{body}\n");
        let rendered = render(&source, &facts_en()).await.unwrap();
        assert_eq!(rendered.text.transformed, Vec::<String>::new(), "{body}");
    }
}

/// Finding 10: a link is read by what it touches, and neighbours by what they spell together.
#[tokio::test]
async fn a_link_over_or_around_a_contact_value_is_judged_by_all_of_it() {
    let evil = "https://evil.example";
    assert_flagged(&[
        (
            "a small link placed on the address",
            Rule::ContactDetails,
            &format!("#box[#contact()#place(top + left, link(\"{evil}\", box(width: 1.2cm, height: 1em)))]"),
        ),
        (
            "over the end of the address only",
            Rule::ContactDetails,
            &format!("#box[mara\\@example.org#place(top + right, link(\"{evil}\", box(width: 0.6cm, height: 1em)))]"),
        ),
        (
            "two links with one target",
            Rule::ContactDetails,
            &format!("#link(\"{evil}\")[mara\\@ex]#link(\"{evil}\")[ample.org]"),
        ),
        (
            "two links with two targets",
            Rule::ContactDetails,
            &format!("#link(\"{evil}\")[mara\\@ex]#link(\"https://other.example\")[ample.org]"),
        ),
        (
            "two links over a line break",
            Rule::ContactDetails,
            &format!("#link(\"{evil}\")[mara\\@ex]\\\n#link(\"{evil}\")[ample.org]"),
        ),
        (
            "a phone number in two links",
            Rule::ContactDetails,
            &format!("#link(\"{evil}\")[+49 170]#link(\"{evil}\")[ 5550137]"),
        ),
        (
            "a profile in two links",
            Rule::ContactDetails,
            &format!("#link(\"{evil}\")[github.com/mara]#link(\"{evil}\")[example]"),
        ),
    ])
    .await;
}

/// Findings 11 to 14.
#[tokio::test]
async fn the_low_rules_hold_at_their_edges() {
    let mut facts = facts_en();
    facts
        .allowed_tokens
        .extend(["docs.example.com/guide", "mara.long@example.com"].map(str::to_owned));
    assert_flagged_with(
        &facts,
        &[
            (
                "both words spaced",
                Rule::BannedPhrases,
                "p a s s i o n a t e #h(1em) t e a m #h(1em) p l a y e r",
            ),
            (
                "two of three spaced",
                Rule::BannedPhrases,
                "passionate t e a m #h(1em) p l a y e r",
            ),
            (
                "00 and a space",
                Rule::ContactDetails,
                "Call 00 49 170 5550138.",
            ),
            (
                "+ and a space",
                Rule::ContactDetails,
                "Call + 49 170 5550138.",
            ),
            (
                "the end of an allowed host",
                Rule::ContactDetails,
                "See e.com for more.",
            ),
            (
                "the end of an allowed page",
                Rule::ContactDetails,
                "See ple.com/guide for more.",
            ),
            (
                "the parent of an allowed host",
                Rule::ContactDetails,
                "See example.com for more.",
            ),
            (
                "the end of an allowed address",
                Rule::ContactDetails,
                "Write to long\\@example.com.",
            ),
            (".Net", Rule::ContactDetails, "See evil.Net for more."),
            (".nEt", Rule::ContactDetails, "See evil.nEt for more."),
            (".NEt", Rule::ContactDetails, "See evil.NEt for more."),
        ],
    )
    .await;
    for body in [
        "See docs.example.com/guide and docs.example.com for more.",
        "Write to mara.long\\@example.com.",
        "Call 00 49 170 5550137.",
        "Built on .NET and ASP.NET.",
        "The hugo took a team playing role.",
    ] {
        let source = format!("{START}{body}\n");
        let report = render_and_check(&source, &facts).await;
        assert_eq!(report.violations, [], "{body}");
    }
}
