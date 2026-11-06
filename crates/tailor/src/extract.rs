//! Reads back what the laid-out pages say, for the integrity check.

use typst::foundations::NativeElement;
use typst::foundations::Smart;
use typst::introspection::Introspector;
use typst::layout::{Frame, FrameItem, Point, Transform};
use typst::model::{Destination, Document, HeadingElem, Numbering};
use typst::visualize::Paint;
use typst_layout::PagedDocument;

use crate::{RenderError, RenderedText};

/// Layout memoization can make a small source repeat a frame exponentially often.
const MAX_RUNS: usize = 200_000;
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
        x1: -1.0,
        y1: -1.0,
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

/// The font size as it lands on the page.
fn readable_size(size: f64, ts: Transform) -> bool {
    let (sx, sy, kx, ky) = (ts.sx.get(), ts.sy.get(), ts.kx.get(), ts.ky.get());
    size * (sx * sy - kx * ky).abs().sqrt() >= MIN_READABLE_SIZE
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
                    let seen = visible.contains(start)
                        && visible.contains(end)
                        && readable_paint(&text.fill)
                        && readable_size(text.size.to_pt(), ts);
                    if !seen && !text.text.trim().is_empty() {
                        result.hidden.push(text.text.to_string());
                    }
                    runs.push(Run {
                        bucket,
                        x: start.x.to_pt(),
                        y: start.y.to_pt(),
                        end_x: end.x.to_pt(),
                        size: text.size.to_pt().max(1.0),
                        text: text.text.to_string(),
                    });
                }
                FrameItem::Shape(shape, _) => {
                    tilings.extend(shape.fill.as_ref().and_then(tiling_frame));
                    if let Some(stroke) = &shape.stroke {
                        tilings.extend(tiling_frame(&stroke.paint));
                    }
                }
                FrameItem::Image(..) => result.image_count += 1,
                FrameItem::Link(Destination::Url(url), _) => {
                    result.metadata.push(url.as_str().to_owned());
                }
                FrameItem::Link(..) | FrameItem::Tag(_) => {}
            }
            for frame in tilings.drain(..) {
                stack.push((frame, Transform::identity(), next_bucket, Visible::NOTHING));
                next_bucket += 1;
            }
        }
    }
    result.lines = lines_from_runs(runs);
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
            result.metadata.push(heading.body.plain_text().to_string());
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
    Ok(result)
}

fn lines_from_runs(mut runs: Vec<Run>) -> Vec<String> {
    runs.sort_by(|a, b| {
        (a.bucket, a.y, a.x)
            .partial_cmp(&(b.bucket, b.y, b.x))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut lines = Vec::new();
    let mut line: Vec<Run> = Vec::new();
    for run in runs {
        let same_line = line.first().is_some_and(|first: &Run| {
            first.bucket == run.bucket
                && (run.y - first.y).abs() <= LINE_GAP * first.size.max(run.size)
        });
        if !same_line && !line.is_empty() {
            lines.push(join_line(std::mem::take(&mut line)));
        }
        line.push(run);
    }
    if !line.is_empty() {
        lines.push(join_line(line));
    }
    lines
}

fn join_line(mut line: Vec<Run>) -> String {
    line.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal));
    let mut text = String::new();
    let mut end = f64::NEG_INFINITY;
    for run in line {
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
