-- The owner can approve the same text again, after replacing it: an approval only counts in its
-- variant while nothing newer has replaced it there, so this needs a row of its own. See SPEC 9a.
DROP TRIGGER cv_approvals_no_update;
DROP TRIGGER cv_approvals_no_delete;

CREATE TABLE cv_approvals_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source_sha256 TEXT NOT NULL,
    facts_sha256 TEXT NOT NULL,
    rendered_text TEXT NOT NULL,
    variant_id INTEGER NOT NULL,
    revision_number INTEGER NOT NULL,
    approved_by TEXT NOT NULL CHECK (approved_by IN ('cli', 'dashboard', 'lock')),
    approved_at TEXT NOT NULL CHECK (approved_at GLOB '????-??-??T??:??:??.???Z')
) STRICT;

INSERT INTO cv_approvals_new SELECT id, source_sha256, facts_sha256, rendered_text, variant_id,
    revision_number, approved_by, approved_at FROM cv_approvals ORDER BY id;
DROP TABLE cv_approvals;
ALTER TABLE cv_approvals_new RENAME TO cv_approvals;

CREATE INDEX cv_approvals_variant ON cv_approvals (variant_id, facts_sha256, id);
CREATE INDEX cv_approvals_source ON cv_approvals (source_sha256, facts_sha256);

CREATE TRIGGER cv_approvals_no_update BEFORE UPDATE ON cv_approvals
BEGIN
    SELECT RAISE(ABORT, 'cv_approvals is append-only');
END;

CREATE TRIGGER cv_approvals_no_delete BEFORE DELETE ON cv_approvals
BEGIN
    SELECT RAISE(ABORT, 'cv_approvals is append-only');
END;
