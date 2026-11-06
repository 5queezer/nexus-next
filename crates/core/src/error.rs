/// Persistence errors. Domain variants display as old Nexus's `Error.message`, so the MCP layer can
/// surface them unchanged.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not_found")]
    NotFound,
    #[error("conflict")]
    Conflict,
    #[error("canonical_job_url_conflict")]
    CanonicalJobUrlConflict,
    #[error("notes_too_long")]
    NotesTooLong,
    #[error("application_cursor_invalid")]
    ApplicationCursorInvalid,
    #[error("invalid_applications")]
    InvalidApplications,
    #[error("submitted_document_immutable")]
    SubmittedDocumentImmutable,
    #[error("submitted_state_reserved")]
    SubmittedStateReserved,
    #[error("document_already_submitted")]
    DocumentAlreadySubmitted,
    #[error("not a lowercase sha256 hex digest")]
    InvalidBlobAddress,
    #[error("blob {0} does not match its address")]
    BlobCorrupt(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// True for a UNIQUE or PRIMARY KEY violation, e.g. a lost idempotency race.
    pub fn is_unique_violation(&self) -> bool {
        matches!(self, Error::Database(sqlx::Error::Database(e)) if e.is_unique_violation())
    }
}
