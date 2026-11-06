//! Reads back what the laid-out pages say, for the integrity check.

use ttf_parser::{GlyphId, OutlineBuilder};
use typst::foundations::NativeElement;
use typst::foundations::Smart;
use typst::introspection::Introspector;
use typst::layout::{Abs, Frame, FrameItem, Point, Size, Transform};
use typst::model::{Destination, Document, HeadingElem, Numbering};
use typst::text::{Glyph, TextItem};
use typst::visualize::{Color, Curve, CurveItem, FillRule, Geometry, LineCap, Paint};
use typst_layout::{Page, PagedDocument};
use unicode_normalization::char::is_combining_mark;

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
/// Text this transparent or this close to white can't be seen on a page, so it counts as hidden. At
/// 0.8 a word on white is already a faint blot. The template's muted grey is well under this.
const MIN_READABLE_ALPHA: f32 = 0.3;
const MAX_READABLE_LUMA: f32 = 0.75;
/// How far text may stand outside what shows, in points, before it counts as hidden.
const VISIBLE_MARGIN: f64 = 1.0;
/// How high above its baseline a run's letters stand, and how far into a link's area a run must reach
/// to count as read through it. In ems. Runs either side of a link touch it, and don't reach in.
const RUN_HEIGHT: f64 = 0.7;
const MIN_LINK_REACH: f64 = 0.1;
/// How much of the shorter box two link boxes must share to stand on one line.
const LINK_LINE_SHARE: f64 = 0.5;
/// A word space narrower than this draws no gap, in ems. Justifying shrinks one to 0.17 at the least.
const MIN_SPACE_ADVANCE: f64 = 0.15;
/// A letter or digit advancing less than this share of its own width draws onto the next. Tracking of
/// -0.3 em, an advance of 0.17 em on a digit, draws `1000` as a blot read as other digits. The fonts'
/// tightest kerned pair stays above this, which `no_kerned_pair_counts_as_squeezed` tests.
const MIN_ADVANCE_SHARE: f64 = 0.6;
/// Fonts kern punctuation far tighter, as in `."`, to under 0.1 em, so it only has this floor, in ems.
const MIN_ADVANCE: f64 = 0.05;
/// What a glyph's advance may exceed its own by, in ems. Past that, `1000` is read as `1 0 0 0`.
const MAX_ADVANCE_ADDED: f64 = 1.0;
/// A stroke thicker than this, in ems, closes the gaps that tell one letter from another.
const MAX_STROKE: f64 = 0.1;
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

/// A stroke paints over the glyph it outlines, so a white one hides it, and a thick one blots it.
fn readable_stroke(text: &TextItem) -> bool {
    text.stroke.as_ref().is_none_or(|stroke| {
        readable_paint(&stroke.paint) && stroke.thickness.to_pt() <= MAX_STROKE * text.size.to_pt()
    })
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

/// Tracking can turn `12` into a drawn `21` and `1000` into `10`, or spread `10` into `1 0`, and word
/// spacing can draw `1 2` as `12`. A space at a line's end has no width, and separates nothing. Marks
/// and invisible characters take no room of their own, whatever their glyph's width says.
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
    let said = |glyph: &Glyph| text.text.get(glyph.range()).unwrap_or_default();
    glyphs[start..=end].iter().any(|glyph| {
        let advance = glyph.x_advance.get();
        if advance < 0.0 || (is_space(glyph) && advance < MIN_SPACE_ADVANCE) {
            return true;
        }
        let own = text.font.x_advance(glyph.id).map_or(0.0, |own| own.get());
        let least = if said(glyph).chars().any(char::is_alphanumeric) {
            MIN_ADVANCE_SHARE * own
        } else {
            MIN_ADVANCE.min(own)
        };
        !is_space(glyph)
            && !said(glyph).chars().all(is_combining_mark)
            && (advance < least || advance > own + MAX_ADVANCE_ADDED)
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
                        && readable_stroke(text)
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
    for page in pages {
        covered_words(page, &mut result.hidden)?;
    }
    result.links = read_links(links, &runs);
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

/// How much contrast with what lies beneath it a glyph's ink needs to be read: what the limit on text
/// colour leaves on white. Euclidean distance in sRGB, with black on white as 1.
const MIN_INK_CONTRAST: f32 = 1.0 - MAX_READABLE_LUMA;
/// The share of a glyph's ink that may be erased, by a paint nothing tells from what lies beneath it.
/// Measured on real covers: `E` draws as `F` at about 7% and `Q` as `O` at about 20%. Under this only
/// serifs go, and the dot of an `i`, at 3 to 5%, which reads the same. Nothing honest erases any.
const MAX_ERASED_INK: f64 = 0.05;
/// The share of a letter or digit that may lie under a paint a reader can see. Measured on honest
/// sources: a normal `#strike` takes up to 15%, in red at 1 pt 29%, and a 1 pt box hugging the text 41%.
/// A dash or a plus sign under a normal `#strike` is mostly covered too, so other glyphs aren't judged
/// by this: a line over a dash is still seen as a line.
const MAX_COVERED_INK: f64 = 0.5;
/// Samples along each side of a glyph's ink box. Those inside its outline are its ink.
const INK_GRID: u32 = 16;
/// Segments to a Bezier curve, for testing what it covers.
const CURVE_SEGMENTS: u32 = 12;
/// A page's height is cut into this many bands, so that a glyph is only held against what is near it.
const PAINT_BANDS: usize = 64;
/// On one page. Layout memoization can make a small source repeat a frame exponentially often.
const MAX_PAINTS: usize = 100_000;

type Rgb = [f32; 3];
type At = (f64, f64);

/// What a paint does to whatever lies under it.
#[derive(Clone)]
enum Layer {
    Solid(Rgb, f32),
    /// A gradient's stops, any of which can stand under or over a glyph.
    Stops(Vec<Rgb>),
    /// A tiling, whose colours aren't known here.
    Unknown,
}

impl Layer {
    fn of(paint: &Paint) -> Self {
        match paint {
            Paint::Solid(color) => {
                let (rgb, alpha) = rgb_of(color);
                Self::Solid(rgb, alpha)
            }
            Paint::Gradient(gradient) => Self::Stops(
                gradient
                    .stops_ref()
                    .iter()
                    .map(|(c, _)| rgb_of(c).0)
                    .collect(),
            ),
            Paint::Tiling(_) => Self::Unknown,
        }
    }

    /// How much of what lies under it shows through. Only a solid colour is known to let any through.
    fn through(&self) -> f32 {
        match self {
            Self::Solid(_, alpha) => 1.0 - alpha,
            Self::Stops(_) | Self::Unknown => 0.0,
        }
    }
}

fn rgb_of(color: &Color) -> (Rgb, f32) {
    let color = color.to_rgb();
    ([color.red, color.green, color.blue], color.alpha)
}

fn contrast(a: Rgb, b: Rgb) -> f32 {
    let squares: f32 = a.iter().zip(&b).map(|(a, b)| (a - b) * (a - b)).sum();
    (squares / 3.0).sqrt()
}

/// What a shape's paint reaches, in the coordinates it was drawn in.
enum Reach {
    Filled {
        outlines: Vec<Vec<At>>,
        even_odd: bool,
    },
    /// A dash pattern counts as solid: its gaps are the source's to choose, and a thin gap shows nothing.
    Stroked {
        segments: Vec<(At, At)>,
        half: f64,
        butt: bool,
    },
    /// A glyph painting over another counts by its box: no honest text stands on other text.
    Glyph { min: At, max: At },
}

fn winding(outline: &[At], (x, y): At) -> i32 {
    let mut winding = 0;
    for (index, &(x0, y0)) in outline.iter().enumerate() {
        let (x1, y1) = outline[(index + 1) % outline.len()];
        if (y0 <= y) != (y1 <= y) && x < x0 + (y - y0) / (y1 - y0) * (x1 - x0) {
            winding += if y1 > y0 { 1 } else { -1 };
        }
    }
    winding
}

fn inside(outlines: &[Vec<At>], point: At, even_odd: bool) -> bool {
    let total: i32 = outlines.iter().map(|outline| winding(outline, point)).sum();
    // Each crossing changes the winding by one, so its parity is the crossings'.
    if even_odd { total % 2 != 0 } else { total != 0 }
}

fn near_segment((a, b): (At, At), (x, y): At, half: f64, butt: bool) -> bool {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let length = dx * dx + dy * dy;
    let along = if length > 0.0 {
        ((x - a.0) * dx + (y - a.1) * dy) / length
    } else {
        0.0
    };
    if butt && !(0.0..=1.0).contains(&along) {
        return false;
    }
    let along = along.clamp(0.0, 1.0);
    (x - a.0 - along * dx).hypot(y - a.1 - along * dy) <= half
}

impl Reach {
    fn holds(&self, point: At) -> bool {
        match self {
            Self::Filled { outlines, even_odd } => inside(outlines, point, *even_odd),
            Self::Stroked {
                segments,
                half,
                butt,
            } => segments
                .iter()
                .any(|segment| near_segment(*segment, point, *half, *butt)),
            Self::Glyph { min, max } => {
                (min.0..=max.0).contains(&point.0) && (min.1..=max.1).contains(&point.1)
            }
        }
    }
}

/// The points after `from` of a flattened Bezier curve.
fn cubic(from: At, a: At, b: At, to: At) -> impl Iterator<Item = At> {
    (1..=CURVE_SEGMENTS).map(move |step| {
        let t = f64::from(step) / f64::from(CURVE_SEGMENTS);
        let u = 1.0 - t;
        let mix = |p0: f64, p1: f64, p2: f64, p3: f64| {
            u * u * u * p0 + 3.0 * u * u * t * p1 + 3.0 * u * t * t * p2 + t * t * t * p3
        };
        (mix(from.0, a.0, b.0, to.0), mix(from.1, a.1, b.1, to.1))
    })
}

/// A curve's parts as polylines, and whether each closes.
fn flatten(curve: &Curve) -> Vec<(Vec<At>, bool)> {
    let at = |point: &Point| (point.x.to_pt(), point.y.to_pt());
    let mut parts: Vec<(Vec<At>, bool)> = Vec::new();
    for item in &curve.0 {
        match item {
            CurveItem::Move(to) => parts.push((vec![at(to)], false)),
            CurveItem::Line(to) => {
                if let Some((part, _)) = parts.last_mut() {
                    part.push(at(to));
                }
            }
            CurveItem::Cubic(a, b, to) => {
                if let Some((part, _)) = parts.last_mut()
                    && let Some(&from) = part.last()
                {
                    part.extend(cubic(from, at(a), at(b), at(to)));
                }
            }
            CurveItem::Close => {
                if let Some((_, closed)) = parts.last_mut() {
                    *closed = true;
                }
            }
        }
    }
    parts
}

/// The polylines of a shape's geometry, and whether each closes.
fn parts_of(geometry: &Geometry) -> Vec<(Vec<At>, bool)> {
    match geometry {
        Geometry::Line(to) => vec![(vec![(0.0, 0.0), (to.x.to_pt(), to.y.to_pt())], false)],
        Geometry::Rect(size) => {
            let (w, h) = (size.x.to_pt(), size.y.to_pt());
            vec![(vec![(0.0, 0.0), (w, 0.0), (w, h), (0.0, h)], true)]
        }
        Geometry::Curve(curve) => flatten(curve),
    }
}

fn segments_of(parts: &[(Vec<At>, bool)]) -> Vec<(At, At)> {
    let mut segments = Vec::new();
    for (outline, closed) in parts {
        segments.extend(outline.windows(2).map(|pair| (pair[0], pair[1])));
        if let (true, Some(first), Some(last)) = (*closed, outline.first(), outline.last()) {
            segments.push((*last, *first));
        }
    }
    segments
}

fn on_page(ts: Transform, (x, y): At) -> At {
    let point = Point::new(Abs::pt(x), Abs::pt(y)).transform(ts);
    (point.x.to_pt(), point.y.to_pt())
}

/// The box on the page around these local points, each grown by `margin`.
fn page_box(ts: Transform, margin: f64, points: impl Iterator<Item = At>) -> (At, At) {
    let mut min = (f64::INFINITY, f64::INFINITY);
    let mut max = (f64::NEG_INFINITY, f64::NEG_INFINITY);
    for (x, y) in points {
        for corner in [
            (x - margin, y - margin),
            (x + margin, y + margin),
            (x - margin, y + margin),
            (x + margin, y - margin),
        ] {
            let (x, y) = on_page(ts, corner);
            min = (min.0.min(x), min.1.min(y));
            max = (max.0.max(x), max.1.max(y));
        }
    }
    (min, max)
}

/// One thing painted on a page.
struct Painted<'a> {
    min: At,
    max: At,
    /// The innermost clip it was painted under, in [`Walk::clips`].
    clip: Option<usize>,
    /// From the page to the coordinates `reach` is in.
    to_local: Transform,
    reach: Reach,
    layer: Layer,
    glyph: Option<PaintedGlyph<'a>>,
}

struct PaintedGlyph<'a> {
    run: usize,
    text: &'a TextItem,
    index: usize,
    /// Where it stands in the coordinates of its run's position, and that position's transform.
    at: At,
    ts: Transform,
}

/// A clip's outline on the page, and the clip around it.
struct Clip {
    outlines: Vec<Vec<At>>,
    parent: Option<usize>,
}

/// Everything painted on one page, in the order it is painted, as `typst_render` and the PDF do it.
#[derive(Default)]
struct Walk<'a> {
    paints: Vec<Painted<'a>>,
    clips: Vec<Clip>,
}

impl<'a> Walk<'a> {
    fn add(&mut self, painted: Painted<'a>) -> Result<(), RenderError> {
        if self.paints.len() == MAX_PAINTS {
            return Err(RenderError::TooManyPages);
        }
        self.paints.push(painted);
        Ok(())
    }

    fn shape(
        &mut self,
        ts: Transform,
        clip: Option<usize>,
        shape: &typst::visualize::Shape,
    ) -> Result<(), RenderError> {
        // A transform that flattens a shape paints nothing.
        let Some(to_local) = ts.invert() else {
            return Ok(());
        };
        let parts = parts_of(&shape.geometry);
        let points = || {
            parts
                .iter()
                .flat_map(|(outline, _)| outline.iter().copied())
        };
        if let Some(fill) = &shape.fill {
            let (min, max) = page_box(ts, 0.0, points());
            self.add(Painted {
                min,
                max,
                clip,
                to_local,
                reach: Reach::Filled {
                    outlines: parts.iter().map(|(outline, _)| outline.clone()).collect(),
                    even_odd: matches!(shape.fill_rule, FillRule::EvenOdd),
                },
                layer: Layer::of(fill),
                glyph: None,
            })?;
        }
        if let Some(stroke) = &shape.stroke {
            let half = stroke.thickness.to_pt() / 2.0;
            let (min, max) = page_box(ts, half, points());
            self.add(Painted {
                min,
                max,
                clip,
                to_local,
                reach: Reach::Stroked {
                    segments: segments_of(&parts),
                    half,
                    butt: matches!(stroke.cap, LineCap::Butt),
                },
                layer: Layer::of(&stroke.paint),
                glyph: None,
            })?;
        }
        Ok(())
    }

    fn text(
        &mut self,
        ts: Transform,
        clip: Option<usize>,
        run: usize,
        text: &'a TextItem,
    ) -> Result<(), RenderError> {
        let Some(to_local) = ts.invert() else {
            return Ok(());
        };
        let size = text.size;
        let (mut x, mut y) = (0.0, 0.0);
        for (index, glyph) in text.glyphs.iter().enumerate() {
            let at = (
                x + glyph.x_offset.at(size).to_pt(),
                -(y + glyph.y_offset.at(size).to_pt()),
            );
            x += glyph.x_advance.at(size).to_pt();
            y += glyph.y_advance.at(size).to_pt();
            let Some(ink) = text.font.ttf().glyph_bounding_box(GlyphId(glyph.id)) else {
                continue;
            };
            let scale = |units: i16| text.font.to_em(units).at(size).to_pt();
            let (low, high) = (
                (at.0 + scale(ink.x_min), at.1 - scale(ink.y_max)),
                (at.0 + scale(ink.x_max), at.1 - scale(ink.y_min)),
            );
            let (min, max) = page_box(
                ts,
                0.0,
                [low, high, (low.0, high.1), (high.0, low.1)].into_iter(),
            );
            self.add(Painted {
                min,
                max,
                clip,
                to_local,
                reach: Reach::Glyph {
                    min: low,
                    max: high,
                },
                layer: Layer::of(&text.fill),
                glyph: Some(PaintedGlyph {
                    run,
                    text,
                    index,
                    at,
                    ts,
                }),
            })?;
        }
        Ok(())
    }

    /// A group's contents are painted where the group stands, under its transform and clip.
    fn page(&mut self, page: &'a Page) -> Result<(), RenderError> {
        let mut stack = vec![(page.frame.items(), Transform::identity(), None)];
        let mut run = 0;
        while let Some((items, ts, clip)) = stack.last_mut() {
            let (ts, clip) = (*ts, *clip);
            let Some((pos, item)) = items.next() else {
                stack.pop();
                continue;
            };
            let here = ts.pre_concat(Transform::translate(pos.x, pos.y));
            match item {
                FrameItem::Group(group) => {
                    let inner = here.pre_concat(group.transform);
                    let clip = group.clip.as_ref().map_or(clip, |curve| {
                        let outline = |(part, _): (Vec<At>, bool)| {
                            part.into_iter().map(|at| on_page(inner, at)).collect()
                        };
                        self.clips.push(Clip {
                            outlines: flatten(curve).into_iter().map(outline).collect(),
                            parent: clip,
                        });
                        Some(self.clips.len() - 1)
                    });
                    stack.push((group.frame.items(), inner, clip));
                }
                FrameItem::Text(text) => {
                    self.text(here, clip, run, text)?;
                    run += 1;
                }
                FrameItem::Shape(shape, _) => self.shape(here, clip, shape)?,
                FrameItem::Image(..) | FrameItem::Link(..) | FrameItem::Tag(_) => {}
            }
        }
        Ok(())
    }

    fn paints_at(&self, paint: &Painted<'_>, point: At) -> bool {
        let mut clip = paint.clip;
        while let Some(at) = clip {
            if !inside(&self.clips[at].outlines, point, false) {
                return false;
            }
            clip = self.clips[at].parent;
        }
        (paint.min.0..=paint.max.0).contains(&point.0)
            && (paint.min.1..=paint.max.1).contains(&point.1)
            && paint.reach.holds(on_page(paint.to_local, point))
    }
}

/// Collects a glyph's outline, in font units, as closed polylines.
#[derive(Default)]
struct Outline(Vec<Vec<At>>);

impl OutlineBuilder for Outline {
    fn move_to(&mut self, x: f32, y: f32) {
        self.0.push(vec![(f64::from(x), f64::from(y))]);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        if let Some(part) = self.0.last_mut() {
            part.push((f64::from(x), f64::from(y)));
        }
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let (c, to) = ((f64::from(x1), f64::from(y1)), (f64::from(x), f64::from(y)));
        if let Some(part) = self.0.last_mut()
            && let Some(&from) = part.last()
        {
            let third = |a: f64, b: f64| a + 2.0 / 3.0 * (b - a);
            let (a, b) = (
                (third(from.0, c.0), third(from.1, c.1)),
                (third(to.0, c.0), third(to.1, c.1)),
            );
            part.extend(cubic(from, a, b, to));
        }
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let point = |x: f32, y: f32| (f64::from(x), f64::from(y));
        if let Some(part) = self.0.last_mut()
            && let Some(&from) = part.last()
        {
            part.extend(cubic(from, point(x1, y1), point(x2, y2), point(x, y)));
        }
    }

    fn close(&mut self) {}
}

/// Points on a glyph's ink, on the page: a grid over its box, kept where its outline holds them. So a
/// line traced along a thin diagonal counts for all it takes, not for the little of a box it fills.
fn ink_samples(glyph: &PaintedGlyph<'_>) -> Vec<At> {
    let font = glyph.text.font.ttf();
    let mut outline = Outline::default();
    let id = GlyphId(glyph.text.glyphs[glyph.index].id);
    let Some(ink) = font.outline_glyph(id, &mut outline) else {
        return Vec::new();
    };
    let scale = glyph.text.size.to_pt() / f64::from(font.units_per_em());
    let cell = |low: i16, high: i16, step: u32| {
        let share = (f64::from(step) + 0.5) / f64::from(INK_GRID);
        f64::from(low) + share * (f64::from(high) - f64::from(low))
    };
    let mut samples = Vec::new();
    for row in 0..INK_GRID {
        for column in 0..INK_GRID {
            let point = (
                cell(ink.x_min, ink.x_max, column),
                cell(ink.y_min, ink.y_max, row),
            );
            if inside(&outline.0, point, false) {
                let local = (glyph.at.0 + scale * point.0, glyph.at.1 - scale * point.1);
                samples.push(on_page(glyph.ts, local));
            }
        }
    }
    samples
}

/// The colours what lies under a point can be, its layers given top first. None if they aren't known.
fn backdrop<'p>(layers: impl Iterator<Item = &'p Layer>) -> Option<Vec<Rgb>> {
    let mut translucent = Vec::new();
    let mut colours = vec![[1.0; 3]];
    for layer in layers {
        match layer {
            Layer::Solid(rgb, alpha) if *alpha < 1.0 => translucent.push((*rgb, *alpha)),
            Layer::Solid(rgb, _) => {
                colours = vec![*rgb];
                break;
            }
            Layer::Stops(stops) => {
                colours.clone_from(stops);
                break;
            }
            Layer::Unknown => return None,
        }
    }
    for (rgb, alpha) in translucent.into_iter().rev() {
        for colour in &mut colours {
            for channel in 0..3 {
                colour[channel] = alpha * rgb[channel] + (1.0 - alpha) * colour[channel];
            }
        }
    }
    Some(colours)
}

/// The word of `text` around a glyph, for saying what is hidden.
fn word_at(text: &TextItem, glyph: &Glyph) -> String {
    let said = text.text.as_str();
    let range = glyph.range();
    let start = said[..range.start]
        .rfind(char::is_whitespace)
        .map_or(0, |at| at + 1);
    let end = said[range.end..]
        .find(char::is_whitespace)
        .map_or(said.len(), |at| range.end + at);
    said[start..end].to_owned()
}

/// What became of a glyph's ink at one point.
#[derive(PartialEq)]
enum Ink {
    Shows,
    /// Under a paint a reader can see, or on one of its own colour, which a reader can see too.
    Covered,
    /// Gone with no trace: under a paint nothing tells from what lies beneath, or in the page's colour.
    Erased,
}

/// Judges the ink of the glyph at `index` at `point`. `near` holds what may reach it, in paint order.
fn ink_at(walk: &Walk<'_>, index: usize, near: &[u32], page: Option<&Layer>, point: At) -> Ink {
    let (fill, alpha) = match &walk.paints[index].layer {
        Layer::Solid(rgb, alpha) => (*rgb, *alpha),
        // The text rules refuse such a paint already.
        Layer::Stops(_) | Layer::Unknown => return Ink::Shows,
    };
    let mut through = 1.0;
    let mut top = None;
    let mut below = Vec::new();
    for other in near.iter().rev().map(|other| *other as usize) {
        let paint = &walk.paints[other];
        if walk.paints_at(paint, point) {
            if other > index {
                through *= paint.layer.through();
                top.get_or_insert(&paint.layer);
            } else {
                below.push(&paint.layer);
            }
        }
    }
    let Some(colours) = backdrop(below.into_iter().chain(page)) else {
        return Ink::Erased;
    };
    let shows = colours
        .iter()
        .map(|colour| alpha * contrast(fill, *colour))
        .fold(f32::MAX, f32::min);
    if through * shows >= MIN_INK_CONTRAST {
        return Ink::Shows;
    }
    // Text of the page's own colour leaves no trace. On a shape of its own colour, the shape shows.
    let tell_apart = |rgb: Rgb, from: &[Rgb]| {
        from.iter()
            .all(|colour| contrast(rgb, *colour) >= MIN_INK_CONTRAST)
    };
    let seen = match top {
        Some(Layer::Solid(rgb, _)) => tell_apart(*rgb, &colours),
        Some(Layer::Stops(_) | Layer::Unknown) => false,
        None => backdrop(page.into_iter()).is_some_and(|colours| tell_apart(fill, &colours)),
    };
    if seen { Ink::Covered } else { Ink::Erased }
}

/// The words on `page` with a glyph a reader can't read: painted over, past what a glyph can lose, or on
/// a paint of nearly its own colour. It reads the frames, in paint order, with no second draw. The stamp
/// isn't there yet, as it is added after the document has been read.
fn covered_words(page: &Page, hidden: &mut Vec<String>) -> Result<(), RenderError> {
    let mut walk = Walk::default();
    walk.page(page)?;
    let page_layer = page.fill_or_white().map(|paint| Layer::of(&paint));
    let band_height = page.frame.height().to_pt().max(1.0) / PAINT_BANDS as f64;
    let band = |y: f64| ((y / band_height).max(0.0) as usize).min(PAINT_BANDS - 1);
    let mut bands: Vec<Vec<u32>> = vec![Vec::new(); PAINT_BANDS];
    for (index, paint) in walk.paints.iter().enumerate() {
        for list in &mut bands[band(paint.min.1)..=band(paint.max.1)] {
            list.push(index as u32);
        }
    }
    let mut near = Vec::new();
    let mut last_word = None;
    for (index, paint) in walk.paints.iter().enumerate() {
        let Some(glyph) = &paint.glyph else {
            continue;
        };
        near.clear();
        for list in &bands[band(paint.min.1)..=band(paint.max.1)] {
            near.extend(list.iter().copied().filter(|other| {
                let other = *other as usize;
                let them = &walk.paints[other];
                other != index
                    && them.min.0 <= paint.max.0
                    && paint.min.0 <= them.max.0
                    && them.min.1 <= paint.max.1
                    && paint.min.1 <= them.max.1
                    // A glyph is mostly gaps from beneath, and a run's own glyphs don't cover it.
                    && them.glyph.as_ref().is_none_or(|them| other > index && them.run != glyph.run)
            }));
        }
        near.sort_unstable();
        near.dedup();
        // Nearly every glyph has nothing near it, and so one answer for all its ink.
        if near.is_empty()
            && ink_at(&walk, index, &near, page_layer.as_ref(), paint.min) == Ink::Shows
        {
            continue;
        }
        let samples = ink_samples(glyph);
        let mut covered = 0_u32;
        let mut erased = 0_u32;
        for point in &samples {
            match ink_at(&walk, index, &near, page_layer.as_ref(), *point) {
                Ink::Shows => {}
                Ink::Covered => covered += 1,
                Ink::Erased => erased += 1,
            }
        }
        let said = glyph.text.glyphs[glyph.index].range();
        let is_letter = glyph.text.text[said].chars().any(char::is_alphanumeric);
        let total = samples.len() as f64;
        let lost = f64::from(erased) > MAX_ERASED_INK * total
            || (is_letter && f64::from(covered + erased) > MAX_COVERED_INK * total);
        if lost {
            let word = word_at(glyph.text, &glyph.text.glyphs[glyph.index]);
            if last_word.as_ref() != Some(&(glyph.run, word.clone())) {
                hidden.push(word.clone());
                last_word = Some((glyph.run, word));
            }
        }
    }
    Ok(())
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

    /// The runs reaching into the area, which is what a reader may take the link to be: a link over
    /// part of a run counts as over all of it.
    fn read(&self, runs: &[Run]) -> String {
        let reach = |from: f64, to: f64, low: f64, high: f64| to.min(high) - from.max(low);
        let mut inside: Vec<Run> = runs
            .iter()
            .filter(|run| {
                let least = MIN_LINK_REACH * run.size;
                run.bucket == self.bucket
                    && reach(run.x, run.end_x, self.x0, self.x1) > least
                    && reach(run.y - RUN_HEIGHT * run.size, run.y, self.y0, self.y1) > least
            })
            .cloned()
            .collect();
        inside.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal));
        join_line(inside)
    }

    fn height(&self) -> f64 {
        self.y1 - self.y0
    }

    /// Whether the two boxes share a line: one stands over half of the shorter's height into the other.
    /// A box is taller than the line spacing, so boxes on neighbouring lines overlap by a little.
    fn same_line(&self, other: &Self) -> bool {
        self.bucket == other.bucket
            && self.y1.min(other.y1) - self.y0.max(other.y0)
                > LINK_LINE_SHARE * self.height().min(other.height())
    }

    /// Whether `next`, the following link in reading order, reads on from this one: it goes to the
    /// same place, or stands right beside it on one line, so that a reader sees one link.
    fn reads_on(&self, next: &Self) -> bool {
        let gap = WORD_GAP * self.height().max(next.height());
        self.bucket == next.bucket
            && (self.target == next.target
                || (self.same_line(next) && (-gap..=gap).contains(&(next.x0 - self.x1))))
    }
}

/// `links` in reading order: by line, then along it. A larger link's box starts higher, so its top alone
/// would put it before whatever stands to its left on the line.
fn in_reading_order(mut links: Vec<PageLink>) -> Vec<PageLink> {
    links.sort_by(|a, b| {
        (a.bucket, a.y0, a.x0)
            .partial_cmp(&(b.bucket, b.y0, b.x0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut ordered: Vec<PageLink> = Vec::with_capacity(links.len());
    let mut line = 0;
    for link in links {
        if !ordered[line..].iter().any(|first| first.same_line(&link)) {
            ordered[line..]
                .sort_by(|a, b| a.x0.partial_cmp(&b.x0).unwrap_or(std::cmp::Ordering::Equal));
            line = ordered.len();
        }
        ordered.push(link);
    }
    ordered[line..].sort_by(|a, b| a.x0.partial_cmp(&b.x0).unwrap_or(std::cmp::Ordering::Equal));
    ordered
}

/// Every link in reading order, with the text it covers and what it spells with its neighbours: an
/// address split over two links is still the address.
fn read_links(links: Vec<PageLink>, runs: &[Run]) -> Vec<RenderedLink> {
    let links = in_reading_order(links);
    let texts: Vec<String> = links.iter().map(|link| link.read(runs)).collect();
    let mut read = Vec::with_capacity(links.len());
    let mut start = 0;
    for end in 1..=links.len() {
        if end < links.len() && links[end - 1].reads_on(&links[end]) {
            continue;
        }
        let together = texts[start..end].concat();
        for (link, text) in links[start..end].iter().zip(&texts[start..end]) {
            read.push(RenderedLink {
                target: link.target.clone(),
                text: text.clone(),
                together: together.clone(),
            });
        }
        start = end;
    }
    read
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
