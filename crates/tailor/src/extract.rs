//! Reads back what the laid-out pages say, for the integrity check.

use typst::foundations::NativeElement;
use typst::foundations::Smart;
use typst::introspection::Introspector;
use typst::layout::{Frame, FrameItem, Point, Size, Transform};
use typst::model::{Destination, Document, HeadingElem, Numbering};
use typst::text::{Glyph, TextItem};
use typst::visualize::Paint;
use typst_layout::PagedDocument;

use crate::{MAX_TEXT_BYTES, RenderError, RenderedLink, RenderedText};

/// Layout memoization can make a small source repeat a frame exponentially often.
const MAX_RUNS: usize = 200_000;
/// Each link is compared with every run on its page.
const MAX_LINKS: usize = 1000;
/// Runs closer than this share a word, so `#box[20]#box[15]` reads `2015`. In ems.
const WORD_GAP: f64 = 0.2;
/// Baselines closer than this share a line, so raised and lowered text stays in it. In ems.
const LINE_GAP: f64 = 0.35;
/// Text below this size in points can't be read, so it counts as hidden.
const MIN_READABLE_SIZE: f64 = 4.0;
/// Text this transparent or this close to white can't be seen on a page, so it counts as hidden.
const MIN_READABLE_ALPHA: f32 = 0.3;
const MAX_READABLE_LUMA: f32 = 0.9;
/// How far text may stand outside what shows, in points, before it counts as hidden.
const VISIBLE_MARGIN: f64 = 1.0;
/// How high above its baseline the middle of a run stands. In ems.
const RUN_MIDDLE: f64 = 0.3;
/// A word space narrower than this draws no gap, in ems. Justifying shrinks one to 0.17 at the least.
const MIN_SPACE_ADVANCE: f64 = 0.15;
/// How far a transform may stray from a uniform scale through rounding alone, as in `rotate(360deg)`.
const TRANSFORM_TOLERANCE: f64 = 1e-6;

#[derive(Clone)]
struct Run {
    bucket: usize,
    x: f64,
    y: f64,
    end_x: f64,
    size: f64,
    text: String,
}

/// What of a page shows, in page coordinates. Empty in a tiling: its tiles repeat text at any size.
#[derive(Clone, Copy)]
struct Visible {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

impl Visible {
    const NOTHING: Self = Self {
        x0: 0.0,
        y0: 0.0,
        x1: f64::NEG_INFINITY,
        y1: f64::NEG_INFINITY,
    };

    fn contains(self, point: Point) -> bool {
        (self.x0 - VISIBLE_MARGIN..=self.x1 + VISIBLE_MARGIN).contains(&point.x.to_pt())
            && (self.y0 - VISIBLE_MARGIN..=self.y1 + VISIBLE_MARGIN).contains(&point.y.to_pt())
    }

    /// `self` clipped to a rectangle given in the coordinates of `ts`.
    fn clipped(self, min: Point, max: Point, ts: Transform) -> Self {
        let corners = [min, max, Point::new(min.x, max.y), Point::new(max.x, min.y)]
            .map(|corner| corner.transform(ts));
        let xs = corners.map(|corner| corner.x.to_pt());
        let ys = corners.map(|corner| corner.y.to_pt());
        Self {
            x0: self.x0.max(xs.into_iter().fold(f64::INFINITY, f64::min)),
            y0: self.y0.max(ys.into_iter().fold(f64::INFINITY, f64::min)),
            x1: self
                .x1
                .min(xs.into_iter().fold(f64::NEG_INFINITY, f64::max)),
            y1: self
                .y1
                .min(ys.into_iter().fold(f64::NEG_INFINITY, f64::max)),
        }
    }
}

fn readable_paint(paint: &Paint) -> bool {
    let Paint::Solid(color) = paint else {
        return false;
    };
    color
        .alpha()
        .is_none_or(|alpha| alpha >= MIN_READABLE_ALPHA)
        && color.to_luma().luma <= MAX_READABLE_LUMA
}

/// How much `ts` scales a glyph along its narrower axis, so that text squeezed one way counts as small.
fn glyph_scale(ts: Transform) -> f64 {
    let (sx, sy, kx, ky) = (ts.sx.get(), ts.sy.get(), ts.kx.get(), ts.ky.get());
    sx.hypot(ky).min(kx.hypot(sy))
}

/// Text under any other transform can draw as something else than it reads: a 6 turned is a 9.
fn draws_as_it_reads(ts: Transform) -> bool {
    let (sx, sy, kx, ky) = (ts.sx.get(), ts.sy.get(), ts.kx.get(), ts.ky.get());
    let tolerance = TRANSFORM_TOLERANCE * sx.abs();
    sx > 0.0 && (sx - sy).abs() <= tolerance && kx.abs() <= tolerance && ky.abs() <= tolerance
}

/// Tracking can turn `12` into a drawn `21`, and word spacing can draw `1 2` as `12`. A space at a
/// line's end has no width, and separates nothing.
fn squeezed(text: &TextItem) -> bool {
    let is_space = |glyph: &Glyph| {
        text.text
            .get(glyph.range())
            .is_some_and(|said| !said.is_empty() && said.trim().is_empty())
    };
    let glyphs = text.glyphs.as_slice();
    let start = glyphs.iter().position(|glyph| !is_space(glyph));
    let end = glyphs.iter().rposition(|glyph| !is_space(glyph));
    let (Some(start), Some(end)) = (start, end) else {
        return false;
    };
    glyphs[start..=end].iter().any(|glyph| {
        let advance = glyph.x_advance.get();
        advance < 0.0 || (is_space(glyph) && advance < MIN_SPACE_ADVANCE)
    })
}

fn drawn_backwards(text: &TextItem) -> bool {
    text.glyphs
        .first()
        .zip(text.glyphs.last())
        .is_some_and(|(first, last)| first.range.start > last.range.start)
}

/// Natural bidi gives digits and left-to-right letters a run of their own, drawn forwards. In a run
/// drawn back to front they stand only by an override, and read otherwise than they draw.
fn overridden(text: &TextItem) -> bool {
    let is_rtl_script = |c: char| matches!(c as u32, 0x0590..=0x08FF | 0xFB1D..=0xFDFF | 0xFE70..=0xFEFF | 0x10800..=0x10FFF | 0x1E800..=0x1EFFF);
    drawn_backwards(text)
        && text
            .text
            .chars()
            .any(|c| c.is_numeric() || (c.is_alphabetic() && !is_rtl_script(c)))
}

/// A run's text for joining by where runs draw. A right-to-left run draws its logical last space
/// first, and it must stay between this run and the one drawn before it.
fn joinable_text(text: &TextItem) -> String {
    let raw = text.text.as_str();
    if !drawn_backwards(text) {
        return raw.to_owned();
    }
    let edge = |is_space: bool| if is_space { " " } else { "" };
    format!(
        "{}{}{}",
        edge(raw.ends_with(' ')),
        raw.trim_matches(' '),
        edge(raw.starts_with(' '))
    )
}

/// The first and last points of a run's baseline and the corners of its glyphs' ink, on the page.
fn run_points(text: &TextItem, pos: Point, ts: Transform) -> Vec<Point> {
    let mut points = vec![pos, pos + Point::with_x(text.width())];
    let bbox = text.bbox();
    // A run of glyphs without outlines leaves the box at infinity.
    if bbox.min.x.to_pt().is_finite() && bbox.max.x.to_pt().is_finite() {
        points.extend([
            pos + bbox.min,
            pos + bbox.max,
            pos + Point::new(bbox.min.x, bbox.max.y),
            pos + Point::new(bbox.max.x, bbox.min.y),
        ]);
    }
    points.into_iter().map(|p| p.transform(ts)).collect()
}

/// A tiling's content is a frame of its own, reached through a paint and not through the page.
fn tiling_frame(paint: &Paint) -> Option<&Frame> {
    match paint {
        Paint::Tiling(tiling) => Some(tiling.frame()),
        Paint::Solid(_) | Paint::Gradient(_) => None,
    }
}

pub(crate) fn rendered_text(document: &PagedDocument) -> Result<RenderedText, RenderError> {
    let mut result = RenderedText::default();
    let mut runs = Vec::new();
    let pages = document.pages();
    let mut stack: Vec<(&Frame, Transform, usize, Visible)> = pages
        .iter()
        .enumerate()
        .rev()
        .map(|(i, page)| {
            let size = page.frame.size();
            let visible = Visible {
                x0: 0.0,
                y0: 0.0,
                x1: size.x.to_pt(),
                y1: size.y.to_pt(),
            };
            (&page.frame, Transform::identity(), i, visible)
        })
        .collect();
    let mut next_bucket = pages.len();
    for page in pages {
        if let Smart::Custom(Some(paint)) = &page.fill
            && let Some(frame) = tiling_frame(paint)
        {
            stack.push((frame, Transform::identity(), next_bucket, Visible::NOTHING));
            next_bucket += 1;
        }
        // The prefix and suffix of a page label reach the PDF even when no footer shows them.
        if let Some(Numbering::Pattern(pattern)) = &page.numbering {
            let prefixes = pattern.pieces.iter().map(|(prefix, _)| prefix.as_str());
            let label: Vec<&str> = prefixes.chain([pattern.suffix.as_str()]).collect();
            let label = label.join(" ");
            if !label.trim().is_empty() {
                result.metadata.push(label);
            }
        }
    }
    let mut tilings = Vec::new();
    let mut links = Vec::new();
    while let Some((frame, ts, bucket, visible)) = stack.pop() {
        for (pos, item) in frame.items() {
            match item {
                FrameItem::Group(group) => {
                    let inner = ts
                        .pre_concat(Transform::translate(pos.x, pos.y))
                        .pre_concat(group.transform);
                    let visible = group.clip.as_ref().map_or(visible, |clip| {
                        let bbox = clip.bbox(None);
                        visible.clipped(bbox.min, bbox.max, inner)
                    });
                    stack.push((&group.frame, inner, bucket, visible));
                }
                FrameItem::Text(text) => {
                    tilings.extend(tiling_frame(&text.fill));
                    if let Some(stroke) = &text.stroke {
                        tilings.extend(tiling_frame(&stroke.paint));
                    }
                    if runs.len() == MAX_RUNS {
                        return Err(RenderError::TooManyPages);
                    }
                    let start = pos.transform(ts);
                    let end = (*pos + Point::with_x(text.width())).transform(ts);
                    let size = text.size.to_pt() * glyph_scale(ts);
                    let seen = run_points(text, *pos, ts)
                        .into_iter()
                        .all(|p| visible.contains(p))
                        && readable_paint(&text.fill)
                        && size >= MIN_READABLE_SIZE;
                    if !text.text.trim().is_empty() {
                        if !seen {
                            result.hidden.push(text.text.to_string());
                        }
                        if !draws_as_it_reads(ts) || squeezed(text) {
                            result.transformed.push(text.text.to_string());
                        }
                        if overridden(text) {
                            result.reversed.push(text.text.to_string());
                        }
                    }
                    runs.push(Run {
                        bucket,
                        x: start.x.to_pt(),
                        y: start.y.to_pt(),
                        end_x: end.x.to_pt(),
                        size: size.max(1.0),
                        text: joinable_text(text),
                    });
                }
                FrameItem::Shape(shape, _) => {
                    tilings.extend(shape.fill.as_ref().and_then(tiling_frame));
                    if let Some(stroke) = &shape.stroke {
                        tilings.extend(tiling_frame(&stroke.paint));
                    }
                }
                FrameItem::Image(..) => result.image_count += 1,
                FrameItem::Link(Destination::Url(url), size) => {
                    if links.len() == MAX_LINKS {
                        return Err(RenderError::TooManyPages);
                    }
                    links.push(PageLink::new(url.as_str(), bucket, *pos, *size, ts));
                }
                FrameItem::Link(..) | FrameItem::Tag(_) => {}
            }
            for frame in tilings.drain(..) {
                stack.push((frame, Transform::identity(), next_bucket, Visible::NOTHING));
                next_bucket += 1;
            }
        }
    }
    result.links = links.iter().map(|link| link.read(&runs)).collect();
    result.lines = lines_from_runs(runs, &mut result.transformed);
    let info = document.info();
    result
        .metadata
        .extend(info.title.iter().map(|t| t.to_string()));
    result
        .metadata
        .extend(info.author.iter().map(|a| a.to_string()));
    result
        .metadata
        .extend(info.description.iter().map(|d| d.to_string()));
    result
        .metadata
        .extend(info.keywords.iter().map(|k| k.to_string()));
    // A show rule can keep a heading off the page, while the PDF still lists it as a bookmark.
    let headings = document.introspector().query(&HeadingElem::ELEM.select());
    for heading in &headings {
        if let Some(heading) = heading.to_packed::<HeadingElem>() {
            // As the PDF exporter builds a bookmark's title.
            let text = heading.body.plain_text();
            result.metadata.push(match &heading.numbers {
                Some(numbers) => format!("{numbers} {text}"),
                None => text.to_string(),
            });
        }
    }
    if let Smart::Custom(Some(date)) = &info.date {
        result.metadata.push(format!(
            "{:04}-{:02}-{:02}",
            date.year().unwrap_or_default(),
            date.month().unwrap_or_default(),
            date.day().unwrap_or_default()
        ));
    }
    // Its size as JSON is what crosses to the parent.
    let bytes = serde_json::to_vec(&result).map_err(|e| RenderError::Worker(e.to_string()))?;
    if bytes.len() > MAX_TEXT_BYTES {
        return Err(RenderError::TooManyPages);
    }
    Ok(result)
}

/// A URL link's clickable area on the page.
struct PageLink {
    target: String,
    bucket: usize,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

impl PageLink {
    fn new(target: &str, bucket: usize, pos: Point, size: Size, ts: Transform) -> Self {
        let a = pos.transform(ts);
        let b = (pos + size.to_point()).transform(ts);
        Self {
            target: target.to_owned(),
            bucket,
            x0: a.x.to_pt().min(b.x.to_pt()),
            y0: a.y.to_pt().min(b.y.to_pt()),
            x1: a.x.to_pt().max(b.x.to_pt()),
            y1: a.y.to_pt().max(b.y.to_pt()),
        }
    }

    /// The runs whose middle lies in the area, which is what a reader takes the link to be.
    fn read(&self, runs: &[Run]) -> RenderedLink {
        let inside = runs.iter().filter(|run| {
            let x = (run.x + run.end_x) / 2.0;
            let y = run.y - RUN_MIDDLE * run.size;
            run.bucket == self.bucket
                && (self.x0..=self.x1).contains(&x)
                && (self.y0..=self.y1).contains(&y)
        });
        RenderedLink {
            target: self.target.clone(),
            text: {
                let mut inside: Vec<Run> = inside.cloned().collect();
                inside.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal));
                join_line(inside)
            },
        }
    }
}

/// A line's runs that stand on another's text by more than a word gap's width, as `#h(-1em)` does.
/// A PDF reader then joins what the lines keep apart. `line` is sorted by where runs start.
fn overlapping(line: &[Run]) -> impl Iterator<Item = &str> {
    let mut end = f64::NEG_INFINITY;
    line.iter()
        .filter(|run| !run.text.trim().is_empty())
        .filter(move |run| {
            let overlaps = run.x < end - WORD_GAP * run.size;
            end = end.max(run.end_x);
            overlaps
        })
        .map(|run| run.text.as_str())
}

fn lines_from_runs(mut runs: Vec<Run>, drawn_over: &mut Vec<String>) -> Vec<String> {
    runs.sort_by(|a, b| {
        (a.bucket, a.y, a.x)
            .partial_cmp(&(b.bucket, b.y, b.x))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut lines = Vec::new();
    let mut line: Vec<Run> = Vec::new();
    let mut finish = |mut line: Vec<Run>| {
        line.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal));
        drawn_over.extend(overlapping(&line).map(str::to_owned));
        lines.push(join_line(line));
    };
    for run in runs {
        let same_line = line.first().is_some_and(|first: &Run| {
            first.bucket == run.bucket
                && (run.y - first.y).abs() <= LINE_GAP * first.size.max(run.size)
        });
        if !same_line && !line.is_empty() {
            finish(std::mem::take(&mut line));
        }
        line.push(run);
    }
    if !line.is_empty() {
        finish(line);
    }
    lines
}

/// Joins runs in the order given, which is where they start on the page.
fn join_line(line: Vec<Run>) -> String {
    let mut text = String::new();
    let mut end = f64::NEG_INFINITY;
    for run in line {
        // A run of only spaces says nothing the gap to the next run doesn't, and a tiny one draws none.
        if run.text.trim().is_empty() {
            continue;
        }
        let gap = run.x - end;
        if gap > WORD_GAP * run.size && !text.is_empty() && !text.ends_with(' ') {
            text.push(' ');
        }
        text.push_str(&run.text);
        end = end.max(run.end_x);
    }
    // Typst writes the hyphen of a broken word as a soft hyphen at the end of the run.
    let text = text.trim();
    match text.strip_suffix('\u{ad}') {
        Some(rest) => format!("{rest}-"),
        None => text.to_owned(),
    }
}
