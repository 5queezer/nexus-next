-- The owner's browser sessions, kept across restarts so that a remembered device stays logged in.
-- Only the SHA-256 of the cookie value is stored. A session ends at expires_at, on logout, on a
-- passphrase change, with `nexus sessions revoke-all`, and on restore without --keep-credentials.
CREATE TABLE owner_sessions (
    id_hash TEXT PRIMARY KEY CHECK (length(id_hash) = 64),
    remembered INTEGER NOT NULL CHECK (remembered IN (0, 1)),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) CHECK (created_at GLOB '????-??-??T??:??:??.???Z'),
    expires_at TEXT NOT NULL CHECK (expires_at GLOB '????-??-??T??:??:??.???Z')
) STRICT;

CREATE INDEX owner_sessions_expiry ON owner_sessions (expires_at);

-- Passkeys the owner registered after a passphrase login. `passkey` is webauthn-rs's serialized
-- credential (public key, counter, backup state); it holds no secret.
CREATE TABLE owner_passkeys (
    credential_id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    passkey TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) CHECK (created_at GLOB '????-??-??T??:??:??.???Z'),
    last_used_at TEXT CHECK (last_used_at GLOB '????-??-??T??:??:??.???Z')
) STRICT;
