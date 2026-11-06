//! The Typst world, and one compile from request to finished output.

use std::sync::LazyLock;

use typst::diag::{FileError, FileResult, Severity as TypstSeverity, SourceDiagnostic, bail};
use typst::foundations::{Bytes, Datetime, Duration, NativeElement, Target};
use typst::introspection::Introspector;
use typst::layout::{Abs, Frame, FrameItem, GroupItem, HideElem, Point, Ratio, Transform};
use typst::model::Document;
use typst::pdf::AttachElem;
use typst::syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World, WorldExt};
use typst_layout::{Page, PagedDocument};
use typst_pdf::PdfOptions;
use typst_render::RenderOptions;

use crate::{
    Diagnostic, MAX_PAGES, MAX_PDF_BYTES, MAX_RENDER_RESULT_BYTES, RenderError, RenderRequest,
    Rendered, Severity,
};

const NEXUS_TYP: &str = include_str!("nexus.typ");

const FONT_FILES: [&[u8]; 4] = [
    include_bytes!("../fonts/LibertinusSerif-Regular.otf"),
    include_bytes!("../fonts/LibertinusSerif-Bold.otf"),
    include_bytes!("../fonts/LibertinusSerif-Italic.otf"),
    include_bytes!("../fonts/LibertinusSerif-BoldItalic.otf"),
];

/// A preview's scale, and the most pixels one may have: the source sets the page size.
const PREVIEW_PIXEL_PER_PT: f64 = 2.0;
const MAX_PREVIEW_PIXELS: f64 = 2_200_000.0;

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
/// in a source can reach it: not a show rule, a `set`, or a font it can't name.
const STAMP_SOURCE: &str = r##"#set page(width: 320pt, height: 180pt, margin: 0pt, fill: none)
#set text(font: "Libertinus Serif", size: 25pt, weight: "bold", fill: rgb(200, 16, 16, 120))
#align(center + horizon, rotate(-28deg, reflow: true)[DRAFT, NOT APPROVED])
"##;
const STAMP_WIDTH: f64 = 320.0;
const STAMP_HEIGHT: f64 = 180.0;
/// A source sets the page's size, so stamps grow with a large page instead of multiplying without end.
const MAX_STAMPS_PER_SIDE: f64 = 8.0;

/// Tiles the stamp over `page`, as the last thing drawn, so that nothing on the page can cover it.
fn stamp_page(page: &mut Page, stamp: &Frame) {
    let (width, height) = (page.frame.width().to_pt(), page.frame.height().to_pt());
    let fit = (width / STAMP_WIDTH).min(height / STAMP_HEIGHT).min(1.0);
    let cap = width.max(height) / (STAMP_WIDTH.max(STAMP_HEIGHT) * MAX_STAMPS_PER_SIDE);
    let scale = fit.max(cap).max(f64::MIN_POSITIVE);
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
    let text = crate::extract::rendered_text(&document)?;
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
            let size = page.frame.size();
            let area = size.x.to_pt().max(1.0) * size.y.to_pt().max(1.0);
            let options = RenderOptions {
                pixel_per_pt: PREVIEW_PIXEL_PER_PT
                    .min((MAX_PREVIEW_PIXELS / area).sqrt())
                    .into(),
                ..RenderOptions::default()
            };
            typst_render::render(page, &options)
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
