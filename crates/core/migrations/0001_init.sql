-- Timestamps are canonical UTC text (YYYY-MM-DDTHH:MM:SS.sssZ), so text order is time order.

CREATE TABLE applications (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    company TEXT NOT NULL,
    role TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'inbound',
    applied_at TEXT CHECK (applied_at GLOB '????-??-??T??:??:??.???Z'),
    last_contact TEXT CHECK (last_contact GLOB '????-??-??T??:??:??.???Z'),
    notes TEXT,
    follow_up_at TEXT CHECK (follow_up_at GLOB '????-??-??T??:??:??.???Z'),
    job_description TEXT,
    source TEXT,
    remote INTEGER NOT NULL DEFAULT 0 CHECK (remote IN (0, 1)),
    salary_min INTEGER,
    salary_max INTEGER,
    rating INTEGER,
    job_url TEXT,
    canonical_job_url TEXT UNIQUE,
    resume_id TEXT,
    company_size TEXT,
    salary_band_mentioned INTEGER NOT NULL DEFAULT 0 CHECK (salary_band_mentioned IN (0, 1)),
    triage_quality INTEGER,
    triage_reason TEXT,
    incoming_source TEXT,
    auto_rejected INTEGER NOT NULL DEFAULT 0 CHECK (auto_rejected IN (0, 1)),
    auto_reject_reason TEXT,
    archived_at TEXT CHECK (archived_at GLOB '????-??-??T??:??:??.???Z'),
    work_mode TEXT,
    eligible_countries TEXT CHECK (json_valid(eligible_countries)),
    primary_locations TEXT CHECK (json_valid(primary_locations)),
    office_days_min INTEGER,
    travel_percent INTEGER,
    visa_sponsorship INTEGER CHECK (visa_sponsorship IN (0, 1)),
    right_to_work_required INTEGER CHECK (right_to_work_required IN (0, 1)),
    timezone_overlap TEXT,
    salary_currency TEXT,
    salary_period TEXT,
    salary_type TEXT,
    ats_name TEXT,
    requisition_id TEXT,
    job_captured_at TEXT CHECK (job_captured_at GLOB '????-??-??T??:??:??.???Z'),
    job_verified_at TEXT CHECK (job_verified_at GLOB '????-??-??T??:??:??.???Z'),
    job_posted_at TEXT CHECK (job_posted_at GLOB '????-??-??T??:??:??.???Z'),
    job_closed_at TEXT CHECK (job_closed_at GLOB '????-??-??T??:??:??.???Z'),
    job_content_hash TEXT,
    job_liveness TEXT,
    job_summary TEXT,
    current_stage TEXT,
    next_action TEXT,
    event_version INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) CHECK (created_at GLOB '????-??-??T??:??:??.???Z'),
    updated_at TEXT NOT NULL CHECK (updated_at GLOB '????-??-??T??:??:??.???Z')
) STRICT;

CREATE INDEX applications_follow_up_at ON applications (follow_up_at);
CREATE INDEX applications_status ON applications (status);
CREATE INDEX applications_created_at ON applications (created_at);

CREATE TABLE contacts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    email TEXT,
    phone TEXT,
    role TEXT,
    linked_in TEXT,
    application_id INTEGER NOT NULL REFERENCES applications (id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) CHECK (created_at GLOB '????-??-??T??:??:??.???Z')
) STRICT;

-- Not in Prisma (Postgres had no index either), but SQLite scans the child table on every cascade.
CREATE INDEX contacts_application_id ON contacts (application_id);

CREATE TABLE application_submissions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    application_id INTEGER NOT NULL REFERENCES applications (id) ON DELETE CASCADE,
    idempotency_key TEXT NOT NULL UNIQUE,
    request_hash TEXT NOT NULL,
    submitted_at TEXT NOT NULL CHECK (submitted_at GLOB '????-??-??T??:??:??.???Z'),
    application_url TEXT,
    ats_name TEXT,
    requisition_id TEXT,
    language TEXT,
    answers TEXT NOT NULL CHECK (json_valid(answers)),
    policy TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(policy)),
    candidate_salary_min INTEGER,
    candidate_salary_max INTEGER,
    candidate_salary_currency TEXT,
    candidate_salary_period TEXT,
    candidate_salary_type TEXT,
    candidate_salary_flexible INTEGER NOT NULL DEFAULT 0 CHECK (candidate_salary_flexible IN (0, 1)),
    document_ids TEXT NOT NULL CHECK (json_valid(document_ids)),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) CHECK (created_at GLOB '????-??-??T??:??:??.???Z')
) STRICT;

CREATE INDEX application_submissions_application_submitted
    ON application_submissions (application_id, submitted_at);

CREATE TABLE documents (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    filename TEXT NOT NULL,
    original_name TEXT NOT NULL,
    size INTEGER NOT NULL,
    mime_type TEXT NOT NULL,
    document_type TEXT NOT NULL DEFAULT 'other',
    state TEXT NOT NULL DEFAULT 'current',
    version INTEGER NOT NULL DEFAULT 1,
    content_hash TEXT,
    source TEXT,
    generated_at TEXT CHECK (generated_at GLOB '????-??-??T??:??:??.???Z'),
    submitted_at TEXT CHECK (submitted_at GLOB '????-??-??T??:??:??.???Z'),
    submission_id INTEGER REFERENCES application_submissions (id) ON DELETE SET NULL,
    uploaded_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) CHECK (uploaded_at GLOB '????-??-??T??:??:??.???Z'),
    -- Address of the file in blobs/. Separate from content_hash, which update_document_metadata
    -- lets clients overwrite or clear.
    blob_sha256 TEXT CHECK (blob_sha256 IS NULL OR (length(blob_sha256) = 64 AND blob_sha256 NOT GLOB '*[^0-9a-f]*')),
    blob_state TEXT NOT NULL DEFAULT 'missing' CHECK (blob_state IN ('present', 'missing')),
    blob_source TEXT CHECK (blob_source IN ('gcs', 'recovered-hermes')),
    content_hash_computed INTEGER NOT NULL DEFAULT 0 CHECK (content_hash_computed IN (0, 1)),
    CHECK ((blob_state = 'present') = (blob_sha256 IS NOT NULL)),
    CHECK (blob_state = 'present' OR blob_source IS NULL)
) STRICT;

CREATE INDEX documents_type_state ON documents (document_type, state);
CREATE INDEX documents_submission_id ON documents (submission_id);
CREATE INDEX documents_blob_sha256 ON documents (blob_sha256);

CREATE TABLE application_documents (
    application_id INTEGER NOT NULL REFERENCES applications (id) ON DELETE CASCADE,
    document_id INTEGER NOT NULL REFERENCES documents (id) ON DELETE CASCADE,
    PRIMARY KEY (application_id, document_id)
) STRICT;

CREATE INDEX application_documents_document_id ON application_documents (document_id);

CREATE TABLE application_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    application_id INTEGER NOT NULL REFERENCES applications (id) ON DELETE CASCADE,
    type TEXT NOT NULL,
    idempotency_key TEXT UNIQUE,
    request_hash TEXT,
    occurred_at TEXT NOT NULL CHECK (occurred_at GLOB '????-??-??T??:??:??.???Z'),
    source TEXT,
    actor TEXT,
    contact_id TEXT,
    outcome TEXT,
    metadata TEXT CHECK (json_valid(metadata)),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) CHECK (created_at GLOB '????-??-??T??:??:??.???Z')
) STRICT;

CREATE INDEX application_events_application_occurred ON application_events (application_id, occurred_at, id);
CREATE INDEX application_events_occurred ON application_events (occurred_at, id);
CREATE INDEX application_events_type_occurred ON application_events (type, occurred_at);
CREATE INDEX application_events_contact_occurred ON application_events (contact_id, occurred_at);
CREATE INDEX application_events_outcome_occurred ON application_events (outcome, occurred_at);

-- No delete trigger: delete_application must still cascade, and cascades fire triggers.
CREATE TRIGGER application_events_append_only BEFORE UPDATE ON application_events
BEGIN
    SELECT RAISE(ABORT, 'application_events are append-only');
END;

CREATE TABLE cv_profile (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    singleton INTEGER NOT NULL DEFAULT 1 UNIQUE CHECK (singleton = 1),
    name TEXT NOT NULL,
    contact TEXT NOT NULL CHECK (json_valid(contact)),
    profile TEXT NOT NULL,
    skills TEXT NOT NULL CHECK (json_valid(skills)),
    experience TEXT NOT NULL CHECK (json_valid(experience)),
    projects TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(projects)),
    education TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(education)),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) CHECK (created_at GLOB '????-??-??T??:??:??.???Z'),
    updated_at TEXT NOT NULL CHECK (updated_at GLOB '????-??-??T??:??:??.???Z')
) STRICT;

CREATE TABLE cv_patches (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    application_id INTEGER NOT NULL UNIQUE REFERENCES applications (id) ON DELETE CASCADE,
    profile_override TEXT,
    experience_ids TEXT NOT NULL CHECK (json_valid(experience_ids)),
    skill_categories TEXT NOT NULL CHECK (json_valid(skill_categories)),
    include_projects INTEGER NOT NULL DEFAULT 0 CHECK (include_projects IN (0, 1)),
    include_education INTEGER NOT NULL DEFAULT 1 CHECK (include_education IN (0, 1)),
    -- No foreign key, as in Prisma: a patch outlives the generated document it points at.
    document_id INTEGER,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) CHECK (created_at GLOB '????-??-??T??:??:??.???Z'),
    updated_at TEXT NOT NULL CHECK (updated_at GLOB '????-??-??T??:??:??.???Z')
) STRICT;

CREATE TABLE api_tokens (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    token_hash TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL DEFAULT 'default',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) CHECK (created_at GLOB '????-??-??T??:??:??.???Z'),
    last_used_at TEXT CHECK (last_used_at GLOB '????-??-??T??:??:??.???Z')
) STRICT;

CREATE TABLE oauth_clients (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    client_id TEXT NOT NULL UNIQUE,
    client_secret_hash TEXT,
    client_name TEXT,
    redirect_uris TEXT NOT NULL CHECK (json_valid(redirect_uris)),
    grant_types TEXT NOT NULL CHECK (json_valid(grant_types)),
    response_types TEXT NOT NULL CHECK (json_valid(response_types)),
    token_endpoint_auth TEXT NOT NULL DEFAULT 'client_secret_post',
    client_secret_expires_at TEXT CHECK (client_secret_expires_at GLOB '????-??-??T??:??:??.???Z'),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) CHECK (created_at GLOB '????-??-??T??:??:??.???Z')
) STRICT;

CREATE TABLE oauth_auth_codes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    code TEXT NOT NULL UNIQUE,
    client_id TEXT NOT NULL,
    redirect_uri TEXT NOT NULL,
    code_challenge TEXT NOT NULL,
    scopes TEXT NOT NULL CHECK (json_valid(scopes)),
    sensitive_consent_version INTEGER NOT NULL DEFAULT 0,
    expires_at TEXT NOT NULL CHECK (expires_at GLOB '????-??-??T??:??:??.???Z'),
    used INTEGER NOT NULL DEFAULT 0 CHECK (used IN (0, 1)),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) CHECK (created_at GLOB '????-??-??T??:??:??.???Z')
) STRICT;

CREATE TABLE oauth_access_tokens (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    token_hash TEXT NOT NULL UNIQUE,
    client_id TEXT NOT NULL,
    scopes TEXT NOT NULL CHECK (json_valid(scopes)),
    sensitive_consent_version INTEGER NOT NULL DEFAULT 0,
    expires_at TEXT NOT NULL CHECK (expires_at GLOB '????-??-??T??:??:??.???Z'),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) CHECK (created_at GLOB '????-??-??T??:??:??.???Z')
) STRICT;

CREATE TABLE oauth_refresh_tokens (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    token_hash TEXT NOT NULL UNIQUE,
    client_id TEXT NOT NULL,
    scopes TEXT NOT NULL CHECK (json_valid(scopes)),
    sensitive_consent_version INTEGER NOT NULL DEFAULT 0,
    expires_at TEXT NOT NULL CHECK (expires_at GLOB '????-??-??T??:??:??.???Z'),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) CHECK (created_at GLOB '????-??-??T??:??:??.???Z')
) STRICT;

-- Written by `nexus import`; lets a re-import tell an untouched import from a database in use.
CREATE TABLE import_meta (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    manifest_sha256 TEXT NOT NULL,
    fingerprint TEXT NOT NULL
) STRICT;
