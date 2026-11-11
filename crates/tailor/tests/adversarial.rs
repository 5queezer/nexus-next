//! Sources that try to get past the gate, with real compiles. Each must be refused, or listed for the
//! owner's review.

mod common;

use common::{facts_en, render, render_and_check};
use nexus_tailor::{
    Contact, Facts, RenderError, RenderRequest, Renderer, Rule, Severity, check, default_source,
};

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

/// A bleed widens the PDF's sheet beyond the page, and a viewer shows all of it: no picture, no stamp
/// and no page size reaches there. Set before `#show: cv`, it outlives the template's own page setup.
#[tokio::test]
async fn a_bleed_is_a_compile_error() {
    let word = "#for i in range(8) { place(top + left, dx: -80pt + i * 6pt, dy: 100pt, rect(width: 4pt, height: 30pt, fill: black)) }";
    for (name, source) in [
        (
            "shapes in a bleed set before the template",
            format!(
                "#import \"/nexus.typ\": *\n#set page(bleed: 100pt)\n#show: cv\n#header()\n{word}\n"
            ),
        ),
        (
            "shapes in a bleed set after it",
            format!("{START}#set page(bleed: 100pt)\n{word}\n"),
        ),
        (
            "a bleed past the largest page",
            "#import \"/nexus.typ\": *\n#set page(bleed: 1500pt)\n#show: cv\n#header()\n"
                .to_owned(),
        ),
        (
            "on one side",
            format!("{START}#set page(bleed: (left: 0.01pt))\nWork.\n"),
        ),
        (
            "only the last page",
            format!("{START}Work.\n#pagebreak()\n#set page(bleed: 3mm)\nMore.\n"),
        ),
    ] {
        match render(&source, &facts_en()).await {
            Err(RenderError::Diagnostics(errors)) => assert!(
                errors.iter().any(|e| e.message.contains("has a bleed")),
                "{name}: {errors:?}"
            ),
            other => panic!(
                "{name}: wanted a compile error, got {:?}",
                other.map(|r| r.text.lines)
            ),
        }
    }
    let none = format!("{START}#set page(bleed: 0pt)\nWork.\n");
    assert!(render(&none, &facts_en()).await.is_ok());
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

/// The owner's decision: text covered by a shape is refused, whatever the shape, and wherever in the
/// source or the page it comes from. Black on black from beneath is the same trick the other way up.
#[tokio::test]
async fn text_covered_by_a_shape_is_refused() {
    let word = "I am not certified.";
    // Painted after the word, over it.
    let after =
        |shape: &str| format!("I am not#box(place(dx: -1.6em, dy: -0.8em, {shape})) certified.");
    let cases: Vec<(&str, String)> = vec![
        ("strike white", "I am #strike(stroke: 14pt + white)[not] certified.".into()),
        ("strike black", "I am #strike(stroke: 14pt + black)[not] certified.".into()),
        ("strike red", "I am #strike(stroke: 14pt + red)[not] certified.".into()),
        (
            "underline white",
            "I am #underline(stroke: 14pt + white, offset: -3pt, evade: false)[not] certified.".into(),
        ),
        ("overline white", "I am #overline(stroke: 14pt + white, offset: -9pt)[not] certified.".into()),
        (
            "place rect white",
            after("rect(width: 2em, height: 1.1em, fill: white)"),
        ),
        (
            "place rect black",
            after("rect(width: 2em, height: 1.1em, fill: black)"),
        ),
        (
            "rect in the page's colour",
            format!("#set page(fill: rgb(\"#fbf6e9\"))\n{}", after("rect(width: 2em, height: 1.1em, fill: rgb(\"#fbf6e9\"))")),
        ),
        (
            "a later place from elsewhere on the page",
            format!("{word}\n#lorem(30)\n#place(top + left, dx: 0pt, dy: 0pt, rect(width: 100%, height: 30%, fill: white))"),
        ),
        ("painted in a box", after("box(rect(width: 2em, height: 1.1em, fill: white))")),
        (
            "under an allowed rotate",
            after("rotate(360deg, rect(width: 2em, height: 1.1em, fill: white))"),
        ),
        (
            "under a scale",
            after("scale(200%, origin: top + left, rect(width: 1em, height: 0.55em, fill: white))"),
        ),
        (
            "a rotated bar across the word",
            after("rotate(5deg, rect(width: 2em, height: 1.1em, fill: white))"),
        ),
        (
            "a page foreground",
            format!("#set page(foreground: place(top + left, rect(width: 100%, height: 40%, fill: white)))\n{word}"),
        ),
        ("a line", after("move(dy: 0.5em, line(length: 2em, stroke: 12pt + white))")),
        (
            "a curve",
            after("curve(fill: white, curve.move((0pt, 0pt)), curve.line((24pt, 0pt)), curve.line((24pt, 12pt)), curve.line((0pt, 12pt)), curve.close())"),
        ),
        (
            "a polygon",
            after("polygon(fill: white, (0pt, 0pt), (24pt, 0pt), (24pt, 12pt), (0pt, 12pt))"),
        ),
        (
            "a gradient",
            after("rect(width: 2em, height: 1.1em, fill: gradient.linear(white, luma(95%)))"),
        ),
        (
            "a tiling",
            after("rect(width: 2em, height: 1.1em, fill: tiling(size: (4pt, 4pt), rect(width: 4pt, height: 4pt, fill: white)))"),
        ),
        (
            "a dashed stroke",
            "I am #strike(stroke: (paint: white, thickness: 14pt, dash: \"dashed\"))[not] certified.".into(),
        ),
        ("a stroke with a gap", after("line(length: 2em, stroke: (paint: white, thickness: 12pt, dash: (2pt, 0.5pt)))")),
        ("text over text", "I am not#box(place(dx: -1.5em, dy: -1.3em, text(size: 40pt, fill: black)[■])) certified.".into()),
        (
            "translucent layers that add up",
            after(&["rect(width: 2em, height: 1.1em, fill: rgb(255, 255, 255, 60%))"; 4].map(|layer| format!("place({layer})")).join(" + ")),
        ),
        // Getting past the rule.
        (
            "many small shapes",
            after(&(0..16).map(|i| format!("place(dy: {}em, rect(width: 2em, height: 0.08em, fill: white))", f64::from(i) * 0.07)).collect::<Vec<_>>().join(" + ")),
        ),
        (
            "a grid of small squares",
            after(&(0..50).flat_map(|x| (0..16).map(move |y| format!("place(dx: {}em, dy: {}em, square(size: 0.05em, fill: white))", f64::from(x) * 0.04, f64::from(y) * 0.07))).collect::<Vec<_>>().join(" + ")),
        ),
        (
            "the last letter gone: not as no",
            "I am not#box(place(dx: -0.35em, dy: -0.8em, rect(width: 0.35em, height: 1.1em, fill: white))) certified.".into(),
        ),
        (
            "half of it, from the right",
            "I am not#box(place(dx: -0.15em, dy: -0.8em, rect(width: 0.15em, height: 1.1em, fill: white))) certified.".into(),
        ),
        (
            "its top half",
            "I am not#box(place(dx: -0.35em, dy: -0.8em, rect(width: 0.35em, height: 0.55em, fill: white))) certified.".into(),
        ),
        (
            "half of it under a black bar, and most of the next",
            "I am not#box(place(dx: -0.35em, dy: -0.8em, rect(width: 0.35em, height: 1.1em, fill: black))) certified.".into(),
        ),
        (
            "E as F, the eraser taking little",
            "Grade #box[E#place(bottom + left, dy: 0.02em, rect(width: 0.6em, height: 0.09em, fill: white))].".into(),
        ),
        (
            "a dot erased from a number",
            "Rated 4#box[.#place(top + left, dy: 0.5em, rect(width: 0.3em, height: 0.2em, fill: white))]9 stars.".into(),
        ),
        (
            "an umlaut erased: schön as schon",
            "Sehr sch#box[ö#place(top + left, dy: -0.1em, rect(width: 0.6em, height: 0.25em, fill: white))]n.".into(),
        ),
        // The shape beneath.
        ("black on a black highlight", "I am #highlight(fill: black)[not] certified.".into()),
        (
            "dark on a dark band",
            "#block(fill: rgb(\"#222\"), inset: 3pt)[I am #text(fill: rgb(\"#111\"))[not] certified.]".into(),
        ),
        (
            "text of the band's colour",
            "#block(fill: rgb(\"#1f3a5f\"), inset: 3pt)[I am #text(fill: rgb(\"#1f3a5f\"))[not] certified.]".into(),
        ),
        (
            "text of the page's colour",
            "#set page(fill: rgb(\"#303030\"))\n#text(fill: rgb(\"#303030\"))[not] certified.".into(),
        ),
        (
            "on a tiling",
            "#block(fill: tiling(size: (4pt, 4pt), rect(width: 4pt, height: 4pt, fill: black)), inset: 3pt)[not certified]".into(),
        ),
    ];
    let cases: Vec<(&str, Rule, &str)> = cases
        .iter()
        .map(|(name, body)| (*name, Rule::HiddenText, body.as_str()))
        .collect();
    assert_flagged(&cases).await;
}

/// The fourth gate review's findings 1 to 5: each is a place where a copy of the renderer differed from
/// the renderer. The check now compares what is drawn, so none of these has a rule of its own.
#[tokio::test]
async fn what_is_drawn_decides_not_a_model_of_the_paint() {
    let laundered = |glyph: &str, at: &str| {
        format!(
            "#box[{glyph}#place(top + left, {at}, fill: white))#place(top + left, {at}, fill: rgb(0, 0, 0, 0)))]"
        )
    };
    let cases: Vec<(&str, String)> = vec![
        (
            "a tiny rect's mitered stroke",
            "I am #box[not#place(top + left, dx: 8pt, dy: 3.5pt, rect(width: 0.01pt, height: 0.01pt, stroke: 15pt + white))] certified.".into(),
        ),
        (
            "a tiny rect's round stroke",
            "I am #box[not#place(top + left, dx: 8pt, dy: 3.5pt, rect(width: 0.01pt, height: 0.01pt, stroke: (paint: white, thickness: 15pt, join: \"round\")))] certified.".into(),
        ),
        (
            "a long miter's tip, clipped to the word",
            "I am #box[not#place(top + left, box(clip: true, width: 17pt, height: 9pt, place(top + left, curve(stroke: (paint: white, thickness: 16pt, join: \"miter\", miter-limit: 200), curve.move((-150pt, -3pt)), curve.line((-9pt, 4pt)), curve.line((-150pt, 11pt))))))] certified.".into(),
        ),
        (
            "a huge circle's flat edge, clipped to the word",
            "I am #box[not#place(top + left, box(clip: true, width: 17pt, height: 9pt, place(top + left, dx: -3857pt, dy: -20064pt, circle(radius: 20000pt, fill: white))))] certified.".into(),
        ),
        (
            "a tiny polygon's stroke",
            "I am #box[not#place(top + left, dx: 8pt, dy: 3.5pt, polygon(stroke: 16pt + white, (0pt, 0pt), (0.01pt, 0pt), (0.01pt, 0.01pt)))] certified.".into(),
        ),
        (
            "a page foreground from anywhere",
            "#set page(foreground: place(top + left, dx: 80pt, dy: 60pt, rect(width: 0.01pt, height: 0.01pt, stroke: 90pt + white)))\nI am not certified.".into(),
        ),
        (
            "a dot under a transparent paint",
            format!("Grew 4{}9 times.", laundered(".", "dx: -1pt, dy: 0pt, rect(width: 4pt, height: 8pt")),
        ),
        (
            "a minus sign under a transparent paint",
            format!("Margin {}5 percent.", laundered("−", "dx: -0.5pt, dy: 0pt, rect(width: 9pt, height: 9pt")),
        ),
        (
            "a dollar sign under a transparent paint",
            format!("Saved {}500.", laundered("\\$", "dx: -0.5pt, dy: -2pt, rect(width: 6.5pt, height: 12pt")),
        ),
        (
            "8 as 3 under a transparent paint",
            format!("Saved {}00.", laundered("8", "dx: -0.5pt, dy: -1pt, rect(width: 2.3pt, height: 9pt")),
        ),
        (
            "black under white over a dot",
            "Grew 4#box[#place(top + left, dx: 0.2pt, dy: 4.6pt, rect(width: 1.8pt, height: 2.6pt, fill: black)).#place(top + left, dx: 0.2pt, dy: 4.6pt, rect(width: 1.8pt, height: 2.6pt, fill: white))]9 times.".into(),
        ),
        (
            "transparent stops over a black bar",
            "I am #box(fill: black, outset: 2pt)[#box(fill: gradient.linear(rgb(255, 255, 255, 0), rgb(255, 255, 255, 0)), outset: 1pt)[not]] certified.".into(),
        ),
        (
            "text of a gradient's middle colour",
            "Mark: #box(width: 300pt, height: 11pt, baseline: 2pt, fill: gradient.linear(white, black, space: rgb), align(center + horizon, text(fill: rgb(128, 128, 128))[not])) here".into(),
        ),
        (
            "pale yellow over a dot",
            "Grew 4#box[.#place(top + left, dx: -0.2pt, dy: 4pt, rect(width: 2.4pt, height: 3.2pt, fill: rgb(255, 255, 140)))]9 times.".into(),
        ),
        (
            "A's crossbar",
            "Graded #text(size: 11pt, kerning: false)[A]#box(width: 0pt, height: 0pt, place(top + left, dx: -7pt, dy: -2.9pt, rect(width: 5.3pt, height: 0.9pt, fill: white))) here.".into(),
        ),
        (
            "an eraser inside a frame a reader can see",
            "Saved #box(stroke: 1pt + black, outset: 2pt)[8#place(top + left, dx: -0.5pt, dy: -1pt, rect(width: 2.3pt, height: 9pt, fill: white))]00.".into(),
        ),
        (
            "a black bar on black text, taking half a digit",
            "Saved #box[8#place(top + left, dx: -0.5pt, dy: -1pt, rect(width: 2.3pt, height: 9pt, fill: black))]00.".into(),
        ),
        (
            "a grey bar a reader can see, reading 8 as 3",
            "Saved #box[8#place(top + left, dx: -0.5pt, dy: -1pt, rect(width: 2.2pt, height: 9pt, fill: luma(60%)))]00.".into(),
        ),
    ];
    let cases: Vec<(&str, Rule, &str)> = cases
        .iter()
        .map(|(name, body)| (*name, Rule::HiddenText, body.as_str()))
        .collect();
    assert_flagged(&cases).await;
}

/// A PDF viewer widens a hairline to a whole pixel: poppler at 144 dpi leaves nothing of a word under
/// these, while `typst_render` leaves it plainly readable. So the check draws them as such a viewer does.
/// Thinner than these, nothing is drawn at all: see `nothing_is_drawn_thinner_than_the_picture_shows`.
#[tokio::test]
async fn hairlines_are_judged_as_the_harshest_viewer_draws_them() {
    let over = |dx: f64, dy: f64, shapes: String| {
        format!("#v({dy}pt)I am#h({dx}pt) #box[not{shapes}] certified.")
    };
    let mut cases = Vec::new();
    // Wherever on the pixel grid they fall. 1 pt apart a word is chopped and still reads, in poppler too,
    // so that is SPEC's limit.
    for offset in 0..6 {
        let (dx, dy) = (f64::from(offset) * 0.09, f64::from(offset) * 0.13);
        cases.push((
            "hairlines 0.5 pt apart",
            over(dx, dy, "#for i in range(60) { place(top + left, dx: i * 0.5pt - 1pt, dy: -1pt, line(angle: 90deg, length: 10pt, stroke: (paint: white, thickness: 0.25pt))) }".into()),
        ));
        cases.push((
            "a dash finer than a pixel",
            over(dx, dy, "#place(top + left, dy: 4pt, line(length: 20pt, stroke: (paint: white, thickness: 10pt, dash: (0.25pt, 0.25pt))))".into()),
        ));
        cases.push((
            "a thin white stroke on the text",
            format!("#v({dy}pt)I am#h({dx}pt) #text(stroke: 0.25pt + white)[not] certified."),
        ));
    }
    let cases: Vec<(&str, Rule, &str)> = cases
        .iter()
        .map(|(name, body)| (*name, Rule::HiddenText, body.as_str()))
        .collect();
    assert_flagged(&cases).await;
}

async fn assert_too_thin(cases: &[(&str, String)]) {
    for (name, body) in cases {
        let source = format!("{START}{body}\n");
        match render(&source, &facts_en()).await {
            Err(RenderError::Diagnostics(errors)) => {
                let first = &errors[0];
                assert!(
                    first.message.contains("than the picture shows"),
                    "{name}: {errors:?}"
                );
                assert_eq!(first.file.as_deref(), Some("/main.typ"), "{name}");
            }
            other => panic!(
                "{name}: wanted a compile error, got {:?}",
                other.map(|r| r.text.hidden)
            ),
        }
    }
}

/// A PDF viewer draws the thinnest line a whole pixel wide, and the picture the owner approves by how
/// little of a pixel it covers. Measured: letters of lines 0.001 pt thick are on no picture, solid black
/// in Ghostscript and half grey in poppler. So nothing may draw thinner than the picture shows.
#[tokio::test]
async fn nothing_is_drawn_thinner_than_the_picture_shows() {
    let letters = |thickness: &str| {
        format!(
            "#for i in range(80) {{ place(top + left, dx: i * 0.25pt, line(angle: 90deg, length: 20pt, stroke: {thickness} + black)) }}"
        )
    };
    let mesh = "#for i in range(24) { place(top + left, dx: -1pt, dy: i * 0.5pt - 1pt, line(length: 19pt, stroke: 0.001pt + black)) }";
    let flat = "#for i in range(24) { place(top + left, dx: -1pt, dy: i * 0.5pt - 1pt, polygon(fill: black, (0pt, 0pt), (19pt, 0pt), (0pt, 0pt))) }";
    assert_too_thin(&[
        ("letters of lines 0.001 pt thick", letters("0.001pt")),
        ("of lines 0.01 pt thick", letters("0.01pt")),
        ("of lines 0.1 pt thick", letters("0.1pt")),
        ("just under half a pixel", letters("0.24pt")),
        (
            "of thin filled rectangles",
            "#for i in range(80) { place(top + left, dx: i * 0.5pt, rect(width: 0.001pt, height: 20pt, fill: black)) }".into(),
        ),
        (
            "a rectangle that can fall between two pixels",
            "#rect(width: 100pt, height: 0.4pt, fill: black)".into(),
        ),
        (
            "specks",
            "#for x in range(42) { for y in range(22) { place(top + left, dx: x * 0.5pt, dy: y * 0.5pt, rect(width: 0.05pt, height: 0.05pt, fill: black)) } }".into(),
        ),
        (
            "a slanted fill of no area",
            "#polygon(fill: black, (0pt, 0pt), (40pt, 30pt), (40pt, 30pt), (0pt, 0pt))".into(),
        ),
        (
            "a slanted sliver",
            "#polygon(fill: black, (0pt, 0pt), (40pt, 30pt), (40pt, 30.1pt), (0pt, 0.1pt))".into(),
        ),
        (
            "a sliver beside a wide part",
            "#curve(fill: black, curve.line((20pt, 0pt)), curve.line((20pt, 20pt)), curve.close(), curve.move((30pt, 0pt)), curve.line((60pt, 30pt)), curve.line((60pt, 30.01pt)), curve.close())".into(),
        ),
        (
            "dashes too short to show",
            "#line(length: 40pt, stroke: (thickness: 10pt, dash: (0.01pt, 0.5pt)))".into(),
        ),
        (
            "dashes of no length",
            "#line(length: 40pt, stroke: (thickness: 10pt, dash: (0pt, 0.5pt)))".into(),
        ),
        (
            "a dash an odd count draws",
            "#line(length: 40pt, stroke: (thickness: 10pt, dash: (2pt, 1pt, 0.1pt)))".into(),
        ),
        (
            "a hairline around letters",
            "#text(stroke: 0.01pt + red)[Work.]".into(),
        ),
        (
            "a stroke of no width around letters",
            "#text(stroke: 0pt + red)[Work.]".into(),
        ),
        (
            "in a tiling",
            "#rect(width: 40pt, height: 20pt, fill: tiling(size: (9pt, 9pt), line(length: 9pt, stroke: 0.01pt)))".into(),
        ),
        (
            "made thin by a scale",
            "#scale(50%, line(length: 40pt, stroke: 0.4pt))".into(),
        ),
        (
            "thin for a large page's coarser picture",
            "#set page(width: 1200pt, height: 1200pt)\n#line(length: 40pt, stroke: 0.3pt)".into(),
        ),
        (
            "white text on a mesh no picture has",
            format!("I am #box[{mesh}#text(fill: white)[not]] certified."),
        ),
        (
            "white text on fills of no area",
            format!("I am #box[{flat}#text(fill: white)[not]] certified."),
        ),
        (
            "white hairlines over a word",
            "I am #box[not#for i in range(60) { place(top + left, dx: i * 0.25pt - 1pt, dy: -1pt, line(angle: 90deg, length: 10pt, stroke: 0.05pt + white)) }] certified.".into(),
        ),
    ])
    .await;
}

/// The rules, bars and frames a CV draws are at the floor or over it, and stay allowed. A stroke or a
/// rectangle of no width draws nothing in any viewer measured.
#[tokio::test]
async fn ordinary_rules_are_thick_enough() {
    for body in [
        "#line(length: 100%, stroke: 0.25pt)\n#line(length: 100%, stroke: 0.4pt)\n#line(length: 100%, stroke: 0.5pt)\n#line(length: 100%)",
        "#table(columns: 2, stroke: 0.4pt, [Rust], [5 years], [Go], [2 years])",
        "#rect(width: 100%, height: 0.5pt, fill: black)\n#rect(width: 2pt, height: 20pt, fill: navy)",
        "#text(size: 8pt)[#underline[Under] #strike[struck] #overline[over]] $ a / b + sqrt(x) $",
        "#text(font: \"Inter\", size: 7pt)[#underline[Under] #strike[struck] #overline[over]]",
        "#line(length: 100%, stroke: (thickness: 0.3pt, dash: \"dotted\"))\n#line(length: 100%, stroke: (thickness: 1pt, dash: \"densely-dash-dotted\"))",
        "#line(length: 100%, stroke: (thickness: 1pt, cap: \"round\", dash: (0pt, 3pt)))",
        "#line(length: 100%, stroke: 0pt)\n#rect(width: 0pt, height: 10pt, fill: black)\n#box(fill: red)[] #highlight[]",
        "#circle(radius: 1.5pt, fill: black) #ellipse(width: 10pt, height: 1pt, fill: black) #box(fill: luma(90%), radius: 2pt)[Pill]",
        "#text(stroke: 0.3pt + navy)[Outlined]",
        "#scale(200%, origin: top + left, line(length: 40pt, stroke: 0.15pt))",
        "#polygon(fill: black, (0pt, 0pt), (40pt, 30pt), (40pt, 31pt), (0pt, 1pt))",
    ] {
        let source = format!("{START}{body}\n");
        let rendered = render(&source, &facts_en()).await;
        assert!(rendered.is_ok(), "{body}: {:?}", rendered.err());
    }
}

/// What can't be checked in its budget is refused, and says so truly, not as a page limit.
#[tokio::test]
async fn ink_too_large_to_check_is_refused_as_hidden() {
    assert_flagged(&[(
        "page-sized glyphs",
        Rule::HiddenText,
        "#set page(width: 1200pt, height: 1200pt, margin: 0pt)\n#for i in range(40) { place(text(size: 1100pt)[M]) }",
    )])
    .await;
}

/// Another run over a run can read as anything, whatever its colour. A mark on its own digits can't.
/// All wherever on the pixel grid they fall, as a small glyph is only a few pixels.
#[tokio::test]
async fn text_over_text_counts_by_its_outline() {
    let mut attacks = Vec::new();
    for offset in 0..12 {
        let at = format!(
            "#v({}pt)#h({}pt)",
            f64::from(offset) * 0.11,
            f64::from(offset) * 0.07
        );
        for (name, rule, body) in [
            (
                "8 over 3, black on black",
                Rule::HiddenText,
                "Rated #box[3#place(top + left, text[8])] here.",
            ),
            // Under the limit by its outline at some offsets, and refused as drawn over on its line.
            (
                "7 over 1",
                Rule::Transform,
                "Rated #box[1#place(top + left, text[7])] here.",
            ),
            (
                "the same word on itself",
                Rule::HiddenText,
                "Rated #box[8#place(top + left, text[8])] here.",
            ),
            (
                "a white copy over a word",
                Rule::HiddenText,
                "I am #box[not#place(top + left, text(fill: white)[not])] certified.",
            ),
        ] {
            attacks.push((name, rule, format!("{at}{body}")));
        }
    }
    let attacks: Vec<(&str, Rule, &str)> = attacks
        .iter()
        .map(|(name, rule, body)| (*name, *rule, body.as_str()))
        .collect();
    assert_flagged(&attacks).await;
    for offset in 0..12 {
        let (dx, dy) = (f64::from(offset) * 0.07, f64::from(offset) * 0.11);
        for body in [
            "Cube root: $root(3, 27)$ and $sqrt(2)$ here.",
            "Next.\n$ arrow(2) + hat(3) $\nThen.",
            "Half: $1/2$ and\n$ 1/2 + 3^2 = 9.25 $",
        ] {
            let source = format!("{START}#v({dy}pt)#h({dx}pt){body}\n");
            let report = render_and_check(&source, &facts_en()).await;
            let hidden: Vec<_> = report
                .violations
                .iter()
                .filter(|violation| violation.rule == Rule::HiddenText)
                .collect();
            assert_eq!(hidden, Vec::<&_>::new(), "{offset} {body}");
        }
    }
}

/// The owner's decision: text is judged by whether a reader can see it against what it stands on.
#[tokio::test]
async fn light_text_is_judged_against_what_it_stands_on() {
    let on = |ground: &str, ink: &str| {
        format!(
            "#block(fill: rgb(\"{ground}\"), inset: 6pt)[#text(fill: rgb(\"{ink}\"))[Readable words 2019]]"
        )
    };
    let bold =
        |ink: &str| format!("#text(fill: rgb(\"{ink}\"), weight: \"bold\")[Experience 2019]");
    let passing = vec![
        bold("#7dd3fc"),
        bold("#4ade80"),
        on("#0f766e", "#99f6e4"),
        on("#1f3a5f", "#ffffff"),
        on("#1f3a5f", "#dfe7f2"),
        on("#222222", "#ffffff"),
        on("#475569", "#ffffff"),
        on("#1e293b", "#e2e8f0"),
        "#box(circle(radius: 6pt, fill: navy, align(center + horizon, text(fill: white, size: 8pt)[in]))) linked".to_owned(),
        "#table(columns: 2, fill: (x, y) => if y == 0 { rgb(\"#1f3a5f\") }, table.header(text(fill: white)[Skill], text(fill: white)[Years]), [Rust], [5])".to_owned(),
        "#text(fill: rgb(\"#aaa\"))[Muted 2019]".to_owned(),
        "#text(fill: rgb(\"#bbb\"))[Muted 2019]".to_owned(),
        bold("#d4a017"),
        bold("#eab308"),
        bold("#fb923c"),
        bold("#06b6d4"),
    ];
    for body in &passing {
        let source = format!("{START}{body}\n");
        let report = render_and_check(&source, &facts_en()).await;
        let hidden: Vec<_> = report
            .violations
            .iter()
            .filter(|violation| violation.rule == Rule::HiddenText)
            .collect();
        assert_eq!(hidden, Vec::<&_>::new(), "{body}");
    }
    let refused = [
        (
            "pale yellow",
            "I am #text(fill: rgb(255, 255, 140))[not] certified.",
        ),
        (
            "pure yellow",
            "I am #text(fill: rgb(\"#ffff00\"))[not] certified.",
        ),
        ("#ccc", "I am #text(fill: rgb(\"#ccc\"))[not] certified."),
        (
            "one step off white",
            "I am #text(fill: rgb(\"#fefefe\"))[not] certified.",
        ),
        (
            "one step off navy",
            "#block(fill: rgb(\"#1f3a5f\"), inset: 6pt)[#text(fill: rgb(\"#1f3b60\"))[not certified]]",
        ),
        (
            "one step off red",
            "#block(fill: rgb(\"#c0392b\"), inset: 6pt)[#text(fill: rgb(\"#c13a2c\"))[not certified]]",
        ),
        (
            "near grey on grey",
            "#block(fill: rgb(\"#808080\"), inset: 6pt)[#text(fill: rgb(\"#8a8a8a\"))[not certified]]",
        ),
        (
            "dark on nearly as dark",
            "#block(fill: rgb(\"#222\"), inset: 6pt)[#text(fill: rgb(\"#2a2a2a\"))[not certified]]",
        ),
        (
            "a thick pale stroke",
            "I am #text(stroke: 1pt + luma(85%))[not] certified.",
        ),
    ];
    let refused: Vec<(&str, Rule, &str)> = refused
        .iter()
        .map(|(name, body)| (*name, Rule::HiddenText, *body))
        .collect();
    assert_flagged(&refused).await;
}

/// Finding 8: a letter the fonts lack draws as a crossed box while the text reads as the letter.
#[tokio::test]
async fn a_letter_the_fonts_lack_is_refused_and_says_which() {
    assert_flagged(&[
        ("Chinese", Rule::MissingGlyph, "I am 非 certified."),
        ("Arabic", Rule::MissingGlyph, "I am ليس certified."),
        ("Devanagari", Rule::MissingGlyph, "I am नहीं certified."),
        ("a letter in math", Rule::MissingGlyph, "Model: $x$ here."),
        (
            "a name in Chinese",
            Rule::MissingGlyph,
            "Worked with 李雷 here.",
        ),
        (
            "a name in Arabic",
            Rule::MissingGlyph,
            "Worked with محمد here.",
        ),
        ("numerals in Chinese", Rule::MissingGlyph, "Since 二〇一五."),
        ("Arabic digits", Rule::MissingGlyph, "مرحبا ٢٠١٩"),
    ])
    .await;
    let source = format!("{START}I am 非 certified.\n");
    let report = render_and_check(&source, &facts_en()).await;
    assert!(
        report
            .violations
            .iter()
            .any(|violation| violation.rule == Rule::MissingGlyph && violation.token == "非"),
        "{:?}",
        report.violations
    );
    for body in [
        "Ελληνικά and Русский, each in its own word.",
        "שלום עולם",
        "Müller, Núñez, Čapek, façade.",
        "#text(font: \"No Such Font\")[Falls back to an embedded font.]",
        "#text(weight: \"bold\", style: \"italic\")[Bold italic.]",
        "Digits in math: $1/2 + 3^2$.",
    ] {
        let source = format!("{START}{body}\n");
        let report = render_and_check(&source, &facts_en()).await;
        let missing: Vec<_> = report
            .violations
            .iter()
            .filter(|violation| violation.rule == Rule::MissingGlyph)
            .collect();
        assert_eq!(missing, Vec::<&_>::new(), "{body}");
    }
}

/// The review's realistic designs: a false refusal here is a defect.
#[tokio::test]
async fn honest_designs_stay_allowed_however_they_are_drawn() {
    let cases = [
        "#grid(columns: (30%, 1fr), gutter: 12pt, block(fill: rgb(\"#1f3a5f\"), inset: 8pt, width: 100%, height: 200pt)[#text(fill: white)[Skills\\ Rust\\ Go]], [Body text beside it.])",
        "#block(fill: rgb(\"#222\"), inset: 10pt, width: 100%)[#text(fill: white, size: 20pt)[Mara Example]]",
        "#block(fill: gradient.linear(rgb(\"#1f3a5f\"), white), inset: 10pt, width: 100%)[#h(1fr)#text(size: 20pt)[Mara]]",
        "#table(columns: 3, inset: 1pt, stroke: 0.5pt, [Rust], [5], [Expert], [Go], [2], [gypsy quay])",
        "#table(columns: 2, inset: 3pt, stroke: 2pt + navy, [Rust], [5 years], [Go], [2 years])",
        "#table(columns: 3, inset: 1pt, stroke: 0.25pt, [Rust gypsy], [5 years], [Expert quay])",
        "#table(columns: 3, fill: luma(90%), stroke: 0.25pt + white, inset: 1pt, [Rust gypsy], [5], [quay])",
        "#underline(stroke: 0.25pt)[Hairline gypsy] and #strike(stroke: 0.25pt)[hairline 2019]",
        "Line above\n#line(length: 100%, stroke: 0.3pt)\nLine below\n#v(-0.5em)\n#line(length: 100%, stroke: 0.25pt)\nAnd under",
        "Skill #box(width: 1fr, repeat[.]) Expert\n#line(length: 100%, stroke: (dash: \"dotted\", thickness: 0.3pt))\nNext",
        "#box(fill: luma(90%))[Tight gypsy] #box(fill: rgb(\"#dbeafe\"), radius: 2pt)[Pill quay] #box(width: 40pt, height: 6pt, fill: navy) after",
        "#set par(leading: 0em)\nTight gypq lines of text.\\\nOn top of gypq each other.\\\nThird of gypq them.",
        "#text(size: 7pt)[Small type 2019, gypsy quay.] #text(size: 5pt, fill: rgb(\"#55606e\"))[Smaller still 2022.]",
        "#text(size: 8pt)[#strike(stroke: 1pt + red)[Struck 2019]]",
        "#underline(evade: false, offset: 1pt)[Tight gypsy quay]",
    ];
    let mut refused = Vec::new();
    for body in cases {
        let source = format!("{START}{body}\n");
        let report = render_and_check(&source, &facts_en()).await;
        let hidden: Vec<_> = report
            .violations
            .iter()
            .filter(|violation| violation.rule == Rule::HiddenText)
            .collect();
        if !hidden.is_empty() {
            refused.push(format!("{body}\n  {hidden:?}"));
        }
    }
    assert!(refused.is_empty(), "{}", refused.join("\n"));
}

/// What a normal CV draws must stay allowed, however close to its text it draws it.
#[tokio::test]
async fn ordinary_lines_fills_and_borders_near_text_stay_allowed() {
    let all =
        "abcdefghijklmnopqrstuvwxyz ABCDEFGHIJKLMNOPQRSTUVWXYZ 0123456789 .,;:!?-–—\\_=+()/%\\@";
    let cases = [
        format!("#underline[{all}]"),
        format!("#underline(evade: false)[{all}]"),
        format!("#strike[{all}]"),
        format!("#overline[{all}]"),
        format!("#strike(stroke: 1pt + red)[{all}]"),
        format!("#underline(stroke: 1.5pt + blue)[{all}]"),
        format!("#text(size: 18pt, weight: \"bold\")[#strike[{all}]]"),
        format!("#underline(stroke: 0.5pt + luma(80%))[{all}]"),
        format!("#overline(stroke: 0.5pt + luma(80%))[{all}]"),
        "Words the shape would cover.#box(width: 4pt, height: 4pt, clip: true, place(dx: -80pt, dy: -5pt, rect(width: 200pt, height: 14pt, fill: white)))".to_owned(),
        format!("#highlight[{all}]"),
        format!("#highlight(fill: rgb(\"#ffe066\"))[{all}]"),
        format!("#highlight(fill: rgb(255, 224, 102, 50%))[{all}]"),
        "#table(columns: 3, fill: (x, y) => if y == 0 { luma(90%) }, stroke: 0.5pt, [*Skill*], [*Years*], [*Level*], [Rust], [5], [Expert])".to_owned(),
        "#table(columns: 2, fill: luma(92%), stroke: white, [Rust], [5], [Go], [3])".to_owned(),
        "#table(columns: 2, fill: (x, y) => if y == 0 { rgb(\"#dde6f0\") }, stroke: none, table.hline(), [Rust], [5], table.hline(), [Go], [3], table.hline())".to_owned(),
        "#line(length: 100%)\nText after a line.\n#line(length: 100%, stroke: 2pt + rgb(\"#1f3a5f\"))".to_owned(),
        "#rect(stroke: 1pt, inset: 0pt)[Tight box gypq]\n#box(stroke: 1pt)[Inline box gypq]\n#rect(stroke: 0.5pt)[Normal box]".to_owned(),
        "#set page(fill: rgb(\"#fbf6e9\"))\nOn cream. #text(fill: rgb(\"#55606e\"))[Muted on cream.]\n#highlight(fill: rgb(\"#f3e7c4\"))[A shade.]".to_owned(),
        "#block(fill: rgb(\"#eef2f7\"), inset: 6pt, radius: 4pt)[A shaded callout with #text(fill: rgb(\"#1f3a5f\"))[blue words].]".to_owned(),
        "#block(fill: gradient.linear(white, rgb(\"#dde6f0\")), inset: 6pt)[On a pale gradient.]".to_owned(),
        "#box(fill: luma(90%), inset: 2pt, radius: 2pt)[`code`] and #box(fill: luma(95%), outset: 2pt)[a tag]".to_owned(),
        "#show raw: it => box(fill: luma(94%), outset: 2pt, it)\n`let a = 1;`\n```rust\nfn main() {}\n```".to_owned(),
        "$ sqrt(x^2 + y^2) = sum_(i=0)^n hat(a)_i / (b + 1) $\nInline $x^2$ and H#sub[2]O and E=mc#super[2].".to_owned(),
        "#smallcaps[Small Caps] #underline[link_with_under-score] #link(\"https://example.org\")[#underline[example.org]]".to_owned(),
        "#set par(leading: 0.2em)\nTight gypq lines of text.\\\nOn top of gypq each other.\\\nThird of gypq them.".to_owned(),
        "#text(size: 28pt, weight: \"bold\")[Large gypq title]\n#v(-0.8em)\nA line drawn close under it.".to_owned(),
        "A white square away from any text.#place(bottom + right, rect(width: 1em, height: 1em, fill: white))".to_owned(),
        "#text(fill: rgb(0, 0, 0, 60%))[Translucent text.] Marked over.#box(place(dx: -4em, dy: -0.8em, rect(width: 4em, height: 1.1em, fill: rgb(255, 224, 102, 60%))))".to_owned(),
        "#set page(fill: rgb(\"#1f3a5f\"))\n#text(fill: rgb(\"#a8b6c8\"))[Pale blue on a dark page reads, and stays below the white rule.]".to_owned(),
    ];
    let mut refused = Vec::new();
    for body in &cases {
        let source = format!("{START}{body}\n");
        let report = render_and_check(&source, &facts_en()).await;
        let hidden: Vec<_> = report
            .violations
            .iter()
            .filter(|violation| violation.rule == Rule::HiddenText)
            .collect();
        if !hidden.is_empty() {
            refused.push(format!("{body}\n  {hidden:?}"));
        }
    }
    assert!(refused.is_empty(), "{}", refused.join("\n"));
}

/// A draft is stamped after its text has been read, so the stamp can't refuse every draft.
#[tokio::test]
async fn the_stamp_refuses_no_draft() {
    let source = format!("{START}#header()\n= Work\n#lorem(200)\n");
    let request = RenderRequest {
        source,
        facts: facts_en(),
        pdf: false,
        preview_pages: 1,
        stamp: true,
    };
    let rendered = Renderer::InProcess.render(request).await.unwrap();
    assert_eq!(rendered.text.hidden, Vec::<String>::new());
    assert_eq!(rendered.previews.len(), 1);
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

/// Honest links on neighbouring lines are two links, however tall their boxes are.
#[tokio::test]
async fn honest_contact_links_that_wrap_or_stack_are_read_apart() {
    let mut facts = facts_en();
    facts.contact = Contact {
        email: "mara.testperson@example.test".to_owned(),
        phone: "+49 170 5550137".to_owned(),
        location: "Frankfurt am Main, Germany".to_owned(),
        linkedin: "linkedin.com/in/mara-testperson-4711".to_owned(),
        github: "github.com/mara-testperson".to_owned(),
    };
    let mail = "#link(\"mailto:mara.testperson@example.test\")[mara.testperson\\@example.test]";
    let linkedin = "#link(\"https://linkedin.com/in/mara-testperson-4711\")[linkedin.com/in/mara-testperson-4711]";
    let github = "#link(\"https://github.com/mara-testperson\")[github.com/mara-testperson]";
    let repo =
        "#link(\"https://github.com/mara-testperson/tally\")[github.com/mara-testperson/tally]";
    let cases = [
        ("the template's own, wrapped", "#header()".to_owned()),
        (
            "the template's own on A5",
            "#set page(paper: \"a5\")\n#header()".to_owned(),
        ),
        ("mail over GitHub", format!("{mail}\\\n{github}")),
        ("three stacked", format!("{mail}\\\n{linkedin}\\\n{github}")),
        ("a repo over the mail", format!("{repo}\\\n{mail}")),
        (
            "tight lines",
            format!("#set par(leading: 0.2em)\n{mail}\\\n{github}\\\n{repo}"),
        ),
        (
            "a short one over a long one",
            format!("#link(\"https://github.com/mara-testperson\")[GitHub]\\\n{mail}"),
        ),
        ("separate paragraphs", format!("{mail}\n\n{github}")),
        ("one line, with a separator", format!("{mail} | {github}")),
        ("one line, a word space apart", format!("{mail} {github}")),
    ];
    let mut refused = Vec::new();
    for (name, body) in &cases {
        let report = render_and_check(&format!("{START}{body}\n"), &facts).await;
        if !report.violations.is_empty() {
            refused.push(format!("{name}: {:?}", report.violations));
        }
    }
    assert!(refused.is_empty(), "{}", refused.join("\n"));
}

/// The other side of the same rule: links side by side on one line stay one link to a reader, whatever
/// their sizes do to the boxes, and whichever order those boxes sort in.
#[tokio::test]
async fn links_side_by_side_read_as_one_at_any_size() {
    let evil = "https://evil.example";
    let other = "https://other.example";
    assert_flagged(&[
        (
            "two targets, the second larger",
            Rule::ContactDetails,
            &format!("#link(\"{evil}\")[#text(size: 8pt)[mara\\@ex]]#link(\"{other}\")[#text(size: 16pt)[ample.org]]"),
        ),
        (
            "two targets, the first larger",
            Rule::ContactDetails,
            &format!("#link(\"{evil}\")[#text(size: 16pt)[mara\\@ex]]#link(\"{other}\")[#text(size: 8pt)[ample.org]]"),
        ),
        (
            "two targets, tiny beside large",
            Rule::ContactDetails,
            &format!("#link(\"{evil}\")[#text(size: 5pt)[mara\\@ex]]#link(\"{other}\")[#text(size: 24pt)[ample.org]]"),
        ),
        (
            "two targets, the same size",
            Rule::ContactDetails,
            &format!("#link(\"{evil}\")[mara\\@ex]#link(\"{other}\")[ample.org]"),
        ),
        (
            "three links",
            Rule::ContactDetails,
            &format!("#link(\"{evil}\")[mara]#link(\"{other}\")[\\@ex]#link(\"{evil}\")[ample.org]"),
        ),
        (
            "raised a little",
            Rule::ContactDetails,
            &format!("#link(\"{evil}\")[mara\\@ex]#box(baseline: -1pt)[#link(\"{other}\")[ample.org]]"),
        ),
    ])
    .await;
}

/// Text and strokes pale enough to nearly vanish count as hidden, and ordinary muted colours don't.
#[tokio::test]
async fn pale_text_and_strokes_count_as_hidden_and_muted_colours_do_not() {
    assert_flagged(&[
        (
            "fill 88%",
            Rule::HiddenText,
            "I am #text(fill: luma(88%))[not] certified.",
        ),
        (
            "fill 80%",
            Rule::HiddenText,
            "I am #text(fill: luma(80%))[not] certified.",
        ),
        (
            "stroke 89%",
            Rule::HiddenText,
            "I am #text(stroke: 1pt + luma(89%))[not] certified.",
        ),
        (
            "stroke 80%",
            Rule::HiddenText,
            "I am #text(stroke: 1pt + luma(80%))[not] certified.",
        ),
        (
            "both",
            Rule::HiddenText,
            "I am #text(fill: luma(89%), stroke: 1pt + luma(89%))[not] certified.",
        ),
    ])
    .await;
    for colour in [
        "black",
        "luma(30%)",
        "luma(50%)",
        "luma(60%)",
        "luma(70%)",
        "gray",
        "rgb(\"#55606e\")",
        "rgb(\"#1f3a5f\")",
        "rgb(\"#666\")",
        "rgb(\"#777\")",
        "rgb(\"#888\")",
        "rgb(\"#999\")",
        "rgb(\"#aaa\")",
        "rgb(\"#0a7ea4\")",
        "rgb(\"#c0392b\")",
        "rgb(\"#2e7d32\")",
        "rgb(\"#3498db\")",
        "rgb(\"#e67e22\")",
    ] {
        let source = format!("{START}#text(fill: {colour})[Muted work]\n");
        let report = render_and_check(&source, &facts_en()).await;
        assert_eq!(report.violations, [], "{colour}");
    }
}

/// What the check says of a line of a CV: the words no reader can see, and those with a mark on them.
async fn judged(line: &str) -> (Vec<String>, Vec<String>) {
    let request = RenderRequest {
        source: format!(
            "{START}= Work\n#employment(\"brightpath\")[\n- {line}\n- Mentored developers.\n]\n"
        ),
        facts: facts_en(),
        pdf: false,
        preview_pages: 0,
        stamp: false,
    };
    let rendered = Renderer::InProcess.render(request).await.unwrap();
    (rendered.text.hidden, rendered.text.marked)
}

/// A line that draws nothing, on any page or in any viewer, and that the widened drawing draws closed up.
fn nowhere(dx: f64, dy: f64, length: f64, thickness: f64, paint: &str) -> String {
    format!(
        "place(top + left, dx: {dx}pt, dy: {dy}pt, line(length: {length}pt, stroke: (paint: {paint}, thickness: {thickness}pt, dash: (array: (0.25pt, 1000pt), phase: 1pt))))"
    )
}

/// `glyph` at 11 pt with a grey ground only the widened drawing has, and white patches that really are
/// on the page. Its box and its advance are in the font's units, thousandths of an em.
fn erased_on_a_ground_of_no_page(
    glyph: &str,
    (x0, y0, x1, y1): (f64, f64, f64, f64),
    advance: f64,
    patches: &[(f64, f64, f64, f64)],
) -> String {
    let s = 0.011;
    let (pad, width, height) = (1.2, (x1 - x0) * s + 2.4, (y1 - y0) * s + 2.4);
    let ground = nowhere(
        x0 * s - pad,
        -y1 * s - pad + height / 2.0,
        width,
        height,
        "rgb(120, 120, 120)",
    );
    let white: Vec<String> = patches
        .iter()
        .map(|(px0, py0, px1, py1)| {
            format!(
                "place(top + left, dx: {}pt, dy: {}pt, rect(width: {}pt, height: {}pt, fill: white))",
                px0 * s - advance * s, -py1 * s, (px1 - px0) * s, (py1 - py0) * s
            )
        })
        .collect();
    format!(
        "#box(width: 0pt, height: 0pt, {{ {ground} }})#text(size: 11pt, kerning: false)[{glyph}]#box(width: 0pt, height: 0pt, {{ {} }})",
        white.join("\n")
    )
}

/// The fifth gate review's findings 2 and 3: a ground only the widened drawing has can't make an eraser
/// look visible, or give white text something to stand on.
#[tokio::test]
async fn a_glyph_must_arrive_in_the_page_as_really_drawn() {
    let band = |paint: &str| nowhere(-1.0, 5.0, 19.0, 12.0, paint);
    let dot = (57.0, -10.0, 163.0, 96.0);
    let minus = (60.0, 226.0, 490.0, 276.0);
    let hyphen = (40.0, 223.0, 298.0, 283.0);
    let comma = (43.0, -133.0, 170.0, 95.0);
    let plus = (60.0, 41.0, 490.0, 462.0);
    let colon = (70.0, 7.0, 176.0, 411.0);
    let e_box = (12.0, -2.0, 528.0, 649.0);
    let eight = (46.0, -10.0, 419.0, 610.0);
    let dollar = (29.0, -98.0, 417.0, 695.0);
    let cases: Vec<(&str, String)> = vec![
        (
            "white on a dash",
            format!(
                "I am #box[#{}#text(fill: white)[not]] certified.",
                band("black")
            ),
        ),
        (
            "navy on navy, with a white dash",
            format!(
                "I am #box(fill: navy, outset: 1pt)[#{}#text(fill: navy)[not]] certified.",
                band("white")
            ),
        ),
        (
            "a dot",
            format!(
                "Grew revenue 4{}9 times.",
                erased_on_a_ground_of_no_page(".", dot, 220.0, &[dot])
            ),
        ),
        (
            "a minus",
            format!(
                "Margin {}5 percent.",
                erased_on_a_ground_of_no_page("−", minus, 550.0, &[minus])
            ),
        ),
        (
            "a hyphen",
            format!(
                "Range 3{}5 percent.",
                erased_on_a_ground_of_no_page("-", hyphen, 338.0, &[hyphen])
            ),
        ),
        (
            "a comma's tail",
            format!(
                "Paid 1{}000 euro.",
                erased_on_a_ground_of_no_page(",", comma, 220.0, &[(43.0, -133.0, 170.0, 22.0)])
            ),
        ),
        (
            "a plus's arms",
            format!(
                "Change {}5 points.",
                erased_on_a_ground_of_no_page(
                    "+",
                    plus,
                    550.0,
                    &[(210.0, 41.0, 390.0, 170.0), (210.0, 330.0, 390.0, 462.0)]
                )
            ),
        ),
        (
            "a colon's top",
            format!(
                "Time 10{}30.",
                erased_on_a_ground_of_no_page(":", colon, 236.0, &[(70.0, 250.0, 176.0, 411.0)])
            ),
        ),
        (
            "a dollar",
            format!(
                "Saved {}500 invoices.",
                erased_on_a_ground_of_no_page("\\$", dollar, 465.0, &[dollar])
            ),
        ),
        (
            "E as F",
            format!(
                "Grade {}.",
                erased_on_a_ground_of_no_page("E", e_box, 557.0, &[(150.0, -2.0, 528.0, 110.0)])
            ),
        ),
        (
            "8 as 3",
            format!(
                "Saved {}00 invoices.",
                erased_on_a_ground_of_no_page("8", eight, 465.0, &[(30.0, 50.0, 160.0, 560.0)])
            ),
        ),
    ];
    for (name, line) in cases {
        let (hidden, _) = judged(&line).await;
        assert!(!hidden.is_empty(), "{name} passed: {line}");
    }
    // White on what really is there stays allowed.
    for line in [
        "I am #box(fill: black, outset: 1pt)[#text(fill: white)[not]] certified.",
        "I am #box(fill: gradient.conic(black, rgb(40, 40, 40)), outset: 1pt)[#text(fill: white)[not]] certified.",
        "I am #box(fill: tiling(size: (0.6pt, 0.6pt), rect(width: 0.6pt, height: 0.6pt, fill: black)), outset: 1pt)[#text(fill: white)[not]] certified.",
    ] {
        let (hidden, _) = judged(line).await;
        assert_eq!(hidden, Vec::<String>::new(), "{line}");
    }
}

/// The fifth gate review's finding 1: a line's thickness can run along the axis a stretch doesn't enlarge,
/// so it stays under a pixel and fades here, and a PDF viewer draws it a whole pixel wide. Such a line is
/// as thin as it was, and a compile error.
#[tokio::test]
async fn hairlines_under_a_stretch_are_widened_along_the_narrower_axis() {
    let across = |count: i32, length: f64| -> String {
        (0..count)
            .map(|i| {
                format!(
                    "place(top + left, dy: {}pt, line(length: {length}pt, stroke: 0.02pt + white))",
                    f64::from(i) * 0.5
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let down: String = (0..38)
        .map(|i| format!("place(top + left, dx: {}pt, line(angle: 90deg, length: 0.2pt, stroke: 0.02pt + white))", f64::from(i) * 0.5))
        .collect::<Vec<_>>()
        .join("\n");
    let turned: String = (0..20)
        .map(|i| format!("place(top + left, dx: {}pt, line(angle: 90deg, length: 0.2pt, stroke: 0.02pt + white))", f64::from(i) * 0.5))
        .collect::<Vec<_>>()
        .join("\n");
    let cases = [
        (
            "stretched across",
            format!(
                "I am #box[not#place(top + left, dx: -1pt, dy: -1pt, scale(x: 10000%, origin: top + left, reflow: false, box(width: 0.2pt, {{\n{}\n}})))] certified.",
                across(20, 0.2)
            ),
        ),
        (
            "stretched down",
            format!(
                "I am #box[not#place(top + left, dx: -1pt, dy: -1pt, scale(y: 10000%, origin: top + left, reflow: false, box(height: 0.2pt, {{\n{down}\n}})))] certified."
            ),
        ),
        (
            "turned and stretched",
            format!(
                "I am #box[not#place(top + left, dx: 8pt, dy: 3pt, rotate(90deg, reflow: false, scale(y: 10000%, origin: top + left, reflow: false, box(width: 10pt, height: 0.2pt, {{\n{turned}\n}}))))] certified."
            ),
        ),
        (
            "on a digit",
            format!(
                "Saved #box[#text(size: 11pt)[8]#place(top + left, dx: -0.5pt, dy: -1pt, scale(x: 1000%, origin: top + left, reflow: false, box(width: 0.3pt, {{\n{}\n}})))]00 invoices.",
                across(20, 0.3)
            ),
        ),
        (
            "over a whole line",
            format!(
                "I am #box(width: 300pt)[#text[not certified as an auditor]#place(top + left, dx: -1pt, dy: -1pt, scale(x: 100000%, origin: top + left, reflow: false, box(width: 0.31pt, {{\n{}\n}})))]",
                across(20, 0.31)
            ),
        ),
    ];
    assert_too_thin(&cases).await;
    // A stretched rule of honest thickness stays allowed.
    let (hidden, _) = judged("Work.#v(3pt)#scale(x: 150%, origin: top + left, reflow: false, line(length: 100pt, stroke: 0.5pt))").await;
    assert_eq!(hidden, Vec::<String>::new());
}

/// The fifth gate review's finding 5, the owner's decision: a small mark on a number isn't refused, as
/// nothing is hidden, and the owner is told to read it. Ordinary lines and fills don't set it off.
#[tokio::test]
async fn a_mark_drawn_on_text_is_said_and_not_refused() {
    let marked = [
        (
            "3 as 8",
            "Saved #text(size: 11pt, kerning: false)[3]#box(width: 0pt, height: 0pt, {place(top + left, dx: -5.0pt, dy: -6.8pt, curve(stroke: 0.7pt + black, curve.move((2.4pt, 0.3pt)), curve.cubic((0.3pt, 0.3pt), (0.3pt, 3pt), (2.4pt, 3.4pt)), curve.cubic((0.0pt, 3.8pt), (0.0pt, 7pt), (2.4pt, 7pt))))})00 invoices.",
        ),
        (
            "15 as 1.5",
            "Grew revenue #text(size: 11pt, kerning: false)[1]#box(width: 0pt, height: 0pt, place(top + left, dx: 0.4pt, dy: -1.3pt, circle(radius: 0.65pt, fill: black)))#h(0.9pt)#text(size: 11pt)[5] times.",
        ),
        (
            "5 as minus 5",
            "Margin#h(3pt)#box(width: 0pt, height: 0pt, place(top + left, dy: -3pt, rect(width: 4.5pt, height: 0.5pt, fill: black)))#h(5pt)5 percent.",
        ),
        (
            "a dot as a comma",
            "Paid 1.#box(width: 0pt, height: 0pt, place(top + left, dx: -1.6pt, dy: 0.3pt, rect(width: 1pt, height: 1.8pt, fill: black)))000 euro.",
        ),
        (
            "F as E",
            "Grade #text(size: 11pt, kerning: false)[F]#box(width: 0pt, height: 0pt, place(top + left, dx: -5pt, dy: -0.6pt, rect(width: 5pt, height: 0.6pt, fill: black))) here.",
        ),
    ];
    for (name, line) in marked {
        let (hidden, marks) = judged(line).await;
        assert_eq!(hidden, Vec::<String>::new(), "{name}");
        assert!(!marks.is_empty(), "{name} was not said");
    }
    let ordinary = [
        "Plain words and 40 numbers.",
        "#underline[Reduced invoice errors across three depots]",
        "A word #underline[in] line.",
        "#underline[gypsy jumpy happy pygmy]",
        "#underline(stroke: 1.5pt + blue)[gypsy jumpy happy pygmy]",
        "#strike[Reduced invoice errors]",
        "#overline[Reduced invoice errors]",
        "#highlight[A] and #highlight(fill: rgb(\"#ffe066\"))[a tag]",
        "#box(fill: rgb(\"#1f3a5f\"), inset: 3pt, radius: 3pt)[#text(fill: white)[Rust]] #box(fill: luma(90%), inset: 2pt, radius: 2pt)[Go]",
        "#box(stroke: 0.5pt, inset: 2pt)[Inline box gypq]",
        "Rust #box(width: 1fr, line(length: 100%, stroke: (dash: \"dotted\")))Expert",
        "Rust #box(width: 1fr, line(length: 100%, stroke: (paint: gray, dash: \"dashed\")))Expert",
        "#table(columns: 3, fill: (x, y) => if y == 0 { luma(90%) }, stroke: 0.5pt, [*Skill*], [*Years*], [*Level*], [Rust], [5], [Expert])",
        "#line(length: 100%)\nText after a line.\n#line(length: 100%, stroke: 2pt + rgb(\"#1f3a5f\"))",
    ];
    for line in ordinary {
        let (hidden, marks) = judged(line).await;
        assert_eq!(hidden, Vec::<String>::new(), "{line}");
        assert_eq!(marks, Vec::<String>::new(), "{line}");
    }
    let request = RenderRequest {
        source: default_source(&facts_en()),
        facts: facts_en(),
        pdf: false,
        preview_pages: 0,
        stamp: false,
    };
    let rendered = Renderer::InProcess.render(request).await.unwrap();
    assert_eq!(
        rendered.text.marked,
        Vec::<String>::new(),
        "the template itself"
    );
}
