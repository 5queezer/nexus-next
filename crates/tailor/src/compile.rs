//! The Typst world, and one compile from request to finished output.

use std::sync::LazyLock;

use typst::diag::{FileError, FileResult, Severity as TypstSeverity, SourceDiagnostic, bail};
use typst::foundations::{Bytes, Datetime, Duration, NativeElement, Target};
use typst::introspection::Introspector;
use typst::layout::{Abs, Frame, FrameItem, GroupItem, HideElem, Point, Ratio, Transform};
use typst::model::Document;
use typst::pdf::AttachElem;
use typst::syntax::{FileId, RootedPath, Source, Span, VirtualPath, VirtualRoot};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World, WorldExt};
use typst_ide::Jump;
use typst_layout::{Page, PagedDocument};
use typst_pdf::PdfOptions;

use crate::{
    Diagnostic, LinePlace, MAX_PAGES, MAX_PDF_BYTES, MAX_RENDER_RESULT_BYTES, RenderError,
    RenderRequest, Rendered, Severity,
};

const NEXUS_TYP: &str = include_str!("nexus.typ");

const FONT_FILES: [&[u8]; 8] = [
    include_bytes!("../fonts/LibertinusSerif-Regular.otf"),
    include_bytes!("../fonts/LibertinusSerif-Bold.otf"),
    include_bytes!("../fonts/LibertinusSerif-Italic.otf"),
    include_bytes!("../fonts/LibertinusSerif-BoldItalic.otf"),
    include_bytes!("../fonts/Inter-Regular.ttf"),
    include_bytes!("../fonts/Inter-Bold.ttf"),
    include_bytes!("../fonts/Inter-Italic.ttf"),
    include_bytes!("../fonts/Inter-BoldItalic.ttf"),
];

const MIN_PAGE_SIDE: f64 = 200.0;
const MAX_PAGE_SIDE: f64 = 1200.0;

/// A source can raise any number of errors of any length, which would cross to the parent and on to
/// the agent. These bounds give the worker a fixed budget for them: see `worker::MAX_OUTPUT_BYTES`.
const MAX_DIAGNOSTICS: usize = 20;
const MAX_HINTS: usize = 5;
const MAX_DIAGNOSTIC_CHARS: usize = 1000;

static FONTS: LazyLock<Vec<Font>> = LazyLock::new(|| {
    FONT_FILES
        .iter()
        .map(|data| Font::new(Bytes::new(*data), 0).expect("embedded font parses"))
        .collect()
});

/// Hidden text is a compile error: Typst drops it from the frames altogether, so the check would
/// never see it. Nothing else marks text hidden, and a user's show rule that takes over `hide` instead
/// of this one can only draw the body, which the check reads, or drop it, from the PDF too.
static LIBRARY: LazyLock<LazyHash<Library>> = LazyLock::new(|| {
    let mut library = Library::builder().build();
    library
        .rules
        .replace::<HideElem>(Target::Paged, |elem, _, _| {
            bail!(
                elem.span(),
                "hide is not allowed: a CV shows everything it says"
            )
        });
    LazyHash::new(library)
});

/// The PDF exporter attaches every element it finds, whatever show rules did to it, and an attachment
/// is text no page shows. So this looks where the exporter looks.
fn refuse_attachments(world: &CvWorld, document: &PagedDocument) -> Result<(), RenderError> {
    let attached = document.introspector().query(&AttachElem::ELEM.select());
    let Some(first) = attached.first() else {
        return Ok(());
    };
    let error = SourceDiagnostic::error(
        first.span(),
        "pdf.attach is not allowed: a CV has no attachments",
    );
    Err(RenderError::Diagnostics(vec![to_diagnostic(world, &error)]))
}

/// A page's sides, in points, as a compile error. A source sets them, and a CV needs neither a sheet
/// over A3's 1191 nor one too small for a stamp a reader can read. The size that counts is the laid
/// out one, since `auto` can run long.
fn refuse_odd_pages(world: &CvWorld, document: &PagedDocument) -> Result<(), RenderError> {
    for (index, page) in document.pages().iter().enumerate() {
        let (width, height) = (page.frame.width().to_pt(), page.frame.height().to_pt());
        if ![width, height]
            .iter()
            .all(|side| (MIN_PAGE_SIDE..=MAX_PAGE_SIDE).contains(side))
        {
            let error = SourceDiagnostic::error(
                Span::detached(),
                format!(
                    "page {} is {width:.0} by {height:.0} pt: a CV page is {MIN_PAGE_SIDE:.0} to \
                     {MAX_PAGE_SIDE:.0} pt on each side",
                    index + 1
                ),
            );
            return Err(RenderError::Diagnostics(vec![to_diagnostic(world, &error)]));
        }
    }
    Ok(())
}

static BOOK: LazyLock<LazyHash<FontBook>> =
    LazyLock::new(|| LazyHash::new(FontBook::from_fonts(FONTS.iter())));

fn project_file(path: &str) -> FileId {
    let path = VirtualPath::new(path).expect("a valid virtual path");
    RootedPath::new(VirtualRoot::Project, path).intern()
}

/// Exactly three virtual files. No filesystem, network, packages or system fonts.
struct CvWorld {
    main: Source,
    nexus: Source,
    facts_id: FileId,
    facts: Bytes,
}

impl CvWorld {
    fn new(source: &str, facts: &crate::Facts) -> Result<Self, RenderError> {
        let facts = serde_json::to_vec(facts).map_err(|e| RenderError::Worker(e.to_string()))?;
        Ok(Self {
            main: Source::new(project_file("/main.typ"), source.to_owned()),
            nexus: Source::new(project_file("/nexus.typ"), NEXUS_TYP.to_owned()),
            facts_id: project_file("/facts.json"),
            facts: Bytes::new(facts),
        })
    }
}

impl World for CvWorld {
    fn library(&self) -> &LazyHash<Library> {
        &LIBRARY
    }

    fn book(&self) -> &LazyHash<FontBook> {
        &BOOK
    }

    fn main(&self) -> FileId {
        self.main.id()
    }

    fn source(&self, id: FileId) -> FileResult<Source> {
        if id == self.main.id() {
            Ok(self.main.clone())
        } else if id == self.nexus.id() {
            Ok(self.nexus.clone())
        } else {
            Err(no_such_file(id))
        }
    }

    fn file(&self, id: FileId) -> FileResult<Bytes> {
        if id == self.facts_id {
            Ok(self.facts.clone())
        } else if id == self.main.id() || id == self.nexus.id() {
            Ok(Bytes::from_string(self.source(id)?))
        } else {
            Err(no_such_file(id))
        }
    }

    fn font(&self, index: usize) -> Option<Font> {
        FONTS.get(index).cloned()
    }

    fn today(&self, _offset: Option<Duration>) -> Option<Datetime> {
        None
    }
}

impl typst_ide::IdeWorld for CvWorld {
    fn upcast(&self) -> &dyn World {
        self
    }
}

/// For each placed word, where `/main.typ` writes it, as the compiler's own jump from a click on the
/// word's start finds it. In UTF-16 units, as a browser counts a textarea's text. None for a word that
/// the facts or the template library wrote.
fn word_sources(
    world: &CvWorld,
    document: &PagedDocument,
    places: &[LinePlace],
) -> Vec<Vec<Option<u32>>> {
    let source_of = |page: &Page, [x, y, width, height]: [f32; 4]| {
        let size = page.frame.size();
        // Inside the first letter's left half, where the jump lands before it.
        let click = Point::new(
            size.x * f64::from(x + width.min(height) / 8.0),
            size.y * f64::from(y + height / 2.0),
        );
        match typst_ide::jump_from_click_in_frame(world, document, &page.frame, click)? {
            Jump::File(id, byte) if id == world.main.id() => {
                u32::try_from(world.main.lines().byte_to_utf16(byte)?).ok()
            }
            _ => None,
        }
    };
    places
        .iter()
        .map(|place| {
            let Some(page) = document.pages().get(place.page as usize) else {
                return Vec::new();
            };
            let words = place.words.iter();
            words.map(|word| source_of(page, *word)).collect()
        })
        .collect()
}

fn no_such_file(id: FileId) -> FileError {
    FileError::Other(Some(
        format!(
            "{} does not exist: only /main.typ, /nexus.typ and /facts.json do",
            id.vpath().get_with_slash()
        )
        .into(),
    ))
}

fn to_diagnostic(world: &CvWorld, diag: &SourceDiagnostic) -> Diagnostic {
    // An error inside the template library is the caller's: point at the call in /main.typ.
    let in_main = |span: typst::syntax::DiagSpan| span.id() == Some(world.main.id());
    let span = if in_main(diag.span) {
        diag.span
    } else {
        diag.trace
            .iter()
            .rev()
            .map(|point| typst::syntax::DiagSpan::from(point.span))
            .find(|span| in_main(*span))
            .unwrap_or(diag.span)
    };
    let bounded = |text: &str| {
        let mut chars = text.chars();
        let mut bounded: String = chars.by_ref().take(MAX_DIAGNOSTIC_CHARS).collect();
        if chars.next().is_some() {
            bounded.push('…');
        }
        bounded
    };
    let mut result = Diagnostic {
        severity: match diag.severity {
            TypstSeverity::Error => Severity::Error,
            TypstSeverity::Warning => Severity::Warning,
        },
        message: bounded(&diag.message),
        file: None,
        line: None,
        column: None,
        end_line: None,
        end_column: None,
        hints: diag
            .hints
            .iter()
            .take(MAX_HINTS)
            .map(|hint| bounded(&hint.v))
            .collect(),
    };
    if let Some(id) = span.id() {
        result.file = Some(id.vpath().get_with_slash().to_owned());
        if let (Some(range), Ok(source)) = (world.range(span), world.source(id)) {
            let position = |byte| source.lines().byte_to_line_column(byte);
            let one_based = |(line, column): (usize, usize)| (line as u32 + 1, column as u32 + 1);
            if let Some((line, column)) = position(range.start).map(one_based) {
                result.line = Some(line);
                result.column = Some(column);
            }
            if let Some((line, column)) = position(range.end).map(one_based) {
                result.end_line = Some(line);
                result.end_column = Some(column);
            }
        }
    }
    result
}

/// One stamp, translucent so that the CV stays readable beneath it. Its own document, so that nothing
/// in a source can reach it: not a show rule, a `set`, or a font it can't name. The outline is opaque
/// and of another colour, because a page filled with the stamp's own red would hide a stamp of one
/// colour, and no fill can match both.
const STAMP_SOURCE: &str = r##"#set page(width: 320pt, height: 180pt, margin: 0pt, fill: none)
#set text(font: "Libertinus Serif", size: 25pt, weight: "bold", fill: rgb(200, 16, 16, 120), stroke: 0.7pt + white)
#align(center + horizon, rotate(-28deg, reflow: true)[DRAFT, NOT APPROVED])
"##;
const STAMP_WIDTH: f64 = 320.0;
const STAMP_HEIGHT: f64 = 180.0;
/// At least this many stamps along each side: one stamp is a diagonal, which leaves two corners bare.
const MIN_STAMPS_PER_SIDE: f64 = 2.0;

/// Tiles the stamp over `page`, as the last thing drawn, so that nothing on the page can cover it.
/// [`refuse_odd_pages`] bounds the page, and with it how many tiles this makes.
fn stamp_page(page: &mut Page, stamp: &Frame) {
    let (width, height) = (page.frame.width().to_pt(), page.frame.height().to_pt());
    let scale = (width / (STAMP_WIDTH * MIN_STAMPS_PER_SIDE))
        .min(height / (STAMP_HEIGHT * MIN_STAMPS_PER_SIDE))
        .min(1.0);
    let columns = (width / (STAMP_WIDTH * scale)).ceil().max(1.0) as u32;
    let rows = (height / (STAMP_HEIGHT * scale)).ceil().max(1.0) as u32;
    let ratio = Ratio::new(scale);
    for row in 0..rows {
        for column in 0..columns {
            let mut group = GroupItem::new(stamp.clone());
            group.transform = Transform::scale(ratio, ratio);
            let at = Point::new(
                Abs::pt(f64::from(column) * STAMP_WIDTH * scale),
                Abs::pt(f64::from(row) * STAMP_HEIGHT * scale),
            );
            page.frame.push(at, FrameItem::Group(group));
        }
    }
}

/// `document` with every page stamped. Call after reading its text: the stamp is not the CV's.
fn stamped(document: &PagedDocument) -> Result<PagedDocument, RenderError> {
    let world = CvWorld::new(STAMP_SOURCE, &crate::Facts::default())?;
    let stamp = typst::compile::<PagedDocument>(&world)
        .output
        .map_err(|_| RenderError::Worker("the stamp does not compile".to_owned()))?;
    let stamp = &stamp
        .pages()
        .first()
        .ok_or_else(|| RenderError::Worker("the stamp has no page".to_owned()))?
        .frame;
    let pages = document.pages().iter().map(|page| {
        let mut page = page.clone();
        stamp_page(&mut page, stamp);
        page
    });
    Ok(PagedDocument::new(pages.collect(), document.info().clone()))
}

pub(crate) fn render(req: &RenderRequest) -> Result<Rendered, RenderError> {
    let world = CvWorld::new(&req.source, &req.facts)?;
    let compiled = typst::compile::<PagedDocument>(&world);
    let warnings: Vec<_> = compiled
        .warnings
        .iter()
        .take(MAX_DIAGNOSTICS)
        .map(|diag| to_diagnostic(&world, diag))
        .collect();
    let bounded_errors = |errors: &[SourceDiagnostic]| {
        let errors = errors.iter().take(MAX_DIAGNOSTICS);
        RenderError::Diagnostics(errors.map(|d| to_diagnostic(&world, d)).collect())
    };
    let document = compiled.output.map_err(|errors| bounded_errors(&errors))?;
    refuse_attachments(&world, &document)?;
    let page_count = u32::try_from(document.pages().len()).unwrap_or(u32::MAX);
    if page_count > MAX_PAGES {
        return Err(RenderError::TooManyPages);
    }
    refuse_odd_pages(&world, &document)?;
    let mut text = crate::extract::rendered_text(&document)?;
    text.layout = crate::layout::layout(&document, &text, crate::template::pages_of(&req.source));
    // Only a picture is clicked on.
    if req.preview_pages > 0 {
        text.sources = word_sources(&world, &document, &text.places);
    }
    let document = if req.stamp {
        stamped(&document)?
    } else {
        document
    };
    let pages = document.pages();
    let pdf = if req.pdf {
        // Untagged: a tagged PDF carries alt texts no page shows. No timestamp: a revision always
        // renders to the same bytes.
        let options = PdfOptions {
            tagged: false,
            ..PdfOptions::default()
        };
        let bytes =
            typst_pdf::pdf(&document, &options).map_err(|errors| bounded_errors(&errors))?;
        if bytes.len() > MAX_PDF_BYTES {
            return Err(RenderError::PdfTooLarge);
        }
        Some(bytes)
    } else {
        None
    };
    let previews: Vec<Vec<u8>> = pages
        .iter()
        .take(req.preview_pages as usize)
        .map(|page| {
            typst_render::render(page, &crate::ink::render_options(page))
                .encode_png()
                .map_err(|e| RenderError::Worker(e.to_string()))
        })
        .collect::<Result<_, _>>()?;
    let total = pdf.as_ref().map_or(0, Vec::len) + previews.iter().map(Vec::len).sum::<usize>();
    if total > MAX_RENDER_RESULT_BYTES {
        return Err(RenderError::ResultTooLarge);
    }
    Ok(Rendered {
        pdf,
        previews,
        page_count,
        text,
        warnings,
    })
}
