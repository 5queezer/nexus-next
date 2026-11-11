//! Typst rendering and the integrity check for tailored CVs. No other crate touches Typst.
//!
//! Every compile in `serve` runs in a child process, `nexus __render`, because a compile can use
//! arbitrary memory and time on input an agent wrote.

mod check;
mod compile;
mod extract;
mod ink;
mod template;
mod worker;

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use check::{
    DiffLine, LineChange, Report, ReviewItem, Rule, Violation, check, review_diff, review_lines,
    review_text, unverified_urls,
};
pub use template::{
    CvContent, CvEmployment, CvProject, CvSkills, TEMPLATE_HELP, TEMPLATES, default_source,
    source_from_profile, template_of, with_template,
};
pub use worker::run_worker;

pub const MAX_SOURCE_BYTES: usize = 256 * 1024;
pub const MAX_PAGES: u32 = 10;
pub const MAX_PDF_BYTES: usize = 10 * 1024 * 1024;
/// A PDF at its limit and a typical preview. One result costs several times this in the transport,
/// as a downloaded document does.
pub const MAX_RENDER_RESULT_BYTES: usize = 12 * 1024 * 1024;
/// Far above what ten pages hold, and below what would strain the server, which keeps a copy.
pub const MAX_TEXT_BYTES: usize = 1024 * 1024;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// An honest 10-page CV peaks at 71 MiB on x86-64, with the check's four drawings of each page. Its
/// figure on the board is still to come: before those drawings, 9 pages took 48 MB there. The unit's
/// `MemoryHigh=256M` is the real ceiling, not `MemoryMax`: above it the kernel stalls the whole cgroup,
/// server included, instead of killing anything. So this limit, the server's own peak, and what a
/// compile can allocate between two polls must fit below it. The server's peak measured 128 MiB on
/// x86-64 after four browsers loaded a 10-page approval, also still to be measured on the board.
pub const DEFAULT_RSS_LIMIT_BYTES: u64 = 128 * 1024 * 1024;

/// The locked facts. [`Facts::renderable`] is served as `/facts.json` for the template library.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Facts {
    pub name: String,
    pub contact: Contact,
    pub employment: Vec<Employment>,
    pub education: Vec<Education>,
    pub languages: Vec<Language>,
    pub metrics: Vec<Metric>,
    pub allowed_tokens: Vec<String>,
    pub banned_phrases: Vec<String>,
    pub vocabulary: Vec<String>,
}

impl Facts {
    /// What a CV can render, and all a compile gets. A source could print the rest, which would then
    /// change under an approved revision, and a banned phrase it printed would come back in the
    /// violation that names it.
    #[must_use]
    pub fn renderable(&self) -> Self {
        Self {
            allowed_tokens: Vec::new(),
            banned_phrases: Vec::new(),
            vocabulary: Vec::new(),
            ..self.clone()
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Contact {
    pub email: String,
    pub phone: String,
    pub linkedin: String,
    pub github: String,
    pub location: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Employment {
    pub id: String,
    pub company: String,
    pub title: String,
    pub date: String,
    pub location: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Education {
    pub id: String,
    pub institution: String,
    pub qualification: String,
    pub date: String,
    pub location: String,
    pub details: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Language {
    pub name: String,
    pub level: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Metric {
    pub key: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderRequest {
    pub source: String,
    pub facts: Facts,
    pub pdf: bool,
    pub preview_pages: u32,
    /// Draw `DRAFT, NOT APPROVED` across every page of the PDF and the previews.
    pub stamp: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rendered {
    pub pdf: Option<Vec<u8>>,
    /// PNG, one per requested page.
    pub previews: Vec<Vec<u8>>,
    pub page_count: u32,
    pub text: RenderedText,
    pub warnings: Vec<Diagnostic>,
}

/// Everything the document says, as the laid-out pages and the PDF carry it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderedText {
    /// Rebuilt by baseline, in reading order. Includes tiny, clipped, off-page and hidden text.
    pub lines: Vec<String>,
    /// The runs among those no reader can see.
    pub hidden: Vec<String>,
    /// The runs among those drawn under anything but a translation and a uniform positive scale, or
    /// squeezed until they draw otherwise than they read.
    pub transformed: Vec<String>,
    /// What these name that the fonts have no letter for, which draws as a crossed box.
    #[serde(default)]
    pub missing: Vec<String>,
    /// The words with a small mark drawn on them or right beside them, which can make one read as
    /// another. Nothing refuses these: the owner is told to read them in the picture.
    #[serde(default)]
    pub marked: Vec<String>,
    /// The runs among those a bidi override draws back to front. Typst trims the control character
    /// from a line's ends, while its effect stays.
    pub reversed: Vec<String>,
    /// Every URL link, with the text it covers on the page.
    pub links: Vec<RenderedLink>,
    /// Document title, authors, description, keywords, date, bookmarks and page labels: with `links`,
    /// what the PDF says beyond its pages.
    pub metadata: Vec<String>,
    pub image_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderedLink {
    pub target: String,
    pub text: String,
    /// `text` with the links it stands beside, or those with its target: what they spell together.
    pub together: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Severity {
    Error,
    Warning,
}

/// A compile problem. Data for the agent, not a failure of the server. Lines and columns start at 1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub end_line: Option<u32>,
    pub end_column: Option<u32>,
    pub hints: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RenderError {
    #[error("the source does not compile")]
    Diagnostics(Vec<Diagnostic>),
    #[error("the source is larger than {MAX_SOURCE_BYTES} bytes")]
    SourceTooLarge,
    #[error("the document has more than {MAX_PAGES} pages")]
    TooManyPages,
    #[error("the PDF is larger than {MAX_PDF_BYTES} bytes")]
    PdfTooLarge,
    #[error("the PDF and previews are larger than {MAX_RENDER_RESULT_BYTES} bytes")]
    ResultTooLarge,
    #[error("the compile took too long")]
    Timeout,
    #[error("the compile used too much memory")]
    MemoryExceeded,
    #[error("render worker failed: {0}")]
    Worker(String),
}

#[derive(Debug, Clone)]
pub enum Renderer {
    /// What `serve` uses. `exe` must run [`run_worker`] when given the argument `__render`.
    Subprocess {
        exe: PathBuf,
        rss_limit_bytes: u64,
        timeout: Duration,
    },
    /// For tests in crates that can't name the binary. Has no resource limits.
    InProcess,
}

impl Renderer {
    /// The running binary as the worker, with the default limits. `/proc/self/exe` still runs the
    /// same image after a package upgrade has replaced the file on disk.
    pub fn current_exe() -> Self {
        Self::Subprocess {
            exe: PathBuf::from("/proc/self/exe"),
            rss_limit_bytes: DEFAULT_RSS_LIMIT_BYTES,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub async fn render(&self, req: RenderRequest) -> Result<Rendered, RenderError> {
        if req.source.len() > MAX_SOURCE_BYTES {
            return Err(RenderError::SourceTooLarge);
        }
        match self {
            Self::Subprocess {
                exe,
                rss_limit_bytes,
                timeout,
            } => worker::render_in_child(exe, *rss_limit_bytes, *timeout, &req).await,
            Self::InProcess => tokio::task::spawn_blocking(move || compile::render(&req))
                .await
                .map_err(|e| RenderError::Worker(e.to_string()))?,
        }
    }
}
