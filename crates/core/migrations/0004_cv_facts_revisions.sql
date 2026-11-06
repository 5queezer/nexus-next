-- SPEC section 9a. Hard facts and CV wording part ways: voice and agents can reach the wording, and
-- only the owner can reach locked facts.

CREATE TABLE cv_facts (
    singleton INTEGER PRIMARY KEY DEFAULT 1 CHECK (singleton = 1),
    name TEXT NOT NULL DEFAULT '',
    contact TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(contact)),
    employment TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(employment)),
    education TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(education)),
    languages TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(languages)),
    metrics TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(metrics)),
    allowed_tokens TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(allowed_tokens)),
    banned_phrases TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(banned_phrases)),
    vocabulary TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(vocabulary)),
    -- The profile's wording as the last lock confirmed it, as `[field, text]` pairs. A CV generated
    -- from just this wording is what the owner has already approved by locking.
    locked_wording TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(locked_wording)),
    version INTEGER NOT NULL DEFAULT 1,
    locked_at TEXT CHECK (locked_at IS NULL OR locked_at GLOB '????-??-??T??:??:??.???Z'),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) CHECK (updated_at GLOB '????-??-??T??:??:??.???Z')
) STRICT;

-- A fixed time, so that an import of the same export gives the same rows.
INSERT INTO cv_facts (singleton, updated_at) VALUES (1, '1970-01-01T00:00:00.000Z');

CREATE TRIGGER cv_facts_stays BEFORE DELETE ON cv_facts
BEGIN
    SELECT RAISE(ABORT, 'cv_facts is a singleton');
END;

-- An M1 database may hold a profile written by upsert_cv_profile: its facts move across here. The
-- importer and upsert_cv_profile split rows the same way. Education has no ID in old Nexus, so an
-- entry's ID is its position. M1's own checks keep the odd values below out, but `cv_facts` must be
-- readable whatever a row holds, or no tool could ever repair it: a fact that isn't a string reads as
-- empty, in `contact` by leaving the key out, and an entry that isn't an object is dropped.
UPDATE cv_facts SET
    name = p.name,
    contact = CASE WHEN json_type(p.contact) = 'object' THEN json_remove(
        p.contact,
        iif(json_type(p.contact, '$.email') = 'text', '$.keep', '$.email'),
        iif(json_type(p.contact, '$.phone') = 'text', '$.keep', '$.phone'),
        iif(json_type(p.contact, '$.linkedin') = 'text', '$.keep', '$.linkedin'),
        iif(json_type(p.contact, '$.github') = 'text', '$.keep', '$.github'),
        iif(json_type(p.contact, '$.location') = 'text', '$.keep', '$.location')
    ) ELSE '{}' END,
    employment = (
        SELECT json_group_array(json_object(
            'id', iif(json_type(value, '$.id') = 'text', value ->> '$.id', ''),
            'company', iif(json_type(value, '$.company') = 'text', value ->> '$.company', ''),
            'title', iif(json_type(value, '$.title') = 'text', value ->> '$.title', ''),
            'date', iif(json_type(value, '$.date') = 'text', value ->> '$.date', ''),
            'location', iif(json_type(value, '$.location') = 'text', value ->> '$.location', '')
        ))
        FROM json_each(p.experience)
        WHERE type = 'object'
    ),
    education = (
        SELECT json_group_array(CASE WHEN json_type(value, '$.details') = 'text' THEN json_object(
            'id', 'edu-' || (key + 1),
            'institution', iif(json_type(value, '$.institution') = 'text', value ->> '$.institution', ''),
            'qualification', iif(json_type(value, '$.degree') = 'text', value ->> '$.degree', ''),
            'date', iif(json_type(value, '$.date') = 'text', value ->> '$.date', ''),
            'location', iif(json_type(value, '$.location') = 'text', value ->> '$.location', ''),
            'details', value ->> '$.details'
        ) ELSE json_object(
            'id', 'edu-' || (key + 1),
            'institution', iif(json_type(value, '$.institution') = 'text', value ->> '$.institution', ''),
            'qualification', iif(json_type(value, '$.degree') = 'text', value ->> '$.degree', ''),
            'date', iif(json_type(value, '$.date') = 'text', value ->> '$.date', ''),
            'location', iif(json_type(value, '$.location') = 'text', value ->> '$.location', '')
        ) END)
        FROM json_each(p.education)
        WHERE type = 'object'
    )
FROM cv_profile p;

UPDATE cv_profile SET experience = (
    SELECT json_group_array(json(json_remove(value, '$.company', '$.title', '$.date', '$.location')))
    FROM json_each(experience)
    WHERE type = 'object'
);

ALTER TABLE cv_profile DROP COLUMN name;
ALTER TABLE cv_profile DROP COLUMN contact;
ALTER TABLE cv_profile DROP COLUMN education;

-- The state before each change, by the owner's paths and by upsert_cv_profile while unlocked.
CREATE TABLE cv_facts_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    version INTEGER NOT NULL,
    state TEXT NOT NULL CHECK (json_valid(state)),
    change TEXT NOT NULL,
    changed_by TEXT NOT NULL,
    changed_at TEXT NOT NULL CHECK (changed_at GLOB '????-??-??T??:??:??.???Z')
) STRICT;

CREATE TRIGGER cv_facts_history_no_update BEFORE UPDATE ON cv_facts_history
BEGIN
    SELECT RAISE(ABORT, 'cv_facts_history is append-only');
END;

CREATE TRIGGER cv_facts_history_no_delete BEFORE DELETE ON cv_facts_history
BEGIN
    SELECT RAISE(ABORT, 'cv_facts_history is append-only');
END;

CREATE TABLE cv_variants (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    application_id INTEGER REFERENCES applications (id) ON DELETE CASCADE,
    head_revision_id INTEGER REFERENCES cv_revisions (id),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) CHECK (created_at GLOB '????-??-??T??:??:??.???Z')
) STRICT;

CREATE INDEX cv_variants_application_id ON cv_variants (application_id);

CREATE TABLE cv_revisions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    variant_id INTEGER NOT NULL REFERENCES cv_variants (id) ON DELETE CASCADE,
    number INTEGER NOT NULL CHECK (number >= 1),
    -- No foreign key: a variant's first revision can have its parent in another variant, which
    -- deleting an application may remove, and this row can't change.
    parent_id INTEGER,
    source TEXT NOT NULL,
    source_sha256 TEXT NOT NULL,
    facts_version INTEGER NOT NULL,
    origin TEXT NOT NULL,
    summary TEXT NOT NULL,
    warnings TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(warnings)),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) CHECK (created_at GLOB '????-??-??T??:??:??.???Z'),
    UNIQUE (variant_id, number)
) STRICT;

INSERT INTO cv_variants (name, created_at) VALUES ('default', '1970-01-01T00:00:00.000Z');

CREATE TRIGGER cv_revisions_no_update BEFORE UPDATE ON cv_revisions
BEGIN
    SELECT RAISE(ABORT, 'cv_revisions are immutable');
END;

-- SQLite fires triggers on foreign-key cascades. It deletes the parent row first, so a cascade finds
-- its variant gone, and any other delete finds it still there.
CREATE TRIGGER cv_revisions_no_delete BEFORE DELETE ON cv_revisions
WHEN EXISTS (SELECT 1 FROM cv_variants WHERE id = OLD.variant_id)
BEGIN
    SELECT RAISE(ABORT, 'cv_revisions are immutable');
END;

CREATE TRIGGER cv_variants_keep_owner BEFORE UPDATE OF name, application_id ON cv_variants
BEGIN
    SELECT RAISE(ABORT, 'a cv_variant keeps its name and application');
END;

CREATE TRIGGER cv_variants_no_delete BEFORE DELETE ON cv_variants
WHEN OLD.application_id IS NULL OR EXISTS (SELECT 1 FROM applications WHERE id = OLD.application_id)
BEGIN
    SELECT RAISE(ABORT, 'a cv_variant goes only with its application');
END;

-- The owner's approvals: exactly this source, rendered from exactly these facts. A revision's state
-- is computed from here and is stored nowhere else. No foreign key: an approval outlives a variant
-- that goes with its application, since identical text stays approved.
CREATE TABLE cv_approvals (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source_sha256 TEXT NOT NULL,
    facts_sha256 TEXT NOT NULL,
    rendered_text TEXT NOT NULL,
    variant_id INTEGER NOT NULL,
    revision_number INTEGER NOT NULL,
    approved_by TEXT NOT NULL CHECK (approved_by IN ('cli', 'dashboard', 'lock')),
    approved_at TEXT NOT NULL CHECK (approved_at GLOB '????-??-??T??:??:??.???Z'),
    UNIQUE (source_sha256, facts_sha256)
) STRICT;

CREATE TRIGGER cv_approvals_no_update BEFORE UPDATE ON cv_approvals
BEGIN
    SELECT RAISE(ABORT, 'cv_approvals is append-only');
END;

CREATE TRIGGER cv_approvals_no_delete BEFORE DELETE ON cv_approvals
BEGIN
    SELECT RAISE(ABORT, 'cv_approvals is append-only');
END;
