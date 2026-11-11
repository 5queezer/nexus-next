//! Sources a CV can start from. Text is written as markup, not string literals, so that a later
//! dictated edit can name it exactly as it reads.

use std::fmt::Write;

use crate::Facts;

/// What `get_cv_source` tells an agent about `/nexus.typ`.
pub const TEMPLATE_HELP: &str = r#"Start a source with `#import "/nexus.typ": *` and `#show: cv` (or `#show: cv.with(lang: "de")`).
`#show: cv.with(template: "modern")` draws the same source in another design: `classic` (the default), `modern`, `banner`, `sidebar` or `compact`.
`#show: cv.with(pages: 1)` or `pages: 2` names how many full pages the owner wants. It changes nothing by itself: `layout` in a `patch_cv` or `render_cv_pdf` reply says how far off the CV is, in lines, and which lines and pages break badly, so cut or add text and read it again.
Hard facts are rendered by id, never typed: `#header()`, `#contact()`, `#employment("id")[- a bullet]`, `#employment("id")` for one compact line, `#education("id")`, `#languages()`, `#metric("key")`.
Wording helpers: `= Section`, `#skills("Category")[Rust, Go]`, `#project("Name", stack: "Rust", url: "example.org")[What it does.]`.
Only /main.typ, /nexus.typ and /facts.json exist. A page is 200 to 1200 pt on each side. No images, packages, `hide` or `pdf.attach`. Text stays upright and at its natural spacing: no `rotate`, `skew`, mirroring, stretching, `tracking` that stacks or spreads glyphs, or a `stroke` over 0.1 em. Every letter must reach the reader, judged on the drawn page: nothing painted over it, text a reader can tell from what it really stands on, and only letters the fonts have (Latin, Greek, Cyrillic, Hebrew; no letters in math, digits are fine). A small mark drawn on text isn't refused, but the owner is told of it. Emails and URLs come from `#contact()`; a link's text is free unless it shows a contact value."#;

/// The designs `/nexus.typ` draws. A source that names none gets the first.
pub const TEMPLATES: [&str; 5] = ["classic", "modern", "banner", "sidebar", "compact"];

const SHOW: &str = "#show: cv";

/// The line that sets up the page: where it stands, and what it sets.
struct ShowLine<'a> {
    at: std::ops::Range<usize>,
    lang: Option<&'a str>,
    template: Option<&'a str>,
    pages: Option<u32>,
}

/// None unless the source has exactly one such line, written as this module writes it: no other is
/// safe to rewrite.
fn show_line(source: &str) -> Option<ShowLine<'_>> {
    let mut found = None;
    let mut at = 0;
    for line in source.split_inclusive('\n') {
        let text = line.trim_end();
        if let Some(rest) = text.strip_prefix(SHOW) {
            if found.is_some() {
                return None;
            }
            let args = match rest {
                "" => "",
                rest => rest.strip_prefix(".with(")?.strip_suffix(')')?,
            };
            let mut show = ShowLine {
                at: at..at + text.len(),
                lang: None,
                template: None,
                pages: None,
            };
            for arg in args.split(',').filter(|arg| !arg.trim().is_empty()) {
                let (key, value) = arg.split_once(':')?;
                let value = value.trim();
                let word = || {
                    let word = value.strip_prefix('"')?.strip_suffix('"')?;
                    word.chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-')
                        .then_some(word)
                };
                match key.trim() {
                    "lang" if show.lang.is_none() => show.lang = Some(word()?),
                    "template" if show.template.is_none() => show.template = Some(word()?),
                    "pages" if show.pages.is_none() && matches!(value, "1" | "2") => {
                        show.pages = value.parse().ok();
                    }
                    _ => return None,
                }
            }
            found = Some(show);
        }
        at += line.len();
    }
    found
}

/// The template a source names, or the first if it names none. None if its `#show: cv` line can't be
/// read.
pub fn template_of(source: &str) -> Option<&str> {
    show_line(source).map(|show| show.template.unwrap_or(TEMPLATES[0]))
}

/// How many full pages a source says it aims for: 1 or 2, or none if it names no target or its
/// `#show: cv` line can't be read.
pub fn pages_of(source: &str) -> Option<u32> {
    show_line(source)?.pages
}

/// `source` with its `#show: cv` line saying this, and nothing else changed.
fn rewritten(source: &str, show: &ShowLine, template: &str, pages: Option<u32>) -> String {
    let mut args = Vec::new();
    if let Some(lang) = show.lang {
        args.push(format!("lang: \"{lang}\""));
    }
    if template != TEMPLATES[0] {
        args.push(format!("template: \"{template}\""));
    }
    if let Some(pages) = pages {
        args.push(format!("pages: {pages}"));
    }
    let line = if args.is_empty() {
        SHOW.to_owned()
    } else {
        format!("{SHOW}.with({})", args.join(", "))
    };
    format!(
        "{}{line}{}",
        &source[..show.at.start],
        &source[show.at.end..]
    )
}

/// `source` drawn in another template: only its `#show: cv` line changes, and the first template is
/// written as no argument. None if that line can't be read or there is no such template.
pub fn with_template(source: &str, template: &str) -> Option<String> {
    let show = show_line(source)?;
    TEMPLATES
        .contains(&template)
        .then(|| rewritten(source, &show, template, show.pages))
}

/// `source` aiming for 1 or 2 full pages, or for none: only its `#show: cv` line changes. None if that
/// line can't be read or the target is another.
pub fn with_pages(source: &str, pages: Option<u32>) -> Option<String> {
    let show = show_line(source)?;
    matches!(pages, None | Some(1 | 2))
        .then(|| rewritten(source, &show, show.template.unwrap_or(TEMPLATES[0]), pages))
}

/// One job in a generated CV. Tier 3 renders as one compact line.
#[derive(Debug, Clone, Default)]
pub struct CvEmployment {
    pub id: String,
    pub tier: u8,
    pub bullets: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CvSkills {
    pub category: String,
    pub items: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CvProject {
    pub name: String,
    pub url: String,
    pub stack: String,
    pub description: String,
}

/// What `generate_tailored_cv` selected, in order.
#[derive(Debug, Clone, Default)]
pub struct CvContent {
    /// `de` for German section headings, anything else for English.
    pub lang: String,
    pub summary: String,
    pub skills: Vec<CvSkills>,
    pub employment: Vec<CvEmployment>,
    pub projects: Vec<CvProject>,
    pub education_ids: Vec<String>,
    pub languages: bool,
}

/// `text` as Typst markup that renders literally.
fn markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut at_start = true;
    while let Some(c) = chars.next() {
        let next = chars.peek().copied();
        let special = match c {
            '\\' | '#' | '$' | '*' | '_' | '`' | '@' | '<' | '>' | '[' | ']' | '~' => true,
            '/' => at_start || matches!(next, Some('/' | '*')),
            '-' => at_start || matches!(next, Some('-' | '?')),
            '=' | '+' => at_start,
            '.' => !out.is_empty() && out.bytes().all(|b| b.is_ascii_digit()),
            _ => false,
        };
        if special {
            out.push('\\');
        }
        out.push(if c == '\n' || c == '\r' { ' ' } else { c });
        at_start = false;
    }
    out
}

/// `text` as the body of a Typst string literal.
fn string(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out
}

const PREAMBLE: &str = "#import \"/nexus.typ\": *\n";

/// A valid starting source: every fact in order, with room for wording.
pub fn default_source(facts: &Facts) -> String {
    let mut out = format!("{PREAMBLE}#show: cv\n\n#header()\n");
    if !facts.employment.is_empty() {
        out.push_str("\n= Experience\n");
        for e in &facts.employment {
            let _ = writeln!(out, "#employment(\"{}\")", string(&e.id));
        }
    }
    if !facts.education.is_empty() {
        out.push_str("\n= Education\n");
        for e in &facts.education {
            let _ = writeln!(out, "#education(\"{}\")", string(&e.id));
        }
    }
    if !facts.languages.is_empty() {
        out.push_str("\n= Languages\n#languages()\n");
    }
    out
}

/// The source of a tailored CV. Hard facts stay references: the facts decide what they render as.
pub fn source_from_profile(content: &CvContent) -> String {
    let german = content.lang == "de";
    let heading = |en: &'static str, de: &'static str| if german { de } else { en };
    let mut out = String::from(PREAMBLE);
    let _ = writeln!(
        out,
        "#show: cv.with(lang: \"{}\")\n\n#header()",
        if german { "de" } else { "en" }
    );
    if !content.summary.trim().is_empty() {
        let _ = writeln!(
            out,
            "\n= {}\n{}",
            heading("Summary", "Profil"),
            markup(&content.summary)
        );
    }
    if !content.skills.is_empty() {
        let _ = writeln!(out, "\n= {}", heading("Skills", "Kenntnisse"));
        for skills in &content.skills {
            let _ = writeln!(
                out,
                "#skills(\"{}\")[{}]",
                string(&skills.category),
                markup(&skills.items.join(", "))
            );
        }
    }
    if !content.employment.is_empty() {
        let _ = writeln!(out, "\n= {}", heading("Experience", "Berufserfahrung"));
        for e in &content.employment {
            let id = string(&e.id);
            if e.tier >= 3 || e.bullets.is_empty() {
                let _ = writeln!(out, "#employment(\"{id}\")");
            } else {
                let _ = writeln!(out, "#employment(\"{id}\")[");
                for bullet in &e.bullets {
                    let _ = writeln!(out, "  - {}", markup(bullet));
                }
                out.push_str("]\n");
            }
        }
    }
    if !content.projects.is_empty() {
        let _ = writeln!(out, "\n= {}", heading("Projects", "Projekte"));
        for p in &content.projects {
            let _ = writeln!(
                out,
                "#project(\"{}\", stack: \"{}\", url: \"{}\")[{}]",
                string(&p.name),
                string(&p.stack),
                string(&p.url),
                markup(&p.description)
            );
        }
    }
    if !content.education_ids.is_empty() {
        let _ = writeln!(out, "\n= {}", heading("Education", "Bildungsweg"));
        for id in &content.education_ids {
            let _ = writeln!(out, "#education(\"{}\")", string(id));
        }
    }
    if content.languages {
        let _ = writeln!(
            out,
            "\n= {}\n#languages()",
            heading("Languages", "Sprachen")
        );
    }
    out
}
