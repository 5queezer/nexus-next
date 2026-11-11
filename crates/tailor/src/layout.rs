//! Advice on how a CV fills its pages: a line that ends in a stub, a heading or an entry a page break
//! cuts off, a last page with little on it. Read from where the words stand, after the fact. No check
//! reads it and no approval stores it.
//!
//! Headings and entries are found as elements, which is exact. Which lines are one wrapped paragraph
//! or bullet is read from the geometry alone: a line continues the one above if it starts at the same
//! left edge in the same size and its first word would not have fit above. So two separate lines of
//! which the first happens to be full read as one, and a side column's lines, whose width this can't
//! see, mostly read as unwrapped.

use typst::foundations::{NativeElement, Value};
use typst::introspection::{Introspector, MetadataElem};
use typst::model::HeadingElem;
use typst_layout::PagedDocument;

use crate::{Layout, LayoutKind, LayoutNote, RenderedText};

/// What `/nexus.typ` puts ahead of and after each employment, education and project entry.
const ENTRY_MARK: &str = "nexus-entry";
const ENTRY_END_MARK: &str = "nexus-entry-end";
/// A word's box is this many ems high: see `extract::WORD_ASCENT`.
const BOX_EMS: f64 = 1.05;
/// Words further apart than this, in ems, stand in different columns of a line, as a date does.
const COLUMN_GAP: f64 = 0.8;
/// The narrowest a word space is drawn, in ems.
const SPACE: f64 = 0.2;
/// How far two left edges may differ and still be one, in points.
const EDGE: f64 = 1.0;
/// Lines of one paragraph are no further apart than this, in box heights.
const MAX_PITCH: f64 = 2.2;
/// A last line shorter than this share of its paragraph's widest is a stub.
const STUB_SHARE: f64 = 0.15;
/// A last page less full than this is nearly empty.
const THIN_PAGE: f64 = 0.25;
const BULLETS: [&str; 5] = ["•", "-", "–", "‣", "◦"];

/// A column's part of a line, in points on its page.
struct Part {
    line: usize,
    first_word: usize,
    words: usize,
    page: u32,
    x0: f64,
    x1: f64,
    y0: f64,
    y1: f64,
    /// Where its text starts, past a bullet.
    body: f64,
    /// How far right its text could have gone.
    limit: f64,
    text: String,
    first_width: f64,
}

impl Part {
    fn height(&self) -> f64 {
        self.y1 - self.y0
    }

    fn at(&self) -> Option<(usize, usize, usize)> {
        Some((self.line, self.first_word, self.words))
    }
}

fn parts(document: &PagedDocument, text: &RenderedText) -> Vec<Part> {
    let sizes: Vec<_> = document
        .pages()
        .iter()
        .map(|page| page.frame.size())
        .collect();
    let mut lines = Vec::new();
    let mut right = 0.0_f64;
    for (line, place) in text.places.iter().enumerate() {
        let Some(size) = sizes.get(place.page as usize) else {
            continue;
        };
        let words: Vec<&str> = text.lines[line].split_whitespace().collect();
        if words.is_empty() || words.len() != place.words.len() {
            continue;
        }
        let (width, height) = (size.x.to_pt(), size.y.to_pt());
        let boxes: Vec<[f64; 4]> = place
            .words
            .iter()
            .map(|[x, y, w, h]| {
                let (x, y) = (f64::from(*x) * width, f64::from(*y) * height);
                [x, y, x + f64::from(*w) * width, y + f64::from(*h) * height]
            })
            .collect();
        right = boxes.iter().fold(right, |right, word| right.max(word[2]));
        lines.push((line, place.page, words, boxes));
    }
    let mut parts = Vec::new();
    for (line, page, words, boxes) in lines {
        let mut start = 0;
        for end in 1..=words.len() {
            let em = (boxes[end - 1][3] - boxes[end - 1][1]) / BOX_EMS;
            if end < words.len() && boxes[end][0] - boxes[end - 1][2] <= COLUMN_GAP * em {
                continue;
            }
            let bullet = BULLETS.contains(&words[start]) && end - start > 1;
            let body = start + usize::from(bullet);
            parts.push(Part {
                line,
                first_word: start,
                words: end - start,
                page,
                x0: boxes[start][0],
                x1: boxes[end - 1][2],
                y0: boxes[start..end]
                    .iter()
                    .fold(f64::INFINITY, |y, word| y.min(word[1])),
                y1: boxes[start..end].iter().fold(0.0, |y, word| y.max(word[3])),
                body: boxes[body][0],
                limit: boxes.get(end).map_or(right, |next| next[0] - em),
                text: words[start..end].join(" "),
                first_width: boxes[body][2] - boxes[body][0],
            });
            start = end;
        }
    }
    parts.sort_by(|a, b| {
        (a.page, a.y0, a.x0)
            .partial_cmp(&(b.page, b.y0, b.x0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    parts
}

/// The part that continues `parts[at]` as the next line of its paragraph.
fn continued(parts: &[Part], at: usize) -> Option<usize> {
    let above = &parts[at];
    let em = above.height() / BOX_EMS;
    parts[at + 1..]
        .iter()
        .position(|below| {
            below.page == above.page
                && below.y0 > above.y0 + 0.5 * above.height()
                && (below.x0 - above.body).abs() <= EDGE
        })
        .map(|offset| at + 1 + offset)
        .filter(|below| {
            let below = &parts[*below];
            below.y0 - above.y0 <= MAX_PITCH * above.height()
                && (below.height() - above.height()).abs() <= 0.03 * above.height()
                && !BULLETS.contains(&below.text.split(' ').next().unwrap_or_default())
                && above.x1 + SPACE * em + below.first_width > above.limit - 0.5
        })
}

/// Where the headings stand, or the marks holding `mark`: page from 0, left and top in points, and
/// what a heading says.
fn marks(document: &PagedDocument, mark: Option<&str>) -> Vec<(u32, f64, f64, String)> {
    let introspector = document.introspector();
    let selector = match mark {
        Some(_) => MetadataElem::ELEM.select(),
        None => HeadingElem::ELEM.select(),
    };
    introspector
        .query(&selector)
        .iter()
        .filter_map(|element| {
            let said = match (
                element.to_packed::<HeadingElem>(),
                element.to_packed::<MetadataElem>(),
            ) {
                (Some(heading), _) => heading.body.plain_text().to_string(),
                (_, Some(found))
                    if Some(&found.value) == mark.map(|mark| Value::Str(mark.into())).as_ref() =>
                {
                    String::new()
                }
                _ => return None,
            };
            let position = introspector.position(element.location()?)?;
            let page = u32::try_from(position.page.get() - 1).ok()?;
            Some((
                page,
                position.point.x.to_pt(),
                position.point.y.to_pt(),
                said,
            ))
        })
        .collect()
}

fn plural(count: usize, word: &str) -> String {
    format!("{count} {word}{}", if count == 1 { "" } else { "s" })
}

pub(crate) fn layout(document: &PagedDocument, text: &RenderedText) -> Layout {
    let pages = u32::try_from(document.pages().len()).unwrap_or(u32::MAX);
    let parts = parts(document, text);
    let mut out = Layout {
        pages,
        ..Layout::default()
    };
    let (Some(last), Some(page)) = (pages.checked_sub(1), document.pages().last()) else {
        return out;
    };
    if parts.is_empty() {
        return out;
    }
    let mut notes = Vec::new();

    // Wrapped paragraphs and bullets, and the stubs they end in.
    let next: Vec<Option<usize>> = (0..parts.len()).map(|at| continued(&parts, at)).collect();
    let mut pitches = Vec::new();
    for start in (0..parts.len()).filter(|at| !next.contains(&Some(*at))) {
        let mut chain = vec![start];
        while let Some(below) = next[*chain.last().unwrap_or(&start)] {
            pitches.push(parts[below].y0 - parts[chain[chain.len() - 1]].y0);
            chain.push(below);
        }
        if chain.len() < 2 {
            continue;
        }
        let (end, above) = (
            &parts[chain[chain.len() - 1]],
            &parts[chain[chain.len() - 2]],
        );
        let widest = chain[..chain.len() - 1]
            .iter()
            .map(|at| parts[*at].x1 - parts[*at].body)
            .fold(0.0, f64::max);
        let width = end.x1 - end.x0;
        if end.words > 1 && width >= STUB_SHARE * widest {
            continue;
        }
        let em = end.height() / BOX_EMS;
        let over = width + SPACE * em - (above.limit - above.x1);
        let letter = (above.x1 - above.body) / above.text.chars().count().max(1) as f64;
        notes.push(LayoutNote {
            kind: LayoutKind::Stub,
            page: end.page + 1,
            line: end.text.clone(),
            at: end.at(),
            message: format!(
                "A paragraph on page {1} ends in a stub: \"{0}\" is the last of {2} lines, {3:.0}% of the width. About {4} fewer above it, or a few more words, ends that.",
                end.text,
                end.page + 1,
                chain.len(),
                100.0 * width / widest.max(1.0),
                plural((over / letter.max(0.1)).ceil().max(1.0) as usize, "character"),
            ),
        });
    }

    // What a page break cuts: a heading from its section, an entry in two.
    let headings = marks(document, None);
    let below = |page: u32, x: f64, y: f64| {
        parts.iter().filter(move |part| {
            part.page == page && part.x0 >= x - EDGE && part.y1 > y + 0.5 * part.height()
        })
    };
    for (page, x, y, said) in &headings {
        let mut lines = below(*page, *x, *y);
        let Some(own) = lines.next() else { continue };
        if *page < last && lines.all(|part| part.line == own.line) {
            notes.push(LayoutNote {
                kind: LayoutKind::HeadingAlone,
                page: page + 1,
                line: own.text.clone(),
                at: own.at(),
                message: format!(
                    "The heading \"{said}\" is the last line of page {}, and its section starts on page {}. Cut or add a line or two above it.",
                    page + 1,
                    page + 2,
                ),
            });
        }
    }
    let entries = marks(document, Some(ENTRY_MARK));
    let ends = marks(document, Some(ENTRY_END_MARK));
    for ((page, x, y, _), end) in entries.iter().zip(&ends) {
        if end.0 <= *page {
            continue;
        }
        let here: Vec<&Part> = below(*page, *x, *y).collect();
        let there: Vec<&Part> = (page + 1..=end.0)
            .flat_map(|later| {
                below(later, *x, 0.0).filter(move |part| later < end.0 || part.y0 < end.2 - EDGE)
            })
            .collect();
        let count = |parts: &[&Part]| {
            let mut lines: Vec<usize> = parts.iter().map(|part| part.line).collect();
            lines.dedup();
            lines.len()
        };
        if let (Some(first), Some(_)) = (here.first(), there.first()) {
            notes.push(LayoutNote {
                kind: LayoutKind::EntrySplit,
                page: page + 1,
                line: first.text.clone(),
                at: first.at(),
                message: format!(
                    "The entry \"{}\" breaks across pages: {} on page {} and {} after. Cutting {} above its end keeps it on one page.",
                    first.text,
                    plural(count(&here), "line"),
                    page + 1,
                    plural(count(&there), "line"),
                    plural(count(&there), "line"),
                ),
            });
        }
    }

    // How full the last page is. The text area is taken to end as far from the bottom as it starts
    // from the top, or where a fuller page's text ends.
    let top = parts
        .iter()
        .map(|part| part.y0)
        .fold(f64::INFINITY, f64::min);
    let bottom = parts
        .iter()
        .map(|part| part.y1)
        .fold(page.frame.height().to_pt() - top, f64::max);
    let used = parts
        .iter()
        .filter(|part| part.page == last)
        .map(|part| part.y1)
        .fold(top, f64::max);
    pitches.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    // A line's pitch, from the paragraphs if any wrapped, or else from the commonest size of text.
    let mut heights: Vec<f64> = parts.iter().map(Part::height).collect();
    heights.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let pitch = pitches
        .get(pitches.len() / 2)
        .copied()
        .unwrap_or(1.45 * heights[heights.len() / 2]);
    let mut on_last: Vec<usize> = parts
        .iter()
        .filter(|part| part.page == last)
        .map(|part| part.line)
        .collect();
    on_last.dedup();
    out.last_page_fill = ((used - top) / (bottom - top).max(1.0)).clamp(0.0, 1.0) as f32;
    out.room_lines = ((bottom - used) / pitch).floor().max(0.0) as u32;
    out.last_page_lines = on_last.len() as u32;
    if last > 0 && (f64::from(out.last_page_fill) < THIN_PAGE || on_last.len() == 1) {
        let first = parts.iter().find(|part| part.page == last);
        notes.push(LayoutNote {
            kind: LayoutKind::ThinLastPage,
            page: pages,
            line: first.map(|part| part.text.clone()).unwrap_or_default(),
            at: first.and_then(Part::at),
            message: format!(
                "Page {pages} holds only {}, {:.0}% of its height. Cutting about that much brings the CV to {}.",
                plural(on_last.len(), "line"),
                100.0 * out.last_page_fill,
                plural(last as usize, "page"),
            ),
        });
    }
    notes.sort_by_key(|note| (note.page, note.at));
    out.notes = notes;
    out
}
