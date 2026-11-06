-- Links each token to the authorization that produced it, so a replayed code or refresh token can
-- revoke the whole grant. `grant_id` is the stored hash of the grant's authorization code.
-- The OAuth tables start empty at cutover, so the default is never read.
ALTER TABLE oauth_access_tokens ADD COLUMN grant_id TEXT NOT NULL DEFAULT '';
ALTER TABLE oauth_refresh_tokens ADD COLUMN grant_id TEXT NOT NULL DEFAULT '';

-- A rotated refresh token stays until it expires: presenting it again is how a leak shows.
ALTER TABLE oauth_refresh_tokens ADD COLUMN rotated_at TEXT
    CHECK (rotated_at GLOB '????-??-??T??:??:??.???Z');
