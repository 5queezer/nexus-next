//! Whether each glyph's ink reaches a reader, judged on what the renderer really draws.
//!
//! Each page is drawn twice, with its text and without. Where a glyph's outline says ink should be,
//! the two pictures must differ by what a reader can see. Nothing here models paints, strokes, clips
//! or gradients, so nothing here can get them wrong: a word painted over draws the same with and
//! without it, whatever painted over it.

use std::collections::HashMap;

use tiny_skia as sk;
use ttf_parser::{GlyphId, OutlineBuilder};
use typst::layout::{Abs, Frame, FrameItem, GroupItem, Transform};
use typst::text::{Glyph, TextItem};
use typst::visualize::{FixedStroke, Geometry, Paint, Shape};
use typst_layout::Page;
use typst_render::RenderOptions;

/// How far apart two colours must be for a reader to tell them apart: see [`difference`]. Black on
/// white is about 1. The coordinator's number, between what real glyphs measure on white for `#ccc`, 0.146,
/// and for `#7dd3fc`, the palest colour that must pass, 0.167. Pure `#ffff00` measures 0.12, and pale yellow
/// `rgb(255,255,140)` 0.07.
const MIN_DIFFERENCE: f32 = 0.16;
/// A reader tells letters from their ground by lightness far more than by hue. The coordinator's number.
const CHROMA_WEIGHT: f32 = 0.5;
/// A pixel a glyph covers less of than this isn't judged. At 2 pixels to a point the thin strokes of
/// 10 pt text still reach it.
const MIN_JUDGED_COVERAGE: f32 = 0.25;
/// A pixel keeps at least this share of the change its coverage should make, or its ink didn't arrive.
const MIN_ARRIVED: f32 = 0.5;
/// The share of a glyph's ink that may vanish with no trace. The coordinator's number. Of 159 honest
/// sources none lost over 0.03, by how a pixel's edge falls. The least a cover changes a letter by, in
/// those measured: `e` as `c` 0.11, `i` without its dot 0.14, `ö` as `o` 0.17.
const MAX_ERASED_INK: f32 = 0.05;
/// The share of a letter or digit that may lie under a paint a reader can see. The coordinator's number.
/// A strike of 1 pt on 8 pt text takes 0.275, and a bar reading `8` as `3` beside it 0.32. On 7 pt text
/// that strike takes 0.37, and is refused. Other glyphs aren't judged by this: a line over a dash is
/// still seen as a line.
const MAX_COVERED_INK: f32 = 0.3;
/// The share of a glyph's ink that may lie under another run's glyphs. The coordinator's number. The `3`
/// of `$root(3, 27)$` has 0.15 to 0.25 under its root sign, by how its few pixels fall, and lines at no
/// leading 0.11. An `8` over a `3` takes 0.7. A `7` over a `1` takes about 0.4, and less at some
/// offsets: on one line, the rule on runs drawn over each other refuses it anyway.
const MAX_OVERLAID_INK: f32 = 0.33;
/// How much of a pixel a glyph must cover to count as ink there, for what is painted before it.
const LATER_INK: f32 = 0.5;
/// How far around a glyph's box to look for what surrounds it, in pixels.
const SURROUND: i64 = 2;
/// Pixels judged on one page, as a multiple of the page's own: a thousand page-sized glyphs would
/// otherwise take minutes. Honest text covers a fraction of its page once. Past it, the source is refused.
const MAX_INK_PAGES: u64 = 16;
/// How wide, in pixels of the check's drawing, it draws the thinnest line. Just over one, as `tiny-skia`
/// draws one pixel or less as a hairline, faded by how little it covers.
const LEAST_PIXELS: f64 = 1.1;
/// Drawn as in the previews. All 355 sources measured got the same verdict at 3 and at 4.
pub(crate) const PIXEL_PER_PT: f64 = 2.0;
pub(crate) const MAX_PIXELS: f64 = 2_200_000.0;

/// The scale a page is drawn at, for a preview and for this check.
pub(crate) fn render_options(page: &Page) -> RenderOptions {
    let size = page.frame.size();
    let area = size.x.to_pt().max(1.0) * size.y.to_pt().max(1.0);
    RenderOptions {
        pixel_per_pt: PIXEL_PER_PT.min((MAX_PIXELS / area).sqrt()).into(),
        ..RenderOptions::default()
    }
}

/// How much `ts` enlarges what it draws, along its wider axis.
fn enlargement(ts: Transform) -> f64 {
    let (sx, sy, kx, ky) = (ts.sx.get(), ts.sy.get(), ts.kx.get(), ts.ky.get());
    sx.hypot(ky).max(kx.hypot(sy)).max(f64::MIN_POSITIVE)
}

/// `stroke` as the harshest PDF viewer draws it: at least `least` wide, and with no dash shorter than
/// that, which it draws closed up. A viewer widens a hairline to a whole pixel, so a mesh of hairlines
/// that leaves a word readable here leaves nothing of it there.
fn widened_stroke(stroke: &FixedStroke, least: Abs) -> FixedStroke {
    let mut stroke = stroke.clone();
    stroke.thickness = stroke.thickness.max(least);
    let thin_dash = stroke
        .dash
        .as_ref()
        .is_some_and(|dash| dash.array.iter().step_by(2).any(|on| *on < least));
    if thin_dash {
        stroke.dash = None;
    }
    stroke
}

/// Whether a paint draws nothing at all, as a fully transparent colour doesn't.
fn paints_nothing(paint: &Paint) -> bool {
    matches!(paint, Paint::Solid(color) if color.alpha() == Some(0.0))
}

/// `shape` as that viewer draws it. A thin filled shape is widened as a stroke is. A rectangle wide enough
/// needs none of it, and a paint of no colour draws nothing: a source can repeat either thousands of times
/// over a whole page, and this mustn't be what makes that slow.
fn widened(shape: &Shape, least: Abs) -> Vec<Shape> {
    let mut shapes = Vec::with_capacity(2);
    if let Some(fill) = shape.fill.as_ref().filter(|fill| !paints_nothing(fill)) {
        let wide =
            matches!(shape.geometry, Geometry::Rect(size) if size.x >= least && size.y >= least);
        shapes.push(Shape {
            geometry: shape.geometry.clone(),
            fill: Some(fill.clone()),
            fill_rule: shape.fill_rule,
            stroke: (!wide).then(|| FixedStroke::from_pair(fill.clone(), least)),
        });
    }
    if let Some(stroke) = shape
        .stroke
        .as_ref()
        .filter(|stroke| !paints_nothing(&stroke.paint))
    {
        shapes.push(Shape {
            geometry: shape.geometry.clone(),
            fill: None,
            fill_rule: shape.fill_rule,
            stroke: Some(widened_stroke(stroke, least)),
        });
    }
    shapes
}

/// `frame` as the check draws it, however deep: every shape [`widened`] so that it is at least `least`
/// wide on the page, and with its text or without. What `ts` does to it is already applied.
fn to_draw(frame: &Frame, ts: Transform, least: Abs, with_text: bool) -> Frame {
    let mut drawn = Frame::new(frame.size(), frame.kind());
    for (pos, item) in frame.items() {
        match item {
            FrameItem::Text(_) if !with_text => {}
            FrameItem::Text(text) if text.stroke.is_some() => {
                let mut text = text.clone();
                let least = least / enlargement(ts);
                text.stroke = text.stroke.map(|stroke| widened_stroke(&stroke, least));
                drawn.push(*pos, FrameItem::Text(text));
            }
            FrameItem::Group(group) => {
                let here = ts
                    .pre_concat(Transform::translate(pos.x, pos.y))
                    .pre_concat(group.transform);
                let mut inner = GroupItem::new(to_draw(&group.frame, here, least, with_text));
                inner.transform = group.transform;
                inner.clip.clone_from(&group.clip);
                drawn.push(*pos, FrameItem::Group(inner));
            }
            FrameItem::Shape(shape, span) => {
                for wide in widened(shape, least / enlargement(ts)) {
                    drawn.push(*pos, FrameItem::Shape(wide, *span));
                }
            }
            other => drawn.push(*pos, other.clone()),
        }
    }
    drawn
}

/// A pixel as a reader sees it: on white paper, in OKLab.
#[derive(Clone, Copy, PartialEq)]
struct Lab([f32; 3]);

fn to_linear(channel: f32) -> f32 {
    if channel <= 0.04045 {
        channel / 12.92
    } else {
        ((channel + 0.055) / 1.055).powf(2.4)
    }
}

impl Lab {
    fn of_srgb([r, g, b]: [f32; 3]) -> Self {
        let (r, g, b) = (to_linear(r), to_linear(g), to_linear(b));
        let l = (0.412_221_47 * r + 0.536_332_54 * g + 0.051_445_99 * b).cbrt();
        let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
        let s = (0.088_302_46 * r + 0.281_718_84 * g + 0.629_978_7 * b).cbrt();
        Self([
            0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
            1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
            0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
        ])
    }
}

/// How far apart a reader finds two colours: plain distance in sRGB rates pale yellow as plainly
/// visible on white, where it can't be read.
fn difference(a: Lab, b: Lab) -> f32 {
    let (dl, da, db) = (a.0[0] - b.0[0], a.0[1] - b.0[1], a.0[2] - b.0[2]);
    (dl * dl + CHROMA_WEIGHT * CHROMA_WEIGHT * (da * da + db * db)).sqrt()
}

/// One drawing of a page, read as sRGB on white paper.
struct Drawing(sk::Pixmap);

impl Drawing {
    fn srgb(&self, index: usize) -> [f32; 3] {
        let pixel = &self.0.data()[index * 4..index * 4 + 4];
        // Premultiplied, so what shows through a transparent pixel is the paper.
        let paper = 255 - u16::from(pixel[3]);
        [pixel[0], pixel[1], pixel[2]].map(|channel| f32::from(u16::from(channel) + paper) / 255.0)
    }
}

/// Builds a glyph's outline as a path, in font units.
struct Outline(sk::PathBuilder);

impl OutlineBuilder for Outline {
    fn move_to(&mut self, x: f32, y: f32) {
        self.0.move_to(x, y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.0.line_to(x, y);
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.0.quad_to(x1, y1, x, y);
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.0.cubic_to(x1, y1, x2, y2, x, y);
    }

    fn close(&mut self) {
        self.0.close();
    }
}

/// Where a glyph's ink should be: how much of each pixel of a box on the page it covers.
struct Coverage {
    x: i64,
    y: i64,
    mask: sk::Mask,
}

/// A page asked to judge more ink than [`MAX_INK_PAGES`].
struct OverBudget;

/// `to_page` takes the run's coordinates, in points, to pixels. `at` is the glyph's place in the run.
fn coverage(
    text: &TextItem,
    glyph: &Glyph,
    to_page: sk::Transform,
    at: (f32, f32),
    size: (u32, u32),
    budget: &mut u64,
) -> Result<Option<Coverage>, OverBudget> {
    let mut outline = Outline(sk::PathBuilder::new());
    let font = text.font.ttf();
    if font
        .outline_glyph(GlyphId(glyph.id), &mut outline)
        .is_none()
    {
        return Ok(None);
    }
    let Some(path) = outline.0.finish() else {
        return Ok(None);
    };
    let scale = (text.size.to_pt() as f32) / f32::from(font.units_per_em());
    let ts = to_page.pre_translate(at.0, at.1).pre_scale(scale, -scale);
    let Some(path) = path.transform(ts) else {
        return Ok(None);
    };
    let bounds = path.bounds();
    let x = (f64::from(bounds.left()).floor() as i64).max(0);
    let y = (f64::from(bounds.top()).floor() as i64).max(0);
    let right = (f64::from(bounds.right()).ceil() as i64).min(i64::from(size.0));
    let bottom = (f64::from(bounds.bottom()).ceil() as i64).min(i64::from(size.1));
    if right <= x || bottom <= y {
        return Ok(None);
    }
    let (width, height) = ((right - x) as u32, (bottom - y) as u32);
    *budget = budget
        .checked_sub(u64::from(width) * u64::from(height))
        .ok_or(OverBudget)?;
    let Some(mut mask) = sk::Mask::new(width, height) else {
        return Ok(None);
    };
    let shift = sk::Transform::from_translate(-(x as f32), -(y as f32));
    mask.fill_path(&path, sk::FillRule::Winding, true, shift);
    Ok(Some(Coverage { x, y, mask }))
}

/// What became of one glyph's ink.
#[derive(Default)]
struct Fate {
    total: f32,
    erased: f32,
    covered: f32,
    overlaid: f32,
    /// How much the arrived ink changes the picture, and how much it should at full coverage.
    change: f32,
    arrived: f32,
}

impl Fate {
    /// The difference from its ground that the glyph's arrived ink makes at full coverage.
    fn contrast(&self) -> Option<f32> {
        (self.arrived > 0.0).then(|| self.change / self.arrived)
    }

    fn unread(&self, is_letter: bool) -> bool {
        let total = self.total;
        if total <= 0.0 {
            return false;
        }
        self.erased > MAX_ERASED_INK * total
            || self.overlaid > MAX_OVERLAID_INK * total
            || (is_letter && self.erased + self.covered > MAX_COVERED_INK * total)
            || self
                .contrast()
                .is_some_and(|contrast| contrast < MIN_DIFFERENCE)
    }
}

/// The colour most of the box around a glyph has, without its text. Where a lost pixel can't be told
/// from it, a reader has nothing to see there: its ink is gone with no trace. A thin frame or a dot
/// beside a glyph doesn't change this, and a paint over most of the box counts as the surroundings,
/// which errs toward refusing.
fn surroundings(bare: &Drawing, cover: &Coverage, size: (u32, u32)) -> Lab {
    let left = (cover.x - SURROUND).max(0);
    let top = (cover.y - SURROUND).max(0);
    let right = (cover.x + i64::from(cover.mask.width()) + SURROUND).min(i64::from(size.0));
    let bottom = (cover.y + i64::from(cover.mask.height()) + SURROUND).min(i64::from(size.1));
    let mut counts: HashMap<[u8; 3], (u32, usize)> = HashMap::new();
    for y in top..bottom {
        for x in left..right {
            let index = (y * i64::from(size.0) + x) as usize;
            // Colours a reader can't tell apart count as one.
            let key = bare
                .srgb(index)
                .map(|channel| (channel * 15.0).round() as u8);
            counts.entry(key).or_insert((0, index)).0 += 1;
        }
    }
    let most = counts.into_values().max_by_key(|(count, _)| *count);
    most.map_or(Lab([1.0, 0.0, 0.0]), |(_, index)| {
        Lab::of_srgb(bare.srgb(index))
    })
}

/// Judges one glyph against the two drawings. `later` marks the ink of runs painted after this one.
fn judge(
    drawn: &Drawing,
    bare: &Drawing,
    cover: &Coverage,
    fill: ([f32; 3], f32),
    later: &[bool],
    size: (u32, u32),
) -> Fate {
    let mut fate = Fate::default();
    let mut lost = Vec::new();
    let width = cover.mask.width() as usize;
    for (offset, covers) in cover.mask.data().iter().enumerate() {
        if *covers == 0 {
            continue;
        }
        let share = f32::from(*covers) / 255.0;
        // The faint edge of a glyph: the renderer and this mask differ most here.
        if share < MIN_JUDGED_COVERAGE {
            continue;
        }
        let x = cover.x as usize + offset % width;
        let y = cover.y as usize + offset / width;
        let index = y * size.0 as usize + x;
        fate.total += share;
        // Where both glyphs truly have ink. Edges touching read as they draw, and are judged below.
        if later[index] && share >= LATER_INK {
            fate.overlaid += share;
            continue;
        }
        let ground_srgb = bare.srgb(index);
        let ground = Lab::of_srgb(ground_srgb);
        let change = difference(Lab::of_srgb(drawn.srgb(index)), ground);
        let opacity = share * fill.1;
        let mix = |channel: usize| {
            ground_srgb[channel] + opacity * (fill.0[channel] - ground_srgb[channel])
        };
        let expected = difference(Lab::of_srgb([mix(0), mix(1), mix(2)]), ground);
        // Text near the limit of what shows keeps a margin for how a pixel's edge happens to fall. Black
        // on a black bar expects no change, and has made none: it hasn't arrived.
        let arrived = change >= share * MIN_DIFFERENCE
            || (expected >= share * MIN_DIFFERENCE * MIN_ARRIVED
                && change >= expected * MIN_ARRIVED);
        if arrived {
            fate.arrived += share;
            fate.change += change;
        } else {
            lost.push((index, share, ground));
        }
    }
    if !lost.is_empty() {
        let around = surroundings(bare, cover, size);
        for (_, share, ground) in lost {
            if difference(ground, around) < MIN_DIFFERENCE {
                fate.erased += share;
            } else {
                fate.covered += share;
            }
        }
    }
    fate
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

/// Every run on a page in the order it is painted, with what takes its coordinates to the page.
fn runs_of(page: &Page) -> Vec<(&TextItem, Transform)> {
    let mut runs = Vec::new();
    let mut stack = vec![(page.frame.items(), Transform::identity())];
    while let Some((items, ts)) = stack.last_mut() {
        let ts = *ts;
        let Some((pos, item)) = items.next() else {
            stack.pop();
            continue;
        };
        let here = ts.pre_concat(Transform::translate(pos.x, pos.y));
        match item {
            FrameItem::Group(group) => {
                stack.push((group.frame.items(), here.pre_concat(group.transform)));
            }
            FrameItem::Text(text) => runs.push((text, here)),
            _ => {}
        }
    }
    runs
}

fn to_sk(ts: Transform, scale: f32) -> sk::Transform {
    sk::Transform::from_row(
        ts.sx.get() as f32,
        ts.ky.get() as f32,
        ts.kx.get() as f32,
        ts.sy.get() as f32,
        ts.tx.to_pt() as f32,
        ts.ty.to_pt() as f32,
    )
    .post_scale(scale, scale)
}

/// The words on `page` with a glyph a reader can't read. Called before the stamp is added, so that the
/// stamp can't refuse a draft. One page's two drawings are dropped before the next page's are made.
pub(crate) fn unread_words(page: &Page, hidden: &mut Vec<String>) {
    let options = render_options(page);
    let scale = options.pixel_per_pt.get() as f32;
    let least = Abs::pt(LEAST_PIXELS / f64::from(scale));
    let mut to_render = page.clone();
    to_render.frame = to_draw(&page.frame, Transform::identity(), least, true);
    let drawn = Drawing(typst_render::render(&to_render, &options));
    to_render.frame = to_draw(&page.frame, Transform::identity(), least, false);
    let bare = Drawing(typst_render::render(&to_render, &options));
    drop(to_render);
    let size = (drawn.0.width(), drawn.0.height());
    let pixels = u64::from(size.0) * u64::from(size.1);
    let mut budget = MAX_INK_PAGES * pixels;
    let mut later = vec![false; pixels as usize];
    let mut unread = Vec::new();
    let mut ink = Vec::new();
    // Backwards, so that all that was painted after a run is known when it is judged. A run's own
    // glyphs may touch: kerning and accents do.
    for (text, ts) in runs_of(page).into_iter().rev() {
        let to_page = to_sk(ts, scale);
        let fill = match &text.fill {
            Paint::Solid(color) => {
                let color = color.to_rgb();
                ([color.red, color.green, color.blue], color.alpha)
            }
            // The text rules refuse such a paint already.
            Paint::Gradient(_) | Paint::Tiling(_) => continue,
        };
        let is_letter = |glyph: &Glyph| {
            text.text
                .get(glyph.range())
                .is_some_and(|said| said.chars().any(char::is_alphanumeric))
        };
        let (mut x, mut y) = (Abs::zero(), Abs::zero());
        let mut last_word = None;
        for glyph in &text.glyphs {
            let at = (
                (x + glyph.x_offset.at(text.size)).to_pt() as f32,
                -((y + glyph.y_offset.at(text.size)).to_pt() as f32),
            );
            x += glyph.x_advance.at(text.size);
            y += glyph.y_advance.at(text.size);
            let cover = match coverage(text, glyph, to_page, at, size, &mut budget) {
                Ok(Some(cover)) => cover,
                Ok(None) => continue,
                // What can't be checked is refused, and the word it stopped at says where.
                Err(OverBudget) => {
                    hidden.push(word_at(text, glyph));
                    return;
                }
            };
            let fate = judge(&drawn, &bare, &cover, fill, &later, size);
            if fate.unread(is_letter(glyph)) {
                let word = word_at(text, glyph);
                if last_word.as_ref() != Some(&word) {
                    unread.push(word.clone());
                    last_word = Some(word);
                }
            }
            let width = cover.mask.width() as usize;
            for (offset, covers) in cover.mask.data().iter().enumerate() {
                if f32::from(*covers) / 255.0 >= LATER_INK {
                    let row = cover.y as usize + offset / width;
                    ink.push(row * size.0 as usize + cover.x as usize + offset % width);
                }
            }
        }
        for index in ink.drain(..) {
            later[index] = true;
        }
    }
    unread.reverse();
    hidden.append(&mut unread);
}
