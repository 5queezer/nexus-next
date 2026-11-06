//! Loads an old-Nexus export into a fresh data directory.
//!
//! Idempotency: the database is built in a temp file beside the target and renamed over it only
//! after every check passes, so a failed run changes nothing and a repeat run produces the same
//! rows. An existing database is replaced only if it is empty or still matches the fingerprint its
//! own import recorded; anything else means it has live data and needs `force`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::{AssertSqlSafe, ConnectOptions, Connection, SqliteConnection};

use crate::blob::{BlobStore, sha256_hex};
use crate::db::{self, MIGRATOR, create_data_dir, db_path, side_file};
use crate::time::Timestamp;

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    /// The export failed a check. Nothing was changed.
    #[error("{0}")]
    Invalid(String),
    /// The target database holds data an import did not write.
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error(transparent)]
    Core(#[from] crate::Error),
}

macro_rules! invalid {
    ($($arg:tt)*) => { ImportError::Invalid(format!($($arg)*)) };
}

type Result<T> = std::result::Result<T, ImportError>;

#[derive(Debug, Clone)]
pub struct ImportOptions {
    pub export_dir: PathBuf,
    pub data_dir: PathBuf,
    /// A file of document IDs, one per line (`#` starts a comment), whose recovered candidates are
    /// confirmed. Without it, no candidate is imported.
    pub approve_recovered: Option<PathBuf>,
    /// Replace a database that holds rows no import wrote.
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableCount {
    pub table: &'static str,
    pub imported: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImportSummary {
    pub tables: Vec<TableCount>,
    pub documents_with_blob: usize,
    pub documents_missing_blob: usize,
    pub recovered_documents: usize,
    pub hashes_computed: usize,
    /// Documents where `MISSING_DOCUMENTS.json` contradicts `blob-index.json`, which wins.
    pub missing_list_disagreements: usize,
    pub blob_files: usize,
    pub orphan_blobs_removed: usize,
}

impl fmt::Display for ImportSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for t in &self.tables {
            writeln!(
                f,
                "{:<26} {:>6} imported, {} skipped",
                t.table, t.imported, t.skipped
            )?;
        }
        writeln!(
            f,
            "documents with blob         {:>5}",
            self.documents_with_blob
        )?;
        writeln!(
            f,
            "  of which recovered        {:>5}",
            self.recovered_documents
        )?;
        writeln!(
            f,
            "documents missing blob      {:>5}",
            self.documents_missing_blob
        )?;
        writeln!(f, "content hashes computed     {:>5}", self.hashes_computed)?;
        writeln!(
            f,
            "missing list disagreements  {:>5}",
            self.missing_list_disagreements
        )?;
        writeln!(f, "distinct blob files         {:>5}", self.blob_files)?;
        write!(
            f,
            "orphan blobs removed        {:>5}",
            self.orphan_blobs_removed
        )
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Int,
    Text,
    Bool,
    Ts,
    Json,
}
use Kind::{Bool, Int, Json, Text, Ts};

struct Spec {
    file: &'static str,
    table: &'static str,
    /// Export columns the new schema deliberately lacks.
    dropped: &'static [&'static str],
    columns: &'static [(&'static str, Kind)],
}

const APPLICATIONS: Spec = Spec {
    file: "Application",
    table: "applications",
    dropped: &["userId", "isDemo", "demoWorkspaceId", "demoKey"],
    columns: &[
        ("id", Int),
        ("company", Text),
        ("role", Text),
        ("status", Text),
        ("appliedAt", Ts),
        ("lastContact", Ts),
        ("notes", Text),
        ("followUpAt", Ts),
        ("jobDescription", Text),
        ("source", Text),
        ("remote", Bool),
        ("salaryMin", Int),
        ("salaryMax", Int),
        ("rating", Int),
        ("jobUrl", Text),
        ("canonicalJobUrl", Text),
        ("resumeId", Text),
        ("companySize", Text),
        ("salaryBandMentioned", Bool),
        ("triageQuality", Int),
        ("triageReason", Text),
        ("incomingSource", Text),
        ("autoRejected", Bool),
        ("autoRejectReason", Text),
        ("archivedAt", Ts),
        ("workMode", Text),
        ("eligibleCountries", Json),
        ("primaryLocations", Json),
        ("officeDaysMin", Int),
        ("travelPercent", Int),
        ("visaSponsorship", Bool),
        ("rightToWorkRequired", Bool),
        ("timezoneOverlap", Text),
        ("salaryCurrency", Text),
        ("salaryPeriod", Text),
        ("salaryType", Text),
        ("atsName", Text),
        ("requisitionId", Text),
        ("jobCapturedAt", Ts),
        ("jobVerifiedAt", Ts),
        ("jobPostedAt", Ts),
        ("jobClosedAt", Ts),
        ("jobContentHash", Text),
        ("jobLiveness", Text),
        ("jobSummary", Text),
        ("currentStage", Text),
        ("nextAction", Text),
        ("eventVersion", Int),
        ("createdAt", Ts),
        ("updatedAt", Ts),
    ],
};

const CONTACTS: Spec = Spec {
    file: "Contact",
    table: "contacts",
    dropped: &[],
    columns: &[
        ("id", Int),
        ("name", Text),
        ("email", Text),
        ("phone", Text),
        ("role", Text),
        ("linkedIn", Text),
        ("applicationId", Int),
        ("createdAt", Ts),
    ],
};

const SUBMISSIONS: Spec = Spec {
    file: "ApplicationSubmission",
    table: "application_submissions",
    dropped: &["userId"],
    columns: &[
        ("id", Int),
        ("applicationId", Int),
        ("idempotencyKey", Text),
        ("requestHash", Text),
        ("submittedAt", Ts),
        ("applicationUrl", Text),
        ("atsName", Text),
        ("requisitionId", Text),
        ("language", Text),
        ("answers", Json),
        ("policy", Json),
        ("candidateSalaryMin", Int),
        ("candidateSalaryMax", Int),
        ("candidateSalaryCurrency", Text),
        ("candidateSalaryPeriod", Text),
        ("candidateSalaryType", Text),
        ("candidateSalaryFlexible", Bool),
        ("documentIds", Json),
        ("createdAt", Ts),
    ],
};

const DOCUMENTS: Spec = Spec {
    file: "Document",
    table: "documents",
    dropped: &["userId", "demoProvenance"],
    columns: &[
        ("id", Int),
        ("filename", Text),
        ("originalName", Text),
        ("size", Int),
        ("mimeType", Text),
        ("documentType", Text),
        ("state", Text),
        ("version", Int),
        ("contentHash", Text),
        ("source", Text),
        ("generatedAt", Ts),
        ("submittedAt", Ts),
        ("submissionId", Int),
        ("uploadedAt", Ts),
    ],
};

const LINKS: Spec = Spec {
    file: "ApplicationToDocument",
    table: "application_documents",
    dropped: &[],
    columns: &[("applicationId", Int), ("documentId", Int)],
};

const EVENTS: Spec = Spec {
    file: "ApplicationEvent",
    table: "application_events",
    dropped: &["userId", "isDemo", "demoWorkspaceId", "demoKey"],
    columns: &[
        ("id", Int),
        ("applicationId", Int),
        ("type", Text),
        ("idempotencyKey", Text),
        ("requestHash", Text),
        ("occurredAt", Ts),
        ("source", Text),
        ("actor", Text),
        ("contactId", Text),
        ("outcome", Text),
        ("metadata", Json),
        ("createdAt", Ts),
    ],
};

const CV_PROFILE: Spec = Spec {
    file: "CvProfile",
    table: "cv_profile",
    dropped: &["userId"],
    columns: &[
        ("id", Int),
        ("name", Text),
        ("contact", Json),
        ("profile", Text),
        ("skills", Json),
        ("experience", Json),
        ("projects", Json),
        ("education", Json),
        ("createdAt", Ts),
        ("updatedAt", Ts),
    ],
};

const CV_PATCHES: Spec = Spec {
    file: "CvPatch",
    table: "cv_patches",
    dropped: &[],
    columns: &[
        ("id", Int),
        ("applicationId", Int),
        ("profileOverride", Text),
        ("experienceIds", Json),
        ("skillCategories", Json),
        ("includeProjects", Bool),
        ("includeEducation", Bool),
        ("documentId", Int),
        ("createdAt", Ts),
        ("updatedAt", Ts),
    ],
};

fn snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for c in name.chars() {
        if c.is_ascii_uppercase() {
            out.push('_');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

#[derive(Deserialize)]
struct Manifest {
    missing_documents: usize,
    files: HashMap<String, String>,
}

/// An export directory with its manifest. Every file is read through here, so it is hash-checked.
struct Export {
    dir: PathBuf,
    manifest: Manifest,
    manifest_sha256: String,
}

impl Export {
    fn open(dir: &Path) -> Result<Self> {
        let bytes = fs::read(dir.join("MANIFEST.json"))
            .map_err(|e| invalid!("cannot read MANIFEST.json: {e}"))?;
        let manifest = serde_json::from_slice(&bytes)
            .map_err(|e| invalid!("MANIFEST.json is malformed: {e}"))?;
        Ok(Self {
            dir: dir.to_owned(),
            manifest,
            manifest_sha256: sha256_hex(&bytes),
        })
    }

    fn read(&self, relative: &str) -> Result<Vec<u8>> {
        let expected = self
            .manifest
            .files
            .get(relative)
            .ok_or_else(|| invalid!("{relative} is not listed in MANIFEST.json"))?;
        let bytes = fs::read(self.dir.join(relative))
            .map_err(|e| invalid!("cannot read {relative}: {e}"))?;
        if sha256_hex(&bytes) != *expected {
            return Err(invalid!("{relative} does not match its MANIFEST.json hash"));
        }
        Ok(bytes)
    }

    fn read_json<T: for<'de> Deserialize<'de>>(&self, relative: &str) -> Result<T> {
        serde_json::from_slice(&self.read(relative)?)
            .map_err(|e| invalid!("{relative} is malformed: {e}"))
    }

    /// Parses `jsonl/<file>.jsonl` into rows with their 1-based line numbers.
    fn rows(&self, file: &str) -> Result<Vec<Row>> {
        let relative = format!("jsonl/{file}.jsonl");
        let bytes = self.read(&relative)?;
        let text = String::from_utf8(bytes).map_err(|_| invalid!("{relative} is not UTF-8"))?;
        let mut rows = Vec::new();
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                return Err(invalid!("{relative} line {} is blank", index + 1));
            }
            let fields = serde_json::from_str(line)
                .map_err(|e| invalid!("{relative} line {}: {e}", index + 1))?;
            rows.push(Row {
                line: index + 1,
                fields,
            });
        }
        Ok(rows)
    }
}

struct Row {
    line: usize,
    fields: Map<String, Value>,
}

impl Row {
    fn string(&self, key: &str) -> Option<&str> {
        self.fields.get(key).and_then(Value::as_str)
    }

    fn int(&self, key: &str) -> Option<i64> {
        self.fields.get(key).and_then(Value::as_i64)
    }

    fn flag(&self, key: &str) -> bool {
        self.fields
            .get(key)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }
}

/// Requires exactly the known export columns, so a changed export schema fails instead of silently
/// losing a column.
fn check_columns(spec: &Spec, rows: &[Row]) -> Result<()> {
    for row in rows {
        for key in row.fields.keys() {
            let known = spec.dropped.contains(&key.as_str())
                || spec.columns.iter().any(|(name, _)| name == key);
            if !known {
                return Err(invalid!(
                    "{} line {}: unknown column {key}",
                    spec.file,
                    row.line
                ));
            }
        }
        for (name, _) in spec.columns {
            if !row.fields.contains_key(*name) {
                return Err(invalid!(
                    "{} line {}: missing column {name}",
                    spec.file,
                    row.line
                ));
            }
        }
    }
    Ok(())
}

async fn insert_rows(conn: &mut SqliteConnection, spec: &Spec, rows: &[Row]) -> Result<()> {
    let columns: Vec<String> = spec
        .columns
        .iter()
        .map(|(name, _)| snake_case(name))
        .collect();
    let binds = vec!["?"; columns.len()].join(", ");
    let sql = format!(
        "INSERT INTO {} ({}) VALUES ({binds})",
        spec.table,
        columns.join(", ")
    );
    for row in rows {
        let context = |name: &str, expected: &str| {
            invalid!("{} line {}: {name} is not {expected}", spec.file, row.line)
        };
        let mut query = sqlx::query(AssertSqlSafe(sql.as_str()));
        for (name, kind) in spec.columns {
            let value = &row.fields[*name];
            query = match (kind, value) {
                (Int, Value::Null) => query.bind(None::<i64>),
                (Bool, Value::Null) => query.bind(None::<bool>),
                (Text | Ts | Json, Value::Null) => query.bind(None::<String>),
                (Int, v) => query.bind(v.as_i64().ok_or_else(|| context(name, "an integer"))?),
                (Bool, v) => query.bind(v.as_bool().ok_or_else(|| context(name, "a boolean"))?),
                (Text, v) => {
                    query.bind(v.as_str().ok_or_else(|| context(name, "text"))?.to_owned())
                }
                (Ts, v) => {
                    let text = v.as_str().ok_or_else(|| context(name, "text"))?;
                    let ts = Timestamp::parse_naive_utc(text)
                        .map_err(|_| context(name, "a UTC timestamp without zone"))?;
                    query.bind(ts)
                }
                (Json, v) => query.bind(v.to_string()),
            };
        }
        query
            .execute(&mut *conn)
            .await
            .map_err(|e| invalid!("{} line {}: {e}", spec.file, row.line))?;
    }
    Ok(())
}

/// Where a document's file comes from, once verified.
struct BlobPlan {
    document_id: i64,
    sha256: String,
    source: &'static str,
    computed_hash: bool,
    /// Path of the verified file, relative to the export directory.
    relative: String,
}

fn check_blob(
    document: &Row,
    id: i64,
    sha256: &str,
    bytes: &[u8],
    relative: String,
    source: &'static str,
) -> Result<BlobPlan> {
    if document.int("size") != Some(bytes.len() as i64) {
        return Err(invalid!("document {id}: size differs from its file"));
    }
    let stored = document.string("contentHash");
    if stored.is_some_and(|hash| !hash.eq_ignore_ascii_case(sha256)) {
        return Err(invalid!(
            "document {id}: stored contentHash differs from its file"
        ));
    }
    Ok(BlobPlan {
        document_id: id,
        sha256: sha256.to_owned(),
        source,
        computed_hash: stored.is_none(),
        relative,
    })
}

#[derive(Deserialize)]
struct BlobEntry {
    sha256: String,
    size: i64,
}

#[derive(Deserialize)]
struct MissingList {
    documents: Vec<MissingEntry>,
}

#[derive(Deserialize)]
struct MissingEntry {
    id: i64,
}

#[derive(Deserialize)]
struct Matches {
    documents: Vec<MatchEntry>,
}

#[derive(Deserialize)]
struct MatchEntry {
    id: i64,
    candidates: Vec<String>,
    name_matched: Vec<String>,
}

/// The unique name-matched candidate, else the single size match. Anything else is ambiguous.
fn pick_candidate(entry: &MatchEntry) -> Result<&str> {
    match (entry.name_matched.as_slice(), entry.candidates.as_slice()) {
        ([one], candidates) if candidates.contains(one) => Ok(one),
        ([], [one]) => Ok(one),
        ([], []) => Err(invalid!(
            "approved document {} has no recovered candidate",
            entry.id
        )),
        _ => Err(invalid!(
            "approved document {} has an ambiguous recovered candidate",
            entry.id
        )),
    }
}

fn read_approvals(path: &Path) -> Result<BTreeSet<i64>> {
    let text = fs::read_to_string(path)
        .map_err(|e| invalid!("cannot read approval file {}: {e}", path.display()))?;
    let mut ids = BTreeSet::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if !line.is_empty() {
            let id = line
                .parse()
                .map_err(|_| invalid!("approval file line {}: not a document id", index + 1))?;
            ids.insert(id);
        }
    }
    Ok(ids)
}

/// Verifies every document's file and decides its blob state: `blob-index.json` is the source of
/// truth, so a filename absent from it means the file is missing. Returns the plans for documents
/// that have a file, and how many documents `MISSING_DOCUMENTS.json` contradicts.
fn plan_blobs(
    export: &Export,
    documents: &[Row],
    all_document_ids: &HashSet<i64>,
    approvals: &BTreeSet<i64>,
) -> Result<(Vec<BlobPlan>, usize)> {
    let index: HashMap<String, BlobEntry> = export.read_json("blob-index.json")?;
    let missing: MissingList = export.read_json("MISSING_DOCUMENTS.json")?;
    let missing: HashSet<i64> = missing.documents.iter().map(|entry| entry.id).collect();
    if missing.len() != export.manifest.missing_documents {
        return Err(invalid!(
            "MANIFEST.json counts {} missing documents, MISSING_DOCUMENTS.json lists {}",
            export.manifest.missing_documents,
            missing.len()
        ));
    }
    if let Some(id) = missing.iter().find(|id| !all_document_ids.contains(id)) {
        return Err(invalid!(
            "MISSING_DOCUMENTS.json lists unknown document {id}"
        ));
    }

    let mut plans = Vec::new();
    let mut awaiting_recovery = HashMap::new();
    let mut disagreements = 0;
    for document in documents {
        let id = document
            .int("id")
            .ok_or_else(|| invalid!("Document without integer id"))?;
        let entry = document.string("filename").and_then(|name| index.get(name));
        if entry.is_some() == missing.contains(&id) {
            disagreements += 1;
        }
        match entry {
            Some(entry) => {
                let relative = format!("blobs/{}", entry.sha256);
                let bytes = export.read(&relative)?;
                if sha256_hex(&bytes) != entry.sha256 {
                    return Err(invalid!("blob-index.json hash for document {id} is wrong"));
                }
                if entry.size != bytes.len() as i64 {
                    return Err(invalid!("blob-index.json size for document {id} is wrong"));
                }
                plans.push(check_blob(
                    document,
                    id,
                    &entry.sha256,
                    &bytes,
                    relative,
                    "gcs",
                )?);
            }
            None => {
                awaiting_recovery.insert(id, document);
            }
        }
    }

    if !approvals.is_empty() {
        let matches: Matches = export.read_json("recovered-candidates/MATCHES.json")?;
        for id in approvals {
            let document = awaiting_recovery
                .get(id)
                .ok_or_else(|| invalid!("approved document {id} is not awaiting recovery"))?;
            let entry = matches
                .documents
                .iter()
                .find(|entry| entry.id == *id)
                .ok_or_else(|| invalid!("approved document {id} is not in MATCHES.json"))?;
            let sha256 = pick_candidate(entry)?;
            let relative = format!("recovered-candidates/{sha256}");
            let bytes = export.read(&relative)?;
            if sha256_hex(&bytes) != sha256 {
                return Err(invalid!(
                    "recovered candidate for document {id} is misnamed"
                ));
            }
            plans.push(check_blob(
                document,
                *id,
                sha256,
                &bytes,
                relative,
                "recovered-hermes",
            )?);
        }
    }
    Ok((plans, disagreements))
}

/// Hash of every data table's rows, so re-imports can be compared and later writes detected.
pub async fn fingerprint(conn: &mut SqliteConnection) -> std::result::Result<String, sqlx::Error> {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table'
           AND name NOT IN ('_sqlx_migrations', 'import_meta') AND name NOT LIKE 'sqlite_stat%'
         ORDER BY name",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut hasher = Sha256::new();
    for table in tables {
        let row_expr: String = sqlx::query_scalar(
            "SELECT group_concat('quote(\"' || name || '\")', ' || char(31) || ')
             FROM pragma_table_info(?)",
        )
        .bind(&table)
        .fetch_one(&mut *conn)
        .await?;
        let rows: Vec<String> = sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT {row_expr} FROM \"{table}\" ORDER BY 1"
        )))
        .fetch_all(&mut *conn)
        .await?;
        hasher.update(table.as_bytes());
        for row in rows {
            hasher.update([30]);
            hasher.update(row.as_bytes());
        }
        hasher.update([29]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// True if an existing database is empty or still exactly what its import wrote.
async fn is_replaceable(path: &Path) -> std::result::Result<bool, sqlx::Error> {
    let mut conn = SqliteConnectOptions::new()
        .filename(path)
        .foreign_keys(false)
        .connect()
        .await?;
    let current = fingerprint(&mut conn).await?;
    let has_meta: bool = sqlx::query_scalar(
        "SELECT count(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'import_meta'",
    )
    .fetch_one(&mut conn)
    .await?;
    let recorded: Option<String> = if has_meta {
        sqlx::query_scalar("SELECT fingerprint FROM import_meta")
            .fetch_optional(&mut conn)
            .await?
    } else {
        None
    };
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name != '_sqlx_migrations'",
    )
    .fetch_all(&mut conn)
    .await?;
    let mut rows = 0_i64;
    for table in tables {
        rows += sqlx::query_scalar::<_, i64>(AssertSqlSafe(format!(
            "SELECT count(*) FROM \"{table}\""
        )))
        .fetch_one(&mut conn)
        .await?;
    }
    conn.close().await?;
    Ok(rows == 0 || recorded.as_deref() == Some(current.as_str()))
}

fn remove_if_present(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

async fn guard_target(target: &Path, force: bool) -> Result<()> {
    if !target.try_exists()? {
        return Ok(());
    }
    if !force && !is_replaceable(target).await.unwrap_or(false) {
        return Err(ImportError::Refused(format!(
            "{} holds data no import wrote (or cannot be read); pass --force to replace it",
            target.display()
        )));
    }
    // `--force` overrides the check on what the database holds, never this one: deleting a live WAL
    // and renaming over the database would lose whatever the server writes next.
    match db::in_use_reason(target).await? {
        Some(reason) => Err(ImportError::Refused(reason)),
        None => Ok(()),
    }
}

/// The highest ID a table has issued: its largest row ID or its Postgres sequence, whichever is
/// higher, so the ID of a deleted row is never reused. Tables without an ID have none.
fn issued_through(
    spec: &Spec,
    rows: &[Row],
    sequences: &HashMap<String, i64>,
) -> Result<Option<i64>> {
    if !spec.columns.iter().any(|(name, _)| *name == "id") {
        return Ok(None);
    }
    let sequence = sequences
        .get(spec.file)
        .ok_or_else(|| invalid!("sequences.json has no value for {}", spec.file))?;
    let max_id = rows
        .iter()
        .filter_map(|row| row.int("id"))
        .max()
        .unwrap_or(0);
    Ok(Some(max_id.max(*sequence)))
}

struct Plan<'a> {
    tables: [(&'a Spec, Vec<Row>); 8],
    blobs: &'a [BlobPlan],
    manifest_sha256: &'a str,
    sequences: &'a HashMap<String, i64>,
    owner_id: &'a str,
    owner_email: Option<String>,
}

/// Writes the temp database. Foreign keys stay off while loading so that a bad reference reaches
/// `foreign_key_check`, which can name the row, instead of failing with a bare constraint error.
async fn build_database(path: &Path, plan: &Plan<'_>) -> Result<()> {
    let mut conn = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Delete)
        .foreign_keys(false)
        .connect()
        .await?;
    MIGRATOR.run(&mut conn).await?;

    let mut tx = conn.begin().await?;
    for (spec, rows) in &plan.tables {
        insert_rows(&mut tx, spec, rows).await?;
    }
    for blob in plan.blobs {
        sqlx::query(
            "UPDATE documents SET blob_state = 'present', blob_source = ?, blob_sha256 = ?,
                 content_hash = COALESCE(content_hash, ?), content_hash_computed = ?
             WHERE id = ?",
        )
        .bind(blob.source)
        .bind(&blob.sha256)
        .bind(&blob.sha256)
        .bind(blob.computed_hash)
        .bind(blob.document_id)
        .execute(&mut *tx)
        .await?;
    }
    for (spec, rows) in &plan.tables {
        let Some(issued) = issued_through(spec, rows, plan.sequences)? else {
            continue;
        };
        sqlx::query("DELETE FROM sqlite_sequence WHERE name = ?")
            .bind(spec.table)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO sqlite_sequence (name, seq) VALUES (?, ?)")
            .bind(spec.table)
            .bind(issued)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;

    for (spec, rows) in &plan.tables {
        let count: i64 = sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT count(*) FROM {}",
            spec.table
        )))
        .fetch_one(&mut conn)
        .await?;
        if count != rows.len() as i64 {
            return Err(invalid!(
                "{}: {count} rows in the database, {} expected from the export",
                spec.table,
                rows.len()
            ));
        }
    }
    let violations: Vec<(String, Option<i64>, String, i64)> =
        sqlx::query_as("PRAGMA foreign_key_check")
            .fetch_all(&mut conn)
            .await?;
    if let Some((table, rowid, parent, _)) = violations.first() {
        return Err(invalid!(
            "{} foreign key violations; first: {table} row {} references missing {parent}",
            violations.len(),
            rowid.unwrap_or_default()
        ));
    }
    let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(&mut conn)
        .await?;
    if integrity != "ok" {
        return Err(invalid!("integrity_check failed: {integrity}"));
    }
    for (spec, rows) in &plan.tables {
        let Some(issued) = issued_through(spec, rows, plan.sequences)? else {
            continue;
        };
        let sequence: Option<i64> =
            sqlx::query_scalar("SELECT seq FROM sqlite_sequence WHERE name = ?")
                .bind(spec.table)
                .fetch_optional(&mut conn)
                .await?;
        if sequence != Some(issued) {
            return Err(invalid!("{}: ID sequence is not {issued}", spec.table));
        }
    }

    let fingerprint = fingerprint(&mut conn).await?;
    sqlx::query(
        "INSERT INTO import_meta (id, manifest_sha256, fingerprint, owner_id, owner_email)
         VALUES (1, ?, ?, ?, ?)",
    )
    .bind(plan.manifest_sha256)
    .bind(fingerprint)
    .bind(plan.owner_id)
    .bind(&plan.owner_email)
    .execute(&mut conn)
    .await?;
    conn.close().await?;
    Ok(())
}

/// The owner recorded by the import, as `(owner_id, owner_email)`. A database that no import wrote
/// has none. An export with no MCP events has no email.
pub async fn imported_owner(
    conn: &mut SqliteConnection,
) -> Result<Option<(String, Option<String>)>> {
    Ok(
        sqlx::query_as("SELECT owner_id, owner_email FROM import_meta WHERE owner_id IS NOT NULL")
            .fetch_optional(conn)
            .await?,
    )
}

/// The export has no user table, so the owner's email is the most common `actor` on MCP events.
fn most_common_mcp_actor(events: &[Row]) -> Option<String> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for event in events
        .iter()
        .filter(|event| event.string("source") == Some("mcp"))
    {
        if let Some(actor) = event.string("actor").filter(|actor| !actor.is_empty()) {
            *counts.entry(actor).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(actor, _)| actor.to_owned())
}

/// Splits rows into kept and skipped, and returns the skipped rows' IDs.
fn partition(rows: Vec<Row>, skip: impl Fn(&Row) -> bool) -> (Vec<Row>, HashSet<i64>) {
    let (skipped, kept): (Vec<Row>, Vec<Row>) = rows.into_iter().partition(|row| skip(row));
    (
        kept,
        skipped.iter().filter_map(|row| row.int("id")).collect(),
    )
}

pub async fn run(options: &ImportOptions) -> Result<ImportSummary> {
    let export = Export::open(&options.export_dir)?;
    let counts: HashMap<String, usize> = export.read_json("row-counts.json")?;
    let sequences: HashMap<String, i64> = export.read_json("sequences.json")?;
    let approvals = match &options.approve_recovered {
        Some(path) => read_approvals(path)?,
        None => BTreeSet::new(),
    };
    create_data_dir(&options.data_dir)?;
    let target = db_path(&options.data_dir);
    guard_target(&target, options.force).await?;

    let applications = export.rows(APPLICATIONS.file)?;
    let contacts = export.rows(CONTACTS.file)?;
    let submissions = export.rows(SUBMISSIONS.file)?;
    let documents = export.rows(DOCUMENTS.file)?;
    let events = export.rows(EVENTS.file)?;
    let cv_profiles = export.rows(CV_PROFILE.file)?;
    let cv_patches = export.rows(CV_PATCHES.file)?;
    let mut links = export.rows(LINKS.file)?;
    for link in &mut links {
        // Prisma names an implicit many-to-many's columns A and B, in model-name order.
        for (from, to) in [("A", "applicationId"), ("B", "documentId")] {
            if let Some(value) = link.fields.remove(from) {
                link.fields.insert(to.to_owned(), value);
            }
        }
    }
    let scanned_emails = export.rows("ScannedEmail")?;
    let loaded = [
        (APPLICATIONS.file, applications.len()),
        (CONTACTS.file, contacts.len()),
        (SUBMISSIONS.file, submissions.len()),
        (DOCUMENTS.file, documents.len()),
        (LINKS.file, links.len()),
        (EVENTS.file, events.len()),
        (CV_PROFILE.file, cv_profiles.len()),
        (CV_PATCHES.file, cv_patches.len()),
        ("ScannedEmail", scanned_emails.len()),
    ];
    for (file, rows) in loaded {
        let expected = counts
            .get(file)
            .ok_or_else(|| invalid!("row-counts.json has no count for {file}"))?;
        if *expected != rows {
            return Err(invalid!(
                "{file}: {rows} rows in its jsonl file, row-counts.json says {expected}"
            ));
        }
    }
    if !scanned_emails.is_empty() {
        return Err(invalid!(
            "ScannedEmail has {} rows; the spec drops this table because it was empty",
            scanned_emails.len()
        ));
    }
    let specs = [
        (&APPLICATIONS, &applications),
        (&CONTACTS, &contacts),
        (&SUBMISSIONS, &submissions),
        (&DOCUMENTS, &documents),
        (&LINKS, &links),
        (&EVENTS, &events),
        (&CV_PROFILE, &cv_profiles),
        (&CV_PATCHES, &cv_patches),
    ];
    for (spec, rows) in specs {
        check_columns(spec, rows)?;
    }

    let owners: BTreeSet<&str> = applications
        .iter()
        .filter_map(|row| row.string("userId"))
        .collect();
    let owner = match owners.len() {
        1 => owners.first().copied().unwrap_or_default().to_owned(),
        0 => return Err(invalid!("no Application rows, so the owner is unknown")),
        n => {
            return Err(invalid!(
                "Application rows have {n} different userId values"
            ));
        }
    };
    let foreign = |row: &Row| row.string("userId") != Some(owner.as_str());

    let totals: Vec<usize> = specs.iter().map(|(_, rows)| rows.len()).collect();
    let all_document_ids: HashSet<i64> = documents.iter().filter_map(|row| row.int("id")).collect();

    let (applications, skipped_apps) = partition(applications, |row| row.flag("isDemo"));
    let of_skipped_app = |row: &Row| {
        row.int("applicationId")
            .is_some_and(|id| skipped_apps.contains(&id))
    };
    let (contacts, _) = partition(contacts, of_skipped_app);
    let (submissions, _) = partition(submissions, |row| foreign(row) || of_skipped_app(row));
    let (documents, skipped_docs) =
        partition(documents, |row| foreign(row) || row.flag("demoProvenance"));
    let (links, _) = partition(links, |row| {
        of_skipped_app(row)
            || row
                .int("documentId")
                .is_some_and(|id| skipped_docs.contains(&id))
    });
    let (events, _) = partition(events, |row| {
        foreign(row) || row.flag("isDemo") || of_skipped_app(row)
    });
    let (cv_profiles, _) = partition(cv_profiles, foreign);
    let (cv_patches, _) = partition(cv_patches, of_skipped_app);

    let (blobs, missing_list_disagreements) =
        plan_blobs(&export, &documents, &all_document_ids, &approvals)?;
    let document_count = documents.len();

    let owner_email = most_common_mcp_actor(&events);
    let tables = [
        (&APPLICATIONS, applications),
        (&CONTACTS, contacts),
        (&SUBMISSIONS, submissions),
        (&DOCUMENTS, documents),
        (&LINKS, links),
        (&EVENTS, events),
        (&CV_PROFILE, cv_profiles),
        (&CV_PATCHES, cv_patches),
    ];
    let mut summary = ImportSummary {
        tables: tables
            .iter()
            .zip(&totals)
            .map(|((spec, rows), total)| TableCount {
                table: spec.table,
                imported: rows.len(),
                skipped: total - rows.len(),
            })
            .collect(),
        ..ImportSummary::default()
    };
    summary.documents_with_blob = blobs.len();
    summary.documents_missing_blob = document_count - blobs.len();
    summary.recovered_documents = blobs.iter().filter(|b| b.source != "gcs").count();
    summary.hashes_computed = blobs.iter().filter(|b| b.computed_hash).count();
    summary.missing_list_disagreements = missing_list_disagreements;

    let temp = side_file(&target, ".importing");
    remove_if_present(&temp)?;
    remove_if_present(&side_file(&temp, "-journal"))?;
    let plan = Plan {
        tables,
        blobs: &blobs,
        manifest_sha256: &export.manifest_sha256,
        sequences: &sequences,
        owner_id: &owner,
        owner_email,
    };
    if let Err(e) = build_database(&temp, &plan).await {
        remove_if_present(&temp)?;
        remove_if_present(&side_file(&temp, "-journal"))?;
        return Err(e);
    }

    // Blobs first: the live database must never reference a file that is not there yet.
    let store = BlobStore::new(&options.data_dir);
    let mut referenced = BTreeSet::new();
    for blob in &blobs {
        if !referenced.contains(&blob.sha256) {
            referenced.insert(store.put(&export.read(&blob.relative)?)?);
        }
    }
    // A stale WAL would be replayed onto the new database.
    remove_if_present(&side_file(&target, "-wal"))?;
    remove_if_present(&side_file(&target, "-shm"))?;
    fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))?;
    fs::rename(&temp, &target)?;
    File::open(&options.data_dir)?.sync_all()?;

    summary.blob_files = referenced.len();
    if !options.force {
        for sha256 in store.list()? {
            if !referenced.contains(&sha256) {
                store.delete(&sha256)?;
                summary.orphan_blobs_removed += 1;
            }
        }
    }
    Ok(summary)
}
