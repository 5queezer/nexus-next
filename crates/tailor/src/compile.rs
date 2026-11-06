//! The Typst world, and one compile from request to finished output.

use std::sync::LazyLock;

use typst::diag::{FileError, FileResult, Severity as TypstSeverity, SourceDiagnostic, bail};
use typst::foundations::{Bytes, Datetime, Duration, Target};
use typst::layout::HideElem;
use typst::pdf::AttachElem;
use typst::syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World, WorldExt};
use typst_layout::PagedDocument;
use typst_pdf::PdfOptions;
use typst_render::RenderOptions;

use crate::{Diagnostic, MAX_PAGES, MAX_PDF_BYTES, RenderError, RenderRequest, Rendered, Severity};

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

static FONTS: LazyLock<Vec<Font>> = LazyLock::new(|| {
    FONT_FILES
        .iter()
        .map(|data| Font::new(Bytes::new(*data), 0).expect("embedded font parses"))
        .collect()
});

/// Text and attachments that no rendered page shows would be invisible to the check, so these are
/// compile errors: Typst drops hidden text from the frames altogether.
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
    library
        .rules
        .replace::<AttachElem>(Target::Paged, |elem, _, _| {
            bail!(
                elem.span(),
                "pdf.attach is not allowed: a CV has no attachments"
            )
        });
    LazyHash::new(library)
});

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
    fn new(req: &RenderRequest) -> Result<Self, RenderError> {
        let facts =
            serde_json::to_vec(&req.facts).map_err(|e| RenderError::Worker(e.to_string()))?;
        Ok(Self {
            main: Source::new(project_file("/main.typ"), req.source.clone()),
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
    let mut result = Diagnostic {
        severity: match diag.severity {
            TypstSeverity::Error => Severity::Error,
            TypstSeverity::Warning => Severity::Warning,
        },
        message: diag.message.to_string(),
        file: None,
        line: None,
        column: None,
        end_line: None,
        end_column: None,
        hints: diag.hints.iter().map(|hint| hint.v.to_string()).collect(),
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

pub(crate) fn render(req: &RenderRequest) -> Result<Rendered, RenderError> {
    let world = CvWorld::new(req)?;
    let compiled = typst::compile::<PagedDocument>(&world);
    let warnings: Vec<_> = compiled
        .warnings
        .iter()
        .map(|diag| to_diagnostic(&world, diag))
        .collect();
    let document = compiled.output.map_err(|errors| {
        RenderError::Diagnostics(errors.iter().map(|d| to_diagnostic(&world, d)).collect())
    })?;
    let pages = document.pages();
    let page_count = u32::try_from(pages.len()).unwrap_or(u32::MAX);
    if page_count > MAX_PAGES {
        return Err(RenderError::TooManyPages);
    }
    let text = crate::extract::rendered_text(&document)?;
    let pdf = if req.pdf {
        // Untagged: a tagged PDF carries alt texts no page shows. No timestamp: a revision always
        // renders to the same bytes.
        let options = PdfOptions {
            tagged: false,
            ..PdfOptions::default()
        };
        let bytes = typst_pdf::pdf(&document, &options).map_err(|errors| {
            RenderError::Diagnostics(errors.iter().map(|d| to_diagnostic(&world, d)).collect())
        })?;
        if bytes.len() > MAX_PDF_BYTES {
            return Err(RenderError::PdfTooLarge);
        }
        Some(bytes)
    } else {
        None
    };
    let previews = pages
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
    Ok(Rendered {
        pdf,
        previews,
        page_count,
        text,
        warnings,
    })
}
